
use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::FromRawHandle;
use std::path::{Component, Path, PathBuf};

use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_ALWAYS,
};

/// `os.OpenFile` (and so Go's `longPath`) only applies the `\\?\` prefix to an
/// absolute path at least this many bytes long.
const LONG_PATH_THRESHOLD: usize = 248;

/// Opens `path` read-write, creating it when it is absent, sharing it with
/// readers, writers AND deleters.
pub(crate) fn open_store_file(path: &Path) -> io::Result<File> {
    let name = long_path(path);
    let mut wide: Vec<u16> = name.as_os_str().encode_wide().collect();
    wide.push(0);

    // SAFETY: `wide` is a NUL-terminated UTF-16 buffer that outlives the call,
    // every argument is a plain integer constant or null (no template file, no
    // security attributes), and the returned handle is checked before use.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_ALWAYS,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `CreateFileW` returned an owned, open, read-write handle and
    // nothing else in this process holds a copy of it, so `File` may own it.
    Ok(unsafe { File::from_raw_handle(handle) })
}

/// Applies the `\\?\` prefix `os.OpenFile` applies to a long absolute path,
/// because `CreateFileW` is handed the name verbatim -- and only then: the
/// threshold, the "already prefixed" test and the missing-volume-name test are
/// the Go original's, and `\\?\` also disables the path normalization a plain
/// name gets, so it must not be added early.
///
/// `to_string_lossy` is used for the length and prefix checks because a Windows
/// path holds WTF-16: a path that is not valid Unicode cannot be prefixed
/// without re-encoding it, and such a path is returned unchanged.
fn long_path(path: &Path) -> PathBuf {
    let text = path.as_os_str().to_string_lossy();
    if text.len() < LONG_PATH_THRESHOLD || text.starts_with(r"\\?\") || volume_name(path).is_empty()
    {
        return path.to_path_buf();
    }
    let prefixed = match text.strip_prefix(r"\\") {
        Some(rest) => OsString::from(format!(r"\\?\UNC\{rest}")),
        None => OsString::from(format!(r"\\?\{}", text)),
    };
    PathBuf::from(prefixed)
}

/// `filepath.VolumeName`: the drive letter or UNC share a path starts with,
/// empty for a relative path.
fn volume_name(path: &Path) -> OsString {
    match path.components().next() {
        Some(Component::Prefix(prefix)) => prefix.as_os_str().to_os_string(),
        _ => OsString::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::long_path;
    use std::path::{Path, PathBuf};

    #[test]
    fn short_paths_are_untouched() {
        for path in [
            r"C:\Users\tester\Potflix\library.idx",
            r"index.pflx",
            r"\\srv\share\a.idx",
        ] {
            assert_eq!(long_path(Path::new(path)), PathBuf::from(path), "{path}");
        }
    }

    #[test]
    fn long_paths_get_the_extended_prefix() {
        let long = format!(r"C:\{}.idx", "a".repeat(260));
        assert!(long.len() >= 248);
        assert_eq!(
            long_path(Path::new(&long)),
            PathBuf::from(format!(r"\\?\{long}"))
        );
    }

    #[test]
    fn long_unc_paths_get_the_unc_prefix() {
        let long = format!(r"\\server\share\{}.idx", "a".repeat(260));
        assert_eq!(
            long_path(Path::new(&long)),
            PathBuf::from(format!(r"\\?\UNC\server\share\{}.idx", "a".repeat(260)))
        );
    }

    #[test]
    fn an_already_prefixed_or_relative_path_is_untouched() {
        let prefixed = format!(r"\\?\{}.idx", "a".repeat(300));
        assert_eq!(long_path(Path::new(&prefixed)), PathBuf::from(&prefixed));
        let relative = format!("{}.idx", "a".repeat(300));
        assert_eq!(long_path(Path::new(&relative)), PathBuf::from(&relative));
    }
}
