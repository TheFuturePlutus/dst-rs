//! # navian-dst
//!
//! A domain-agnostic **deterministic-simulation-testing (DST)** substrate.
//!
//! DST replays a system under a harness-controlled clock, RNG, network, and
//! executor so that a failing run can be reproduced bit-for-bit and shrunk to
//! its minimal trigger. This crate provides the reusable building blocks —
//! nothing here knows anything about a particular application domain.
//!
//! ## Non-determinism traits
//!
//! Four traits abstract the sources of non-determinism that would otherwise
//! prevent replay-based testing. Each has a zero-overhead production impl
//! (direct calls to `std`/`tokio`/`rand`) and a deterministic simulation impl:
//!
//! - [`Time`] — wall-clock and monotonic time, plus async sleep
//! - [`Random`] — RNG and UUID generation
//! - [`Network`] — cluster-internal node-to-node communication (marker)
//! - [`Executor`] — task spawning and `block_on`
//!
//! **The simulation impls are reproducible only under single-threaded drive.**
//! [`SimulatedTime`] and [`SimulatedRandom`] fix the *content* of the clock and
//! byte stream, but not the *order* in which concurrent OS threads consume them.
//! Bit-for-bit replay therefore requires running the workload on the
//! deterministic [`SimScheduler`] (one task at a time), not on the multi-threaded
//! `tokio` runtime. [`SimulatedExecutor`] only counts spawns; it does not by
//! itself confer determinism.
//!
//! ## Harness building blocks
//!
//! - [`SimScheduler`] — a single-threaded deterministic async scheduler
//! - [`FaultSchedule`] — a seeded, reproducible timeline of injected [`Fault`]s
//! - [`InvariantEngine`] — a generic, step-aware invariant checker
//! - [`ddmin`] — delta-debugging trace shrinker

#![warn(missing_docs)]

pub mod executor;
pub mod invariant;
pub mod network;
pub mod random;
pub mod schedule;
pub mod shrink;
pub mod sim_scheduler;
pub mod time;

// ── Curated public surface ─────────────────────────────────────────────

// Non-determinism traits and their built-in implementations.
pub use executor::{timeout, Executor, ProductionExecutor, SimulatedExecutor, TimedOut};
pub use network::{Delivery, Network, ProductionNetwork, SimulatedNetwork};
pub use random::{ProductionRandom, Random, SimulatedRandom};
pub use time::{ProductionTime, SimulatedTime, Time};

// Harness extensions: seeded fault schedules, delta-debug shrinking, and a
// domain-agnostic invariant engine.
pub use invariant::{Invariant, InvariantEngine, Violation};
pub use schedule::{Fault, FaultSchedule};
pub use shrink::ddmin;
pub use sim_scheduler::{sleep as sim_sleep, yield_now as sim_yield_now, SimScheduler};
