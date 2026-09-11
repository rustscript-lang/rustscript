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
//! * Every [`HostTypeSchema::Named`] becomes [`TypeSchema::Named`] (name
//!   identity, empty type args). Field shapes are available as
//!   [`TypeSchema::Object`] via [`HostTypeSchema::to_compiler_object_schema`].
//! * Dynamic [`HostTypeSchema::Map`] stays [`TypeSchema::Map`]. Resources are
//!   never collapsed onto Named/Map.
//! * Compiler-irrelevant host details (parameter passing modes etc.) are not
//!   carried across; only the value shape is translated.

use crate::host_api::{HostStructField, HostStructSchema, HostTypeSchema};

use super::{StructDecl, TypeSchema};

impl HostTypeSchema {
    /// Maps this host schema onto the compiler's [`TypeSchema`], recursively
    /// via [`Self::to_compiler_schema`].
    ///
    /// This is the conversion boundary that later parser/compiler catalog
    /// integration calls when it needs the compiler's semantic view of a
    /// host signature. Every [`HostTypeSchema::Resource`] becomes the
    /// distinct nominal [`TypeSchema::Resource`] carrying the same shared
    /// [`ResourceTypeKey`]. Named structs become [`TypeSchema::Named`];
    /// dynamic maps stay maps.
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
            HostTypeSchema::Optional(inner) => {
                TypeSchema::Optional(Box::new(inner.to_compiler_schema()))
            }
            HostTypeSchema::Callable { params, result } => TypeSchema::Callable {
                params: params.iter().map(Self::to_compiler_schema).collect(),
                result: Box::new(result.to_compiler_schema()),
            },
            HostTypeSchema::Resource(key) => TypeSchema::Resource(key.clone()),
            HostTypeSchema::Named { name, .. } => TypeSchema::Named(name.clone(), Vec::new()),
        }
    }

    /// Structural object schema used for field access and object-literal
    /// matching. Named structs expand to [`TypeSchema::Object`]; other
    /// variants match [`Self::to_compiler_schema`].
    pub fn to_compiler_object_schema(&self) -> TypeSchema {
        match self {
            HostTypeSchema::Named { fields, .. } => match_object_schema_from_fields(fields),
            HostTypeSchema::Array(inner) => {
                TypeSchema::Array(Box::new(inner.to_compiler_object_schema()))
            }
            HostTypeSchema::Map(inner) => {
                TypeSchema::Map(Box::new(inner.to_compiler_object_schema()))
            }
            HostTypeSchema::Optional(inner) => {
                TypeSchema::Optional(Box::new(inner.to_compiler_object_schema()))
            }
            HostTypeSchema::Callable { params, result } => TypeSchema::Callable {
                params: params.iter().map(Self::to_compiler_object_schema).collect(),
                result: Box::new(result.to_compiler_object_schema()),
            },
            other => other.to_compiler_schema(),
        }
    }
}

impl HostStructSchema {
    /// Object field schema for this catalog struct.
    pub fn to_compiler_object_schema(&self) -> TypeSchema {
        object_schema_from_fields(&self.fields)
    }

    /// Parser/type-checker struct declaration installed from the catalog.
    pub fn to_struct_decl(&self) -> StructDecl {
        StructDecl {
            name: self.name.clone(),
            type_params: Vec::new(),
            body_schema: self.to_compiler_object_schema(),
        }
    }
}

fn object_schema_from_fields(fields: &[HostStructField]) -> TypeSchema {
    TypeSchema::Object(
        fields
            .iter()
            .map(|field| (field.name.clone(), field.ty.to_compiler_schema()))
            .collect(),
    )
}

fn match_object_schema_from_fields(fields: &[HostStructField]) -> TypeSchema {
    TypeSchema::Object(
        fields
            .iter()
            .map(|field| (field.name.clone(), field.ty.to_compiler_object_schema()))
            .collect(),
    )
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

    #[test]
    fn to_compiler_schema_maps_named_struct_to_named_not_map() {
        let host = HostTypeSchema::named_struct(
            "Point",
            vec![
                crate::host_api::HostStructField::new("x", HostTypeSchema::Int),
                crate::host_api::HostStructField::new("y", HostTypeSchema::Int),
            ],
        );
        let mapped = host.to_compiler_schema();
        assert_eq!(mapped, TypeSchema::Named("Point".to_string(), vec![]));
        assert_ne!(mapped, TypeSchema::Map(Box::new(TypeSchema::Unknown)));
        let object = host.to_compiler_object_schema();
        match object {
            TypeSchema::Object(fields) => {
                assert_eq!(fields.get("x"), Some(&TypeSchema::Int));
                assert_eq!(fields.get("y"), Some(&TypeSchema::Int));
            }
            other => panic!("expected object schema, got {other:?}"),
        }
    }

    #[test]
    fn to_compiler_schema_preserves_nested_resource_in_named_struct() {
        let host = HostTypeSchema::named_struct(
            "HandleBox",
            vec![crate::host_api::HostStructField::new(
                "file",
                HostTypeSchema::Resource(io_file_key()),
            )],
        );
        let mapped = host.to_compiler_schema();
        assert_eq!(mapped, TypeSchema::Named("HandleBox".to_string(), vec![]));
        let object = host.to_compiler_object_schema();
        match object {
            TypeSchema::Object(fields) => {
                assert_eq!(
                    fields.get("file"),
                    Some(&TypeSchema::Resource(io_file_key()))
                );
            }
            other => panic!("expected object schema, got {other:?}"),
        }
    }
}
