# navian-dst-cli

**Command-line tools for the [`navian-dst`](https://crates.io/crates/navian-dst)
deterministic-simulation-testing substrate.** Installs a single `navian-dst` binary.

```bash
cargo install navian-dst-cli   # provides the `navian-dst` binary
```

The `navian-dst` CLI is the build-time companion to the `navian-dst` library: it finds the
sources of non-determinism in a Rust crate and mechanically rewrites the safe ones
so the crate becomes replayable under a simulated clock.

## `navian-dst scan` — find determinism leaks

A static (syn-based) detector for calls into wall-clock time, RNG, network,
unstructured concurrency, environment/process reads, filesystem enumeration,
hash-container iteration order, and relaxed atomics — the things that break
replay-based testing.

```bash
navian-dst scan                     # human report for the current directory
navian-dst scan --json ./src        # machine-readable JSON (schema below)
navian-dst scan --deny              # CI gate: fail on HIGH-confidence findings
navian-dst scan --deny-level medium # lower the gate to also fail on MEDIUM
```

Categories: `time`, `random`, `network`, `concurrency`, `iteration`, `env`,
`filesystem`, `atomic`.

### Confidence tiers

It is a **name-based heuristic**, not a sound analyzer, so every finding carries
a confidence tier and a stable rule id (e.g. `DST-TIME-001`):

- **high** — well-known std/ecosystem sources unlikely to be shadowed
  (`SystemTime::now`, `Instant::now`, `thread_rng`, `OsRng`, `getrandom`,
  `TcpStream::connect`, `thread::spawn`). **The CI gate fails on these.**
- **medium** — recognizable but more shadowable (`chrono::Utc::now`,
  `uuid::Uuid::new_v4`, `std::env::var`, `read_dir`).
- **advisory** — fuzzy / often-intentional (`.gen()`, `HashMap`/`HashSet`
  iteration order, `Ordering::Relaxed`, `tokio::spawn`, rayon `par_iter`).

`--deny` fails on **high** by default; `--deny-level {high|medium|advisory}`
lowers the threshold (`advisory` = fail on anything). Keeping the gate on high
only is what lets `scan` surface fuzzy sources without a wall of gate-failing
false positives.

### Exit codes

- `0` — clean, or findings present without a `--deny` gate.
- `1` — one or more findings at or above the `--deny` threshold under a gate.
- `2` — tool/usage error (bad arguments, unreadable/nonexistent path).

### JSON schema

`--json` prints an array of items, each a stable object:

```json
{
  "rule_id":    "DST-TIME-001",
  "confidence": "high",
  "category":   "time",
  "file":       "src/lib.rs",
  "line":       12,
  "col":        5,
  "function":   "stamp",
  "snippet":    "SystemTime::now()"
}
```

## `navian-dst migrate` — seam-safe time rewrites

A conservative rewriter. v1 handles **TIME** leaks inside inherent methods of
named-field structs: it adds a `time: Arc<dyn navian_dst::Time>` field (defaulted to
the production clock in every constructor, so existing callers are unchanged) and
routes the leak call sites through it:

- `SystemTime::now()…as_millis()` → `self.time.now_ms()`
- `Instant::now()` → `self.time.instant_now()`
- `tokio::time::sleep(d).await` → `self.time.sleep(d).await`

```bash
navian-dst migrate --dry-run    # print a unified diff, write nothing
navian-dst migrate              # apply, then run `cargo check` (auto-restores on failure)
```

After applying, `migrate` runs `cargo check`; if it fails, the original files are
restored. Anything it cannot map cleanly (free functions, random/network/
concurrency leaks) is left untouched and reported under "Skipped (manual / agent
needed)" — that's where the [`dst-init`](https://github.com/TheFuturePlutus/navian-dst) AI
install layer takes over.

## Part of navian-dst

This crate is one piece of the [navian-dst](https://github.com/TheFuturePlutus/navian-dst)
project. Start there for the substrate, the examples, and the `/dst-init`
agent-driven install flow.

## License

Licensed under the [Apache License, Version 2.0](../../LICENSE).
