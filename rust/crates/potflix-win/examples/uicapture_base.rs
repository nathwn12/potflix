
use std::ffi::c_void;

use potflix_win::tray;
use potflix_win::win32;
use potflix_win::window;
use windows_sys::Win32::Foundation::{HWND, RECT};
use windows_sys::Win32::Graphics::Gdi::{DeleteDC, DeleteObject, SelectObject};
use windows_sys::Win32::Storage::Xps::PrintWindow;
use windows_sys::Win32::System::StationsAndDesktops::{
    CloseDesktop, CreateDesktopW, GetThreadDesktop, SetThreadDesktop,
};
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, FindWindowW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE,
};

fn utf16(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Pump this thread's queue for `ms` milliseconds.
fn pump(ms: u64) {
    let end = std::time::Instant::now() + std::time::Duration::from_millis(ms);
    while std::time::Instant::now() < end {
        unsafe {
            let mut msg: MSG = std::mem::zeroed();
            while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Create a private desktop and move this thread onto it. Returns
/// `(new, previous)` as address-sized values (raw handles are not `Send`);
/// the caller restores `previous` and closes `new`.
fn enter_hidden_desktop(tag: &str) -> Result<(usize, usize), String> {
    let name = utf16(&format!("potflix-cap-{tag}"));
    let prev = unsafe { GetThreadDesktop(GetCurrentThreadId()) } as isize;
    // SAFETY: name is a live null-terminated desktop name; no security attrs.
    // `GENERIC_ALL` (0x10000000) like the proven runner script.
    let desk = unsafe {
        CreateDesktopW(
            name.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            0x1000_0000,
            std::ptr::null(),
        )
    };
    if desk.is_null() {
        return Err("CreateDesktopW failed".into());
    }
    // SAFETY: this thread owns no windows yet, so the switch is legal.
    if unsafe { SetThreadDesktop(desk) } == 0 {
        unsafe { CloseDesktop(desk) };
        return Err("SetThreadDesktop failed (thread owns a window?)".into());
    }
    Ok((desk as usize, prev as usize))
}

/// Leave the private desktop and destroy it.
fn leave_hidden_desktop(desk: usize, prev: usize) {
    unsafe {
        SetThreadDesktop(prev as *mut c_void);
        CloseDesktop(desk as *mut c_void);
    }
}

fn view_of(s: &str) -> Result<window::View, String> {
    match s {
        "main" => Ok(window::View::Main),
        "settings" => Ok(window::View::Settings),
        "card" => Ok(window::View::Card),
        _ => Err(format!("unknown view {s:?}; want main|settings|card")),
    }
}

/// Mean/min/max/uniform of a BGRA buffer; `uniform == true` fails the proof.
fn measure(px: &[u8]) -> (u8, u8, f64, bool) {
    let mut min = 255u8;
    let mut max = 0u8;
    let mut sum = 0u64;
    let mut first = None;
    let mut uniform = true;
    for c in px {
        min = min.min(*c);
        max = max.max(*c);
        sum += *c as u64;
        match first {
            None => first = Some(*c),
            Some(f) => {
                if *c != f {
                    uniform = false;
                }
            }
        }
    }
    let mean = sum as f64 / px.len().max(1) as f64;
    (min, max, mean, uniform)
}

/// Create a 32bpp top-down DIB section; returns `(dc, bitmap, bits_ptr)`.
/// The caller selects, paints, then deletes. Unlike `CreateCompatibleBitmap`
/// (a DDB with no CPU-visible bits), a DIB section's pixels are readable.
fn create_dib(w: i32, h: i32) -> Result<(isize, isize, *const u8), String> {
    use windows_sys::Win32::Graphics::Gdi::{
        CreateCompatibleDC, CreateDIBSection, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
    };
    // SAFETY: DC/bitmap/DIB created here, released by the caller.
    unsafe {
        let screen = windows_sys::Win32::Graphics::Gdi::GetDC(std::ptr::null_mut());
        let mem = CreateCompatibleDC(screen);
        windows_sys::Win32::Graphics::Gdi::ReleaseDC(std::ptr::null_mut(), screen);
        if mem.is_null() {
            return Err("capture DC failed".into());
        }
        let mut info: BITMAPINFO = std::mem::zeroed();
        info.bmiHeader.biSize = core::mem::size_of::<BITMAPINFOHEADER>() as u32;
        info.bmiHeader.biWidth = w;
        info.bmiHeader.biHeight = -h; // top-down: row 0 is the top.
        info.bmiHeader.biPlanes = 1;
        info.bmiHeader.biBitCount = 32;
        info.bmiHeader.biCompression = BI_RGB;
        let mut bits: *mut c_void = std::ptr::null_mut();
        let bmp = CreateDIBSection(
            mem,
            &info,
            DIB_RGB_COLORS,
            &mut bits,
            std::ptr::null_mut(),
            0,
        );
        if bmp.is_null() || bits.is_null() {
            DeleteDC(mem);
            return Err("CreateDIBSection failed".into());
        }
        Ok((mem as isize, bmp as isize, bits as *const u8))
    }
}

/// Save an `HBITMAP` as PNG through the GDI+ image encoder (no new crate).
fn save_png(hbm: isize, path: &str) -> Result<(), String> {
    use windows_sys::Win32::Graphics::GdiPlus::{
        GdipCreateBitmapFromHBITMAP, GdipDisposeImage, GdipSaveImageToFile, GdiplusShutdown,
        GdiplusStartup, GdiplusStartupInput, GpBitmap, GpImage,
    };
    let clsid = windows_sys::core::GUID {
        data1: 0x557C_F406,
        data2: 0x1A04,
        data3: 0x11D3,
        data4: [0x9A, 0x73, 0x00, 0x00, 0xF8, 0x1E, 0xF3, 0x2E],
    };
    // SAFETY: flat GDI+ calls with live handles; everything released below.
    unsafe {
        let mut token: usize = 0;
        let input = GdiplusStartupInput {
            GdiplusVersion: 1,
            ..Default::default()
        };
        if GdiplusStartup(&mut token, &input, std::ptr::null_mut()) != 0 {
            return Err("GdiplusStartup failed".into());
        }
        let mut img: *mut GpBitmap = std::ptr::null_mut();
        let st = GdipCreateBitmapFromHBITMAP(hbm as *mut c_void, std::ptr::null_mut(), &mut img);
        if st != 0 || img.is_null() {
            GdiplusShutdown(token);
            return Err(format!("GdipCreateBitmapFromHBITMAP status={st}"));
        }
        let wpath = utf16(path);
        let st = GdipSaveImageToFile(
            img as *mut GpImage,
            wpath.as_ptr(),
            &clsid,
            std::ptr::null(),
        );
        GdipDisposeImage(img as *mut GpImage);
        GdiplusShutdown(token);
        if st != 0 {
            return Err(format!("GdipSaveImageToFile status={st}"));
        }
    }
    Ok(())
}

/// Capture `hwnd`'s client area with `PrintWindow` (flag 2, then flag 0) into a
/// fresh bitmap; returns `(pixels, w, h, hbitmap)` for the first non-blank
/// capture, else an error.
fn print_window(hwnd: HWND, w: i32, h: i32) -> Result<(Vec<u8>, isize), String> {
    // SAFETY: DCs/bitmaps created and selected within this call.
    unsafe {
        let (mem, bmp, bits) = create_dib(w, h)?;
        let mem = mem as *mut c_void;
        let old = SelectObject(mem, bmp as *mut c_void);
        for flag in [2 /* PW_RENDERFULLCONTENT */, 0] {
            PrintWindow(hwnd, mem, flag);
            windows_sys::Win32::Graphics::Gdi::GdiFlush();
            let px = std::slice::from_raw_parts(bits, (w as usize) * (h as usize) * 4).to_vec();
            let (_, _, _, uniform) = measure(&px);
            if !uniform {
                SelectObject(mem, old);
                DeleteDC(mem);
                return Ok((px, bmp));
            }
        }
        SelectObject(mem, old);
        DeleteObject(bmp as *mut c_void);
        DeleteDC(mem);
        Err("PrintWindow returned a blank bitmap on flags 2 and 0".into())
    }
}

fn report_and_save(
    tag: &str,
    px: &[u8],
    hbm: isize,
    path: &str,
    extra: &str,
) -> Result<(), String> {
    let (min, max, mean, uniform) = measure(px);
    println!("MEASURE {tag}: min={min} max={max} mean={mean:.1} uniform={uniform} {extra}");
    if uniform {
        return Err(format!("{tag}: blank capture"));
    }
    save_png(hbm, path)?;
    println!("SAVED {path}");
    Ok(())
}

fn client_size(hwnd: HWND) -> (i32, i32) {
    let mut rc = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    // SAFETY: `rc` is a live RECT.
    unsafe { windows_sys::Win32::UI::WindowsAndMessaging::GetClientRect(hwnd, &mut rc) };
    (rc.right - rc.left, rc.bottom - rc.top)
}

fn mode_window(args: &[String]) -> Result<(), String> {
    let out = args.get(2).cloned().ok_or("window <out.png> ...")?;
    let view = view_of(&args.get(3).cloned().unwrap_or("main".into()))?;
    let (desk, prev) = enter_hidden_desktop("wnd")?;
    let r = (|| {
        window::create_main_window().map_err(|e| format!("create_main_window: {e}"))?;
        {
            let mut st = window::ui();
            st.cur_view = view;
            st.focus_id = 0;
            st.refresh_dir_list();
        }
        window::show_main_window();
        pump(1200);
        let hwnd = window::ui().main_hwnd as HWND;
        if hwnd.is_null() {
            return Err("no main window".into());
        }
        unsafe { win32::UpdateWindow(hwnd) };
        pump(400);
        let (w, h) = client_size(hwnd);
        let dpi = window::window_dpi(hwnd);
        let (px, hbm) = print_window(hwnd, w, h)?;
        let extra = format!("dpi={dpi} client={w}x{h}");
        report_and_save("window", &px, hbm, &out, &extra)?;
        // SAFETY: bitmap/DC released after the save.
        unsafe {
            DeleteObject(hbm as *mut c_void);
        }
        Ok(())
    })();
    leave_hidden_desktop(desk, prev);
    r
}

fn mode_menu(args: &[String]) -> Result<(), String> {
    let out = args.get(2).cloned().ok_or("menu <out.png>")?;
    let (desk, prev) = enter_hidden_desktop("menu")?;
    let r = (|| {
        let tw = tray::create_tray_window().map_err(|e| format!("tray window: {e}"))?;
        let owner = tw.hwnd as usize;
        let out2 = out.clone();
        let watcher = std::thread::spawn(move || -> Result<(), String> {
            // SAFETY: this thread owns no windows, so the switch is legal.
            if unsafe { SetThreadDesktop(desk as *mut c_void) } == 0 {
                return Err("watcher SetThreadDesktop failed".into());
            }
            let cls = utf16("#32768");
            for _ in 0..100 {
                // SAFETY: class string is live.
                let menu_hwnd = unsafe { FindWindowW(cls.as_ptr(), std::ptr::null()) };
                if !menu_hwnd.is_null() {
                    std::thread::sleep(std::time::Duration::from_millis(400));
                    let (w, h) = client_size(menu_hwnd);
                    let (w, h) = (w.max(1), h.max(1));
                    match print_window(menu_hwnd, w, h) {
                        Ok((px, hbm)) => {
                            let res = report_and_save("menu", &px, hbm, &out2, "");
                            // SAFETY: bitmap released after the save.
                            unsafe { DeleteObject(hbm as *mut c_void) };
                            res?;
                        }
                        Err(e) => return Err(e),
                    }
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            // SAFETY: cross-desktop post to our own window; ends tracking.
            unsafe { win32::PostMessageW(owner as HWND, 0x001F /* WM_CANCELMODE */, 0, 0) };
            Ok(())
        });
        // SAFETY: `owner` is our tray window on this thread; the modal
        // `TrackPopupMenu` loop dispatches our own messages until the watcher
        // cancels tracking (selection 0 = cancel = no dispatch).
        unsafe { tray::show_tray_menu(owner as HWND) };
        watcher.join().map_err(|_| "watcher panicked".to_string())?
    })();
    leave_hidden_desktop(desk, prev);
    r
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).cloned().unwrap_or_default();
    let r = match mode.as_str() {
        "window" => mode_window(&args),
        "menu" => mode_menu(&args),
        _ => Err("usage: uicapture_base (window|menu) ...".into()),
    };
    if let Err(e) = r {
        eprintln!("UICAPTURE-FAIL: {e}");
        std::process::exit(1);
    }
    println!("UICAPTURE-PASS");
}
