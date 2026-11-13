//! `QA7-G1` (WM_CLOSE zombie): a window close must END THE PROCESS when there
//! is no tray icon to come back from.
//!
//! The main window's `WM_CLOSE` parks the window back to the tray
//! (`ShowWindow(SW_HIDE)`, never `DestroyWindow`) -- correct while a notify
//! icon exists, because the tray menu is the way back in. A tray-less process
//! (the notify-icon `NIM_ADD` failed) is left with the window visible as its
//! ONLY way in and its ONLY way out, so hiding it there is a zombie: no icon,
//! no window, and the single-instance port still held, so a relaunch answers
//! "Potflix is already running." and exits.
//!
//! This is a PROCESS-level proof, not a handler-level one: the child creates
//! the real main window and pumps messages; the parent posts `WM_CLOSE` and
//! then requires the child PROCESS to be gone. A hidden window that keeps the
//! process alive fails here, which is exactly the reported defect.
//!
//! The child is this same test binary, re-executed with `POTFLIX_WM_CLOSE_CHILD`
//! set and a single exact test selected, so nothing here needs a second crate
//! or a live PotPlayer/Discord.

use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use potflix_win::tray;
use potflix_win::window;
use windows_sys::Win32::Foundation::{HWND, LPARAM};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClassNameW, GetWindowThreadProcessId, PostMessageW, WM_CLOSE,
};

/// Set on the re-executed child only.
const CHILD_ENV: &str = "POTFLIX_WM_CLOSE_CHILD";

/// The child-side test the parent selects with `--exact`.
const CHILD_TEST: &str = "wm_close_child_creates_the_main_window";

/// How long the child may take to create its window.
const WINDOW_WAIT: Duration = Duration::from_secs(20);

/// How long the child may take to die after `WM_CLOSE`. Generous: this is a
/// process teardown, not a paint.
const EXIT_WAIT: Duration = Duration::from_secs(10);

/// The window `enum_child_window` found, as an address-sized value (a raw
/// `HWND` is not `Send`, and the callback cannot carry context).
static FOUND: AtomicIsize = AtomicIsize::new(0);

/// The pid whose top-level windows the callback accepts.
static WANT_PID: AtomicU32 = AtomicU32::new(0);

/// Accepts the first top-level window of [`WANT_PID`] whose class is the main
/// window's, and stops the enumeration.
unsafe extern "system" fn enum_child_window(hwnd: HWND, _lparam: LPARAM) -> i32 {
    let mut pid = 0u32;
    // SAFETY: plain out-parameter write on a live window handle.
    unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    if pid != WANT_PID.load(Ordering::SeqCst) {
        return 1;
    }
    let mut buf = [0u16; 64];
    // SAFETY: the buffer is ours and its length is passed.
    let n = unsafe { GetClassNameW(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
    if n <= 0 {
        return 1;
    }
    if String::from_utf16_lossy(&buf[..n as usize]) == window::MAIN_CLASS_NAME {
        FOUND.store(hwnd as isize, Ordering::SeqCst);
        return 0;
    }
    1
}

/// The child's main window handle, or `None` while it does not exist yet.
fn child_window(pid: u32) -> Option<HWND> {
    FOUND.store(0, Ordering::SeqCst);
    WANT_PID.store(pid, Ordering::SeqCst);
    // SAFETY: the callback only reads its own statics and the OS-provided
    // handles, and the enumeration runs to completion before the read below.
    unsafe { EnumWindows(Some(enum_child_window), 0) };
    let found = FOUND.load(Ordering::SeqCst);
    if found == 0 {
        None
    } else {
        Some(found as HWND)
    }
}

/// Polls for the child's main window until [`WINDOW_WAIT`] expires.
fn wait_for_child_window(pid: u32) -> Option<HWND> {
    let deadline = Instant::now() + WINDOW_WAIT;
    while Instant::now() < deadline {
        if let Some(hwnd) = child_window(pid) {
            return Some(hwnd);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

/// Polls the child until it is gone, or [`EXIT_WAIT`] expires.
fn wait_for_exit(child: &mut Child) -> bool {
    let deadline = Instant::now() + EXIT_WAIT;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => return false,
        }
    }
    false
}

/// A scratch `LOCALAPPDATA` for the child, so the real config is never read or
/// written by this test.
fn child_local_app_data() -> std::path::PathBuf {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let dir = std::env::temp_dir().join(format!("potflix-wmclose-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// CHILD half: create the real main window and pump messages until the parent
/// closes it. Returns only when `WM_QUIT` arrives, i.e. when the process is
/// meant to end.
#[test]
fn wm_close_child_creates_the_main_window() {
    if std::env::var_os(CHILD_ENV).is_none() {
        return; // the parent process: nothing to do in this half
    }
    window::create_main_window().expect("the child creates the real main window");
    tray::message_loop();
}

/// PARENT half: `WM_CLOSE` on a tray-less process must end the process.
#[test]
fn wm_close_ends_the_process_when_no_tray_icon_is_installed() {
    if std::env::var_os(CHILD_ENV).is_some() {
        return; // the child: only the half above runs there
    }
    let exe = std::env::current_exe().expect("the test binary's own path");
    let local_app_data = child_local_app_data();
    let mut child = Command::new(&exe)
        .args(["--exact", CHILD_TEST, "--nocapture", "--test-threads=1"])
        .env(CHILD_ENV, "1")
        .env("LOCALAPPDATA", &local_app_data)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn the child test process");

    let hwnd = wait_for_child_window(child.id());
    if hwnd.is_none() {
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&local_app_data);
        panic!("the child never created its main window ({CHILD_TEST})");
    }
    // SAFETY: our own child's window, and the message path is the only one this
    // harness uses -- no input injection, no focus change.
    unsafe { PostMessageW(hwnd.expect("checked above"), WM_CLOSE, 0, 0) };

    let gone = wait_for_exit(&mut child);
    if !gone {
        let _ = child.kill();
        let _ = child.wait();
    }
    let _ = std::fs::remove_dir_all(&local_app_data);
    assert!(
        gone,
        "the child process is still alive {EXIT_WAIT:?} after WM_CLOSE: the window was hidden, \
         not closed, so a tray-less process has no icon, no window and still holds the \
         single-instance port"
    );
}
