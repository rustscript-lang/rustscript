use pd_host_schema::{
    ResourceMode, borrowed_resource_return, guest_result_arguments, guest_type_shape, type_label,
};
use syn::{Type, parse_quote};
#[test]
fn exact_guest_union_arity_and_transport_identity() {
    for ty in [
        parse_quote!(GuestResult),
        parse_quote!(GuestResult<i64>),
        parse_quote!(GuestResult<i64, String, bool>),
        parse_quote!(GuestResult<'static, i64>),
    ] {
        assert!(guest_result_arguments(&ty).is_err());
    }
    for ty in [
        parse_quote!(Result<i64, String>),
        parse_quote!(VmResult<i64>),
    ] {
        assert!(guest_result_arguments(&ty).unwrap().is_none());
    }
    assert_eq!(
        type_label(&parse_quote!(VmResult<GuestResult<i64, String>>)).unwrap(),
        "Result<int, string>"
    );
}
#[test]
fn both_resource_branches_are_observed_in_declaration_order() {
    let ty: Type =
        parse_quote!(GuestResult<ResourceOwned<Success>, Option<ResourceOwned<Failure>>>);
    let shape = guest_type_shape(&ty).unwrap();
    assert_eq!(shape.resources().len(), 2);
    assert_eq!(
        shape.resource_mode().unwrap(),
        Some(ResourceMode::TakeOwned)
    );
    assert_eq!(shape.label(), "Result<resource, resource | null>");
    assert!(
        guest_type_shape(&parse_quote!(
            GuestResult<ResourceOwned<Success>, ResourceRef<'static, Failure>>
        ))
        .unwrap()
        .resource_mode()
        .is_err()
    );
}
#[test]
fn borrowed_return_visitor_checks_both_branches_and_containers() {
    for ty in [
        parse_quote!(GuestResult<ResourceRef<'static, R>, i64>),
        parse_quote!(GuestResult<i64, Option<Vec<ResourceMut<'static, R>>>>),
        parse_quote!(GuestResult<Packet<ResourceRef<'static, R>>, i64>),
    ] {
        assert!(borrowed_resource_return(&ty).is_some());
    }
}
#[test]
fn typed_payloads_reject_dynamic_and_nested_transport() {
    for ty in [
        parse_quote!(GuestResult<Value, i64>),
        parse_quote!(GuestResult<i64, VmMap>),
        parse_quote!(GuestResult<i64, Result<String, String>>),
    ] {
        assert!(guest_type_shape(&ty).is_err());
    }
}
