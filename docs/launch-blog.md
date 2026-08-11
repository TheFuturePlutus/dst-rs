# The library that installs itself: deterministic simulation testing for Rust

## A write that was acknowledged, and then wasn't

Picture the most ordinary code path in any financial system. A request comes in to
credit an account. The service writes the deposit, tells the client *"saved,"* and
moves on. The client — another service, a mobile app, a partner bank — takes that
acknowledgement as truth. The money is now real to everyone downstream.

Now put a crash in the wrong place. The service acknowledges the write *before* the
write is actually durable, and the process dies in the microsecond window between
the two. The client was told the deposit succeeded. Durable storage never got it.
On recovery, the money is simply gone — acknowledged, and lost.

This is not an exotic bug. It's an ordering bug, one line out of place, and it
passes every test you have. Your unit tests don't crash the process; your
integration tests don't crash it *at that exact instruction*. You could run the
happy path a billion times and never see it, because the failure needs three
things to coincide — a write, an acknowledgement, and a crash — inside a window
measured in microseconds. Production will eventually line them up for you, at the
worst possible time, with no reproducer.

**`dst-rs` is built to line them up on purpose.** It's a deterministic simulation
testing (DST) substrate for Rust: it takes control of time, randomness, the
network, and scheduling, drives them all from a single seed, and injects crashes
and faults on a reproducible timeline. Somewhere in the seed space is a schedule
that crashes right after the acknowledgement — and DST will find it, then shrink
the failing run down to the one fault that mattered: *crash immediately after ack.*

That's not a hypothetical. It's a real test in the repo. `catches_planted_bug.rs`
plants exactly this durability bug — a ledger that acks before it persists — sweeps
a fixed seed range under fault injection, finds a failing seed, and uses
delta-debugging to reduce the failing schedule to a **single `CrashRestart`**. The
correct ledger, persist-before-ack, survives every one of those same schedules. The
harness catches the *bug*, not merely the presence of faults.

## Who this is for

If you write stateful services in Rust — ledgers, queues, state machines,
replicated stores, anything where "we told someone it happened" must imply "it
happened" — this is for you. DST is the technique behind FoundationDB's legendary
reliability and behind Antithesis; it reaches the crash-in-the-window class of bug
that ordinary tests structurally cannot. `dst-rs` makes it a small library you drop
into an existing crate, not a platform you migrate to.

## Why now

DST has been the gold standard for a decade, and almost nobody does it. The reason
isn't that the idea is hard — it's that the *adoption* is tedious. To make a
codebase replayable you have to hunt down every call to the wall clock, every
`rand::random`, every network handle, every `spawn`, and thread an injectable
abstraction through all of them. It's a large, mechanical, error-prone refactor
with no payoff until it's mostly done. That barrier is why DST stayed the province
of a few elite infra teams.

What changed is that the barrier is now *exactly* the kind of work a coding agent
does perfectly: find every instance of a mechanical pattern and rewrite it
consistently, keeping the tree compiling. So `dst-rs` ships with the thing that
installs it. **The library that installs itself.**

## What DST actually is

Strip away the mystique and DST is three moves, resting on one idea: **the only
reason concurrent bugs are irreproducible is that a few inputs are hidden.** Name
those inputs, seed them, and reproducibility falls out.

`dst-rs` names them as four injected traits, each with a real production impl and a
deterministic simulation impl:

- **`Time`** — wall-clock, monotonic, and async sleep
- **`Random`** — RNG and UUID generation
- **`Network`** — node-to-node delivery
- **`Executor`** — task spawning and `block_on`

On top of those sit the three pillars:

1. **Deterministic replay.** A seeded RNG, a `FaultSchedule`, and a single-thread
   `SimScheduler` make an entire concurrent workload replay bit-for-bit. Same seed,
   same trace — every time.
2. **Fault injection.** A `FaultSchedule` is a seeded timeline of faults —
   `TransientError`, `CrashRestart`, `ClockSkew` — and a `SimulatedNetwork` turns it
   into delivered / delayed / failed / dropped outcomes. A hostile, flaky world,
   perfectly reproducible.
3. **Automatic shrinking.** When a run fails, `ddmin` (Zeller & Hildebrandt
   delta-debugging) strips the failing trace down to a 1-minimal reproducer:
   remove any remaining fault and the failure vanishes. You don't debug a
   thousand-event trace; you debug one crash.

A DST test is then just a loop over seeds asserting an invariant. Here is the shape,
straight from the `kv_chaos` example — a store that persists before it acks, checked
against *every acknowledged write survives every crash*:

```rust
for seed in 0..500u64 {
    let rng = SimulatedRandom::from_seed(seed);
    let schedule = FaultSchedule::generate(seed, 40, 35); // 40 steps, 35% fault density
    let mut store = KvStore::new();
    let mut acknowledged: HashMap<u64, u64> = HashMap::new();

    for step in 0..40 {
        match schedule.at(step) {
            Fault::CrashRestart => store.recover(), // reload cache from durable
            _ => {
                let (k, v) = (rng.next_u64() % 8, rng.next_u64());
                let (k, v) = store.put(k, v);       // persist FIRST, then ack
                acknowledged.insert(k, v);
            }
        }
        for (k, v) in &acknowledged {
            assert_eq!(store.get(k), Some(*v), "seed {seed}: lost an acked write");
        }
    }
}
```

Across 500 seeds and ~2,400 injected crashes, the correct store never loses a write.
Flip `put` to ack-before-persist and it fails — on a specific seed you can replay
forever with `SEED=<n>`.

## The install layer

Adopting `dst-rs` means making your types injectable. The unit of work is turning
this:

```rust
struct RateLimiter { tokens: u32 }

impl RateLimiter {
    fn refill(&mut self) {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis();
        // ...decide refill from `now`...
    }
}
```

into this — the clock becomes a seam, defaulted to the real clock so every existing
caller compiles unchanged:

```rust
struct RateLimiter { tokens: u32, time: Arc<dyn dst_rs::Time> }

impl RateLimiter {
    fn refill(&mut self) {
        let now = self.time.now_ms();
        // ...decide refill from `now`...
    }
}
```

Multiply that by every clock, RNG, socket, and spawn in the codebase and you have
the refactor that has scared teams off DST for ten years. So we automated it. The
`dst` CLI has two subcommands — `scan` finds the leaks (time / random / network /
concurrency), and `migrate` performs the seam-safe time rewrite above, adds the
field, defaults it to production, and runs `cargo check`, rolling back on any
failure. What `migrate` can't safely rewrite, it reports.

That's where the AI layer closes the gap. `dst-rs` ships a Claude Code plugin and a
Cursor rule: you run `/dst-init`, and the agent drives `scan`, runs `migrate`,
hand-finishes the leaks the tool deliberately skips (free functions, RNG, network,
concurrency) using the same injection convention, and scaffolds a seed-loop test
against a real invariant — keeping the crate compiling and shippable after every
single step. Incremental by construction: migrate one file, review, stop, continue
tomorrow.

This is the broader idea we think matters beyond this one library: an **agent-native
SDK**. Instead of a README a human executes, ship the capability to an agent in four
parts — *describe* the tool (the skill), *teach* the conventions and invariants
(the reference), *do* the mechanical work (the CLI and the migration playbook), and
*verify* the result (`cargo check` + `cargo test` + a replay hint). Describe, teach,
do, verify. The library doesn't just document how to adopt it — it adopts itself
into your codebase and leaves a passing test behind.

## Honest positioning

`dst-rs` is not the first DST tool for Rust and doesn't pretend to be.
[turmoil](https://github.com/tokio-rs/turmoil) simulates a tokio network for
partition testing; [madsim](https://github.com/madsim-rs/madsim) is a full
deterministic runtime that replaces tokio. Both are excellent. `dst-rs` makes a
different bet: a *substrate, not a runtime* — four small traits you inject
incrementally into ordinary code, no runtime swap — bundling the whole loop (replay
+ seeded faults + an invariant engine + `ddmin` shrinking) in one place, plus the
agent-driven install that does the adoption for you. The all-in-one loop you can
drop into an existing crate, installed by an agent, is the differentiation — not
being first.

The crash-after-ack bug will happen in production eventually. The only question is
whether you meet it first, on a seed, with a one-line reproducer — or at 3am, with
none.

*`dst-rs` is Apache-2.0. `cargo add dst-rs`, run the examples, and try `/dst-init`
on a crate you own.*
