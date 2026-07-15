use std::env;
use std::hint::black_box;
use std::time::{Duration, Instant};

use vm::{JitConfig, JitTraceTerminal, Program, Value, Vm, VmStatus, compile_source};

const DEFAULT_WIDTH: usize = 256;
const DEFAULT_ITERATIONS: usize = 50_000;
const DEFAULT_SAMPLES: usize = 15;

#[derive(Clone, Copy, Debug)]
enum Workload {
    Array,
    Map,
}

impl Workload {
    fn label(self) -> &'static str {
        match self {
            Self::Array => "array",
            Self::Map => "map",
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum ExecMode {
    Interpreter,
    Jit,
}

impl ExecMode {
    fn label(self) -> &'static str {
        match self {
            Self::Interpreter => "interpreter",
            Self::Jit => "jit",
        }
    }
}

struct Measurement {
    samples: Vec<Duration>,
    native_traces: usize,
    call_boundary_traces: usize,
    loop_back_traces: usize,
    native_execs: u64,
    trace_exits: u64,
    native_loop_backs: u64,
    helper_fallbacks: u64,
    generic_builtin_calls: u64,
}

#[derive(Clone, Copy, Debug)]
struct Config {
    width: usize,
    iterations: usize,
    samples: usize,
    jit: bool,
    jit_only: bool,
}

impl Config {
    fn parse() -> Result<Self, String> {
        let mut config = Self {
            width: DEFAULT_WIDTH,
            iterations: DEFAULT_ITERATIONS,
            samples: DEFAULT_SAMPLES,
            jit: false,
            jit_only: false,
        };
        let mut args = env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--width" => config.width = parse_usize("--width", args.next())?,
                "--iterations" => config.iterations = parse_usize("--iterations", args.next())?,
                "--samples" => config.samples = parse_usize("--samples", args.next())?,
                "--jit" => config.jit = true,
                "--jit-only" => {
                    config.jit = true;
                    config.jit_only = true;
                }
                other => return Err(format!("unknown argument '{other}'")),
            }
        }
        if config.width == 0 || config.iterations == 0 || config.samples == 0 {
            return Err("width, iterations, and samples must be positive".to_string());
        }
        Ok(config)
    }
}

fn parse_usize(flag: &str, value: Option<String>) -> Result<usize, String> {
    value
        .ok_or_else(|| format!("missing value for {flag}"))?
        .parse::<usize>()
        .map_err(|err| format!("invalid value for {flag}: {err}"))
}

fn main() {
    if let Err(err) = run() {
        eprintln!("collection rebind benchmark failed: {err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let config = Config::parse()?;
    let modes: &[ExecMode] = if config.jit_only {
        &[ExecMode::Jit]
    } else if config.jit {
        &[ExecMode::Interpreter, ExecMode::Jit]
    } else {
        &[ExecMode::Interpreter]
    };

    for workload in [Workload::Array, Workload::Map] {
        let program = compile_workload(workload, config.width, config.iterations)?;
        for mode in modes {
            let measurement = measure(&program, workload, *mode, config)?;
            let mut sorted = measurement.samples.clone();
            sorted.sort_unstable();
            let median = sorted[sorted.len() / 2];
            let sample_ns = measurement
                .samples
                .iter()
                .map(Duration::as_nanos)
                .map(|value| value.to_string())
                .collect::<Vec<_>>()
                .join(",");
            println!(
                "workload={} mode={} width={} iterations={} warmup_runs=1 reused_vm=true median_ns={} native_traces={} call_boundary_traces={} loop_back_traces={} native_execs={} trace_exits={} native_loop_backs={} helper_fallbacks={} generic_builtin_calls={} samples_ns={}",
                workload.label(),
                mode.label(),
                config.width,
                config.iterations,
                median.as_nanos(),
                measurement.native_traces,
                measurement.call_boundary_traces,
                measurement.loop_back_traces,
                measurement.native_execs,
                measurement.trace_exits,
                measurement.native_loop_backs,
                measurement.helper_fallbacks,
                measurement.generic_builtin_calls,
                sample_ns
            );
        }
    }
    Ok(())
}

fn compile_workload(
    workload: Workload,
    width: usize,
    iterations: usize,
) -> Result<Program, String> {
    let literal = match workload {
        Workload::Array => format!("[{}]", vec!["0"; width].join(",")),
        Workload::Map => format!(
            "{{{}}}",
            (0..width)
                .map(|index| format!("{index}: 0"))
                .collect::<Vec<_>>()
                .join(",")
        ),
    };
    let source = format!(
        r#"
        let mut values = {literal};
        let mut i = 0;
        while i < {iterations} {{
            let index = i % {width};
            values[index] = i;
            i = i + 1;
        }}
        values[0];
        "#
    );
    let compiled = compile_source(&source)
        .map_err(|err| format!("failed to compile {} workload: {err}", workload.label()))?;
    Ok(compiled.program.with_local_count(compiled.locals))
}

fn measure(
    program: &Program,
    workload: Workload,
    mode: ExecMode,
    config: Config,
) -> Result<Measurement, String> {
    let mut vm = configured_vm(program, mode);
    let warmup = vm
        .run()
        .map_err(|err| format!("{} {} warmup failed: {err}", workload.label(), mode.label()))?;
    verify_result(&vm, warmup, config)?;
    if matches!(mode, ExecMode::Jit) && vm.jit_native_exec_count() == 0 {
        return Err(format!(
            "{} warmup did not execute native traces:\n{}",
            workload.label(),
            vm.dump_jit_info()
        ));
    }

    let snapshot_before = vm.jit_snapshot();
    let metrics_before = snapshot_before.metrics;
    let native_traces = snapshot_before.traces.len();
    let call_boundary_traces = snapshot_before
        .traces
        .iter()
        .filter(|trace| trace.has_call)
        .count();
    let loop_back_traces = snapshot_before
        .traces
        .iter()
        .filter(|trace| trace.terminal == JitTraceTerminal::LoopBack)
        .count();
    let mut samples = Vec::with_capacity(config.samples);
    let mut generic_builtin_calls = 0u64;
    for _ in 0..config.samples {
        vm.reset_for_reuse();
        let started = Instant::now();
        let status = vm
            .run()
            .map_err(|err| format!("{} {} run failed: {err}", workload.label(), mode.label()))?;
        let elapsed = started.elapsed();
        verify_result(&vm, status, config)?;
        generic_builtin_calls = generic_builtin_calls
            .saturating_add(vm.interpreter_metrics_snapshot().generic_builtin_call_count);
        black_box(vm.stack());
        samples.push(elapsed);
    }
    let snapshot_after = vm.jit_snapshot();
    if snapshot_after.traces.len() != native_traces {
        return Err(format!(
            "{} {} compiled additional traces during measured runs: warmup={} measured={}",
            workload.label(),
            mode.label(),
            native_traces,
            snapshot_after.traces.len()
        ));
    }
    let metrics_after = snapshot_after.metrics;
    Ok(Measurement {
        samples,
        native_traces,
        call_boundary_traces,
        loop_back_traces,
        native_execs: metrics_after
            .native_trace_exec_count
            .saturating_sub(metrics_before.native_trace_exec_count),
        trace_exits: metrics_after
            .trace_exit_count
            .saturating_sub(metrics_before.trace_exit_count),
        native_loop_backs: metrics_after
            .native_loop_back_count
            .saturating_sub(metrics_before.native_loop_back_count),
        helper_fallbacks: metrics_after
            .helper_fallback_count
            .saturating_sub(metrics_before.helper_fallback_count),
        generic_builtin_calls,
    })
}

fn configured_vm(program: &Program, mode: ExecMode) -> Vm {
    let mut vm = Vm::new(program.clone());
    vm.set_jit_config(JitConfig {
        enabled: matches!(mode, ExecMode::Jit),
        hot_loop_threshold: 1,
        max_trace_len: 16_384,
    });
    vm
}

fn verify_result(vm: &Vm, status: VmStatus, config: Config) -> Result<(), String> {
    if status != VmStatus::Halted {
        return Err(format!("expected halted VM, got {status:?}"));
    }
    let expected = ((config.iterations - 1) / config.width * config.width) as i64;
    if vm.stack() != [Value::Int(expected)] {
        return Err(format!(
            "unexpected result: expected {expected}, got {:?}",
            vm.stack()
        ));
    }
    Ok(())
}
