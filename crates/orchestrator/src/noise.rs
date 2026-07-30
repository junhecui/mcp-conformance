//! P2-08: `N = D1 Δ D1'`, computed per tool, per run, from two genuinely independent
//! `Arm 1'`-shaped executions (architecture.md §4.2) — never assumed empty (ADR-003).
//!
//! The pure Δ computation itself lives in `normalise::noise_floor`, over already-decoded
//! evidence; this module is the thin, effectful wrapper that actually *produces* the two
//! independent captures a real noise-floor measurement needs, by reusing
//! [`crate::arms::run_arm_1_prime`] twice against two distinct scratch directories.

use std::path::Path;

use crate::arms::{run_arm_1_prime, ArmError, ArmProgram};

/// Run `program` twice, independently (two fresh sandboxes, same base layer and arguments —
/// architecture.md §4.1's `D1`/`D1'` pair), and return `N`, the symmetric difference between
/// the two captures.
///
/// `scratch_root` is split into two subdirectories (`d1`, `d1-prime`) internally so the two
/// runs never share an overlay — reusing the same overlay across both would make this
/// `Arm 2R`'s restart, not two independent `Arm 1'`-shaped runs.
///
/// # Errors
///
/// Propagates [`crate::arms::run_arm_1_prime`]'s error for whichever of the two runs fails.
pub fn measure_noise_floor(
    program: &ArmProgram,
    scratch_root: &Path,
    blob_store: &store::BlobStore,
) -> Result<Vec<normalise::NoiseFloorEntry>, ArmError> {
    let d1 = run_arm_1_prime(program, &scratch_root.join("d1"), blob_store)?;
    let d1_prime = run_arm_1_prime(program, &scratch_root.join("d1-prime"), blob_store)?;
    Ok(normalise::noise_floor(&d1.evidence, &d1_prime.evidence))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arms::tests_support::{build_stub_base_layer, stub_program};
    use serde_json::json;
    use std::path::PathBuf;
    use std::time::Duration;

    /// The stub server from P2-07's own tests (`effect.txt`, written identically on every
    /// independent run's first call) is fully deterministic across independent processes —
    /// `D1` and `D1'` must therefore agree completely, and `N` must come back empty. This is
    /// the "clean" case: a tool with no ambient per-run variation has nothing to normalise.
    #[test]
    fn a_deterministic_tool_has_an_empty_noise_floor() {
        let _slot = crate::arms::tests_support::take_sandbox_slot();
        let lower = tempfile::tempdir().expect("lower tempdir");
        build_stub_base_layer(lower.path());
        let program = stub_program(lower.path());

        let scratch = tempfile::tempdir().expect("scratch");
        let store_dir = tempfile::tempdir().expect("store dir");
        let blob_store = store::BlobStore::open(store_dir.path()).expect("open blob store");

        let n = measure_noise_floor(&program, scratch.path(), &blob_store).expect("measure N");
        assert_eq!(n, vec![], "two independent runs of a deterministic tool must agree completely");
    }

    /// ADR-003, made concrete rather than assumed: a tool with genuine per-run variation
    /// (this stub writes a file named after the wall-clock nanosecond it ran at — the same
    /// realistic shape ADR-008's `**/*.pid`/`**/*.lock` ephemeral patterns already exist to
    /// catch) must show up as a non-empty `N`. If this test ever started passing with an
    /// empty noise floor, `noise_floor` itself would be broken, not the tool being
    /// unexpectedly deterministic — two independent `date +%s%N` calls colliding is not a
    /// real possibility across two separate process launches.
    #[test]
    fn a_tool_with_genuine_per_run_variation_has_a_non_empty_noise_floor() {
        let _slot = crate::arms::tests_support::take_sandbox_slot();
        const NOISY_SCRIPT: &[u8] = b"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s' \"$line\" | sed -n 's/.*\"id\":\\([0-9]*\\).*/\\1/p')
  case \"$line\" in
    *'\"method\":\"initialize\"'*)
      printf '{\"jsonrpc\":\"2.0\",\"id\":%s,\"result\":{\"protocolVersion\":\"2025-11-25\",\"capabilities\":{},\"serverInfo\":{\"name\":\"noisy-stub\",\"version\":\"0.0.0\"}}}\\n' \"$id\"
      ;;
    *'\"method\":\"notifications/initialized\"'*)
      ;;
    *'\"method\":\"tools/call\"'*)
      touch \"scratch-$(date +%s%N).tmp\"
      printf '{\"jsonrpc\":\"2.0\",\"id\":%s,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"ok\"}]}}\\n' \"$id\"
      ;;
  esac
done
";
        let lower = tempfile::tempdir().expect("lower tempdir");
        let entries = vec![sandbox::EntrySpec {
            path: PathBuf::from("stub_server.sh"),
            kind: sandbox::EntryKind::File(NOISY_SCRIPT.to_vec()),
            mode: 0o755,
        }];
        sandbox::build(lower.path(), &entries).expect("build noisy stub base layer");

        let program = ArmProgram {
            base_layer: lower.path().to_path_buf(),
            program: PathBuf::from("/bin/sh"),
            args: vec!["stub_server.sh".to_string()],
            tool_name: "noop".to_string(),
            arguments: json!({}),
            timeout: Duration::from_secs(10),
        };

        let scratch = tempfile::tempdir().expect("scratch");
        let store_dir = tempfile::tempdir().expect("store dir");
        let blob_store = store::BlobStore::open(store_dir.path()).expect("open blob store");

        let n = measure_noise_floor(&program, scratch.path(), &blob_store).expect("measure N");
        assert!(
            !n.is_empty(),
            "two independent runs each creating a uniquely-timestamped file must disagree"
        );
        assert!(
            n.iter().all(|entry| matches!(
                entry,
                normalise::NoiseFloorEntry::OnlyInFirst(p) | normalise::NoiseFloorEntry::OnlyInSecond(p)
                    if p.starts_with(b"scratch-") && p.ends_with(b".tmp")
            )),
            "every noise entry must be one of the uniquely-named scratch files, not something \
             unexpected: {n:?}"
        );
    }
}
