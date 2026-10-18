use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::Value;
use crate::compiler::TypeSchema;
use crate::host_api::HostTypeSchema;
use crate::result::{PendingScriptResource, ScriptValueContext, VmScriptArgs, VmScriptValue};

use super::execution_scope::ExecutionScopeError;
use super::{EpochCheckpoint, EpochHandle, FuelCheckpoint, Vm, VmError, VmResult, VmStatus};

mod publication;

/// Lightweight Wasmtime-style store wrapper for VM state and host context data.
pub struct Store<T = ()> {
    vm: Vm,
    data: T,
    callback_registry: CallbackRegistryOwner,
}

struct CallbackRegistryOwner(Arc<AtomicBool>);

impl Drop for CallbackRegistryOwner {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

pub trait ScriptArgs {
    const ARITY: Option<usize>;

    fn into_values(self) -> Vec<Value>;

    fn schemas() -> Option<Vec<TypeSchema>> {
        None
    }
}

pub trait IntoScriptValue {
    fn into_script_value(self) -> Value;

    fn schema() -> Option<TypeSchema> {
        None
    }
}

pub trait ScriptResult: Sized {
    fn from_value(value: Value) -> VmResult<Self>;

    fn schema() -> Option<TypeSchema> {
        None
    }
}

pub struct ScriptCallback<Args = Vec<Value>, Ret = Value> {
    registry: Arc<AtomicBool>,
    callable: Value,
    schema: Option<TypeSchema>,
    subscription: Arc<AtomicBool>,
    marker: PhantomData<fn(Args) -> Ret>,
}

pub struct QueuedScriptInvocation {
    registry: Arc<AtomicBool>,
    subscription: Arc<AtomicBool>,
    callable: Value,
    args: Vec<Value>,
}

impl<Args, Ret> Clone for ScriptCallback<Args, Ret> {
    fn clone(&self) -> Self {
        Self {
            registry: Arc::clone(&self.registry),
            callable: self.callable.clone(),
            schema: self.schema.clone(),
            subscription: Arc::clone(&self.subscription),
            marker: PhantomData,
        }
    }
}

impl<Args, Ret> ScriptCallback<Args, Ret> {
    pub fn schema(&self) -> Option<&TypeSchema> {
        self.schema.as_ref()
    }

    pub fn is_subscribed(&self) -> bool {
        self.registry.load(Ordering::Acquire) && self.subscription.load(Ordering::Acquire)
    }

    pub fn unsubscribe(&self) {
        self.subscription.store(false, Ordering::Release);
    }
}

impl<Args: ScriptArgs, Ret: ScriptResult> ScriptCallback<Args, Ret> {
    pub fn call<T>(&self, store: &mut Store<T>, args: Args) -> VmResult<Ret> {
        let invocation = self.prepare(args)?;
        store.validate_invocation(&invocation)?;
        Ret::from_value(
            store
                .vm
                .invoke_callable(invocation.callable, &invocation.args)?,
        )
    }

    pub fn start<T>(&self, store: &mut Store<T>, args: Args) -> VmResult<VmStatus> {
        let invocation = self.prepare(args)?;
        store.validate_invocation(&invocation)?;
        store
            .vm
            .start_callable(invocation.callable, &invocation.args)
    }

    pub fn prepare(&self, args: Args) -> VmResult<QueuedScriptInvocation> {
        if !self.registry.load(Ordering::Acquire) {
            return Err(VmError::InvalidFrameState(
                "script callback registry is invalidated",
            ));
        }
        if !self.subscription.load(Ordering::Acquire) {
            return Err(VmError::InvalidFrameState(
                "script callback is unsubscribed",
            ));
        }
        Ok(QueuedScriptInvocation {
            registry: Arc::clone(&self.registry),
            subscription: Arc::clone(&self.subscription),
            callable: self.callable.clone(),
            args: args.into_values(),
        })
    }
}

impl ScriptValueContext<'_> {
    /// Inserts one owned value through the normal typed publication authority.
    /// Conversion/admission failure rolls this insertion back; after admission
    /// the journal reclaims only abandoned owners when invocation completes.
    pub fn publish_owned<T: crate::HostResourceType>(
        &mut self,
        value: crate::ResourceOwned<T>,
    ) -> VmResult<Value> {
        let value = crate::result::publish_owned(
            self.vm,
            value,
            &<crate::ResourceOwned<T> as VmScriptValue>::schema()?,
            Ok,
        )?;
        let handle = crate::result::resource_handle(&value)?;
        self.pending.push(PendingScriptResource {
            handle,
            key: T::KEY,
            close: close_unpublished::<T>,
        });
        Ok(value)
    }

    fn rollback(&mut self, primary: VmError) -> VmError {
        let mut failures = Vec::new();
        for pending in self.pending.drain(..).rev() {
            if let Err(error) = (pending.close)(self.vm, pending.handle) {
                failures.push(error.to_string());
            }
        }
        if failures.is_empty() {
            primary
        } else {
            VmError::HostError(format!(
                "{primary}; callback publication rollback: {}",
                failures.join("; ")
            ))
        }
    }
}

fn close_unpublished<T: crate::HostResourceType>(
    vm: &mut Vm,
    handle: crate::ResourceHandle,
) -> VmResult<()> {
    // Frame admission can reject after the normal abort protocol retired the
    // entry. Never reinterpret a stale handle as a new generation to close.
    let key = crate::ResourceTypeKey::new(T::KEY)
        .map_err(|error| VmError::HostError(error.to_string()))?;
    if vm
        .host_context()
        .typed_resource_with_key::<T>(handle, &key)
        .is_ok()
    {
        vm.host_context()
            .close_resource::<T>(handle, crate::ResourceCloseReason::Requested)
            .map_err(|error| VmError::HostError(error.to_string()))?;
    }
    Ok(())
}

impl<T: crate::HostResourceType> VmScriptValue for crate::ResourceOwned<T> {
    fn schema() -> VmResult<HostTypeSchema> {
        crate::ResourceTypeKey::new(T::KEY)
            .map(HostTypeSchema::Resource)
            .map_err(|error| VmError::HostError(error.to_string()))
    }
    fn into_value(self, context: &mut ScriptValueContext<'_>) -> VmResult<Value> {
        context.publish_owned(self)
    }
    fn from_value(vm: &mut Vm, value: Value) -> VmResult<Self> {
        let key = crate::ResourceTypeKey::new(T::KEY)
            .map_err(|error| VmError::HostError(error.to_string()))?;
        vm.host_context()
            .take_resource_with_key::<T>(crate::result::resource_handle(&value)?, &key)
            .map(crate::ResourceOwned::new)
            .map_err(|error| VmError::HostError(error.to_string()))
    }
}

macro_rules! impl_vm_script_scalar {
    ($ty:ty, $schema:ident) => {
        impl VmScriptValue for $ty {
            fn schema() -> VmResult<HostTypeSchema> {
                Ok(HostTypeSchema::$schema)
            }
            fn into_value(self, _: &mut ScriptValueContext<'_>) -> VmResult<Value> {
                Ok(crate::IntoVmValue::into_vm_value(self))
            }
            fn from_value(_: &mut Vm, value: Value) -> VmResult<Self> {
                crate::FromVmValue::from_vm_value(&value, "callback result")
            }
        }
    };
}
impl_vm_script_scalar!(i64, Int);
impl_vm_script_scalar!(bool, Bool);
impl_vm_script_scalar!(String, String);
impl_vm_script_scalar!((), Null);

impl<T: VmScriptValue, E: VmScriptValue> VmScriptValue for crate::GuestResult<T, E> {
    fn schema() -> VmResult<HostTypeSchema> {
        Ok(HostTypeSchema::Result {
            ok: Box::new(T::schema()?),
            err: Box::new(E::schema()?),
        })
    }
    fn into_value(self, context: &mut ScriptValueContext<'_>) -> VmResult<Value> {
        match self {
            Self::Ok(value) => Ok(crate::result::carrier(true, value.into_value(context)?)),
            Self::Err(value) => Ok(crate::result::carrier(false, value.into_value(context)?)),
        }
    }
    fn from_value(vm: &mut Vm, value: Value) -> VmResult<Self> {
        vm.validate_guest_result_value(&value, &Self::schema()?)?;
        let (tag, payload) = crate::result::payload(&value)?;
        if tag {
            T::from_value(vm, payload.clone()).map(Self::Ok)
        } else {
            E::from_value(vm, payload.clone()).map(Self::Err)
        }
    }
}

impl<T: VmScriptValue> VmScriptValue for Option<T> {
    fn schema() -> VmResult<HostTypeSchema> {
        Ok(HostTypeSchema::Optional(Box::new(T::schema()?)))
    }
    fn into_value(self, context: &mut ScriptValueContext<'_>) -> VmResult<Value> {
        self.map_or(Ok(Value::Null), |value| value.into_value(context))
    }
    fn from_value(vm: &mut Vm, value: Value) -> VmResult<Self> {
        if matches!(value, Value::Null) {
            Ok(None)
        } else {
            T::from_value(vm, value).map(Some)
        }
    }
}

impl VmScriptArgs for () {
    fn schemas() -> VmResult<Vec<HostTypeSchema>> {
        Ok(Vec::new())
    }
    fn into_values(self, _: &mut ScriptValueContext<'_>) -> VmResult<Vec<Value>> {
        Ok(Vec::new())
    }
}
macro_rules! impl_vm_script_args {
    ($($name:ident),+) => {
        impl<$($name: VmScriptValue),+> VmScriptArgs for ($($name,)+) {
            fn schemas() -> VmResult<Vec<HostTypeSchema>> { Ok(vec![$($name::schema()?),+]) }
            #[allow(non_snake_case)]
            fn into_values(self, context: &mut ScriptValueContext<'_>) -> VmResult<Vec<Value>> {
                let ($($name,)+) = self;
                Ok(vec![$($name.into_value(context)?),+])
            }
        }
    };
}
impl_vm_script_args!(A);
impl_vm_script_args!(A, B);
impl_vm_script_args!(A, B, C);
impl_vm_script_args!(A, B, C, D);

impl<Args: VmScriptArgs, Ret: VmScriptValue> ScriptCallback<Args, Ret> {
    /// Synchronously invokes an RSS callback using live-VM owned conversions.
    /// Context-free prepare/start/queued callbacks retain their existing API.
    /// A native handler retaining an owned resource in opaque Rust state must
    /// take it through the typed resource API (or a `ResourceOwned` parameter).
    /// Copying an encoded integer token alone does not transfer ownership.
    /// If bounded ownership inspection fails, execution roots are retained and
    /// the VM requires explicit shutdown/reset; no successful abort is claimed.
    pub fn call_with_vm<T>(&self, store: &mut Store<T>, args: Args) -> VmResult<Ret> {
        let invocation = QueuedScriptInvocation {
            registry: Arc::clone(&self.registry),
            subscription: Arc::clone(&self.subscription),
            callable: self.callable.clone(),
            args: Vec::new(),
        };
        store.validate_invocation(&invocation)?;
        store.vm.ensure_scope_ready()?;
        // Reuse the ordinary callback's prototype/identity/schema authority.
        store.vm_script_callback::<Args, Ret>(self.callable.clone())?;
        if !store.vm.instance.execution_frames.is_empty() {
            return Err(VmError::InvalidFrameState(
                "host invocation requires a halted VM",
            ));
        }
        let mut context = ScriptValueContext {
            vm: &mut store.vm,
            pending: Vec::new(),
        };
        let values = match args.into_values(&mut context).and_then(|values| {
            let schemas = Args::schemas()?;
            if values.len() != schemas.len() {
                return Err(VmError::TypeMismatch("callback argument count"));
            }
            for (value, schema) in values.iter().zip(&schemas) {
                context.vm.validate_guest_result_value(value, schema)?;
            }
            Ok(values)
        }) {
            Ok(values) => values,
            Err(error) => return Err(context.rollback(error)),
        };
        let argc = match u8::try_from(values.len()) {
            Ok(argc) => argc,
            Err(_) => {
                return Err(context.rollback(VmError::InvalidFrameState("too many arguments")));
            }
        };
        let stack_base = context.vm.instance.stack.len();
        let frame_count = context.vm.instance.execution_frames.len();
        let local_base = context.vm.instance.locals.len();
        context.vm.instance.stack.push(self.callable.clone());
        context.vm.instance.stack.extend(values);
        context.vm.instance.host_return = None;
        // This is the same frame-entry seam as Vm::start_callable. The narrow
        // RSS-only hook separates rejection from execution without guessing
        // from errors or closing guest-owned entries after execution starts.
        if let Err(error) = context.vm.execute_call_value(argc, None) {
            context.vm.abort_host_invocation(stack_base, frame_count);
            return Err(context.rollback(error));
        }
        if let Some(frame) = context.vm.instance.execution_frames.last_mut() {
            frame.continuation = super::FrameContinuation::ReturnToHost;
        }
        let result = match context.vm.run_internal(None, false) {
            Ok(VmStatus::Halted) => {
                if !context.pending.is_empty() {
                    // Keep the actual return rooted until classification is
                    // complete: it and surviving captures retain their owners.
                    let abandoned = publication::abandoned(
                        context.vm,
                        &context.pending,
                        stack_base,
                        local_base,
                        frame_count,
                    )
                    .map_err(|projection| {
                        context.vm.instance.callback_publication_unresolved = true;
                        VmError::HostError(format!(
                            "callback publication ownership unresolved (execution roots retained; explicit shutdown/reset required): {projection}"
                        ))
                    })?;
                    for pending in context.pending.drain(..).rev() {
                        if abandoned.contains(&pending.handle) {
                            // A close failure stays tracked in the existing
                            // resource table; preserve success and try siblings.
                            let _ = (pending.close)(context.vm, pending.handle);
                        }
                    }
                }
                context
                    .vm
                    .instance
                    .host_return
                    .take()
                    .ok_or(VmError::InvalidFrameState(
                        "host invocation completed without a result",
                    ))
            }
            Ok(VmStatus::Yielded) => Err(VmError::InvalidFrameState("host invocation yielded")),
            Ok(VmStatus::Waiting(_)) => {
                Err(VmError::InvalidFrameState("host invocation is waiting"))
            }
            Err(error) => Err(error),
        };
        let value = match result {
            Ok(value) => value,
            Err(error) => {
                if context.pending.is_empty() {
                    context.vm.abort_host_invocation(stack_base, frame_count);
                    return Err(error);
                }
                let abandoned = match publication::abandoned(
                    context.vm,
                    &context.pending,
                    stack_base,
                    local_base,
                    frame_count,
                ) {
                    Ok(abandoned) => abandoned,
                    Err(projection) => {
                        // Preserve the actual topology when ownership cannot
                        // be proven. Do not drain roots or claim reclamation.
                        context.vm.instance.callback_publication_unresolved = true;
                        return Err(VmError::HostError(format!(
                            "{error}; callback publication ownership unresolved (execution roots retained; explicit shutdown/reset required): {projection}"
                        )));
                    }
                };
                // Project before draining roots, then let the ordinary abort
                // cancel waiting operations before beginning resource close.
                // Pending closes remain in the existing table for real polls.
                context.vm.abort_host_invocation(stack_base, frame_count);
                for pending in context.pending.drain(..).rev() {
                    if abandoned.contains(&pending.handle) {
                        // Keep the primary transport error and still try
                        // eligible siblings after a close failure.
                        let _ = (pending.close)(context.vm, pending.handle);
                    }
                }
                return Err(error);
            }
        };
        context
            .vm
            .validate_guest_result_value(&value, &Ret::schema()?)?;
        Ret::from_value(context.vm, value)
    }
}

impl ScriptArgs for () {
    const ARITY: Option<usize> = Some(0);

    fn into_values(self) -> Vec<Value> {
        Vec::new()
    }

    fn schemas() -> Option<Vec<TypeSchema>> {
        Some(Vec::new())
    }
}

impl ScriptArgs for Vec<Value> {
    const ARITY: Option<usize> = None;

    fn into_values(self) -> Vec<Value> {
        self
    }
}

impl ScriptArgs for Value {
    const ARITY: Option<usize> = Some(1);

    fn into_values(self) -> Vec<Value> {
        vec![self]
    }
}

macro_rules! impl_script_args_tuple {
    ($arity:expr; $($name:ident),+) => {
        impl<$($name),+> ScriptArgs for ($($name,)+)
        where
            $($name: IntoScriptValue,)+
        {
            const ARITY: Option<usize> = Some($arity);

            #[allow(non_snake_case)]
            fn into_values(self) -> Vec<Value> {
                let ($($name,)+) = self;
                vec![$($name.into_script_value(),)+]
            }

            fn schemas() -> Option<Vec<TypeSchema>> {
                Some(vec![$($name::schema()?),+])
            }
        }
    };
}

impl_script_args_tuple!(1; A);
impl_script_args_tuple!(2; A, B);
impl_script_args_tuple!(3; A, B, C);
impl_script_args_tuple!(4; A, B, C, D);

impl IntoScriptValue for Value {
    fn into_script_value(self) -> Value {
        self
    }
}

impl IntoScriptValue for i64 {
    fn into_script_value(self) -> Value {
        Value::Int(self)
    }

    fn schema() -> Option<TypeSchema> {
        Some(TypeSchema::Int)
    }
}

impl IntoScriptValue for bool {
    fn into_script_value(self) -> Value {
        Value::Bool(self)
    }

    fn schema() -> Option<TypeSchema> {
        Some(TypeSchema::Bool)
    }
}

impl IntoScriptValue for String {
    fn into_script_value(self) -> Value {
        Value::string(self)
    }

    fn schema() -> Option<TypeSchema> {
        Some(TypeSchema::String)
    }
}

impl IntoScriptValue for &str {
    fn into_script_value(self) -> Value {
        Value::string(self)
    }

    fn schema() -> Option<TypeSchema> {
        Some(TypeSchema::String)
    }
}

impl ScriptResult for Value {
    fn from_value(value: Value) -> VmResult<Self> {
        Ok(value)
    }
}

impl ScriptResult for () {
    fn from_value(value: Value) -> VmResult<Self> {
        match value {
            Value::Null => Ok(()),
            _ => Err(VmError::TypeMismatch("null callback result")),
        }
    }

    fn schema() -> Option<TypeSchema> {
        Some(TypeSchema::Null)
    }
}

impl ScriptResult for i64 {
    fn from_value(value: Value) -> VmResult<Self> {
        match value {
            Value::Int(value) => Ok(value),
            _ => Err(VmError::TypeMismatch("int callback result")),
        }
    }

    fn schema() -> Option<TypeSchema> {
        Some(TypeSchema::Int)
    }
}

impl ScriptResult for bool {
    fn from_value(value: Value) -> VmResult<Self> {
        match value {
            Value::Bool(value) => Ok(value),
            _ => Err(VmError::TypeMismatch("bool callback result")),
        }
    }

    fn schema() -> Option<TypeSchema> {
        Some(TypeSchema::Bool)
    }
}

impl<T: IntoScriptValue, E: IntoScriptValue> IntoScriptValue for crate::GuestResult<T, E> {
    fn into_script_value(self) -> Value {
        match self {
            Self::Ok(value) => crate::result::carrier(true, value.into_script_value()),
            Self::Err(error) => crate::result::carrier(false, error.into_script_value()),
        }
    }

    fn schema() -> Option<TypeSchema> {
        Some(TypeSchema::Result {
            ok: Box::new(T::schema()?),
            err: Box::new(E::schema()?),
        })
    }
}

impl<T: ScriptResult, E: ScriptResult> ScriptResult for crate::GuestResult<T, E> {
    fn from_value(value: Value) -> VmResult<Self> {
        let (tag, payload) = crate::result::payload(&value)?;
        if tag {
            T::from_value(payload.clone()).map(Self::Ok)
        } else {
            E::from_value(payload.clone()).map(Self::Err)
        }
    }

    fn schema() -> Option<TypeSchema> {
        Some(TypeSchema::Result {
            ok: Box::new(T::schema()?),
            err: Box::new(E::schema()?),
        })
    }
}

impl ScriptResult for String {
    fn from_value(value: Value) -> VmResult<Self> {
        match value {
            Value::String(text) => Ok(text.to_string()),
            _ => Err(VmError::TypeMismatch("string callback result")),
        }
    }
    fn schema() -> Option<TypeSchema> {
        Some(TypeSchema::String)
    }
}

impl<T: IntoScriptValue> IntoScriptValue for Option<T> {
    fn into_script_value(self) -> Value {
        self.map_or(Value::Null, T::into_script_value)
    }
    fn schema() -> Option<TypeSchema> {
        Some(TypeSchema::Optional(Box::new(T::schema()?)))
    }
}

impl<T: ScriptResult> ScriptResult for Option<T> {
    fn from_value(value: Value) -> VmResult<Self> {
        if matches!(value, Value::Null) {
            Ok(None)
        } else {
            T::from_value(value).map(Some)
        }
    }
    fn schema() -> Option<TypeSchema> {
        Some(TypeSchema::Optional(Box::new(T::schema()?)))
    }
}

impl<T: IntoScriptValue> IntoScriptValue for Vec<T> {
    fn into_script_value(self) -> Value {
        Value::array(self.into_iter().map(T::into_script_value).collect())
    }
    fn schema() -> Option<TypeSchema> {
        Some(TypeSchema::Array(Box::new(T::schema()?)))
    }
}

impl<T: ScriptResult> ScriptResult for Vec<T> {
    fn from_value(value: Value) -> VmResult<Self> {
        let Value::Array(values) = value else {
            return Err(VmError::TypeMismatch("array callback result"));
        };
        values.iter().cloned().map(T::from_value).collect()
    }
    fn schema() -> Option<TypeSchema> {
        Some(TypeSchema::Array(Box::new(T::schema()?)))
    }
}

impl<T> Store<T> {
    pub fn new(mut vm: Vm, data: T) -> Self {
        let callback_registry = Arc::new(AtomicBool::new(true));
        vm.register_callback_registry(&callback_registry);
        Self {
            vm,
            data,
            callback_registry: CallbackRegistryOwner(callback_registry),
        }
    }

    pub fn vm(&self) -> &Vm {
        &self.vm
    }

    pub fn vm_mut(&mut self) -> &mut Vm {
        &mut self.vm
    }

    pub fn into_vm(self) -> Vm {
        self.vm
    }

    pub fn data(&self) -> &T {
        &self.data
    }

    pub fn data_mut(&mut self) -> &mut T {
        &mut self.data
    }

    pub fn into_data(self) -> T {
        self.data
    }

    pub fn run(&mut self) -> VmResult<VmStatus> {
        self.vm.run()
    }

    pub fn resolve_exported_callable(&self, name: &str) -> VmResult<Value> {
        self.vm.resolve_exported_callable(name)
    }

    pub fn script_callback_by_name<Args, Ret>(
        &mut self,
        name: &str,
    ) -> VmResult<ScriptCallback<Args, Ret>>
    where
        Args: ScriptArgs,
        Ret: ScriptResult,
    {
        let callable = self.resolve_exported_callable(name)?;
        self.script_callback(callable)
    }

    pub fn script_callback<Args, Ret>(
        &mut self,
        callable: Value,
    ) -> VmResult<ScriptCallback<Args, Ret>>
    where
        Args: ScriptArgs,
        Ret: ScriptResult,
    {
        self.build_script_callback(callable, Args::ARITY, Args::schemas(), Ret::schema())
    }

    fn build_script_callback<Args, Ret>(
        &mut self,
        callable: Value,
        arity: Option<usize>,
        argument_schemas: Option<Vec<TypeSchema>>,
        result_schema: Option<TypeSchema>,
    ) -> VmResult<ScriptCallback<Args, Ret>> {
        if let Some(error) = self.vm.scope_reset_error().cloned() {
            return Err(VmError::ExecutionScope(error));
        }
        if self.vm.scope_reset_pending() {
            return Err(VmError::ExecutionScope(ExecutionScopeError::ScopeClosing));
        }
        if !self.callback_registry.0.load(Ordering::Acquire) {
            self.install_callback_registry();
        }
        let Value::Callable(value) = &callable else {
            return Err(VmError::InvalidCallable);
        };
        if !self.vm.owns_callable(&callable) {
            return Err(VmError::InvalidFrameState(
                "script callable does not belong to this store",
            ));
        }
        let prototype = self
            .vm
            .program()
            .callable_prototypes
            .get(value.prototype_id as usize)
            .ok_or(VmError::InvalidCallablePrototype(value.prototype_id))?;
        if let Some(arity) = arity
            && arity != usize::from(prototype.arity)
        {
            return Err(VmError::CallableArityMismatch {
                prototype_id: value.prototype_id,
                expected: prototype.arity,
                got: u8::try_from(arity).unwrap_or(u8::MAX),
            });
        }
        if let Some(TypeSchema::Callable { params, result }) = prototype.schema.as_ref() {
            if let Some(args) = argument_schemas
                && (args.len() != params.len()
                    || !args
                        .iter()
                        .zip(params)
                        .all(|(actual, expected)| callback_schema_accepts(expected, actual)))
            {
                return Err(VmError::TypeMismatch("script callback argument schema"));
            }
            if let Some(actual) = result_schema
                && !callback_schema_accepts(result, &actual)
            {
                return Err(VmError::TypeMismatch("script callback result schema"));
            }
        }
        Ok(ScriptCallback {
            registry: Arc::clone(&self.callback_registry.0),
            callable,
            schema: prototype.schema.clone(),
            subscription: Arc::new(AtomicBool::new(true)),
            marker: PhantomData,
        })
    }

    pub fn vm_script_callback_by_name<Args: VmScriptArgs, Ret: VmScriptValue>(
        &mut self,
        name: &str,
    ) -> VmResult<ScriptCallback<Args, Ret>> {
        let callable = self.resolve_exported_callable(name)?;
        self.vm_script_callback(callable)
    }

    pub fn vm_script_callback<Args: VmScriptArgs, Ret: VmScriptValue>(
        &mut self,
        callable: Value,
    ) -> VmResult<ScriptCallback<Args, Ret>> {
        let args = Args::schemas()?;
        for schema in &args {
            schema
                .validate()
                .map_err(|error| VmError::HostError(error.to_string()))?;
        }
        let ret = Ret::schema()?;
        ret.validate()
            .map_err(|error| VmError::HostError(error.to_string()))?;
        let callback = self.build_script_callback(
            callable,
            Some(args.len()),
            Some(
                args.iter()
                    .map(HostTypeSchema::to_compiler_schema)
                    .collect(),
            ),
            Some(ret.to_compiler_schema()),
        )?;
        let Value::Callable(value) = &callback.callable else {
            return Err(VmError::InvalidCallable);
        };
        let prototype = &self.vm.program().callable_prototypes[value.prototype_id as usize];
        if !matches!(prototype.target, super::CallableTarget::ScriptFunction(_)) {
            return Err(VmError::InvalidFrameState(
                "VM-aware callbacks require an RSS callable",
            ));
        }
        if !matches!(prototype.schema, Some(TypeSchema::Callable { .. })) {
            return Err(VmError::TypeMismatch("typed script callback schema"));
        }
        Ok(callback)
    }

    pub fn enqueue_callback(&mut self, invocation: QueuedScriptInvocation) -> VmResult<()> {
        self.validate_invocation(&invocation)?;
        self.vm.queue_callable_with_subscription(
            invocation.callable,
            invocation.args,
            Some(invocation.subscription),
        )
    }

    pub fn drain_callbacks(&mut self) -> VmResult<Vec<Value>> {
        self.vm.drain_callable_queue()
    }

    pub fn take_callback_result<Ret: ScriptResult>(&mut self) -> VmResult<Option<Ret>> {
        self.vm
            .take_callable_result()
            .map(Ret::from_value)
            .transpose()
    }

    fn validate_invocation(&self, invocation: &QueuedScriptInvocation) -> VmResult<()> {
        if !Arc::ptr_eq(&invocation.registry, &self.callback_registry.0) {
            return Err(VmError::InvalidFrameState(
                "script callback belongs to another store",
            ));
        }
        if !invocation.registry.load(Ordering::Acquire) {
            return Err(VmError::InvalidFrameState(
                "script callback registry is invalidated",
            ));
        }
        if !invocation.subscription.load(Ordering::Acquire) {
            return Err(VmError::InvalidFrameState(
                "script callback is unsubscribed",
            ));
        }
        Ok(())
    }

    pub fn reset_for_reuse(&mut self) -> VmResult<()> {
        self.vm.reset_for_reuse()?;
        if !self.vm.scope_reset_pending() {
            self.install_callback_registry();
        }
        Ok(())
    }

    /// Polls a pending reset and publishes a callback registry only after the
    /// VM's old execution scope has reached clean quiescence.
    pub fn poll_reset_for_reuse(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<VmResult<()>> {
        let was_pending = self.vm.scope_reset_pending();
        match self.vm.poll_reset_for_reuse(cx) {
            std::task::Poll::Pending => std::task::Poll::Pending,
            std::task::Poll::Ready(Ok(())) => {
                if was_pending && !self.vm.scope_reset_pending() {
                    self.install_callback_registry();
                }
                std::task::Poll::Ready(Ok(()))
            }
            std::task::Poll::Ready(Err(error)) => std::task::Poll::Ready(Err(error)),
        }
    }

    /// Whether this store is safe to return to a VM reuse pool.
    pub fn is_reusable(&self) -> bool {
        self.vm.is_reusable()
    }

    pub fn replace_vm(&mut self, mut vm: Vm) {
        self.callback_registry.0.store(false, Ordering::Release);
        self.vm.shutdown();
        let callback_registry = Arc::new(AtomicBool::new(true));
        vm.register_callback_registry(&callback_registry);
        self.callback_registry = CallbackRegistryOwner(callback_registry);
        self.vm = vm;
    }

    fn install_callback_registry(&mut self) {
        let callback_registry = Arc::new(AtomicBool::new(true));
        self.vm.register_callback_registry(&callback_registry);
        self.callback_registry = CallbackRegistryOwner(callback_registry);
    }

    pub fn resume(&mut self) -> VmResult<VmStatus> {
        self.vm.resume()
    }

    pub fn set_fuel(&mut self, fuel: u64) {
        self.vm.set_fuel(fuel);
    }

    pub fn clear_fuel(&mut self) {
        self.vm.clear_fuel();
    }

    pub fn set_fuel_check_interval(&mut self, interval: u32) -> VmResult<()> {
        self.vm.set_fuel_check_interval(interval)
    }

    pub fn fuel_check_interval(&self) -> u32 {
        self.vm.fuel_check_interval()
    }

    pub fn get_fuel(&self) -> Option<u64> {
        self.vm.get_fuel()
    }

    pub fn add_fuel(&mut self, fuel: u64) -> VmResult<()> {
        self.vm.add_fuel(fuel)
    }

    pub fn recharge(&mut self, fuel: u64) -> VmResult<()> {
        self.vm.recharge_fuel(fuel)
    }

    pub fn consume_fuel(&mut self, fuel: u64) -> VmResult<()> {
        self.vm.consume_fuel(fuel)
    }

    pub fn consume_fuel_tick(&mut self) -> VmResult<()> {
        self.vm.consume_fuel_tick()
    }

    pub fn epoch_handle(&self) -> EpochHandle {
        self.vm.epoch_handle()
    }

    pub fn current_epoch(&self) -> u64 {
        self.vm.current_epoch()
    }

    pub fn increment_epoch(&self) -> u64 {
        self.vm.increment_epoch()
    }

    pub fn increment_epoch_by(&self, delta: u64) -> u64 {
        self.vm.increment_epoch_by(delta)
    }

    pub fn set_epoch_deadline(&mut self, ticks_beyond_current: u64) -> VmResult<()> {
        self.vm.set_epoch_deadline(ticks_beyond_current)
    }

    pub fn clear_epoch_deadline(&mut self) {
        self.vm.clear_epoch_deadline();
    }

    pub fn epoch_deadline(&self) -> Option<u64> {
        self.vm.epoch_deadline()
    }

    pub fn epoch_deadline_delta(&self) -> Option<u64> {
        self.vm.epoch_deadline_delta()
    }

    pub fn set_epoch_check_interval(&mut self, interval: u32) -> VmResult<()> {
        self.vm.set_epoch_check_interval(interval)
    }

    pub fn epoch_check_interval(&self) -> u32 {
        self.vm.epoch_check_interval()
    }

    pub fn consume_epoch_tick(&mut self) -> VmResult<()> {
        self.vm.consume_epoch_tick()
    }

    pub fn epoch_checkpoint(&self) -> EpochCheckpoint {
        self.vm.epoch_checkpoint()
    }

    pub fn restore_epoch(&mut self, checkpoint: EpochCheckpoint) {
        self.vm.restore_epoch(checkpoint);
    }

    pub fn fuel_checkpoint(&self) -> FuelCheckpoint {
        self.vm.fuel_checkpoint()
    }

    pub fn checkpoint(&self) -> FuelCheckpoint {
        self.vm.checkpoint()
    }

    pub fn restore_fuel(&mut self, checkpoint: FuelCheckpoint) {
        self.vm.restore_fuel(checkpoint);
    }

    pub fn restore_checkpoint(&mut self, checkpoint: FuelCheckpoint) {
        self.vm.restore_checkpoint(checkpoint);
    }
}

fn callback_schema_accepts(expected: &TypeSchema, actual: &TypeSchema) -> bool {
    matches!(expected, TypeSchema::Unknown) || expected == actual
}

impl Store<()> {
    pub fn from_vm(vm: Vm) -> Self {
        Self::new(vm, ())
    }
}
