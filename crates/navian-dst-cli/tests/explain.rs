//! Integration tests for the `explain` subcommand and the enriched
//! `scan --json` agent worklist (Phase 1 Batch C).
//!
//! `explain` turns a rule id into an actionable fix recipe (why / seam /
//! before → after / autofixable), and `scan --json` gains three additive keys
//! per finding (`suggested_seam`, `autofixable`, `fix_hint`) WITHOUT changing
//! the stable top-level array or any existing item key.

use std::path::PathBuf;
use std::process::{Command, Output};

use serde_json::Value;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_navian-dst")
}

fn run(args: &[&str]) -> Output {
    Command::new(bin())
        .args(args)
        .output()
        .expect("failed to run navian-dst binary")
}

fn fixtures_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

// ── explain <RULE-ID> (human) ────────────────────────────────────────────────

#[test]
fn explain_known_rule_prints_recipe_and_exits_zero() {
    let out = run(&["explain", "DST-TIME-001"]);
    assert_eq!(out.status.code(), Some(0), "explain <known> must exit 0");
    let stdout = String::from_utf8_lossy(&out.stdout);

    // The recipe must carry every section: id, why, seam, before → after.
    assert!(stdout.contains("DST-TIME-001"), "missing rule id: {stdout}");
    assert!(
        stdout.contains("Why it is nondeterministic:"),
        "missing why section: {stdout}"
    );
    assert!(
        stdout.contains("Injectable seam:") && stdout.contains("navian_dst::Time"),
        "missing seam section: {stdout}"
    );
    assert!(
        stdout.contains("Before → after:")
            && stdout.contains("SystemTime::now")
            && stdout.contains("now_ms()"),
        "missing before/after: {stdout}"
    );
    // A migrate-handled rule advertises the auto-fix honestly.
    assert!(
        stdout.contains("Auto-fixable: YES"),
        "DST-TIME-001 must be auto-fixable: {stdout}"
    );
}

#[test]
fn explain_manual_rule_reports_not_autofixable() {
    // A concurrency rule is NOT part of the migrate time family.
    let out = run(&["explain", "DST-CONC-002"]);
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Auto-fixable: NO"),
        "DST-CONC-002 must not be auto-fixable: {stdout}"
    );
    // It still names a real seam (the Executor / SimScheduler).
    assert!(
        stdout.contains("navian_dst::Executor") || stdout.contains("SimScheduler"),
        "missing executor seam guidance: {stdout}"
    );
}

// ── explain --list / bare explain ────────────────────────────────────────────

#[test]
fn explain_list_shows_every_rule_id() {
    let out = run(&["explain", "--list"]);
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout);

    // A representative id from each category family must appear.
    for id in [
        "DST-TIME-001",
        "DST-RAND-001",
        "DST-NET-001",
        "DST-CONC-001",
        "DST-ITER-001",
        "DST-ENV-001",
        "DST-FS-001",
        "DST-ATOMIC-001",
    ] {
        assert!(stdout.contains(id), "list is missing {id}: {stdout}");
    }
    assert!(
        stdout.contains("Determinism rules (29)"),
        "expected the full catalog count: {stdout}"
    );
}

#[test]
fn bare_explain_lists_like_list_flag() {
    let list = run(&["explain", "--list"]);
    let bare = run(&["explain"]);
    assert_eq!(bare.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&bare.stdout),
        String::from_utf8_lossy(&list.stdout),
        "bare `explain` must list every rule like `explain --list`"
    );
}

// ── explain unknown id → exit 2 ──────────────────────────────────────────────

#[test]
fn explain_unknown_rule_exits_two_with_help() {
    let out = run(&["explain", "DST-NOPE-999"]);
    assert_eq!(out.status.code(), Some(2), "unknown id must exit 2");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("unknown rule id") && stderr.contains("--list"),
        "unknown-id error must point at --list: {stderr}"
    );
}

// ── explain <id> --format json ───────────────────────────────────────────────

#[test]
fn explain_json_has_documented_keys() {
    let out = run(&["explain", "DST-TIME-001", "--format", "json"]);
    assert_eq!(out.status.code(), Some(0));
    let doc: Value = serde_json::from_slice(&out.stdout).expect("explain --format json must be valid JSON");

    // The documented agent schema:
    // {rule_id, category, confidence, why, suggested_seam, autofixable, before, after}.
    assert_eq!(doc["rule_id"], "DST-TIME-001");
    assert_eq!(doc["category"], "time");
    assert_eq!(doc["confidence"], "high");
    assert_eq!(doc["autofixable"], true);
    for key in ["why", "suggested_seam", "before", "after"] {
        assert!(
            doc[key].as_str().is_some_and(|s| !s.is_empty()),
            "explain json key `{key}` must be a non-empty string: {doc}"
        );
    }
}

#[test]
fn explain_list_json_is_array_of_all_rules() {
    let out = run(&["explain", "--list", "--format", "json"]);
    assert_eq!(out.status.code(), Some(0));
    let arr: Value = serde_json::from_slice(&out.stdout).expect("list json must be valid JSON");
    let items = arr.as_array().expect("list json must be an array");
    assert_eq!(items.len(), 29, "must list every catalog rule");
    for it in items {
        assert!(it["rule_id"].as_str().unwrap().starts_with("DST-"));
        assert!(it["description"].is_string());
        assert!(it["autofixable"].is_boolean());
    }
}

// ── scan --json enrichment (additive, back-compat) ───────────────────────────

fn scan_json(dir: &str) -> Vec<Value> {
    let out = Command::new(bin())
        .arg("scan")
        .arg(fixtures_root().join(dir))
        .arg("--json")
        .output()
        .expect("failed to run scan");
    assert!(out.status.success(), "scan exited non-zero");
    let v: Value = serde_json::from_slice(&out.stdout).expect("scan --json must be valid JSON");
    v.as_array().expect("scan --json must stay a top-level array").clone()
}

#[test]
fn scan_json_findings_carry_agent_worklist_keys() {
    let findings = scan_json("leaky");
    assert!(!findings.is_empty());

    for f in &findings {
        // Existing keys are all still present and in place.
        for key in [
            "rule_id", "confidence", "category", "file", "line", "col", "function", "snippet",
        ] {
            assert!(f.get(key).is_some(), "missing legacy key `{key}`: {f}");
        }
        // New additive keys.
        assert!(
            f["suggested_seam"].as_str().is_some_and(|s| !s.is_empty()),
            "missing/empty suggested_seam: {f}"
        );
        assert!(f["autofixable"].is_boolean(), "missing autofixable: {f}");
        assert!(
            f["fix_hint"].as_str().is_some_and(|s| s.contains("navian-dst")),
            "missing/weak fix_hint: {f}"
        );
    }
}

#[test]
fn scan_json_autofixable_true_for_time_family_false_for_concurrency() {
    let findings = scan_json("leaky");
    let by_rule = |id: &str| {
        findings
            .iter()
            .find(|f| f["rule_id"] == id)
            .unwrap_or_else(|| panic!("no finding for {id}"))
            .clone()
    };

    // The time-family rule the migrate codemod handles → autofixable = true, and
    // its hint points at migrate.
    let t = by_rule("DST-TIME-001");
    assert_eq!(t["autofixable"], true, "DST-TIME-001 must be autofixable");
    assert!(
        t["fix_hint"].as_str().unwrap().contains("migrate"),
        "autofixable hint must mention migrate: {t}"
    );

    // A concurrency finding → autofixable = false, hint routes to explain.
    let c = findings
        .iter()
        .find(|f| f["category"] == "concurrency")
        .expect("expected a concurrency finding");
    assert_eq!(c["autofixable"], false, "concurrency must NOT be autofixable");
    assert!(
        c["fix_hint"].as_str().unwrap().contains("explain"),
        "manual hint must route to explain: {c}"
    );
}

#[test]
fn scan_json_iteration_rule_is_not_autofixable() {
    // The expanded fixture holds HashMap/HashSet iteration (DST-ITER-001).
    let findings = scan_json("expanded");
    let iter = findings
        .iter()
        .find(|f| f["rule_id"] == "DST-ITER-001")
        .expect("expected an iteration finding");
    assert_eq!(iter["autofixable"], false, "iteration must NOT be autofixable");
}
