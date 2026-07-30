//! The overlayfs base (lower) layer builder — P1-02.
//!
//! Constructs the directory tree a later overlay mount (P1-03) will sit on top of, from a
//! small declarative [`BaseLayerSpec`], and proves that building it twice from an identical
//! spec produces byte-identical [`evtree`] output. architecture.md §12 item 5: "everything
//! downstream depends on it" — a nondeterministic base would silently poison every diff a
//! later tool invocation produces.
//!
//! This module deliberately does **not** attempt P2-05's job ("World provisioner — generic
//! fixtures": seeded DB, mock backends, per-server fixture binding). It builds and proves
//! reproducibility for a plain seeded filesystem, which is what P1-02's exit criterion asks
//! for; P2-05 is free to grow a richer spec type on top of this one later.
//!
//! # Design: two ways to prove reproducibility, and why both exist
//!
//! 1. **[`BaseLayerSpec::to_entries`] — pure, in-memory, no ambient state at all.** Every
//!    field of every produced [`evtree::Entry`] comes directly from the spec or from a fixed
//!    constant (`uid = 0`, `gid = 0`, `mtime = 0`, `inode = 0`). Two calls to `to_entries`
//!    against logically-identical specs, built via unrelated code paths (different
//!    insertion order, different call sites), are provably byte-identical after
//!    [`evtree::encode`] — see the tests below. This is the strongest, cheapest form of the
//!    proof, and the one that would catch a regression like accidental `HashMap` iteration
//!    order or a stray timestamp read.
//! 2. **[`BaseLayerSpec::materialize`] + [`capture`] — real files on real disk.** P1-03
//!    needs an actual directory to bind-mount an overlay onto, so this module also proves
//!    the *realistic* path: materialise the spec twice, into two independent temp
//!    directories, walk each with `lstat`, and compare. See "Judgment call" below for the
//!    one field this comparison cannot use as-is.
//!
//! # Judgment call: inode numbers are not part of the reproducibility claim
//!
//! [`evtree::Entry::inode`] is real data the kernel assigns at file-creation time. No
//! operation available to an unprivileged process can choose or predict it, on any POSIX
//! filesystem, ever — unlike `mtime` (see below, which this builder *does* pin), inode has
//! no userspace override at all. This means two **independently constructed** real directory
//! trees — even from a byte-for-byte identical spec, even in the same process, back to back
//! — will almost certainly disagree on inode numbers. That is not a bug in the builder; it
//! is what "inode" means.
//!
//! ADR-009's own reproducibility claim ("capturing the *same* on-disk tree twice ... without
//! modification in between") sidesteps this entirely, because both captures see the same
//! files and therefore the same inodes — and this module proves that narrower claim too, in
//! [`same_tree_captured_twice_without_rebuilding_is_exactly_identical`]. P1-02's own exit
//! criterion is the stronger one ("two *independent constructions*"), which is a claim
//! about the *builder*, not about the filesystem's inode allocator — so this module compares
//! independently-constructed trees only after `strip_construction_noise` removes the one
//! field the builder genuinely cannot control. A reviewer should treat this as the central
//! judgment call in this file.
//!
//! `mtime` is **not** part of that same excuse, and the module previously claimed it was for
//! the wrong reason: `std::fs::File::set_times` / `std::fs::FileTimes::set_modified` have
//! been stable since Rust 1.75 — well inside this project's pinned 1.85.1 toolchain
//! (`rust-toolchain.toml`) — so there never was a missing-stable-API justification for
//! leaving it wall-clock. [`BaseLayerSpec::materialize`] now pins every regular file's and
//! every directory's `mtime` to a fixed value (`SystemTime::UNIX_EPOCH`, matching
//! [`BaseLayerSpec::to_entries`]'s `mtime_sec = 0` / `mtime_nsec = 0` constants for the
//! in-memory path) once all content at that path has been written. Directory pinning
//! specifically happens in a *second pass*, after every `mkdir`/file-write/symlink call in
//! the function has already run: the kernel bumps a directory's own mtime every time an
//! entry is added to it, so pinning a directory's mtime inline — in the same pass that is
//! still populating it with children — would just be overwritten by the next child's
//! creation.
//!
//! The one entry type left genuinely unpinned is **symlinks**: changing a symlink's *own*
//! mtime (as opposed to the mtime of whatever it points to) needs `lutimes`/`utimensat`
//! with `AT_SYMLINK_NOFOLLOW` semantics, and `std::fs::File::set_times` has no way to
//! request that — opening a symlink via `File::open` follows it to its target, so there is
//! no stable `std` path to a file handle representing the link itself. A freshly
//! materialised symlink therefore still carries the wall-clock time of its own creation,
//! and `strip_construction_noise` still normalises mtime for that one entry type, and only
//! that one, for exactly this reason — see that function's doc comment.
//!
//! # Overlayfs semantics (design.md §9, ADR-009), and why they show up here at all
//!
//! The *base/lower* layer this module builds does not, in the ordinary case, contain
//! whiteouts or opaque directories — those are properties of an *upper* layer relative to
//! what it shadows, which is P1-04's evidence, not P1-02's input. [`SpecNode::whiteout`] and
//! [`SpecNode::opaque_dir`] exist anyway, for two reasons: (1) the P1-02 backlog entry
//! explicitly lists "whiteout and opaque-directory semantics understood and documented" as
//! an exit-criterion checklist item, and the sharpest way to demonstrate understanding is a
//! working, tested representation, not prose alone; (2) a bespoke fixture that itself needs
//! to seed a nested-overlay scenario, or a regression test for the format, has a legitimate
//! reason to construct one. Neither is ever written to real disk by [`materialize`] — see
//! that function's docs for why.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::io::Write as _;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::symlink;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::Path;
use std::time::SystemTime;

use datamodel::Digest;
use evtree::{Entry, Payload, XAttr};
use sha2::{Digest as _, Sha256};

const S_IFDIR: u32 = 0o040_000;
const S_IFREG: u32 = 0o100_000;
const S_IFLNK: u32 = 0o120_000;
const S_IFCHR: u32 = 0o020_000;

/// One filesystem object as declared in a [`BaseLayerSpec`], before construction.
///
/// Deliberately smaller than [`evtree::Payload`] in one respect (no FIFO/block-device/
/// socket variants) and larger in another (mode/content/xattrs live here, not split
/// across the `Entry`'s flat field list) — this is the *authoring* shape a fixture writer
/// reasons about; [`BaseLayerSpec::to_entries`] is what lowers it to the wire shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpecNode {
    /// A directory. `mode` is permission bits only (e.g. `0o755`), never POSIX type bits —
    /// [`BaseLayerSpec::to_entries`] adds those.
    Directory {
        /// Permission bits, e.g. `0o755`.
        mode: u32,
        /// Extended attributes to attach, name/value pairs. Empty for an ordinary
        /// directory; see [`SpecNode::opaque_dir`] for the overlay-opaque case.
        xattrs: Vec<(Vec<u8>, Vec<u8>)>,
    },
    /// A regular file with inline content.
    Regular {
        /// Permission bits, e.g. `0o644`.
        mode: u32,
        /// Complete file content.
        content: Vec<u8>,
        /// Extended attributes to attach, name/value pairs.
        xattrs: Vec<(Vec<u8>, Vec<u8>)>,
    },
    /// A symbolic link. `target` is not validated to exist — dangling symlinks are valid.
    Symlink {
        /// Permission bits. On the platforms this module targets, a freshly created
        /// symlink's permission bits are fixed by the OS (conventionally `0o777`) and not
        /// independently `chmod`-able without `lchmod`, which `std` does not expose; this
        /// field exists for format completeness and is not enforced against reality by
        /// [`BaseLayerSpec::materialize`].
        mode: u32,
        /// Link target, raw bytes (not required to resolve to anything).
        target: Vec<u8>,
    },
    /// A synthetic character-device declaration. **Never materialised on real disk** by
    /// [`BaseLayerSpec::materialize`] — creating a real device node needs `CAP_MKNOD` (in
    /// practice, root), which this builder does not require or assume. Exists purely so a
    /// spec can express — and this module can prove it encodes correctly — the two
    /// overlayfs-specific shapes named in design.md §9: see [`SpecNode::whiteout`] and
    /// [`SpecNode::opaque_dir`].
    CharDevice {
        /// Device major number.
        major: u32,
        /// Device minor number.
        minor: u32,
    },
}

impl SpecNode {
    /// A directory with the conventional default mode `0o755` and no xattrs.
    #[must_use]
    pub fn dir() -> Self {
        Self::Directory { mode: 0o755, xattrs: Vec::new() }
    }

    /// A directory with an explicit mode and no xattrs.
    #[must_use]
    pub fn dir_mode(mode: u32) -> Self {
        Self::Directory { mode, xattrs: Vec::new() }
    }

    /// A regular file, mode `0o644`, with the given content.
    #[must_use]
    pub fn file(content: impl Into<Vec<u8>>) -> Self {
        Self::Regular { mode: 0o644, content: content.into(), xattrs: Vec::new() }
    }

    /// A regular file with an explicit mode.
    #[must_use]
    pub fn file_mode(mode: u32, content: impl Into<Vec<u8>>) -> Self {
        Self::Regular { mode, content: content.into(), xattrs: Vec::new() }
    }

    /// A symbolic link to `target`, mode `0o777` (see [`SpecNode::Symlink`]'s field docs
    /// for why that value is the only one this module actually enforces on real disk).
    #[must_use]
    pub fn symlink(target: impl Into<Vec<u8>>) -> Self {
        Self::Symlink { mode: 0o777, target: target.into() }
    }

    /// An overlayfs whiteout: a character-device entry at device `0/0`, mode `0`
    /// (design.md §9, ADR-009). See the module docs for why this is representable but
    /// never materialised for real.
    #[must_use]
    pub fn whiteout() -> Self {
        Self::CharDevice { major: 0, minor: 0 }
    }

    /// An overlayfs opaque directory: an ordinary directory carrying
    /// `trusted.overlay.opaque=y` (or `user.overlay.opaque=y` under the `userxattr` mount
    /// option, ADR-010 — see [`SpecNode::opaque_dir_userxattr`]).
    #[must_use]
    pub fn opaque_dir() -> Self {
        Self::Directory {
            mode: 0o755,
            xattrs: vec![(b"trusted.overlay.opaque".to_vec(), b"y".to_vec())],
        }
    }

    /// The `userxattr`-mount-option form of [`SpecNode::opaque_dir`].
    #[must_use]
    pub fn opaque_dir_userxattr() -> Self {
        Self::Directory {
            mode: 0o755,
            xattrs: vec![(b"user.overlay.opaque".to_vec(), b"y".to_vec())],
        }
    }
}

/// A declarative specification for a base (lower) layer directory tree.
///
/// Backed by a `BTreeMap` keyed on raw path bytes, never a `HashMap` — this is the single
/// biggest lever against the "no random ordering" requirement: iteration order is the sort
/// order by construction, with no separate step that could be forgotten. [`evtree::encode`]
/// re-sorts defensively regardless, so this is belt-and-braces, not the only thing standing
/// between this module and nondeterminism.
#[derive(Debug, Clone, Default)]
pub struct BaseLayerSpec {
    nodes: BTreeMap<Vec<u8>, SpecNode>,
}

/// Reject a path that [`BaseLayerSpec::add`] cannot safely accept: empty, absolute, or
/// containing a `..` component anywhere in it. See [`BaseLayerSpec::add`]'s `# Panics`
/// section for why each of these is rejected rather than sanitised or silently accepted.
///
/// Panics (matching this builder's existing convention of `assert!`-style rejection for
/// caller misuse, e.g. the pre-existing empty-path check this function absorbed) rather
/// than returning a `Result`: every other invalid-input case in this builder's public API
/// is already a panic, and a spec is built once, programmatically, by the harness itself —
/// not parsed from untrusted wire input at this layer — so there is no caller here that
/// would ever want to recover from a malformed path rather than fix the bug that produced
/// it.
fn validate_relative_path(path: &[u8]) {
    assert!(!path.is_empty(), "BaseLayerSpec::add: path must not be empty (root is not an entry)");
    assert!(
        path[0] != b'/',
        "BaseLayerSpec::add: path must be relative, not absolute: {:?}",
        String::from_utf8_lossy(path)
    );
    assert!(
        path.split(|&b| b == b'/').all(|segment| segment != b".."),
        "BaseLayerSpec::add: path must not contain a '..' component: {:?}",
        String::from_utf8_lossy(path)
    );
    // Empty components (`a//b`, a trailing `/`) and `.` components (`a/./b`) are rejected
    // too: they materialize/capture at a *different* path string than the spec key (the
    // kernel collapses them), so two distinct spec keys could silently overwrite one
    // on-disk path, and the pure `to_entries()` proof path would no longer describe the
    // same tree the real-disk `capture()` proof path sees.
    assert!(
        path.split(|&b| b == b'/').all(|segment| !segment.is_empty() && segment != b"."),
        "BaseLayerSpec::add: path must not contain empty or '.' components: {:?}",
        String::from_utf8_lossy(path)
    );
}

impl BaseLayerSpec {
    /// An empty spec.
    #[must_use]
    pub fn new() -> Self {
        Self { nodes: BTreeMap::new() }
    }

    /// Declare one filesystem object at `path` (raw bytes, `/`-separated, relative to the
    /// tree root — never starting with `/`).
    ///
    /// Any ancestor directory not already present is synthesised with
    /// [`SpecNode::dir`]'s default mode, so a caller adding `"a/b/c.txt"` to an otherwise
    /// empty spec does not also have to separately declare `"a"` and `"a/b"`. Declare an
    /// ancestor explicitly (in either order relative to its descendants) to override that
    /// default — the last write for a given path always wins, matching ordinary map
    /// semantics.
    ///
    /// # Panics
    ///
    /// - If `path` is empty. The tree root is never itself an entry (`evtree`'s own
    ///   convention); there is nothing meaningful to declare at it.
    /// - If `path` is absolute (starts with `/`). [`BaseLayerSpec::materialize`] joins
    ///   `path` onto a destination directory; an absolute path would make `Path::join`
    ///   discard that destination entirely and write outside the intended tree.
    /// - If `path` contains a `..` component, anywhere — not just a leading one. `Path::join`
    ///   does not normalise `..`, so an unchecked one would let `materialize` write past
    ///   `dest` at the real filesystem layer, a genuine arbitrary-file-write primitive once
    ///   any less-trusted source (a per-server fixture, say — see the module docs) can
    ///   construct a spec. Rejected here, structurally, rather than left as something a
    ///   caller has to remember to sanitise first.
    pub fn add(&mut self, path: impl Into<Vec<u8>>, node: SpecNode) -> &mut Self {
        let path = path.into();
        validate_relative_path(&path);
        self.ensure_parents(&path);
        self.nodes.insert(path, node);
        self
    }

    fn ensure_parents(&mut self, path: &[u8]) {
        let mut start = 0;
        while let Some(offset) = path[start..].iter().position(|&b| b == b'/') {
            let end = start + offset;
            if end > 0 {
                let ancestor = path[..end].to_vec();
                self.nodes.entry(ancestor).or_insert_with(SpecNode::dir);
            }
            start = end + 1;
        }
    }

    /// How many entries this spec declares (after ancestor synthesis).
    #[must_use]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Whether this spec declares no entries at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Lower this spec directly to [`evtree::Entry`] values — no filesystem I/O.
    ///
    /// Every field is either taken verbatim from the spec or set to a fixed constant
    /// (`uid = 0`, `gid = 0`, `mtime = 0`, `inode = 0`): nothing here reads a clock, an
    /// environment variable, or any other ambient state, which is what makes this
    /// transformation reproducible by construction rather than by convention.
    #[must_use]
    pub fn to_entries(&self) -> Vec<Entry> {
        self.nodes.iter().map(|(path, node)| Self::entry_for(path, node)).collect()
    }

    fn entry_for(path: &[u8], node: &SpecNode) -> Entry {
        let (mode, dev_major, dev_minor, xattrs, payload) = match node {
            SpecNode::Directory { mode, xattrs } => {
                (S_IFDIR | (mode & 0o7777), 0, 0, xattrs.clone(), Payload::Directory)
            }
            SpecNode::Regular { mode, content, xattrs } => (
                S_IFREG | (mode & 0o7777),
                0,
                0,
                xattrs.clone(),
                Payload::Regular(content.clone()),
            ),
            SpecNode::Symlink { mode, target } => {
                (S_IFLNK | (mode & 0o7777), 0, 0, Vec::new(), Payload::Symlink(target.clone()))
            }
            SpecNode::CharDevice { major, minor } => (S_IFCHR, *major, *minor, Vec::new(), Payload::CharDevice),
        };
        Entry {
            path: path.to_vec(),
            mode,
            uid: 0,
            gid: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            inode: 0,
            dev_major,
            dev_minor,
            xattrs: xattrs.into_iter().map(|(name, value)| XAttr { name, value }).collect(),
            payload,
        }
    }

    /// Materialise this spec onto real disk at `dest` (which must already exist).
    ///
    /// [`SpecNode::CharDevice`] entries (including [`SpecNode::whiteout`]) are silently
    /// skipped — not an error — because creating a real device-special file needs
    /// `CAP_MKNOD`, which this builder neither has nor wants to require. A spec containing
    /// one remains fully valid for [`BaseLayerSpec::to_entries`]; only the real-disk
    /// projection is necessarily partial. Xattrs declared on other node types are **not**
    /// written to real disk either (no `setxattr` call here) — real xattr I/O is deferred to
    /// whichever future component needs it (a candidate for the P2-05 world provisioner,
    /// once it exists); this builder's own reproducibility proof does not depend on it.
    ///
    /// Every regular file's and every directory's `mtime` is pinned to
    /// `SystemTime::UNIX_EPOCH` (module docs' "Judgment call" section explains why, and why
    /// symlinks are the one exception). Directories are pinned in a second pass, after every
    /// `mkdir`/file-write/symlink call below has already run — see that same section for why
    /// pinning inline, in the pass that is still populating a directory with children,
    /// would not stick.
    ///
    /// # Errors
    ///
    /// Any underlying filesystem operation failing.
    pub fn materialize(&self, dest: &Path) -> io::Result<()> {
        for (path, node) in &self.nodes {
            let target = dest.join(OsStr::from_bytes(path));
            debug_assert!(
                target.starts_with(dest),
                "materialize: computed target {target:?} for spec path {path:?} escapes \
                 dest {dest:?} — this should be unreachable, since BaseLayerSpec::add \
                 rejects absolute and '..'-containing paths at insertion time"
            );
            match node {
                SpecNode::Directory { mode, .. } => {
                    fs::create_dir_all(&target)?;
                    fs::set_permissions(&target, fs::Permissions::from_mode(*mode))?;
                }
                SpecNode::Regular { mode, content, .. } => {
                    if let Some(parent) = target.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    let mut file = fs::File::create(&target)?;
                    file.write_all(content)?;
                    file.set_times(fs::FileTimes::new().set_modified(SystemTime::UNIX_EPOCH))?;
                    drop(file);
                    fs::set_permissions(&target, fs::Permissions::from_mode(*mode))?;
                }
                SpecNode::Symlink { target: link_target, .. } => {
                    if let Some(parent) = target.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    symlink(OsStr::from_bytes(link_target), &target)?;
                    // mtime deliberately left wall-clock — see module docs' "Judgment call"
                    // section for why a symlink's own mtime cannot be pinned via stable std.
                }
                SpecNode::CharDevice { .. } => {
                    // Deliberately skipped; see this function's doc comment.
                }
            }
        }

        // Second pass: pin every directory's own mtime only now that every mkdir/write/
        // symlink call above has finished. The kernel bumps a directory's mtime each time an
        // entry is added to it, so pinning inline in the first pass would just be
        // overwritten by whatever gets created in that directory next.
        for (path, node) in &self.nodes {
            if matches!(node, SpecNode::Directory { .. }) {
                let target = dest.join(OsStr::from_bytes(path));
                debug_assert!(
                    target.starts_with(dest),
                    "materialize: computed target {target:?} for spec path {path:?} escapes \
                     dest {dest:?} in the directory mtime-pinning pass"
                );
                let handle = fs::File::open(&target)?;
                handle.set_times(fs::FileTimes::new().set_modified(SystemTime::UNIX_EPOCH))?;
            }
        }

        Ok(())
    }
}

/// Zero the fields of every entry that this builder genuinely cannot make agree across two
/// **independently constructed** real directory trees: [`Entry::inode`] always (no
/// userspace operation controls it — see the module docs' "Judgment call" section), and
/// [`Entry::mtime_sec`]/[`Entry::mtime_nsec`] for [`evtree::Payload::Symlink`] entries only
/// — [`BaseLayerSpec::materialize`] pins `mtime` for every regular file and directory it
/// writes, so those already agree without help from this function, and leaving them
/// untouched here is what proves the pin actually worked rather than papering over whether
/// it did. Symlinks are the one entry type `materialize` does not pin the mtime of (no
/// stable `std` path to `AT_SYMLINK_NOFOLLOW`-style timestamp control — see the module
/// docs), so their mtime is still normalised away here.
///
/// **This is not the ADR-008/P1-06 normalisation ruleset.** That is `normalise`'s job: a
/// pure function of `(evidence, ruleset_version)`, versioned as data in `rulesets/`, whose
/// purpose is deciding what a *tool's* observed changeset means for a verdict. This
/// function is a much narrower, purely mechanical utility scoped to this crate's own
/// construction-reproducibility proof: it exists only so two *independently constructed*
/// real directory trees can be compared for "did the builder produce the same thing,"
/// which is a meta-property of `world`, not a claim about any tool under test. It must
/// never be reused as a stand-in for the real normaliser, and lives in `world` rather than
/// anywhere near `normalise`/`verdict` precisely so it cannot be mistaken for one (ADR-005).
/// `#[cfg(test)]`-gated on top of that, for the same reason `discovery::Transport` is
/// `pub(crate)`: the boundary is enforced structurally, not by comment alone. Its only
/// caller is this crate's own test module, and a real caller reaching for it from outside
/// test code — the exact misuse the doc comment above warns against — is now a compile
/// error rather than a review comment.
#[cfg(test)]
pub(crate) fn strip_construction_noise(entries: &mut [Entry]) {
    for entry in entries {
        entry.inode = 0;
        if matches!(entry.payload, Payload::Symlink(_)) {
            entry.mtime_sec = 0;
            entry.mtime_nsec = 0;
        }
    }
}

/// Walk a real directory tree with `lstat`, producing [`evtree::Entry`] values.
///
/// A prototype of the walker P1-04's observation collector will eventually need for the
/// overlay *upper* layer (ADR-009's "Follow-on decisions"); `world` needs the same
/// capability now, for its own lower-layer reproducibility proof, so it is built once here
/// rather than twice. Two things it deliberately does not do, both left for P1-04 to add
/// when it has a real upper layer (with real xattrs and possibly real device nodes) to
/// contend with:
///
/// - **No xattr capture.** Always emits an empty [`Entry::xattrs`] list — no `listxattr`
///   call. This builder's own constructed trees never carry real on-disk xattrs (see
///   [`BaseLayerSpec::materialize`]'s docs), so there is nothing for this walker to miss in
///   practice; a spec that wants an xattr represented uses
///   [`BaseLayerSpec::to_entries`] directly instead of round-tripping through real disk.
/// - **Best-effort device major/minor decoding.** Uses the Linux glibc `makedev` encoding
///   (see [`major_minor`]). Never exercised by this crate's own tests, because
///   `materialize` never creates a real device node — included so this walker is a
///   faithful starting point for P1-04, which will run against real overlay upper layers
///   in the pinned Linux VM (ADR-010) where that encoding is authoritative.
///
/// # Errors
///
/// Any underlying filesystem operation failing, or an on-disk object of a POSIX type this
/// crate's [`evtree::Payload`] cannot represent.
pub fn capture(root: &Path) -> io::Result<Vec<Entry>> {
    let mut entries = Vec::new();
    capture_into(root, root, &mut entries)?;
    Ok(entries)
}

fn capture_into(root: &Path, dir: &Path, out: &mut Vec<Entry>) -> io::Result<()> {
    for child in fs::read_dir(dir)? {
        let child = child?;
        let abs = child.path();
        let meta = fs::symlink_metadata(&abs)?;
        let file_type = meta.file_type();

        let rel = abs
            .strip_prefix(root)
            .unwrap_or_else(|_| panic!("{abs:?} is not under walk root {root:?}"));
        let rel_bytes = rel.as_os_str().as_bytes().to_vec();

        let payload = if file_type.is_symlink() {
            let target = fs::read_link(&abs)?;
            Payload::Symlink(target.as_os_str().as_bytes().to_vec())
        } else if file_type.is_dir() {
            Payload::Directory
        } else if file_type.is_file() {
            Payload::Regular(fs::read(&abs)?)
        } else if file_type.is_fifo() {
            Payload::Fifo
        } else if file_type.is_char_device() {
            Payload::CharDevice
        } else if file_type.is_block_device() {
            Payload::BlockDevice
        } else if file_type.is_socket() {
            Payload::Socket
        } else {
            return Err(io::Error::other(format!("unsupported file type at {abs:?}")));
        };

        let (dev_major, dev_minor) = if file_type.is_char_device() || file_type.is_block_device() {
            major_minor(meta.rdev())
        } else {
            (0, 0)
        };

        out.push(Entry {
            path: rel_bytes,
            mode: meta.mode(),
            uid: meta.uid(),
            gid: meta.gid(),
            mtime_sec: meta.mtime(),
            mtime_nsec: u32::try_from(meta.mtime_nsec()).unwrap_or(0),
            inode: meta.ino(),
            dev_major,
            dev_minor,
            xattrs: Vec::new(),
            payload,
        });

        if file_type.is_dir() {
            capture_into(root, &abs, out)?;
        }
    }
    Ok(())
}

/// Decode a `st_rdev` value into (major, minor) using the Linux glibc `makedev` encoding.
/// See [`capture`]'s docs for the scope of this function's applicability.
fn major_minor(rdev: u64) -> (u32, u32) {
    let major = ((rdev >> 8) & 0xfff) | ((rdev >> 32) & !0xfff);
    let minor = (rdev & 0xff) | ((rdev >> 12) & !0xff);
    (u32::try_from(major).unwrap_or(u32::MAX), u32::try_from(minor).unwrap_or(u32::MAX))
}

/// SHA-256 digest of `entries`' canonical `evtree1` encoding.
///
/// Mirrors `store::BlobStore`'s own content-addressing scheme (`datamodel::Digest`,
/// SHA-256) so a base layer's identity is compared the same way any other evidence blob in
/// this system would be — "byte-identical" and "digest-identical" are the same claim here,
/// exactly as they are once this base layer's capture is actually handed to `BlobStore`.
#[must_use]
pub fn digest(entries: &[Entry]) -> Digest {
    let bytes = evtree::encode(entries);
    let hash = Sha256::digest(&bytes);
    Digest::from_bytes(hash.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // ---------------------------------------------------------------------------------
    // Pure, in-memory determinism — no disk I/O.
    // ---------------------------------------------------------------------------------

    #[test]
    fn empty_spec_encodes_to_header_only() {
        let spec = BaseLayerSpec::new();
        assert!(spec.is_empty());
        let bytes = evtree::encode(&spec.to_entries());
        assert_eq!(bytes.len(), 8 + 2 + 8); // magic + version + entry_count, no entries
    }

    #[test]
    fn to_entries_is_byte_identical_across_independently_built_specs() {
        // Two unrelated closures build "the same" spec via different call sequences,
        // simulating two independent constructions at the spec level.
        let build_a = || {
            let mut s = BaseLayerSpec::new();
            s.add("dir/nested/file.txt", SpecNode::file(b"hello".to_vec()));
            s.add("top.txt", SpecNode::file(b"top".to_vec()));
            s.add("dir", SpecNode::dir_mode(0o750)); // explicit override, added after child
            s
        };
        let build_b = || {
            let mut s = BaseLayerSpec::new();
            s.add("dir", SpecNode::dir_mode(0o750)); // same override, added before children
            s.add("top.txt", SpecNode::file(b"top".to_vec()));
            s.add("dir/nested/file.txt", SpecNode::file(b"hello".to_vec()));
            s
        };
        let a = evtree::encode(&build_a().to_entries());
        let b = evtree::encode(&build_b().to_entries());
        assert_eq!(a, b);
        assert_eq!(digest(&build_a().to_entries()), digest(&build_b().to_entries()));
    }

    #[test]
    fn ensure_parents_synthesizes_missing_ancestor_directories() {
        let mut spec = BaseLayerSpec::new();
        spec.add("a/b/c.txt", SpecNode::file(b"x".to_vec()));
        let entries = spec.to_entries();
        let paths: Vec<&[u8]> = entries.iter().map(|e| e.path.as_slice()).collect();
        assert!(paths.contains(&b"a".as_slice()));
        assert!(paths.contains(&b"a/b".as_slice()));
        assert!(paths.contains(&b"a/b/c.txt".as_slice()));
        let synthesized = entries.iter().find(|e| e.path == b"a").unwrap();
        assert_eq!(synthesized.mode, S_IFDIR | 0o755); // SpecNode::dir()'s default
    }

    #[test]
    fn explicit_directory_declaration_overrides_the_synthesized_default() {
        let mut spec = BaseLayerSpec::new();
        spec.add("a/f", SpecNode::file(b"x".to_vec())); // synthesizes "a" at 0o755 first
        spec.add("a", SpecNode::dir_mode(0o700)); // then explicitly overrides it
        let entries = spec.to_entries();
        let a = entries.iter().find(|e| e.path == b"a").unwrap();
        assert_eq!(a.mode, S_IFDIR | 0o700);
    }

    #[test]
    fn to_entries_mode_includes_posix_type_bits_matching_a_real_lstat() {
        let mut spec = BaseLayerSpec::new();
        spec.add("d", SpecNode::dir_mode(0o755));
        spec.add("d/f", SpecNode::file_mode(0o644, b"x".to_vec()));
        spec.add("d/l", SpecNode::symlink(b"f".to_vec()));
        let entries = spec.to_entries();
        let by_path = |p: &str| entries.iter().find(|e| e.path == p.as_bytes()).unwrap();
        assert_eq!(by_path("d").mode, 0o040_755);
        assert_eq!(by_path("d/f").mode, 0o100_644);
        assert_eq!(by_path("d/l").mode, 0o120_777);
    }

    #[test]
    fn whiteout_lowers_to_a_char_device_at_dev_zero_zero() {
        let mut spec = BaseLayerSpec::new();
        spec.add("shadowed-file", SpecNode::whiteout());
        let entries = spec.to_entries();
        let w = &entries[0];
        assert_eq!(w.payload, Payload::CharDevice);
        assert_eq!((w.dev_major, w.dev_minor), (0, 0));
        assert_eq!(w.mode, S_IFCHR);

        // Round-trips through the real wire format, not just the in-memory Entry.
        let decoded = evtree::decode(&evtree::encode(&entries)).unwrap();
        assert_eq!(decoded, entries);
    }

    #[test]
    fn opaque_dir_lowers_to_a_directory_with_the_overlay_xattr() {
        let mut spec = BaseLayerSpec::new();
        spec.add("shadowed-dir", SpecNode::opaque_dir());
        let entries = spec.to_entries();
        let o = &entries[0];
        assert_eq!(o.payload, Payload::Directory);
        assert_eq!(o.xattrs, vec![XAttr::new("trusted.overlay.opaque", "y")]);

        let decoded = evtree::decode(&evtree::encode(&entries)).unwrap();
        assert_eq!(decoded, entries);
    }

    #[test]
    fn opaque_dir_userxattr_variant_uses_the_user_namespace() {
        let mut spec = BaseLayerSpec::new();
        spec.add("shadowed-dir", SpecNode::opaque_dir_userxattr());
        let entries = spec.to_entries();
        assert_eq!(entries[0].xattrs, vec![XAttr::new("user.overlay.opaque", "y")]);
    }

    #[test]
    fn char_device_is_absent_from_the_spec_add_panics_on_empty_path() {
        let mut spec = BaseLayerSpec::new();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            spec.add("", SpecNode::dir());
        }));
        assert!(result.is_err());
    }

    // ---------------------------------------------------------------------------------
    // Path traversal / absolute-path rejection — security fix.
    // ---------------------------------------------------------------------------------

    #[test]
    fn add_panics_on_an_absolute_path() {
        let mut spec = BaseLayerSpec::new();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            spec.add("/etc/passwd", SpecNode::file(b"pwned".to_vec()));
        }));
        assert!(result.is_err(), "an absolute path must be rejected, not silently escape `dest`");
    }

    #[test]
    fn add_panics_on_a_leading_dotdot_component() {
        let mut spec = BaseLayerSpec::new();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            spec.add("../escaped.txt", SpecNode::file(b"pwned".to_vec()));
        }));
        assert!(result.is_err());
    }

    #[test]
    fn add_panics_on_a_dotdot_component_buried_mid_path() {
        // Not just a leading `..` — a component check, not a string-prefix check. `a/../b`
        // has no leading `..` at all, but still traverses upward past `dest` at real-write
        // time because `Path::join` never normalises it away.
        let mut spec = BaseLayerSpec::new();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            spec.add("a/../../etc/passwd", SpecNode::file(b"pwned".to_vec()));
        }));
        assert!(result.is_err());

        let mut spec2 = BaseLayerSpec::new();
        let result2 = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            spec2.add("a/../b", SpecNode::file(b"x".to_vec()));
        }));
        assert!(result2.is_err(), "a '..' component buried mid-path must be rejected too, not just a leading one");
    }

    #[test]
    fn add_panics_on_empty_and_dot_components() {
        // `a//b`, `a/b/`, and `a/./b` all collapse to a different on-disk path than the
        // spec key, so the pure and real-disk proof paths would silently diverge.
        for bad in ["a//b", "a/b/", "a/./b", "./a"] {
            let mut spec = BaseLayerSpec::new();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                spec.add(bad, SpecNode::file(b"x".to_vec()));
            }));
            assert!(result.is_err(), "path {bad:?} must be rejected");
        }
    }

    #[test]
    fn legitimate_multi_segment_nested_paths_are_still_accepted() {
        // The fix must not be so aggressive that it rejects ordinary nested paths — only
        // real absolute/traversal paths.
        let mut spec = BaseLayerSpec::new();
        spec.add("a/b/c/d.txt", SpecNode::file(b"fine".to_vec()));
        spec.add("a/b/sibling.txt", SpecNode::file(b"also fine".to_vec()));
        let entries = spec.to_entries();
        let paths: Vec<&[u8]> = entries.iter().map(|e| e.path.as_slice()).collect();
        assert!(paths.contains(&b"a/b/c/d.txt".as_slice()));
        assert!(paths.contains(&b"a/b/sibling.txt".as_slice()));
        assert!(paths.contains(&b"a/b/c".as_slice()));
    }

    // ---------------------------------------------------------------------------------
    // Real disk: the P1-02 exit criterion itself.
    // ---------------------------------------------------------------------------------

    fn sample_spec() -> BaseLayerSpec {
        let mut spec = BaseLayerSpec::new();
        spec.add("readme.txt", SpecNode::file(b"hello base layer".to_vec()));
        spec.add("bin/tool", SpecNode::file_mode(0o755, b"#!/bin/sh\necho hi\n".to_vec()));
        spec.add("etc/config.json", SpecNode::file(b"{}".to_vec()));
        spec.add("etc/empty-dir", SpecNode::dir());
        spec.add("etc/link-to-config", SpecNode::symlink(b"config.json".to_vec()));
        spec.add("etc/dangling-link", SpecNode::symlink(b"does/not/exist".to_vec()));
        spec.add("deeply/nested/path/for/testing/depth/file.txt", SpecNode::file(b"deep".to_vec()));
        spec
    }

    #[test]
    fn two_independent_real_constructions_produce_byte_identical_layers() {
        // This is the literal P1-02 exit criterion: "Two independent constructions of the
        // same base produce byte-identical layers."
        let spec = sample_spec();

        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        spec.materialize(dir_a.path()).unwrap();
        spec.materialize(dir_b.path()).unwrap();

        let mut entries_a = capture(dir_a.path()).unwrap();
        let mut entries_b = capture(dir_b.path()).unwrap();

        // Judgment call, documented at length on `strip_construction_noise`: inode numbers
        // are kernel-assigned and cannot be made to agree across two independently created
        // trees. `mtime` is now genuinely pinned by `materialize` for regular files and
        // directories, so it no longer needs stripping for those — only symlinks (the one
        // entry type `materialize` cannot pin) still get their mtime normalised away here.
        // Everything else — path, mode, uid, gid, xattrs, content, symlink targets — must
        // still agree, and does.
        strip_construction_noise(&mut entries_a);
        strip_construction_noise(&mut entries_b);

        assert_eq!(entries_a, entries_b);
        assert_eq!(evtree::encode(&entries_a), evtree::encode(&entries_b));
        assert_eq!(digest(&entries_a), digest(&entries_b));

        // And, before any normalisation: prove both halves of the claim above directly
        // against raw, unstripped captures, so the asserts after stripping aren't vacuously
        // true because everything happened to coincide.
        let mut raw_a = capture(dir_a.path()).unwrap();
        let mut raw_b = capture(dir_b.path()).unwrap();
        raw_a.sort_by(|x, y| x.path.cmp(&y.path));
        raw_b.sort_by(|x, y| x.path.cmp(&y.path));

        // (1) Inode numbers really do disagree — the field `strip_construction_noise`
        // cannot avoid stripping.
        let inodes_a: Vec<u64> = raw_a.iter().map(|e| e.inode).collect();
        let inodes_b: Vec<u64> = raw_b.iter().map(|e| e.inode).collect();
        assert_ne!(inodes_a, inodes_b, "two separate temp dirs unexpectedly share inode numbers");

        // (2) mtime pinning actually worked: every non-symlink entry already agrees between
        // the two independent constructions, and already sits at the pinned UNIX_EPOCH
        // value, with zero help from `strip_construction_noise`.
        assert_eq!(raw_a.len(), raw_b.len());
        for (a, b) in raw_a.iter().zip(raw_b.iter()) {
            assert_eq!(a.path, b.path, "raw captures disagree on path ordering after sorting");
            if matches!(a.payload, Payload::Symlink(_)) {
                continue;
            }
            let path_lossy = String::from_utf8_lossy(&a.path);
            assert_eq!(
                (a.mtime_sec, a.mtime_nsec),
                (b.mtime_sec, b.mtime_nsec),
                "mtime should already be pinned identically for non-symlink path {path_lossy:?}"
            );
            assert_eq!(
                (a.mtime_sec, a.mtime_nsec),
                (0, 0),
                "materialize should pin mtime to SystemTime::UNIX_EPOCH for {path_lossy:?}"
            );
        }
    }

    #[test]
    fn same_on_disk_tree_captured_twice_without_rebuilding_is_exactly_identical() {
        // ADR-009's own (narrower) claim, proven directly: capturing the identical files
        // twice needs no normalisation at all, inode and mtime included, because nothing
        // was rebuilt in between.
        let spec = sample_spec();
        let dir = tempfile::tempdir().unwrap();
        spec.materialize(dir.path()).unwrap();

        let first = capture(dir.path()).unwrap();
        let second = capture(dir.path()).unwrap();
        assert_eq!(first, second);
        assert_eq!(evtree::encode(&first), evtree::encode(&second));
    }

    #[test]
    fn materialized_content_and_permissions_match_the_spec_on_real_disk() {
        let spec = sample_spec();
        let dir = tempfile::tempdir().unwrap();
        spec.materialize(dir.path()).unwrap();

        let readme = dir.path().join("readme.txt");
        assert_eq!(fs::read(&readme).unwrap(), b"hello base layer");
        assert_eq!(fs::symlink_metadata(&readme).unwrap().permissions().mode() & 0o777, 0o644);

        let tool = dir.path().join("bin/tool");
        assert_eq!(fs::symlink_metadata(&tool).unwrap().permissions().mode() & 0o777, 0o755);

        let empty_dir = dir.path().join("etc/empty-dir");
        assert!(empty_dir.is_dir());
        assert_eq!(fs::read_dir(&empty_dir).unwrap().count(), 0);

        let link = dir.path().join("etc/link-to-config");
        assert_eq!(fs::read_link(&link).unwrap(), PathBuf::from("config.json"));

        let dangling = dir.path().join("etc/dangling-link");
        assert!(fs::symlink_metadata(&dangling).is_ok()); // the link itself exists
        assert!(fs::metadata(&dangling).is_err()); // its target does not

        let deep = dir.path().join("deeply/nested/path/for/testing/depth/file.txt");
        assert_eq!(fs::read(&deep).unwrap(), b"deep");
    }

    #[test]
    fn char_device_entries_are_skipped_by_materialize_but_kept_by_to_entries() {
        let mut spec = BaseLayerSpec::new();
        spec.add("regular.txt", SpecNode::file(b"x".to_vec()));
        spec.add("would-be-whiteout", SpecNode::whiteout());

        let dir = tempfile::tempdir().unwrap();
        spec.materialize(dir.path()).unwrap();

        assert!(dir.path().join("regular.txt").exists());
        assert!(fs::symlink_metadata(dir.path().join("would-be-whiteout")).is_err());

        // But it is still fully present in the pure, in-memory projection.
        let entries = spec.to_entries();
        assert!(entries.iter().any(|e| e.path == b"would-be-whiteout" && e.payload == Payload::CharDevice));
    }

    #[test]
    fn captured_real_tree_is_sorted_regardless_of_directory_creation_order() {
        let mut spec = BaseLayerSpec::new();
        // Insert in an order that is neither alphabetical nor reverse-alphabetical.
        spec.add("zeta.txt", SpecNode::file(b"z".to_vec()));
        spec.add("alpha.txt", SpecNode::file(b"a".to_vec()));
        spec.add("mid.txt", SpecNode::file(b"m".to_vec()));

        let dir = tempfile::tempdir().unwrap();
        spec.materialize(dir.path()).unwrap();
        let entries = capture(dir.path()).unwrap();
        let bytes = evtree::encode(&entries);
        let decoded = evtree::decode(&bytes).unwrap();
        for w in decoded.windows(2) {
            assert!(w[0].path < w[1].path, "not sorted: {:?} >= {:?}", w[0].path, w[1].path);
        }
    }

    #[test]
    fn deeply_nested_directories_materialize_and_capture_round_trip() {
        let mut spec = BaseLayerSpec::new();
        let mut path = String::new();
        for i in 0..20 {
            if i > 0 {
                path.push('/');
            }
            path.push_str(&format!("level{i}"));
        }
        path.push_str("/leaf.txt");
        spec.add(path.clone(), SpecNode::file(b"leaf content".to_vec()));

        let dir = tempfile::tempdir().unwrap();
        spec.materialize(dir.path()).unwrap();
        let entries = capture(dir.path()).unwrap();
        let leaf = entries.iter().find(|e| e.path == path.as_bytes()).unwrap();
        assert_eq!(leaf.payload, Payload::Regular(b"leaf content".to_vec()));
    }

    #[test]
    fn major_minor_round_trips_the_zero_zero_whiteout_case() {
        assert_eq!(major_minor(0), (0, 0));
    }
}
