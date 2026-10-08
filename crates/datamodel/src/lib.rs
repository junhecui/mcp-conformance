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

/// Evidence exactly as harvested, before any normalisation — the input to `normalise`.
///
/// Both fields are `evtree1` byte strings (ADR-009), the exact bytes the evidence store
/// (F-05) holds under each blob's digest. Keeping them as bytes rather than decoded entries
/// means replay (P1-09) is literally `normalise(stored bytes, ruleset)` and that `normalise`
/// is total over *every* byte string, not just over well-formed trees: decoding happens
/// inside the pure closure, where a malformed blob becomes a typed error rather than a
/// panic somewhere upstream (ADR-011).
///
/// **Capture is lossless** — mtimes and inode data are noise but are still stored, because
/// discarding them at capture time is normalisation, and ADR-005 requires normalisation to
/// be a pure function of stored evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawEvidence {
    /// The `evtree1` capture of the overlay's **lower** (base) directory, as mounted — the
    /// byte-reproducible world P1-02/P2-05 built. It is the reference every upper-layer
    /// entry is compared against (ADR-011 decision b): without it a directory copied up only
    /// to hold a changed child is indistinguishable from a genuinely new one. Must be
    /// captured in the same way and in the same ID view as `upper_layer`; any mismatch
    /// between the two makes a tool look *less* read-only, never more.
    pub base_layer: Vec<u8>,
    /// The `evtree1` capture of the overlay **upper** layer after the run, read from the
    /// host side (ADR-009) — the kernel-provided changeset.
    pub upper_layer: Vec<u8>,
}

/// A normalisation ruleset, already parsed.
///
/// Passed in as a value rather than a path. That is the precondition that keeps
/// `normalise` free of I/O: the ruleset file lives in `rulesets/`, and parsing it is the
/// caller's job (`store::ruleset::load`).
///
/// Ruleset v1 carries only ADR-008's two glob allowlists. Matching order is `ephemeral`
/// first, then `server_internal`; anything unmatched is `user_state` (ADR-008).
///
/// **Identity is tamper-evident.** [`Self::source_digest`] is the SHA-256 of the exact
/// ruleset file bytes, computed by the loader — never supplied by whoever wrote the file —
/// and the loader refuses a published version label whose bytes no longer hash to the
/// digest registered for it. [`Self::identity`] (label *and* digest) is what a verdict
/// should record, so an edited ruleset can never masquerade under an old version's name.
/// This crate cannot hash (that would pull an algorithm into the pure closure); a
/// `Ruleset` built by hand, e.g. in a test, carries whatever digest its author chose and is
/// attested by nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ruleset {
    /// Human-readable version label, e.g. `"v1"`.
    pub version: String,
    /// SHA-256 of the ruleset file's exact bytes.
    pub source_digest: Digest,
    /// Glob patterns classifying a path as [`PathClass::Ephemeral`]. Checked first.
    pub ephemeral: Vec<String>,
    /// Glob patterns classifying a path as [`PathClass::ServerInternal`].
    pub server_internal: Vec<String>,
}

impl Ruleset {
    /// `"<version>+sha256:<hex digest>"` — the tamper-evident identity to record against
    /// anything derived under this ruleset.
    #[must_use]
    pub fn identity(&self) -> String {
        alloc::format!("{}+sha256:{}", self.version, self.source_digest)
    }
}

/// ADR-008's path taxonomy. `UserState` is the default for anything a ruleset does not
/// match — the conservative direction, since it makes a tool look *less* read-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PathClass {
    /// Anything not matched by a ruleset allowlist. `readOnlyHint` is decided against this.
    UserState,
    /// Conventional tool-owned state (caches, config, `__pycache__`, ...).
    ServerInternal,
    /// Noise intrinsic to process execution (`/tmp`, lock/pid/socket files, ...).
    Ephemeral,
}

/// The POSIX file type of a changed node — the seven types `evtree1` distinguishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FileType {
    /// Regular file.
    Regular,
    /// Directory.
    Directory,
    /// Symbolic link.
    Symlink,
    /// Named pipe.
    Fifo,
    /// Character device (other than an overlay whiteout, which becomes
    /// [`ChangeKind::Deleted`]).
    CharDevice,
    /// Block device.
    BlockDevice,
    /// Unix domain socket.
    Socket,
}

/// The state of a node as the upper layer records it after normalisation.
///
/// Deliberately **excludes** `mtime` and `inode` (kernel-assigned / run-time noise: two
/// independent runs never agree on them, so keeping them would put every touched path in
/// the noise floor `D1 Δ D1′`), and the overlay's own bookkeeping xattrs — the exact leaf
/// names the kernel writes (`opaque`, `impure`, `origin`, `uuid`, `redirect`, `nlink`,
/// `upper`, `metacopy`, `protattr`) under `trusted.overlay.` or `user.overlay.`, whose
/// meaning is captured by [`ChangeKind`] instead. Matched by **name, never by prefix**:
/// `user.*` is the namespace POSIX gives a file's owner and the tool under test owns its own
/// upper layer, so an unrecognised `user.overlay.*` name is an ordinary xattr and is kept
/// (ADR-011 decision 5). Everything else is kept verbatim, so two changes compare equal only
/// if the resulting state is identical.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Node {
    /// POSIX file type.
    pub file_type: FileType,
    /// `st_mode`, verbatim.
    pub mode: u32,
    /// Owning user ID.
    pub uid: u32,
    /// Owning group ID.
    pub gid: u32,
    /// Device major number (devices only; `0` otherwise).
    pub dev_major: u32,
    /// Device minor number (devices only; `0` otherwise).
    pub dev_minor: u32,
    /// Non-overlay-private xattrs as `(name, value)`, sorted by name.
    pub xattrs: Vec<(Vec<u8>, Vec<u8>)>,
    /// Full content for a regular file, the target for a symlink, empty otherwise. Exact
    /// bytes rather than a digest: equality is all P2-08's set comparison needs, and a hash
    /// would pull a digest algorithm into the pure closure. Storage-side digests are the
    /// evidence store's job.
    pub data: Vec<u8>,
}

/// What happened at one path, relative to the base layer (ADR-011).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ChangeKind {
    /// Nothing existed at this path in the base layer (or an ancestor was replaced by an
    /// opaque directory, hiding the base).
    Created(Node),
    /// A node of the same type existed in the base. Both flags may be `false`: the node was
    /// still copied up (opened for writing, timestamps set, ...) and presence in the upper
    /// layer is itself evidence of a write, so it is reported rather than dropped.
    Modified {
        /// The resulting node.
        node: Node,
        /// Content (regular file) or target (symlink) differs from the base.
        content_changed: bool,
        /// Mode, ownership, device numbers or non-overlay xattrs differ from the base.
        metadata_changed: bool,
    },
    /// A node of a *different* type existed in the base.
    Replaced(Node),
    /// An overlay whiteout (char device `0/0`): the base entry was deleted. `was` is the base
    /// node's type, or `None` if the base had nothing at this path (anomalous; still
    /// reported, since a whiteout is never structural).
    Deleted {
        /// The type of what was deleted, if the base layer had it.
        was: Option<FileType>,
    },
    /// An opaque directory (`*.overlay.opaque` xattr present): the directory's base contents
    /// are hidden wholesale — the directory was removed and recreated.
    DirectoryReplaced(Node),
}

/// One normalised change.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Change {
    /// Absolute path inside the sandbox (`/` + the upper-layer-relative path), raw bytes.
    pub path: Vec<u8>,
    /// What happened there.
    pub kind: ChangeKind,
}

/// A changeset after normalisation — the input to every verification protocol.
///
/// Partitioned by ADR-008's taxonomy; each partition is sorted by raw path bytes and paths
/// are unique, so two changesets derived from equal inputs are equal, and `Change: Ord`
/// lets P2-08 compute `D1 Δ D1′` as a plain set difference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalChangeset {
    /// [`Ruleset::identity`] of the ruleset this was derived under.
    pub ruleset_identity: String,
    /// Changes `readOnlyHint` is decided against (ADR-008).
    pub user_state: Vec<Change>,
    /// Reported alongside the verdict, never discarded.
    pub server_internal: Vec<Change>,
    /// Reported alongside the verdict, never discarded.
    pub ephemeral: Vec<Change>,
}

impl CanonicalChangeset {
    /// The partition for one class.
    #[must_use]
    pub fn class(&self, class: PathClass) -> &[Change] {
        match class {
            PathClass::UserState => &self.user_state,
            PathClass::ServerInternal => &self.server_internal,
            PathClass::Ephemeral => &self.ephemeral,
        }
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

    /// The inverse of this type's `Display`: exactly 64 lowercase hex characters, nothing
    /// else. `None` for anything that isn't — including uppercase, surrounding whitespace,
    /// or a `0x` prefix.
    ///
    /// Strict on purpose. A digest read back out of a results file becomes a path inside the
    /// evidence store (F-05 addresses blobs by `Display` form), so accepting exactly the
    /// alphabet `Display` emits is what makes "a digest string can never name a path outside
    /// the store" a property of the type rather than of every caller's validation.
    #[must_use]
    pub fn from_hex(s: &str) -> Option<Self> {
        fn nibble(c: u8) -> Option<u8> {
            match c {
                b'0'..=b'9' => Some(c - b'0'),
                b'a'..=b'f' => Some(c - b'a' + 10),
                _ => None,
            }
        }
        let bytes = s.as_bytes();
        if bytes.len() != 64 {
            return None;
        }
        let mut out = [0u8; 32];
        for (i, pair) in bytes.chunks_exact(2).enumerate() {
            out[i] = (nibble(pair[0])? << 4) | nibble(pair[1])?;
        }
        Some(Self(out))
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
    use super::Digest;
    use alloc::string::ToString;

    #[test]
    fn from_hex_inverts_display() {
        let mut bytes = [0u8; 32];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(37);
        }
        let digest = Digest::from_bytes(bytes);
        assert_eq!(Digest::from_hex(&digest.to_string()), Some(digest));
    }

    #[test]
    fn from_hex_rejects_anything_display_would_not_emit() {
        let valid = "ab".repeat(32);
        assert!(Digest::from_hex(&valid).is_some());
        assert_eq!(Digest::from_hex(&valid.to_uppercase()), None, "uppercase");
        assert_eq!(Digest::from_hex(&valid[..62]), None, "short");
        assert_eq!(Digest::from_hex(&alloc::format!("{valid}00")), None, "long");
        assert_eq!(Digest::from_hex(&alloc::format!("../{}", &valid[3..])), None, "path");
        assert_eq!(Digest::from_hex(&alloc::format!(" {}", &valid[1..])), None, "whitespace");
        assert_eq!(Digest::from_hex(""), None, "empty");
    }
}
