//! Shared, host-agnostic semantic model of the host API.
//!
//! This module defines an ordinary, owned, serializable-friendly description of
//! the functions and resource types a host exposes to scripts. It is deliberately
//! independent of the compiler's inference types, the VM's runtime
//! [`crate::vm`] resource table, the wire format ([`crate::vmbc`]) and the
//! generated builtin catalog ([`crate::builtins`]), so all of those can be
//! consumed without introducing a reverse dependency.
//!
//! ## Design invariants
//!
//! * **Host-agnostic.** The catalog carries only semantic signatures: scalar,
//!   collection, callable and unknown schemas plus typed resource references.
//!   It does not talk about handles, bytecode or VM state.
//! * **Owned and serializable-friendly.** Every type owns its data (`String` /
//!   `Vec`) and derives or implements [`serde::Serialize`] /
//!   [`serde::Deserialize`]. No lifetimes, no `&'static` slices, no
//!   [`std::any::TypeId`].
//! * **Validated at every boundary.** `ResourceTypeKey` and `HostApiCatalog`
//!   implement *validating* deserialization, so malformed keys, duplicate
//!   signatures, undeclared resource references and invalid passing modes
//!   cannot enter through serde — the same rules the builder enforces.
//! * **Explicit resource ownership.** A parameter whose type **contains any
//!   resource**, directly or recursively (`Optional`, `Array`, `Map`,
//!   `Callable`), must use an explicit borrow/ownership passing mode; `Value`
//!   is forbidden. A parameter whose type contains **no** resource must use
//!   `Value`; a borrow/ownership mode is forbidden.
//! * **Overloading.** Host functions may legally share a name with distinct
//!   argument signatures (standard builtins such as `len` dispatch for string,
//!   array, bytes and map). Overloads must differ in their **argument type /
//!   passing-mode sequence**: two functions sharing a name and an identical
//!   argument type + passing sequence are ambiguous — parameter names and the
//!   return type do not disambiguate call sites — so they are rejected even
//!   when those fields differ.
//! * **Deterministic fingerprint.** [`HostApiCatalog::fingerprint`] produces a
//!   stable digest over *semantic* fields only, prefixed by a domain magic and
//!   a format version. Functions are sorted by their full canonical signature
//!   bytes, so overloaded registration order is irrelevant. Documentation is
//!   excluded.
//!
//! ## Fingerprint security note
//!
//! The 64-bit FNV-1a fingerprint is **not** a cryptographic digest. It is an
//! equality / change-detection fingerprint only: it is deterministic and
//! collision-resistant *enough* for detecting when two catalogs differ, but it
//! must **never** be used for authentication, integrity, or any context where
//! an attacker can influence catalog bytes. Treat `HostApiFingerprint` as a
//! convenience equality key, not a MAC.

use std::fmt;

use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, VariantAccess, Visitor};
use serde::ser::{SerializeStruct, SerializeStructVariant};
use serde::{Deserialize, Serialize};

/// Max byte length of a validated [`ResourceTypeKey`] name.
pub const MAX_HOST_RESOURCE_KEY_LEN: usize = 128;
const MAX_RESOURCE_KEY_LEN: usize = MAX_HOST_RESOURCE_KEY_LEN;

/// Max byte length of a validated host function name.
pub const MAX_HOST_FUNCTION_NAME_LEN: usize = 128;
const MAX_FUNCTION_NAME_LEN: usize = MAX_HOST_FUNCTION_NAME_LEN;

/// Maximum number of schema nodes along any single schema path. The root is
/// counted as depth one. Keeping this at 64 bounds both validation work and
/// the recursion used by serializers after validation.
pub const MAX_HOST_SCHEMA_DEPTH: usize = 64;

/// Maximum aggregate [`HostTypeSchema`] nodes in one schema or catalog.
/// Arrays, maps, options and callable results count as nodes; callable
/// parameter schemas count as nodes too.
pub const MAX_HOST_SCHEMA_NODES: usize = 16_384;

/// Maximum aggregate callable parameter/property slots in one schema or
/// catalog. This bounds wide callable signatures and leaves room for future
/// object-property schema variants without changing the budget contract.
pub const MAX_HOST_SCHEMA_PROPERTIES: usize = 4_096;

/// Maximum aggregate host-function/import parameter records in one catalog.
pub const MAX_HOST_CATALOG_PARAMETERS: usize = 4_096;

/// Maximum resource declarations in one catalog.
pub const MAX_HOST_CATALOG_RESOURCES: usize = 1_024;

/// Maximum function/overload declarations in one catalog.
pub const MAX_HOST_CATALOG_FUNCTIONS: usize = 1_024;

/// Maximum byte length of names on host parameter records.
pub const MAX_HOST_PARAMETER_NAME_LEN: usize = 128;

/// Maximum byte length of host resource/function documentation.
pub const MAX_HOST_DESCRIPTION_LEN: usize = 4_096;

/// 8-byte domain magic prepended to every fingerprint so digest bytes in one
/// domain (host API catalogs) cannot be confused with unrelated FNV digests
/// produced by other tooling.
const FINGERPRINT_DOMAIN_MAGIC: &[u8; 8] = b"rss-hapi";

/// The fingerprint wire/format version. Bump whenever the canonical byte
/// encoding or semantic interpretation changes so old and new digests are
/// never compared across versions.
const FINGERPRINT_FORMAT_VERSION: u8 = 1;

/// Error returned when a [`ResourceTypeKey`] cannot be constructed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResourceTypeKeyError {
    Empty,
    TooLong(usize),
    InvalidChar { index: usize, ch: char },
    InvalidDotPlacement { index: usize },
}

impl fmt::Display for ResourceTypeKeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "resource type key must not be empty"),
            Self::TooLong(len) => write!(
                f,
                "resource type key is {len} bytes; the maximum is {MAX_RESOURCE_KEY_LEN}"
            ),
            Self::InvalidChar { index, ch } => write!(
                f,
                "resource type key contains invalid character {ch:?} at byte offset {index}"
            ),
            Self::InvalidDotPlacement { index } => write!(
                f,
                "resource type key contains an empty namespace segment at byte offset {index}"
            ),
        }
    }
}

impl std::error::Error for ResourceTypeKeyError {}

/// A validated, stable identifier for a host resource type.
///
/// The key is an ordinary lowercase dot-namespaced name such as `io.file` or
/// `sqlite.connection`. Each segment is a non-empty run of lowercase ASCII
/// letters (`a`-`z`), digits (`0`-`9`), `_` or `-`; no segment-leading-letter
/// requirement exists, so a lone-segment key such as `file` or `0host` is
/// legal. A single-segment key (e.g. `file`) is allowed and simply carries no
/// namespace. `.` is reserved purely as the separator between non-empty
/// segments, so a key may not start or end with a dot and may not contain an
/// empty segment.
///
/// Validation rejects empty, over-long, non-ASCII and malformed-namespace
/// names so the value can serve as a stable map key, a fingerprint input and
/// a serialized identifier without further laundering.
///
/// This deliberately replaces any reliance on [`std::any::TypeId`]: resource
/// identity is a value, not a type reflection.
///
/// Deserialization is validating: a serialized key that fails [`Self::new`]
/// validation is rejected at the serde boundary.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize)]
pub struct ResourceTypeKey(String);

impl ResourceTypeKey {
    /// Validates and builds a resource type key.
    pub fn new(name: impl Into<String>) -> Result<Self, ResourceTypeKeyError> {
        let name = name.into();
        validate_resource_key(&name)?;
        Ok(Self(name))
    }

    /// The key text, e.g. `io.file`.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ResourceTypeKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

struct BoundedStringVisitor {
    field: &'static str,
    limit: usize,
}

impl<'de> Visitor<'de> for BoundedStringVisitor {
    type Value = String;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "a UTF-8 string of at most {} bytes", self.limit)
    }

    fn visit_borrowed_str<E>(self, value: &'de str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.visit_str(value)
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        if value.len() > self.limit {
            return Err(E::custom(HostSchemaValidationError::StringTooLong {
                field: self.field,
                len: value.len(),
                limit: self.limit,
            }));
        }
        Ok(value.to_owned())
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        if value.len() > self.limit {
            return Err(E::custom(HostSchemaValidationError::StringTooLong {
                field: self.field,
                len: value.len(),
                limit: self.limit,
            }));
        }
        Ok(value)
    }
}

fn deserialize_bounded_string<'de, D>(
    deserializer: D,
    field: &'static str,
    limit: usize,
) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserializer.deserialize_str(BoundedStringVisitor { field, limit })
}

struct BoundedStringSeed {
    field: &'static str,
    limit: usize,
}

impl<'de> DeserializeSeed<'de> for BoundedStringSeed {
    type Value = String;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserialize_bounded_string(deserializer, self.field, self.limit)
    }
}

impl<'de> Deserialize<'de> for ResourceTypeKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let text =
            deserialize_bounded_string(deserializer, "resource type key", MAX_RESOURCE_KEY_LEN)?;
        Self::new(text).map_err(serde::de::Error::custom)
    }
}

fn validate_resource_key(name: &str) -> Result<(), ResourceTypeKeyError> {
    if name.is_empty() {
        return Err(ResourceTypeKeyError::Empty);
    }
    if name.len() > MAX_RESOURCE_KEY_LEN {
        return Err(ResourceTypeKeyError::TooLong(name.len()));
    }
    // Allowed: ASCII lowercase a-z, 0-9, '_' and '-', with '.' used purely as a
    // namespace separator between non-empty segments.
    for (index, b) in name.bytes().enumerate() {
        let valid = b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-' | b'.');
        if !valid {
            return Err(ResourceTypeKeyError::InvalidChar {
                index,
                ch: name[index..].chars().next().unwrap_or('\u{fffd}'),
            });
        }
    }
    // Report the exact byte offset of each empty segment: a `.` that directly
    // follows another `.` (or the leading dot) opens an empty segment at that
    // dot, and a trailing `.` leaves an empty segment at the end of the name.
    let mut segment_start = 0usize;
    for (index, b) in name.bytes().enumerate() {
        if b == b'.' {
            if index == segment_start {
                return Err(ResourceTypeKeyError::InvalidDotPlacement { index });
            }
            segment_start = index + 1;
        }
    }
    if segment_start == name.len() {
        return Err(ResourceTypeKeyError::InvalidDotPlacement {
            index: segment_start,
        });
    }
    Ok(())
}

/// How a host function receives a parameter.
///
/// When a parameter's type [`contains`][HostTypeSchema::contains_resource] a
/// resource, **`Value` is forbidden** and the caller must chose one of
/// `Borrow`, `BorrowMut` or `TakeOwned`. When it contains no resource, `Value`
/// is required and a borrow/ownership mode is forbidden.
///
/// Ownership modes compose with a parameter's *aggregate* resource content:
///
/// * `Borrow` / `BorrowMut` apply **call-scoped, recursively** to every
///   resource contained anywhere in the type — direct, `Optional`, `Array`,
///   `Map`, or nested inside a `Callable` — so the callee may read (or
///   exclusively mutate) the whole aggregate for the duration of the call
///   without the caller losing the outer value.
/// * `TakeOwned` **transfers ownership of all contained resources** (and of
///   the value itself) to the callee; the caller no longer holds them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum HostParamPassing {
    /// The parameter is a plain value with no contained resource; the callee
    /// may copy or drop it freely.
    Value,
    /// An immutable borrow of the argument value; borrows every contained
    /// resource call-scoped and recursively.
    Borrow,
    /// An exclusive mutable borrow of the argument value; mutably borrows
    /// every contained resource call-scoped and recursively.
    BorrowMut,
    /// Ownership of the argument value and all contained owned resources is
    /// transferred to the callee.
    TakeOwned,
}

impl HostParamPassing {
    /// Whether the mode borrows, mutates or transfers rather than copying.
    pub fn is_reference_mode(self) -> bool {
        !matches!(self, Self::Value)
    }
}

/// Semantic schema of a single host value type.
///
/// Covers the same scalar / collection / callable / unknown surface used by
/// the compiler's inference pass, and adds an explicit [`Self::Resource`]
/// variant that references a declared [`ResourceTypeKey`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum HostTypeSchema {
    Unknown,
    Null,
    Int,
    Float,
    Number,
    Bool,
    String,
    Bytes,
    Array(Box<HostTypeSchema>),
    Map(Box<HostTypeSchema>),
    Optional(Box<HostTypeSchema>),
    Callable {
        params: Vec<HostTypeSchema>,
        result: Box<HostTypeSchema>,
    },
    /// A host resource identified by a declared [`ResourceTypeKey`].
    Resource(ResourceTypeKey),
}

impl HostTypeSchema {
    /// Returns the resource key when this schema (directly, or wrapped in a
    /// single optional layer) denotes a host resource. This is a shallow
    /// helper; use [`Self::contains_resource`] for the full recursive test.
    pub fn resource_key(&self) -> Option<&ResourceTypeKey> {
        let mut current = self;
        for _ in 0..MAX_HOST_SCHEMA_DEPTH {
            match current {
                Self::Resource(key) => return Some(key),
                Self::Optional(inner) => current = inner,
                _ => return None,
            }
        }
        None
    }

    /// Validates the complete schema using the shared bounded validator.
    pub fn validate(&self) -> Result<(), HostSchemaValidationError> {
        let mut budget = ComplexityBudget::default();
        let mut on_resource = |_key: &ResourceTypeKey| {};
        validate_type_schema_with_budget(self, &mut budget, &mut on_resource).map(|_| ())
    }

    /// Whether this schema references at least one resource, anywhere in the
    /// tree (direct, `Optional`, `Array`, `Map` value, or inside a `Callable`
    /// parameter/result).
    pub fn contains_resource(&self) -> bool {
        let mut budget = ComplexityBudget::default();
        let mut on_resource = |_key: &ResourceTypeKey| {};
        validate_type_schema_with_budget(self, &mut budget, &mut on_resource).unwrap_or(false)
    }

    /// Collects every resource key referenced anywhere in this schema tree.
    pub fn collect_resource_keys<'a>(&'a self, out: &mut Vec<&'a ResourceTypeKey>) {
        let mut budget = ComplexityBudget::default();
        let mut on_resource = |key: &'a ResourceTypeKey| out.push(key);
        let _ = validate_type_schema_with_budget(self, &mut budget, &mut on_resource);
    }
}

impl Serialize for HostTypeSchema {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.validate().map_err(serde::ser::Error::custom)?;
        match self {
            Self::Unknown => serializer.serialize_unit_variant("HostTypeSchema", 0, "Unknown"),
            Self::Null => serializer.serialize_unit_variant("HostTypeSchema", 1, "Null"),
            Self::Int => serializer.serialize_unit_variant("HostTypeSchema", 2, "Int"),
            Self::Float => serializer.serialize_unit_variant("HostTypeSchema", 3, "Float"),
            Self::Number => serializer.serialize_unit_variant("HostTypeSchema", 4, "Number"),
            Self::Bool => serializer.serialize_unit_variant("HostTypeSchema", 5, "Bool"),
            Self::String => serializer.serialize_unit_variant("HostTypeSchema", 6, "String"),
            Self::Bytes => serializer.serialize_unit_variant("HostTypeSchema", 7, "Bytes"),
            Self::Array(inner) => {
                serializer.serialize_newtype_variant("HostTypeSchema", 8, "Array", inner)
            }
            Self::Map(inner) => {
                serializer.serialize_newtype_variant("HostTypeSchema", 9, "Map", inner)
            }
            Self::Optional(inner) => {
                serializer.serialize_newtype_variant("HostTypeSchema", 10, "Optional", inner)
            }
            Self::Callable { params, result } => {
                let mut state =
                    serializer.serialize_struct_variant("HostTypeSchema", 11, "Callable", 2)?;
                state.serialize_field("params", params)?;
                state.serialize_field("result", result)?;
                state.end()
            }
            Self::Resource(key) => {
                serializer.serialize_newtype_variant("HostTypeSchema", 12, "Resource", key)
            }
        }
    }
}

#[derive(Deserialize)]
enum HostTypeSchemaVariant {
    Unknown,
    Null,
    Int,
    Float,
    Number,
    Bool,
    String,
    Bytes,
    Array,
    Map,
    Optional,
    Callable,
    Resource,
}

struct HostTypeSchemaSeed<'a> {
    budget: &'a mut ComplexityBudget,
    depth: usize,
    property: bool,
}

impl<'de> DeserializeSeed<'de> for HostTypeSchemaSeed<'_> {
    type Value = HostTypeSchema;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        if self.depth > MAX_HOST_SCHEMA_DEPTH {
            return Err(de::Error::custom(
                HostSchemaValidationError::NestingDepthExceeded {
                    limit: MAX_HOST_SCHEMA_DEPTH,
                },
            ));
        }
        if self.property {
            self.budget
                .charge_properties(1)
                .map_err(de::Error::custom)?;
        }
        self.budget.charge_nodes(1).map_err(de::Error::custom)?;
        deserializer.deserialize_enum(
            "HostTypeSchema",
            &[
                "Unknown", "Null", "Int", "Float", "Number", "Bool", "String", "Bytes", "Array",
                "Map", "Optional", "Callable", "Resource",
            ],
            HostTypeSchemaVisitor {
                budget: self.budget,
                depth: self.depth,
            },
        )
    }
}

struct HostTypeSchemaVisitor<'a> {
    budget: &'a mut ComplexityBudget,
    depth: usize,
}

impl<'de> Visitor<'de> for HostTypeSchemaVisitor<'_> {
    type Value = HostTypeSchema;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a bounded HostTypeSchema enum")
    }

    fn visit_enum<A>(self, data: A) -> Result<Self::Value, A::Error>
    where
        A: de::EnumAccess<'de>,
    {
        let (variant, access) = data.variant::<HostTypeSchemaVariant>()?;
        match variant {
            HostTypeSchemaVariant::Unknown => {
                access.unit_variant().map(|()| HostTypeSchema::Unknown)
            }
            HostTypeSchemaVariant::Null => access.unit_variant().map(|()| HostTypeSchema::Null),
            HostTypeSchemaVariant::Int => access.unit_variant().map(|()| HostTypeSchema::Int),
            HostTypeSchemaVariant::Float => access.unit_variant().map(|()| HostTypeSchema::Float),
            HostTypeSchemaVariant::Number => access.unit_variant().map(|()| HostTypeSchema::Number),
            HostTypeSchemaVariant::Bool => access.unit_variant().map(|()| HostTypeSchema::Bool),
            HostTypeSchemaVariant::String => access.unit_variant().map(|()| HostTypeSchema::String),
            HostTypeSchemaVariant::Bytes => access.unit_variant().map(|()| HostTypeSchema::Bytes),
            HostTypeSchemaVariant::Array => {
                let depth = next_schema_depth::<A::Error>(self.depth)?;
                access
                    .newtype_variant_seed(HostTypeSchemaSeed {
                        budget: self.budget,
                        depth,
                        property: false,
                    })
                    .map(|inner| HostTypeSchema::Array(Box::new(inner)))
            }
            HostTypeSchemaVariant::Map => {
                let depth = next_schema_depth::<A::Error>(self.depth)?;
                access
                    .newtype_variant_seed(HostTypeSchemaSeed {
                        budget: self.budget,
                        depth,
                        property: false,
                    })
                    .map(|inner| HostTypeSchema::Map(Box::new(inner)))
            }
            HostTypeSchemaVariant::Optional => {
                let depth = next_schema_depth::<A::Error>(self.depth)?;
                access
                    .newtype_variant_seed(HostTypeSchemaSeed {
                        budget: self.budget,
                        depth,
                        property: false,
                    })
                    .map(|inner| HostTypeSchema::Optional(Box::new(inner)))
            }
            HostTypeSchemaVariant::Callable => access
                .newtype_variant_seed(CallableSchemaSeed {
                    budget: self.budget,
                    depth: self.depth,
                })
                .map(|(params, result)| HostTypeSchema::Callable { params, result }),
            HostTypeSchemaVariant::Resource => access
                .newtype_variant::<ResourceTypeKey>()
                .map(HostTypeSchema::Resource),
        }
    }
}

fn next_schema_depth<E>(depth: usize) -> Result<usize, E>
where
    E: de::Error,
{
    depth.checked_add(1).ok_or_else(|| {
        E::custom(HostSchemaValidationError::IntegerOverflow {
            field: "schema depth",
        })
    })
}

impl<'de> Deserialize<'de> for HostTypeSchema {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let mut budget = ComplexityBudget::default();
        HostTypeSchemaSeed {
            budget: &mut budget,
            depth: 1,
            property: false,
        }
        .deserialize(deserializer)
    }
}

struct CallableSchemaSeed<'a> {
    budget: &'a mut ComplexityBudget,
    depth: usize,
}

impl<'de> DeserializeSeed<'de> for CallableSchemaSeed<'_> {
    type Value = (Vec<HostTypeSchema>, Box<HostTypeSchema>);

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let child_depth = next_schema_depth::<D::Error>(self.depth)?;
        deserializer.deserialize_struct(
            "HostTypeSchema::Callable",
            &["params", "result"],
            CallableSchemaVisitor {
                budget: self.budget,
                child_depth,
            },
        )
    }
}

struct CallableSchemaVisitor<'a> {
    budget: &'a mut ComplexityBudget,
    child_depth: usize,
}

#[derive(Deserialize)]
#[serde(field_identifier, rename_all = "snake_case")]
enum CallableField {
    Params,
    Result,
}

impl<'de> Visitor<'de> for CallableSchemaVisitor<'_> {
    type Value = (Vec<HostTypeSchema>, Box<HostTypeSchema>);

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a callable schema object")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        bounded_map_size_hint(map.size_hint(), "callable schema", 2)?;
        let mut entries = 0;
        let mut params = None;
        let mut result = None;
        loop {
            let Some(field) = map.next_key::<CallableField>()? else {
                break;
            };
            bounded_map_entry(&mut entries, "callable schema", 2)?;
            match field {
                CallableField::Params => {
                    if params.is_some() {
                        return Err(de::Error::duplicate_field("params"));
                    }
                    params = Some(map.next_value_seed(HostSchemaListSeed {
                        budget: self.budget,
                        depth: self.child_depth,
                    })?);
                }
                CallableField::Result => {
                    if result.is_some() {
                        return Err(de::Error::duplicate_field("result"));
                    }
                    result = Some(Box::new(map.next_value_seed(HostTypeSchemaSeed {
                        budget: self.budget,
                        depth: self.child_depth,
                        property: false,
                    })?));
                }
            }
        }
        let params = params.ok_or_else(|| de::Error::missing_field("params"))?;
        let result = result.ok_or_else(|| de::Error::missing_field("result"))?;
        Ok((params, result))
    }
}

struct HostSchemaListSeed<'a> {
    budget: &'a mut ComplexityBudget,
    depth: usize,
}

impl<'de> DeserializeSeed<'de> for HostSchemaListSeed<'_> {
    type Value = Vec<HostTypeSchema>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_seq(HostSchemaListVisitor {
            budget: self.budget,
            depth: self.depth,
        })
    }
}

struct HostSchemaListVisitor<'a> {
    budget: &'a mut ComplexityBudget,
    depth: usize,
}

impl<'de> Visitor<'de> for HostSchemaListVisitor<'_> {
    type Value = Vec<HostTypeSchema>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a bounded schema list")
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let hint = seq.size_hint();
        let capacity = bounded_sequence_capacity(
            hint,
            MAX_HOST_SCHEMA_PROPERTIES - self.budget.properties,
            HostSchemaValidationError::PropertyBudgetExceeded {
                limit: MAX_HOST_SCHEMA_PROPERTIES,
            },
        )?;
        let mut values = Vec::new();
        if capacity != 0 {
            values.try_reserve_exact(capacity).map_err(|_| {
                de::Error::custom(HostSchemaValidationError::AllocationFailed {
                    field: "callable parameters",
                })
            })?;
        }
        while let Some(value) = seq.next_element_seed(HostTypeSchemaSeed {
            budget: self.budget,
            depth: self.depth,
            property: true,
        })? {
            values.try_reserve_exact(1).map_err(|_| {
                de::Error::custom(HostSchemaValidationError::AllocationFailed {
                    field: "callable parameters",
                })
            })?;
            values.push(value);
        }
        Ok(values)
    }
}

impl fmt::Display for HostTypeSchema {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.validate().map_err(|_| fmt::Error)?;
        match self {
            Self::Unknown => write!(f, "unknown"),
            Self::Null => write!(f, "null"),
            Self::Int => write!(f, "int"),
            Self::Float => write!(f, "float"),
            Self::Number => write!(f, "number"),
            Self::Bool => write!(f, "bool"),
            Self::String => write!(f, "string"),
            Self::Bytes => write!(f, "bytes"),
            Self::Array(inner) => write!(f, "array<{inner}>"),
            Self::Map(inner) => write!(f, "map<{inner}>"),
            Self::Optional(inner) => write!(f, "optional<{inner}>"),
            Self::Callable { params, result } => {
                write!(f, "fn(")?;
                for (index, param) in params.iter().enumerate() {
                    if index > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{param}")?;
                }
                write!(f, ") -> {result}")
            }
            Self::Resource(key) => write!(f, "resource<{key}>"),
        }
    }
}

/// Semantic description of one declared host resource type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResourceTypeSchema {
    /// The stable, validated resource type key.
    pub key: ResourceTypeKey,
    /// Human-readable documentation; excluded from the fingerprint.
    pub description: String,
}

impl ResourceTypeSchema {
    pub fn new(key: ResourceTypeKey, description: impl Into<String>) -> Self {
        Self {
            key,
            description: description.into(),
        }
    }
}

impl Serialize for ResourceTypeSchema {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        validate_description(&self.description).map_err(serde::ser::Error::custom)?;
        let mut state = serializer.serialize_struct("ResourceTypeSchema", 2)?;
        state.serialize_field("key", &self.key)?;
        state.serialize_field("description", &self.description)?;
        state.end()
    }
}

#[derive(Deserialize)]
#[serde(field_identifier, rename_all = "snake_case")]
enum ResourceSchemaField {
    Key,
    Description,
}

struct ResourceTypeSchemaVisitor;

impl<'de> Visitor<'de> for ResourceTypeSchemaVisitor {
    type Value = ResourceTypeSchema;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a bounded resource type schema object")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        bounded_map_size_hint(map.size_hint(), "resource schema", 2)?;
        let mut entries = 0;
        let mut key = None;
        let mut description = None;
        loop {
            let Some(field) = map.next_key::<ResourceSchemaField>()? else {
                break;
            };
            bounded_map_entry(&mut entries, "resource schema", 2)?;
            match field {
                ResourceSchemaField::Key => {
                    if key.is_some() {
                        return Err(de::Error::duplicate_field("key"));
                    }
                    key = Some(map.next_value::<ResourceTypeKey>()?);
                }
                ResourceSchemaField::Description => {
                    if description.is_some() {
                        return Err(de::Error::duplicate_field("description"));
                    }
                    description = Some(map.next_value_seed(BoundedStringSeed {
                        field: "description",
                        limit: MAX_HOST_DESCRIPTION_LEN,
                    })?);
                }
            }
        }
        Ok(ResourceTypeSchema {
            key: key.ok_or_else(|| de::Error::missing_field("key"))?,
            description: description.ok_or_else(|| de::Error::missing_field("description"))?,
        })
    }
}

impl<'de> Deserialize<'de> for ResourceTypeSchema {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_struct(
            "ResourceTypeSchema",
            &["key", "description"],
            ResourceTypeSchemaVisitor,
        )
    }
}

/// Semantic description of one host function parameter.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct HostParamSchema {
    /// Parameter name, unique within its function.
    pub name: String,
    pub ty: HostTypeSchema,
    pub passing: HostParamPassing,
}

impl HostParamSchema {
    /// Builds a `Value`-passing parameter. Use this only when `ty` contains no
    /// resource; a containing resource requires [`Self::with_passing`] with an
    /// explicit borrow/ownership mode.
    pub fn value(name: impl Into<String>, ty: HostTypeSchema) -> Self {
        Self {
            name: name.into(),
            ty,
            passing: HostParamPassing::Value,
        }
    }

    pub fn with_passing(
        name: impl Into<String>,
        ty: HostTypeSchema,
        passing: HostParamPassing,
    ) -> Self {
        Self {
            name: name.into(),
            ty,
            passing,
        }
    }
}

/// Semantic description of one host function's signature.
///
/// Only semantic fields (name, parameters, passing modes, return type) feed
/// the catalog fingerprint; `description` is documentation and is excluded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostFunctionSchema {
    pub name: String,
    pub params: Vec<HostParamSchema>,
    pub return_type: HostTypeSchema,
    /// Human-readable documentation, excluded from the fingerprint.
    pub description: String,
}

impl HostFunctionSchema {
    pub fn new(name: impl Into<String>, params: Vec<HostParamSchema>) -> Self {
        Self {
            name: name.into(),
            params,
            return_type: HostTypeSchema::Unknown,
            description: String::new(),
        }
    }

    pub fn with_return(
        name: impl Into<String>,
        params: Vec<HostParamSchema>,
        return_type: HostTypeSchema,
    ) -> Self {
        Self {
            name: name.into(),
            params,
            return_type,
            description: String::new(),
        }
    }

    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }

    /// Returns a stable, collision-free token for this function's semantic
    /// identity. The token includes the canonical name, every parameter name,
    /// parameter schema, passing mode and return schema; documentation is not
    /// part of the token. It is suitable for an opaque URI segment used by a
    /// language-service definition location.
    pub fn identity_discriminator(&self) -> String {
        self.try_identity_discriminator()
            .unwrap_or_else(|error| format!("invalid-host-schema:{error}"))
    }

    /// Fallible identity rendering for callers that need to surface malformed
    /// manually-constructed schemas instead of using the compatibility fallback.
    pub fn try_identity_discriminator(&self) -> Result<String, HostSchemaValidationError> {
        let bytes = self.try_semantic_bytes()?;
        let capacity =
            bytes
                .len()
                .checked_mul(2)
                .ok_or(HostSchemaValidationError::IntegerOverflow {
                    field: "identity discriminator",
                })?;
        let mut identity = String::new();
        identity.try_reserve_exact(capacity).map_err(|_| {
            HostSchemaValidationError::AllocationFailed {
                field: "identity discriminator",
            }
        })?;
        const HEX: &[u8; 16] = b"0123456789abcdef";
        for byte in bytes {
            identity.push(char::from(HEX[usize::from(byte >> 4)]));
            identity.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        Ok(identity)
    }

    /// Canonical semantic bytes for this function: name, then the parameter
    /// list (each parameter’s name, type and passing mode), then the return
    /// type. This is the full semantic encoding used by the catalog
    /// fingerprint, so any semantic change (including a parameter-label or
    /// return-type change) alters the digest. It is **not** used for overload
    /// identity — see [`Self::try_overload_identity_bytes`].
    fn semantic_bytes(&self) -> Vec<u8> {
        self.try_semantic_bytes()
            .unwrap_or_else(|error| invalid_schema_bytes(&error))
    }

    fn try_semantic_bytes(&self) -> Result<Vec<u8>, HostSchemaValidationError> {
        let mut budget = ComplexityBudget::default();
        validate_function_shape(self, &mut budget)?;
        let mut bytes = Vec::new();
        push_len_str(&mut bytes, &self.name)?;
        push_len(&mut bytes, self.params.len())?;
        for param in &self.params {
            push_len_str(&mut bytes, &param.name)?;
            try_push_type(&mut bytes, &param.ty)?;
            push_tag(&mut bytes, passing_tag(param.passing));
        }
        try_push_type(&mut bytes, &self.return_type)?;
        Ok(bytes)
    }

    fn try_overload_identity_bytes(&self) -> Result<Vec<u8>, HostSchemaValidationError> {
        let mut budget = ComplexityBudget::default();
        validate_function_shape(self, &mut budget)?;
        let mut bytes = Vec::new();
        push_len_str(&mut bytes, &self.name)?;
        push_len(&mut bytes, self.params.len())?;
        for param in &self.params {
            try_push_type(&mut bytes, &param.ty)?;
            push_tag(&mut bytes, passing_tag(param.passing));
        }
        Ok(bytes)
    }
}

/// A parameter schema retained in a compiled host import identity.
///
/// The field names intentionally mirror the public catalog schema while
/// keeping the import representation independent from the catalog's prose
/// documentation.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct HostImportParam {
    pub name: String,
    pub schema: HostTypeSchema,
    pub passing: HostParamPassing,
}

/// The complete schema identity selected for one host import.
///
/// Runtime binding uses every field here. In particular, parameter resource
/// keys, the return schema, the function name and the catalog fingerprint are
/// retained; arity is only a cheap preliminary check and never an identity.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct HostImportSchema {
    pub name: String,
    pub params: Vec<HostImportParam>,
    pub return_type: HostTypeSchema,
    pub fingerprint: HostApiFingerprint,
}

impl HostImportSchema {
    pub fn from_function(catalog: &HostApiCatalog, function: &HostFunctionSchema) -> Self {
        Self {
            name: function.name.clone(),
            params: function
                .params
                .iter()
                .map(|param| HostImportParam {
                    name: param.name.clone(),
                    schema: param.ty.clone(),
                    passing: param.passing,
                })
                .collect(),
            return_type: function.return_type.clone(),
            fingerprint: catalog.fingerprint(),
        }
    }

    pub fn arity(&self) -> usize {
        self.params.len()
    }
}

impl HostParamSchema {
    /// Validates this parameter's bounded name and type structure.
    pub fn validate(&self) -> Result<(), HostSchemaValidationError> {
        validate_parameter_name(&self.name)?;
        let mut budget = ComplexityBudget::default();
        let mut on_resource = |_key: &ResourceTypeKey| {};
        validate_type_schema_with_budget(&self.ty, &mut budget, &mut on_resource).map(|_| ())
    }
}

impl HostImportParam {
    /// Validates this import parameter's bounded name and type structure.
    pub fn validate(&self) -> Result<(), HostSchemaValidationError> {
        validate_parameter_name(&self.name)?;
        let mut budget = ComplexityBudget::default();
        let mut on_resource = |_key: &ResourceTypeKey| {};
        validate_type_schema_with_budget(&self.schema, &mut budget, &mut on_resource).map(|_| ())
    }
}

impl HostFunctionSchema {
    /// Validates the bounded structure of this function schema.
    pub fn validate(&self) -> Result<(), HostSchemaValidationError> {
        let mut budget = ComplexityBudget::default();
        validate_function_shape(self, &mut budget)
    }
}

impl HostImportSchema {
    /// Validates the bounded structure of this compiled import schema.
    pub fn validate(&self) -> Result<(), HostSchemaValidationError> {
        let mut budget = ComplexityBudget::default();
        validate_import_shape(self, &mut budget)
    }
}

impl Serialize for HostParamSchema {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.validate().map_err(serde::ser::Error::custom)?;
        let mut state = serializer.serialize_struct("HostParamSchema", 3)?;
        state.serialize_field("name", &self.name)?;
        state.serialize_field("ty", &self.ty)?;
        state.serialize_field("passing", &self.passing)?;
        state.end()
    }
}

#[derive(Deserialize)]
#[serde(field_identifier, rename_all = "snake_case")]
enum HostParamSchemaField {
    Name,
    Ty,
    Passing,
}

struct HostParamSchemaSeed<'a> {
    budget: &'a mut ComplexityBudget,
    charge_parameter: bool,
}

impl<'de> DeserializeSeed<'de> for HostParamSchemaSeed<'_> {
    type Value = HostParamSchema;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        if self.charge_parameter {
            self.budget
                .charge_parameters(1)
                .map_err(de::Error::custom)?;
        }
        deserializer.deserialize_struct(
            "HostParamSchema",
            &["name", "ty", "passing"],
            HostParamSchemaVisitor {
                budget: self.budget,
            },
        )
    }
}

struct HostParamSchemaVisitor<'a> {
    budget: &'a mut ComplexityBudget,
}

impl<'de> Visitor<'de> for HostParamSchemaVisitor<'_> {
    type Value = HostParamSchema;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a bounded host parameter schema object")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        bounded_map_size_hint(map.size_hint(), "host parameter schema", 3)?;
        let mut entries = 0;
        let mut name = None;
        let mut ty = None;
        let mut passing = None;
        loop {
            let Some(field) = map.next_key::<HostParamSchemaField>()? else {
                break;
            };
            bounded_map_entry(&mut entries, "host parameter schema", 3)?;
            match field {
                HostParamSchemaField::Name => {
                    if name.is_some() {
                        return Err(de::Error::duplicate_field("name"));
                    }
                    name = Some(map.next_value_seed(BoundedStringSeed {
                        field: "parameter name",
                        limit: MAX_HOST_PARAMETER_NAME_LEN,
                    })?);
                }
                HostParamSchemaField::Ty => {
                    if ty.is_some() {
                        return Err(de::Error::duplicate_field("ty"));
                    }
                    ty = Some(map.next_value_seed(HostTypeSchemaSeed {
                        budget: self.budget,
                        depth: 1,
                        property: false,
                    })?);
                }
                HostParamSchemaField::Passing => {
                    if passing.is_some() {
                        return Err(de::Error::duplicate_field("passing"));
                    }
                    passing = Some(map.next_value::<HostParamPassing>()?);
                }
            }
        }
        Ok(HostParamSchema {
            name: name.ok_or_else(|| de::Error::missing_field("name"))?,
            ty: ty.ok_or_else(|| de::Error::missing_field("ty"))?,
            passing: passing.ok_or_else(|| de::Error::missing_field("passing"))?,
        })
    }
}

impl<'de> Deserialize<'de> for HostParamSchema {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let mut budget = ComplexityBudget::default();
        HostParamSchemaSeed {
            budget: &mut budget,
            charge_parameter: false,
        }
        .deserialize(deserializer)
    }
}

impl Serialize for HostFunctionSchema {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.validate().map_err(serde::ser::Error::custom)?;
        let mut state = serializer.serialize_struct("HostFunctionSchema", 4)?;
        state.serialize_field("name", &self.name)?;
        state.serialize_field("params", &self.params)?;
        state.serialize_field("return_type", &self.return_type)?;
        state.serialize_field("description", &self.description)?;
        state.end()
    }
}

#[derive(Deserialize)]
#[serde(field_identifier, rename_all = "snake_case")]
enum HostFunctionSchemaField {
    Name,
    Params,
    ReturnType,
    Description,
}

struct HostFunctionSchemaSeed<'a> {
    budget: &'a mut ComplexityBudget,
}

impl<'de> DeserializeSeed<'de> for HostFunctionSchemaSeed<'_> {
    type Value = HostFunctionSchema;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_struct(
            "HostFunctionSchema",
            &["name", "params", "return_type", "description"],
            HostFunctionSchemaVisitor {
                budget: self.budget,
            },
        )
    }
}

struct HostFunctionSchemaVisitor<'a> {
    budget: &'a mut ComplexityBudget,
}

impl<'de> Visitor<'de> for HostFunctionSchemaVisitor<'_> {
    type Value = HostFunctionSchema;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a bounded host function schema object")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        bounded_map_size_hint(map.size_hint(), "host function schema", 4)?;
        let mut entries = 0;
        let mut name = None;
        let mut params = None;
        let mut return_type = None;
        let mut description = None;
        loop {
            let Some(field) = map.next_key::<HostFunctionSchemaField>()? else {
                break;
            };
            bounded_map_entry(&mut entries, "host function schema", 4)?;
            match field {
                HostFunctionSchemaField::Name => {
                    if name.is_some() {
                        return Err(de::Error::duplicate_field("name"));
                    }
                    name = Some(map.next_value_seed(BoundedStringSeed {
                        field: "function name",
                        limit: MAX_FUNCTION_NAME_LEN,
                    })?);
                }
                HostFunctionSchemaField::Params => {
                    if params.is_some() {
                        return Err(de::Error::duplicate_field("params"));
                    }
                    params = Some(map.next_value_seed(HostParamListSeed {
                        budget: self.budget,
                    })?);
                }
                HostFunctionSchemaField::ReturnType => {
                    if return_type.is_some() {
                        return Err(de::Error::duplicate_field("return_type"));
                    }
                    return_type = Some(map.next_value_seed(HostTypeSchemaSeed {
                        budget: self.budget,
                        depth: 1,
                        property: false,
                    })?);
                }
                HostFunctionSchemaField::Description => {
                    if description.is_some() {
                        return Err(de::Error::duplicate_field("description"));
                    }
                    description = Some(map.next_value_seed(BoundedStringSeed {
                        field: "description",
                        limit: MAX_HOST_DESCRIPTION_LEN,
                    })?);
                }
            }
        }
        Ok(HostFunctionSchema {
            name: name.ok_or_else(|| de::Error::missing_field("name"))?,
            params: params.ok_or_else(|| de::Error::missing_field("params"))?,
            return_type: return_type.ok_or_else(|| de::Error::missing_field("return_type"))?,
            description: description.ok_or_else(|| de::Error::missing_field("description"))?,
        })
    }
}

impl<'de> Deserialize<'de> for HostFunctionSchema {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let mut budget = ComplexityBudget::default();
        let schema = HostFunctionSchemaSeed {
            budget: &mut budget,
        }
        .deserialize(deserializer)?;
        schema.validate().map_err(de::Error::custom)?;
        Ok(schema)
    }
}

struct HostParamListSeed<'a> {
    budget: &'a mut ComplexityBudget,
}

impl<'de> DeserializeSeed<'de> for HostParamListSeed<'_> {
    type Value = Vec<HostParamSchema>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_seq(HostParamListVisitor {
            budget: self.budget,
        })
    }
}

struct HostParamListVisitor<'a> {
    budget: &'a mut ComplexityBudget,
}

impl<'de> Visitor<'de> for HostParamListVisitor<'_> {
    type Value = Vec<HostParamSchema>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a bounded host parameter list")
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let hint = seq.size_hint();
        let capacity = bounded_sequence_capacity(
            hint,
            MAX_HOST_CATALOG_PARAMETERS - self.budget.parameters,
            HostSchemaValidationError::ParameterBudgetExceeded {
                limit: MAX_HOST_CATALOG_PARAMETERS,
            },
        )?;
        let mut values = Vec::new();
        if capacity != 0 {
            values.try_reserve_exact(capacity).map_err(|_| {
                de::Error::custom(HostSchemaValidationError::AllocationFailed {
                    field: "host parameters",
                })
            })?;
        }
        while let Some(value) = seq.next_element_seed(HostParamSchemaSeed {
            budget: self.budget,
            charge_parameter: true,
        })? {
            values.try_reserve_exact(1).map_err(|_| {
                de::Error::custom(HostSchemaValidationError::AllocationFailed {
                    field: "host parameters",
                })
            })?;
            values.push(value);
        }
        Ok(values)
    }
}

impl Serialize for HostImportParam {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.validate().map_err(serde::ser::Error::custom)?;
        let mut state = serializer.serialize_struct("HostImportParam", 3)?;
        state.serialize_field("name", &self.name)?;
        state.serialize_field("schema", &self.schema)?;
        state.serialize_field("passing", &self.passing)?;
        state.end()
    }
}

#[derive(Deserialize)]
#[serde(field_identifier, rename_all = "snake_case")]
enum HostImportParamField {
    Name,
    Schema,
    Passing,
}

struct HostImportParamSeed<'a> {
    budget: &'a mut ComplexityBudget,
    charge_parameter: bool,
}

impl<'de> DeserializeSeed<'de> for HostImportParamSeed<'_> {
    type Value = HostImportParam;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        if self.charge_parameter {
            self.budget
                .charge_parameters(1)
                .map_err(de::Error::custom)?;
        }
        deserializer.deserialize_struct(
            "HostImportParam",
            &["name", "schema", "passing"],
            HostImportParamVisitor {
                budget: self.budget,
            },
        )
    }
}

struct HostImportParamVisitor<'a> {
    budget: &'a mut ComplexityBudget,
}

impl<'de> Visitor<'de> for HostImportParamVisitor<'_> {
    type Value = HostImportParam;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a bounded host import parameter object")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        bounded_map_size_hint(map.size_hint(), "host import parameter", 3)?;
        let mut entries = 0;
        let mut name = None;
        let mut schema = None;
        let mut passing = None;
        loop {
            let Some(field) = map.next_key::<HostImportParamField>()? else {
                break;
            };
            bounded_map_entry(&mut entries, "host import parameter", 3)?;
            match field {
                HostImportParamField::Name => {
                    if name.is_some() {
                        return Err(de::Error::duplicate_field("name"));
                    }
                    name = Some(map.next_value_seed(BoundedStringSeed {
                        field: "parameter name",
                        limit: MAX_HOST_PARAMETER_NAME_LEN,
                    })?);
                }
                HostImportParamField::Schema => {
                    if schema.is_some() {
                        return Err(de::Error::duplicate_field("schema"));
                    }
                    schema = Some(map.next_value_seed(HostTypeSchemaSeed {
                        budget: self.budget,
                        depth: 1,
                        property: false,
                    })?);
                }
                HostImportParamField::Passing => {
                    if passing.is_some() {
                        return Err(de::Error::duplicate_field("passing"));
                    }
                    passing = Some(map.next_value::<HostParamPassing>()?);
                }
            }
        }
        Ok(HostImportParam {
            name: name.ok_or_else(|| de::Error::missing_field("name"))?,
            schema: schema.ok_or_else(|| de::Error::missing_field("schema"))?,
            passing: passing.ok_or_else(|| de::Error::missing_field("passing"))?,
        })
    }
}

impl<'de> Deserialize<'de> for HostImportParam {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let mut budget = ComplexityBudget::default();
        HostImportParamSeed {
            budget: &mut budget,
            charge_parameter: false,
        }
        .deserialize(deserializer)
    }
}

impl Serialize for HostImportSchema {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.validate().map_err(serde::ser::Error::custom)?;
        let mut state = serializer.serialize_struct("HostImportSchema", 4)?;
        state.serialize_field("name", &self.name)?;
        state.serialize_field("params", &self.params)?;
        state.serialize_field("return_type", &self.return_type)?;
        state.serialize_field("fingerprint", &self.fingerprint)?;
        state.end()
    }
}

#[derive(Deserialize)]
#[serde(field_identifier, rename_all = "snake_case")]
enum HostImportSchemaField {
    Name,
    Params,
    ReturnType,
    Fingerprint,
}

struct HostImportSchemaSeed<'a> {
    budget: &'a mut ComplexityBudget,
}

impl<'de> DeserializeSeed<'de> for HostImportSchemaSeed<'_> {
    type Value = HostImportSchema;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_struct(
            "HostImportSchema",
            &["name", "params", "return_type", "fingerprint"],
            HostImportSchemaVisitor {
                budget: self.budget,
            },
        )
    }
}

struct HostImportSchemaVisitor<'a> {
    budget: &'a mut ComplexityBudget,
}

impl<'de> Visitor<'de> for HostImportSchemaVisitor<'_> {
    type Value = HostImportSchema;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a bounded host import schema object")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        bounded_map_size_hint(map.size_hint(), "host import schema", 4)?;
        let mut entries = 0;
        let mut name = None;
        let mut params = None;
        let mut return_type = None;
        let mut fingerprint = None;
        loop {
            let Some(field) = map.next_key::<HostImportSchemaField>()? else {
                break;
            };
            bounded_map_entry(&mut entries, "host import schema", 4)?;
            match field {
                HostImportSchemaField::Name => {
                    if name.is_some() {
                        return Err(de::Error::duplicate_field("name"));
                    }
                    name = Some(map.next_value_seed(BoundedStringSeed {
                        field: "function name",
                        limit: MAX_FUNCTION_NAME_LEN,
                    })?);
                }
                HostImportSchemaField::Params => {
                    if params.is_some() {
                        return Err(de::Error::duplicate_field("params"));
                    }
                    params = Some(map.next_value_seed(HostImportParamListSeed {
                        budget: self.budget,
                    })?);
                }
                HostImportSchemaField::ReturnType => {
                    if return_type.is_some() {
                        return Err(de::Error::duplicate_field("return_type"));
                    }
                    return_type = Some(map.next_value_seed(HostTypeSchemaSeed {
                        budget: self.budget,
                        depth: 1,
                        property: false,
                    })?);
                }
                HostImportSchemaField::Fingerprint => {
                    if fingerprint.is_some() {
                        return Err(de::Error::duplicate_field("fingerprint"));
                    }
                    fingerprint = Some(map.next_value::<HostApiFingerprint>()?);
                }
            }
        }
        Ok(HostImportSchema {
            name: name.ok_or_else(|| de::Error::missing_field("name"))?,
            params: params.ok_or_else(|| de::Error::missing_field("params"))?,
            return_type: return_type.ok_or_else(|| de::Error::missing_field("return_type"))?,
            fingerprint: fingerprint.ok_or_else(|| de::Error::missing_field("fingerprint"))?,
        })
    }
}

impl<'de> Deserialize<'de> for HostImportSchema {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let mut budget = ComplexityBudget::default();
        let schema = HostImportSchemaSeed {
            budget: &mut budget,
        }
        .deserialize(deserializer)?;
        schema.validate().map_err(de::Error::custom)?;
        Ok(schema)
    }
}

struct HostImportParamListSeed<'a> {
    budget: &'a mut ComplexityBudget,
}

impl<'de> DeserializeSeed<'de> for HostImportParamListSeed<'_> {
    type Value = Vec<HostImportParam>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_seq(HostImportParamListVisitor {
            budget: self.budget,
        })
    }
}

struct HostImportParamListVisitor<'a> {
    budget: &'a mut ComplexityBudget,
}

impl<'de> Visitor<'de> for HostImportParamListVisitor<'_> {
    type Value = Vec<HostImportParam>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a bounded host import parameter list")
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let hint = seq.size_hint();
        let capacity = bounded_sequence_capacity(
            hint,
            MAX_HOST_CATALOG_PARAMETERS - self.budget.parameters,
            HostSchemaValidationError::ParameterBudgetExceeded {
                limit: MAX_HOST_CATALOG_PARAMETERS,
            },
        )?;
        let mut values = Vec::new();
        if capacity != 0 {
            values.try_reserve_exact(capacity).map_err(|_| {
                de::Error::custom(HostSchemaValidationError::AllocationFailed {
                    field: "host import parameters",
                })
            })?;
        }
        while let Some(value) = seq.next_element_seed(HostImportParamSeed {
            budget: self.budget,
            charge_parameter: true,
        })? {
            values.try_reserve_exact(1).map_err(|_| {
                de::Error::custom(HostSchemaValidationError::AllocationFailed {
                    field: "host import parameters",
                })
            })?;
            values.push(value);
        }
        Ok(values)
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FunctionNameError {
    Empty,
    TooLong(usize),
    InvalidChar { index: usize, ch: char },
    EmptySegment { index: usize },
}

impl fmt::Display for FunctionNameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "host function name must not be empty"),
            Self::TooLong(len) => write!(
                f,
                "host function name is {len} bytes; the maximum is {MAX_FUNCTION_NAME_LEN}"
            ),
            Self::InvalidChar { index, ch } => write!(
                f,
                "host function name contains invalid control/whitespace/symbol character \
                 {ch:?} at byte offset {index}"
            ),
            Self::EmptySegment { index } => write!(
                f,
                "host function name contains an empty `::` path segment at byte offset {index}"
            ),
        }
    }
}

impl std::error::Error for FunctionNameError {}

/// Resource and recursive-shape limits shared by the in-memory validator,
/// serializers, fingerprints and serde visitors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostSchemaValidationError {
    NestingDepthExceeded {
        limit: usize,
    },
    NodeBudgetExceeded {
        limit: usize,
    },
    PropertyBudgetExceeded {
        limit: usize,
    },
    ParameterBudgetExceeded {
        limit: usize,
    },
    ResourceBudgetExceeded {
        limit: usize,
    },
    FunctionBudgetExceeded {
        limit: usize,
    },
    MapEntriesExceeded {
        field: &'static str,
        limit: usize,
    },
    StringTooLong {
        field: &'static str,
        len: usize,
        limit: usize,
    },
    InvalidFunctionName {
        name: String,
        reason: FunctionNameError,
    },
    AllocationFailed {
        field: &'static str,
    },
    IntegerOverflow {
        field: &'static str,
    },
}

impl fmt::Display for HostSchemaValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NestingDepthExceeded { limit } => {
                write!(f, "host schema nesting depth exceeds maximum of {limit}")
            }
            Self::NodeBudgetExceeded { limit } => {
                write!(f, "host schema node budget exceeds maximum of {limit}")
            }
            Self::PropertyBudgetExceeded { limit } => {
                write!(f, "host schema property budget exceeds maximum of {limit}")
            }
            Self::ParameterBudgetExceeded { limit } => {
                write!(
                    f,
                    "host catalog parameter budget exceeds maximum of {limit}"
                )
            }
            Self::ResourceBudgetExceeded { limit } => {
                write!(f, "host catalog resource budget exceeds maximum of {limit}")
            }
            Self::FunctionBudgetExceeded { limit } => {
                write!(f, "host catalog function budget exceeds maximum of {limit}")
            }
            Self::MapEntriesExceeded { field, limit } => {
                write!(f, "host {field} map contains more than {limit} entries")
            }
            Self::StringTooLong { field, len, limit } => {
                write!(f, "host {field} is {len} bytes; the maximum is {limit}")
            }
            Self::InvalidFunctionName { name, reason } => {
                write!(f, "invalid host function name `{name}`: {reason}")
            }
            Self::AllocationFailed { field } => {
                write!(f, "host schema allocation failed while reading {field}")
            }
            Self::IntegerOverflow { field } => {
                write!(f, "host schema length overflow while encoding {field}")
            }
        }
    }
}

impl std::error::Error for HostSchemaValidationError {}

#[derive(Clone, Copy, Debug, Default)]
struct ComplexityBudget {
    nodes: usize,
    properties: usize,
    parameters: usize,
    resources: usize,
    functions: usize,
}

impl ComplexityBudget {
    fn charge_nodes(&mut self, amount: usize) -> Result<(), HostSchemaValidationError> {
        self.nodes = checked_budget_add(
            self.nodes,
            amount,
            MAX_HOST_SCHEMA_NODES,
            HostSchemaValidationError::NodeBudgetExceeded {
                limit: MAX_HOST_SCHEMA_NODES,
            },
        )?;
        Ok(())
    }

    fn charge_properties(&mut self, amount: usize) -> Result<(), HostSchemaValidationError> {
        self.properties = checked_budget_add(
            self.properties,
            amount,
            MAX_HOST_SCHEMA_PROPERTIES,
            HostSchemaValidationError::PropertyBudgetExceeded {
                limit: MAX_HOST_SCHEMA_PROPERTIES,
            },
        )?;
        Ok(())
    }

    fn charge_parameters(&mut self, amount: usize) -> Result<(), HostSchemaValidationError> {
        self.parameters = checked_budget_add(
            self.parameters,
            amount,
            MAX_HOST_CATALOG_PARAMETERS,
            HostSchemaValidationError::ParameterBudgetExceeded {
                limit: MAX_HOST_CATALOG_PARAMETERS,
            },
        )?;
        Ok(())
    }

    fn charge_resources(&mut self, amount: usize) -> Result<(), HostSchemaValidationError> {
        self.resources = checked_budget_add(
            self.resources,
            amount,
            MAX_HOST_CATALOG_RESOURCES,
            HostSchemaValidationError::ResourceBudgetExceeded {
                limit: MAX_HOST_CATALOG_RESOURCES,
            },
        )?;
        Ok(())
    }

    fn charge_functions(&mut self, amount: usize) -> Result<(), HostSchemaValidationError> {
        self.functions = checked_budget_add(
            self.functions,
            amount,
            MAX_HOST_CATALOG_FUNCTIONS,
            HostSchemaValidationError::FunctionBudgetExceeded {
                limit: MAX_HOST_CATALOG_FUNCTIONS,
            },
        )?;
        Ok(())
    }
}

fn checked_budget_add(
    current: usize,
    amount: usize,
    limit: usize,
    error: HostSchemaValidationError,
) -> Result<usize, HostSchemaValidationError> {
    let next = current.checked_add(amount).ok_or_else(|| error.clone())?;
    if next > limit {
        return Err(error);
    }
    Ok(next)
}

fn bounded_map_size_hint<E>(hint: Option<usize>, field: &'static str, limit: usize) -> Result<(), E>
where
    E: de::Error,
{
    if hint.is_some_and(|hint| hint > limit) {
        return Err(E::custom(HostSchemaValidationError::MapEntriesExceeded {
            field,
            limit,
        }));
    }
    Ok(())
}

fn bounded_map_entry<E>(entries: &mut usize, field: &'static str, limit: usize) -> Result<(), E>
where
    E: de::Error,
{
    *entries = entries
        .checked_add(1)
        .ok_or_else(|| E::custom(HostSchemaValidationError::MapEntriesExceeded { field, limit }))?;
    if *entries > limit {
        return Err(E::custom(HostSchemaValidationError::MapEntriesExceeded {
            field,
            limit,
        }));
    }
    Ok(())
}

fn bounded_sequence_capacity<E>(
    hint: Option<usize>,
    remaining: usize,
    error: HostSchemaValidationError,
) -> Result<usize, E>
where
    E: de::Error,
{
    let capacity = hint.unwrap_or(0);
    if capacity > remaining {
        return Err(E::custom(error));
    }
    Ok(capacity)
}

fn bounded_string_error(
    field: &'static str,
    len: usize,
    limit: usize,
) -> Result<(), HostSchemaValidationError> {
    if len > limit {
        return Err(HostSchemaValidationError::StringTooLong { field, len, limit });
    }
    Ok(())
}

fn validate_parameter_name(name: &str) -> Result<(), HostSchemaValidationError> {
    bounded_string_error("parameter name", name.len(), MAX_HOST_PARAMETER_NAME_LEN)
}

fn validate_description(description: &str) -> Result<(), HostSchemaValidationError> {
    bounded_string_error("description", description.len(), MAX_HOST_DESCRIPTION_LEN)
}

fn validate_type_schema_with_budget<'a, F>(
    schema: &'a HostTypeSchema,
    budget: &mut ComplexityBudget,
    on_resource: &mut F,
) -> Result<bool, HostSchemaValidationError>
where
    F: FnMut(&'a ResourceTypeKey),
{
    let mut pending: Vec<(&HostTypeSchema, usize)> = Vec::new();
    pending
        .try_reserve(1)
        .map_err(|_| HostSchemaValidationError::AllocationFailed {
            field: "schema traversal",
        })?;
    pending.push((schema, 1));
    let mut contains_resource = false;

    while let Some((current, depth)) = pending.pop() {
        if depth > MAX_HOST_SCHEMA_DEPTH {
            return Err(HostSchemaValidationError::NestingDepthExceeded {
                limit: MAX_HOST_SCHEMA_DEPTH,
            });
        }
        budget.charge_nodes(1)?;

        match current {
            HostTypeSchema::Resource(key) => {
                contains_resource = true;
                on_resource(key);
            }
            HostTypeSchema::Array(inner)
            | HostTypeSchema::Map(inner)
            | HostTypeSchema::Optional(inner) => {
                let child_depth =
                    depth
                        .checked_add(1)
                        .ok_or(HostSchemaValidationError::IntegerOverflow {
                            field: "schema depth",
                        })?;
                pending.try_reserve(1).map_err(|_| {
                    HostSchemaValidationError::AllocationFailed {
                        field: "schema traversal",
                    }
                })?;
                pending.push((inner, child_depth));
            }
            HostTypeSchema::Callable { params, result } => {
                budget.charge_properties(params.len())?;
                let child_depth =
                    depth
                        .checked_add(1)
                        .ok_or(HostSchemaValidationError::IntegerOverflow {
                            field: "schema depth",
                        })?;
                let needed = params.len().checked_add(1).ok_or(
                    HostSchemaValidationError::IntegerOverflow {
                        field: "schema traversal capacity",
                    },
                )?;
                pending.try_reserve(needed).map_err(|_| {
                    HostSchemaValidationError::AllocationFailed {
                        field: "schema traversal",
                    }
                })?;
                pending.push((result, child_depth));
                for param in params.iter().rev() {
                    pending.push((param, child_depth));
                }
            }
            HostTypeSchema::Unknown
            | HostTypeSchema::Null
            | HostTypeSchema::Int
            | HostTypeSchema::Float
            | HostTypeSchema::Number
            | HostTypeSchema::Bool
            | HostTypeSchema::String
            | HostTypeSchema::Bytes => {}
        }
    }

    Ok(contains_resource)
}

fn validate_function_shape(
    function: &HostFunctionSchema,
    budget: &mut ComplexityBudget,
) -> Result<(), HostSchemaValidationError> {
    validate_function_name(&function.name).map_err(|reason| {
        HostSchemaValidationError::InvalidFunctionName {
            name: function.name.clone(),
            reason,
        }
    })?;
    bounded_string_error("function name", function.name.len(), MAX_FUNCTION_NAME_LEN)?;
    validate_description(&function.description)?;
    budget.charge_parameters(function.params.len())?;
    let mut on_resource = |_key: &ResourceTypeKey| {};
    for param in &function.params {
        validate_parameter_name(&param.name)?;
        validate_type_schema_with_budget(&param.ty, budget, &mut on_resource)?;
    }
    validate_type_schema_with_budget(&function.return_type, budget, &mut on_resource)?;
    Ok(())
}

fn validate_import_shape(
    schema: &HostImportSchema,
    budget: &mut ComplexityBudget,
) -> Result<(), HostSchemaValidationError> {
    validate_function_name(&schema.name).map_err(|reason| {
        HostSchemaValidationError::InvalidFunctionName {
            name: schema.name.clone(),
            reason,
        }
    })?;
    bounded_string_error("function name", schema.name.len(), MAX_FUNCTION_NAME_LEN)?;
    budget.charge_parameters(schema.params.len())?;
    let mut on_resource = |_key: &ResourceTypeKey| {};
    for param in &schema.params {
        validate_parameter_name(&param.name)?;
        validate_type_schema_with_budget(&param.schema, budget, &mut on_resource)?;
    }
    validate_type_schema_with_budget(&schema.return_type, budget, &mut on_resource)?;
    Ok(())
}

/// Validate a host function name against the grammar used by the standard
/// catalog, e.g. `len`, `__bind_callable`, `bytes::from_utf8`, `io::open`,
/// `jit::set_hot_loop_threshold`.
///
/// Grammar: one or more path segments joined by the exact `::` separator, each
/// segment being a non-empty ASCII identifier (`[A-Za-z_][A-Za-z0-9_]*`).
/// Named functions must not start or end with `::`, must not contain empty
/// segments (`a::b`, `::`, `a::::b` are rejected), must not contain a lone
/// `:` and must not contain any control/whitespace/symbol outside the segment
/// alphabet.
fn validate_function_name(name: &str) -> Result<(), FunctionNameError> {
    if name.is_empty() {
        return Err(FunctionNameError::Empty);
    }
    if name.len() > MAX_FUNCTION_NAME_LEN {
        return Err(FunctionNameError::TooLong(name.len()));
    }
    // Iterate raw bytes; allowed characters are ASCII (alphanumeric, `_`,
    // `:`). Any control, whitespace, symbol (`.`, `-`, `@`, …) or non-ASCII
    // byte is rejected here; the `::` separator, empty segments and any lone
    // `:` are handled by the segment pass below.
    for (index, b) in name.bytes().enumerate() {
        if !(b.is_ascii_alphanumeric() || b == b'_' || b == b':') {
            return Err(FunctionNameError::InvalidChar {
                index,
                ch: name[index..].chars().next().unwrap_or('\u{fffd}'),
            });
        }
    }
    // Walk the `::`-separated segments tracking each segment's exact byte
    // offset, so an empty segment is reported at the offset of the separator
    // that opens it rather than at the first separator found in the name.
    let mut cursor = 0usize;
    for segment in name.split("::") {
        if segment.is_empty() {
            // `cursor` is the byte offset at which this empty segment begins:
            // the start of a `::` separator, or the end of the name when the
            // name ends in `::`.
            return Err(FunctionNameError::EmptySegment { index: cursor });
        }
        let mut chars = segment.chars();
        let first = chars.next().expect("segment is non-empty");
        let valid_start = first.is_ascii_alphabetic() || first == '_';
        if !valid_start {
            return Err(FunctionNameError::InvalidChar {
                index: cursor,
                ch: first,
            });
        }
        for (offset, c) in segment.char_indices() {
            if !(c.is_ascii_alphanumeric() || c == '_') {
                return Err(FunctionNameError::InvalidChar {
                    index: cursor + offset,
                    ch: c,
                });
            }
        }
        cursor += segment.len() + 2; // skip this segment and the `::` separator
    }
    Ok(())
}

/// Errors produced while building a [`HostApiCatalog`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostApiCatalogError {
    DuplicateResourceKey(ResourceTypeKey),
    /// Two registered functions share a name and an identical ordered argument
    /// type/passing sequence, making the overload set ambiguous. Parameter
    /// names, return type and documentation do not disambiguate call sites.
    DuplicateFunctionSignature {
        name: String,
    },
    InvalidFunctionName {
        name: String,
        reason: FunctionNameError,
    },
    DuplicateParameterName {
        function: String,
        parameter: String,
    },
    UnknownResourceReference {
        function: String,
        key: ResourceTypeKey,
    },
    /// A borrow/ownership passing mode was used on a non-resource parameter.
    NonResourcePassingMode {
        function: String,
        parameter: String,
        passing: HostParamPassing,
    },
    /// A resource-containing parameter was declared with `Value`; an explicit
    /// `Borrow`/`BorrowMut`/`TakeOwned` is required.
    ResourceValuePassing {
        function: String,
        parameter: String,
    },
    SchemaValidation(HostSchemaValidationError),
}

impl fmt::Display for HostApiCatalogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateResourceKey(key) => write!(f, "duplicate resource type key `{key}`"),
            Self::DuplicateFunctionSignature { name } => write!(
                f,
                "duplicate host function overload `{name}`: identical name and identical \
                 argument type/passing sequence (parameter names and return type cannot \
                 disambiguate overloads)"
            ),
            Self::InvalidFunctionName { name, reason } => {
                write!(f, "invalid host function name `{name}`: {reason}")
            }
            Self::DuplicateParameterName {
                function,
                parameter,
            } => write!(
                f,
                "host function `{function}` declares duplicate parameter name `{parameter}`"
            ),
            Self::UnknownResourceReference { function, key } => write!(
                f,
                "host function `{function}` references undeclared resource type `{key}`"
            ),
            Self::NonResourcePassingMode {
                function,
                parameter,
                passing,
            } => write!(
                f,
                "host function `{function}` uses passing mode {passing:?} on non-resource \
                 parameter `{parameter}`; value types must use `Value`",
            ),
            Self::ResourceValuePassing {
                function,
                parameter,
            } => write!(
                f,
                "host function `{function}` passes resource-containing parameter `{parameter}` \
                 by `Value`; an explicit Borrow/BorrowMut/TakeOwned is required",
            ),
            Self::SchemaValidation(error) => error.fmt(f),
        }
    }
}

impl From<HostSchemaValidationError> for HostApiCatalogError {
    fn from(error: HostSchemaValidationError) -> Self {
        Self::SchemaValidation(error)
    }
}

impl std::error::Error for HostApiCatalogError {}

/// An immutable, validated catalog of the host API surface.
///
/// Construction is done via the builder ([`HostApiCatalog::builder`]) or via
/// serde; both routes run the same validation, so a catalog is only exposed
/// once all cross-references, passing-mode and name/overload invariants hold.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostApiCatalog {
    resources: Vec<ResourceTypeSchema>,
    functions: Vec<HostFunctionSchema>,
}

/// A deterministic 64-bit fingerprint of a [`HostApiCatalog`].
///
/// Computed by FNV-1a over a canonical encoding of the semantic fields only,
/// prefixed by a domain magic and a format version. This is an equality /
/// change-detection digest only — **never** an authentication or integrity
/// value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct HostApiFingerprint(u64);

impl HostApiFingerprint {
    #[allow(dead_code)]
    pub(crate) const fn from_wire(value: u64) -> Self {
        Self(value)
    }

    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

impl serde::Serialize for HostApiFingerprint {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_u64(self.0)
    }
}

impl<'de> serde::Deserialize<'de> for HostApiFingerprint {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(HostApiFingerprint(u64::deserialize(deserializer)?))
    }
}

impl fmt::Display for HostApiFingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

impl Serialize for HostApiCatalog {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.validate().map_err(serde::ser::Error::custom)?;
        let mut state = serializer.serialize_struct("HostApiCatalog", 2)?;
        state.serialize_field("resources", &self.resources)?;
        state.serialize_field("functions", &self.functions)?;
        state.end()
    }
}

#[derive(Deserialize)]
#[serde(field_identifier, rename_all = "snake_case")]
enum HostApiCatalogField {
    Resources,
    Functions,
}

struct CatalogResourceSeed<'a> {
    budget: &'a mut ComplexityBudget,
}

impl<'de> DeserializeSeed<'de> for CatalogResourceSeed<'_> {
    type Value = ResourceTypeSchema;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        self.budget.charge_resources(1).map_err(de::Error::custom)?;
        ResourceTypeSchema::deserialize(deserializer)
    }
}

struct CatalogFunctionSeed<'a> {
    budget: &'a mut ComplexityBudget,
}

impl<'de> DeserializeSeed<'de> for CatalogFunctionSeed<'_> {
    type Value = HostFunctionSchema;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        self.budget.charge_functions(1).map_err(de::Error::custom)?;
        HostFunctionSchemaSeed {
            budget: self.budget,
        }
        .deserialize(deserializer)
    }
}

struct CatalogResourceListSeed<'a> {
    budget: &'a mut ComplexityBudget,
}

impl<'de> DeserializeSeed<'de> for CatalogResourceListSeed<'_> {
    type Value = Vec<ResourceTypeSchema>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_seq(CatalogResourceListVisitor {
            budget: self.budget,
        })
    }
}

struct CatalogResourceListVisitor<'a> {
    budget: &'a mut ComplexityBudget,
}

impl<'de> Visitor<'de> for CatalogResourceListVisitor<'_> {
    type Value = Vec<ResourceTypeSchema>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a bounded catalog resource list")
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let hint = seq.size_hint();
        let capacity = bounded_sequence_capacity(
            hint,
            MAX_HOST_CATALOG_RESOURCES - self.budget.resources,
            HostSchemaValidationError::ResourceBudgetExceeded {
                limit: MAX_HOST_CATALOG_RESOURCES,
            },
        )?;
        let mut values = Vec::new();
        if capacity != 0 {
            values.try_reserve_exact(capacity).map_err(|_| {
                de::Error::custom(HostSchemaValidationError::AllocationFailed {
                    field: "catalog resources",
                })
            })?;
        }
        while let Some(value) = seq.next_element_seed(CatalogResourceSeed {
            budget: self.budget,
        })? {
            values.try_reserve_exact(1).map_err(|_| {
                de::Error::custom(HostSchemaValidationError::AllocationFailed {
                    field: "catalog resources",
                })
            })?;
            values.push(value);
        }
        Ok(values)
    }
}

struct CatalogFunctionListSeed<'a> {
    budget: &'a mut ComplexityBudget,
}

impl<'de> DeserializeSeed<'de> for CatalogFunctionListSeed<'_> {
    type Value = Vec<HostFunctionSchema>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_seq(CatalogFunctionListVisitor {
            budget: self.budget,
        })
    }
}

struct CatalogFunctionListVisitor<'a> {
    budget: &'a mut ComplexityBudget,
}

impl<'de> Visitor<'de> for CatalogFunctionListVisitor<'_> {
    type Value = Vec<HostFunctionSchema>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a bounded catalog function list")
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let hint = seq.size_hint();
        let capacity = bounded_sequence_capacity(
            hint,
            MAX_HOST_CATALOG_FUNCTIONS - self.budget.functions,
            HostSchemaValidationError::FunctionBudgetExceeded {
                limit: MAX_HOST_CATALOG_FUNCTIONS,
            },
        )?;
        let mut values = Vec::new();
        if capacity != 0 {
            values.try_reserve_exact(capacity).map_err(|_| {
                de::Error::custom(HostSchemaValidationError::AllocationFailed {
                    field: "catalog functions",
                })
            })?;
        }
        while let Some(value) = seq.next_element_seed(CatalogFunctionSeed {
            budget: self.budget,
        })? {
            values.try_reserve_exact(1).map_err(|_| {
                de::Error::custom(HostSchemaValidationError::AllocationFailed {
                    field: "catalog functions",
                })
            })?;
            values.push(value);
        }
        Ok(values)
    }
}

struct HostApiCatalogVisitor<'a> {
    budget: &'a mut ComplexityBudget,
}

impl<'de> Visitor<'de> for HostApiCatalogVisitor<'_> {
    type Value = HostApiCatalog;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a bounded host API catalog object")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        bounded_map_size_hint(map.size_hint(), "host API catalog", 2)?;
        let mut entries = 0;
        let mut resources = None;
        let mut functions = None;
        loop {
            let Some(field) = map.next_key::<HostApiCatalogField>()? else {
                break;
            };
            bounded_map_entry(&mut entries, "host API catalog", 2)?;
            match field {
                HostApiCatalogField::Resources => {
                    if resources.is_some() {
                        return Err(de::Error::duplicate_field("resources"));
                    }
                    resources = Some(map.next_value_seed(CatalogResourceListSeed {
                        budget: self.budget,
                    })?);
                }
                HostApiCatalogField::Functions => {
                    if functions.is_some() {
                        return Err(de::Error::duplicate_field("functions"));
                    }
                    functions = Some(map.next_value_seed(CatalogFunctionListSeed {
                        budget: self.budget,
                    })?);
                }
            }
        }
        HostApiBuilder {
            resources: resources.ok_or_else(|| de::Error::missing_field("resources"))?,
            functions: functions.ok_or_else(|| de::Error::missing_field("functions"))?,
        }
        .build()
        .map_err(de::Error::custom)
    }
}

impl<'de> Deserialize<'de> for HostApiCatalog {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let mut budget = ComplexityBudget::default();
        deserializer.deserialize_struct(
            "HostApiCatalog",
            &["resources", "functions"],
            HostApiCatalogVisitor {
                budget: &mut budget,
            },
        )
    }
}

/// Stage-one, mutable builder for a [`HostApiCatalog`].
///
/// Cross-function invariants (referenced resource keys being declared,
/// reference/ownership passing modes, overload signatures, name grammar,
/// duplicate parameter names) are enforced in [`HostApiBuilder::build`], which
/// is what makes construction order independent.
#[derive(Clone, Debug, Default)]
pub struct HostApiBuilder {
    resources: Vec<ResourceTypeSchema>,
    functions: Vec<HostFunctionSchema>,
}

impl Default for HostApiCatalog {
    fn default() -> Self {
        Self::builder()
            .build()
            .expect("empty catalog is always valid")
    }
}

impl HostApiCatalog {
    /// Starts an empty, validated-construction catalog builder.
    pub fn builder() -> HostApiBuilder {
        HostApiBuilder::default()
    }

    /// Looks up a host function by exact name, returning it **only when it is
    /// unambiguous** (exactly one registered function matches). If none match,
    /// or the name is legally overloaded, this returns `None` — use
    /// [`Self::functions_named`] to resolve overloads.
    pub fn function(&self, name: &str) -> Option<&HostFunctionSchema> {
        match self.functions_named(name)[..] {
            [single] => Some(single),
            _ => None,
        }
    }

    /// All host functions registered under the given name, preserving
    /// registration order. An empty slice means the name is not declared; a
    /// non-empty slice of length > 1 means the name is overloaded.
    pub fn functions_named(&self, name: &str) -> Vec<&HostFunctionSchema> {
        self.functions
            .iter()
            .filter(|function| function.name == name)
            .collect()
    }

    /// Looks up a function by the complete compiled host-import identity.
    ///
    /// Name and arity are only preliminary facts. The parameter labels,
    /// schemas (including nominal resource keys), passing modes, return schema
    /// and catalog fingerprint all participate in the match. Documentation is
    /// intentionally excluded because it is not part of a compiled import's
    /// identity.
    pub fn function_for_import(&self, import: &HostImportSchema) -> Option<&HostFunctionSchema> {
        if import.validate().is_err() {
            return None;
        }
        self.functions
            .iter()
            .find(|function| HostImportSchema::from_function(self, function) == *import)
    }

    /// Looks up a declared resource type by key text.
    pub fn resource(&self, key: &str) -> Option<&ResourceTypeSchema> {
        self.resources
            .iter()
            .find(|resource| resource.key.as_str() == key)
    }

    /// Whether the catalog declares the given resource type key.
    pub fn has_resource(&self, key: &ResourceTypeKey) -> bool {
        self.resources.iter().any(|resource| &resource.key == key)
    }

    /// All declared resource types (in registration order).
    pub fn resources(&self) -> &[ResourceTypeSchema] {
        &self.resources
    }

    /// All host functions (in registration order).
    pub fn functions(&self) -> &[HostFunctionSchema] {
        &self.functions
    }

    /// Validates this catalog with the same bounded traversal used by the
    /// builder and all identity paths.
    pub fn validate(&self) -> Result<(), HostApiCatalogError> {
        validate_surface(&self.resources, &self.functions)
    }

    /// Canonical semantic bytes for the whole catalog: `FINGERPRINT_DOMAIN_MAGIC`
    /// ++ `FINGERPRINT_FORMAT_VERSION` ++ resources (sorted by key) ++
    /// functions (sorted by full semantic signature bytes).
    fn canonical_bytes(&self) -> Vec<u8> {
        self.try_canonical_bytes()
            .unwrap_or_else(|error| invalid_catalog_bytes(&error))
    }

    fn try_canonical_bytes(&self) -> Result<Vec<u8>, HostApiCatalogError> {
        self.validate()?;
        let mut bytes = Vec::new();

        bytes.extend_from_slice(FINGERPRINT_DOMAIN_MAGIC);
        bytes.push(FINGERPRINT_FORMAT_VERSION);

        // Resources sorted by key text.
        let mut resources: Vec<&ResourceTypeSchema> = self.resources.iter().collect();
        resources.sort_by(|a, b| a.key.cmp(&b.key));
        push_tag(&mut bytes, b'R');
        push_len(&mut bytes, resources.len())?;
        for resource in &resources {
            push_len_str(&mut bytes, resource.key.as_str())?;
        }

        // Functions sorted by their full canonical semantic signature bytes so
        // overloaded registration order is irrelevant (exact duplicates are
        // already rejected at build time).
        let mut functions: Vec<(Vec<u8>, &HostFunctionSchema)> = self
            .functions
            .iter()
            .map(|function| (function.semantic_bytes(), function))
            .collect();
        functions.sort_by(|a, b| a.0.cmp(&b.0));
        push_tag(&mut bytes, b'F');
        push_len(&mut bytes, functions.len())?;
        for (semantic, _) in functions {
            bytes.extend(semantic);
        }

        Ok(bytes)
    }

    /// Deterministic, order-independent fingerprint of the semantic contents.
    ///
    /// The fingerprint covers resource keys and every function’s name,
    /// parameter (name, type, passing mode) and return type. It excludes
    /// documentation and registration order. See the module doc for the
    /// security caveat: this 64-bit FNV digest is equality / change-detection
    /// only, never authentication.
    pub fn fingerprint(&self) -> HostApiFingerprint {
        HostApiFingerprint(fnv1a(&self.canonical_bytes()))
    }

    /// Fallible fingerprint calculation for callers loading a catalog from an
    /// external source and needing a stable validation error.
    pub fn try_fingerprint(&self) -> Result<HostApiFingerprint, HostApiCatalogError> {
        Ok(HostApiFingerprint(fnv1a(&self.try_canonical_bytes()?)))
    }
}

/// Validates a complete collection of host import schemas with one aggregate
/// budget. This is used by public program-loading and registration boundaries.
pub fn validate_host_import_schemas(
    schemas: &[HostImportSchema],
) -> Result<(), HostSchemaValidationError> {
    validate_host_import_schema_iter(schemas.iter())
}

#[cfg(feature = "runtime")]
pub(crate) fn validate_optional_host_import_schemas(
    schemas: &[Option<HostImportSchema>],
) -> Result<(), HostSchemaValidationError> {
    validate_host_import_schema_iter(schemas.iter().filter_map(Option::as_ref))
}

fn validate_host_import_schema_iter<'a, I>(schemas: I) -> Result<(), HostSchemaValidationError>
where
    I: IntoIterator<Item = &'a HostImportSchema>,
{
    let mut budget = ComplexityBudget::default();
    for schema in schemas {
        budget.charge_functions(1)?;
        validate_import_shape(schema, &mut budget)?;
    }
    Ok(())
}

/// Validate the caller-supplied resource/function collections. Shared by the
/// builder and the serde path so both reject the same malformed inputs.
fn validate_surface(
    resources: &[ResourceTypeSchema],
    functions: &[HostFunctionSchema],
) -> Result<(), HostApiCatalogError> {
    let mut budget = ComplexityBudget::default();
    budget
        .charge_resources(resources.len())
        .map_err(HostApiCatalogError::from)?;
    budget
        .charge_functions(functions.len())
        .map_err(HostApiCatalogError::from)?;

    for resource in resources {
        validate_description(&resource.description).map_err(HostApiCatalogError::from)?;
    }

    // Duplicate resource keys.
    for (i, resource) in resources.iter().enumerate() {
        if resources[..i].iter().any(|prior| prior.key == resource.key) {
            return Err(HostApiCatalogError::DuplicateResourceKey(
                resource.key.clone(),
            ));
        }
    }

    // Per-function invariants.
    for function in functions {
        // Preserve the catalog-specific name error for callers that already
        // match this public error variant.
        if let Err(reason) = validate_function_name(&function.name) {
            return Err(HostApiCatalogError::InvalidFunctionName {
                name: function.name.clone(),
                reason,
            });
        }
        validate_description(&function.description).map_err(HostApiCatalogError::from)?;
        budget
            .charge_parameters(function.params.len())
            .map_err(HostApiCatalogError::from)?;

        // Unique parameter names.
        for (i, param) in function.params.iter().enumerate() {
            validate_parameter_name(&param.name).map_err(HostApiCatalogError::from)?;
            if function.params[..i]
                .iter()
                .any(|prior| prior.name == param.name)
            {
                return Err(HostApiCatalogError::DuplicateParameterName {
                    function: function.name.clone(),
                    parameter: param.name.clone(),
                });
            }
        }

        // Passing-mode and resource-reference invariants. The same iterative
        // schema validator also performs the aggregate node/property accounting.
        for param in &function.params {
            let mut missing_key = None;
            let mut on_resource = |key: &ResourceTypeKey| {
                if missing_key.is_none() && !resources.iter().any(|resource| &resource.key == key) {
                    missing_key = Some(key.clone());
                }
            };
            let contains_resource =
                validate_type_schema_with_budget(&param.ty, &mut budget, &mut on_resource)
                    .map_err(HostApiCatalogError::from)?;

            if let Some(key) = missing_key {
                return Err(HostApiCatalogError::UnknownResourceReference {
                    function: function.name.clone(),
                    key,
                });
            }
            if contains_resource {
                // A resource-containing parameter must use an explicit mode.
                if param.passing == HostParamPassing::Value {
                    return Err(HostApiCatalogError::ResourceValuePassing {
                        function: function.name.clone(),
                        parameter: param.name.clone(),
                    });
                }
            } else if param.passing.is_reference_mode() {
                // A non-resource parameter must use `Value`.
                return Err(HostApiCatalogError::NonResourcePassingMode {
                    function: function.name.clone(),
                    parameter: param.name.clone(),
                    passing: param.passing,
                });
            }
        }

        // Return references must be declared too.
        let mut missing_key = None;
        let mut on_resource = |key: &ResourceTypeKey| {
            if missing_key.is_none() && !resources.iter().any(|resource| &resource.key == key) {
                missing_key = Some(key.clone());
            }
        };
        validate_type_schema_with_budget(&function.return_type, &mut budget, &mut on_resource)
            .map_err(HostApiCatalogError::from)?;
        if let Some(key) = missing_key {
            return Err(HostApiCatalogError::UnknownResourceReference {
                function: function.name.clone(),
                key,
            });
        }
    }

    // Reject ambiguous overloads: two functions sharing a name and an identical
    // ordered argument type/passing sequence. Parameter names and the return
    // type do not disambiguate call sites, so same-name overloads that differ
    // only in labels or return schema are rejected. Legal overloads (same name,
    // distinct argument schema) are allowed.
    for (i, function) in functions.iter().enumerate() {
        let identity = function
            .try_overload_identity_bytes()
            .map_err(HostApiCatalogError::from)?;
        for prior in &functions[..i] {
            if prior
                .try_overload_identity_bytes()
                .map_err(HostApiCatalogError::from)?
                == identity
            {
                return Err(HostApiCatalogError::DuplicateFunctionSignature {
                    name: function.name.clone(),
                });
            }
        }
    }

    Ok(())
}

impl HostApiBuilder {
    /// Starts an empty catalog builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a resource type.
    pub fn resource(&mut self, resource: ResourceTypeSchema) {
        self.resources.push(resource);
    }

    /// Registers a host function signature. Same-name functions with distinct
    /// signatures (overloads) are allowed.
    pub fn function(&mut self, function: HostFunctionSchema) {
        self.functions.push(function);
    }

    /// Returns the number of resource types registered so far.
    pub fn resource_count(&self) -> usize {
        self.resources.len()
    }

    /// Returns the number of functions registered so far.
    pub fn function_count(&self) -> usize {
        self.functions.len()
    }

    /// Validates and freezes the catalog.
    pub fn build(self) -> Result<HostApiCatalog, HostApiCatalogError> {
        validate_surface(&self.resources, &self.functions)?;
        Ok(HostApiCatalog {
            resources: self.resources,
            functions: self.functions,
        })
    }
}

fn push_tag(bytes: &mut Vec<u8>, tag: u8) {
    bytes.push(tag);
}

fn push_len(bytes: &mut Vec<u8>, value: usize) -> Result<(), HostSchemaValidationError> {
    // Fixed 8-byte little-endian length so encodings are unambiguous, and any
    // structural field write is order-independent in aggregate.
    let value = u64::try_from(value)
        .map_err(|_| HostSchemaValidationError::IntegerOverflow { field: "length" })?;
    bytes.extend_from_slice(&value.to_le_bytes());
    Ok(())
}

fn push_len_str(bytes: &mut Vec<u8>, value: &str) -> Result<(), HostSchemaValidationError> {
    push_len(bytes, value.len())?;
    bytes.extend_from_slice(value.as_bytes());
    Ok(())
}

fn try_push_type(
    bytes: &mut Vec<u8>,
    schema: &HostTypeSchema,
) -> Result<(), HostSchemaValidationError> {
    let mut pending: Vec<&HostTypeSchema> = Vec::new();
    pending
        .try_reserve(1)
        .map_err(|_| HostSchemaValidationError::AllocationFailed {
            field: "schema encoding",
        })?;
    pending.push(schema);

    while let Some(current) = pending.pop() {
        match current {
            HostTypeSchema::Unknown => push_tag(bytes, b'U'),
            HostTypeSchema::Null => push_tag(bytes, b'N'),
            HostTypeSchema::Int => push_tag(bytes, b'I'),
            HostTypeSchema::Float => push_tag(bytes, b'F'),
            HostTypeSchema::Number => push_tag(bytes, b'#'),
            HostTypeSchema::Bool => push_tag(bytes, b'B'),
            HostTypeSchema::String => push_tag(bytes, b'S'),
            HostTypeSchema::Bytes => push_tag(bytes, b'Y'),
            HostTypeSchema::Array(inner) => {
                push_tag(bytes, b'[');
                pending.try_reserve(1).map_err(|_| {
                    HostSchemaValidationError::AllocationFailed {
                        field: "schema encoding",
                    }
                })?;
                pending.push(inner);
            }
            HostTypeSchema::Map(inner) => {
                push_tag(bytes, b'{');
                pending.try_reserve(1).map_err(|_| {
                    HostSchemaValidationError::AllocationFailed {
                        field: "schema encoding",
                    }
                })?;
                pending.push(inner);
            }
            HostTypeSchema::Optional(inner) => {
                push_tag(bytes, b'?');
                pending.try_reserve(1).map_err(|_| {
                    HostSchemaValidationError::AllocationFailed {
                        field: "schema encoding",
                    }
                })?;
                pending.push(inner);
            }
            HostTypeSchema::Callable { params, result } => {
                push_tag(bytes, b'c');
                push_len(bytes, params.len())?;
                let needed = params.len().checked_add(1).ok_or(
                    HostSchemaValidationError::IntegerOverflow {
                        field: "schema encoding capacity",
                    },
                )?;
                pending.try_reserve(needed).map_err(|_| {
                    HostSchemaValidationError::AllocationFailed {
                        field: "schema encoding",
                    }
                })?;
                pending.push(result);
                for param in params.iter().rev() {
                    pending.push(param);
                }
            }
            HostTypeSchema::Resource(key) => {
                push_tag(bytes, b'r');
                push_len_str(bytes, key.as_str())?;
            }
        }
    }
    Ok(())
}

fn invalid_schema_bytes(error: &HostSchemaValidationError) -> Vec<u8> {
    let mut bytes = b"invalid-host-schema:".to_vec();
    bytes.extend_from_slice(error.to_string().as_bytes());
    bytes
}

fn invalid_catalog_bytes(error: &HostApiCatalogError) -> Vec<u8> {
    let mut bytes = b"invalid-host-api-catalog:".to_vec();
    bytes.extend_from_slice(error.to_string().as_bytes());
    bytes
}

fn passing_tag(passing: HostParamPassing) -> u8 {
    match passing {
        HostParamPassing::Value => b'v',
        // Distinct tags so Borrow and BorrowMut are semantically different.
        HostParamPassing::Borrow => b'b',
        HostParamPassing::BorrowMut => b'm',
        HostParamPassing::TakeOwned => b'o',
    }
}

const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = FNV_OFFSET_BASIS;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn io_file_key() -> ResourceTypeKey {
        ResourceTypeKey::new("io.file").expect("valid key")
    }

    fn sqlite_connection_key() -> ResourceTypeKey {
        ResourceTypeKey::new("sqlite.connection").expect("valid key")
    }

    fn io_file_resource() -> ResourceTypeSchema {
        ResourceTypeSchema::new(io_file_key(), "An open file handle")
    }

    fn sqlite_connection_resource() -> ResourceTypeSchema {
        ResourceTypeSchema::new(sqlite_connection_key(), "An open SQLite connection")
    }

    fn fn_io_open(docs: &str) -> HostFunctionSchema {
        HostFunctionSchema::with_return(
            "io::open",
            vec![
                HostParamSchema::value("path", HostTypeSchema::String),
                HostParamSchema::value("mode", HostTypeSchema::String),
            ],
            HostTypeSchema::Resource(io_file_key()),
        )
        .with_description(docs)
    }

    fn fn_io_read_all(passing: HostParamPassing) -> HostFunctionSchema {
        HostFunctionSchema::with_return(
            "io::read_all",
            vec![HostParamSchema::with_passing(
                "handle",
                HostTypeSchema::Resource(io_file_key()),
                passing,
            )],
            HostTypeSchema::String,
        )
    }

    fn fn_sqlite_open() -> HostFunctionSchema {
        HostFunctionSchema::with_return(
            "sqlite::open",
            vec![HostParamSchema::value("path", HostTypeSchema::String)],
            HostTypeSchema::Resource(sqlite_connection_key()),
        )
    }

    fn catalog_with_io_and_sqlite() -> HostApiCatalog {
        let mut builder = HostApiCatalog::builder();
        builder.resource(io_file_resource());
        builder.resource(sqlite_connection_resource());
        builder.function(fn_io_open("docs"));
        builder.function(fn_io_read_all(HostParamPassing::Borrow));
        builder.function(fn_sqlite_open());
        builder.build().expect("valid catalog")
    }

    // --- ResourceTypeKey validation ---

    #[test]
    fn resource_type_key_validation() {
        assert!(ResourceTypeKey::new("io.file").is_ok());
        assert!(ResourceTypeKey::new("sqlite.connection").is_ok());
        assert!(ResourceTypeKey::new("a-b_c.0").is_ok());
        assert_eq!(ResourceTypeKey::new(""), Err(ResourceTypeKeyError::Empty));
        assert!(ResourceTypeKey::new("A").is_err());
        assert!(ResourceTypeKey::new("has space").is_err());
        assert!(ResourceTypeKey::new(".leading").is_err());
        assert!(ResourceTypeKey::new("trailing.").is_err());
        assert!(ResourceTypeKey::new("double..dot").is_err());
        assert!(ResourceTypeKey::new("a".repeat(129)).is_err());
    }

    #[test]
    fn resource_type_key_deduplicates_by_value() {
        assert_eq!(
            ResourceTypeKey::new("io.file").unwrap(),
            ResourceTypeKey::new("io.file").unwrap()
        );
        assert_ne!(
            ResourceTypeKey::new("io.file").unwrap(),
            ResourceTypeKey::new("io.file2").unwrap()
        );
    }

    // --- Catalog construction validation ---

    #[test]
    fn duplicate_resource_key_rejected() {
        let mut builder = HostApiCatalog::builder();
        builder.resource(io_file_resource());
        builder.resource(ResourceTypeSchema::new(io_file_key(), "duplicate"));
        assert_eq!(
            builder.build(),
            Err(HostApiCatalogError::DuplicateResourceKey(io_file_key()))
        );
    }

    // --- Overloading ---

    #[test]
    fn legal_overloads_allowed() {
        // Standard builtins legally overload `len` for multiple value shapes.
        let mut builder = HostApiCatalog::builder();
        builder.function(HostFunctionSchema::with_return(
            "len",
            vec![HostParamSchema::value("value", HostTypeSchema::String)],
            HostTypeSchema::Int,
        ));
        builder.function(HostFunctionSchema::with_return(
            "len",
            vec![HostParamSchema::value(
                "value",
                HostTypeSchema::Map(Box::new(HostTypeSchema::String)),
            )],
            HostTypeSchema::Int,
        ));
        builder.function(HostFunctionSchema::with_return(
            "len",
            vec![HostParamSchema::value("value", HostTypeSchema::Bytes)],
            HostTypeSchema::Int,
        ));
        let catalog = builder.build().expect("legal overloads must build");
        assert_eq!(catalog.functions_named("len").len(), 3);
        // Ambiguous name => `function` returns None, `functions_named` returns all.
        assert!(catalog.function("len").is_none());
    }

    #[test]
    fn exact_duplicate_overload_rejected() {
        let mut builder = HostApiCatalog::builder();
        builder.resource(io_file_resource());
        builder.function(fn_io_open("one"));
        // Identical name, identical params, identical return => exact duplicate.
        let duplicate = HostFunctionSchema::with_return(
            "io::open",
            vec![
                HostParamSchema::value("path", HostTypeSchema::String),
                HostParamSchema::value("mode", HostTypeSchema::String),
            ],
            HostTypeSchema::Resource(io_file_key()),
        );
        builder.function(duplicate);
        assert_eq!(
            builder.build(),
            Err(HostApiCatalogError::DuplicateFunctionSignature {
                name: "io::open".to_string()
            })
        );
    }

    #[test]
    fn same_signature_different_return_rejected() {
        // Same name, same argument type/passing sequence, but a differing
        // return type: still ambiguous at call sites, so rejected.
        let mut builder = HostApiCatalog::builder();
        builder.function(HostFunctionSchema::with_return(
            "convert",
            vec![HostParamSchema::value("value", HostTypeSchema::Int)],
            HostTypeSchema::Int,
        ));
        builder.function(HostFunctionSchema::with_return(
            "convert",
            vec![HostParamSchema::value("value", HostTypeSchema::Int)],
            HostTypeSchema::String,
        ));
        assert_eq!(
            builder.build(),
            Err(HostApiCatalogError::DuplicateFunctionSignature {
                name: "convert".to_string()
            })
        );
    }

    #[test]
    fn same_signature_different_parameter_labels_rejected() {
        // Same name, same argument types+passing, but different parameter
        // labels => identical overload identity, so rejected.
        let mut builder = HostApiCatalog::builder();
        builder.function(HostFunctionSchema::with_return(
            "get",
            vec![
                HostParamSchema::value("a", HostTypeSchema::Int),
                HostParamSchema::value("b", HostTypeSchema::String),
            ],
            HostTypeSchema::Int,
        ));
        builder.function(HostFunctionSchema::with_return(
            "get",
            vec![
                HostParamSchema::value("x", HostTypeSchema::Int),
                HostParamSchema::value("y", HostTypeSchema::String),
            ],
            HostTypeSchema::Int,
        ));
        assert_eq!(
            builder.build(),
            Err(HostApiCatalogError::DuplicateFunctionSignature {
                name: "get".to_string()
            })
        );
    }

    #[test]
    fn ambiguous_argument_identity_with_resource_same_passing_rejected() {
        // Same resource argument and borrowing mode in both overloads, differing
        // only in the return resource: argument identity is the same => rejected.
        let mut builder = HostApiCatalog::builder();
        builder.resource(io_file_resource());
        builder.resource(sqlite_connection_resource());
        builder.function(HostFunctionSchema::with_return(
            "open",
            vec![HostParamSchema::with_passing(
                "path",
                HostTypeSchema::String,
                HostParamPassing::Value,
            )],
            HostTypeSchema::Resource(io_file_key()),
        ));
        builder.function(HostFunctionSchema::with_return(
            "open",
            vec![HostParamSchema::with_passing(
                "loc",
                HostTypeSchema::String,
                HostParamPassing::Value,
            )],
            HostTypeSchema::Resource(sqlite_connection_key()),
        ));
        assert_eq!(
            builder.build(),
            Err(HostApiCatalogError::DuplicateFunctionSignature {
                name: "open".to_string()
            })
        );
    }

    #[test]
    fn ambiguous_function_lookup_returns_none() {
        let mut builder = HostApiCatalog::builder();
        builder.function(HostFunctionSchema::with_return(
            "len",
            vec![HostParamSchema::value("value", HostTypeSchema::String)],
            HostTypeSchema::Int,
        ));
        builder.function(HostFunctionSchema::with_return(
            "len",
            vec![HostParamSchema::value("value", HostTypeSchema::Int)],
            HostTypeSchema::Int,
        ));
        let catalog = builder.build().expect("valid");
        assert!(catalog.function("len").is_none());
        assert_eq!(catalog.functions_named("len").len(), 2);
        assert!(catalog.function("absent").is_none());
        assert!(catalog.functions_named("absent").is_empty());
    }

    #[test]
    fn unambiguous_function_lookup_returns_it() {
        let catalog = catalog_with_io_and_sqlite();
        assert_eq!(
            catalog.function("io::open").expect("unique").name,
            "io::open"
        );
        assert!(catalog.function("io::read_all").is_some());
    }

    // --- Ownership mode enforcement ---

    #[test]
    fn non_resource_borrow_rejected() {
        let mut builder = HostApiCatalog::builder();
        builder.resource(io_file_resource());
        builder.function(HostFunctionSchema::with_return(
            "write",
            vec![HostParamSchema::with_passing(
                "text",
                HostTypeSchema::String,
                HostParamPassing::Borrow,
            )],
            HostTypeSchema::Null,
        ));
        assert_eq!(
            builder.build(),
            Err(HostApiCatalogError::NonResourcePassingMode {
                function: "write".to_string(),
                parameter: "text".to_string(),
                passing: HostParamPassing::Borrow,
            })
        );
    }

    #[test]
    fn non_resource_take_owned_rejected() {
        let mut builder = HostApiCatalog::builder();
        builder.resource(io_file_resource());
        builder.function(HostFunctionSchema::with_return(
            "consume",
            vec![HostParamSchema::with_passing(
                "value",
                HostTypeSchema::Int,
                HostParamPassing::TakeOwned,
            )],
            HostTypeSchema::Int,
        ));
        assert_eq!(
            builder.build(),
            Err(HostApiCatalogError::NonResourcePassingMode {
                function: "consume".to_string(),
                parameter: "value".to_string(),
                passing: HostParamPassing::TakeOwned,
            })
        );
    }

    #[test]
    fn non_resource_deeply_nested_borrow_rejected() {
        // An Array<String> contains no resource, so Borrow is forbidden even
        // though `resource_key()` (shallow) would say None too.
        let mut builder = HostApiCatalog::builder();
        let array_of_strings = HostTypeSchema::Array(Box::new(HostTypeSchema::String));
        assert!(!array_of_strings.contains_resource());
        builder.function(HostFunctionSchema::with_return(
            "join",
            vec![HostParamSchema::with_passing(
                "parts",
                array_of_strings,
                HostParamPassing::Borrow,
            )],
            HostTypeSchema::String,
        ));
        assert!(matches!(
            builder.build(),
            Err(HostApiCatalogError::NonResourcePassingMode { .. })
        ));
    }

    #[test]
    fn resource_value_passing_rejected() {
        for ty in [
            HostTypeSchema::Resource(io_file_key()),
            HostTypeSchema::Optional(Box::new(HostTypeSchema::Resource(io_file_key()))),
            HostTypeSchema::Array(Box::new(HostTypeSchema::Resource(io_file_key()))),
            HostTypeSchema::Map(Box::new(HostTypeSchema::Resource(io_file_key()))),
            HostTypeSchema::Callable {
                params: vec![HostTypeSchema::Resource(io_file_key())],
                result: Box::new(HostTypeSchema::String),
            },
        ] {
            assert!(ty.contains_resource(), "schema must carry a resource");
            let mut builder = HostApiCatalog::builder();
            builder.resource(io_file_resource());
            builder.function(HostFunctionSchema::with_return(
                "takes_resource",
                vec![HostParamSchema::value("value", ty)],
                HostTypeSchema::Null,
            ));
            assert_eq!(
                builder.build(),
                Err(HostApiCatalogError::ResourceValuePassing {
                    function: "takes_resource".to_string(),
                    parameter: "value".to_string(),
                })
            );
        }
    }

    #[test]
    fn resource_in_container_with_explicit_pass_allowed() {
        // An Array<Resource> may be passed with an explicit mode (Borrow),
        // which applies call-scoped to the contained resources.
        let mut builder = HostApiCatalog::builder();
        builder.resource(io_file_resource());
        builder.function(HostFunctionSchema::with_return(
            "close_all",
            vec![HostParamSchema::with_passing(
                "handles",
                HostTypeSchema::Array(Box::new(HostTypeSchema::Resource(io_file_key()))),
                HostParamPassing::Borrow,
            )],
            HostTypeSchema::Null,
        ));
        builder.function(HostFunctionSchema::with_return(
            "reap",
            vec![HostParamSchema::with_passing(
                "handles",
                HostTypeSchema::Map(Box::new(HostTypeSchema::Resource(io_file_key()))),
                HostParamPassing::TakeOwned,
            )],
            HostTypeSchema::Null,
        ));
        builder
            .build()
            .expect("explicit aggregate passing is valid");
    }

    #[test]
    fn undeclared_resource_in_param_rejected() {
        let mut builder = HostApiCatalog::builder();
        builder.resource(io_file_resource());
        builder.function(HostFunctionSchema::with_return(
            "use_missing",
            vec![HostParamSchema::with_passing(
                "h",
                HostTypeSchema::Resource(ResourceTypeKey::new("missing.file").unwrap()),
                HostParamPassing::Borrow,
            )],
            HostTypeSchema::Null,
        ));
        assert!(matches!(
            builder.build(),
            Err(HostApiCatalogError::UnknownResourceReference { .. })
        ));
    }

    #[test]
    fn undeclared_resource_return_rejected() {
        let mut builder = HostApiCatalog::builder();
        builder.function(HostFunctionSchema::with_return(
            "open",
            vec![],
            HostTypeSchema::Resource(ResourceTypeKey::new("missing.file").unwrap()),
        ));
        assert!(matches!(
            builder.build(),
            Err(HostApiCatalogError::UnknownResourceReference { .. })
        ));
    }

    #[test]
    fn undeclared_resource_inside_container_rejected() {
        let mut builder = HostApiCatalog::builder();
        builder.resource(io_file_resource());
        builder.function(HostFunctionSchema::with_return(
            "get_files",
            vec![],
            HostTypeSchema::Map(Box::new(HostTypeSchema::Resource(
                ResourceTypeKey::new("db.files").unwrap(),
            ))),
        ));
        assert!(matches!(
            builder.build(),
            Err(HostApiCatalogError::UnknownResourceReference { .. })
        ));
    }

    // --- Function name grammar ---

    #[test]
    fn function_name_grammar_accepts_standard_names() {
        for name in [
            "len",
            "__bind_callable",
            "bytes::from_utf8",
            "io::open",
            "io::read_all",
            "jit::set_hot_loop_threshold",
            "math::atan2",
            "bytes::from_array_u8",
            "_private",
        ] {
            assert!(
                validate_function_name(name).is_ok(),
                "`{name}` must be a valid host function name"
            );
        }
    }

    #[test]
    fn function_name_grammar_rejects_malformed() {
        let invalid: &[&str] = &[
            "",
            "::leading",
            "trailing::",
            "double::::colon",
            "a:b",       // lone single colon, not `::`
            "a a",       // whitespace
            "1abc",      // segment starts with digit
            "a-b",       // hyphen is a symbol
            "a.b",       // dot is a resource-key separator, not a function separator
            "-x",        // leading symbol
            "a\nb",      // control/whitespace
            "a\\tb",     // tab
            "caf\u{e9}", // non-ASCII (é)
            "a\"b",      // quote symbol
        ];
        for name in invalid {
            assert!(
                validate_function_name(name).is_err(),
                "`{name}` should be rejected as a host function name"
            );
        }
    }

    #[test]
    fn function_name_too_long_rejected() {
        let too_long = "a".repeat(MAX_FUNCTION_NAME_LEN + 1);
        assert_eq!(
            validate_function_name(&too_long),
            Err(FunctionNameError::TooLong(too_long.len()))
        );
    }

    #[test]
    fn empty_function_name_rejected() {
        assert_eq!(validate_function_name(""), Err(FunctionNameError::Empty));
    }

    #[test]
    fn invalid_function_name_rejected_at_build() {
        let mut builder = HostApiCatalog::builder();
        builder.function(HostFunctionSchema::new("bad name", vec![]));
        assert!(matches!(
            builder.build(),
            Err(HostApiCatalogError::InvalidFunctionName { .. })
        ));
    }

    // --- Duplicate parameter names ---

    #[test]
    fn duplicate_parameter_name_rejected() {
        let mut builder = HostApiCatalog::builder();
        builder.function(HostFunctionSchema::with_return(
            "dup",
            vec![
                HostParamSchema::value("a", HostTypeSchema::Int),
                HostParamSchema::value("a", HostTypeSchema::String),
            ],
            HostTypeSchema::Null,
        ));
        assert_eq!(
            builder.build(),
            Err(HostApiCatalogError::DuplicateParameterName {
                function: "dup".to_string(),
                parameter: "a".to_string(),
            })
        );
    }

    // --- Display ---

    #[test]
    fn resource_displays_as_resource_angle_brackets() {
        let s = HostTypeSchema::Resource(io_file_key());
        assert_eq!(format!("{s}"), "resource<io.file>");
        let opt = HostTypeSchema::Optional(Box::new(s));
        assert_eq!(format!("{opt}"), "optional<resource<io.file>>");
    }

    // --- Fingerprint semantics ---

    #[test]
    fn fingerprint_has_domain_magic_and_version() {
        let catalog = catalog_with_io_and_sqlite();
        let bytes = catalog.canonical_bytes();
        assert_eq!(
            &bytes[..FINGERPRINT_DOMAIN_MAGIC.len()],
            FINGERPRINT_DOMAIN_MAGIC
        );
        assert_eq!(
            bytes[FINGERPRINT_DOMAIN_MAGIC.len()],
            FINGERPRINT_FORMAT_VERSION
        );
        assert_ne!(catalog.fingerprint().as_u64(), 0);
    }

    #[test]
    fn fingerprint_version_is_one() {
        assert_eq!(FINGERPRINT_FORMAT_VERSION, 1);
    }

    #[test]
    fn order_independent_fingerprint() {
        let mut builder_a = HostApiCatalog::builder();
        builder_a.resource(io_file_resource());
        builder_a.resource(sqlite_connection_resource());
        builder_a.function(fn_io_read_all(HostParamPassing::Borrow));
        builder_a.function(fn_sqlite_open());
        builder_a.function(fn_io_open("docs"));
        let catalog_a = builder_a.build().expect("valid");

        let mut builder_b = HostApiCatalog::builder();
        builder_b.function(fn_io_open("other docs"));
        builder_b.resource(sqlite_connection_resource());
        builder_b.function(fn_sqlite_open());
        builder_b.resource(io_file_resource());
        builder_b.function(fn_io_read_all(HostParamPassing::Borrow));
        let catalog_b = builder_b.build().expect("valid");

        assert_eq!(catalog_a.fingerprint(), catalog_b.fingerprint());
    }

    /// Two catalogs exposing the same overloaded `len` set but registered in
    /// different orders must fingerprint identically.
    #[test]
    fn overload_order_independent_fingerprint() {
        let mut builder_a = HostApiCatalog::builder();
        builder_a.function(len_overload(HostTypeSchema::String));
        builder_a.function(len_overload(HostTypeSchema::Array(Box::new(
            HostTypeSchema::Int,
        ))));
        builder_a.function(len_overload(HostTypeSchema::Bytes));
        let a = builder_a.build().expect("valid");

        let mut builder_b = HostApiCatalog::builder();
        builder_b.function(len_overload(HostTypeSchema::Bytes));
        builder_b.function(len_overload(HostTypeSchema::String));
        builder_b.function(len_overload(HostTypeSchema::Array(Box::new(
            HostTypeSchema::Int,
        ))));
        let b = builder_b.build().expect("valid");

        assert_eq!(a.fingerprint(), b.fingerprint());
        assert_eq!(a.fingerprint(), a.fingerprint());

        // Adding a distinct overload changes the fingerprint (semantic change).
        let mut builder_c = HostApiCatalog::builder();
        builder_c.function(len_overload(HostTypeSchema::String));
        builder_c.function(len_overload(HostTypeSchema::Array(Box::new(
            HostTypeSchema::Int,
        ))));
        builder_c.function(len_overload(HostTypeSchema::Map(Box::new(
            HostTypeSchema::String,
        ))));
        let c = builder_c.build().expect("valid");
        assert_ne!(a.fingerprint(), c.fingerprint());
    }

    #[test]
    fn semantic_change_alters_fingerprint() {
        let base = catalog_with_io_and_sqlite();

        let mut builder = HostApiCatalog::builder();
        builder.resource(io_file_resource());
        builder.resource(sqlite_connection_resource());
        builder.function(HostFunctionSchema::with_return(
            "io::open",
            vec![
                HostParamSchema::value("path", HostTypeSchema::String),
                HostParamSchema::value("mode", HostTypeSchema::String),
            ],
            HostTypeSchema::String,
        ));
        builder.function(fn_io_read_all(HostParamPassing::Borrow));
        builder.function(fn_sqlite_open());
        let changed = builder.build().expect("valid");

        assert_ne!(base.fingerprint(), changed.fingerprint());
    }

    #[test]
    fn param_label_change_alters_fingerprint() {
        // Overload identity ignores labels, but the fingerprint must still see
        // them (semantic_bytes is unchanged and label-full).
        let mut a = HostApiCatalog::builder();
        a.function(HostFunctionSchema::with_return(
            "f",
            vec![HostParamSchema::value("a", HostTypeSchema::Int)],
            HostTypeSchema::Int,
        ));
        let catalog_a = a.build().expect("valid");

        let mut b = HostApiCatalog::builder();
        b.function(HostFunctionSchema::with_return(
            "f",
            vec![HostParamSchema::value("renamed", HostTypeSchema::Int)],
            HostTypeSchema::Int,
        ));
        let catalog_b = b.build().expect("valid");

        assert_ne!(catalog_a.fingerprint(), catalog_b.fingerprint());
    }

    #[test]
    fn return_type_change_alters_fingerprint() {
        // Two catalogs whose only difference is a return type must have
        // distinct fingerprints.
        let mut a = HostApiCatalog::builder();
        a.function(HostFunctionSchema::with_return(
            "convert",
            vec![HostParamSchema::value("value", HostTypeSchema::Int)],
            HostTypeSchema::Int,
        ));
        let catalog_a = a.build().expect("valid");

        let mut b = HostApiCatalog::builder();
        b.function(HostFunctionSchema::with_return(
            "convert",
            vec![HostParamSchema::value("value", HostTypeSchema::Int)],
            HostTypeSchema::String,
        ));
        let catalog_b = b.build().expect("valid");

        assert_ne!(catalog_a.fingerprint(), catalog_b.fingerprint());
    }

    #[test]
    fn passing_mode_change_alters_fingerprint() {
        let base = catalog_with_io_and_sqlite();

        let mut builder = HostApiCatalog::builder();
        builder.resource(io_file_resource());
        builder.resource(sqlite_connection_resource());
        builder.function(fn_io_open("docs"));
        builder.function(fn_io_read_all(HostParamPassing::TakeOwned));
        builder.function(fn_sqlite_open());
        let changed = builder.build().expect("valid");

        assert_ne!(base.fingerprint(), changed.fingerprint());
        assert_ne!(
            passing_tag(HostParamPassing::Borrow),
            passing_tag(HostParamPassing::BorrowMut)
        );
    }

    #[test]
    fn docs_change_does_not_alter_fingerprint() {
        let mut a = HostApiCatalog::builder();
        a.resource(io_file_resource());
        a.function(fn_io_open("first description"));
        a.function(fn_io_read_all(HostParamPassing::Borrow));
        let catalog_a = a.build().expect("valid");

        let mut b = HostApiCatalog::builder();
        b.resource(ResourceTypeSchema::new(
            io_file_key(),
            "completely different docs",
        ));
        b.function(fn_io_open("second description"));
        b.function(fn_io_read_all(HostParamPassing::Borrow));
        let catalog_b = b.build().expect("valid");

        assert_eq!(catalog_a.fingerprint(), catalog_b.fingerprint());
        assert_ne!(catalog_a, catalog_b);
    }

    #[test]
    fn fingerprint_is_stable() {
        let catalog = catalog_with_io_and_sqlite();
        assert_eq!(catalog.fingerprint(), catalog.fingerprint());
    }

    // --- Lookups ---

    #[test]
    fn lookup_function_and_resource() {
        let catalog = catalog_with_io_and_sqlite();

        let open = catalog.function("io::open").expect("io::open present");
        assert_eq!(open.params.len(), 2);
        assert_eq!(open.return_type, HostTypeSchema::Resource(io_file_key()));

        let read = catalog.function("io::read_all").expect("present");
        assert_eq!(read.params[0].passing, HostParamPassing::Borrow);

        let sqlite = catalog.function("sqlite::open").expect("present");
        assert_eq!(
            sqlite.return_type,
            HostTypeSchema::Resource(sqlite_connection_key())
        );

        assert!(catalog.resource("io.file").is_some());
        assert!(catalog.resource("sqlite.connection").is_some());
        assert!(catalog.has_resource(&io_file_key()));
        assert!(catalog.has_resource(&sqlite_connection_key()));
        assert!(catalog.resource("does.not.exist").is_none());
        assert!(catalog.function("io::nope").is_none());
    }

    // --- Serde / validating deserialization ---

    fn valid_catalog_json() -> serde_json::Value {
        json!({
            "resources": [{ "key": "io.file", "description": "file" }],
            "functions": [{
                "name": "io::read_all",
                "params": [
                    { "name": "handle", "ty": { "Resource": "io.file" }, "passing": "Borrow" }
                ],
                "return_type": "String",
                "description": ""
            }]
        })
    }

    #[test]
    fn serde_round_trip_valid_catalog() {
        let catalog: HostApiCatalog =
            serde_json::from_value(valid_catalog_json()).expect("valid JSON should deserialize");
        assert_eq!(catalog.fingerprint(), catalog.fingerprint());
        assert_eq!(catalog.functions_named("io::read_all").len(), 1);
    }

    #[test]
    fn serde_rejects_malformed_resource_key() {
        // A bare malformed key must fail ResourceTypeKey's own Deserialize.
        assert!(serde_json::from_str::<ResourceTypeKey>("\"bad key\"").is_err());
        assert!(serde_json::from_str::<ResourceTypeKey>("\"a..b\"").is_err());

        // And a malformed key hiding inside a catalog's resources must fail.
        let mut v = valid_catalog_json();
        v["resources"][0]["key"] = json!("has space");
        assert!(serde_json::from_value::<HostApiCatalog>(v).is_err());
    }

    #[test]
    fn serde_rejects_duplicate_overload() {
        // Same name, identical params, identical return -> duplicate overload.
        let mut v = valid_catalog_json();
        let dup = v["functions"][0].clone();
        v["functions"].as_array_mut().unwrap().push(dup);
        assert!(serde_json::from_value::<HostApiCatalog>(v).is_err());
    }

    #[test]
    fn serde_rejects_ambiguous_overload_by_arg_identity() {
        // The serde path runs the same validate_surface as the builder: two
        // functions sharing a name and argument type/passing sequence are
        // rejected even when only the return type differs.
        let hostile = r#"{
            "resources": [],
            "functions": [
                {
                    "name": "convert",
                    "params": [
                        { "name": "value", "ty": "Int", "passing": "Value" }
                    ],
                    "return_type": "Int",
                    "description": ""
                },
                {
                    "name": "convert",
                    "params": [
                        { "name": "value", "ty": "Int", "passing": "Value" }
                    ],
                    "return_type": "String",
                    "description": ""
                }
            ]
        }"#;
        assert!(serde_json::from_str::<HostApiCatalog>(hostile).is_err());
    }

    #[test]
    fn serde_rejects_undeclared_resource_reference() {
        let mut v = valid_catalog_json();
        v["functions"][0]["params"][0]["ty"] = json!({ "Resource": "missing.file" });
        assert!(serde_json::from_value::<HostApiCatalog>(v).is_err());
    }

    #[test]
    fn serde_rejects_invalid_passing_modes() {
        // Value on a resource-containing param.
        let mut v = valid_catalog_json();
        v["functions"][0]["params"][0]["passing"] = json!("Value");
        assert!(serde_json::from_value::<HostApiCatalog>(v).is_err());

        // A borrow on a non-resource (String) param.
        let mut v = valid_catalog_json();
        v["functions"][0]["params"][0]["ty"] = json!("String");
        v["functions"][0]["params"][0]["passing"] = json!("Borrow");
        assert!(serde_json::from_value::<HostApiCatalog>(v).is_err());
    }

    #[test]
    fn serde_rejects_invalid_function_name() {
        let mut v = valid_catalog_json();
        v["functions"][0]["name"] = json!("bad name");
        assert!(serde_json::from_value::<HostApiCatalog>(v).is_err());
    }

    #[test]
    fn serde_rejects_duplicate_parameter_name() {
        let mut v = valid_catalog_json();
        v["functions"][0]["params"] = json!([
            { "name": "x", "ty": "String", "passing": "Value" },
            { "name": "x", "ty": "Int", "passing": "Value" }
        ]);
        assert!(serde_json::from_value::<HostApiCatalog>(v).is_err());
    }

    #[test]
    fn fingerprint_serde_round_trip_and_value() {
        let fp = HostApiFingerprint(0xdead_beef);
        let s = serde_json::to_string(&fp).unwrap();
        assert_eq!(s, "3735928559"); // u64 numeric via transparent
        let back: HostApiFingerprint = serde_json::from_str(&s).unwrap();
        assert_eq!(back, fp);
        assert_eq!(back.as_u64(), fp.as_u64());
    }

    // --- Recursive complexity limits ---

    fn nested_array(depth: usize) -> HostTypeSchema {
        let mut schema = HostTypeSchema::Int;
        for _ in 0..depth {
            schema = HostTypeSchema::Array(Box::new(schema));
        }
        schema
    }

    fn catalog_with_return(return_type: HostTypeSchema) -> HostApiCatalog {
        let mut builder = HostApiCatalog::builder();
        builder.function(HostFunctionSchema::with_return(
            "limits::probe",
            Vec::new(),
            return_type,
        ));
        builder.build().expect("test catalog should build")
    }

    #[test]
    fn catalog_accepts_schema_depth_boundary_and_rejects_one_more_level() {
        // The root is depth one, so 63 wrappers plus the scalar occupy the
        // documented 64-node path boundary.
        assert_eq!(
            catalog_with_return(nested_array(MAX_HOST_SCHEMA_DEPTH - 1))
                .functions()
                .len(),
            1
        );
        let mut over_builder = HostApiCatalog::builder();
        over_builder.function(HostFunctionSchema::with_return(
            "limits::too_deep",
            Vec::new(),
            nested_array(MAX_HOST_SCHEMA_DEPTH),
        ));
        assert!(over_builder.build().is_err());
    }

    #[test]
    fn callable_property_budget_accepts_boundary_and_rejects_one_more() {
        const MAX_PROPERTIES: usize = MAX_HOST_SCHEMA_PROPERTIES;
        let boundary = (0..MAX_PROPERTIES).map(|_| HostTypeSchema::Int).collect();
        let mut boundary_builder = HostApiCatalog::builder();
        boundary_builder.function(HostFunctionSchema::with_return(
            "limits::properties",
            Vec::new(),
            HostTypeSchema::Callable {
                params: boundary,
                result: Box::new(HostTypeSchema::Int),
            },
        ));
        assert!(boundary_builder.build().is_ok());

        let over = (0..=MAX_PROPERTIES).map(|_| HostTypeSchema::Int).collect();
        let mut over_builder = HostApiCatalog::builder();
        over_builder.function(HostFunctionSchema::with_return(
            "limits::properties_over",
            Vec::new(),
            HostTypeSchema::Callable {
                params: over,
                result: Box::new(HostTypeSchema::Int),
            },
        ));
        assert!(over_builder.build().is_err());
    }

    #[test]
    fn catalog_parameter_budget_accepts_boundary_and_rejects_one_more() {
        const MAX_PARAMETERS: usize = MAX_HOST_CATALOG_PARAMETERS;
        let boundary = (0..MAX_PARAMETERS)
            .map(|index| HostParamSchema::value(format!("p{index}"), HostTypeSchema::Int))
            .collect();
        let mut boundary_builder = HostApiCatalog::builder();
        boundary_builder.function(HostFunctionSchema::with_return(
            "limits::parameters",
            boundary,
            HostTypeSchema::Int,
        ));
        assert!(boundary_builder.build().is_ok());

        let over = (0..=MAX_PARAMETERS)
            .map(|index| HostParamSchema::value(format!("p{index}"), HostTypeSchema::Int))
            .collect();
        let mut over_builder = HostApiCatalog::builder();
        over_builder.function(HostFunctionSchema::with_return(
            "limits::parameters_over",
            over,
            HostTypeSchema::Int,
        ));
        assert!(over_builder.build().is_err());
    }

    #[test]
    fn schema_node_budget_accepts_boundary_and_rejects_one_more() {
        let branch = || nested_array(MAX_HOST_SCHEMA_DEPTH - 2);
        let boundary = (0..260).map(|_| branch()).collect();
        let mut boundary_builder = HostApiCatalog::builder();
        boundary_builder.function(HostFunctionSchema::with_return(
            "limits::nodes",
            Vec::new(),
            HostTypeSchema::Callable {
                params: boundary,
                result: Box::new(HostTypeSchema::Int),
            },
        ));
        assert!(boundary_builder.build().is_ok());

        let over = (0..261).map(|_| branch()).collect();
        let mut over_builder = HostApiCatalog::builder();
        over_builder.function(HostFunctionSchema::with_return(
            "limits::nodes_over",
            Vec::new(),
            HostTypeSchema::Callable {
                params: over,
                result: Box::new(HostTypeSchema::Int),
            },
        ));
        assert!(over_builder.build().is_err());
    }

    #[test]
    fn invalid_schema_serialization_and_identity_are_bounded() {
        let schema = nested_array(MAX_HOST_SCHEMA_DEPTH);
        assert!(serde_json::to_string(&schema).is_err());
        let function = HostFunctionSchema::with_return("limits::invalid", Vec::new(), schema);
        let identity = function.identity_discriminator();
        assert!(identity.starts_with("invalid-host-schema:"));
        assert!(identity.len() < 256);
    }
    #[test]
    fn catalog_function_list_overflow_is_rejected_before_loading_entries() {
        let mut functions = String::new();
        functions.push('[');
        for index in 0..(MAX_HOST_CATALOG_FUNCTIONS + 1) {
            if index != 0 {
                functions.push(',');
            }
            functions.push_str(&format!(
                "{{\"name\":\"limits::overload{index}\",\"params\":[],\"return_type\":\"Int\",\"description\":\"\"}}"
            ));
        }
        functions.push(']');
        let json = format!("{{\"resources\":[],\"functions\":{functions}}}");
        assert!(serde_json::from_str::<HostApiCatalog>(&json).is_err());
    }

    #[test]
    fn deeply_nested_json_is_rejected_without_unwinding_the_process() {
        let depth = 1024;
        let mut nested = String::with_capacity(depth * 10 + 5);
        for _ in 0..depth {
            nested.push_str("{\"Array\":");
        }
        nested.push_str("\"Int\"");
        for _ in 0..depth {
            nested.push('}');
        }
        let raw = format!(
            "{{\"resources\":[],\"functions\":[{{\"name\":\"limits::json\",\"params\":[],\"return_type\":{nested},\"description\":\"\"}}]}}"
        );
        let result = std::panic::catch_unwind(|| serde_json::from_str::<HostApiCatalog>(&raw));
        assert!(result.is_ok(), "deep JSON must not panic");
        assert!(
            result.expect("deep JSON result").is_err(),
            "deep JSON must exceed the schema-depth limit"
        );
    }

    #[test]
    fn wide_callable_json_is_rejected_at_the_schema_boundary() {
        let params = (0..4097).map(|_| "\"Int\"").collect::<Vec<_>>().join(",");
        let raw = format!("{{\"Callable\":{{\"params\":[{params}],\"result\":\"Int\"}}}}");
        assert!(serde_json::from_str::<HostTypeSchema>(&raw).is_err());
    }

    #[test]
    fn deeply_nested_import_schema_json_is_rejected_before_identity_use() {
        let depth = 1024;
        let mut nested = String::with_capacity(depth * 10 + 5);
        for _ in 0..depth {
            nested.push_str("{\"Optional\":");
        }
        nested.push_str("\"Int\"");
        for _ in 0..depth {
            nested.push('}');
        }
        let raw = format!(
            "{{\"name\":\"limits::import\",\"params\":[],\"return_type\":{nested},\"fingerprint\":0}}"
        );
        let result = std::panic::catch_unwind(|| serde_json::from_str::<HostImportSchema>(&raw));
        assert!(result.is_ok(), "deep import JSON must not panic");
        assert!(
            result.expect("deep import result").is_err(),
            "deep import JSON must exceed the schema-depth limit"
        );
    }

    // --- helpers used by tests above ---

    fn len_overload(ty: HostTypeSchema) -> HostFunctionSchema {
        HostFunctionSchema::with_return(
            "len",
            vec![HostParamSchema::value("value", ty)],
            HostTypeSchema::Int,
        )
    }
}
