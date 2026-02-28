#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Int(i64),
    Float(f64),
    Bool(bool),
    String(String),
    Array(Vec<Value>),
    Map(Vec<(Value, Value)>),
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
    pub imports: Vec<HostImport>,
    pub debug: Option<crate::debug_info::DebugInfo>,
}

impl Program {
    pub fn new(constants: Vec<Value>, code: Vec<u8>) -> Self {
        Self {
            constants,
            code,
            imports: Vec::new(),
            debug: None,
        }
    }

    pub fn with_debug(
        constants: Vec<Value>,
        code: Vec<u8>,
        debug: Option<crate::debug_info::DebugInfo>,
    ) -> Self {
        Self {
            constants,
            code,
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
        Self {
            constants,
            code,
            imports,
            debug,
        }
    }
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
}

impl OpCode {
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
            _ => None,
        }
    }
}
