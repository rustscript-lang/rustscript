use std::collections::HashSet;
use std::time::{Duration, Instant};

use crate::debug_info::{DebugInfo, LocalInfo};
use crate::vm::{Program, Value, Vm, VmStatus};

use super::{
    DebugCommandBridgeError, Debugger, ReplAction, ReplayBreakpoints, StepMode, VmRecording,
    VmRecordingFrame, handle_command, handle_replay_command,
};

fn wait_for_bridge_attachment(bridge: &super::DebugCommandBridge, attached: bool) {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if bridge.status().attached == attached {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for debugger attached={attached}"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn vm_with_named_local(name: &str, value: Value) -> Vm {
    let program = Program::with_debug(
        vec![value],
        vec![
            crate::vm::OpCode::Ldc as u8,
            0,
            0,
            0,
            0,
            crate::vm::OpCode::Stloc as u8,
            0,
            crate::vm::OpCode::Ret as u8,
        ],
        Some(DebugInfo {
            source: None,
            lines: vec![],
            functions: vec![],
            locals: vec![LocalInfo {
                name: name.to_string(),
                index: 0,
                declared_line: None,
                last_line: None,
            }],
        }),
    );
    let mut vm = Vm::new(program.with_local_count(1));
    let status = vm.run().expect("vm should run");
    assert_eq!(status, crate::vm::VmStatus::Halted);
    vm
}

fn vm_with_named_unassigned_local(name: &str) -> Vm {
    let program = Program::with_debug(
        vec![],
        vec![crate::vm::OpCode::Ret as u8],
        Some(DebugInfo {
            source: None,
            lines: vec![],
            functions: vec![],
            locals: vec![LocalInfo {
                name: name.to_string(),
                index: 0,
                declared_line: None,
                last_line: None,
            }],
        }),
    );
    Vm::new(program.with_local_count(1))
}

fn vm_with_scoped_named_locals() -> Vm {
    let program = Program::with_debug(
        vec![Value::Int(1)],
        vec![
            crate::vm::OpCode::Ldc as u8,
            0,
            0,
            0,
            0,
            crate::vm::OpCode::Stloc as u8,
            0,
            crate::vm::OpCode::Ret as u8,
        ],
        Some(DebugInfo {
            source: None,
            lines: vec![crate::debug_info::LineInfo { offset: 0, line: 1 }],
            functions: vec![],
            locals: vec![
                LocalInfo {
                    name: "a".to_string(),
                    index: 0,
                    declared_line: Some(1),
                    last_line: Some(1),
                },
                LocalInfo {
                    name: "b".to_string(),
                    index: 0,
                    declared_line: Some(2),
                    last_line: Some(3),
                },
            ],
        }),
    );
    let mut vm = Vm::new(program.with_local_count(1));
    let status = vm.run().expect("vm should run");
    assert_eq!(status, crate::vm::VmStatus::Halted);
    vm
}

#[test]
fn print_local_by_name_uses_debug_name() {
    let mut vm = vm_with_named_local("counter", Value::Int(42));
    let mut out = Vec::<u8>::new();
    let mut breakpoints = HashSet::new();
    let mut line_breakpoints = HashSet::new();
    let mut step_mode = StepMode::Running;

    let action = handle_command(
        "print counter",
        &mut vm,
        &mut breakpoints,
        &mut line_breakpoints,
        &mut step_mode,
        &mut out,
    );
    assert_eq!(action, ReplAction::Continue);
    let text = String::from_utf8(out).expect("output should be utf-8");
    assert!(text.contains("counter = Int(42)"));
}

#[test]
fn print_local_by_name_reports_unknown_local() {
    let mut vm = vm_with_named_local("counter", Value::Int(42));
    let mut out = Vec::<u8>::new();
    let mut breakpoints = HashSet::new();
    let mut line_breakpoints = HashSet::new();
    let mut step_mode = StepMode::Running;

    handle_command(
        "p missing",
        &mut vm,
        &mut breakpoints,
        &mut line_breakpoints,
        &mut step_mode,
        &mut out,
    );
    let text = String::from_utf8(out).expect("output should be utf-8");
    assert!(text.contains("unknown local 'missing'"));
}

#[test]
fn print_local_by_name_shows_null_for_unassigned_local() {
    let mut vm = vm_with_named_unassigned_local("counter");
    let mut out = Vec::<u8>::new();
    let mut breakpoints = HashSet::new();
    let mut line_breakpoints = HashSet::new();
    let mut step_mode = StepMode::Running;

    handle_command(
        "p counter",
        &mut vm,
        &mut breakpoints,
        &mut line_breakpoints,
        &mut step_mode,
        &mut out,
    );
    let text = String::from_utf8(out).expect("output should be utf-8");
    assert!(text.contains("counter = Null"));
}

#[test]
fn locals_command_filters_by_debug_line_visibility() {
    let mut vm = vm_with_scoped_named_locals();
    let mut out = Vec::<u8>::new();
    let mut breakpoints = HashSet::new();
    let mut line_breakpoints = HashSet::new();
    let mut step_mode = StepMode::Running;

    handle_command(
        "locals",
        &mut vm,
        &mut breakpoints,
        &mut line_breakpoints,
        &mut step_mode,
        &mut out,
    );

    let text = String::from_utf8(out).expect("output should be utf-8");
    assert!(text.contains("a = Int(1)"));
    assert!(!text.contains("b = "));
}

#[test]
fn print_local_by_name_reports_not_visible_when_outside_debug_window() {
    let mut vm = vm_with_scoped_named_locals();
    let mut out = Vec::<u8>::new();
    let mut breakpoints = HashSet::new();
    let mut line_breakpoints = HashSet::new();
    let mut step_mode = StepMode::Running;

    handle_command(
        "p b",
        &mut vm,
        &mut breakpoints,
        &mut line_breakpoints,
        &mut step_mode,
        &mut out,
    );

    let text = String::from_utf8(out).expect("output should be utf-8");
    assert!(text.contains("local 'b' is not visible"));
}

#[test]
fn recording_encode_decode_roundtrip() {
    let program = Program::with_debug(
        vec![Value::Int(1)],
        vec![crate::vm::OpCode::Ret as u8],
        Some(DebugInfo {
            source: Some("let x = 1;".to_string()),
            lines: vec![crate::debug_info::LineInfo { offset: 0, line: 1 }],
            functions: vec![],
            locals: vec![LocalInfo {
                name: "x".to_string(),
                index: 0,
                declared_line: None,
                last_line: None,
            }],
        }),
    );
    let recording = VmRecording {
        program: program.clone(),
        frames: vec![
            VmRecordingFrame {
                ip: 0,
                call_depth: 0,
                stack: vec![Value::array(vec![Value::Int(7), Value::Bool(true)])],
                locals: vec![Value::map(vec![(Value::string("k"), Value::Int(9))])],
            },
            VmRecordingFrame {
                ip: 1,
                call_depth: 0,
                stack: vec![Value::Int(42)],
                locals: vec![Value::Int(42)],
            },
        ],
        terminal_status: Some(VmStatus::Halted),
    };

    let bytes = recording.encode().expect("encode should succeed");
    let decoded = VmRecording::decode(&bytes).expect("decode should succeed");

    assert_eq!(decoded.frames, recording.frames);
    assert_eq!(decoded.terminal_status, recording.terminal_status);
    assert_eq!(decoded.program.code, program.code);
    assert_eq!(decoded.program.constants, program.constants);
    assert_eq!(decoded.program.imports, program.imports);
    assert_eq!(decoded.program.debug, program.debug);
}

#[test]
fn recording_debugger_captures_initial_and_terminal_frames() {
    let program = Program::new(vec![], vec![crate::vm::OpCode::Ret as u8]);
    let mut vm = Vm::new(program.clone());
    let mut debugger = Debugger::with_recording(program);

    let status = vm
        .run_with_debugger(&mut debugger)
        .expect("recorded run should succeed");
    assert_eq!(status, VmStatus::Halted);

    let recording = debugger
        .take_recording()
        .expect("recording should be available");
    assert!(recording.frames.len() >= 2);
    assert_eq!(recording.frames.first().expect("first frame").ip, 0);
    assert_eq!(recording.terminal_status, Some(VmStatus::Halted));
}

#[test]
fn replay_break_sets_pause_point_for_continue() {
    let recording = VmRecording {
        program: Program::new(vec![], vec![]),
        frames: vec![
            VmRecordingFrame {
                ip: 0,
                call_depth: 0,
                stack: vec![],
                locals: vec![],
            },
            VmRecordingFrame {
                ip: 1,
                call_depth: 0,
                stack: vec![],
                locals: vec![],
            },
            VmRecordingFrame {
                ip: 2,
                call_depth: 0,
                stack: vec![],
                locals: vec![],
            },
        ],
        terminal_status: Some(VmStatus::Halted),
    };
    let mut cursor = 0usize;
    let mut replay_breakpoints = ReplayBreakpoints::default();
    let mut out = Vec::<u8>::new();

    handle_replay_command(
        "break 1",
        &recording,
        &mut cursor,
        &mut replay_breakpoints,
        &mut out,
    );
    out.clear();
    handle_replay_command(
        "continue",
        &recording,
        &mut cursor,
        &mut replay_breakpoints,
        &mut out,
    );
    let text = String::from_utf8(out).expect("output should be utf-8");
    assert!(text.contains("frame 1/2"));
    assert_eq!(cursor, 1);
}

#[test]
fn replay_continue_marks_end_of_recording() {
    let recording = VmRecording {
        program: Program::new(vec![], vec![]),
        frames: vec![
            VmRecordingFrame {
                ip: 0,
                call_depth: 0,
                stack: vec![],
                locals: vec![],
            },
            VmRecordingFrame {
                ip: 1,
                call_depth: 0,
                stack: vec![],
                locals: vec![],
            },
        ],
        terminal_status: Some(VmStatus::Halted),
    };
    let mut cursor = 0usize;
    let mut replay_breakpoints = ReplayBreakpoints::default();
    let mut out = Vec::<u8>::new();

    handle_replay_command(
        "continue",
        &recording,
        &mut cursor,
        &mut replay_breakpoints,
        &mut out,
    );

    let text = String::from_utf8(out).expect("output should be utf-8");
    assert!(text.contains("[end:Halted]"));
    assert_eq!(cursor, 1);
}

#[test]
fn replay_break_line_sets_pause_point_for_continue() {
    let program = Program::with_debug(
        vec![],
        vec![],
        Some(DebugInfo {
            source: None,
            lines: vec![
                crate::debug_info::LineInfo { offset: 0, line: 1 },
                crate::debug_info::LineInfo { offset: 5, line: 2 },
            ],
            functions: vec![],
            locals: vec![],
        }),
    );
    let recording = VmRecording {
        program,
        frames: vec![
            VmRecordingFrame {
                ip: 0,
                call_depth: 0,
                stack: vec![],
                locals: vec![],
            },
            VmRecordingFrame {
                ip: 5,
                call_depth: 0,
                stack: vec![],
                locals: vec![],
            },
            VmRecordingFrame {
                ip: 9,
                call_depth: 0,
                stack: vec![],
                locals: vec![],
            },
        ],
        terminal_status: Some(VmStatus::Halted),
    };
    let mut cursor = 0usize;
    let mut replay_breakpoints = ReplayBreakpoints::default();
    let mut out = Vec::<u8>::new();

    handle_replay_command(
        "break line 2",
        &recording,
        &mut cursor,
        &mut replay_breakpoints,
        &mut out,
    );
    out.clear();
    handle_replay_command(
        "continue",
        &recording,
        &mut cursor,
        &mut replay_breakpoints,
        &mut out,
    );

    let text = String::from_utf8(out).expect("output should be utf-8");
    assert!(text.contains("frame 1/2"));
    assert_eq!(cursor, 1);
}

#[test]
fn replay_break_line_on_non_executable_source_line_resolves_forward() {
    let program = Program::with_debug(
        vec![],
        vec![],
        Some(DebugInfo {
            source: None,
            lines: vec![
                crate::debug_info::LineInfo { offset: 0, line: 8 },
                crate::debug_info::LineInfo {
                    offset: 10,
                    line: 13,
                },
            ],
            functions: vec![],
            locals: vec![],
        }),
    );
    let recording = VmRecording {
        program,
        frames: vec![
            VmRecordingFrame {
                ip: 0,
                call_depth: 0,
                stack: vec![],
                locals: vec![],
            },
            VmRecordingFrame {
                ip: 10,
                call_depth: 0,
                stack: vec![],
                locals: vec![],
            },
        ],
        terminal_status: Some(VmStatus::Halted),
    };
    let mut cursor = 0usize;
    let mut replay_breakpoints = ReplayBreakpoints::default();
    let mut out = Vec::<u8>::new();

    handle_replay_command(
        "break line 11",
        &recording,
        &mut cursor,
        &mut replay_breakpoints,
        &mut out,
    );
    let set_text = String::from_utf8(out.clone()).expect("output should be utf-8");
    assert!(
        set_text.contains("line 13 (requested line 11)"),
        "expected resolved-line message, got: {set_text}"
    );

    out.clear();
    handle_replay_command(
        "continue",
        &recording,
        &mut cursor,
        &mut replay_breakpoints,
        &mut out,
    );
    assert_eq!(
        cursor, 1,
        "continue should pause at resolved executable line"
    );
}

#[test]
fn replay_offset_breakpoint_persists_until_cleared() {
    let recording = VmRecording {
        program: Program::new(vec![], vec![]),
        frames: vec![
            VmRecordingFrame {
                ip: 0,
                call_depth: 0,
                stack: vec![],
                locals: vec![],
            },
            VmRecordingFrame {
                ip: 1,
                call_depth: 0,
                stack: vec![],
                locals: vec![],
            },
            VmRecordingFrame {
                ip: 2,
                call_depth: 0,
                stack: vec![],
                locals: vec![],
            },
            VmRecordingFrame {
                ip: 1,
                call_depth: 0,
                stack: vec![],
                locals: vec![],
            },
            VmRecordingFrame {
                ip: 3,
                call_depth: 0,
                stack: vec![],
                locals: vec![],
            },
        ],
        terminal_status: Some(VmStatus::Halted),
    };
    let mut cursor = 0usize;
    let mut replay_breakpoints = ReplayBreakpoints::default();
    let mut out = Vec::<u8>::new();

    handle_replay_command(
        "break 1",
        &recording,
        &mut cursor,
        &mut replay_breakpoints,
        &mut out,
    );
    out.clear();
    handle_replay_command(
        "continue",
        &recording,
        &mut cursor,
        &mut replay_breakpoints,
        &mut out,
    );
    assert_eq!(
        cursor, 1,
        "first continue should stop at the first matching ip"
    );

    out.clear();
    handle_replay_command(
        "continue",
        &recording,
        &mut cursor,
        &mut replay_breakpoints,
        &mut out,
    );
    assert_eq!(
        cursor, 3,
        "second continue should stop at the next matching ip while breakpoint is still set"
    );
}

#[test]
fn replay_line_breakpoint_persists_until_cleared() {
    let program = Program::with_debug(
        vec![],
        vec![],
        Some(DebugInfo {
            source: None,
            lines: vec![
                crate::debug_info::LineInfo { offset: 0, line: 1 },
                crate::debug_info::LineInfo { offset: 1, line: 7 },
                crate::debug_info::LineInfo { offset: 2, line: 2 },
                crate::debug_info::LineInfo { offset: 3, line: 7 },
            ],
            functions: vec![],
            locals: vec![],
        }),
    );
    let recording = VmRecording {
        program,
        frames: vec![
            VmRecordingFrame {
                ip: 0,
                call_depth: 0,
                stack: vec![],
                locals: vec![],
            },
            VmRecordingFrame {
                ip: 1,
                call_depth: 0,
                stack: vec![],
                locals: vec![],
            },
            VmRecordingFrame {
                ip: 2,
                call_depth: 0,
                stack: vec![],
                locals: vec![],
            },
            VmRecordingFrame {
                ip: 3,
                call_depth: 0,
                stack: vec![],
                locals: vec![],
            },
            VmRecordingFrame {
                ip: 4,
                call_depth: 0,
                stack: vec![],
                locals: vec![],
            },
        ],
        terminal_status: Some(VmStatus::Halted),
    };
    let mut cursor = 0usize;
    let mut replay_breakpoints = ReplayBreakpoints::default();
    let mut out = Vec::<u8>::new();

    handle_replay_command(
        "break line 7",
        &recording,
        &mut cursor,
        &mut replay_breakpoints,
        &mut out,
    );
    out.clear();
    handle_replay_command(
        "continue",
        &recording,
        &mut cursor,
        &mut replay_breakpoints,
        &mut out,
    );
    assert_eq!(
        cursor, 1,
        "first continue should stop at the first matching line"
    );

    out.clear();
    handle_replay_command(
        "continue",
        &recording,
        &mut cursor,
        &mut replay_breakpoints,
        &mut out,
    );
    assert_eq!(
        cursor, 3,
        "second continue should stop at the next matching line while breakpoint is still set"
    );
}

#[test]
fn bridge_continue_stops_again_at_line_breakpoint() {
    let source = "let a = 1;\nlet b = 2;\nlet c = 3;\n".to_string();
    let program = Program::with_debug(
        vec![Value::Int(1), Value::Int(2), Value::Int(3)],
        vec![
            crate::vm::OpCode::Ldc as u8,
            0,
            0,
            0,
            0,
            crate::vm::OpCode::Stloc as u8,
            0,
            crate::vm::OpCode::Ldc as u8,
            1,
            0,
            0,
            0,
            crate::vm::OpCode::Stloc as u8,
            1,
            crate::vm::OpCode::Ldc as u8,
            2,
            0,
            0,
            0,
            crate::vm::OpCode::Stloc as u8,
            2,
            crate::vm::OpCode::Ret as u8,
        ],
        Some(DebugInfo {
            source: Some(source),
            lines: vec![
                crate::debug_info::LineInfo { offset: 0, line: 1 },
                crate::debug_info::LineInfo { offset: 7, line: 2 },
                crate::debug_info::LineInfo {
                    offset: 14,
                    line: 3,
                },
                crate::debug_info::LineInfo {
                    offset: 19,
                    line: 4,
                },
            ],
            functions: vec![],
            locals: vec![
                LocalInfo {
                    name: "a".to_string(),
                    index: 0,
                    declared_line: None,
                    last_line: None,
                },
                LocalInfo {
                    name: "b".to_string(),
                    index: 1,
                    declared_line: None,
                    last_line: None,
                },
                LocalInfo {
                    name: "c".to_string(),
                    index: 2,
                    declared_line: None,
                    last_line: None,
                },
            ],
        }),
    );
    let bridge = super::DebugCommandBridge::new();
    let mut debugger = Debugger::with_command_bridge(bridge.clone());
    debugger.stop_on_entry();

    let join = std::thread::spawn(move || {
        let mut vm = Vm::new(program.with_local_count(3));
        vm.run_with_debugger(&mut debugger)
            .expect("debugged vm run should succeed")
    });

    wait_for_bridge_attachment(&bridge, true);
    let set_response = bridge
        .execute("break line 3", Duration::from_millis(200))
        .expect("set breakpoint command should succeed");
    assert!(set_response.attached);

    let continue_response = bridge
        .execute("continue", Duration::from_millis(200))
        .expect("continue command should succeed");
    assert!(continue_response.resumed);
    assert!(!continue_response.attached);

    wait_for_bridge_attachment(&bridge, true);
    let where_response = bridge
        .execute("where", Duration::from_millis(200))
        .expect("where command should succeed");
    assert!(where_response.attached);
    assert!(
        where_response.output.contains("line 3"),
        "expected to pause at line 3, got: {}",
        where_response.output
    );

    let final_continue = bridge
        .execute("continue", Duration::from_millis(200))
        .expect("final continue should succeed");
    assert!(final_continue.resumed);
    assert!(!final_continue.attached);

    let status = join.join().expect("debugger thread should join");
    assert_eq!(status, VmStatus::Halted);
}

#[test]
fn public_replay_api_updates_cursor_and_reports_line() {
    let program = Program::with_debug(
        vec![],
        vec![],
        Some(DebugInfo {
            source: None,
            lines: vec![crate::debug_info::LineInfo {
                offset: 0,
                line: 11,
            }],
            functions: vec![],
            locals: vec![],
        }),
    );
    let recording = VmRecording {
        program,
        frames: vec![
            VmRecordingFrame {
                ip: 0,
                call_depth: 0,
                stack: vec![],
                locals: vec![],
            },
            VmRecordingFrame {
                ip: 5,
                call_depth: 0,
                stack: vec![],
                locals: vec![],
            },
        ],
        terminal_status: Some(VmStatus::Halted),
    };
    let mut state = super::VmRecordingReplayState::default();

    let response = super::run_recording_replay_command(&recording, &mut state, "step");
    assert!(!response.exited);
    assert_eq!(state.cursor, 1);
    assert!(response.at_end);
    assert_eq!(response.current_line, Some(11));
}

#[test]
fn handle_command_next_and_out_set_expected_step_modes() {
    let mut vm = Vm::new(Program::new(vec![], vec![crate::vm::OpCode::Ret as u8]));
    let mut out = Vec::<u8>::new();
    let mut breakpoints = HashSet::new();
    let mut line_breakpoints = HashSet::new();
    let mut step_mode = StepMode::Running;

    let action = handle_command(
        "next",
        &mut vm,
        &mut breakpoints,
        &mut line_breakpoints,
        &mut step_mode,
        &mut out,
    );
    assert_eq!(action, ReplAction::Break);
    assert!(
        matches!(step_mode, StepMode::StepOver { depth: 0, ip: 0 }),
        "unexpected step mode after next: {step_mode:?}"
    );

    let action = handle_command(
        "out",
        &mut vm,
        &mut breakpoints,
        &mut line_breakpoints,
        &mut step_mode,
        &mut out,
    );
    assert_eq!(action, ReplAction::Break);
    assert!(
        matches!(step_mode, StepMode::StepOut { depth: 0 }),
        "unexpected step mode after out: {step_mode:?}"
    );
}

#[test]
fn break_line_on_non_executable_source_line_resolves_forward() {
    let mut vm = Vm::new(Program::with_debug(
        vec![],
        vec![crate::vm::OpCode::Nop as u8, crate::vm::OpCode::Ret as u8],
        Some(DebugInfo {
            source: None,
            lines: vec![
                crate::debug_info::LineInfo { offset: 0, line: 8 },
                crate::debug_info::LineInfo {
                    offset: 1,
                    line: 13,
                },
            ],
            functions: vec![],
            locals: vec![],
        }),
    ));
    let mut out = Vec::<u8>::new();
    let mut breakpoints = HashSet::new();
    let mut line_breakpoints = HashSet::new();
    let mut step_mode = StepMode::Running;

    let action = handle_command(
        "break line 11",
        &mut vm,
        &mut breakpoints,
        &mut line_breakpoints,
        &mut step_mode,
        &mut out,
    );
    assert_eq!(action, ReplAction::Continue);
    assert!(
        line_breakpoints.contains(&13),
        "breakpoint should resolve to next executable line"
    );
    assert!(
        !line_breakpoints.contains(&11),
        "raw non-executable line should not remain as unresolved breakpoint"
    );
    let text = String::from_utf8(out).expect("output should be utf-8");
    assert!(
        text.contains("line breakpoint set at 13 (requested line 11)"),
        "expected resolved-line message, got: {text}"
    );
}

#[test]
fn handle_command_fuel_queries_and_updates_budget() {
    let mut vm = Vm::new(Program::new(vec![], vec![crate::vm::OpCode::Ret as u8]));
    vm.set_fuel(9);
    let mut out = Vec::<u8>::new();
    let mut breakpoints = HashSet::new();
    let mut line_breakpoints = HashSet::new();
    let mut step_mode = StepMode::Running;

    let action = handle_command(
        "fuel",
        &mut vm,
        &mut breakpoints,
        &mut line_breakpoints,
        &mut step_mode,
        &mut out,
    );
    assert_eq!(action, ReplAction::Continue);
    let text = String::from_utf8(out.clone()).expect("output should be utf-8");
    assert!(text.contains("fuel: 9"), "{text}");

    out.clear();
    handle_command(
        "fuel set 4",
        &mut vm,
        &mut breakpoints,
        &mut line_breakpoints,
        &mut step_mode,
        &mut out,
    );
    assert_eq!(vm.get_fuel(), Some(4));
    let text = String::from_utf8(out.clone()).expect("output should be utf-8");
    assert!(text.contains("fuel set to 4"), "{text}");

    out.clear();
    handle_command(
        "fuel interval 5",
        &mut vm,
        &mut breakpoints,
        &mut line_breakpoints,
        &mut step_mode,
        &mut out,
    );
    assert_eq!(vm.fuel_check_interval(), 5);
}

#[test]
fn handle_command_epoch_queries_and_updates_deadline() {
    let mut vm = Vm::new(Program::new(vec![], vec![crate::vm::OpCode::Ret as u8]));
    vm.set_epoch_deadline(2)
        .expect("setting epoch deadline should succeed");
    let mut out = Vec::<u8>::new();
    let mut breakpoints = HashSet::new();
    let mut line_breakpoints = HashSet::new();
    let mut step_mode = StepMode::Running;

    let action = handle_command(
        "epoch",
        &mut vm,
        &mut breakpoints,
        &mut line_breakpoints,
        &mut step_mode,
        &mut out,
    );
    assert_eq!(action, ReplAction::Continue);
    let text = String::from_utf8(out.clone()).expect("output should be utf-8");
    assert!(text.contains("epoch: current=0, deadline=2"), "{text}");

    out.clear();
    handle_command(
        "epoch tick 3",
        &mut vm,
        &mut breakpoints,
        &mut line_breakpoints,
        &mut step_mode,
        &mut out,
    );
    let text = String::from_utf8(out.clone()).expect("output should be utf-8");
    assert!(text.contains("epoch advanced by 3 to 3"), "{text}");

    out.clear();
    handle_command(
        "epoch deadline 4",
        &mut vm,
        &mut breakpoints,
        &mut line_breakpoints,
        &mut step_mode,
        &mut out,
    );
    assert_eq!(vm.epoch_deadline(), Some(7));
    let text = String::from_utf8(out.clone()).expect("output should be utf-8");
    assert!(
        text.contains("epoch deadline set 4 ticks beyond current epoch"),
        "{text}"
    );

    out.clear();
    handle_command(
        "epoch interval 5",
        &mut vm,
        &mut breakpoints,
        &mut line_breakpoints,
        &mut step_mode,
        &mut out,
    );
    assert_eq!(vm.epoch_check_interval(), 5);
}

#[test]
fn debugger_bridge_can_recover_from_out_of_fuel_by_adding_fuel() {
    let program = Program::new(
        vec![],
        vec![
            crate::vm::OpCode::Nop as u8,
            crate::vm::OpCode::Nop as u8,
            crate::vm::OpCode::Ret as u8,
        ],
    );
    let bridge = super::DebugCommandBridge::new();
    let mut debugger = Debugger::with_command_bridge(bridge.clone());

    let join = std::thread::spawn(move || {
        let mut vm = Vm::new(program);
        vm.set_fuel(1);
        vm.run_with_debugger(&mut debugger)
            .expect("debugged vm run should recover and succeed")
    });

    wait_for_bridge_attachment(&bridge, true);

    let fuel_response = bridge
        .execute("fuel", Duration::from_millis(200))
        .expect("fuel command should succeed");
    assert!(
        fuel_response.output.contains("fuel: 0"),
        "{}",
        fuel_response.output
    );

    let add_response = bridge
        .execute("fuel add 2", Duration::from_millis(200))
        .expect("fuel add command should succeed");
    assert!(
        add_response.output.contains("fuel added: 2"),
        "{}",
        add_response.output
    );

    let continue_response = bridge
        .execute("continue", Duration::from_millis(200))
        .expect("continue command should succeed");
    assert!(continue_response.resumed);
    assert!(!continue_response.attached);

    let status = join.join().expect("debugger thread should join");
    assert_eq!(status, VmStatus::Halted);
}

#[test]
fn debugger_bridge_can_recover_from_epoch_deadline_with_auto_rearm() {
    let program = Program::new(
        vec![],
        vec![
            crate::vm::OpCode::Nop as u8,
            crate::vm::OpCode::Nop as u8,
            crate::vm::OpCode::Ret as u8,
        ],
    );
    let bridge = super::DebugCommandBridge::new();
    let mut debugger = Debugger::with_command_bridge(bridge.clone());

    let join = std::thread::spawn(move || {
        let mut vm = Vm::new(program);
        vm.set_epoch_deadline(1)
            .expect("setting epoch deadline should succeed");
        assert_eq!(vm.increment_epoch(), 1);
        vm.run_with_debugger(&mut debugger)
            .expect("debugged vm run should recover and succeed")
    });

    wait_for_bridge_attachment(&bridge, true);

    let epoch_response = bridge
        .execute("epoch", Duration::from_millis(200))
        .expect("epoch command should succeed");
    assert!(
        epoch_response.output.contains("deadline=1"),
        "{}",
        epoch_response.output
    );

    let continue_response = bridge
        .execute("continue", Duration::from_millis(200))
        .expect("continue command should succeed");
    assert!(continue_response.resumed);
    assert!(!continue_response.attached);

    let status = join.join().expect("debugger thread should join");
    assert_eq!(status, VmStatus::Halted);
}

#[test]
fn replay_next_and_out_follow_call_depth_transitions() {
    let recording = VmRecording {
        program: Program::new(vec![], vec![]),
        frames: vec![
            VmRecordingFrame {
                ip: 10,
                call_depth: 0,
                stack: vec![],
                locals: vec![],
            },
            VmRecordingFrame {
                ip: 10,
                call_depth: 1,
                stack: vec![],
                locals: vec![],
            },
            VmRecordingFrame {
                ip: 11,
                call_depth: 1,
                stack: vec![],
                locals: vec![],
            },
            VmRecordingFrame {
                ip: 10,
                call_depth: 0,
                stack: vec![],
                locals: vec![],
            },
            VmRecordingFrame {
                ip: 12,
                call_depth: 0,
                stack: vec![],
                locals: vec![],
            },
        ],
        terminal_status: Some(VmStatus::Halted),
    };
    let mut replay_breakpoints = ReplayBreakpoints::default();
    let mut out = Vec::<u8>::new();

    let mut cursor = 0usize;
    handle_replay_command(
        "next",
        &recording,
        &mut cursor,
        &mut replay_breakpoints,
        &mut out,
    );
    assert_eq!(cursor, 4, "next should step over nested call frames");

    out.clear();
    cursor = 1;
    handle_replay_command(
        "out",
        &recording,
        &mut cursor,
        &mut replay_breakpoints,
        &mut out,
    );
    assert_eq!(cursor, 3, "out should stop when call depth decreases");
}

#[test]
fn replay_api_reports_empty_recording_without_crashing() {
    let recording = VmRecording {
        program: Program::new(vec![], vec![]),
        frames: vec![],
        terminal_status: None,
    };
    let mut state = super::VmRecordingReplayState::default();

    let response = super::run_recording_replay_command(&recording, &mut state, "continue");
    assert_eq!(response.output, "recording has no captured frames");
    assert!(response.at_end);
    assert!(!response.exited);
    assert_eq!(response.current_line, None);
}

#[test]
fn debug_command_bridge_reports_not_attached_timeout_and_closed_states() {
    let bridge = super::DebugCommandBridge::new();
    assert!(!bridge.status().attached);

    let not_attached = bridge
        .execute("where", Duration::from_millis(5))
        .expect_err("bridge should reject commands before attach");
    assert_eq!(not_attached, DebugCommandBridgeError::NotAttached);

    {
        let mut state = bridge
            .inner
            .state
            .lock()
            .expect("debug command bridge lock poisoned");
        state.attached = true;
    }
    let timeout = bridge
        .execute("where", Duration::from_millis(5))
        .expect_err("bridge should time out with no debugger repl consumer");
    assert_eq!(timeout, DebugCommandBridgeError::Timeout);

    bridge.close();
    let closed = bridge
        .execute("where", Duration::from_millis(5))
        .expect_err("closed bridge should reject commands");
    assert_eq!(closed, DebugCommandBridgeError::Closed);
    let status = bridge.status();
    assert!(!status.attached);
    assert_eq!(status.current_line, None);
}
