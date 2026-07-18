#![allow(dead_code)]

use super::super::{VmError, VmResult};

pub(crate) struct ExecutableBuffer {
    ptr: *mut u8,
    len: usize,
}

unsafe impl Send for ExecutableBuffer {}
unsafe impl Sync for ExecutableBuffer {}

#[cfg(any(windows, unix))]
impl ExecutableBuffer {
    pub(crate) fn new(code: &[u8]) -> VmResult<Self> {
        if code.is_empty() {
            return Err(VmError::JitNative(
                "native trace produced empty machine code".to_string(),
            ));
        }

        let ptr = unsafe { alloc_executable(code.len()) }?;
        unsafe {
            with_jit_write_protection_disabled(|| {
                std::ptr::copy_nonoverlapping(code.as_ptr(), ptr, code.len());
            });
            flush_instruction_cache(ptr, code.len());
            if let Err(err) = protect_executable_buffer(ptr, code.len()) {
                free_executable(ptr, code.len());
                return Err(err);
            }
        }

        Ok(Self {
            ptr,
            len: code.len(),
        })
    }

    pub(crate) fn entry(&self) -> *const u8 {
        self.ptr.cast_const()
    }
}

#[cfg(not(any(windows, unix)))]
impl ExecutableBuffer {
    pub(crate) fn new(_code: &[u8]) -> VmResult<Self> {
        Err(VmError::JitNative(
            "executable trace buffers are unavailable on this target".to_string(),
        ))
    }

    pub(crate) fn entry(&self) -> *const u8 {
        self.ptr.cast_const()
    }
}

#[cfg(any(windows, unix))]
impl Drop for ExecutableBuffer {
    fn drop(&mut self) {
        if self.ptr.is_null() || self.len == 0 {
            return;
        }
        unsafe {
            free_executable(self.ptr, self.len);
        }
    }
}

#[cfg(windows)]
unsafe fn alloc_executable(len: usize) -> VmResult<*mut u8> {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc,
    };

    let ptr = unsafe {
        VirtualAlloc(
            std::ptr::null_mut(),
            len,
            MEM_COMMIT | MEM_RESERVE,
            PAGE_READWRITE,
        )
    } as *mut u8;
    if ptr.is_null() {
        return Err(VmError::JitNative(
            "VirtualAlloc failed for executable trace buffer".to_string(),
        ));
    }
    Ok(ptr)
}

#[cfg(windows)]
unsafe fn free_executable(ptr: *mut u8, _len: usize) {
    use windows_sys::Win32::System::Memory::{MEM_RELEASE, VirtualFree};

    let _ = unsafe { VirtualFree(ptr.cast(), 0, MEM_RELEASE) };
}

#[cfg(windows)]
unsafe fn flush_instruction_cache(ptr: *mut u8, len: usize) {
    use windows_sys::Win32::System::Diagnostics::Debug::FlushInstructionCache;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    let process = unsafe { GetCurrentProcess() };
    let _ = unsafe { FlushInstructionCache(process, ptr.cast(), len) };
}

#[cfg(unix)]
unsafe fn alloc_executable(len: usize) -> VmResult<*mut u8> {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    let prot = libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC;
    #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
    let prot = libc::PROT_READ | libc::PROT_WRITE;
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            prot,
            libc::MAP_PRIVATE | map_anon_flag() | map_jit_flag(),
            -1,
            0,
        )
    };
    if ptr == libc::MAP_FAILED {
        return Err(VmError::JitNative(format!(
            "mmap failed for executable trace buffer: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(ptr.cast())
}

#[cfg(unix)]
unsafe fn free_executable(ptr: *mut u8, len: usize) {
    let _ = unsafe { libc::munmap(ptr.cast(), len) };
}

#[cfg(unix)]
unsafe fn flush_instruction_cache(ptr: *mut u8, len: usize) {
    let _ = (ptr, len);

    #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
    {
        unsafe extern "C" {
            fn sys_icache_invalidate(start: *mut core::ffi::c_void, len: usize);
        }

        unsafe { sys_icache_invalidate(ptr.cast(), len) };
    }

    #[cfg(all(target_arch = "aarch64", not(target_os = "macos")))]
    {
        unsafe extern "C" {
            fn __clear_cache(start: *mut u8, end: *mut u8);
        }

        unsafe { __clear_cache(ptr, ptr.add(len)) };
    }
}

#[cfg(unix)]
const fn map_anon_flag() -> i32 {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        libc::MAP_ANONYMOUS
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        libc::MAP_ANON
    }
}

#[cfg(all(unix, target_os = "macos", target_arch = "aarch64"))]
const fn map_jit_flag() -> i32 {
    libc::MAP_JIT
}

#[cfg(not(all(unix, target_os = "macos", target_arch = "aarch64")))]
const fn map_jit_flag() -> i32 {
    0
}

#[cfg(all(unix, target_os = "macos", target_arch = "aarch64"))]
pub(crate) unsafe fn protect_executable_buffer(_ptr: *mut u8, _len: usize) -> VmResult<()> {
    if unsafe { libc::pthread_jit_write_protect_supported_np() } == 0 {
        return Err(VmError::JitNative(
            "macOS JIT write protection is unavailable".to_string(),
        ));
    }
    unsafe { libc::pthread_jit_write_protect_np(1) };
    Ok(())
}

#[cfg(all(unix, not(all(target_os = "macos", target_arch = "aarch64"))))]
pub(crate) unsafe fn protect_executable_buffer(ptr: *mut u8, len: usize) -> VmResult<()> {
    if unsafe { libc::mprotect(ptr.cast(), len, libc::PROT_READ | libc::PROT_EXEC) } != 0 {
        return Err(VmError::JitNative(format!(
            "mprotect failed for executable trace buffer: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(())
}

#[cfg(windows)]
pub(crate) unsafe fn protect_executable_buffer(ptr: *mut u8, len: usize) -> VmResult<()> {
    use windows_sys::Win32::System::Memory::{PAGE_EXECUTE_READ, VirtualProtect};

    let mut previous = 0;
    if unsafe { VirtualProtect(ptr.cast(), len, PAGE_EXECUTE_READ, &mut previous) } == 0 {
        return Err(VmError::JitNative(
            "VirtualProtect failed for executable trace buffer".to_string(),
        ));
    }
    Ok(())
}

#[cfg(all(unix, target_os = "macos", target_arch = "aarch64"))]
pub(crate) unsafe fn prepare_for_execution() {
    if unsafe { libc::pthread_jit_write_protect_supported_np() } != 0 {
        unsafe { libc::pthread_jit_write_protect_np(1) };
    }
}

#[cfg(not(all(unix, target_os = "macos", target_arch = "aarch64")))]
pub(crate) unsafe fn prepare_for_execution() {}

#[cfg(all(unix, target_os = "macos", target_arch = "aarch64"))]
unsafe fn with_jit_write_protection_disabled<R>(f: impl FnOnce() -> R) -> R {
    struct WriteProtectionGuard {
        restore: bool,
    }

    impl Drop for WriteProtectionGuard {
        fn drop(&mut self) {
            if self.restore {
                unsafe { libc::pthread_jit_write_protect_np(1) };
            }
        }
    }

    let restore = unsafe { libc::pthread_jit_write_protect_supported_np() } != 0;
    let _guard = if restore {
        unsafe { libc::pthread_jit_write_protect_np(0) };
        Some(WriteProtectionGuard { restore: true })
    } else {
        None
    };
    f()
}

#[cfg(not(all(unix, target_os = "macos", target_arch = "aarch64")))]
unsafe fn with_jit_write_protection_disabled<R>(f: impl FnOnce() -> R) -> R {
    f()
}
