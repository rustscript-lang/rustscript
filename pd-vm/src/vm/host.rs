use std::task::{Context, Poll, Wake, Waker};

use crate::builtins::BuiltinFunction;

use super::*;

pub type HostOpId = u64;

#[derive(Debug, PartialEq)]
pub enum CallOutcome {
    Return(Vec<Value>),
    Yield,
    Pending(HostOpId),
}

pub trait HostFunction: Send {
    fn call(&mut self, vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome>;
}

pub trait HostAsyncBridge: Send {
    fn poll_op(&mut self, op_id: HostOpId, cx: &mut Context<'_>) -> Poll<VmResult<Vec<Value>>>;
}

pub type StaticHostFunction = fn(&mut Vm, &[Value]) -> VmResult<CallOutcome>;

type HostFactory = dyn Fn() -> Box<dyn HostFunction> + Send + Sync;

enum RegistryEntryKind {
    Factory(Box<HostFactory>),
    Static(StaticHostFunction),
}

struct RegistryEntry {
    arity: u8,
    kind: RegistryEntryKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostBindingPlan {
    import_signature: Vec<HostImport>,
    registry_slots: Vec<u16>,
    resolved_calls: Vec<u16>,
}

pub struct HostFunctionRegistry {
    entries: Vec<RegistryEntry>,
    by_name: HashMap<String, u16>,
    plan_cache: HashMap<Vec<HostImport>, HostBindingPlan>,
}

impl Default for HostFunctionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl HostFunctionRegistry {
    pub fn new() -> Self {
        let mut registry = Self {
            entries: Vec::new(),
            by_name: HashMap::new(),
            plan_cache: HashMap::new(),
        };
        builtins_impl::register_default_host_functions(&mut registry);
        registry
    }

    pub fn register<F>(&mut self, name: impl Into<String>, arity: u8, factory: F)
    where
        F: Fn() -> Box<dyn HostFunction> + Send + Sync + 'static,
    {
        let name = name.into();
        if let Some(&slot) = self.by_name.get(&name)
            && let Some(entry) = self.entries.get_mut(slot as usize)
        {
            entry.arity = arity;
            entry.kind = RegistryEntryKind::Factory(Box::new(factory));
            self.plan_cache.clear();
            return;
        }

        let slot = self.entries.len() as u16;
        self.entries.push(RegistryEntry {
            arity,
            kind: RegistryEntryKind::Factory(Box::new(factory)),
        });
        self.by_name.insert(name, slot);
        self.plan_cache.clear();
    }

    pub fn register_static(
        &mut self,
        name: impl Into<String>,
        arity: u8,
        function: StaticHostFunction,
    ) {
        let name = name.into();
        if let Some(&slot) = self.by_name.get(&name)
            && let Some(entry) = self.entries.get_mut(slot as usize)
        {
            entry.arity = arity;
            entry.kind = RegistryEntryKind::Static(function);
            self.plan_cache.clear();
            return;
        }

        let slot = self.entries.len() as u16;
        self.entries.push(RegistryEntry {
            arity,
            kind: RegistryEntryKind::Static(function),
        });
        self.by_name.insert(name, slot);
        self.plan_cache.clear();
    }

    pub fn bind_vm_cached(&mut self, vm: &mut Vm) -> VmResult<()> {
        let plan = self.prepare_plan(&vm.program.imports)?;
        self.bind_vm_with_plan(vm, &plan)
    }

    pub fn prepare_plan(&mut self, imports: &[HostImport]) -> VmResult<HostBindingPlan> {
        self.plan_for_imports(imports).cloned()
    }

    fn plan_for_imports(&mut self, imports: &[HostImport]) -> VmResult<&HostBindingPlan> {
        if !self.plan_cache.contains_key(imports) {
            let mut registry_slot_to_vm_slot: HashMap<u16, u16> = HashMap::new();
            let mut registry_slots = Vec::new();
            let mut resolved_calls = Vec::with_capacity(imports.len());

            for import in imports {
                let registry_slot = self
                    .by_name
                    .get(&import.name)
                    .copied()
                    .ok_or_else(|| VmError::UnboundImport(import.name.clone()))?;
                let entry = self
                    .entries
                    .get(registry_slot as usize)
                    .ok_or(VmError::InvalidCall(registry_slot))?;
                if entry.arity != import.arity {
                    return Err(VmError::InvalidCallArity {
                        import: import.name.clone(),
                        expected: entry.arity,
                        got: import.arity,
                    });
                }

                let vm_slot = if let Some(&existing) = registry_slot_to_vm_slot.get(&registry_slot)
                {
                    existing
                } else {
                    let slot = registry_slots.len() as u16;
                    registry_slots.push(registry_slot);
                    registry_slot_to_vm_slot.insert(registry_slot, slot);
                    slot
                };
                resolved_calls.push(vm_slot);
            }

            self.plan_cache.insert(
                imports.to_vec(),
                HostBindingPlan {
                    import_signature: imports.to_vec(),
                    registry_slots,
                    resolved_calls,
                },
            );
        }

        self.plan_cache
            .get(imports)
            .ok_or_else(|| VmError::HostError("host binding plan cache lookup failed".to_string()))
    }

    pub fn bind_vm_with_plan(&self, vm: &mut Vm, plan: &HostBindingPlan) -> VmResult<()> {
        if vm.program.imports != plan.import_signature {
            return Err(VmError::HostError(
                "host binding plan does not match vm import signature".to_string(),
            ));
        }
        if !vm.host_functions.is_empty() || !vm.host_function_symbols.is_empty() {
            return Err(VmError::HostError(
                "host binding cache requires an unbound vm".to_string(),
            ));
        }

        for &registry_slot in &plan.registry_slots {
            let entry = self
                .entries
                .get(registry_slot as usize)
                .ok_or(VmError::InvalidCall(registry_slot))?;
            match &entry.kind {
                RegistryEntryKind::Factory(factory) => {
                    vm.register_function(factory());
                }
                RegistryEntryKind::Static(function) => {
                    vm.register_static_function(*function);
                }
            }
        }
        vm.install_resolved_calls(plan.resolved_calls.clone())?;
        Ok(())
    }
}

pub(super) enum VmHostFunction {
    Dynamic(Box<dyn HostFunction>),
    Static(StaticHostFunction),
}

pub(super) enum HostCallExecOutcome {
    Returned,
    Yielded,
    Pending(HostOpId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct WaitingHostOp {
    pub(super) op_id: HostOpId,
    pub(super) source: WaitingHostOpSource,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum WaitingHostOpSource {
    HostBridge,
    BuiltinIo,
}

struct NoopWake;

impl Wake for NoopWake {
    fn wake(self: Arc<Self>) {}
}

fn noop_waker() -> Waker {
    Waker::from(Arc::new(NoopWake))
}

impl Vm {
    pub fn register_function(&mut self, function: Box<dyn HostFunction>) -> u16 {
        let index = self.host_functions.len() as u16;
        self.host_functions.push(VmHostFunction::Dynamic(function));
        self.resolved_calls_dirty = true;
        index
    }

    pub fn register_static_function(&mut self, function: StaticHostFunction) -> u16 {
        let index = self.host_functions.len() as u16;
        self.host_functions.push(VmHostFunction::Static(function));
        self.resolved_calls_dirty = true;
        index
    }

    pub fn bind_function(&mut self, name: impl Into<String>, function: Box<dyn HostFunction>) {
        let name = name.into();
        if let Some(builtin) = BuiltinFunction::from_namespaced_name(&name) {
            self.bind_builtin_overrideslot(builtin.call_index(), VmHostFunction::Dynamic(function));
            return;
        }
        if let Some(&index) = self.host_function_symbols.get(&name)
            && let Some(slot) = self.host_functions.get_mut(index as usize)
        {
            *slot = VmHostFunction::Dynamic(function);
            self.resolved_calls_dirty = true;
            return;
        }

        let index = self.register_function(function);
        self.host_function_symbols.insert(name, index);
        self.resolved_calls_dirty = true;
    }

    pub fn bind_static_function(&mut self, name: impl Into<String>, function: StaticHostFunction) {
        let name = name.into();
        if let Some(builtin) = BuiltinFunction::from_namespaced_name(&name) {
            self.bind_builtin_overrideslot(builtin.call_index(), VmHostFunction::Static(function));
            return;
        }
        if let Some(&index) = self.host_function_symbols.get(&name)
            && let Some(slot) = self.host_functions.get_mut(index as usize)
        {
            *slot = VmHostFunction::Static(function);
            self.resolved_calls_dirty = true;
            return;
        }

        let index = self.register_static_function(function);
        self.host_function_symbols.insert(name, index);
        self.resolved_calls_dirty = true;
    }

    pub fn bind_builtin_override(
        &mut self,
        name: impl Into<String>,
        function: Box<dyn HostFunction>,
    ) -> VmResult<()> {
        let name = name.into();
        let builtin = BuiltinFunction::from_namespaced_name(&name).ok_or_else(|| {
            VmError::HostError(format!("unknown namespaced builtin override '{name}'"))
        })?;
        self.bind_builtin_overrideslot(builtin.call_index(), VmHostFunction::Dynamic(function));
        Ok(())
    }

    pub fn bind_builtin_static_override(
        &mut self,
        name: impl Into<String>,
        function: StaticHostFunction,
    ) -> VmResult<()> {
        let name = name.into();
        let builtin = BuiltinFunction::from_namespaced_name(&name).ok_or_else(|| {
            VmError::HostError(format!("unknown namespaced builtin override '{name}'"))
        })?;
        self.bind_builtin_overrideslot(builtin.call_index(), VmHostFunction::Static(function));
        Ok(())
    }

    fn bind_builtin_overrideslot(&mut self, builtin_call_index: u16, function: VmHostFunction) {
        if let Some(&host_slot) = self.builtin_overrides.get(&builtin_call_index)
            && let Some(slot) = self.host_functions.get_mut(host_slot as usize)
        {
            *slot = function;
            return;
        }

        let host_slot = self.host_functions.len() as u16;
        self.host_functions.push(function);
        self.builtin_overrides.insert(builtin_call_index, host_slot);
    }

    pub fn set_async_bridge(&mut self, bridge: Box<dyn HostAsyncBridge>) {
        self.async_bridge = Some(bridge);
    }

    pub fn clear_async_bridge(&mut self) {
        self.async_bridge = None;
    }

    pub fn set_runtime_print_sink<F>(&mut self, sink: F)
    where
        F: FnMut(String) + Send + 'static,
    {
        self.runtime_print_sink = Some(Box::new(sink));
    }

    pub fn clear_runtime_print_sink(&mut self) {
        self.runtime_print_sink = None;
    }

    pub(crate) fn write_runtime_print(&mut self, rendered: String) -> VmResult<()> {
        let Some(sink) = self.runtime_print_sink.as_mut() else {
            return Err(VmError::HostError(
                "runtime print sink is not configured".to_string(),
            ));
        };
        sink(rendered);
        Ok(())
    }

    pub fn allocate_host_op_id(&mut self) -> HostOpId {
        let op_id = self.next_host_op_id;
        self.next_host_op_id = self.next_host_op_id.wrapping_add(1).max(1);
        op_id
    }

    pub fn waiting_host_op_id(&self) -> Option<HostOpId> {
        self.waiting_host_op.map(|op| op.op_id)
    }

    pub fn complete_host_op(&mut self, op_id: HostOpId, values: Vec<Value>) -> VmResult<()> {
        self.complete_waiting_host_op(op_id, values)
    }

    pub fn poll_waiting_host_op(&mut self, cx: &mut Context<'_>) -> Poll<VmResult<()>> {
        let Some(waiting) = self.waiting_host_op else {
            return Poll::Ready(Ok(()));
        };

        let poll_result = match waiting.source {
            WaitingHostOpSource::HostBridge => {
                let bridge_ptr = match self.async_bridge.as_mut() {
                    Some(bridge) => bridge.as_mut() as *mut dyn HostAsyncBridge,
                    None => {
                        return Poll::Ready(Err(VmError::HostError(format!(
                            "vm waiting on host op {} without an async bridge",
                            waiting.op_id
                        ))));
                    }
                };

                unsafe { (&mut *bridge_ptr).poll_op(waiting.op_id, cx) }
            }
            WaitingHostOpSource::BuiltinIo => {
                builtins_impl::poll_builtin_io_op(self, waiting.op_id, cx)
            }
        };

        match poll_result {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(values)) => {
                self.complete_waiting_host_op(waiting.op_id, values)?;
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(err)) => {
                self.waiting_host_op = None;
                Poll::Ready(Err(err))
            }
        }
    }

    pub async fn await_waiting_host_op(&mut self) -> VmResult<()> {
        std::future::poll_fn(|cx| self.poll_waiting_host_op(cx)).await
    }

    pub fn wait_for_host_op_blocking(&mut self) -> VmResult<()> {
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        loop {
            match self.poll_waiting_host_op(&mut cx) {
                Poll::Ready(result) => return result,
                Poll::Pending => {
                    #[cfg(not(target_arch = "wasm32"))]
                    {
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                    #[cfg(target_arch = "wasm32")]
                    {
                        return Err(VmError::HostError(
                            "blocking host-op wait is unsupported on wasm32 runtime".to_string(),
                        ));
                    }
                }
            }
        }
    }

    pub(super) fn execute_host_call(
        &mut self,
        index: u16,
        argc_u8: u8,
        call_ip: usize,
    ) -> VmResult<HostCallExecOutcome> {
        let argc = argc_u8 as usize;
        let mut args = Vec::with_capacity(argc);
        for _ in 0..argc {
            args.push(self.pop_value()?);
        }
        args.reverse();

        if let Some(builtin) = BuiltinFunction::from_call_index(index) {
            if !builtin.accepts_arity(argc_u8) {
                return Err(VmError::InvalidCallArity {
                    import: builtin.name().to_string(),
                    expected: builtin.arity(),
                    got: argc_u8,
                });
            }
            if self.builtin_overrides.contains_key(&index) {
                return self.execute_builtin_override_call(index, args, call_ip);
            }
            match builtins_impl::execute_builtin_call(self, builtin, args)? {
                builtins_impl::BuiltinCallOutcome::Return(values) => {
                    for value in values {
                        self.stack.push(value);
                    }
                    return Ok(HostCallExecOutcome::Returned);
                }
                builtins_impl::BuiltinCallOutcome::Pending(op_id) => {
                    let resume_ip = self.call_resume_ip(call_ip)?;
                    self.set_waiting_host_op(op_id, WaitingHostOpSource::BuiltinIo)?;
                    self.ip = resume_ip;
                    return Ok(HostCallExecOutcome::Pending(op_id));
                }
            }
        }

        let resolved_index = self.resolve_call_target(index, argc_u8)?;
        self.execute_bound_host_function(resolved_index, args, call_ip)
    }

    pub(super) fn execute_builtin_override_call(
        &mut self,
        builtin_call_index: u16,
        args: Vec<Value>,
        call_ip: usize,
    ) -> VmResult<HostCallExecOutcome> {
        let resolved_index = self
            .builtin_overrides
            .get(&builtin_call_index)
            .copied()
            .ok_or_else(|| {
                VmError::HostError(format!(
                    "missing builtin override slot for call index {builtin_call_index}"
                ))
            })?;
        self.execute_bound_host_function(resolved_index, args, call_ip)
    }

    pub(super) fn execute_bound_host_function(
        &mut self,
        resolved_index: u16,
        args: Vec<Value>,
        call_ip: usize,
    ) -> VmResult<HostCallExecOutcome> {
        self.call_depth += 1;
        let function_ptr =
            self.host_functions
                .get_mut(resolved_index as usize)
                .ok_or(VmError::InvalidCall(resolved_index))? as *mut VmHostFunction;
        let outcome = unsafe {
            match &mut *function_ptr {
                VmHostFunction::Dynamic(function) => function.call(self, &args),
                VmHostFunction::Static(function) => function(self, &args),
            }
        };
        self.call_depth = self.call_depth.saturating_sub(1);
        let outcome = outcome?;

        match outcome {
            CallOutcome::Return(values) => {
                for value in values {
                    self.stack.push(value);
                }
                Ok(HostCallExecOutcome::Returned)
            }
            CallOutcome::Yield => {
                for value in args {
                    self.stack.push(value);
                }
                if self.native_only_aot {
                    return Err(VmError::JitNative(
                        "native-only AOT bundles do not support host CallOutcome::Yield"
                            .to_string(),
                    ));
                }
                self.ip = call_ip;
                Ok(HostCallExecOutcome::Yielded)
            }
            CallOutcome::Pending(op_id) => {
                let resume_ip = self.call_resume_ip(call_ip)?;
                self.set_waiting_host_op(op_id, WaitingHostOpSource::HostBridge)?;
                self.ip = resume_ip;
                Ok(HostCallExecOutcome::Pending(op_id))
            }
        }
    }

    pub(super) fn call_resume_ip(&self, call_ip: usize) -> VmResult<usize> {
        let resume_ip = call_ip.checked_add(4).ok_or(VmError::BytecodeBounds)?;
        if resume_ip > self.program.code.len() {
            return Err(VmError::BytecodeBounds);
        }
        Ok(resume_ip)
    }

    pub(super) fn set_waiting_host_op(
        &mut self,
        op_id: HostOpId,
        source: WaitingHostOpSource,
    ) -> VmResult<()> {
        if let Some(active) = self.waiting_host_op
            && active.op_id != op_id
        {
            return Err(VmError::HostError(format!(
                "vm already waiting on host op {}, cannot wait on {}",
                active.op_id, op_id
            )));
        }
        self.waiting_host_op = Some(WaitingHostOp { op_id, source });
        Ok(())
    }

    pub(super) fn complete_waiting_host_op(
        &mut self,
        op_id: HostOpId,
        values: Vec<Value>,
    ) -> VmResult<()> {
        let waiting = self.waiting_host_op.ok_or_else(|| {
            VmError::HostError(format!(
                "host op {} completed but vm is not waiting on any op",
                op_id
            ))
        })?;
        if waiting.op_id != op_id {
            return Err(VmError::HostError(format!(
                "host op {} completed while vm waits on {}",
                op_id, waiting.op_id
            )));
        }
        self.waiting_host_op = None;
        for value in values {
            self.stack.push(value);
        }
        Ok(())
    }

    pub(super) fn install_resolved_calls(&mut self, resolved_calls: Vec<u16>) -> VmResult<()> {
        if self.program.imports.len() != resolved_calls.len() {
            return Err(VmError::HostError(format!(
                "resolved call cache size mismatch: expected {}, got {}",
                self.program.imports.len(),
                resolved_calls.len()
            )));
        }
        for &index in &resolved_calls {
            if index as usize >= self.host_functions.len() {
                return Err(VmError::InvalidCall(index));
            }
        }
        self.resolved_calls = resolved_calls;
        self.resolved_calls_dirty = false;
        Ok(())
    }

    pub(super) fn ensure_call_bindings(&mut self) -> VmResult<()> {
        if self.program.imports.is_empty() || !self.resolved_calls_dirty {
            return Ok(());
        }

        if self.host_function_symbols.is_empty() && self.host_functions.is_empty() {
            let import_names = self
                .program
                .imports
                .iter()
                .map(|import| import.name.clone())
                .collect::<Vec<_>>();
            for name in import_names {
                let _ = builtins_impl::bind_default_host_function(self, &name);
            }
        }

        let use_legacy_order = self.host_function_symbols.is_empty();
        let mut resolved = Vec::with_capacity(self.program.imports.len());
        let imports = self.program.imports.clone();
        for (index, import) in imports.iter().enumerate() {
            if use_legacy_order {
                if index >= self.host_functions.len() {
                    return Err(VmError::InvalidCall(index as u16));
                }
                resolved.push(index as u16);
                continue;
            }

            let bound = if let Some(bound) = self.host_function_symbols.get(&import.name).copied() {
                bound
            } else if builtins_impl::bind_default_host_function(self, &import.name) {
                self.host_function_symbols
                    .get(&import.name)
                    .copied()
                    .ok_or_else(|| VmError::UnboundImport(import.name.clone()))?
            } else {
                return Err(VmError::UnboundImport(import.name.clone()));
            };
            resolved.push(bound);
        }

        self.resolved_calls = resolved;
        self.resolved_calls_dirty = false;
        Ok(())
    }

    pub(super) fn resolve_call_target(&mut self, index: u16, argc: u8) -> VmResult<u16> {
        if self.program.imports.is_empty() {
            return Ok(index);
        }

        self.ensure_call_bindings()?;
        let import = self
            .program
            .imports
            .get(index as usize)
            .ok_or(VmError::InvalidCall(index))?;
        if import.arity != argc {
            return Err(VmError::InvalidCallArity {
                import: import.name.clone(),
                expected: import.arity,
                got: argc,
            });
        }

        self.resolved_calls
            .get(index as usize)
            .copied()
            .ok_or(VmError::InvalidCall(index))
    }
}
