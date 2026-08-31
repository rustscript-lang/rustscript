#![cfg(feature = "runtime")]

#[path = "builtins/io_builtin_edge_tests.rs"]
mod io_builtin_edge_tests;

#[path = "builtins/io_scope_lifecycle_tests.rs"]
mod io_scope_lifecycle_tests;

#[cfg(feature = "sqlite")]
#[path = "builtins/sqlite_scope_lifecycle_tests.rs"]
mod sqlite_scope_lifecycle_tests;

#[path = "builtins/stdlib_tests.rs"]
mod stdlib_tests;
