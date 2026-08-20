//! Windows process-tree termination.
//!
//! On Windows, `CreateToolhelp32Snapshot` / `Process32FirstW` /
//! `Process32NextW` / `TerminateProcess` are used to enumerate and terminate
//! all descendant processes of a given parent. This is necessary because
//! Windows does not have Unix-style process groups, and `child.kill()` only
//! terminates the direct child, leaving grandchildren orphaned.
//!
//! This module is only compiled on `cfg(windows)`.

#![cfg(windows)]

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::Debug::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess};

/// Terminate the given process and all its descendants.
///
/// Uses CreateToolhelp32Snapshot to enumerate the process tree and
/// terminates every descendant before terminating the root.
pub(crate) fn terminate_process_tree(process_id: u32) {
    if process_id == 0 {
        return;
    }

    // Collect all descendants.
    let descendants = collect_descendants(process_id);
    // Terminate descendants first (leaf-first).
    for pid in descendants {
        terminate_process(pid);
    }
    // Terminate the root process.
    terminate_process(process_id);
}

fn collect_descendants(parent_pid: u32) -> Vec<u32> {
    let mut result = Vec::new();
    // SAFETY: Standard Windows snapshot API. The snapshot handle is
    // closed via CloseHandle on all paths.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return result;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        if Process32FirstW(snapshot, &mut entry) == 0 {
            CloseHandle(snapshot);
            return result;
        }
        // First pass: collect all pid/ppid pairs.
        let mut all_processes: Vec<(u32, u32)> = Vec::new();
        loop {
            all_processes.push((entry.th32ProcessID, entry.th32ParentProcessID));
            if Process32NextW(snapshot, &mut entry) == 0 {
                break;
            }
        }
        CloseHandle(snapshot);

        // Build a tree: collect all descendants recursively.
        let mut to_visit = vec![parent_pid];
        while let Some(pid) = to_visit.pop() {
            for &(child_pid, ppid) in &all_processes {
                if ppid == pid && child_pid != pid {
                    result.push(child_pid);
                    to_visit.push(child_pid);
                }
            }
        }
    }
    result
}

fn terminate_process(process_id: u32) {
    // SAFETY: Standard Windows process termination API.
    unsafe {
        let handle = OpenProcess(PROCESS_TERMINATE, 0, process_id);
        if handle.is_null() || handle == INVALID_HANDLE_VALUE as HANDLE {
            return;
        }
        let _ = TerminateProcess(handle, 1);
        let _ = CloseHandle(handle);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminate_process_tree_does_not_crash_with_zero_pid() {
        // Calling with pid 0 should be a no-op.
        terminate_process_tree(0);
    }

    #[test]
    fn terminate_process_tree_does_not_crash_with_invalid_pid() {
        // Calling with a non-existent pid should be safe (OpenProcess fails).
        terminate_process_tree(0xFFFFFFFF);
    }
}
