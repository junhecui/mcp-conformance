//! Protocol-probe oracle (Track B, B-01): decide `readOnlyHint`/`idempotentHint` for a
//! Class B (remote, uncontainable) server from the protocol surface alone — probe →
//! invoke → probe — when the server exposes MCP **resources** that reflect its own state
//! (architecture.md §2's narrow exception).
//!
//! Unlike `discovery` (P0-01), which is structurally forbidden from calling a tool, this
//! crate's entire job is to call one — deliberately, against a **live, real, third-party**
//! server. There is no sandbox here: Class B servers cannot be contained (architecture.md
//! §2), so an invocation's side effects, if any, are real. Running real code found in the
//! wild against an assumed-hostile trust model is this project's premise (design.md §3),
//! not something this crate second-guesses — but that authorisation is at the project
//! level, not a licence for recklessness here. Concretely:
//!
//! - Arguments are synthesised as narrowly as possible ([`argsynth_min`]) — structural
//!   validity only, no attempt to construct anything elaborate.
//! - Every invocation is logged by the caller, never silent.
//! - A server with no usable probe surface is left `unverifiable`, never guessed at
//!   ([`protocol`]).
//!
//! Every verdict this crate produces is tagged `oracle = protocol_probe`
//! ([`datamodel::Oracle::ProtocolProbe`]) — weaker than the kernel-changeset oracle,
//! because it only observes whatever state the server chooses to expose through resources.
//! B-03 (`store::aggregate`) exists so that distinction can never quietly disappear in a
//! published report.

mod argsynth_min;
mod client;
mod protocol;
mod runner;
mod snapshot;

pub use argsynth_min::synthesize_arguments;
pub use client::{ProbeClient, ProbeError};
// The two reason-code constructors are re-exported alongside the decision functions, which
// they were not before P1-07. `invocation_failed` is the one reason code this oracle shares
// with the Class A engine (`verdict::reason::INVOCATION_FAILED`), spelled separately because
// ADR-005 forbids an edge between `probe` and `verdict` in either direction; exporting it
// lets `xtask` — which can see both crates without either seeing the other — assert the two
// spellings still agree (`xtask/tests/reason_codes.rs`). `no_probe_surface` comes with it
// rather than being left behind, since exporting one of a pair of sibling constructors is
// the kind of asymmetry that costs the next reader a detour.
pub use protocol::{
    ProbeAssessment, assess_idempotent, assess_read_only, invocation_failed, no_probe_surface,
};
pub use runner::{ProbeTarget, probe_idempotent_hint, probe_read_only_hint};
pub use snapshot::{ProbeSurface, ResourceRef, discover_surface, snapshot_state};
