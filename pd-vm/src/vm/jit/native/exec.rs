use crate::{VmError, VmResult};

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
            std::ptr::copy_nonoverlapping(code.as_ptr(), ptr, code.len());
            flush_instruction_cache(ptr, code.len());
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
        MEM_COMMIT, MEM_RESERVE, PAGE_EXECUTE_READWRITE, VirtualAlloc,
    };

    let ptr = unsafe {
        VirtualAlloc(
            std::ptr::null_mut(),
            len,
            MEM_COMMIT | MEM_RESERVE,
            PAGE_EXECUTE_READWRITE,
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
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
            libc::MAP_PRIVATE | map_anon_flag(),
            -1,
            0,
        )
    };
    if ptr == libc::MAP_FAILED {
        return Err(VmError::JitNative(
            "mmap failed for executable trace buffer".to_string(),
        ));
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
