use pd_host_function::pd_host_function;
use vm::{GuestResult, VmError, VmResult};

/// Returns a guest value or a fatal transport failure.
#[pd_host_function(name = "example::read")]
fn read(mode: i64) -> VmResult<GuestResult<i64, String>> {
    match mode {
        0 => Ok(GuestResult::Ok(7)),
        1 => Ok(GuestResult::Err("missing".into())),
        _ => Err(VmError::HostError("transport failed".into())),
    }
}
