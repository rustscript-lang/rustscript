#![cfg(feature = "runtime")]

#[cfg(feature = "async")]
#[path = "support/async_test_bridge.rs"]
mod async_test_bridge;

#[cfg(not(feature = "async"))]
#[path = "builtins/io_builtin_edge_tests.rs"]
mod io_builtin_edge_tests;

#[cfg(feature = "async")]
#[path = "builtins/io_async_tests.rs"]
mod io_async_tests;

#[path = "builtins/bounded_process_tests.rs"]
mod bounded_process_tests;

#[path = "builtins/stdlib_tests.rs"]
mod stdlib_tests;
