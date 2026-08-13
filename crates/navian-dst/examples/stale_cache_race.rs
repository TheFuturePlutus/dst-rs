//! `stale_cache_race` — the cache-invalidation stale-read race, reproduced
//! THROUGH the deterministic [`SimScheduler`] (not a hand-sequenced model).
//!
//! This is the race the launch blog opens with. A versioned source-of-truth
//! `Store` sits behind a `Cache`. Two tasks run concurrently and the scheduler
//! interleaves them at their `.await` points:
//!
//!   1. **Reader** — cache miss → fetch `(value, version)` from the store →
//!      (await / yield) → populate the cache.
//!   2. **Writer** — commit a new version to the store → invalidate (clear) the
//!      cache.
//!
//! Invariant (read-after-write consistency):
//!
//!   > a cache read that completes after an acknowledged write must not serve a
//!   > version older than that write.
//!
//! Two variants of the reader's *populate* step:
//!
//!   - **Correct** — the populate is fenced by version (compare-and-set: only
//!     populate if the fetched version is still the latest). The invariant HOLDS
//!     across the whole seed campaign.
//!   - **Planted** — a naive populate with no version check. It FAILS on the
//!     interleaving where the writer's commit+invalidate lands *between* the
//!     reader's fetch and its populate, so the reader repopulates the cache with
//!     the value it grabbed a moment earlier — the old one.
//!
//! The seed parameterizes how many times each task yields, so the SCHEDULER —
//! not a hand-written ordering — decides the interleaving. The failure therefore
//! appears only under *some* interleavings, which is what makes it a real
//! concurrency bug rather than an assertion that always fires.
//!
//! Run it:
//!
//! ```text
//! cargo run --example stale_cache_race
//! ```

use std::cell::RefCell;
use std::rc::Rc;

use navian_dst::{
    sim_yield_now, Invariant, InvariantEngine, Random, SimScheduler, SimulatedRandom, Violation,
};

/// A value tagged with the store version it was written at.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Versioned {
    value: u64,
    version: u64,
}

/// The versioned source of truth (single key). Every commit bumps the version.
struct Store {
    cur: Versioned,
}

impl Store {
    fn new() -> Self {
        Self {
            cur: Versioned {
                value: 100,
                version: 1,
            },
        }
    }

    /// Read the current `(value, version)`.
    fn read(&self) -> Versioned {
        self.cur
    }

    /// Commit a new value; returns the new (monotonically increasing) version.
    fn commit(&mut self, value: u64) -> u64 {
        self.cur = Versioned {
            value,
            version: self.cur.version + 1,
        };
        self.cur.version
    }

    fn version(&self) -> u64 {
        self.cur.version
    }
}

/// The cache in front of the store. `None` == a miss.
#[derive(Default)]
struct Cache {
    entry: Option<Versioned>,
}

impl Cache {
    fn get(&self) -> Option<Versioned> {
        self.entry
    }

    fn put(&mut self, v: Versioned) {
        self.entry = Some(v);
    }

    fn invalidate(&mut self) {
        self.entry = None;
    }
}

/// The world the two tasks share and the invariant is evaluated over.
struct World {
    store: Store,
    cache: Cache,
    /// The latest version the writer has committed AND acknowledged.
    acked_version: u64,
}

/// Which populate strategy the reader uses.
#[derive(Clone, Copy)]
pub enum Variant {
    /// Version-fenced populate (compare-and-set). Holds the invariant.
    Correct,
    /// Naive populate, no version check. The planted bug.
    Planted,
}

/// A snapshot of what a subsequent reader would be served, versus the latest
/// acknowledged write — exactly the read-after-write invariant's inputs.
#[derive(Clone, Copy)]
struct Snapshot {
    served_version: Option<u64>,
    acked_version: u64,
}

fn stale_read_invariants() -> InvariantEngine<Snapshot> {
    InvariantEngine::new(vec![Invariant::new(
        "read_after_write_serves_no_older_version",
        |s: &Snapshot| match s.served_version {
            // A cached entry serves this version to the next read; it must not be
            // older than the acknowledged write.
            Some(v) => v >= s.acked_version,
            // A miss re-fetches the latest — never stale.
            None => true,
        },
    )])
}

/// Drive one Reader+Writer interleaving under the scheduler for `seed`, returning
/// the invariant violation (if the run served a stale version) or `None`.
///
/// The seed picks each task's yield counts; the [`SimScheduler`] then owns the
/// actual interleaving of the two tasks at those `.await` points.
pub fn run_scenario(seed: u64, variant: Variant) -> Option<Violation> {
    let rng = SimulatedRandom::from_seed(seed);
    // Seed-derived yield counts. These shift where each task sits relative to the
    // other in the scheduler's FIFO ready queue, so different seeds realize
    // different interleavings — including the one that lands the writer's
    // commit+invalidate inside the reader's fetch<->populate window.
    let reader_lead = (rng.next_u64() % 3) as usize; // yields before the fetch
    let reader_gap = 1 + (rng.next_u64() % 3) as usize; // yields in the fetch<->populate window
    let writer_lead = (rng.next_u64() % 5) as usize; // yields before the commit

    let world = Rc::new(RefCell::new(World {
        store: Store::new(),
        cache: Cache::default(),
        acked_version: 1,
    }));

    let mut sched = SimScheduler::new();

    // ── Reader: cache miss -> fetch -> (window) -> populate. ──
    {
        let world = Rc::clone(&world);
        sched.spawn(async move {
            for _ in 0..reader_lead {
                sim_yield_now().await;
            }
            // Cache-miss check + fetch. The borrow is dropped before any await.
            let fetched = {
                let w = world.borrow();
                if w.cache.get().is_some() {
                    return; // a hit needs no fetch (never happens with one reader)
                }
                w.store.read()
            };
            // The fetch<->populate window — the interleaving points the scheduler
            // controls. A writer that commits+invalidates here is the race.
            for _ in 0..reader_gap {
                sim_yield_now().await;
            }
            let mut w = world.borrow_mut();
            match variant {
                Variant::Planted => {
                    // BUG: populate unconditionally. If the store advanced and the
                    // cache was invalidated during the window, this reinstates the
                    // stale value we fetched earlier.
                    w.cache.put(fetched);
                }
                Variant::Correct => {
                    // FENCE (compare-and-set): only populate if the version we
                    // fetched is still the latest. If a writer advanced the store
                    // during the window, drop the stale fetch and leave a miss —
                    // the next read re-fetches the current value.
                    if w.store.version() == fetched.version {
                        w.cache.put(fetched);
                    }
                }
            }
        });
    }

    // ── Writer: commit a new version -> acknowledge -> invalidate the cache. ──
    {
        let world = Rc::clone(&world);
        sched.spawn(async move {
            for _ in 0..writer_lead {
                sim_yield_now().await;
            }
            let mut w = world.borrow_mut();
            let new_version = w.store.commit(999);
            w.acked_version = new_version; // the write is now acknowledged
            w.cache.invalidate();
        });
    }

    sched.run();

    let snapshot = {
        let w = world.borrow();
        Snapshot {
            served_version: w.cache.get().map(|e| e.version),
            acked_version: w.acked_version,
        }
    };
    stale_read_invariants().check(0, &snapshot)
}

fn main() {
    const SEEDS: u64 = 500;

    // Correct variant: the version fence holds the invariant across the campaign.
    let mut correct_violations = 0u64;
    for seed in 0..SEEDS {
        if run_scenario(seed, Variant::Correct).is_some() {
            correct_violations += 1;
        }
    }

    // Planted variant: some interleavings land the invalidation inside the
    // fetch<->populate window; the harness catches exactly those.
    let mut planted_failures = 0u64;
    let mut first_failing: Option<u64> = None;
    for seed in 0..SEEDS {
        if run_scenario(seed, Variant::Planted).is_some() {
            planted_failures += 1;
            if first_failing.is_none() {
                first_failing = Some(seed);
            }
        }
    }

    println!("stale_cache_race — cache-invalidation stale-read race, via SimScheduler");
    println!("  seeds run:                  {SEEDS}");
    println!("  correct variant failures:   {correct_violations}");
    println!("  planted variant failures:   {planted_failures} (of {SEEDS} seeds)");
    if let Some(seed) = first_failing {
        println!("  first failing planted seed: {seed}  (replay it forever with SEED={seed})");
    }

    // The example's contract: the correct (fenced) variant never serves a stale
    // version, across every seed.
    assert_eq!(
        correct_violations, 0,
        "a version-fenced reader must never serve a stale version"
    );
    // Non-vacuous, and genuinely a concurrency bug: the planted variant fails on
    // SOME interleavings but not ALL of them (if it failed on none, the harness
    // proves nothing; if on all, it wouldn't be interleaving-dependent).
    assert!(
        planted_failures > 0,
        "the planted variant must fail on at least one interleaving"
    );
    assert!(
        planted_failures < SEEDS,
        "the failure must be interleaving-dependent, not fire on every seed"
    );

    println!(
        "  RESULT: correct holds across all {SEEDS} seeds; planted is caught and \
         interleaving-dependent ✓"
    );
}
