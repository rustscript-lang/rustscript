use serde::Serialize;
use vm::{builtin_namespace_specs, default_host_callables, language_builtin_specs};

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
}

pub fn build_completion_catalog() -> CompletionCatalog {
    let mut rustscript = Vec::new();
    let mut javascript = Vec::new();
    let mut lua = Vec::new();

    for builtin in language_builtin_specs() {
        let entry = CompletionEntry {
            label: builtin.name.to_string(),
            insert_text: format!("{}()", builtin.name),
            detail: "builtin".to_string(),
            documentation: builtin.docs.to_string(),
            kind: "Function".to_string(),
        };
        push_unique(&mut rustscript, entry.clone());
        push_unique(&mut javascript, entry.clone());
        push_unique(&mut lua, entry);
    }

    for callable in default_host_callables() {
        add_host_completion(&mut rustscript, callable.name, callable.docs);
        add_host_completion(
            &mut javascript,
            &callable.name.replace("::", "."),
            callable.docs,
        );
        add_host_completion(&mut lua, &callable.name.replace("::", "."), callable.docs);
    }

    for namespace in builtin_namespace_specs() {
        for member in namespace.members {
            let rust_label = format!("{}::{}", namespace.namespace, member.name);
            let dot_label = format!("{}.{}", namespace.namespace, member.name);
            add_host_completion(&mut rustscript, &rust_label, member.docs);
            add_host_completion(&mut javascript, &dot_label, member.docs);
            add_host_completion(&mut lua, &dot_label, member.docs);
        }
    }

    rustscript.sort_by(|lhs, rhs| lhs.label.cmp(&rhs.label));
    javascript.sort_by(|lhs, rhs| lhs.label.cmp(&rhs.label));
    lua.sort_by(|lhs, rhs| lhs.label.cmp(&rhs.label));

    CompletionCatalog {
        rustscript,
        javascript,
        lua,
    }
}

fn add_host_completion(target: &mut Vec<CompletionEntry>, label: &str, docs: &str) {
    push_unique(
        target,
        CompletionEntry {
            label: label.to_string(),
            insert_text: format!("{label}()"),
            detail: "callable".to_string(),
            documentation: docs.to_string(),
            kind: "Function".to_string(),
        },
    );
}

fn push_unique(target: &mut Vec<CompletionEntry>, entry: CompletionEntry) {
    if !target.iter().any(|existing| existing.label == entry.label) {
        target.push(entry);
    }
}
