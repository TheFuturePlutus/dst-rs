---
name: dst
description: Deep reference for making a Rust crate deterministically testable with dst-rs. Conventions for injecting harness-controlled Time/Random/Network/Executor seams, the boundary between what dst-cli migrate rewrites automatically and what the agent hand-finishes, a real seed-loop DST test template, and the invariants of a safe migration. Use when running /dst-init or when a user asks to make a crate replayable / deterministically testable / fault-injectable.
---

# Making a crate deterministically testable with dst-rs

`dst-rs` is a domain-agnostic deterministic-simulation-testing substrate. It
replaces the four sources of non-determinism that make concurrent/async bugs
un-reproducible — the **clock, the RNG, the network, and task scheduling** — with
seed-driven, harness-controlled implementations. Once a crate routes those
through injectable seams, a failing run replays bit-for-bit from a seed and its
failing fault set shrinks (via `ddmin`) to the few events that matter.

This skill is the reference behind the `/dst-init` command. It assumes the
already-built `dst-cli` tools (`dst scan`, `dst migrate`) do the mechanical work;
your job is to drive them and hand-finish what they can't safely rewrite.

## The prime invariant of a migration

**Never leave the tree un-compiling.** Every step is seam-safe:

1. Inject a new field (`time`/`rng`/…) typed as `Arc<dyn Trait>`.
2. **Default it to the production impl in every constructor** — so all existing
   callers compile and behave identically. Production is zero-overhead.
3. Rewrite the leak call sites to go through the field.

Partial migration is a feature, not a defect: you can migrate one struct, ship,
and migrate the next later. **Prefer a false negative** (leave a leak, report it)
**over a wrong rewrite.** If an edit turns the tree red, fix or revert it before
doing anything else. Verify with `cargo check` after each edit.

## The tools (real surface — do not invent flags)

`dst` is the binary (crate `dst-cli`); in-repo run it as `cargo run -q -p dst-cli --`.

### `dst scan [PATH]`
Static determinism-leak detector. Flags:
- `--json` — emit the machine-readable leak array and nothing else.
- `--deny` — exit non-zero if any leaks are found (turns scan into a CI gate).

JSON contract — an array of objects, each: `file`, `line`, `col`, `category`
(`time` | `random` | `network` | `concurrency`), `snippet`, `fn` (enclosing
function, or null). No other shape.

### `dst migrate [PATH]`
Rewrites a conservative, seam-safe subset of leaks. Flags:
- `--dry-run` — print a unified diff, write nothing.
- `--traits <list>` — comma-separated trait families. **v1 supports only `time`**
  (any other value is a hard error). Default is `time`.

After applying (non-dry-run) it runs `cargo check` itself and **restores the
originals if the check fails** — which is why `dst-rs` must already be a
dependency before you migrate (see below). Its summary prints
`structs migrated`, `leaks rewritten`, `leaks skipped`, and the check outcome.

## Prerequisite before migrate: dst-rs must be a dependency

The rewrites reference `dst_rs::Time` and `dst_rs::ProductionTime`. If `dst-rs`
isn't in the crate's `Cargo.toml`, migrate's post-rewrite `cargo check` fails and
it auto-restores — you'll see "leaks rewritten" followed by "cargo check: FAILED
— original files RESTORED" and no actual change. So, **right after scan and
before migrate**:

```bash
cargo add dst-rs --manifest-path <crate>/Cargo.toml          # runtime use
# cargo add dst-rs --dev --manifest-path <crate>/Cargo.toml  # test-only use
```

If `dst-rs` isn't published, add a path/workspace dep by hand
(`dst-rs = { path = "…/crates/dst-rs" }`) and confirm with `cargo check`.

## What migrate handles automatically vs. what you hand-finish

**Automatic (v1):** TIME leaks inside **inherent methods of named-field
structs**. It adds `time: std::sync::Arc<dyn dst_rs::Time>`, defaults it to
`dst_rs::ProductionTime` in every constructor, and rewrites:

| Leak idiom                              | Rewrite                    |
|-----------------------------------------|----------------------------|
| `SystemTime::now()…as_millis() as i64`  | `self.time.now_ms()`       |
| `Instant::now()`                        | `self.time.instant_now()`  |
| `tokio::time::sleep(d).await`           | `self.time.sleep(d).await` |

**You hand-finish (migrate reports these as "skipped"):**

- **TIME in free functions** — no `self` to hang a clock off. Thread an explicit
  parameter and update callers, or move the logic onto a clock-owning struct:
  ```rust
  pub fn boot_timestamp(time: &dyn dst_rs::Time) -> i64 { time.now_ms() }
  ```
- **TIME in trait methods** — add the clock to the implementing struct's state
  and route through `self.time`; keep the trait signature stable.
- **RANDOM** (`rand::random`, `rand::thread_rng`, `.gen()`, `.gen_range()`,
  `uuid::Uuid::new_v4`, `fastrand::*`) — add `rng: Arc<dyn dst_rs::Random>`
  defaulted to `dst_rs::ProductionRandom`; route through `self.rng.next_u64()`,
  `self.rng.next_uuid()`, `self.rng.shuffle_u64(&mut xs)`.
- **NETWORK** (`reqwest`, `std::net::{TcpStream,TcpListener,UdpSocket}`,
  `tokio::net::*`) — hide behind a trait the test fakes; DST supplies
  `SimulatedNetwork` driven by a `FaultSchedule` (delivered / delayed / failed /
  dropped outcomes).
- **CONCURRENCY** (`std::thread::spawn`, `tokio::spawn`) — route spawning through
  an injected `Executor` (`ProductionExecutor` in prod,
  `SimulatedExecutor` / `SimScheduler` under test).

Same rule throughout: default every injected field to the production impl so
callers compile unchanged.

## The injection seam for tests

`migrate` keeps `new` on the production default, which is correct for shipped
code but gives a test no way to pass a `SimulatedTime`. Add a **test-only
injecting constructor** next to `new`:

```rust
impl RateLimiter {
    /// Production default — unchanged callers.
    pub fn new(tokens: u32) -> Self {
        Self { time: std::sync::Arc::new(dst_rs::ProductionTime), tokens, last_ms: 0 }
    }
    /// DST seam: inject a harness-controlled clock.
    pub fn with_time(tokens: u32, time: std::sync::Arc<dyn dst_rs::Time>) -> Self {
        Self { time, tokens, last_ms: 0 }
    }
}
```

## The real dst-rs API you'll use in tests (no fictional methods)

- **Time**: `SimulatedTime::new(start_ms: i64)`; `.advance_ms(delta: i64)`;
  `.set_to_ms(ms)`; `.current_ms()`. Trait methods: `now_ms() -> i64`,
  `instant_now() -> Instant`, `async sleep(Duration)`. Production impl:
  `ProductionTime`. `SimulatedTime::sleep` returns cost-free when the clock is
  advanced past the deadline — advance it from another task under `#[tokio::test]`.
- **Random**: `SimulatedRandom::from_seed(seed: u64)`; `.next_u64()`,
  `.next_uuid()`, `.shuffle_u64(&mut [u64])`. Production: `ProductionRandom`.
- **FaultSchedule**: `generate(seed, steps, density)`, `quiescent(steps)`,
  `from_faults(vec)`, `.at(step) -> Fault`, `.fault_steps() -> Vec<usize>`,
  `.fault_count()`, `.iter()`. `Fault` = `None | TransientError | CrashRestart |
  ClockSkew(i64)`.
- **InvariantEngine<S>**: `new(vec![Invariant::new("name", |s: &S| bool)])`;
  `.check(step, &state) -> Option<Violation>` (first failure);
  `.check_all(step, &state) -> Vec<Violation>`. `Violation { step, invariant }`.
- **SimulatedNetwork**: `new(FaultSchedule)`, `.send(v) -> Delivery`
  (`Delivered | Delayed{..} | Failed | Dropped`).
- **Shrinking**: `ddmin(&fault_points, |subset| still_fails(subset)) -> minimal`.

## Seed-loop DST test template

Copy this, adapt the crate name, struct, and invariant. It is modeled directly
on `crates/dst-rs/examples/kv_chaos.rs` (seed loop + `FaultSchedule` + `Fault`
match + invariant) and injects a `SimulatedTime` into the migrated struct. Every
method used above is real dst-rs API. This scaffold is **synchronous** — the
simplest green test; for a struct whose leak is `sleep`, use `#[tokio::test]` and
advance the clock from a spawned task (see `time.rs`'s
`simulated_sleep_returns_when_clock_advances`).

```rust
//! Seed-loop DST test for the migrated `RateLimiter`.
//! Scaffolded by /dst-init. Replay a failing seed bit-for-bit with:
//!   SEED=<n> cargo test dst_ratelimiter -- --nocapture

use std::sync::Arc;

// Replace `your_crate` with this crate's package name (underscored).
use your_crate::RateLimiter;
use dst_rs::{Fault, FaultSchedule, Invariant, InvariantEngine, Random, SimulatedRandom, SimulatedTime};

/// The world the invariants observe after each step.
struct World {
    now_ms: i64,
    prev_ms: i64,
}

fn invariants() -> InvariantEngine<World> {
    InvariantEngine::new(vec![
        // A correctly-injected clock seam must read monotonically while the
        // harness only advances the simulated clock forward. A wrong rewrite
        // (e.g. reading real time, or caching a stale instant) breaks this.
        Invariant::new("clock_monotonic", |w: &World| w.now_ms >= w.prev_ms),
    ])
}

#[test]
fn dst_ratelimiter_clock_seam_holds_across_seeds() {
    const SEEDS: u64 = 200;
    const STEPS: usize = 32;
    const DENSITY: u64 = 30; // % of steps carrying a fault

    // SEED=<n> runs a single seed for triage (mirrors the dst-rs examples).
    let seeds: Vec<u64> = match std::env::var("SEED").ok().and_then(|s| s.parse().ok()) {
        Some(s) => vec![s],
        None => (0..SEEDS).collect(),
    };

    let eng = invariants();

    for seed in seeds {
        // Deterministic clock, seeded RNG, seeded fault timeline — all replayable.
        let clock = Arc::new(SimulatedTime::new(1_700_000_000_000));
        let rng = SimulatedRandom::from_seed(seed);
        let schedule = FaultSchedule::generate(seed, STEPS, DENSITY);

        // Inject the simulated clock via the seam added during migration.
        let limiter = RateLimiter::with_time(8, clock.clone());
        let mut prev_ms = limiter.now_ms();

        for step in 0..STEPS {
            // Advance virtual time by a seed-derived positive delta (cost-free).
            let delta = 1 + (rng.next_u64() % 250) as i64;
            match schedule.at(step) {
                // A clock skew is an extra forward jump; a correct seam stays
                // monotonic because the harness only ever moves the clock ahead.
                Fault::ClockSkew(ms) => clock.advance_ms(delta + ms.abs()),
                _ => clock.advance_ms(delta),
            }

            let now = limiter.now_ms();
            let world = World { now_ms: now, prev_ms };
            if let Some(v) = eng.check(step, &world) {
                panic!(
                    "seed {seed}: invariant `{}` violated at step {step} (now={now}, prev={prev_ms}). \
                     Replay: SEED={seed} cargo test dst_ratelimiter -- --nocapture",
                    v.invariant
                );
            }
            prev_ms = now;
        }
    }
}
```

For a durability-style struct (owns state, must survive `Fault::CrashRestart`),
follow `kv_chaos.rs` instead: track an `acknowledged` ground-truth map, call your
struct's `recover()` on `Fault::CrashRestart`, and assert every acknowledged
write reads back. When a seed fails, feed `schedule.fault_steps()` to `ddmin` to
shrink it to the minimal failing fault set.

## Definition of done

1. `dst scan --json` summarized to the user.
2. `dst-rs` is a dependency (added if it was missing).
3. `dst migrate` applied; its summary reported; check PASSED.
4. Every skipped leak hand-finished by the same seam convention, or explicitly
   left with a reason.
5. At least one seed-loop DST test added and green.
6. `cargo check` and `cargo test` both green.
7. Replay hint (`SEED=<n> cargo test …`) handed to the user.
