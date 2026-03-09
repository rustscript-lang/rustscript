use edge_abi::{FUNCTIONS as EDGE_HOST_FUNCTIONS, host_namespace_specs};
use serde::Serialize;

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

    add_host_import_entries(&mut rustscript, &mut javascript, &mut lua, &mut scheme);
    add_host_function_entries(&mut rustscript, &mut javascript, &mut lua, &mut scheme);
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

fn add_host_import_entries(
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

fn add_host_function_entries(
    rustscript: &mut Vec<CompletionEntry>,
    javascript: &mut Vec<CompletionEntry>,
    lua: &mut Vec<CompletionEntry>,
    scheme: &mut Vec<CompletionEntry>,
) {
    for function in EDGE_HOST_FUNCTIONS {
        let params = numbered_params(usize::from(function.arity));
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
                detail: format!("pd-edge host {}", signature(function.name, &params)),
                documentation: namespace_docs.clone(),
                kind: "function".to_string(),
            },
        );
        push_unique(
            javascript,
            CompletionEntry {
                label: dot_path.clone(),
                insert_text: format!("{dot_path}({})", comma_args(&params)),
                detail: format!("pd-edge host {}", signature(&dot_path, &params)),
                documentation: namespace_docs.clone(),
                kind: "function".to_string(),
            },
        );
        push_unique(
            lua,
            CompletionEntry {
                label: dot_path.clone(),
                insert_text: format!("{dot_path}({})", comma_args(&params)),
                detail: format!("pd-edge host {}", signature(&dot_path, &params)),
                documentation: namespace_docs.clone(),
                kind: "function".to_string(),
            },
        );
        push_unique(
            scheme,
            CompletionEntry {
                label: dot_path.clone(),
                insert_text: format!("({dot_path} {})", space_args(&params)),
                detail: format!("pd-edge host {}", signature(&dot_path, &params)),
                documentation: namespace_docs,
                kind: "function".to_string(),
            },
        );
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
                documentation: format!("Imports embedded stdlib module `{spec}`."),
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
                documentation: format!("Imports embedded stdlib module `{spec}`."),
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
                documentation: format!("Imports embedded stdlib module `{spec}`."),
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
                documentation: format!("Imports embedded stdlib module `{spec}`."),
                kind: "module".to_string(),
            },
        );

        for function in functions {
            let docs = format!("Embedded stdlib function from `{spec}`.");
            let rust_path = format!("{alias}::{}", function.name);
            let dot_path = format!("{alias}.{}", function.name);
            let scheme_path = format!("{alias}:{}", function.name);

            push_unique(
                rustscript,
                CompletionEntry {
                    label: rust_path.clone(),
                    insert_text: format!("{rust_path}({})", comma_args(&function.params)),
                    detail: format!("stdlib {}", signature(&rust_path, &function.params)),
                    documentation: docs.clone(),
                    kind: "function".to_string(),
                },
            );
            push_unique(
                javascript,
                CompletionEntry {
                    label: dot_path.clone(),
                    insert_text: format!("{dot_path}({})", comma_args(&function.params)),
                    detail: format!("stdlib {}", signature(&dot_path, &function.params)),
                    documentation: docs.clone(),
                    kind: "function".to_string(),
                },
            );
            push_unique(
                lua,
                CompletionEntry {
                    label: dot_path.clone(),
                    insert_text: format!("{dot_path}({})", comma_args(&function.params)),
                    detail: format!("stdlib {}", signature(&dot_path, &function.params)),
                    documentation: docs.clone(),
                    kind: "function".to_string(),
                },
            );
            push_unique(
                scheme,
                CompletionEntry {
                    label: scheme_path.clone(),
                    insert_text: format!("({scheme_path} {})", space_args(&function.params)),
                    detail: format!("stdlib {}", signature(&scheme_path, &function.params)),
                    documentation: docs,
                    kind: "function".to_string(),
                },
            );
        }
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
        let Some(close_offset) = rest[(open_paren + 1)..].find(')') else {
            continue;
        };
        let close_paren = open_paren + 1 + close_offset;
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

fn numbered_params(arity: usize) -> Vec<String> {
    (1..=arity).map(|index| format!("arg{index}")).collect()
}

fn signature(name: &str, params: &[String]) -> String {
    format!("{name}({})", params.join(", "))
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
    fn completion_catalog_contains_edge_host_entries() {
        let catalog = build_completion_catalog();
        assert!(
            catalog
                .rustscript
                .iter()
                .any(|entry| entry.label == "http::request::get_id"),
            "expected RustScript completion for http::request::get_id",
        );
        assert!(
            catalog
                .rustscript
                .iter()
                .any(|entry| entry.label == "use http;"),
            "expected RustScript host namespace import completion for use http;",
        );
        assert!(
            catalog
                .rustscript
                .iter()
                .any(|entry| entry.label == "http::request::get_id"),
            "expected RustScript completion for namespace-exported http::request::get_id",
        );
        assert!(
            catalog
                .javascript
                .iter()
                .any(|entry| entry.label == "import * as http from \"http\";"),
            "expected JavaScript host namespace import completion for http",
        );
        assert!(
            catalog
                .javascript
                .iter()
                .any(|entry| entry.label == "http.request.get_id"),
            "expected JavaScript completion for namespace-exported http.request.get_id",
        );
    }

    #[test]
    fn completion_catalog_contains_stdlib_entries() {
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
    }
}
