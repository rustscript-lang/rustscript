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
//!   `Vec`) and derives [`serde::Serialize`] / [`serde::Deserialize`]. No
//!   lifetimes, no `&'static` slices, no [`std::any::TypeId`].
//! * **Validated construction.** A catalog cannot be built with duplicate
//!   resource keys, duplicate function names, undeclared referenced resource
//!   keys, or reference/ownership parameter modes on non-resource parameters.
//! * **Deterministic fingerprint.** [`HostApiCatalog::fingerprint`] produces a
//!   stable digest over *semantic* fields only. Documentation and registration
//!   order are excluded, so two catalogs assembled in different orders from the
//!   same semantic data hash identically.

use std::fmt;

/// Max byte length of a validated [`ResourceTypeKey`] name.
const MAX_RESOURCE_KEY_LEN: usize = 128;

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
/// `sqlite.connection`. Validation rejects empty, over-long, non-ASCII and
/// malformed-namespace names so the value can serve as a stable map key, a
/// fingerprint input and a serialized identifier without further laundering.
///
/// This deliberately replaces any reliance on [`std::any::TypeId`]: resource
/// identity is a value, not a type reflection.
#[derive(
    Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
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
    for segment in name.split('.') {
        if segment.is_empty() {
            let index = name.find('.').map(|dot| dot.saturating_sub(1)).unwrap_or(0);
            return Err(ResourceTypeKeyError::InvalidDotPlacement { index });
        }
    }
    Ok(())
}

/// How a host function receives a parameter.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum HostParamPassing {
    /// The parameter is a plain value; the callee may copy or drop it freely.
    Value,
    /// An immutable borrow of the argument value.
    Borrow,
    /// An exclusive mutable borrow of the argument value.
    BorrowMut,
    /// Ownership of the argument value is transferred to the callee.
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
    /// single optional layer) denotes a host resource.
    pub fn resource_key(&self) -> Option<&ResourceTypeKey> {
        match self {
            Self::Resource(key) => Some(key),
            Self::Optional(inner) => inner.resource_key(),
            _ => None,
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
            Self::Resource(key) => write!(f, "resource[{key}]"),
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
}

/// Errors produced while building a [`HostApiCatalog`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostApiCatalogError {
    DuplicateResourceKey(ResourceTypeKey),
    DuplicateFunctionName(String),
    UnknownResourceReference {
        function: String,
        key: ResourceTypeKey,
    },
    NonResourcePassingMode {
        function: String,
        parameter: String,
        passing: HostParamPassing,
    },
}

impl fmt::Display for HostApiCatalogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateResourceKey(key) => write!(f, "duplicate resource type key `{key}`"),
            Self::DuplicateFunctionName(name) => {
                write!(f, "duplicate host function name `{name}`")
            }
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
                "host function `{function}` uses passing mode {passing:?} on \
                 non-resource parameter `{parameter}`",
            ),
        }
    }
}

impl std::error::Error for HostApiCatalogError {}

/// An immutable, validated catalog of the host API surface.
///
/// Construction is done via the builder ([`HostApiCatalog::builder`]); a
/// catalog is only exposed once all cross-references and passing-mode
/// invariants hold.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HostApiCatalog {
    resources: Vec<ResourceTypeSchema>,
    functions: Vec<HostFunctionSchema>,
}

/// A deterministic 64-bit fingerprint of a [`HostApiCatalog`].
///
/// Computed by FNV-1a over a canonical encoding of the semantic fields only.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct HostApiFingerprint(u64);

impl HostApiFingerprint {
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

impl fmt::Display for HostApiFingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

/// Stage-one, mutable builder for a [`HostApiCatalog`].
///
/// Cross-function invariants (referenced resource keys being declared,
/// reference/ownership passing modes on non-resource types) are enforced in
/// [`HostApiBuilder::build`], which is what makes construction order
/// independent.
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

    /// Looks up a host function by exact name.
    pub fn function(&self, name: &str) -> Option<&HostFunctionSchema> {
        self.functions.iter().find(|function| function.name == name)
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

    /// Deterministic, order-independent fingerprint of the semantic contents.
    ///
    /// The fingerprint covers resource keys and every function's name,
    /// parameter (name, type, passing mode) and return type. It excludes
    /// documentation and registration order.
    pub fn fingerprint(&self) -> HostApiFingerprint {
        let mut bytes = Vec::new();

        // Resources sorted by key text.
        let mut resources: Vec<&ResourceTypeSchema> = self.resources.iter().collect();
        resources.sort_by(|a, b| a.key.cmp(&b.key));
        push_tag(&mut bytes, b'R');
        push_len(&mut bytes, resources.len());
        for resource in &resources {
            push_len_str(&mut bytes, resource.key.as_str());
        }

        // Functions sorted by name for order-independence.
        let mut functions: Vec<&HostFunctionSchema> = self.functions.iter().collect();
        functions.sort_by(|a, b| a.name.cmp(&b.name));
        push_tag(&mut bytes, b'F');
        push_len(&mut bytes, functions.len());
        for function in &functions {
            push_len_str(&mut bytes, &function.name);
            push_len(&mut bytes, function.params.len());
            for param in &function.params {
                push_len_str(&mut bytes, &param.name);
                push_type(&mut bytes, &param.ty);
                push_tag(&mut bytes, passing_tag(param.passing));
            }
            push_type(&mut bytes, &function.return_type);
        }

        HostApiFingerprint(fnv1a(&bytes))
    }
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

    /// Registers a host function signature.
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
        let resources = self.resources;
        let functions = self.functions;

        // Duplicate resource keys.
        for (i, resource) in resources.iter().enumerate() {
            if resources[..i].iter().any(|prior| prior.key == resource.key) {
                return Err(HostApiCatalogError::DuplicateResourceKey(
                    resource.key.clone(),
                ));
            }
        }

        // Duplicate function names.
        for (i, function) in functions.iter().enumerate() {
            if functions[..i]
                .iter()
                .any(|prior| prior.name == function.name)
            {
                return Err(HostApiCatalogError::DuplicateFunctionName(
                    function.name.clone(),
                ));
            }
        }

        // Per-function invariants.
        for function in &functions {
            for param in &function.params {
                // A reference/ownership passing mode requires a resource param.
                if param.passing.is_reference_mode() && param.ty.resource_key().is_none() {
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

        Ok(HostApiCatalog {
            resources,
            functions,
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

    fn fn_io_read_all() -> HostFunctionSchema {
        HostFunctionSchema::with_return(
            "io::read_all",
            vec![HostParamSchema::with_passing(
                "handle",
                HostTypeSchema::Resource(io_file_key()),
                HostParamPassing::Borrow,
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
        builder.function(fn_io_read_all());
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

    #[test]
    fn duplicate_function_name_rejected() {
        let mut builder = HostApiCatalog::builder();
        builder.resource(io_file_resource());
        builder.function(fn_io_open("one"));
        builder.function(fn_io_open("two"));
        assert_eq!(
            builder.build(),
            Err(HostApiCatalogError::DuplicateFunctionName(
                "io::open".to_string()
            ))
        );
    }

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
    fn undeclared_resource_in_param_rejected() {
        let mut builder = HostApiCatalog::builder();
        builder.function(HostFunctionSchema::with_return(
            "use_missing",
            vec![HostParamSchema::value(
                "h",
                HostTypeSchema::Resource(ResourceTypeKey::new("missing.file").unwrap()),
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
        // A map value referencing a never-declared key must be rejected even
        // though the map itself is not a resource.
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

    // --- Fingerprint semantics ---

    #[test]
    fn order_independent_fingerprint() {
        let mut builder_a = HostApiCatalog::builder();
        builder_a.resource(io_file_resource());
        builder_a.resource(sqlite_connection_resource());
        builder_a.function(fn_io_read_all());
        builder_a.function(fn_sqlite_open());
        builder_a.function(fn_io_open("docs"));
        let catalog_a = builder_a.build().expect("valid");

        let mut builder_b = HostApiCatalog::builder();
        builder_b.function(fn_io_open("other docs"));
        builder_b.resource(sqlite_connection_resource());
        builder_b.function(fn_sqlite_open());
        builder_b.resource(io_file_resource());
        builder_b.function(fn_io_read_all());
        let catalog_b = builder_b.build().expect("valid");

        assert_eq!(catalog_a.fingerprint(), catalog_b.fingerprint());
    }

    #[test]
    fn semantic_change_alters_fingerprint() {
        let base = catalog_with_io_and_sqlite();

        // Same resources; io::open now returns String instead of io.file.
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
        builder.function(fn_io_read_all());
        builder.function(fn_sqlite_open());
        let changed = builder.build().expect("valid");

        assert_ne!(base.fingerprint(), changed.fingerprint());
    }

    #[test]
    fn passing_mode_change_alters_fingerprint() {
        let base = catalog_with_io_and_sqlite();

        // io::read_all borrow changes to TakeOwned.
        let mut builder = HostApiCatalog::builder();
        builder.resource(io_file_resource());
        builder.resource(sqlite_connection_resource());
        builder.function(fn_io_open("docs"));
        builder.function(HostFunctionSchema::with_return(
            "io::read_all",
            vec![HostParamSchema::with_passing(
                "handle",
                HostTypeSchema::Resource(io_file_key()),
                HostParamPassing::TakeOwned,
            )],
            HostTypeSchema::String,
        ));
        builder.function(fn_sqlite_open());
        let changed = builder.build().expect("valid");

        assert_ne!(base.fingerprint(), changed.fingerprint());
    }

    #[test]
    fn borrow_mut_versus_borrow_differ() {
        assert_ne!(
            passing_tag(HostParamPassing::Borrow),
            passing_tag(HostParamPassing::BorrowMut)
        );
    }

    #[test]
    fn docs_change_does_not_alter_fingerprint() {
        let mut builder = HostApiCatalog::builder();
        builder.resource(io_file_resource());
        builder.function(fn_io_open("first description"));
        builder.function(fn_io_read_all());
        let catalog_a = builder.build().expect("valid");

        let mut builder = HostApiCatalog::builder();
        builder.resource(ResourceTypeSchema::new(
            io_file_key(),
            "completely different docs",
        ));
        builder.function(fn_io_open("second description"));
        builder.function(fn_io_read_all());
        let catalog_b = builder.build().expect("valid");

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
}
