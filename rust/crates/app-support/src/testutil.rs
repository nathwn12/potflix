
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Prefix on every scratch directory, so a leaked one is attributable.
pub const SCRATCH_PREFIX: &str = "potflix-app-support-scratch";

static SEQ: AtomicU64 = AtomicU64::new(0);

/// A scratch directory that deletes itself on drop.
pub struct Scratch {
    path: PathBuf,
}

impl Scratch {
    /// Creates `<temp>/potflix-app-support-scratch-<tag>-<pid>-<n>`.
    pub fn new(tag: &str) -> Scratch {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("{SCRATCH_PREFIX}-{tag}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&path).expect("create scratch dir");
        Scratch { path }
    }

    /// The scratch directory itself.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A path under the scratch directory; parents are NOT created, so a test
    /// can prove the atomic write creates them.
    pub fn join(&self, relative: &str) -> PathBuf {
        self.path.join(relative)
    }

    /// Writes a file directly under the scratch directory and returns its path.
    pub fn write(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.path.join(name);
        std::fs::write(&path, contents).expect("write scratch fixture");
        path
    }

    /// Reads a file directly under the scratch directory.
    pub fn read(&self, name: &str) -> Vec<u8> {
        std::fs::read(self.path.join(name)).expect("read scratch file")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[test]
fn scratch_is_unique_and_removed_on_drop() {
    let a = Scratch::new("unique");
    let b = Scratch::new("unique");
    assert_ne!(a.path(), b.path(), "scratch dirs must not collide");
    let path = a.path().to_path_buf();
    assert!(path.is_dir());
    drop(a);
    assert!(!path.exists(), "scratch dir must be removed on drop");
    assert!(b.path().is_dir());
}
