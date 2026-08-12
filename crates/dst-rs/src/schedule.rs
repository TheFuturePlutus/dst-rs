//! Seeded fault-injection *schedules* for DST runs.
//!
//! The substrate traits ([`crate::Time`], [`crate::Random`]) abstract the
//! *sources* of non-determinism and
//! let a test inject one fault at a time. A [`FaultSchedule`] lifts that to a
//! reproducible *timeline*: one [`Fault`] per simulation step, derived entirely
//! from a seed, so a failing run can be replayed and — via
//! [`crate::shrink::ddmin`] over the step indices — shrunk to the minimal set
//! of fault points that actually break the system.
//!
//! Three fault kinds cover the recovery paths worth exercising:
//! - `TransientError` — the backend (LLM / store / peer) fails this step; the
//!   system must retry and lose no work.
//! - `CrashRestart` — the process dies and reopens from durable state; in-memory
//!   counters reset but nothing durable is lost or double-completed.
//! - `ClockSkew` — the wall clock jumps (NTP correction / VM pause); ordering and
//!   conservation must not depend on monotonic wall time.

use crate::random::SimulatedRandom;
use crate::Random;

/// A fault to inject before a given simulation step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Fault {
    /// No fault this step — the common case.
    None,
    /// A transient backend failure: retry expected, no work lost.
    TransientError,
    /// Process death + restart from durable state.
    CrashRestart,
    /// The wall clock jumps by this many milliseconds (may be negative).
    ClockSkew(i64),
}

impl Fault {
    /// Whether this is an actual injected fault (not the quiescent `None`).
    pub fn is_fault(&self) -> bool {
        !matches!(self, Fault::None)
    }
}

/// A reproducible, seed-keyed sequence of faults — one entry per simulation step.
/// The whole timeline is materialized up front from the seed, so it is fully
/// replayable and inspectable (you can print the exact fault sequence that broke a
/// run, then feed the step indices to `ddmin`).
///
/// # Examples
///
/// ```
/// use dst_rs::FaultSchedule;
///
/// // Same seed ⇒ identical timeline; ~30% of 200 steps carry a fault.
/// let a = FaultSchedule::generate(123, 200, 30);
/// let b = FaultSchedule::generate(123, 200, 30);
/// assert_eq!(a.fault_steps(), b.fault_steps());
/// assert!(a.fault_count() > 0);
/// ```
#[derive(Clone, Debug)]
pub struct FaultSchedule {
    faults: Vec<Fault>,
}

impl FaultSchedule {
    /// Build a `steps`-long schedule. `density` (0..=100) is the percent chance any
    /// given step carries a fault; when it does, the kind is chosen uniformly and a
    /// `ClockSkew` gets a seed-derived signed delta of up to ±1 day.
    ///
    /// Uses the shared [`SimulatedRandom`] (seeded ChaCha20), so the same seed
    /// always yields the same schedule — the DST replay guarantee.
    pub fn generate(seed: u64, steps: usize, density: u64) -> Self {
        let rng = SimulatedRandom::from_seed(seed);
        let density = density.min(100);
        let mut faults = Vec::with_capacity(steps);
        for _ in 0..steps {
            let f = if (rng.next_u64() % 100) < density {
                match rng.next_u64() % 3 {
                    0 => Fault::TransientError,
                    1 => Fault::CrashRestart,
                    _ => {
                        // A nonzero skew of up to ±1 day (magnitude in 1..=86_400_000
                        // so a "fault" step is never a no-op ClockSkew(0)); sign from
                        // a fresh draw.
                        let mag = 1 + (rng.next_u64() % 86_400_000) as i64;
                        let sign = if rng.next_u64().is_multiple_of(2) {
                            1
                        } else {
                            -1
                        };
                        Fault::ClockSkew(sign * mag)
                    }
                }
            } else {
                Fault::None
            };
            faults.push(f);
        }
        Self { faults }
    }

    /// A fault-free schedule — the baseline a faulted run is compared against.
    pub fn quiescent(steps: usize) -> Self {
        Self {
            faults: vec![Fault::None; steps],
        }
    }

    /// Build a schedule directly from an explicit list of faults (for targeted
    /// tests and for reconstructing a shrunk schedule).
    pub fn from_faults(faults: Vec<Fault>) -> Self {
        Self { faults }
    }

    /// A copy with every `ClockSkew` replaced by `None`. Use this when driving a
    /// system-under-test that does not (yet) thread a controllable clock through its
    /// internals: a between-tick driver can honestly inject `TransientError` and
    /// `CrashRestart`, but it cannot make a wall-clock jump observable to the SUT, so
    /// leaving `ClockSkew` in the schedule would overstate what the run actually
    /// exercised. `ClockSkew` generation itself stays covered by the schedule-level
    /// tests; this just keeps a runtime driver's coverage claim honest.
    pub fn without_clock_skew(&self) -> FaultSchedule {
        FaultSchedule {
            faults: self
                .faults
                .iter()
                .map(|f| match f {
                    Fault::ClockSkew(_) => Fault::None,
                    other => *other,
                })
                .collect(),
        }
    }

    /// The fault to inject before `step`. Steps past the end are `None` — a run may
    /// take more ticks than scheduled; the tail is simply quiescent.
    pub fn at(&self, step: usize) -> Fault {
        self.faults.get(step).copied().unwrap_or(Fault::None)
    }

    /// The number of steps in the schedule (including quiescent `None` steps).
    pub fn len(&self) -> usize {
        self.faults.len()
    }

    /// Whether the schedule has zero steps.
    pub fn is_empty(&self) -> bool {
        self.faults.is_empty()
    }

    /// How many non-`None` faults the schedule carries — lets a test assert it is
    /// actually exercising faults (a schedule that injects nothing proves nothing).
    pub fn fault_count(&self) -> usize {
        self.faults.iter().filter(|f| f.is_fault()).count()
    }

    /// The `[start, end)` step indices carrying a fault — the natural input to
    /// `ddmin` when shrinking which fault points break the system.
    pub fn fault_steps(&self) -> Vec<usize> {
        self.faults
            .iter()
            .enumerate()
            .filter(|(_, f)| f.is_fault())
            .map(|(i, _)| i)
            .collect()
    }

    /// Iterate over the per-step faults in step order.
    pub fn iter(&self) -> impl Iterator<Item = &Fault> {
        self.faults.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replays_identically_and_actually_injects_all_kinds() {
        let a = FaultSchedule::generate(123, 200, 30);
        let b = FaultSchedule::generate(123, 200, 30);
        let fa: Vec<Fault> = a.iter().copied().collect();
        let fb: Vec<Fault> = b.iter().copied().collect();
        assert_eq!(fa, fb, "same seed ⇒ identical schedule (replayable)");
        assert_eq!(a.len(), 200);
        // At ~30% density over 200 steps the schedule must carry real faults —
        // otherwise a "fault test" driven by it would be silently vacuous.
        assert!(
            a.fault_count() > 20,
            "too few faults injected: {}",
            a.fault_count()
        );
        let has = |p: fn(&Fault) -> bool| fa.iter().any(p);
        assert!(
            has(|f| matches!(f, Fault::TransientError)),
            "no transient errors"
        );
        assert!(has(|f| matches!(f, Fault::CrashRestart)), "no crashes");
        assert!(has(|f| matches!(f, Fault::ClockSkew(_))), "no clock skew");
    }

    #[test]
    fn different_seeds_produce_different_schedules() {
        let a: Vec<Fault> = FaultSchedule::generate(1, 128, 40)
            .iter()
            .copied()
            .collect();
        let b: Vec<Fault> = FaultSchedule::generate(2, 128, 40)
            .iter()
            .copied()
            .collect();
        assert_ne!(a, b, "distinct seeds should (overwhelmingly) differ");
    }

    #[test]
    fn density_zero_and_hundred_are_extremes() {
        assert_eq!(
            FaultSchedule::generate(9, 100, 0).fault_count(),
            0,
            "0% ⇒ no faults"
        );
        assert_eq!(
            FaultSchedule::generate(9, 100, 100).fault_count(),
            100,
            "100% ⇒ every step is a fault"
        );
    }

    #[test]
    fn quiescent_injects_nothing_and_tail_is_safe() {
        let q = FaultSchedule::quiescent(50);
        assert_eq!(q.fault_count(), 0);
        assert!(q.fault_steps().is_empty());
        assert_eq!(q.at(10), Fault::None);
        assert_eq!(
            q.at(9999),
            Fault::None,
            "past the end is quiescent, not a panic"
        );
    }

    #[test]
    fn without_clock_skew_drops_only_clock_skew() {
        let full = FaultSchedule::generate(123, 200, 30);
        let stripped = full.without_clock_skew();
        assert_eq!(full.len(), stripped.len());
        // No ClockSkew survives...
        assert!(stripped.iter().all(|f| !matches!(f, Fault::ClockSkew(_))));
        // ...but every Transient/Crash is preserved position-for-position.
        for (a, b) in full.iter().zip(stripped.iter()) {
            match a {
                Fault::ClockSkew(_) => assert_eq!(*b, Fault::None),
                other => assert_eq!(b, other),
            }
        }
        // The original carried some clock skew, so this test isn't vacuous.
        assert!(full.iter().any(|f| matches!(f, Fault::ClockSkew(_))));
        assert!(stripped.fault_count() < full.fault_count());
    }

    #[test]
    fn fault_steps_indexes_exactly_the_injected_steps() {
        let s = FaultSchedule::from_faults(vec![
            Fault::None,
            Fault::TransientError,
            Fault::None,
            Fault::CrashRestart,
        ]);
        assert_eq!(s.fault_steps(), vec![1, 3]);
        assert_eq!(s.fault_count(), 2);
    }
}
