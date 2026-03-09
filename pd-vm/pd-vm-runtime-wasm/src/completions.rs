use serde::Serialize;
use vm::builtin_namespace_specs;

use crate::runtime::host_function_specs;
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

    add_host_function_entries(&mut rustscript, &mut javascript, &mut lua, &mut scheme);
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

fn add_host_function_entries(
    rustscript: &mut Vec<CompletionEntry>,
    javascript: &mut Vec<CompletionEntry>,
    lua: &mut Vec<CompletionEntry>,
    scheme: &mut Vec<CompletionEntry>,
) {
    for host in host_function_specs() {
        let params = numbered_params(host.arity);
        let signature = function_signature(host.name, &params);

        match host.name {
            "print" => {
                push_unique(
                    rustscript,
                    CompletionEntry {
                        label: "print".to_string(),
                        insert_text: format!("print({});", comma_args(&params)),
                        detail: "RustScript output helper".to_string(),
                        documentation:
                            "Writes a value to playground print output. Supports Rust-style formatting when called as print(\"...\", ...)."
                                .to_string(),
                        kind: "function".to_string(),
                    },
                );
                push_unique(
                    rustscript,
                    CompletionEntry {
                        label: "print (format)".to_string(),
                        insert_text: "print(\"{}\", ${1:value});".to_string(),
                        detail: "RustScript formatting call".to_string(),
                        documentation:
                            "Formats with Rust std::fmt-style placeholders, then writes without a trailing newline."
                                .to_string(),
                        kind: "function".to_string(),
                    },
                );
                push_unique(
                    rustscript,
                    CompletionEntry {
                        label: "println".to_string(),
                        insert_text: format!("println({});", comma_args(&params)),
                        detail: "RustScript output helper with newline".to_string(),
                        documentation:
                            "Writes a value to playground print output and appends a newline. Supports Rust-style formatting when called as println(\"...\", ...)."
                                .to_string(),
                        kind: "function".to_string(),
                    },
                );
                push_unique(
                    javascript,
                    CompletionEntry {
                        label: "console.log".to_string(),
                        insert_text: format!("console.log({});", comma_args(&params)),
                        detail: "JavaScript output helper".to_string(),
                        documentation: "Writes a value to playground print output.".to_string(),
                        kind: "function".to_string(),
                    },
                );
                push_unique(
                    lua,
                    CompletionEntry {
                        label: "print".to_string(),
                        insert_text: format!("print({})", comma_args(&params)),
                        detail: "Lua output helper".to_string(),
                        documentation: "Writes a value to playground print output.".to_string(),
                        kind: "function".to_string(),
                    },
                );
                push_unique(
                    scheme,
                    CompletionEntry {
                        label: "print".to_string(),
                        insert_text: format!("(print {})", space_args(&params)),
                        detail: "Scheme output helper".to_string(),
                        documentation: "Writes a value to playground print output.".to_string(),
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
            other if other.contains("::") => {
                add_namespaced_host_function_entries(
                    rustscript, javascript, lua, scheme, other, &params, host.docs,
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

fn add_namespaced_host_function_entries(
    rustscript: &mut Vec<CompletionEntry>,
    javascript: &mut Vec<CompletionEntry>,
    lua: &mut Vec<CompletionEntry>,
    scheme: &mut Vec<CompletionEntry>,
    name: &str,
    params: &[String],
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
            detail: format!("playground host {}", function_signature(name, params)),
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
            detail: format!("playground host {}", function_signature(&dot_path, params)),
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
            detail: format!("playground host {}", function_signature(&dot_path, params)),
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
            detail: format!("playground host {}", function_signature(&dot_path, params)),
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
                    namespace.alias, namespace.namespace
                ),
                insert_text: format!(
                    "import * as {} from \"{}\";",
                    namespace.alias, namespace.namespace
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
                    namespace.alias, namespace.namespace
                ),
                insert_text: format!(
                    "local {} = require(\"{}\")",
                    namespace.alias, namespace.namespace
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
                    namespace.alias, namespace.namespace
                ),
                insert_text: format!(
                    "(require (prefix-in {}. \"{}\"))",
                    namespace.alias, namespace.namespace
                ),
                detail: format!("Scheme {} import", namespace.namespace),
                documentation: format!("{} {}", namespace.docs, support_note),
                kind: "module".to_string(),
            },
        );

        for member in namespace.members {
            let params = numbered_params(member.arity);
            let suffix = if namespace.runtime_supported_on_wasm {
                "Supported in wasm playground runtime."
            } else {
                "Unsupported at runtime on wasm playground."
            };
            let docs = format!("{} {}", member.docs, suffix);

            let rust_label = format!("{}::{}", namespace.alias, member.name);
            let dot_label = format!("{}.{}", namespace.alias, member.name);

            push_unique(
                rustscript,
                CompletionEntry {
                    label: rust_label.clone(),
                    insert_text: format!("{rust_label}({})", comma_args(&params)),
                    detail: format!("builtin {}", function_signature(&rust_label, &params)),
                    documentation: docs.clone(),
                    kind: "function".to_string(),
                },
            );
            push_unique(
                javascript,
                CompletionEntry {
                    label: dot_label.clone(),
                    insert_text: format!("{dot_label}({})", comma_args(&params)),
                    detail: format!("builtin {}", function_signature(&dot_label, &params)),
                    documentation: docs.clone(),
                    kind: "function".to_string(),
                },
            );
            push_unique(
                lua,
                CompletionEntry {
                    label: dot_label.clone(),
                    insert_text: format!("{dot_label}({})", comma_args(&params)),
                    detail: format!("builtin {}", function_signature(&dot_label, &params)),
                    documentation: docs.clone(),
                    kind: "function".to_string(),
                },
            );
            push_unique(
                scheme,
                CompletionEntry {
                    label: dot_label.clone(),
                    insert_text: format!("({dot_label} {})", space_args(&params)),
                    detail: format!("builtin {}", function_signature(&dot_label, &params)),
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
    use super::{build_completion_catalog, parse_pub_functions};

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
}
