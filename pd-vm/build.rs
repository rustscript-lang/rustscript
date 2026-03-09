use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
struct NamespaceDecl {
    module: String,
    namespace: String,
    alias: String,
    docs: String,
    runtime_supported_on_wasm: bool,
    supports_regex_flags: bool,
    members: Vec<NamespaceMemberDecl>,
}

#[derive(Clone, Debug)]
enum NamespaceMemberDecl {
    Builtin {
        variant: String,
        member_name: String,
        arity: usize,
        handler: String,
        dispatch: String,
        docs: String,
    },
    Alias {
        variant: String,
        member_name: String,
        arity: usize,
        docs: String,
    },
}

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("missing manifest dir"));
    let builtins_impl_dir = manifest_dir.join("src").join("vm").join("builtins_impl");
    let namespace_list = builtins_impl_dir.join("namespaces.rs");
    let namespace_files = parse_namespace_include_order(&namespace_list);
    let declarations = namespace_files
        .iter()
        .map(|relative| parse_namespace_file(&builtins_impl_dir.join(relative)))
        .collect::<Vec<_>>();
    let (pre_count, post_count) = declarations.split_at(1);

    println!("cargo:rerun-if-changed={}", namespace_list.display());
    for relative in &namespace_files {
        println!(
            "cargo:rerun-if-changed={}",
            builtins_impl_dir.join(relative).display()
        );
    }

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("missing OUT_DIR"));
    write_generated_file(
        &out_dir.join("builtin_namespace_metadata.rs"),
        &render_metadata_modules(&declarations),
    );
    write_generated_file(
        &out_dir.join("builtin_namespace_pre_count_variants.rs"),
        &render_variant_list(pre_count),
    );
    write_generated_file(
        &out_dir.join("builtin_namespace_post_count_variants.rs"),
        &render_variant_list(post_count),
    );
    write_generated_file(
        &out_dir.join("builtin_namespace_pre_count_main_range.rs"),
        &render_main_range_list(pre_count),
    );
    write_generated_file(
        &out_dir.join("builtin_namespace_post_count_main_range.rs"),
        &render_main_range_list(post_count),
    );
    write_generated_file(
        &out_dir.join("builtin_namespace_pre_count_name_arms.rs"),
        &render_name_arms(pre_count),
    );
    write_generated_file(
        &out_dir.join("builtin_namespace_post_count_name_arms.rs"),
        &render_name_arms(post_count),
    );
    write_generated_file(
        &out_dir.join("builtin_namespace_pre_count_arity_arms.rs"),
        &render_arity_arms(pre_count),
    );
    write_generated_file(
        &out_dir.join("builtin_namespace_post_count_arity_arms.rs"),
        &render_arity_arms(post_count),
    );
    write_generated_file(
        &out_dir.join("builtin_namespace_pre_count_dispatch_arms.rs"),
        &render_dispatch_arms(pre_count),
    );
    write_generated_file(
        &out_dir.join("builtin_namespace_post_count_dispatch_arms.rs"),
        &render_dispatch_arms(post_count),
    );
    write_generated_file(
        &out_dir.join("builtin_catalog_generated.rs"),
        &render_builtin_catalog(&declarations),
    );
    write_generated_file(
        &out_dir.join("builtin_namespaced_dispatch_generated.rs"),
        &render_namespaced_dispatch(&declarations),
    );
}

fn write_generated_file(path: &Path, contents: &str) {
    fs::write(path, contents)
        .unwrap_or_else(|err| panic!("failed to write generated file {}: {err}", path.display()));
}

fn parse_namespace_include_order(path: &Path) -> Vec<String> {
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
            .unwrap_or_else(|| panic!("unterminated include! path in {}", path.display()));
        files.push(rest[..end].to_string());
        rest = &rest[end + 1..];
    }
    files
}

fn parse_namespace_file(path: &Path) -> NamespaceDecl {
    let source = fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
    let module = extract_ident_field(&source, "module:");
    let namespace = extract_string_field(&source, "namespace:");
    let alias = extract_string_field(&source, "alias:");
    let docs = extract_string_field(&source, "docs:");
    let runtime_supported_on_wasm = extract_bool_field(&source, "runtime_supported_on_wasm:");
    let supports_regex_flags = extract_bool_field(&source, "supports_regex_flags:");
    let members_block = extract_bracket_block(&source, "members:");
    let members = parse_members(&members_block);
    NamespaceDecl {
        module,
        namespace,
        alias,
        docs,
        runtime_supported_on_wasm,
        supports_regex_flags,
        members,
    }
}

fn extract_ident_field(source: &str, marker: &str) -> String {
    let rest = source
        .split_once(marker)
        .unwrap_or_else(|| panic!("missing field {marker}"))
        .1;
    let (ident, _) = parse_ident(rest);
    ident
}

fn extract_string_field(source: &str, marker: &str) -> String {
    let rest = source
        .split_once(marker)
        .unwrap_or_else(|| panic!("missing field {marker}"))
        .1;
    let (value, _) = parse_string(rest);
    value
}

fn extract_bool_field(source: &str, marker: &str) -> bool {
    let rest = source
        .split_once(marker)
        .unwrap_or_else(|| panic!("missing field {marker}"))
        .1;
    let rest = skip_ws(rest);
    if let Some(rest) = rest.strip_prefix("true") {
        let _ = rest;
        true
    } else if let Some(rest) = rest.strip_prefix("false") {
        let _ = rest;
        false
    } else {
        panic!("invalid bool field {marker}");
    }
}

fn extract_bracket_block(source: &str, marker: &str) -> String {
    let rest = source
        .split_once(marker)
        .unwrap_or_else(|| panic!("missing block {marker}"))
        .1;
    let start = rest
        .find('[')
        .unwrap_or_else(|| panic!("missing '[' for {marker}"));
    let after_start = &rest[start..];
    let end = find_matching(after_start, '[', ']');
    after_start[1..end].to_string()
}

fn parse_members(source: &str) -> Vec<NamespaceMemberDecl> {
    let mut members = Vec::new();
    let mut rest = source;
    loop {
        rest = skip_ws_and_commas(rest);
        if rest.is_empty() {
            break;
        }
        let (kind, after_kind) = parse_ident(rest);
        let after_kind = skip_ws(after_kind);
        let after_bang = after_kind
            .strip_prefix('!')
            .unwrap_or_else(|| panic!("missing ! after member kind {kind}"));
        let after_paren = skip_ws(after_bang)
            .strip_prefix('(')
            .unwrap_or_else(|| panic!("missing ( after member kind {kind}"));
        let paren_end = find_matching_with_offset(after_paren, '(', ')');
        let args = &after_paren[..paren_end];
        let remainder = &after_paren[paren_end + 1..];
        members.push(parse_member(kind.as_str(), args));
        rest = remainder;
    }
    members
}

fn parse_member(kind: &str, args: &str) -> NamespaceMemberDecl {
    let mut rest = args;
    let (variant, next) = parse_ident(rest);
    rest = expect_comma(next);
    let (member_name, next) = parse_string(rest);
    rest = expect_comma(next);
    let (arity, next) = parse_usize(rest);
    rest = next;
    match kind {
        "namespace_builtin" => {
            rest = expect_comma(rest);
            let (handler, next) = parse_ident(rest);
            rest = expect_comma(next);
            let (dispatch, next) = parse_ident(rest);
            rest = expect_comma(next);
            let (docs, next) = parse_string(rest);
            let rest = skip_ws(next);
            if !rest.is_empty() {
                panic!("unexpected trailing tokens in builtin member: {rest}");
            }
            NamespaceMemberDecl::Builtin {
                variant,
                member_name,
                arity,
                handler,
                dispatch,
                docs,
            }
        }
        "namespace_alias" => {
            rest = expect_comma(rest);
            let (docs, next) = parse_string(rest);
            let rest = skip_ws(next);
            if !rest.is_empty() {
                panic!("unexpected trailing tokens in alias member: {rest}");
            }
            NamespaceMemberDecl::Alias {
                variant,
                member_name,
                arity,
                docs,
            }
        }
        other => panic!("unknown member kind {other}"),
    }
}

fn render_metadata_modules(declarations: &[NamespaceDecl]) -> String {
    let mut out = String::new();
    for decl in declarations {
        writeln!(&mut out, "mod {} {{", decl.module).unwrap();
        writeln!(
            &mut out,
            "    use super::{{BuiltinFunction, BuiltinNamespaceLookup, BuiltinNamespaceMemberLookup, BuiltinNamespaceMemberSpec, BuiltinNamespaceSpec}};"
        )
        .unwrap();
        writeln!(
            &mut out,
            "    pub(super) const MEMBERS: &[BuiltinNamespaceMemberSpec] = &["
        )
        .unwrap();
        for member in &decl.members {
            match member {
                NamespaceMemberDecl::Builtin {
                    member_name,
                    arity,
                    docs,
                    ..
                }
                | NamespaceMemberDecl::Alias {
                    member_name,
                    arity,
                    docs,
                    ..
                } => {
                    writeln!(
                        &mut out,
                        "        BuiltinNamespaceMemberSpec::new({member_name:?}, {arity}, {docs:?}),"
                    )
                    .unwrap();
                }
            }
        }
        writeln!(&mut out, "    ];").unwrap();
        writeln!(
            &mut out,
            "    pub(super) const LOOKUP_MEMBERS: &[BuiltinNamespaceMemberLookup] = &["
        )
        .unwrap();
        for member in &decl.members {
            match member {
                NamespaceMemberDecl::Builtin {
                    variant,
                    member_name,
                    ..
                }
                | NamespaceMemberDecl::Alias {
                    variant,
                    member_name,
                    ..
                } => {
                    writeln!(
                        &mut out,
                        "        BuiltinNamespaceMemberLookup::new({member_name:?}, BuiltinFunction::{variant}),"
                    )
                    .unwrap();
                }
            }
        }
        writeln!(&mut out, "    ];").unwrap();
        writeln!(
            &mut out,
            "    pub(super) const LOOKUP: BuiltinNamespaceLookup = BuiltinNamespaceLookup::new({:?}, LOOKUP_MEMBERS);",
            decl.namespace
        )
        .unwrap();
        writeln!(
            &mut out,
            "    pub(super) const SPEC: BuiltinNamespaceSpec = BuiltinNamespaceSpec::new({:?}, {:?}, {:?}, {}, {}, MEMBERS);",
            decl.namespace,
            decl.alias,
            decl.docs,
            decl.runtime_supported_on_wasm,
            decl.supports_regex_flags
        )
        .unwrap();
        writeln!(&mut out, "}}").unwrap();
        writeln!(&mut out).unwrap();
    }
    writeln!(
        &mut out,
        "const BUILTIN_NAMESPACE_LOOKUPS: &[BuiltinNamespaceLookup] = &["
    )
    .unwrap();
    for decl in declarations {
        writeln!(&mut out, "    {}::LOOKUP,", decl.module).unwrap();
    }
    writeln!(&mut out, "];").unwrap();
    writeln!(&mut out).unwrap();
    writeln!(
        &mut out,
        "const BUILTIN_NAMESPACE_SPECS: &[BuiltinNamespaceSpec] = &["
    )
    .unwrap();
    for decl in declarations {
        writeln!(&mut out, "    {}::SPEC,", decl.module).unwrap();
    }
    writeln!(&mut out, "];").unwrap();
    out
}

fn render_variant_list(declarations: &[NamespaceDecl]) -> String {
    render_builtin_members(declarations, |decl, member| match member {
        NamespaceMemberDecl::Builtin { variant, .. } => {
            let _ = decl;
            format!("{variant},")
        }
        NamespaceMemberDecl::Alias { .. } => String::new(),
    })
}

fn render_main_range_list(declarations: &[NamespaceDecl]) -> String {
    render_builtin_members(declarations, |decl, member| match member {
        NamespaceMemberDecl::Builtin { variant, .. } => {
            let _ = decl;
            format!("BuiltinFunction::{variant},")
        }
        NamespaceMemberDecl::Alias { .. } => String::new(),
    })
}

fn render_name_arms(declarations: &[NamespaceDecl]) -> String {
    render_builtin_members(declarations, |decl, member| match member {
        NamespaceMemberDecl::Builtin {
            variant,
            member_name,
            ..
        } => format!(
            "BuiltinFunction::{variant} => {:?},",
            format!("{}_{}", decl.namespace, member_name)
        ),
        NamespaceMemberDecl::Alias { .. } => String::new(),
    })
}

fn render_arity_arms(declarations: &[NamespaceDecl]) -> String {
    render_builtin_members(declarations, |decl, member| match member {
        NamespaceMemberDecl::Builtin { variant, arity, .. } => {
            let _ = decl;
            format!("BuiltinFunction::{variant} => {arity},")
        }
        NamespaceMemberDecl::Alias { .. } => String::new(),
    })
}

fn render_dispatch_arms(declarations: &[NamespaceDecl]) -> String {
    render_builtin_members(declarations, |decl, member| match member {
        NamespaceMemberDecl::Builtin {
            variant,
            handler,
            dispatch,
            ..
        } => match dispatch.as_str() {
            "args_ref" => format!(
                "BuiltinFunction::{variant} => {}::{handler}(&args).map(BuiltinCallOutcome::Return),",
                decl.module
            ),
            "args_owned" => format!(
                "BuiltinFunction::{variant} => {}::{handler}(args).map(BuiltinCallOutcome::Return),",
                decl.module
            ),
            "vm_args_owned" => {
                format!(
                    "BuiltinFunction::{variant} => {}::{handler}(vm, args),",
                    decl.module
                )
            }
            "vm_noargs" => {
                format!(
                    "BuiltinFunction::{variant} => {}::{handler}(vm),",
                    decl.module
                )
            }
            other => panic!("unsupported dispatch kind {other}"),
        },
        NamespaceMemberDecl::Alias { .. } => String::new(),
    })
}

fn render_builtin_catalog(declarations: &[NamespaceDecl]) -> String {
    let (pre_count, post_count) = declarations.split_at(1);
    let mut out = String::new();
    writeln!(
        &mut out,
        "#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]"
    )
    .unwrap();
    writeln!(&mut out, "#[repr(u16)]").unwrap();
    writeln!(&mut out, "pub(crate) enum BuiltinFunction {{").unwrap();
    writeln!(&mut out, "    Len = 0,").unwrap();
    writeln!(&mut out, "    Slice,").unwrap();
    writeln!(&mut out, "    Concat,").unwrap();
    writeln!(&mut out, "    ArrayNew,").unwrap();
    writeln!(&mut out, "    ArrayPush,").unwrap();
    writeln!(&mut out, "    MapNew,").unwrap();
    writeln!(&mut out, "    Get,").unwrap();
    writeln!(&mut out, "    Set,").unwrap();
    writeln!(&mut out, "    Keys,").unwrap();
    for decl in pre_count {
        for member in &decl.members {
            if let NamespaceMemberDecl::Builtin { variant, .. } = member {
                writeln!(&mut out, "    {variant},").unwrap();
            }
        }
    }
    writeln!(&mut out, "    Count,").unwrap();
    for decl in post_count {
        for member in &decl.members {
            if let NamespaceMemberDecl::Builtin { variant, .. } = member {
                writeln!(&mut out, "    {variant},").unwrap();
            }
        }
    }
    writeln!(&mut out, "    FormatTemplate,").unwrap();
    writeln!(&mut out, "    ToString,").unwrap();
    writeln!(&mut out, "    TypeOf,").unwrap();
    writeln!(&mut out, "    Assert,").unwrap();
    writeln!(&mut out, "}}").unwrap();
    writeln!(&mut out).unwrap();

    writeln!(
        &mut out,
        "const MAIN_RANGE_BUILTINS: &[BuiltinFunction] = &["
    )
    .unwrap();
    writeln!(&mut out, "    BuiltinFunction::Len,").unwrap();
    writeln!(&mut out, "    BuiltinFunction::Slice,").unwrap();
    writeln!(&mut out, "    BuiltinFunction::Concat,").unwrap();
    writeln!(&mut out, "    BuiltinFunction::ArrayNew,").unwrap();
    writeln!(&mut out, "    BuiltinFunction::ArrayPush,").unwrap();
    writeln!(&mut out, "    BuiltinFunction::MapNew,").unwrap();
    writeln!(&mut out, "    BuiltinFunction::Get,").unwrap();
    writeln!(&mut out, "    BuiltinFunction::Set,").unwrap();
    writeln!(&mut out, "    BuiltinFunction::Keys,").unwrap();
    for decl in pre_count {
        for member in &decl.members {
            if let NamespaceMemberDecl::Builtin { variant, .. } = member {
                writeln!(&mut out, "    BuiltinFunction::{variant},").unwrap();
            }
        }
    }
    writeln!(&mut out, "    BuiltinFunction::Count,").unwrap();
    for decl in post_count {
        for member in &decl.members {
            if let NamespaceMemberDecl::Builtin { variant, .. } = member {
                writeln!(&mut out, "    BuiltinFunction::{variant},").unwrap();
            }
        }
    }
    writeln!(&mut out, "];").unwrap();
    writeln!(&mut out).unwrap();

    out.push_str(&render_metadata_modules(declarations));
    writeln!(&mut out).unwrap();

    writeln!(&mut out, "impl BuiltinFunction {{").unwrap();
    writeln!(&mut out, "    pub(crate) fn name(self) -> &'static str {{").unwrap();
    writeln!(&mut out, "        match self {{").unwrap();
    writeln!(&mut out, "            BuiltinFunction::Len => \"len\",").unwrap();
    writeln!(&mut out, "            BuiltinFunction::Slice => \"slice\",").unwrap();
    writeln!(
        &mut out,
        "            BuiltinFunction::Concat => \"concat\","
    )
    .unwrap();
    writeln!(
        &mut out,
        "            BuiltinFunction::ArrayNew => \"array_new\","
    )
    .unwrap();
    writeln!(
        &mut out,
        "            BuiltinFunction::ArrayPush => \"array_push\","
    )
    .unwrap();
    writeln!(
        &mut out,
        "            BuiltinFunction::MapNew => \"map_new\","
    )
    .unwrap();
    writeln!(&mut out, "            BuiltinFunction::Get => \"get\",").unwrap();
    writeln!(&mut out, "            BuiltinFunction::Set => \"set\",").unwrap();
    writeln!(&mut out, "            BuiltinFunction::Keys => \"keys\",").unwrap();
    for decl in pre_count {
        for member in &decl.members {
            if let NamespaceMemberDecl::Builtin {
                variant,
                member_name,
                ..
            } = member
            {
                writeln!(
                    &mut out,
                    "            BuiltinFunction::{variant} => {:?},",
                    format!("{}_{}", decl.namespace, member_name)
                )
                .unwrap();
            }
        }
    }
    writeln!(&mut out, "            BuiltinFunction::Count => \"count\",").unwrap();
    for decl in post_count {
        for member in &decl.members {
            if let NamespaceMemberDecl::Builtin {
                variant,
                member_name,
                ..
            } = member
            {
                writeln!(
                    &mut out,
                    "            BuiltinFunction::{variant} => {:?},",
                    format!("{}_{}", decl.namespace, member_name)
                )
                .unwrap();
            }
        }
    }
    writeln!(
        &mut out,
        "            BuiltinFunction::FormatTemplate => \"__format_template\","
    )
    .unwrap();
    writeln!(
        &mut out,
        "            BuiltinFunction::ToString => \"__to_string\","
    )
    .unwrap();
    writeln!(
        &mut out,
        "            BuiltinFunction::TypeOf => \"type_of\","
    )
    .unwrap();
    writeln!(
        &mut out,
        "            BuiltinFunction::Assert => \"assert\","
    )
    .unwrap();
    writeln!(&mut out, "        }}").unwrap();
    writeln!(&mut out, "    }}").unwrap();
    writeln!(&mut out).unwrap();

    writeln!(&mut out, "    pub(crate) fn arity(self) -> u8 {{").unwrap();
    writeln!(&mut out, "        match self {{").unwrap();
    writeln!(&mut out, "            BuiltinFunction::Len => 1,").unwrap();
    writeln!(&mut out, "            BuiltinFunction::Slice => 3,").unwrap();
    writeln!(&mut out, "            BuiltinFunction::Concat => 2,").unwrap();
    writeln!(&mut out, "            BuiltinFunction::ArrayNew => 0,").unwrap();
    writeln!(&mut out, "            BuiltinFunction::ArrayPush => 2,").unwrap();
    writeln!(&mut out, "            BuiltinFunction::MapNew => 0,").unwrap();
    writeln!(&mut out, "            BuiltinFunction::Get => 2,").unwrap();
    writeln!(&mut out, "            BuiltinFunction::Set => 3,").unwrap();
    writeln!(&mut out, "            BuiltinFunction::Keys => 1,").unwrap();
    for decl in pre_count {
        for member in &decl.members {
            if let NamespaceMemberDecl::Builtin { variant, arity, .. } = member {
                writeln!(
                    &mut out,
                    "            BuiltinFunction::{variant} => {arity},"
                )
                .unwrap();
            }
        }
    }
    writeln!(&mut out, "            BuiltinFunction::Count => 1,").unwrap();
    for decl in post_count {
        for member in &decl.members {
            if let NamespaceMemberDecl::Builtin { variant, arity, .. } = member {
                writeln!(
                    &mut out,
                    "            BuiltinFunction::{variant} => {arity},"
                )
                .unwrap();
            }
        }
    }
    writeln!(
        &mut out,
        "            BuiltinFunction::FormatTemplate => 2,"
    )
    .unwrap();
    writeln!(&mut out, "            BuiltinFunction::ToString => 1,").unwrap();
    writeln!(&mut out, "            BuiltinFunction::TypeOf => 1,").unwrap();
    writeln!(&mut out, "            BuiltinFunction::Assert => 1,").unwrap();
    writeln!(&mut out, "        }}").unwrap();
    writeln!(&mut out, "    }}").unwrap();
    writeln!(&mut out, "}}").unwrap();
    out
}

fn render_namespaced_dispatch(declarations: &[NamespaceDecl]) -> String {
    let mut out = String::new();
    writeln!(
        &mut out,
        "fn execute_namespaced_builtin_call(vm: &mut Vm, builtin: BuiltinFunction, args: Vec<Value>) -> VmResult<BuiltinCallOutcome> {{"
    )
    .unwrap();
    writeln!(&mut out, "    match builtin {{").unwrap();
    for decl in declarations {
        for member in &decl.members {
            if let NamespaceMemberDecl::Builtin {
                variant,
                handler,
                dispatch,
                ..
            } = member
            {
                let arm = match dispatch.as_str() {
                    "args_ref" => format!(
                        "BuiltinFunction::{variant} => {}::{handler}(&args).map(BuiltinCallOutcome::Return),",
                        decl.module
                    ),
                    "args_owned" => format!(
                        "BuiltinFunction::{variant} => {}::{handler}(args).map(BuiltinCallOutcome::Return),",
                        decl.module
                    ),
                    "vm_args_owned" => format!(
                        "BuiltinFunction::{variant} => {}::{handler}(vm, args),",
                        decl.module
                    ),
                    "vm_noargs" => {
                        format!(
                            "BuiltinFunction::{variant} => {}::{handler}(vm),",
                            decl.module
                        )
                    }
                    other => panic!("unsupported dispatch kind {other}"),
                };
                writeln!(&mut out, "        {arm}").unwrap();
            }
        }
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

fn render_builtin_members(
    declarations: &[NamespaceDecl],
    render: impl Fn(&NamespaceDecl, &NamespaceMemberDecl) -> String,
) -> String {
    let mut out = String::new();
    for decl in declarations {
        for member in &decl.members {
            let rendered = render(decl, member);
            if !rendered.is_empty() {
                writeln!(&mut out, "{rendered}").unwrap();
            }
        }
    }
    out
}

fn parse_ident(source: &str) -> (String, &str) {
    let source = skip_ws(source);
    let end = source
        .find(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
        .unwrap_or(source.len());
    if end == 0 {
        panic!("expected identifier");
    }
    (source[..end].to_string(), &source[end..])
}

fn parse_usize(source: &str) -> (usize, &str) {
    let source = skip_ws(source);
    let end = source
        .find(|ch: char| !ch.is_ascii_digit())
        .unwrap_or(source.len());
    if end == 0 {
        panic!("expected usize");
    }
    (
        source[..end]
            .parse::<usize>()
            .unwrap_or_else(|err| panic!("invalid usize '{}': {err}", &source[..end])),
        &source[end..],
    )
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
            '"' => {
                let end = 1 + index;
                return (value, &source[end + 1..]);
            }
            other => value.push(other),
        }
    }
    panic!("unterminated string literal");
}

fn expect_comma(source: &str) -> &str {
    skip_ws(source)
        .strip_prefix(',')
        .unwrap_or_else(|| panic!("expected comma in '{source}'"))
}

fn skip_ws(source: &str) -> &str {
    source.trim_start_matches(char::is_whitespace)
}

fn skip_ws_and_commas(source: &str) -> &str {
    source.trim_start_matches(|ch: char| ch.is_whitespace() || ch == ',')
}

fn find_matching(source: &str, open: char, close: char) -> usize {
    let source = skip_ws(source);
    let source = source
        .strip_prefix(open)
        .unwrap_or_else(|| panic!("expected '{open}'"));
    find_matching_with_offset(source, open, close) + 1
}

fn find_matching_with_offset(source: &str, open: char, close: char) -> usize {
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
            _ if ch == open => depth += 1,
            _ if ch == close => {
                depth -= 1;
                if depth == 0 {
                    return index;
                }
            }
            _ => {}
        }
    }
    panic!("unterminated block starting with '{open}'");
}
