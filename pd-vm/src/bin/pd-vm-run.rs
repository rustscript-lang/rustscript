use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;
use vm::{
    CallOutcome, Debugger, DisassembleOptions, FunctionDecl, HostFunction, Value, Vm, VmError,
    VmRecording, VmStatus, compile_source, compile_source_file, disassemble_vmbc_with_options,
    encode_program, replay_recording_stdio,
};

const DEFAULT_SOURCE: &str = "examples/example.rss";

#[derive(Debug, Clone, PartialEq, Eq)]
struct CliConfig {
    source: Option<String>,
    emit_vmbc_path: Option<String>,
    disasm_vmbc_path: Option<String>,
    record_path: Option<String>,
    view_recording_path: Option<String>,
    show_source: bool,
    repl: bool,
    debug: bool,
    tcp_addr: Option<String>,
    stop_on_entry: bool,
    jit_dump: bool,
    jit_hot_loop_threshold: Option<u32>,
    help: bool,
}

impl Default for CliConfig {
    fn default() -> Self {
        Self {
            source: None,
            emit_vmbc_path: None,
            disasm_vmbc_path: None,
            record_path: None,
            view_recording_path: None,
            show_source: false,
            repl: false,
            debug: false,
            tcp_addr: None,
            stop_on_entry: true,
            jit_dump: false,
            jit_hot_loop_threshold: None,
            help: false,
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cli = parse_cli_args(&args).map_err(io::Error::other)?;
    if cli.help {
        print_usage();
        return Ok(());
    }
    if cli.repl {
        return run_repl();
    }
    if let Some(input_path) = cli.disasm_vmbc_path.as_ref() {
        let bytes = std::fs::read(input_path)?;
        let listing = disassemble_vmbc_with_options(
            &bytes,
            DisassembleOptions {
                show_source: cli.show_source,
            },
        )?;
        print!("{listing}");
        return Ok(());
    }
    if let Some(recording_path) = cli.view_recording_path.as_ref() {
        let recording = VmRecording::load_from_file(recording_path)?;
        replay_recording_stdio(&recording);
        return Ok(());
    }

    let source_path = resolve_source_path(cli.source.as_deref())?;
    let compiled = compile_source_file(&source_path)?;
    if let Some(output_path) = cli.emit_vmbc_path.as_ref() {
        let encoded = encode_program(&compiled.program)?;
        std::fs::write(output_path, &encoded)?;
        println!("wrote {} bytes to {}", encoded.len(), output_path);
        return Ok(());
    }
    let recording_program = cli.record_path.as_ref().map(|_| compiled.program.clone());
    let mut vm = Vm::with_locals(compiled.program, compiled.locals);
    if let Some(hot_loop) = cli.jit_hot_loop_threshold {
        let mut jit_config = vm.jit_config().clone();
        jit_config.hot_loop_threshold = hot_loop;
        vm.set_jit_config(jit_config);
    }
    register_functions(&mut vm, &compiled.functions)?;

    if let Some(record_path) = cli.record_path.as_ref() {
        let program = recording_program.expect("recording mode should clone program");
        let mut debugger = Debugger::with_recording(program);
        loop {
            let status = vm.run_with_debugger(&mut debugger)?;
            match status {
                VmStatus::Halted => {
                    println!("vm halted");
                    println!("stack: {:?}", vm.stack());
                    break;
                }
                VmStatus::Yielded => {
                    println!("vm yielded, resuming...");
                    continue;
                }
            }
        }
        let recording = debugger
            .take_recording()
            .ok_or_else(|| io::Error::other("recording state unavailable"))?;
        recording.save_to_file(record_path)?;
        println!(
            "recording saved to {} (frames={})",
            record_path,
            recording.frames.len()
        );
        return Ok(());
    }

    let mut debugger = if cli.debug {
        let mut debugger = if let Some(addr) = &cli.tcp_addr {
            println!("[debug] tcp debugger listening on {addr}");
            Debugger::with_tcp(addr)?
        } else {
            Debugger::new()
        };
        if cli.stop_on_entry {
            debugger.stop_on_entry();
        }
        Some(debugger)
    } else {
        None
    };

    loop {
        let status = if let Some(debugger) = debugger.as_mut() {
            vm.run_with_debugger(debugger)?
        } else {
            vm.run()?
        };
        match status {
            VmStatus::Halted => {
                println!("vm halted");
                println!("stack: {:?}", vm.stack());
                break;
            }
            VmStatus::Yielded => {
                println!("vm yielded, resuming...");
                continue;
            }
        }
    }
    if cli.jit_dump {
        println!("{}", vm.dump_jit_info());
    }
    Ok(())
}

fn parse_cli_args(args: &[String]) -> Result<CliConfig, String> {
    let mut cfg = CliConfig::default();
    if args.is_empty() {
        cfg.repl = true;
        return Ok(cfg);
    }
    let mut index = 0usize;

    if let Some(first) = args.first()
        && first == "debug"
    {
        cfg.debug = true;
        index = 1;
    } else if let Some(first) = args.first()
        && first == "repl"
    {
        cfg.repl = true;
        index = 1;
    }

    while index < args.len() {
        match args[index].as_str() {
            "-h" | "--help" => {
                cfg.help = true;
                index += 1;
            }
            "--debug" => {
                cfg.debug = true;
                index += 1;
            }
            "--tcp" => {
                cfg.debug = true;
                let addr = args
                    .get(index + 1)
                    .ok_or_else(|| "missing value for --tcp".to_string())?
                    .clone();
                cfg.tcp_addr = Some(addr);
                index += 2;
            }
            "--stop-on-entry" => {
                cfg.debug = true;
                cfg.stop_on_entry = true;
                index += 1;
            }
            "--no-stop-on-entry" => {
                cfg.debug = true;
                cfg.stop_on_entry = false;
                index += 1;
            }
            "--jit-dump" => {
                cfg.jit_dump = true;
                index += 1;
            }
            "--jit-hot-loop" => {
                let raw = args
                    .get(index + 1)
                    .ok_or_else(|| "missing value for --jit-hot-loop".to_string())?;
                let value = raw
                    .parse::<u32>()
                    .map_err(|_| format!("invalid --jit-hot-loop value '{raw}'"))?;
                cfg.jit_hot_loop_threshold = Some(value);
                index += 2;
            }
            "--emit-vmbc" => {
                let path = args
                    .get(index + 1)
                    .ok_or_else(|| "missing value for --emit-vmbc".to_string())?;
                cfg.emit_vmbc_path = Some(path.clone());
                index += 2;
            }
            "--disasm-vmbc" => {
                let path = args
                    .get(index + 1)
                    .ok_or_else(|| "missing value for --disasm-vmbc".to_string())?;
                cfg.disasm_vmbc_path = Some(path.clone());
                index += 2;
            }
            "--record" => {
                let path = args
                    .get(index + 1)
                    .ok_or_else(|| "missing value for --record".to_string())?;
                cfg.record_path = Some(path.clone());
                index += 2;
            }
            "--view-record" => {
                let path = args
                    .get(index + 1)
                    .ok_or_else(|| "missing value for --view-record".to_string())?;
                cfg.view_recording_path = Some(path.clone());
                index += 2;
            }
            "--show-source" => {
                cfg.show_source = true;
                index += 1;
            }
            "--repl" => {
                cfg.repl = true;
                index += 1;
            }
            value if value.starts_with('-') => {
                return Err(format!("unknown flag '{value}'"));
            }
            path => {
                if cfg.source.is_some() {
                    return Err("multiple source paths provided".to_string());
                }
                cfg.source = Some(path.to_string());
                index += 1;
            }
        }
    }

    if cfg.repl {
        if cfg.source.is_some() {
            return Err("repl mode does not accept a source path".to_string());
        }
        if cfg.debug
            || cfg.tcp_addr.is_some()
            || cfg.jit_dump
            || cfg.jit_hot_loop_threshold.is_some()
            || cfg.emit_vmbc_path.is_some()
            || cfg.record_path.is_some()
            || cfg.view_recording_path.is_some()
        {
            return Err(
                "repl mode cannot be combined with debug/jit/emit-vmbc runtime flags".to_string(),
            );
        }
    }
    if cfg.disasm_vmbc_path.is_some() {
        if cfg.source.is_some() {
            return Err("disasm mode does not accept a source path".to_string());
        }
        if cfg.repl
            || cfg.debug
            || cfg.tcp_addr.is_some()
            || cfg.jit_dump
            || cfg.jit_hot_loop_threshold.is_some()
            || cfg.emit_vmbc_path.is_some()
            || cfg.record_path.is_some()
            || cfg.view_recording_path.is_some()
        {
            return Err(
                "disasm mode cannot be combined with repl/debug/jit/emit-vmbc runtime flags"
                    .to_string(),
            );
        }
    } else if cfg.show_source {
        return Err("--show-source requires --disasm-vmbc".to_string());
    }
    if cfg.record_path.is_some()
        && (cfg.debug
            || cfg.tcp_addr.is_some()
            || cfg.jit_dump
            || cfg.jit_hot_loop_threshold.is_some()
            || cfg.emit_vmbc_path.is_some()
            || cfg.disasm_vmbc_path.is_some()
            || cfg.view_recording_path.is_some()
            || cfg.show_source)
    {
        return Err(
            "record mode cannot be combined with debug/jit/emit/disasm/view-record flags"
                .to_string(),
        );
    }
    if cfg.view_recording_path.is_some()
        && (cfg.source.is_some()
            || cfg.debug
            || cfg.tcp_addr.is_some()
            || cfg.jit_dump
            || cfg.jit_hot_loop_threshold.is_some()
            || cfg.emit_vmbc_path.is_some()
            || cfg.disasm_vmbc_path.is_some()
            || cfg.record_path.is_some()
            || cfg.show_source)
    {
        return Err(
            "view-record mode cannot be combined with source/debug/jit/emit/disasm flags"
                .to_string(),
        );
    }

    Ok(cfg)
}

fn resolve_source_path(arg: Option<&str>) -> Result<PathBuf, io::Error> {
    let rel = arg.unwrap_or(DEFAULT_SOURCE);
    let provided = PathBuf::from(rel);
    if provided.is_absolute() {
        return Ok(provided);
    }

    let cwd_path = std::env::current_dir()?.join(&provided);
    if cwd_path.exists() {
        return Ok(cwd_path);
    }

    Ok(Path::new(env!("CARGO_MANIFEST_DIR")).join(provided))
}

fn register_functions(vm: &mut Vm, functions: &[FunctionDecl]) -> Result<(), io::Error> {
    for decl in functions {
        match decl.name.as_str() {
            "print" => vm.bind_function("print", Box::new(PrintFunction)),
            "add_one" => vm.bind_function("add_one", Box::new(AddOneFunction)),
            "echo" => vm.bind_function("echo", Box::new(EchoFunction)),
            name if name.starts_with("http::") => {
                return Err(io::Error::other(format!(
                    "host function '{name}' requires pd-edge runtime context",
                )));
            }
            other => {
                return Err(io::Error::other(format!(
                    "no host binding for function '{other}'",
                )));
            }
        }
    }
    Ok(())
}

fn print_usage() {
    println!("Usage:");
    println!("  pd-vm-run                  (defaults to REPL)");
    println!("  pd-vm-run [source_path]");
    println!("  pd-vm-run --repl");
    println!("  pd-vm-run repl");
    println!("  pd-vm-run --emit-vmbc <output.vmbc> [source_path]");
    println!("  pd-vm-run --disasm-vmbc <input.vmbc> [--show-source]");
    println!("  pd-vm-run --record <output.pdr> [source_path]");
    println!("  pd-vm-run --view-record <input.pdr>");
    println!("  pd-vm-run --debug [--stop-on-entry|--no-stop-on-entry] [source_path]");
    println!("  pd-vm-run --debug --tcp <addr> [source_path]");
    println!(
        "  pd-vm-run [--jit-hot-loop <n>] [--jit-dump] [--emit-vmbc <output.vmbc>] [source_path]"
    );
    println!("  pd-vm-run debug [--tcp <addr>] [source_path]");
}

fn run_repl() -> Result<(), Box<dyn std::error::Error>> {
    println!("pd-vm REPL (RustScript)");
    println!("history: up/down arrows, commands: .help, .quit");
    println!("state: locals persist across entries");
    let mut editor = DefaultEditor::new()?;
    let mut session = ReplSession::default();
    loop {
        match editor.readline("pd-vm> ") {
            Ok(line) => {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                if let Some(action) = handle_repl_command(line) {
                    if action == ReplAction::Break {
                        break;
                    }
                    continue;
                }
                let _ = editor.add_history_entry(line);
                let compiled = match compile_repl_snippet(line, &session.locals) {
                    Ok(compiled) => compiled,
                    Err(err) => {
                        println!("{err}");
                        continue;
                    }
                };
                let mut vm = Vm::with_locals(compiled.program, compiled.locals);
                if let Err(err) = register_functions(&mut vm, &compiled.functions) {
                    println!("{err}");
                    continue;
                }
                loop {
                    match vm.run() {
                        Ok(VmStatus::Halted) => {
                            sync_repl_session(&vm, &mut session);
                            if let Some(value) = vm.stack().last() {
                                println!("=> {}", format_value(value));
                            } else {
                                println!("=> <empty>");
                            }
                            break;
                        }
                        Ok(VmStatus::Yielded) => continue,
                        Err(err) => {
                            sync_repl_session(&vm, &mut session);
                            println!("runtime error: {err}");
                            break;
                        }
                    }
                }
            }
            Err(ReadlineError::Interrupted) | Err(ReadlineError::Eof) => {
                println!("bye");
                break;
            }
            Err(err) => {
                return Err(Box::new(io::Error::other(err.to_string())));
            }
        }
    }
    Ok(())
}

#[derive(Default)]
struct ReplSession {
    locals: BTreeMap<String, Value>,
}

fn sync_repl_session(vm: &Vm, session: &mut ReplSession) {
    let Some(debug) = vm.debug_info() else {
        return;
    };
    let mut next = BTreeMap::new();
    for local in &debug.locals {
        let Some(value) = vm.locals().get(local.index as usize) else {
            continue;
        };
        if is_repl_serializable_value(value) {
            next.insert(local.name.clone(), value.clone());
        }
    }
    session.locals = next;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReplAction {
    Continue,
    Break,
}

fn handle_repl_command(line: &str) -> Option<ReplAction> {
    match line {
        ".quit" | ".exit" => Some(ReplAction::Break),
        ".help" => {
            println!("commands:");
            println!("  .help      show commands");
            println!("  .quit      quit repl");
            println!("  .exit      quit repl");
            Some(ReplAction::Continue)
        }
        _ if line.starts_with('.') => {
            println!("unknown command: {line}");
            Some(ReplAction::Continue)
        }
        _ => None,
    }
}

fn compile_repl_snippet(
    input: &str,
    locals: &BTreeMap<String, Value>,
) -> Result<vm::CompiledProgram, vm::SourceError> {
    let trimmed = input.trim_end();
    let source = build_repl_source(trimmed, locals);
    match compile_source(&source) {
        Ok(compiled) => Ok(compiled),
        Err(first_err) => {
            if trimmed.ends_with(';') {
                return Err(remap_repl_source_error(first_err, locals.len()));
            }
            let fallback = format!("{trimmed};");
            let fallback_source = build_repl_source(&fallback, locals);
            compile_source(&fallback_source)
                .map_err(|_| remap_repl_source_error(first_err, locals.len()))
        }
    }
}

fn build_repl_source(input: &str, locals: &BTreeMap<String, Value>) -> String {
    if locals.is_empty() {
        return input.to_string();
    }

    let mut source = String::new();
    for (name, value) in locals {
        if let Some(literal) = render_repl_value_literal(value) {
            source.push_str("let ");
            source.push_str(name);
            source.push_str(" = ");
            source.push_str(&literal);
            source.push_str(";\n");
        }
    }
    source.push_str(input);
    source
}

fn remap_repl_source_error(error: vm::SourceError, prelude_lines: usize) -> vm::SourceError {
    match error {
        vm::SourceError::Parse(mut parse) => {
            if prelude_lines > 0 {
                parse.line = parse.line.saturating_sub(prelude_lines).max(1);
            }
            vm::SourceError::Parse(parse)
        }
        other => other,
    }
}

fn render_repl_value_literal(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::Int(number) => {
            if *number == i64::MIN {
                None
            } else {
                Some(number.to_string())
            }
        }
        Value::Float(_) => None,
        Value::Bool(flag) => Some(flag.to_string()),
        Value::String(text) => Some(format!("\"{}\"", escape_repl_string_literal(text))),
        Value::Array(items) => {
            let mut rendered = Vec::with_capacity(items.len());
            for item in items {
                rendered.push(render_repl_value_literal(item)?);
            }
            Some(format!("[{}]", rendered.join(", ")))
        }
        Value::Map(entries) => {
            let mut rendered = Vec::with_capacity(entries.len());
            for (key, item) in entries {
                let rendered_key = render_repl_map_key_literal(key)?;
                let rendered_value = render_repl_value_literal(item)?;
                rendered.push(format!("{rendered_key}: {rendered_value}"));
            }
            Some(format!("{{{}}}", rendered.join(", ")))
        }
    }
}

fn render_repl_map_key_literal(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::Int(number) => {
            if *number == i64::MIN {
                None
            } else {
                Some(number.to_string())
            }
        }
        Value::Bool(flag) => Some(flag.to_string()),
        Value::String(text) => Some(format!("\"{}\"", escape_repl_string_literal(text))),
        Value::Float(_) | Value::Array(_) | Value::Map(_) => None,
    }
}

fn escape_repl_string_literal(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            '\0' => escaped.push_str("\\0"),
            other => escaped.push(other),
        }
    }
    escaped
}

fn is_repl_serializable_value(value: &Value) -> bool {
    render_repl_value_literal(value).is_some()
}

struct PrintFunction;

impl HostFunction for PrintFunction {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        let rendered = args.iter().map(format_value).collect::<Vec<_>>().join(" ");
        println!("{rendered}");
        Ok(CallOutcome::Return(args.to_vec()))
    }
}

struct AddOneFunction;

impl HostFunction for AddOneFunction {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        let value = match args.first() {
            Some(Value::Int(value)) => *value,
            _ => return Err(VmError::TypeMismatch("int")),
        };
        Ok(CallOutcome::Return(vec![Value::Int(value + 1)]))
    }
}

struct EchoFunction;

impl HostFunction for EchoFunction {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        let value = args.first().cloned().ok_or(VmError::StackUnderflow)?;
        Ok(CallOutcome::Return(vec![value]))
    }
}

fn format_value(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Int(value) => value.to_string(),
        Value::Float(value) => value.to_string(),
        Value::Bool(value) => value.to_string(),
        Value::String(value) => value.clone(),
        Value::Array(values) => {
            let parts = values
                .iter()
                .map(format_value)
                .collect::<Vec<_>>()
                .join(", ");
            format!("[{parts}]")
        }
        Value::Map(entries) => {
            let parts = entries
                .iter()
                .map(|(key, value)| format!("{}: {}", format_value(key), format_value(value)))
                .collect::<Vec<_>>()
                .join(", ");
            format!("{{{parts}}}")
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::parse_cli_args;
    use vm::Value;

    fn s(value: &str) -> String {
        value.to_string()
    }

    #[test]
    fn parse_cli_defaults() {
        let cfg = parse_cli_args(&[]).expect("parse should succeed");
        assert!(cfg.repl);
        assert!(!cfg.debug);
        assert!(cfg.tcp_addr.is_none());
        assert!(cfg.stop_on_entry);
        assert!(!cfg.jit_dump);
        assert!(cfg.jit_hot_loop_threshold.is_none());
        assert!(cfg.source.is_none());
        assert!(cfg.emit_vmbc_path.is_none());
        assert!(cfg.disasm_vmbc_path.is_none());
        assert!(!cfg.show_source);
    }

    #[test]
    fn parse_cli_debug_with_source_and_tcp() {
        let cfg = parse_cli_args(&[
            s("--debug"),
            s("--tcp"),
            s("127.0.0.1:9002"),
            s("examples/example.lua"),
        ])
        .expect("parse should succeed");
        assert!(cfg.debug);
        assert_eq!(cfg.tcp_addr.as_deref(), Some("127.0.0.1:9002"));
        assert_eq!(cfg.source.as_deref(), Some("examples/example.lua"));
    }

    #[test]
    fn parse_cli_legacy_debug_command() {
        let cfg =
            parse_cli_args(&[s("debug"), s("examples/example.rss")]).expect("parse should succeed");
        assert!(cfg.debug);
        assert_eq!(cfg.source.as_deref(), Some("examples/example.rss"));
    }

    #[test]
    fn parse_cli_rejects_multiple_sources() {
        let err = parse_cli_args(&[s("a.rss"), s("b.rss")]).expect_err("parse should fail");
        assert!(err.contains("multiple source paths"));
    }

    #[test]
    fn parse_cli_jit_flags() {
        let cfg = parse_cli_args(&[
            s("--jit-hot-loop"),
            s("2"),
            s("--jit-dump"),
            s("examples/example.rss"),
        ])
        .expect("parse should succeed");
        assert_eq!(cfg.jit_hot_loop_threshold, Some(2));
        assert!(cfg.jit_dump);
        assert_eq!(cfg.source.as_deref(), Some("examples/example.rss"));
    }

    #[test]
    fn parse_cli_emit_vmbc_path() {
        let cfg = parse_cli_args(&[
            s("--emit-vmbc"),
            s("out/program.vmbc"),
            s("examples/example.rss"),
        ])
        .expect("parse should succeed");
        assert_eq!(cfg.emit_vmbc_path.as_deref(), Some("out/program.vmbc"));
        assert_eq!(cfg.source.as_deref(), Some("examples/example.rss"));
    }

    #[test]
    fn parse_cli_emit_vmbc_requires_path() {
        let err = parse_cli_args(&[s("--emit-vmbc")]).expect_err("parse should fail");
        assert!(err.contains("missing value for --emit-vmbc"));
    }

    #[test]
    fn parse_cli_disasm_vmbc_path() {
        let cfg = parse_cli_args(&[
            s("--disasm-vmbc"),
            s("out/program.vmbc"),
            s("--show-source"),
        ])
        .expect("parse should succeed");
        assert_eq!(cfg.disasm_vmbc_path.as_deref(), Some("out/program.vmbc"));
        assert!(cfg.show_source);
    }

    #[test]
    fn parse_cli_disasm_requires_path() {
        let err = parse_cli_args(&[s("--disasm-vmbc")]).expect_err("parse should fail");
        assert!(err.contains("missing value for --disasm-vmbc"));
    }

    #[test]
    fn parse_cli_show_source_requires_disasm() {
        let err = parse_cli_args(&[s("--show-source")]).expect_err("parse should fail");
        assert!(err.contains("requires --disasm-vmbc"));
    }

    #[test]
    fn parse_cli_disasm_rejects_source_path() {
        let err = parse_cli_args(&[
            s("--disasm-vmbc"),
            s("program.vmbc"),
            s("examples/example.rss"),
        ])
        .expect_err("parse should fail");
        assert!(err.contains("does not accept a source path"));
    }

    #[test]
    fn parse_cli_record_path() {
        let cfg = parse_cli_args(&[s("--record"), s("out/run.pdr"), s("examples/example.rss")])
            .expect("parse should succeed");
        assert_eq!(cfg.record_path.as_deref(), Some("out/run.pdr"));
        assert_eq!(cfg.source.as_deref(), Some("examples/example.rss"));
    }

    #[test]
    fn parse_cli_view_record_path() {
        let cfg =
            parse_cli_args(&[s("--view-record"), s("out/run.pdr")]).expect("parse should succeed");
        assert_eq!(cfg.view_recording_path.as_deref(), Some("out/run.pdr"));
        assert!(cfg.source.is_none());
    }

    #[test]
    fn parse_cli_record_rejects_debug() {
        let err = parse_cli_args(&[s("--record"), s("run.pdr"), s("--debug")])
            .expect_err("parse should fail");
        assert!(err.contains("record mode"));
    }

    #[test]
    fn parse_cli_repl_flag() {
        let cfg = parse_cli_args(&[s("--repl")]).expect("parse should succeed");
        assert!(cfg.repl);
    }

    #[test]
    fn parse_cli_repl_legacy_command() {
        let cfg = parse_cli_args(&[s("repl")]).expect("parse should succeed");
        assert!(cfg.repl);
    }

    #[test]
    fn parse_cli_repl_rejects_source_path() {
        let err = parse_cli_args(&[s("--repl"), s("examples/example.rss")])
            .expect_err("parse should fail");
        assert!(err.contains("does not accept a source path"));
    }

    #[test]
    fn parse_cli_repl_rejects_emit_vmbc() {
        let err = parse_cli_args(&[s("--repl"), s("--emit-vmbc"), s("out.vmbc")])
            .expect_err("parse should fail");
        assert!(err.contains("cannot be combined"));
    }

    #[test]
    fn repl_compile_falls_back_to_expression_semicolon() {
        let compiled =
            super::compile_repl_snippet("1 + 2", &BTreeMap::new()).expect("compile should succeed");
        assert_eq!(compiled.locals, 0);
    }

    #[test]
    fn repl_compile_uses_persisted_locals() {
        let mut locals = BTreeMap::new();
        locals.insert("x".to_string(), Value::Int(41));
        let compiled =
            super::compile_repl_snippet("x + 1", &locals).expect("compile should succeed");
        assert!(compiled.locals >= 1);
    }

    #[test]
    fn repl_compile_remaps_parse_error_line_numbers() {
        let mut locals = BTreeMap::new();
        locals.insert("x".to_string(), Value::Int(1));
        match super::compile_repl_snippet("let y = ;", &locals) {
            Err(vm::SourceError::Parse(parse)) => assert_eq!(parse.line, 1),
            Err(other) => panic!("expected parse error, got {other}"),
            Ok(_) => panic!("expected parse error, got successful compile"),
        }
    }
}
