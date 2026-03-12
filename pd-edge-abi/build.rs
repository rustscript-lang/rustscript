use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use syn::{FnArg, Item, Meta, Pat, ReturnType, Type};

#[derive(Clone, Debug)]
struct AbiFunctionDecl {
    name: String,
    param_names: Vec<String>,
    param_types: Vec<String>,
    return_type: String,
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

    println!("cargo:rerun-if-changed={}", functions_list.display());
    println!("cargo:rerun-if-changed={}", namespace_list.display());

    let function_files = parse_include_order(&functions_list);
    let function_decls = function_files
        .iter()
        .flat_map(|relative| {
            let path = spec_dir.join(relative);
            println!("cargo:rerun-if-changed={}", path.display());
            parse_function_file(&path)
        })
        .collect::<Vec<_>>();

    let namespace_decls = parse_namespace_file(&namespace_list);
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

fn parse_include_order(path: &Path) -> Vec<String> {
    let source = fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
    let mut files = Vec::new();
    let mut rest = source.as_str();
    loop {
        let Some(index) = rest.find("include!(\"") else {
            break;
        };
        rest = &rest[index + "include!(\"".len()..];
        let end = rest
            .find('"')
            .unwrap_or_else(|| panic!("unterminated include! in {}", path.display()));
        files.push(rest[..end].to_string());
        rest = &rest[end + 1..];
    }
    files
}

fn parse_function_file(path: &Path) -> Vec<AbiFunctionDecl> {
    let source = fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
    let parsed = syn::parse_file(&source)
        .unwrap_or_else(|err| panic!("failed to parse {}: {err}", path.display()));
    parsed
        .items
        .iter()
        .filter_map(parse_function_decl)
        .collect()
}

fn parse_function_decl(item: &Item) -> Option<AbiFunctionDecl> {
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

    Some(AbiFunctionDecl {
        name,
        param_names,
        param_types,
        return_type: match &function.sig.output {
            ReturnType::Default => "Null".to_string(),
            ReturnType::Type(_, ty) => type_label(ty),
        },
    })
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
                "Any" => "Any".to_string(),
                "Array" => "Array".to_string(),
                "Map" => "Map".to_string(),
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

fn parse_namespace_file(path: &Path) -> Vec<NamespaceDecl> {
    let source = fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
    let mut decls = Vec::new();
    let mut rest = source.as_str();
    loop {
        let Some(index) = rest.find("edge_host_namespace!(") else {
            break;
        };
        rest = &rest[index + "edge_host_namespace!(".len()..];
        let end = find_matching_paren(rest);
        let args = &rest[..end];
        let (root, rest_after_root) = parse_string(args);
        let rest_after_root = expect_comma(rest_after_root);
        let (docs, rest_after_docs) = parse_string(rest_after_root);
        let rest_after_docs = skip_ws(rest_after_docs);
        if !rest_after_docs.is_empty() {
            panic!("unexpected trailing tokens in namespace decl: {rest_after_docs}");
        }
        decls.push(NamespaceDecl { root, docs });
        rest = &rest[end + 1..];
    }
    decls
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
            "pub const {}: AbiFunction = AbiFunction {{ index: {}, name: {:?}, arity: {}, param_names: &{}_PARAM_NAMES, param_types: &{}_PARAM_TYPES, return_type: AbiValueType::{} }};",
            abi_const_name(function),
            fn_const_name(function),
            function.name,
            function.param_names.len(),
            abi_const_name(function),
            abi_const_name(function),
            function.return_type
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
    out.push_str("  \"abi_version\": 16,\n");
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
            "    {{ \"index\": {index}, \"name\": {:?}, \"arity\": {}, \"params\": [{}], \"return_type\": {:?} }}{suffix}",
            function.name,
            function.param_names.len(),
            params,
            function.return_type.to_ascii_lowercase()
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

fn parse_string(source: &str) -> (String, &str) {
    let source = skip_ws(source);
    let mut chars = source.char_indices();
    let Some((_, '"')) = chars.next() else {
        panic!("expected string literal");
    };
    let mut value = String::new();
    let mut escaped = false;
    for (index, ch) in source[1..].char_indices() {
        if escaped {
            value.push(match ch {
                '\\' => '\\',
                '"' => '"',
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                other => other,
            });
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '"' => return (value, &source[index + 2..]),
            other => value.push(other),
        }
    }
    panic!("unterminated string literal");
}

fn expect_comma(source: &str) -> &str {
    skip_ws(source)
        .strip_prefix(',')
        .unwrap_or_else(|| panic!("expected comma near '{source}'"))
}

fn skip_ws(source: &str) -> &str {
    source.trim_start_matches(char::is_whitespace)
}

fn find_matching_paren(source: &str) -> usize {
    let mut depth = 1usize;
    let mut in_string = false;
    let mut escaped = false;
    for (index, ch) in source.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
                continue;
            }
            match ch {
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return index;
                }
            }
            _ => {}
        }
    }
    panic!("unterminated macro invocation");
}
