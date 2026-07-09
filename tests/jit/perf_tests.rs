use std::hint::black_box;
use std::time::Instant;

use vm::{
    CallOutcome, HostArgsFunction, HostFunction, HostFunctionRegistry, HostStackFunction,
    JitConfig, OpCode, Program, Value, ValueType, Vm, VmStatus, compile_source,
    compile_source_file,
};

struct PerfNoopHost {
    _marker: u64,
}

impl HostFunction for PerfNoopHost {
    fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> Result<CallOutcome, vm::VmError> {
        Ok(CallOutcome::Return(vm::CallReturn::none()))
    }
}

fn perf_noop_host_static(_vm: &mut Vm, _args: &[Value]) -> Result<CallOutcome, vm::VmError> {
    Ok(CallOutcome::Return(vm::CallReturn::none()))
}

struct PerfIdentityHost;

impl HostFunction for PerfIdentityHost {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, vm::VmError> {
        perf_identity_host_result(args)
    }
}

struct PerfIdentityStackHost;

impl HostStackFunction for PerfIdentityStackHost {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, vm::VmError> {
        perf_identity_host_result(args)
    }
}

struct PerfIdentityArgsHost;

impl HostArgsFunction for PerfIdentityArgsHost {
    fn call(&mut self, args: &[Value]) -> Result<CallOutcome, vm::VmError> {
        perf_identity_host_result(args)
    }
}

fn perf_identity_host_static(_vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, vm::VmError> {
    perf_identity_host_result(args)
}

fn perf_identity_host_stack_static(
    _vm: &mut Vm,
    args: &[Value],
) -> Result<CallOutcome, vm::VmError> {
    perf_identity_host_result(args)
}

fn perf_identity_host_args_static(args: &[Value]) -> Result<CallOutcome, vm::VmError> {
    perf_identity_host_result(args)
}

fn perf_identity_host_result(args: &[Value]) -> Result<CallOutcome, vm::VmError> {
    let value = match args {
        [] => Value::Int(1),
        [Value::Int(value)] => Value::Int(*value),
        _ => return Err(vm::VmError::TypeMismatch("int")),
    };
    Ok(CallOutcome::Return(vm::CallReturn::one(value)))
}

fn force_local_types(program: Program, hints: &[(usize, ValueType)]) -> Program {
    let mut type_map = program.type_map.clone().unwrap_or_default();
    if type_map.local_types.len() < program.local_count {
        type_map
            .local_types
            .resize(program.local_count, ValueType::Unknown);
    }
    for (slot, ty) in hints {
        type_map.local_types[*slot] = *ty;
    }
    program.with_type_map(type_map)
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
fn perf_host_call_steady_state_latency() {
    const CALLS: i64 = 200_000;
    const TRIALS: usize = 5;

    let expected_zero_arg = CALLS;
    let expected_one_arg = CALLS * (CALLS - 1) / 2;

    benchmark_source_latency_case(
        "host legacy dynamic 0arg",
        &build_host_call_latency_source("perf_host0", 0, CALLS),
        &[Value::Int(expected_zero_arg)],
        CALLS,
        TRIALS,
        |vm| {
            vm.bind_function("perf_host0", Box::new(PerfIdentityHost));
        },
    );
    benchmark_source_latency_case(
        "host legacy static 0arg",
        &build_host_call_latency_source("perf_host0", 0, CALLS),
        &[Value::Int(expected_zero_arg)],
        CALLS,
        TRIALS,
        |vm| {
            vm.bind_static_function("perf_host0", perf_identity_host_static);
        },
    );
    benchmark_source_latency_case(
        "host args dynamic 0arg",
        &build_host_call_latency_source("perf_host0", 0, CALLS),
        &[Value::Int(expected_zero_arg)],
        CALLS,
        TRIALS,
        |vm| {
            vm.bind_args_function("perf_host0", Box::new(PerfIdentityArgsHost));
        },
    );
    benchmark_source_latency_case(
        "host args static 0arg",
        &build_host_call_latency_source("perf_host0", 0, CALLS),
        &[Value::Int(expected_zero_arg)],
        CALLS,
        TRIALS,
        |vm| {
            vm.bind_static_args_function("perf_host0", perf_identity_host_args_static);
        },
    );
    benchmark_source_latency_case(
        "host stack dynamic 0arg",
        &build_host_call_latency_source("perf_host0", 0, CALLS),
        &[Value::Int(expected_zero_arg)],
        CALLS,
        TRIALS,
        |vm| {
            vm.bind_stack_function("perf_host0", Box::new(PerfIdentityStackHost));
        },
    );
    benchmark_source_latency_case(
        "host stack static 0arg",
        &build_host_call_latency_source("perf_host0", 0, CALLS),
        &[Value::Int(expected_zero_arg)],
        CALLS,
        TRIALS,
        |vm| {
            vm.bind_static_stack_function("perf_host0", perf_identity_host_stack_static);
        },
    );

    benchmark_source_latency_case(
        "host legacy dynamic 1arg",
        &build_host_call_latency_source("perf_host1", 1, CALLS),
        &[Value::Int(expected_one_arg)],
        CALLS,
        TRIALS,
        |vm| {
            vm.bind_function("perf_host1", Box::new(PerfIdentityHost));
        },
    );
    benchmark_source_latency_case(
        "host legacy static 1arg",
        &build_host_call_latency_source("perf_host1", 1, CALLS),
        &[Value::Int(expected_one_arg)],
        CALLS,
        TRIALS,
        |vm| {
            vm.bind_static_function("perf_host1", perf_identity_host_static);
        },
    );
    benchmark_source_latency_case(
        "host args dynamic 1arg",
        &build_host_call_latency_source("perf_host1", 1, CALLS),
        &[Value::Int(expected_one_arg)],
        CALLS,
        TRIALS,
        |vm| {
            vm.bind_args_function("perf_host1", Box::new(PerfIdentityArgsHost));
        },
    );
    benchmark_source_latency_case(
        "host args static 1arg",
        &build_host_call_latency_source("perf_host1", 1, CALLS),
        &[Value::Int(expected_one_arg)],
        CALLS,
        TRIALS,
        |vm| {
            vm.bind_static_args_function("perf_host1", perf_identity_host_args_static);
        },
    );
    benchmark_source_latency_case(
        "host stack dynamic 1arg",
        &build_host_call_latency_source("perf_host1", 1, CALLS),
        &[Value::Int(expected_one_arg)],
        CALLS,
        TRIALS,
        |vm| {
            vm.bind_stack_function("perf_host1", Box::new(PerfIdentityStackHost));
        },
    );
    benchmark_source_latency_case(
        "host stack static 1arg",
        &build_host_call_latency_source("perf_host1", 1, CALLS),
        &[Value::Int(expected_one_arg)],
        CALLS,
        TRIALS,
        |vm| {
            vm.bind_static_stack_function("perf_host1", perf_identity_host_stack_static);
        },
    );
}

#[test]
#[ignore = "performance characterization test; run manually"]
fn perf_proc_macro_builtin_heap_arg_latency() {
    const CALLS: i64 = 50_000;
    const TRIALS: usize = 5;

    benchmark_source_latency_case(
        "proc-macro bytes::from_array_u8",
        &build_proc_macro_bytes_from_array_u8_source(CALLS),
        &[Value::Int(CALLS * 16)],
        CALLS,
        TRIALS,
        |_| {},
    );

    benchmark_source_latency_case(
        "proc-macro bytes::to_hex control",
        &build_proc_macro_bytes_to_hex_control_source(CALLS),
        &[Value::Int(CALLS * 32)],
        CALLS,
        TRIALS,
        |_| {},
    );

    benchmark_source_latency_case(
        "proc-macro format-template via print formatting",
        &build_proc_macro_format_template_source(CALLS),
        &[Value::Int(expected_print_format_total(CALLS))],
        CALLS,
        TRIALS,
        |vm| {
            vm.set_runtime_print_sink(|_| {});
        },
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
#[ignore = "performance/diagnostics harness; run manually"]
fn perf_jit_diagnostics_capture_exit_and_call_boundary_counters() {
    if !native_jit_supported() {
        println!("skipping jit diagnostics harness on unsupported native JIT target");
        return;
    }

    let numeric_source = r#"
        let mut i = 1;
        let mut sum = 0;
        while i < 64 {
            let is_evenish = ((i % 2) == 0) && true;
            let is_small = (i < 3) || is_evenish;
            if is_small {
                sum = sum + 1;
            } else {
                sum = sum + 2;
            }
            i = i + 1;
        }
        sum;
    "#;
    let numeric_compiled = compile_source(numeric_source).expect("numeric diagnostics compile");
    let mut numeric_vm = Vm::new(numeric_compiled.program);
    numeric_vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 1_024,
    });
    let numeric_status = numeric_vm.run().expect("numeric diagnostics vm should run");
    assert_eq!(numeric_status, VmStatus::Halted);
    let numeric_metrics = numeric_vm.jit_snapshot().metrics;
    println!(
        "jit numeric diagnostics: boxed_load_sites={} boxed_store_sites={} trace_exits={} guard_like_exits={} native_loop_backs={} native_execs={}",
        numeric_metrics.boxed_load_site_count,
        numeric_metrics.boxed_store_site_count,
        numeric_metrics.trace_exit_count,
        numeric_metrics.guard_exit_count(),
        numeric_metrics.native_loop_back_count,
        numeric_metrics.native_trace_exec_count
    );
    assert!(numeric_metrics.boxed_load_site_count > 0);
    assert!(numeric_metrics.boxed_store_site_count > 0);
    assert!(numeric_metrics.trace_exit_count > 0);
    assert!(numeric_metrics.native_loop_back_count > 0);

    let call_source = r#"
        fn print(x);
        let mut i = 0;
        while i < 32 {
            print(i);
            i = i + 1;
        }
        i;
    "#;
    let call_compiled = compile_source(call_source).expect("call diagnostics compile");
    let mut call_vm = Vm::new(call_compiled.program);
    call_vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 1_024,
    });
    for func in &call_compiled.functions {
        match func.name.as_str() {
            "print" => call_vm.register_function(Box::new(PerfNoopHost { _marker: 0 })),
            _ => panic!("unexpected function {}", func.name),
        };
    }
    let call_status = call_vm.run().expect("call diagnostics vm should run");
    assert_eq!(call_status, VmStatus::Halted);
    let call_snapshot = call_vm.jit_snapshot();
    let call_metrics = call_snapshot.metrics;
    println!(
        "jit call-boundary diagnostics: fallbacks={} native_execs={} traces={} attempts={} trace_exits={}",
        call_metrics.helper_fallback_count,
        call_metrics.native_trace_exec_count,
        call_snapshot.traces.len(),
        call_snapshot.attempts.len(),
        call_metrics.trace_exit_count
    );
    assert!(call_snapshot.traces.iter().any(|trace| trace.has_call));
    assert!(call_metrics.trace_exit_count > 0);
    assert!(call_metrics.native_trace_exec_count > 0);
    assert_eq!(call_metrics.helper_fallback_count, 0);
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
    assert_eq!(warmup_interpreter.stack, expected_stack);
    assert_eq!(warmup_jit.stack, warmup_interpreter.stack);

    let mut interpreter_times = Vec::with_capacity(TRIALS);
    let mut jit_times = Vec::with_capacity(TRIALS);
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

        assert_eq!(
            interpreter_run.stack, expected_stack,
            "interpreter result mismatch on trial {trial}",
        );
        assert_eq!(
            jit_run.stack, interpreter_run.stack,
            "native JIT result mismatch on trial {trial}",
        );

        interpreter_times.push(interpreter_run.elapsed);
        jit_times.push(jit_run.elapsed);
    }

    let interpreter_median = median_duration(&mut interpreter_times);
    let jit_median = median_duration(&mut jit_times);
    let jit_speedup = interpreter_median.as_secs_f64() / jit_median.as_secs_f64();

    println!(
        "tight-loop latency median: interpreter={}ms jit={}ms jit_speedup={:.2}x",
        interpreter_median.as_millis(),
        jit_median.as_millis(),
        jit_speedup,
    );

    assert!(
        jit_median < interpreter_median,
        "expected JIT median latency to be lower (interpreter={:?}, jit={:?})",
        interpreter_median,
        jit_median
    );
}

#[test]
#[ignore = "performance characterization test; run manually"]
fn perf_jit_native_characterizes_array_builtin_loop_latency() {
    if !native_jit_supported() {
        println!("skipping array builtin perf on unsupported native JIT target");
        return;
    }

    const OUTER_LOOPS: i64 = 8_192;
    const TRIALS: usize = 7;
    let elements = [3_i64, 5, 7, 11, 13, 17, 19, 23, 29, 31, 37, 41];
    let source = build_array_builtin_perf_source(&elements, OUTER_LOOPS);
    let compiled = compile_source(&source).expect("array builtin perf compile should succeed");
    let program = force_local_types(
        compiled.program,
        &[
            (0, ValueType::Array),
            (1, ValueType::Int),
            (2, ValueType::Int),
            (3, ValueType::Int),
        ],
    );
    let expected = OUTER_LOOPS * elements.iter().sum::<i64>();
    let expected_stack = vec![Value::Int(expected)];

    let mut interpreter_vm = Vm::new(program.clone());
    interpreter_vm.set_jit_config(JitConfig {
        enabled: false,
        hot_loop_threshold: 1,
        max_trace_len: 2_048,
    });
    let interpreter_warmup = warm_reusable_vm_once(&mut interpreter_vm, &expected_stack);
    let mut interpreter_times =
        sample_reused_vm_latencies(&mut interpreter_vm, &expected_stack, TRIALS);

    let mut jit_vm = Vm::new(program);
    jit_vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 2_048,
    });
    let jit_warmup = warm_reusable_vm_once(&mut jit_vm, &expected_stack);
    assert!(
        jit_vm.jit_native_trace_count() > 0,
        "expected array builtin perf warmup to install native traces"
    );
    assert!(
        jit_vm.jit_native_exec_count() > 0,
        "expected array builtin perf warmup to execute native traces"
    );
    let mut jit_times = sample_reused_vm_latencies(&mut jit_vm, &expected_stack, TRIALS);
    let snapshot = jit_vm.jit_snapshot();
    assert!(
        snapshot
            .traces
            .iter()
            .any(|trace| trace.op_names().iter().any(|op| op == "array_len")),
        "expected array perf trace to include array_len, dump:\n{}",
        jit_vm.dump_jit_info()
    );
    assert!(
        snapshot
            .traces
            .iter()
            .any(|trace| trace.op_names().iter().any(|op| op == "array_get")),
        "expected array perf trace to include array_get, dump:\n{}",
        jit_vm.dump_jit_info()
    );
    assert!(
        snapshot
            .traces
            .iter()
            .any(|trace| trace.op_names().iter().any(|op| op == "array_has")),
        "expected array perf trace to include array_has, dump:\n{}",
        jit_vm.dump_jit_info()
    );

    let interpreter_median = median_duration(&mut interpreter_times);
    let jit_median = median_duration(&mut jit_times);
    let jit_speedup =
        interpreter_median.as_secs_f64() / jit_median.as_secs_f64().max(f64::MIN_POSITIVE);
    println!(
        "array builtin warmed latency: interpreter={}us jit={}us jit_speedup={:.2}x interpreter_warmup_us={} jit_warmup_us={} jit_traces={} jit_native_execs={}",
        interpreter_median.as_micros(),
        jit_median.as_micros(),
        jit_speedup,
        interpreter_warmup.as_micros(),
        jit_warmup.as_micros(),
        jit_vm.jit_native_trace_count(),
        jit_vm.jit_native_exec_count()
    );
}

#[test]
#[ignore = "performance characterization test; run manually"]
fn perf_jit_native_characterizes_map_builtin_loop_latency() {
    if !native_jit_supported() {
        println!("skipping map builtin perf on unsupported native JIT target");
        return;
    }

    const OUTER_LOOPS: i64 = 32_768;
    const TRIALS: usize = 7;
    let entries = [("a", 7_i64), ("b", 11), ("c", 13), ("d", 17)];
    let source = build_map_builtin_perf_source(&entries, OUTER_LOOPS);
    let compiled = compile_source(&source).expect("map builtin perf compile should succeed");
    let program = force_local_types(
        compiled.program,
        &[
            (0, ValueType::Map),
            (1, ValueType::Int),
            (2, ValueType::Int),
        ],
    );
    let per_iter = entries.iter().map(|(_, value)| *value).sum::<i64>() + entries.len() as i64;
    let expected = OUTER_LOOPS * per_iter;
    let expected_stack = vec![Value::Int(expected)];

    let mut interpreter_vm = Vm::new(program.clone());
    interpreter_vm.set_jit_config(JitConfig {
        enabled: false,
        hot_loop_threshold: 1,
        max_trace_len: 2_048,
    });
    let interpreter_warmup = warm_reusable_vm_once(&mut interpreter_vm, &expected_stack);
    let mut interpreter_times =
        sample_reused_vm_latencies(&mut interpreter_vm, &expected_stack, TRIALS);

    let mut jit_vm = Vm::new(program);
    jit_vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 2_048,
    });
    let jit_warmup = warm_reusable_vm_once(&mut jit_vm, &expected_stack);
    assert!(
        jit_vm.jit_native_trace_count() > 0,
        "expected map builtin perf warmup to install native traces"
    );
    assert!(
        jit_vm.jit_native_exec_count() > 0,
        "expected map builtin perf warmup to execute native traces"
    );
    let mut jit_times = sample_reused_vm_latencies(&mut jit_vm, &expected_stack, TRIALS);
    let snapshot = jit_vm.jit_snapshot();
    assert!(
        snapshot
            .traces
            .iter()
            .any(|trace| trace.op_names().iter().any(|op| op == "map_len")),
        "expected map perf trace to include map_len, dump:\n{}",
        jit_vm.dump_jit_info()
    );
    assert!(
        snapshot
            .traces
            .iter()
            .any(|trace| trace.op_names().iter().any(|op| op == "map_get")),
        "expected map perf trace to include map_get, dump:\n{}",
        jit_vm.dump_jit_info()
    );
    assert!(
        snapshot
            .traces
            .iter()
            .any(|trace| trace.op_names().iter().any(|op| op == "map_has")),
        "expected map perf trace to include map_has, dump:\n{}",
        jit_vm.dump_jit_info()
    );

    let interpreter_median = median_duration(&mut interpreter_times);
    let jit_median = median_duration(&mut jit_times);
    let jit_speedup =
        interpreter_median.as_secs_f64() / jit_median.as_secs_f64().max(f64::MIN_POSITIVE);
    println!(
        "map builtin warmed latency: interpreter={}us jit={}us jit_speedup={:.2}x interpreter_warmup_us={} jit_warmup_us={} jit_traces={} jit_native_execs={}",
        interpreter_median.as_micros(),
        jit_median.as_micros(),
        jit_speedup,
        interpreter_warmup.as_micros(),
        jit_warmup.as_micros(),
        jit_vm.jit_native_trace_count(),
        jit_vm.jit_native_exec_count()
    );
}

#[test]
#[ignore = "manual run performance test; run explicitly"]
fn perf_manual_aes_128_cbc_rustscript_matches_in_interpreter_jit_and_aot() {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/aes_128_cbc_usage.rss");
    let compiled =
        compile_source_file(path.as_path()).expect("aes RustScript usage example should compile");

    let expected = vec![Value::string("7649abac8119b246cee98e9b12e9197d")];

    let full_benchmark = std::env::var_os("PDVM_RUN_AES_PERF").is_some();
    let trials = std::env::var("PDVM_PERF_AES_TRIALS")
        .ok()
        .and_then(|raw| raw.parse::<usize>().ok())
        .filter(|count| *count > 0)
        .unwrap_or(if full_benchmark { 7 } else { 1 });
    let (hot_loop_threshold, max_trace_len) = if full_benchmark {
        (1, 16_384)
    } else {
        (64, 512)
    };
    let diag_enabled = std::env::var_os("PDVM_PERF_AES_JIT_DIAG").is_some();
    println!(
        "aes perf mode: {} (trials={}, hot_loop_threshold={}, max_trace_len={}, aot=enabled)",
        if full_benchmark {
            "benchmark"
        } else {
            "bounded smoke"
        },
        trials,
        hot_loop_threshold,
        max_trace_len
    );
    let mut interpreter_times = Vec::with_capacity(trials);
    let mut jit_times = Vec::with_capacity(trials);
    let mut aot_times = Vec::with_capacity(trials);
    let mut jit_attempts_total = 0usize;
    let mut jit_traces_total = 0usize;
    let mut jit_nyi_total = 0usize;
    let mut jit_native_exec_total = 0u64;
    let mut aot_exec_total = 0u64;

    for trial in 0..trials {
        let mut vm_interpreter = Vm::new(compiled.program.clone());
        vm_interpreter.set_jit_config(JitConfig {
            enabled: false,
            hot_loop_threshold,
            max_trace_len,
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
            hot_loop_threshold,
            max_trace_len,
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
            let metrics = snapshot.metrics;
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
            let helper_like_ops = snapshot
                .traces
                .iter()
                .flat_map(|trace| trace.op_names().iter())
                .filter(|op| {
                    matches!(
                        op.as_str(),
                        "ldc" | "pop" | "dup" | "ldloc" | "stloc" | "call"
                    )
                })
                .count();
            let total_ops = snapshot
                .traces
                .iter()
                .map(|trace| trace.op_names().len())
                .sum::<usize>();
            println!(
                "aes jit trial0 diagnostics: attempts={} traces={} nyi={} native_exec={} helper_like_ops={} total_ops={} traces_with_calls={} traces_with_yielding_calls={} boxed_load_sites={} boxed_store_sites={} trace_exits={} guard_like_exits={} native_loop_backs={} fallbacks={}",
                trial_attempts,
                trial_traces,
                trial_nyi,
                trial_native_exec,
                helper_like_ops,
                total_ops,
                traces_with_calls,
                traces_with_yielding_calls,
                metrics.boxed_load_site_count,
                metrics.boxed_store_site_count,
                metrics.trace_exit_count,
                metrics.guard_exit_count(),
                metrics.native_loop_back_count,
                metrics.helper_fallback_count
            );
            if std::env::var_os("PDVM_PERF_AES_JIT_DUMP").is_some() {
                println!("aes jit trial0 dump:\n{}", vm_jit.dump_jit_info());
            }
        }
        jit_times.push(jit_elapsed);

        let mut vm_aot = Vm::new(compiled.program.clone());
        vm_aot.set_jit_config(JitConfig {
            enabled: false,
            hot_loop_threshold,
            max_trace_len,
        });
        vm_aot
            .compile_aot()
            .expect("aes aot compile should succeed");
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
        assert!(
            vm_aot.aot_exec_count() > 0,
            "expected aot execution count > 0, dump:\n{}",
            vm_aot.dump_aot_info()
        );
        aot_exec_total = aot_exec_total.saturating_add(vm_aot.aot_exec_count());
        if diag_enabled && trial == 0 && std::env::var_os("PDVM_PERF_AES_AOT_DUMP").is_some() {
            println!("aes aot trial0 dump:\n{}", vm_aot.dump_aot_info());
        }
        aot_times.push(aot_elapsed);
    }

    let interpreter_median = median_duration(&mut interpreter_times);
    let jit_median = median_duration(&mut jit_times);
    let aot_median = median_duration(&mut aot_times);
    let jit_speedup =
        interpreter_median.as_secs_f64() / jit_median.as_secs_f64().max(f64::MIN_POSITIVE);
    let aot_speedup =
        interpreter_median.as_secs_f64() / aot_median.as_secs_f64().max(f64::MIN_POSITIVE);
    println!(
        "aes-128-cbc rss latency median: interpreter={}us jit={}us aot={}us jit_speedup={:.2}x aot_speedup={:.2}x",
        interpreter_median.as_micros(),
        jit_median.as_micros(),
        aot_median.as_micros(),
        jit_speedup,
        aot_speedup
    );
    if diag_enabled {
        println!(
            "aes jit aggregate diagnostics across {} trials: attempts={} traces={} nyi={} native_exec={}",
            trials, jit_attempts_total, jit_traces_total, jit_nyi_total, jit_native_exec_total
        );
        println!(
            "aes aot aggregate diagnostics across {} trials: executions={}",
            trials, aot_exec_total
        );
    }
}

#[test]
#[ignore = "manual run performance test; run explicitly"]
fn perf_manual_ifft_math_matches_in_interpreter_jit_and_aot_without_warmup_or_compile_time() {
    if !native_jit_supported() {
        println!("skipping ifft perf on unsupported native JIT target");
        return;
    }

    let full_benchmark = std::env::var_os("PDVM_RUN_IFFT_PERF").is_some();
    let trials = std::env::var("PDVM_PERF_IFFT_TRIALS")
        .ok()
        .and_then(|raw| raw.parse::<usize>().ok())
        .filter(|count| *count > 0)
        .unwrap_or(if full_benchmark { 9 } else { 5 });
    let program_repeats = std::env::var("PDVM_PERF_IFFT_REPEATS")
        .ok()
        .and_then(|raw| raw.parse::<usize>().ok())
        .filter(|count| *count > 0)
        .unwrap_or(if full_benchmark { 4_096 } else { 512 });
    let hot_loop_threshold = 1;
    let max_trace_len = if full_benchmark { 16_384 } else { 4_096 };

    let source = build_ifft_perf_source(program_repeats);
    let source_compile_started = Instant::now();
    let compiled = compile_source(&source).expect("ifft perf source should compile");
    let source_compile_elapsed = source_compile_started.elapsed();
    let expected_iterations = i64::try_from(program_repeats).expect("program repeats should fit");
    let expected_stack = vec![Value::Int(expected_iterations)];

    println!(
        "ifft perf mode: {} (trials={}, program_repeats={}, hot_loop_threshold={}, max_trace_len={}, timed_section=reset+run only, source_compile_us={})",
        if full_benchmark {
            "benchmark"
        } else {
            "bounded smoke"
        },
        trials,
        program_repeats,
        hot_loop_threshold,
        max_trace_len,
        source_compile_elapsed.as_micros()
    );

    let mut vm_interpreter = Vm::new(compiled.program.clone());
    vm_interpreter.set_jit_config(JitConfig {
        enabled: false,
        hot_loop_threshold,
        max_trace_len,
    });
    let interpreter_warmup = warm_reusable_vm_once(&mut vm_interpreter, &expected_stack);
    let mut interpreter_times =
        sample_reused_vm_latencies(&mut vm_interpreter, &expected_stack, trials);

    let mut vm_jit = Vm::new(compiled.program.clone());
    vm_jit.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold,
        max_trace_len,
    });
    let jit_warmup = warm_reusable_vm_once(&mut vm_jit, &expected_stack);
    assert!(
        vm_jit.jit_native_trace_count() > 0,
        "expected JIT warmup to install native traces"
    );
    assert!(
        vm_jit.jit_native_exec_count() > 0,
        "expected JIT warmup to execute native traces"
    );
    let mut jit_times = sample_reused_vm_latencies(&mut vm_jit, &expected_stack, trials);
    let jit_trace_count = vm_jit.jit_native_trace_count();
    let jit_exec_count = vm_jit.jit_native_exec_count();

    let mut vm_aot = Vm::new(compiled.program);
    vm_aot.set_jit_config(JitConfig {
        enabled: false,
        hot_loop_threshold,
        max_trace_len,
    });
    let aot_compile_started = Instant::now();
    vm_aot
        .compile_aot()
        .expect("ifft perf aot compile should succeed");
    let aot_compile_elapsed = aot_compile_started.elapsed();
    let aot_warmup = warm_reusable_vm_once(&mut vm_aot, &expected_stack);
    assert!(
        vm_aot.aot_exec_count() > 0,
        "expected warmed aot vm to execute natively, dump:\n{}",
        vm_aot.dump_aot_info()
    );
    let mut aot_times = sample_reused_vm_latencies(&mut vm_aot, &expected_stack, trials);
    let aot_exec_count = vm_aot.aot_exec_count();

    let interpreter_median = median_duration(&mut interpreter_times);
    let jit_median = median_duration(&mut jit_times);
    let aot_median = median_duration(&mut aot_times);
    let jit_speedup =
        interpreter_median.as_secs_f64() / jit_median.as_secs_f64().max(f64::MIN_POSITIVE);
    let aot_speedup =
        interpreter_median.as_secs_f64() / aot_median.as_secs_f64().max(f64::MIN_POSITIVE);

    println!(
        "ifft_math warmed latency median: interpreter={}us jit={}us aot={}us jit_speedup={:.2}x aot_speedup={:.2}x",
        interpreter_median.as_micros(),
        jit_median.as_micros(),
        aot_median.as_micros(),
        jit_speedup,
        aot_speedup
    );
    println!(
        "ifft_math setup diagnostics: interpreter_warmup_us={} jit_warmup_us={} aot_compile_us={} aot_warmup_us={} jit_traces={} jit_native_execs={} aot_execs={}",
        interpreter_warmup.as_micros(),
        jit_warmup.as_micros(),
        aot_compile_elapsed.as_micros(),
        aot_warmup.as_micros(),
        jit_trace_count,
        jit_exec_count,
        aot_exec_count
    );
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
}

fn run_sum_loop_with_mode(
    program: &Program,
    local_count: usize,
    mode: PerfExecMode,
    expected: i64,
) -> PerfRun {
    let mut vm = Vm::new(program.clone().with_local_count(local_count));
    let enable_jit = mode == PerfExecMode::Jit;
    vm.set_jit_config(JitConfig {
        enabled: enable_jit,
        hot_loop_threshold: 1,
        max_trace_len: 1_024,
    });

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

fn build_ifft_perf_source(program_repeats: usize) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/ifft_math.rss");
    let source = std::fs::read_to_string(&path).expect("ifft math example should be readable");
    let (prefix, _) = source
        .split_once("// These bins are the forward DFT of [1, 2, 3, 4].")
        .expect("ifft math example should contain the benchmark split marker");

    format!(
        r#"{}

let spectrum_re = [10.0,-2.0,-2.0,-2.0];
let spectrum_im = [0.0, 2.0, 0.0,-2.0];

let mut iteration = 0;
let mut sample_total = 0.0;
let mut imag_total = 0.0;
while iteration < {} {{
    let signal = ifft(spectrum_re, spectrum_im);
    let signal_re = signal.re;
    let signal_im = signal.im;

    sample_total = sample_total + signal_re[0] + signal_re[1] + signal_re[2] + signal_re[3];
    imag_total = imag_total
        + math::abs(signal_im[0])
        + math::abs(signal_im[1])
        + math::abs(signal_im[2])
        + math::abs(signal_im[3]);
    iteration = iteration + 1;
}}

assert(nearly_equal(sample_total, 10.0 * {}.0));
assert(nearly_zero(imag_total));

iteration;
"#,
        prefix.trim_end(),
        program_repeats,
        program_repeats
    )
}

fn build_host_call_latency_source(name: &str, arity: u8, calls: i64) -> String {
    match arity {
        0 => format!(
            r#"
            fn {name}();
            let mut i = 0;
            let mut sum = 0;
            while i < {calls} {{
                sum = sum + {name}();
                i = i + 1;
            }}
            sum;
        "#
        ),
        1 => format!(
            r#"
            fn {name}(x);
            let mut i = 0;
            let mut sum = 0;
            while i < {calls} {{
                sum = sum + {name}(i);
                i = i + 1;
            }}
            sum;
        "#
        ),
        other => panic!("unsupported host latency arity {other}"),
    }
}

fn build_proc_macro_bytes_from_array_u8_source(calls: i64) -> String {
    format!(
        r#"
        use bytes;
        let values = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
        let mut i = 0;
        let mut sum = 0;
        while i < {calls} {{
            let payload = bytes::from_array_u8(values);
            sum = sum + payload.length;
            i = i + 1;
        }}
        sum;
    "#
    )
}

fn build_proc_macro_bytes_to_hex_control_source(calls: i64) -> String {
    format!(
        r#"
        use bytes;
        let payload = bytes::from_hex("00112233445566778899aabbccddeeff");
        let mut i = 0;
        let mut sum = 0;
        while i < {calls} {{
            let text = bytes::to_hex(payload);
            sum = sum + text.length;
            i = i + 1;
        }}
        sum;
    "#
    )
}

fn build_proc_macro_format_template_source(calls: i64) -> String {
    format!(
        r#"
        let mut i = 0;
        let mut sum = 0;
        while i < {calls} {{
            let rendered = print("item={{}} next={{}}", i, i + 1);
            sum = sum + rendered.length;
            i = i + 1;
        }}
        sum;
    "#
    )
}

fn expected_print_format_total(calls: i64) -> i64 {
    let mut total = 0_i64;
    for i in 0..calls {
        total += format!("item={} next={}", i, i + 1).len() as i64;
    }
    total
}

fn build_array_builtin_perf_source(elements: &[i64], outer_loops: i64) -> String {
    let values = elements
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        r#"
        let arr = [{values}];
        let mut outer = 0;
        let mut sum = 0;
        while outer < {outer_loops} {{
            let mut i = 0;
            while i < arr.length {{
                if arr.has(i) {{
                    sum = sum + arr[i];
                }}
                i = i + 1;
            }}
            outer = outer + 1;
        }}
        sum;
    "#
    )
}

fn build_map_builtin_perf_source(entries: &[(&str, i64)], outer_loops: i64) -> String {
    let map_literal = entries
        .iter()
        .map(|(key, value)| format!("\"{key}\": {value}"))
        .collect::<Vec<_>>()
        .join(", ");
    let body = entries
        .iter()
        .map(|(key, _)| {
            format!(
                "            if data.has(\"{key}\") {{\n                sum = sum + data[\"{key}\"];\n            }}\n"
            )
        })
        .collect::<String>();

    format!(
        r#"
        let data = {{{map_literal}}};
        let mut outer = 0;
        let mut sum = 0;
        while outer < {outer_loops} {{
            let len = data.length;
{body}            sum = sum + len;
            outer = outer + 1;
        }}
        sum;
    "#
    )
}

fn warm_reusable_vm_once(vm: &mut Vm, expected_stack: &[Value]) -> std::time::Duration {
    let elapsed = run_vm_once(vm, expected_stack);
    vm.reset_for_reuse();
    elapsed
}

fn benchmark_source_latency_case<F>(
    label: &str,
    source: &str,
    expected_stack: &[Value],
    calls: i64,
    trials: usize,
    configure_vm: F,
) where
    F: Fn(&mut Vm),
{
    let compiled = compile_source(source).expect("benchmark source should compile");
    let mut vm = Vm::new(compiled.program);
    vm.set_jit_config(JitConfig {
        enabled: false,
        hot_loop_threshold: 1,
        max_trace_len: 1_024,
    });
    configure_vm(&mut vm);

    let warmup = warm_reusable_vm_once(&mut vm, expected_stack);
    let mut samples = sample_reused_vm_latencies(&mut vm, expected_stack, trials);
    let median = median_duration(&mut samples);
    println!(
        "{label}: warmup_us={} median_us={} per_call_ns={}",
        warmup.as_micros(),
        median.as_micros(),
        median.as_nanos() / calls.max(1) as u128
    );
}

fn sample_reused_vm_latencies(
    vm: &mut Vm,
    expected_stack: &[Value],
    trials: usize,
) -> Vec<std::time::Duration> {
    let mut samples = Vec::with_capacity(trials);
    for _ in 0..trials {
        let elapsed = run_vm_once(vm, expected_stack);
        samples.push(elapsed);
        vm.reset_for_reuse();
    }
    samples
}

fn run_vm_once(vm: &mut Vm, expected_stack: &[Value]) -> std::time::Duration {
    let started = Instant::now();
    let status = vm.run().expect("vm should run");
    let elapsed = started.elapsed();
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), expected_stack);
    elapsed
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
        let mut sum = 0;
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
