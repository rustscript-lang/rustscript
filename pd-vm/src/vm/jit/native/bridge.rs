use super::*;

fn run_step<F>(vm: *mut Vm, helper_name: &str, f: F) -> i32
where
    F: FnOnce(&mut Vm) -> VmResult<i32>,
{
    let Some(vm_ref) = (unsafe { vm.as_mut() }) else {
        store_bridge_error(VmError::JitNative(format!(
            "cranelift trace {helper_name} helper received null vm pointer"
        )));
        return STATUS_ERROR;
    };

    match f(vm_ref) {
        Ok(status) => status,
        Err(err) => {
            store_bridge_error(err);
            STATUS_ERROR
        }
    }
}

fn bridge_name_for_op(op: i64) -> Option<&'static str> {
    match op {
        OP_LDC => Some("ldc"),
        OP_ADD => Some("add"),
        OP_SUB => Some("sub"),
        OP_MUL => Some("mul"),
        OP_DIV => Some("div"),
        OP_MOD => Some("mod"),
        OP_SHL => Some("shl"),
        OP_SHR => Some("shr"),
        OP_LSHR => Some("lshr"),
        OP_AND => Some("and"),
        OP_OR => Some("or"),
        OP_NOT => Some("not"),
        OP_NEG => Some("neg"),
        OP_CEQ => Some("ceq"),
        OP_CLT => Some("clt"),
        OP_CGT => Some("cgt"),
        OP_POP => Some("pop"),
        OP_DUP => Some("dup"),
        OP_LDLOC => Some("ldloc"),
        OP_STLOC => Some("stloc"),
        OP_CALL => Some("call"),
        OP_BUILTIN_CALL => Some("builtin_call"),
        OP_GUARD_FALSE => Some("guard_false"),
        OP_JUMP => Some("jump_ip"),
        _ => None,
    }
}

pub(super) extern "C" fn pd_vm_cranelift_step(vm: *mut Vm, op: i64, a: i64, b: i64, c: i64) -> i32 {
    run_step(vm, "step", |vm| {
        if op == OP_BUILTIN_CALL {
            let bridge_name = u16::try_from(a)
                .ok()
                .and_then(BuiltinFunction::from_call_index)
                .map(BuiltinFunction::name)
                .unwrap_or("builtin_call");
            vm.record_jit_native_bridge_hit(bridge_name);
        } else if let Some(name) = bridge_name_for_op(op) {
            vm.record_jit_native_bridge_hit(name);
        }

        match op {
            OP_LDC => {
                let index = u32::try_from(a)
                    .map_err(|_| VmError::JitNative("ldc index out of range".to_string()))?;
                let value = vm
                    .program
                    .constants
                    .get(index as usize)
                    .cloned()
                    .ok_or(VmError::InvalidConstant(index))?;
                vm.stack.push(value);
                Ok(STATUS_CONTINUE)
            }
            OP_ADD => {
                vm.binary_add_op()?;
                Ok(STATUS_CONTINUE)
            }
            OP_SUB => {
                vm.binary_numeric_op(
                    |lhs, rhs| Ok(lhs.wrapping_sub(rhs)),
                    |lhs, rhs| Ok(lhs - rhs),
                )?;
                Ok(STATUS_CONTINUE)
            }
            OP_MUL => {
                vm.binary_numeric_op(
                    |lhs, rhs| Ok(lhs.wrapping_mul(rhs)),
                    |lhs, rhs| Ok(lhs * rhs),
                )?;
                Ok(STATUS_CONTINUE)
            }
            OP_DIV => {
                vm.binary_numeric_op(crate::vm::checked_int_div, |lhs, rhs| Ok(lhs / rhs))?;
                Ok(STATUS_CONTINUE)
            }
            OP_MOD => {
                vm.binary_numeric_op(crate::vm::checked_int_rem, |lhs, rhs| Ok(lhs % rhs))?;
                Ok(STATUS_CONTINUE)
            }
            OP_SHL => {
                let rhs = vm.pop_shift_amount()?;
                let lhs = vm.pop_int()?;
                vm.stack
                    .push(crate::bytecode::Value::Int(lhs.wrapping_shl(rhs)));
                Ok(STATUS_CONTINUE)
            }
            OP_SHR => {
                let rhs = vm.pop_shift_amount()?;
                let lhs = vm.pop_int()?;
                vm.stack
                    .push(crate::bytecode::Value::Int(lhs.wrapping_shr(rhs)));
                Ok(STATUS_CONTINUE)
            }
            OP_LSHR => {
                let rhs = vm.pop_shift_amount()?;
                let lhs = vm.pop_int()?;
                vm.stack
                    .push(crate::bytecode::Value::Int(crate::vm::logical_shr_i64(
                        lhs, rhs,
                    )));
                Ok(STATUS_CONTINUE)
            }
            OP_AND => {
                let rhs = vm.pop_bool()?;
                let lhs = vm.pop_bool()?;
                vm.stack.push(crate::bytecode::Value::Bool(lhs && rhs));
                Ok(STATUS_CONTINUE)
            }
            OP_OR => {
                let rhs = vm.pop_bool()?;
                let lhs = vm.pop_bool()?;
                vm.stack.push(crate::bytecode::Value::Bool(lhs || rhs));
                Ok(STATUS_CONTINUE)
            }
            OP_NOT => {
                vm.unary_not_op()?;
                Ok(STATUS_CONTINUE)
            }
            OP_NEG => {
                let value = vm.pop_numeric()?;
                match value {
                    NumericValue::Int(value) => vm
                        .stack
                        .push(crate::bytecode::Value::Int(value.wrapping_neg())),
                    NumericValue::Float(value) => {
                        vm.stack.push(crate::bytecode::Value::Float(-value))
                    }
                }
                Ok(STATUS_CONTINUE)
            }
            OP_CEQ => {
                let rhs = vm.pop_value()?;
                let lhs = vm.pop_value()?;
                vm.stack.push(crate::bytecode::Value::Bool(lhs == rhs));
                Ok(STATUS_CONTINUE)
            }
            OP_CLT => {
                vm.compare_numeric_op(|lhs, rhs| lhs < rhs, |lhs, rhs| lhs < rhs)?;
                Ok(STATUS_CONTINUE)
            }
            OP_CGT => {
                vm.compare_numeric_op(|lhs, rhs| lhs > rhs, |lhs, rhs| lhs > rhs)?;
                Ok(STATUS_CONTINUE)
            }
            OP_POP => {
                vm.pop_value()?;
                Ok(STATUS_CONTINUE)
            }
            OP_DUP => {
                let value = vm.peek_value()?.clone();
                vm.stack.push(value);
                Ok(STATUS_CONTINUE)
            }
            OP_LDLOC => {
                let index = u8::try_from(a)
                    .map_err(|_| VmError::JitNative("ldloc index out of range".to_string()))?;
                let slot = vm
                    .locals
                    .get_mut(index as usize)
                    .ok_or(VmError::InvalidLocal(index))?;
                let value = std::mem::replace(slot, crate::bytecode::Value::Null);
                vm.stack.push(value);
                Ok(STATUS_CONTINUE)
            }
            OP_STLOC => {
                let index = u8::try_from(a)
                    .map_err(|_| VmError::JitNative("stloc index out of range".to_string()))?;
                let value = vm.pop_value()?;
                vm.store_local_with_drop_contract(index, value)?;
                Ok(STATUS_CONTINUE)
            }
            OP_CALL => {
                let index = u16::try_from(a)
                    .map_err(|_| VmError::JitNative("call index out of range".to_string()))?;
                let argc = u8::try_from(b)
                    .map_err(|_| VmError::JitNative("call argc out of range".to_string()))?;
                let call_ip = usize::try_from(c)
                    .map_err(|_| VmError::JitNative("call ip out of range".to_string()))?;
                match vm.execute_host_call(index, argc, call_ip)? {
                    HostCallExecOutcome::Returned => Ok(STATUS_CONTINUE),
                    HostCallExecOutcome::Yielded => Ok(STATUS_YIELDED),
                    HostCallExecOutcome::Pending(_) => Ok(STATUS_WAITING),
                }
            }
            OP_BUILTIN_CALL => {
                let index = u16::try_from(a).map_err(|_| {
                    VmError::JitNative("builtin call index out of range".to_string())
                })?;
                let argc = u8::try_from(b).map_err(|_| {
                    VmError::JitNative("builtin call argc out of range".to_string())
                })?;
                let call_ip = usize::try_from(c)
                    .map_err(|_| VmError::JitNative("builtin call ip out of range".to_string()))?;
                match vm.execute_host_call(index, argc, call_ip)? {
                    HostCallExecOutcome::Returned => Ok(STATUS_CONTINUE),
                    HostCallExecOutcome::Yielded => Ok(STATUS_YIELDED),
                    HostCallExecOutcome::Pending(_) => Ok(STATUS_WAITING),
                }
            }
            OP_GUARD_FALSE => {
                let exit_ip = usize::try_from(a)
                    .map_err(|_| VmError::JitNative("guard exit ip out of range".to_string()))?;
                let condition = vm.pop_bool()?;
                if !condition {
                    vm.jump_to(exit_ip)?;
                    return Ok(STATUS_TRACE_EXIT);
                }
                Ok(STATUS_CONTINUE)
            }
            OP_JUMP => {
                let target_ip = usize::try_from(a)
                    .map_err(|_| VmError::JitNative("jump target out of range".to_string()))?;
                vm.jump_to(target_ip)?;
                Ok(STATUS_TRACE_EXIT)
            }
            _ => Err(VmError::JitNative(format!(
                "cranelift step helper received unsupported op id {op}"
            ))),
        }
    })
}
