# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] — initial release

First public release of the `dst-rs` deterministic-simulation-testing substrate,
the `dst` CLI, and the AI-native install layer.

### `dst-rs` — the DST substrate (library)

- **Deterministic non-determinism traits.** `Time`, `Random`, `Network`, and
  `Executor` each ship a zero-overhead production impl (direct `std`/`tokio`/
  `rand` calls) and a seed-driven simulation impl (`SimulatedTime`,
  `SimulatedRandom`, `SimulatedNetwork`, `SimulatedExecutor`).
- **`SimScheduler`** — a single-threaded, virtual-clock async scheduler that runs
  many tasks in a reproducible order so a whole concurrent workload replays
  byte-identically.
- **`FaultSchedule`** — a seeded, reproducible timeline of injected faults
  (`TransientError` / `CrashRestart` / `ClockSkew`), and a `SimulatedNetwork` that
  turns it into delivered / delayed / failed / dropped outcomes.
- **`InvariantEngine`** — a generic, step-aware invariant checker returning the
  first `Violation`.
- **`ddmin`** — Zeller & Hildebrandt delta-debugging that shrinks a failing trace
  to a 1-minimal reproducer.
- Two runnable examples (`hello_retry`, `kv_chaos`) and a credibility test
  (`catches_planted_bug`) that finds a planted durability bug and shrinks it to a
  single `CrashRestart`.
- 74 tests.

### `dst-cli` — command-line tools (`dst` binary)

- **`dst scan`** — a static determinism-leak detector (syn-based) that finds calls
  into wall-clock time, RNG, network, and unstructured-concurrency APIs. Human and
  `--json` output; `--deny` turns it into a CI gate.
- **`dst migrate`** — a conservative, seam-safe rewriter that injects
  `time: Arc<dyn dst_rs::Time>` into named-field structs (defaulted to the
  production clock) and routes TIME leak call sites through it. Runs `cargo check`
  after applying and auto-restores the originals on failure. `--dry-run` prints a
  unified diff.
- 18 tests.

### AI-native install layer (`plugin/`, `.cursor/rules/`)

- A Claude Code plugin (`/dst-init` command, `dst` skill, `dst-migrator` subagent)
  and an equivalent Cursor project rule. Points a coding agent at a Rust crate and
  it scans, migrates the time seams, hand-finishes the leaks the tool skips, and
  scaffolds a seed-loop DST test — keeping the tree compiling at every step.

[0.1.0]: https://github.com/navian-ai/dst-rs/releases/tag/v0.1.0
