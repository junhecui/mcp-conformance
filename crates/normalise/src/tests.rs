//! Unit and property tests for `normalise`. See ADR-011 for the semantics under test.

use super::*;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::ToString;
use alloc::vec;
use datamodel::Digest;
use evtree::XAttr;

// ---------------------------------------------------------------------------------------
// Builders
// ---------------------------------------------------------------------------------------

fn entry(path: &str, payload: Payload) -> Entry {
    Entry {
        path: path.as_bytes().to_vec(),
        mode: match payload {
            Payload::Directory => 0o040_755,
            Payload::Regular(_) => 0o100_644,
            _ => 0,
        },
        uid: 1000,
        gid: 1000,
        mtime_sec: 0,
        mtime_nsec: 0,
        inode: 0,
        dev_major: 0,
        dev_minor: 0,
        xattrs: Vec::new(),
        payload,
    }
}

fn dir(path: &str) -> Entry {
    entry(path, Payload::Directory)
}

fn file(path: &str, content: &str) -> Entry {
    entry(path, Payload::Regular(content.as_bytes().to_vec()))
}

fn whiteout(path: &str) -> Entry {
    entry(path, Payload::CharDevice)
}

fn with_xattr(mut e: Entry, name: &str, value: &str) -> Entry {
    e.xattrs.push(XAttr::new(name, value));
    e.xattrs.sort_by(|a, b| a.name.cmp(&b.name));
    e
}

/// What overlayfs typically leaves on a copied-up directory: a fresh inode, a fresh mtime
/// (a child was created) and private bookkeeping xattrs. None of it is a mutation.
fn copied_up(mut e: Entry) -> Entry {
    e.inode = 0xDEAD_BEEF;
    e.mtime_sec = 1_800_000_000;
    e.mtime_nsec = 42;
    let e = with_xattr(e, "trusted.overlay.origin", "\x00fh-bytes");
    with_xattr(e, "trusted.overlay.impure", "y")
}

/// ADR-008's ruleset v1 lists, as data. The digest is a placeholder: this is a hand-built
/// ruleset, attested by nothing (see `datamodel::Ruleset`).
fn v1() -> Ruleset {
    Ruleset {
        version: "v1".to_string(),
        source_digest: Digest::from_bytes([7; 32]),
        ephemeral: ["/tmp/**", "/var/tmp/**", "/run/**", "**/*.lock", "**/*.pid", "**/*.sock"]
            .iter()
            .map(|s| (*s).to_string())
            .collect(),
        server_internal: [
            "**/.cache/**",
            "**/.config/**",
            "**/.local/state/**",
            "**/__pycache__/**",
            "**/node_modules/.cache/**",
        ]
        .iter()
        .map(|s| (*s).to_string())
        .collect(),
    }
}

fn evidence(base: &[Entry], upper: &[Entry]) -> RawEvidence {
    RawEvidence { base_layer: evtree::encode(base), upper_layer: evtree::encode(upper) }
}

fn run(base: &[Entry], upper: &[Entry]) -> CanonicalChangeset {
    normalise(&evidence(base, upper), &v1()).expect("well-formed evidence")
}

fn paths(changes: &[Change]) -> Vec<&str> {
    changes.iter().map(|c| core::str::from_utf8(&c.path).unwrap()).collect()
}

fn only(changes: &[Change]) -> &ChangeKind {
    assert_eq!(changes.len(), 1, "expected exactly one change, got {changes:?}");
    &changes[0].kind
}

// ---------------------------------------------------------------------------------------
// Overlay semantics (ADR-011 decision b)
// ---------------------------------------------------------------------------------------

#[test]
fn empty_upper_layer_is_an_empty_changeset() {
    let cs = run(&[dir("home")], &[]);
    assert!(cs.user_state.is_empty() && cs.server_internal.is_empty() && cs.ephemeral.is_empty());
    assert_eq!(cs.ruleset_identity, v1().identity());
    assert!(cs.ruleset_identity.starts_with("v1+sha256:0707"));
}

#[test]
fn new_file_is_created() {
    let cs = run(&[dir("home")], &[copied_up(dir("home")), file("home/notes.txt", "hi")]);
    assert_eq!(paths(&cs.user_state), ["/home/notes.txt"]);
    match only(&cs.user_state) {
        ChangeKind::Created(node) => {
            assert_eq!(node.file_type, FileType::Regular);
            assert_eq!(node.data, b"hi");
        }
        other => panic!("expected Created, got {other:?}"),
    }
}

#[test]
fn changed_content_is_modified() {
    let cs = run(&[dir("d"), file("d/f", "old")], &[copied_up(dir("d")), file("d/f", "new")]);
    assert!(matches!(
        only(&cs.user_state),
        ChangeKind::Modified { content_changed: true, metadata_changed: false, .. }
    ));
}

#[test]
fn copied_up_but_unchanged_file_is_still_reported() {
    // Opened for writing (or touched) without a content change: presence in the upper
    // layer is evidence of a write and resolves toward *less* read-only.
    let mut touched = file("f", "same");
    touched.mtime_sec = 99;
    let cs = run(&[file("f", "same")], &[touched]);
    assert!(matches!(
        only(&cs.user_state),
        ChangeKind::Modified { content_changed: false, metadata_changed: false, .. }
    ));
}

#[test]
fn chmod_is_a_metadata_change() {
    let mut chmodded = file("f", "same");
    chmodded.mode = 0o100_600;
    let cs = run(&[file("f", "same")], &[chmodded]);
    assert!(matches!(
        only(&cs.user_state),
        ChangeKind::Modified { content_changed: false, metadata_changed: true, .. }
    ));
}

#[test]
fn non_overlay_xattr_change_is_a_metadata_change_and_kept_on_the_node() {
    let upper = with_xattr(file("f", "same"), "security.selinux", "label");
    let cs = run(&[file("f", "same")], &[upper]);
    match only(&cs.user_state) {
        ChangeKind::Modified { node, metadata_changed: true, .. } => {
            assert_eq!(node.xattrs, vec![(b"security.selinux".to_vec(), b"label".to_vec())]);
        }
        other => panic!("expected a metadata change, got {other:?}"),
    }
}

#[test]
fn overlay_private_xattrs_are_stripped_and_never_a_metadata_change() {
    let upper = with_xattr(file("f", "x"), "user.overlay.origin", "fh");
    let cs = run(&[file("f", "x")], &[upper]);
    match only(&cs.user_state) {
        ChangeKind::Modified { node, metadata_changed: false, .. } => assert!(node.xattrs.is_empty()),
        other => panic!("unexpected {other:?}"),
    }
}

/// Every leaf name overlayfs itself writes, in both namespaces, is still stripped.
#[test]
fn every_real_overlay_private_name_is_stripped() {
    const NAMES: [&str; 9] = [
        "impure", "metacopy", "nlink", "opaque", "origin", "protattr", "redirect", "upper",
        "uuid",
    ];
    for ns in ["trusted.overlay.", "user.overlay."] {
        let mut upper = file("f", "x");
        for n in NAMES {
            upper = with_xattr(upper, &format!("{ns}{n}"), "kernel-bookkeeping");
        }
        let cs = run(&[file("f", "x")], &[upper]);
        match only(&cs.user_state) {
            ChangeKind::Modified { node, metadata_changed: false, .. } => {
                assert!(node.xattrs.is_empty(), "{ns}: leaked {:?}", node.xattrs);
            }
            other => panic!("{ns}: unexpected {other:?}"),
        }
    }
}

/// A name the kernel does **not** write is an ordinary xattr, even inside an overlay
/// namespace. `user.*` belongs to the file's owner, which is the tool under test; stripping
/// by prefix made its writes invisible to both `same_metadata` and the node (ADR-011
/// decision 5).
#[test]
fn an_unrecognised_overlay_namespace_xattr_is_reported_not_stripped() {
    for name in ["user.overlay.stolen", "trusted.overlay.stolen", "user.overlay.opaquex"] {
        let upper = with_xattr(file("f", "x"), name, "payload");
        let cs = run(&[file("f", "x")], &[upper]);
        match only(&cs.user_state) {
            ChangeKind::Modified { node, content_changed: false, metadata_changed: true } => {
                assert_eq!(
                    node.xattrs,
                    vec![(name.as_bytes().to_vec(), b"payload".to_vec())],
                    "{name}"
                );
            }
            other => panic!("{name}: expected a reported metadata change, got {other:?}"),
        }
    }
}

/// The exploit shape the P1-06 security review demonstrated against a real overlay mounted
/// with ADR-010's pinned options in trusted mode: a tool declaring `readOnlyHint: true`
/// stashes a payload in an unrecognised `user.overlay.*` xattr on a user-state directory it
/// also drops one allowlisted file into. Under prefix stripping the directory's
/// `metadata_changed` was `false`, so the structural-omission rule dropped it entirely and
/// the only reported path was the ephemeral lock file — `user_state` empty, i.e. `holds`,
/// with the payload in no partition at all.
#[test]
fn an_unknown_overlay_xattr_cannot_launder_a_user_state_directory_out_of_the_changeset() {
    let base = [dir("home"), dir("home/u")];
    let payload = "A".repeat(448);
    let stashed = with_xattr(copied_up(dir("home/u")), "user.overlay.stolen", &payload);
    let upper = [copied_up(dir("home")), stashed, file("home/u/scratch.lock", "")];

    let cs = run(&base, &upper);
    assert_eq!(paths(&cs.user_state), ["/home/u"], "the stashed write must be reported");
    assert_eq!(paths(&cs.ephemeral), ["/home/u/scratch.lock"]);
    assert!(cs.server_internal.is_empty());
    match &cs.user_state[0].kind {
        ChangeKind::Modified { node, metadata_changed: true, .. } => assert_eq!(
            node.xattrs,
            vec![(b"user.overlay.stolen".to_vec(), payload.into_bytes())]
        ),
        other => panic!("expected a reported metadata change on /home/u, got {other:?}"),
    }
}

#[test]
fn directories_copied_up_only_to_hold_a_changed_child_are_structural() {
    let base = [dir("home"), dir("home/u"), dir("home/u/.cache")];
    let upper = [
        copied_up(dir("home")),
        copied_up(dir("home/u")),
        copied_up(dir("home/u/.cache")),
        file("home/u/.cache/blob", "x"),
    ];
    let cs = run(&base, &upper);
    assert!(cs.user_state.is_empty(), "structural copy-ups leaked: {:?}", cs.user_state);
    assert_eq!(paths(&cs.server_internal), ["/home/u/.cache/blob"]);
    assert!(cs.ephemeral.is_empty());
}

#[test]
fn a_new_empty_directory_is_a_mutation() {
    let cs = run(&[dir("data")], &[copied_up(dir("data")), dir("data/new")]);
    assert_eq!(paths(&cs.user_state), ["/data/new"]);
    assert!(matches!(only(&cs.user_state), ChangeKind::Created(n) if n.file_type == FileType::Directory));
}

#[test]
fn a_new_directory_with_children_is_still_created() {
    // Not in the base, so not structural, even though it has a child.
    let cs = run(&[], &[dir("a"), file("a/x", "1")]);
    assert_eq!(paths(&cs.user_state), ["/a", "/a/x"]);
    assert!(cs.user_state.iter().all(|c| matches!(c.kind, ChangeKind::Created(_))));
}

#[test]
fn an_existing_directory_with_no_upper_children_is_reported() {
    // Copied up with nothing beneath it: a child was created then removed, or its
    // timestamps were set. No structural reason exists, so it is reported.
    let cs = run(&[dir("d")], &[copied_up(dir("d"))]);
    assert!(matches!(
        only(&cs.user_state),
        ChangeKind::Modified { content_changed: false, metadata_changed: false, .. }
    ));
}

#[test]
fn a_directory_with_changed_metadata_is_reported_even_with_children() {
    let mut d = copied_up(dir("d"));
    d.mode = 0o040_700;
    let cs = run(&[dir("d")], &[d, file("d/x", "1")]);
    assert_eq!(paths(&cs.user_state), ["/d", "/d/x"]);
    assert!(matches!(cs.user_state[0].kind, ChangeKind::Modified { metadata_changed: true, .. }));
}

#[test]
fn ownership_change_on_a_copied_up_directory_defeats_structural_omission() {
    // ADR-011: base and upper must be captured in the same ID view. If they are not, the
    // mismatch shows up as a metadata change — the conservative direction.
    let mut d = copied_up(dir("d"));
    d.uid = 0;
    let cs = run(&[dir("d")], &[d, file("d/x", "1")]);
    assert_eq!(paths(&cs.user_state), ["/d", "/d/x"]);
}

#[test]
fn whiteout_is_a_deletion() {
    let cs = run(&[dir("d"), file("d/f", "x")], &[copied_up(dir("d")), whiteout("d/f")]);
    assert_eq!(paths(&cs.user_state), ["/d/f"]);
    assert_eq!(only(&cs.user_state), &ChangeKind::Deleted { was: Some(FileType::Regular) });
}

#[test]
fn whiteout_of_a_path_absent_from_the_base_is_still_reported() {
    let cs = run(&[], &[whiteout("ghost")]);
    assert_eq!(only(&cs.user_state), &ChangeKind::Deleted { was: None });
}

#[test]
fn non_zero_char_device_is_not_a_whiteout() {
    let mut dev = whiteout("null");
    dev.dev_major = 1;
    dev.dev_minor = 3;
    let cs = run(&[], &[dev]);
    assert!(matches!(only(&cs.user_state), ChangeKind::Created(n) if n.file_type == FileType::CharDevice));
}

#[test]
fn opaque_directory_is_replaced_and_hides_the_base_beneath_it() {
    for name in ["trusted.overlay.opaque", "user.overlay.opaque"] {
        let base = [dir("d"), file("d/f", "same")];
        let upper = [with_xattr(dir("d"), name, "y"), file("d/f", "same")];
        let cs = run(&base, &upper);
        assert_eq!(paths(&cs.user_state), ["/d", "/d/f"], "{name}");
        assert!(matches!(cs.user_state[0].kind, ChangeKind::DirectoryReplaced(_)), "{name}");
        // Identical bytes to the base file, but the base is hidden: it is a new file.
        assert!(matches!(cs.user_state[1].kind, ChangeKind::Created(_)), "{name}");
        // The opaque marker itself is overlay-private and not part of the node.
        let ChangeKind::DirectoryReplaced(node) = &cs.user_state[0].kind else { unreachable!() };
        assert!(node.xattrs.is_empty());
    }
}

#[test]
fn opaque_hiding_applies_to_the_whole_subtree() {
    let base = [dir("d"), dir("d/e"), file("d/e/f", "same")];
    let upper = [with_xattr(dir("d"), "trusted.overlay.opaque", "y"), dir("d/e"), file("d/e/f", "same")];
    let cs = run(&base, &upper);
    assert_eq!(paths(&cs.user_state), ["/d", "/d/e", "/d/e/f"]);
    assert!(matches!(cs.user_state[1].kind, ChangeKind::Created(_)));
    assert!(matches!(cs.user_state[2].kind, ChangeKind::Created(_)));
}

#[test]
fn any_opaque_value_counts() {
    // Kernels >= 6.7 also write "x" (a directory holding xwhiteouts). Treating every value
    // as opaque reports a mutation where there might only be a structural copy-up — the
    // conservative direction.
    let upper = [with_xattr(dir("d"), "trusted.overlay.opaque", "x"), file("d/f", "1")];
    let cs = run(&[dir("d")], &upper);
    assert!(matches!(cs.user_state[0].kind, ChangeKind::DirectoryReplaced(_)));
}

#[test]
fn type_change_is_a_replacement() {
    let cs = run(&[file("a", "x")], &[dir("a"), file("a/b", "y")]);
    assert_eq!(paths(&cs.user_state), ["/a", "/a/b"]);
    assert!(matches!(cs.user_state[0].kind, ChangeKind::Replaced(ref n) if n.file_type == FileType::Directory));
}

#[test]
fn symlink_retarget_is_a_content_change() {
    let link = |t: &str| entry("l", Payload::Symlink(t.as_bytes().to_vec()));
    let cs = run(&[link("/a")], &[link("/b")]);
    assert!(matches!(only(&cs.user_state), ChangeKind::Modified { content_changed: true, .. }));
}

#[test]
fn subtree_detection_uses_tree_order_not_byte_order() {
    // Byte order is `d`, `d-x`, `d/y`: a naive "next entry" check would miss d's child.
    let base = [dir("d"), file("d-x", "1")];
    let upper = [copied_up(dir("d")), file("d-x", "2"), file("d/y", "3")];
    let cs = run(&base, &upper);
    assert_eq!(paths(&cs.user_state), ["/d-x", "/d/y"]);
}

// ---------------------------------------------------------------------------------------
// Noise and determinism (ADR-011 decision c)
// ---------------------------------------------------------------------------------------

#[test]
fn mtime_and_inode_never_affect_the_changeset() {
    let base = [dir("d"), file("d/f", "old")];
    let mk = |sec: i64, ino: u64| {
        let mut d = copied_up(dir("d"));
        d.mtime_sec = sec;
        d.inode = ino;
        let mut f = file("d/f", "new");
        f.mtime_sec = sec + 1;
        f.mtime_nsec = 7;
        f.inode = ino + 1;
        vec![d, f, file("d/g", "n")]
    };
    assert_eq!(run(&base, &mk(1, 100)), run(&base, &mk(2_000_000_000, 987_654)));
}

#[test]
fn output_is_sorted_by_raw_path_bytes_and_unique() {
    let upper = [file("b", "1"), file("a-z", "1"), dir("a"), file("a/c", "1"), file("A", "1")];
    let cs = run(&[], &upper);
    assert_eq!(paths(&cs.user_state), ["/A", "/a", "/a-z", "/a/c", "/b"]);
}

// ---------------------------------------------------------------------------------------
// Classification (ADR-008)
// ---------------------------------------------------------------------------------------

#[test]
fn adr_008_classification() {
    let upper = [
        file("tmp/scratch", "1"),
        file("srv/app.lock", "1"),
        file("root/.cache/pip/x", "1"),
        file("root/.cache/held.lock", "1"),
        file("srv/data.db", "1"),
        file("app/pkg/__pycache__/m.pyc", "1"),
    ];
    let base = [dir("tmp"), dir("srv"), dir("root"), dir("root/.cache"), dir("app"), dir("app/pkg"), dir("app/pkg/__pycache__")];
    let mut upper = upper.to_vec();
    for d in &base {
        upper.push(copied_up(d.clone()));
    }
    let cs = run(&base, &upper);
    assert_eq!(paths(&cs.user_state), ["/srv/data.db"]);
    assert_eq!(paths(&cs.server_internal), ["/app/pkg/__pycache__/m.pyc", "/root/.cache/pip/x"]);
    // Ephemeral is checked first, so a lock file inside a cache dir is ephemeral.
    assert_eq!(paths(&cs.ephemeral), ["/root/.cache/held.lock", "/srv/app.lock", "/tmp/scratch"]);
}

#[test]
fn newly_created_cache_directory_is_itself_server_internal() {
    let cs = run(&[dir("root")], &[copied_up(dir("root")), dir("root/.cache"), file("root/.cache/x", "1")]);
    assert!(cs.user_state.is_empty(), "{:?}", cs.user_state);
    assert_eq!(paths(&cs.server_internal), ["/root/.cache", "/root/.cache/x"]);
}

#[test]
fn forged_dot_dot_and_empty_components_cannot_launder_into_an_allowlist() {
    for forged in ["tmp/../home/secret", "tmp/./x", "tmp//x", "tmp/x/", "/tmp/x", ""] {
        let cs = run(&[], &[file(forged, "1")]);
        assert_eq!(cs.user_state.len(), 1, "{forged:?} escaped user_state: {cs:?}");
    }
}

/// ADR-011 decision 8 advertises the forged-component guard as a property of classification
/// generally, so the matcher must not contradict `classify` about it.
#[test]
fn classify_and_the_matcher_agree_on_forged_components() {
    let rules = CompiledRuleset::compile(&v1()).unwrap();
    let forged: [&[u8]; 5] =
        [b"tmp/../home/secret", b"tmp/./x", b"tmp//x", b"tmp/x/", b".cache/../secret"];
    for path in forged {
        assert_eq!(rules.classify(path), PathClass::UserState, "{path:?}");
        let mut abs = vec![b'/'];
        abs.extend_from_slice(path);
        let ruleset = v1();
        for pattern in ruleset.ephemeral.iter().chain(&ruleset.server_internal) {
            let g = Glob::parse(pattern).unwrap();
            assert!(!g.matches_path(&abs), "{pattern} matched forged {abs:?}");
        }
    }
}

#[test]
fn classify_is_exposed_for_reporting() {
    let rules = CompiledRuleset::compile(&v1()).unwrap();
    assert_eq!(rules.classify(b"tmp/x"), PathClass::Ephemeral);
    assert_eq!(rules.classify(b"home/u/.config/app.toml"), PathClass::ServerInternal);
    assert_eq!(rules.classify(b"home/u/doc.txt"), PathClass::UserState);
    assert_eq!(rules.classify(b"\xff/x.pid"), PathClass::Ephemeral);
}

/// `CompiledRuleset` reuse across evidence bundles is the documented replay path (P1-09),
/// so the compiled and uncompiled entry points must not drift apart.
#[test]
fn normalise_and_normalise_compiled_agree_across_bundles() {
    let rules = CompiledRuleset::compile(&v1()).unwrap();
    let bundles: Vec<(Vec<Entry>, Vec<Entry>)> = vec![
        (vec![], vec![]),
        (vec![dir("home")], vec![copied_up(dir("home")), file("home/notes.txt", "hi")]),
        (vec![dir("d"), file("d/f", "old")], vec![copied_up(dir("d")), file("d/f", "new")]),
        (vec![dir("d"), file("d/f", "x")], vec![copied_up(dir("d")), whiteout("d/f")]),
        (
            vec![dir("d"), file("d/f", "same")],
            vec![with_xattr(dir("d"), "trusted.overlay.opaque", "y"), file("d/f", "same")],
        ),
        (
            vec![dir("tmp"), dir("root"), dir("root/.cache")],
            vec![
                copied_up(dir("tmp")),
                file("tmp/scratch", "1"),
                copied_up(dir("root")),
                copied_up(dir("root/.cache")),
                file("root/.cache/x", "1"),
                with_xattr(file("srv.db", "1"), "user.overlay.stolen", "p"),
            ],
        ),
    ];
    for (i, (base, upper)) in bundles.iter().enumerate() {
        let ev = evidence(base, upper);
        let once = normalise(&ev, &v1()).expect("well-formed evidence");
        let reused = normalise_compiled(&ev, &rules).expect("well-formed evidence");
        assert_eq!(once, reused, "bundle {i}: the two entry points disagree");
        let n = once.user_state.len() + once.server_internal.len() + once.ephemeral.len();
        assert_eq!(n == 0, i == 0, "bundle {i} is vacuous");
    }
}

// ---------------------------------------------------------------------------------------
// Errors and hostile input (ADR-011 decision d)
// ---------------------------------------------------------------------------------------

#[test]
fn malformed_evidence_is_an_error_not_a_panic() {
    let good = evtree::encode(&[file("f", "x")]);
    let bad = RawEvidence { base_layer: good.clone(), upper_layer: b"nope".to_vec() };
    assert!(matches!(normalise(&bad, &v1()), Err(NormaliseError::MalformedUpperLayer(_))));
    let bad = RawEvidence { base_layer: Vec::new(), upper_layer: good };
    assert!(matches!(normalise(&bad, &v1()), Err(NormaliseError::MalformedBaseLayer(_))));
}

#[test]
fn invalid_pattern_is_an_error() {
    let mut rules = v1();
    rules.server_internal.push("relative/**".to_string());
    let err = normalise(&evidence(&[], &[]), &rules).unwrap_err();
    assert_eq!(
        err,
        NormaliseError::InvalidPattern { pattern: "relative/**".to_string(), error: GlobError::NotAnchored }
    );
    assert!(format!("{err}").contains("relative/**"));
}

#[test]
fn every_truncation_of_valid_evidence_is_rejected_cleanly() {
    let upper = evtree::encode(&[
        copied_up(dir("d")),
        with_xattr(file("d/f", "content"), "user.x", "1"),
        whiteout("d/g"),
    ]);
    let base = evtree::encode(&[dir("d"), file("d/g", "1")]);
    for cut in 0..upper.len() {
        let ev = RawEvidence { base_layer: base.clone(), upper_layer: upper[..cut].to_vec() };
        assert!(normalise(&ev, &v1()).is_err(), "truncation at {cut} accepted");
    }
    for cut in 0..base.len() {
        let ev = RawEvidence { base_layer: base[..cut].to_vec(), upper_layer: upper.clone() };
        assert!(normalise(&ev, &v1()).is_err(), "base truncation at {cut} accepted");
    }
}

/// xorshift64* — deterministic, dependency-free randomness for property tests.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

#[test]
fn random_bytes_and_bit_flips_never_panic() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let valid = evtree::encode(&[
        copied_up(dir("a")),
        with_xattr(dir("a/b"), "trusted.overlay.opaque", "y"),
        file("a/b/c", "xyz"),
        whiteout("a/w"),
    ]);
    for _ in 0..3000 {
        let mut bytes = valid.clone();
        for _ in 0..=rng.below(4) {
            let i = rng.below(bytes.len() as u64) as usize;
            bytes[i] ^= 1 << rng.below(8);
        }
        let ev = RawEvidence { base_layer: valid.clone(), upper_layer: bytes.clone() };
        let _ = normalise(&ev, &v1());
        let ev = RawEvidence { base_layer: bytes, upper_layer: valid.clone() };
        let _ = normalise(&ev, &v1());
    }
    for _ in 0..3000 {
        let len = rng.below(96) as usize;
        let mut bytes: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
        if rng.below(2) == 0 && bytes.len() >= 10 {
            // Give the random tail a real header so it reaches entry parsing.
            bytes[..8].copy_from_slice(&evtree::MAGIC);
            bytes[8..10].copy_from_slice(&evtree::FORMAT_VERSION.to_be_bytes());
        }
        let ev = RawEvidence { base_layer: valid.clone(), upper_layer: bytes };
        let _ = normalise(&ev, &v1());
    }
}

fn random_tree(rng: &mut Rng, max: u64) -> Vec<Entry> {
    // A tiny alphabet so paths collide between base and upper, and include '-' and '.'
    // (which sort around '/') and bytes that are not UTF-8.
    const SEGS: [&[u8]; 8] = [b"a", b"b", b"a-", b"tmp", b".cache", b"x.lock", b"\xff", b".."];
    let mut map: BTreeMap<Vec<u8>, Entry> = BTreeMap::new();
    for _ in 0..rng.below(max) {
        let depth = 1 + rng.below(4);
        let mut path = Vec::new();
        for d in 0..depth {
            if d > 0 {
                path.push(b'/');
            }
            path.extend_from_slice(SEGS[rng.below(SEGS.len() as u64) as usize]);
        }
        let payload = match rng.below(6) {
            0 | 1 => Payload::Directory,
            2 => Payload::Regular(vec![rng.below(3) as u8]),
            3 => Payload::Symlink(vec![b't', rng.below(3) as u8]),
            4 => Payload::CharDevice,
            _ => Payload::Fifo,
        };
        let mut e = entry("", payload);
        e.path = path.clone();
        e.mode = rng.below(2) as u32;
        e.dev_minor = rng.below(2) as u32;
        e.mtime_sec = rng.next() as i64;
        e.inode = rng.next();
        if rng.below(4) == 0 {
            let name = ["trusted.overlay.opaque", "user.overlay.origin", "user.x"][rng.below(3) as usize];
            e.xattrs.push(XAttr::new(name, "y"));
        }
        map.insert(path, e);
    }
    map.into_values().collect()
}

/// Property test over random base/upper pairs:
/// - total and deterministic;
/// - output partitions sorted, unique, disjoint, and no larger than the input;
/// - **every reported path is an actual upper-layer entry** — the containment the omission
///   check below does not give, which only proves nothing is dropped without cause;
/// - **the only omissions are structural**: an upper path missing from the output is a
///   directory that is a directory in the base and has a reported descendant.
#[test]
fn random_trees_satisfy_the_structural_omission_invariant() {
    let mut rng = Rng(0xD1B5_4A32_D192_ED03);
    let mut omitted = 0usize;
    for _ in 0..2000 {
        let base = random_tree(&mut rng, 12);
        // Seed the upper layer with noisy copies of some base entries (fresh inode/mtime,
        // overlay bookkeeping xattrs), as a real copy-up would, so structural omission
        // actually gets exercised; then overlay random entries on top.
        let mut merged: BTreeMap<Vec<u8>, Entry> = BTreeMap::new();
        for b in &base {
            if rng.below(2) == 0 {
                let mut e = b.clone();
                e.inode = rng.next();
                e.mtime_sec = rng.next() as i64;
                if !e.xattrs.iter().any(|x| x.name == b"trusted.overlay.impure") {
                    e = with_xattr(e, "trusted.overlay.impure", "y");
                }
                merged.insert(e.path.clone(), e);
            }
        }
        for e in random_tree(&mut rng, 12) {
            merged.insert(e.path.clone(), e);
        }
        let upper: Vec<Entry> = merged.into_values().collect();
        let ev = evidence(&base, &upper);
        let cs = normalise(&ev, &v1()).expect("encode output is canonical");
        assert_eq!(cs, normalise(&ev, &v1()).unwrap(), "non-deterministic");

        let mut all: Vec<&Change> =
            cs.user_state.iter().chain(&cs.server_internal).chain(&cs.ephemeral).collect();
        for part in [&cs.user_state, &cs.server_internal, &cs.ephemeral] {
            assert!(part.windows(2).all(|w| w[0].path < w[1].path), "unsorted or duplicate");
        }
        all.sort_by(|a, b| a.path.cmp(&b.path));
        assert!(all.windows(2).all(|w| w[0].path != w[1].path), "a path in two partitions");
        assert!(all.len() <= upper.len());

        for c in &all {
            let rel = c.path.strip_prefix(b"/").expect("reported paths are absolute");
            assert!(
                upper.iter().any(|e| e.path == rel),
                "reported path {:?} is not an upper-layer entry",
                c.path
            );
        }

        for e in &upper {
            let mut abs = vec![b'/'];
            abs.extend_from_slice(&e.path);
            if all.iter().any(|c| c.path == abs) {
                continue;
            }
            omitted += 1;
            assert!(matches!(e.payload, Payload::Directory), "non-directory omitted: {e:?}");
            let in_base = base.iter().find(|b| b.path == e.path).expect("omitted path not in base");
            assert!(matches!(in_base.payload, Payload::Directory));
            let mut prefix = abs.clone();
            prefix.push(b'/');
            assert!(
                all.iter().any(|c| c.path.starts_with(&prefix)),
                "omitted directory {abs:?} has no reported descendant"
            );
        }
    }
    // Guard against the generator never exercising the interesting branch.
    assert!(omitted > 50, "only {omitted} structural omissions generated");
}

/// Bounded on large and deeply nested hostile trees: a 1,000-level chain and 20,000
/// siblings complete quickly (a quadratic pass over path bytes would not).
#[test]
fn large_and_deep_trees_are_handled() {
    let mut deep = Vec::new();
    let mut path = String::new();
    for i in 0..1000 {
        if i > 0 {
            path.push('/');
        }
        path.push('d');
        deep.push(dir(&path));
    }
    let base = deep.clone();
    let mut upper: Vec<Entry> = deep.into_iter().map(copied_up).collect();
    upper.push(file(&format!("{path}/leaf"), "x"));
    let cs = run(&base, &upper);
    assert_eq!(cs.user_state.len(), 1, "every ancestor is structural");

    let wide: Vec<Entry> = (0..20_000).map(|i| file(&format!("w/{i:05}"), "x")).collect();
    let cs = run(&[], &wide);
    assert_eq!(cs.user_state.len(), 20_000);
}

// ---------------------------------------------------------------------------------------
// Classification for the verdict engine (ADR-012 decision 5)
// ---------------------------------------------------------------------------------------

/// Every [`NormaliseError`] must classify, and all three causes must stay distinguishable,
/// because the split is by **whose fault the failure is** and the reason code reaches
/// publication:
///
/// - a malformed **upper** layer is a finding about the tool's own writes;
/// - a malformed **base** layer is a harness fault — `world::base_layer` (P1-02) builds the
///   base and the overlay mounts it read-only, so the tool has no way to corrupt it;
/// - an uncompilable ruleset is a harness fault too.
///
/// Collapsing the first two (which is what this impl did until P1-07's review) published a
/// harness bug as a finding against a server, and — read the other way — handed any server
/// deniability for a real malformed capture.
#[test]
fn every_normalise_error_classifies_into_a_derivation_failure() {
    use datamodel::DerivationFailure;

    let good = evtree::encode(&[file("f", "x")]);
    let bad_upper = normalise(
        &RawEvidence { base_layer: good.clone(), upper_layer: b"nope".to_vec() },
        &v1(),
    )
    .unwrap_err();
    let bad_base =
        normalise(&RawEvidence { base_layer: b"nope".to_vec(), upper_layer: good }, &v1())
            .unwrap_err();
    let mut rules = v1();
    rules.ephemeral.push("relative/**".to_string());
    let bad_rules = normalise(&evidence(&[], &[]), &rules).unwrap_err();

    assert_eq!(DerivationFailure::from(&bad_upper), DerivationFailure::MalformedEvidence);
    assert_eq!(DerivationFailure::from(&bad_base), DerivationFailure::MalformedBaseLayer);
    assert_eq!(DerivationFailure::from(&bad_rules), DerivationFailure::InvalidRuleset);
    // All three mutually distinct: no pair may share one published reason code.
    let classified = [
        DerivationFailure::from(&bad_upper),
        DerivationFailure::from(&bad_base),
        DerivationFailure::from(&bad_rules),
    ];
    for (i, a) in classified.iter().enumerate() {
        for b in &classified[i + 1..] {
            assert_ne!(a, b, "two causes collapsed into one reason code");
        }
    }
}

// ---------------------------------------------------------------------------------------
// The un-amended ADR-008 deletion gap, pinned where it actually happens
// ---------------------------------------------------------------------------------------

/// ADR-008 classifies on the path alone, so **deleting** a base-layer file whose *name*
/// matches an allowlist lands in that allowlist and never in `user_state` — which makes
/// `readOnlyHint` read as `holds` for a tool that destroyed real state. ADR-011's open
/// questions propose amending the taxonomy to classify on `(path, ChangeKind)`: let an
/// allowlist suppress `Created` and `Modified` only, and send `Deleted`, `Replaced` and
/// `DirectoryReplaced` always to `user_state`.
///
/// **This test is expected to change when that amendment lands**, and that is its entire
/// job. `docs/tasks.md` previously credited
/// `verdict::tests::hostile_content_outside_user_state_still_reads_as_holds_and_is_reported`
/// with pinning this, but that test hand-places a `Deleted` change into the `ephemeral`
/// partition and never calls `normalise` — so it asserts how the *verdict engine* treats a
/// partition it was handed, not how classification fills it. A sweep of the suite found no
/// test anywhere putting a deletion of an allowlisted path through `normalise`, which means
/// the amendment could have landed with every test still green. This is that test: it runs
/// the real classifier over a real whiteout of `/srv/app.lock` (present in the base, matched
/// by v1's `**/*.lock`) and asserts today's behaviour, so the amendment has to come here and
/// say what it changed.
#[test]
fn deleting_an_allowlisted_path_lands_in_ephemeral_today_not_user_state() {
    let base = [dir("srv"), file("srv/app.lock", "pid 1")];
    let upper = [copied_up(dir("srv")), whiteout("srv/app.lock")];
    let cs = run(&base, &upper);

    assert_eq!(
        paths(&cs.ephemeral),
        ["/srv/app.lock"],
        "v1's `**/*.lock` matches on path alone, so the deletion is classified ephemeral"
    );
    assert_eq!(
        only(&cs.ephemeral),
        &ChangeKind::Deleted { was: Some(FileType::Regular) },
        "and it really is a deletion, not a creation — which is the whole objection"
    );
    assert!(
        cs.user_state.is_empty(),
        "today a destructive change to an allowlisted name decides nothing: {:?}",
        cs.user_state
    );

    // The same path *created* rather than deleted is the case ADR-008's membership bar was
    // actually argued with ("a tool touching a lock file at startup"), and the amendment
    // would leave it exactly where it is. Asserted alongside so the amendment's diff shows
    // one line changing and not two.
    let created = run(&[dir("srv")], &[copied_up(dir("srv")), file("srv/app.lock", "pid 1")]);
    assert_eq!(paths(&created.ephemeral), ["/srv/app.lock"]);
    assert!(matches!(only(&created.ephemeral), ChangeKind::Created(_)));
}
