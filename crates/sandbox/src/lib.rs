//! Construct namespaces, launch the tool under test, enforce caps, tear down.
//!
//! **Must not:** emit any verdict. This crate reports *what happened* to a run (how the
//! process ended, whether the hard timeout fired, whether teardown found anything left
//! behind) and where the overlay upper layer is. Deciding what any of that means is P1-05's
//! integrity gate and P1-07's verdict engine; neither is a dependency of this crate, and
//! `cargo purity` keeps `normalise`/`verdict` from ever depending on it.
//!
//! # What exists (P1-03)
//!
//! [`supervisor`]: a private mount namespace, an overlayfs mount over a caller-supplied lower
//! layer with ADR-010's pinned options (privileged mount, `nouserxattr` branch — ADR-016),
//! `pivot_root` into the merged view with the old root detached, a credential drop to a
//! dedicated, run-exclusive unprivileged id with an empty bounding set, locked securebits and
//! `no_new_privs`, a hard wall-clock timeout for the whole run, and a teardown that preserves
//! the upper layer for P1-04 and removes everything else. `sweep` (private): finding and
//! killing every process of a run by its uid, which a nested user+mount namespace cannot
//! evade. [`overlay`]: the pure option-string construction and path validation the mount
//! uses.
//!
//! # What does not exist yet, and where it slots in
//!
//! PID + user namespaces (P2-01), cgroups (P2-02), network namespace (P3-01), seccomp
//! (P4-01). Each slot is marked in `supervisor.rs` at the point in the pre-exec sequence
//! where it belongs. Until P2-01 lands there is no kernel boundary around a run's processes;
//! [`supervisor::TeardownReport`] records what the uid sweep found and killed instead.
//!
//! # Linux-only, at the build level
//!
//! F-02 requires this gate to be a build-level fact rather than a runtime check. The crate
//! root is `cfg`'d out entirely off Linux, so on a macOS development host this compiles to
//! an empty crate and `cargo build` still succeeds workspace-wide. Nothing degrades
//! silently into a capability probe: code that needs the sandbox fails to resolve, loudly.
//!
//! The kernel under test is the F-08 pinned guest (Ubuntu 24.04, GA 6.8), not whatever Linux
//! the crate happens to compile on: WSL2's kernel builds and unit-tests this crate but its
//! overlayfs behaviour is not evidence (tasks.md F-08).

#![cfg(target_os = "linux")]

pub mod overlay;
pub mod supervisor;
mod sweep;

pub use supervisor::{
    RunAs, RunRecord, Sandbox, SandboxError, SandboxSpec, SandboxStdio, TeardownReport, spawn,
};
