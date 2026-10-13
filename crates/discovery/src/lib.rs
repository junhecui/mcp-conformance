//! Speak either MCP lifecycle — `2026-07-28`'s handshake-free `server/discover` or the
//! `2025-11-25`-and-earlier `initialize` — then `tools/list`; capture the raw JSON
//! byte-exact before parsing; pin tool metadata.
//!
//! Era selection is **modern-first** and decided from what the server actually answers, per
//! the spec's own backward-compatibility algorithm. See [`client`]'s module documentation
//! for the sequence and [`era`] for the classifier.
//!
//! **Must not:** Call any tool. This is enforced structurally: no tool-call method exists
//! on the client type (P0-01) — see the `client` module's source for exactly how.
//!
//! Request pacing and the per-registrable-domain request budget a sweep runs under live in
//! [`pace`] (P0-12), enforced at the one point every HTTP request this crate sends is built.
//!
//! Contract: [architecture.md §3.1].

mod client;
pub mod era;
pub mod jsonrpc;
pub mod pace;
mod pin;
mod transport;

pub use client::{
    Discovery, DiscoveryClient, DiscoveryError, DiscoveryPath, EraProvenance, ProbeAbsence,
    ProbeEvidence, RevisionSource, negotiated_spec_revision,
};
pub use era::{Era, FallbackReason};
pub use pace::{
    Accounting, Denied, DomainAccounting, MAX_REDIRECTS, MAX_REQUESTS_PER_DISCOVERY, PacePolicy,
    PacedAgent, Pacer, Permit, PermitStats, StatusHandling, registrable_domain,
    registrable_domain_of_endpoint,
};
pub use pin::{PinError, ToolPin, pin_tools};
