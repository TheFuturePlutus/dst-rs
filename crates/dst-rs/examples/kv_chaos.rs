//! `kv_chaos` — a tiny in-memory KV store under crash/fault injection.
//!
//! The second shop-window example: a *correctly* designed store whose durability
//! invariant HOLDS across every seed. The store writes durably BEFORE
//! acknowledging (the opposite of the planted bug in
//! `tests/catches_planted_bug.rs`), and on a crash it recovers by reloading its
//! in-memory cache from durable storage.
//!
//! Invariant:
//!
//!   > after any crash/recovery, every acknowledged (key, value) reads back with
//!   > exactly the value that was written.
//!
//! Run it:
//!
//! ```text
//! cargo run --example kv_chaos
//! ```

use std::collections::HashMap;

use dst_rs::{Fault, FaultSchedule, Random, SimulatedRandom};

/// A tiny KV store with an in-memory cache over durable storage.
///
/// Correct ordering: `put` persists to `durable` FIRST, then updates the cache and
/// acknowledges. A crash therefore never leaves an acknowledged write unpersisted;
/// recovery just reloads the cache from durable storage.
struct KvStore {
    durable: HashMap<u64, u64>,
    cache: HashMap<u64, u64>,
}

impl KvStore {
    fn new() -> Self {
        Self {
            durable: HashMap::new(),
            cache: HashMap::new(),
        }
    }

    /// Persist-then-acknowledge. Returns the acknowledged (key, value).
    fn put(&mut self, key: u64, value: u64) -> (u64, u64) {
        self.durable.insert(key, value); // durable FIRST
        self.cache.insert(key, value); // then cache
        (key, value) // then ack
    }

    /// Read from the in-memory cache (the hot path).
    fn get(&self, key: u64) -> Option<u64> {
        self.cache.get(&key).copied()
    }

    /// Crash + restart: in-memory cache is lost and rebuilt from durable storage.
    fn recover(&mut self) {
        self.cache = self.durable.clone();
    }
}

fn main() {
    const SEEDS: u64 = 500;
    const STEPS: usize = 40;
    const DENSITY: u64 = 35;
    const KEYSPACE: u64 = 8;

    let mut total_writes: u64 = 0;
    let mut total_crashes: u64 = 0;
    let mut violations: u64 = 0;

    for seed in 0..SEEDS {
        let rng = SimulatedRandom::from_seed(seed);
        let schedule = FaultSchedule::generate(seed, STEPS, DENSITY);
        let mut store = KvStore::new();
        // Ground truth: every key we've been ACKNOWLEDGED for, and its latest value.
        let mut acknowledged: HashMap<u64, u64> = HashMap::new();

        for step in 0..STEPS {
            match schedule.at(step) {
                Fault::CrashRestart => {
                    total_crashes += 1;
                    store.recover();
                    // Invariant check right after recovery: every acknowledged write
                    // must still read back with its written value.
                    for (&k, &v) in &acknowledged {
                        if store.get(k) != Some(v) {
                            eprintln!(
                                "INVARIANT VIOLATED seed {seed} step {step}: key {k} = {:?}, expected {v}",
                                store.get(k)
                            );
                            violations += 1;
                        }
                    }
                }
                // Transient/skew/none: perform a write and read it back.
                _ => {
                    let key = rng.next_u64() % KEYSPACE;
                    let value = rng.next_u64();
                    let (k, v) = store.put(key, value);
                    acknowledged.insert(k, v);
                    total_writes += 1;
                    // Read-your-writes on the hot path.
                    if store.get(k) != Some(v) {
                        eprintln!(
                            "INVARIANT VIOLATED seed {seed} step {step}: read-your-writes failed"
                        );
                        violations += 1;
                    }
                }
            }
        }

        // Final check: recover once more and verify the whole acknowledged set.
        store.recover();
        for (&k, &v) in &acknowledged {
            if store.get(k) != Some(v) {
                violations += 1;
            }
        }
    }

    println!("kv_chaos — a correct KV store under crash/fault injection");
    println!("  seeds run:            {SEEDS}");
    println!("  fault density:        {DENSITY}%, {STEPS} steps/seed");
    println!("  total writes:         {total_writes}");
    println!("  total crash/recovers: {total_crashes}");
    println!("  invariant violations: {violations}");

    assert_eq!(
        violations, 0,
        "a persist-before-ack store must never lose an acknowledged write"
    );
    println!("  RESULT: all {SEEDS} seeds held durability across every crash ✓");
}
