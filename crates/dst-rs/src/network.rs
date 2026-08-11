//! `Network` trait — cluster-internal node-to-node communication.
//!
//! The trait itself stays a **marker** (Phase 0, ADR-0022 alternative #4): engine
//! types can be parameterized by `N: Network` without committing to a method
//! surface, and both impls remain `Arc<dyn Network>`-object-safe. The
//! fault-injection surface lives as **inherent** methods on the concrete impls
//! (a generic `send<T>` would break object-safety of the marker), so a test can
//! drive a believable "flaky link" without touching the trait.
//!
//! [`SimulatedNetwork`] maps the shared [`Fault`]/[`FaultSchedule`] timeline onto
//! delivery outcomes — one schedule step consumed per send:
//!
//! - [`Fault::None`]          → [`Delivery::Delivered`] (the link is clear)
//! - [`Fault::TransientError`] → [`Delivery::Failed`]    (retryable error)
//! - [`Fault::CrashRestart`]  → [`Delivery::Dropped`]    (silent loss, no response)
//! - [`Fault::ClockSkew`]     → [`Delivery::Delayed`]    (arrives, but late)
//!
//! Reusing the existing seeded schedule (rather than a parallel fault system)
//! means a network run replays bit-for-bit and shrinks via [`crate::ddmin`] just
//! like any other DST failure. [`ProductionNetwork`] is a real pass-through: every
//! send is [`Delivery::Delivered`].

use std::sync::atomic::{AtomicUsize, Ordering};

use crate::schedule::{Fault, FaultSchedule};

/// Cluster-internal network abstraction (marker trait; see module docs).
pub trait Network: Send + Sync + 'static {}

/// The outcome of a single simulated send. `Delivered`/`Delayed` are successes
/// (`Delayed` still arrived, just late); `Failed`/`Dropped` are non-deliveries a
/// correct caller must retry or time out on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery<T> {
    /// Delivered immediately, on a clear link.
    Delivered(T),
    /// Delivered, but only after `delay_ms` of simulated delay (clock skew).
    Delayed { value: T, delay_ms: u64 },
    /// Transient link/peer error — nothing arrived; the caller should retry.
    Failed,
    /// Silently dropped — no response arrives at all; the caller must time out.
    Dropped,
}

impl<T> Delivery<T> {
    /// Whether the payload arrived (immediately or late).
    pub fn is_delivered(&self) -> bool {
        matches!(self, Delivery::Delivered(_) | Delivery::Delayed { .. })
    }

    /// Whether this outcome is a non-delivery a correct caller should retry.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Delivery::Failed | Delivery::Dropped)
    }

    /// The simulated delay in ms (0 unless this was a `Delayed` delivery).
    pub fn delay_ms(&self) -> u64 {
        match self {
            Delivery::Delayed { delay_ms, .. } => *delay_ms,
            _ => 0,
        }
    }

    /// Consume the outcome, yielding the delivered payload if it arrived.
    pub fn into_delivered(self) -> Option<T> {
        match self {
            Delivery::Delivered(v) | Delivery::Delayed { value: v, .. } => Some(v),
            Delivery::Failed | Delivery::Dropped => None,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────
// ProductionNetwork
// ─────────────────────────────────────────────────────────────────────

/// Production-mode `Network`: a real pass-through. Every send is `Delivered`.
///
/// Phase 2+ wraps `tonic` clients with persistent connections to peer shards;
/// the fault-free `send` contract here is what those real transports must honor
/// on a healthy link.
#[derive(Debug, Default, Clone, Copy)]
pub struct ProductionNetwork;

impl Network for ProductionNetwork {}

impl ProductionNetwork {
    /// Pass-through send: always delivers immediately.
    pub fn send<T>(&self, payload: T) -> Delivery<T> {
        Delivery::Delivered(payload)
    }
}

// ─────────────────────────────────────────────────────────────────────
// SimulatedNetwork
// ─────────────────────────────────────────────────────────────────────

/// Simulation-mode `Network`: a seeded, fault-injectable link.
///
/// Each [`send`](Self::send) consumes the next step of an embedded
/// [`FaultSchedule`] and maps that step's [`Fault`] to a [`Delivery`] outcome
/// (see the module docs for the mapping). Because the schedule is fully derived
/// from a seed, a run replays identically and its failing step set shrinks via
/// [`crate::ddmin`]. Steps past the schedule end are treated as [`Fault::None`]
/// (a clear link), so a retry loop that outlasts the schedule always eventually
/// gets through.
///
/// The step counter is an atomic so the network can be shared behind an `Arc`
/// across cooperating tasks and still advance deterministically under a
/// single-threaded scheduler.
#[derive(Debug)]
pub struct SimulatedNetwork {
    schedule: FaultSchedule,
    step: AtomicUsize,
}

impl Default for SimulatedNetwork {
    /// A perfect (fault-free) link — every send delivers. Equivalent to
    /// `SimulatedNetwork::new(FaultSchedule::quiescent(0))`.
    fn default() -> Self {
        Self::perfect()
    }
}

impl Network for SimulatedNetwork {}

impl SimulatedNetwork {
    /// Build a network whose link faults follow `schedule`.
    pub fn new(schedule: FaultSchedule) -> Self {
        Self {
            schedule,
            step: AtomicUsize::new(0),
        }
    }

    /// A perfect, fault-free link (all sends delivered). Useful as a baseline the
    /// faulted run is compared against.
    pub fn perfect() -> Self {
        Self::new(FaultSchedule::quiescent(0))
    }

    /// The schedule step the next `send` will consume.
    pub fn step(&self) -> usize {
        self.step.load(Ordering::SeqCst)
    }

    /// Rewind to the start of the schedule (e.g. to replay the same link twice).
    pub fn reset(&self) {
        self.step.store(0, Ordering::SeqCst);
    }

    /// The fault the next `send` will apply, without consuming a step.
    pub fn peek(&self) -> Fault {
        self.schedule.at(self.step())
    }

    /// Attempt to send `payload`, consuming the next schedule step and mapping its
    /// fault to a [`Delivery`] outcome.
    pub fn send<T>(&self, payload: T) -> Delivery<T> {
        let step = self.step.fetch_add(1, Ordering::SeqCst);
        match self.schedule.at(step) {
            Fault::None => Delivery::Delivered(payload),
            Fault::TransientError => Delivery::Failed,
            Fault::CrashRestart => Delivery::Dropped,
            Fault::ClockSkew(ms) => Delivery::Delayed {
                value: payload,
                delay_ms: ms.unsigned_abs(),
            },
        }
    }

    /// Payload-free convenience for probing the link (e.g. a health ping).
    pub fn attempt(&self) -> Delivery<()> {
        self.send(())
    }
}

// ─────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_network_is_object_safe() {
        let _: std::sync::Arc<dyn Network> = std::sync::Arc::new(ProductionNetwork);
    }

    #[test]
    fn simulated_network_is_object_safe() {
        let _: std::sync::Arc<dyn Network> = std::sync::Arc::new(SimulatedNetwork::perfect());
    }

    #[test]
    fn network_trait_compiles_for_generic_engine_types() {
        // A generic struct can still be parameterized by the marker trait.
        struct EngineLike<N: Network> {
            _net: N,
        }
        let _e: EngineLike<ProductionNetwork> = EngineLike {
            _net: ProductionNetwork,
        };
        let _s: EngineLike<SimulatedNetwork> = EngineLike {
            _net: SimulatedNetwork::perfect(),
        };
    }

    #[test]
    fn production_network_always_delivers() {
        let net = ProductionNetwork;
        for i in 0..100u32 {
            assert_eq!(net.send(i), Delivery::Delivered(i));
        }
    }

    #[test]
    fn perfect_simulated_network_always_delivers() {
        let net = SimulatedNetwork::perfect();
        for i in 0..100u32 {
            assert_eq!(net.send(i), Delivery::Delivered(i));
        }
    }

    #[test]
    fn fault_kinds_map_to_delivery_outcomes() {
        let schedule = FaultSchedule::from_faults(vec![
            Fault::None,
            Fault::TransientError,
            Fault::CrashRestart,
            Fault::ClockSkew(250),
            Fault::ClockSkew(-99),
        ]);
        let net = SimulatedNetwork::new(schedule);
        assert_eq!(net.send(1u32), Delivery::Delivered(1));
        assert_eq!(net.send(2u32), Delivery::Failed);
        assert_eq!(net.send(3u32), Delivery::Dropped);
        assert_eq!(
            net.send(4u32),
            Delivery::Delayed {
                value: 4,
                delay_ms: 250
            }
        );
        // Negative skew still means a positive delay magnitude.
        assert_eq!(net.send(5u32).delay_ms(), 99);
    }

    #[test]
    fn steps_past_the_schedule_end_are_a_clear_link() {
        let net = SimulatedNetwork::new(FaultSchedule::from_faults(vec![Fault::TransientError]));
        assert_eq!(net.send(1u32), Delivery::Failed); // step 0
        assert_eq!(net.send(2u32), Delivery::Delivered(2)); // step 1, past end
        assert_eq!(net.send(3u32), Delivery::Delivered(3)); // step 2, past end
    }

    #[test]
    fn same_seed_produces_identical_delivery_trace() {
        let trace = |seed: u64| -> Vec<bool> {
            let net = SimulatedNetwork::new(FaultSchedule::generate(seed, 64, 40));
            (0..64).map(|i| net.send(i).is_delivered()).collect()
        };
        assert_eq!(trace(7), trace(7), "same seed ⇒ identical link behavior");
        assert_ne!(
            trace(7),
            trace(8),
            "distinct seeds ⇒ different link behavior"
        );
    }

    #[test]
    fn reset_rewinds_the_schedule() {
        let net = SimulatedNetwork::new(FaultSchedule::from_faults(vec![
            Fault::TransientError,
            Fault::None,
        ]));
        assert_eq!(net.send(0u32), Delivery::Failed);
        assert_eq!(net.step(), 1);
        net.reset();
        assert_eq!(net.step(), 0);
        assert_eq!(
            net.send(0u32),
            Delivery::Failed,
            "same outcome after rewind"
        );
    }

    #[test]
    fn peek_does_not_consume_a_step() {
        let net = SimulatedNetwork::new(FaultSchedule::from_faults(vec![Fault::CrashRestart]));
        assert_eq!(net.peek(), Fault::CrashRestart);
        assert_eq!(net.step(), 0, "peek must not advance");
        assert_eq!(net.send(1u32), Delivery::Dropped);
    }

    #[test]
    fn delivery_helpers() {
        assert!(Delivery::Delivered(1u32).is_delivered());
        assert!(Delivery::Delayed {
            value: 1u32,
            delay_ms: 5
        }
        .is_delivered());
        assert!(Delivery::<u32>::Failed.is_retryable());
        assert!(Delivery::<u32>::Dropped.is_retryable());
        assert_eq!(Delivery::Delivered(9u32).into_delivered(), Some(9));
        assert_eq!(Delivery::<u32>::Dropped.into_delivered(), None);
    }
}
