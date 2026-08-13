// Fixture: TIME determinism leaks.
// These calls read wall-clock/monotonic time or sleep the OS thread.

use std::time::{Instant, SystemTime};

pub fn stamp() -> SystemTime {
    // LEAK: SystemTime::now
    SystemTime::now()
}

pub fn monotonic() -> Instant {
    // LEAK: Instant::now
    Instant::now()
}

pub fn nap() {
    // LEAK: std::thread::sleep
    std::thread::sleep(std::time::Duration::from_millis(10));
}

pub async fn async_nap() {
    // LEAK: tokio::time::sleep
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
}

pub fn wall() -> i64 {
    // LEAK: chrono::Utc::now
    chrono::Utc::now().timestamp()
}

// ── Decoys in a TIME file: none of these must be flagged ──

pub struct Clock;
impl Clock {
    // A user-defined method also named `now` — NOT a leak.
    pub fn now(&self) -> u64 {
        0
    }
}

pub fn decoys() {
    // The string "now" is not a call.
    let _s = "now";
    // A comment mentioning thread::sleep must not be flagged.
    let clock = Clock;
    let _t = clock.now(); // method call `.now()` — NOT a leak
}
