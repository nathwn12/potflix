
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows_sys::Win32::Storage::FileSystem::GetFileAttributesW;

/// `FILE_ATTRIBUTE_REPARSE_POINT` (`reparse_windows.go:9`).
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

/// `INVALID_FILE_ATTRIBUTES`: `GetFileAttributesW`'s failure sentinel.
const INVALID_FILE_ATTRIBUTES: u32 = 0xFFFF_FFFF;

/// `dirHasReparseAttribute`: whether `path` carries the Windows reparse
/// attribute, without following it.
pub(super) fn dir_has_reparse_attribute(path: &Path) -> bool {
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);
    let attrs = unsafe { GetFileAttributesW(wide.as_ptr()) };
    attrs != INVALID_FILE_ATTRIBUTES && (attrs & FILE_ATTRIBUTE_REPARSE_POINT) != 0
}
