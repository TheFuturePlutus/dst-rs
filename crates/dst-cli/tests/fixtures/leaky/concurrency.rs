// Fixture: CONCURRENCY determinism leaks.

pub fn os_thread() {
    // LEAK: std::thread::spawn
    std::thread::spawn(|| {
        let _ = 1 + 1;
    });
}

pub fn async_task() {
    // LEAK: tokio::spawn
    tokio::spawn(async {
        let _ = 1 + 1;
    });
}

// ── Decoys: must NOT be flagged ──

pub fn spawn() {
    // A user-defined free function named `spawn`.
}

pub struct Builder;
impl Builder {
    // A method named `spawn` — NOT a leak (we only flag path-form spawn).
    pub fn spawn(&self) {}
}

pub fn decoys() {
    // Calling the user-defined free `spawn()` bare — NOT a leak.
    spawn();
    let b = Builder;
    b.spawn(); // method `.spawn()` — NOT a leak
    // A comment mentioning thread::spawn must not be flagged.
}
