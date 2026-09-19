#![cfg(all(feature = "sqlite", not(target_arch = "wasm32")))]
//! Architecture guard for the macro-owned async SQLite host adapter.

const SQLITE_SOURCE: &str = include_str!("../src/builtins/runtime/sqlite.rs");

#[test]
fn sqlite_host_functions_are_macro_owned_async_functions() {
    for function in ["open", "execute", "query", "transaction", "close"] {
        let signature = format!("async fn builtin_sqlite_{function}_impl");
        assert!(
            SQLITE_SOURCE.contains(&signature),
            "sqlite::{function} must be an ordinary async #[pd_host_function]"
        );
    }

    assert!(
        SQLITE_SOURCE.contains("CaptureAsyncHostContext"),
        "SQLite calls must capture owned VM context before async submission"
    );
    assert!(
        SQLITE_SOURCE.contains("tokio_rusqlite::Connection"),
        "the SQLite resource must use the maintained Tokio-facing adapter"
    );

    assert_eq!(
        SQLITE_SOURCE.matches("HostFutureOutput::complete").count(),
        2,
        "only open insertion and close removal may require terminal VM completion"
    );
}

#[test]
fn sqlite_host_owns_no_threads_or_custom_operation_driver() {
    let implementation = SQLITE_SOURCE
        .split_once("\n#[cfg(test)]\nmod tests")
        .map_or(SQLITE_SOURCE, |(implementation, _tests)| implementation);
    for forbidden in [
        "std::thread",
        "thread::",
        "JoinHandle",
        "std::sync::mpsc",
        "crossbeam_channel",
        "AtomicWaker",
        "RawWaker",
        "HostOperation",
        "OperationSpec",
        "OperationOutcome",
        "OperationCancelReason",
        "SqliteWorker",
        "SqliteOpDriver",
        "SqliteOpShared",
        "schedule_operation",
        "register_scoped_operation_completion",
        "completion mailbox",
        "close_waker",
        "quiescence_waker",
    ] {
        assert!(
            !implementation.contains(forbidden),
            "SQLite host source must not contain custom scheduling token `{forbidden}`"
        );
    }
}
