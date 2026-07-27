//! Build a byte-reproducible base layer: seeded filesystem, seeded database, mock backends.
//!
//! **Must not:** Produce nondeterministic bases. This silently poisons every diff and the failure is invisible in the output.
//!
//! Contract: [architecture.md §3.1]. Placeholder — see docs/tasks.md.
