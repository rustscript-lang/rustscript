use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use super::super::linker::ParsedUnit;
use super::super::modules::ModuleGraph;
use super::super::source_map::SourceMap;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrontendImportSyntax {
    RustScript,
    JavaScript,
    Lua,
}

#[derive(Clone, Debug)]
pub struct NamedImport {
    pub imported: String,
    pub local: String,
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
    pub(super) module_graph: ModuleGraph,
    /// Compilation-wide source map keyed by the module graph's
    /// [`SourceId`](super::super::modules::SourceId) space. Every module's
    /// raw text is registered here at its graph source id so spans produced
    /// during load and merge resolve to the owning source for diagnostics.
    pub(super) sources: SourceMap,
}
