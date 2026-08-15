// Fixture: additional RANDOM source `getrandom` (HIGH confidence).

pub fn entropy() {
    let mut buf = [0u8; 16];
    // LEAK: getrandom::getrandom — raw OS entropy (HIGH).
    let _ = getrandom::getrandom(&mut buf);
}

// ── Decoy: must NOT be flagged ──

pub mod mycrate {
    pub fn getrandom(_b: &mut [u8]) -> u8 {
        0
    }
}

pub fn decoys() {
    let mut buf = [0u8; 4];
    // A user crate's own, foreign-qualified `getrandom` — NOT the real one.
    let _ = mycrate::getrandom(&mut buf);
}
