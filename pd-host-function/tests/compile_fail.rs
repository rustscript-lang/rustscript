#[test]
fn invalid_resource_signatures_emit_macro_diagnostics() {
    let cases = trybuild::TestCases::new();
    cases.compile_fail("tests/ui/fail/*.rs");
}

#[test]
fn valid_resource_signatures_compile_against_public_vm_api() {
    let cases = trybuild::TestCases::new();
    cases.pass("tests/ui/pass/*.rs");
}
