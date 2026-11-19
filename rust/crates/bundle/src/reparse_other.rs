
use std::path::Path;

/// `dirHasReparseAttribute`'s non-Windows stub: always false.
pub(super) fn dir_has_reparse_attribute(_path: &Path) -> bool {
    false
}
