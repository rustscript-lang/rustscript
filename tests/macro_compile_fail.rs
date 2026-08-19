//! Compile-fail diagnostics for `#[pd_host_function]` resource usage.
//!
//! These exercises prove the proc macro rejects misuse at *expansion* time —
//! invalid resource keys, generic host functions, borrowed resource returns,
//! and alias-shaped annotated paths — so the failures are clear compile
//! errors instead of runtime panics. The `.stderr` files record the exact
//! diagnostic (regenerate with `TRYBUILD=overwrite` after an intentional
//! message change).

#[test]
fn pd_host_function_resource_diagnostics_fail_to_compile() {
    let cases = trybuild::TestCases::new();
    cases.compile_fail("tests/ui/*.rs");
}
