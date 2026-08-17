use super::error::{RuntimeError, RuntimeErrorCode, RuntimeResult};
use super::event::{EventEmitter, EventLimits, EventReceipt, EventSink};
use crate::vm::{Value, VmResult};

pub const RUNTIME_INPUT_NAME: &str = "runtime::input";
#[allow(dead_code)]
pub const RUNTIME_EMIT_NAME: &str = "runtime::emit";

#[allow(dead_code)]
pub type RuntimeEventSink = dyn EventSink;

/// Configuration for one VM/run-scoped generic runtime context.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RuntimeContextConfig {
    event_limits: EventLimits,
}

impl RuntimeContextConfig {
    pub const fn new(event_limits: EventLimits) -> Self {
        Self { event_limits }
    }

    pub const fn event_limits(self) -> EventLimits {
        self.event_limits
    }
}

impl Default for RuntimeContextConfig {
    fn default() -> Self {
        Self::new(EventLimits::default())
    }
}

/// Run-scoped input and generic event transport hooks.
///
/// The context stores values as VM [`Value`]s and delegates event persistence/delivery to the
/// embedding. It has no knowledge of sessions, providers, platforms, or event names.
pub struct RuntimeContext {
    input: Option<Value>,
    events: EventEmitter,
}

#[allow(dead_code)]
impl RuntimeContext {
    pub fn with_config(config: RuntimeContextConfig) -> RuntimeResult<Self> {
        Ok(Self {
            input: None,
            events: EventEmitter::new(config.event_limits()),
        })
    }

    pub fn config(&self) -> RuntimeContextConfig {
        RuntimeContextConfig::new(self.events.limits())
    }

    pub fn set_input(&mut self, value: Value) -> RuntimeResult<()> {
        self.input = Some(value);
        Ok(())
    }

    pub fn clear_input(&mut self) {
        self.input = None;
    }

    pub fn reset_for_reuse(&mut self) {
        self.input = None;
        self.events.reset_for_reuse();
    }

    pub fn input(&self) -> RuntimeResult<Value> {
        self.input.clone().ok_or_else(|| {
            RuntimeError::new(
                RuntimeErrorCode::InputUnavailable,
                RUNTIME_INPUT_NAME,
                "run input has not been configured",
            )
        })
    }

    pub fn set_event_sink<S>(&mut self, sink: S) -> RuntimeResult<()>
    where
        S: EventSink + 'static,
    {
        self.events.set_sink(sink);
        Ok(())
    }

    pub fn clear_event_sink(&mut self) {
        self.events.clear_sink();
    }

    pub fn emit(&mut self, value: Value) -> RuntimeResult<EventReceipt> {
        self.events.emit(value)
    }

    pub fn emitted_events(&self) -> u64 {
        self.events.emitted_events()
    }

    pub fn event_limits(&self) -> EventLimits {
        self.events.limits()
    }
}

impl Default for RuntimeContext {
    fn default() -> Self {
        Self::with_config(RuntimeContextConfig::default())
            .expect("default runtime context configuration should be valid")
    }
}

/// Parent registration helper for the zero-argument `runtime::input()` host function.
pub fn runtime_input(context: &RuntimeContext) -> VmResult<Value> {
    context
        .input()
        .map_err(|error| crate::vm::VmError::HostError(error.to_string()))
}

/// Parent registration helper for the one-argument `runtime::emit(value)` host function.
pub fn runtime_emit(context: &mut RuntimeContext, value: Value) -> VmResult<()> {
    context
        .emit(value)
        .map(|_| ())
        .map_err(|error| crate::vm::VmError::HostError(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::{RUNTIME_EMIT_NAME, RUNTIME_INPUT_NAME, RuntimeContext};

    #[test]
    fn host_names_are_generic_and_stable() {
        assert_eq!(RUNTIME_INPUT_NAME, "runtime::input");
        assert_eq!(RUNTIME_EMIT_NAME, "runtime::emit");
        assert!(std::mem::size_of::<RuntimeContext>() > 0);
    }
}
