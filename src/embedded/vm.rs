use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use super::{
    HostBinding, HostDispatcher, HostFunction, OpCode, Program, Value, VmError,
    resolve_host_functions,
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

pub struct Vm<C = ()> {
    program: Program,
    ip: usize,
    stack: Vec<Value>,
    locals: Vec<Value>,
    host_functions: Vec<HostFunction<C>>,
    host_dispatcher: Option<HostDispatcher<C>>,
    context: C,
    fuel: Option<u64>,
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
        Self {
            program,
            ip: 0,
            stack: Vec::new(),
            locals: vec![Value::Null; local_count],
            host_functions,
            host_dispatcher,
            context,
            fuel: None,
        }
    }

    pub fn run(&mut self) -> VmResult<VmStatus> {
        loop {
            self.charge_fuel()?;
            let raw = self.read_u8()?;
            let opcode = OpCode::try_from(raw).map_err(|()| VmError::InvalidOpcode(raw))?;
            match opcode {
                OpCode::Nop => {}
                OpCode::Ret => return Ok(VmStatus::Halted),
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
                    let value = self
                        .locals
                        .get(usize::from(index))
                        .cloned()
                        .ok_or(VmError::InvalidLocal(index))?;
                    self.stack.push(value);
                }
                OpCode::Stloc => {
                    let index = self.read_u8()?;
                    let value = self.pop()?;
                    self.store_local(index, value)?;
                }
                OpCode::Call => {
                    let index = self.read_u16()?;
                    let arity = self.read_u8()?;
                    self.call_host(index, arity)?;
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
        let slot = self
            .locals
            .get_mut(usize::from(index))
            .ok_or(VmError::InvalidLocal(index))?;
        *slot = value;
        Ok(())
    }

    fn jump(&mut self, target: u32) -> VmResult<()> {
        let target_index = target as usize;
        if target_index >= self.program.code().len() {
            return Err(VmError::InvalidJump(target));
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
