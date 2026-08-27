use std::collections::{HashMap, HashSet};
use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use syn::{Attribute, FnArg, Item, ItemFn, Meta, Pat, ReturnType, Type};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SourceCategory {
    DefaultHost,
    NamespacedBuiltin,
    MetadataOnlyBuiltin,
}

#[derive(Clone, Debug)]
struct SourceSpec {
    path: String,
    module: String,
    category: SourceCategory,
}

#[derive(Clone, Debug)]
struct CallableParamDecl {
    name: String,
    ty_label: String,
    optional: bool,
}

#[derive(Clone, Debug)]
struct WrapperDecl {
    fn_name: String,
    mut_fn_name: String,
    params: Vec<WrapperParamKind>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WrapperParamKind {
    Vm,
    SliceArgs,
}

#[allow(clippy::enum_variant_names)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HostBindingKind {
    StaticStack,
    StaticArgs,
    StaticNonYieldingArgs,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HostExecutionKind {
    Sync,
    MaySuspend,
}

impl HostBindingKind {
    pub(crate) fn render_bind_static_call(&self, name: &str, function_name: &str) -> String {
        let method = match self {
            Self::StaticStack => "bind_static_stack_function",
            Self::StaticNonYieldingArgs => "bind_static_non_yielding_args_function",
            Self::StaticArgs => "bind_static_args_function",
        };
        format!("vm.{method}({name:?}, {function_name});")
    }
}

/// Documented call-index blocks shared by builtins and host imports.
///
/// Must match the block table in `src/builtins/catalog.rs`.
pub(crate) const ORDINARY_BLOCK_START: u16 = 0xFFA2;
pub(crate) const SPECIAL_CALL_BLOCK_START: u16 = 0xFF90;
pub(crate) const SPECIAL_CALL_BLOCK_END: u16 = 0xFFA1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CatalogClass {
    Ordinary,
    Internal,
    Special,
}

impl CatalogClass {
    fn parse(value: &str) -> Self {
        match value {
            "Ordinary" => Self::Ordinary,
            "Internal" => Self::Internal,
            "Special" => Self::Special,
            other => panic!(
                "unknown builtin catalog class {other:?} (expected Ordinary, Internal, or Special)"
            ),
        }
    }
}

/// One explicit static builtin ID from `src/builtins/catalog.rs`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CatalogEntry {
    pub(crate) id: u16,
    pub(crate) source_name: String,
    pub(crate) variant: String,
    pub(crate) class: CatalogClass,
    pub(crate) feature_gate: String,
}

#[derive(Clone, Debug)]
struct CallableDecl {
    rust_ident: String,
    module: String,
    name: String,
    docs: String,
    params: Vec<CallableParamDecl>,
    return_label: String,
    static_return_type: String,
    wrapper: Option<WrapperDecl>,
    host_binding_kind: HostBindingKind,
    host_execution: HostExecutionKind,
}

#[derive(Clone, Debug)]
struct NamespaceDecl {
    namespace: String,
    module: String,
    docs: String,
    runtime_supported_on_wasm: bool,
}

#[derive(Clone, Debug)]
struct Group<'a> {
    key: String,
    items: Vec<&'a CallableDecl>,
}

fn main() {
    emit_git_build_metadata();

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("missing manifest dir"));
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("missing OUT_DIR"));

    let namespace_manifest = manifest_dir
        .join("src")
        .join("builtins")
        .join("runtime")
        .join("namespaces.rs");
    println!("cargo:rerun-if-changed={}", namespace_manifest.display());
    let namespaces = parse_namespace_manifest(&namespace_manifest);

    let catalog_path = manifest_dir.join("src").join("builtins").join("catalog.rs");
    println!("cargo:rerun-if-changed={}", catalog_path.display());
    let catalog = parse_catalog(&catalog_path);

    let host_sources = [SourceSpec {
        path: "src/builtins/runtime/host.rs".to_string(),
        module: "host".to_string(),
        category: SourceCategory::DefaultHost,
    }];
    let builtin_sources = builtin_source_specs(&namespaces);
    let core_sources = [SourceSpec {
        path: "src/builtins/runtime/core.rs".to_string(),
        module: "core".to_string(),
        category: SourceCategory::MetadataOnlyBuiltin,
    }];

    let mut next_order = 0usize;
    let host_callables = parse_sources(&manifest_dir, &host_sources, &mut next_order);
    let builtin_callables = parse_sources(&manifest_dir, &builtin_sources, &mut next_order);
    let core_callables = parse_sources(&manifest_dir, &core_sources, &mut next_order);
    let metadata_callables = core_callables.clone();

    validate_namespace_roots(&builtin_callables, &namespaces);
    validate_known_language_builtins(&core_callables);
    validate_wrapper_shapes(&host_callables, SourceCategory::DefaultHost);
    validate_wrapper_shapes(&builtin_callables, SourceCategory::NamespacedBuiltin);

    write_generated_file(
        &out_dir.join("builtin_catalog_generated.rs"),
        &render_builtin_catalog(
            &namespaces,
            &host_callables,
            &builtin_callables,
            &metadata_callables,
            &catalog,
        ),
    );
    write_generated_file(
        &out_dir.join("builtin_runtime_dispatch_generated.rs"),
        &render_builtin_runtime_dispatch(&host_callables, &builtin_callables),
    );
}

fn emit_git_build_metadata() {
    println!("cargo:rerun-if-env-changed=PD_BUILD_GIT_TAG");
    println!("cargo:rerun-if-env-changed=PD_BUILD_GIT_COMMIT");
    println!("cargo:rerun-if-env-changed=PD_BUILD_GIT_DIRTY");

    let git_tag = env::var("PD_BUILD_GIT_TAG").unwrap_or_else(|_| {
        run_git(["describe", "--tags", "--exact-match"]).unwrap_or_else(|| "untagged".to_string())
    });
    let git_commit = env::var("PD_BUILD_GIT_COMMIT").unwrap_or_else(|_| {
        run_git(["rev-parse", "--short=12", "HEAD"]).unwrap_or_else(|| "unknown".to_string())
    });
    let git_dirty = env::var("PD_BUILD_GIT_DIRTY").unwrap_or_else(|_| {
        match run_git(["status", "--porcelain", "--untracked-files=no"]) {
            Some(output) if !output.trim().is_empty() => "true".to_string(),
            _ => "false".to_string(),
        }
    });

    println!("cargo:rustc-env=PD_BUILD_GIT_TAG={git_tag}");
    println!("cargo:rustc-env=PD_BUILD_GIT_COMMIT={git_commit}");
    println!("cargo:rustc-env=PD_BUILD_GIT_DIRTY={git_dirty}");
}

fn run_git<const N: usize>(args: [&str; N]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()
        .map(|value| value.trim().to_string())
}

fn write_generated_file(path: &Path, contents: &str) {
    fs::write(path, contents)
        .unwrap_or_else(|err| panic!("failed to write {}: {err}", path.display()));
}

fn builtin_source_specs(namespaces: &[NamespaceDecl]) -> Vec<SourceSpec> {
    namespaces
        .iter()
        .map(|namespace| {
            // At this layer the `io` namespace maps directly to the blocking
            // backend; the async/blocking split is introduced later with the
            // host async execution layer.
            let path = if namespace.module == "io" {
                "src/builtins/runtime/io/blocking.rs".to_string()
            } else {
                format!("src/builtins/runtime/{}.rs", namespace.module)
            };
            SourceSpec {
                path,
                module: namespace.module.clone(),
                category: SourceCategory::NamespacedBuiltin,
            }
        })
        .collect()
}

fn parse_sources(
    manifest_dir: &Path,
    specs: &[SourceSpec],
    next_order: &mut usize,
) -> Vec<CallableDecl> {
    let mut out = Vec::new();
    for spec in specs {
        let path = manifest_dir.join(&spec.path);
        println!("cargo:rerun-if-changed={}", path.display());
        let mut file_callables = parse_source_file(&path, spec, *next_order);
        *next_order += file_callables.len();
        out.append(&mut file_callables);
    }
    out
}

pub(crate) fn classify_host_binding(function: &ItemFn) -> HostBindingKind {
    if function.sig.inputs.iter().any(|input| match input {
        FnArg::Typed(pat_type) => is_vm_context_type(&pat_type.ty),
        _ => false,
    }) {
        return HostBindingKind::StaticStack;
    }
    let return_type = normalized_return_type(&function.sig.output);
    if matches!(
        plain_path_type(&return_type).as_deref(),
        Some("CallOutcome")
    ) {
        return HostBindingKind::StaticArgs;
    }
    if sole_type_argument(&return_type, "VmResult")
        .is_some_and(|inner| matches!(plain_path_type(&inner).as_deref(), Some("CallOutcome")))
    {
        return HostBindingKind::StaticArgs;
    }
    if is_supported_ordinary_return_type(&return_type) {
        return HostBindingKind::StaticNonYieldingArgs;
    }
    HostBindingKind::StaticArgs
}

pub(crate) fn infer_host_execution(function: &ItemFn) -> HostExecutionKind {
    let return_type = normalized_return_type(&function.sig.output);
    if contains_host_call_result(&return_type) {
        HostExecutionKind::MaySuspend
    } else {
        HostExecutionKind::Sync
    }
}

fn contains_host_call_result(ty: &Type) -> bool {
    if sole_type_argument(ty, "HostCallResult").is_some() {
        return true;
    }
    sole_type_argument(ty, "VmResult").is_some_and(|inner| contains_host_call_result(&inner))
}

fn is_supported_ordinary_return_type(ty: &Type) -> bool {
    match ty {
        Type::Group(group) => is_supported_ordinary_return_type(&group.elem),
        Type::Paren(paren) => is_supported_ordinary_return_type(&paren.elem),
        Type::Reference(reference) => is_supported_ordinary_return_type(&reference.elem),
        Type::Path(path) => {
            let Some(segment) = path.path.segments.last() else {
                return false;
            };
            match segment.ident.to_string().as_str() {
                "Option" | "VmResult" => sole_type_argument(ty, &segment.ident.to_string())
                    .is_some_and(|inner| is_supported_ordinary_return_type(&inner)),
                "Vec" => sole_type_argument(ty, "Vec")
                    .is_some_and(|inner| is_supported_vec_return_type(&inner)),
                "Value" | "bool" | "i64" | "u32" | "usize" | "f64" | "String" | "str"
                | "SharedArray" | "VmBytes" | "SharedBytes" | "VmMap" | "SharedMap"
                | "NumberValue" => matches!(segment.arguments, syn::PathArguments::None),
                _ => false,
            }
        }
        Type::Tuple(tuple) => tuple.elems.is_empty(),
        _ => false,
    }
}

fn is_supported_vec_return_type(ty: &Type) -> bool {
    if is_plain_path_type(ty, "Value") {
        return true;
    }
    let Type::Tuple(tuple) = unwrap_surface_type(ty) else {
        return false;
    };
    tuple.elems.len() == 2
        && tuple
            .elems
            .iter()
            .all(|elem| is_plain_path_type(elem, "Value"))
}

fn is_plain_path_type(ty: &Type, expected: &str) -> bool {
    let ty = unwrap_surface_type(ty);
    let Type::Path(path) = &ty else {
        return false;
    };
    path.path.segments.last().is_some_and(|segment| {
        segment.ident == expected && matches!(segment.arguments, syn::PathArguments::None)
    })
}

fn normalized_return_type(output: &ReturnType) -> Type {
    match output {
        ReturnType::Default => syn::parse_quote!(()),
        ReturnType::Type(_, ty) => unwrap_surface_type(ty),
    }
}

fn unwrap_surface_type(ty: &Type) -> Type {
    match ty {
        Type::Group(group) => unwrap_surface_type(&group.elem),
        Type::Paren(paren) => unwrap_surface_type(&paren.elem),
        Type::Reference(reference) => unwrap_surface_type(&reference.elem),
        _ => ty.clone(),
    }
}

fn plain_path_type(ty: &Type) -> Option<String> {
    match ty {
        Type::Group(group) => plain_path_type(&group.elem),
        Type::Paren(paren) => plain_path_type(&paren.elem),
        Type::Reference(reference) => plain_path_type(&reference.elem),
        Type::Path(path) => path.path.segments.last().map(|seg| seg.ident.to_string()),
        _ => None,
    }
}

fn sole_type_argument(ty: &Type, wrapper: &str) -> Option<Type> {
    let ty = unwrap_surface_type(ty);
    match &ty {
        Type::Path(path) => {
            let segment = path.path.segments.last()?;
            if segment.ident != wrapper {
                return None;
            }
            let syn::PathArguments::AngleBracketed(args) = &segment.arguments else {
                return None;
            };
            if args.args.len() != 1 {
                return None;
            }
            let arg = args.args.first()?;
            match arg {
                syn::GenericArgument::Type(inner) => Some(inner.clone()),
                _ => None,
            }
        }
        _ => None,
    }
}

fn parse_source_file(path: &Path, spec: &SourceSpec, _order_offset: usize) -> Vec<CallableDecl> {
    let source = fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
    let parsed = syn::parse_file(&source)
        .unwrap_or_else(|err| panic!("failed to parse {}: {err}", path.display()));

    let mut out = Vec::new();
    for item in parsed.items.iter() {
        let Item::Fn(function) = item else {
            continue;
        };
        let Some(name) = pd_host_function_name(&function.attrs) else {
            continue;
        };
        let params = parse_callable_params(function);
        let rust_ident = function.sig.ident.to_string();
        let docs = callable_docs(&name, &function.attrs);
        let wrapper = match spec.category {
            SourceCategory::MetadataOnlyBuiltin => None,
            _ => Some(generated_wrapper_decl(function)),
        };
        out.push(CallableDecl {
            rust_ident,
            module: spec.module.clone(),
            name,
            docs,
            params,
            return_label: return_type_label(&function.sig.output),
            static_return_type: static_return_type_label(&function.sig.output),
            wrapper,
            host_binding_kind: classify_host_binding(function),
            host_execution: infer_host_execution(function),
        });
    }
    out
}

/// Parse the authoritative static builtin ID catalog (`src/builtins/catalog.rs`).
///
/// Each entry is one line:
///
/// ```text
/// builtin_id!(0xXXXX, "source_name", RustVariant, Class, feature_gate);
/// ```
///
/// Fails on malformed lines, duplicate IDs, duplicate source names, or
/// duplicate Rust variants. Class and gate values are validated here; block
/// membership and callable coverage are validated by
/// [`validate_catalog_contract`].
fn parse_catalog(path: &Path) -> Vec<CatalogEntry> {
    let source = fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
    parse_catalog_source(&source, &path.display().to_string())
}

pub(crate) fn parse_catalog_source(source: &str, display: &str) -> Vec<CatalogEntry> {
    let mut entries = Vec::new();
    let mut seen_ids = HashSet::new();
    let mut seen_names = HashSet::new();
    let mut seen_variants = HashSet::new();
    for (line_index, raw_line) in source.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with("//") {
            continue;
        }
        let line_number = line_index + 1;
        let Some(rest) = line.strip_prefix("builtin_id!(") else {
            panic!("{display}:{line_number}: unexpected line in builtin catalog: {line:?}");
        };
        let rest = rest.strip_suffix(");").unwrap_or_else(|| {
            panic!("{display}:{line_number}: catalog entry must end with ');': {line:?}")
        });
        let parts = rest.split(',').map(str::trim).collect::<Vec<_>>();
        if parts.len() != 5 {
            panic!(
                "{display}:{line_number}: catalog entry needs 5 fields \
                 (id, source_name, RustVariant, Class, feature_gate): {line:?}"
            );
        }
        let id = u16::from_str_radix(parts[0].trim_start_matches("0x"), 16).unwrap_or_else(|err| {
            panic!("{display}:{line_number}: invalid builtin id {parts:?}: {err}")
        });
        let source_name = strip_quoted(parts[1]).unwrap_or_else(|| {
            panic!("{display}:{line_number}: expected quoted source name: {line:?}")
        });
        let variant = parts[2].to_string();
        let class = CatalogClass::parse(parts[3]);
        let feature_gate = parts[4].to_string();
        if feature_gate != "none" {
            panic!(
                "{display}:{line_number}: unsupported feature gate {feature_gate:?}; \
                 only 'none' is defined today"
            );
        }
        if !seen_ids.insert(id) {
            panic!("{display}:{line_number}: duplicate builtin id 0x{id:04X}");
        }
        if !seen_names.insert(source_name.clone()) {
            panic!("{display}:{line_number}: duplicate builtin source name {source_name:?}");
        }
        if !seen_variants.insert(variant.clone()) {
            panic!("{display}:{line_number}: duplicate builtin variant {variant}");
        }
        entries.push(CatalogEntry {
            id,
            source_name,
            variant,
            class,
            feature_gate,
        });
    }
    entries
}

fn strip_quoted(value: &str) -> Option<String> {
    value
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .map(str::to_string)
}

/// Validate the static builtin ID contract against the discovered callables.
///
/// Fails the build when:
/// - a runtime callable has no explicit catalog ID, or a catalog entry has no
///   runtime callable (typos and missing IDs);
/// - a catalog variant does not match the derived variant for its source name;
/// - a class disagrees with the dispatch classification (ordinary vs
///   special-call) or with the `__` internal-name prefix;
/// - an ID falls outside its documented block.
pub(crate) fn validate_catalog_contract(
    entries: &[CatalogEntry],
    discovered_names: &HashSet<String>,
    special_variants: &HashSet<String>,
) {
    let catalog_names = entries
        .iter()
        .map(|entry| entry.source_name.as_str())
        .collect::<HashSet<_>>();
    let discovered = discovered_names
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    if catalog_names != discovered {
        let mut missing = discovered.difference(&catalog_names).collect::<Vec<_>>();
        missing.sort_unstable();
        let mut extra = catalog_names.difference(&discovered).collect::<Vec<_>>();
        extra.sort_unstable();
        panic!(
            "builtin catalog does not match discovered callables: \
             missing explicit IDs for {missing:?}, catalog entries without callables {extra:?}"
        );
    }
    for entry in entries {
        let expected_variant = builtin_variant_name(&entry.source_name);
        if expected_variant != entry.variant {
            panic!(
                "catalog variant mismatch for '{}': derived {expected_variant}, catalog {}",
                entry.source_name, entry.variant
            );
        }
        let is_special_call = special_variants.contains(&entry.variant);
        match entry.class {
            CatalogClass::Ordinary => {
                if is_special_call {
                    panic!(
                        "catalog entry '{}' is class Ordinary but dispatches as a special-call builtin",
                        entry.source_name
                    );
                }
                if entry.id < ORDINARY_BLOCK_START {
                    panic!(
                        "ordinary builtin '{}' id 0x{:04X} falls outside the ordinary block \
                         0x{ORDINARY_BLOCK_START:04X}..=0xFFFF",
                        entry.source_name, entry.id
                    );
                }
            }
            CatalogClass::Internal | CatalogClass::Special => {
                if !is_special_call {
                    panic!(
                        "catalog entry '{}' is class {:?} but is not a special-call builtin",
                        entry.source_name, entry.class
                    );
                }
                if !(SPECIAL_CALL_BLOCK_START..=SPECIAL_CALL_BLOCK_END).contains(&entry.id) {
                    panic!(
                        "special-call builtin '{}' id 0x{:04X} falls outside the special-call block \
                         0x{SPECIAL_CALL_BLOCK_START:04X}..=0x{SPECIAL_CALL_BLOCK_END:04X}",
                        entry.source_name, entry.id
                    );
                }
            }
        }
        let is_internal_name = entry.source_name.starts_with("__");
        match entry.class {
            CatalogClass::Internal if !is_internal_name => {
                panic!(
                    "catalog entry '{}' is class Internal but its source name has no '__' prefix",
                    entry.source_name
                );
            }
            CatalogClass::Special if is_internal_name => {
                panic!(
                    "catalog entry '{}' has an internal '__' source name but is class Special; \
                     use Internal",
                    entry.source_name
                );
            }
            _ => {}
        }
    }
}

fn parse_namespace_manifest(path: &Path) -> Vec<NamespaceDecl> {
    let source = fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
    let mut decls = Vec::new();
    let mut rest = source.as_str();
    loop {
        let Some(index) = rest.find("builtin_namespace!(") else {
            break;
        };
        rest = &rest[index + "builtin_namespace!(".len()..];
        let end = find_matching_paren(rest);
        let args = &rest[..end];
        let (namespace, rest_after_namespace) = parse_string(args);
        let rest_after_namespace = expect_comma(rest_after_namespace);
        let (module, rest_after_module) = parse_string(rest_after_namespace);
        let rest_after_module = expect_comma(rest_after_module);
        let (docs, rest_after_docs) = parse_string(rest_after_module);
        let rest_after_docs = expect_comma(rest_after_docs);
        let (runtime_supported_on_wasm, rest_after_wasm) = parse_bool(rest_after_docs);
        if !skip_ws(rest_after_wasm).is_empty() {
            panic!("unexpected trailing tokens in namespace declaration: {rest_after_wasm}");
        }
        decls.push(NamespaceDecl {
            namespace,
            module,
            docs,
            runtime_supported_on_wasm,
        });
        rest = &rest[end + 1..];
    }
    decls
}

fn validate_namespace_roots(callables: &[CallableDecl], namespaces: &[NamespaceDecl]) {
    let declared = namespaces
        .iter()
        .map(|namespace| namespace.namespace.as_str())
        .collect::<HashSet<_>>();
    let used = callables
        .iter()
        .filter_map(|callable| callable.name.split_once("::").map(|(root, _)| root))
        .collect::<HashSet<_>>();
    if declared != used {
        panic!(
            "builtin namespace declarations do not match annotated callables: declared={declared:?}, used={used:?}"
        );
    }
}

fn validate_known_language_builtins(callables: &[CallableDecl]) {
    let known = callables
        .iter()
        .map(|callable| callable.name.as_str())
        .collect::<HashSet<_>>();
    for name in required_language_builtin_stubs() {
        if !known.contains(name) {
            panic!("missing lowering stub for language builtin '{name}'");
        }
    }
    for name in required_internal_builtin_stubs() {
        if !known.contains(name) {
            panic!("missing lowering stub for internal builtin '{name}'");
        }
    }
}

fn validate_wrapper_shapes(callables: &[CallableDecl], category: SourceCategory) {
    for callable in callables {
        let Some(_wrapper) = callable.wrapper.as_ref() else {
            continue;
        };
        validate_optional_param_layout(callable);
        match category {
            SourceCategory::DefaultHost | SourceCategory::NamespacedBuiltin => {}
            SourceCategory::MetadataOnlyBuiltin => {}
        }
    }
}

fn validate_optional_param_layout(callable: &CallableDecl) {
    let mut saw_optional = false;
    for param in &callable.params {
        if param.optional {
            saw_optional = true;
            continue;
        }
        if saw_optional {
            panic!(
                "callable '{}' has a required parameter after an optional parameter",
                callable.name
            );
        }
    }
}

fn render_builtin_catalog(
    namespaces: &[NamespaceDecl],
    host_callables: &[CallableDecl],
    builtin_callables: &[CallableDecl],
    metadata_callables: &[CallableDecl],
    catalog: &[CatalogEntry],
) -> String {
    let language_group_input = metadata_callables
        .iter()
        .filter(|callable| is_language_builtin_stub_name(&callable.name))
        .cloned()
        .collect::<Vec<_>>();
    let language_groups = stable_groups(&language_group_input, |callable| callable.name.clone());
    let language_builtin_order = language_groups
        .iter()
        .map(|group| group.key.clone())
        .collect::<Vec<_>>();
    let host_group_input = host_callables.to_vec();
    let host_groups = stable_groups(&host_group_input, |callable| callable.name.clone());
    let (builtin_variant_order, actual_builtin_by_variant) =
        ordered_actual_builtin_variants(namespaces, builtin_callables, metadata_callables);
    let discovered_names = builtin_callables
        .iter()
        .chain(metadata_callables.iter())
        .map(|callable| callable.name.clone())
        .collect::<HashSet<_>>();
    let special_variants = special_builtin_order()
        .iter()
        .chain(appended_builtin_order())
        .map(|name| builtin_variant_name(name))
        .collect::<HashSet<_>>();
    validate_catalog_contract(catalog, &discovered_names, &special_variants);
    let catalog_id_by_variant = catalog
        .iter()
        .map(|entry| (entry.variant.clone(), entry.id))
        .collect::<HashMap<_, _>>();

    let namespace_member_group_input = builtin_callables
        .iter()
        .chain(
            metadata_callables
                .iter()
                .filter(|callable| callable.name.contains("::")),
        )
        .cloned()
        .collect::<Vec<_>>();
    let namespace_member_groups = stable_groups(&namespace_member_group_input, |callable| {
        callable.name.clone()
    });
    for variant in &builtin_variant_order {
        if !actual_builtin_by_variant.contains_key(variant) {
            panic!("missing callable signatures for builtin variant '{variant}'");
        }
    }

    let mut out = String::new();
    out.push_str(&render_callable_consts(
        &host_callables
            .iter()
            .chain(builtin_callables.iter())
            .chain(metadata_callables.iter())
            .collect::<Vec<_>>(),
    ));

    writeln!(
        &mut out,
        "#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]"
    )
    .unwrap();
    writeln!(&mut out, "#[repr(u16)]").unwrap();
    writeln!(&mut out, "pub enum BuiltinFunction {{").unwrap();
    for variant in &builtin_variant_order {
        let id = catalog_id_by_variant
            .get(variant)
            .unwrap_or_else(|| panic!("missing static id for builtin variant '{variant}'"));
        writeln!(&mut out, "    {variant} = 0x{id:04X},").unwrap();
    }
    writeln!(&mut out, "}}").unwrap();
    writeln!(&mut out).unwrap();

    for group in &language_groups {
        render_signature_group_const(
            &mut out,
            group,
            &language_signature_group_const_name(&group.key),
        );
    }
    for variant in &builtin_variant_order {
        let items = actual_builtin_by_variant
            .get(variant)
            .unwrap_or_else(|| panic!("missing builtin variant group '{variant}'"));
        render_signature_group_const(
            &mut out,
            &Group {
                key: variant.clone(),
                items: items.clone(),
            },
            &variant_signature_group_const_name(variant),
        );
    }
    for group in &namespace_member_groups {
        render_signature_group_const(
            &mut out,
            group,
            &namespace_member_signature_group_const_name(&group.key),
        );
    }

    render_namespace_metadata(&mut out, namespaces, &namespace_member_groups);
    render_default_host_array(&mut out, &host_groups);
    render_language_builtin_specs(&mut out, &language_builtin_order, &language_groups);
    render_namespace_member_signature_lookup(&mut out, &namespace_member_groups);

    writeln!(
        &mut out,
        "/// Every VM-visible builtin in catalog order; the enum discriminants are the static IDs."
    )
    .unwrap();
    writeln!(
        &mut out,
        "pub const BUILTIN_CATALOG: &[BuiltinFunction] = &["
    )
    .unwrap();
    for variant in &builtin_variant_order {
        writeln!(&mut out, "    BuiltinFunction::{variant},").unwrap();
    }
    writeln!(&mut out, "];").unwrap();
    writeln!(&mut out).unwrap();

    writeln!(
        &mut out,
        "pub fn language_builtin_specs() -> &'static [LanguageBuiltinSpec] {{"
    )
    .unwrap();
    writeln!(&mut out, "    &LANGUAGE_BUILTIN_SPECS").unwrap();
    writeln!(&mut out, "}}").unwrap();
    writeln!(&mut out).unwrap();

    writeln!(
        &mut out,
        "pub fn default_host_callables() -> &'static [CallableDef] {{"
    )
    .unwrap();
    writeln!(&mut out, "    &DEFAULT_HOST_CALLABLES").unwrap();
    writeln!(&mut out, "}}").unwrap();
    writeln!(&mut out).unwrap();

    writeln!(
        &mut out,
        "pub(crate) fn default_host_callable(name: &str) -> Option<&'static CallableDef> {{"
    )
    .unwrap();
    writeln!(
        &mut out,
        "    DEFAULT_HOST_CALLABLES.iter().find(|callable| callable.name == name)"
    )
    .unwrap();
    writeln!(&mut out, "}}").unwrap();
    writeln!(&mut out).unwrap();

    writeln!(
        &mut out,
        "pub fn builtin_namespace_specs() -> &'static [BuiltinNamespaceSpec] {{"
    )
    .unwrap();
    writeln!(&mut out, "    BUILTIN_NAMESPACE_SPECS").unwrap();
    writeln!(&mut out, "}}").unwrap();
    writeln!(&mut out).unwrap();

    writeln!(
        &mut out,
        "pub fn is_builtin_namespace(namespace: &str) -> bool {{"
    )
    .unwrap();
    writeln!(
        &mut out,
        "    BUILTIN_NAMESPACE_SPECS.iter().any(|entry| entry.namespace == namespace)"
    )
    .unwrap();
    writeln!(&mut out, "}}").unwrap();
    writeln!(&mut out).unwrap();

    writeln!(
        &mut out,
        "pub fn resolve_builtin_namespace_call(namespace: &str, member: &str) -> Option<BuiltinFunction> {{"
    )
    .unwrap();
    writeln!(
        &mut out,
        "    let entry = BUILTIN_NAMESPACE_LOOKUPS.iter().find(|entry| entry.name == namespace)?;"
    )
    .unwrap();
    writeln!(
        &mut out,
        "    entry.members.iter().find(|item| item.name == member).map(|item| item.builtin)"
    )
    .unwrap();
    writeln!(&mut out, "}}").unwrap();
    writeln!(&mut out).unwrap();

    writeln!(
        &mut out,
        "pub(crate) fn builtin_namespace_hint() -> String {{"
    )
    .unwrap();
    writeln!(
        &mut out,
        "    BUILTIN_NAMESPACE_SPECS.iter().map(|entry| entry.namespace).collect::<Vec<_>>().join(\"/\")"
    )
    .unwrap();
    writeln!(&mut out, "}}").unwrap();
    writeln!(&mut out).unwrap();

    writeln!(
        &mut out,
        "pub(crate) fn resolve_namespaced_builtin(name: &str) -> Option<BuiltinFunction> {{"
    )
    .unwrap();
    writeln!(&mut out, "    let mut parts = name.trim().split(\"::\");").unwrap();
    writeln!(&mut out, "    let namespace = parts.next()?;").unwrap();
    writeln!(&mut out, "    let member = parts.next()?;").unwrap();
    writeln!(&mut out, "    if parts.next().is_some() {{ return None; }}").unwrap();
    writeln!(
        &mut out,
        "    resolve_builtin_namespace_call(namespace, member)"
    )
    .unwrap();
    writeln!(&mut out, "}}").unwrap();
    writeln!(&mut out).unwrap();

    writeln!(&mut out, "impl BuiltinFunction {{").unwrap();
    render_builtin_name_method(&mut out, &builtin_variant_order, &actual_builtin_by_variant);
    render_builtin_arity_method(&mut out, &builtin_variant_order, &actual_builtin_by_variant);
    render_builtin_accepts_arity_method(
        &mut out,
        &builtin_variant_order,
        &actual_builtin_by_variant,
    );
    render_builtin_static_return_type_method(
        &mut out,
        &builtin_variant_order,
        &actual_builtin_by_variant,
    );
    render_builtin_signature_method(&mut out, &builtin_variant_order);
    writeln!(
        &mut out,
        "    pub fn from_namespaced_name(name: &str) -> Option<Self> {{"
    )
    .unwrap();
    writeln!(&mut out, "        resolve_namespaced_builtin(name)").unwrap();
    writeln!(&mut out, "    }}").unwrap();
    writeln!(&mut out).unwrap();
    writeln!(
        &mut out,
        "    pub fn from_source_name(name: &str) -> Option<Self> {{"
    )
    .unwrap();
    writeln!(&mut out, "        match name {{").unwrap();
    for entry in catalog {
        writeln!(
            &mut out,
            "            {:?} => Some(Self::{}),",
            entry.source_name, entry.variant
        )
        .unwrap();
    }
    writeln!(&mut out, "            _ => None,").unwrap();
    writeln!(&mut out, "        }}").unwrap();
    writeln!(&mut out, "    }}").unwrap();
    writeln!(&mut out).unwrap();
    writeln!(&mut out, "    pub fn call_index(self) -> u16 {{").unwrap();
    writeln!(
        &mut out,
        "        // The enum is #[repr(u16)] with explicit static ID discriminants."
    )
    .unwrap();
    writeln!(&mut out, "        self as u16").unwrap();
    writeln!(&mut out, "    }}").unwrap();
    writeln!(&mut out).unwrap();
    writeln!(
        &mut out,
        "    pub(crate) fn from_call_index(index: u16) -> Option<Self> {{"
    )
    .unwrap();
    writeln!(&mut out, "        match index {{").unwrap();
    for variant in &builtin_variant_order {
        let id = catalog_id_by_variant
            .get(variant)
            .unwrap_or_else(|| panic!("missing static id for builtin variant '{variant}'"));
        writeln!(&mut out, "            0x{id:04X} => Some(Self::{variant}),").unwrap();
    }
    writeln!(&mut out, "            _ => None,").unwrap();
    writeln!(&mut out, "        }}").unwrap();
    writeln!(&mut out, "    }}").unwrap();
    writeln!(&mut out, "}}").unwrap();

    out
}

fn render_builtin_runtime_dispatch(
    host_callables: &[CallableDecl],
    builtin_callables: &[CallableDecl],
) -> String {
    let mut out = String::new();

    for callable in host_callables {
        let wrapper = callable
            .wrapper
            .as_ref()
            .expect("host wrappers should exist");
        let adapter_name = host_wrapper_adapter_name(callable);
        if callable.host_binding_kind == HostBindingKind::StaticStack {
            writeln!(
                &mut out,
                "fn {adapter_name}(vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {{"
            )
            .unwrap();
        } else {
            writeln!(
                &mut out,
                "fn {adapter_name}(args: &[Value]) -> VmResult<CallOutcome> {{"
            )
            .unwrap();
        }
        writeln!(
            &mut out,
            "    {}",
            render_wrapper_call(
                &callable.module,
                wrapper,
                SourceCategory::DefaultHost,
                "args",
            )
        )
        .unwrap();
        writeln!(&mut out, "}}").unwrap();
        writeln!(&mut out).unwrap();
    }

    writeln!(
        &mut out,
        "pub(crate) fn register_default_host_functions(registry: &mut super::HostFunctionRegistry) {{"
    )
    .unwrap();
    for callable in host_callables {
        if callable.host_binding_kind == HostBindingKind::StaticStack {
            writeln!(
                &mut out,
                "    registry.register_static_stack({:?}, {}, {});",
                callable.name,
                callable.params.len(),
                host_wrapper_adapter_name(callable)
            )
            .unwrap();
        } else if callable.host_binding_kind == HostBindingKind::StaticNonYieldingArgs {
            writeln!(
                &mut out,
                "    registry.register_static_non_yielding_args({:?}, {}, {});",
                callable.name,
                callable.params.len(),
                host_wrapper_adapter_name(callable)
            )
            .unwrap();
        } else {
            writeln!(
                &mut out,
                "    registry.register_static_args({:?}, {}, {});",
                callable.name,
                callable.params.len(),
                host_wrapper_adapter_name(callable)
            )
            .unwrap();
        }
    }
    writeln!(&mut out, "}}").unwrap();
    writeln!(&mut out).unwrap();

    writeln!(
        &mut out,
        "pub(crate) fn bind_default_host_function(vm: &mut Vm, name: &str) -> bool {{"
    )
    .unwrap();
    writeln!(&mut out, "    match name {{").unwrap();
    for callable in host_callables {
        let bind_call = callable
            .host_binding_kind
            .render_bind_static_call(&callable.name, &host_wrapper_adapter_name(callable));
        writeln!(&mut out, "        {:?} => {{", callable.name).unwrap();
        writeln!(&mut out, "            {bind_call}").unwrap();
        writeln!(&mut out, "            true").unwrap();
        writeln!(&mut out, "        }}").unwrap();
    }
    writeln!(&mut out, "        _ => false,").unwrap();
    writeln!(&mut out, "    }}").unwrap();
    writeln!(&mut out, "}}").unwrap();
    writeln!(&mut out).unwrap();

    writeln!(
        &mut out,
        "fn execute_namespaced_builtin_call(vm: &mut Vm, builtin: BuiltinFunction, args: &mut [Value]) -> VmResult<BuiltinCallOutcome> {{"
    )
    .unwrap();
    writeln!(&mut out, "    match builtin {{").unwrap();
    for callable in builtin_callables
        .iter()
        .filter(|callable| callable.wrapper.is_some())
    {
        let variant = builtin_variant_name(&callable.name);
        let wrapper = callable
            .wrapper
            .as_ref()
            .expect("builtin wrappers should exist");
        writeln!(
            &mut out,
            "        BuiltinFunction::{variant} => {},",
            render_wrapper_call(
                &callable.module,
                wrapper,
                SourceCategory::NamespacedBuiltin,
                "args",
            )
        )
        .unwrap();
    }
    writeln!(
        &mut out,
        "        _ => unreachable!(\"execute_namespaced_builtin_call only handles namespaced builtins\"),"
    )
    .unwrap();
    writeln!(&mut out, "    }}").unwrap();
    writeln!(&mut out, "}}").unwrap();

    out
}

fn render_callable_consts(callables: &[&CallableDecl]) -> String {
    let mut out = String::new();
    for callable in callables {
        let base = callable_const_base(callable);
        writeln!(
            &mut out,
            "const {base}_PARAMS: [CallableParam; {}] = [",
            callable.params.len()
        )
        .unwrap();
        for param in &callable.params {
            writeln!(
                &mut out,
                "    CallableParam {{ name: {:?}, ty: CallableParamType::{}, optional: {} }},",
                param.name,
                callable_param_variant(&param.ty_label),
                param.optional
            )
            .unwrap();
        }
        writeln!(&mut out, "];").unwrap();
        writeln!(
            &mut out,
            "const {base}_SIGNATURE: CallableSignature = CallableSignature {{ params: &{base}_PARAMS, return_type: {:?} }};",
            callable.return_label
        )
        .unwrap();
        writeln!(
            &mut out,
            "#[allow(dead_code)]\nconst {base}_DEF: CallableDef = CallableDef {{ name: {:?}, docs: {:?}, signature: {base}_SIGNATURE, host_execution: HostExecution::{} }};",
            callable.name,
            callable.docs,
            match callable.host_execution {
                HostExecutionKind::Sync => "Sync",
                HostExecutionKind::MaySuspend => "MaySuspend",
            }
        )
        .unwrap();
        writeln!(&mut out).unwrap();
    }
    out
}

fn render_signature_group_const(out: &mut String, group: &Group<'_>, const_name: &str) {
    writeln!(
        out,
        "const {const_name}: [CallableSignature; {}] = [",
        group.items.len()
    )
    .unwrap();
    for callable in &group.items {
        writeln!(out, "    {}_SIGNATURE,", callable_const_base(callable)).unwrap();
    }
    writeln!(out, "];").unwrap();
    writeln!(out).unwrap();
}

fn render_namespace_metadata(out: &mut String, namespaces: &[NamespaceDecl], groups: &[Group<'_>]) {
    for namespace in namespaces {
        let module_name = format!("namespace_{}", namespace.namespace);
        writeln!(&mut *out, "mod {module_name} {{").unwrap();
        writeln!(
            &mut *out,
            "    use super::{{BuiltinFunction, BuiltinNamespaceLookup, BuiltinNamespaceMemberLookup, BuiltinNamespaceMemberSpec, BuiltinNamespaceSpec, ValueType}};"
        )
        .unwrap();
        let namespace_groups = groups
            .iter()
            .filter(|group| group.key.starts_with(&(namespace.namespace.clone() + "::")))
            .collect::<Vec<_>>();
        writeln!(
            &mut *out,
            "    pub(super) const MEMBERS: &[BuiltinNamespaceMemberSpec] = &["
        )
        .unwrap();
        for group in &namespace_groups {
            let member_name = group
                .key
                .split_once("::")
                .map(|(_, member)| member)
                .expect("namespaced callable group should include ::");
            let docs = group
                .items
                .first()
                .map(|callable| callable.docs.as_str())
                .unwrap_or("");
            let arity = group
                .items
                .iter()
                .map(|callable| callable.params.len())
                .max()
                .unwrap_or(0);
            let return_type = group
                .items
                .first()
                .map(|callable| callable.static_return_type.as_str())
                .unwrap_or("Unknown");
            writeln!(
                &mut *out,
                "        BuiltinNamespaceMemberSpec::new({member_name:?}, {arity}, ValueType::{return_type}, {docs:?}),"
            )
            .unwrap();
        }
        writeln!(&mut *out, "    ];").unwrap();
        writeln!(
            &mut *out,
            "    pub(super) const LOOKUP_MEMBERS: &[BuiltinNamespaceMemberLookup] = &["
        )
        .unwrap();
        for group in &namespace_groups {
            let member_name = group
                .key
                .split_once("::")
                .map(|(_, member)| member)
                .expect("namespaced callable group should include ::");
            writeln!(
                &mut *out,
                "        BuiltinNamespaceMemberLookup::new({member_name:?}, BuiltinFunction::{}),",
                namespace_member_target_variant(&group.key)
            )
            .unwrap();
        }
        writeln!(&mut *out, "    ];").unwrap();
        writeln!(
            &mut *out,
            "    pub(super) const LOOKUP: BuiltinNamespaceLookup = BuiltinNamespaceLookup::new({:?}, LOOKUP_MEMBERS);",
            namespace.namespace
        )
        .unwrap();
        writeln!(
            &mut *out,
            "    pub(super) const SPEC: BuiltinNamespaceSpec = BuiltinNamespaceSpec::new({:?}, {:?}, {}, MEMBERS);",
            namespace.namespace,
            namespace.docs,
            namespace.runtime_supported_on_wasm
        )
        .unwrap();
        writeln!(&mut *out, "}}").unwrap();
        writeln!(&mut *out).unwrap();
    }

    writeln!(
        &mut *out,
        "const BUILTIN_NAMESPACE_LOOKUPS: &[BuiltinNamespaceLookup] = &["
    )
    .unwrap();
    for namespace in namespaces {
        writeln!(&mut *out, "    namespace_{}::LOOKUP,", namespace.namespace).unwrap();
    }
    writeln!(&mut *out, "];").unwrap();
    writeln!(&mut *out).unwrap();

    writeln!(
        &mut *out,
        "const BUILTIN_NAMESPACE_SPECS: &[BuiltinNamespaceSpec] = &["
    )
    .unwrap();
    for namespace in namespaces {
        writeln!(&mut *out, "    namespace_{}::SPEC,", namespace.namespace).unwrap();
    }
    writeln!(&mut *out, "];").unwrap();
    writeln!(&mut *out).unwrap();
}

fn render_default_host_array(out: &mut String, groups: &[Group<'_>]) {
    writeln!(
        out,
        "const DEFAULT_HOST_CALLABLES: [CallableDef; {}] = [",
        groups.len()
    )
    .unwrap();
    for group in groups {
        let callable = group.items.first().expect("host group should not be empty");
        writeln!(out, "    {}_DEF,", callable_const_base(callable)).unwrap();
    }
    writeln!(out, "];").unwrap();
    writeln!(out).unwrap();
}

fn render_language_builtin_specs(
    out: &mut String,
    language_builtin_order: &[String],
    groups: &[Group<'_>],
) {
    writeln!(
        out,
        "const LANGUAGE_BUILTIN_SPECS: [LanguageBuiltinSpec; {}] = [",
        language_builtin_order.len()
    )
    .unwrap();
    for name in language_builtin_order {
        let group = groups
            .iter()
            .find(|group| group.key == *name)
            .unwrap_or_else(|| panic!("missing language builtin group '{name}'"));
        let docs = group
            .items
            .first()
            .map(|callable| callable.docs.as_str())
            .unwrap_or("");
        writeln!(
            out,
            "    LanguageBuiltinSpec {{ name: {:?}, docs: {:?}, signatures: &{} }},",
            name,
            docs,
            language_signature_group_const_name(name)
        )
        .unwrap();
    }
    writeln!(out, "];").unwrap();
    writeln!(out).unwrap();
}

fn render_namespace_member_signature_lookup(out: &mut String, groups: &[Group<'_>]) {
    writeln!(
        out,
        "pub fn callable_signatures_for_builtin_namespace_member(namespace: &str, member: &str, arity: usize) -> Option<&'static [CallableSignature]> {{"
    )
    .unwrap();
    writeln!(
        out,
        "    let signatures: &'static [CallableSignature] = match (namespace, member) {{"
    )
    .unwrap();
    for group in groups {
        let (namespace, member) = group
            .key
            .split_once("::")
            .expect("namespaced callable group should include ::");
        writeln!(
            out,
            "        ({namespace:?}, {member:?}) => &{},",
            namespace_member_signature_group_const_name(&group.key)
        )
        .unwrap();
    }
    writeln!(out, "        _ => return None,").unwrap();
    writeln!(out, "    }};").unwrap();
    writeln!(
        out,
        "    if signatures.iter().any(|signature| {{ let required = signature.params.iter().take_while(|param| !param.optional).count(); required <= arity && arity <= signature.params.len() }}) {{"
    )
    .unwrap();
    writeln!(out, "        Some(signatures)").unwrap();
    writeln!(out, "    }} else {{").unwrap();
    writeln!(out, "        None").unwrap();
    writeln!(out, "    }}").unwrap();
    writeln!(out, "}}").unwrap();
    writeln!(out).unwrap();
}

fn render_builtin_name_method(
    out: &mut String,
    builtin_variant_order: &[String],
    actual_builtin_by_variant: &HashMap<String, Vec<&CallableDecl>>,
) {
    writeln!(out, "    pub fn name(self) -> &'static str {{").unwrap();
    writeln!(out, "        match self {{").unwrap();
    for variant in builtin_variant_order {
        let internal_name = builtin_internal_name(variant, actual_builtin_by_variant);
        writeln!(
            out,
            "            BuiltinFunction::{variant} => {internal_name:?},"
        )
        .unwrap();
    }
    writeln!(out, "        }}").unwrap();
    writeln!(out, "    }}").unwrap();
    writeln!(out).unwrap();
}

fn render_builtin_arity_method(
    out: &mut String,
    builtin_variant_order: &[String],
    actual_builtin_by_variant: &HashMap<String, Vec<&CallableDecl>>,
) {
    writeln!(out, "    pub fn arity(self) -> u8 {{").unwrap();
    writeln!(out, "        match self {{").unwrap();
    for variant in builtin_variant_order {
        let arity = actual_builtin_by_variant
            .get(variant)
            .map(|items| {
                items
                    .iter()
                    .map(|callable| required_param_count(&callable.params))
                    .min()
                    .unwrap_or(0)
            })
            .unwrap_or_else(|| panic!("missing arity for builtin variant '{variant}'"));
        writeln!(out, "            BuiltinFunction::{variant} => {arity},").unwrap();
    }
    writeln!(out, "        }}").unwrap();
    writeln!(out, "    }}").unwrap();
    writeln!(out).unwrap();
}

fn render_builtin_accepts_arity_method(
    out: &mut String,
    builtin_variant_order: &[String],
    actual_builtin_by_variant: &HashMap<String, Vec<&CallableDecl>>,
) {
    writeln!(out, "    pub fn accepts_arity(self, arity: u8) -> bool {{").unwrap();
    writeln!(out, "        match self {{").unwrap();
    for variant in builtin_variant_order {
        let mut conditions = actual_builtin_by_variant
            .get(variant)
            .unwrap_or_else(|| panic!("missing signatures for builtin variant '{variant}'"))
            .iter()
            .map(|callable| {
                let min = required_param_count(&callable.params);
                let max = callable.params.len();
                if min == max {
                    format!("arity as usize == {min}")
                } else {
                    format!("({min}..={max}).contains(&(arity as usize))")
                }
            })
            .collect::<Vec<_>>();
        conditions.dedup();
        let conditions = conditions.join(" || ");
        writeln!(
            out,
            "            BuiltinFunction::{variant} => {conditions},"
        )
        .unwrap();
    }
    writeln!(out, "        }}").unwrap();
    writeln!(out, "    }}").unwrap();
    writeln!(out).unwrap();
}

fn render_builtin_static_return_type_method(
    out: &mut String,
    builtin_variant_order: &[String],
    actual_builtin_by_variant: &HashMap<String, Vec<&CallableDecl>>,
) {
    writeln!(
        out,
        "    pub(crate) fn static_return_type(self) -> ValueType {{"
    )
    .unwrap();
    writeln!(out, "        match self {{").unwrap();
    for variant in builtin_variant_order {
        let value_type = actual_builtin_by_variant
            .get(variant)
            .and_then(|items| items.first())
            .map(|callable| callable.static_return_type.as_str())
            .unwrap_or_else(|| panic!("missing return type for builtin variant '{variant}'"));
        writeln!(
            out,
            "            BuiltinFunction::{variant} => ValueType::{value_type},"
        )
        .unwrap();
    }
    writeln!(out, "        }}").unwrap();
    writeln!(out, "    }}").unwrap();
    writeln!(out).unwrap();
}

fn render_builtin_signature_method(out: &mut String, builtin_variant_order: &[String]) {
    writeln!(
        out,
        "    pub(crate) fn callable_signatures(self) -> &'static [CallableSignature] {{"
    )
    .unwrap();
    writeln!(out, "        match self {{").unwrap();
    for variant in builtin_variant_order {
        writeln!(
            out,
            "            BuiltinFunction::{variant} => &{},",
            variant_signature_group_const_name(variant)
        )
        .unwrap();
    }
    writeln!(out, "        }}").unwrap();
    writeln!(out, "    }}").unwrap();
    writeln!(out).unwrap();
}

fn required_param_count(params: &[CallableParamDecl]) -> usize {
    params.iter().take_while(|param| !param.optional).count()
}

fn stable_groups<F>(callables: &[CallableDecl], mut key_fn: F) -> Vec<Group<'_>>
where
    F: FnMut(&CallableDecl) -> String,
{
    let mut groups = Vec::<Group<'_>>::new();
    let mut positions = HashMap::<String, usize>::new();
    for callable in callables {
        let key = key_fn(callable);
        if let Some(index) = positions.get(&key).copied() {
            groups[index].items.push(callable);
        } else {
            positions.insert(key.clone(), groups.len());
            groups.push(Group {
                key,
                items: vec![callable],
            });
        }
    }
    groups
}

fn callable_const_base(callable: &CallableDecl) -> String {
    let prefix = callable.module.replace("::", "_");
    to_shouty_snake(&format!("{prefix}_{}", callable.rust_ident))
}

fn callable_param_variant(label: &str) -> &'static str {
    match label {
        "any" => "Any",
        "null" => "Null",
        "int" => "Int",
        "float" => "Float",
        "bool" => "Bool",
        "string" => "String",
        "bytes" => "Bytes",
        "array" => "Array",
        "map" => "Map",
        "number" => "Number",
        other => panic!("unsupported callable param type '{other}'"),
    }
}

fn core_prefix_builtin_order() -> &'static [&'static str] {
    &[
        "len",
        "slice",
        "concat",
        "array_new",
        "array_push",
        "map_new",
        "get",
        "has",
        "set",
        "keys",
    ]
}

fn core_suffix_builtin_order() -> &'static [&'static str] {
    &["count"]
}

fn special_builtin_order() -> &'static [&'static str] {
    &["__format_template", "__to_string", "type", "assert"]
}

fn appended_builtin_order() -> &'static [&'static str] {
    &[
        "string_contains",
        "string_replace_literal",
        "string_lower_ascii",
        "string_split_literal",
        "__map_iter_init",
        "__map_iter_next",
        "__map_iter_take_key",
        "__map_iter_take_value",
        "__map_iter_close",
        "__bind_callable",
        "__detach_local",
    ]
}

fn required_language_builtin_stubs() -> &'static [&'static str] {
    &[
        "len",
        "slice",
        "concat",
        "array_new",
        "array_push",
        "map_new",
        "get",
        "has",
        "set",
        "keys",
        "count",
        "string_contains",
        "string_replace_literal",
        "string_lower_ascii",
        "string_split_literal",
        "type",
        "assert",
    ]
}

fn required_internal_builtin_stubs() -> &'static [&'static str] {
    &[
        "__format_template",
        "__to_string",
        "__bind_callable",
        "__detach_local",
    ]
}

fn is_language_builtin_stub_name(name: &str) -> bool {
    !name.contains("::") && !is_internal_builtin_name(name)
}

fn is_internal_builtin_name(name: &str) -> bool {
    name.starts_with("__")
}

fn ordered_actual_builtin_variants<'a>(
    namespaces: &[NamespaceDecl],
    builtin_callables: &'a [CallableDecl],
    metadata_callables: &'a [CallableDecl],
) -> (Vec<String>, HashMap<String, Vec<&'a CallableDecl>>) {
    let mut actual_builtin_by_variant = HashMap::<String, Vec<&CallableDecl>>::new();
    for callable in builtin_callables {
        let variant = builtin_variant_name(&callable.name);
        actual_builtin_by_variant
            .entry(variant)
            .or_default()
            .push(callable);
    }
    for callable in metadata_callables {
        let variant = builtin_variant_name(&callable.name);
        actual_builtin_by_variant
            .entry(variant)
            .or_default()
            .push(callable);
    }

    let mut ordered = Vec::new();
    let mut seen = HashSet::new();
    for name in core_prefix_builtin_order() {
        push_ordered_variant(&mut ordered, &mut seen, builtin_variant_name(name));
    }
    for namespace in namespaces {
        for callable in builtin_callables
            .iter()
            .filter(|callable| namespace_root(&callable.name) == Some(namespace.namespace.as_str()))
        {
            push_ordered_variant(
                &mut ordered,
                &mut seen,
                builtin_variant_name(&callable.name),
            );
        }
    }
    for name in core_suffix_builtin_order() {
        push_ordered_variant(&mut ordered, &mut seen, builtin_variant_name(name));
    }
    for name in special_builtin_order() {
        push_ordered_variant(&mut ordered, &mut seen, builtin_variant_name(name));
    }
    for name in appended_builtin_order() {
        push_ordered_variant(&mut ordered, &mut seen, builtin_variant_name(name));
    }

    let extras = actual_builtin_by_variant
        .keys()
        .filter(|variant| !seen.contains(*variant))
        .cloned()
        .collect::<Vec<_>>();
    if !extras.is_empty() {
        panic!("unordered builtin variants remain after generation: {extras:?}");
    }

    (ordered, actual_builtin_by_variant)
}

fn push_ordered_variant(out: &mut Vec<String>, seen: &mut HashSet<String>, variant: String) {
    if seen.insert(variant.clone()) {
        out.push(variant);
    }
}

fn namespace_root(name: &str) -> Option<&str> {
    name.split_once("::").map(|(root, _)| root)
}

pub(crate) fn builtin_variant_name(name: &str) -> String {
    match name {
        "type" => "TypeOf".to_string(),
        "__to_string" => "ToString".to_string(),
        "__format_template" => "FormatTemplate".to_string(),
        other => {
            let mut out = String::new();
            for segment in other.split("::") {
                for part in segment.split('_') {
                    if part.is_empty() {
                        continue;
                    }
                    out.push_str(&variant_segment(part));
                }
            }
            if out.is_empty() {
                panic!("unsupported builtin variant name for '{other}'");
            }
            out
        }
    }
}

fn variant_segment(segment: &str) -> String {
    match segment {
        "nan" => "NaN".to_string(),
        "powf" => "PowF".to_string(),
        "powi" => "PowI".to_string(),
        "copysign" => "CopySign".to_string(),
        other => {
            let mut chars = other.chars();
            let Some(first) = chars.next() else {
                return String::new();
            };
            let mut out = String::new();
            out.push(first.to_ascii_uppercase());
            for ch in chars {
                out.push(ch.to_ascii_lowercase());
            }
            out
        }
    }
}

fn namespace_member_target_variant(name: &str) -> String {
    builtin_variant_name(name)
}

fn builtin_internal_name(
    variant: &str,
    actual_builtin_by_variant: &HashMap<String, Vec<&CallableDecl>>,
) -> String {
    match variant {
        "TypeOf" => "type_of".to_string(),
        "ToString" => "__to_string".to_string(),
        "FormatTemplate" => "__format_template".to_string(),
        _ => actual_builtin_by_variant
            .get(variant)
            .and_then(|items| items.first())
            .map(|callable| callable.name.replace("::", "_"))
            .unwrap_or_else(|| panic!("missing builtin name for variant '{variant}'")),
    }
}

fn language_signature_group_const_name(name: &str) -> String {
    format!("LANGUAGE_{}_SIGNATURES", to_shouty_snake(name))
}

fn variant_signature_group_const_name(variant: &str) -> String {
    format!("BUILTIN_{}_SIGNATURES", to_shouty_snake(variant))
}

fn namespace_member_signature_group_const_name(name: &str) -> String {
    format!("MEMBER_{}_SIGNATURES", to_shouty_snake(name))
}

fn render_wrapper_call(
    module: &str,
    wrapper: &WrapperDecl,
    category: SourceCategory,
    slice_args_expr: &str,
) -> String {
    let mut args = Vec::new();
    for param in &wrapper.params {
        match param {
            WrapperParamKind::Vm => args.push("vm".to_string()),
            WrapperParamKind::SliceArgs => args.push(slice_args_expr.to_string()),
        }
    }
    let wrapper_name = match category {
        SourceCategory::NamespacedBuiltin => wrapper.mut_fn_name.as_str(),
        SourceCategory::DefaultHost | SourceCategory::MetadataOnlyBuiltin => {
            wrapper.fn_name.as_str()
        }
    };
    let call = format!("{module}::{wrapper_name}({})", args.join(", "));
    match category {
        SourceCategory::DefaultHost => {
            format!("{call}.map(IntoHostCallOutcome::into_host_call_outcome)")
        }
        SourceCategory::NamespacedBuiltin => {
            format!("{call}.map(IntoBuiltinCallOutcome::into_builtin_call_outcome)")
        }
        SourceCategory::MetadataOnlyBuiltin => call,
    }
}

fn wrapper_name_for_callable(rust_ident: &str) -> String {
    match rust_ident.strip_suffix("_impl") {
        Some(prefix) => prefix.to_string(),
        None => rust_ident.to_string(),
    }
}

fn host_wrapper_adapter_name(callable: &CallableDecl) -> String {
    format!("__pd_host_adapter_{}", callable.rust_ident)
}

fn generated_wrapper_decl(function: &ItemFn) -> WrapperDecl {
    let mut params = Vec::new();
    for input in &function.sig.inputs {
        let FnArg::Typed(pat_type) = input else {
            panic!("methods are not supported in #[pd_host_function] declarations");
        };
        if is_vm_context_type(&pat_type.ty) {
            params.push(WrapperParamKind::Vm);
        }
    }
    params.push(WrapperParamKind::SliceArgs);
    let fn_name = wrapper_name_for_callable(&function.sig.ident.to_string());
    WrapperDecl {
        mut_fn_name: format!("{fn_name}_mut"),
        fn_name,
        params,
    }
}

fn parse_callable_params(function: &ItemFn) -> Vec<CallableParamDecl> {
    function
        .sig
        .inputs
        .iter()
        .filter_map(|input| {
            let FnArg::Typed(pat_type) = input else {
                panic!("methods are not supported in #[pd_host_function] declarations");
            };
            if is_vm_context_type(&pat_type.ty) {
                return None;
            }
            let Pat::Ident(ident) = pat_type.pat.as_ref() else {
                panic!("callable parameters must use identifier patterns");
            };
            let (ty_label, optional) = param_type_label(&pat_type.ty);
            Some(CallableParamDecl {
                name: ident.ident.to_string(),
                ty_label,
                optional,
            })
        })
        .collect()
}

fn param_type_label(ty: &Type) -> (String, bool) {
    match ty {
        Type::Group(group) => param_type_label(&group.elem),
        Type::Paren(paren) => param_type_label(&paren.elem),
        Type::Reference(reference) => param_type_label(&reference.elem),
        Type::Path(path) => {
            let segment = path
                .path
                .segments
                .last()
                .unwrap_or_else(|| panic!("unsupported callable type"));
            if segment.ident == "Option" {
                let syn::PathArguments::AngleBracketed(args) = &segment.arguments else {
                    panic!("Option<T> requires one generic argument");
                };
                let Some(syn::GenericArgument::Type(inner)) = args.args.first() else {
                    panic!("Option<T> requires one generic argument");
                };
                let (inner_label, inner_optional) = param_type_label(inner);
                if inner_optional {
                    panic!("nested Option<T> is not supported in callable parameters");
                }
                (inner_label, true)
            } else {
                (type_label(ty), false)
            }
        }
        _ => (type_label(ty), false),
    }
}

fn pd_host_function_name(attrs: &[Attribute]) -> Option<String> {
    let attr = attrs
        .iter()
        .find(|attr| attr.path().is_ident("pd_host_function"))?;
    let Meta::List(list) = &attr.meta else {
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

fn callable_docs(name: &str, attrs: &[Attribute]) -> String {
    let docs = doc_string(attrs);
    if docs.is_empty() {
        panic!("callable '{name}' is missing /// doc comments");
    }
    docs
}

fn return_type_label(output: &ReturnType) -> String {
    match output {
        ReturnType::Default => "null".to_string(),
        ReturnType::Type(_, ty) => type_label(ty),
    }
}

fn static_return_type_label(output: &ReturnType) -> String {
    value_type_from_label(&return_type_label(output)).to_string()
}

fn type_label(ty: &Type) -> String {
    match ty {
        Type::Group(group) => type_label(&group.elem),
        Type::Paren(paren) => type_label(&paren.elem),
        Type::Reference(reference) => type_label(&reference.elem),
        Type::Slice(slice) => match slice.elem.as_ref() {
            Type::Path(path) => {
                let segment = path
                    .path
                    .segments
                    .last()
                    .unwrap_or_else(|| panic!("unsupported callable type"));
                match segment.ident.to_string().as_str() {
                    "u8" => "bytes".to_string(),
                    _ => panic!("unsupported callable type"),
                }
            }
            _ => panic!("unsupported callable type"),
        },
        Type::Tuple(tuple) if tuple.elems.is_empty() => "null".to_string(),
        Type::Path(path) => {
            let segment = path
                .path
                .segments
                .last()
                .unwrap_or_else(|| panic!("unsupported callable type"));
            let ident = segment.ident.to_string();
            match ident.as_str() {
                "i8" | "i16" | "i32" | "i64" | "i128" | "isize" | "u8" | "u16" | "u32" | "u64"
                | "u128" | "usize" => "int".to_string(),
                "f32" | "f64" => "float".to_string(),
                "bool" => "bool".to_string(),
                "String" | "str" | "VmStringRef" => "string".to_string(),
                "Bytes" | "VmBytes" | "VmBytesRef" | "VmBytesHandle" => "bytes".to_string(),
                "Any" | "AnyValue" | "Value" | "VmValueRef" | "VmValueOwned" => "any".to_string(),
                "Array" | "VmArray" | "VmArrayRef" | "VmArrayHandle" => "array".to_string(),
                "Map" | "VmMap" | "VmMapRef" | "VmMapHandle" => "map".to_string(),
                "Number" | "NumberValue" => "number".to_string(),
                "Unknown" | "UnknownValue" => "unknown".to_string(),
                "CallOutcome" => "unknown".to_string(),
                "Option" => {
                    let syn::PathArguments::AngleBracketed(args) = &segment.arguments else {
                        panic!("Option<T> requires one generic argument");
                    };
                    let Some(syn::GenericArgument::Type(inner)) = args.args.first() else {
                        panic!("Option<T> requires one generic argument");
                    };
                    format!("{} | null", type_label(inner))
                }
                "VmResult" | "HostCallResult" => {
                    let syn::PathArguments::AngleBracketed(args) = &segment.arguments else {
                        panic!("{ident}<T> requires one generic argument");
                    };
                    let Some(syn::GenericArgument::Type(inner)) = args.args.first() else {
                        panic!("{ident}<T> requires one generic argument");
                    };
                    type_label(inner)
                }
                "Vec" => type_label_for_vec(segment),
                _ => panic!("unsupported callable type '{ident}'"),
            }
        }
        _ => panic!("unsupported callable type"),
    }
}

fn type_label_for_vec(segment: &syn::PathSegment) -> String {
    let syn::PathArguments::AngleBracketed(args) = &segment.arguments else {
        panic!("Vec<T> requires one generic argument");
    };
    let Some(syn::GenericArgument::Type(inner)) = args.args.first() else {
        panic!("Vec<T> requires one generic argument");
    };
    if is_value_type(inner) {
        return "array".to_string();
    }
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
                "map".to_string()
            } else {
                panic!("unsupported Vec tuple type in callable metadata")
            }
        }
        _ => panic!("unsupported Vec return type in callable metadata"),
    }
}

fn value_type_from_label(label: &str) -> &'static str {
    match label {
        "null" => "Null",
        "int" => "Int",
        "float" => "Float",
        "bool" => "Bool",
        "string" => "String",
        "bytes" => "Bytes",
        "array" => "Array",
        "map" => "Map",
        _ => "Unknown",
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

fn to_shouty_snake(value: &str) -> String {
    let mut out = String::new();
    let mut prev_is_lower_or_digit = false;
    for ch in value.chars() {
        if !ch.is_ascii_alphanumeric() {
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
    out.trim_matches('_').to_string()
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

fn parse_bool(source: &str) -> (bool, &str) {
    let source = skip_ws(source);
    if let Some(rest) = source.strip_prefix("true") {
        (true, rest)
    } else if let Some(rest) = source.strip_prefix("false") {
        (false, rest)
    } else {
        panic!("expected bool literal");
    }
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
