// VM-side builtin execution entrypoints.
// Builtin metadata and call-index mapping live in crate::builtins.
use std::task::{Context, Poll};

use crate::builtins::BuiltinFunction;
use crate::vm::{CallOutcome, CallReturn, HostOpId, Value, Vm, VmResult};
#[cfg(feature = "async")]
use crate::vm::{CaptureAsyncHostContext, HostFutureOutput, VmError};

use self::cancellation::{CancellationReason, OperationId, OperationOwner, OperationState};
use self::error::{RuntimeError, RuntimeErrorCode};
use self::resource::ResourceHandle;

type RuntimeOperationPoller = fn(&mut Vm, HostOpId, &mut Context<'_>) -> Poll<VmResult<CallReturn>>;

const RUNTIME_OPERATION_POLLERS: &[(OperationOwner, RuntimeOperationPoller)] = &[
    #[cfg(not(feature = "async"))]
    (OperationOwner::Io, io::poll_builtin_io_op),
];

mod aot;
mod bytes;
pub(crate) mod cancellation;
pub(crate) mod context;
mod context_host;
pub(crate) mod core;
pub(crate) mod error;
pub(crate) mod event;
mod host;
#[cfg(feature = "http-client")]
mod http;
mod io;
#[cfg(target_arch = "wasm32")]
mod io_wasm;
mod jit;
mod json;
mod map_iter;
mod math;
pub(crate) mod print;
pub(crate) mod regex;
pub(crate) mod resource;
#[cfg(feature = "sqlite")]
mod sqlite;
mod typed;

#[cfg(feature = "http-client")]
pub use http::{HttpConfig, HttpHostExt};
pub use io::{IoHostExt, IoPolicy};
#[cfg(feature = "sqlite")]
pub use sqlite::{
    SqliteExtension, SqliteHostExt, SqliteLimits, SqlitePolicy, register_sqlite_builtin_module,
    sqlite_host_catalog,
};
pub use typed::HostCallResult;
// Typed argument decoders used by `#[pd_host_function]`-generated wrappers.
// Re-exported through `builtins::runtime` (and the crate root) so host SDK
// adapters outside the builtin modules can decode by reference / by take.
#[allow(unused_imports)]
use typed::{
    AnyValue, IntoBuiltinCallOutcome, NumberValue, UnknownValue, VmArray, VmBytes, VmCallable,
    VmMap, return_none,
};
pub use typed::{
    BorrowVmValue, FromVmValue, IntoHostCallOutcome, TakeVmValue, arg, borrow_arg, return_one,
    take_arg,
};

pub(crate) enum BuiltinCallOutcome {
    Return(CallReturn),
    #[allow(dead_code)]
    Halt,
    Pending(HostOpId),
}

include!(concat!(
    env!("OUT_DIR"),
    "/builtin_runtime_dispatch_generated.rs"
));

pub(crate) fn execute_builtin_call(
    vm: &mut Vm,
    builtin: BuiltinFunction,
    args: &mut [Value],
) -> VmResult<BuiltinCallOutcome> {
    match builtin {
        BuiltinFunction::Len => core::builtin_len(args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::Slice => core::builtin_slice(args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::Concat => core::builtin_concat(args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::ArrayNew => Ok(BuiltinCallOutcome::Return(return_one(
            core::builtin_array_new_impl(),
        ))),
        BuiltinFunction::ArrayPush => {
            core::builtin_array_push(args).map(BuiltinCallOutcome::Return)
        }
        BuiltinFunction::MapNew => Ok(BuiltinCallOutcome::Return(return_one(
            core::builtin_map_new_impl(),
        ))),
        BuiltinFunction::Get => core::builtin_get(args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::Has => core::builtin_has(args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::Set => core::builtin_set(args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::Keys => core::builtin_keys(args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::Count => core::builtin_count(args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::MapIterInit => map_iter::init(vm, args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::MapIterNext => map_iter::next(vm, args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::MapIterTakeKey => {
            map_iter::take_key(vm, args).map(BuiltinCallOutcome::Return)
        }
        BuiltinFunction::MapIterTakeValue => {
            map_iter::take_value(vm, args).map(BuiltinCallOutcome::Return)
        }
        BuiltinFunction::MapIterClose => map_iter::close(vm, args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::BindCallable => {
            let prototype_id = match args.first() {
                Some(Value::Int(value)) => u32::try_from(*value)
                    .map_err(|_| crate::vm::VmError::InvalidCallablePrototype(u32::MAX))?,
                _ => return Err(crate::vm::VmError::TypeMismatch("callable prototype id")),
            };
            let captures = std::mem::replace(
                args.get_mut(1)
                    .ok_or(crate::vm::VmError::TypeMismatch("callable captures"))?,
                Value::Null,
            )
            .into_owned_array()
            .map_err(|_| crate::vm::VmError::TypeMismatch("callable captures"))?;
            vm.bind_callable_value(prototype_id, captures)
                .map(|value| BuiltinCallOutcome::Return(return_one(value)))
        }
        BuiltinFunction::DetachLocal => {
            let slot = match args.first() {
                Some(Value::Int(value)) => u8::try_from(*value)
                    .map_err(|_| crate::vm::VmError::TypeMismatch("local slot"))?,
                _ => return Err(crate::vm::VmError::TypeMismatch("local slot")),
            };
            vm.detach_local_with_drop_contract(slot)?;
            Ok(BuiltinCallOutcome::Return(return_none()))
        }
        BuiltinFunction::StringContains => core::builtin_string_contains(args)
            .map(IntoBuiltinCallOutcome::into_builtin_call_outcome),
        BuiltinFunction::StringReplaceLiteral => core::builtin_string_replace_literal(args)
            .map(IntoBuiltinCallOutcome::into_builtin_call_outcome),
        BuiltinFunction::StringLowerAscii => core::builtin_string_lower_ascii(args)
            .map(IntoBuiltinCallOutcome::into_builtin_call_outcome),
        BuiltinFunction::StringSplitLiteral => core::builtin_string_split_literal(args)
            .map(IntoBuiltinCallOutcome::into_builtin_call_outcome),
        BuiltinFunction::FormatTemplate => core::builtin_format_template(args)
            .map(IntoBuiltinCallOutcome::into_builtin_call_outcome),
        BuiltinFunction::ToString => {
            core::builtin_to_string(args).map(IntoBuiltinCallOutcome::into_builtin_call_outcome)
        }
        BuiltinFunction::TypeOf => {
            core::builtin_type_of(args).map(IntoBuiltinCallOutcome::into_builtin_call_outcome)
        }
        BuiltinFunction::Assert => core::builtin_assert(args).map(|()| {
            // Successful asserts are control checks, not value-producing expressions.
            BuiltinCallOutcome::Return(return_none())
        }),
        _ => execute_namespaced_builtin_call(vm, builtin, args),
    }
}

pub(crate) fn cancel_builtin_io_op_with_reason(
    vm: &mut Vm,
    op_id: HostOpId,
    reason: CancellationReason,
) {
    let Ok(op_id) = OperationId::from_raw(op_id) else {
        return;
    };
    let target_resource = vm
        .host
        .runtime_operations
        .get(op_id)
        .ok()
        .filter(|operation| operation.owner() == OperationOwner::Io)
        .and_then(|operation| operation.resource());
    cancel_runtime_operation(vm, op_id, reason);
    if let Some(target_resource) = target_resource {
        let _ = close_runtime_resource(vm, target_resource, reason);
    }
}

pub(crate) fn cancel_runtime_operation(
    vm: &mut Vm,
    op_id: OperationId,
    reason: CancellationReason,
) {
    let payload = vm
        .host
        .runtime_operations
        .get(op_id)
        .ok()
        .and_then(|operation| operation.payload());
    let _ = vm.host.runtime_operations.cancel(op_id, reason);
    if let Some(payload) = payload {
        let _ = close_runtime_resource(vm, payload, reason);
    }
}

fn cancel_runtime_operations(
    vm: &mut Vm,
    operations: Vec<OperationState>,
    reason: CancellationReason,
) {
    let operations = operations
        .into_iter()
        .map(|operation| {
            let payload = operation.payload();
            (operation, payload)
        })
        .collect::<Vec<_>>();
    for (operation, _) in &operations {
        operation.token().mark_cancelled(reason);
    }
    for (operation, _) in &operations {
        let _ = vm.host.runtime_operations.cancel(operation.id(), reason);
    }
    for (_, payload) in operations {
        if let Some(payload) = payload {
            let _ = close_runtime_resource(vm, payload, reason);
        }
    }
}

pub(crate) fn close_runtime_resource(
    vm: &mut Vm,
    handle: ResourceHandle,
    reason: CancellationReason,
) -> error::RuntimeResult<resource::CloseStatus> {
    let operations = vm.host.runtime_operations.operations_for_resource(handle);
    cancel_runtime_operations(vm, operations, reason);
    vm.host.runtime_resources.close(handle, reason)
}

pub(crate) fn poll_builtin_io_op(
    vm: &mut Vm,
    op_id: HostOpId,
    cx: &mut Context<'_>,
) -> Poll<VmResult<CallReturn>> {
    let operation_id = match OperationId::from_raw(op_id) {
        Ok(operation_id) => operation_id,
        Err(error) => {
            return Poll::Ready(Err(crate::vm::VmError::HostError(error.to_string())));
        }
    };
    let operation = match vm.host.runtime_operations.get(operation_id) {
        Ok(operation) => operation,
        Err(error) => {
            return Poll::Ready(Err(crate::vm::VmError::HostError(error.to_string())));
        }
    };
    if let Err(error) = operation.token().check() {
        let reason = operation
            .token()
            .reason()
            .unwrap_or(CancellationReason::Requested);
        cancel_builtin_io_op_with_reason(vm, op_id, reason);
        return Poll::Ready(Err(crate::vm::VmError::HostError(error.to_string())));
    }

    let Some((_, poller)) = RUNTIME_OPERATION_POLLERS
        .iter()
        .find(|(owner, _)| *owner == operation.owner())
    else {
        return Poll::Ready(Err(crate::vm::VmError::HostError(format!(
            "runtime operation owner {:?} is unavailable in this build",
            operation.owner()
        ))));
    };
    let result = poller(vm, op_id, cx);

    match result {
        Poll::Pending => Poll::Pending,
        Poll::Ready(Ok(values)) => {
            let _ = vm.host.runtime_operations.complete(operation_id);
            Poll::Ready(Ok(values))
        }
        Poll::Ready(Err(error)) => {
            if let Some(reason) = operation.token().reason() {
                cancel_builtin_io_op_with_reason(vm, op_id, reason);
                return Poll::Ready(Err(error));
            }
            let runtime_error = RuntimeError::new(
                RuntimeErrorCode::OperationFailed,
                "runtime::operation",
                error.to_string(),
            )
            .with_value(op_id);
            let _ = vm.host.runtime_operations.fail(operation_id, runtime_error);
            Poll::Ready(Err(error))
        }
    }
}

pub(crate) fn close_all_handles(vm: &mut Vm) {
    vm.host.reset_for_reuse();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{OpCode, Program};

    #[test]
    fn builtin_assert_success_returns_no_stack_value() {
        let mut vm = Vm::new(Program::new(Vec::new(), vec![OpCode::Ret as u8]));
        let mut args = [Value::Bool(true)];

        let outcome = execute_builtin_call(&mut vm, BuiltinFunction::Assert, &mut args)
            .expect("assert should succeed");

        match outcome {
            BuiltinCallOutcome::Return(values) => assert!(
                values.is_empty(),
                "successful assert should not push a null sentinel"
            ),
            BuiltinCallOutcome::Halt => {
                panic!("assert should not halt builtin execution");
            }
            BuiltinCallOutcome::Pending(op_id) => {
                panic!("assert should not yield pending host op {op_id}")
            }
        }
    }
}
