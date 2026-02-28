use serde::Serialize;
use vm::{LintReport, SourceFlavor, lint_source_with_flavor};

#[derive(Serialize)]
struct LintResponse {
    diagnostics: Vec<LintDiagnostic>,
}

#[derive(Serialize)]
struct LintDiagnostic {
    line: usize,
    message: String,
}

fn parse_flavor(raw: &str) -> SourceFlavor {
    match raw.trim().to_ascii_lowercase().as_str() {
        "javascript" | "js" => SourceFlavor::JavaScript,
        "lua" => SourceFlavor::Lua,
        "scheme" | "scm" => SourceFlavor::Scheme,
        _ => SourceFlavor::RustScript,
    }
}

fn pack_ptr_len(ptr: *mut u8, len: usize) -> u64 {
    ((len as u64) << 32) | (ptr as u64)
}

fn unpack_input<'a>(ptr: u32, len: u32) -> &'a [u8] {
    if ptr == 0 || len == 0 {
        return &[];
    }
    unsafe { std::slice::from_raw_parts(ptr as *const u8, len as usize) }
}

fn leak_bytes(bytes: Vec<u8>) -> u64 {
    let owned = bytes.into_boxed_slice();
    let len = owned.len();
    let ptr = Box::into_raw(owned) as *mut u8;
    pack_ptr_len(ptr, len)
}

fn report_to_json(report: LintReport) -> Vec<u8> {
    let response = LintResponse {
        diagnostics: report
            .diagnostics
            .into_iter()
            .map(|item| LintDiagnostic {
                line: item.line,
                message: item.message,
            })
            .collect(),
    };
    serde_json::to_vec(&response).unwrap_or_else(|_| b"{\"diagnostics\":[]}".to_vec())
}

#[unsafe(no_mangle)]
pub extern "C" fn wasm_alloc(len: u32) -> *mut u8 {
    let mut buf = Vec::<u8>::with_capacity(len as usize);
    let ptr = buf.as_mut_ptr();
    std::mem::forget(buf);
    ptr
}

#[unsafe(no_mangle)]
pub extern "C" fn wasm_dealloc(ptr: u32, len: u32) {
    if ptr == 0 || len == 0 {
        return;
    }
    unsafe {
        let _ = Vec::from_raw_parts(ptr as *mut u8, len as usize, len as usize);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn lint_source_json(
    source_ptr: u32,
    source_len: u32,
    flavor_ptr: u32,
    flavor_len: u32,
) -> u64 {
    let source_bytes = unpack_input(source_ptr, source_len);
    let source = match std::str::from_utf8(source_bytes) {
        Ok(value) => value,
        Err(err) => {
            let report = LintResponse {
                diagnostics: vec![LintDiagnostic {
                    line: 1,
                    message: format!("invalid utf-8 source: {err}"),
                }],
            };
            let fallback =
                serde_json::to_vec(&report).unwrap_or_else(|_| b"{\"diagnostics\":[]}".to_vec());
            return leak_bytes(fallback);
        }
    };

    let flavor_raw = std::str::from_utf8(unpack_input(flavor_ptr, flavor_len)).unwrap_or("rss");
    let flavor = parse_flavor(flavor_raw);
    let report = lint_source_with_flavor(source, flavor);
    leak_bytes(report_to_json(report))
}

#[cfg(test)]
mod tests {
    use super::parse_flavor;
    use vm::SourceFlavor;

    #[test]
    fn parse_flavor_accepts_aliases() {
        assert_eq!(parse_flavor("js"), SourceFlavor::JavaScript);
        assert_eq!(parse_flavor("scm"), SourceFlavor::Scheme);
        assert_eq!(parse_flavor("lua"), SourceFlavor::Lua);
        assert_eq!(parse_flavor("rss"), SourceFlavor::RustScript);
    }
}
