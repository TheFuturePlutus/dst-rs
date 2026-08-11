//! Fixture app for the `dst migrate` integration test.
//!
//! `RateLimiter` is a named-field struct whose **inherent, `&self` methods**
//! read wall-clock time (`SystemTime::now()…as_millis()`), a monotonic instant
//! (`Instant::now()`), and sleep (`tokio::time::sleep`). Those are exactly the
//! three rewritable TIME leaks `dst migrate` v1 handles.
//!
//! `boot_timestamp` is a **free function** with the same `SystemTime` millis
//! idiom — a decoy that migrate must SKIP and report, never rewrite (there is
//! no `self` to hang a clock off of).
//!
//! Before migration this compiles against real `std`/`tokio`. After migration
//! the struct gains a `time: Arc<dyn dst_rs::Time>` field defaulted to the real
//! production clock in `new`, and the three leaks route through `self.time`.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// A toy rate limiter that stamps events with wall-clock time.
pub struct RateLimiter {
    tokens: u32,
    last_ms: i64,
}

impl RateLimiter {
    /// Constructor. Note: takes no clock and does NOT read time here, so the
    /// seam stays simple — every existing caller of `new` is unchanged after
    /// migration because the injected `time` field defaults to production.
    pub fn new(tokens: u32) -> Self {
        Self { tokens, last_ms: 0 }
    }

    pub fn tokens(&self) -> u32 {
        self.tokens
    }

    pub fn last_ms(&self) -> i64 {
        self.last_ms
    }

    /// LEAK (rewritable): wall-clock millis idiom → `self.time.now_ms()`.
    pub fn now_ms(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64
    }

    /// LEAK (rewritable): `Instant::now()` → `self.time.instant_now()`.
    pub fn mono(&self) -> Instant {
        Instant::now()
    }

    /// LEAK (rewritable): `tokio::time::sleep(d).await` → `self.time.sleep(d).await`.
    pub async fn throttle(&self, d: Duration) {
        tokio::time::sleep(d).await;
    }
}

/// LEAK (must be SKIPPED + reported): a free function has no `self`, so the
/// wall-clock read here cannot be routed through a struct field. migrate must
/// leave it exactly as-is and report it as manual/agent work.
pub fn boot_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}
