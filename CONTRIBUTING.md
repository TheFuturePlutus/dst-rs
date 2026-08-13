# Contributing to navian-dst

Thanks for your interest in improving `navian-dst`. This is a small, deliberately
focused substrate for deterministic simulation testing (DST) in Rust — the bar
for changes is high precisely because everything downstream relies on it being
correct and reproducible.

## Building and testing

The workspace has two crates: the `navian-dst` library and the `navian-dst-cli` tooling.

```bash
# Build everything
cargo build --workspace

# Run the whole test suite, including doctests
cargo test --workspace

# Lint (the CI bar: zero warnings)
cargo clippy --workspace --all-targets -- -D warnings

# Formatting (must be clean)
cargo fmt --all -- --check

# Docs must build with no warnings (broken links, missing docs, etc.)
cargo doc --workspace --no-deps
```

The minimum supported Rust version (MSRV) is **1.87**. CI checks the workspace on
a pinned `1.87.0` toolchain in addition to stable; please don't reach for
features newer than that without raising the MSRV deliberately in a separate,
called-out change.

## The bar for a change

A pull request is ready when all of the following hold:

1. `cargo test --workspace` is green, including doctests.
2. `cargo clippy --workspace --all-targets -- -D warnings` is clean — no new
   warnings in code you touched.
3. `cargo fmt --all -- --check` is clean.
4. `cargo doc --workspace --no-deps` builds with **zero** warnings. Every public
   item carries a real doc comment (`#![warn(missing_docs)]` is enabled on both
   crates), and headline APIs carry runnable doctests.
5. New behavior is covered by a test. Bug fixes come with a regression test that
   fails before the fix.

## The DST determinism note

This crate exists to make runs **reproducible bit-for-bit from a seed**. That
constraint shapes what a correct contribution looks like:

- **No hidden non-determinism.** Anything that reads wall-clock time, draws
  randomness, talks to the network, or spawns tasks must go through the
  substrate traits (`Time`, `Random`, `Network`, `Executor`) so a simulation
  build can control it. The `navian-dst` CLI's `scan` subcommand exists to catch leaks.
- **Predicates and reproduction functions must be side-effect-free.** `ddmin`
  and the invariant engine call them many times over overlapping subsets; an
  observable side effect makes results build-profile-dependent.
- **The simulation implementations are reproducible only under single-threaded
  drive** (`SimScheduler`). Don't add an API that quietly depends on
  multi-threaded interleaving and calls itself deterministic.
- **Determinism is a testable property.** If you touch the seeded machinery, add
  a "same seed ⇒ identical trace" assertion rather than relying on review.

## Submitting

- Keep pull requests focused; one logical change per PR.
- Explain *why* in the description, not just *what*.
- Call out any change to the public API surface explicitly (a new `pub` item, a
  changed signature, or an enum gaining a variant — the public enums are
  `#[non_exhaustive]` so downstream matches keep compiling).

By contributing you agree that your contributions are licensed under the
project's Apache-2.0 license.
