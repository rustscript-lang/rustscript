use super::{Vm, VmError};

pub fn render_vm_error(vm: &Vm, err: &VmError) -> String {
    let mut out = format!("runtime error: {err}");
    let ip = vm.ip();
    if let Some(debug) = vm.debug_info()
        && let Some(line) = debug.line_for_offset(ip)
    {
        out.push_str(&format!("\nat ip {ip} (line {line})"));
        if let Some(source) = debug.source.as_ref()
            && let Some(line_text) = source
                .lines()
                .nth(line.saturating_sub(1) as usize)
                .map(str::to_string)
        {
            out.push_str(&format!("\n{line:>3} | {line_text}\n  | ^"));
        }
    } else {
        out.push_str(&format!("\nat ip {ip}"));
    }
    out
}
