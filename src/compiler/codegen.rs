use std::collections::{BTreeSet, HashMap, HashSet};

use crate::assembler::Assembler;
use crate::builtins::BuiltinFunction;
use crate::{Program, TypeMap, Value, ValueType};

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
    inline_call_stack: Vec<u16>,
    callable_bindings: HashMap<LocalSlot, CallableBinding>,
    enable_local_move_semantics: bool,
    typing_mode: TypingMode,
    type_state: typing::LocalTypeState,
    type_map: TypeMap,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum CaptureBindingMode {
    Copy,
    Borrow,
    BorrowMut,
    Move,
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
            inline_call_stack: Vec::new(),
            callable_bindings: HashMap::new(),
            enable_local_move_semantics: false,
            typing_mode: TypingMode::DynamicHints,
            type_state: typing::LocalTypeState::default(),
            type_map: TypeMap::default(),
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
        self.compile_stmts(stmts)?;
        self.assembler.ret();
        let mut program = self
            .assembler
            .finish_program()
            .map_err(CompileError::Assembler)?;
        self.type_map.strict_types = self.typing_mode.is_strict();
        program.type_map = Some(self.type_map);
        Ok(program)
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
            Stmt::ClosureLet { line, closure } => {
                self.assembler.mark_line(*line);
                self.bind_closure_captures(closure)?;
            }
            Stmt::FuncDecl {
                index,
                has_impl,
                line,
                ..
            } => {
                self.assembler.mark_line(*line);
                if *has_impl {
                    self.bind_function_decl_captures(*index)?;
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
            Expr::FunctionRef(_) => {
                return Err(CompileError::CallableUsedAsValue);
            }
            Expr::Call(index, _, args) => {
                self.compile_function_call(*index, args)?;
            }
            Expr::Closure(_) => {
                return Err(CompileError::CallableUsedAsValue);
            }
            Expr::ClosureCall(closure, args) => {
                self.compile_inline_closure_call(closure, args)?;
            }
            Expr::LocalCall(index, _, args) => {
                let callable = self
                    .callable_bindings
                    .get(index)
                    .cloned()
                    .ok_or(CompileError::NonCallableLocal(*index))?;
                self.compile_callable_call(callable, args)?;
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
                if self.callable_bindings.contains_key(index) {
                    return Err(CompileError::CallableUsedAsValue);
                }
                self.emit_copy_ldloc(*index)?;
            }
            Expr::MoveVar(index) => {
                if self.callable_bindings.contains_key(index) {
                    return Err(CompileError::CallableUsedAsValue);
                }
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

    fn bind_closure_captures(&mut self, closure: &ClosureExpr) -> Result<(), CompileError> {
        let mut seen = HashSet::new();
        for (source_index, captured_slot) in &closure.capture_copies {
            if !seen.insert((*source_index, *captured_slot)) {
                continue;
            }
            let capture_mode = self.closure_capture_mode_for_slot(closure, *captured_slot);
            self.bind_capture_copy(*source_index, *captured_slot, capture_mode)?;
        }
        Ok(())
    }

    fn bind_function_decl_captures(&mut self, index: u16) -> Result<(), CompileError> {
        let Some(function_impl) = self.function_impls.get(&index).cloned() else {
            return Ok(());
        };
        let mut seen = HashSet::new();
        for (source_index, captured_slot) in &function_impl.capture_copies {
            if !seen.insert((*source_index, *captured_slot)) {
                continue;
            }
            let capture_mode = self.function_capture_mode_for_slot(&function_impl, *captured_slot);
            self.bind_capture_copy(*source_index, *captured_slot, capture_mode)?;
        }
        Ok(())
    }

    fn bind_capture_copy(
        &mut self,
        source_index: LocalSlot,
        captured_slot: LocalSlot,
        capture_mode: CaptureBindingMode,
    ) -> Result<(), CompileError> {
        let captured_type = self.type_state.get(source_index);
        if self.enable_local_move_semantics && capture_mode == CaptureBindingMode::Move {
            self.emit_move_ldloc(source_index)?;
            self.type_state.set(source_index, typing::BoundType::Null);
        } else {
            self.emit_copy_ldloc(source_index)?;
        }
        self.emit_stloc(captured_slot)?;
        self.type_state.set(captured_slot, captured_type);
        Ok(())
    }

    fn function_capture_mode_for_slot(
        &self,
        function_impl: &FunctionImpl,
        captured_slot: LocalSlot,
    ) -> CaptureBindingMode {
        let mut mode = CaptureBindingMode::Copy;
        let mut seen = false;
        self.capture_mode_for_stmts(
            &function_impl.body_stmts,
            captured_slot,
            CaptureBindingMode::Move,
            &mut mode,
            &mut seen,
        );
        self.capture_mode_for_expr(
            &function_impl.body_expr,
            captured_slot,
            CaptureBindingMode::Move,
            &mut mode,
            &mut seen,
        );
        if seen { mode } else { CaptureBindingMode::Move }
    }

    fn closure_capture_mode_for_slot(
        &self,
        closure: &ClosureExpr,
        captured_slot: LocalSlot,
    ) -> CaptureBindingMode {
        let mut mode = CaptureBindingMode::Copy;
        let mut seen = false;
        self.capture_mode_for_expr(
            &closure.body,
            captured_slot,
            CaptureBindingMode::Move,
            &mut mode,
            &mut seen,
        );
        if seen { mode } else { CaptureBindingMode::Move }
    }

    fn capture_mode_for_stmts(
        &self,
        stmts: &[Stmt],
        captured_slot: LocalSlot,
        context: CaptureBindingMode,
        mode: &mut CaptureBindingMode,
        seen: &mut bool,
    ) {
        for stmt in stmts {
            self.capture_mode_for_stmt(stmt, captured_slot, context, mode, seen);
        }
    }

    fn capture_mode_for_stmt(
        &self,
        stmt: &Stmt,
        captured_slot: LocalSlot,
        context: CaptureBindingMode,
        mode: &mut CaptureBindingMode,
        seen: &mut bool,
    ) {
        match stmt {
            Stmt::Noop { .. }
            | Stmt::FuncDecl { .. }
            | Stmt::Break { .. }
            | Stmt::Continue { .. } => {}
            Stmt::Drop { index, .. } => {
                if *index == captured_slot {
                    *seen = true;
                    *mode = (*mode).max(context);
                }
            }
            Stmt::Let { index, expr, .. } | Stmt::Assign { index, expr, .. } => {
                if *index == captured_slot {
                    *seen = true;
                    *mode = (*mode).max(context);
                }
                self.capture_mode_for_expr(expr, captured_slot, context, mode, seen);
            }
            Stmt::ClosureLet { closure, .. } => {
                for (nested_source_slot, nested_captured_slot) in &closure.capture_copies {
                    if *nested_source_slot == captured_slot {
                        self.capture_mode_for_expr(
                            &closure.body,
                            *nested_captured_slot,
                            CaptureBindingMode::Move,
                            mode,
                            seen,
                        );
                    }
                }
                self.capture_mode_for_expr(&closure.body, captured_slot, context, mode, seen);
            }
            Stmt::Expr { expr, .. } => {
                self.capture_mode_for_expr(expr, captured_slot, context, mode, seen);
            }
            Stmt::IfElse {
                condition,
                then_branch,
                else_branch,
                ..
            } => {
                self.capture_mode_for_expr(condition, captured_slot, context, mode, seen);
                self.capture_mode_for_stmts(then_branch, captured_slot, context, mode, seen);
                self.capture_mode_for_stmts(else_branch, captured_slot, context, mode, seen);
            }
            Stmt::For {
                init,
                condition,
                post,
                body,
                ..
            } => {
                self.capture_mode_for_stmt(init, captured_slot, context, mode, seen);
                self.capture_mode_for_expr(condition, captured_slot, context, mode, seen);
                self.capture_mode_for_stmt(post, captured_slot, context, mode, seen);
                self.capture_mode_for_stmts(body, captured_slot, context, mode, seen);
            }
            Stmt::While {
                condition, body, ..
            } => {
                self.capture_mode_for_expr(condition, captured_slot, context, mode, seen);
                self.capture_mode_for_stmts(body, captured_slot, context, mode, seen);
            }
        }
    }

    fn capture_mode_for_expr(
        &self,
        expr: &Expr,
        captured_slot: LocalSlot,
        context: CaptureBindingMode,
        mode: &mut CaptureBindingMode,
        seen: &mut bool,
    ) {
        match expr {
            Expr::Null
            | Expr::Int(_)
            | Expr::Float(_)
            | Expr::Bool(_)
            | Expr::Bytes(_)
            | Expr::String(_)
            | Expr::FunctionRef(_) => {}
            Expr::Var(index) => {
                if *index == captured_slot {
                    *seen = true;
                    *mode = (*mode).max(context);
                }
            }
            Expr::MoveVar(index) => {
                if *index == captured_slot {
                    *seen = true;
                    *mode = CaptureBindingMode::Move;
                }
            }
            Expr::MoveField { root, .. } | Expr::MoveIndex { root, .. } => {
                if *root == captured_slot {
                    *seen = true;
                    *mode = CaptureBindingMode::Move;
                }
            }
            Expr::OptionalGet {
                container,
                key,
                container_slot,
                key_slot,
            } => {
                if *container_slot == captured_slot || *key_slot == captured_slot {
                    *seen = true;
                    *mode = (*mode).max(context);
                }
                self.capture_mode_for_expr(container, captured_slot, context, mode, seen);
                self.capture_mode_for_expr(key, captured_slot, context, mode, seen);
            }
            Expr::OptionUnwrapOr {
                value,
                value_slot,
                fallback,
            } => {
                if *value_slot == captured_slot {
                    *seen = true;
                    *mode = (*mode).max(context);
                }
                self.capture_mode_for_expr(value, captured_slot, context, mode, seen);
                self.capture_mode_for_expr(fallback, captured_slot, context, mode, seen);
            }
            Expr::Call(_, _, args) | Expr::LocalCall(_, _, args) => {
                for arg in args {
                    self.capture_mode_for_expr(arg, captured_slot, context, mode, seen);
                }
            }
            Expr::Closure(closure) => {
                for (nested_source_slot, nested_captured_slot) in &closure.capture_copies {
                    if *nested_source_slot == captured_slot {
                        self.capture_mode_for_expr(
                            &closure.body,
                            *nested_captured_slot,
                            CaptureBindingMode::Move,
                            mode,
                            seen,
                        );
                    }
                }
                self.capture_mode_for_expr(&closure.body, captured_slot, context, mode, seen);
            }
            Expr::ClosureCall(closure, args) => {
                for arg in args {
                    self.capture_mode_for_expr(arg, captured_slot, context, mode, seen);
                }
                for (nested_source_slot, nested_captured_slot) in &closure.capture_copies {
                    if *nested_source_slot == captured_slot {
                        self.capture_mode_for_expr(
                            &closure.body,
                            *nested_captured_slot,
                            CaptureBindingMode::Move,
                            mode,
                            seen,
                        );
                    }
                }
                self.capture_mode_for_expr(&closure.body, captured_slot, context, mode, seen);
            }
            Expr::Add(lhs, rhs)
            | Expr::Sub(lhs, rhs)
            | Expr::Mul(lhs, rhs)
            | Expr::Div(lhs, rhs)
            | Expr::Mod(lhs, rhs)
            | Expr::And(lhs, rhs)
            | Expr::Or(lhs, rhs)
            | Expr::Eq(lhs, rhs)
            | Expr::Lt(lhs, rhs)
            | Expr::Gt(lhs, rhs) => {
                self.capture_mode_for_expr(lhs, captured_slot, context, mode, seen);
                self.capture_mode_for_expr(rhs, captured_slot, context, mode, seen);
            }
            Expr::Neg(inner) | Expr::Not(inner) => {
                self.capture_mode_for_expr(inner, captured_slot, context, mode, seen);
            }
            Expr::ToOwned(inner) => {
                self.capture_mode_for_expr(
                    inner,
                    captured_slot,
                    CaptureBindingMode::Copy,
                    mode,
                    seen,
                );
            }
            Expr::Borrow(inner) => {
                self.capture_mode_for_expr(
                    inner,
                    captured_slot,
                    CaptureBindingMode::Borrow,
                    mode,
                    seen,
                );
            }
            Expr::BorrowMut(inner) => {
                self.capture_mode_for_expr(
                    inner,
                    captured_slot,
                    CaptureBindingMode::BorrowMut,
                    mode,
                    seen,
                );
            }
            Expr::IfElse {
                condition,
                then_expr,
                else_expr,
            } => {
                self.capture_mode_for_expr(condition, captured_slot, context, mode, seen);
                self.capture_mode_for_expr(then_expr, captured_slot, context, mode, seen);
                self.capture_mode_for_expr(else_expr, captured_slot, context, mode, seen);
            }
            Expr::Match {
                value_slot,
                result_slot,
                value,
                arms,
                default,
            } => {
                if *value_slot == captured_slot || *result_slot == captured_slot {
                    *seen = true;
                    *mode = (*mode).max(context);
                }
                self.capture_mode_for_expr(value, captured_slot, context, mode, seen);
                for (_, arm_expr) in arms {
                    self.capture_mode_for_expr(arm_expr, captured_slot, context, mode, seen);
                }
                self.capture_mode_for_expr(default, captured_slot, context, mode, seen);
            }
            Expr::Block { stmts, expr } => {
                self.capture_mode_for_stmts(stmts, captured_slot, context, mode, seen);
                self.capture_mode_for_expr(expr, captured_slot, context, mode, seen);
            }
        }
    }

    fn callable_binding_from_expr(
        &mut self,
        expr: &Expr,
    ) -> Result<Option<CallableBinding>, CompileError> {
        match expr {
            Expr::Closure(closure) => {
                self.bind_closure_captures(closure)?;
                Ok(Some(CallableBinding::Closure(closure.clone())))
            }
            Expr::FunctionRef(index) => Ok(Some(CallableBinding::Function(*index))),
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
        self.compile_scalar_expr(expr)?;
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

    fn compile_function_call(&mut self, index: u16, args: &[Expr]) -> Result<(), CompileError> {
        if let Some(function_impl) = self.function_impls.get(&index).cloned() {
            return self.compile_inline_function_call(index, &function_impl, args);
        }
        self.compile_direct_call(index, args)
    }

    fn compile_inline_function_call(
        &mut self,
        index: u16,
        function_impl: &FunctionImpl,
        args: &[Expr],
    ) -> Result<(), CompileError> {
        if function_impl.param_slots.len() != args.len() {
            return Err(CompileError::CallableArityMismatch {
                expected: function_impl.param_slots.len(),
                got: args.len(),
            });
        }
        let frame_slots = collect_function_frame_slots(function_impl);
        let callable_snapshot = self.callable_bindings.clone();
        for (arg, slot) in args.iter().zip(function_impl.param_slots.iter()) {
            self.assign_expr_to_slot(*slot, None, arg)?;
        }
        if self.inline_call_stack.contains(&index) {
            self.callable_bindings = callable_snapshot;
            return Err(CompileError::InlineFunctionRecursion(format!(
                "recursive RustScript function call detected for function index {}",
                index
            )));
        }
        self.inline_call_stack.push(index);
        let result = (|| -> Result<(), CompileError> {
            self.compile_stmts(&function_impl.body_stmts)?;
            self.compile_expr(&function_impl.body_expr)
        })();
        self.inline_call_stack.pop();
        self.callable_bindings = callable_snapshot;
        result?;
        self.emit_inline_frame_clears(&frame_slots)?;
        Ok(())
    }

    fn compile_inline_closure_call(
        &mut self,
        closure: &ClosureExpr,
        args: &[Expr],
    ) -> Result<(), CompileError> {
        if closure.param_slots.len() != args.len() {
            return Err(CompileError::CallableArityMismatch {
                expected: closure.param_slots.len(),
                got: args.len(),
            });
        }
        let frame_slots = collect_closure_frame_slots(closure);
        let callable_snapshot = self.callable_bindings.clone();
        for (arg, slot) in args.iter().zip(closure.param_slots.iter()) {
            self.assign_expr_to_slot(*slot, None, arg)?;
        }
        let result = self.compile_expr(&closure.body);
        self.callable_bindings = callable_snapshot;
        result?;
        self.emit_inline_frame_clears(&frame_slots)?;
        Ok(())
    }

    fn compile_direct_call(&mut self, index: u16, args: &[Expr]) -> Result<(), CompileError> {
        for arg in args {
            self.compile_scalar_expr(arg)?;
        }
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

    fn compile_callable_call(
        &mut self,
        callable: CallableBinding,
        args: &[Expr],
    ) -> Result<(), CompileError> {
        match callable {
            CallableBinding::Closure(closure) => self.compile_inline_closure_call(&closure, args),
            CallableBinding::Function(index) => self.compile_function_call(index, args),
        }
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
        self.assembler.push_const(Value::Null);
        self.assembler.stloc(operand);
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

    fn emit_inline_frame_clears(&mut self, slots: &[LocalSlot]) -> Result<(), CompileError> {
        for slot in slots {
            self.assembler.push_const(Value::Null);
            self.emit_stloc(*slot)?;
            self.type_state.set(*slot, typing::BoundType::Null);
        }
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

fn local_slot_operand(index: LocalSlot) -> Result<u8, CompileError> {
    u8::try_from(index).map_err(|_| CompileError::LocalSlotOverflow(index))
}

fn collect_function_frame_slots(function_impl: &FunctionImpl) -> Vec<LocalSlot> {
    let mut slots = BTreeSet::new();
    for slot in &function_impl.param_slots {
        slots.insert(*slot);
    }
    for stmt in &function_impl.body_stmts {
        collect_stmt_slot_footprint(stmt, &mut slots);
    }
    collect_expr_slot_footprint(&function_impl.body_expr, &mut slots);
    for (_, captured_slot) in &function_impl.capture_copies {
        slots.remove(captured_slot);
    }
    let mut out = slots.into_iter().collect::<Vec<_>>();
    out.sort_unstable_by(|lhs, rhs| rhs.cmp(lhs));
    out
}

fn collect_closure_frame_slots(closure: &ClosureExpr) -> Vec<LocalSlot> {
    let mut slots = BTreeSet::new();
    for slot in &closure.param_slots {
        slots.insert(*slot);
    }
    collect_expr_slot_footprint(&closure.body, &mut slots);
    for (_, captured_slot) in &closure.capture_copies {
        slots.remove(captured_slot);
    }
    let mut out = slots.into_iter().collect::<Vec<_>>();
    out.sort_unstable_by(|lhs, rhs| rhs.cmp(lhs));
    out
}

fn collect_stmt_slot_footprint(stmt: &Stmt, slots: &mut BTreeSet<LocalSlot>) {
    match stmt {
        Stmt::Noop { .. } | Stmt::FuncDecl { .. } | Stmt::Break { .. } | Stmt::Continue { .. } => {}
        Stmt::Drop { index, .. } => {
            slots.insert(*index);
        }
        Stmt::Let { index, expr, .. } | Stmt::Assign { index, expr, .. } => {
            slots.insert(*index);
            collect_expr_slot_footprint(expr, slots);
        }
        Stmt::ClosureLet { closure, .. } => {
            for slot in &closure.param_slots {
                slots.insert(*slot);
            }
            for (source_slot, captured_slot) in &closure.capture_copies {
                slots.insert(*source_slot);
                slots.insert(*captured_slot);
            }
            collect_expr_slot_footprint(&closure.body, slots);
        }
        Stmt::Expr { expr, .. } => collect_expr_slot_footprint(expr, slots),
        Stmt::IfElse {
            condition,
            then_branch,
            else_branch,
            ..
        } => {
            collect_expr_slot_footprint(condition, slots);
            for stmt in then_branch {
                collect_stmt_slot_footprint(stmt, slots);
            }
            for stmt in else_branch {
                collect_stmt_slot_footprint(stmt, slots);
            }
        }
        Stmt::For {
            init,
            condition,
            post,
            body,
            ..
        } => {
            collect_stmt_slot_footprint(init, slots);
            collect_expr_slot_footprint(condition, slots);
            collect_stmt_slot_footprint(post, slots);
            for stmt in body {
                collect_stmt_slot_footprint(stmt, slots);
            }
        }
        Stmt::While {
            condition, body, ..
        } => {
            collect_expr_slot_footprint(condition, slots);
            for stmt in body {
                collect_stmt_slot_footprint(stmt, slots);
            }
        }
    }
}

fn collect_expr_slot_footprint(expr: &Expr, slots: &mut BTreeSet<LocalSlot>) {
    match expr {
        Expr::Null
        | Expr::Int(_)
        | Expr::Float(_)
        | Expr::Bool(_)
        | Expr::Bytes(_)
        | Expr::String(_)
        | Expr::FunctionRef(_) => {}
        Expr::Var(index) | Expr::MoveVar(index) => {
            slots.insert(*index);
        }
        Expr::MoveField { root, .. } | Expr::MoveIndex { root, .. } => {
            slots.insert(*root);
        }
        Expr::OptionalGet {
            container,
            key,
            container_slot,
            key_slot,
        } => {
            slots.insert(*container_slot);
            slots.insert(*key_slot);
            collect_expr_slot_footprint(container, slots);
            collect_expr_slot_footprint(key, slots);
        }
        Expr::OptionUnwrapOr {
            value,
            value_slot,
            fallback,
        } => {
            slots.insert(*value_slot);
            collect_expr_slot_footprint(value, slots);
            collect_expr_slot_footprint(fallback, slots);
        }
        Expr::Call(_, _, args) => {
            for arg in args {
                collect_expr_slot_footprint(arg, slots);
            }
        }
        Expr::LocalCall(index, _, args) => {
            slots.insert(*index);
            for arg in args {
                collect_expr_slot_footprint(arg, slots);
            }
        }
        Expr::Closure(closure) => {
            for slot in &closure.param_slots {
                slots.insert(*slot);
            }
            for (source_slot, captured_slot) in &closure.capture_copies {
                slots.insert(*source_slot);
                slots.insert(*captured_slot);
            }
            collect_expr_slot_footprint(&closure.body, slots);
        }
        Expr::ClosureCall(closure, args) => {
            for slot in &closure.param_slots {
                slots.insert(*slot);
            }
            for (source_slot, captured_slot) in &closure.capture_copies {
                slots.insert(*source_slot);
                slots.insert(*captured_slot);
            }
            for arg in args {
                collect_expr_slot_footprint(arg, slots);
            }
            collect_expr_slot_footprint(&closure.body, slots);
        }
        Expr::Add(lhs, rhs)
        | Expr::Sub(lhs, rhs)
        | Expr::Mul(lhs, rhs)
        | Expr::Div(lhs, rhs)
        | Expr::Mod(lhs, rhs)
        | Expr::And(lhs, rhs)
        | Expr::Or(lhs, rhs)
        | Expr::Eq(lhs, rhs)
        | Expr::Lt(lhs, rhs)
        | Expr::Gt(lhs, rhs) => {
            collect_expr_slot_footprint(lhs, slots);
            collect_expr_slot_footprint(rhs, slots);
        }
        Expr::Neg(inner)
        | Expr::Not(inner)
        | Expr::ToOwned(inner)
        | Expr::Borrow(inner)
        | Expr::BorrowMut(inner) => collect_expr_slot_footprint(inner, slots),
        Expr::IfElse {
            condition,
            then_expr,
            else_expr,
        } => {
            collect_expr_slot_footprint(condition, slots);
            collect_expr_slot_footprint(then_expr, slots);
            collect_expr_slot_footprint(else_expr, slots);
        }
        Expr::Match {
            value_slot,
            result_slot,
            value,
            arms,
            default,
        } => {
            slots.insert(*value_slot);
            slots.insert(*result_slot);
            collect_expr_slot_footprint(value, slots);
            for (_, arm_expr) in arms {
                collect_expr_slot_footprint(arm_expr, slots);
            }
            collect_expr_slot_footprint(default, slots);
        }
        Expr::Block { stmts, expr } => {
            for stmt in stmts {
                collect_stmt_slot_footprint(stmt, slots);
            }
            collect_expr_slot_footprint(expr, slots);
        }
    }
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
