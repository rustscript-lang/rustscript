use std::collections::HashMap;
use std::fs::OpenOptions;
use std::future::Future;
use std::io::{Read, Write};
use std::pin::Pin;
use std::process::{Child, Command, Stdio};
use std::task::{Context, Poll};

use crate::builtins::{
    BuiltinFunction, BuiltinNamespace, BuiltinNamespaceMember, BuiltinNamespaceRegistry,
};
use futures_channel::oneshot;

use super::super::{HostOpId, Value, Vm, VmError, VmResult};
use super::{BuiltinCallOutcome, arg_string};

pub(in crate::vm) struct IoState {
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

struct IoAsyncCompletion {
    restored_handle: Option<(i64, IoHandle)>,
    result: VmResult<Vec<Value>>,
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

pub(super) fn builtin_io_open(vm: &mut Vm, args: Vec<Value>) -> VmResult<BuiltinCallOutcome> {
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

pub(super) fn builtin_io_popen(vm: &mut Vm, args: Vec<Value>) -> VmResult<BuiltinCallOutcome> {
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

pub(super) fn builtin_io_read_all(vm: &mut Vm, args: Vec<Value>) -> VmResult<BuiltinCallOutcome> {
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

pub(super) fn builtin_io_read_line(vm: &mut Vm, args: Vec<Value>) -> VmResult<BuiltinCallOutcome> {
    let handle_id = arg_handle_id(&args, 0, "io_read_line handle")?;
    let handle = io_take_handle(vm, handle_id)?;
    let op_id = schedule_io_task(vm, move || {
        let mut handle = handle;
        let result = match &mut handle {
            IoHandle::File(file) => {
                read_line_from_reader(file).map(|line| vec![Value::String(line)])
            }
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

pub(super) fn builtin_io_write(vm: &mut Vm, args: Vec<Value>) -> VmResult<BuiltinCallOutcome> {
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

pub(super) fn builtin_io_flush(vm: &mut Vm, args: Vec<Value>) -> VmResult<BuiltinCallOutcome> {
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

pub(super) fn builtin_io_close(vm: &mut Vm, args: Vec<Value>) -> VmResult<BuiltinCallOutcome> {
    let handle_id = arg_handle_id(&args, 0, "io_close handle")?;
    let handle = io_take_handle(vm, handle_id)?;
    let op_id = schedule_io_task(vm, move || IoAsyncCompletion {
        restored_handle: None,
        result: close_io_handle(handle).map(|_| vec![Value::Bool(true)]),
    })?;
    Ok(BuiltinCallOutcome::Pending(op_id))
}

pub(super) fn builtin_io_exists(vm: &mut Vm, args: Vec<Value>) -> VmResult<BuiltinCallOutcome> {
    let path = arg_string(&args, 0, "io_exists path")?.to_string();
    let op_id = schedule_io_task(vm, move || IoAsyncCompletion {
        restored_handle: None,
        result: Ok(vec![Value::Bool(
            std::path::Path::new(path.as_str()).exists(),
        )]),
    })?;
    Ok(BuiltinCallOutcome::Pending(op_id))
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

const NAMESPACE_MEMBERS: &[BuiltinNamespaceMember] = &[
    BuiltinNamespaceMember::new("open", BuiltinFunction::IoOpen),
    BuiltinNamespaceMember::new("popen", BuiltinFunction::IoPopen),
    BuiltinNamespaceMember::new("read_all", BuiltinFunction::IoReadAll),
    BuiltinNamespaceMember::new("read_line", BuiltinFunction::IoReadLine),
    BuiltinNamespaceMember::new("write", BuiltinFunction::IoWrite),
    BuiltinNamespaceMember::new("flush", BuiltinFunction::IoFlush),
    BuiltinNamespaceMember::new("close", BuiltinFunction::IoClose),
    BuiltinNamespaceMember::new("exists", BuiltinFunction::IoExists),
];

pub(crate) fn register_builtin_namespace(registry: &mut BuiltinNamespaceRegistry) {
    registry.register(BuiltinNamespace::new("io", NAMESPACE_MEMBERS, false));
}
