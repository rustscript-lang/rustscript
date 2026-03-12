use edge_abi::{
    AbiParamType, AbiValueType, FUNCTIONS as EDGE_HOST_FUNCTIONS, host_namespace_specs,
};
use serde::Serialize;
use vm::{
    CallableParam, CallableSignature, builtin_namespace_specs,
    callable_signatures_for_builtin_namespace_member, default_host_callables,
    language_builtin_specs,
};

use crate::stdlib::embedded_stdlib_modules;

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct CompletionEntry {
    pub label: String,
    pub insert_text: String,
    pub detail: String,
    pub documentation: String,
    pub kind: String,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct CompletionCatalog {
    pub rustscript: Vec<CompletionEntry>,
    pub javascript: Vec<CompletionEntry>,
    pub lua: Vec<CompletionEntry>,
    pub scheme: Vec<CompletionEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ParsedFunction {
    name: String,
    params: Vec<String>,
}

pub fn build_completion_catalog() -> CompletionCatalog {
    let mut rustscript = Vec::new();
    let mut javascript = Vec::new();
    let mut lua = Vec::new();
    let mut scheme = Vec::new();

    add_edge_host_import_entries(&mut rustscript, &mut javascript, &mut lua, &mut scheme);
    add_edge_host_function_entries(&mut rustscript, &mut javascript, &mut lua, &mut scheme);
    add_host_function_entries(&mut rustscript, &mut javascript, &mut lua, &mut scheme);
    add_language_builtin_entries(&mut rustscript, &mut javascript, &mut lua, &mut scheme);
    add_builtin_namespace_entries(&mut rustscript, &mut javascript, &mut lua, &mut scheme);
    add_stdlib_entries(&mut rustscript, &mut javascript, &mut lua, &mut scheme);

    rustscript.sort_by(|lhs, rhs| lhs.label.cmp(&rhs.label));
    javascript.sort_by(|lhs, rhs| lhs.label.cmp(&rhs.label));
    lua.sort_by(|lhs, rhs| lhs.label.cmp(&rhs.label));
    scheme.sort_by(|lhs, rhs| lhs.label.cmp(&rhs.label));

    CompletionCatalog {
        rustscript,
        javascript,
        lua,
        scheme,
    }
}

fn add_language_builtin_entries(
    rustscript: &mut Vec<CompletionEntry>,
    javascript: &mut Vec<CompletionEntry>,
    lua: &mut Vec<CompletionEntry>,
    scheme: &mut Vec<CompletionEntry>,
) {
    for builtin in language_builtin_specs() {
        let params = signature_params(builtin.signatures);
        let detail = format!(
            "builtin {}",
            overload_signatures(builtin.name, builtin.signatures)
        );

        push_unique(
            rustscript,
            CompletionEntry {
                label: builtin.name.to_string(),
                insert_text: format!("{}({})", builtin.name, comma_args(&params)),
                detail: detail.clone(),
                documentation: builtin.docs.to_string(),
                kind: "function".to_string(),
            },
        );
        push_unique(
            javascript,
            CompletionEntry {
                label: builtin.name.to_string(),
                insert_text: format!("{}({})", builtin.name, comma_args(&params)),
                detail: detail.clone(),
                documentation: builtin.docs.to_string(),
                kind: "function".to_string(),
            },
        );
        push_unique(
            lua,
            CompletionEntry {
                label: builtin.name.to_string(),
                insert_text: format!("{}({})", builtin.name, comma_args(&params)),
                detail: detail.clone(),
                documentation: builtin.docs.to_string(),
                kind: "function".to_string(),
            },
        );
        push_unique(
            scheme,
            CompletionEntry {
                label: builtin.name.to_string(),
                insert_text: format!("({} {})", builtin.name, space_args(&params)),
                detail,
                documentation: builtin.docs.to_string(),
                kind: "function".to_string(),
            },
        );
    }
}

fn add_host_function_entries(
    rustscript: &mut Vec<CompletionEntry>,
    javascript: &mut Vec<CompletionEntry>,
    lua: &mut Vec<CompletionEntry>,
    scheme: &mut Vec<CompletionEntry>,
) {
    for host in default_host_callables() {
        let params = named_params(host.signature.params);
        let signature =
            typed_signature(host.name, host.signature.params, host.signature.return_type);

        match host.name {
            "print" => {
                push_unique(
                    rustscript,
                    CompletionEntry {
                        label: "print".to_string(),
                        insert_text: format!("print({});", comma_args(&params)),
                        detail: format!("playground host {signature}"),
                        documentation: host.docs.to_string(),
                        kind: "function".to_string(),
                    },
                );
                push_unique(
                    rustscript,
                    CompletionEntry {
                        label: "print (format)".to_string(),
                        insert_text: "print(\"{}\", ${1:value});".to_string(),
                        detail: "playground host print(format: string, value: any) -> any"
                            .to_string(),
                        documentation:
                            "Formats with Rust std::fmt-style placeholders, then writes without a trailing newline."
                                .to_string(),
                        kind: "function".to_string(),
                    },
                );
                push_unique(
                    javascript,
                    CompletionEntry {
                        label: "console.log".to_string(),
                        insert_text: format!("console.log({});", comma_args(&params)),
                        detail: "playground host console.log(value: any) -> any".to_string(),
                        documentation: host.docs.to_string(),
                        kind: "function".to_string(),
                    },
                );
                push_unique(
                    lua,
                    CompletionEntry {
                        label: "print".to_string(),
                        insert_text: format!("print({})", comma_args(&params)),
                        detail: format!(
                            "playground host {}",
                            typed_signature(
                                "print",
                                host.signature.params,
                                host.signature.return_type
                            )
                        ),
                        documentation: host.docs.to_string(),
                        kind: "function".to_string(),
                    },
                );
                push_unique(
                    scheme,
                    CompletionEntry {
                        label: "print".to_string(),
                        insert_text: format!("(print {})", space_args(&params)),
                        detail: format!(
                            "playground host {}",
                            typed_signature(
                                "print",
                                host.signature.params,
                                host.signature.return_type
                            )
                        ),
                        documentation: host.docs.to_string(),
                        kind: "function".to_string(),
                    },
                );
                push_unique(
                    scheme,
                    CompletionEntry {
                        label: "(declare (print ...))".to_string(),
                        insert_text: "(declare (print value))".to_string(),
                        detail: "declare print host binding".to_string(),
                        documentation: "Declares print host binding for Scheme flavor programs."
                            .to_string(),
                        kind: "module".to_string(),
                    },
                );
            }
            "println" => {
                push_unique(
                    rustscript,
                    CompletionEntry {
                        label: "println".to_string(),
                        insert_text: format!("println({});", comma_args(&params)),
                        detail: format!("playground host {signature}"),
                        documentation: host.docs.to_string(),
                        kind: "function".to_string(),
                    },
                );
            }
            other if other.contains("::") => {
                add_namespaced_host_function_entries(
                    rustscript,
                    javascript,
                    lua,
                    scheme,
                    other,
                    &params,
                    host.signature.params,
                    host.signature.return_type,
                    host.docs,
                );
            }
            other => {
                push_unique(
                    rustscript,
                    CompletionEntry {
                        label: other.to_string(),
                        insert_text: format!("{other}({})", comma_args(&params)),
                        detail: format!("playground host {signature}"),
                        documentation: host.docs.to_string(),
                        kind: "function".to_string(),
                    },
                );

                push_unique(
                    javascript,
                    CompletionEntry {
                        label: other.to_string(),
                        insert_text: format!("{other}({})", comma_args(&params)),
                        detail: format!("playground host {signature}"),
                        documentation: host.docs.to_string(),
                        kind: "function".to_string(),
                    },
                );

                push_unique(
                    lua,
                    CompletionEntry {
                        label: other.to_string(),
                        insert_text: format!("{other}({})", comma_args(&params)),
                        detail: format!("playground host {signature}"),
                        documentation: host.docs.to_string(),
                        kind: "function".to_string(),
                    },
                );

                push_unique(
                    scheme,
                    CompletionEntry {
                        label: other.to_string(),
                        insert_text: format!("({other} {})", space_args(&params)),
                        detail: format!("playground host {signature}"),
                        documentation: host.docs.to_string(),
                        kind: "function".to_string(),
                    },
                );
            }
        }
    }
}

fn add_edge_host_import_entries(
    rustscript: &mut Vec<CompletionEntry>,
    javascript: &mut Vec<CompletionEntry>,
    lua: &mut Vec<CompletionEntry>,
    scheme: &mut Vec<CompletionEntry>,
) {
    for namespace in host_namespace_specs() {
        let root = namespace.root;
        let docs = format!(
            "{} Imports host namespace `{root}` for pd-edge host calls.",
            namespace.docs
        );
        push_unique(
            rustscript,
            CompletionEntry {
                label: format!("use {root};"),
                insert_text: format!("use {root};"),
                detail: format!("RustScript {root} host namespace import"),
                documentation: docs.clone(),
                kind: "module".to_string(),
            },
        );
        push_unique(
            javascript,
            CompletionEntry {
                label: format!("import * as {root} from \"{root}\";"),
                insert_text: format!("import * as {root} from \"{root}\";"),
                detail: format!("JavaScript {root} host namespace import"),
                documentation: docs.clone(),
                kind: "module".to_string(),
            },
        );
        push_unique(
            lua,
            CompletionEntry {
                label: format!("local {root} = require(\"{root}\")"),
                insert_text: format!("local {root} = require(\"{root}\")"),
                detail: format!("Lua {root} host namespace import"),
                documentation: docs.clone(),
                kind: "module".to_string(),
            },
        );
        push_unique(
            scheme,
            CompletionEntry {
                label: format!("(require (prefix-in {root}. \"{root}\"))"),
                insert_text: format!("(require (prefix-in {root}. \"{root}\"))"),
                detail: format!("Scheme {root} host namespace import"),
                documentation: docs,
                kind: "module".to_string(),
            },
        );
    }
}

fn add_edge_host_function_entries(
    rustscript: &mut Vec<CompletionEntry>,
    javascript: &mut Vec<CompletionEntry>,
    lua: &mut Vec<CompletionEntry>,
    scheme: &mut Vec<CompletionEntry>,
) {
    for function in EDGE_HOST_FUNCTIONS {
        let params = abi_param_names(function.param_names);
        let signature = abi_signature(
            function.name,
            function.param_names,
            function.param_types,
            function.return_type,
        );
        let dot_path = function.name.replace("::", ".");
        let root = function.name.split("::").next().unwrap_or(function.name);
        let namespace_doc_prefix = host_namespace_specs()
            .iter()
            .find(|namespace| namespace.root == root)
            .map(|namespace| namespace.docs)
            .unwrap_or("pd-edge host namespace.");
        let docs = format!(
            "pd-edge host function from ABI index {} with arity {}.",
            function.index, function.arity
        );
        let namespace_docs = format!(
            "{namespace_doc_prefix} {docs} Namespace-export form (after importing `{root}`) is also available."
        );

        push_unique(
            rustscript,
            CompletionEntry {
                label: function.name.to_string(),
                insert_text: format!("{}({})", function.name, comma_args(&params)),
                detail: format!("pd-edge host {signature}"),
                documentation: namespace_docs.clone(),
                kind: "function".to_string(),
            },
        );
        push_unique(
            javascript,
            CompletionEntry {
                label: dot_path.clone(),
                insert_text: format!("{dot_path}({})", comma_args(&params)),
                detail: format!(
                    "pd-edge host {}",
                    abi_signature(
                        &dot_path,
                        function.param_names,
                        function.param_types,
                        function.return_type,
                    )
                ),
                documentation: namespace_docs.clone(),
                kind: "function".to_string(),
            },
        );
        push_unique(
            lua,
            CompletionEntry {
                label: dot_path.clone(),
                insert_text: format!("{dot_path}({})", comma_args(&params)),
                detail: format!(
                    "pd-edge host {}",
                    abi_signature(
                        &dot_path,
                        function.param_names,
                        function.param_types,
                        function.return_type,
                    )
                ),
                documentation: namespace_docs.clone(),
                kind: "function".to_string(),
            },
        );
        push_unique(
            scheme,
            CompletionEntry {
                label: dot_path.clone(),
                insert_text: format!("({dot_path} {})", space_args(&params)),
                detail: format!(
                    "pd-edge host {}",
                    abi_signature(
                        &dot_path,
                        function.param_names,
                        function.param_types,
                        function.return_type,
                    )
                ),
                documentation: namespace_docs,
                kind: "function".to_string(),
            },
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn add_namespaced_host_function_entries(
    rustscript: &mut Vec<CompletionEntry>,
    javascript: &mut Vec<CompletionEntry>,
    lua: &mut Vec<CompletionEntry>,
    scheme: &mut Vec<CompletionEntry>,
    name: &str,
    params: &[String],
    signature_params: &[CallableParam],
    return_type: &str,
    docs: &str,
) {
    let Some((root, _member)) = name.split_once("::") else {
        return;
    };
    let dot_path = name.replace("::", ".");
    let import_docs = format!("Imports virtual host namespace `{root}` for playground host calls.");

    push_unique(
        rustscript,
        CompletionEntry {
            label: name.to_string(),
            insert_text: format!("{name}({})", comma_args(params)),
            detail: format!(
                "playground host {}",
                typed_signature(name, signature_params, return_type)
            ),
            documentation: docs.to_string(),
            kind: "function".to_string(),
        },
    );
    push_unique(
        rustscript,
        CompletionEntry {
            label: format!("use {root};"),
            insert_text: format!("use {root};"),
            detail: format!("RustScript {root} host namespace import"),
            documentation: import_docs.clone(),
            kind: "module".to_string(),
        },
    );

    push_unique(
        javascript,
        CompletionEntry {
            label: dot_path.clone(),
            insert_text: format!("{dot_path}({})", comma_args(params)),
            detail: format!(
                "playground host {}",
                typed_signature(&dot_path, signature_params, return_type)
            ),
            documentation: docs.to_string(),
            kind: "function".to_string(),
        },
    );
    push_unique(
        javascript,
        CompletionEntry {
            label: format!("import * as {root} from \"{root}\";"),
            insert_text: format!("import * as {root} from \"{root}\";"),
            detail: format!("JavaScript {root} host namespace import"),
            documentation: import_docs.clone(),
            kind: "module".to_string(),
        },
    );

    push_unique(
        lua,
        CompletionEntry {
            label: dot_path.clone(),
            insert_text: format!("{dot_path}({})", comma_args(params)),
            detail: format!(
                "playground host {}",
                typed_signature(&dot_path, signature_params, return_type)
            ),
            documentation: docs.to_string(),
            kind: "function".to_string(),
        },
    );
    push_unique(
        lua,
        CompletionEntry {
            label: format!("local {root} = require(\"{root}\")"),
            insert_text: format!("local {root} = require(\"{root}\")"),
            detail: format!("Lua {root} host namespace import"),
            documentation: import_docs.clone(),
            kind: "module".to_string(),
        },
    );

    push_unique(
        scheme,
        CompletionEntry {
            label: dot_path.clone(),
            insert_text: format!("({dot_path} {})", space_args(params)),
            detail: format!(
                "playground host {}",
                typed_signature(&dot_path, signature_params, return_type)
            ),
            documentation: docs.to_string(),
            kind: "function".to_string(),
        },
    );
    push_unique(
        scheme,
        CompletionEntry {
            label: format!("(require (prefix-in {root}. \"{root}\"))"),
            insert_text: format!("(require (prefix-in {root}. \"{root}\"))"),
            detail: format!("Scheme {root} host namespace import"),
            documentation: import_docs,
            kind: "module".to_string(),
        },
    );
}

fn add_builtin_namespace_entries(
    rustscript: &mut Vec<CompletionEntry>,
    javascript: &mut Vec<CompletionEntry>,
    lua: &mut Vec<CompletionEntry>,
    scheme: &mut Vec<CompletionEntry>,
) {
    for namespace in builtin_namespace_specs() {
        let support_note = if namespace.runtime_supported_on_wasm {
            "Supported in wasm playground runtime."
        } else {
            "Listed for API discovery, but unsupported at runtime on wasm playground."
        };

        push_unique(
            rustscript,
            CompletionEntry {
                label: format!("use {};", namespace.namespace),
                insert_text: format!("use {};", namespace.namespace),
                detail: format!("RustScript {} import", namespace.namespace),
                documentation: format!("{} {}", namespace.docs, support_note),
                kind: "module".to_string(),
            },
        );
        push_unique(
            javascript,
            CompletionEntry {
                label: format!(
                    "import * as {} from \"{}\";",
                    namespace.namespace, namespace.namespace
                ),
                insert_text: format!(
                    "import * as {} from \"{}\";",
                    namespace.namespace, namespace.namespace
                ),
                detail: format!("JavaScript {} import", namespace.namespace),
                documentation: format!("{} {}", namespace.docs, support_note),
                kind: "module".to_string(),
            },
        );
        push_unique(
            lua,
            CompletionEntry {
                label: format!(
                    "local {} = require(\"{}\")",
                    namespace.namespace, namespace.namespace
                ),
                insert_text: format!(
                    "local {} = require(\"{}\")",
                    namespace.namespace, namespace.namespace
                ),
                detail: format!("Lua {} import", namespace.namespace),
                documentation: format!("{} {}", namespace.docs, support_note),
                kind: "module".to_string(),
            },
        );
        push_unique(
            scheme,
            CompletionEntry {
                label: format!(
                    "(require (prefix-in {}. \"{}\"))",
                    namespace.namespace, namespace.namespace
                ),
                insert_text: format!(
                    "(require (prefix-in {}. \"{}\"))",
                    namespace.namespace, namespace.namespace
                ),
                detail: format!("Scheme {} import", namespace.namespace),
                documentation: format!("{} {}", namespace.docs, support_note),
                kind: "module".to_string(),
            },
        );

        for member in namespace.members {
            let suffix = if namespace.runtime_supported_on_wasm {
                "Supported in wasm playground runtime."
            } else {
                "Unsupported at runtime on wasm playground."
            };
            let docs = format!("{} {}", member.docs, suffix);
            let signatures = callable_signatures_for_builtin_namespace_member(
                namespace.namespace,
                member.name,
                member.arity,
            );
            let params = signatures
                .map(signature_params)
                .unwrap_or_else(|| numbered_params(member.arity));

            let rust_label = format!("{}::{}", namespace.namespace, member.name);
            let dot_label = format!("{}.{}", namespace.namespace, member.name);
            let rust_detail = signatures
                .map(|items| format!("builtin {}", overload_signatures(&rust_label, items)))
                .unwrap_or_else(|| format!("builtin {}", function_signature(&rust_label, &params)));
            let dot_detail = signatures
                .map(|items| format!("builtin {}", overload_signatures(&dot_label, items)))
                .unwrap_or_else(|| format!("builtin {}", function_signature(&dot_label, &params)));

            push_unique(
                rustscript,
                CompletionEntry {
                    label: rust_label.clone(),
                    insert_text: format!("{rust_label}({})", comma_args(&params)),
                    detail: rust_detail.clone(),
                    documentation: docs.clone(),
                    kind: "function".to_string(),
                },
            );
            push_unique(
                javascript,
                CompletionEntry {
                    label: dot_label.clone(),
                    insert_text: format!("{dot_label}({})", comma_args(&params)),
                    detail: dot_detail.clone(),
                    documentation: docs.clone(),
                    kind: "function".to_string(),
                },
            );
            push_unique(
                lua,
                CompletionEntry {
                    label: dot_label.clone(),
                    insert_text: format!("{dot_label}({})", comma_args(&params)),
                    detail: dot_detail.clone(),
                    documentation: docs.clone(),
                    kind: "function".to_string(),
                },
            );
            push_unique(
                scheme,
                CompletionEntry {
                    label: dot_label.clone(),
                    insert_text: format!("({dot_label} {})", space_args(&params)),
                    detail: dot_detail,
                    documentation: docs,
                    kind: "function".to_string(),
                },
            );
        }
    }
}

fn add_stdlib_entries(
    rustscript: &mut Vec<CompletionEntry>,
    javascript: &mut Vec<CompletionEntry>,
    lua: &mut Vec<CompletionEntry>,
    scheme: &mut Vec<CompletionEntry>,
) {
    for (spec, source) in embedded_stdlib_modules() {
        let Some(module_name) = module_name_from_spec(spec) else {
            continue;
        };
        let alias = module_alias(module_name);
        let functions = parse_pub_functions(source);
        if functions.is_empty() {
            continue;
        }

        push_unique(
            rustscript,
            CompletionEntry {
                label: format!("use stdlib::rss::{module_name} as {alias};"),
                insert_text: format!("use stdlib::rss::{module_name} as {alias};"),
                detail: "RustScript stdlib import".to_string(),
                documentation: format!("Imports embedded module `{spec}`."),
                kind: "module".to_string(),
            },
        );
        push_unique(
            javascript,
            CompletionEntry {
                label: format!("import * as {alias} from \"../stdlib/rss/{module_name}.rss\";"),
                insert_text: format!(
                    "import * as {alias} from \"../stdlib/rss/{module_name}.rss\";"
                ),
                detail: "JavaScript stdlib import".to_string(),
                documentation: format!("Imports embedded module `{spec}`."),
                kind: "module".to_string(),
            },
        );
        push_unique(
            lua,
            CompletionEntry {
                label: format!("local {alias} = require(\"../stdlib/rss/{module_name}.rss\")"),
                insert_text: format!(
                    "local {alias} = require(\"../stdlib/rss/{module_name}.rss\")"
                ),
                detail: "Lua stdlib import".to_string(),
                documentation: format!("Imports embedded module `{spec}`."),
                kind: "module".to_string(),
            },
        );
        push_unique(
            scheme,
            CompletionEntry {
                label: format!("(import (prefix \"../stdlib/rss/{module_name}.rss\" {alias}:))"),
                insert_text: format!(
                    "(import (prefix \"../stdlib/rss/{module_name}.rss\" {alias}:))"
                ),
                detail: "Scheme stdlib import".to_string(),
                documentation: format!("Imports embedded module `{spec}`."),
                kind: "module".to_string(),
            },
        );

        for function in functions {
            let signature = function_signature(&function.name, &function.params);
            let docs = format!("Embedded stdlib function from `{spec}`.");

            push_unique(
                rustscript,
                CompletionEntry {
                    label: format!("{alias}::{}", function.name),
                    insert_text: format!(
                        "{alias}::{}({})",
                        function.name,
                        comma_args(&function.params)
                    ),
                    detail: format!("stdlib::rss::{module_name}::{signature}"),
                    documentation: docs.clone(),
                    kind: "function".to_string(),
                },
            );
            push_unique(
                javascript,
                CompletionEntry {
                    label: format!("{alias}.{}", function.name),
                    insert_text: format!(
                        "{alias}.{}({})",
                        function.name,
                        comma_args(&function.params)
                    ),
                    detail: format!("{module_name}.{signature}"),
                    documentation: docs.clone(),
                    kind: "function".to_string(),
                },
            );
            push_unique(
                lua,
                CompletionEntry {
                    label: format!("{alias}.{}", function.name),
                    insert_text: format!(
                        "{alias}.{}({})",
                        function.name,
                        comma_args(&function.params)
                    ),
                    detail: format!("{module_name}.{signature}"),
                    documentation: docs.clone(),
                    kind: "function".to_string(),
                },
            );
            push_unique(
                scheme,
                CompletionEntry {
                    label: format!("{alias}:{}", function.name),
                    insert_text: format!(
                        "({alias}:{} {})",
                        function.name,
                        space_args(&function.params)
                    ),
                    detail: format!("{module_name}.{signature}"),
                    documentation: docs,
                    kind: "function".to_string(),
                },
            );
        }
    }
}

fn numbered_params(arity: usize) -> Vec<String> {
    (1..=arity).map(|index| format!("arg{index}")).collect()
}

fn named_params(params: &[CallableParam]) -> Vec<String> {
    params
        .iter()
        .take_while(|param| !param.optional)
        .map(|param| param.name.to_string())
        .collect()
}

fn signature_params(signatures: &[CallableSignature]) -> Vec<String> {
    signatures
        .first()
        .map(|signature| named_params(signature.params))
        .unwrap_or_default()
}

fn abi_param_names(params: &[&str]) -> Vec<String> {
    params.iter().map(|param| (*param).to_string()).collect()
}

fn overload_signatures(name: &str, signatures: &[CallableSignature]) -> String {
    signatures
        .iter()
        .map(|signature| typed_signature(name, signature.params, signature.return_type))
        .collect::<Vec<_>>()
        .join(" | ")
}

fn typed_signature(name: &str, params: &[CallableParam], return_type: &str) -> String {
    format!("{name}({}) -> {return_type}", typed_params(params))
}

fn typed_params(params: &[CallableParam]) -> String {
    params
        .iter()
        .map(|param| {
            if param.optional {
                format!("{}?: {}", param.name, param.ty.label())
            } else {
                format!("{}: {}", param.name, param.ty.label())
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn abi_signature(
    name: &str,
    param_names: &[&str],
    param_types: &[AbiParamType],
    return_type: AbiValueType,
) -> String {
    let params = param_names
        .iter()
        .zip(param_types.iter().copied())
        .map(|(name, param)| format!("{name}: {}", abi_param_type_label(param)))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{name}({params}) -> {}", abi_value_type_label(return_type))
}

fn abi_param_type_label(value: AbiParamType) -> &'static str {
    match value {
        AbiParamType::Any => "any",
        AbiParamType::Null => "null",
        AbiParamType::Int => "int",
        AbiParamType::Float => "float",
        AbiParamType::Bool => "bool",
        AbiParamType::String => "string",
        AbiParamType::Array => "array",
        AbiParamType::Map => "map",
        AbiParamType::Number => "number",
    }
}

fn abi_value_type_label(value: AbiValueType) -> &'static str {
    match value {
        AbiValueType::Unknown => "unknown",
        AbiValueType::Null => "null",
        AbiValueType::Int => "int",
        AbiValueType::Float => "float",
        AbiValueType::Bool => "bool",
        AbiValueType::String => "string",
        AbiValueType::Array => "array",
        AbiValueType::Map => "map",
    }
}

fn function_signature(name: &str, params: &[String]) -> String {
    format!("{name}({})", params.join(", "))
}

fn comma_args(params: &[String]) -> String {
    snippet_params(params, ", ")
}

fn space_args(params: &[String]) -> String {
    snippet_params(params, " ")
}

fn snippet_params(params: &[String], separator: &str) -> String {
    params
        .iter()
        .enumerate()
        .map(|(index, value)| format!("${{{}:{}}}", index + 1, value))
        .collect::<Vec<_>>()
        .join(separator)
}

fn module_name_from_spec(spec: &str) -> Option<&str> {
    let file = spec.rsplit('/').next()?;
    file.strip_suffix(".rss")
}

fn module_alias(module_name: &str) -> &str {
    match module_name {
        "strings" => "string",
        _ => module_name,
    }
}

fn parse_pub_functions(source: &str) -> Vec<ParsedFunction> {
    let mut out = Vec::new();
    for line in source.lines() {
        let trimmed = line.trim_start();
        let Some(rest) = trimmed.strip_prefix("pub fn ") else {
            continue;
        };
        let Some(open_paren) = rest.find('(') else {
            continue;
        };
        let Some(close_paren_offset) = rest[(open_paren + 1)..].find(')') else {
            continue;
        };
        let close_paren = open_paren + 1 + close_paren_offset;
        let name = rest[..open_paren].trim();
        if name.is_empty() {
            continue;
        }

        let params = rest[(open_paren + 1)..close_paren]
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        out.push(ParsedFunction {
            name: name.to_string(),
            params,
        });
    }
    out.sort_by(|lhs, rhs| lhs.name.cmp(&rhs.name));
    out
}

fn push_unique(entries: &mut Vec<CompletionEntry>, entry: CompletionEntry) {
    if entries
        .iter()
        .any(|existing| existing.label == entry.label && existing.insert_text == entry.insert_text)
    {
        return;
    }
    entries.push(entry);
}

#[cfg(test)]
mod tests {
    use super::{build_completion_catalog, parse_pub_functions, signature_params, typed_signature};
    use edge_abi::{FUNCTIONS as EDGE_HOST_FUNCTIONS, host_namespace_specs};
    use vm::{CallableParam, CallableParamType, CallableSignature};

    #[test]
    fn parse_pub_functions_extracts_public_signatures() {
        let source = r#"
            fn local_only() {}
            pub fn trim(value) {}
            pub fn replace(value, needle, replacement) {}
        "#;
        let functions = parse_pub_functions(source);
        assert_eq!(functions.len(), 2);
        assert_eq!(functions[0].name, "replace");
        assert_eq!(functions[0].params, vec!["value", "needle", "replacement"]);
        assert_eq!(functions[1].name, "trim");
        assert_eq!(functions[1].params, vec!["value"]);
    }

    #[test]
    fn optional_params_are_rendered_and_omitted_from_snippets() {
        static PARAMS: [CallableParam; 2] = [
            CallableParam {
                name: "pattern",
                ty: CallableParamType::String,
                optional: false,
            },
            CallableParam {
                name: "flags",
                ty: CallableParamType::String,
                optional: true,
            },
        ];
        static SIGNATURES: [CallableSignature; 1] = [CallableSignature {
            params: &PARAMS,
            return_type: "bool",
        }];

        assert_eq!(signature_params(&SIGNATURES), vec!["pattern".to_string()]);
        assert_eq!(
            typed_signature("demo", &PARAMS, "bool"),
            "demo(pattern: string, flags?: string) -> bool"
        );
    }

    #[test]
    fn completion_catalog_contains_stdlib_members() {
        let catalog = build_completion_catalog();
        assert!(
            catalog
                .rustscript
                .iter()
                .any(|entry| entry.label == "string::trim"),
            "expected RustScript stdlib completion for string::trim",
        );
        assert!(
            catalog
                .javascript
                .iter()
                .any(|entry| entry.label == "string.trim"),
            "expected JavaScript stdlib completion for string.trim",
        );
        assert!(
            catalog
                .rustscript
                .iter()
                .any(|entry| entry.label == "parse::try_parse_int_base"),
            "expected RustScript stdlib completion for parse::try_parse_int_base",
        );
        assert!(
            catalog
                .rustscript
                .iter()
                .any(|entry| entry.label == "set::union"),
            "expected RustScript stdlib completion for set::union",
        );
        assert!(
            catalog
                .rustscript
                .iter()
                .any(|entry| entry.label == "lrucache::put"),
            "expected RustScript stdlib completion for lrucache::put",
        );
        assert!(
            catalog
                .javascript
                .iter()
                .any(|entry| entry.label == "parse.try_parse_int_base"),
            "expected JavaScript stdlib completion for parse.try_parse_int_base",
        );
        assert!(
            catalog
                .javascript
                .iter()
                .any(|entry| entry.label == "set.union"),
            "expected JavaScript stdlib completion for set.union",
        );
        assert!(
            catalog
                .rustscript
                .iter()
                .any(|entry| entry.label == "math::sqrt"),
            "expected RustScript builtin completion for math::sqrt",
        );
    }

    #[test]
    fn completion_catalog_contains_host_functions() {
        let catalog = build_completion_catalog();
        assert!(
            catalog
                .rustscript
                .iter()
                .any(|entry| entry.label == "http::request::get_id"),
            "expected RustScript pd-edge host completion for http::request::get_id",
        );
        assert!(
            catalog
                .javascript
                .iter()
                .any(|entry| entry.label == "import * as http from \"http\";"),
            "expected JavaScript pd-edge host namespace import completion for http",
        );
        assert!(
            catalog
                .rustscript
                .iter()
                .any(|entry| entry.label == "runtime::sleep"),
            "expected RustScript host completion for runtime::sleep",
        );
        assert!(
            catalog
                .rustscript
                .iter()
                .any(|entry| entry.label == "println"),
            "expected RustScript completion for println",
        );
        assert!(
            catalog
                .rustscript
                .iter()
                .any(|entry| entry.label == "print (format)"),
            "expected RustScript completion for print formatting call",
        );
        assert!(
            catalog
                .scheme
                .iter()
                .any(|entry| entry.label == "runtime.sleep"),
            "expected Scheme host completion for runtime.sleep",
        );
        assert!(
            catalog
                .rustscript
                .iter()
                .any(|entry| entry.label == "re::match"),
            "expected RustScript completion for re::match",
        );
        assert!(
            catalog
                .rustscript
                .iter()
                .any(|entry| entry.label == "json::encode"),
            "expected RustScript completion for json::encode",
        );
        assert!(
            catalog
                .rustscript
                .iter()
                .any(|entry| entry.label == "use math;"),
            "expected RustScript completion for use math;",
        );
    }

    #[test]
    fn completion_catalog_tracks_every_edge_abi_entry() {
        let catalog = build_completion_catalog();

        for function in EDGE_HOST_FUNCTIONS {
            let dot_label = function.name.replace("::", ".");
            assert!(
                catalog
                    .rustscript
                    .iter()
                    .any(|entry| entry.label == function.name),
                "missing RustScript completion for edge ABI function {}",
                function.name,
            );
            assert!(
                catalog
                    .javascript
                    .iter()
                    .any(|entry| entry.label == dot_label),
                "missing JavaScript completion for edge ABI function {}",
                function.name,
            );
            assert!(
                catalog.lua.iter().any(|entry| entry.label == dot_label),
                "missing Lua completion for edge ABI function {}",
                function.name,
            );
            assert!(
                catalog.scheme.iter().any(|entry| entry.label == dot_label),
                "missing Scheme completion for edge ABI function {}",
                function.name,
            );
        }

        for namespace in host_namespace_specs() {
            let rust_import = format!("use {};", namespace.root);
            let js_import = format!(
                "import * as {} from \"{}\";",
                namespace.root, namespace.root
            );
            let lua_import = format!("local {} = require(\"{}\")", namespace.root, namespace.root);
            let scheme_import = format!(
                "(require (prefix-in {}. \"{}\"))",
                namespace.root, namespace.root
            );

            assert!(
                catalog
                    .rustscript
                    .iter()
                    .any(|entry| entry.label == rust_import),
                "missing RustScript namespace import completion for {}",
                namespace.root,
            );
            assert!(
                catalog
                    .javascript
                    .iter()
                    .any(|entry| entry.label == js_import),
                "missing JavaScript namespace import completion for {}",
                namespace.root,
            );
            assert!(
                catalog.lua.iter().any(|entry| entry.label == lua_import),
                "missing Lua namespace import completion for {}",
                namespace.root,
            );
            assert!(
                catalog
                    .scheme
                    .iter()
                    .any(|entry| entry.label == scheme_import),
                "missing Scheme namespace import completion for {}",
                namespace.root,
            );
        }
    }
}
