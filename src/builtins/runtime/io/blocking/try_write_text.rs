use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

use pd_host_function::pd_host_function;

use super::super::{IoWriteTextResult, io_policy, io_write_text_precheck};
use super::{HostCallResult, authorize_blocking_io_path_with_policy, schedule_io_task};
use crate::vm::{CallReturn, Value, Vm, VmResult};

/// Writes a whole UTF-8 text file and returns ordinary policy or OS failures as data.
#[pd_host_function(name = "io::try_write_text", contract = super::super::io_try_write_text_contract)]
pub(super) fn builtin_io_try_write_text(
    vm: &mut Vm,
    path: &str,
    text: &str,
) -> VmResult<HostCallResult<Value>> {
    let policy = io_policy(vm);
    let path = path.to_string();
    let text = text.to_string();
    let op_id = schedule_io_task(vm, "io::try_write_text", move |shared| {
        let result = match authorize_blocking_io_path_with_policy(policy.as_ref(), &path, true) {
            Ok(path) => match io_write_text_precheck(policy.as_ref(), &text) {
                Some(result) => result,
                None => write_text_file(&path, &text),
            },
            Err(error) => IoWriteTextResult::failure(error.result_code()),
        };
        shared.succeed(CallReturn::one(result.into_value()));
    })?;
    Ok(HostCallResult::Pending(op_id))
}

fn write_text_file(path: &Path, text: &str) -> IoWriteTextResult {
    let result = (|| -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        file.write_all(text.as_bytes())?;
        file.flush()?;
        drop(file);
        Ok(())
    })();
    match result {
        Ok(()) => IoWriteTextResult::success(),
        Err(_) => IoWriteTextResult::failure("io_error"),
    }
}
