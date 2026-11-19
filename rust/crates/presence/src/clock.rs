
/// A monotonic interval clock (`gate.go:76-96`). The zero value is ready to use.
#[derive(Debug, Clone, Default)]
pub(crate) struct IntervalClock {
    /// Whether the first observation has been folded in.
    started: bool,
    /// The last wall-clock value fed in.
    last: i64,
    /// The monotonic interval value, same units (ms).
    value: i64,
}

impl IntervalClock {
    /// Folds one wall-clock observation in and returns the monotonic value. The
    /// first observation seeds `value = now`, so on a forward-only clock
    /// `tick(now) == now`.
    pub(crate) fn tick(&mut self, now: i64) -> i64 {
        if !self.started {
            self.started = true;
            self.last = now;
            self.value = now;
            return self.value;
        }
        let delta = now - self.last;
        if delta > 0 {
            self.value += delta;
        }
        self.last = now;
        self.value
    }
}
