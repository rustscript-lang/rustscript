use super::BuiltinCallOutcome;
pub(super) use crate::bytecode::{SharedArray, SharedBytes, SharedMap, VmMap};
use crate::vm::{CallOutcome, CallReturn, HostOpId, Value, VmError, VmResult};

pub(super) type AnyValue = Value;
pub(super) type UnknownValue = Value;
#[allow(dead_code)]
pub(super) type VmValueOwned = Value;
pub(super) type VmArray = Vec<Value>;
pub(super) type VmBytes = Vec<u8>;
pub(super) type VmValueRef<'a> = &'a Value;
pub(super) type VmStringRef<'a> = &'a str;
pub(super) type VmArrayRef<'a> = &'a [Value];
pub(super) type VmBytesRef<'a> = &'a [u8];
pub(super) type VmMapRef<'a> = &'a VmMap;
#[allow(dead_code)]
pub(super) type VmArrayHandle = SharedArray;
#[allow(dead_code)]
pub(super) type VmBytesHandle = SharedBytes;
#[allow(dead_code)]
pub(super) type VmMapHandle = SharedMap;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum NumberValue {
    Int(i64),
    Float(f64),
}

impl NumberValue {
    pub(super) fn as_f64(self) -> f64 {
        match self {
            Self::Int(value) => value as f64,
            Self::Float(value) => value,
        }
    }
}

pub(super) fn missing_arg(label: &str) -> VmError {
    VmError::HostError(format!("missing argument: {label}"))
}

pub(super) trait BorrowVmValue<'a>: Sized {
    fn borrow_vm_value(value: &'a Value, label: &str) -> VmResult<Self>;

    fn from_missing_arg(label: &str) -> VmResult<Self> {
        Err(missing_arg(label))
    }
}

pub(super) trait FromVmValue<'a>: Sized {
    fn from_vm_value(value: &'a Value, label: &str) -> VmResult<Self>;

    fn from_missing_arg(label: &str) -> VmResult<Self> {
        Err(missing_arg(label))
    }
}

impl<'a, T> BorrowVmValue<'a> for T
where
    T: FromVmValue<'a>,
{
    fn borrow_vm_value(value: &'a Value, label: &str) -> VmResult<Self> {
        T::from_vm_value(value, label)
    }

    fn from_missing_arg(label: &str) -> VmResult<Self> {
        T::from_missing_arg(label)
    }
}

pub(super) trait TakeVmValue: Sized {
    fn take_vm_value(slot: &mut Value, label: &str) -> VmResult<Self>;

    fn from_missing_arg(label: &str) -> VmResult<Self> {
        Err(missing_arg(label))
    }
}

pub(super) fn borrow_arg<'a, T>(args: &'a [Value], index: usize, label: &str) -> VmResult<T>
where
    T: BorrowVmValue<'a>,
{
    match args.get(index) {
        Some(value) => T::borrow_vm_value(value, label),
        None => T::from_missing_arg(label),
    }
}

pub(super) fn arg<'a, T>(args: &'a [Value], index: usize, label: &str) -> VmResult<T>
where
    T: BorrowVmValue<'a>,
{
    borrow_arg(args, index, label)
}

pub(super) fn take_arg<T>(args: &mut [Value], index: usize, label: &str) -> VmResult<T>
where
    T: TakeVmValue,
{
    match args.get_mut(index) {
        Some(slot) => T::take_vm_value(slot, label),
        None => T::from_missing_arg(label),
    }
}

impl<'a> FromVmValue<'a> for &'a Value {
    fn from_vm_value(value: &'a Value, _label: &str) -> VmResult<Self> {
        Ok(value)
    }
}

impl FromVmValue<'_> for Value {
    fn from_vm_value(value: &Value, _label: &str) -> VmResult<Self> {
        Ok(value.clone())
    }
}

impl<'a> FromVmValue<'a> for &'a str {
    fn from_vm_value(value: &'a Value, _label: &str) -> VmResult<Self> {
        match value {
            Value::String(text) => Ok(text.as_str()),
            _ => Err(VmError::TypeMismatch("string")),
        }
    }
}

impl<'a> FromVmValue<'a> for &'a [u8] {
    fn from_vm_value(value: &'a Value, _label: &str) -> VmResult<Self> {
        match value {
            Value::Bytes(values) => Ok(values.as_slice()),
            _ => Err(VmError::TypeMismatch("bytes")),
        }
    }
}

impl<'a> FromVmValue<'a> for &'a [Value] {
    fn from_vm_value(value: &'a Value, _label: &str) -> VmResult<Self> {
        match value {
            Value::Array(values) => Ok(values.as_slice()),
            _ => Err(VmError::TypeMismatch("array")),
        }
    }
}

impl<'a> FromVmValue<'a> for &'a VmMap {
    fn from_vm_value(value: &'a Value, _label: &str) -> VmResult<Self> {
        match value {
            Value::Map(entries) => Ok(entries.as_ref()),
            _ => Err(VmError::TypeMismatch("map")),
        }
    }
}

impl FromVmValue<'_> for SharedArray {
    fn from_vm_value(value: &Value, _label: &str) -> VmResult<Self> {
        match value {
            Value::Array(values) => Ok(values.clone()),
            _ => Err(VmError::TypeMismatch("array")),
        }
    }
}

impl FromVmValue<'_> for SharedBytes {
    fn from_vm_value(value: &Value, _label: &str) -> VmResult<Self> {
        match value {
            Value::Bytes(values) => Ok(values.clone()),
            _ => Err(VmError::TypeMismatch("bytes")),
        }
    }
}

impl FromVmValue<'_> for SharedMap {
    fn from_vm_value(value: &Value, _label: &str) -> VmResult<Self> {
        match value {
            Value::Map(entries) => Ok(entries.clone()),
            _ => Err(VmError::TypeMismatch("map")),
        }
    }
}

impl FromVmValue<'_> for bool {
    fn from_vm_value(value: &Value, _label: &str) -> VmResult<Self> {
        match value {
            Value::Bool(flag) => Ok(*flag),
            _ => Err(VmError::TypeMismatch("bool")),
        }
    }
}

impl FromVmValue<'_> for i64 {
    fn from_vm_value(value: &Value, _label: &str) -> VmResult<Self> {
        match value {
            Value::Int(number) => Ok(*number),
            _ => Err(VmError::TypeMismatch("int")),
        }
    }
}

impl FromVmValue<'_> for u32 {
    fn from_vm_value(value: &Value, label: &str) -> VmResult<Self> {
        let raw = i64::from_vm_value(value, label)?;
        if raw < 0 {
            return Err(VmError::HostError(format!("{label} must be non-negative")));
        }
        u32::try_from(raw).map_err(|_| VmError::HostError(format!("{label} overflow")))
    }
}

impl FromVmValue<'_> for usize {
    fn from_vm_value(value: &Value, label: &str) -> VmResult<Self> {
        let raw = i64::from_vm_value(value, label)?;
        if raw < 0 {
            return Err(VmError::HostError(format!("{label} must be non-negative")));
        }
        usize::try_from(raw).map_err(|_| VmError::HostError(format!("{label} overflow")))
    }
}

impl FromVmValue<'_> for f64 {
    fn from_vm_value(value: &Value, _label: &str) -> VmResult<Self> {
        match value {
            Value::Float(number) => Ok(*number),
            _ => Err(VmError::TypeMismatch("float")),
        }
    }
}

impl FromVmValue<'_> for NumberValue {
    fn from_vm_value(value: &Value, _label: &str) -> VmResult<Self> {
        match value {
            Value::Int(number) => Ok(Self::Int(*number)),
            Value::Float(number) => Ok(Self::Float(*number)),
            _ => Err(VmError::TypeMismatch("number")),
        }
    }
}

impl<'a, T> FromVmValue<'a> for Option<T>
where
    T: FromVmValue<'a>,
{
    fn from_vm_value(value: &'a Value, label: &str) -> VmResult<Self> {
        if matches!(value, Value::Null) {
            Ok(None)
        } else {
            T::from_vm_value(value, label).map(Some)
        }
    }

    fn from_missing_arg(_label: &str) -> VmResult<Self> {
        Ok(None)
    }
}

impl TakeVmValue for Value {
    fn take_vm_value(slot: &mut Value, _label: &str) -> VmResult<Self> {
        Ok(std::mem::replace(slot, Value::Null))
    }
}

impl TakeVmValue for SharedArray {
    fn take_vm_value(slot: &mut Value, _label: &str) -> VmResult<Self> {
        match std::mem::replace(slot, Value::Null) {
            Value::Array(values) => Ok(values),
            _ => Err(VmError::TypeMismatch("array")),
        }
    }
}

impl TakeVmValue for SharedBytes {
    fn take_vm_value(slot: &mut Value, _label: &str) -> VmResult<Self> {
        match std::mem::replace(slot, Value::Null) {
            Value::Bytes(values) => Ok(values),
            _ => Err(VmError::TypeMismatch("bytes")),
        }
    }
}

impl TakeVmValue for SharedMap {
    fn take_vm_value(slot: &mut Value, _label: &str) -> VmResult<Self> {
        match std::mem::replace(slot, Value::Null) {
            Value::Map(entries) => Ok(entries),
            _ => Err(VmError::TypeMismatch("map")),
        }
    }
}

impl<T> TakeVmValue for Option<T>
where
    T: TakeVmValue,
{
    fn take_vm_value(slot: &mut Value, label: &str) -> VmResult<Self> {
        if matches!(slot, Value::Null) {
            Ok(None)
        } else {
            T::take_vm_value(slot, label).map(Some)
        }
    }

    fn from_missing_arg(_label: &str) -> VmResult<Self> {
        Ok(None)
    }
}

pub(super) trait IntoVmValue {
    fn into_vm_value(self) -> Value;
}

pub(super) fn return_none() -> CallReturn {
    CallReturn::none()
}

pub(super) fn return_one<T>(value: T) -> CallReturn
where
    T: IntoVmValue,
{
    CallReturn::one(value.into_vm_value())
}

impl IntoVmValue for Value {
    fn into_vm_value(self) -> Value {
        self
    }
}

impl IntoVmValue for bool {
    fn into_vm_value(self) -> Value {
        Value::Bool(self)
    }
}

impl IntoVmValue for i64 {
    fn into_vm_value(self) -> Value {
        Value::Int(self)
    }
}

impl IntoVmValue for u32 {
    fn into_vm_value(self) -> Value {
        Value::Int(i64::from(self))
    }
}

impl IntoVmValue for usize {
    fn into_vm_value(self) -> Value {
        Value::Int(i64::try_from(self).unwrap_or(i64::MAX))
    }
}

impl IntoVmValue for f64 {
    fn into_vm_value(self) -> Value {
        Value::Float(self)
    }
}

impl IntoVmValue for String {
    fn into_vm_value(self) -> Value {
        Value::string(self)
    }
}

impl IntoVmValue for &str {
    fn into_vm_value(self) -> Value {
        Value::string(self)
    }
}

impl IntoVmValue for () {
    fn into_vm_value(self) -> Value {
        Value::Null
    }
}

impl<T> IntoVmValue for Option<T>
where
    T: IntoVmValue,
{
    fn into_vm_value(self) -> Value {
        match self {
            Some(value) => value.into_vm_value(),
            None => Value::Null,
        }
    }
}

impl IntoVmValue for Vec<Value> {
    fn into_vm_value(self) -> Value {
        Value::array(self)
    }
}

impl IntoVmValue for SharedArray {
    fn into_vm_value(self) -> Value {
        Value::Array(self)
    }
}

impl IntoVmValue for VmBytes {
    fn into_vm_value(self) -> Value {
        Value::bytes(self)
    }
}

impl IntoVmValue for SharedBytes {
    fn into_vm_value(self) -> Value {
        Value::Bytes(self)
    }
}

impl IntoVmValue for Vec<(Value, Value)> {
    fn into_vm_value(self) -> Value {
        Value::map(self)
    }
}

impl IntoVmValue for VmMap {
    fn into_vm_value(self) -> Value {
        Value::Map(self.into())
    }
}

impl IntoVmValue for SharedMap {
    fn into_vm_value(self) -> Value {
        Value::Map(self)
    }
}

impl IntoVmValue for NumberValue {
    fn into_vm_value(self) -> Value {
        match self {
            NumberValue::Int(value) => Value::Int(value),
            NumberValue::Float(value) => Value::Float(value),
        }
    }
}

pub(super) trait IntoBuiltinCallOutcome {
    fn into_builtin_call_outcome(self) -> BuiltinCallOutcome;
}

impl<T> IntoBuiltinCallOutcome for T
where
    T: IntoVmValue,
{
    fn into_builtin_call_outcome(self) -> BuiltinCallOutcome {
        BuiltinCallOutcome::Return(return_one(self))
    }
}

#[allow(dead_code)]
pub(super) enum BuiltinResult<T> {
    Return(T),
    Pending(HostOpId),
}

impl<T> IntoBuiltinCallOutcome for BuiltinResult<T>
where
    T: IntoVmValue,
{
    fn into_builtin_call_outcome(self) -> BuiltinCallOutcome {
        match self {
            Self::Return(value) => value.into_builtin_call_outcome(),
            Self::Pending(op_id) => BuiltinCallOutcome::Pending(op_id),
        }
    }
}

pub(super) trait IntoHostCallOutcome {
    fn into_host_call_outcome(self) -> CallOutcome;
}

impl IntoHostCallOutcome for CallOutcome {
    fn into_host_call_outcome(self) -> CallOutcome {
        self
    }
}

impl<T> IntoHostCallOutcome for T
where
    T: IntoVmValue,
{
    fn into_host_call_outcome(self) -> CallOutcome {
        CallOutcome::Return(return_one(self))
    }
}

#[cfg(test)]
mod tests {
    use super::{Value, arg};

    #[test]
    fn optional_arg_decodes_missing_as_none() {
        let args = [];
        let value =
            arg::<Option<&str>>(&args, 0, "label").expect("missing optional arg should decode");
        assert_eq!(value, None);
    }

    #[test]
    fn optional_arg_decodes_null_as_none() {
        let args = [Value::Null];
        let value =
            arg::<Option<&str>>(&args, 0, "label").expect("null optional arg should decode");
        assert_eq!(value, None);
    }

    #[test]
    fn optional_arg_decodes_present_value_as_some() {
        let args = [Value::string("hello")];
        let value =
            arg::<Option<&str>>(&args, 0, "label").expect("present optional arg should decode");
        assert_eq!(value, Some("hello"));
    }
}
