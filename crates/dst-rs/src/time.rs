//! `Time` trait — wall-clock, monotonic time, and async sleep.
//!
//! Production impl is zero-overhead (direct calls to
//! `std::time::SystemTime::now`, `std::time::Instant::now`, `tokio::time::sleep`).
//! Simulation impl reads from a harness-controlled clock and suspends
//! sleepers on a `tokio::sync::watch` channel.

use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::watch;

/// Wall-clock and monotonic time abstraction.
///
/// All engine code that needs the current time, a monotonic instant, or
/// to wait for a duration goes through this trait. Production builds use
/// [`ProductionTime`] (real OS time); simulation builds use
/// [`SimulatedTime`] (harness-controlled clock).
#[async_trait::async_trait]
pub trait Time: Send + Sync + 'static {
    /// Current wall-clock time in milliseconds since the Unix epoch.
    fn now_ms(&self) -> i64;

    /// Monotonic instant, suitable for measuring elapsed durations.
    ///
    /// Production: `std::time::Instant::now()`. Simulation: a synthetic
    /// instant derived from the harness clock; monotonically advances
    /// as the harness advances simulated time.
    fn instant_now(&self) -> Instant;

    /// Suspend the calling task for at least `duration`.
    ///
    /// Production: `tokio::time::sleep`. Simulation: returns when the
    /// harness clock advances past `now_ms() + duration`. Simulation
    /// sleep does **not** consume real wall-clock time; the harness can
    /// advance the clock arbitrarily fast.
    async fn sleep(&self, duration: Duration);
}

// ─────────────────────────────────────────────────────────────────────
// ProductionTime
// ─────────────────────────────────────────────────────────────────────

/// Production-mode `Time` — direct calls to `std` and `tokio`.
///
/// Zero-overhead: trait methods compile to the same code as the underlying
/// `std`/`tokio` calls when used through generics.
#[derive(Debug, Default, Clone, Copy)]
pub struct ProductionTime;

#[async_trait::async_trait]
impl Time for ProductionTime {
    #[inline]
    fn now_ms(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }

    #[inline]
    fn instant_now(&self) -> Instant {
        Instant::now()
    }

    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await
    }
}

// ─────────────────────────────────────────────────────────────────────
// SimulatedTime
// ─────────────────────────────────────────────────────────────────────

/// Simulation-mode `Time` — harness-controlled clock.
///
/// The clock starts at `start_ms` (Unix epoch milliseconds). The harness
/// calls [`SimulatedTime::advance`] or [`SimulatedTime::set_to`] to move
/// the clock forward; sleepers waiting on the clock wake when the clock
/// passes their deadline.
///
/// Internally uses a `tokio::sync::watch` channel so that multiple advances
/// in quick succession coalesce — sleepers always observe the latest value
/// and there is no missed-wakeup race.
pub struct SimulatedTime {
    /// Real `Instant` captured at construction; used to synthesize
    /// monotonic instants that track the simulated clock.
    real_epoch: Instant,
    /// Simulated clock value at construction (Unix ms). Used as the
    /// reference point for `instant_now`.
    start_ms: AtomicI64,
    /// Sender side of the clock-broadcast channel.
    clock_tx: watch::Sender<i64>,
}

impl SimulatedTime {
    /// Create a new simulated clock starting at `start_ms` (Unix milliseconds).
    pub fn new(start_ms: i64) -> Self {
        let (tx, _) = watch::channel(start_ms);
        Self {
            real_epoch: Instant::now(),
            start_ms: AtomicI64::new(start_ms),
            clock_tx: tx,
        }
    }

    /// Advance the simulated clock by `delta` milliseconds.
    ///
    /// All sleepers whose wake time is now in the past will be woken on
    /// their next poll. This call does not wait; it returns immediately.
    pub fn advance_ms(&self, delta: i64) {
        self.clock_tx.send_modify(|c| *c += delta);
    }

    /// Set the simulated clock to an absolute Unix-millisecond value.
    ///
    /// Uses `send_replace` so the value is updated even when no sleepers
    /// are currently subscribed (matches `advance_ms`'s `send_modify`
    /// semantics).
    pub fn set_to_ms(&self, ms: i64) {
        self.clock_tx.send_replace(ms);
    }

    /// Get the current simulated clock value (Unix ms).
    pub fn current_ms(&self) -> i64 {
        *self.clock_tx.borrow()
    }
}

#[async_trait::async_trait]
impl Time for SimulatedTime {
    #[inline]
    fn now_ms(&self) -> i64 {
        *self.clock_tx.borrow()
    }

    fn instant_now(&self) -> Instant {
        let elapsed_ms = (self.now_ms() - self.start_ms.load(Ordering::Relaxed)).max(0);
        self.real_epoch + Duration::from_millis(elapsed_ms as u64)
    }

    async fn sleep(&self, duration: Duration) {
        let wake_at = self.now_ms() + duration.as_millis() as i64;
        let mut rx = self.clock_tx.subscribe();
        loop {
            if *rx.borrow() >= wake_at {
                return;
            }
            // `changed()` returns when the watch value mutates. If the
            // sender has been dropped, returns Err — exit cleanly.
            if rx.changed().await.is_err() {
                return;
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn production_time_now_ms_is_positive() {
        let t = ProductionTime;
        let now = t.now_ms();
        assert!(
            now > 1_700_000_000_000,
            "now_ms should be a recent unix-ms timestamp, got {now}"
        );
    }

    #[tokio::test]
    async fn production_time_instant_now_is_monotonic() {
        let t = ProductionTime;
        let a = t.instant_now();
        let b = t.instant_now();
        assert!(b >= a, "instant_now must be monotonic");
    }

    #[tokio::test(start_paused = true)]
    async fn production_time_sleep_advances() {
        // Using `start_paused = true` makes tokio's mock clock control sleep,
        // so this test runs instantly.
        let t = ProductionTime;
        let before = tokio::time::Instant::now();
        t.sleep(Duration::from_millis(50)).await;
        let elapsed = before.elapsed();
        assert!(elapsed >= Duration::from_millis(50));
    }

    #[test]
    fn simulated_time_starts_at_provided_value() {
        let t = SimulatedTime::new(1_700_000_000_000);
        assert_eq!(t.now_ms(), 1_700_000_000_000);
        assert_eq!(t.current_ms(), 1_700_000_000_000);
    }

    #[test]
    fn simulated_time_advance() {
        let t = SimulatedTime::new(0);
        t.advance_ms(1_000);
        assert_eq!(t.now_ms(), 1_000);
        t.advance_ms(500);
        assert_eq!(t.now_ms(), 1_500);
    }

    #[test]
    fn simulated_time_set_to() {
        let t = SimulatedTime::new(0);
        t.set_to_ms(42_000);
        assert_eq!(t.now_ms(), 42_000);
    }

    #[test]
    fn simulated_time_instant_now_advances_with_clock() {
        let t = SimulatedTime::new(0);
        let i0 = t.instant_now();
        t.advance_ms(1_000);
        let i1 = t.instant_now();
        let delta = i1.duration_since(i0);
        assert_eq!(delta, Duration::from_millis(1_000));
    }

    #[tokio::test]
    async fn simulated_sleep_returns_when_clock_advances() {
        use std::sync::Arc;
        let t = Arc::new(SimulatedTime::new(0));
        let t2 = t.clone();

        let sleeper = tokio::spawn(async move {
            t2.sleep(Duration::from_millis(500)).await;
            t2.now_ms()
        });

        // Give the sleeper a chance to subscribe.
        tokio::task::yield_now().await;
        // Advance past the sleep duration.
        t.advance_ms(500);

        let observed_ms = sleeper.await.expect("sleeper task panicked");
        assert!(
            observed_ms >= 500,
            "expected clock >= 500ms, got {observed_ms}"
        );
    }

    #[tokio::test]
    async fn simulated_sleep_can_be_woken_by_multiple_advances() {
        use std::sync::Arc;
        let t = Arc::new(SimulatedTime::new(0));
        let t2 = t.clone();

        let sleeper = tokio::spawn(async move {
            t2.sleep(Duration::from_millis(300)).await;
        });

        tokio::task::yield_now().await;
        t.advance_ms(100);
        tokio::task::yield_now().await;
        t.advance_ms(100);
        tokio::task::yield_now().await;
        t.advance_ms(100);

        // Sleeper should still be waiting (only 300ms total, edge case).
        // Advance once more to clear the deadline.
        t.advance_ms(1);

        sleeper.await.expect("sleeper task panicked");
    }

    /// Production impl is `Send + Sync + 'static` so `Arc<dyn Time>` works.
    #[test]
    fn production_time_is_object_safe() {
        let _: std::sync::Arc<dyn Time> = std::sync::Arc::new(ProductionTime);
    }

    #[test]
    fn simulated_time_is_object_safe() {
        let _: std::sync::Arc<dyn Time> = std::sync::Arc::new(SimulatedTime::new(0));
    }
}
