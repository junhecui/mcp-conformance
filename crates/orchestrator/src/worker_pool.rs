//! P5-01's other half: a worker pool draining a [`crate::queue::RunQueue`]. Each slot is a
//! real OS thread holding its own DB connection, leasing one job at a time and running it
//! to completion before leasing its next — "one sandbox per worker slot at a time"
//! (`lib.rs`'s own crate-level doc comment, architecture.md §7) is exactly what that loop
//! shape gives for free: a slot never leases job *N+1* until whatever it's doing for job
//! *N* has returned.
//!
//! **Scope actually verified here, disclosed rather than assumed:** every slot spawned by
//! [`WorkerPool::drain_all`] runs as a thread inside *this one process, on this one
//! container*, sharing one kernel. A real multi-host deployment — the "over Linux hosts"
//! half of P5-01's exit criterion — would run each slot's loop in a separate process on a
//! separate host, all pointed at the same [`crate::queue::RunQueue`] database over a shared
//! network filesystem or a network-attached DB server; nothing in [`RunQueue`]'s design
//! assumes same-host callers (it already has to tolerate independent connections racing the
//! same file — that's `store::db::lease_job`'s own proven property). But this environment
//! has exactly one Linux host to run anything on, so that deployment shape is asserted, not
//! tested, here. What *is* tested for real below: exactly-once job delivery under genuine
//! concurrent threads/connections, and recovery from a lease that expired because its
//! worker never came back (simulating a crash). Both tests below deliberately use a
//! lightweight, non-sandboxed handler — spawning real concurrent sandboxes on this one
//! shared kernel to prove pool mechanics would introduce exactly the noise coupling
//! architecture.md §7 warns `slots > 1` against on a single host; that is a deployment
//! question for wherever this pool actually runs multi-host, not something this pool's own
//! unit tests should manufacture on a laptop-shaped container.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::queue::{QueueError, RunQueue};

/// What a job handler decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobOutcome {
    /// The job succeeded; mark it `done`.
    Completed,
    /// The job failed in a way that must not be retried; mark it `failed`.
    Failed,
}

/// A pool of worker slots draining a [`RunQueue`].
pub struct WorkerPool;

impl WorkerPool {
    /// Drain every job currently outstanding (`pending` or `leased`) in the queue at
    /// `db_path`, using `slots` independent worker threads, each invoking `handler(payload)`
    /// for every job it leases. Returns once as many jobs have been finalised (completed or
    /// failed, by any slot) as were outstanding when this call started.
    ///
    /// `lease` bounds how long a slot may hold a job before another slot is allowed to
    /// re-lease it — the crash-recovery window. A slot that finds nothing to lease sleeps
    /// briefly and retries rather than spinning; this is the batch/offline shape P5-02 needs
    /// ("regenerates all verdicts; it is not a step in the run loop"), not a permanent
    /// daemon that waits for work that hasn't been enqueued yet.
    ///
    /// # Errors
    /// The first queue I/O error any slot encounters.
    ///
    /// # Panics
    /// If a worker thread itself panics (a bug in `handler`, not a queue error) — propagated
    /// rather than silently dropped, so a broken handler fails the pool loudly instead of
    /// quietly under-processing the queue.
    pub fn drain_all<F>(db_path: &Path, slots: usize, lease: Duration, handler: F) -> Result<usize, QueueError>
    where
        F: Fn(&str) -> JobOutcome + Send + Sync + 'static,
    {
        let target = {
            let queue = RunQueue::open(db_path)?;
            queue.outstanding_count()?
        };
        if target <= 0 {
            return Ok(0);
        }

        let handler = Arc::new(handler);
        let finished = Arc::new(AtomicI64::new(0));

        let workers: Vec<thread::JoinHandle<Result<(), QueueError>>> = (0..slots)
            .map(|slot| {
                let db_path: PathBuf = db_path.to_path_buf();
                let handler = Arc::clone(&handler);
                let finished = Arc::clone(&finished);
                thread::spawn(move || run_slot(slot, &db_path, lease, target, &finished, handler.as_ref()))
            })
            .collect();

        let mut first_error = None;
        for worker in workers {
            let result = worker.join().expect("worker thread must not panic");
            if let Err(e) = result {
                first_error.get_or_insert(e);
            }
        }
        if let Some(e) = first_error {
            return Err(e);
        }

        let finished_count = finished.load(Ordering::SeqCst);
        Ok(usize::try_from(finished_count).expect("finished count is never negative"))
    }
}

fn run_slot(
    slot: usize,
    db_path: &Path,
    lease: Duration,
    target: i64,
    finished: &AtomicI64,
    handler: &(dyn Fn(&str) -> JobOutcome + Send + Sync),
) -> Result<(), QueueError> {
    let queue = RunQueue::open(db_path)?;
    let worker_id = format!("slot-{slot}");

    while finished.load(Ordering::SeqCst) < target {
        if let Some(job) = queue.lease(&worker_id, lease)? {
            match handler(&job.payload) {
                JobOutcome::Completed => queue.complete(job.job_id)?,
                JobOutcome::Failed => queue.fail(job.job_id)?,
            }
            finished.fetch_add(1, Ordering::SeqCst);
        } else {
            if finished.load(Ordering::SeqCst) >= target {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Mutex;

    /// P5-01's exit criterion for the pool half: every job enqueued gets processed exactly
    /// once, across real concurrent worker threads racing a real on-disk queue — not
    /// dropped, not double-processed. Also proves the pool actually parallelises (not
    /// accidentally serial): the handler sleeps briefly and records how many concurrent
    /// invocations it observes at once, which must exceed 1 with more than one slot.
    #[test]
    fn every_job_is_processed_exactly_once_by_a_real_concurrent_pool() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("queue.db");
        let job_count = 24;

        {
            let queue = RunQueue::open(&db_path).expect("open queue");
            for i in 0..job_count {
                queue.enqueue(&format!(r#"{{"index":{i}}}"#)).expect("enqueue");
            }
        }

        let seen: Arc<Vec<AtomicUsize>> =
            Arc::new((0..job_count).map(|_| AtomicUsize::new(0)).collect());
        let concurrent = Arc::new(AtomicI64::new(0));
        let max_concurrent = Arc::new(AtomicI64::new(0));

        let seen_clone = Arc::clone(&seen);
        let concurrent_clone = Arc::clone(&concurrent);
        let max_concurrent_clone = Arc::clone(&max_concurrent);
        let processed = WorkerPool::drain_all(&db_path, 4, Duration::from_secs(30), move |payload| {
            let now = concurrent_clone.fetch_add(1, Ordering::SeqCst) + 1;
            max_concurrent_clone.fetch_max(now, Ordering::SeqCst);
            thread::sleep(Duration::from_millis(20));

            let parsed: serde_json::Value = serde_json::from_str(payload).expect("valid JSON payload");
            let index = parsed["index"].as_u64().expect("index field") as usize;
            seen_clone[index].fetch_add(1, Ordering::SeqCst);

            concurrent_clone.fetch_sub(1, Ordering::SeqCst);
            JobOutcome::Completed
        })
        .expect("drain_all");

        assert_eq!(processed, job_count);
        for (index, counter) in seen.iter().enumerate() {
            assert_eq!(
                counter.load(Ordering::SeqCst),
                1,
                "job {index} must be processed exactly once, got {}",
                counter.load(Ordering::SeqCst)
            );
        }
        assert!(
            max_concurrent.load(Ordering::SeqCst) > 1,
            "a 4-slot pool over 24 jobs must observe real overlap, not accidental serialisation"
        );

        let queue = RunQueue::open(&db_path).expect("reopen queue");
        assert_eq!(queue.outstanding_count().expect("count"), 0);
    }

    /// Crash recovery: a job leased with a very short lease, whose "worker" then never
    /// completes it (simulating a crash), must still get processed once the lease expires
    /// — proven against the real pool, not just `store::db::lease_job` in isolation.
    #[test]
    fn a_job_abandoned_by_a_crashed_lease_is_recovered_by_the_pool() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("queue.db");

        let queue = RunQueue::open(&db_path).expect("open queue");
        queue.enqueue(r#"{"marker":"abandoned"}"#).expect("enqueue");
        // Simulate a worker that leased the job and then crashed: lease it directly and
        // drop the guard without ever completing or failing it.
        let leased =
            queue.lease("dead-worker", Duration::from_millis(50)).expect("lease").expect("lease job");
        drop(queue);
        thread::sleep(Duration::from_millis(80)); // let the short lease expire

        let recovered_payload = Arc::new(Mutex::new(None));
        let recovered_payload_clone = Arc::clone(&recovered_payload);
        let processed = WorkerPool::drain_all(&db_path, 1, Duration::from_secs(10), move |payload| {
            *recovered_payload_clone.lock().expect("lock") = Some(payload.to_string());
            JobOutcome::Completed
        })
        .expect("drain_all");

        assert_eq!(processed, 1);
        assert_eq!(
            recovered_payload.lock().expect("lock").as_deref(),
            Some(r#"{"marker":"abandoned"}"#)
        );
        let _ = leased; // only its side effect (the lease row) matters, not the value itself
    }

    /// A job a handler reports [`JobOutcome::Failed`] for must be recorded as terminally
    /// failed — never re-leased, unlike an expired lease.
    #[test]
    fn a_handler_reported_failure_is_never_retried() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("queue.db");
        {
            let queue = RunQueue::open(&db_path).expect("open queue");
            queue.enqueue(r#"{"should":"fail"}"#).expect("enqueue");
        }

        let processed = WorkerPool::drain_all(&db_path, 1, Duration::from_secs(10), |_payload| {
            JobOutcome::Failed
        })
        .expect("drain_all");
        assert_eq!(processed, 1);

        // A failed job is not "outstanding" (pending/leased), so a second drain must find
        // nothing to do — proving it was never silently returned to the queue.
        let queue = RunQueue::open(&db_path).expect("reopen queue");
        assert_eq!(queue.outstanding_count().expect("count"), 0);
    }

    /// An empty queue must drain immediately, doing no work and spawning no threads that
    /// would otherwise spin forever waiting for a `target` of zero jobs.
    #[test]
    fn draining_an_empty_queue_is_a_no_op() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("queue.db");
        RunQueue::open(&db_path).expect("open queue"); // creates the (empty) queue file

        let processed = WorkerPool::drain_all(&db_path, 3, Duration::from_secs(10), |_| {
            panic!("handler must never be invoked for an empty queue")
        })
        .expect("drain_all");
        assert_eq!(processed, 0);
    }
}
