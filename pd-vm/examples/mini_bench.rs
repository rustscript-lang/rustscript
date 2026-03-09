use std::fmt::Write as _;
use std::fs;
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;
use std::time::{Duration, Instant};

use serde_json::json;
use vm::{
    CallOutcome, CompiledProgram, HostFunction, HostFunctionRegistry, JitConfig, JitSnapshot,
    JitTraceTerminal, Program, SourceFlavor, Value, Vm, VmError, VmStatus, compile_source,
    compile_source_file, compile_source_with_flavor,
};

const DEFAULT_COMPILE_ITERS: usize = 20;
const DEFAULT_COMPILE_STRESS_LINES: usize = 1_000;
const DEFAULT_LOAD_ITERS: usize = 1_500;
const DEFAULT_LOAD_LOCAL_COUNT: usize = 4_096;
const DEFAULT_RUN_TRIALS: usize = 7;
const DEFAULT_RSS_VM_COUNT: usize = 256;
const DEFAULT_HOT_LOOP_INNER: i64 = 40_000;
const DEFAULT_HOT_LOOP_OUTER: i64 = 8;
const DEFAULT_AOT_ITERS: usize = 7;
const LOAD_HOST_COUNTS: [usize; 6] = [0, 1, 10, 50, 100, 500];

fn main() {
    if let Err(err) = real_main() {
        eprintln!("mini benchmark failed: {err}");
        std::process::exit(1);
    }
}

fn real_main() -> Result<(), String> {
    let config = BenchConfig::parse(std::env::args().skip(1))?;
    if let Some(mode) = config.rss_child_mode {
        let sample = measure_retained_rss_for_mode(mode, config.rss_vm_count)?;
        println!("{}", sample.to_child_line());
        return Ok(());
    }

    print_banner(&config);
    benchmark_compile(&config)?;
    benchmark_aot_compile(&config)?;
    benchmark_load(&config)?;
    benchmark_runtime(&config)?;
    benchmark_lua_hot_loop_compare(&config)?;
    benchmark_rss(&config)?;
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PerfExecMode {
    Interpreter,
    Jit,
    Aot,
}

impl PerfExecMode {
    fn label(self) -> &'static str {
        match self {
            Self::Interpreter => "interpreter",
            Self::Jit => "jit",
            Self::Aot => "aot",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RssMode {
    Interpreter,
    Jit,
}

impl RssMode {
    fn label(self) -> &'static str {
        match self {
            Self::Interpreter => "interpreter",
            Self::Jit => "jit",
        }
    }

    fn parse(input: &str) -> Option<Self> {
        match input {
            "interpreter" => Some(Self::Interpreter),
            "jit" => Some(Self::Jit),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
struct BenchConfig {
    compile_iters: usize,
    compile_stress_lines: usize,
    load_iters: usize,
    load_local_count: usize,
    run_trials: usize,
    rss_vm_count: usize,
    hot_loop_inner: i64,
    hot_loop_outer: i64,
    aot_iters: usize,
    rss_child_mode: Option<RssMode>,
}

impl Default for BenchConfig {
    fn default() -> Self {
        Self {
            compile_iters: DEFAULT_COMPILE_ITERS,
            compile_stress_lines: DEFAULT_COMPILE_STRESS_LINES,
            load_iters: DEFAULT_LOAD_ITERS,
            load_local_count: DEFAULT_LOAD_LOCAL_COUNT,
            run_trials: DEFAULT_RUN_TRIALS,
            rss_vm_count: DEFAULT_RSS_VM_COUNT,
            hot_loop_inner: DEFAULT_HOT_LOOP_INNER,
            hot_loop_outer: DEFAULT_HOT_LOOP_OUTER,
            aot_iters: DEFAULT_AOT_ITERS,
            rss_child_mode: None,
        }
    }
}

impl BenchConfig {
    fn parse(args: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut config = Self::default();
        let mut args = args.peekable();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--compile-iters" => {
                    config.compile_iters = parse_usize_flag("--compile-iters", args.next())?;
                }
                "--compile-stress-lines" => {
                    config.compile_stress_lines =
                        parse_usize_flag("--compile-stress-lines", args.next())?;
                }
                "--load-iters" => {
                    config.load_iters = parse_usize_flag("--load-iters", args.next())?;
                }
                "--load-locals" => {
                    config.load_local_count = parse_usize_flag("--load-locals", args.next())?;
                }
                "--run-trials" => {
                    config.run_trials = parse_usize_flag("--run-trials", args.next())?;
                }
                "--rss-vms" => {
                    config.rss_vm_count = parse_usize_flag("--rss-vms", args.next())?;
                }
                "--hot-loop-inner" => {
                    config.hot_loop_inner = parse_i64_flag("--hot-loop-inner", args.next())?;
                }
                "--hot-loop-outer" => {
                    config.hot_loop_outer = parse_i64_flag("--hot-loop-outer", args.next())?;
                }
                "--aot-iters" => {
                    config.aot_iters = parse_usize_flag("--aot-iters", args.next())?;
                }
                "--rss-child" => {
                    let value = args
                        .next()
                        .ok_or_else(|| "missing value for --rss-child".to_string())?;
                    config.rss_child_mode = Some(
                        RssMode::parse(&value)
                            .ok_or_else(|| format!("invalid --rss-child mode '{value}'"))?,
                    );
                }
                "--help" | "-h" => {
                    print_help();
                    std::process::exit(0);
                }
                other => {
                    return Err(format!("unrecognized argument '{other}'"));
                }
            }
        }

        if config.compile_iters == 0
            || config.load_iters == 0
            || config.run_trials == 0
            || config.rss_vm_count == 0
            || config.aot_iters == 0
        {
            return Err("iteration counts must be >= 1".to_string());
        }
        if config.hot_loop_inner <= 0 || config.hot_loop_outer <= 0 {
            return Err("hot loop sizes must be > 0".to_string());
        }
        Ok(config)
    }
}

fn parse_usize_flag(flag: &str, value: Option<String>) -> Result<usize, String> {
    value
        .ok_or_else(|| format!("missing value for {flag}"))?
        .parse::<usize>()
        .map_err(|err| format!("invalid usize for {flag}: {err}"))
}

fn parse_i64_flag(flag: &str, value: Option<String>) -> Result<i64, String> {
    value
        .ok_or_else(|| format!("missing value for {flag}"))?
        .parse::<i64>()
        .map_err(|err| format!("invalid i64 for {flag}: {err}"))
}

fn print_help() {
    println!("pd-vm mini benchmark platform");
    println!("usage: cargo run -p pd-vm --example mini_bench --release -- [options]");
    println!("  --compile-iters <n>");
    println!("  --compile-stress-lines <n>");
    println!("  --load-iters <n>");
    println!("  --load-locals <n>");
    println!("  --run-trials <n>");
    println!("  --rss-vms <n>");
    println!("  --hot-loop-inner <n>");
    println!("  --hot-loop-outer <n>");
    println!("  --aot-iters <n>");
}

fn print_banner(config: &BenchConfig) {
    println!("pd-vm mini benchmark platform");
    println!(
        "config: compile_iters={} compile_stress_lines={} load_iters={} load_locals={} run_trials={} rss_vms={} hot_loop_inner={} hot_loop_outer={} aot_iters={} native_jit_supported={}",
        config.compile_iters,
        config.compile_stress_lines,
        config.load_iters,
        config.load_local_count,
        config.run_trials,
        config.rss_vm_count,
        config.hot_loop_inner,
        config.hot_loop_outer,
        config.aot_iters,
        native_jit_supported()
    );
    println!();
}

fn benchmark_compile(config: &BenchConfig) -> Result<(), String> {
    println!("[compile]");
    let example_dir = example_dir();
    let workloads = [
        (
            "rss_complex_inline",
            CompileWorkload::Inline(SourceFlavor::RustScript, build_complex_rss_source()),
        ),
        (
            "lua_complex_file",
            CompileWorkload::File(example_dir.join("example_complex.lua")),
        ),
        (
            "js_complex_file",
            CompileWorkload::File(example_dir.join("example_complex.js")),
        ),
        (
            "scm_complex_file",
            CompileWorkload::File(example_dir.join("example_complex.scm")),
        ),
        (
            "rss_stress_inline",
            CompileWorkload::Inline(
                SourceFlavor::RustScript,
                build_compiler_stress_source(config.compile_stress_lines),
            ),
        ),
    ];

    for (label, workload) in workloads {
        let sample = measure_compile_workload(label, &workload, config.compile_iters)?;
        println!(
            "  {:<20} total_ms={:<8} avg_us={:<10} locals={} imports={} constants={} code_bytes={}",
            sample.label,
            sample.elapsed.as_millis(),
            sample.avg_micros(),
            sample.locals,
            sample.import_count,
            sample.constant_count,
            sample.code_len,
        );
    }
    println!();
    Ok(())
}

fn benchmark_aot_compile(config: &BenchConfig) -> Result<(), String> {
    println!("[aot_compile]");
    if !native_jit_supported() {
        println!("  native AOT unsupported on this target");
        println!();
        return Ok(());
    }

    let workload = build_hot_loop_workload(2_000, 4)?;

    let mut samples = Vec::with_capacity(config.aot_iters);
    let mut prepared_trace_counts = Vec::with_capacity(config.aot_iters);
    for _ in 0..config.aot_iters {
        let mut vm = Vm::new(workload.program.clone());
        vm.set_jit_config(native_jit_config());
        let started = Instant::now();
        let prepared = vm
            .prepare_aot()
            .map_err(|err| format!("failed to prepare AOT for hot loop workload: {err}"))?;
        samples.push(started.elapsed());
        prepared_trace_counts.push(prepared as u64);
    }

    println!(
        "  hot_loop_medium median_us={} avg_us={} prepared_traces_median={}",
        median_duration(&mut samples).as_micros(),
        average_duration(&samples).as_micros(),
        median_u64(&mut prepared_trace_counts),
    );
    println!();
    Ok(())
}

fn benchmark_load(config: &BenchConfig) -> Result<(), String> {
    println!("[load]");
    for host_count in LOAD_HOST_COUNTS {
        let load_program = build_load_program(host_count, config.load_local_count)?;
        let sample = measure_load_time(
            &load_program,
            config.load_iters,
            config.load_local_count,
            host_count,
        )?;
        println!(
            "  hosts={:<4} total_ms={:<8} avg_ns={:<12} imports={} locals={}",
            host_count,
            sample.elapsed.as_millis(),
            sample.avg_nanos(),
            sample.import_count,
            sample.local_count,
        );
    }
    println!();
    Ok(())
}

fn benchmark_runtime(config: &BenchConfig) -> Result<(), String> {
    println!("[run]");

    let aes_path = example_dir().join("aes_128_cbc_usage.rss");
    let hot_loop = build_hot_loop_workload(config.hot_loop_inner, config.hot_loop_outer)?;
    let hot_expected = vec![Value::Int(hot_loop.expected)];

    match compile_source_file(&aes_path) {
        Ok(aes_compiled) => {
            let aes_expected = vec![Value::string("7649abac8119b246cee98e9b12e9197d")];
            benchmark_runtime_workload(
                "aes_128_cbc_usage",
                &aes_compiled.program,
                &aes_expected,
                config.run_trials,
            )?;
        }
        Err(err) => {
            println!(
                "  {:<16} mode=all          skipped compile_error={}",
                "aes_128_cbc_usage", err
            );
        }
    }

    benchmark_runtime_workload(
        "hot_loop",
        &hot_loop.program,
        &hot_expected,
        config.run_trials,
    )?;
    println!();
    Ok(())
}

fn benchmark_runtime_workload(
    label: &str,
    program: &Program,
    expected_stack: &[Value],
    trials: usize,
) -> Result<(), String> {
    let interpreter =
        measure_runtime_mode(program, expected_stack, PerfExecMode::Interpreter, trials)?;
    println!(
        "  {:<16} mode={:<12} median_us={:<10} avg_us={:<10}",
        label,
        interpreter.mode.label(),
        interpreter.median.as_micros(),
        average_duration(&interpreter.samples).as_micros(),
    );

    if native_jit_supported() {
        let jit = measure_runtime_mode(program, expected_stack, PerfExecMode::Jit, trials)?;
        println!(
            "  {:<16} mode={:<12} median_us={:<10} avg_us={:<10}",
            label,
            jit.mode.label(),
            jit.median.as_micros(),
            average_duration(&jit.samples).as_micros(),
        );

        let aot = measure_runtime_mode(program, expected_stack, PerfExecMode::Aot, trials)?;
        println!(
            "  {:<16} mode={:<12} median_us={:<10} avg_us={:<10}",
            label,
            aot.mode.label(),
            aot.median.as_micros(),
            average_duration(&aot.samples).as_micros(),
        );
    } else {
        println!(
            "  {:<16} mode=jit/aot      unsupported on this target",
            label
        );
    }

    Ok(())
}

fn benchmark_rss(config: &BenchConfig) -> Result<(), String> {
    println!("[rss]");
    let interpreter = measure_retained_rss_via_child(config, RssMode::Interpreter)?;
    print_rss_sample(&interpreter);

    if native_jit_supported() {
        let jit = measure_retained_rss_via_child(config, RssMode::Jit)?;
        print_rss_sample(&jit);
    } else {
        println!("  mode=jit          unsupported on this target");
    }
    println!();
    Ok(())
}

fn measure_compile_workload(
    label: &str,
    workload: &CompileWorkload,
    iterations: usize,
) -> Result<CompileSample, String> {
    let started = Instant::now();
    let mut last = None;
    for _ in 0..iterations {
        let compiled = compile_workload_once(workload)?;
        black_box(compiled.program.code.len());
        last = Some(compiled);
    }
    let elapsed = started.elapsed();
    let compiled = last.ok_or_else(|| format!("no compile results produced for {label}"))?;
    Ok(CompileSample {
        label: label.to_string(),
        elapsed,
        locals: compiled.locals,
        import_count: compiled.program.imports.len(),
        constant_count: compiled.program.constants.len(),
        code_len: compiled.program.code.len(),
        iterations,
    })
}

fn compile_workload_once(workload: &CompileWorkload) -> Result<CompiledProgram, String> {
    match workload {
        CompileWorkload::File(path) => compile_source_file(path)
            .map_err(|err| format!("compile failed for '{}': {err}", path.display())),
        CompileWorkload::Inline(flavor, source) => compile_source_with_flavor(source, *flavor)
            .map_err(|err| format!("inline compile failed for {flavor:?}: {err}")),
    }
}

enum CompileWorkload {
    File(PathBuf),
    Inline(SourceFlavor, String),
}

struct CompileSample {
    label: String,
    elapsed: Duration,
    locals: usize,
    import_count: usize,
    constant_count: usize,
    code_len: usize,
    iterations: usize,
}

impl CompileSample {
    fn avg_micros(&self) -> u128 {
        self.elapsed.as_micros() / self.iterations as u128
    }
}

struct LoadSample {
    elapsed: Duration,
    iterations: usize,
    import_count: usize,
    local_count: usize,
}

impl LoadSample {
    fn avg_nanos(&self) -> u128 {
        self.elapsed.as_nanos() / self.iterations as u128
    }
}

fn measure_load_time(
    program: &Program,
    iterations: usize,
    local_count: usize,
    host_count: usize,
) -> Result<LoadSample, String> {
    let mut registry = HostFunctionRegistry::new();
    for index in 0..host_count {
        registry.register(format!("host_{index}"), 1, || {
            Box::new(PerfNoopHost::default())
        });
    }
    let plan = if host_count == 0 {
        None
    } else {
        Some(
            registry
                .prepare_plan(&program.imports)
                .map_err(|err| format!("failed to prepare host binding plan: {err}"))?,
        )
    };

    let started = Instant::now();
    for _ in 0..iterations {
        let mut vm = Vm::new(program.clone().with_local_count(local_count));
        if let Some(plan) = &plan {
            registry
                .bind_vm_with_plan(&mut vm, plan)
                .map_err(|err| format!("failed to bind host plan: {err}"))?;
        }
        black_box(vm.stack().len());
    }
    Ok(LoadSample {
        elapsed: started.elapsed(),
        iterations,
        import_count: program.imports.len(),
        local_count,
    })
}

fn build_load_program(host_count: usize, local_count: usize) -> Result<Program, String> {
    let source = build_load_stress_source(host_count, 256);
    let compiled = compile_source(&source).map_err(|err| {
        format!("failed to compile load stress source for {host_count} hosts: {err}")
    })?;
    Ok(compiled
        .program
        .with_local_count(local_count.max(compiled.locals)))
}

struct RuntimeSample {
    mode: PerfExecMode,
    samples: Vec<Duration>,
    median: Duration,
}

fn measure_runtime_mode(
    program: &Program,
    expected_stack: &[Value],
    mode: PerfExecMode,
    trials: usize,
) -> Result<RuntimeSample, String> {
    let mut samples = Vec::with_capacity(trials);
    for _ in 0..trials {
        let mut vm = Vm::new(program.clone());
        configure_vm_for_mode(&mut vm, mode);
        warm_vm_for_mode(&mut vm, mode, expected_stack)?;
        vm.reset_for_reuse();
        let started = Instant::now();
        let status = vm
            .run()
            .map_err(|err| format!("timed {} run failed: {err}", mode.label()))?;
        let elapsed = started.elapsed();
        ensure_expected_completion(&vm, status, expected_stack, mode.label())?;
        if mode != PerfExecMode::Interpreter {
            ensure_jit_executed(&vm, mode)?;
        }
        samples.push(elapsed);
    }
    let mut median_samples = samples.clone();
    let median = median_duration(&mut median_samples);
    Ok(RuntimeSample {
        mode,
        samples,
        median,
    })
}

fn configure_vm_for_mode(vm: &mut Vm, mode: PerfExecMode) {
    let jit_enabled = mode != PerfExecMode::Interpreter;
    vm.set_jit_config(JitConfig {
        enabled: jit_enabled,
        hot_loop_threshold: 1,
        max_trace_len: 16_384,
    });
}

fn warm_vm_for_mode(
    vm: &mut Vm,
    mode: PerfExecMode,
    expected_stack: &[Value],
) -> Result<(), String> {
    if mode == PerfExecMode::Aot {
        vm.prepare_aot()
            .map_err(|err| format!("AOT prepare failed during warmup: {err}"))?;
    }
    let status = vm
        .run()
        .map_err(|err| format!("warmup {} run failed: {err}", mode.label()))?;
    ensure_expected_completion(vm, status, expected_stack, mode.label())?;
    Ok(())
}

fn ensure_expected_completion(
    vm: &Vm,
    status: VmStatus,
    expected_stack: &[Value],
    label: &str,
) -> Result<(), String> {
    if status != VmStatus::Halted {
        return Err(format!("expected {label} run to halt, got {status:?}"));
    }
    if vm.stack() != expected_stack {
        return Err(format!(
            "unexpected stack for {label} run: expected {:?}, got {:?}",
            expected_stack,
            vm.stack()
        ));
    }
    Ok(())
}

fn ensure_jit_executed(vm: &Vm, mode: PerfExecMode) -> Result<(), String> {
    if vm.jit_native_exec_count() == 0 {
        return Err(format!(
            "expected native execution count > 0 for {} mode",
            mode.label()
        ));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LuaCompareMode {
    PdVmInterpreter,
    PdVmJit,
    LuaJitOff,
    LuaJitJit,
}

impl LuaCompareMode {
    fn label(self) -> &'static str {
        match self {
            Self::PdVmInterpreter => "pd-vm",
            Self::PdVmJit => "pd-vm-jit",
            Self::LuaJitOff => "luajit-joff",
            Self::LuaJitJit => "luajit-jit",
        }
    }
}

#[derive(Clone, Debug)]
struct LuaCompareSample {
    mode: LuaCompareMode,
    available: bool,
    skip_reason: Option<String>,
    compile_excluded: bool,
    warmup_runs: usize,
    timed_runs: usize,
    total_elapsed: Option<Duration>,
    normalized_ns_per_inner_iter: Option<f64>,
    result: Option<i64>,
    native_trace_count_before: Option<usize>,
    native_trace_count_after: Option<usize>,
    native_exec_delta: Option<u64>,
    jit_runtime_diagnostics: Vec<(String, u64)>,
    native_bridge_stats: Vec<(String, u64)>,
    interpreter_opcode_counts: Vec<(String, u64)>,
    interpreter_superinstruction_counts: Vec<(String, u64)>,
}

fn benchmark_lua_hot_loop_compare(config: &BenchConfig) -> Result<(), String> {
    println!("[lua_compare]");

    let shared_source = build_lua_hot_loop_shared_source();
    let pd_vm_source = shared_source
        .replace("__PDVM_HOT_LOOP_INNER__", &config.hot_loop_inner.to_string())
        .replace("__PDVM_HOT_LOOP_OUTER__", &config.hot_loop_outer.to_string());
    let compiled = compile_source_with_flavor(&pd_vm_source, SourceFlavor::Lua)
        .map_err(|err| format!("failed to compile Lua hot loop workload: {err}"))?;
    let expected = config.hot_loop_inner * (config.hot_loop_inner - 1) / 2;

    let mut samples = Vec::new();
    samples.push(measure_pd_vm_lua_compare_mode(
        &compiled,
        LuaCompareMode::PdVmInterpreter,
        expected,
        config.run_trials,
        config.hot_loop_inner,
        config.hot_loop_outer,
    )?);

    if native_jit_supported() {
        samples.push(measure_pd_vm_lua_compare_mode(
            &compiled,
            LuaCompareMode::PdVmJit,
            expected,
            config.run_trials,
            config.hot_loop_inner,
            config.hot_loop_outer,
        )?);
    } else {
        samples.push(skipped_lua_compare_sample(
            LuaCompareMode::PdVmJit,
            config.run_trials,
            "native JIT unsupported on this target",
        ));
    }

    samples.push(measure_luajit_compare_mode(
        &shared_source,
        LuaCompareMode::LuaJitOff,
        expected,
        config.run_trials,
        config.hot_loop_inner,
        config.hot_loop_outer,
    ));
    samples.push(measure_luajit_compare_mode(
        &shared_source,
        LuaCompareMode::LuaJitJit,
        expected,
        config.run_trials,
        config.hot_loop_inner,
        config.hot_loop_outer,
    ));

    for sample in &samples {
        if let Some(reason) = sample.skip_reason.as_deref() {
            println!("  {:<12} skipped {}", sample.mode.label(), reason);
            continue;
        }
        println!(
            "  {:<12} total_us={:<10} ns_per_inner_iter={:.2}",
            sample.mode.label(),
            sample.total_elapsed.unwrap_or_default().as_micros(),
            sample.normalized_ns_per_inner_iter.unwrap_or_default(),
        );
    }

    write_lua_compare_artifacts(config, &shared_source, expected, &samples)?;
    println!();
    Ok(())
}

fn measure_pd_vm_lua_compare_mode(
    compiled: &CompiledProgram,
    mode: LuaCompareMode,
    expected: i64,
    timed_runs: usize,
    inner: i64,
    outer: i64,
) -> Result<LuaCompareSample, String> {
    let mut vm = Vm::new(compiled.program.clone().with_local_count(compiled.locals));
    let jit_enabled = mode == LuaCompareMode::PdVmJit;
    vm.set_jit_config(JitConfig {
        enabled: jit_enabled,
        hot_loop_threshold: 1,
        max_trace_len: 16_384,
    });
    vm.set_jit_native_bridge_stats_enabled(jit_enabled);
    vm.set_interpreter_opcode_profiling_enabled(!jit_enabled);

    let expected_stack = [Value::Int(expected)];
    let warm_status = vm
        .run()
        .map_err(|err| format!("warmup {} run failed: {err}", mode.label()))?;
    ensure_expected_completion(&vm, warm_status, &expected_stack, mode.label())?;
    vm.reset_for_reuse();
    vm.clear_jit_native_bridge_stats();
    vm.clear_interpreter_opcode_profile();

    let native_trace_count_before = vm.jit_native_trace_count();
    let native_exec_before = vm.jit_native_exec_count();
    let jit_snapshot_before = vm.jit_snapshot();
    let started = Instant::now();
    let total_program_runs = timed_runs.saturating_mul(outer as usize);
    for run_index in 0..total_program_runs {
        let status = vm
            .run()
            .map_err(|err| format!("timed {} run failed: {err}", mode.label()))?;
        ensure_expected_completion(&vm, status, &expected_stack, mode.label())?;
        if jit_enabled {
            ensure_jit_executed(&vm, PerfExecMode::Jit)?;
        }
        if run_index + 1 < total_program_runs {
            vm.reset_for_reuse();
        }
    }
    let total_elapsed = started.elapsed();
    let jit_snapshot_after = vm.jit_snapshot();

    Ok(LuaCompareSample {
        mode,
        available: true,
        skip_reason: None,
        compile_excluded: true,
        warmup_runs: 1,
        timed_runs,
        total_elapsed: Some(total_elapsed),
        normalized_ns_per_inner_iter: Some(normalized_ns_per_inner_iter(
            total_elapsed,
            inner,
            outer,
            timed_runs,
        )),
        result: Some(expected),
        native_trace_count_before: Some(native_trace_count_before),
        native_trace_count_after: Some(vm.jit_native_trace_count()),
        native_exec_delta: Some(vm.jit_native_exec_count().saturating_sub(native_exec_before)),
        jit_runtime_diagnostics: trace_execution_diagnostics(
            &jit_snapshot_before,
            &jit_snapshot_after,
        ),
        native_bridge_stats: vm
            .jit_native_bridge_stats_snapshot()
            .into_iter()
            .map(|(name, count)| (name.to_string(), count))
            .collect(),
        interpreter_opcode_counts: vm.interpreter_opcode_profile_snapshot(),
        interpreter_superinstruction_counts: vm.interpreter_superinstruction_profile_snapshot(),
    })
}

fn measure_luajit_compare_mode(
    shared_source: &str,
    mode: LuaCompareMode,
    expected: i64,
    timed_runs: usize,
    inner: i64,
    outer: i64,
) -> LuaCompareSample {
    let mut command = Command::new("luajit");
    if mode == LuaCompareMode::LuaJitOff {
        command.arg("-joff");
    }
    let script = build_luajit_benchmark_script(shared_source, timed_runs, inner, outer);
    let output = command.arg("-e").arg(script).output();
    let Ok(output) = output else {
        return skipped_lua_compare_sample(mode, timed_runs, "luajit not found in PATH");
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return skipped_lua_compare_sample(
            mode,
            timed_runs,
            &format!("luajit failed: {}", stderr.trim()),
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed = parse_luajit_compare_output(&stdout);
    match parsed {
        Ok((result, elapsed_ns)) if result == expected => LuaCompareSample {
            mode,
            available: true,
            skip_reason: None,
            compile_excluded: true,
            warmup_runs: 1,
            timed_runs,
            total_elapsed: Some(Duration::from_nanos(elapsed_ns)),
            normalized_ns_per_inner_iter: Some(
                elapsed_ns as f64 / (inner as f64 * outer as f64 * timed_runs as f64),
            ),
            result: Some(result),
            native_trace_count_before: None,
            native_trace_count_after: None,
            native_exec_delta: None,
            jit_runtime_diagnostics: Vec::new(),
            native_bridge_stats: Vec::new(),
            interpreter_opcode_counts: Vec::new(),
            interpreter_superinstruction_counts: Vec::new(),
        },
        Ok((result, _)) => skipped_lua_compare_sample(
            mode,
            timed_runs,
            &format!("unexpected LuaJIT result {result}, expected {expected}"),
        ),
        Err(err) => skipped_lua_compare_sample(mode, timed_runs, &err),
    }
}

fn skipped_lua_compare_sample(
    mode: LuaCompareMode,
    timed_runs: usize,
    reason: &str,
) -> LuaCompareSample {
    LuaCompareSample {
        mode,
        available: false,
        skip_reason: Some(reason.to_string()),
        compile_excluded: true,
        warmup_runs: 1,
        timed_runs,
        total_elapsed: None,
        normalized_ns_per_inner_iter: None,
        result: None,
        native_trace_count_before: None,
        native_trace_count_after: None,
        native_exec_delta: None,
        jit_runtime_diagnostics: Vec::new(),
        native_bridge_stats: Vec::new(),
        interpreter_opcode_counts: Vec::new(),
        interpreter_superinstruction_counts: Vec::new(),
    }
}

fn build_lua_hot_loop_shared_source() -> String {
    r#"
local i = 0
local sum = 0
while i < __PDVM_HOT_LOOP_INNER__ do
    sum = sum + i
    i = i + 1
end
sum
"#
    .trim()
    .to_string()
}

fn build_luajit_benchmark_script(
    shared_source: &str,
    timed_runs: usize,
    inner: i64,
    outer: i64,
) -> String {
    let function_body = shared_source
        .replace("__PDVM_HOT_LOOP_INNER__", &inner.to_string())
        .replace("__PDVM_HOT_LOOP_OUTER__", &outer.to_string());
    format!(
        r#"
local function pd_vm_hot_loop()
{function_body}
end
local warmup_runs = 1
local last = 0
for _ = 1, warmup_runs do
    last = pd_vm_hot_loop()
end
local started = os.clock()
for _ = 1, {timed_runs} * {outer} do
    last = pd_vm_hot_loop()
end
local elapsed_ns = math.floor((os.clock() - started) * 1000000000)
io.write(string.format("result=%d elapsed_ns=%d\n", last, elapsed_ns))
"#
    )
}

fn parse_luajit_compare_output(stdout: &str) -> Result<(i64, u64), String> {
    let line = stdout
        .lines()
        .find(|line| line.contains("result="))
        .ok_or_else(|| format!("missing result line in LuaJIT output:\n{stdout}"))?;
    let mut result = None;
    let mut elapsed_ns = None;
    for part in line.split_whitespace() {
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        match key {
            "result" => {
                result = Some(
                    value
                        .parse::<i64>()
                        .map_err(|err| format!("invalid LuaJIT result '{value}': {err}"))?,
                );
            }
            "elapsed_ns" => {
                elapsed_ns = Some(
                    value
                        .parse::<u64>()
                        .map_err(|err| format!("invalid LuaJIT elapsed_ns '{value}': {err}"))?,
                );
            }
            _ => {}
        }
    }
    Ok((
        result.ok_or_else(|| format!("missing result in LuaJIT output: {line}"))?,
        elapsed_ns.ok_or_else(|| format!("missing elapsed_ns in LuaJIT output: {line}"))?,
    ))
}

fn normalized_ns_per_inner_iter(
    elapsed: Duration,
    inner: i64,
    outer: i64,
    timed_runs: usize,
) -> f64 {
    elapsed.as_nanos() as f64 / (inner as f64 * outer as f64 * timed_runs as f64)
}

fn trace_execution_diagnostics(
    before: &JitSnapshot,
    after: &JitSnapshot,
) -> Vec<(String, u64)> {
    let mut before_execs = before
        .traces
        .iter()
        .map(|trace| (trace.id, trace.executions))
        .collect::<std::collections::HashMap<_, _>>();
    let mut loop_back = 0_u64;
    let mut branch_exit = 0_u64;
    let mut halt = 0_u64;
    for trace in &after.traces {
        let before_exec = before_execs.remove(&trace.id).unwrap_or(0);
        let delta = trace.executions.saturating_sub(before_exec);
        match trace.terminal {
            JitTraceTerminal::LoopBack => loop_back = loop_back.saturating_add(delta),
            JitTraceTerminal::BranchExit => branch_exit = branch_exit.saturating_add(delta),
            JitTraceTerminal::Halt => halt = halt.saturating_add(delta),
        }
    }
    let mut entries = Vec::new();
    for (name, count) in [
        ("loop_back_trace_exec_delta", loop_back),
        ("branch_exit_trace_exec_delta", branch_exit),
        ("halt_trace_exec_delta", halt),
    ] {
        if count > 0 {
            entries.push((name.to_string(), count));
        }
    }
    entries
}

fn write_lua_compare_artifacts(
    config: &BenchConfig,
    shared_source: &str,
    expected: i64,
    samples: &[LuaCompareSample],
) -> Result<(), String> {
    let results_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("results");
    fs::create_dir_all(&results_dir)
        .map_err(|err| format!("failed to create results dir '{}': {err}", results_dir.display()))?;

    let timestamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_err(|err| format!("system clock error while preparing benchmark artifact: {err}"))?
        .as_secs();

    let artifact = json!({
        "workload": {
            "name": "lua_hot_loop_same_source",
            "source_kind": "shared_lua_hot_loop_function",
            "hot_loop_inner": config.hot_loop_inner,
            "hot_loop_outer": config.hot_loop_outer,
            "expected_result": expected,
        },
        "methodology": {
            "compile_excluded_from_timing": true,
            "warmup_excluded_from_timing": true,
            "timed_runs_use_steady_state_execution_only": true,
        },
        "shared_source": shared_source,
        "samples": samples.iter().map(|sample| {
            json!({
                "mode": sample.mode.label(),
                "available": sample.available,
                "skip_reason": sample.skip_reason,
                "compile_excluded": sample.compile_excluded,
                "warmup_runs": sample.warmup_runs,
                "timed_runs": sample.timed_runs,
                "total_elapsed_ns": sample.total_elapsed.map(|value| value.as_nanos() as u64),
                "normalized_ns_per_inner_iter": sample.normalized_ns_per_inner_iter,
                "result": sample.result,
                "native_trace_count_before": sample.native_trace_count_before,
                "native_trace_count_after": sample.native_trace_count_after,
                "native_exec_delta": sample.native_exec_delta,
                "jit_runtime_diagnostics": sample.jit_runtime_diagnostics,
                "native_bridge_stats": sample.native_bridge_stats,
                "interpreter_opcode_counts": sample.interpreter_opcode_counts,
                "interpreter_superinstruction_counts": sample.interpreter_superinstruction_counts,
            })
        }).collect::<Vec<_>>(),
    });

    let run_json_path = results_dir.join(format!("pd_vm_lua_hot_loop_{timestamp}.json"));
    let latest_json_path = results_dir.join("pd_vm_lua_hot_loop_latest.json");
    let markdown_path = results_dir.join("pd_vm_lua_hot_loop_latest.md");
    let artifact_text = serde_json::to_string_pretty(&artifact)
        .map_err(|err| format!("failed to serialize Lua benchmark artifact: {err}"))?;
    fs::write(&run_json_path, &artifact_text)
        .map_err(|err| format!("failed to write '{}': {err}", run_json_path.display()))?;
    fs::write(&latest_json_path, &artifact_text)
        .map_err(|err| format!("failed to write '{}': {err}", latest_json_path.display()))?;

    let mut markdown = String::new();
    markdown.push_str("# pd-vm Lua Hot Loop Benchmark\n\n");
    markdown.push_str("Methodology: compile outside timer, one warmup run outside timer, timed runs only over steady-state execution.\n\n");
    markdown.push_str("| mode | status | total_us | ns_per_inner_iter |\n");
    markdown.push_str("| --- | --- | ---: | ---: |\n");
    for sample in samples {
        let status = sample.skip_reason.as_deref().unwrap_or("ok");
        let total_us = sample
            .total_elapsed
            .map(|value| value.as_micros().to_string())
            .unwrap_or_else(|| "-".to_string());
        let normalized = sample
            .normalized_ns_per_inner_iter
            .map(|value| format!("{value:.2}"))
            .unwrap_or_else(|| "-".to_string());
        let _ = writeln!(
            &mut markdown,
            "| {} | {} | {} | {} |",
            sample.mode.label(),
            status,
            total_us,
            normalized
        );
    }
    fs::write(&markdown_path, markdown)
        .map_err(|err| format!("failed to write '{}': {err}", markdown_path.display()))?;
    Ok(())
}

struct HotLoopWorkload {
    program: Program,
    expected: i64,
}

fn build_hot_loop_workload(inner: i64, outer: i64) -> Result<HotLoopWorkload, String> {
    let source = format!(
        r#"
        let mut outer = 0;
        let mut i = 0;
        let mut sum = 0;
        while outer < {outer} {{
            i = 0;
            while i < {inner} {{
                let a = i + 7;
                let b = a - 3;
                let c = b * 8;
                let d = c / 8;
                let e = d + i;
                let n = 0 - e;
                let p = 0 - n;
                sum = sum + p;
                i = i + 1;
            }}
            outer = outer + 1;
        }}
        sum;
        "#
    );
    let compiled = compile_source(&source)
        .map_err(|err| format!("failed to compile hot loop workload: {err}"))?;
    let expected_per_outer = inner * inner + 3 * inner;
    Ok(HotLoopWorkload {
        program: compiled.program.with_local_count(compiled.locals),
        expected: outer * expected_per_outer,
    })
}

#[derive(Default)]
struct PerfNoopHost {
    _marker: u64,
}

impl HostFunction for PerfNoopHost {
    fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> Result<CallOutcome, VmError> {
        Ok(CallOutcome::Return(Vec::new()))
    }
}

#[derive(Clone, Copy, Debug)]
struct RssSample {
    mode: RssMode,
    vm_count: usize,
    before: Option<u64>,
    after: Option<u64>,
    avg_bytes_per_vm: Option<u64>,
}

impl RssSample {
    fn to_child_line(self) -> String {
        format!(
            "RSS_CHILD mode={} vm_count={} before={} after={} avg={}",
            self.mode.label(),
            self.vm_count,
            encode_option_u64(self.before),
            encode_option_u64(self.after),
            encode_option_u64(self.avg_bytes_per_vm),
        )
    }
}

fn measure_retained_rss_via_child(
    config: &BenchConfig,
    mode: RssMode,
) -> Result<RssSample, String> {
    let exe =
        std::env::current_exe().map_err(|err| format!("failed to locate current exe: {err}"))?;
    let output = Command::new(exe)
        .arg("--rss-child")
        .arg(mode.label())
        .arg("--rss-vms")
        .arg(config.rss_vm_count.to_string())
        .output()
        .map_err(|err| format!("failed to spawn RSS child for {} mode: {err}", mode.label()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "RSS child for {} mode failed with status {}: {}",
            mode.label(),
            output.status,
            stderr.trim()
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_rss_child_output(&stdout)
}

fn parse_rss_child_output(stdout: &str) -> Result<RssSample, String> {
    let line = stdout
        .lines()
        .find(|line| line.starts_with("RSS_CHILD "))
        .ok_or_else(|| format!("missing RSS_CHILD line in child output:\n{stdout}"))?;
    let mut mode = None;
    let mut vm_count = None;
    let mut before = None;
    let mut after = None;
    let mut avg = None;
    for part in line.split_whitespace().skip(1) {
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        match key {
            "mode" => {
                mode = RssMode::parse(value);
            }
            "vm_count" => {
                vm_count = Some(
                    value
                        .parse::<usize>()
                        .map_err(|err| format!("invalid vm_count in RSS child output: {err}"))?,
                );
            }
            "before" => before = Some(decode_option_u64(value)?),
            "after" => after = Some(decode_option_u64(value)?),
            "avg" => avg = Some(decode_option_u64(value)?),
            _ => {}
        }
    }
    Ok(RssSample {
        mode: mode.ok_or_else(|| format!("missing mode in RSS child output: {line}"))?,
        vm_count: vm_count
            .ok_or_else(|| format!("missing vm_count in RSS child output: {line}"))?,
        before: before.flatten(),
        after: after.flatten(),
        avg_bytes_per_vm: avg.flatten(),
    })
}

fn encode_option_u64(value: Option<u64>) -> String {
    match value {
        Some(value) => value.to_string(),
        None => "none".to_string(),
    }
}

fn decode_option_u64(value: &str) -> Result<Option<u64>, String> {
    if value == "none" {
        Ok(None)
    } else {
        value
            .parse::<u64>()
            .map(Some)
            .map_err(|err| format!("invalid u64 '{value}': {err}"))
    }
}

fn measure_retained_rss_for_mode(mode: RssMode, vm_count: usize) -> Result<RssSample, String> {
    let hot_loop = build_hot_loop_workload(DEFAULT_HOT_LOOP_INNER / 4, DEFAULT_HOT_LOOP_OUTER)?;
    let expected_stack = vec![Value::Int(hot_loop.expected)];
    let before = current_rss_bytes();
    let mut retained = Vec::with_capacity(vm_count);
    for _ in 0..vm_count {
        let mut vm = Vm::new(hot_loop.program.clone());
        configure_vm_for_mode(
            &mut vm,
            match mode {
                RssMode::Interpreter => PerfExecMode::Interpreter,
                RssMode::Jit => PerfExecMode::Jit,
            },
        );
        let status = vm
            .run()
            .map_err(|err| format!("RSS {} run failed: {err}", mode.label()))?;
        ensure_expected_completion(&vm, status, &expected_stack, mode.label())?;
        retained.push(vm);
    }
    black_box(&retained);
    let after = current_rss_bytes();
    let avg_bytes_per_vm = before
        .zip(after)
        .map(|(before, after)| after.saturating_sub(before) / vm_count as u64);
    Ok(RssSample {
        mode,
        vm_count,
        before,
        after,
        avg_bytes_per_vm,
    })
}

fn print_rss_sample(sample: &RssSample) {
    let mut line = format!(
        "  mode={:<12} retained_vms={:<6}",
        sample.mode.label(),
        sample.vm_count
    );
    match (sample.before, sample.after, sample.avg_bytes_per_vm) {
        (Some(before), Some(after), Some(avg)) => {
            let _ = write!(
                &mut line,
                " before={}B after={}B avg_per_vm={}B ({:.2} KiB)",
                before,
                after,
                avg,
                avg as f64 / 1024.0
            );
        }
        _ => {
            line.push_str(" rss=unsupported");
        }
    }
    println!("{line}");
}

fn build_compiler_stress_source(line_count: usize) -> String {
    let mut source = String::from(
        r#"
        let mut i = 0;
        let mut sum = 0;
        while i < 32 {
            sum = sum + i;
            i = i + 1;
        }
        "#,
    );
    for index in 0..line_count {
        let _ = writeln!(
            &mut source,
            "let value_{index} = sum + {index}; sum = sum + value_{index};"
        );
    }
    source.push_str("sum;\n");
    source
}

fn build_complex_rss_source() -> String {
    r#"
    fn keep(value) { value }

    let mut total = 0;
    for (let mut i = 0; i < 8; i = i + 1) {
        total = total + i;
    }

    let mut base = 7;
    let add = |value| value + base;
    let mut base = 8;
    let closure_value = add(5);

    let profile = {
        stats: {
            score: closure_value,
            values: [1, 2, 3, 4],
        },
        name: "rss",
    };
    let chained_score = profile?.stats?.score;
    let missing_score = profile?.missing?.value;
    let arr = profile.stats.values;

    let mut picked = match chained_score {
        12 => keep(chained_score),
        _ => 0,
    };

    let folded = if total > 0 => {
        picked = picked + arr[0];
        picked
    } else => {
        0
    };

    let mut while_i = 0;
    while while_i < 5 {
        total = total + while_i;
        while_i = while_i + 1;
    }

    let nested = |seed| seed + 3;
    let derived = nested(9);

    let final_value = if missing_score == null && folded > 0 => {
        derived + total
    } else => {
        0
    };
    final_value;
    "#
    .to_string()
}

fn build_load_stress_source(host_count: usize, source_locals: usize) -> String {
    let mut source = String::new();
    for index in 0..host_count {
        let _ = writeln!(&mut source, "fn host_{index}(x);");
    }
    source.push_str("let mut acc = 0;\n");
    for index in 0..source_locals {
        let _ = writeln!(&mut source, "let local_{index} = acc + {index};");
    }
    source.push_str("acc;\n");
    source
}

fn average_duration(samples: &[Duration]) -> Duration {
    let total_nanos = samples
        .iter()
        .fold(0u128, |acc, sample| acc.saturating_add(sample.as_nanos()));
    Duration::from_nanos((total_nanos / samples.len() as u128) as u64)
}

fn median_duration(samples: &mut [Duration]) -> Duration {
    samples.sort_unstable();
    samples[samples.len() / 2]
}

fn median_u64(samples: &mut [u64]) -> u64 {
    samples.sort_unstable();
    samples[samples.len() / 2]
}

fn native_jit_supported() -> bool {
    (cfg!(target_arch = "x86_64")
        && (cfg!(target_os = "windows") || (cfg!(unix) && !cfg!(target_os = "macos"))))
        || (cfg!(target_arch = "aarch64")
            && (cfg!(target_os = "linux") || cfg!(target_os = "macos")))
}

fn native_jit_config() -> JitConfig {
    JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 16_384,
    }
}

fn example_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("examples")
}

#[cfg(target_os = "windows")]
fn current_rss_bytes() -> Option<u64> {
    use std::mem::size_of;
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    let process = unsafe { GetCurrentProcess() };
    let mut counters: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        K32GetProcessMemoryInfo(
            process,
            &mut counters,
            size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        )
    };
    if ok == 0 {
        return None;
    }
    Some(counters.WorkingSetSize as u64)
}

#[cfg(unix)]
fn current_rss_bytes() -> Option<u64> {
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage as *mut libc::rusage) };
    if rc != 0 {
        return None;
    }
    #[cfg(target_os = "macos")]
    {
        Some(usage.ru_maxrss as u64)
    }
    #[cfg(not(target_os = "macos"))]
    {
        Some((usage.ru_maxrss as u64).saturating_mul(1024))
    }
}

#[cfg(not(any(unix, target_os = "windows")))]
fn current_rss_bytes() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_luajit_compare_output_reads_result_and_elapsed_ns() {
        let parsed = parse_luajit_compare_output("result=42 elapsed_ns=12345\n")
            .expect("parse should succeed");
        assert_eq!(parsed, (42, 12_345));
    }

    #[test]
    fn pd_vm_lua_compare_interpreter_records_methodology_and_profiles() {
        let source = build_lua_hot_loop_shared_source()
            .replace("__PDVM_HOT_LOOP_INNER__", "8")
            .replace("__PDVM_HOT_LOOP_OUTER__", "1");
        let compiled = compile_source_with_flavor(&source, SourceFlavor::Lua)
            .expect("lua source should compile");
        let sample = measure_pd_vm_lua_compare_mode(
            &compiled,
            LuaCompareMode::PdVmInterpreter,
            28,
            2,
            8,
            3,
        )
        .expect("interpreter benchmark should succeed");

        assert!(sample.available);
        assert_eq!(sample.warmup_runs, 1);
        assert_eq!(sample.timed_runs, 2);
        assert_eq!(sample.result, Some(28));
        assert!(sample.total_elapsed.is_some());
        assert!(!sample.interpreter_opcode_counts.is_empty());
    }

    #[test]
    fn pd_vm_lua_compare_jit_records_native_exec_delta() {
        if !native_jit_supported() {
            return;
        }

        let source = build_lua_hot_loop_shared_source()
            .replace("__PDVM_HOT_LOOP_INNER__", "16")
            .replace("__PDVM_HOT_LOOP_OUTER__", "1");
        let compiled = compile_source_with_flavor(&source, SourceFlavor::Lua)
            .expect("lua source should compile");
        let sample = measure_pd_vm_lua_compare_mode(
            &compiled,
            LuaCompareMode::PdVmJit,
            120,
            2,
            16,
            2,
        )
        .expect("jit benchmark should succeed");

        assert!(sample.available);
        assert!(
            sample.native_exec_delta.is_some_and(|value| value > 0),
            "expected native execution during timed section, got {sample:?}"
        );
        assert!(
            !sample.jit_runtime_diagnostics.is_empty(),
            "expected jit runtime diagnostics, got {sample:?}"
        );
        assert!(
            sample
                .native_trace_count_after
                .zip(sample.native_trace_count_before)
                .is_some_and(|(after, before)| after >= before),
            "expected trace count to stay stable or grow, got {sample:?}"
        );
    }
}
