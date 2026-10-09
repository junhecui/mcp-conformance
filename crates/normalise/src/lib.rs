//! `(raw_evidence, ruleset) -> canonical_changeset`. Pure and deterministic.
//!
//! **Must not:** read anything outside its inputs — no clock, no filesystem, no network.
//!
//! `no_std` is load-bearing, not stylistic. It is what makes ADR-005 a property of the
//! build rather than a comment: there is no `std::fs` to call because `std` is not linked.
//! F-04's dependency-graph assertion is the outer layer; this is the inner one. P1-06 kept
//! it: path matching is a small hand-written matcher ([`glob`]), and `evtree` (which this
//! crate uses to decode the stored evidence) was made `no_std` + `alloc` and added to
//! `cargo purity`'s allowlist rather than letting `std` back in one edge away.
//!
//! The semantics implemented here are recorded in [ADR-011]; in brief:
//!
//! - **Inputs.** Both the overlay upper layer and the base (lower) layer arrive as `evtree1`
//!   bytes ([`RawEvidence`]). The base is a required second input: it is what distinguishes
//!   a directory copied up only to hold a changed child (structural, omitted) from a
//!   genuinely new or modified directory (a mutation), and a modified file from a new one.
//! - **Overlay semantics.** Whiteout (char device `0/0`) → [`ChangeKind::Deleted`]; a
//!   directory carrying a `trusted.overlay.opaque`/`user.overlay.opaque` xattr (any value)
//!   → [`ChangeKind::DirectoryReplaced`], and the base is treated as absent beneath it;
//!   anything else is compared against the base path: absent → `Created`, different type →
//!   `Replaced`, same type → `Modified`. A same-type entry is reported even if nothing
//!   observable differs (it was still copied up). The **only** entry ever omitted is a
//!   directory that exists as a directory in the base, with unchanged metadata, and that has
//!   at least one descendant in the upper layer — so every omission is backed by at least
//!   one reported descendant change.
//! - **Noise.** `mtime` and `inode` never enter a [`Change`] (kernel/run-time noise that
//!   two independent runs never agree on); the overlay's own bookkeeping xattrs are stripped
//!   from [`Node`]s, their meaning having been captured by [`ChangeKind`] — matched as the
//!   exact leaf names the kernel writes (`OVERLAY_PRIVATE_NAMES`) under either the
//!   `trusted.overlay.` or the `user.overlay.` namespace, never by prefix. Everything else is
//!   kept and compared verbatim, an unrecognised `user.overlay.*` name included: `user.*` is
//!   the namespace POSIX gives a file's owner, and the tool under test owns everything it
//!   creates in its own upper layer.
//! - **Classification.** ADR-008: `ephemeral` globs first, then `server_internal`, default
//!   `user_state`. A path with an empty, `.` or `..` component — which no real capture
//!   produces — is always `user_state`, so forged evidence cannot launder `/tmp/../home/x`
//!   into `ephemeral`.
//! - **Hostile input.** Total over every byte string: malformed evidence is a
//!   [`NormaliseError`], never a panic. Time is `O(S log S)` in the evidence size `S` (sorts
//!   and binary searches) plus `O(S · P)` for glob matching against a ruleset of size `P`;
//!   memory is `O(S)`.
//!
//! [ADR-011]: ../../../docs/adr/011-normaliser-semantics.md

#![no_std]

extern crate alloc;

pub mod glob;

use alloc::string::String;
use alloc::vec::Vec;
use core::cmp::Ordering;

use datamodel::{
    CanonicalChangeset, Change, ChangeKind, FileType, Node, PathClass, RawEvidence, Ruleset,
};
use evtree::{DecodeError, Entry, Payload};
use glob::{Glob, GlobError};

/// Why normalisation could not produce a changeset. Every variant means "this evidence or
/// ruleset is unusable", which a verdict must report as `unverifiable`, never as `holds`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NormaliseError {
    /// The upper-layer capture is not valid canonical `evtree1`.
    MalformedUpperLayer(DecodeError),
    /// The base-layer capture is not valid canonical `evtree1`.
    MalformedBaseLayer(DecodeError),
    /// A ruleset pattern failed to compile.
    InvalidPattern {
        /// The offending pattern, verbatim.
        pattern: String,
        /// Why it was rejected.
        error: GlobError,
    },
}

impl core::fmt::Display for NormaliseError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::MalformedUpperLayer(e) => write!(f, "malformed upper-layer evidence: {e}"),
            Self::MalformedBaseLayer(e) => write!(f, "malformed base-layer evidence: {e}"),
            Self::InvalidPattern { pattern, error } => {
                write!(f, "invalid ruleset pattern {pattern:?}: {error}")
            }
        }
    }
}

impl core::error::Error for NormaliseError {}

/// Classify a normalisation failure for the verdict engine (ADR-012 decision 5).
///
/// Three error variants map onto three distinct classifications, split by **whose fault the
/// failure is** rather than by where in the pipeline it surfaced: a malformed *upper* layer
/// is a finding about the tool, while a malformed *base* layer and an uncompilable ruleset
/// are both harness faults that must never be published as findings about a server.
///
/// This mapping lives here, next to the error it maps, because it is the only edge the
/// purity rule leaves open: `verdict` may not depend on `normalise` and `normalise` may not
/// depend on `verdict` (ADR-005 — neither is on `cargo purity`'s allowlist), so the shared
/// classification is [`datamodel::DerivationFailure`] and the derivation driver converts
/// through it. Having exactly one `From` impl is what stops two drivers from classifying the
/// same error differently.
impl From<&NormaliseError> for datamodel::DerivationFailure {
    fn from(error: &NormaliseError) -> Self {
        match error {
            // Hostile or corrupt stored evidence. A finding about the evidence, not an
            // internal error — the tool under test wrote the tree this was captured from.
            NormaliseError::MalformedUpperLayer(_) => Self::MalformedEvidence,
            // Operator-side, and deliberately *not* folded into the line above. The base
            // layer is built by the harness (`world::base_layer`, P1-02) and is mounted
            // read-only beneath the tool, which has no way to write to it, so a base layer
            // that will not decode is a harness fault — identical in kind to the
            // uncompilable ruleset below. Mapping it to `MalformedEvidence` published a
            // harness bug as a finding against a server, and, read the other way, handed
            // any server deniability for a real malformed capture.
            NormaliseError::MalformedBaseLayer(_) => Self::MalformedBaseLayer,
            // Operator-side: the harness was handed a ruleset it cannot apply. Says nothing
            // about the tool, and must not be reported as though it did.
            NormaliseError::InvalidPattern { .. } => Self::InvalidRuleset,
        }
    }
}

/// A ruleset with its patterns compiled. Compile once and reuse across many evidence
/// bundles (e.g. the whole verdict table during replay) via [`normalise_compiled`].
#[derive(Debug, Clone)]
pub struct CompiledRuleset {
    identity: String,
    ephemeral: Vec<Glob>,
    server_internal: Vec<Glob>,
}

impl CompiledRuleset {
    /// Compile every pattern in `ruleset`.
    ///
    /// # Errors
    ///
    /// [`NormaliseError::InvalidPattern`] naming the first pattern that fails.
    pub fn compile(ruleset: &Ruleset) -> Result<Self, NormaliseError> {
        let compile_all = |pats: &[String]| -> Result<Vec<Glob>, NormaliseError> {
            pats.iter()
                .map(|p| {
                    Glob::parse(p).map_err(|error| NormaliseError::InvalidPattern {
                        pattern: p.clone(),
                        error,
                    })
                })
                .collect()
        };
        Ok(Self {
            identity: ruleset.identity(),
            ephemeral: compile_all(&ruleset.ephemeral)?,
            server_internal: compile_all(&ruleset.server_internal)?,
        })
    }

    /// Classify an upper-layer-relative path (ADR-008).
    #[must_use]
    pub fn classify(&self, rel_path: &[u8]) -> PathClass {
        let comps: Vec<&[u8]> = rel_path.split(|&b| b == b'/').collect();
        // No real capture contains these; forged evidence must not use them to escape the
        // default. `""` also covers an empty path, a leading/trailing `/`, and `//`.
        // `Glob::matches_components` refuses them too (ADR-011 decision 8), so this is
        // belt-and-braces rather than the only place the property holds.
        if comps.iter().any(|c| c.is_empty() || *c == b"." || *c == b"..") {
            return PathClass::UserState;
        }
        if self.ephemeral.iter().any(|g| g.matches_components(&comps)) {
            PathClass::Ephemeral
        } else if self.server_internal.iter().any(|g| g.matches_components(&comps)) {
            PathClass::ServerInternal
        } else {
            PathClass::UserState
        }
    }
}

/// Apply a ruleset to raw evidence, yielding the canonical changeset that every
/// verification protocol is evaluated against.
///
/// The ruleset arrives already parsed. See [`datamodel::Ruleset`] for why.
///
/// # Errors
///
/// [`NormaliseError`] if either capture is malformed or a pattern is invalid.
pub fn normalise(
    evidence: &RawEvidence,
    ruleset: &Ruleset,
) -> Result<CanonicalChangeset, NormaliseError> {
    normalise_compiled(evidence, &CompiledRuleset::compile(ruleset)?)
}

/// [`normalise`] against an already-compiled ruleset.
///
/// # Errors
///
/// [`NormaliseError`] if either capture is malformed.
pub fn normalise_compiled(
    evidence: &RawEvidence,
    rules: &CompiledRuleset,
) -> Result<CanonicalChangeset, NormaliseError> {
    let base = evtree::decode(&evidence.base_layer).map_err(NormaliseError::MalformedBaseLayer)?;
    let mut upper =
        evtree::decode(&evidence.upper_layer).map_err(NormaliseError::MalformedUpperLayer)?;

    // Tree order: '/' ranks below every other byte, so each path is immediately followed by
    // its whole subtree. (Plain byte order puts `a-x` between `a` and `a/x`.) This is what
    // lets descendant and ancestor questions be answered in one linear pass below.
    upper.sort_by(|a, b| tree_cmp(&a.path, &b.path));

    let mut out = CanonicalChangeset {
        ruleset_identity: rules.identity.clone(),
        user_state: Vec::new(),
        server_internal: Vec::new(),
        ephemeral: Vec::new(),
    };

    // Ancestors of the current entry that are themselves upper-layer entries, each with
    // whether the base is hidden beneath it (an opaque directory at or above it). Every
    // entry is pushed once and popped at most once; each pop/peek costs one prefix check
    // bounded by the ancestor's path length, so the pass is linear in total path bytes.
    let mut stack: Vec<(usize, bool)> = Vec::new();

    for i in 0..upper.len() {
        while let Some(&(top, _)) = stack.last() {
            if is_descendant(&upper[i].path, &upper[top].path) {
                break;
            }
            stack.pop();
        }
        let base_hidden = stack.last().is_some_and(|&(_, hidden)| hidden);
        let has_descendant =
            upper.get(i + 1).is_some_and(|next| is_descendant(&next.path, &upper[i].path));

        let entry = &upper[i];
        let opaque = matches!(entry.payload, Payload::Directory) && is_opaque(entry);
        let base_entry = if base_hidden { None } else { lookup(&base, &entry.path) };

        let kind = classify_change(entry, base_entry, opaque, has_descendant);
        stack.push((i, base_hidden || opaque));

        if let Some(kind) = kind {
            let mut path = Vec::with_capacity(entry.path.len() + 1);
            path.push(b'/');
            path.extend_from_slice(&entry.path);
            let change = Change { path, kind };
            match rules.classify(&entry.path) {
                PathClass::UserState => out.user_state.push(change),
                PathClass::ServerInternal => out.server_internal.push(change),
                PathClass::Ephemeral => out.ephemeral.push(change),
            }
        }
    }

    // Deterministic, plain-byte order on output; paths are unique (evtree::decode rejects
    // duplicates), so this is a total order on each partition.
    for part in [&mut out.user_state, &mut out.server_internal, &mut out.ephemeral] {
        part.sort_by(|a, b| a.path.cmp(&b.path));
    }
    Ok(out)
}

/// `None` means structural: omitted from the changeset.
fn classify_change(
    entry: &Entry,
    base: Option<&Entry>,
    opaque: bool,
    has_descendant: bool,
) -> Option<ChangeKind> {
    if is_whiteout(entry) {
        return Some(ChangeKind::Deleted { was: base.map(|b| file_type(&b.payload)) });
    }
    let node = to_node(entry);
    if opaque {
        return Some(ChangeKind::DirectoryReplaced(node));
    }
    let Some(base) = base else {
        return Some(ChangeKind::Created(node));
    };
    if file_type(&base.payload) != node.file_type {
        return Some(ChangeKind::Replaced(node));
    }
    let content_changed = payload_data(&base.payload) != node.data.as_slice();
    let metadata_changed = !same_metadata(base, entry);
    if node.file_type == FileType::Directory && !metadata_changed && has_descendant {
        // Copied up only to contain a changed child. The child is reported on its own.
        return None;
    }
    Some(ChangeKind::Modified { node, content_changed, metadata_changed })
}

/// Everything a [`Node`] keeps except type and data: mode, ownership, device numbers and
/// non-overlay-private xattrs. `mtime` and `inode` are deliberately not compared.
fn same_metadata(a: &Entry, b: &Entry) -> bool {
    a.mode == b.mode
        && a.uid == b.uid
        && a.gid == b.gid
        && a.dev_major == b.dev_major
        && a.dev_minor == b.dev_minor
        && visible_xattrs(a).eq(visible_xattrs(b))
}

fn visible_xattrs(e: &Entry) -> impl Iterator<Item = (&[u8], &[u8])> {
    e.xattrs
        .iter()
        .filter(|x| !is_overlay_private(&x.name))
        .map(|x| (x.name.as_slice(), x.value.as_slice()))
}

/// Compare paths with `/` ranked below every other byte.
fn tree_cmp(a: &[u8], b: &[u8]) -> Ordering {
    let rank = |x: u8| if x == b'/' { 0u16 } else { u16::from(x) + 1 };
    for (&x, &y) in a.iter().zip(b) {
        match rank(x).cmp(&rank(y)) {
            Ordering::Equal => {}
            other => return other,
        }
    }
    a.len().cmp(&b.len())
}

/// Is `path` strictly inside `ancestor` (`ancestor/...`)?
fn is_descendant(path: &[u8], ancestor: &[u8]) -> bool {
    path.len() > ancestor.len() && path.starts_with(ancestor) && path[ancestor.len()] == b'/'
}

fn lookup<'a>(base: &'a [Entry], path: &[u8]) -> Option<&'a Entry> {
    // `evtree::decode` guarantees strictly ascending raw-byte order.
    base.binary_search_by(|e| e.path.as_slice().cmp(path)).ok().map(|i| &base[i])
}

/// The two namespaces overlayfs keeps its own bookkeeping in: `trusted.overlay.` under a
/// privileged mount, `user.overlay.` under `userxattr` (ADR-010).
const OVERLAY_NAMESPACES: [&[u8]; 2] = [b"trusted.overlay.", b"user.overlay."];

/// The finite set of leaf names the kernel itself writes inside those namespaces.
///
/// Matched **exactly**, never by prefix. `user.*` is the namespace POSIX gives a file's
/// owner, and the tool under test owns every file it creates in the upper layer, so it can
/// set `user.overlay.<anything>` itself — verified on a live overlay in trusted mode, not
/// only under `userxattr` (ADR-011 decision 5). Stripping the whole prefix would hand a
/// hostile tool a write that is deleted from the [`Node`] *and* ignored by `same_metadata`,
/// leaving a copied-up directory with `metadata_changed == false` and therefore eligible for
/// the structural-omission rule: the payload would land in no partition at all and
/// `readOnlyHint` would read as `holds`. An unrecognised name is an ordinary xattr and is
/// reported like any other.
const OVERLAY_PRIVATE_NAMES: [&[u8]; 9] = [
    b"impure",
    b"metacopy",
    b"nlink",
    b"opaque",
    b"origin",
    b"protattr",
    b"redirect",
    b"upper",
    b"uuid",
];

fn is_overlay_private(name: &[u8]) -> bool {
    OVERLAY_NAMESPACES
        .iter()
        .any(|ns| name.strip_prefix(*ns).is_some_and(|leaf| OVERLAY_PRIVATE_NAMES.contains(&leaf)))
}

fn is_opaque(entry: &Entry) -> bool {
    entry
        .xattrs
        .iter()
        .any(|x| x.name == b"trusted.overlay.opaque" || x.name == b"user.overlay.opaque")
}

fn is_whiteout(entry: &Entry) -> bool {
    matches!(entry.payload, Payload::CharDevice) && entry.dev_major == 0 && entry.dev_minor == 0
}

fn file_type(payload: &Payload) -> FileType {
    match payload {
        Payload::Regular(_) => FileType::Regular,
        Payload::Directory => FileType::Directory,
        Payload::Symlink(_) => FileType::Symlink,
        Payload::Fifo => FileType::Fifo,
        Payload::CharDevice => FileType::CharDevice,
        Payload::BlockDevice => FileType::BlockDevice,
        Payload::Socket => FileType::Socket,
    }
}

fn payload_data(payload: &Payload) -> &[u8] {
    match payload {
        Payload::Regular(bytes) | Payload::Symlink(bytes) => bytes,
        _ => &[],
    }
}

fn to_node(entry: &Entry) -> Node {
    Node {
        file_type: file_type(&entry.payload),
        mode: entry.mode,
        uid: entry.uid,
        gid: entry.gid,
        dev_major: entry.dev_major,
        dev_minor: entry.dev_minor,
        // Already sorted by name: evtree::decode rejects unsorted xattrs.
        xattrs: entry
            .xattrs
            .iter()
            .filter(|x| !is_overlay_private(&x.name))
            .map(|x| (x.name.clone(), x.value.clone()))
            .collect(),
        data: payload_data(&entry.payload).to_vec(),
    }
}

#[cfg(test)]
mod tests;
