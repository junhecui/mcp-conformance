//! Run queue, worker pool, scheduling. The one crate permitted to depend on everything.
//!
//! **Must not:** run more than one sandbox per worker slot at a time. Concurrent sandboxes
//! share a kernel and a page cache, and the resulting timing coupling is exactly the noise
//! that P2-08's noise floor is trying to measure. Scale out, not up (architecture.md §7).
//!
//! This is also where ruleset YAML is loaded and parsed, so that `normalise` can take a
//! parsed [`datamodel::Ruleset`] and stay free of I/O.
//!
//! Placeholder — P5-01.
