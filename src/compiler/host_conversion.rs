//! Compiler-owned bridge from the host-agnostic semantic model to the
//! compiler's inference [`TypeSchema`].
//!
//! This module owns the only direction of the host -> compiler schema
//! mapping. The root [`crate::host_api`] module deliberately does **not**
//! import anything from [`crate::compiler`]: it stays a standalone,
//! host-agnostic, serializable-friendly description of the functions and
//! resource types a host exposes. Translation into the compiler's inference
//! world is the compiler's responsibility, so it lives here.
//!
//! The public conversion API is the inherent method
//! [`crate::host_api::HostTypeSchema::to_compiler_schema`], provided by this
//! module. Later parser/compiler catalog integration calls it whenever it
//! needs the compiler's semantic view of a host signature.
//!
//! ## Mapping invariants
//!
//! * Every [`HostTypeSchema::Resource`] becomes the distinct nominal
//!   [`TypeSchema::Resource`] (via [`crate::host_api::ResourceTypeKey`])
//!   carrying the same shared key.
//! * No host schema is ever collapsed onto the structural
//!   [`TypeSchema::Named`] / [`TypeSchema::Map`] fallback.
//! * Compiler-irrelevant host details (parameter passing modes etc.) are not
//!   carried across; only the value shape is translated.

use crate::host_api::HostTypeSchema;

use super::TypeSchema;

impl HostTypeSchema {
    /// Maps this host schema onto the compiler's [`TypeSchema`], recursively
    /// via [`Self::to_compiler_schema`].
    ///
    /// This is the conversion boundary that later parser/compiler catalog
    /// integration calls when it needs the compiler's semantic view of a
    /// host signature. Every [`HostTypeSchema::Resource`] becomes the
    /// distinct nominal [`TypeSchema::Resource`] carrying the same shared
    /// [`ResourceTypeKey`]; no host schema is ever collapsed to a
    /// structural `Named`/`Map` fallback.
    pub fn to_compiler_schema(&self) -> TypeSchema {
        match self {
            HostTypeSchema::Unknown => TypeSchema::Unknown,
            HostTypeSchema::Null => TypeSchema::Null,
            HostTypeSchema::Int => TypeSchema::Int,
            HostTypeSchema::Float => TypeSchema::Float,
            HostTypeSchema::Number => TypeSchema::Number,
            HostTypeSchema::Bool => TypeSchema::Bool,
            HostTypeSchema::String => TypeSchema::String,
            HostTypeSchema::Bytes => TypeSchema::Bytes,
            HostTypeSchema::Array(inner) => TypeSchema::Array(Box::new(inner.to_compiler_schema())),
            HostTypeSchema::Map(inner) => TypeSchema::Map(Box::new(inner.to_compiler_schema())),
            HostTypeSchema::Object(fields) => TypeSchema::Object(
                fields
                    .iter()
                    .map(|(key, ty)| (key.clone(), ty.to_compiler_schema()))
                    .collect(),
            ),
            HostTypeSchema::Optional(inner) => {
                TypeSchema::Optional(Box::new(inner.to_compiler_schema()))
            }
            HostTypeSchema::Callable { params, result } => TypeSchema::Callable {
                params: params.iter().map(Self::to_compiler_schema).collect(),
                result: Box::new(result.to_compiler_schema()),
            },
            HostTypeSchema::Resource(key) => TypeSchema::Resource(key.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::TypeSchema;
    use crate::host_api::HostTypeSchema;
    use crate::host_api::ResourceTypeKey;

    fn io_file_key() -> ResourceTypeKey {
        ResourceTypeKey::new("io.file").expect("valid key")
    }

    fn sqlite_connection_key() -> ResourceTypeKey {
        ResourceTypeKey::new("sqlite.connection").expect("valid key")
    }

    #[test]
    fn to_compiler_schema_maps_resource_nominally() {
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
