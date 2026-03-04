#![cfg(feature = "runtime")]
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
        let arr=[];
        arr[arr.length]=c;
        let m={value:arr[0]};
        m.value+match c{3=>1,_=>0};
    "#;
    let spaced = r#"
        let        a = 1 ;
        let    b    =    2 ;
        let c = if    a < b    => { a + b } else   => { 0 } ;
        let arr = [ ] ;
        arr [ arr . length ] = c ;
        let m = { value : arr [ 0 ] } ;
        m . value + match c { 3 => 1 , _ => 0 , } ;
    "#;

    assert_eq!(
        run_program(compact, SourceFlavor::RustScript),
        run_program(spaced, SourceFlavor::RustScript),
    );
}

#[test]
fn javascript_spacing_variants_produce_same_result() {
    let compact = r#"
        const a=1;
        let b=2;
        if(a<b){b=b+a;}else{b=0;}
        let add=(v)=>v+b;
        add(3);
    "#;
    let spaced = r#"
        const      a = 1 ;
        let   b = 2 ;
        if ( a < b ) { b = b + a ; } else { b = 0 ; }
        let add = ( v ) => v + b ;
        add ( 3 ) ;
    "#;

    assert_eq!(
        run_program(compact, SourceFlavor::JavaScript),
        run_program(spaced, SourceFlavor::JavaScript),
    );
}

#[test]
fn lua_spacing_variants_produce_same_result() {
    let compact = r#"
        local a=1
        local b=2
        if a<b then
            b=b+a
        else
            b=0
        end
        b
    "#;
    let spaced = r#"
        local      a = 1
        local   b = 2
        if    a < b    then
            b = b + a
        else
            b = 0
        end
        b
    "#;

    assert_eq!(
        run_program(compact, SourceFlavor::Lua),
        run_program(spaced, SourceFlavor::Lua),
    );
}

#[test]
fn scheme_spacing_variants_produce_same_result() {
    let compact = r#"
        (define a 1)
        (define b 2)
        (if (< a b)
            (begin
                (set! b (+ b a))
                b)
            0)
    "#;
    let spaced = r#"
        ( define   a   1 )
        ( define b 2 )
        ( if ( < a b )
            ( begin
                ( set!   b ( + b a ) )
                b )
            0 )
    "#;

    assert_eq!(
        run_program(compact, SourceFlavor::Scheme),
        run_program(spaced, SourceFlavor::Scheme),
    );
}
