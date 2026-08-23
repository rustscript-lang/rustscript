use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use super::{HostImportSchema, Program, Value, VmError, VmResult};

use super::error::HostImportBindingError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostError {
    message: &'static str,
}

impl HostError {
    pub const fn new(message: &'static str) -> Self {
        Self { message }
    }

    pub const fn message(self) -> &'static str {
        self.message
    }
}

pub type HostFunction<C> = fn(&mut C, &[Value]) -> Result<Option<Value>, HostError>;
pub type HostDispatcher<C> = fn(&mut C, &str, &[Value]) -> Result<Option<Value>, HostError>;

/// A statically registered host function binding.
///
/// Two kinds exist:
///
/// * **Schema-less** (legacy) — [`HostBinding::new`]: binds any import with
///   the same name and arity. This is the compatibility path for genuinely
///   legacy imports whose VMBC carries no exact schema (`HostImport.schema ==
///   None`).
/// * **Exact** — [`HostBinding::exact`]: binds only an import whose name,
///   arity and `HostImportSchema` (parameter labels, type schemas, passing
///   modes, return schema) and catalog fingerprint are all identical. An exact
///   import is never satisfied by a schema-less binding.
///
/// Exact bindings own their schema so equality is structural and includes the
/// fingerprint; the binding is `Clone` but deliberately not `Copy`.
#[derive(Clone, Debug)]
pub struct HostBinding<C> {
    name: &'static str,
    arity: u8,
    function: HostFunction<C>,
    schema: Option<HostImportSchema>,
}

impl<C> HostBinding<C> {
    /// Registers a legacy schema-less binding by name and arity.
    pub const fn new(name: &'static str, arity: u8, function: HostFunction<C>) -> Self {
        Self {
            name,
            arity,
            function,
            schema: None,
        }
    }

    /// Registers an exact-schema binding that only satisfies imports whose
    /// schema (including the catalog fingerprint) is identical.
    ///
    /// `arity` must equal `schema.params.len()`; a mismatch is rejected with
    /// [`HostImportBindingError::SchemaArityMismatch`] (mirroring std
    /// `HostFunctionRegistry::push_exact`), and a schema with more parameters
    /// than the `u8` import arity can address is rejected with
    /// [`HostImportBindingError::InvalidSchema`]. Validation happens before any
    /// binding is returned, so a caller can never construct an exact binding
    /// whose `arity` disagrees with its own schema.
    pub fn exact(
        name: &'static str,
        arity: u8,
        schema: HostImportSchema,
        function: HostFunction<C>,
    ) -> Result<Self, HostImportBindingError> {
        let params_len = u8::try_from(schema.params.len()).map_err(|_| {
            HostImportBindingError::InvalidSchema {
                import: String::from(name),
                reason: format!(
                    "schema declares {} parameters; at most 255 are addressable",
                    schema.params.len()
                ),
            }
        })?;
        if params_len != arity {
            return Err(HostImportBindingError::SchemaArityMismatch {
                import: String::from(name),
                expected: params_len,
                got: arity,
            });
        }
        Ok(Self {
            name,
            arity,
            function,
            schema: Some(schema),
        })
    }

    pub const fn name(&self) -> &'static str {
        self.name
    }

    pub const fn arity(&self) -> u8 {
        self.arity
    }

    /// The exact schema this binding requires, or `None` for a legacy
    /// schema-less binding.
    pub const fn schema(&self) -> Option<&HostImportSchema> {
        self.schema.as_ref()
    }
}

pub(crate) fn resolve_host_functions<C>(
    program: &Program,
    bindings: &[HostBinding<C>],
) -> VmResult<Vec<HostFunction<C>>> {
    let mut resolved = Vec::new();
    resolved
        .try_reserve_exact(program.imports().len())
        .map_err(|_| VmError::HostBindingCapacity)?;

    for import in program.imports() {
        let binding = match import.schema.as_ref() {
            Some(import_schema) => {
                // Deterministic key: name + exact schema. Collect *all* matches so a
                // duplicate registration is rejected instead of silently first-matched
                // (std registry `Duplicate` semantics). Overloads differ in schema, so
                // they remain distinguishable; only identical keys collide.
                let mut matches = bindings.iter().filter(|binding| {
                    binding.name == import.name && binding.schema.as_ref() == Some(import_schema)
                });
                let first = matches.next();
                let binding = match first {
                    None => {
                        return Err(VmError::HostImportBinding(
                            HostImportBindingError::MissingExact {
                                import: import.name.clone(),
                            },
                        ));
                    }
                    Some(binding) => {
                        if matches.next().is_some() {
                            return Err(VmError::HostImportBinding(
                                HostImportBindingError::Duplicate {
                                    import: import.name.clone(),
                                },
                            ));
                        }
                        binding
                    }
                };
                // Arity is derived from the schema's parameter count, exactly as std
                // `resolve_import` does; an independent caller-supplied arity is never
                // trusted on the exact path.
                let schema_params = u8::try_from(import_schema.params.len()).map_err(|_| {
                    VmError::HostImportBinding(HostImportBindingError::InvalidSchema {
                        import: import.name.clone(),
                        reason: format!(
                            "schema declares {} parameters; at most 255 are addressable",
                            import_schema.params.len()
                        ),
                    })
                })?;
                if schema_params != import.arity {
                    return Err(VmError::InvalidCallArity {
                        import: import.name.clone(),
                        expected: schema_params,
                        got: import.arity,
                    });
                }
                if import.return_type != import_schema.return_type.coarse_value_type() {
                    return Err(VmError::HostImportBinding(
                        HostImportBindingError::ReturnTypeMismatch {
                            import: import.name.clone(),
                            expected: import_schema.return_type.coarse_value_type(),
                            got: import.return_type,
                        },
                    ));
                }
                binding
            }
            None => {
                // Schema-less (legacy) imports bind by name against schema-less
                // bindings only, keyed by name + arity. Count the bindings that
                // match the import's arity: exactly one resolves deterministically;
                // more than one is an ambiguous (duplicate) registration; none
                // (but the name exists) is an arity mismatch, mirroring std's
                // by-name resolve.
                let mut first_arity = None;
                let mut matching_binding = None;
                let mut matching_count = 0usize;
                for candidate in bindings.iter() {
                    if candidate.name != import.name || candidate.schema.is_some() {
                        continue;
                    }
                    if first_arity.is_none() {
                        first_arity = Some(candidate.arity);
                    }
                    if candidate.arity == import.arity {
                        matching_count += 1;
                        if matching_binding.is_none() {
                            matching_binding = Some(candidate);
                        }
                    }
                }
                let Some(first_arity) = first_arity else {
                    return Err(VmError::UnboundImport(import.name.clone()));
                };
                match matching_count {
                    0 => {
                        return Err(VmError::InvalidCallArity {
                            import: import.name.clone(),
                            expected: first_arity,
                            got: import.arity,
                        });
                    }
                    1 => matching_binding.expect("count 1 implies a matching binding"),
                    _ => {
                        return Err(VmError::HostImportBinding(
                            HostImportBindingError::Duplicate {
                                import: import.name.clone(),
                            },
                        ));
                    }
                }
            }
        };
        resolved.push(binding.function);
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::String;
    use alloc::vec;

    use super::super::{
        HostApiFingerprint, HostImport, HostImportParam, HostParamPassing, ResourceTypeKey,
        TypeSchema, ValueType,
    };

    fn noop_host(_context: &mut (), _args: &[Value]) -> Result<Option<Value>, HostError> {
        Ok(None)
    }

    fn fingerprint(value: u64) -> HostApiFingerprint {
        HostApiFingerprint::from_wire(value)
    }

    fn int_param(name: &str) -> HostImportParam {
        HostImportParam {
            name: String::from(name),
            schema: TypeSchema::Int,
            passing: HostParamPassing::Value,
        }
    }

    /// Exact import schema with one `int` param and an `int` return.
    fn exact_schema(fp: HostApiFingerprint) -> HostImportSchema {
        HostImportSchema {
            params: vec![int_param("value")],
            return_type: TypeSchema::Int,
            fingerprint: fp,
        }
    }

    fn exact_import(name: &str, arity: u8, fp: HostApiFingerprint) -> HostImport {
        HostImport {
            name: String::from(name),
            arity,
            return_type: ValueType::Int,
            schema: Some(exact_schema(fp)),
        }
    }

    fn schema_less_import(name: &str, arity: u8) -> HostImport {
        HostImport {
            name: String::from(name),
            arity,
            return_type: ValueType::Unknown,
            schema: None,
        }
    }

    fn program_with_imports(imports: Vec<HostImport>) -> Program {
        Program::new(Vec::new(), Vec::new(), imports)
    }

    #[test]
    fn exact_import_binds_when_schema_and_fingerprint_match() {
        let fp = fingerprint(0xABCD);
        let program = program_with_imports(vec![exact_import("gpio_set", 1, fp)]);
        let bindings = [
            HostBinding::exact("gpio_set", 1, exact_schema(fp), noop_host)
                .expect("constructor validates arity against schema"),
        ];

        let resolved = resolve_host_functions(&program, &bindings)
            .expect("exact schema and fingerprint match should bind");
        assert_eq!(resolved.len(), 1);
    }

    #[test]
    fn exact_import_rejects_fingerprint_mismatch() {
        let program = program_with_imports(vec![exact_import("gpio_set", 1, fingerprint(1))]);
        let bindings = [
            HostBinding::exact("gpio_set", 1, exact_schema(fingerprint(2)), noop_host)
                .expect("constructor validates arity against schema"),
        ];

        assert!(matches!(
            resolve_host_functions(&program, &bindings),
            Err(VmError::HostImportBinding(HostImportBindingError::MissingExact {
                import
            })) if import == "gpio_set"
        ));
    }

    #[test]
    fn exact_import_rejects_param_schema_mismatch() {
        let program = program_with_imports(vec![exact_import("gpio_set", 1, fingerprint(7))]);
        let mut wrong_param = exact_schema(fingerprint(7));
        wrong_param.params[0].schema = TypeSchema::Float;
        let bindings = [HostBinding::exact("gpio_set", 1, wrong_param, noop_host)
            .expect("constructor validates arity")];

        assert!(matches!(
            resolve_host_functions(&program, &bindings),
            Err(VmError::HostImportBinding(HostImportBindingError::MissingExact {
                import
            })) if import == "gpio_set"
        ));
    }

    #[test]
    fn exact_import_rejects_passing_mode_mismatch() {
        let program = program_with_imports(vec![exact_import("gpio_set", 1, fingerprint(7))]);
        let mut wrong_passing = exact_schema(fingerprint(7));
        wrong_passing.params[0].passing = HostParamPassing::Borrow;
        let bindings = [HostBinding::exact("gpio_set", 1, wrong_passing, noop_host)
            .expect("constructor validates arity")];

        assert!(matches!(
            resolve_host_functions(&program, &bindings),
            Err(VmError::HostImportBinding(HostImportBindingError::MissingExact {
                import
            })) if import == "gpio_set"
        ));
    }

    #[test]
    fn exact_import_rejects_return_schema_mismatch() {
        let program = program_with_imports(vec![exact_import("gpio_set", 1, fingerprint(7))]);
        let mut wrong_return = exact_schema(fingerprint(7));
        wrong_return.return_type = TypeSchema::Float;
        let bindings = [HostBinding::exact("gpio_set", 1, wrong_return, noop_host)
            .expect("constructor validates arity")];

        assert!(matches!(
            resolve_host_functions(&program, &bindings),
            Err(VmError::HostImportBinding(HostImportBindingError::MissingExact {
                import
            })) if import == "gpio_set"
        ));
    }

    #[test]
    fn exact_import_rejects_resource_key_mismatch() {
        let fp = fingerprint(13);
        let mut import = exact_import("gpio_set", 1, fp);
        import.schema = Some(HostImportSchema {
            params: vec![HostImportParam {
                name: String::from("value"),
                schema: TypeSchema::Resource(
                    ResourceTypeKey::from_wire(String::from("io.file")).expect("valid key"),
                ),
                passing: HostParamPassing::Borrow,
            }],
            return_type: TypeSchema::Unknown,
            fingerprint: fp,
        });
        import.return_type = ValueType::Unknown;
        let program = program_with_imports(vec![import]);

        let mut wrong_key = exact_schema(fp);
        wrong_key.params[0] = HostImportParam {
            name: String::from("value"),
            schema: TypeSchema::Resource(
                ResourceTypeKey::from_wire(String::from("io.other")).expect("valid key"),
            ),
            passing: HostParamPassing::Borrow,
        };
        wrong_key.return_type = TypeSchema::Unknown;
        let bindings = [HostBinding::exact("gpio_set", 1, wrong_key, noop_host)
            .expect("constructor validates arity")];

        assert!(matches!(
            resolve_host_functions(&program, &bindings),
            Err(VmError::HostImportBinding(HostImportBindingError::MissingExact {
                import
            })) if import == "gpio_set"
        ));
    }

    #[test]
    fn exact_import_rejects_coarse_return_type_mismatch() {
        // An inconsistent program whose coarse return type disagrees with the
        // exact schema's coarse return type is rejected with the typed
        // `ReturnTypeMismatch`, mirroring the std VM's bind-time check.
        let mut import = exact_import("gpio_set", 1, fingerprint(17));
        import.return_type = ValueType::Float;
        let program = program_with_imports(vec![import]);
        let bindings =
            [
                HostBinding::exact("gpio_set", 1, exact_schema(fingerprint(17)), noop_host)
                    .expect("constructor validates arity against schema"),
            ];

        assert!(matches!(
            resolve_host_functions(&program, &bindings),
            Err(VmError::HostImportBinding(HostImportBindingError::ReturnTypeMismatch {
                import,
                expected: ValueType::Int,
                got: ValueType::Float,
            })) if import == "gpio_set"
        ));
    }

    #[test]
    fn schema_less_import_does_not_use_exact_only_bindings() {
        let program = program_with_imports(vec![schema_less_import("gpio_set", 1)]);
        let bindings = [
            HostBinding::exact("gpio_set", 1, exact_schema(fingerprint(7)), noop_host)
                .expect("constructor validates arity against schema"),
        ];

        assert!(matches!(
            resolve_host_functions(&program, &bindings),
            Err(VmError::UnboundImport(import)) if import == "gpio_set"
        ));
    }

    #[test]
    fn exact_import_rejects_name_mismatch() {
        let program = program_with_imports(vec![exact_import("gpio_set", 1, fingerprint(7))]);
        let bindings =
            [
                HostBinding::exact("other_name", 1, exact_schema(fingerprint(7)), noop_host)
                    .expect("constructor validates arity against schema"),
            ];

        assert!(matches!(
            resolve_host_functions(&program, &bindings),
            Err(VmError::HostImportBinding(HostImportBindingError::MissingExact {
                import
            })) if import == "gpio_set"
        ));
    }

    #[test]
    fn exact_import_rejects_arity_mismatch() {
        // The expected arity is derived from the schema's parameter count (mirroring
        // std `resolve_import`), not from an independent caller-supplied value.
        let program = program_with_imports(vec![exact_import("gpio_set", 2, fingerprint(7))]);
        let bindings = [
            HostBinding::exact("gpio_set", 1, exact_schema(fingerprint(7)), noop_host)
                .expect("constructor validates arity against schema"),
        ];

        assert!(matches!(
            resolve_host_functions(&program, &bindings),
            Err(VmError::InvalidCallArity {
                import,
                expected: 1,
                got: 2,
            }) if import == "gpio_set"
        ));
    }

    #[test]
    fn exact_binding_constructor_requires_arity_to_match_schema_params() {
        // `exact_schema` declares exactly one parameter; registering arity 2 must be
        // rejected with a typed `SchemaArityMismatch` (std `push_exact` semantics).
        let err = HostBinding::exact("gpio_set", 2, exact_schema(fingerprint(7)), noop_host)
            .expect_err("arity must equal schema parameter count");
        assert!(matches!(
            err,
            HostImportBindingError::SchemaArityMismatch {
                import,
                expected: 1,
                got: 2,
            } if import == "gpio_set"
        ));
    }

    #[test]
    fn exact_binding_constructor_accepts_arity_matching_schema_params() {
        let binding = HostBinding::exact("gpio_set", 1, exact_schema(fingerprint(7)), noop_host)
            .expect("arity 1 matches one-param schema");
        assert_eq!(binding.arity(), 1);
        assert!(binding.schema().is_some());
        assert_eq!(binding.schema().unwrap().params.len(), 1);
    }

    #[test]
    fn exact_binding_constructor_rejects_schema_with_too_many_params() {
        // More than 255 parameters cannot be addressed by a `u8` import arity, so the
        // construction rejects it (std `InvalidSchema`), preventing a silent truncation.
        let params = (0..256)
            .map(|index| int_param(&format!("p{index}")))
            .collect::<alloc::vec::Vec<_>>();
        let schema = HostImportSchema {
            params,
            return_type: TypeSchema::Int,
            fingerprint: fingerprint(7),
        };
        let err = HostBinding::exact("gpio_set", 1, schema, noop_host)
            .expect_err(">255-param schema must be rejected");
        assert!(matches!(
            err,
            HostImportBindingError::InvalidSchema { import, .. } if import == "gpio_set"
        ));
    }

    #[test]
    fn exact_import_never_falls_back_to_name_only_binding() {
        let program = program_with_imports(vec![exact_import("gpio_set", 1, fingerprint(7))]);
        let bindings = [HostBinding::new("gpio_set", 1, noop_host)];

        assert!(matches!(
            resolve_host_functions(&program, &bindings),
            Err(VmError::HostImportBinding(HostImportBindingError::MissingExact {
                import
            })) if import == "gpio_set"
        ));
    }

    #[test]
    fn schema_less_import_still_binds_by_name_and_arity() {
        let program = program_with_imports(vec![schema_less_import("gpio_set", 2)]);
        let bindings = [HostBinding::new("gpio_set", 2, noop_host)];

        let resolved = resolve_host_functions(&program, &bindings)
            .expect("schema-less import should bind by name and arity");
        assert_eq!(resolved.len(), 1);
    }

    #[test]
    fn overloaded_name_uses_exact_schema_to_disambiguate() {
        let fp = fingerprint(9);
        let mut first = exact_schema(fp);
        first.params[0].schema = TypeSchema::Int;
        let mut second = exact_schema(fp);
        second.params[0].schema = TypeSchema::Float;

        // Import selects the Float overload by schema; the Int overload and a
        // name-only binding are both present and must not be picked.
        let mut import = exact_import("gpio_set", 1, fp);
        import.schema = Some(second.clone());
        let program = program_with_imports(vec![import]);
        let bindings = [
            HostBinding::new("gpio_set", 1, noop_host),
            HostBinding::exact("gpio_set", 1, first, noop_host)
                .expect("constructor validates arity"),
            HostBinding::exact("gpio_set", 1, second, noop_host)
                .expect("constructor validates arity"),
        ];

        let resolved = resolve_host_functions(&program, &bindings)
            .expect("overload resolution should bind the exact schema");
        assert_eq!(resolved.len(), 1);
    }

    #[test]
    fn duplicate_exact_bindings_are_rejected() {
        // Two bindings with the same name and identical exact schema would resolve
        // order-dependently; the resolver rejects them deterministically instead of
        // silently first-matching (std registry `Duplicate` semantics).
        let fp = fingerprint(11);
        let program = program_with_imports(vec![exact_import("gpio_set", 1, fp)]);
        let bindings = [
            HostBinding::exact("gpio_set", 1, exact_schema(fp), noop_host)
                .expect("constructor validates arity"),
            HostBinding::exact("gpio_set", 1, exact_schema(fp), noop_host)
                .expect("constructor validates arity"),
        ];

        assert!(matches!(
            resolve_host_functions(&program, &bindings),
            Err(VmError::HostImportBinding(HostImportBindingError::Duplicate { import }))
                if import == "gpio_set"
        ));
    }

    #[test]
    fn duplicate_schema_less_bindings_are_rejected() {
        // The same name+arity registered twice as schema-less bindings is equally
        // order-dependent; the resolver must not silently first-match.
        let program = program_with_imports(vec![schema_less_import("gpio_set", 2)]);
        let bindings = [
            HostBinding::new("gpio_set", 2, noop_host),
            HostBinding::new("gpio_set", 2, noop_host),
        ];

        assert!(matches!(
            resolve_host_functions(&program, &bindings),
            Err(VmError::HostImportBinding(HostImportBindingError::Duplicate { import }))
                if import == "gpio_set"
        ));
    }

    #[test]
    fn schema_less_same_name_different_arity_is_not_a_duplicate() {
        // The schema-less key is name + arity: two bindings sharing a name but with
        // different arities are distinct and must each resolve, not collide.
        let program = program_with_imports(vec![
            schema_less_import("gpio_set", 1),
            schema_less_import("gpio_set", 2),
        ]);
        let bindings = [
            HostBinding::new("gpio_set", 1, noop_host),
            HostBinding::new("gpio_set", 2, noop_host),
        ];

        let resolved = resolve_host_functions(&program, &bindings)
            .expect("same-name different-arity schema-less bindings are not duplicates");
        assert_eq!(resolved.len(), 2);
    }

    #[test]
    fn schema_less_arity_mismatch_still_reports_invalid_call_arity() {
        // An import whose arity matches no same-name binding reports
        // `InvalidCallArity` (not `UnboundImport` and not `Duplicate`), preserving
        // the existing legacy error even when a different-arity binding is present.
        let program = program_with_imports(vec![schema_less_import("gpio_set", 2)]);
        let bindings = [HostBinding::new("gpio_set", 1, noop_host)];

        assert!(matches!(
            resolve_host_functions(&program, &bindings),
            Err(VmError::InvalidCallArity {
                import,
                expected: 1,
                got: 2,
            }) if import == "gpio_set"
        ));
    }

    #[test]
    fn distinct_schema_overloads_are_not_duplicates() {
        // Two exact bindings share a name but differ in schema: they are overloads,
        // not duplicates, and each resolves to the correct function.
        let fp = fingerprint(23);
        let mut int_schema = exact_schema(fp);
        int_schema.params[0].schema = TypeSchema::Int;
        let mut float_schema = exact_schema(fp);
        float_schema.params[0].schema = TypeSchema::Float;

        let mut import = exact_import("gpio_set", 1, fp);
        import.schema = Some(int_schema.clone());
        let program = program_with_imports(vec![import]);
        let bindings = [
            HostBinding::exact("gpio_set", 1, int_schema, noop_host)
                .expect("constructor validates arity"),
            HostBinding::exact("gpio_set", 1, float_schema, noop_host)
                .expect("constructor validates arity"),
        ];

        let resolved =
            resolve_host_functions(&program, &bindings).expect("distinct overloads should resolve");
        assert_eq!(resolved.len(), 1);
    }
}
