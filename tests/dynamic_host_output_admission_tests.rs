#![cfg(feature = "runtime")]

fn dynamic_output_catalog() -> std::sync::Arc<vm::HostApiCatalog> {
    let mut builder = vm::HostApiBuilder::new();
    builder.function(vm::HostFunctionSchema::with_return(
        "demo::missing",
        Vec::new(),
        vm::HostTypeSchema::Null,
    ));
    builder.function(vm::HostFunctionSchema::with_return(
        "demo::flag",
        Vec::new(),
        vm::HostTypeSchema::Bool,
    ));
    std::sync::Arc::new(builder.build().expect("dynamic output catalog"))
}

fn reject_nested_dynamic_output(source: &str) {
    let rejected = vm::compile_source_with_flavor_and_options(
        source,
        vm::SourceFlavor::RustScript,
        vm::CompileSourceFileOptions::default().with_host_api_catalog(dynamic_output_catalog()),
    );
    let message = match rejected {
        Ok(_) => {
            panic!("dynamic host output must be rejected in every structured position: {source}")
        }
        Err(error) => error.to_string(),
    };
    assert!(
        message.contains("dynamic or legacy-null"),
        "expected the host-output admission diagnostic for {source}, got {message}"
    );
}

#[test]
fn rss_rejects_dynamic_host_output_inside_match_arm() {
    reject_nested_dynamic_output(
        "use demo;\nlet value = match 1 { 1 => if true => { demo::missing(); 1 } else => { 2 }, _ => 2 };\n1;",
    );
}

#[test]
fn rss_rejects_dynamic_host_output_inside_option_payload() {
    reject_nested_dynamic_output(
        "use demo;\nlet value = Option::Some(if true => { demo::missing(); 1 } else => { 2 });\nvalue;",
    );
}

#[test]
fn rss_rejects_dynamic_host_output_inside_result_arm() {
    reject_nested_dynamic_output(
        "use demo;\nlet seed: Result<int, int> = Result::Ok(1);\nlet value = match seed { Result::Ok(v) => if true => { demo::missing(); v } else => { 0 }, Result::Err(e) => 0 };\nvalue;",
    );
}

#[test]
fn rss_rejects_dynamic_host_output_inside_method_callback_body() {
    reject_nested_dynamic_output(
        "use demo;\nlet value = Option::Some(1).map(|item| if true => { demo::missing(); item } else => { 0 });\nvalue;",
    );
}
