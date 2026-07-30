//! The `evtree1` canonical evidence-tree wire format — [ADR-009].
//!
//! This crate is the code ADR-009 describes but that F-07 deliberately left unbuilt: the
//! ADR itself says the wire format's "Follow-on decisions" are for **P1-04** ("implements
//! the walker and (de)serialiser against this spec") and for **P1-02** ("reuses this format
//! and its capture-twice-compare-digests test methodology"). At the time P1-02 needed it,
//! P1-04 had not landed, so this crate exists a phase earlier than ADR-009 anticipated —
//! built once, here, so P1-04 has a shared implementation to extend rather than a second
//! encoder to invent independently. See `docs/adr/009-evidence-tree-serialisation.md` for
//! the full rationale; this module implements exactly the wire format that document
//! specifies and nothing more.
//!
//! # What this crate does and does not do
//!
//! [`encode`] and [`decode`] are a pure, total, round-trip-exact codec between a `Vec<`
//! [`Entry`]`>` and the `evtree1` byte format. Nothing here touches a filesystem, a clock,
//! or any other ambient state — that is deliberate: a directory *walker* (the component that
//! turns a real directory tree into `Entry` values via `lstat`/`listxattr`) is a distinct,
//! I/O-performing concern, layered on top of this codec by its callers (`world`'s
//! `base_layer` module today; `observe` per P1-04, later). Keeping the codec itself free of
//! I/O means it can be tested exhaustively against synthetic entry lists without touching a
//! disk, and reused unchanged by every future caller.
//!
//! # Overlayfs semantics this format represents (design.md §9, ADR-009)
//!
//! Two overlayfs-specific facts must be representable without the codec *interpreting*
//! them (architecture.md §3.1: "Observation collector — must not interpret anything"):
//!
//! - **Whiteout.** Overlayfs marks a lower-layer entry as deleted-in-the-upper-layer by
//!   creating, in the upper layer, a character device special file with device number
//!   `0/0`. This format needs no dedicated "whiteout" variant: [`Payload::CharDevice`] with
//!   [`Entry::dev_major`] `== 0` and [`Entry::dev_minor`] `== 0` at that path *is* a
//!   whiteout, representable with the same generic encoding used for any other char device
//!   a tool might create. Recognising that shape as meaningful is `normalise`'s job
//!   (ADR-005), not this codec's.
//! - **Opaque directory.** Overlayfs marks "do not merge this upper directory with the
//!   lower directory of the same name" with the extended attribute
//!   `trusted.overlay.opaque=y` (or `user.overlay.opaque=y` under the `userxattr` mount
//!   option — ADR-010). This format captures the *complete* xattr set on every entry
//!   generically ([`Entry::xattrs`]); no xattr name is special-cased by the codec. An opaque
//!   directory is simply a [`Payload::Directory`] entry whose xattr list happens to contain
//!   that name/value pair.

use std::fmt;

/// `b"EVTREE1\0"` — the fixed 8-byte magic that opens every capture.
pub const MAGIC: [u8; 8] = *b"EVTREE1\0";

/// The only wire format version this codec currently emits or accepts.
pub const FORMAT_VERSION: u16 = 1;

/// One filesystem object captured from (or destined for) a directory tree.
///
/// Every field mirrors what `lstat(2)`/`listxattr(2)` report for a real captured tree
/// (ADR-009's losslessness requirement), or what a declarative tree builder assigns before
/// any real filesystem object exists. Nothing here is optional or inferred: a value must be
/// supplied for every field, even when it is a fixed constant like `0` for a synthetically
/// constructed entry that has no meaningful inode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Path relative to the captured tree's root, `/`-separated, raw bytes — **not**
    /// assumed to be valid UTF-8. POSIX paths may contain arbitrary non-NUL bytes.
    pub path: Vec<u8>,
    /// POSIX permission and type bits, as `lstat`'s `st_mode` would report them (the type
    /// bits are redundant with [`Payload`]'s discriminant but are still carried here,
    /// verbatim, per the losslessness requirement — this codec does not reconcile the two
    /// or treat a mismatch as an error; that is a `normalise`-side concern, if one ever
    /// arises).
    pub mode: u32,
    /// Owning user ID.
    pub uid: u32,
    /// Owning group ID.
    pub gid: u32,
    /// Modification time, seconds component.
    pub mtime_sec: i64,
    /// Modification time, nanoseconds component.
    pub mtime_nsec: u32,
    /// Inode number. Retained per ADR-009's losslessness requirement even though it is
    /// noise for most purposes — see that ADR's "Losslessness" section for why discarding
    /// it here would itself be a form of normalisation.
    pub inode: u64,
    /// Device major number. Meaningful for char/block-device entries (and, per the
    /// whiteout convention above, `0` there is itself meaningful); `0` and unused for every
    /// other entry type.
    pub dev_major: u32,
    /// Device minor number. See [`Entry::dev_major`].
    pub dev_minor: u32,
    /// The complete extended-attribute set on this entry, name/value pairs. Captured
    /// wholesale — this codec does not filter to overlay-specific names (see the module
    /// doc's "Opaque directory" note).
    pub xattrs: Vec<XAttr>,
    /// The entry's POSIX type and any type-specific payload.
    pub payload: Payload,
}

/// One extended attribute: a name/value pair, both raw bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XAttr {
    /// Attribute name, e.g. `b"trusted.overlay.opaque"`.
    pub name: Vec<u8>,
    /// Attribute value. Not assumed to be text — xattr values are opaque byte strings.
    pub value: Vec<u8>,
}

impl XAttr {
    /// Construct an xattr from string-like name/value for the common case (names are
    /// conventionally ASCII; values are frequently `"y"`-style flags). Byte-level
    /// construction (`XAttr { name: ..., value: ... }`) remains available for the general
    /// case.
    #[must_use]
    pub fn new(name: impl Into<Vec<u8>>, value: impl Into<Vec<u8>>) -> Self {
        Self { name: name.into(), value: value.into() }
    }
}

/// The seven POSIX file types the wire format distinguishes, generically — see the module
/// doc for why there is no dedicated "whiteout" or "opaque directory" variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Payload {
    /// A regular file; its complete byte content, inlined.
    Regular(Vec<u8>),
    /// A directory. Carries no payload of its own — its children are separate entries in
    /// the same flat, sorted entry list, distinguished by their `path` prefix.
    Directory,
    /// A symbolic link and its target, verbatim (not resolved, not validated to exist).
    Symlink(Vec<u8>),
    /// A named pipe.
    Fifo,
    /// A character-special device. See the module doc: `dev_major == 0 && dev_minor == 0`
    /// at a path is how an overlayfs whiteout is represented, with no special casing here.
    CharDevice,
    /// A block-special device.
    BlockDevice,
    /// A Unix domain socket.
    Socket,
}

impl Payload {
    /// The `type_tag` byte the wire format assigns this variant.
    #[must_use]
    pub const fn type_tag(&self) -> u8 {
        match self {
            Self::Regular(_) => 1,
            Self::Directory => 2,
            Self::Symlink(_) => 3,
            Self::Fifo => 4,
            Self::CharDevice => 5,
            Self::BlockDevice => 6,
            Self::Socket => 7,
        }
    }
}

/// Why [`decode`] rejected a byte sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// Fewer bytes remained than the format required at this point.
    UnexpectedEof,
    /// The first 8 bytes were not [`MAGIC`].
    BadMagic,
    /// The header declared a `format_version` this codec does not understand.
    UnsupportedVersion(u16),
    /// An entry's `type_tag` byte was not one of the seven defined values.
    InvalidTypeTag(u8),
    /// The buffer had bytes remaining after the declared `entry_count` entries were parsed.
    TrailingBytes,
    /// A length-prefixed field declared a length that does not fit in this platform's
    /// `usize` (only reachable on 32-bit targets against an adversarially large `u64`).
    LengthOverflow,
    /// Entries were not in strictly ascending raw-path-byte order, or an entry's xattrs
    /// were not in strictly ascending name order. [`encode`] always sorts, so bytes it
    /// produced never trip this; input that does was written by something else, and
    /// accepting it would let two different byte sequences decode to the same logical
    /// tree — breaking the one-encoding-per-content property the content-addressed
    /// evidence store (F-05) relies on. "Strictly" also rejects duplicate paths/names.
    NotCanonical,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnexpectedEof => write!(f, "evtree1: unexpected end of buffer"),
            Self::BadMagic => write!(f, "evtree1: bad magic — not an evtree1 capture"),
            Self::UnsupportedVersion(v) => write!(f, "evtree1: unsupported format_version {v}"),
            Self::InvalidTypeTag(t) => write!(f, "evtree1: invalid type_tag {t}"),
            Self::TrailingBytes => write!(f, "evtree1: trailing bytes after last entry"),
            Self::LengthOverflow => write!(f, "evtree1: length-prefixed field overflows usize"),
            Self::NotCanonical => {
                write!(f, "evtree1: entries or xattrs not in strict canonical sort order")
            }
        }
    }
}

impl std::error::Error for DecodeError {}

// ---------------------------------------------------------------------------------------
// Encode
// ---------------------------------------------------------------------------------------

/// Serialise `entries` to the `evtree1` wire format.
///
/// **Sorts defensively.** The caller's input order is never trusted: entries are always
/// re-sorted by raw path bytes (and each entry's xattrs by raw name bytes) before encoding,
/// so determinism does not depend on every caller remembering to sort — this is what makes
/// the "no random ordering" half of P1-02's exit criterion hold regardless of how a caller
/// (e.g. a `HashMap`-backed walker) happened to enumerate entries.
///
/// Contains no clock read, no random-number generation, and no field populated from
/// anything other than `entries` itself — the byte-reproducibility property required by
/// ADR-009 and P1-02 follows directly from that.
///
/// Paths must be unique across `entries` (two entries at one path describe no valid
/// filesystem tree, and every real caller — a `BTreeMap`-backed spec, a directory walk —
/// guarantees this by construction). Duplicates are not deduplicated here; the resulting
/// bytes are non-canonical and [`decode`] rejects them.
#[must_use]
pub fn encode(entries: &[Entry]) -> Vec<u8> {
    let mut sorted: Vec<&Entry> = entries.iter().collect();
    sorted.sort_by(|a, b| a.path.cmp(&b.path));

    let mut out = Vec::new();
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&FORMAT_VERSION.to_be_bytes());
    out.extend_from_slice(&(sorted.len() as u64).to_be_bytes());

    for entry in sorted {
        write_entry(&mut out, entry);
    }
    out
}

fn write_len_prefixed(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
    out.extend_from_slice(bytes);
}

fn write_entry(out: &mut Vec<u8>, entry: &Entry) {
    write_len_prefixed(out, &entry.path);
    out.push(entry.payload.type_tag());
    out.extend_from_slice(&entry.mode.to_be_bytes());
    out.extend_from_slice(&entry.uid.to_be_bytes());
    out.extend_from_slice(&entry.gid.to_be_bytes());
    out.extend_from_slice(&entry.mtime_sec.to_be_bytes());
    out.extend_from_slice(&entry.mtime_nsec.to_be_bytes());
    out.extend_from_slice(&entry.inode.to_be_bytes());
    out.extend_from_slice(&entry.dev_major.to_be_bytes());
    out.extend_from_slice(&entry.dev_minor.to_be_bytes());

    let mut xattrs: Vec<&XAttr> = entry.xattrs.iter().collect();
    xattrs.sort_by(|a, b| a.name.cmp(&b.name));
    out.extend_from_slice(&(xattrs.len() as u32).to_be_bytes());
    for x in xattrs {
        write_len_prefixed(out, &x.name);
        write_len_prefixed(out, &x.value);
    }

    match &entry.payload {
        Payload::Regular(content) => write_len_prefixed(out, content),
        Payload::Directory | Payload::Fifo | Payload::CharDevice | Payload::BlockDevice | Payload::Socket => {}
        Payload::Symlink(target) => write_len_prefixed(out, target),
    }
}

// ---------------------------------------------------------------------------------------
// Decode
// ---------------------------------------------------------------------------------------

/// Parse the `evtree1` wire format back into entries, sorted by path.
///
/// Exact round-trip inverse of [`encode`] for any byte sequence `encode` itself produced.
/// Also usable against untrusted input (bounds-checked throughout; never panics or reads
/// out of bounds — every length-prefixed field is validated against the remaining buffer
/// before use, and pre-allocation is bounded by the buffer's actual size, never by a
/// self-declared count), since a future evidence-store reader (P1-04's descendants) will
/// eventually decode blobs nothing in this workspace wrote.
///
/// Rejects non-canonical input ([`DecodeError::NotCanonical`]): entries must be strictly
/// ascending by raw path bytes and each entry's xattrs strictly ascending by name, exactly
/// as [`encode`] emits them. A canonical format with a content-addressed store behind it
/// must not admit two byte encodings of one logical tree.
pub fn decode(bytes: &[u8]) -> Result<Vec<Entry>, DecodeError> {
    let mut r = Reader::new(bytes);

    let magic = r.take(8)?;
    if magic != MAGIC {
        return Err(DecodeError::BadMagic);
    }
    let version = r.u16()?;
    if version != FORMAT_VERSION {
        return Err(DecodeError::UnsupportedVersion(version));
    }
    let count = r.u64()?;
    let count = usize::try_from(count).map_err(|_| DecodeError::LengthOverflow)?;

    // Pre-allocation is capped by what the remaining bytes could possibly hold, not by the
    // header's self-declared count: every entry occupies at least MIN_ENTRY_ENCODED_LEN
    // bytes on the wire, so a forged header claiming billions of entries against a
    // near-empty buffer allocates nothing of consequence before the first read fails.
    let mut entries = Vec::with_capacity(count.min(r.remaining() / MIN_ENTRY_ENCODED_LEN));
    for _ in 0..count {
        let entry = read_entry(&mut r)?;
        if entries.last().is_some_and(|prev: &Entry| prev.path >= entry.path) {
            return Err(DecodeError::NotCanonical);
        }
        entries.push(entry);
    }
    if r.remaining() != 0 {
        return Err(DecodeError::TrailingBytes);
    }
    Ok(entries)
}

/// The smallest possible encoded entry: path length prefix (8) + type_tag (1) + mode (4) +
/// uid (4) + gid (4) + mtime_sec (8) + mtime_nsec (4) + inode (8) + dev_major (4) +
/// dev_minor (4) + xattr count (4) — an empty-path directory with no xattrs. Used only to
/// bound pre-allocation in [`decode`].
const MIN_ENTRY_ENCODED_LEN: usize = 8 + 1 + 4 + 4 + 4 + 8 + 4 + 8 + 4 + 4 + 4;

fn read_entry(r: &mut Reader<'_>) -> Result<Entry, DecodeError> {
    let path = r.bytes_len_prefixed()?;
    let type_tag = r.u8()?;
    let mode = r.u32()?;
    let uid = r.u32()?;
    let gid = r.u32()?;
    let mtime_sec = r.i64()?;
    let mtime_nsec = r.u32()?;
    let inode = r.u64()?;
    let dev_major = r.u32()?;
    let dev_minor = r.u32()?;

    let xattr_count = r.u32()?;
    // Same remaining-bytes cap as the entry list: each xattr is at least two u64 length
    // prefixes (16 bytes) on the wire.
    let mut xattrs = Vec::with_capacity((xattr_count as usize).min(r.remaining() / 16));
    for _ in 0..xattr_count {
        let name = r.bytes_len_prefixed()?;
        let value = r.bytes_len_prefixed()?;
        if xattrs.last().is_some_and(|prev: &XAttr| prev.name >= name) {
            return Err(DecodeError::NotCanonical);
        }
        xattrs.push(XAttr { name, value });
    }

    let payload = match type_tag {
        1 => Payload::Regular(r.bytes_len_prefixed()?),
        2 => Payload::Directory,
        3 => Payload::Symlink(r.bytes_len_prefixed()?),
        4 => Payload::Fifo,
        5 => Payload::CharDevice,
        6 => Payload::BlockDevice,
        7 => Payload::Socket,
        other => return Err(DecodeError::InvalidTypeTag(other)),
    };

    Ok(Entry { path, mode, uid, gid, mtime_sec, mtime_nsec, inode, dev_major, dev_minor, xattrs, payload })
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    const fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    const fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        if self.remaining() < n {
            return Err(DecodeError::UnexpectedEof);
        }
        let slice = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, DecodeError> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }

    fn u32(&mut self) -> Result<u32, DecodeError> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn u64(&mut self) -> Result<u64, DecodeError> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }

    fn i64(&mut self) -> Result<i64, DecodeError> {
        Ok(i64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }

    fn bytes_len_prefixed(&mut self) -> Result<Vec<u8>, DecodeError> {
        let len = self.u64()?;
        let len = usize::try_from(len).map_err(|_| DecodeError::LengthOverflow)?;
        Ok(self.take(len)?.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(path: &str) -> Entry {
        Entry {
            path: path.as_bytes().to_vec(),
            mode: 0o755,
            uid: 0,
            gid: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            inode: 0,
            dev_major: 0,
            dev_minor: 0,
            xattrs: Vec::new(),
            payload: Payload::Directory,
        }
    }

    fn file(path: &str, content: &[u8]) -> Entry {
        Entry {
            path: path.as_bytes().to_vec(),
            mode: 0o644,
            uid: 0,
            gid: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            inode: 0,
            dev_major: 0,
            dev_minor: 0,
            xattrs: Vec::new(),
            payload: Payload::Regular(content.to_vec()),
        }
    }

    #[test]
    fn empty_capture_round_trips() {
        let bytes = encode(&[]);
        assert_eq!(&bytes[..8], &MAGIC);
        let decoded = decode(&bytes).unwrap();
        assert!(decoded.is_empty());
    }

    #[test]
    fn round_trip_preserves_every_field_for_each_entry_type() {
        let entries = vec![
            Entry {
                path: b"a/regular".to_vec(),
                mode: 0o644,
                uid: 1000,
                gid: 1000,
                mtime_sec: 1_753_000_000,
                mtime_nsec: 123_456_789,
                inode: 42,
                dev_major: 0,
                dev_minor: 0,
                xattrs: vec![XAttr::new("user.foo", "bar")],
                payload: Payload::Regular(b"hello world".to_vec()),
            },
            dir("a"),
            Entry {
                path: b"a/link".to_vec(),
                mode: 0o777,
                uid: 0,
                gid: 0,
                mtime_sec: -1,
                mtime_nsec: 0,
                inode: 7,
                dev_major: 0,
                dev_minor: 0,
                xattrs: Vec::new(),
                payload: Payload::Symlink(b"../target".to_vec()),
            },
            Entry {
                path: b"a/fifo".to_vec(),
                mode: 0o600,
                uid: 0,
                gid: 0,
                mtime_sec: 0,
                mtime_nsec: 0,
                inode: 9,
                dev_major: 0,
                dev_minor: 0,
                xattrs: Vec::new(),
                payload: Payload::Fifo,
            },
            Entry {
                path: b"a/sock".to_vec(),
                mode: 0o600,
                uid: 0,
                gid: 0,
                mtime_sec: 0,
                mtime_nsec: 0,
                inode: 10,
                dev_major: 0,
                dev_minor: 0,
                xattrs: Vec::new(),
                payload: Payload::Socket,
            },
            Entry {
                path: b"a/whiteout".to_vec(),
                mode: 0,
                uid: 0,
                gid: 0,
                mtime_sec: 0,
                mtime_nsec: 0,
                inode: 11,
                dev_major: 0,
                dev_minor: 0,
                xattrs: Vec::new(),
                payload: Payload::CharDevice,
            },
            Entry {
                path: b"a/blockdev".to_vec(),
                mode: 0,
                uid: 0,
                gid: 0,
                mtime_sec: 0,
                mtime_nsec: 0,
                inode: 12,
                dev_major: 8,
                dev_minor: 1,
                xattrs: Vec::new(),
                payload: Payload::BlockDevice,
            },
        ];

        let bytes = encode(&entries);
        let mut decoded = decode(&bytes).unwrap();
        let mut expected = entries;
        expected.sort_by(|a, b| a.path.cmp(&b.path));
        decoded.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(decoded, expected);
    }

    #[test]
    fn encode_sorts_by_raw_path_bytes_regardless_of_input_order() {
        let forward = encode(&[dir("a"), dir("b"), dir("c")]);
        let reverse = encode(&[dir("c"), dir("b"), dir("a")]);
        let shuffled = encode(&[dir("b"), dir("a"), dir("c")]);
        assert_eq!(forward, reverse);
        assert_eq!(forward, shuffled);
    }

    #[test]
    fn encode_sorts_xattrs_by_raw_name_bytes_regardless_of_input_order() {
        let mut e1 = dir("a");
        e1.xattrs = vec![XAttr::new("zzz", "1"), XAttr::new("aaa", "2")];
        let mut e2 = dir("a");
        e2.xattrs = vec![XAttr::new("aaa", "2"), XAttr::new("zzz", "1")];
        assert_eq!(encode(&[e1]), encode(&[e2]));
    }

    #[test]
    fn identical_entries_built_independently_encode_byte_identical() {
        // Simulates "two independent constructions" at the codec level: two entry lists
        // built from scratch by unrelated code paths, containing the same logical data.
        let build = || vec![file("x", b"1"), dir("y"), file("y/z", b"2")];
        assert_eq!(encode(&build()), encode(&build()));
    }

    #[test]
    fn whiteout_is_a_char_device_with_zero_major_minor() {
        // design.md §9 / ADR-009: a whiteout is a plain char-device entry at dev 0/0. No
        // dedicated variant exists — this test proves the generic encoding represents it
        // and that decode reconstructs the exact same fact.
        let whiteout = Entry {
            path: b"deleted-by-upper".to_vec(),
            mode: 0,
            uid: 0,
            gid: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            inode: 99,
            dev_major: 0,
            dev_minor: 0,
            xattrs: Vec::new(),
            payload: Payload::CharDevice,
        };
        let decoded = decode(&encode(&[whiteout.clone()])).unwrap();
        assert_eq!(decoded, vec![whiteout]);
        assert_eq!(decoded[0].payload.type_tag(), 5);
        assert_eq!((decoded[0].dev_major, decoded[0].dev_minor), (0, 0));
    }

    #[test]
    fn opaque_directory_is_a_directory_with_the_overlay_xattr() {
        // ADR-009: opaque directories are represented as an ordinary directory entry
        // carrying `trusted.overlay.opaque=y` (or `user.overlay.opaque=y` under
        // `userxattr`, ADR-010) in its generically-captured xattr set. No dedicated flag.
        let mut opaque = dir("shadowed");
        opaque.xattrs = vec![XAttr::new("trusted.overlay.opaque", "y")];
        let decoded = decode(&encode(&[opaque.clone()])).unwrap();
        assert_eq!(decoded, vec![opaque]);
        assert_eq!(decoded[0].xattrs[0].name, b"trusted.overlay.opaque");
        assert_eq!(decoded[0].xattrs[0].value, b"y");
    }

    #[test]
    fn userxattr_opaque_marker_is_equally_representable() {
        let mut opaque = dir("shadowed");
        opaque.xattrs = vec![XAttr::new("user.overlay.opaque", "y")];
        let decoded = decode(&encode(&[opaque.clone()])).unwrap();
        assert_eq!(decoded, vec![opaque]);
    }

    #[test]
    fn path_bytes_need_not_be_valid_utf8() {
        let mut e = dir("placeholder");
        e.path = vec![0x61, 0xFF, 0xFE, 0x62]; // not valid UTF-8
        let decoded = decode(&encode(&[e.clone()])).unwrap();
        assert_eq!(decoded[0].path, e.path);
    }

    #[test]
    fn decode_rejects_bad_magic() {
        let mut bytes = encode(&[dir("a")]);
        bytes[0] = b'X';
        assert_eq!(decode(&bytes), Err(DecodeError::BadMagic));
    }

    #[test]
    fn decode_rejects_unsupported_version() {
        let mut bytes = encode(&[]);
        bytes[8..10].copy_from_slice(&99u16.to_be_bytes());
        assert_eq!(decode(&bytes), Err(DecodeError::UnsupportedVersion(99)));
    }

    #[test]
    fn decode_rejects_truncated_buffer() {
        let bytes = encode(&[file("a", b"hello")]);
        for cut in 1..bytes.len() {
            let truncated = &bytes[..cut];
            // Every truncation must either fail cleanly or (only at the exact full length)
            // succeed — it must never panic or read out of bounds.
            let _ = decode(truncated);
        }
        assert!(decode(&bytes[..bytes.len() - 1]).is_err());
    }

    #[test]
    fn decode_rejects_trailing_bytes() {
        let mut bytes = encode(&[dir("a")]);
        bytes.push(0xAB);
        assert_eq!(decode(&bytes), Err(DecodeError::TrailingBytes));
    }

    #[test]
    fn decode_rejects_invalid_type_tag() {
        let mut bytes = encode(&[dir("a")]);
        // The type_tag byte sits right after the length-prefixed path "a" (u64 len + 1
        // byte): 8 (magic) + 2 (version) + 8 (count) + 8 (path len) + 1 (path byte) = 27.
        let type_tag_offset = 8 + 2 + 8 + 8 + 1;
        assert_eq!(bytes[type_tag_offset], 2); // sanity: this is indeed Directory's tag
        bytes[type_tag_offset] = 0;
        assert_eq!(decode(&bytes), Err(DecodeError::InvalidTypeTag(0)));
    }

    /// The 18-byte header of a valid capture, followed by a forged `entry_count`.
    fn forged_header(count: u64) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&MAGIC);
        bytes.extend_from_slice(&FORMAT_VERSION.to_be_bytes());
        bytes.extend_from_slice(&count.to_be_bytes());
        bytes
    }

    /// A header claiming u64::MAX entries against an empty remainder must fail fast with
    /// no allocation of consequence — the pre-allocation cap is derived from the bytes
    /// actually present, never from the header's own claim.
    #[test]
    fn decode_does_not_preallocate_from_a_forged_entry_count() {
        assert_eq!(decode(&forged_header(u64::MAX)), Err(DecodeError::UnexpectedEof));
    }

    #[test]
    fn decode_rejects_entries_out_of_canonical_order() {
        let a = encode(&[dir("a")]);
        let b = encode(&[dir("b")]);
        // Splice the two single-entry bodies together in the wrong order under a count=2
        // header. 18 = magic (8) + version (2) + entry_count (8).
        let mut bytes = forged_header(2);
        bytes.extend_from_slice(&b[18..]);
        bytes.extend_from_slice(&a[18..]);
        assert_eq!(decode(&bytes), Err(DecodeError::NotCanonical));
    }

    #[test]
    fn decode_rejects_duplicate_paths() {
        let a = encode(&[dir("a")]);
        let mut bytes = forged_header(2);
        bytes.extend_from_slice(&a[18..]);
        bytes.extend_from_slice(&a[18..]);
        assert_eq!(decode(&bytes), Err(DecodeError::NotCanonical));
    }

    #[test]
    fn decode_rejects_xattrs_out_of_canonical_order() {
        let mut e = dir("a");
        e.xattrs = vec![XAttr::new("aaa", "1"), XAttr::new("zzz", "2")];
        let mut bytes = encode(&[e]);
        // Locate the two xattr blocks and swap them. Offsets: header 18 + path prefix 8 +
        // path "a" 1 + type_tag 1 + fixed numeric fields 40 + xattr count 4 = 72. Each
        // xattr is name prefix 8 + name 3 + value prefix 8 + value 1 = 20 bytes.
        let first = 72..92;
        let second = 92..112;
        assert_eq!(&bytes[first.start + 8..first.start + 11], b"aaa", "offset sanity");
        assert_eq!(&bytes[second.start + 8..second.start + 11], b"zzz", "offset sanity");
        let swapped: Vec<u8> = [
            &bytes[..first.start],
            &bytes[second.clone()],
            &bytes[first.clone()],
            &bytes[second.end..],
        ]
        .concat();
        assert_ne!(swapped, bytes);
        bytes = swapped;
        assert_eq!(decode(&bytes), Err(DecodeError::NotCanonical));
    }

    #[test]
    fn deeply_nested_paths_round_trip() {
        let deep_path = (0..50).map(|i| format!("level{i}")).collect::<Vec<_>>().join("/");
        let entries = vec![dir(&deep_path)];
        let decoded = decode(&encode(&entries)).unwrap();
        assert_eq!(decoded, entries);
    }

    #[test]
    fn many_entries_preserve_forced_sort_order_on_decode() {
        let mut entries: Vec<Entry> = (0..500).map(|i| dir(&format!("n{i:04}"))).collect();
        entries.reverse(); // feed them in intentionally wrong order
        let decoded = decode(&encode(&entries)).unwrap();
        let mut expected = entries;
        expected.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(decoded, expected);
        // and prove it is genuinely sorted, not just equal to a re-sorted expectation
        for w in decoded.windows(2) {
            assert!(w[0].path < w[1].path);
        }
    }
}
