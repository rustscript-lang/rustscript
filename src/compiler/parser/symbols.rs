use crate::ValueType;

use super::*;

pub(super) fn is_virtual_host_namespace_spec(spec: &str) -> bool {
    if spec.contains('/') || spec.ends_with(".rss") {
        return false;
    }

    let mut chars = spec.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    is_ident_start(first) && chars.all(is_ident_continue)
}

#[cfg(feature = "edge-abi")]
fn abi_value_type_to_value_type(value: edge_abi::AbiValueType) -> ValueType {
    match value {
        edge_abi::AbiValueType::Unknown => ValueType::Unknown,
        edge_abi::AbiValueType::Null => ValueType::Null,
        edge_abi::AbiValueType::Int => ValueType::Int,
        edge_abi::AbiValueType::Float => ValueType::Float,
        edge_abi::AbiValueType::Bool => ValueType::Bool,
        edge_abi::AbiValueType::String => ValueType::String,
        edge_abi::AbiValueType::Bytes => ValueType::Bytes,
        edge_abi::AbiValueType::Array => ValueType::Array,
        edge_abi::AbiValueType::Map => ValueType::Map,
    }
}

fn known_host_return_type(name: &str) -> ValueType {
    edge_host_return_type(name)
        .or_else(|| {
            default_host_callable(name)
                .and_then(|callable| parse_host_return_value_type(callable.signature.return_type))
        })
        .unwrap_or(ValueType::Unknown)
}

#[cfg(feature = "edge-abi")]
fn edge_host_return_type(name: &str) -> Option<ValueType> {
    edge_abi::function_by_name(name)
        .map(|function| abi_value_type_to_value_type(function.return_type))
}

#[cfg(not(feature = "edge-abi"))]
fn edge_host_return_type(_name: &str) -> Option<ValueType> {
    None
}

fn known_host_return_schema(name: &str) -> Option<TypeSchema> {
    default_host_callable(name)
        .and_then(|callable| parse_host_return_schema(callable.signature.return_type))
}

fn parse_host_return_schema(spec: &str) -> Option<TypeSchema> {
    let spec = spec.trim();
    if spec.is_empty() || spec == "unknown" {
        return None;
    }
    if let Some(inner) = parse_optional_host_return_schema(spec) {
        return Some(inner);
    }
    parse_simple_host_return_schema(spec)
}

fn parse_host_return_value_type(spec: &str) -> Option<ValueType> {
    let spec = spec.trim();
    if spec.is_empty() || spec == "unknown" {
        return None;
    }
    if let Some(inner) = parse_optional_host_return_schema(spec) {
        return Some(inner.coarse_value_type());
    }
    match spec {
        "null" => Some(ValueType::Null),
        "int" => Some(ValueType::Int),
        "float" => Some(ValueType::Float),
        "number" => Some(ValueType::Unknown),
        "bool" => Some(ValueType::Bool),
        "string" => Some(ValueType::String),
        "bytes" => Some(ValueType::Bytes),
        "array" => Some(ValueType::Array),
        "map" => Some(ValueType::Map),
        _ => None,
    }
}

fn parse_optional_host_return_schema(spec: &str) -> Option<TypeSchema> {
    let parts = spec.split('|').map(str::trim).collect::<Vec<_>>();
    if parts.len() != 2 {
        return None;
    }
    let non_null = parts.iter().copied().find(|part| *part != "null")?;
    parts.contains(&"null").then_some(())?;
    parse_simple_host_return_schema(non_null).map(|schema| TypeSchema::Optional(Box::new(schema)))
}

fn parse_simple_host_return_schema(spec: &str) -> Option<TypeSchema> {
    match spec {
        "null" => Some(TypeSchema::Null),
        "int" => Some(TypeSchema::Int),
        "float" => Some(TypeSchema::Float),
        "number" => Some(TypeSchema::Number),
        "bool" => Some(TypeSchema::Bool),
        "string" => Some(TypeSchema::String),
        "bytes" => Some(TypeSchema::Bytes),
        "array" => Some(TypeSchema::Array(Box::new(TypeSchema::Unknown))),
        "map" => Some(TypeSchema::Map(Box::new(TypeSchema::Unknown))),
        _ => None,
    }
}

fn known_host_accepts_arity(name: &str, arity: u8) -> bool {
    #[cfg(feature = "edge-abi")]
    if let Some(function) = edge_abi::function_by_name(name) {
        return function.param_types.len() == usize::from(arity);
    }
    default_host_callable(name).is_some_and(|callable| {
        let required = callable
            .signature
            .params
            .iter()
            .take_while(|param| !param.optional)
            .count();
        required <= usize::from(arity) && usize::from(arity) <= callable.signature.params.len()
    })
}

impl Parser {
    pub(super) fn get_local(&mut self, name: &str) -> Result<LocalSlot, ParseError> {
        if let Some(current_scope) = self.closure_scopes.last()
            && let Some(&index) = current_scope.get(name)
        {
            return Ok(index);
        }

        if self.closure_scopes.len() > 1 {
            for scope in self.closure_scopes[..self.closure_scopes.len() - 1]
                .iter()
                .rev()
            {
                if let Some(&source_index) = scope.get(name) {
                    return self.capture_or_direct_local(name, source_index);
                }
            }
        }

        if let Some(source_index) = self.locals.get(name).copied() {
            return self.capture_or_direct_local(name, source_index);
        }

        Err(ParseError {
            span: None,
            code: None,
            line: self.current_line(),
            message: format!("unknown local '{name}'"),
        })
    }

    pub(super) fn capture_or_direct_local(
        &mut self,
        name: &str,
        source_index: LocalSlot,
    ) -> Result<LocalSlot, ParseError> {
        if let Some(capture_idx) = self.closure_capture_contexts.len().checked_sub(1) {
            if let Some(&captured_slot) =
                self.closure_capture_contexts[capture_idx].by_name.get(name)
            {
                return Ok(captured_slot);
            }
            let captured_slot = self.allocate_hidden_local()?;
            let source_mutable = self.is_local_slot_mutable(source_index);
            self.set_local_slot_mutable(captured_slot, source_mutable);
            self.closure_capture_contexts[capture_idx]
                .by_name
                .insert(name.to_string(), captured_slot);
            self.closure_capture_contexts[capture_idx]
                .capture_copies
                .push((source_index, captured_slot));
            return Ok(captured_slot);
        }
        Ok(source_index)
    }

    pub(super) fn has_local_binding(&self, name: &str) -> bool {
        for scope in self.closure_scopes.iter().rev() {
            if scope.contains_key(name) {
                return true;
            }
        }
        self.locals.contains_key(name)
    }

    pub(super) fn resolve_function_for_call(
        &mut self,
        name: &str,
        arg_count: usize,
    ) -> Result<FunctionDecl, ParseError> {
        if let Some(decl) = self.functions.get(name).cloned() {
            if decl.arity as usize != arg_count {
                return Err(ParseError {
                    span: None,
                    code: None,
                    line: self.current_line(),
                    message: format!("function '{name}' expects {} arguments", decl.arity),
                });
            }
            return Ok(decl);
        }

        if name == STDLIB_PRINT_NAME {
            let arg_arity = u8::try_from(arg_count).map_err(|_| ParseError {
                span: None,
                code: None,
                line: self.current_line(),
                message: "function arity too large".to_string(),
            })?;
            if arg_arity != STDLIB_PRINT_ARITY {
                return Err(ParseError {
                    span: None,
                    code: None,
                    line: self.current_line(),
                    message: format!(
                        "function '{STDLIB_PRINT_NAME}' expects {STDLIB_PRINT_ARITY} arguments"
                    ),
                });
            }
            return self.define_builtin_function(STDLIB_PRINT_NAME, STDLIB_PRINT_ARITY);
        }
        if self.allow_implicit_externs {
            let arity = u8::try_from(arg_count).map_err(|_| ParseError {
                span: None,
                code: None,
                line: self.current_line(),
                message: "function arity too large".to_string(),
            })?;
            return self.define_external_function(name, arity);
        }

        Err(ParseError {
            span: None,
            code: None,
            line: self.current_line(),
            message: format!("unknown function '{name}'"),
        })
    }

    pub(super) fn define_builtin_function(
        &mut self,
        name: &str,
        arity: u8,
    ) -> Result<FunctionDecl, ParseError> {
        if let Some(existing) = self.functions.get(name) {
            return Ok(existing.clone());
        }
        if self.locals.contains_key(name) {
            return Err(ParseError {
                span: None,
                code: None,
                line: self.current_line(),
                message: format!("name '{name}' already used by a local binding"),
            });
        }
        let index = self.next_function;
        self.next_function = self.next_function.checked_add(1).ok_or(ParseError {
            span: None,
            code: None,
            line: self.current_line(),
            message: "function index overflow".to_string(),
        })?;
        let decl = FunctionDecl {
            name: name.to_string(),
            arity,
            index,
            args: (0..arity).map(|idx| format!("arg{idx}")).collect(),
            arg_schemas: vec![None; usize::from(arity)],
            return_schema: None,
            type_params: Vec::new(),
            exported: true,
            return_type: ValueType::Unknown,
            symbol: None,
        };
        self.functions.insert(name.to_string(), decl.clone());
        self.function_list.push(decl.clone());
        Ok(decl)
    }

    pub(super) fn define_external_function(
        &mut self,
        name: &str,
        arity: u8,
    ) -> Result<FunctionDecl, ParseError> {
        if let Some(existing) = self.functions.get(name) {
            if existing.arity != arity {
                return Err(ParseError {
                    span: None,
                    code: None,
                    line: self.current_line(),
                    message: format!("function '{name}' expects {} arguments", existing.arity),
                });
            }
            return Ok(existing.clone());
        }
        if self.locals.contains_key(name) {
            return Err(ParseError {
                span: None,
                code: None,
                line: self.current_line(),
                message: format!("name '{name}' already used by a local binding"),
            });
        }
        // The module loader resolves (or rejects) every implicit extern's
        // call sites; the marker keeps synthetic externs out of module
        // declaration/export tables.
        self.implicit_extern_names.insert(name.to_string());
        let index = self.next_function;
        self.next_function = self.next_function.checked_add(1).ok_or(ParseError {
            span: None,
            code: None,
            line: self.current_line(),
            message: "function index overflow".to_string(),
        })?;
        let args = (0..arity).map(|idx| format!("arg{idx}")).collect();
        let decl = FunctionDecl {
            name: name.to_string(),
            arity,
            index,
            args,
            arg_schemas: vec![None; usize::from(arity)],
            return_schema: None,
            type_params: Vec::new(),
            exported: true,
            return_type: ValueType::Unknown,
            symbol: None,
        };
        self.functions.insert(name.to_string(), decl.clone());
        self.function_list.push(decl.clone());
        Ok(decl)
    }

    pub(super) fn define_host_function(
        &mut self,
        name: &str,
        arity: u8,
    ) -> Result<FunctionDecl, ParseError> {
        // When a host catalog is present and declares this name, the catalog
        // is authoritative: resolve the exact-arity overload set from it and
        // never fall back to the static known-host table. The Arc snapshot is
        // cloned into an owned local so the candidate borrows are not tied to
        // `self`, letting the `&mut self` helper below run.
        let host_catalog = self.host_catalog.clone();
        if let Some(catalog) = host_catalog.as_ref() {
            let declared = catalog.functions_named(name);
            if !declared.is_empty() {
                return self.define_catalog_host_function(name, arity, declared);
            }
        }

        if let Some(existing) = self.functions.get(name) {
            if existing.arity != arity && !known_host_accepts_arity(name, arity) {
                return Err(ParseError {
                    span: None,
                    code: None,
                    line: self.current_line(),
                    message: format!("function '{name}' expects {} arguments", existing.arity),
                });
            }
            return Ok(existing.clone());
        }
        if self.locals.contains_key(name) {
            return Err(ParseError {
                span: None,
                code: None,
                line: self.current_line(),
                message: format!("name '{name}' already used by a local binding"),
            });
        }
        let index = self.next_function;
        self.next_function = self.next_function.checked_add(1).ok_or(ParseError {
            span: None,
            code: None,
            line: self.current_line(),
            message: "function index overflow".to_string(),
        })?;
        let args = (0..arity).map(|idx| format!("arg{idx}")).collect();
        let decl = FunctionDecl {
            name: name.to_string(),
            arity,
            index,
            args,
            arg_schemas: vec![None; usize::from(arity)],
            return_schema: known_host_return_schema(name),
            type_params: Vec::new(),
            exported: false,
            return_type: known_host_return_type(name),
            symbol: None,
        };
        self.functions.insert(name.to_string(), decl.clone());
        self.function_list.push(decl.clone());
        Ok(decl)
    }

    /// Resolves one host-call site against the authoritative catalog.
    ///
    /// `declared` is the catalog's full discovery-order list of functions
    /// registered under `name`. Only the exact-arity overloads become flat
    /// functions; each is recorded in the fingerprint-bound
    /// [`HostApiIrMetadata`] as the complete candidate set for its
    /// `(name, arity)` identity. Because the catalog must never destabilize
    /// user-declared, builtin or module identities, catalog flat functions are
    /// keyed separately by `(name, arity)` and are kept out of the name-only
    /// [`Parser::functions`] map.
    ///
    /// The produced [`FunctionDecl`] stays unresolved (candidate-level):
    /// generic argument names, no arg/return schemas, `ValueType::Unknown`
    /// and no preselection from candidate parameter types or return schema.
    fn define_catalog_host_function(
        &mut self,
        name: &str,
        arity: u8,
        declared: Vec<&HostFunctionSchema>,
    ) -> Result<FunctionDecl, ParseError> {
        // Exact-arity overloads, preserving catalog discovery (registration)
        // order. Pass-only variants are never deduplicated or reordered.
        let exact = declared
            .iter()
            .copied()
            .filter(|schema| schema.params.len() == usize::from(arity))
            .collect::<Vec<_>>();

        if exact.is_empty() {
            let mut arities = declared
                .iter()
                .map(|schema| schema.params.len())
                .collect::<Vec<_>>();
            arities.sort_unstable();
            arities.dedup();
            let arity_list = arities
                .iter()
                .map(|a| a.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(ParseError {
                span: None,
                code: None,
                line: self.current_line(),
                message: format!(
                    "host function '{name}' has no overload with {arity} argument(s); declared \
                     arities: {arity_list}"
                ),
            });
        }

        // Same `(name, arity)` reuses its flat declaration/index and records a
        // candidate set exactly once.
        let key = (name.to_string(), arity);
        if let Some(existing) = self.catalog_function_decls.get(&key) {
            return Ok(existing.clone());
        }
        if self.locals.contains_key(name) {
            return Err(ParseError {
                span: None,
                code: None,
                line: self.current_line(),
                message: format!("name '{name}' already used by a local binding"),
            });
        }

        // Prevalidate index capacity and the metadata record before committing
        // any externally observable function-list mutation, so a catalog or
        // record failure leaves function identity untouched.
        let index = self.next_function;
        let next = self.next_function.checked_add(1).ok_or(ParseError {
            span: None,
            code: None,
            line: self.current_line(),
            message: "function index overflow".to_string(),
        })?;
        let candidate_schemas = exact.into_iter().cloned().collect();
        self.record_host_candidate(index, candidate_schemas)?;

        let args = (0..arity).map(|idx| format!("arg{idx}")).collect();
        let decl = FunctionDecl {
            name: name.to_string(),
            arity,
            index,
            args,
            arg_schemas: vec![None; usize::from(arity)],
            return_schema: None,
            type_params: Vec::new(),
            exported: false,
            return_type: ValueType::Unknown,
            symbol: None,
        };
        self.next_function = next;
        self.catalog_function_decls.insert(key, decl.clone());
        self.function_list.push(decl.clone());
        Ok(decl)
    }

    /// Records a complete exact-arity candidate list for one flat function in
    /// the catalog metadata carrier. No-op when the carrier is absent.
    fn record_host_candidate(
        &mut self,
        index: u16,
        candidates: Vec<HostFunctionSchema>,
    ) -> Result<(), ParseError> {
        let Some(metadata) = &mut self.host_api_metadata else {
            return Ok(());
        };
        metadata.record_candidates(index, candidates)
    }

    pub(super) fn get_or_assign_local(
        &mut self,
        name: &str,
    ) -> Result<(LocalSlot, bool), ParseError> {
        if let Some(&index) = self.locals.get(name) {
            return Ok((index, false));
        }
        let index = self.allocate_hidden_local()?;
        self.locals.insert(name.to_string(), index);
        self.named_local_bindings.push((name.to_string(), index));
        Ok((index, true))
    }

    pub(super) fn predeclare_local(
        &mut self,
        binding: &ReplLocalBinding,
    ) -> Result<(), ParseError> {
        if self.locals.contains_key(&binding.name) {
            return Err(ParseError {
                span: None,
                code: None,
                line: 1,
                message: format!("duplicate repl local '{}'", binding.name),
            });
        }
        let index = self.allocate_hidden_local()?;
        self.locals.insert(binding.name.clone(), index);
        self.named_local_bindings
            .push((binding.name.clone(), index));
        self.set_local_slot_mutable(index, binding.mutable);
        Ok(())
    }

    pub(super) fn allocate_hidden_local(&mut self) -> Result<LocalSlot, ParseError> {
        let index = self.next_local;
        self.next_local = self.next_local.checked_add(1).ok_or(ParseError {
            span: None,
            code: None,
            line: self.current_line(),
            message: "local index overflow".to_string(),
        })?;
        self.mutable_locals.push(true);
        Ok(index)
    }
}

#[cfg(test)]
mod catalog_host_definition_tests {
    use std::sync::Arc;

    use crate::compiler::parser::ParserDialect;
    use crate::host_api::{
        HostApiBuilder, HostApiCatalog, HostFunctionSchema, HostParamPassing, HostParamSchema,
        HostTypeSchema, ResourceTypeKey, ResourceTypeSchema,
    };

    use super::*;

    struct ProbeDialect;
    impl ParserDialect for ProbeDialect {}
    static PROBE_DIALECT: ProbeDialect = ProbeDialect;

    fn catalog_with(
        resources: Vec<ResourceTypeSchema>,
        functions: Vec<HostFunctionSchema>,
    ) -> Arc<HostApiCatalog> {
        let mut builder = HostApiBuilder::new();
        for resource in resources {
            builder.resource(resource);
        }
        for function in functions {
            builder.function(function);
        }
        Arc::new(builder.build().expect("test catalog must be valid"))
    }

    fn function_with_arity(name: &str, arity: usize) -> HostFunctionSchema {
        HostFunctionSchema::new(
            name,
            (0..arity)
                .map(|i| HostParamSchema::value(format!("a{i}"), HostTypeSchema::Int))
                .collect(),
        )
    }

    fn parser_with(catalog: Arc<HostApiCatalog>) -> Parser {
        Parser::new_with_host_catalog("", 0, false, false, true, false, &PROBE_DIALECT, catalog)
            .expect("probe parser must construct")
    }

    #[test]
    fn catalog_without_source_declares_metadata_with_fingerprint() {
        let catalog = Arc::new(HostApiCatalog::builder().build().unwrap());
        let parser = parser_with(Arc::clone(&catalog));
        let metadata = parser.host_api_metadata().expect("metadata present");
        assert_eq!(metadata.fingerprint(), catalog.fingerprint());
        assert_eq!(metadata.function_indices().len(), 0);
    }

    #[test]
    fn same_name_distinct_arities_are_distinct_declarations_with_complete_candidate_sets() {
        let catalog = catalog_with(
            Vec::new(),
            vec![
                function_with_arity("pkg::f", 0),
                function_with_arity("pkg::f", 1),
            ],
        );
        let mut parser = parser_with(catalog);
        let arity0 = parser.define_host_function("pkg::f", 0).unwrap();
        let arity1 = parser.define_host_function("pkg::f", 1).unwrap();
        assert_ne!(
            arity0.index, arity1.index,
            "distinct arities need distinct indices"
        );
        assert_eq!(arity0.arity, 0);
        assert_eq!(arity1.arity, 1);
        assert_eq!(arity0.name, "pkg::f");
        assert_eq!(arity1.name, "pkg::f");
        // Candidate-level: unresolved schemas and unknown static return type.
        assert_eq!(arity0.return_type, ValueType::Unknown);
        assert_eq!(arity0.arg_schemas, Vec::<Option<TypeSchema>>::new());
        let metadata = parser.host_api_metadata().unwrap();
        assert_eq!(
            metadata.candidates(arity0.index).unwrap().len(),
            1,
            "arity-0 complete candidate set"
        );
        assert_eq!(
            metadata.candidates(arity1.index).unwrap().len(),
            1,
            "arity-1 complete candidate set"
        );
        let mut indices = metadata.function_indices().collect::<Vec<_>>();
        indices.sort_unstable();
        assert_eq!(indices, vec![arity0.index, arity1.index]);
        assert_eq!(parser.function_decls().len(), 2);
    }

    #[test]
    fn same_name_arity_reuses_index_and_records_once() {
        let catalog = catalog_with(Vec::new(), vec![function_with_arity("pkg::g", 1)]);
        let mut parser = parser_with(catalog);
        let first = parser.define_host_function("pkg::g", 1).unwrap();
        let second = parser.define_host_function("pkg::g", 1).unwrap();
        assert_eq!(
            first.index, second.index,
            "same (name,arity) reuses the index"
        );
        let metadata = parser.host_api_metadata().unwrap();
        assert_eq!(
            metadata.function_indices().collect::<Vec<_>>(),
            vec![first.index]
        );
        assert_eq!(metadata.candidates(first.index).unwrap().len(), 1);
        assert_eq!(parser.function_decls().len(), 1);
    }

    #[test]
    fn passing_only_overloads_keep_catalog_discovery_order() {
        let key = ResourceTypeKey::new("acme.file").unwrap();
        let resource = ResourceTypeSchema::new(key.clone(), "an acme file");
        let borrowed = HostFunctionSchema::new(
            "pkg::h",
            vec![HostParamSchema::with_passing(
                "f",
                HostTypeSchema::Resource(key.clone()),
                HostParamPassing::Borrow,
            )],
        );
        let mut_ = HostFunctionSchema::new(
            "pkg::h",
            vec![HostParamSchema::with_passing(
                "f",
                HostTypeSchema::Resource(key),
                HostParamPassing::BorrowMut,
            )],
        );
        let catalog = catalog_with(vec![resource], vec![borrowed, mut_]);
        let mut parser = parser_with(catalog);
        let decl = parser.define_host_function("pkg::h", 1).unwrap();
        let metadata = parser.host_api_metadata().unwrap();
        let candidates = metadata.candidates(decl.index).unwrap();
        assert_eq!(
            candidates.len(),
            2,
            "pass-only overloads are never deduplicated"
        );
        assert_eq!(candidates[0].params[0].passing, HostParamPassing::Borrow);
        assert_eq!(candidates[1].params[0].passing, HostParamPassing::BorrowMut);
    }

    #[test]
    fn wrong_arity_lists_sorted_distinct_arities_and_leaves_state_unchanged() {
        let catalog = catalog_with(
            Vec::new(),
            vec![
                function_with_arity("pkg::w", 1),
                function_with_arity("pkg::w", 5),
                function_with_arity("pkg::w", 3),
            ],
        );
        let mut parser = parser_with(catalog);
        let before_indices = parser
            .host_api_metadata()
            .unwrap()
            .function_indices()
            .count();
        let before_count = parser.function_decls().len();
        let err = parser
            .define_host_function("pkg::w", 2)
            .expect_err("wrong arity must be rejected");
        assert!(
            err.to_string().contains("declared arities: 1, 3, 5"),
            "unexpected error: {err}"
        );
        assert_eq!(
            parser.function_decls().len(),
            before_count,
            "function list unchanged"
        );
        assert_eq!(
            parser
                .host_api_metadata()
                .unwrap()
                .function_indices()
                .count(),
            before_indices,
            "metadata indices unchanged"
        );
        // A matching arity still resolves normally afterwards.
        let decl = parser.define_host_function("pkg::w", 3).unwrap();
        assert_eq!(parser.function_decls().len(), before_count + 1);
        assert!(
            !parser
                .host_api_metadata()
                .unwrap()
                .candidates(decl.index)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn absent_catalog_name_preserves_standard_host_behavior() {
        // Catalog only knows `pkg::a`; calling an undeclared host name must
        // keep the standard host resolution (a resolved host decl, no
        // candidate record, no schema preselection).
        let catalog = catalog_with(Vec::new(), vec![function_with_arity("pkg::a", 1)]);
        let mut parser = parser_with(catalog);
        let decl = parser.define_host_function("extra::x", 1).unwrap();
        assert_eq!(decl.name, "extra::x");
        // Legacy declarations produce a static-known untyped decl (no catalog
        // candidate recorded for it).
        assert_eq!(
            parser
                .host_api_metadata()
                .unwrap()
                .function_indices()
                .count(),
            0,
            "undeclared name must not record a candidate"
        );
        assert_eq!(parser.function_decls().len(), 1);
        // The catalog-declared name still resolves through the catalog.
        let catalog_decl = parser.define_host_function("pkg::a", 1).unwrap();
        assert_eq!(
            parser
                .host_api_metadata()
                .unwrap()
                .candidates(catalog_decl.index)
                .unwrap()
                .len(),
            1
        );
    }
}
