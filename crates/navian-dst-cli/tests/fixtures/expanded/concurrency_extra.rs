// Fixture: rayon parallel iteration (ADVISORY concurrency).

pub fn parallel(v: &[u32]) {
    // LEAK: rayon par_iter — parallel iteration order/reduction (ADVISORY).
    v.par_iter().for_each(|_| {});
}
