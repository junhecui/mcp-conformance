//! Load a normalisation ruleset file from `rulesets/` into a [`datamodel::Ruleset`].
//!
//! Lives here, outside the pure crates, because reading and parsing a file is I/O-adjacent
//! work ADR-005 keeps out of `normalise` — which takes an already-parsed ruleset
//! ([ADR-011]).
//!
//! # Tamper-evident identity
//!
//! The loader — never the file — computes [`Ruleset::source_digest`], the SHA-256 of the
//! exact bytes. A published version's bytes are pinned in [`PUBLISHED`]: [`load`] refuses a
//! file claiming a published label whose digest does not match, so an edited `v1.yaml`
//! cannot keep calling itself `v1`. Unpublished work (e.g. a P2-10 v2 candidate) goes
//! through [`load_draft`], which refuses any published label. Either way the identity a
//! verdict records ([`Ruleset::identity`]) carries the digest, so two different rule texts
//! never share one.
//!
//! # File format
//!
//! A strict subset of YAML, parsed by hand rather than with a YAML library (one small,
//! fixed shape does not justify the dependency, and anything outside the subset is
//! rejected rather than half-understood):
//!
//! ```yaml
//! # comment lines
//! version: "v1"
//! ephemeral:
//!   - "/tmp/**"
//! server_internal:
//!   - "**/.cache/**"
//! ```
//!
//! Exactly these three top-level keys, each once; every scalar double-quoted with no
//! backslash or inner quote; list items indented by exactly two spaces; `#` comments only
//! on their own line; no carriage returns or tabs. Every pattern must also compile under
//! `normalise::glob`, so a bad pattern fails at load time rather than at derivation time.
//!
//! [ADR-011]: ../../../docs/adr/011-normaliser-semantics.md

use std::fmt;
use std::path::Path;

use datamodel::{Digest, Ruleset};
use normalise::glob::{Glob, GlobError};
use sha2::{Digest as _, Sha256};

/// Published ruleset versions and the SHA-256 (lowercase hex) of their exact file bytes.
///
/// Append-only. A published ruleset is never edited — verdicts recorded under it must stay
/// reproducible from `(evidence, ruleset)` (ADR-005) — so changing a rule means adding a new
/// version here, not changing a digest.
pub const PUBLISHED: &[(&str, &str)] =
    &[("v1", "48e55850021a462d5710d72e06b5bebe256b1a8107d5a70b0202a1f0b63c1128")];

/// Why a ruleset file was rejected.
#[derive(Debug)]
pub enum RulesetError {
    /// Reading the file failed.
    Io(std::io::Error),
    /// The file is not inside the accepted YAML subset.
    Syntax {
        /// 1-based line number, or `0` when the fault is the file as a whole — it is not
        /// UTF-8, or a top-level key is missing — and no single line can be blamed.
        line: usize,
        /// What was wrong.
        message: String,
    },
    /// A pattern does not compile under `normalise::glob`.
    InvalidPattern {
        /// The pattern, verbatim.
        pattern: String,
        /// Why.
        error: GlobError,
    },
    /// The file claims a published version label but its bytes do not hash to the digest
    /// pinned for that label in [`PUBLISHED`].
    DigestMismatch {
        /// The claimed label.
        version: String,
        /// The pinned digest.
        expected: String,
        /// The digest of the bytes actually loaded.
        actual: String,
    },
    /// [`load`] was given a label that is not in [`PUBLISHED`].
    Unpublished(String),
    /// [`load_draft`] was given a label that *is* published.
    DraftClaimsPublishedVersion(String),
    /// [`load_file`]'s file name is not `<version>.yaml`.
    FileNameMismatch {
        /// The label inside the file.
        version: String,
        /// The file name.
        file_name: String,
    },
}

impl fmt::Display for RulesetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "reading ruleset: {e}"),
            // `line: 0` is the whole-file fault above; "ruleset line 0:" would be a lie.
            Self::Syntax { line: 0, message } => write!(f, "ruleset: {message}"),
            Self::Syntax { line, message } => write!(f, "ruleset line {line}: {message}"),
            Self::InvalidPattern { pattern, error } => {
                write!(f, "ruleset pattern {pattern:?}: {error}")
            }
            Self::DigestMismatch { version, expected, actual } => write!(
                f,
                "ruleset {version:?} is published with sha256 {expected}, but these bytes hash \
                 to {actual}; published rulesets are immutable — add a new version instead"
            ),
            Self::Unpublished(v) => write!(f, "ruleset version {v:?} is not published"),
            Self::DraftClaimsPublishedVersion(v) => {
                write!(f, "draft ruleset uses the published version label {v:?}")
            }
            Self::FileNameMismatch { version, file_name } => {
                write!(f, "ruleset {version:?} must be stored as {version}.yaml, not {file_name}")
            }
        }
    }
}

impl std::error::Error for RulesetError {}

/// Load a **published** ruleset from its exact bytes, verifying its pinned digest.
///
/// # Errors
///
/// Any [`RulesetError`] other than `Io`/`FileNameMismatch`.
pub fn load(bytes: &[u8]) -> Result<Ruleset, RulesetError> {
    let ruleset = parse(bytes)?;
    match PUBLISHED.iter().find(|(v, _)| *v == ruleset.version) {
        None => Err(RulesetError::Unpublished(ruleset.version)),
        Some((_, expected)) if *expected != ruleset.source_digest.to_string() => {
            Err(RulesetError::DigestMismatch {
                version: ruleset.version.clone(),
                expected: (*expected).to_string(),
                actual: ruleset.source_digest.to_string(),
            })
        }
        Some(_) => Ok(ruleset),
    }
}

/// Load an **unpublished** ruleset (e.g. a candidate v2). Its identity is still bound to
/// its digest; it simply may not borrow a published label.
///
/// # Errors
///
/// Any parse error, or [`RulesetError::DraftClaimsPublishedVersion`].
pub fn load_draft(bytes: &[u8]) -> Result<Ruleset, RulesetError> {
    let ruleset = parse(bytes)?;
    if PUBLISHED.iter().any(|(v, _)| *v == ruleset.version) {
        return Err(RulesetError::DraftClaimsPublishedVersion(ruleset.version));
    }
    Ok(ruleset)
}

/// Read and [`load`] a published ruleset file, also requiring it to be named
/// `<version>.yaml`.
///
/// # Errors
///
/// Any [`RulesetError`].
pub fn load_file(path: &Path) -> Result<Ruleset, RulesetError> {
    let bytes = std::fs::read(path).map_err(RulesetError::Io)?;
    let ruleset = load(&bytes)?;
    let file_name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if file_name != format!("{}.yaml", ruleset.version) {
        return Err(RulesetError::FileNameMismatch { version: ruleset.version, file_name });
    }
    Ok(ruleset)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    None,
    Ephemeral,
    ServerInternal,
}

fn parse(bytes: &[u8]) -> Result<Ruleset, RulesetError> {
    let err = |line: usize, message: &str| RulesetError::Syntax { line, message: message.to_string() };
    let text = std::str::from_utf8(bytes).map_err(|_| err(0, "not valid UTF-8"))?;

    let mut version: Option<String> = None;
    let mut ephemeral: Option<Vec<String>> = None;
    let mut server_internal: Option<Vec<String>> = None;
    let mut section = Section::None;

    for (idx, line) in text.split('\n').enumerate() {
        let n = idx + 1;
        if line.contains('\r') || line.contains('\t') {
            return Err(err(n, "carriage returns and tabs are not allowed"));
        }
        let trimmed = line.trim_start_matches(' ');
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some(item) = line.strip_prefix("  - ") {
            let pattern = quoted(item).ok_or_else(|| err(n, "list item must be one double-quoted string"))?;
            Glob::parse(&pattern)
                .map_err(|error| RulesetError::InvalidPattern { pattern: pattern.clone(), error })?;
            let list = match section {
                Section::Ephemeral => ephemeral.as_mut(),
                Section::ServerInternal => server_internal.as_mut(),
                Section::None => None,
            };
            list.ok_or_else(|| err(n, "list item outside a list"))?.push(pattern);
            continue;
        }
        if line.starts_with(' ') {
            return Err(err(n, "unexpected indentation"));
        }
        let (key, rest) = line.split_once(':').ok_or_else(|| err(n, "expected `key:`"))?;
        match key {
            "version" => {
                let v = quoted(rest.trim_start_matches(' '))
                    .ok_or_else(|| err(n, "version must be one double-quoted string"))?;
                if !v.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'_') {
                    return Err(err(n, "version label may contain only [A-Za-z0-9._-]"));
                }
                if version.replace(v).is_some() {
                    return Err(err(n, "duplicate key `version`"));
                }
                section = Section::None;
            }
            "ephemeral" | "server_internal" => {
                if !rest.is_empty() {
                    return Err(err(n, "list key must be followed by a newline"));
                }
                let (slot, s) = if key == "ephemeral" {
                    (&mut ephemeral, Section::Ephemeral)
                } else {
                    (&mut server_internal, Section::ServerInternal)
                };
                if slot.replace(Vec::new()).is_some() {
                    return Err(err(n, "duplicate list key"));
                }
                section = s;
            }
            _ => return Err(err(n, "unknown key")),
        }
    }

    let missing = |k: &str| RulesetError::Syntax { line: 0, message: format!("missing key `{k}`") };
    Ok(Ruleset {
        version: version.ok_or_else(|| missing("version"))?,
        source_digest: Digest::from_bytes(Sha256::digest(bytes).into()),
        ephemeral: ephemeral.ok_or_else(|| missing("ephemeral"))?,
        server_internal: server_internal.ok_or_else(|| missing("server_internal"))?,
    })
}

/// `"..."` with no inner `"` or `\`, non-empty, and nothing after the closing quote.
fn quoted(s: &str) -> Option<String> {
    let inner = s.strip_prefix('"')?.strip_suffix('"')?;
    if inner.is_empty() || inner.contains('"') || inner.contains('\\') {
        return None;
    }
    Some(inner.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use datamodel::{ChangeKind, PathClass, RawEvidence};

    const V1: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../rulesets/v1.yaml"));

    #[test]
    fn v1_loads_with_exactly_adr_008s_allowlists() {
        let r = load(V1).unwrap();
        assert_eq!(r.version, "v1");
        assert_eq!(
            r.ephemeral,
            ["/tmp/**", "/var/tmp/**", "/run/**", "**/*.lock", "**/*.pid", "**/*.sock"]
        );
        assert_eq!(
            r.server_internal,
            [
                "**/.cache/**",
                "**/.config/**",
                "**/.local/state/**",
                "**/__pycache__/**",
                "**/node_modules/.cache/**"
            ]
        );
        assert_eq!(r.identity(), format!("v1+sha256:{}", PUBLISHED[0].1));
    }

    #[test]
    fn v1_file_on_disk_loads_by_path() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../rulesets/v1.yaml");
        assert_eq!(load_file(&path).unwrap(), load(V1).unwrap());
    }

    #[test]
    fn any_edit_to_a_published_ruleset_is_refused() {
        // Even a whitespace-only edit inside a comment changes the identity.
        let mut edited = V1.to_vec();
        edited.extend_from_slice(b"\n# harmless-looking comment\n");
        assert!(matches!(load(&edited), Err(RulesetError::DigestMismatch { .. })));

        // And a rule edit, the case that actually matters.
        let text = std::str::from_utf8(V1).unwrap().replace("  - \"/run/**\"\n", "  - \"/home/**\"\n");
        let err = load(text.as_bytes()).unwrap_err();
        assert!(matches!(err, RulesetError::DigestMismatch { ref version, .. } if version == "v1"));
        assert!(err.to_string().contains("immutable"));
    }

    #[test]
    fn crlf_checkout_is_refused_loudly() {
        let crlf = std::str::from_utf8(V1).unwrap().replace('\n', "\r\n");
        assert!(matches!(load(crlf.as_bytes()), Err(RulesetError::Syntax { .. })));
    }

    #[test]
    fn drafts_carry_their_own_digest_and_cannot_borrow_a_published_label() {
        let draft = b"version: \"v2-candidate\"\nephemeral:\n  - \"/tmp/**\"\nserver_internal:\n";
        let r = load_draft(draft).unwrap();
        assert!(r.server_internal.is_empty());
        assert_eq!(r.source_digest, Digest::from_bytes(Sha256::digest(draft).into()));
        assert!(matches!(load(draft), Err(RulesetError::Unpublished(_))));
        assert!(matches!(load_draft(V1), Err(RulesetError::DraftClaimsPublishedVersion(_))));
    }

    #[test]
    fn rejects_everything_outside_the_subset() {
        let cases: &[&[u8]] = &[
            b"",
            b"version: \"x\"\nephemeral:\n",                                        // missing key
            b"version: x\nephemeral:\nserver_internal:\n",                           // unquoted
            b"version: \"x\"\nversion: \"y\"\nephemeral:\nserver_internal:\n",      // duplicate
            b"version: \"x\"\nephemeral:\nephemeral:\nserver_internal:\n",           // duplicate
            b"version: \"x\"\nextra:\nephemeral:\nserver_internal:\n",               // unknown key
            b"version: \"x\"\n  - \"/a\"\nephemeral:\nserver_internal:\n",           // stray item
            b"version: \"x\"\nephemeral:\n    - \"/a\"\nserver_internal:\n",         // indent
            b"version: \"x\"\nephemeral:\n  - /a\nserver_internal:\n",               // unquoted
            b"version: \"x\"\nephemeral:\n  - \"/a\" # c\nserver_internal:\n",       // trailing
            b"version: \"x\"\nephemeral:\n  - \"/a\\\\b\"\nserver_internal:\n",      // escape
            b"version: \"x\"\nephemeral: [\"/a\"]\nserver_internal:\n",              // flow style
            b"version: \"x\"\nephemeral:\n\t- \"/a\"\nserver_internal:\n",           // tab
            b"version: \"v1+x\"\nephemeral:\nserver_internal:\n",                // label charset
            b"\xff",                                                                 // not UTF-8
        ];
        for case in cases {
            assert!(
                matches!(load_draft(case), Err(RulesetError::Syntax { .. })),
                "accepted {:?}",
                String::from_utf8_lossy(case)
            );
        }
    }

    #[test]
    fn rejects_patterns_the_matcher_cannot_compile() {
        let bad = b"version: \"x\"\nephemeral:\n  - \"*.lock\"\nserver_internal:\n";
        assert!(matches!(
            load_draft(bad),
            Err(RulesetError::InvalidPattern { error: GlobError::NotAnchored, .. })
        ));
    }

    /// The loaded v1 drives `normalise` end to end — the replay shape P1-09 will use.
    #[test]
    fn loaded_v1_drives_normalise() {
        use evtree::{Entry, Payload, encode};
        let e = |p: &str, payload| Entry {
            path: p.as_bytes().to_vec(),
            mode: 0o644,
            uid: 0,
            gid: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            inode: 0,
            dev_major: 0,
            dev_minor: 0,
            xattrs: Vec::new(),
            payload,
        };
        let base = encode(&[e("tmp", Payload::Directory), e("home", Payload::Directory)]);
        let upper = encode(&[
            e("tmp", Payload::Directory),
            e("tmp/x", Payload::Regular(b"1".to_vec())),
            e("home", Payload::Directory),
            e("home/doc", Payload::Regular(b"2".to_vec())),
        ]);
        let ruleset = load(V1).unwrap();
        let cs = normalise::normalise(&RawEvidence { base_layer: base, upper_layer: upper }, &ruleset).unwrap();
        assert_eq!(cs.ruleset_identity, ruleset.identity());
        assert_eq!(cs.class(PathClass::Ephemeral).len(), 1);
        assert_eq!(cs.class(PathClass::UserState)[0].path, b"/home/doc");
        assert!(matches!(cs.class(PathClass::UserState)[0].kind, ChangeKind::Created(_)));
    }
}
