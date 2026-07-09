use std::collections::BTreeSet;

use crate::vm::{OpCode, Program};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AotCfg {
    pub(crate) entry_ip: usize,
    pub(crate) blocks: Vec<AotBasicBlock>,
}

impl AotCfg {
    pub(crate) fn block(&self, start_ip: usize) -> Option<&AotBasicBlock> {
        self.blocks.iter().find(|block| block.start_ip == start_ip)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AotBasicBlock {
    pub(crate) start_ip: usize,
    pub(crate) end_ip: usize,
    pub(crate) terminal: AotBlockTerminal,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AotBlockTerminal {
    Return,
    Jump {
        target_ip: usize,
    },
    ConditionalJump {
        target_ip: usize,
        fallthrough_ip: usize,
    },
    Fallthrough {
        next_ip: usize,
    },
    Stop,
}

impl AotBlockTerminal {
    pub(crate) fn successor_ips(&self) -> Vec<usize> {
        match self {
            Self::Return | Self::Stop => Vec::new(),
            Self::Jump { target_ip } => vec![*target_ip],
            Self::ConditionalJump {
                target_ip,
                fallthrough_ip,
            } => vec![*target_ip, *fallthrough_ip],
            Self::Fallthrough { next_ip } => vec![*next_ip],
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AotCfgError {
    InvalidOpcode {
        ip: usize,
        opcode: u8,
    },
    TruncatedInstruction {
        ip: usize,
        opcode: OpCode,
        expected_operand_bytes: usize,
        available_operand_bytes: usize,
    },
    InvalidJumpTarget {
        ip: usize,
        target: usize,
    },
}

pub(crate) fn build_cfg(program: &Program) -> Result<AotCfg, AotCfgError> {
    build_cfg_from_code(&program.code)
}

fn build_cfg_from_code(code: &[u8]) -> Result<AotCfg, AotCfgError> {
    if code.is_empty() {
        return Ok(AotCfg {
            entry_ip: 0,
            blocks: Vec::new(),
        });
    }

    let block_starts = collect_block_starts(code)?;
    let mut blocks = Vec::with_capacity(block_starts.len());

    for (index, start_ip) in block_starts.iter().copied().enumerate() {
        let next_block_start = block_starts.get(index + 1).copied();
        let mut ip = start_ip;
        loop {
            let (opcode, next_ip) = decode_instruction_bounds(code, ip)?;
            let terminal = match opcode {
                OpCode::Ret => Some(AotBlockTerminal::Return),
                OpCode::Br => Some(AotBlockTerminal::Jump {
                    target_ip: read_jump_target(code, ip)?,
                }),
                OpCode::Brfalse => Some(AotBlockTerminal::ConditionalJump {
                    target_ip: read_jump_target(code, ip)?,
                    fallthrough_ip: next_ip,
                }),
                _ if next_ip == code.len() => Some(AotBlockTerminal::Stop),
                _ if Some(next_ip) == next_block_start => {
                    Some(AotBlockTerminal::Fallthrough { next_ip })
                }
                _ => None,
            };

            if let Some(terminal) = terminal {
                blocks.push(AotBasicBlock {
                    start_ip,
                    end_ip: next_ip,
                    terminal,
                });
                break;
            }

            ip = next_ip;
        }
    }

    Ok(AotCfg {
        entry_ip: 0,
        blocks,
    })
}

fn collect_block_starts(code: &[u8]) -> Result<Vec<usize>, AotCfgError> {
    let mut starts = BTreeSet::new();
    starts.insert(0usize);

    let mut ip = 0usize;
    while ip < code.len() {
        let (opcode, next_ip) = decode_instruction_bounds(code, ip)?;
        match opcode {
            OpCode::Br => {
                starts.insert(read_jump_target(code, ip)?);
            }
            OpCode::Brfalse => {
                starts.insert(read_jump_target(code, ip)?);
                if next_ip < code.len() {
                    starts.insert(next_ip);
                }
            }
            _ => {}
        }
        ip = next_ip;
    }

    Ok(starts.into_iter().collect())
}

fn decode_instruction_bounds(code: &[u8], ip: usize) -> Result<(OpCode, usize), AotCfgError> {
    let opcode_byte = *code
        .get(ip)
        .ok_or(AotCfgError::InvalidOpcode { ip, opcode: 0xFF })?;
    let opcode = OpCode::try_from(opcode_byte).map_err(|_| AotCfgError::InvalidOpcode {
        ip,
        opcode: opcode_byte,
    })?;
    let operand_len = opcode.operand_len();
    let operands_start = ip.saturating_add(1);
    let operands_end = operands_start.saturating_add(operand_len);
    if operands_end > code.len() {
        return Err(AotCfgError::TruncatedInstruction {
            ip,
            opcode,
            expected_operand_bytes: operand_len,
            available_operand_bytes: code.len().saturating_sub(operands_start),
        });
    }
    Ok((opcode, operands_end))
}

fn read_jump_target(code: &[u8], ip: usize) -> Result<usize, AotCfgError> {
    let bytes = code
        .get(ip + 1..ip + 5)
        .ok_or(AotCfgError::TruncatedInstruction {
            ip,
            opcode: OpCode::Br,
            expected_operand_bytes: 4,
            available_operand_bytes: code.len().saturating_sub(ip + 1),
        })?;
    let target =
        u32::from_le_bytes(bytes.try_into().expect("branch target width matches")) as usize;
    if target >= code.len() {
        return Err(AotCfgError::InvalidJumpTarget { ip, target });
    }
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BytecodeBuilder, Value};

    fn patch_branch_target(code: &mut [u8], instr_ip: u32, target: u32) {
        let start = instr_ip as usize + 1;
        code[start..start + 4].copy_from_slice(&target.to_le_bytes());
    }

    #[test]
    fn aot_cfg_builds_conditional_diamond_blocks() {
        let mut bc = BytecodeBuilder::new();
        bc.ldc(0);
        let branch_ip = bc.position();
        bc.brfalse(0);
        let true_ip = bc.position();
        bc.ldc(1);
        let jump_ip = bc.position();
        bc.br(0);
        let false_ip = bc.position();
        bc.ldc(2);
        let join_ip = bc.position();
        bc.ret();

        let mut code = bc.finish();
        patch_branch_target(&mut code, branch_ip, false_ip);
        patch_branch_target(&mut code, jump_ip, join_ip);

        let program = Program::new(
            vec![Value::Bool(false), Value::Int(10), Value::Int(20)],
            code,
        );
        let cfg = build_cfg(&program).expect("cfg should build");

        assert_eq!(
            cfg.blocks
                .iter()
                .map(|block| block.start_ip)
                .collect::<Vec<_>>(),
            vec![0, true_ip as usize, false_ip as usize, join_ip as usize]
        );
        assert_eq!(
            cfg.block(0).expect("entry block").terminal,
            AotBlockTerminal::ConditionalJump {
                target_ip: false_ip as usize,
                fallthrough_ip: true_ip as usize,
            }
        );
        assert_eq!(
            cfg.block(true_ip as usize).expect("true block").terminal,
            AotBlockTerminal::Jump {
                target_ip: join_ip as usize,
            }
        );
        assert_eq!(
            cfg.block(false_ip as usize).expect("false block").terminal,
            AotBlockTerminal::Fallthrough {
                next_ip: join_ip as usize,
            }
        );
        assert_eq!(
            cfg.block(join_ip as usize).expect("join block").terminal,
            AotBlockTerminal::Return
        );
    }

    #[test]
    fn aot_cfg_builds_loop_back_edges() {
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
        bc.ldloc(0);
        bc.ret();

        let mut code = bc.finish();
        patch_branch_target(&mut code, branch_ip, loop_ip);

        let program = Program::new(vec![Value::Int(0), Value::Int(1), Value::Int(4)], code);
        let cfg = build_cfg(&program).expect("cfg should build");

        assert_eq!(
            cfg.blocks
                .iter()
                .map(|block| block.start_ip)
                .collect::<Vec<_>>(),
            vec![0, loop_ip as usize, exit_ip as usize]
        );
        assert_eq!(
            cfg.block(0).expect("init block").terminal,
            AotBlockTerminal::Fallthrough {
                next_ip: loop_ip as usize,
            }
        );
        assert_eq!(
            cfg.block(loop_ip as usize).expect("loop block").terminal,
            AotBlockTerminal::ConditionalJump {
                target_ip: loop_ip as usize,
                fallthrough_ip: exit_ip as usize,
            }
        );
        assert_eq!(
            cfg.block(exit_ip as usize).expect("exit block").terminal,
            AotBlockTerminal::Return
        );
    }

    #[test]
    fn aot_cfg_rejects_out_of_bounds_branch_targets() {
        let mut bc = BytecodeBuilder::new();
        let branch_ip = bc.position();
        bc.br(0);
        bc.ret();

        let mut code = bc.finish();
        patch_branch_target(&mut code, branch_ip, 999);

        let program = Program::new(Vec::new(), code);
        assert_eq!(
            build_cfg(&program),
            Err(AotCfgError::InvalidJumpTarget {
                ip: branch_ip as usize,
                target: 999,
            })
        );
    }
}
