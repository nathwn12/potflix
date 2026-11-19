
use std::io;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_IO_PENDING, GENERIC_READ, GENERIC_WRITE, HANDLE,
    INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_FLAG_OVERLAPPED, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_UNLIMITED_INSTANCES,
};
use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject, INFINITE};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

/// One raw write, honouring the pipe's write deadline (Go's `io.Writer`).
pub(crate) trait WriteBytes {
    fn write_some(&self, buf: &[u8]) -> io::Result<usize>;
}

/// One raw read (Go's `io.Reader`).
pub(crate) trait ReadBytes {
    fn read_some(&self, buf: &mut [u8]) -> io::Result<usize>;
}

/// How this handle's I/O is issued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IoMode {
    /// Client end: `FILE_FLAG_OVERLAPPED`, every operation waits on its own event.
    Overlapped,
    /// Fake-server end: plain synchronous `ReadFile`/`WriteFile`.
    Blocking,
}

/// One named-pipe handle.
pub(crate) struct NamedPipe {
    h: HANDLE,
    mode: IoMode,
    closed: AtomicBool,
    write_deadline: Mutex<Option<Instant>>,
}

// SAFETY: the handle value is an opaque kernel object; every method takes
// `&self` and the kernel serialises its own I/O, so sharing across threads is
// sound (the Go side shares the same `*os.File` the same way).
unsafe impl Send for NamedPipe {}
unsafe impl Sync for NamedPipe {}

impl NamedPipe {
    fn new(h: HANDLE, mode: IoMode) -> NamedPipe {
        NamedPipe {
            h,
            mode,
            closed: AtomicBool::new(false),
            write_deadline: Mutex::new(None),
        }
    }

    /// Opens the client end of one pipe (`openPipe`, `pipe.go`), with
    /// `FILE_FLAG_OVERLAPPED`.
    ///
    /// The handle MUST be overlapped. A synchronous handle makes a pending
    /// `ReadFile` block a subsequent `WriteFile` on the same handle, which
    /// deadlocks this client's read pump against its own writes. Issuing both
    /// as overlapped operations (own `OVERLAPPED`, own event) keeps the read
    /// pump and the write path independent.
    pub(crate) fn open(name: &str) -> io::Result<NamedPipe> {
        let wide = wide(name);
        // SAFETY: `wide` is a NUL-terminated UTF-16 buffer that outlives the call.
        let h = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                ptr::null_mut(),
            )
        };
        if h.is_null() || h == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        Ok(NamedPipe::new(h, IoMode::Overlapped))
    }

    /// Creates one server instance and blocks in `ConnectNamedPipe`
    /// (`FakeServer.Accept`, `fakepipe.go`). `buf_bytes` is the per-direction
    /// pipe buffer: a tiny value lets a test force the client's write to block.
    pub(crate) fn create_server(name: &str, buf_bytes: u32) -> io::Result<NamedPipe> {
        let wide = wide(name);
        // SAFETY: `wide` is a NUL-terminated UTF-16 buffer that outlives the call.
        let h = unsafe {
            CreateNamedPipeW(
                wide.as_ptr(),
                PIPE_ACCESS_DUPLEX,
                0, // byte-type, blocking
                PIPE_UNLIMITED_INSTANCES,
                buf_bytes,
                buf_bytes,
                0,
                ptr::null(),
            )
        };
        if h.is_null() || h == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // Blocks until a client connects. ERROR_PIPE_CONNECTED (a client raced us
        // between create and connect) is also success, so the return value is
        // intentionally not checked.
        // SAFETY: `h` is live and this call's OVERLAPPED is null, i.e. blocking.
        unsafe { ConnectNamedPipe(h, ptr::null_mut()) };
        Ok(NamedPipe::new(h, IoMode::Blocking))
    }

    /// Closes the handle once. Any operation still pending is cancelled first so
    /// a reader parked in `read_some` returns instead of parking forever
    /// (Go: `os.File.Close` through the runtime poller).
    pub(crate) fn close(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        // SAFETY: `h` is live until the line below, and the flag above makes this
        // the single close. A null OVERLAPPED cancels every operation the process
        // has pending on this handle, whichever thread issued it.
        unsafe {
            CancelIoEx(self.h, ptr::null());
            CloseHandle(self.h);
        }
    }

    /// Arms (or clears) the deadline applied to every write on this pipe. It
    /// mirrors `SetWriteDeadline`: an expired deadline fails the write.
    pub(crate) fn set_write_deadline(&self, deadline: Option<Instant>) {
        *self.write_deadline.lock().expect("write deadline") = deadline;
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    fn closed_err(&self) -> io::Error {
        io::Error::new(io::ErrorKind::NotConnected, "named pipe is closed")
    }
}

impl Drop for NamedPipe {
    fn drop(&mut self) {
        self.close();
    }
}

impl ReadBytes for NamedPipe {
    fn read_some(&self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.is_closed() {
            return Err(self.closed_err());
        }
        let len = buf.len().min(u32::MAX as usize) as u32;
        match self.mode {
            IoMode::Overlapped => overlapped_op(self.h, buf.as_mut_ptr(), len, IoDir::Read, None),
            IoMode::Blocking => blocking_op(self.h, buf.as_mut_ptr(), len, IoDir::Read),
        }
    }
}

impl WriteBytes for NamedPipe {
    fn write_some(&self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.is_closed() {
            return Err(self.closed_err());
        }
        let len = buf.len().min(u32::MAX as usize) as u32;
        match self.mode {
            IoMode::Overlapped => {
                let deadline = *self.write_deadline.lock().expect("write deadline");
                overlapped_op(self.h, buf.as_ptr() as *mut u8, len, IoDir::Write, deadline)
            }
            IoMode::Blocking => blocking_op(self.h, buf.as_ptr() as *mut u8, len, IoDir::Write),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum IoDir {
    Read,
    Write,
}

/// One synchronous operation on a blocking handle.
fn blocking_op(h: HANDLE, buf: *mut u8, len: u32, dir: IoDir) -> io::Result<usize> {
    let mut n: u32 = 0;
    // SAFETY: `h` is live, and `buf`/`len` describe caller-owned storage that the
    // call fills before returning. A blocking handle takes no OVERLAPPED.
    let ok = unsafe {
        match dir {
            IoDir::Read => ReadFile(h, buf, len, &mut n, ptr::null_mut()),
            IoDir::Write => WriteFile(h, buf as *const u8, len, &mut n, ptr::null_mut()),
        }
    };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}

/// One overlapped operation on a client handle, collected by waiting on its own
/// event. `deadline` bounds the wait; on expiry the operation is cancelled and
/// drained before returning, so the caller's buffer is never still in flight.
fn overlapped_op(
    h: HANDLE,
    buf: *mut u8,
    len: u32,
    dir: IoDir,
    deadline: Option<Instant>,
) -> io::Result<usize> {
    // SAFETY: no attribute, manual-reset = 0, initially non-signaled, unnamed.
    let event = unsafe { CreateEventW(ptr::null(), 0, 0, ptr::null()) };
    if event.is_null() {
        return Err(io::Error::last_os_error());
    }
    let mut ov = OVERLAPPED {
        hEvent: event,
        ..OVERLAPPED::default()
    };
    let mut n: u32 = 0;
    // SAFETY: `h` is an overlapped handle; `buf`/`len` are the caller's storage,
    // kept alive by the caller until this function returns; `ov` outlives the
    // operation and is drained on every path below.
    let started = unsafe {
        match dir {
            IoDir::Read => ReadFile(h, buf, len, &mut n, &mut ov),
            IoDir::Write => WriteFile(h, buf as *const u8, len, &mut n, &mut ov),
        }
    };
    let outcome = if started != 0 {
        Ok(n as usize)
    } else {
        // SAFETY: read immediately after the failing call, as Windows requires.
        let err = unsafe { GetLastError() };
        if err != ERROR_IO_PENDING {
            Err(io::Error::from_raw_os_error(err as i32))
        } else {
            let wait_ms = match deadline {
                None => INFINITE,
                Some(d) => d
                    .saturating_duration_since(Instant::now())
                    .as_millis()
                    .min(u32::MAX as u128) as u32,
            };
            // SAFETY: `event` is live and is signalled by the operation's completion.
            match unsafe { WaitForSingleObject(event, wait_ms) } {
                WAIT_OBJECT_0 => overlapped_result(h, &ov),
                WAIT_TIMEOUT => {
                    drain(h, event, &ov);
                    Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "named pipe deadline exceeded",
                    ))
                }
                _ => {
                    let e = io::Error::last_os_error();
                    drain(h, event, &ov);
                    Err(e)
                }
            }
        }
    };
    // SAFETY: no operation is pending on `event` at this point.
    unsafe { CloseHandle(event) };
    outcome
}

/// Cancels one expired operation and waits for it to complete, so nothing is
/// left writing into a buffer the caller is about to reuse.
fn drain(h: HANDLE, event: HANDLE, ov: &OVERLAPPED) {
    // SAFETY: `h` is live and `ov` is the operation being cancelled.
    unsafe {
        CancelIoEx(h, ov);
        WaitForSingleObject(event, INFINITE);
    }
    let _ = overlapped_result(h, ov);
}

fn overlapped_result(h: HANDLE, ov: &OVERLAPPED) -> io::Result<usize> {
    let mut done: u32 = 0;
    // SAFETY: the operation on `ov` has completed, so this only reads its status.
    let ok = unsafe { GetOverlappedResult(h, ov, &mut done, 0) };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(done as usize)
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}
