#![cfg(feature = "runtime")]

#[path = "jit/jit_nyi_edge_tests.rs"]
mod jit_nyi_edge_tests;

#[path = "jit/jit_tests.rs"]
mod jit_tests;

#[path = "jit/perf_tests.rs"]
mod perf_tests;
