//! Classify named script functions by whether they require a runtime
//! `Value::Callable` identity (materialization).
//!
//! The classification is keyed by the resolved flat function index assigned
//! during semantic module merge — never by source name — so same-named
//! declarations in independent modules classify independently. Codegen
//! consumes the classification when allocating hidden callable slots:
//! direct-only functions are lowered by the direct script-call opcode with
//! no hidden slot, and every function that needs materialization keeps a
//! hidden callable slot bound at frame entry.
//!
//! # Flow model
//!
//! The classification is computed by one authoritative IR visitor plus a
//! small monotone fixed-point dataflow:
//!
//! - The visitor handles every [`Expr`]/[`Stmt`] variant in exactly one
//!   place and emits the semantic events: named function values
//!   (`referenced_as_value`), statically resolved calls (`called_directly`),
//!   per-frame slot-flow records, call sites with argument provenance, and
//!   closure/capture boundaries. New IR variants must be added to the
//!   visitor; there are no parallel walkers that can drift.
//! - Each execution frame (program root, named function body, closure body)
//!   owns a slot-value flow: which named functions can occupy which local
//!   slots, which slots are invoked through `Expr::LocalCall`, and which
//!   call sites pass which argument provenance into which callee.
//! - A dynamic callable target is an invocation of a tracked slot
//!   (`LocalCall`), or an argument that provably reaches an invoked
//!   parameter slot of a known callee (named function or closure), tracked
//!   transitively across frames. Passing a function value to an opaque
//!   callee (host/builtin) or storing it in a container only marks
//!   `referenced_as_value`; it never claims `dynamic_target_required`
//!   without tracked flow to an invocation. This keeps
//!   `requires_callable_slot` sound: every function value in the merged IR
//!   originates from an `Expr::FunctionRef` node, so `referenced_as_value`
//!   is always set where a dynamic target could be.
//! - Callable provenance that the flow record cannot enumerate — call
//!   results, container reads, closures in value position, and slot values
//!   that are not classified script functions — is tracked as *unknown*
//!   per slot, and crosses the same alias, parameter, and capture edges as
//!   tracked values. A dynamic invocation may claim that an argument
//!   provably avoids a dynamic target (`Some(false)`) only when the callee
//!   set is complete and every possible callee is known not to invoke the
//!   parameter; unknown provenance keeps the propagation conservative.
//! - Captures copy values across frame boundaries (closures at creation
//!   time, named functions at frame entry); the fixed point seeds capture
//!   slots from the declaring frame's flow and translates invocations of a
//!   captured slot back to its source slot, so a captured callable invoked
//!   from inside a closure is attributed to the slot that held it.
//! - `runtime_self_required` only fires for recursion that executes in the
//!   function's own frame: a statically resolved self-call in the function's
//!   executable body (blocks, branches and loops are the same frame; closure
//!   bodies are not), or a dynamic invocation of the function's own value
//!   reachable from its frame (stored value invoked through `LocalCall`, or
//!   the value passed to a callee that invokes its parameter).
//!
//! # Cost
//!
//! Classification runs once per compilation on the merged IR: one full IR
//! walk plus a monotone fixed point over frames, slots, and call sites. The
//! fixed point terminates because every lattice (slot values, invoked
//! slots, closure values, invoked parameters) only grows and is bounded by
//! the merged IR size; there is no O(function × IR) rescanning. This is
//! pure metadata production; codegen consumes `requires_callable_slot`
//! when counting callable slots and assigning hidden callable locals.

use std::collections::{BTreeSet, HashMap, HashSet};

use super::ir::{ClosureExpr, Expr, FrontendIr, LocalSlot, Stmt};

/// Semantic facts about how one named script function is used across the
/// whole merged compilation.
///
/// Compiler-internal metadata for the hidden callable slot allocation
/// decision; not part of the public API.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct CallableUseFacts {
    /// The function is invoked through a statically resolved call site.
    pub called_directly: bool,
    /// The function value appears in the value domain (`Expr::FunctionRef`),
    /// for example stored into a local, a map, or an array.
    pub referenced_as_value: bool,
    /// The function is exported under the `ExportedCallable` contract.
    pub exported: bool,
    /// The function captures an environment (declaration-time capture cells).
    pub captures_environment: bool,
    /// A dynamic call site can reach this function through tracked value
    /// flow: the function value is stored into a slot that is invoked
    /// (`Expr::LocalCall`), or it is passed as an argument to a parameter of
    /// a known callee that is itself dynamically invoked.
    pub dynamic_target_required: bool,
    /// The function's own runtime callable identity must be bound at frame
    /// entry (capturing or dynamic recursion path).
    pub runtime_self_required: bool,
}

impl CallableUseFacts {
    /// Single decision derived from the semantic facts: does this function
    /// need a hidden callable local slot?
    ///
    /// Plain direct calls — including non-capturing direct recursion — do
    /// not require a slot; the direct script-call opcode lowers them by
    /// prototype ID. Every other fact forces materialization into a hidden
    /// callable slot that the runtime frame binds at entry.
    pub fn requires_callable_slot(&self) -> bool {
        self.referenced_as_value
            || self.exported
            || self.captures_environment
            || self.dynamic_target_required
            || self.runtime_self_required
    }
}

/// One observed classification entry for a resolved flat function identity,
/// produced by the production pipeline (parse -> module merge -> lifetime ->
/// classification -> Compiler) and attached to [`CompiledProgram`] so the
/// crate's unit tests can assert the facts the compiler actually received.
///
/// Compiled into unit-test builds only; never part of the public API.
#[cfg(test)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CallableUseObservation {
    /// Resolved flat function index (the classification key).
    pub function_index: u16,
    /// Merged declaration name, carried only so tests can identify the
    /// entry; classification itself never keys by name.
    pub name: String,
    pub facts: CallableUseFacts,
}

/// Classify every named script function in the merged IR.
///
/// Facts are keyed by the resolved flat function index (the identity the
/// linker assigned through `SymbolId` remapping), never by source name.
pub(crate) fn classify_named_callables(ir: &FrontendIr) -> HashMap<u16, CallableUseFacts> {
    let mut classifier = Classifier::new(ir);
    classifier.classify(ir);
    classifier.facts
}

/// Argument value provenance: the named function values an expression
/// directly evaluates to, the slots it reads, and whether it can also
/// evaluate to a callable whose identity is not tracked.
#[derive(Clone, Debug, Default)]
struct ArgFlow {
    functions: BTreeSet<u16>,
    slots: BTreeSet<LocalSlot>,
    /// The expression can evaluate to a callable the flow record cannot
    /// enumerate (call results, container reads, closures in value
    /// position). A slot seeded with such a flow has an incomplete callee
    /// set and may never be claimed to provably avoid invoking a parameter.
    unknown: bool,
}

/// A statically resolved call site with per-argument provenance.
#[derive(Clone, Debug)]
struct CallSite {
    callee: u16,
    args: Vec<ArgFlow>,
}

/// A closure invocation with per-argument provenance.
#[derive(Clone, Debug)]
struct ClosureCallSite {
    callee_frame: usize,
    args: Vec<ArgFlow>,
}

/// A dynamic invocation of a local slot with per-argument provenance.
#[derive(Clone, Debug)]
struct LocalCallSite {
    slot: LocalSlot,
    args: Vec<ArgFlow>,
}

/// One execution frame's slot-flow records: the program root, a named
/// function body, or a closure body. Slot numbers are frame-relative; the
/// fixed point never mixes slots across frames except through the explicit
/// capture mappings.
#[derive(Default)]
struct FrameFlow {
    /// The named function whose body this frame executes (`None` for the
    /// program root and closure bodies).
    function: Option<u16>,
    /// Parameter slots of this frame (named functions and closures).
    params: Vec<LocalSlot>,
    /// Slots that directly received named function values.
    seeds: HashMap<LocalSlot, BTreeSet<u16>>,
    /// Slot aliases: `target` receives the values of every `source`.
    aliases: HashMap<LocalSlot, BTreeSet<LocalSlot>>,
    /// Slots invoked through `Expr::LocalCall`.
    local_calls: BTreeSet<LocalSlot>,
    /// LocalCall sites with arguments.
    local_call_sites: Vec<LocalCallSite>,
    /// Named call sites.
    call_sites: Vec<CallSite>,
    /// Closure call sites.
    closure_call_sites: Vec<ClosureCallSite>,
    /// Closures created in this frame: (child frame, capture copies).
    closures_created: Vec<(usize, Vec<(LocalSlot, LocalSlot)>)>,
    /// Closure frames stored into slots (rebinds union).
    closure_slots: HashMap<LocalSlot, Vec<usize>>,
    /// Slots that received values whose callable provenance is untracked
    /// (call results, container reads): their callee sets are incomplete.
    unknown: HashSet<LocalSlot>,
}

/// The classification pass: one authoritative visitor plus a monotone
/// fixed-point dataflow over per-frame slot flows.
struct Classifier {
    facts: HashMap<u16, CallableUseFacts>,
    frames: Vec<FrameFlow>,
    /// Functions that call themselves from their own executable frame.
    direct_self: HashSet<u16>,
    /// Named-function body frame per function index.
    function_frames: HashMap<u16, usize>,
    /// Captures per named function: (body frame, capture copies).
    function_captures: HashMap<u16, (usize, Vec<(LocalSlot, LocalSlot)>)>,
    /// Frame that declares each function (capture sources live there).
    decl_frames: HashMap<u16, usize>,
    /// Fixed-point state: slot contents per frame.
    values: Vec<HashMap<LocalSlot, BTreeSet<u16>>>,
    /// Fixed-point state: slots whose contents reach a dynamic callable
    /// target.
    invoked: Vec<BTreeSet<LocalSlot>>,
    /// Fixed-point state: closure frames per slot (alias-closed).
    closure_values: Vec<HashMap<LocalSlot, Vec<usize>>>,
    /// Fixed-point state: parameter slots that reach a dynamic callable
    /// target.
    dyn_params: Vec<BTreeSet<LocalSlot>>,
    /// Fixed-point state: slots with unknown callable provenance per frame.
    unknown_values: Vec<HashSet<LocalSlot>>,
}

impl Classifier {
    fn new(ir: &FrontendIr) -> Self {
        let mut facts: HashMap<u16, CallableUseFacts> = ir
            .function_impls
            .keys()
            .map(|&index| (index, CallableUseFacts::default()))
            .collect();
        for decl in &ir.functions {
            if let Some(fact) = facts.get_mut(&decl.index) {
                fact.exported = decl.exported;
            }
        }
        for (index, function_impl) in &ir.function_impls {
            if let Some(fact) = facts.get_mut(index) {
                fact.captures_environment = !function_impl.capture_copies.is_empty();
            }
        }
        Self {
            facts,
            frames: vec![FrameFlow::default()],
            direct_self: HashSet::new(),
            function_frames: HashMap::new(),
            function_captures: HashMap::new(),
            decl_frames: HashMap::new(),
            values: Vec::new(),
            invoked: Vec::new(),
            closure_values: Vec::new(),
            dyn_params: Vec::new(),
            unknown_values: Vec::new(),
        }
    }

    fn classify(&mut self, ir: &FrontendIr) {
        // Create every named-function frame up front so call sites in any
        // body can resolve callee frames regardless of walk order.
        let mut function_impls = ir.function_impls.iter().collect::<Vec<_>>();
        function_impls.sort_unstable_by_key(|(index, _)| **index);
        for (index, function_impl) in &function_impls {
            let frame = self.frames.len();
            self.frames.push(FrameFlow {
                function: Some(**index),
                params: function_impl.param_slots.clone(),
                ..FrameFlow::default()
            });
            self.function_frames.insert(**index, frame);
        }
        for (index, function_impl) in &function_impls {
            let frame = self.function_frames[index];
            for stmt in &function_impl.body_stmts {
                self.stmt(frame, stmt);
            }
            self.expr(frame, &function_impl.body_expr);
            self.function_captures
                .insert(**index, (frame, function_impl.capture_copies.clone()));
        }
        for stmt in &ir.stmts {
            self.stmt(0, stmt);
        }
        self.fixed_point();
        self.attribute();
    }

    /// Authoritative statement visitor. Every [`Stmt`] variant is handled
    /// here exactly once.
    fn stmt(&mut self, frame: usize, stmt: &Stmt) {
        match stmt {
            Stmt::Noop { .. } | Stmt::Break { .. } | Stmt::Continue { .. } | Stmt::Drop { .. } => {}
            Stmt::Let { index, expr, .. } | Stmt::Assign { index, expr, .. } => {
                let mut flow = self.value_flow(expr);
                if matches!(expr, Expr::Closure(_)) {
                    // A directly assigned closure is fully tracked through
                    // `closure_slots` below, so the slot's callee set stays
                    // complete.
                    flow.unknown = false;
                }
                self.seed_slot(frame, *index, &flow);
                if let Expr::Closure(closure) = expr {
                    let child = self.closure(frame, closure);
                    self.frames[frame]
                        .closure_slots
                        .entry(*index)
                        .or_default()
                        .push(child);
                } else {
                    self.expr(frame, expr);
                }
            }
            Stmt::ClosureLet { closure, .. } => {
                self.closure(frame, closure);
            }
            Stmt::FuncDecl { index, .. } => {
                self.decl_frames.entry(*index).or_insert(frame);
            }
            Stmt::Expr { expr, .. } => self.expr(frame, expr),
            Stmt::IfElse {
                condition,
                then_branch,
                else_branch,
                ..
            } => {
                self.expr(frame, condition);
                for stmt in then_branch {
                    self.stmt(frame, stmt);
                }
                for stmt in else_branch {
                    self.stmt(frame, stmt);
                }
            }
            Stmt::For {
                init,
                condition,
                post,
                body,
                ..
            } => {
                self.stmt(frame, init);
                self.expr(frame, condition);
                self.stmt(frame, post);
                for stmt in body {
                    self.stmt(frame, stmt);
                }
            }
            Stmt::While {
                condition, body, ..
            } => {
                self.expr(frame, condition);
                for stmt in body {
                    self.stmt(frame, stmt);
                }
            }
        }
    }

    /// Authoritative expression visitor. Every [`Expr`] variant is handled
    /// here exactly once; nested statements in blocks and closure bodies are
    /// routed back through [`Self::stmt`] / [`Self::closure`].
    fn expr(&mut self, frame: usize, expr: &Expr) {
        match expr {
            Expr::Null
            | Expr::Int(_)
            | Expr::Float(_)
            | Expr::Bool(_)
            | Expr::String(_)
            | Expr::Bytes(_) => {}
            Expr::FunctionRef(index, _) => {
                if let Some(fact) = self.facts.get_mut(index) {
                    fact.referenced_as_value = true;
                }
            }
            // The classification runs on merged IR where module function
            // references are already lowered to plain `Expr::FunctionRef`
            // and `Expr::Call`; unresolved refs are rejected before this
            // point. Only argument expressions can still be visited here.
            Expr::ModuleFunctionRef(..) | Expr::UnresolvedFunctionRef { .. } => {}
            Expr::ModuleCall(_, _, args, _) => {
                for arg in args {
                    self.expr(frame, arg);
                }
            }
            Expr::OptionalGet { container, key, .. } => {
                self.expr(frame, container);
                self.expr(frame, key);
            }
            Expr::OptionUnwrapOr {
                value, fallback, ..
            } => {
                self.expr(frame, value);
                self.expr(frame, fallback);
            }
            Expr::Call(target, _, args, _, _) => {
                if let Some(fact) = self.facts.get_mut(target) {
                    fact.called_directly = true;
                    if self.frames[frame].function == Some(*target) {
                        self.direct_self.insert(*target);
                    }
                }
                if self.function_frames.contains_key(target) {
                    let flows = args.iter().map(|arg| self.value_flow(arg)).collect();
                    self.frames[frame].call_sites.push(CallSite {
                        callee: *target,
                        args: flows,
                    });
                }
                for arg in args {
                    self.expr(frame, arg);
                }
            }
            Expr::LocalCall(slot, _, args, _) => {
                self.frames[frame].local_calls.insert(*slot);
                if !args.is_empty() {
                    let flows = args.iter().map(|arg| self.value_flow(arg)).collect();
                    self.frames[frame].local_call_sites.push(LocalCallSite {
                        slot: *slot,
                        args: flows,
                    });
                }
                for arg in args {
                    self.expr(frame, arg);
                }
            }
            Expr::Closure(closure) => {
                self.closure(frame, closure);
            }
            Expr::ClosureCall(closure, args) => {
                let callee_frame = self.closure(frame, closure);
                let flows = args.iter().map(|arg| self.value_flow(arg)).collect();
                self.frames[frame].closure_call_sites.push(ClosureCallSite {
                    callee_frame,
                    args: flows,
                });
                for arg in args {
                    self.expr(frame, arg);
                }
            }
            Expr::Add(lhs, rhs)
            | Expr::Sub(lhs, rhs)
            | Expr::Mul(lhs, rhs)
            | Expr::Div(lhs, rhs)
            | Expr::Mod(lhs, rhs)
            | Expr::Eq(lhs, rhs)
            | Expr::Lt(lhs, rhs)
            | Expr::Gt(lhs, rhs)
            | Expr::And(lhs, rhs)
            | Expr::Or(lhs, rhs) => {
                self.expr(frame, lhs);
                self.expr(frame, rhs);
            }
            Expr::Neg(inner)
            | Expr::Not(inner)
            | Expr::ToOwned(inner)
            | Expr::Borrow(inner)
            | Expr::BorrowMut(inner) => {
                self.expr(frame, inner);
            }
            Expr::Var(_) | Expr::MoveVar(_) | Expr::MoveField { .. } | Expr::MoveIndex { .. } => {}
            Expr::IfElse {
                condition,
                then_expr,
                else_expr,
            } => {
                self.expr(frame, condition);
                self.expr(frame, then_expr);
                self.expr(frame, else_expr);
            }
            Expr::Match {
                value,
                arms,
                default,
                ..
            } => {
                self.expr(frame, value);
                for (_, arm_expr) in arms {
                    self.expr(frame, arm_expr);
                }
                self.expr(frame, default);
            }
            Expr::Block { stmts, expr } => {
                for stmt in stmts {
                    self.stmt(frame, stmt);
                }
                self.expr(frame, expr);
            }
        }
    }

    /// Walk a closure body in its own frame and register the capture
    /// boundary with the creating frame. Returns the child frame index.
    fn closure(&mut self, frame: usize, closure: &ClosureExpr) -> usize {
        let child = self.frames.len();
        self.frames.push(FrameFlow {
            function: None,
            params: closure.param_slots.clone(),
            ..FrameFlow::default()
        });
        self.expr(child, &closure.body);
        self.frames[frame]
            .closures_created
            .push((child, closure.capture_copies.clone()));
        child
    }

    /// Top-level value provenance of an expression: the named function
    /// values it directly evaluates to, the slots it reads, and whether it
    /// can evaluate to a callable the flow record cannot enumerate. This is
    /// a provenance query over the value-producing shapes only (function
    /// values, slot reads, and union control flow); every other expression
    /// yields no tracked provenance, and its nested function values are
    /// still recorded by the visitor.
    fn value_flow(&self, expr: &Expr) -> ArgFlow {
        match expr {
            Expr::FunctionRef(index, _) => ArgFlow {
                functions: BTreeSet::from([*index]),
                slots: BTreeSet::new(),
                unknown: false,
            },
            Expr::Borrow(inner) | Expr::BorrowMut(inner) | Expr::ToOwned(inner) => {
                self.value_flow(inner)
            }
            Expr::Var(slot) | Expr::MoveVar(slot) => ArgFlow {
                functions: BTreeSet::new(),
                slots: BTreeSet::from([*slot]),
                unknown: false,
            },
            Expr::IfElse {
                then_expr,
                else_expr,
                ..
            } => {
                let mut flow = self.value_flow(then_expr);
                let other = self.value_flow(else_expr);
                flow.functions.extend(other.functions);
                flow.slots.extend(other.slots);
                flow.unknown |= other.unknown;
                flow
            }
            Expr::Match { arms, default, .. } => {
                let mut flow = self.value_flow(default);
                for (_, arm_expr) in arms {
                    let arm = self.value_flow(arm_expr);
                    flow.functions.extend(arm.functions);
                    flow.slots.extend(arm.slots);
                    flow.unknown |= arm.unknown;
                }
                flow
            }
            Expr::Block { stmts: _, expr } => self.value_flow(expr),
            Expr::OptionUnwrapOr {
                value, fallback, ..
            } => {
                let mut flow = self.value_flow(value);
                let other = self.value_flow(fallback);
                flow.functions.extend(other.functions);
                flow.slots.extend(other.slots);
                flow.unknown |= other.unknown;
                flow
            }
            // Call results, container reads, module references, moved
            // container fields, and closures in value position can be
            // callables whose identity the flow record cannot enumerate; a
            // slot seeded with them has an incomplete callee set. Their
            // nested function values are recorded by the visitor as value
            // references.
            Expr::ModuleCall(..)
            | Expr::ModuleFunctionRef(..)
            | Expr::UnresolvedFunctionRef { .. }
            | Expr::Call(..)
            | Expr::LocalCall(..)
            | Expr::ClosureCall(..)
            | Expr::OptionalGet { .. }
            | Expr::MoveField { .. }
            | Expr::MoveIndex { .. }
            | Expr::Closure(_) => ArgFlow {
                functions: BTreeSet::new(),
                slots: BTreeSet::new(),
                unknown: true,
            },
            // Literals and numeric/boolean operations cannot produce
            // callable values.
            _ => ArgFlow::default(),
        }
    }

    fn seed_slot(&mut self, frame: usize, slot: LocalSlot, flow: &ArgFlow) {
        if flow.unknown {
            self.frames[frame].unknown.insert(slot);
        }
        if !flow.functions.is_empty() {
            self.frames[frame]
                .seeds
                .entry(slot)
                .or_default()
                .extend(flow.functions.iter().copied());
        }
        if !flow.slots.is_empty() {
            self.frames[frame]
                .aliases
                .entry(slot)
                .or_default()
                .extend(flow.slots.iter().copied());
        }
    }

    /// Monotone fixed point over per-frame slot values, invoked slots,
    /// closure values, unknown callable provenance, and dynamically invoked
    /// parameters. Terminates because every lattice only grows.
    fn fixed_point(&mut self) {
        let frame_count = self.frames.len();
        self.values = (0..frame_count)
            .map(|frame| self.frames[frame].seeds.clone())
            .collect();
        self.invoked = (0..frame_count)
            .map(|frame| self.frames[frame].local_calls.clone())
            .collect();
        self.closure_values = (0..frame_count)
            .map(|frame| self.frames[frame].closure_slots.clone())
            .collect();
        self.unknown_values = (0..frame_count)
            .map(|frame| self.frames[frame].unknown.clone())
            .collect();
        self.dyn_params = vec![BTreeSet::new(); frame_count];

        // Frame-derived records are immutable during the fixed point; clone
        // them once so the iteration only mutates the growing lattices.
        let aliases = self
            .frames
            .iter()
            .map(|frame| frame.aliases.clone())
            .collect::<Vec<_>>();
        let frame_params = self
            .frames
            .iter()
            .map(|frame| frame.params.clone())
            .collect::<Vec<_>>();
        let call_sites = self
            .frames
            .iter()
            .map(|frame| frame.call_sites.clone())
            .collect::<Vec<_>>();
        let closure_call_sites = self
            .frames
            .iter()
            .map(|frame| frame.closure_call_sites.clone())
            .collect::<Vec<_>>();
        let local_call_sites = self
            .frames
            .iter()
            .map(|frame| frame.local_call_sites.clone())
            .collect::<Vec<_>>();
        let closures_created = (0..frame_count)
            .flat_map(|frame| {
                self.frames[frame]
                    .closures_created
                    .iter()
                    .map(move |(child, captures)| (frame, *child, captures.clone()))
            })
            .collect::<Vec<_>>();
        let function_captures = self
            .function_captures
            .iter()
            .map(|(index, (body_frame, captures))| (*index, *body_frame, captures.clone()))
            .collect::<Vec<_>>();

        let mut changed = true;
        while changed {
            changed = false;
            for frame in 0..frame_count {
                // Intra-frame alias closure for slot values: values stored
                // into an aliased slot flow into its targets.
                for (target, sources) in &aliases[frame] {
                    let mut source_values = BTreeSet::new();
                    for source in sources {
                        if let Some(values) = self.values[frame].get(source) {
                            source_values.extend(values.iter().copied());
                        }
                    }
                    if !source_values.is_empty() {
                        let target_values = self.values[frame].entry(*target).or_default();
                        for index in source_values {
                            if target_values.insert(index) {
                                changed = true;
                            }
                        }
                    }
                }
                // Reverse alias: a slot feeding an invoked slot is invoked
                // too, so its contents reach the dynamic callable target.
                for (target, sources) in &aliases[frame] {
                    if !self.invoked[frame].contains(target) {
                        continue;
                    }
                    for source in sources {
                        if self.invoked[frame].insert(*source) {
                            changed = true;
                        }
                    }
                }
                // Unknown callable provenance follows the same alias edges.
                for (target, sources) in &aliases[frame] {
                    if sources
                        .iter()
                        .any(|source| self.unknown_values[frame].contains(source))
                        && self.unknown_values[frame].insert(*target)
                    {
                        changed = true;
                    }
                }
                // Closure values follow the same alias edges.
                for (target, sources) in &aliases[frame] {
                    let mut source_closures = Vec::new();
                    for source in sources {
                        if let Some(closures) = self.closure_values[frame].get(source) {
                            source_closures.extend(closures.iter().copied());
                        }
                    }
                    if !source_closures.is_empty() {
                        let target_closures =
                            self.closure_values[frame].entry(*target).or_default();
                        for child in source_closures {
                            if !target_closures.contains(&child) {
                                target_closures.push(child);
                                changed = true;
                            }
                        }
                    }
                }
                // Invoked parameter slots reach a dynamic callable target.
                for param in &frame_params[frame] {
                    if self.invoked[frame].contains(param) && self.dyn_params[frame].insert(*param)
                    {
                        changed = true;
                    }
                }
                // Named call sites: an invoked callee parameter makes the
                // argument provenance invoked in this frame.
                for site in &call_sites[frame] {
                    let Some(&callee_frame) = self.function_frames.get(&site.callee) else {
                        continue;
                    };
                    for (arg_index, arg) in site.args.iter().enumerate() {
                        let Some(param) = frame_params[callee_frame].get(arg_index) else {
                            continue;
                        };
                        if !self.dyn_params[callee_frame].contains(param) {
                            continue;
                        }
                        for slot in &arg.slots {
                            if self.invoked[frame].insert(*slot) {
                                changed = true;
                            }
                        }
                        for index in &arg.functions {
                            if let Some(fact) = self.facts.get_mut(index) {
                                fact.dynamic_target_required = true;
                            }
                        }
                    }
                }
                // Closure call sites: same rule, plus closure parameter value
                // seeding so intra-closure aliasing sees the argument values.
                for site in &closure_call_sites[frame] {
                    for (arg_index, arg) in site.args.iter().enumerate() {
                        let Some(param) = frame_params[site.callee_frame].get(arg_index) else {
                            continue;
                        };
                        if self.dyn_params[site.callee_frame].contains(param) {
                            for slot in &arg.slots {
                                if self.invoked[frame].insert(*slot) {
                                    changed = true;
                                }
                            }
                            for index in &arg.functions {
                                if let Some(fact) = self.facts.get_mut(index) {
                                    fact.dynamic_target_required = true;
                                }
                            }
                        }
                        if self.seed_param_values(frame, site.callee_frame, *param, arg) {
                            changed = true;
                        }
                    }
                }
                // LocalCall sites: resolve statically known callees (named
                // function values in the slot, closures stored into it);
                // incomplete callee sets stay conservative.
                for site in &local_call_sites[frame] {
                    let slot_values = self.values[frame]
                        .get(&site.slot)
                        .cloned()
                        .unwrap_or_default();
                    let slot_closures = self
                        .closure_values
                        .get(frame)
                        .and_then(|closures| closures.get(&site.slot))
                        .cloned()
                        .unwrap_or_default();
                    for (arg_index, arg) in site.args.iter().enumerate() {
                        let known_invokes = callee_invokes_param(
                            site.slot,
                            frame,
                            &slot_values,
                            &slot_closures,
                            &self.function_frames,
                            &frame_params,
                            &self.dyn_params,
                            &self.unknown_values,
                            arg_index,
                        );
                        if matches!(known_invokes, Some(false)) {
                            // Known callees never invoke this parameter and
                            // the callee set is complete: the argument does
                            // not reach a dynamic target.
                            continue;
                        }
                        for slot in &arg.slots {
                            if self.invoked[frame].insert(*slot) {
                                changed = true;
                            }
                        }
                        for index in &arg.functions {
                            if let Some(fact) = self.facts.get_mut(index) {
                                fact.dynamic_target_required = true;
                            }
                        }
                        for &callee_frame in &slot_closures {
                            if let Some(param) = frame_params[callee_frame].get(arg_index)
                                && self.seed_param_values(frame, callee_frame, *param, arg)
                            {
                                changed = true;
                            }
                        }
                    }
                }
            }
            // Capture seeding across frame boundaries: closures copy values
            // from their creating frame at creation time; named functions
            // copy from their declaring frame at frame entry. An invocation
            // of a captured slot inside the child frame also invokes the
            // source slot in the creating frame (closure-escape dynamic
            // paths), translated transitively by the fixed point. Unknown
            // callable provenance crosses the same boundaries.
            for (frame, child, captures) in &closures_created {
                for (source, captured) in captures {
                    let source_values =
                        self.values[*frame].get(source).cloned().unwrap_or_default();
                    if !source_values.is_empty() {
                        let target_values = self.values[*child].entry(*captured).or_default();
                        for index in source_values {
                            if target_values.insert(index) {
                                changed = true;
                            }
                        }
                    }
                    if self.unknown_values[*frame].contains(source)
                        && self.unknown_values[*child].insert(*captured)
                    {
                        changed = true;
                    }
                    if self.invoked[*child].contains(captured)
                        && self.invoked[*frame].insert(*source)
                    {
                        changed = true;
                    }
                }
            }
            for (index, body_frame, captures) in &function_captures {
                let decl_frame = self.decl_frames.get(index).copied().unwrap_or(0);
                for (source, captured) in captures {
                    let source_values = self.values[decl_frame]
                        .get(source)
                        .cloned()
                        .unwrap_or_default();
                    if !source_values.is_empty() {
                        let target_values = self.values[*body_frame].entry(*captured).or_default();
                        for value in source_values {
                            if target_values.insert(value) {
                                changed = true;
                            }
                        }
                    }
                    if self.unknown_values[decl_frame].contains(source)
                        && self.unknown_values[*body_frame].insert(*captured)
                    {
                        changed = true;
                    }
                    if self.invoked[*body_frame].contains(captured)
                        && self.invoked[decl_frame].insert(*source)
                    {
                        changed = true;
                    }
                }
            }
        }
    }

    /// Seed a callee's parameter slot with the argument's value provenance
    /// (direct function values plus the caller slot contents) and unknown
    /// callable provenance. Returns whether either lattice grew.
    fn seed_param_values(
        &mut self,
        caller_frame: usize,
        callee_frame: usize,
        param: LocalSlot,
        arg: &ArgFlow,
    ) -> bool {
        let mut changed = false;
        if (arg.unknown
            || arg
                .slots
                .iter()
                .any(|slot| self.unknown_values[caller_frame].contains(slot)))
            && self.unknown_values[callee_frame].insert(param)
        {
            changed = true;
        }
        let mut param_values = arg.functions.clone();
        for slot in &arg.slots {
            if let Some(slot_values) = self.values[caller_frame].get(slot) {
                param_values.extend(slot_values.iter().copied());
            }
        }
        if param_values.is_empty() {
            return changed;
        }
        let target = self.values[callee_frame].entry(param).or_default();
        for index in param_values {
            if target.insert(index) {
                changed = true;
            }
        }
        changed
    }

    /// Derive the final facts from the fixed-point state.
    fn attribute(&mut self) {
        // Every slot whose contents reach a dynamic callable target marks
        // those contents as dynamic targets.
        let invoked = self.invoked.clone();
        for (frame, slots) in invoked.iter().enumerate() {
            for slot in slots {
                if let Some(indexes) = self.values[frame].get(slot) {
                    for index in indexes {
                        if let Some(fact) = self.facts.get_mut(index) {
                            fact.dynamic_target_required = true;
                        }
                    }
                }
            }
        }

        // Frame-local self recursion: dynamic invocations of the function's
        // own value reachable from its own frame — a stored value invoked
        // through LocalCall, or the value passed to a callee that invokes
        // its parameter.
        let frame_params = self
            .frames
            .iter()
            .map(|frame| frame.params.clone())
            .collect::<Vec<_>>();
        let mut dynamic_self = HashSet::new();
        for (index, &(body_frame, _)) in &self.function_captures {
            for slot in &self.invoked[body_frame] {
                if self
                    .values
                    .get(body_frame)
                    .and_then(|values| values.get(slot))
                    .is_some_and(|indexes| indexes.contains(index))
                {
                    dynamic_self.insert(*index);
                }
            }
            for site in &self.frames[body_frame].call_sites {
                let Some(&callee_frame) = self.function_frames.get(&site.callee) else {
                    continue;
                };
                for (arg_index, arg) in site.args.iter().enumerate() {
                    if arg.functions.contains(index)
                        && frame_params[callee_frame]
                            .get(arg_index)
                            .is_some_and(|param| self.dyn_params[callee_frame].contains(param))
                    {
                        dynamic_self.insert(*index);
                    }
                }
            }
            for site in &self.frames[body_frame].closure_call_sites {
                for (arg_index, arg) in site.args.iter().enumerate() {
                    if arg.functions.contains(index)
                        && frame_params[site.callee_frame]
                            .get(arg_index)
                            .is_some_and(|param| self.dyn_params[site.callee_frame].contains(param))
                    {
                        dynamic_self.insert(*index);
                    }
                }
            }
            for site in &self.frames[body_frame].local_call_sites {
                let slot_values = self
                    .values
                    .get(body_frame)
                    .and_then(|values| values.get(&site.slot))
                    .cloned()
                    .unwrap_or_default();
                let slot_closures = self
                    .closure_values
                    .get(body_frame)
                    .and_then(|closures| closures.get(&site.slot))
                    .cloned()
                    .unwrap_or_default();
                for (arg_index, arg) in site.args.iter().enumerate() {
                    if !arg.functions.contains(index) {
                        continue;
                    }
                    let known_invokes = callee_invokes_param(
                        site.slot,
                        body_frame,
                        &slot_values,
                        &slot_closures,
                        &self.function_frames,
                        &frame_params,
                        &self.dyn_params,
                        &self.unknown_values,
                        arg_index,
                    );
                    if !matches!(known_invokes, Some(false)) {
                        dynamic_self.insert(*index);
                    }
                }
            }
        }

        for index in self.function_captures.keys().copied().collect::<Vec<_>>() {
            let self_recursive = self.direct_self.contains(&index) || dynamic_self.contains(&index);
            if let Some(fact) = self.facts.get_mut(&index) {
                fact.runtime_self_required =
                    self_recursive && (fact.captures_environment || fact.dynamic_target_required);
            }
        }
    }
}

/// Whether any statically known callee of a local slot (named function
/// values in the slot, closures stored into it) dynamically invokes argument
/// position `arg_index`.
///
/// Returns `Some(true)` when at least one known callee invokes the
/// parameter, `Some(false)` when the callee set is complete and every
/// possible target provably does not invoke it, and `None` when the callee
/// set is incomplete — no callee is known, the slot also holds values with
/// untracked callable provenance (call results, container reads), or a slot
/// value is not a classified script function — so the caller must stay
/// conservative.
#[allow(clippy::too_many_arguments)]
fn callee_invokes_param(
    slot: LocalSlot,
    frame: usize,
    slot_values: &BTreeSet<u16>,
    slot_closures: &[usize],
    function_frames: &HashMap<u16, usize>,
    frame_params: &[Vec<LocalSlot>],
    dyn_params: &[BTreeSet<LocalSlot>],
    unknown_values: &[HashSet<LocalSlot>],
    arg_index: usize,
) -> Option<bool> {
    if slot_values.is_empty() && slot_closures.is_empty() {
        return None;
    }
    let mut any_invokes = false;
    let mut all_known = true;
    for &callee in slot_values {
        let Some(&callee_frame) = function_frames.get(&callee) else {
            // A callable value whose invocation behavior was not classified
            // (e.g. a host/builtin function value): it cannot be proven not
            // to invoke the parameter.
            all_known = false;
            continue;
        };
        if frame_params[callee_frame]
            .get(arg_index)
            .is_some_and(|param| dyn_params[callee_frame].contains(param))
        {
            any_invokes = true;
        }
    }
    for &callee_frame in slot_closures {
        if frame_params[callee_frame]
            .get(arg_index)
            .is_some_and(|param| dyn_params[callee_frame].contains(param))
        {
            any_invokes = true;
        }
    }
    if any_invokes {
        return Some(true);
    }
    if !all_known || unknown_values[frame].contains(&slot) {
        // Incomplete callee set: a possible callee with unknown invocation
        // behavior keeps the propagation conservative.
        return None;
    }
    Some(false)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use crate::ValueType;

    use super::super::ir::{AssignmentKind, FunctionDecl, FunctionImpl, LocalSlot, MatchPattern};
    use super::super::linker::{ParsedUnit, merge_units};
    use super::super::modules::{ModuleId, SymbolId};
    use super::*;

    fn decl(index: u16, name: &str, exported: bool, symbol: Option<SymbolId>) -> FunctionDecl {
        FunctionDecl {
            name: name.to_string(),
            arity: 0,
            index,
            args: Vec::new(),
            arg_schemas: Vec::new(),
            return_schema: None,
            type_params: Vec::new(),
            exported,
            return_type: ValueType::Int,
            symbol,
        }
    }

    fn impl_with(
        capture_copies: Vec<(LocalSlot, LocalSlot)>,
        body_stmts: Vec<Stmt>,
        body_expr: Expr,
    ) -> FunctionImpl {
        impl_with_params(Vec::new(), capture_copies, body_stmts, body_expr)
    }

    fn impl_with_params(
        param_slots: Vec<LocalSlot>,
        capture_copies: Vec<(LocalSlot, LocalSlot)>,
        body_stmts: Vec<Stmt>,
        body_expr: Expr,
    ) -> FunctionImpl {
        FunctionImpl {
            param_slots,
            capture_copies,
            body_stmts,
            body_expr,
            body_expr_line: 1,
        }
    }

    fn ir_with(
        stmts: Vec<Stmt>,
        functions: Vec<FunctionDecl>,
        function_impls: HashMap<u16, FunctionImpl>,
    ) -> FrontendIr {
        FrontendIr {
            stmts,
            locals: 0,
            local_bindings: Vec::new(),
            struct_schemas: HashMap::new(),
            unknown_type_spans: Vec::new(),
            functions,
            function_impls,
            stmt_sources: Vec::new(),
            function_sources: HashMap::new(),
            use_declarations: Vec::new(),
            implicit_extern_names: Vec::new(),
            host_api_metadata: None,
            semantic_index: None,
            parsed_semantic_index: None,
            catalog_visibility: None,
            lexer_tokens: Vec::new(),
        }
    }

    fn call(index: u16) -> Expr {
        Expr::Call(index, Vec::new(), Vec::new(), None, None)
    }

    fn func_decl_stmt(name: &str, index: u16) -> Stmt {
        Stmt::FuncDecl {
            name: name.to_string(),
            index,
            arity: 0,
            args: Vec::new(),
            exported: false,
            has_impl: true,
            line: 1,
        }
    }

    fn expr_stmt(expr: Expr) -> Stmt {
        Stmt::Expr { expr, line: 1 }
    }

    fn let_stmt(slot: LocalSlot, expr: Expr) -> Stmt {
        Stmt::Let {
            index: slot,
            declared_schema: None,
            expr,
            line: 1,
        }
    }

    #[test]
    fn materialization_direct_only_helper_needs_no_callable_slot() {
        // `helper` is only ever invoked through statically resolved calls
        // (from the root and from `caller`). No value reference, no export,
        // no captures: it must not require a callable slot.
        let helper_impl = impl_with(Vec::new(), Vec::new(), Expr::Int(1));
        let caller_impl = impl_with(Vec::new(), Vec::new(), call(0));
        let ir = ir_with(
            vec![
                func_decl_stmt("helper", 0),
                func_decl_stmt("caller", 1),
                expr_stmt(call(0)),
                expr_stmt(call(1)),
            ],
            vec![
                decl(0, "helper", false, None),
                decl(1, "caller", false, None),
            ],
            HashMap::from([(0, helper_impl), (1, caller_impl)]),
        );

        let facts = classify_named_callables(&ir);
        let helper = facts[&0];
        assert!(helper.called_directly);
        assert!(!helper.referenced_as_value);
        assert!(!helper.exported);
        assert!(!helper.captures_environment);
        assert!(!helper.dynamic_target_required);
        assert!(!helper.runtime_self_required);
        assert!(!helper.requires_callable_slot());
        assert!(facts[&1].called_directly);
    }

    #[test]
    fn materialization_exported_direct_helper_requires_slot() {
        let ir = ir_with(
            vec![func_decl_stmt("helper", 0), expr_stmt(call(0))],
            vec![decl(0, "helper", true, None)],
            HashMap::from([(0, impl_with(Vec::new(), Vec::new(), Expr::Int(1)))]),
        );

        let helper = classify_named_callables(&ir)[&0];
        assert!(helper.called_directly);
        assert!(helper.exported);
        assert!(helper.requires_callable_slot());
    }

    #[test]
    fn materialization_value_referenced_local_requires_slot() {
        // `let stored = helper;` puts the function value into the value
        // domain even though nothing invokes it dynamically.
        let ir = ir_with(
            vec![
                func_decl_stmt("helper", 0),
                expr_stmt(call(0)),
                let_stmt(10, Expr::FunctionRef(0, Vec::new())),
            ],
            vec![decl(0, "helper", false, None)],
            HashMap::from([(0, impl_with(Vec::new(), Vec::new(), Expr::Int(1)))]),
        );

        let helper = classify_named_callables(&ir)[&0];
        assert!(helper.called_directly);
        assert!(helper.referenced_as_value);
        assert!(!helper.dynamic_target_required);
        assert!(helper.requires_callable_slot());
    }

    #[test]
    fn materialization_container_storage_keeps_materialization_without_dynamic_target() {
        // `list.push(helper)` flows the function value into a container
        // through an opaque callee. The value is referenced and materialized,
        // but no tracked value flow reaches an actual dynamic callable
        // target, so `dynamic_target_required` stays false (F6 precision);
        // materialization is preserved through `referenced_as_value`.
        let push = Expr::Call(
            200,
            Vec::new(),
            vec![Expr::Var(11), Expr::FunctionRef(0, Vec::new())],
            None,
            None,
        );
        let ir = ir_with(
            vec![func_decl_stmt("helper", 0), let_stmt(12, push)],
            vec![decl(0, "helper", false, None)],
            HashMap::from([(0, impl_with(Vec::new(), Vec::new(), Expr::Int(1)))]),
        );

        let helper = classify_named_callables(&ir)[&0];
        assert!(helper.referenced_as_value);
        assert!(!helper.dynamic_target_required);
        assert!(helper.requires_callable_slot());
    }

    #[test]
    fn materialization_preserves_annotated_call_resolution() {
        use crate::compiler::ir::TypeSchema as IrTypeSchema;
        use crate::compiler::{ResolvedHostCall, ResolvedHostParam};
        use crate::host_api::{HostApiFingerprint, HostParamPassing};
        fn fingerprint(n: u64) -> HostApiFingerprint {
            serde_json::from_value(serde_json::Value::Number(n.into())).unwrap()
        }
        let resolution = ResolvedHostCall {
            name: "read".to_string(),
            params: vec![ResolvedHostParam {
                name: "x".to_string(),
                schema: IrTypeSchema::Int,
            }],
            return_type: IrTypeSchema::Int,
            passing: vec![HostParamPassing::Borrow],
            fingerprint: fingerprint(1),
        };
        let annotated = Expr::Call(0, Vec::new(), Vec::new(), Some(Box::new(resolution)), None);
        let ir = ir_with(
            vec![func_decl_stmt("helper", 0), expr_stmt(annotated.clone())],
            vec![decl(0, "helper", false, None)],
            HashMap::from([(0, impl_with(Vec::new(), Vec::new(), Expr::Int(1)))]),
        );
        // The materialization classifier must accept an annotated call and
        // the clone it receives must keep the resolution.
        let facts = classify_named_callables(&ir);
        assert!(facts.contains_key(&0));
        let Expr::Call(_, _, _, resolution_after, _) = &annotated else {
            panic!("expected a Call");
        };
        assert_eq!(resolution_after.as_deref().unwrap().name, "read");
    }

    #[test]
    fn materialization_locally_stored_value_called_dynamically_requires_dynamic_target() {
        // The stored function value is invoked through `LocalCall` on the
        // local that received it: a dynamic call site can target it.
        let ir = ir_with(
            vec![
                func_decl_stmt("helper", 0),
                let_stmt(10, Expr::FunctionRef(0, Vec::new())),
                expr_stmt(Expr::LocalCall(10, Vec::new(), Vec::new(), None)),
            ],
            vec![decl(0, "helper", false, None)],
            HashMap::from([(0, impl_with(Vec::new(), Vec::new(), Expr::Int(1)))]),
        );

        let helper = classify_named_callables(&ir)[&0];
        assert!(helper.referenced_as_value);
        assert!(helper.dynamic_target_required);
        assert!(helper.requires_callable_slot());
    }

    #[test]
    fn materialization_capturing_named_function_requires_environment() {
        let ir = ir_with(
            vec![func_decl_stmt("read", 0), expr_stmt(call(0))],
            vec![decl(0, "read", false, None)],
            HashMap::from([(0, impl_with(vec![(5, 7)], Vec::new(), Expr::Int(1)))]),
        );

        let read = classify_named_callables(&ir)[&0];
        assert!(read.called_directly);
        assert!(read.captures_environment);
        assert!(read.requires_callable_slot());
    }

    #[test]
    fn materialization_noncapturing_direct_recursion_needs_no_runtime_self() {
        // `fn count() { count() }` recurses through a statically resolved
        // call and captures nothing: once the direct script-call opcode
        // exists it needs neither a slot nor a runtime self identity.
        let count_impl = impl_with(Vec::new(), Vec::new(), call(0));
        let ir = ir_with(
            vec![func_decl_stmt("count", 0), expr_stmt(call(0))],
            vec![decl(0, "count", false, None)],
            HashMap::from([(0, count_impl)]),
        );

        let count = classify_named_callables(&ir)[&0];
        assert!(count.called_directly);
        assert!(!count.captures_environment);
        assert!(!count.runtime_self_required);
        assert!(!count.requires_callable_slot());
    }

    #[test]
    fn materialization_capturing_recursion_retains_runtime_self() {
        // A capturing function that recurses directly needs its runtime self
        // identity bound at frame entry to re-enter with its environment.
        let ir = ir_with(
            vec![func_decl_stmt("walk", 0), expr_stmt(call(0))],
            vec![decl(0, "walk", false, None)],
            HashMap::from([(0, impl_with(vec![(5, 7)], Vec::new(), call(0)))]),
        );

        let walk = classify_named_callables(&ir)[&0];
        assert!(walk.called_directly);
        assert!(walk.captures_environment);
        assert!(walk.runtime_self_required);
        assert!(walk.requires_callable_slot());
    }

    #[test]
    fn materialization_same_source_name_follows_resolved_identity() {
        // Two functions both named `helper`, each with its own resolved
        // identity: classification must follow the function index, never the
        // shared source name.
        let ir = ir_with(
            vec![
                func_decl_stmt("helper", 0),
                func_decl_stmt("helper", 1),
                expr_stmt(call(0)),
                expr_stmt(call(1)),
            ],
            vec![
                decl(0, "helper", true, None),
                decl(1, "helper", false, None),
            ],
            HashMap::from([
                (0, impl_with(Vec::new(), Vec::new(), Expr::Int(1))),
                (1, impl_with(Vec::new(), Vec::new(), Expr::Int(2))),
            ]),
        );

        let facts = classify_named_callables(&ir);
        assert_eq!(facts.len(), 2);
        let exported = facts[&0];
        let direct_only = facts[&1];
        assert!(exported.exported);
        assert!(exported.requires_callable_slot());
        assert!(!direct_only.exported);
        assert!(direct_only.called_directly);
        assert!(!direct_only.requires_callable_slot());
    }

    #[test]
    fn materialization_classification_survives_module_merge_remap() {
        // Two independent modules each declare `fn helper` plus a `run` that
        // calls it. The root calls its own exported `helper` directly and
        // imports the sibling's `run` through a `ModuleCall`. After the real
        // merge pipeline remaps unit indices and symbols to flat indices,
        // classification must attribute facts to the resolved flat identity
        // of each same-named function.
        let sibling_symbol_helper = SymbolId {
            module: ModuleId(2),
            index: 0,
        };
        let sibling_symbol_run = SymbolId {
            module: ModuleId(2),
            index: 1,
        };
        let root_symbol_helper = SymbolId {
            module: ModuleId(1),
            index: 0,
        };

        let sibling_unit = ParsedUnit {
            parsed: ir_with(
                vec![func_decl_stmt("helper", 0), func_decl_stmt("run", 1)],
                vec![
                    decl(0, "helper", false, Some(sibling_symbol_helper)),
                    decl(1, "run", false, Some(sibling_symbol_run)),
                ],
                HashMap::from([
                    (0, impl_with(Vec::new(), Vec::new(), Expr::Int(11))),
                    // `run` calls the sibling's own `helper` (unit index 0).
                    (1, impl_with(Vec::new(), Vec::new(), call(0))),
                ]),
            ),
            scope_identity: Some("sibling__m2".to_string()),
            source_name: "sibling.rss".to_string(),
            module: ModuleId(2),
            source_id: 1,
        };

        let root_unit = ParsedUnit {
            parsed: ir_with(
                vec![
                    func_decl_stmt("helper", 0),
                    expr_stmt(call(0)),
                    // Imported call resolved to the sibling's `run` symbol.
                    expr_stmt(Expr::ModuleCall(
                        sibling_symbol_run,
                        Vec::new(),
                        Vec::new(),
                        None,
                    )),
                ],
                vec![decl(0, "helper", true, Some(root_symbol_helper))],
                HashMap::from([(0, impl_with(Vec::new(), Vec::new(), Expr::Int(22)))]),
            ),
            scope_identity: None,
            source_name: "main.rss".to_string(),
            module: ModuleId(1),
            source_id: 0,
        };

        let merged =
            merge_units(vec![sibling_unit, root_unit]).expect("hand-built units must merge");

        // Both same-named helpers survive as distinct flat entries; the
        // assertions below key everything by resolved identity, never by the
        // merged display name (a mangling policy change must not affect
        // them).
        assert_eq!(merged.functions.len(), 3);
        assert_eq!(merged.function_impls.len(), 3);

        let facts = classify_named_callables(&merged);
        assert_eq!(facts.len(), 3);
        for index in merged.function_impls.keys() {
            assert!(facts.contains_key(index), "every impl must be classified");
        }

        let flat_of = |symbol: SymbolId| -> u16 {
            merged
                .functions
                .iter()
                .find(|function| function.symbol == Some(symbol))
                .expect("symbol must have a flat entry")
                .index
        };

        // The two same-named helpers must classify under distinct resolved
        // flat identities.
        let root_helper_index = flat_of(root_symbol_helper);
        let sibling_helper_index = flat_of(sibling_symbol_helper);
        assert_ne!(
            root_helper_index, sibling_helper_index,
            "same-named helpers must have distinct flat identities"
        );

        // The root's exported helper (flat index from symbol remap) keeps the
        // exported fact and requires materialization.
        let root_helper = facts[&root_helper_index];
        assert!(root_helper.called_directly);
        assert!(root_helper.exported);
        assert!(root_helper.requires_callable_slot());

        // The sibling's direct-only helper (same source name, different
        // identity) is called directly by its own `run` and needs no slot.
        let sibling_helper = facts[&sibling_helper_index];
        assert!(sibling_helper.called_directly);
        assert!(!sibling_helper.exported);
        assert!(!sibling_helper.requires_callable_slot());

        // The sibling's `run` is reached from the root through the
        // symbol-resolved `ModuleCall` and is classified as called directly.
        let sibling_run = facts[&flat_of(sibling_symbol_run)];
        assert!(sibling_run.called_directly);
        assert!(!sibling_run.requires_callable_slot());
    }

    #[test]
    fn materialization_requires_callable_slot_ignores_call_count_and_spelling() {
        // The decision is a pure function of the semantic facts: many direct
        // calls still need no slot, while a single value reference does.
        let many_calls = ir_with(
            vec![
                func_decl_stmt("hot", 0),
                expr_stmt(call(0)),
                expr_stmt(call(0)),
                expr_stmt(call(0)),
            ],
            vec![decl(0, "hot", false, None)],
            HashMap::from([(0, impl_with(Vec::new(), Vec::new(), Expr::Int(1)))]),
        );
        assert!(!classify_named_callables(&many_calls)[&0].requires_callable_slot());

        let single_value_use = ir_with(
            vec![
                func_decl_stmt("hot", 0),
                let_stmt(10, Expr::FunctionRef(0, Vec::new())),
            ],
            vec![decl(0, "hot", false, None)],
            HashMap::from([(0, impl_with(Vec::new(), Vec::new(), Expr::Int(1)))]),
        );
        assert!(classify_named_callables(&single_value_use)[&0].requires_callable_slot());
    }

    #[test]
    fn materialization_facts_ignore_unrelated_statement_kinds() {
        // Assignments and drops of ordinary values must not perturb the
        // classification of an unrelated direct-only function.
        let ir = ir_with(
            vec![
                func_decl_stmt("helper", 0),
                expr_stmt(call(0)),
                let_stmt(10, Expr::Int(5)),
                Stmt::Assign {
                    kind: AssignmentKind::Set,
                    index: 10,
                    expr: Expr::Int(6),
                    line: 1,
                },
                Stmt::Drop { index: 10, line: 1 },
            ],
            vec![decl(0, "helper", false, None)],
            HashMap::from([(0, impl_with(Vec::new(), Vec::new(), Expr::Int(1)))]),
        );

        let helper = classify_named_callables(&ir)[&0];
        assert!(helper.called_directly);
        assert!(!helper.referenced_as_value);
        assert!(!helper.dynamic_target_required);
        assert!(!helper.requires_callable_slot());
    }

    // --- F1: slot-to-slot / control-flow propagation of dynamic targets ---

    #[test]
    fn materialization_slot_alias_chain_propagates_dynamic_target() {
        // `let a = helper; let b = a; b();`: the function value flows through
        // slot-to-slot aliasing before the dynamic invocation.
        let ir = ir_with(
            vec![
                func_decl_stmt("helper", 0),
                let_stmt(10, Expr::FunctionRef(0, Vec::new())),
                let_stmt(11, Expr::Var(10)),
                expr_stmt(Expr::LocalCall(11, Vec::new(), Vec::new(), None)),
            ],
            vec![decl(0, "helper", false, None)],
            HashMap::from([(0, impl_with(Vec::new(), Vec::new(), Expr::Int(1)))]),
        );

        let helper = classify_named_callables(&ir)[&0];
        assert!(helper.referenced_as_value);
        assert!(helper.dynamic_target_required);
    }

    #[test]
    fn materialization_move_var_alias_propagates_dynamic_target() {
        // `let a = helper; let b = move a; b();`: moved values keep flowing.
        let ir = ir_with(
            vec![
                func_decl_stmt("helper", 0),
                let_stmt(10, Expr::FunctionRef(0, Vec::new())),
                let_stmt(11, Expr::MoveVar(10)),
                expr_stmt(Expr::LocalCall(11, Vec::new(), Vec::new(), None)),
            ],
            vec![decl(0, "helper", false, None)],
            HashMap::from([(0, impl_with(Vec::new(), Vec::new(), Expr::Int(1)))]),
        );

        let helper = classify_named_callables(&ir)[&0];
        assert!(helper.dynamic_target_required);
    }

    #[test]
    fn materialization_ifelse_branch_values_propagate_dynamic_target() {
        // `let x = if c { helper } else { other }; x();`: either branch value
        // can reach the dynamic invocation.
        let ir = ir_with(
            vec![
                func_decl_stmt("helper", 0),
                func_decl_stmt("other", 1),
                let_stmt(
                    10,
                    Expr::IfElse {
                        condition: Box::new(Expr::Bool(true)),
                        then_expr: Box::new(Expr::FunctionRef(0, Vec::new())),
                        else_expr: Box::new(Expr::FunctionRef(1, Vec::new())),
                    },
                ),
                expr_stmt(Expr::LocalCall(10, Vec::new(), Vec::new(), None)),
            ],
            vec![
                decl(0, "helper", false, None),
                decl(1, "other", false, None),
            ],
            HashMap::from([
                (0, impl_with(Vec::new(), Vec::new(), Expr::Int(1))),
                (1, impl_with(Vec::new(), Vec::new(), Expr::Int(2))),
            ]),
        );

        let facts = classify_named_callables(&ir);
        assert!(facts[&0].dynamic_target_required);
        assert!(facts[&1].dynamic_target_required);
    }

    #[test]
    fn materialization_match_arm_values_propagate_dynamic_target() {
        // `let x = match v { 1 => helper, _ => other }; x();`
        let ir = ir_with(
            vec![
                func_decl_stmt("helper", 0),
                func_decl_stmt("other", 1),
                let_stmt(
                    10,
                    Expr::Match {
                        value_slot: 20,
                        result_slot: 21,
                        value: Box::new(Expr::Int(1)),
                        arms: vec![(MatchPattern::Int(1), Expr::FunctionRef(0, Vec::new()))],
                        default: Box::new(Expr::FunctionRef(1, Vec::new())),
                    },
                ),
                expr_stmt(Expr::LocalCall(10, Vec::new(), Vec::new(), None)),
            ],
            vec![
                decl(0, "helper", false, None),
                decl(1, "other", false, None),
            ],
            HashMap::from([
                (0, impl_with(Vec::new(), Vec::new(), Expr::Int(1))),
                (1, impl_with(Vec::new(), Vec::new(), Expr::Int(2))),
            ]),
        );

        let facts = classify_named_callables(&ir);
        assert!(facts[&0].dynamic_target_required);
        assert!(facts[&1].dynamic_target_required);
    }

    #[test]
    fn materialization_block_result_propagates_dynamic_target() {
        // `let x = { helper }; x();`: the block result value flows to the slot.
        let ir = ir_with(
            vec![
                func_decl_stmt("helper", 0),
                let_stmt(
                    10,
                    Expr::Block {
                        stmts: Vec::new(),
                        expr: Box::new(Expr::FunctionRef(0, Vec::new())),
                    },
                ),
                expr_stmt(Expr::LocalCall(10, Vec::new(), Vec::new(), None)),
            ],
            vec![decl(0, "helper", false, None)],
            HashMap::from([(0, impl_with(Vec::new(), Vec::new(), Expr::Int(1)))]),
        );

        let helper = classify_named_callables(&ir)[&0];
        assert!(helper.dynamic_target_required);
    }

    #[test]
    fn materialization_rebind_alias_propagates_dynamic_target() {
        // `let a = helper; a = other; let b = a; b();`: `b` aliases `a` after
        // the rebind; the rebound value must still be attributed.
        let ir = ir_with(
            vec![
                func_decl_stmt("helper", 0),
                func_decl_stmt("other", 1),
                let_stmt(10, Expr::FunctionRef(0, Vec::new())),
                Stmt::Assign {
                    kind: AssignmentKind::Set,
                    index: 10,
                    expr: Expr::FunctionRef(1, Vec::new()),
                    line: 1,
                },
                let_stmt(11, Expr::Var(10)),
                expr_stmt(Expr::LocalCall(11, Vec::new(), Vec::new(), None)),
            ],
            vec![
                decl(0, "helper", false, None),
                decl(1, "other", false, None),
            ],
            HashMap::from([
                (0, impl_with(Vec::new(), Vec::new(), Expr::Int(1))),
                (1, impl_with(Vec::new(), Vec::new(), Expr::Int(2))),
            ]),
        );

        let facts = classify_named_callables(&ir);
        assert!(facts[&0].referenced_as_value);
        assert!(facts[&1].dynamic_target_required);
    }

    #[test]
    fn materialization_closure_captured_callable_propagates_dynamic_target() {
        // `let a = helper; let c = || { a() }; c();`: the closure captures slot
        // `a` and invokes the captured value dynamically in its own frame.
        let ir = ir_with(
            vec![
                func_decl_stmt("helper", 0),
                let_stmt(10, Expr::FunctionRef(0, Vec::new())),
                let_stmt(
                    11,
                    Expr::Closure(ClosureExpr {
                        param_slots: Vec::new(),
                        capture_copies: vec![(10, 30)],
                        body: Box::new(Expr::LocalCall(30, Vec::new(), Vec::new(), None)),
                    }),
                ),
                expr_stmt(Expr::LocalCall(11, Vec::new(), Vec::new(), None)),
            ],
            vec![decl(0, "helper", false, None)],
            HashMap::from([(0, impl_with(Vec::new(), Vec::new(), Expr::Int(1)))]),
        );

        let helper = classify_named_callables(&ir)[&0];
        assert!(helper.dynamic_target_required);
    }

    #[test]
    fn materialization_named_function_capture_invocation_marks_dynamic_target() {
        // `let a = helper; fn g() { a(); } g();`: the named function `g`
        // captures slot `a` and invokes the captured value in its own frame.
        let ir = ir_with(
            vec![
                func_decl_stmt("helper", 0),
                func_decl_stmt("g", 1),
                let_stmt(10, Expr::FunctionRef(0, Vec::new())),
                expr_stmt(call(1)),
            ],
            vec![decl(0, "helper", false, None), decl(1, "g", false, None)],
            HashMap::from([
                (0, impl_with(Vec::new(), Vec::new(), Expr::Int(1))),
                (
                    1,
                    impl_with(
                        vec![(10, 30)],
                        Vec::new(),
                        Expr::LocalCall(30, Vec::new(), Vec::new(), None),
                    ),
                ),
            ]),
        );

        let facts = classify_named_callables(&ir);
        assert!(facts[&0].dynamic_target_required);
    }

    // --- F2: frame-local self recursion ---

    #[test]
    fn materialization_nested_closure_recursion_is_not_frame_local_self_recursion() {
        // `fn f() { let c = || { f() }; c(); }` with captures: the call to `f`
        // executes in the closure's frame, not in `f`'s own executable body,
        // so it must not count as direct self-recursion.
        let f_impl = impl_with(
            vec![(5, 7)],
            vec![
                let_stmt(
                    10,
                    Expr::Closure(ClosureExpr {
                        param_slots: Vec::new(),
                        capture_copies: Vec::new(),
                        body: Box::new(call(0)),
                    }),
                ),
                expr_stmt(Expr::LocalCall(10, Vec::new(), Vec::new(), None)),
            ],
            Expr::Int(1),
        );
        let ir = ir_with(
            vec![func_decl_stmt("f", 0), expr_stmt(call(0))],
            vec![decl(0, "f", false, None)],
            HashMap::from([(0, f_impl)]),
        );

        let f = classify_named_callables(&ir)[&0];
        assert!(f.called_directly);
        assert!(f.captures_environment);
        assert!(!f.runtime_self_required);
        assert!(f.requires_callable_slot());
    }

    #[test]
    fn materialization_function_value_recursion_requires_runtime_self() {
        // `fn f() { let g = f; g(); }`: the function's own value is invoked
        // dynamically from within its own frame — a dynamic recursion path
        // that must bind the runtime self identity.
        let f_impl = impl_with(
            Vec::new(),
            vec![
                let_stmt(10, Expr::FunctionRef(0, Vec::new())),
                expr_stmt(Expr::LocalCall(10, Vec::new(), Vec::new(), None)),
            ],
            Expr::Int(1),
        );
        let ir = ir_with(
            vec![func_decl_stmt("f", 0), expr_stmt(call(0))],
            vec![decl(0, "f", false, None)],
            HashMap::from([(0, f_impl)]),
        );

        let f = classify_named_callables(&ir)[&0];
        assert!(f.dynamic_target_required);
        assert!(f.runtime_self_required);
    }

    // --- F6: dynamic targets only through tracked invocation flow ---

    #[test]
    fn materialization_opaque_callee_arg_keeps_materialization_without_dynamic_target() {
        // `consume(helper)` where `consume` never invokes its parameter: the
        // function value is referenced and materialized, but no tracked value
        // flow reaches an actual dynamic callable target.
        let consume_impl = impl_with_params(vec![10], Vec::new(), Vec::new(), Expr::Int(1));
        let ir = ir_with(
            vec![
                func_decl_stmt("helper", 0),
                func_decl_stmt("consume", 1),
                expr_stmt(Expr::Call(
                    1,
                    Vec::new(),
                    vec![Expr::FunctionRef(0, Vec::new())],
                    None,
                    None,
                )),
            ],
            vec![
                decl(0, "helper", false, None),
                decl(1, "consume", false, None),
            ],
            HashMap::from([
                (0, impl_with(Vec::new(), Vec::new(), Expr::Int(1))),
                (1, consume_impl),
            ]),
        );

        let facts = classify_named_callables(&ir);
        let helper = facts[&0];
        assert!(helper.referenced_as_value);
        assert!(!helper.dynamic_target_required);
        assert!(helper.requires_callable_slot());
    }

    #[test]
    fn materialization_invoking_callee_param_marks_dynamic_target() {
        // `apply(f) { f() }` invoked as `apply(helper)`: the argument reaches
        // a dynamic callable target inside the callee frame.
        let apply_impl = impl_with_params(
            vec![10],
            Vec::new(),
            Vec::new(),
            Expr::LocalCall(10, Vec::new(), Vec::new(), None),
        );
        let ir = ir_with(
            vec![
                func_decl_stmt("helper", 0),
                func_decl_stmt("apply", 1),
                expr_stmt(Expr::Call(
                    1,
                    Vec::new(),
                    vec![Expr::FunctionRef(0, Vec::new())],
                    None,
                    None,
                )),
            ],
            vec![
                decl(0, "helper", false, None),
                decl(1, "apply", false, None),
            ],
            HashMap::from([
                (0, impl_with(Vec::new(), Vec::new(), Expr::Int(1))),
                (1, apply_impl),
            ]),
        );

        let facts = classify_named_callables(&ir);
        assert!(facts[&0].referenced_as_value);
        assert!(facts[&0].dynamic_target_required);
        assert!(!facts[&1].dynamic_target_required);
    }

    #[test]
    fn materialization_callee_param_alias_invocation_marks_dynamic_target() {
        // `apply(f) { let g = f; g(); }`: the parameter reaches the dynamic
        // invocation through an intra-frame alias.
        let apply_impl = impl_with_params(
            vec![10],
            Vec::new(),
            vec![let_stmt(11, Expr::Var(10))],
            Expr::LocalCall(11, Vec::new(), Vec::new(), None),
        );
        let ir = ir_with(
            vec![
                func_decl_stmt("helper", 0),
                func_decl_stmt("apply", 1),
                expr_stmt(Expr::Call(
                    1,
                    Vec::new(),
                    vec![Expr::FunctionRef(0, Vec::new())],
                    None,
                    None,
                )),
            ],
            vec![
                decl(0, "helper", false, None),
                decl(1, "apply", false, None),
            ],
            HashMap::from([
                (0, impl_with(Vec::new(), Vec::new(), Expr::Int(1))),
                (1, apply_impl),
            ]),
        );

        let facts = classify_named_callables(&ir);
        assert!(facts[&0].dynamic_target_required);
    }

    #[test]
    fn materialization_transitive_callee_param_invocation_marks_dynamic_target() {
        // `apply2(g) { apply(g) }` and `apply(f) { f() }`; `apply2(helper)`:
        // the argument reaches the dynamic callable target through two frames.
        let apply_impl = impl_with_params(
            vec![20],
            Vec::new(),
            Vec::new(),
            Expr::LocalCall(20, Vec::new(), Vec::new(), None),
        );
        let apply2_impl = impl_with_params(
            vec![10],
            Vec::new(),
            Vec::new(),
            Expr::Call(1, Vec::new(), vec![Expr::Var(10)], None, None),
        );
        let ir = ir_with(
            vec![
                func_decl_stmt("helper", 0),
                func_decl_stmt("apply", 1),
                func_decl_stmt("apply2", 2),
                expr_stmt(Expr::Call(
                    2,
                    Vec::new(),
                    vec![Expr::FunctionRef(0, Vec::new())],
                    None,
                    None,
                )),
            ],
            vec![
                decl(0, "helper", false, None),
                decl(1, "apply", false, None),
                decl(2, "apply2", false, None),
            ],
            HashMap::from([
                (0, impl_with(Vec::new(), Vec::new(), Expr::Int(1))),
                (1, apply_impl),
                (2, apply2_impl),
            ]),
        );

        let facts = classify_named_callables(&ir);
        assert!(facts[&0].dynamic_target_required);
        assert!(!facts[&1].dynamic_target_required);
    }

    #[test]
    fn materialization_closure_call_param_invocation_marks_dynamic_target() {
        // Immediate closure invocation `(|f| f())(helper)`.
        let closure = ClosureExpr {
            param_slots: vec![30],
            capture_copies: Vec::new(),
            body: Box::new(Expr::LocalCall(30, Vec::new(), Vec::new(), None)),
        };
        let ir = ir_with(
            vec![
                func_decl_stmt("helper", 0),
                expr_stmt(Expr::ClosureCall(
                    closure,
                    vec![Expr::FunctionRef(0, Vec::new())],
                )),
            ],
            vec![decl(0, "helper", false, None)],
            HashMap::from([(0, impl_with(Vec::new(), Vec::new(), Expr::Int(1)))]),
        );

        let helper = classify_named_callables(&ir)[&0];
        assert!(helper.dynamic_target_required);
    }

    #[test]
    fn materialization_stored_closure_call_param_invocation_marks_dynamic_target() {
        // `let apply = |f| f(); apply(helper);`: the closure is stored in a
        // slot and later invoked through `LocalCall` with an argument that
        // reaches its invoked parameter.
        let closure = ClosureExpr {
            param_slots: vec![30],
            capture_copies: Vec::new(),
            body: Box::new(Expr::LocalCall(30, Vec::new(), Vec::new(), None)),
        };
        let ir = ir_with(
            vec![
                func_decl_stmt("helper", 0),
                let_stmt(10, Expr::Closure(closure)),
                expr_stmt(Expr::LocalCall(
                    10,
                    Vec::new(),
                    vec![Expr::FunctionRef(0, Vec::new())],
                    None,
                )),
            ],
            vec![decl(0, "helper", false, None)],
            HashMap::from([(0, impl_with(Vec::new(), Vec::new(), Expr::Int(1)))]),
        );

        let helper = classify_named_callables(&ir)[&0];
        assert!(helper.dynamic_target_required);
    }

    // --- F7: incomplete callee sets stay conservative (unknown provenance) ---

    #[test]
    fn materialization_unknown_callee_provenance_keeps_conservative_propagation() {
        // `let f = helper; f = get_cb(); f(cb);`: the slot holds a known
        // named function that never invokes its parameter *and* a call
        // result whose callable provenance is untracked. The callee set is
        // incomplete, so `Some(false)` must not suppress the conservative
        // propagation: the argument still reaches a dynamic callable target.
        let ir = ir_with(
            vec![
                func_decl_stmt("helper", 0),
                func_decl_stmt("cb", 1),
                func_decl_stmt("get_cb", 2),
                let_stmt(10, Expr::FunctionRef(0, Vec::new())),
                Stmt::Assign {
                    kind: AssignmentKind::Set,
                    index: 10,
                    expr: Expr::Call(2, Vec::new(), Vec::new(), None, None),
                    line: 1,
                },
                expr_stmt(Expr::LocalCall(
                    10,
                    Vec::new(),
                    vec![Expr::FunctionRef(1, Vec::new())],
                    None,
                )),
            ],
            vec![
                decl(0, "helper", false, None),
                decl(1, "cb", false, None),
                decl(2, "get_cb", false, None),
            ],
            HashMap::from([
                // `helper(x)` never invokes its parameter.
                (
                    0,
                    impl_with_params(vec![40], Vec::new(), Vec::new(), Expr::Int(1)),
                ),
                (1, impl_with(Vec::new(), Vec::new(), Expr::Int(2))),
                (2, impl_with(Vec::new(), Vec::new(), Expr::Int(3))),
            ]),
        );

        let facts = classify_named_callables(&ir);
        assert!(facts[&0].dynamic_target_required);
        assert!(
            facts[&1].dynamic_target_required,
            "the argument must be conservatively treated as reaching a dynamic target"
        );
    }

    #[test]
    fn materialization_control_flow_closure_callee_keeps_conservative_propagation() {
        // `let f = if c { |x| x() } else { helper }; f(cb);`: the closure
        // branch is created but never recorded in the slot's closure set
        // (only direct closure lets are), so the callee set is incomplete
        // even though `helper` is a known non-invoking callee.
        let ir = ir_with(
            vec![
                func_decl_stmt("helper", 0),
                func_decl_stmt("cb", 1),
                let_stmt(
                    10,
                    Expr::IfElse {
                        condition: Box::new(Expr::Bool(true)),
                        then_expr: Box::new(Expr::Closure(ClosureExpr {
                            param_slots: vec![30],
                            capture_copies: Vec::new(),
                            body: Box::new(Expr::LocalCall(30, Vec::new(), Vec::new(), None)),
                        })),
                        else_expr: Box::new(Expr::FunctionRef(0, Vec::new())),
                    },
                ),
                expr_stmt(Expr::LocalCall(
                    10,
                    Vec::new(),
                    vec![Expr::FunctionRef(1, Vec::new())],
                    None,
                )),
            ],
            vec![decl(0, "helper", false, None), decl(1, "cb", false, None)],
            HashMap::from([
                // `helper(x)` never invokes its parameter.
                (
                    0,
                    impl_with_params(vec![40], Vec::new(), Vec::new(), Expr::Int(1)),
                ),
                (1, impl_with(Vec::new(), Vec::new(), Expr::Int(2))),
            ]),
        );

        let facts = classify_named_callables(&ir);
        assert!(facts[&0].dynamic_target_required);
        assert!(
            facts[&1].dynamic_target_required,
            "the untracked closure branch must keep the propagation conservative"
        );
    }

    #[test]
    fn materialization_unknown_provenance_flows_through_closure_param_transitively() {
        // `let apply = |f| { let g = f; g(cb) }; let a = helper; a = get_cb();
        // apply(a);`: the unknown provenance of `a` must flow through the
        // closure's parameter slot and its alias `g`, so `cb` is
        // conservatively treated as reaching a dynamic callable target even
        // though the known callee `helper` never invokes its parameter.
        let closure = ClosureExpr {
            param_slots: vec![30],
            capture_copies: Vec::new(),
            body: Box::new(Expr::Block {
                stmts: vec![let_stmt(31, Expr::Var(30))],
                expr: Box::new(Expr::LocalCall(
                    31,
                    Vec::new(),
                    vec![Expr::FunctionRef(1, Vec::new())],
                    None,
                )),
            }),
        };
        let ir = ir_with(
            vec![
                func_decl_stmt("helper", 0),
                func_decl_stmt("cb", 1),
                func_decl_stmt("get_cb", 2),
                let_stmt(10, Expr::FunctionRef(0, Vec::new())),
                Stmt::Assign {
                    kind: AssignmentKind::Set,
                    index: 10,
                    expr: Expr::Call(2, Vec::new(), Vec::new(), None, None),
                    line: 1,
                },
                let_stmt(11, Expr::Closure(closure)),
                expr_stmt(Expr::LocalCall(11, Vec::new(), vec![Expr::Var(10)], None)),
            ],
            vec![
                decl(0, "helper", false, None),
                decl(1, "cb", false, None),
                decl(2, "get_cb", false, None),
            ],
            HashMap::from([
                // `helper(x)` never invokes its parameter.
                (
                    0,
                    impl_with_params(vec![40], Vec::new(), Vec::new(), Expr::Int(1)),
                ),
                (1, impl_with(Vec::new(), Vec::new(), Expr::Int(2))),
                (2, impl_with(Vec::new(), Vec::new(), Expr::Int(3))),
            ]),
        );

        let facts = classify_named_callables(&ir);
        assert!(
            facts[&1].dynamic_target_required,
            "unknown provenance must flow through the closure parameter alias chain"
        );
    }

    #[test]
    fn materialization_unknown_provenance_flows_through_alias_chain_transitively() {
        // `let a = helper; a = get_cb(); let b = a; let c = b; c(cb);`: the
        // unknown provenance travels through two alias hops before the
        // invocation, so the callee set of `c` is incomplete and `cb` must
        // be conservatively marked as reaching a dynamic callable target.
        let ir = ir_with(
            vec![
                func_decl_stmt("helper", 0),
                func_decl_stmt("cb", 1),
                func_decl_stmt("get_cb", 2),
                let_stmt(10, Expr::FunctionRef(0, Vec::new())),
                Stmt::Assign {
                    kind: AssignmentKind::Set,
                    index: 10,
                    expr: Expr::Call(2, Vec::new(), Vec::new(), None, None),
                    line: 1,
                },
                let_stmt(11, Expr::Var(10)),
                let_stmt(12, Expr::Var(11)),
                expr_stmt(Expr::LocalCall(
                    12,
                    Vec::new(),
                    vec![Expr::FunctionRef(1, Vec::new())],
                    None,
                )),
            ],
            vec![
                decl(0, "helper", false, None),
                decl(1, "cb", false, None),
                decl(2, "get_cb", false, None),
            ],
            HashMap::from([
                // `helper(x)` never invokes its parameter.
                (
                    0,
                    impl_with_params(vec![40], Vec::new(), Vec::new(), Expr::Int(1)),
                ),
                (1, impl_with(Vec::new(), Vec::new(), Expr::Int(2))),
                (2, impl_with(Vec::new(), Vec::new(), Expr::Int(3))),
            ]),
        );

        let facts = classify_named_callables(&ir);
        assert!(
            facts[&1].dynamic_target_required,
            "unknown provenance must flow through the alias chain to the invocation"
        );
    }

    #[test]
    fn materialization_complete_control_flow_callee_set_keeps_precision() {
        // `let f = if c { helper } else { other }; f(cb);`: every branch is
        // a tracked named function and neither invokes its parameter, so the
        // callee set is complete and `Some(false)` legitimately suppresses
        // the propagation (precision guard: the soundness fix must not
        // degrade fully-tracked control flow).
        let ir = ir_with(
            vec![
                func_decl_stmt("helper", 0),
                func_decl_stmt("other", 1),
                func_decl_stmt("cb", 2),
                let_stmt(
                    10,
                    Expr::IfElse {
                        condition: Box::new(Expr::Bool(true)),
                        then_expr: Box::new(Expr::FunctionRef(0, Vec::new())),
                        else_expr: Box::new(Expr::FunctionRef(1, Vec::new())),
                    },
                ),
                expr_stmt(Expr::LocalCall(
                    10,
                    Vec::new(),
                    vec![Expr::FunctionRef(2, Vec::new())],
                    None,
                )),
            ],
            vec![
                decl(0, "helper", false, None),
                decl(1, "other", false, None),
                decl(2, "cb", false, None),
            ],
            HashMap::from([
                (
                    0,
                    impl_with_params(vec![40], Vec::new(), Vec::new(), Expr::Int(1)),
                ),
                (
                    1,
                    impl_with_params(vec![41], Vec::new(), Vec::new(), Expr::Int(2)),
                ),
                (2, impl_with(Vec::new(), Vec::new(), Expr::Int(3))),
            ]),
        );

        let facts = classify_named_callables(&ir);
        assert!(facts[&0].dynamic_target_required);
        assert!(facts[&1].dynamic_target_required);
        assert!(
            !facts[&2].dynamic_target_required,
            "a complete callee set of non-invoking functions must suppress propagation"
        );
    }
}
