use std::path::Path;

use vm::{
    CompileSourceFileOptions, Value, Vm, VmStatus, compile_source_file_with_options,
    standard_composition, standard_host_catalog,
};

fn run_rustscript_spec(path: &Path) -> Vec<Value> {
    let catalog = standard_host_catalog();
    let options = CompileSourceFileOptions::default().with_host_api_catalog(catalog.clone());
    let compiled = compile_source_file_with_options(path, options).expect("spec should compile");
    assert!(
        compiled
            .program
            .imports
            .iter()
            .filter_map(|import| import.schema.as_ref())
            .all(|schema| schema.fingerprint == catalog.fingerprint()),
        "stdlib host imports must use the exact standard catalog fingerprint"
    );

    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    vm.set_standard_composition(standard_composition());
    #[cfg(feature = "async")]
    super::async_test_bridge::install(&mut vm);
    loop {
        let status = vm.run().expect("spec vm should run");
        match status {
            VmStatus::Halted => break,
            VmStatus::Yielded => continue,
            VmStatus::Waiting(_op_id) => vm
                .wait_for_host_op_blocking()
                .expect("spec vm should complete builtin async op"),
        }
    }
    vm.stack().to_vec()
}

#[test]
fn stdlib_strings_spec_passes() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let path = root.join("stdlib/tests/strings.rss");
    let stack = run_rustscript_spec(&path);
    assert_eq!(stack, Vec::<Value>::new());
}

#[test]
fn stdlib_bytes_spec_passes() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let path = root.join("stdlib/tests/bytes.rss");
    let stack = run_rustscript_spec(&path);
    assert_eq!(stack, Vec::<Value>::new());
}

#[test]
fn stdlib_io_primitives_spec_passes() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let path = root.join("stdlib/tests/io_primitives.rss");
    let stack = run_rustscript_spec(&path);
    assert_eq!(stack, Vec::<Value>::new());
}

#[test]
fn stdlib_collections_primitives_spec_passes() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let path = root.join("stdlib/tests/collections_primitives.rss");
    let stack = run_rustscript_spec(&path);
    assert_eq!(stack, Vec::<Value>::new());
}

#[test]
fn stdlib_collections_spec_passes() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let path = root.join("stdlib/tests/collections.rss");
    let stack = run_rustscript_spec(&path);
    assert_eq!(stack, Vec::<Value>::new());
}

#[test]
fn stdlib_iter_spec_passes() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let path = root.join("stdlib/tests/iter.rss");
    let stack = run_rustscript_spec(&path);
    assert_eq!(stack, Vec::<Value>::new());
}

#[test]
fn stdlib_io_spec_passes() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let path = root.join("stdlib/tests/io.rss");
    let stack = run_rustscript_spec(&path);
    assert_eq!(stack, Vec::<Value>::new());
}

#[test]
fn stdlib_path_spec_passes() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let path = root.join("stdlib/tests/path.rss");
    let stack = run_rustscript_spec(&path);
    assert_eq!(stack, Vec::<Value>::new());
}

#[test]
fn stdlib_math_spec_passes() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let path = root.join("stdlib/tests/math.rss");
    let stack = run_rustscript_spec(&path);
    assert_eq!(stack, Vec::<Value>::new());
}

#[test]
fn stdlib_parse_spec_passes() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let path = root.join("stdlib/tests/parse.rss");
    let stack = run_rustscript_spec(&path);
    assert_eq!(stack, Vec::<Value>::new());
}

#[test]
fn stdlib_re_spec_passes() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let path = root.join("stdlib/tests/re.rss");
    let stack = run_rustscript_spec(&path);
    assert_eq!(stack, Vec::<Value>::new());
}

#[test]
fn stdlib_lrucache_spec_passes() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let path = root.join("stdlib/tests/lrucache.rss");
    let stack = run_rustscript_spec(&path);
    assert_eq!(stack, Vec::<Value>::new());
}
