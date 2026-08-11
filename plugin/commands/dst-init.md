---
description: Make a Rust crate deterministically testable — scan for determinism leaks, migrate the time seams with dst-cli, hand-fix what it skips, and scaffold a seed-loop DST test.
argument-hint: "[path-to-crate-or-dir]  (defaults to .)"
allowed-tools: Bash, Read, Edit, Write, Glob, Grep
---

# /dst-init — make this crate deterministically testable

You are turning a Rust crate into one that can be replayed bit-for-bit from a
seed and tested under fault injection with `dst-rs`. You drive the already-built
`dst-cli` tools (`scan` + `migrate`) and hand-finish the leaks they deliberately
skip. **Read `skills/dst/SKILL.md` first** — it holds the conventions, the
injection pattern, and the seed-loop test template you will reuse below.

Target path: `$1` (default `.` — the current crate/workspace).

## The one rule that governs every step

**The tree must compile at every step.** Partial migration always compiles:
`dst migrate` adds a `time` field defaulted to the *production* clock, so every
existing caller is unchanged. Never leave the tree red. Prefer a false negative
(skip a leak, report it) over a wrong rewrite. Verify with `cargo check` after
each edit; if red, fix or revert that edit before moving on.

## Resolve the CLI once

Use the installed binary if present, else run it from this repo:

```bash
dst --version 2>/dev/null && DST="dst" || DST="cargo run -q -p dst-cli --"
```

Everywhere below, `$DST` means `dst` (installed) or `cargo run -q -p dst-cli --`.

---

## Step 1 — Scan: get the leak map

```bash
$DST scan --json "$1"
```

The JSON is exactly an array of leaks: `{file, line, col, category, snippet, fn}`.
Categories are `time`, `random`, `network`, `concurrency`. Also run it without
`--json` for the human summary. **Summarize to the user**: total leaks, the
per-category counts, and which files/functions are hottest. This is the map;
nothing is changed yet.

If there are zero leaks, say so and stop — the crate is already replay-safe for
the categories dst-cli covers.

## Step 2 — Ensure `dst-rs` is a dependency (BEFORE migrate)

`dst migrate` runs `cargo check` after rewriting and **auto-restores the
originals if the check fails** — and the rewrite references `dst_rs::Time` /
`dst_rs::ProductionTime`, so a crate without the dependency will fail that check
and silently roll back. Add it first:

```bash
# Runtime use (the migrated struct fields live in shipped code):
cargo add dst-rs --manifest-path <crate>/Cargo.toml
# If the crate should only pull dst-rs in for tests, use a dev-dependency:
# cargo add dst-rs --dev --manifest-path <crate>/Cargo.toml
```

If `cargo add` can't resolve `dst-rs` (not published / local workspace), add a
path or workspace dependency to the crate's `Cargo.toml` by hand, e.g.
`dst-rs = { path = "…/crates/dst-rs" }`. Confirm with `cargo check` before
migrating. (In *this* repo, the fixture crate at
`crates/dst-cli/tests/fixtures/app` already depends on `dst-rs` — no add needed.)

## Step 3 — Migrate: apply the seam-safe TIME rewrites

Preview first, then apply:

```bash
$DST migrate --dry-run "$1"     # prints a unified diff, writes nothing
$DST migrate "$1"               # applies, then runs cargo check itself
```

What v1 does (and ONLY this): for TIME leaks inside inherent methods of
named-field structs, it adds `time: std::sync::Arc<dyn dst_rs::Time>` to the
struct, defaults it to `dst_rs::ProductionTime` in every constructor, and
rewrites the call sites:
- `SystemTime::now()…as_millis()` → `self.time.now_ms()`
- `Instant::now()` → `self.time.instant_now()`
- `tokio::time::sleep(d).await` → `self.time.sleep(d).await`

The tool prints a **Migrate summary**: `structs migrated`, `leaks rewritten`,
`leaks skipped`, and (on the applied path) `cargo check: PASSED`. **Report those
numbers to the user.** If it prints `cargo check: FAILED — original files
RESTORED`, the tree is back to its pre-migrate state; read the error, fix the
blocker (usually the missing dependency from Step 2), and re-run.

## Step 4 — Hand-finish every SKIPPED leak (agent's job)

`migrate` lists what it could not map cleanly under "Skipped (manual / agent
needed)". Handle each yourself, following the SAME conventions, one edit at a
time, `cargo check` after each. The boundary (full detail in the skill):

- **Free functions** (no `self` to hang a clock off): thread an explicit
  parameter — `fn boot_ts(time: &dyn dst_rs::Time) -> i64 { time.now_ms() }` —
  and update callers. Or move the logic onto a struct that owns a clock.
- **RANDOM leaks** (`rand::random`, `thread_rng`, `gen`/`gen_range`,
  `Uuid::new_v4`, `fastrand`): add a `rng: Arc<dyn dst_rs::Random>` struct field
  defaulted to `dst_rs::ProductionRandom`; route calls through
  `self.rng.next_u64()` / `self.rng.next_uuid()` / `self.rng.shuffle_u64(..)`.
- **NETWORK leaks** (`reqwest`, `TcpStream`, `TcpListener`, tokio net): abstract
  behind a trait the test can fake; in DST the seeded `SimulatedNetwork` +
  `FaultSchedule` supply delivered/delayed/failed/dropped outcomes.
- **CONCURRENCY leaks** (`std::thread::spawn`, `tokio::spawn`): route task
  spawning through an injected `Executor` (`ProductionExecutor` in prod,
  `SimulatedExecutor`/`SimScheduler` under test) so scheduling is deterministic.

Always default the injected field to the production impl so existing callers
compile unchanged. Add a test-only injecting constructor (e.g.
`pub fn with_time(tokens: u32, time: Arc<dyn dst_rs::Time>) -> Self`) so tests
can pass a `SimulatedTime`; keep `new` on the production default.

## Step 5 — Scaffold a seed-loop DST test

Pick one migrated struct and write a seed-loop test modeled on
`crates/dst-rs/examples/kv_chaos.rs` and the template in the skill: inject a
`SimulatedTime` (via the `with_time` seam), loop many seeds, build a
`FaultSchedule::generate(seed, steps, density)`, step through it, and assert a
real invariant with `InvariantEngine`. Honor `SEED=<n>` for single-seed triage,
exactly like the dst-rs examples. Copy the template from the skill verbatim and
adapt the crate name, struct, and invariant.

## Step 6 — Verify green

```bash
cargo check
cargo test
```

If red, fix or revert until green. Do not declare done on a red tree.

## Step 7 — Report

Tell the user:
- **What changed**: structs migrated, leaks rewritten, leaks you hand-fixed
  (and how), leaks intentionally left (and why).
- **What test was added**: file, the struct + invariant it guards, seed count.
- **The replay hint**: `SEED=<n> cargo test <test_name> -- --nocapture` to
  reproduce any failing seed bit-for-bit.

Emphasize the incremental path: this can be done one file at a time, and the
crate compiles and ships after every single step.
