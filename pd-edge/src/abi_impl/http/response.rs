use axum::http::HeaderName;
use vm::{CallOutcome, Value, Vm, VmError};

use super::super::{
    SharedProxyVmContext, SharedVmAsyncOps, bind_async_host_handler, expect_arg_count, expect_int,
    expect_string, headers_to_value_map, parse_header_args, parse_header_name_arg,
    parse_headers_map_arg,
};

macro_rules! bind_response_handler {
    ($vm:expr, $async_ops:expr, $symbol:literal, $context:expr, |$vm_arg:ident, $args_arg:ident, $context_arg:ident| $body:block) => {{
        let context = $context.clone();
        bind_async_host_handler($vm, $async_ops, $symbol, move |$vm_arg, $args_arg| {
            let mut $context_arg = context.lock().expect("vm context lock poisoned");
            $body
        });
    }};
}

pub(super) fn register(vm: &mut Vm, context: SharedProxyVmContext, async_ops: SharedVmAsyncOps) {
    bind_response_handler!(
        vm,
        &async_ops,
        "http::response::set_header",
        context,
        |_vm, args, context| {
            expect_arg_count(args, 2)?;
            let (header_name, header_value) = parse_header_args(args)?;
            context.touch_response_output();
            context.response_headers.insert(header_name, header_value);
            Ok(CallOutcome::Return(vec![]))
        }
    );
    bind_response_handler!(
        vm,
        &async_ops,
        "http::response::remove_header",
        context,
        |_vm, args, context| {
            expect_arg_count(args, 1)?;
            let header_name = parse_header_name_arg(args, 0)?;
            context.touch_response_output();
            context.response_headers.remove(header_name);
            Ok(CallOutcome::Return(vec![]))
        }
    );
    bind_response_handler!(
        vm,
        &async_ops,
        "http::response::set_body",
        context,
        |_vm, args, context| {
            expect_arg_count(args, 1)?;
            let body = expect_string(args, 0)?;
            context.touch_response_output();
            context.response_content = Some(body);
            Ok(CallOutcome::Return(vec![]))
        }
    );
    bind_response_handler!(
        vm,
        &async_ops,
        "http::response::set_status",
        context,
        |_vm, args, context| {
            expect_arg_count(args, 1)?;
            let status = expect_int(args, 0)?;
            if !(100..=599).contains(&status) {
                return Err(VmError::HostError(format!(
                    "status code must be in range 100..=599, got '{status}'",
                )));
            }
            context.touch_response_output();
            context.response_status = Some(status as u16);
            Ok(CallOutcome::Return(vec![]))
        }
    );
    bind_response_handler!(
        vm,
        &async_ops,
        "http::response::get_status",
        context,
        |_vm, args, context| {
            expect_arg_count(args, 0)?;
            context.touch_response_output();
            let status = context.response_status.unwrap_or(0);
            Ok(CallOutcome::Return(vec![Value::Int(status as i64)]))
        }
    );
    bind_response_handler!(
        vm,
        &async_ops,
        "http::response::get_body",
        context,
        |_vm, args, context| {
            expect_arg_count(args, 0)?;
            context.touch_response_output();
            let value = context.response_content.clone().unwrap_or_default();
            Ok(CallOutcome::Return(vec![Value::String(value)]))
        }
    );
    bind_response_handler!(
        vm,
        &async_ops,
        "http::response::get_header",
        context,
        |_vm, args, context| {
            expect_arg_count(args, 1)?;
            let name = expect_string(args, 0)?;
            let header_name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| VmError::HostError(format!("invalid header name '{name}'")))?;
            context.touch_response_output();
            let value = context
                .response_headers
                .get(&header_name)
                .and_then(|value| value.to_str().ok())
                .unwrap_or("");
            Ok(CallOutcome::Return(vec![Value::String(value.to_string())]))
        }
    );
    bind_response_handler!(
        vm,
        &async_ops,
        "http::response::get_headers",
        context,
        |_vm, args, context| {
            expect_arg_count(args, 0)?;
            context.touch_response_output();
            Ok(CallOutcome::Return(vec![headers_to_value_map(
                &context.response_headers,
            )]))
        }
    );
    bind_response_handler!(
        vm,
        &async_ops,
        "http::response::add_header",
        context,
        |_vm, args, context| {
            expect_arg_count(args, 2)?;
            let (header_name, header_value) = parse_header_args(args)?;
            context.touch_response_output();
            context.response_headers.append(header_name, header_value);
            Ok(CallOutcome::Return(vec![]))
        }
    );
    bind_response_handler!(
        vm,
        &async_ops,
        "http::response::clear_header",
        context,
        |_vm, args, context| {
            expect_arg_count(args, 1)?;
            let header_name = parse_header_name_arg(args, 0)?;
            context.touch_response_output();
            context.response_headers.remove(header_name);
            Ok(CallOutcome::Return(vec![]))
        }
    );
    bind_response_handler!(
        vm,
        &async_ops,
        "http::response::set_headers",
        context,
        |_vm, args, context| {
            expect_arg_count(args, 1)?;
            let headers = parse_headers_map_arg(args, 0)?;
            context.touch_response_output();
            for (name, values) in headers {
                context.response_headers.remove(name.clone());
                for value in values {
                    context.response_headers.append(name.clone(), value);
                }
            }
            Ok(CallOutcome::Return(vec![]))
        }
    );
}
