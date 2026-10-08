//! A small, `no_std`, bounded glob matcher for ruleset path patterns (ADR-008, ADR-011).
//!
//! # Syntax (deliberately minimal)
//!
//! A pattern is a `/`-separated sequence of components and must be **anchored**: it starts
//! either with `/` (matched from the sandbox root) or with a leading `**/`.
//!
//! - `**` as a *whole* component matches zero or more path components, in every position —
//!   so `/tmp/**` matches `/tmp` itself as well as everything inside it. This differs from
//!   gitignore/`globset`, where a trailing `/**` needs at least one component, and it is a
//!   deliberate choice (ADR-011): ADR-008's patterns name *directories* as tool-owned
//!   (`.cache`, `__pycache__`), and creating that directory is no more diagnostic of a
//!   read-only violation than writing inside it. Under one-or-more semantics,
//!   `**/__pycache__/**` would classify the `__pycache__` directory CPython creates on first
//!   import as `user_state`, making the rule nearly useless.
//! - `*` within a component matches any run of bytes (including none) other than `/`.
//! - `?` within a component matches exactly one byte other than `/`.
//! - Every other byte is literal.
//!
//! On the path side, a component that is empty, `.`, `..`, or that itself contains a `/` is
//! not something a real capture produces and never matches (ADR-011 decision 8). That guard
//! lives in [`Glob::matches_components`], so it holds at every entry point rather than only
//! at `CompiledRuleset::classify`'s.
//!
//! Rejected at parse time rather than silently treated as literals, so a ruleset written
//! for a richer glob dialect fails loudly: `**` inside a larger component (`a**b`),
//! character classes / braces / escapes (`[`, `]`, `{`, `}`, `\`), empty components
//! (`//`, trailing `/`), `.` and `..` components, and unanchored patterns.
//!
//! # Complexity
//!
//! Matching never backtracks exponentially. Both levels (components against `**`, bytes
//! against `*`) use the classic single-resume-point wildcard algorithm, which visits each
//! `(text position, pattern position)` pair at most once per star segment. A path of `L`
//! bytes against a pattern of `P` bytes therefore costs `O(L · P)` byte comparisons in the
//! worst case — linear in the (hostile) path for a fixed, operator-trusted ruleset — with
//! no recursion and no allocation during matching.

use alloc::vec::Vec;

/// Why a pattern was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlobError {
    /// The pattern is empty.
    Empty,
    /// The pattern starts with neither `/` nor `**/`.
    NotAnchored,
    /// An empty component (`//`, or a trailing `/`).
    EmptyComponent,
    /// A `.` or `..` component.
    DotComponent,
    /// `**` appears inside a larger component.
    MisplacedDoubleStar,
    /// A byte with glob meaning in other dialects that this matcher does not support.
    UnsupportedSyntax(u8),
}

impl core::fmt::Display for GlobError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Empty => f.write_str("empty pattern"),
            Self::NotAnchored => f.write_str("pattern must start with `/` or `**/`"),
            Self::EmptyComponent => f.write_str("empty path component"),
            Self::DotComponent => f.write_str("`.` or `..` component"),
            Self::MisplacedDoubleStar => f.write_str("`**` must be a whole path component"),
            Self::UnsupportedSyntax(b) => write!(f, "unsupported glob syntax byte {:?}", *b as char),
        }
    }
}

impl core::error::Error for GlobError {}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ByteTok {
    Star,
    One,
    Lit(u8),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CompTok {
    /// Zero or more whole components.
    DoubleStar,
    /// Exactly one component matching this byte pattern.
    Comp(Vec<ByteTok>),
}

/// A compiled pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Glob {
    toks: Vec<CompTok>,
}

impl Glob {
    /// Compile a pattern. See the module doc for the accepted syntax.
    ///
    /// # Errors
    ///
    /// [`GlobError`] for anything outside that syntax.
    pub fn parse(pattern: &str) -> Result<Self, GlobError> {
        let bytes = pattern.as_bytes();
        if bytes.is_empty() {
            return Err(GlobError::Empty);
        }
        let body = if let Some(rest) = bytes.strip_prefix(b"/") {
            rest
        } else if bytes.starts_with(b"**/") {
            bytes
        } else {
            return Err(GlobError::NotAnchored);
        };

        let mut toks: Vec<CompTok> = Vec::new();
        for comp in body.split(|&b| b == b'/') {
            if comp.is_empty() {
                return Err(GlobError::EmptyComponent);
            }
            if comp == b"." || comp == b".." {
                return Err(GlobError::DotComponent);
            }
            if comp == b"**" {
                // `**/**` is the same as `**`; collapsing keeps the token list minimal.
                if toks.last() != Some(&CompTok::DoubleStar) {
                    toks.push(CompTok::DoubleStar);
                }
                continue;
            }
            if comp.windows(2).any(|w| w == b"**") {
                return Err(GlobError::MisplacedDoubleStar);
            }
            let mut bt = Vec::with_capacity(comp.len());
            for &b in comp {
                match b {
                    b'*' => bt.push(ByteTok::Star),
                    b'?' => bt.push(ByteTok::One),
                    b'[' | b']' | b'{' | b'}' | b'\\' => return Err(GlobError::UnsupportedSyntax(b)),
                    _ => bt.push(ByteTok::Lit(b)),
                }
            }
            toks.push(CompTok::Comp(bt));
        }
        Ok(Self { toks })
    }

    /// Does this pattern match a path given as its components?
    ///
    /// A component that is empty, `.`, `..`, or that itself contains a `/` never matches: no
    /// real capture produces one, and forged evidence must not be able to launder
    /// `/tmp/../home/secret` into an allowlist (ADR-011 decision 8). The guard is here, in
    /// the matcher, rather than only at `CompiledRuleset::classify`'s entry point, so that
    /// two public entry points on the crate whose output is the verdict cannot disagree about
    /// a security property.
    #[must_use]
    pub fn matches_components(&self, components: &[&[u8]]) -> bool {
        if components
            .iter()
            .any(|c| c.is_empty() || *c == b"." || *c == b".." || c.contains(&b'/'))
        {
            return false;
        }
        wildcard(
            components,
            &self.toks,
            |t| matches!(t, CompTok::DoubleStar),
            |t, c| match t {
                CompTok::Comp(bt) => match_component(bt, c),
                CompTok::DoubleStar => false,
            },
        )
    }

    /// Does this pattern match an absolute path (`/a/b/c`)? A path that is not absolute
    /// never matches.
    #[must_use]
    pub fn matches_path(&self, path: &[u8]) -> bool {
        let Some(rest) = path.strip_prefix(b"/") else {
            return false;
        };
        let comps: Vec<&[u8]> = rest.split(|&b| b == b'/').collect();
        self.matches_components(&comps)
    }
}

fn match_component(pat: &[ByteTok], comp: &[u8]) -> bool {
    wildcard(
        comp,
        pat,
        |t| matches!(t, ByteTok::Star),
        |t, &b| match t {
            ByteTok::One => b != b'/',
            ByteTok::Lit(l) => *l == b,
            ByteTok::Star => false,
        },
    )
}

/// Classic wildcard matching with a single resume point.
///
/// Correct whenever every non-star token consumes exactly one text element (true at both
/// levels here). On a mismatch it rewinds to just after the most recent star and lets that
/// star absorb one more element; earlier stars never need revisiting, because the latest
/// star can absorb anything an earlier one could. Each resume advances the star's absorbed
/// span by one, so the loop runs at most `O(|text| · |pat|)` iterations.
fn wildcard<T, P>(
    text: &[T],
    pat: &[P],
    is_star: impl Fn(&P) -> bool,
    matches_one: impl Fn(&P, &T) -> bool,
) -> bool {
    let (mut t, mut p) = (0usize, 0usize);
    // (pattern index just after the star, text index the star currently absorbs up to)
    let mut resume: Option<(usize, usize)> = None;
    while t < text.len() {
        if p < pat.len() && is_star(&pat[p]) {
            p += 1;
            resume = Some((p, t));
        } else if p < pat.len() && matches_one(&pat[p], &text[t]) {
            p += 1;
            t += 1;
        } else if let Some((rp, rt)) = resume {
            p = rp;
            t = rt + 1;
            resume = Some((rp, rt + 1));
        } else {
            return false;
        }
    }
    pat[p..].iter().all(is_star)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(pat: &str, path: &str) -> bool {
        Glob::parse(pat).unwrap().matches_path(path.as_bytes())
    }

    #[test]
    fn adr_008_ephemeral_patterns() {
        assert!(m("/tmp/**", "/tmp/x"));
        assert!(m("/tmp/**", "/tmp/a/b/c"));
        assert!(m("/tmp/**", "/tmp"), "trailing ** is zero-or-more (ADR-011)");
        assert!(!m("/tmp/**", "/tmpfoo/x"));
        assert!(!m("/tmp/**", "/home/tmp/x"));
        assert!(m("/var/tmp/**", "/var/tmp/q"));
        assert!(m("/run/**", "/run/user/1000/bus"));
        assert!(m("**/*.lock", "/x.lock"), "leading ** is zero-or-more");
        assert!(m("**/*.lock", "/a/b/package.lock"));
        assert!(m("**/*.lock", "/a/.lock"));
        assert!(!m("**/*.lock", "/a/x.lock/inner"));
        assert!(!m("**/*.lock", "/a/x.locked"));
        assert!(m("**/*.pid", "/var/run.pid"));
        assert!(m("**/*.sock", "/s/a.sock"));
    }

    #[test]
    fn adr_008_server_internal_patterns() {
        assert!(m("**/.cache/**", "/root/.cache/pip/x"));
        assert!(m("**/.cache/**", "/.cache/x"));
        assert!(m("**/.cache/**", "/root/.cache"), "the named directory itself matches");
        assert!(m("**/__pycache__/**", "/app/pkg/__pycache__"));
        assert!(!m("**/.cache/**", "/root/.cachex/y"));
        assert!(m("**/.local/state/**", "/home/u/.local/state/app/log"));
        assert!(!m("**/.local/state/**", "/home/u/.local/share/x"));
        assert!(m("**/__pycache__/**", "/app/pkg/__pycache__/m.cpython-312.pyc"));
        assert!(m("**/node_modules/.cache/**", "/app/node_modules/.cache/babel/x"));
        assert!(!m("**/node_modules/.cache/**", "/app/node_modules/left-pad/index.js"));
    }

    #[test]
    fn single_byte_wildcard() {
        assert!(m("/a?c", "/abc"));
        assert!(!m("/a?c", "/ac"));
        assert!(!m("/a?c", "/a/c"));
    }

    #[test]
    fn middle_double_star_is_zero_or_more() {
        assert!(m("/a/**/z", "/a/z"));
        assert!(m("/a/**/z", "/a/b/c/z"));
        assert!(!m("/a/**/z", "/a/b/c/y"));
        assert!(m("/a/**/**/z", "/a/z"));
    }

    #[test]
    fn relative_and_non_absolute_paths_never_match() {
        assert!(!m("**/*.lock", "x.lock"));
        assert!(!m("/tmp/**", ""));
    }

    /// ADR-011 decision 8's forged-component guard is a property of the matcher, not only of
    /// `CompiledRuleset::classify`. Before this, `matches_path("/tmp/**",
    /// "/tmp/../home/secret")` was `true` while `classify` said `UserState`.
    #[test]
    fn forged_path_components_never_match() {
        assert!(!m("/tmp/**", "/tmp/../home/secret"));
        assert!(!m("/tmp/**", "/tmp/./x"));
        assert!(!m("/tmp/**", "/tmp//x"));
        assert!(!m("/tmp/**", "/tmp/x/"));
        assert!(!m("/tmp/**", "/"));
        assert!(!m("**/*.lock", "/a/../x.lock"));
        assert!(!m("**/.cache/**", "/home/../.cache/x"));
        // Still matches the honest paths, so the guard is not a blanket refusal.
        assert!(m("/tmp/**", "/tmp/a/b"));
        assert!(m("**/*.lock", "/a/b.lock"));

        // A component containing `/` is not one either: `matches_components` is given
        // already-split components, and `/a/*` must not match the single component `b/c/d`.
        let g = Glob::parse("/a/*").unwrap();
        assert!(g.matches_components(&[b"a".as_slice(), b"b".as_slice()]));
        assert!(!g.matches_components(&[b"a".as_slice(), b"b/c/d".as_slice()]));
    }

    #[test]
    fn rejects_unsupported_syntax() {
        assert_eq!(Glob::parse(""), Err(GlobError::Empty));
        assert_eq!(Glob::parse("*.lock"), Err(GlobError::NotAnchored));
        assert_eq!(Glob::parse("tmp/**"), Err(GlobError::NotAnchored));
        assert_eq!(Glob::parse("/a//b"), Err(GlobError::EmptyComponent));
        assert_eq!(Glob::parse("/a/"), Err(GlobError::EmptyComponent));
        assert_eq!(Glob::parse("/a/../b"), Err(GlobError::DotComponent));
        assert_eq!(Glob::parse("/a/./b"), Err(GlobError::DotComponent));
        assert_eq!(Glob::parse("/a**b"), Err(GlobError::MisplacedDoubleStar));
        assert_eq!(Glob::parse("/a/***"), Err(GlobError::MisplacedDoubleStar));
        assert_eq!(Glob::parse("/[ab]"), Err(GlobError::UnsupportedSyntax(b'[')));
        assert_eq!(Glob::parse("/{a,b}"), Err(GlobError::UnsupportedSyntax(b'{')));
        assert_eq!(Glob::parse("/a\\*"), Err(GlobError::UnsupportedSyntax(b'\\')));
    }

    #[test]
    fn non_utf8_paths_match_bytewise() {
        let g = Glob::parse("**/*.lock").unwrap();
        assert!(g.matches_path(b"/\xff\xfe/x.lock"));
        assert!(!g.matches_path(b"/\xff\xfe/x.lockx"));
    }

    /// The pathological case for naive backtracking: many stars against a long
    /// near-miss. Exponential matchers take astronomically long here; this one is
    /// `O(L · P)`. Uses an iteration bound via path size rather than wall-clock time.
    #[test]
    fn no_exponential_blowup_on_hostile_inputs() {
        let g = Glob::parse("/*a*a*a*a*a*a*a*a*a*a*b").unwrap();
        let mut path = alloc::vec![b'/'];
        path.extend(core::iter::repeat_n(b'a', 20_000));
        assert!(!g.matches_path(&path));

        let g = Glob::parse("**/x/**/x/**/x/**/x/**/y").unwrap();
        let mut path = Vec::new();
        for _ in 0..20_000 {
            path.extend_from_slice(b"/x");
        }
        assert!(!g.matches_path(&path));
    }
}
