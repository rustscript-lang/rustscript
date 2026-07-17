use core::cell::RefCell;

use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use super::{
    CallableEnvironment, CallableTarget, CallableValue, HostBinding, HostDispatcher, HostFunction,
    OpCode, Program, Value, VmError, resolve_host_functions,
};

pub type VmResult<T> = Result<T, VmError>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VmStatus {
    Halted,
}

#[derive(Clone, Copy)]
enum NumericValue {
    Int(i64),
    Float(f64),
}

#[derive(Clone, Debug)]
struct ExecutionFrame {
    return_ip: usize,
    operand_stack_base: usize,
    local_base: usize,
    local_count: usize,
    prototype_id: u32,
    active_callable: Rc<CallableValue>,
}

pub struct Vm<C = ()> {
    program: Program,
    ip: usize,
    stack: Vec<Value>,
    locals: Vec<Value>,
    host_functions: Vec<HostFunction<C>>,
    host_dispatcher: Option<HostDispatcher<C>>,
    context: C,
    fuel: Option<u64>,
    program_instance: u64,
    frames: Vec<ExecutionFrame>,
}

impl Vm<()> {
    pub fn new(program: Program) -> Self {
        Self::from_parts(program, (), Vec::new(), None)
    }
}

impl<C> Vm<C> {
    pub fn with_host_bindings(
        program: Program,
        context: C,
        bindings: &[HostBinding<C>],
    ) -> VmResult<Self> {
        let host_functions = resolve_host_functions(&program, bindings)?;
        Ok(Self::from_parts(program, context, host_functions, None))
    }

    pub fn with_host_dispatcher(
        program: Program,
        context: C,
        dispatcher: HostDispatcher<C>,
    ) -> Self {
        Self::from_parts(program, context, Vec::new(), Some(dispatcher))
    }

    fn from_parts(
        program: Program,
        context: C,
        host_functions: Vec<HostFunction<C>>,
        host_dispatcher: Option<HostDispatcher<C>>,
    ) -> Self {
        let local_count = program.local_count();
        let mut vm = Self {
            program,
            ip: 0,
            stack: Vec::new(),
            locals: vec![Value::Null; local_count],
            host_functions,
            host_dispatcher,
            context,
            fuel: None,
            program_instance: 1,
            frames: Vec::new(),
        };
        vm.initialize_root_callables();
        vm
    }

    fn initialize_root_callables(&mut self) {
        for binding in self.program.root_callable_bindings() {
            let Some(prototype) = self
                .program
                .callable_prototypes()
                .get(binding.prototype_id as usize)
            else {
                continue;
            };
            let slot = binding.local_slot as usize;
            if let Some(local) = self.locals.get_mut(slot) {
                *local = Value::Callable(Rc::new(CallableValue {
                    program_instance: self.program_instance,
                    prototype_id: binding.prototype_id,
                    kind: prototype.kind,
                    env: None,
                }));
            }
        }
    }

    fn active_local_base(&self) -> usize {
        self.frames.last().map_or(0, |frame| frame.local_base)
    }

    fn absolute_local(&self, index: u8) -> VmResult<usize> {
        let absolute = self.active_local_base().saturating_add(index as usize);
        (absolute < self.locals.len())
            .then_some(absolute)
            .ok_or(VmError::InvalidLocal(index))
    }

    fn make_callable(&mut self, prototype_id: u32) -> VmResult<()> {
        let prototype = self
            .program
            .callable_prototypes()
            .get(prototype_id as usize)
            .ok_or(VmError::InvalidCallablePrototype(prototype_id))?;
        let capture_count = prototype.capture_slots.len();
        if self.stack.len() < capture_count {
            return Err(VmError::StackUnderflow);
        }
        let captures = self.stack.split_off(self.stack.len() - capture_count);
        let env: CallableEnvironment = Rc::new(RefCell::new(captures));
        self.stack.push(Value::Callable(Rc::new(CallableValue {
            program_instance: self.program_instance,
            prototype_id,
            kind: prototype.kind,
            env: Some(env),
        })));
        Ok(())
    }

    fn call_value(&mut self, argc: u8) -> VmResult<()> {
        if self.frames.len() >= 1024 {
            return Err(VmError::CallStackOverflow);
        }
        let operand_count = argc as usize + 1;
        if self.stack.len() < operand_count {
            return Err(VmError::StackUnderflow);
        }
        let stack_base = self.stack.len() - operand_count;
        let mut operands = self.stack.split_off(stack_base);
        let callable = match operands.remove(0) {
            Value::Callable(callable) => callable,
            _ => return Err(VmError::InvalidCallable),
        };
        if callable.program_instance != self.program_instance {
            return Err(VmError::StaleCallable);
        }
        let prototype = self
            .program
            .callable_prototypes()
            .get(callable.prototype_id as usize)
            .cloned()
            .ok_or(VmError::InvalidCallablePrototype(callable.prototype_id))?;
        if prototype.arity != argc || prototype.parameter_slots.len() != operands.len() {
            return Err(VmError::InvalidCallArity {
                import: String::from("script callable"),
                expected: prototype.arity,
                got: argc,
            });
        }
        match prototype.target {
            CallableTarget::HostImport(index) => {
                self.stack.extend(operands);
                self.call_host(index, argc)
            }
            CallableTarget::ScriptFunction(function_id) => {
                let function = self
                    .program
                    .script_functions()
                    .get(function_id as usize)
                    .cloned()
                    .ok_or(VmError::InvalidCallablePrototype(callable.prototype_id))?;
                let inherited = {
                    let base = self.active_local_base();
                    let count = self
                        .frames
                        .last()
                        .map_or(self.locals.len(), |frame| frame.local_count);
                    self.locals[base..base.saturating_add(count)]
                        .iter()
                        .enumerate()
                        .filter_map(|(slot, value)| match value {
                            Value::Callable(_) => Some((slot, value.clone())),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                };
                let local_base = self.locals.len();
                self.locals.resize(
                    local_base.saturating_add(prototype.frame_local_count),
                    Value::Null,
                );
                for binding in self.program.root_callable_bindings() {
                    if let Some(binding_prototype) = self
                        .program
                        .callable_prototypes()
                        .get(binding.prototype_id as usize)
                    {
                        let slot = binding.local_slot as usize;
                        if slot < prototype.frame_local_count {
                            self.locals[local_base + slot] =
                                Value::Callable(Rc::new(CallableValue {
                                    program_instance: self.program_instance,
                                    prototype_id: binding.prototype_id,
                                    kind: binding_prototype.kind,
                                    env: None,
                                }));
                        }
                    }
                }
                for (slot, value) in inherited {
                    if slot < prototype.frame_local_count {
                        self.locals[local_base + slot] = value;
                    }
                }
                for (slot, argument) in prototype.parameter_slots.iter().zip(operands) {
                    let slot = *slot as usize;
                    if slot >= prototype.frame_local_count {
                        return Err(VmError::InvalidCallablePrototype(callable.prototype_id));
                    }
                    self.locals[local_base + slot] = argument;
                }
                if let Some(env) = &callable.env {
                    for (slot, value) in prototype
                        .capture_slots
                        .iter()
                        .zip(env.borrow().iter().cloned())
                    {
                        let slot = *slot as usize;
                        if slot >= prototype.frame_local_count {
                            return Err(VmError::InvalidCallablePrototype(callable.prototype_id));
                        }
                        self.locals[local_base + slot] = value;
                    }
                }
                if let Some(slot) = prototype.self_slot {
                    let slot = slot as usize;
                    if slot >= prototype.frame_local_count {
                        return Err(VmError::InvalidCallablePrototype(callable.prototype_id));
                    }
                    self.locals[local_base + slot] = Value::Callable(callable.clone());
                }
                self.frames.push(ExecutionFrame {
                    return_ip: self.ip,
                    operand_stack_base: stack_base,
                    local_base,
                    local_count: prototype.frame_local_count,
                    prototype_id: callable.prototype_id,
                    active_callable: callable,
                });
                self.ip = function.entry_ip as usize;
                Ok(())
            }
        }
    }

    fn return_from_frame(&mut self) -> VmResult<bool> {
        let Some(frame) = self.frames.pop() else {
            return Ok(false);
        };
        if self.stack.len() < frame.operand_stack_base {
            return Err(VmError::StackUnderflow);
        }
        let result = if self.stack.len() > frame.operand_stack_base {
            self.stack.pop().unwrap_or(Value::Null)
        } else {
            Value::Null
        };
        self.stack.truncate(frame.operand_stack_base);
        if let Some(environment) = frame.active_callable.env.as_ref()
            && let Some(prototype) = self
                .program
                .callable_prototypes()
                .get(frame.prototype_id as usize)
        {
            let mut cells = environment.borrow_mut();
            for (cell, slot) in cells.iter_mut().zip(&prototype.capture_slots) {
                if prototype.self_slot == Some(*slot) {
                    continue;
                }
                let absolute = frame.local_base.saturating_add(*slot as usize);
                let value = self
                    .locals
                    .get(absolute)
                    .cloned()
                    .ok_or(VmError::InvalidCallablePrototype(frame.prototype_id))?;
                *cell = value;
            }
        }
        self.locals.truncate(frame.local_base);
        self.ip = frame.return_ip;
        self.stack.push(result);
        Ok(true)
    }

    pub fn run(&mut self) -> VmResult<VmStatus> {
        loop {
            self.charge_fuel()?;
            let raw = self.read_u8()?;
            let opcode = OpCode::try_from(raw).map_err(|()| VmError::InvalidOpcode(raw))?;
            match opcode {
                OpCode::Nop => {}
                OpCode::Ret => {
                    if !self.return_from_frame()? {
                        return Ok(VmStatus::Halted);
                    }
                }
                OpCode::Ldc => {
                    let index = self.read_u32()?;
                    let value = self
                        .program
                        .constants()
                        .get(index as usize)
                        .cloned()
                        .ok_or(VmError::InvalidConstant(index))?;
                    self.stack.push(value);
                }
                OpCode::Add => self.add()?,
                OpCode::Sub => {
                    self.numeric_binary(|lhs, rhs| Ok(lhs.wrapping_sub(rhs)), |lhs, rhs| lhs - rhs)?
                }
                OpCode::Mul => {
                    self.numeric_binary(|lhs, rhs| Ok(lhs.wrapping_mul(rhs)), |lhs, rhs| lhs * rhs)?
                }
                OpCode::Div => self.numeric_binary(checked_int_div, |lhs, rhs| lhs / rhs)?,
                OpCode::Neg => self.neg()?,
                OpCode::Ceq => {
                    let rhs = self.pop()?;
                    let lhs = self.pop()?;
                    self.stack.push(Value::Bool(lhs == rhs));
                }
                OpCode::Clt => self.numeric_compare(|lhs, rhs| lhs < rhs, |lhs, rhs| lhs < rhs)?,
                OpCode::Cgt => self.numeric_compare(|lhs, rhs| lhs > rhs, |lhs, rhs| lhs > rhs)?,
                OpCode::Br => {
                    let target = self.read_u32()?;
                    self.jump(target)?;
                }
                OpCode::Brfalse => {
                    let target = self.read_u32()?;
                    if !self.pop_bool()? {
                        self.jump(target)?;
                    }
                }
                OpCode::Pop => {
                    self.pop()?;
                }
                OpCode::Dup => {
                    let value = self.stack.last().cloned().ok_or(VmError::StackUnderflow)?;
                    self.stack.push(value);
                }
                OpCode::Ldloc => {
                    let index = self.read_u8()?;
                    let absolute = self.absolute_local(index)?;
                    self.stack.push(self.locals[absolute].clone());
                }
                OpCode::Stloc => {
                    let index = self.read_u8()?;
                    let absolute = self.absolute_local(index)?;
                    let value = self.pop()?;
                    self.locals[absolute] = value;
                }

                OpCode::Call => {
                    let index = self.read_u16()?;
                    let arity = self.read_u8()?;
                    self.call_host(index, arity)?;
                }
                OpCode::CallValue => {
                    let arity = self.read_u8()?;
                    self.call_value(arity)?;
                }
                OpCode::MakeCallable => {
                    let prototype_id = self.read_u32()?;
                    self.make_callable(prototype_id)?;
                }
                OpCode::Shl => {
                    let rhs = self.pop_shift()?;
                    let lhs = self.pop_int()?;
                    self.stack.push(Value::Int(lhs.wrapping_shl(rhs)));
                }
                OpCode::Shr => {
                    let rhs = self.pop_shift()?;
                    let lhs = self.pop_int()?;
                    self.stack.push(Value::Int(lhs.wrapping_shr(rhs)));
                }
                OpCode::Mod => self.numeric_binary(checked_int_rem, |lhs, rhs| lhs % rhs)?,
                OpCode::And => {
                    let rhs = self.pop_bool()?;
                    let lhs = self.pop_bool()?;
                    self.stack.push(Value::Bool(lhs && rhs));
                }
                OpCode::Or => {
                    let rhs = self.pop_bool()?;
                    let lhs = self.pop_bool()?;
                    self.stack.push(Value::Bool(lhs || rhs));
                }
                OpCode::Not => {
                    let value = self.pop_bool()?;
                    self.stack.push(Value::Bool(!value));
                }
                OpCode::Lshr => {
                    let rhs = self.pop_shift()?;
                    let lhs = self.pop_int()?;
                    self.stack.push(Value::Int(((lhs as u64) >> rhs) as i64));
                }
            }
        }
    }

    pub fn stack(&self) -> &[Value] {
        &self.stack
    }

    pub fn locals(&self) -> &[Value] {
        &self.locals
    }

    pub fn set_local(&mut self, index: u8, value: Value) -> VmResult<()> {
        self.store_local(index, value)
    }

    pub fn ip(&self) -> usize {
        self.ip
    }

    pub fn context(&self) -> &C {
        &self.context
    }

    pub fn context_mut(&mut self) -> &mut C {
        &mut self.context
    }

    pub fn into_context(self) -> C {
        self.context
    }

    pub fn set_fuel(&mut self, fuel: u64) {
        self.fuel = Some(fuel);
    }

    pub fn clear_fuel(&mut self) {
        self.fuel = None;
    }

    pub fn fuel(&self) -> Option<u64> {
        self.fuel
    }

    pub fn add_fuel(&mut self, fuel: u64) -> VmResult<()> {
        self.fuel = Some(
            self.fuel
                .unwrap_or(0)
                .checked_add(fuel)
                .ok_or(VmError::FuelOverflow)?,
        );
        Ok(())
    }

    fn charge_fuel(&mut self) -> VmResult<()> {
        let Some(remaining) = self.fuel else {
            return Ok(());
        };
        if remaining == 0 {
            return Err(VmError::OutOfFuel {
                needed: 1,
                remaining: 0,
            });
        }
        self.fuel = Some(remaining - 1);
        Ok(())
    }

    fn call_host(&mut self, index: u16, arity: u8) -> VmResult<()> {
        if let Some(result) = self.call_core_builtin(index, arity) {
            return result;
        }
        let import = self
            .program
            .imports()
            .get(usize::from(index))
            .ok_or(VmError::InvalidCall(index))?;
        if import.arity != arity {
            return Err(VmError::InvalidCallArity {
                import: import.name.clone(),
                expected: import.arity,
                got: arity,
            });
        }
        let argument_count = usize::from(arity);
        let argument_start = self
            .stack
            .len()
            .checked_sub(argument_count)
            .ok_or(VmError::StackUnderflow)?;
        let arguments = &self.stack[argument_start..];
        let result = if let Some(function) = self.host_functions.get(usize::from(index)).copied() {
            function(&mut self.context, arguments)
        } else if let Some(dispatcher) = self.host_dispatcher {
            dispatcher(&mut self.context, import.name.as_str(), arguments)
        } else {
            return Err(VmError::HostCallsUnavailable(index));
        }
        .map_err(|error| VmError::HostError(error.message()))?;
        self.stack.truncate(argument_start);
        if let Some(value) = result {
            self.stack.push(value);
        }
        Ok(())
    }

    fn call_core_builtin(&mut self, index: u16, arity: u8) -> Option<VmResult<()>> {
        const BUILTIN_BASE: u16 = 0xFFA3;
        const ARRAY_NEW: u16 = BUILTIN_BASE + 3;
        const ARRAY_PUSH: u16 = BUILTIN_BASE + 4;
        const MAP_NEW: u16 = BUILTIN_BASE + 5;
        const SET: u16 = BUILTIN_BASE + 8;

        Some(match index {
            ARRAY_NEW => {
                if let Err(error) = self.require_builtin_arity("array_new", arity, 0) {
                    return Some(Err(error));
                }
                self.stack.push(Value::array(Vec::new()));
                Ok(())
            }
            ARRAY_PUSH => {
                if let Err(error) = self.require_builtin_arity("array_push", arity, 2) {
                    return Some(Err(error));
                }
                let start = match self.stack.len().checked_sub(2) {
                    Some(start) => start,
                    None => return Some(Err(VmError::StackUnderflow)),
                };
                let value = self.stack[start + 1].clone();
                let Value::Array(mut values) = self.stack[start].clone() else {
                    return Some(Err(VmError::TypeMismatch("array")));
                };
                Rc::make_mut(&mut values).push(value);
                self.stack.truncate(start);
                self.stack.push(Value::Array(values));
                Ok(())
            }
            MAP_NEW => {
                if let Err(error) = self.require_builtin_arity("map_new", arity, 0) {
                    return Some(Err(error));
                }
                self.stack.push(Value::map(Vec::new()));
                Ok(())
            }
            SET => {
                if let Err(error) = self.require_builtin_arity("set", arity, 3) {
                    return Some(Err(error));
                }
                let start = match self.stack.len().checked_sub(3) {
                    Some(start) => start,
                    None => return Some(Err(VmError::StackUnderflow)),
                };
                let key = self.stack[start + 1].clone();
                let value = self.stack[start + 2].clone();
                let Value::Map(mut entries) = self.stack[start].clone() else {
                    return Some(Err(VmError::TypeMismatch("map")));
                };
                let mutable = Rc::make_mut(&mut entries);
                if let Some((_, current)) = mutable.iter_mut().find(|(current, _)| current == &key)
                {
                    *current = value;
                } else {
                    mutable.push((key, value));
                }
                self.stack.truncate(start);
                self.stack.push(Value::Map(entries));
                Ok(())
            }
            _ => return None,
        })
    }

    fn require_builtin_arity(&self, name: &str, got: u8, expected: u8) -> VmResult<()> {
        if got == expected {
            Ok(())
        } else {
            Err(VmError::InvalidCallArity {
                import: name.into(),
                expected,
                got,
            })
        }
    }

    fn add(&mut self) -> VmResult<()> {
        let rhs = self.pop()?;
        let lhs = self.pop()?;
        match (lhs, rhs) {
            (Value::Int(lhs), Value::Int(rhs)) => {
                self.stack.push(Value::Int(lhs.wrapping_add(rhs)));
            }
            (Value::Int(lhs), Value::Float(rhs)) => {
                self.stack.push(Value::Float(lhs as f64 + rhs));
            }
            (Value::Float(lhs), Value::Int(rhs)) => {
                self.stack.push(Value::Float(lhs + rhs as f64));
            }
            (Value::Float(lhs), Value::Float(rhs)) => {
                self.stack.push(Value::Float(lhs + rhs));
            }
            (Value::String(lhs), Value::String(rhs)) => {
                let mut value = String::with_capacity(lhs.len() + rhs.len());
                value.push_str(lhs.as_str());
                value.push_str(rhs.as_str());
                self.stack.push(Value::string(value));
            }
            (Value::Bytes(lhs), Value::Bytes(rhs)) => {
                let mut value = Vec::with_capacity(lhs.len() + rhs.len());
                value.extend_from_slice(lhs.as_slice());
                value.extend_from_slice(rhs.as_slice());
                self.stack.push(Value::bytes(value));
            }
            (Value::Array(lhs), Value::Array(rhs)) => {
                let mut value = Vec::with_capacity(lhs.len() + rhs.len());
                value.extend(lhs.iter().cloned());
                value.extend(rhs.iter().cloned());
                self.stack.push(Value::array(value));
            }
            _ => {
                return Err(VmError::TypeMismatch(
                    "number, string/string, bytes/bytes, or array/array",
                ));
            }
        }
        Ok(())
    }

    fn neg(&mut self) -> VmResult<()> {
        match self.pop_numeric()? {
            NumericValue::Int(value) => self.stack.push(Value::Int(value.wrapping_neg())),
            NumericValue::Float(value) => self.stack.push(Value::Float(-value)),
        }
        Ok(())
    }

    fn numeric_binary(
        &mut self,
        int_op: impl FnOnce(i64, i64) -> VmResult<i64>,
        float_op: impl FnOnce(f64, f64) -> f64,
    ) -> VmResult<()> {
        let rhs = self.pop_numeric()?;
        let lhs = self.pop_numeric()?;
        match (lhs, rhs) {
            (NumericValue::Int(lhs), NumericValue::Int(rhs)) => {
                self.stack.push(Value::Int(int_op(lhs, rhs)?));
            }
            (lhs, rhs) => {
                self.stack
                    .push(Value::Float(float_op(as_float(lhs), as_float(rhs))));
            }
        }
        Ok(())
    }

    fn numeric_compare(
        &mut self,
        int_op: impl FnOnce(i64, i64) -> bool,
        float_op: impl FnOnce(f64, f64) -> bool,
    ) -> VmResult<()> {
        let rhs = self.pop_numeric()?;
        let lhs = self.pop_numeric()?;
        let result = match (lhs, rhs) {
            (NumericValue::Int(lhs), NumericValue::Int(rhs)) => int_op(lhs, rhs),
            (lhs, rhs) => float_op(as_float(lhs), as_float(rhs)),
        };
        self.stack.push(Value::Bool(result));
        Ok(())
    }

    fn pop(&mut self) -> VmResult<Value> {
        self.stack.pop().ok_or(VmError::StackUnderflow)
    }

    fn pop_numeric(&mut self) -> VmResult<NumericValue> {
        match self.pop()? {
            Value::Int(value) => Ok(NumericValue::Int(value)),
            Value::Float(value) => Ok(NumericValue::Float(value)),
            _ => Err(VmError::TypeMismatch("number")),
        }
    }

    fn pop_int(&mut self) -> VmResult<i64> {
        match self.pop()? {
            Value::Int(value) => Ok(value),
            _ => Err(VmError::TypeMismatch("int")),
        }
    }

    fn pop_bool(&mut self) -> VmResult<bool> {
        match self.pop()? {
            Value::Bool(value) => Ok(value),
            _ => Err(VmError::TypeMismatch("bool")),
        }
    }

    fn pop_shift(&mut self) -> VmResult<u32> {
        let value = self.pop_int()?;
        if !(0..=63).contains(&value) {
            return Err(VmError::InvalidShift(value));
        }
        Ok(value as u32)
    }

    fn store_local(&mut self, index: u8, value: Value) -> VmResult<()> {
        let absolute = self.absolute_local(index)?;
        self.locals[absolute] = value;
        Ok(())
    }

    fn jump(&mut self, target: u32) -> VmResult<()> {
        let target_index = target as usize;
        if target_index >= self.program.code().len() {
            return Err(VmError::InvalidJump(target));
        }
        if !self.program.function_regions().is_empty() {
            let expected_prototype = self.frames.last().map(|frame| frame.prototype_id);
            let target_prototype = self
                .program
                .function_regions()
                .iter()
                .find(|region| {
                    region.start_ip as usize <= target_index
                        && target_index < region.end_ip as usize
                })
                .and_then(|region| region.prototype_id);
            if target_prototype != expected_prototype {
                return Err(VmError::InvalidJump(target));
            }
        }
        self.ip = target_index;
        Ok(())
    }

    fn read_u8(&mut self) -> VmResult<u8> {
        let value = *self
            .program
            .code()
            .get(self.ip)
            .ok_or(VmError::BytecodeBounds)?;
        self.ip += 1;
        Ok(value)
    }

    fn read_u16(&mut self) -> VmResult<u16> {
        Ok(u16::from_le_bytes(self.read_array()?))
    }

    fn read_u32(&mut self) -> VmResult<u32> {
        Ok(u32::from_le_bytes(self.read_array()?))
    }

    fn read_array<const N: usize>(&mut self) -> VmResult<[u8; N]> {
        let end = self.ip.checked_add(N).ok_or(VmError::BytecodeBounds)?;
        let bytes = self
            .program
            .code()
            .get(self.ip..end)
            .ok_or(VmError::BytecodeBounds)?;
        self.ip = end;
        bytes.try_into().map_err(|_| VmError::BytecodeBounds)
    }
}

fn as_float(value: NumericValue) -> f64 {
    match value {
        NumericValue::Int(value) => value as f64,
        NumericValue::Float(value) => value,
    }
}

fn checked_int_div(lhs: i64, rhs: i64) -> VmResult<i64> {
    if rhs == 0 {
        return Err(VmError::DivisionByZero);
    }
    if lhs == i64::MIN && rhs == -1 {
        return Err(VmError::IntegerOverflow("division"));
    }
    Ok(lhs / rhs)
}

fn checked_int_rem(lhs: i64, rhs: i64) -> VmResult<i64> {
    if rhs == 0 {
        return Err(VmError::DivisionByZero);
    }
    if lhs == i64::MIN && rhs == -1 {
        return Err(VmError::IntegerOverflow("remainder"));
    }
    Ok(lhs % rhs)
}
