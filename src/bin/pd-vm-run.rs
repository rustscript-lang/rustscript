use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;
use vm::{
    CallOutcome, CallReturn, Debugger, DisassembleOptions, JitConfig, ReplLocalBinding,
    SourceFlavor, SourceMap, SourcePathError, Value, Vm, VmError, VmRecording, VmStatus,
    compile_source_file, compile_source_for_repl_with_locals, disassemble_vmbc_with_options,
    encode_program, format_source_with_flavor, render_source_error, render_vm_error,
    replay_recording_stdio,
};
use vm::{HostFunctionRegistry, HostImport};

const DEFAULT_SOURCE: &str = "examples/example.rss";

#[derive(Debug, Clone, PartialEq, Eq)]
struct CliConfig {
    source: Option<String>,
    emit_vmbc_path: Option<String>,
    epoch_check_interval: Option<u32>,
    disasm_vmbc_path: Option<String>,
    record_path: Option<String>,
    view_recording_path: Option<String>,
    show_source: bool,
    fmt: bool,
    fmt_check: bool,
    repl: bool,
    debug: bool,
    tcp_addr: Option<String>,
    stop_on_entry: bool,
    aot: bool,
    aot_dump: bool,
    aot_save_path: Option<String>,
    aot_load_path: Option<String>,
    jit_dump: bool,
    jit_dump_show_machine_code: bool,
    jit_hot_loop_threshold: Option<u32>,
    fuel: Option<u64>,
    epoch_deadline: Option<u64>,
    help: bool,
    version: bool,
}

impl Default for CliConfig {
    fn default() -> Self {
        Self {
            source: None,
            emit_vmbc_path: None,
            epoch_check_interval: None,
            disasm_vmbc_path: None,
            record_path: None,
            view_recording_path: None,
            show_source: false,
            fmt: false,
            fmt_check: false,
            repl: false,
            debug: false,
            tcp_addr: None,
            stop_on_entry: true,
            aot: false,
            aot_dump: false,
            aot_save_path: None,
            aot_load_path: None,
            jit_dump: false,
            jit_dump_show_machine_code: true,
            jit_hot_loop_threshold: None,
            fuel: None,
            epoch_deadline: None,
            help: false,
            version: false,
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if let Err(err) = run_main() {
        eprintln!("{err}");
        std::process::exit(1);
    }
    Ok(())
}

fn run_main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cli = parse_cli_args(&args).map_err(io::Error::other)?;
    if cli.version {
        println!("{}", binary_version_text());
        return Ok(());
    }
    if cli.help {
        print_usage();
        return Ok(());
    }
    if cli.fmt {
        return run_fmt(&cli);
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

    if let Some(mut vm) = try_new_cli_vm_from_standalone_aot(&cli)? {
        if let Some(output_path) = cli.emit_vmbc_path.as_ref() {
            let encoded = encode_program(vm.program())?;
            std::fs::write(output_path, &encoded)?;
            println!("wrote {} bytes to {}", encoded.len(), output_path);
            return Ok(());
        }

        apply_runtime_flags(&mut vm, &cli)?;
        run_vm_loop(&mut vm, None, cli.fuel)?;
        if cli.aot_dump {
            println!("{}", vm.dump_aot_info());
        }
        if cli.jit_dump {
            println!(
                "{}",
                vm.dump_jit_info_with_machine_code(cli.jit_dump_show_machine_code)
            );
        }
        return Ok(());
    }

    let source_path = resolve_source_path(cli.source.as_deref())?;
    let compiled = compile_source_file(&source_path)
        .map_err(|err| io::Error::other(render_source_path_error(&source_path, &err)))?;
    if let Some(output_path) = cli.emit_vmbc_path.as_ref() {
        let encoded = encode_program(&compiled.program)?;
        std::fs::write(output_path, &encoded)?;
        println!("wrote {} bytes to {}", encoded.len(), output_path);
        return Ok(());
    }
    let recording_program = cli.record_path.as_ref().map(|_| compiled.program.clone());
    let mut vm = new_cli_vm(compiled.program.with_local_count(compiled.locals), &cli);
    apply_runtime_flags(&mut vm, &cli)?;
    let imports = vm.program().imports.clone();
    register_imports(&mut vm, &imports)?;
    prepare_aot_for_cli(&mut vm, &cli)?;

    if let Some(record_path) = cli.record_path.as_ref() {
        let program = recording_program.expect("recording mode should clone program");
        let mut debugger = Debugger::with_recording(program);
        run_vm_loop(&mut vm, Some(&mut debugger), cli.fuel)?;
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

    run_vm_loop(&mut vm, debugger.as_mut(), cli.fuel)?;
    if cli.aot_dump {
        println!("{}", vm.dump_aot_info());
    }
    if cli.jit_dump {
        println!(
            "{}",
            vm.dump_jit_info_with_machine_code(cli.jit_dump_show_machine_code)
        );
    }
    Ok(())
}

fn try_new_cli_vm_from_standalone_aot(cli: &CliConfig) -> Result<Option<Vm>, io::Error> {
    let Some(path) = cli.aot_load_path.as_deref() else {
        return Ok(None);
    };
    if cli.source.is_some() {
        return Ok(None);
    }

    let mut vm = Vm::new_from_aot_artifact_file_with_jit_config(path, cli_jit_config(cli))
        .map_err(io::Error::other)?;
    configure_cli_vm(&mut vm);
    let imports = vm.program().imports.clone();
    register_imports(&mut vm, &imports)?;

    if let Some(save_path) = cli.aot_save_path.as_deref() {
        vm.save_aot_artifact_to_file(save_path)
            .map_err(io::Error::other)?;
    }

    Ok(Some(vm))
}

fn apply_runtime_flags(vm: &mut Vm, cli: &CliConfig) -> Result<(), io::Error> {
    vm.set_jit_native_bridge_stats_enabled(cli.jit_dump);
    if let Some(interval) = cli.epoch_check_interval {
        vm.set_epoch_check_interval(interval)
            .map_err(|err| io::Error::other(render_vm_error(vm, &err)))?;
    }
    if let Some(fuel) = cli.fuel {
        vm.set_fuel(fuel);
    }
    if let Some(deadline) = cli.epoch_deadline {
        vm.set_epoch_deadline(deadline)
            .map_err(|err| io::Error::other(render_vm_error(vm, &err)))?;
    }
    Ok(())
}

fn prepare_aot_for_cli(vm: &mut Vm, cli: &CliConfig) -> Result<(), io::Error> {
    if let Some(path) = cli.aot_load_path.as_deref() {
        vm.load_aot_artifact_from_file(path)
            .map_err(io::Error::other)?;
    } else if cli.aot || cli.aot_save_path.is_some() {
        vm.compile_aot()
            .map_err(|err| io::Error::other(render_vm_error(vm, &err)))?;
    }

    if let Some(path) = cli.aot_save_path.as_deref() {
        vm.save_aot_artifact_to_file(path)
            .map_err(io::Error::other)?;
    }
    Ok(())
}

fn run_vm_loop(
    vm: &mut Vm,
    mut debugger: Option<&mut Debugger>,
    fuel_recharge: Option<u64>,
) -> Result<(), io::Error> {
    loop {
        let status = if let Some(active_debugger) = debugger.as_deref_mut() {
            vm.run_with_debugger(active_debugger)
                .map_err(|err| io::Error::other(render_vm_error(vm, &err)))?
        } else {
            vm.run()
                .map_err(|err| io::Error::other(render_vm_error(vm, &err)))?
        };
        match status {
            VmStatus::Halted => {
                println!("vm halted");
                println!("stack: {:?}", vm.stack());
                return Ok(());
            }
            VmStatus::Yielded => match vm.last_yield_reason() {
                Some(vm::VmYieldReason::Fuel)
                    if fuel_recharge.is_some() && vm.get_fuel() == Some(0) =>
                {
                    let recharge = fuel_recharge.unwrap_or(0);
                    if recharge > 0 {
                        vm.recharge_fuel(recharge)
                            .map_err(|err| io::Error::other(render_vm_error(vm, &err)))?;
                        println!("vm yielded, recharged {recharge} fuel, resuming...");
                    } else {
                        println!("vm yielded, resuming...");
                    }
                }
                Some(vm::VmYieldReason::Epoch) => {
                    let deadline = vm
                        .epoch_deadline()
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "disabled".to_string());
                    println!(
                        "vm yielded at epoch deadline (current={}, deadline={deadline})",
                        vm.current_epoch()
                    );
                    return Ok(());
                }
                _ => {
                    println!("vm yielded, resuming...");
                }
            },
            VmStatus::Waiting(_op_id) => {
                vm.wait_for_host_op_blocking()
                    .map_err(|err| io::Error::other(render_vm_error(vm, &err)))?;
            }
        }
    }
}

fn render_source_path_error(source_path: &Path, err: &SourcePathError) -> String {
    match err {
        SourcePathError::Source(vm::SourceError::Parse(parse)) => {
            let source = std::fs::read_to_string(source_path).unwrap_or_default();
            let mut source_map = SourceMap::new();
            let source_id = source_map.add_source(source_path.display().to_string(), source);
            let parse = parse
                .clone()
                .with_line_span_from_source(&source_map, source_id);
            render_source_error(&source_map, &parse, true)
        }
        SourcePathError::Source(vm::SourceError::Compile(compile)) => {
            let render_path = compile
                .source_name()
                .map(Path::new)
                .filter(|path| path.exists())
                .unwrap_or(source_path);
            let source = std::fs::read_to_string(render_path).unwrap_or_default();
            let mut source_map = SourceMap::new();
            source_map.add_source(render_path.display().to_string(), source);
            vm::render_compile_error(&source_map, compile, true)
        }
        SourcePathError::InvalidImportSyntax {
            path,
            line,
            message,
        } => {
            let source = std::fs::read_to_string(path).unwrap_or_default();
            let mut source_map = SourceMap::new();
            let source_id = source_map.add_source(path.display().to_string(), source);
            let parse = vm::ParseError::at_line(*line, message.clone())
                .with_line_span_from_source(&source_map, source_id);
            render_source_error(&source_map, &parse, true)
        }
        _ => err.to_string(),
    }
}

fn render_format_path_error(source_path: &Path, source: &str, err: &vm::FormatError) -> String {
    match err {
        vm::FormatError::Parse(parse) => {
            let mut source_map = SourceMap::new();
            let source_id =
                source_map.add_source(source_path.display().to_string(), source.to_string());
            let parse = parse
                .clone()
                .with_line_span_from_source(&source_map, source_id);
            render_source_error(&source_map, &parse, true)
        }
        vm::FormatError::UnsupportedFlavor(_) => err.to_string(),
    }
}

fn run_fmt(cli: &CliConfig) -> Result<(), Box<dyn std::error::Error>> {
    let source_arg = cli
        .source
        .as_deref()
        .ok_or_else(|| io::Error::other("fmt mode requires a source path"))?;
    let source_path = resolve_source_path(Some(source_arg))?;
    let flavor = source_flavor_from_path(&source_path)?;
    let source = std::fs::read_to_string(&source_path)?;
    let formatted = format_source_with_flavor(&source, flavor)
        .map_err(|err| io::Error::other(render_format_path_error(&source_path, &source, &err)))?;

    if cli.fmt_check {
        if formatted == source {
            return Ok(());
        }
        return Err(Box::new(io::Error::other(format!(
            "would reformat {}",
            source_path.display()
        ))));
    }

    if formatted == source {
        println!("already formatted {}", source_path.display());
        return Ok(());
    }

    std::fs::write(&source_path, formatted)?;
    println!("formatted {}", source_path.display());
    Ok(())
}

fn parse_cli_args(args: &[String]) -> Result<CliConfig, String> {
    let mut cfg = CliConfig::default();
    if args.is_empty() {
        cfg.repl = true;
        return Ok(cfg);
    }
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "-V" | "--version"))
    {
        cfg.version = true;
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
    } else if let Some(first) = args.first()
        && first == "fmt"
    {
        cfg.fmt = true;
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
            "--aot" => {
                cfg.aot = true;
                index += 1;
            }
            "--aot-dump" => {
                cfg.aot_dump = true;
                index += 1;
            }
            "--aot-save" => {
                let path = args
                    .get(index + 1)
                    .ok_or_else(|| "missing value for --aot-save".to_string())?;
                cfg.aot_save_path = Some(path.clone());
                index += 2;
            }
            "--aot-load" => {
                let path = args
                    .get(index + 1)
                    .ok_or_else(|| "missing value for --aot-load".to_string())?;
                cfg.aot_load_path = Some(path.clone());
                index += 2;
            }
            "--jit-dump" | "--dump-jit" => {
                cfg.jit_dump = true;
                index += 1;
            }
            "--jit-dump-no-code" => {
                cfg.jit_dump_show_machine_code = false;
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
            "--fuel" => {
                let raw = args
                    .get(index + 1)
                    .ok_or_else(|| "missing value for --fuel".to_string())?;
                let value = raw
                    .parse::<u64>()
                    .map_err(|_| format!("invalid --fuel value '{raw}'"))?;
                cfg.fuel = Some(value);
                index += 2;
            }
            "--epoch-deadline" => {
                let raw = args
                    .get(index + 1)
                    .ok_or_else(|| "missing value for --epoch-deadline".to_string())?;
                let value = raw
                    .parse::<u64>()
                    .map_err(|_| format!("invalid --epoch-deadline value '{raw}'"))?;
                cfg.epoch_deadline = Some(value);
                index += 2;
            }
            "--emit-vmbc" => {
                let path = args
                    .get(index + 1)
                    .ok_or_else(|| "missing value for --emit-vmbc".to_string())?;
                cfg.emit_vmbc_path = Some(path.clone());
                index += 2;
            }
            "--epoch-check-interval" => {
                let raw = args
                    .get(index + 1)
                    .ok_or_else(|| "missing value for --epoch-check-interval".to_string())?;
                cfg.epoch_check_interval = Some(parse_cli_u32_flag("--epoch-check-interval", raw)?);
                index += 2;
            }
            "--disasm-vmbc" => {
                let path = args
                    .get(index + 1)
                    .ok_or_else(|| "missing value for --disasm-vmbc".to_string())?;
                cfg.disasm_vmbc_path = Some(path.clone());
                index += 2;
            }
            value if value.starts_with("--epoch-check-interval=") => {
                let raw = value.trim_start_matches("--epoch-check-interval=");
                cfg.epoch_check_interval = Some(parse_cli_u32_flag("--epoch-check-interval", raw)?);
                index += 1;
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
            "--check" => {
                cfg.fmt_check = true;
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

    if !cfg.jit_dump_show_machine_code && !cfg.jit_dump {
        return Err("--jit-dump-no-code requires --jit-dump or --dump-jit".to_string());
    }
    if cfg.fmt_check && !cfg.fmt {
        return Err("--check requires fmt mode".to_string());
    }
    if cfg.fuel.is_some() && cfg.epoch_deadline.is_some() {
        return Err("--fuel and --epoch-deadline are mutually exclusive".to_string());
    }
    if cfg.fuel.is_some() && cfg.epoch_check_interval.is_some() {
        return Err("--fuel cannot be combined with --epoch-check-interval".to_string());
    }
    if cfg.aot && cfg.aot_load_path.is_some() {
        return Err("--aot and --aot-load are mutually exclusive".to_string());
    }

    if cfg.repl {
        if cfg.source.is_some() {
            return Err("repl mode does not accept a source path".to_string());
        }
        if cfg.debug
            || cfg.aot
            || cfg.aot_dump
            || cfg.aot_save_path.is_some()
            || cfg.aot_load_path.is_some()
            || cfg.tcp_addr.is_some()
            || cfg.jit_dump
            || cfg.jit_hot_loop_threshold.is_some()
            || cfg.fuel.is_some()
            || cfg.epoch_deadline.is_some()
            || cfg.epoch_check_interval.is_some()
            || cfg.emit_vmbc_path.is_some()
            || cfg.disasm_vmbc_path.is_some()
            || cfg.record_path.is_some()
            || cfg.view_recording_path.is_some()
        {
            return Err(
                "repl mode cannot be combined with debug/aot/jit/fuel/epoch/emit/disasm runtime flags"
                    .to_string(),
            );
        }
    }
    if cfg.disasm_vmbc_path.is_some() {
        if cfg.source.is_some() {
            return Err("disasm mode does not accept a source path".to_string());
        }
        if cfg.repl
            || cfg.debug
            || cfg.aot
            || cfg.aot_dump
            || cfg.aot_save_path.is_some()
            || cfg.aot_load_path.is_some()
            || cfg.tcp_addr.is_some()
            || cfg.jit_dump
            || cfg.jit_hot_loop_threshold.is_some()
            || cfg.fuel.is_some()
            || cfg.epoch_deadline.is_some()
            || cfg.epoch_check_interval.is_some()
            || cfg.emit_vmbc_path.is_some()
            || cfg.record_path.is_some()
            || cfg.view_recording_path.is_some()
        {
            return Err(
                "disasm mode cannot be combined with repl/debug/aot/jit/fuel/epoch/emit runtime flags"
                    .to_string(),
            );
        }
    } else if cfg.show_source {
        return Err("--show-source requires --disasm-vmbc".to_string());
    }

    if cfg.fmt {
        if cfg.source.is_none() && !cfg.help {
            return Err("fmt mode requires a source path".to_string());
        }
        if cfg.repl
            || cfg.debug
            || cfg.aot
            || cfg.aot_dump
            || cfg.aot_save_path.is_some()
            || cfg.aot_load_path.is_some()
            || cfg.tcp_addr.is_some()
            || cfg.jit_dump
            || cfg.jit_hot_loop_threshold.is_some()
            || cfg.fuel.is_some()
            || cfg.epoch_deadline.is_some()
            || cfg.epoch_check_interval.is_some()
            || cfg.emit_vmbc_path.is_some()
            || cfg.disasm_vmbc_path.is_some()
            || cfg.record_path.is_some()
            || cfg.view_recording_path.is_some()
            || cfg.show_source
        {
            return Err(
                "fmt mode cannot be combined with repl/debug/aot/jit/fuel/epoch/emit/disasm/record flags"
                    .to_string(),
            );
        }
    }

    if cfg.debug
        && (cfg.aot || cfg.aot_dump || cfg.aot_save_path.is_some() || cfg.aot_load_path.is_some())
    {
        return Err("debug mode cannot be combined with aot runtime flags".to_string());
    }

    if cfg.epoch_check_interval.is_some() && cfg.epoch_deadline.is_none() && !cfg.debug {
        return Err("--epoch-check-interval requires --epoch-deadline or --debug".to_string());
    }
    if cfg.record_path.is_some()
        && (cfg.debug
            || cfg.aot
            || cfg.aot_dump
            || cfg.aot_save_path.is_some()
            || cfg.aot_load_path.is_some()
            || cfg.tcp_addr.is_some()
            || cfg.jit_dump
            || cfg.jit_hot_loop_threshold.is_some()
            || cfg.emit_vmbc_path.is_some()
            || cfg.disasm_vmbc_path.is_some()
            || cfg.view_recording_path.is_some()
            || cfg.show_source)
    {
        return Err(
            "record mode cannot be combined with debug/aot/jit/emit/disasm/view-record flags"
                .to_string(),
        );
    }
    if cfg.view_recording_path.is_some()
        && (cfg.source.is_some()
            || cfg.debug
            || cfg.aot
            || cfg.aot_dump
            || cfg.aot_save_path.is_some()
            || cfg.aot_load_path.is_some()
            || cfg.tcp_addr.is_some()
            || cfg.jit_dump
            || cfg.jit_hot_loop_threshold.is_some()
            || cfg.fuel.is_some()
            || cfg.epoch_deadline.is_some()
            || cfg.epoch_check_interval.is_some()
            || cfg.emit_vmbc_path.is_some()
            || cfg.disasm_vmbc_path.is_some()
            || cfg.record_path.is_some()
            || cfg.show_source)
    {
        return Err(
            "view-record mode cannot be combined with source/debug/aot/jit/fuel/epoch/emit/disasm flags"
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

fn source_flavor_from_path(path: &Path) -> Result<SourceFlavor, io::Error> {
    let ext = path
        .extension()
        .and_then(|value| value.to_str())
        .ok_or_else(|| io::Error::other(SourcePathError::MissingExtension))?;
    SourceFlavor::from_extension(ext)
        .ok_or_else(|| io::Error::other(SourcePathError::UnsupportedExtension(ext.to_string())))
}

fn parse_cli_u32_flag(flag: &str, raw: &str) -> Result<u32, String> {
    raw.parse::<u32>()
        .map_err(|_| format!("invalid {flag} value '{raw}'"))
}

fn register_imports(vm: &mut Vm, imports: &[HostImport]) -> Result<(), io::Error> {
    for import in imports {
        if import.name.starts_with("http::") {
            return Err(io::Error::other(format!(
                "host function '{}' requires pd-edge runtime context",
                import.name,
            )));
        }
    }
    if imports.is_empty() {
        return Ok(());
    }
    let plan = cli_host_registry()
        .prepare_shared_plan(imports)
        .map_err(|err| io::Error::other(render_vm_error(vm, &err)))?;
    cli_host_registry()
        .bind_vm_with_plan(vm, &plan)
        .map_err(|err| io::Error::other(render_vm_error(vm, &err)))?;
    Ok(())
}

fn new_cli_vm(program: vm::Program, cli: &CliConfig) -> Vm {
    let mut vm = Vm::new_with_jit_config(program, cli_jit_config(cli));
    configure_cli_vm(&mut vm);
    vm
}

fn cli_jit_config(cli: &CliConfig) -> JitConfig {
    let mut jit_config = JitConfig::default();
    if let Some(hot_loop_threshold) = cli.jit_hot_loop_threshold {
        jit_config.hot_loop_threshold = hot_loop_threshold;
    }
    jit_config
}

fn configure_cli_vm(vm: &mut Vm) {
    vm.set_runtime_print_sink(|rendered| {
        print!("{rendered}");
    });
}

fn cli_host_registry() -> &'static HostFunctionRegistry {
    static REGISTRY: OnceLock<HostFunctionRegistry> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        let mut registry = HostFunctionRegistry::new();
        registry.register_static("add_one", 1, add_one_host_function);
        registry.register_static("echo", 1, echo_host_function);
        registry
    })
}

fn print_usage() {
    println!("Usage:");
    println!("  pd-vm-run                  (defaults to REPL)");
    println!("  pd-vm-run --version");
    println!("  pd-vm-run [source_path]");
    println!("  pd-vm-run fmt [--check] <source_path>");
    println!("  pd-vm-run --repl");
    println!("  pd-vm-run repl");
    println!("  pd-vm-run --emit-vmbc <output.vmbc> [source_path]");
    println!("  pd-vm-run --disasm-vmbc <input.vmbc> [--show-source]");
    println!("  pd-vm-run --record <output.pdr> [source_path]");
    println!("  pd-vm-run --view-record <input.pdr>");
    println!("  pd-vm-run --debug [--stop-on-entry|--no-stop-on-entry] [source_path]");
    println!("  pd-vm-run --debug --tcp <addr> [source_path]");
    println!(
        "  pd-vm-run [--aot|--aot-load <artifact.pat>] [--aot-save <artifact.pat>] [--aot-dump] [source_path]"
    );
    println!(
        "  pd-vm-run [--jit-hot-loop <n>] [--jit-dump|--dump-jit] [--jit-dump-no-code] [--emit-vmbc <output.vmbc>] [source_path]"
    );
    println!(
        "  pd-vm-run [--fuel <n>|--epoch-deadline <n>] [--epoch-check-interval <n>] [source_path]"
    );
    println!("  pd-vm-run debug [--tcp <addr>] [source_path]");
    println!();
    println!("Options:");
    println!("  -V, --version              Show version with git metadata");
    println!("  -h, --help                 Show this help");
    println!("      --check                In fmt mode, fail if formatting would change the file");
}

fn binary_version_text() -> String {
    let binary = env!("CARGO_BIN_NAME");
    let git_tag = option_env!("PD_BUILD_GIT_TAG").unwrap_or("untagged");
    let git_commit = option_env!("PD_BUILD_GIT_COMMIT").unwrap_or("unknown");
    let git_dirty = option_env!("PD_BUILD_GIT_DIRTY").unwrap_or("false");
    let dirty = matches!(git_dirty, "true" | "1" | "yes" | "dirty");

    if dirty {
        format!("{binary} {git_tag} (dirty commit: {git_commit})")
    } else {
        format!("{binary} {git_tag}")
    }
}

fn run_repl() -> Result<(), Box<dyn std::error::Error>> {
    println!("pd-vm REPL (RustScript)");
    println!("history: up/down arrows, commands: .help, .quit, .cancel");
    println!("state: locals persist across entries");
    let mut editor = DefaultEditor::new()?;
    let mut session = ReplSession::default();
    let mut pending_input = String::new();
    loop {
        let prompt = if pending_input.is_empty() {
            "pd-vm> "
        } else {
            "...> "
        };
        match editor.readline(prompt) {
            Ok(line) => {
                let trimmed = line.trim();
                if pending_input.is_empty() {
                    if trimmed.is_empty() {
                        continue;
                    }
                    if let Some(action) = handle_repl_command(trimmed) {
                        if action == ReplAction::Break {
                            break;
                        }
                        continue;
                    }
                } else if trimmed == ".cancel" {
                    pending_input.clear();
                    println!("pending input cleared");
                    continue;
                }

                if !pending_input.is_empty() {
                    pending_input.push('\n');
                }
                pending_input.push_str(line.trim_end());
                if !is_repl_input_complete(&pending_input) {
                    continue;
                }

                let snippet = pending_input.trim().to_string();
                pending_input.clear();
                if snippet.is_empty() {
                    continue;
                }

                let _ = editor.add_history_entry(&snippet);
                let compiled = match compile_repl_snippet(&snippet, &session.locals) {
                    Ok(compiled) => compiled,
                    Err(err) => {
                        println!("{}", render_repl_compile_error(&snippet, &err));
                        continue;
                    }
                };
                let mut vm = Vm::new_with_jit_config(
                    compiled
                        .compiled
                        .program
                        .with_local_count(compiled.compiled.locals),
                    JitConfig::default(),
                );
                configure_cli_vm(&mut vm);
                let imports = vm.program().imports.clone();
                if let Err(err) = register_imports(&mut vm, &imports) {
                    println!("{err}");
                    continue;
                }
                if let Err(err) = seed_repl_vm_locals(&mut vm, &session.locals) {
                    println!("{}", render_vm_error(&vm, &err));
                    continue;
                }
                loop {
                    match vm.run() {
                        Ok(VmStatus::Halted) => {
                            sync_repl_session(&vm, &compiled.bindings, &mut session);
                            if let Some(value) = vm.stack().last() {
                                println!("=> {}", format_value(value));
                            } else {
                                println!("=> <empty>");
                            }
                            break;
                        }
                        Ok(VmStatus::Yielded) => continue,
                        Ok(VmStatus::Waiting(_op_id)) => {
                            if let Err(err) = vm.wait_for_host_op_blocking() {
                                sync_repl_session(&vm, &compiled.bindings, &mut session);
                                println!("{}", render_vm_error(&vm, &err));
                                break;
                            }
                            continue;
                        }
                        Err(err) => {
                            sync_repl_session(&vm, &compiled.bindings, &mut session);
                            println!("{}", render_vm_error(&vm, &err));
                            break;
                        }
                    }
                }
            }
            Err(ReadlineError::Interrupted) => {
                if pending_input.is_empty() {
                    println!("bye");
                    break;
                }
                pending_input.clear();
                println!("pending input cleared");
            }
            Err(ReadlineError::Eof) => {
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
    locals: BTreeMap<String, ReplSessionLocal>,
}

#[derive(Clone, Debug, PartialEq)]
struct ReplSessionLocal {
    value: Value,
    mutable: bool,
    schema: Option<vm::compiler::TypeSchema>,
    optional: bool,
}

fn sync_repl_session(vm: &Vm, bindings: &[ReplLocalBinding], session: &mut ReplSession) {
    if bindings.is_empty() {
        session.locals.clear();
        return;
    }
    let Some(debug) = vm.debug_info() else {
        session.locals.clear();
        return;
    };
    let mut next = BTreeMap::new();
    for binding in bindings {
        let Some(index) = debug.local_index(&binding.name) else {
            continue;
        };
        let Some(value) = vm.locals().get(index as usize) else {
            continue;
        };
        let (schema, optional) = repl_local_schema_from_vm(vm, index as usize, value);
        next.insert(
            binding.name.clone(),
            ReplSessionLocal {
                value: value.clone(),
                mutable: binding.mutable,
                schema,
                optional,
            },
        );
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
        ".cancel" => {
            println!("no pending input");
            Some(ReplAction::Continue)
        }
        ".help" => {
            println!("commands:");
            println!("  .help      show commands");
            println!("  .quit      quit repl");
            println!("  .exit      quit repl");
            println!("  .cancel    clear pending multiline input");
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
    locals: &BTreeMap<String, ReplSessionLocal>,
) -> Result<vm::CompiledReplProgram, vm::SourceError> {
    let trimmed = input.trim_end();
    let bindings = locals
        .iter()
        .map(|(name, local)| ReplLocalBinding {
            name: name.clone(),
            mutable: local.mutable,
            schema: local.schema.clone(),
            optional: local.optional,
        })
        .collect::<Vec<_>>();
    match compile_source_for_repl_with_locals(trimmed, &bindings) {
        Ok(compiled) => Ok(compiled),
        Err(first_err) => {
            if trimmed.ends_with(';') {
                return Err(first_err);
            }
            let fallback = format!("{trimmed};");
            compile_source_for_repl_with_locals(&fallback, &bindings).map_err(|_| first_err)
        }
    }
}

fn seed_repl_vm_locals(
    vm: &mut Vm,
    locals: &BTreeMap<String, ReplSessionLocal>,
) -> Result<(), VmError> {
    if locals.is_empty() {
        return Ok(());
    }
    for (name, local) in locals {
        let index = {
            let Some(debug) = vm.debug_info() else {
                return Err(VmError::HostError(
                    "repl debug info unavailable while restoring locals".to_string(),
                ));
            };
            debug.local_index(name).ok_or_else(|| {
                VmError::HostError(format!("repl local '{name}' missing from compiled snippet"))
            })?
        };
        vm.set_local(index, local.value.clone())?;
    }
    Ok(())
}

fn repl_local_schema_from_vm(
    vm: &Vm,
    index: usize,
    value: &Value,
) -> (Option<vm::compiler::TypeSchema>, bool) {
    let fallback = repl_schema_from_value(value);
    let Some(type_map) = vm.program().type_map.as_ref() else {
        return (fallback, false);
    };
    let schema = type_map
        .local_schemas
        .get(index)
        .cloned()
        .flatten()
        .or_else(|| {
            type_map
                .local_types
                .get(index)
                .copied()
                .and_then(repl_schema_from_value_type)
        })
        .or(fallback);
    let optional = type_map.optional_slots.get(index).copied().unwrap_or(false);
    (schema, optional)
}

fn repl_schema_from_value(value: &Value) -> Option<vm::compiler::TypeSchema> {
    use vm::compiler::TypeSchema;

    match value {
        Value::Null => Some(TypeSchema::Null),
        Value::Int(_) => Some(TypeSchema::Int),
        Value::Float(_) => Some(TypeSchema::Float),
        Value::Bool(_) => Some(TypeSchema::Bool),
        Value::String(_) => Some(TypeSchema::String),
        Value::Bytes(_) => Some(TypeSchema::Bytes),
        Value::Array(_) => Some(TypeSchema::Array(Box::new(TypeSchema::Unknown))),
        Value::Map(_) => Some(TypeSchema::Map(Box::new(TypeSchema::Unknown))),
    }
}

fn repl_schema_from_value_type(value_type: vm::ValueType) -> Option<vm::compiler::TypeSchema> {
    use vm::compiler::TypeSchema;

    match value_type {
        vm::ValueType::Unknown => None,
        vm::ValueType::Null => Some(TypeSchema::Null),
        vm::ValueType::Int => Some(TypeSchema::Int),
        vm::ValueType::Float => Some(TypeSchema::Float),
        vm::ValueType::Bool => Some(TypeSchema::Bool),
        vm::ValueType::String => Some(TypeSchema::String),
        vm::ValueType::Bytes => Some(TypeSchema::Bytes),
        vm::ValueType::Array => Some(TypeSchema::Array(Box::new(TypeSchema::Unknown))),
        vm::ValueType::Map => Some(TypeSchema::Map(Box::new(TypeSchema::Unknown))),
    }
}

fn is_repl_input_complete(input: &str) -> bool {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Delimiter {
        Paren,
        Bracket,
        Brace,
    }

    let mut stack: Vec<Delimiter> = Vec::new();
    let mut chars = input.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;
    let mut in_line_comment = false;
    let mut in_block_comment = false;
    let mut code = String::with_capacity(input.len());

    while let Some(ch) = chars.next() {
        if in_line_comment {
            if ch == '\n' {
                in_line_comment = false;
                code.push('\n');
            }
            continue;
        }
        if in_block_comment {
            if ch == '*'
                && let Some('/') = chars.peek()
            {
                chars.next();
                in_block_comment = false;
            }
            continue;
        }
        if in_string {
            if escaped {
                escaped = false;
                continue;
            }
            match ch {
                '\\' => escaped = true,
                '"' => {
                    in_string = false;
                    code.push('"');
                }
                _ => {}
            }
            continue;
        }

        if ch == '/' {
            match chars.peek().copied() {
                Some('/') => {
                    chars.next();
                    in_line_comment = true;
                    continue;
                }
                Some('*') => {
                    chars.next();
                    in_block_comment = true;
                    continue;
                }
                _ => {}
            }
        }

        match ch {
            '"' => {
                in_string = true;
                code.push('"');
            }
            '(' => {
                stack.push(Delimiter::Paren);
                code.push(ch);
            }
            '[' => {
                stack.push(Delimiter::Bracket);
                code.push(ch);
            }
            '{' => {
                stack.push(Delimiter::Brace);
                code.push(ch);
            }
            ')' => {
                if stack.pop() != Some(Delimiter::Paren) {
                    return true;
                }
                code.push(ch);
            }
            ']' => {
                if stack.pop() != Some(Delimiter::Bracket) {
                    return true;
                }
                code.push(ch);
            }
            '}' => {
                if stack.pop() != Some(Delimiter::Brace) {
                    return true;
                }
                code.push(ch);
            }
            _ => code.push(ch),
        }
    }

    if in_string || in_block_comment || !stack.is_empty() {
        return false;
    }

    let trimmed = code.trim_end();
    if trimmed.is_empty() {
        return true;
    }

    const TRAILING_INCOMPLETE_TOKENS: [&str; 18] = [
        "=>", "::", "&&", "||", "<=", ">=", "==", "!=", "=", ",", ".", "+", "-", "*", "/", "%",
        "!", ":",
    ];
    !TRAILING_INCOMPLETE_TOKENS
        .iter()
        .any(|token| trimmed.ends_with(token))
}

fn render_repl_compile_error(snippet: &str, err: &vm::SourceError) -> String {
    match err {
        vm::SourceError::Parse(parse) => {
            let mut source_map = SourceMap::new();
            let source_id = source_map.add_source("<repl>", snippet.to_string());
            let parse = parse
                .clone()
                .with_line_span_from_source(&source_map, source_id);
            render_source_error(&source_map, &parse, true)
        }
        _ => err.to_string(),
    }
}

fn add_one_host_function(_vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
    let value = match args.first() {
        Some(Value::Int(value)) => *value,
        _ => return Err(VmError::TypeMismatch("int")),
    };
    Ok(CallOutcome::Return(CallReturn::one(Value::Int(value + 1))))
}

fn echo_host_function(_vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
    let value = args.first().cloned().ok_or(VmError::StackUnderflow)?;
    Ok(CallOutcome::Return(CallReturn::one(value)))
}

fn format_value(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Int(value) => value.to_string(),
        Value::Float(value) => value.to_string(),
        Value::Bool(value) => value.to_string(),
        Value::String(value) => value.as_str().to_string(),
        Value::Bytes(value) => format_bytes(value.as_ref()),
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

fn format_bytes(bytes: &[u8]) -> String {
    let preview_len = bytes.len().min(16);
    let mut preview = String::with_capacity(preview_len * 2);
    for byte in &bytes[..preview_len] {
        preview.push(hex_nibble(byte >> 4));
        preview.push(hex_nibble(byte & 0x0F));
    }
    if bytes.len() > preview_len {
        format!("bytes[len={} hex={}..]", bytes.len(), preview)
    } else {
        format!("bytes[len={} hex={}]", bytes.len(), preview)
    }
}

fn hex_nibble(value: u8) -> char {
    match value {
        0..=9 => char::from(b'0' + value),
        10..=15 => char::from(b'a' + (value - 10)),
        _ => unreachable!("hex nibble out of range"),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{
        CliConfig, parse_cli_args, prepare_aot_for_cli, register_imports,
        try_new_cli_vm_from_standalone_aot,
    };
    use vm::{HostImport, OpCode, Program, Value, ValueType, Vm, VmStatus};

    fn s(value: &str) -> String {
        value.to_string()
    }

    fn native_aot_supported() -> bool {
        (cfg!(target_arch = "x86_64")
            && (cfg!(target_os = "windows") || (cfg!(unix) && !cfg!(target_os = "macos"))))
            || (cfg!(target_arch = "aarch64")
                && (cfg!(target_os = "linux") || cfg!(target_os = "macos")))
    }

    fn unique_artifact_path() -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after unix epoch")
            .as_nanos();
        path.push(format!("pd-vm-run-aot-{}-{stamp}.pat", std::process::id()));
        path
    }

    fn run_repl_snippet_and_sync(session: &mut super::ReplSession, snippet: &str) -> Vm {
        let compiled =
            super::compile_repl_snippet(snippet, &session.locals).expect("compile should succeed");
        let mut vm = Vm::new(
            compiled
                .compiled
                .program
                .with_local_count(compiled.compiled.locals),
        );
        super::configure_cli_vm(&mut vm);
        let imports = vm.program().imports.clone();
        super::register_imports(&mut vm, &imports).expect("register should succeed");
        super::seed_repl_vm_locals(&mut vm, &session.locals).expect("locals should restore");
        loop {
            match vm.run().expect("snippet should run") {
                VmStatus::Halted => break,
                VmStatus::Yielded => continue,
                VmStatus::Waiting(_) => vm
                    .wait_for_host_op_blocking()
                    .expect("snippet should not block"),
            }
        }
        super::sync_repl_session(&vm, &compiled.bindings, session);
        vm
    }

    #[test]
    fn register_imports_binds_cached_cli_host_registry_plan() {
        let imports = vec![
            HostImport {
                name: "print".to_string(),
                arity: 1,
                return_type: ValueType::Unknown,
            },
            HostImport {
                name: "echo".to_string(),
                arity: 1,
                return_type: ValueType::Unknown,
            },
        ];
        let program =
            Program::with_imports_and_debug(vec![], vec![OpCode::Ret as u8], imports.clone(), None);

        let mut first = Vm::new(program.clone());
        register_imports(&mut first, &imports).expect("first vm should bind imports");
        assert_eq!(first.bound_function_count(), 2);

        let mut second = Vm::new(program);
        register_imports(&mut second, &imports).expect("second vm should reuse cached plan");
        assert_eq!(second.bound_function_count(), 2);
    }

    #[test]
    fn parse_cli_defaults() {
        let cfg = parse_cli_args(&[]).expect("parse should succeed");
        assert!(cfg.repl);
        assert!(!cfg.debug);
        assert!(!cfg.version);
        assert!(cfg.tcp_addr.is_none());
        assert!(cfg.stop_on_entry);
        assert!(!cfg.aot);
        assert!(!cfg.aot_dump);
        assert!(cfg.aot_save_path.is_none());
        assert!(cfg.aot_load_path.is_none());
        assert!(!cfg.jit_dump);
        assert!(cfg.jit_dump_show_machine_code);
        assert!(cfg.jit_hot_loop_threshold.is_none());
        assert!(cfg.fuel.is_none());
        assert!(cfg.epoch_deadline.is_none());
        assert!(cfg.source.is_none());
        assert!(cfg.epoch_check_interval.is_none());
        assert!(cfg.emit_vmbc_path.is_none());
        assert!(cfg.disasm_vmbc_path.is_none());
        assert!(!cfg.show_source);
        assert!(!cfg.fmt);
        assert!(!cfg.fmt_check);
    }

    #[test]
    fn parse_cli_version_flag() {
        let cfg = parse_cli_args(&[s("--version")]).expect("parse should succeed");
        assert!(cfg.version);
        assert!(!cfg.repl);
    }

    #[test]
    fn parse_cli_version_short_flag() {
        let cfg = parse_cli_args(&[s("-V")]).expect("parse should succeed");
        assert!(cfg.version);
        assert!(!cfg.repl);
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
            s("--jit-dump-no-code"),
            s("examples/example.rss"),
        ])
        .expect("parse should succeed");
        assert_eq!(cfg.jit_hot_loop_threshold, Some(2));
        assert!(cfg.jit_dump);
        assert!(!cfg.jit_dump_show_machine_code);
        assert_eq!(cfg.source.as_deref(), Some("examples/example.rss"));
    }

    #[test]
    fn parse_cli_aot_flags() {
        let cfg = parse_cli_args(&[
            s("--aot"),
            s("--aot-dump"),
            s("--aot-save"),
            s("out/program.pat"),
            s("examples/example.rss"),
        ])
        .expect("parse should succeed");
        assert!(cfg.aot);
        assert!(cfg.aot_dump);
        assert_eq!(cfg.aot_save_path.as_deref(), Some("out/program.pat"));
        assert_eq!(cfg.source.as_deref(), Some("examples/example.rss"));
    }

    #[test]
    fn parse_cli_aot_load_without_source_path() {
        let cfg =
            parse_cli_args(&[s("--aot-load"), s("out/program.pat")]).expect("parse should succeed");
        assert_eq!(cfg.aot_load_path.as_deref(), Some("out/program.pat"));
        assert!(cfg.source.is_none());
    }

    #[test]
    fn parse_cli_rejects_aot_and_load_together() {
        let err = parse_cli_args(&[
            s("--aot"),
            s("--aot-load"),
            s("out/program.pat"),
            s("examples/example.rss"),
        ])
        .expect_err("parse should fail");
        assert!(err.contains("mutually exclusive"));
    }

    #[test]
    fn parse_cli_debug_rejects_aot_runtime_flags() {
        let err = parse_cli_args(&[s("--debug"), s("--aot"), s("examples/example.rss")])
            .expect_err("parse should fail");
        assert!(err.contains("debug mode"));
    }

    #[test]
    fn parse_cli_dump_jit_alias() {
        let cfg = parse_cli_args(&[s("--dump-jit"), s("examples/example.rss")])
            .expect("parse should succeed");
        assert!(cfg.jit_dump);
        assert!(cfg.jit_dump_show_machine_code);
        assert_eq!(cfg.source.as_deref(), Some("examples/example.rss"));
    }

    #[test]
    fn parse_cli_jit_dump_no_code_requires_dump_flag() {
        let err = parse_cli_args(&[s("--jit-dump-no-code"), s("examples/example.rss")])
            .expect_err("parse should fail");
        assert!(err.contains("requires --jit-dump or --dump-jit"));
    }

    #[test]
    fn parse_cli_fuel_flag() {
        let cfg = parse_cli_args(&[s("--fuel"), s("123"), s("examples/example.rss")])
            .expect("parse should succeed");
        assert_eq!(cfg.fuel, Some(123));
        assert_eq!(cfg.source.as_deref(), Some("examples/example.rss"));
    }

    #[test]
    fn parse_cli_fuel_requires_value() {
        let err = parse_cli_args(&[s("--fuel")]).expect_err("parse should fail");
        assert!(err.contains("missing value for --fuel"));
    }

    #[test]
    fn parse_cli_epoch_deadline_flag() {
        let cfg = parse_cli_args(&[s("--epoch-deadline"), s("3"), s("examples/example.rss")])
            .expect("parse should succeed");
        assert_eq!(cfg.epoch_deadline, Some(3));
        assert_eq!(cfg.source.as_deref(), Some("examples/example.rss"));
    }

    #[test]
    fn parse_cli_rejects_fuel_and_epoch_deadline_together() {
        let err = parse_cli_args(&[
            s("--fuel"),
            s("10"),
            s("--epoch-deadline"),
            s("3"),
            s("examples/example.rss"),
        ])
        .expect_err("parse should fail");
        assert!(err.contains("mutually exclusive"));
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
    fn parse_cli_view_record_rejects_fuel() {
        let err = parse_cli_args(&[s("--view-record"), s("out/run.pdr"), s("--fuel"), s("10")])
            .expect_err("parse should fail");
        assert!(err.contains("view-record mode"));
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
    fn parse_cli_fmt_command() {
        let cfg =
            parse_cli_args(&[s("fmt"), s("examples/example.rss")]).expect("parse should succeed");
        assert!(cfg.fmt);
        assert!(!cfg.fmt_check);
        assert_eq!(cfg.source.as_deref(), Some("examples/example.rss"));
    }

    #[test]
    fn parse_cli_fmt_check_flag() {
        let cfg = parse_cli_args(&[s("fmt"), s("--check"), s("examples/example.rss")])
            .expect("parse should succeed");
        assert!(cfg.fmt);
        assert!(cfg.fmt_check);
        assert_eq!(cfg.source.as_deref(), Some("examples/example.rss"));
    }

    #[test]
    fn parse_cli_fmt_requires_source_path() {
        let err = parse_cli_args(&[s("fmt")]).expect_err("parse should fail");
        assert!(err.contains("requires a source path"));
    }

    #[test]
    fn parse_cli_check_requires_fmt() {
        let err = parse_cli_args(&[s("--check"), s("examples/example.rss")])
            .expect_err("parse should fail");
        assert!(err.contains("requires fmt mode"));
    }

    #[test]
    fn parse_cli_fmt_rejects_debug_flag() {
        let err = parse_cli_args(&[s("fmt"), s("--debug"), s("examples/example.rss")])
            .expect_err("parse should fail");
        assert!(err.contains("fmt mode"));
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
    fn parse_cli_repl_rejects_fuel() {
        let err =
            parse_cli_args(&[s("--repl"), s("--fuel"), s("10")]).expect_err("parse should fail");
        assert!(err.contains("cannot be combined"));
    }

    #[test]
    fn prepare_cli_aot_can_save_and_reload_artifact() {
        if !native_aot_supported() {
            return;
        }

        let program = Program::new(
            vec![Value::Int(9)],
            vec![OpCode::Ldc as u8, 0, 0, 0, 0, OpCode::Ret as u8],
        );
        let artifact_path = unique_artifact_path();

        let mut save_vm = Vm::new(program.clone());
        let save_cfg = CliConfig {
            aot_save_path: Some(artifact_path.display().to_string()),
            ..CliConfig::default()
        };
        prepare_aot_for_cli(&mut save_vm, &save_cfg).expect("aot save should succeed");
        assert!(save_vm.has_aot_program(), "save path should install aot");

        let mut load_vm = Vm::new(program);
        let load_cfg = CliConfig {
            aot_load_path: Some(artifact_path.display().to_string()),
            ..CliConfig::default()
        };
        prepare_aot_for_cli(&mut load_vm, &load_cfg).expect("aot load should succeed");
        assert!(load_vm.has_aot_program(), "load path should install aot");
        let status = load_vm.run().expect("loaded aot vm should run");
        assert_eq!(status, VmStatus::Halted);
        assert_eq!(load_vm.stack(), &[Value::Int(9)]);

        std::fs::remove_file(&artifact_path).expect("artifact cleanup should succeed");
    }

    #[test]
    fn standalone_cli_aot_load_without_source_uses_embedded_program() {
        if !native_aot_supported() {
            return;
        }

        let program = Program::new(
            vec![Value::Int(9)],
            vec![OpCode::Ldc as u8, 0, 0, 0, 0, OpCode::Ret as u8],
        )
        .with_local_count(5);
        let artifact_path = unique_artifact_path();

        let mut save_vm = Vm::new(program.clone());
        let save_cfg = CliConfig {
            aot_save_path: Some(artifact_path.display().to_string()),
            ..CliConfig::default()
        };
        prepare_aot_for_cli(&mut save_vm, &save_cfg).expect("aot save should succeed");

        let load_cfg = CliConfig {
            aot_load_path: Some(artifact_path.display().to_string()),
            ..CliConfig::default()
        };
        let mut loaded_vm = try_new_cli_vm_from_standalone_aot(&load_cfg)
            .expect("standalone load should succeed")
            .expect("standalone load should create a vm");

        assert!(
            loaded_vm.has_aot_program(),
            "standalone load should install aot"
        );
        assert_eq!(loaded_vm.program().local_count, 5);

        let status = loaded_vm.run().expect("standalone aot vm should run");
        assert_eq!(status, VmStatus::Halted);
        assert_eq!(loaded_vm.stack(), &[Value::Int(9)]);

        std::fs::remove_file(&artifact_path).expect("artifact cleanup should succeed");
    }

    #[test]
    fn repl_compile_falls_back_to_expression_semicolon() {
        let compiled =
            super::compile_repl_snippet("1 + 2", &BTreeMap::new()).expect("compile should succeed");
        assert_eq!(compiled.compiled.locals, 0);
    }

    #[test]
    fn repl_compile_uses_persisted_locals() {
        let mut locals = BTreeMap::new();
        locals.insert(
            "x".to_string(),
            super::ReplSessionLocal {
                value: Value::Int(41),
                mutable: false,
                schema: Some(vm::compiler::TypeSchema::Int),
                optional: false,
            },
        );
        let compiled =
            super::compile_repl_snippet("x + 1", &locals).expect("compile should succeed");
        assert!(compiled.compiled.locals >= 1);
    }

    #[test]
    fn repl_session_persists_locals_between_entries() {
        let mut session = super::ReplSession::default();
        let _ = run_repl_snippet_and_sync(&mut session, "let x = 41;");
        assert_eq!(
            session.locals.get("x").map(|local| &local.value),
            Some(&Value::Int(41))
        );

        let vm = run_repl_snippet_and_sync(&mut session, "x + 1");
        assert_eq!(vm.stack().last(), Some(&Value::Int(42)));
    }

    #[test]
    fn repl_session_persists_mutable_locals_between_entries() {
        let mut session = super::ReplSession::default();
        let _ = run_repl_snippet_and_sync(&mut session, "let mut x = 1;");
        assert_eq!(
            session.locals.get("x").map(|local| local.mutable),
            Some(true)
        );

        let _ = run_repl_snippet_and_sync(&mut session, "x = x + 1;");
        let vm = run_repl_snippet_and_sync(&mut session, "x");
        assert_eq!(vm.stack().last(), Some(&Value::Int(2)));
    }

    #[test]
    fn repl_session_persists_null_between_entries() {
        let mut session = super::ReplSession::default();
        let _ = run_repl_snippet_and_sync(&mut session, "let x = null;");
        assert_eq!(
            session.locals.get("x").map(|local| &local.value),
            Some(&Value::Null)
        );

        let vm = run_repl_snippet_and_sync(&mut session, "x");
        assert_eq!(vm.stack().last(), Some(&Value::Null));
    }

    #[test]
    fn repl_session_persists_float_between_entries() {
        let mut session = super::ReplSession::default();
        let _ = run_repl_snippet_and_sync(&mut session, "let x = 1.5;");
        assert_eq!(
            session.locals.get("x").map(|local| &local.value),
            Some(&Value::Float(1.5))
        );

        let vm = run_repl_snippet_and_sync(&mut session, "x + 0.5");
        assert_eq!(vm.stack().last(), Some(&Value::Float(2.0)));
    }

    #[test]
    fn repl_compile_remaps_parse_error_line_numbers() {
        let mut locals = BTreeMap::new();
        locals.insert(
            "x".to_string(),
            super::ReplSessionLocal {
                value: Value::Int(1),
                mutable: false,
                schema: Some(vm::compiler::TypeSchema::Int),
                optional: false,
            },
        );
        match super::compile_repl_snippet("let y = ;", &locals) {
            Err(vm::SourceError::Parse(parse)) => assert_eq!(parse.line, 1),
            Err(other) => panic!("expected parse error, got {other}"),
            Ok(_) => panic!("expected parse error, got successful compile"),
        }
    }

    #[test]
    fn repl_input_complete_for_balanced_match_block() {
        let input = "let b = match a {\n    Some(String) => 2,\n    _ => 3,\n};";
        assert!(super::is_repl_input_complete(input));
    }

    #[test]
    fn repl_input_incomplete_for_open_brace() {
        assert!(!super::is_repl_input_complete("let b = match a {"));
    }

    #[test]
    fn repl_input_incomplete_for_unclosed_string() {
        assert!(!super::is_repl_input_complete("let s = \"hello"));
    }

    #[test]
    fn repl_input_incomplete_for_unclosed_block_comment() {
        assert!(!super::is_repl_input_complete("let a = 1; /* comment"));
    }

    #[test]
    fn repl_input_ignores_comment_delimiters() {
        assert!(super::is_repl_input_complete("// {\nlet a = 1;"));
    }

    #[test]
    fn repl_input_incomplete_for_trailing_operator() {
        assert!(!super::is_repl_input_complete("let a = 1 +"));
    }
}
