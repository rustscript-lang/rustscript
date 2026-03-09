macro_rules! declare_io_namespace {
    ($callback:ident) => {
        $callback! {
            module: io,
            namespace: "io",
            alias: "io",
            docs: "I/O builtin namespace.",
            runtime_supported_on_wasm: false,
            supports_regex_flags: false,
            members: [
                namespace_builtin!(
                    IoOpen,
                    "open",
                    2,
                    builtin_io_open,
                    vm_args_owned,
                    "Open file handle."
                ),
                namespace_builtin!(
                    IoPopen,
                    "popen",
                    2,
                    builtin_io_popen,
                    vm_args_owned,
                    "Open process handle."
                ),
                namespace_builtin!(
                    IoReadAll,
                    "read_all",
                    1,
                    builtin_io_read_all,
                    vm_args_owned,
                    "Read full handle contents."
                ),
                namespace_builtin!(
                    IoReadLine,
                    "read_line",
                    1,
                    builtin_io_read_line,
                    vm_args_owned,
                    "Read single line from handle."
                ),
                namespace_builtin!(
                    IoWrite,
                    "write",
                    2,
                    builtin_io_write,
                    vm_args_owned,
                    "Write text to handle."
                ),
                namespace_builtin!(
                    IoFlush,
                    "flush",
                    1,
                    builtin_io_flush,
                    vm_args_owned,
                    "Flush handle buffers."
                ),
                namespace_builtin!(
                    IoClose,
                    "close",
                    1,
                    builtin_io_close,
                    vm_args_owned,
                    "Close handle."
                ),
                namespace_builtin!(
                    IoExists,
                    "exists",
                    1,
                    builtin_io_exists,
                    vm_args_owned,
                    "Check file existence."
                ),
            ],
        }
    };
}
