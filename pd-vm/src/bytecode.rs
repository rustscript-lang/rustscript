use std::sync::Arc;

pub type SharedString = Arc<String>;
pub type SharedArray = Arc<Vec<Value>>;
pub type SharedMap = Arc<Vec<(Value, Value)>>;

#[derive(Clone, Debug)]
pub enum Value {
    Null,
    Int(i64),
    Float(f64),
    Bool(bool),
    String(SharedString),
    Array(SharedArray),
    Map(SharedMap),
}

impl Value {
    pub fn string(value: impl Into<String>) -> Self {
        Self::String(Arc::new(value.into()))
    }

    pub fn array(values: Vec<Value>) -> Self {
        Self::Array(Arc::new(values))
    }

    pub fn map(entries: Vec<(Value, Value)>) -> Self {
        Self::Map(Arc::new(entries))
    }

    pub fn into_owned_string(self) -> Result<String, Self> {
        match self {
            Self::String(value) => Ok(unwrap_or_clone_shared(value)),
            other => Err(other),
        }
    }

    pub fn into_owned_array(self) -> Result<Vec<Value>, Self> {
        match self {
            Self::Array(values) => Ok(unwrap_or_clone_shared(values)),
            other => Err(other),
        }
    }

    pub fn into_owned_map(self) -> Result<Vec<(Value, Value)>, Self> {
        match self {
            Self::Map(entries) => Ok(unwrap_or_clone_shared(entries)),
            other => Err(other),
        }
    }
}

pub(crate) fn unwrap_or_clone_shared<T: Clone>(value: Arc<T>) -> T {
    match Arc::try_unwrap(value) {
        Ok(inner) => inner,
        Err(shared) => (*shared).clone(),
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Null, Self::Null) => true,
            (Self::Int(lhs), Self::Int(rhs)) => lhs == rhs,
            (Self::Float(lhs), Self::Float(rhs)) => lhs == rhs,
            (Self::Bool(lhs), Self::Bool(rhs)) => lhs == rhs,
            (Self::String(lhs), Self::String(rhs)) => lhs == rhs,
            (Self::Array(lhs), Self::Array(rhs)) => lhs == rhs,
            (Self::Map(lhs), Self::Map(rhs)) => map_entries_eq(lhs.as_slice(), rhs.as_slice()),
            _ => false,
        }
    }
}

fn map_entries_eq(lhs: &[(Value, Value)], rhs: &[(Value, Value)]) -> bool {
    if lhs.len() != rhs.len() {
        return false;
    }
    let mut matched = vec![false; rhs.len()];
    'outer: for lhs_entry in lhs {
        for (index, rhs_entry) in rhs.iter().enumerate() {
            if matched[index] || lhs_entry != rhs_entry {
                continue;
            }
            matched[index] = true;
            continue 'outer;
        }
        return false;
    }
    true
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct HostImport {
    pub name: String,
    pub arity: u8,
}

#[derive(Clone, Debug)]
pub struct Program {
    pub constants: Vec<Value>,
    pub code: Vec<u8>,
    pub local_count: usize,
    pub imports: Vec<HostImport>,
    pub debug: Option<crate::debug_info::DebugInfo>,
}

impl Program {
    pub fn new(constants: Vec<Value>, code: Vec<u8>) -> Self {
        let local_count = infer_local_count_from_code(&code);
        Self {
            constants,
            code,
            local_count,
            imports: Vec::new(),
            debug: None,
        }
    }

    pub fn with_debug(
        constants: Vec<Value>,
        code: Vec<u8>,
        debug: Option<crate::debug_info::DebugInfo>,
    ) -> Self {
        let local_count = infer_local_count_from_code(&code);
        Self {
            constants,
            code,
            local_count,
            imports: Vec::new(),
            debug,
        }
    }

    pub fn with_imports_and_debug(
        constants: Vec<Value>,
        code: Vec<u8>,
        imports: Vec<HostImport>,
        debug: Option<crate::debug_info::DebugInfo>,
    ) -> Self {
        let local_count = infer_local_count_from_code(&code);
        Self {
            constants,
            code,
            local_count,
            imports,
            debug,
        }
    }

    pub fn with_local_count(mut self, local_count: usize) -> Self {
        self.local_count = local_count;
        self
    }
}

fn infer_local_count_from_code(code: &[u8]) -> usize {
    let mut ip = 0usize;
    let mut max_local_index: Option<u8> = None;

    while let Some(&opcode) = code.get(ip) {
        ip += 1;
        let Ok(opcode) = OpCode::try_from(opcode) else {
            break;
        };
        let operand_len = opcode.operand_len();
        if ip + operand_len > code.len() {
            break;
        }
        match opcode {
            OpCode::Ldloc | OpCode::Stloc => {
                let index = code[ip];
                max_local_index = Some(max_local_index.map_or(index, |prev| prev.max(index)));
            }
            _ => {}
        }
        ip += operand_len;
    }

    max_local_index.map_or(0, |index| index as usize + 1)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum OpCode {
    Nop = 0x00,
    Ret = 0x01,
    Ldc = 0x02,
    Add = 0x03,
    Sub = 0x04,
    Mul = 0x05,
    Div = 0x06,
    Neg = 0x07,
    Ceq = 0x08,
    Clt = 0x09,
    Cgt = 0x0A,
    Br = 0x0B,
    Brfalse = 0x0C,
    Pop = 0x0D,
    Dup = 0x0E,
    Ldloc = 0x0F,
    Stloc = 0x10,
    Call = 0x11,
    Shl = 0x12,
    Shr = 0x13,
    Mod = 0x14,
    And = 0x15,
    Or = 0x16,
    Not = 0x17,
    Lshr = 0x18,
}

impl TryFrom<u8> for OpCode {
    type Error = ();

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            x if x == Self::Nop as u8 => Ok(Self::Nop),
            x if x == Self::Ret as u8 => Ok(Self::Ret),
            x if x == Self::Ldc as u8 => Ok(Self::Ldc),
            x if x == Self::Add as u8 => Ok(Self::Add),
            x if x == Self::Sub as u8 => Ok(Self::Sub),
            x if x == Self::Mul as u8 => Ok(Self::Mul),
            x if x == Self::Div as u8 => Ok(Self::Div),
            x if x == Self::Neg as u8 => Ok(Self::Neg),
            x if x == Self::Ceq as u8 => Ok(Self::Ceq),
            x if x == Self::Clt as u8 => Ok(Self::Clt),
            x if x == Self::Cgt as u8 => Ok(Self::Cgt),
            x if x == Self::Br as u8 => Ok(Self::Br),
            x if x == Self::Brfalse as u8 => Ok(Self::Brfalse),
            x if x == Self::Pop as u8 => Ok(Self::Pop),
            x if x == Self::Dup as u8 => Ok(Self::Dup),
            x if x == Self::Ldloc as u8 => Ok(Self::Ldloc),
            x if x == Self::Stloc as u8 => Ok(Self::Stloc),
            x if x == Self::Call as u8 => Ok(Self::Call),
            x if x == Self::Shl as u8 => Ok(Self::Shl),
            x if x == Self::Shr as u8 => Ok(Self::Shr),
            x if x == Self::Mod as u8 => Ok(Self::Mod),
            x if x == Self::And as u8 => Ok(Self::And),
            x if x == Self::Or as u8 => Ok(Self::Or),
            x if x == Self::Not as u8 => Ok(Self::Not),
            x if x == Self::Lshr as u8 => Ok(Self::Lshr),
            _ => Err(()),
        }
    }
}

impl OpCode {
    pub const fn operand_len(self) -> usize {
        match self {
            Self::Nop
            | Self::Ret
            | Self::Add
            | Self::Sub
            | Self::Mul
            | Self::Div
            | Self::Neg
            | Self::Ceq
            | Self::Clt
            | Self::Cgt
            | Self::Pop
            | Self::Dup
            | Self::Shl
            | Self::Shr
            | Self::Mod
            | Self::And
            | Self::Or
            | Self::Not
            | Self::Lshr => 0,
            Self::Ldc | Self::Br | Self::Brfalse => 4,
            Self::Ldloc | Self::Stloc => 1,
            Self::Call => 3,
        }
    }

    pub fn mnemonic(self) -> &'static str {
        match self {
            OpCode::Nop => "nop",
            OpCode::Ret => "ret",
            OpCode::Ldc => "ldc",
            OpCode::Add => "add",
            OpCode::Sub => "sub",
            OpCode::Mul => "mul",
            OpCode::Div => "div",
            OpCode::Neg => "neg",
            OpCode::Ceq => "ceq",
            OpCode::Clt => "clt",
            OpCode::Cgt => "cgt",
            OpCode::Br => "br",
            OpCode::Brfalse => "brfalse",
            OpCode::Pop => "pop",
            OpCode::Dup => "dup",
            OpCode::Ldloc => "ldloc",
            OpCode::Stloc => "stloc",
            OpCode::Call => "call",
            OpCode::Shl => "shl",
            OpCode::Shr => "shr",
            OpCode::Mod => "mod",
            OpCode::And => "and",
            OpCode::Or => "or",
            OpCode::Not => "not",
            OpCode::Lshr => "lshr",
        }
    }

    pub fn parse_mnemonic(op: &str) -> Option<Self> {
        match op {
            "nop" => Some(OpCode::Nop),
            "ret" => Some(OpCode::Ret),
            "ldc" => Some(OpCode::Ldc),
            "add" => Some(OpCode::Add),
            "sub" => Some(OpCode::Sub),
            "mul" => Some(OpCode::Mul),
            "div" => Some(OpCode::Div),
            "neg" => Some(OpCode::Neg),
            "ceq" => Some(OpCode::Ceq),
            "clt" => Some(OpCode::Clt),
            "cgt" => Some(OpCode::Cgt),
            "br" => Some(OpCode::Br),
            "brfalse" => Some(OpCode::Brfalse),
            "pop" => Some(OpCode::Pop),
            "dup" => Some(OpCode::Dup),
            "ldloc" => Some(OpCode::Ldloc),
            "stloc" => Some(OpCode::Stloc),
            "call" => Some(OpCode::Call),
            "shl" => Some(OpCode::Shl),
            "shr" => Some(OpCode::Shr),
            "mod" => Some(OpCode::Mod),
            "and" => Some(OpCode::And),
            "or" => Some(OpCode::Or),
            "not" => Some(OpCode::Not),
            "lshr" => Some(OpCode::Lshr),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heap_value_clone_shares_backing() {
        let string = Value::string("hello");
        let string_clone = string.clone();
        let (Value::String(lhs), Value::String(rhs)) = (&string, &string_clone) else {
            panic!("expected string values");
        };
        assert!(Arc::ptr_eq(lhs, rhs));

        let array = Value::array(vec![Value::Int(1), Value::Int(2)]);
        let array_clone = array.clone();
        let (Value::Array(lhs), Value::Array(rhs)) = (&array, &array_clone) else {
            panic!("expected array values");
        };
        assert!(Arc::ptr_eq(lhs, rhs));

        let map = Value::map(vec![(Value::string("k"), Value::Int(9))]);
        let map_clone = map.clone();
        let (Value::Map(lhs), Value::Map(rhs)) = (&map, &map_clone) else {
            panic!("expected map values");
        };
        assert!(Arc::ptr_eq(lhs, rhs));
    }
}
