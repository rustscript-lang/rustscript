//! IO builtin host implementation.
//!
//! This module is the single canonical owner of the standard `io` host module:
//! the guest contracts, the descriptor ownership list, the `io.file` resource
//! metadata, and [`io_host_module`]. Only the function *implementations* are
//! selected per target and per feature:
//!
//! - `async` (non-wasm32): `async_io` drives IO through tokio and submits async
//!   host functions via the generic async host bridge.
//! - default (non-wasm32): `blocking` drives IO through worker threads
//!   registered as concrete operation drivers in the execution scope.
//! - wasm32: `wasm` (the `io_wasm.rs` backend) keeps the catalog surface and
//!   rejects every IO call with a host error; the target has no file system.
//!
//! The non-wasm32 implementations share the same execution-scope resource
//! model: live handles are `IoResource`s owned by the VM's execution scope and
//! in-flight IO work is driven by concrete operation drivers registered in the
//! same scope. Only the concurrency mechanism differs. Every backend declares
//! the same [`IO_FILE_KEY`] and [`IO_FILE_DESCRIPTION`] for its own concrete
//! handle type, so the guest contract and the target it compiles for cannot
//! drift.

use super::borrow_arg;
#[cfg(all(feature = "async", not(target_arch = "wasm32")))]
use super::{CallOutcome, CaptureAsyncHostContext, return_one};
use crate::host_extension::HostNamedStruct;
use crate::vm::Value;
#[cfg(not(target_arch = "wasm32"))]
use crate::vm::{Vm, VmError, VmResult};
#[cfg(target_arch = "wasm32")]
use crate::vm::{Vm, VmResult};
#[cfg(not(target_arch = "wasm32"))]
use std::path::PathBuf;

/// The synchronous host-call channel that hands a pending operation id back to
/// the VM. The blocking and wasm backends schedule a concrete operation driver;
/// the async backend submits a future through the generic bridge instead.
#[cfg(any(not(feature = "async"), target_arch = "wasm32"))]
pub(super) use super::HostCallResult;

/// The canonical catalog key of the `io.file` resource type.
///
/// Declared once for every backend: each backend implements
/// [`HostResourceType`](crate::host_extension::HostResourceType) for its own
/// concrete handle type but names this key and
/// [`IO_FILE_DESCRIPTION`], so the guest contract cannot drift between
/// targets.
pub(crate) const IO_FILE_KEY: &str = "io.file";

/// The canonical catalog description of the `io.file` resource type.
pub(crate) const IO_FILE_DESCRIPTION: &str = "An open file handle";

// ---- adapter policy (native backends) -------------------------------------
//
// The policy surface is enforced by the native backends: the wasm32 backend
// rejects every IO call regardless of policy, so the type and its accessors
// exist only where they are consulted.

#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IoPolicy {
    pub allowed_roots: Vec<String>,
    pub allow_write: bool,
    pub allow_process: bool,
    pub max_read_bytes: usize,
    pub max_write_bytes: usize,
}

#[cfg(not(target_arch = "wasm32"))]
impl Default for IoPolicy {
    fn default() -> Self {
        Self {
            allowed_roots: Vec::new(),
            allow_write: false,
            allow_process: false,
            max_read_bytes: 1024 * 1024,
            max_write_bytes: 1024 * 1024,
        }
    }
}

/// Typed result returned by `io::try_write_text`.
///
/// `error_code` is empty on success, and otherwise is one of `policy_denied`,
/// `write_limit`, `io_error`, or `unsupported`. The result never carries the
/// path or text. Native writes are non-atomic: an OS failure after opening may
/// leave an empty or partial file; success means write and flush completed and
/// the handle was closed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IoWriteTextResult {
    /// Whether the complete text write and flush succeeded.
    pub ok: bool,
    /// A stable, non-sensitive failure code, or the empty string on success.
    pub error_code: String,
}

impl HostNamedStruct for IoWriteTextResult {
    const NAME: &'static str = "IoWriteTextResult";

    fn host_struct_fields() -> Vec<HostStructField> {
        vec![
            HostStructField::new("ok", HostTypeSchema::Bool),
            HostStructField::new("error_code", HostTypeSchema::String),
        ]
    }
}

impl IoWriteTextResult {
    pub(super) fn success() -> Self {
        Self {
            ok: true,
            error_code: String::new(),
        }
    }

    pub(super) fn failure(error_code: &str) -> Self {
        Self {
            ok: false,
            error_code: error_code.to_string(),
        }
    }

    pub(super) fn into_value(self) -> Value {
        Value::map(vec![
            (Value::string("ok"), Value::Bool(self.ok)),
            (Value::string("error_code"), Value::string(self.error_code)),
        ])
    }
}

fn io_write_text_result_schema() -> crate::host_api::HostStructSchema {
    crate::host_api::HostStructSchema::new(
        IoWriteTextResult::NAME,
        IoWriteTextResult::host_struct_fields(),
    )
    .with_description("Result of io::try_write_text; error_code is empty on success.")
}

/// Guest contract for `io::try_write_text`.
fn io_try_write_text_contract() -> HostFunctionSchema {
    HostFunctionSchema::with_return(
        "io::try_write_text",
        vec![
            HostParamSchema::value("path", HostTypeSchema::String),
            HostParamSchema::value("text", HostTypeSchema::String),
        ],
        io_write_text_result_schema().as_type(),
    )
    .with_description(
        "Creates or truncates a UTF-8 text file. Ordinary policy, size, and OS failures return IoWriteTextResult; later OS errors may leave partial bytes.",
    )
}

const IO_NAMED_STRUCTS: &[(&str, &str)] = &[(
    "IoWriteTextResult",
    "Typed result of a whole-file UTF-8 text write; error_code is empty on success.",
)];

#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug)]
pub(super) enum IoPathAuthorizationError {
    WriteDenied,
    OutsideAllowedRoots(PathBuf),
    Resolution(String),
}

#[cfg(not(target_arch = "wasm32"))]
impl IoPathAuthorizationError {
    pub(super) fn into_vm_error(self) -> VmError {
        match self {
            Self::WriteDenied => {
                VmError::HostError("io path write requires the write capability".to_string())
            }
            Self::OutsideAllowedRoots(path) => VmError::HostError(format!(
                "io path '{}' is outside the allowed roots",
                path.display()
            )),
            Self::Resolution(message) => VmError::HostError(message),
        }
    }

    pub(super) fn result_code(&self) -> &'static str {
        match self {
            Self::WriteDenied | Self::OutsideAllowedRoots(_) => "policy_denied",
            Self::Resolution(_) => "io_error",
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub(super) fn io_write_text_precheck(
    policy: Option<&IoPolicy>,
    text: &str,
) -> Option<IoWriteTextResult> {
    let policy = policy?;
    if !policy.allow_write {
        return Some(IoWriteTextResult::failure("policy_denied"));
    }
    if text.len() > policy.max_write_bytes {
        return Some(IoWriteTextResult::failure("write_limit"));
    }
    None
}

/// I/O host configuration owned by the I/O host implementation.
#[cfg(not(target_arch = "wasm32"))]
pub trait IoHostExt {
    fn configure_io(&mut self, policy: IoPolicy);
    fn clear_io_configuration(&mut self);
}

#[cfg(not(target_arch = "wasm32"))]
impl IoHostExt for Vm {
    fn configure_io(&mut self, mut policy: IoPolicy) {
        policy.allowed_roots.sort();
        policy.allowed_roots.dedup();
        // Adapter-declared policy stored in the generic module-state store:
        // module-level policy survives execution-scope reset (an embedder's
        // roots/capabilities remain in force across `reset_for_reuse`), while
        // the adapter's per-invocation runtime state lives in the scope arena.
        self.host.set_module_state(policy);
    }

    fn clear_io_configuration(&mut self) {
        self.host.remove_module_state::<IoPolicy>();
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub(super) fn io_policy(vm: &Vm) -> Option<IoPolicy> {
    vm.host
        .get_module_state::<IoPolicy>()
        .cloned()
        .or_else(|| (!vm.host.default_builtin_capabilities_enabled()).then(IoPolicy::default))
}

/// The Reader-layer bound used to enforce `max_read_bytes` in `io::read_all`.
///
/// Collecting `limit + 1` bytes is enough to prove that the stream exceeds the
/// limit, so the read can stop at the boundary instead of waiting for EOF.
/// `None` means `limit + 1` cannot be represented in the reader's `u64` budget
/// (the limit is effectively unbounded at this address width).
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn read_all_probe_bound(limit: usize) -> Option<u64> {
    u64::try_from(limit)
        .ok()
        .and_then(|limit| limit.checked_add(1))
}

/// Turns the bytes collected under a `limit + 1` probe into the guest string.
///
/// The contract is checked before decoding so an over-limit stream reports the
/// limit even when its probe ends mid-character; only a payload within the
/// limit is validated as UTF-8, with the message the previous implementation
/// produced for invalid text.
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn decode_read_all(bytes: Vec<u8>, limit: Option<usize>) -> VmResult<String> {
    if let Some(limit) = limit
        && let Some(bound) = read_all_probe_bound(limit)
        && u64::try_from(bytes.len()).is_ok_and(|len| len >= bound)
    {
        return Err(VmError::HostError(
            "io_read_all exceeded read limit".to_string(),
        ));
    }
    String::from_utf8(bytes).map_err(|_| {
        VmError::HostError("io_read_all failed: stream did not contain valid UTF-8".to_string())
    })
}

// ---- cfg-selected implementations -----------------------------------------

#[cfg(all(feature = "async", not(target_arch = "wasm32")))]
mod async_io;
#[cfg(all(not(feature = "async"), not(target_arch = "wasm32")))]
mod blocking;
/// The wasm32 backend lives beside the native ones in the runtime module and is
/// pulled in here so it inherits this module's contracts, catalog, ownership
/// list, and resource metadata.
#[cfg(target_arch = "wasm32")]
#[path = "../io_wasm.rs"]
mod wasm;

#[cfg(all(feature = "async", not(target_arch = "wasm32")))]
pub(crate) use async_io::*;
#[cfg(all(not(feature = "async"), not(target_arch = "wasm32")))]
pub(crate) use blocking::*;
#[cfg(target_arch = "wasm32")]
pub(crate) use wasm::*;

// ---- guest contracts -------------------------------------------------------

use crate::host_api::{
    HostFunctionSchema, HostParamPassing, HostParamSchema, HostStructField, HostTypeSchema,
    ResourceTypeKey,
};

/// The `io.file` resource key, derived from its single canonical declaration.
///
/// The guest contracts below name the key through this helper, so a contract
/// cannot drift from the resource type it refers to.
fn io_file_key() -> ResourceTypeKey {
    io_file_resource().schema.key
}

/// Guest contract for `io::open`.
///
/// The runtime signature carries the raw scope-token `i64` for the opened
/// handle; the guest contract is the typed `io.file` resource it denotes.
fn io_open_contract() -> HostFunctionSchema {
    HostFunctionSchema::with_return(
        "io::open",
        vec![
            HostParamSchema::value("path", HostTypeSchema::String),
            HostParamSchema::value("mode", HostTypeSchema::String),
        ],
        HostTypeSchema::Resource(io_file_key()),
    )
}

/// Guest contract for `io::read_all`.
fn io_read_all_contract() -> HostFunctionSchema {
    HostFunctionSchema::with_return(
        "io::read_all",
        vec![HostParamSchema::with_passing(
            "handle",
            HostTypeSchema::Resource(io_file_key()),
            HostParamPassing::Borrow,
        )],
        HostTypeSchema::String,
    )
}

/// Guest contract for `io::close`.
fn io_close_contract() -> HostFunctionSchema {
    HostFunctionSchema::with_return(
        "io::close",
        vec![HostParamSchema::with_passing(
            "handle",
            HostTypeSchema::Resource(io_file_key()),
            HostParamPassing::TakeOwned,
        )],
        HostTypeSchema::Bool,
    )
}

/// Guest contract for `io::write`.
///
/// The runtime signature still carries the raw scope-token `i64`. The guest
/// contract borrows the typed `io.file` resource, matching `io::read_all`.
fn io_write_contract() -> HostFunctionSchema {
    HostFunctionSchema::with_return(
        "io::write",
        vec![
            HostParamSchema::with_passing(
                "handle",
                HostTypeSchema::Resource(io_file_key()),
                HostParamPassing::Borrow,
            ),
            HostParamSchema::value("text", HostTypeSchema::String),
        ],
        HostTypeSchema::Int,
    )
}

/// Guest contract for `io::flush`.
fn io_flush_contract() -> HostFunctionSchema {
    HostFunctionSchema::with_return(
        "io::flush",
        vec![HostParamSchema::with_passing(
            "handle",
            HostTypeSchema::Resource(io_file_key()),
            HostParamPassing::Borrow,
        )],
        HostTypeSchema::Bool,
    )
}

/// The functions this module publishes as the `io` guest catalog surface.
///
/// The surface includes the `io.file` resource operations and the typed,
/// resource-free whole-file writer. Remaining `io::*` members stay on the
/// namespaced-builtin path.
const IO_CATALOG_FUNCTIONS: &[fn() -> crate::host_extension::HostFunctionDescriptor] = &[
    builtin_io_open_descriptor,
    builtin_io_read_all_descriptor,
    builtin_io_write_descriptor,
    builtin_io_try_write_text_descriptor,
    builtin_io_flush_descriptor,
    builtin_io_close_descriptor,
];

fn io_catalog_module() -> crate::host_extension::HostModuleDescriptor {
    super::host_modules::catalog_module("io", IO_CATALOG_FUNCTIONS, &[io_file_resource])
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::{decode_read_all, read_all_probe_bound};

    #[test]
    fn read_all_probe_bound_saturates_instead_of_overflowing() {
        assert_eq!(read_all_probe_bound(0), Some(1));
        assert_eq!(
            read_all_probe_bound(usize::MAX - 1),
            Some(usize::MAX as u64)
        );
        assert_eq!(read_all_probe_bound(usize::MAX), None);
    }

    #[test]
    fn decode_read_all_checks_the_limit_before_decoding() {
        // The probe ends one byte past the limit and mid-way through invalid
        // UTF-8: the limit must win over the decode error.
        let error = decode_read_all(vec![0xFF; 5], Some(4))
            .expect_err("the probe over the limit must be rejected");
        assert!(
            error
                .to_string()
                .contains("io_read_all exceeded read limit"),
            "unexpected error: {error}"
        );

        // Invalid UTF-8 within the limit keeps the published decode message.
        let error =
            decode_read_all(vec![0xFF, 0xFE], Some(4)).expect_err("invalid UTF-8 must be rejected");
        assert!(
            error
                .to_string()
                .contains("io_read_all failed: stream did not contain valid UTF-8"),
            "unexpected error: {error}"
        );

        // A valid payload exactly at the limit decodes; no limit reads all.
        assert_eq!(
            decode_read_all("éé".as_bytes().to_vec(), Some(4)).expect("exact limit decodes"),
            "éé"
        );
        assert_eq!(
            decode_read_all("payload".as_bytes().to_vec(), None).expect("no limit decodes"),
            "payload"
        );
    }
}

/// The standard `io` host module: the catalog surface plus every owned
/// function of the selected backend.
pub(super) fn io_host_module() -> super::host_modules::StandardHostModule {
    use super::host_modules::StandardHostModule;

    const OWNED: &[fn() -> crate::host_extension::HostFunctionDescriptor] = &[
        builtin_io_open_descriptor,
        builtin_io_popen_descriptor,
        builtin_io_read_all_descriptor,
        builtin_io_read_line_descriptor,
        builtin_io_write_descriptor,
        builtin_io_try_write_text_descriptor,
        builtin_io_flush_descriptor,
        builtin_io_close_descriptor,
        builtin_io_exists_descriptor,
    ];

    StandardHostModule {
        name: "io",
        catalog: io_catalog_module,
        owned: OWNED,
        named_structs: IO_NAMED_STRUCTS,
    }
}
