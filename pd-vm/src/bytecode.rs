use std::collections::{HashMap, hash_map};
use std::fmt;
use std::hash::{BuildHasherDefault, Hash, Hasher};
use std::sync::{Arc, OnceLock};

pub type SharedString = Arc<String>;
pub type SharedArray = Arc<Vec<Value>>;
pub type SharedMap = Arc<VmMap>;

type VmMapStorage = HashMap<MapKey, Value, BuildHasherDefault<StableHasher>>;

/// Runtime map storage for VM values.
///
/// Keys and values may be any runtime [`Value`]. Key equality is hybrid:
/// scalars and strings compare by value, while arrays and maps compare by
/// heap-object identity. Float keys use canonicalized IEEE bits: `0.0` /
/// `-0.0` are treated as the same key, while `NaN` keys only compare equal
/// when their bit patterns match. Duplicate inserts overwrite the prior value
/// for the same key.
///
/// Heap-backed keys remain stable after insertion. Values are reference-counted
/// and container writes detach before mutation, so later writes through an
/// alias create a new heap object instead of mutating a key already stored in
/// the map.
#[derive(Clone, Default)]
pub struct VmMap {
    entries: VmMapStorage,
}

#[derive(Clone, Debug)]
struct MapKey(Value);

pub struct VmMapIter<'a> {
    inner: hash_map::Iter<'a, MapKey, Value>,
}

pub struct VmMapIntoIter {
    inner: hash_map::IntoIter<MapKey, Value>,
}

#[derive(Default)]
pub(crate) struct StableHasher(u64);

impl Hasher for StableHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        const OFFSET_BASIS: u64 = 0xcbf29ce484222325;
        const PRIME: u64 = 0x100000001b3;

        if self.0 == 0 {
            self.0 = OFFSET_BASIS;
        }
        for byte in bytes {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(PRIME);
        }
    }
}

impl VmMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_entries(entries: Vec<(Value, Value)>) -> Self {
        let mut out = Self::new();
        for (key, value) in entries {
            out.insert(key, value);
        }
        out
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn iter(&self) -> VmMapIter<'_> {
        VmMapIter {
            inner: self.entries.iter(),
        }
    }

    pub fn get(&self, key: &Value) -> Option<&Value> {
        self.entries.get(&MapKey::new(key.clone()))
    }

    pub fn insert(&mut self, key: Value, value: Value) -> Option<Value> {
        self.entries.insert(MapKey::new(key), value)
    }

    pub fn remove(&mut self, key: &Value) -> Option<Value> {
        self.entries.remove(&MapKey::new(key.clone()))
    }
}

impl From<Vec<(Value, Value)>> for VmMap {
    fn from(value: Vec<(Value, Value)>) -> Self {
        Self::from_entries(value)
    }
}

impl IntoIterator for VmMap {
    type Item = (Value, Value);
    type IntoIter = VmMapIntoIter;

    fn into_iter(self) -> Self::IntoIter {
        VmMapIntoIter {
            inner: self.entries.into_iter(),
        }
    }
}

impl<'a> IntoIterator for &'a VmMap {
    type Item = (&'a Value, &'a Value);
    type IntoIter = VmMapIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl fmt::Debug for VmMap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

impl PartialEq for VmMap {
    fn eq(&self, other: &Self) -> bool {
        self.entries == other.entries
    }
}

impl Eq for VmMap {}

impl MapKey {
    fn new(value: Value) -> Self {
        Self(value)
    }

    fn value(&self) -> &Value {
        &self.0
    }

    fn into_value(self) -> Value {
        self.0
    }
}

impl PartialEq for MapKey {
    fn eq(&self, other: &Self) -> bool {
        map_key_eq(&self.0, &other.0)
    }
}

impl Eq for MapKey {}

impl Hash for MapKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        hash_map_key(&self.0, state);
    }
}

impl<'a> Iterator for VmMapIter<'a> {
    type Item = (&'a Value, &'a Value);

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|(key, value)| (key.value(), value))
    }
}

impl Iterator for VmMapIntoIter {
    type Item = (Value, Value);

    fn next(&mut self) -> Option<Self::Item> {
        self.inner
            .next()
            .map(|(key, value)| (key.into_value(), value))
    }
}

fn hash_map_key(value: &Value, state: &mut impl Hasher) {
    match value {
        Value::Null => {
            6u8.hash(state);
        }
        Value::Int(value) => {
            0u8.hash(state);
            value.hash(state);
        }
        Value::Float(value) => {
            1u8.hash(state);
            canonical_float_key_bits(*value).hash(state);
        }
        Value::Bool(value) => {
            2u8.hash(state);
            value.hash(state);
        }
        Value::String(value) => {
            3u8.hash(state);
            value.hash(state);
        }
        Value::Array(values) => {
            4u8.hash(state);
            Arc::as_ptr(values).hash(state);
        }
        Value::Map(entries) => {
            5u8.hash(state);
            Arc::as_ptr(entries).hash(state);
        }
    }
}

fn map_key_eq(lhs: &Value, rhs: &Value) -> bool {
    match (lhs, rhs) {
        (Value::Null, Value::Null) => true,
        (Value::Int(lhs), Value::Int(rhs)) => lhs == rhs,
        (Value::Float(lhs), Value::Float(rhs)) => {
            canonical_float_key_bits(*lhs) == canonical_float_key_bits(*rhs)
        }
        (Value::Bool(lhs), Value::Bool(rhs)) => lhs == rhs,
        (Value::String(lhs), Value::String(rhs)) => lhs == rhs,
        (Value::Array(lhs), Value::Array(rhs)) => Arc::ptr_eq(lhs, rhs),
        (Value::Map(lhs), Value::Map(rhs)) => Arc::ptr_eq(lhs, rhs),
        _ => false,
    }
}

/// Hash a value structurally for VM-internal cache keys.
///
/// The hasher itself is a small deterministic 64-bit FNV-1a-style accumulator.
/// Arrays hash recursively in order and maps hash recursively without caring
/// about entry order, so the result is stable across allocations.
pub(crate) fn hash_value(value: &Value, state: &mut impl Hasher) {
    match value {
        Value::Null => {
            6u8.hash(state);
        }
        Value::Int(value) => {
            0u8.hash(state);
            value.hash(state);
        }
        Value::Float(value) => {
            1u8.hash(state);
            canonical_float_key_bits(*value).hash(state);
        }
        Value::Bool(value) => {
            2u8.hash(state);
            value.hash(state);
        }
        Value::String(value) => {
            3u8.hash(state);
            value.hash(state);
        }
        Value::Array(values) => {
            4u8.hash(state);
            values.len().hash(state);
            for value in values.iter() {
                hash_value(value, state);
            }
        }
        Value::Map(entries) => {
            5u8.hash(state);
            entries.len().hash(state);
            let mut entry_hashes = entries
                .iter()
                .map(|(key, value)| {
                    let mut entry_hasher = StableHasher::default();
                    hash_value(key, &mut entry_hasher);
                    hash_value(value, &mut entry_hasher);
                    entry_hasher.finish()
                })
                .collect::<Vec<_>>();
            entry_hashes.sort_unstable();
            for entry_hash in entry_hashes {
                entry_hash.hash(state);
            }
        }
    }
}

fn canonical_float_key_bits(value: f64) -> u64 {
    if value == 0.0 {
        0.0f64.to_bits()
    } else {
        value.to_bits()
    }
}

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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ValueType {
    Unknown = 0,
    Null = 1,
    Int = 2,
    Float = 3,
    Bool = 4,
    String = 5,
    Array = 6,
    Map = 7,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TypeMap {
    pub local_types: Vec<ValueType>,
    pub operand_types: HashMap<usize, (ValueType, ValueType)>,
}

impl Value {
    pub fn string(value: impl Into<String>) -> Self {
        Self::String(Arc::new(value.into()))
    }

    pub fn array(values: Vec<Value>) -> Self {
        Self::Array(Arc::new(values))
    }

    pub fn map(entries: Vec<(Value, Value)>) -> Self {
        Self::Map(Arc::new(VmMap::from(entries)))
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

    pub fn into_owned_map(self) -> Result<VmMap, Self> {
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
            (Self::Map(lhs), Self::Map(rhs)) => lhs == rhs,
            _ => false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct HostImport {
    pub name: String,
    pub arity: u8,
    pub return_type: ValueType,
}

#[derive(Debug)]
pub(crate) struct DecodedInstructionData {
    pub(crate) ldc_values: Box<[Option<Value>]>,
    pub(crate) jump_targets: Box<[Option<usize>]>,
    pub(crate) local_indices: Box<[Option<u8>]>,
}

impl DecodedInstructionData {
    fn build(program: &Program) -> Self {
        let mut ldc_values = vec![None; program.code.len()];
        let mut jump_targets = vec![None; program.code.len()];
        let mut local_indices = vec![None; program.code.len()];
        let mut ip = 0usize;
        while ip < program.code.len() {
            let opcode = match OpCode::try_from(program.code[ip]) {
                Ok(opcode) => opcode,
                Err(_) => break,
            };
            match opcode {
                OpCode::Ldc => {
                    if let Some(raw_index) = read_u32_at(&program.code, ip + 1)
                        && let Some(value) = program.constants.get(raw_index as usize)
                    {
                        ldc_values[ip] = Some(value.clone());
                    }
                }
                OpCode::Br | OpCode::Brfalse => {
                    if let Some(target) = read_u32_at(&program.code, ip + 1) {
                        jump_targets[ip] = Some(target as usize);
                    }
                }
                OpCode::Ldloc | OpCode::Stloc => {
                    if let Some(index) = program.code.get(ip + 1).copied() {
                        local_indices[ip] = Some(index);
                    }
                }
                _ => {}
            }
            ip = ip.saturating_add(1 + opcode.operand_len());
        }
        Self {
            ldc_values: ldc_values.into_boxed_slice(),
            jump_targets: jump_targets.into_boxed_slice(),
            local_indices: local_indices.into_boxed_slice(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Program {
    pub constants: Vec<Value>,
    pub code: Vec<u8>,
    pub local_count: usize,
    pub imports: Vec<HostImport>,
    pub debug: Option<crate::debug_info::DebugInfo>,
    pub type_map: Option<TypeMap>,
    decoded_instruction_data_cache: Arc<OnceLock<Arc<DecodedInstructionData>>>,
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
            type_map: None,
            decoded_instruction_data_cache: Arc::new(OnceLock::new()),
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
            type_map: None,
            decoded_instruction_data_cache: Arc::new(OnceLock::new()),
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
            type_map: None,
            decoded_instruction_data_cache: Arc::new(OnceLock::new()),
        }
    }

    pub fn with_local_count(mut self, local_count: usize) -> Self {
        self.local_count = local_count;
        self
    }

    pub fn with_type_map(mut self, type_map: TypeMap) -> Self {
        self.type_map = Some(type_map);
        self
    }

    pub(crate) fn shared_decoded_instruction_data(&self) -> Arc<DecodedInstructionData> {
        Arc::clone(
            self.decoded_instruction_data_cache
                .get_or_init(|| Arc::new(DecodedInstructionData::build(self))),
        )
    }
}

fn read_u32_at(code: &[u8], offset: usize) -> Option<u32> {
    let bytes = code.get(offset..offset + 4)?;
    Some(u32::from_le_bytes(bytes.try_into().ok()?))
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

    #[test]
    fn composite_map_key_remains_stable_after_alias_detach() {
        let source_key = Value::array(vec![Value::Int(1), Value::Int(2)]);
        let alias = source_key.clone();
        let lookup_key = source_key.clone();
        let expected = Value::string("kept");

        let mut map = VmMap::new();
        map.insert(source_key, expected.clone());

        let mutated_alias = match alias {
            Value::Array(values) => {
                let mut owned = unwrap_or_clone_shared(values);
                owned[0] = Value::Int(9);
                Value::array(owned)
            }
            other => panic!("expected array alias, got {other:?}"),
        };

        assert_eq!(map.get(&lookup_key), Some(&expected));
        assert_eq!(
            map.get(&Value::array(vec![Value::Int(1), Value::Int(2)])),
            None
        );
        assert_eq!(map.get(&mutated_alias), None);
    }

    #[test]
    fn nested_map_keys_use_identity_lookup() {
        let nested_key = Value::map(vec![
            (Value::string("a"), Value::Int(1)),
            (Value::string("b"), Value::Int(2)),
        ]);
        let lookup_key = nested_key.clone();
        let structural_peer = Value::map(vec![
            (Value::string("b"), Value::Int(2)),
            (Value::string("a"), Value::Int(1)),
        ]);
        let expected = Value::Bool(true);

        let mut map = VmMap::new();
        map.insert(nested_key, expected.clone());

        assert_eq!(map.get(&lookup_key), Some(&expected));
        assert_eq!(map.get(&structural_peer), None);
    }
}
