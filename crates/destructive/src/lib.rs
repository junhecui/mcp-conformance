//! Mechanical destructive/additive proxy, model classification, and human-label agreement statistics.
//!
//! **Must not:** Write into the deterministic verdict path (ADR-006). Read tool descriptions as anything but untrusted data.
//!
//! Contract: [architecture.md §3.1]. `mechanical_proxy` (Q-01) is the first piece landed;
//! the model classifier (Q-03) and agreement statistics (Q-04) remain — see docs/tasks.md.

mod mechanical_proxy;
pub use mechanical_proxy::{partition, ChangeKind, MechanicalProxyPartition, ProxyClassifiedPath};
