
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// 5 MB: `go\debuglog.go:21`.
pub const DEFAULT_LOG_CAP_BYTES: u64 = 5 * 1024 * 1024;

/// `potflix-debug.log` + `.1` + `.2`: `go\debuglog.go:22`.
pub const DEFAULT_LOG_MAX_FILES: usize = 3;

/// The log file name, beside the executable (`go\main.go:119`).
pub const LOG_FILE_NAME: &str = "potflix-debug.log";

/// The directory holding the running binary, or `"."` when it cannot be read --
/// `go\main.go`'s `exeDir()` fallback, so a log path always resolves.
pub fn exe_dir_or_dot() -> PathBuf {
    crate::clientid::exe_dir().unwrap_or_else(|| PathBuf::from("."))
}

/// The production log path: `<exe dir>\potflix-debug.log`.
pub fn default_log_path() -> PathBuf {
    exe_dir_or_dot().join(LOG_FILE_NAME)
}

/// `format_hms(9, 5, 3) == "09:05:03"`: Go's layout `15:04:05` (24-hour,
/// zero-padded, colon-separated).
pub fn format_hms(hour: u32, minute: u32, second: u32) -> String {
    format!("{hour:02}:{minute:02}:{second:02}")
}

/// The local `HH:mm:ss` for this instant, in the Go format.
pub fn timestamp_now() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs() as i64)
        .unwrap_or(0);
    let (hour, minute, second) = local_hms(seconds).unwrap_or_else(|| utc_hms(seconds));
    format_hms(hour, minute, second)
}

/// Seconds since the epoch split into UTC `(hour, minute, second)`. The fallback
/// when the C runtime cannot map the instant (it always can on Windows); it
/// keeps the timestamp well-formed rather than panicking in a logger.
fn utc_hms(seconds: i64) -> (u32, u32, u32) {
    let day = seconds.rem_euclid(86_400);
    (
        (day / 3_600) as u32,
        ((day % 3_600) / 60) as u32,
        (day % 60) as u32,
    )
}

/// The C runtime's `struct tm`, in UCRT order (all fields are `int`; only the
/// first three are read). `time_t` is 64-bit on this target.
#[repr(C)]
struct CrtTm {
    tm_sec: i32,
    tm_min: i32,
    tm_hour: i32,
    tm_mday: i32,
    tm_mon: i32,
    tm_year: i32,
    tm_wday: i32,
    tm_yday: i32,
    tm_isdst: i32,
}

#[link(name = "ucrt")]
unsafe extern "C" {
    /// Thread-safe `localtime`: `errno_t _localtime64_s(struct tm*, const __time64_t*)`,
    /// 0 on success.
    fn _localtime64_s(tm: *mut CrtTm, time: *const i64) -> i32;
}

/// The local `(hour, minute, second)` for a Unix timestamp.
fn local_hms(seconds: i64) -> Option<(u32, u32, u32)> {
    let mut tm = CrtTm {
        tm_sec: 0,
        tm_min: 0,
        tm_hour: 0,
        tm_mday: 0,
        tm_mon: 0,
        tm_year: 0,
        tm_wday: 0,
        tm_yday: 0,
        tm_isdst: 0,
    };
    // SAFETY: `tm` is a live, correctly sized `struct tm` and `seconds` is a live
    // `__time64_t`; `_localtime64_s` writes only into `tm` and reports failure
    // through its return value instead of panicking.
    let rc = unsafe { _localtime64_s(&mut tm, &seconds) };
    if rc != 0 {
        return None;
    }
    Some((tm.tm_hour as u32, tm.tm_min as u32, tm.tm_sec as u32))
}

/// The port's single debug log (`go\debuglog.go`'s `logger`).
///
/// The handle is opened on construction and appended to; a failed open leaves
/// the logger inert. `log` and `close` take `&self` because the Go logger is
/// shared by pointer and mutates under its own mutex.
pub struct Logger {
    inner: Mutex<Inner>,
}

struct Inner {
    path: PathBuf,
    cap_bytes: u64,
    max_files: usize,
    file: Option<File>,
    size: u64,
}

impl Logger {
    /// Opens `path` for append (creating it), capped at `cap_bytes` with
    /// `max_files` rotation slots. A zero cap disables rotation (unbounded), the
    /// Go tests' `cap 0` case.
    pub fn new(path: impl Into<PathBuf>, cap_bytes: u64, max_files: usize) -> Logger {
        let mut inner = Inner {
            path: path.into(),
            cap_bytes,
            max_files,
            file: None,
            size: 0,
        };
        inner.open_locked();
        Logger {
            inner: Mutex::new(inner),
        }
    }

    /// The production logger: beside the executable, 5 MB x 3.
    pub fn beside_exe() -> Logger {
        Logger::new(
            default_log_path(),
            DEFAULT_LOG_CAP_BYTES,
            DEFAULT_LOG_MAX_FILES,
        )
    }

    /// The log path.
    pub fn path(&self) -> PathBuf {
        self.inner.lock().expect("logger mutex").path.clone()
    }

    /// The current file's byte size, as the logger tracks it.
    pub fn size(&self) -> u64 {
        self.inner.lock().expect("logger mutex").size
    }

    /// Appends `HH:mm:ss msg` + newline, rotating first when the line would push
    /// the file past the cap. A closed or never-opened logger is a no-op.
    pub fn log(&self, message: &str) {
        let line = format!("{} {message}\n", timestamp_now());

        let mut inner = self.inner.lock().expect("logger mutex");
        if inner.file.is_none() {
            return;
        }
        if inner.cap_bytes > 0 && inner.size + line.len() as u64 > inner.cap_bytes {
            inner.rotate_locked();
        }
        if let Some(file) = inner.file.as_mut()
            && let Ok(written) = file.write(line.as_bytes())
        {
            inner.size += written as u64;
        }
    }

    /// Releases the file handle. Rotation hooks are not needed elsewhere.
    pub fn close(&self) {
        let mut inner = self.inner.lock().expect("logger mutex");
        inner.file = None;
    }

    /// `potflix-debug.log.N`.
    pub fn rotated_path(&self, index: usize) -> PathBuf {
        let path = self.inner.lock().expect("logger mutex").path.clone();
        rotated_path_of(&path, index)
    }
}

impl Inner {
    fn open_locked(&mut self) {
        match OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            Ok(file) => {
                self.size = file.metadata().map(|meta| meta.len()).unwrap_or(0);
                self.file = Some(file);
            }
            Err(_) => {
                self.file = None;
            }
        }
    }

    /// Shifts `.N-1` -> `.N`, drops the oldest, and starts a fresh file. A single
    /// slot (`max_files < 2`) has nowhere to rotate to, so it just reopens.
    /// Failures are ignored, as in Go: a log must never fail the app.
    fn rotate_locked(&mut self) {
        self.file = None;
        if self.max_files < 2 {
            self.open_locked();
            return;
        }
        let _ = std::fs::remove_file(rotated_path_of(&self.path, self.max_files - 1));
        for index in (1..self.max_files - 1).rev() {
            let _ = std::fs::rename(
                rotated_path_of(&self.path, index),
                rotated_path_of(&self.path, index + 1),
            );
        }
        let _ = std::fs::rename(&self.path, rotated_path_of(&self.path, 1));
        self.open_locked();
    }
}

fn rotated_path_of(path: &Path, index: usize) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(format!(".{index}"));
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::Scratch;

    /// The `HH:mm:ss` shape, without a regex crate.
    fn assert_hms_prefix(line: &str) {
        let bytes = line.as_bytes();
        assert!(bytes.len() >= 8, "line too short: {line:?}");
        assert_eq!(bytes[2], b':', "line = {line:?}");
        assert_eq!(bytes[5], b':', "line = {line:?}");
        for (index, byte) in bytes[..8].iter().enumerate() {
            if index == 2 || index == 5 {
                continue;
            }
            assert!(byte.is_ascii_digit(), "line = {line:?}");
        }
    }

    #[test]
    fn format_is_go_layout_15_04_05() {
        assert_eq!(format_hms(0, 0, 0), "00:00:00");
        assert_eq!(format_hms(9, 5, 3), "09:05:03");
        assert_eq!(format_hms(23, 59, 59), "23:59:59");
    }

    #[test]
    fn timestamp_now_is_hms_local_shape() {
        assert_hms_prefix(&timestamp_now());
        assert_eq!(timestamp_now().len(), 8);
    }

    #[test]
    fn log_writes_one_hms_prefixed_line_per_event() {
        let scratch = Scratch::new("log-lines");
        let logger = Logger::new(scratch.join("potflix-debug.log"), 0, 1);
        logger.log("first");
        logger.log("second");
        logger.close();

        let text = String::from_utf8(scratch.read("potflix-debug.log")).expect("utf-8 log");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "text = {text:?}");
        assert_hms_prefix(lines[0]);
        assert_eq!(&lines[0][9..], "first");
        assert_eq!(&lines[1][9..], "second");
        assert!(text.ends_with('\n'));
    }

    #[test]
    fn rotation_at_forced_tiny_cap_keeps_current_within_cap() {
        let scratch = Scratch::new("rotate");
        let path = scratch.join("potflix-debug-rotation.log");
        let logger = Logger::new(&path, 256, 3);
        for index in 0..60 {
            logger.log(&format!(
                "rotation test line {index:03} padding padding padding"
            ));
        }
        logger.close();

        assert!(
            rotated_path_of(&path, 1).is_file(),
            "rotation did not produce {}.1",
            path.display()
        );
        let current = std::fs::metadata(&path).expect("rotated current log").len();
        assert!(
            current <= 256,
            "rotated current log {current} bytes exceeds cap 256"
        );
    }

    #[test]
    fn rotation_never_exceeds_the_slot_count() {
        let scratch = Scratch::new("rotate-slots");
        let path = scratch.join("slots.log");
        let logger = Logger::new(&path, 128, 3);
        for index in 0..200 {
            logger.log(&format!("line {index:03} padding padding padding padding"));
        }
        logger.close();

        for index in 1..=2 {
            assert!(
                rotated_path_of(&path, index).is_file(),
                "missing slot {index}"
            );
        }
        assert!(
            !rotated_path_of(&path, 3).exists(),
            "a 3-slot logger must never keep a .3 file"
        );
    }

    #[test]
    fn single_slot_does_not_rotate() {
        let scratch = Scratch::new("rotate-one");
        let path = scratch.join("one.log");
        let logger = Logger::new(&path, 64, 1);
        for index in 0..40 {
            logger.log(&format!("line {index:03} padding padding padding"));
        }
        logger.close();

        assert!(
            !rotated_path_of(&path, 1).exists(),
            "max_files 1 must not rotate"
        );
        assert!(std::fs::metadata(&path).expect("current log").len() > 64);
    }

    #[test]
    fn zero_cap_never_rotates() {
        let scratch = Scratch::new("rotate-off");
        let path = scratch.join("off.log");
        let logger = Logger::new(&path, 0, 3);
        for index in 0..40 {
            logger.log(&format!("line {index:03} padding padding padding"));
        }
        logger.close();

        assert!(!rotated_path_of(&path, 1).exists());
        assert!(std::fs::metadata(&path).expect("current log").len() > 256);
    }

    #[test]
    fn unopenable_path_is_a_silent_no_op() {
        let scratch = Scratch::new("log-inert");
        let logger = Logger::new(scratch.join("no-such-dir").join("potflix-debug.log"), 0, 1);
        logger.log("this line goes nowhere");
        assert_eq!(logger.size(), 0);
        logger.close();
        assert!(!scratch.join("no-such-dir").exists());
    }

    #[test]
    fn closed_logger_is_a_no_op() {
        let scratch = Scratch::new("log-closed");
        let logger = Logger::new(scratch.join("closed.log"), 0, 1);
        logger.log("kept");
        logger.close();
        logger.log("dropped");

        let text = String::from_utf8(scratch.read("closed.log")).expect("utf-8 log");
        assert!(text.contains("kept"));
        assert!(!text.contains("dropped"), "text = {text:?}");
    }

    #[test]
    fn append_continues_an_existing_file() {
        let scratch = Scratch::new("log-append");
        let path = scratch.join("append.log");
        Logger::new(&path, 0, 1).log("one");
        let second = Logger::new(&path, 0, 1);
        second.log("two");
        second.close();

        let text = String::from_utf8(scratch.read("append.log")).expect("utf-8 log");
        assert_eq!(text.lines().count(), 2, "text = {text:?}");
        assert!(text.ends_with('\n'));
    }

    #[test]
    fn default_log_path_is_beside_the_exe() {
        let path = default_log_path();
        assert_eq!(
            path.file_name().and_then(|n| n.to_str()),
            Some(LOG_FILE_NAME)
        );
        assert_eq!(path.parent(), Some(exe_dir_or_dot().as_path()));
        assert_eq!(DEFAULT_LOG_CAP_BYTES, 5 * 1024 * 1024);
        assert_eq!(DEFAULT_LOG_MAX_FILES, 3);
    }
}
