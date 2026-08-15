// Fixture: ITERATION-ORDER sources (ADVISORY confidence).

use std::collections::{BTreeMap, HashMap, HashSet};

pub fn map_for_loop() {
    let m: HashMap<u32, u32> = HashMap::new();
    // LEAK: iterating a HashMap has nondeterministic order (ADVISORY).
    for (k, v) in &m {
        let _ = (k, v);
    }
}

pub fn set_keys() {
    let mut s = HashSet::new();
    s.insert(1u32);
    // LEAK: iterating a HashSet (ADVISORY).
    let _ = s.iter().count();
}

// ── Decoys: must NOT be flagged ──

pub fn decoys() {
    // A Vec `.iter()` / for-loop is deterministic — NOT a leak.
    let v: Vec<u32> = Vec::new();
    for x in v.iter() {
        let _ = x;
    }
    // BTreeMap iteration is ordered/deterministic — NOT a leak.
    let t: BTreeMap<u32, u32> = BTreeMap::new();
    for k in t.keys() {
        let _ = k;
    }
}
