//! Baseline / suppression store.
//!
//! A baseline records the [`fingerprint`] of every finding currently in a tree
//! so a team can adopt the `scan --deny` gate on a codebase that is not yet
//! clean: existing findings are written to a baseline file once, and thereafter
//! `scan --baseline <file>` suppresses anything whose fingerprint is in the
//! store — so the gate fails only on NEW findings.
//!
//! Suppression keys on the [`fingerprint`] alone (rule + enclosing function +
//! normalized snippet, line-independent). `file` and `function` are stored per
//! entry only for human readability of the baseline file — they do not affect
//! matching, which is why a finding that moves within or across files stays
//! suppressed.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::scanner::{fingerprint, ScanReport};

/// Current on-disk baseline schema version.
pub const BASELINE_VERSION: u32 = 1;

/// One suppressed finding in a baseline file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BaselineEntry {
    /// Stable rule id, e.g. `DST-TIME-001` (readability / auditing).
    pub rule_id: String,
    /// The shared, line-independent fingerprint — the ONLY field matched on.
    pub fingerprint: String,
    /// File the finding was in when baselined (readability only).
    pub file: String,
    /// Enclosing function, if any (readability only).
    pub function: Option<String>,
}

/// A baseline file: a schema version plus the sorted set of suppressed findings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Baseline {
    /// On-disk schema version (see [`BASELINE_VERSION`]).
    pub version: u32,
    /// The suppressed findings, sorted for a clean diff.
    pub findings: Vec<BaselineEntry>,
}

impl Baseline {
    /// Build a baseline from a scan report: fingerprint every leak and sort for
    /// a stable diff.
    ///
    /// One entry is kept **per occurrence** — duplicates are intentionally NOT
    /// collapsed. The number of entries sharing a fingerprint IS the
    /// multiplicity, which suppression consumes as a count (see
    /// [`fingerprint_counts`](Baseline::fingerprint_counts)). Collapsing them
    /// would let a newly-added identical leak be suppressed for free.
    pub fn from_report(report: &ScanReport) -> Baseline {
        let mut findings: Vec<BaselineEntry> = report
            .leaks
            .iter()
            .map(|l| BaselineEntry {
                rule_id: l.rule_id.to_string(),
                fingerprint: fingerprint(l),
                file: l.file.clone(),
                function: l.function.clone(),
            })
            .collect();
        // Deterministic order so the baseline diffs cleanly across runs.
        findings.sort_by(|a, b| {
            a.file
                .cmp(&b.file)
                .then(a.rule_id.cmp(&b.rule_id))
                .then(a.function.cmp(&b.function))
                .then(a.fingerprint.cmp(&b.fingerprint))
        });
        Baseline {
            version: BASELINE_VERSION,
            findings,
        }
    }

    /// The multiset of fingerprints to suppress: fingerprint → occurrence count.
    ///
    /// Suppression is count-bounded — it removes at MOST this many occurrences
    /// of each fingerprint from a later scan. So a finding that merely *moved*
    /// (same count, different line/file) stays suppressed, but an *added*
    /// occurrence (count now exceeds the baseline) is a NEW finding and survives
    /// to the gate. This is what closes the copy-paste / duplicate-leak hole:
    /// the fingerprint deliberately excludes file+line, so without counting, a
    /// single baselined entry would wave through unlimited identical leaks.
    pub fn fingerprint_counts(&self) -> HashMap<String, usize> {
        let mut counts = HashMap::new();
        for e in &self.findings {
            *counts.entry(e.fingerprint.clone()).or_insert(0) += 1;
        }
        counts
    }

    /// Load a baseline from disk. A missing or malformed file is an error the
    /// caller MUST surface as a usage error (exit 2) — never a silent empty
    /// baseline that would let the gate certify a tree it did not really check.
    pub fn load(path: &Path) -> Result<Baseline, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("could not read baseline {}: {e}", path.display()))?;
        let baseline: Baseline = serde_json::from_str(&text)
            .map_err(|e| format!("invalid baseline {}: {e}", path.display()))?;
        // A version-skewed baseline was written by a different fingerprint
        // algorithm; matching it against today's would silently mis-suppress.
        // Surface it as the same usage error (exit 2) as a missing/corrupt file
        // rather than let a stale file quietly change the gate outcome.
        if baseline.version != BASELINE_VERSION {
            return Err(format!(
                "unsupported baseline version {} (expected {}) in {}",
                baseline.version,
                BASELINE_VERSION,
                path.display()
            ));
        }
        Ok(baseline)
    }

    /// Pretty, newline-terminated JSON for writing to disk.
    pub fn to_pretty_json(&self) -> String {
        let mut s = serde_json::to_string_pretty(self).unwrap_or_default();
        s.push('\n');
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scanner::scan_source;

    fn report_of(src: &str) -> ScanReport {
        let mut report = ScanReport::default();
        scan_source("src/lib.rs", src, &mut report);
        report
    }

    #[test]
    fn from_report_fingerprints_and_sorts() {
        let report = report_of("fn f() { std::time::SystemTime::now(); }");
        let baseline = Baseline::from_report(&report);
        assert_eq!(baseline.version, BASELINE_VERSION);
        assert_eq!(baseline.findings.len(), 1);
        assert_eq!(baseline.findings[0].rule_id, "DST-TIME-001");
        assert_eq!(baseline.findings[0].fingerprint.len(), 16);
    }

    #[test]
    fn roundtrip_through_json_preserves_fingerprints() {
        let report = report_of(
            "fn f() { std::time::SystemTime::now(); rand::thread_rng(); }",
        );
        let baseline = Baseline::from_report(&report);
        let json = baseline.to_pretty_json();
        let back: Baseline = serde_json::from_str(&json).unwrap();
        assert_eq!(baseline.fingerprint_counts(), back.fingerprint_counts());
    }

    #[test]
    fn duplicate_leaks_are_counted_not_collapsed() {
        // Two identical leaks in one function → two entries, count 2. Collapsing
        // them would let a newly-added identical leak be suppressed for free.
        let report = report_of(
            "fn f() { std::time::SystemTime::now(); std::time::SystemTime::now(); }",
        );
        let baseline = Baseline::from_report(&report);
        assert_eq!(baseline.findings.len(), 2);
        let counts = baseline.fingerprint_counts();
        assert_eq!(counts.values().copied().max(), Some(2));
    }

    #[test]
    fn load_missing_file_is_error() {
        assert!(Baseline::load(Path::new("does/not/exist.json")).is_err());
    }

    #[test]
    fn load_rejects_a_version_skewed_baseline() {
        // A baseline written by a different fingerprint algorithm must be
        // rejected (surfaced as a usage error), not silently mis-matched.
        let dir = std::env::temp_dir().join(format!(
            "dst-baseline-ver-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bl.json");
        std::fs::write(&path, "{\"version\": 999, \"findings\": []}").unwrap();
        let err = Baseline::load(&path).unwrap_err();
        assert!(err.contains("unsupported baseline version 999"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
