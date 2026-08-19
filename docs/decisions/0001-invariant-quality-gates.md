# ADR-0001: Invariant-quality gates — enforce presence, provide adversarial review

- **Status:** Proposed
- **Date:** 2026-08-19
- **Deciders:** Seshu Guddanti
- **Principle(s):** Determinism proves *reproducibility*, not *correctness of the oracle*. The tool owns the mechanical layer; the human (or their agent) owns domain truth. The CLI may enforce structure and provide critique, but it must never decide what the right domain rule is.

## Context

navian-dst gives a program a seeded, replayable harness: time, RNG, network,
executor, fault schedules are controlled, and a failing run shrinks to a minimal
reproducer via `InvariantEngine` + `ddmin`. Seeded replay makes a failure
*reproducible*.

A reviewer on the launch post named the real gap precisely:

> Seeded replay solves the debugging problem, but the harder product decision is
> choosing invariants that represent money movement correctly. Those invariants
> should be treated as versioned domain policy and reviewed independently from the
> agent-written implementation. Otherwise the same agent can generate both the code
> and the tests that agree on the same wrong rule.

This is the **oracle problem**. Two concrete failure modes the tool does nothing
about today:

1. **Vacuous runs.** A DST test can drive `SimScheduler` / `FaultSchedule` /
   `SimulatedRandom` through thousands of steps and register **zero** invariants
   (`InvariantEngine::new(vec![])`, or an engine that is never `.check()`ed). It
   passes deterministically while asserting nothing. Nothing in the CLI flags this.
   See `crates/navian-dst/src/invariant.rs` (`is_empty`, `len` exist but are never
   consulted by any gate).

2. **Self-agreeing oracles.** One agent writes both the money-movement
   implementation and the invariants that check it; they agree on the same wrong
   rule. The tool cannot know the rule is wrong — but it currently offers no
   independent pressure on the invariant set at all.

The CLI is today a static, name-based scanner (`scan`/`migrate`/`check`/`explain`/
`init`, see `crates/navian-dst-cli/src/main.rs`). It already owns the *mechanical*
determinism layer well. It does not yet touch invariant **quality**.

## Decision

Add two invariant-quality features to `navian-dst-cli`, each matching the layer it
belongs to. They ship in **separate commits**: (1) presence enforcement (the
`invariants` subcommand); (2) adversarial review (the `review` subcommand). Both are
now realized.

1. **Presence enforcement (hard CI gate).** A static check that fails when a file
   exercises the simulation surface (`SimScheduler`, `FaultSchedule`,
   `SimulatedRandom`, the executor) but registers no invariants and never invokes
   `InvariantEngine::check`/`check_all`. It certifies *that assertions exist*, never
   *that they are correct*. Analysis is per FILE for v1 (a documented limitation:
   a vacuous test can be masked by an assertion elsewhere in the same file — the
   safe false-`Ok` direction; per-test attribution is a fast-follow, deferred
   because the naive per-`fn` version would false-`Missing` on ordinary sim
   helpers). An UNUSED empty `InvariantEngine::new(vec![])` counts as MISSING; an
   empty engine that is later `.check`ed is an accepted false OK — file-global
   analysis cannot distinguish it from a legitimately-checked helper engine in the
   same file, and the cardinal rule forbids risking a false MISSING to catch it.

2. **Adversarial invariant review (advisory only, never modifies code).** The
   `review` subcommand extracts each declared `Invariant` (name + predicate source,
   via `syn`, including inside `vec![...]`) and critiques the set. The critique has
   two layers matching what a tool can vs. can't know: (a) **deterministic,
   domain-agnostic static checks** — TAUTOLOGY (predicate can never be false),
   IGNORES-STATE (predicate never reads its state param), DUPLICATE — always, with
   no network or key; and (b) the **domain-specific** part ("which invariants are
   you MISSING?") is emitted as an **adversarial prompt handed to the author's own
   LLM/agent**, NOT computed by a hardcoded checklist — a domain-agnostic tool must
   not bake in money-movement (or any vertical's) property list, and this also
   avoids a networked, non-deterministic call inside a determinism tool. It **votes,
   it does not gate** (always exits 0), consistent with the LLM-zero-authority
   stance. It changes no source.

The division of labor is explicit: **the CLI writes/enforces the mechanical parts;
the human (or their agent) authors the domain invariants; the CLI then (1) enforces
that they exist and (2) critiques them — but never asserts what is correct.**

**Safe direction.** Presence enforcement is a name-based heuristic, so — exactly
like the leak scanner's `--deny` gate — the gate-failing tier (`Missing`) must
never fire on a file that genuinely asserts. When the invariant signal is ambiguous
the checker leans toward `Ok` (a vacuous file slipping through is the acceptable
error; failing a good test is not). The one residual false-`Missing` a per-file
static check cannot resolve — a scenario file that delegates checking to a shared
harness helper — is handled by an **in-source waiver marker**
(`navian-dst:invariants-elsewhere`): the site is still reported (as waived, so it
stays auditable) but never fails the gate. A repo-wide baseline is a possible
future addition but is **not** required for v1; the waiver is the escape hatch.

## Alternatives considered

1. **Have the tool judge invariant *correctness*.** Rejected — it would require the
   CLI to encode domain truth (what "correct money movement" is), which is exactly
   the versioned domain policy the human owns. A tool that grades correctness
   becomes another self-agreeing oracle.
2. **Make the LLM review a hard gate.** Rejected — non-deterministic and
   authoritative-by-accident; violates LLM-votes-not-gates. It would also block CI
   on a model's opinion. Advisory keeps the human in the decision seat.
3. **Presence check as advisory only.** Rejected — "you ran a simulation and
   asserted nothing" is an objective defect (unambiguous once detected); it deserves
   a hard gate, like the determinism gate. (The name-based *detector* still has
   documented false negatives — aliased paths, the accepted 2-arg `.check` bypass —
   but that is a limit of detection, not of the defect's objectivity.)
4. **Do nothing.** The gap the reviewer named stays open: vacuous tests pass green
   and nothing pressures a self-agreeing oracle. The launch narrative ("replay you
   can trust") is undercut by having no answer here.

## Consequences

**Wanted:**
- A green DST run now means *something was asserted*, not just *it replayed*.
- An independent, adversarial pass on the invariant set — the first counter-pressure
  to a single-author oracle — without the tool pretending to know domain truth.
- A public, code-backed answer to the reviewer: the gap is acknowledged and closed
  with enforcement + critique, not a claim that the tool decides correctness.
- Sets up the fast-follows (policy ids, policy manifest, authorship separation) that
  turn "versioned domain policy, reviewed independently" into enforced structure.

**Unwanted:**
- Presence enforcement is heuristic (name-based, like `scan`): it can false-negative
  on invariants reached only through aliases/type-aliases/qself paths (documented
  limitations — alias resolution is a fast-follow), and it errs toward `Ok` on
  ambiguous signals rather than risk failing a good test. The residual
  delegated-check false-`Missing` is covered by the in-source waiver marker, not a
  baseline. The cost of erring toward `Ok` is that a determined author can defeat
  the gate (e.g. an unrelated 2-arg `.check`), which is the accepted trade for
  never breaking a genuine test.
- The LLM review needs a key (`ANTHROPIC_API_KEY` or compatible) and is
  non-deterministic; it must be clearly labelled advisory and degrade to static-only
  tautology checks when no key is set. No network call without an explicit opt-in.
- Two more subcommands to document and keep coherent with `scan`'s output/exit
  conventions.

**Out of scope (planned as fast-follows, not decided here):**
- **Policy-id requirement** + **policy manifest** (`navian-dst-policy.toml`) linking
  invariants to versioned, independently-reviewed domain policy.
- **Authorship / PR-diff separation** (git blame/diff) between impl and invariants.
- **Mutation "teeth" testing** and **negative-test corpus** (require an invariant to
  actually fire on a known-bad mutation / invalid fixture) — the strongest signal,
  deferred to a v0.3 "teeth" milestone.

## Implementation references

- Feature 1 (shipped): `crates/navian-dst-cli/src/presence.rs` (the analyzer) and
  the `Commands::Invariants` arm in `crates/navian-dst-cli/src/main.rs`.
- Feature 2 (shipped): `crates/navian-dst-cli/src/review.rs` (extraction + static
  weakness detectors + adversarial-prompt builder) and the `Commands::Review` arm.
- Library seam: `crates/navian-dst/src/invariant.rs` (`InvariantEngine::is_empty`/
  `len` — the presence semantics the gate mirrors statically).
- Tests: presence gate over fixtures (a vacuous sim → MISSING; an UNUSED empty
  engine → MISSING; an empty engine that is `.check`ed → OK, the accepted false OK;
  an empty engine beside a real helper-checked engine → OK, never a false MISSING;
  a registered/checked invariant → OK; a delegated, waived file → not gated).
  Feature-2 tests will cover review extraction over the counter-world example in
  `invariant.rs`.

## References

- Related: `ROADMAP.md` — "Property/invariant harness … extends the current
  `InvariantEngine`"; this ADR is the quality-gate half of that line item.
- External: reviewer comment on the navian-dst launch post (oracle problem).
- Follow-up ADRs (planned): ADR-0002 policy manifest + authorship separation;
  ADR-0003 mutation teeth testing.
