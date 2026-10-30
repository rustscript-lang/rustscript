use crate::host_api::{HostFunctionSchema, HostParamSchema, HostTypeSchema};
use pd_host_function::pd_host_function;

fn json_is_valid_contract() -> HostFunctionSchema {
    HostFunctionSchema::with_return(
        "json::is_valid",
        vec![HostParamSchema::value("text", HostTypeSchema::String)],
        HostTypeSchema::Bool,
    )
}

/// Returns whether `text` is accepted by the strict `json::decode` parser.
#[pd_host_function(name = "json::is_valid", contract = json_is_valid_contract)]
pub(super) fn builtin_json_is_valid(text: &str) -> bool {
    super::json::decode_strict_value(text).is_ok()
}
