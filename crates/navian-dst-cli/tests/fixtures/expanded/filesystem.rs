// Fixture: FILESYSTEM sources (all MEDIUM confidence).

pub fn list() {
    // LEAK: std::fs::read_dir — unordered directory listing (MEDIUM).
    let _ = std::fs::read_dir(".");
}

pub fn matches() {
    // LEAK: glob::glob — filesystem-ordered matches (MEDIUM).
    let _ = glob::glob("*.rs");
}

pub fn temp() {
    // LEAK: tempfile TempDir::new — random temp path (MEDIUM).
    let _ = tempfile::TempDir::new();
}

// ── Decoy: must NOT be flagged ──

pub fn decoys() {
    // A foreign-qualified `read_dir` — NOT the std one.
    let _ = mydb::read_dir();
}
