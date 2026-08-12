//! A generic, step-aware invariant engine for DST runs.
//!
//! A domain-specific harness might hard-code an invariant set against one
//! concrete state type and report aggregate violation *counts*. This module is
//! the domain-agnostic complement:
//! [`InvariantEngine<S>`] is generic over the caller's world `S` (a queue snapshot,
//! a ledger, a counter — anything), runs a declared set of named [`Invariant`]s
//! after each simulation step, and reports the **first** [`Violation`] with the
//! step index at which it broke. That step index is what makes a DST failure
//! triageable: it points at the earliest moment the world went wrong, which then
//! feeds trace shrinking (see [`crate::shrink::ddmin`]).
//!
//! It replaces scattered inline `assert!`s in a simulation loop with a declarative
//! invariant list that is reusable across subsystems.

/// A named boolean check over some simulation state `S`. `holds(state)` returns
/// `true` when the invariant is satisfied.
///
/// `holds` should be a **pure** predicate. [`InvariantEngine::check`] short-circuits
/// at the first failure (in declaration order), so a `holds` with side effects —
/// e.g. one that records the previous value to check monotonicity — may be skipped
/// on a step where an earlier invariant already failed, leaving its internal state
/// stale. Keep cross-step state outside the predicate set, or use
/// [`InvariantEngine::check_all`] (which always evaluates every invariant).
pub struct Invariant<S> {
    /// Human-readable name reported in a [`Violation`] when this check fails.
    pub name: &'static str,
    holds: Box<dyn Fn(&S) -> bool>,
}

impl<S> Invariant<S> {
    /// Build a named invariant from a predicate over the state `S`.
    pub fn new(name: &'static str, holds: impl Fn(&S) -> bool + 'static) -> Self {
        Self {
            name,
            holds: Box::new(holds),
        }
    }
}

/// A violated invariant: which one, and the step index at which it first broke.
#[derive(Clone, Debug, PartialEq, Eq)]
#[must_use = "a Violation reports a DST failure and should be acted on, not dropped"]
pub struct Violation {
    /// The simulation step index at which the invariant first broke.
    pub step: usize,
    /// The name of the invariant that was violated.
    pub invariant: &'static str,
}

/// Runs a set of [`Invariant`]s over evolving state. Domain-agnostic: `S` is the
/// caller's world.
pub struct InvariantEngine<S> {
    invariants: Vec<Invariant<S>>,
}

impl<S> InvariantEngine<S> {
    /// Build an engine that checks `invariants` in declaration order.
    pub fn new(invariants: Vec<Invariant<S>>) -> Self {
        Self { invariants }
    }

    /// Check every invariant against `state` at `step`; return the FIRST violation
    /// (declaration order), or `None` if all hold. First-violation semantics make
    /// the report point at the earliest/most-fundamental broken property.
    pub fn check(&self, step: usize, state: &S) -> Option<Violation> {
        for inv in &self.invariants {
            if !(inv.holds)(state) {
                return Some(Violation {
                    step,
                    invariant: inv.name,
                });
            }
        }
        None
    }

    /// Every invariant violated at `step` (not just the first) — for a full report.
    pub fn check_all(&self, step: usize, state: &S) -> Vec<Violation> {
        self.invariants
            .iter()
            .filter(|inv| !(inv.holds)(state))
            .map(|inv| Violation {
                step,
                invariant: inv.name,
            })
            .collect()
    }

    /// The number of invariants this engine checks.
    pub fn len(&self) -> usize {
        self.invariants.len()
    }

    /// Whether the engine holds no invariants (checks are a no-op).
    pub fn is_empty(&self) -> bool {
        self.invariants.is_empty()
    }
}

#[cfg(test)]
mod tests {
    //! Toy worlds (a counter, deliberately NOT LAL) prove the engine is reusable
    //! beyond the subsystem it complements.
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    struct CounterWorld {
        value: i64,
        cap: i64,
    }

    /// Two invariants: a stateless bound and a stateful monotonicity check that
    /// remembers the previous value.
    fn counter_invariants(prev: Rc<Cell<i64>>) -> InvariantEngine<CounterWorld> {
        InvariantEngine::new(vec![
            Invariant::new("under_cap", |w: &CounterWorld| w.value <= w.cap),
            Invariant::new("monotonic", move |w: &CounterWorld| {
                let ok = w.value >= prev.get();
                prev.set(w.value);
                ok
            }),
        ])
    }

    #[test]
    fn passes_a_well_behaved_run() {
        let eng = counter_invariants(Rc::new(Cell::new(0)));
        for step in 0..10 {
            let w = CounterWorld {
                value: step as i64,
                cap: 100,
            };
            assert_eq!(
                eng.check(step, &w),
                None,
                "monotonic under-cap run must pass"
            );
        }
    }

    #[test]
    fn reports_first_violation_with_step_and_name() {
        let eng = counter_invariants(Rc::new(Cell::new(0)));
        // A monotonic climb that breaches the cap at step 3.
        let seq = [0i64, 40, 80, 120];
        let mut hit = None;
        for (step, &v) in seq.iter().enumerate() {
            let w = CounterWorld { value: v, cap: 100 };
            if let Some(viol) = eng.check(step, &w) {
                hit = Some(viol);
                break;
            }
        }
        assert_eq!(
            hit,
            Some(Violation {
                step: 3,
                invariant: "under_cap"
            }),
            "must pinpoint the cap breach at step 3"
        );
    }

    #[test]
    fn declaration_order_decides_which_violation_is_first() {
        // Both invariants fail at once; `check` returns the first-declared one.
        let eng = counter_invariants(Rc::new(Cell::new(500)));
        let w = CounterWorld {
            value: 200,
            cap: 100,
        }; // over cap AND below prev 500
        assert_eq!(
            eng.check(7, &w),
            Some(Violation {
                step: 7,
                invariant: "under_cap"
            }),
            "under_cap is declared first, so it wins the first-violation report"
        );
    }

    #[test]
    fn check_all_returns_every_violation() {
        let eng = counter_invariants(Rc::new(Cell::new(500)));
        let w = CounterWorld {
            value: 200,
            cap: 100,
        };
        let all = eng.check_all(5, &w);
        assert_eq!(all.len(), 2, "both invariants should report at once");
        assert!(all.iter().any(|v| v.invariant == "under_cap"));
        assert!(all.iter().any(|v| v.invariant == "monotonic"));
        assert!(all.iter().all(|v| v.step == 5));
    }
}
