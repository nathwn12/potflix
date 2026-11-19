
use core::ffi::c_void;
use core::ptr::{NonNull, null_mut};

use windows_sys::Win32::Foundation::{HWND, LPARAM};
use windows_sys::Win32::UI::WindowsAndMessaging::{EnumWindows, GetWindowTextW, IsWindow};
use windows_sys::core::BOOL;

/// A validated PotPlayer window handle: Go's `hwnd uintptr` with the null state
/// removed by the type. Go takes a raw handle and re-checks `hwnd == 0` at the
/// entry points that care (`pathquery.go:182`); a `Handle` always refers to a
/// window, so nothing here has to, and the public functions stay safe.
pub type Handle = NonNull<c_void>;

/// PotPlayer's window-title convention (Java pollLoop :92-93, :97-98).
pub const APP_NAME: &str = "PotPlayer";
pub const SUFFIX: &str = " - PotPlayer";

/// `maxWindowText` is the `GetWindowTextW` buffer size in UTF-16 units. Java used
/// a fixed WINDOW_TEXT_SIZE buffer; media titles are long, so 1024 is generous.
const MAX_WINDOW_TEXT: usize = 1024;

/// Reports whether a window title identifies a PotPlayer window.
pub fn matches(title: &str) -> bool {
    title.ends_with(SUFFIX) || title == APP_NAME
}

/// Receives the first matching HWND during an `EnumWindows` sweep.
/// [`find_first`] is synchronous and the poll loop is single-threaded, so this
/// package-level slot is safe; it is deliberately not advertised as concurrent
/// (Go `enumTarget`, `window.go:43`). Reproduced as a plain `static`, not
/// improved into a synchronised handoff.
static mut ENUM_TARGET: HWND = null_mut();

/// Stops the enumeration (returns FALSE) on the first match.
unsafe extern "system" fn enum_windows_callback(hwnd: HWND, _lparam: LPARAM) -> BOOL {
    let Some(handle) = NonNull::new(hwnd) else {
        return 1;
    };
    if matches(&title(handle)) {
        unsafe { ENUM_TARGET = hwnd };
        0
    } else {
        1
    }
}

/// Returns the first PotPlayer window in `EnumWindows` order, if any. Java
/// tracked only `windows.get(0)`; multiple players are ignored.
pub fn find_first() -> Option<Handle> {
    unsafe {
        ENUM_TARGET = null_mut();
        EnumWindows(Some(enum_windows_callback), 0);
        NonNull::new(ENUM_TARGET)
    }
}

/// Reads a window's current title via `GetWindowTextW`. Returns `""` when the
/// window has no title or is gone.
pub fn title(hwnd: Handle) -> String {
    let mut buf = [0u16; MAX_WINDOW_TEXT];
    let n = unsafe { GetWindowTextW(hwnd.as_ptr(), buf.as_mut_ptr(), MAX_WINDOW_TEXT as i32) };
    if n <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buf[..n as usize])
}

/// Reports whether a window handle is still valid (Java `IsWindow`, :110).
pub fn alive(hwnd: Handle) -> bool {
    unsafe { IsWindow(hwnd.as_ptr()) != 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_title() {
        let cases: [(&str, bool); 10] = [
            ("Some Movie - PotPlayer", true),
            ("PotPlayer", true),
            ("Show.S01E02.1080p - PotPlayer", true),
            ("- PotPlayer", false),
            ("PotPlayer - PotPlayer", true),
            ("Some Movie", false),
            ("Some Movie - PotPlayer extra", false),
            ("PotPlayer2", false),
            ("", false),
            ("PotPlayer ", false),
        ];
        for (title, want) in cases {
            assert_eq!(matches(title), want, "matches({title:?})");
        }
    }
}
