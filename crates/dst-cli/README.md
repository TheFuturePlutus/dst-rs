# dst-cli

**Command-line tools for the [`dst-rs`](https://crates.io/crates/dst-rs)
deterministic-simulation-testing substrate.** Installs a single `dst` binary.

```bash
cargo install dst-cli   # provides the `dst` binary
```

The `dst` CLI is the build-time companion to the `dst-rs` library: it finds the
sources of non-determinism in a Rust crate and mechanically rewrites the safe ones
so the crate becomes replayable under a simulated clock.

## `dst scan` — find determinism leaks

A static (syn-based) detector for calls into wall-clock time, RNG, network, and
unstructured-concurrency APIs — the things that break replay-based testing.

```bash
dst scan                 # human report for the current directory
dst scan --json ./src    # machine-readable: [{file,line,col,category,snippet,fn}]
dst scan --deny          # exit non-zero if any leak is found (CI gate)
```

Categories: `time`, `random`, `network`, `concurrency`.

## `dst migrate` — seam-safe time rewrites

A conservative rewriter. v1 handles **TIME** leaks inside inherent methods of
named-field structs: it adds a `time: Arc<dyn dst_rs::Time>` field (defaulted to
the production clock in every constructor, so existing callers are unchanged) and
routes the leak call sites through it:

- `SystemTime::now()…as_millis()` → `self.time.now_ms()`
- `Instant::now()` → `self.time.instant_now()`
- `tokio::time::sleep(d).await` → `self.time.sleep(d).await`

```bash
dst migrate --dry-run    # print a unified diff, write nothing
dst migrate              # apply, then run `cargo check` (auto-restores on failure)
```

After applying, `migrate` runs `cargo check`; if it fails, the original files are
restored. Anything it cannot map cleanly (free functions, random/network/
concurrency leaks) is left untouched and reported under "Skipped (manual / agent
needed)" — that's where the [`dst-init`](https://github.com/TheFuturePlutus/dst-rs) AI
install layer takes over.

## Part of dst-rs

This crate is one piece of the [dst-rs](https://github.com/TheFuturePlutus/dst-rs)
project. Start there for the substrate, the examples, and the `/dst-init`
agent-driven install flow.

## License

Licensed under the [Apache License, Version 2.0](../../LICENSE).
