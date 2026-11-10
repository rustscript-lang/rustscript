//! Typed guest `Option<T>`, separate from the low-level nullable Rust
//! `Option` codec and from the VM's internal Null value.
//!
//! A guest Option is a physical map with a Boolean `tag` (`true` = Some,
//! `false` = None) and the active `payload`. None carries no payload; Some
//! carries exactly one payload. The payload may be physically Null for the
//! logical Unit value, in which case a canonicalizing writer is allowed to
//! omit the entry; the Boolean tag still separates `Some(())` from `None`.

/// A guest-language `Option<T>`.
///
/// `VmResult<GuestOption<T>>` keeps fatal VM errors in the outer result.
/// Only the selected variant is encoded; payload truthiness never selects it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum GuestOption<T> {
    Some(T),
    None,
}

impl<T> From<Option<T>> for GuestOption<T> {
    fn from(value: Option<T>) -> Self {
        match value {
            Some(value) => Self::Some(value),
            None => Self::None,
        }
    }
}

impl<T> From<GuestOption<T>> for Option<T> {
    fn from(value: GuestOption<T>) -> Self {
        match value {
            GuestOption::Some(value) => Some(value),
            GuestOption::None => None,
        }
    }
}

#[cfg(feature = "runtime")]
#[doc(hidden)]
pub fn payload(value: &crate::Value) -> crate::VmResult<(bool, Option<&crate::Value>)> {
    use crate::{Value, VmError};
    let Value::Map(entries) = value else {
        return Err(VmError::TypeMismatch("Option"));
    };
    if entries.len() > 2 {
        return Err(VmError::TypeMismatch("canonical Option fields"));
    }
    for (key, _) in entries.iter() {
        let Value::String(name) = key else {
            return Err(VmError::TypeMismatch("canonical Option fields"));
        };
        if name.as_str() != "tag" && name.as_str() != "payload" {
            return Err(VmError::TypeMismatch("canonical Option fields"));
        }
    }
    let Some(Value::Bool(some)) = entries.get(&Value::string("tag")) else {
        return Err(VmError::TypeMismatch("Option Boolean tag"));
    };
    let payload = entries.get(&Value::string("payload"));
    if !*some && payload.is_some() {
        return Err(VmError::TypeMismatch("Option None payload"));
    }
    Ok((*some, payload))
}

/// Carrier for `None`: a tag only, with no active payload entry.
#[cfg(feature = "runtime")]
#[doc(hidden)]
pub fn none_carrier() -> crate::Value {
    crate::Value::map(vec![(
        crate::Value::string("tag"),
        crate::Value::Bool(false),
    )])
}

/// Carrier for `Some(payload)`.
#[cfg(feature = "runtime")]
#[doc(hidden)]
pub fn some_carrier(payload: crate::Value) -> crate::Value {
    crate::Value::map(vec![
        (crate::Value::string("tag"), crate::Value::Bool(true)),
        (crate::Value::string("payload"), payload),
    ])
}
