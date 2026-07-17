use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::Value;
use crate::compiler::TypeSchema;

use super::{EpochCheckpoint, EpochHandle, FuelCheckpoint, Vm, VmError, VmResult, VmStatus};

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

impl<Args, Ret> ScriptCallback<Args, Ret>
where
    Args: ScriptArgs,
    Ret: ScriptResult,
{
    pub fn schema(&self) -> Option<&TypeSchema> {
        self.schema.as_ref()
    }

    pub fn is_subscribed(&self) -> bool {
        self.registry.load(Ordering::Acquire) && self.subscription.load(Ordering::Acquire)
    }

    pub fn unsubscribe(&self) {
        self.subscription.store(false, Ordering::Release);
    }

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
        if let Some(arity) = Args::ARITY
            && arity != usize::from(prototype.arity)
        {
            return Err(VmError::CallableArityMismatch {
                prototype_id: value.prototype_id,
                expected: prototype.arity,
                got: u8::try_from(arity).unwrap_or(u8::MAX),
            });
        }
        if let Some(TypeSchema::Callable { params, result }) = prototype.schema.as_ref() {
            if let Some(args) = Args::schemas()
                && (args.len() != params.len()
                    || !args
                        .iter()
                        .zip(params)
                        .all(|(actual, expected)| callback_schema_accepts(expected, actual)))
            {
                return Err(VmError::TypeMismatch("script callback argument schema"));
            }
            if let Some(actual) = Ret::schema()
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

    pub fn reset_for_reuse(&mut self) {
        self.vm.reset_for_reuse();
        self.install_callback_registry();
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
