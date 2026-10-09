//! `(canonical_evidence, protocol_version) -> verdict`. Pure.
//!
//! **Must not:** call a model, a network, or a clock.
//!
//! `no_std`, per ADR-007. Taking a clock as a dependency here is not a mistake CI catches
//! after the fact; it is code that does not link. Anything that later wants `std` in the
//! verdict engine is a signal that it belongs *outside* the verdict engine — which is the
//! intended effect, and will feel like an obstruction at least once.
//!
//! # Shape of the API, and why
//!
//! Three invariants shape every signature in this crate. Each is enforced by the types
//! rather than by a check a caller or a future author has to remember. [ADR-012] records
//! the decisions and the alternatives rejected.
//!
//! 1. **Only gate-passed evidence may be assessed** (ADR-004: the integrity gate is a hard
//!    precondition and is not configurable off). Every entry point takes a [`GatedRun`],
//!    which borrows a [`datamodel::GateAttestation`] — a token with no public constructor,
//!    mintable only by declaring yourself the integrity gate via
//!    [`datamodel::IntegrityGate`]. The gate itself is P1-05 and does not exist yet; none
//!    of its logic is needed to stand this boundary up, and none of it is guessed at here.
//! 2. **An empty changeset from a failed invocation is `unverifiable`, never `holds`.** The
//!    changeset and the [`InvocationResult`] arrive together through [`Observation::new`],
//!    the only constructor, so a caller cannot supply one and silently omit the other —
//!    which is precisely the gap commit `832d990` closed in Track B. And
//!    `Assessment::holds` takes a `Completion` witness that only a completed invocation can
//!    produce, so the dangerous outcome is **unreachable through this engine** rather than
//!    merely discouraged. It is not unconstructible in general: see [`Assessment`] for the
//!    forgery routes this does and does not close, and `store::db::insert_verdict` for the
//!    one it cannot.
//! 3. **`unverifiable` always carries a reason code** (architecture.md §6 invariant 3). The
//!    only constructor that produces `Outcome::Unverifiable` requires one; the set this
//!    crate emits is [`reason`], pending P2-11's closed taxonomy.
//!
//! The verdict is decided against the `user_state` partition alone and the other two are
//! reported alongside it (architecture.md §4.3, ADR-008) — see [`PartitionCounts`].
//!
//! [ADR-012]: ../../../docs/adr/012-verdict-engine-and-readonlyhint.md

#![no_std]

extern crate alloc;

mod assessment;
mod observation;

#[cfg(test)]
mod tests;

pub use assessment::Assessment;
pub use observation::{GatedRun, Observation};

// Re-exported so that the engine's callers name one vocabulary. Both live in `datamodel`
// because they also cross the storage boundary — `store` writes `PartitionCounts` and
// `InvocationResult` to `VERDICT` columns, and ADR-005 forbids an edge between `verdict` and
// `store` in either direction.
pub use datamodel::{InvocationResult, PartitionCounts};

use datamodel::DerivationFailure;

/// The reason codes this crate emits.
///
/// Not the closed taxonomy — that is P2-11, which will ratify these against what real runs
/// actually produce rather than against what seemed plausible before any run existed (the
/// same discipline ADR-003 applies to normalisation rules: a code that never appears should
/// not exist). Named constants rather than inline literals so the spelling cannot drift
/// between the engine, its tests, and the row it is stored in.
pub mod reason {
    /// The `tools/call` did not complete, so nothing can be concluded from the absence of
    /// a change, and a `false`/defaulted declaration cannot be confirmed either.
    ///
    /// Deliberately the **same spelling** Track B uses for the same situation
    /// (`probe::protocol::invocation_failed`), so the two oracles' records read the same
    /// way where they mean the same thing. The two spellings are not yet single-sourced —
    /// `verdict` and `probe` cannot share a constant without an edge ADR-005 forbids — and
    /// P2-11 is where the taxonomy should become one shared artefact.
    pub const INVOCATION_FAILED: &str = "invocation_failed";

    /// The stored **upper-layer** capture is not valid canonical `evtree1`
    /// ([`datamodel::DerivationFailure::MalformedEvidence`]). A finding about the tool: it
    /// wrote the tree these bytes were captured from.
    pub const MALFORMED_EVIDENCE: &str = "malformed_evidence";

    /// The stored **base-layer** capture is not valid canonical `evtree1`
    /// ([`datamodel::DerivationFailure::MalformedBaseLayer`]). Operator-side, like
    /// [`INVALID_RULESET`]: the base layer is harness-built (`world::base_layer`) and
    /// read-only to the tool, so this says nothing about the server and must never be
    /// published as though it did.
    pub const MALFORMED_BASE_LAYER: &str = "malformed_base_layer";

    /// A ruleset pattern did not compile
    /// ([`datamodel::DerivationFailure::InvalidRuleset`]). Operator-side: this says nothing
    /// about the tool, and is kept distinct from `MALFORMED_EVIDENCE` so that publication
    /// cannot report a harness fault as a finding about a server.
    pub const INVALID_RULESET: &str = "invalid_ruleset";

    /// The tool declared (or defaulted to) `readOnlyHint: false` and changed nothing in
    /// `user_state`. Nothing was contradicted and nothing was confirmed — see
    /// [`crate::read_only_hint`] for why this is not `holds`.
    pub const NO_USER_STATE_CHANGE: &str = "no_user_state_change";
}

/// The MCP specification default for `readOnlyHint` when a tool does not declare it
/// (design.md §1; re-verified against the `2026-07-28` draft schema's `ToolAnnotations` by
/// O-01 on 2026-07-27 and unchanged).
///
/// A named constant rather than a bare `false` at each call site: the four annotations do
/// *not* share a default (`destructiveHint` and `openWorldHint` default to `true`), so a
/// hardcoded `false` in the wrong protocol is a silent inversion of a safety-relevant
/// claim.
pub const READ_ONLY_HINT_DEFAULT: bool = false;

/// A tool's declared value for one boolean annotation, with its provenance.
///
/// An enum rather than a `bool` because the two likeliest call-site errors are both
/// invisible in a `bool`: passing "the annotation was present" instead of its value, and
/// hardcoding the wrong spec default for the annotation at hand. [`Self::effective`] makes
/// the default an explicit argument, and P0-05's explicit/defaulted/absent distinction
/// survives into the verdict rather than being flattened on the way in.
///
/// `Defaulted` covers both of P0-05's non-explicit buckets — an `annotations` object that
/// omits this key, and no `annotations` object at all. They are different *coverage*
/// findings (design.md's open question 1 is about absence, not just mismatch) and the same
/// *conformance* input: the spec default applies either way, so the census keeps them
/// apart and the engine does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Declared {
    /// The server declared this value explicitly.
    Explicit(bool),
    /// The server declared nothing; the spec default applies.
    Defaulted,
}

impl Declared {
    /// The value a client would act on: the declaration if there was one, else `default`.
    #[must_use]
    pub const fn effective(self, default: bool) -> bool {
        match self {
            Self::Explicit(value) => value,
            Self::Defaulted => default,
        }
    }

    /// Whether the server actually declared this annotation.
    #[must_use]
    pub const fn is_explicit(self) -> bool {
        matches!(self, Self::Explicit(_))
    }
}

/// Decide `readOnlyHint` from a single-invocation changeset.
///
/// A non-empty canonical changeset over `user_state` contradicts a `true` declaration
/// (P1-07's exit criterion). The `user_state` / `server_internal` / `ephemeral` split is
/// ADR-008, applied by `normalise` (P1-06, ADR-011); the decision itself is this function.
///
/// # The decision table
///
/// `mutated` is `!changeset.user_state.is_empty()`. `declared` is
/// `declared.effective(READ_ONLY_HINT_DEFAULT)`.
///
/// | derivation | invocation | declared | mutated | outcome | reason |
/// |---|---|---|---|---|---|
/// | failed | any | any | — | `unverifiable` | `malformed_evidence` / `malformed_base_layer` / `invalid_ruleset` |
/// | ok | any | `true` | yes | **`violated`** | — |
/// | ok | completed | `true` | no | `holds` | — |
/// | ok | completed | `false` | yes | `holds` | — |
/// | ok | completed | `false` | no | `unverifiable` | `no_user_state_change` |
/// | ok | failed | `true` | no | `unverifiable` | `invocation_failed` |
/// | ok | failed | `false` | any | `unverifiable` | `invocation_failed` |
///
/// Three rows deserve their reasons stated rather than inferred.
///
/// **`violated` does not require the invocation to have completed.** A change present in
/// `user_state` is a write that happened, and a tool that claims it does not modify state
/// has contradicted itself whether or not its call then reported an error. The asymmetry is
/// the point: a failed invocation can never manufacture confidence (`holds`), but it also
/// must not suppress a contradiction the kernel already recorded.
///
/// **`declared: false` with nothing observed is `unverifiable`, not `holds`.** `false` is
/// the conservative spec default and the overwhelmingly common case (48.6% of census-era
/// tools declare no annotations at all), so treating "we saw no local write" as
/// confirmation would fill the `holds` column with an observation that contradicts nothing
/// and confirms nothing. It is also the case design.md §8's *external state invisibility*
/// bites hardest: a tool that mutates a remote service produces no local changeset, and
/// Phase 1 has no network observation at all. This matches
/// `probe::protocol::assess_read_only`'s fourth row, which is `unverifiable` for the
/// structurally analogous reason; only the code differs
/// (`probe_surface_incomplete` names the weakness of the probe oracle, whereas here the
/// declaration is simply unfalsifiable by absence).
///
/// **`declared: false` with a mutation is `holds`.** The declaration said the tool may
/// modify state and the tool modified state. Same as Track B's third row.
#[must_use]
pub fn read_only_hint(declared: Declared, run: &GatedRun<'_>) -> Assessment {
    let observation = run.observation();
    let call = observation.call();

    // A pure, in-closure derivation failure is a reportable fact about stored evidence
    // (ADR-012 decision 5), so it is a verdict with a reason code rather than a missing row.
    //
    // These arms carry neither partition counts nor a ruleset identity, where every arm
    // below carries both, and that asymmetry is deliberate rather than an oversight: there
    // is no changeset, so there is nothing to count, and ADR-012 decision 7's whole point is
    // that the identity is taken *off the changeset* rather than restated by a caller — an
    // engine that invented one here would be asserting which ruleset bytes produced a
    // changeset that was never produced. The row shape that keeps the oracle truthful while
    // making both absences structural is
    // `store::db::VerdictProvenance::KernelChangesetDerivationFailed`. The invocation
    // result *is* carried: it is a fact about the run, not about the derivation.
    let changeset = match observation.derived() {
        Ok(changeset) => changeset,
        Err(DerivationFailure::MalformedEvidence) => {
            return Assessment::unverifiable(reason::MALFORMED_EVIDENCE, call, None, None);
        }
        Err(DerivationFailure::MalformedBaseLayer) => {
            return Assessment::unverifiable(reason::MALFORMED_BASE_LAYER, call, None, None);
        }
        Err(DerivationFailure::InvalidRuleset) => {
            return Assessment::unverifiable(reason::INVALID_RULESET, call, None, None);
        }
    };

    // Reported in every outcome below, including the unverifiable ones: the partition
    // counts and the ruleset the changeset was derived under are facts about the evidence,
    // and withholding them where the verdict is undecided is exactly where a reader most
    // needs them (architecture.md §4.3).
    let reported = Some(PartitionCounts::of(changeset));
    let identity = Some(changeset.ruleset_identity.clone());

    let claims_read_only = declared.effective(READ_ONLY_HINT_DEFAULT);
    let mutated = !changeset.user_state.is_empty();

    match (claims_read_only, mutated, observation.completion()) {
        // Positive contradiction. No completion witness required, by design.
        (true, true, _) => Assessment::violated(call, reported, identity),
        // Everything below reads an absence or confirms a declaration, and a run whose
        // invocation did not complete licenses neither.
        (_, _, None) => {
            Assessment::unverifiable(reason::INVOCATION_FAILED, call, reported, identity)
        }
        (true, false, Some(completion)) | (false, true, Some(completion)) => {
            Assessment::holds(completion, call, reported, identity)
        }
        (false, false, Some(_)) => {
            Assessment::unverifiable(reason::NO_USER_STATE_CHANGE, call, reported, identity)
        }
    }
}
