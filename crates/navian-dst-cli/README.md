# navian-dst-cli

**A CLI companion to the [`navian-dst`](https://crates.io/crates/navian-dst) library
for adopting deterministic-simulation testing (DST) in Rust.** It finds the sources
of non-determinism in a crate, mechanically rewrites the safe ones so the crate
becomes replayable under a simulated clock, and hands the rest — as a structured
worklist — to your AI coding agent.

```bash
cargo install navian-dst-cli   # installs a single binary named `navian-dst`
```

The design goal is honesty: the scanner tells you exactly how sure it is about
each finding, the codemod refuses to touch anything it can't prove keeps
compiling, and everything it can't handle is reported rather than silently
skipped.

## Subcommands

| Command | What it does |
|---|---|
| `scan` | Static determinism-leak detector (name-based heuristic, tiered). |
| `migrate` | Seam-safe codemod: routes the time idiom through an injectable clock. |
| `baseline` | Record current findings so `scan --baseline` fails only on NEW ones. |
| `explain` | Per-rule fix recipe (human or agent-consumable JSON). |
| `init` | Scaffold a CI workflow + config stub (never overwrites). |
| `check` | Run a command N times under a fixed seed; report deterministic vs divergent. |
| `invariants` | Gate that a DST test actually asserts something — flag simulations that run but register no invariant. |
| `review` | Adversarially critique the invariants that ARE present (tautology / ignores-state / duplicate) + emit an agent prompt. |

---

### `navian-dst scan` — determinism-leak detector

A static (syn-based) detector for calls into wall-clock time, RNG, network,
unstructured concurrency, hash-container iteration order, environment/process
reads, filesystem enumeration, and relaxed atomics — the things that break
replay-based testing. Nearly 30 rules (29 today) across eight categories:
`time`, `random`, `network`, `concurrency`, `iteration`, `env`, `filesystem`,
`atomic`. Each finding carries a stable rule id (e.g. `DST-TIME-001`).

```bash
navian-dst scan                       # human report for the current directory
navian-dst scan --format json ./src   # machine-readable worklist (schema below)
navian-dst scan --deny                # CI gate: fail on HIGH-confidence findings
navian-dst scan --deny-level medium   # lower the gate to also fail on MEDIUM
navian-dst scan --format sarif > out.sarif   # GitHub code-scanning upload
```

**It is a NAME-BASED heuristic, not a sound analyzer.** It matches recognizable
call names and tail segments (`SystemTime::now`, `thread_rng`, `OsRng`,
`tokio::spawn`) and does no name resolution — so it *can* false-positive on a
same-named user symbol (`my_time::SystemTime::now()`) and *can* false-negative on
a renamed import (`use ... as Sys; Sys::now()`). It errs toward flagging, which
is the right default for a replay-safety gate: a clean scan is a prompt to
review, not a proof of determinism.

**Formats** (`--format`): `human` (default), `json` (also reachable via the
back-compat `--json` alias), `sarif` (SARIF 2.1.0 for GitHub code scanning), and
`github` (`::error`/`::warning`/`::notice` workflow commands).

**JSON worklist schema** — one object per finding, built for handing to an agent:

```json
{
  "rule_id":        "DST-TIME-001",
  "confidence":     "high",
  "category":       "time",
  "file":           "src/lib.rs",
  "line":           3,
  "col":            5,
  "function":       "stamp",
  "snippet":        "SystemTime::now()",
  "suggested_seam": "navian_dst::Time — inject an `Arc<dyn navian_dst::Time>` field …",
  "autofixable":    true,
  "fix_hint":       "navian-dst migrate can rewrite this (run: navian-dst migrate); details: navian-dst explain DST-TIME-001"
}
```

### `navian-dst migrate` — seam-safe time codemod

A deliberately conservative rewriter. v1 handles **TIME** leaks inside inherent
methods of named-field structs. It rewrites the `SystemTime`/`Instant` millis
time idiom into an injectable `Arc<dyn navian_dst::Time>` clock and adds a
`with_time()` builder so tests can swap in a simulated clock while production
callers stay unchanged:

- adds a `time: Arc<dyn navian_dst::Time>` field, defaulted to the real
  `ProductionTime` in every constructor;
- adds `pub fn with_time(mut self, time: Arc<dyn navian_dst::Time>) -> Self`;
- rewrites `SystemTime::now()…as_millis()` (through any integer cast) →
  `self.time.now_ms()`, `Instant::now()` → `self.time.instant_now()`, and
  `tokio::time::sleep(d).await` → `self.time.sleep(d).await`;
- prunes imports left dead by the rewrite.

```bash
navian-dst migrate --dry-run          # print a unified diff, write nothing
navian-dst migrate                    # apply, gated on `cargo check --all-targets`
navian-dst migrate --check-doctests   # additionally gate on `cargo test --doc`
```

After applying, `migrate` runs `cargo check --all-targets`; **if anything won't
compile, the whole run is rolled back** and the originals restored. Doctests are
not run by the default gate (cargo can only verify them by *executing* them);
pass `--check-doctests` to add a `cargo test --doc` gate on top. Anything the
codemod can't prove safe (free functions, random/network/concurrency leaks) is
left untouched and reported under **"Skipped (manual / agent needed)"** — that's
the handoff to your coding agent.

### `navian-dst baseline` — adopt the gate on an existing codebase

Turn on `scan --deny` even when the codebase isn't clean yet: record the current
findings once, commit the file, and thereafter the gate fails only on **NEW**
findings. Entries store a stable, line-independent fingerprint (shared with the
SARIF output), so a suppressed finding stays suppressed as surrounding lines move.

```bash
navian-dst baseline                                  # → navian-dst-baseline.json
navian-dst baseline --out ci/dst-baseline.json ./src
navian-dst scan --deny --baseline ci/dst-baseline.json   # fails only on new leaks
```

A finding suppressed by the baseline is dropped from the output *and* from the
`--deny` decision. A missing or invalid baseline is a usage error (exit 2).

### `navian-dst init` — scaffold CI adoption

Writes a GitHub Actions workflow that runs `scan --deny` as a gate
(`.github/workflows/navian-dst.yml`) and a commented config template
(`navian-dst.toml`). **Existing files are never overwritten** — they're reported
as `skip … (already exists — left untouched)`.

```bash
navian-dst init
```

### `navian-dst explain` — per-rule fix recipe

For any rule id: why it's nondeterministic, the injectable seam to route it
through, a before → after snippet, and whether `migrate` auto-fixes it. This is
the "hand it to your AI agent" recipe for a single finding.

```bash
navian-dst explain                       # list every rule (same as --list)
navian-dst explain DST-TIME-001          # full fix recipe
navian-dst explain DST-TIME-001 --format json   # structured, for agents
```

The `--format json` form emits `rule_id`, `confidence`, `category`, `why`,
`suggested_seam`, `before`, `after`, and `autofixable`.

### `navian-dst check` — run-twice determinism check

The dynamic counterpart to `scan`: run a command several times under one fixed
seed and report whether its output is **IDENTICAL** (deterministic) or
**DIVERGES**. A correctly seeded run reproduces byte-for-byte, so an escaped
clock read, real RNG, `HashMap` iteration order, or thread interleaving shows up
here even though the static scan never sees it.

```bash
navian-dst check -- cargo run --quiet --bin replay-hash
navian-dst check --runs 5 --seed 42 --timeout 30 -- ./snapshot
navian-dst check --ignore '^elapsed' -- ./snapshot   # strip lines that legitimately vary
```

**Seeding, honestly.** The `navian-dst` *library* takes its seed through its API
(`SimulatedRandom::from_seed`, `FaultSchedule::new`, the `SimScheduler`), **not**
from the environment. `check` cannot force a seed onto library code — it exports
`NAVIAN_DST_SEED` and `SEED` for the command to consult *if it chooses*, and
otherwise just runs the command and diffs the output. So it catches gross /
escaped nondeterminism; it does not itself make a program deterministic.

Point it at a test/binary that prints a **state snapshot** or a **replay hash**,
not at raw `cargo test` (its "finished in 0.03s" timing always differs and would
cry wolf). Use `--ignore <regex>` (repeatable) to drop lines like timestamps
before comparison. Only **stdout and exit status** are compared — stderr is
captured and shown but not compared. Without `--timeout`, a hanging/deadlocked
command hangs `check`; `--timeout <secs>` kills and reports an overrunning run
instead.

### `navian-dst invariants` — assert-something gate

Seeded replay proves a run is *reproducible*; it says nothing about whether the
run **asserts** anything. A DST test can drive the whole simulation surface
(`SimScheduler`, `FaultSchedule`, `SimulatedRandom`, …) through thousands of steps
and register **zero** invariants, then pass green having checked nothing. This gate
flags those files.

```bash
navian-dst invariants .           # report
navian-dst invariants . --deny    # CI gate: fail if any simulation asserts nothing
```

Per simulation site (file): `MISSING` (asserts nothing — `--deny` fails on it),
`RAW-ONLY` (only raw `assert!` macros — fails under `--deny-raw`), or `OK` (a real
invariant is constructed or `.check`ed). An unused `InvariantEngine::new(vec![])`
counts as `MISSING`. It certifies assertions **exist**, never that they are
*correct*. A delegated-check file can opt out with a `navian-dst:invariants-elsewhere`
comment. Exit `0` clean / `1` gated tier fired / `2` usage or unparsable-under-gate.

### `navian-dst review` — adversarial invariant critique

The companion to `invariants`: for every `Invariant::new("name", |state| …)` —
including inside `vec![…]` — it flags, deterministically and offline, the ones that
are structurally hollow:

```bash
navian-dst review .                 # report + an adversarial prompt for your agent
navian-dst review . --format json   # invariants + weaknesses + prompt, for an agent
navian-dst review . --prompt-only   # just the critique prompt, to pipe into a tool
```

- `TAUTOLOGY` — the predicate can never be false (`|_| true`, a pure `cond || true`, `{ true }`).
- `IGNORES-STATE` — the predicate never reads the state it is handed.
- `DUPLICATE` — same predicate as another invariant in the same `vec!` set.

The domain-specific "which invariants are you **missing**?" critique is not
hardcoded (a domain-agnostic tool can't know your vertical) — it is emitted as an
**adversarial prompt** you hand your own LLM/agent. `review` votes, it never gates:
it always exits `0` (only a usage error exits `2`).

## Confidence tiers & exit codes

Every finding is tiered by how likely the name match is to be real:

| Tier | Meaning | Examples | Gated by `--deny`? |
|---|---|---|---|
| **high** | Well-known std/ecosystem sources unlikely to be shadowed | `SystemTime::now`, `thread_rng`, `OsRng`, `getrandom`, `TcpStream::connect`, `thread::spawn` | **yes (default)** |
| **medium** | Recognizable but more shadowable | `chrono::Utc::now`, `uuid::Uuid::new_v4`, `std::env::var`, `read_dir` | only with `--deny-level medium` |
| **advisory** | Fuzzy / often-intentional | `.gen()`, `HashMap`/`HashSet` iteration order, `Ordering::Relaxed`, `tokio::spawn`, rayon `par_iter` | only with `--deny-level advisory` |

`--deny` fails on **high** by default; `--deny-level {high|medium|advisory}`
lowers the threshold (`advisory` = fail on anything, and implies `--deny`).
Keeping the gate on high only is what lets `scan` surface fuzzy sources without a
wall of gate-failing false positives.

| Exit code | Meaning |
|---|---|
| `0` | `scan`: clean, or findings present without a gate. `check`: all runs identical. |
| `1` | `scan --deny`: a finding at/above the threshold. `check`: a run diverged (or timed out). |
| `2` | Tool/usage error — bad arguments, or unreadable/nonexistent/unparseable input. |

## The "your AI agent finishes the rest" workflow

`migrate` handles the safe time rewrites; the rest is a machine-readable worklist
you hand to a coding agent:

1. `navian-dst scan --format json` → a worklist where each item carries
   `suggested_seam`, `autofixable`, and `fix_hint`.
2. `navian-dst explain <RULE-ID> --format json` → the structured fix recipe
   (why, seam, before → after) for each rule the worklist references.
3. Hand both to your agent: `autofixable` items go through `migrate`; the rest it
   rewrites against the seam the recipe names.

## Scope & limitations (read this)

- **`scan` is a name-based heuristic, not a sound analyzer.** It can
  false-positive on shadowed names and false-negative on renamed imports. Treat a
  clean scan as a prompt to review, not a proof.
- **`migrate` is a conservative micro-codemod, not a general rewriter.** It
  covers only the Time family inside inherent methods of named-field structs, and
  rolls back anything that won't compile. Broader rewrites are intentionally left
  to a human or an agent.
- **This is not a runtime simulator.** `navian-dst` and this CLI give you
  *injectable seams* and a *run-twice check* for a single Rust crate. Whole-system,
  network-level, or multi-language deterministic simulation is the domain of
  platforms like [madsim](https://github.com/madsim-rs/madsim),
  [turmoil](https://github.com/tokio-rs/turmoil), and
  [shuttle](https://github.com/awslabs/shuttle) — complementary, different scope.
- **Doctests are the one gate gap.** `migrate`'s default gate is `cargo check
  --all-targets`, which does not run doctests; `--check-doctests` closes the gap
  by adding a `cargo test --doc` gate.

## Part of navian-dst

- Library crate: [`navian-dst`](https://crates.io/crates/navian-dst) — the DST
  substrate (`Time`, `Random`, schedulers, fault injection).
- API docs: [docs.rs/navian-dst](https://docs.rs/navian-dst)
- Repository: [github.com/TheFuturePlutus/navian-dst](https://github.com/TheFuturePlutus/navian-dst)

## License

Licensed under the [Apache License, Version 2.0](../../LICENSE).
