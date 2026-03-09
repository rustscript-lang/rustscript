macro_rules! declare_json_namespace {
    ($callback:ident) => {
        $callback! {
            module: json,
            namespace: "json",
            alias: "json",
            docs: "JSON builtin namespace.",
            runtime_supported_on_wasm: true,
            supports_regex_flags: false,
            members: [
                namespace_builtin!(
                    JsonEncode,
                    "encode",
                    1,
                    builtin_json_encode,
                    args_ref,
                    "Serialize value to JSON string."
                ),
                namespace_builtin!(
                    JsonDecode,
                    "decode",
                    1,
                    builtin_json_decode,
                    args_ref,
                    "Parse JSON string into VM value."
                ),
            ],
        }
    };
}
