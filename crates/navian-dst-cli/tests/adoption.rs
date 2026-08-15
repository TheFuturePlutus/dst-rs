//! Integration tests for the Phase-1 Batch-B adoption features: the `--format`
//! output modes (SARIF + GitHub), baseline suppression, and `init` scaffolding.
//!
//! Every test drives the COMPILED `navian-dst` binary end-to-end against files
//! written into a private scratch dir, so it exercises the real CLI wiring
//! (argument parsing, exit codes, stdout) — not just library internals.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

// ── Scratch dir (unique, removed on drop) ──────────────────────────────────

struct Scratch {
    root: PathBuf,
}
impl Scratch {
    fn new(tag: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("dst-adopt-{tag}-{nanos}"));
        std::fs::create_dir_all(&root).expect("create scratch dir");
        Scratch { root }
    }
    fn write(&self, rel: &str, contents: &str) -> PathBuf {
        let path = self.root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, contents).unwrap();
        path
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_navian-dst")
}

/// Run `navian-dst <args...>`; returns (exit code, stdout, stderr).
fn run(args: &[&str]) -> (Option<i32>, String, String) {
    let out = Command::new(bin())
        .args(args)
        .output()
        .expect("failed to run dst binary");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Run in a specific working directory (for `init`).
fn run_in(dir: &Path, args: &[&str]) -> (Option<i32>, String, String) {
    let out = Command::new(bin())
        .args(args)
        .current_dir(dir)
        .output()
        .expect("failed to run dst binary");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

// A file with exactly one HIGH time leak inside `fn handler`.
const ONE_HIGH_LEAK: &str = "fn handler() { let _ = std::time::SystemTime::now(); }\n";

// ── SARIF ──────────────────────────────────────────────────────────────────

#[test]
fn sarif_has_valid_structure() {
    let s = Scratch::new("sarif-struct");
    s.write("src/lib.rs", ONE_HIGH_LEAK);
    let (code, stdout, _) = run(&["scan", "--format", "sarif", s.root.to_str().unwrap()]);
    assert_eq!(code, Some(0), "sarif scan without --deny must exit 0");

    let doc: Value = serde_json::from_str(&stdout).expect("SARIF must be valid JSON");
    assert_eq!(doc["version"], "2.1.0");
    assert!(doc["$schema"].is_string(), "missing $schema");

    let driver = &doc["runs"][0]["tool"]["driver"];
    assert_eq!(driver["name"], "navian-dst");
    assert!(driver["informationUri"].is_string());
    assert!(driver["version"].is_string());

    // The whole catalog is present as reportingDescriptors.
    let rules = driver["rules"].as_array().expect("rules must be an array");
    assert!(rules.len() >= 25, "expected the full catalog, got {}", rules.len());
    for r in rules {
        assert!(r["id"].as_str().unwrap().starts_with("DST-"));
        assert!(r["name"].is_string());
        assert!(r["shortDescription"]["text"].is_string());
        let level = r["defaultConfiguration"]["level"].as_str().unwrap();
        assert!(
            ["error", "warning", "note"].contains(&level),
            "bad rule level {level}"
        );
    }

    // Exactly one result, fully formed.
    let results = doc["runs"][0]["results"].as_array().unwrap();
    assert_eq!(results.len(), 1, "one leak → one result");
    let res = &results[0];
    assert_eq!(res["ruleId"], "DST-TIME-001");
    assert_eq!(res["level"], "error"); // High → error
    assert!(res["message"]["text"].is_string());
    let region = &res["locations"][0]["physicalLocation"]["region"];
    assert_eq!(region["startLine"], 1);
    assert!(region["startColumn"].as_u64().unwrap() >= 1);
    assert!(
        res["locations"][0]["physicalLocation"]["artifactLocation"]["uri"].is_string()
    );
    assert!(
        res["partialFingerprints"]["dstFingerprintV1"].is_string(),
        "each result must carry dstFingerprintV1"
    );
}

#[test]
fn sarif_fingerprint_is_line_independent() {
    // The SAME logical finding, once at the top of a file and once shifted down
    // by inserted blank lines, must produce an IDENTICAL dstFingerprintV1.
    let a = Scratch::new("sarif-fp-a");
    a.write("src/lib.rs", ONE_HIGH_LEAK);
    let b = Scratch::new("sarif-fp-b");
    b.write("src/lib.rs", &format!("\n\n\n\n{ONE_HIGH_LEAK}"));

    let fp = |s: &Scratch| -> String {
        let (_, stdout, _) = run(&["scan", "--format", "sarif", s.root.to_str().unwrap()]);
        let doc: Value = serde_json::from_str(&stdout).unwrap();
        doc["runs"][0]["results"][0]["partialFingerprints"]["dstFingerprintV1"]
            .as_str()
            .unwrap()
            .to_string()
    };

    assert_eq!(
        fp(&a),
        fp(&b),
        "fingerprint must survive the finding moving down the file"
    );
}

// ── GitHub workflow commands ────────────────────────────────────────────────

#[test]
fn github_format_emits_one_annotation_per_finding() {
    let s = Scratch::new("gh");
    s.write("src/lib.rs", ONE_HIGH_LEAK);
    let (code, stdout, _) = run(&["scan", "--format", "github", s.root.to_str().unwrap()]);
    assert_eq!(code, Some(0));

    let lines: Vec<&str> = stdout.lines().filter(|l| l.starts_with("::")).collect();
    assert_eq!(lines.len(), 1, "one leak → one annotation: {stdout}");
    let line = lines[0];
    assert!(line.starts_with("::error "), "High → ::error: {line}");
    assert!(line.contains("line=1,"), "must carry the line: {line}");
    assert!(line.contains(",col="), "must carry the column: {line}");
    assert!(line.contains("title=DST-TIME-001::"), "title is the rule id: {line}");
    assert!(line.contains("src/lib.rs"), "must name the file: {line}");
}

// ── `--json` back-compat alias ──────────────────────────────────────────────

#[test]
fn json_flag_is_alias_for_format_json() {
    let s = Scratch::new("json-alias");
    s.write("src/lib.rs", ONE_HIGH_LEAK);
    let (_, a, _) = run(&["scan", "--json", s.root.to_str().unwrap()]);
    let (_, b, _) = run(&["scan", "--format", "json", s.root.to_str().unwrap()]);
    assert_eq!(a, b, "--json must be identical to --format json");
    // And it is a JSON array of leaks.
    let arr: Value = serde_json::from_str(&a).unwrap();
    assert!(arr.is_array());
}

// ── Baseline / suppression ──────────────────────────────────────────────────

#[test]
fn baseline_then_deny_suppresses_all() {
    let s = Scratch::new("baseline-all");
    s.write("src/lib.rs", ONE_HIGH_LEAK);
    let bfile = s.root.join("navian-dst-baseline.json");

    // Baseline the existing finding.
    let (code, stdout, _) = run(&[
        "baseline",
        s.root.to_str().unwrap(),
        "--out",
        bfile.to_str().unwrap(),
    ]);
    assert_eq!(code, Some(0), "baseline must succeed");
    assert!(stdout.contains("Baselined 1 finding"), "stdout: {stdout}");
    assert!(bfile.exists(), "baseline file must be written");

    // With the baseline, the gate sees NO new findings → exit 0.
    let (code, _, stderr) = run(&[
        "scan",
        "--deny",
        "--baseline",
        bfile.to_str().unwrap(),
        s.root.to_str().unwrap(),
    ]);
    assert_eq!(code, Some(0), "all findings baselined → gate passes");
    assert!(
        stderr.contains("suppressed 1 finding"),
        "must report suppression: {stderr}"
    );
}

#[test]
fn baseline_gate_fails_only_on_new_finding() {
    let s = Scratch::new("baseline-new");
    s.write("src/lib.rs", ONE_HIGH_LEAK);
    let bfile = s.root.join("navian-dst-baseline.json");
    let (code, _, _) = run(&[
        "baseline",
        s.root.to_str().unwrap(),
        "--out",
        bfile.to_str().unwrap(),
    ]);
    assert_eq!(code, Some(0));

    // Introduce a NEW high leak in a different file / function.
    s.write(
        "src/net.rs",
        "fn connect() { let _ = std::net::TcpStream::connect(\"x\"); }\n",
    );

    let (code, _, stderr) = run(&[
        "scan",
        "--deny",
        "--baseline",
        bfile.to_str().unwrap(),
        s.root.to_str().unwrap(),
    ]);
    assert_eq!(code, Some(1), "a NEW high finding must fail the gate");
    // The old one is still suppressed (only the new one survives).
    assert!(
        stderr.contains("suppressed 1 finding"),
        "the baselined finding must still be suppressed: {stderr}"
    );
}

#[test]
fn baseline_does_not_suppress_a_copy_pasted_leak_in_another_file() {
    // The fingerprint excludes file+line, so a naive set-membership suppression
    // would wave through an identical leak copy-pasted into a NEW file. Count-
    // bounded suppression must catch it: baseline has ONE occurrence, the tree
    // now has TWO, so one is genuinely new and the gate must fire.
    let s = Scratch::new("baseline-copypaste");
    s.write("src/a.rs", ONE_HIGH_LEAK);
    let bfile = s.root.join("navian-dst-baseline.json");
    let (code, _, _) = run(&[
        "baseline",
        s.root.to_str().unwrap(),
        "--out",
        bfile.to_str().unwrap(),
    ]);
    assert_eq!(code, Some(0));

    // Same rule + function name (`handler`) + snippet, different file.
    s.write("src/b.rs", ONE_HIGH_LEAK);

    let (code, _, stderr) = run(&[
        "scan",
        "--deny",
        "--baseline",
        bfile.to_str().unwrap(),
        s.root.to_str().unwrap(),
    ]);
    assert_eq!(
        code,
        Some(1),
        "an identical leak copy-pasted to a new file is NEW and must fail the gate"
    );
    assert!(
        stderr.contains("suppressed 1 finding"),
        "only the ONE baselined occurrence may be suppressed: {stderr}"
    );
}

#[test]
fn baseline_does_not_suppress_an_added_duplicate_in_the_same_function() {
    // Same-file multiplicity: baseline one call, then add a SECOND identical
    // call in the same function. The added occurrence exceeds the baseline
    // count and must survive to the gate.
    let s = Scratch::new("baseline-dup");
    s.write("src/lib.rs", ONE_HIGH_LEAK);
    let bfile = s.root.join("navian-dst-baseline.json");
    let (code, _, _) = run(&[
        "baseline",
        s.root.to_str().unwrap(),
        "--out",
        bfile.to_str().unwrap(),
    ]);
    assert_eq!(code, Some(0));

    s.write(
        "src/lib.rs",
        "fn handler() { let _ = std::time::SystemTime::now(); let _ = std::time::SystemTime::now(); }\n",
    );

    let (code, _, stderr) = run(&[
        "scan",
        "--deny",
        "--baseline",
        bfile.to_str().unwrap(),
        s.root.to_str().unwrap(),
    ]);
    assert_eq!(code, Some(1), "an added identical leak must fail the gate");
    assert!(stderr.contains("suppressed 1 finding"), "{stderr}");
}

#[test]
fn missing_baseline_file_is_usage_error() {
    let s = Scratch::new("baseline-missing");
    s.write("src/lib.rs", ONE_HIGH_LEAK);
    let (code, _, stderr) = run(&[
        "scan",
        "--deny",
        "--baseline",
        s.root.join("nope.json").to_str().unwrap(),
        s.root.to_str().unwrap(),
    ]);
    assert_eq!(code, Some(2), "missing baseline → exit 2");
    assert!(stderr.contains("baseline"), "stderr should explain: {stderr}");
}

#[test]
fn invalid_baseline_file_is_usage_error() {
    let s = Scratch::new("baseline-invalid");
    s.write("src/lib.rs", ONE_HIGH_LEAK);
    let bad = s.write("bad-baseline.json", "{ this is not valid json");
    let (code, _, _) = run(&[
        "scan",
        "--baseline",
        bad.to_str().unwrap(),
        s.root.to_str().unwrap(),
    ]);
    assert_eq!(code, Some(2), "invalid baseline → exit 2, even without --deny");
}

#[test]
fn baseline_missing_scan_path_is_usage_error() {
    let (code, _, _) = run(&["baseline", "this/does/not/exist", "--out", "x.json"]);
    assert_eq!(code, Some(2));
}

// ── init scaffolding ────────────────────────────────────────────────────────

#[test]
fn init_creates_both_files() {
    let s = Scratch::new("init-create");
    let (code, stdout, _) = run_in(&s.root, &["init"]);
    assert_eq!(code, Some(0));
    let workflow = s.root.join(".github/workflows/navian-dst.yml");
    let config = s.root.join("navian-dst.toml");
    assert!(workflow.exists(), "workflow must be created");
    assert!(config.exists(), "config must be created");
    assert!(stdout.contains("create"), "must report creation: {stdout}");

    // The scaffolded workflow really runs the gate; the config is labeled a stub.
    let wf = std::fs::read_to_string(&workflow).unwrap();
    assert!(wf.contains("navian-dst scan --deny"), "workflow must gate");
    let cfg = std::fs::read_to_string(&config).unwrap();
    assert!(
        cfg.to_lowercase().contains("template") || cfg.to_lowercase().contains("stub"),
        "config must honestly declare itself a template"
    );
}

#[test]
fn init_second_run_skips_existing_files() {
    let s = Scratch::new("init-skip");
    let (code, _, _) = run_in(&s.root, &["init"]);
    assert_eq!(code, Some(0));

    // Mark the files with a sentinel; a second run must NOT overwrite them.
    let workflow = s.root.join(".github/workflows/navian-dst.yml");
    let config = s.root.join("navian-dst.toml");
    std::fs::write(&workflow, "SENTINEL\n").unwrap();
    std::fs::write(&config, "SENTINEL\n").unwrap();

    let (code, stdout, _) = run_in(&s.root, &["init"]);
    assert_eq!(code, Some(0));
    assert_eq!(stdout.matches("skip").count(), 2, "both files skipped: {stdout}");
    assert_eq!(std::fs::read_to_string(&workflow).unwrap(), "SENTINEL\n");
    assert_eq!(std::fs::read_to_string(&config).unwrap(), "SENTINEL\n");
}
