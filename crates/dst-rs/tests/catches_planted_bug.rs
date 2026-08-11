//! The credibility test: a REAL, deliberately-planted bug, caught by the harness.
//!
//! The planted bug is a **durability ordering** defect: a ledger that *acknowledges*
//! a deposit to the caller BEFORE the write is durable. A correct ledger persists
//! first, then acks. The two implementations are byte-identical except for that
//! ordering — this is the kind of bug that passes every happy-path test and only
//! surfaces when a crash lands in the tiny window between ack and persist.
//!
//! What this test proves, end-to-end:
//!   (a) sweeping seeds under fault injection, the harness FINDS a failing seed for
//!       the buggy ledger (and finds NONE for the correct one — so the harness is
//!       discriminating, not trivially always-failing);
//!   (b) `ddmin` shrinks the failing schedule to a MINIMAL reproducer — a single
//!       `CrashRestart` — which is the actionable bug report.
//!
//! Determinism: a fixed seed range (0..256) and seeded schedules, so this test is
//! reproducible, never flaky. `SEED=<n>` re-runs a single seed for manual triage.

use dst_rs::{ddmin, Fault, FaultSchedule, Invariant, InvariantEngine, Violation};

/// State of the ledger the invariant engine inspects.
///
/// * `acknowledged` — total deposits the caller was told succeeded.
/// * `durable`      — total deposits actually persisted to durable storage.
///
/// A correct system guarantees `durable == acknowledged`: nothing acknowledged is
/// ever lost. That is the one invariant this harness checks.
#[derive(Clone, Copy, Debug)]
struct LedgerState {
    acknowledged: u64,
    durable: u64,
}

fn ledger_invariants() -> InvariantEngine<LedgerState> {
    InvariantEngine::new(vec![Invariant::new(
        "acknowledged_writes_are_durable",
        |s: &LedgerState| s.durable == s.acknowledged,
    )])
}

/// Run the ledger across `schedule`, one deposit of 1 per step. Returns the FIRST
/// invariant violation (the step at which an acknowledged deposit was lost), or
/// `None` if the run stayed correct.
///
/// `buggy == true` acks before persisting (the planted bug); `false` persists
/// before acking (correct). Only `CrashRestart` faults interact with the ledger —
/// they strike in the ack↔persist window; other fault kinds are quiescent here.
fn run_ledger(schedule: &FaultSchedule, buggy: bool) -> Option<Violation> {
    let mut acknowledged: u64 = 0;
    let mut durable: u64 = 0;
    let engine = ledger_invariants();

    for step in 0..schedule.len() {
        // A deposit of 1 arrives this step; a crash may strike mid-write.
        let crash = matches!(schedule.at(step), Fault::CrashRestart);

        if buggy {
            // BUG: acknowledge to the caller BEFORE the write is durable.
            acknowledged += 1;
            if !crash {
                // No crash: the write reaches durable storage.
                durable += 1;
            }
            // Crash in the ack↔persist window: the acknowledged write is lost —
            // `durable` is NOT incremented, so it falls behind `acknowledged`.
        } else {
            // CORRECT: persist first, then acknowledge. A crash before persist
            // simply leaves the deposit un-acknowledged (the caller retries);
            // nothing acknowledged is ever lost.
            if !crash {
                durable += 1;
                acknowledged += 1;
            }
        }

        let state = LedgerState {
            acknowledged,
            durable,
        };
        if let Some(v) = engine.check(step, &state) {
            return Some(v);
        }
    }
    None
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

    // (a) The harness FINDS a failing seed for the buggy ledger.
    let (seed, sched) = find_failing_seed().expect("harness should find a failing seed in 0..256");
    let violation = run_ledger(&sched, true).expect("the failing seed must reproduce");

    // The correct ledger must NOT fail on that same schedule — proves the harness
    // is discriminating (it catches the BUG, not merely the presence of faults).
    assert!(
        run_ledger(&sched, false).is_none(),
        "the correct ledger must survive the schedule that breaks the buggy one"
    );

    // (b) `ddmin` shrinks the failing schedule to a MINIMAL reproducer.
    // Input: every fault point in the schedule (crashes plus irrelevant noise).
    let fault_points: Vec<(usize, Fault)> = sched
        .fault_steps()
        .into_iter()
        .map(|i| (i, sched.at(i)))
        .collect();

    // Predicate: rebuild a full-length schedule containing ONLY this subset of
    // faults, and check whether the buggy ledger still loses an acknowledged write.
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

/// Sanity floor: the buggy ledger loses a write on *every* schedule containing a
/// crash, and the correct one never does — over the whole fixed seed range. This
/// makes the "finds a failing seed" claim above non-accidental.
#[test]
fn buggy_loses_writes_iff_a_crash_occurs_correct_never_does() {
    for seed in 0..256u64 {
        let sched = FaultSchedule::generate(seed, 32, 25);
        let has_crash = sched.iter().any(|f| matches!(f, Fault::CrashRestart));
        let buggy_failed = run_ledger(&sched, true).is_some();
        assert_eq!(
            buggy_failed, has_crash,
            "seed {seed}: buggy ledger must fail exactly when a crash occurs"
        );
        assert!(
            run_ledger(&sched, false).is_none(),
            "seed {seed}: the correct ledger must never lose an acknowledged write"
        );
    }
}
