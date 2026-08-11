#![allow(clippy::duplicate_mod)]

#[cfg(feature = "runtime")]
#[path = "compiler/compiler_common_tests.rs"]
mod compiler_common_tests;

#[path = "compiler/diagnostics_tests.rs"]
mod diagnostics_tests;

#[cfg(feature = "runtime")]
#[path = "compiler/frontend_plugin_tests.rs"]
mod frontend_plugin_tests;

#[cfg(feature = "runtime")]
#[path = "compiler/module_import_tests.rs"]
mod module_import_tests;

#[cfg(feature = "runtime")]
#[path = "compiler/compiler_rustscript_tests.rs"]
mod compiler_rustscript_tests;

#[cfg(feature = "runtime")]
#[path = "compiler/type_inference_tests.rs"]
mod type_inference_tests;

#[cfg(feature = "runtime")]
#[path = "compiler/whitespace_resilience_tests.rs"]
mod whitespace_resilience_tests;

#[cfg(feature = "runtime")]
#[path = "compiler/semantic_module_m12_tests.rs"]
mod semantic_module_m12_tests;

#[cfg(feature = "runtime")]
#[path = "compiler/semantic_module_m3_tests.rs"]
mod semantic_module_m3_tests;

#[cfg(feature = "runtime")]
#[path = "compiler/semantic_module_m4_tests.rs"]
mod semantic_module_m4_tests;

#[cfg(feature = "runtime")]
#[path = "compiler/semantic_module_m5_tests.rs"]
mod semantic_module_m5_tests;

#[cfg(feature = "runtime")]
#[path = "compiler/semantic_module_m6_tests.rs"]
mod semantic_module_m6_tests;
