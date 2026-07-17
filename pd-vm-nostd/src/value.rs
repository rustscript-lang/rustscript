use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::RefCell;

pub type SharedString = Rc<String>;
pub type SharedBytes = Rc<Vec<u8>>;
pub type SharedArray = Rc<Vec<Value>>;
pub type SharedMap = Rc<Vec<(Value, Value)>>;
pub type ProgramInstanceId = u64;
pub type SharedCallable = Rc<CallableValue>;
pub type CallableEnvironment = Rc<RefCell<Vec<Value>>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallableKind {
    FunctionItem,
    Closure,
    HostFunction,
}

#[derive(Clone, Debug)]
pub struct CallableValue {
    pub program_instance: ProgramInstanceId,
    pub prototype_id: u32,
    pub kind: CallableKind,
    pub env: Option<CallableEnvironment>,
}

#[derive(Clone, Debug)]
pub enum Value {
    Null,
    Int(i64),
    Float(f64),
    Bool(bool),
    String(SharedString),
    Bytes(SharedBytes),
    Array(SharedArray),
    Map(SharedMap),
    Callable(SharedCallable),
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Null, Self::Null) => true,
            (Self::Int(lhs), Self::Int(rhs)) => lhs == rhs,
            (Self::Float(lhs), Self::Float(rhs)) => lhs.to_bits() == rhs.to_bits(),
            (Self::Bool(lhs), Self::Bool(rhs)) => lhs == rhs,
            (Self::String(lhs), Self::String(rhs)) => lhs == rhs,
            (Self::Bytes(lhs), Self::Bytes(rhs)) => lhs == rhs,
            (Self::Array(lhs), Self::Array(rhs)) => lhs == rhs,
            (Self::Map(lhs), Self::Map(rhs)) => lhs == rhs,
            (Self::Callable(lhs), Self::Callable(rhs)) => {
                if lhs.env.is_none() && rhs.env.is_none() {
                    lhs.program_instance == rhs.program_instance
                        && lhs.prototype_id == rhs.prototype_id
                        && lhs.kind == rhs.kind
                } else {
                    Rc::ptr_eq(lhs, rhs)
                }
            }
            _ => false,
        }
    }
}

impl Value {
    pub fn string(value: impl Into<String>) -> Self {
        Self::String(Rc::new(value.into()))
    }

    pub fn bytes(value: impl Into<Vec<u8>>) -> Self {
        Self::Bytes(Rc::new(value.into()))
    }

    pub fn array(values: Vec<Value>) -> Self {
        Self::Array(Rc::new(values))
    }

    pub fn map(entries: Vec<(Value, Value)>) -> Self {
        Self::Map(Rc::new(entries))
    }
}
