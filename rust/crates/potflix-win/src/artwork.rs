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

/// The source and destination rects for one cover blit into `slot`: the image
/// is scaled to COVER the slot and centre-cropped, so every source fills the
/// tile edge to edge whatever its aspect ratio is -- a landscape still fills a
/// portrait slot instead of letterboxing or being stretched out of shape.
/// `src` is in image pixels, `dst` is `slot` itself. `None` on a degenerate
/// image or slot (nothing to draw).
#[must_use]
pub fn cover_rects(img_w: u32, img_h: u32, slot: win32::Rect) -> Option<(win32::Rect, win32::Rect)> {
    let slot_w = i64::from(slot.right - slot.left);
    let slot_h = i64::from(slot.bottom - slot.top);
    if img_w == 0 || img_h == 0 || slot_w <= 0 || slot_h <= 0 {
        return None;
    }
    let (iw, ih) = (i64::from(img_w), i64::from(img_h));
    // Compare the two aspects without floats: iw/ih > sw/sh <=> iw*sh > ih*sw.
    // The wider side gives up its excess, so the source always carries the
    // slot's aspect and the blit never has a band to leave behind.
    let (src_w, src_h) = if iw * slot_h > ih * slot_w {
        ((ih * slot_w + slot_h / 2) / slot_h, ih)
    } else {
        (iw, (iw * slot_h + slot_w / 2) / slot_w)
    };
    let src_w = src_w.clamp(1, iw);
    let src_h = src_h.clamp(1, ih);
    let src = win32::Rect {
        left: ((iw - src_w) / 2) as i32,
        top: ((ih - src_h) / 2) as i32,
        right: ((iw + src_w) / 2) as i32,
        bottom: ((ih + src_h) / 2) as i32,
    };
    Some((src, slot))
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
    /// keeps the tile's corner shape; the image is scaled to COVER the rect and
    /// centre-cropped (see [`cover_rects`]), so a landscape source fills a
    /// portrait tile instead of letterboxing inside it.
    pub(crate) fn draw(&self, graphics: *mut gp::GpGraphics, rc: win32::Rect, radius: i32) {
        use gp::{
            CombineModeReplace, GdipAddPathArcI, GdipClosePathFigure, GdipCreatePath,
            GdipDeletePath, GdipDrawImageRectRectI, GdipResetClip, GdipSetClipPath,
            FillModeAlternate, UnitPixel,
        };
        if graphics.is_null() {
            return;
        }
        let Some((src, dst)) = cover_rects(self.width, self.height, rc) else {
            return;
        };
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
            GdipDrawImageRectRectI(
                graphics,
                self.image as *mut gp::GpImage,
                dst.left,
                dst.top,
                dst.right - dst.left,
                dst.bottom - dst.top,
                src.left,
                src.top,
                src.right - src.left,
                src.bottom - src.top,
                UnitPixel,
                std::ptr::null(),
                0,
                std::ptr::null_mut(),
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

    /// A cover must COVER its slot: the destination is the slot edge to edge
    /// (no letterbox band, no stretch) and the source is a centre-biased crop
    /// that carries the slot's own aspect. The result rows and the hero cover
    /// both paint through this one geometry, so a landscape poster in a
    /// portrait tile cannot read as a different shape than its neighbours.
    #[test]
    fn a_cover_crops_to_fill_its_slot_whatever_the_source_aspect() {
        // The 32x48 result thumb: the portrait slot the rows use.
        let slot = win32::Rect {
            left: 10,
            top: 20,
            right: 42,
            bottom: 68,
        };
        let slot_ratio = 32.0 / 48.0;

        // A 16:9 still in a portrait slot: crop the sides, never letterbox.
        let (src, dst) = cover_rects(1600, 900, slot).expect("a 16:9 cover has rects");
        assert_eq!(
            (dst.left, dst.top, dst.right, dst.bottom),
            (10, 20, 42, 68),
            "the tile is covered edge to edge, so no band can show"
        );
        assert_eq!(
            (src.top, src.bottom),
            (0, 900),
            "a wide source keeps its full height and gives up the sides"
        );
        assert!(
            (src.left - (1600 - src.right)).abs() <= 1,
            "the crop is centre-biased: {}..{} of 1600",
            src.left,
            src.right
        );
        let src_ratio = f64::from(src.right - src.left) / f64::from(src.bottom - src.top);
        assert!(
            (src_ratio - slot_ratio).abs() < 0.02,
            "the crop carries the slot's aspect {slot_ratio:.3}, got {src_ratio:.3}"
        );

        // A portrait poster (the common case) into the same slot: full width,
        // crop the height.
        for (w, h) in [(2000u32, 3000u32), (913, 1426)] {
            let (src, dst) = cover_rects(w, h, slot).expect("portrait cover has rects");
            assert_eq!((dst.right - dst.left, dst.bottom - dst.top), (32, 48));
            assert_eq!(
                (src.left, src.right),
                (0, w as i32),
                "a tall source keeps its full width"
            );
            assert!(
                (src.top - (h as i32 - src.bottom)).abs() <= 1,
                "the crop is centre-biased: {}..{} of {h}",
                src.top,
                src.bottom
            );
            let src_ratio = f64::from(src.right - src.left) / f64::from(src.bottom - src.top);
            assert!(
                (src_ratio - slot_ratio).abs() < 0.02,
                "{w}x{h} crop carries the slot's aspect {slot_ratio:.3}, got {src_ratio:.3}"
            );
        }

        // A square source is wider than this slot, so it gives up the sides and
        // keeps its full height -- the same rule, not a special case.
        let (src, _) = cover_rects(1000, 1000, slot).expect("square cover has rects");
        assert_eq!((src.top, src.bottom), (0, 1000), "a square keeps its height");
        assert!(
            (src.left - (1000 - src.right)).abs() <= 1,
            "the crop is centre-biased: {}..{} of 1000",
            src.left,
            src.right
        );

        // Degenerate input draws nothing rather than inventing a rect.
        assert!(cover_rects(0, 900, slot).is_none());
        assert!(
            cover_rects(
                1600,
                900,
                win32::Rect {
                    left: 0,
                    top: 0,
                    right: 0,
                    bottom: 48
                }
            )
            .is_none()
        );
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
