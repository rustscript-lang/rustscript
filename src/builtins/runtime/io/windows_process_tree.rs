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

use std::os::windows::io::AsRawHandle;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
    TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Threading::{
    OpenProcess, OpenThread, PROCESS_TERMINATE, ResumeThread, THREAD_SUSPEND_RESUME,
    TerminateProcess,
};

/// A Windows Job Object configured to terminate its entire process tree when
/// the owner closes it.
pub(crate) struct ProcessJob {
    handle: HANDLE,
}

// SAFETY: ProcessJob exclusively owns a Job Object HANDLE. A HANDLE is an
// opaque kernel identifier, not a pointer to Rust memory. All operations
// (AssignProcessToJobObject at construction, TerminateJobObject, CloseHandle)
// act on that owned identifier. CloseHandle runs once in Drop. Concurrent
// TerminateJobObject is permitted by the Windows kernel for job objects; the
// owner never aliases the handle as a typed Rust reference.
unsafe impl Send for ProcessJob {}
unsafe impl Sync for ProcessJob {}

impl ProcessJob {
    pub(crate) fn attach(child: &std::process::Child) -> std::io::Result<Self> {
        // SAFETY: The null security attributes and name request an unnamed
        // private job object owned by this process.
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error());
        }

        // SAFETY: The structure is initialized to the documented zero state;
        // only the KILL_ON_JOB_CLOSE flag is enabled below.
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let set_ok = unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if set_ok == 0 {
            // SAFETY: `handle` was returned by CreateJobObjectW and has not
            // been transferred elsewhere.
            unsafe { CloseHandle(handle) };
            return Err(std::io::Error::last_os_error());
        }

        let process = child.as_raw_handle() as HANDLE;
        // SAFETY: Both handles are live handles owned by this process. The job
        // remains owned by ProcessJob after successful assignment.
        if unsafe { AssignProcessToJobObject(handle, process) } == 0 {
            // SAFETY: same handle ownership as above.
            unsafe { CloseHandle(handle) };
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self { handle })
    }

    /// Assigns a still-suspended child to the job, then resumes it. Failure
    /// leaves the process terminated rather than running outside the job.
    pub(crate) fn attach_and_resume(child: &std::process::Child) -> std::io::Result<Self> {
        let process = child.as_raw_handle() as HANDLE;
        match Self::attach(child) {
            Ok(job) => {
                if let Err(error) = resume_suspended_process(process) {
                    job.terminate();
                    terminate_process_handle(process);
                    return Err(error);
                }
                Ok(job)
            }
            Err(error) => {
                terminate_process_handle(process);
                Err(error)
            }
        }
    }

    pub(crate) fn terminate(&self) {
        // SAFETY: `self.handle` is a live Job Object handle until Drop.
        unsafe {
            let _ = TerminateJobObject(self.handle, 1);
        }
    }
}

impl Drop for ProcessJob {
    fn drop(&mut self) {
        // SAFETY: `self.handle` is owned exclusively by this RAII value.
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

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
        terminate_process_handle(handle);
        let _ = CloseHandle(handle);
    }
}

fn terminate_process_handle(handle: HANDLE) {
    // SAFETY: `handle` is a live process handle supplied by the caller.
    unsafe {
        let _ = TerminateProcess(handle, 1);
    }
}

fn resume_suspended_process(process: HANDLE) -> std::io::Result<()> {
    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn NtResumeProcess(process_handle: HANDLE) -> i32;
    }
    // SAFETY: `process` is the child process HANDLE retained from spawn.
    // NtResumeProcess resumes every thread in that process, including the
    // CREATE_SUSPENDED primary thread, without enumerating threads.
    let status = unsafe { NtResumeProcess(process) };
    if status < 0 {
        Err(std::io::Error::other(format!(
            "NtResumeProcess failed with NTSTATUS {status:#x}"
        )))
    } else {
        Ok(())
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn resume_primary_thread(process_id: u32) -> std::io::Result<()> {
    // SAFETY: Toolhelp snapshot/thread APIs; every opened handle is closed.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error());
        }
        let mut entry: THREADENTRY32 = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
        if Thread32First(snapshot, &mut entry) == 0 {
            CloseHandle(snapshot);
            return Err(std::io::Error::last_os_error());
        }
        let mut resumed = false;
        loop {
            if entry.th32OwnerProcessID == process_id {
                let thread = OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID);
                if thread.is_null() || thread == INVALID_HANDLE_VALUE as HANDLE {
                    CloseHandle(snapshot);
                    return Err(std::io::Error::last_os_error());
                }
                if ResumeThread(thread) == u32::MAX {
                    let error = std::io::Error::last_os_error();
                    CloseHandle(thread);
                    CloseHandle(snapshot);
                    return Err(error);
                }
                CloseHandle(thread);
                resumed = true;
            }
            if Thread32Next(snapshot, &mut entry) == 0 {
                break;
            }
        }
        CloseHandle(snapshot);
        if !resumed {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "suspended primary thread was not found",
            ));
        }
        Ok(())
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

    #[test]
    fn resume_primary_thread_fails_closed_for_missing_process() {
        assert!(resume_primary_thread(0xFFFF_FFFE).is_err());
    }

    #[test]
    fn process_job_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<ProcessJob>();
    }
}
