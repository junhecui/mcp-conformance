//! Mechanical destructive/additive proxy, model classification, and human-label agreement statistics.
//!
//! **Must not:** Write into the deterministic verdict path (ADR-006). Read tool descriptions as anything but untrusted data.
//!
//! Contract: [architecture.md §3.1]. `mechanical_proxy` (Q-01) and `agreement` (Q-02's own
//! Cohen's-kappa tooling, reused again by Q-04) are landed; the model classifier (Q-03) and
//! the actual labelled set and agreement statistics (Q-02/Q-04's own remaining, human-only
//! halves) remain — see docs/tasks.md and docs/labelling_protocol.md.

mod agreement;
pub use agreement::{cohens_kappa, AgreementError};

mod mechanical_proxy;
pub use mechanical_proxy::{partition, ChangeKind, MechanicalProxyPartition, ProxyClassifiedPath};
