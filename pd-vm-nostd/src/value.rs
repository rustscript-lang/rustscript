use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;

pub type SharedString = Rc<String>;
pub type SharedBytes = Rc<Vec<u8>>;
pub type SharedArray = Rc<Vec<Value>>;
pub type SharedMap = Rc<Vec<(Value, Value)>>;

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Int(i64),
    Float(f64),
    Bool(bool),
    String(SharedString),
    Bytes(SharedBytes),
    Array(SharedArray),
    Map(SharedMap),
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
