//! Out-of-line source authority and weak observations of real capture cells.
use super::resource::{HostResource, Resource, ResourceHandle};
use super::{HostContext, HostContextError, Vm};
use crate::compiler::capture_certification::CaptureCertification;
use crate::host_api::ResourceTypeKey;
use crate::{CallableValue, CaptureBindingMode, Program, TypeSchema, Value};
use std::collections::HashMap;
use std::sync::{
    Arc, Mutex, Weak,
    atomic::{AtomicBool, Ordering},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureRootState {
    Live,
    Released,
    Unavailable,
}

/// A weak cell observation. It owns no guest value, resource, environment or cell.
#[derive(Clone)]
pub struct ResourceCaptureRoot {
    epoch: Weak<AtomicBool>,
    validity: Arc<CellOrigin>,
}
impl ResourceCaptureRoot {
    pub fn observe(&self, context: &HostContext<'_>) -> CaptureRootState {
        context.observe_capture_root(self)
    }
}

pub struct CheckedResourceCapture<T: HostResource> {
    token: Resource<T>,
    root: ResourceCaptureRoot,
}
impl<T: HostResource> CheckedResourceCapture<T> {
    pub fn token(&self) -> &Resource<T> {
        &self.token
    }
    pub fn root(&self) -> &ResourceCaptureRoot {
        &self.root
    }
    pub(super) fn new(token: Resource<T>, root: ResourceCaptureRoot) -> Self {
        Self { token, root }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CaptureQueryError {
    Unavailable,
    NoMatchingCapture,
    AmbiguousCapture,
    Resource(Box<HostContextError>),
}
impl std::fmt::Display for CaptureQueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "capture query: {self:?}")
    }
}
impl std::error::Error for CaptureQueryError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CaptureCertificationStats {
    pub capture_facts: usize,
    pub entries: usize,
    pub capacity: usize,
    pub registrations: u64,
    pub probes: u64,
    pub retired: u64,
    pub valid: bool,
}
struct CellOrigin {
    cell: Weak<Mutex<Value>>,
    issued: Weak<CallableValue>,
    prototype: u32,
    ordinal: usize,
    key: ResourceTypeKey,
    handle: ResourceHandle,
    valid: AtomicBool,
}
struct CaptureState {
    program: Weak<Program>,
    epoch: Arc<AtomicBool>,
    facts: CaptureCertification,
    capture_facts: usize,
    sites: HashMap<usize, u32>,
    native_site: Option<usize>,
    entries: Vec<Arc<CellOrigin>>,
    index: HashMap<usize, usize>,
    frames: HashMap<usize, Vec<Option<Arc<CellOrigin>>>>,
    cursor: usize,
    registrations: u64,
    probes: u64,
    retired: u64,
}
impl CaptureState {
    fn valid_for(&self, program: &Arc<Program>) -> bool {
        self.epoch.load(Ordering::Acquire) && self.program.ptr_eq(&Arc::downgrade(program))
    }
    fn register(&mut self, origin: Arc<CellOrigin>) {
        let pointer = origin.cell.as_ptr() as usize;
        if self.index.contains_key(&pointer) {
            return;
        }
        self.index.insert(pointer, self.entries.len());
        self.entries.push(origin);
        self.registrations = self.registrations.saturating_add(1);
        // Exactly a fixed maximum of four probes per new cell. Swap removal
        // updates the index and rechecks the swapped record on the next probe.
        for _ in 0..4 {
            if self.entries.is_empty() {
                self.cursor = 0;
                break;
            }
            if self.cursor >= self.entries.len() {
                self.cursor = 0;
            }
            self.probes = self.probes.saturating_add(1);
            if self.entries[self.cursor].cell.upgrade().is_none() {
                let expired = self.entries.swap_remove(self.cursor);
                self.index.remove(&(expired.cell.as_ptr() as usize));
                if let Some(swapped) = self.entries.get(self.cursor) {
                    self.index
                        .insert(swapped.cell.as_ptr() as usize, self.cursor);
                }
                self.retired = self.retired.saturating_add(1);
            } else {
                self.cursor += 1;
            }
        }
    }
}
impl Vm {
    pub(crate) fn attach_capture_certification(&mut self, facts: CaptureCertification) {
        self.revoke_capture_certification();
        let sites = facts.creation_sites.iter().copied().collect();
        let capture_facts = facts.prototypes.iter().map(Vec::len).sum();
        self.host.set_module_state(CaptureState {
            program: Arc::downgrade(&self.program),
            epoch: Arc::new(AtomicBool::new(true)),
            facts,
            capture_facts,
            sites,
            native_site: None,
            entries: Vec::new(),
            index: HashMap::new(),
            frames: HashMap::new(),
            cursor: 0,
            registrations: 0,
            probes: 0,
            retired: 0,
        });
    }
    pub(super) fn revoke_capture_certification(&mut self) {
        if let Some(state) = self.host.get_module_state_mut::<CaptureState>() {
            state.epoch.store(false, Ordering::Release);
            state.frames.clear();
        }
    }
    pub fn capture_certification_stats(&self) -> Option<CaptureCertificationStats> {
        let state = self.host.get_module_state::<CaptureState>()?;
        Some(CaptureCertificationStats {
            capture_facts: state.capture_facts,
            entries: state.entries.len(),
            capacity: state.entries.capacity(),
            registrations: state.registrations,
            probes: state.probes,
            retired: state.retired,
            valid: state.valid_for(&self.program),
        })
    }
    pub(super) fn certify_capture_cells(&mut self, callable: &Arc<CallableValue>) {
        let Some(state) = self.host.get_module_state_mut::<CaptureState>() else {
            return;
        };
        if !state.valid_for(&self.program)
            || state
                .sites
                .get(&state.native_site.unwrap_or(self.instance.ip))
                != Some(&callable.prototype_id)
        {
            return;
        }
        let Some(facts) = state.facts.prototypes.get(callable.prototype_id as usize) else {
            return;
        };
        let Some(prototype) = self
            .program
            .callable_prototypes
            .get(callable.prototype_id as usize)
        else {
            return;
        };
        let Some(environment) = &callable.env else {
            return;
        };
        let Ok(cells) = environment.cells.lock() else {
            return;
        };
        if facts.len() != cells.len() || cells.len() != prototype.capture_slots.len() {
            return;
        }
        let mut origins = Vec::new();
        for (ordinal, (fact, cell)) in facts.iter().zip(cells.iter()).enumerate() {
            if prototype.capture_source_slots.get(ordinal) != Some(&fact.source)
                || prototype.capture_slots.get(ordinal) != Some(&fact.target)
                || prototype.capture_modes.get(ordinal) != Some(&fact.mode)
                || fact.mode != CaptureBindingMode::Move
            {
                continue;
            }
            let Some(TypeSchema::Resource(key)) = &fact.schema else {
                continue;
            };
            let Ok(value) = cell.lock() else {
                continue;
            };
            let Value::Int(raw) = &*value else {
                continue;
            };
            let Ok(handle) = ResourceHandle::from_raw(*raw as u64) else {
                continue;
            };
            origins.push(Arc::new(CellOrigin {
                cell: Arc::downgrade(cell),
                issued: Arc::downgrade(callable),
                prototype: callable.prototype_id,
                ordinal,
                key: key.clone(),
                handle,
                valid: AtomicBool::new(true),
            }));
        }
        for origin in origins {
            state.register(origin);
        }
    }
    pub(super) fn certify_capture_frame(
        &mut self,
        base: usize,
        callable: Option<&Arc<CallableValue>>,
        prototype_id: u32,
    ) {
        let Some(state) = self.host.get_module_state_mut::<CaptureState>() else {
            return;
        };
        state.frames.remove(&base);
        if !state.valid_for(&self.program) {
            return;
        }
        let Some(callable) = callable else {
            return;
        };
        let Some(prototype) = self.program.callable_prototypes.get(prototype_id as usize) else {
            return;
        };
        let Some(environment) = &callable.env else {
            return;
        };
        let Ok(cells) = environment.cells.lock() else {
            return;
        };
        let roots = cells
            .iter()
            .enumerate()
            .map(|(ordinal, cell)| {
                let record = state
                    .entries
                    .get(*state.index.get(&(Arc::as_ptr(cell) as usize))?)?;
                let absolute =
                    base.checked_add(usize::from(*prototype.capture_slots.get(ordinal)?))?;
                let connected = self.instance.capture_cells.get(&absolute)?;
                (record.prototype == prototype_id
                    && record.ordinal == ordinal
                    && record.issued.ptr_eq(&Arc::downgrade(callable))
                    && record.cell.ptr_eq(&Arc::downgrade(cell))
                    && Arc::ptr_eq(connected, cell)
                    && record.valid.load(Ordering::Acquire))
                .then(|| record.clone())
            })
            .collect();
        state.frames.insert(base, roots);
    }
    pub(super) fn release_capture_frame(&mut self, base: usize) {
        if let Some(state) = self.host.get_module_state_mut::<CaptureState>() {
            state.frames.remove(&base);
        }
    }
    /// Native code supplies the authenticated instruction offset, rather than
    /// changing interpreter IP or relying on a stale local mirror.
    pub(super) fn execute_host_call_with_capture_site(
        &mut self,
        index: u16,
        argc: u8,
        call_ip: usize,
    ) -> super::VmResult<super::host::HostCallExecOutcome> {
        let previous = self
            .host
            .get_module_state_mut::<CaptureState>()
            .map(|state| {
                let previous = state.native_site;
                state.native_site = call_ip.checked_add(4);
                previous
            });
        let outcome = self.execute_host_call(index, argc, call_ip);
        if let Some(previous) = previous
            && let Some(state) = self.host.get_module_state_mut::<CaptureState>()
        {
            state.native_site = previous;
        }
        outcome
    }
    pub(super) fn invalidate_capture_cell(&mut self, cell: &crate::bytecode::SharedCaptureCell) {
        let Some(state) = self.host.get_module_state::<CaptureState>() else {
            return;
        };
        if let Some(record) = state
            .index
            .get(&(Arc::as_ptr(cell) as usize))
            .and_then(|i| state.entries.get(*i))
        {
            record.valid.store(false, Ordering::Release);
        }
    }
    pub(super) fn checked_capture_origin(
        &self,
        expected: &ResourceTypeKey,
    ) -> Result<(ResourceHandle, ResourceCaptureRoot), CaptureQueryError> {
        let state = self
            .host
            .get_module_state::<CaptureState>()
            .ok_or(CaptureQueryError::Unavailable)?;
        if !state.valid_for(&self.program) {
            return Err(CaptureQueryError::Unavailable);
        }
        let frame = self
            .instance
            .execution_frames
            .last()
            .ok_or(CaptureQueryError::Unavailable)?;
        let id = frame
            .prototype_id
            .ok_or(CaptureQueryError::NoMatchingCapture)?;
        let prototype = self
            .program
            .callable_prototypes
            .get(id as usize)
            .ok_or(CaptureQueryError::Unavailable)?;
        let facts = state
            .facts
            .prototypes
            .get(id as usize)
            .ok_or(CaptureQueryError::Unavailable)?;
        if facts.len() != prototype.capture_slots.len() {
            return Err(CaptureQueryError::Unavailable);
        }
        if facts.iter().any(|fact| {
            fact.mode == CaptureBindingMode::Move
                && matches!(
                    fact.schema,
                    None | Some(TypeSchema::Unknown | TypeSchema::GenericParam(_))
                )
        }) {
            return Err(CaptureQueryError::Unavailable);
        }
        let mut matching = facts.iter().enumerate().filter(|(_, fact)| {
            fact.mode == CaptureBindingMode::Move
                && fact.schema.as_ref() == Some(&TypeSchema::Resource(expected.clone()))
        });
        let (ordinal, fact) = matching
            .next()
            .ok_or(CaptureQueryError::NoMatchingCapture)?;
        if matching.next().is_some() {
            return Err(CaptureQueryError::AmbiguousCapture);
        }
        let record = state
            .frames
            .get(&frame.local_base)
            .and_then(|roots| roots.get(ordinal))
            .and_then(Option::as_ref)
            .ok_or(CaptureQueryError::Unavailable)?;
        let absolute = frame.local_base + usize::from(fact.target);
        let cell = self
            .instance
            .capture_cells
            .get(&absolute)
            .ok_or(CaptureQueryError::Unavailable)?;
        if record.prototype != id
            || record.ordinal != ordinal
            || !record.cell.ptr_eq(&Arc::downgrade(cell))
            || !record.valid.load(Ordering::Acquire)
        {
            return Err(CaptureQueryError::Unavailable);
        }
        let value = cell.lock().map_err(|_| CaptureQueryError::Unavailable)?;
        if !matches!(&*value, Value::Int(raw) if *raw as u64 == record.handle.raw()) {
            return Err(CaptureQueryError::Unavailable);
        }
        Ok((
            record.handle,
            ResourceCaptureRoot {
                epoch: Arc::downgrade(&state.epoch),
                validity: record.clone(),
            },
        ))
    }
    pub(super) fn observe_capture_root(&self, root: &ResourceCaptureRoot) -> CaptureRootState {
        let Some(state) = self.host.get_module_state::<CaptureState>() else {
            return CaptureRootState::Unavailable;
        };
        if !state.valid_for(&self.program)
            || !root.epoch.ptr_eq(&Arc::downgrade(&state.epoch))
            || !root.validity.valid.load(Ordering::Acquire)
            || self
                .host
                .execution_scope
                .resources()
                .validate_resource_type_key(root.validity.handle, &root.validity.key)
                .is_err()
        {
            return CaptureRootState::Unavailable;
        }
        let Some(cell) = root.validity.cell.upgrade() else {
            return CaptureRootState::Released;
        };
        let live = cell.lock().is_ok_and(
            |value| matches!(&*value, Value::Int(raw) if *raw as u64 == root.validity.handle.raw()),
        );
        drop(cell);
        if live {
            CaptureRootState::Live
        } else {
            CaptureRootState::Unavailable
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        CompileSourceFileOptions, HostApiBuilder, HostFunctionSchema, HostParamPassing,
        HostParamSchema, HostTypeSchema, ResourceTypeSchema, SourceFlavor, VmStatus,
    };
    struct BoxPayload(i64);
    impl HostResource for BoxPayload {
        fn resource_type_key() -> Option<ResourceTypeKey> {
            Some(key())
        }
    }
    struct OtherConcrete;
    impl HostResource for OtherConcrete {
        fn resource_type_key() -> Option<ResourceTypeKey> {
            Some(key())
        }
    }
    fn key() -> ResourceTypeKey {
        ResourceTypeKey::new("neutral.cell-box").unwrap()
    }
    #[derive(Default)]
    struct ProbeState {
        environment: Option<Weak<crate::CallableEnvironment>>,
        root: Option<ResourceCaptureRoot>,
        handle: Option<ResourceHandle>,
        calls: usize,
        expected: Option<CaptureQueryError>,
        write_same_bits: bool,
        internal_same_bits: bool,
        detach: bool,
        held_alias: Option<Value>,
    }

    fn fixture(source: &str, certified: bool) -> Vm {
        let mut b = HostApiBuilder::new();
        b.resource(ResourceTypeSchema::new(key(), "cell box"));
        b.function(HostFunctionSchema::with_return(
            "neutral::open",
            vec![],
            HostTypeSchema::Resource(key()),
        ));
        b.function(HostFunctionSchema::with_return(
            "neutral::check",
            vec![],
            HostTypeSchema::Int,
        ));
        b.function(HostFunctionSchema::with_return(
            "neutral::close",
            vec![HostParamSchema::with_passing(
                "value",
                HostTypeSchema::Resource(key()),
                HostParamPassing::TakeOwned,
            )],
            HostTypeSchema::Null,
        ));
        let catalog = Arc::new(b.build().unwrap());
        let options = CompileSourceFileOptions::new().with_host_api_catalog(catalog.clone());
        let issued = crate::compile_source_with_flavor_and_options_with_capture_certification(
            source,
            SourceFlavor::RustScript,
            options,
        )
        .unwrap();
        let mut vm = if certified {
            issued.into_vm().unwrap()
        } else {
            issued.into_uncertified().into_vm().unwrap()
        };
        vm.host_context().set_module_state(ProbeState::default());
        let mut registry = crate::HostFunctionRegistry::new();
        for schema in crate::catalog_import_schemas(&catalog, "neutral::open") {
            registry
                .register_exact_static("neutral::open", 0, schema, |vm, _| {
                    let token = vm.host_context().push_resource(BoxPayload(41)).unwrap();
                    Ok(super::super::CallOutcome::Return(
                        super::super::CallReturn::one(Value::Int(token.handle().raw() as i64)),
                    ))
                })
                .unwrap();
        }
        for schema in crate::catalog_import_schemas(&catalog, "neutral::check") {
            registry.register_exact_static("neutral::check", 0, schema, |vm, _| {
                let expected = vm.host_context().module_state::<ProbeState>().unwrap().expected.clone();
                if let Some(expected) = expected {
                    assert!(matches!(vm.host_context().checked_move_resource_capture::<BoxPayload>(&key()), Err(error) if error == expected));
                } else {
                    let held = vm.host_context().module_state_mut::<ProbeState>().unwrap().held_alias.take();
                    drop(held);
                    let context = vm.host_context();
                    let checked = context.checked_move_resource_capture::<BoxPayload>(&key()).unwrap();
                    assert!(matches!(context.checked_move_resource_capture::<BoxPayload>(&ResourceTypeKey::new("neutral.other-key").unwrap()), Err(CaptureQueryError::NoMatchingCapture)));
                    assert!(matches!(context.checked_move_resource_capture::<OtherConcrete>(&key()), Err(CaptureQueryError::Resource(_))));
                    assert_eq!(context.resource(checked.token()).unwrap().get().0, 41);
                    assert_eq!(checked.root().observe(&context), CaptureRootState::Live);
                    if let Some(environment) = &context.module_state::<ProbeState>().unwrap().environment {
                        assert!(environment.upgrade().is_none(), "no-self frame must not retain its environment");
                    }
                    let root = checked.root().clone();
                    let handle = checked.token().handle();
                    let replace = context.module_state::<ProbeState>().unwrap().write_same_bits;
                    let internal = context.module_state::<ProbeState>().unwrap().internal_same_bits;
                    let detach = context.module_state::<ProbeState>().unwrap().detach;
                    if replace {
                        let frame = vm.instance.execution_frames.last().unwrap();
                        let prototype = &vm.program.callable_prototypes[frame.prototype_id.unwrap() as usize];
                        vm.set_local(prototype.capture_slots[0] as u8, Value::Int(handle.raw() as i64)).unwrap();
                        assert_eq!(root.observe(&vm.host_context()), CaptureRootState::Unavailable);
                    } else if internal || detach {
                        let frame = vm.instance.execution_frames.last().unwrap();
                        let slot = vm.program.callable_prototypes[frame.prototype_id.unwrap() as usize].capture_slots[0] as u8;
                        if internal { vm.store_local_with_drop_contract(slot, Value::Int(handle.raw() as i64)).unwrap(); }
                        if detach { vm.detach_local_with_drop_contract(slot).unwrap(); }
                        assert_eq!(root.observe(&vm.host_context()), CaptureRootState::Unavailable);
                        assert!(vm.capture_certification_stats().unwrap().valid);
                        assert!(matches!(vm.host_context().checked_move_resource_capture::<BoxPayload>(&key()), Err(CaptureQueryError::Unavailable)));
                    } else {
                        let resources_before = vm.host_context().resource_count();
                        let token = vm.host_context().push_resource(BoxPayload(9)).unwrap();
                        {
                            let context = vm.host_context();
                            let candidate = context.typed_resource_with_key::<BoxPayload>(token.handle(), &key()).unwrap();
                            assert_eq!(context.resource(&candidate).unwrap().get().0, 9);
                            assert_ne!(candidate.handle(), handle, "a valid typed candidate is not proof of the captured owner");
                            assert_eq!(context.resource_count(), resources_before + 1);
                        }
                        vm.host_context().close_resource::<BoxPayload>(token.handle(), super::super::resource::ResourceCloseReason::Requested).unwrap();
                        assert_eq!(vm.host_context().resource_count(), resources_before);
                        assert_eq!(root.observe(&vm.host_context()), CaptureRootState::Live);
                    }
                    let mut context = vm.host_context();
                    let state = context.module_state_mut::<ProbeState>().unwrap();
                    state.root = Some(root); state.handle = Some(handle);
                }
                vm.host_context().module_state_mut::<ProbeState>().unwrap().calls += 1;
                Ok(super::super::CallOutcome::Return(super::super::CallReturn::one(Value::Int(41))))
            }).unwrap();
        }
        for schema in crate::catalog_import_schemas(&catalog, "neutral::close") {
            registry
                .register_exact_static("neutral::close", 1, schema, |vm, args| {
                    let Value::Int(raw) = args[0] else {
                        panic!("resource carrier")
                    };
                    vm.host_context()
                        .close_resource::<BoxPayload>(
                            ResourceHandle::from_raw(raw as u64).unwrap(),
                            super::super::resource::ResourceCloseReason::Requested,
                        )
                        .unwrap();
                    Ok(super::super::CallOutcome::Return(
                        super::super::CallReturn::none(),
                    ))
                })
                .unwrap();
        }
        registry.bind_vm_cached(&mut vm).unwrap();
        vm
    }
    const NO_SELF: &str = "let owner = neutral::open(); || if true => { neutral::check(); 41 } else => { neutral::close(owner); 41 };";
    fn take_closure(vm: &mut Vm) -> Value {
        assert_eq!(vm.run().unwrap(), VmStatus::Halted);
        let value = vm.instance.stack.pop().unwrap();
        let Value::Callable(callable) = &value else {
            panic!("source closure")
        };
        assert!(
            vm.program.callable_prototypes[callable.prototype_id as usize]
                .self_slot
                .is_none()
        );
        vm.host_context()
            .module_state_mut::<ProbeState>()
            .unwrap()
            .environment = callable.env.as_ref().map(Arc::downgrade);
        value
    }
    #[test]
    fn no_self_actual_frame_cell_survives_expired_environment_then_releases() {
        let mut vm = fixture(NO_SELF, true);
        let closure = take_closure(&mut vm);
        assert_eq!(vm.invoke_callable(closure, &[]).unwrap(), Value::Int(41));
        let root = vm
            .host_context()
            .module_state::<ProbeState>()
            .unwrap()
            .root
            .clone()
            .unwrap();
        assert_eq!(root.observe(&vm.host_context()), CaptureRootState::Released);
        assert_eq!(vm.host_context().resource_count(), 1);
        assert!(vm.instance.capture_cells.is_empty());
        assert_eq!(
            vm.host_context()
                .module_state::<ProbeState>()
                .unwrap()
                .calls,
            1
        );
    }
    #[test]
    fn public_same_bits_write_revokes_epoch_before_write_and_reset_does_not_restore_it() {
        let mut vm = fixture(NO_SELF, true);
        vm.host_context()
            .module_state_mut::<ProbeState>()
            .unwrap()
            .write_same_bits = true;
        let closure = take_closure(&mut vm);
        vm.invoke_callable(closure, &[]).unwrap();
        let root = vm
            .host_context()
            .module_state::<ProbeState>()
            .unwrap()
            .root
            .clone()
            .unwrap();
        assert_eq!(
            root.observe(&vm.host_context()),
            CaptureRootState::Unavailable
        );
        assert!(!vm.capture_certification_stats().unwrap().valid);
        vm.reset_for_reuse().unwrap();
        assert_eq!(
            root.observe(&vm.host_context()),
            CaptureRootState::Unavailable
        );
    }
    #[test]
    fn uncertified_compilation_has_legacy_execution_without_capture_authority() {
        let mut vm = fixture(NO_SELF, false);
        vm.host_context()
            .module_state_mut::<ProbeState>()
            .unwrap()
            .expected = Some(CaptureQueryError::Unavailable);
        let closure = take_closure(&mut vm);
        assert_eq!(vm.invoke_callable(closure, &[]).unwrap(), Value::Int(41));
        assert!(vm.capture_certification_stats().is_none());
        assert_eq!(
            vm.host_context()
                .module_state::<ProbeState>()
                .unwrap()
                .calls,
            1
        );
    }
    #[cfg(feature = "cranelift-jit")]
    #[test]
    fn loading_legacy_aot_artifact_revokes_source_origin_but_executes() {
        let mut vm = fixture(NO_SELF, true);
        let bytes = vm.encode_aot_artifact().unwrap();
        assert!(vm.capture_certification_stats().unwrap().valid);
        vm.load_aot_artifact(&bytes).unwrap();
        assert!(
            !vm.capture_certification_stats().unwrap().valid,
            "legacy native bytes cannot retain compiler issuance authority"
        );
        vm.host_context()
            .module_state_mut::<ProbeState>()
            .unwrap()
            .expected = Some(CaptureQueryError::Unavailable);
        let closure = take_closure(&mut vm);
        assert_eq!(vm.invoke_callable(closure, &[]).unwrap(), Value::Int(41));
        assert!(vm.aot_exec_count() > 0);
        println!("legacy artifact native entries={}", vm.aot_exec_count());
        assert_eq!(
            vm.host_context()
                .module_state::<ProbeState>()
                .unwrap()
                .calls,
            1
        );
    }
    #[cfg(feature = "cranelift-jit")]
    #[test]
    fn source_certified_aot_native_execution_keeps_actual_capture_query() {
        let mut vm = fixture(NO_SELF, true);
        vm.compile_aot().unwrap();
        let closure = take_closure(&mut vm);
        assert_eq!(vm.invoke_callable(closure, &[]).unwrap(), Value::Int(41));
        assert!(vm.aot_exec_count() > 0);
        println!("{}", vm.dump_aot_info());
    }
    #[cfg(feature = "cranelift-jit")]
    #[test]
    fn source_certified_trace_jit_loop_then_capture_query() {
        let source = "let owner = neutral::open(); fn dispatch() -> int { let mut n = 0; while n < 500 { n = n + 1; } neutral::check(); if true => { 41 } else => { neutral::close(owner); 41 } } dispatch();";
        let mut vm = fixture(source, true);
        let config = crate::JitConfig {
            enabled: true,
            hot_loop_threshold: 2,
            ..Default::default()
        };
        vm.set_jit_config(config);
        assert_eq!(vm.run().unwrap(), VmStatus::Halted);
        assert!(vm.jit_snapshot().metrics.native_trace_exec_count > 0);
        println!("{:?}", vm.jit_snapshot().metrics);
    }
    #[test]
    fn final_external_alias_dropped_during_host_call_leaves_frame_cell_live() {
        let mut vm = fixture(NO_SELF, true);
        let closure = take_closure(&mut vm);
        vm.host_context()
            .module_state_mut::<ProbeState>()
            .unwrap()
            .held_alias = Some(closure.clone());
        vm.invoke_callable(closure, &[]).unwrap();
        let root = vm
            .host_context()
            .module_state::<ProbeState>()
            .unwrap()
            .root
            .clone()
            .unwrap();
        assert_eq!(root.observe(&vm.host_context()), CaptureRootState::Released);
        assert_eq!(vm.host_context().resource_count(), 1);
        assert!(vm.instance.capture_cells.is_empty());
    }
    #[test]
    fn internal_same_bits_replacement_invalidates_only_the_cell_origin() {
        let mut vm = fixture(NO_SELF, true);
        vm.host_context()
            .module_state_mut::<ProbeState>()
            .unwrap()
            .internal_same_bits = true;
        let closure = take_closure(&mut vm);
        vm.invoke_callable(closure, &[]).unwrap();
        let root = vm
            .host_context()
            .module_state::<ProbeState>()
            .unwrap()
            .root
            .clone()
            .unwrap();
        assert_eq!(
            root.observe(&vm.host_context()),
            CaptureRootState::Unavailable
        );
        assert!(vm.capture_certification_stats().unwrap().valid);
        assert_eq!(vm.host_context().resource_count(), 1);
    }
    #[test]
    fn detached_cell_root_is_unavailable_without_automatic_resource_close() {
        let mut vm = fixture(NO_SELF, true);
        vm.host_context()
            .module_state_mut::<ProbeState>()
            .unwrap()
            .detach = true;
        let closure = take_closure(&mut vm);
        vm.invoke_callable(closure, &[]).unwrap();
        let root = vm
            .host_context()
            .module_state::<ProbeState>()
            .unwrap()
            .root
            .clone()
            .unwrap();
        assert_eq!(
            root.observe(&vm.host_context()),
            CaptureRootState::Unavailable
        );
        assert_eq!(vm.host_context().resource_count(), 1);
        assert!(vm.instance.capture_cells.is_empty());
    }
    #[test]
    fn aliases_share_one_real_root_and_foreign_scope_cannot_observe_it() {
        let mut vm = fixture(NO_SELF, true);
        let closure = take_closure(&mut vm);
        let alias = closure.clone();
        vm.host_context()
            .module_state_mut::<ProbeState>()
            .unwrap()
            .environment = None;
        vm.invoke_callable(closure, &[]).unwrap();
        let root = vm
            .host_context()
            .module_state::<ProbeState>()
            .unwrap()
            .root
            .clone()
            .unwrap();
        assert_eq!(root.observe(&vm.host_context()), CaptureRootState::Live);
        assert_eq!(vm.capture_certification_stats().unwrap().registrations, 1);
        let mut foreign = fixture(NO_SELF, true);
        assert_eq!(
            root.observe(&foreign.host_context()),
            CaptureRootState::Unavailable
        );
        drop(alias);
        assert_eq!(root.observe(&vm.host_context()), CaptureRootState::Released);
    }
    #[test]
    fn two_direct_move_resources_are_ambiguous_before_typed_borrow() {
        let source = "let first = neutral::open(); let second = neutral::open(); || if true => { neutral::check(); 41 } else => { neutral::close(first); neutral::close(second); 41 };";
        let mut vm = fixture(source, true);
        vm.host_context()
            .module_state_mut::<ProbeState>()
            .unwrap()
            .expected = Some(CaptureQueryError::AmbiguousCapture);
        let closure = take_closure(&mut vm);
        vm.invoke_callable(closure, &[]).unwrap();
        assert_eq!(
            vm.host_context()
                .module_state::<ProbeState>()
                .unwrap()
                .calls,
            1
        );
        assert_eq!(vm.capture_certification_stats().unwrap().registrations, 2);
    }
    #[test]
    fn registered_fake_callable_with_genuine_environment_has_no_issuance_origin() {
        let mut vm = fixture(NO_SELF, true);
        let closure = take_closure(&mut vm);
        let Value::Callable(genuine) = &closure else {
            unreachable!()
        };
        let fake = Arc::new(CallableValue {
            prototype_id: genuine.prototype_id,
            kind: genuine.kind,
            env: genuine.env.clone(),
        });
        // The existing identity registry can admit a native/adopted callable;
        // it must not mint compiler cell-issuance proof for that new identity.
        super::super::callable_maintenance::register(
            &mut vm.instance.owned_callables,
            &mut vm.host.module_state_store,
            Arc::downgrade(&fake),
        );
        vm.host_context()
            .module_state_mut::<ProbeState>()
            .unwrap()
            .expected = Some(CaptureQueryError::Unavailable);
        vm.invoke_callable(Value::Callable(fake), &[]).unwrap();
        assert_eq!(
            vm.host_context()
                .module_state::<ProbeState>()
                .unwrap()
                .calls,
            1
        );
        assert_eq!(vm.capture_certification_stats().unwrap().registrations, 1);
    }
    #[test]
    fn explicit_close_and_slot_reuse_do_not_restore_old_observation() {
        let mut vm = fixture(NO_SELF, true);
        let closure = take_closure(&mut vm);
        vm.invoke_callable(closure, &[]).unwrap();
        let context = vm.host_context();
        let state = context.module_state::<ProbeState>().unwrap();
        let root = state.root.clone().unwrap();
        let handle = state.handle.unwrap();

        vm.host_context()
            .close_resource::<BoxPayload>(
                handle,
                super::super::resource::ResourceCloseReason::Requested,
            )
            .unwrap();
        assert_eq!(
            root.observe(&vm.host_context()),
            CaptureRootState::Unavailable
        );
        let fresh = vm.host_context().push_resource(BoxPayload(41)).unwrap();
        assert_ne!(fresh.handle(), handle);
        assert_eq!(
            root.observe(&vm.host_context()),
            CaptureRootState::Unavailable
        );
        assert_eq!(vm.host_context().resource_count(), 1);
    }
    #[test]
    fn legacy_vmbc_bytes_execute_without_recreating_capture_authority() {
        let mut vm = fixture(NO_SELF, true);
        let bytes = crate::vmbc::encode_program(vm.program()).unwrap();
        let decoded = crate::vmbc::decode_program(&bytes).unwrap();
        assert_eq!(crate::vmbc::encode_program(&decoded).unwrap(), bytes);
        // Preserve the exact catalog handlers, but deliberately use a fresh
        // ordinary instance; no private compiler pair crosses the wire.
        let mut legacy = fixture(NO_SELF, false);
        legacy.program = Arc::new(decoded);
        legacy
            .host_context()
            .module_state_mut::<ProbeState>()
            .unwrap()
            .expected = Some(CaptureQueryError::Unavailable);
        let closure = take_closure(&mut legacy);
        assert_eq!(
            legacy.invoke_callable(closure, &[]).unwrap(),
            Value::Int(41)
        );
        assert!(legacy.capture_certification_stats().is_none());
        assert!(
            vm.host_context()
                .checked_move_resource_capture::<BoxPayload>(&key())
                .is_err()
        );
    }
    #[test]
    fn fixed_live_cell_history_has_fixed_four_probe_registration_work() {
        let source = "pub fn factory() { let owner = neutral::open(); || if true => { neutral::check(); 41 } else => { neutral::close(owner); 41 } }";
        let mut vm = fixture(source, true);
        vm.run().unwrap();
        let factory = vm.resolve_exported_callable("factory").unwrap();
        let live = (0..3)
            .map(|_| vm.invoke_callable(factory.clone(), &[]).unwrap())
            .collect::<Vec<_>>();
        let mut max_entries = 0;
        for _ in 0..2048 {
            let closure = vm.invoke_callable(factory.clone(), &[]).unwrap();
            let Value::Callable(callable) = &closure else {
                panic!("factory closure")
            };
            vm.host_context()
                .module_state_mut::<ProbeState>()
                .unwrap()
                .environment = callable.env.as_ref().map(Arc::downgrade);
            vm.invoke_callable(closure, &[]).unwrap();
            let context = vm.host_context();
            let state = context.module_state::<ProbeState>().unwrap();
            let root = state.root.clone().unwrap();
            let handle = state.handle.unwrap();
            assert_eq!(root.observe(&context), CaptureRootState::Released);

            vm.host_context()
                .close_resource::<BoxPayload>(
                    handle,
                    super::super::resource::ResourceCloseReason::Requested,
                )
                .unwrap();
            let stats = vm.capture_certification_stats().unwrap();
            max_entries = max_entries.max(stats.entries);
            assert!(stats.entries <= 12, "{stats:?}");
            assert!(stats.probes <= stats.registrations * 4, "{stats:?}");
            assert_eq!(vm.host_context().resource_count(), 3);
        }
        let stats = vm.capture_certification_stats().unwrap();
        assert_eq!(stats.registrations, 2051);
        assert!(stats.capacity <= 16, "{stats:?}");
        println!("max_entries={max_entries} {stats:?}");
        drop(live);
    }
}
