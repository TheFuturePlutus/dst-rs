# dst-init — the AI-native install layer for navian-dst

Point your coding agent at a Rust crate and it makes the crate **deterministically
testable**: it finds the determinism leaks (wall-clock time, RNG, network,
unstructured concurrency), rewrites the safe time seams so the crate becomes
injectable with a simulated clock, hand-finishes what the tool can't safely
rewrite, and scaffolds a **seed-loop DST test** you can replay bit-for-bit from a
seed.

This layer is pure orchestration. It drives the already-built `navian-dst-cli` tools
(`navian-dst scan` + `navian-dst migrate`) and the `navian-dst` library — it adds **no new Rust
tooling**. Two front-ends are provided: a **Claude Code plugin** and a **Cursor
project rule**.

## What's in here

```
plugin/
├── .claude-plugin/
│   └── plugin.json          # Claude Code plugin manifest (name: dst-init)
├── commands/
│   └── dst-init.md          # /dst-init [path] — the orchestration playbook
├── skills/
│   └── dst/
│       └── SKILL.md         # deep reference: conventions, API, test template
├── agents/
│   └── dst-migrator.md      # optional subagent that runs the same playbook
└── README.md                # this file

.cursor/
└── rules/
    └── dst.mdc              # the same playbook as a Cursor project rule
```

## Prerequisites

- A Rust toolchain (`cargo`).
- The `navian-dst` CLI on `PATH`, **or** run it from this repo. Install with:
  ```bash
  cargo install --path crates/navian-dst-cli    # provides the `navian-dst` binary
  ```
  Without installing, the playbook falls back to `cargo run -q -p navian-dst-cli --`.
- The target crate should depend on `navian-dst` (the playbook adds it if missing —
  it must be present before `navian-dst migrate`, or migrate's post-rewrite
  `cargo check` fails and auto-restores).

## Install the Claude Code plugin

**From a local checkout** (this repo): add the plugin directory as a
marketplace, then install:

```bash
# In Claude Code:
/plugin marketplace add /absolute/path/to/navian-dst/plugin
/plugin install dst-init
```

Or point the marketplace at the Git repo hosting this folder
(`/plugin marketplace add <owner>/<repo>`) and `/plugin install dst-init`.
After install, restart Claude Code if the command doesn't appear immediately.

Verify it loaded: type `/` and confirm `/dst-init` is listed. The bundled
`dst` skill and the optional `dst-migrator` subagent load with it.

## Use `/dst-init`

```
/dst-init                       # operate on the current crate/workspace
/dst-init crates/my-crate       # operate on a specific path
```

The command runs the playbook end to end:

1. **Scan** — `navian-dst scan --json <path>`; summarizes leak counts by category.
2. **Ensure dependency** — adds `navian-dst` to `Cargo.toml` if missing.
3. **Migrate** — `navian-dst migrate --dry-run` then `navian-dst migrate`; applies the
   seam-safe time rewrites (adds `time: Arc<dyn navian_dst::Time>`, defaulted to the
   production clock, and routes the leak call sites through it).
4. **Hand-finish** — the agent handles the leaks migrate skips (free functions,
   random, network, concurrency) using the same struct-field injection
   convention, keeping the tree compiling at every step.
5. **Scaffold a test** — a seed-loop DST test for one migrated struct: inject
   `SimulatedTime`, loop seeds under a `FaultSchedule`, assert an invariant with
   `InvariantEngine`.
6. **Verify + report** — `cargo check` + `cargo test`, then reports what changed
   and the `SEED=<n> cargo test …` replay hint.

Everything is incremental: the crate compiles and ships after every step, so you
can migrate one file, review, and continue.

## Use the Cursor rule

Copy `.cursor/rules/dst.mdc` into your target project's `.cursor/rules/`
directory (or keep this repo open in Cursor). It's an agent-requested rule
scoped to `**/*.rs` and `**/Cargo.toml`. Then just ask the agent:

> Make this crate deterministically testable.

Cursor picks up the rule and follows the same playbook driving `navian-dst scan` /
`navian-dst migrate`, with the same conventions and seed-loop test template.

## Why there is no MCP server

MCP is for exposing a **live service** an agent calls at runtime. `navian-dst` is a
**library** and `navian-dst-cli` is a **build-time CLI** — the agent invokes them
through the shell exactly like `cargo`, and the whole value is editing source in
place. An MCP server would add a network hop and a daemon for zero benefit. The
right integration surface is a slash command / skill / project rule that runs the
CLI and edits code — which is what this layer is. **MCP is intentionally not
used.**

## Reference

- `crates/navian-dst/README.md` — the substrate (three pillars: replay, fault
  injection, shrinking).
- `crates/navian-dst/examples/hello_retry.rs`, `examples/kv_chaos.rs` — the real
  seed-loop DST patterns the scaffolded test is modeled on.
- `plugin/skills/dst/SKILL.md` — the full conventions, API surface, and test
  template.
