use std::collections::BTreeSet;

use crate::builtins::BuiltinFunction;
use crate::vm::{OpCode, Program, ValueType};

use super::cfg::{AotBasicBlock, AotBlockTerminal, AotCfg, AotCfgError, AotCfgRegion, build_cfg};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AotProgram {
    pub(crate) entry_ip: usize,
    pub(crate) regions: Vec<AotCfgRegion>,
    pub(crate) blocks: Vec<AotIrBlock>,
    pub(crate) resume_ips: Vec<usize>,
}

impl AotProgram {
    pub(crate) fn block(&self, start_ip: usize) -> Option<&AotIrBlock> {
        self.blocks.iter().find(|block| block.start_ip == start_ip)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AotIrBlock {
    pub(crate) start_ip: usize,
    pub(crate) end_ip: usize,
    pub(crate) instructions: Vec<AotInstruction>,
    pub(crate) terminal: AotBlockTerminal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AotConcatKind {
    String,
    Bytes,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AotTextBytesKind {
    String,
    Bytes,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AotBytesCodecKind {
    FromArrayU8,
    ToArrayU8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AotInstruction {
    Nop,
    Ldc { const_index: u32 },
    Add,
    IAdd,
    FAdd,
    Concat(AotConcatKind),
    Len(AotTextBytesKind),
    Slice(AotTextBytesKind),
    Get(AotTextBytesKind),
    HasBytes,
    BytesCodec(AotBytesCodecKind),
    ArraySet,
    ArrayPush,
    MapSet,
    Sub,
    ISub,
    FSub,
    Mul,
    IMul,
    FMul,
    Div,
    IDiv,
    FDiv,
    Mod,
    IMod,
    FMod,
    Shl,
    Shr,
    Lshr,
    And,
    Or,
    Not,
    Neg,
    INeg,
    FNeg,
    Ceq,
    FCeq,
    Clt,
    FClt,
    Cgt,
    FCgt,
    Pop,
    Dup,
    Ldloc { index: u8 },
    LdlocOwned { index: u8 },
    Stloc { index: u8 },
    Call(AotCall),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AotCall {
    pub(crate) index: u16,
    pub(crate) argc: u8,
    pub(crate) call_ip: usize,
    pub(crate) resume_ip: usize,
    pub(crate) dispatch: AotCallDispatch,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AotCallDispatch {
    Builtin,
    HostImport,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AotLowerError {
    Cfg(AotCfgError),
    InvalidOpcode {
        ip: usize,
        opcode: u8,
    },
    InvalidImmediate {
        ip: usize,
        opcode: OpCode,
        kind: &'static str,
    },
    BytecodeBounds {
        ip: usize,
    },
}

impl From<AotCfgError> for AotLowerError {
    fn from(value: AotCfgError) -> Self {
        Self::Cfg(value)
    }
}

pub(crate) fn lower_program(program: &Program) -> Result<AotProgram, AotLowerError> {
    let cfg = build_cfg(program)?;
    lower_from_cfg(program, &cfg)
}

fn lower_from_cfg(program: &Program, cfg: &AotCfg) -> Result<AotProgram, AotLowerError> {
    let mut blocks = Vec::with_capacity(cfg.blocks.len());
    let mut resume_ips = BTreeSet::new();
    resume_ips.insert(cfg.entry_ip);

    for block in &cfg.blocks {
        resume_ips.insert(block.start_ip);
        let lowered_block = lower_block(program, block, &mut resume_ips)?;
        blocks.push(lowered_block);
    }

    Ok(AotProgram {
        entry_ip: cfg.entry_ip,
        regions: cfg.regions.clone(),
        blocks,
        resume_ips: resume_ips.into_iter().collect(),
    })
}

fn lower_block(
    program: &Program,
    block: &AotBasicBlock,
    resume_ips: &mut BTreeSet<usize>,
) -> Result<AotIrBlock, AotLowerError> {
    let code = &program.code;
    let mut instructions = Vec::new();
    let mut provenance = Some(Vec::<AotStackProvenance>::new());
    let mut local_nulls = vec![false; program.local_count];
    let mut ip = block.start_ip;

    while ip < block.end_ip {
        let opcode_byte = *code.get(ip).ok_or(AotLowerError::BytecodeBounds { ip })?;
        let opcode = OpCode::try_from(opcode_byte).map_err(|_| AotLowerError::InvalidOpcode {
            ip,
            opcode: opcode_byte,
        })?;
        let next_ip = ip
            .checked_add(1 + opcode.operand_len())
            .ok_or(AotLowerError::BytecodeBounds { ip })?;

        if is_explicit_terminal_opcode(code, block, ip, opcode, next_ip)? {
            break;
        }

        match opcode {
            OpCode::Nop => instructions.push(AotInstruction::Nop),
            OpCode::Ldc => {
                let const_index =
                    read_u32(code, ip + 1).ok_or(AotLowerError::InvalidImmediate {
                        ip,
                        opcode,
                        kind: "ldc index",
                    })?;
                instructions.push(AotInstruction::Ldc { const_index });
                if let Some(stack) = provenance.as_mut() {
                    let known_null = matches!(
                        program.constants.get(const_index as usize),
                        Some(crate::Value::Null)
                    );
                    stack.push(AotStackProvenance::Constant { known_null });
                }
            }
            OpCode::Add => instructions.push(typed_instruction(program, ip, opcode)),
            OpCode::Sub => instructions.push(typed_instruction(program, ip, opcode)),
            OpCode::Mul => instructions.push(typed_instruction(program, ip, opcode)),
            OpCode::Div => instructions.push(typed_instruction(program, ip, opcode)),
            OpCode::Mod => instructions.push(typed_instruction(program, ip, opcode)),
            OpCode::Shl => instructions.push(AotInstruction::Shl),
            OpCode::Shr => instructions.push(AotInstruction::Shr),
            OpCode::Lshr => instructions.push(AotInstruction::Lshr),
            OpCode::And => instructions.push(AotInstruction::And),
            OpCode::Or => instructions.push(AotInstruction::Or),
            OpCode::Not => instructions.push(AotInstruction::Not),
            OpCode::Neg => instructions.push(typed_instruction(program, ip, opcode)),
            OpCode::Ceq => instructions.push(typed_instruction(program, ip, opcode)),
            OpCode::Clt => instructions.push(typed_instruction(program, ip, opcode)),
            OpCode::Cgt => instructions.push(typed_instruction(program, ip, opcode)),
            OpCode::Pop => instructions.push(AotInstruction::Pop),
            OpCode::Dup => instructions.push(AotInstruction::Dup),
            OpCode::Ldloc => {
                let index = read_u8(code, ip + 1).ok_or(AotLowerError::InvalidImmediate {
                    ip,
                    opcode,
                    kind: "ldloc index",
                })?;
                let instruction_index = instructions.len();
                instructions.push(AotInstruction::Ldloc { index });
                if let Some(stack) = provenance.as_mut() {
                    stack.push(AotStackProvenance::Local {
                        index,
                        instruction_index,
                    });
                }
            }
            OpCode::Stloc => {
                let index = read_u8(code, ip + 1).ok_or(AotLowerError::InvalidImmediate {
                    ip,
                    opcode,
                    kind: "stloc index",
                })?;
                instructions.push(AotInstruction::Stloc { index });
                if let Some(stack) = provenance.as_mut() {
                    if let Some(value) = stack.pop() {
                        local_nulls[index as usize] = value.known_null();
                    } else {
                        provenance = None;
                    }
                }
            }
            OpCode::Call => {
                let index = read_u16(code, ip + 1).ok_or(AotLowerError::InvalidImmediate {
                    ip,
                    opcode,
                    kind: "call index",
                })?;
                let argc = read_u8(code, ip + 3).ok_or(AotLowerError::InvalidImmediate {
                    ip,
                    opcode,
                    kind: "call argc",
                })?;
                if let Some(builtin) = BuiltinFunction::from_call_index(index) {
                    if !builtin.accepts_arity(argc) {
                        return Err(AotLowerError::InvalidImmediate {
                            ip,
                            opcode,
                            kind: "builtin call arity",
                        });
                    }
                    if let Some((instruction, source_instruction)) =
                        delayed_move_collection_instruction(
                            program,
                            code,
                            ip,
                            next_ip,
                            builtin,
                            argc,
                            provenance.as_ref(),
                            &local_nulls,
                        )
                    {
                        let source_local = match instructions.get(source_instruction) {
                            Some(AotInstruction::Ldloc { index }) => *index,
                            _ => unreachable!("validated delayed-move source"),
                        };
                        instructions[source_instruction] = AotInstruction::LdlocOwned {
                            index: source_local,
                        };
                        instructions.push(instruction);
                    } else if let Some(instruction) =
                        typed_builtin_instruction(program, ip, builtin)
                    {
                        instructions.push(instruction);
                    } else {
                        let call = AotCall {
                            index,
                            argc,
                            call_ip: ip,
                            resume_ip: next_ip,
                            dispatch: AotCallDispatch::Builtin,
                        };
                        resume_ips.insert(call.call_ip);
                        resume_ips.insert(call.resume_ip);
                        instructions.push(AotInstruction::Call(call));
                    }
                } else {
                    let call = AotCall {
                        index,
                        argc,
                        call_ip: ip,
                        resume_ip: next_ip,
                        dispatch: AotCallDispatch::HostImport,
                    };
                    resume_ips.insert(call.call_ip);
                    resume_ips.insert(call.resume_ip);
                    instructions.push(AotInstruction::Call(call));
                }
                apply_call_provenance(&mut provenance, argc, index);
            }

            OpCode::CallValue => {
                return Err(AotLowerError::InvalidImmediate {
                    ip,
                    opcode,
                    kind: "script callable frame operation requires runtime lowering",
                });
            }
            OpCode::Ret | OpCode::Br | OpCode::Brfalse => {
                return Err(AotLowerError::InvalidImmediate {
                    ip,
                    opcode,
                    kind: "unexpected terminal in lowered instruction stream",
                });
            }
        }

        apply_non_call_stack_effect(&mut provenance, opcode);
        ip = next_ip;
    }

    Ok(AotIrBlock {
        start_ip: block.start_ip,
        end_ip: block.end_ip,
        instructions,
        terminal: block.terminal.clone(),
    })
}

#[derive(Clone, Copy)]
enum AotStackProvenance {
    Derived,
    Local { index: u8, instruction_index: usize },
    Constant { known_null: bool },
}

impl AotStackProvenance {
    fn known_null(self) -> bool {
        matches!(self, Self::Constant { known_null: true })
    }
}

#[allow(clippy::too_many_arguments)]
fn delayed_move_collection_instruction(
    program: &Program,
    code: &[u8],
    call_ip: usize,
    next_ip: usize,
    builtin: BuiltinFunction,
    argc: u8,
    provenance: Option<&Vec<AotStackProvenance>>,
    local_nulls: &[bool],
) -> Option<(AotInstruction, usize)> {
    let expected_argc = match builtin {
        BuiltinFunction::Set => 3,
        BuiltinFunction::ArrayPush => 2,
        _ => return None,
    };
    if argc != expected_argc {
        return None;
    }
    let stack = provenance?;
    let args = stack.get(stack.len().checked_sub(argc as usize)?..)?;
    let AotStackProvenance::Local {
        index: source_local,
        instruction_index,
    } = args[0]
    else {
        return None;
    };
    if !local_nulls
        .get(source_local as usize)
        .copied()
        .unwrap_or(false)
        || args[1..].iter().any(
            |arg| matches!(arg, AotStackProvenance::Local { index, .. } if *index == source_local),
        )
        || code.get(next_ip).copied() != Some(OpCode::Stloc as u8)
        || read_u8(code, next_ip + 1) != Some(source_local)
    {
        return None;
    }
    match builtin {
        BuiltinFunction::ArrayPush => Some((AotInstruction::ArrayPush, instruction_index)),
        BuiltinFunction::Set => match program
            .type_map
            .as_ref()
            .and_then(|type_map| type_map.operand_types.get(&call_ip))
            .map(|types| types.0)
        {
            Some(ValueType::Array) => Some((AotInstruction::ArraySet, instruction_index)),
            Some(ValueType::Map) => Some((AotInstruction::MapSet, instruction_index)),
            _ => None,
        },
        _ => None,
    }
}

fn apply_call_provenance(
    provenance: &mut Option<Vec<AotStackProvenance>>,
    argc: u8,
    call_index: u16,
) {
    let Some(stack) = provenance.as_mut() else {
        return;
    };
    if stack.len() < argc as usize {
        *provenance = None;
        return;
    }
    stack.truncate(stack.len() - argc as usize);
    if !matches!(
        BuiltinFunction::from_call_index(call_index),
        Some(BuiltinFunction::Assert | BuiltinFunction::DetachLocal)
    ) {
        stack.push(AotStackProvenance::Derived);
    }
}

fn apply_non_call_stack_effect(provenance: &mut Option<Vec<AotStackProvenance>>, opcode: OpCode) {
    let Some(stack) = provenance.as_mut() else {
        return;
    };
    let (pops, pushes) = match opcode {
        OpCode::Add
        | OpCode::Sub
        | OpCode::Mul
        | OpCode::Div
        | OpCode::Mod
        | OpCode::Shl
        | OpCode::Shr
        | OpCode::Lshr
        | OpCode::And
        | OpCode::Or
        | OpCode::Ceq
        | OpCode::Clt
        | OpCode::Cgt => (2, 1),
        OpCode::Not | OpCode::Neg => (1, 1),
        OpCode::Pop => (1, 0),
        OpCode::Dup => {
            if let Some(value) = stack.last().copied() {
                stack.push(value);
            } else {
                *provenance = None;
            }
            return;
        }
        _ => return,
    };
    if stack.len() < pops {
        *provenance = None;
        return;
    }
    stack.truncate(stack.len() - pops);
    stack.extend(std::iter::repeat_n(AotStackProvenance::Derived, pushes));
}

fn is_explicit_terminal_opcode(
    code: &[u8],
    block: &AotBasicBlock,
    ip: usize,
    opcode: OpCode,
    next_ip: usize,
) -> Result<bool, AotLowerError> {
    if next_ip != block.end_ip {
        return Ok(false);
    }

    match &block.terminal {
        AotBlockTerminal::Return => Ok(opcode == OpCode::Ret),
        AotBlockTerminal::Jump { target_ip } => {
            Ok(opcode == OpCode::Br && read_u32(code, ip + 1) == Some(*target_ip as u32))
        }
        AotBlockTerminal::ConditionalJump {
            target_ip,
            fallthrough_ip,
        } => Ok(opcode == OpCode::Brfalse
            && read_u32(code, ip + 1) == Some(*target_ip as u32)
            && next_ip == *fallthrough_ip),
        AotBlockTerminal::Fallthrough { .. } => Ok(false),
        AotBlockTerminal::CallValue {
            argc,
            call_ip,
            resume_ip,
        } => Ok(opcode == OpCode::CallValue
            && read_u8(code, ip + 1) == Some(*argc)
            && ip == *call_ip
            && next_ip == *resume_ip),
        AotBlockTerminal::InterpreterExit { exit_ip } => {
            Ok(opcode == OpCode::CallValue && ip == *exit_ip)
        }
        AotBlockTerminal::Stop => Ok(false),
    }
}

fn read_u8(code: &[u8], offset: usize) -> Option<u8> {
    code.get(offset).copied()
}

fn read_u16(code: &[u8], offset: usize) -> Option<u16> {
    let bytes = code.get(offset..offset + 2)?;
    Some(u16::from_le_bytes(bytes.try_into().ok()?))
}

fn read_u32(code: &[u8], offset: usize) -> Option<u32> {
    let bytes = code.get(offset..offset + 4)?;
    Some(u32::from_le_bytes(bytes.try_into().ok()?))
}

fn typed_instruction(program: &Program, ip: usize, opcode: OpCode) -> AotInstruction {
    let operand_types = program
        .type_map
        .as_ref()
        .and_then(|type_map| type_map.operand_types.get(&ip))
        .copied()
        .unwrap_or((ValueType::Unknown, ValueType::Unknown));
    match (opcode, operand_types) {
        (OpCode::Add, (ValueType::Int, ValueType::Int)) => AotInstruction::IAdd,
        (OpCode::Add, (ValueType::Float, ValueType::Float)) => AotInstruction::FAdd,
        (OpCode::Add, (ValueType::String, ValueType::String)) => {
            AotInstruction::Concat(AotConcatKind::String)
        }
        (OpCode::Add, (ValueType::Bytes, ValueType::Bytes)) => {
            AotInstruction::Concat(AotConcatKind::Bytes)
        }
        (OpCode::Sub, (ValueType::Int, ValueType::Int)) => AotInstruction::ISub,
        (OpCode::Sub, (ValueType::Float, ValueType::Float)) => AotInstruction::FSub,
        (OpCode::Mul, (ValueType::Int, ValueType::Int)) => AotInstruction::IMul,
        (OpCode::Mul, (ValueType::Float, ValueType::Float)) => AotInstruction::FMul,
        (OpCode::Div, (ValueType::Int, ValueType::Int)) => AotInstruction::IDiv,
        (OpCode::Div, (ValueType::Float, ValueType::Float)) => AotInstruction::FDiv,
        (OpCode::Mod, (ValueType::Int, ValueType::Int)) => AotInstruction::IMod,
        (OpCode::Mod, (ValueType::Float, ValueType::Float)) => AotInstruction::FMod,
        (OpCode::Neg, (ValueType::Int, _)) => AotInstruction::INeg,
        (OpCode::Neg, (ValueType::Float, _)) => AotInstruction::FNeg,
        (OpCode::Ceq, (ValueType::Float, ValueType::Float)) => AotInstruction::FCeq,
        (OpCode::Clt, (ValueType::Float, ValueType::Float)) => AotInstruction::FClt,
        (OpCode::Cgt, (ValueType::Float, ValueType::Float)) => AotInstruction::FCgt,
        (OpCode::Add, _) => AotInstruction::Add,
        (OpCode::Sub, _) => AotInstruction::Sub,
        (OpCode::Mul, _) => AotInstruction::Mul,
        (OpCode::Div, _) => AotInstruction::Div,
        (OpCode::Mod, _) => AotInstruction::Mod,
        (OpCode::Neg, _) => AotInstruction::Neg,
        (OpCode::Ceq, _) => AotInstruction::Ceq,
        (OpCode::Clt, _) => AotInstruction::Clt,
        (OpCode::Cgt, _) => AotInstruction::Cgt,
        _ => unreachable!("typed_instruction only supports arithmetic and comparison opcodes"),
    }
}

fn typed_builtin_instruction(
    program: &Program,
    call_ip: usize,
    builtin: BuiltinFunction,
) -> Option<AotInstruction> {
    let (lhs, _) = program
        .type_map
        .as_ref()
        .and_then(|type_map| type_map.operand_types.get(&call_ip))
        .copied()
        .unwrap_or((ValueType::Unknown, ValueType::Unknown));

    match builtin {
        BuiltinFunction::Len => match lhs {
            ValueType::String => Some(AotInstruction::Len(AotTextBytesKind::String)),
            ValueType::Bytes => Some(AotInstruction::Len(AotTextBytesKind::Bytes)),
            _ => None,
        },
        BuiltinFunction::Slice => match lhs {
            ValueType::String => Some(AotInstruction::Slice(AotTextBytesKind::String)),
            ValueType::Bytes => Some(AotInstruction::Slice(AotTextBytesKind::Bytes)),
            _ => None,
        },
        BuiltinFunction::Get => match lhs {
            ValueType::String => Some(AotInstruction::Get(AotTextBytesKind::String)),
            ValueType::Bytes => Some(AotInstruction::Get(AotTextBytesKind::Bytes)),
            _ => None,
        },
        BuiltinFunction::Has if lhs == ValueType::Bytes => Some(AotInstruction::HasBytes),
        BuiltinFunction::BytesFromArrayU8 if lhs == ValueType::Array => {
            Some(AotInstruction::BytesCodec(AotBytesCodecKind::FromArrayU8))
        }
        BuiltinFunction::BytesToArrayU8 if lhs == ValueType::Bytes => {
            Some(AotInstruction::BytesCodec(AotBytesCodecKind::ToArrayU8))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BytecodeBuilder, TypeMap, Value};

    fn patch_branch_target(code: &mut [u8], instr_ip: u32, target: u32) {
        let start = instr_ip as usize + 1;
        code[start..start + 4].copy_from_slice(&target.to_le_bytes());
    }

    #[test]
    fn aot_ir_lowers_diamond_control_flow() {
        let mut bc = BytecodeBuilder::new();
        bc.ldc(0);
        let branch_ip = bc.position();
        bc.brfalse(0);
        let true_ip = bc.position();
        bc.ldloc(1);
        let jump_ip = bc.position();
        bc.br(0);
        let false_ip = bc.position();
        bc.ldloc(2);
        let join_ip = bc.position();
        bc.ret();

        let mut code = bc.finish();
        patch_branch_target(&mut code, branch_ip, false_ip);
        patch_branch_target(&mut code, jump_ip, join_ip);

        let program = Program::new(
            vec![Value::Bool(true), Value::Int(10), Value::Int(20)],
            code,
        );
        let lowered = lower_program(&program).expect("lowering should succeed");

        assert_eq!(
            lowered
                .blocks
                .iter()
                .map(|block| block.start_ip)
                .collect::<Vec<_>>(),
            vec![0, true_ip as usize, false_ip as usize, join_ip as usize]
        );
        assert_eq!(
            lowered.block(0).expect("entry block").instructions,
            vec![AotInstruction::Ldc { const_index: 0 }]
        );
        assert_eq!(
            lowered
                .block(true_ip as usize)
                .expect("true block")
                .instructions,
            vec![AotInstruction::Ldloc { index: 1 }]
        );
        assert_eq!(
            lowered
                .block(false_ip as usize)
                .expect("false block")
                .instructions,
            vec![AotInstruction::Ldloc { index: 2 }]
        );
        assert_eq!(
            lowered
                .block(join_ip as usize)
                .expect("join block")
                .instructions,
            Vec::<AotInstruction>::new()
        );
    }

    #[test]
    fn aot_ir_tracks_call_resume_ips() {
        let mut bc = BytecodeBuilder::new();
        bc.ldc(0);
        bc.ldc(1);
        let call_ip = bc.position();
        bc.call(1024, 2);
        let resume_ip = bc.position();
        bc.ret();

        let program = Program::new(vec![Value::Int(10), Value::Int(20)], bc.finish());
        let lowered = lower_program(&program).expect("lowering should succeed");
        let block = lowered.block(0).expect("entry block");

        assert_eq!(
            block.instructions,
            vec![
                AotInstruction::Ldc { const_index: 0 },
                AotInstruction::Ldc { const_index: 1 },
                AotInstruction::Call(AotCall {
                    index: 1024,
                    argc: 2,
                    call_ip: call_ip as usize,
                    resume_ip: resume_ip as usize,
                    dispatch: AotCallDispatch::HostImport,
                }),
            ]
        );
        assert_eq!(
            lowered.resume_ips,
            vec![0, call_ip as usize, resume_ip as usize]
        );
    }

    #[test]
    fn aot_ir_uses_type_map_for_specialized_ops() {
        let mut bc = BytecodeBuilder::new();
        bc.ldloc(0);
        bc.ldloc(1);
        bc.add();
        bc.ldloc(2);
        bc.neg();
        bc.ldloc(3);
        bc.ldloc(4);
        bc.ceq();
        bc.ret();

        let mut program = Program::new(
            vec![
                Value::Float(1.0),
                Value::Float(2.0),
                Value::Int(7),
                Value::Float(3.0),
                Value::Float(3.0),
            ],
            bc.finish(),
        );
        let mut type_map = TypeMap::default();
        type_map
            .operand_types
            .insert(4, (ValueType::Float, ValueType::Float));
        type_map
            .operand_types
            .insert(7, (ValueType::Int, ValueType::Unknown));
        type_map
            .operand_types
            .insert(12, (ValueType::Float, ValueType::Float));
        program.type_map = Some(type_map);

        let lowered = lower_program(&program).expect("lowering should succeed");
        let block = lowered.block(0).expect("entry block");

        assert_eq!(
            block.instructions,
            vec![
                AotInstruction::Ldloc { index: 0 },
                AotInstruction::Ldloc { index: 1 },
                AotInstruction::FAdd,
                AotInstruction::Ldloc { index: 2 },
                AotInstruction::INeg,
                AotInstruction::Ldloc { index: 3 },
                AotInstruction::Ldloc { index: 4 },
                AotInstruction::FCeq,
            ]
        );
    }

    #[test]
    fn aot_ir_lowers_typed_string_and_bytes_steps() {
        let mut bc = BytecodeBuilder::new();
        bc.ldc(0);
        bc.ldc(1);
        let string_concat_ip = bc.position();
        bc.add();

        bc.ldc(0);
        let string_len_ip = bc.position();
        bc.call(BuiltinFunction::Len.call_index(), 1);

        bc.ldc(0);
        bc.ldc(2);
        bc.ldc(3);
        let string_slice_ip = bc.position();
        bc.call(BuiltinFunction::Slice.call_index(), 3);

        bc.ldc(0);
        bc.ldc(3);
        let string_get_ip = bc.position();
        bc.call(BuiltinFunction::Get.call_index(), 2);

        bc.ldc(4);
        bc.ldc(5);
        let bytes_concat_ip = bc.position();
        bc.add();

        bc.ldc(6);
        let bytes_len_ip = bc.position();
        bc.call(BuiltinFunction::Len.call_index(), 1);

        bc.ldc(6);
        bc.ldc(3);
        bc.ldc(7);
        let bytes_slice_ip = bc.position();
        bc.call(BuiltinFunction::Slice.call_index(), 3);

        bc.ldc(6);
        bc.ldc(3);
        let bytes_get_ip = bc.position();
        bc.call(BuiltinFunction::Get.call_index(), 2);

        bc.ldc(6);
        bc.ldc(7);
        let bytes_has_ip = bc.position();
        bc.call(BuiltinFunction::Has.call_index(), 2);

        bc.ldc(8);
        let bytes_from_array_ip = bc.position();
        bc.call(BuiltinFunction::BytesFromArrayU8.call_index(), 1);

        bc.ldc(6);
        let bytes_to_array_ip = bc.position();
        bc.call(BuiltinFunction::BytesToArrayU8.call_index(), 1);
        bc.ret();

        let mut program = Program::new(
            vec![
                Value::string("ab"),
                Value::string("c"),
                Value::Int(0),
                Value::Int(1),
                Value::bytes(vec![1, 2]),
                Value::bytes(vec![3]),
                Value::bytes(vec![1, 2, 3]),
                Value::Int(2),
                Value::array(vec![Value::Int(1), Value::Int(2), Value::Int(3)]),
            ],
            bc.finish(),
        );
        let mut type_map = TypeMap::default();
        type_map.operand_types.insert(
            string_concat_ip as usize,
            (ValueType::String, ValueType::String),
        );
        type_map.operand_types.insert(
            string_len_ip as usize,
            (ValueType::String, ValueType::Unknown),
        );
        type_map.operand_types.insert(
            string_slice_ip as usize,
            (ValueType::String, ValueType::Int),
        );
        type_map
            .operand_types
            .insert(string_get_ip as usize, (ValueType::String, ValueType::Int));
        type_map.operand_types.insert(
            bytes_concat_ip as usize,
            (ValueType::Bytes, ValueType::Bytes),
        );
        type_map.operand_types.insert(
            bytes_len_ip as usize,
            (ValueType::Bytes, ValueType::Unknown),
        );
        type_map
            .operand_types
            .insert(bytes_slice_ip as usize, (ValueType::Bytes, ValueType::Int));
        type_map
            .operand_types
            .insert(bytes_get_ip as usize, (ValueType::Bytes, ValueType::Int));
        type_map
            .operand_types
            .insert(bytes_has_ip as usize, (ValueType::Bytes, ValueType::Int));
        type_map.operand_types.insert(
            bytes_from_array_ip as usize,
            (ValueType::Array, ValueType::Unknown),
        );
        type_map.operand_types.insert(
            bytes_to_array_ip as usize,
            (ValueType::Bytes, ValueType::Unknown),
        );
        program.type_map = Some(type_map);

        let lowered = lower_program(&program).expect("lowering should succeed");
        let block = lowered.block(0).expect("entry block");

        assert_eq!(
            block.instructions,
            vec![
                AotInstruction::Ldc { const_index: 0 },
                AotInstruction::Ldc { const_index: 1 },
                AotInstruction::Concat(AotConcatKind::String),
                AotInstruction::Ldc { const_index: 0 },
                AotInstruction::Len(AotTextBytesKind::String),
                AotInstruction::Ldc { const_index: 0 },
                AotInstruction::Ldc { const_index: 2 },
                AotInstruction::Ldc { const_index: 3 },
                AotInstruction::Slice(AotTextBytesKind::String),
                AotInstruction::Ldc { const_index: 0 },
                AotInstruction::Ldc { const_index: 3 },
                AotInstruction::Get(AotTextBytesKind::String),
                AotInstruction::Ldc { const_index: 4 },
                AotInstruction::Ldc { const_index: 5 },
                AotInstruction::Concat(AotConcatKind::Bytes),
                AotInstruction::Ldc { const_index: 6 },
                AotInstruction::Len(AotTextBytesKind::Bytes),
                AotInstruction::Ldc { const_index: 6 },
                AotInstruction::Ldc { const_index: 3 },
                AotInstruction::Ldc { const_index: 7 },
                AotInstruction::Slice(AotTextBytesKind::Bytes),
                AotInstruction::Ldc { const_index: 6 },
                AotInstruction::Ldc { const_index: 3 },
                AotInstruction::Get(AotTextBytesKind::Bytes),
                AotInstruction::Ldc { const_index: 6 },
                AotInstruction::Ldc { const_index: 7 },
                AotInstruction::HasBytes,
                AotInstruction::Ldc { const_index: 8 },
                AotInstruction::BytesCodec(AotBytesCodecKind::FromArrayU8),
                AotInstruction::Ldc { const_index: 6 },
                AotInstruction::BytesCodec(AotBytesCodecKind::ToArrayU8),
            ]
        );
    }

    #[test]
    fn aot_ir_uses_existing_call_opcode_for_callable_binding() {
        let compiled = crate::compile_source_for_repl(
            r#"
                let delta = 1;
                let function = |value| value + delta;
                function(41);
            "#,
        )
        .expect("closure source should compile");
        let lowered = lower_program(&compiled.program).expect("lowering should succeed");

        assert_eq!(lowered.regions.len(), 2);
        assert!(lowered.blocks.iter().any(|block| {
            block.instructions.iter().any(|instruction| {
                matches!(
                    instruction,
                    AotInstruction::Call(AotCall {
                        index,
                        argc: 2,
                        dispatch: AotCallDispatch::Builtin,
                        ..
                    }) if *index == BuiltinFunction::BindCallable.call_index()
                )
            })
        }));
        assert!(
            lowered
                .blocks
                .iter()
                .any(|block| matches!(block.terminal, AotBlockTerminal::CallValue { argc: 1, .. }))
        );
    }

    #[test]
    fn aot_ir_preserves_loop_terminal_shape() {
        let mut bc = BytecodeBuilder::new();
        bc.ldc(0);
        bc.stloc(0);
        let loop_ip = bc.position();
        bc.ldloc(0);
        bc.ldc(1);
        bc.add();
        bc.stloc(0);
        bc.ldloc(0);
        bc.ldc(2);
        bc.ceq();
        let branch_ip = bc.position();
        bc.brfalse(0);
        let exit_ip = bc.position();
        bc.ret();

        let mut code = bc.finish();
        patch_branch_target(&mut code, branch_ip, loop_ip);

        let program = Program::new(vec![Value::Int(0), Value::Int(1), Value::Int(4)], code);
        let lowered = lower_program(&program).expect("lowering should succeed");

        assert_eq!(
            lowered
                .block(loop_ip as usize)
                .expect("loop block")
                .instructions,
            vec![
                AotInstruction::Ldloc { index: 0 },
                AotInstruction::Ldc { const_index: 1 },
                AotInstruction::Add,
                AotInstruction::Stloc { index: 0 },
                AotInstruction::Ldloc { index: 0 },
                AotInstruction::Ldc { const_index: 2 },
                AotInstruction::Ceq,
            ]
        );
        assert_eq!(
            lowered
                .block(loop_ip as usize)
                .expect("loop block")
                .terminal,
            AotBlockTerminal::ConditionalJump {
                target_ip: loop_ip as usize,
                fallthrough_ip: exit_ip as usize,
            }
        );
    }
}
