
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use metadata_http::cancel::Cancel;

const POLL: Duration = Duration::from_millis(10);

/// A parent scope plus the child scopes derived from it.
pub(crate) struct CancelLink {
    parent: Cancel,
    children: Arc<Mutex<Vec<Cancel>>>,
    done: Arc<AtomicBool>,
}

impl CancelLink {
    /// Starts watching `parent`.
    pub(crate) fn new(parent: &Cancel) -> CancelLink {
        let children: Arc<Mutex<Vec<Cancel>>> = Arc::new(Mutex::new(Vec::new()));
        let done = Arc::new(AtomicBool::new(false));
        {
            let parent = parent.clone();
            let children = Arc::clone(&children);
            let done = Arc::clone(&done);
            thread::spawn(move || {
                loop {
                    if parent.err().is_some() {
                        for child in children.lock().unwrap().drain(..) {
                            child.cancel();
                        }
                        return;
                    }
                    if done.load(Ordering::SeqCst) {
                        return;
                    }
                    let _ = parent.wait(POLL);
                }
            });
        }
        CancelLink {
            parent: parent.clone(),
            children,
            done,
        }
    }

    /// Registers `child` so a parent cancel reaches it. A child registered
    /// after the parent (or the link) is done is cancelled at once.
    pub(crate) fn add(&self, child: Cancel) {
        if self.parent.err().is_some() || self.done.load(Ordering::SeqCst) {
            child.cancel();
            return;
        }
        self.children.lock().unwrap().push(child);
    }

    /// Stops the watcher (all children were cancelled or completed).
    pub(crate) fn finish(&self) {
        self.done.store(true, Ordering::SeqCst);
        self.children.lock().unwrap().clear();
    }
}
