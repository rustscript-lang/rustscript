use core::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VmError {
    StackUnderflow,
    TypeMismatch(&'static str),
    DivisionByZero,
    IntegerOverflow(&'static str),
    InvalidShift(i64),
    InvalidConstant(u32),
    InvalidLocal(u8),
    InvalidCall(u16),
    HostCallsUnavailable(u16),
    InvalidOpcode(u8),
    BytecodeBounds,
    InvalidJump(u32),
}

impl fmt::Display for VmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StackUnderflow => f.write_str("stack underflow"),
            Self::TypeMismatch(expected) => write!(f, "type mismatch: expected {expected}"),
            Self::DivisionByZero => f.write_str("division by zero"),
            Self::IntegerOverflow(operation) => {
                write!(f, "integer overflow in {operation}")
            }
            Self::InvalidShift(value) => write!(f, "invalid shift amount: {value}"),
            Self::InvalidConstant(index) => write!(f, "invalid constant index: {index}"),
            Self::InvalidLocal(index) => write!(f, "invalid local index: {index}"),
            Self::InvalidCall(index) => write!(f, "invalid call index: {index}"),
            Self::HostCallsUnavailable(index) => {
                write!(f, "host calls are unavailable for import: {index}")
            }
            Self::InvalidOpcode(opcode) => write!(f, "invalid opcode: {opcode:#04x}"),
            Self::BytecodeBounds => f.write_str("bytecode operand is out of bounds"),
            Self::InvalidJump(target) => write!(f, "invalid jump target: {target}"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for VmError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireError {
    UnexpectedEof,
    InvalidMagic([u8; 4]),
    UnsupportedVersion(u16),
    UnsupportedFlags(u16),
    InvalidConstantTag(u8),
    InvalidBool(u8),
    InvalidTypeMapFlag(u8),
    InvalidDebugFlag(u8),
    InvalidValueType(u8),
    InvalidUtf8,
    LengthTooLarge(&'static str, usize),
    SchemaTooDeep,
    TrailingBytes,
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnexpectedEof => f.write_str("unexpected end of VMBC input"),
            Self::InvalidMagic(found) => write!(f, "invalid VMBC magic: {found:?}"),
            Self::UnsupportedVersion(version) => {
                write!(f, "unsupported VMBC version: {version}")
            }
            Self::UnsupportedFlags(flags) => write!(f, "unsupported VMBC flags: {flags}"),
            Self::InvalidConstantTag(tag) => write!(f, "invalid VMBC constant tag: {tag}"),
            Self::InvalidBool(value) => write!(f, "invalid VMBC boolean: {value}"),
            Self::InvalidTypeMapFlag(value) => write!(f, "invalid type-map flag: {value}"),
            Self::InvalidDebugFlag(value) => write!(f, "invalid debug flag: {value}"),
            Self::InvalidValueType(value) => write!(f, "invalid value type: {value}"),
            Self::InvalidUtf8 => f.write_str("invalid UTF-8 in VMBC string"),
            Self::LengthTooLarge(field, length) => {
                write!(f, "{field} length is too large: {length}")
            }
            Self::SchemaTooDeep => f.write_str("VMBC type schema nesting is too deep"),
            Self::TrailingBytes => f.write_str("trailing bytes after VMBC payload"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for WireError {}
