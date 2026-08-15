//! Integration tests for the `check` subcommand — the run-twice-diff
//! determinism gesture.
//!
//! `check` runs a command N times under a fixed seed and reports whether the
//! (filtered stdout, exit status) is IDENTICAL across runs (deterministic,
//! exit 0) or DIVERGES (exit 1). A command that cannot be spawned is a usage
//! error (exit 2). It measures DIVERGENCE, not pass/fail: an identical failure
//! every run is still "deterministic".
//!
//! Commands are `sh -c` one-liners using only shell builtins (`echo`, `$$`,
//! `exit`) so the tests stay hermetic and portable — `$$` (the shell PID) is a
//! POSIX builtin that differs between the two distinct `sh` processes, giving a
//! reliable, tool-free source of nondeterminism.

use std::process::{Command, Output};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_navian-dst")
}

fn run(args: &[&str]) -> Output {
    Command::new(bin())
        .args(args)
        .output()
        .expect("failed to run navian-dst binary")
}

// ── deterministic command → exit 0 ───────────────────────────────────────────

#[test]
fn deterministic_command_is_identical_exit_zero() {
    let out = run(&["check", "--", "sh", "-c", "echo hello"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("deterministic") && stdout.contains("identical"),
        "expected an identical/deterministic report: {stdout}"
    );
    // Default seed is 0.
    assert!(stdout.contains("seed 0"), "should name the seed: {stdout}");
}

// ── nondeterministic command → exit 1 with a divergence diff ──────────────────

#[test]
fn nondeterministic_command_diverges_exit_one_with_diff() {
    // `$$` is the shell's PID: a fresh, different value for each of the two
    // distinct `sh` processes → a reliable divergence.
    let out = run(&["check", "--", "sh", "-c", "echo $$"]);
    assert_eq!(out.status.code(), Some(1), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("DIVERGENCE"), "expected a divergence report: {stdout}");
    // A unified diff of the first differing pair.
    assert!(
        stdout.contains("--- run 1") && stdout.contains("+++ run 2"),
        "expected a unified diff header: {stdout}"
    );
    // And a reproduce hint that names the seed env we export.
    assert!(
        stdout.contains("NAVIAN_DST_SEED=0") && stdout.contains("SEED=0"),
        "expected a reproduce hint with the seed env: {stdout}"
    );
}

// ── --ignore drops the varying line → back to identical (exit 0) ──────────────

#[test]
fn ignore_regex_masks_varying_line_exit_zero() {
    // Line 1 is stable; line 2 varies (PID). Ignoring the PID line makes the two
    // runs compare equal.
    let out = run(&[
        "check",
        "--ignore",
        "^pid ",
        "--",
        "sh",
        "-c",
        "echo stable; echo pid $$",
    ]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "ignoring the varying line should be identical; stdout: {} stderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    // Sanity: WITHOUT the ignore, the same command diverges.
    let bare = run(&["check", "--", "sh", "-c", "echo stable; echo pid $$"]);
    assert_eq!(bare.status.code(), Some(1), "without --ignore it must diverge");
}

// ── --runs 3 works ───────────────────────────────────────────────────────────

#[test]
fn runs_three_deterministic_exit_zero() {
    let out = run(&["check", "--runs", "3", "--", "sh", "-c", "echo fixed"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("3 runs identical"), "should report 3 runs: {stdout}");
}

// ── command not found → exit 2 (usage/tool error) ────────────────────────────

#[test]
fn command_not_found_exits_two() {
    let out = run(&["check", "--", "navian-dst-no-such-binary-xyzzy"]);
    assert_eq!(out.status.code(), Some(2), "a missing command must be a usage error");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("could not run command"), "expected a spawn error: {stderr}");
}

// ── deterministic FAILURE → exit 0 (divergence, not pass/fail) ────────────────

#[test]
fn identical_failure_is_deterministic_exit_zero() {
    // Fails with the same stdout and the same non-zero exit every run → this is
    // "deterministic" for the purposes of `check` (exit 0), proving check
    // measures divergence, not pass/fail.
    let out = run(&["check", "--", "sh", "-c", "echo boom; exit 3"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "an identical failure must be deterministic; stdout: {} stderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("deterministic"), "expected deterministic verdict: {stdout}");
    // And it reports the shared non-zero status honestly.
    assert!(stdout.contains("exit code 3"), "should surface the shared exit status: {stdout}");
}

// ── guardrails: bad --runs and bad --ignore are usage errors (exit 2) ─────────

#[test]
fn runs_below_two_is_usage_error() {
    let out = run(&["check", "--runs", "1", "--", "sh", "-c", "echo hi"]);
    assert_eq!(out.status.code(), Some(2), "runs < 2 must be a usage error");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--runs must be >= 2"), "expected a --runs error: {stderr}");
}

#[test]
fn invalid_ignore_regex_is_usage_error() {
    let out = run(&["check", "--ignore", "(", "--", "sh", "-c", "echo hi"]);
    assert_eq!(out.status.code(), Some(2), "a bad regex must be a usage error");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("invalid --ignore regex"), "expected a regex error: {stderr}");
}

// ── --timeout surfaces a hang instead of blocking forever ─────────────────────

#[test]
fn timeout_kills_a_hanging_command_and_reports_it() {
    // `sleep 30` would hang check forever without a timeout; --timeout 1 kills
    // it and reports the timeout (exit 1). Total wall time ~2s (two runs × ~1s).
    let out = run(&["check", "--timeout", "1", "--", "sh", "-c", "sleep 30"]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "a timed-out run must be surfaced as a failure; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("TIMEOUT"), "expected a timeout report: {stdout}");
}

#[test]
fn timeout_bounds_wall_clock_even_when_parent_exits_fast() {
    // The parent `sh` exits in milliseconds but backgrounds a `sleep 30` that
    // inherits the stdout pipe. `--timeout` must still bound total wall-clock
    // (via the reader drain), not block ~30s on the orphaned grandchild.
    let start = std::time::Instant::now();
    let out = run(&["check", "--timeout", "1", "--", "sh", "-c", "sleep 30 & echo done"]);
    let elapsed = start.elapsed();
    assert_eq!(
        out.status.code(),
        Some(1),
        "a run whose output never completes must be surfaced as a timeout; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        elapsed < std::time::Duration::from_secs(20),
        "check must respect --timeout, not block on the grandchild (took {elapsed:?})"
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("TIMEOUT"));
}

#[test]
fn timeout_generous_lets_fast_command_pass() {
    // A fast, deterministic command under a generous timeout is still identical.
    let out = run(&["check", "--timeout", "30", "--", "sh", "-c", "echo hi"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains("deterministic"));
}

#[test]
fn timeout_zero_is_usage_error() {
    let out = run(&["check", "--timeout", "0", "--", "sh", "-c", "echo hi"]);
    assert_eq!(out.status.code(), Some(2), "--timeout 0 must be a usage error");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--timeout must be >= 1"), "expected a timeout error: {stderr}");
}

// ── reproduce hint carries --ignore so a copy-paste re-run behaves the same ───

#[test]
fn divergence_reproduce_hint_includes_ignore_patterns() {
    // The command diverges even after filtering (an UN-ignored PID line), so we
    // still get a divergence; its re-run hint must echo the --ignore we passed.
    let out = run(&[
        "check",
        "--ignore",
        "^noise ",
        "--",
        "sh",
        "-c",
        "echo noise $$; echo pid $$",
    ]);
    assert_eq!(out.status.code(), Some(1), "should still diverge on the un-ignored line");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("--ignore '^noise '") || stdout.contains("--ignore ^noise"),
        "re-run hint must re-emit --ignore: {stdout}"
    );
}
