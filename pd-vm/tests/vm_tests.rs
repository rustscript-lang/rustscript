#![cfg(feature = "runtime")]

#[path = "vm/drop_contract_tests.rs"]
mod drop_contract_tests;

#[path = "vm/functional_parity_tests.rs"]
mod functional_parity_tests;

#[path = "vm/runtime_state_edge_tests.rs"]
mod runtime_state_edge_tests;

#[path = "vm/vm_async_runtime_tests.rs"]
mod vm_async_runtime_tests;

#[path = "vm/vm_runtime_tests.rs"]
mod vm_runtime_tests;
