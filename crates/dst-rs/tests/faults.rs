//! Pillar 2 — **fault injection**.
//!
//! A `FaultSchedule` built from a seed must (a) actually fire the expected fault
//! kinds, (b) be reproducible across runs, and (c) drive the fault-injectable
//! `SimulatedNetwork` so the injected timeline is observable end-to-end.

use dst_rs::{Delivery, Fault, FaultSchedule, SimulatedNetwork};

#[test]
fn seed_produces_a_reproducible_fault_timeline() {
    let a: Vec<Fault> = FaultSchedule::generate(4242, 256, 30)
        .iter()
        .copied()
        .collect();
    let b: Vec<Fault> = FaultSchedule::generate(4242, 256, 30)
        .iter()
        .copied()
        .collect();
    assert_eq!(a, b, "same seed ⇒ identical fault timeline (replayable)");
}

#[test]
fn schedule_fires_all_three_fault_kinds() {
    let sched = FaultSchedule::generate(4242, 256, 30);
    assert!(
        sched.fault_count() > 30,
        "a 30%-density 256-step schedule must carry real faults, got {}",
        sched.fault_count()
    );
    let has = |p: fn(&Fault) -> bool| sched.iter().any(p);
    assert!(
        has(|f| matches!(f, Fault::TransientError)),
        "no transient errors fired"
    );
    assert!(
        has(|f| matches!(f, Fault::CrashRestart)),
        "no crash/restarts fired"
    );
    assert!(
        has(|f| matches!(f, Fault::ClockSkew(_))),
        "no clock skews fired"
    );
}

#[test]
fn distinct_seeds_yield_distinct_timelines() {
    let a: Vec<Fault> = FaultSchedule::generate(1, 128, 40)
        .iter()
        .copied()
        .collect();
    let b: Vec<Fault> = FaultSchedule::generate(2, 128, 40)
        .iter()
        .copied()
        .collect();
    assert_ne!(a, b);
}

/// The injected timeline is observable through the network: each scheduled fault
/// produces exactly the corresponding delivery outcome, in order.
#[test]
fn injected_faults_are_observable_through_the_network() {
    let seed = 777;
    let steps = 64;
    let sched = FaultSchedule::generate(seed, steps, 45);
    let net = SimulatedNetwork::new(sched.clone());

    for step in 0..steps {
        let expected = sched.at(step);
        let got = net.send(step as u64);
        match expected {
            Fault::None => assert!(matches!(got, Delivery::Delivered(_)), "step {step}"),
            Fault::TransientError => assert_eq!(got, Delivery::Failed, "step {step}"),
            Fault::CrashRestart => assert_eq!(got, Delivery::Dropped, "step {step}"),
            Fault::ClockSkew(ms) => {
                assert!(matches!(got, Delivery::Delayed { .. }), "step {step}");
                assert_eq!(got.delay_ms(), ms.unsigned_abs(), "step {step}");
            }
            // `Fault` is `#[non_exhaustive]`; `generate` only emits the kinds above.
            other => unreachable!("unexpected fault kind {other:?} at step {step}"),
        }
    }
    // Rebuilding the network from the same schedule replays the same outcomes.
    let delivered_trace = |s: &FaultSchedule| -> Vec<bool> {
        let n = SimulatedNetwork::new(s.clone());
        (0..steps)
            .map(|i| n.send(i as u64).is_delivered())
            .collect()
    };
    assert_eq!(
        delivered_trace(&sched),
        delivered_trace(&sched),
        "a network rebuilt from the same schedule replays the same delivery trace"
    );
}

/// The step indices carrying a fault are exactly what `ddmin` should be handed.
#[test]
fn fault_steps_index_exactly_the_injected_steps() {
    let sched = FaultSchedule::from_faults(vec![
        Fault::None,
        Fault::TransientError,
        Fault::None,
        Fault::CrashRestart,
        Fault::ClockSkew(10),
    ]);
    assert_eq!(sched.fault_steps(), vec![1, 3, 4]);
    assert_eq!(sched.fault_count(), 3);
}
