use axum::http::Method;
use edge_abi::{AbiFunction, symbols::http::upstream::request as abi};
use vm::{CallOutcome, Vm, VmError};

use super::super::super::{
    SharedProxyVmContext, SharedVmAsyncOps, bind_async_host_handler, expect_arg_count,
    expect_string, is_valid_request_path, is_valid_upstream, parse_header_args,
    parse_header_name_arg, parse_headers_map_arg, serialize_query_pairs,
};

macro_rules! bind_upstream_request_handler {
    ($vm:expr, $async_ops:expr, $symbol:expr, $context:expr, |$vm_arg:ident, $args_arg:ident, $context_arg:ident| $body:block) => {{
        let context = $context.clone();
        bind_async_host_handler(
            $vm,
            $async_ops,
            ($symbol).name,
            move |$vm_arg, $args_arg| {
                let mut $context_arg = context.lock().expect("vm context lock poisoned");
                $body
            },
        );
    }};
}

pub(super) fn register(vm: &mut Vm, context: SharedProxyVmContext, async_ops: SharedVmAsyncOps) {
    bind_upstream_request_handler!(
        vm,
        &async_ops,
        abi::SET_HEADER,
        context,
        |_vm, args, context| {
            expect_arg_count(args, 2)?;
            let (header_name, header_value) = parse_header_args(args)?;
            context.touch_upstream_request();
            context
                .outbound_request_headers
                .insert(header_name, header_value);
            Ok(CallOutcome::Return(vec![]))
        }
    );
    bind_upstream_request_handler!(
        vm,
        &async_ops,
        abi::REMOVE_HEADER,
        context,
        |_vm, args, context| {
            expect_arg_count(args, 1)?;
            let header_name = parse_header_name_arg(args, 0)?;
            context.touch_upstream_request();
            context.outbound_request_headers.remove(header_name);
            Ok(CallOutcome::Return(vec![]))
        }
    );
    bind_upstream_request_handler!(
        vm,
        &async_ops,
        abi::SET_METHOD,
        context,
        |_vm, args, context| {
            expect_arg_count(args, 1)?;
            let method = expect_string(args, 0)?;
            let parsed = Method::from_bytes(method.as_bytes())
                .map_err(|_| VmError::HostError(format!("invalid http method '{method}'")))?;
            context.touch_upstream_request();
            context.outbound_request_method = parsed;
            Ok(CallOutcome::Return(vec![]))
        }
    );
    bind_upstream_request_handler!(
        vm,
        &async_ops,
        abi::SET_PATH,
        context,
        |_vm, args, context| {
            expect_arg_count(args, 1)?;
            let path = expect_string(args, 0)?;
            if !is_valid_request_path(&path) {
                return Err(VmError::HostError(format!(
                    "path must start with '/' and must not contain whitespace, '?', or '#', got '{path}'",
                )));
            }
            context.touch_upstream_request();
            context.outbound_request_path = path;
            Ok(CallOutcome::Return(vec![]))
        }
    );
    bind_set_query_symbol(vm, &async_ops, abi::SET_QUERY, &context);
    bind_upstream_request_handler!(
        vm,
        &async_ops,
        abi::SET_TARGET,
        context,
        |_vm, args, context| {
            expect_arg_count(args, 1)?;
            let upstream = expect_string(args, 0)?;
            if !is_valid_upstream(&upstream) {
                return Err(VmError::HostError(format!(
                    "upstream must be host:port or http(s)://host[:port][/path], got '{upstream}'",
                )));
            }
            context.touch_upstream_request();
            context.upstream = Some(upstream);
            Ok(CallOutcome::Return(vec![]))
        }
    );
    bind_upstream_request_handler!(
        vm,
        &async_ops,
        abi::SET_BODY,
        context,
        |_vm, args, context| {
            expect_arg_count(args, 1)?;
            let body = expect_string(args, 0)?;
            context.touch_upstream_request();
            context.outbound_request_body = body.into_bytes();
            context.outbound_request_body_overridden = true;
            Ok(CallOutcome::Return(vec![]))
        }
    );
    bind_upstream_request_handler!(
        vm,
        &async_ops,
        abi::ADD_HEADER,
        context,
        |_vm, args, context| {
            expect_arg_count(args, 2)?;
            let (header_name, header_value) = parse_header_args(args)?;
            context.touch_upstream_request();
            context
                .outbound_request_headers
                .append(header_name, header_value);
            Ok(CallOutcome::Return(vec![]))
        }
    );
    bind_upstream_request_handler!(
        vm,
        &async_ops,
        abi::CLEAR_HEADER,
        context,
        |_vm, args, context| {
            expect_arg_count(args, 1)?;
            let header_name = parse_header_name_arg(args, 0)?;
            context.touch_upstream_request();
            context.outbound_request_headers.remove(header_name);
            Ok(CallOutcome::Return(vec![]))
        }
    );
    bind_upstream_request_handler!(
        vm,
        &async_ops,
        abi::SET_HEADERS,
        context,
        |_vm, args, context| {
            expect_arg_count(args, 1)?;
            let headers = parse_headers_map_arg(args, 0)?;
            context.touch_upstream_request();
            for (name, values) in headers {
                context.outbound_request_headers.remove(name.clone());
                for value in values {
                    context.outbound_request_headers.append(name.clone(), value);
                }
            }
            Ok(CallOutcome::Return(vec![]))
        }
    );
    bind_set_query_symbol(vm, &async_ops, abi::SET_RAW_QUERY, &context);
    bind_upstream_request_handler!(
        vm,
        &async_ops,
        abi::SET_QUERY_ARG,
        context,
        |_vm, args, context| {
            expect_arg_count(args, 2)?;
            let key = expect_string(args, 0)?;
            let value = expect_string(args, 1)?;
            context.touch_upstream_request();
            let mut pairs = url::form_urlencoded::parse(context.outbound_request_query.as_bytes())
                .map(|(name, value)| (name.into_owned(), value.into_owned()))
                .collect::<Vec<_>>();
            pairs.retain(|(name, _)| name != &key);
            pairs.push((key, value));
            context.outbound_request_query = serialize_query_pairs(pairs);
            Ok(CallOutcome::Return(vec![]))
        }
    );
}

fn bind_set_query_symbol(
    vm: &mut Vm,
    async_ops: &SharedVmAsyncOps,
    symbol: AbiFunction,
    context: &SharedProxyVmContext,
) {
    let context = context.clone();
    bind_async_host_handler(vm, async_ops, symbol.name, move |_vm, args| {
        expect_arg_count(args, 1)?;
        let raw_query = expect_string(args, 0)?;
        let query = raw_query.strip_prefix('?').unwrap_or(raw_query.as_str());
        if query.contains('#') || query.chars().any(|ch| ch.is_whitespace()) {
            return Err(VmError::HostError(format!(
                "query must not contain whitespace or '#', got '{raw_query}'",
            )));
        }
        let mut context = context.lock().expect("vm context lock poisoned");
        context.touch_upstream_request();
        context.outbound_request_query = query.to_string();
        Ok(CallOutcome::Return(vec![]))
    });
}
