use axum::http::HeaderName;
use vm::{CallOutcome, Value, Vm, VmError};

use super::super::super::{
    SharedProxyVmContext, SharedVmAsyncOps, bind_async_host_handler, expect_arg_count,
    expect_string, headers_to_value_map,
};

macro_rules! bind_upstream_response_handler {
    ($vm:expr, $async_ops:expr, $symbol:literal, $context:expr, |$vm_arg:ident, $args_arg:ident, $context_arg:ident| $body:block) => {{
        let context = $context.clone();
        bind_async_host_handler($vm, $async_ops, $symbol, move |$vm_arg, $args_arg| {
            let mut $context_arg = context.lock().expect("vm context lock poisoned");
            $body
        });
    }};
}

pub(super) fn register(vm: &mut Vm, context: SharedProxyVmContext, async_ops: SharedVmAsyncOps) {
    bind_upstream_response_handler!(
        vm,
        &async_ops,
        "http::upstream::response::get_status",
        context,
        |_vm, args, context| {
            expect_arg_count(args, 0)?;
            context.touch_upstream_response();
            let status = context.upstream_response_status.unwrap_or(0);
            Ok(CallOutcome::Return(vec![Value::Int(status as i64)]))
        }
    );
    bind_upstream_response_handler!(
        vm,
        &async_ops,
        "http::upstream::response::get_header",
        context,
        |_vm, args, context| {
            expect_arg_count(args, 1)?;
            let name = expect_string(args, 0)?;
            let header_name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| VmError::HostError(format!("invalid header name '{name}'")))?;
            context.touch_upstream_response();
            let value = context
                .upstream_response_headers
                .get(&header_name)
                .and_then(|value| value.to_str().ok())
                .unwrap_or("");
            Ok(CallOutcome::Return(vec![Value::string(value)]))
        }
    );
    bind_upstream_response_handler!(
        vm,
        &async_ops,
        "http::upstream::response::get_headers",
        context,
        |_vm, args, context| {
            expect_arg_count(args, 0)?;
            context.touch_upstream_response();
            Ok(CallOutcome::Return(vec![headers_to_value_map(
                &context.upstream_response_headers,
            )]))
        }
    );
    bind_upstream_response_handler!(
        vm,
        &async_ops,
        "http::upstream::response::get_body",
        context,
        |_vm, args, context| {
            expect_arg_count(args, 0)?;
            context.touch_upstream_response();
            let value = context
                .upstream_response_content
                .clone()
                .unwrap_or_default();
            Ok(CallOutcome::Return(vec![Value::string(value)]))
        }
    );
}
