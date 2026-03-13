/// Reads one line from process stdin and returns it as a string.
#[pd_host_function(name = "console::stdin::read_line")]
fn console_stdin_read_line() -> String {
    unreachable!("abi declaration only")
}

/// Reads all remaining bytes from process stdin and returns them as a string.
#[pd_host_function(name = "console::stdin::read_all")]
fn console_stdin_read_all() -> String {
    unreachable!("abi declaration only")
}

/// Writes text to process stdout and returns the number of bytes written.
#[pd_host_function(name = "console::stdout::write")]
fn console_stdout_write(text: &str) -> i64 {
    unreachable!("abi declaration only")
}

/// Flushes process stdout and reports whether the flush succeeded.
#[pd_host_function(name = "console::stdout::flush")]
fn console_stdout_flush() -> bool {
    unreachable!("abi declaration only")
}

/// Writes text to process stderr and returns the number of bytes written.
#[pd_host_function(name = "console::stderr::write")]
fn console_stderr_write(text: &str) -> i64 {
    unreachable!("abi declaration only")
}

/// Flushes process stderr and reports whether the flush succeeded.
#[pd_host_function(name = "console::stderr::flush")]
fn console_stderr_flush() -> bool {
    unreachable!("abi declaration only")
}

/// Returns the number of arguments passed to the loaded console program.
#[pd_host_function(name = "console::args::count")]
fn console_args_count() -> i64 {
    unreachable!("abi declaration only")
}

/// Returns the argument at the requested zero-based index, or an empty string when it is missing.
#[pd_host_function(name = "console::args::get")]
fn console_args_get(index: i64) -> String {
    unreachable!("abi declaration only")
}
