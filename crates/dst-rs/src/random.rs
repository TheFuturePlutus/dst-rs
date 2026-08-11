//! `Random` trait — RNG and UUID generation.
//!
//! Per ADR-0022. Production impl uses `rand::thread_rng` and `Uuid::new_v4`.
//! Simulation impl uses `ChaCha20Rng` seeded from the harness, producing
//! deterministic sequences and deterministic UUIDs.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use rand::{RngCore, SeedableRng};
use rand_chacha::ChaCha20Rng;
use uuid::Uuid;

/// Randomness abstraction.
///
/// All engine code that needs random bytes, integers, or UUIDs goes
/// through this trait. Production builds use [`ProductionRandom`] (real
/// thread-local entropy); simulation builds use [`SimulatedRandom`]
/// (seeded ChaCha20, fully deterministic).
pub trait Random: Send + Sync + 'static {
    /// Next pseudo-random `u64`.
    fn next_u64(&self) -> u64;

    /// Generate a UUID. Production produces v4 (random); simulation produces
    /// a deterministic v4-shape UUID derived from the seed and an internal
    /// counter — different invocations always produce different UUIDs, but
    /// the sequence is fully reproducible from the seed.
    fn next_uuid(&self) -> Uuid;

    /// Fisher-Yates shuffle in place.
    fn shuffle_u64(&self, slice: &mut [u64]);
}

// ─────────────────────────────────────────────────────────────────────
// ProductionRandom
// ─────────────────────────────────────────────────────────────────────

/// Production-mode `Random` — uses thread-local OS entropy.
///
/// Each call grabs a `thread_rng`; this is approximately free (it's a
/// thread-local handle). UUIDs are standard v4 (random).
#[derive(Debug, Default, Clone, Copy)]
pub struct ProductionRandom;

impl Random for ProductionRandom {
    #[inline]
    fn next_u64(&self) -> u64 {
        rand::thread_rng().next_u64()
    }

    #[inline]
    fn next_uuid(&self) -> Uuid {
        Uuid::new_v4()
    }

    fn shuffle_u64(&self, slice: &mut [u64]) {
        use rand::seq::SliceRandom;
        slice.shuffle(&mut rand::thread_rng());
    }
}

// ─────────────────────────────────────────────────────────────────────
// SimulatedRandom
// ─────────────────────────────────────────────────────────────────────

/// Simulation-mode `Random` — seeded ChaCha20.
///
/// Same seed produces identical sequences. UUIDs are constructed from
/// `(rng_state, counter)` so they're unique within a run but reproducible
/// across runs from the same seed.
pub struct SimulatedRandom {
    rng: Mutex<ChaCha20Rng>,
    uuid_counter: AtomicU64,
}

impl SimulatedRandom {
    /// Create a new RNG seeded from the given 64-bit seed.
    pub fn from_seed(seed: u64) -> Self {
        Self {
            rng: Mutex::new(ChaCha20Rng::seed_from_u64(seed)),
            uuid_counter: AtomicU64::new(0),
        }
    }

    /// Create a new RNG with a 256-bit seed (full ChaCha20 seed surface).
    pub fn from_full_seed(seed: [u8; 32]) -> Self {
        Self {
            rng: Mutex::new(ChaCha20Rng::from_seed(seed)),
            uuid_counter: AtomicU64::new(0),
        }
    }
}

impl Random for SimulatedRandom {
    fn next_u64(&self) -> u64 {
        self.rng
            .lock()
            .expect("SimulatedRandom mutex poisoned")
            .next_u64()
    }

    fn next_uuid(&self) -> Uuid {
        // Deterministic UUID: pull 8 random bytes from the RNG, append an
        // 8-byte monotonic counter. Result is a distinct UUID per call but
        // reproducible across runs from the same seed.
        let counter = self.uuid_counter.fetch_add(1, Ordering::Relaxed);
        let rng_bytes = self
            .rng
            .lock()
            .expect("SimulatedRandom mutex poisoned")
            .next_u64();

        let mut bytes = [0u8; 16];
        bytes[0..8].copy_from_slice(&rng_bytes.to_le_bytes());
        bytes[8..16].copy_from_slice(&counter.to_le_bytes());

        // Mark as a v4 UUID so external systems reading it see a
        // well-formed v4 (deterministic content, but valid v4 shape).
        bytes[6] = (bytes[6] & 0x0F) | 0x40; // version 4
        bytes[8] = (bytes[8] & 0x3F) | 0x80; // RFC 4122 variant

        Uuid::from_bytes(bytes)
    }

    fn shuffle_u64(&self, slice: &mut [u64]) {
        use rand::seq::SliceRandom;
        slice.shuffle(&mut *self.rng.lock().expect("SimulatedRandom mutex poisoned"));
    }
}

// ─────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn production_random_produces_different_values() {
        let r = ProductionRandom;
        let a = r.next_u64();
        let b = r.next_u64();
        // Astronomically unlikely to collide twice in a row.
        assert_ne!(a, b);
    }

    #[test]
    fn production_random_uuid_unique() {
        let r = ProductionRandom;
        let mut seen = HashSet::new();
        for _ in 0..1000 {
            assert!(seen.insert(r.next_uuid()));
        }
    }

    #[test]
    fn simulated_random_same_seed_same_sequence() {
        let r1 = SimulatedRandom::from_seed(42);
        let r2 = SimulatedRandom::from_seed(42);
        for _ in 0..100 {
            assert_eq!(r1.next_u64(), r2.next_u64());
        }
    }

    #[test]
    fn simulated_random_different_seed_different_sequence() {
        let r1 = SimulatedRandom::from_seed(1);
        let r2 = SimulatedRandom::from_seed(2);
        let v1: Vec<_> = (0..32).map(|_| r1.next_u64()).collect();
        let v2: Vec<_> = (0..32).map(|_| r2.next_u64()).collect();
        assert_ne!(v1, v2);
    }

    #[test]
    fn simulated_random_uuid_deterministic() {
        let r1 = SimulatedRandom::from_seed(7);
        let r2 = SimulatedRandom::from_seed(7);
        for _ in 0..100 {
            assert_eq!(r1.next_uuid(), r2.next_uuid());
        }
    }

    #[test]
    fn simulated_random_uuid_unique_within_run() {
        let r = SimulatedRandom::from_seed(99);
        let mut seen = HashSet::new();
        for _ in 0..10_000 {
            assert!(seen.insert(r.next_uuid()));
        }
    }

    #[test]
    fn simulated_random_uuid_is_valid_v4_shape() {
        let r = SimulatedRandom::from_seed(0);
        for _ in 0..100 {
            let u = r.next_uuid();
            // Version 4
            assert_eq!(u.get_version_num(), 4);
            // RFC 4122 variant
            assert_eq!(u.get_variant(), uuid::Variant::RFC4122);
        }
    }

    #[test]
    fn simulated_random_shuffle_deterministic() {
        let r1 = SimulatedRandom::from_seed(123);
        let r2 = SimulatedRandom::from_seed(123);
        let mut v1: Vec<u64> = (0..50).collect();
        let mut v2: Vec<u64> = (0..50).collect();
        r1.shuffle_u64(&mut v1);
        r2.shuffle_u64(&mut v2);
        assert_eq!(v1, v2);
    }

    #[test]
    fn production_random_is_object_safe() {
        let _: std::sync::Arc<dyn Random> = std::sync::Arc::new(ProductionRandom);
    }

    #[test]
    fn simulated_random_is_object_safe() {
        let _: std::sync::Arc<dyn Random> = std::sync::Arc::new(SimulatedRandom::from_seed(0));
    }
}
