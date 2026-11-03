//! `win32` -- mirrors the Go root file `go\win32.go`.
//!
//! The raw Win32 surface: `user32`, `kernel32`, `shell32`, `gdi32`, `ole32` and
//! the procedure pointers Go reaches through `syscall.NewLazyDLL` /
//! `NewLazyProc`. In Rust this is `windows-sys` (feature-gated bindings), with
//! the same DLLs and the same procedures -- no C toolchain, no `cc` dependency.
//!
//! # Layout law
//!
//! The Go port hands Win32 raw struct pointers, so every struct below is a
//! byte-exact mirror of `go\win32.go`: pointer-sized fields are `uintptr`
//! (`usize` here, 8 bytes on amd64), fixed fields stay `u32`/`i32`, and
//! `#[repr(C)]` reproduces the two 4-byte pads Go inserts in `NOTIFYICONDATAW`
//! (after `cbSize`, and after `uCallbackMessage`/`hIcon`). A wrong offset is a
//! silent runtime failure -- `Shell_NotifyIconW` just returns FALSE -- so every
//! struct carries a compile-time size assertion and `#[cfg(test)]` offset
//! assertions: adding, removing, or reordering a field must break the build,
//! never the tray. The field names are snake_case (Go's are camelCase); the
//! offset assertions, not the names, are what pin the ABI.
//!
//! Measured with `go1.27.0` `unsafe.Sizeof` on amd64 and asserted below:
//! `wndClassEx` 80, `guid` 16, `notifyIconData` 976, `point` 8, `msgT` 48,
//! `rect` 16, `paintStruct` **72**, `browseInfoW` 64, `trackMouseEvent` 24.
//!
//! `paintStruct` is the one place the Go source comment is wrong: `win32.go:240`
//! (and the port map) say "68 bytes on amd64", but 68 is the sum of the field
//! widths, not the ABI size. Go rounds the struct up to its 8-byte alignment and
//! reports 72, exactly like `PAINTSTRUCT` in the Windows headers -- so 72 is
//! asserted here. A 68-byte buffer would be a stack overwrite by `BeginPaint`.
//!
//! 32-bit (x86) layouts are unverified (the port map flags them), so the size
//! assertions are gated to `x86_64` rather than pretending to cover x86.
//!
//! # Value traps reproduced, not smoothed over
//!
//! * `cwUseDefault` (`win32.go:229`) is `CW_USEDEFAULT`, i.e. `(int)-2147483648`
//!   -- see [`CW_USEDEFAULT`].
//! * `attachParentProcess` / `stdOutputHandle` / `stdErrorHandle`
//!   (`win32.go:75-78`) are Go's `^uintptr(n)` complement form -- see
//!   [`ATTACH_PARENT_PROCESS`].

use core::mem::size_of;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
pub use windows_sys::Win32::Graphics::Gdi;

// ---------------------------------------------------------------------------
// Binding table (`go\win32.go:8-36`).
//
// Each `pub use` IS the proc pointer Go builds with `NewLazyProc`: same DLL,
// same entry point. A few user32 entries live in `windows-sys`'s `Gdi` module
// because the header that documents them is `wingdi.h`, not because the DLL
// differs.
// ---------------------------------------------------------------------------

// user32 (`go\win32.go:13-28`).
pub use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconFromResourceEx, CreatePopupMenu, CreateWindowExW, DefWindowProcW,
    DestroyMenu, DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW, PostMessageW,
    PostQuitMessage, RegisterClassExW, SetForegroundWindow, TrackPopupMenu, TranslateMessage,
};

// kernel32 (`go\win32.go:30-33`).
pub use windows_sys::Win32::Storage::FileSystem::CreateFileW;
pub use windows_sys::Win32::System::Console::{AttachConsole, GetStdHandle};
pub use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;

// shell32 (`go\win32.go:35`).
pub use windows_sys::Win32::UI::Shell::{
    SHBrowseForFolderW, SHGetPathFromIDListW, Shell_NotifyIconW,
};

// ---------------------------------------------------------------------------
// Settings-window bindings (`go\win32.go:140-172`): paint, show, message,
// cursor, and folder-picker procs; gdi32 + ole32.
// ---------------------------------------------------------------------------

// user32 (`go\win32.go:141-151`; `BeginPaint`/`EndPaint`/`DrawTextW`/
// `UpdateWindow` are declared under `Gdi` in `windows-sys` but still come from
// `user32.dll`).
pub use windows_sys::Win32::Graphics::Gdi::{BeginPaint, DrawTextW, EndPaint, UpdateWindow};
pub use windows_sys::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
pub use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetClientRect, GetDlgItem, LoadCursorW, SendMessageW, SetWindowTextW, ShowWindow,
};

// shell32 (`go\win32.go:153-154`).
// gdi32 + ole32 (`go\win32.go:156-171`).
pub use windows_sys::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateFontW, CreateSolidBrush, DeleteDC, DeleteObject, RoundRect,
    SelectObject, SetBkMode, SetTextColor,
};
pub use windows_sys::Win32::System::Com::CoTaskMemFree;
pub use windows_sys::Win32::System::Ole::OleInitialize;

// ---------------------------------------------------------------------------
// Owner-drawn settings-window bindings, round 2 (`go\win32.go:270-286`): DPI,
// geometry, mouse tracking, and GDI helpers.
// ---------------------------------------------------------------------------

pub use windows_sys::Win32::Graphics::Gdi::{
    CreateCompatibleBitmap, CreatePen, FillRect, GetStockObject, InvalidateRect,
};
pub use windows_sys::Win32::UI::HiDpi::{
    GetAwarenessFromDpiAwarenessContext, GetThreadDpiAwarenessContext,
};
pub use windows_sys::Win32::UI::HiDpi::{GetDpiForSystem, GetDpiForWindow};
pub use windows_sys::Win32::UI::Input::KeyboardAndMouse::TrackMouseEvent;
pub use windows_sys::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, DrawIconEx, GetSystemMetrics, GetWindowRect, MoveWindow, SetCursor,
    SetWindowPos,
};

/// `SetWindowPos` flags: keep position/size/Z-order/activation and only announce
/// a frame change; and the `WM_DPICHANGED` move (`SWP_NOZORDER | SWP_NOACTIVATE`).
pub const SWP_NOSIZE: u32 = 0x0001;
pub const SWP_NOMOVE: u32 = 0x0002;
pub const SWP_NOZORDER: u32 = 0x0004;
pub const SWP_NOACTIVATE: u32 = 0x0010;
pub const SWP_FRAMECHANGED: u32 = 0x0020;

/// `DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2` (`(HANDLE)-4`), the context this
/// app declares in its manifest.
pub const DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2: isize = -4;

/// `GetAwarenessFromDpiAwarenessContext` result for Per-Monitor-V2.
pub const DPI_AWARENESS_PER_MONITOR_AWARE: i32 = 2;

/// The process's DPI awareness for the calling thread, as
/// `GetAwarenessFromDpiAwarenessContext(GetThreadDpiAwarenessContext())`.
/// `-1` when the API answers nothing (pre-1607 Windows).
#[must_use]
pub fn dpi_awareness() -> i32 {
    // SAFETY: both procs read process/thread state only.
    unsafe {
        let ctx = GetThreadDpiAwarenessContext();
        if ctx.is_null() {
            return -1;
        }
        GetAwarenessFromDpiAwarenessContext(ctx)
    }
}

// ---------------------------------------------------------------------------
// Native dark window chrome (Rust-only; `go\win32.go` has no counterpart -- the
// Go build keeps the stock light caption).
//
// `dwmapi.dll` ships with the OS and is reached through the same `windows-sys`
// FFI path as every proc above, so there is no new dependency: only the additive
// `Win32_Graphics_Dwm` feature, exactly like the settings-window features.
//
// Attribute numbers and OS floors (learn.microsoft.com/windows/win32/api/dwmapi/
// ne-dwmapi-dwmwindowattribute, verified 2026-10-14): `DWMWA_USE_IMMERSIVE_DARK_MODE`
// is 20 (Windows 11 22000+; the pre-22000 Windows 10 attribute was 19, which this
// app does not target), `DWMWA_WINDOW_CORNER_PREFERENCE` 33, `DWMWA_BORDER_COLOR`
// 34, `DWMWA_CAPTION_COLOR` 35, `DWMWA_TEXT_COLOR` 36.
// ---------------------------------------------------------------------------

pub use windows_sys::Win32::Graphics::Dwm::{
    DWMWA_BORDER_COLOR, DWMWA_CAPTION_COLOR, DWMWA_TEXT_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE,
    DwmSetWindowAttribute,
};

/// `DwmSetWindowAttribute` for one 4-byte value, best-effort.
///
/// On a Windows build that predates an attribute the call returns a failing
/// HRESULT; it is discarded so an unknown attribute degrades to the stock
/// chrome instead of aborting window creation.
fn dwm_set(hwnd: HWND, attr: i32, value: u32) {
    // SAFETY: `value` is a 4-byte `COLORREF`/`BOOL` that outlives the call; DWM
    // copies it, and the returned HRESULT needs no cleanup.
    unsafe {
        DwmSetWindowAttribute(
            hwnd,
            attr as u32,
            std::ptr::from_ref(&value).cast(),
            size_of::<u32>() as u32,
        );
    }
}

/// Paint the OS caption dark so the title bar continues the dark client area
/// instead of the stock white bar (Rust-only chrome; no layout or behavior).
///
/// `dwma_use_immersive_dark_mode` darkens the caption and its buttons;
/// `dwma_caption_color` pins the bar to `caption` (a `COLORREF`, i.e. an
/// [`crate::window::rgb`] value) so it matches the client background exactly
/// rather than the system grey; `dwma_text_color` keeps the title readable on
/// it; and `dwma_border_color` blends the 1px DWM frame dot. Also passed to the
/// settings view's window, which shares this class' caption.
///
/// This is the WHOLE chrome change: `WS_CAPTION` stays. A custom-drawn caption
/// (`WS_POPUP` + `WM_NCCALCSIZE` + `WM_NCHITTEST`, per
/// learn.microsoft.com/windows/win32/dwm/customframe, verified 2026-10-14) would
/// mean re-implementing drag/resize hit testing, `DwmDefWindowProc` caption
/// buttons, and Windows 11 snap layouts -- and would put this app's eight
/// hard-won UI-reentrancy fixes (commit 3503ddb) back in play, since every one
/// of those messages synchronously re-enters `main_wnd_proc`.
pub fn apply_dark_caption(hwnd: HWND, caption: u32, text: u32) {
    dwm_set(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, 1);
    dwm_set(hwnd, DWMWA_CAPTION_COLOR, caption);
    dwm_set(hwnd, DWMWA_TEXT_COLOR, text);
    dwm_set(hwnd, DWMWA_BORDER_COLOR, caption);
}

// ---------------------------------------------------------------------------
// Dark owner-drawn tray menu (Rust-only: the Go build leaves the stock white
// popup). `SetMenuInfo`/`MIM_BACKGROUND` paints the popup's interior dark
// (learn.microsoft.com/windows/win32/api/winuser/nf-winuser-setmenuinfo, verified
// 2026-10-14); `MF_OWNERDRAW` + `WM_MEASUREITEM`/`WM_DRAWITEM` gives each item
// palette-correct text and hover. The menu's own item ids, labels and dispatch
// are untouched, so the tray's behavior is unchanged.
// ---------------------------------------------------------------------------

pub use windows_sys::Win32::UI::Controls::{DRAWITEMSTRUCT, MEASUREITEMSTRUCT};
pub use windows_sys::Win32::UI::WindowsAndMessaging::{MENUINFO, SetMenuInfo};

/// `InsertMenuItemW` + `MENUITEMINFOW`: the documented owner-draw recipe is an
/// `MFT_OWNERDRAW` item carrying `dwItemData`, measured in `WM_MEASUREITEM`
/// and painted in `WM_DRAWITEM` (learn.microsoft.com/windows/win32/menurc/
/// using-menus "Creating Owner-Drawn Menu Items", accessed 2026-10-14).
pub use windows_sys::Win32::UI::WindowsAndMessaging::{InsertMenuItemW, MENUITEMINFOW};

/// `MIIM_*` mask bits and `MFT_*/MFS_*` type/state bits for `MENUITEMINFOW`.
pub const MIIM_STATE: u32 = 0x0001;
pub const MIIM_ID: u32 = 0x0002;
pub const MIIM_STRING: u32 = 0x0040;
pub const MIIM_FTYPE: u32 = 0x0100;
pub const MIIM_DATA: u32 = 0x0020;
pub const MFT_STRING: u32 = 0x0000;
pub const MFT_OWNERDRAW: u32 = 0x0100;
pub const MFS_ENABLED: u32 = 0x0000;

/// `WM_MEASUREITEM` / `WM_DRAWITEM`: sent to the menu-owner window for each
/// owner-drawn item before display and on every paint.
pub const WM_MEASUREITEM: u32 = 0x002C;
pub const WM_DRAWITEM: u32 = 0x002B;

/// `MIM_BACKGROUND | MIM_APPLYTOSUBMENUS`: paint the popup background with
/// `hbrBack`, and apply the same to submenus.
pub const MENU_INFO_BACKGROUND: u32 = 2 | 0x8000_0000;
pub const MF_OWNERDRAW: u32 = 0x0000_0100;
pub const MF_SEPARATOR: u32 = 0x0000_0800;
/// `ODS_SELECTED` / `ODS_HOTLIGHT` / `ODS_DISABLED` (`itemState` in
/// `DRAWITEMSTRUCT`).
pub const ODS_SELECTED: u32 = 0x0001;
pub const ODS_DISABLED: u32 = 0x0004;
pub const ODS_HOTLIGHT: u32 = 0x0040;
/// `ODS_FOCUS` (`0x0010`): the item has the keyboard focus. The Fix match
/// dialog's owner-drawn buttons read it for the focus ring.
pub const ODS_FOCUS: u32 = 0x0010;
/// `ODT_MENU`: the owner-draw request is a menu item.
pub const ODT_MENU: u32 = 1;
/// `ODT_LISTBOX` (`2`) / `ODT_BUTTON` (`4`) / `ODT_STATIC` (`5`): the
/// owner-draw request kinds the Fix match dialog paints (`WM_DRAWITEM`).
pub const ODT_LISTBOX: u32 = 2;
pub const ODT_BUTTON: u32 = 4;
pub const ODT_STATIC: u32 = 5;

// ---------------------------------------------------------------------------
// GDI+ (`gdiplus.dll`) -- antialiased rounded geometry.
//
// GDI's own `RoundRect` rasterises with the selected 1px pen and aliased
// brushes: its arcs are hard-stepped, which at 96 DPI reads as a chunky corner.
// GDI+ fills a `GraphicsPath` with `SmoothingModeAntiAlias`, which is the
// documented smoothed path for arbitrary geometry (learn.microsoft.com/windows/
// win32/gdiplus/-gdiplus-antialiasing-with-lines-and-curves-about, verified
// 2026-10-14). Same DLL family as gdi32, same windows-sys FFI path: no new
// dependency, no C toolchain.
// ---------------------------------------------------------------------------

pub use windows_sys::Win32::Graphics::GdiPlus::{
    CombineModeReplace, FillModeAlternate, GdipAddPathArcI, GdipClosePathFigure,
    GdipCreateBitmapFromFile, GdipCreateFromHDC, GdipCreatePath, GdipCreatePath2I,
    GdipCreatePen1, GdipCreateSolidFill, GdipDeleteBrush, GdipDeleteGraphics, GdipDeletePath,
    GdipDeletePen, GdipDisposeImage, GdipDrawImageRectI, GdipDrawPath, GdipFillPath,
    GdipGetImageHeight, GdipGetImageWidth, GdipResetClip, GdipSetClipPath,
    GdipSetInterpolationMode, GdipSetSmoothingMode,
    GdiplusShutdown, GdiplusStartup, InterpolationModeHighQualityBicubic,
    SmoothingModeAntiAlias, UnitPixel,
};

/// `GdiplusStartupInput` (version 1, no debug callback, background thread kept).
/// Built by hand so the crate does not have to name the bindings' struct.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct GdiplusStartupInput {
    pub gdiplus_version: u32,
    pub debug_event_callback: usize,
    pub suppress_background_thread: i32,
    pub suppress_external_codecs: i32,
}

impl Default for GdiplusStartupInput {
    fn default() -> Self {
        Self {
            gdiplus_version: 1,
            debug_event_callback: 0,
            suppress_background_thread: 0,
            suppress_external_codecs: 0,
        }
    }
}

#[cfg(target_arch = "x86_64")]
const _: () = assert!(size_of::<GdiplusStartupInput>() == 24);
#[cfg(target_arch = "x86_64")]
const _: () =
    assert!(size_of::<windows_sys::Win32::Graphics::GdiPlus::GdiplusStartupInput>() == 24);

// ---------------------------------------------------------------------------
// Window messages (`go\win32.go:38-49`).
// ---------------------------------------------------------------------------

pub const WM_NULL: u32 = 0x0000;
pub const WM_DESTROY: u32 = 0x0002;
pub const WM_CLOSE: u32 = 0x0010;
pub const WM_RBUTTON_UP: u32 = 0x0205;
pub const WM_CONTEXT_MENU: u32 = 0x007B;

pub const WM_APP: u32 = 0x8000;
/// `Shell_NotifyIconW` callback message (`go\win32.go:47`).
pub const WM_APP_TRAY: u32 = WM_APP + 1;
/// Self-posted: stop the message loop (`go\win32.go:48`).
pub const WM_APP_QUIT: u32 = WM_APP + 2;

// `NOTIFYICONDATAW` flags and `Shell_NotifyIconW` verbs (`go\win32.go:52-59`).
pub const NIM_ADD: u32 = 0x00000000;
pub const NIM_DELETE: u32 = 0x00000002;

pub const NIF_MESSAGE: u32 = 0x00000001;
pub const NIF_ICON: u32 = 0x00000002;
pub const NIF_TIP: u32 = 0x00000004;

// Menu / popup flags (`go\win32.go:62-67`).
pub const MF_STRING: u32 = 0x00000000;
pub const TPM_RIGHT_BUTTON: u32 = 0x0002;
pub const TPM_BOTTOM_ALIGN: u32 = 0x0020;
pub const TPM_RETURN_CMD: u32 = 0x0100;

// `CreateIconFromResourceEx` / `CreateFileW` constants (`go\win32.go:70-79`).
pub const ICON_RESOURCE_VERSION: u32 = 0x00030000;
pub const GENERIC_WRITE: u32 = 0x40000000;
pub const FILE_SHARE_READ_WRITE: u32 = 0x00000001 | 0x00000002;
pub const OPEN_EXISTING: u32 = 3;

/// Go `attachParentProcess = ^uintptr(0)` (`win32.go:75`): `(DWORD)-1`, the
/// complement form. Spelled as the complement of zero, not as a literal, so the
/// 32-bit parameter value is provably the same bits Go passes.
pub const ATTACH_PARENT_PROCESS: u32 = !0;

/// Go `stdOutputHandle = ^uintptr(10)` (`win32.go:77`): `(DWORD)-11`,
/// `STD_OUTPUT_HANDLE`.
pub const STD_OUTPUT_HANDLE: u32 = !10;

/// Go `stdErrorHandle = ^uintptr(11)` (`win32.go:78`): `(DWORD)-12`,
/// `STD_ERROR_HANDLE`.
pub const STD_ERROR_HANDLE: u32 = !11;

// Show / paint / icon messages (`go\win32.go:175-185`).
pub const SW_HIDE: i32 = 0;
pub const SW_SHOW: i32 = 5;

pub const WM_PAINT: u32 = 0x000F;

/// `WM_PRINT` (0x0317): `PrintWindow` asks the window to draw into the
/// caller's DC. `DefWindowProcW` only draws the frame, so the client area is
/// the app's job; without it a headless snapshot is an empty bitmap.
pub const WM_PRINT: u32 = 0x0317;

/// `WM_PRINTCLIENT` (0x0318): the client-only form of [`WM_PRINT`].
pub const WM_PRINTCLIENT: u32 = 0x0318;

pub const WM_COMMAND: u32 = 0x0111;
pub const WM_SET_ICON: u32 = 0x0080;

/// `WM_GETICON` (0x007F): the message that READS the window's own icon slots --
/// what the titlebar, alt-tab and the taskbar actually consume. Distinct from
/// the class slots `GetClassLongPtrW` reads, which is why the proof checks both.
pub const WM_GET_ICON: u32 = 0x007F;

pub const ICON_SMALL: u32 = 0;
pub const ICON_BIG: u32 = 1;

// Window / control styles used by the settings window (`go\win32.go:188-200`).
pub const WS_CAPTION: u32 = 0x00C00000;
pub const WS_SYS_MENU: u32 = 0x00080000;
pub const WS_MINIMIZE_BOX: u32 = 0x00020000;
pub const WS_CHILD: u32 = 0x40000000;
pub const WS_VISIBLE: u32 = 0x10000000;
pub const WS_BORDER: u32 = 0x00800000;
pub const WS_V_SCROLL: u32 = 0x00200000;
pub const WS_TAB_STOP: u32 = 0x00010000;

pub const LBS_NOTIFY: u32 = 0x0001;
pub const BN_CLICKED: u32 = 0;
/// `BS_OWNERDRAW` (`0x000B`): the Fix match dialog's push buttons paint
/// themselves in `WM_DRAWITEM` with the main window's button anatomy.
pub const BS_OWNERDRAW: u32 = 0x000B;
/// `LBS_OWNERDRAWFIXED` (`0x0010`) / `LBS_HASSTRINGS` (`0x0040`): the Fix match
/// results list is fixed-height owner-drawn but keeps its strings.
pub const LBS_OWNERDRAWFIXED: u32 = 0x0010;
pub const LBS_HASSTRINGS: u32 = 0x0040;
/// `SS_OWNERDRAW` (`0x000D`): the Fix match empty panel / progress strip /
/// header rule paint themselves in `WM_DRAWITEM`.
pub const SS_OWNERDRAW: u32 = 0x000D;
/// `EN_CHANGE` (`0x0300`): the edit-text-changed notification the Fix match
/// dialog uses to re-arm Search.
pub const EN_CHANGE: u32 = 0x0300;
/// `EN_SETFOCUS` (`0x0100`) / `EN_KILLFOCUS` (`0x0200`): the edit focus
/// notifications the Fix match dialog uses to repaint its field frames.
pub const EN_SETFOCUS: u32 = 0x0100;
pub const EN_KILLFOCUS: u32 = 0x0200;

// List box messages used by the settings window (`go\win32.go:203-209`).
pub const LB_ADD_STRING: u32 = 0x0180;
pub const LB_DELETE_STRING: u32 = 0x0182;
pub const LB_RESET_CONTENT: u32 = 0x0184;
pub const LB_GET_CUR_SEL: u32 = 0x0188;
pub const LB_GET_TEXT: u32 = 0x0189;

// Paint constants used by the settings window (`go\win32.go:212-230`).
pub const DT_LEFT: u32 = 0x0000;
pub const DT_SINGLE_LINE: u32 = 0x0020;
pub const DT_NO_PREFIX: u32 = 0x0800;

pub const TRANSPARENT: i32 = 1;

pub const FW_NORMAL: i32 = 400;
pub const FW_BOLD: i32 = 700;

pub const DEFAULT_CHARSET: u32 = 1;

/// `MAKEINTRESOURCE(32512)`, `IDC_ARROW` (`go\win32.go:224`). The proc takes a
/// `PCWSTR`, so a caller passes `IDC_ARROW as usize as *const u16`.
pub const IDC_ARROW: u32 = 32512;

pub const BIF_RETURN_ONLY_FS_DIRS: u32 = 0x0001;
pub const BIF_NEW_DIALOG_STYLE: u32 = 0x0040;

/// Go `cwUseDefault = uintptr(0x80000000)` (`win32.go:229`).
///
/// `CW_USEDEFAULT` is `(int)-2147483648`: the bit pattern `0x8000_0000` read as
/// a **signed** 32-bit value. The trap is reproducing it as a `u32` and widening
/// it zero-extended -- `0x0000_0000_8000_0000` is pixel 2147483648, not "let
/// Windows pick the position". Kept as `i32` here, so every widening
/// (`CW_USEDEFAULT as i64`) sign-extends to `0xFFFF_FFFF_8000_0000`, which is
/// what an `INT` parameter receives on x64.
pub const CW_USEDEFAULT: i32 = 0x8000_0000u32 as i32;

// Owner-drawn window messages (`go\win32.go:289-298`).
pub const WM_MOUSE_MOVE: u32 = 0x0200;
pub const WM_L_BUTTON_DOWN: u32 = 0x0201;
pub const WM_L_BUTTON_UP: u32 = 0x0202;
pub const WM_SET_CURSOR: u32 = 0x0020;
pub const WM_ERASE_BKGND: u32 = 0x0014;
pub const WM_SIZE: u32 = 0x0005;
pub const WM_DPI_CHANGED: u32 = 0x02E0;
pub const WM_MOUSE_LEAVE: u32 = 0x02A3;
/// `WM_CANCELMODE`: ends menu tracking (used by the capture harness to dismiss
/// the tray popup after its pixels are read; the app itself never posts it).
pub const WM_CANCELMODE: u32 = 0x001F;
/// `PW_RENDERFULLCONTENT` (`PrintWindow` flag 2): capture the full composed
/// content, required on themed/owner-drawn surfaces (flag 0 can return a blank
/// bitmap while reporting success).
pub const PW_RENDERFULLCONTENT: u32 = 2;
/// `WM_MOUSEWHEEL`. A wheel notch is `WHEEL_DELTA` (120) in `HIWORD(wParam)`;
/// the wheel message carries SCREEN coordinates in `lParam`, not client.
pub const WM_MOUSEWHEEL: u32 = 0x020A;
pub const WHEEL_DELTA: i32 = 120;

/// The signed wheel delta of a `WM_MOUSEWHEEL` message: `HIWORD(wParam)` as an
/// `i16` (negative = wheel toward the user).
#[must_use]
pub fn wheel_delta(wparam: usize) -> i32 {
    let hi = ((wparam >> 16) & 0xFFFF) as u16;
    i32::from(hi as i16)
}

// `DrawTextW` alignment/ellipsis flags for the owner-drawn UI
// (`go\win32.go:301-306`).
pub const DT_CENTER: u32 = 0x0001;
pub const DT_RIGHT: u32 = 0x0002;
pub const DT_V_CENTER: u32 = 0x0004;
pub const DT_END_ELLIPSIS: u32 = 0x8000;
/// `DT_EDITCONTROL` (`0x2000`): duplicate the last line's alignment for a
/// multi-line control. The Fix match empty panel's centered wrap uses it so a
/// two-line hint centers as one block under its title.
pub const DT_EDITCONTROL: u32 = 0x2000;

// Pen, blit, icon, cursor, and metrics constants (`go\win32.go:309-326`).
pub const PS_SOLID: i32 = 0;

pub const SRCCOPY: u32 = 0x00CC0020;

pub const DI_NORMAL: u32 = 0x0003;

/// `MAKEINTRESOURCE(32649)`, `IDC_HAND` (`go\win32.go:316`).
pub const IDC_HAND: u32 = 32649;

pub const SM_CX_SCREEN: i32 = 0;
pub const SM_CY_SCREEN: i32 = 1;

pub const TME_LEAVE: u32 = 0x00000002;

pub const FW_SEMI_BOLD: i32 = 600;

// Font output quality and precision (`CreateFontW` `iQuality`/`iOutPrecision`;
// learn.microsoft.com/windows/win32/api/wingdi/nf-wingdi-createfontw, verified
// 2026-10-14). This app passed `0` (= `DEFAULT_QUALITY`, "appearance does not
// matter") for both, leaving text at the mercy of the system smoothing default;
// `CLEARTYPE_QUALITY` asks for subpixel AA explicitly, and `OUT_TT_PRECIS`
// prefers a TrueType outline face, which is the only kind ClearType can smooth
// (the docs list bitmap/vector/device fonts as unsupported).
pub const CLEARTYPE_QUALITY: u32 = 5;
pub const OUT_TT_PRECIS: u32 = 4;
pub const CLIP_DEFAULT_PRECIS: u32 = 0;

pub const NULL_BRUSH_STOCK: i32 = 5;

// ---------------------------------------------------------------------------
// Structs (`go\win32.go:81-135, :232-265, :328-334`).
// ---------------------------------------------------------------------------

/// `WNDPROC`: the window-procedure signature both windows register.
pub type WndProc = unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT;

/// Go `wndClassEx` (`win32.go:81-94`) -- `WNDCLASSEXW`, 80 bytes on amd64.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct WndClassEx {
    pub cb_size: u32,
    pub style: u32,
    pub lpfn_wnd_proc: usize,
    pub cb_cls_extra: i32,
    pub cb_wnd_extra: i32,
    pub h_instance: usize,
    pub h_icon: usize,
    pub h_cursor: usize,
    pub hbr_background: usize,
    pub lpsz_menu_name: *const u16,
    pub lpsz_class_name: *const u16,
    pub h_icon_sm: usize,
}

/// Go `guid` (`win32.go:96-101`) -- 16 bytes, the same shape as
/// `windows_sys::core::GUID`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Guid {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

/// Go `notifyIconData` (`win32.go:103-120`) -- `NOTIFYICONDATAW`, Vista+ layout,
/// **976 bytes on amd64**.
///
/// The two pads are implicit in `#[repr(C)]` and asserted in the tests: 4 bytes
/// after `cb_size` (so `h_wnd` lands on 8) and 4 bytes after `u_callback_message`
/// (so `h_icon` lands on 32). `guid_item` is 4-aligned, which leaves
/// `h_balloon_icon` at 968 with no extra pad.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct NotifyIconData {
    pub cb_size: u32,
    pub h_wnd: usize,
    pub u_id: u32,
    pub u_flags: u32,
    pub u_callback_message: u32,
    pub h_icon: usize,
    pub sz_tip: [u16; 128],
    pub dw_state: u32,
    pub dw_state_mask: u32,
    pub sz_info: [u16; 256],
    pub u_timeout: u32,
    pub sz_info_title: [u16; 64],
    pub dw_info_flags: u32,
    pub guid_item: Guid,
    pub h_balloon_icon: usize,
}

/// Go `var nid notifyIconData` (`tray.go:102`): zeroed, then filled in.
impl Default for NotifyIconData {
    fn default() -> Self {
        // SAFETY: every field is an integer, an integer array, or a raw pointer,
        // so an all-zero value is valid for the type.
        unsafe { core::mem::zeroed() }
    }
}

/// Go `point` (`win32.go:122-125`) -- `POINT`, 8 bytes.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

/// Go `msgT` (`win32.go:128-135`) -- `MSG`, 48 bytes on amd64.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct MsgT {
    pub hwnd: usize,
    pub message: u32,
    pub w_param: usize,
    pub l_param: usize,
    pub time: u32,
    pub pt: Point,
}

/// Go `rect` (`win32.go:233-238`) -- `RECT`, 16 bytes.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

/// Go `paintStruct` (`win32.go:241-251`) -- `PAINTSTRUCT`, **72 bytes** on amd64
/// (see the module doc: the Go comment's "68" is the field-width sum, not the
/// ABI size).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct PaintStruct {
    pub hdc: usize,
    pub erase: i32,
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
    pub restore: i32,
    pub inc_update: i32,
    pub reserved: [u8; 32],
}

/// Go `browseInfoW` (`win32.go:254-265`) -- `BROWSEINFOW`, 64 bytes on amd64.
///
/// Go names both explicit pads `_`; Rust has no blank field name, so they are
/// `_pad0` / `_pad1` and are never read.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct BrowseInfoW {
    pub hwnd_owner: usize,
    pub pidl_root: usize,
    pub display_name: *const u16,
    pub title: *const u16,
    pub flags: u32,
    pub _pad0: u32,
    pub callback: usize,
    pub l_param: usize,
    pub image: i32,
    pub _pad1: i32,
}

/// Go `trackMouseEvent` (`win32.go:329-334`) -- `TRACKMOUSEEVENT`, 24 bytes on
/// amd64 (4 bytes of tail padding after `dw_hover_time`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct TrackMouseEvent {
    pub cb_size: u32,
    pub dw_flags: u32,
    pub hwnd_track: usize,
    pub dw_hover_time: u32,
}

// ---------------------------------------------------------------------------
// Brand-surface bindings (Rust-only; `go\win32.go` has no counterpart).
//
// The identity pass in `design\make_logo.py` ships the app's own typefaces and
// its own 256x256 Ribbon-P `.ico`. Applying them needs three procs the port
// never declared:
//
//   * `AddFontMemResourceEx` (gdi32) -- install a face from a memory image for
//     the process lifetime, so `design\fonts\*.ttf` can ship inside the binary
//     with no install step and no system-font dependency. The image must stay
//     valid while the font is in use, which the `'static` byte slices from
//     `include_bytes!` guarantee.
//     (learn.microsoft.com/windows/win32/api/wingdi/nf-wingdi-addfontmemresourceex,
//     verified 2026-10-14.)
//   * `GetClassLongPtrW`/`SetClassLongPtrW` (user32) -- read and write the
//     window CLASS's icon slots. `GCLP_HICON` is the proof handle the redesign
//     asserts non-zero; `WM_SETICON` alone would leave the class slot empty.
//     (learn.microsoft.com/windows/win32/api/winuser/nf-winuser-getclasslongptrw,
//     verified 2026-10-14.)
//   * `DestroyIcon` (user32) -- release an `HICON` this process created.
//
// `CreateIconFromResourceEx` (already exported above) is the loader: the
// embedded `.ico` holds one BMP/DIB frame, which is exactly the "resource bits"
// shape it consumes, so no temp file and no `LoadImageW` are needed.
// ---------------------------------------------------------------------------

pub use windows_sys::Win32::Graphics::Gdi::AddFontMemResourceEx;
pub use windows_sys::Win32::UI::WindowsAndMessaging::{
    DestroyIcon, GetClassLongPtrW, SetClassLongPtrW,
};

/// `GetTextFaceW` (gdi32): the face name a DC actually selected. The brand
/// loader reads it back so a missing embedded face is a reported failure, never
/// a silent system fallback -- the same refusal `design\make_logo.py`'s
/// `load_font` makes with its `getname()[0]` assert.
pub use windows_sys::Win32::Graphics::Gdi::GetTextFaceW;

/// `GCLP_HICON` / `GCLP_HICONSM` (`winuser.h`): the class's large and small icon
/// slots, as negative `GetClassLongPtrW` indices.
pub const GCLP_HICON: i32 = -14;
pub const GCLP_HICONSM: i32 = -34;

/// `SM_CXSMICON` / `SM_CYSMICON` (`winuser.h`): the small-icon slot's size.
pub const SM_CXSMICON: i32 = 49;
pub const SM_CYSMICON: i32 = 50;

/// `FW_BLACK` (900) and `FW_MEDIUM` (500): the weights the brand faces ask for.
/// Archivo Black is a single-master 900 display face; Inter Variable exposes
/// named instances, and GDI reaches the default instance through weight alone.
pub const FW_BLACK: i32 = 900;
pub const FW_MEDIUM: i32 = 500;

/// `LR_DEFAULTCOLOR` (0): `CreateIconFromResourceEx`'s only supported flag.
pub const LR_DEFAULTCOLOR: u32 = 0;

/// `SetTextCharacterExtra` (gdi32): GDI's own inter-character spacing, in
/// logical units. The wordmark and the micro-labels are letter-spaced
/// (`design\make_logo.py` tracks `POTFLIX` at 0.08em and manual kerning pairs on
/// top); GDI has no other letter-spacing control, and this one applies to the
/// whole `DrawTextW` call, which is exactly the uniform tracking the brand uses.
/// (learn.microsoft.com/windows/win32/api/wingdi/nf-wingdi-settextcharacterextra,
/// verified 2026-10-14.)
pub use windows_sys::Win32::Graphics::Gdi::SetTextCharacterExtra;

// ---------------------------------------------------------------------------
// Compile-time size assertions: a field added or reordered here fails the
// build, not `Shell_NotifyIconW`/`RegisterClassExW`/`BeginPaint` at runtime.
// ---------------------------------------------------------------------------

#[cfg(target_arch = "x86_64")]
const _: () = assert!(size_of::<WndClassEx>() == 80);
#[cfg(target_arch = "x86_64")]
const _: () = assert!(size_of::<Guid>() == 16);
#[cfg(target_arch = "x86_64")]
const _: () = assert!(size_of::<NotifyIconData>() == 976);
#[cfg(target_arch = "x86_64")]
const _: () = assert!(size_of::<Point>() == 8);
#[cfg(target_arch = "x86_64")]
const _: () = assert!(size_of::<MsgT>() == 48);
#[cfg(target_arch = "x86_64")]
const _: () = assert!(size_of::<Rect>() == 16);
#[cfg(target_arch = "x86_64")]
const _: () = assert!(size_of::<PaintStruct>() == 72);
#[cfg(target_arch = "x86_64")]
const _: () = assert!(size_of::<BrowseInfoW>() == 64);
#[cfg(target_arch = "x86_64")]
const _: () = assert!(size_of::<TrackMouseEvent>() == 24);

// ---------------------------------------------------------------------------
// UTF-16 helpers (Go reaches for `syscall.UTF16PtrFromString` /
// `syscall.StringToUTF16`, which Rust's std does not provide).
// ---------------------------------------------------------------------------

/// Go `syscall.UTF16PtrFromString(s)`: `s` as NUL-terminated UTF-16 units. The
/// returned `Vec` owns the buffer; callers pass `.as_ptr()` and keep it alive
/// across the call (Win32 only reads it during the call).
pub fn utf16_z(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(core::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::{offset_of, size_of};

    use windows_sys::Win32::Graphics::Gdi as gdi;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse as kbm;
    use windows_sys::Win32::UI::Shell as shell;
    use windows_sys::Win32::UI::WindowsAndMessaging as wam;

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn struct_sizes_match_the_go_measurement() {
        // `unsafe.Sizeof` on go1.27.0 amd64, one line per struct.
        assert_eq!(size_of::<WndClassEx>(), 80);
        assert_eq!(size_of::<Guid>(), 16);
        assert_eq!(size_of::<NotifyIconData>(), 976);
        assert_eq!(size_of::<Point>(), 8);
        assert_eq!(size_of::<MsgT>(), 48);
        assert_eq!(size_of::<Rect>(), 16);
        // 68 is the field-width sum; Go's own Sizeof rounds it to the 8-byte
        // alignment, and so does `#[repr(C)]`. `BeginPaint` writes 72 bytes.
        assert_eq!(size_of::<PaintStruct>(), 72);
        assert_eq!(size_of::<BrowseInfoW>(), 64);
        assert_eq!(size_of::<TrackMouseEvent>(), 24);
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn notify_icon_data_offsets_match_the_vista_layout() {
        assert_eq!(offset_of!(NotifyIconData, cb_size), 0);
        // 4-byte pad after cb_size, then the window handle.
        assert_eq!(offset_of!(NotifyIconData, h_wnd), 8);
        assert_eq!(offset_of!(NotifyIconData, u_id), 16);
        assert_eq!(offset_of!(NotifyIconData, u_flags), 20);
        assert_eq!(offset_of!(NotifyIconData, u_callback_message), 24);
        // 4-byte pad after uCallbackMessage, then the icon handle.
        assert_eq!(offset_of!(NotifyIconData, h_icon), 32);
        assert_eq!(offset_of!(NotifyIconData, sz_tip), 40);
        assert_eq!(offset_of!(NotifyIconData, dw_state), 296);
        assert_eq!(offset_of!(NotifyIconData, dw_state_mask), 300);
        assert_eq!(offset_of!(NotifyIconData, sz_info), 304);
        assert_eq!(offset_of!(NotifyIconData, u_timeout), 816);
        assert_eq!(offset_of!(NotifyIconData, sz_info_title), 820);
        assert_eq!(offset_of!(NotifyIconData, dw_info_flags), 948);
        assert_eq!(offset_of!(NotifyIconData, guid_item), 952);
        assert_eq!(offset_of!(NotifyIconData, h_balloon_icon), 968);
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn wnd_class_ex_offsets_match_the_go_field_order() {
        assert_eq!(offset_of!(WndClassEx, cb_size), 0);
        assert_eq!(offset_of!(WndClassEx, style), 4);
        assert_eq!(offset_of!(WndClassEx, lpfn_wnd_proc), 8);
        assert_eq!(offset_of!(WndClassEx, cb_cls_extra), 16);
        assert_eq!(offset_of!(WndClassEx, cb_wnd_extra), 20);
        assert_eq!(offset_of!(WndClassEx, h_instance), 24);
        assert_eq!(offset_of!(WndClassEx, h_icon), 32);
        assert_eq!(offset_of!(WndClassEx, h_cursor), 40);
        assert_eq!(offset_of!(WndClassEx, hbr_background), 48);
        assert_eq!(offset_of!(WndClassEx, lpsz_menu_name), 56);
        assert_eq!(offset_of!(WndClassEx, lpsz_class_name), 64);
        assert_eq!(offset_of!(WndClassEx, h_icon_sm), 72);
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn paint_struct_offsets_match_the_go_field_order() {
        assert_eq!(offset_of!(PaintStruct, hdc), 0);
        assert_eq!(offset_of!(PaintStruct, erase), 8);
        assert_eq!(offset_of!(PaintStruct, left), 12);
        assert_eq!(offset_of!(PaintStruct, top), 16);
        assert_eq!(offset_of!(PaintStruct, right), 20);
        assert_eq!(offset_of!(PaintStruct, bottom), 24);
        assert_eq!(offset_of!(PaintStruct, restore), 28);
        assert_eq!(offset_of!(PaintStruct, inc_update), 32);
        assert_eq!(offset_of!(PaintStruct, reserved), 36);
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn msg_and_tracking_offsets_match_the_go_field_order() {
        assert_eq!(offset_of!(MsgT, hwnd), 0);
        assert_eq!(offset_of!(MsgT, message), 8);
        assert_eq!(offset_of!(MsgT, w_param), 16);
        assert_eq!(offset_of!(MsgT, l_param), 24);
        assert_eq!(offset_of!(MsgT, time), 32);
        assert_eq!(offset_of!(MsgT, pt), 36);
        assert_eq!(offset_of!(TrackMouseEvent, cb_size), 0);
        assert_eq!(offset_of!(TrackMouseEvent, dw_flags), 4);
        assert_eq!(offset_of!(TrackMouseEvent, hwnd_track), 8);
        assert_eq!(offset_of!(TrackMouseEvent, dw_hover_time), 16);
        assert_eq!(offset_of!(BrowseInfoW, flags), 32);
        assert_eq!(offset_of!(BrowseInfoW, _pad0), 36);
        assert_eq!(offset_of!(BrowseInfoW, callback), 40);
        assert_eq!(offset_of!(BrowseInfoW, l_param), 48);
        assert_eq!(offset_of!(BrowseInfoW, image), 56);
        assert_eq!(offset_of!(BrowseInfoW, _pad1), 60);
    }

    /// The mirrors are cast to the `windows-sys` structs at the call sites, so
    /// they must be the same size as the bindings' own types. This is what makes
    /// those casts sound.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn mirrors_are_the_same_size_as_the_windows_sys_structs() {
        assert_eq!(size_of::<WndClassEx>(), size_of::<wam::WNDCLASSEXW>());
        assert_eq!(
            size_of::<NotifyIconData>(),
            size_of::<shell::NOTIFYICONDATAW>()
        );
        assert_eq!(size_of::<MsgT>(), size_of::<wam::MSG>());
        assert_eq!(
            size_of::<Point>(),
            size_of::<windows_sys::Win32::Foundation::POINT>()
        );
        assert_eq!(
            size_of::<Rect>(),
            size_of::<windows_sys::Win32::Foundation::RECT>()
        );
        assert_eq!(size_of::<PaintStruct>(), size_of::<gdi::PAINTSTRUCT>());
        assert_eq!(
            size_of::<TrackMouseEvent>(),
            size_of::<kbm::TRACKMOUSEEVENT>()
        );
        assert_eq!(size_of::<BrowseInfoW>(), size_of::<shell::BROWSEINFOW>());
        assert_eq!(size_of::<Guid>(), size_of::<windows_sys::core::GUID>());
    }

    #[test]
    fn cw_use_default_sign_extends() {
        assert_eq!(CW_USEDEFAULT, i32::MIN);
        assert_eq!(CW_USEDEFAULT as u32, 0x8000_0000);
        // Widening sign-extends -- what an INT parameter receives on x64.
        assert_eq!(CW_USEDEFAULT as i64, -2147483648);
        assert_eq!(CW_USEDEFAULT as i64 as u64, 0xFFFF_FFFF_8000_0000);
        // The trap: the zero-extended form is a different value (pixel
        // 2147483648, not "let Windows choose").
        assert_ne!(CW_USEDEFAULT as i64 as u64, 0x0000_0000_8000_0000);
        assert_ne!(
            (CW_USEDEFAULT as u32) as i64 as u64,
            CW_USEDEFAULT as i64 as u64
        );
        assert_eq!(CW_USEDEFAULT, wam::CW_USEDEFAULT);
    }

    #[test]
    fn complement_constants_are_the_negative_dwords() {
        // Go: ^uintptr(0), ^uintptr(10), ^uintptr(11) -- the low 32 bits are
        // (DWORD)-1, (DWORD)-11, (DWORD)-12.
        assert_eq!(ATTACH_PARENT_PROCESS, !0u32);
        assert_eq!(STD_OUTPUT_HANDLE, !10u32);
        assert_eq!(STD_ERROR_HANDLE, !11u32);
        assert_eq!(ATTACH_PARENT_PROCESS, 0xFFFF_FFFF);
        assert_eq!(STD_OUTPUT_HANDLE, 0xFFFF_FFF5);
        assert_eq!(STD_ERROR_HANDLE, 0xFFFF_FFF4);
        assert_eq!(STD_OUTPUT_HANDLE, -11i32 as u32);
        assert_eq!(STD_ERROR_HANDLE, -12i32 as u32);
    }

    #[test]
    fn message_ids_are_the_go_values() {
        assert_eq!(WM_APP, 0x8000);
        assert_eq!(WM_APP_TRAY, 0x8001);
        assert_eq!(WM_APP_QUIT, 0x8002);
        assert_eq!(WM_NULL, 0x0000);
        assert_eq!(WM_DESTROY, 0x0002);
        assert_eq!(WM_CLOSE, 0x0010);
        assert_eq!(WM_RBUTTON_UP, 0x0205);
        assert_eq!(WM_CONTEXT_MENU, 0x007B);
        assert_eq!(WM_MOUSE_MOVE, 0x0200);
        assert_eq!(WM_MOUSE_LEAVE, 0x02A3);
        assert_eq!(WM_DPI_CHANGED, 0x02E0);
        assert_eq!(WM_ERASE_BKGND, 0x0014);
        assert_eq!(WM_SIZE, 0x0005);
        assert_eq!(WM_SET_CURSOR, 0x0020);
        assert_eq!(WM_SET_ICON, 0x0080);
        assert_eq!(WM_PAINT, 0x000F);
        assert_eq!(WM_COMMAND, 0x0111);
    }

    /// The declared values must be the values the bindings themselves use, or a
    /// call goes out with the wrong flag.
    #[test]
    fn flag_and_style_constants_agree_with_windows_sys() {
        assert_eq!(NIM_ADD, shell::NIM_ADD);
        assert_eq!(NIM_DELETE, shell::NIM_DELETE);
        assert_eq!(NIF_MESSAGE, shell::NIF_MESSAGE);
        assert_eq!(NIF_ICON, shell::NIF_ICON);
        assert_eq!(NIF_TIP, shell::NIF_TIP);
        assert_eq!(BIF_RETURN_ONLY_FS_DIRS, shell::BIF_RETURNONLYFSDIRS);
        assert_eq!(BIF_NEW_DIALOG_STYLE, shell::BIF_NEWDIALOGSTYLE);

        assert_eq!(MF_STRING, wam::MF_STRING);
        assert_eq!(TPM_RIGHT_BUTTON, wam::TPM_RIGHTBUTTON);
        assert_eq!(TPM_BOTTOM_ALIGN, wam::TPM_BOTTOMALIGN);
        assert_eq!(TPM_RETURN_CMD, wam::TPM_RETURNCMD);
        assert_eq!(ICON_SMALL, wam::ICON_SMALL);
        assert_eq!(ICON_BIG, wam::ICON_BIG);
        assert_eq!(SW_HIDE, wam::SW_HIDE);
        assert_eq!(SW_SHOW, wam::SW_SHOW);
        assert_eq!(LBS_NOTIFY as i32, wam::LBS_NOTIFY);
        assert_eq!(BN_CLICKED, wam::BN_CLICKED);
        assert_eq!(WS_CAPTION, wam::WS_CAPTION);
        assert_eq!(WS_SYS_MENU, wam::WS_SYSMENU);
        assert_eq!(WS_MINIMIZE_BOX, wam::WS_MINIMIZEBOX);
        assert_eq!(WS_CHILD, wam::WS_CHILD);
        assert_eq!(WS_VISIBLE, wam::WS_VISIBLE);
        assert_eq!(WS_BORDER, wam::WS_BORDER);
        assert_eq!(WS_V_SCROLL, wam::WS_VSCROLL);
        assert_eq!(WS_TAB_STOP, wam::WS_TABSTOP);
        // Resource ids are passed as PCWSTR, so compare the integer value.
        assert_eq!(IDC_ARROW as usize, wam::IDC_ARROW as usize);
        assert_eq!(IDC_HAND as usize, wam::IDC_HAND as usize);
        assert_eq!(WM_NULL, wam::WM_NULL);
        assert_eq!(WM_DESTROY, wam::WM_DESTROY);
        assert_eq!(WM_CLOSE, wam::WM_CLOSE);
        assert_eq!(WM_RBUTTON_UP, wam::WM_RBUTTONUP);
        assert_eq!(WM_CONTEXT_MENU, wam::WM_CONTEXTMENU);
        assert_eq!(WM_APP, wam::WM_APP);
        assert_eq!(WM_PAINT, wam::WM_PAINT);
        assert_eq!(WM_PRINT, wam::WM_PRINT);
        assert_eq!(WM_PRINTCLIENT, wam::WM_PRINTCLIENT);
        assert_eq!(WM_COMMAND, wam::WM_COMMAND);
        assert_eq!(WM_SET_ICON, wam::WM_SETICON);
        assert_eq!(WM_MOUSE_MOVE, wam::WM_MOUSEMOVE);
        assert_eq!(WM_L_BUTTON_DOWN, wam::WM_LBUTTONDOWN);
        assert_eq!(WM_L_BUTTON_UP, wam::WM_LBUTTONUP);
        assert_eq!(WM_SET_CURSOR, wam::WM_SETCURSOR);
        // Brand-surface bindings: the class-icon slots, the small-icon metric,
        // and the two brand font weights, each against windows-sys.
        assert_eq!(GCLP_HICON, wam::GCLP_HICON);
        assert_eq!(GCLP_HICONSM, wam::GCLP_HICONSM);
        assert_eq!(SM_CXSMICON, wam::SM_CXSMICON);
        assert_eq!(SM_CYSMICON, wam::SM_CYSMICON);
        assert_eq!(FW_BLACK, gdi::FW_BLACK as i32);
        assert_eq!(FW_MEDIUM, gdi::FW_MEDIUM as i32);
        assert_eq!(LR_DEFAULTCOLOR, 0);
        assert_eq!(WM_ERASE_BKGND, wam::WM_ERASEBKGND);
        assert_eq!(WM_SIZE, wam::WM_SIZE);
        assert_eq!(WM_DPI_CHANGED, wam::WM_DPICHANGED);
        assert_eq!(LB_ADD_STRING, wam::LB_ADDSTRING);
        assert_eq!(LB_DELETE_STRING, wam::LB_DELETESTRING);
        assert_eq!(LB_RESET_CONTENT, wam::LB_RESETCONTENT);
        assert_eq!(LB_GET_CUR_SEL, wam::LB_GETCURSEL);
        assert_eq!(LB_GET_TEXT, wam::LB_GETTEXT);

        assert_eq!(DT_LEFT, gdi::DT_LEFT);
        assert_eq!(DT_CENTER, gdi::DT_CENTER);
        assert_eq!(DT_RIGHT, gdi::DT_RIGHT);
        assert_eq!(DT_V_CENTER, gdi::DT_VCENTER);
        assert_eq!(DT_SINGLE_LINE, gdi::DT_SINGLELINE);
        assert_eq!(DT_NO_PREFIX, gdi::DT_NOPREFIX);
        assert_eq!(DT_END_ELLIPSIS, gdi::DT_END_ELLIPSIS);
        assert_eq!(DT_EDITCONTROL, gdi::DT_EDITCONTROL);
        assert_eq!(PS_SOLID, gdi::PS_SOLID);
        assert_eq!(SRCCOPY, gdi::SRCCOPY);
        assert_eq!(DI_NORMAL, wam::DI_NORMAL);
        assert_eq!(NULL_BRUSH_STOCK, gdi::NULL_BRUSH);
        assert_eq!(DEFAULT_CHARSET as u8, gdi::DEFAULT_CHARSET);
        assert_eq!(TRANSPARENT as u32, gdi::TRANSPARENT);
        assert_eq!(FW_NORMAL as u32, gdi::FW_NORMAL);
        assert_eq!(FW_BOLD as u32, gdi::FW_BOLD);
        assert_eq!(FW_SEMI_BOLD as u32, gdi::FW_SEMIBOLD);
        assert_eq!(SM_CX_SCREEN, wam::SM_CXSCREEN);
        assert_eq!(SM_CY_SCREEN, wam::SM_CYSCREEN);
        assert_eq!(TME_LEAVE, kbm::TME_LEAVE);
        assert_eq!(ICON_RESOURCE_VERSION, 0x0003_0000);
        assert_eq!(GENERIC_WRITE, windows_sys::Win32::Foundation::GENERIC_WRITE);
        assert_eq!(FILE_SHARE_READ_WRITE, 3);
        assert_eq!(
            OPEN_EXISTING,
            windows_sys::Win32::Storage::FileSystem::OPEN_EXISTING
        );
        // Fix match owner-draw values: the dialog's buttons, list and statics.
        assert_eq!(ODS_SELECTED, windows_sys::Win32::UI::Controls::ODS_SELECTED);
        assert_eq!(ODS_DISABLED, windows_sys::Win32::UI::Controls::ODS_DISABLED);
        assert_eq!(ODS_HOTLIGHT, windows_sys::Win32::UI::Controls::ODS_HOTLIGHT);
        assert_eq!(ODS_FOCUS, windows_sys::Win32::UI::Controls::ODS_FOCUS);
        assert_eq!(ODT_MENU, windows_sys::Win32::UI::Controls::ODT_MENU);
        assert_eq!(ODT_LISTBOX, windows_sys::Win32::UI::Controls::ODT_LISTBOX);
        assert_eq!(ODT_BUTTON, windows_sys::Win32::UI::Controls::ODT_BUTTON);
        assert_eq!(ODT_STATIC, windows_sys::Win32::UI::Controls::ODT_STATIC);
        assert_eq!(LBS_OWNERDRAWFIXED as i32, wam::LBS_OWNERDRAWFIXED);
        assert_eq!(LBS_HASSTRINGS as i32, wam::LBS_HASSTRINGS);
        assert_eq!(BS_OWNERDRAW as i32, wam::BS_OWNERDRAW);
        // `SS_OWNERDRAW` lives behind the `Win32_System_SystemServices`
        // feature, which this crate does not enable; pin the stable value.
        assert_eq!(SS_OWNERDRAW, 0x000D);
        assert_eq!(EN_CHANGE, wam::EN_CHANGE);
        assert_eq!(EN_SETFOCUS, wam::EN_SETFOCUS);
        assert_eq!(EN_KILLFOCUS, wam::EN_KILLFOCUS);
        assert_eq!(WM_DRAWITEM, wam::WM_DRAWITEM);
        assert_eq!(WM_MEASUREITEM, wam::WM_MEASUREITEM);
    }

    #[test]
    fn utf16_z_is_nul_terminated() {
        assert_eq!(utf16_z(""), [0]);
        assert_eq!(utf16_z("Potflix"), [80, 111, 116, 102, 108, 105, 120, 0]);
        assert_eq!(utf16_z("PotflixTrayWnd").len(), 14 + 1);
    }
}
