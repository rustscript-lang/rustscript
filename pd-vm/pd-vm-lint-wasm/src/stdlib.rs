use vm::CompileSourceFileOptions;

const STDLIB_RSS_MODULES: &[(&str, &str)] = &[
    (
        "stdlib/rss/collections.rss",
        include_str!("../../stdlib/rss/collections.rss"),
    ),
    ("stdlib/rss/io.rss", include_str!("../../stdlib/rss/io.rss")),
    (
        "stdlib/rss/iter.rss",
        include_str!("../../stdlib/rss/iter.rss"),
    ),
    (
        "stdlib/rss/math.rss",
        include_str!("../../stdlib/rss/math.rss"),
    ),
    (
        "stdlib/rss/path.rss",
        include_str!("../../stdlib/rss/path.rss"),
    ),
    (
        "stdlib/rss/strings.rss",
        include_str!("../../stdlib/rss/strings.rss"),
    ),
];

pub(crate) fn embedded_stdlib_modules() -> &'static [(&'static str, &'static str)] {
    STDLIB_RSS_MODULES
}

pub(crate) fn embedded_stdlib_compile_options() -> CompileSourceFileOptions {
    let mut options = CompileSourceFileOptions::new();
    for (spec, source) in embedded_stdlib_modules() {
        options.set_module_override_source(*spec, *source);
    }
    options
}
