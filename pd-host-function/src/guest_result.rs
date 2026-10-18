//! Expansion for the explicit guest union, using the shared syntax tree.
use pd_host_schema::{GuestTypeShape, ResourceMode};
use proc_macro2::TokenStream;
use quote::quote;
use syn::{Error, Type};

pub(super) fn shape(ty: &Type) -> Result<Option<GuestTypeShape<'_>>, Error> {
    pd_host_schema::guest_result_shape(ty).map_err(|message| Error::new_spanned(ty, message))
}

pub(super) fn uses_vm(item: &syn::ItemFn) -> bool {
    item.sig.inputs.iter().any(|arg| match arg {
        syn::FnArg::Typed(arg) => shape(&arg.ty)
            .ok()
            .flatten()
            .is_some_and(|s| !s.resources().is_empty()),
        _ => false,
    }) || match &item.sig.output {
        syn::ReturnType::Type(_, ty) => shape(super::unwrap_transparent_return(ty))
            .ok()
            .flatten()
            .is_some_and(|s| !s.resources().is_empty()),
        _ => false,
    }
}

pub(super) fn schema(shape: &GuestTypeShape<'_>) -> Result<TokenStream, Error> {
    let sdk = super::sdk_path();
    Ok(match shape {
        GuestTypeShape::Result(ok, err) => {
            let ok = schema(ok)?;
            let err = schema(err)?;
            quote!(#sdk::HostTypeSchema::Result { ok: Box::new(#ok), err: Box::new(#err) })
        }
        GuestTypeShape::Optional(inner) => {
            let inner = schema(inner)?;
            quote!(#sdk::HostTypeSchema::Optional(Box::new(#inner)))
        }
        GuestTypeShape::Array(inner) => {
            let inner = schema(inner)?;
            quote!(#sdk::HostTypeSchema::Array(Box::new(#inner)))
        }
        GuestTypeShape::Resource(spec) => {
            let key = super::resource_key_tokens(spec);
            quote!(#sdk::HostTypeSchema::Resource(#key))
        }
        GuestTypeShape::Scalar(label) => {
            super::host_type_schema_from_label(label, &syn::parse_quote!(()))?
        }
        GuestTypeShape::Named(ty) => quote!(<#ty as #sdk::HostNamedStruct>::host_type_schema()),
    })
}

pub(super) fn decode(shape: &GuestTypeShape<'_>, value: TokenStream) -> TokenStream {
    let sdk = super::sdk_path();
    match shape {
        GuestTypeShape::Result(ok, err) => {
            let ok = decode(ok, quote!(__pd_payload));
            let err = decode(err, quote!(__pd_payload));
            quote!({ let (__pd_tag, __pd_payload) = #sdk::result::payload(#value)?;
                if __pd_tag { #sdk::GuestResult::Ok(#ok) } else { #sdk::GuestResult::Err(#err) } })
        }
        GuestTypeShape::Optional(inner) => {
            let inner = decode(inner, quote!(__pd_optional));
            quote!({ let __pd_optional = #value; if matches!(__pd_optional, #sdk::Value::Null) { None } else { Some(#inner) } })
        }
        GuestTypeShape::Array(inner) => {
            let inner = decode(inner, quote!(__pd_element));
            quote!({ let #sdk::Value::Array(__pd_array) = #value else { return Err(#sdk::VmError::TypeMismatch("array Result payload")); };
                let mut __pd_decoded = Vec::with_capacity(__pd_array.len());
                for __pd_element in __pd_array.iter() { __pd_decoded.push(#inner); } __pd_decoded })
        }
        GuestTypeShape::Resource(spec) => {
            let inner = &spec.inner;
            let key = super::resource_key_tokens(spec);
            let method = match spec.mode {
                ResourceMode::Borrow => quote!(borrow_resource_with_key),
                ResourceMode::BorrowMut => quote!(borrow_resource_mut_with_key),
                _ => quote!(take_resource_with_key),
            };
            let extracted = quote!(__pd_guest_context.#method::<#inner>(#sdk::result::resource_handle(#value)?, &#key).map_err(|error| #sdk::VmError::HostError(error.to_string()))?);
            if spec.mode == ResourceMode::TakeOwned {
                quote!(#sdk::ResourceOwned::new(#extracted))
            } else {
                extracted
            }
        }
        _ => quote!(#sdk::FromVmValue::from_vm_value(#value, "Result payload")?),
    }
}

pub(super) fn encode(
    shape: &GuestTypeShape<'_>,
    value: TokenStream,
    root_schema: &TokenStream,
) -> TokenStream {
    fn walk(
        shape: &GuestTypeShape<'_>,
        value: TokenStream,
        root: &TokenStream,
        wrap: &dyn Fn(TokenStream) -> TokenStream,
    ) -> TokenStream {
        let sdk = super::sdk_path();
        match shape {
            GuestTypeShape::Result(ok, err) => {
                let ok = walk(ok, quote!(__pd_selected), root, &|v| {
                    wrap(quote!(#sdk::result::carrier(true, #v)))
                });
                let err = walk(err, quote!(__pd_selected), root, &|v| {
                    wrap(quote!(#sdk::result::carrier(false, #v)))
                });
                quote!(match #value { #sdk::GuestResult::Ok(__pd_selected) => #ok, #sdk::GuestResult::Err(__pd_selected) => #err })
            }
            GuestTypeShape::Optional(inner) => {
                let some = walk(inner, quote!(__pd_selected), root, wrap);
                let none = wrap(quote!(#sdk::Value::Null));
                quote!(match #value { Some(__pd_selected) => #some, None => #none })
            }
            GuestTypeShape::Array(inner) => {
                let element = walk(inner, quote!(__pd_element), root, &|v| v);
                wrap(
                    quote!(#sdk::Value::array(#value.into_iter().map(|__pd_element| #element).collect())),
                )
            }
            GuestTypeShape::Resource(spec) => {
                let inner = &spec.inner;
                let wrapped = wrap(quote!(__pd_token));
                quote!(#sdk::result::publish_owned::<#inner, _>(vm, #value, &#root, |__pd_token| Ok(#wrapped))?)
            }
            _ => wrap(quote!(#sdk::IntoVmValue::into_vm_value(#value))),
        }
    }
    walk(shape, value, root_schema, &|v| v)
}
