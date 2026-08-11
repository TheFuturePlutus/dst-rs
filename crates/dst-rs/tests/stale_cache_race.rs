//! Part A backing test: the cache-invalidation **stale-read race**, reproduced
//! through the deterministic [`SimScheduler`].
//!
//! This mirrors the `stale_cache_race` example (kept as its own compact model,
//! the same way `catches_planted_bug` carries its own ledger next to the
//! `kv_chaos` example). It proves, end-to-end:
//!
//!   * the harness FINDS a reproducible failing seed for the planted (unfenced)
//!     populate, and the failure REPLAYS deterministically (same seed ⇒ same
//!     violation);
//!   * the version-fenced (correct) populate HOLDS the read-after-write invariant
//!     across the whole seed campaign;
//!   * the failure is genuinely INTERLEAVING-DEPENDENT — it appears under some
//!     scheduler interleavings and not others — which is what makes it a real
//!     concurrency bug and proves the SCHEDULER (not a hand-sequenced order) is
//!     driving the interleaving.

use std::cell::RefCell;
use std::rc::Rc;

use dst_rs::{
    sim_yield_now, Invariant, InvariantEngine, Random, SimScheduler, SimulatedRandom, Violation,
};

#[derive(Clone, Copy, PartialEq, Eq)]
struct Versioned {
    version: u64,
}

struct Store {
    version: u64,
}
impl Store {
    fn new() -> Self {
        Self { version: 1 }
    }
    fn read(&self) -> Versioned {
        Versioned {
            version: self.version,
        }
    }
    fn commit(&mut self) -> u64 {
        self.version += 1;
        self.version
    }
    fn version(&self) -> u64 {
        self.version
    }
}

struct World {
    store: Store,
    cache: Option<Versioned>,
    acked_version: u64,
}

#[derive(Clone, Copy)]
enum Variant {
    Correct,
    Planted,
}

#[derive(Clone, Copy)]
struct Snapshot {
    served_version: Option<u64>,
    acked_version: u64,
}

fn invariants() -> InvariantEngine<Snapshot> {
    InvariantEngine::new(vec![Invariant::new(
        "read_after_write_serves_no_older_version",
        |s: &Snapshot| match s.served_version {
            Some(v) => v >= s.acked_version,
            None => true,
        },
    )])
}

/// One Reader+Writer interleaving under the scheduler. The seed sets each task's
/// yield counts; the scheduler owns the actual interleaving.
fn run_scenario(seed: u64, variant: Variant) -> Option<Violation> {
    let rng = SimulatedRandom::from_seed(seed);
    let reader_lead = (rng.next_u64() % 3) as usize;
    let reader_gap = 1 + (rng.next_u64() % 3) as usize;
    let writer_lead = (rng.next_u64() % 5) as usize;

    let world = Rc::new(RefCell::new(World {
        store: Store::new(),
        cache: None,
        acked_version: 1,
    }));

    let mut sched = SimScheduler::new();

    {
        let world = Rc::clone(&world);
        sched.spawn(async move {
            for _ in 0..reader_lead {
                sim_yield_now().await;
            }
            let fetched = {
                let w = world.borrow();
                if w.cache.is_some() {
                    return;
                }
                w.store.read()
            };
            for _ in 0..reader_gap {
                sim_yield_now().await;
            }
            let mut w = world.borrow_mut();
            match variant {
                Variant::Planted => w.cache = Some(fetched),
                Variant::Correct => {
                    if w.store.version() == fetched.version {
                        w.cache = Some(fetched);
                    }
                }
            }
        });
    }

    {
        let world = Rc::clone(&world);
        sched.spawn(async move {
            for _ in 0..writer_lead {
                sim_yield_now().await;
            }
            let mut w = world.borrow_mut();
            let new_version = w.store.commit();
            w.acked_version = new_version;
            w.cache = None; // invalidate
        });
    }

    sched.run();

    let snapshot = {
        let w = world.borrow();
        Snapshot {
            served_version: w.cache.map(|e| e.version),
            acked_version: w.acked_version,
        }
    };
    invariants().check(0, &snapshot)
}

const SEEDS: u64 = 500;

/// CLAIM (Part A): the harness finds a reproducible failing seed for the planted
/// variant, and the same seed replays the identical failure.
#[test]
fn harness_finds_and_replays_a_stale_read_in_the_planted_variant() {
    let failing = (0..SEEDS).find(|&seed| run_scenario(seed, Variant::Planted).is_some());
    let seed = failing.expect("the planted variant must fail on some seed in 0..500");
    println!("stale_cache_race: first failing planted seed = {seed}");

    // Determinism / replay: the same seed reproduces the identical violation.
    let a = run_scenario(seed, Variant::Planted);
    let b = run_scenario(seed, Variant::Planted);
    assert!(a.is_some(), "the failing seed must reproduce a violation");
    assert_eq!(a, b, "same seed must replay the identical violation");
    assert_eq!(
        a.unwrap().invariant,
        "read_after_write_serves_no_older_version"
    );
}

/// CLAIM (Part A): the version-fenced (correct) variant holds the invariant
/// across the entire seed campaign.
#[test]
fn correct_variant_holds_across_the_seed_campaign() {
    let violations = (0..SEEDS)
        .filter(|&seed| run_scenario(seed, Variant::Correct).is_some())
        .count();
    assert_eq!(
        violations, 0,
        "the version-fenced reader must never serve a stale version"
    );
}

/// CLAIM (Part A): the failure is interleaving-dependent — it appears under some
/// scheduler interleavings and NOT others. This is the proof that the scheduler
/// (not a hand-sequenced order) drives the race: if the planted variant failed on
/// every seed, the outcome would not depend on the interleaving at all.
#[test]
fn stale_read_is_interleaving_dependent() {
    let failures = (0..SEEDS)
        .filter(|&seed| run_scenario(seed, Variant::Planted).is_some())
        .count() as u64;
    assert!(
        failures > 0,
        "the planted variant must fail on at least one interleaving"
    );
    assert!(
        failures < SEEDS,
        "the planted variant must PASS on some interleavings too (got {failures}/{SEEDS}) — \
         otherwise the failure would not be interleaving-dependent"
    );
}
