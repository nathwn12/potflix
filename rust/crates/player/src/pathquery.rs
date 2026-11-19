
use core::ffi::c_void;
use core::fmt;
use core::mem::size_of;
use core::ptr::{addr_of_mut, null, null_mut};
use core::time::Duration;
use std::sync::{Mutex, OnceLock, mpsc};
use std::thread;
use std::time::Instant;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, HWND_MESSAGE, MSG, PM_REMOVE,
    PeekMessageW, PostMessageW, RegisterClassExW, TranslateMessage, WM_COPYDATA, WNDCLASSEXW,
};

use crate::control::{MSG_GET_PLAY_FILE_NAME, REQUEST_TYPE};
use crate::window::Handle;

/// `pathQueryClassName`.
const PATH_QUERY_CLASS_NAME: &str = "PotflixPathQuery";

/// `pathQueryTimeout`.
const PATH_QUERY_TIMEOUT: Duration = Duration::from_millis(800);

/// The pump's idle sleep (`time.Sleep(5 * time.Millisecond)`, :236).
const PUMP_SLEEP: Duration = Duration::from_millis(5);

/// Mirrors `COPYDATASTRUCT` (`dwData` `ULONG_PTR`, `cbData` `DWORD`, `lpData`
/// `PVOID`; 24 bytes on amd64 with the mid padding) -- Go `copyData`,
/// `pathquery.go:55`. The explicit pad mirrors Go's `_ uint32`; `windows-sys`
/// carries no binding for this struct, so it is reproduced here.
#[repr(C)]
#[derive(Clone, Copy)]
struct CopyDataStruct {
    dw_data: usize,
    cb_data: u32,
    _pad: u32,
    lp_data: *mut c_void,
}

/// amd64 layout, checked at compile time (`pathquery.go:53-60`).
#[cfg(target_pointer_width = "64")]
const _: () = {
    assert!(size_of::<CopyDataStruct>() == 24);
    assert!(core::mem::offset_of!(CopyDataStruct, dw_data) == 0);
    assert!(core::mem::offset_of!(CopyDataStruct, cb_data) == 8);
    assert!(core::mem::offset_of!(CopyDataStruct, lp_data) == 16);
};

/// The distinct failures [`current_file`] can report. The messages mirror the Go
/// `errors.New` strings verbatim (`pathquery.go:182-229`) so a log line matches
/// across the port.
///
/// Go has one more: `player: no PotPlayer window` for `hwnd == 0`
/// (`pathquery.go:182-184`). The parameter is a
/// [`Handle`](crate::window::Handle), which cannot be null, so that state is
/// unrepresentable here rather than checked at run time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathQueryError {
    RegisterClass,
    CreateWindow,
    PostMessage,
    EmptyReply,
    Timeout,
}

impl fmt::Display for PathQueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::RegisterClass => "player: RegisterClassExW for path query failed",
            Self::CreateWindow => "player: CreateWindowExW for path query failed",
            Self::PostMessage => "player: PostMessageW for play file name failed",
            Self::EmptyReply => "player: empty play file name",
            Self::Timeout => "player: play file name query timed out",
        })
    }
}

impl std::error::Error for PathQueryError {}

/// Serialises [`current_file`] so the single package-level reply slot below is
/// safe to use from the poll loop (Go `pathQueryMu`, `pathquery.go:100`).
static PATH_QUERY_MU: Mutex<()> = Mutex::new(());

/// The in-flight reply slot, set for the duration of one [`current_file`] call
/// while [`PATH_QUERY_MU`] is held (Go `pathQueryCh`, `pathquery.go:104`). A
/// zero-capacity-blocking-free `sync_channel(1)` with a non-blocking `try_send`
/// is Go's buffered `chan pathQueryResult` + `select`/`default` send.
static PATH_QUERY_CH: Mutex<Option<mpsc::SyncSender<String>>> = Mutex::new(None);

/// Answers `WM_COPYDATA` with `dwData == MSG_GET_PLAY_FILE_NAME` by copying
/// `cbData` bytes out of `lpData` and signalling the in-flight call.
///
/// The `COPYDATASTRUCT` and the payload are read with a byte move, never by
/// dereferencing `lParam` as a typed pointer -- Go reaches for kernel32
/// `RtlMoveMemory` for exactly this (`pathquery.go:106-117`); the identical
/// primitive here is `ptr::copy_nonoverlapping` (a length-bounded, unaligned
/// byte move between raw addresses).
unsafe extern "system" fn path_query_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_COPYDATA && lparam != 0 {
        let mut cds: CopyDataStruct = unsafe { core::mem::zeroed() };
        unsafe {
            core::ptr::copy_nonoverlapping(
                lparam as *const u8,
                addr_of_mut!(cds).cast::<u8>(),
                size_of::<CopyDataStruct>(),
            );
        }
        if cds.cb_data > 0 && !cds.lp_data.is_null() {
            let mut data = vec![0u8; cds.cb_data as usize];
            unsafe {
                core::ptr::copy_nonoverlapping(
                    cds.lp_data as *const u8,
                    data.as_mut_ptr(),
                    data.len(),
                );
            }
            if let Some(path) = decode_copy_data_string(cds.dw_data, &data) {
                if let Ok(slot) = PATH_QUERY_CH.lock()
                    && let Some(tx) = slot.as_ref()
                {
                    let _ = tx.try_send(path);
                }
                return 1;
            }
        }
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// Registers the receiver window class exactly once; the failure is cached, as
/// Go caches it in `pathQueryClassErr` (`pathquery.go:132-160`).
static PATH_QUERY_CLASS: OnceLock<Result<(), PathQueryError>> = OnceLock::new();

/// Keeps the UTF-16 class-name buffer alive for the life of the process:
/// `RegisterClassExW` does not copy `lpszClassName` (Go keeps
/// `pathQueryClassNameP`, `pathquery.go:134`).
static PATH_QUERY_CLASS_NAME_W: OnceLock<Vec<u16>> = OnceLock::new();

fn class_name_w() -> *const u16 {
    PATH_QUERY_CLASS_NAME_W
        .get_or_init(|| {
            PATH_QUERY_CLASS_NAME
                .encode_utf16()
                .chain(core::iter::once(0))
                .collect()
        })
        .as_ptr()
}

fn ensure_path_query_class() -> Result<(), PathQueryError> {
    *PATH_QUERY_CLASS.get_or_init(|| {
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(path_query_wnd_proc),
            hInstance: unsafe { GetModuleHandleW(null()) },
            lpszClassName: class_name_w(),
            ..Default::default()
        };
        if unsafe { RegisterClassExW(&wc) } == 0 {
            Err(PathQueryError::RegisterClass)
        } else {
            Ok(())
        }
    })
}

/// Destroys the message-only receiver window when it leaves scope (Go's
/// `defer procPathDestroyWindow.Call(receiver)`, `pathquery.go:210`).
struct ReceiverWindow(HWND);

impl Drop for ReceiverWindow {
    fn drop(&mut self) {
        unsafe { DestroyWindow(self.0) };
    }
}

/// Clears the in-flight reply slot when [`current_file`] returns (Go's
/// `defer func() { pathQueryCh = nil }()`, `pathquery.go:193`).
struct ReplySlotGuard;

impl Drop for ReplySlotGuard {
    fn drop(&mut self) {
        if let Ok(mut slot) = PATH_QUERY_CH.lock() {
            *slot = None;
        }
    }
}

/// Decodes one `WM_COPYDATA` payload: `Some` iff `dw_data` is
/// [`MSG_GET_PLAY_FILE_NAME`] (0x6020) and `data` is non-empty, trimming a
/// single trailing NUL; the remaining bytes are the UTF-8 path.
///
/// Go returns `string(data)` over arbitrary bytes; Rust's `String` must be valid
/// UTF-8, so a payload that is not valid UTF-8 decodes to `None` and the caller
/// treats it as "no path" instead of carrying a mangled path into metadata
/// lookup. Every other byte is preserved as-is (including embedded NULs).
pub fn decode_copy_data_string(dw_data: usize, data: &[u8]) -> Option<String> {
    if dw_data != MSG_GET_PLAY_FILE_NAME as usize {
        return None;
    }
    if data.is_empty() {
        return None;
    }
    let data = if data[data.len() - 1] == 0 {
        &data[..data.len() - 1]
    } else {
        data
    };
    String::from_utf8(data.to_vec()).ok()
}

/// Returns the full path of the file PotPlayer (`hwnd`) is playing. An empty
/// reply, a failed post, or no reply within ~800ms yields `Err` and the caller
/// treats it as "no path". It is safe for the poll loop: the reply slot is
/// serialised by [`PATH_QUERY_MU`].
pub fn current_file(hwnd: Handle) -> Result<String, PathQueryError> {
    let _call = PATH_QUERY_MU.lock().unwrap_or_else(|e| e.into_inner());

    ensure_path_query_class()?;

    let (tx, rx) = mpsc::sync_channel::<String>(1);
    *PATH_QUERY_CH.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
    let _slot = ReplySlotGuard;

    let receiver = unsafe {
        CreateWindowExW(
            0,
            class_name_w(),
            null(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            null_mut(),
            GetModuleHandleW(null()),
            null(),
        )
    };
    if receiver.is_null() {
        return Err(PathQueryError::CreateWindow);
    }
    let _window = ReceiverWindow(receiver);

    if unsafe {
        PostMessageW(
            hwnd.as_ptr(),
            REQUEST_TYPE,
            MSG_GET_PLAY_FILE_NAME as WPARAM,
            receiver as LPARAM,
        )
    } == 0
    {
        return Err(PathQueryError::PostMessage);
    }

    let deadline = Instant::now() + PATH_QUERY_TIMEOUT;
    let mut msg: MSG = unsafe { core::mem::zeroed() };
    loop {
        match rx.try_recv() {
            Ok(path) if path.is_empty() => return Err(PathQueryError::EmptyReply),
            Ok(path) => return Ok(path),
            Err(mpsc::TryRecvError::Empty | mpsc::TryRecvError::Disconnected) => {}
        }
        if Instant::now() > deadline {
            return Err(PathQueryError::Timeout);
        }
        if unsafe { PeekMessageW(&mut msg, null_mut(), 0, 0, PM_REMOVE) } != 0 {
            unsafe {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        } else {
            thread::sleep(PUMP_SLEEP);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_copy_data_string_match() {
        let got = decode_copy_data_string(0x6020, br"Q:\Videos\movie.mkv");
        assert_eq!(got.as_deref(), Some(r"Q:\Videos\movie.mkv"));
    }

    #[test]
    fn decode_copy_data_string_wrong_dw_data() {
        assert_eq!(
            decode_copy_data_string(20486, br"Q:\Videos\movie.mkv"),
            None
        );
        assert_eq!(decode_copy_data_string(0, br"Q:\Videos\movie.mkv"), None);
    }

    #[test]
    fn decode_copy_data_string_empty() {
        // Go's `nil` and `[]byte{}` are one empty slice in Rust.
        assert_eq!(decode_copy_data_string(0x6020, &[]), None);
    }

    #[test]
    fn decode_copy_data_string_trailing_nul() {
        let got = decode_copy_data_string(0x6020, b"Q:\\Videos\\movie.mkv\0");
        assert_eq!(got.as_deref(), Some(r"Q:\Videos\movie.mkv"));
    }

    #[test]
    fn decode_copy_data_string_non_nul_preserved() {
        // No trailing NUL: the bytes pass through untouched, including any
        // embedded NUL.
        let input = "Q:\\Vid\0eos\\movie.mkv";
        let got = decode_copy_data_string(0x6020, input.as_bytes());
        assert_eq!(got.as_deref(), Some(input));
        // A single trailing NUL is trimmed; a double trailing NUL keeps one.
        let got = decode_copy_data_string(0x6020, b"Q:\\Videos\\movie.mkv\0\0");
        assert_eq!(got.as_deref(), Some("Q:\\Videos\\movie.mkv\0"));
    }

    /// `pathquery_live_test.go`: needs a running PotPlayer, so it is ignored
    /// unless explicitly requested (`cargo test -p player -- --ignored`).
    #[test]
    #[ignore = "live PotPlayer query: needs a running PotPlayer playing a file"]
    fn live_current_file() {
        let hwnd =
            crate::window::find_first().expect("live PotPlayer query: no PotPlayer window found");
        let path = current_file(hwnd).expect("current_file");
        assert!(
            !path.is_empty() && path.contains(":\\"),
            "current_file({hwnd:?}) = {path:?}, want a non-empty absolute path"
        );
    }
}
