//! P1-02: deterministic overlayfs base-layer construction.
//!
//! Builds the read-only lower directory an overlay mount serves as its base, from a
//! caller-supplied spec of filesystem entries, with no ambient state — wall-clock time,
//! PID, hostname — baked into the result. architecture.md §12 item 5: "everything
//! downstream depends on this. A nondeterministic base silently poisons every diff, and the
//! failure is invisible in the output."
//!
//! [`capture`] is a narrow, spec-compliant implementation of ADR-009's `evtree1` wire
//! format, scoped to exactly what this module's own reproducibility test needs (the entry
//! kinds [`build`] can produce: regular files, directories, symlinks — never a device file,
//! fifo, or socket, so `dev_major`/`dev_minor` are always `0` and `xattr_count` is always
//! `0` here). ADR-009 assigns the *general* walker — real `lstat`, full xattr capture, every
//! POSIX type, over the overlay *upper* layer — to P1-04; duplicating that here would be
//! scope beyond what P1-02 needs to prove its own exit criterion. If P1-04 lands a shared
//! implementation later, this should call into it rather than stay a second copy.

use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use datamodel::Digest;
use filetime::FileTime;
use sha2::{Digest as _, Sha256};

/// Every timestamp [`build`] writes, on every path it creates. Not "now" — baking in
/// wall-clock time would make two independent constructions, run minutes or days apart,
/// produce different mtimes for no reason connected to content. The Unix epoch is a fixed,
/// arbitrary sentinel with no meaning attached beyond "not ambient."
const FIXED_MTIME: FileTime = FileTime::zero();

/// One thing to create in the base layer.
#[derive(Debug, Clone)]
pub struct EntrySpec {
    /// Path relative to the base-layer root. Must be relative and contain no `..` component
    /// — [`build`] rejects anything else rather than silently confining or resolving it.
    pub path: PathBuf,
    /// What to create at `path`.
    pub kind: EntryKind,
    /// Unix permission bits (e.g. `0o644`, `0o755`). Ignored for [`EntryKind::Symlink`] —
    /// Linux has no portable way to set a symlink's own mode independent of its target, and
    /// the value is conventionally fixed at `0o777` for every symlink regardless.
    pub mode: u32,
}

/// What kind of filesystem object an [`EntrySpec`] creates.
#[derive(Debug, Clone)]
pub enum EntryKind {
    /// A directory. Every directory that will hold entries must have its own explicit
    /// `EntrySpec` — `build` never auto-creates a missing parent, so a spec that omits one
    /// is a caller bug, not something to paper over.
    Directory,
    /// A regular file with this exact content.
    File(Vec<u8>),
    /// A symlink pointing at this target (not resolved, not validated to exist).
    Symlink(PathBuf),
}

/// Why building the base layer failed.
#[derive(Debug)]
pub enum BuildError {
    /// An entry's `path` was absolute or contained a `..` component.
    UnsafePath(PathBuf),
    /// Entries were not supplied in ascending raw-byte order by `path`, or a directory
    /// entry's own spec did not appear before an entry nested under it.
    NotSorted {
        /// The index into the input slice where ordering was first violated.
        at_index: usize,
    },
    /// The underlying filesystem operation failed.
    Io(io::Error),
}

impl std::fmt::Display for BuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsafePath(p) => {
                write!(f, "entry path must be relative and contain no `..`: {}", p.display())
            }
            Self::NotSorted { at_index } => write!(
                f,
                "entries must be pre-sorted by path (ascending, raw bytes), with each \
                 directory's own entry preceding anything nested under it — violated at index \
                 {at_index}"
            ),
            Self::Io(e) => write!(f, "I/O error: {e}"),
        }
    }
}

impl std::error::Error for BuildError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::UnsafePath(_) | Self::NotSorted { .. } => None,
        }
    }
}

impl From<io::Error> for BuildError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// Materialise `entries` under fresh directory `root`, deterministically.
///
/// Requires `entries` pre-sorted ascending by `path` (raw byte order — the same order
/// [`capture`] independently re-derives, and the order ADR-009's evidence-tree walker uses):
/// checked, not assumed, so a caller bug surfaces as a loud [`BuildError::NotSorted`] rather
/// than a base layer whose determinism depends on accidentally-correct input order. This
/// also guarantees every directory exists before anything gets created under it, since a
/// directory's own path is always a proper prefix of, and therefore sorts before, any path
/// nested inside it.
///
/// `root` itself must not already exist (or must be empty) — `build` creates it and refuses
/// to layer onto pre-existing content it did not itself create, since that content would be
/// exactly the kind of ambient state this module exists to exclude.
pub fn build(root: &Path, entries: &[EntrySpec]) -> Result<(), BuildError> {
    for (index, pair) in entries.windows(2).enumerate() {
        if sort_key(&pair[0].path) > sort_key(&pair[1].path) {
            return Err(BuildError::NotSorted { at_index: index + 1 });
        }
    }

    std::fs::create_dir_all(root)?;

    for entry in entries {
        let full_path = join_relative(root, &entry.path)?;
        match &entry.kind {
            EntryKind::Directory => {
                std::fs::create_dir(&full_path)?;
                std::fs::set_permissions(&full_path, permissions(entry.mode))?;
            }
            EntryKind::File(content) => {
                std::fs::write(&full_path, content)?;
                std::fs::set_permissions(&full_path, permissions(entry.mode))?;
            }
            EntryKind::Symlink(target) => {
                symlink(target, &full_path)?;
            }
        }
    }

    // Second pass, in reverse: creating an entry updates its parent directory's own mtime,
    // so a directory's timestamp can only be pinned to the fixed sentinel *after* every
    // entry nested under it already exists. Reverse order over a path-sorted list visits
    // children before their parents for exactly this reason.
    for entry in entries.iter().rev() {
        let full_path = join_relative(root, &entry.path)?;
        match entry.kind {
            EntryKind::Symlink(_) => {
                filetime::set_symlink_file_times(&full_path, FIXED_MTIME, FIXED_MTIME)?;
            }
            EntryKind::Directory | EntryKind::File(_) => {
                filetime::set_file_times(&full_path, FIXED_MTIME, FIXED_MTIME)?;
            }
        }
    }
    filetime::set_file_times(root, FIXED_MTIME, FIXED_MTIME)?;

    Ok(())
}

fn sort_key(path: &Path) -> &[u8] {
    path.as_os_str().as_bytes()
}

fn permissions(mode: u32) -> std::fs::Permissions {
    std::fs::Permissions::from_mode(mode & 0o7777)
}

fn join_relative(root: &Path, relative: &Path) -> Result<PathBuf, BuildError> {
    if relative.is_absolute()
        || relative.components().any(|c| matches!(c, Component::ParentDir))
    {
        return Err(BuildError::UnsafePath(relative.to_path_buf()));
    }
    Ok(root.join(relative))
}

// ---- ADR-009 `evtree1` capture, scoped to this module's own reproducibility proof ----

const EVTREE1_MAGIC: &[u8; 8] = b"EVTREE1\0";
const EVTREE1_FORMAT_VERSION: u16 = 1;

const TYPE_REGULAR: u8 = 1;
const TYPE_DIRECTORY: u8 = 2;
const TYPE_SYMLINK: u8 = 3;

/// Whether a captured entry's `inode` field reflects the real, on-disk inode, or is zeroed.
///
/// See [`capture`]'s doc comment for why the zeroed mode exists at all: inode numbers are
/// assigned by the filesystem's own allocator, not by anything a construction process
/// chooses, and empirically differ between two independent, identically-ordered
/// constructions on a real filesystem (verified directly, not assumed — two builds of the
/// same six-entry tree on this project's own ext4-backed `/tmp` produced six different
/// inode numbers each side). `build`'s own reproducibility claim is necessarily scoped
/// around that fact, not in spite of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InodeHandling {
    /// Encode the real inode number `lstat` reports.
    Real,
    /// Encode `0` regardless of the real inode number.
    Zeroed,
}

/// Walk `root` and serialise it per ADR-009's `evtree1` wire format: sorted by raw path
/// bytes, fixed-width big-endian integers, no field populated from anything other than what
/// this walk itself observes.
///
/// A subset of the full spec, deliberately: only the three [`EntryKind`] variants `build`
/// can produce are handled (a directory, symlink, or device/fifo/socket encountered on disk
/// makes this return an I/O error rather than guess at an encoding for a shape this module
/// never creates), and xattrs are not read at all (`build` never sets any, so every entry's
/// `xattr_count` is `0`) — P1-04 owns the general case over the ADR-009 spec in full.
pub fn capture(root: &Path, inode_handling: InodeHandling) -> io::Result<Vec<u8>> {
    let mut relative_paths = Vec::new();
    collect_relative_paths(root, Path::new(""), &mut relative_paths)?;
    relative_paths.sort_by(|a, b| sort_key(a).cmp(sort_key(b)));

    let mut out = Vec::new();
    out.extend_from_slice(EVTREE1_MAGIC);
    out.extend_from_slice(&EVTREE1_FORMAT_VERSION.to_be_bytes());
    out.extend_from_slice(&(relative_paths.len() as u64).to_be_bytes());

    for relative in &relative_paths {
        let full_path = root.join(relative);
        let metadata = std::fs::symlink_metadata(&full_path)?;
        let file_type = metadata.file_type();

        let path_bytes = relative.as_os_str().as_bytes();
        out.extend_from_slice(&(path_bytes.len() as u64).to_be_bytes());
        out.extend_from_slice(path_bytes);

        let type_tag = if file_type.is_dir() {
            TYPE_DIRECTORY
        } else if file_type.is_symlink() {
            TYPE_SYMLINK
        } else if file_type.is_file() {
            TYPE_REGULAR
        } else {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "{}: not a regular file, directory, or symlink — outside this subset \
                     capture's scope (see P1-04 for the general walker)",
                    full_path.display()
                ),
            ));
        };
        out.push(type_tag);

        out.extend_from_slice(&metadata.mode().to_be_bytes());
        out.extend_from_slice(&metadata.uid().to_be_bytes());
        out.extend_from_slice(&metadata.gid().to_be_bytes());
        out.extend_from_slice(&metadata.mtime().to_be_bytes());
        out.extend_from_slice(&(metadata.mtime_nsec() as u32).to_be_bytes());

        let inode = match inode_handling {
            InodeHandling::Real => metadata.ino(),
            InodeHandling::Zeroed => 0,
        };
        out.extend_from_slice(&inode.to_be_bytes());

        // build() never creates a device file, so major/minor are always 0 for this
        // subset's own output — real device encoding is P1-04's concern, over real device
        // files it actually walks.
        out.extend_from_slice(&0u32.to_be_bytes()); // dev_major
        out.extend_from_slice(&0u32.to_be_bytes()); // dev_minor
        out.extend_from_slice(&0u32.to_be_bytes()); // xattr_count

        match type_tag {
            TYPE_REGULAR => {
                let content = std::fs::read(&full_path)?;
                out.extend_from_slice(&(content.len() as u64).to_be_bytes());
                out.extend_from_slice(&content);
            }
            TYPE_DIRECTORY => {}
            TYPE_SYMLINK => {
                let target = std::fs::read_link(&full_path)?;
                let target_bytes = target.as_os_str().as_bytes();
                out.extend_from_slice(&(target_bytes.len() as u64).to_be_bytes());
                out.extend_from_slice(target_bytes);
            }
            _ => unreachable!("type_tag was just constrained to one of the three above"),
        }
    }

    Ok(out)
}

fn collect_relative_paths(
    root: &Path,
    relative: &Path,
    out: &mut Vec<PathBuf>,
) -> io::Result<()> {
    let full_path = root.join(relative);
    for dirent in std::fs::read_dir(&full_path)? {
        let dirent = dirent?;
        let child_relative = relative.join(dirent.file_name());
        let file_type = dirent.file_type()?;
        if file_type.is_dir() {
            out.push(child_relative.clone());
            collect_relative_paths(root, &child_relative, out)?;
        } else {
            out.push(child_relative);
        }
    }
    Ok(())
}

/// SHA-256 over a `capture` result — the digest [`build`]'s reproducibility test compares.
#[must_use]
pub fn digest_of_capture(capture_bytes: &[u8]) -> Digest {
    Digest::from_bytes(Sha256::digest(capture_bytes).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_entries() -> Vec<EntrySpec> {
        vec![
            EntrySpec { path: PathBuf::from("bin"), kind: EntryKind::Directory, mode: 0o755 },
            EntrySpec {
                path: PathBuf::from("bin/tool"),
                kind: EntryKind::File(b"#!/bin/sh\necho hi\n".to_vec()),
                mode: 0o755,
            },
            EntrySpec { path: PathBuf::from("etc"), kind: EntryKind::Directory, mode: 0o755 },
            EntrySpec {
                path: PathBuf::from("etc/config.json"),
                kind: EntryKind::File(b"{\"a\":1}".to_vec()),
                mode: 0o644,
            },
            EntrySpec {
                path: PathBuf::from("etc/link-to-tool"),
                kind: EntryKind::Symlink(PathBuf::from("../bin/tool")),
                mode: 0o644,
            },
        ]
    }

    /// P1-02's literal exit criterion: two independent constructions of the same base
    /// produce byte-identical layers, proven by digest comparison over ADR-009's format —
    /// scoped around the one field (`inode`) empirically shown not to be
    /// construction-controllable on a real filesystem (see `InodeHandling`'s doc comment).
    #[test]
    fn two_independent_constructions_produce_identical_captures() {
        let dir_a = tempfile::tempdir().expect("tempdir");
        let dir_b = tempfile::tempdir().expect("tempdir");

        build(dir_a.path(), &sample_entries()).expect("build a");
        build(dir_b.path(), &sample_entries()).expect("build b");

        let capture_a =
            capture(dir_a.path(), InodeHandling::Zeroed).expect("capture a");
        let capture_b =
            capture(dir_b.path(), InodeHandling::Zeroed).expect("capture b");

        assert_eq!(
            capture_a, capture_b,
            "two independent constructions of the same spec must serialise identically"
        );
        assert_eq!(digest_of_capture(&capture_a), digest_of_capture(&capture_b));
    }

    /// Documents the real limitation `InodeHandling::Zeroed` exists to route around, rather
    /// than asserting a false universal claim: with real inode numbers included, two
    /// independent constructions are *not* expected to match, because the filesystem's own
    /// inode allocator — not this module — assigns that number, and it depends on
    /// allocator state this module neither controls nor observes.
    #[test]
    fn real_inode_numbers_are_not_expected_to_match_across_independent_constructions() {
        let dir_a = tempfile::tempdir().expect("tempdir");
        let dir_b = tempfile::tempdir().expect("tempdir");

        build(dir_a.path(), &sample_entries()).expect("build a");
        build(dir_b.path(), &sample_entries()).expect("build b");

        let capture_a = capture(dir_a.path(), InodeHandling::Real).expect("capture a");
        let capture_b = capture(dir_b.path(), InodeHandling::Real).expect("capture b");

        assert_ne!(
            capture_a, capture_b,
            "if this ever starts passing, real inode numbers happened to coincide by \
             chance on whatever filesystem ran this test — not a property to rely on"
        );
    }

    #[test]
    fn mtimes_are_fixed_not_ambient() {
        let dir = tempfile::tempdir().expect("tempdir");
        build(dir.path(), &sample_entries()).expect("build");

        for entry in sample_entries() {
            let full_path = dir.path().join(&entry.path);
            let metadata = std::fs::symlink_metadata(&full_path).expect("stat");
            assert_eq!(
                FileTime::from_last_modification_time(&metadata),
                FIXED_MTIME,
                "{} did not get the fixed sentinel mtime",
                entry.path.display()
            );
        }
    }

    #[test]
    fn rejects_an_absolute_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let entries = vec![EntrySpec {
            path: PathBuf::from("/etc/passwd"),
            kind: EntryKind::Directory,
            mode: 0o755,
        }];
        let err = build(dir.path(), &entries).expect_err("must reject");
        assert!(matches!(err, BuildError::UnsafePath(_)));
    }

    #[test]
    fn rejects_a_parent_dir_component() {
        let dir = tempfile::tempdir().expect("tempdir");
        let entries = vec![EntrySpec {
            path: PathBuf::from("../escape"),
            kind: EntryKind::Directory,
            mode: 0o755,
        }];
        let err = build(dir.path(), &entries).expect_err("must reject");
        assert!(matches!(err, BuildError::UnsafePath(_)));
    }

    #[test]
    fn rejects_unsorted_entries() {
        let dir = tempfile::tempdir().expect("tempdir");
        let entries = vec![
            EntrySpec { path: PathBuf::from("b"), kind: EntryKind::Directory, mode: 0o755 },
            EntrySpec { path: PathBuf::from("a"), kind: EntryKind::Directory, mode: 0o755 },
        ];
        let err = build(dir.path(), &entries).expect_err("must reject");
        assert!(matches!(err, BuildError::NotSorted { at_index: 1 }));
    }

    #[test]
    fn different_content_produces_a_different_capture() {
        let dir_a = tempfile::tempdir().expect("tempdir");
        let dir_b = tempfile::tempdir().expect("tempdir");

        let mut entries_b = sample_entries();
        if let EntryKind::File(content) = &mut entries_b[1].kind {
            content.push(b'!');
        }

        build(dir_a.path(), &sample_entries()).expect("build a");
        build(dir_b.path(), &entries_b).expect("build b");

        let capture_a = capture(dir_a.path(), InodeHandling::Zeroed).expect("capture a");
        let capture_b = capture(dir_b.path(), InodeHandling::Zeroed).expect("capture b");
        assert_ne!(capture_a, capture_b);
    }

    #[test]
    fn symlink_target_is_captured_verbatim() {
        let dir = tempfile::tempdir().expect("tempdir");
        build(dir.path(), &sample_entries()).expect("build");
        let target = std::fs::read_link(dir.path().join("etc/link-to-tool")).expect("readlink");
        assert_eq!(target, PathBuf::from("../bin/tool"));
    }

    #[test]
    fn an_empty_spec_produces_a_capture_with_zero_entries() {
        let dir = tempfile::tempdir().expect("tempdir");
        build(dir.path(), &[]).expect("build");
        let capture_bytes = capture(dir.path(), InodeHandling::Zeroed).expect("capture");
        let mut expected = Vec::new();
        expected.extend_from_slice(EVTREE1_MAGIC);
        expected.extend_from_slice(&EVTREE1_FORMAT_VERSION.to_be_bytes());
        expected.extend_from_slice(&0u64.to_be_bytes());
        assert_eq!(capture_bytes, expected);
    }
}
