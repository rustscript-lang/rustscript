#![cfg(feature = "runtime")]

use pd_host_function::pd_host_function;
use std::sync::{Arc, Mutex};
use vm::resource::{CloseProgress, HostResource, ResourceCloseReason, ResourceResult};
use vm::{
    GuestResult, HostResourceType, ResourceOwned, ResourceTypeKey, Value, Vm, VmResult, VmStatus,
};

static LOCK: Mutex<()> = Mutex::new(());
static EVENTS: Mutex<Vec<(&str, i64)>> = Mutex::new(Vec::new());
fn event(kind: &'static str, id: i64) {
    EVENTS.lock().unwrap().push((kind, id));
}
#[derive(Debug)]
struct Token(i64);
impl HostResource for Token {
    fn resource_type_key() -> Option<ResourceTypeKey> {
        Some(ResourceTypeKey::new(Self::KEY).unwrap())
    }
    fn begin_close(&mut self, _: ResourceCloseReason) -> ResourceResult<CloseProgress> {
        event("close", self.0);
        Ok(CloseProgress::Ready)
    }
}
impl HostResourceType for Token {
    const KEY: &'static str = "joined.token";
    const DESCRIPTION: &'static str = "joined Result SDK and try integration token";
}
impl Drop for Token {
    fn drop(&mut self) {
        event("drop", self.0);
    }
}
type Owned = GuestResult<ResourceOwned<Token>, ResourceOwned<Token>>;
fn owned(ok: bool, id: i64) -> Owned {
    if ok {
        GuestResult::Ok(ResourceOwned::new(Token(id)))
    } else {
        GuestResult::Err(ResourceOwned::new(Token(id)))
    }
}
fn assert_owned(value: &Owned, ok: bool, id: i64) {
    match value {
        GuestResult::Ok(value) => {
            assert!(ok);
            assert_eq!(value.as_ref().0, id);
        }
        GuestResult::Err(value) => {
            assert!(!ok);
            assert_eq!(value.as_ref().0, id);
        }
    }
}
mod generated {
    pub use vm::borrow_arg;
    pub mod functions {
        use super::super::*;
        /// Returns the active typed owner, with an observable once-only effect.
        #[pd_host_function(name = "joined::make")]
        fn make(
            ok: bool,
            id: i64,
        ) -> VmResult<GuestResult<ResourceOwned<Token>, ResourceOwned<Token>>> {
            event("make", id);
            Ok(owned(ok, id))
        }
        /// Borrows a fully typed active owner on the success continuation.
        #[pd_host_function(name = "joined::read")]
        fn read(value: GuestResult<vm::ResourceRef<'_, Token>, vm::ResourceRef<'_, Token>>) -> i64 {
            let id = match value {
                GuestResult::Ok(value) | GuestResult::Err(value) => value.get().0,
            };
            event("read", id);
            id
        }
        /// Transfers ownership to opaque host state, outside publication reclaim.
        #[pd_host_function(name = "joined::keep")]
        fn keep(
            vm: &mut Vm,
            value: GuestResult<ResourceOwned<Token>, ResourceOwned<Token>>,
        ) -> i64 {
            vm.host_context().set_module_state(value);
            1
        }
        /// Explicitly closes a native-taken active payload on the normal path.
        #[pd_host_function(name = "joined::close")]
        fn close(value: GuestResult<ResourceOwned<Token>, ResourceOwned<Token>>) -> VmResult<i64> {
            let mut value = match value {
                GuestResult::Ok(value) | GuestResult::Err(value) => value.into_inner(),
            };
            value.begin_close(ResourceCloseReason::Requested).unwrap();
            Ok(value.0)
        }
        /// Separates fatal VM transport from both guest Result branches.
        #[pd_host_function(name = "joined::tri")]
        fn tri(mode: i64) -> VmResult<GuestResult<i64, String>> {
            event("tri", mode);
            match mode {
                0 => Ok(GuestResult::Ok(7)),
                1 => Ok(GuestResult::Err("joined error".into())),
                _ => Err(vm::VmError::HostError("joined fatal".into())),
            }
        }
        /// Records effects on each side of the function-local propagation boundary.
        #[pd_host_function(name = "joined::tick")]
        fn tick(id: i64) -> i64 {
            event("tick", id);
            id
        }
    }
}
fn machine(source: &str, decoded: bool) -> Vm {
    let descriptors = vec![
        generated::functions::make_descriptor(),
        generated::functions::read_descriptor(),
        generated::functions::keep_descriptor(),
        generated::functions::close_descriptor(),
        generated::functions::tri_descriptor(),
        generated::functions::tick_descriptor(),
    ];
    let catalog = Arc::new(vm::HostFunctionDescriptor::collect_catalog(&descriptors).unwrap());
    let compiled = vm::compile_source_with_flavor_and_options(
        source,
        vm::SourceFlavor::RustScript,
        vm::CompileSourceFileOptions::default().with_host_api_catalog(catalog),
    )
    .unwrap();
    let program = compiled.program.with_local_count(compiled.locals);
    let program = if decoded {
        let bytes = vm::encode_program(&program).unwrap();
        assert_eq!(u16::from_le_bytes([bytes[4], bytes[5]]), 13);
        let decoded = vm::decode_program(&bytes).unwrap();
        assert_eq!(decoded.host_import_schemas(), program.host_import_schemas());
        assert_eq!(vm::encode_program(&decoded).unwrap(), bytes);
        decoded
    } else {
        program
    };
    let mut registry = vm::HostFunctionRegistry::empty();
    registry.set_capability_profile(vm::CapabilityProfile::deny_all());
    vm::HostModuleDescriptor::install_descriptors(&mut registry, &descriptors).unwrap();
    for descriptor in &descriptors {
        registry.authorize_registered_builtin_import(&descriptor.schema.name);
    }
    let mut machine = Vm::new(program);
    registry.bind_vm_cached(&mut machine).unwrap();
    machine.set_fuel(100_000);
    machine
}

#[cfg(feature = "cranelift-jit")]
#[test]
fn dynamic_resource_try_hot_loop_before_teardown_all_backends() {
    let _lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    for mode in ["interpreter", "aot", "trace"] {
        for ok in [true, false] {
            EVENTS.lock().unwrap().clear();
            let source = format!(
                r#"use joined;
                fn relay(flag: bool) -> Result<resource<joined.token>, resource<joined.token>> {{
                    let local = joined::make(true, 101);
                    let mut i = 0;
                    while i < 64 {{
                        let produced = joined::make(flag || i != 48, i);
                        let normalized: Result<resource<joined.token>, resource<joined.token>> = match produced {{
                            Result::Ok(v) => Result::Ok(v),
                            Result::Err(e) => Result::Err(e),
                        }};
                        let selected = normalized?;
                        let temporary: Result<resource<joined.token>, resource<joined.token>> = Result::Ok(selected);
                        let closed = joined::close(temporary);
                        i = i + 1;
                    }}
                    let local_closed = joined::close(local);
                    let after = joined::tick(2);
                    joined::make(true, 501)
                }}
                let returned = relay(joined::tick({}) == 0);
                joined::close(returned);"#,
                if ok { 0 } else { 1 }
            );
            let mut machine = machine(&source, true);
            machine.set_jit_config(vm::JitConfig {
                enabled: mode == "trace",
                hot_loop_threshold: 1,
                max_trace_len: 512,
            });
            if mode == "aot" {
                machine
                    .compile_aot()
                    .expect("required resource AOT admission");
                assert!(machine.dump_aot_info().contains("lowering=ssa"));
            }
            assert_eq!(machine.run().expect("resource runtime"), VmStatus::Halted);
            assert_eq!(
                machine.stack().last(),
                Some(&Value::Int(if ok { 501 } else { 48 }))
            );
            assert_eq!(machine.host_context().resource_count(), 0);
            let events = EVENTS.lock().unwrap().clone();
            let iterations = if ok { 64 } else { 49 };
            for id in (0..iterations)
                .chain([101])
                .chain(if ok { Some(501) } else { None })
            {
                for kind in ["make", "close", "drop"] {
                    assert_eq!(
                        events.iter().filter(|e| **e == (kind, id)).count(),
                        1,
                        "{mode} {ok} {id} {kind}: {events:?}"
                    );
                }
            }
            assert_eq!(events.contains(&("tick", 2)), ok);
            if !ok {
                let err_make = events.iter().position(|e| *e == ("make", 48)).unwrap();
                let local_close = events.iter().position(|e| *e == ("close", 101)).unwrap();
                let err_close = events.iter().position(|e| *e == ("close", 48)).unwrap();
                assert!(err_make < local_close && local_close < err_close);
                assert!(!events.iter().any(|e| e.0 == "make" && e.1 == 49));
            }
            if mode == "aot" {
                assert!(machine.aot_exec_count() > 0);
            }
            if mode == "trace" {
                eprintln!(
                    "R4 resource full trace\n{}",
                    machine
                        .dump_jit_info()
                        .lines()
                        .filter(|line| !line.trim_start().starts_with("code:"))
                        .collect::<Vec<_>>()
                        .join("\n")
                );
                let snapshot = machine.jit_snapshot();
                let relevant: Vec<_> = snapshot
                    .traces
                    .iter()
                    .filter(|t| {
                        t.executions > 0
                            && t.op_names().iter().any(|op| op == "map_get")
                            && t.ssa_text().contains("tag")
                            && t.ssa_text().contains("payload")
                    })
                    .collect();
                for trace in &relevant {
                    eprintln!(
                        "R4 resource trace root={} executions={} terminal={:?}\n{}",
                        trace.root_ip,
                        trace.executions,
                        trace.terminal,
                        trace.ssa_text()
                    );
                }
                assert!(
                    !relevant.is_empty(),
                    "required resource tag/payload native region missing: {:?}",
                    snapshot.attempts
                );
            }
            eprintln!(
                "R4 resource mode={mode} ok={ok} live_before_teardown=0 iterations={iterations} trace={} aot={} events={events:?}",
                machine.jit_native_exec_count(),
                machine.aot_exec_count()
            );
        }
    }
}

#[test]
fn generated_host_try_is_once_only_function_local_and_fatal_across_v13() {
    let _lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    for decoded in [false, true] {
        for mode in [0, 1, 2] {
            EVENTS.lock().unwrap().clear();
            let mut machine = machine(
                &format!(
                    r#"use joined;
                    fn relay(mode: int) -> Result<int, string> {{
                        let value: int = joined::tri(mode)?;
                        joined::tick(2);
                        Result::Ok(value + 1)
                    }}
                    fn outer(mode: int) -> Result<int, string> {{
                        let alias = relay;
                        let value = alias(mode);
                        joined::tick(3);
                        value
                    }}
                    outer({mode});"#
                ),
                decoded,
            );
            if mode == 2 {
                assert!(
                    machine
                        .run()
                        .unwrap_err()
                        .to_string()
                        .contains("joined fatal")
                );
                assert_eq!(*EVENTS.lock().unwrap(), [("tri", mode)]);
            } else {
                assert_eq!(machine.run().unwrap(), VmStatus::Halted);
                let value = machine.stack().last().unwrap().clone();
                let Value::Map(fields) = &value else {
                    panic!("Result carrier required")
                };
                assert_eq!(
                    fields.get(&Value::string("tag")),
                    Some(&Value::Bool(mode == 0))
                );
                let typed =
                    <GuestResult<i64, String> as vm::ScriptResult>::from_value(value).unwrap();
                assert_eq!(
                    typed,
                    if mode == 0 {
                        GuestResult::Ok(8)
                    } else {
                        GuestResult::Err("joined error".into())
                    }
                );
                assert_eq!(
                    *EVENTS.lock().unwrap(),
                    if mode == 0 {
                        vec![("tri", 0), ("tick", 2), ("tick", 3)]
                    } else {
                        vec![("tri", 1), ("tick", 3)]
                    }
                );
            }
            eprintln!(
                "joined host decoded={decoded} mode={mode} events={:?}",
                *EVENTS.lock().unwrap()
            );
        }
    }
}

const RELAY: &str = r#"use joined;
pub fn relay(value: Result<resource<joined.token>, resource<joined.token>>, sibling: Result<resource<joined.token>, resource<joined.token>>) -> Result<resource<joined.token>, resource<joined.token>> {
    let local = joined::make(true, 101);
    let selected: resource<joined.token> = value?;
    joined::read(local);
    joined::close(local);
    Result::Ok(selected)
}
pub fn nested(value: Result<resource<joined.token>, resource<joined.token>>, sibling: Result<resource<joined.token>, resource<joined.token>>) -> Result<resource<joined.token>, resource<joined.token>> {
    let alias = relay;
    alias(value, sibling)
}
pub fn host_relay(ok: bool) -> Result<resource<joined.token>, resource<joined.token>> {
    let local = joined::make(true, 101);
    let selected: resource<joined.token> = joined::make(ok, 501)?;
    joined::close(local);
    Result::Ok(selected)
}
"#;

#[test]
fn generated_owned_host_result_try_preserves_active_owner_on_v13_callback_return() {
    let _lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    for decoded in [false, true] {
        for ok in [false, true] {
            EVENTS.lock().unwrap().clear();
            let mut store = vm::Store::from_vm(machine(RELAY, decoded));
            store.run().unwrap();
            let callback = store
                .vm_script_callback_by_name::<(bool,), Owned>("host_relay")
                .unwrap();
            let returned = callback.call_with_vm(&mut store, (ok,)).unwrap();
            assert_owned(&returned, ok, 501);
            assert_eq!(store.vm_mut().host_context().resource_count(), 0);
            let events = EVENTS.lock().unwrap().clone();
            assert_eq!(events.iter().filter(|e| **e == ("make", 501)).count(), 1);
            assert_eq!(events.iter().filter(|e| **e == ("close", 101)).count(), 1);
            assert_eq!(events.iter().filter(|e| **e == ("drop", 101)).count(), 1);
            assert!(!events.contains(&("close", 501)));
            assert!(!events.contains(&("drop", 501)));
            eprintln!(
                "joined host-owner decoded={decoded} ok={ok} returned=501 live=0 events={events:?}"
            );
            drop(returned);
        }
    }
}

#[test]
fn owned_callback_try_does_not_reclaim_native_taken_sibling() {
    let _lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    for ok in [true, false] {
        EVENTS.lock().unwrap().clear();
        let source = RELAY.replacen("let local =", "joined::keep(sibling); let local =", 1);
        let mut store = vm::Store::from_vm(machine(&source, true));
        store.run().unwrap();
        let callback = store
            .vm_script_callback_by_name::<(Owned, Owned), Owned>("relay")
            .unwrap();
        let returned = callback
            .call_with_vm(&mut store, (owned(ok, 301), owned(!ok, 302)))
            .unwrap();
        assert_owned(&returned, ok, 301);
        assert_eq!(store.vm_mut().host_context().resource_count(), 0);
        let retained = store
            .vm_mut()
            .host_context()
            .take_module_state::<Owned>()
            .unwrap();
        assert_owned(&retained, !ok, 302);
        let events = EVENTS.lock().unwrap().clone();
        assert_eq!(events.iter().filter(|e| **e == ("close", 101)).count(), 1);
        for id in [301, 302] {
            assert!(!events.contains(&("close", id)));
            assert!(!events.contains(&("drop", id)));
        }
        eprintln!("joined native-taken ok={ok} returned=301 retained=302 live=0 events={events:?}");
        drop(returned);
        drop(retained);
    }
}

#[test]
fn try_callback_preserves_surviving_factory_capture_through_alias() {
    let _lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    for decoded in [false, true] {
        EVENTS.lock().unwrap().clear();
        let mut store = vm::Store::from_vm(machine(
            r#"use joined;
            pub fn factory() {
                let saved = joined::make(true, 401);
                fn callback(value: Result<resource<joined.token>, resource<joined.token>>, sibling: Result<resource<joined.token>, resource<joined.token>>, release: bool) -> Result<resource<joined.token>, resource<joined.token>> {
                    let local = joined::make(true, 101);
                    let selected: resource<joined.token> = value?;
                    if release { joined::close(saved); } else { joined::read(saved); }
                    joined::close(local);
                    Result::Ok(selected)
                }
                callback
            }
        "#,
            decoded,
        ));
        store.run().unwrap();
        let factory = store.vm_mut().resolve_exported_callable("factory").unwrap();
        let callable = store.vm_mut().invoke_callable(factory, &[]).unwrap();
        let callback = store
            .vm_script_callback::<(Owned, Owned, bool), Owned>(callable)
            .unwrap();
        let alias = callback.clone();
        for ok in [false, true, false, true] {
            let before = EVENTS.lock().unwrap().len();
            let returned = alias
                .call_with_vm(&mut store, (owned(ok, 301), owned(!ok, 302), false))
                .unwrap();
            assert_owned(&returned, ok, 301);
            assert_eq!(store.vm_mut().host_context().resource_count(), 1);
            let events = EVENTS.lock().unwrap()[before..].to_vec();
            assert_eq!(events.iter().filter(|e| **e == ("close", 101)).count(), 1);
            assert_eq!(events.iter().filter(|e| **e == ("close", 302)).count(), 1);
            assert_eq!(events.contains(&("read", 401)), ok);
            assert!(!events.contains(&("close", 401)));
            assert!(!events.contains(&("drop", 401)));
            eprintln!(
                "joined capture decoded={decoded} ok={ok} returned=301 saved=401 live=1 events={events:?}"
            );
            drop(returned);
        }
        drop(alias);
        callback.unsubscribe();
        // The capture remains a scoped host resource until explicit scope shutdown.
        store
            .vm_mut()
            .execution_scope()
            .begin_close(ResourceCloseReason::Requested)
            .unwrap();
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(
            store
                .vm_mut()
                .execution_scope()
                .poll_close(&mut cx)
                .is_ready()
        );
        assert_eq!(store.vm_mut().host_context().resource_count(), 0);
        assert_eq!(
            EVENTS
                .lock()
                .unwrap()
                .iter()
                .filter(|e| **e == ("close", 401))
                .count(),
            1
        );
    }
}

#[test]
fn owned_callback_try_ret_and_publication_reclaim_preserve_return_and_unrelated_v13() {
    let _lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    for decoded in [false, true] {
        for name in ["relay", "nested"] {
            for ok in [true, false] {
                EVENTS.lock().unwrap().clear();
                let mut store = vm::Store::from_vm(machine(RELAY, decoded));
                store.run().unwrap();
                let unrelated = store
                    .vm_mut()
                    .host_context()
                    .insert_resource(Token(900))
                    .unwrap()
                    .handle();
                let callback = store
                    .vm_script_callback_by_name::<(Owned, Owned), Owned>(name)
                    .unwrap();
                let returned = callback
                    .call_with_vm(&mut store, (owned(ok, 301), owned(!ok, 302)))
                    .unwrap();
                eprintln!(
                    "joined before assertions decoded={decoded} name={name} ok={ok} events={:?}",
                    *EVENTS.lock().unwrap()
                );
                assert_owned(&returned, ok, 301);
                assert_eq!(store.vm_mut().host_context().resource_count(), 1);
                assert_eq!(
                    store
                        .vm_mut()
                        .host_context()
                        .borrow_resource::<Token>(unrelated)
                        .unwrap()
                        .get()
                        .0,
                    900
                );
                let events = EVENTS.lock().unwrap().clone();
                for id in [101, 302] {
                    assert_eq!(
                        events.iter().filter(|e| **e == ("close", id)).count(),
                        1,
                        "{events:?}"
                    );
                    assert_eq!(
                        events.iter().filter(|e| **e == ("drop", id)).count(),
                        1,
                        "{events:?}"
                    );
                }
                assert!(!events.contains(&("close", 301)));
                assert!(!events.contains(&("drop", 301)));
                assert!(!events.contains(&("close", 900)));
                assert_eq!(events.contains(&("read", 101)), ok);
                eprintln!(
                    "joined owned decoded={decoded} path={name} ok={ok} returned=301 unrelated=900 live=1 events={events:?}"
                );
                drop(returned);
                store
                    .vm_mut()
                    .host_context()
                    .close_resource::<Token>(unrelated, ResourceCloseReason::Requested)
                    .unwrap();
                assert_eq!(store.vm_mut().host_context().resource_count(), 0);
            }
        }
    }
}
