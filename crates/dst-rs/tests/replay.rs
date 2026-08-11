//! Pillar 1 — **deterministic replay**.
//!
//! A DST harness is only useful if a run can be reproduced bit-for-bit from its
//! seed. These tests drive small workloads through the substrate's deterministic
//! sources (seeded RNG, seeded fault schedule / network, single-thread scheduler)
//! and prove: same seed ⇒ identical trace, different seed ⇒ different trace.

use std::cell::RefCell;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::rc::Rc;

use dst_rs::{
    sim_yield_now, Fault, FaultSchedule, Random, SimScheduler, SimulatedNetwork, SimulatedRandom,
};

/// A small mixed workload: draw from the seeded RNG and push each draw through
/// the seeded fault-injectable network, recording a compact per-step trace.
fn run_workload(seed: u64) -> Vec<(u64, u8)> {
    let rng = SimulatedRandom::from_seed(seed);
    let net = SimulatedNetwork::new(FaultSchedule::generate(seed, 48, 35));
    (0..48)
        .map(|_| {
            let draw = rng.next_u64() % 1000;
            // Map the delivery outcome to a small tag so the trace is easy to diff.
            let tag = match net.send(draw) {
                dst_rs::Delivery::Delivered(_) => 0,
                dst_rs::Delivery::Delayed { .. } => 1,
                dst_rs::Delivery::Failed => 2,
                dst_rs::Delivery::Dropped => 3,
            };
            (draw, tag)
        })
        .collect()
}

#[test]
fn same_seed_replays_bit_for_bit() {
    let a = run_workload(2024);
    let b = run_workload(2024);
    assert_eq!(
        a, b,
        "same seed must reproduce the identical workload trace"
    );
    // Non-vacuous: the run actually exercised faults, not an all-clear link.
    assert!(
        a.iter().any(|(_, tag)| *tag != 0),
        "expected some non-trivial fault outcomes in the trace"
    );
}

#[test]
fn different_seed_diverges() {
    let a = run_workload(1);
    let b = run_workload(2);
    assert_ne!(a, b, "distinct seeds should produce different traces");
}

/// The scheduler-level replay guarantee: an interleaving of cooperative tasks on
/// the single-thread `SimScheduler` produces the identical trace — and identical
/// poll-count — every run.
#[test]
fn sim_scheduler_interleaving_replays_identically() {
    fn trace() -> (Vec<u32>, u64) {
        let out = Rc::new(RefCell::new(Vec::new()));
        let mut s = SimScheduler::new();
        for id in 0..4u32 {
            let o = out.clone();
            s.spawn(async move {
                for step in 0..3 {
                    o.borrow_mut().push(id * 10 + step);
                    sim_yield_now().await;
                }
            });
        }
        s.run();
        let steps = s.steps();
        (Rc::try_unwrap(out).unwrap().into_inner(), steps)
    }
    let a = trace();
    let b = trace();
    assert_eq!(
        a, b,
        "scheduler interleaving + poll count must be reproducible"
    );
    assert_eq!(a.0.len(), 12);
}

/// CLAIM (Part B): **same seed reproduces the trace under `SimScheduler`.** A
/// seed-parameterized concurrent workload (each task yields a seed-derived number
/// of times, so the seed shapes the interleaving) is driven through the scheduler
/// and its interleaving trace is hashed. The same seed yields an identical hash;
/// a different seed yields a different one.
#[test]
fn same_seed_reproduces_scheduler_trace_hash() {
    fn trace_hash(seed: u64) -> u64 {
        let rng = SimulatedRandom::from_seed(seed);
        // Seed-derived yield counts steer where each task lands in the FIFO ready
        // queue — i.e. the interleaving the scheduler realizes.
        let counts: Vec<u64> = (0..4).map(|_| rng.next_u64() % 4).collect();
        let log = Rc::new(RefCell::new(Vec::new()));
        let mut s = SimScheduler::new();
        for id in 0..4u64 {
            let log = Rc::clone(&log);
            let n = counts[id as usize];
            s.spawn(async move {
                for step in 0..=n {
                    log.borrow_mut().push((id, step));
                    sim_yield_now().await;
                }
            });
        }
        s.run();
        let mut h = DefaultHasher::new();
        log.borrow().hash(&mut h);
        // Fold in the poll count too — another determinism witness.
        s.steps().hash(&mut h);
        h.finish()
    }

    // Same seed → identical trace hash, run to run.
    for seed in [1u64, 7, 42, 1000] {
        assert_eq!(
            trace_hash(seed),
            trace_hash(seed),
            "seed {seed} must reproduce the identical scheduler trace"
        );
    }

    // Different seeds → the trace diverges (not all seeds collapse to one trace).
    let distinct: std::collections::BTreeSet<u64> = (0..20u64).map(trace_hash).collect();
    assert!(
        distinct.len() > 1,
        "distinct seeds must produce more than one scheduler trace"
    );
}

/// Replaying a schedule from the same seed yields the same fault at every step,
/// so a network built from it behaves identically across two independent runs.
#[test]
fn network_link_behavior_is_seed_reproducible() {
    let outcomes = |seed: u64| -> Vec<Fault> {
        let net = SimulatedNetwork::new(FaultSchedule::generate(seed, 32, 50));
        // `peek` before each send records exactly the fault the link will apply.
        (0..32)
            .map(|_| {
                let f = net.peek();
                let _ = net.send(());
                f
            })
            .collect()
    };
    assert_eq!(outcomes(99), outcomes(99));
    assert_ne!(outcomes(99), outcomes(100));
}
