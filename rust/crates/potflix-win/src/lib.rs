
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
