use std::env;
use std::hint::black_box;
use std::time::{Duration, Instant};

use vm::{JitConfig, Program, Value, Vm, VmStatus, compile_source};

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

#[derive(Clone, Copy, Debug)]
struct Config {
    width: usize,
    iterations: usize,
    samples: usize,
    jit: bool,
}

impl Config {
    fn parse() -> Result<Self, String> {
        let mut config = Self {
            width: DEFAULT_WIDTH,
            iterations: DEFAULT_ITERATIONS,
            samples: DEFAULT_SAMPLES,
            jit: false,
        };
        let mut args = env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--width" => config.width = parse_usize("--width", args.next())?,
                "--iterations" => config.iterations = parse_usize("--iterations", args.next())?,
                "--samples" => config.samples = parse_usize("--samples", args.next())?,
                "--jit" => config.jit = true,
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
    let modes: &[ExecMode] = if config.jit {
        &[ExecMode::Interpreter, ExecMode::Jit]
    } else {
        &[ExecMode::Interpreter]
    };

    for workload in [Workload::Array, Workload::Map] {
        let program = compile_workload(workload, config.width, config.iterations)?;
        for mode in modes {
            let samples = measure(&program, workload, *mode, config)?;
            let mut sorted = samples.clone();
            sorted.sort_unstable();
            let median = sorted[sorted.len() / 2];
            let sample_ns = samples
                .iter()
                .map(Duration::as_nanos)
                .map(|value| value.to_string())
                .collect::<Vec<_>>()
                .join(",");
            println!(
                "workload={} mode={} width={} iterations={} median_ns={} samples_ns={}",
                workload.label(),
                mode.label(),
                config.width,
                config.iterations,
                median.as_nanos(),
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
) -> Result<Vec<Duration>, String> {
    run_once(program, workload, mode, config)?;
    let mut samples = Vec::with_capacity(config.samples);
    for _ in 0..config.samples {
        let mut vm = configured_vm(program, mode);
        let started = Instant::now();
        let status = vm
            .run()
            .map_err(|err| format!("{} {} run failed: {err}", workload.label(), mode.label()))?;
        let elapsed = started.elapsed();
        verify_result(&vm, status, config)?;
        black_box(vm.stack());
        samples.push(elapsed);
    }
    Ok(samples)
}

fn run_once(
    program: &Program,
    workload: Workload,
    mode: ExecMode,
    config: Config,
) -> Result<(), String> {
    let mut vm = configured_vm(program, mode);
    let status = vm
        .run()
        .map_err(|err| format!("{} {} warmup failed: {err}", workload.label(), mode.label()))?;
    verify_result(&vm, status, config)
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
