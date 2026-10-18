//! Typed guest success/failure values, separate from the VM transport channel.

/// A guest-language `Result<T, E>`.
///
/// `VmResult<GuestResult<T, E>>` keeps fatal VM errors in the outer result.
/// Only the selected variant is encoded; payload truthiness never selects it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum GuestResult<T, E> {
    Ok(T),
    Err(E),
}

impl<T, E> From<Result<T, E>> for GuestResult<T, E> {
    fn from(value: Result<T, E>) -> Self {
        match value {
            Ok(value) => Self::Ok(value),
            Err(error) => Self::Err(error),
        }
    }
}

impl<T, E> From<GuestResult<T, E>> for Result<T, E> {
    fn from(value: GuestResult<T, E>) -> Self {
        match value {
            GuestResult::Ok(value) => Ok(value),
            GuestResult::Err(error) => Err(error),
        }
    }
}

#[cfg(feature = "runtime")]
#[doc(hidden)]
pub fn payload(value: &crate::Value) -> crate::VmResult<(bool, &crate::Value)> {
    use crate::{Value, VmError};
    let Value::Map(entries) = value else {
        return Err(VmError::TypeMismatch("Result"));
    };
    if entries.iter().any(|(key, _)| !matches!(key, Value::String(key) if key.as_str() == "tag" || key.as_str() == "payload")) {
        return Err(VmError::TypeMismatch("canonical Result fields"));
    }
    let Some(Value::Bool(tag)) = entries.get(&Value::string("tag")) else {
        return Err(VmError::TypeMismatch("Result Boolean tag"));
    };
    Ok((
        *tag,
        entries
            .get(&Value::string("payload"))
            .unwrap_or(&Value::Null),
    ))
}

#[cfg(feature = "runtime")]
#[doc(hidden)]
pub fn resource_handle(value: &crate::Value) -> crate::VmResult<crate::ResourceHandle> {
    let crate::Value::Int(raw) = value else {
        return Err(crate::VmError::TypeMismatch("resource handle"));
    };
    let raw = u64::try_from(*raw)
        .map_err(|_| crate::VmError::TypeMismatch("positive resource handle"))?;
    crate::ResourceHandle::from_raw(raw)
        .map_err(|error| crate::VmError::HostError(error.to_string()))
}

/// Context-aware publication used by generated owned Result returns.
/// Validation or assembly failure closes the unpublished resource through the
/// existing execution-scope lifecycle, preserving the primary transport error.
#[cfg(feature = "runtime")]
#[doc(hidden)]
pub fn publish_owned<T: crate::HostResourceType, F>(
    vm: &mut crate::Vm,
    value: crate::ResourceOwned<T>,
    schema: &crate::HostTypeSchema,
    assemble: F,
) -> crate::VmResult<crate::Value>
where
    F: FnOnce(crate::Value) -> crate::VmResult<crate::Value>,
{
    schema
        .validate()
        .map_err(|error| crate::VmError::HostError(error.to_string()))?;
    let key = crate::ResourceTypeKey::new(T::KEY)
        .map_err(|error| crate::VmError::HostError(error.to_string()))?;
    vm.host_context()
        .validate_resource_type_key::<T>(&key)
        .map_err(|error| crate::VmError::HostError(error.to_string()))?;
    let token = vm
        .host_context()
        .insert_resource(value.into_inner())
        .map_err(|error| crate::VmError::HostError(error.to_string()))?;
    let handle = token.handle();
    let result = assemble(crate::Value::Int(handle.raw() as i64)).and_then(|value| {
        vm.validate_guest_result_value(&value, schema)?;
        Ok(value)
    });
    if let Err(primary) = &result {
        vm.host_context()
            .close_resource::<T>(handle, crate::ResourceCloseReason::Requested)
            .map_err(|error| {
                crate::VmError::HostError(format!(
                    "Result publication rollback failed: {error}; primary: {primary}"
                ))
            })?;
    }
    result
}

#[cfg(feature = "runtime")]
#[doc(hidden)]
pub fn carrier(tag: bool, payload: crate::Value) -> crate::Value {
    crate::Value::map(vec![
        (crate::Value::string("tag"), crate::Value::Bool(tag)),
        (crate::Value::string("payload"), payload),
    ])
}

/// Fallible live-VM codec for typed callback payloads, including owned resources.
/// The ordinary context-free callback conversion traits remain independent.
#[cfg(feature = "runtime")]
pub trait VmScriptValue: Sized {
    fn schema() -> crate::VmResult<crate::HostTypeSchema>;
    fn into_value(self, context: &mut ScriptValueContext<'_>) -> crate::VmResult<crate::Value>;
    fn from_value(vm: &mut crate::Vm, value: crate::Value) -> crate::VmResult<Self>;
}

/// Typed callback arguments converted only after Store/callable admission.
#[cfg(feature = "runtime")]
pub trait VmScriptArgs: Sized {
    fn schemas() -> crate::VmResult<Vec<crate::HostTypeSchema>>;
    fn into_values(
        self,
        context: &mut ScriptValueContext<'_>,
    ) -> crate::VmResult<Vec<crate::Value>>;
}

/// A callback's resource publications. The invocation retains its typed journal
/// through execution so an abort can reclaim only abandoned publications.
#[cfg(feature = "runtime")]
pub struct ScriptValueContext<'a> {
    pub(crate) vm: &'a mut crate::Vm,
    pub(crate) pending: Vec<PendingScriptResource>,
}

#[cfg(feature = "runtime")]
pub(crate) struct PendingScriptResource {
    pub(crate) handle: crate::ResourceHandle,
    pub(crate) key: &'static str,
    pub(crate) close: fn(&mut crate::Vm, crate::ResourceHandle) -> crate::VmResult<()>,
}
