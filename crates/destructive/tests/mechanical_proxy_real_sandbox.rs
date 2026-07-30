//! Q-01's own exit criterion, over a real sandboxed run rather than only hand-built
//! `RawEvidence` structs: a real process, inside a real overlay, deletes one pre-existing
//! file, overwrites another, and creates a brand-new one — and `destructive::partition`
//! correctly buckets all three from the base layer's and the upper layer's own real
//! captures.
//!
//! Linux-only (same as `sandbox`/`observe` themselves): both need real Linux-specific
//! syscalls with no portable equivalent.
#![cfg(target_os = "linux")]

use std::path::Path;
use std::time::Duration;

use sandbox::{EntryKind as BaseLayerEntryKind, EntrySpec, OverlaySpec, SandboxSpec};

#[test]
fn a_real_delete_overwrite_and_create_partition_correctly() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let lower = scratch.path().join("lower");

    sandbox::build(
        &lower,
        &[
            EntrySpec { path: "home".into(), kind: BaseLayerEntryKind::Directory, mode: 0o755 },
            EntrySpec {
                path: "home/existing.txt".into(),
                kind: BaseLayerEntryKind::File(b"will be deleted".to_vec()),
                mode: 0o644,
            },
            EntrySpec {
                path: "home/replaced.txt".into(),
                kind: BaseLayerEntryKind::File(b"original content".to_vec()),
                mode: 0o644,
            },
        ],
    )
    .expect("build base layer");

    // The base layer's own real path set — captured the same way the upper layer will be,
    // before it is ever mounted into an overlay.
    let base_capture_bytes = observe::evtree::capture(&lower).expect("capture base layer");
    let base_raw_evidence = observe::evtree::decode(&base_capture_bytes).expect("decode base layer");
    let base_layer_paths: std::collections::BTreeSet<Vec<u8>> =
        base_raw_evidence.entries.iter().map(|e| e.path.clone()).collect();
    assert!(base_layer_paths.contains(b"home/existing.txt".as_slice()));

    let spec = SandboxSpec {
        overlay: OverlaySpec {
            lower,
            upper: scratch.path().join("upper"),
            work: scratch.path().join("work"),
            mountpoint: scratch.path().join("merged"),
        },
        program: "/usr/local/bin/python3".into(),
        args: vec![
            "-c".to_string(),
            "\
import os
os.remove('home/existing.txt')
with open('home/replaced.txt', 'w') as f:
    f.write('new content')
with open('home/new.txt', 'w') as f:
    f.write('brand new')
"
            .to_string(),
        ],
        timeout: Duration::from_secs(10),
        network_isolated: false,
    };

    let (handle, stdin, stdout) = sandbox::spawn(&spec).expect("spawn");
    drop(stdin); // this script reads nothing from stdin
    drop(stdout);
    let outcome = handle.wait().expect("wait");
    assert!(!outcome.timed_out);
    assert!(outcome.exit_status.is_some_and(|s| s.success()), "the script must exit cleanly");

    let upper_bytes = observe::evtree::capture(&outcome.upper).expect("capture upper layer");
    let raw_evidence = observe::evtree::decode(&upper_bytes).expect("decode upper layer");

    let ruleset_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../rulesets/v1.json");
    let ruleset = orchestrator::load_ruleset(&ruleset_path).expect("load ruleset");

    let partition = destructive::partition(&raw_evidence, &base_layer_paths, &ruleset);

    fn find<'a>(
        paths: &'a [destructive::ProxyClassifiedPath],
        path: &[u8],
    ) -> Option<&'a destructive::ProxyClassifiedPath> {
        paths.iter().find(|p| p.path == path)
    }

    let deleted = find(&partition.deletions_or_overwrites, b"home/existing.txt")
        .expect("the deleted file must be classified");
    assert_eq!(deleted.change, destructive::ChangeKind::Deletion);

    let overwritten = find(&partition.deletions_or_overwrites, b"home/replaced.txt")
        .expect("the overwritten file must be classified");
    assert_eq!(overwritten.change, destructive::ChangeKind::Overwrite);

    let created = find(&partition.pure_additions, b"home/new.txt")
        .expect("the new file must be classified as a pure addition");
    assert_eq!(created.change, destructive::ChangeKind::Addition);
}
