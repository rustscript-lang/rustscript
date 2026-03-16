use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use syn::{Attribute, FnArg, Item, Meta, Pat, ReturnType, Type};

#[derive(Clone, Debug)]
struct AbiFunctionDecl {
    name: String,
    param_names: Vec<String>,
    param_types: Vec<String>,
    return_type: String,
    docs: String,
}

#[derive(Clone, Debug)]
struct NamespaceDecl {
    root: String,
    docs: String,
}

#[derive(Default)]
struct SymbolTree {
    children: BTreeMap<String, SymbolTree>,
    functions: Vec<AbiFunctionDecl>,
}

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("missing manifest dir"));
    let spec_dir = manifest_dir.join("src").join("abi_spec");
    let functions_list = spec_dir.join("functions.rs");
    let namespace_list = spec_dir.join("namespaces.rs");
    let enabled_features = enabled_feature_flags();
    let edge_impl_docs = parse_edge_host_impl_docs(&manifest_dir, &enabled_features);

    println!("cargo:rerun-if-changed={}", functions_list.display());
    println!("cargo:rerun-if-changed={}", namespace_list.display());

    let function_files = parse_include_order(&functions_list, &enabled_features);
    let function_decls = function_files
        .iter()
        .flat_map(|relative| {
            let path = spec_dir.join(relative);
            println!("cargo:rerun-if-changed={}", path.display());
            parse_function_file(&path, &edge_impl_docs)
        })
        .collect::<Vec<_>>();

    let namespace_decls = parse_namespace_file(&namespace_list, &enabled_features);
    validate_namespace_roots(&function_decls, &namespace_decls);

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("missing OUT_DIR"));
    write_generated_file(
        &out_dir.join("edge_abi_generated.rs"),
        &render_abi_rust(&function_decls, &namespace_decls),
    );
    write_generated_file(
        &out_dir.join("edge_abi_manifest.json"),
        &render_abi_json(&function_decls),
    );
}

fn write_generated_file(path: &Path, contents: &str) {
    fs::write(path, contents)
        .unwrap_or_else(|err| panic!("failed to write {}: {err}", path.display()));
}

fn enabled_feature_flags() -> BTreeSet<String> {
    env::vars()
        .filter_map(|(key, _)| key.strip_prefix("CARGO_FEATURE_").map(str::to_string))
        .collect()
}

fn parse_include_order(path: &Path, enabled_features: &BTreeSet<String>) -> Vec<String> {
    let source = fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
    let parsed = syn::parse_file(&source)
        .unwrap_or_else(|err| panic!("failed to parse {}: {err}", path.display()));

    let mut files = Vec::new();
    for item in parsed.items {
        let Item::Macro(item_macro) = item else {
            continue;
        };
        if !cfg_matches(&item_macro.attrs, enabled_features) {
            continue;
        }
        if !item_macro.mac.path.is_ident("include") {
            continue;
        }
        let include_path = syn::parse2::<syn::LitStr>(item_macro.mac.tokens.clone())
            .unwrap_or_else(|err| {
                panic!("failed to parse include! path in {}: {err}", path.display())
            });
        files.push(include_path.value());
    }
    if files.is_empty() {
        panic!(
            "no abi function includes were discovered in {}",
            path.display()
        );
    }
    files
}

fn parse_function_file(
    path: &Path,
    edge_impl_docs: &BTreeMap<String, String>,
) -> Vec<AbiFunctionDecl> {
    let source = fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
    let parsed = syn::parse_file(&source)
        .unwrap_or_else(|err| panic!("failed to parse {}: {err}", path.display()));
    parsed
        .items
        .iter()
        .filter_map(|item| parse_function_decl(item, edge_impl_docs))
        .collect()
}

fn parse_function_decl(
    item: &Item,
    edge_impl_docs: &BTreeMap<String, String>,
) -> Option<AbiFunctionDecl> {
    let Item::Fn(function) = item else {
        return None;
    };
    let name = pd_host_function_name(&function.attrs).unwrap_or_else(|| {
        panic!(
            "abi spec function '{}' is missing #[pd_host_function(name = \"...\")]",
            function.sig.ident
        )
    });
    let mut param_names = Vec::new();
    let mut param_types = Vec::new();
    for input in &function.sig.inputs {
        let FnArg::Typed(pat_type) = input else {
            panic!("abi spec methods are not supported");
        };
        let Pat::Ident(ident) = pat_type.pat.as_ref() else {
            panic!("abi spec parameters must use identifier patterns");
        };
        param_names.push(ident.ident.to_string());
        param_types.push(type_label(&pat_type.ty));
    }
    let spec_docs = doc_string(&function.attrs);
    let docs = edge_impl_docs
        .get(&name)
        .filter(|docs| !docs.trim().is_empty())
        .cloned()
        .or_else(|| (!spec_docs.trim().is_empty()).then_some(spec_docs))
        .unwrap_or_else(|| {
            panic!(
                "edge ABI function '{name}' is missing /// doc comments on its #[pd_edge_host_function] implementation or ABI spec declaration"
            )
        });

    Some(AbiFunctionDecl {
        name,
        param_names,
        param_types,
        return_type: match &function.sig.output {
            ReturnType::Default => "Null".to_string(),
            ReturnType::Type(_, ty) => type_label(ty),
        },
        docs,
    })
}

fn parse_edge_host_impl_docs(
    manifest_dir: &Path,
    enabled_features: &BTreeSet<String>,
) -> BTreeMap<String, String> {
    let Some(repo_root) = manifest_dir.parent() else {
        return BTreeMap::new();
    };
    let impl_dir = repo_root.join("pd-edge").join("src").join("abi_impl");
    if !impl_dir.exists() {
        return BTreeMap::new();
    }

    let mut files = Vec::new();
    collect_rs_files(&impl_dir, &mut files);

    let mut docs_by_name = BTreeMap::new();
    for path in files {
        println!("cargo:rerun-if-changed={}", path.display());
        let source = fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
        let parsed = syn::parse_file(&source)
            .unwrap_or_else(|err| panic!("failed to parse {}: {err}", path.display()));
        collect_edge_impl_docs_from_items(&parsed.items, enabled_features, &mut docs_by_name);
    }
    docs_by_name
}

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries = fs::read_dir(dir)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", dir.display()))
        .filter_map(Result::ok)
        .collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.path());

    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

fn collect_edge_impl_docs_from_items(
    items: &[Item],
    enabled_features: &BTreeSet<String>,
    docs_by_name: &mut BTreeMap<String, String>,
) {
    for item in items {
        match item {
            Item::Fn(function) => {
                if !edge_impl_cfg_matches(&function.attrs, enabled_features) {
                    continue;
                }
                let Some(name) = pd_edge_host_function_name(&function.attrs) else {
                    continue;
                };
                let docs = doc_string(&function.attrs);
                if docs.trim().is_empty() {
                    continue;
                }
                match docs_by_name.get(&name) {
                    Some(existing) if existing == &docs => {}
                    Some(existing) => {
                        panic!(
                            "duplicate pd_edge_host_function docs for '{name}': {:?} vs {:?}",
                            existing, docs
                        );
                    }
                    None => {
                        docs_by_name.insert(name, docs);
                    }
                }
            }
            Item::Mod(item_mod) => {
                if !edge_impl_cfg_matches(&item_mod.attrs, enabled_features) {
                    continue;
                }
                if let Some((_, items)) = &item_mod.content {
                    collect_edge_impl_docs_from_items(items, enabled_features, docs_by_name);
                }
            }
            _ => {}
        }
    }
}

fn pd_host_function_name(attrs: &[syn::Attribute]) -> Option<String> {
    let attr = attrs
        .iter()
        .find(|attr| attr.path().is_ident("pd_host_function"))?;
    let meta = &attr.meta;
    let Meta::List(list) = meta else {
        panic!("#[pd_host_function] must use name = \"...\"");
    };
    let args = list
        .parse_args_with(syn::punctuated::Punctuated::<Meta, syn::Token![,]>::parse_terminated)
        .unwrap_or_else(|err| panic!("failed to parse #[pd_host_function(...)] args: {err}"));
    let Some(Meta::NameValue(name_value)) = args.first() else {
        panic!("#[pd_host_function] requires name = \"...\"");
    };
    if !name_value.path.is_ident("name") {
        panic!("#[pd_host_function] only supports name = \"...\"");
    }
    match &name_value.value {
        syn::Expr::Lit(expr_lit) => {
            if let syn::Lit::Str(value) = &expr_lit.lit {
                Some(value.value())
            } else {
                panic!("callable name must be a string literal");
            }
        }
        _ => panic!("callable name must be a string literal"),
    }
}

fn pd_edge_host_function_name(attrs: &[syn::Attribute]) -> Option<String> {
    let attr = attrs
        .iter()
        .find(|attr| attr.path().is_ident("pd_edge_host_function"))?;
    let Meta::List(list) = &attr.meta else {
        panic!("#[pd_edge_host_function] must use name = ..., scope = ...");
    };
    let args = list
        .parse_args_with(syn::punctuated::Punctuated::<Meta, syn::Token![,]>::parse_terminated)
        .unwrap_or_else(|err| panic!("failed to parse #[pd_edge_host_function(...)] args: {err}"));
    let name_value = args
        .iter()
        .find_map(|meta| match meta {
            Meta::NameValue(name_value) if name_value.path.is_ident("name") => Some(name_value),
            _ => None,
        })
        .unwrap_or_else(|| panic!("#[pd_edge_host_function] requires name = ..."));
    Some(edge_host_name_expr(&name_value.value))
}

fn edge_host_name_expr(value: &syn::Expr) -> String {
    match value {
        syn::Expr::Lit(expr_lit) => match &expr_lit.lit {
            syn::Lit::Str(value) => value.value(),
            _ => panic!("edge host callable name must be a string literal or <path>.name"),
        },
        syn::Expr::Field(field) => {
            let syn::Member::Named(member) = &field.member else {
                panic!("edge host callable name must use .name");
            };
            if member != "name" {
                panic!("edge host callable name must use .name");
            }
            let syn::Expr::Path(path) = field.base.as_ref() else {
                panic!("edge host callable name must use a path ending in .name");
            };
            canonicalize_edge_host_path(&path.path)
        }
        syn::Expr::Path(path) => canonicalize_edge_host_path(&path.path),
        _ => panic!("edge host callable name must be a string literal or <path>.name"),
    }
}

fn canonicalize_edge_host_path(path: &syn::Path) -> String {
    let segments = path
        .segments
        .iter()
        .map(|segment| segment.ident.to_string())
        .collect::<Vec<_>>();
    let Some((first, rest)) = segments.split_first() else {
        panic!("edge host callable path must not be empty");
    };

    let mut canonical = match first.as_str() {
        "edge_runtime" => vec!["runtime".to_string()],
        "edge_rate_limit" => vec!["rate_limit".to_string()],
        "http_request" => vec!["http".to_string(), "request".to_string()],
        "http_response" => vec!["http".to_string(), "response".to_string()],
        "http_exchange" => vec!["http".to_string(), "exchange".to_string()],
        "http_upstream_request" => {
            vec![
                "http".to_string(),
                "upstream".to_string(),
                "request".to_string(),
            ]
        }
        "http_upstream_response" => {
            vec![
                "http".to_string(),
                "upstream".to_string(),
                "response".to_string(),
            ]
        }
        "proxy_symbols" => vec!["proxy".to_string()],
        other => vec![other.to_string()],
    };

    canonical.extend(rest.iter().map(|segment| {
        if segment
            .chars()
            .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_')
        {
            segment.to_ascii_lowercase()
        } else {
            segment.to_string()
        }
    }));

    canonical.join("::")
}

fn doc_string(attrs: &[Attribute]) -> String {
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

fn type_label(ty: &Type) -> String {
    match ty {
        Type::Group(group) => type_label(&group.elem),
        Type::Paren(paren) => type_label(&paren.elem),
        Type::Reference(reference) => type_label(&reference.elem),
        Type::Tuple(tuple) if tuple.elems.is_empty() => "Null".to_string(),
        Type::Path(path) => {
            let Some(segment) = path.path.segments.last() else {
                panic!("unsupported callable type");
            };
            let ident = segment.ident.to_string();
            match ident.as_str() {
                "i8" | "i16" | "i32" | "i64" | "i128" | "isize" | "u8" | "u16" | "u32" | "u64"
                | "u128" | "usize" => "Int".to_string(),
                "f32" | "f64" => "Float".to_string(),
                "bool" => "Bool".to_string(),
                "String" | "str" => "String".to_string(),
                "Any" | "AnyValue" | "Value" => "Any".to_string(),
                "Array" => "Array".to_string(),
                "Map" | "VmMap" => "Map".to_string(),
                "Number" => "Number".to_string(),
                "Unknown" => "Unknown".to_string(),
                "Option" => {
                    let syn::PathArguments::AngleBracketed(args) = &segment.arguments else {
                        panic!("Option<T> requires one generic argument");
                    };
                    let Some(syn::GenericArgument::Type(inner)) = args.args.first() else {
                        panic!("Option<T> requires one type argument");
                    };
                    let inner_label = type_label(inner);
                    if inner_label == "String" {
                        "String".to_string()
                    } else if inner_label == "Array" {
                        "Array".to_string()
                    } else if inner_label == "Map" {
                        "Map".to_string()
                    } else {
                        panic!("unsupported Option return type '{inner_label}'");
                    }
                }
                _ => panic!("unsupported callable type '{ident}'"),
            }
        }
        _ => panic!("unsupported callable type"),
    }
}

fn parse_namespace_file(path: &Path, enabled_features: &BTreeSet<String>) -> Vec<NamespaceDecl> {
    let source = fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
    let parsed = syn::parse_file(&source)
        .unwrap_or_else(|err| panic!("failed to parse {}: {err}", path.display()));

    let mut decls = Vec::new();
    for item in parsed.items {
        let Item::Macro(item_macro) = item else {
            continue;
        };
        if !cfg_matches(&item_macro.attrs, enabled_features) {
            continue;
        }
        if !item_macro.mac.path.is_ident("edge_host_namespace") {
            continue;
        }
        let args = syn::parse::Parser::parse2(
            syn::punctuated::Punctuated::<syn::LitStr, syn::Token![,]>::parse_terminated,
            item_macro.mac.tokens.clone(),
        )
        .unwrap_or_else(|err| {
            panic!(
                "failed to parse namespace decl in {}: {err}",
                path.display()
            )
        });
        let mut values = args.into_iter();
        let root = values
            .next()
            .unwrap_or_else(|| panic!("namespace decl in {} is missing a root", path.display()))
            .value();
        let docs = values
            .next()
            .unwrap_or_else(|| panic!("namespace decl in {} is missing docs", path.display()))
            .value();
        if values.next().is_some() {
            panic!(
                "namespace decl in {} has unexpected trailing arguments",
                path.display()
            );
        }
        decls.push(NamespaceDecl { root, docs });
    }
    decls
}

fn cfg_matches(attrs: &[Attribute], enabled_features: &BTreeSet<String>) -> bool {
    attrs.iter().all(|attr| match &attr.meta {
        Meta::List(list) if attr.path().is_ident("cfg") => {
            eval_cfg_meta_list(list, enabled_features)
        }
        _ => true,
    })
}

fn edge_impl_cfg_matches(attrs: &[Attribute], enabled_features: &BTreeSet<String>) -> bool {
    attrs.iter().all(|attr| match &attr.meta {
        Meta::List(list) if attr.path().is_ident("cfg") => {
            eval_edge_impl_cfg_meta_list(list, enabled_features)
        }
        _ => true,
    })
}

fn eval_cfg_meta_list(list: &syn::MetaList, enabled_features: &BTreeSet<String>) -> bool {
    let nested = list
        .parse_args_with(syn::punctuated::Punctuated::<Meta, syn::Token![,]>::parse_terminated)
        .unwrap_or_else(|err| panic!("failed to parse #[cfg(...)] args: {err}"));
    if nested.len() != 1 {
        panic!("#[cfg(...)] on ABI specs must contain exactly one top-level expression");
    }
    eval_cfg_meta(
        nested
            .first()
            .expect("checked nested cfg expression length above"),
        enabled_features,
    )
}

fn eval_edge_impl_cfg_meta_list(list: &syn::MetaList, enabled_features: &BTreeSet<String>) -> bool {
    let nested = list
        .parse_args_with(syn::punctuated::Punctuated::<Meta, syn::Token![,]>::parse_terminated)
        .unwrap_or_else(|err| panic!("failed to parse #[cfg(...)] args: {err}"));
    if nested.len() != 1 {
        panic!("#[cfg(...)] on edge impl docs must contain exactly one top-level expression");
    }
    eval_edge_impl_cfg_meta(
        nested
            .first()
            .expect("checked nested cfg expression length above"),
        enabled_features,
    )
}

fn eval_cfg_meta(meta: &Meta, enabled_features: &BTreeSet<String>) -> bool {
    match meta {
        Meta::NameValue(name_value) if name_value.path.is_ident("feature") => {
            match &name_value.value {
                syn::Expr::Lit(expr_lit) => match &expr_lit.lit {
                    syn::Lit::Str(value) => {
                        enabled_features.contains(&value.value().to_ascii_uppercase())
                    }
                    _ => panic!("cfg(feature = ...) must use a string literal"),
                },
                _ => panic!("cfg(feature = ...) must use a string literal"),
            }
        }
        Meta::List(list) if list.path.is_ident("all") => list
            .parse_args_with(syn::punctuated::Punctuated::<Meta, syn::Token![,]>::parse_terminated)
            .unwrap_or_else(|err| panic!("failed to parse cfg(all(...)) args: {err}"))
            .iter()
            .all(|meta| eval_cfg_meta(meta, enabled_features)),
        Meta::List(list) if list.path.is_ident("any") => list
            .parse_args_with(syn::punctuated::Punctuated::<Meta, syn::Token![,]>::parse_terminated)
            .unwrap_or_else(|err| panic!("failed to parse cfg(any(...)) args: {err}"))
            .iter()
            .any(|meta| eval_cfg_meta(meta, enabled_features)),
        Meta::List(list) if list.path.is_ident("not") => {
            let nested = list
                .parse_args_with(
                    syn::punctuated::Punctuated::<Meta, syn::Token![,]>::parse_terminated,
                )
                .unwrap_or_else(|err| panic!("failed to parse cfg(not(...)) args: {err}"));
            if nested.len() != 1 {
                panic!("cfg(not(...)) must contain exactly one expression");
            }
            !eval_cfg_meta(
                nested
                    .first()
                    .expect("checked nested cfg expression length above"),
                enabled_features,
            )
        }
        _ => panic!("unsupported cfg expression in ABI specs"),
    }
}

fn eval_edge_impl_cfg_meta(meta: &Meta, enabled_features: &BTreeSet<String>) -> bool {
    match meta {
        Meta::NameValue(name_value) if name_value.path.is_ident("feature") => {
            match &name_value.value {
                syn::Expr::Lit(expr_lit) => match &expr_lit.lit {
                    syn::Lit::Str(value) => {
                        enabled_features.contains(&value.value().to_ascii_uppercase())
                    }
                    _ => panic!("cfg(feature = ...) must use a string literal"),
                },
                _ => panic!("cfg(feature = ...) must use a string literal"),
            }
        }
        Meta::Path(path) if path.is_ident("test") => false,
        Meta::NameValue(name_value) if name_value.path.is_ident("target_arch") => true,
        Meta::List(list) if list.path.is_ident("all") => list
            .parse_args_with(syn::punctuated::Punctuated::<Meta, syn::Token![,]>::parse_terminated)
            .unwrap_or_else(|err| panic!("failed to parse cfg(all(...)) args: {err}"))
            .iter()
            .all(|meta| eval_edge_impl_cfg_meta(meta, enabled_features)),
        Meta::List(list) if list.path.is_ident("any") => list
            .parse_args_with(syn::punctuated::Punctuated::<Meta, syn::Token![,]>::parse_terminated)
            .unwrap_or_else(|err| panic!("failed to parse cfg(any(...)) args: {err}"))
            .iter()
            .any(|meta| eval_edge_impl_cfg_meta(meta, enabled_features)),
        Meta::List(list) if list.path.is_ident("not") => {
            let nested = list
                .parse_args_with(
                    syn::punctuated::Punctuated::<Meta, syn::Token![,]>::parse_terminated,
                )
                .unwrap_or_else(|err| panic!("failed to parse cfg(not(...)) args: {err}"));
            if nested.len() != 1 {
                panic!("cfg(not(...)) must contain exactly one expression");
            }
            !eval_edge_impl_cfg_meta(
                nested
                    .first()
                    .expect("checked nested cfg expression length above"),
                enabled_features,
            )
        }
        _ => true,
    }
}

fn validate_namespace_roots(functions: &[AbiFunctionDecl], namespaces: &[NamespaceDecl]) {
    let declared = namespaces
        .iter()
        .map(|namespace| namespace.root.as_str())
        .collect::<BTreeSet<_>>();
    let used = functions
        .iter()
        .map(|function| {
            function
                .name
                .split("::")
                .next()
                .unwrap_or_else(|| panic!("invalid abi name {}", function.name))
        })
        .collect::<BTreeSet<_>>();
    if declared != used {
        panic!(
            "host namespace declarations do not match function roots: declared={declared:?}, used={used:?}"
        );
    }
}

fn render_abi_rust(functions: &[AbiFunctionDecl], namespaces: &[NamespaceDecl]) -> String {
    let mut out = String::new();

    for (index, function) in functions.iter().enumerate() {
        writeln!(
            &mut out,
            "pub const {}: u16 = {index};",
            fn_const_name(function)
        )
        .unwrap();
    }
    writeln!(&mut out).unwrap();

    for function in functions {
        writeln!(
            &mut out,
            "pub const {}_PARAM_NAMES: [&str; {}] = [{}];",
            abi_const_name(function),
            function.param_names.len(),
            function
                .param_names
                .iter()
                .map(|param| format!("{param:?}"))
                .collect::<Vec<_>>()
                .join(", ")
        )
        .unwrap();
        writeln!(
            &mut out,
            "pub const {}_PARAM_TYPES: [AbiParamType; {}] = [{}];",
            abi_const_name(function),
            function.param_types.len(),
            function
                .param_types
                .iter()
                .map(|param| format!("AbiParamType::{param}"))
                .collect::<Vec<_>>()
                .join(", ")
        )
        .unwrap();
        writeln!(
            &mut out,
            "pub const {}: AbiFunction = AbiFunction {{ index: {}, name: {:?}, arity: {}, param_names: &{}_PARAM_NAMES, param_types: &{}_PARAM_TYPES, return_type: AbiValueType::{}, docs: {:?} }};",
            abi_const_name(function),
            fn_const_name(function),
            function.name,
            function.param_names.len(),
            abi_const_name(function),
            abi_const_name(function),
            function.return_type,
            function.docs
        )
        .unwrap();
    }
    writeln!(&mut out).unwrap();

    writeln!(
        &mut out,
        "pub const FUNCTIONS: [AbiFunction; {}] = [",
        functions.len()
    )
    .unwrap();
    for function in functions {
        writeln!(&mut out, "    {},", abi_const_name(function)).unwrap();
    }
    writeln!(&mut out, "];").unwrap();
    writeln!(&mut out).unwrap();

    writeln!(
        &mut out,
        "pub const HOST_NAMESPACES: [HostNamespaceSpec; {}] = [",
        namespaces.len()
    )
    .unwrap();
    for namespace in namespaces {
        writeln!(
            &mut out,
            "    HostNamespaceSpec {{ root: {:?}, docs: {:?} }},",
            namespace.root, namespace.docs
        )
        .unwrap();
    }
    writeln!(&mut out, "];").unwrap();
    writeln!(&mut out).unwrap();

    writeln!(
        &mut out,
        "pub const HOST_FUNCTION_COUNT: u16 = FUNCTIONS.len() as u16;"
    )
    .unwrap();
    writeln!(&mut out).unwrap();

    let tree = build_symbol_tree(functions);
    writeln!(&mut out, "pub mod symbols {{").unwrap();
    render_symbol_tree(&mut out, &tree, 1);
    writeln!(&mut out, "}}").unwrap();

    out
}

fn render_abi_json(functions: &[AbiFunctionDecl]) -> String {
    let mut out = String::new();
    out.push_str("{\n");
    out.push_str("  \"abi_version\": 21,\n");
    out.push_str("  \"functions\": [\n");
    for (index, function) in functions.iter().enumerate() {
        let suffix = if index + 1 == functions.len() {
            ""
        } else {
            ","
        };
        let params = function
            .param_names
            .iter()
            .zip(function.param_types.iter())
            .map(|(name, ty)| {
                format!(
                    "{{\"name\": {name:?}, \"type\": {:?}}}",
                    ty.to_ascii_lowercase()
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        writeln!(
            &mut out,
            "    {{ \"index\": {index}, \"name\": {:?}, \"arity\": {}, \"params\": [{}], \"return_type\": {:?}, \"docs\": {:?} }}{suffix}",
            function.name,
            function.param_names.len(),
            params,
            function.return_type.to_ascii_lowercase(),
            function.docs
        )
        .unwrap();
    }
    out.push_str("  ]\n");
    out.push_str("}\n");
    out
}

fn build_symbol_tree(functions: &[AbiFunctionDecl]) -> SymbolTree {
    let mut root = SymbolTree::default();
    for function in functions {
        let segments = function.name.split("::").collect::<Vec<_>>();
        let mut node = &mut root;
        for segment in &segments[..segments.len().saturating_sub(1)] {
            node = node.children.entry((*segment).to_string()).or_default();
        }
        node.functions.push(function.clone());
    }
    root
}

fn render_symbol_tree(out: &mut String, tree: &SymbolTree, indent: usize) {
    let pad = "    ".repeat(indent);
    for (name, child) in &tree.children {
        writeln!(out, "{pad}pub mod {name} {{").unwrap();
        render_symbol_tree(out, child, indent + 1);
        writeln!(out, "{pad}}}").unwrap();
    }
    for function in &tree.functions {
        let leaf = function
            .name
            .rsplit("::")
            .next()
            .expect("function should have final segment");
        writeln!(
            out,
            "{pad}pub const {}: crate::AbiFunction = crate::{};",
            to_shouty_snake(leaf),
            abi_const_name(function)
        )
        .unwrap();
    }
}

fn fn_const_name(function: &AbiFunctionDecl) -> String {
    format!("FN_{}", to_shouty_snake(&function.name.replace("::", "_")))
}

fn abi_const_name(function: &AbiFunctionDecl) -> String {
    format!("ABI_{}", to_shouty_snake(&function.name.replace("::", "_")))
}

fn to_shouty_snake(value: &str) -> String {
    let mut out = String::new();
    let mut prev_is_lower_or_digit = false;
    for ch in value.chars() {
        if ch == '_' {
            if !out.ends_with('_') {
                out.push('_');
            }
            prev_is_lower_or_digit = false;
            continue;
        }
        if ch.is_ascii_uppercase() && prev_is_lower_or_digit && !out.ends_with('_') {
            out.push('_');
        }
        out.push(ch.to_ascii_uppercase());
        prev_is_lower_or_digit = ch.is_ascii_lowercase() || ch.is_ascii_digit();
    }
    out
}
