// Fixture: ATOMIC ordering source (ADVISORY confidence).

use std::sync::atomic::{AtomicU64, Ordering};

pub fn touch(x: &AtomicU64) -> u64 {
    // LEAK: Ordering::Relaxed — unordered atomic (ADVISORY).
    x.store(1, Ordering::Relaxed);
    // LEAK: a second relaxed load.
    x.load(Ordering::Relaxed)
}

// ── Decoy: must NOT be flagged ──

pub enum Mode {
    Relaxed,
}

pub fn decoys() -> Mode {
    // A user enum variant named `Relaxed` (no `Ordering` parent) — NOT a leak.
    Mode::Relaxed
}
