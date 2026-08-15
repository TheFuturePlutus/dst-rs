//! Part B backing tests for the `navian-dst migrate` SAFETY guarantees, run against
//! real crates via the compiled `navian-dst` binary:
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

/// Absolute path to the real `navian-dst` crate (sibling of `navian-dst-cli`).
fn navian_dst_dir() -> PathBuf {
    manifest_dir()
        .join("..")
        .join("navian-dst")
        .canonicalize()
        .expect("navian-dst crate should exist next to navian-dst-cli")
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

/// Run the compiled `navian-dst migrate` on `path` with a private target dir.
/// Returns (success, stdout).
fn run_migrate(path: &Path, target: &Path, extra: &[&str]) -> (bool, String) {
    let bin = env!("CARGO_BIN_EXE_navian-dst");
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

/// Run `cargo check --all-targets` in `dir` with the shared target dir.
fn cargo_check_all_targets(dir: &Path, target: &Path) -> (bool, String) {
    let out = Command::new("cargo")
        .args(["check", "--all-targets"])
        .current_dir(dir)
        .env("CARGO_TARGET_DIR", target)
        .output()
        .expect("failed to spawn cargo");
    let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
    combined.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.success(), combined)
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
/// FAIL `cargo check`, because the crate does NOT depend on `navian-dst` — so the
/// injected `navian_dst::Time` / `navian_dst::ProductionTime` references don't resolve.
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
        // Own workspace; NO navian-dst dependency (that's what makes the check fail).
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

    // Run migrate: it will rewrite lib.rs, `cargo check` will FAIL (no navian-dst
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

// ── The internal gate is `--all-targets`: migrate never ships a tree that fails
//    `cargo check --all-targets` (a public struct built via a literal in an
//    integration test would otherwise break only the test target). ─────────────

#[test]
fn migrate_rolls_back_when_it_would_break_an_integration_test() {
    let scratch = Scratch::new("extlit");
    let app = scratch.root.join("app");
    let target = scratch.root.join("target");
    let src = app.join("src");
    let tests = app.join("tests");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::create_dir_all(&tests).unwrap();

    // Depends on navian-dst so the migrated lib itself WOULD compile — the ONLY
    // break is in the integration-test target, which a lib-only check misses.
    let abs = navian_dst_dir();
    std::fs::write(
        app.join("Cargo.toml"),
        format!(
            "[workspace]\n\n[package]\nname = \"extlit\"\nversion = \"0.0.0\"\nedition = \"2021\"\npublish = false\n\n[dependencies]\nnavian-dst = {{ path = {:?} }}\n",
            abs.to_str().unwrap()
        ),
    )
    .unwrap();

    // A PUBLIC struct with a migratable leak. `id` is public so an external crate
    // can build it via a struct literal.
    std::fs::write(
        src.join("lib.rs"),
        "use std::time::Instant;\n\
         pub struct PublicFoo { pub id: u32 }\n\
         impl PublicFoo {\n\
         \x20   pub fn new(id: u32) -> Self { Self { id } }\n\
         \x20   pub fn tick(&self) -> Instant { Instant::now() }\n\
         }\n",
    )
    .unwrap();

    // Integration test (a SEPARATE crate) that builds PublicFoo via a struct
    // literal — outside the struct's module, so migrate cannot rewrite it, and
    // after the private `time` field is injected this target no longer compiles.
    std::fs::write(
        tests.join("it.rs"),
        "use extlit::PublicFoo;\n\
         #[test]\n\
         fn builds_via_literal() {\n\
         \x20   let foo = PublicFoo { id: 9 };\n\
         \x20   assert_eq!(foo.id, 9);\n\
         }\n",
    )
    .unwrap();

    let lib_rs = src.join("lib.rs");
    let it_rs = tests.join("it.rs");

    // Sanity: everything compiles across ALL targets before migration.
    let (ok, log) = cargo_check_all_targets(&app, &target);
    assert!(
        ok,
        "fixture must compile (all targets) before migration:\n{log}"
    );

    let lib_before = read(&lib_rs);
    let it_before = read(&it_rs);

    // Migrate ONLY src/. The internal `--all-targets` gate must catch the break
    // in the integration-test target and roll the WHOLE run back.
    let (ok, out) = run_migrate(&src, &target, &[]);
    assert!(
        !ok,
        "migrate must exit non-zero when it would break any target:\n{out}"
    );
    assert!(
        out.contains("FAILED") && out.contains("RESTORED"),
        "migrate must report the target failure and the restore:\n{out}"
    );

    // Rolled back to EXACT pre-migration bytes — nothing applied.
    assert_eq!(
        read(&lib_rs),
        lib_before,
        "lib.rs must be restored exactly (migration must not ship)"
    );
    assert_eq!(
        read(&it_rs),
        it_before,
        "the integration test must be byte-identical"
    );

    // The invariant: the tree still compiles across ALL targets (leak left
    // un-migrated, which is the correct safe outcome).
    let (ok, log) = cargo_check_all_targets(&app, &target);
    assert!(
        ok,
        "after rollback the tree must be green under `cargo check --all-targets`:\n{log}"
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

    // This crate DOES depend on navian-dst (absolute path), so the migrated code
    // compiles and `cargo check` PASSES.
    let abs = navian_dst_dir();
    std::fs::write(
        app.join("Cargo.toml"),
        format!(
            "[workspace]\n\n[package]\nname = \"bothsites\"\nversion = \"0.0.0\"\nedition = \"2021\"\npublish = false\n\n[dependencies]\nnavian-dst = {{ path = {:?} }}\n",
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
        migrated.contains("time: std::sync::Arc<dyn navian_dst::Time>"),
        "field must be added:\n{migrated}"
    );
    let defaults = migrated
        .matches("time: std::sync::Arc::new(navian_dst::ProductionTime::default())")
        .count();
    assert_eq!(
        defaults, 2,
        "both the constructor and the direct struct literal must be defaulted:\n{migrated}"
    );
}

// ── Doctest gate: opt-in `--check-doctests`, plus a default warning ───────────
//
// `cargo check --all-targets` cannot verify doctests (cargo only checks them by
// RUNNING them), so a struct built via a literal inside a doc example is a gap.
// `--check-doctests` closes it (roll back on doctest failure); by default migrate
// still applies but WARNS so the gap is never silent.

/// Write a crate depending on navian-dst whose lib is `src/lib.rs` = `lib_src`,
/// with package/crate name `name`. Returns (app_dir, src_dir).
fn write_dep_crate(scratch: &Scratch, name: &str, lib_src: &str) -> (PathBuf, PathBuf) {
    let app = scratch.root.join("app");
    let src = app.join("src");
    std::fs::create_dir_all(&src).unwrap();
    let abs = navian_dst_dir();
    std::fs::write(
        app.join("Cargo.toml"),
        format!(
            "[workspace]\n\n[package]\nname = \"{name}\"\nversion = \"0.0.0\"\nedition = \"2021\"\npublish = false\n\n[dependencies]\nnavian-dst = {{ path = {:?} }}\n",
            abs.to_str().unwrap()
        ),
    )
    .unwrap();
    std::fs::write(src.join("lib.rs"), lib_src).unwrap();
    (app, src)
}

/// A lib whose PUBLIC struct is built via a struct LITERAL inside a doctest —
/// migration adds the private `time` field and breaks that doctest.
const DOCTEST_LITERAL_LIB: &str = "use std::time::Instant;\n\
     /// A widget.\n\
     ///\n\
     /// ```\n\
     /// use doclit::PublicFoo;\n\
     /// let f = PublicFoo { id: 4 };\n\
     /// assert_eq!(f.id, 4);\n\
     /// ```\n\
     pub struct PublicFoo { pub id: u32 }\n\
     impl PublicFoo {\n\
     \x20   pub fn new(id: u32) -> Self { Self { id } }\n\
     \x20   pub fn tick(&self) -> Instant { Instant::now() }\n\
     }\n";

#[test]
fn check_doctests_gate_rolls_back_when_a_doctest_builds_via_literal() {
    let scratch = Scratch::new("doclit");
    let target = scratch.root.join("target");
    let (_app, src) = write_dep_crate(&scratch, "doclit", DOCTEST_LITERAL_LIB);
    let lib_rs = src.join("lib.rs");
    let before = read(&lib_rs);

    // With --check-doctests, the doctest gate runs after --all-targets passes,
    // catches the broken doctest, and rolls the WHOLE run back.
    let (ok, out) = run_migrate(&src, &target, &["--check-doctests"]);
    assert!(
        !ok,
        "migrate --check-doctests must exit non-zero on a broken doctest:\n{out}"
    );
    assert!(
        out.contains("FAILED") && out.contains("RESTORED"),
        "migrate must report the doctest failure and the restore:\n{out}"
    );
    assert_eq!(
        read(&lib_rs),
        before,
        "lib.rs must be restored exactly — a broken doctest must not ship"
    );
}

#[test]
fn without_flag_migrates_but_warns_that_doctests_are_unverified() {
    let scratch = Scratch::new("docwarn");
    let target = scratch.root.join("target");
    // Same crate, but package name `doclit` so the doctest path matches; without
    // --check-doctests migrate APPLIES (the doctest gap is not gated) and warns.
    let (_app, src) = write_dep_crate(&scratch, "doclit", DOCTEST_LITERAL_LIB);
    let lib_rs = src.join("lib.rs");

    let (ok, out) = run_migrate(&src, &target, &[]);
    assert!(ok, "migrate must succeed without the doctest gate:\n{out}");
    assert!(
        out.contains("cargo check: PASSED"),
        "the --all-targets gate must pass:\n{out}"
    );
    // The gap must be surfaced, not silent.
    assert!(
        out.contains("doctests are not verified")
            && out.contains("--check-doctests"),
        "migrate must warn that doctests were not verified:\n{out}"
    );
    // It did apply the rewrite (shipping, with the warning).
    let migrated = std::fs::read_to_string(&lib_rs).unwrap();
    assert!(
        migrated.contains("self.time.instant_now()"),
        "the leak should have been rewritten:\n{migrated}"
    );
}

#[test]
fn check_doctests_gate_allows_a_passing_doctest() {
    // A doctest that builds the struct via its CONSTRUCTOR keeps working after
    // migration (the field defaults to production), so --check-doctests must NOT
    // falsely roll it back.
    let lib_src = "use std::time::Instant;\n\
         /// A widget.\n\
         ///\n\
         /// ```\n\
         /// use docok::PublicFoo;\n\
         /// let f = PublicFoo::new(4);\n\
         /// assert_eq!(f.id(), 4);\n\
         /// ```\n\
         pub struct PublicFoo { id: u32 }\n\
         impl PublicFoo {\n\
         \x20   pub fn new(id: u32) -> Self { Self { id } }\n\
         \x20   pub fn id(&self) -> u32 { self.id }\n\
         \x20   pub fn tick(&self) -> Instant { Instant::now() }\n\
         }\n";
    let scratch = Scratch::new("docok");
    let target = scratch.root.join("target");
    let (_app, src) = write_dep_crate(&scratch, "docok", lib_src);
    let lib_rs = src.join("lib.rs");

    let (ok, out) = run_migrate(&src, &target, &["--check-doctests"]);
    assert!(
        ok,
        "migrate --check-doctests must succeed when doctests pass:\n{out}"
    );
    assert!(
        out.contains("cargo check: PASSED") && out.contains("cargo test --doc: PASSED"),
        "both gates must pass:\n{out}"
    );
    let migrated = std::fs::read_to_string(&lib_rs).unwrap();
    assert!(
        migrated.contains("self.time.instant_now()"),
        "the leak must have been rewritten (change applied):\n{migrated}"
    );
}
