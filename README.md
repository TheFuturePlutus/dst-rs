# dst-rs

**Deterministic simulation testing for Rust — replay any run from a seed, inject faults, and auto-shrink failures to a minimal reproducer.**

[![CI](https://github.com/navian-ai/dst-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/navian-ai/dst-rs/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/dst-rs.svg)](https://crates.io/crates/dst-rs)
[![docs.rs](https://img.shields.io/docsrs/dst-rs)](https://docs.rs/dst-rs)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](./LICENSE)

> The crates.io and docs.rs badges go live with the `v0.1.0` release; until the
> crate is published they will show as "not found".

Concurrent and async bugs are the ones that pass every test and then page you at
3am: they need a crash, a reorder, or a dropped packet to land in a microsecond
window you can't reproduce on demand. Deterministic simulation testing (DST)
takes control of the clock, the RNG, the network, and task scheduling, drives
them all from a single seed, and injects faults on a reproducible timeline — so a
failing run replays **bit-for-bit** and can be shrunk to the handful of events
that actually caused it. `dst-rs` is a small, domain-agnostic substrate for doing
this in ordinary Rust: no forked runtime, no rewrite — you inject four traits and
loop over seeds.

## Two things: a library, and a plugin that installs it

- **`dst-rs`** (`crates/dst-rs`) — the DST substrate. The deterministic
  clock/RNG/network/executor, a single-thread scheduler, seeded fault schedules,
  an invariant engine, and a delta-debugging shrinker.
- **`dst-init`** (`plugin/`, `.cursor/rules/`) — the AI-native install layer.
  Point a coding agent (Claude Code or Cursor) at a Rust crate and it does the
  mechanical refactor for you: finds the determinism leaks, rewrites the time
  seams so your types become injectable, hand-finishes what the tool can't safely
  touch, and scaffolds a seed-loop DST test. It drives the **`dst` CLI**
  (`crates/dst-cli`: `scan` + `migrate`) under the hood.

The barrier to DST has always been the boring, repetitive refactor of threading a
clock/RNG/network through an existing codebase. That refactor is exactly what an
agent does perfectly — so the library ships with the thing that installs it.

## Quickstart

```bash
cargo add dst-rs
```

A seed-loop test: sweep many seeds, inject crashes on a reproducible timeline, and
assert the one invariant that matters — *every acknowledged write survives every
crash*. This is the shape of every DST test (it mirrors the `kv_chaos` example):

```rust
use std::collections::HashMap;
use dst_rs::{Fault, FaultSchedule, Random, SimulatedRandom};

#[test]
fn acknowledged_writes_survive_every_crash() {
    for seed in 0..500u64 {
        let rng = SimulatedRandom::from_seed(seed);
        let schedule = FaultSchedule::generate(seed, /*steps*/ 40, /*fault %*/ 35);
        let (mut durable, mut cache) = (HashMap::new(), HashMap::new());
        let mut acknowledged: HashMap<u64, u64> = HashMap::new();

        for step in 0..schedule.len() {
            match schedule.at(step) {
                Fault::CrashRestart => cache = durable.clone(), // recover from durable
                _ => {
                    let (k, v) = (rng.next_u64() % 8, rng.next_u64());
                    durable.insert(k, v); // persist BEFORE ack — the correct order
                    cache.insert(k, v);
                    acknowledged.insert(k, v);
                }
            }
            for (k, v) in &acknowledged {
                assert_eq!(cache.get(k), Some(v), "seed {seed}: lost an acked write");
            }
        }
    }
}
```

Swap `persist BEFORE ack` for the reverse and the test fails — deterministically,
on a specific seed you can re-run forever. That's the whole point.

## The three pillars

1. **Deterministic replay.** A seeded `SimulatedRandom`, a `FaultSchedule`, and the
   single-thread `SimScheduler` make a whole concurrent workload replay identically.
   Same seed ⇒ identical trace; different seed ⇒ different trace.
2. **Fault injection.** A `FaultSchedule` is a seeded, reproducible timeline of
   `Fault`s (`TransientError` / `CrashRestart` / `ClockSkew`). `SimulatedNetwork`
   turns that timeline into delivery outcomes (delivered / delayed / failed /
   dropped), so a flaky link is deterministic and replayable.
3. **Automatic shrinking.** `ddmin` (Zeller & Hildebrandt delta-debugging) reduces a
   bloated failing trace to a **1-minimal** reproducer: removing any single remaining
   fault makes the failure disappear.

These sit on four injected traits — `Time`, `Random`, `Network`, `Executor` — each
with a zero-overhead production impl (direct `std`/`tokio`/`rand` calls) and a
deterministic simulation impl. You inject the simulated ones under test and keep
the production ones in shipped code.

## Examples

Two runnable examples show *correct* systems holding their invariants under chaos —
real output, no benchmarks invented:

```console
$ cargo run --example hello_retry
hello_retry — retry-with-backoff over a fault-injected network
  seeds run:            500
  link density:         60% faulty, 16 steps/seed
  max attempts/call:    20
  total attempts:       818
  worst-case attempts:  9
  avg attempts/call:    1.64
  invariant violations: 0
  RESULT: all 500 seeds held the invariant ✓

$ cargo run --example kv_chaos
kv_chaos — a correct KV store under crash/fault injection
  seeds run:            500
  fault density:        35%, 40 steps/seed
  total writes:         17631
  total crash/recovers: 2369
  invariant violations: 0
  RESULT: all 500 seeds held durability across every crash ✓
```

And the credibility test — a deliberately-planted durability bug (a ledger that
acks a deposit *before* it is durable). The harness sweeps seeds, **finds** a
failing one, and `ddmin` **shrinks** it to a single `CrashRestart`; the correct
ledger survives the same schedules, proving the harness catches the bug, not merely
the presence of faults:

```bash
cargo test --test catches_planted_bug -- --nocapture   # prints the failure report
SEED=7 cargo test --test catches_planted_bug            # re-run one seed for triage
```

## Install it into your crate: `/dst-init`

If you use Claude Code or Cursor, you don't do the refactor by hand. Install the
plugin and run one command:

```text
/plugin marketplace add /absolute/path/to/dst-rs/plugin
/plugin install dst-init
/dst-init            # or: /dst-init crates/my-crate
```

The agent runs the playbook end to end:

1. **Scan** — `dst scan --json` maps every determinism leak by category (time,
   random, network, concurrency).
2. **Migrate** — `dst migrate` applies the seam-safe TIME rewrites: it adds
   `time: Arc<dyn dst_rs::Time>` to your structs (defaulted to the real clock, so
   existing callers are unchanged) and routes the leak call sites through it, then
   runs `cargo check` and auto-restores on any failure.
3. **Scaffold** — a seed-loop DST test for one migrated struct, modeled on the
   examples: inject `SimulatedTime`, loop seeds under a `FaultSchedule`, assert an
   invariant with `InvariantEngine`.
4. **Verify** — `cargo check` + `cargo test`, then it reports what changed and the
   `SEED=<n> cargo test …` replay hint.

Cursor users copy `.cursor/rules/dst.mdc` into their project and just ask *"make
this crate deterministically testable"* — same playbook. See
[`plugin/README.md`](plugin/README.md) for details, including why there is
deliberately **no MCP server**.

### Incremental adoption

None of this is all-or-nothing. `dst migrate` defaults every injected field to the
production impl, so a partially-migrated crate compiles and ships after **every
single step**. You can migrate one file, review the diff, run your suite, and stop —
each seam you add is independently useful, and the payoff (replayable failures)
starts the moment you have one DST test.

## How it compares

`dst-rs` is **not** the first DST tool for Rust, and doesn't claim to be. The honest
positioning:

- **[turmoil](https://github.com/tokio-rs/turmoil)** intercepts tokio's network and
  time to simulate a cluster — excellent for network-partition testing, scoped to
  networked tokio apps.
- **[madsim](https://github.com/madsim-rs/madsim)** is a deterministic async runtime
  that *replaces* tokio wholesale — very powerful, but you adopt its runtime.

`dst-rs` is a **substrate, not a runtime**: you inject four small traits into
ordinary code, incrementally, with no runtime swap — and it bundles the whole DST
loop in one place (replay **+** seeded fault schedules **+** an invariant engine
**+** `ddmin` shrinking), plus the **AI-native install layer** that does the
adoption refactor for you. The combination — an all-in-one loop you can drop into an
existing crate, installed by an agent — is the differentiation, not "first."

## Workspace layout

```
crates/
  dst-rs/     # the DST substrate (library) — traits, scheduler, faults, invariants, ddmin
  dst-cli/    # the `dst` binary — scan (leak detector) + migrate (seam-safe rewriter)
plugin/       # Claude Code plugin: /dst-init command, dst skill, dst-migrator subagent
.cursor/      # the same playbook as a Cursor project rule
docs/         # launch blog post and design notes
```

## Testing

```bash
cargo test --workspace     # dst-rs (74) + dst-cli (18)
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

## License

Licensed under the [Apache License, Version 2.0](./LICENSE).
