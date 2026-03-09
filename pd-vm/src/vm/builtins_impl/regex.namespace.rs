macro_rules! declare_regex_namespace {
    ($callback:ident) => {
        $callback! {
            module: regex,
            namespace: "re",
            alias: "re",
            docs: "Regex builtin namespace.",
            runtime_supported_on_wasm: true,
            supports_regex_flags: true,
            members: [
                namespace_alias!(
                    ReIsMatch,
                    "match",
                    3,
                    "Regex match with optional flags argument."
                ),
                namespace_builtin!(
                    ReIsMatch,
                    "is_match",
                    2,
                    builtin_re_is_match,
                    args_ref,
                    "Regex match without explicit flags."
                ),
                namespace_builtin!(
                    ReFind,
                    "find",
                    2,
                    builtin_re_find,
                    args_ref,
                    "Find first regex match."
                ),
                namespace_builtin!(
                    ReReplace,
                    "replace",
                    3,
                    builtin_re_replace,
                    args_ref,
                    "Replace regex matches."
                ),
                namespace_builtin!(
                    ReSplit,
                    "split",
                    2,
                    builtin_re_split,
                    args_ref,
                    "Split text by regex delimiter."
                ),
                namespace_builtin!(
                    ReCaptures,
                    "captures",
                    2,
                    builtin_re_captures,
                    args_ref,
                    "Return capture groups."
                ),
            ],
        }
    };
}
