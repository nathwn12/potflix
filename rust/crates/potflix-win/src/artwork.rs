//! `potflix-win::artwork` -- the cover cache and the decoded GDI+ image.
//!
//! The card already carries a resolved https poster URL. The preview paints
//! that poster only when its bytes are locally available: the live path
//! fetches the URL on a worker (never on the UI thread, never before
//! window-first-paint) and writes one atomically-created cache file under
//! `%LOCALAPPDATA%\Potflix\meta\artwork`. Tests and the capture harness seed
//! the same cache with explicit bytes, so no test touches the network.
//!
//! Missing bytes are an absence, not a branded fallback: [`is_cached`] is the
//! paint path's gate, and `SurfacePreview::art` only turns on when it answers
//! true. GDI+ is the decoder/drawer already linked by this crate; there is no
//! new dependency.

use std::collections::HashSet;
use std::io::Read;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::Graphics::GdiPlus as gp;

use crate::win32::{self, utf16_z};

/// The cache directory name under the app's metadata root.
const CACHE_DIR: &str = "artwork";
/// The hard cap for one cover body. Posters are far below this; a wrong URL
/// must not grow the cache or the UI thread's memory without bound.
const MAX_COVER_BYTES: u64 = 8 << 20;
/// One background fetch's whole-request budget.
const FETCH_TIMEOUT: Duration = Duration::from_secs(8);
/// One background fetch's cancellation deadline (slightly longer than the
/// transport timeout so the transport reports first).
const FETCH_DEADLINE: Duration = Duration::from_secs(10);

/// URLs currently being fetched. The live poll can call `ensure_async` on
/// every tick; this set keeps it to one worker per URL.
static IN_FLIGHT: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

/// FNV-1a 64-bit: stable cache names without a new crate.
fn url_hash(url: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for b in url.as_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Whether `url` is an artwork URL the Discord card can actually carry: http(s)
/// and within the wire budget. Empty/invalid answers are an absence.
#[must_use]
pub fn is_http_url(url: &str) -> bool {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or("");
    !rest.is_empty()
        && !rest.starts_with('/')
        && !rest.chars().any(char::is_whitespace)
        && url.len() <= 300
}

/// The cache file for one URL. It is deterministic, so a completed fetch is
/// found by the next paint without a second copy in memory.
#[must_use]
pub fn cache_path(url: &str) -> PathBuf {
    app_support::config::metadata_dir()
        .join(CACHE_DIR)
        .join(format!("{:016x}.img", url_hash(url)))
}

/// Whether a non-empty cache file exists for `url`.
#[must_use]
pub fn is_cached(url: &str) -> bool {
    if !is_http_url(url) {
        return false;
    }
    std::fs::metadata(cache_path(url)).is_ok_and(|m| m.is_file() && m.len() > 0)
}

/// Seed the cache for one URL. The bytes are written beside the final file and
/// renamed, so a paint can never observe a half-written cover. This is the
/// deterministic seam the tests and capture harness use; the live path uses
/// [`ensure_async`].
pub fn put_cached(url: &str, bytes: &[u8]) -> std::io::Result<PathBuf> {
    if !is_http_url(url) || bytes.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "artwork: not a usable cover",
        ));
    }
    let path = cache_path(url);
    let dir = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(
        ".{:016x}.{}.tmp",
        url_hash(url),
        std::process::id()
    ));
    std::fs::write(&tmp, bytes)?;
    if path.exists() {
        let _ = std::fs::remove_file(&path);
    }
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

/// Start one background fetch for `url` unless it is cached or already in
/// flight. The worker posts [`crate::window::WM_APP_ARTWORK_READY`] so the
/// visible card repaints as soon as the bytes land. A failure leaves the cache
/// absent; the preview stays collapsed instead of inventing a tile.
pub fn ensure_async(url: &str) {
    if !is_http_url(url) || is_cached(url) {
        return;
    }
    {
        let mut in_flight = IN_FLIGHT
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !in_flight.insert(url.to_string()) {
            return;
        }
    }
    let url = url.to_string();
    std::thread::spawn(move || {
        let cancel = metadata_http::Cancel::with_deadline(FETCH_DEADLINE);
        let client = metadata_http::HttpClient::new(FETCH_TIMEOUT);
        let outcome = (|| -> Result<(), String> {
            let response = client
                .get_with_accept(&cancel, &url, "image/*")
                .map_err(|e| format!("request failed: {e}"))?;
            let status = response.status();
            if !status.is_success() {
                return Err(format!("HTTP {}", status.as_u16()));
            }
            let mut body = response.into_body();
            let mut bytes = Vec::new();
            body.as_reader()
                .take(MAX_COVER_BYTES)
                .read_to_end(&mut bytes)
                .map_err(|e| format!("body read failed: {e}"))?;
            if bytes.is_empty() {
                return Err("empty body".to_string());
            }
            put_cached(&url, &bytes).map_err(|e| format!("cache write failed: {e}"))?;
            Ok(())
        })();
        if let Err(reason) = outcome {
            crate::window::app_logf(&format!("artwork: fetch {url}: {reason}"));
        }
        IN_FLIGHT
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&url);
        let hwnd = crate::window::ui().main_hwnd as HWND;
        if !hwnd.is_null() {
            // SAFETY: asynchronous post only; no window state is touched here.
            unsafe {
                win32::PostMessageW(hwnd, crate::window::WM_APP_ARTWORK_READY, 0, 0);
            }
        }
    });
}

/// One decoded cover. The GDI+ image lives until the frame's paint returns;
/// [`Drop`] disposes it exactly once.
pub struct Cover {
    image: *mut gp::GpBitmap,
    pub width: u32,
    pub height: u32,
}

impl Cover {
    /// Load a cached cover for `url`, or `None` when the cache has no decodable
    /// image. A cache file that no longer decodes (truncated fetch, wrong
    /// bytes) is deleted so the next paint retries the fetch instead of
    /// serving a permanent absence. The caller owns the decoded handle for
    /// the duration of one paint.
    #[must_use]
    pub fn load(url: &str) -> Option<Self> {
        if !is_cached(url) {
            return None;
        }
        if !crate::window::gdiplus_start() {
            return None;
        }
        let path = cache_path(url);
        let wide = utf16_z(&path.to_string_lossy());
        let mut image: *mut gp::GpBitmap = std::ptr::null_mut();
        // SAFETY: `wide` is a live null-terminated path; `image` is written only
        // by GDI+ and checked before use.
        let status = unsafe { win32::GdipCreateBitmapFromFile(wide.as_ptr(), &mut image) };
        if status != 0 || image.is_null() {
            // Undecodable bytes poison the slot forever: drop the file so the
            // next paint refetches instead of missing forever.
            let _ = std::fs::remove_file(&path);
            return None;
        }
        if status != 0 || image.is_null() {
            return None;
        }
        let mut width = 0u32;
        let mut height = 0u32;
        // SAFETY: `image` is a live bitmap from the call above.
        let ok = unsafe {
            win32::GdipGetImageWidth(image as *mut gp::GpImage, &mut width) == 0
                && win32::GdipGetImageHeight(image as *mut gp::GpImage, &mut height) == 0
        };
        if !ok || width == 0 || height == 0 {
            // SAFETY: `image` is live and is released exactly once on failure.
            unsafe { win32::GdipDisposeImage(image as *mut gp::GpImage) };
            return None;
        }
        Some(Self {
            image,
            width,
            height,
        })
    }

    /// Draw the cover into `rc` through the live GDI+ context. A rounded clip
    /// keeps the tile's corner shape; the image is scaled to fill the rect.
    pub(crate) fn draw(&self, graphics: *mut gp::GpGraphics, rc: win32::Rect, radius: i32) {
        use gp::{
            CombineModeReplace, GdipAddPathArcI, GdipClosePathFigure, GdipCreatePath,
            GdipDeletePath, GdipDrawImageRectI, GdipResetClip, GdipSetClipPath, FillModeAlternate,
        };
        if graphics.is_null() || rc.right <= rc.left || rc.bottom <= rc.top {
            return;
        }
        let mut path: *mut gp::GpPath = std::ptr::null_mut();
        // SAFETY: every handle is created and released within this call.
        unsafe {
            win32::GdipSetInterpolationMode(graphics, win32::InterpolationModeHighQualityBicubic);
            if GdipCreatePath(FillModeAlternate, &mut path) != 0 || path.is_null() {
                return;
            }
            let r = radius.clamp(0, (rc.right - rc.left).min(rc.bottom - rc.top) / 2);
            let d = r * 2;
            if r > 0 {
                GdipAddPathArcI(path, rc.left, rc.top, d, d, 180.0, 90.0);
                GdipAddPathArcI(path, rc.right - d, rc.top, d, d, 270.0, 90.0);
                GdipAddPathArcI(path, rc.right - d, rc.bottom - d, d, d, 0.0, 90.0);
                GdipAddPathArcI(path, rc.left, rc.bottom - d, d, d, 90.0, 90.0);
                GdipClosePathFigure(path);
                GdipSetClipPath(graphics, path, CombineModeReplace);
            }
            GdipDrawImageRectI(
                graphics,
                self.image as *mut gp::GpImage,
                rc.left,
                rc.top,
                rc.right - rc.left,
                rc.bottom - rc.top,
            );
            if r > 0 {
                GdipResetClip(graphics);
            }
            GdipDeletePath(path);
        }
    }
}

impl Drop for Cover {
    fn drop(&mut self) {
        // SAFETY: `image` is live and this is its only disposal.
        unsafe { win32::GdipDisposeImage(self.image as *mut gp::GpImage) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_gate_rejects_empty_non_http_and_over_budget() {
        assert!(is_http_url("https://example.com/poster.jpg"));
        assert!(is_http_url("http://example.com/poster.jpg"));
        assert!(!is_http_url(""));
        assert!(!is_http_url("file:///cover.png"));
        assert!(!is_http_url("https://"));
        assert!(!is_http_url(&format!("https://example.com/{}", "x".repeat(300))));
    }

    #[test]
    fn cache_roundtrip_is_atomic_and_url_keyed() {
        crate::test_env::EnvGuard::with_scratch("artwork-cache", |_dir| {
            let a = "https://example.com/a.jpg";
            let b = "https://example.com/b.jpg";
            assert!(!is_cached(a));
            let path = put_cached(a, b"not an image yet").unwrap();
            assert_eq!(path, cache_path(a));
            assert!(is_cached(a));
            assert!(!is_cached(b), "one URL never aliases another");
            assert_eq!(std::fs::read(&path).unwrap(), b"not an image yet");
        });
    }
}
