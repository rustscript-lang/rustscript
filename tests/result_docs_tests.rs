#![cfg(feature = "runtime")]

use vm::Value;

#[test]
fn product_result_rss_examples_compile_roundtrip_and_execute() {
    let docs = include_str!("../docs/result.md");
    let mut count = 0;
    for block in docs.split("```rss\n").skip(1) {
        let source = block.split("```").next().unwrap();
        let expected: i64 = source
            .lines()
            .next()
            .unwrap()
            .strip_prefix("// expected: ")
            .unwrap()
            .parse()
            .unwrap();
        let compiled = vm::compile_source(source).unwrap();
        let bytes =
            vm::encode_program(&compiled.program.with_local_count(compiled.locals)).unwrap();
        let program = vm::decode_program(&bytes).unwrap();
        let mut machine = vm::Vm::new_with_jit_config(
            program,
            vm::JitConfig {
                enabled: false,
                hot_loop_threshold: 1,
                max_trace_len: 512,
            },
        );
        machine.set_fuel(100_000);
        assert_eq!(machine.run().unwrap(), vm::VmStatus::Halted);
        assert_eq!(machine.stack().last(), Some(&vm::Value::Int(expected)));
        count += 1;
    }
    assert_eq!(count, 2);
}

mod rust_example {
    pub use vm::borrow_arg;
    pub mod functions {
        include!("../docs/result_host_example.rs");

        #[test]
        fn documented_rust_host_example_is_exact_and_compiled() {
            let docs = include_str!("../docs/result.md");
            assert!(docs.contains(include_str!("../docs/result_host_example.rs").trim()));
            let descriptor = read_descriptor();
            assert_eq!(descriptor.schema.name, "example::read");
            assert!(matches!(
                descriptor.schema.return_type,
                vm::HostTypeSchema::Result { .. }
            ));
            let catalog = std::sync::Arc::new(
                vm::HostFunctionDescriptor::collect_catalog(&[descriptor]).unwrap(),
            );
            for mode in [0, 1, 2] {
                let descriptor = read_descriptor();
                let compiled = vm::compile_source_with_flavor_and_options(
                    &format!("example::read({mode});"),
                    vm::SourceFlavor::RustScript,
                    vm::CompileSourceFileOptions::new().with_host_api_catalog(catalog.clone()),
                )
                .unwrap();
                let mut machine = vm::Vm::new(compiled.program.with_local_count(compiled.locals));
                let mut registry = vm::HostFunctionRegistry::empty();
                vm::HostModuleDescriptor::install_descriptors(&mut registry, &[descriptor])
                    .unwrap();
                registry.bind_vm_cached(&mut machine).unwrap();
                machine.set_fuel(100_000);
                if mode == 2 {
                    assert!(
                        machine
                            .run()
                            .unwrap_err()
                            .to_string()
                            .contains("transport failed")
                    );
                } else {
                    assert_eq!(machine.run().unwrap(), vm::VmStatus::Halted);
                    let result = <GuestResult<i64, String> as vm::ScriptResult>::from_value(
                        machine.stack().last().unwrap().clone(),
                    )
                    .unwrap();
                    assert_eq!(
                        result,
                        if mode == 0 {
                            GuestResult::Ok(7)
                        } else {
                            GuestResult::Err("missing".into())
                        }
                    );
                }
            }
        }
    }
}
