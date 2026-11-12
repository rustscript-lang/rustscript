//! Shared test plugin for fixtures that still exercise the pre-Option nullable
//! VM contract. This is not a product language mode: RustScript itself
//! continues to reject legacy null.

use std::sync::Arc;

use vm::{
    CompileSourceFileOptions, FrontendId, FrontendIr, FrontendTypingPolicy, ParseError,
    ParserDialect, SharedParserOptions, SourceFlavor, SourceFrontendContext,
    SourceFrontendDescriptor, SourcePlugin, parse_source_with_dialect_and_catalog,
};

struct LegacyNullableDialect;

impl ParserDialect for LegacyNullableDialect {
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

static LEGACY_NULLABLE_DIALECT: LegacyNullableDialect = LegacyNullableDialect;
static LEGACY_NULLABLE_PLUGIN: LegacyNullablePlugin = LegacyNullablePlugin;

struct LegacyNullablePlugin;

impl SourcePlugin for LegacyNullablePlugin {
    fn descriptor(&self) -> SourceFrontendDescriptor {
        SourceFrontendDescriptor {
            id: FrontendId::new("legacy-nullable-fixture"),
            extensions: &["rss"],
            typing_policy: FrontendTypingPolicy::Strict,
            supports_ownership_checking: true,
        }
    }

    fn parser_dialect(&self) -> Option<&'static dyn ParserDialect> {
        Some(&LEGACY_NULLABLE_DIALECT)
    }

    fn parse_source(
        &self,
        source: &str,
        context: &SourceFrontendContext<'_>,
    ) -> Result<FrontendIr, ParseError> {
        parse_source_with_dialect_and_catalog(
            source,
            &LEGACY_NULLABLE_DIALECT,
            SharedParserOptions {
                source_id: context.source_id,
                allow_implicit_externs: true,
                allow_implicit_semicolons: false,
                enforce_mutable_bindings: true,
                import_scan_mode: false,
            },
            context
                .host_api_catalog
                .map(|catalog| Arc::new(catalog.clone())),
        )
    }
}

pub(crate) fn legacy_nullable_flavor() -> SourceFlavor {
    SourceFlavor::Plugin(LEGACY_NULLABLE_PLUGIN.descriptor().id)
}

pub(crate) fn legacy_nullable_options(
    mut options: CompileSourceFileOptions,
) -> CompileSourceFileOptions {
    options.add_source_plugin(&LEGACY_NULLABLE_PLUGIN);
    options
}
