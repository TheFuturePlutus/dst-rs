//! Determinism-leak *migrator* — Phase 3b.
//!
//! Where [`crate::scanner`] only *reports* leaks, `migrate` *rewrites* a
//! conservative, seam-safe subset of them so the crate keeps compiling and
//! becomes injectable with a simulated clock.
//!
//! ## What v1 handles (and only this)
//!
//! **Time** leaks that occur inside an *inherent* method of a **named-field
//! struct** (`impl Foo { fn m(&self) { … } }`). For each such struct `Foo`
//! that contains at least one *rewritable* time leak:
//!
//! 1. A field `time: std::sync::Arc<dyn navian_dst::Time>` is added to the struct.
//! 2. Every struct-literal site `Foo { … }` / `Self { … }` in the migrated
//!    path defaults the field to `Arc::new(navian_dst::ProductionTime::default())`
//!    — so every existing `Foo::new()` caller is unchanged (seam-safety).
//! 3. The leak call sites are rewritten:
//!    - `Instant::now()`               → `self.time.instant_now()`
//!    - `tokio::time::sleep(d).await`  → `self.time.sleep(d).await`
//!    - `SystemTime::now()…as_millis()`→ `self.time.now_ms()`
//!
//! Everything else — leaks in free functions, closures, trait-impl methods
//! (fixed signatures), tuple/unit structs, `std::thread::sleep`, `chrono`,
//! bare `SystemTime::now()` outside the millis idiom, and every non-Time
//! category — is **left untouched and reported** as a manual/agent task.
//!
//! ## Why this is safe by construction
//!
//! Constructors default the new field to the *real* production clock, so the
//! rewrite is invisible to callers. If any rewrite nonetheless fails to
//! type-check, the caller (`main`) restores the original bytes from an
//! in-memory snapshot and reports the failure — the tree is never left broken.
//!
//! ## Known gate gap: doctests
//!
//! The apply-time safety gate is `cargo check --all-targets`, which covers the
//! lib, bins, integration tests, examples, and benches. It does NOT cover
//! **doctests**: cargo can only verify a doctest by *running* it (`cargo test
//! --doc`; `--no-run` is rejected), so `--all-targets` structurally excludes
//! them. A migrated struct constructed via a struct literal *inside* a
//! doc-comment example would therefore break `cargo test --doc` while the gate
//! stays green — and migrate cannot rewrite that literal, since it lives in a
//! doc-string, not the parsed AST. Mitigations: `--check-doctests` adds
//! `cargo test --doc` as a second gate (opt-in — it runs user code and is slow),
//! and when it is off migrate prints a warning after migrating any struct, so the
//! gap is never silent.
//!
//! ## Offsets
//!
//! All edits are computed as byte ranges via [`proc_macro2::Span::byte_range`]
//! (valid because we parse from an in-memory `&str` with the `span-locations`
//! feature) and applied to the original text sorted last-edit-first so earlier
//! offsets stay valid.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use syn::spanned::Spanned;
use syn::visit::{self, Visit};
use walkdir::WalkDir;

// ── Public option / result surface ──────────────────────────────────────────

/// Which non-determinism trait families to migrate. v1: Time only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TraitFamily {
    Time,
}

/// Options controlling a migrate run.
#[derive(Debug, Clone)]
pub struct MigrateOptions {
    pub dry_run: bool,
    pub traits: Vec<TraitFamily>,
    /// After the `cargo check --all-targets` gate passes, ALSO run
    /// `cargo test --doc` and roll the whole run back if any doctest fails.
    ///
    /// OFF by default: doctests can only be verified by *running* them (cargo
    /// rejects `--no-run` for `--doc`), which executes user code and is slow — so
    /// gating on them is opt-in. When off, a warning is emitted instead (see the
    /// module docs' "Known gate gap").
    pub check_doctests: bool,
}

/// A single leak that was deliberately NOT rewritten, with a human reason.
#[derive(Debug, Clone)]
pub struct Skip {
    pub file: String,
    pub line: usize,
    pub col: usize,
    pub snippet: String,
    pub reason: String,
}

/// Per-file plan: original bytes, rewritten bytes, and a unified diff.
#[derive(Debug, Clone)]
pub struct FileChange {
    pub path: PathBuf,
    pub original: String,
    pub modified: String,
    pub diff: String,
}

/// Outcome of `cargo check` after a (non-dry-run) apply.
#[derive(Debug, Clone)]
pub enum CheckOutcome {
    /// Not run (dry-run, or no files changed).
    Skipped,
    Passed,
    /// `cargo check --all-targets` failed (some target — lib/bin/test/example/
    /// bench — no longer compiles); files were restored. Carries captured stderr.
    Failed(String),
}

/// Aggregate result of a migrate run.
#[derive(Debug, Clone)]
pub struct MigrateResult {
    /// Files that changed (empty when nothing to do).
    pub changes: Vec<FileChange>,
    /// Struct names that were migrated (field added / leaks rewritten).
    pub structs_migrated: BTreeSet<String>,
    /// Number of individual leak call-sites rewritten.
    pub leaks_rewritten: usize,
    /// Leaks intentionally skipped, with reasons.
    pub skips: Vec<Skip>,
    /// Files found but unparseable (skipped, never fatal).
    pub parse_failures: Vec<String>,
    pub check: CheckOutcome,
    /// True if the migrator determined the tree was already migrated / had
    /// nothing rewritable (idempotent no-op).
    pub no_op: bool,
    /// Whether the applied run additionally gated on `cargo test --doc`
    /// (`--check-doctests`). When false and a struct was migrated, the caller
    /// emits the doctest-gap warning.
    pub doctests_checked: bool,
}

// ── Entry point ─────────────────────────────────────────────────────────────

/// Plan and (unless `dry_run`) apply a migration over `root`.
///
/// `root` may be a single `.rs` file or a directory; only structs *defined*
/// under `root` are considered (scoped migration).
pub fn migrate_path(root: &Path, opts: &MigrateOptions) -> MigrateResult {
    // v1 only knows Time. Anything else is a caller error handled in main; here
    // we simply act on Time.
    debug_assert!(opts.traits.contains(&TraitFamily::Time));

    let files = collect_rs_files(root);
    let mut sources: Vec<(PathBuf, String)> = Vec::new();
    let mut read_failures = Vec::new();
    for path in files {
        match std::fs::read_to_string(&path) {
            Ok(s) => sources.push((path, s)),
            Err(_) => read_failures.push(path.display().to_string()),
        }
    }

    let mut result = plan_sources(&sources);
    result.parse_failures.extend(read_failures);

    // ── Apply / check / restore (only when not a dry run). ──
    result.check = if opts.dry_run || result.changes.is_empty() {
        CheckOutcome::Skipped
    } else {
        apply_and_check(root, &result.changes, opts.check_doctests)
    };
    // Only meaningful once something was actually applied; harmless otherwise.
    result.doctests_checked = opts.check_doctests && matches!(result.check, CheckOutcome::Passed);

    result
}

/// Pure planning core: given `(path, source)` pairs, compute the full set of
/// changes, migrated structs, and skips WITHOUT touching the filesystem.
/// Exposed for unit testing; `migrate_path` wraps it with IO + `cargo check`.
pub fn plan_sources(sources: &[(PathBuf, String)]) -> MigrateResult {
    let mut parse_failures = Vec::new();

    struct Parsed {
        path: PathBuf,
        display: String,
        source: String,
        ast: syn::File,
    }
    let mut parsed: Vec<Parsed> = Vec::new();
    for (path, source) in sources {
        let display = path.display().to_string();
        match syn::parse_file(source) {
            Ok(ast) => parsed.push(Parsed {
                path: path.clone(),
                display,
                source: source.clone(),
                ast,
            }),
            Err(_) => parse_failures.push(display),
        }
    }

    // ── Pass 1: collect every struct definition across the scoped files. ──
    let mut struct_defs: BTreeMap<String, StructDef> = BTreeMap::new();
    for p in &parsed {
        let mut c = StructCollector {
            file: &p.display,
            defs: &mut struct_defs,
        };
        c.visit_file(&p.ast);
    }

    // ── Pass 2: per-file transform walk — collect rewrites, literals, skips. ──
    struct FilePlan {
        idx: usize,
        rewrites: Vec<Rewrite>,
        literals: Vec<StructLiteral>,
        skips: Vec<Skip>,
        inherent_impls: Vec<InherentImpl>,
    }
    let mut plans: Vec<FilePlan> = Vec::new();
    for (idx, p) in parsed.iter().enumerate() {
        let mut t = TransformVisitor {
            file: &p.display,
            source: &p.source,
            struct_defs: &struct_defs,
            fn_stack: Vec::new(),
            impl_stack: Vec::new(),
            closure_depth: 0,
            consumed: Vec::new(),
            cast_guarded: Vec::new(),
            unsafe_cast_guarded: Vec::new(),
            rewrites: Vec::new(),
            literals: Vec::new(),
            skips: Vec::new(),
            inherent_impls: Vec::new(),
        };
        t.visit_file(&p.ast);
        plans.push(FilePlan {
            idx,
            rewrites: t.rewrites,
            literals: t.literals,
            skips: t.skips,
            inherent_impls: t.inherent_impls,
        });
    }

    // Structs that earned migration: they have >=1 rewritable leak in an
    // inherent method (rewrites are only ever recorded for such structs).
    let mut structs_migrated: BTreeSet<String> = BTreeSet::new();
    for plan in &plans {
        for r in &plan.rewrites {
            structs_migrated.insert(r.struct_name.clone());
        }
    }

    // ── Choose where to place each migrated struct's public `with_time` builder. ──
    // A struct that already defines a `with_time` method (anywhere in scope) is
    // skipped (idempotency). Otherwise the builder is placed in the inherent impl
    // block that CONTAINS the (first) rewrite for that struct — so its `Self`
    // matches the impl where `self.time` is used, covering the general `impl<T>`
    // for a generic struct rather than a specialized one.
    let mut has_with_time: BTreeSet<String> = BTreeSet::new();
    for plan in &plans {
        for imp in &plan.inherent_impls {
            if imp.has_with_time {
                has_with_time.insert(imp.struct_name.clone());
            }
        }
    }
    // struct name -> (plan index, byte offset just past the containing impl's `{`).
    let mut with_time_target: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for (pi, plan) in plans.iter().enumerate() {
        for r in &plan.rewrites {
            if has_with_time.contains(&r.struct_name) {
                continue;
            }
            with_time_target
                .entry(r.struct_name.clone())
                .or_insert((pi, r.impl_brace_open_end));
        }
    }

    // ── Pass 3: turn the plan into concrete byte edits per file & apply. ──
    let mut changes: Vec<FileChange> = Vec::new();
    let mut leaks_rewritten = 0usize;
    let mut all_skips: Vec<Skip> = Vec::new();

    for (pi, plan) in plans.iter().enumerate() {
        let p = &parsed[plan.idx];
        let mut edits: Vec<Edit> = Vec::new();

        // (a) Add the `time` field to migrated structs defined in THIS file
        //     (unless the struct already carries it — idempotency).
        for (name, def) in &struct_defs {
            if def.file != p.display {
                continue;
            }
            if !structs_migrated.contains(name) {
                continue;
            }
            if def.has_time {
                continue; // already migrated: don't re-add the field.
            }
            // Only named-field structs reach `structs_migrated` (see the
            // TransformVisitor's named-field gate), so brace_open is Some.
            if let Some(open_end) = def.field_brace_open_end {
                edits.push(Edit {
                    start: open_end,
                    end: open_end,
                    text: format!("\n    {FIELD_DECL},"),
                });
            }
        }

        // (b) Default the field in every struct-literal of a migrated struct
        //     that does not already set it.
        for lit in &plan.literals {
            if !structs_migrated.contains(&lit.struct_name) {
                continue;
            }
            if lit.already_has_time {
                continue;
            }
            if lit.has_rest {
                // `Foo { .., ..base }` — the base already supplies every unset
                // field (including the injected `time`). Inserting our own
                // `time: default` here would OVERRIDE the base's clock with the
                // production default. Skip the insert and report so the author
                // confirms the base carries the intended clock.
                all_skips.push(make_skip(
                    &p.display,
                    &p.source,
                    lit.lit_start,
                    lit.lit_end,
                    "struct literal uses `..base` update syntax — the base supplies `time`; not inserting a default (confirm the base carries the intended clock)",
                ));
                continue;
            }
            edits.push(Edit {
                start: lit.brace_open_end,
                end: lit.brace_open_end,
                text: format!(" {FIELD_DEFAULT},"),
            });
        }

        // (c) Rewrite the leak call sites.
        for r in &plan.rewrites {
            leaks_rewritten += 1;
            edits.push(Edit {
                start: r.start,
                end: r.end,
                text: r.replacement.clone(),
            });
        }

        // (d) Add a PUBLIC `with_time` consuming builder to one inherent impl of
        //     each migrated struct whose builder target lives in THIS file, so
        //     downstream/integration code can inject a fake clock (the field is
        //     private). Placed once per struct; skipped if one already exists.
        for (target_pi, offset) in with_time_target.values() {
            if *target_pi == pi {
                edits.push(Edit {
                    start: *offset,
                    end: *offset,
                    text: WITH_TIME_METHOD.to_string(),
                });
            }
        }

        // Skips are reported regardless of whether the file changed.
        all_skips.extend(plan.skips.iter().cloned());

        if edits.is_empty() {
            continue;
        }
        let mut modified = apply_edits(&p.source, edits);
        // After rewriting time leaks, an imported time name (`SystemTime` /
        // `UNIX_EPOCH` / `Instant`) may no longer appear anywhere in the file.
        // Remove such now-dead imports (conservatively — only when the name is
        // truly unused in the rewritten text).
        if !plan.rewrites.is_empty() {
            modified = prune_unused_time_imports(&modified);
        }
        let diff = unified_diff(&p.display, &p.source, &modified);
        changes.push(FileChange {
            path: p.path.clone(),
            original: p.source.clone(),
            modified,
            diff,
        });
    }

    let no_op = changes.is_empty();

    MigrateResult {
        changes,
        structs_migrated,
        leaks_rewritten,
        skips: all_skips,
        parse_failures,
        check: CheckOutcome::Skipped,
        no_op,
        doctests_checked: false,
    }
}

const FIELD_DECL: &str = "time: std::sync::Arc<dyn navian_dst::Time>";
const FIELD_DEFAULT: &str = "time: std::sync::Arc::new(navian_dst::ProductionTime::default())";

/// The public consuming builder injected into one inherent impl of every
/// migrated struct, so downstream/integration code can swap in a fake clock
/// (the `time` field itself is private). Leading newline lands it just after the
/// impl's opening `{`.
const WITH_TIME_METHOD: &str = "\n    /// Inject a clock for deterministic tests (defaults to the production clock).\n    pub fn with_time(mut self, time: std::sync::Arc<dyn navian_dst::Time>) -> Self {\n        self.time = time;\n        self\n    }\n";

// ── Write / cargo-check / restore ───────────────────────────────────────────

/// Write every change, run `cargo check --all-targets` on the owning crate, and
/// restore the originals from the in-memory snapshot if the check fails (for ANY
/// target — lib, bins, tests, examples, benches).
///
/// ## Known gate gap: doctests
///
/// `cargo check --all-targets` does NOT cover doctests — cargo can only verify a
/// doctest by *running* it (`cargo test --doc`; `--no-run` is rejected). So a
/// migrated struct constructed via a struct literal INSIDE a doc-comment example
/// (e.g. ```` ``` let f = Foo { id: 1 }; ``` ````) would break `cargo test --doc`
/// while this gate stays green. migrate cannot rewrite that literal either — it
/// lives in a doc-string, not the AST. Two mitigations: pass `check_doctests` to
/// additionally gate on `cargo test --doc` (opt-in, because it runs user code and
/// is slow), and — when it is off — the caller prints a warning so the gap is
/// never silent.
fn apply_and_check(root: &Path, changes: &[FileChange], check_doctests: bool) -> CheckOutcome {
    // Write modified bytes.
    for c in changes {
        if let Err(e) = std::fs::write(&c.path, &c.modified) {
            // Best-effort restore of whatever we already wrote, then report.
            restore(changes);
            return CheckOutcome::Failed(format!("failed to write {}: {e}", c.path.display()));
        }
    }

    let manifest =
        find_manifest(root).or_else(|| changes.first().and_then(|c| find_manifest(&c.path)));

    // `--all-targets` is LOAD-BEARING, not cosmetic. A plain `cargo check` builds
    // only the default (lib/bin) targets, so it would MISS a break the migration
    // caused in an integration test, example, or bench — e.g. a public struct
    // built via a struct literal in `tests/*.rs`: adding the private `time` field
    // makes that external literal fail with `error[E0451]: field time is private`,
    // yet a lib-only check passes. Checking every target closes that gap; any
    // target failing to compile trips the exact-byte rollback below, so migrate
    // never leaves a tree where `cargo check --all-targets` fails.
    let output = {
        let mut cmd = std::process::Command::new("cargo");
        cmd.arg("check").arg("--all-targets");
        if let Some(m) = &manifest {
            cmd.arg("--manifest-path").arg(m);
        }
        cmd.output()
    };

    match output {
        Ok(out) if out.status.success() => {
            // Optional second gate: doctests. `--all-targets` structurally
            // excludes them (they can only be verified by running), so a struct
            // built via a literal inside a doc example breaks `cargo test --doc`
            // while the check above stays green. Opt-in because it runs user code.
            if check_doctests {
                run_doctests_gate(&manifest, changes)
            } else {
                CheckOutcome::Passed
            }
        }
        Ok(out) => {
            restore(changes);
            CheckOutcome::Failed(String::from_utf8_lossy(&out.stderr).into_owned())
        }
        Err(e) => {
            restore(changes);
            CheckOutcome::Failed(format!("could not run `cargo check`: {e}"))
        }
    }
}

/// Run `cargo test --doc` (honoring the caller's env) and, on any doctest
/// compile/run failure, restore the originals — same all-or-nothing rollback as
/// the `--all-targets` gate. Called only after that gate has already passed.
fn run_doctests_gate(manifest: &Option<PathBuf>, changes: &[FileChange]) -> CheckOutcome {
    let output = {
        let mut cmd = std::process::Command::new("cargo");
        cmd.arg("test").arg("--doc");
        if let Some(m) = manifest {
            cmd.arg("--manifest-path").arg(m);
        }
        cmd.output()
    };
    match output {
        Ok(out) if out.status.success() => CheckOutcome::Passed,
        Ok(out) => {
            restore(changes);
            // Doctest failures surface on stdout (the harness) as well as stderr.
            let mut msg = String::from_utf8_lossy(&out.stdout).into_owned();
            msg.push_str(&String::from_utf8_lossy(&out.stderr));
            CheckOutcome::Failed(msg)
        }
        Err(e) => {
            restore(changes);
            CheckOutcome::Failed(format!("could not run `cargo test --doc`: {e}"))
        }
    }
}

fn restore(changes: &[FileChange]) {
    for c in changes {
        let _ = std::fs::write(&c.path, &c.original);
    }
}

/// Nearest ancestor directory containing a `Cargo.toml`, starting at `start`
/// (or its parent if `start` is a file).
fn find_manifest(start: &Path) -> Option<PathBuf> {
    let mut dir = if start.is_file() {
        start.parent()?.to_path_buf()
    } else {
        start.to_path_buf()
    };
    loop {
        let candidate = dir.join("Cargo.toml");
        if candidate.is_file() {
            return Some(candidate);
        }
        if !dir.pop() {
            return None;
        }
    }
}

// ── File collection ─────────────────────────────────────────────────────────

fn collect_rs_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if root.is_file() {
        if root.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(root.to_path_buf());
        }
        return out;
    }
    let walker = WalkDir::new(root).into_iter().filter_entry(|e| {
        if e.depth() == 0 {
            return true;
        }
        let name = e.file_name().to_string_lossy();
        if e.file_type().is_dir() {
            name != "target" && !name.starts_with('.')
        } else {
            true
        }
    });
    for entry in walker.filter_map(Result::ok) {
        if entry.file_type().is_file()
            && entry.path().extension().and_then(|e| e.to_str()) == Some("rs")
        {
            out.push(entry.path().to_path_buf());
        }
    }
    out.sort();
    out
}

// ── Struct definitions (pass 1) ─────────────────────────────────────────────

#[derive(Debug, Clone)]
struct StructDef {
    file: String,
    /// Byte offset just past the opening `{` of the named-field brace, or
    /// `None` for tuple/unit structs (which cannot receive a named field).
    field_brace_open_end: Option<usize>,
    /// The struct already carries the INJECTED clock field — a field named
    /// `time` whose type is `Arc<dyn ...Time>` (idempotency). A domain field
    /// merely *named* `time` (e.g. `time: u64`) does NOT set this; see
    /// `time_conflict`.
    has_time: bool,
    /// The struct has a field named `time` of some OTHER type — injecting our
    /// own `time` field would collide, so such a struct cannot be migrated.
    time_conflict: bool,
    /// The struct has a `#[derive(...)]` naming a trait that `Arc<dyn Time>`
    /// cannot satisfy (see [`DERIVE_BLOCKERS`]). Such a struct is skipped:
    /// injecting the field would fail the derive and, via the all-or-nothing
    /// `cargo check`, revert the whole run.
    blocking_derive: bool,
}

struct StructCollector<'a> {
    file: &'a str,
    defs: &'a mut BTreeMap<String, StructDef>,
}

/// Derive traits that a `time: Arc<dyn navian_dst::Time>` field cannot satisfy, so
/// injecting the field would fail the derive and — via the all-or-nothing
/// `cargo check` — revert the whole run. `Arc<dyn Time>` is not
/// `Debug`/`PartialEq`/`Eq`/`Hash`/`PartialOrd`/`Ord`, not `Copy`, and not
/// `Default` (`dyn Time` is unsized and has no `Default`). The common serde
/// derives are included too — they fail identically (`Arc<dyn Time>` is neither
/// `Serialize` nor `Deserialize`). `Clone` is intentionally ABSENT — `Arc<dyn
/// Time>` *is* `Clone`. Over-inclusion here is safe: it only causes a struct to
/// be reported as a manual skip rather than migrated; under-inclusion nukes the
/// entire run, so we err toward skipping.
const DERIVE_BLOCKERS: &[&str] = &[
    "Debug",
    "PartialEq",
    "Eq",
    "Hash",
    "PartialOrd",
    "Ord",
    "Copy",
    "Default",
    "Serialize",
    "Deserialize",
];

/// Whether `attrs` carry a `#[derive(...)]` (plain or via `#[cfg_attr(...)]`)
/// that a `time: Arc<dyn navian_dst::Time>` field would break (see
/// [`DERIVE_BLOCKERS`]).
///
/// Two subtleties this handles that a naive single-ident scan misses:
///   * **Qualified derives** — `#[derive(serde::Serialize)]` derives `Serialize`
///     via its LAST path segment, not a bare ident.
///   * **`cfg_attr` derives** — `#[cfg_attr(feature = "x", derive(PartialEq))]`
///     only fires under a feature, but would still break the build there. Its
///     nested `derive(...)` blockers are inspected too; if a `cfg_attr` carries a
///     `derive` we cannot fully parse, we conservatively treat it as blocking.
fn has_blocking_derive(attrs: &[syn::Attribute]) -> bool {
    let mut derives = Vec::new();
    let opaque = collect_derives(attrs, &mut derives);
    opaque
        || derives
            .iter()
            .any(|d| DERIVE_BLOCKERS.contains(&d.as_str()))
}

/// Push the last-segment ident of every derived trait in `attrs` into `out`
/// (so `serde::Serialize` → `Serialize`). Returns `true` if a `cfg_attr`
/// carried a `derive(...)` we could not fully parse — in which case the caller
/// must treat the struct as blocked (fail-safe).
fn collect_derives(attrs: &[syn::Attribute], out: &mut Vec<String>) -> bool {
    let mut opaque = false;
    for attr in attrs {
        if attr.path().is_ident("derive") {
            let _ = attr.parse_nested_meta(|meta| {
                if let Some(seg) = meta.path.segments.last() {
                    out.push(seg.ident.to_string());
                }
                Ok(())
            });
        } else if attr.path().is_ident("cfg_attr") && collect_cfg_attr_derives(attr, out).is_err() {
            opaque = true;
        }
    }
    opaque
}

/// Parse a `#[cfg_attr(pred, attr, …)]` and push any nested `derive(...)`
/// trait idents into `out`. `Err(())` means the attribute (or a nested derive)
/// could not be parsed — the caller treats that as a blocker (fail-safe).
fn collect_cfg_attr_derives(attr: &syn::Attribute, out: &mut Vec<String>) -> Result<(), ()> {
    use syn::punctuated::Punctuated;
    use syn::{Meta, Token};

    let metas: Punctuated<Meta, Token![,]> = attr
        .parse_args_with(Punctuated::parse_terminated)
        .map_err(|_| ())?;
    // First element is the cfg predicate; the rest are conditional attributes.
    for meta in metas.iter().skip(1) {
        if !meta.path().is_ident("derive") {
            continue;
        }
        match meta {
            Meta::List(list) => {
                let paths: Punctuated<syn::Path, Token![,]> = list
                    .parse_args_with(Punctuated::parse_terminated)
                    .map_err(|_| ())?;
                for p in paths {
                    if let Some(seg) = p.segments.last() {
                        out.push(seg.ident.to_string());
                    }
                }
            }
            // `derive` used without a parenthesized list is malformed/unexpected
            // — be conservative and treat it as unparseable.
            _ => return Err(()),
        }
    }
    Ok(())
}

/// Whether `ty` is the injected clock shape `std::sync::Arc<dyn navian_dst::Time>` —
/// an `Arc` (reached by the **exact** `std::sync::Arc` path) wrapping a trait
/// object whose bound is the **exact** `navian_dst::Time` path.
///
/// Recognition is deliberately syntactic-exact on BOTH the wrapper and the
/// trait: the wrapper path must be `std::sync::Arc` or `::std::sync::Arc`
/// (exactly three segments), and the trait path must be `navian_dst::Time` or
/// `::navian_dst::Time` (exactly two segments). This is precisely what `migrate`
/// emits (see [`FIELD_DECL`]), so its own output is recognized on re-run
/// (idempotency), while a `time`-named field of any OTHER shape — a
/// `my_crate::Arc<dyn navian_dst::Time>`, a bare `Arc<dyn navian_dst::Time>`, a bare
/// `Arc<dyn Time>`, `Arc<dyn my_crate::Time>`, or a nested
/// `Arc<dyn foo::navian_dst::Time>` — is NOT treated as already-migrated. We do not
/// attempt to prove a bare `Arc`/`Time` refers to `std::sync::Arc`/`navian_dst::Time`:
/// syntactic `use` tracking cannot be done soundly without a name resolver, and a
/// false "already migrated" verdict would rewrite leaks against an uncontrolled
/// clock. The caller records such a field as a conflict and skips the struct.
fn is_arc_dyn_time(ty: &syn::Type) -> bool {
    let syn::Type::Path(tp) = ty else {
        return false;
    };
    // The wrapper must be EXACTLY `std::sync::Arc` / `::std::sync::Arc` (a
    // leading `::` leaves the segment idents unchanged, so both forms match).
    let wrapper: Vec<&syn::Ident> = tp.path.segments.iter().map(|s| &s.ident).collect();
    if !(wrapper.len() == 3 && wrapper[0] == "std" && wrapper[1] == "sync" && wrapper[2] == "Arc") {
        return false;
    }
    // The angle-bracketed args live on the last (`Arc`) segment.
    let Some(seg) = tp.path.segments.last() else {
        return false;
    };
    let syn::PathArguments::AngleBracketed(args) = &seg.arguments else {
        return false;
    };
    for a in &args.args {
        if let syn::GenericArgument::Type(syn::Type::TraitObject(to)) = a {
            for b in &to.bounds {
                if let syn::TypeParamBound::Trait(tb) = b {
                    let segs: Vec<&syn::Ident> =
                        tb.path.segments.iter().map(|s| &s.ident).collect();
                    // ONLY the exact `navian_dst::Time` / `::navian_dst::Time` path
                    // (two segments: `navian_dst` then `Time`). A leading `::`
                    // leaves the segments unchanged, so both forms match here.
                    // Anything else (bare `Time`, `my_crate::Time`,
                    // `foo::navian_dst::Time`) is UNPROVEN and not our clock.
                    if segs.len() == 2 && segs[0] == "navian_dst" && segs[1] == "Time" {
                        return true;
                    }
                }
            }
        }
    }
    false
}

impl<'ast> Visit<'ast> for StructCollector<'_> {
    fn visit_item_struct(&mut self, node: &'ast syn::ItemStruct) {
        let name = node.ident.to_string();
        let blocking_derive = has_blocking_derive(&node.attrs);
        let (brace_open_end, has_time, time_conflict) = match &node.fields {
            syn::Fields::Named(named) => {
                let open_end = named.brace_token.span.open().byte_range().end;
                let mut injected = false;
                let mut conflict = false;
                for f in &named.named {
                    if f.ident.as_ref().map(|i| i == "time").unwrap_or(false) {
                        if is_arc_dyn_time(&f.ty) {
                            injected = true;
                        } else {
                            conflict = true;
                        }
                    }
                }
                (Some(open_end), injected, conflict)
            }
            _ => (None, false, false),
        };
        // First definition wins (there should only be one per name in scope).
        self.defs.entry(name).or_insert(StructDef {
            file: self.file.to_string(),
            field_brace_open_end: brace_open_end,
            has_time,
            time_conflict,
            blocking_derive,
        });
        visit::visit_item_struct(self, node);
    }
}

// ── Transform walk (pass 2) ─────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct Rewrite {
    struct_name: String,
    start: usize,
    end: usize,
    replacement: String,
    /// Byte offset just past the opening `{` of the inherent impl block that
    /// CONTAINS this rewrite — where the struct's public `with_time` builder is
    /// placed, so its `Self` matches the impl where `self.time` is used (and, for
    /// a generic struct, covers the general `impl<T>` rather than a specialized
    /// one).
    impl_brace_open_end: usize,
}

#[derive(Debug, Clone)]
struct StructLiteral {
    struct_name: String,
    /// Byte offset just past the literal's opening `{`.
    brace_open_end: usize,
    /// Byte offset of the literal's start (its path) — for skip location/snippet.
    lit_start: usize,
    /// Byte offset of the end of the literal — for skip snippet.
    lit_end: usize,
    already_has_time: bool,
    /// The literal uses struct-update syntax (`..base`). Such a literal already
    /// inherits every unset field (including an injected `time`) from `base`, so
    /// inserting our own `time: default` would OVERRIDE the base's clock. We skip
    /// the insert and report it instead.
    has_rest: bool,
}

/// An inherent `impl Foo { … }` block (never a trait impl). Used to detect an
/// existing `with_time` method (idempotency) on a migrated struct.
#[derive(Debug, Clone)]
struct InherentImpl {
    struct_name: String,
    /// This impl already defines a `with_time` method (idempotency): don't add a
    /// second one to the struct.
    has_with_time: bool,
}

/// What kind of enclosing function a leak sits in.
#[derive(Debug, Clone)]
enum FnCtx {
    /// Inherent method `impl Foo { fn … }`. The bool is whether the fn has a
    /// `self` receiver — an associated fn without `self` cannot use `self.time`.
    Inherent { struct_name: String, has_self: bool },
    /// Trait-impl method `impl Trait for Foo { fn … }`.
    Trait,
    /// Free function or anything else.
    Free,
}

#[derive(Debug, Clone)]
struct ImplCtx {
    self_ty: Option<String>,
    is_trait: bool,
    /// Byte offset just past this impl block's opening `{`.
    brace_open_end: usize,
}

struct TransformVisitor<'a> {
    file: &'a str,
    source: &'a str,
    struct_defs: &'a BTreeMap<String, StructDef>,
    fn_stack: Vec<FnCtx>,
    impl_stack: Vec<ImplCtx>,
    closure_depth: usize,
    /// Byte ranges already consumed by a `SystemTime::now()…as_millis()`
    /// match, so the inner `SystemTime::now()` call is not double-reported.
    consumed: Vec<(usize, usize)>,
    /// Byte ranges of `SystemTime::now()…as_millis()` chains that sit directly
    /// under a SIGNED, `i64`-holding `as` cast (`as i64`/`as i128`), so rewriting
    /// them to the `i64`-typed `now_ms()` is type-safe. Populated in
    /// `visit_expr_cast` before the inner method call is visited.
    cast_guarded: Vec<(usize, usize)>,
    /// Like `cast_guarded`, but for chains under an UNSIGNED/narrowing integer
    /// cast (`as u64`/`as usize`/…). These are NOT rewritten — a negative
    /// (pre-epoch) `now_ms()` would wrap — but they get a precise skip reason.
    unsafe_cast_guarded: Vec<(usize, usize)>,
    rewrites: Vec<Rewrite>,
    literals: Vec<StructLiteral>,
    skips: Vec<Skip>,
    /// Every inherent `impl` block in the file, for placing the `with_time`
    /// builder on migrated structs.
    inherent_impls: Vec<InherentImpl>,
}

/// 1-based line, 1-based col from a byte offset into `source`.
fn line_col(source: &str, byte: usize) -> (usize, usize) {
    let mut line = 1usize;
    let mut col = 1usize;
    for (i, ch) in source.char_indices() {
        if i >= byte {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (line, col)
}

/// The first source line of the span `[start, end)`, trimmed and length-capped.
fn snippet(source: &str, start: usize, end: usize) -> String {
    let raw: String = source[start.min(source.len())..end.min(source.len())]
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    if raw.chars().count() > 80 {
        let short: String = raw.chars().take(79).collect();
        format!("{short}…")
    } else {
        raw
    }
}

/// Build a [`Skip`] for a span in `source`.
fn make_skip(file: &str, source: &str, start: usize, end: usize, reason: &str) -> Skip {
    let (line, col) = line_col(source, start);
    Skip {
        file: file.to_string(),
        line,
        col,
        snippet: snippet(source, start, end),
        reason: reason.to_string(),
    }
}

impl TransformVisitor<'_> {
    fn push_skip(&mut self, start: usize, end: usize, reason: &str) {
        self.skips
            .push(make_skip(self.file, self.source, start, end, reason));
    }

    fn is_consumed(&self, start: usize, end: usize) -> bool {
        self.consumed.iter().any(|(s, e)| start >= *s && end <= *e)
    }

    /// Byte offset just past the opening `{` of the innermost enclosing impl —
    /// the block that contains the leak being rewritten. A rewrite is only ever
    /// recorded inside an inherent `&self` method, so this is always the impl
    /// where `self.time` is used and where the `with_time` builder belongs.
    fn cur_impl_brace_open_end(&self) -> usize {
        self.impl_stack
            .last()
            .map(|c| c.brace_open_end)
            .unwrap_or(0)
    }

    fn is_cast_guarded(&self, start: usize, end: usize) -> bool {
        self.cast_guarded
            .iter()
            .any(|(s, e)| start == *s && end == *e)
    }

    fn is_unsafe_cast_guarded(&self, start: usize, end: usize) -> bool {
        self.unsafe_cast_guarded
            .iter()
            .any(|(s, e)| start == *s && end == *e)
    }

    /// Decide how to handle a *rewritable-shaped* time leak given the current
    /// lexical context. Returns the migrated struct name if it should be
    /// rewritten, else records a skip and returns None.
    fn resolve_rewrite_target(&mut self, start: usize, end: usize) -> Option<String> {
        if self.closure_depth > 0 {
            self.push_skip(start, end, "inside a closure — not rewritten (v1)");
            return None;
        }
        match self.fn_stack.last() {
            Some(FnCtx::Inherent {
                struct_name,
                has_self,
            }) => {
                if !has_self {
                    self.push_skip(
                        start,
                        end,
                        "associated fn without a `self` receiver — cannot use self.time — map manually",
                    );
                    return None;
                }
                let name = struct_name.clone();
                match self.struct_defs.get(&name) {
                    Some(def) if def.field_brace_open_end.is_some() => {
                        if def.blocking_derive {
                            self.push_skip(
                                start,
                                end,
                                "enclosing struct has a #[derive] (e.g. Debug/PartialEq/Eq/Hash/Ord/Copy/Default/Serialize) that a `time: Arc<dyn Time>` field would break — map manually",
                            );
                            return None;
                        }
                        if def.time_conflict {
                            self.push_skip(
                                start,
                                end,
                                "enclosing struct already has a `time` field of a different type (not provably `navian_dst::Time`) — cannot inject a clock — rename the field or map manually",
                            );
                            return None;
                        }
                        Some(name)
                    }
                    Some(_) => {
                        self.push_skip(
                            start,
                            end,
                            "enclosing struct is not a named-field struct — cannot inject `time`",
                        );
                        None
                    }
                    None => {
                        self.push_skip(
                            start,
                            end,
                            "enclosing struct is not defined in the scanned path — out of scope",
                        );
                        None
                    }
                }
            }
            Some(FnCtx::Trait) => {
                self.push_skip(
                    start,
                    end,
                    "trait-impl method (fixed signature) — needs manual/agent handling",
                );
                None
            }
            Some(FnCtx::Free) | None => {
                self.push_skip(
                    start,
                    end,
                    "not inside a struct method (free fn) — needs manual/agent handling",
                );
                None
            }
        }
    }
}

/// The path segments `a::b::c` of a call target, as strings.
fn path_segs(path: &syn::Path) -> Vec<String> {
    path.segments.iter().map(|s| s.ident.to_string()).collect()
}

/// The last-segment ident of a path type (`std::os::raw::c_int` → `c_int`).
fn path_ty_ident(ty: &syn::Type) -> Option<String> {
    if let syn::Type::Path(tp) = ty {
        if let Some(seg) = tp.path.segments.last() {
            return Some(seg.ident.to_string());
        }
    }
    None
}

/// Whether `ty` is a primitive integer type (any signedness/width). Used only
/// to distinguish "there IS an integer cast" from a non-int cast, so an unsafe
/// unsigned cast gets a precise skip reason rather than the generic u128 one.
fn is_int_cast_ty(ty: &syn::Type) -> bool {
    const INT_TYPES: &[&str] = &[
        "i8", "i16", "i32", "i64", "i128", "isize", "u8", "u16", "u32", "u64", "u128", "usize",
    ];
    path_ty_ident(ty)
        .map(|id| INT_TYPES.contains(&id.as_str()))
        .unwrap_or(false)
}

/// Whether `self.time.now_ms() as <ty>` preserves the semantics of the original
/// `…as_millis() as <ty>`. `now_ms()` returns **`i64`**, and a simulated clock
/// can legitimately return a NEGATIVE (pre-epoch) value — where the original
/// `duration_since(UNIX_EPOCH).unwrap()` would have *panicked*. Casting a
/// negative `i64` to an unsigned target (`u64`/`u128`/`usize`) silently wraps to
/// a huge value instead. So the only cast targets we rewrite are SIGNED types
/// wide enough to hold all of `i64`: `i64` and `i128`. Everything else (unsigned
/// or narrower) is skipped and reported.
fn is_safe_now_ms_cast_ty(ty: &syn::Type) -> bool {
    const SAFE_TYPES: &[&str] = &["i64", "i128"];
    path_ty_ident(ty)
        .map(|id| SAFE_TYPES.contains(&id.as_str()))
        .unwrap_or(false)
}

/// If `expr` (unwrapping parens) is an `…as_millis()` method call, return it.
fn as_millis_chain(expr: &syn::Expr) -> Option<&syn::ExprMethodCall> {
    match expr {
        syn::Expr::MethodCall(m) if m.method == "as_millis" => Some(m),
        syn::Expr::Paren(p) => as_millis_chain(&p.expr),
        _ => None,
    }
}

/// Follow a `.as_millis()` receiver chain down to a root `SystemTime::now()`
/// call. Returns that call's span if the chain roots there.
fn system_now_root(expr: &syn::Expr) -> Option<proc_macro2::Span> {
    match expr {
        syn::Expr::Call(call) => {
            if let syn::Expr::Path(p) = &*call.func {
                let segs = path_segs(&p.path);
                let n = segs.len();
                if n >= 2 && segs[n - 2] == "SystemTime" && segs[n - 1] == "now" {
                    return Some(call.span());
                }
            }
            None
        }
        syn::Expr::MethodCall(m) => system_now_root(&m.receiver),
        syn::Expr::Try(t) => system_now_root(&t.expr),
        syn::Expr::Paren(p) => system_now_root(&p.expr),
        syn::Expr::Unary(u) => system_now_root(&u.expr),
        _ => None,
    }
}

impl<'ast> Visit<'ast> for TransformVisitor<'_> {
    fn visit_item_impl(&mut self, node: &'ast syn::ItemImpl) {
        let self_ty = match &*node.self_ty {
            syn::Type::Path(p) => p.path.segments.last().map(|s| s.ident.to_string()),
            _ => None,
        };
        let brace_open_end = node.brace_token.span.open().byte_range().end;
        // Record inherent (non-trait) impl blocks so we can detect an existing
        // `with_time` method on a migrated struct (idempotency).
        if node.trait_.is_none() {
            if let Some(name) = &self_ty {
                let has_with_time = node.items.iter().any(|it| {
                    matches!(it, syn::ImplItem::Fn(f) if f.sig.ident == "with_time")
                });
                self.inherent_impls.push(InherentImpl {
                    struct_name: name.clone(),
                    has_with_time,
                });
            }
        }
        self.impl_stack.push(ImplCtx {
            self_ty,
            is_trait: node.trait_.is_some(),
            brace_open_end,
        });
        visit::visit_item_impl(self, node);
        self.impl_stack.pop();
    }

    fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
        self.fn_stack.push(FnCtx::Free);
        visit::visit_item_fn(self, node);
        self.fn_stack.pop();
    }

    fn visit_impl_item_fn(&mut self, node: &'ast syn::ImplItemFn) {
        let has_self = matches!(node.sig.inputs.first(), Some(syn::FnArg::Receiver(_)));
        let ctx = match self.impl_stack.last() {
            Some(ImplCtx {
                self_ty: Some(name),
                is_trait: false,
                ..
            }) => FnCtx::Inherent {
                struct_name: name.clone(),
                has_self,
            },
            Some(ImplCtx { is_trait: true, .. }) => FnCtx::Trait,
            _ => FnCtx::Free,
        };
        self.fn_stack.push(ctx);
        visit::visit_impl_item_fn(self, node);
        self.fn_stack.pop();
    }

    fn visit_trait_item_fn(&mut self, node: &'ast syn::TraitItemFn) {
        // Method in a `trait` definition (default body). Fixed signature.
        self.fn_stack.push(FnCtx::Trait);
        visit::visit_trait_item_fn(self, node);
        self.fn_stack.pop();
    }

    fn visit_expr_closure(&mut self, node: &'ast syn::ExprClosure) {
        self.closure_depth += 1;
        visit::visit_expr_closure(self, node);
        self.closure_depth -= 1;
    }

    fn visit_expr_struct(&mut self, node: &'ast syn::ExprStruct) {
        // Resolve the literal's struct name (`Self` → enclosing impl's type).
        let last = node.path.segments.last().map(|s| s.ident.to_string());
        let name = match last.as_deref() {
            Some("Self") => self.impl_stack.last().and_then(|c| c.self_ty.clone()),
            Some(other) => Some(other.to_string()),
            None => None,
        };
        if let Some(name) = name {
            if self.struct_defs.contains_key(&name) {
                let already_has_time = node.fields.iter().any(|f| match &f.member {
                    syn::Member::Named(i) => i == "time",
                    syn::Member::Unnamed(_) => false,
                });
                let brace_open_end = node.brace_token.span.open().byte_range().end;
                let whole = node.span().byte_range();
                self.literals.push(StructLiteral {
                    struct_name: name,
                    brace_open_end,
                    lit_start: whole.start,
                    lit_end: whole.end,
                    already_has_time,
                    has_rest: node.rest.is_some(),
                });
            }
        }
        visit::visit_expr_struct(self, node);
    }

    fn visit_expr_cast(&mut self, node: &'ast syn::ExprCast) {
        // A `SystemTime::now()…as_millis() as <int>` cast is only type- AND
        // semantics-safe to rewrite when the target is a SIGNED type that holds
        // all of `i64` (`as i64`/`as i128`) — `now_ms()` is i64 and may be
        // negative pre-epoch. An unsigned/narrowing integer cast is recorded
        // separately so the method-call visit can emit a precise skip. Mark the
        // chain's span BEFORE recursing so the inner method-call visit sees it.
        if let Some(m) = as_millis_chain(&node.expr) {
            if system_now_root(&m.receiver).is_some() {
                let r = m.span().byte_range();
                if is_safe_now_ms_cast_ty(&node.ty) {
                    self.cast_guarded.push((r.start, r.end));
                } else if is_int_cast_ty(&node.ty) {
                    self.unsafe_cast_guarded.push((r.start, r.end));
                }
            }
        }
        visit::visit_expr_cast(self, node);
    }

    fn visit_expr_method_call(&mut self, node: &'ast syn::ExprMethodCall) {
        // SystemTime::now()…as_millis()  →  self.time.now_ms()
        if node.method == "as_millis" {
            if let Some(now_span) = system_now_root(&node.receiver) {
                let whole = node.span().byte_range();
                let nr = now_span.byte_range();
                // Mark the inner now() call consumed so it isn't reported as a
                // bare leak when we recurse into it.
                self.consumed.push((nr.start, nr.end));
                if !self.is_cast_guarded(whole.start, whole.end) {
                    let reason = if self.is_unsafe_cast_guarded(whole.start, whole.end) {
                        // There IS an integer cast, but it is unsigned/narrowing:
                        // `now_ms()` is i64 and may be negative pre-epoch, which
                        // would silently WRAP under `as u64` (vs the original
                        // `.unwrap()` which panics). Refuse to rewrite.
                        "SystemTime millis idiom under an unsigned/narrowing integer cast (e.g. `as u64`) — now_ms() is i64 and a pre-epoch value would wrap, unlike the original `.unwrap()` — map manually"
                    } else {
                        // No integer `as` cast around the chain: `now_ms()` returns
                        // i64 but `as_millis()` is u128, so a blind rewrite could
                        // change the expression's type and fail to compile.
                        "SystemTime millis idiom without a signed integer (`as i64`) cast — rewriting to now_ms() would change u128→i64 — map manually"
                    };
                    self.push_skip(whole.start, whole.end, reason);
                } else if let Some(target) = self.resolve_rewrite_target(whole.start, whole.end) {
                    let impl_brace_open_end = self.cur_impl_brace_open_end();
                    self.rewrites.push(Rewrite {
                        struct_name: target,
                        start: whole.start,
                        end: whole.end,
                        replacement: "self.time.now_ms()".to_string(),
                        impl_brace_open_end,
                    });
                }
            }
        }
        visit::visit_expr_method_call(self, node);
    }

    fn visit_expr_call(&mut self, node: &'ast syn::ExprCall) {
        if let syn::Expr::Path(p) = &*node.func {
            let segs = path_segs(&p.path);
            let n = segs.len();
            let last = segs.last().map(String::as_str).unwrap_or("");
            let prev = if n >= 2 { segs[n - 2].as_str() } else { "" };

            let func_span = node.func.span().byte_range();
            let whole_span = node.span().byte_range();

            // Instant::now()  →  self.time.instant_now()   (replace callee only)
            if prev == "Instant" && last == "now" {
                if let Some(target) = self.resolve_rewrite_target(whole_span.start, whole_span.end)
                {
                    let impl_brace_open_end = self.cur_impl_brace_open_end();
                    self.rewrites.push(Rewrite {
                        struct_name: target,
                        start: func_span.start,
                        end: func_span.end,
                        replacement: "self.time.instant_now".to_string(),
                        impl_brace_open_end,
                    });
                }
            }
            // tokio::time::sleep(d)  →  self.time.sleep(d)   (replace callee only)
            else if prev == "time" && last == "sleep" {
                if let Some(target) = self.resolve_rewrite_target(whole_span.start, whole_span.end)
                {
                    let impl_brace_open_end = self.cur_impl_brace_open_end();
                    self.rewrites.push(Rewrite {
                        struct_name: target,
                        start: func_span.start,
                        end: func_span.end,
                        replacement: "self.time.sleep".to_string(),
                        impl_brace_open_end,
                    });
                }
            }
            // SystemTime::now() NOT already consumed by an as_millis match.
            else if prev == "SystemTime"
                && last == "now"
                && !self.is_consumed(whole_span.start, whole_span.end)
            {
                self.push_skip(
                    whole_span.start,
                    whole_span.end,
                    "bare SystemTime::now() (not a recognizable millis idiom) — map manually",
                );
            }
            // std::thread::sleep — blocking; Time::sleep is async.
            else if prev == "thread" && last == "sleep" {
                self.push_skip(
                    whole_span.start,
                    whole_span.end,
                    "std::thread::sleep is blocking; Time::sleep is async — map manually",
                );
            }
            // chrono Utc::now / Local::now — returns DateTime, not a Time method.
            else if (prev == "Utc" || prev == "Local") && last == "now" {
                self.push_skip(
                    whole_span.start,
                    whole_span.end,
                    "chrono time (returns DateTime) — no direct Time trait mapping — map manually",
                );
            }
            // Other tokio::time::* with no direct Time equivalent.
            else if prev == "time" && matches!(last, "sleep_until" | "interval" | "timeout") {
                self.push_skip(
                    whole_span.start,
                    whole_span.end,
                    "tokio::time::* with no direct Time trait equivalent — map manually",
                );
            }
        }
        visit::visit_expr_call(self, node);
    }
}

// ── Edit application ────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct Edit {
    start: usize,
    end: usize,
    text: String,
}

/// Apply byte-range edits to `source`. Edits are applied last-first so earlier
/// offsets remain valid; overlapping edits are dropped defensively.
fn apply_edits(source: &str, mut edits: Vec<Edit>) -> String {
    // Sort by start descending; on a tie, longer/earlier-inserted first.
    edits.sort_by(|a, b| b.start.cmp(&a.start).then(b.end.cmp(&a.end)));
    let mut out = source.to_string();
    let mut last_start = usize::MAX;
    for e in edits {
        // Defensive: skip an edit that overlaps one we already applied.
        if e.end > last_start {
            continue;
        }
        out.replace_range(e.start..e.end, &e.text);
        last_start = e.start;
    }
    out
}

/// Remove now-dead `use` imports of the time names our rewrites eliminate
/// (`SystemTime` / `UNIX_EPOCH` / `Instant`) from already-rewritten `src`.
///
/// CONSERVATIVE by construction:
///   * A name is removed only when it no longer occurs as a whole word ANYWHERE
///     in the rewritten text outside a `use` statement (return types, other
///     methods, free functions, macros — all counted, so anything still using it
///     keeps the import).
///   * Only the three names above are candidates; every other import (e.g.
///     `Duration`) is left alone.
///   * Renamed / glob / nested use-trees are left untouched — only a terminal
///     plain `Name` or an all-plain-`Name` group is edited.
///
/// Returns `src` unchanged if it does not parse or nothing is removable.
fn prune_unused_time_imports(src: &str) -> String {
    const CANDIDATES: &[&str] = &["SystemTime", "UNIX_EPOCH", "Instant"];

    let Ok(ast) = syn::parse_file(src) else {
        return src.to_string();
    };

    // Collect every `use` item; clones retain their source spans.
    struct UseCollector {
        items: Vec<syn::ItemUse>,
    }
    impl<'ast> Visit<'ast> for UseCollector {
        fn visit_item_use(&mut self, node: &'ast syn::ItemUse) {
            self.items.push(node.clone());
            // No nested use items to descend into.
        }
    }
    let mut uc = UseCollector { items: Vec::new() };
    uc.visit_file(&ast);
    if uc.items.is_empty() {
        return src.to_string();
    }

    let use_ranges: Vec<(usize, usize)> = uc
        .items
        .iter()
        .map(|u| {
            let r = u.span().byte_range();
            (r.start, r.end)
        })
        .collect();

    let bytes = src.as_bytes();
    let is_ident_byte = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    // Whether `name` occurs as a whole word anywhere EXCEPT inside the `use` item
    // at `cur` (its own import). A mention in ANY other `use` counts as still-used
    // (conservative: we never strip a name a sibling import still needs).
    let used_elsewhere = |name: &str, cur: (usize, usize)| -> bool {
        let mut i = 0usize;
        while let Some(off) = src[i..].find(name) {
            let start = i + off;
            let end = start + name.len();
            let before_ok = start == 0 || !is_ident_byte(bytes[start - 1]);
            let after_ok = end >= bytes.len() || !is_ident_byte(bytes[end]);
            let is_own_import = start >= cur.0 && end <= cur.1;
            if before_ok && after_ok && !is_own_import {
                return true;
            }
            i = start + 1;
        }
        false
    };
    let should_remove =
        |name: &str, cur: (usize, usize)| CANDIDATES.contains(&name) && !used_elsewhere(name, cur);

    // Terminal use-tree, following the leading `a::b::` path segments.
    fn terminal(tree: &syn::UseTree) -> &syn::UseTree {
        match tree {
            syn::UseTree::Path(p) => terminal(&p.tree),
            other => other,
        }
    }

    // Expand `[start, end)` (a `use …;` span) to cover its whole line: leading
    // indentation and one trailing newline, so no blank line is left behind.
    let remove_line = |start: usize, end: usize| -> Edit {
        let mut s = start;
        while s > 0 && (bytes[s - 1] == b' ' || bytes[s - 1] == b'\t') {
            s -= 1;
        }
        let mut e = end;
        if e < bytes.len() && bytes[e] == b'\r' {
            e += 1;
        }
        if e < bytes.len() && bytes[e] == b'\n' {
            e += 1;
        }
        Edit {
            start: s,
            end: e,
            text: String::new(),
        }
    };

    let mut edits: Vec<Edit> = Vec::new();
    for (item, &(item_start, item_end)) in uc.items.iter().zip(use_ranges.iter()) {
        // Only PRIVATE (inherited-visibility) imports are prune candidates. A
        // `pub use` / `pub(crate) use` is a re-export — removing it silently drops
        // a public API item, and an unused `pub use` emits NO warning, so the
        // whole-run `cargo check` rollback would not catch the breakage. Leave any
        // non-private `use` (including a group under it) entirely untouched.
        if !matches!(item.vis, syn::Visibility::Inherited) {
            continue;
        }
        let cur = (item_start, item_end);
        match terminal(&item.tree) {
            // `use a::b::Name;`
            syn::UseTree::Name(n) => {
                if should_remove(&n.ident.to_string(), cur) {
                    edits.push(remove_line(item_start, item_end));
                }
            }
            // `use a::b::{X, Y, Z};` — only all-plain-name groups are edited.
            syn::UseTree::Group(g) => {
                let mut names: Vec<String> = Vec::new();
                let mut all_plain = true;
                for m in &g.items {
                    match m {
                        syn::UseTree::Name(n) => names.push(n.ident.to_string()),
                        _ => {
                            all_plain = false;
                            break;
                        }
                    }
                }
                if !all_plain {
                    continue;
                }
                let kept: Vec<&str> = names
                    .iter()
                    .filter(|n| !should_remove(n, cur))
                    .map(String::as_str)
                    .collect();
                if kept.len() == names.len() {
                    continue; // nothing to remove
                }
                if kept.is_empty() {
                    edits.push(remove_line(item_start, item_end));
                } else {
                    let brace = g.brace_token.span.join().byte_range();
                    edits.push(Edit {
                        start: brace.start,
                        end: brace.end,
                        text: format!("{{{}}}", kept.join(", ")),
                    });
                }
            }
            _ => {}
        }
    }

    if edits.is_empty() {
        return src.to_string();
    }
    apply_edits(src, edits)
}

// ── Unified diff (dependency-free) ──────────────────────────────────────────

#[derive(Clone, Copy)]
enum DiffOp {
    /// (index into a, index into b)
    Equal(usize, usize),
    /// index into a (deleted line)
    Del(usize),
    /// index into b (inserted line)
    Ins(usize),
}

impl DiffOp {
    fn is_equal(&self) -> bool {
        matches!(self, DiffOp::Equal(_, _))
    }
}

/// Produce a git-style unified diff between `old` and `new` for `path`.
/// Empty string when the two are identical.
pub fn unified_diff(path: &str, old: &str, new: &str) -> String {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();

    // The LCS matrix is O(|a|·|b|) memory; on a very large file that blows up
    // (e.g. a 100k-line file would allocate a ~10^10-cell matrix). Above a cap,
    // skip the pretty line diff and emit a one-line summary instead — the actual
    // rewrite is still applied and cargo-checked; only the human diff is elided.
    const MAX_LCS_CELLS: usize = 4_000_000; // ~32 MB of usize cells
    if old != new && a.len().saturating_mul(b.len()) > MAX_LCS_CELLS {
        return format!(
            "--- a/{path}\n+++ b/{path}\n@@ diff omitted: file too large for a line-level diff ({} → {} lines) @@\n",
            a.len(),
            b.len()
        );
    }

    let ops = diff_ops(&a, &b);

    if ops.iter().all(DiffOp::is_equal) {
        return String::new();
    }

    const CTX: usize = 3;

    // Split the op stream into hunks: maximal windows of change surrounded by
    // up to CTX context lines, merging changes separated by <= 2*CTX equals.
    let mut hunks: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < ops.len() {
        if ops[i].is_equal() {
            i += 1;
            continue;
        }
        // Start a hunk with CTX lines of leading context.
        let start = i.saturating_sub(CTX);
        // Extend across changes and short equal-runs.
        let mut end = i;
        while end < ops.len() {
            if ops[end].is_equal() {
                // Count the equal run.
                let run_start = end;
                while end < ops.len() && ops[end].is_equal() {
                    end += 1;
                }
                let run_len = end - run_start;
                let more_changes = end < ops.len();
                if !more_changes || run_len > 2 * CTX {
                    // Close the hunk with up to CTX trailing context.
                    end = run_start + run_len.min(CTX);
                    break;
                }
                // else: absorb the short equal run and keep extending.
            } else {
                end += 1;
            }
        }
        hunks.push((start, end.min(ops.len())));
        i = end;
    }

    let mut out = format!("--- a/{path}\n+++ b/{path}\n");
    for (s, e) in hunks {
        emit_hunk(&mut out, &ops[s..e], &a, &b);
    }
    out
}

/// Classic LCS diff over line indices. Adequate for the small files migrate
/// touches; deterministic output.
fn diff_ops(a: &[&str], b: &[&str]) -> Vec<DiffOp> {
    let n = a.len();
    let m = b.len();
    let mut lcs = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if a[i] == b[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let mut ops = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if a[i] == b[j] {
            ops.push(DiffOp::Equal(i, j));
            i += 1;
            j += 1;
        } else if lcs[i + 1][j] >= lcs[i][j + 1] {
            ops.push(DiffOp::Del(i));
            i += 1;
        } else {
            ops.push(DiffOp::Ins(j));
            j += 1;
        }
    }
    while i < n {
        ops.push(DiffOp::Del(i));
        i += 1;
    }
    while j < m {
        ops.push(DiffOp::Ins(j));
        j += 1;
    }
    ops
}

fn emit_hunk(out: &mut String, ops: &[DiffOp], a: &[&str], b: &[&str]) {
    let mut a_start = None;
    let mut b_start = None;
    let mut a_count = 0;
    let mut b_count = 0;
    for op in ops {
        match op {
            DiffOp::Equal(ia, ib) => {
                a_start.get_or_insert(*ia);
                b_start.get_or_insert(*ib);
                a_count += 1;
                b_count += 1;
            }
            DiffOp::Del(ia) => {
                a_start.get_or_insert(*ia);
                a_count += 1;
            }
            DiffOp::Ins(ib) => {
                b_start.get_or_insert(*ib);
                b_count += 1;
            }
        }
    }
    let a0 = a_start.map(|s| s + 1).unwrap_or(0);
    let b0 = b_start.map(|s| s + 1).unwrap_or(0);
    out.push_str(&format!("@@ -{a0},{a_count} +{b0},{b_count} @@\n"));
    for op in ops {
        match op {
            DiffOp::Equal(ia, _) => {
                out.push(' ');
                out.push_str(a[*ia]);
                out.push('\n');
            }
            DiffOp::Del(ia) => {
                out.push('-');
                out.push_str(a[*ia]);
                out.push('\n');
            }
            DiffOp::Ins(ib) => {
                out.push('+');
                out.push_str(b[*ib]);
                out.push('\n');
            }
        }
    }
}

// ── Unit tests (pure, in-memory — no filesystem, no cargo) ───────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(src: &str) -> MigrateResult {
        let sources = vec![(PathBuf::from("lib.rs"), src.to_string())];
        plan_sources(&sources)
    }

    fn modified(src: &str) -> String {
        let r = plan(src);
        r.changes
            .first()
            .map(|c| c.modified.clone())
            .unwrap_or_else(|| src.to_string())
    }

    #[test]
    fn rewrites_instant_now_in_inherent_method() {
        let src = r#"
struct Foo { n: u64 }
impl Foo {
    fn new() -> Self { Foo { n: 0 } }
    fn tick(&self) -> std::time::Instant { Instant::now() }
}
"#;
        let out = modified(src);
        assert!(out.contains("self.time.instant_now()"), "got:\n{out}");
        // Field added to the struct.
        assert!(
            out.contains("time: std::sync::Arc<dyn navian_dst::Time>"),
            "got:\n{out}"
        );
        // Constructor literal defaulted.
        assert!(
            out.contains("time: std::sync::Arc::new(navian_dst::ProductionTime::default())"),
            "got:\n{out}"
        );
    }

    #[test]
    fn rewrites_tokio_sleep_preserving_await_and_args() {
        let src = r#"
struct Foo;
impl Foo {
    async fn wait(&self, d: std::time::Duration) {
        tokio::time::sleep(d).await;
    }
}
"#;
        // Foo is a unit struct -> cannot inject a named field, so this must be
        // SKIPPED, not rewritten.
        let r = plan(src);
        assert!(r.changes.is_empty(), "unit struct must not be migrated");
        assert_eq!(r.skips.len(), 1);
        assert!(r.skips[0].reason.contains("not a named-field struct"));
    }

    #[test]
    fn rewrites_tokio_sleep_in_named_struct() {
        let src = r#"
struct Foo { x: u8 }
impl Foo {
    fn new() -> Self { Self { x: 0 } }
    async fn wait(&self, d: std::time::Duration) {
        tokio::time::sleep(d).await;
    }
}
"#;
        let out = modified(src);
        assert!(out.contains("self.time.sleep(d).await"), "got:\n{out}");
    }

    #[test]
    fn rewrites_systemtime_millis_idiom() {
        let src = r#"
struct Foo { x: u8 }
impl Foo {
    fn new() -> Self { Self { x: 0 } }
    fn now(&self) -> i64 {
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64
    }
}
"#;
        let out = modified(src);
        assert!(out.contains("self.time.now_ms() as i64"), "got:\n{out}");
        assert!(
            !out.contains("SystemTime::now"),
            "old call still present:\n{out}"
        );
        // Exactly one leak rewritten (the whole chain), not two.
        let r = plan(src);
        assert_eq!(r.leaks_rewritten, 1);
        assert!(r.skips.is_empty(), "unexpected skips: {:?}", r.skips);
    }

    #[test]
    fn skips_free_function_leak() {
        let src = r#"
fn stamp() -> std::time::Instant { Instant::now() }
"#;
        let r = plan(src);
        assert!(r.changes.is_empty());
        assert_eq!(r.skips.len(), 1);
        assert!(r.skips[0].reason.contains("free fn"));
    }

    #[test]
    fn skips_trait_impl_method_leak() {
        let src = r#"
struct Foo { x: u8 }
trait Ticker { fn tick(&self) -> std::time::Instant; }
impl Ticker for Foo {
    fn tick(&self) -> std::time::Instant { Instant::now() }
}
"#;
        let r = plan(src);
        assert!(r.changes.is_empty(), "trait-impl leak must not migrate");
        assert_eq!(r.skips.len(), 1);
        assert!(r.skips[0].reason.contains("trait-impl"));
    }

    #[test]
    fn skips_closure_leak() {
        let src = r#"
struct Foo { x: u8 }
impl Foo {
    fn go(&self) {
        let f = || Instant::now();
        let _ = f();
    }
}
"#;
        let r = plan(src);
        assert!(r.changes.is_empty(), "closure leak must not migrate");
        assert_eq!(r.skips.len(), 1);
        assert!(r.skips[0].reason.contains("closure"));
    }

    #[test]
    fn skips_thread_sleep_and_chrono() {
        let src = r#"
struct Foo { x: u8 }
impl Foo {
    fn new() -> Self { Self { x: 0 } }
    fn nap(&self) {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    fn wall(&self) -> i64 { chrono::Utc::now().timestamp() }
}
"#;
        let r = plan(src);
        // Neither maps cleanly -> both skipped, nothing migrated.
        assert!(r.changes.is_empty());
        assert_eq!(r.skips.len(), 2);
        assert!(r.skips.iter().any(|s| s.reason.contains("blocking")));
        assert!(r.skips.iter().any(|s| s.reason.contains("chrono")));
    }

    #[test]
    fn idempotent_when_time_field_present() {
        // A struct that already carries `time` and whose leaks are already
        // rewritten must be a no-op.
        let src = r#"
struct Foo {
    time: std::sync::Arc<dyn navian_dst::Time>,
    x: u8,
}
impl Foo {
    fn new() -> Self {
        Self { time: std::sync::Arc::new(navian_dst::ProductionTime::default()), x: 0 }
    }
    fn now(&self) -> i64 { self.time.now_ms() }
}
"#;
        let r = plan(src);
        assert!(r.no_op, "already-migrated code must be a no-op");
        assert!(r.changes.is_empty());
        assert!(r.skips.is_empty());
    }

    #[test]
    fn second_run_is_noop_end_to_end() {
        let src = r#"
struct Foo { x: u8 }
impl Foo {
    fn new() -> Self { Self { x: 0 } }
    fn tick(&self) -> std::time::Instant { Instant::now() }
}
"#;
        let once = modified(src);
        // Feed the migrated output back in — must not change again.
        let twice = modified(&once);
        assert_eq!(once, twice, "migrate is not idempotent");
        let r2 = plan(&once);
        assert!(r2.no_op, "second run should be a no-op");
    }

    #[test]
    fn skips_associated_fn_without_self() {
        // A constructor that reads the clock has no `self`, so it cannot be
        // rewritten to `self.time…`; it must be skipped, not broken.
        let src = r#"
struct Foo { last: i64 }
impl Foo {
    fn new() -> Self {
        Self { last: SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64 }
    }
}
"#;
        let r = plan(src);
        assert!(r.changes.is_empty(), "must not rewrite a self-less fn");
        assert_eq!(r.skips.len(), 1);
        assert!(
            r.skips[0].reason.contains("without a `self` receiver"),
            "reason: {}",
            r.skips[0].reason
        );
    }

    #[test]
    fn bare_system_now_is_skipped_not_rewritten() {
        let src = r#"
struct Foo { x: u8 }
impl Foo {
    fn new() -> Self { Self { x: 0 } }
    fn stamp(&self) -> std::time::SystemTime { SystemTime::now() }
}
"#;
        let r = plan(src);
        assert!(
            r.changes.is_empty(),
            "bare SystemTime::now must not migrate"
        );
        assert_eq!(r.skips.len(), 1);
        assert!(r.skips[0].reason.contains("bare SystemTime::now"));
    }

    #[test]
    fn struct_update_base_is_skipped_not_overridden() {
        // A `..base` literal already inherits the injected `time` from `base`;
        // inserting `time: default` would OVERRIDE the base's clock. It must be
        // skipped + reported, and only the plain literal gets the default.
        let src = r#"
struct Foo { x: u8 }
impl Foo {
    fn new() -> Self { Self { x: 0 } }
    fn bumped(&self) -> Self { Self { x: self.x + 1, ..self.clone() } }
    fn tick(&self) -> std::time::Instant { Instant::now() }
}
"#;
        let r = plan(src);
        let out = r
            .changes
            .first()
            .map(|c| c.modified.clone())
            .expect("Foo is migrated");
        // Exactly ONE default insert — for `Self { x: 0 }`, NOT for the
        // `..self.clone()` literal.
        assert_eq!(
            out.matches(FIELD_DEFAULT).count(),
            1,
            "the ..base literal must NOT get a time default:\n{out}"
        );
        // The struct-update literal is reported as a skip.
        assert!(
            r.skips.iter().any(|s| s.reason.contains("..base")),
            "expected a `..base` skip, got: {:?}",
            r.skips
        );
    }

    #[test]
    fn derive_debug_struct_is_skipped_not_broken() {
        // Injecting `time: Arc<dyn Time>` into a struct that derives Debug/PartialEq
        // would break the derive and fail cargo check → whole-run revert. Skip it.
        let src = r#"
#[derive(Debug, PartialEq)]
struct Stamped { id: u64, at_ms: i64 }
impl Stamped {
    fn new(id: u64) -> Self { Self { id, at_ms: 0 } }
    fn stamp(&self) -> i64 {
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64
    }
}
"#;
        let r = plan(src);
        assert!(
            r.changes.is_empty(),
            "derive-Debug struct must not be migrated"
        );
        assert!(
            r.structs_migrated.is_empty(),
            "derive-Debug struct must not be marked migrated"
        );
        assert_eq!(r.skips.len(), 1);
        assert!(
            r.skips[0].reason.contains("derive"),
            "reason: {}",
            r.skips[0].reason
        );
    }

    #[test]
    fn derive_copy_or_default_struct_is_skipped_not_broken() {
        // `Arc<dyn Time>` is neither `Copy` nor `Default`, so injecting the field
        // into a struct deriving either would break the derive and revert the
        // whole run. Both must be skipped, not migrated.
        for derive in ["Copy, Clone", "Default"] {
            let src = format!(
                r#"
#[derive({derive})]
struct S {{ n: u64 }}
impl S {{
    fn new() -> Self {{ Self {{ n: 0 }} }}
    fn t(&self) -> std::time::Instant {{ Instant::now() }}
}}
"#
            );
            let r = plan(&src);
            assert!(
                r.changes.is_empty() && r.structs_migrated.is_empty(),
                "#[derive({derive})] struct must be skipped, not migrated"
            );
            assert_eq!(r.skips.len(), 1, "derive `{derive}`");
            assert!(
                r.skips[0].reason.contains("derive"),
                "derive `{derive}` reason: {}",
                r.skips[0].reason
            );
        }
    }

    #[test]
    fn systemtime_millis_without_int_cast_is_skipped() {
        // `as_millis()` is u128; `now_ms()` is i64. Without an integer cast the
        // rewrite would change the expression's type → skip + report.
        let src = r#"
struct Foo { x: u8 }
impl Foo {
    fn new() -> Self { Self { x: 0 } }
    fn now(&self) -> u128 {
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis()
    }
}
"#;
        let r = plan(src);
        assert!(
            r.changes.is_empty(),
            "u128 millis (no int cast) must not be rewritten"
        );
        assert_eq!(r.skips.len(), 1);
        assert!(
            r.skips[0].reason.contains("u128") || r.skips[0].reason.contains("cast"),
            "reason: {}",
            r.skips[0].reason
        );
    }

    #[test]
    fn systemtime_millis_with_u64_cast_is_skipped_not_rewritten() {
        // `now_ms()` is i64 and can be negative pre-epoch; casting that to an
        // unsigned `u64` would silently WRAP (the original `.unwrap()` panics).
        // So an `as u64` cast must be SKIPPED + reported, not rewritten.
        let src = r#"
struct Foo { x: u8 }
impl Foo {
    fn new() -> Self { Self { x: 0 } }
    fn now(&self) -> u64 {
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64
    }
}
"#;
        let r = plan(src);
        assert!(
            r.changes.is_empty(),
            "unsigned `as u64` cast must not be rewritten"
        );
        assert!(r.structs_migrated.is_empty());
        assert_eq!(r.skips.len(), 1);
        assert!(
            r.skips[0].reason.contains("unsigned"),
            "reason: {}",
            r.skips[0].reason
        );
    }

    #[test]
    fn systemtime_millis_with_i64_cast_is_rewritten() {
        // A signed `i64` cast holds all of `now_ms()`'s range → safe to rewrite.
        let src = r#"
struct Foo { x: u8 }
impl Foo {
    fn new() -> Self { Self { x: 0 } }
    fn now(&self) -> i64 {
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64
    }
}
"#;
        let out = modified(src);
        assert!(out.contains("self.time.now_ms() as i64"), "got:\n{out}");
        assert!(!out.contains("SystemTime::now"), "stale call:\n{out}");
    }

    #[test]
    fn foreign_dyn_time_field_is_conflict_not_migrated() {
        // A `time` field typed `Arc<dyn other::Time>` is NOT our injected clock;
        // rewriting leaks to `self.time.now_ms()` would target the WRONG trait
        // and silently preserve an uncontrolled clock. Must be skipped + reported.
        let src = r#"
struct Foo { time: std::sync::Arc<dyn other::Time>, x: u8 }
impl Foo {
    fn tick(&self) -> std::time::Instant { Instant::now() }
}
"#;
        let r = plan(src);
        assert!(
            r.changes.is_empty(),
            "must not rewrite against a foreign Time trait"
        );
        assert!(
            r.structs_migrated.is_empty(),
            "foreign-Time struct must not be marked migrated"
        );
        assert_eq!(r.skips.len(), 1);
        assert!(
            r.skips[0].reason.contains("different type"),
            "reason: {}",
            r.skips[0].reason
        );
    }

    #[test]
    fn foreign_arc_wrapper_dyn_time_is_conflict_not_migrated() {
        // MAJOR regression: `my_crate::Arc<dyn navian_dst::Time>` is NOT our injected
        // clock — the wrapper path is `my_crate::Arc`, not `std::sync::Arc`. The
        // old last-segment check accepted any `*::Arc`. Even though the trait is
        // the real `navian_dst::Time`, an unproven wrapper must read as a conflict so
        // we never rewrite leaks against an uncontrolled `Arc` type.
        let src = r#"
struct Foo { time: my_crate::Arc<dyn navian_dst::Time>, x: u8 }
impl Foo {
    fn tick(&self) -> std::time::Instant { Instant::now() }
}
"#;
        let r = plan(src);
        assert!(
            r.changes.is_empty() && r.structs_migrated.is_empty(),
            "my_crate::Arc wrapper must not read as injected"
        );
        assert_eq!(r.skips.len(), 1);
        assert!(
            r.skips[0].reason.contains("different type"),
            "reason: {}",
            r.skips[0].reason
        );
    }

    #[test]
    fn bare_time_field_is_never_treated_as_injected() {
        // Recognition is exact-path only: a bare `Arc<dyn Time>` is UNPROVEN
        // (syntactic `use` tracking is unsound without a resolver) → treated as a
        // conflict, never silently accepted as our clock. This holds regardless
        // of any `use navian_dst::Time;` import in the file.
        let imported = r#"
use navian_dst::Time;
struct Foo { time: std::sync::Arc<dyn Time>, x: u8 }
impl Foo {
    fn tick(&self) -> std::time::Instant { Instant::now() }
}
"#;
        let r = plan(imported);
        assert!(r.changes.is_empty() && r.structs_migrated.is_empty());
        assert_eq!(r.skips.len(), 1);
        assert!(
            r.skips[0].reason.contains("different type"),
            "reason: {}",
            r.skips[0].reason
        );
    }

    #[test]
    fn mod_local_navian_dst_import_does_not_leak_to_sibling_bare_time() {
        // BLOCKER regression: an inline `mod m { use navian_dst::Time; }` must NOT
        // make an unrelated top-level `time: Arc<dyn Time>` look injected. Rust
        // scope is per-module; the old file-wide import flag conflated them.
        let src = r#"
mod m { use navian_dst::Time; }
struct Foo { time: std::sync::Arc<dyn Time>, x: u8 }
impl Foo {
    fn tick(&self) -> std::time::Instant { Instant::now() }
}
"#;
        let r = plan(src);
        assert!(
            r.changes.is_empty() && r.structs_migrated.is_empty(),
            "sibling bare `Time` must not read as injected"
        );
        assert_eq!(r.skips.len(), 1);
        assert!(
            r.skips[0].reason.contains("different type"),
            "reason: {}",
            r.skips[0].reason
        );
    }

    #[test]
    fn nested_navian_dst_path_time_is_not_treated_as_injected() {
        // BLOCKER regression: `Arc<dyn foo::navian_dst::Time>` is NOT `navian_dst::Time`.
        // The old penultimate-segment check accepted any `*::navian_dst::Time`.
        let src = r#"
struct Foo { time: std::sync::Arc<dyn foo::navian_dst::Time>, x: u8 }
impl Foo {
    fn tick(&self) -> std::time::Instant { Instant::now() }
}
"#;
        let r = plan(src);
        assert!(
            r.changes.is_empty() && r.structs_migrated.is_empty(),
            "nested `foo::navian_dst::Time` must not read as injected"
        );
        assert_eq!(r.skips.len(), 1);
        assert!(
            r.skips[0].reason.contains("different type"),
            "reason: {}",
            r.skips[0].reason
        );
    }

    #[test]
    fn leading_colon_navian_dst_time_is_treated_as_injected() {
        // The absolute path `::navian_dst::Time` IS our trait (leading `::` leaves
        // the two segments unchanged) → already-migrated no-op.
        let src = r#"
struct Foo { time: std::sync::Arc<dyn ::navian_dst::Time>, x: u8 }
impl Foo {
    fn new() -> Self { Self { time: std::sync::Arc::new(navian_dst::ProductionTime::default()), x: 0 } }
    fn now(&self) -> i64 { self.time.now_ms() }
}
"#;
        let r = plan(src);
        assert!(
            r.no_op,
            "`::navian_dst::Time` must read as already-migrated"
        );
        assert!(r.skips.is_empty(), "unexpected skips: {:?}", r.skips);
    }

    #[test]
    fn qualified_derive_serialize_is_skipped() {
        // `#[derive(serde::Serialize)]` derives `Serialize` via the LAST path
        // segment; a single-ident scan would miss it, inject the field, and break
        // the derive → whole-run revert. Must be detected and skipped.
        let src = r#"
#[derive(serde::Serialize)]
struct S { n: u64 }
impl S {
    fn new() -> Self { Self { n: 0 } }
    fn t(&self) -> std::time::Instant { Instant::now() }
}
"#;
        let r = plan(src);
        assert!(
            r.changes.is_empty() && r.structs_migrated.is_empty(),
            "qualified serde::Serialize derive must be skipped"
        );
        assert_eq!(r.skips.len(), 1);
        assert!(
            r.skips[0].reason.contains("derive"),
            "reason: {}",
            r.skips[0].reason
        );
    }

    #[test]
    fn cfg_attr_derive_is_skipped() {
        // `#[cfg_attr(feature = "x", derive(PartialEq))]` derives PartialEq under
        // a feature. Injecting the field would break the build there. The nested
        // derive must be inspected and the struct skipped.
        let src = r#"
#[cfg_attr(feature = "x", derive(PartialEq))]
struct S { n: u64 }
impl S {
    fn new() -> Self { Self { n: 0 } }
    fn t(&self) -> std::time::Instant { Instant::now() }
}
"#;
        let r = plan(src);
        assert!(
            r.changes.is_empty() && r.structs_migrated.is_empty(),
            "cfg_attr(derive(PartialEq)) struct must be skipped"
        );
        assert_eq!(r.skips.len(), 1);
        assert!(
            r.skips[0].reason.contains("derive"),
            "reason: {}",
            r.skips[0].reason
        );
    }

    #[test]
    fn domain_time_field_of_other_type_is_not_treated_as_migrated() {
        // A struct with a domain `time: u64` field must NOT be treated as already
        // migrated, and must NOT have a second `time` field injected. Skip it.
        let src = r#"
struct Foo { time: u64, x: u8 }
impl Foo {
    fn new() -> Self { Self { time: 0, x: 0 } }
    fn tick(&self) -> std::time::Instant { Instant::now() }
}
"#;
        let r = plan(src);
        assert!(
            r.changes.is_empty(),
            "must not migrate a struct whose `time` field is a different type"
        );
        assert_eq!(r.skips.len(), 1);
        assert!(
            r.skips[0].reason.contains("different type"),
            "reason: {}",
            r.skips[0].reason
        );
    }

    #[test]
    fn large_file_diff_is_summarized_not_quadratic() {
        // Above the LCS cap, unified_diff emits a summary instead of building a
        // quadratic matrix (OOM guard).
        let big: String = "x\n".repeat(3_000);
        let big2 = format!("{big}y\n");
        let d = unified_diff("big.rs", &big, &big2);
        assert!(
            d.contains("diff omitted"),
            "expected a summarized diff, got: {}",
            &d[..d.len().min(200)]
        );
    }

    #[test]
    fn unified_diff_roundtrips() {
        let old = "a\nb\nc\n";
        let new = "a\nB\nc\n";
        let d = unified_diff("x.rs", old, new);
        assert!(d.contains("--- a/x.rs"));
        assert!(d.contains("+++ b/x.rs"));
        assert!(d.contains("-b"));
        assert!(d.contains("+B"));
        // Identical inputs -> empty diff.
        assert!(unified_diff("x.rs", old, old).is_empty());
    }

    // ── FIX 1: public `with_time` builder ─────────────────────────────────────

    #[test]
    fn migrated_struct_gets_public_with_time_builder() {
        let src = r#"
struct Foo { n: u64 }
impl Foo {
    fn new() -> Self { Foo { n: 0 } }
    fn tick(&self) -> std::time::Instant { Instant::now() }
}
"#;
        let out = modified(src);
        // The PUBLIC consuming builder is generated, once.
        assert!(
            out.contains(
                "pub fn with_time(mut self, time: std::sync::Arc<dyn navian_dst::Time>) -> Self"
            ),
            "expected a public with_time builder:\n{out}"
        );
        assert_eq!(
            out.matches("fn with_time").count(),
            1,
            "with_time must be added exactly once:\n{out}"
        );
        // It lands inside the inherent impl (before the constructor).
        assert!(
            out.contains("self.time = time;"),
            "with_time body missing:\n{out}"
        );
    }

    #[test]
    fn with_time_is_not_duplicated_when_already_present() {
        // The struct still has an un-rewritten leak (so it IS migrated), but it
        // already defines a `with_time` method — we must NOT add a second one.
        let src = r#"
struct Foo { x: u8 }
impl Foo {
    fn new() -> Self { Self { x: 0 } }
    fn with_time(mut self, time: std::sync::Arc<dyn navian_dst::Time>) -> Self {
        self.time = time;
        self
    }
    fn tick(&self) -> std::time::Instant { Instant::now() }
}
"#;
        let r = plan(src);
        let out = r
            .changes
            .first()
            .map(|c| c.modified.clone())
            .expect("Foo is migrated");
        // Leak rewritten and field added, but with_time NOT duplicated.
        assert!(out.contains("self.time.instant_now()"), "got:\n{out}");
        assert!(
            out.contains("time: std::sync::Arc<dyn navian_dst::Time>,"),
            "field must be added:\n{out}"
        );
        assert_eq!(
            out.matches("fn with_time").count(),
            1,
            "existing with_time must not be duplicated:\n{out}"
        );
    }

    #[test]
    fn with_time_survives_second_run_idempotently() {
        let src = r#"
struct Foo { x: u8 }
impl Foo {
    fn new() -> Self { Self { x: 0 } }
    fn tick(&self) -> std::time::Instant { Instant::now() }
}
"#;
        let once = modified(src);
        assert_eq!(
            once.matches("fn with_time").count(),
            1,
            "first run adds with_time once:\n{once}"
        );
        let twice = modified(&once);
        assert_eq!(once, twice, "second run must be a no-op:\n{twice}");
    }

    #[test]
    fn with_time_goes_in_the_impl_that_contains_the_rewrite() {
        // A generic struct with TWO inherent impls: a specialized `impl Foo<u8>`
        // and the general `impl<T> Foo<T>` that holds the migrated method. The
        // `with_time` builder must land in the GENERAL impl (where `self.time` is
        // used), not the specialized one — otherwise `Self` would be `Foo<u8>`.
        let src = r#"
struct Foo<T> { x: T }
impl Foo<u8> {
    fn special(&self) -> u8 { self.x }
}
impl<T> Foo<T> {
    fn new(x: T) -> Self { Self { x } }
    fn tick(&self) -> std::time::Instant { Instant::now() }
}
"#;
        let out = modified(src);
        assert_eq!(
            out.matches("fn with_time").count(),
            1,
            "with_time added exactly once:\n{out}"
        );
        // Placed at the top of the GENERAL impl block.
        assert!(
            out.contains("impl<T> Foo<T> {\n    /// Inject a clock for deterministic tests"),
            "with_time must be in the general impl<T> Foo<T> block:\n{out}"
        );
        // The specialized impl is left untouched.
        assert!(
            out.contains("impl Foo<u8> {\n    fn special"),
            "specialized impl Foo<u8> must not receive with_time:\n{out}"
        );
    }

    // ── FIX 2: dead time-import pruning ────────────────────────────────────────

    #[test]
    fn removes_dead_instant_import_after_rewrite() {
        // `Instant` is used ONLY at the rewritten leak site → its import becomes
        // dead and must be removed.
        let src = r#"
use std::time::Instant;
struct Foo { x: u8 }
impl Foo {
    fn new() -> Self { Self { x: 0 } }
    fn tick(&self) { let _ = Instant::now(); }
}
"#;
        let out = modified(src);
        assert!(out.contains("self.time.instant_now()"), "got:\n{out}");
        assert!(
            !out.contains("use std::time::Instant"),
            "dead Instant import must be removed:\n{out}"
        );
    }

    #[test]
    fn keeps_pub_use_reexport_even_when_locally_unused() {
        // A `pub use std::time::Instant;` is a PUBLIC re-export. Even when the
        // only local `Instant` reference is the rewritten leak, the re-export must
        // survive: an unused `pub use` emits NO warning, so the whole-run cargo
        // check would NOT catch its removal, silently breaking downstream users of
        // `mycrate::Instant`.
        let src = r#"
pub use std::time::Instant;
struct Foo { x: u8 }
impl Foo {
    fn new() -> Self { Self { x: 0 } }
    fn tick(&self) { let _ = Instant::now(); }
}
"#;
        let out = modified(src);
        assert!(out.contains("self.time.instant_now()"), "got:\n{out}");
        assert!(
            out.contains("pub use std::time::Instant;"),
            "public re-export must be preserved:\n{out}"
        );
    }

    #[test]
    fn keeps_time_import_still_referenced_elsewhere() {
        // `Instant` is still used as a return type after the rewrite → its import
        // must be preserved.
        let src = r#"
use std::time::Instant;
struct Foo { x: u8 }
impl Foo {
    fn new() -> Self { Self { x: 0 } }
    fn mono(&self) -> Instant { Instant::now() }
}
"#;
        let out = modified(src);
        assert!(out.contains("self.time.instant_now()"), "got:\n{out}");
        assert!(
            out.contains("use std::time::Instant;"),
            "still-used Instant import must be kept:\n{out}"
        );
    }

    #[test]
    fn prunes_only_the_dead_name_from_a_group_import() {
        // `Duration` stays in use (a param type); `Instant` becomes dead. Only
        // `Instant` is dropped from the group; `Duration` is preserved.
        let src = r#"
use std::time::{Duration, Instant};
struct Foo { x: u8 }
impl Foo {
    fn new() -> Self { Self { x: 0 } }
    fn nap(&self, d: Duration) { let _ = d; let _ = Instant::now(); }
}
"#;
        let out = modified(src);
        assert!(out.contains("self.time.instant_now()"), "got:\n{out}");
        assert!(
            out.contains("use std::time::{Duration};"),
            "group must keep Duration and drop Instant:\n{out}"
        );
        assert!(
            !out.contains("Instant"),
            "Instant must be gone entirely:\n{out}"
        );
    }

    #[test]
    fn byte_offsets_are_correct_for_multibyte_source() {
        // A non-ASCII comment before the leak shifts byte offsets vs char
        // offsets; the rewrite must still land exactly on the call.
        let src = "struct Foo { x: u8 }\n// café ☕ comment\nimpl Foo {\n    fn new() -> Self { Self { x: 0 } }\n    fn t(&self) -> std::time::Instant { Instant::now() }\n}\n";
        let out = modified(src);
        assert!(out.contains("self.time.instant_now()"), "got:\n{out}");
        assert!(!out.contains("Instant::now()"), "stale call:\n{out}");
        // The café comment must be preserved intact.
        assert!(out.contains("// café ☕ comment"), "corrupted:\n{out}");
    }
}
