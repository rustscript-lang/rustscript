use axum::http::HeaderName;
use vm::{CallOutcome, HostFunction, Value, Vm, VmError};

use super::super::{
    SharedProxyVmContext, SharedVmAsyncOps, bind_async_host, expect_arg_count, expect_int,
    expect_string, headers_to_value_map, parse_header_args, parse_header_name_arg,
    parse_headers_map_arg,
};

pub(super) fn register(vm: &mut Vm, context: SharedProxyVmContext, async_ops: SharedVmAsyncOps) {
    bind_async_host(
        vm,
        &async_ops,
        "http::response::set_header",
        Box::new(SetResponseHeaderFunction::new(context.clone())),
    );
    bind_async_host(
        vm,
        &async_ops,
        "http::response::remove_header",
        Box::new(RemoveResponseHeaderFunction::new(context.clone())),
    );
    bind_async_host(
        vm,
        &async_ops,
        "http::response::set_body",
        Box::new(SetResponseContentFunction::new(context.clone())),
    );
    bind_async_host(
        vm,
        &async_ops,
        "http::response::set_status",
        Box::new(SetResponseStatusFunction::new(context.clone())),
    );
    bind_async_host(
        vm,
        &async_ops,
        "http::response::get_status",
        Box::new(GetResponseStatusFunction::new(context.clone())),
    );
    bind_async_host(
        vm,
        &async_ops,
        "http::response::get_body",
        Box::new(GetResponseBodyFunction::new(context.clone())),
    );
    bind_async_host(
        vm,
        &async_ops,
        "http::response::get_header",
        Box::new(GetResponseHeaderFunction::new(context.clone())),
    );
    bind_async_host(
        vm,
        &async_ops,
        "http::response::get_headers",
        Box::new(GetResponseHeadersFunction::new(context.clone())),
    );
    bind_async_host(
        vm,
        &async_ops,
        "http::response::add_header",
        Box::new(AddResponseHeaderFunction::new(context.clone())),
    );
    bind_async_host(
        vm,
        &async_ops,
        "http::response::clear_header",
        Box::new(ClearResponseHeaderFunction::new(context.clone())),
    );
    bind_async_host(
        vm,
        &async_ops,
        "http::response::set_headers",
        Box::new(SetResponseHeadersFunction::new(context)),
    );
}

struct SetResponseHeaderFunction {
    context: SharedProxyVmContext,
}

impl SetResponseHeaderFunction {
    fn new(context: SharedProxyVmContext) -> Self {
        Self { context }
    }
}

impl HostFunction for SetResponseHeaderFunction {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        expect_arg_count(args, 2)?;
        let (header_name, header_value) = parse_header_args(args)?;

        let mut context = self.context.lock().expect("vm context lock poisoned");
        context.touch_response_output();
        context.response_headers.insert(header_name, header_value);
        Ok(CallOutcome::Return(vec![]))
    }
}

struct AddResponseHeaderFunction {
    context: SharedProxyVmContext,
}

impl AddResponseHeaderFunction {
    fn new(context: SharedProxyVmContext) -> Self {
        Self { context }
    }
}

impl HostFunction for AddResponseHeaderFunction {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        expect_arg_count(args, 2)?;
        let (header_name, header_value) = parse_header_args(args)?;

        let mut context = self.context.lock().expect("vm context lock poisoned");
        context.touch_response_output();
        context.response_headers.append(header_name, header_value);
        Ok(CallOutcome::Return(vec![]))
    }
}

struct SetResponseHeadersFunction {
    context: SharedProxyVmContext,
}

impl SetResponseHeadersFunction {
    fn new(context: SharedProxyVmContext) -> Self {
        Self { context }
    }
}

impl HostFunction for SetResponseHeadersFunction {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        expect_arg_count(args, 1)?;
        let headers = parse_headers_map_arg(args, 0)?;

        let mut context = self.context.lock().expect("vm context lock poisoned");
        context.touch_response_output();
        for (name, values) in headers {
            context.response_headers.remove(name.clone());
            for value in values {
                context.response_headers.append(name.clone(), value);
            }
        }
        Ok(CallOutcome::Return(vec![]))
    }
}

struct RemoveResponseHeaderFunction {
    context: SharedProxyVmContext,
}

impl RemoveResponseHeaderFunction {
    fn new(context: SharedProxyVmContext) -> Self {
        Self { context }
    }
}

impl HostFunction for RemoveResponseHeaderFunction {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        expect_arg_count(args, 1)?;
        let header_name = parse_header_name_arg(args, 0)?;

        let mut context = self.context.lock().expect("vm context lock poisoned");
        context.touch_response_output();
        context.response_headers.remove(header_name);
        Ok(CallOutcome::Return(vec![]))
    }
}

struct ClearResponseHeaderFunction {
    context: SharedProxyVmContext,
}

impl ClearResponseHeaderFunction {
    fn new(context: SharedProxyVmContext) -> Self {
        Self { context }
    }
}

impl HostFunction for ClearResponseHeaderFunction {
    fn call(&mut self, vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        RemoveResponseHeaderFunction::new(self.context.clone()).call(vm, args)
    }
}

struct GetResponseHeaderFunction {
    context: SharedProxyVmContext,
}

impl GetResponseHeaderFunction {
    fn new(context: SharedProxyVmContext) -> Self {
        Self { context }
    }
}

impl HostFunction for GetResponseHeaderFunction {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        expect_arg_count(args, 1)?;
        let name = expect_string(args, 0)?;
        let header_name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| VmError::HostError(format!("invalid header name '{name}'")))?;

        let mut context = self.context.lock().expect("vm context lock poisoned");
        context.touch_response_output();
        let value = context
            .response_headers
            .get(&header_name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("");
        Ok(CallOutcome::Return(vec![Value::String(value.to_string())]))
    }
}

struct GetResponseHeadersFunction {
    context: SharedProxyVmContext,
}

impl GetResponseHeadersFunction {
    fn new(context: SharedProxyVmContext) -> Self {
        Self { context }
    }
}

impl HostFunction for GetResponseHeadersFunction {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        expect_arg_count(args, 0)?;
        let mut context = self.context.lock().expect("vm context lock poisoned");
        context.touch_response_output();
        Ok(CallOutcome::Return(vec![headers_to_value_map(
            &context.response_headers,
        )]))
    }
}

struct SetResponseContentFunction {
    context: SharedProxyVmContext,
}

impl SetResponseContentFunction {
    fn new(context: SharedProxyVmContext) -> Self {
        Self { context }
    }
}

impl HostFunction for SetResponseContentFunction {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        expect_arg_count(args, 1)?;
        let body = expect_string(args, 0)?;
        let mut context = self.context.lock().expect("vm context lock poisoned");
        context.touch_response_output();
        context.response_content = Some(body);
        Ok(CallOutcome::Return(vec![]))
    }
}

struct GetResponseBodyFunction {
    context: SharedProxyVmContext,
}

impl GetResponseBodyFunction {
    fn new(context: SharedProxyVmContext) -> Self {
        Self { context }
    }
}

impl HostFunction for GetResponseBodyFunction {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        expect_arg_count(args, 0)?;
        let mut context = self.context.lock().expect("vm context lock poisoned");
        context.touch_response_output();
        let value = context.response_content.clone().unwrap_or_default();
        Ok(CallOutcome::Return(vec![Value::String(value)]))
    }
}

struct SetResponseStatusFunction {
    context: SharedProxyVmContext,
}

impl SetResponseStatusFunction {
    fn new(context: SharedProxyVmContext) -> Self {
        Self { context }
    }
}

impl HostFunction for SetResponseStatusFunction {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        expect_arg_count(args, 1)?;
        let status = expect_int(args, 0)?;
        if !(100..=599).contains(&status) {
            return Err(VmError::HostError(format!(
                "status code must be in range 100..=599, got '{status}'",
            )));
        }

        let mut context = self.context.lock().expect("vm context lock poisoned");
        context.touch_response_output();
        context.response_status = Some(status as u16);
        Ok(CallOutcome::Return(vec![]))
    }
}

struct GetResponseStatusFunction {
    context: SharedProxyVmContext,
}

impl GetResponseStatusFunction {
    fn new(context: SharedProxyVmContext) -> Self {
        Self { context }
    }
}

impl HostFunction for GetResponseStatusFunction {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        expect_arg_count(args, 0)?;
        let mut context = self.context.lock().expect("vm context lock poisoned");
        context.touch_response_output();
        let status = context.response_status.unwrap_or(0);
        Ok(CallOutcome::Return(vec![Value::Int(status as i64)]))
    }
}
