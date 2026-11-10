use vm::{GuestResult, Store, Vm, VmResult, VmStatus};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let compiled = vm::compile_source(
        "pub fn echo(value: Result<int, string>) -> Result<int, string> { value }",
    )?;
    let mut store = Store::new(
        Vm::new(compiled.program.with_local_count(compiled.locals)),
        (),
    );
    store.vm_mut().set_fuel(100_000);
    assert_eq!(store.run()?, VmStatus::Halted);

    let callback = store.script_callback_by_name::<
        (GuestResult<i64, String>,),
        GuestResult<i64, String>,
    >("echo")?;

    let ok: Result<i64, String> = callback.call(&mut store, (GuestResult::Ok(0),))?.into();
    assert_eq!(ok, Ok(0));

    let err: Result<i64, String> = callback
        .call(&mut store, (GuestResult::Err(String::new()),))?
        .into();
    assert_eq!(err, Err(String::new()));

    let native: VmResult<Result<i64, String>> = callback
        .call(&mut store, (GuestResult::Ok(7),))
        .map(Into::into);
    assert_eq!(native?, Ok(7));

    let guest_error: VmResult<Result<i64, String>> = callback
        .call(&mut store, (GuestResult::Err("guest failure".into()),))
        .map(Into::into);
    assert_eq!(guest_error?, Err("guest failure".to_string()));

    Ok(())
}
