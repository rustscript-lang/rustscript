// Shared builtin catalog (ids, names, arity, call-index mapping).
// VM execution logic lives under vm/builtins_impl/.
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug)]
pub(crate) struct BuiltinNamespaceMember {
    pub(crate) name: &'static str,
    pub(crate) builtin: BuiltinFunction,
}

impl BuiltinNamespaceMember {
    pub(crate) const fn new(name: &'static str, builtin: BuiltinFunction) -> Self {
        Self { name, builtin }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct BuiltinNamespace {
    pub(crate) name: &'static str,
    pub(crate) members: &'static [BuiltinNamespaceMember],
    pub(crate) supports_regex_flags: bool,
}

impl BuiltinNamespace {
    pub(crate) const fn new(
        name: &'static str,
        members: &'static [BuiltinNamespaceMember],
        supports_regex_flags: bool,
    ) -> Self {
        Self {
            name,
            members,
            supports_regex_flags,
        }
    }
}

#[derive(Default)]
pub(crate) struct BuiltinNamespaceRegistry {
    namespaces: Vec<BuiltinNamespace>,
}

impl BuiltinNamespaceRegistry {
    pub(crate) fn register(&mut self, namespace: BuiltinNamespace) {
        if self
            .namespaces
            .iter()
            .any(|entry| entry.name == namespace.name)
        {
            return;
        }
        self.namespaces.push(namespace);
    }

    fn namespaces(&self) -> &[BuiltinNamespace] {
        &self.namespaces
    }
}

fn builtin_namespace_registry() -> &'static BuiltinNamespaceRegistry {
    static REGISTRY: OnceLock<BuiltinNamespaceRegistry> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        let mut registry = BuiltinNamespaceRegistry::default();
        #[cfg(feature = "runtime")]
        crate::vm::builtins_impl::register_builtin_namespaces(&mut registry);
        registry
    })
}

fn builtin_namespaces() -> &'static [BuiltinNamespace] {
    builtin_namespace_registry().namespaces()
}

pub(crate) fn is_builtin_namespace(namespace: &str) -> bool {
    builtin_namespaces()
        .iter()
        .any(|entry| entry.name == namespace)
}

pub(crate) fn resolve_builtin_namespace_call(
    namespace: &str,
    member: &str,
) -> Option<BuiltinFunction> {
    let entry = builtin_namespaces()
        .iter()
        .find(|entry| entry.name == namespace)?;
    entry
        .members
        .iter()
        .find(|item| item.name == member)
        .map(|item| item.builtin)
}

pub(crate) fn namespace_supports_regex_flags(namespace: &str) -> bool {
    builtin_namespaces()
        .iter()
        .find(|entry| entry.name == namespace)
        .is_some_and(|entry| entry.supports_regex_flags)
}

pub(crate) fn builtin_namespace_hint() -> String {
    builtin_namespaces()
        .iter()
        .map(|entry| entry.name)
        .collect::<Vec<_>>()
        .join("/")
}

pub(crate) fn resolve_namespaced_builtin(name: &str) -> Option<BuiltinFunction> {
    let mut parts = name.trim().split("::");
    let namespace = parts.next()?;
    let member = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    resolve_builtin_namespace_call(namespace, member)
}
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
    JsonEncode = 23,
    JsonDecode = 24,
    JitSetConfig = 25,
    JitGetConfig = 26,
    JitSetEnabled = 27,
    JitGetEnabled = 28,
    JitSetHotLoopThreshold = 29,
    JitGetHotLoopThreshold = 30,
    JitSetMaxTraceLen = 31,
    JitGetMaxTraceLen = 32,
    ToString = 33,
    TypeOf = 34,
    Assert = 35,
}

pub(crate) const BUILTIN_CALL_BASE: u16 = 0xFFE0;
/// Number of builtins in the main range (indices 0..32 above BUILTIN_CALL_BASE).
/// ToString, TypeOf, and Assert live at special indices below BUILTIN_CALL_BASE.
pub(crate) const BUILTIN_CALL_COUNT: u16 = 33;

impl BuiltinFunction {
    pub(crate) fn from_namespaced_name(name: &str) -> Option<Self> {
        resolve_namespaced_builtin(name)
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
            BuiltinFunction::JsonEncode => "json_encode",
            BuiltinFunction::JsonDecode => "json_decode",
            BuiltinFunction::JitSetConfig => "jit_set_config",
            BuiltinFunction::JitGetConfig => "jit_get_config",
            BuiltinFunction::JitSetEnabled => "jit_set_enabled",
            BuiltinFunction::JitGetEnabled => "jit_get_enabled",
            BuiltinFunction::JitSetHotLoopThreshold => "jit_set_hot_loop_threshold",
            BuiltinFunction::JitGetHotLoopThreshold => "jit_get_hot_loop_threshold",
            BuiltinFunction::JitSetMaxTraceLen => "jit_set_max_trace_len",
            BuiltinFunction::JitGetMaxTraceLen => "jit_get_max_trace_len",
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
            BuiltinFunction::JsonEncode => 1,
            BuiltinFunction::JsonDecode => 1,
            BuiltinFunction::JitSetConfig => 3,
            BuiltinFunction::JitGetConfig => 0,
            BuiltinFunction::JitSetEnabled => 1,
            BuiltinFunction::JitGetEnabled => 0,
            BuiltinFunction::JitSetHotLoopThreshold => 1,
            BuiltinFunction::JitGetHotLoopThreshold => 0,
            BuiltinFunction::JitSetMaxTraceLen => 1,
            BuiltinFunction::JitGetMaxTraceLen => 0,
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
            23 => Some(BuiltinFunction::JsonEncode),
            24 => Some(BuiltinFunction::JsonDecode),
            25 => Some(BuiltinFunction::JitSetConfig),
            26 => Some(BuiltinFunction::JitGetConfig),
            27 => Some(BuiltinFunction::JitSetEnabled),
            28 => Some(BuiltinFunction::JitGetEnabled),
            29 => Some(BuiltinFunction::JitSetHotLoopThreshold),
            30 => Some(BuiltinFunction::JitGetHotLoopThreshold),
            31 => Some(BuiltinFunction::JitSetMaxTraceLen),
            32 => Some(BuiltinFunction::JitGetMaxTraceLen),
            _ => None,
        }
    }
}
