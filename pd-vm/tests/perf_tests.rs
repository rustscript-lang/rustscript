#![cfg(feature = "runtime")]
use std::hint::black_box;
use std::time::Instant;

use vm::{
    CallOutcome, HostFunction, HostFunctionRegistry, JitConfig, OpCode, Program, Value, Vm,
    VmStatus, compile_source, compile_source_file,
};

struct PerfNoopHost {
    _marker: u64,
}

impl HostFunction for PerfNoopHost {
    fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> Result<CallOutcome, vm::VmError> {
        Ok(CallOutcome::Return(Vec::new()))
    }
}

fn perf_noop_host_static(_vm: &mut Vm, _args: &[Value]) -> Result<CallOutcome, vm::VmError> {
    Ok(CallOutcome::Return(Vec::new()))
}

#[test]
#[ignore = "performance characterization test; run manually"]
fn perf_vm_creation_cleanup_speed_and_ram_usage() {
    let iterations = 25_000usize;
    let retained_count = 8_000usize;
    let program = Program::new(
        vec![Value::Int(1)],
        vec![OpCode::Ldc as u8, 0, 0, 0, 0, OpCode::Ret as u8],
    );

    let rss_before = current_rss_bytes();
    let started = Instant::now();
    for _ in 0..iterations {
        let vm = Vm::new(program.clone().with_local_count(64));
        black_box(vm);
    }
    let elapsed = started.elapsed();
    let rss_after = current_rss_bytes();
    let per_vm_ram = rss_before
        .zip(rss_after)
        .map(|(before, after)| after.saturating_sub(before) / iterations as u64);

    println!(
        "vm creation/cleanup: iterations={iterations}, elapsed_ms={}, per_vm_ns={}",
        elapsed.as_millis(),
        elapsed.as_nanos() / iterations as u128
    );
    print_rss_delta("vm creation/cleanup", rss_before, rss_after);
    if let Some(per_vm) = per_vm_ram {
        println!(
            "vm creation/cleanup avg net rss growth per vm: {}B ({:.2} KiB)",
            per_vm,
            per_vm as f64 / 1024.0
        );
    } else {
        println!("vm creation/cleanup avg net rss growth per vm: unsupported on this platform");
    }

    let retained_rss_before = current_rss_bytes();
    let mut retained_vms = Vec::with_capacity(retained_count);
    for _ in 0..retained_count {
        retained_vms.push(Vm::new(program.clone().with_local_count(64)));
    }
    black_box(&retained_vms);
    let retained_rss_after = current_rss_bytes();
    print_rss_delta("vm retained batch", retained_rss_before, retained_rss_after);
    if let Some(per_vm) = retained_rss_before
        .zip(retained_rss_after)
        .map(|(before, after)| after.saturating_sub(before) / retained_count as u64)
    {
        println!(
            "vm retained avg ram per vm: {}B ({:.2} KiB) across {} retained vms",
            per_vm,
            per_vm as f64 / 1024.0,
            retained_count
        );
    } else {
        println!("vm retained avg ram per vm: unsupported on this platform");
    }

    drop(retained_vms);
    let retained_rss_after_drop = current_rss_bytes();
    print_rss_delta(
        "vm retained batch after drop",
        retained_rss_after,
        retained_rss_after_drop,
    );

    let host_import_count = 32usize;
    let host_iterations = 6_000usize;
    let host_source = build_host_import_stress_source(host_import_count);
    let host_compiled = compile_source(&host_source).expect("host import compile should succeed");
    let host_names: Vec<String> = host_compiled
        .functions
        .iter()
        .map(|func| func.name.clone())
        .collect();
    assert_eq!(host_names.len(), host_import_count);

    let plain_compiled = compile_source("let v = 1; v;").expect("plain compile should succeed");

    let plain_rss_before = current_rss_bytes();
    let plain_started = Instant::now();
    for _ in 0..host_iterations {
        let mut vm = Vm::new(plain_compiled.program.clone());
        let status = vm.run().expect("plain vm run should succeed");
        assert_eq!(status, VmStatus::Halted);
        black_box(vm.stack());
    }
    let plain_elapsed = plain_started.elapsed();
    let plain_rss_after = current_rss_bytes();
    print_rss_delta(
        "host overhead baseline (plain run)",
        plain_rss_before,
        plain_rss_after,
    );

    let host_rss_before = current_rss_bytes();
    let host_started = Instant::now();
    for _ in 0..host_iterations {
        let mut vm = Vm::new(host_compiled.program.clone());
        for name in &host_names {
            vm.bind_function(name, Box::new(PerfNoopHost { _marker: 0 }));
        }
        let status = vm.run().expect("host import vm run should succeed");
        assert_eq!(status, VmStatus::Halted);
        black_box(vm.stack());
    }
    let host_elapsed = host_started.elapsed();
    let host_rss_after = current_rss_bytes();
    print_rss_delta(
        "host overhead (bind + import load)",
        host_rss_before,
        host_rss_after,
    );

    let plain_per_vm_ns = plain_elapsed.as_nanos() / host_iterations as u128;
    let host_per_vm_ns = host_elapsed.as_nanos() / host_iterations as u128;
    let overhead_per_vm_ns = host_per_vm_ns.saturating_sub(plain_per_vm_ns);
    let overhead_per_import_ns = overhead_per_vm_ns / host_import_count as u128;
    println!(
        "host register/load overhead: iterations={host_iterations}, imports_per_vm={host_import_count}, plain_per_vm_ns={plain_per_vm_ns}, host_per_vm_ns={host_per_vm_ns}, overhead_per_vm_ns={overhead_per_vm_ns}, overhead_per_import_ns={overhead_per_import_ns}",
    );

    let mut registry = HostFunctionRegistry::new();
    for name in &host_names {
        registry.register(name.clone(), 1, || Box::new(PerfNoopHost { _marker: 0 }));
    }
    let cached_plan = registry
        .prepare_plan(&host_compiled.program.imports)
        .expect("cached host plan should build");

    let cached_rss_before = current_rss_bytes();
    let cached_started = Instant::now();
    for _ in 0..host_iterations {
        let mut vm = Vm::new(host_compiled.program.clone());
        registry
            .bind_vm_with_plan(&mut vm, &cached_plan)
            .expect("cached host binding should succeed");
        let status = vm.run().expect("cached host import vm run should succeed");
        assert_eq!(status, VmStatus::Halted);
        black_box(vm.stack());
    }
    let cached_elapsed = cached_started.elapsed();
    let cached_rss_after = current_rss_bytes();
    print_rss_delta(
        "host overhead (cached bind + cached import load)",
        cached_rss_before,
        cached_rss_after,
    );

    let cached_per_vm_ns = cached_elapsed.as_nanos() / host_iterations as u128;
    let cached_overhead_per_vm_ns = cached_per_vm_ns.saturating_sub(plain_per_vm_ns);
    let cached_overhead_per_import_ns = cached_overhead_per_vm_ns / host_import_count as u128;
    println!(
        "host cache overhead: iterations={host_iterations}, imports_per_vm={host_import_count}, plain_per_vm_ns={plain_per_vm_ns}, cached_per_vm_ns={cached_per_vm_ns}, overhead_per_vm_ns={cached_overhead_per_vm_ns}, overhead_per_import_ns={cached_overhead_per_import_ns}",
    );

    let mut static_registry = HostFunctionRegistry::new();
    for name in &host_names {
        static_registry.register_static(name.clone(), 1, perf_noop_host_static);
    }
    let static_cached_plan = static_registry
        .prepare_plan(&host_compiled.program.imports)
        .expect("static host plan should build");

    let static_cached_rss_before = current_rss_bytes();
    let static_cached_started = Instant::now();
    for _ in 0..host_iterations {
        let mut vm = Vm::new(host_compiled.program.clone());
        static_registry
            .bind_vm_with_plan(&mut vm, &static_cached_plan)
            .expect("cached static host binding should succeed");
        let status = vm.run().expect("cached static host vm run should succeed");
        assert_eq!(status, VmStatus::Halted);
        black_box(vm.stack());
    }
    let static_cached_elapsed = static_cached_started.elapsed();
    let static_cached_rss_after = current_rss_bytes();
    print_rss_delta(
        "host overhead (cached static fn ptr)",
        static_cached_rss_before,
        static_cached_rss_after,
    );

    let static_cached_per_vm_ns = static_cached_elapsed.as_nanos() / host_iterations as u128;
    let static_cached_overhead_per_vm_ns = static_cached_per_vm_ns.saturating_sub(plain_per_vm_ns);
    let static_cached_overhead_per_import_ns =
        static_cached_overhead_per_vm_ns / host_import_count as u128;
    println!(
        "host static fn ptr overhead: iterations={host_iterations}, imports_per_vm={host_import_count}, plain_per_vm_ns={plain_per_vm_ns}, static_cached_per_vm_ns={static_cached_per_vm_ns}, overhead_per_vm_ns={static_cached_overhead_per_vm_ns}, overhead_per_import_ns={static_cached_overhead_per_import_ns}",
    );
}

#[test]
#[ignore = "performance characterization test; run manually"]
fn perf_compiler_speed_and_ram_usage() {
    let iterations = 200usize;
    let source = build_compiler_stress_source(1_000);

    let rss_before = current_rss_bytes();
    let started = Instant::now();
    for _ in 0..iterations {
        let compiled = compile_source(&source).expect("compile should succeed");
        black_box(compiled.locals);
    }
    let elapsed = started.elapsed();
    let per_compile_us = elapsed.as_micros() / iterations as u128;

    println!(
        "compiler perf: iterations={iterations}, elapsed_ms={}, per_compile_us={}",
        elapsed.as_millis(),
        per_compile_us
    );
    print_rss_delta("compiler", rss_before, current_rss_bytes());
}

#[test]
fn jit_emitted_machine_code_is_executed_on_native_targets() {
    let source = r#"
        let mut i = 0;
        let mut sum = 0;
        while i < 200 {
            sum = sum + i;
            i = i + 1;
        }
        sum;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program);
    vm.set_jit_config(JitConfig {
        enabled: native_jit_supported(),
        hot_loop_threshold: 1,
        max_trace_len: 1_024,
    });

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(19_900)]);

    if native_jit_supported() {
        let native_trace_count = vm.jit_native_trace_count();
        let native_exec_count = vm.jit_native_exec_count();
        let dump = vm.dump_jit_info();

        assert!(
            native_trace_count > 0,
            "expected native traces > 0, dump:\n{dump}"
        );
        assert!(
            native_exec_count > 0,
            "expected native execution count > 0, dump:\n{dump}"
        );
        assert!(
            dump.contains("native codegen backend:"),
            "missing native backend line"
        );
        assert!(dump.contains("native trace#"), "missing native trace entry");
        assert!(dump.contains("code:"), "missing native machine code bytes");
    }
}

#[test]
#[ignore = "performance characterization test; run manually"]
fn perf_jit_native_reduces_tight_loop_latency() {
    if !native_jit_supported() {
        println!("skipping latency comparison on unsupported native JIT target");
        return;
    }

    const INNER_LOOP_ITERS: i64 = 40_000;
    const OUTER_LOOPS: i64 = 8;
    const TRIALS: usize = 7;
    let source = format!(
        r#"
        let mut outer = 0;
        let mut i = 0;
        let mut sum = 0;
        while outer < {OUTER_LOOPS} {{
            i = 0;
            while i < {INNER_LOOP_ITERS} {{
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

    let compiled = compile_source(&source).expect("compile should succeed");
    let expected_per_outer = INNER_LOOP_ITERS * INNER_LOOP_ITERS + 3 * INNER_LOOP_ITERS;
    let expected = OUTER_LOOPS * expected_per_outer;
    let expected_stack = vec![Value::Int(expected)];

    let warmup_interpreter = run_sum_loop_with_mode(
        &compiled.program,
        compiled.locals,
        PerfExecMode::Interpreter,
        expected,
    );
    let warmup_jit = run_sum_loop_with_mode(
        &compiled.program,
        compiled.locals,
        PerfExecMode::Jit,
        expected,
    );
    let warmup_aot = run_sum_loop_with_mode(
        &compiled.program,
        compiled.locals,
        PerfExecMode::Aot,
        expected,
    );
    assert_eq!(warmup_interpreter.stack, expected_stack);
    assert_eq!(warmup_jit.stack, warmup_interpreter.stack);
    assert_eq!(warmup_aot.stack, warmup_interpreter.stack);

    let mut interpreter_times = Vec::with_capacity(TRIALS);
    let mut jit_times = Vec::with_capacity(TRIALS);
    let mut aot_times = Vec::with_capacity(TRIALS);
    for trial in 0..TRIALS {
        let interpreter_run = run_sum_loop_with_mode(
            &compiled.program,
            compiled.locals,
            PerfExecMode::Interpreter,
            expected,
        );
        let jit_run = run_sum_loop_with_mode(
            &compiled.program,
            compiled.locals,
            PerfExecMode::Jit,
            expected,
        );
        let aot_run = run_sum_loop_with_mode(
            &compiled.program,
            compiled.locals,
            PerfExecMode::Aot,
            expected,
        );

        assert_eq!(
            interpreter_run.stack, expected_stack,
            "interpreter result mismatch on trial {trial}",
        );
        assert_eq!(
            jit_run.stack, interpreter_run.stack,
            "native JIT result mismatch on trial {trial}",
        );
        assert_eq!(
            aot_run.stack, interpreter_run.stack,
            "AOT result mismatch on trial {trial}",
        );

        interpreter_times.push(interpreter_run.elapsed);
        jit_times.push(jit_run.elapsed);
        aot_times.push(aot_run.elapsed);
    }

    let interpreter_median = median_duration(&mut interpreter_times);
    let jit_median = median_duration(&mut jit_times);
    let aot_median = median_duration(&mut aot_times);
    let jit_speedup = interpreter_median.as_secs_f64() / jit_median.as_secs_f64();
    let aot_speedup = interpreter_median.as_secs_f64() / aot_median.as_secs_f64();

    println!(
        "tight-loop latency median: interpreter={}ms jit={}ms aot={}ms jit_speedup={:.2}x aot_speedup={:.2}x",
        interpreter_median.as_millis(),
        jit_median.as_millis(),
        aot_median.as_millis(),
        jit_speedup,
        aot_speedup,
    );

    assert!(
        jit_median < interpreter_median,
        "expected JIT median latency to be lower (interpreter={:?}, jit={:?})",
        interpreter_median,
        jit_median
    );
}

#[test]
#[ignore = "manual run performance test; run explicitly"]
fn perf_manual_aes_128_cbc_rustscript_matches_in_interpreter_jit_and_aot() {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/aes_128_cbc_usage.rss");
    let compiled = compile_source_file(&path).expect("aes RustScript usage example should compile");

    let expected = vec![Value::String(
        "7649abac8119b246cee98e9b12e9197d".to_string(),
    )];

    const TRIALS: usize = 7;
    let diag_enabled = std::env::var_os("PDVM_PERF_AES_JIT_DIAG").is_some();
    let mut interpreter_times = Vec::with_capacity(TRIALS);
    let mut jit_times = Vec::with_capacity(TRIALS);
    let mut aot_times = Vec::with_capacity(TRIALS);
    let mut aot_prepare_times = Vec::<std::time::Duration>::with_capacity(TRIALS);
    let mut jit_attempts_total = 0usize;
    let mut jit_traces_total = 0usize;
    let mut jit_nyi_total = 0usize;
    let mut jit_native_exec_total = 0u64;
    let mut aot_prepared_total = 0usize;
    let mut aot_native_exec_total = 0u64;

    for trial in 0..TRIALS {
        let mut vm_interpreter = Vm::new(compiled.program.clone());
        vm_interpreter.set_jit_config(JitConfig {
            enabled: false,
            hot_loop_threshold: 1,
            max_trace_len: 16_384,
        });
        let interpreter_started = Instant::now();
        let interpreter_status = vm_interpreter
            .run()
            .expect("aes RustScript example should run in interpreter mode");
        let interpreter_elapsed = interpreter_started.elapsed();
        assert_eq!(
            interpreter_status,
            VmStatus::Halted,
            "interpreter should halt on trial {trial}"
        );
        assert_eq!(
            vm_interpreter.stack(),
            expected.as_slice(),
            "interpreter result mismatch on trial {trial}"
        );
        interpreter_times.push(interpreter_elapsed);

        let mut vm_jit = Vm::new(compiled.program.clone());
        vm_jit.set_jit_config(JitConfig {
            enabled: true,
            hot_loop_threshold: 1,
            max_trace_len: 16_384,
        });
        let jit_started = Instant::now();
        let jit_status = vm_jit
            .run()
            .expect("aes RustScript example should run in jit mode");
        let jit_elapsed = jit_started.elapsed();
        assert_eq!(
            jit_status,
            VmStatus::Halted,
            "jit should halt on trial {trial}"
        );
        assert_eq!(
            vm_jit.stack(),
            expected.as_slice(),
            "jit result mismatch on trial {trial}"
        );
        assert_eq!(
            vm_jit.stack(),
            vm_interpreter.stack(),
            "interpreter/jit stack mismatch on trial {trial}"
        );
        let snapshot = vm_jit.jit_snapshot();
        let trial_attempts = snapshot.attempts.len();
        let trial_traces = snapshot.traces.len();
        let trial_nyi = snapshot
            .attempts
            .iter()
            .filter(|attempt| attempt.result.is_err())
            .count();
        let trial_native_exec = vm_jit.jit_native_exec_count();
        jit_attempts_total = jit_attempts_total.saturating_add(trial_attempts);
        jit_traces_total = jit_traces_total.saturating_add(trial_traces);
        jit_nyi_total = jit_nyi_total.saturating_add(trial_nyi);
        jit_native_exec_total = jit_native_exec_total.saturating_add(trial_native_exec);

        if diag_enabled && trial == 0 {
            let traces_with_calls = snapshot
                .traces
                .iter()
                .filter(|trace| trace.has_call)
                .count();
            let traces_with_yielding_calls = snapshot
                .traces
                .iter()
                .filter(|trace| trace.has_yielding_call)
                .count();
            let helper_like_steps = snapshot
                .traces
                .iter()
                .flat_map(|trace| trace.steps.iter())
                .filter(|step| {
                    matches!(
                        step,
                        vm::jit::TraceStep::Ldc(_)
                            | vm::jit::TraceStep::Pop
                            | vm::jit::TraceStep::Dup
                            | vm::jit::TraceStep::Ldloc(_)
                            | vm::jit::TraceStep::Stloc(_)
                            | vm::jit::TraceStep::Call { .. }
                    )
                })
                .count();
            let total_steps = snapshot
                .traces
                .iter()
                .map(|trace| trace.steps.len())
                .sum::<usize>();
            println!(
                "aes jit trial0 diagnostics: attempts={} traces={} nyi={} native_exec={} helper_like_steps={} total_steps={} traces_with_calls={} traces_with_yielding_calls={}",
                trial_attempts,
                trial_traces,
                trial_nyi,
                trial_native_exec,
                helper_like_steps,
                total_steps,
                traces_with_calls,
                traces_with_yielding_calls
            );
            if std::env::var_os("PDVM_PERF_AES_JIT_DUMP").is_some() {
                println!("aes jit trial0 dump:\n{}", vm_jit.dump_jit_info());
            }
        }
        jit_times.push(jit_elapsed);

        let mut vm_aot = Vm::new(compiled.program.clone());
        vm_aot.set_jit_config(JitConfig {
            enabled: true,
            hot_loop_threshold: 1,
            max_trace_len: 16_384,
        });
        let aot_prepare_started = Instant::now();
        let aot_prepared = vm_aot
            .prepare_aot()
            .expect("aes RustScript example should AOT precompile");
        let aot_prepare_elapsed = aot_prepare_started.elapsed();
        let aot_started = Instant::now();
        let aot_status = vm_aot
            .run()
            .expect("aes RustScript example should run in aot mode");
        let aot_elapsed = aot_started.elapsed();
        assert_eq!(
            aot_status,
            VmStatus::Halted,
            "aot should halt on trial {trial}"
        );
        assert_eq!(
            vm_aot.stack(),
            expected.as_slice(),
            "aot result mismatch on trial {trial}"
        );
        assert_eq!(
            vm_aot.stack(),
            vm_interpreter.stack(),
            "interpreter/aot stack mismatch on trial {trial}"
        );
        assert_eq!(
            vm_aot.stack(),
            vm_jit.stack(),
            "jit/aot stack mismatch on trial {trial}"
        );
        let aot_native_exec = vm_aot.jit_native_exec_count();
        aot_prepared_total = aot_prepared_total.saturating_add(aot_prepared);
        aot_native_exec_total = aot_native_exec_total.saturating_add(aot_native_exec);
        aot_prepare_times.push(aot_prepare_elapsed);
        aot_times.push(aot_elapsed);

        if diag_enabled && trial == 0 {
            let aot_snapshot = vm_aot.jit_snapshot();
            println!(
                "aes aot trial0 diagnostics: prepared={} traces={} attempts={} native_exec={}",
                aot_prepared,
                aot_snapshot.traces.len(),
                aot_snapshot.attempts.len(),
                aot_native_exec
            );
            if std::env::var_os("PDVM_PERF_AES_JIT_DUMP").is_some() {
                println!("aes aot trial0 dump:\n{}", vm_aot.dump_jit_info());
            }
        }
    }

    let interpreter_median = median_duration(&mut interpreter_times);
    let jit_median = median_duration(&mut jit_times);
    let aot_prepare_median = median_duration(&mut aot_prepare_times);
    let aot_median = median_duration(&mut aot_times);
    let jit_speedup =
        interpreter_median.as_secs_f64() / jit_median.as_secs_f64().max(f64::MIN_POSITIVE);
    let aot_speedup =
        interpreter_median.as_secs_f64() / aot_median.as_secs_f64().max(f64::MIN_POSITIVE);
    println!(
        "aes-128-cbc rss latency median: interpreter={}us jit={}us aot_run={}us aot_prepare={}us jit_speedup={:.2}x aot_speedup={:.2}x",
        interpreter_median.as_micros(),
        jit_median.as_micros(),
        aot_median.as_micros(),
        aot_prepare_median.as_micros(),
        jit_speedup,
        aot_speedup
    );
    if diag_enabled {
        println!(
            "aes jit aggregate diagnostics across {} trials: attempts={} traces={} nyi={} native_exec={}",
            TRIALS, jit_attempts_total, jit_traces_total, jit_nyi_total, jit_native_exec_total
        );
        println!(
            "aes aot aggregate diagnostics across {} trials: prepared={} native_exec={}",
            TRIALS, aot_prepared_total, aot_native_exec_total
        );
    }
}

#[test]
#[ignore = "performance characterization test; run manually"]
fn perf_cooperative_fuel_configuration_impacts_latency() {
    const INNER_LOOP_ITERS: i64 = 25_000;
    const OUTER_LOOPS: i64 = 4;
    const TRIALS: usize = 5;
    const FIXED_INTERVAL_FOR_FUEL_SWEEP: u32 = 1;
    const FIXED_FUEL_FOR_INTERVAL_SWEEP: u64 = 4_096;

    let source = format!(
        r#"
        let mut outer = 0;
        let mut i = 0;
        let mut sum = 0;
        while outer < {OUTER_LOOPS} {{
            i = 0;
            while i < {INNER_LOOP_ITERS} {{
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

    let compiled = compile_source(&source).expect("compile should succeed");
    let expected_per_outer = INNER_LOOP_ITERS * INNER_LOOP_ITERS + 3 * INNER_LOOP_ITERS;
    let expected = OUTER_LOOPS * expected_per_outer;

    let baseline = sample_fuel_perf_median(
        &compiled.program,
        compiled.locals,
        expected,
        None,
        1,
        TRIALS,
    );
    println!(
        "cooperative fuel baseline: fuel=disabled interval=n/a median_latency_us={} median_yields={} median_recharges={}",
        baseline.elapsed.as_micros(),
        baseline.yield_count,
        baseline.recharge_count
    );

    let fuel_values = [
        1_u64, 2, 4, 8, 16, 32, 64, 128, 256, 512, 1_024, 2_048, 4_096, 8_192,
    ];
    println!(
        "cooperative fuel sweep: fixed_check_interval={} (fuel starts at 1)",
        FIXED_INTERVAL_FOR_FUEL_SWEEP
    );
    for fuel in fuel_values {
        let median = sample_fuel_perf_median(
            &compiled.program,
            compiled.locals,
            expected,
            Some(fuel),
            FIXED_INTERVAL_FOR_FUEL_SWEEP,
            TRIALS,
        );
        let slowdown = median.elapsed.as_secs_f64() / baseline.elapsed.as_secs_f64().max(1e-12);
        println!(
            "  fuel={} interval={} median_latency_us={} median_yields={} median_recharges={} slowdown_vs_no_fuel={:.2}x",
            fuel,
            FIXED_INTERVAL_FOR_FUEL_SWEEP,
            median.elapsed.as_micros(),
            median.yield_count,
            median.recharge_count,
            slowdown
        );
    }

    let interval_values = [1_u32, 2, 4, 8, 16, 32, 64, 128, 256];
    println!(
        "cooperative interval sweep: fixed_fuel={} (check interval starts at 1)",
        FIXED_FUEL_FOR_INTERVAL_SWEEP
    );
    for interval in interval_values {
        let median = sample_fuel_perf_median(
            &compiled.program,
            compiled.locals,
            expected,
            Some(FIXED_FUEL_FOR_INTERVAL_SWEEP),
            interval,
            TRIALS,
        );
        let slowdown = median.elapsed.as_secs_f64() / baseline.elapsed.as_secs_f64().max(1e-12);
        println!(
            "  fuel={} interval={} median_latency_us={} median_yields={} median_recharges={} slowdown_vs_no_fuel={:.2}x",
            FIXED_FUEL_FOR_INTERVAL_SWEEP,
            interval,
            median.elapsed.as_micros(),
            median.yield_count,
            median.recharge_count,
            slowdown
        );
    }
}

fn native_jit_supported() -> bool {
    (cfg!(target_arch = "x86_64")
        && (cfg!(target_os = "windows") || (cfg!(unix) && !cfg!(target_os = "macos"))))
        || (cfg!(target_arch = "aarch64")
            && (cfg!(target_os = "linux") || cfg!(target_os = "macos")))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PerfExecMode {
    Interpreter,
    Jit,
    Aot,
}

fn run_sum_loop_with_mode(
    program: &Program,
    local_count: usize,
    mode: PerfExecMode,
    expected: i64,
) -> PerfRun {
    let mut vm = Vm::new(program.clone().with_local_count(local_count));
    let enable_jit = mode != PerfExecMode::Interpreter;
    vm.set_jit_config(JitConfig {
        enabled: enable_jit,
        hot_loop_threshold: 1,
        max_trace_len: 1_024,
    });

    if mode == PerfExecMode::Aot {
        vm.prepare_aot().expect("AOT precompile should succeed");
    }
    let started = Instant::now();
    let status = vm.run().expect("vm should run");
    let elapsed = started.elapsed();

    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(expected)]);

    if enable_jit {
        assert!(
            vm.jit_native_trace_count() > 0,
            "expected native trace count > 0"
        );
        assert!(
            vm.jit_native_exec_count() > 0,
            "expected native exec count > 0"
        );
    }

    PerfRun {
        elapsed,
        stack: vm.stack().to_vec(),
    }
}

struct PerfRun {
    elapsed: std::time::Duration,
    stack: Vec<Value>,
}

fn median_duration(samples: &mut [std::time::Duration]) -> std::time::Duration {
    samples.sort_unstable();
    samples[samples.len() / 2]
}

#[derive(Clone, Copy, Debug)]
struct FuelPerfRun {
    elapsed: std::time::Duration,
    yield_count: u64,
    recharge_count: u64,
}

#[derive(Clone, Copy, Debug)]
struct FuelPerfMedian {
    elapsed: std::time::Duration,
    yield_count: u64,
    recharge_count: u64,
}

fn sample_fuel_perf_median(
    program: &Program,
    local_count: usize,
    expected: i64,
    fuel_per_yield: Option<u64>,
    fuel_check_interval: u32,
    trials: usize,
) -> FuelPerfMedian {
    let mut elapsed_samples = Vec::with_capacity(trials);
    let mut yield_samples = Vec::with_capacity(trials);
    let mut recharge_samples = Vec::with_capacity(trials);

    for _ in 0..trials {
        let run = run_sum_loop_with_cooperative_fuel(
            program,
            local_count,
            expected,
            fuel_per_yield,
            fuel_check_interval,
        );
        elapsed_samples.push(run.elapsed);
        yield_samples.push(run.yield_count);
        recharge_samples.push(run.recharge_count);
    }

    FuelPerfMedian {
        elapsed: median_duration(&mut elapsed_samples),
        yield_count: median_u64(&mut yield_samples),
        recharge_count: median_u64(&mut recharge_samples),
    }
}

fn run_sum_loop_with_cooperative_fuel(
    program: &Program,
    local_count: usize,
    expected: i64,
    fuel_per_yield: Option<u64>,
    fuel_check_interval: u32,
) -> FuelPerfRun {
    let mut vm = Vm::new(program.clone().with_local_count(local_count));
    vm.set_jit_config(JitConfig {
        enabled: false,
        hot_loop_threshold: 1,
        max_trace_len: 1_024,
    });
    if let Some(fuel) = fuel_per_yield {
        vm.set_fuel_check_interval(fuel_check_interval)
            .expect("fuel check interval should be valid");
        vm.set_fuel(fuel);
    }

    let started = Instant::now();
    let mut yield_count = 0_u64;
    let mut recharge_count = 0_u64;
    // Very low fuel settings (for example fuel=1, interval=1) can require
    // several million cooperative yields while still making forward progress.
    let max_yields = 20_000_000_u64;

    loop {
        let status = vm.run().expect("vm should run");
        match status {
            VmStatus::Halted => break,
            VmStatus::Yielded => {
                yield_count = yield_count.saturating_add(1);
                if yield_count > max_yields {
                    panic!(
                        "fuel configuration appears to make no forward progress: fuel_per_yield={fuel_per_yield:?}, fuel_check_interval={fuel_check_interval}"
                    );
                }
                if let Some(fuel) = fuel_per_yield
                    && vm.get_fuel() == Some(0)
                {
                    vm.recharge_fuel(fuel)
                        .expect("fuel recharge should succeed");
                    recharge_count = recharge_count.saturating_add(1);
                }
            }
            VmStatus::Waiting(op_id) => {
                panic!("unexpected waiting host op in perf loop: op_id={op_id}");
            }
        }
    }

    assert_eq!(vm.stack(), &[Value::Int(expected)]);

    FuelPerfRun {
        elapsed: started.elapsed(),
        yield_count,
        recharge_count,
    }
}

fn median_u64(samples: &mut [u64]) -> u64 {
    samples.sort_unstable();
    samples[samples.len() / 2]
}

fn build_compiler_stress_source(line_count: usize) -> String {
    let mut source = String::from(
        r#"
        let i = 0;
        let sum = 0;
    "#,
    );
    for _ in 0..line_count {
        source.push_str("sum = sum + 1;\n");
    }
    source.push_str("sum;\n");
    source
}

fn build_host_import_stress_source(import_count: usize) -> String {
    let mut source = String::new();
    for index in 0..import_count {
        source.push_str(&format!("fn host_{index}(x);\n"));
    }
    source.push_str("let v = 1;\n");
    source.push_str("v;\n");
    source
}

fn print_rss_delta(label: &str, before: Option<u64>, rss_after: Option<u64>) {
    match (before, rss_after) {
        (Some(b), Some(a)) => {
            let delta = a as i128 - b as i128;
            println!("{label} rss: before={b}B after={a}B delta={delta}B");
        }
        _ => {
            println!("{label} rss: unsupported on this platform");
        }
    }
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
