//! `hello_retry` — retry-with-backoff against a fault-injectable network.
//!
//! The shop-window example for *correct* code under DST: a client that retries a
//! send over the seeded, flaky [`SimulatedNetwork`], looped over many seeds. The
//! invariant is the contract a correct retry must honor:
//!
//!   > if the link was clear on ANY attempt in the window, we return `Ok`.
//!
//! A correct bounded retry — one that keeps trying across the fault window instead
//! of giving up after one blip — satisfies this for every seed. Run it:
//!
//! ```text
//! cargo run --example hello_retry
//! ```

use dst_rs::{Delivery, FaultSchedule, SimulatedNetwork};

/// Outcome of one client call: whether it ultimately returned Ok, whether the link
/// was ever clear during the attempts, and how many attempts it took.
struct CallResult {
    returned_ok: bool,
    link_was_ever_clear: bool,
    attempts: u32,
}

/// A correct retry-with-backoff client. It retries up to `max_attempts` times over
/// the fault-injectable network; `Delayed`/`Delivered` are successes, `Failed`/
/// `Dropped` trigger a (simulated, cost-free) backoff and another attempt.
fn retrying_client(net: &SimulatedNetwork, max_attempts: u32) -> CallResult {
    let mut link_was_ever_clear = false;
    for attempt in 1..=max_attempts {
        let outcome = net.send(attempt);
        if !outcome.is_retryable() {
            link_was_ever_clear = true;
        }
        match outcome {
            Delivery::Delivered(_) | Delivery::Delayed { .. } => {
                return CallResult {
                    returned_ok: true,
                    link_was_ever_clear,
                    attempts: attempt,
                };
            }
            // Transient failure or a silent drop: back off and retry. Under the
            // deterministic scheduler a backoff sleep costs no wall-clock time; here
            // we simply advance to the next attempt (and the next schedule step).
            Delivery::Failed | Delivery::Dropped => continue,
        }
    }
    CallResult {
        returned_ok: false,
        link_was_ever_clear,
        attempts: max_attempts,
    }
}

fn main() {
    const SEEDS: u64 = 500;
    const STEPS: usize = 16;
    const DENSITY: u64 = 60; // a genuinely hostile link: ~60% of steps fault
                             // `max_attempts > STEPS` so the client can outlast the
                             // whole schedule; past the end the link is clear, so a
                             // correct retry is guaranteed to eventually succeed.
    let max_attempts = (STEPS as u32) + 4;

    let mut total_attempts: u64 = 0;
    let mut violations = 0u64;
    let mut worst_attempts = 0u32;

    for seed in 0..SEEDS {
        let net = SimulatedNetwork::new(FaultSchedule::generate(seed, STEPS, DENSITY));
        let r = retrying_client(&net, max_attempts);
        total_attempts += r.attempts as u64;
        worst_attempts = worst_attempts.max(r.attempts);

        // Invariant: link ever clear ⇒ returned Ok.
        if r.link_was_ever_clear && !r.returned_ok {
            eprintln!("INVARIANT VIOLATED at seed {seed}: link was clear but returned Err");
            violations += 1;
        }
    }

    println!("hello_retry — retry-with-backoff over a fault-injected network");
    println!("  seeds run:            {SEEDS}");
    println!("  link density:         {DENSITY}% faulty, {STEPS} steps/seed");
    println!("  max attempts/call:    {max_attempts}");
    println!("  total attempts:       {total_attempts}");
    println!("  worst-case attempts:  {worst_attempts}");
    println!(
        "  avg attempts/call:    {:.2}",
        total_attempts as f64 / SEEDS as f64
    );
    println!("  invariant violations: {violations}");

    assert_eq!(
        violations, 0,
        "a correct retry must hold the invariant on every seed"
    );
    println!("  RESULT: all {SEEDS} seeds held the invariant ✓");
}
