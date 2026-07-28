//! Build a byte-reproducible base layer: seeded filesystem, seeded database, mock backends.
//!
//! **Must not:** Produce nondeterministic bases. This silently poisons every diff and the failure is invisible in the output.
//!
//! Contract: [architecture.md §3.1].
//!
//! [`base_layer`] is P1-02: the overlayfs base (lower) layer builder and its
//! byte-reproducibility proof. Seeded databases and mock backends (P2-05) remain
//! unimplemented — see that module's docs for the exact scope split.

pub mod base_layer;
