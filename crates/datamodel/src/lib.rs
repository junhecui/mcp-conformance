//! Shared vocabulary: pure data types with no behaviour.
//!
//! Every other crate depends on this one and it depends on nothing. That is what lets
//! `normalise` and `verdict` name their inputs and outputs without reaching for an I/O
//! crate — see [ADR-007] and ADR-005.
//!
//! `no_std` is deliberate: a clock or a filesystem is not merely discouraged here, it is
//! not linked.
//!
//! Named `datamodel` rather than `model` because "model" already means *language model*
//! throughout the design docs, and this crate sits inside the pure allowlist where that
//! ambiguity would be actively dangerous.
//!
//! [ADR-007]: ../../../docs/adr/007-implementation-language.md

#![no_std]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

/// The outcome of a single conformance assessment.
///
/// `Unverifiable` is a first-class verdict, not an error path (design.md §9). Collapsing it
/// into `Holds` would be the worst available failure mode for a trust signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Outcome {
    /// Observation is consistent with the declared annotation.
    Holds,
    /// Observation contradicts the declared annotation.
    Violated,
    /// The protocol could not decide. Always accompanied by a [`ReasonCode`].
    Unverifiable,
}

/// Which observation surface produced a verdict.
///
/// Recorded on every verdict so that results are never aggregated across oracles without
/// disclosure (ADR-002). B-03 makes that a test rather than a habit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Oracle {
    /// Kernel-provided overlay changeset. The strong oracle; Class A only.
    KernelChangeset,
    /// Probe → invoke → probe over the protocol surface. Weaker: it observes only the
    /// state the server chooses to expose.
    ProtocolProbe,
}

impl Oracle {
    /// The exact text F-06's `VERDICT.oracle` `CHECK` constraint accepts
    /// (`crates/store/migrations/0001_initial_schema.sql`). Kept here, next to the enum
    /// this constraint mirrors, rather than duplicated as a string literal at every call
    /// site that needs to write or read one.
    #[must_use]
    pub const fn as_db_str(self) -> &'static str {
        match self {
            Self::KernelChangeset => "kernel_changeset",
            Self::ProtocolProbe => "protocol_probe",
        }
    }

    /// The inverse of [`Self::as_db_str`], for reading a stored verdict back out.
    #[must_use]
    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "kernel_changeset" => Some(Self::KernelChangeset),
            "protocol_probe" => Some(Self::ProtocolProbe),
            _ => None,
        }
    }
}

impl core::fmt::Display for Oracle {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_db_str())
    }
}

/// Where a `VERDICT` sits in the responsible-disclosure workflow (P5-03, design.md's open
/// question 4). `VERDICT.embargo_state` (architecture.md §6) — added ahead of Phase 5 while
/// it was cheap (architecture.md §12 item 6), given real behaviour by P5-03's disclosure
/// workflow.
///
/// A verdict that never needs naming (the common case — most results are published in
/// aggregate, never per-server) simply stays [`Self::None`] forever; this state machine is
/// only ever exercised for a verdict someone has decided to name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EmbargoState {
    /// No embargo consideration — the default, and where an aggregated-only verdict stays.
    None,
    /// Under active responsible-disclosure embargo: known internally, not yet safe to name
    /// in published results.
    Embargoed,
    /// The embargo has run its course; safe to publish by name.
    Disclosed,
}

impl EmbargoState {
    /// The exact text F-06's `VERDICT.embargo_state` `CHECK` constraint accepts.
    #[must_use]
    pub const fn as_db_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Embargoed => "embargoed",
            Self::Disclosed => "disclosed",
        }
    }

    /// The inverse of [`Self::as_db_str`].
    #[must_use]
    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "none" => Some(Self::None),
            "embargoed" => Some(Self::Embargoed),
            "disclosed" => Some(Self::Disclosed),
            _ => None,
        }
    }
}

impl core::fmt::Display for EmbargoState {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_db_str())
    }
}

impl Outcome {
    /// The exact text F-06's `VERDICT.outcome` `CHECK` constraint accepts.
    #[must_use]
    pub const fn as_db_str(self) -> &'static str {
        match self {
            Self::Holds => "holds",
            Self::Violated => "violated",
            Self::Unverifiable => "unverifiable",
        }
    }

    /// The inverse of [`Self::as_db_str`].
    #[must_use]
    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "holds" => Some(Self::Holds),
            "violated" => Some(Self::Violated),
            "unverifiable" => Some(Self::Unverifiable),
            _ => None,
        }
    }
}

impl core::fmt::Display for Outcome {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_db_str())
    }
}

/// Which of the four MCP behavioural annotations a verdict assesses.
///
/// Shared vocabulary for the same reason [`Oracle`] and [`Outcome`] are: `VERDICT.annotation`
/// (architecture.md §6) has a closed `CHECK` set, and every producer or consumer of a
/// verdict — the eventual Class A verdict engine, the Track B protocol-probe oracle, the
/// coverage aggregator's cousins — should name it the same way rather than each owning a
/// parallel string literal that can drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Annotation {
    /// `readOnlyHint`.
    ReadOnlyHint,
    /// `destructiveHint`.
    DestructiveHint,
    /// `idempotentHint`.
    IdempotentHint,
    /// `openWorldHint`.
    OpenWorldHint,
}

impl Annotation {
    /// The exact text F-06's `VERDICT.annotation` `CHECK` constraint accepts — the MCP
    /// wire name, not a Rust-cased variant.
    #[must_use]
    pub const fn as_db_str(self) -> &'static str {
        match self {
            Self::ReadOnlyHint => "readOnlyHint",
            Self::DestructiveHint => "destructiveHint",
            Self::IdempotentHint => "idempotentHint",
            Self::OpenWorldHint => "openWorldHint",
        }
    }

    /// The inverse of [`Self::as_db_str`].
    #[must_use]
    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "readOnlyHint" => Some(Self::ReadOnlyHint),
            "destructiveHint" => Some(Self::DestructiveHint),
            "idempotentHint" => Some(Self::IdempotentHint),
            "openWorldHint" => Some(Self::OpenWorldHint),
            _ => None,
        }
    }
}

impl core::fmt::Display for Annotation {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_db_str())
    }
}

/// Whether a server can be contained, decided at intake (ADR-002).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ContainabilityClass {
    /// Launchable locally. Full containment, full observation suite.
    A,
    /// Remote HTTP endpoint only. Protocol proxy at most; no changeset, no syscalls.
    B,
    /// Genuinely undecidable. A real class, never a fallback — the classifier must not guess.
    Unclassifiable,
}

/// Why an [`Outcome::Unverifiable`] was reached.
///
/// P2-11's closed taxonomy — *"`unverifiable` without a reason is not a finding, it is a
/// shrug"* — fixed only once the integrity gate (P2-03) and the idempotency protocol (P2-09)
/// had actually run and produced real codes to fix, per this crate's own long-standing
/// deferral: guessing the variants earlier would have been fixing the very codes the paper
/// reports before there was any evidence for what they should be. Every variant below is a
/// code some real, already-implemented producer actually emits today — none are speculative
/// placeholders for a producer that doesn't exist yet (P4-01's seccomp-derived escape-class
/// codes, for instance, are not here, because nothing produces one yet).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReasonCode {
    /// architecture.md §5.1's `G1`: the integrity gate has positive reason to distrust clean
    /// teardown. Produced by `integrity::decide`.
    ContainmentUncertain,
    /// architecture.md §5.1's `G2`: a resource cap was hit, truncating the run before the
    /// tool finished its work — an empty or partial changeset proves nothing. Produced by
    /// `integrity::decide`.
    ExecutionTruncated,
    /// architecture.md §5.1's `G3`: the sandbox's hard wall-clock timeout fired. Produced by
    /// `integrity::decide`.
    Timeout,
    /// architecture.md §4.2's `C2`: `idempotentHint`'s multi-arm protocol found `D2 Δ D1`
    /// within the noise floor but `D2R Δ D1` outside it — an effect suppressed within one
    /// process, reappearing after a restart. Produced by `verdict::idempotent_hint`.
    CachingSuppressedInProcess,
    /// Track B's protocol-probe oracle (B-01) found no observable resource/state surface at
    /// all to probe before or after invocation. Produced by `probe`.
    NoProbeSurface,
    /// Track B's protocol-probe oracle: the tool invocation itself failed, so no
    /// before/after comparison is possible. Produced by `probe`.
    InvocationFailed,
    /// Track B's protocol-probe oracle: some, but not all, of the state needed to decide
    /// the protocol was observable — an absent *observed* change doesn't confirm nothing
    /// changed, since this oracle only sees what the server chose to expose. Produced by
    /// `probe`.
    ProbeSurfaceIncomplete,
    /// architecture.md §4.4's `openWorldHint` decision tree: the strict arm (no route out,
    /// P3-01) saw no egress attempt *and* the tool itself failed — genuinely ambiguous
    /// whether the failure was caused by the missing network or something unrelated,
    /// undecidable without a rerun under the instrumented arm (P3-02 through P3-04) to see
    /// whether the tool reaches further once egress is actually possible. Produced by
    /// `verdict::open_world_hint`.
    EgressAmbiguousRerunInstrumented,
}

impl ReasonCode {
    /// The exact text this taxonomy writes for `VERDICT.reason_code` (architecture.md §6) —
    /// kept in one place, the same discipline [`Oracle::as_db_str`]/[`Outcome::as_db_str`]
    /// already follow, so nothing downstream can drift from what this crate actually
    /// produces by re-deriving a string literal at a second call site.
    #[must_use]
    pub const fn as_db_str(self) -> &'static str {
        match self {
            Self::ContainmentUncertain => "containment_uncertain",
            Self::ExecutionTruncated => "execution_truncated",
            Self::Timeout => "timeout",
            Self::CachingSuppressedInProcess => "caching_suppressed_in_process",
            Self::NoProbeSurface => "no_probe_surface",
            Self::InvocationFailed => "invocation_failed",
            Self::ProbeSurfaceIncomplete => "probe_surface_incomplete",
            Self::EgressAmbiguousRerunInstrumented => "egress_ambiguous_rerun_instrumented",
        }
    }

    /// The inverse of [`Self::as_db_str`], for reading a stored verdict's `reason_code` back
    /// out. `None` for anything not in this closed set — including a code an *older* build
    /// of this codebase might have written before this taxonomy closed; a reader from
    /// stored data must not guess at a code it doesn't recognise.
    #[must_use]
    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "containment_uncertain" => Some(Self::ContainmentUncertain),
            "execution_truncated" => Some(Self::ExecutionTruncated),
            "timeout" => Some(Self::Timeout),
            "caching_suppressed_in_process" => Some(Self::CachingSuppressedInProcess),
            "no_probe_surface" => Some(Self::NoProbeSurface),
            "invocation_failed" => Some(Self::InvocationFailed),
            "probe_surface_incomplete" => Some(Self::ProbeSurfaceIncomplete),
            "egress_ambiguous_rerun_instrumented" => Some(Self::EgressAmbiguousRerunInstrumented),
            _ => None,
        }
    }
}

impl core::fmt::Display for ReasonCode {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_db_str())
    }
}

/// The seven POSIX file types ADR-009's `evtree1` wire format distinguishes generically —
/// no `whiteout` or `opaque_directory` variant here on purpose. A whiteout is exactly
/// `CharDevice` at `dev_major = 0, dev_minor = 0`; recognising it as such is `normalise`'s
/// job (this crate stays pure data), never a fact this enum bakes in ahead of that.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EntryKind {
    /// A regular file.
    Regular,
    /// A directory.
    Directory,
    /// A symbolic link.
    Symlink,
    /// A named pipe.
    Fifo,
    /// A character-special device file — what an overlayfs whiteout is, generically.
    CharDevice,
    /// A block-special device file.
    BlockDevice,
    /// A Unix domain socket.
    Socket,
}

/// One filesystem object exactly as ADR-009's `evtree1` walker captured it — decoded, but
/// otherwise unfiltered. `path` is raw bytes (POSIX paths are not guaranteed UTF-8),
/// relative to the captured tree's root, matching the wire format's own encoding exactly.
///
/// Every field `lstat`/`listxattr` can report is retained per ADR-009's losslessness
/// requirement, except the xattr set and file content, which stay out of this in-memory
/// type for now — `normalise`'s only current consumer (ADR-008's path taxonomy) classifies
/// by `path` alone. Revisit when a protocol that needs them (xattr-aware normalisation,
/// content-level idempotency diffing — P2-09) actually exists; carrying full file content
/// through every `EvidenceEntry` today would be pure cost against no present use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceEntry {
    /// Raw path bytes, `/`-separated, relative to the tree root.
    pub path: Vec<u8>,
    /// Which POSIX file type this entry is.
    pub kind: EntryKind,
    /// Unix mode bits, as `lstat` reported them (type bits included).
    pub mode: u32,
    /// Owning UID, as `lstat` reported it — noise under a remapped user namespace, per
    /// ADR-009, but retained losslessly regardless.
    pub uid: u32,
    /// Owning GID.
    pub gid: u32,
    /// Modification time, seconds component.
    pub mtime_sec: i64,
    /// Modification time, nanoseconds component.
    pub mtime_nsec: u32,
    /// Inode number — noise, per ADR-009 and P1-02's own empirical finding, but retained
    /// losslessly regardless.
    pub inode: u64,
    /// Device major number. Only meaningful for `CharDevice`/`BlockDevice`; zero otherwise
    /// (this is how a whiteout's `major=0, minor=0` is represented).
    pub dev_major: u32,
    /// Device minor number. See `dev_major`.
    pub dev_minor: u32,
}

/// Evidence exactly as harvested, before any normalisation.
///
/// Shape is deferred to ADR-009 (F-07): the primary artifact is a *directory tree*, decoded
/// here into [`EvidenceEntry`] values in the same sorted-by-path order the wire format
/// itself uses. `#[non_exhaustive]` — constructed via [`Self::new`] rather than a struct
/// literal, so a future field (e.g. xattrs, once something downstream needs them) is not a
/// breaking change for every caller.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct RawEvidence {
    /// The decoded entries, in the order the capture's own sort produced.
    pub entries: Vec<EvidenceEntry>,
}

impl RawEvidence {
    /// Wrap already-decoded entries. Decoding the `evtree1` bytes themselves is I/O-adjacent
    /// (per ADR-009) and lives outside this crate — `observe::evtree::decode`, in practice.
    #[must_use]
    pub const fn new(entries: Vec<EvidenceEntry>) -> Self {
        Self { entries }
    }
}

/// A normalisation ruleset, already parsed — ADR-008's `ephemeral`/`server_internal` glob
/// lists (checked in that order; a path matching neither falls through to `user_state`),
/// plus a version tag for `VERDICT.ruleset_version` (architecture.md §6).
///
/// Passed in as a value rather than a path. That is the precondition that keeps
/// `normalise` free of I/O: the ruleset file lives in `rulesets/`, and parsing it is the
/// caller's job (today, `orchestrator::load_ruleset`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct Ruleset {
    /// This ruleset's version tag, e.g. `"v1"`.
    pub version: String,
    /// Glob patterns (ADR-008) whose match means "noise intrinsic to process execution,
    /// never diagnostic of this tool's behaviour" — checked before `server_internal_globs`.
    pub ephemeral_globs: Vec<String>,
    /// Glob patterns (ADR-008) whose match means "a conventional tool-owned state
    /// directory, by name" — checked after `ephemeral_globs`.
    pub server_internal_globs: Vec<String>,
}

impl Ruleset {
    /// Construct a ruleset from its already-parsed parts.
    #[must_use]
    pub const fn new(
        version: String,
        ephemeral_globs: Vec<String>,
        server_internal_globs: Vec<String>,
    ) -> Self {
        Self { version, ephemeral_globs, server_internal_globs }
    }
}

/// ADR-008's classification of one changeset path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PathTaxonomy {
    /// The default (ADR-008's conservative direction): a path not matched by either
    /// allowlist below.
    UserState,
    /// Matched a `server_internal` glob — a conventional tool-owned state directory.
    ServerInternal,
    /// Matched an `ephemeral` glob — noise intrinsic to process execution on Linux.
    Ephemeral,
}

/// One classified path in a canonical changeset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassifiedPath {
    /// The raw path bytes, exactly as captured.
    pub path: Vec<u8>,
    /// Its ADR-008 classification.
    pub taxonomy: PathTaxonomy,
}

/// A changeset after normalisation — the input to every verification protocol. Sorted by
/// raw path bytes, the same discipline ADR-009's own format already uses, so this stays an
/// order-independent function of its inputs regardless of the order evidence arrived in.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct CanonicalChangeset {
    /// Every entry from the source evidence, classified.
    pub entries: Vec<ClassifiedPath>,
}

impl CanonicalChangeset {
    /// Wrap already-classified, already-sorted entries.
    #[must_use]
    pub const fn new(entries: Vec<ClassifiedPath>) -> Self {
        Self { entries }
    }

    /// architecture.md §4.3: `readOnlyHint` is decided against the `user_state`-classified
    /// subset only. `true` means that subset is empty — consistent with a `true`
    /// declaration; `false` contradicts it.
    #[must_use]
    pub fn user_state_is_empty(&self) -> bool {
        !self.entries.iter().any(|entry| entry.taxonomy == PathTaxonomy::UserState)
    }
}

/// One connection destination exactly as `observe::connection_log` captured it via
/// `SO_ORIGINAL_DST` — decoded, but otherwise unfiltered, the network-evidence analogue of
/// [`EvidenceEntry`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ObservedDestination {
    /// The real destination IPv4 address, in network byte order (as `SO_ORIGINAL_DST`
    /// reports it), not a text form — this crate stays free of any string-formatting or
    /// parsing dependency for it.
    pub address: [u8; 4],
    /// The real destination port.
    pub port: u16,
}

impl ObservedDestination {
    /// Wrap an already-decoded destination.
    #[must_use]
    pub const fn new(address: [u8; 4], port: u16) -> Self {
        Self { address, port }
    }
}

/// An IPv4 subnet, expressed as a base address plus prefix length — e.g. `sandbox::netns`'s
/// veth bridge, `10.200.0.0/30`. Kept as a plain value here (not hardcoded in this crate)
/// specifically so `datamodel` never needs to know `sandbox`'s own choice of bridge
/// addresses; a caller (`normalise`'s destination classifier, in practice) supplies it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Ipv4Network {
    /// The subnet's base address, in network byte order.
    pub address: [u8; 4],
    /// The subnet's prefix length, `0..=32`.
    pub prefix_len: u8,
}

impl Ipv4Network {
    /// Wrap an already-known subnet.
    #[must_use]
    pub const fn new(address: [u8; 4], prefix_len: u8) -> Self {
        Self { address, prefix_len }
    }
}

/// P3-04: whether an observed connection destination is reachable only via this project's
/// own containment plumbing (loopback, or the sandbox's own veth bridge subnet) or
/// represents a genuine attempt to reach outside the sandbox.
///
/// This is the classification architecture.md §4.4's decision tree calls "in-sandbox versus
/// external" — the input `openWorldHint` (P3-05) needs to tell "the tool addressed its own
/// containment plumbing" apart from "the tool tried to leave," neither of which the raw
/// destination address alone distinguishes without knowing which addresses `sandbox::netns`
/// itself uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DestinationClass {
    /// Loopback (`127.0.0.0/8`) or the sandbox's own bridge subnet — addresses that exist
    /// only because of this project's own containment plumbing, not because the tool is
    /// trying to reach the outside world.
    InSandbox,
    /// Anything else — a genuine attempt at egress beyond the sandbox.
    External,
}

/// One classified connection destination, the network-evidence analogue of
/// [`ClassifiedPath`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ClassifiedDestination {
    /// The destination exactly as observed.
    pub destination: ObservedDestination,
    /// Its P3-04 classification.
    pub class: DestinationClass,
}

/// A SHA-256 content digest identifying a blob in the evidence store (F-05).
///
/// Pure value type: hashing needs an algorithm implementation, which is [`store`]'s job,
/// not this crate's — `datamodel` stays free of behaviour so `normalise` and `verdict`
/// stay `no_std` and dependency-free (ADR-005). This type only carries the 32 bytes and
/// knows how to print them; `EVIDENCE.digest` (architecture.md §6) is one of these.
///
/// [`store`]: ../../store/index.html
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Digest([u8; 32]);

impl Digest {
    /// Wrap an already-computed digest. Callers are responsible for the hashing itself.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The raw digest bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Parse [`Display`](core::fmt::Display)'s own output back into a `Digest` — the
    /// direction nothing needed until P5-02's offline derivation batch job, which reads a
    /// digest back out of `EVIDENCE.digest` (stored as text) and has to turn it back into
    /// the typed value [`store::BlobStore::get`] and [`store::object_store::ObjectStore::
    /// get`] both require. Deliberately stricter than a generic hex parser: only exactly
    /// 64 *lowercase* hex characters are accepted, matching both what [`Display`]
    /// (core::fmt::Display) ever produces and what `EVIDENCE.digest`'s own `CHECK`
    /// constraint (`crates/store/migrations/0001_initial_schema.sql`) already enforces —
    /// `None` for anything else, including a technically-valid uppercase hex string, rather
    /// than silently accepting a shape this type never itself produces.
    ///
    /// [`store`]: ../../store/index.html
    #[must_use]
    pub fn from_hex(s: &str) -> Option<Self> {
        let bytes = s.as_bytes();
        if bytes.len() != 64 {
            return None;
        }
        let mut out = [0u8; 32];
        for (i, slot) in out.iter_mut().enumerate() {
            let hi = hex_nibble(bytes[2 * i])?;
            let lo = hex_nibble(bytes[2 * i + 1])?;
            *slot = (hi << 4) | lo;
        }
        Some(Self(out))
    }
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

impl core::fmt::Display for Digest {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for byte in &self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;

    /// P2-11's own exit criterion, checked directly: every variant round-trips through
    /// `as_db_str`/`from_db_str` — the same property `Oracle`/`Outcome`/`Annotation` are
    /// each already held to. Enumerated explicitly (not derived from some external list)
    /// so adding a variant without adding it here fails to compile — this is the one
    /// exhaustiveness guard a closed taxonomy actually needs.
    #[test]
    fn every_reason_code_round_trips_through_its_db_string() {
        let all = [
            ReasonCode::ContainmentUncertain,
            ReasonCode::ExecutionTruncated,
            ReasonCode::Timeout,
            ReasonCode::CachingSuppressedInProcess,
            ReasonCode::NoProbeSurface,
            ReasonCode::InvocationFailed,
            ReasonCode::ProbeSurfaceIncomplete,
        ];
        for code in all {
            assert_eq!(ReasonCode::from_db_str(code.as_db_str()), Some(code));
        }
    }

    #[test]
    fn an_unrecognised_string_is_not_guessed_at() {
        assert_eq!(ReasonCode::from_db_str("something_from_a_future_build"), None);
    }

    #[test]
    fn display_matches_as_db_str() {
        assert_eq!(ReasonCode::Timeout.to_string(), "timeout");
    }

    #[test]
    fn digest_from_hex_round_trips_through_display() {
        let original = Digest::from_bytes([0xabu8; 32]);
        let rendered = original.to_string();
        assert_eq!(Digest::from_hex(&rendered), Some(original));
    }

    #[test]
    fn digest_from_hex_rejects_uppercase() {
        let hex = Digest::from_bytes([0xabu8; 32]).to_string().to_uppercase();
        assert_eq!(Digest::from_hex(&hex), None);
    }

    #[test]
    fn digest_from_hex_rejects_the_wrong_length() {
        assert_eq!(Digest::from_hex("abcd"), None);
        assert_eq!(Digest::from_hex(&"a".repeat(65)), None);
    }

    #[test]
    fn digest_from_hex_rejects_non_hex_characters() {
        assert_eq!(Digest::from_hex(&"g".repeat(64)), None);
    }

    #[test]
    fn every_embargo_state_round_trips_through_its_db_string() {
        let all = [EmbargoState::None, EmbargoState::Embargoed, EmbargoState::Disclosed];
        for state in all {
            assert_eq!(EmbargoState::from_db_str(state.as_db_str()), Some(state));
        }
    }

    #[test]
    fn embargo_state_as_db_str_matches_the_schemas_check_constraint() {
        assert_eq!(EmbargoState::None.as_db_str(), "none");
        assert_eq!(EmbargoState::Embargoed.as_db_str(), "embargoed");
        assert_eq!(EmbargoState::Disclosed.as_db_str(), "disclosed");
    }

    #[test]
    fn an_unrecognised_embargo_state_string_is_not_guessed_at() {
        assert_eq!(EmbargoState::from_db_str("cleared"), None);
    }
}
