// Fixture: exactly ONE hard leak (TIME) and nothing else. Used to prove that
// `--deny` exits non-zero on a hard leak (the CI gate still bites).

use std::time::SystemTime;

pub fn stamp() -> std::time::SystemTime {
    SystemTime::now()
}
