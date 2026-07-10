use core::fmt;

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
