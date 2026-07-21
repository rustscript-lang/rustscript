use std::hint::black_box;
use std::time::{Duration, Instant};

use vm::{
    CallOutcome, CallReturn, JitConfig, SourceFlavor, Value, Vm, VmStatus,
    compile_source_with_flavor,
};

const DEFAULT_COUNT: i64 = 1_000;
const DEFAULT_SAMPLES: usize = 7;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Jit,
    Aot,
}

impl Mode {
    fn label(self) -> &'static str {
        match self {
            Self::Jit => "jit",
            Self::Aot => "aot",
        }
    }
}

fn main() {
    if let Err(err) = run() {
        eprintln!("script-rs-bench aot harness failed: {err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut count = DEFAULT_COUNT;
    let mut samples = DEFAULT_SAMPLES;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--count" => {
                count = args
                    .next()
                    .ok_or_else(|| "missing --count".to_string())?
                    .parse::<i64>()
                    .map_err(|err| format!("bad --count: {err}"))?;
            }
            "--samples" => {
                samples = args
                    .next()
                    .ok_or_else(|| "missing --samples".to_string())?
                    .parse::<usize>()
                    .map_err(|err| format!("bad --samples: {err}"))?;
            }
            other => return Err(format!("unknown arg {other}")),
        }
    }
    if count <= 0 || samples == 0 {
        return Err("count and samples must be positive".to_string());
    }

    let source = build_source(count);
    let compiled = compile_source_with_flavor(&source, SourceFlavor::RustScript)
        .map_err(|err| format!("compile failed: {err}"))?;
    let program = compiled.program.with_local_count(compiled.locals);

    for mode in [Mode::Jit, Mode::Aot] {
        let mut times = Vec::with_capacity(samples);
        let mut bridge = Vec::new();
        let mut aot_execs = 0;
        let mut jit_execs = 0;
        for sample in 0..samples {
            let mut vm = Vm::new(program.clone());
            configure_vm(&mut vm, mode)?;
            let started = Instant::now();
            let status = vm
                .run()
                .map_err(|err| format!("{} run failed: {err}", mode.label()))?;
            let elapsed = started.elapsed();
            verify(&vm, status, count)?;
            if sample + 1 == samples {
                bridge = vm.jit_native_bridge_stats_snapshot();
                aot_execs = vm.aot_exec_count();
                jit_execs = vm.jit_native_exec_count();
            }
            black_box(vm.stack());
            times.push(elapsed);
        }
        times.sort_unstable();
        let median = times[times.len() / 2];
        println!(
            "mode={} count={} median_us={} aot_execs={} jit_execs={} bridge={:?} samples_us={}",
            mode.label(),
            count,
            median.as_micros(),
            aot_execs,
            jit_execs,
            bridge,
            format_samples(&times)
        );
    }
    Ok(())
}

fn configure_vm(vm: &mut Vm, mode: Mode) -> Result<(), String> {
    vm.bind_static_non_yielding_args_function("rand", rand_host);
    vm.bind_static_non_yielding_args_function("RustData_new", data_new_host);
    vm.bind_static_non_yielding_args_function("lt", lt_host);
    vm.set_jit_native_bridge_stats_enabled(true);
    match mode {
        Mode::Jit => vm.set_jit_config(JitConfig {
            enabled: true,
            hot_loop_threshold: 1,
            max_trace_len: 16_384,
        }),
        Mode::Aot => vm
            .compile_aot()
            .map_err(|err| format!("aot compile failed: {err}"))?,
    }
    Ok(())
}

fn rand_host(args: &[Value]) -> vm::VmResult<CallOutcome> {
    let n = expect_int(args, 0, "rand")?.max(1);
    // Deterministic LCG-shaped value; enough to keep the benchmark repeatable.
    let v = (n.wrapping_mul(1_103_515_245).wrapping_add(12_345) & 0x7fff_ffff) % n;
    Ok(CallOutcome::Return(CallReturn::one(Value::Int(v))))
}

fn data_new_host(args: &[Value]) -> vm::VmResult<CallOutcome> {
    let x = expect_int(args, 0, "RustData_new")?;
    Ok(CallOutcome::Return(CallReturn::one(Value::Int(x))))
}

fn lt_host(args: &[Value]) -> vm::VmResult<CallOutcome> {
    let lhs = expect_int(args, 0, "lt")?;
    let rhs = expect_int(args, 1, "lt")?;
    Ok(CallOutcome::Return(CallReturn::one(Value::Bool(lhs < rhs))))
}

fn expect_int(args: &[Value], index: usize, name: &str) -> vm::VmResult<i64> {
    match args.get(index) {
        Some(Value::Int(value)) => Ok(*value),
        other => Err(vm::VmError::HostError(format!(
            "{name} expected int arg {index}, got {other:?}"
        ))),
    }
}

fn verify(vm: &Vm, status: VmStatus, count: i64) -> Result<(), String> {
    if status != VmStatus::Halted {
        return Err(format!("expected halt, got {status:?}"));
    }
    match vm.stack() {
        [Value::Array(values)] if values.len() == count as usize => {
            for pair in values.windows(2) {
                let [Value::Int(lhs), Value::Int(rhs)] = pair else {
                    return Err(format!("unexpected pair {pair:?}"));
                };
                if lhs > rhs {
                    return Err(format!("array not sorted: {lhs} > {rhs}"));
                }
            }
            Ok(())
        }
        other => Err(format!("unexpected stack {other:?}")),
    }
}

fn format_samples(samples: &[Duration]) -> String {
    samples
        .iter()
        .map(|sample| sample.as_micros().to_string())
        .collect::<Vec<_>>()
        .join(",")
}

fn build_source(count: i64) -> String {
    format!(
        r#"
fn RustData_new(v: int) -> int;
fn lt(lhs: int, rhs: int) -> bool;

let mut array: int[] = [];
for i in 0..{count} {{
    let value = (i * 1103515245 + 12345) % {count};
    array[array.length] = RustData_new(value);
}}

let mut i = 1;
while i < array.length {{
    let key = array[i].copy();
    let mut j = i - 1;
    let mut scanning = true;
    while scanning {{
        if lt(key, array[j].copy()) {{
            array[j + 1] = array[j].copy();
            if j == 0 {{
                scanning = false;
            }} else {{
                j = j - 1;
            }}
        }} else {{
            scanning = false;
        }}
    }}
    if j == 0 && lt(key, array[0].copy()) {{
        array[0] = key;
    }} else {{
        array[j + 1] = key;
    }}
    i = i + 1;
}}
array;
"#
    )
}
