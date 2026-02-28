#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u16)]
pub(crate) enum BuiltinFunction {
    Len = 0,
    Slice = 1,
    Concat = 2,
    ArrayNew = 3,
    ArrayPush = 4,
    MapNew = 5,
    Get = 6,
    Set = 7,
    IoOpen = 8,
    IoPopen = 9,
    IoReadAll = 10,
    IoReadLine = 11,
    IoWrite = 12,
    IoFlush = 13,
    IoClose = 14,
    IoExists = 15,
    ToString = 16,
    TypeOf = 17,
    Assert = 18,
}

pub(crate) const BUILTIN_CALL_BASE: u16 = 0xFFF0;
pub(crate) const BUILTIN_CALL_COUNT: u16 = 16;

impl BuiltinFunction {
    pub(crate) fn name(self) -> &'static str {
        match self {
            BuiltinFunction::Len => "len",
            BuiltinFunction::Slice => "slice",
            BuiltinFunction::Concat => "concat",
            BuiltinFunction::ArrayNew => "array_new",
            BuiltinFunction::ArrayPush => "array_push",
            BuiltinFunction::MapNew => "map_new",
            BuiltinFunction::Get => "get",
            BuiltinFunction::Set => "set",
            BuiltinFunction::IoOpen => "io_open",
            BuiltinFunction::IoPopen => "io_popen",
            BuiltinFunction::IoReadAll => "io_read_all",
            BuiltinFunction::IoReadLine => "io_read_line",
            BuiltinFunction::IoWrite => "io_write",
            BuiltinFunction::IoFlush => "io_flush",
            BuiltinFunction::IoClose => "io_close",
            BuiltinFunction::IoExists => "io_exists",
            BuiltinFunction::ToString => "__to_string",
            BuiltinFunction::TypeOf => "type_of",
            BuiltinFunction::Assert => "assert",
        }
    }

    pub(crate) fn arity(self) -> u8 {
        match self {
            BuiltinFunction::Len => 1,
            BuiltinFunction::Slice => 3,
            BuiltinFunction::Concat => 2,
            BuiltinFunction::ArrayNew => 0,
            BuiltinFunction::ArrayPush => 2,
            BuiltinFunction::MapNew => 0,
            BuiltinFunction::Get => 2,
            BuiltinFunction::Set => 3,
            BuiltinFunction::IoOpen => 2,
            BuiltinFunction::IoPopen => 2,
            BuiltinFunction::IoReadAll => 1,
            BuiltinFunction::IoReadLine => 1,
            BuiltinFunction::IoWrite => 2,
            BuiltinFunction::IoFlush => 1,
            BuiltinFunction::IoClose => 1,
            BuiltinFunction::IoExists => 1,
            BuiltinFunction::ToString => 1,
            BuiltinFunction::TypeOf => 1,
            BuiltinFunction::Assert => 1,
        }
    }

    pub(crate) fn call_index(self) -> u16 {
        match self {
            BuiltinFunction::ToString => BUILTIN_CALL_BASE - 3,
            BuiltinFunction::TypeOf => BUILTIN_CALL_BASE - 2,
            BuiltinFunction::Assert => BUILTIN_CALL_BASE - 1,
            _ => BUILTIN_CALL_BASE + self as u16,
        }
    }

    pub(crate) fn from_call_index(index: u16) -> Option<Self> {
        if index == BUILTIN_CALL_BASE - 3 {
            return Some(BuiltinFunction::ToString);
        }
        if index == BUILTIN_CALL_BASE - 2 {
            return Some(BuiltinFunction::TypeOf);
        }
        if index == BUILTIN_CALL_BASE - 1 {
            return Some(BuiltinFunction::Assert);
        }
        let offset = index.checked_sub(BUILTIN_CALL_BASE)?;
        if offset >= BUILTIN_CALL_COUNT {
            return None;
        }
        match offset {
            0 => Some(BuiltinFunction::Len),
            1 => Some(BuiltinFunction::Slice),
            2 => Some(BuiltinFunction::Concat),
            3 => Some(BuiltinFunction::ArrayNew),
            4 => Some(BuiltinFunction::ArrayPush),
            5 => Some(BuiltinFunction::MapNew),
            6 => Some(BuiltinFunction::Get),
            7 => Some(BuiltinFunction::Set),
            8 => Some(BuiltinFunction::IoOpen),
            9 => Some(BuiltinFunction::IoPopen),
            10 => Some(BuiltinFunction::IoReadAll),
            11 => Some(BuiltinFunction::IoReadLine),
            12 => Some(BuiltinFunction::IoWrite),
            13 => Some(BuiltinFunction::IoFlush),
            14 => Some(BuiltinFunction::IoClose),
            15 => Some(BuiltinFunction::IoExists),
            _ => None,
        }
    }

    pub(crate) fn from_name(name: &str) -> Option<Self> {
        match name {
            "len" => Some(BuiltinFunction::Len),
            "slice" => Some(BuiltinFunction::Slice),
            "concat" => Some(BuiltinFunction::Concat),
            "array_new" => Some(BuiltinFunction::ArrayNew),
            "array_push" => Some(BuiltinFunction::ArrayPush),
            "map_new" => Some(BuiltinFunction::MapNew),
            "get" => Some(BuiltinFunction::Get),
            "set" => Some(BuiltinFunction::Set),
            "io_open" => Some(BuiltinFunction::IoOpen),
            "io_popen" => Some(BuiltinFunction::IoPopen),
            "io_read_all" => Some(BuiltinFunction::IoReadAll),
            "io_read_line" => Some(BuiltinFunction::IoReadLine),
            "io_write" => Some(BuiltinFunction::IoWrite),
            "io_flush" => Some(BuiltinFunction::IoFlush),
            "io_close" => Some(BuiltinFunction::IoClose),
            "io_exists" => Some(BuiltinFunction::IoExists),
            "type_of" => Some(BuiltinFunction::TypeOf),
            "assert" => Some(BuiltinFunction::Assert),
            _ => None,
        }
    }
}
