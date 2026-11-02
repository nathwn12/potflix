//! `potflix-win` -- the Win32 surface split out of the Go root package: the raw
//! binding table (`go\win32.go`), the tray window (`go\tray.go`), the settings
//! window (`go\window.go` + `go\settings_sections.go`) and the single-instance
//! lock (`go\instance.go`).
//!
//! Workspace seam: the app-support half is its only intra-repo dependency, and
//! `windows-sys` is the only FFI path -- no C toolchain.

pub mod artwork;
pub mod brand;
pub mod search;
pub mod settings;
pub mod snapshot;
pub mod tray;
pub mod win32;
pub mod window;
/// Shared test-only serial for process-global `LOCALAPPDATA`/config state,
/// used by the `search` and `window` test lanes (one lock, no per-module
/// mutexes).
#[cfg(test)]
mod test_env;
