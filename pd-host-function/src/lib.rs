use proc_macro::TokenStream;
use quote::quote;
use syn::{
    Error, FnArg, GenericArgument, ItemFn, LitStr, Meta, Pat, PatIdent, PathArguments, ReturnType,
    Token, Type, parse_macro_input, punctuated::Punctuated,
};

use pd_host_schema::{
    ResourceMode, ResourceReturnKind, ResourceSpec, StateSpec, borrowed_resource_return,
    resource_return_kind, resource_spec, state_spec,
};

#[proc_macro_attribute]
pub fn pd_host_function(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args = parse_macro_input!(attr with Punctuated::<Meta, Token![,]>::parse_terminated);
    match expand_pd_host_function(args, parse_macro_input!(item as ItemFn)) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

fn expand_pd_host_function(
    attr: Punctuated<Meta, Token![,]>,
    mut item: ItemFn,
) -> Result<proc_macro2::TokenStream, Error> {
    let (guest_name, contract) = parse_function_args(&attr)?;
    let is_async = item.sig.asyncness.is_some();
    let docs = doc_string(&item.attrs);
    let mut resource_params = Vec::<(String, ResourceSpec)>::new();
    let mut state_params = Vec::<(String, StateSpec)>::new();
    for input in &item.sig.inputs {
        let is_host_context = is_host_context_param(input);
        if !is_host_context && !is_vm_context_param(input) {
            let FnArg::Typed(pat_type) = input else {
                return Err(Error::new_spanned(input, "methods are not supported"));
            };
            if let Some(spec) = state_spec(&pat_type.ty) {
                if is_async {
                    return Err(Error::new_spanned(
                        &pat_type.ty,
                        "hidden host state parameters cannot cross async/yield; capture an owned \
                         value or provider instead",
                    ));
                }
                let Pat::Ident(PatIdent { ident, .. }) = pat_type.pat.as_ref() else {
                    return Err(Error::new_spanned(
                        &pat_type.pat,
                        "hidden host state parameters must use identifier patterns",
                    ));
                };
                state_params.push((ident.to_string(), spec));
                continue;
            }
            let spec = resource_spec(&pat_type.ty, &pat_type.attrs)
                .map_err(|message| Error::new_spanned(&pat_type.ty, message))?;
            if let Some(spec) = spec {
                if is_async && !matches!(spec.mode, ResourceMode::TakeOwned) {
                    return Err(Error::new_spanned(
                        &pat_type.ty,
                        "resource borrows cannot cross async/yield; only TakeOwned may move into an owned operation",
                    ));
                }
                let Pat::Ident(PatIdent { ident, .. }) = pat_type.pat.as_ref() else {
                    return Err(Error::new_spanned(
                        &pat_type.pat,
                        "resource parameters must use identifier patterns",
                    ));
                };
                resource_params.push((ident.to_string(), spec));
                continue;
            }
        }
        if is_async {
            validate_async_param(input)?;
        } else if is_host_context_param(input) {
            return Err(Error::new_spanned(
                input,
                "#[pd_host_context] is only valid on async host functions",
            ));
        }
        if !is_host_context_param(input) && !is_vm_context_param(input) {
            validate_param(input)?;
        }
    }
    validate_sync_vm_resource_borrow_conflict(&item, &resource_params)?;
    validate_sync_vm_state_conflict(&item, &state_params)?;
    validate_state_resource_combination(&state_params, &resource_params)?;
    validate_return_type(&item.sig.output, has_named_struct_attr(&item.attrs))?;
    if contract.is_some() && has_named_struct_attr(&item.attrs) {
        return Err(Error::new_spanned(
            &item.sig.ident,
            "#[pd_host_named_struct] is redundant with a declared contract; the contract schema \
             carries the named return",
        ));
    }

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
        generate_async_vm_wrapper(&item, &wrapper_name, &resource_params)?
    } else {
        generate_vm_wrapper(
            &item,
            &wrapper_name,
            &guest_name,
            &resource_params,
            &state_params,
        )?
    };
    let descriptor = generate_host_function_descriptor(
        &item,
        &wrapper_name,
        &guest_name,
        &docs,
        &resource_params,
        &state_params,
        contract.as_ref(),
    )?;
    for input in &mut item.sig.inputs {
        if let FnArg::Typed(pat_type) = input {
            pat_type.attrs.retain(|attr| {
                !matches!(
                    attr.path()
                        .get_ident()
                        .map(syn::Ident::to_string)
                        .as_deref(),
                    Some(
                        "pd_host_context"
                            | "pd_host_param"
                            | "pd_host_resource"
                            | "pd_host_passing"
                            | "pd_borrow"
                            | "pd_borrow_mut"
                            | "pd_take_owned"
                            | "pd_value"
                            | "pd_host_named_struct"
                    )
                )
            });
        }
    }
    item.attrs
        .retain(|attr| !attr.path().is_ident("pd_host_named_struct"));
    Ok(quote! {
        #item
        #wrapper
        #descriptor
    })
}

fn is_vm_context_param(arg: &FnArg) -> bool {
    match arg {
        FnArg::Typed(pat_type) => is_vm_context_type(&pat_type.ty),
        FnArg::Receiver(_) => false,
    }
}

fn is_mut_vm_context_param(arg: &FnArg) -> bool {
    match arg {
        FnArg::Typed(pat_type) => is_mut_vm_context_type(&pat_type.ty),
        FnArg::Receiver(_) => false,
    }
}

fn is_mut_vm_context_type(ty: &Type) -> bool {
    match ty {
        Type::Group(group) => is_mut_vm_context_type(&group.elem),
        Type::Paren(paren) => is_mut_vm_context_type(&paren.elem),
        Type::Reference(reference) => {
            reference.mutability.is_some() && is_vm_context_type(&reference.elem)
        }
        _ => false,
    }
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
        Type::BareFn(function) => {
            function
                .inputs
                .iter()
                .all(|input| is_async_owned_type(&input.ty))
                && match &function.output {
                    ReturnType::Default => true,
                    ReturnType::Type(_, output) => is_async_owned_type(output),
                }
        }
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

/// Parses `#[pd_host_function(name = "...")]` plus the optional
/// `contract = <path>` guest-schema override.
///
/// `contract` names a zero-argument callable returning a
/// [`HostFunctionSchema`](pd_host_schema) for functions whose guest contract
/// cannot be inferred from the Rust signature alone (raw handle parameters,
/// fixed-shape map returns). The contract is declared next to the function it
/// describes, so the adapter, binding class, and effects still come from one
/// macro expansion and there is no parallel catalog entry.
fn parse_function_args(
    args: &Punctuated<Meta, Token![,]>,
) -> Result<(LitStr, Option<syn::Path>), Error> {
    let mut name: Option<LitStr> = None;
    let mut contract: Option<syn::Path> = None;
    for meta in args {
        match meta {
            Meta::NameValue(name_value) if name_value.path.is_ident("name") => {
                let syn::Expr::Lit(expr_lit) = &name_value.value else {
                    return Err(Error::new_spanned(
                        &name_value.value,
                        "callable name must be a string literal",
                    ));
                };
                let syn::Lit::Str(value) = &expr_lit.lit else {
                    return Err(Error::new_spanned(
                        &expr_lit.lit,
                        "callable name must be a string literal",
                    ));
                };
                if name.is_some() {
                    return Err(Error::new_spanned(
                        meta,
                        "duplicate `name = \"...\"` argument",
                    ));
                }
                name = Some(value.clone());
            }
            Meta::NameValue(name_value) if name_value.path.is_ident("contract") => {
                let syn::Expr::Path(expr_path) = &name_value.value else {
                    return Err(Error::new_spanned(
                        &name_value.value,
                        "`contract` must name a zero-argument guest-schema callable",
                    ));
                };
                if contract.is_some() {
                    return Err(Error::new_spanned(
                        meta,
                        "duplicate `contract = ...` argument",
                    ));
                }
                contract = Some(expr_path.path.clone());
            }
            other => {
                return Err(Error::new_spanned(
                    other,
                    "#[pd_host_function] only supports name = \"...\", an optional \
                     contract = <path>",
                ));
            }
        }
    }
    let Some(name) = name else {
        return Err(Error::new(
            proc_macro2::Span::call_site(),
            "expected #[pd_host_function(name = \"...\")]",
        ));
    };
    Ok((name, contract))
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
    if resource_spec(&pat_type.ty, &pat_type.attrs)
        .map_err(|message| Error::new_spanned(&pat_type.ty, message))?
        .is_some()
    {
        return Ok(());
    }
    if has_named_struct_attr(&pat_type.attrs) {
        return Ok(());
    }
    type_label(&pat_type.ty)?;
    Ok(())
}

fn validate_sync_vm_resource_borrow_conflict(
    item: &ItemFn,
    resource_params: &[(String, ResourceSpec)],
) -> Result<(), Error> {
    if item.sig.asyncness.is_some() || !item.sig.inputs.iter().any(is_mut_vm_context_param) {
        return Ok(());
    }

    for input in &item.sig.inputs {
        let FnArg::Typed(pat_type) = input else {
            continue;
        };
        let Pat::Ident(PatIdent { ident, .. }) = pat_type.pat.as_ref() else {
            continue;
        };
        let Some((_, spec)) = resource_params
            .iter()
            .find(|(name, _)| name == &ident.to_string())
        else {
            continue;
        };
        if matches!(spec.mode, ResourceMode::Borrow | ResourceMode::BorrowMut) {
            return Err(Error::new_spanned(
                &pat_type.ty,
                "synchronous host functions cannot combine `&mut Vm` with borrowed resource parameters (`ResourceRef`/`ResourceMut`); generated HostContext holds the same mutable VM borrow",
            ));
        }
    }
    Ok(())
}

/// Rejects `&mut Vm` next to hidden host state parameters.
///
/// The generated wrapper resolves hidden state through
/// `Vm::host_context()`, which holds the same mutable VM borrow for as long as
/// the state guards live. A raw `&mut Vm` parameter cannot coexist with it, so
/// the combination fails closed at compile time instead of producing an
/// unborrowable wrapper.
fn validate_sync_vm_state_conflict(
    item: &ItemFn,
    state_params: &[(String, StateSpec)],
) -> Result<(), Error> {
    if state_params.is_empty() {
        return Ok(());
    }
    for input in &item.sig.inputs {
        let FnArg::Typed(pat_type) = input else {
            continue;
        };
        if is_vm_context_type(&pat_type.ty) {
            return Err(Error::new_spanned(
                &pat_type.ty,
                "host functions cannot combine `&mut Vm` with hidden host state parameters; the generated HostContext holds the same mutable VM borrow",
            ));
        }
    }
    Ok(())
}

/// Rejects resource parameters next to hidden host state parameters.
///
/// Resource extraction resolves each resource through its own
/// `Vm::host_context()` borrow, which cannot coexist with the state
/// resolution's borrow. Keeping the two apart preserves the borrow-checked
/// wrapper shape instead of silently reaching into VM internals.
fn validate_state_resource_combination(
    state_params: &[(String, StateSpec)],
    resource_params: &[(String, ResourceSpec)],
) -> Result<(), Error> {
    if state_params.is_empty() || resource_params.is_empty() {
        return Ok(());
    }
    Err(Error::new(
        proc_macro2::Span::call_site(),
        "host functions cannot combine resource parameters with hidden host state parameters; \
         resolve the resource in a separate host call",
    ))
}

fn validate_return_type(output: &ReturnType, named_struct: bool) -> Result<(), Error> {
    match output {
        ReturnType::Default => Ok(()),
        ReturnType::Type(_, ty) => {
            if let Some(found) = borrowed_resource_return(ty) {
                let resource_name = match found.kind {
                    ResourceReturnKind::Borrow => "ResourceRef",
                    ResourceReturnKind::BorrowMut => "ResourceMut",
                    ResourceReturnKind::Owned => unreachable!(
                        "borrowed_resource_return only returns borrowed resource wrappers"
                    ),
                };
                if found.wrappers.is_empty() {
                    return Err(Error::new_spanned(
                        ty,
                        format!(
                            "{resource_name} cannot be a host function return; resource borrows cannot cross the host boundary"
                        ),
                    ));
                }
                let wrappers = found
                    .wrappers
                    .iter()
                    .map(|wrapper| format!("`{wrapper}`"))
                    .collect::<Vec<_>>()
                    .join(" -> ");
                return Err(Error::new_spanned(
                    ty,
                    format!(
                        "{resource_name} cannot appear in a host function return nested inside {wrappers}; resource borrows cannot cross the host boundary"
                    ),
                ));
            }
            if named_struct {
                return Ok(());
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

fn generate_vm_wrapper(
    item: &ItemFn,
    wrapper_name: &syn::Ident,
    guest_name: &LitStr,
    resource_params: &[(String, ResourceSpec)],
    state_params: &[(String, StateSpec)],
) -> Result<proc_macro2::TokenStream, Error> {
    let impl_name = &item.sig.ident;
    let mut wrapper_params = Vec::<proc_macro2::TokenStream>::new();
    let mut call_args = Vec::<proc_macro2::TokenStream>::new();
    let mut imm_extract_stmts = Vec::<proc_macro2::TokenStream>::new();
    let mut mut_extract_stmts = Vec::<proc_macro2::TokenStream>::new();
    let mutable_wrapper_name = syn::Ident::new(&format!("{wrapper_name}_mut"), wrapper_name.span());
    let has_vm = item.sig.inputs.iter().any(|input| match input {
        FnArg::Typed(pat_type) => is_vm_context_type(&pat_type.ty),
        FnArg::Receiver(_) => false,
    });
    let needs_vm = has_vm || !resource_params.is_empty() || !state_params.is_empty();
    if needs_vm {
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

    // Hidden host state is resolved before any guest argument is touched:
    // every state is ensured first (lazy initialization / provider conflicts),
    // then borrowed. Keeping resolution and borrowing apart lets one wrapper
    // hold several distinct state borrows from the same generic context
    // without an intermediate mutable borrow.
    let context_ident = syn::Ident::new("__pd_host_state_context", proc_macro2::Span::call_site());
    let sdk = sdk_path();
    let state_ensure_stmts = state_params
        .iter()
        .map(|(_, spec)| {
            let inner = &spec.inner;
            let label = state_effect_label(spec);
            quote! {
                #context_ident
                    .ensure_host_state::<#inner>(#guest_name, #label)
                    .map_err(|error| #sdk::VmError::HostError(error.to_string()))?;
            }
        })
        .collect::<Vec<_>>();
    let state_context_stmt = (!state_params.is_empty()).then(|| {
        quote! {
            let mut #context_ident = vm.host_context();
        }
    });

    let mut arg_index = 0usize;
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
        if let Some((_, spec)) = state_params
            .iter()
            .find(|(name, _)| name == &ident.to_string())
        {
            let inner = &spec.inner;
            let label = state_effect_label(spec);
            let accessor = if spec.is_write() {
                quote!(host_state_mut)
            } else {
                quote!(host_state_ref)
            };
            let borrow = quote! {
                let #ident = #context_ident
                    .#accessor::<#inner>(#guest_name, #label)
                    .map_err(|error| #sdk::VmError::HostError(error.to_string()))?;
            };
            imm_extract_stmts.push(borrow.clone());
            mut_extract_stmts.push(borrow);
            call_args.push(quote!(#ident));
            continue;
        }
        if let Some((_, spec)) = resource_params
            .iter()
            .find(|(name, _)| name == &ident.to_string())
        {
            let extract = resource_extract_tokens(&ident.to_string(), spec, arg_index)?;
            imm_extract_stmts.push(extract.clone());
            mut_extract_stmts.push(extract);
            call_args.push(quote!(#ident));
            arg_index += 1;
            continue;
        }
        let label = LitStr::new(
            &format!("{} {ident}", wrapper_name),
            proc_macro2::Span::call_site(),
        );
        let index = syn::Index::from(arg_index);
        imm_extract_stmts.push(quote! {
            let #ident = super::borrow_arg::<#ty>(args, #index, #label)?;
        });
        let extractor = if uses_taken_extractor(ty) {
            quote!(super::take_arg::<#ty>(args, #index, #label)?)
        } else {
            quote!(super::borrow_arg::<#ty>(&*args, #index, #label)?)
        };
        mut_extract_stmts.push(quote! {
            let #ident = #extractor;
        });
        call_args.push(quote!(#ident));
        arg_index += 1;
    }

    let wrapper_output = wrapper_output_type(&item.sig.output)?;
    let call_expr = if return_is_vm_result(&item.sig.output) {
        quote!(#impl_name(#(#call_args),*))
    } else {
        quote!(Ok(#impl_name(#(#call_args),*)))
    };

    Ok(quote! {
        #[allow(dead_code)]
        pub(crate) fn #wrapper_name(#(#imm_wrapper_params),*) -> #wrapper_output {
            #state_context_stmt
            #(#state_ensure_stmts)*
            #(#imm_extract_stmts)*
            #call_expr
        }

        #[allow(dead_code)]
        pub(crate) fn #mutable_wrapper_name(#(#mut_wrapper_params),*) -> #wrapper_output {
            #state_context_stmt
            #(#state_ensure_stmts)*
            #(#mut_extract_stmts)*
            #call_expr
        }
    })
}

/// Deterministic effect label used for host-state diagnostics.
fn state_effect_label(spec: &StateSpec) -> &'static str {
    if spec.is_write() {
        "state write"
    } else {
        "state read"
    }
}

/// Generates the extraction statement for one resource parameter.
///
/// The guest passes the raw handle as a signed integer; the wrapper decodes
/// it through the public host-context SDK and re-validates it against the
/// current execution scope before handing the typed token / borrow to the
/// impl. `TakeOwned` removes the value from the table exactly once and wraps
/// it in `ResourceOwned<T>` when that canonical parameter type is used;
/// `Borrow`/`BorrowMut` hand call-scoped borrows.
fn resource_extract_tokens(
    ident: &str,
    spec: &ResourceSpec,
    arg_index: usize,
) -> Result<proc_macro2::TokenStream, Error> {
    let sdk = sdk_path();
    let ident = syn::Ident::new(ident, proc_macro2::Span::call_site());
    let inner = &spec.inner;
    let index = syn::Index::from(arg_index);
    let handle_label = LitStr::new("resource handle", proc_macro2::Span::call_site());
    let key_ident = syn::Ident::new(
        &format!("__pd_resource_key_{ident}"),
        proc_macro2::Span::call_site(),
    );
    let key_validation = spec.key.as_ref().map(|key| {
        let key = LitStr::new(key.as_str(), proc_macro2::Span::call_site());
        quote! {
            let #key_ident = #sdk::host_api::ResourceTypeKey::new(#key)
                .map_err(|error| #sdk::VmError::HostError(error.to_string()))?;
            #sdk::resource::ResourceTable::validate_concrete_resource_type_key::<#inner>(
                &#key_ident,
            )
            .map_err(|error| #sdk::VmError::HostError(error.to_string()))?;
        }
    });

    let borrow_call = if spec.key.is_some() {
        quote! {
            .borrow_resource_with_key::<#inner>(handle, &#key_ident)
        }
    } else {
        quote! {
            .borrow_resource::<#inner>(handle)
        }
    };
    let borrow_mut_call = if spec.key.is_some() {
        quote! {
            .borrow_resource_mut_with_key::<#inner>(handle, &#key_ident)
        }
    } else {
        quote! {
            .borrow_resource_mut::<#inner>(handle)
        }
    };
    let take_call = if spec.key.is_some() {
        quote! {
            .take_resource_with_key::<#inner>(handle, &#key_ident)
        }
    } else {
        quote! {
            .take_resource::<#inner>(handle)
        }
    };

    let decode_handle = quote! {
        let raw = super::arg::<i64>(args, #index, #handle_label)?;
        let handle = #sdk::resource::ResourceHandle::from_raw(raw as u64)
            .map_err(|error| #sdk::VmError::HostError(error.to_string()))?;
    };
    let context_ident = syn::Ident::new(
        &format!("__pd_resource_context_{ident}"),
        proc_macro2::Span::call_site(),
    );
    let extraction = match spec.mode {
        ResourceMode::Borrow => quote! {
            #key_validation
            #decode_handle
            let #context_ident = vm.host_context();
            let #ident = #context_ident
                #borrow_call
                .map_err(|error| #sdk::VmError::HostError(error.to_string()))?;
        },
        ResourceMode::BorrowMut => quote! {
            #key_validation
            #decode_handle
            let mut #context_ident = vm.host_context();
            let #ident = #context_ident
                #borrow_mut_call
                .map_err(|error| #sdk::VmError::HostError(error.to_string()))?;
        },
        ResourceMode::TakeOwned => {
            let owned_value = quote! {
                vm
                    .host_context()
                    #take_call
                    .map_err(|error| #sdk::VmError::HostError(error.to_string()))?
            };
            if spec.owned_wrapper {
                quote! {
                    #key_validation
                    #decode_handle
                    let #ident = #sdk::resource::ResourceOwned::new(#owned_value);
                }
            } else {
                quote! {
                    #key_validation
                    #decode_handle
                    let #ident = #owned_value;
                }
            }
        }
        ResourceMode::Value => {
            return Err(Error::new(
                proc_macro2::Span::call_site(),
                "resource-containing Value parameters are rejected; use Borrow, BorrowMut, or TakeOwned",
            ));
        }
    };
    Ok(extraction)
}

fn generate_async_vm_wrapper(
    item: &ItemFn,
    wrapper_name: &syn::Ident,
    resource_params: &[(String, ResourceSpec)],
) -> Result<proc_macro2::TokenStream, Error> {
    let impl_name = &item.sig.ident;
    let mutable_wrapper_name = syn::Ident::new(&format!("{wrapper_name}_mut"), wrapper_name.span());
    let mut extract_stmts = Vec::<proc_macro2::TokenStream>::new();
    let mut call_args = Vec::<proc_macro2::TokenStream>::new();
    let mut arg_index = 0usize;

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
            extract_stmts.push(quote! {
                let #ident = <#ty as super::CaptureAsyncHostContext>::capture_with_args(vm, args)?;
            });
            call_args.push(quote!(#ident));
            continue;
        }
        if let Some((_, spec)) = resource_params
            .iter()
            .find(|(name, _)| name == &ident.to_string())
        {
            // Only TakeOwned may move into an owned operation; the typed token
            // is captured before the future is submitted.
            let extract = resource_extract_tokens(&ident.to_string(), spec, arg_index)?;
            extract_stmts.push(extract);
            call_args.push(quote!(#ident));
            arg_index += 1;
            continue;
        }
        let label = LitStr::new(
            &format!("{} {ident}", wrapper_name),
            proc_macro2::Span::call_site(),
        );
        let index = syn::Index::from(arg_index);
        extract_stmts.push(quote! {
            let #ident = super::borrow_arg::<#ty>(args, #index, #label)?;
        });
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
    let body = quote! {
        #(#extract_stmts)*
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

fn return_is_vm_result(output: &ReturnType) -> bool {
    vm_result_inner_type(output)
        .expect("pd_host_function return type should already be validated")
        .is_some()
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

fn type_label(ty: &Type) -> Result<String, Error> {
    pd_host_schema::type_label(ty).map_err(|message| Error::new_spanned(ty, message))
}

fn sdk_path() -> proc_macro2::TokenStream {
    match std::env::var("CARGO_CRATE_NAME").as_deref() {
        Ok("vm") => quote!(crate),
        _ => quote!(::vm),
    }
}

#[allow(clippy::too_many_arguments)]
fn generate_host_function_descriptor(
    item: &ItemFn,
    wrapper_name: &syn::Ident,
    guest_name: &LitStr,
    docs: &str,
    resource_params: &[(String, ResourceSpec)],
    state_params: &[(String, StateSpec)],
    contract: Option<&syn::Path>,
) -> Result<proc_macro2::TokenStream, Error> {
    let descriptor_name =
        syn::Ident::new(&format!("{wrapper_name}_descriptor"), wrapper_name.span());
    let adapter_name =
        syn::Ident::new(&format!("{wrapper_name}_host_adapter"), wrapper_name.span());
    let sdk = sdk_path();
    let binding = classify_generated_binding(item, resource_params, state_params);

    if let Some(contract) = contract {
        return generate_contract_host_function_descriptor(
            &sdk,
            &descriptor_name,
            &adapter_name,
            wrapper_name,
            guest_name,
            contract,
            state_params,
            binding,
            item,
        );
    }

    let mut param_tokens = Vec::new();
    let mut effect_tokens = Vec::new();
    let mut resource_meta_tokens = Vec::new();
    for input in &item.sig.inputs {
        let FnArg::Typed(pat_type) = input else {
            continue;
        };
        if is_host_context_param(input) || is_vm_context_param(input) {
            continue;
        }
        let Pat::Ident(PatIdent { ident, .. }) = pat_type.pat.as_ref() else {
            continue;
        };
        let param_name = ident.to_string();
        if let Some((_, spec)) = state_params.iter().find(|(name, _)| name == &param_name) {
            // Host-private state is hidden from guest arity, the function
            // schema, and every fingerprint; only the runtime effect records
            // it.
            let inner = &spec.inner;
            let ctor = if spec.is_write() {
                quote!(write)
            } else {
                quote!(read)
            };
            effect_tokens.push(quote! {
                #sdk::host_extension::HostEffect::HostState(
                    #sdk::host_extension::HostStateEffect::#ctor::<#inner>(),
                )
            });
            continue;
        }
        if let Some((_, spec)) = resource_params.iter().find(|(name, _)| name == &param_name) {
            let key_expr = resource_key_tokens(spec);
            let passing = match spec.mode {
                ResourceMode::Borrow => quote!(#sdk::host_extension::HostParamPassing::Borrow),
                ResourceMode::BorrowMut => {
                    quote!(#sdk::host_extension::HostParamPassing::BorrowMut)
                }
                ResourceMode::TakeOwned => {
                    quote!(#sdk::host_extension::HostParamPassing::TakeOwned)
                }
                ResourceMode::Value => quote!(#sdk::host_extension::HostParamPassing::Value),
            };
            param_tokens.push(quote! {
                #sdk::host_extension::HostParamSchema::with_passing(
                    #param_name,
                    #sdk::host_extension::HostTypeSchema::Resource(#key_expr),
                    #passing,
                )
            });
            if let Some(effect) = resource_effect_tokens(spec.mode, &key_expr) {
                effect_tokens.push(effect);
            }
            resource_meta_tokens.push(resource_meta_tokens_for(spec));
        } else {
            let schema =
                host_type_schema_tokens(&pat_type.ty, has_named_struct_attr(&pat_type.attrs))?;
            param_tokens.push(quote! {
                #sdk::host_extension::HostParamSchema::value(#param_name, #schema)
            });
        }
    }

    let named_return = has_named_struct_attr(&item.attrs);
    let (return_schema, return_effect, return_meta) =
        return_schema_tokens(&item.sig.output, named_return)?;
    if let Some(effect) = return_effect {
        effect_tokens.push(effect);
    }
    if let Some(meta) = return_meta {
        resource_meta_tokens.push(meta);
    }

    let (binding_kind, adapter, adapter_fn) = match binding {
        GeneratedBinding::Stack => (
            quote!(#sdk::host_extension::HostBindingKind::StaticStack),
            quote!(#sdk::host_extension::HostAdapterDescriptor::StaticStack(#adapter_name)),
            quote! {
                #[allow(dead_code)]
                fn #adapter_name(
                    vm: &mut #sdk::Vm,
                    args: &[#sdk::Value],
                ) -> #sdk::VmResult<#sdk::CallOutcome> {
                    #sdk::host_extension::host_descriptor_call_outcome(
                        #wrapper_name(vm, args),
                    )
                }
            },
        ),
        GeneratedBinding::Args => (
            quote!(#sdk::host_extension::HostBindingKind::StaticArgs),
            quote!(#sdk::host_extension::HostAdapterDescriptor::StaticArgs(#adapter_name)),
            quote! {
                #[allow(dead_code)]
                fn #adapter_name(
                    args: &[#sdk::Value],
                ) -> #sdk::VmResult<#sdk::CallOutcome> {
                    #sdk::host_extension::host_descriptor_call_outcome(#wrapper_name(args))
                }
            },
        ),
        GeneratedBinding::NonYieldingArgs => (
            quote!(#sdk::host_extension::HostBindingKind::StaticNonYieldingArgs),
            quote!(#sdk::host_extension::HostAdapterDescriptor::StaticNonYieldingArgs(#adapter_name)),
            quote! {
                #[allow(dead_code)]
                fn #adapter_name(
                    args: &[#sdk::Value],
                ) -> #sdk::VmResult<#sdk::CallOutcome> {
                    #sdk::host_extension::host_descriptor_call_outcome(#wrapper_name(args))
                }
            },
        ),
    };

    Ok(quote! {
        #adapter_fn

        #[allow(dead_code)]
        pub fn #descriptor_name() -> #sdk::host_extension::HostFunctionDescriptor {
            #sdk::host_extension::HostFunctionDescriptor {
                schema: #sdk::host_extension::HostFunctionSchema::with_return(
                    #guest_name,
                    vec![#(#param_tokens),*],
                    #return_schema,
                )
                .with_description(#docs),
                binding: #sdk::host_extension::HostBindingDescriptor {
                    kind: #binding_kind,
                },
                effects: vec![#(#effect_tokens),*],
                adapter: #adapter,
                resource_types: vec![#(#resource_meta_tokens),*],
            }
        }
    })
}

enum GeneratedBinding {
    Stack,
    Args,
    NonYieldingArgs,
}

/// Adapter tokens for one generated binding class.
///
/// Returns `(binding kind, adapter value, adapter function)` so the inferred
/// and contract-declared descriptor paths cannot diverge.
fn generated_adapter_tokens(
    sdk: &proc_macro2::TokenStream,
    adapter_name: &syn::Ident,
    wrapper_name: &syn::Ident,
    binding: GeneratedBinding,
) -> (
    proc_macro2::TokenStream,
    proc_macro2::TokenStream,
    proc_macro2::TokenStream,
) {
    match binding {
        GeneratedBinding::Stack => (
            quote!(#sdk::host_extension::HostBindingKind::StaticStack),
            quote!(#sdk::host_extension::HostAdapterDescriptor::StaticStack(#adapter_name)),
            quote! {
                #[allow(dead_code)]
                fn #adapter_name(
                    vm: &mut #sdk::Vm,
                    args: &[#sdk::Value],
                ) -> #sdk::VmResult<#sdk::CallOutcome> {
                    #sdk::host_extension::host_descriptor_call_outcome(
                        #wrapper_name(vm, args),
                    )
                }
            },
        ),
        GeneratedBinding::Args => (
            quote!(#sdk::host_extension::HostBindingKind::StaticArgs),
            quote!(#sdk::host_extension::HostAdapterDescriptor::StaticArgs(#adapter_name)),
            quote! {
                #[allow(dead_code)]
                fn #adapter_name(
                    args: &[#sdk::Value],
                ) -> #sdk::VmResult<#sdk::CallOutcome> {
                    #sdk::host_extension::host_descriptor_call_outcome(#wrapper_name(args))
                }
            },
        ),
        GeneratedBinding::NonYieldingArgs => (
            quote!(#sdk::host_extension::HostBindingKind::StaticNonYieldingArgs),
            quote!(#sdk::host_extension::HostAdapterDescriptor::StaticNonYieldingArgs(#adapter_name)),
            quote! {
                #[allow(dead_code)]
                fn #adapter_name(
                    args: &[#sdk::Value],
                ) -> #sdk::VmResult<#sdk::CallOutcome> {
                    #sdk::host_extension::host_descriptor_call_outcome(#wrapper_name(args))
                }
            },
        ),
    }
}

/// Descriptor tokens for a function that declares its guest schema explicitly.
///
/// The contract supplies the guest-facing parameter and return schemas; the
/// runtime adapter, binding class, and hidden host-state effects still come
/// from this one macro expansion. Guest resource effects are derived from the
/// contract schema, so a raw-handle signature cannot drift from the declared
/// guest contract. Resource type *declarations* stay at the module level: a
/// contract schema that names a resource key must be paired with a
/// `HostResourceType` implementation contributed by the owning module, and
/// catalog construction fails closed when a key has no declaration.
#[allow(clippy::too_many_arguments)]
fn generate_contract_host_function_descriptor(
    sdk: &proc_macro2::TokenStream,
    descriptor_name: &syn::Ident,
    adapter_name: &syn::Ident,
    wrapper_name: &syn::Ident,
    guest_name: &LitStr,
    contract: &syn::Path,
    state_params: &[(String, StateSpec)],
    binding: GeneratedBinding,
    item: &ItemFn,
) -> Result<proc_macro2::TokenStream, Error> {
    let mut state_effect_tokens = Vec::new();
    for input in &item.sig.inputs {
        let FnArg::Typed(pat_type) = input else {
            continue;
        };
        let Pat::Ident(PatIdent { ident, .. }) = pat_type.pat.as_ref() else {
            continue;
        };
        let param_name = ident.to_string();
        let Some((_, spec)) = state_params.iter().find(|(name, _)| name == &param_name) else {
            continue;
        };
        let inner = &spec.inner;
        let ctor = if spec.is_write() {
            quote!(write)
        } else {
            quote!(read)
        };
        state_effect_tokens.push(quote! {
            #sdk::host_extension::HostEffect::HostState(
                #sdk::host_extension::HostStateEffect::#ctor::<#inner>(),
            )
        });
    }

    let (binding_kind, adapter, adapter_fn) =
        generated_adapter_tokens(sdk, adapter_name, wrapper_name, binding);

    Ok(quote! {
        #adapter_fn

        #[allow(dead_code)]
        pub fn #descriptor_name() -> #sdk::host_extension::HostFunctionDescriptor {
            let schema = #sdk::host_extension::declared_host_contract(#contract(), #guest_name);
            let mut effects: Vec<#sdk::host_extension::HostEffect> = vec![#(#state_effect_tokens),*];
            effects.extend(#sdk::host_extension::guest_resource_effects(&schema));
            #sdk::host_extension::HostFunctionDescriptor {
                schema,
                binding: #sdk::host_extension::HostBindingDescriptor {
                    kind: #binding_kind,
                },
                effects,
                adapter: #adapter,
                resource_types: Vec::new(),
            }
        }
    })
}

fn classify_generated_binding(
    item: &ItemFn,
    resource_params: &[(String, ResourceSpec)],
    state_params: &[(String, StateSpec)],
) -> GeneratedBinding {
    let is_async = item.sig.asyncness.is_some();
    let has_vm = item.sig.inputs.iter().any(is_vm_context_param);
    if is_async || has_vm || !resource_params.is_empty() || !state_params.is_empty() {
        return GeneratedBinding::Stack;
    }
    match &item.sig.output {
        ReturnType::Default => GeneratedBinding::NonYieldingArgs,
        ReturnType::Type(_, ty) => {
            if is_call_outcome_return(ty) {
                GeneratedBinding::Args
            } else if is_supported_ordinary_return_type(ty) {
                GeneratedBinding::NonYieldingArgs
            } else {
                GeneratedBinding::Args
            }
        }
    }
}

fn type_path_ends_with(ty: &Type, name: &str) -> bool {
    match ty {
        Type::Group(group) => type_path_ends_with(&group.elem, name),
        Type::Paren(paren) => type_path_ends_with(&paren.elem, name),
        Type::Path(path) => path
            .path
            .segments
            .last()
            .is_some_and(|segment| segment.ident == name),
        _ => false,
    }
}

fn sole_type_argument(ty: &Type) -> Option<&Type> {
    match ty {
        Type::Group(group) => sole_type_argument(&group.elem),
        Type::Paren(paren) => sole_type_argument(&paren.elem),
        Type::Path(path) => path.path.segments.last().and_then(first_type_arg),
        _ => None,
    }
}

fn is_call_outcome_return(ty: &Type) -> bool {
    if type_path_ends_with(ty, "CallOutcome") {
        return true;
    }
    sole_type_argument(ty)
        .filter(|_| type_path_ends_with(ty, "VmResult") || type_path_ends_with(ty, "HostResult"))
        .is_some_and(is_call_outcome_return)
}

fn is_supported_ordinary_return_type(ty: &Type) -> bool {
    match ty {
        Type::Group(group) => is_supported_ordinary_return_type(&group.elem),
        Type::Paren(paren) => is_supported_ordinary_return_type(&paren.elem),
        Type::Tuple(tuple) if tuple.elems.is_empty() => true,
        Type::Path(path) => {
            let Some(segment) = path.path.segments.last() else {
                return false;
            };
            match segment.ident.to_string().as_str() {
                "i64" | "f64" | "bool" | "String" | "Value" | "HostString" | "HostValue"
                | "VmString" | "VmValue" => true,
                "Vec" => sole_type_argument(ty).is_some_and(|inner| {
                    type_path_ends_with(inner, "u8") || type_path_ends_with(inner, "Value")
                }),
                "HostBytes" | "VmBytes" | "HostArray" | "VmArray" => true,
                "Option" => sole_type_argument(ty).is_some_and(is_supported_ordinary_return_type),
                "Result" | "VmResult" | "HostResult" => {
                    sole_type_argument(ty).is_some_and(is_supported_ordinary_return_type)
                }
                _ => false,
            }
        }
        _ => false,
    }
}

fn has_named_struct_attr(attrs: &[syn::Attribute]) -> bool {
    attrs
        .iter()
        .any(|attr| attr.path().is_ident("pd_host_named_struct"))
}

fn resource_key_tokens(spec: &ResourceSpec) -> proc_macro2::TokenStream {
    let sdk = sdk_path();
    if let Some(key) = &spec.key {
        quote! {
            #sdk::host_extension::ResourceTypeKey::new(#key).expect("validated resource type key")
        }
    } else {
        let inner = &spec.inner;
        quote! {
            #sdk::host_extension::ResourceTypeKey::new(
                <#inner as #sdk::host_extension::HostResourceType>::KEY,
            )
            .expect("validated resource type key")
        }
    }
}

fn resource_meta_tokens_for(spec: &ResourceSpec) -> proc_macro2::TokenStream {
    let sdk = sdk_path();
    let inner = &spec.inner;
    if let Some(key) = &spec.key {
        quote! {
            #sdk::host_extension::compatible_resource_type_meta::<#inner>(#key)
        }
    } else {
        quote! {
            #sdk::host_extension::HostResourceTypeMeta::of::<#inner>()
        }
    }
}

fn resource_effect_tokens(
    mode: ResourceMode,
    key_expr: &proc_macro2::TokenStream,
) -> Option<proc_macro2::TokenStream> {
    let ctor = match mode {
        ResourceMode::Borrow => quote!(borrow),
        ResourceMode::BorrowMut => quote!(borrow_mut),
        ResourceMode::TakeOwned => quote!(take_owned),
        ResourceMode::Value => return None,
    };
    let sdk = sdk_path();
    Some(quote! {
        #sdk::host_extension::HostEffect::GuestResource(#sdk::host_extension::ResourceEffect::#ctor(#key_expr))
    })
}

fn return_schema_tokens(
    output: &ReturnType,
    named_return: bool,
) -> Result<
    (
        proc_macro2::TokenStream,
        Option<proc_macro2::TokenStream>,
        Option<proc_macro2::TokenStream>,
    ),
    Error,
> {
    let sdk = sdk_path();
    let ReturnType::Type(_, ty) = output else {
        return Ok((
            quote!(#sdk::host_extension::HostTypeSchema::Null),
            None,
            None,
        ));
    };
    let surface = unwrap_transparent_return(ty);
    if resource_return_kind(surface) == Some(ResourceReturnKind::Owned) {
        let inner = last_generic_type(surface).ok_or_else(|| {
            Error::new_spanned(surface, "Resource return must have a type argument")
        })?;
        let key_expr = quote! {
            #sdk::host_extension::ResourceTypeKey::new(
                <#inner as #sdk::host_extension::HostResourceType>::KEY,
            )
            .expect("validated resource type key")
        };
        return Ok((
            quote!(#sdk::host_extension::HostTypeSchema::Resource(#key_expr)),
            Some(quote! {
                #sdk::host_extension::HostEffect::GuestResource(
                    #sdk::host_extension::ResourceEffect::create(#key_expr)
                )
            }),
            Some(quote! {
                #sdk::host_extension::HostResourceTypeMeta::of::<#inner>()
            }),
        ));
    }
    Ok((host_type_schema_tokens(surface, named_return)?, None, None))
}

fn unwrap_transparent_return(ty: &Type) -> &Type {
    match ty {
        Type::Group(group) => unwrap_transparent_return(&group.elem),
        Type::Paren(paren) => unwrap_transparent_return(&paren.elem),
        Type::Path(path) => {
            let Some(segment) = path.path.segments.last() else {
                return ty;
            };
            match segment.ident.to_string().as_str() {
                "VmResult" | "HostCallResult" | "HostFutureOutput" => first_type_arg(segment)
                    .map(unwrap_transparent_return)
                    .unwrap_or(ty),
                _ => ty,
            }
        }
        _ => ty,
    }
}

fn first_type_arg(segment: &syn::PathSegment) -> Option<&Type> {
    let PathArguments::AngleBracketed(args) = &segment.arguments else {
        return None;
    };
    args.args.iter().find_map(|argument| match argument {
        GenericArgument::Type(ty) => Some(ty),
        _ => None,
    })
}

fn last_generic_type(ty: &Type) -> Option<&Type> {
    let Type::Path(path) = ty else {
        return None;
    };
    let segment = path.path.segments.last()?;
    let PathArguments::AngleBracketed(args) = &segment.arguments else {
        return None;
    };
    args.args.iter().rev().find_map(|argument| match argument {
        GenericArgument::Type(inner) => Some(inner),
        _ => None,
    })
}

fn host_type_schema_tokens(
    ty: &Type,
    named_struct: bool,
) -> Result<proc_macro2::TokenStream, Error> {
    let sdk = sdk_path();
    if named_struct {
        return Ok(quote!(<#ty as #sdk::host_extension::HostNamedStruct>::host_type_schema()));
    }
    let label = type_label(ty)?;
    host_type_schema_from_label(&label, ty)
}

fn host_type_schema_from_label(label: &str, _ty: &Type) -> Result<proc_macro2::TokenStream, Error> {
    let sdk = sdk_path();
    if let Some(inner) = label.strip_suffix(" | null") {
        let inner_tokens = host_type_schema_from_label(inner, _ty)?;
        return Ok(quote!(#sdk::host_extension::HostTypeSchema::Optional(Box::new(#inner_tokens))));
    }
    Ok(match label {
        "int" => quote!(#sdk::host_extension::HostTypeSchema::Int),
        "float" => quote!(#sdk::host_extension::HostTypeSchema::Float),
        "bool" => quote!(#sdk::host_extension::HostTypeSchema::Bool),
        "string" => quote!(#sdk::host_extension::HostTypeSchema::String),
        "bytes" => quote!(#sdk::host_extension::HostTypeSchema::Bytes),
        "number" => quote!(#sdk::host_extension::HostTypeSchema::Number),
        "null" => quote!(#sdk::host_extension::HostTypeSchema::Null),
        "any" | "unknown" => quote!(#sdk::host_extension::HostTypeSchema::Unknown),
        "array" => quote!(#sdk::host_extension::HostTypeSchema::Array(Box::new(
            #sdk::host_extension::HostTypeSchema::Unknown
        ))),
        "map" => quote!(#sdk::host_extension::HostTypeSchema::Map(Box::new(
            #sdk::host_extension::HostTypeSchema::Unknown
        ))),
        _ => quote!(#sdk::host_extension::HostTypeSchema::Unknown),
    })
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
    fn async_callable_wrapper_accepts_owned_bare_function_schema() {
        let attr: Punctuated<Meta, Token![,]> = parse_quote!(name = "test::async_stream");
        let item: ItemFn = parse_quote! {
            /// Streams through an owned callback asynchronously.
            async fn async_stream(
                callback: VmCallable<fn(VmMap) -> VmMap>,
            ) -> VmResult<HostFutureOutput<VmMap>> {
                todo!()
            }
        };

        let expanded = expand_pd_host_function(attr, item)
            .expect("an owned callable wrapper may cross the async boundary")
            .to_string();
        assert!(expanded.contains("VmCallable < fn (VmMap) -> VmMap >"));
        assert!(expanded.contains("submit_host_future"));
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

    #[test]
    fn take_owned_resource_param_generates_owned_extraction() {
        let attr: Punctuated<Meta, Token![,]> = parse_quote!(name = "test::use_counter");
        let item: ItemFn = parse_quote! {
            /// Reads a counter resource by owned value.
            fn use_counter(
                #[pd_host_resource(passing = "take_owned", key = "demo.counter")]
                counter: ResourceOwned<Counter>,
            ) -> VmResult<CallOutcome> {
                todo!()
            }
        };
        let expanded = expand_pd_host_function(attr, item).unwrap().to_string();
        assert!(expanded.contains("take_resource"));
        assert!(expanded.contains("ResourceOwned :: new"));
        assert!(expanded.contains("ResourceHandle :: from_raw"));
        assert!(expanded.contains("host_context"));
    }

    #[test]
    fn resource_parameter_adds_vm_to_wrapper_and_preserves_shared_mode() {
        let attr: Punctuated<Meta, Token![,]> = parse_quote!(name = "test::peek_counter");
        let item: ItemFn = parse_quote! {
            /// Peeks a counter resource by immutable borrow.
            fn peek_counter(
                #[pd_host_resource(passing = "borrow", key = "demo.counter")]
                counter: ResourceRef<'_, Counter>,
            ) -> VmResult<i64> {
                todo!()
            }
        };
        let expanded = expand_pd_host_function(attr, item).unwrap().to_string();
        assert!(expanded.contains("vm : & mut super :: super :: Vm"));
        assert!(expanded.contains("borrow_resource"));
        assert!(expanded.contains("ResourceHandle :: from_raw"));
    }

    #[test]
    fn contract_argument_declares_schema_effects_and_adapter_in_one_expansion() {
        let attr: Punctuated<Meta, Token![,]> =
            parse_quote!(name = "io::open", contract = io_open_contract);
        let item: ItemFn = parse_quote! {
            /// Opens a raw handle whose guest contract is declared explicitly.
            fn open(vm: &mut Vm, path: &str) -> VmResult<i64> {
                todo!()
            }
        };
        let expanded = expand_pd_host_function(attr, item).unwrap().to_string();
        assert!(
            expanded.contains("declared_host_contract"),
            "a declared contract must route through the validated contract boundary: {expanded}"
        );
        assert!(
            expanded.contains("guest_resource_effects"),
            "guest resource effects must derive from the declared contract schema: {expanded}"
        );
        assert!(
            expanded.contains("resource_types : Vec :: new"),
            "a raw-handle contract contributes no typed resource metadata of its own: {expanded}"
        );
        assert!(
            expanded.contains("HostBindingKind :: StaticStack"),
            "the adapter/binding class still comes from the Rust signature: {expanded}"
        );
        assert!(
            expanded.contains("open_descriptor"),
            "the descriptor factory name is unchanged: {expanded}"
        );
    }

    #[test]
    fn contract_argument_keeps_hidden_state_effects() {
        let attr: Punctuated<Meta, Token![,]> =
            parse_quote!(name = "re::match", contract = re_match_contract);
        let item: ItemFn = parse_quote! {
            /// Uses hidden module state while declaring its guest contract.
            fn re_match(
                cache: HostStateMut<RegexCache>,
                pattern: &str,
            ) -> VmResult<bool> {
                todo!()
            }
        };
        let expanded = expand_pd_host_function(attr, item).unwrap().to_string();
        assert!(
            expanded.contains("HostStateEffect :: write :: < RegexCache >"),
            "hidden host state effects must survive a declared contract: {expanded}"
        );
        assert!(
            expanded.contains("declared_host_contract"),
            "the declared contract must still be used: {expanded}"
        );
    }

    #[test]
    fn contract_argument_must_name_a_path() {
        let attr: Punctuated<Meta, Token![,]> =
            parse_quote!(name = "test::open", contract = "not-a-path");
        let item: ItemFn = parse_quote! {
            /// A literal is not a schema callable.
            fn open() -> VmResult<i64> {
                todo!()
            }
        };
        let error = expand_pd_host_function(attr, item)
            .expect_err("a contract must be a schema callable path");
        assert!(
            error
                .to_string()
                .contains("zero-argument guest-schema callable")
        );
    }

    #[test]
    fn contract_argument_conflicts_with_named_struct_attribute() {
        let attr: Punctuated<Meta, Token![,]> =
            parse_quote!(name = "test::open", contract = io_open_contract);
        let item: ItemFn = parse_quote! {
            /// The contract already carries the named return.
            #[pd_host_named_struct]
            fn open() -> VmResult<VmMap> {
                todo!()
            }
        };
        let error = expand_pd_host_function(attr, item)
            .expect_err("contract and named-struct attribute are mutually exclusive");
        assert!(
            error
                .to_string()
                .contains("redundant with a declared contract")
        );
    }

    #[test]
    fn unknown_function_attribute_argument_is_rejected() {
        let attr: Punctuated<Meta, Token![,]> =
            parse_quote!(name = "test::open", schema_path = io_open_contract);
        let item: ItemFn = parse_quote! {
            /// Unknown arguments must fail closed.
            fn open() -> VmResult<i64> {
                todo!()
            }
        };
        let error = expand_pd_host_function(attr, item)
            .expect_err("unknown #[pd_host_function] arguments must be rejected");
        assert!(error.to_string().contains("contract = <path>"));
    }

    #[test]
    fn duplicate_name_argument_is_rejected() {
        let attr: Punctuated<Meta, Token![,]> = parse_quote!(name = "test::a", name = "test::b");
        let item: ItemFn = parse_quote! {
            /// Duplicate names must fail closed.
            fn a() -> VmResult<i64> {
                todo!()
            }
        };
        let error = expand_pd_host_function(attr, item)
            .expect_err("duplicate name arguments must be rejected");
        assert!(error.to_string().contains("duplicate `name"));
    }

    #[test]
    fn resource_function_generates_host_function_descriptor() {
        let attr: Punctuated<Meta, Token![,]> = parse_quote!(name = "test::peek_counter");
        let item: ItemFn = parse_quote! {
            /// Peeks a counter resource by immutable borrow.
            fn peek_counter(
                #[pd_host_resource(passing = "borrow", key = "demo.counter")]
                counter: ResourceRef<'_, Counter>,
            ) -> VmResult<i64> {
                todo!()
            }
        };
        let expanded = expand_pd_host_function(attr, item).unwrap().to_string();
        assert!(expanded.contains("HostFunctionDescriptor"));
        assert!(expanded.contains("peek_counter_descriptor"));
        assert!(expanded.contains("ResourceEffect"));
        assert!(expanded.contains("GuestResource"));
        assert!(expanded.contains("HostBindingKind :: StaticStack"));
        assert!(expanded.contains("demo.counter"));
    }

    #[test]
    fn borrow_resource_with_vm_param_is_rejected_before_generation() {
        let attr: Punctuated<Meta, Token![,]> = parse_quote!(name = "test::peek_counter");
        let item: ItemFn = parse_quote! {
            /// A borrowed resource cannot share the mutable VM parameter.
            fn peek_counter(
                vm: &mut Vm,
                #[pd_host_resource(passing = "borrow", key = "demo.counter")]
                counter: ResourceRef<'_, Counter>,
            ) -> VmResult<i64> {
                todo!()
            }
        };
        let error = expand_pd_host_function(attr, item)
            .expect_err("a mutable VM and borrowed resource cannot share a wrapper");
        assert!(error.to_string().contains("cannot combine"));
        assert!(error.to_string().contains("HostContext"));
    }

    #[test]
    fn borrow_mut_resource_param_generates_mut_borrow_extraction() {
        let attr: Punctuated<Meta, Token![,]> = parse_quote!(name = "test::bump_counter");
        let item: ItemFn = parse_quote! {
            /// Bumps a counter resource by mutable borrow.
            fn bump_counter(
                #[pd_host_resource(passing = "borrow_mut", key = "demo.counter")]
                counter: ResourceMut<'_, Counter>,
            ) -> VmResult<i64> {
                todo!()
            }
        };
        let expanded = expand_pd_host_function(attr, item).unwrap().to_string();
        assert!(expanded.contains("borrow_resource_mut"));
    }

    #[test]
    fn resource_value_passing_rejected() {
        let attr: Punctuated<Meta, Token![,]> = parse_quote!(name = "test::bad_value");
        let item: ItemFn = parse_quote! {
            /// A resource passed by value must be rejected.
            fn bad_value(
                #[pd_host_resource(passing = "value", key = "demo.counter")]
                counter: Resource<Counter>,
            ) -> VmResult<i64> {
                todo!()
            }
        };
        let error = expand_pd_host_function(attr, item)
            .expect_err("resource Value passing must be rejected");
        assert!(error.to_string().contains("Value"));
    }

    #[test]
    fn async_borrow_resource_rejected() {
        let attr: Punctuated<Meta, Token![,]> = parse_quote!(name = "test::async_borrow");
        let item: ItemFn = parse_quote! {
            /// A borrowed resource cannot cross an async boundary.
            async fn async_borrow(
                #[pd_host_resource(passing = "borrow", key = "demo.counter")]
                counter: ResourceRef<'_, Counter>,
            ) -> VmResult<String> {
                todo!()
            }
        };
        let error = expand_pd_host_function(attr, item)
            .expect_err("async resource borrows must be rejected");
        assert!(error.to_string().contains("cannot cross async"));
    }

    #[test]
    fn resource_ref_return_rejected() {
        let attr: Punctuated<Meta, Token![,]> = parse_quote!(name = "test::bad_return");
        let item: ItemFn = parse_quote! {
            /// A resource borrow return must be rejected.
            fn bad_return(value: i64) -> ResourceRef<'_, Counter> {
                todo!()
            }
        };
        let error =
            expand_pd_host_function(attr, item).expect_err("ResourceRef return must be rejected");
        assert!(error.to_string().contains("ResourceRef"));
    }

    #[test]
    fn typed_resource_descriptor_uses_host_resource_type_not_empty_description() {
        let attr: Punctuated<Meta, Token![,]> = parse_quote!(name = "demo::read_counter");
        let item: ItemFn = parse_quote! {
            /// Reads a typed counter.
            fn read_counter(counter: ResourceRef<'_, Counter>) -> i64 {
                counter.0
            }
        };
        let expanded = expand_pd_host_function(attr, item)
            .expect("typed ResourceRef should generate a descriptor")
            .to_string();
        assert!(
            expanded.contains("HostResourceTypeMeta :: of"),
            "typed wrappers must use HostResourceTypeMeta::of: {expanded}"
        );
        assert!(
            expanded.contains("HostResourceType"),
            "typed wrappers must derive catalog key/description from HostResourceType: {expanded}"
        );
        assert!(
            !expanded.contains("HostResource :: resource_type_key"),
            "typed wrappers must not use HostResource::resource_type_key for catalog metadata: {expanded}"
        );
        assert!(
            !expanded.contains("resource_type_key () . expect"),
            "typed wrappers must not expect() a HostResource key: {expanded}"
        );
    }

    #[test]
    fn async_function_emits_descriptor_with_static_stack_adapter() {
        let attr: Punctuated<Meta, Token![,]> = parse_quote!(name = "demo::delay");
        let item: ItemFn = parse_quote! {
            /// Yields then returns.
            async fn delay(#[pd_host_context] context: HostContext, ticks: i64) -> i64 {
                let _ = context;
                ticks
            }
        };
        let expanded = expand_pd_host_function(attr, item)
            .expect("async functions must emit descriptors")
            .to_string();
        assert!(
            expanded.contains("fn delay_descriptor"),
            "async expansion must include a descriptor factory: {expanded}"
        );
        assert!(
            expanded.contains("HostBindingKind :: StaticStack"),
            "async VM-aware functions must select StaticStack: {expanded}"
        );
        assert!(
            expanded.contains("HostParamSchema :: value (\"ticks\""),
            "async descriptor guest schema must include ticks: {expanded}"
        );
        assert!(
            !expanded.contains("HostParamSchema :: value (\"context\""),
            "async descriptor guest arity must exclude host context: {expanded}"
        );
    }

    #[test]
    fn args_only_ordinary_return_selects_static_non_yielding_args() {
        let attr: Punctuated<Meta, Token![,]> = parse_quote!(name = "demo::add");
        let item: ItemFn = parse_quote! {
            /// Adds two integers.
            fn add(lhs: i64, rhs: i64) -> i64 {
                lhs + rhs
            }
        };
        let expanded = expand_pd_host_function(attr, item)
            .expect("ordinary args-only functions must emit descriptors")
            .to_string();
        assert!(
            expanded.contains("HostBindingKind :: StaticNonYieldingArgs"),
            "proven one-value ordinary returns must select StaticNonYieldingArgs: {expanded}"
        );
        assert!(
            !expanded.contains("HostBindingKind :: StaticArgs"),
            "ordinary non-yielding returns must not be installed as StaticArgs: {expanded}"
        );
    }

    #[test]
    fn call_outcome_args_only_selects_static_args() {
        let attr: Punctuated<Meta, Token![,]> = parse_quote!(name = "demo::maybe_yield");
        let item: ItemFn = parse_quote! {
            /// May suspend.
            fn maybe_yield(flag: bool) -> crate::vm::CallOutcome {
                let _ = flag;
                crate::vm::CallOutcome::Return(crate::vm::CallReturn::None)
            }
        };
        let expanded = expand_pd_host_function(attr, item)
            .expect("CallOutcome functions must emit descriptors")
            .to_string();
        assert!(
            expanded.contains("HostBindingKind :: StaticArgs"),
            "args-only CallOutcome must select StaticArgs: {expanded}"
        );
        assert!(
            !expanded.contains("HostBindingKind :: StaticNonYieldingArgs"),
            "suspension-capable returns must not select StaticNonYieldingArgs: {expanded}"
        );
    }

    #[test]
    fn generated_descriptor_uses_public_vm_sdk_paths() {
        let attr: Punctuated<Meta, Token![,]> = parse_quote!(name = "demo::add");
        let item: ItemFn = parse_quote! {
            /// Adds two integers.
            fn add(lhs: i64, rhs: i64) -> i64 {
                lhs + rhs
            }
        };
        let expanded = expand_pd_host_function(attr, item)
            .expect("value-only functions must emit descriptors")
            .to_string();
        assert!(
            expanded.contains(":: vm ::"),
            "generated descriptors must use the public vm crate path: {expanded}"
        );
        assert!(
            !expanded.contains("crate :: host_api"),
            "generated descriptors must not hardcode crate::host_api: {expanded}"
        );
        assert!(
            !expanded.contains("crate :: vm ::"),
            "generated descriptors must not hardcode crate::vm: {expanded}"
        );
        assert!(
            !expanded.contains("crate :: resource"),
            "generated descriptors must not hardcode crate::resource: {expanded}"
        );
    }

    #[test]
    fn named_struct_attr_uses_host_named_struct_schema() {
        let attr: Punctuated<Meta, Token![,]> = parse_quote!(name = "geo::origin");
        let item: ItemFn = parse_quote! {
            /// Returns a named point.
            #[pd_host_named_struct]
            fn origin() -> Point {
                todo!()
            }
        };
        let expanded = expand_pd_host_function(attr, item)
            .expect("named-struct returns must emit descriptors")
            .to_string();
        assert!(
            expanded.contains("HostNamedStruct"),
            "named-struct returns must use HostNamedStruct: {expanded}"
        );
        assert!(
            !expanded.contains("pd_host_named_struct"),
            "named-struct attribute must be stripped: {expanded}"
        );
    }

    #[test]
    fn hidden_state_parameter_is_omitted_from_guest_arity_and_declared_as_write() {
        let attr: Punctuated<Meta, Token![,]> = parse_quote!(name = "demo::cached");
        let item: ItemFn = parse_quote! {
            /// Reads a cached value through per-VM host state.
            fn cached(cache: HostStateMut<'_, Cache>, key: String) -> VmResult<i64> {
                let _ = (cache, key);
                todo!()
            }
        };
        let expanded = expand_pd_host_function(attr, item)
            .expect("a hidden state parameter must expand")
            .to_string();
        assert!(
            expanded.contains("HostParamSchema :: value (\"key\""),
            "guest parameters must stay in the schema: {expanded}"
        );
        assert!(
            !expanded.contains("\"cache\""),
            "a hidden state parameter must never appear in the guest schema: {expanded}"
        );
        assert!(
            expanded.contains("HostStateEffect :: write :: < Cache >"),
            "a mutable hidden state parameter must declare a write effect: {expanded}"
        );
        assert!(
            !expanded.contains("HostStateEffect :: read ::"),
            "a write-only state parameter must not declare a read effect: {expanded}"
        );
        assert!(
            expanded.contains("HostBindingKind :: StaticStack"),
            "state-resolving hosts must use the vm-aware stack adapter: {expanded}"
        );
        assert!(
            expanded.contains("ensure_host_state"),
            "generated wrappers must resolve state through the generic table: {expanded}"
        );
        assert!(
            expanded.contains("host_state_mut"),
            "generated wrappers must borrow the resolved state: {expanded}"
        );
        assert!(
            !expanded.contains("cached_impl (vm"),
            "a state-only host impl must not take a raw Vm parameter: {expanded}"
        );
    }

    #[test]
    fn hidden_state_read_parameter_declares_a_read_effect() {
        let attr: Punctuated<Meta, Token![,]> = parse_quote!(name = "demo::peek_cache");
        let item: ItemFn = parse_quote! {
            /// Peeks per-VM host state.
            fn peek_cache(cache: HostStateRef<'_, Cache>) -> VmResult<i64> {
                let _ = cache;
                todo!()
            }
        };
        let expanded = expand_pd_host_function(attr, item)
            .expect("a shared hidden state parameter must expand")
            .to_string();
        assert!(
            expanded.contains("HostStateEffect :: read :: < Cache >"),
            "a shared hidden state parameter must declare a read effect: {expanded}"
        );
        assert!(
            !expanded.contains("HostStateEffect :: write ::"),
            "a read-only state parameter must not declare a write effect: {expanded}"
        );
        assert!(
            expanded.contains("host_state_ref"),
            "generated wrappers must borrow shared state through the generic table: {expanded}"
        );
        assert!(
            expanded.contains("HostFunctionSchema :: with_return"),
            "the descriptor must still be generated: {expanded}"
        );
        assert!(
            expanded.contains("vec ! []") || !expanded.contains("HostParamSchema"),
            "a state-only host must expose an empty guest parameter list: {expanded}"
        );
    }

    #[test]
    fn async_hidden_state_parameter_is_rejected() {
        let attr: Punctuated<Meta, Token![,]> = parse_quote!(name = "demo::async_cache");
        let item: ItemFn = parse_quote! {
            /// Attempts to borrow per-VM state across a yield.
            async fn async_cache(cache: HostStateMut<'_, Cache>) -> VmResult<i64> {
                let _ = cache;
                todo!()
            }
        };
        let error = expand_pd_host_function(attr, item)
            .expect_err("a hidden state borrow must not cross async/yield");
        assert!(
            error.to_string().contains("async"),
            "the diagnostic must explain the async restriction: {error}"
        );
    }

    #[test]
    fn hidden_state_parameter_with_mutable_vm_is_rejected() {
        let attr: Punctuated<Meta, Token![,]> = parse_quote!(name = "demo::cached_with_vm");
        let item: ItemFn = parse_quote! {
            /// Attempts to combine a raw VM borrow with resolved host state.
            fn cached_with_vm(vm: &mut Vm, cache: HostStateMut<'_, Cache>) -> VmResult<i64> {
                let _ = (vm, cache);
                todo!()
            }
        };
        let error = expand_pd_host_function(attr, item)
            .expect_err("a mutable VM and resolved host state cannot share a wrapper");
        assert!(
            error.to_string().contains("HostContext"),
            "the diagnostic must explain the shared mutable borrow: {error}"
        );
    }

    #[test]
    fn multiple_hidden_state_parameters_expand_to_distinct_resolutions() {
        let attr: Punctuated<Meta, Token![,]> = parse_quote!(name = "demo::two_states");
        let item: ItemFn = parse_quote! {
            /// Uses two different per-VM states.
            fn two_states(
                first: HostStateMut<'_, FirstCache>,
                second: HostStateRef<'_, SecondCache>,
                key: String,
            ) -> VmResult<i64> {
                let _ = (first, second, key);
                todo!()
            }
        };
        let expanded = expand_pd_host_function(attr, item)
            .expect("multiple hidden state parameters must expand")
            .to_string();
        assert!(
            expanded.contains("FirstCache") && expanded.contains("SecondCache"),
            "both state types must be resolved: {expanded}"
        );
        assert!(
            expanded.contains("HostStateEffect :: write :: < FirstCache >")
                && expanded.contains("HostStateEffect :: read :: < SecondCache >"),
            "each state parameter keeps its own access mode: {expanded}"
        );
        assert!(
            !expanded.contains("\"first\"") && !expanded.contains("\"second\""),
            "hidden state parameters must not leak into the guest arity: {expanded}"
        );
    }
}
