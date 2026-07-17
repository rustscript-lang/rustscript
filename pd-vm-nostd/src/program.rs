use alloc::string::String;
use alloc::vec::Vec;

use super::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ValueType {
    Unknown = 0,
    Null = 1,
    Int = 2,
    Float = 3,
    Bool = 4,
    String = 5,
    Bytes = 6,
    Array = 7,
    Map = 8,
    Callable = 9,
}

impl TryFrom<u8> for ValueType {
    type Error = ();

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Unknown),
            1 => Ok(Self::Null),
            2 => Ok(Self::Int),
            3 => Ok(Self::Float),
            4 => Ok(Self::Bool),
            5 => Ok(Self::String),
            6 => Ok(Self::Bytes),
            7 => Ok(Self::Array),
            8 => Ok(Self::Map),
            9 => Ok(Self::Callable),
            _ => Err(()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallableTarget {
    ScriptFunction(u32),
    HostImport(u16),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum CaptureBindingMode {
    Copy = 0,
    Borrow = 1,
    BorrowMut = 2,
    Move = 3,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScriptFunction {
    pub entry_ip: u32,
    pub end_ip: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallablePrototype {
    pub kind: super::CallableKind,
    pub target: CallableTarget,
    pub arity: u8,
    pub frame_local_count: usize,
    pub parameter_slots: Vec<u16>,
    pub capture_source_slots: Vec<u16>,
    pub capture_slots: Vec<u16>,
    pub capture_modes: Vec<CaptureBindingMode>,
    pub self_slot: Option<u16>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FunctionRegion {
    pub start_ip: u32,
    pub end_ip: u32,
    pub prototype_id: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RootCallableBinding {
    pub local_slot: u16,
    pub prototype_id: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExportedCallable {
    pub name: String,
    pub local_slot: u16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostImport {
    pub name: String,
    pub arity: u8,
    pub return_type: ValueType,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Program {
    constants: Vec<Value>,
    code: Vec<u8>,
    local_count: usize,
    imports: Vec<HostImport>,
    script_functions: Vec<ScriptFunction>,
    callable_prototypes: Vec<CallablePrototype>,
    function_regions: Vec<FunctionRegion>,
    root_callable_bindings: Vec<RootCallableBinding>,
    exported_callables: Vec<ExportedCallable>,
}

impl Program {
    pub(crate) fn new(constants: Vec<Value>, code: Vec<u8>, imports: Vec<HostImport>) -> Self {
        let local_count = infer_local_count(&code);
        Self {
            constants,
            code,
            local_count,
            imports,
            script_functions: Vec::new(),
            callable_prototypes: Vec::new(),
            function_regions: Vec::new(),
            root_callable_bindings: Vec::new(),
            exported_callables: Vec::new(),
        }
    }

    pub(crate) fn with_local_count(mut self, local_count: usize) -> Self {
        self.local_count = self.local_count.max(local_count);
        self
    }

    pub(crate) fn with_callable_metadata(
        mut self,
        script_functions: Vec<ScriptFunction>,
        callable_prototypes: Vec<CallablePrototype>,
        function_regions: Vec<FunctionRegion>,
        root_callable_bindings: Vec<RootCallableBinding>,
        exported_callables: Vec<ExportedCallable>,
    ) -> Self {
        self.script_functions = script_functions;
        self.callable_prototypes = callable_prototypes;
        self.function_regions = function_regions;
        self.local_count = self
            .local_count
            .max(
                root_callable_bindings
                    .iter()
                    .map(|binding| binding.local_slot as usize + 1)
                    .max()
                    .unwrap_or(0),
            )
            .max(
                exported_callables
                    .iter()
                    .map(|exported| exported.local_slot as usize + 1)
                    .max()
                    .unwrap_or(0),
            );
        self.root_callable_bindings = root_callable_bindings;
        self.exported_callables = exported_callables;
        self
    }

    pub fn script_functions(&self) -> &[ScriptFunction] {
        &self.script_functions
    }

    pub fn callable_prototypes(&self) -> &[CallablePrototype] {
        &self.callable_prototypes
    }

    pub fn function_regions(&self) -> &[FunctionRegion] {
        &self.function_regions
    }

    pub fn root_callable_bindings(&self) -> &[RootCallableBinding] {
        &self.root_callable_bindings
    }

    pub fn exported_callables(&self) -> &[ExportedCallable] {
        &self.exported_callables
    }

    pub fn constants(&self) -> &[Value] {
        &self.constants
    }

    pub fn code(&self) -> &[u8] {
        &self.code
    }

    pub fn local_count(&self) -> usize {
        self.local_count
    }

    pub fn imports(&self) -> &[HostImport] {
        &self.imports
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
    Cgt = 0x0a,
    Br = 0x0b,
    Brfalse = 0x0c,
    Pop = 0x0d,
    Dup = 0x0e,
    Ldloc = 0x0f,
    Stloc = 0x10,
    Call = 0x11,
    Shl = 0x12,
    Shr = 0x13,
    Mod = 0x14,
    And = 0x15,
    Or = 0x16,
    Not = 0x17,
    Lshr = 0x18,
    CallValue = 0x19,
}

impl OpCode {
    pub const fn operand_len(self) -> usize {
        match self {
            Self::Ldc | Self::Br | Self::Brfalse => 4,
            Self::Ldloc | Self::Stloc | Self::CallValue => 1,
            Self::Call => 3,
            _ => 0,
        }
    }
}

impl TryFrom<u8> for OpCode {
    type Error = ();

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0x00 => Ok(Self::Nop),
            0x01 => Ok(Self::Ret),
            0x02 => Ok(Self::Ldc),
            0x03 => Ok(Self::Add),
            0x04 => Ok(Self::Sub),
            0x05 => Ok(Self::Mul),
            0x06 => Ok(Self::Div),
            0x07 => Ok(Self::Neg),
            0x08 => Ok(Self::Ceq),
            0x09 => Ok(Self::Clt),
            0x0a => Ok(Self::Cgt),
            0x0b => Ok(Self::Br),
            0x0c => Ok(Self::Brfalse),
            0x0d => Ok(Self::Pop),
            0x0e => Ok(Self::Dup),
            0x0f => Ok(Self::Ldloc),
            0x10 => Ok(Self::Stloc),
            0x11 => Ok(Self::Call),
            0x12 => Ok(Self::Shl),
            0x13 => Ok(Self::Shr),
            0x14 => Ok(Self::Mod),
            0x15 => Ok(Self::And),
            0x16 => Ok(Self::Or),
            0x17 => Ok(Self::Not),
            0x18 => Ok(Self::Lshr),
            0x19 => Ok(Self::CallValue),
            _ => Err(()),
        }
    }
}

fn infer_local_count(code: &[u8]) -> usize {
    let mut ip = 0;
    let mut max_local = None::<u8>;
    while let Some(&raw) = code.get(ip) {
        let Ok(opcode) = OpCode::try_from(raw) else {
            break;
        };
        ip += 1;
        let operand_len = opcode.operand_len();
        if ip.saturating_add(operand_len) > code.len() {
            break;
        }
        if matches!(opcode, OpCode::Ldloc | OpCode::Stloc) {
            let index = code[ip];
            max_local = Some(max_local.map_or(index, |current| current.max(index)));
        }
        ip += operand_len;
    }
    max_local.map_or(0, |index| usize::from(index) + 1)
}
