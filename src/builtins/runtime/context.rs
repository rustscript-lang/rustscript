//! Run-scoped invocation stream configuration.
//!
//! The [`RuntimeContext`] carries only the per-item event bound applied by
//! `stream::emit`. Event values are owned by the active invocation's single
//! pending-event slot; there is no ambient input, no embedding event sink, and
//! no sequence or persistence policy here.

use super::error::RuntimeResult;
use super::event::EventLimits;

/// The authoritative `stream::emit` builtin identity.
#[allow(dead_code)]
pub const STREAM_EMIT_NAME: &str = "stream::emit";

/// Configuration for one VM/run-scoped invocation stream.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RuntimeContextConfig {
    event_limits: EventLimits,
}

#[allow(dead_code)]
impl RuntimeContextConfig {
    pub fn new(event_limits: EventLimits) -> Self {
        Self { event_limits }
    }

    #[allow(dead_code)]
    pub const fn event_limits(self) -> EventLimits {
        self.event_limits
    }
}

/// Run-scoped invocation stream configuration.
#[derive(Debug, Default)]
pub struct RuntimeContext {
    event_limits: EventLimits,
}

#[allow(dead_code)]
impl RuntimeContext {
    pub fn with_config(config: RuntimeContextConfig) -> RuntimeResult<Self> {
        Ok(Self {
            event_limits: config.event_limits,
        })
    }

    pub fn config(&self) -> RuntimeContextConfig {
        RuntimeContextConfig::new(self.event_limits)
    }

    pub fn event_limits(&self) -> EventLimits {
        self.event_limits
    }
}

#[cfg(test)]
mod tests {
    use super::{EventLimits, RuntimeContext, RuntimeContextConfig, STREAM_EMIT_NAME};

    #[test]
    fn host_name_is_generic_and_stable() {
        assert_eq!(STREAM_EMIT_NAME, "stream::emit");
        assert!(std::mem::size_of::<RuntimeContext>() > 0);
    }

    #[test]
    fn per_item_event_limits_are_configurable() {
        let limits = EventLimits::new(128, 4).expect("limits should be valid");
        let context = RuntimeContext::with_config(RuntimeContextConfig::new(limits))
            .expect("context should be constructible");
        assert_eq!(context.event_limits(), limits);
        assert_eq!(context.config().event_limits(), limits);
    }
}
