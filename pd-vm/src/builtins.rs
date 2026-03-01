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
    Keys = 8,
    IoOpen = 9,
    IoPopen = 10,
    IoReadAll = 11,
    IoReadLine = 12,
    IoWrite = 13,
    IoFlush = 14,
    IoClose = 15,
    IoExists = 16,
    Count = 17,
    ReIsMatch = 18,
    ReFind = 19,
    ReReplace = 20,
    ReSplit = 21,
    ReCaptures = 22,
    ToString = 23,
    TypeOf = 24,
    Assert = 25,
}

pub(crate) const BUILTIN_CALL_BASE: u16 = 0xFFE0;
/// Number of builtins in the main range (indices 0..22 above BUILTIN_CALL_BASE).
/// ToString, TypeOf, and Assert live at special indices below BUILTIN_CALL_BASE.
pub(crate) const BUILTIN_CALL_COUNT: u16 = 23;

impl BuiltinFunction {
    pub(crate) fn from_namespaced_name(name: &str) -> Option<Self> {
        match name.trim() {
            "io::open" => Some(BuiltinFunction::IoOpen),
            "io::popen" => Some(BuiltinFunction::IoPopen),
            "io::read_all" => Some(BuiltinFunction::IoReadAll),
            "io::read_line" => Some(BuiltinFunction::IoReadLine),
            "io::write" => Some(BuiltinFunction::IoWrite),
            "io::flush" => Some(BuiltinFunction::IoFlush),
            "io::close" => Some(BuiltinFunction::IoClose),
            "io::exists" => Some(BuiltinFunction::IoExists),
            "re::match" | "re::is_match" => Some(BuiltinFunction::ReIsMatch),
            "re::find" => Some(BuiltinFunction::ReFind),
            "re::replace" => Some(BuiltinFunction::ReReplace),
            "re::split" => Some(BuiltinFunction::ReSplit),
            "re::captures" => Some(BuiltinFunction::ReCaptures),
            _ => None,
        }
    }

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
            BuiltinFunction::Keys => "keys",
            BuiltinFunction::IoOpen => "io_open",
            BuiltinFunction::IoPopen => "io_popen",
            BuiltinFunction::IoReadAll => "io_read_all",
            BuiltinFunction::IoReadLine => "io_read_line",
            BuiltinFunction::IoWrite => "io_write",
            BuiltinFunction::IoFlush => "io_flush",
            BuiltinFunction::IoClose => "io_close",
            BuiltinFunction::IoExists => "io_exists",
            BuiltinFunction::Count => "count",
            BuiltinFunction::ReIsMatch => "re_is_match",
            BuiltinFunction::ReFind => "re_find",
            BuiltinFunction::ReReplace => "re_replace",
            BuiltinFunction::ReSplit => "re_split",
            BuiltinFunction::ReCaptures => "re_captures",
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
            BuiltinFunction::Keys => 1,
            BuiltinFunction::IoOpen => 2,
            BuiltinFunction::IoPopen => 2,
            BuiltinFunction::IoReadAll => 1,
            BuiltinFunction::IoReadLine => 1,
            BuiltinFunction::IoWrite => 2,
            BuiltinFunction::IoFlush => 1,
            BuiltinFunction::IoClose => 1,
            BuiltinFunction::IoExists => 1,
            BuiltinFunction::Count => 1,
            BuiltinFunction::ReIsMatch => 2,
            BuiltinFunction::ReFind => 2,
            BuiltinFunction::ReReplace => 3,
            BuiltinFunction::ReSplit => 2,
            BuiltinFunction::ReCaptures => 2,
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
            8 => Some(BuiltinFunction::Keys),
            9 => Some(BuiltinFunction::IoOpen),
            10 => Some(BuiltinFunction::IoPopen),
            11 => Some(BuiltinFunction::IoReadAll),
            12 => Some(BuiltinFunction::IoReadLine),
            13 => Some(BuiltinFunction::IoWrite),
            14 => Some(BuiltinFunction::IoFlush),
            15 => Some(BuiltinFunction::IoClose),
            16 => Some(BuiltinFunction::IoExists),
            17 => Some(BuiltinFunction::Count),
            18 => Some(BuiltinFunction::ReIsMatch),
            19 => Some(BuiltinFunction::ReFind),
            20 => Some(BuiltinFunction::ReReplace),
            21 => Some(BuiltinFunction::ReSplit),
            22 => Some(BuiltinFunction::ReCaptures),
            _ => None,
        }
    }
}
