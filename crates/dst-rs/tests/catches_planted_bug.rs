//! The credibility test: a REAL, deliberately-planted bug, caught by the harness.
//!
//! The planted bug is a **durability ordering** defect: a ledger that *acknowledges*
//! a deposit to the caller BEFORE the write is confirmed durable. A correct ledger
//! persists first, then acks. The two implementations are byte-identical except for
//! that ordering — the kind of bug that passes every happy-path test and only
//! surfaces when a durable-write is silently lost in the tiny window between ack and
//! persist.
//!
//! Crucially, the failure here is produced and found by the harness machinery, not by
//! a hand-coded boolean:
//!   - the crash is a **real [`Delivery::Dropped`]** returned by a [`SimulatedNetwork`]
//!     whose link faults come from a **seeded [`FaultSchedule`]** (a `CrashRestart`
//!     maps to a silently-dropped persist);
//!   - the deposit workload runs on the deterministic [`SimScheduler`], which
//!     interleaves the ack↔persist window at a real `.await` point;
//!   - the durability invariant is evaluated by [`InvariantEngine`];
//!   - the failing schedule is shrunk to its minimal trigger by [`ddmin`].
//!
//! What this test proves, end-to-end:
//!   (a) sweeping seeds under fault injection, the harness FINDS a failing seed for
//!       the buggy ledger, and finds NONE for the correct one (so it is
//!       discriminating — it catches the BUG, not merely the presence of faults);
//!   (b) `ddmin` shrinks the failing schedule to a MINIMAL reproducer — a single
//!       `CrashRestart` — the actionable bug report.
//!
//! Determinism: a fixed seed range (0..256) and seeded schedules, so this test is
//! reproducible, never flaky. `SEED=<n>` re-runs a single seed for manual triage.

use std::cell::RefCell;
use std::rc::Rc;

use dst_rs::{
    ddmin, sim_yield_now, Delivery, Fault, FaultSchedule, Invariant, InvariantEngine, SimScheduler,
    SimulatedNetwork, Violation,
};

/// State of the ledger the invariant engine inspects.
///
/// * `acknowledged` — total deposits the caller was told succeeded.
/// * `durable`      — total deposits actually persisted to durable storage.
///
/// A correct system guarantees `durable >= acknowledged`: nothing acknowledged is
/// ever lost. That is the one invariant this harness checks.
#[derive(Clone, Copy, Debug, Default)]
struct LedgerState {
    acknowledged: u64,
    durable: u64,
}

fn durability_invariants() -> InvariantEngine<LedgerState> {
    InvariantEngine::new(vec![Invariant::new(
        "acknowledged_writes_are_durable",
        |s: &LedgerState| s.durable >= s.acknowledged,
    )])
}

/// Whether a persist attempt reached durable storage. Only a crash (`Dropped`,
/// mapped from `Fault::CrashRestart`) loses the write; a transient `Failed` is
/// retried to success and a `Delayed` delivery still lands. This is the durable
/// storage's semantics — the *bug* is the ledger's ack-before-persist ordering,
/// not this mapping.
fn persisted(outcome: Delivery<()>) -> bool {
    !matches!(outcome, Delivery::Dropped)
}

/// Drive the ledger across a fault-injected link on the deterministic scheduler.
/// One deposit of 1 per schedule step. Returns the FIRST invariant violation (the
/// step at which an acknowledged deposit was lost), or `None` if the run stayed
/// correct.
///
/// `buggy == true` acks before confirming the persist (the planted bug); `false`
/// persists (confirms) before acking (correct).
fn run_ledger(schedule: &FaultSchedule, buggy: bool) -> Option<Violation> {
    let net = Rc::new(SimulatedNetwork::new(schedule.clone()));
    let state = Rc::new(RefCell::new(LedgerState::default()));
    let found: Rc<RefCell<Option<Violation>>> = Rc::new(RefCell::new(None));
    let steps = schedule.len();

    let mut sched = SimScheduler::new();
    {
        let net = Rc::clone(&net);
        let state = Rc::clone(&state);
        let found = Rc::clone(&found);
        sched.spawn(async move {
            let engine = durability_invariants();
            for step in 0..steps {
                if buggy {
                    // BUG: acknowledge to the caller BEFORE the persist is
                    // confirmed durable.
                    state.borrow_mut().acknowledged += 1;
                    // The ack↔persist window — the scheduler may interleave here.
                    sim_yield_now().await;
                    if persisted(net.send(())) {
                        state.borrow_mut().durable += 1;
                    }
                    // A crash in this window: acknowledged but never persisted →
                    // durable falls behind acknowledged → lost write.
                } else {
                    // CORRECT: persist (confirm) first, then acknowledge. A crash
                    // before the persist confirms simply leaves the deposit
                    // un-acknowledged (the caller retries); nothing is lost.
                    let ok = persisted(net.send(()));
                    sim_yield_now().await;
                    if ok {
                        let mut s = state.borrow_mut();
                        s.durable += 1;
                        s.acknowledged += 1;
                    }
                }

                let snapshot = *state.borrow();
                if found.borrow().is_none() {
                    if let Some(v) = engine.check(step, &snapshot) {
                        *found.borrow_mut() = Some(v);
                    }
                }
            }
        });
    }
    sched.run();

    let v = found.borrow_mut().take();
    v
}

/// Search a fixed seed range for a schedule that makes the buggy ledger violate
/// its durability invariant. Returns the first failing (seed, schedule).
fn find_failing_seed() -> Option<(u64, FaultSchedule)> {
    const STEPS: usize = 32;
    const DENSITY: u64 = 25;
    (0..256u64).find_map(|seed| {
        let sched = FaultSchedule::generate(seed, STEPS, DENSITY);
        run_ledger(&sched, true).map(|_| (seed, sched))
    })
}

/// A one-screen report of a caught failure — what broke, where, and the minimal
/// fault trace that reproduces it. This is the artifact a DST run should emit.
fn report(seed: u64, violation: &Violation, minimal: &[(usize, Fault)]) -> String {
    let mut out = String::new();
    out.push_str("── DST FAILURE ─────────────────────────────\n");
    out.push_str(&format!("seed:        {seed}\n"));
    out.push_str(&format!(
        "invariant:   {} broke at step {}\n",
        violation.invariant, violation.step
    ));
    out.push_str(&format!("minimal trace ({} fault(s)):\n", minimal.len()));
    for (idx, fault) in minimal {
        out.push_str(&format!("  step {idx:>3}: {fault:?}\n"));
    }
    out.push_str("re-run:      SEED=");
    out.push_str(&seed.to_string());
    out.push_str(" cargo test --test catches_planted_bug\n");
    out.push_str("────────────────────────────────────────────\n");
    out
}

#[test]
fn harness_finds_and_shrinks_the_planted_durability_bug() {
    // Optional manual-triage hook: `SEED=<n>` re-runs exactly one seed.
    if let Ok(seed) = std::env::var("SEED").map(|s| s.parse::<u64>().expect("SEED must be a u64")) {
        let sched = FaultSchedule::generate(seed, 32, 25);
        let v = run_ledger(&sched, true);
        println!("SEED={seed}: buggy ledger => {v:?}");
        return;
    }

    // (a) The harness FINDS a failing seed for the buggy ledger — the failure comes
    //     out of a real SimulatedNetwork Dropped delivery driven on the scheduler.
    let (seed, sched) = find_failing_seed().expect("harness should find a failing seed in 0..256");
    let violation = run_ledger(&sched, true).expect("the failing seed must reproduce");

    // The correct ledger must NOT fail on that same schedule — proves the harness
    // is discriminating (it catches the BUG, not merely the presence of faults).
    assert!(
        run_ledger(&sched, false).is_none(),
        "the correct ledger must survive the schedule that breaks the buggy one"
    );

    // (b) `ddmin` shrinks the failing schedule to a MINIMAL reproducer.
    // Input: every fault point in the schedule (crashes plus irrelevant noise —
    // transient errors and clock skews that the ledger tolerates).
    let fault_points: Vec<(usize, Fault)> = sched
        .fault_steps()
        .into_iter()
        .map(|i| (i, sched.at(i)))
        .collect();

    // Predicate: rebuild a full-length schedule containing ONLY this subset of
    // faults, drive the buggy ledger through the harness again, and check whether
    // it still loses an acknowledged write.
    let reproduces = |subset: &[(usize, Fault)]| -> bool {
        let mut faults = vec![Fault::None; sched.len()];
        for (i, f) in subset {
            faults[*i] = *f;
        }
        run_ledger(&FaultSchedule::from_faults(faults), true).is_some()
    };
    assert!(
        reproduces(&fault_points),
        "the full fault set must reproduce"
    );

    let minimal = ddmin(&fault_points, reproduces);

    // ── The key assertions: the minimal reproducer is a SINGLE crash ────────────
    assert_eq!(
        minimal.len(),
        1,
        "ddmin must isolate the lone essential fault, got {minimal:?}"
    );
    assert!(
        matches!(minimal[0].1, Fault::CrashRestart),
        "the essential fault is a CrashRestart in the ack↔persist window, got {:?}",
        minimal[0]
    );
    // 1-minimality: with no faults at all, the buggy ledger is correct.
    assert!(
        !reproduces(&[]),
        "empty fault set must NOT reproduce (no crash => no lost write)"
    );

    // Emit the actionable report (visible with `--nocapture`).
    print!("{}", report(seed, &violation, &minimal));
}

/// Sanity floor: over the whole fixed seed range, the buggy ledger loses a write on
/// *exactly* the schedules that carry a crash (a dropped persist), and the correct
/// one never does. This makes the "finds a failing seed" claim non-accidental, and
/// pins the harness's discrimination to the real fault, not to any noise fault.
#[test]
fn buggy_loses_writes_iff_a_crash_occurs_correct_never_does() {
    for seed in 0..256u64 {
        let sched = FaultSchedule::generate(seed, 32, 25);
        let has_crash = sched.iter().any(|f| matches!(f, Fault::CrashRestart));
        let buggy_failed = run_ledger(&sched, true).is_some();
        assert_eq!(
            buggy_failed, has_crash,
            "seed {seed}: buggy ledger must fail exactly when a crash (dropped persist) occurs"
        );
        assert!(
            run_ledger(&sched, false).is_none(),
            "seed {seed}: the correct ledger must never lose an acknowledged write"
        );
    }
}

/// A tight, self-contained end-to-end drive of [`SimScheduler`] + [`SimulatedNetwork`]
/// on a hand-built schedule: a single `CrashRestart` at a known step. It proves the
/// two harness primitives cooperate to expose the ordering bug (buggy loses the
/// write, correct does not) without any seed search — the machinery, in the small.
#[test]
fn scheduler_and_network_together_expose_the_ordering_bug() {
    // A clear link except for one dropped persist at step 2.
    let schedule = FaultSchedule::from_faults(vec![
        Fault::None,
        Fault::None,
        Fault::CrashRestart, // the single crash in the ack↔persist window
        Fault::None,
        Fault::None,
    ]);

    // Buggy ledger: the crash lands between ack and persist → a lost write, caught
    // by the invariant engine at the crash step.
    let buggy = run_ledger(&schedule, true).expect("buggy ledger must lose the acked write");
    assert_eq!(
        buggy.step, 2,
        "the violation must be pinpointed at the crash step"
    );
    assert_eq!(buggy.invariant, "acknowledged_writes_are_durable");

    // Correct ledger: same schedule, same harness, no lost write.
    assert!(
        run_ledger(&schedule, false).is_none(),
        "correct ledger survives the same dropped persist"
    );
}
