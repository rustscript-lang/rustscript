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
        match opcode {
            x if x == OpCode::Nop as u8
                || x == OpCode::Ret as u8
                || x == OpCode::Add as u8
                || x == OpCode::Sub as u8
                || x == OpCode::Mul as u8
                || x == OpCode::Div as u8
                || x == OpCode::Neg as u8
                || x == OpCode::Ceq as u8
                || x == OpCode::Clt as u8
                || x == OpCode::Cgt as u8
                || x == OpCode::Pop as u8
                || x == OpCode::Dup as u8
                || x == OpCode::Shl as u8
                || x == OpCode::Shr as u8
                || x == OpCode::Mod as u8
                || x == OpCode::And as u8
                || x == OpCode::Or as u8 => {}
            x if x == OpCode::Ldc as u8
                || x == OpCode::Br as u8
                || x == OpCode::Brfalse as u8 => {
                    if ip + 4 > code.len() {
                        break;
                    }
                    ip += 4;
                }
            x if x == OpCode::Ldloc as u8 || x == OpCode::Stloc as u8 => {
                let Some(&index) = code.get(ip) else {
                    break;
                };
                ip += 1;
                max_local_index = Some(max_local_index.map_or(index, |prev| prev.max(index)));
            }
            x if x == OpCode::Call as u8 => {
                if ip + 3 > code.len() {
                    break;
                }
                ip += 3;
            }
            _ => break,
        }
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
