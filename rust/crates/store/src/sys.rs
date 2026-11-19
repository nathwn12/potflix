
#[cfg(not(windows))]
mod other;
#[cfg(not(windows))]
pub(crate) use other::open_store_file;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(crate) use windows::open_store_file;
