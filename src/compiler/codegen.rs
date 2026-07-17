use std::collections::HashMap;

use crate::assembler::Assembler;
use crate::builtins::BuiltinFunction;
use crate::{
    CallableKind, CallablePrototype, CallableTarget, ExportedCallable, FunctionRegion, Program,
    RootCallableBinding, ScriptFunction, TypeMap, Value, ValueType,
};

use super::ir::{
    ClosureExpr, Expr, FunctionDecl, FunctionImpl, LocalSlot, MatchPattern, MatchTypePattern, Stmt,
    StructDecl, TypeSchema,
};
use super::{CompileError, TypingMode, typing};

pub struct Compiler {
    assembler: Assembler,
    next_label_id: u32,
    loop_stack: Vec<LoopContext>,
    function_impls: HashMap<u16, FunctionImpl>,
    function_decls: HashMap<u16, FunctionDecl>,
    struct_schemas: HashMap<String, StructDecl>,
    host_import_return_types: HashMap<u16, typing::BoundType>,
    host_import_signatures: HashMap<u16, typing::HostCallableSignature>,
    call_index_remap: HashMap<u16, u16>,

    callable_bindings: HashMap<LocalSlot, CallableBinding>,
    enable_local_move_semantics: bool,
    typing_mode: TypingMode,
    type_state: typing::LocalTypeState,
    type_map: TypeMap,
    root_local_count: usize,
    frame_local_count: usize,
    function_slots: HashMap<u16, LocalSlot>,
    specialized_function_slots: Vec<(u16, Vec<TypeSchema>, LocalSlot)>,
    function_prototype_ids: HashMap<u16, u32>,
    script_functions: Vec<ScriptFunction>,
    callable_prototypes: Vec<CallablePrototype>,
    function_regions: Vec<FunctionRegion>,
    root_callable_bindings: Vec<RootCallableBinding>,
    pending_closures: Vec<(u32, ClosureExpr)>,
    callable_prototype_bindings: HashMap<LocalSlot, u32>,
    closure_param_hints: HashMap<u32, Vec<(typing::BoundType, Option<TypeSchema>)>>,
}

struct LoopContext {
    continue_label: String,
    break_label: String,
}

#[derive(Clone)]
enum CallableBinding {
    Closure(ClosureExpr),
    Function(u16),
}

impl Default for Compiler {
    fn default() -> Self {
        Self::new()
    }
}

impl Compiler {
    pub fn new() -> Self {
        Self {
            assembler: Assembler::new(),
            next_label_id: 0,
            loop_stack: Vec::new(),
            function_impls: HashMap::new(),
            function_decls: HashMap::new(),
            struct_schemas: HashMap::new(),
            host_import_return_types: HashMap::new(),
            host_import_signatures: HashMap::new(),
            call_index_remap: HashMap::new(),

            callable_bindings: HashMap::new(),
            enable_local_move_semantics: false,
            typing_mode: TypingMode::DynamicHints,
            type_state: typing::LocalTypeState::default(),
            type_map: TypeMap::default(),
            root_local_count: 0,
            frame_local_count: 0,
            function_slots: HashMap::new(),
            specialized_function_slots: Vec::new(),
            function_prototype_ids: HashMap::new(),
            script_functions: Vec::new(),
            callable_prototypes: Vec::new(),
            function_regions: Vec::new(),
            root_callable_bindings: Vec::new(),
            pending_closures: Vec::new(),
            callable_prototype_bindings: HashMap::new(),
            closure_param_hints: HashMap::new(),
        }
    }

    pub fn set_source(&mut self, source: String) {
        self.assembler.set_source(source);
    }

    pub fn add_function_debug(&mut self, func: &FunctionDecl) {
        self.assembler
            .add_function(func.name.clone(), func.args.clone());
    }

    pub fn add_local_debug(
        &mut self,
        name: String,
        index: LocalSlot,
        declared_line: Option<u32>,
        last_line: Option<u32>,
    ) -> Result<(), CompileError> {
        self.assembler.add_local_with_range(
            name,
            local_slot_operand(index)?,
            declared_line,
            last_line,
        );
        Ok(())
    }

    pub fn set_root_local_count(&mut self, root_local_count: usize) {
        self.root_local_count = root_local_count;
    }

    pub fn set_function_impls(&mut self, function_impls: HashMap<u16, FunctionImpl>) {
        self.function_impls = function_impls;
    }

    pub fn set_function_decls(&mut self, function_decls: HashMap<u16, FunctionDecl>) {
        self.function_decls = function_decls;
    }

    pub fn set_struct_schemas(&mut self, struct_schemas: HashMap<String, StructDecl>) {
        self.struct_schemas = struct_schemas;
    }

    pub(crate) fn set_host_import_return_types(
        &mut self,
        host_import_return_types: HashMap<u16, typing::BoundType>,
    ) {
        self.host_import_return_types = host_import_return_types;
    }

    pub(crate) fn set_host_import_signatures(
        &mut self,
        host_import_signatures: HashMap<u16, typing::HostCallableSignature>,
    ) {
        self.host_import_signatures = host_import_signatures;
    }

    pub fn set_call_index_remap(&mut self, call_index_remap: HashMap<u16, u16>) {
        self.call_index_remap = call_index_remap;
    }

    pub fn set_enable_local_move_semantics(&mut self, enable_local_move_semantics: bool) {
        self.enable_local_move_semantics = enable_local_move_semantics;
    }

    pub(crate) fn set_typing_mode(&mut self, typing_mode: TypingMode) {
        self.typing_mode = typing_mode;
    }

    pub(crate) fn set_type_inference(&mut self, type_info: typing::TypeInferenceResult) {
        self.type_map.local_types = type_info.local_types;
        self.type_map.local_schemas = type_info.local_schemas;
        self.type_map.callable_slots = type_info.callable_slots;
        self.type_map.optional_slots = type_info.optional_slots;
    }

    pub fn compile_program(mut self, stmts: &[Stmt]) -> Result<Program, CompileError> {
        let named_functions = self.prepare_named_callables()?;
        self.compile_stmts(stmts)?;
        self.assembler.ret();
        let root_end = self.assembler.position();
        if root_end > 0 {
            self.function_regions.push(FunctionRegion {
                start_ip: 0,
                end_ip: root_end,
                prototype_id: None,
            });
        }

        for function_index in named_functions {
            self.compile_named_function_body(function_index)?;
        }
        let mut closure_index = 0usize;
        while closure_index < self.pending_closures.len() {
            let (prototype_id, closure) = self.pending_closures[closure_index].clone();
            self.compile_closure_body(prototype_id, &closure)?;
            closure_index += 1;
        }
        for prototype in &mut self.callable_prototypes {
            prototype.frame_local_count = self.frame_local_count;
        }
        let mut exported_callables = self
            .function_decls
            .values()
            .filter(|decl| decl.exported)
            .filter_map(|decl| {
                self.function_slots
                    .get(&decl.index)
                    .copied()
                    .map(|local_slot| ExportedCallable {
                        name: decl.name.clone(),
                        local_slot,
                    })
            })
            .collect::<Vec<_>>();
        exported_callables.sort_unstable_by(|lhs, rhs| lhs.name.cmp(&rhs.name));

        let mut program = self
            .assembler
            .finish_program()
            .map_err(CompileError::Assembler)?;
        self.type_map.strict_types = self.typing_mode.is_strict();
        program.type_map = Some(self.type_map);
        program.local_count = self.frame_local_count;
        program.script_functions = self.script_functions;
        program.callable_prototypes = self.callable_prototypes;
        program.function_regions = self.function_regions;
        program.root_callable_bindings = self.root_callable_bindings;
        program.exported_callables = exported_callables;
        Ok(program)
    }

    fn seed_frame_type_state_from_type_map(&mut self) {
        for index in 0..self.frame_local_count.min(usize::from(u16::MAX) + 1) {
            let Ok(slot) = LocalSlot::try_from(index) else {
                break;
            };
            let schema = self.type_map.local_schemas.get(index).cloned().flatten();
            let ty = schema
                .as_ref()
                .map(typing::bound_type_from_schema)
                .unwrap_or_else(|| {
                    self.type_map
                        .local_types
                        .get(index)
                        .copied()
                        .map(typing::BoundType::from)
                        .unwrap_or(typing::BoundType::Unknown)
                });
            self.type_state
                .set_with_schema_origin(slot, ty, schema, false);
        }
    }

    fn prepare_named_callables(&mut self) -> Result<Vec<u16>, CompileError> {
        let mut indices = self.function_impls.keys().copied().collect::<Vec<_>>();
        indices.sort_unstable();
        self.frame_local_count = self
            .root_local_count
            .checked_add(indices.len())
            .ok_or(CompileError::LocalSlotOverflow(LocalSlot::MAX))?;
        if self.frame_local_count > usize::from(u8::MAX) + 1 {
            return Err(CompileError::LocalSlotOverflow(LocalSlot::MAX));
        }

        for (position, function_index) in indices.iter().copied().enumerate() {
            let hidden_slot = LocalSlot::try_from(self.root_local_count + position)
                .map_err(|_| CompileError::LocalSlotOverflow(LocalSlot::MAX))?;
            let prototype_id = self.callable_prototypes.len() as u32;
            let script_function_id = self.script_functions.len() as u32 + position as u32;
            let function_impl = self
                .function_impls
                .get(&function_index)
                .expect("function index came from implementation map");
            let decl = self.function_decls.get(&function_index);
            self.function_slots.insert(function_index, hidden_slot);
            self.function_prototype_ids
                .insert(function_index, prototype_id);
            self.callable_prototypes.push(CallablePrototype {
                kind: if function_impl.capture_copies.is_empty() {
                    CallableKind::FunctionItem
                } else {
                    CallableKind::Closure
                },
                target: CallableTarget::ScriptFunction(script_function_id),
                arity: function_impl.param_slots.len() as u8,
                frame_local_count: self.frame_local_count,
                parameter_slots: function_impl.param_slots.clone(),
                capture_source_slots: function_impl
                    .capture_copies
                    .iter()
                    .map(|(source, _)| *source)
                    .collect(),
                capture_slots: function_impl
                    .capture_copies
                    .iter()
                    .map(|(_, target)| *target)
                    .collect(),
                capture_modes: function_impl
                    .capture_copies
                    .iter()
                    .map(|(_, target)| {
                        super::lifetime::function_capture_binding_mode(function_impl, *target)
                    })
                    .collect(),
                self_slot: Some(hidden_slot),
                schema: decl.map(|decl| TypeSchema::Callable {
                    params: decl
                        .arg_schemas
                        .iter()
                        .map(|schema| schema.clone().unwrap_or(TypeSchema::Unknown))
                        .collect(),
                    result: Box::new(decl.return_schema.clone().unwrap_or(TypeSchema::Unknown)),
                }),
            });
            if function_impl.capture_copies.is_empty() {
                self.root_callable_bindings.push(RootCallableBinding {
                    local_slot: hidden_slot,
                    prototype_id,
                });
            }
        }
        Ok(indices)
    }

    fn compile_named_function_body(&mut self, function_index: u16) -> Result<(), CompileError> {
        let function_impl = self
            .function_impls
            .get(&function_index)
            .cloned()
            .expect("prepared function implementation must exist");
        let prototype_id = self.function_prototype_ids[&function_index];
        let entry_ip = self.assembler.position();
        let callable_snapshot = self.callable_bindings.clone();
        let type_snapshot = self.type_state.clone();
        let loop_snapshot = std::mem::take(&mut self.loop_stack);
        self.seed_frame_type_state_from_type_map();
        if let Some(decl) = self.function_decls.get(&function_index) {
            for (slot, schema) in function_impl.param_slots.iter().zip(&decl.arg_schemas) {
                match schema {
                    Some(schema) => self.type_state.set_with_schema_origin(
                        *slot,
                        typing::bound_type_from_schema(schema),
                        Some(schema.clone()),
                        true,
                    ),
                    None => self.type_state.set_with_schema_origin(
                        *slot,
                        typing::BoundType::Unknown,
                        None,
                        false,
                    ),
                }
            }
        }
        self.compile_stmts(&function_impl.body_stmts)?;
        self.compile_expr(&function_impl.body_expr)?;
        self.assembler.ret();
        self.loop_stack = loop_snapshot;
        self.callable_bindings = callable_snapshot;
        self.type_state = type_snapshot;
        let end_ip = self.assembler.position();
        self.script_functions
            .push(ScriptFunction { entry_ip, end_ip });
        self.function_regions.push(FunctionRegion {
            start_ip: entry_ip,
            end_ip,
            prototype_id: Some(prototype_id),
        });
        Ok(())
    }

    fn compile_closure_body(
        &mut self,
        prototype_id: u32,
        closure: &ClosureExpr,
    ) -> Result<(), CompileError> {
        let entry_ip = self.assembler.position();
        let callable_snapshot = self.callable_bindings.clone();
        let type_snapshot = self.type_state.clone();
        let loop_snapshot = std::mem::take(&mut self.loop_stack);
        self.seed_frame_type_state_from_type_map();
        if let Some(hints) = self.closure_param_hints.get(&prototype_id).cloned() {
            for (slot, (ty, schema)) in closure.param_slots.iter().zip(hints) {
                self.type_state
                    .set_with_schema_origin(*slot, ty, schema, false);
            }
        }
        self.compile_expr(&closure.body)?;
        self.assembler.ret();
        self.loop_stack = loop_snapshot;
        self.callable_bindings = callable_snapshot;
        self.type_state = type_snapshot;
        let end_ip = self.assembler.position();
        let function_id = self.script_functions.len() as u32;
        self.script_functions
            .push(ScriptFunction { entry_ip, end_ip });
        self.callable_prototypes[prototype_id as usize].target =
            CallableTarget::ScriptFunction(function_id);
        self.function_regions.push(FunctionRegion {
            start_ip: entry_ip,
            end_ip,
            prototype_id: Some(prototype_id),
        });
        Ok(())
    }

    fn compile_stmts(&mut self, stmts: &[Stmt]) -> Result<(), CompileError> {
        for stmt in stmts {
            self.compile_stmt(stmt)?;
        }
        Ok(())
    }

    fn compile_stmt(&mut self, stmt: &Stmt) -> Result<(), CompileError> {
        match stmt {
            Stmt::Noop { line } => {
                self.assembler.mark_line(*line);
            }
            Stmt::Let {
                index,
                declared_schema,
                expr,
                line,
            } => {
                self.assembler.mark_line(*line);
                self.assign_expr_to_slot(*index, declared_schema.as_ref(), expr)?;
            }
            Stmt::Assign {
                index, expr, line, ..
            } => {
                self.assembler.mark_line(*line);
                self.assign_expr_to_slot(*index, None, expr)?;
            }
            Stmt::ClosureLet { line, .. } => {
                self.assembler.mark_line(*line);
            }
            Stmt::FuncDecl {
                index,
                has_impl,
                line,
                ..
            } => {
                self.assembler.mark_line(*line);
                if *has_impl {
                    self.emit_named_callable_binding(*index)?;
                }
            }
            Stmt::Expr { expr, line } => {
                self.assembler.mark_line(*line);
                self.compile_expr(expr)?;
            }
            Stmt::IfElse {
                condition,
                then_branch,
                else_branch,
                line,
            } => {
                self.assembler.mark_line(*line);
                if matches!(condition, Expr::Bool(true)) {
                    self.compile_stmts(then_branch)?;
                    return Ok(());
                }
                if matches!(condition, Expr::Bool(false)) {
                    self.compile_stmts(else_branch)?;
                    return Ok(());
                }
                let callable_snapshot = self.callable_bindings.clone();
                let type_state_snapshot = self.type_state.clone();
                let then_refined_type_state =
                    typing::refine_state_for_condition(&type_state_snapshot, condition, true);
                let else_refined_type_state =
                    typing::refine_state_for_condition(&type_state_snapshot, condition, false);
                let else_label = self.fresh_label("else");
                let end_label = self.fresh_label("endif");
                self.compile_scalar_expr(condition)?;
                self.assembler.brfalse_label(&else_label);
                self.type_state = then_refined_type_state;
                self.compile_stmts(then_branch)?;
                let then_type_state = self.type_state.clone();
                self.assembler.br_label(&end_label);
                self.assembler
                    .label(&else_label)
                    .map_err(CompileError::Assembler)?;
                self.callable_bindings = callable_snapshot.clone();
                self.type_state = else_refined_type_state;
                self.compile_stmts(else_branch)?;
                let else_type_state = self.type_state.clone();
                self.assembler
                    .label(&end_label)
                    .map_err(CompileError::Assembler)?;
                self.callable_bindings = callable_snapshot;
                self.type_state
                    .merge_from_branches(&then_type_state, &else_type_state);
            }
            Stmt::For {
                init,
                condition,
                post,
                body,
                line,
            } => {
                let callable_snapshot = self.callable_bindings.clone();
                self.assembler.mark_line(*line);
                self.compile_stmt(init)?;
                let loop_entry_type_state = self.type_state.clone();
                let stabilized_loop_type_state =
                    self.stabilize_loop_type_state(&loop_entry_type_state, |iterated| {
                        let _ = typing::infer_expr_type_with_function_impls_and_imports(
                            condition,
                            iterated,
                            &self.function_impls,
                            &self.function_decls,
                            &self.struct_schemas,
                            &self.host_import_return_types,
                            &self.host_import_signatures,
                        );
                        typing::apply_stmts_with_function_impls_and_imports(
                            body,
                            iterated,
                            &self.function_impls,
                            &self.function_decls,
                            &self.struct_schemas,
                            &self.host_import_return_types,
                            &self.host_import_signatures,
                        );
                        typing::apply_stmts_with_function_impls_and_imports(
                            std::slice::from_ref(post),
                            iterated,
                            &self.function_impls,
                            &self.function_decls,
                            &self.struct_schemas,
                            &self.host_import_return_types,
                            &self.host_import_signatures,
                        );
                    });
                self.type_state = stabilized_loop_type_state;
                let start_label = self.fresh_label("for_start");
                let continue_label = self.fresh_label("for_continue");
                let end_label = self.fresh_label("for_end");
                self.assembler
                    .label(&start_label)
                    .map_err(CompileError::Assembler)?;
                self.compile_scalar_expr(condition)?;
                self.assembler.brfalse_label(&end_label);
                self.loop_stack.push(LoopContext {
                    continue_label: continue_label.clone(),
                    break_label: end_label.clone(),
                });
                self.compile_stmts(body)?;
                self.loop_stack.pop();
                self.assembler
                    .label(&continue_label)
                    .map_err(CompileError::Assembler)?;
                self.compile_stmt(post)?;
                self.assembler.br_label(&start_label);
                self.assembler
                    .label(&end_label)
                    .map_err(CompileError::Assembler)?;
                self.callable_bindings = callable_snapshot;
                self.type_state = self
                    .simulate_stmt_type_state(std::slice::from_ref(stmt), &loop_entry_type_state);
            }
            Stmt::While {
                condition,
                body,
                line,
            } => {
                let callable_snapshot = self.callable_bindings.clone();
                let loop_entry_type_state = self.type_state.clone();
                let stabilized_loop_type_state =
                    self.stabilize_loop_type_state(&loop_entry_type_state, |iterated| {
                        let _ = typing::infer_expr_type_with_function_impls_and_imports(
                            condition,
                            iterated,
                            &self.function_impls,
                            &self.function_decls,
                            &self.struct_schemas,
                            &self.host_import_return_types,
                            &self.host_import_signatures,
                        );
                        typing::apply_stmts_with_function_impls_and_imports(
                            body,
                            iterated,
                            &self.function_impls,
                            &self.function_decls,
                            &self.struct_schemas,
                            &self.host_import_return_types,
                            &self.host_import_signatures,
                        );
                    });
                self.assembler.mark_line(*line);
                self.type_state = stabilized_loop_type_state;
                let start_label = self.fresh_label("while_start");
                let end_label = self.fresh_label("while_end");
                self.assembler
                    .label(&start_label)
                    .map_err(CompileError::Assembler)?;
                self.compile_scalar_expr(condition)?;
                self.assembler.brfalse_label(&end_label);
                self.loop_stack.push(LoopContext {
                    continue_label: start_label.clone(),
                    break_label: end_label.clone(),
                });
                self.compile_stmts(body)?;
                self.loop_stack.pop();
                self.assembler.br_label(&start_label);
                self.assembler
                    .label(&end_label)
                    .map_err(CompileError::Assembler)?;
                self.callable_bindings = callable_snapshot;
                self.type_state = self
                    .simulate_stmt_type_state(std::slice::from_ref(stmt), &loop_entry_type_state);
            }
            Stmt::Break { line } => {
                self.assembler.mark_line(*line);
                let loop_ctx = self
                    .loop_stack
                    .last()
                    .ok_or(CompileError::BreakOutsideLoop)?;
                self.assembler.br_label(&loop_ctx.break_label);
            }
            Stmt::Continue { line } => {
                self.assembler.mark_line(*line);
                let loop_ctx = self
                    .loop_stack
                    .last()
                    .ok_or(CompileError::ContinueOutsideLoop)?;
                self.assembler.br_label(&loop_ctx.continue_label);
            }
            Stmt::Drop { index, line } => {
                self.assembler.mark_line(*line);
                self.assign_expr_to_slot(*index, None, &Expr::Null)?;
            }
        }
        Ok(())
    }

    fn compile_expr(&mut self, expr: &Expr) -> Result<(), CompileError> {
        match expr {
            Expr::Null => {
                self.assembler.push_const(Value::Null);
            }
            Expr::Int(value) => {
                self.assembler.push_const(Value::Int(*value));
            }
            Expr::Float(value) => {
                self.assembler.push_const(Value::Float(*value));
            }
            Expr::Bool(value) => {
                self.assembler.push_const(Value::Bool(*value));
            }
            Expr::String(value) => {
                self.assembler.push_const(Value::string(value.clone()));
            }
            Expr::Bytes(value) => {
                self.assembler.push_const(Value::bytes(value.clone()));
            }
            Expr::OptionalGet {
                container,
                key,
                container_slot,
                key_slot,
            } => {
                self.compile_optional_get_expr(container, key, *container_slot, *key_slot)?;
            }
            Expr::OptionUnwrapOr {
                value,
                value_slot,
                fallback,
            } => {
                self.compile_option_unwrap_or_expr(value, *value_slot, fallback)?;
            }
            Expr::FunctionRef(index, type_args) => {
                let slot = self.ensure_function_value_slot(*index, type_args)?;
                self.emit_copy_ldloc(slot)?;
            }
            Expr::Call(index, _, args) => {
                self.compile_function_call(*index, args)?;
            }
            Expr::Closure(closure) => {
                let _ = self.emit_closure_callable(closure)?;
            }
            Expr::ClosureCall(closure, args) => {
                let prototype_id = self.emit_closure_callable(closure)?;
                self.record_closure_param_hints(prototype_id, args);
                self.compile_callvalue_args(args)?;
            }
            Expr::LocalCall(index, _, args) => {
                if let Some(prototype_id) = self.callable_prototype_bindings.get(index).copied() {
                    self.record_closure_param_hints(prototype_id, args);
                }
                self.emit_copy_ldloc(*index)?;
                self.compile_callvalue_args(args)?;
            }
            Expr::Add(lhs, rhs) => {
                let lhs_ty = self.value_type_of_expr(lhs);
                let rhs_ty = self.value_type_of_expr(rhs);
                if is_definitely_string_expr(lhs) {
                    self.compile_scalar_expr(lhs)?;
                    self.compile_string_concat_operand(rhs)?;
                    self.record_operand_types(ValueType::String, ValueType::String);
                    self.assembler.add();
                    return Ok(());
                }
                if is_definitely_string_expr(rhs) {
                    self.compile_string_concat_operand(lhs)?;
                    self.compile_scalar_expr(rhs)?;
                    self.record_operand_types(ValueType::String, ValueType::String);
                    self.assembler.add();
                    return Ok(());
                }
                self.compile_scalar_expr(lhs)?;
                self.compile_scalar_expr(rhs)?;
                self.record_operand_types(lhs_ty, rhs_ty);
                self.assembler.add();
            }
            Expr::Sub(lhs, rhs) => {
                let lhs_ty = self.value_type_of_expr(lhs);
                let rhs_ty = self.value_type_of_expr(rhs);
                self.compile_scalar_expr(lhs)?;
                self.compile_scalar_expr(rhs)?;
                self.record_operand_types(lhs_ty, rhs_ty);
                self.assembler.sub();
            }
            Expr::Mul(lhs, rhs) => {
                if let Expr::Int(value) = rhs.as_ref()
                    && let Some(shift) = shift_amount_for_power_of_two(*value)
                {
                    self.compile_scalar_expr(lhs)?;
                    self.assembler.push_const(Value::Int(shift as i64));
                    self.assembler.shl();
                } else if let Expr::Int(value) = lhs.as_ref()
                    && let Some(shift) = shift_amount_for_power_of_two(*value)
                {
                    self.compile_scalar_expr(rhs)?;
                    self.assembler.push_const(Value::Int(shift as i64));
                    self.assembler.shl();
                } else {
                    let lhs_ty = self.value_type_of_expr(lhs);
                    let rhs_ty = self.value_type_of_expr(rhs);
                    self.compile_scalar_expr(lhs)?;
                    self.compile_scalar_expr(rhs)?;
                    self.record_operand_types(lhs_ty, rhs_ty);
                    self.assembler.mul();
                }
            }
            Expr::Div(lhs, rhs) => {
                let lhs_ty = self.value_type_of_expr(lhs);
                let rhs_ty = self.value_type_of_expr(rhs);
                self.compile_scalar_expr(lhs)?;
                self.compile_scalar_expr(rhs)?;
                self.record_operand_types(lhs_ty, rhs_ty);
                self.assembler.div();
            }
            Expr::Mod(lhs, rhs) => {
                let lhs_ty = self.value_type_of_expr(lhs);
                let rhs_ty = self.value_type_of_expr(rhs);
                self.compile_scalar_expr(lhs)?;
                self.compile_scalar_expr(rhs)?;
                self.record_operand_types(lhs_ty, rhs_ty);
                self.assembler.modulo();
            }
            Expr::Neg(inner) => {
                let inner_ty = self.value_type_of_expr(inner);
                self.compile_scalar_expr(inner)?;
                self.record_unary_operand_type(inner_ty);
                self.assembler.neg();
            }
            Expr::Not(inner) => {
                self.compile_scalar_expr(inner)?;
                self.assembler.not();
            }
            Expr::ToOwned(inner) => {
                self.compile_scalar_expr(inner)?;
            }
            Expr::Borrow(inner) | Expr::BorrowMut(inner) => {
                self.compile_scalar_expr(inner)?;
            }
            Expr::And(lhs, rhs) => {
                self.compile_short_circuit_and(lhs, rhs)?;
            }
            Expr::Or(lhs, rhs) => {
                self.compile_short_circuit_or(lhs, rhs)?;
            }
            Expr::Eq(lhs, rhs) => {
                let lhs_ty = self.value_type_of_expr(lhs);
                let rhs_ty = self.value_type_of_expr(rhs);
                self.compile_scalar_expr(lhs)?;
                self.compile_scalar_expr(rhs)?;
                self.record_operand_types(lhs_ty, rhs_ty);
                self.assembler.ceq();
            }
            Expr::Lt(lhs, rhs) => {
                let lhs_ty = self.value_type_of_expr(lhs);
                let rhs_ty = self.value_type_of_expr(rhs);
                self.compile_scalar_expr(lhs)?;
                self.compile_scalar_expr(rhs)?;
                self.record_operand_types(lhs_ty, rhs_ty);
                self.assembler.clt();
            }
            Expr::Gt(lhs, rhs) => {
                let lhs_ty = self.value_type_of_expr(lhs);
                let rhs_ty = self.value_type_of_expr(rhs);
                self.compile_scalar_expr(lhs)?;
                self.compile_scalar_expr(rhs)?;
                self.record_operand_types(lhs_ty, rhs_ty);
                self.assembler.cgt();
            }
            Expr::Var(index) => {
                self.emit_copy_ldloc(*index)?;
            }
            Expr::MoveVar(index) => {
                self.emit_move_ldloc(*index)?;
                self.type_state.set(*index, typing::BoundType::Null);
            }
            Expr::MoveField { root, key } => {
                self.emit_copy_ldloc(*root)?;
                self.assembler.push_const(Value::string(key.clone()));
                self.assembler.call(BuiltinFunction::Get.call_index(), 2);

                self.emit_copy_ldloc(*root)?;
                self.assembler.push_const(Value::string(key.clone()));
                self.assembler.push_const(Value::Null);
                self.assembler.call(BuiltinFunction::Set.call_index(), 3);
                self.emit_stloc(*root)?;
            }
            Expr::MoveIndex { root, index } => {
                self.emit_copy_ldloc(*root)?;
                self.assembler.push_const(Value::Int(*index));
                self.assembler.call(BuiltinFunction::Get.call_index(), 2);

                self.emit_copy_ldloc(*root)?;
                self.assembler.push_const(Value::Int(*index));
                self.assembler.push_const(Value::Null);
                self.assembler.call(BuiltinFunction::Set.call_index(), 3);
                self.emit_stloc(*root)?;
            }
            Expr::IfElse {
                condition,
                then_expr,
                else_expr,
            } => {
                let callable_snapshot = self.callable_bindings.clone();
                let type_state_snapshot = self.type_state.clone();
                self.compile_scalar_expr(condition)?;
                let else_label = self.fresh_label("if_else");
                let end_label = self.fresh_label("if_end");
                self.assembler.brfalse_label(&else_label);
                self.type_state =
                    typing::refine_state_for_condition(&type_state_snapshot, condition, true);
                self.compile_expr(then_expr)?;
                let then_type_state = self.type_state.clone();
                self.assembler.br_label(&end_label);
                self.assembler
                    .label(&else_label)
                    .map_err(CompileError::Assembler)?;
                self.callable_bindings = callable_snapshot.clone();
                self.type_state =
                    typing::refine_state_for_condition(&type_state_snapshot, condition, false);
                self.compile_expr(else_expr)?;
                let else_type_state = self.type_state.clone();
                self.assembler
                    .label(&end_label)
                    .map_err(CompileError::Assembler)?;
                self.callable_bindings = callable_snapshot;
                self.type_state
                    .merge_from_branches(&then_type_state, &else_type_state);
            }
            Expr::Match {
                value_slot,
                result_slot,
                value,
                arms,
                default,
            } => {
                self.compile_scalar_expr(value)?;
                self.emit_stloc(*value_slot)?;
                let callable_snapshot = self.callable_bindings.clone();
                let match_entry_type_state = self.type_state.clone();
                let end_label = self.fresh_label("match_end");
                let mut merged_type_state: Option<typing::LocalTypeState> = None;
                for (pattern, arm_expr) in arms {
                    let next_label = self.fresh_label("match_next");
                    self.callable_bindings = callable_snapshot.clone();
                    self.type_state = match_entry_type_state.clone();
                    self.compile_match_pattern_condition(*value_slot, pattern)?;
                    self.assembler.brfalse_label(&next_label);
                    self.bind_match_pattern_slot(
                        pattern,
                        value,
                        *value_slot,
                        &match_entry_type_state,
                    )?;
                    self.compile_scalar_expr(arm_expr)?;
                    self.emit_stloc(*result_slot)?;
                    let arm_type_state = self.type_state.clone();
                    merged_type_state = Some(match merged_type_state {
                        Some(existing) => {
                            let mut merged = typing::LocalTypeState::default();
                            merged.merge_from_branches(&existing, &arm_type_state);
                            merged
                        }
                        None => arm_type_state,
                    });
                    self.assembler.br_label(&end_label);
                    self.assembler
                        .label(&next_label)
                        .map_err(CompileError::Assembler)?;
                }
                self.callable_bindings = callable_snapshot.clone();
                self.type_state = match_entry_type_state.clone();
                self.compile_scalar_expr(default)?;
                self.emit_stloc(*result_slot)?;
                let default_type_state = self.type_state.clone();
                self.assembler
                    .label(&end_label)
                    .map_err(CompileError::Assembler)?;
                self.callable_bindings = callable_snapshot;
                self.type_state = if let Some(existing) = merged_type_state {
                    let mut merged = typing::LocalTypeState::default();
                    merged.merge_from_branches(&existing, &default_type_state);
                    merged
                } else {
                    default_type_state
                };
                self.emit_copy_ldloc(*result_slot)?;
            }
            Expr::Block { stmts, expr } => {
                self.compile_stmts(stmts)?;
                self.compile_expr(expr)?;
            }
        }
        Ok(())
    }

    fn compile_optional_get_expr(
        &mut self,
        container: &Expr,
        key: &Expr,
        container_slot: LocalSlot,
        key_slot: LocalSlot,
    ) -> Result<(), CompileError> {
        self.compile_scalar_expr(container)?;
        self.emit_stloc(container_slot)?;
        self.compile_scalar_expr(key)?;
        self.emit_stloc(key_slot)?;

        let map_lookup = Expr::IfElse {
            condition: Box::new(Expr::Call(
                BuiltinFunction::Has.call_index(),
                Vec::new(),
                vec![Expr::Var(container_slot), Expr::Var(key_slot)],
            )),
            then_expr: Box::new(Expr::Call(
                BuiltinFunction::Get.call_index(),
                Vec::new(),
                vec![Expr::Var(container_slot), Expr::Var(key_slot)],
            )),
            else_expr: Box::new(Expr::Null),
        };
        let index_lookup = Expr::IfElse {
            condition: Box::new(Expr::Eq(
                Box::new(Expr::Call(
                    BuiltinFunction::TypeOf.call_index(),
                    Vec::new(),
                    vec![Expr::Var(key_slot)],
                )),
                Box::new(Expr::String("int".to_string())),
            )),
            then_expr: Box::new(Expr::IfElse {
                condition: Box::new(Expr::Lt(
                    Box::new(Expr::Var(key_slot)),
                    Box::new(Expr::Int(0)),
                )),
                then_expr: Box::new(Expr::Null),
                else_expr: Box::new(Expr::IfElse {
                    condition: Box::new(Expr::Lt(
                        Box::new(Expr::Var(key_slot)),
                        Box::new(Expr::Call(
                            BuiltinFunction::Len.call_index(),
                            Vec::new(),
                            vec![Expr::Var(container_slot)],
                        )),
                    )),
                    then_expr: Box::new(Expr::Call(
                        BuiltinFunction::Get.call_index(),
                        Vec::new(),
                        vec![Expr::Var(container_slot), Expr::Var(key_slot)],
                    )),
                    else_expr: Box::new(Expr::Null),
                }),
            }),
            else_expr: Box::new(Expr::Null),
        };
        let lowered = Expr::IfElse {
            condition: Box::new(Expr::Eq(
                Box::new(Expr::Call(
                    BuiltinFunction::TypeOf.call_index(),
                    Vec::new(),
                    vec![Expr::Var(container_slot)],
                )),
                Box::new(Expr::String("null".to_string())),
            )),
            then_expr: Box::new(Expr::Null),
            else_expr: Box::new(Expr::IfElse {
                condition: Box::new(Expr::Eq(
                    Box::new(Expr::Call(
                        BuiltinFunction::TypeOf.call_index(),
                        Vec::new(),
                        vec![Expr::Var(container_slot)],
                    )),
                    Box::new(Expr::String("map".to_string())),
                )),
                then_expr: Box::new(map_lookup),
                else_expr: Box::new(Expr::IfElse {
                    condition: Box::new(Expr::Eq(
                        Box::new(Expr::Call(
                            BuiltinFunction::TypeOf.call_index(),
                            Vec::new(),
                            vec![Expr::Var(container_slot)],
                        )),
                        Box::new(Expr::String("array".to_string())),
                    )),
                    then_expr: Box::new(index_lookup.clone()),
                    else_expr: Box::new(Expr::IfElse {
                        condition: Box::new(Expr::Eq(
                            Box::new(Expr::Call(
                                BuiltinFunction::TypeOf.call_index(),
                                Vec::new(),
                                vec![Expr::Var(container_slot)],
                            )),
                            Box::new(Expr::String("string".to_string())),
                        )),
                        then_expr: Box::new(index_lookup),
                        else_expr: Box::new(Expr::Null),
                    }),
                }),
            }),
        };

        self.compile_expr(&lowered)
    }

    fn compile_option_unwrap_or_expr(
        &mut self,
        value: &Expr,
        value_slot: LocalSlot,
        fallback: &Expr,
    ) -> Result<(), CompileError> {
        self.compile_scalar_expr(value)?;
        self.emit_stloc(value_slot)?;
        let lowered = Expr::IfElse {
            condition: Box::new(Expr::Eq(
                Box::new(Expr::Call(
                    BuiltinFunction::TypeOf.call_index(),
                    Vec::new(),
                    vec![Expr::Var(value_slot)],
                )),
                Box::new(Expr::String("null".to_string())),
            )),
            then_expr: Box::new(fallback.clone()),
            else_expr: Box::new(Expr::Var(value_slot)),
        };
        self.compile_expr(&lowered)
    }

    fn emit_named_callable_binding(&mut self, index: u16) -> Result<(), CompileError> {
        let Some(function_impl) = self.function_impls.get(&index).cloned() else {
            return Ok(());
        };
        if function_impl.capture_copies.is_empty() {
            return Ok(());
        }
        let prototype_id = *self
            .function_prototype_ids
            .get(&index)
            .ok_or(CompileError::CallableUsedAsValue)?;
        let slot = *self
            .function_slots
            .get(&index)
            .ok_or(CompileError::CallableUsedAsValue)?;
        self.emit_bind_callable(
            prototype_id,
            function_impl
                .capture_copies
                .iter()
                .map(|(source, _)| *source),
        )?;
        self.emit_stloc(slot)?;
        Ok(())
    }

    fn callable_binding_from_expr(
        &mut self,
        expr: &Expr,
    ) -> Result<Option<CallableBinding>, CompileError> {
        match expr {
            Expr::Closure(closure) => Ok(Some(CallableBinding::Closure(closure.clone()))),
            Expr::FunctionRef(index, _) => Ok(Some(CallableBinding::Function(*index))),
            Expr::Var(index) => Ok(self.callable_bindings.get(index).cloned()),
            _ => Ok(None),
        }
    }

    fn assign_expr_to_slot(
        &mut self,
        slot: LocalSlot,
        declared_schema: Option<&TypeSchema>,
        expr: &Expr,
    ) -> Result<(), CompileError> {
        if let Some(callable) = self.callable_binding_from_expr(expr)? {
            self.callable_bindings.insert(slot, callable.clone());
            match callable {
                CallableBinding::Closure(closure) => self.type_state.bind_closure(slot, &closure),
                CallableBinding::Function(index) => self.type_state.bind_function(slot, index),
            }
            if let Expr::Closure(closure) = expr {
                let prototype_id = self.emit_closure_callable_with_self(closure, Some(slot))?;
                self.callable_prototype_bindings.insert(slot, prototype_id);
            } else {
                if let Expr::Var(source) | Expr::MoveVar(source) = expr
                    && let Some(prototype_id) =
                        self.callable_prototype_bindings.get(source).copied()
                {
                    self.callable_prototype_bindings.insert(slot, prototype_id);
                }
                self.compile_expr(expr)?;
            }
            self.emit_stloc(slot)?;
            return Ok(());
        }
        let declared_binding = declared_schema.map(TypeSchema::split_optional).or_else(|| {
            self.type_state
                .has_declared_schema(slot)
                .then(|| {
                    (
                        self.type_state.schema(slot).cloned(),
                        self.type_state.is_optional(slot),
                    )
                })
                .and_then(|(schema, optional)| schema.map(|schema| (schema, optional)))
        });
        let slot_declared_schema = declared_binding.as_ref().map(|(schema, _)| schema.clone());
        let declared_optional = declared_binding
            .as_ref()
            .map(|(_, optional)| *optional)
            .unwrap_or(false);
        let optional = typing::expr_is_optional_with_function_impls_and_imports(
            expr,
            &self.type_state,
            &self.function_impls,
            &self.function_decls,
            &self.struct_schemas,
            &self.host_import_return_types,
            &self.host_import_signatures,
        ) || declared_optional;
        let ty = if optional {
            typing::infer_optional_expr_inner_type_with_function_impls_and_imports(
                expr,
                &self.type_state,
                &self.function_impls,
                &self.function_decls,
                &self.struct_schemas,
                &self.host_import_return_types,
                &self.host_import_signatures,
            )
        } else {
            self.infer_bound_type(expr)
        };
        self.callable_bindings.remove(&slot);
        if !self.try_compile_same_local_collection_rebind(slot, expr)? {
            self.compile_scalar_expr(expr)?;
        }
        self.emit_stloc(slot)?;
        let schema = slot_declared_schema.clone().or_else(|| {
            if optional {
                typing::infer_optional_expr_inner_schema_with_function_impls_and_imports(
                    expr,
                    &self.type_state,
                    &self.function_impls,
                    &self.function_decls,
                    &self.struct_schemas,
                    &self.host_import_return_types,
                    &self.host_import_signatures,
                )
            } else {
                typing::infer_expr_schema_with_function_impls_and_imports(
                    expr,
                    &self.type_state,
                    &self.function_impls,
                    &self.function_decls,
                    &self.struct_schemas,
                    &self.host_import_return_types,
                    &self.host_import_signatures,
                )
            }
        });
        let from_declared_schema =
            slot_declared_schema.is_some() || self.type_state.has_declared_schema(slot);
        let ty = slot_declared_schema
            .as_ref()
            .map(typing::bound_type_from_schema)
            .unwrap_or(ty);
        self.type_state.set_with_optional_schema_origin(
            slot,
            ty,
            schema,
            from_declared_schema,
            optional,
        );
        Ok(())
    }

    fn try_compile_same_local_collection_rebind(
        &mut self,
        target: LocalSlot,
        expr: &Expr,
    ) -> Result<bool, CompileError> {
        if !self.enable_local_move_semantics {
            return Ok(false);
        }
        let Expr::Call(index, _, args) = expr else {
            return Ok(false);
        };
        let Some(builtin) = BuiltinFunction::from_call_index(*index) else {
            return Ok(false);
        };
        let expected_arity = match builtin {
            BuiltinFunction::Set => 3,
            BuiltinFunction::ArrayPush => 2,
            _ => return Ok(false),
        };
        if args.len() != expected_arity
            || !matches!(args.first(), Some(Expr::Var(source)) if *source == target)
        {
            return Ok(false);
        }

        for arg in args {
            self.compile_scalar_expr(arg)?;
        }
        self.assembler.push_const(Value::Null);
        self.emit_stloc(target)?;
        self.emit_direct_call(*index, args)?;
        Ok(true)
    }

    fn compile_scalar_expr(&mut self, expr: &Expr) -> Result<(), CompileError> {
        self.compile_expr(expr)
    }

    fn compile_short_circuit_and(&mut self, lhs: &Expr, rhs: &Expr) -> Result<(), CompileError> {
        let false_label = self.fresh_label("and_false");
        let end_label = self.fresh_label("and_end");
        self.compile_scalar_expr(lhs)?;
        self.assembler.brfalse_label(&false_label);
        self.compile_scalar_expr(rhs)?;
        self.assembler.br_label(&end_label);
        self.assembler
            .label(&false_label)
            .map_err(CompileError::Assembler)?;
        self.assembler.push_const(Value::Bool(false));
        self.assembler
            .label(&end_label)
            .map_err(CompileError::Assembler)?;
        Ok(())
    }

    fn compile_short_circuit_or(&mut self, lhs: &Expr, rhs: &Expr) -> Result<(), CompileError> {
        let rhs_label = self.fresh_label("or_rhs");
        let end_label = self.fresh_label("or_end");
        self.compile_scalar_expr(lhs)?;
        self.assembler.brfalse_label(&rhs_label);
        self.assembler.push_const(Value::Bool(true));
        self.assembler.br_label(&end_label);
        self.assembler
            .label(&rhs_label)
            .map_err(CompileError::Assembler)?;
        self.compile_scalar_expr(rhs)?;
        self.assembler
            .label(&end_label)
            .map_err(CompileError::Assembler)?;
        Ok(())
    }

    fn ensure_function_value_slot(
        &mut self,
        index: u16,
        type_args: &[TypeSchema],
    ) -> Result<LocalSlot, CompileError> {
        if type_args.is_empty()
            && self
                .function_decls
                .get(&index)
                .is_some_and(|decl| !decl.type_params.is_empty())
        {
            let name = self
                .function_decls
                .get(&index)
                .map(|decl| decl.name.as_str())
                .unwrap_or("<unknown>");
            return Err(CompileError::CallableArgumentTypeMismatch {
                line: None,
                source_name: None,
                detail: format!(
                    "generic function value '{name}' requires explicit type arguments or an unambiguous callable context"
                ),
            });
        }
        if !type_args.is_empty()
            && let Some((_, _, slot)) = self
                .specialized_function_slots
                .iter()
                .find(|(candidate, args, _)| *candidate == index && args == type_args)
        {
            return Ok(*slot);
        }
        if type_args.is_empty()
            && let Some(slot) = self.function_slots.get(&index)
        {
            return Ok(*slot);
        }
        if !type_args.is_empty() && self.function_slots.contains_key(&index) {
            return self.ensure_specialized_function_slot(index, type_args);
        }

        let (target_index, arity) = if let Some(builtin) = BuiltinFunction::from_call_index(index) {
            (index, builtin.arity())
        } else if let Some(decl) = self.function_decls.get(&index) {
            (
                self.call_index_remap.get(&index).copied().unwrap_or(index),
                decl.args.len() as u8,
            )
        } else {
            return Err(CompileError::CallableUsedAsValue);
        };
        let slot = self.allocate_hidden_callable_slot()?;
        let prototype_id = self.callable_prototypes.len() as u32;
        self.callable_prototypes.push(CallablePrototype {
            kind: CallableKind::HostFunction,
            target: CallableTarget::HostImport(target_index),
            arity,
            frame_local_count: self.frame_local_count,
            parameter_slots: Vec::new(),
            capture_source_slots: Vec::new(),
            capture_slots: Vec::new(),
            capture_modes: Vec::new(),
            self_slot: None,
            schema: self.instantiated_callable_schema(index, type_args),
        });
        self.root_callable_bindings.push(RootCallableBinding {
            local_slot: slot,
            prototype_id,
        });
        self.function_slots.insert(index, slot);
        self.function_prototype_ids.insert(index, prototype_id);
        if type_args.is_empty() {
            Ok(slot)
        } else {
            self.ensure_specialized_function_slot(index, type_args)
        }
    }

    fn ensure_specialized_function_slot(
        &mut self,
        index: u16,
        type_args: &[TypeSchema],
    ) -> Result<LocalSlot, CompileError> {
        let base_prototype_id = *self
            .function_prototype_ids
            .get(&index)
            .ok_or(CompileError::CallableUsedAsValue)?;
        if self
            .function_impls
            .get(&index)
            .is_some_and(|function| !function.capture_copies.is_empty())
        {
            return self
                .function_slots
                .get(&index)
                .copied()
                .ok_or(CompileError::CallableUsedAsValue);
        }
        let slot = self.allocate_hidden_callable_slot()?;
        let mut prototype = self.callable_prototypes[base_prototype_id as usize].clone();
        prototype.schema = self.instantiated_callable_schema(index, type_args);
        prototype.frame_local_count = self.frame_local_count;
        let prototype_id = self.callable_prototypes.len() as u32;
        self.callable_prototypes.push(prototype);
        self.root_callable_bindings.push(RootCallableBinding {
            local_slot: slot,
            prototype_id,
        });
        self.specialized_function_slots
            .push((index, type_args.to_vec(), slot));
        Ok(slot)
    }

    fn allocate_hidden_callable_slot(&mut self) -> Result<LocalSlot, CompileError> {
        let slot = LocalSlot::try_from(self.frame_local_count)
            .map_err(|_| CompileError::LocalSlotOverflow(LocalSlot::MAX))?;
        let _ = local_slot_operand(slot)?;
        self.frame_local_count = self.frame_local_count.saturating_add(1);
        Ok(slot)
    }

    fn instantiated_callable_schema(
        &self,
        index: u16,
        type_args: &[TypeSchema],
    ) -> Option<TypeSchema> {
        let decl = self.function_decls.get(&index)?;
        if decl.type_params.len() != type_args.len() {
            return None;
        }
        let bindings = decl
            .type_params
            .iter()
            .cloned()
            .zip(type_args.iter().cloned())
            .collect::<HashMap<_, _>>();
        Some(TypeSchema::Callable {
            params: decl
                .arg_schemas
                .iter()
                .map(|schema| {
                    schema
                        .as_ref()
                        .map(|schema| substitute_type_schema(schema, &bindings))
                        .unwrap_or(TypeSchema::Unknown)
                })
                .collect(),
            result: Box::new(
                decl.return_schema
                    .as_ref()
                    .map(|schema| substitute_type_schema(schema, &bindings))
                    .unwrap_or(TypeSchema::Unknown),
            ),
        })
    }

    fn record_closure_param_hints(&mut self, prototype_id: u32, args: &[Expr]) {
        let hints = args
            .iter()
            .map(|arg| {
                let schema = typing::infer_expr_schema_with_function_impls_and_imports(
                    arg,
                    &self.type_state,
                    &self.function_impls,
                    &self.function_decls,
                    &self.struct_schemas,
                    &self.host_import_return_types,
                    &self.host_import_signatures,
                );
                let ty = schema
                    .as_ref()
                    .map(typing::bound_type_from_schema)
                    .unwrap_or_else(|| self.infer_bound_type(arg));
                (ty, schema)
            })
            .collect::<Vec<_>>();

        self.closure_param_hints
            .entry(prototype_id)
            .and_modify(|existing| {
                for (index, hint) in hints.iter().enumerate() {
                    if let Some(existing) = existing.get_mut(index)
                        && existing.0 == typing::BoundType::Unknown
                    {
                        *existing = hint.clone();
                    }
                }
            })
            .or_insert(hints);
    }

    fn compile_function_call(&mut self, index: u16, args: &[Expr]) -> Result<(), CompileError> {
        if self.function_impls.contains_key(&index) {
            let slot = *self
                .function_slots
                .get(&index)
                .ok_or(CompileError::CallableUsedAsValue)?;
            self.emit_copy_ldloc(slot)?;
            return self.compile_callvalue_args(args);
        }
        self.compile_direct_call(index, args)
    }

    fn compile_callvalue_args(&mut self, args: &[Expr]) -> Result<(), CompileError> {
        for arg in args {
            self.compile_scalar_expr(arg)?;
        }
        let argc = u8::try_from(args.len()).map_err(|_| CompileError::CallArityOverflow)?;
        self.assembler.call_value(argc);
        Ok(())
    }

    fn emit_closure_callable(&mut self, closure: &ClosureExpr) -> Result<u32, CompileError> {
        self.emit_closure_callable_with_self(closure, None)
    }

    fn emit_closure_callable_with_self(
        &mut self,
        closure: &ClosureExpr,
        binding_slot: Option<LocalSlot>,
    ) -> Result<u32, CompileError> {
        let prototype_id = self.callable_prototypes.len() as u32;
        self.callable_prototypes.push(CallablePrototype {
            kind: CallableKind::Closure,
            target: CallableTarget::ScriptFunction(u32::MAX),
            arity: u8::try_from(closure.param_slots.len())
                .map_err(|_| CompileError::CallArityOverflow)?,
            frame_local_count: self.frame_local_count,
            parameter_slots: closure.param_slots.clone(),
            capture_source_slots: closure
                .capture_copies
                .iter()
                .map(|(source, _)| *source)
                .collect(),
            capture_slots: closure
                .capture_copies
                .iter()
                .map(|(_, target)| *target)
                .collect(),
            capture_modes: closure
                .capture_copies
                .iter()
                .map(|(_, target)| super::lifetime::closure_capture_binding_mode(closure, *target))
                .collect(),
            self_slot: binding_slot.and_then(|binding_slot| {
                closure
                    .capture_copies
                    .iter()
                    .find_map(|(source, target)| (*source == binding_slot).then_some(*target))
            }),
            schema: binding_slot.and_then(|slot| {
                self.type_map
                    .local_schemas
                    .get(slot as usize)
                    .cloned()
                    .flatten()
            }),
        });
        self.pending_closures.push((prototype_id, closure.clone()));
        self.emit_bind_callable(
            prototype_id,
            closure.capture_copies.iter().map(|(source, _)| *source),
        )?;
        Ok(prototype_id)
    }

    fn emit_bind_callable(
        &mut self,
        prototype_id: u32,
        capture_slots: impl IntoIterator<Item = LocalSlot>,
    ) -> Result<(), CompileError> {
        self.assembler
            .push_const(Value::Int(i64::from(prototype_id)));
        self.assembler
            .call(BuiltinFunction::ArrayNew.call_index(), 0);
        for source_slot in capture_slots {
            self.emit_copy_ldloc(source_slot)?;
            self.assembler
                .call(BuiltinFunction::ArrayPush.call_index(), 2);
        }
        self.assembler
            .call(BuiltinFunction::BindCallable.call_index(), 2);
        Ok(())
    }

    fn compile_direct_call(&mut self, index: u16, args: &[Expr]) -> Result<(), CompileError> {
        for arg in args {
            self.compile_scalar_expr(arg)?;
        }
        self.emit_direct_call(index, args)
    }

    fn emit_direct_call(&mut self, index: u16, args: &[Expr]) -> Result<(), CompileError> {
        let argc = u8::try_from(args.len()).map_err(|_| CompileError::CallArityOverflow)?;
        if let Some(builtin) = BuiltinFunction::from_call_index(index) {
            debug_assert!(builtin.accepts_arity(argc));
            self.record_builtin_call_operand_types(args);
            self.assembler.call(index, argc);
            return Ok(());
        }
        let remapped_index = self.call_index_remap.get(&index).copied().unwrap_or(index);
        self.assembler.call(remapped_index, argc);
        Ok(())
    }

    fn compile_match_pattern_condition(
        &mut self,
        value_slot: LocalSlot,
        pattern: &MatchPattern,
    ) -> Result<(), CompileError> {
        match pattern {
            MatchPattern::Int(v) => {
                self.emit_copy_ldloc(value_slot)?;
                self.assembler.push_const(Value::Int(*v));
                self.assembler.ceq();
            }
            MatchPattern::String(v) => {
                self.emit_copy_ldloc(value_slot)?;
                self.assembler.push_const(Value::string(v.clone()));
                self.assembler.ceq();
            }
            MatchPattern::Bytes(v) => {
                self.emit_copy_ldloc(value_slot)?;
                self.assembler.push_const(Value::bytes(v.clone()));
                self.assembler.ceq();
            }
            MatchPattern::Null => {
                self.emit_copy_ldloc(value_slot)?;
                self.assembler.push_const(Value::Null);
                self.assembler.ceq();
            }
            MatchPattern::None => {
                self.emit_copy_ldloc(value_slot)?;
                self.assembler.push_const(Value::Null);
                self.assembler.ceq();
            }
            MatchPattern::SomeBinding(_) => {
                self.emit_copy_ldloc(value_slot)?;
                self.assembler.push_const(Value::Null);
                self.assembler.ceq();
                self.assembler.not();
            }
            MatchPattern::Type(type_pattern) => {
                self.compile_match_type_pattern_condition(value_slot, type_pattern)?;
            }
        }
        Ok(())
    }

    fn compile_match_type_pattern_condition(
        &mut self,
        value_slot: LocalSlot,
        type_pattern: &MatchTypePattern,
    ) -> Result<(), CompileError> {
        match type_pattern {
            MatchTypePattern::Int => self.compile_type_name_equals(value_slot, "int")?,
            MatchTypePattern::Float => self.compile_type_name_equals(value_slot, "float")?,
            MatchTypePattern::Bool => self.compile_type_name_equals(value_slot, "bool")?,
            MatchTypePattern::String => self.compile_type_name_equals(value_slot, "string")?,
            MatchTypePattern::Bytes => self.compile_type_name_equals(value_slot, "bytes")?,
            MatchTypePattern::Array => self.compile_type_name_equals(value_slot, "array")?,
            MatchTypePattern::Map => self.compile_type_name_equals(value_slot, "map")?,
            MatchTypePattern::Number => {
                let number_fallback_label = self.fresh_label("match_type_number_fallback");
                let number_end_label = self.fresh_label("match_type_number_end");

                self.compile_type_name_equals(value_slot, "int")?;
                self.assembler.brfalse_label(&number_fallback_label);
                self.assembler.push_const(Value::Bool(true));
                self.assembler.br_label(&number_end_label);
                self.assembler
                    .label(&number_fallback_label)
                    .map_err(CompileError::Assembler)?;
                self.compile_type_name_equals(value_slot, "float")?;
                self.assembler
                    .label(&number_end_label)
                    .map_err(CompileError::Assembler)?;
            }
        }
        Ok(())
    }

    fn compile_type_name_equals(
        &mut self,
        value_slot: LocalSlot,
        expected: &str,
    ) -> Result<(), CompileError> {
        self.emit_copy_ldloc(value_slot)?;
        self.assembler.call(BuiltinFunction::TypeOf.call_index(), 1);
        self.assembler
            .push_const(Value::string(expected.to_string()));
        self.assembler.ceq();
        Ok(())
    }

    fn bind_match_pattern_slot(
        &mut self,
        pattern: &MatchPattern,
        value: &Expr,
        value_slot: LocalSlot,
        match_entry_type_state: &typing::LocalTypeState,
    ) -> Result<(), CompileError> {
        let Some(binding_slot) = pattern.binding_slot() else {
            return Ok(());
        };
        let ty = typing::infer_optional_expr_inner_type_with_function_impls_and_imports(
            value,
            match_entry_type_state,
            &self.function_impls,
            &self.function_decls,
            &self.struct_schemas,
            &self.host_import_return_types,
            &self.host_import_signatures,
        );
        let schema = typing::infer_optional_expr_inner_schema_with_function_impls_and_imports(
            value,
            match_entry_type_state,
            &self.function_impls,
            &self.function_decls,
            &self.struct_schemas,
            &self.host_import_return_types,
            &self.host_import_signatures,
        );
        self.emit_copy_ldloc(value_slot)?;
        self.emit_stloc(binding_slot)?;
        self.type_state
            .set_with_optional_schema_origin(binding_slot, ty, schema, false, false);
        Ok(())
    }

    fn infer_bound_type(&self, expr: &Expr) -> typing::BoundType {
        typing::infer_expr_type_with_function_impls_and_imports(
            expr,
            &self.type_state,
            &self.function_impls,
            &self.function_decls,
            &self.struct_schemas,
            &self.host_import_return_types,
            &self.host_import_signatures,
        )
    }

    fn simulate_stmt_type_state(
        &self,
        stmts: &[Stmt],
        initial_state: &typing::LocalTypeState,
    ) -> typing::LocalTypeState {
        let mut state = initial_state.clone();
        typing::apply_stmts_with_function_impls_and_imports(
            stmts,
            &mut state,
            &self.function_impls,
            &self.function_decls,
            &self.struct_schemas,
            &self.host_import_return_types,
            &self.host_import_signatures,
        );
        state
    }

    fn stabilize_loop_type_state<F>(
        &self,
        initial_state: &typing::LocalTypeState,
        mut run_iteration: F,
    ) -> typing::LocalTypeState
    where
        F: FnMut(&mut typing::LocalTypeState),
    {
        let zero_iteration = initial_state.clone();
        let mut first_iteration = initial_state.clone();
        run_iteration(&mut first_iteration);
        let mut second_iteration = first_iteration.clone();
        run_iteration(&mut second_iteration);

        let mut stable_iteration = typing::LocalTypeState::default();
        stable_iteration.merge_from_branches(&first_iteration, &second_iteration);

        let mut stabilized = zero_iteration.clone();
        stabilized.merge_from_branches(&zero_iteration, &stable_iteration);
        stabilized
    }

    fn value_type_of_expr(&self, expr: &Expr) -> ValueType {
        ValueType::from(self.infer_bound_type(expr))
    }

    fn record_operand_types(&mut self, lhs: ValueType, rhs: ValueType) {
        if lhs == ValueType::Unknown || rhs == ValueType::Unknown {
            return;
        }
        self.type_map
            .operand_types
            .insert(self.assembler.position() as usize, (lhs, rhs));
    }

    fn record_unary_operand_type(&mut self, operand: ValueType) {
        if operand == ValueType::Unknown {
            return;
        }
        self.type_map.operand_types.insert(
            self.assembler.position() as usize,
            (operand, ValueType::Unknown),
        );
    }

    fn record_builtin_call_operand_types(&mut self, args: &[Expr]) {
        if args.is_empty() {
            return;
        }
        let lhs = self.value_type_of_expr(&args[0]);
        let rhs = args
            .get(1)
            .map(|expr| self.value_type_of_expr(expr))
            .unwrap_or(ValueType::Unknown);
        if lhs == ValueType::Unknown && rhs == ValueType::Unknown {
            return;
        }
        self.type_map
            .operand_types
            .insert(self.assembler.position() as usize, (lhs, rhs));
    }

    fn fresh_label(&mut self, prefix: &str) -> String {
        let label = format!("{prefix}_{}", self.next_label_id);
        self.next_label_id += 1;
        label
    }

    fn emit_move_ldloc(&mut self, slot: LocalSlot) -> Result<(), CompileError> {
        let operand = local_slot_operand(slot)?;
        self.assembler.ldloc(operand);
        self.assembler.push_const(Value::Int(i64::from(operand)));
        self.assembler
            .call(BuiltinFunction::DetachLocal.call_index(), 1);
        Ok(())
    }

    fn emit_copy_ldloc(&mut self, slot: LocalSlot) -> Result<(), CompileError> {
        self.assembler.ldloc(local_slot_operand(slot)?);
        Ok(())
    }

    fn emit_stloc(&mut self, slot: LocalSlot) -> Result<(), CompileError> {
        self.assembler.stloc(local_slot_operand(slot)?);
        Ok(())
    }

    fn compile_string_concat_operand(&mut self, expr: &Expr) -> Result<(), CompileError> {
        if let Some(value) = eval_const_int_expr(expr) {
            self.assembler.push_const(Value::string(value.to_string()));
            return Ok(());
        }

        self.compile_scalar_expr(expr)?;
        self.lower_number_to_string_for_concat_top();
        Ok(())
    }

    fn lower_number_to_string_for_concat_top(&mut self) {
        let not_int_label = self.fresh_label("concat_not_int");
        let not_float_label = self.fresh_label("concat_not_float");
        let done_label = self.fresh_label("concat_value_done");

        self.assembler.dup();
        self.assembler.call(BuiltinFunction::TypeOf.call_index(), 1);
        self.assembler.push_const(Value::string("int"));
        self.assembler.ceq();
        self.assembler.brfalse_label(&not_int_label);
        self.assembler
            .call(BuiltinFunction::ToString.call_index(), 1);
        self.assembler.br_label(&done_label);

        self.assembler
            .label(&not_int_label)
            .expect("compiler-generated label should be valid");
        self.assembler.dup();
        self.assembler.call(BuiltinFunction::TypeOf.call_index(), 1);
        self.assembler.push_const(Value::string("float"));
        self.assembler.ceq();
        self.assembler.brfalse_label(&not_float_label);
        self.assembler
            .call(BuiltinFunction::ToString.call_index(), 1);
        self.assembler.br_label(&done_label);

        self.assembler
            .label(&not_float_label)
            .expect("compiler-generated label should be valid");
        self.assembler
            .label(&done_label)
            .expect("compiler-generated label should be valid");
    }
}

fn substitute_type_schema(
    schema: &TypeSchema,
    bindings: &HashMap<String, TypeSchema>,
) -> TypeSchema {
    match schema {
        TypeSchema::GenericParam(name) => bindings
            .get(name)
            .cloned()
            .unwrap_or_else(|| schema.clone()),
        TypeSchema::Optional(inner) => {
            TypeSchema::Optional(Box::new(substitute_type_schema(inner, bindings)))
        }
        TypeSchema::Named(name, args) => TypeSchema::Named(
            name.clone(),
            args.iter()
                .map(|schema| substitute_type_schema(schema, bindings))
                .collect(),
        ),
        TypeSchema::Array(inner) => {
            TypeSchema::Array(Box::new(substitute_type_schema(inner, bindings)))
        }
        TypeSchema::ArrayTuple(items) => TypeSchema::ArrayTuple(
            items
                .iter()
                .map(|schema| substitute_type_schema(schema, bindings))
                .collect(),
        ),
        TypeSchema::ArrayTupleRest { prefix, rest } => TypeSchema::ArrayTupleRest {
            prefix: prefix
                .iter()
                .map(|schema| substitute_type_schema(schema, bindings))
                .collect(),
            rest: Box::new(substitute_type_schema(rest, bindings)),
        },
        TypeSchema::Map(inner) => {
            TypeSchema::Map(Box::new(substitute_type_schema(inner, bindings)))
        }
        TypeSchema::Object(fields) => TypeSchema::Object(
            fields
                .iter()
                .map(|(name, schema)| (name.clone(), substitute_type_schema(schema, bindings)))
                .collect(),
        ),
        TypeSchema::Callable { params, result } => TypeSchema::Callable {
            params: params
                .iter()
                .map(|schema| substitute_type_schema(schema, bindings))
                .collect(),
            result: Box::new(substitute_type_schema(result, bindings)),
        },
        _ => schema.clone(),
    }
}

fn local_slot_operand(index: LocalSlot) -> Result<u8, CompileError> {
    u8::try_from(index).map_err(|_| CompileError::LocalSlotOverflow(index))
}

fn shift_amount_for_power_of_two(value: i64) -> Option<u32> {
    if value <= 0 {
        return None;
    }
    let as_u64 = value as u64;
    if !as_u64.is_power_of_two() {
        return None;
    }
    Some(as_u64.trailing_zeros())
}

fn is_definitely_string_expr(expr: &Expr) -> bool {
    match expr {
        Expr::String(_) => true,
        Expr::ToOwned(inner) | Expr::Borrow(inner) | Expr::BorrowMut(inner) => {
            is_definitely_string_expr(inner)
        }
        Expr::Add(lhs, rhs) => {
            (is_definitely_string_expr(lhs) && is_definitely_string_expr(rhs))
                || (is_definitely_string_expr(lhs) && eval_const_int_expr(rhs).is_some())
                || (eval_const_int_expr(lhs).is_some() && is_definitely_string_expr(rhs))
        }
        _ => false,
    }
}

fn eval_const_int_expr(expr: &Expr) -> Option<i64> {
    match expr {
        Expr::Int(value) => Some(*value),
        Expr::ToOwned(inner) | Expr::Borrow(inner) | Expr::BorrowMut(inner) => {
            eval_const_int_expr(inner)
        }
        Expr::Neg(inner) => eval_const_int_expr(inner)?.checked_neg(),
        Expr::Add(lhs, rhs) => eval_const_int_expr(lhs)?.checked_add(eval_const_int_expr(rhs)?),
        Expr::Sub(lhs, rhs) => eval_const_int_expr(lhs)?.checked_sub(eval_const_int_expr(rhs)?),
        Expr::Mul(lhs, rhs) => eval_const_int_expr(lhs)?.checked_mul(eval_const_int_expr(rhs)?),
        Expr::Div(lhs, rhs) => {
            let rhs = eval_const_int_expr(rhs)?;
            if rhs == 0 {
                return None;
            }
            eval_const_int_expr(lhs)?.checked_div(rhs)
        }
        _ => None,
    }
}
