//! `Executor` trait — task spawning.
//!
//! Per ADR-0022. Production impl wraps `tokio::spawn` directly. Simulation
//! impl runs tasks single-threaded under harness control (Phase 0 ships the
//! production path; the full single-threaded scheduler lands in the phase
//! that needs cross-node deterministic execution, currently Phase 4 / 8).

use std::future::Future;
use std::sync::Mutex;
use std::time::Duration;

use tokio::task::JoinHandle;

use super::time::Time;

/// Marker for a future that did not complete within the given duration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimedOut;

impl std::fmt::Display for TimedOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("operation timed out")
    }
}

impl std::error::Error for TimedOut {}

/// Run `future` to completion or return [`TimedOut`] if `duration` elapses
/// first, measured by the supplied [`Time`] implementation.
///
/// Production: behaves like `tokio::time::timeout` (sleep is real). Simulation:
/// the timeout fires when the harness clock advances past `now + duration`,
/// making timeout-path tests deterministic (ADR-0022 §A3 + Step 3 §4.11
/// Option β — a free helper using `Time::sleep` rather than widening the
/// `Time` trait).
pub async fn timeout<T: Time + ?Sized, F: Future>(
    time: &T,
    duration: Duration,
    future: F,
) -> Result<F::Output, TimedOut> {
    tokio::select! {
        out = future => Ok(out),
        _ = time.sleep(duration) => Err(TimedOut),
    }
}

/// Task-spawning abstraction.
///
/// Engine code that needs to spawn a background task (replication stream,
/// ClickHouse syncer, reaper, etc.) goes through this trait. Production
/// builds use [`ProductionExecutor`] (real `tokio::spawn`); simulation
/// builds use [`SimulatedExecutor`] (records spawned tasks; full
/// scheduler lands in a later phase).
///
/// **Not dyn-safe.** The generic `spawn<F>` method precludes building a
/// vtable. Per ADR-0022 alternative #3, `Executor` is hot-path-only and
/// uses generics; cold paths that need dyn dispatch don't spawn tasks.
/// Holders of an `Executor` parameterize their own types with `E: Executor`.
pub trait Executor: Send + Sync + 'static {
    /// Handle to a spawned task (ADR-0066). Production/simulation use tokio's
    /// `JoinHandle`; a deterministic `SimScheduler`-backed executor uses its own
    /// scheduler-native handle. The abstraction has NO required surface — every
    /// production `spawn()` call site discards the handle — so this GAT is pure
    /// prep that keeps the existing executors byte-identical while unblocking a
    /// non-tokio implementation.
    type JoinHandle<O: Send + 'static>: Send;

    /// Spawn a future on the executor; returns a handle bound to the future's output.
    fn spawn<F>(&self, future: F) -> Self::JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static;
}

// ─────────────────────────────────────────────────────────────────────
// ProductionExecutor
// ─────────────────────────────────────────────────────────────────────

/// Production-mode `Executor` — direct `tokio::spawn`.
///
/// Zero-overhead: the spawn call compiles to the same code as a direct
/// `tokio::spawn(future)`.
#[derive(Debug, Default, Clone, Copy)]
pub struct ProductionExecutor;

impl Executor for ProductionExecutor {
    type JoinHandle<O: Send + 'static> = JoinHandle<O>;

    #[inline]
    fn spawn<F>(&self, future: F) -> Self::JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        tokio::spawn(future)
    }
}

// ─────────────────────────────────────────────────────────────────────
// SimulatedExecutor
// ─────────────────────────────────────────────────────────────────────

/// Simulation-mode `Executor` — Phase 0 uses real `tokio::spawn` while
/// recording spawned-task counts for property checks.
///
/// The full single-threaded deterministic scheduler (per
/// `pulse-architecture-v1.md` §10.5) lands in a later phase when
/// cross-task interleaving needs to be controlled by the harness.
/// Phase 0's tests run against the multi-threaded `tokio` runtime;
/// determinism in Phase 0 is provided by `Time` and `Random`, not by
/// scheduler control.
pub struct SimulatedExecutor {
    spawned: Mutex<u64>,
}

impl Default for SimulatedExecutor {
    fn default() -> Self {
        Self::new()
    }
}

impl SimulatedExecutor {
    pub fn new() -> Self {
        Self {
            spawned: Mutex::new(0),
        }
    }

    /// Total number of `spawn` calls observed.
    pub fn spawned_count(&self) -> u64 {
        *self
            .spawned
            .lock()
            .expect("SimulatedExecutor mutex poisoned")
    }
}

impl Executor for SimulatedExecutor {
    type JoinHandle<O: Send + 'static> = JoinHandle<O>;

    fn spawn<F>(&self, future: F) -> Self::JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        *self
            .spawned
            .lock()
            .expect("SimulatedExecutor mutex poisoned") += 1;
        tokio::spawn(future)
    }
}

// ─────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn production_executor_runs_task() {
        let e = ProductionExecutor;
        let handle = e.spawn(async { 42 });
        assert_eq!(handle.await.unwrap(), 42);
    }

    #[tokio::test]
    async fn simulated_executor_runs_task_and_counts() {
        let e = SimulatedExecutor::new();
        assert_eq!(e.spawned_count(), 0);
        let h1 = e.spawn(async { 1 });
        let h2 = e.spawn(async { 2 });
        let h3 = e.spawn(async { 3 });
        assert_eq!(h1.await.unwrap() + h2.await.unwrap() + h3.await.unwrap(), 6);
        assert_eq!(e.spawned_count(), 3);
    }

    /// `Executor` is intentionally generic-only (not dyn-safe) per ADR-0022.
    /// Holders parameterize their own types with `E: Executor` and
    /// monomorphize at compile time. This avoids vtable dispatch on the
    /// hot path while still letting the substrate swap production/simulation
    /// implementations at link time.
    #[tokio::test]
    async fn executor_holder_is_generic_over_implementation() {
        struct EngineLike<E: Executor> {
            exec: E,
        }
        impl<E: Executor> EngineLike<E> {
            // Returns the executor's associated handle type (ADR-0066 GAT) — for
            // Production/Simulated this is `tokio::task::JoinHandle<i32>`, for a
            // deterministic executor it is that executor's own handle.
            fn spawn_42(&self) -> E::JoinHandle<i32> {
                self.exec.spawn(async { 42 })
            }
        }
        // Production-mode engine
        let prod = EngineLike {
            exec: ProductionExecutor,
        };
        // Simulation-mode engine
        let sim = EngineLike {
            exec: SimulatedExecutor::new(),
        };
        // Both compile and the same `spawn_42` method dispatches correctly
        // through monomorphization.
        let _ = (prod.spawn_42(), sim.spawn_42());
    }

    use super::super::time::{ProductionTime, SimulatedTime};

    #[tokio::test(start_paused = true)]
    async fn timeout_returns_inner_value_when_future_completes() {
        // Tokio's paused clock makes ProductionTime::sleep instant here.
        let t = ProductionTime;
        let r = timeout(&t, Duration::from_millis(100), async { 7 }).await;
        assert_eq!(r, Ok(7));
    }

    #[tokio::test]
    async fn timeout_under_simulated_time_fires_on_clock_advance() {
        use std::sync::Arc;
        let t = Arc::new(SimulatedTime::new(0));
        let t2 = t.clone();

        // Future that never completes — only the timeout can resolve it.
        let task = tokio::spawn(async move {
            timeout(
                t2.as_ref(),
                Duration::from_millis(500),
                std::future::pending::<()>(),
            )
            .await
        });

        // Let the timeout sleeper subscribe to the watch channel.
        tokio::task::yield_now().await;
        // Advance the simulated clock past the timeout deadline.
        t.advance_ms(500);

        let result = task.await.expect("task panicked");
        assert_eq!(result, Err(TimedOut));
    }
}
