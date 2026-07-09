#[path = "../common/mod.rs"]
mod common;
use common::*;

fn run_program(source: &str, flavor: SourceFlavor) -> Vec<Value> {
    let compiled = compile_source_with_flavor(source, flavor).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    vm.stack().to_vec()
}

#[test]
fn rustscript_spacing_variants_produce_same_result() {
    let compact = r#"
        let a=1;
        let b=2;
        let c=if a<b=>{a+b}else=>{0};
        let mut arr=[];
        arr[arr.length]=c;
        let m={value:arr[0]};
        m.value+match c{3=>1,_=>0};
    "#;
    let spaced = r#"
        let        a = 1 ;
        let    b    =    2 ;
        let c = if    a < b    => { a + b } else   => { 0 } ;
        let mut arr = [ ] ;
        arr [ arr . length ] = c ;
        let m = { value : arr [ 0 ] } ;
        m . value + match c { 3 => 1 , _ => 0 , } ;
    "#;

    assert_eq!(
        run_program(compact, SourceFlavor::RustScript),
        run_program(spaced, SourceFlavor::RustScript),
    );
}
