// Fixture: ENV / process sources (all MEDIUM confidence).

use std::process::Command;

pub fn read_var() {
    // LEAK: std::env::var — ambient process input (MEDIUM).
    let _ = std::env::var("HOME");
}

pub fn read_args() {
    // LEAK: std::env::args (MEDIUM).
    let _ = std::env::args().count();
}

pub fn pid() -> u32 {
    // LEAK: std::process::id (MEDIUM).
    std::process::id()
}

pub fn run() {
    // LEAK: std::process::Command (MEDIUM).
    let _ = Command::new("ls").status();
}

// ── Decoys: must NOT be flagged ──

pub fn args() -> u32 {
    // A user free function named `args`.
    0
}

pub fn decoys() {
    // Bare user `args()` — NOT a leak (no `env::` qualifier).
    let _ = args();
}
