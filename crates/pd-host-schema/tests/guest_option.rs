use pd_host_schema::{
    GuestTypeShape, ResourceMode, guest_result_shape, guest_type_shape, type_label,
};
use syn::parse_quote;

#[derive(Debug)]
struct FakeResource;

fn shape(ty: &syn::Type) -> GuestTypeShape<'_> {
    guest_type_shape(ty).expect("typed guest Option schema")
}

#[test]
fn guest_option_labels_are_distinct_from_legacy_nullable_option() {
    let value: syn::Type = parse_quote!(GuestOption<Vec<i64>>);
    let shape = shape(&value);
    assert!(matches!(shape, GuestTypeShape::Option(_)));
    assert_eq!(shape.label(), "Option<array<int>>");
    assert_eq!(type_label(&value).unwrap(), "Option<array<int>>");

    let nullable: syn::Type = parse_quote!(Option<i64>);
    assert_eq!(type_label(&nullable).unwrap(), "int | null");
    let (label, optional) = pd_host_schema::parameter_type_label(&nullable).unwrap();
    assert_eq!(label, "int | null");
    assert!(optional);
    let (label, optional) = pd_host_schema::parameter_type_label(&value).unwrap();
    assert_eq!(label, "Option<array<int>>");
    assert!(!optional, "a guest Option is not an omitted argument");
}

#[test]
fn guest_option_nests_unit_result_and_nullable_payloads() {
    let nested: syn::Type = parse_quote!(GuestOption<GuestOption<Option<Vec<bool>>>>);
    let nested_shape = shape(&nested);
    assert_eq!(nested_shape.label(), "Option<Option<array<bool> | null>>");
    let unit: syn::Type = parse_quote!(GuestOption<()>);
    let unit_shape = shape(&unit);
    assert!(matches!(unit_shape, GuestTypeShape::Option(_)));
    assert_eq!(unit_shape.label(), "Option<()>");
    let unit_only: syn::Type = parse_quote!(());
    assert!(matches!(shape(&unit_only), GuestTypeShape::Unit));
    assert_eq!(shape(&unit_only).label(), "()");
    assert_eq!(type_label(&unit_only).unwrap(), "()");

    let result: syn::Type = parse_quote!(GuestOption<GuestResult<i64, String>>);
    let result_shape = shape(&result);
    assert_eq!(result_shape.label(), "Option<Result<int, string>>");
    let nested_result: syn::Type = parse_quote!(GuestResult<GuestOption<i64>, String>);
    assert_eq!(shape(&nested_result).label(), "Result<Option<int>, string>");
}

#[test]
fn guest_option_resources_keep_their_access_mode() {
    let _ = FakeResource;
    let owned: syn::Type = parse_quote!(GuestOption<ResourceOwned<FakeResource>>);
    let owned_shape = shape(&owned);
    let resources = owned_shape.resources();
    assert_eq!(resources.len(), 1);
    assert_eq!(resources[0].mode, ResourceMode::TakeOwned);
    assert_eq!(
        owned_shape.resource_mode().unwrap(),
        Some(ResourceMode::TakeOwned)
    );
    assert_eq!(owned_shape.label(), "Option<resource>");

    let borrowed: syn::Type = parse_quote!(GuestOption<ResourceRef<'_, FakeResource>>);
    assert_eq!(
        shape(&borrowed).resource_mode().unwrap(),
        Some(ResourceMode::Borrow)
    );
    let mixed: syn::Type =
        parse_quote!(GuestResult<ResourceRef<'_, FakeResource>, ResourceOwned<FakeResource>>);
    assert!(
        shape(&mixed).resource_mode().is_err(),
        "mixed resource modes must remain refused"
    );
    let array: syn::Type = parse_quote!(GuestOption<Vec<ResourceOwned<FakeResource>>>);
    let error = guest_type_shape(&array).unwrap_err();
    assert!(error.contains("resource arrays"), "{error}");
}

#[test]
fn guest_option_callable_payload_uses_the_typed_callable_schema() {
    let callable: syn::Type = parse_quote!(GuestOption<VmCallable<fn(i64, String) -> bool>>);
    let option_shape = shape(&callable);
    assert_eq!(option_shape.label(), "Option<fn(int, string) -> bool>");
    let GuestTypeShape::Option(inner) = &option_shape else {
        panic!("callable Option must keep the Option wrapper");
    };
    let GuestTypeShape::Callable { params, result } = inner.as_ref() else {
        panic!("callable payload must use the Callable shape");
    };
    assert_eq!(params.len(), 2);
    assert_eq!(params[1].label(), "string");
    assert_eq!(result.label(), "bool");
    assert!(
        option_shape.resources().is_empty(),
        "a callable value owns no resource"
    );
    assert_eq!(option_shape.resource_mode().unwrap(), None);

    let unit: syn::Type = parse_quote!(GuestOption<VmCallable<fn() -> ()>>);
    assert_eq!(shape(&unit).label(), "Option<fn() -> ()>");
    // `fn()` and `fn() -> ()` are the same Rust function type: an omitted
    // return publishes logical Unit, not the low-level Null spelling.
    let defaulted: syn::Type = parse_quote!(GuestOption<VmCallable<fn()>>);
    let defaulted_shape = shape(&defaulted);
    assert_eq!(defaulted_shape.label(), "Option<fn() -> ()>");
    let GuestTypeShape::Option(inner) = &defaulted_shape else {
        panic!("bare callable Option must keep the Option wrapper");
    };
    let GuestTypeShape::Callable { params, result } = inner.as_ref() else {
        panic!("bare callable payload must use the Callable shape");
    };
    assert!(params.is_empty());
    assert!(matches!(result.as_ref(), GuestTypeShape::Unit));

    let defaulted_typed: syn::Type = parse_quote!(GuestOption<VmCallable<fn(i64)>>);
    let explicit_typed: syn::Type = parse_quote!(GuestOption<VmCallable<fn(i64) -> ()>>);
    assert_eq!(shape(&defaulted_typed).label(), "Option<fn(int) -> ()>");
    assert_eq!(
        shape(&defaulted_typed).label(),
        shape(&explicit_typed).label()
    );

    let nested: syn::Type = parse_quote!(GuestResult<GuestOption<VmCallable<fn() -> ()>>, String>);
    assert_eq!(shape(&nested).label(), "Result<Option<fn() -> ()>, string>");
    let nested_defaulted: syn::Type =
        parse_quote!(GuestResult<GuestOption<VmCallable<fn()>>, String>);
    assert_eq!(
        shape(&nested_defaulted).label(),
        "Result<Option<fn() -> ()>, string>"
    );

    let malformed: syn::Type = parse_quote!(GuestOption<VmCallable<i64>>);
    let error = guest_type_shape(&malformed).unwrap_err();
    assert!(error.contains("fn(...) ->"), "{error}");
}

#[test]
fn guest_option_shape_detection_and_arity_are_strict() {
    let optional: syn::Type = parse_quote!(GuestOption<i64>);
    assert!(
        guest_result_shape(&optional)
            .expect("valid guest Option")
            .is_some()
    );
    let plain: syn::Type = parse_quote!(i64);
    assert!(guest_result_shape(&plain).unwrap().is_none());
    let missing: syn::Type = parse_quote!(GuestOption);
    let error = guest_result_shape(&missing).unwrap_err();
    assert!(error.contains("requires one"), "{error}");
    let two: syn::Type = parse_quote!(GuestOption<i64, bool>);
    let error = guest_type_shape(&two).unwrap_err();
    assert!(error.contains("one generic argument"), "{error}");
    let transport: syn::Type = parse_quote!(GuestOption<VmResult<i64>>);
    let error = guest_type_shape(&transport).unwrap_err();
    assert!(error.contains("transport results"), "{error}");
}

#[test]
fn callable_unit_labels_use_the_unit_spelling() {
    let callable: syn::Type = parse_quote!(VmCallable<fn(i64) -> ()>);
    assert_eq!(type_label(&callable).unwrap(), "fn(int) -> ()");
    let bare: syn::Type = parse_quote!(VmCallable<fn()>);
    assert_eq!(type_label(&bare).unwrap(), "fn() -> ()");
    let optional_callable: syn::Type = parse_quote!(Option<VmCallable<fn() -> ()>>);
    assert_eq!(type_label(&optional_callable).unwrap(), "fn() -> () | null");
}
