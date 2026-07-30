//! P2-09: drive architecture.md §4.2's `idempotentHint` multi-arm protocol against real
//! sandboxed runs — `D1`, `D1'` (for the noise floor), `D2`, and `D2R` — and feed the
//! resulting difference sets to [`verdict::idempotent_hint`].
//!
//! # Why this needs its own, content-aware delta — not P2-08's `normalise::noise_floor`
//!
//! `normalise::noise_floor` (P2-08) is deliberately scoped to *path* symmetric difference,
//! because its purpose is seeding candidate normalisation-rule *globs*, which are
//! path-shaped. That scoping makes it the wrong tool for this task: the exact
//! caching-confound scenario architecture.md §4.2 describes — a tool whose second,
//! same-process call has no additional effect, but whose effect reappears after a restart —
//! shows up as the *same path* with *different content* (`effect.txt` growing from one line
//! to two), which a path-only comparison cannot see at all. `datamodel::EvidenceEntry`'s own
//! doc comment anticipated exactly this gap ("content-level idempotency diffing — P2-09").
//!
//! Rather than widening ADR-009's `evtree1` wire format (and risking every already-stored
//! evidence blob's replay-compatibility, P1-09's own exit criterion) to carry content
//! digests through `datamodel::EvidenceEntry`, [`content_delta`] compares two arms' real,
//! on-disk upper directories directly with plain `std::fs` — reading file bytes needs I/O
//! `datamodel`/`normalise`/`verdict` are permitted none of (ADR-005), so this lives here,
//! the one crate allowed to depend on everything and touch a filesystem freely.

use std::collections::BTreeSet;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// Compare two directory trees and return every relative path where they disagree — present
/// in only one, or present in both but a different type (file/dir/symlink), different file
/// content, or a different symlink target. A directory present in both is never itself
/// reported as differing; only the leaves under it can be.
///
/// Sorted by raw path bytes on return, matching this codebase's usual determinism
/// discipline, even though the caller (`idempotent_hint`'s own subset check) does not
/// currently depend on that order.
///
/// # Errors
///
/// Returns an [`io::Error`] if walking either tree, or reading a file's or symlink's
/// contents for comparison, fails.
pub fn content_delta(a: &Path, b: &Path) -> io::Result<Vec<Vec<u8>>> {
    let mut a_paths = Vec::new();
    collect_relative_paths(a, Path::new(""), &mut a_paths)?;
    let mut b_paths = Vec::new();
    collect_relative_paths(b, Path::new(""), &mut b_paths)?;

    let a_set: BTreeSet<&PathBuf> = a_paths.iter().collect();
    let b_set: BTreeSet<&PathBuf> = b_paths.iter().collect();

    let mut differing = Vec::new();
    for relative in a_set.union(&b_set) {
        let in_a = a_set.contains(relative);
        let in_b = b_set.contains(relative);
        if in_a != in_b {
            differing.push(path_bytes(relative));
            continue;
        }

        let full_a = a.join(relative);
        let full_b = b.join(relative);
        let meta_a = std::fs::symlink_metadata(&full_a)?;
        let meta_b = std::fs::symlink_metadata(&full_b)?;
        let type_a = meta_a.file_type();
        let type_b = meta_b.file_type();

        if type_a.is_dir() && type_b.is_dir() {
            continue; // a directory carries no leaf-level signal of its own
        }
        if type_a.is_dir() != type_b.is_dir() || type_a.is_symlink() != type_b.is_symlink() {
            differing.push(path_bytes(relative));
            continue;
        }
        if type_a.is_symlink() {
            if std::fs::read_link(&full_a)? != std::fs::read_link(&full_b)? {
                differing.push(path_bytes(relative));
            }
            continue;
        }
        if std::fs::read(&full_a)? != std::fs::read(&full_b)? {
            differing.push(path_bytes(relative));
        }
    }
    differing.sort();
    Ok(differing)
}

fn collect_relative_paths(root: &Path, relative: &Path, out: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in std::fs::read_dir(root.join(relative))? {
        let entry = entry?;
        let child = relative.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            out.push(child.clone());
            collect_relative_paths(root, &child, out)?;
        } else {
            out.push(child);
        }
    }
    Ok(())
}

fn path_bytes(p: &Path) -> Vec<u8> {
    p.as_os_str().as_bytes().to_vec()
}

#[cfg(test)]
mod tests {
    use crate::arms::tests_support::take_sandbox_slot;
    use crate::arms::{run_arm_1_prime, run_arm_2, run_arm_2r, ArmProgram};
    use serde_json::json;
    use std::path::PathBuf;
    use std::time::Duration;

    use super::content_delta;

    /// Run the full `D1`/`D1'`/`D2`/`D2R` set for `program` and decide `idempotentHint`
    /// exactly as `orchestrator` would in a real pipeline: `N` and both deltas via
    /// `content_delta`, fed straight into `verdict::idempotent_hint`.
    fn decide(program: &ArmProgram, scratch_root: &std::path::Path) -> verdict::Assessment {
        let store_dir = tempfile::tempdir().expect("store dir");
        let blob_store = store::BlobStore::open(store_dir.path()).expect("open blob store");

        let d1 = run_arm_1_prime(program, &scratch_root.join("d1"), &blob_store).expect("d1");
        let d1_prime =
            run_arm_1_prime(program, &scratch_root.join("d1-prime"), &blob_store).expect("d1'");
        let d2 = run_arm_2(program, &scratch_root.join("d2"), &blob_store).expect("d2");
        let d2r = run_arm_2r(program, &scratch_root.join("d2r"), &blob_store).expect("d2r");

        let noise_floor = content_delta(&d1.upper, &d1_prime.upper).expect("N");
        let d2_delta_d1 = content_delta(&d2.upper, &d1.upper).expect("D2 delta D1");
        let d2r_delta_d1 = content_delta(&d2r.upper, &d1.upper).expect("D2R delta D1");

        verdict::idempotent_hint(&d2_delta_d1, &d2r_delta_d1, &noise_floor)
    }

    fn build_program(root: &std::path::Path, script: &[u8]) -> ArmProgram {
        let entries = vec![sandbox::EntrySpec {
            path: PathBuf::from("stub_server.sh"),
            kind: sandbox::EntryKind::File(script.to_vec()),
            mode: 0o755,
        }];
        sandbox::build(root, &entries).expect("build stub base layer");
        ArmProgram {
            base_layer: root.to_path_buf(),
            program: PathBuf::from("/bin/sh"),
            args: vec!["stub_server.sh".to_string()],
            tool_name: "noop".to_string(),
            arguments: json!({}),
            timeout: Duration::from_secs(10),
        }
    }

    /// A stub that never touches the filesystem on `tools/call` at all — genuinely
    /// idempotent by construction, since there is no effect to disagree about in the first
    /// place.
    const NOOP_SCRIPT: &[u8] = b"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s' \"$line\" | sed -n 's/.*\"id\":\\([0-9]*\\).*/\\1/p')
  case \"$line\" in
    *'\"method\":\"initialize\"'*)
      printf '{\"jsonrpc\":\"2.0\",\"id\":%s,\"result\":{\"protocolVersion\":\"2025-11-25\",\"capabilities\":{},\"serverInfo\":{\"name\":\"noop-stub\",\"version\":\"0.0.0\"}}}\\n' \"$id\"
      ;;
    *'\"method\":\"notifications/initialized\"'*)
      ;;
    *'\"method\":\"tools/call\"'*)
      printf '{\"jsonrpc\":\"2.0\",\"id\":%s,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"ok\"}]}}\\n' \"$id\"
      ;;
  esac
done
";

    /// A stub whose *every* call unconditionally appends to `effect.txt` — no in-process
    /// suppression at all, unlike the P2-07 caching-confound stub. Genuinely non-idempotent:
    /// a second call, same process or not, always leaves an additional, real effect.
    const ALWAYS_APPENDS_SCRIPT: &[u8] = b"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s' \"$line\" | sed -n 's/.*\"id\":\\([0-9]*\\).*/\\1/p')
  case \"$line\" in
    *'\"method\":\"initialize\"'*)
      printf '{\"jsonrpc\":\"2.0\",\"id\":%s,\"result\":{\"protocolVersion\":\"2025-11-25\",\"capabilities\":{},\"serverInfo\":{\"name\":\"noncached-stub\",\"version\":\"0.0.0\"}}}\\n' \"$id\"
      ;;
    *'\"method\":\"notifications/initialized\"'*)
      ;;
    *'\"method\":\"tools/call\"'*)
      printf 'called\\n' >> effect.txt
      printf '{\"jsonrpc\":\"2.0\",\"id\":%s,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"ok\"}]}}\\n' \"$id\"
      ;;
  esac
done
";

    /// A tool that never touches the filesystem is genuinely idempotent: `D1 == D2 == D2R`
    /// (all empty), so both deltas are empty and trivially within any noise floor.
    #[test]
    fn a_tool_with_no_filesystem_effect_holds() {
        let _slot = take_sandbox_slot();
        let lower = tempfile::tempdir().expect("lower");
        let program = build_program(lower.path(), NOOP_SCRIPT);
        let scratch = tempfile::tempdir().expect("scratch");

        let assessment = decide(&program, scratch.path());
        assert_eq!(assessment, verdict::Assessment::holds(datamodel::Oracle::KernelChangeset));
    }

    /// A tool whose *every* call unconditionally appends to `effect.txt` (no in-process
    /// suppression at all) is genuinely non-idempotent: `D2`'s two calls leave two lines,
    /// `D1`'s one call leaves one — a real difference outside `N` (which is empty, since two
    /// independent single calls each leave exactly one, identical line). Caught by `C1`
    /// alone; no restart needed to prove it.
    #[test]
    fn a_tool_that_always_appends_unconditionally_is_violated() {
        let _slot = take_sandbox_slot();
        let lower = tempfile::tempdir().expect("lower");
        let program = build_program(lower.path(), ALWAYS_APPENDS_SCRIPT);
        let scratch = tempfile::tempdir().expect("scratch");

        let assessment = decide(&program, scratch.path());
        assert_eq!(assessment, verdict::Assessment::violated(datamodel::Oracle::KernelChangeset));
    }

    /// The P2-07 stub server (in-process-only call counter suppresses a second same-process
    /// write, but a restart's fresh counter lets it reappear) is the caching-confound shape
    /// architecture.md §4.2 describes exactly: `D2 Δ D1` is empty (the suppressed second call
    /// leaves `D2` identical to `D1`), but `D2R Δ D1` is not (the restart's extra write makes
    /// `effect.txt` two lines instead of one) — `unverifiable`, not `holds` or `violated`.
    #[test]
    fn the_p2_07_caching_confound_stub_is_unverifiable_with_the_caching_reason() {
        let _slot = take_sandbox_slot();
        let lower = tempfile::tempdir().expect("lower");
        crate::arms::tests_support::build_stub_base_layer(lower.path());
        let program = crate::arms::tests_support::stub_program(lower.path());
        let scratch = tempfile::tempdir().expect("scratch");

        let assessment = decide(&program, scratch.path());
        assert_eq!(
            assessment,
            verdict::Assessment::unverifiable(
                datamodel::Oracle::KernelChangeset,
                datamodel::ReasonCode::CachingSuppressedInProcess
            )
        );
    }
}
