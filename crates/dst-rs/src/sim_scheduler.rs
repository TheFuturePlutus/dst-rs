//! Deterministic single-thread scheduler — Phase B #1 keystone.
//!
//! The FoundationDB / Antithesis core: run many async tasks on ONE thread in a
//! **reproducible** order, driven by a **virtual clock**, so a whole concurrent
//! workload replays byte-identically. This is the substrate the live chaos loop needs
//! to become byte-deterministic — today it runs on multi-threaded tokio, so
//! `SimulatedExecutor` can only *count* spawns, not order them.
//!
//! This module is the standalone, self-contained primitive: `spawn` + `run` with a
//! FIFO-deterministic ready queue, a cooperative `yield_now`, and a virtual-clock
//! `sleep`. Wiring it in to replace the engine's ~91 `tokio::spawn` sites + ~389
//! wall-clock reads is the follow-on migration (a strangler pattern, LAL / chaos
//! `--live` first). Tasks are `!Send` (single-thread); the waker uses an `Arc<Mutex>`
//! ready queue because `std::task::Wake` requires `Send + Sync`.

use std::cell::{Cell, RefCell};
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};

type LocalFuture = Pin<Box<dyn Future<Output = ()>>>;

// The sleep/yield futures reach the running scheduler through these thread-locals,
// set for the duration of `run`. Single-threaded, so no cross-thread sharing.
thread_local! {
    static CLOCK_MS: Cell<u64> = const { Cell::new(0) };
    // (wake_at_ms, seq, waker) — seq breaks ties deterministically (insertion order).
    static TIMERS: RefCell<BinaryHeap<Reverse<(u64, u64, WakerBox)>>> =
        const { RefCell::new(BinaryHeap::new()) };
    static TIMER_SEQ: Cell<u64> = const { Cell::new(0) };
    // True while a `run()` is in progress on this thread. The clock / timer heap
    // / timer-seq above are thread-local shared state that a NESTED `run()` would
    // silently clobber (reset to 0, drop pending timers), corrupting the outer
    // run. `run()` is therefore non-reentrant and this flag enforces it loudly.
    static RUNNING: Cell<bool> = const { Cell::new(false) };
}

/// RAII guard that marks a `run()` as in-progress and clears the flag on drop —
/// including on unwind, so a panicking task cannot leave the thread wedged in a
/// "running" state that poisons the next `run()`.
struct RunGuard;
impl Drop for RunGuard {
    fn drop(&mut self) {
        RUNNING.with(|r| r.set(false));
    }
}

/// A `Waker` that is `Ord`-comparable only by the tuple around it (never by itself),
/// so it can live in the timer heap without requiring `Waker: Ord`.
struct WakerBox(Waker);
impl PartialEq for WakerBox {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}
impl Eq for WakerBox {}
impl PartialOrd for WakerBox {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for WakerBox {
    fn cmp(&self, _: &Self) -> std::cmp::Ordering {
        std::cmp::Ordering::Equal
    }
}

/// Re-enqueues its task id onto the shared ready queue when woken.
struct TaskWaker {
    id: u64,
    ready: Arc<Mutex<VecDeque<u64>>>,
}
impl Wake for TaskWaker {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.ready
            .lock()
            .expect("ready queue poisoned")
            .push_back(self.id);
    }
}

/// A deterministic single-thread async executor with a virtual clock.
///
/// # Examples
///
/// ```
/// use dst_rs::SimScheduler;
/// use std::cell::Cell;
/// use std::rc::Rc;
///
/// let ran = Rc::new(Cell::new(false));
/// let flag = ran.clone();
///
/// let mut sched = SimScheduler::new();
/// sched.spawn(async move { flag.set(true); });
/// sched.run(); // drives every spawned task to completion, deterministically
///
/// assert!(ran.get());
/// ```
pub struct SimScheduler {
    next_id: u64,
    tasks: HashMap<u64, LocalFuture>,
    ready: Arc<Mutex<VecDeque<u64>>>,
    steps: u64,
}

impl Default for SimScheduler {
    fn default() -> Self {
        Self::new()
    }
}

impl SimScheduler {
    /// Create an empty scheduler with no spawned tasks and a zeroed step count.
    pub fn new() -> Self {
        Self {
            next_id: 0,
            tasks: HashMap::new(),
            ready: Arc::new(Mutex::new(VecDeque::new())),
            steps: 0,
        }
    }

    /// Schedule a future. Ready in spawn order — the FIFO queue makes the initial
    /// interleaving deterministic.
    pub fn spawn(&mut self, fut: impl Future<Output = ()> + 'static) {
        let id = self.next_id;
        self.next_id += 1;
        self.tasks.insert(id, Box::pin(fut));
        self.ready
            .lock()
            .expect("ready queue poisoned")
            .push_back(id);
    }

    /// The virtual clock (ms). Advances only when the ready queue drains and a timer
    /// is due.
    pub fn now_ms(&self) -> u64 {
        CLOCK_MS.with(|c| c.get())
    }

    /// Total task polls executed — a determinism witness (same workload ⇒ same steps).
    pub fn steps(&self) -> u64 {
        self.steps
    }

    /// Run every task (and its descendants) to completion. Polls ready tasks in FIFO
    /// order; when the ready queue empties, advances the virtual clock to the next due
    /// timer and wakes its sleeper. Terminates when no task is ready and no timer is
    /// pending.
    ///
    /// **Non-reentrant.** The virtual clock, timer heap, and timer sequence are
    /// per-thread shared state; a `run()` invoked from within a task being driven
    /// by an outer `run()` (on the same thread) would reset and clobber that
    /// state. Such a nested call panics with a clear message rather than
    /// corrupting the outer run silently. Run independent schedulers on separate
    /// threads, or sequentially.
    pub fn run(&mut self) {
        RUNNING.with(|r| {
            assert!(
                !r.get(),
                "SimScheduler::run() is not reentrant: a nested run() on the same \
                 thread would clobber the outer run's virtual clock and timers. \
                 Run schedulers sequentially or on separate threads."
            );
            r.set(true);
        });
        // From here on, always clear RUNNING on exit (including panic/unwind).
        let _running = RunGuard;

        CLOCK_MS.with(|c| c.set(0));
        TIMERS.with(|t| t.borrow_mut().clear());
        TIMER_SEQ.with(|s| s.set(0));

        loop {
            let next = self.ready.lock().expect("ready queue poisoned").pop_front();
            let Some(id) = next else {
                // Ready queue empty — fire the earliest timer, or stop.
                let due = TIMERS.with(|t| t.borrow_mut().pop());
                match due {
                    Some(Reverse((wake_at, _seq, waker))) => {
                        CLOCK_MS.with(|c| {
                            if wake_at > c.get() {
                                c.set(wake_at);
                            }
                        });
                        waker.0.wake(); // re-enqueues the sleeping task
                        continue;
                    }
                    None => break, // nothing ready, no timers → done
                }
            };

            // The task may have completed via another path already.
            let Some(mut fut) = self.tasks.remove(&id) else {
                continue;
            };
            let waker: Waker = Arc::new(TaskWaker {
                id,
                ready: self.ready.clone(),
            })
            .into();
            let mut cx = Context::from_waker(&waker);
            self.steps += 1;
            match fut.as_mut().poll(&mut cx) {
                Poll::Ready(()) => { /* dropped */ }
                Poll::Pending => {
                    // Park it; a waker (yield/sleep/other) will re-enqueue the id.
                    self.tasks.insert(id, fut);
                }
            }
        }
    }
}

/// Yield once: park, immediately re-schedule. Creates a deterministic interleaving
/// point (the task goes to the back of the FIFO ready queue).
pub async fn yield_now() {
    struct YieldOnce(bool);
    impl Future for YieldOnce {
        type Output = ();
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            if self.0 {
                Poll::Ready(())
            } else {
                self.0 = true;
                cx.waker().wake_by_ref(); // re-enqueue behind everyone ready now
                Poll::Pending
            }
        }
    }
    YieldOnce(false).await
}

/// Sleep until the virtual clock has advanced by `ms`. Registers a timer; the run
/// loop wakes it when the clock reaches the deadline. Deterministic: wake order is by
/// (deadline, registration-seq).
pub async fn sleep(ms: u64) {
    struct Sleep {
        deadline: u64,
        registered: bool,
    }
    impl Future for Sleep {
        type Output = ();
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            if CLOCK_MS.with(|c| c.get()) >= self.deadline {
                return Poll::Ready(());
            }
            if !self.registered {
                self.registered = true;
                let seq = TIMER_SEQ.with(|s| {
                    let v = s.get();
                    s.set(v + 1);
                    v
                });
                TIMERS.with(|t| {
                    t.borrow_mut().push(Reverse((
                        self.deadline,
                        seq,
                        WakerBox(cx.waker().clone()),
                    )));
                });
            }
            Poll::Pending
        }
    }
    let deadline = CLOCK_MS.with(|c| c.get()) + ms;
    Sleep {
        deadline,
        registered: false,
    }
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;

    #[test]
    fn runs_all_spawned_tasks_to_completion() {
        let done = Rc::new(Cell::new(0));
        let mut s = SimScheduler::new();
        for _ in 0..5 {
            let d = done.clone();
            s.spawn(async move {
                d.set(d.get() + 1);
            });
        }
        s.run();
        assert_eq!(done.get(), 5, "every spawned task ran to completion");
    }

    /// The keystone property: a workload of interleaving tasks produces the SAME
    /// execution trace every run — reproducibility without a real clock or threads.
    #[test]
    fn interleaving_is_deterministic_run_to_run() {
        fn trace() -> Vec<u32> {
            let out = Rc::new(RefCell::new(Vec::new()));
            let mut s = SimScheduler::new();
            for id in 0..3u32 {
                let o = out.clone();
                s.spawn(async move {
                    for step in 0..3 {
                        o.borrow_mut().push(id * 10 + step);
                        yield_now().await;
                    }
                });
            }
            s.run();
            Rc::try_unwrap(out).unwrap().into_inner()
        }
        let a = trace();
        let b = trace();
        assert_eq!(
            a, b,
            "same workload must produce the same interleaving trace"
        );
        // Round-robin FIFO: each task's step 0 before any step 1, etc.
        assert_eq!(
            &a[0..3],
            &[0, 10, 20],
            "step 0 of all tasks first (FIFO round-robin)"
        );
        assert_eq!(a.len(), 9);
    }

    /// `run()` is non-reentrant: nesting one inside a task driven by an outer
    /// `run()` would clobber the shared thread-local clock/timer state, so it must
    /// panic loudly instead of corrupting the outer run silently.
    #[test]
    #[should_panic(expected = "not reentrant")]
    fn nested_run_panics_loudly() {
        let mut outer = SimScheduler::new();
        outer.spawn(async {
            let mut inner = SimScheduler::new();
            inner.spawn(async {});
            inner.run(); // reentrant on the same thread → must panic
        });
        outer.run();
    }

    /// After a panic unwinds out of `run()`, the re-entrancy flag must be cleared
    /// so a subsequent, sequential `run()` on the same thread still works.
    #[test]
    fn run_is_usable_again_after_a_panicking_run() {
        let mut s = SimScheduler::new();
        s.spawn(async { panic!("boom") });
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| s.run()));
        assert!(
            caught.is_err(),
            "the task panic should surface out of run()"
        );

        // A fresh, sequential run must not be poisoned by the previous unwind.
        let done = Rc::new(Cell::new(false));
        let d = done.clone();
        let mut s2 = SimScheduler::new();
        s2.spawn(async move {
            d.set(true);
        });
        s2.run();
        assert!(
            done.get(),
            "sequential run after a panic must still execute"
        );
    }

    #[test]
    fn sleep_advances_the_virtual_clock_in_deadline_order() {
        let order = Rc::new(RefCell::new(Vec::new()));
        let mut s = SimScheduler::new();
        for (id, delay) in [(1u32, 30u64), (2, 10), (3, 20)] {
            let o = order.clone();
            s.spawn(async move {
                sleep(delay).await;
                o.borrow_mut().push(id);
            });
        }
        s.run();
        // Wakes in deadline order (10, 20, 30) regardless of spawn order.
        assert_eq!(*order.borrow(), vec![2, 3, 1]);
        // No wall-clock elapsed — the clock is virtual, advanced to the last deadline.
        assert_eq!(s.now_ms(), 30);
    }

    #[test]
    fn nested_spawn_is_not_needed_but_wakers_compose() {
        // A task that yields many times still terminates deterministically.
        let count = Rc::new(Cell::new(0u32));
        let mut s = SimScheduler::new();
        let c = count.clone();
        s.spawn(async move {
            for _ in 0..100 {
                yield_now().await;
                c.set(c.get() + 1);
            }
        });
        s.run();
        assert_eq!(count.get(), 100);
    }

    /// Piece-6 de-risk spike: does `tokio::sync::mpsc` work under SimScheduler (a
    /// NON-tokio executor)? The engine's worker mailboxes are tokio mpsc; if `recv()`
    /// requires a tokio runtime context, running the worker pool on SimScheduler would
    /// force swapping the channel type (big blast radius). This proves it does NOT —
    /// tokio mpsc is runtime-agnostic: a producer + consumer wired by an mpsc channel
    /// both drive to completion on the deterministic scheduler, in FIFO order.
    #[test]
    fn spike_tokio_mpsc_recv_works_under_sim_scheduler() {
        let got = Rc::new(RefCell::new(Vec::new()));
        let mut s = SimScheduler::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<u32>();

        // Producer: send 0..5, yielding between each so the consumer interleaves.
        s.spawn(async move {
            for i in 0..5 {
                tx.send(i).expect("send");
                yield_now().await;
            }
            // tx dropped here → the consumer's recv() will observe channel close.
        });
        // Consumer: drain until the channel closes.
        let g = got.clone();
        s.spawn(async move {
            while let Some(v) = rx.recv().await {
                g.borrow_mut().push(v);
            }
        });

        s.run();
        assert_eq!(
            *got.borrow(),
            vec![0, 1, 2, 3, 4],
            "tokio mpsc recv() drives to completion under SimScheduler (no tokio runtime)"
        );
    }

    /// Piece-6 keystone proof: the engine's worker-pool CONCURRENCY PATTERN — N workers,
    /// each draining its own tokio-mpsc mailbox, fed by a hash-routing dispatcher — runs
    /// DETERMINISTICALLY on SimScheduler. This is the exact shape of `WorkerPool` +
    /// `run_worker` (worker.rs), minus the engine's business logic: today those owner
    /// loops run on N OS threads (nondeterministic interleaving, per the Piece-3 finding);
    /// here they run as N cooperative tasks on one deterministic thread, so two runs of
    /// the same workload produce the byte-identical per-worker processing trace. The
    /// remaining integration is purely wiring: the `Executor` trait returns a
    /// `tokio::task::JoinHandle`, so injecting a SimScheduler-backed executor into the
    /// engine needs that trait's return type abstracted (a GAT) first — a separate ADR.
    #[test]
    fn worker_pool_pattern_is_deterministic_on_sim_scheduler() {
        // One deterministic run: 3 mailbox-draining workers + a dispatcher that
        // hash-routes `n` events. Returns the per-worker ordered processing trace.
        fn run_once(n: u32) -> Vec<(u32, u32)> {
            let trace = Rc::new(RefCell::new(Vec::new())); // (worker_idx, event_id)
            let mut s = SimScheduler::new();
            let num_workers = 3u32;

            // Spawn the workers first (ids 0..3), each with its own mailbox.
            let mut txs = Vec::new();
            for w in 0..num_workers {
                let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<u32>();
                txs.push(tx);
                let tr = trace.clone();
                s.spawn(async move {
                    // Yield once up front so all workers "start" before events flow —
                    // mirrors run_worker's startup beat, and exercises interleaving.
                    yield_now().await;
                    while let Some(ev) = rx.recv().await {
                        tr.borrow_mut().push((w, ev));
                    }
                });
            }

            // Dispatcher: route each event to `hash(ev) % num_workers`, yielding between
            // sends so workers interleave. Drops all txs at the end → workers finish.
            s.spawn(async move {
                for ev in 0..n {
                    let w = (ev.wrapping_mul(2_654_435_761) % num_workers) as usize;
                    txs[w].send(ev).expect("send to worker");
                    yield_now().await;
                }
                // txs dropped here.
            });

            s.run();
            Rc::try_unwrap(trace).unwrap().into_inner()
        }

        let a = run_once(30);
        let b = run_once(30);
        assert_eq!(
            a, b,
            "same workload must produce the identical per-worker trace across runs"
        );
        assert_eq!(a.len(), 30, "every event was processed exactly once");
        // Non-vacuous: work actually spread across more than one worker.
        let distinct_workers: std::collections::BTreeSet<u32> = a.iter().map(|(w, _)| *w).collect();
        assert!(
            distinct_workers.len() > 1,
            "events routed to multiple workers, not a single one"
        );
    }
}
