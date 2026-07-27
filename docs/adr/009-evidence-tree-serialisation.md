# ADR-009: Canonical evidence-tree serialisation

**Status:** Accepted
**Date:** 2026-07-27
**Author:** Jun Cui
**Closes:** [F-07](../tasks.md#f-07-canonical-evidence-tree-serialisation--adr-009)
**Blocks:** P1-04 (Observation collector)
**Related:** F-05 (`store::BlobStore`), `datamodel::RawEvidence`, design.md §9, architecture.md
§12 item 5 (P1-02 base-layer byte-reproducibility), ADR-005 (pure, offline derivation)

---

## Context

`datamodel::RawEvidence` is currently an empty, `#[non_exhaustive]` placeholder. Its doc
comment states two constraints and defers the rest:

> Shape is deferred to ADR-009 (F-07). Two constraints already bind it: the primary artifact
> is a *directory tree*, not a byte string, and **capture is lossless** — mtimes and inode
> data are noise but must still be stored, because discarding them at capture time is
> normalisation, and ADR-005 requires normalisation to be a pure function of stored evidence.

F-05's `BlobStore` (`crates/store/src/lib.rs`) only knows how to store and retrieve flat
byte sequences, each addressed by the SHA-256 digest of its own content (`put(&[u8]) ->
Digest`, `get(&Digest) -> Vec<u8>`). It has no notion of a tree. architecture.md §6's
`EVIDENCE` entity carries a single `blob_ref` per row, one row per `(run_id, kind)` pair —
the data model already commits to *one blob per observation surface*, not a graph of blobs.
This ADR decides what bytes go into that one blob when `kind = upper_layer`, and how they
get back out.

Two things from earlier docs bind the answer:

- **design.md §9 / architecture.md §5**: the overlay *upper layer* is captured directly, not
  the merged mount. On Linux, overlayfs represents a deletion of a lower-layer entry as a
  **whiteout** — a character device special file with device number 0/0 — and represents "do
  not merge with the lower directory of the same name" as an **opaque directory**, marked by
  the extended attribute `trusted.overlay.opaque=y` (or `user.overlay.opaque=y` under the
  `userxattr` mount option — see ADR-010). Both are *facts about the upper layer as it sits
  on disk*, not something a directory walk interprets away. If the serialiser cannot
  represent a character device or an xattr, it cannot represent a whiteout or an opaque
  directory, and the read-only/idempotency protocols in architecture.md §4.2–§4.3 lose
  exactly the evidence they most need.
- **architecture.md §3.1**: "Observation collector — Must not: Interpret anything." The
  serialisation format is consumed by that component. It must capture what `lstat` and
  `listxattr` report, uniformly, and must not itself decide "this char device is a whiteout"
  — that classification is `normalise`'s job, over `(evidence, ruleset)`, per ADR-005.

---

## Decision

A **bespoke, sorted, single-blob format** — call it `evtree1` internally. One captured
directory tree serialises to exactly one `Vec<u8>`, which goes into `BlobStore::put` as-is
and comes back out through `BlobStore::get` unchanged; the resulting `Digest` is the
`EVIDENCE.blob_ref` for that run's `kind = "upper_layer"` row. No second blob, no manifest
indirection, no partial tree stored separately from its content.

The walker (P1-04's job, not this ADR's) must use `lstat`, never `stat`, so it observes
symlinks and device-special files as themselves rather than following or resolving them,
and must read the upper directory from the host side (outside any overlay mount), so
whiteouts and opaque markers are seen as the plain filesystem objects they are rather than
being interpreted by the kernel's overlay merge logic before the walker ever sees them.

### Wire format

A capture is a sequence of **entries**, one per filesystem object found under the upper
layer root (the root itself is not an entry — its content is the entry list).

**Framing.** All multi-byte integers are big-endian, fixed-width — never native-endian,
never variable-length (LEB128 etc.), so the byte layout does not depend on which machine
produced or reads it. Every variable-length field (path, symlink target, xattr name/value,
file content) is length-prefixed with an explicit `u64`, never null-terminated — POSIX paths
and xattr values may contain arbitrary non-NUL bytes, and a length prefix has no encoding
ambiguity a terminator-based scheme would.

```
capture      := header entry*
header       := magic(8) format_version(u16) entry_count(u64)
magic        := b"EVTREE1\0"

entry        := path type_tag mode(u32) uid(u32) gid(u32)
                mtime_sec(i64) mtime_nsec(u32)
                inode(u64) dev_major(u32) dev_minor(u32)
                xattr_count(u32) xattr*
                type_payload

path         := len(u64) bytes(len)        -- raw bytes, NOT assumed UTF-8; '/'-separated;
                                            -- relative to the upper-layer root

type_tag     := u8   -- 1=regular 2=directory 3=symlink 4=fifo 5=char-device
                      -- 6=block-device 7=socket
                      -- (POSIX file types, generically — see "Why generic POSIX types")

xattr        := name_len(u64) name(name_len) value_len(u64) value(value_len)
                -- sorted by raw name bytes; captures ALL xattrs, not just the overlay ones

type_payload := regular:    size(u64) content(size)
             |  directory:  (nothing further — dev_major/minor unused, zeroed)
             |  symlink:    target_len(u64) target(target_len)
             |  fifo:       (nothing further)
             |  char/block: (nothing further — dev_major/dev_minor above carry the info;
                             this is how a whiteout, major=0 minor=0, is represented)
             |  socket:     (nothing further)
```

**Sort order.** Entries are sorted by `path`, compared as raw byte strings (`memcmp`
semantics — not locale-aware, not case-folded). `readdir(3)` order is unspecified by POSIX
and varies with the on-disk hash layout, insertion history, and filesystem implementation;
without an explicit sort, two captures of the identical tree could legally emit entries in
different orders. Byte-wise sort needs no locale data, and produces one canonical order
regardless of which libc or filesystem the capture ran under.

**Why generic POSIX types, not an overlay-specific "whiteout" tag.** A whiteout is exactly
"a character device, mode roughly `0`, `dev_major=0`, `dev_minor=0`, at a path that also
exists in the lower layer." Every one of those facts is representable by the generic
char-device entry above with no special case. Baking `is_whiteout` into the serialiser would
require it to know the lower layer's contents (it doesn't — it walks the upper layer only)
and would cross into interpretation, which §3.1 forbids this component from doing. The
generic encoding also captures FIFOs and sockets a hostile tool creates in its own writable
directory — an unusual thing for a tool to do, and precisely the kind of thing this project
exists to notice rather than silently drop because it wasn't anticipated.

**Why xattrs are captured wholesale, not just `trusted.overlay.opaque`.** Same reasoning:
hardcoding one magic xattr name into the format would be interpretation-by-omission — a
tool setting some other security-relevant xattr on a file it wrote would be invisible.
Capturing the full xattr set, sorted by name for the same determinism reason as the entry
list, keeps the walker dumb and the format complete. `normalise` (ADR-008's taxonomy plus
whatever ADR-009-consuming code P1-06 writes) is where `trusted.overlay.opaque` /
`user.overlay.opaque` gets recognised as meaningful.

**Content is inlined, not blob-per-file.** A regular file's bytes sit directly in its entry.
See "Options considered" for why a git-tree-style per-file blob graph was rejected.

---

## Reproducibility properties

**The property this ADR proves: capturing the same on-disk tree twice, without
modification in between, serialises to byte-identical output.** This follows directly from
the format's construction:

1. Traversal order is forced by the explicit sort, independent of `readdir` order.
2. No field is populated from anything other than `lstat`/`listxattr`/file content on that
   walk — no wall-clock timestamp of *when the capture ran*, no random padding, no
   process-specific data (PID, hostname) anywhere in the format. Re-stating the format's own
   discipline: **the serialiser reads nothing outside the tree it is walking.**
2. Fixed-width, big-endian integers remove the one remaining source of nondeterminism a
   naive `#[repr(C)]` struct dump would have — native struct layout depends on the
   compiling/running architecture's endianness and alignment rules.

This is a narrower, and much cheaper to establish, claim than "two *different constructions*
of a base layer produce the same tree." That property belongs to **P1-02** (the overlayfs
base-layer builder's own byte-reproducibility exit criterion, architecture.md §12 item 5) and
is not decided here — P1-02 has to make the *construction itself* deterministic (fixed file
ordering when writing the base, no ambient timestamps baked in by whatever tool populates it,
etc.) before this serialiser's determinism is enough to prove the base layer reproduces.
What this ADR guarantees P1-02 for free is the *second half* of that proof: once P1-02's
construction is deterministic, capturing its result with this format and comparing digests
is a sufficient test — P1-02 does not need to invent its own tree-comparison logic, only
reuse this one and assert equal `Digest`s from two from-scratch builds.

The read-back path (`BlobStore::get`) is exact by F-05's own contract (byte-identical
put/get, proven by F-05's `put_then_get_is_byte_identical` test) — this ADR's format does
not need to re-establish that; it only needs the *serialisation* step to be deterministic,
which is the part F-05 could not have guaranteed on its own.

---

## Losslessness

Everything `lstat` and `listxattr` report on a walk is retained: mode bits, `uid`/`gid`,
`mtime` at nanosecond resolution, inode number, device major/minor, the full xattr set, and
— for regular files — the complete byte content. None of this is filtered, rounded, or
dropped at capture time, including the fields the project already expects to be mostly noise
(`inode`, `uid`/`gid` under a remapped user namespace, `mtime`). Per `RawEvidence`'s existing
contract and ADR-005: throwing any of this away here would be normalisation happening
outside `normalise`, silently, in a component whose contract is "interpret nothing." ADR-008
already establishes where the *first* real interpretation of this data happens (the
`user_state`/`server_internal`/`ephemeral` path taxonomy, operating on the `path` field);
nothing upstream of `normalise` is allowed to pre-filter what reaches it.

One explicit non-goal: **compression.** Nothing above precludes `BlobStore` or its future
object-store backend (P5-01) from compressing blobs transparently at the storage layer. That
is a storage-engine concern, orthogonal to what bytes the format logically contains, and is
not decided here.

---

## Options considered

| Option | Shape | Precision | Cost |
|---|---|---|---|
| **A: bespoke, sorted, single blob** (chosen) | One blob per capture, content inlined | Full — see Losslessness | Simple: one `BlobStore::put` call, one digest, no second-order graph to walk to reconstruct a tree |
| B: git-tree-like recursive Merkle DAG (blob-per-file, tree-per-directory, root digest) | Many blobs, content-addressed per file, dedup across runs/tools | Full | Extra indirection for no proven benefit yet: dedup only pays off when files are byte-identical across *different* tool runs, which is not the common case (§ below); `EVIDENCE.blob_ref` would need to become "root tree digest" plus a reachability walk to actually harvest all referenced blobs, complicating the simple single-row-single-blob shape architecture.md §6 already committed to |
| C: PAX-extended `tar`/USTAR via an existing crate | One blob, external format | **Lossy** — classic `ustar` mtime is 1-second resolution, not nanosecond; no native xattr support without PAX extended headers, which are themselves an underspecified, vendor-extension-prone area | Canonicalisation (fixed entry order, no PAX ambiguity) still has to be built on top regardless of using a real tar library, so the "reuse an existing format" benefit is mostly absorbed by that work; the precision loss alone is disqualifying against the lossless requirement |
| D: JSON/CBOR manifest + externally-referenced content blobs | One "index" blob plus N content blobs | Full | Same multi-blob indirection cost as B, plus a serialisation-format dependency (`serde_json`/`ciborium`) that the observation collector — an I/O crate, not one of the pure ones, so this isn't an ADR-005 purity concern — doesn't otherwise need |

**On B specifically, since it is the most tempting alternative.** Git's blob/tree model
earns its complexity because a git repository's whole point is diffing and storing many
*related* trees cheaply over time (commit history). A single conformance run's evidence
tree is captured once, hashed once, and is compared to *at most* one or two sibling captures
(`D1`/`D1'`/`D2`/`D2R` in architecture.md §4.2) — by the normaliser, over fully-materialised
in-memory data, not by walking a blob graph. There is no repeated-history use case here to
amortise the DAG's cost against, and `RawEvidence` (the in-memory type `normalise` actually
consumes) is populated from one deserialised capture regardless of which wire format
produced it — so choosing B would only change how bytes sit in `BlobStore`, not simplify
anything downstream. Revisit if per-file dedup across a large corpus of runs becomes a
measured storage-cost problem (a P5-01 concern, not a Phase 1 one).

---

## How this fits the data model

- `EVIDENCE.kind = "upper_layer"`, `EVIDENCE.blob_ref` = the `Digest` returned by
  `BlobStore::put(capture_bytes)`.
- `RawEvidence` (the in-memory, `normalise`-facing type) is the *parsed* form of one
  capture — a `Vec` of decoded entries — reconstructed by deserialising the blob read back
  from the store. Deserialisation is I/O-adjacent (it runs wherever the blob was fetched
  from) and is not itself part of the pure `normalise`/`verdict` closure; `RawEvidence` is
  handed to `normalise` already parsed, exactly as `Ruleset` already is (ADR-007 §"Layer 3").
- Two independent `EVIDENCE` rows (e.g. `D1` and `D2` in the idempotency protocol) are two
  independently-addressed blobs; nothing in this format merges or diffs them — diffing is
  `normalise`'s job over two decoded `RawEvidence` values, not a property of storage.

---

## Consequences

**Good.**

- Whiteouts and opaque directories are representable without any overlay-specific logic in
  the serialiser — both fall out of the generic POSIX-type-plus-xattr encoding, which also
  means the format has no gap for a filesystem object type nobody anticipated.
- One blob per capture matches architecture.md §6's `EVIDENCE` shape exactly; no schema
  change and no second table for a "tree manifest."
- The reproducibility property this ADR proves (same tree, two captures, identical bytes) is
  cheap to state, cheap to test, and is exactly the building block P1-02 needs for its own,
  harder property — this ADR does that work once instead of leaving P1-02 to reinvent it.
- Full losslessness at capture time keeps `normalise` the *only* place interpretation
  happens, preserving ADR-005's reproducibility guarantee end to end: re-deriving verdicts
  under a new ruleset never requires new capture data, because none was ever discarded.

**Costs, accepted.**

- **No cross-run content dedup.** Two runs of the same tool that write an identical 50 MB
  file each store that file's bytes once *within* their own capture but the two captures are
  separate blobs, so the 50 MB is stored twice across the two `EVIDENCE` rows. Accepted for
  Phase 1–4; revisit under P5-01 if measured evidence-store growth demands it (Option B is
  the natural escape hatch, and nothing in this ADR forecloses adopting it later — the
  in-memory `RawEvidence` shape `normalise` consumes would not need to change, only how bytes
  are laid out in `BlobStore`).
- **Full-content inlining means large writes produce large blobs.** A tool that writes a
  multi-gigabyte file makes the entire capture that large. No streaming/chunking is
  specified here; P1-04 may need to chunk the write into `BlobStore` calls at the I/O level
  even though the *logical* format is unchanged (this is an implementation concern for P1-04,
  not a format concern for this ADR).
- **Bespoke format, not a standard one.** Nothing outside this project can open an `evtree1`
  blob with an off-the-shelf tool the way it could inspect a `.tar`. Accepted: the precision
  requirement (nanosecond mtimes, full xattrs, generic device files) ruled out every
  standard format considered (Option C), and a bespoke reader is a small, self-contained
  piece of P1-04, not a burden imposed on evidence consumers outside the harness — evidence
  is consumed through `normalise`, never by a human opening the blob directly.

---

## Follow-on decisions this ADR does not make

- **P1-04** implements the walker and (de)serialiser against this spec, and is where the
  "must use `lstat`, must read the upper layer from the host side" requirements above
  actually get enforced in code.
- **P1-02** reuses this format and its capture-twice-compare-digests test methodology to
  prove base-layer construction reproducibility — a property about the *construction*, which
  this ADR does not attempt to guarantee, only the *measurement* of it.
- **P1-06 / ADR-008** consumes decoded `RawEvidence` entries — the taxonomy already operates
  on `path`; it will also be where `trusted.overlay.opaque` / `user.overlay.opaque` and the
  major=0/minor=0 whiteout convention are first given semantic meaning, per §3.1's
  "interpret nothing" boundary living in `normalise`, not here.
- **ADR-010 (F-00)** decides `redirect_dir`/`metacopy`/`index`/`userxattr` mount options,
  which determine whether the opaque-directory xattr this format captures verbatim shows up
  under the `trusted.*` or `user.*` namespace — this format is agnostic to which; it stores
  whatever name/value pairs `listxattr` reports either way.
