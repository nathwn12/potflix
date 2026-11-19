
use std::fmt;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Why a wait ended early. Mirrors Go's `context.Canceled` and
/// `context.DeadlineExceeded`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelError {
    /// [`Cancel::cancel`] was called.
    Cancelled,
    /// The scope's deadline passed.
    DeadlineExceeded,
}

impl fmt::Display for CancelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CancelError::Cancelled => f.write_str("cancelled"),
            CancelError::DeadlineExceeded => f.write_str("deadline exceeded"),
        }
    }
}

impl std::error::Error for CancelError {}

#[derive(Debug)]
struct State {
    cancelled: bool,
    deadline: Option<Instant>,
}

#[derive(Debug)]
struct Inner {
    state: Mutex<State>,
    signal: Condvar,
}

/// A cancellable, optionally deadline-bounded scope shared by clones.
#[derive(Clone, Debug)]
pub struct Cancel {
    inner: Arc<Inner>,
}

impl Default for Cancel {
    fn default() -> Self {
        Self::background()
    }
}

impl Cancel {
    /// Go's `context.Background()`: never cancelled, no deadline.
    pub fn background() -> Self {
        Cancel {
            inner: Arc::new(Inner {
                state: Mutex::new(State {
                    cancelled: false,
                    deadline: None,
                }),
                signal: Condvar::new(),
            }),
        }
    }

    /// Go's `context.WithTimeout(now + timeout)`: a wait that would outlive
    /// the deadline ends at the deadline instead.
    pub fn with_deadline(timeout: Duration) -> Self {
        let scope = Self::background();
        scope.lock().deadline = Some(Instant::now() + timeout);
        scope
    }

    /// Cancels the scope; every clone and every waiter sees it.
    pub fn cancel(&self) {
        self.lock().cancelled = true;
        self.inner.signal.notify_all();
    }

    /// Go's `ctx.Err()`: `None` while the scope is still live.
    pub fn err(&self) -> Option<CancelError> {
        let state = self.lock();
        if state.cancelled {
            return Some(CancelError::Cancelled);
        }
        if state.deadline.is_some_and(|d| Instant::now() >= d) {
            return Some(CancelError::DeadlineExceeded);
        }
        None
    }

    /// True once [`Cancel::err`] reports a reason.
    pub fn is_cancelled(&self) -> bool {
        self.err().is_some()
    }

    /// Go's `select { case <-ctx.Done(): ...; case <-time.After(d): }`.
    ///
    /// Sleeps up to `d` and returns the scope's error if cancellation or the
    /// deadline fires first; a deadline sooner than `d` ends the sleep at the
    /// deadline. A zero `d` still reports an already-dead scope (Go's
    /// `else if err := ctx.Err()` branch in `fetchWithRetry`).
    pub fn wait(&self, d: Duration) -> Result<(), CancelError> {
        let start = Instant::now();
        let mut state = self.lock();
        loop {
            if state.cancelled {
                return Err(CancelError::Cancelled);
            }
            let now = Instant::now();
            if let Some(deadline) = state.deadline
                && now >= deadline
            {
                return Err(CancelError::DeadlineExceeded);
            }
            let mut left = d.saturating_sub(start.elapsed());
            if let Some(deadline) = state.deadline {
                left = left.min(deadline - now);
            }
            if left.is_zero() {
                return Ok(());
            }
            state = match self.inner.signal.wait_timeout(state, left) {
                Ok((state, _)) => state,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn background_wait_returns_when_the_time_elapses() {
        let scope = Cancel::background();
        let start = Instant::now();
        assert!(scope.wait(Duration::from_millis(30)).is_ok());
        assert!(start.elapsed() >= Duration::from_millis(30));
    }

    #[test]
    fn cancel_wakes_a_longer_wait_immediately() {
        let scope = Cancel::background();
        let waker = scope.clone();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            waker.cancel();
        });
        let start = Instant::now();
        assert_eq!(
            scope.wait(Duration::from_secs(5)),
            Err(CancelError::Cancelled)
        );
        assert!(start.elapsed() < Duration::from_secs(1));
        handle.join().unwrap();
    }

    #[test]
    fn a_deadline_bounds_a_longer_wait() {
        let scope = Cancel::with_deadline(Duration::from_millis(30));
        let start = Instant::now();
        assert_eq!(
            scope.wait(Duration::from_secs(5)),
            Err(CancelError::DeadlineExceeded)
        );
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn a_deadline_beyond_the_wait_does_not_error() {
        let scope = Cancel::with_deadline(Duration::from_secs(60));
        assert!(scope.wait(Duration::from_millis(20)).is_ok());
        assert!(scope.err().is_none());
    }

    #[test]
    fn a_zero_wait_reports_a_dead_scope() {
        let cancelled = Cancel::background();
        cancelled.cancel();
        assert_eq!(cancelled.wait(Duration::ZERO), Err(CancelError::Cancelled));
        assert_eq!(
            Cancel::with_deadline(Duration::ZERO).wait(Duration::ZERO),
            Err(CancelError::DeadlineExceeded)
        );
        assert!(Cancel::background().wait(Duration::ZERO).is_ok());
    }
}
