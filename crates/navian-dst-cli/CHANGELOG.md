# Changelog

All notable changes to `navian-dst-cli` are documented here. The format is based
on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.3.0]

### Added

- **`invariants` subcommand — an assert-something gate.** Seeded replay proves a
  run is *reproducible*, not that it *asserts* anything. This flags files that
  drive the simulation surface (`SimScheduler` / `FaultSchedule` / `Simulated*`)
  but register no invariant and make no assertion: `MISSING` (`--deny` fails),
  `RAW-ONLY` (only raw `assert!`, `--deny-raw`), `OK`. An unused
  `InvariantEngine::new(vec![])` counts as `MISSING`; a delegated-check file can opt
  out with a `navian-dst:invariants-elsewhere` comment. Certifies invariants are
  present, never that they are correct.
- **`review` subcommand — adversarial invariant critique.** For every
  `Invariant::new("name", |state| …)` (including inside `vec![…]`) it flags,
  deterministically and offline, the structurally hollow ones: `TAUTOLOGY`
  (can never fail), `IGNORES-STATE` (never reads the state), `DUPLICATE` (same
  predicate within one `vec!` set). The domain-specific "what are you missing?"
  critique is emitted as an adversarial prompt for your own LLM/agent, not
  hardcoded. Advisory — always exits `0`. `--format json`, `--prompt-only`.

## [0.2.0]

### Added

- **Confidence tiers and stable rule ids on every `scan` finding.** Each finding
  is now tagged `high` / `medium` / `advisory` and carries a stable id (e.g.
  `DST-TIME-001`), alongside an expanded catalog of nearly 30 rules (29 today)
  across eight categories: `time`, `random`, `network`, `concurrency`,
  `iteration`, `env`, `filesystem`, `atomic`.
- **SARIF and GitHub output for `scan`.** `--format sarif` emits SARIF 2.1.0 for
  GitHub code scanning and other SARIF consumers; `--format github` emits
  `::error`/`::warning`/`::notice` workflow commands.
- **`baseline` subcommand + `scan --baseline <file>` suppression.** Record the
  current findings once so a team can turn on the `--deny` gate on a not-yet-clean
  codebase and fail only on NEW findings. Entries use a stable, line-independent
  fingerprint shared with the SARIF output.
- **`init` subcommand.** Scaffolds a GitHub Actions workflow that runs
  `scan --deny` plus a commented `navian-dst.toml` config stub. Never overwrites
  existing files (reports them as skipped).
- **`explain` subcommand + agent-consumable `--format json` worklist.**
  `explain <RULE-ID>` prints the per-rule fix recipe (why it's nondeterministic,
  the injectable seam, a before → after snippet, whether `migrate` auto-fixes it);
  `explain --list` lists every rule. `scan --format json` now emits a per-finding
  worklist that includes `suggested_seam`, `autofixable`, and `fix_hint` for
  handing to a coding agent.
- **`check` subcommand.** Runs a command N times under one fixed seed and reports
  whether output is IDENTICAL (deterministic) or DIVERGES. Supports `--seed`,
  `--runs`, `--timeout`, and repeatable `--ignore <regex>`.
- **`migrate` `with_time()` injection API.** The time codemod now adds a public
  `with_time(self, Arc<dyn navian_dst::Time>) -> Self` builder alongside the
  injected clock field, so tests can swap in a simulated clock while production
  constructors default to the real clock.
- **`migrate --check-doctests`.** Optionally adds a `cargo test --doc` gate on top
  of the `cargo check` gate, rolling back if any doctest fails.

### Changed

- **`scan --deny` now gates on `high` confidence only** by default, so fuzzy
  sources can be surfaced without failing CI on false positives. `--deny-level
  {high|medium|advisory}` lowers the threshold (and implies `--deny`).
- **`migrate` now gates on `cargo check --all-targets`** (previously a narrower
  `cargo check`), so the compile verification covers tests, examples, and benches
  before a rewrite is kept.
- **`migrate` now handles any integer cast of the `SystemTime` millis idiom** —
  bare `… .as_millis()`, `as u64`, `as u128`, etc. — where previously only the
  `as i64` form was rewritten and the others were skipped.
- **Documented exit-code contract:** `0` = clean / no divergence, `1` = gate
  failure or divergence, `2` = usage/tool error.

### Fixed

- **`migrate` injectability:** the injected clock is now reachable — a public
  `with_time()` builder is generated instead of leaving the field private and
  unsettable.
- **`migrate` dead-import pruning:** imports left unused by a rewrite (e.g.
  `use std::time::SystemTime;`) are removed instead of triggering warnings.
- **`scan` RNG double-count:** RNG leaks that matched more than one pattern are no
  longer reported twice.
- **`scan --deny` no longer passes green on unreadable or unparseable input.**
  A path that can't be read or parsed is now a usage error (exit 2) rather than an
  empty, silently-clean result.

## [0.1.0]

### Added

- Initial release: `scan` (static determinism-leak detector) and a first-cut
  time-family `migrate` codemod.
