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
/// Deliberately an open newtype for now. The closed taxonomy is P2-11 — *"`unverifiable`
/// without a reason is not a finding, it is a shrug"* — and fixing the variants before the
/// integrity gate and the idempotency protocol have run would be guessing at the very
/// codes the paper reports.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ReasonCode(pub String);

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
}

impl core::fmt::Display for Digest {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for byte in &self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}
