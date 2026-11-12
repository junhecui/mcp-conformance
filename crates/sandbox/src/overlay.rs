//! Overlayfs mount-option construction and the path validation it depends on. Pure: no
//! syscalls, no filesystem access, so every rule here is unit-testable on any Linux host.
//!
//! # The pinned options (ADR-010) and the `userxattr` branch (ADR-016)
//!
//! Every overlay this crate mounts carries exactly [`PINNED_OPTIONS`]:
//! `redirect_dir=off,metacopy=off,index=off`. `metacopy=off` is the security-load-bearing one
//! (forged `REDIRECT`/`METACOPY` xattrs in an untrusted layer). ADR-010 left a fourth option,
//! `userxattr`, conditional on whether the mount is privileged; ADR-016 decides it is
//! privileged — the supervisor mounts as real root inside a fresh mount namespace, before
//! any credential drop — so `userxattr` is **not** set and overlay bookkeeping lands in
//! `trusted.overlay.*`, which an unprivileged tool cannot remove (ADR-011 decision 5's live
//! `EPERM` result). [`mount_data`] has no way to add it: there is no parameter for it.
//!
//! # Why paths are validated by character set rather than escaped
//!
//! `mount(2)`'s data argument is a single comma-separated string that overlayfs re-parses;
//! `:` separates lower layers and `\` is an escape. A path containing `,metacopy=on` would
//! silently re-enable the one option ADR-010 calls security-load-bearing, and a `:` would
//! splice in an extra lower layer. Escaping is overlayfs-version-specific and easy to get
//! subtly wrong; refusing every byte outside a conservative set is not. The paths come from
//! the harness operator, not the tool, so this is defence in depth rather than the primary
//! boundary — but it is the cheapest one available and it fails loudly.

use std::ffi::CString;
use std::fmt;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// ADR-010's pinned overlayfs options, verbatim and in this order. Not configurable.
pub const PINNED_OPTIONS: &str = "redirect_dir=off,metacopy=off,index=off";

/// Why a path was refused as an overlay layer, scratch location, or in-sandbox path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathRejection {
    /// Not absolute. Relative paths would resolve against whatever the cwd happens to be at
    /// mount time, which in the child is not the caller's.
    NotAbsolute,
    /// Empty, or exactly `/`.
    Root,
    /// Contains a byte outside `[A-Za-z0-9/._+-]` (the byte is reported).
    ForbiddenByte(u8),
    /// Contains a `.` or `..` segment, an empty segment (`//`), or a trailing `/`. Rejected
    /// rather than normalised so the path that is validated is the path that is mounted.
    NonCanonical,
}

impl fmt::Display for PathRejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAbsolute => f.write_str("path is not absolute"),
            Self::Root => f.write_str("path is empty or the filesystem root"),
            Self::ForbiddenByte(b) => write!(
                f,
                "path contains byte 0x{b:02x}, outside the permitted set [A-Za-z0-9/._+-] \
                 (overlayfs option parsing treats ',' ':' '\\' '=' specially)"
            ),
            Self::NonCanonical => {
                f.write_str("path has a '.', '..' or empty segment, or a trailing '/'")
            }
        }
    }
}

fn permitted(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'_' | b'+' | b'-')
}

/// Validate one absolute path for use in an overlay option string or as an in-sandbox path.
///
/// # Errors
///
/// The first [`PathRejection`] that applies.
pub fn validate_path(path: &Path) -> Result<(), PathRejection> {
    let bytes = path.as_os_str().as_bytes();
    if bytes.is_empty() || bytes == b"/" {
        return Err(PathRejection::Root);
    }
    if bytes[0] != b'/' {
        return Err(PathRejection::NotAbsolute);
    }
    if let Some(&b) = bytes.iter().find(|&&b| !permitted(b)) {
        return Err(PathRejection::ForbiddenByte(b));
    }
    // Skip the leading '/', then every segment must be non-empty and not '.'/'..'. A
    // trailing '/' yields an empty final segment and is caught by the same rule.
    for segment in bytes[1..].split(|&b| b == b'/') {
        if segment.is_empty() || segment == b"." || segment == b".." {
            return Err(PathRejection::NonCanonical);
        }
    }
    Ok(())
}

/// Which overlay role a rejected path was offered for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// `lowerdir`.
    Lower,
    /// `upperdir`.
    Upper,
    /// `workdir`.
    Work,
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Lower => "lowerdir",
            Self::Upper => "upperdir",
            Self::Work => "workdir",
        })
    }
}

/// Build the complete `mount(2)` data string for one overlay:
/// `lowerdir=L,upperdir=U,workdir=W,` followed by [`PINNED_OPTIONS`]. Exactly one lower
/// layer; no `userxattr` (ADR-016).
///
/// # Errors
///
/// The role and reason of the first path that fails [`validate_path`].
pub fn mount_data(lower: &Path, upper: &Path, work: &Path) -> Result<CString, (Role, PathRejection)> {
    for (role, path) in [(Role::Lower, lower), (Role::Upper, upper), (Role::Work, work)] {
        validate_path(path).map_err(|reason| (role, reason))?;
    }
    let mut data = Vec::new();
    data.extend_from_slice(b"lowerdir=");
    data.extend_from_slice(lower.as_os_str().as_bytes());
    data.extend_from_slice(b",upperdir=");
    data.extend_from_slice(upper.as_os_str().as_bytes());
    data.extend_from_slice(b",workdir=");
    data.extend_from_slice(work.as_os_str().as_bytes());
    data.push(b',');
    data.extend_from_slice(PINNED_OPTIONS.as_bytes());
    // Cannot contain NUL: validate_path admits no NUL byte.
    Ok(CString::new(data).expect("validated paths contain no NUL"))
}

/// Whether two absolute, already-canonicalised paths name disjoint subtrees — neither is the
/// other nor an ancestor of the other. Component-wise, so `/a/b` and `/a/bc` are disjoint.
///
/// The supervisor requires the lower layer and the scratch directory (which holds the upper
/// and work directories) to be disjoint: an upperdir inside the lowerdir would be visible —
/// and readable — from inside the sandbox through the lower layer, which is exactly the
/// reachability ADR-011 decision 5 says must be ruled out.
#[must_use]
pub fn disjoint(a: &Path, b: &Path) -> bool {
    !a.starts_with(b) && !b.starts_with(a)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> &Path {
        Path::new(s)
    }

    #[test]
    fn mount_data_is_exactly_the_three_layers_plus_the_pinned_options() {
        let data = mount_data(p("/srv/lower"), p("/srv/run-1/upper"), p("/srv/run-1/work")).unwrap();
        assert_eq!(
            data.to_bytes(),
            b"lowerdir=/srv/lower,upperdir=/srv/run-1/upper,workdir=/srv/run-1/work,\
              redirect_dir=off,metacopy=off,index=off"
        );
    }

    #[test]
    fn the_privileged_branch_never_sets_userxattr() {
        // ADR-016: privileged mount, so ADR-010's conditional `userxattr` must not be set.
        // `nouserxattr` is the kernel default and is echoed by /proc/mounts on its own.
        let data = mount_data(p("/l"), p("/u"), p("/w")).unwrap();
        let s = std::str::from_utf8(data.to_bytes()).unwrap();
        assert!(!s.contains("userxattr"), "{s}");
        for opt in ["redirect_dir=off", "metacopy=off", "index=off"] {
            assert_eq!(s.matches(opt).count(), 1, "{opt} must appear exactly once in {s}");
        }
        assert!(!s.contains("=on"), "{s}");
    }

    #[test]
    fn option_injection_through_a_path_is_refused() {
        // A comma would append an option (re-enabling metacopy); a colon would splice in an
        // extra lower layer; a backslash is overlayfs's escape character; '=' is refused too.
        for (bad, byte) in [
            ("/srv/lower,metacopy=on", b','),
            ("/srv/lower:/etc", b':'),
            ("/srv/low\\er", b'\\'),
            ("/srv/a=b", b'='),
            ("/srv/a b", b' '),
            ("/srv/caf\u{e9}", 0xc3),
        ] {
            assert_eq!(validate_path(p(bad)), Err(PathRejection::ForbiddenByte(byte)), "{bad}");
            assert_eq!(
                mount_data(p(bad), p("/u"), p("/w")),
                Err((Role::Lower, PathRejection::ForbiddenByte(byte)))
            );
        }
        assert_eq!(
            mount_data(p("/l"), p("/u,index=on"), p("/w")),
            Err((Role::Upper, PathRejection::ForbiddenByte(b',')))
        );
        assert_eq!(
            mount_data(p("/l"), p("/u"), p("/w:/x")),
            Err((Role::Work, PathRejection::ForbiddenByte(b':')))
        );
    }

    #[test]
    fn non_canonical_and_relative_paths_are_refused() {
        assert_eq!(validate_path(p("srv/lower")), Err(PathRejection::NotAbsolute));
        assert_eq!(validate_path(p("")), Err(PathRejection::Root));
        assert_eq!(validate_path(p("/")), Err(PathRejection::Root));
        for bad in ["/srv/../etc", "/srv/./lower", "/srv//lower", "/srv/lower/", "/..", "/."] {
            assert_eq!(validate_path(p(bad)), Err(PathRejection::NonCanonical), "{bad}");
        }
        for good in ["/srv/lower", "/tmp/.tmpAb3_x+y-z", "/a", "/srv/..hidden", "/srv/x.."] {
            assert_eq!(validate_path(p(good)), Ok(()), "{good}");
        }
    }

    #[test]
    fn disjointness_is_component_wise() {
        assert!(disjoint(p("/a/b"), p("/a/bc")));
        assert!(disjoint(p("/srv/lower"), p("/srv/scratch")));
        assert!(!disjoint(p("/srv/lower"), p("/srv/lower/run/upper")));
        assert!(!disjoint(p("/srv/scratch/lower"), p("/srv/scratch")));
        assert!(!disjoint(p("/srv/x"), p("/srv/x")));
        assert!(!disjoint(p("/"), p("/srv/x")));
    }
}
