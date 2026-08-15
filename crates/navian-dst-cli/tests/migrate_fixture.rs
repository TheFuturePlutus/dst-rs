//! Integration test for `navian-dst migrate`, run against a REAL compiling crate.
//!
//! The committed fixture at `tests/fixtures/app/` is a standalone Cargo crate
//! that depends on `navian-dst`. This test copies it to a temp dir (so the checked
//! -in fixture stays pristine and the test is repeatable), rewrites the `navian-dst`
//! dependency to an absolute path, and then proves the six properties that make
//! `migrate` trustworthy:
//!
//!   1. the fixture COMPILES BEFORE migration,
//!   2. `migrate` runs and reports the expected work,
//!   3. it STILL COMPILES AFTER migration  (seam-safety — the key property),
//!   4. a `SimulatedTime` can be injected and yields DETERMINISTIC behavior,
//!   5. the free-function leak is REPORTED AS SKIPPED, not rewritten,
//!   6. a second `migrate` is a no-op  (IDEMPOTENCY).
//!
//! All cargo invocations for the copy share one temp `CARGO_TARGET_DIR`, and the
//! copy is its own workspace, so nothing here contends with the outer build.

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

fn fixture_src() -> PathBuf {
    manifest_dir().join("tests").join("fixtures").join("app")
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
        let root = std::env::temp_dir().join(format!("dst-migrate-it-{tag}-{nanos}"));
        std::fs::create_dir_all(&root).expect("create scratch dir");
        Scratch { root }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Recursively copy `src` dir into `dst` dir (creating `dst`), skipping any
/// `target/` directory. No external crates.
fn copy_dir(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name();
        if name == "target" {
            continue;
        }
        let from = entry.path();
        let to = dst.join(&name);
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&from, &to);
        } else {
            std::fs::copy(&from, &to).unwrap();
        }
    }
}

/// Run `cargo <args>` in `dir` with the shared target dir; return (ok, stdout+stderr).
fn cargo(dir: &Path, target: &Path, args: &[&str]) -> (bool, String) {
    let out = Command::new("cargo")
        .args(args)
        .current_dir(dir)
        .env("CARGO_TARGET_DIR", target)
        .output()
        .expect("failed to spawn cargo");
    let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
    combined.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.success(), combined)
}

/// Run the compiled `navian-dst` binary with the shared target dir in its env (so its
/// internal `cargo check` reuses the same build cache). Returns (ok, stdout).
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

#[test]
fn migrate_is_seam_safe_deterministic_and_idempotent() {
    let scratch = Scratch::new("app");
    let app = scratch.root.join("app");
    let target = scratch.root.join("target");
    copy_dir(&fixture_src(), &app);

    // Point the copy at the real navian-dst via an absolute path.
    let manifest = app.join("Cargo.toml");
    let toml = std::fs::read_to_string(&manifest).unwrap();
    let abs = navian_dst_dir();
    let toml = toml.replace(
        "path = \"../../../../navian-dst\"",
        &format!("path = {:?}", abs.to_str().unwrap()),
    );
    assert!(
        toml.contains(abs.to_str().unwrap()),
        "failed to rewrite navian-dst path in fixture Cargo.toml"
    );
    std::fs::write(&manifest, toml).unwrap();

    let src_dir = app.join("src");
    let lib_rs = src_dir.join("lib.rs");

    // ── (1) Compiles BEFORE migration. ──
    let (ok, log) = cargo(&app, &target, &["check", "--quiet"]);
    assert!(ok, "fixture must compile before migration:\n{log}");

    // ── (2) Run migrate; (3) it compiles after (migrate's own cargo check);
    //        (5) the free-fn leak is reported skipped. ──
    let (ok, out) = run_migrate(&src_dir, &target, &[]);
    assert!(ok, "migrate exited non-zero:\n{out}");
    assert!(
        out.contains("cargo check: PASSED"),
        "migrate must compile the result (seam-safety):\n{out}"
    );
    assert!(
        out.contains("structs migrated : 1") && out.contains("RateLimiter"),
        "expected RateLimiter migrated:\n{out}"
    );
    assert!(
        out.contains("leaks rewritten  : 3"),
        "expected 3 leaks rewritten:\n{out}"
    );
    // (5) two leaks reported as skipped: the free function, and the leak inside
    //     the `#[derive(Debug, PartialEq)]` struct (which must NOT be migrated).
    assert!(
        out.contains("leaks skipped    : 2"),
        "expected exactly 2 skips (free fn + derive-Debug struct):\n{out}"
    );
    assert!(
        out.contains("free fn") && out.contains("SystemTime::now()"),
        "the skip must include the free-function SystemTime leak, reported (not rewritten):\n{out}"
    );
    // The derive-Debug struct's leak must be reported as skipped, not rewritten.
    assert!(
        out.contains("derive"),
        "expected a skip explaining the #[derive] struct was left alone:\n{out}"
    );

    // Verify the actual rewrites landed, and the free fn was left ALONE.
    let migrated = std::fs::read_to_string(&lib_rs).unwrap();
    assert!(migrated.contains("time: std::sync::Arc<dyn navian_dst::Time>"));
    assert!(migrated.contains("self.time.now_ms() as i64"));
    assert!(migrated.contains("self.time.instant_now()"));
    assert!(migrated.contains("self.time.sleep(d).await"));
    // The free function keeps its original SystemTime::now() leak verbatim.
    let boot_body = migrated
        .split("pub fn boot_timestamp")
        .nth(1)
        .expect("boot_timestamp fn present");
    assert!(
        boot_body.contains("SystemTime::now()"),
        "free-fn leak must NOT be rewritten:\n{boot_body}"
    );

    // The derive-Debug struct must be left completely untouched: its derives and
    // its SystemTime leak survive verbatim, and it gains no `time` field. (That
    // the whole crate still `cargo check`s above proves the derives weren't
    // broken — otherwise the run would have reverted.)
    assert!(
        migrated.contains("#[derive(Debug, PartialEq)]"),
        "derive-Debug struct's derives must be preserved:\n{migrated}"
    );
    let stamped_impl = migrated
        .split("impl Stamped")
        .nth(1)
        .expect("Stamped impl present");
    assert!(
        stamped_impl.contains("SystemTime::now()"),
        "derive-Debug struct's leak must NOT be rewritten:\n{stamped_impl}"
    );
    assert!(
        !stamped_impl.contains("self.time"),
        "derive-Debug struct must not be routed through self.time:\n{stamped_impl}"
    );

    // ── (6) Second run is a no-op / idempotent. ──
    let before = std::fs::read_to_string(&lib_rs).unwrap();
    let (ok, out2) = run_migrate(&src_dir, &target, &[]);
    assert!(ok, "second migrate exited non-zero:\n{out2}");
    assert!(
        out2.contains("leaks rewritten  : 0") && out2.contains("idempotent no-op"),
        "second run must be a no-op:\n{out2}"
    );
    let after = std::fs::read_to_string(&lib_rs).unwrap();
    assert_eq!(
        before, after,
        "second migrate changed the file — not idempotent"
    );

    // ── (4) SimulatedTime injects and behaves deterministically. ──
    // Append an in-crate test (private-field access) that constructs the
    // migrated struct with a sim clock, then run it.
    let sim_test = r#"

#[cfg(test)]
mod injected_sim_test {
    use super::*;
    use navian_dst::SimulatedTime;
    use std::sync::Arc;

    #[test]
    fn deterministic_under_simulated_clock() {
        let clock = Arc::new(SimulatedTime::new(1_000));
        let rl = RateLimiter { time: clock.clone(), tokens: 5, last_ms: 0 };
        assert_eq!(rl.now_ms(), 1_000);
        clock.advance_ms(500);
        assert_eq!(rl.now_ms(), 1_500);
        let i0 = rl.mono();
        clock.advance_ms(250);
        let i1 = rl.mono();
        assert_eq!(i1.duration_since(i0), std::time::Duration::from_millis(250));
    }
}
"#;
    let mut with_test = std::fs::read_to_string(&lib_rs).unwrap();
    with_test.push_str(sim_test);
    std::fs::write(&lib_rs, with_test).unwrap();

    // External-style injection: a SEPARATE test crate (an integration test) can
    // only touch PUBLIC API, so it MUST go through the generated `with_time`
    // builder — the private `time` field is inaccessible (E0451) without it. This
    // proves the builder is public AND actually routes the injected clock.
    let tests_dir = app.join("tests");
    std::fs::create_dir_all(&tests_dir).unwrap();
    std::fs::write(
        tests_dir.join("inject.rs"),
        r#"
use app::RateLimiter;
use navian_dst::SimulatedTime;
use std::sync::Arc;

#[test]
fn external_with_time_injection_uses_the_injected_clock() {
    let clock = Arc::new(SimulatedTime::new(4_242));
    // PUBLIC API only: `new` + the generated `with_time` consuming builder.
    let rl = RateLimiter::new(5).with_time(clock.clone());
    assert_eq!(rl.now_ms(), 4_242);
    clock.advance_ms(100);
    assert_eq!(rl.now_ms(), 4_342);
}
"#,
    )
    .unwrap();

    let (ok, log) = cargo(&app, &target, &["test"]);
    assert!(
        ok && log.contains("deterministic_under_simulated_clock")
            && log.contains("external_with_time_injection_uses_the_injected_clock"),
        "SimulatedTime injection tests (in-crate field + external builder) must pass:\n{log}"
    );
}
