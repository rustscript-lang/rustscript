use std::io::{self, Read, Write};

use edge_abi::symbols::console as console_symbols;
use pd_edge_host_function::pd_edge_host_function;
use vm::{CallOutcome, Value, Vm, VmError};

use super::current_console_program_args;

/// Reads one line from process stdin and returns it as a string.
#[pd_edge_host_function(name = console_symbols::stdin::READ_LINE.name, scope = console)]
async fn stdin_read_line(_vm: &mut Vm) -> Result<CallOutcome, VmError> {
    let line = tokio::task::spawn_blocking(move || {
        let mut input = String::new();
        io::stdin()
            .read_line(&mut input)
            .map_err(|err| VmError::HostError(format!("stdin read_line failed: {err}")))?;
        Ok::<String, VmError>(input)
    })
    .await
    .map_err(|err| VmError::HostError(format!("stdin read_line task failed: {err}")))??;
    Ok(CallOutcome::Return(vec![Value::string(line)]))
}

/// Reads all remaining bytes from process stdin and returns them as a string.
#[pd_edge_host_function(name = console_symbols::stdin::READ_ALL.name, scope = console)]
async fn stdin_read_all(_vm: &mut Vm) -> Result<CallOutcome, VmError> {
    let text = tokio::task::spawn_blocking(move || {
        let mut input = String::new();
        io::stdin()
            .read_to_string(&mut input)
            .map_err(|err| VmError::HostError(format!("stdin read_all failed: {err}")))?;
        Ok::<String, VmError>(input)
    })
    .await
    .map_err(|err| VmError::HostError(format!("stdin read_all task failed: {err}")))??;
    Ok(CallOutcome::Return(vec![Value::string(text)]))
}

/// Writes text to process stdout and returns the number of bytes written.
#[pd_edge_host_function(name = console_symbols::stdout::WRITE.name, scope = console)]
async fn stdout_write(_vm: &mut Vm, text: String) -> Result<CallOutcome, VmError> {
    let written = tokio::task::spawn_blocking(move || {
        let mut out = io::stdout().lock();
        out.write_all(text.as_bytes())
            .and_then(|_| out.flush())
            .map_err(|err| VmError::HostError(format!("stdout write failed: {err}")))?;
        Ok::<i64, VmError>(text.len() as i64)
    })
    .await
    .map_err(|err| VmError::HostError(format!("stdout write task failed: {err}")))??;
    Ok(CallOutcome::Return(vec![Value::Int(written)]))
}

/// Flushes process stdout and reports whether the flush succeeded.
#[pd_edge_host_function(name = console_symbols::stdout::FLUSH.name, scope = console)]
async fn stdout_flush(_vm: &mut Vm) -> Result<CallOutcome, VmError> {
    tokio::task::spawn_blocking(move || {
        io::stdout()
            .flush()
            .map_err(|err| VmError::HostError(format!("stdout flush failed: {err}")))?;
        Ok::<(), VmError>(())
    })
    .await
    .map_err(|err| VmError::HostError(format!("stdout flush task failed: {err}")))??;
    Ok(CallOutcome::Return(vec![Value::Bool(true)]))
}

/// Writes text to process stderr and returns the number of bytes written.
#[pd_edge_host_function(name = console_symbols::stderr::WRITE.name, scope = console)]
async fn stderr_write(_vm: &mut Vm, text: String) -> Result<CallOutcome, VmError> {
    let written = tokio::task::spawn_blocking(move || {
        let mut out = io::stderr().lock();
        out.write_all(text.as_bytes())
            .and_then(|_| out.flush())
            .map_err(|err| VmError::HostError(format!("stderr write failed: {err}")))?;
        Ok::<i64, VmError>(text.len() as i64)
    })
    .await
    .map_err(|err| VmError::HostError(format!("stderr write task failed: {err}")))??;
    Ok(CallOutcome::Return(vec![Value::Int(written)]))
}

/// Flushes process stderr and reports whether the flush succeeded.
#[pd_edge_host_function(name = console_symbols::stderr::FLUSH.name, scope = console)]
async fn stderr_flush(_vm: &mut Vm) -> Result<CallOutcome, VmError> {
    tokio::task::spawn_blocking(move || {
        io::stderr()
            .flush()
            .map_err(|err| VmError::HostError(format!("stderr flush failed: {err}")))?;
        Ok::<(), VmError>(())
    })
    .await
    .map_err(|err| VmError::HostError(format!("stderr flush task failed: {err}")))??;
    Ok(CallOutcome::Return(vec![Value::Bool(true)]))
}

/// Returns the number of arguments passed to the loaded console program.
#[pd_edge_host_function(name = console_symbols::args::COUNT.name, scope = console)]
fn args_count() -> Result<CallOutcome, VmError> {
    let program_args = current_console_program_args()?;
    Ok(CallOutcome::Return(vec![Value::Int(
        program_args.len() as i64
    )]))
}

/// Returns the argument at the requested zero-based index, or an empty string when it is missing.
#[pd_edge_host_function(name = console_symbols::args::GET.name, scope = console)]
fn args_get(index: i64) -> Result<CallOutcome, VmError> {
    if index < 0 {
        return Err(VmError::HostError(format!(
            "console::args::get expects a non-negative index, got {index}",
        )));
    }
    let program_args = current_console_program_args()?;
    let value = program_args
        .get(index as usize)
        .cloned()
        .unwrap_or_default();
    Ok(CallOutcome::Return(vec![Value::string(value)]))
}
