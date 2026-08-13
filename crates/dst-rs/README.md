# dst-rs

**Deterministic simulation testing for async Rust — replay any run from a seed, inject faults, and auto-shrink failures to a minimal reproducer.**

[![Crates.io](https://img.shields.io/crates/v/dst-rs.svg)](https://crates.io/crates/dst-rs)
[![Docs.rs](https://docs.rs/dst-rs/badge.svg)](https://docs.rs/dst-rs)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](../../LICENSE)

`dst-rs` is a domain-agnostic DST substrate. It replaces the sources of
non-determinism that normally make concurrent/async bugs impossible to reproduce
— the clock, the RNG, the network, and task scheduling — with harness-controlled,
seed-driven implementations. A failing run can then be replayed bit-for-bit and
its failing fault set shrunk to the handful of events that actually matter.

It's a **substrate, not a runtime**: you inject four small traits into ordinary
code — no forked async runtime, no rewrite. Each trait has a zero-overhead
production impl (direct `std`/`tokio`/`rand` calls) and a deterministic simulation
impl you swap in under test.

## Install

```bash
cargo add dst-rs
```

## The three pillars

1. **Deterministic replay** — a seeded `SimulatedRandom`, `FaultSchedule`, and the
   single-thread `SimScheduler` make a whole concurrent workload replay identically.
   Same seed ⇒ identical trace; different seed ⇒ different trace.
2. **Fault injection** — a `FaultSchedule` is a seeded, reproducible timeline of
   `Fault`s (`TransientError` / `CrashRestart` / `ClockSkew`). `SimulatedNetwork`
   turns that timeline into delivery outcomes (delivered / delayed / failed /
   dropped) so a flaky link is deterministic and replayable.
3. **Automatic shrinking** — `ddmin` (Zeller & Hildebrandt delta-debugging) reduces
   a bloated failing trace to a **1-minimal** reproducer: removing any single
   remaining element makes the failure disappear.

## Usage

```rust
use dst_rs::{ddmin, Delivery, FaultSchedule, SimulatedNetwork};

// 1. Build a seeded fault timeline and a network that obeys it.
let schedule = FaultSchedule::generate(/* seed */ 42, /* steps */ 32, /* density% */ 30);
let net = SimulatedNetwork::new(schedule.clone());

// 2. Run your workload; the network fails/delays/drops per the seeded schedule.
for i in 0..8 {
    match net.send(i) {
        Delivery::Delivered(v)                => println!("ok {v}"),
        Delivery::Delayed { value, delay_ms } => println!("late {value} (+{delay_ms}ms)"),
        Delivery::Failed                      => println!("retryable error"),
        Delivery::Dropped                     => println!("silently dropped"),
    }
}

// 3. When a run fails, shrink the fault points to a minimal reproducer.
let fault_points: Vec<usize> = schedule.fault_steps();
let minimal = ddmin(&fault_points, |subset| /* does this subset still fail? */ !subset.is_empty());
println!("minimal failing faults: {minimal:?}");
```

## Examples

Two runnable examples show correct systems holding their invariants under chaos:

```bash
# Retry-with-backoff over a fault-injected network. Invariant: if the link was
# clear on any attempt, the client returns Ok. Holds across 500 seeds.
cargo run --example hello_retry

# A persist-before-ack KV store under crash injection. Invariant: every
# acknowledged write survives every crash/recovery. Holds across 500 seeds.
cargo run --example kv_chaos
```

## The credibility test

`tests/catches_planted_bug.rs` contains a **deliberately-planted** durability bug
(a ledger that acknowledges a write before it is durable). The harness sweeps a
fixed seed range under fault injection, **finds** a failing seed, and `ddmin`
**shrinks** it to a single `CrashRestart` — the minimal reproducer. The correct
ledger survives the same schedules, proving the harness catches the bug, not
merely the presence of faults.

```bash
cargo test --test catches_planted_bug -- --nocapture   # prints the failure report
SEED=7 cargo test --test catches_planted_bug            # re-run one seed for triage
```

## Testing

```bash
cargo test   # lib unit tests + replay / faults / shrinking / catches_planted_bug
```

## Install it into your own crate

The mechanical refactor of threading a clock/RNG/network through an existing
codebase is exactly what a coding agent does perfectly. The
[dst-rs project](https://github.com/TheFuturePlutus/dst-rs) ships an **AI-native install
layer** (`/dst-init` for Claude Code, a rule for Cursor) plus the **`dst` CLI**
(`dst scan` + `dst migrate`) that does it for you — scan the leaks, rewrite the
time seams, scaffold a seed-loop test. See the project README for the full flow.

## License

Licensed under the [Apache License, Version 2.0](../../LICENSE).
