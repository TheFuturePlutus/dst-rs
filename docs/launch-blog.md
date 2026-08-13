# The library that installs itself
*An incremental adoption layer for deterministic simulation in existing Rust services*

*Repository, crates.io, and generated API-doc links go live at the `v0.1.0` release.*

`navian-dst` is not a new deterministic runtime. It addresses the *adoption* problem
around deterministic simulation testing (DST): finding ambient nondeterminism in an
existing service, introducing injectable seams for it, constructing a replayable
invariant test, and shrinking failures to a minimal reproducer — without an
all-at-once runtime migration. Its distinctive layer is the scanner, the migrator,
and the coding-agent workflow that perform that adoption incrementally.

## The bug you can't reproduce

Many high-throughput systems put a cache in front of their database. Checking the
source of truth on every read is exactly the coordination cost the cache exists to
avoid — so you keep two copies of the truth, with one rule: when the data changes,
invalidate the cache.

Here is the race. A read misses the cache and goes to fetch the current value. At the
same instant, a write updates the database and fires the invalidation. If the
invalidation lands *before* the slower read stores what it fetched, the read
repopulates the cache with the value it grabbed a moment earlier — the old one. The
cache is now stale until something ages it out: the item still shows "in stock" after
it sold out; the profile shows the address the user just changed away from.

You can prevent this race — with coordination, versioning, or fencing — but each
option spends complexity, latency, or the throughput the cache was for. And
conventional unit and integration tests rarely reproduce the critical interleaving:
the read and the write have to overlap inside a narrow window between the fetch and
the cache population, and tests run them one after another. Underneath it is an
*ordering* bug: two correct operations, one unlucky interleave. The same shape recurs
wherever you trade coordination for speed — a lost update between two
read-modify-writes, a reader acting on a value a transaction then rolls back, a
timed-out request retried while the original is still in flight.

Whether this is a *bug* depends on what the system promises. An intentionally
eventually-consistent cache is allowed to return stale data. But for a system that
promises read-after-write consistency — *any read that begins after an acknowledged
write completes must not return an older version* — a stale read is a safety
violation, not a latency blip. That distinction is exactly what Jepsen classifies as
a stale read.

The repository ships this exact race as a runnable, seed-replayable example
(`examples/stale_cache_race.rs`): across 500 seeds a version-fenced cache holds, while
the unfenced variant fails on 209 of them — reproducibly, and only under the
interleavings where the invalidation lands between the read's fetch and its cache
write. That is the scheduler exploring orderings, not a hand-sequenced script.

This class of bug is what the most reliable systems are built to hunt. FoundationDB
spent its first eighteen months running exclusively in simulation before sending its
first real network packet, and has accumulated an extraordinary volume of simulated
testing since
([FoundationDB engineering docs](https://apple.github.io/foundationdb/engineering.html)).
And Kyle Kingsbury is precise about what such testing can claim: *"Because Jepsen
tests are experiments, they can only prove the existence of errors, not their
absence."* ([jepsen.io/ethics](https://jepsen.io/ethics)) That epistemology carries
through everything below.

## What `navian-dst` actually is

The premise of DST: many concurrency failures become reproducible once their hidden
environmental inputs — time, randomness, scheduling, and I/O outcomes — are made
explicit and seeded.

`navian-dst` names four sources of nondeterminism as injectable traits, each with a real
production impl and a deterministic simulation impl:

- **`Time`** — wall-clock (`now_ms`), monotonic (`instant_now`), and async `sleep`
- **`Random`** — RNG and UUID generation
- **`Network`** — node-to-node delivery, with per-message fault outcomes
- **`Executor`** — task spawning and `block_on`

On top of those sit the machinery:

1. **Deterministic replay.** A seeded RNG, a `FaultSchedule`, and a single-thread
   `SimScheduler` make a concurrent workload replay bit-for-bit. Within the declared
   simulation boundary, the same binary, configuration, and seed reproduce the same
   trace.
2. **Fault injection.** A `FaultSchedule` is a seeded timeline of faults
   (`TransientError`, `CrashRestart`, `ClockSkew`). `SimulatedNetwork` maps message
   faults to delivered / delayed / failed / dropped outcomes; your test reads the
   schedule to drive modeled crash-and-recovery and clock-change events.
3. **Shrinking.** When a run fails, `ddmin` (Zeller–Hildebrandt delta-debugging)
   reduces the failing trace to a *1-minimal* fault set: remove any single remaining
   element and the failure disappears. You debug a small fault set rather than the
   original thousand-event trace — in the planted example, it shrinks to one crash.
4. **The `InvariantEngine`.** The properties you assert after each step. A violation
   is what turns a run into a *found bug*; a passing sweep records that *the tested
   seed campaign passed* — not that the invariant holds for all inputs. A finite seed
   sweep finds counterexamples; it does not prove their absence.

Two honest boundaries, stated up front:

- **There is no `Storage` trait.** `navian-dst` schedules crash events reproducibly; your
  application or test supplies the persistence and recovery model. A `CrashRestart` is
  a *modeled event* the harness delivers on the timeline — it invokes your recovery
  path; it does not terminate an OS process or intercept real `write`/`fsync`. The
  example below models its own durable/cache split precisely because the substrate
  does not model storage for you.
- **The scheduler explores *task-level* interleavings** at your `.await` points across
  a seed campaign — not arbitrary instruction-level preemption of real threads. Code
  that never routes through the seams is invisible to it. (More on coverage below.)

## A test is a loop over seeds

Here is the shape, from the `kv_chaos` example — a store that persists *before* it
acknowledges, checked against *every acknowledged write survives every modeled crash*:

```rust
for seed in 0..500u64 {
    let rng = SimulatedRandom::from_seed(seed);            // interior mutability — no `mut`
    let schedule = FaultSchedule::generate(seed, 40, 35);  // 40 steps, 35% fault density
    let mut store = KvStore::new();
    // BTreeMap: deterministic iteration order, so a failure surfaces the same way each run
    let mut acknowledged: BTreeMap<u64, u64> = BTreeMap::new();

    for step in 0..40 {
        match schedule.at(step) {
            Fault::CrashRestart => {
                store.recover();                           // modeled crash: rebuild cache from durable
                for (&k, &v) in &acknowledged {            // acked writes must survive
                    assert_eq!(store.get(k), Some(v), "seed {seed}: lost an acked write");
                }
            }
            // TransientError / ClockSkew / None: do a write, then read it back.
            _ => {
                let (k, v) = store.put(rng.next_u64() % 8, rng.next_u64()); // persist, THEN ack
                acknowledged.insert(k, v);
            }
        }
    }
}
```

Across 500 seeds and roughly 2,400 modeled crashes, the correct store never loses an
acknowledged write. Flip `put` to acknowledge-before-persist and a specific seed
fails — replay it forever with `SEED=<n>`. (The repo's `catches_planted_bug` test
drives that failing variant through the `SimScheduler` and `SimulatedNetwork`, with
the crash arriving as a real dropped delivery, and shrinks the schedule to the single
fault that matters.)

## Why now

If you write stateful Rust, none of this is news — you already know what a seeded
scheduler buys you. The news is *why you haven't done it*: adoption is a large,
mechanical, error-prone refactor — hunt down every wall-clock read, every
`rand::random`, every socket and `spawn`, and thread an injectable seam through all of
them — with no payoff until it's mostly done. That tax is why DST stayed the province
of specialized infrastructure teams.

What changed is that the tax is now repetitive, compiler-verifiable refactoring — work
well suited to a coding agent *when every step is bounded by compilation, tests, and
human review.* So `navian-dst` ships with the thing that installs it.

## The install layer

Adoption means making your types injectable. The unit of work is turning a type that
reads the ambient clock into one that takes a clock seam — defaulted to the real
clock, so existing callers are unaffected:

```rust
use std::sync::Arc;
use navian_dst::Time;

struct RateLimiter {
    tokens: u32,
    time: Arc<dyn Time>,
}

impl RateLimiter {
    // Existing callers use `new` unchanged — it defaults to the real clock.
    fn new(tokens: u32) -> Self {
        Self { tokens, time: Arc::new(navian_dst::ProductionTime::default()) }
    }

    // Tests inject a simulated clock.
    fn with_time(tokens: u32, time: Arc<dyn Time>) -> Self {
        Self { tokens, time }
    }

    fn refill(&mut self) {
        let now = self.time.instant_now(); // monotonic — a rate limiter wants Instant, not wall time
        // ...decide refill from elapsed monotonic time...
    }
}
```

Callers using `RateLimiter::new(..)` compile unchanged; a *direct* struct literal like
`RateLimiter { tokens }` must be updated. That is precisely the mechanical part the
tooling handles. The `navian-dst` CLI has two subcommands: `scan` finds the ambient
nondeterminism, and `migrate` performs the seam-safe rewrites — it adds the field,
defaults it to production in every constructor and struct literal it can see, rewrites
the leak sites, and runs `cargo check`. Anything it cannot rewrite safely — free
functions, `#[derive]`-blocked structs, RNG, network, concurrency — it *skips and
reports* rather than guessing.

`migrate` is designed to be safe on a real, dirty worktree: it never runs git, edits
files in place, supports `--dry-run` (which emits a reviewable patch and runs no
check), and on any `cargo check` failure restores the exact bytes it changed from an
in-memory snapshot — so a failed migration leaves the tree as it found it. It also
works on its own; the coding-agent workflow is a convenience on top, not a
requirement.

## An agent-native SDK: describe, teach, do, verify

The broader idea, beyond this one library: in the agent era an SDK can ship more than
a README a human executes. It can ship a capability an agent executes, in four parts —
*describe* the tool (a skill file), *teach* the conventions and invariants (a
reference doc), *do* the mechanical work (the CLI and migration playbook), and
*verify* the result (`cargo check` + `cargo test` + a `SEED=` replay hint). `navian-dst`
ships exactly this as a Claude Code plugin and a Cursor rule: run `/dst-init`, and the
agent drives `scan`, runs `migrate`, hand-finishes what `migrate` skips, and scaffolds
a seed-loop test — keeping the crate compiling after every step, one file at a time.

Every step remains bounded by compilation, tests, and a diff you review. The agent
does the typing; the engineer owns the invariant, the simulation boundary, and the
evidence.

## Testing the code your agent wrote

There is a sharper reason this matters now. As agents reduce the cost of *producing*
code, the bottleneck moves to *establishing confidence in it* — and among the hardest
defects agents can leave behind are the ordering and failure-path bugs that happy-path
tests rarely exercise. DST does not make the model deterministic, and it does not make
generated code "trustworthy." It subjects that code to reproducible failure schedules
and produces *evidence* about the specific invariants tested: either a counterexample
with a seed, or a record that the campaign passed. The agent wires and runs the tests;
engineers review the invariant, the simulation boundary, and the resulting evidence.
(That second thesis — who tests the failure paths of agent-written code — is a larger
argument than one v0.1 library should carry; it deserves its own article.)

## Running it in practice

**Does it keep my runtime?** Yes. The production `Executor` is tokio and the
production `Time`/`Random`/`Network` impls are the real thing — you inject seams, you
don't swap your runtime. Simulated impls appear only in tests.

**What does adoption look like?**

```text
$ navian-dst scan src/
== Determinism leaks ==

TIME (5)
  src/limiter.rs:28   [TIME]     SystemTime::now()   (in fn `refill`)
  src/session.rs:71   [TIME]     Instant::now()      (in fn `touch`)
RANDOM (3)
  src/token.rs:14     [RANDOM]   Uuid::new_v4()      (in fn `issue`)
NETWORK (2) · CONCURRENCY (1)

11 leaks across 4 files — run `navian-dst migrate` (or `/dst-init`) to wire the seams
```

**How does it fit CI?** A seed sweep is cheap — `kv_chaos` typically completes 500
seeds in under a second in our CI environment. Run a few hundred seeds per PR and a
larger sweep nightly; a failure prints the exact `SEED=<n>` to replay locally.

**Is it real?** It's `v0.1`, Apache-2.0, extracted from tooling we built for our own
event engine, with the replay / fault / shrink loop covered by its own test suite
(149 tests at the time of writing; the exact count is pinned to the `v0.1.0` tag).

## How it compares

`navian-dst` is not the first or the broadest DST tool for Rust, and the field is strong:

- **[turmoil](https://github.com/tokio-rs/turmoil)** — runs multiple simulated hosts
  on one thread and provides deterministic network and filesystem implementations,
  including latency, drops, partitions, crashes, and torn writes.
- **[madsim](https://github.com/madsim-rs/madsim)** — a deterministic,
  tokio-compatible *runtime* with simulated scheduling, time, RNG, network, and
  filesystem, plus compatible replacements or patches for several common dependencies
  (e.g. tokio, tonic, getrandom).
- **[shuttle](https://github.com/awslabs/shuttle)** — randomized concurrency
  scheduling (random and PCT) for larger concurrent components; probabilistic, and
  explicit that a passing test does not prove correctness.
- **[loom](https://github.com/tokio-rs/loom)** — *exhaustive* interleaving and
  memory-order permutation for small lock-free primitives under the C11 model (with
  documented incompleteness).

These control different surfaces, and the complementary tools — proptest/quickcheck
(inputs), kani (bounded proof), miri (UB) — sit on other axes again. `navian-dst` makes
one specific bet: a *substrate, not a runtime* — inject four traits incrementally into
ordinary code, no runtime swap — bundling replay + seeded faults + an invariant engine
+ shrinking, plus an agent-driven install. If you want the broadest coverage on day
one with a runtime swap, use madsim; turmoil is the mature choice for host / network /
filesystem simulation of a distributed protocol. `navian-dst` trades some of that coverage
for incremental adoption you can start in one file.

The honest cost of "substrate, not runtime" is coverage: only code routed through the
seams is visible to the simulator. And no simulator reaches *total* determinism —
madsim, for instance, ships a determinism self-check that re-runs a test under the
same seed and fails on divergence, precisely because escapes exist: `HashMap`
iteration order, real `Instant`/`SystemTime`, ASLR, floating-point reordering, native
threads, and any I/O that bypasses the shims. `navian-dst` inherits the same caveats and
points you at single-threaded `SimScheduler` drive to hold determinism; treat
everything outside the seams as untested, not as verified.

## Why we built it, and why we're opening it

We built `navian-dst` for our own use. At Navian AI we build a real-time
event-processing engine for financial services, where a wrong answer under a rare
failure can become a customer-impacting and potentially reportable incident. It has to
stay correct when a node dies mid-write, when the network splits, when events arrive
out of order — and to produce replayable evidence when an invariant fails.
Deterministic simulation became an important layer in that assurance strategy,
alongside conventional testing, model-based invariants, observability, and production
controls.

None of that substrate is our moat — the moat is the detection engine on top, not the
harness that tests it. It's domain-agnostic infrastructure most teams can't justify
building from scratch, and `navian-dst` is the first piece we're extracting. On the
near-term list, in descending readiness: **streaming drift detectors** (CUSUM and
ADWIN — textbook, deterministic, already written to be domain-agnostic), a
**distribution-free conformal anomaly primitive** (calibrated, per-entity p-values),
and the **chaos / reproduction-shrinking harness** that is `navian-dst`'s sibling. Further
out — today a design spec rather than code — is **PulseBench**, a neutral benchmark for
structured-entity memory. We'll ship what's ready and clearly label what isn't. We're
opening it to give back
to the Rust ecosystem we build on, to make the "agent-native install" case in the open
with working code, and because the fastest way to harden reliability tooling is to put
it in front of people who will break it in ways we didn't imagine. If you find a hole,
that's the point — tell us.

---

*`navian-dst` is `v0.1`, Apache-2.0. `cargo add navian-dst`, run the examples, and try
`/dst-init` on a crate you own. It will find counterexamples or tell you a seed
campaign passed — it won't promise more than that, and neither will we.*
