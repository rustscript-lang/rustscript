#![cfg(feature = "runtime")]

#[cfg(feature = "async")]
#[path = "support/async_test_bridge.rs"]
mod async_test_bridge;

#[cfg(not(feature = "async"))]
#[path = "builtins/io_builtin_edge_tests.rs"]
mod io_builtin_edge_tests;

#[cfg(all(not(feature = "async"), not(target_arch = "wasm32")))]
#[path = "builtins/io_scope_lifecycle_tests.rs"]
mod io_scope_lifecycle_tests;

#[cfg(feature = "async")]
#[path = "builtins/io_async_tests.rs"]
mod io_async_tests;

#[cfg(feature = "sqlite")]
#[path = "builtins/sqlite_scope_lifecycle_tests.rs"]
mod sqlite_scope_lifecycle_tests;

#[path = "builtins/stdlib_tests.rs"]
mod stdlib_tests;
