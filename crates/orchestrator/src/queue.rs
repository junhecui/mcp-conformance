//! P5-01: a persistent run queue, wrapping `store::db`'s run-queue primitives
//! (`enqueue_job`/`lease_job`/`complete_job`/`fail_job`/`count_outstanding_jobs`) with the
//! one thing that module deliberately doesn't own: a clock. Everything here reads
//! `SystemTime::now()` and converts to the unix-epoch-second integers `store::db` compares
//! leases against — kept in this crate rather than `store` so `store::db`'s functions stay
//! exactly what their signatures say: given a `now`, do this, no hidden dependency on when
//! they're called.
//!
//! Genuinely cross-platform (SQLite plus a clock, nothing Linux-specific) — not gated to
//! `target_os = "linux"` like the sandbox-driving modules below it in `lib.rs`.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use store::db::QueueJobRow;

/// Why a [`RunQueue`] operation failed.
#[derive(Debug)]
pub enum QueueError {
    /// The underlying metadata DB reported an error.
    Db(rusqlite::Error),
}

impl std::fmt::Display for QueueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Db(e) => write!(f, "run queue database error: {e}"),
        }
    }
}

impl std::error::Error for QueueError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Db(e) => Some(e),
        }
    }
}

impl From<rusqlite::Error> for QueueError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Db(e)
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is before the unix epoch")
        .as_secs()
        .try_into()
        .expect("current unix time fits in i64 for the next few centuries")
}

/// A persistent, SQLite-backed job queue (`store::db`'s `run_queue` table): jobs survive a
/// process restart, and a job's lease expiring because its worker crashed makes it
/// available for another worker to pick up, rather than being lost.
///
/// Opening two [`RunQueue`]s at the same `path` (e.g. one per worker-pool slot) is the
/// intended way to get several independent connections racing the same on-disk queue —
/// `store::db::lease_job`'s own tests prove that race is safe.
pub struct RunQueue {
    conn: rusqlite::Connection,
}

impl RunQueue {
    /// Open (creating and migrating if necessary) the queue at `path`.
    ///
    /// # Errors
    /// Whatever `store::db::open_and_migrate` can fail with.
    pub fn open(path: &Path) -> Result<Self, QueueError> {
        let path_str = path.to_str().expect("run queue path must be valid UTF-8");
        let conn = store::db::open_and_migrate(path_str)?;
        Ok(Self { conn })
    }

    /// Enqueue one job. `payload` must be valid JSON; this queue has no opinion on what it
    /// means, only on delivering it to exactly one lease at a time.
    ///
    /// # Errors
    /// Whatever `store::db::enqueue_job` can fail with.
    pub fn enqueue(&self, payload: &str) -> Result<i64, QueueError> {
        Ok(store::db::enqueue_job(&self.conn, payload, &now_unix().to_string())?)
    }

    /// Lease one available job for `worker_id`, holding it for `lease` before another
    /// worker may reclaim it. `None` if nothing is currently available.
    ///
    /// # Errors
    /// Whatever `store::db::lease_job` can fail with.
    pub fn lease(&self, worker_id: &str, lease: Duration) -> Result<Option<QueueJobRow>, QueueError> {
        let now = now_unix();
        let deadline = now + i64::try_from(lease.as_secs()).unwrap_or(i64::MAX);
        Ok(store::db::lease_job(&self.conn, worker_id, now, deadline)?)
    }

    /// Mark a leased job done.
    ///
    /// # Errors
    /// Whatever `store::db::complete_job` can fail with.
    pub fn complete(&self, job_id: i64) -> Result<(), QueueError> {
        Ok(store::db::complete_job(&self.conn, job_id)?)
    }

    /// Mark a leased job permanently failed (never retried).
    ///
    /// # Errors
    /// Whatever `store::db::fail_job` can fail with.
    pub fn fail(&self, job_id: i64) -> Result<(), QueueError> {
        Ok(store::db::fail_job(&self.conn, job_id)?)
    }

    /// How many jobs still need a worker's attention (`pending` plus `leased`).
    ///
    /// # Errors
    /// Whatever `store::db::count_outstanding_jobs` can fail with.
    pub fn outstanding_count(&self) -> Result<i64, QueueError> {
        Ok(store::db::count_outstanding_jobs(&self.conn)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enqueue_lease_and_complete_round_trip_through_a_real_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("queue.db");

        let queue = RunQueue::open(&path).expect("open queue");
        let job_id = queue.enqueue(r#"{"tool":"read_file"}"#).expect("enqueue");
        assert_eq!(queue.outstanding_count().expect("count"), 1);

        let leased = queue
            .lease("worker-1", Duration::from_secs(30))
            .expect("lease")
            .expect("a job must be available");
        assert_eq!(leased.job_id, job_id);
        assert_eq!(leased.payload, r#"{"tool":"read_file"}"#);

        queue.complete(job_id).expect("complete");
        assert_eq!(queue.outstanding_count().expect("count"), 0);
    }

    #[test]
    fn a_freshly_leased_job_is_not_immediately_available_again() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("queue.db");
        let queue = RunQueue::open(&path).expect("open queue");
        queue.enqueue("{}").expect("enqueue");

        queue.lease("worker-1", Duration::from_secs(60)).expect("lease").expect("first lease");
        assert!(
            queue.lease("worker-2", Duration::from_secs(60)).expect("lease").is_none(),
            "a job leased with a 60s lease must not be handed to a second worker immediately"
        );
    }

    #[test]
    fn reopening_the_same_path_sees_the_same_queue() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("queue.db");

        let job_id = {
            let queue = RunQueue::open(&path).expect("open queue");
            queue.enqueue("{}").expect("enqueue")
        };

        // A second, independent `RunQueue` over the same file must see the job the first
        // one persisted — proving this queue survives past the process/connection that
        // enqueued into it, the whole point of backing it with a real file rather than an
        // in-memory structure.
        let queue = RunQueue::open(&path).expect("reopen queue");
        let leased = queue
            .lease("worker-1", Duration::from_secs(30))
            .expect("lease")
            .expect("the persisted job must still be there");
        assert_eq!(leased.job_id, job_id);
    }
}
