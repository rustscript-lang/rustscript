use std::collections::HashMap;
use std::future::Future;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::pin::Pin;
use std::process::{Child, Command, Stdio};
use std::task::{Context, Poll};

use futures_channel::oneshot;
use regex::Regex;

use crate::builtins::BuiltinFunction;

use super::{HostOpId, Value, Vm, VmError, VmResult};

pub(super) struct IoState {
    pub(super) next_handle: i64,
    pub(super) handles: HashMap<i64, IoHandle>,
    pending_ops: HashMap<HostOpId, oneshot::Receiver<IoAsyncCompletion>>,
}

impl Default for IoState {
    fn default() -> Self {
        Self {
            next_handle: 1,
            handles: HashMap::new(),
            pending_ops: HashMap::new(),
        }
    }
}

pub(super) enum IoHandle {
    File(std::fs::File),
    PopenRead { child: Child },
    PopenWrite { child: Child },
}

pub(super) enum BuiltinCallOutcome {
    Return(Vec<Value>),
    Pending(HostOpId),
}

struct IoAsyncCompletion {
    restored_handle: Option<(i64, IoHandle)>,
    result: VmResult<Vec<Value>>,
}

pub(super) fn execute_builtin_call(
    vm: &mut Vm,
    builtin: BuiltinFunction,
    args: Vec<Value>,
) -> VmResult<BuiltinCallOutcome> {
    match builtin {
        BuiltinFunction::Len => builtin_len(&args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::Slice => builtin_slice(args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::Concat => builtin_concat(args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::ArrayNew => Ok(BuiltinCallOutcome::Return(vec![Value::Array(Vec::new())])),
        BuiltinFunction::ArrayPush => builtin_array_push(args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::MapNew => Ok(BuiltinCallOutcome::Return(vec![Value::Map(Vec::new())])),
        BuiltinFunction::Get => builtin_get(args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::Set => builtin_set(args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::Keys => builtin_keys(args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::IoOpen => builtin_io_open(vm, args),
        BuiltinFunction::IoPopen => builtin_io_popen(vm, args),
        BuiltinFunction::IoReadAll => builtin_io_read_all(vm, args),
        BuiltinFunction::IoReadLine => builtin_io_read_line(vm, args),
        BuiltinFunction::IoWrite => builtin_io_write(vm, args),
        BuiltinFunction::IoFlush => builtin_io_flush(vm, args),
        BuiltinFunction::IoClose => builtin_io_close(vm, args),
        BuiltinFunction::IoExists => builtin_io_exists(vm, args),
        BuiltinFunction::Count => builtin_count(&args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::ReIsMatch => builtin_re_is_match(&args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::ReFind => builtin_re_find(&args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::ReReplace => builtin_re_replace(&args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::ReSplit => builtin_re_split(&args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::ReCaptures => builtin_re_captures(&args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::ToString => builtin_to_string(&args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::TypeOf => builtin_type_of(&args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::Assert => builtin_assert(&args).map(BuiltinCallOutcome::Return),
    }
}

pub(super) fn poll_builtin_io_op(
    vm: &mut Vm,
    op_id: HostOpId,
    cx: &mut Context<'_>,
) -> Poll<VmResult<Vec<Value>>> {
    let poll_result = {
        let receiver = match vm.io_state.pending_ops.get_mut(&op_id) {
            Some(receiver) => receiver,
            None => {
                return Poll::Ready(Err(VmError::HostError(format!(
                    "unknown builtin io op {op_id}",
                ))));
            }
        };
        Pin::new(receiver).poll(cx)
    };

    match poll_result {
        Poll::Pending => Poll::Pending,
        Poll::Ready(Ok(completion)) => {
            vm.io_state.pending_ops.remove(&op_id);
            if let Some((handle_id, handle)) = completion.restored_handle {
                vm.io_state.handles.insert(handle_id, handle);
            }
            Poll::Ready(completion.result)
        }
        Poll::Ready(Err(_)) => {
            vm.io_state.pending_ops.remove(&op_id);
            Poll::Ready(Err(VmError::HostError(format!(
                "builtin io op {op_id} was cancelled",
            ))))
        }
    }
}

pub(super) fn close_all_handles(vm: &mut Vm) {
    let handles = std::mem::take(&mut vm.io_state.handles);
    for (_, handle) in handles {
        let _ = close_io_handle(handle);
    }
}

fn builtin_len(args: &[Value]) -> VmResult<Vec<Value>> {
    let value = args
        .first()
        .ok_or_else(|| VmError::HostError("missing argument to len".to_string()))?;
    let len = match value {
        Value::String(text) => text.chars().count() as i64,
        Value::Array(values) => values.len() as i64,
        Value::Map(entries) => entries.len() as i64,
        _ => return Err(VmError::TypeMismatch("string/array/map")),
    };
    Ok(vec![Value::Int(len)])
}

fn builtin_slice(args: Vec<Value>) -> VmResult<Vec<Value>> {
    let mut iter = args.into_iter();
    let source = iter
        .next()
        .ok_or_else(|| VmError::HostError("missing source for slice".to_string()))?;
    let start = iter
        .next()
        .ok_or_else(|| VmError::HostError("missing slice start".to_string()))?
        .as_int()?;
    let len = iter
        .next()
        .ok_or_else(|| VmError::HostError("missing slice length".to_string()))?
        .as_int()?;

    if start < 0 || len <= 0 {
        return match source {
            Value::String(_) => Ok(vec![Value::String(String::new())]),
            Value::Array(_) => Ok(vec![Value::Array(Vec::new())]),
            _ => Err(VmError::TypeMismatch("string/array")),
        };
    }

    let start = usize::try_from(start).map_err(|_| {
        VmError::HostError("slice start overflow while converting to usize".to_string())
    })?;
    let len = usize::try_from(len).map_err(|_| {
        VmError::HostError("slice length overflow while converting to usize".to_string())
    })?;
    match source {
        Value::String(text) => {
            let out = text.chars().skip(start).take(len).collect::<String>();
            Ok(vec![Value::String(out)])
        }
        Value::Array(values) => {
            let out = values.into_iter().skip(start).take(len).collect::<Vec<_>>();
            Ok(vec![Value::Array(out)])
        }
        _ => Err(VmError::TypeMismatch("string/array")),
    }
}

fn builtin_concat(args: Vec<Value>) -> VmResult<Vec<Value>> {
    let mut iter = args.into_iter();
    let lhs = iter
        .next()
        .ok_or_else(|| VmError::HostError("missing left argument to concat".to_string()))?;
    let rhs = iter
        .next()
        .ok_or_else(|| VmError::HostError("missing right argument to concat".to_string()))?;
    match (lhs, rhs) {
        (Value::String(lhs), Value::String(rhs)) => {
            let mut out = String::with_capacity(lhs.len() + rhs.len());
            out.push_str(&lhs);
            out.push_str(&rhs);
            Ok(vec![Value::String(out)])
        }
        (Value::Array(lhs), Value::Array(rhs)) => {
            let mut out = lhs;
            out.extend(rhs);
            Ok(vec![Value::Array(out)])
        }
        _ => Err(VmError::TypeMismatch("string/string or array/array")),
    }
}

fn builtin_array_push(args: Vec<Value>) -> VmResult<Vec<Value>> {
    let mut iter = args.into_iter();
    let mut out = match iter
        .next()
        .ok_or_else(|| VmError::HostError("missing array argument".to_string()))?
    {
        Value::Array(values) => values,
        _ => return Err(VmError::TypeMismatch("array")),
    };
    let value = iter
        .next()
        .ok_or_else(|| VmError::HostError("missing value argument".to_string()))?;
    out.push(value);
    Ok(vec![Value::Array(out)])
}

fn builtin_get(args: Vec<Value>) -> VmResult<Vec<Value>> {
    let mut iter = args.into_iter();
    let container = iter
        .next()
        .ok_or_else(|| VmError::HostError("missing container argument".to_string()))?;
    let key = iter
        .next()
        .ok_or_else(|| VmError::HostError("missing key argument".to_string()))?;

    match container {
        Value::Array(values) => {
            let index = key.as_int()?;
            if index < 0 {
                return Err(VmError::HostError(
                    "array index must be non-negative".to_string(),
                ));
            }
            let index = usize::try_from(index)
                .map_err(|_| VmError::HostError("array index overflow".to_string()))?;
            let mut values = values;
            if index >= values.len() {
                return Err(VmError::HostError(format!(
                    "array index {index} out of bounds"
                )));
            }
            let value = values.swap_remove(index);
            Ok(vec![value])
        }
        Value::Map(entries) => {
            for (existing_key, value) in entries {
                if existing_key == key {
                    return Ok(vec![value]);
                }
            }
            Err(VmError::HostError("map key not found".to_string()))
        }
        Value::String(text) => {
            let index = key.as_int()?;
            if index < 0 {
                return Err(VmError::HostError(
                    "string index must be non-negative".to_string(),
                ));
            }
            let index = usize::try_from(index)
                .map_err(|_| VmError::HostError("string index overflow".to_string()))?;
            let value = text
                .chars()
                .nth(index)
                .map(|ch| Value::String(ch.to_string()))
                .ok_or_else(|| VmError::HostError(format!("string index {index} out of bounds")))?;
            Ok(vec![value])
        }
        _ => Err(VmError::TypeMismatch("array/map/string")),
    }
}

fn builtin_type_of(args: &[Value]) -> VmResult<Vec<Value>> {
    let value = args
        .first()
        .ok_or_else(|| VmError::HostError("missing argument to type_of".to_string()))?;
    let ty = match value {
        Value::Null => "null",
        Value::Int(_) => "int",
        Value::Float(_) => "float",
        Value::Bool(_) => "bool",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Map(_) => "map",
    };
    Ok(vec![Value::String(ty.to_string())])
}

fn builtin_to_string(args: &[Value]) -> VmResult<Vec<Value>> {
    let value = args
        .first()
        .ok_or_else(|| VmError::HostError("missing argument to __to_string".to_string()))?;
    let text = match value {
        Value::Int(v) => v.to_string(),
        Value::Float(v) => v.to_string(),
        _ => return Err(VmError::TypeMismatch("number")),
    };
    Ok(vec![Value::String(text)])
}

fn builtin_set(args: Vec<Value>) -> VmResult<Vec<Value>> {
    let mut iter = args.into_iter();
    let container = iter
        .next()
        .ok_or_else(|| VmError::HostError("missing container argument".to_string()))?;
    let key = iter
        .next()
        .ok_or_else(|| VmError::HostError("missing key argument".to_string()))?;
    let value = iter
        .next()
        .ok_or_else(|| VmError::HostError("missing value argument".to_string()))?;

    match container {
        Value::Array(values) => {
            let index = key.as_int()?;
            if index < 0 {
                return Err(VmError::HostError(
                    "array index must be non-negative".to_string(),
                ));
            }
            let index = usize::try_from(index)
                .map_err(|_| VmError::HostError("array index overflow".to_string()))?;
            let mut out = values;
            if index < out.len() {
                out[index] = value;
            } else if index == out.len() {
                out.push(value);
            } else {
                let mut entries = out
                    .into_iter()
                    .enumerate()
                    .map(|(idx, existing)| (Value::Int(idx as i64), existing))
                    .collect::<Vec<_>>();
                entries.push((Value::Int(index as i64), value));
                return Ok(vec![Value::Map(entries)]);
            }
            Ok(vec![Value::Array(out)])
        }
        Value::Map(entries) => {
            let mut out = entries;
            if let Some((_, existing_value)) = out
                .iter_mut()
                .find(|(existing_key, _)| *existing_key == key)
            {
                *existing_value = value;
            } else {
                out.push((key, value));
            }
            Ok(vec![Value::Map(out)])
        }
        _ => Err(VmError::TypeMismatch("array/map")),
    }
}

fn builtin_keys(args: Vec<Value>) -> VmResult<Vec<Value>> {
    let container = args
        .into_iter()
        .next()
        .ok_or_else(|| VmError::HostError("missing container argument".to_string()))?;

    let keys = match container {
        Value::Array(values) => (0..values.len())
            .map(|index| Value::Int(index as i64))
            .collect::<Vec<_>>(),
        Value::Map(entries) => entries.into_iter().map(|(key, _)| key).collect::<Vec<_>>(),
        _ => return Err(VmError::TypeMismatch("array/map")),
    };
    Ok(vec![Value::Array(keys)])
}

fn builtin_count(args: &[Value]) -> VmResult<Vec<Value>> {
    let container = args
        .first()
        .ok_or_else(|| VmError::HostError("missing container argument".to_string()))?;
    let count = match container {
        Value::Array(values) => values.len() as i64,
        Value::Map(entries) => entries.len() as i64,
        _ => return Err(VmError::TypeMismatch("array/map")),
    };
    Ok(vec![Value::Int(count)])
}

fn builtin_io_open(vm: &mut Vm, args: Vec<Value>) -> VmResult<BuiltinCallOutcome> {
    let path = arg_string(&args, 0, "io_open path")?;
    let mode = arg_string(&args, 1, "io_open mode")?;
    let reserved_id = io_reserve_handle_id(vm);
    let path = path.to_string();
    let mode = mode.to_string();
    let op_id = schedule_io_task(vm, move || {
        let mut options = OpenOptions::new();
        match mode.as_str() {
            "r" => {
                options.read(true);
            }
            "w" => {
                options.write(true).create(true).truncate(true);
            }
            "a" => {
                options.write(true).create(true).append(true);
            }
            "r+" => {
                options.read(true).write(true);
            }
            "w+" => {
                options.read(true).write(true).create(true).truncate(true);
            }
            "a+" => {
                options.read(true).write(true).create(true).append(true);
            }
            other => {
                return IoAsyncCompletion {
                    restored_handle: None,
                    result: Err(VmError::HostError(format!(
                        "unsupported io_open mode '{other}', expected r/w/a/r+/w+/a+",
                    ))),
                };
            }
        }

        match options.open(path) {
            Ok(file) => IoAsyncCompletion {
                restored_handle: Some((reserved_id, IoHandle::File(file))),
                result: Ok(vec![Value::Int(reserved_id)]),
            },
            Err(err) => IoAsyncCompletion {
                restored_handle: None,
                result: Err(VmError::HostError(format!("io_open failed: {err}"))),
            },
        }
    })?;
    Ok(BuiltinCallOutcome::Pending(op_id))
}

fn builtin_io_popen(vm: &mut Vm, args: Vec<Value>) -> VmResult<BuiltinCallOutcome> {
    let command = arg_string(&args, 0, "io_popen command")?;
    let mode = arg_string(&args, 1, "io_popen mode")?;
    if mode != "r" && mode != "w" {
        return Err(VmError::HostError(format!(
            "unsupported io_popen mode '{mode}', expected r or w"
        )));
    }
    let reserved_id = io_reserve_handle_id(vm);
    let command = command.to_string();
    let mode = mode.to_string();
    let op_id = schedule_io_task(vm, move || {
        let child = match spawn_shell_command(command.as_str(), mode.as_str()) {
            Ok(child) => child,
            Err(err) => {
                return IoAsyncCompletion {
                    restored_handle: None,
                    result: Err(err),
                };
            }
        };
        let handle = match mode.as_str() {
            "r" => {
                if child.stdout.is_none() {
                    return IoAsyncCompletion {
                        restored_handle: None,
                        result: Err(VmError::HostError(
                            "io_popen('r') did not provide stdout pipe".to_string(),
                        )),
                    };
                }
                IoHandle::PopenRead { child }
            }
            "w" => {
                if child.stdin.is_none() {
                    return IoAsyncCompletion {
                        restored_handle: None,
                        result: Err(VmError::HostError(
                            "io_popen('w') did not provide stdin pipe".to_string(),
                        )),
                    };
                }
                IoHandle::PopenWrite { child }
            }
            _ => unreachable!("mode validated above"),
        };
        IoAsyncCompletion {
            restored_handle: Some((reserved_id, handle)),
            result: Ok(vec![Value::Int(reserved_id)]),
        }
    })?;
    Ok(BuiltinCallOutcome::Pending(op_id))
}

fn builtin_io_read_all(vm: &mut Vm, args: Vec<Value>) -> VmResult<BuiltinCallOutcome> {
    let handle_id = arg_handle_id(&args, 0, "io_read_all handle")?;
    let handle = io_take_handle(vm, handle_id)?;
    let op_id = schedule_io_task(vm, move || {
        let mut handle = handle;
        let mut out = String::new();
        let result = match &mut handle {
            IoHandle::File(file) => file
                .read_to_string(&mut out)
                .map_err(|err| VmError::HostError(format!("io_read_all failed: {err}")))
                .map(|_| vec![Value::String(out)]),
            IoHandle::PopenRead { child } => {
                let stdout = match child.stdout.as_mut() {
                    Some(stdout) => stdout,
                    None => {
                        return IoAsyncCompletion {
                            restored_handle: Some((handle_id, handle)),
                            result: Err(VmError::HostError(
                                "io_read_all popen handle missing stdout".to_string(),
                            )),
                        };
                    }
                };
                stdout
                    .read_to_string(&mut out)
                    .map_err(|err| VmError::HostError(format!("io_read_all failed: {err}")))
                    .map(|_| vec![Value::String(out)])
            }
            IoHandle::PopenWrite { .. } => Err(VmError::HostError(
                "io_read_all requires a readable handle".to_string(),
            )),
        };
        IoAsyncCompletion {
            restored_handle: Some((handle_id, handle)),
            result,
        }
    })?;
    Ok(BuiltinCallOutcome::Pending(op_id))
}

fn builtin_io_read_line(vm: &mut Vm, args: Vec<Value>) -> VmResult<BuiltinCallOutcome> {
    let handle_id = arg_handle_id(&args, 0, "io_read_line handle")?;
    let handle = io_take_handle(vm, handle_id)?;
    let op_id = schedule_io_task(vm, move || {
        let mut handle = handle;
        let result = match &mut handle {
            IoHandle::File(file) => read_line_from_reader(file).map(|line| vec![Value::String(line)]),
            IoHandle::PopenRead { child } => {
                let stdout = match child.stdout.as_mut() {
                    Some(stdout) => stdout,
                    None => {
                        return IoAsyncCompletion {
                            restored_handle: Some((handle_id, handle)),
                            result: Err(VmError::HostError(
                                "io_read_line popen handle missing stdout".to_string(),
                            )),
                        };
                    }
                };
                read_line_from_reader(stdout).map(|line| vec![Value::String(line)])
            }
            IoHandle::PopenWrite { .. } => Err(VmError::HostError(
                "io_read_line requires a readable handle".to_string(),
            )),
        };
        IoAsyncCompletion {
            restored_handle: Some((handle_id, handle)),
            result,
        }
    })?;
    Ok(BuiltinCallOutcome::Pending(op_id))
}

fn builtin_io_write(vm: &mut Vm, args: Vec<Value>) -> VmResult<BuiltinCallOutcome> {
    let handle_id = arg_handle_id(&args, 0, "io_write handle")?;
    let data = arg_string(&args, 1, "io_write data")?;
    let bytes = data.as_bytes().to_vec();
    let handle = io_take_handle(vm, handle_id)?;
    let op_id = schedule_io_task(vm, move || {
        let mut handle = handle;
        let result = match &mut handle {
            IoHandle::File(file) => file
                .write(&bytes)
                .map_err(|err| VmError::HostError(format!("io_write failed: {err}")))
                .map(|written| vec![Value::Int(written as i64)]),
            IoHandle::PopenWrite { child } => {
                let stdin = match child.stdin.as_mut() {
                    Some(stdin) => stdin,
                    None => {
                        return IoAsyncCompletion {
                            restored_handle: Some((handle_id, handle)),
                            result: Err(VmError::HostError(
                                "io_write popen handle missing stdin".to_string(),
                            )),
                        };
                    }
                };
                stdin
                    .write(&bytes)
                    .map_err(|err| VmError::HostError(format!("io_write failed: {err}")))
                    .map(|written| vec![Value::Int(written as i64)])
            }
            IoHandle::PopenRead { .. } => Err(VmError::HostError(
                "io_write requires a writable handle".to_string(),
            )),
        };
        IoAsyncCompletion {
            restored_handle: Some((handle_id, handle)),
            result,
        }
    })?;
    Ok(BuiltinCallOutcome::Pending(op_id))
}

fn builtin_io_flush(vm: &mut Vm, args: Vec<Value>) -> VmResult<BuiltinCallOutcome> {
    let handle_id = arg_handle_id(&args, 0, "io_flush handle")?;
    let handle = io_take_handle(vm, handle_id)?;
    let op_id = schedule_io_task(vm, move || {
        let mut handle = handle;
        let result = match &mut handle {
            IoHandle::File(file) => file
                .flush()
                .map_err(|err| VmError::HostError(format!("io_flush failed: {err}")))
                .map(|_| vec![Value::Bool(true)]),
            IoHandle::PopenWrite { child } => {
                let stdin = match child.stdin.as_mut() {
                    Some(stdin) => stdin,
                    None => {
                        return IoAsyncCompletion {
                            restored_handle: Some((handle_id, handle)),
                            result: Err(VmError::HostError(
                                "io_flush popen handle missing stdin".to_string(),
                            )),
                        };
                    }
                };
                stdin
                    .flush()
                    .map_err(|err| VmError::HostError(format!("io_flush failed: {err}")))
                    .map(|_| vec![Value::Bool(true)])
            }
            IoHandle::PopenRead { .. } => Ok(vec![Value::Bool(true)]),
        };
        IoAsyncCompletion {
            restored_handle: Some((handle_id, handle)),
            result,
        }
    })?;
    Ok(BuiltinCallOutcome::Pending(op_id))
}

fn builtin_io_close(vm: &mut Vm, args: Vec<Value>) -> VmResult<BuiltinCallOutcome> {
    let handle_id = arg_handle_id(&args, 0, "io_close handle")?;
    let handle = io_take_handle(vm, handle_id)?;
    let op_id = schedule_io_task(vm, move || IoAsyncCompletion {
        restored_handle: None,
        result: close_io_handle(handle).map(|_| vec![Value::Bool(true)]),
    })?;
    Ok(BuiltinCallOutcome::Pending(op_id))
}

fn builtin_io_exists(vm: &mut Vm, args: Vec<Value>) -> VmResult<BuiltinCallOutcome> {
    let path = arg_string(&args, 0, "io_exists path")?.to_string();
    let op_id = schedule_io_task(vm, move || IoAsyncCompletion {
        restored_handle: None,
        result: Ok(vec![Value::Bool(std::path::Path::new(path.as_str()).exists())]),
    })?;
    Ok(BuiltinCallOutcome::Pending(op_id))
}

fn builtin_re_is_match(args: &[Value]) -> VmResult<Vec<Value>> {
    let pattern = arg_string(args, 0, "re_is_match pattern")?;
    let text = arg_string(args, 1, "re_is_match text")?;
    let regex = Regex::new(pattern)
        .map_err(|err| VmError::HostError(format!("re_is_match invalid pattern: {err}")))?;
    Ok(vec![Value::Bool(regex.is_match(text))])
}

fn builtin_re_find(args: &[Value]) -> VmResult<Vec<Value>> {
    let pattern = arg_string(args, 0, "re_find pattern")?;
    let text = arg_string(args, 1, "re_find text")?;
    let regex = Regex::new(pattern)
        .map_err(|err| VmError::HostError(format!("re_find invalid pattern: {err}")))?;
    let value = match regex.find(text) {
        Some(matched) => Value::String(matched.as_str().to_string()),
        None => Value::Null,
    };
    Ok(vec![value])
}

fn builtin_re_replace(args: &[Value]) -> VmResult<Vec<Value>> {
    let pattern = arg_string(args, 0, "re_replace pattern")?;
    let text = arg_string(args, 1, "re_replace text")?;
    let replacement = arg_string(args, 2, "re_replace replacement")?;
    let regex = Regex::new(pattern)
        .map_err(|err| VmError::HostError(format!("re_replace invalid pattern: {err}")))?;
    let replaced = regex.replace_all(text, replacement).into_owned();
    Ok(vec![Value::String(replaced)])
}

fn builtin_re_split(args: &[Value]) -> VmResult<Vec<Value>> {
    let pattern = arg_string(args, 0, "re_split pattern")?;
    let text = arg_string(args, 1, "re_split text")?;
    let regex = Regex::new(pattern)
        .map_err(|err| VmError::HostError(format!("re_split invalid pattern: {err}")))?;
    let parts = regex
        .split(text)
        .map(|part| Value::String(part.to_string()))
        .collect::<Vec<_>>();
    Ok(vec![Value::Array(parts)])
}

fn builtin_re_captures(args: &[Value]) -> VmResult<Vec<Value>> {
    let pattern = arg_string(args, 0, "re_captures pattern")?;
    let text = arg_string(args, 1, "re_captures text")?;
    let regex = Regex::new(pattern)
        .map_err(|err| VmError::HostError(format!("re_captures invalid pattern: {err}")))?;
    let Some(captures) = regex.captures(text) else {
        return Ok(vec![Value::Array(Vec::new())]);
    };

    let mut groups = Vec::with_capacity(captures.len());
    for index in 0..captures.len() {
        let group_value = match captures.get(index) {
            Some(group) => Value::String(group.as_str().to_string()),
            None => Value::Null,
        };
        groups.push(group_value);
    }
    Ok(vec![Value::Array(groups)])
}

fn builtin_assert(args: &[Value]) -> VmResult<Vec<Value>> {
    let condition = args
        .first()
        .ok_or_else(|| VmError::HostError("missing argument: assert condition".to_string()))?
        .as_bool()?;
    if condition {
        Ok(Vec::new())
    } else {
        Err(VmError::HostError("assertion failed".to_string()))
    }
}

fn spawn_shell_command(command: &str, mode: &str) -> VmResult<Child> {
    let mut process = if cfg!(windows) {
        let mut cmd = Command::new("cmd");
        cmd.arg("/C").arg(command);
        cmd
    } else {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(command);
        cmd
    };

    match mode {
        "r" => {
            process.stdout(Stdio::piped()).stdin(Stdio::null());
        }
        "w" => {
            process.stdin(Stdio::piped()).stdout(Stdio::null());
        }
        _ => {}
    }

    process
        .spawn()
        .map_err(|err| VmError::HostError(format!("io_popen failed: {err}")))
}

fn io_reserve_handle_id(vm: &mut Vm) -> i64 {
    let id = vm.io_state.next_handle;
    vm.io_state.next_handle = vm.io_state.next_handle.saturating_add(1);
    id
}

fn io_take_handle(vm: &mut Vm, handle_id: i64) -> VmResult<IoHandle> {
    vm.io_state
        .handles
        .remove(&handle_id)
        .ok_or_else(|| VmError::HostError(format!("io handle {handle_id} not found")))
}

fn schedule_io_task(
    vm: &mut Vm,
    task: impl FnOnce() -> IoAsyncCompletion + Send + 'static,
) -> VmResult<HostOpId> {
    let op_id = vm.allocate_host_op_id();
    let (sender, receiver) = oneshot::channel();
    std::thread::Builder::new()
        .name("pd-vm-io".to_string())
        .spawn(move || {
            let completion = task();
            let _ = sender.send(completion);
        })
        .map_err(|err| VmError::HostError(format!("failed to spawn io task: {err}")))?;
    vm.io_state.pending_ops.insert(op_id, receiver);
    Ok(op_id)
}

fn arg_string<'a>(args: &'a [Value], index: usize, label: &str) -> VmResult<&'a str> {
    match args.get(index) {
        Some(Value::String(value)) => Ok(value.as_str()),
        Some(_) => Err(VmError::TypeMismatch("string")),
        None => Err(VmError::HostError(format!("missing argument: {label}"))),
    }
}

fn arg_handle_id(args: &[Value], index: usize, label: &str) -> VmResult<i64> {
    let id = args
        .get(index)
        .ok_or_else(|| VmError::HostError(format!("missing argument: {label}")))?
        .as_int()?;
    if id <= 0 {
        return Err(VmError::HostError(format!(
            "invalid io handle id {id}, expected positive id"
        )));
    }
    Ok(id)
}

fn close_io_handle(mut handle: IoHandle) -> VmResult<()> {
    match &mut handle {
        IoHandle::File(file) => {
            file.flush().ok();
        }
        IoHandle::PopenRead { child } => {
            child
                .wait()
                .map_err(|err| VmError::HostError(format!("io_close popen wait failed: {err}")))?;
        }
        IoHandle::PopenWrite { child } => {
            let _ = child.stdin.take();
            child
                .wait()
                .map_err(|err| VmError::HostError(format!("io_close popen wait failed: {err}")))?;
        }
    }
    Ok(())
}

fn read_line_from_reader(reader: &mut impl Read) -> VmResult<String> {
    let mut bytes = Vec::new();
    let mut one = [0u8; 1];
    loop {
        let read = reader
            .read(&mut one)
            .map_err(|err| VmError::HostError(format!("io_read_line failed: {err}")))?;
        if read == 0 {
            break;
        }
        bytes.push(one[0]);
        if one[0] == b'\n' {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}
