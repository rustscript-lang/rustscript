use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use super::super::linker::ParsedUnit;

#[derive(Clone, Debug)]
pub(super) struct NamedImport {
    pub(super) imported: String,
    pub(super) local: String,
}

#[derive(Clone, Debug)]
pub(super) enum ImportClause {
    AllPublic,
    Named(Vec<NamedImport>),
    Namespace(String),
    Prefix(String),
}

#[derive(Clone, Debug)]
pub(super) struct ModuleImport {
    pub(super) spec: String,
    pub(super) clause: ImportClause,
    pub(super) line: usize,
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
}
pub(super) struct ImportRewriteResult {
    pub(super) source: String,
}
