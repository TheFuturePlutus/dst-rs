// Fixture: RANDOM determinism leaks.

use rand::Rng;

pub fn coin() -> u32 {
    // LEAK: rand::random
    rand::random::<u32>()
}

pub fn ranged() -> u32 {
    // LEAK: rand::thread_rng
    let mut rng = rand::thread_rng();
    // LEAK: .gen_range on a thread rng
    let a = rng.gen_range(0..10);
    // LEAK: .gen on a thread rng
    let b: u8 = rng.gen();
    a + b as u32
}

pub fn fast() -> u32 {
    // LEAK: fastrand::u32
    fastrand::u32(0..100)
}

pub fn ident() -> String {
    // LEAK: uuid::Uuid::new_v4
    uuid::Uuid::new_v4().to_string()
}

// ── Decoys: must NOT be flagged ──

pub struct Generator;
impl Generator {
    // A method named `generate` (not `gen`) — NOT a leak.
    pub fn generate(&self) -> u32 {
        7
    }
}

pub fn decoys() {
    let g = Generator;
    let _x = g.generate(); // NOT a leak
    let _gen = 5; // a variable named gen — NOT a call
}
