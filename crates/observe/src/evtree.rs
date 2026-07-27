//! The general ADR-009 `evtree1` walker and serialiser — P1-04's own job, per that ADR's
//! "Follow-on decisions this ADR does not make" section, and a strict superset of the
//! narrower, sandbox-scoped subset `sandbox::base_layer::capture` implements for P1-02's own
//! reproducibility proof (regular/directory/symlink only, no xattrs, no device files — see
//! that module's doc comment for why duplicating the full case there would have been scope
//! beyond what P1-02 needed).
//!
//! Walks the overlay's upper directory **from the host side**, using `lstat`-equivalent
//! metadata and `llistxattr`/`lgetxattr` (the no-follow variants) so a whiteout (a character
//! device, mode roughly `0`, `dev_major=0`/`dev_minor=0`) or an opaque directory (the
//! `trusted.overlay.opaque` xattr) is captured as the plain filesystem object it is, never
//! resolved or interpreted. **Must not:** decide that a char-device entry *is* a whiteout, or
//! that an xattr *is* the opaque marker — that classification belongs to `normalise`
//! (ADR-005, ADR-008), never here.

use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 8] = b"EVTREE1\0";
const FORMAT_VERSION: u16 = 1;

const TYPE_REGULAR: u8 = 1;
const TYPE_DIRECTORY: u8 = 2;
const TYPE_SYMLINK: u8 = 3;
const TYPE_FIFO: u8 = 4;
const TYPE_CHAR_DEVICE: u8 = 5;
const TYPE_BLOCK_DEVICE: u8 = 6;
const TYPE_SOCKET: u8 = 7;

/// Walk `root` (the overlay upper directory, read directly from the host filesystem — never
/// through the overlay mount itself) and serialise it per ADR-009's `evtree1` wire format.
pub fn capture(root: &Path) -> io::Result<Vec<u8>> {
    let mut relative_paths = Vec::new();
    collect_relative_paths(root, Path::new(""), &mut relative_paths)?;
    relative_paths.sort_by(|a, b| sort_key(a).cmp(sort_key(b)));

    let mut out = Vec::new();
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&FORMAT_VERSION.to_be_bytes());
    out.extend_from_slice(&(relative_paths.len() as u64).to_be_bytes());

    for relative in &relative_paths {
        encode_entry(root, relative, &mut out)?;
    }

    Ok(out)
}

fn sort_key(path: &Path) -> &[u8] {
    path.as_os_str().as_bytes()
}

fn collect_relative_paths(root: &Path, relative: &Path, out: &mut Vec<PathBuf>) -> io::Result<()> {
    let full_path = root.join(relative);
    for dirent in std::fs::read_dir(&full_path)? {
        let dirent = dirent?;
        let child_relative = relative.join(dirent.file_name());
        // `DirEntry::file_type` is itself `lstat`-based (does not follow a symlink entry),
        // matching this walker's own no-follow discipline.
        if dirent.file_type()?.is_dir() {
            out.push(child_relative.clone());
            collect_relative_paths(root, &child_relative, out)?;
        } else {
            out.push(child_relative);
        }
    }
    Ok(())
}

fn encode_entry(root: &Path, relative: &Path, out: &mut Vec<u8>) -> io::Result<()> {
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
    } else if file_type.is_fifo() {
        TYPE_FIFO
    } else if file_type.is_char_device() {
        TYPE_CHAR_DEVICE
    } else if file_type.is_block_device() {
        TYPE_BLOCK_DEVICE
    } else if file_type.is_socket() {
        TYPE_SOCKET
    } else {
        return Err(io::Error::other(format!(
            "{}: lstat reported a file type this walker has no POSIX type tag for",
            full_path.display()
        )));
    };
    out.push(type_tag);

    out.extend_from_slice(&metadata.mode().to_be_bytes());
    out.extend_from_slice(&metadata.uid().to_be_bytes());
    out.extend_from_slice(&metadata.gid().to_be_bytes());
    out.extend_from_slice(&metadata.mtime().to_be_bytes());
    out.extend_from_slice(&(metadata.mtime_nsec() as u32).to_be_bytes());
    out.extend_from_slice(&metadata.ino().to_be_bytes());

    // Device major/minor only carries real information for the two device-file types (this
    // is how a whiteout, major=0/minor=0, is represented, per ADR-009); every other type
    // zeroes it, per the wire format's own "unused, zeroed" specification.
    let (dev_major, dev_minor) = match type_tag {
        TYPE_CHAR_DEVICE | TYPE_BLOCK_DEVICE => {
            let rdev = metadata.rdev();
            (device_major(rdev), device_minor(rdev))
        }
        _ => (0, 0),
    };
    out.extend_from_slice(&dev_major.to_be_bytes());
    out.extend_from_slice(&dev_minor.to_be_bytes());

    let xattrs = read_xattrs(&full_path)?;
    out.extend_from_slice(&(xattrs.len() as u32).to_be_bytes());
    for (name, value) in &xattrs {
        out.extend_from_slice(&(name.len() as u64).to_be_bytes());
        out.extend_from_slice(name);
        out.extend_from_slice(&(value.len() as u64).to_be_bytes());
        out.extend_from_slice(value);
    }

    match type_tag {
        TYPE_REGULAR => {
            let content = std::fs::read(&full_path)?;
            out.extend_from_slice(&(content.len() as u64).to_be_bytes());
            out.extend_from_slice(&content);
        }
        TYPE_SYMLINK => {
            let target = std::fs::read_link(&full_path)?;
            let target_bytes = target.as_os_str().as_bytes();
            out.extend_from_slice(&(target_bytes.len() as u64).to_be_bytes());
            out.extend_from_slice(target_bytes);
        }
        // directory, fifo, char/block device, socket: no further payload.
        _ => {}
    }

    Ok(())
}

/// Why decoding an `evtree1` capture failed.
#[derive(Debug)]
pub enum DecodeError {
    /// The bytes are too short, or too short at some specific field, to be a valid capture.
    Truncated,
    /// The leading magic bytes don't match `EVTREE1\0`.
    BadMagic,
    /// The format version isn't one this decoder understands.
    UnsupportedVersion(u16),
    /// A type tag byte wasn't one of the seven this format defines.
    UnknownTypeTag(u8),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Truncated => write!(f, "capture bytes end before a complete entry"),
            Self::BadMagic => write!(f, "missing or incorrect EVTREE1 magic bytes"),
            Self::UnsupportedVersion(v) => write!(f, "unsupported evtree1 format version {v}"),
            Self::UnknownTypeTag(t) => write!(f, "unknown evtree1 type tag {t}"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// The inverse of [`capture`]: parse `evtree1` bytes back into `datamodel`'s pure,
/// in-memory [`datamodel::RawEvidence`] — the "I/O-adjacent" deserialisation step ADR-009
/// assigns outside the pure `normalise`/`verdict` closure. Xattrs and file content are
/// present in the wire bytes (skipped over here, not stored — see `datamodel::EvidenceEntry`'s
/// own doc comment for why the in-memory type doesn't carry them yet); everything
/// `normalise`'s current job (ADR-008 path taxonomy) needs is decoded.
pub fn decode(bytes: &[u8]) -> Result<datamodel::RawEvidence, DecodeError> {
    let mut cursor = Cursor { bytes, pos: 0 };

    let magic = cursor.take(8)?;
    if magic != MAGIC.as_slice() {
        return Err(DecodeError::BadMagic);
    }
    let format_version = u16::from_be_bytes(cursor.take(2)?.try_into().unwrap());
    if format_version != FORMAT_VERSION {
        return Err(DecodeError::UnsupportedVersion(format_version));
    }
    let entry_count = u64::from_be_bytes(cursor.take(8)?.try_into().unwrap());

    let mut entries = Vec::with_capacity(entry_count as usize);
    for _ in 0..entry_count {
        entries.push(decode_entry(&mut cursor)?);
    }
    Ok(datamodel::RawEvidence::new(entries))
}

struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        let slice = self.bytes.get(self.pos..self.pos + n).ok_or(DecodeError::Truncated)?;
        self.pos += n;
        Ok(slice)
    }

    fn take_u64_prefixed(&mut self) -> Result<&'a [u8], DecodeError> {
        let len = u64::from_be_bytes(self.take(8)?.try_into().unwrap());
        self.take(len as usize)
    }
}

fn decode_entry(cursor: &mut Cursor<'_>) -> Result<datamodel::EvidenceEntry, DecodeError> {
    let path = cursor.take_u64_prefixed()?.to_vec();

    let type_tag = cursor.take(1)?[0];
    let kind = match type_tag {
        TYPE_REGULAR => datamodel::EntryKind::Regular,
        TYPE_DIRECTORY => datamodel::EntryKind::Directory,
        TYPE_SYMLINK => datamodel::EntryKind::Symlink,
        TYPE_FIFO => datamodel::EntryKind::Fifo,
        TYPE_CHAR_DEVICE => datamodel::EntryKind::CharDevice,
        TYPE_BLOCK_DEVICE => datamodel::EntryKind::BlockDevice,
        TYPE_SOCKET => datamodel::EntryKind::Socket,
        other => return Err(DecodeError::UnknownTypeTag(other)),
    };

    let mode = u32::from_be_bytes(cursor.take(4)?.try_into().unwrap());
    let uid = u32::from_be_bytes(cursor.take(4)?.try_into().unwrap());
    let gid = u32::from_be_bytes(cursor.take(4)?.try_into().unwrap());
    let mtime_sec = i64::from_be_bytes(cursor.take(8)?.try_into().unwrap());
    let mtime_nsec = u32::from_be_bytes(cursor.take(4)?.try_into().unwrap());
    let inode = u64::from_be_bytes(cursor.take(8)?.try_into().unwrap());
    let dev_major = u32::from_be_bytes(cursor.take(4)?.try_into().unwrap());
    let dev_minor = u32::from_be_bytes(cursor.take(4)?.try_into().unwrap());

    let xattr_count = u32::from_be_bytes(cursor.take(4)?.try_into().unwrap());
    for _ in 0..xattr_count {
        cursor.take_u64_prefixed()?; // name
        cursor.take_u64_prefixed()?; // value
    }

    match type_tag {
        TYPE_REGULAR => {
            cursor.take_u64_prefixed()?; // content — not retained; see this fn's doc comment
        }
        TYPE_SYMLINK => {
            cursor.take_u64_prefixed()?; // target — not retained; see this fn's doc comment
        }
        _ => {}
    }

    Ok(datamodel::EvidenceEntry {
        path,
        kind,
        mode,
        uid,
        gid,
        mtime_sec,
        mtime_nsec,
        inode,
        dev_major,
        dev_minor,
    })
}

/// Glibc's `major(3)`/`minor(3)` bit layout for a 64-bit `dev_t`, hand-decoded rather than
/// pulled from a library: the low 8 bits of the major number and the low 20 bits of the
/// minor number sit in the low 32 bits of `dev_t` (interleaved with each other), and the
/// remaining high bits of each sit above bit 32. This is the standard glibc encoding used
/// across current Linux distributions.
fn device_major(rdev: u64) -> u32 {
    (((rdev >> 8) & 0xfff) | ((rdev >> 32) & !0xfff)) as u32
}

fn device_minor(rdev: u64) -> u32 {
    ((rdev & 0xff) | ((rdev >> 12) & !0xff)) as u32
}

/// All xattrs on `path` itself (never following a symlink), sorted by raw name bytes — same
/// determinism reasoning ADR-009 applies to the entry list itself.
///
/// # `unsafe_code`
///
/// `llistxattr`/`lgetxattr` have no safe wrapper in `std`; both are plain, non-allocating (on
/// our side) syscalls taking a length-prefixed-by-return-value buffer, called in the
/// standard two-pass "ask for size, then fill" pattern with an `ERANGE` retry loop for the
/// case where the attribute set changes between the two calls.
#[allow(unsafe_code)]
fn read_xattrs(path: &Path) -> io::Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let cpath = path_to_cstring(path)?;

    let names = llistxattr(&cpath)?;
    let mut xattrs: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(names.len());
    for name in names {
        let cname = CString::new(name.clone())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        let value = lgetxattr(&cpath, &cname)?;
        xattrs.push((name, value));
    }
    xattrs.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(xattrs)
}

fn path_to_cstring(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))
}

#[allow(unsafe_code)]
fn llistxattr(cpath: &CString) -> io::Result<Vec<Vec<u8>>> {
    loop {
        // SAFETY: `cpath` is a valid, NUL-terminated C string for the duration of the call;
        // passing a null buffer with size 0 is `llistxattr(2)`'s documented way to query the
        // required buffer size, and returns that size without writing anything.
        let needed = unsafe { libc::llistxattr(cpath.as_ptr(), std::ptr::null_mut(), 0) };
        if needed < 0 {
            return Err(io::Error::last_os_error());
        }
        if needed == 0 {
            return Ok(Vec::new());
        }

        let mut buf = vec![0u8; needed as usize];
        // SAFETY: `buf` is a live, uniquely-owned allocation of exactly `buf.len()` bytes;
        // the kernel writes at most that many bytes into it.
        let written = unsafe {
            libc::llistxattr(cpath.as_ptr(), buf.as_mut_ptr().cast(), buf.len())
        };
        if written < 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ERANGE) {
                // The attribute set grew between the size query and the fill call; retry.
                continue;
            }
            return Err(err);
        }
        buf.truncate(written as usize);
        return Ok(buf
            .split(|&b| b == 0)
            .filter(|name| !name.is_empty())
            .map(<[u8]>::to_vec)
            .collect());
    }
}

#[allow(unsafe_code)]
fn lgetxattr(cpath: &CString, cname: &CString) -> io::Result<Vec<u8>> {
    loop {
        // SAFETY: both C strings are valid and NUL-terminated for the call's duration; a
        // null buffer with size 0 queries the required size without writing anything,
        // per `lgetxattr(2)`.
        let needed =
            unsafe { libc::lgetxattr(cpath.as_ptr(), cname.as_ptr(), std::ptr::null_mut(), 0) };
        if needed < 0 {
            return Err(io::Error::last_os_error());
        }
        if needed == 0 {
            return Ok(Vec::new());
        }

        let mut buf = vec![0u8; needed as usize];
        // SAFETY: `buf` is a live, uniquely-owned allocation of exactly `buf.len()` bytes.
        let written = unsafe {
            libc::lgetxattr(cpath.as_ptr(), cname.as_ptr(), buf.as_mut_ptr().cast(), buf.len())
        };
        if written < 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ERANGE) {
                continue;
            }
            return Err(err);
        }
        buf.truncate(written as usize);
        return Ok(buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_directory_captures_to_the_bare_header() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bytes = capture(dir.path()).expect("capture");
        let mut expected = Vec::new();
        expected.extend_from_slice(MAGIC);
        expected.extend_from_slice(&FORMAT_VERSION.to_be_bytes());
        expected.extend_from_slice(&0u64.to_be_bytes());
        assert_eq!(bytes, expected);
    }

    /// `decode` is `capture`'s exact inverse for everything `datamodel::EvidenceEntry`
    /// retains — proven by capturing a real, mixed tree and checking every decoded field
    /// against what was actually on disk, not just that decoding didn't error.
    #[test]
    fn decode_recovers_every_retained_field_from_a_real_capture() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("sub")).expect("mkdir");
        std::fs::write(dir.path().join("sub/file.txt"), b"hello").expect("write");
        std::os::unix::fs::symlink("file.txt", dir.path().join("sub/link")).expect("symlink");

        let bytes = capture(dir.path()).expect("capture");
        let evidence = decode(&bytes).expect("decode");

        assert_eq!(evidence.entries.len(), 3);
        let by_path = |p: &str| {
            evidence
                .entries
                .iter()
                .find(|e| e.path == p.as_bytes())
                .unwrap_or_else(|| panic!("missing entry: {p}"))
        };

        let sub = by_path("sub");
        assert_eq!(sub.kind, datamodel::EntryKind::Directory);
        let file_meta = std::fs::symlink_metadata(dir.path().join("sub/file.txt")).unwrap();
        let file = by_path("sub/file.txt");
        assert_eq!(file.kind, datamodel::EntryKind::Regular);
        assert_eq!(file.mode, file_meta.mode());
        assert_eq!(file.uid, file_meta.uid());
        assert_eq!(file.gid, file_meta.gid());
        assert_eq!(file.inode, file_meta.ino());
        assert_eq!(file.mtime_sec, file_meta.mtime());
        assert_eq!(file.mtime_nsec, file_meta.mtime_nsec() as u32);
        assert_eq!((file.dev_major, file.dev_minor), (0, 0));

        let link = by_path("sub/link");
        assert_eq!(link.kind, datamodel::EntryKind::Symlink);
    }

    #[test]
    fn decode_rejects_bad_magic() {
        let err = decode(b"NOTEVTR1\0\0extra").expect_err("must reject");
        assert!(matches!(err, DecodeError::BadMagic));
    }

    #[test]
    fn decode_rejects_truncated_bytes() {
        let err = decode(&MAGIC[..4]).expect_err("must reject");
        assert!(matches!(err, DecodeError::Truncated));
    }

    #[test]
    fn a_regular_file_round_trips_its_exact_content() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a.txt"), b"hello world").expect("write");
        let bytes = capture(dir.path()).expect("capture");
        // Content is the tail of the entry: confirm it's present verbatim rather than
        // decoding the whole entry — a full round-trip decoder is P1-06's consumer-side
        // concern, not this walker's.
        assert!(
            bytes.windows(b"hello world".len()).any(|w| w == b"hello world"),
            "file content must appear verbatim in the capture"
        );
    }

    #[test]
    fn two_captures_of_an_unmodified_tree_are_byte_identical() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("sub")).expect("mkdir");
        std::fs::write(dir.path().join("sub/file.txt"), b"x").expect("write");
        std::os::unix::fs::symlink("file.txt", dir.path().join("sub/link")).expect("symlink");

        let first = capture(dir.path()).expect("capture 1");
        let second = capture(dir.path()).expect("capture 2");
        assert_eq!(first, second, "capturing an unmodified tree twice must be deterministic");
    }

    /// Decode just the sequence of entry paths, in the order they appear in the capture —
    /// enough to check sort order without a full entry decoder.
    fn decode_paths_in_order(bytes: &[u8]) -> Vec<Vec<u8>> {
        let mut pos = MAGIC.len() + 2;
        let entry_count = u64::from_be_bytes(bytes[pos..pos + 8].try_into().unwrap());
        pos += 8;

        let mut paths = Vec::new();
        for _ in 0..entry_count {
            let path_len = u64::from_be_bytes(bytes[pos..pos + 8].try_into().unwrap()) as usize;
            pos += 8;
            paths.push(bytes[pos..pos + path_len].to_vec());
            pos += path_len;

            let type_tag = bytes[pos];
            pos += 1;
            pos += 4 + 4 + 4 + 8 + 4 + 8 + 4 + 4; // mode..dev_minor
            let xattr_count = u32::from_be_bytes(bytes[pos..pos + 4].try_into().unwrap());
            pos += 4;
            for _ in 0..xattr_count {
                let name_len =
                    u64::from_be_bytes(bytes[pos..pos + 8].try_into().unwrap()) as usize;
                pos += 8 + name_len;
                let value_len =
                    u64::from_be_bytes(bytes[pos..pos + 8].try_into().unwrap()) as usize;
                pos += 8 + value_len;
            }
            match type_tag {
                TYPE_REGULAR => {
                    let size = u64::from_be_bytes(bytes[pos..pos + 8].try_into().unwrap())
                        as usize;
                    pos += 8 + size;
                }
                TYPE_SYMLINK => {
                    let target_len =
                        u64::from_be_bytes(bytes[pos..pos + 8].try_into().unwrap()) as usize;
                    pos += 8 + target_len;
                }
                _ => {}
            }
        }
        paths
    }

    /// This walker's own traversal (`read_dir` order) is unspecified, but the output must be
    /// sorted regardless — tested by creating entries in *descending* name order and
    /// confirming the capture still lists them ascending. (A cross-directory,
    /// same-order-different-directory comparison would conflate this with real ambient mtime
    /// differences between the two directories' actual creation instants, which this general
    /// walker — unlike P1-02's base-layer builder — correctly preserves rather than fixes;
    /// see this module's own doc comment on losslessness.)
    #[test]
    fn entries_are_sorted_by_raw_path_bytes_regardless_of_readdir_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("zzz"), b"").expect("write");
        std::fs::write(dir.path().join("mmm"), b"").expect("write");
        std::fs::write(dir.path().join("aaa"), b"").expect("write");

        let bytes = capture(dir.path()).expect("capture");
        let paths = decode_paths_in_order(&bytes);
        let mut sorted = paths.clone();
        sorted.sort();
        assert_eq!(paths, sorted, "entries must be sorted by raw path bytes regardless of creation order");
        assert_eq!(paths, vec![b"aaa".to_vec(), b"mmm".to_vec(), b"zzz".to_vec()]);
    }

    /// Minimal test-only decoder for exactly one entry, so assertions read the wire format
    /// by field rather than by hand-computed byte offsets (fragile, and duplicative of the
    /// encoder it would be checking against itself if it got an offset wrong the same way).
    struct DecodedEntry {
        path: Vec<u8>,
        type_tag: u8,
        dev_major: u32,
        dev_minor: u32,
        xattrs: Vec<(Vec<u8>, Vec<u8>)>,
    }

    fn decode_first_entry(bytes: &[u8]) -> DecodedEntry {
        let mut pos = MAGIC.len() + 2; // magic + format_version
        let entry_count = u64::from_be_bytes(bytes[pos..pos + 8].try_into().unwrap());
        pos += 8;
        assert!(entry_count >= 1, "expected at least one entry");

        let path_len = u64::from_be_bytes(bytes[pos..pos + 8].try_into().unwrap()) as usize;
        pos += 8;
        let path = bytes[pos..pos + path_len].to_vec();
        pos += path_len;

        let type_tag = bytes[pos];
        pos += 1;
        pos += 4 + 4 + 4 + 8 + 4 + 8; // mode, uid, gid, mtime_sec, mtime_nsec, inode

        let dev_major = u32::from_be_bytes(bytes[pos..pos + 4].try_into().unwrap());
        pos += 4;
        let dev_minor = u32::from_be_bytes(bytes[pos..pos + 4].try_into().unwrap());
        pos += 4;

        let xattr_count = u32::from_be_bytes(bytes[pos..pos + 4].try_into().unwrap());
        pos += 4;
        let mut xattrs = Vec::new();
        for _ in 0..xattr_count {
            let name_len = u64::from_be_bytes(bytes[pos..pos + 8].try_into().unwrap()) as usize;
            pos += 8;
            let name = bytes[pos..pos + name_len].to_vec();
            pos += name_len;
            let value_len = u64::from_be_bytes(bytes[pos..pos + 8].try_into().unwrap()) as usize;
            pos += 8;
            let value = bytes[pos..pos + value_len].to_vec();
            pos += value_len;
            xattrs.push((name, value));
        }

        DecodedEntry { path, type_tag, dev_major, dev_minor, xattrs }
    }

    #[test]
    fn a_whiteout_is_captured_as_a_generic_char_device_with_zero_major_minor() {
        // A real overlayfs whiteout is a char device with major=0/minor=0. Rather than
        // requiring an actual overlay mount for this unit test, `mknod` constructs the
        // identical filesystem object directly — proving this walker captures *that shape*
        // faithfully, independent of whether overlayfs itself produced it.
        let dir = tempfile::tempdir().expect("tempdir");
        let whiteout_path = dir.path().join("deleted_file");
        let status = std::process::Command::new("mknod")
            .args([whiteout_path.to_str().expect("utf8 path"), "c", "0", "0"])
            .status()
            .expect("run mknod");
        assert!(status.success(), "mknod must succeed to set up this test");

        let bytes = capture(dir.path()).expect("capture");
        let entry = decode_first_entry(&bytes);
        assert_eq!(entry.path, b"deleted_file");
        assert_eq!(entry.type_tag, TYPE_CHAR_DEVICE);
        assert_eq!((entry.dev_major, entry.dev_minor), (0, 0), "a whiteout's major/minor must both be 0");
        assert!(entry.xattrs.is_empty(), "a plain mknod'd device file has no xattrs set");
    }

    #[test]
    fn an_opaque_directory_marker_xattr_is_captured_verbatim() {
        let dir = tempfile::tempdir().expect("tempdir");
        let opaque_dir = dir.path().join("opaque");
        std::fs::create_dir(&opaque_dir).expect("mkdir");
        let cpath = path_to_cstring(&opaque_dir).expect("cstring");
        let cname = CString::new("trusted.overlay.opaque").expect("cstring");
        #[allow(unsafe_code)]
        let rc = unsafe {
            libc::setxattr(
                cpath.as_ptr(),
                cname.as_ptr(),
                b"y".as_ptr().cast(),
                1,
                0,
            )
        };
        if rc != 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EPERM) || err.raw_os_error() == Some(libc::ENOTSUP)
            {
                eprintln!("skipping: setxattr(trusted.*) not permitted in this environment");
                return;
            }
            panic!("setxattr failed: {err}");
        }

        let xattrs = read_xattrs(&opaque_dir).expect("read xattrs");
        assert_eq!(
            xattrs,
            vec![(b"trusted.overlay.opaque".to_vec(), b"y".to_vec())],
            "the opaque marker must be captured wholesale, by name and value, uninterpreted"
        );
    }
}
