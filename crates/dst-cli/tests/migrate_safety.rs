//! Part B backing tests for the `dst migrate` SAFETY guarantees, run against
//! real crates via the compiled `dst` binary:
//!
//!   * **Dirty-worktree-safe** — a file migrate will not touch is byte-identical
//!     after a run, even when that run fails and reverts (B1).
//!   * **Exact-byte rollback** — on a `cargo check` failure, every edited file is
//!     restored to its pre-migration bytes exactly (B2).
//!   * **`--dry-run` makes no changes** — every file is byte-identical before and
//!     after a dry run (B4).
//!   * **Constructor AND struct-literal** — a struct built both via `Foo::new()`
//!     and via a direct `Foo { .. }` literal compiles after migrate (B7).
//!
//! Each temp crate is its own workspace (`[workspace]`) with a private
//! `CARGO_TARGET_DIR`, so nothing here contends with the outer build.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Absolute path to the real `dst-rs` crate (sibling of `dst-cli`).
fn dst_rs_dir() -> PathBuf {
    manifest_dir()
        .join("..")
        .join("dst-rs")
        .canonicalize()
        .expect("dst-rs crate should exist next to dst-cli")
}

/// A unique scratch dir under the system temp, removed on drop.
struct Scratch {
    root: PathBuf,
}
impl Scratch {
    fn new(tag: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("dst-migrate-safety-{tag}-{nanos}"));
        std::fs::create_dir_all(&root).expect("create scratch dir");
        Scratch { root }
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Run the compiled `dst migrate` on `path` with a private target dir.
/// Returns (success, stdout).
fn run_migrate(path: &Path, target: &Path, extra: &[&str]) -> (bool, String) {
    let bin = env!("CARGO_BIN_EXE_dst");
    let mut args = vec!["migrate", path.to_str().unwrap()];
    args.extend_from_slice(extra);
    let out = Command::new(bin)
        .args(&args)
        .env("CARGO_TARGET_DIR", target)
        .output()
        .expect("failed to run dst binary");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut h = DefaultHasher::new();
    bytes.hash(&mut h);
    h.finish()
}

fn read(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

// ── B1 + B2: a FAILING migrate is dirty-worktree-safe and rolls back exactly ──

/// Build a temp crate that COMPILES before migration but whose migration will
/// FAIL `cargo check`, because the crate does NOT depend on `dst-rs` — so the
/// injected `dst_rs::Time` / `dst_rs::ProductionTime` references don't resolve.
/// This deterministically drives migrate's write → check → restore path.
///
/// * `src/lib.rs` — a migratable `Widget` (rewritten, then reverted).
/// * `src/untouched.rs` — a file migrate SCANS but does not rewrite (a free-fn
///   leak is skipped), carrying a distinctive marker to prove byte-identity.
fn write_no_dep_crate(app: &Path) {
    let src = app.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        app.join("Cargo.toml"),
        // Own workspace; NO dst-rs dependency (that's what makes the check fail).
        "[workspace]\n\n[package]\nname = \"nodep\"\nversion = \"0.0.0\"\nedition = \"2021\"\npublish = false\n",
    )
    .unwrap();
    std::fs::write(
        src.join("lib.rs"),
        "pub mod untouched;\n\
         use std::time::Instant;\n\
         pub struct Widget { n: u64 }\n\
         impl Widget {\n\
         \x20   pub fn new() -> Self { Self { n: 0 } }\n\
         \x20   pub fn n(&self) -> u64 { self.n }\n\
         \x20   pub fn tick(&self) -> Instant { Instant::now() }\n\
         }\n",
    )
    .unwrap();
    // A DISTINCTIVE pre-existing edit in a file migrate will not rewrite (the
    // free-function leak is reported+skipped, never touched).
    std::fs::write(
        src.join("untouched.rs"),
        "// PRE-EXISTING UNCOMMITTED EDIT — migrate must never touch this byte.\n\
         use std::time::{SystemTime, UNIX_EPOCH};\n\
         pub fn boot_ms() -> i64 {\n\
         \x20   SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64\n\
         }\n",
    )
    .unwrap();
}

#[test]
fn failing_migrate_is_dirty_worktree_safe_and_rolls_back_exact_bytes() {
    let scratch = Scratch::new("nodep");
    let app = scratch.root.join("app");
    let target = scratch.root.join("target");
    write_no_dep_crate(&app);

    let src = app.join("src");
    let lib_rs = src.join("lib.rs");
    let untouched_rs = src.join("untouched.rs");

    // Pre-migration snapshots (exact bytes + hashes).
    let lib_before = read(&lib_rs);
    let untouched_before = read(&untouched_rs);
    let lib_hash_before = hash_bytes(&lib_before);
    let untouched_hash_before = hash_bytes(&untouched_before);

    // Run migrate: it will rewrite lib.rs, `cargo check` will FAIL (no dst-rs
    // dep), and migrate must restore the originals and exit non-zero.
    let (ok, out) = run_migrate(&src, &target, &[]);
    assert!(
        !ok,
        "migrate must exit non-zero when its rewrite fails cargo check:\n{out}"
    );
    assert!(
        out.contains("cargo check: FAILED") && out.contains("RESTORED"),
        "migrate must report the failure and the restore:\n{out}"
    );

    // B2: the edited file is restored to its EXACT pre-migration bytes.
    let lib_after = read(&lib_rs);
    assert_eq!(
        hash_bytes(&lib_after),
        lib_hash_before,
        "edited lib.rs must hash-match the pre-migration snapshot after rollback"
    );
    assert_eq!(
        lib_after, lib_before,
        "lib.rs bytes must be exactly restored"
    );

    // B1: the file migrate did not rewrite is byte-identical (the pre-existing
    // uncommitted edit survives untouched).
    let untouched_after = read(&untouched_rs);
    assert_eq!(
        hash_bytes(&untouched_after),
        untouched_hash_before,
        "the untouched file must hash-match its pre-migration bytes"
    );
    assert_eq!(
        untouched_after, untouched_before,
        "the untouched file's bytes must be identical"
    );
}

// ── B4: --dry-run changes nothing on disk ────────────────────────────────────

#[test]
fn dry_run_makes_no_changes_on_disk() {
    let scratch = Scratch::new("dryrun");
    let app = scratch.root.join("app");
    let target = scratch.root.join("target");
    write_no_dep_crate(&app); // migratable, and dry-run never runs cargo check

    let src = app.join("src");
    let files = [src.join("lib.rs"), src.join("untouched.rs")];
    let before: Vec<(PathBuf, u64)> = files
        .iter()
        .map(|f| (f.clone(), hash_bytes(&read(f))))
        .collect();

    let (ok, out) = run_migrate(&src, &target, &["--dry-run"]);
    assert!(ok, "dry-run migrate should succeed:\n{out}");
    // It must PLAN a rewrite (a diff is shown) so this isn't a vacuous no-op.
    assert!(
        out.contains("--- a/") && out.contains("self.time.instant_now"),
        "dry-run must print the planned rewrite diff:\n{out}"
    );

    // Every file is byte-identical after the dry run.
    for (f, h_before) in &before {
        let h_after = hash_bytes(&read(f));
        assert_eq!(
            h_after,
            *h_before,
            "--dry-run modified {} on disk",
            f.display()
        );
    }
}

// ── B7: constructor AND struct-literal both compile after migrate ────────────

#[test]
fn constructor_and_struct_literal_both_compile_after_migrate() {
    let scratch = Scratch::new("bothsites");
    let app = scratch.root.join("app");
    let target = scratch.root.join("target");
    let src = app.join("src");
    std::fs::create_dir_all(&src).unwrap();

    // This crate DOES depend on dst-rs (absolute path), so the migrated code
    // compiles and `cargo check` PASSES.
    let abs = dst_rs_dir();
    std::fs::write(
        app.join("Cargo.toml"),
        format!(
            "[workspace]\n\n[package]\nname = \"bothsites\"\nversion = \"0.0.0\"\nedition = \"2021\"\npublish = false\n\n[dependencies]\ndst-rs = {{ path = {:?} }}\n",
            abs.to_str().unwrap()
        ),
    )
    .unwrap();

    // `Widget` is constructed BOTH via `Widget::new()` (a `Self { .. }` literal)
    // and via a direct `Widget { .. }` literal in a free function. Both sites
    // must receive the defaulted `time` field and compile after migrate.
    std::fs::write(
        src.join("lib.rs"),
        "use std::time::Instant;\n\
         pub struct Widget { n: u64 }\n\
         impl Widget {\n\
         \x20   pub fn new() -> Self { Self { n: 0 } }\n\
         \x20   pub fn n(&self) -> u64 { self.n }\n\
         \x20   pub fn tick(&self) -> Instant { Instant::now() }\n\
         }\n\
         // Direct struct-literal construction, NOT via the constructor.\n\
         pub fn make_seven() -> Widget { Widget { n: 7 } }\n",
    )
    .unwrap();

    let lib_rs = src.join("lib.rs");
    let (ok, out) = run_migrate(&src, &target, &[]);
    assert!(ok, "migrate must succeed and compile the result:\n{out}");
    assert!(
        out.contains("cargo check: PASSED"),
        "migrate must pass cargo check (both construction sites defaulted):\n{out}"
    );
    assert!(
        out.contains("structs migrated : 1") && out.contains("Widget"),
        "Widget must be migrated:\n{out}"
    );

    let migrated = std::fs::read_to_string(&lib_rs).unwrap();
    // The leak was rewritten.
    assert!(
        migrated.contains("self.time.instant_now()"),
        "leak must be rewritten:\n{migrated}"
    );
    // The field was added, and BOTH construction sites got the default (two
    // inserts: the `new()` `Self { .. }` and the `make_seven()` `Widget { .. }`).
    assert!(
        migrated.contains("time: std::sync::Arc<dyn dst_rs::Time>"),
        "field must be added:\n{migrated}"
    );
    let defaults = migrated
        .matches("time: std::sync::Arc::new(dst_rs::ProductionTime::default())")
        .count();
    assert_eq!(
        defaults, 2,
        "both the constructor and the direct struct literal must be defaulted:\n{migrated}"
    );
}
