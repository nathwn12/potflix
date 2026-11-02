//! `snapshot` -- hidden-desktop UI previews produced by the real app window.
//!
//! One implementation for the product (`potflix.exe --snapshot`) and the
//! `uicapture snapshot` example mode: private desktop via `CreateDesktopW` +
//! `SetThreadDesktop`, `create_main_window` plus a foreground-free show,
//! message pump only, `PrintWindow` client capture to a bitmap, PNG out via
//! the GDI+ image encoder.
//!
//! Never touches the visible desktop: no input injection, shown only on the
//! private desktop. Exit contract lives with the callers: `Ok` means a
//! non-blank PNG was written, `Err` carries the plain reason.

use std::ffi::c_void;

use windows_sys::Win32::Foundation::{HWND, RECT};
use windows_sys::Win32::Graphics::Gdi::{DeleteDC, DeleteObject, SelectObject};
use windows_sys::Win32::Storage::Xps::PrintWindow;
use windows_sys::Win32::System::StationsAndDesktops::{
    CloseDesktop, CreateDesktopW, GetThreadDesktop, SetThreadDesktop,
};
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE,
};

use crate::window::{self, View};

fn utf16(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Seed a deterministic gradient cover for one fixture URL so previews show
/// decoded pixels without any network fetch.
fn seed_cover(url: &str) -> Result<(), String> {
    const W: i32 = 240;
    const H: i32 = 360;
    let stride = (W * 3 + 3) & !3;
    let pixel_bytes = (stride * H) as usize;
    let mut out = Vec::with_capacity(54 + pixel_bytes);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&((54 + pixel_bytes) as u32).to_le_bytes());
    out.extend_from_slice(&[0, 0, 0, 0]);
    out.extend_from_slice(&54u32.to_le_bytes());
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&W.to_le_bytes());
    out.extend_from_slice(&H.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&24u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&(pixel_bytes as u32).to_le_bytes());
    out.extend_from_slice(&2835u32.to_le_bytes());
    out.extend_from_slice(&2835u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    for y in 0..H {
        for x in 0..W {
            let r = (24 + x * 190 / W) as u8;
            let g = (32 + y * 150 / H) as u8;
            let b = (90 + (x + y) * 120 / (W + H)) as u8;
            out.extend_from_slice(&[b, g, r]);
        }
        out.extend(std::iter::repeat_n(0, (stride - W * 3) as usize));
    }
    crate::artwork::put_cached(url, &out)
        .map(|_| ())
        .map_err(|e| format!("seed cover: {e}"))
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
/// `(new, previous)` as address-sized values; the caller restores `previous`
/// and closes `new`.
fn enter_hidden_desktop(tag: &str) -> Result<(usize, usize), String> {
    let name = utf16(&format!("potflix-snap-{tag}"));
    let prev = unsafe { GetThreadDesktop(GetCurrentThreadId()) } as usize;
    // SAFETY: name is a live null-terminated desktop name; no security attrs.
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
    Ok((desk as usize, prev))
}

/// Leave the private desktop and destroy it.
fn leave_hidden_desktop(desk: usize, prev: usize) {
    unsafe {
        SetThreadDesktop(prev as *mut c_void);
        CloseDesktop(desk as *mut c_void);
    }
}

/// Append one line to the product debug log beside the executable, even when
/// the process logger was never installed (the `--snapshot` entry runs before
/// it) and even when stdout goes nowhere. Never fails the capture: logging
/// must not mask the real reason.
fn log_fail(frame: &str, msg: &str) {
    let line = format!("snapshot {frame} fail: {msg}");
    crate::window::app_logf(&line);
    let path = app_support::debuglog::default_log_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        use std::io::Write as _;
        let _ = writeln!(f, "{line}");
    }
    eprintln!("SNAPSHOT-FAIL: {line}");
}

/// RAII hidden-desktop lease: restoring the previous desktop and closing the
/// private one happens on drop, so an early return -- or an unwind -- can
/// never leak the desktop or strand the thread on it.
struct DesktopGuard {
    desk: usize,
    prev: usize,
    live: bool,
}

impl DesktopGuard {
    fn enter(tag: &str) -> Result<Self, String> {
        let (desk, prev) = enter_hidden_desktop(tag)?;
        Ok(Self {
            desk,
            prev,
            live: true,
        })
    }
}

impl Drop for DesktopGuard {
    fn drop(&mut self) {
        if self.live {
            leave_hidden_desktop(self.desk, self.prev);
            self.live = false;
        }
    }
}

/// A realistic play anchor: twelve minutes into the runtime.
fn preview_start_ms() -> i64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(1_700_000_000_000);
    now - 12 * 60_000
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
    // PNG encoder CLSID {557CF406-1A04-11D3-9A73-0000F81EF32E}.
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

/// Capture `hwnd`'s CLIENT area with `PrintWindow` (client-only first, then
/// full-content fallbacks). Returns pixels plus the bitmap for the first
/// non-blank capture.
fn print_client(hwnd: HWND, w: i32, h: i32) -> Result<(Vec<u8>, isize), String> {
    // SAFETY: DCs/bitmaps created and selected within this call.
    unsafe {
        let (mem, bmp, bits) = create_dib(w, h)?;
        let mem = mem as *mut c_void;
        let old = SelectObject(mem, bmp as *mut c_void);
        for flag in [1u32, crate::win32::PW_RENDERFULLCONTENT, 0] {
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
        Err("PrintWindow returned a blank client bitmap (flags 1, 2, 0)".into())
    }
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

/// Show the hidden-desktop window without ever foregrounding: `ShowWindow` +
/// `UpdateWindow` only. The window lives on the private desktop, so it is
/// never visible to the user.
fn show_hidden_window() {
    let (hwnd, needs_center) = {
        let mut st = window::ui();
        if st.main_hwnd == 0 {
            return;
        }
        let hwnd = st.main_hwnd as HWND;
        let needs_center = !st.ui_centered;
        if needs_center {
            st.ui_centered = true;
        }
        (hwnd, needs_center)
    };
    if needs_center {
        window::center_window(hwnd);
    }
    // SAFETY: ShowWindow/UpdateWindow only affect `hwnd` on this hidden desktop.
    unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::ShowWindow(
            hwnd,
            crate::win32::SW_SHOW,
        );
        windows_sys::Win32::Graphics::Gdi::UpdateWindow(hwnd);
    }
}

/// Resize `hwnd` so its CLIENT area becomes `w` x `h`.
fn resize_to_client(hwnd: HWND, w: i32, h: i32) {
    let mut wr: RECT = unsafe { std::mem::zeroed() };
    unsafe { windows_sys::Win32::UI::WindowsAndMessaging::GetWindowRect(hwnd, &mut wr) };
    let (cw0, ch0) = client_size(hwnd);
    let dw = (wr.right - wr.left) - cw0;
    let dh = (wr.bottom - wr.top) - ch0;
    // SAFETY: `hwnd` is our window on this thread.
    unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::MoveWindow(
            hwnd,
            wr.left,
            wr.top,
            w + dw,
            h + dh,
            1,
        );
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

fn populated_movie() -> (
    String,
    presence::Resolved,
    presence::Action,
    app_support::config::CardKind,
) {
    let long_syn = "A retired operative living quietly is pulled back when a ruthless syndicate burns his home. He crosses continents, gathers old allies, and faces a final choice that will decide everything he loves and more.";
    let res = presence::Resolved {
        series: "Dune: Part Two".to_string(),
        year: 2024,
        is_movie: true,
        genres: "Sci-Fi, Adventure".to_string(),
        length: "2h 46m".to_string(),
        synopsis: long_syn.to_string(),
        artwork: "https://example.com/dune-poster.jpg".to_string(),
        // A Film never carries an episode marker: zero season/number/absolute
        // so the hero derives `2024 · Film · 2h 46m`, never `S02E01`.
        season: 0,
        number: 0,
        absolute: 0,
        ..presence::Resolved::default()
    };
    let action = presence::Action {
        running: true,
        has_start: true,
        start: preview_start_ms(),
        ct: 1000,
        ..presence::Action::default()
    };
    (
        "Dune.Part.Two.2024.1080p.mkv - PotPlayer".to_string(),
        res,
        action,
        app_support::config::CardKind::Movie,
    )
}

fn picker_rows() -> Vec<metadata::Candidate> {
    vec![
        metadata::Candidate {
            kind: metadata::Kind::MOVIE,
            title: "Dune: Part Two".to_string(),
            year: 2024,
            provider: "cinemeta".to_string(),
            provider_id: "tt15239678".to_string(),
            poster: "https://example.com/dune-poster.jpg".to_string(),
            synopsis: "Paul Atreides unites with the Fremen.".to_string(),
            genres: "Sci-Fi . Adventure".to_string(),
            length: "2h 46m".to_string(),
            episode_count: 0,
        },
        metadata::Candidate {
            kind: metadata::Kind::SERIES,
            title: "Dune: Prophecy".to_string(),
            year: 2024,
            provider: "tvmaze".to_string(),
            provider_id: "tvmaze-dune-prophecy".to_string(),
            poster: String::new(),
            synopsis: "The Bene Gesserit origin.".to_string(),
            genres: "Sci-Fi . Drama".to_string(),
            length: "60 min".to_string(),
            episode_count: 6,
        },
        metadata::Candidate {
            kind: metadata::Kind::MOVIE,
            title: "Dune".to_string(),
            year: 1984,
            provider: "cinemeta".to_string(),
            provider_id: "tt0087182".to_string(),
            poster: String::new(),
            synopsis: "The spice must flow.".to_string(),
            genres: "Sci-Fi".to_string(),
            length: "2h 17m".to_string(),
            episode_count: 0,
        },
    ]
}

fn capture_one(
    tag: &str,
    out: &str,
    setup: impl FnOnce() -> Result<(), String>,
    wide: Option<(i32, i32)>,
) -> Result<(), String> {
    // Fail fast on an unwritable destination: creating the parent dir here
    // turns a GDI+ save error into a plain early reason with the path in it.
    if let Some(parent) = std::path::Path::new(out).parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("snapshot {tag}: cannot create out dir {}: {e}", parent.display()))?;
    }
    let _guard = DesktopGuard::enter(tag).map_err(|e| {
        let msg = format!("snapshot {tag}: {e}");
        log_fail(tag, &e);
        msg
    })?;
    let r = (|| {
        window::create_main_window().map_err(|e| format!("create_main_window: {e}"))?;
        // A setup panic must never strand the thread on the hidden desktop
        // (the guard still drops) and must never surface as a silent crash:
        // convert it to a plain reason and log it to the debug log.
        let setup_r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(setup))
            .map_err(|p| {
                let msg = if let Some(s) = p.downcast_ref::<String>() {
                    s.clone()
                } else if let Some(s) = p.downcast_ref::<&str>() {
                    (*s).to_string()
                } else {
                    "setup panicked".to_string()
                };
                format!("setup panic: {msg}")
            })
            .and_then(|inner| inner);
        setup_r?;
        show_hidden_window();
        pump(900);
        let hwnd = window::ui().main_hwnd as HWND;
        if hwnd.is_null() {
            return Err("no main window".into());
        }
        if let Some((w, h)) = wide {
            resize_to_client(hwnd, w, h);
            pump(500);
            // SAFETY: `hwnd` is our window.
            unsafe { windows_sys::Win32::Graphics::Gdi::UpdateWindow(hwnd) };
            pump(300);
        } else {
            // SAFETY: `hwnd` is our window.
            unsafe { windows_sys::Win32::Graphics::Gdi::UpdateWindow(hwnd) };
            pump(400);
        }
        let (cw, ch) = client_size(hwnd);
        if cw <= 0 || ch <= 0 {
            return Err(format!("snapshot {tag}: client {cw}x{ch} is not paintable"));
        }
        let dpi = window::window_dpi(hwnd);
        let (px, hbm) = print_client(hwnd, cw, ch)?;
        let extra = format!("frame={tag} dpi={dpi} client={cw}x{ch}");
        let res = report_and_save(tag, &px, hbm, out, &extra);
        // SAFETY: bitmap released after the save.
        unsafe {
            DeleteObject(hbm as *mut c_void);
        }
        res
    })();
    if let Err(e) = r.as_ref() {
        log_fail(tag, e);
    }
    r
}

fn capture_card(out: &str) -> Result<(), String> {
    capture_one(
        "card",
        out,
        || {
            let (raw, res, action, kind) = populated_movie();
            seed_cover(&res.artwork)?;
            // A live title is playing, so the hero ghost exists in this frame.
            app_support::config::set_live_now_playing(Some(
                app_support::config::NowPlaying {
                    raw_title: raw.clone(),
                    path: r"C:\Media\Dune.Part.Two.2024.1080p.mkv".to_string(),
                },
            ));
            let mut st = window::ui();
            st.cur_view = View::Card;
            st.record_preview(kind, raw, Some(action), res);
            // Same three rows the picker frame uses: the screen shows a
            // populated list like the mock, not an empty panel.
            st.picker.set_query("Dune".to_string());
            st.picker
                .set_filter(crate::search::KindFilter::All);
            let generation = match st.picker.begin_search() {
                Some((generation, _, _, _)) => generation,
                None => {
                    return Err(format!(
                        "card: query refused: {}",
                        st.picker.status
                    ));
                }
            };
            if !st.picker.finish_search(generation, picker_rows()) {
                return Err("card: finished batch rejected (stale generation)".into());
            }
            st.picker.selected = 0;
            st.picker.top = 0;
            st.picker.open = true;
            st.focus_id = 0;
            st.synopsis_pinned = false;
            st.advanced_open = false;
            st.preview_surface = window::PreviewSurface::Profile;
            st.refresh_dir_list();
            st.picker.open = true;
            st.picker.selected = st.picker.selected.min(st.picker.candidates.len().saturating_sub(1));
            st.picker.top = 0;
            st.invalidate();
            Ok(())
        },
        None,
    )
}

fn capture_card_empty(out: &str) -> Result<(), String> {
    capture_one(
        "card-empty",
        out,
        || {
            // Nothing playing: the hero ghost must be absent, not disabled.
            app_support::config::set_live_now_playing(None);
            let mut st = window::ui();
            st.cur_view = View::Card;
            st.picker.set_query(String::new());
            st.picker
                .set_filter(crate::search::KindFilter::All);
            st.focus_id = 0;
            st.synopsis_pinned = false;
            st.advanced_open = false;
            st.preview_surface = window::PreviewSurface::Profile;
            st.refresh_dir_list();
            Ok(())
        },
        None,
    )
}

fn capture_settings(out: &str) -> Result<(), String> {
    capture_one(
        "settings",
        out,
        || {
            let mut st = window::ui();
            st.enter_settings();
            st.focus_id = 0;
            st.refresh_dir_list();
            Ok(())
        },
        None,
    )
}

fn capture_picker(out: &str) -> Result<(), String> {
    capture_one(
        "picker",
        out,
        || {
            let (raw, res, action, kind) = populated_movie();
            seed_cover(&res.artwork)?;
            {
                let mut st = window::ui();
                st.cur_view = View::Card;
                st.record_preview(kind, raw, Some(action), res);
                // Production search transition: query -> begin -> finished
                // batch, so `open`/`searched`/`status`/`generation` agree the
                // way the paint path expects. Direct field pokes can leave a
                // stale generation that a later pump disagrees with.
                st.picker.set_query("Dune".to_string());
                st.picker
                    .set_filter(crate::search::KindFilter::All);
                let generation = match st.picker.begin_search() {
                    Some((generation, _, _, _)) => generation,
                    None => {
                        return Err(format!(
                            "picker: query refused: {}",
                            st.picker.status
                        ));
                    }
                };
                if !st.picker.finish_search(generation, picker_rows()) {
                    return Err("picker: finished batch rejected (stale generation)".into());
                }
                // Highlight the first row: the populated list with a selected
                // row the frame promises. Clamped so paint never reads past
                // the list.
                let len = st.picker.candidates.len();
                if len == 0 {
                    return Err("picker: production merge dropped all rows".into());
                }
                st.picker.selected = 0;
                st.picker.top = 0;
                st.focus_id = 0;
                st.synopsis_pinned = false;
                st.advanced_open = false;
                st.preview_surface = window::PreviewSurface::Profile;
                st.refresh_dir_list();
                // `refresh_dir_list` only touches dirs + repaint; re-assert the
                // overlay on the same guard so no second lock is taken.
                st.picker.open = true;
                if st.picker.candidates.is_empty() {
                    return Err("picker: overlay lost rows after dir refresh".into());
                }
                st.picker.selected = st.picker.selected.min(st.picker.candidates.len().saturating_sub(1));
                st.picker.top = 0;
                st.invalidate();
            }
            Ok(())
        },
        None,
    )
}

fn capture_picker_empty(out: &str) -> Result<(), String> {
    capture_one(
        "picker-empty",
        out,
        || {
            let (raw, res, action, kind) = populated_movie();
            seed_cover(&res.artwork)?;
            {
                let mut st = window::ui();
                st.cur_view = View::Card;
                st.record_preview(kind, raw, Some(action), res);
                // Production empty transition: a finished search with zero
                // rows owns the exact "No matches ..." status, `open=false`
                // and `searched=true`. No manual status string to drift.
                st.picker.set_query("zzzz-no-match".to_string());
                st.picker
                    .set_filter(crate::search::KindFilter::All);
                let generation = match st.picker.begin_search() {
                    Some((generation, _, _, _)) => generation,
                    None => {
                        return Err(format!(
                            "picker-empty: query refused: {}",
                            st.picker.status
                        ));
                    }
                };
                if !st.picker.finish_search(generation, Vec::new()) {
                    return Err("picker-empty: empty finish rejected (stale generation)".into());
                }
                // Empty list: no highlight to paint, but keep the indices at
                // zero so no paint path reads past the list.
                st.picker.selected = 0;
                st.picker.top = 0;
                st.picker.open = false;
                if st.picker.candidates.is_empty() {
                    // Production already set the no-match status; keep it.
                } else {
                    return Err("picker-empty: empty finish left rows".into());
                }
                st.focus_id = 0;
                st.synopsis_pinned = false;
                st.advanced_open = false;
                st.preview_surface = window::PreviewSurface::Profile;
                st.refresh_dir_list();
                st.picker.open = false;
                st.picker.selected = 0;
                st.picker.top = 0;
                st.invalidate();
            }
            Ok(())
        },
        None,
    )
}

fn capture_wide(out: &str) -> Result<(), String> {
    capture_one(
        "wide",
        out,
        || {
            let (raw, res, action, kind) = populated_movie();
            seed_cover(&res.artwork)?;
            let mut st = window::ui();
            st.cur_view = View::Card;
            st.record_preview(kind, raw, Some(action), res);
            st.picker
                .set_query("Dune: Part Two".to_string());
            st.picker
                .set_filter(crate::search::KindFilter::All);
            st.focus_id = 0;
            st.synopsis_pinned = false;
            st.advanced_open = false;
            st.preview_surface = window::PreviewSurface::Profile;
            st.refresh_dir_list();
            Ok(())
        },
        Some((1180, 700)),
    )
}

/// Capture one named snapshot frame to `out`.
///
/// Frames: `card`, `card-empty`, `settings`, `picker`, `picker-empty`, `wide`.
/// Unknown names fail with a plain reason. On success a non-blank PNG exists
/// at `out`.
pub fn capture_snapshot(frame: &str, out: &str) -> Result<(), String> {
    if out.trim().is_empty() {
        let e = "snapshot: --out <path.png> is required".to_string();
        log_fail(frame, &e);
        return Err(e);
    }
    let r = match frame {
        "card" => capture_card(out),
        "card-empty" => capture_card_empty(out),
        "settings" => capture_settings(out),
        "picker" => capture_picker(out),
        "picker-empty" => capture_picker_empty(out),
        "wide" => capture_wide(out),
        _ => Err(format!(
            "unknown frame {frame:?}; want card|card-empty|settings|picker|picker-empty|wide"
        )),
    };
    if let Err(e) = r.as_ref() {
        log_fail(frame, e);
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_frame_names_the_six_frames() {
        let e = capture_snapshot("bogus", "C:\\tmp\\x.png").unwrap_err();
        assert!(
            e.contains("unknown frame"),
            "plain reason for an unknown frame, got {e:?}"
        );
        for want in [
            "card",
            "card-empty",
            "settings",
            "picker",
            "picker-empty",
            "wide",
        ] {
            assert!(e.contains(want), "reason lists {want}, got {e:?}");
        }
    }

    #[test]
    fn empty_out_is_a_plain_reason() {
        let e = capture_snapshot("card", "   ").unwrap_err();
        assert!(e.contains("--out"), "got {e:?}");
    }

    #[test]
    fn picker_fixture_survives_the_production_merge() {
        // The production merge (`merge_candidates` + `merge_same_work`) must
        // keep all three rows: regressions here are the blank-picker class.
        let rows = picker_rows();
        assert_eq!(rows.len(), 3, "fixture holds three rows");
        let merged =
            crate::search::merge_candidates("Dune", vec![rows.clone()]);
        assert_eq!(
            merged.len(),
            3,
            "production merge keeps the mixed list, got {}",
            merged.len()
        );
    }

    #[test]
    fn film_fixture_never_carries_an_episode_marker() {
        // `2024 · Film · S02E01` is the defect: a Film never carries `SxxExx`.
        let (_, res, _, kind) = populated_movie();
        assert_eq!(kind, app_support::config::CardKind::Movie);
        assert!(res.is_movie, "the card fixture is a movie");
        assert_eq!((res.season, res.number, res.absolute), (0, 0, 0));
        let meta = crate::window::hero_meta_for(
            res.year,
            res.is_movie,
            false,
            &res.length,
            res.season,
            res.number,
            res.absolute,
        );
        assert_eq!(meta, "2024 · Film · 2h 46m", "movie hero derives runtime: {meta:?}");
        assert!(!meta.contains('S'), "no episode marker on a Film: {meta:?}");
        for c in picker_rows() {
            if c.kind == metadata::Kind::MOVIE {
                assert_eq!(c.episode_count, 0, "Film row carries episodes: {:?}", c.title);
            }
        }
    }

    #[test]
    fn series_hero_derives_the_episode_code() {
        let meta = crate::window::hero_meta_for(2024, false, true, "60 min", 2, 1, 0);
        assert_eq!(meta, "2024 · Anime series · S02E01", "series hero: {meta:?}");
        let plain = crate::window::hero_meta_for(2024, false, false, "60 min", 2, 1, 0);
        assert_eq!(plain, "2024 · Series · S02E01", "non-anime series: {plain:?}");
    }
}
