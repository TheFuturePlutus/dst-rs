//! Integration test: run the compiled `navian-dst scan --json` against the checked-in
//! leaky fixture crate and assert it finds EXACTLY the expected leaks — the
//! right count in each category, and ZERO false positives on the decoys.
//!
//! Precision is the priority: a decoy that gets flagged fails the test.

use std::path::PathBuf;
use std::process::Command;

use serde::Deserialize;

// Mirrors the stable `--json` item schema:
// {rule_id, confidence, category, file, line, col, function, snippet}.
#[derive(Debug, Deserialize)]
struct Leak {
    rule_id: String,
    confidence: String,
    category: String,
    file: String,
    #[allow(dead_code)]
    line: usize,
    #[allow(dead_code)]
    col: usize,
    #[allow(dead_code)]
    function: Option<String>,
    snippet: String,
}

fn fixtures_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

fn scan_json_dir(dir: &str) -> Vec<Leak> {
    let bin = env!("CARGO_BIN_EXE_navian-dst");
    let out = Command::new(bin)
        .arg("scan")
        .arg(fixtures_root().join(dir))
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

fn run_scan_json() -> Vec<Leak> {
    scan_json_dir("leaky")
}

fn count(leaks: &[Leak], cat: &str) -> usize {
    leaks.iter().filter(|l| l.category == cat).count()
}

#[test]
fn finds_exactly_the_expected_leaks() {
    let leaks = run_scan_json();

    // Exact per-category counts.
    assert_eq!(count(&leaks, "time"), 5, "TIME leaks: {leaks:#?}");
    // 10 RANDOM: rand::random, thread_rng (bare/rand-qualified), gen_range, gen,
    // fastrand, Uuid::new_v4, Uuid::now_v7, SmallRng::from_entropy, OsRng (receiver),
    // OsRng (value/path). A foreign `my_utils::thread_rng()` is deliberately NOT one.
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
    let bin = env!("CARGO_BIN_EXE_navian-dst");
    Command::new(bin)
        .arg("scan")
        .arg(fixtures_root().join(dir))
        .args(extra)
        .output()
        .expect("failed to run dst binary")
}

#[test]
fn json_items_carry_full_schema() {
    // Every item stably includes rule_id + confidence (plus the fields already
    // asserted elsewhere: category/file/line/col/function/snippet).
    let leaks = run_scan_json();
    assert!(!leaks.is_empty());
    for l in &leaks {
        assert!(l.rule_id.starts_with("DST-"), "bad rule_id: {}", l.rule_id);
        assert!(
            ["high", "medium", "advisory"].contains(&l.confidence.as_str()),
            "bad confidence: {}",
            l.confidence
        );
    }
    // The high-signal std sources are HIGH-confidence.
    let conf_of = |needle: &str| {
        leaks
            .iter()
            .find(|l| l.snippet.contains(needle))
            .map(|l| l.confidence.as_str())
    };
    assert_eq!(conf_of("SystemTime::now"), Some("high"));
    assert_eq!(conf_of("thread_rng"), Some("high"));
    // chrono is MEDIUM, tokio::spawn is ADVISORY.
    assert_eq!(conf_of("Utc::now"), Some("medium"));
    assert_eq!(conf_of("tokio::spawn"), Some("advisory"));
}

// ── Expanded catalog (new sources live in fixtures/expanded) ───────────────

#[test]
fn expanded_catalog_detected_at_expected_tiers() {
    let leaks = scan_json_dir("expanded");

    // Exact per-category counts for the new fixtures.
    assert_eq!(count(&leaks, "time"), 2, "{leaks:#?}"); // OffsetDateTime, tokio Instant
    assert_eq!(count(&leaks, "random"), 1, "{leaks:#?}"); // getrandom
    assert_eq!(count(&leaks, "env"), 4, "{leaks:#?}"); // var, args, id, Command
    assert_eq!(count(&leaks, "filesystem"), 3, "{leaks:#?}"); // read_dir, glob, tempfile
    assert_eq!(count(&leaks, "iteration"), 2, "{leaks:#?}"); // HashMap for, HashSet .iter
    assert_eq!(count(&leaks, "atomic"), 2, "{leaks:#?}"); // two Ordering::Relaxed
    assert_eq!(count(&leaks, "concurrency"), 1, "{leaks:#?}"); // rayon par_iter
    assert_eq!(leaks.len(), 15, "total: {leaks:#?}");

    // Confidence distribution: exactly one HIGH (getrandom).
    let conf = |c: &str| leaks.iter().filter(|l| l.confidence == c).count();
    assert_eq!(conf("high"), 1, "{leaks:#?}");
    assert_eq!(conf("medium"), 9, "{leaks:#?}");
    assert_eq!(conf("advisory"), 5, "{leaks:#?}");

    // Spot-check specific rule IDs and their tiers.
    let rule_conf = |needle: &str| {
        leaks
            .iter()
            .find(|l| l.snippet.contains(needle))
            .map(|l| (l.rule_id.as_str(), l.confidence.as_str()))
    };
    assert_eq!(rule_conf("getrandom"), Some(("DST-RAND-005", "high")));
    assert_eq!(rule_conf("env::var"), Some(("DST-ENV-001", "medium")));
    assert_eq!(rule_conf("read_dir"), Some(("DST-FS-001", "medium")));
    assert_eq!(rule_conf("Ordering::Relaxed"), Some(("DST-ATOMIC-001", "advisory")));
    assert_eq!(rule_conf("par_iter"), Some(("DST-CONC-003", "advisory")));
    assert!(leaks
        .iter()
        .any(|l| l.rule_id == "DST-ITER-001" && l.confidence == "advisory"));
    assert!(leaks
        .iter()
        .any(|l| l.rule_id == "DST-TIME-004" && l.confidence == "medium"));
    assert!(leaks
        .iter()
        .any(|l| l.rule_id == "DST-TIME-005" && l.confidence == "medium"));
}

#[test]
fn expanded_decoys_are_not_flagged() {
    // The `decoys` functions across the expanded fixtures must never fire.
    let leaks = scan_json_dir("expanded");
    let in_decoy: Vec<_> = leaks
        .iter()
        .filter(|l| l.function.as_deref() == Some("decoys"))
        .collect();
    assert!(in_decoy.is_empty(), "expanded decoys flagged: {in_decoy:#?}");
    // No BTreeMap/Vec iteration got flagged (only Hash* containers).
    assert!(
        !leaks.iter().any(|l| l.snippet.contains("t.keys()")
            || l.snippet.contains("v.iter()")),
        "flagged a deterministic Vec/BTreeMap iteration"
    );
}

// ── Zero-false-positive CI gate as the catalog grows ───────────────────────

#[test]
fn deny_stays_zero_fp_on_high_source_decoys() {
    // The decoys_hi fixture holds shadowed/foreign look-alikes of every
    // high-confidence source (local `struct SystemTime`, `fn thread_rng`,
    // `fn getrandom`, `RngKind::OsRng`, `mock::TcpStream::pair`, …). A default
    // `--deny` (high threshold) must exit ZERO on it.
    let out = run_scan("decoys_hi", &["--deny"]);
    assert!(
        out.status.success(),
        "--deny must stay zero-FP on high-source decoys: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn high_source_decoys_emit_no_high_finding() {
    // Stronger than the exit code: NOT ONE finding in the gate decoys may be
    // high-confidence — that is the invariant the shadow/qualifier guards keep.
    let leaks = scan_json_dir("decoys_hi");
    let high: Vec<_> = leaks.iter().filter(|l| l.confidence == "high").collect();
    assert!(high.is_empty(), "high findings in gate decoys: {high:#?}");
}

// ── FIX 2: unreadable/unparseable input must not pass the gate green ────────

#[test]
fn unparseable_without_deny_exits_zero() {
    // A parse failure is reported but is not, by itself, a gate failure.
    let out = run_scan("unparseable", &[]);
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn unparseable_under_deny_exits_two() {
    // Under a gate, a file that could not be parsed means the tree cannot be
    // certified clean → exit 2 (cannot certify), never a green 0.
    for level in [
        &["--deny"][..],
        &["--deny-level", "high"][..],
        &["--deny-level", "advisory"][..],
    ] {
        let out = run_scan("unparseable", level);
        assert_eq!(
            out.status.code(),
            Some(2),
            "unparseable under {level:?} must exit 2, got {:?}",
            out.status.code()
        );
    }
}

#[test]
fn deny_fails_on_hard_leak() {
    // `--deny` (high threshold) on a hard leak exits non-zero.
    let out = run_scan("hard_only", &["--deny"]);
    assert!(
        !out.status.success(),
        "--deny must fail on a hard (time) leak"
    );
}

// ── Exit-code contract: 0 clean/no-gate, 1 gated finding, 2 usage error ────

#[test]
fn exit_code_zero_without_deny_even_with_findings() {
    // Findings present but no gate → exit 0.
    let out = run_scan("expanded", &[]);
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn exit_code_one_when_gate_bites() {
    // `expanded` contains a HIGH getrandom leak → default `--deny` exits 1.
    let out = run_scan("expanded", &["--deny"]);
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn deny_default_ignores_medium_and_advisory() {
    // env.rs is MEDIUM-only: default `--deny` (high) must NOT fail → exit 0.
    let out = run_scan("expanded/env.rs", &["--deny"]);
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn deny_level_medium_fails_on_medium() {
    let out = run_scan("expanded/env.rs", &["--deny-level", "medium"]);
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn deny_level_medium_passes_on_advisory_only() {
    // atomics.rs is ADVISORY-only → a `medium` threshold must PASS.
    let out = run_scan("expanded/atomics.rs", &["--deny-level", "medium"]);
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn deny_level_advisory_fails_on_advisory() {
    let out = run_scan("expanded/atomics.rs", &["--deny-level", "advisory"]);
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn exit_code_two_on_bad_deny_level() {
    let out = run_scan("expanded", &["--deny-level", "bogus"]);
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn exit_code_two_on_missing_path() {
    let out = run_scan("this/does/not/exist", &[]);
    assert_eq!(out.status.code(), Some(2));
}
