
use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

pub(crate) fn open_store_file(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
}
