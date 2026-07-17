use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::compiler::TypeSchema;
use crate::{ProgramInstanceId, Value};

use super::{EpochCheckpoint, EpochHandle, FuelCheckpoint, Vm, VmError, VmResult, VmStatus};

static NEXT_STORE_ID: AtomicU64 = AtomicU64::new(1);

/// Lightweight Wasmtime-style store wrapper for VM state and host context data.
pub struct Store<T = ()> {
    vm: Vm,
    data: T,
    id: u64,
}

pub trait ScriptArgs {
    const ARITY: Option<usize>;

    fn into_values(self) -> Vec<Value>;
}

pub trait IntoScriptValue {
    fn into_script_value(self) -> Value;
}

pub trait ScriptResult: Sized {
    fn from_value(value: Value) -> VmResult<Self>;
}

pub struct ScriptCallback<Args = Vec<Value>, Ret = Value> {
    store_id: u64,
    program_instance: ProgramInstanceId,
    callable: Value,
    schema: Option<TypeSchema>,
    subscribed: Arc<AtomicBool>,
    marker: PhantomData<fn(Args) -> Ret>,
}

pub struct QueuedScriptInvocation {
    store_id: u64,
    program_instance: ProgramInstanceId,
    callable: Value,
    args: Vec<Value>,
}

impl<Args, Ret> Clone for ScriptCallback<Args, Ret> {
    fn clone(&self) -> Self {
        Self {
            store_id: self.store_id,
            program_instance: self.program_instance,
            callable: self.callable.clone(),
            schema: self.schema.clone(),
            subscribed: Arc::clone(&self.subscribed),
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
        self.subscribed.load(Ordering::Acquire)
    }

    pub fn unsubscribe(&self) {
        self.subscribed.store(false, Ordering::Release);
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
        if !self.is_subscribed() {
            return Err(VmError::InvalidFrameState(
                "script callback is unsubscribed",
            ));
        }
        Ok(QueuedScriptInvocation {
            store_id: self.store_id,
            program_instance: self.program_instance,
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
}

impl IntoScriptValue for bool {
    fn into_script_value(self) -> Value {
        Value::Bool(self)
    }
}

impl IntoScriptValue for String {
    fn into_script_value(self) -> Value {
        Value::string(self)
    }
}

impl IntoScriptValue for &str {
    fn into_script_value(self) -> Value {
        Value::string(self)
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
}

impl ScriptResult for i64 {
    fn from_value(value: Value) -> VmResult<Self> {
        match value {
            Value::Int(value) => Ok(value),
            _ => Err(VmError::TypeMismatch("int callback result")),
        }
    }
}

impl ScriptResult for bool {
    fn from_value(value: Value) -> VmResult<Self> {
        match value {
            Value::Bool(value) => Ok(value),
            _ => Err(VmError::TypeMismatch("bool callback result")),
        }
    }
}

impl<T> Store<T> {
    pub fn new(vm: Vm, data: T) -> Self {
        Self {
            vm,
            data,
            id: NEXT_STORE_ID.fetch_add(1, Ordering::Relaxed),
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

    pub fn script_callback<Args, Ret>(&self, callable: Value) -> VmResult<ScriptCallback<Args, Ret>>
    where
        Args: ScriptArgs,
        Ret: ScriptResult,
    {
        let Value::Callable(value) = &callable else {
            return Err(VmError::InvalidCallable);
        };
        if value.program_instance != self.vm.program_instance_id() {
            return Err(VmError::StaleCallable {
                expected: self.vm.program_instance_id(),
                found: value.program_instance,
            });
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
        Ok(ScriptCallback {
            store_id: self.id,
            program_instance: value.program_instance,
            callable,
            schema: prototype.schema.clone(),
            subscribed: Arc::new(AtomicBool::new(true)),
            marker: PhantomData,
        })
    }

    pub fn enqueue_callback(&mut self, invocation: QueuedScriptInvocation) -> VmResult<()> {
        self.validate_invocation(&invocation)?;
        self.vm.queue_callable(invocation.callable, invocation.args)
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
        if invocation.store_id != self.id {
            return Err(VmError::InvalidFrameState(
                "script callback belongs to another store",
            ));
        }
        if invocation.program_instance != self.vm.program_instance_id() {
            return Err(VmError::StaleCallable {
                expected: self.vm.program_instance_id(),
                found: invocation.program_instance,
            });
        }
        Ok(())
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

impl Store<()> {
    pub fn from_vm(vm: Vm) -> Self {
        Self::new(vm, ())
    }
}
