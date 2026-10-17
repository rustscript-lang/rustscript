use vm::GuestResult;

#[test]
fn public_wrapper_is_available_without_runtime() {
    let ok: GuestResult<i64, String> = std::result::Result::Ok(7).into();
    assert_eq!(ok, GuestResult::Ok(7));
    let err: std::result::Result<i64, &str> = GuestResult::Err("guest error").into();
    assert_eq!(err, Err("guest error"));
}
