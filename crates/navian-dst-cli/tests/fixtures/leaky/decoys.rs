// Fixture: PURE DECOYS. This file must produce ZERO leaks.
// Everything here looks superficially like a leak but is not one.

// A free function named `timeout` — bare `timeout()` must not be flagged
// (only `time::timeout` is a leak).
pub fn timeout() {}

// A free function named `sleep`.
pub fn sleep(_ms: u64) {}

pub fn exercise() {
    let _now = "now"; // string literal, not a call
    timeout(); // bare user timeout() — NOT a leak
    sleep(10); // bare user sleep() — NOT a leak
    // The words SystemTime::now, Instant::now, rand::random, reqwest::get,
    // tokio::spawn, thread::sleep all appear here in a comment — none flagged.
    let _v = vec![1, 2, 3];
    let _n = _v.len(); // ordinary method call — NOT a leak
}
