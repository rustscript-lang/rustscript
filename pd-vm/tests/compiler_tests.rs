#[cfg(feature = "runtime")]
#[path = "compiler/compiler_common_tests.rs"]
mod compiler_common_tests;

#[cfg(feature = "runtime")]
#[path = "compiler/compiler_javascript_tests.rs"]
mod compiler_javascript_tests;

#[cfg(feature = "runtime")]
#[path = "compiler/compiler_lua_tests.rs"]
mod compiler_lua_tests;

#[path = "compiler/diagnostics_tests.rs"]
mod diagnostics_tests;

#[cfg(feature = "runtime")]
#[path = "compiler/module_import_tests.rs"]
mod module_import_tests;

#[cfg(feature = "runtime")]
#[path = "compiler/compiler_rustscript_tests.rs"]
mod compiler_rustscript_tests;

#[cfg(feature = "runtime")]
#[path = "compiler/compiler_scheme_tests.rs"]
mod compiler_scheme_tests;

#[cfg(feature = "runtime")]
#[path = "compiler/type_inference_tests.rs"]
mod type_inference_tests;

#[cfg(feature = "runtime")]
#[path = "compiler/whitespace_resilience_tests.rs"]
mod whitespace_resilience_tests;
