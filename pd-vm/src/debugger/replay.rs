use super::*;

pub(super) enum ReplayAction {
    Continue,
    Exit,
}

impl ReplayAction {
    pub(super) fn should_exit(self) -> bool {
        matches!(self, ReplayAction::Exit)
    }
}

pub(super) fn handle_replay_command(
    line: &str,
    recording: &VmRecording,
    cursor: &mut usize,
    replay_breakpoints: &mut ReplayBreakpoints,
    out: &mut dyn Write,
) -> ReplayAction {
    let mut parts = line.split_whitespace();
    let Some(cmd) = parts.next() else {
        return ReplayAction::Continue;
    };
    match cmd {
        "q" | "quit" | "exit" => return ReplayAction::Exit,
        "c" | "continue" => {
            if let Some(next) =
                replay_breakpoints.next_pause_frame(recording, cursor.saturating_add(1))
            {
                *cursor = next;
            } else {
                *cursor = recording.frames.len().saturating_sub(1);
            }
            let _ = write_replay_position(recording, *cursor, out);
            return ReplayAction::Continue;
        }
        "s" | "step" | "stepi" => {
            if *cursor + 1 < recording.frames.len() {
                *cursor += 1;
            }
            let _ = write_replay_position(recording, *cursor, out);
            return ReplayAction::Continue;
        }
        "n" | "next" => {
            *cursor = replay_step_over(recording, *cursor);
            let _ = write_replay_position(recording, *cursor, out);
            return ReplayAction::Continue;
        }
        "finish" | "out" => {
            *cursor = replay_step_out(recording, *cursor);
            let _ = write_replay_position(recording, *cursor, out);
            return ReplayAction::Continue;
        }
        "b" | "break" => {
            if let Some(arg) = parts.next() {
                if arg == "line" {
                    if let Some(requested_line) = parse_u32(parts.next()) {
                        let line = recording
                            .program
                            .debug
                            .as_ref()
                            .map(|info| resolve_executable_line(info, requested_line))
                            .unwrap_or(requested_line);
                        replay_breakpoints.line_breakpoints.insert(line);
                        if line == requested_line {
                            let _ = writeln!(out, "replay pause point set at line {line}");
                        } else {
                            let _ = writeln!(
                                out,
                                "replay pause point set at line {line} (requested line {requested_line})"
                            );
                        }
                    } else {
                        let _ = writeln!(out, "usage: break line <number>");
                    }
                    return ReplayAction::Continue;
                }
                if let Ok(offset) = arg.parse::<usize>() {
                    replay_breakpoints.offset_breakpoints.insert(offset);
                    let _ = writeln!(out, "replay pause point set at offset {offset}");
                } else {
                    let _ = writeln!(out, "expected instruction offset");
                }
            } else {
                let _ = writeln!(out, "usage: break <offset>");
            }
            return ReplayAction::Continue;
        }
        "bl" => {
            if let Some(requested_line) = parse_u32(parts.next()) {
                let line = recording
                    .program
                    .debug
                    .as_ref()
                    .map(|info| resolve_executable_line(info, requested_line))
                    .unwrap_or(requested_line);
                replay_breakpoints.line_breakpoints.insert(line);
                if line == requested_line {
                    let _ = writeln!(out, "replay pause point set at line {line}");
                } else {
                    let _ = writeln!(
                        out,
                        "replay pause point set at line {line} (requested line {requested_line})"
                    );
                }
            } else {
                let _ = writeln!(out, "usage: bl <line>");
            }
            return ReplayAction::Continue;
        }
        "clear" => {
            if let Some(arg) = parts.next() {
                if arg == "line" {
                    if let Some(requested_line) = parse_u32(parts.next()) {
                        let line = recording
                            .program
                            .debug
                            .as_ref()
                            .map(|info| resolve_executable_line(info, requested_line))
                            .unwrap_or(requested_line);
                        replay_breakpoints.line_breakpoints.remove(&line);
                        if line == requested_line {
                            let _ = writeln!(out, "replay pause point cleared at line {line}");
                        } else {
                            let _ = writeln!(
                                out,
                                "replay pause point cleared at line {line} (requested line {requested_line})"
                            );
                        }
                    } else {
                        let _ = writeln!(out, "usage: clear line <number>");
                    }
                    return ReplayAction::Continue;
                }
                if let Ok(offset) = arg.parse::<usize>() {
                    replay_breakpoints.offset_breakpoints.remove(&offset);
                    let _ = writeln!(out, "replay pause point cleared at offset {offset}");
                } else {
                    let _ = writeln!(out, "expected instruction offset");
                }
            } else {
                let _ = writeln!(out, "usage: clear <offset>");
            }
            return ReplayAction::Continue;
        }
        "cl" => {
            if let Some(requested_line) = parse_u32(parts.next()) {
                let line = recording
                    .program
                    .debug
                    .as_ref()
                    .map(|info| resolve_executable_line(info, requested_line))
                    .unwrap_or(requested_line);
                replay_breakpoints.line_breakpoints.remove(&line);
                if line == requested_line {
                    let _ = writeln!(out, "replay pause point cleared at line {line}");
                } else {
                    let _ = writeln!(
                        out,
                        "replay pause point cleared at line {line} (requested line {requested_line})"
                    );
                }
            } else {
                let _ = writeln!(out, "usage: cl <line>");
            }
            return ReplayAction::Continue;
        }
        "breaks" => {
            let _ = writeln!(
                out,
                "replay pause offsets: {:?}",
                replay_breakpoints.offset_breakpoints
            );
            let _ = writeln!(
                out,
                "replay pause lines: {:?}",
                replay_breakpoints.line_breakpoints
            );
            return ReplayAction::Continue;
        }
        _ => {}
    }

    let frame = &recording.frames[*cursor];
    match cmd {
        "stack" => {
            let _ = writeln!(out, "stack: {:?}", frame.stack);
        }
        "locals" => {
            print_replay_locals(recording, frame, out);
        }
        "p" | "print" => {
            if let Some(name) = parts.next() {
                print_replay_local_by_name(recording, frame, name, out);
            } else {
                let _ = writeln!(out, "usage: print <local_name>");
            }
        }
        "ip" => {
            let _ = writeln!(out, "ip: {}", frame.ip);
        }
        "where" => {
            print_replay_where(recording, frame, out);
        }
        "funcs" => {
            if let Some(info) = recording.program.debug.as_ref() {
                for func in &info.functions {
                    let _ = writeln!(out, "fn {}({})", func.name, format_args_list(func));
                }
            } else {
                let _ = writeln!(out, "no debug info");
            }
        }
        "help" => {
            let _ = writeln!(
                out,
                "commands: break, break line, bl, clear, clear line, cl, breaks, continue, step, next, out, stack, locals, print, ip, where, funcs, help, quit"
            );
        }
        _ => {
            let _ = writeln!(out, "unknown command");
        }
    }
    ReplayAction::Continue
}

pub(super) fn replay_step_over(recording: &VmRecording, cursor: usize) -> usize {
    if cursor + 1 >= recording.frames.len() {
        return cursor;
    }
    let start = &recording.frames[cursor];
    for index in (cursor + 1)..recording.frames.len() {
        let frame = &recording.frames[index];
        if frame.call_depth <= start.call_depth && frame.ip != start.ip {
            return index;
        }
    }
    recording.frames.len().saturating_sub(1)
}

pub(super) fn replay_step_out(recording: &VmRecording, cursor: usize) -> usize {
    if cursor + 1 >= recording.frames.len() {
        return cursor;
    }
    let start = &recording.frames[cursor];
    for index in (cursor + 1)..recording.frames.len() {
        let frame = &recording.frames[index];
        if frame.call_depth < start.call_depth {
            return index;
        }
    }
    recording.frames.len().saturating_sub(1)
}

pub(super) fn write_replay_position(
    recording: &VmRecording,
    cursor: usize,
    out: &mut dyn Write,
) -> io::Result<()> {
    let last = recording.frames.len().saturating_sub(1);
    let frame = &recording.frames[cursor];
    write!(out, "frame {cursor}/{last}")?;
    if replay_at_end(recording, cursor) {
        match recording.terminal_status {
            Some(status) => write!(out, " [end:{status:?}]")?,
            None => write!(out, " [end]")?,
        }
    }
    write!(out, ": ip={} depth={}", frame.ip, frame.call_depth)?;
    if let Some(info) = recording.program.debug.as_ref()
        && let Some(line) = info.line_for_offset(frame.ip)
    {
        if let Some(text) = info.source_line(line) {
            writeln!(out, " line {line}: {text}")?;
            return Ok(());
        }
        writeln!(out, " line: {line}")?;
        return Ok(());
    }
    writeln!(out)?;
    Ok(())
}

pub(super) fn replay_at_end(recording: &VmRecording, cursor: usize) -> bool {
    cursor + 1 >= recording.frames.len()
}

#[derive(Default)]
pub(super) struct ReplayBreakpoints {
    offset_breakpoints: HashSet<usize>,
    line_breakpoints: HashSet<u32>,
}

impl ReplayBreakpoints {
    pub(super) fn next_pause_frame(
        &self,
        recording: &VmRecording,
        start_index: usize,
    ) -> Option<usize> {
        if self.offset_breakpoints.is_empty() && self.line_breakpoints.is_empty() {
            return None;
        }
        for index in start_index..recording.frames.len() {
            let frame = &recording.frames[index];
            if self.offset_breakpoints.contains(&frame.ip) {
                return Some(index);
            }
            if let Some(info) = recording.program.debug.as_ref()
                && let Some(line) = info.line_for_offset(frame.ip)
                && self.line_breakpoints.contains(&line)
            {
                return Some(index);
            }
        }
        None
    }
}

pub fn run_recording_replay_command(
    recording: &VmRecording,
    state: &mut VmRecordingReplayState,
    command: &str,
) -> VmRecordingReplayResponse {
    if recording.frames.is_empty() {
        return VmRecordingReplayResponse {
            output: "recording has no captured frames".to_string(),
            current_line: None,
            at_end: true,
            exited: false,
        };
    }

    if state.cursor >= recording.frames.len() {
        state.cursor = recording.frames.len().saturating_sub(1);
    }

    let mut replay_breakpoints = ReplayBreakpoints {
        offset_breakpoints: state.offset_breakpoints.clone(),
        line_breakpoints: state.line_breakpoints.clone(),
    };
    let mut output = Vec::<u8>::new();
    let action = handle_replay_command(
        command,
        recording,
        &mut state.cursor,
        &mut replay_breakpoints,
        &mut output,
    );

    state.offset_breakpoints = replay_breakpoints.offset_breakpoints;
    state.line_breakpoints = replay_breakpoints.line_breakpoints;
    let current_line = replay_current_line(recording, state.cursor);

    VmRecordingReplayResponse {
        output: String::from_utf8_lossy(&output).to_string(),
        current_line,
        at_end: replay_at_end(recording, state.cursor),
        exited: action.should_exit(),
    }
}

pub fn replay_recording_stdio(recording: &VmRecording) {
    if recording.frames.is_empty() {
        println!("recording has no captured frames");
        return;
    }

    let mut cursor = 0usize;
    let mut replay_breakpoints = ReplayBreakpoints::default();
    println!(
        "recording loaded: frames={}, terminal={:?}",
        recording.frames.len(),
        recording.terminal_status
    );
    let _ = write_replay_position(recording, cursor, &mut io::stdout());

    let stdin = io::stdin();
    let mut input = String::new();
    loop {
        input.clear();
        if replay_at_end(recording, cursor) {
            print!("(pdb-rec:end) ");
        } else {
            print!("(pdb-rec) ");
        }
        let _ = io::stdout().flush();
        if stdin.read_line(&mut input).is_err() {
            break;
        }
        if handle_replay_command(
            &input,
            recording,
            &mut cursor,
            &mut replay_breakpoints,
            &mut io::stdout(),
        )
        .should_exit()
        {
            break;
        }
    }
}

pub(super) fn replay_current_line(recording: &VmRecording, cursor: usize) -> Option<u32> {
    let frame = recording.frames.get(cursor)?;
    recording
        .program
        .debug
        .as_ref()
        .and_then(|info| info.line_for_offset(frame.ip))
}

pub(super) fn print_replay_locals(
    recording: &VmRecording,
    frame: &VmRecordingFrame,
    out: &mut dyn Write,
) {
    let Some(info) = recording.program.debug.as_ref() else {
        let _ = writeln!(out, "locals: {:?}", frame.locals);
        return;
    };

    if info.locals.is_empty() {
        let _ = writeln!(out, "locals: {:?}", frame.locals);
        return;
    }

    let current_line = info.line_for_offset(frame.ip);
    for local in &info.locals {
        if !local_visible_at_line(local, current_line) {
            continue;
        }
        match frame.locals.get(local.index as usize) {
            Some(value) => {
                let _ = writeln!(out, "{} = {:?}", local.name, value);
            }
            None => {
                let _ = writeln!(out, "{} = <unavailable>", local.name);
            }
        }
    }
}

pub(super) fn local_visible_at_line(local: &LocalInfo, line: Option<u32>) -> bool {
    let Some(line) = line else {
        return true;
    };
    if let Some(declared_line) = local.declared_line
        && line < declared_line
    {
        return false;
    }
    if let Some(last_line) = local.last_line
        && line > last_line
    {
        return false;
    }
    true
}

pub(super) fn print_replay_local_by_name(
    recording: &VmRecording,
    frame: &VmRecordingFrame,
    name: &str,
    out: &mut dyn Write,
) {
    let Some(info) = recording.program.debug.as_ref() else {
        let _ = writeln!(out, "no debug info");
        return;
    };

    let Some(local) = info.locals.iter().find(|local| local.name == name) else {
        let _ = writeln!(out, "unknown local '{name}'");
        return;
    };
    let current_line = info.line_for_offset(frame.ip);
    if !local_visible_at_line(local, current_line) {
        let _ = writeln!(out, "local '{name}' is not visible in this frame");
        return;
    }

    match frame.locals.get(local.index as usize) {
        Some(value) => {
            let _ = writeln!(out, "{name} = {:?}", value);
        }
        None => {
            let _ = writeln!(out, "local '{name}' is out of range for this frame");
        }
    }
}

pub(super) fn print_replay_where(
    recording: &VmRecording,
    frame: &VmRecordingFrame,
    out: &mut dyn Write,
) {
    if let Some(info) = recording.program.debug.as_ref() {
        if let Some(line) = info.line_for_offset(frame.ip) {
            if let Some(text) = info.source_line(line) {
                let _ = writeln!(out, "line {line}: {text}");
            } else {
                let _ = writeln!(out, "line: {line}");
            }
        } else {
            let _ = writeln!(out, "line: unknown");
        }
    } else {
        let _ = writeln!(out, "no debug info");
    }
}
