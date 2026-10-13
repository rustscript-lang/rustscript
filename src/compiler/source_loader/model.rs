use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use super::super::linker::ParsedUnit;
use super::super::modules::ModuleGraph;
use super::super::source_map::{SourceMap, Span};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum DeclarationNamespace {
    #[default]
    Value,
    Type,
}

#[derive(Clone, Debug)]
pub struct NamedImport {
    pub imported: String,
    pub local: String,
    pub namespace: DeclarationNamespace,
}

impl NamedImport {
    pub fn value(imported: impl Into<String>, local: impl Into<String>) -> Self {
        Self {
            imported: imported.into(),
            local: local.into(),
            namespace: DeclarationNamespace::Value,
        }
    }

    pub fn type_only(imported: impl Into<String>, local: impl Into<String>) -> Self {
        Self {
            imported: imported.into(),
            local: local.into(),
            namespace: DeclarationNamespace::Type,
        }
    }
}

#[derive(Clone, Debug)]
pub enum ImportClause {
    AllPublic,
    Named(Vec<NamedImport>),
    Namespace(String),
    Prefix(String),
}

#[derive(Clone, Debug)]
pub struct ModuleImport {
    pub spec: String,
    pub clause: ImportClause,
    pub line: usize,
    pub namespace: DeclarationNamespace,
    pub span: Option<Span>,
}

impl ModuleImport {
    pub fn value(spec: impl Into<String>, clause: ImportClause, line: usize) -> Self {
        Self {
            spec: spec.into(),
            clause,
            line,
            namespace: DeclarationNamespace::Value,
            span: None,
        }
    }

    pub fn type_only(mut self) -> Self {
        self.namespace = DeclarationNamespace::Type;
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ExportedFunctionSignature {
    pub(super) arity: u8,
    pub(super) type_params: Vec<String>,
}

#[derive(Default)]
pub(super) struct ModuleCollectState {
    pub(super) visiting: Vec<PathBuf>,
    pub(super) seen: HashSet<PathBuf>,
    pub(super) units: Vec<ParsedUnit>,
    pub(super) module_exports: HashMap<PathBuf, HashMap<String, ExportedFunctionSignature>>,
    pub(super) module_type_exports:
        HashMap<PathBuf, HashMap<String, crate::compiler::FrontendTypeExport>>,
    pub(super) module_graph: ModuleGraph,
    /// Compilation-wide source map keyed by the module graph's
    /// [`SourceId`](super::super::modules::SourceId) space. Every module's
    /// raw text is registered here at its graph source id so spans produced
    /// during load and merge resolve to the owning source.
    pub(super) sources: SourceMap,
}
