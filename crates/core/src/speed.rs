//! Download speed (bytes/sec) and ETA estimation.
//!
//! The downloader observes cumulative byte counts at irregular intervals; this
//! turns a stream of `(bytes_done, instant)` ticks into a *smoothed* throughput
//! (an exponentially-weighted moving average, EWMA) and an ETA. Smoothing keeps
//! the displayed speed steady across bursty chunk arrivals.
//!
//! Time is injected (an `f64` seconds-since-epoch / monotonic clock value) so the
//! math is fully deterministic and unit-testable: feed synthetic ticks, assert
//! the smoothed speed + ETA. The queue feeds it `Instant`s in production.

/// Smoothing factor for the EWMA in [0, 1]: weight given to the newest sample.
/// Higher reacts faster but is noisier; ~0.3 balances steadiness and response.
const EWMA_ALPHA: f64 = 0.3;

/// Tracks smoothed download speed for a single in-flight transfer.
///
/// Cheap and `Clone`-free; create one per download attempt. Call [`observe`] on
/// every progress tick with the cumulative bytes downloaded and the current time
/// (seconds, monotonic). It returns the smoothed speed in bytes/sec, or `None`
/// until at least two ticks have been seen (need an interval to measure).
///
/// [`observe`]: SpeedTracker::observe
#[derive(Debug, Clone, Default)]
pub struct SpeedTracker {
    last: Option<(u64, f64)>,
    /// Current EWMA of bytes/sec, `None` until the first interval is measured.
    ewma: Option<f64>,
}

impl SpeedTracker {
    pub fn new() -> Self {
        SpeedTracker::default()
    }

    /// Record a cumulative `bytes_done` reading at time `now_secs` (monotonic
    /// seconds). Returns the smoothed speed in bytes/sec once measurable.
    ///
    /// Non-advancing ticks (same or fewer bytes, or no elapsed time) update the
    /// last reading but don't perturb the average — they carry no new throughput
    /// information.
    pub fn observe(&mut self, bytes_done: u64, now_secs: f64) -> Option<u64> {
        if let Some((prev_bytes, prev_t)) = self.last {
            let dt = now_secs - prev_t;
            let db = bytes_done.saturating_sub(prev_bytes);
            if dt > 0.0 && db > 0 {
                let inst = db as f64 / dt;
                self.ewma = Some(match self.ewma {
                    Some(prev) => EWMA_ALPHA * inst + (1.0 - EWMA_ALPHA) * prev,
                    None => inst,
                });
                self.last = Some((bytes_done, now_secs));
            } else {
                // Stalled or out-of-order tick: advance the position but keep the
                // average. (A genuinely stalled transfer keeps its last speed; the
                // queue surfaces stalls via timeouts, not via the speed readout.)
                self.last = Some((bytes_done.max(prev_bytes), now_secs.max(prev_t)));
            }
        } else {
            self.last = Some((bytes_done, now_secs));
        }
        self.speed_bps()
    }

    /// The current smoothed speed in bytes/sec, or `None` before it's measurable.
    pub fn speed_bps(&self) -> Option<u64> {
        self.ewma.map(|s| s.max(0.0).round() as u64)
    }
}

/// Estimate seconds remaining for a transfer at `speed_bps` given `downloaded`
/// of `total` bytes. Returns `None` when the total or speed is unknown/zero, or
/// the download is already complete (`downloaded >= total`).
pub fn eta_secs(downloaded: u64, total: Option<u64>, speed_bps: Option<u64>) -> Option<u64> {
    let total = total?;
    let speed = speed_bps?;
    if speed == 0 || downloaded >= total {
        return None;
    }
    let remaining = total - downloaded;
    Some((remaining as f64 / speed as f64).ceil() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn needs_two_ticks_before_a_speed_is_known() {
        let mut t = SpeedTracker::new();
        assert_eq!(t.observe(0, 0.0), None, "first tick has no interval");
        // 1 MiB over 1 second → ~1 MiB/s.
        let s = t.observe(1_048_576, 1.0).unwrap();
        assert_eq!(s, 1_048_576);
    }

    #[test]
    fn ewma_smooths_toward_steady_rate() {
        let mut t = SpeedTracker::new();
        t.observe(0, 0.0);
        // Steady 1000 B/s for several ticks → converges to 1000.
        let mut last = 0;
        for i in 1..=10 {
            last = t.observe(1000 * i, i as f64).unwrap();
        }
        // After many equal samples the EWMA sits exactly on the rate.
        assert_eq!(last, 1000);
    }

    #[test]
    fn ewma_reacts_but_lags_a_step_change() {
        let mut t = SpeedTracker::new();
        t.observe(0, 0.0);
        // First interval: 1000 B/s.
        let s1 = t.observe(1000, 1.0).unwrap();
        assert_eq!(s1, 1000);
        // Second interval jumps to 2000 B/s: smoothed value moves partway, not
        // all the way (alpha=0.3 → 0.3*2000 + 0.7*1000 = 1300).
        let s2 = t.observe(3000, 2.0).unwrap();
        assert_eq!(s2, 1300);
    }

    #[test]
    fn stalled_tick_keeps_last_speed() {
        let mut t = SpeedTracker::new();
        t.observe(0, 0.0);
        let s = t.observe(2000, 1.0).unwrap();
        assert_eq!(s, 2000);
        // No new bytes over the next second → speed unchanged (not crashed to 0).
        let s2 = t.observe(2000, 2.0).unwrap();
        assert_eq!(s2, 2000);
    }

    #[test]
    fn eta_basic_division() {
        // 100 of 1000 bytes done at 100 B/s → 900 remaining / 100 = 9s.
        assert_eq!(eta_secs(100, Some(1000), Some(100)), Some(9));
    }

    #[test]
    fn eta_rounds_up() {
        // 950 remaining at 100 B/s = 9.5s → ceil to 10.
        assert_eq!(eta_secs(50, Some(1000), Some(100)), Some(10));
    }

    #[test]
    fn eta_none_without_total_or_speed() {
        assert_eq!(eta_secs(100, None, Some(100)), None, "unknown total");
        assert_eq!(eta_secs(100, Some(1000), None), None, "unknown speed");
    }

    #[test]
    fn zero_speed_yields_no_eta() {
        // Explicitly: zero-speed → eta_secs = None.
        assert_eq!(eta_secs(100, Some(1000), Some(0)), None);
    }

    #[test]
    fn complete_download_has_no_eta() {
        assert_eq!(eta_secs(1000, Some(1000), Some(100)), None);
        assert_eq!(eta_secs(1500, Some(1000), Some(100)), None);
    }
}
