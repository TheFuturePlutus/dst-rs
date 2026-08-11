//! Pillar 3 — **automatic shrinking**.
//!
//! When a seeded run fails, the fault timeline that triggered it is usually far
//! larger than the *minimal* trigger. `ddmin` peels a bloated failing trace down
//! to a 1-minimal reproducer. These tests frame `ddmin` over fault timelines with
//! deliberately-planted redundancy and assert it recovers the essential core.

use dst_rs::{ddmin, Fault, FaultSchedule};

/// A failing run whose only *essential* fault is a single `CrashRestart`, buried
/// among many irrelevant `TransientError`/`ClockSkew` steps. `ddmin` must peel the
/// redundant faults away and return exactly the one crash.
#[test]
fn shrinks_a_noisy_timeline_to_the_single_essential_crash() {
    // 20 fault steps; index 11 is the lone crash that "breaks" the system.
    let mut faults = vec![Fault::TransientError; 20];
    faults[11] = Fault::CrashRestart;
    let sched = FaultSchedule::from_faults(faults);

    // Input to ddmin: the (index, fault) pairs at every fault step.
    let fault_points: Vec<(usize, Fault)> = sched
        .fault_steps()
        .into_iter()
        .map(|i| (i, sched.at(i)))
        .collect();
    assert_eq!(fault_points.len(), 20);

    // The failure reproduces iff a CrashRestart is present.
    let reproduces =
        |subset: &[(usize, Fault)]| subset.iter().any(|(_, f)| matches!(f, Fault::CrashRestart));

    let minimal = ddmin(&fault_points, reproduces);
    assert_eq!(minimal.len(), 1, "must shrink to a single essential fault");
    assert_eq!(minimal[0], (11, Fault::CrashRestart));
}

/// A failure that needs *two* specific faults in the timeline (a bad interleaving
/// of a crash and a clock-skew). `ddmin` returns a 1-minimal pair — dropping
/// either makes the failure vanish — with input order preserved.
#[test]
fn finds_a_minimal_two_fault_conjunction() {
    let mut faults = vec![Fault::TransientError; 30];
    faults[6] = Fault::CrashRestart;
    faults[23] = Fault::ClockSkew(500);
    let sched = FaultSchedule::from_faults(faults);

    let fault_points: Vec<(usize, Fault)> = sched
        .fault_steps()
        .into_iter()
        .map(|i| (i, sched.at(i)))
        .collect();

    // Bug needs BOTH a crash AND a clock skew present.
    let reproduces = |subset: &[(usize, Fault)]| {
        subset.iter().any(|(_, f)| matches!(f, Fault::CrashRestart))
            && subset.iter().any(|(_, f)| matches!(f, Fault::ClockSkew(_)))
    };

    let minimal = ddmin(&fault_points, reproduces);
    assert_eq!(minimal.len(), 2, "irreducible conjunction has size 2");
    assert_eq!(
        minimal[0],
        (6, Fault::CrashRestart),
        "order preserved: crash first"
    );
    assert_eq!(minimal[1], (23, Fault::ClockSkew(500)));

    // 1-minimality: removing either element makes the predicate pass.
    for i in 0..minimal.len() {
        let mut without = minimal.clone();
        without.remove(i);
        assert!(
            !reproduces(&without),
            "result is not 1-minimal: {without:?} still reproduces"
        );
    }
}

/// CLAIM (Part B): **shrinking is 1-minimal.** After `ddmin` reduces a noisy,
/// seed-generated fault timeline whose failure needs a specific two-fault
/// conjunction, removing ANY single element from the shrunk set must stop
/// reproducing the failure. This asserts 1-minimality generically over the
/// shrunk output (not just the size).
#[test]
fn shrunk_fault_set_is_one_minimal() {
    // A realistic, noisy timeline from a seed, plus two planted essential faults
    // that the failure requires BOTH of.
    let mut faults = FaultSchedule::generate(20260811, 96, 30)
        .iter()
        .copied()
        .collect::<Vec<_>>();
    faults[17] = Fault::CrashRestart;
    faults[71] = Fault::ClockSkew(1234);
    let sched = FaultSchedule::from_faults(faults);

    let fault_points: Vec<(usize, Fault)> = sched
        .fault_steps()
        .into_iter()
        .map(|i| (i, sched.at(i)))
        .collect();
    assert!(
        fault_points.len() > 10,
        "need a non-trivial timeline to make shrinking meaningful"
    );

    // The failure needs the crash at 17 AND the skew at 71 — both present.
    let reproduces = |subset: &[(usize, Fault)]| {
        subset.iter().any(|&(i, _)| i == 17) && subset.iter().any(|&(i, _)| i == 71)
    };

    let minimal = ddmin(&fault_points, reproduces);
    assert!(reproduces(&minimal), "the shrunk set must still reproduce");

    // 1-minimality: dropping any single element must break reproduction.
    for i in 0..minimal.len() {
        let mut without = minimal.clone();
        without.remove(i);
        assert!(
            !reproduces(&without),
            "not 1-minimal: removing element {i} ({:?}) still reproduces",
            minimal[i]
        );
    }
}

/// Shrinking is deterministic: the same failing input yields the same reproducer.
#[test]
fn shrinking_is_deterministic() {
    let sched = FaultSchedule::generate(31337, 128, 40);
    let fault_points: Vec<(usize, Fault)> = sched
        .fault_steps()
        .into_iter()
        .map(|i| (i, sched.at(i)))
        .collect();
    assert!(
        fault_points.len() > 10,
        "need a non-trivial timeline to shrink"
    );

    // Pick a real crash index from this seed to anchor the predicate on.
    let crash_idx = fault_points
        .iter()
        .find(|(_, f)| matches!(f, Fault::CrashRestart))
        .map(|(i, _)| *i)
        .expect("seed 31337 should schedule at least one crash");
    let reproduces = |subset: &[(usize, Fault)]| subset.iter().any(|(i, _)| *i == crash_idx);

    let a = ddmin(&fault_points, reproduces);
    let b = ddmin(&fault_points, reproduces);
    assert_eq!(a, b, "ddmin must be deterministic");
    assert_eq!(a.len(), 1);
    assert_eq!(a[0].0, crash_idx);
}
