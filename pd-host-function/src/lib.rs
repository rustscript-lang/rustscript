use proc_macro::TokenStream;
use quote::quote;
use syn::{
    Error, FnArg, ItemFn, LitStr, Meta, Pat, PatIdent, ReturnType, Token, Type, parse_macro_input,
    punctuated::Punctuated,
};

use pd_host_schema::ResourceMode;

#[derive(Clone)]
struct ResourceParamInfo {
    mode: ResourceMode,
    inner: Type,
    owned_wrapper: bool,
    key: Option<LitStr>,
}

#[proc_macro_attribute]
pub fn pd_host_function(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args = parse_macro_input!(attr with Punctuated::<Meta, Token![,]>::parse_terminated);
    let item = parse_macro_input!(item as ItemFn);
    let result = expand_pd_host_function(args, item);
    match result {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

fn expand_pd_host_function(
    attr: Punctuated<Meta, Token![,]>,
    mut item: ItemFn,
) -> Result<proc_macro2::TokenStream, Error> {
    parse_name_arg(&attr)?;
    let is_async = item.sig.asyncness.is_some();
    let docs = doc_string(&item.attrs);
    // The generated adapter cannot instantiate a generic `T` (there is no
    // turbofish at the wrapper call site); reject generic host functions at
    // expansion time instead of emitting a wrapper that references an
    // undeclared type parameter. Resource parameters in particular must name a
    // concrete resource type.
    if !item.sig.generics.params.is_empty() {
        return Err(Error::new_spanned(
            &item.sig.generics,
            "#[pd_host_function] does not support generic host functions; the adapter requires concrete parameter and return types",
        ));
    }
    for input in &item.sig.inputs {
        let resource = resource_param_info(input)?;
        if is_async {
            if let Some(info) = &resource {
                if !matches!(info.mode, ResourceMode::TakeOwned) {
                    return Err(Error::new_spanned(
                        input,
                        "resource borrows cannot cross async/yield; only TakeOwned may move into an owned operation",
                    ));
                }
            } else {
                validate_async_param(input)?;
            }
        } else if is_host_context_param(input) {
            return Err(Error::new_spanned(
                input,
                "#[pd_host_context] is only valid on async host functions",
            ));
        }
        if !is_host_context_param(input) && resource.is_none() {
            validate_param(input)?;
        }
    }
    validate_return_type(&item.sig.output)?;

    if is_abi_declaration_only(&item) {
        return Ok(quote!(#item));
    }
    if docs.trim().is_empty() {
        return Err(Error::new_spanned(
            &item.sig.ident,
            "#[pd_host_function] requires /// doc comments",
        ));
    }

    let (wrapper_name, impl_name) = wrapper_and_impl_names(&item.sig.ident);
    if item.sig.ident != impl_name {
        item.sig.ident = impl_name.clone();
    }
    let wrapper = if is_async {
        generate_async_vm_wrapper(&item, &wrapper_name)?
    } else {
        generate_vm_wrapper(&item, &wrapper_name)?
    };
    for input in &mut item.sig.inputs {
        if let FnArg::Typed(pat_type) = input {
            pat_type.attrs.retain(|attr| {
                !attr.path().is_ident("pd_host_context") && !is_resource_attribute(attr)
            });
        }
    }
    Ok(quote! {
        #item
        #wrapper
    })
}

/// Canonical resource-parameter parsing.
///
/// This delegates entirely to the shared `pd-host-schema` rules so the proc
/// macro and the build script can never disagree about which types are
/// resources, which passing mode they imply, and which keys are legal. The
/// macro maps shared diagnostics onto the declared type's span and keeps the
/// raw key literal (already validated) for code generation.
fn resource_param_info(arg: &FnArg) -> Result<Option<ResourceParamInfo>, Error> {
    let FnArg::Typed(pat_type) = arg else {
        return Ok(None);
    };
    let spec = pd_host_schema::resource_spec(&pat_type.ty, &pat_type.attrs)
        .map_err(|message| Error::new_spanned(&pat_type.ty, message))?;
    let Some(spec) = spec else {
        return Ok(None);
    };
    let key = spec
        .key
        .as_deref()
        .map(|key| LitStr::new(key, proc_macro2::Span::call_site()));
    Ok(Some(ResourceParamInfo {
        mode: spec.mode,
        inner: spec.inner,
        owned_wrapper: spec.owned_wrapper,
        key,
    }))
}

fn is_resource_attribute(attr: &syn::Attribute) -> bool {
    [
        "pd_host_param",
        "pd_host_resource",
        "pd_host_passing",
        "pd_borrow",
        "pd_borrow_mut",
        "pd_take_owned",
        "pd_to_owned",
        "pd_value",
    ]
    .iter()
    .any(|name| attr.path().is_ident(name))
}
fn validate_async_param(arg: &FnArg) -> Result<(), Error> {
    let FnArg::Typed(pat_type) = arg else {
        return Err(Error::new_spanned(arg, "methods are not supported"));
    };
    if is_vm_context_type(&pat_type.ty) {
        return Err(Error::new_spanned(
            &pat_type.ty,
            "async host functions cannot borrow Vm; capture owned host context before submission",
        ));
    }
    if is_host_context_param(arg) {
        return Ok(());
    }
    if !is_async_owned_type(&pat_type.ty) {
        return Err(Error::new_spanned(
            &pat_type.ty,
            "async host function parameters must be owned and 'static",
        ));
    }
    Ok(())
}

fn is_host_context_param(arg: &FnArg) -> bool {
    match arg {
        FnArg::Typed(pat_type) => pat_type
            .attrs
            .iter()
            .any(|attr| attr.path().is_ident("pd_host_context")),
        FnArg::Receiver(_) => false,
    }
}

fn is_async_owned_type(ty: &Type) -> bool {
    match ty {
        Type::Group(group) => is_async_owned_type(&group.elem),
        Type::Paren(paren) => is_async_owned_type(&paren.elem),
        Type::Reference(_) | Type::Slice(_) => false,
        Type::Tuple(tuple) => tuple.elems.iter().all(is_async_owned_type),
        Type::Path(path) => {
            let Some(segment) = path.path.segments.last() else {
                return false;
            };
            if matches!(
                segment.ident.to_string().as_str(),
                "str" | "VmStringRef" | "VmBytesRef" | "VmArrayRef" | "VmMapRef" | "VmValueRef"
            ) {
                return false;
            }
            match &segment.arguments {
                syn::PathArguments::None => true,
                syn::PathArguments::AngleBracketed(args) => args.args.iter().all(|arg| match arg {
                    syn::GenericArgument::Type(inner) => is_async_owned_type(inner),
                    _ => false,
                }),
                syn::PathArguments::Parenthesized(_) => false,
            }
        }
        _ => false,
    }
}

fn parse_name_arg(args: &Punctuated<Meta, Token![,]>) -> Result<LitStr, Error> {
    let Some(Meta::NameValue(name_value)) = args.first() else {
        return Err(Error::new(
            proc_macro2::Span::call_site(),
            "expected #[pd_host_function(name = \"...\")]",
        ));
    };
    if args.len() != 1 {
        let extra = args
            .iter()
            .nth(1)
            .expect("a non-empty attribute with more than one argument has an extra argument");
        return Err(Error::new_spanned(
            extra,
            "#[pd_host_function] only supports name = \"...\"",
        ));
    }
    if !name_value.path.is_ident("name") {
        return Err(Error::new_spanned(
            &name_value.path,
            "expected #[pd_host_function(name = \"...\")]",
        ));
    }
    match &name_value.value {
        syn::Expr::Lit(expr_lit) => {
            if let syn::Lit::Str(value) = &expr_lit.lit {
                Ok(value.clone())
            } else {
                Err(Error::new_spanned(
                    &expr_lit.lit,
                    "callable name must be a string literal",
                ))
            }
        }
        other => Err(Error::new_spanned(
            other,
            "callable name must be a string literal",
        )),
    }
}

fn doc_string(attrs: &[syn::Attribute]) -> String {
    attrs
        .iter()
        .filter_map(|attr| {
            if !attr.path().is_ident("doc") {
                return None;
            }
            match &attr.meta {
                Meta::NameValue(name_value) => match &name_value.value {
                    syn::Expr::Lit(expr_lit) => match &expr_lit.lit {
                        syn::Lit::Str(value) => Some(value.value().trim().to_string()),
                        _ => None,
                    },
                    _ => None,
                },
                _ => None,
            }
        })
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn validate_param(arg: &FnArg) -> Result<(), Error> {
    let FnArg::Typed(pat_type) = arg else {
        return Err(Error::new_spanned(arg, "methods are not supported"));
    };
    if is_vm_context_type(&pat_type.ty) {
        return Ok(());
    }
    let Pat::Ident(PatIdent { .. }) = pat_type.pat.as_ref() else {
        return Err(Error::new_spanned(
            &pat_type.pat,
            "callable parameters must use identifier patterns",
        ));
    };
    type_label(&pat_type.ty)?;
    Ok(())
}

fn validate_return_type(output: &ReturnType) -> Result<(), Error> {
    match output {
        ReturnType::Default => Ok(()),
        ReturnType::Type(_, ty) => {
            // Only the owned `Resource<T>` handle wrapper may be returned:
            // returning a `ResourceRef`/`ResourceMut` would hand a borrow
            // across the host boundary, which is forbidden by design.
            match pd_host_schema::resource_return_kind(ty) {
                Some(pd_host_schema::ResourceReturnKind::Borrow) => {
                    return Err(Error::new_spanned(
                        ty,
                        "ResourceRef cannot be a host function return; resource borrows cannot cross the host boundary",
                    ));
                }
                Some(pd_host_schema::ResourceReturnKind::BorrowMut) => {
                    return Err(Error::new_spanned(
                        ty,
                        "ResourceMut cannot be a host function return; mutable resource borrows cannot cross the host boundary",
                    ));
                }
                Some(pd_host_schema::ResourceReturnKind::Owned) | None => {}
            }
            type_label(ty)?;
            Ok(())
        }
    }
}

fn is_abi_declaration_only(item: &ItemFn) -> bool {
    let [stmt] = item.block.stmts.as_slice() else {
        return false;
    };
    let syn::Stmt::Expr(expr, None) = stmt else {
        return false;
    };
    let syn::Expr::Macro(expr_macro) = expr else {
        return false;
    };
    expr_macro.mac.path.is_ident("unreachable")
}

fn resource_mode_tokens(mode: ResourceMode) -> proc_macro2::TokenStream {
    match mode {
        ResourceMode::Borrow => quote!(super::super::ResourceAccessMode::Borrow),
        ResourceMode::BorrowMut => quote!(super::super::ResourceAccessMode::BorrowMut),
        ResourceMode::TakeOwned => quote!(super::super::ResourceAccessMode::TakeOwned),
        ResourceMode::Value => quote!(super::super::ResourceAccessMode::Value),
    }
}

fn resource_request_tokens(
    info: &ResourceParamInfo,
    index: &syn::Index,
    label: &LitStr,
) -> proc_macro2::TokenStream {
    let inner = &info.inner;
    let mode = resource_mode_tokens(info.mode);
    match &info.key {
        Some(key) => quote! {
            super::super::ResourceAccessRequest::from_value_with_key::<#inner>(
                &args[#index],
                #mode,
                super::super::ResourceTypeKey::new(#key)
                    .expect("resource key was validated by #[pd_host_function]"),
                #label,
            )?
        },
        None => quote! {
            super::super::ResourceAccessRequest::from_value::<#inner>(
                &args[#index],
                #mode,
                #label,
            )?
        },
    }
}

fn resource_extract_tokens(
    info: &ResourceParamInfo,
    ty: &Type,
    ident: &syn::Ident,
    index: &syn::Index,
) -> proc_macro2::TokenStream {
    let inner = &info.inner;
    let value = match info.mode {
        ResourceMode::Borrow => quote!(__pd_resource_frame
            .borrow::<#inner>(#index)
            .map_err(super::super::VmError::from)?),
        ResourceMode::BorrowMut => quote!(__pd_resource_frame
            .borrow_mut::<#inner>(#index)
            .map_err(super::super::VmError::from)?),
        ResourceMode::TakeOwned => {
            let taken = quote!(__pd_resource_frame
                .take_owned::<#inner>(#index)
                .map_err(super::super::VmError::from)?);
            if info.owned_wrapper {
                quote!(<#ty>::new(#taken))
            } else {
                taken
            }
        }
        ResourceMode::Value => {
            quote!(compile_error!("resource Value adapters are rejected"))
        }
    };
    quote!(let #ident = #value;)
}

fn generate_vm_wrapper(
    item: &ItemFn,
    wrapper_name: &syn::Ident,
) -> Result<proc_macro2::TokenStream, Error> {
    let impl_name = &item.sig.ident;
    let mut wrapper_params = Vec::<proc_macro2::TokenStream>::new();
    let mut call_args = Vec::<proc_macro2::TokenStream>::new();
    let mut imm_ordinary_decodes = Vec::<proc_macro2::TokenStream>::new();
    let mut mut_ordinary_decodes = Vec::<proc_macro2::TokenStream>::new();
    let mut imm_resource_extracts = Vec::<proc_macro2::TokenStream>::new();
    let mut mut_resource_extracts = Vec::<proc_macro2::TokenStream>::new();
    let mut resource_requests = Vec::<proc_macro2::TokenStream>::new();
    let mutable_wrapper_name = syn::Ident::new(&format!("{wrapper_name}_mut"), wrapper_name.span());
    let has_vm = item.sig.inputs.iter().any(|input| match input {
        FnArg::Typed(pat_type) => is_vm_context_type(&pat_type.ty),
        FnArg::Receiver(_) => false,
    });
    let has_resource = item
        .sig
        .inputs
        .iter()
        .any(|input| resource_param_info(input).ok().flatten().is_some());
    if has_vm || has_resource {
        wrapper_params.push(quote!(vm: &mut super::super::Vm));
        if has_vm {
            call_args.push(quote!(vm));
        }
    }
    let imm_wrapper_params = {
        let mut params = wrapper_params.clone();
        params.push(quote!(args: &[super::super::Value]));
        params
    };
    let mut_wrapper_params = {
        let mut params = wrapper_params.clone();
        params.push(quote!(args: &mut [super::super::Value]));
        params
    };

    // `arg_index` addresses the incoming `args` slice (shared by ordinary and
    // resource parameters); `resource_index` addresses the resource access
    // frame, which only contains the resource requests. These are distinct, so
    // a resource whose position in the argument list differs from its position
    // in the frame is still extracted from the correct frame slot.
    let mut arg_index = 0usize;
    let mut resource_index = 0usize;
    for input in &item.sig.inputs {
        let FnArg::Typed(pat_type) = input else {
            return Err(Error::new_spanned(input, "methods are not supported"));
        };
        if is_vm_context_type(&pat_type.ty) {
            continue;
        }
        let Pat::Ident(PatIdent { ident, .. }) = pat_type.pat.as_ref() else {
            return Err(Error::new_spanned(
                &pat_type.pat,
                "callable parameters must use identifier patterns",
            ));
        };
        let ty = &pat_type.ty;
        let label = LitStr::new(
            &format!("{} {}", wrapper_name, ident),
            proc_macro2::Span::call_site(),
        );
        let args_index = syn::Index::from(arg_index);
        if let Some(info) = resource_param_info(input)? {
            resource_requests.push(resource_request_tokens(&info, &args_index, &label));
            let frame_index = syn::Index::from(resource_index);
            let extraction = resource_extract_tokens(&info, ty, ident, &frame_index);
            imm_resource_extracts.push(extraction.clone());
            mut_resource_extracts.push(extraction);
            resource_index += 1;
        } else {
            // Ordinary parameters and value types are decoded *before* any
            // resource take, so a wrong-typed trailing ordinary argument can
            // never leave an earlier TakeOwned resource half-consumed.
            imm_ordinary_decodes.push(quote! {
                let #ident = super::borrow_arg::<#ty>(args, #args_index, #label)?;
            });
            let extractor = if uses_taken_extractor(ty) {
                quote!(super::take_arg::<#ty>(args, #args_index, #label)?)
            } else {
                quote!(super::borrow_arg::<#ty>(&*args, #args_index, #label)?)
            };
            mut_ordinary_decodes.push(quote! {
                let #ident = #extractor;
            });
        }
        call_args.push(quote!(#ident));
        arg_index += 1;
    }

    let wrapper_output = wrapper_output_type(&item.sig.output)?;
    let call_expr = if return_is_vm_result(&item.sig.output) {
        quote!(#impl_name(#(#call_args),*))
    } else {
        quote!(Ok(#impl_name(#(#call_args),*)))
    };
    let imm_resource_frame = if has_resource {
        quote! {
            let mut __pd_resource_frame = vm.begin_resource_access(vec![#(#resource_requests),*])?;
        }
    } else {
        quote! {}
    };
    let mut_resource_frame = imm_resource_frame.clone();

    Ok(quote! {
        #[allow(dead_code)]
        pub(crate) fn #wrapper_name(#(#imm_wrapper_params),*) -> #wrapper_output {
            #imm_resource_frame
            #(#imm_ordinary_decodes)*
            #(#imm_resource_extracts)*
            #call_expr
        }

        #[allow(dead_code)]
        pub(crate) fn #mutable_wrapper_name(#(#mut_wrapper_params),*) -> #wrapper_output {
            #mut_resource_frame
            #(#mut_ordinary_decodes)*
            #(#mut_resource_extracts)*
            #call_expr
        }
    })
}

fn generate_async_vm_wrapper(
    item: &ItemFn,
    wrapper_name: &syn::Ident,
) -> Result<proc_macro2::TokenStream, Error> {
    let impl_name = &item.sig.ident;
    let mutable_wrapper_name = syn::Ident::new(&format!("{wrapper_name}_mut"), wrapper_name.span());
    let mut ordinary_decodes = Vec::<proc_macro2::TokenStream>::new();
    let mut resource_extracts = Vec::<proc_macro2::TokenStream>::new();
    let mut call_args = Vec::<proc_macro2::TokenStream>::new();
    let mut resource_requests = Vec::<proc_macro2::TokenStream>::new();
    let mut arg_index = 0usize;
    let mut resource_index = 0usize;

    for input in &item.sig.inputs {
        let FnArg::Typed(pat_type) = input else {
            return Err(Error::new_spanned(input, "methods are not supported"));
        };
        let Pat::Ident(PatIdent { ident, .. }) = pat_type.pat.as_ref() else {
            return Err(Error::new_spanned(
                &pat_type.pat,
                "callable parameters must use identifier patterns",
            ));
        };
        let ty = &pat_type.ty;
        if is_host_context_param(input) {
            ordinary_decodes.push(quote! {
                let #ident = <#ty as super::CaptureAsyncHostContext>::capture_with_args(vm, args)?;
            });
            call_args.push(quote!(#ident));
            continue;
        }
        let label = LitStr::new(
            &format!("{} {}", wrapper_name, ident),
            proc_macro2::Span::call_site(),
        );
        let args_index = syn::Index::from(arg_index);
        if let Some(info) = resource_param_info(input)? {
            resource_requests.push(resource_request_tokens(&info, &args_index, &label));
            // Async resource parameters are restricted to TakeOwned (the
            // borrows are rejected during validation), so extraction mutates
            // the table. Ordinary decodes are emitted first so a wrong-typed
            // trailing ordinary argument leaves every resource GuestOwned.
            let frame_index = syn::Index::from(resource_index);
            resource_extracts.push(resource_extract_tokens(&info, ty, ident, &frame_index));
            resource_index += 1;
        } else {
            ordinary_decodes.push(quote! {
                let #ident = super::borrow_arg::<#ty>(args, #args_index, #label)?;
            });
        }
        call_args.push(quote!(#ident));
        arg_index += 1;
    }

    let await_value = if return_is_vm_result(&item.sig.output) {
        quote!(#impl_name(#(#call_args),*).await?)
    } else {
        quote!(#impl_name(#(#call_args),*).await)
    };
    let future_result = if return_is_host_future_output(&item.sig.output) {
        quote!(Ok(value.map(super::return_one)))
    } else {
        quote! {
            match super::IntoHostCallOutcome::into_host_call_outcome(value) {
                super::CallOutcome::Return(values) => {
                    Ok(super::HostFutureOutput::returning(values))
                }
                super::CallOutcome::Pending(op_id) => Err(super::VmError::HostError(
                    format!("async host function returned nested pending operation {op_id}"),
                )),
                super::CallOutcome::Halt | super::CallOutcome::Yield => Err(
                    super::VmError::HostError(
                        "async host function returned a control-flow outcome".to_string(),
                    ),
                ),
            }
        }
    };
    let resource_frame = if resource_requests.is_empty() {
        quote! {}
    } else {
        quote! {
            let mut __pd_resource_frame = vm.begin_resource_access(vec![#(#resource_requests),*])?;
        }
    };
    let body = quote! {
        #resource_frame
        #(#ordinary_decodes)*
        #(#resource_extracts)*
        vm.submit_host_future(Box::pin(async move {
            let value = #await_value;
            #future_result
        }))
    };

    Ok(quote! {
        #[allow(dead_code)]
        pub(crate) fn #wrapper_name(
            vm: &mut super::super::Vm,
            args: &[super::super::Value],
        ) -> super::super::VmResult<super::CallOutcome> {
            #body
        }

        #[allow(dead_code)]
        pub(crate) fn #mutable_wrapper_name(
            vm: &mut super::super::Vm,
            args: &mut [super::super::Value],
        ) -> super::super::VmResult<super::CallOutcome> {
            #body
        }
    })
}

fn wrapper_and_impl_names(name: &syn::Ident) -> (syn::Ident, syn::Ident) {
    let original = name.to_string();
    match original.strip_suffix("_impl") {
        Some(prefix) => (
            syn::Ident::new(prefix, name.span()),
            syn::Ident::new(&original, name.span()),
        ),
        None => (
            syn::Ident::new(&original, name.span()),
            syn::Ident::new(&format!("{original}_impl"), name.span()),
        ),
    }
}

fn wrapper_output_type(output: &ReturnType) -> Result<proc_macro2::TokenStream, Error> {
    if let Some(inner) = vm_result_inner_type(output)? {
        return Ok(quote!(super::super::VmResult<#inner>));
    }

    match output {
        ReturnType::Default => Ok(quote!(super::super::VmResult<()>)),
        ReturnType::Type(_, ty) => Ok(quote!(super::super::VmResult<#ty>)),
    }
}

fn vm_result_inner_type(output: &ReturnType) -> Result<Option<Type>, Error> {
    let ReturnType::Type(_, ty) = output else {
        return Ok(None);
    };
    unwrap_vm_result_type(ty)
}

fn unwrap_vm_result_type(ty: &Type) -> Result<Option<Type>, Error> {
    match ty {
        Type::Group(group) => unwrap_vm_result_type(&group.elem),
        Type::Paren(paren) => unwrap_vm_result_type(&paren.elem),
        Type::Reference(reference) => unwrap_vm_result_type(&reference.elem),
        Type::Path(path) => {
            let Some(segment) = path.path.segments.last() else {
                return Ok(None);
            };
            if segment.ident != "VmResult" {
                return Ok(None);
            }
            let syn::PathArguments::AngleBracketed(args) = &segment.arguments else {
                return Err(Error::new_spanned(
                    &segment.arguments,
                    format!("{}<T> requires one generic argument", segment.ident),
                ));
            };
            let Some(syn::GenericArgument::Type(inner)) = args.args.first() else {
                return Err(Error::new_spanned(
                    args,
                    format!("{}<T> requires one type argument", segment.ident),
                ));
            };
            Ok(Some(inner.clone()))
        }
        _ => Ok(None),
    }
}

fn return_is_host_future_output(output: &ReturnType) -> bool {
    vm_result_inner_type(output)
        .expect("pd_host_function return type should already be validated")
        .and_then(|ty| match ty {
            Type::Path(path) => path
                .path
                .segments
                .last()
                .map(|segment| segment.ident.clone()),
            _ => None,
        })
        .is_some_and(|ident| ident == "HostFutureOutput")
}

fn return_is_vm_result(output: &ReturnType) -> bool {
    vm_result_inner_type(output)
        .expect("pd_host_function return type should already be validated")
        .is_some()
}

fn type_label(ty: &Type) -> Result<String, Error> {
    match ty {
        Type::Group(group) => type_label(&group.elem),
        Type::Paren(paren) => type_label(&paren.elem),
        Type::Reference(reference) => type_label(&reference.elem),
        Type::Slice(slice) => match slice.elem.as_ref() {
            Type::Path(path) => {
                let Some(segment) = path.path.segments.last() else {
                    return Err(Error::new_spanned(slice, "unsupported callable type"));
                };
                if segment.ident == "u8" {
                    Ok("bytes".to_string())
                } else {
                    Err(Error::new_spanned(slice, "unsupported callable type"))
                }
            }
            _ => Err(Error::new_spanned(slice, "unsupported callable type")),
        },
        Type::Tuple(tuple) if tuple.elems.is_empty() => Ok("null".to_string()),
        Type::Path(path) => {
            let Some(segment) = path.path.segments.last() else {
                return Err(Error::new_spanned(path, "unsupported callable type"));
            };
            let ident = segment.ident.to_string();
            match ident.as_str() {
                "i8" | "i16" | "i32" | "i64" | "i128" | "isize" | "u8" | "u16" | "u32" | "u64"
                | "u128" | "usize" => Ok("int".to_string()),
                "f32" | "f64" => Ok("float".to_string()),
                "bool" => Ok("bool".to_string()),
                "String" | "str" | "VmStringRef" => Ok("string".to_string()),
                "Bytes" | "VmBytes" | "VmBytesRef" | "VmBytesHandle" => Ok("bytes".to_string()),
                "Any" | "AnyValue" | "Value" | "VmValueRef" | "VmValueOwned" => {
                    Ok("any".to_string())
                }
                "Array" | "VmArray" | "VmArrayRef" | "VmArrayHandle" => Ok("array".to_string()),
                "Map" | "VmMap" | "VmMapRef" | "VmMapHandle" => Ok("map".to_string()),
                "Number" | "NumberValue" => Ok("number".to_string()),
                "Resource" => Ok(pd_host_schema::RESOURCE_SCHEMA_LABEL.to_string()),
                "ResourceOwned" => Err(Error::new_spanned(
                    path,
                    "ResourceOwned is an input-only TakeOwned wrapper",
                )),
                "ResourceRef" | "ResourceMut" => {
                    Ok(pd_host_schema::RESOURCE_SCHEMA_LABEL.to_string())
                }
                "VmCallable" => callable_type_label(segment),
                "Unknown" | "UnknownValue" => Ok("unknown".to_string()),
                "CallOutcome" => Ok("unknown".to_string()),
                "Option" => {
                    let syn::PathArguments::AngleBracketed(args) = &segment.arguments else {
                        return Err(Error::new_spanned(
                            &segment.arguments,
                            "Option<T> requires one generic argument",
                        ));
                    };
                    let Some(syn::GenericArgument::Type(inner)) = args.args.first() else {
                        return Err(Error::new_spanned(
                            args,
                            "Option<T> requires one type argument",
                        ));
                    };
                    let inner_label = type_label(inner)?;
                    Ok(format!("{inner_label} | null"))
                }
                "VmResult" | "HostCallResult" | "HostFutureOutput" => {
                    let syn::PathArguments::AngleBracketed(args) = &segment.arguments else {
                        return Err(Error::new_spanned(
                            &segment.arguments,
                            format!("{ident}<T> requires one generic argument"),
                        ));
                    };
                    let Some(syn::GenericArgument::Type(inner)) = args.args.first() else {
                        return Err(Error::new_spanned(
                            args,
                            format!("{ident}<T> requires one type argument"),
                        ));
                    };
                    type_label(inner)
                }
                "Vec" => type_label_for_vec(segment),
                _ => Err(Error::new_spanned(
                    path,
                    format!("unsupported callable type '{ident}'"),
                )),
            }
        }
        _ => Err(Error::new_spanned(ty, "unsupported callable type")),
    }
}

fn callable_type_label(segment: &syn::PathSegment) -> Result<String, Error> {
    let syn::PathArguments::AngleBracketed(args) = &segment.arguments else {
        return Err(Error::new_spanned(
            &segment.arguments,
            "VmCallable requires a function signature",
        ));
    };
    let Some(syn::GenericArgument::Type(Type::BareFn(function))) = args.args.first() else {
        return Err(Error::new_spanned(
            args,
            "VmCallable requires fn(...) -> ...",
        ));
    };
    let params = function
        .inputs
        .iter()
        .map(|input| type_label(&input.ty))
        .collect::<Result<Vec<_>, _>>()?;
    let result = match &function.output {
        ReturnType::Default => "null".to_string(),
        ReturnType::Type(_, ty) => type_label(ty)?,
    };
    Ok(format!("fn({}) -> {result}", params.join(", ")))
}

fn type_label_for_vec(segment: &syn::PathSegment) -> Result<String, Error> {
    let syn::PathArguments::AngleBracketed(args) = &segment.arguments else {
        return Err(Error::new_spanned(
            &segment.arguments,
            "Vec<T> requires one generic argument",
        ));
    };
    let Some(syn::GenericArgument::Type(inner)) = args.args.first() else {
        return Err(Error::new_spanned(
            args,
            "Vec<T> requires one type argument",
        ));
    };
    match inner {
        Type::Tuple(tuple) if tuple.elems.len() == 2 => {
            let lhs = tuple
                .elems
                .first()
                .expect("tuple should contain first element");
            let rhs = tuple
                .elems
                .last()
                .expect("tuple should contain second element");
            if is_value_type(lhs) && is_value_type(rhs) {
                Ok("map".to_string())
            } else {
                Err(Error::new_spanned(
                    inner,
                    "unsupported Vec tuple type in callable metadata",
                ))
            }
        }
        _ if is_value_type(inner) => Ok("array".to_string()),
        _ => {
            let inner_label = type_label(inner)?;
            Err(Error::new_spanned(
                inner,
                format!("unsupported Vec return type '{inner_label}'"),
            ))
        }
    }
}

fn is_value_type(ty: &Type) -> bool {
    match ty {
        Type::Group(group) => is_value_type(&group.elem),
        Type::Paren(paren) => is_value_type(&paren.elem),
        Type::Reference(reference) => is_value_type(&reference.elem),
        Type::Path(path) => path
            .path
            .segments
            .last()
            .is_some_and(|segment| segment.ident == "Value"),
        _ => false,
    }
}

fn is_vm_context_type(ty: &Type) -> bool {
    match ty {
        Type::Group(group) => is_vm_context_type(&group.elem),
        Type::Paren(paren) => is_vm_context_type(&paren.elem),
        Type::Reference(reference) => is_vm_context_type(&reference.elem),
        Type::Path(path) => path
            .path
            .segments
            .last()
            .is_some_and(|segment| segment.ident == "Vm"),
        _ => false,
    }
}

fn uses_taken_extractor(ty: &Type) -> bool {
    match ty {
        Type::Group(group) => uses_taken_extractor(&group.elem),
        Type::Paren(paren) => uses_taken_extractor(&paren.elem),
        Type::Reference(_) => false,
        Type::Path(path) => path.path.segments.last().is_some_and(|segment| {
            matches!(
                segment.ident.to_string().as_str(),
                "Value"
                    | "AnyValue"
                    | "UnknownValue"
                    | "VmArray"
                    | "VmBytes"
                    | "VmMap"
                    | "VmArrayHandle"
                    | "VmBytesHandle"
                    | "VmMapHandle"
                    | "VmValueOwned"
            )
        }),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{expand_pd_host_function, type_label};
    use syn::{ItemFn, Meta, Token, Type, parse_quote, punctuated::Punctuated};

    #[test]
    fn accepts_host_call_result_from_the_function_signature() {
        let attr: Punctuated<Meta, Token![,]> = parse_quote!(name = "test::suspend");
        let item: ItemFn = parse_quote! {
            /// Returns a value after a host operation completes.
            #[pd_host_function(name = "test::suspend")]
            fn suspend() -> VmResult<HostCallResult<Value>> {
                todo!()
            }
        };

        let expanded = expand_pd_host_function(attr, item)
            .expect("HostCallResult should be accepted from the return signature");
        assert!(expanded.to_string().contains("HostCallResult"));
    }

    #[test]
    fn rejects_host_result_compatibility_wrapper() {
        let attr: Punctuated<Meta, Token![,]> = parse_quote!(name = "test::legacy");
        let item: ItemFn = parse_quote! {
            /// Legacy result wrapper must be rejected.
            #[pd_host_function(name = "test::legacy")]
            fn legacy() -> HostResult<Value> {
                todo!()
            }
        };

        let error = expand_pd_host_function(attr, item)
            .expect_err("HostResult must not be accepted as a return wrapper");
        assert!(error.to_string().contains("unsupported callable type"));
    }

    #[test]
    fn rejects_async_attribute_instead_of_treating_it_as_a_host_contract() {
        let attr: Punctuated<Meta, Token![,]> =
            parse_quote!(name = "test::suspend", r#async = true);
        let item: ItemFn = parse_quote! {
            /// Returns a value after a host operation completes.
            #[pd_host_function(name = "test::suspend")]
            fn suspend() -> VmResult<HostCallResult<Value>> {
                todo!()
            }
        };

        let error = expand_pd_host_function(attr, item)
            .expect_err("the pd-host-function macro must not accept an async attribute");
        assert!(error.to_string().contains("only supports name"));
    }

    #[test]
    fn ordinary_async_signature_generates_host_driven_future_submission() {
        let attr: Punctuated<Meta, Token![,]> = parse_quote!(name = "test::async_call");
        let item: ItemFn = parse_quote!(
            /// Returns an owned string asynchronously.
            async fn async_call(
                #[pd_host_context] context: TestContext,
                value: String,
            ) -> VmResult<String> {
                context.run(value).await
            }
        );
        let expanded = expand_pd_host_function(attr, item)
            .expect("ordinary owned async function should use the generic async host contract")
            .to_string();
        assert!(expanded.contains("submit_host_future"));
        assert!(expanded.contains("async move"));
        assert!(expanded.contains("borrow_arg"));
        assert!(expanded.contains("CaptureAsyncHostContext"));
        assert!(expanded.contains("capture_with_args"));
        assert!(!expanded.contains("pd_host_context"));
    }

    #[test]
    fn async_host_future_output_maps_its_inner_value_to_call_return() {
        let attr: Punctuated<Meta, Token![,]> = parse_quote!(name = "test::completion");
        let item: ItemFn = parse_quote! {
            /// Completes after mutating VM-owned state.
            async fn completion() -> VmResult<HostFutureOutput<i64>> {
                todo!()
            }
        };

        let expanded = expand_pd_host_function(attr, item)
            .expect("host future output should be accepted")
            .to_string();
        assert!(expanded.contains("value . map (super :: return_one)"));
    }

    #[test]
    fn async_signature_rejects_borrowed_parameters() {
        let attr: Punctuated<Meta, Token![,]> = parse_quote!(name = "test::borrowed");
        let item: ItemFn = parse_quote! {
            async fn borrowed(value: &str) -> VmResult<String> {
                Ok(value.to_string())
            }
        };

        let error = expand_pd_host_function(attr, item).expect_err("borrow should be rejected");
        assert!(
            error
                .to_string()
                .contains("parameters must be owned and 'static")
        );
    }

    #[test]
    fn resource_parameters_generate_preflighted_typed_access() {
        let attr: Punctuated<Meta, Token![,]> = parse_quote!(name = "test::resource");
        let item: ItemFn = parse_quote! {
            /// Uses a borrowed resource and returns its guest handle.
            fn resource(
                #[pd_host_param(passing = "borrow", key = "test.fake")]
                resource: ResourceRef<'_, FakeResource>,
            ) -> Resource<FakeResource> {
                todo!()
            }
        };
        let expanded = expand_pd_host_function(attr, item)
            .expect("resource parameter should be accepted")
            .to_string();
        assert!(expanded.contains("begin_resource_access"));
        assert!(expanded.contains("ResourceAccessRequest"));
        assert!(expanded.contains("ResourceAccessMode :: Borrow"));
        assert!(expanded.contains("borrow :: < FakeResource >"));
    }

    #[test]
    fn async_resource_borrow_is_rejected_but_owned_take_is_extracted_before_future() {
        let borrow_attr: Punctuated<Meta, Token![,]> = parse_quote!(name = "test::borrow");
        let borrow_item: ItemFn = parse_quote! {
            /// Borrow cannot cross a pending future.
            async fn borrow(resource: ResourceRef<'_, FakeResource>) -> VmResult<i64> {
                let _ = resource;
                Ok(0)
            }
        };
        let error = expand_pd_host_function(borrow_attr, borrow_item)
            .expect_err("async resource borrow must be rejected");
        assert!(error.to_string().contains("cannot cross async/yield"));

        let take_attr: Punctuated<Meta, Token![,]> = parse_quote!(name = "test::take");
        let take_item: ItemFn = parse_quote! {
            /// Moves a resource into an owned async operation.
            async fn take(resource: ResourceOwned<FakeResource>) -> VmResult<i64> {
                let _ = resource;
                Ok(0)
            }
        };
        let expanded = expand_pd_host_function(take_attr, take_item)
            .expect("owned resource take should be accepted")
            .to_string();
        assert!(expanded.contains("begin_resource_access"));
        assert!(expanded.contains("take_owned"));
        assert!(expanded.contains("async move"));
    }

    #[test]
    fn resource_requests_use_argument_index_while_frame_uses_resource_relative_index() {
        // A resource after a prefix ordinary argument must read args[1] but be
        // extracted from frame slot 0 (the frame only contains the resources).
        let attr: Punctuated<Meta, Token![,]> = parse_quote!(name = "test::combo");
        let item: ItemFn = parse_quote! {
            /// Takes a resource after a prefix ordinary argument.
            fn combo(prefix: i64, resource: ResourceOwned<FakeResource>) -> i64 {
                todo!()
            }
        };
        let expanded = expand_pd_host_function(attr, item)
            .expect("interleaved resource should be accepted")
            .to_string();
        assert!(
            expanded.contains("& args [1]"),
            "resource request must read the argument at its argument index: {expanded}"
        );
        assert!(
            expanded.contains("take_owned :: < FakeResource > (0)"),
            "resource extraction must use the resource-relative frame index: {expanded}"
        );
        assert!(
            !expanded.contains("take_owned :: < FakeResource > (1)"),
            "resource extraction must not use the argument index: {expanded}"
        );
    }

    #[test]
    fn sync_wrapper_decodes_ordinary_arguments_before_resource_takes() {
        // `take(r, n)`: the ordinary `n` decode must appear before the frame's
        // `take_owned` so a wrong-typed `n` leaves the resource GuestOwned.
        let attr: Punctuated<Meta, Token![,]> = parse_quote!(name = "test::take");
        let item: ItemFn = parse_quote! {
            /// Takes a resource and an ordinary argument.
            fn take(resource: ResourceOwned<FakeResource>, n: i64) -> VmResult<i64> {
                todo!()
            }
        };
        let expanded = expand_pd_host_function(attr, item)
            .expect("resource take should be accepted")
            .to_string();
        let ordinary = expanded
            .find("borrow_arg :: < i64 > (args , 1")
            .unwrap_or_else(|| panic!("missing ordinary decode: {expanded}"));
        let take = expanded
            .find("take_owned")
            .unwrap_or_else(|| panic!("missing take_owned: {expanded}"));
        assert!(
            ordinary < take,
            "ordinary decode must precede the resource take"
        );
    }

    #[test]
    fn async_wrapper_extracts_owned_resource_before_submitting_the_future() {
        let attr: Punctuated<Meta, Token![,]> = parse_quote!(name = "test::take_async");
        let item: ItemFn = parse_quote! {
            /// Moves a resource into an owned async operation after an ordinary arg.
            async fn take_async(prefix: i64, resource: ResourceOwned<FakeResource>) -> VmResult<i64> {
                let _ = (prefix, resource);
                Ok(0)
            }
        };
        let expanded = expand_pd_host_function(attr, item)
            .expect("owned async resource take should be accepted")
            .to_string();
        let ordinary = expanded
            .find("borrow_arg :: < i64 > (args , 0")
            .unwrap_or_else(|| panic!("missing ordinary decode: {expanded}"));
        let take = expanded
            .find("take_owned :: < FakeResource > (0)")
            .unwrap_or_else(|| panic!("missing frame take: {expanded}"));
        let future = expanded
            .find("submit_host_future")
            .unwrap_or_else(|| panic!("missing future submission: {expanded}"));
        assert!(
            ordinary < take,
            "ordinary decode must precede the resource take"
        );
        assert!(
            take < future,
            "resource take must precede the owned future submission"
        );
    }

    #[test]
    fn owned_resource_return_is_accepted() {
        let attr: Punctuated<Meta, Token![,]> = parse_quote!(name = "test::make");
        let item: ItemFn = parse_quote! {
            /// Returns an owned resource handle.
            fn make(seed: i64) -> Resource<FakeResource> {
                todo!()
            }
        };
        let expanded = expand_pd_host_function(attr, item)
            .expect("owned Resource<T> return should be accepted")
            .to_string();
        assert!(expanded.contains("Resource < FakeResource >"));
    }

    #[test]
    fn borrowed_resource_returns_are_rejected() {
        for (return_type, message) in [
            (
                "ResourceRef<'_, FakeResource>",
                "ResourceRef cannot be a host function return",
            ),
            (
                "ResourceMut<'_, FakeResource>",
                "ResourceMut cannot be a host function return",
            ),
        ] {
            let ty: Type = syn::parse_str(return_type).expect("parse return type");
            let attr: Punctuated<Meta, Token![,]> = parse_quote!(name = "test::borrow_return");
            let item: ItemFn = parse_quote! {
                /// A borrow must not cross the host boundary.
                fn borrow_return(value: i64) -> #ty {
                    todo!()
                }
            };
            let error = expand_pd_host_function(attr, item)
                .expect_err("borrowed resource returns must be rejected");
            assert!(error.to_string().contains(message), "{error}");
        }
    }

    #[test]
    fn invalid_resource_keys_are_rejected_at_expansion() {
        for key in ["", "bad key", "io..file", "A.b"] {
            let attr: Punctuated<Meta, Token![,]> = parse_quote!(name = "test::keyed");
            let item: syn::ItemFn = syn::parse_quote! {
                /// Uses an explicit resource key.
                fn keyed(
                    #[pd_host_param(passing = "take_owned", key = #key)]
                    resource: FakeResource,
                ) -> i64 {
                    todo!()
                }
            };
            let error = expand_pd_host_function(attr, item)
                .expect_err("invalid resource keys must fail at expansion time");
            assert!(error.to_string().contains("resource type key"), "{error}");
        }

        let overlong = "a".repeat(129);
        let attr: Punctuated<Meta, Token![,]> = parse_quote!(name = "test::keyed");
        let item: syn::ItemFn = syn::parse_quote! {
            /// Uses an over-long resource key.
            fn keyed(
                #[pd_host_param(passing = "take_owned", key = #overlong)]
                resource: FakeResource,
            ) -> i64 {
                todo!()
            }
        };
        let error = expand_pd_host_function(attr, item)
            .expect_err("over-long resource keys must fail at expansion time");
        assert!(error.to_string().contains("maximum is 128"), "{error}");
    }

    #[test]
    fn generic_host_functions_are_rejected_at_expansion() {
        let attr: Punctuated<Meta, Token![,]> = parse_quote!(name = "test::generic");
        let item: syn::ItemFn = syn::parse_quote! {
            /// Generic host functions cannot be instantiated by the adapter.
            fn generic_resource<T>(resource: ResourceOwned<T>) -> i64 {
                todo!()
            }
        };
        let error = expand_pd_host_function(attr, item)
            .expect_err("generic host functions must be rejected");
        assert!(
            error
                .to_string()
                .contains("does not support generic host functions"),
            "{error}"
        );
    }

    #[test]
    fn alias_annotated_resource_shape_is_rejected_at_expansion() {
        let attr: Punctuated<Meta, Token![,]> = parse_quote!(name = "test::aliased");
        let item: syn::ItemFn = syn::parse_quote! {
            /// An alias wrapper path is not a canonical resource wrapper.
            fn aliased(
                #[pd_host_param(passing = "borrow")]
                resource: my_alias::Wrapper<'static, FakeResource>,
            ) -> i64 {
                todo!()
            }
        };
        let error = expand_pd_host_function(attr, item)
            .expect_err("alias-shaped resource annotation must be rejected");
        assert!(error.to_string().contains("alias"), "{error}");
    }

    #[test]
    fn callable_wrapper_preserves_parameter_and_result_schema() {
        let ty: Type = parse_quote!(VmCallable<fn(VmMap) -> VmMap>);
        assert_eq!(type_label(&ty).unwrap(), "fn(map) -> map");
        let attr: Punctuated<Meta, Token![,]> = parse_quote!(name = "test::stream");
        let item: ItemFn = parse_quote! {
            /// Starts a synthetic callable stream.
            fn stream(callback: VmCallable<fn(VmMap) -> VmMap>) -> VmResult<CallOutcome> {
                todo!()
            }
        };
        let expanded = expand_pd_host_function(attr, item).unwrap().to_string();
        assert!(expanded.contains("VmCallable < fn (VmMap) -> VmMap >"));
        assert!(expanded.contains("borrow_arg"));

        let float_ty: Type = parse_quote!(VmCallable<fn(f64) -> f64>);
        assert_eq!(type_label(&float_ty).unwrap(), "fn(float) -> float");
    }
}
