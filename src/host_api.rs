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

use serde::Deserialize;

use crate::compiler::TypeSchema;

/// Max byte length of a validated [`ResourceTypeKey`] name.
const MAX_RESOURCE_KEY_LEN: usize = 128;

/// Max byte length of a validated host function name.
const MAX_FUNCTION_NAME_LEN: usize = 128;

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

impl<'de> Deserialize<'de> for ResourceTypeKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let text = String::deserialize(deserializer)?;
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
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
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
        match self {
            Self::Resource(key) => Some(key),
            Self::Optional(inner) => inner.resource_key(),
            _ => None,
        }
    }

    /// Whether this schema references at least one resource, anywhere in the
    /// tree (direct, `Optional`, `Array`, `Map` value, or inside a `Callable`
    /// parameter/result).
    pub fn contains_resource(&self) -> bool {
        match self {
            Self::Resource(_) => true,
            Self::Array(inner) | Self::Map(inner) | Self::Optional(inner) => {
                inner.contains_resource()
            }
            Self::Callable { params, result } => {
                params.iter().any(|param| param.contains_resource()) || result.contains_resource()
            }
            Self::Unknown
            | Self::Null
            | Self::Int
            | Self::Float
            | Self::Number
            | Self::Bool
            | Self::String
            | Self::Bytes => false,
        }
    }

    /// Collects every resource key referenced anywhere in this schema tree.
    pub fn collect_resource_keys<'a>(&'a self, out: &mut Vec<&'a ResourceTypeKey>) {
        match self {
            Self::Resource(key) => out.push(key),
            Self::Array(inner) | Self::Map(inner) | Self::Optional(inner) => {
                inner.collect_resource_keys(out);
            }
            Self::Callable { params, result } => {
                for param in params {
                    param.collect_resource_keys(out);
                }
                result.collect_resource_keys(out);
            }
            Self::Unknown
            | Self::Null
            | Self::Int
            | Self::Float
            | Self::Number
            | Self::Bool
            | Self::String
            | Self::Bytes => {}
        }
    }

    /// Maps this host schema onto the compiler's [`TypeSchema`], recursively.
    ///
    /// This is the conversion boundary that later parser/compiler catalog
    /// integration will call when it needs the compiler's semantic view of a
    /// host signature. Every [`Self::Resource`] becomes the distinct nominal
    /// [`TypeSchema::Resource`] carrying the same shared [`ResourceTypeKey`];
    /// no host schema is ever collapsed to a structural `Named`/`Map` fallback.
    /// Compiler-irrelevant host details (parameter passing modes etc.) are not
    /// carried across; only the value shape is translated.
    pub fn to_compiler_schema(&self) -> TypeSchema {
        match self {
            Self::Unknown => TypeSchema::Unknown,
            Self::Null => TypeSchema::Null,
            Self::Int => TypeSchema::Int,
            Self::Float => TypeSchema::Float,
            Self::Number => TypeSchema::Number,
            Self::Bool => TypeSchema::Bool,
            Self::String => TypeSchema::String,
            Self::Bytes => TypeSchema::Bytes,
            Self::Array(inner) => TypeSchema::Array(Box::new(inner.to_compiler_schema())),
            Self::Map(inner) => TypeSchema::Map(Box::new(inner.to_compiler_schema())),
            Self::Optional(inner) => TypeSchema::Optional(Box::new(inner.to_compiler_schema())),
            Self::Callable { params, result } => TypeSchema::Callable {
                params: params.iter().map(Self::to_compiler_schema).collect(),
                result: Box::new(result.to_compiler_schema()),
            },
            Self::Resource(key) => TypeSchema::Resource(key.clone()),
        }
    }
}

impl fmt::Display for HostTypeSchema {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
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
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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

/// Semantic description of one host function parameter.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
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
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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

    /// Canonical semantic bytes for this function: name, then the parameter
    /// list (each parameter’s name, type and passing mode), then the return
    /// type. This is the full semantic encoding used by the catalog
    /// fingerprint, so any semantic change (including a parameter-label or
    /// return-type change) alters the digest. It is **not** used for overload
    /// identity — see [`Self::overload_identity_bytes`].
    fn semantic_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        push_len_str(&mut bytes, &self.name);
        push_len(&mut bytes, self.params.len());
        for param in &self.params {
            push_len_str(&mut bytes, &param.name);
            push_type(&mut bytes, &param.ty);
            push_tag(&mut bytes, passing_tag(param.passing));
        }
        push_type(&mut bytes, &self.return_type);
        bytes
    }

    /// Canonical overload-identity bytes: the function name plus the ordered
    /// parameter type schemas and passing modes only. Parameter names, the
    /// return schema and documentation are deliberately excluded, so two
    /// functions have the same identity precisely when their name and argument
    /// type/passing sequence match. Because argument shape is what dispatch
    /// and call sites resolve on, that identity being shared makes the
    /// overload set ambiguous regardless of labels or return type.
    ///
    /// This key feeds overload duplicate detection only — never the catalog
    /// fingerprint, which keeps using [`Self::semantic_bytes`].
    fn overload_identity_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        push_len_str(&mut bytes, &self.name);
        push_len(&mut bytes, self.params.len());
        for param in &self.params {
            push_type(&mut bytes, &param.ty);
            push_tag(&mut bytes, passing_tag(param.passing));
        }
        bytes
    }
}

/// Why a host function name is invalid.
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
        }
    }
}

impl std::error::Error for HostApiCatalogError {}

/// An immutable, validated catalog of the host API surface.
///
/// Construction is done via the builder ([`HostApiCatalog::builder`]) or via
/// serde; both routes run the same validation, so a catalog is only exposed
/// once all cross-references, passing-mode and name/overload invariants hold.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
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

/// Mirror of [`HostApiCatalog`]’s serialized shape so `Deserialize` can parse
/// it and then re-validate, keeping serde as safe as the builder.
#[derive(serde::Deserialize)]
struct HostApiCatalogRepr {
    resources: Vec<ResourceTypeSchema>,
    functions: Vec<HostFunctionSchema>,
}

impl<'de> Deserialize<'de> for HostApiCatalog {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let repr = HostApiCatalogRepr::deserialize(deserializer)?;
        let builder = HostApiBuilder {
            resources: repr.resources,
            functions: repr.functions,
        };
        builder.build().map_err(serde::de::Error::custom)
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

    /// Canonical semantic bytes for the whole catalog: `FINGERPRINT_DOMAIN_MAGIC`
    /// ++ `FINGERPRINT_FORMAT_VERSION` ++ resources (sorted by key) ++
    /// functions (sorted by full semantic signature bytes).
    fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();

        bytes.extend_from_slice(FINGERPRINT_DOMAIN_MAGIC);
        bytes.push(FINGERPRINT_FORMAT_VERSION);

        // Resources sorted by key text.
        let mut resources: Vec<&ResourceTypeSchema> = self.resources.iter().collect();
        resources.sort_by(|a, b| a.key.cmp(&b.key));
        push_tag(&mut bytes, b'R');
        push_len(&mut bytes, resources.len());
        for resource in &resources {
            push_len_str(&mut bytes, resource.key.as_str());
        }

        // Functions sorted by their full canonical semantic signature bytes so
        // overloaded registration order is irrelevant (exact duplicates are
        // already rejected at build time).
        let mut functions: Vec<&HostFunctionSchema> = self.functions.iter().collect();
        functions.sort_by_key(|a| a.semantic_bytes());
        push_tag(&mut bytes, b'F');
        push_len(&mut bytes, functions.len());
        for function in &functions {
            bytes.extend(function.semantic_bytes());
        }

        bytes
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
}

/// Validate the caller-supplied resource/function collections. Shared by the
/// builder and the serde path so both reject the same malformed inputs.
fn validate_surface(
    resources: &[ResourceTypeSchema],
    functions: &[HostFunctionSchema],
) -> Result<(), HostApiCatalogError> {
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
        // Valid function name.
        if let Err(reason) = validate_function_name(&function.name) {
            return Err(HostApiCatalogError::InvalidFunctionName {
                name: function.name.clone(),
                reason,
            });
        }

        // Unique parameter names.
        for (i, param) in function.params.iter().enumerate() {
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

        // Passing-mode and resource-reference invariants.
        for param in &function.params {
            let contains_resource = param.ty.contains_resource();
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

            // Every referenced resource key must be declared.
            let mut keys = Vec::new();
            param.ty.collect_resource_keys(&mut keys);
            for key in keys {
                if !resources.iter().any(|resource| &resource.key == key) {
                    return Err(HostApiCatalogError::UnknownResourceReference {
                        function: function.name.clone(),
                        key: key.clone(),
                    });
                }
            }
        }

        // Return references must be declared too.
        let mut keys = Vec::new();
        function.return_type.collect_resource_keys(&mut keys);
        for key in keys {
            if !resources.iter().any(|resource| &resource.key == key) {
                return Err(HostApiCatalogError::UnknownResourceReference {
                    function: function.name.clone(),
                    key: key.clone(),
                });
            }
        }
    }

    // Reject ambiguous overloads: two functions sharing a name and an identical
    // ordered argument type/passing sequence. Parameter names and the return
    // type do not disambiguate call sites, so same-name overloads that differ
    // only in labels or return schema are rejected. Legal overloads (same name,
    // distinct argument schema) are allowed.
    for (i, function) in functions.iter().enumerate() {
        let identity = function.overload_identity_bytes();
        for prior in &functions[..i] {
            if prior.overload_identity_bytes() == identity {
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

fn push_len(bytes: &mut Vec<u8>, value: usize) {
    // Fixed 8-byte little-endian length so encodings are unambiguous, and any
    // structural field write is order-independent in aggregate.
    bytes.extend_from_slice(&(value as u64).to_le_bytes());
}

fn push_len_str(bytes: &mut Vec<u8>, value: &str) {
    push_len(bytes, value.len());
    bytes.extend_from_slice(value.as_bytes());
}

fn push_type(bytes: &mut Vec<u8>, schema: &HostTypeSchema) {
    match schema {
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
            push_type(bytes, inner);
        }
        HostTypeSchema::Map(inner) => {
            push_tag(bytes, b'{');
            push_type(bytes, inner);
        }
        HostTypeSchema::Optional(inner) => {
            push_tag(bytes, b'?');
            push_type(bytes, inner);
        }
        HostTypeSchema::Callable { params, result } => {
            push_tag(bytes, b'c');
            push_len(bytes, params.len());
            for param in params {
                push_type(bytes, param);
            }
            push_type(bytes, result);
        }
        HostTypeSchema::Resource(key) => {
            push_tag(bytes, b'r');
            push_len_str(bytes, key.as_str());
        }
    }
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

    // --- helpers used by tests above ---

    fn len_overload(ty: HostTypeSchema) -> HostFunctionSchema {
        HostFunctionSchema::with_return(
            "len",
            vec![HostParamSchema::value("value", ty)],
            HostTypeSchema::Int,
        )
    }

    // --- host -> compiler schema mapping ---

    #[test]
    fn to_compiler_schema_maps_resource_nominally() {
        use crate::compiler::TypeSchema;
        let mapped = HostTypeSchema::Resource(sqlite_connection_key()).to_compiler_schema();
        // The shared key is preserved as a distinct nominal variant.
        assert_eq!(mapped, TypeSchema::Resource(sqlite_connection_key()));
        // It is NOT collapsed onto the structural `Named`/`Map` fallback.
        assert_ne!(
            mapped,
            TypeSchema::Named("sqlite.connection".to_string(), vec![])
        );
        assert_ne!(mapped, TypeSchema::Map(Box::new(TypeSchema::Unknown)));
        assert_eq!(mapped.resource_key(), Some(&sqlite_connection_key()));
    }

    #[test]
    fn to_compiler_schema_maps_nested_containers() {
        use crate::compiler::TypeSchema;

        let host = HostTypeSchema::Optional(Box::new(HostTypeSchema::Array(Box::new(
            HostTypeSchema::Map(Box::new(HostTypeSchema::Resource(io_file_key()))),
        ))));
        let mapped = host.to_compiler_schema();
        assert_eq!(
            mapped,
            TypeSchema::Optional(Box::new(TypeSchema::Array(Box::new(TypeSchema::Map(
                Box::new(TypeSchema::Resource(io_file_key()))
            )))))
        );
    }

    #[test]
    fn to_compiler_schema_maps_callable_with_resources() {
        use crate::compiler::TypeSchema;

        let host = HostTypeSchema::Callable {
            params: vec![
                HostTypeSchema::Resource(sqlite_connection_key()),
                HostTypeSchema::String,
            ],
            result: Box::new(HostTypeSchema::Resource(io_file_key())),
        };
        let mapped = host.to_compiler_schema();
        assert_eq!(
            mapped,
            TypeSchema::Callable {
                params: vec![
                    TypeSchema::Resource(sqlite_connection_key()),
                    TypeSchema::String,
                ],
                result: Box::new(TypeSchema::Resource(io_file_key())),
            }
        );
    }

    #[test]
    fn to_compiler_schema_scalars_are_direct() {
        use crate::compiler::TypeSchema;

        assert_eq!(
            HostTypeSchema::Unknown.to_compiler_schema(),
            TypeSchema::Unknown
        );
        assert_eq!(HostTypeSchema::Int.to_compiler_schema(), TypeSchema::Int);
        assert_eq!(
            HostTypeSchema::String.to_compiler_schema(),
            TypeSchema::String
        );
        assert_eq!(
            HostTypeSchema::Bytes.to_compiler_schema(),
            TypeSchema::Bytes
        );
    }
}
