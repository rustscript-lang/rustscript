use super::*;

#[derive(Clone, Copy, Debug)]
enum ScalarValue {
    Int(i64),
    Float(f64),
}

impl ScalarValue {
    #[inline(always)]
    fn from_value(value: &Value) -> Option<Self> {
        match value {
            Value::Int(value) => Some(Self::Int(*value)),
            Value::Float(value) => Some(Self::Float(*value)),
            _ => None,
        }
    }

    #[inline(always)]
    fn into_value(self) -> Value {
        match self {
            Self::Int(value) => Value::Int(value),
            Self::Float(value) => Value::Float(value),
        }
    }
}

impl Vm {
    #[inline(always)]
    pub(super) fn decoded_ldc_value_at(&self, opcode_ip: usize) -> Option<&Value> {
        self.decoded_instruction_data
            .ldc_values
            .get(opcode_ip)
            .and_then(|value| value.as_ref())
    }

    #[inline(always)]
    pub(super) fn decoded_jump_target_at(&self, opcode_ip: usize) -> Option<usize> {
        self.decoded_instruction_data
            .jump_targets
            .get(opcode_ip)
            .and_then(|target| *target)
    }

    #[inline(always)]
    pub(super) fn decoded_local_index_at(&self, opcode_ip: usize) -> Option<u8> {
        self.decoded_instruction_data
            .local_indices
            .get(opcode_ip)
            .and_then(|index| *index)
    }

    #[inline(always)]
    pub(super) fn try_fuse_scalar_sequence(
        &mut self,
        src: u8,
        allow_superinstructions: bool,
    ) -> VmResult<bool> {
        if !allow_superinstructions {
            return Ok(false);
        }
        let Some(initial) = self.local_numeric_value(src).map(|value| match value {
            NumericValue::Int(value) => ScalarValue::Int(value),
            NumericValue::Float(value) => ScalarValue::Float(value),
        }) else {
            return Ok(false);
        };
        let code = &self.program.code;
        let mut cursor = self.ip;
        let mut stack = [None; 8];
        let mut stack_len = 1usize;
        stack[0] = Some(initial);
        let mut steps = 0usize;
        while cursor < code.len() && steps < 16 {
            let opcode = match OpCode::try_from(code[cursor]) {
                Ok(opcode) => opcode,
                Err(_) => return Ok(false),
            };
            match opcode {
                OpCode::Ldc => {
                    let Some(value) = self
                        .decoded_ldc_value_at(cursor)
                        .and_then(ScalarValue::from_value)
                    else {
                        return Ok(false);
                    };
                    if stack_len == stack.len() {
                        return Ok(false);
                    }
                    stack[stack_len] = Some(value);
                    stack_len += 1;
                    cursor += 5;
                }
                OpCode::Ldloc => {
                    let Some(index) = self.decoded_local_index_at(cursor) else {
                        return Ok(false);
                    };
                    let Some(value) = self.local_numeric_value(index).map(|value| match value {
                        NumericValue::Int(value) => ScalarValue::Int(value),
                        NumericValue::Float(value) => ScalarValue::Float(value),
                    }) else {
                        return Ok(false);
                    };
                    if stack_len == stack.len() {
                        return Ok(false);
                    }
                    stack[stack_len] = Some(value);
                    stack_len += 1;
                    cursor += 2;
                }
                OpCode::Add
                | OpCode::Sub
                | OpCode::Mul
                | OpCode::Div
                | OpCode::Mod
                | OpCode::Shl => {
                    if stack_len < 2 {
                        return Ok(false);
                    }
                    let rhs = stack[stack_len - 1]
                        .take()
                        .expect("rhs scalar should exist");
                    let lhs = stack[stack_len - 2]
                        .take()
                        .expect("lhs scalar should exist");
                    stack_len -= 2;
                    let lhs_f = match lhs {
                        ScalarValue::Int(value) => value as f64,
                        ScalarValue::Float(value) => value,
                    };
                    let rhs_f = match rhs {
                        ScalarValue::Int(value) => value as f64,
                        ScalarValue::Float(value) => value,
                    };
                    let result = match opcode {
                        OpCode::Add => match (lhs, rhs) {
                            (ScalarValue::Int(lhs), ScalarValue::Int(rhs)) => {
                                ScalarValue::Int(lhs.wrapping_add(rhs))
                            }
                            _ => ScalarValue::Float(lhs_f + rhs_f),
                        },
                        OpCode::Sub => match (lhs, rhs) {
                            (ScalarValue::Int(lhs), ScalarValue::Int(rhs)) => {
                                ScalarValue::Int(lhs.wrapping_sub(rhs))
                            }
                            _ => ScalarValue::Float(lhs_f - rhs_f),
                        },
                        OpCode::Mul => match (lhs, rhs) {
                            (ScalarValue::Int(lhs), ScalarValue::Int(rhs)) => {
                                ScalarValue::Int(lhs.wrapping_mul(rhs))
                            }
                            _ => ScalarValue::Float(lhs_f * rhs_f),
                        },
                        OpCode::Div => match (lhs, rhs) {
                            (ScalarValue::Int(lhs), ScalarValue::Int(rhs)) => {
                                ScalarValue::Int(checked_int_div(lhs, rhs)?)
                            }
                            _ => ScalarValue::Float(lhs_f / rhs_f),
                        },
                        OpCode::Mod => match (lhs, rhs) {
                            (ScalarValue::Int(lhs), ScalarValue::Int(rhs)) => {
                                ScalarValue::Int(checked_int_rem(lhs, rhs)?)
                            }
                            _ => ScalarValue::Float(lhs_f % rhs_f),
                        },
                        OpCode::Shl => match (lhs, rhs) {
                            (ScalarValue::Int(lhs), ScalarValue::Int(rhs)) => {
                                if !(0..=63).contains(&rhs) {
                                    return Err(VmError::InvalidShift(rhs));
                                }
                                ScalarValue::Int(lhs.wrapping_shl(rhs as u32))
                            }
                            _ => return Ok(false),
                        },
                        _ => unreachable!(),
                    };
                    stack[stack_len] = Some(result);
                    stack_len += 1;
                    cursor += 1;
                }
                OpCode::Stloc => {
                    if stack_len != 1 {
                        return Ok(false);
                    }
                    let Some(dst) = self.decoded_local_index_at(cursor) else {
                        return Ok(false);
                    };
                    let value = stack[0]
                        .take()
                        .expect("result scalar should exist")
                        .into_value();
                    self.store_local_with_drop_contract(dst, value)?;
                    self.ip = cursor + 2;
                    return Ok(true);
                }
                OpCode::Clt | OpCode::Cgt => {
                    if stack_len != 2 {
                        return Ok(false);
                    }
                    if code.get(cursor + 1).copied() != Some(OpCode::Brfalse as u8) {
                        return Ok(false);
                    }
                    let Some(target) = self.decoded_jump_target_at(cursor + 1) else {
                        return Ok(false);
                    };
                    let rhs = stack[stack_len - 1]
                        .take()
                        .expect("rhs scalar should exist");
                    let lhs = stack[stack_len - 2]
                        .take()
                        .expect("lhs scalar should exist");
                    let lhs_f = match lhs {
                        ScalarValue::Int(value) => value as f64,
                        ScalarValue::Float(value) => value,
                    };
                    let rhs_f = match rhs {
                        ScalarValue::Int(value) => value as f64,
                        ScalarValue::Float(value) => value,
                    };
                    let condition = match opcode {
                        OpCode::Clt => match (lhs, rhs) {
                            (ScalarValue::Int(lhs), ScalarValue::Int(rhs)) => lhs < rhs,
                            _ => lhs_f < rhs_f,
                        },
                        OpCode::Cgt => match (lhs, rhs) {
                            (ScalarValue::Int(lhs), ScalarValue::Int(rhs)) => lhs > rhs,
                            _ => lhs_f > rhs_f,
                        },
                        _ => unreachable!(),
                    };
                    self.ip = cursor + 6;
                    if !condition {
                        self.jump_to(target)?;
                    }
                    return Ok(true);
                }
                _ => return Ok(false),
            }
            steps += 1;
        }
        Ok(false)
    }
}
