//! A tiny, `no_std` glob matcher covering exactly the syntax ADR-008's ruleset v1 patterns
//! use: `**` (matches zero or more whole path segments) and a single `*` within one segment
//! (matches any run of non-`/` bytes). Deliberately not a general glob or regex engine — the
//! codebase's own `normalise` doc comment already flags `regex` as the fallback if real path
//! matching outgrows this, and ruleset v1's patterns (ADR-008) don't: every pattern has at
//! most one `*`, always at a segment boundary.
//!
//! Patterns are matched against raw path bytes as captured (`/`-separated, no leading `/`,
//! per ADR-009). A pattern written with a leading `/` (e.g. `/tmp/**`, meaning "anchored at
//! the tree root") has that slash stripped before matching, since captured paths never
//! carry one either.

use alloc::vec::Vec;

/// Whether `path` matches `pattern`.
#[must_use]
pub fn glob_match(pattern: &[u8], path: &[u8]) -> bool {
    let pattern = pattern.strip_prefix(b"/").unwrap_or(pattern);
    let pattern_segments = split_segments(pattern);
    let path_segments = split_segments(path);
    match_segments(&pattern_segments, &path_segments)
}

fn split_segments(bytes: &[u8]) -> Vec<&[u8]> {
    bytes.split(|&b| b == b'/').filter(|segment| !segment.is_empty()).collect()
}

fn match_segments(pattern: &[&[u8]], path: &[&[u8]]) -> bool {
    if pattern.is_empty() {
        return path.is_empty();
    }
    if pattern[0] == b"**" {
        // `**` matches zero segments (try the rest of the pattern here) or one-plus (try
        // the same pattern again one segment further into the path).
        if match_segments(&pattern[1..], path) {
            return true;
        }
        return !path.is_empty() && match_segments(pattern, &path[1..]);
    }
    if path.is_empty() {
        return false;
    }
    segment_match(pattern[0], path[0]) && match_segments(&pattern[1..], &path[1..])
}

/// Match one path segment against one pattern segment, which may contain a single `*`.
/// Anything after a second `*`, if a pattern ever had one, is treated as a literal —
/// unneeded by any ruleset v1 pattern, and documented here rather than silently mishandled.
fn segment_match(pattern_segment: &[u8], candidate: &[u8]) -> bool {
    match pattern_segment.iter().position(|&b| b == b'*') {
        None => pattern_segment == candidate,
        Some(star_index) => {
            let prefix = &pattern_segment[..star_index];
            let suffix = &pattern_segment[star_index + 1..];
            candidate.len() >= prefix.len() + suffix.len()
                && candidate.starts_with(prefix)
                && candidate.ends_with(suffix)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matches(pattern: &str, path: &str) -> bool {
        glob_match(pattern.as_bytes(), path.as_bytes())
    }

    #[test]
    fn leading_slash_anchor_matches_from_the_tree_root() {
        assert!(matches("/tmp/**", "tmp/foo"));
        assert!(matches("/tmp/**", "tmp/foo/bar"));
        assert!(!matches("/tmp/**", "var/tmp/foo"), "must not match a `tmp` that isn't at the root");
    }

    #[test]
    fn double_star_matches_zero_segments_too() {
        // /tmp/** must match "tmp" itself (the directory entry, zero segments after it),
        // not only paths strictly under it.
        assert!(matches("/tmp/**", "tmp"));
    }

    #[test]
    fn double_star_prefix_matches_at_any_depth() {
        assert!(matches("**/*.lock", "a.lock"));
        assert!(matches("**/*.lock", "a/b/c.lock"));
        assert!(!matches("**/*.lock", "a/b/c.lock.bak"));
    }

    #[test]
    fn wildcard_within_a_segment_does_not_cross_a_path_separator() {
        assert!(!matches("**/*.lock", "a.lock/b"), "the match must land on the final segment");
    }

    #[test]
    fn multi_segment_literal_chain_after_double_star() {
        assert!(matches("**/.local/state/**", "home/user/.local/state/foo"));
        assert!(!matches("**/.local/state/**", "home/user/.local/config/foo"));
    }

    #[test]
    fn every_adr_008_ephemeral_pattern_matches_its_own_worked_example() {
        assert!(matches("/tmp/**", "tmp/scratch.txt"));
        assert!(matches("/var/tmp/**", "var/tmp/scratch.txt"));
        assert!(matches("/run/**", "run/foo.sock"));
        assert!(matches("**/*.lock", "server/data.lock"));
        assert!(matches("**/*.pid", "server/server.pid"));
        assert!(matches("**/*.sock", "server/ipc.sock"));
    }

    #[test]
    fn every_adr_008_server_internal_pattern_matches_its_own_worked_example() {
        assert!(matches("**/.cache/**", "home/user/.cache/tool/entry"));
        assert!(matches("**/.config/**", "home/user/.config/tool/settings"));
        assert!(matches("**/.local/state/**", "home/user/.local/state/tool.db"));
        assert!(matches("**/__pycache__/**", "app/__pycache__/module.pyc"));
        assert!(matches("**/node_modules/.cache/**", "app/node_modules/.cache/babel/x"));
    }

    #[test]
    fn an_unrelated_path_matches_neither_list() {
        assert!(!matches("/tmp/**", "home/user/document.txt"));
        assert!(!matches("**/*.lock", "home/user/document.txt"));
        assert!(!matches("**/.cache/**", "home/user/document.txt"));
    }
}
