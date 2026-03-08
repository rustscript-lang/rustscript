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
    MathPi = 33,
    MathTau = 34,
    MathE = 35,
    MathEpsilon = 36,
    MathInf = 37,
    MathNegInf = 38,
    MathNaN = 39,
    MathAbs = 40,
    MathSqrt = 41,
    MathCbrt = 42,
    MathExp = 43,
    MathExp2 = 44,
    MathLn = 45,
    MathLn1p = 46,
    MathLog2 = 47,
    MathLog10 = 48,
    MathSin = 49,
    MathCos = 50,
    MathTan = 51,
    MathAsin = 52,
    MathAcos = 53,
    MathAtan = 54,
    MathSinh = 55,
    MathCosh = 56,
    MathTanh = 57,
    MathFloor = 58,
    MathCeil = 59,
    MathRound = 60,
    MathTrunc = 61,
    MathFract = 62,
    MathSignum = 63,
    MathToDegrees = 64,
    MathToRadians = 65,
    MathIsNaN = 66,
    MathIsInfinite = 67,
    MathIsFinite = 68,
    MathAtan2 = 69,
    MathPowF = 70,
    MathPowI = 71,
    MathHypot = 72,
    MathLog = 73,
    MathMin = 74,
    MathMax = 75,
    MathCopySign = 76,
    MathClamp = 77,
    MathMulAdd = 78,
    FormatTemplate = 79,
    ToString = 80,
    TypeOf = 81,
    Assert = 82,
}

pub(crate) const BUILTIN_CALL_BASE: u16 = 0xFFB0;
/// Number of builtins in the main range (indices 0..78 above BUILTIN_CALL_BASE).
/// FormatTemplate, ToString, TypeOf, and Assert live at special indices below BUILTIN_CALL_BASE.
pub(crate) const BUILTIN_CALL_COUNT: u16 = 79;

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
            BuiltinFunction::MathPi => "math_pi",
            BuiltinFunction::MathTau => "math_tau",
            BuiltinFunction::MathE => "math_e",
            BuiltinFunction::MathEpsilon => "math_epsilon",
            BuiltinFunction::MathInf => "math_inf",
            BuiltinFunction::MathNegInf => "math_neg_inf",
            BuiltinFunction::MathNaN => "math_nan",
            BuiltinFunction::MathAbs => "math_abs",
            BuiltinFunction::MathSqrt => "math_sqrt",
            BuiltinFunction::MathCbrt => "math_cbrt",
            BuiltinFunction::MathExp => "math_exp",
            BuiltinFunction::MathExp2 => "math_exp2",
            BuiltinFunction::MathLn => "math_ln",
            BuiltinFunction::MathLn1p => "math_ln_1p",
            BuiltinFunction::MathLog2 => "math_log2",
            BuiltinFunction::MathLog10 => "math_log10",
            BuiltinFunction::MathSin => "math_sin",
            BuiltinFunction::MathCos => "math_cos",
            BuiltinFunction::MathTan => "math_tan",
            BuiltinFunction::MathAsin => "math_asin",
            BuiltinFunction::MathAcos => "math_acos",
            BuiltinFunction::MathAtan => "math_atan",
            BuiltinFunction::MathSinh => "math_sinh",
            BuiltinFunction::MathCosh => "math_cosh",
            BuiltinFunction::MathTanh => "math_tanh",
            BuiltinFunction::MathFloor => "math_floor",
            BuiltinFunction::MathCeil => "math_ceil",
            BuiltinFunction::MathRound => "math_round",
            BuiltinFunction::MathTrunc => "math_trunc",
            BuiltinFunction::MathFract => "math_fract",
            BuiltinFunction::MathSignum => "math_signum",
            BuiltinFunction::MathToDegrees => "math_to_degrees",
            BuiltinFunction::MathToRadians => "math_to_radians",
            BuiltinFunction::MathIsNaN => "math_is_nan",
            BuiltinFunction::MathIsInfinite => "math_is_infinite",
            BuiltinFunction::MathIsFinite => "math_is_finite",
            BuiltinFunction::MathAtan2 => "math_atan2",
            BuiltinFunction::MathPowF => "math_powf",
            BuiltinFunction::MathPowI => "math_powi",
            BuiltinFunction::MathHypot => "math_hypot",
            BuiltinFunction::MathLog => "math_log",
            BuiltinFunction::MathMin => "math_min",
            BuiltinFunction::MathMax => "math_max",
            BuiltinFunction::MathCopySign => "math_copysign",
            BuiltinFunction::MathClamp => "math_clamp",
            BuiltinFunction::MathMulAdd => "math_mul_add",
            BuiltinFunction::FormatTemplate => "__format_template",
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
            BuiltinFunction::MathPi => 0,
            BuiltinFunction::MathTau => 0,
            BuiltinFunction::MathE => 0,
            BuiltinFunction::MathEpsilon => 0,
            BuiltinFunction::MathInf => 0,
            BuiltinFunction::MathNegInf => 0,
            BuiltinFunction::MathNaN => 0,
            BuiltinFunction::MathAbs => 1,
            BuiltinFunction::MathSqrt => 1,
            BuiltinFunction::MathCbrt => 1,
            BuiltinFunction::MathExp => 1,
            BuiltinFunction::MathExp2 => 1,
            BuiltinFunction::MathLn => 1,
            BuiltinFunction::MathLn1p => 1,
            BuiltinFunction::MathLog2 => 1,
            BuiltinFunction::MathLog10 => 1,
            BuiltinFunction::MathSin => 1,
            BuiltinFunction::MathCos => 1,
            BuiltinFunction::MathTan => 1,
            BuiltinFunction::MathAsin => 1,
            BuiltinFunction::MathAcos => 1,
            BuiltinFunction::MathAtan => 1,
            BuiltinFunction::MathSinh => 1,
            BuiltinFunction::MathCosh => 1,
            BuiltinFunction::MathTanh => 1,
            BuiltinFunction::MathFloor => 1,
            BuiltinFunction::MathCeil => 1,
            BuiltinFunction::MathRound => 1,
            BuiltinFunction::MathTrunc => 1,
            BuiltinFunction::MathFract => 1,
            BuiltinFunction::MathSignum => 1,
            BuiltinFunction::MathToDegrees => 1,
            BuiltinFunction::MathToRadians => 1,
            BuiltinFunction::MathIsNaN => 1,
            BuiltinFunction::MathIsInfinite => 1,
            BuiltinFunction::MathIsFinite => 1,
            BuiltinFunction::MathAtan2 => 2,
            BuiltinFunction::MathPowF => 2,
            BuiltinFunction::MathPowI => 2,
            BuiltinFunction::MathHypot => 2,
            BuiltinFunction::MathLog => 2,
            BuiltinFunction::MathMin => 2,
            BuiltinFunction::MathMax => 2,
            BuiltinFunction::MathCopySign => 2,
            BuiltinFunction::MathClamp => 3,
            BuiltinFunction::MathMulAdd => 3,
            BuiltinFunction::FormatTemplate => 2,
            BuiltinFunction::ToString => 1,
            BuiltinFunction::TypeOf => 1,
            BuiltinFunction::Assert => 1,
        }
    }

    pub(crate) fn call_index(self) -> u16 {
        match self {
            BuiltinFunction::FormatTemplate => BUILTIN_CALL_BASE - 4,
            BuiltinFunction::ToString => BUILTIN_CALL_BASE - 3,
            BuiltinFunction::TypeOf => BUILTIN_CALL_BASE - 2,
            BuiltinFunction::Assert => BUILTIN_CALL_BASE - 1,
            _ => BUILTIN_CALL_BASE + self as u16,
        }
    }

    pub(crate) fn from_call_index(index: u16) -> Option<Self> {
        if index == BUILTIN_CALL_BASE - 4 {
            return Some(BuiltinFunction::FormatTemplate);
        }
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
            33 => Some(BuiltinFunction::MathPi),
            34 => Some(BuiltinFunction::MathTau),
            35 => Some(BuiltinFunction::MathE),
            36 => Some(BuiltinFunction::MathEpsilon),
            37 => Some(BuiltinFunction::MathInf),
            38 => Some(BuiltinFunction::MathNegInf),
            39 => Some(BuiltinFunction::MathNaN),
            40 => Some(BuiltinFunction::MathAbs),
            41 => Some(BuiltinFunction::MathSqrt),
            42 => Some(BuiltinFunction::MathCbrt),
            43 => Some(BuiltinFunction::MathExp),
            44 => Some(BuiltinFunction::MathExp2),
            45 => Some(BuiltinFunction::MathLn),
            46 => Some(BuiltinFunction::MathLn1p),
            47 => Some(BuiltinFunction::MathLog2),
            48 => Some(BuiltinFunction::MathLog10),
            49 => Some(BuiltinFunction::MathSin),
            50 => Some(BuiltinFunction::MathCos),
            51 => Some(BuiltinFunction::MathTan),
            52 => Some(BuiltinFunction::MathAsin),
            53 => Some(BuiltinFunction::MathAcos),
            54 => Some(BuiltinFunction::MathAtan),
            55 => Some(BuiltinFunction::MathSinh),
            56 => Some(BuiltinFunction::MathCosh),
            57 => Some(BuiltinFunction::MathTanh),
            58 => Some(BuiltinFunction::MathFloor),
            59 => Some(BuiltinFunction::MathCeil),
            60 => Some(BuiltinFunction::MathRound),
            61 => Some(BuiltinFunction::MathTrunc),
            62 => Some(BuiltinFunction::MathFract),
            63 => Some(BuiltinFunction::MathSignum),
            64 => Some(BuiltinFunction::MathToDegrees),
            65 => Some(BuiltinFunction::MathToRadians),
            66 => Some(BuiltinFunction::MathIsNaN),
            67 => Some(BuiltinFunction::MathIsInfinite),
            68 => Some(BuiltinFunction::MathIsFinite),
            69 => Some(BuiltinFunction::MathAtan2),
            70 => Some(BuiltinFunction::MathPowF),
            71 => Some(BuiltinFunction::MathPowI),
            72 => Some(BuiltinFunction::MathHypot),
            73 => Some(BuiltinFunction::MathLog),
            74 => Some(BuiltinFunction::MathMin),
            75 => Some(BuiltinFunction::MathMax),
            76 => Some(BuiltinFunction::MathCopySign),
            77 => Some(BuiltinFunction::MathClamp),
            78 => Some(BuiltinFunction::MathMulAdd),
            _ => None,
        }
    }
}
