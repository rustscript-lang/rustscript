// VM-side builtin execution entrypoints.
// Builtin metadata and call-index mapping live in crate::builtins.
use std::task::{Context, Poll};

use crate::builtins::BuiltinFunction;

use super::{CallOutcome, HostOpId, Value, Vm, VmResult};

mod core;
#[cfg(not(target_arch = "wasm32"))]
mod io;
#[cfg(target_arch = "wasm32")]
mod io_wasm;
mod jit;
mod json;
mod math;
pub(crate) mod print;
mod regex;
mod runtime;
mod typed;

#[cfg(target_arch = "wasm32")]
use io_wasm as io;

pub(in crate::vm) use io::IoState;
use typed::{
    AnyValue, BuiltinResult, IntoBuiltinCallOutcome, IntoHostCallOutcome, NumberValue,
    UnknownValue, VmArray, VmMap, arg, return_values,
};

pub(super) enum BuiltinCallOutcome {
    Return(Vec<Value>),
    Pending(HostOpId),
}

include!(concat!(
    env!("OUT_DIR"),
    "/builtin_runtime_dispatch_generated.rs"
));

pub(super) fn execute_builtin_call(
    vm: &mut Vm,
    builtin: BuiltinFunction,
    args: Vec<Value>,
) -> VmResult<BuiltinCallOutcome> {
    match builtin {
        BuiltinFunction::Len => core::builtin_len(&args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::Slice => core::builtin_slice(args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::Concat => core::builtin_concat(args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::ArrayNew => Ok(BuiltinCallOutcome::Return(return_values(
            core::builtin_array_new_impl(),
        ))),
        BuiltinFunction::ArrayPush => {
            core::builtin_array_push(args).map(BuiltinCallOutcome::Return)
        }
        BuiltinFunction::MapNew => Ok(BuiltinCallOutcome::Return(return_values(
            core::builtin_map_new_impl(),
        ))),
        BuiltinFunction::Get => core::builtin_get(args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::Set => core::builtin_set(args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::Keys => core::builtin_keys(args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::Count => core::builtin_count(&args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::FormatTemplate => core::builtin_format_template(&args)
            .map(IntoBuiltinCallOutcome::into_builtin_call_outcome),
        BuiltinFunction::ToString => {
            core::builtin_to_string(&args).map(IntoBuiltinCallOutcome::into_builtin_call_outcome)
        }
        BuiltinFunction::TypeOf => {
            core::builtin_type_of(&args).map(IntoBuiltinCallOutcome::into_builtin_call_outcome)
        }
        BuiltinFunction::Assert => {
            core::builtin_assert(&args).map(IntoBuiltinCallOutcome::into_builtin_call_outcome)
        }
        _ => execute_namespaced_builtin_call(vm, builtin, args),
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
