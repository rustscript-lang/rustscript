use proc_macro::TokenStream;
use quote::quote;
use syn::{
    Error, FnArg, ItemFn, LitStr, Meta, Pat, PatIdent, ReturnType, Token, Type, parse_macro_input,
    punctuated::Punctuated,
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
    parse_name_arg(&attr)?;
    let docs = doc_string(&item.attrs);
    for input in &item.sig.inputs {
        validate_param(input)?;
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
    let wrapper = generate_vm_wrapper(&item, &wrapper_name)?;
    Ok(quote! {
        #item
        #wrapper
    })
}

fn parse_name_arg(args: &Punctuated<Meta, Token![,]>) -> Result<LitStr, Error> {
    let Some(Meta::NameValue(name_value)) = args.first() else {
        return Err(Error::new(
            proc_macro2::Span::call_site(),
            "expected #[pd_host_function(name = \"...\")]",
        ));
    };
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
) -> Result<proc_macro2::TokenStream, Error> {
    let impl_name = &item.sig.ident;
    let mut wrapper_params = Vec::<proc_macro2::TokenStream>::new();
    let mut call_args = Vec::<proc_macro2::TokenStream>::new();
    let mut extract_stmts = Vec::<proc_macro2::TokenStream>::new();

    let has_vm = item.sig.inputs.iter().any(|input| match input {
        FnArg::Typed(pat_type) => is_vm_context_type(&pat_type.ty),
        FnArg::Receiver(_) => false,
    });
    if has_vm {
        wrapper_params.push(quote!(vm: &mut super::super::Vm));
        call_args.push(quote!(vm));
    }
    wrapper_params.push(quote!(args: &[super::super::Value]));

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
        let label = LitStr::new(
            &format!("{} {}", wrapper_name, ident),
            proc_macro2::Span::call_site(),
        );
        let index = syn::Index::from(arg_index);
        extract_stmts.push(quote! {
            let #ident = super::arg::<#ty>(args, #index, #label)?;
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
        pub(super) fn #wrapper_name(#(#wrapper_params),*) -> #wrapper_output {
            #(#extract_stmts)*
            #call_expr
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
            if !matches!(
                segment.ident.to_string().as_str(),
                "VmResult" | "HostResult"
            ) {
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

fn type_label(ty: &Type) -> Result<String, Error> {
    match ty {
        Type::Group(group) => type_label(&group.elem),
        Type::Paren(paren) => type_label(&paren.elem),
        Type::Reference(reference) => type_label(&reference.elem),
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
                "String" | "str" => Ok("string".to_string()),
                "Any" | "AnyValue" | "Value" => Ok("any".to_string()),
                "Array" | "VmArray" => Ok("array".to_string()),
                "Map" | "VmMap" => Ok("map".to_string()),
                "Number" | "NumberValue" => Ok("number".to_string()),
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
                "VmResult" | "BuiltinResult" | "HostResult" => {
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
