//! Construct namespaces, launch the tool under test, enforce caps, tear down.
//!
//! **Must not:** emit any verdict.
//!
//! # Linux-only, at the build level
//!
//! F-02 requires this gate to be a build-level fact rather than a runtime check. The crate
//! root is `cfg`'d out entirely off Linux, so on a macOS development host this compiles to
//! an empty crate and `cargo build` still succeeds workspace-wide. Nothing degrades
//! silently into a capability probe: code that needs the sandbox fails to resolve, loudly.
//!
//! The consequence is that the sandbox is **untestable on the primary development host**.
//! That is F-00's problem — a pinned Linux image with fixed kernel version and overlayfs
//! mount options — and it is blocking, because overlayfs whiteout and opaque-directory
//! semantics vary by kernel version and therefore so does the measured noise floor.

#![cfg(target_os = "linux")]

// Placeholder — P1-03. Linux-only dependencies (`nix`, `seccompiler`, `cgroups-rs`) get
// added here under `[target.'cfg(target_os = "linux")'.dependencies]` when they are needed,
// so that a non-Linux host never even resolves them.
