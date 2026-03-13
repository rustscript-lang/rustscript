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
    params: Vec<WrapperParamKind>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WrapperParamKind {
    Vm,
    SliceArgs,
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
        .map(|namespace| SourceSpec {
            path: format!("src/builtins/runtime/{}.rs", namespace.module),
            module: namespace.module.clone(),
            category: SourceCategory::NamespacedBuiltin,
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
        });
    }
    out
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
    writeln!(&mut out, "pub(crate) enum BuiltinFunction {{").unwrap();
    for (index, variant) in builtin_variant_order.iter().enumerate() {
        if index == 0 {
            writeln!(&mut out, "    {variant} = 0,").unwrap();
        } else {
            writeln!(&mut out, "    {variant},").unwrap();
        }
    }
    writeln!(&mut out, "}}").unwrap();
    writeln!(&mut out).unwrap();

    writeln!(
        &mut out,
        "const MAIN_RANGE_BUILTINS: &[BuiltinFunction] = &["
    )
    .unwrap();
    for variant in main_range_builtin_variants(&builtin_variant_order) {
        writeln!(&mut out, "    BuiltinFunction::{variant},").unwrap();
    }
    writeln!(&mut out, "];").unwrap();
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
        "pub(crate) const BUILTIN_CALL_BASE: u16 = 0xFFB0;"
    )
    .unwrap();
    writeln!(
        &mut out,
        "pub(crate) const BUILTIN_CALL_COUNT: u16 = MAIN_RANGE_BUILTINS.len() as u16;"
    )
    .unwrap();
    writeln!(&mut out).unwrap();
    writeln!(
        &mut out,
        "const SPECIAL_CALL_BUILTINS: &[(u16, BuiltinFunction)] = &["
    )
    .unwrap();
    writeln!(
        &mut out,
        "    (BUILTIN_CALL_BASE - 4, BuiltinFunction::FormatTemplate),"
    )
    .unwrap();
    writeln!(
        &mut out,
        "    (BUILTIN_CALL_BASE - 3, BuiltinFunction::ToString),"
    )
    .unwrap();
    writeln!(
        &mut out,
        "    (BUILTIN_CALL_BASE - 2, BuiltinFunction::TypeOf),"
    )
    .unwrap();
    writeln!(
        &mut out,
        "    (BUILTIN_CALL_BASE - 1, BuiltinFunction::Assert),"
    )
    .unwrap();
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
        "pub(crate) fn is_builtin_namespace(namespace: &str) -> bool {{"
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
        "pub(crate) fn resolve_builtin_namespace_call(namespace: &str, member: &str) -> Option<BuiltinFunction> {{"
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
        "#[cfg(feature = \"runtime\")]\npub(crate) fn resolve_namespaced_builtin(name: &str) -> Option<BuiltinFunction> {{"
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
        "    #[cfg(feature = \"runtime\")]\n    pub(crate) fn from_namespaced_name(name: &str) -> Option<Self> {{"
    )
    .unwrap();
    writeln!(&mut out, "        resolve_namespaced_builtin(name)").unwrap();
    writeln!(&mut out, "    }}").unwrap();
    writeln!(&mut out).unwrap();
    writeln!(&mut out, "    pub(crate) fn call_index(self) -> u16 {{").unwrap();
    writeln!(&mut out, "        match self {{").unwrap();
    writeln!(
        &mut out,
        "            BuiltinFunction::FormatTemplate => BUILTIN_CALL_BASE - 4,"
    )
    .unwrap();
    writeln!(
        &mut out,
        "            BuiltinFunction::ToString => BUILTIN_CALL_BASE - 3,"
    )
    .unwrap();
    writeln!(
        &mut out,
        "            BuiltinFunction::TypeOf => BUILTIN_CALL_BASE - 2,"
    )
    .unwrap();
    writeln!(
        &mut out,
        "            BuiltinFunction::Assert => BUILTIN_CALL_BASE - 1,"
    )
    .unwrap();
    writeln!(
        &mut out,
        "            _ => BUILTIN_CALL_BASE + self as u16,"
    )
    .unwrap();
    writeln!(&mut out, "        }}").unwrap();
    writeln!(&mut out, "    }}").unwrap();
    writeln!(&mut out).unwrap();
    writeln!(
        &mut out,
        "    pub(crate) fn from_call_index(index: u16) -> Option<Self> {{"
    )
    .unwrap();
    writeln!(
        &mut out,
        "        if let Some((_, builtin)) = SPECIAL_CALL_BUILTINS.iter().find(|(call_index, _)| *call_index == index) {{"
    )
    .unwrap();
    writeln!(&mut out, "            return Some(*builtin);").unwrap();
    writeln!(&mut out, "        }}").unwrap();
    writeln!(
        &mut out,
        "        let offset = index.checked_sub(BUILTIN_CALL_BASE)?;"
    )
    .unwrap();
    writeln!(
        &mut out,
        "        if offset >= BUILTIN_CALL_COUNT {{ return None; }}"
    )
    .unwrap();
    writeln!(
        &mut out,
        "        MAIN_RANGE_BUILTINS.get(offset as usize).copied()"
    )
    .unwrap();
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
        if wrapper_uses_vm(wrapper) {
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
        let wrapper = callable
            .wrapper
            .as_ref()
            .expect("host wrappers should exist");
        if wrapper_uses_vm(wrapper) {
            writeln!(
                &mut out,
                "    registry.register_static({:?}, {}, {});",
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
        let wrapper = callable
            .wrapper
            .as_ref()
            .expect("host wrappers should exist");
        writeln!(&mut out, "        {:?} => {{", callable.name).unwrap();
        if wrapper_uses_vm(wrapper) {
            writeln!(
                &mut out,
                "            vm.bind_static_function({:?}, {});",
                callable.name,
                host_wrapper_adapter_name(callable)
            )
            .unwrap();
        } else {
            writeln!(
                &mut out,
                "            vm.bind_static_args_function({:?}, {});",
                callable.name,
                host_wrapper_adapter_name(callable)
            )
            .unwrap();
        }
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
            "#[allow(dead_code)]\nconst {base}_DEF: CallableDef = CallableDef {{ name: {:?}, docs: {:?}, signature: {base}_SIGNATURE }};",
            callable.name,
            callable.docs
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
    writeln!(out, "    pub(crate) fn name(self) -> &'static str {{").unwrap();
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
    writeln!(out, "    pub(crate) fn arity(self) -> u8 {{").unwrap();
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
    writeln!(
        out,
        "    pub(crate) fn accepts_arity(self, arity: u8) -> bool {{"
    )
    .unwrap();
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
        "type",
        "assert",
    ]
}

fn required_internal_builtin_stubs() -> &'static [&'static str] {
    &["__format_template", "__to_string"]
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

fn builtin_variant_name(name: &str) -> String {
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

fn main_range_builtin_variants(builtin_variant_order: &[String]) -> Vec<String> {
    builtin_variant_order
        .iter()
        .filter(|variant| {
            !matches!(
                variant.as_str(),
                "FormatTemplate" | "ToString" | "TypeOf" | "Assert"
            )
        })
        .cloned()
        .collect()
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
    let call = format!("{module}::{}({})", wrapper.fn_name, args.join(", "));
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

fn wrapper_uses_vm(wrapper: &WrapperDecl) -> bool {
    wrapper
        .params
        .iter()
        .any(|param| matches!(param, WrapperParamKind::Vm))
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
    WrapperDecl {
        fn_name: wrapper_name_for_callable(&function.sig.ident.to_string()),
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
                "String" | "str" => "string".to_string(),
                "Any" | "AnyValue" | "Value" => "any".to_string(),
                "Array" | "VmArray" => "array".to_string(),
                "Map" | "VmMap" => "map".to_string(),
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
                "VmResult" | "BuiltinResult" | "HostResult" => {
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
