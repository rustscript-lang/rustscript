use std::env;
use std::fmt;
use std::fs;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::process;
use std::time::{SystemTime, UNIX_EPOCH};

use vm::{
    JitConfig, SourceFlavor, Value, Vm, VmStatus, compile_source_file, compile_source_with_flavor,
};

const DEFAULT_VALID_CASES: usize = 150;
const DEFAULT_MUTATION_CASES: usize = 600;
const DEFAULT_MUTATION_CORPUS: usize = 24;
const MAX_MUTATED_SOURCE_LEN: usize = 8192;

fn main() {
    if let Err(err) = run_main() {
        eprintln!("{err}");
        process::exit(1);
    }
}

fn run_main() -> Result<(), String> {
    let config = Config::parse(env::args().skip(1).collect())?;
    fs::create_dir_all(&config.out_dir).map_err(|err| {
        format!(
            "failed to create output dir {}: {err}",
            config.out_dir.display()
        )
    })?;

    println!(
        "rustscript-fuzz seed={} valid={} mutate={} out={}",
        config.seed,
        config.valid_cases,
        config.mutation_cases,
        config.out_dir.display()
    );
    if config.curated_only {
        println!("mode=curated-only");
    }
    if config.skip_jit {
        println!("jit=disabled");
    }
    let mut harness = Harness::new(config);
    harness.run()?;
    Ok(())
}

#[derive(Debug, Clone)]
struct Config {
    seed: u64,
    valid_cases: usize,
    mutation_cases: usize,
    curated_only: bool,
    skip_jit: bool,
    out_dir: PathBuf,
}

impl Config {
    fn parse(args: Vec<String>) -> Result<Self, String> {
        let default_seed = random_seed();
        let mut seed = default_seed;
        let mut valid_cases = DEFAULT_VALID_CASES;
        let mut mutation_cases = DEFAULT_MUTATION_CASES;
        let mut curated_only = false;
        let mut skip_jit = false;
        let mut out_dir: Option<PathBuf> = None;

        let mut index = 0usize;
        while index < args.len() {
            match args[index].as_str() {
                "-h" | "--help" => {
                    println!("{}", usage_text(default_seed));
                    process::exit(0);
                }
                "--seed" => {
                    let raw = args
                        .get(index + 1)
                        .ok_or_else(|| "missing value for --seed".to_string())?;
                    seed = raw
                        .parse::<u64>()
                        .map_err(|_| format!("invalid --seed value '{raw}'"))?;
                    index += 2;
                }
                "--valid" => {
                    let raw = args
                        .get(index + 1)
                        .ok_or_else(|| "missing value for --valid".to_string())?;
                    valid_cases = raw
                        .parse::<usize>()
                        .map_err(|_| format!("invalid --valid value '{raw}'"))?;
                    index += 2;
                }
                "--mutate" => {
                    let raw = args
                        .get(index + 1)
                        .ok_or_else(|| "missing value for --mutate".to_string())?;
                    mutation_cases = raw
                        .parse::<usize>()
                        .map_err(|_| format!("invalid --mutate value '{raw}'"))?;
                    index += 2;
                }
                "--out-dir" => {
                    let raw = args
                        .get(index + 1)
                        .ok_or_else(|| "missing value for --out-dir".to_string())?;
                    out_dir = Some(PathBuf::from(raw));
                    index += 2;
                }
                "--curated-only" => {
                    curated_only = true;
                    index += 1;
                }
                "--skip-jit" => {
                    skip_jit = true;
                    index += 1;
                }
                other => {
                    return Err(format!(
                        "unknown flag '{other}'\n\n{}",
                        usage_text(default_seed)
                    ));
                }
            }
        }

        let out_dir = out_dir.unwrap_or_else(|| default_output_dir(seed));
        Ok(Self {
            seed,
            valid_cases,
            mutation_cases,
            curated_only,
            skip_jit,
            out_dir,
        })
    }
}

fn usage_text(default_seed: u64) -> String {
    format!(
        "Usage: cargo run -p pd-vm --example rustscript_fuzz -- [options]

Options:
  --seed <u64>        Deterministic seed (default random; current suggestion {default_seed})
  --valid <count>     Number of random valid programs to generate (default {DEFAULT_VALID_CASES})
  --mutate <count>    Number of compile-only mutation cases (default {DEFAULT_MUTATION_CASES})
  --out-dir <path>    Directory for summaries and failing repros
  --curated-only      Run only large curated correctness cases
  --skip-jit          Skip JIT runtime verification for valid cases
  -h, --help          Show this help
"
    )
}

fn default_output_dir(seed: u64) -> PathBuf {
    workspace_root()
        .join("target")
        .join("rustscript-fuzz")
        .join(format!(
            "seed-{seed}-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
        ))
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("pd-vm crate should have workspace parent")
        .to_path_buf()
}

fn examples_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("examples")
}

fn random_seed() -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    now.as_secs() ^ (u64::from(now.subsec_nanos()) << 1)
}

struct Harness {
    config: Config,
    rng: Rng,
    stats: Stats,
}

impl Harness {
    fn new(config: Config) -> Self {
        Self {
            rng: Rng::new(config.seed),
            stats: Stats::default(),
            config,
        }
    }

    fn run(&mut self) -> Result<(), String> {
        self.run_curated_cases();
        if !self.config.curated_only {
            self.run_random_valid_cases();
            self.run_mutation_cases();
        }

        let summary_path = self.write_summary()?;
        println!(
            "summary curated={}/{} valid={}/{} mutate={}/{} failures={} panics={} file={}",
            self.stats.curated_passed,
            self.stats.curated_total,
            self.stats.valid_passed,
            self.stats.valid_total,
            self.stats.mutation_passed,
            self.stats.mutation_total,
            self.stats.failures.len(),
            self.stats.panic_failures,
            summary_path.display()
        );
        if self.stats.failures.is_empty() {
            return Ok(());
        }
        Err(format!(
            "fuzz harness found {} failure(s); see {}",
            self.stats.failures.len(),
            summary_path.display()
        ))
    }

    fn run_curated_cases(&mut self) {
        let cases = curated_cases();
        self.stats.curated_total = cases.len();
        for case in cases {
            match self.execute_valid_case(&case) {
                Ok(()) => {
                    self.stats.curated_passed += 1;
                }
                Err(failure) => self.record_failure(failure),
            }
        }
    }

    fn run_random_valid_cases(&mut self) {
        self.stats.valid_total = self.config.valid_cases;
        for index in 0..self.config.valid_cases {
            if index > 0 && index % 25 == 0 {
                println!("progress valid {index}/{}", self.config.valid_cases);
            }
            let case = generate_valid_case(&mut self.rng, index);
            match self.execute_valid_case(&case) {
                Ok(()) => {
                    self.stats.valid_passed += 1;
                }
                Err(failure) => self.record_failure(failure),
            }
        }
    }

    fn run_mutation_cases(&mut self) {
        let mutation_corpus = mutation_corpus(&mut self.rng);
        self.stats.mutation_total = self.config.mutation_cases;
        for index in 0..self.config.mutation_cases {
            if index > 0 && index % 100 == 0 {
                println!("progress mutate {index}/{}", self.config.mutation_cases);
            }
            let name = format!("mutate-{index:04}");
            let source = mutate_source(&mut self.rng, &mutation_corpus);
            match compile_only_case(&name, source) {
                Ok(()) => {
                    self.stats.mutation_passed += 1;
                }
                Err(failure) => self.record_failure(failure),
            }
        }
    }

    fn execute_valid_case(&self, case: &ValidCase) -> Result<(), FailureRecord> {
        let source = case.source_text().map_err(|detail| FailureRecord {
            lane: case.lane,
            name: case.name.clone(),
            source: String::new(),
            detail,
            phase: FailurePhase::SetupError,
        })?;

        let compiled =
            catch_panic(format!("compile {}", case.name), || case.compile()).map_err(|detail| {
                FailureRecord {
                    lane: case.lane,
                    name: case.name.clone(),
                    source: source.clone(),
                    detail,
                    phase: FailurePhase::CompilePanic,
                }
            })?;
        let compiled = compiled.map_err(|detail| FailureRecord {
            lane: case.lane,
            name: case.name.clone(),
            source: source.clone(),
            detail,
            phase: FailurePhase::CompileError,
        })?;

        let program = compiled.program.with_local_count(compiled.locals);
        let interpreted = catch_panic(format!("run interpreter {}", case.name), || {
            run_program(&program, ExecutionMode::Interpreter)
        })
        .map_err(|detail| FailureRecord {
            lane: case.lane,
            name: case.name.clone(),
            source: source.clone(),
            detail,
            phase: FailurePhase::InterpreterPanic,
        })?;
        let interpreted = interpreted.map_err(|detail| FailureRecord {
            lane: case.lane,
            name: case.name.clone(),
            source: source.clone(),
            detail,
            phase: FailurePhase::RuntimeError,
        })?;
        assert_expected_stack(case, "interpreter", &source, &interpreted)?;

        if !self.config.skip_jit {
            let jitted = catch_panic(format!("run jit {}", case.name), || {
                run_program(&program, ExecutionMode::Jit)
            })
            .map_err(|detail| FailureRecord {
                lane: case.lane,
                name: case.name.clone(),
                source: source.clone(),
                detail,
                phase: FailurePhase::JitPanic,
            })?;
            let jitted = jitted.map_err(|detail| FailureRecord {
                lane: case.lane,
                name: case.name.clone(),
                source: source.clone(),
                detail,
                phase: FailurePhase::RuntimeError,
            })?;
            assert_expected_stack(case, "jit", &source, &jitted)?;
            if jitted != interpreted {
                return Err(FailureRecord {
                    lane: case.lane,
                    name: case.name.clone(),
                    source,
                    detail: format!(
                        "jit stack diverged from interpreter\nexpected: {}\ninterpreted: {}\njit: {}",
                        ValueStack(&case.expected_stack),
                        ValueStack(&interpreted),
                        ValueStack(&jitted)
                    ),
                    phase: FailurePhase::StackMismatch,
                });
            }
        }

        Ok(())
    }

    fn record_failure(&mut self, failure: FailureRecord) {
        if failure.phase.is_panic() {
            self.stats.panic_failures += 1;
        }
        let artifact_index = self.stats.failures.len();
        let source_path = self.config.out_dir.join(format!(
            "{artifact_index:04}-{}-{}.rss",
            failure.lane,
            sanitize_name(&failure.name)
        ));
        let detail_path = self.config.out_dir.join(format!(
            "{artifact_index:04}-{}-{}.txt",
            failure.lane,
            sanitize_name(&failure.name)
        ));

        if !failure.source.is_empty() {
            let _ = fs::write(&source_path, &failure.source);
        }
        let _ = fs::write(
            &detail_path,
            format!(
                "lane={}\nname={}\nphase={}\n\n{}\n",
                failure.lane, failure.name, failure.phase, failure.detail
            ),
        );

        println!(
            "failure lane={} phase={} name={} detail_file={}",
            failure.lane,
            failure.phase,
            failure.name,
            detail_path.display()
        );
        self.stats.failures.push(FailureSummary {
            lane: failure.lane,
            name: failure.name,
            phase: failure.phase,
            detail_path,
            source_path: if failure.source.is_empty() {
                None
            } else {
                Some(source_path)
            },
        });
    }

    fn write_summary(&self) -> Result<PathBuf, String> {
        let summary_path = self.config.out_dir.join("summary.txt");
        let mut lines = Vec::new();
        lines.push(format!("seed={}", self.config.seed));
        lines.push(format!("curated_total={}", self.stats.curated_total));
        lines.push(format!("curated_passed={}", self.stats.curated_passed));
        lines.push(format!("valid_total={}", self.stats.valid_total));
        lines.push(format!("valid_passed={}", self.stats.valid_passed));
        lines.push(format!("mutation_total={}", self.stats.mutation_total));
        lines.push(format!("mutation_passed={}", self.stats.mutation_passed));
        lines.push(format!("panic_failures={}", self.stats.panic_failures));
        lines.push(format!("failure_count={}", self.stats.failures.len()));
        if !self.stats.failures.is_empty() {
            lines.push(String::new());
            lines.push("failures:".to_string());
            for failure in &self.stats.failures {
                lines.push(format!(
                    "lane={} phase={} name={} detail={}{}",
                    failure.lane,
                    failure.phase,
                    failure.name,
                    failure.detail_path.display(),
                    failure
                        .source_path
                        .as_ref()
                        .map(|path| format!(" source={}", path.display()))
                        .unwrap_or_default()
                ));
            }
        }
        fs::write(&summary_path, lines.join("\n"))
            .map_err(|err| format!("failed to write summary {}: {err}", summary_path.display()))?;
        Ok(summary_path)
    }
}

#[derive(Default)]
struct Stats {
    curated_total: usize,
    curated_passed: usize,
    valid_total: usize,
    valid_passed: usize,
    mutation_total: usize,
    mutation_passed: usize,
    panic_failures: usize,
    failures: Vec<FailureSummary>,
}

struct FailureSummary {
    lane: &'static str,
    name: String,
    phase: FailurePhase,
    detail_path: PathBuf,
    source_path: Option<PathBuf>,
}

struct FailureRecord {
    lane: &'static str,
    name: String,
    source: String,
    detail: String,
    phase: FailurePhase,
}

#[derive(Clone, Copy, Debug)]
enum FailurePhase {
    SetupError,
    CompileError,
    CompilePanic,
    RuntimeError,
    InterpreterPanic,
    JitPanic,
    StackMismatch,
}

impl FailurePhase {
    fn is_panic(self) -> bool {
        matches!(
            self,
            Self::CompilePanic | Self::InterpreterPanic | Self::JitPanic
        )
    }
}

impl fmt::Display for FailurePhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            FailurePhase::SetupError => "setup_error",
            FailurePhase::CompileError => "compile_error",
            FailurePhase::CompilePanic => "compile_panic",
            FailurePhase::RuntimeError => "runtime_error",
            FailurePhase::InterpreterPanic => "interpreter_panic",
            FailurePhase::JitPanic => "jit_panic",
            FailurePhase::StackMismatch => "stack_mismatch",
        };
        write!(f, "{text}")
    }
}

enum ProgramInput {
    Inline(String),
    File(PathBuf),
}

struct ValidCase {
    lane: &'static str,
    name: String,
    input: ProgramInput,
    expected_stack: Vec<Value>,
}

impl ValidCase {
    fn compile(&self) -> Result<vm::CompiledProgram, String> {
        match &self.input {
            ProgramInput::Inline(source) => {
                compile_source_with_flavor(source, SourceFlavor::RustScript)
                    .map_err(|err| err.to_string())
            }
            ProgramInput::File(path) => compile_source_file(path).map_err(|err| err.to_string()),
        }
    }

    fn source_text(&self) -> Result<String, String> {
        match &self.input {
            ProgramInput::Inline(source) => Ok(source.clone()),
            ProgramInput::File(path) => fs::read_to_string(path)
                .map_err(|err| format!("failed to read {}: {err}", path.display())),
        }
    }
}

#[derive(Clone, Copy)]
enum ExecutionMode {
    Interpreter,
    Jit,
}

fn run_program(program: &vm::Program, mode: ExecutionMode) -> Result<Vec<Value>, String> {
    match mode {
        ExecutionMode::Interpreter => {
            let mut vm = Vm::new(program.clone());
            configure_vm(&mut vm);
            vm.set_jit_config(JitConfig {
                enabled: false,
                hot_loop_threshold: 8,
                max_trace_len: 256,
            });
            run_vm_to_completion(&mut vm)
        }
        ExecutionMode::Jit => {
            let mut vm = Vm::new(program.clone());
            configure_vm(&mut vm);
            vm.set_jit_config(JitConfig {
                enabled: true,
                hot_loop_threshold: 1,
                max_trace_len: 512,
            });
            run_vm_to_completion(&mut vm)
        }
    }
}

fn configure_vm(vm: &mut Vm) {
    vm.set_runtime_print_sink(|_rendered| {});
}

fn run_vm_to_completion(vm: &mut Vm) -> Result<Vec<Value>, String> {
    let mut started = false;
    loop {
        let status = if started {
            vm.resume()
        } else {
            started = true;
            vm.run()
        }
        .map_err(|err| format!("vm execution failed: {err}"))?;

        match status {
            VmStatus::Halted => return Ok(vm.stack().to_vec()),
            VmStatus::Yielded => continue,
            VmStatus::Waiting(_op_id) => vm
                .wait_for_host_op_blocking()
                .map_err(|err| format!("vm wait failed: {err}"))?,
        }
    }
}

fn assert_expected_stack(
    case: &ValidCase,
    backend: &str,
    source: &str,
    actual_stack: &[Value],
) -> Result<(), FailureRecord> {
    if actual_stack == case.expected_stack.as_slice() {
        return Ok(());
    }
    Err(FailureRecord {
        lane: case.lane,
        name: case.name.clone(),
        source: source.to_string(),
        detail: format!(
            "{backend} produced unexpected stack\nexpected: {}\nactual: {}",
            ValueStack(&case.expected_stack),
            ValueStack(actual_stack)
        ),
        phase: FailurePhase::StackMismatch,
    })
}

fn compile_only_case(name: &str, source: String) -> Result<(), FailureRecord> {
    let _ = catch_panic(format!("compile mutation {name}"), || {
        compile_source_with_flavor(&source, SourceFlavor::RustScript)
    })
    .map_err(|detail| FailureRecord {
        lane: "mutation",
        name: name.to_string(),
        source: source.clone(),
        detail,
        phase: FailurePhase::CompilePanic,
    })?;
    Ok(())
}

fn curated_cases() -> Vec<ValidCase> {
    let example_root = examples_root();
    let hybrid = large_hybrid_case();
    vec![
        ValidCase {
            lane: "curated",
            name: "aes_128_cbc_usage".to_string(),
            input: ProgramInput::File(example_root.join("aes_128_cbc_usage.rss")),
            expected_stack: vec![Value::string("7649abac8119b246cee98e9b12e9197d")],
        },
        ValidCase {
            lane: "curated",
            name: "ifft_math".to_string(),
            input: ProgramInput::File(example_root.join("ifft_math.rss")),
            expected_stack: vec![
                Value::Float(1.0),
                Value::Float(2.0),
                Value::Float(3.0),
                Value::Float(4.0),
            ],
        },
        ValidCase {
            lane: "curated",
            name: hybrid.name,
            input: ProgramInput::Inline(hybrid.source),
            expected_stack: vec![Value::Int(hybrid.expected)],
        },
    ]
}

struct LargeHybrid {
    name: String,
    source: String,
    expected: i64,
}

fn large_hybrid_case() -> LargeHybrid {
    let bias = 7i64;
    let values = (0..24)
        .map(|i| (((i * 3) % 11) as i64) + bias + i as i64)
        .collect::<Vec<_>>();
    let total = values.iter().copied().sum::<i64>();
    let expected = total + 8;
    let source = r#"
struct Score { value: int, weight: int }
struct Envelope { score: Score }

let mut bias = 7;
fn adjust(v) { v + bias }
bias = 1000;

let mut values = [];
let mut i = 0;
while i < 24 {
    let raw = (i * 3) % 11;
    values[values.length] = adjust(raw) + i;
    i = i + 1;
}

let mut idx = 0;
let mut even = 0;
let mut odd = 0;
while idx < values.length {
    if (idx % 2) == 0 {
        even = even + values[idx];
    } else {
        odd = odd + values[idx];
    }
    idx = idx + 1;
}

let present: Envelope = { score: { value: even, weight: 3 } };
let missing: Envelope = null;

present?.score?.value.unwrap_or(0)
    + present?.score?.weight.unwrap_or(0)
    + missing?.score?.value.unwrap_or(5)
    + odd;
"#
    .trim()
    .to_string();
    LargeHybrid {
        name: "large_hybrid_optional_capture".to_string(),
        source,
        expected,
    }
}

fn generate_valid_case(rng: &mut Rng, index: usize) -> ValidCase {
    let pick = rng.usize(0, 6);
    match pick {
        0 => loop_accumulator_case(rng, index),
        1 => nested_break_case(rng, index),
        2 => named_capture_case(rng, index),
        3 => closure_fold_case(rng, index),
        4 => array_builder_case(rng, index),
        _ => optional_schema_case(rng, index),
    }
}

fn loop_accumulator_case(rng: &mut Rng, index: usize) -> ValidCase {
    let start = rng.i64(0, 5);
    let limit = start + rng.i64(8, 24);
    let step = rng.i64(1, 3);
    let modulus = rng.i64(2, 6);
    let target = rng.i64(0, modulus - 1);
    let add_mul = rng.i64(1, 5);
    let add_bias = rng.i64(0, 9);
    let sub_mul = rng.i64(1, 4);
    let sub_bias = rng.i64(-4, 6);
    let base = rng.i64(-15, 20);

    let mut i = start;
    let mut acc = base;
    while i < limit {
        if i % modulus == target {
            acc += (i * add_mul) + add_bias;
        } else {
            acc = acc - (i * sub_mul) + sub_bias;
        }
        i += step;
    }

    ValidCase {
        lane: "valid",
        name: format!("loop_accumulator_{index:04}"),
        input: ProgramInput::Inline(format!(
            r#"
let mut i = {start};
let mut acc = {base};
while i < {limit} {{
    if (i % {modulus}) == {target} {{
        acc = acc + (i * {add_mul}) + {add_bias};
    }} else {{
        acc = acc - (i * {sub_mul}) + {sub_bias};
    }}
    i = i + {step};
}}
acc;
"#
        )),
        expected_stack: vec![Value::Int(acc)],
    }
}

fn nested_break_case(rng: &mut Rng, index: usize) -> ValidCase {
    let outer_limit = rng.i64(2, 7);
    let inner_limit = rng.i64(3, 8);
    let break_at = rng.i64(1, inner_limit - 1);
    let modulus = rng.i64(2, 5);
    let target = rng.i64(0, modulus - 1);
    let outer_mul = rng.i64(2, 6);
    let inner_mul = rng.i64(1, 4);
    let bonus = rng.i64(-2, 7);
    let penalty = rng.i64(-3, 5);
    let base = rng.i64(-10, 15);

    let mut outer = 0i64;
    let mut total = base;
    while outer < outer_limit {
        let mut inner = 0i64;
        while inner < inner_limit {
            if inner == break_at {
                break;
            }
            if (outer + inner) % modulus == target {
                total += (outer * outer_mul) + (inner * inner_mul) + bonus;
            } else {
                total = total - inner + penalty;
            }
            inner += 1;
        }
        outer += 1;
    }

    ValidCase {
        lane: "valid",
        name: format!("nested_break_{index:04}"),
        input: ProgramInput::Inline(format!(
            r#"
let mut outer = 0;
let mut total = {base};
while outer < {outer_limit} {{
    let mut inner = 0;
    while inner < {inner_limit} {{
        if inner == {break_at} {{
            break;
        }}
        if ((outer + inner) % {modulus}) == {target} {{
            total = total + (outer * {outer_mul}) + (inner * {inner_mul}) + {bonus};
        }} else {{
            total = total - inner + {penalty};
        }}
        inner = inner + 1;
    }}
    outer = outer + 1;
}}
total;
"#
        )),
        expected_stack: vec![Value::Int(total)],
    }
}

fn named_capture_case(rng: &mut Rng, index: usize) -> ValidCase {
    let bias = rng.i64(1, 12);
    let changed_bias = rng.i64(20, 80);
    let scale = rng.i64(1, 6);
    let n = rng.i64(4, 16);
    let shift = rng.i64(-3, 9);

    let mut acc = 0i64;
    for i in 0..n {
        let adjusted = (i * scale) + bias;
        acc += adjusted - shift;
    }

    ValidCase {
        lane: "valid",
        name: format!("named_capture_{index:04}"),
        input: ProgramInput::Inline(format!(
            r#"
let mut bias = {bias};
fn adjust(v) {{ (v * {scale}) + bias }}
fn mix(v) {{ adjust(v) - {shift} }}
bias = {changed_bias};

let mut i = 0;
let mut acc = 0;
while i < {n} {{
    acc = acc + mix(i);
    i = i + 1;
}}
acc;
"#
        )),
        expected_stack: vec![Value::Int(acc)],
    }
}

fn closure_fold_case(rng: &mut Rng, index: usize) -> ValidCase {
    let scale = rng.i64(1, 6);
    let shift = rng.i64(-4, 8);
    let start = rng.i64(0, 5);
    let n = rng.i64(3, 12);

    let mut acc = 0i64;
    for i in 0..n {
        let value = i + start;
        acc += (value * scale) + shift;
    }

    ValidCase {
        lane: "valid",
        name: format!("closure_fold_{index:04}"),
        input: ProgramInput::Inline(format!(
            r#"
let scale = {scale};
let shift = {shift};
let map = |value| (value * scale) + shift;

let mut i = 0;
let mut acc = 0;
while i < {n} {{
    acc = acc + map(i + {start});
    i = i + 1;
}}
acc;
"#
        )),
        expected_stack: vec![Value::Int(acc)],
    }
}

fn array_builder_case(rng: &mut Rng, index: usize) -> ValidCase {
    let n = rng.usize(3, 12);
    let mul = rng.i64(1, 6);
    let bias = rng.i64(-3, 8);
    let pick = rng.usize(0, n - 1);
    let mut values = Vec::with_capacity(n);
    for idx in 0..n {
        values.push((idx as i64 * mul) + bias);
    }
    let sum = values.iter().copied().sum::<i64>();
    let expected = sum + values[pick];

    ValidCase {
        lane: "valid",
        name: format!("array_builder_{index:04}"),
        input: ProgramInput::Inline(format!(
            r#"
let mut values = [];
let mut i = 0;
while i < {n} {{
    values[values.length] = (i * {mul}) + {bias};
    i = i + 1;
}}

let mut idx = 0;
let mut sum = 0;
while idx < values.length {{
    sum = sum + values[idx];
    idx = idx + 1;
}}
sum + values[{pick}];
"#
        )),
        expected_stack: vec![Value::Int(expected)],
    }
}

fn optional_schema_case(rng: &mut Rng, index: usize) -> ValidCase {
    let left = rng.i64(1, 40);
    let right = rng.i64(1, 40);
    let fallback = rng.i64(0, 15);
    let extra = rng.i64(-5, 9);
    let expected = left + fallback + extra;

    ValidCase {
        lane: "valid",
        name: format!("optional_schema_{index:04}"),
        input: ProgramInput::Inline(format!(
            r#"
struct Stats {{ left: int, right: int }}
struct Wrapper {{ stats: Stats }}

let present: Wrapper = {{ stats: {{ left: {left}, right: {right} }} }};
let missing: Wrapper = null;

let a = present?.stats?.left.unwrap_or(0);
let b = missing?.stats?.right.unwrap_or({fallback});
a + b + {extra};
"#
        )),
        expected_stack: vec![Value::Int(expected)],
    }
}

fn mutation_corpus(rng: &mut Rng) -> Vec<String> {
    let mut corpus = Vec::with_capacity(DEFAULT_MUTATION_CORPUS + 4);
    corpus.push(large_hybrid_case().source);
    corpus.push(
        r#"
struct Stats { score: int }
struct Wrapper { stats: Stats }
let entry: Wrapper = { stats: { score: 41 } };
match entry?.stats?.score {
    None => 0,
    Some(score) => score + 1,
    _ => 0,
};
"#
        .trim()
        .to_string(),
    );
    corpus.push(
        r#"
let mut a = [];
let mut i = 0;
while i < 6 {
    a[a.length] = i;
    i = i + 1;
}
a[1] + a[4];
"#
        .trim()
        .to_string(),
    );
    corpus.push(
        r#"
let scale = 3;
let map = |value| value * scale;
map(7) + map(2);
"#
        .trim()
        .to_string(),
    );
    while corpus.len() < DEFAULT_MUTATION_CORPUS {
        corpus.push(
            generate_valid_case(rng, corpus.len())
                .source_text()
                .unwrap_or_default(),
        );
    }
    corpus
}

fn mutate_source(rng: &mut Rng, corpus: &[String]) -> String {
    let mut source = corpus[rng.usize(0, corpus.len() - 1)].clone();
    let edit_count = rng.usize(2, 8);
    for _ in 0..edit_count {
        let action = rng.usize(0, 5);
        match action {
            0 => insert_token(rng, &mut source),
            1 => delete_span(rng, &mut source),
            2 => replace_span(rng, &mut source),
            3 => duplicate_span(rng, &mut source),
            _ => splice_corpus_fragment(rng, &mut source, corpus),
        }
        if source.len() > MAX_MUTATED_SOURCE_LEN {
            source.truncate(MAX_MUTATED_SOURCE_LEN);
        }
    }
    if rng.one_in(4) {
        let cut = rng.usize(0, source.len());
        source.truncate(cut);
    }
    if source.trim().is_empty() {
        source.push(';');
    }
    source
}

fn insert_token(rng: &mut Rng, source: &mut String) {
    let position = rng.usize(0, source.len());
    source.insert_str(position, mutation_token(rng));
}

fn delete_span(rng: &mut Rng, source: &mut String) {
    if source.is_empty() {
        return;
    }
    let start = rng.usize(0, source.len() - 1);
    let len = rng.usize(1, (source.len() - start).min(24));
    source.replace_range(start..start + len, "");
}

fn replace_span(rng: &mut Rng, source: &mut String) {
    if source.is_empty() {
        source.push_str(mutation_token(rng));
        return;
    }
    let start = rng.usize(0, source.len() - 1);
    let len = rng.usize(1, (source.len() - start).min(20));
    source.replace_range(start..start + len, mutation_token(rng));
}

fn duplicate_span(rng: &mut Rng, source: &mut String) {
    if source.is_empty() {
        return;
    }
    let start = rng.usize(0, source.len() - 1);
    let len = rng.usize(1, (source.len() - start).min(20));
    let fragment = source[start..start + len].to_string();
    let insert_at = rng.usize(0, source.len());
    source.insert_str(insert_at, &fragment);
}

fn splice_corpus_fragment(rng: &mut Rng, source: &mut String, corpus: &[String]) {
    let donor = &corpus[rng.usize(0, corpus.len() - 1)];
    if donor.is_empty() {
        return;
    }
    let start = rng.usize(0, donor.len() - 1);
    let len = rng.usize(1, (donor.len() - start).min(32));
    let fragment = &donor[start..start + len];
    let insert_at = rng.usize(0, source.len());
    source.insert_str(insert_at, fragment);
}

fn mutation_token(rng: &mut Rng) -> &'static str {
    const TOKENS: &[&str] = &[
        "let ",
        "mut ",
        "fn x(a) { a }\n",
        "struct X { a: int }\n",
        "while ",
        "if ",
        "else ",
        "match ",
        "Some(",
        "None",
        "null",
        "unwrap_or(",
        "?.",
        "::",
        " += ",
        " == ",
        " [",
        "] ",
        "{",
        "}",
        "(",
        ")",
        ";",
        "\n",
        "\"",
        "/*",
        "*/",
        "// fuzz\n",
        "0x8000000000000000",
        "runtime::sleep(",
    ];
    TOKENS[rng.usize(0, TOKENS.len() - 1)]
}

fn catch_panic<T, F>(label: String, f: F) -> Result<T, String>
where
    F: FnOnce() -> T,
{
    catch_unwind(AssertUnwindSafe(f)).map_err(|payload| {
        let message = if let Some(text) = payload.downcast_ref::<&str>() {
            (*text).to_string()
        } else if let Some(text) = payload.downcast_ref::<String>() {
            text.clone()
        } else {
            "non-string panic payload".to_string()
        };
        format!("{label} panicked: {message}")
    })
}

fn sanitize_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "case".to_string()
    } else {
        out
    }
}

struct ValueStack<'a>(&'a [Value]);

impl fmt::Display for ValueStack<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.0)
    }
}

struct Rng {
    state: u64,
}

impl Rng {
    fn new(seed: u64) -> Self {
        let state = if seed == 0 {
            0xA5A5_5A5A_DEAD_BEEF
        } else {
            seed
        };
        Self { state }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }

    fn usize(&mut self, min: usize, max: usize) -> usize {
        assert!(min <= max);
        if min == max {
            return min;
        }
        let span = max - min + 1;
        min + (self.next_u64() as usize % span)
    }

    fn i64(&mut self, min: i64, max: i64) -> i64 {
        assert!(min <= max);
        if min == max {
            return min;
        }
        let span = (max - min + 1) as u64;
        min + (self.next_u64() % span) as i64
    }

    fn one_in(&mut self, denominator: u64) -> bool {
        denominator > 0 && self.next_u64().is_multiple_of(denominator)
    }
}
