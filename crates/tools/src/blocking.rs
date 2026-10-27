//! One blocking job for the file work of one tool call.
//!
//! Each `tokio::fs` call is one round trip to the blocking pool, and only a
//! part of that round trip is the file work: the rest is a wake, a clock read
//! and a poll. A tool call whose file work is serial does all of it in one job
//! here, with `std::fs`, and the cancel token stays outside the job.

/// Runs `job` on the blocking pool and returns what it made.
///
/// A `JoinError` from a panicking job keeps its own unwind, so a bug here is
/// not reported as a file error that the model would answer with a retry.
pub(crate) async fn run<T: Send + 'static>(job: impl FnOnce() -> T + Send + 'static) -> T {
    #[cfg(test)]
    count_hop();
    match tokio::task::spawn_blocking(job).await {
        Ok(value) => value,
        Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
        Err(error) => unreachable!("a blocking job was cancelled: {error}"),
    }
}

#[cfg(test)]
thread_local! {
    /// The blocking jobs this thread started. A `current_thread` test runs its
    /// future on the test's own thread, so the count belongs to one test.
    static HOPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Counts one blocking job started on the pool, from [`run`] or from a tool
/// that spawns its own jobs in parallel, such as a walk's per-folder jobs.
#[cfg(test)]
pub(crate) fn count_hop() {
    HOPS.with(|hops| hops.set(hops.get() + 1));
}

/// The blocking jobs this thread started since the last reset.
#[cfg(test)]
pub(crate) fn hops() -> usize {
    HOPS.with(std::cell::Cell::get)
}

/// Starts counting blocking jobs from zero.
#[cfg(test)]
pub(crate) fn reset_hops() {
    HOPS.with(|hops| hops.set(0));
}
