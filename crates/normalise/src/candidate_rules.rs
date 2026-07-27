//! P2-10: turn a corpus's worth of observed noise-floor paths into candidate ADR-008 glob
//! rules — "every element of an observed `N` is a candidate normalisation rule."
//!
//! # Scope: simple, disclosed generalisation, not a learned model
//!
//! Three proposals per observed path, in order of how far they generalise beyond the one
//! literal observation:
//!
//! 1. **The literal path itself**, always — the exit criterion's own words ("every element
//!    of an observed `N` is a candidate rule") are satisfied verbatim, with no
//!    generalisation risk at all.
//! 2. **An extension-based rule** (`**/*.ext`) when the path ends in a recognisable
//!    ephemeral-looking extension (`.lock`, `.pid`, `.sock`, `.tmp`, `.log`) — the same shape
//!    ADR-008's own v1 ruleset already uses for exactly these three extensions.
//! 3. **A parent-directory rule** (`**/<dirname>/**`) when the path has a containing
//!    directory — generalises "this one file was noise" into "everything under this
//!    directory is candidate noise," the same shape as v1's `**/.cache/**` and friends.
//!
//! All three are *candidates*: this module proposes, a human or a later audit step (this
//! same task's "delete a rule that never appears in any observed `N`" bullet) decides. It
//! deliberately does not try to be cleverer than that — inferring, say, "this looks like a
//! randomly-generated suffix" would be a real, useful improvement but a much larger, far
//! more speculative piece of logic than this task's exit criterion asks for.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// Extensions this module treats as ephemeral-shaped by name alone — the same three ADR-008's
/// own v1 ruleset already encodes (`**/*.lock`, `**/*.pid`, `**/*.sock`), plus two more
/// conventional temp/log suffixes.
const RECOGNISED_EXTENSIONS: &[&str] = &["lock", "pid", "sock", "tmp", "log"];

/// Propose candidate glob rules for every path in `observed_noise_paths` (typically the
/// union of every tool's own `D1 Δ D1'` across a measured corpus). Output is deduplicated
/// and sorted, but otherwise unfiltered — every proposal this function can derive from the
/// input, with no judgement about which candidates are actually worth adopting.
#[must_use]
pub fn candidate_rules_from_noise(observed_noise_paths: &[Vec<u8>]) -> Vec<String> {
    let mut candidates: Vec<String> = Vec::new();

    for raw_path in observed_noise_paths {
        let Ok(path) = core::str::from_utf8(raw_path) else { continue };
        if path.is_empty() {
            continue;
        }

        candidates.push(path.to_string());

        if let Some(extension) = path.rsplit('.').next() {
            if extension != path && RECOGNISED_EXTENSIONS.contains(&extension) {
                candidates.push(alloc::format!("**/*.{extension}"));
            }
        }

        if let Some((parent, _filename)) = path.rsplit_once('/') {
            if let Some(dirname) = parent.rsplit('/').next() {
                if !dirname.is_empty() {
                    candidates.push(alloc::format!("**/{dirname}/**"));
                }
            }
        }
    }

    candidates.sort();
    candidates.dedup();
    candidates
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(s: &str) -> Vec<u8> {
        s.as_bytes().to_vec()
    }

    #[test]
    fn every_observed_path_is_itself_a_candidate() {
        let candidates = candidate_rules_from_noise(&[path("effect.txt"), path("tmp/scratch.lock")]);
        assert!(candidates.contains(&"effect.txt".to_string()));
        assert!(candidates.contains(&"tmp/scratch.lock".to_string()));
    }

    #[test]
    fn a_recognised_extension_proposes_a_glob_rule() {
        let candidates = candidate_rules_from_noise(&[path("var/run/tool.pid")]);
        assert!(candidates.contains(&"**/*.pid".to_string()));
    }

    #[test]
    fn an_unrecognised_extension_does_not_propose_an_extension_rule() {
        let candidates = candidate_rules_from_noise(&[path("home/user/output.txt.bak")]);
        assert!(!candidates.iter().any(|c| c == "**/*.bak"));
    }

    #[test]
    fn a_path_with_a_parent_directory_proposes_a_directory_rule() {
        let candidates = candidate_rules_from_noise(&[path("home/user/.cache/tool/x")]);
        assert!(candidates.contains(&"**/tool/**".to_string()));
    }

    #[test]
    fn a_top_level_path_with_no_parent_directory_proposes_no_directory_rule() {
        let candidates = candidate_rules_from_noise(&[path("effect.txt")]);
        assert!(!candidates.iter().any(|c| c.starts_with("**/") && c.ends_with("/**")));
    }

    #[test]
    fn output_is_deduplicated_and_sorted() {
        let candidates =
            candidate_rules_from_noise(&[path("a/x.lock"), path("b/x.lock"), path("a/x.lock")]);
        let mut expected = candidates.clone();
        expected.sort();
        expected.dedup();
        assert_eq!(candidates, expected);
        assert_eq!(candidates.iter().filter(|c| *c == "**/*.lock").count(), 1);
    }

    #[test]
    fn an_empty_corpus_proposes_nothing() {
        assert_eq!(candidate_rules_from_noise(&[]), Vec::<String>::new());
    }
}
