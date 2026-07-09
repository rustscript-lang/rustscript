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
    edge_abi::function_by_name(name)
        .map(|function| abi_value_type_to_value_type(function.return_type))
        .or_else(|| {
            default_host_callable(name)
                .and_then(|callable| parse_host_return_value_type(callable.signature.return_type))
        })
        .unwrap_or(ValueType::Unknown)
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
    parts.iter().any(|part| *part == "null").then(|| ())?;
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
        };
        self.functions.insert(name.to_string(), decl.clone());
        self.function_list.push(decl.clone());
        Ok(decl)
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
