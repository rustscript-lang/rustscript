use axum::http::HeaderName;
use vm::{CallOutcome, Value, Vm, VmError};

use super::super::{
    SharedProxyVmContext, SharedVmAsyncOps, bind_async_host_handler, expect_arg_count, expect_int,
    expect_string, headers_to_value_map, query_to_value_map, read_request_body_all,
    read_request_body_next_chunk, request_body_eof, request_path_with_query, schedule_future_call,
};

macro_rules! bind_request_handler {
    ($vm:expr, $async_ops:expr, $symbol:literal, $context:expr, |$vm_arg:ident, $args_arg:ident, $context_arg:ident| $body:block) => {{
        let context = $context.clone();
        bind_async_host_handler($vm, $async_ops, $symbol, move |$vm_arg, $args_arg| {
            let mut $context_arg = context.lock().expect("vm context lock poisoned");
            $body
        });
    }};
}

pub(super) fn register(vm: &mut Vm, context: SharedProxyVmContext, async_ops: SharedVmAsyncOps) {
    let field_symbols = [
        ("http::request::get_id", RequestField::Id),
        ("http::request::get_method", RequestField::Method),
        ("http::request::get_path", RequestField::Path),
        ("http::request::get_query", RequestField::Query),
        ("http::request::get_scheme", RequestField::Scheme),
        ("http::request::get_host", RequestField::Host),
        ("http::request::get_client_ip", RequestField::ClientIp),
        (
            "http::request::get_path_with_query",
            RequestField::PathWithQuery,
        ),
        ("http::request::get_raw_query", RequestField::RawQuery),
        ("http::request::get_http_version", RequestField::HttpVersion),
    ];
    for (symbol, field) in field_symbols {
        bind_request_field(vm, &async_ops, &context, symbol, field);
    }

    bind_request_handler!(
        vm,
        &async_ops,
        "http::request::get_header",
        context,
        |_vm, args, context| {
            expect_arg_count(args, 1)?;
            let name = expect_string(args, 0)?;
            let header_name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| VmError::HostError(format!("invalid header name '{name}'")))?;
            context.touch_request_headers();
            let value = context
                .inbound_request_headers
                .get(&header_name)
                .and_then(|value| value.to_str().ok())
                .unwrap_or("");
            Ok(CallOutcome::Return(vec![Value::string(value)]))
        }
    );
    bind_request_handler!(
        vm,
        &async_ops,
        "http::request::get_headers",
        context,
        |_vm, args, context| {
            expect_arg_count(args, 0)?;
            context.touch_request_headers();
            Ok(CallOutcome::Return(vec![headers_to_value_map(
                &context.inbound_request_headers,
            )]))
        }
    );
    bind_request_handler!(
        vm,
        &async_ops,
        "http::request::get_query_arg",
        context,
        |_vm, args, context| {
            expect_arg_count(args, 1)?;
            let name = expect_string(args, 0)?;
            context.touch_request_line();
            let value = url::form_urlencoded::parse(context.inbound_request_query.as_bytes())
                .find_map(|(key, value)| {
                    if key == name {
                        Some(value.into_owned())
                    } else {
                        None
                    }
                })
                .unwrap_or_default();
            Ok(CallOutcome::Return(vec![Value::string(value)]))
        }
    );
    bind_request_handler!(
        vm,
        &async_ops,
        "http::request::get_query_args",
        context,
        |_vm, args, context| {
            expect_arg_count(args, 0)?;
            context.touch_request_line();
            Ok(CallOutcome::Return(vec![query_to_value_map(
                &context.inbound_request_query,
            )]))
        }
    );
    bind_get_request_body(vm, &async_ops, &context);
    bind_request_handler!(
        vm,
        &async_ops,
        "http::request::get_port",
        context,
        |_vm, args, context| {
            expect_arg_count(args, 0)?;
            context.touch_request_line();
            Ok(CallOutcome::Return(vec![Value::Int(
                context.inbound_request_port as i64,
            )]))
        }
    );
}

pub(super) fn register_streaming_extensions(
    vm: &mut Vm,
    context: SharedProxyVmContext,
    async_ops: SharedVmAsyncOps,
) {
    bind_get_request_body_chunk(vm, &async_ops, &context);
    bind_get_request_body_eof(vm, &async_ops, &context);
}

#[derive(Clone, Copy)]
enum RequestField {
    Id,
    Method,
    Path,
    Query,
    RawQuery,
    PathWithQuery,
    HttpVersion,
    Scheme,
    Host,
    ClientIp,
}

fn bind_request_field(
    vm: &mut Vm,
    async_ops: &SharedVmAsyncOps,
    context: &SharedProxyVmContext,
    symbol: &'static str,
    field: RequestField,
) {
    let context = context.clone();
    bind_async_host_handler(vm, async_ops, symbol, move |_vm, args| {
        expect_arg_count(args, 0)?;
        let mut context = context.lock().expect("vm context lock poisoned");
        context.touch_request_line();
        let value = match field {
            RequestField::Id => context.inbound_request_id.clone(),
            RequestField::Method => context.inbound_request_method.as_str().to_string(),
            RequestField::Path => context.inbound_request_path.clone(),
            RequestField::Query => context.inbound_request_query.clone(),
            RequestField::RawQuery => context.inbound_request_query.clone(),
            RequestField::PathWithQuery => request_path_with_query(
                context.inbound_request_path.as_str(),
                context.inbound_request_query.as_str(),
            ),
            RequestField::HttpVersion => context.inbound_request_http_version.clone(),
            RequestField::Scheme => context.inbound_request_scheme.clone(),
            RequestField::Host => context.inbound_request_host.clone(),
            RequestField::ClientIp => context.inbound_request_client_ip.clone(),
        };
        Ok(CallOutcome::Return(vec![Value::string(value)]))
    });
}

fn bind_get_request_body(
    vm: &mut Vm,
    async_ops: &SharedVmAsyncOps,
    context: &SharedProxyVmContext,
) {
    let context = context.clone();
    let async_ops_for_bind = async_ops.clone();
    let async_ops_for_call = async_ops_for_bind.clone();
    bind_async_host_handler(
        vm,
        &async_ops_for_bind,
        "http::request::get_body",
        move |vm, args| {
            expect_arg_count(args, 0)?;
            let context = context.clone();
            schedule_future_call(vm, &async_ops_for_call, async move {
                let body = read_request_body_all(&context).await?;
                Ok(vec![Value::string(
                    String::from_utf8_lossy(&body).into_owned(),
                )])
            })
        },
    );
}

fn bind_get_request_body_chunk(
    vm: &mut Vm,
    async_ops: &SharedVmAsyncOps,
    context: &SharedProxyVmContext,
) {
    let context = context.clone();
    let async_ops_for_bind = async_ops.clone();
    let async_ops_for_call = async_ops_for_bind.clone();
    bind_async_host_handler(
        vm,
        &async_ops_for_bind,
        "http::request::body::next_chunk",
        move |vm, args| {
            expect_arg_count(args, 1)?;
            let max_bytes = expect_int(args, 0)?;
            if max_bytes <= 0 {
                return Err(VmError::HostError(format!(
                    "body chunk size must be > 0, got '{max_bytes}'",
                )));
            }
            let context = context.clone();
            schedule_future_call(vm, &async_ops_for_call, async move {
                let chunk = read_request_body_next_chunk(&context, max_bytes as usize).await?;
                Ok(vec![Value::string(
                    String::from_utf8_lossy(&chunk).into_owned(),
                )])
            })
        },
    );
}

fn bind_get_request_body_eof(
    vm: &mut Vm,
    async_ops: &SharedVmAsyncOps,
    context: &SharedProxyVmContext,
) {
    let context = context.clone();
    let async_ops_for_bind = async_ops.clone();
    let async_ops_for_call = async_ops_for_bind.clone();
    bind_async_host_handler(
        vm,
        &async_ops_for_bind,
        "http::request::body::eof",
        move |vm, args| {
            expect_arg_count(args, 0)?;
            let context = context.clone();
            schedule_future_call(vm, &async_ops_for_call, async move {
                let eof = request_body_eof(&context).await?;
                Ok(vec![Value::Bool(eof)])
            })
        },
    );
}
