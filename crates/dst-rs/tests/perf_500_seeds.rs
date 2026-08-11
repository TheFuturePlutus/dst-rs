//! Part B backing test — **500-seed performance**.
//!
//! Mirrors the `kv_chaos` example's 500-seed sweep (a correct persist-before-ack
//! store under crash/fault injection — same constants as the example) and times
//! it. Wall-time is REPORTED, not hard-asserted at a tight bound: a `<1s` gate is
//! CI-flaky (shared runners, cold caches), so we assert only a generous ceiling
//! and print the measured number for the record.

use std::collections::HashMap;
use std::time::Instant;

use dst_rs::{Fault, FaultSchedule, Random, SimulatedRandom};

/// The `kv_chaos` correct store: persist to durable FIRST, then cache, then ack.
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
    fn put(&mut self, key: u64, value: u64) -> (u64, u64) {
        self.durable.insert(key, value);
        self.cache.insert(key, value);
        (key, value)
    }
    fn get(&self, key: u64) -> Option<u64> {
        self.cache.get(&key).copied()
    }
    fn recover(&mut self) {
        self.cache = self.durable.clone();
    }
}

/// Run the same 500-seed sweep the `kv_chaos` example runs, returning the total
/// invariant violations (must be zero for the correct store).
fn run_sweep() -> u64 {
    const SEEDS: u64 = 500;
    const STEPS: usize = 40;
    const DENSITY: u64 = 35;
    const KEYSPACE: u64 = 8;

    let mut violations = 0u64;
    for seed in 0..SEEDS {
        let rng = SimulatedRandom::from_seed(seed);
        let schedule = FaultSchedule::generate(seed, STEPS, DENSITY);
        let mut store = KvStore::new();
        let mut acknowledged: HashMap<u64, u64> = HashMap::new();

        for step in 0..STEPS {
            match schedule.at(step) {
                Fault::CrashRestart => {
                    store.recover();
                    for (&k, &v) in &acknowledged {
                        if store.get(k) != Some(v) {
                            violations += 1;
                        }
                    }
                }
                _ => {
                    let key = rng.next_u64() % KEYSPACE;
                    let value = rng.next_u64();
                    let (k, v) = store.put(key, value);
                    acknowledged.insert(k, v);
                    if store.get(k) != Some(v) {
                        violations += 1;
                    }
                }
            }
        }
        store.recover();
        for (&k, &v) in &acknowledged {
            if store.get(k) != Some(v) {
                violations += 1;
            }
        }
    }
    violations
}

/// CLAIM (Part B): the 500-seed kv_chaos sweep runs quickly. We REPORT the
/// measured wall-time and assert only a generous, non-flaky ceiling.
#[test]
fn kv_chaos_500_seed_sweep_is_fast() {
    let start = Instant::now();
    let violations = run_sweep();
    let elapsed = start.elapsed();

    assert_eq!(
        violations, 0,
        "the correct store must lose no acknowledged write"
    );
    println!(
        "kv_chaos 500-seed sweep wall-time: {:.3} ms",
        elapsed.as_secs_f64() * 1000.0
    );
    assert!(
        elapsed.as_secs_f64() < 10.0,
        "500-seed sweep took {:.3}s — well over the 10s ceiling",
        elapsed.as_secs_f64()
    );
}
