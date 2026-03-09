macro_rules! declare_jit_namespace {
    ($callback:ident) => {
        $callback! {
            module: jit,
            namespace: "jit",
            alias: "jit",
            docs: "JIT control builtin namespace.",
            runtime_supported_on_wasm: false,
            supports_regex_flags: false,
            members: [
                namespace_builtin!(
                    JitSetConfig,
                    "set_config",
                    3,
                    Map,
                    builtin_jit_set_config,
                    vm_args_owned,
                    "Replace JIT configuration."
                ),
                namespace_builtin!(
                    JitGetConfig,
                    "get_config",
                    0,
                    Map,
                    builtin_jit_get_config,
                    vm_noargs,
                    "Return current JIT configuration."
                ),
                namespace_builtin!(
                    JitSetEnabled,
                    "set_enabled",
                    1,
                    Bool,
                    builtin_jit_set_enabled,
                    vm_args_owned,
                    "Enable or disable JIT compilation."
                ),
                namespace_builtin!(
                    JitGetEnabled,
                    "get_enabled",
                    0,
                    Bool,
                    builtin_jit_get_enabled,
                    vm_noargs,
                    "Return whether JIT is enabled."
                ),
                namespace_builtin!(
                    JitSetHotLoopThreshold,
                    "set_hot_loop_threshold",
                    1,
                    Int,
                    builtin_jit_set_hot_loop_threshold,
                    vm_args_owned,
                    "Set hot loop threshold."
                ),
                namespace_builtin!(
                    JitGetHotLoopThreshold,
                    "get_hot_loop_threshold",
                    0,
                    Int,
                    builtin_jit_get_hot_loop_threshold,
                    vm_noargs,
                    "Return hot loop threshold."
                ),
                namespace_builtin!(
                    JitSetMaxTraceLen,
                    "set_max_trace_len",
                    1,
                    Int,
                    builtin_jit_set_max_trace_len,
                    vm_args_owned,
                    "Set maximum trace length."
                ),
                namespace_builtin!(
                    JitGetMaxTraceLen,
                    "get_max_trace_len",
                    0,
                    Int,
                    builtin_jit_get_max_trace_len,
                    vm_noargs,
                    "Return maximum trace length."
                ),
            ],
        }
    };
}
