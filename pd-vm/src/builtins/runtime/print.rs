use crate::vm::{CallOutcome, CallReturn, HostFunction, Value, Vm, VmResult};

pub fn format_value(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Int(value) => value.to_string(),
        Value::Float(value) => value.to_string(),
        Value::Bool(value) => value.to_string(),
        Value::String(value) => value.as_str().to_string(),
        Value::Bytes(value) => format_bytes(value.as_ref()),
        Value::Array(values) => {
            let parts = values
                .iter()
                .map(format_value)
                .collect::<Vec<_>>()
                .join(", ");
            format!("[{parts}]")
        }
        Value::Map(entries) => {
            let parts = entries
                .iter()
                .map(|(key, value)| format!("{}: {}", format_value(key), format_value(value)))
                .collect::<Vec<_>>()
                .join(", ");
            format!("{{{parts}}}")
        }
    }
}

fn format_bytes(bytes: &[u8]) -> String {
    let preview_len = bytes.len().min(16);
    let mut preview = String::with_capacity(preview_len * 2);
    for byte in &bytes[..preview_len] {
        preview.push(hex_nibble(byte >> 4));
        preview.push(hex_nibble(byte & 0x0F));
    }
    if bytes.len() > preview_len {
        format!("bytes[len={} hex={}..]", bytes.len(), preview)
    } else {
        format!("bytes[len={} hex={}]", bytes.len(), preview)
    }
}

fn hex_nibble(value: u8) -> char {
    match value {
        0..=9 => char::from(b'0' + value),
        10..=15 => char::from(b'a' + (value - 10)),
        _ => unreachable!("hex nibble out of range"),
    }
}

fn format_values(args: &[Value]) -> String {
    args.iter().map(format_value).collect::<Vec<_>>().join(" ")
}

fn borrowed_args_return(args: &[Value]) -> CallReturn {
    match args {
        [] => CallReturn::none(),
        [value] => CallReturn::one(value.clone()),
        _ => CallReturn::one(Value::array(args.to_vec())),
    }
}

pub struct PrintHostFunction<F>
where
    F: FnMut(String) + Send + 'static,
{
    sink: F,
}

impl<F> PrintHostFunction<F>
where
    F: FnMut(String) + Send + 'static,
{
    pub fn new(sink: F) -> Self {
        Self { sink }
    }
}

impl<F> HostFunction for PrintHostFunction<F>
where
    F: FnMut(String) + Send + 'static,
{
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {
        let rendered = format_values(args);
        (self.sink)(rendered);
        Ok(CallOutcome::Return(borrowed_args_return(args)))
    }
}

pub struct PrintlnHostFunction<F>
where
    F: FnMut(String) + Send + 'static,
{
    sink: F,
}

impl<F> PrintlnHostFunction<F>
where
    F: FnMut(String) + Send + 'static,
{
    pub fn new(sink: F) -> Self {
        Self { sink }
    }
}

impl<F> HostFunction for PrintlnHostFunction<F>
where
    F: FnMut(String) + Send + 'static,
{
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {
        let mut rendered = format_values(args);
        rendered.push('\n');
        (self.sink)(rendered);
        Ok(CallOutcome::Return(borrowed_args_return(args)))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use crate::bytecode::Program;
    use crate::vm::{HostFunction, Value, Vm};

    use super::{PrintHostFunction, PrintlnHostFunction, format_value};

    fn vm_for_host_call() -> Vm {
        Vm::new(Program::new(
            Vec::new(),
            vec![crate::bytecode::OpCode::Ret as u8],
        ))
    }

    #[test]
    fn format_value_renders_nested_values() {
        let value = Value::map(vec![(
            Value::string("items"),
            Value::array(vec![Value::Int(1), Value::Bool(true)]),
        )]);
        assert_eq!(format_value(&value), "{items: [1, true]}");
    }

    #[test]
    fn print_host_function_writes_to_sink() {
        let lines = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink_lines = Arc::clone(&lines);
        let mut host = PrintHostFunction::new(move |rendered| {
            if let Ok(mut guard) = sink_lines.lock() {
                guard.push(rendered);
            }
        });
        let mut vm = vm_for_host_call();

        host.call(&mut vm, &[Value::Int(2), Value::string("ok")])
            .expect("print host call should succeed");

        let guard = lines.lock().expect("sink should be lockable");
        assert_eq!(guard.as_slice(), ["2 ok"]);
    }

    #[test]
    fn println_host_function_appends_newline() {
        let lines = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink_lines = Arc::clone(&lines);
        let mut host = PrintlnHostFunction::new(move |rendered| {
            if let Ok(mut guard) = sink_lines.lock() {
                guard.push(rendered);
            }
        });
        let mut vm = vm_for_host_call();

        host.call(&mut vm, &[Value::string("line")])
            .expect("println host call should succeed");

        let guard = lines.lock().expect("sink should be lockable");
        assert_eq!(guard.as_slice(), ["line\n"]);
    }
}
