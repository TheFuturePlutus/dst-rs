//! Integration test: run the compiled `dst scan --json` against the checked-in
//! leaky fixture crate and assert it finds EXACTLY the expected leaks — the
//! right count in each category, and ZERO false positives on the decoys.
//!
//! Precision is the priority: a decoy that gets flagged fails the test.

use std::path::PathBuf;
use std::process::Command;

use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct Leak {
    file: String,
    #[allow(dead_code)]
    line: usize,
    #[allow(dead_code)]
    col: usize,
    category: String,
    snippet: String,
    #[serde(rename = "fn")]
    #[allow(dead_code)]
    func: Option<String>,
}

fn fixtures_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

fn fixture_dir() -> PathBuf {
    fixtures_root().join("leaky")
}

fn run_scan_json() -> Vec<Leak> {
    let bin = env!("CARGO_BIN_EXE_dst");
    let out = Command::new(bin)
        .arg("scan")
        .arg(fixture_dir())
        .arg("--json")
        .output()
        .expect("failed to run dst binary");

    assert!(
        out.status.success(),
        "scan exited non-zero: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("scan did not emit valid JSON")
}

fn count(leaks: &[Leak], cat: &str) -> usize {
    leaks.iter().filter(|l| l.category == cat).count()
}

#[test]
fn finds_exactly_the_expected_leaks() {
    let leaks = run_scan_json();

    // Exact per-category counts.
    assert_eq!(count(&leaks, "time"), 5, "TIME leaks: {leaks:#?}");
    // 10 RANDOM: rand::random, thread_rng, gen_range, gen, fastrand, Uuid::new_v4,
    // Uuid::now_v7, SmallRng::from_entropy, OsRng (receiver form), OsRng (bound to
    // a local — value/path-expression form).
    assert_eq!(count(&leaks, "random"), 10, "RANDOM leaks: {leaks:#?}");
    assert_eq!(count(&leaks, "network"), 5, "NETWORK leaks: {leaks:#?}");
    assert_eq!(
        count(&leaks, "concurrency"),
        2,
        "CONCURRENCY leaks: {leaks:#?}"
    );

    // Exact total — no extra, no missing.
    assert_eq!(leaks.len(), 22, "total leaks: {leaks:#?}");

    // Only these four fixture files contain leaks; every category has exactly
    // one file. Nothing else may appear.
    let expected_categories = ["time", "random", "network", "concurrency"];
    for l in &leaks {
        assert!(
            expected_categories.contains(&l.category.as_str()),
            "unexpected category {}",
            l.category
        );
    }
}

#[test]
fn zero_false_positives_on_decoys() {
    let leaks = run_scan_json();

    // decoys.rs is pure decoys — it must never surface a single leak.
    let from_decoys: Vec<_> = leaks
        .iter()
        .filter(|l| l.file.ends_with("decoys.rs"))
        .collect();
    assert!(
        from_decoys.is_empty(),
        "false positives in decoys.rs: {from_decoys:#?}"
    );

    // The user-defined `Clock::now()` method call must not be flagged: no leak
    // snippet should be the bare `.now()` method form.
    assert!(
        !leaks.iter().any(|l| l.snippet.contains("clock.now")),
        "flagged a user-defined .now() method"
    );

    // The user free functions `spawn()`, `sleep()`, `timeout()` called bare
    // must not be flagged.
    assert!(
        !leaks.iter().any(|l| l.snippet == "spawn()"
            || l.snippet == "sleep(10)"
            || l.snippet == "timeout()"),
        "flagged a bare user-defined free function"
    );

    // The `.generate()` and `.spawn()` decoy methods must not be flagged.
    assert!(
        !leaks
            .iter()
            .any(|l| l.snippet.contains("generate()") || l.snippet.contains("b.spawn()")),
        "flagged a decoy method call"
    );

    // A user module's own `my_utils::thread_rng()` must NOT be flagged — only
    // `rand`'s `thread_rng` is a leak (qualification, not bare name match).
    assert!(
        !leaks
            .iter()
            .any(|l| l.snippet.contains("my_utils::thread_rng")),
        "flagged a user-defined qualified thread_rng()"
    );
}

#[test]
fn spot_check_specific_leaks() {
    let leaks = run_scan_json();

    let has = |cat: &str, needle: &str| {
        leaks
            .iter()
            .any(|l| l.category == cat && l.snippet.contains(needle))
    };

    assert!(has("time", "SystemTime::now"), "missing SystemTime::now");
    assert!(has("time", "Instant::now"), "missing Instant::now");
    assert!(has("time", "thread::sleep"), "missing thread::sleep");
    assert!(has("time", "Utc::now"), "missing chrono::Utc::now");
    assert!(has("random", "rand::random"), "missing rand::random");
    assert!(has("random", "thread_rng"), "missing thread_rng");
    assert!(has("random", "gen_range"), "missing gen_range");
    assert!(has("random", "new_v4"), "missing Uuid::new_v4");
    assert!(has("random", "now_v7"), "missing Uuid::now_v7");
    assert!(has("random", "fastrand"), "missing fastrand");
    assert!(has("random", "from_entropy"), "missing from_entropy");
    assert!(has("random", "OsRng"), "missing OsRng");
    assert!(has("network", "reqwest::get"), "missing reqwest::get");
    assert!(has("network", "TcpStream"), "missing TcpStream");
    assert!(has("network", "tokio::net"), "missing tokio::net");
    assert!(has("concurrency", "thread::spawn"), "missing thread::spawn");
    assert!(has("concurrency", "tokio::spawn"), "missing tokio::spawn");
}

fn run_scan(dir: &str, extra: &[&str]) -> std::process::Output {
    let bin = env!("CARGO_BIN_EXE_dst");
    Command::new(bin)
        .arg("scan")
        .arg(fixtures_root().join(dir))
        .args(extra)
        .output()
        .expect("failed to run dst binary")
}

#[test]
fn possible_random_is_listed_in_human_report() {
    // MAJOR 4 regression: a scan whose only finding is POSSIBLE-RANDOM must LIST
    // it (previously it was counted in the total but never displayed) and the
    // summary count must match.
    let out = run_scan("possible_only", &[]);
    assert!(out.status.success(), "scan should succeed without --deny");
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        stdout.contains("POSSIBLE-RANDOM (1)"),
        "human report must list the POSSIBLE-RANDOM finding:\n{stdout}"
    );
    assert!(
        stdout.contains("from_entropy"),
        "the finding snippet must appear:\n{stdout}"
    );
    assert!(
        stdout.contains("1 possible-random"),
        "summary line must count possible-random:\n{stdout}"
    );
    // It must not masquerade as a hard RANDOM leak.
    assert!(
        stdout.contains("0 random"),
        "possible-random must not be counted as hard random:\n{stdout}"
    );
    // JSON total is 1 and the single leak is possible-random.
    let leaks = run_scan_dir_json("possible_only");
    assert_eq!(leaks.len(), 1, "exactly one finding: {leaks:#?}");
    assert_eq!(leaks[0].category, "possible-random");
}

#[test]
fn deny_does_not_fail_on_possible_random_only() {
    // MAJOR 5 regression: `--deny` on a possible-random-only scan exits 0.
    let out = run_scan("possible_only", &["--deny"]);
    assert!(
        out.status.success(),
        "--deny must not fail on POSSIBLE-RANDOM only: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn deny_possible_fails_on_possible_random() {
    // Opt-in `--deny-possible` DOES fail on a possible-random-only scan.
    let out = run_scan("possible_only", &["--deny", "--deny-possible"]);
    assert!(
        !out.status.success(),
        "--deny --deny-possible must fail on POSSIBLE-RANDOM"
    );
}

#[test]
fn deny_fails_on_hard_leak() {
    // MAJOR 5 regression: `--deny` on a hard leak exits non-zero.
    let out = run_scan("hard_only", &["--deny"]);
    assert!(
        !out.status.success(),
        "--deny must fail on a hard (time) leak"
    );
}

fn run_scan_dir_json(dir: &str) -> Vec<Leak> {
    let out = run_scan(dir, &["--json"]);
    assert!(
        out.status.success(),
        "scan --json exited non-zero: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("scan did not emit valid JSON")
}
