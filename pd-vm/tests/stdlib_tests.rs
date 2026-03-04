#![cfg(feature = "runtime")]
use std::path::Path;

use vm::{Value, Vm, VmStatus, compile_source_file};

fn run_rustscript_spec(path: &Path) -> Vec<Value> {
    let compiled = compile_source_file(path).expect("spec should compile");
    assert!(
        compiled.functions.is_empty(),
        "stdlib RustScript specs should not require host imports"
    );
    assert!(
        compiled.program.imports.is_empty(),
        "stdlib RustScript specs should not emit host imports for builtins"
    );

    let mut vm = Vm::new(compiled.program);
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
fn stdlib_re_spec_passes() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let path = root.join("stdlib/tests/re.rss");
    let stack = run_rustscript_spec(&path);
    assert_eq!(stack, Vec::<Value>::new());
}
