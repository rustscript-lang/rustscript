use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};

use crate::builtins::BuiltinFunction;

use super::{Value, Vm, VmError, VmResult};

pub(super) struct IoState {
    pub(super) next_handle: i64,
    pub(super) handles: HashMap<i64, IoHandle>,
}

impl Default for IoState {
    fn default() -> Self {
        Self {
            next_handle: 1,
            handles: HashMap::new(),
        }
    }
}

pub(super) enum IoHandle {
    File(std::fs::File),
    PopenRead { child: Child },
    PopenWrite { child: Child },
}

pub(super) fn execute_builtin_call(
    vm: &mut Vm,
    builtin: BuiltinFunction,
    args: &[Value],
) -> VmResult<Vec<Value>> {
    match builtin {
        BuiltinFunction::Len => builtin_len(args),
        BuiltinFunction::Slice => builtin_slice(args),
        BuiltinFunction::Concat => builtin_concat(args),
        BuiltinFunction::ArrayNew => Ok(vec![Value::Array(Vec::new())]),
        BuiltinFunction::ArrayPush => builtin_array_push(args),
        BuiltinFunction::MapNew => Ok(vec![Value::Map(Vec::new())]),
        BuiltinFunction::Get => builtin_get(args),
        BuiltinFunction::Set => builtin_set(args),
        BuiltinFunction::IoOpen => builtin_io_open(vm, args),
        BuiltinFunction::IoPopen => builtin_io_popen(vm, args),
        BuiltinFunction::IoReadAll => builtin_io_read_all(vm, args),
        BuiltinFunction::IoReadLine => builtin_io_read_line(vm, args),
        BuiltinFunction::IoWrite => builtin_io_write(vm, args),
        BuiltinFunction::IoFlush => builtin_io_flush(vm, args),
        BuiltinFunction::IoClose => builtin_io_close(vm, args),
        BuiltinFunction::IoExists => builtin_io_exists(args),
        BuiltinFunction::ToString => builtin_to_string(args),
        BuiltinFunction::TypeOf => builtin_type_of(args),
        BuiltinFunction::Assert => builtin_assert(args),
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

fn builtin_slice(args: &[Value]) -> VmResult<Vec<Value>> {
    let source = args
        .first()
        .ok_or_else(|| VmError::HostError("missing source for slice".to_string()))?;
    let start = args
        .get(1)
        .ok_or_else(|| VmError::HostError("missing slice start".to_string()))?
        .as_int()?;
    let len = args
        .get(2)
        .ok_or_else(|| VmError::HostError("missing slice length".to_string()))?
        .as_int()?;

    if start < 0 || len <= 0 {
        return Ok(vec![match source {
            Value::String(_) => Value::String(String::new()),
            Value::Array(_) => Value::Array(Vec::new()),
            _ => return Err(VmError::TypeMismatch("string/array")),
        }]);
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
            let out = values
                .iter()
                .skip(start)
                .take(len)
                .cloned()
                .collect::<Vec<_>>();
            Ok(vec![Value::Array(out)])
        }
        _ => Err(VmError::TypeMismatch("string/array")),
    }
}

fn builtin_concat(args: &[Value]) -> VmResult<Vec<Value>> {
    let lhs = args
        .first()
        .ok_or_else(|| VmError::HostError("missing left argument to concat".to_string()))?;
    let rhs = args
        .get(1)
        .ok_or_else(|| VmError::HostError("missing right argument to concat".to_string()))?;
    match (lhs, rhs) {
        (Value::String(lhs), Value::String(rhs)) => {
            let mut out = String::with_capacity(lhs.len() + rhs.len());
            out.push_str(lhs);
            out.push_str(rhs);
            Ok(vec![Value::String(out)])
        }
        (Value::Array(lhs), Value::Array(rhs)) => {
            let mut out = lhs.clone();
            out.extend(rhs.iter().cloned());
            Ok(vec![Value::Array(out)])
        }
        _ => Err(VmError::TypeMismatch("string/string or array/array")),
    }
}

fn builtin_array_push(args: &[Value]) -> VmResult<Vec<Value>> {
    let values = match args
        .first()
        .ok_or_else(|| VmError::HostError("missing array argument".to_string()))?
    {
        Value::Array(values) => values,
        _ => return Err(VmError::TypeMismatch("array")),
    };
    let mut out = values.clone();
    let value = args
        .get(1)
        .ok_or_else(|| VmError::HostError("missing value argument".to_string()))?
        .clone();
    out.push(value);
    Ok(vec![Value::Array(out)])
}

fn builtin_get(args: &[Value]) -> VmResult<Vec<Value>> {
    let container = args
        .first()
        .ok_or_else(|| VmError::HostError("missing container argument".to_string()))?;
    let key = args
        .get(1)
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
            let value = values
                .get(index)
                .cloned()
                .ok_or_else(|| VmError::HostError(format!("array index {index} out of bounds")))?;
            Ok(vec![value])
        }
        Value::Map(entries) => {
            for (existing_key, value) in entries {
                if existing_key == key {
                    return Ok(vec![value.clone()]);
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

fn builtin_set(args: &[Value]) -> VmResult<Vec<Value>> {
    let container = args
        .first()
        .ok_or_else(|| VmError::HostError("missing container argument".to_string()))?;
    let key = args
        .get(1)
        .ok_or_else(|| VmError::HostError("missing key argument".to_string()))?;
    let value = args
        .get(2)
        .ok_or_else(|| VmError::HostError("missing value argument".to_string()))?
        .clone();

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
            let mut out = values.clone();
            if index < out.len() {
                out[index] = value;
            } else if index == out.len() {
                out.push(value);
            } else {
                return Err(VmError::HostError(format!(
                    "array set index {index} out of bounds"
                )));
            }
            Ok(vec![Value::Array(out)])
        }
        Value::Map(entries) => {
            let mut out = entries.clone();
            if let Some((_, existing_value)) =
                out.iter_mut().find(|(existing_key, _)| existing_key == key)
            {
                *existing_value = value;
            } else {
                out.push((key.clone(), value));
            }
            Ok(vec![Value::Map(out)])
        }
        _ => Err(VmError::TypeMismatch("array/map")),
    }
}

fn builtin_io_open(vm: &mut Vm, args: &[Value]) -> VmResult<Vec<Value>> {
    let path = arg_string(args, 0, "io_open path")?;
    let mode = arg_string(args, 1, "io_open mode")?;

    let mut options = OpenOptions::new();
    match mode {
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
            return Err(VmError::HostError(format!(
                "unsupported io_open mode '{other}', expected r/w/a/r+/w+/a+"
            )));
        }
    }

    let file = options
        .open(path)
        .map_err(|err| VmError::HostError(format!("io_open failed: {err}")))?;
    let id = io_insert_handle(vm, IoHandle::File(file));
    Ok(vec![Value::Int(id)])
}

fn builtin_io_popen(vm: &mut Vm, args: &[Value]) -> VmResult<Vec<Value>> {
    let command = arg_string(args, 0, "io_popen command")?;
    let mode = arg_string(args, 1, "io_popen mode")?;
    if mode != "r" && mode != "w" {
        return Err(VmError::HostError(format!(
            "unsupported io_popen mode '{mode}', expected r or w"
        )));
    }

    let child = spawn_shell_command(command, mode)?;
    let handle = match mode {
        "r" => {
            if child.stdout.is_none() {
                return Err(VmError::HostError(
                    "io_popen('r') did not provide stdout pipe".to_string(),
                ));
            }
            IoHandle::PopenRead { child }
        }
        "w" => {
            if child.stdin.is_none() {
                return Err(VmError::HostError(
                    "io_popen('w') did not provide stdin pipe".to_string(),
                ));
            }
            IoHandle::PopenWrite { child }
        }
        _ => unreachable!("mode validated above"),
    };
    let id = io_insert_handle(vm, handle);
    Ok(vec![Value::Int(id)])
}

fn builtin_io_read_all(vm: &mut Vm, args: &[Value]) -> VmResult<Vec<Value>> {
    let handle_id = arg_handle_id(args, 0, "io_read_all handle")?;
    let mut out = String::new();
    let handle = io_get_handle_mut(vm, handle_id)?;
    match handle {
        IoHandle::File(file) => {
            file.read_to_string(&mut out)
                .map_err(|err| VmError::HostError(format!("io_read_all failed: {err}")))?;
        }
        IoHandle::PopenRead { child } => {
            let stdout = child.stdout.as_mut().ok_or_else(|| {
                VmError::HostError("io_read_all popen handle missing stdout".to_string())
            })?;
            stdout
                .read_to_string(&mut out)
                .map_err(|err| VmError::HostError(format!("io_read_all failed: {err}")))?;
        }
        IoHandle::PopenWrite { .. } => {
            return Err(VmError::HostError(
                "io_read_all requires a readable handle".to_string(),
            ));
        }
    }
    Ok(vec![Value::String(out)])
}

fn builtin_io_read_line(vm: &mut Vm, args: &[Value]) -> VmResult<Vec<Value>> {
    let handle_id = arg_handle_id(args, 0, "io_read_line handle")?;
    let handle = io_get_handle_mut(vm, handle_id)?;
    let line = match handle {
        IoHandle::File(file) => read_line_from_reader(file)?,
        IoHandle::PopenRead { child } => {
            let stdout = child.stdout.as_mut().ok_or_else(|| {
                VmError::HostError("io_read_line popen handle missing stdout".to_string())
            })?;
            read_line_from_reader(stdout)?
        }
        IoHandle::PopenWrite { .. } => {
            return Err(VmError::HostError(
                "io_read_line requires a readable handle".to_string(),
            ));
        }
    };
    Ok(vec![Value::String(line)])
}

fn builtin_io_write(vm: &mut Vm, args: &[Value]) -> VmResult<Vec<Value>> {
    let handle_id = arg_handle_id(args, 0, "io_write handle")?;
    let data = arg_string(args, 1, "io_write data")?;
    let bytes = data.as_bytes();

    let handle = io_get_handle_mut(vm, handle_id)?;
    let written = match handle {
        IoHandle::File(file) => file
            .write(bytes)
            .map_err(|err| VmError::HostError(format!("io_write failed: {err}")))?,
        IoHandle::PopenWrite { child } => {
            let stdin = child.stdin.as_mut().ok_or_else(|| {
                VmError::HostError("io_write popen handle missing stdin".to_string())
            })?;
            stdin
                .write(bytes)
                .map_err(|err| VmError::HostError(format!("io_write failed: {err}")))?
        }
        IoHandle::PopenRead { .. } => {
            return Err(VmError::HostError(
                "io_write requires a writable handle".to_string(),
            ));
        }
    };

    Ok(vec![Value::Int(written as i64)])
}

fn builtin_io_flush(vm: &mut Vm, args: &[Value]) -> VmResult<Vec<Value>> {
    let handle_id = arg_handle_id(args, 0, "io_flush handle")?;
    let handle = io_get_handle_mut(vm, handle_id)?;
    match handle {
        IoHandle::File(file) => file
            .flush()
            .map_err(|err| VmError::HostError(format!("io_flush failed: {err}")))?,
        IoHandle::PopenWrite { child } => {
            let stdin = child.stdin.as_mut().ok_or_else(|| {
                VmError::HostError("io_flush popen handle missing stdin".to_string())
            })?;
            stdin
                .flush()
                .map_err(|err| VmError::HostError(format!("io_flush failed: {err}")))?;
        }
        IoHandle::PopenRead { .. } => {}
    }
    Ok(vec![Value::Bool(true)])
}

fn builtin_io_close(vm: &mut Vm, args: &[Value]) -> VmResult<Vec<Value>> {
    let handle_id = arg_handle_id(args, 0, "io_close handle")?;
    let handle = vm
        .io_state
        .handles
        .remove(&handle_id)
        .ok_or_else(|| VmError::HostError(format!("io handle {handle_id} not found")))?;
    close_io_handle(handle)?;
    Ok(vec![Value::Bool(true)])
}

fn builtin_io_exists(args: &[Value]) -> VmResult<Vec<Value>> {
    let path = arg_string(args, 0, "io_exists path")?;
    Ok(vec![Value::Bool(std::path::Path::new(path).exists())])
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

fn io_insert_handle(vm: &mut Vm, handle: IoHandle) -> i64 {
    let id = vm.io_state.next_handle;
    vm.io_state.next_handle = vm.io_state.next_handle.saturating_add(1);
    vm.io_state.handles.insert(id, handle);
    id
}

fn io_get_handle_mut(vm: &mut Vm, handle_id: i64) -> VmResult<&mut IoHandle> {
    vm.io_state
        .handles
        .get_mut(&handle_id)
        .ok_or_else(|| VmError::HostError(format!("io handle {handle_id} not found")))
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
