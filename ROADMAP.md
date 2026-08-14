# Roadmap

`navian-dst` is the first of a small **family of reliability tools** we built while
building Navian Pulse and are open-sourcing over time. Each tool targets a
different pillar of *"is this system actually correct and reliable?"* — and each
ships as its own focused crate under the `navian-` name.

This is a living document. What real users ask for in
[Issues](https://github.com/TheFuturePlutus/navian-dst/issues) and
[Discussions](https://github.com/TheFuturePlutus/navian-dst/discussions) outranks
anything written here.

## The pillars

| Crate | Pillar | What it proves | Status |
|-------|--------|----------------|--------|
| `navian-dst` | **Concurrency** | race / ordering / fault bugs reproduce from a seed | **released (v0.1)** |
| `navian-metamorphic`\* | **Correctness** | your math and statistics are actually right | planned |
| `navian-model`\* | **Integrity** | system state matches a reference model, even under faults | planned |
| `navian-memcheck`\* | **Memory** | state stays bounded; memory is flat after warmup | planned |
| `navian-bench`\* | **Scale** | latency / throughput don't silently regress | exploring |

\* names not final.

## Near-term sequence

Roughly one focused release per week. Dates are intent, not promises.

### navian-dst — finish the flagship
- **Property/invariant harness** — declarative `always` / `sometimes` invariants (extends the current `InvariantEngine`).
- **`run_seeds` helper** — sweep N seeds, auto-shrink the first failure, and print the `SEED=<n>` replay hint, in one call.

### Correctness — `navian-metamorphic`
- **Metamorphic relation API** — declare relations (shift-invariance, scale-equivariance, monotonic-under-transform, definitional checks) and verify them across seeded inputs.
- **Reference-value helpers** — compare against known-correct answers to N significant digits.
- **proptest integration.**

### Integrity — `navian-model`
- **Model-based conformance** — run the real system and a simple reference model under the same seed + fault schedule (shared with `navian-dst`); any state divergence is a bug.
- **Crash-recovery conformance** — send N events, crash, restart, assert all N recovered.

### Memory — `navian-memcheck`
- **Memory-stability harness** — a long fault-injected run, sample RSS, assert the post-warmup slope stays under a threshold (catches leaks and unbounded growth).
- **Bounded-collection audit** — as a CLI / lint.

### Scale — `navian-bench` (exploring)
- A deterministic latency/throughput **regression gate** (P99 budgets), layered on the excellent existing tools (criterion, iai-callgrind) rather than replacing them.

## Principles
- **One focused crate per pillar**, independently versioned — take only what you need.
- **Extraction over invention** — these are generalized from tooling we already run against Pulse.
- **Developer-first** — everything runs in a normal `cargo test` on your own machine, with nothing to swap and no platform to buy.

Feedback and requests are welcome — open an [issue](https://github.com/TheFuturePlutus/navian-dst/issues) or start a [discussion](https://github.com/TheFuturePlutus/navian-dst/discussions).
