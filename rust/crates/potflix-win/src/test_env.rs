
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

/// The one serial for every `LOCALAPPDATA`/config/store/mirror test in this crate.
static ENV_SERIAL: Mutex<()> = Mutex::new(());

fn lock_serial() -> MutexGuard<'static, ()> {
    ENV_SERIAL.lock().unwrap_or_else(PoisonError::into_inner)
}

fn try_lock_serial() -> Option<MutexGuard<'static, ()>> {
    ENV_SERIAL.try_lock().ok()
}

fn unique_dir(tag: &str) -> PathBuf {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    std::env::temp_dir().join(format!("potflix-env-{tag}-{}-{n}", std::process::id()))
}

/// Holds [`ENV_SERIAL`] and, unless only [`EnvGuard::hold`] was used, a
/// scratch `LOCALAPPDATA` root. Dropping restores the previous variable value,
/// the previous live-mirror value, and removes the scratch dir, so the
/// restore is panic-safe. The mirror is read under the serial (lock first,
/// then snapshot), so the snapshot can never catch a mid-flight publish from
/// another lane.
pub struct EnvGuard {
    _serial: MutexGuard<'static, ()>,
    prev: Option<OsString>,
    dir: Option<PathBuf>,
    mirror: app_support::config::CardSpecs,
}

impl EnvGuard {
    /// Hold the serial without touching the variable: for tests that only
    /// read the real store/mirror and must not observe a mid-flight repoint.
    /// The lock comes first and the previous value is read under it: reading
    /// before locking would snapshot a root another lane is about to replace.
    pub fn hold() -> Self {
        let serial = lock_serial();
        let prev = std::env::var_os(app_support::config::LOCAL_APP_DATA_ENV);
        let mirror = app_support::config::live_card_specs();
        Self {
            _serial: serial,
            prev,
            dir: None,
            mirror,
        }
    }

    /// Try to hold the serial without blocking: `None` while another lane
    /// holds it. The exclusion proof below uses this instead of sleeps.
    pub fn try_hold() -> Option<Self> {
        let serial = try_lock_serial()?;
        let prev = std::env::var_os(app_support::config::LOCAL_APP_DATA_ENV);
        let mirror = app_support::config::live_card_specs();
        Some(Self {
            _serial: serial,
            prev,
            dir: None,
            mirror,
        })
    }

    /// Hold the serial with `LOCALAPPDATA` pointed at a fresh scratch dir.
    pub fn scratch(tag: &str) -> Self {
        let mut guard = Self::hold();
        let dir = unique_dir(tag);
        std::fs::create_dir_all(&dir).unwrap();
        // SAFETY: the serial above excludes every other test that reads this
        // variable or the store; the previous value is restored in `Drop`.
        unsafe {
            std::env::set_var(app_support::config::LOCAL_APP_DATA_ENV, &dir);
        }
        guard.dir = Some(dir);
        guard
    }

    /// Re-point the variable elsewhere while keeping the serial (and the
    /// scratch dir, still removed on drop): for the save-failure tests that
    /// point the root at a regular file so the atomic save cannot proceed.
    pub fn repoint(&mut self, path: &Path) {
        // SAFETY: same serial as `scratch`; restored in `Drop`.
        unsafe {
            std::env::set_var(app_support::config::LOCAL_APP_DATA_ENV, path);
        }
    }

    /// The scratch root ([`EnvGuard::scratch`] only).
    pub fn dir(&self) -> &Path {
        self.dir.as_deref().expect("hold() carries no scratch dir")
    }

    /// Hold the serial, run `f` under a fresh scratch root, then restore.
    /// Panic-safe: the guard drops during unwind.
    pub fn with_scratch<T>(tag: &str, f: impl FnOnce(&Path) -> T) -> T {
        let guard = Self::scratch(tag);
        let out = f(guard.dir());
        drop(guard);
        out
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        app_support::config::set_live_card_specs(self.mirror.clone());
        match self.prev.clone() {
            Some(value) => unsafe {
                std::env::set_var(app_support::config::LOCAL_APP_DATA_ENV, value);
            },
            None => unsafe {
                std::env::remove_var(app_support::config::LOCAL_APP_DATA_ENV);
            },
        }
        if let Some(dir) = self.dir.take() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// The serial excludes concurrent repoints and concurrent mirror publishes:
/// while one lane holds the guard, another lane's `try_hold` fails; once the
/// guard drops, the variable and the mirror are both restored. No sleeps, no
/// timing: exclusion is proven by the lock itself.
#[test]
fn env_serial_excludes_concurrent_repoint_and_mirror_publish() {
    let lane_a = EnvGuard::scratch("lane-a");
    let root_a = lane_a.dir().to_path_buf();
    assert_eq!(
        std::env::var_os(app_support::config::LOCAL_APP_DATA_ENV),
        Some(root_a.clone().into_os_string()),
        "lane A holds its own root"
    );
    assert!(
        EnvGuard::try_hold().is_none(),
        "the serial must exclude a second lane while the first holds it"
    );
    let prev = lane_a.prev.clone();
    let mirror_a = lane_a.mirror.clone();
    drop(lane_a);
    let recheck = EnvGuard::hold();
    assert_eq!(
        std::env::var_os(app_support::config::LOCAL_APP_DATA_ENV),
        prev,
        "drop restores the previous root"
    );
    assert_eq!(
        app_support::config::live_card_specs(),
        mirror_a,
        "drop restores the previous mirror"
    );
    assert!(
        EnvGuard::try_hold().is_none(),
        "holding the serial excludes try_hold re-entry on this thread"
    );
    drop(recheck);
    assert!(
        EnvGuard::try_hold().is_some(),
        "releasing the serial frees it"
    );
}
