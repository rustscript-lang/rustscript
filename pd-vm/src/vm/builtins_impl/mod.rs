// VM-side builtin execution entrypoints.
// Builtin metadata and call-index mapping live in crate::builtins.
use std::task::{Context, Poll};

use crate::builtins::BuiltinFunction;

use super::{HostOpId, Value, Vm, VmError, VmResult};

mod core;
#[cfg(not(target_arch = "wasm32"))]
mod io;
#[cfg(target_arch = "wasm32")]
mod io_wasm;
mod jit;
mod json;
pub(crate) mod print;
mod regex;
mod runtime;

#[cfg(target_arch = "wasm32")]
use io_wasm as io;

pub(in crate::vm) use io::IoState;

pub(super) enum BuiltinCallOutcome {
    Return(Vec<Value>),
    Pending(HostOpId),
}

pub(crate) fn register_default_host_functions(registry: &mut super::HostFunctionRegistry) {
    runtime::register_default_host_functions(registry);
}

pub(crate) fn bind_default_host_function(vm: &mut Vm, name: &str) -> bool {
    runtime::bind_default_host_function(vm, name)
}

pub(crate) fn register_builtin_namespaces(
    registry: &mut crate::builtins::BuiltinNamespaceRegistry,
) {
    io::register_builtin_namespace(registry);
    regex::register_builtin_namespace(registry);
    json::register_builtin_namespace(registry);
    jit::register_builtin_namespace(registry);
}

pub(super) fn execute_builtin_call(
    vm: &mut Vm,
    builtin: BuiltinFunction,
    args: Vec<Value>,
) -> VmResult<BuiltinCallOutcome> {
    match builtin {
        BuiltinFunction::Len => core::builtin_len(&args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::Slice => core::builtin_slice(args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::Concat => core::builtin_concat(args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::ArrayNew => Ok(BuiltinCallOutcome::Return(vec![Value::Array(Vec::new())])),
        BuiltinFunction::ArrayPush => {
            core::builtin_array_push(args).map(BuiltinCallOutcome::Return)
        }
        BuiltinFunction::MapNew => Ok(BuiltinCallOutcome::Return(vec![Value::Map(Vec::new())])),
        BuiltinFunction::Get => core::builtin_get(args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::Set => core::builtin_set(args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::Keys => core::builtin_keys(args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::IoOpen => io::builtin_io_open(vm, args),
        BuiltinFunction::IoPopen => io::builtin_io_popen(vm, args),
        BuiltinFunction::IoReadAll => io::builtin_io_read_all(vm, args),
        BuiltinFunction::IoReadLine => io::builtin_io_read_line(vm, args),
        BuiltinFunction::IoWrite => io::builtin_io_write(vm, args),
        BuiltinFunction::IoFlush => io::builtin_io_flush(vm, args),
        BuiltinFunction::IoClose => io::builtin_io_close(vm, args),
        BuiltinFunction::IoExists => io::builtin_io_exists(vm, args),
        BuiltinFunction::Count => core::builtin_count(&args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::ReIsMatch => {
            regex::builtin_re_is_match(&args).map(BuiltinCallOutcome::Return)
        }
        BuiltinFunction::ReFind => regex::builtin_re_find(&args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::ReReplace => {
            regex::builtin_re_replace(&args).map(BuiltinCallOutcome::Return)
        }
        BuiltinFunction::ReSplit => regex::builtin_re_split(&args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::ReCaptures => {
            regex::builtin_re_captures(&args).map(BuiltinCallOutcome::Return)
        }
        BuiltinFunction::JsonEncode => {
            json::builtin_json_encode(&args).map(BuiltinCallOutcome::Return)
        }
        BuiltinFunction::JsonDecode => {
            json::builtin_json_decode(&args).map(BuiltinCallOutcome::Return)
        }
        BuiltinFunction::JitSetConfig => jit::builtin_jit_set_config(vm, args),
        BuiltinFunction::JitGetConfig => jit::builtin_jit_get_config(vm),
        BuiltinFunction::JitSetEnabled => jit::builtin_jit_set_enabled(vm, args),
        BuiltinFunction::JitGetEnabled => jit::builtin_jit_get_enabled(vm),
        BuiltinFunction::JitSetHotLoopThreshold => {
            jit::builtin_jit_set_hot_loop_threshold(vm, args)
        }
        BuiltinFunction::JitGetHotLoopThreshold => jit::builtin_jit_get_hot_loop_threshold(vm),
        BuiltinFunction::JitSetMaxTraceLen => jit::builtin_jit_set_max_trace_len(vm, args),
        BuiltinFunction::JitGetMaxTraceLen => jit::builtin_jit_get_max_trace_len(vm),
        BuiltinFunction::FormatTemplate => {
            core::builtin_format_template(args).map(BuiltinCallOutcome::Return)
        }
        BuiltinFunction::ToString => core::builtin_to_string(&args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::TypeOf => core::builtin_type_of(&args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::Assert => core::builtin_assert(&args).map(BuiltinCallOutcome::Return),
    }
}

pub(super) fn poll_builtin_io_op(
    vm: &mut Vm,
    op_id: HostOpId,
    cx: &mut Context<'_>,
) -> Poll<VmResult<Vec<Value>>> {
    io::poll_builtin_io_op(vm, op_id, cx)
}

pub(super) fn close_all_handles(vm: &mut Vm) {
    io::close_all_handles(vm);
}

fn arg_string<'a>(args: &'a [Value], index: usize, label: &str) -> VmResult<&'a str> {
    match args.get(index) {
        Some(Value::String(value)) => Ok(value.as_str()),
        Some(_) => Err(VmError::TypeMismatch("string")),
        None => Err(VmError::HostError(format!("missing argument: {label}"))),
    }
}
