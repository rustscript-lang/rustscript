//! Test-only parser dialect for fixtures that still exercise the pre-Option
//! nullable VM contract. This is not a product language mode: RustScript
//! itself continues to reject legacy null.

use std::path::Path;
use std::sync::Arc;

use crate::compiler::ir::FrontendIr;
use crate::compiler::parser::ParserDialect;
use crate::compiler::source_loader::test_module_imports_from_uses;
use crate::compiler::{
    CompileSourceFileOptions, FrontendId, FrontendTypingPolicy, ModuleImport, ParseError,
    SharedParserOptions, SourceError, SourceFlavor, SourceFrontendContext,
    SourceFrontendDescriptor, SourcePathError, SourcePlugin,
};

struct LegacyRssDialect;

impl ParserDialect for LegacyRssDialect {
    fn is_import_keyword(&self, ident: &str) -> bool {
        ident == "import"
    }

    fn allow_let_mut_binding(&self) -> bool {
        true
    }

    fn allow_macro_calls(&self) -> bool {
        true
    }

    fn allow_plus_equal_operator(&self) -> bool {
        true
    }

    fn allow_for_in_loop(&self) -> bool {
        true
    }

    fn allow_result_syntax(&self) -> bool {
        true
    }

    fn allow_option_syntax(&self) -> bool {
        true
    }

    fn allow_result_try(&self) -> bool {
        true
    }

    fn reject_legacy_null(&self) -> bool {
        false
    }
}

static LEGACY_RSS_DIALECT: LegacyRssDialect = LegacyRssDialect;
static LEGACY_RSS_PLUGIN: LegacyRssPlugin = LegacyRssPlugin;

struct LegacyRssPlugin;

impl SourcePlugin for LegacyRssPlugin {
    fn descriptor(&self) -> SourceFrontendDescriptor {
        SourceFrontendDescriptor {
            id: FrontendId::new("legacy-rss-test"),
            extensions: &["rss"],
            typing_policy: FrontendTypingPolicy::Strict,
            supports_ownership_checking: true,
        }
    }

    fn parser_dialect(&self) -> Option<&'static dyn ParserDialect> {
        Some(dialect())
    }

    fn parse_source(
        &self,
        source: &str,
        context: &SourceFrontendContext<'_>,
    ) -> Result<FrontendIr, ParseError> {
        parse_with_catalog(source, context, false)
    }

    fn parse_module_imports(
        &self,
        source: &str,
        path: &Path,
        context: &SourceFrontendContext<'_>,
    ) -> Result<Vec<ModuleImport>, SourcePathError> {
        let ir = parse_with_catalog(source, context, true)
            .map_err(|error| SourcePathError::Source(SourceError::Parse(error)))?;
        test_module_imports_from_uses(path, &ir.use_declarations)
    }
}

fn parse_with_catalog(
    source: &str,
    context: &SourceFrontendContext<'_>,
    import_scan_mode: bool,
) -> Result<FrontendIr, ParseError> {
    let catalog = context
        .host_api_catalog
        .map(|catalog| Arc::new(catalog.clone()));
    // Module compilation parses with implicit externs so imported names and
    // catalog host calls survive until the loader or catalog binder remaps them.
    super::parse_with_parser(
        source,
        context.source_id,
        true,
        false,
        true,
        import_scan_mode,
        dialect(),
        catalog,
    )
}

pub(crate) fn dialect() -> &'static dyn ParserDialect {
    &LEGACY_RSS_DIALECT
}

pub(crate) fn plugin() -> &'static dyn SourcePlugin {
    &LEGACY_RSS_PLUGIN
}

pub(crate) fn flavor() -> SourceFlavor {
    SourceFlavor::Plugin(plugin().descriptor().id)
}

pub(crate) fn parse(source: &str) -> Result<FrontendIr, ParseError> {
    super::parse_source_with_dialect(
        source,
        dialect(),
        SharedParserOptions {
            source_id: 0,
            allow_implicit_externs: false,
            allow_implicit_semicolons: false,
            enforce_mutable_bindings: true,
            import_scan_mode: false,
        },
    )
}

pub(crate) fn options_with(mut options: CompileSourceFileOptions) -> CompileSourceFileOptions {
    options.add_source_plugin(plugin());
    options
}
