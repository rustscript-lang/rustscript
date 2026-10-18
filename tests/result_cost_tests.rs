#![cfg(all(feature = "runtime", feature = "cranelift-jit"))]
//! Finite, opt-in same-binary Result/record/direct cost characterization.
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Instant,
};
use vm::{CallOutcome, CallReturn, HostFunction, Value, Vm, VmResult, VmStatus};
thread_local! {
    static ACTIVE: Cell<bool> = const { Cell::new(false) };
    static ALLOCS: Cell<u64> = const { Cell::new(0) };
    static BYTES: Cell<u64> = const { Cell::new(0) };
    static LIVE: Cell<i64> = const { Cell::new(0) };
}
struct Counting;
#[global_allocator]
static ALLOCATOR: Counting = Counting;
fn account(size: usize, allocating: bool) {
    let _ = LIVE.try_with(|v| {
        v.set(
            v.get()
                + if allocating {
                    size as i64
                } else {
                    -(size as i64)
                },
        )
    });
    if allocating && ACTIVE.try_with(Cell::get).unwrap_or(false) {
        ALLOCS.with(|v| v.set(v.get() + 1));
        BYTES.with(|v| v.set(v.get() + size as u64));
    }
}
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            account(layout.size(), true);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        account(layout.size(), false);
        unsafe {
            System.dealloc(p, layout);
        }
    }
    unsafe fn realloc(&self, p: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, layout, size) };
        if !q.is_null() {
            account(layout.size(), false);
            account(size, true);
        }
        q
    }
}
struct Input(i64, Arc<AtomicUsize>);
impl HostFunction for Input {
    fn call(&mut self, _: &mut Vm, _: &[Value]) -> VmResult<CallOutcome> {
        self.1.fetch_add(1, Ordering::Relaxed);
        Ok(CallOutcome::Return(CallReturn::one(Value::Int(self.0))))
    }
}
fn code_metrics(program: &vm::Program) -> (usize, usize, usize) {
    let mut ip = 0;
    let (mut instructions, mut branches, mut map_new_calls) = (0, 0, 0);
    while ip < program.code.len() {
        let opcode = vm::OpCode::try_from(program.code[ip]).unwrap();
        instructions += 1;
        if matches!(opcode, vm::OpCode::Br | vm::OpCode::Brfalse) {
            branches += 1;
        }
        if opcode == vm::OpCode::Call
            && u16::from_le_bytes([program.code[ip + 1], program.code[ip + 2]])
                == vm::BuiltinFunction::MapNew.call_index()
        {
            map_new_calls += 1;
        }
        ip += 1 + opcode.operand_len();
    }
    (instructions, branches, map_new_calls)
}

fn source(carrier: &str, workload: &str) -> String {
    let prelude = "struct Pair { tag: bool, payload: int }\nfn integer(n: int) -> int { n }\n";
    let (ret, ok, err, take) = match carrier {
        "result" => (
            "Result<int, int>",
            "Result::Ok(n)",
            "Result::Err(n)",
            "match value { Result::Ok(n) => integer(n), Result::Err(e) => integer(e), }",
        ),
        "map" => (
            "Pair",
            "{ tag: true, payload: n }",
            "{ tag: false, payload: n }",
            "if value.tag => { integer(value.payload) } else => { integer(value.payload) }",
        ),
        "direct" => ("int", "n", "n", "integer(value)"),
        _ => unreachable!(),
    };
    if workload == "construct_match" || workload == "function_return" {
        let construct = format!("if flag => {{ {ok} }} else => {{ {err} }}");
        let (function, body) = if workload == "function_return" {
            (
                format!("fn choose(flag: bool, n: int) -> {ret} {{ {construct} }}"),
                "choose(flag, n)".to_string(),
            )
        } else {
            (String::new(), construct)
        };
        return format!(
            "{prelude}{function} let n = bench::input(); let flag = bench::fail() == 1000; let mut i = 0; let mut sum = 0; while i < 256 {{ let value: {ret} = {body}; let selected = {take}; sum = sum + integer(selected); i = i + 1; }} sum;"
        );
    }
    let function = match carrier {
        "result" => {
            r#"fn work(n: int, fail: int) -> Result<int, int> {
            let mut i = 0; let mut sum = 0;
            while i < 256 {
                let next: Result<int, int> = if i == fail => { Result::Err(sum) } else => { Result::Ok(n) };
                let selected = next?; sum = sum + integer(selected); i = i + 1;
            }
            Result::Ok(sum)
        }"#
        }
        "map" => {
            r#"fn work(n: int, fail: int) -> Pair {
            let mut i = 0; let mut sum = 0; let mut good = true;
            while i < 256 && good {
                let next: Pair = if i == fail => { { tag: false, payload: sum } } else => { { tag: true, payload: n } };
                if next.tag { sum = sum + integer(next.payload); i = i + 1; } else { good = false; }
            }
            let output: Pair = { tag: good, payload: sum };
            output
        }"#
        }
        "direct" => {
            r#"fn work(n: int, fail: int) -> int {
            let mut i = 0; let mut sum = 0;
            while i < 256 && i != fail { sum = sum + integer(n); i = i + 1; }
            sum
        }"#
        }
        _ => unreachable!(),
    };
    format!(
        "{prelude}{function} let value = work(bench::input(), bench::fail()); let observed = {take}; observed;"
    )
}
struct Case {
    machine: Vm,
    calls: Arc<AtomicUsize>,
    carrier: &'static str,
    workload: &'static str,
    backend: &'static str,
    expected: i64,
}
fn rss() -> String {
    std::fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .filter(|l| l.starts_with("VmRSS:") || l.starts_with("VmHWM:"))
        .collect::<Vec<_>>()
        .join("; ")
}
fn execute(case: &mut Case) {
    let before = case.calls.load(Ordering::Relaxed);
    assert_eq!(case.machine.run().unwrap(), VmStatus::Halted);
    assert_eq!(
        case.machine.stack().last(),
        Some(&Value::Int(case.expected))
    );
    assert_eq!(case.calls.load(Ordering::Relaxed) - before, 2);
    assert_eq!(case.machine.host_context().resource_count(), 0);
}
#[test]
#[ignore = "finite opt-in allocation/timing characterization; isolated harness"]
fn measured_result_map_direct_costs() {
    eprintln!(
        "R4_PERF_PROTOCOL warmup=3 rounds=4 order=forward/reverse backend=interpreter,trace,aot iterations=256 compile_and_reset_outside_timer profile=debug requested_bytes_thread_local=true rss={}",
        rss()
    );
    let mut builder = vm::HostApiBuilder::new();
    for name in ["bench::input", "bench::fail"] {
        builder.function(vm::HostFunctionSchema::with_return(
            name,
            vec![],
            vm::HostTypeSchema::Int,
        ));
    }
    let catalog = Arc::new(builder.build().unwrap());
    let mut cases = Vec::new();
    for backend in ["interpreter", "trace", "aot"] {
        for workload in ["construct_match", "function_return", "try_ok", "try_err"] {
            for carrier in ["direct", "map", "result"] {
                let text = source(carrier, workload);
                let start = Instant::now();
                let compiled = vm::compile_source_with_flavor_and_options(
                    &text,
                    vm::SourceFlavor::RustScript,
                    vm::CompileSourceFileOptions::new().with_host_api_catalog(catalog.clone()),
                )
                .unwrap();
                let compile_ns = start.elapsed().as_nanos();
                let bytecode_bytes = compiled.program.code.len();
                let locals = compiled.locals;
                let (instructions, branches, map_new_calls) = code_metrics(&compiled.program);
                if carrier == "direct" {
                    let baseline = vm::compile_source_with_flavor_and_options(
                        &text.replace("struct Pair { tag: bool, payload: int }\n", ""),
                        vm::SourceFlavor::RustScript,
                        vm::CompileSourceFileOptions::new().with_host_api_catalog(catalog.clone()),
                    )
                    .unwrap();
                    assert_eq!(compiled.locals, baseline.locals);
                    assert_eq!(compiled.program.code, baseline.program.code);
                    assert_eq!(
                        code_metrics(&compiled.program),
                        code_metrics(&baseline.program)
                    );
                    assert_eq!(
                        map_new_calls, 0,
                        "no hidden carrier construction in direct control"
                    );
                }
                let mut machine = Vm::new_with_jit_config(
                    compiled.program.with_local_count(locals),
                    vm::JitConfig {
                        enabled: backend == "trace",
                        hot_loop_threshold: 1,
                        max_trace_len: 512,
                    },
                );
                let calls = Arc::new(AtomicUsize::new(0));
                machine.bind_function("bench::input", Box::new(Input(7, calls.clone())));
                machine.bind_function(
                    "bench::fail",
                    Box::new(Input(
                        if workload == "try_err" { 63 } else { 1000 },
                        calls.clone(),
                    )),
                );
                machine.set_fuel(1_000_000);
                let start = Instant::now();
                if backend == "aot" {
                    machine.compile_aot().unwrap();
                }
                let native_compile_ns = start.elapsed().as_nanos();
                let mut case = Case {
                    machine,
                    calls,
                    carrier,
                    workload,
                    backend,
                    expected: if workload == "try_err" { 441 } else { 1792 },
                };
                for _ in 0..3 {
                    execute(&mut case);
                    case.machine.reset_for_reuse().unwrap();
                    case.machine.set_fuel(1_000_000);
                }
                let dump = if backend == "aot" {
                    case.machine.dump_aot_info()
                } else {
                    case.machine.dump_jit_info()
                };
                let sizes = dump
                    .lines()
                    .filter(|l| {
                        l.contains("code_bytes=")
                            || l.contains("native code bytes:")
                            || l.contains("native compile time:")
                    })
                    .collect::<Vec<_>>()
                    .join("; ");
                eprintln!(
                    "R4_PERF_SETUP {}",
                    serde_json::json!({"backend":backend,"workload":workload,"carrier":carrier,"compile_ns":compile_ns,"native_compile_ns":native_compile_ns,"bytecode_bytes":bytecode_bytes,"locals":locals,"instructions":instructions,"branches":branches,"map_new_calls":map_new_calls,"direct_no_carrier_declaration_baseline_equal":carrier == "direct","code":sizes})
                );
                cases.push(case);
            }
        }
    }
    let rss_before = rss();
    for round in 0..4 {
        let environment = || {
            let processes: Vec<_> = std::fs::read_dir("/proc")
                .unwrap()
                .filter_map(Result::ok)
                .filter_map(|entry| {
                    let pid = entry.file_name().to_string_lossy().parse::<u32>().ok()?;
                    let name = std::fs::read_to_string(entry.path().join("comm")).ok()?;
                    if !matches!(name.trim(), "cargo" | "rustc" | "rustdoc") {
                        return None;
                    }
                    let cwd = std::fs::read_link(entry.path().join("cwd")).ok();
                    Some(serde_json::json!({"pid":pid,"comm":name.trim(),"cwd":cwd}))
                })
                .collect();
            serde_json::json!({"loadavg": std::fs::read_to_string("/proc/loadavg").unwrap(), "affinity": std::fs::read_to_string("/proc/self/status").unwrap().lines().find(|l| l.starts_with("Cpus_allowed_list:")).unwrap_or("unknown").to_string(), "unix_seconds": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64(), "compiler_processes":processes})
        };
        eprintln!(
            "R4_PERF_ROUND {}",
            serde_json::json!({"round":round,"phase":"before","host":environment()})
        );
        let order: Vec<_> = if round % 2 == 0 {
            (0..cases.len()).collect()
        } else {
            (0..cases.len()).rev().collect()
        };
        for index in order {
            let case = &mut cases[index];
            case.machine.reset_for_reuse().unwrap();
            case.machine.set_fuel(1_000_000);
            let snap = case.machine.jit_snapshot();
            let inventory = (
                snap.attempts.len(),
                snap.traces.len(),
                case.machine.jit_native_trace_count(),
            );
            let calls_before = case.calls.load(Ordering::Relaxed);
            let jit_before = case.machine.jit_native_exec_count();
            let aot_before = case.machine.aot_exec_count();
            ALLOCS.with(|v| v.set(0));
            BYTES.with(|v| v.set(0));
            let live_before = LIVE.with(Cell::get);
            ACTIVE.with(|v| v.set(true));
            let start = Instant::now();
            let status = case.machine.run().unwrap();
            let ns = start.elapsed().as_nanos();
            ACTIVE.with(|v| v.set(false));
            let allocs = ALLOCS.with(Cell::get);
            let bytes = BYTES.with(Cell::get);
            let live_delta = LIVE.with(Cell::get) - live_before;
            assert_eq!(status, VmStatus::Halted);
            assert_eq!(
                case.machine.stack().last(),
                Some(&Value::Int(case.expected))
            );
            assert_eq!(case.calls.load(Ordering::Relaxed) - calls_before, 2);
            assert_eq!(case.machine.host_context().resource_count(), 0);
            let snap = case.machine.jit_snapshot();
            assert_eq!(
                inventory,
                (
                    snap.attempts.len(),
                    snap.traces.len(),
                    case.machine.jit_native_trace_count()
                ),
                "warm inventory changed"
            );
            let jit = case.machine.jit_native_exec_count() - jit_before;
            let aot = case.machine.aot_exec_count() - aot_before;
            if case.backend == "trace" {
                assert!(jit > 0);
            }
            if case.backend == "aot" {
                assert!(aot > 0);
            }
            eprintln!(
                "R4_PERF_SAMPLE {}",
                serde_json::json!({"round":round,"backend":case.backend,"workload":case.workload,"carrier":case.carrier,"ns":ns,"allocs":allocs,"bytes":bytes,"live_delta":live_delta,"jit":jit,"aot":aot,"result":case.expected})
            );
        }
        eprintln!(
            "R4_PERF_ROUND {}",
            serde_json::json!({"round":round,"phase":"after","host":environment()})
        );
    }
    let rss_after = rss();
    drop(cases);
    eprintln!(
        "R4_PERF_MEMORY before={rss_before} after={rss_after} after_drop={}",
        rss()
    );
}
