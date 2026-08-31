//! Cancellable Windows anonymous-pipe I/O for bounded processes.
//!
//! Anonymous pipes do not provide reliable `PIPE_NOWAIT` semantics. Readers
//! wait on the pipe handle together with a manual-reset cancel event. Writers
//! keep the kernel handle in an atomic so `close` can independently
//! `CloseHandle` it and unblock a blocked `WriteFile`.

#![cfg(windows)]

use std::io::{self, Read, Write};
use std::os::windows::io::IntoRawHandle;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows_sys::Win32::System::Pipes::PeekNamedPipe;
use windows_sys::Win32::System::Threading::{CreateEventW, SetEvent, WaitForMultipleObjects};

const WAIT_SLICE_MS: u32 = 5;

pub(crate) struct CancelEvent {
    handle: HANDLE,
}

impl CancelEvent {
    pub(crate) fn new() -> io::Result<Arc<Self>> {
        // SAFETY: A null ACL and name create an unnamed manual-reset event
        // owned by this process.
        let handle = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Arc::new(Self { handle }))
    }

    #[allow(dead_code)]
    fn signal(&self) {
        // SAFETY: `handle` is the live event owned by this `CancelEvent`.
        unsafe {
            let _ = SetEvent(self.handle);
        }
    }
}

impl Drop for CancelEvent {
    fn drop(&mut self) {
        // SAFETY: exclusive ownership of the event handle.
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

// SAFETY: `HANDLE` is an opaque kernel identifier, not a pointer to Rust
// memory. The event is closed once in Drop.
unsafe impl Send for CancelEvent {}
unsafe impl Sync for CancelEvent {}

struct RawHandleCell {
    handle: AtomicUsize,
}

impl RawHandleCell {
    fn new(handle: HANDLE) -> Self {
        Self {
            handle: AtomicUsize::new(handle as usize),
        }
    }

    fn load(&self) -> HANDLE {
        self.handle.load(Ordering::Acquire) as HANDLE
    }

    fn close(&self) {
        let handle = self.handle.swap(0, Ordering::AcqRel);
        if handle != 0 {
            // SAFETY: the swapped-out value is the exclusive remaining
            // reference to this kernel handle.
            unsafe {
                let _ = CloseHandle(handle as HANDLE);
            }
        }
    }
}

impl Drop for RawHandleCell {
    fn drop(&mut self) {
        self.close();
    }
}

pub(crate) struct CancellableRead {
    inner: Arc<CancellableReadInner>,
}

struct CancellableReadInner {
    handle: RawHandleCell,
    cancel: Arc<CancelEvent>,
}

impl CancellableRead {
    pub(crate) fn from_stdout(pipe: std::process::ChildStdout) -> io::Result<Self> {
        Self::from_handle(pipe.into_raw_handle() as HANDLE)
    }

    pub(crate) fn from_stderr(pipe: std::process::ChildStderr) -> io::Result<Self> {
        Self::from_handle(pipe.into_raw_handle() as HANDLE)
    }

    fn from_handle(handle: HANDLE) -> io::Result<Self> {
        Ok(Self {
            inner: Arc::new(CancellableReadInner {
                handle: RawHandleCell::new(handle),
                cancel: CancelEvent::new()?,
            }),
        })
    }

    #[allow(dead_code)]
    pub(crate) fn close(&self) {
        self.inner.cancel.signal();
        self.inner.handle.close();
    }
}

impl Clone for CancellableRead {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl Read for CancellableRead {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let handle = self.inner.handle.load();
            if handle.is_null() {
                return Ok(0);
            }
            let mut available = 0u32;
            // SAFETY: `handle` is a live pipe handle owned by `RawHandleCell`
            // until `close` swaps it to null. PeekNamedPipe does not consume it.
            let peeked = unsafe {
                PeekNamedPipe(
                    handle,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                    &mut available,
                    std::ptr::null_mut(),
                )
            };
            if peeked == 0 {
                return Err(io::Error::last_os_error());
            }
            if available > 0 {
                let to_read = (available as usize).min(buf.len()) as u32;
                let mut read = 0u32;
                // SAFETY: `buf` is writable for `to_read` bytes and `handle`
                // remains the owned pipe handle for this call.
                let ok = unsafe {
                    ReadFile(
                        handle,
                        buf.as_mut_ptr().cast(),
                        to_read,
                        &mut read,
                        std::ptr::null_mut(),
                    )
                };
                if ok == 0 {
                    return Err(io::Error::last_os_error());
                }
                return Ok(read as usize);
            }
            let handles = [handle, self.inner.cancel.handle];
            // SAFETY: both handles are live for the duration of the wait.
            let waited = unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, WAIT_SLICE_MS) };
            if waited == WAIT_OBJECT_0 + 1 {
                return Ok(0);
            }
            if waited == WAIT_TIMEOUT {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "pipe wait timed out",
                ));
            }
            if waited != WAIT_OBJECT_0 {
                return Err(io::Error::last_os_error());
            }
        }
    }
}

pub(crate) struct CancellableWrite {
    handle: Arc<RawHandleCell>,
}

impl CancellableWrite {
    pub(crate) fn from_stdin(pipe: std::process::ChildStdin) -> Self {
        Self {
            handle: Arc::new(RawHandleCell::new(pipe.into_raw_handle() as HANDLE)),
        }
    }

    pub(crate) fn close(&self) {
        self.handle.close();
    }

    pub(crate) fn write_bytes(&self, buf: &[u8]) -> io::Result<usize> {
        let handle = self.handle.load();
        if handle.is_null() {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "stdin closed"));
        }
        let mut written = 0u32;
        // SAFETY: `handle` is the live stdin pipe until `close` swaps it out.
        // Closing that handle from another thread unblocks WriteFile.
        let ok = unsafe {
            WriteFile(
                handle,
                buf.as_ptr().cast(),
                buf.len() as u32,
                &mut written,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(written as usize)
        }
    }
}

impl Clone for CancellableWrite {
    fn clone(&self) -> Self {
        Self {
            handle: Arc::clone(&self.handle),
        }
    }
}

impl Write for CancellableWrite {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.write_bytes(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
