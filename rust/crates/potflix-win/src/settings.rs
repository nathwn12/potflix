//! `settings` -- mirrors the settings surface split out of the Go root package.
//!
//! In Go the settings window and its sections live in `window.go` /
//! `settings_sections.go`; the persisted values they edit are `config.go`. This
//! module owns the Settings view's LIBRARY + STARTUP + SCAN seams: the pure
//! status-string mappers, the window-free clear/export/import cores, the native
//! Save-As/Open dialogs, and the HKCU Run startup toggle. The stateful handlers
//! live on `window::UiState`; the win32/dialog/registry work lives here so
//! `window.rs` keeps only layout rects, paint labels, and action routing.

// Win32 entry points take raw handles by design; see the same allow in
// `window.rs`.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::Controls::Dialogs::{
    GetOpenFileNameW, GetSaveFileNameW, OPENFILENAMEW,
};

use crate::window::fit_scan_status;

/// `appVersion` (`go\settings_sections.go:38`). Derived from this crate's own
/// version, which is the workspace version (`rust\Cargo.toml`), which in turn
/// tracks the installer's `MyAppVersion` (`tools\potflix.iss`): the app version
/// can never disagree with the binary that wrote it.
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

// ---------------------------------------------------------------------------
// Status strings (`go\settings_sections.go:48-91`).
// ---------------------------------------------------------------------------

/// `clearCacheStatus` (`go\settings_sections.go:48`).
#[must_use]
pub fn clear_cache_status(titles: i32, records: i32) -> String {
    format!("Cleared {titles} cached titles - {records} folder records")
}

/// `clearMetadataStatus` (`go\settings_sections.go:57`).
pub const CLEAR_METADATA_STATUS: &str =
    "Cleared metadata cache - titles refetch on the next scan; the file index is kept.";

/// `clearIndexStatus` (`go\settings_sections.go:61`).
#[must_use]
pub fn clear_index_status(records: i64) -> String {
    format!(
        "Cleared file index - {records} folder records dropped; the next scan re-indexes every folder."
    )
}

/// `exportBundleStatus` (`go\settings_sections.go:68`).
#[must_use]
pub fn export_bundle_status(entries: i32, dest: &str) -> String {
    fit_scan_status(&format!("Exported {entries} titles -> {dest}"))
}

/// `importBundleStatus` (`go\settings_sections.go:74`).
#[must_use]
pub fn import_bundle_status(st: &bundle::ImportStats) -> String {
    format!(
        "Imported: {} added, {} already present, {} skipped",
        st.added,
        st.present,
        st.skipped_schema + st.invalid
    )
}

/// `exportDefaultName` (`go\settings_sections.go:80`): the Save-As default.
#[must_use]
pub fn export_default_name_ymd(year: i32, month: u32, day: u32) -> String {
    format!("potflix-metadata-{year:04}-{month:02}-{day:02}.json")
}

/// `exportDefaultName(time.Now())` (`go\settings_sections.go:386`).
#[must_use]
pub fn export_default_name_now() -> String {
    let (y, m, d) = today_ymd();
    export_default_name_ymd(y, m, d)
}

/// The civil date of "now" in UTC (Howard Hinnant's `civil_from_days`).
#[must_use]
pub fn today_ymd() -> (i32, u32, u32) {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    civil_from_days(secs.div_euclid(86_400))
}

fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m as u32, d as u32)
}

/// `startupToggleLabel` (`go\settings_sections.go:86`).
#[must_use]
pub fn startup_toggle_label(on: bool) -> &'static str {
    if on {
        "[x] Start with Windows"
    } else {
        "[ ] Start with Windows"
    }
}

// ---------------------------------------------------------------------------
// SCAN speed section (`go\settings_sections.go:99-191`).
// ---------------------------------------------------------------------------

/// `scanHintText` (`go\settings_sections.go:105`).
pub const SCAN_HINT_TEXT: &str = "Turbo = each provider's published limit (default). Balanced = half speed, Gentle = quarter speed - pick one if you want to be gentler on the services.";

/// `scanSegmentLabel` (`go\settings_sections.go:108`).
#[must_use]
pub fn scan_segment_label(profile: &str) -> &'static str {
    match app_support::config::normalize_scan_speed(profile) {
        app_support::config::SCAN_SPEED_TURBO => "Turbo",
        app_support::config::SCAN_SPEED_GENTLE => "Gentle",
        _ => "Balanced",
    }
}

/// `scanProfileForButton` (`go\settings_sections.go:121`).
#[must_use]
pub fn scan_profile_for_button(id: i32) -> &'static str {
    match id {
        crate::window::BTN_SCAN_GENTLE => app_support::config::SCAN_SPEED_GENTLE,
        crate::window::BTN_SCAN_BALANCED => app_support::config::SCAN_SPEED_BALANCED,
        crate::window::BTN_SCAN_TURBO => app_support::config::SCAN_SPEED_TURBO,
        _ => "",
    }
}

/// `scanButtonForProfile` (`go\settings_sections.go:135`).
#[must_use]
pub fn scan_button_for_profile(profile: &str) -> i32 {
    match app_support::config::normalize_scan_speed(profile) {
        app_support::config::SCAN_SPEED_GENTLE => crate::window::BTN_SCAN_GENTLE,
        app_support::config::SCAN_SPEED_TURBO => crate::window::BTN_SCAN_TURBO,
        _ => crate::window::BTN_SCAN_BALANCED,
    }
}

/// Theme segment model: the two named themes, AMOLED first (the default).
#[must_use]
pub fn theme_labels() -> [(&'static str, &'static str); 2] {
    [
        ("AMOLED", app_support::config::THEME_AMOLED),
        ("Dark", app_support::config::THEME_DARK),
    ]
}

/// `themeProfileForButton`: the stored id a theme button selects.
#[must_use]
pub fn theme_profile_for_button(id: i32) -> &'static str {
    match id {
        crate::window::BTN_THEME_AMOLED => app_support::config::THEME_AMOLED,
        crate::window::BTN_THEME_DARK => app_support::config::THEME_DARK,
        _ => "",
    }
}

/// `themeButtonForProfile`: the button id a stored theme selects.
#[must_use]
pub fn theme_button_for_profile(theme: &str) -> i32 {
    match app_support::config::normalize_theme(theme) {
        app_support::config::THEME_DARK => crate::window::BTN_THEME_DARK,
        _ => crate::window::BTN_THEME_AMOLED,
    }
}

/// Theme segment style: the selected segment uses the text-colour underline
/// convention (`BTN_TOGGLE_ON`), never red — the same anatomy as the Card
/// scope row.
#[must_use]
pub fn theme_segment_style(theme: &str, current: &str) -> i32 {
    if app_support::config::normalize_theme(theme)
        == app_support::config::normalize_theme(current)
    {
        return crate::window::BTN_TOGGLE_ON;
    }
    crate::window::BTN_TOGGLE
}

/// `onSelectTheme`: the window-free core of the Theme-segment click. It
/// normalises the choice, persists it to `cfg_path`, and returns the status
/// text. It never touches the search cache or match state — only the `theme`
/// key moves. Explicit `cfg_path` so tests drive it on a temp store.
pub fn on_select_theme(cfg_path: &Path, theme: &str) -> Result<String, String> {
    let theme = app_support::config::normalize_theme(theme);
    let mut cfg = app_support::config::load_config_for_update(cfg_path).map_err(|e| {
        format!("settings store unreadable ({e}) - quarantined; nothing saved")
    })?;
    cfg.theme = theme.to_string();
    app_support::config::save_config_to(cfg_path, &cfg).map_err(|e| e.to_string())?;
    Ok(format!(
        "Theme: {}",
        if theme == app_support::config::THEME_DARK {
            "Dark"
        } else {
            "AMOLED"
        }
    ))
}

/// `scanSegmentStyle` (`go\settings_sections.go:149`).
#[must_use]
pub fn scan_segment_style(seg: &str, current: &str) -> i32 {
    if app_support::config::normalize_scan_speed(seg)
        == app_support::config::normalize_scan_speed(current)
    {
        return crate::window::BTN_TAB_SEL;
    }
    crate::window::BTN_TAB
}

/// `onSelectScanSpeed` (`go\settings_sections.go:177`): the window-free core of
/// the SCAN-segment click. It normalises the choice, persists it to `cfg_path`,
/// then applies it to the live rate limiter immediately via
/// `metadata::SetRateProfile` - Turbo/Balanced/Gentle take effect with no
/// restart (the README's "applied immediately"). Returns the status text the
/// Settings view shows, or the save error. Explicit `cfg_path` so tests drive it
/// on a temp store instead of the real one.
pub fn on_select_scan_speed(cfg_path: &Path, profile: &str) -> Result<String, String> {
    let profile = app_support::config::normalize_scan_speed(profile);
    let mut cfg = app_support::config::load_config_for_update(cfg_path).map_err(|e| {
        format!("settings store unreadable ({e}) - quarantined; nothing saved")
    })?;
    cfg.scan_speed = profile.to_string();
    app_support::config::save_config_to(cfg_path, &cfg).map_err(|e| e.to_string())?;
    metadata_http::set_rate_profile(profile);
    Ok(format!("Scan speed: {}", scan_segment_label(profile)))
}

// ---------------------------------------------------------------------------
// Window-free cores (`go\settings_sections.go:203-275`).
// ---------------------------------------------------------------------------

/// `clearLibraryCache` (`go\settings_sections.go:203`).
pub fn clear_library_cache(
    meta_dir: &str,
    index_path: &str,
) -> (i32, i64, Result<(), repair::Error>) {
    let titles = match bundle::clear(meta_dir) {
        Ok(t) => t,
        Err(e) => return (0, 0, Err(repair::Error::new(e.to_string()))),
    };
    let (records, err) = clear_scan_index(index_path);
    (titles, records, err)
}

/// `clearMetadataCache` (`go\settings_sections.go:217`). The clear's SCOPE is
/// every saved match the app holds, not one file: the metadata store AND the
/// picker's own answer cache ([`metadata::search_cache_clear_at`]), which
/// answers a typed search with no network at all. Clearing only the store left
/// the picker replaying `search-cache.json`, so the owner's "Drop every saved
/// match" visibly did nothing until that file happened to be replaced.
///
/// Both locations are explicit, so a test drives temp paths and can never
/// delete the owner's real cache.
pub fn clear_metadata_cache(
    meta_dir: &str,
    search_cache: &Path,
) -> Result<(), repair::Error> {
    metadata::cache::clear_cache(meta_dir).map_err(|e| repair::Error::new(e.message()))?;
    metadata::search_cache_clear_at(search_cache).map_err(|e| repair::Error::new(e.message()))
}

/// `clearScanIndex` (`go\settings_sections.go:230`): drop the live handle first.
pub fn clear_scan_index(index_path: &str) -> (i64, Result<(), repair::Error>) {
    let records = library::scanindex::load_scan_index(index_path).len();
    let _ = library::index::close_index(index_path);
    for p in index_files(index_path) {
        if let Err(e) = std::fs::remove_file(&p)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            return (records, Err(repair::Error::new(e.to_string())));
        }
    }
    (records, Ok(()))
}

/// `indexFiles` (`go\settings_sections.go:243`).
#[must_use]
pub fn index_files(index_path: &str) -> Vec<String> {
    let store = library::index::index_store_path(index_path);
    if store.is_empty() || store == index_path {
        return vec![index_path.to_string()];
    }
    vec![store, index_path.to_string()]
}

/// `exportBundleTo` (`go\settings_sections.go:252`).
pub fn export_bundle_to(
    meta_dir: &str,
    dest: &str,
    schema: i32,
    ver: &str,
) -> Result<i32, repair::Error> {
    let mut f = std::fs::File::create(dest).map_err(|e| repair::Error::new(e.to_string()))?;
    let st = bundle::export(meta_dir, &mut f, schema, ver);
    let closed = f.sync_all();
    match st {
        Ok(st) => {
            if let Err(e) = closed {
                return Err(repair::Error::new(e.to_string()));
            }
            Ok(st.entries)
        }
        Err(e) => Err(repair::Error::new(e.to_string())),
    }
}

/// `importBundleFrom` (`go\settings_sections.go:268`).
pub fn import_bundle_from(
    meta_dir: &str,
    src: &str,
    schema: i32,
) -> Result<bundle::ImportStats, repair::Error> {
    let mut f = std::fs::File::open(src).map_err(|e| repair::Error::new(e.to_string()))?;
    bundle::import(meta_dir, &mut f, schema).map_err(|e| repair::Error::new(e.to_string()))
}

// ---------------------------------------------------------------------------
// Native dialogs (`go\settings_sections.go:281-428`).
// ---------------------------------------------------------------------------

const MB_YESNO: u32 = 0x0000_0004;
const MB_ICONWARNING: u32 = 0x0000_0030;
const IDYES: i32 = 6;

const OFN_OVERWRITEPROMPT: u32 = 0x0000_0002;
const OFN_NOCHANGEDIR: u32 = 0x0000_0008;
const OFN_PATHMUSTEXIST: u32 = 0x0000_0800;
const OFN_FILEMUSTEXIST: u32 = 0x0000_1000;
const OFN_EXPLORER: u32 = 0x0008_0000;

/// `openFileName` (`go\settings_sections.go:306`): `OPENFILENAMEW` on amd64,
/// field order == ABI order, `lStructSize` the compiled size.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct OpenFileNameW {
    pub l_struct_size: u32,
    pub hwnd_owner: HWND,
    pub h_instance: usize,
    pub lpstr_filter: *const u16,
    pub lpstr_custom_filter: *mut u16,
    pub n_max_cust_filter: u32,
    pub n_filter_index: u32,
    pub lpstr_file: *mut u16,
    pub n_max_file: u32,
    pub lpstr_file_title: *mut u16,
    pub n_max_file_title: u32,
    pub lpstr_initial_dir: *const u16,
    pub lpstr_title: *const u16,
    pub flags: u32,
    pub n_file_offset: u16,
    pub n_file_extension: u16,
    pub lpstr_def_ext: *const u16,
    pub l_cust_data: usize,
    pub lpfn_hook: usize,
    pub lp_template_name: *const u16,
    pub pv_reserved: usize,
    pub dw_reserved: u32,
    pub flags_ex: u32,
}

impl Default for OpenFileNameW {
    fn default() -> Self {
        // SAFETY: a POD struct of integers and pointers; zeroes are valid.
        unsafe { std::mem::zeroed() }
    }
}

/// `utf16Filter` (`go\settings_sections.go:336`): interior NULs preserved.
#[must_use]
pub fn utf16_filter(s: &str) -> Vec<u16> {
    let mut u: Vec<u16> = s.encode_utf16().collect();
    u.push(0);
    u
}

/// `jsonBundleFilter` (`go\settings_sections.go:345`).
#[must_use]
pub fn json_bundle_filter() -> Vec<u16> {
    utf16_filter("Potflix metadata (*.json)\x00*.json\x00All files (*.*)\x00*.*\x00")
}

/// `desktopDir` (`go\settings_sections.go:351`).
#[must_use]
pub fn desktop_dir() -> String {
    std::env::var("USERPROFILE")
        .ok()
        .filter(|h| !h.is_empty())
        .map_or_else(String::new, |h| {
            Path::new(&h).join("Desktop").to_string_lossy().into_owned()
        })
}

/// `confirmClear` (`go\settings_sections.go:361`).
#[must_use]
pub fn confirm_clear(owner: HWND, title: &str, text: &str) -> bool {
    let t = crate::win32::utf16_z(text);
    let c = crate::win32::utf16_z(title);
    // SAFETY: two live NUL-terminated buffers; MessageBoxW reads them.
    let r = unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::MessageBoxW(
            owner,
            t.as_ptr(),
            c.as_ptr(),
            MB_YESNO | MB_ICONWARNING,
        )
    };
    r == IDYES
}

/// `clearMetadataConfirmText` (`go\settings_sections.go:373`).
#[must_use]
pub fn clear_metadata_confirm_text() -> &'static str {
    "Delete every cached title? They will be fetched again from the internet. Your folders and the file index are kept."
}

/// `clearIndexConfirmText` (`go\settings_sections.go:377`).
#[must_use]
pub fn clear_index_confirm_text() -> &'static str {
    "Delete the file index? The next scan re-walks every folder. Your folders and the cached metadata are kept."
}

fn fill_file_buffer(file: &mut [u16], name: &str) {
    let u: Vec<u16> = name.encode_utf16().take(file.len() - 1).collect();
    file[..u.len()].copy_from_slice(&u);
}

/// `saveBundleDialog` (`go\settings_sections.go:383`).
#[must_use]
#[allow(clippy::field_reassign_with_default)]
pub fn save_bundle_dialog(owner: HWND) -> String {
    let filter = json_bundle_filter();
    let mut file = vec![0u16; 260];
    fill_file_buffer(&mut file, &export_default_name_now());
    let init = desktop_dir();
    let iu = crate::win32::utf16_z(&init);
    let title = crate::win32::utf16_z("Export Potflix metadata");
    let def_ext = crate::win32::utf16_z("json");
    let mut ofn = OpenFileNameW::default();
    ofn.l_struct_size = std::mem::size_of::<OpenFileNameW>() as u32;
    ofn.hwnd_owner = owner;
    ofn.lpstr_filter = filter.as_ptr();
    ofn.lpstr_file = file.as_mut_ptr();
    ofn.n_max_file = file.len() as u32;
    ofn.lpstr_initial_dir = if init.is_empty() {
        std::ptr::null()
    } else {
        iu.as_ptr()
    };
    ofn.lpstr_title = title.as_ptr();
    ofn.flags = OFN_EXPLORER | OFN_NOCHANGEDIR | OFN_PATHMUSTEXIST | OFN_OVERWRITEPROMPT;
    ofn.lpstr_def_ext = def_ext.as_ptr();
    call_file_dialog_with(ofn, true)
}

/// `openBundleDialog` (`go\settings_sections.go:409`).
#[must_use]
#[allow(clippy::field_reassign_with_default)]
pub fn open_bundle_dialog(owner: HWND) -> String {
    let filter = json_bundle_filter();
    let mut file = vec![0u16; 260];
    let init = desktop_dir();
    let iu = crate::win32::utf16_z(&init);
    let title = crate::win32::utf16_z("Import Potflix metadata");
    let mut ofn = OpenFileNameW::default();
    ofn.l_struct_size = std::mem::size_of::<OpenFileNameW>() as u32;
    ofn.hwnd_owner = owner;
    ofn.lpstr_filter = filter.as_ptr();
    ofn.lpstr_file = file.as_mut_ptr();
    ofn.n_max_file = file.len() as u32;
    ofn.lpstr_initial_dir = if init.is_empty() {
        std::ptr::null()
    } else {
        iu.as_ptr()
    };
    ofn.lpstr_title = title.as_ptr();
    ofn.flags = OFN_EXPLORER | OFN_NOCHANGEDIR | OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST;
    call_file_dialog_with(ofn, false)
}

fn call_file_dialog_with(mut ofn: OpenFileNameW, save: bool) -> String {
    // SAFETY: `ofn` mirrors OPENFILENAMEW; both are the same ABI size (asserted
    // in tests), so the cast is sound.
    let r = unsafe {
        let p = std::ptr::from_mut(&mut ofn).cast::<OPENFILENAMEW>();
        if save {
            GetSaveFileNameW(p)
        } else {
            GetOpenFileNameW(p)
        }
    };
    if r == 0 {
        return String::new();
    }
    let file = unsafe { std::slice::from_raw_parts(ofn.lpstr_file, ofn.n_max_file as usize) };
    let end = file.iter().position(|&c| c == 0).unwrap_or(file.len());
    String::from_utf16_lossy(&file[..end])
}

// ---------------------------------------------------------------------------
// STARTUP registry (`go\settings_sections.go:435-585`).
// ---------------------------------------------------------------------------

const STARTUP_RUN_SUBKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const STARTUP_RUN_VALUE: &str = "Potflix";

/// `autostartArg` (`go\settings_sections.go:464`).
pub const AUTOSTART_ARG: &str = "--minimized";

/// `startupCommandFor` (`go\settings_sections.go:469`).
#[must_use]
pub fn startup_command_for(exe: &str) -> String {
    format!("\"{exe}\" {AUTOSTART_ARG}")
}

/// `startupValueFor` (`go\settings_sections.go:495`).
#[must_use]
pub fn startup_value_for(exe: &str) -> String {
    format!("\"{exe}\"")
}

/// `startupValueEnabled` (`go\settings_sections.go:476`).
#[must_use]
pub fn startup_value_enabled(got: &str, exe: &str) -> bool {
    got == startup_command_for(exe) || got == startup_value_for(exe)
}

/// `hasArg` (`go\main.go:218`).
#[must_use]
pub fn has_arg(args: &[String], flag: &str) -> bool {
    args.iter().any(|a| a == flag)
}

/// `launchedViaAutostart` (`go\settings_sections.go:483`).
#[must_use]
pub fn launched_via_autostart(args: &[String]) -> bool {
    has_arg(args, AUTOSTART_ARG)
}

/// `shouldShowMainWindow` (`go\settings_sections.go:490`).
#[must_use]
pub fn should_show_main_window(args: &[String]) -> bool {
    !launched_via_autostart(args)
}

/// `startupExePath` (`go\settings_sections.go:500`).
#[must_use]
pub fn startup_exe_path() -> String {
    std::env::current_exe()
        .ok()
        .filter(|p| !p.as_os_str().is_empty())
        .map_or_else(String::new, |p| p.to_string_lossy().into_owned())
}

/// `MetadataDir` (`go\config.go:152`): `...\Potflix\meta`, never a hardcoded env.
#[must_use]
pub fn metadata_dir() -> String {
    app_support::config::metadata_dir()
        .to_string_lossy()
        .into_owned()
}

/// `ScanIndexPath` (`go\config.go:162`).
#[must_use]
pub fn scan_index_path() -> String {
    app_support::config::scan_index_path()
        .to_string_lossy()
        .into_owned()
}

fn open_run_key(access: u32) -> Option<windows_sys::Win32::System::Registry::HKEY> {
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RegOpenKeyExW};
    let sub = crate::win32::utf16_z(STARTUP_RUN_SUBKEY);
    let mut hk: windows_sys::Win32::System::Registry::HKEY = std::ptr::null_mut();
    // SAFETY: `sub` is live; `hk` is written on success.
    let r = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, sub.as_ptr(), 0, access, &mut hk) };
    if r != 0 || hk.is_null() {
        None
    } else {
        Some(hk)
    }
}

fn read_startup_value() -> Option<String> {
    use windows_sys::Win32::System::Registry::{REG_SZ, RegQueryValueExW};
    let hk = open_run_key(windows_sys::Win32::System::Registry::KEY_QUERY_VALUE)?;
    let name = crate::win32::utf16_z(STARTUP_RUN_VALUE);
    let mut typ = 0u32;
    let mut size = 0u32;
    // SAFETY: first query returns the required size only (`lpdata` null).
    let r = unsafe {
        RegQueryValueExW(
            hk,
            name.as_ptr(),
            std::ptr::null(),
            &mut typ,
            std::ptr::null_mut(),
            &mut size,
        )
    };
    if r != 0 || typ != REG_SZ || size == 0 || size > 4096 {
        // SAFETY: close the key opened above.
        unsafe { windows_sys::Win32::System::Registry::RegCloseKey(hk) };
        return None;
    }
    let mut buf = vec![0u16; size as usize / 2 + 1];
    // SAFETY: `buf` sized for `size` bytes.
    let r = unsafe {
        RegQueryValueExW(
            hk,
            name.as_ptr(),
            std::ptr::null(),
            &mut typ,
            buf.as_mut_ptr().cast::<u8>(),
            &mut size,
        )
    };
    // SAFETY: close the key.
    unsafe { windows_sys::Win32::System::Registry::RegCloseKey(hk) };
    if r != 0 {
        return None;
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..end]))
}

/// `queryStartupEnabled` (`go\settings_sections.go:549`).
#[must_use]
pub fn query_startup_enabled() -> bool {
    let exe = startup_exe_path();
    if exe.is_empty() {
        return false;
    }
    read_startup_value().is_some_and(|got| startup_value_enabled(&got, &exe))
}

/// One source of truth for "start with Windows": the HKCU Run key. The config
/// file only caches that value, refreshed here on every read, so the two can
/// never drift apart (a write to config alone is not the switch).
pub fn sync_startup_cache(cfg_path: &Path) -> bool {
    sync_startup_cache_with(cfg_path, query_startup_enabled())
}

/// The testable core of [`sync_startup_cache`]: records `enabled` (the
/// registry's answer) into the config cache. A missing store writes fresh; an
/// unreadable one is left alone (already quarantined by the load) and the
/// registry answer is still returned.
pub fn sync_startup_cache_with(cfg_path: &Path, enabled: bool) -> bool {
    if let Ok(mut cfg) = app_support::config::load_config_for_update(cfg_path)
        && cfg.start_with_windows != enabled
    {
        cfg.start_with_windows = enabled;
        let _ = app_support::config::save_config_to(cfg_path, &cfg);
    }
    enabled
}

/// `setStartupEnabled` (`go\settings_sections.go:560`).
pub fn set_startup_enabled(on: bool) -> Result<(), String> {
    use windows_sys::Win32::System::Registry::{
        KEY_SET_VALUE, REG_SZ, RegCloseKey, RegDeleteValueW, RegSetValueExW,
    };
    let hk =
        open_run_key(KEY_SET_VALUE).ok_or_else(|| "startup: cannot open Run key".to_string())?;
    let name = crate::win32::utf16_z(STARTUP_RUN_VALUE);
    if !on {
        // SAFETY: delete the value under the open key.
        let r = unsafe { RegDeleteValueW(hk, name.as_ptr()) };
        // SAFETY: close the key.
        unsafe { RegCloseKey(hk) };
        if r != 0 && r != 2 {
            return Err(format!("startup: cannot delete Run value ({r})"));
        }
        return Ok(());
    }
    let exe = startup_exe_path();
    if exe.is_empty() {
        // SAFETY: close the key.
        unsafe { RegCloseKey(hk) };
        return Err("startup: cannot resolve exe path".to_string());
    }
    // Double-NUL-terminated UTF-16, byte length len*2 (`go\settings_sections.go:578-580`).
    let mut data: Vec<u16> = startup_command_for(&exe).encode_utf16().collect();
    data.push(0);
    data.push(0);
    // SAFETY: `name`/`data` live for the call; the key is closed after.
    let r = unsafe {
        RegSetValueExW(
            hk,
            name.as_ptr(),
            0,
            REG_SZ,
            data.as_ptr().cast::<u8>(),
            (data.len() * 2) as u32,
        )
    };
    // SAFETY: close the key.
    unsafe { RegCloseKey(hk) };
    if r != 0 {
        return Err(format!("startup: cannot write Run value ({r})"));
    }
    Ok(())
}

/// `onOpenLogFolder` (`go\settings_sections.go:688`).
pub fn on_open_log_folder() {
    let dir = crate::win32::utf16_z(&app_support::debuglog::exe_dir_or_dot().to_string_lossy());
    let op = crate::win32::utf16_z("open");
    // SAFETY: two live buffers; ShellExecuteW reads them.
    unsafe {
        windows_sys::Win32::UI::Shell::ShellExecuteW(
            std::ptr::null_mut(),
            op.as_ptr(),
            dir.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            crate::window::SW_SHOW_NORMAL,
        );
    }
    crate::window::app_logf("startup: opened log folder");
}

/// `browseForFolder` (`go\window.go:2786`).
#[must_use]
pub fn browse_for_folder(owner: HWND) -> String {
    use windows_sys::Win32::UI::Shell::{SHBrowseForFolderW, SHGetPathFromIDListW};
    let mut display = [0u16; 260];
    let title = crate::win32::utf16_z("Choose a media folder");
    let bi = crate::win32::BrowseInfoW {
        hwnd_owner: owner as usize,
        pidl_root: 0,
        display_name: display.as_mut_ptr(),
        title: title.as_ptr(),
        flags: crate::win32::BIF_RETURN_ONLY_FS_DIRS | crate::win32::BIF_NEW_DIALOG_STYLE,
        _pad0: 0,
        callback: 0,
        l_param: 0,
        image: 0,
        _pad1: 0,
    };
    // SAFETY: `bi` is live; the returned pidl is freed below.
    unsafe {
        let bi_ptr = std::ptr::from_ref(&bi).cast::<windows_sys::Win32::UI::Shell::BROWSEINFOW>();
        let pidl = SHBrowseForFolderW(bi_ptr);
        if pidl.is_null() {
            return String::new();
        }
        let mut path = [0u16; 260];
        let r = SHGetPathFromIDListW(pidl, path.as_mut_ptr());
        crate::win32::CoTaskMemFree(pidl.cast::<core::ffi::c_void>() as *const core::ffi::c_void);
        if r == 0 {
            return String::new();
        }
        let end = path.iter().position(|&c| c == 0).unwrap_or(path.len());
        String::from_utf16_lossy(&path[..end])
    }
}

// ===TESTS===

#[cfg(test)]
mod tests {
    #![allow(clippy::field_reassign_with_default)]
    use super::*;

    struct Scratch {
        dir: std::path::PathBuf,
    }
    impl Scratch {
        fn new(tag: &str) -> Self {
            let n = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos());
            let dir =
                std::env::temp_dir().join(format!("potflix-win-{tag}-{}-{n}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            Self { dir }
        }
        fn path(&self) -> &std::path::Path {
            &self.dir
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn seed_sixteen_hex(dir: &std::path::Path, key: &str, schema: i32, media: &str) -> String {
        let name = metadata::cache::cache_file_name_for(key);
        let body = format!(r#"{{"key":"{key}","schema":{schema},"media":{media}}}"#);
        std::fs::write(dir.join(&name), body).unwrap();
        name
    }

    #[test]
    fn export_default_name_formats() {
        assert_eq!(
            export_default_name_ymd(2026, 10, 9),
            "potflix-metadata-2026-10-09.json"
        );
    }

    #[test]
    fn import_bundle_status_summary() {
        let st = bundle::ImportStats {
            added: 2,
            present: 5,
            skipped_schema: 1,
            invalid: 3,
            total: 11,
        };
        assert_eq!(
            import_bundle_status(&st),
            "Imported: 2 added, 5 already present, 4 skipped"
        );
        assert_eq!(
            import_bundle_status(&bundle::ImportStats::default()),
            "Imported: 0 added, 0 already present, 0 skipped"
        );
    }

    #[test]
    fn clear_export_status_strings() {
        assert_eq!(
            clear_cache_status(383, 716),
            "Cleared 383 cached titles - 716 folder records"
        );
        let got = export_bundle_status(3, r"C:\Users\x\Desktop\potflix-metadata-2026-10-09.json");
        assert!(got.starts_with("Exported 3 titles -> "), "{got}");
    }

    #[test]
    fn startup_toggle_label_text() {
        assert_eq!(startup_toggle_label(true), "[x] Start with Windows");
        assert_eq!(startup_toggle_label(false), "[ ] Start with Windows");
    }

    #[test]
    fn startup_value_for_quotes_exe() {
        let exe = r"C:\Program Files\Potflix\potflix.exe";
        assert_eq!(startup_value_for(exe), format!("\"{exe}\""));
    }

    #[test]
    fn bundle_rule_tracks_metadata() {
        let scratch = Scratch::new("bundle-rule");
        let dir = scratch.path();
        let key = "movie|tracked|2001";
        let name = metadata::cache::cache_file_name_for(key);
        assert_eq!(name.len(), 16 + ".json".len());
        assert!(name.ends_with(".json"));
        std::fs::write(
            dir.join(&name),
            format!(
                r#"{{"key":"{key}","schema":{},"media":{{"title":"T"}}}}"#,
                metadata::cache::CACHE_SCHEMA
            ),
        )
        .unwrap();
        assert_eq!(bundle::count(&dir.to_string_lossy()).unwrap(), 1);
        assert_eq!(bundle::clear(&dir.to_string_lossy()).unwrap(), 1);
    }

    #[test]
    fn metadata_dir_and_scan_index_path_conventions() {
        let meta = metadata_dir();
        assert!(
            Path::new(&meta).ends_with(std::path::Path::new("Potflix").join("meta")),
            "{meta}"
        );
        let idx = scan_index_path();
        assert!(
            Path::new(&idx).ends_with(std::path::Path::new("Potflix").join("scanindex.json")),
            "{idx}"
        );
    }

    #[test]
    fn startup_registry_is_source_of_truth_for_config_cache() {
        // The registry says on but the cache says off: the sync rewrites the
        // cache, so the two never drift.
        let scratch = Scratch::new("startup-truth");
        let path = scratch.path().join("config.json");
        let stale = app_support::config::Config {
            dirs: vec![r"C:\Media".into()],
            ..app_support::config::Config::default()
        };
        app_support::config::save_config_to(&path, &stale).unwrap();
        assert!(sync_startup_cache_with(&path, true));
        let cached = app_support::config::load_config_from(&path).unwrap();
        assert!(cached.start_with_windows, "cache follows the registry");
        assert_eq!(cached.dirs, vec![r"C:\Media".to_string()], "dirs kept");

        // And back off again; a missing store is created from the registry.
        assert!(!sync_startup_cache_with(&path, false));
        assert!(!app_support::config::load_config_from(&path).unwrap().start_with_windows);
        let fresh = scratch.path().join("fresh.json");
        assert!(sync_startup_cache_with(&fresh, true));
        assert!(app_support::config::load_config_from(&fresh).unwrap().start_with_windows);
    }

    #[test]
    fn config_start_with_windows_round_trip() {
        let scratch = Scratch::new("cfg-round-trip");
        let path = scratch.path().join("Potflix").join("config.json");
        let want = app_support::config::Config {
            dirs: vec![r"C:\Media".into()],
            start_with_windows: true,
            ..app_support::config::Config::default()
        };
        app_support::config::save_config_to(&path, &want).unwrap();
        let got = app_support::config::load_config_from(&path).unwrap();
        assert!(got.start_with_windows);
        assert_eq!(got.dirs.len(), 1);

        let legacy = scratch.path().join("legacy.json");
        std::fs::write(&legacy, r#"{"dirs":[]}"#).unwrap();
        let old = app_support::config::load_config_from(&legacy).unwrap();
        assert!(!old.start_with_windows);
    }

    #[test]
    fn clear_library_cache_core() {
        let scratch = Scratch::new("clear-lib");
        let meta = scratch.path().join("meta");
        std::fs::create_dir_all(&meta).unwrap();
        seed_sixteen_hex(
            &meta,
            "movie|alpha|2001",
            metadata::cache::CACHE_SCHEMA,
            r#"{"title":"Alpha"}"#,
        );
        seed_sixteen_hex(
            &meta,
            "movie|beta|2002",
            metadata::cache::CACHE_SCHEMA,
            r#"{"title":"Beta"}"#,
        );
        std::fs::write(meta.join("notes.json"), r#"{"keep":true}"#).unwrap();

        let idx_path = scratch.path().join("scanindex.json");
        let idx = idx_path.to_string_lossy().into_owned();
        let meta_s = meta.to_string_lossy().into_owned();
        {
            let ix = library::scanindex::load_scan_index(&idx);
            ix.record(
                r"C:\Media",
                "fp",
                metadata::cache::CACHE_SCHEMA,
                &["k1".to_string()],
            );
            ix.save(&idx).unwrap();
        }

        let (titles, records, err) = clear_library_cache(&meta_s, &idx);
        assert!(err.is_ok(), "{err:?}");
        assert_eq!((titles, records), (2, 1));
        assert!(!idx_path.exists(), "scan index file was not deleted");
        assert_eq!(
            std::fs::read_to_string(meta.join("notes.json")).unwrap(),
            r#"{"keep":true}"#
        );
        assert_eq!(bundle::count(&meta_s).unwrap(), 0);

        let (_, _, err) = clear_library_cache(&meta_s, &idx);
        assert!(err.is_ok(), "missing index must not error");
    }

    /// `QA4-D1` (cache scope): "Clear metadata" promises "Drop every saved
    /// match", and the picker's own saved answers are part of that set -- a
    /// stored search answers a typed query with no network at all. Pre-fix the
    /// action cleared the metadata store only, so `search-cache.json` kept
    /// serving the very rows the user just cleared.
    #[test]
    fn clear_metadata_cache_drops_the_pickers_saved_answers() {
        let scratch = Scratch::new("clear-meta-search");
        let meta = scratch.path().join("meta");
        std::fs::create_dir_all(&meta).unwrap();
        let key = "the office|2005|series";
        let path = scratch.path().join("search-cache.json");
        let rows = [metadata::Candidate {
            kind: metadata::Kind::SERIES,
            title: "The Office (2005)".to_string(),
            year: 2005,
            provider: "tvmaze".to_string(),
            provider_id: "tvmaze-office".to_string(),
            poster: String::new(),
            synopsis: String::new(),
            genres: String::new(),
            length: String::new(),
            episode_count: 188,
        }];
        let mut cache = metadata::SearchCache::load_from(&path);
        cache.put(key, &rows, &[]).expect("seed the picker cache");
        assert!(
            metadata::SearchCache::load_from(&path).get(key).is_some(),
            "fixture: the picker cache holds one saved answer"
        );

        clear_metadata_cache(&meta.to_string_lossy(), &path).expect("ClearMetadataCache");

        assert!(
            metadata::SearchCache::load_from(&path).get(key).is_none(),
            "the picker still serves its saved answer after Clear metadata"
        );
    }

    #[test]
    fn export_import_cores_round_trip() {
        let src = Scratch::new("export-src");
        seed_sixteen_hex(
            src.path(),
            "movie|alpha|2001",
            metadata::cache::CACHE_SCHEMA,
            r#"{"title":"Alpha"}"#,
        );
        seed_sixteen_hex(
            src.path(),
            "series|beta|2020|2",
            metadata::cache::CACHE_SCHEMA,
            r#"{"title":"Beta"}"#,
        );

        let dest_dir = Scratch::new("export-dest");
        let dest = dest_dir.path().join(export_default_name_now());
        let dest_s = dest.to_string_lossy().into_owned();
        let n = export_bundle_to(
            &src.path().to_string_lossy(),
            &dest_s,
            metadata::cache::CACHE_SCHEMA,
            APP_VERSION,
        )
        .unwrap();
        assert_eq!(n, 2);

        let dst = Scratch::new("import-dst");
        let dst_s = dst.path().to_string_lossy().into_owned();
        let st = import_bundle_from(&dst_s, &dest_s, metadata::cache::CACHE_SCHEMA).unwrap();
        assert_eq!((st.added, st.total, st.present), (2, 2, 0));
        let again = import_bundle_from(&dst_s, &dest_s, metadata::cache::CACHE_SCHEMA).unwrap();
        assert_eq!((again.added, again.present), (0, 2));
        assert_eq!(bundle::count(&dst_s).unwrap(), 2);

        let nope = Scratch::new("import-nope").path().join("nope.json");
        assert!(
            import_bundle_from(
                &dst_s,
                &nope.to_string_lossy(),
                metadata::cache::CACHE_SCHEMA
            )
            .is_err()
        );
    }

    #[test]
    fn on_select_scan_speed_applies_live() {
        let scratch = Scratch::new("scan-speed");
        let cfg = scratch.path().join("Potflix").join("config.json");
        for profile in [
            app_support::config::SCAN_SPEED_GENTLE,
            app_support::config::SCAN_SPEED_BALANCED,
            app_support::config::SCAN_SPEED_TURBO,
        ] {
            let status = on_select_scan_speed(&cfg, profile).unwrap();
            assert_eq!(
                metadata_http::current_rate_profile(),
                profile,
                "live profile after selecting {profile}"
            );
            assert_eq!(
                status,
                format!("Scan speed: {}", scan_segment_label(profile))
            );
            let saved = app_support::config::load_config_from(&cfg).unwrap();
            assert_eq!(saved.scan_speed, profile, "persisted {profile}");
        }
        // A mixed-case selection normalises before it reaches the limiter.
        assert_eq!(
            on_select_scan_speed(&cfg, " GENTLE ").unwrap(),
            "Scan speed: Gentle"
        );
        assert_eq!(metadata_http::current_rate_profile(), "gentle");
    }

    #[test]
    fn json_bundle_filter_encoding() {
        let f = json_bundle_filter();
        assert!(f.len() >= 4 && f[f.len() - 1] == 0 && f[f.len() - 2] == 0);
        let nuls = f.iter().filter(|&&u| u == 0).count();
        assert!(nuls >= 4, "filter holds {nuls} NULs");
    }

    #[test]
    fn open_file_name_w_matches_binding_size() {
        assert_eq!(
            std::mem::size_of::<OpenFileNameW>(),
            std::mem::size_of::<OPENFILENAMEW>()
        );
    }

    #[test]
    fn launched_via_autostart_flag() {
        assert!(!launched_via_autostart(&["potflix.exe".to_string()]));
        assert!(launched_via_autostart(&[
            "potflix.exe".to_string(),
            AUTOSTART_ARG.to_string()
        ]));
    }

    #[test]
    fn should_show_main_window_policy() {
        assert!(should_show_main_window(&["potflix.exe".to_string()]));
        assert!(!should_show_main_window(&[
            "potflix.exe".to_string(),
            AUTOSTART_ARG.to_string()
        ]));
    }

    #[test]
    fn startup_command_for_flagged() {
        let exe = r"C:\Program Files\Potflix\potflix.exe";
        assert_eq!(
            startup_command_for(exe),
            format!("\"{exe}\" {AUTOSTART_ARG}")
        );
    }

    #[test]
    fn startup_value_enabled_tolerant() {
        let exe = r"C:\Program Files\Potflix\potflix.exe";
        assert!(startup_value_enabled(&startup_command_for(exe), exe));
        assert!(startup_value_enabled(&startup_value_for(exe), exe));
        assert!(!startup_value_enabled(r#""C:\Other\app.exe""#, exe));
        assert!(!startup_value_enabled("", exe));
    }
}
