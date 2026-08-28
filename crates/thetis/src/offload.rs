//! Keeping synchronous work off the async runtime's threads.
//!
//! Thetis does a lot of genuinely blocking work — redb transactions, cranelift
//! compiles, filesystem sweeps — from inside `async fn`s. Run inline, each of
//! those occupies a tokio worker thread for its whole duration, and there are
//! only as many worker threads as cores. A handful of concurrent store scans or
//! one guest compile is enough to starve the gateway: broadcast frames stop
//! moving, health polls stop answering, and the whole system reads as frozen
//! while nothing is actually deadlocked.
//!
//! `block_in_place` is the right tool where the work borrows its arguments (so
//! `spawn_blocking`, which needs `'static`, would mean cloning everything): it
//! tells the runtime to hand this thread's *other* tasks to a different worker
//! for the duration.

/// Runs blocking work without starving the runtime.
///
/// Falls back to running in place outside a multi-thread runtime, where
/// `block_in_place` would panic and there are no sibling tasks to starve
/// anyway — which is the shape every unit test runs under.
pub fn blocking<T>(f: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current().map(|h| h.runtime_flavor()) {
        Ok(tokio::runtime::RuntimeFlavor::MultiThread) => tokio::task::block_in_place(f),
        _ => f(),
    }
}

/// Logs when the async runtime stops keeping time.
///
/// Runtime starvation — every worker thread parked in a redb transaction, a
/// cranelift compile, a blocking `flock` — has no symptom of its own. Tasks
/// simply stop making progress, and it presents identically to a deadlock. A
/// task that only has to wake up and check the clock cannot be late unless the
/// runtime itself is, so a gap here is direct evidence, and it names the one
/// failure mode that `/admin/waits` cannot show (there is nothing to wait on;
/// nothing is running).
pub fn spawn_stall_detector() {
    const TICK: std::time::Duration = std::time::Duration::from_secs(5);
    // Scheduling jitter under load is normal; only a gap far beyond the tick
    // means threads were held.
    const COMPLAIN_AFTER: std::time::Duration = std::time::Duration::from_secs(8);

    tokio::spawn(async move {
        let mut last = std::time::Instant::now();
        loop {
            tokio::time::sleep(TICK).await;
            let now = std::time::Instant::now();
            let gap = now.duration_since(last);
            if gap > COMPLAIN_AFTER {
                tracing::warn!(
                    gap_ms = gap.as_millis() as u64,
                    threads = std::thread::available_parallelism()
                        .map(|n| n.get())
                        .unwrap_or(0),
                    "the async runtime stalled: a timer that should fire every {}s was {}s late. \
                     Something blocking is running on a runtime thread.",
                    TICK.as_secs(),
                    gap.as_secs(),
                );
            }
            last = now;
        }
    });
}
