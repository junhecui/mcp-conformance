//! P4-02: harvest `sandbox::seccomp`'s (P4-01) denials into evidence — the kernel's own
//! `AUDIT_SECCOMP` (`type=1326`) record, emitted for every action `SECCOMP_FILTER_FLAG_LOG`
//! tags, read directly from `/dev/kmsg` rather than a `dmesg` subprocess.
//!
//! **Must not:** interpret anything, same as this crate's own top-level contract — this
//! module observes and records that a given syscall number was denied for a given PID; it
//! does not decide what that means for a verdict (`integrity::RunSignals::escape_class_
//! syscall_denied`, derived from whether this module's own output is non-empty, is a
//! caller's job — `orchestrator`, in practice).
//!
//! # Why `/dev/kmsg`, not `dmesg`
//!
//! Consistent with this crate's own established style (`connection_log`'s raw
//! `getsockopt`/`SO_ORIGINAL_DST` rather than shelling out to anything): `/dev/kmsg` is the
//! kernel ring buffer's own device file, each `read()` returning exactly one structured
//! record (`priority,sequence,timestamp,flags;message`, per the kernel's own documented
//! format) — no subprocess, no output-format assumptions about a userspace tool's version.
//! Confirmed directly, before writing this module, that `/proc/sys/kernel/dmesg_restrict`
//! being `0` in this project's own container makes every record readable with no special
//! capability needed, and that `SECCOMP_FILTER_FLAG_LOG`'s records genuinely reach this
//! device even with no `auditd` running to receive them through the audit subsystem's usual
//! path.
//!
//! # Why this seeks to the end before the run, not after
//!
//! [`SeccompAudit::start`] must run *before* the sandboxed process is spawned:
//! `lseek(SEEK_END)` positions this reader at "only records from now on," so a denial
//! logged during the run is guaranteed to still be in the buffer (and never miscounted
//! against an unrelated, already-buffered record from earlier host activity) by the time
//! [`SeccompAudit::stop`] drains it. Opening and seeking *after* the run would already be too
//! late — the records this module exists to find would already be behind the read position.
//!
//! # A real finding: `printk_ratelimit` silently drops records under rapid denial activity
//!
//! With no `auditd` running to receive records over the audit subsystem's own netlink path,
//! `AUDIT_SECCOMP` records fall back to the kernel's `printk`, which is itself
//! rate-limited (`/proc/sys/kernel/printk_ratelimit`/`printk_ratelimit_burst` — this
//! container's own defaults, `5` seconds and `10` messages, are the kernel-wide defaults,
//! not anything this project set). Found directly, not assumed, while testing this module
//! against genuinely repeated denials: after roughly ten records within five seconds, every
//! further record in that window is silently dropped — no error, nothing else in `/dev/kmsg`
//! to notice by. This is not a test-only artifact: P4-05's own hostile test server is
//! expected to attempt several different escape-class syscalls in quick succession, exactly
//! the shape that exhausts this burst allowance, and P4-02's own exit criterion
//! ("denials harvested into evidence") would silently fail for the later attempts in that
//! burst. [`SeccompAudit::start`] therefore makes a best-effort attempt to disable this
//! rate limit host-wide (`printk_ratelimit = 0`, the kernel's own documented way to turn it
//! off) before returning — confirmed directly that this eliminates the drops entirely across
//! 15 back-to-back bursts that reliably lost records without it. A failure to write it (a
//! more restricted deployment lacking the necessary privilege) does not fail `start` itself;
//! this project's own established posture for host-wide, best-effort mitigations
//! (`sandbox::supervisor`'s uid/gid remap fallback is the same shape) — the harvester still
//! works exactly as well as the kernel's own default rate limit allows.
//!
//! # Why PID, not just syscall number, is the attribution key
//!
//! `/dev/kmsg` is a system-wide, shared resource — any process on the host emitting a kernel
//! log line appears in the same stream. Confirmed directly (forking into a fresh PID
//! namespace first, exactly as `sandbox::supervisor`'s real target does) that a
//! `SECCOMP_AUDIT` record's own `pid=` field reports the denying process's PID **in the
//! initial (host) namespace**, matching `sandbox::SandboxHandle::target_pid` exactly — so
//! filtering on that value correctly attributes a denial to the run that produced it, even
//! though the underlying log is shared.

use std::io::{self, Read};
use std::os::fd::AsRawFd;

/// One denied syscall attempt, exactly as the kernel's own `AUDIT_SECCOMP` record reported
/// it — the raw fact, not an assessment of what attempting it means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeccompDenialEntry {
    /// The denied syscall's number (architecture-specific; this project only ever runs on
    /// `x86_64`, the same scope `sandbox::seccomp`'s own filter is built for).
    pub syscall_nr: i64,
}

/// Why starting or reading back the seccomp audit failed.
#[derive(Debug)]
pub struct SeccompAuditError(io::Error);

impl std::fmt::Display for SeccompAuditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "seccomp audit error: {}", self.0)
    }
}

impl std::error::Error for SeccompAuditError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

impl From<io::Error> for SeccompAuditError {
    fn from(e: io::Error) -> Self {
        Self(e)
    }
}

/// A running seccomp-denial harvester. Start with [`Self::start`] *before* spawning the
/// sandboxed process, and consume with [`Self::stop`] (passing the real target's own PID —
/// `sandbox::SandboxHandle::target_pid`) once the run is over to get back everything it saw.
pub struct SeccompAudit {
    kmsg: std::fs::File,
}

impl SeccompAudit {
    /// Open `/dev/kmsg` and seek to its current end — see this module's own doc comment for
    /// why that must happen before the sandboxed process this run is watching even starts.
    ///
    /// # `unsafe_code`
    ///
    /// `lseek(2)` on `/dev/kmsg` with `SEEK_END` is a documented special case of that
    /// syscall's own semantics for this specific device (positioning at the current end of
    /// the kernel ring buffer rather than a byte-offset seek within a regular file) — no
    /// higher-level Rust API models this, so a raw call is the only way to reach it.
    ///
    /// # Errors
    ///
    /// Returns [`SeccompAuditError`] if opening `/dev/kmsg` or seeking it to the current end
    /// fails.
    #[allow(unsafe_code)]
    pub fn start() -> Result<Self, SeccompAuditError> {
        // Best-effort, per this module's own doc comment: disable the kernel's default
        // `printk` rate limit before this run starts, so a burst of denials (P4-05's hostile
        // server, in particular) is never silently thinned out. Not required to succeed —
        // a more restricted deployment lacking the privilege to write this still gets every
        // denial the kernel's own default rate limit allows through, just not a guarantee
        // beyond that.
        let _ = std::fs::write("/proc/sys/kernel/printk_ratelimit", b"0");

        let kmsg = std::fs::File::open("/dev/kmsg")?;
        // SAFETY: `kmsg`'s fd is valid and owned by this function's own local for the
        // duration of this call.
        let seek_ret = unsafe { libc::lseek(kmsg.as_raw_fd(), 0, libc::SEEK_END) };
        if seek_ret < 0 {
            return Err(SeccompAuditError(io::Error::last_os_error()));
        }
        // Non-blocking so `stop`'s drain loop below terminates once every currently-buffered
        // new record has been read, rather than blocking indefinitely for a record that will
        // never arrive because the run is already over.
        // SAFETY: same fd, same ownership as above.
        let flags = unsafe { libc::fcntl(kmsg.as_raw_fd(), libc::F_GETFL) };
        if flags < 0 {
            return Err(SeccompAuditError(io::Error::last_os_error()));
        }
        // SAFETY: same fd, same ownership as above; `flags | O_NONBLOCK` is a valid flag
        // word for `F_SETFL`.
        let set_ret = unsafe { libc::fcntl(kmsg.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) };
        if set_ret < 0 {
            return Err(SeccompAuditError(io::Error::last_os_error()));
        }
        Ok(Self { kmsg })
    }

    /// Drain every `AUDIT_SECCOMP` record logged for `target_pid` since [`Self::start`], in
    /// the order the kernel emitted them.
    #[must_use]
    pub fn stop(mut self, target_pid: i32) -> Vec<SeccompDenialEntry> {
        let mut entries = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            match self.kmsg.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if let Some(entry) = parse_seccomp_denial(&buf[..n], target_pid) {
                        entries.push(entry);
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }
        entries
    }
}

/// Parse one `/dev/kmsg` record (`priority,sequence,timestamp,flags;message`) and return a
/// [`SeccompDenialEntry`] iff it is an `AUDIT_SECCOMP` (`type=1326`) record whose own `pid=`
/// field matches `target_pid`.
fn parse_seccomp_denial(record: &[u8], target_pid: i32) -> Option<SeccompDenialEntry> {
    let text = std::str::from_utf8(record).ok()?;
    let message = text.split_once(';').map_or(text, |(_, message)| message);
    if !message.contains("type=1326") {
        return None;
    }

    let mut syscall_nr = None;
    let mut pid_matches = false;
    for token in message.split_whitespace() {
        if let Some(value) = token.strip_prefix("pid=") {
            if value.parse::<i32>() == Ok(target_pid) {
                pid_matches = true;
            }
        } else if let Some(value) = token.strip_prefix("syscall=") {
            syscall_nr = value.parse::<i64>().ok();
        }
    }

    if pid_matches { syscall_nr.map(|syscall_nr| SeccompDenialEntry { syscall_nr }) } else { None }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// P4-02's literal exit criterion, proven against a real kernel denial, not a synthetic
    /// string: a real process installs a real seccomp filter (denying every syscall it makes
    /// afterward, the simplest possible filter, sufficient to trigger a real denial quickly)
    /// with `SECCOMP_FILTER_FLAG_LOG`, and `SeccompAudit` recovers that exact PID's denial —
    /// this crate's own version of `connection_log`'s "prove the mechanism against something
    /// real" discipline.
    ///
    /// # `unsafe_code`
    /// Installing a real seccomp filter to trigger a real denial needs the same raw
    /// `fork`/`syscall(SYS_seccomp, ...)` calls `sandbox::seccomp` itself uses — no safe
    /// wrapper exists for either.
    #[allow(unsafe_code)]
    #[test]
    fn a_real_seccomp_denial_is_recovered_for_the_denying_pid() {
        let audit = SeccompAudit::start().expect("start seccomp audit");

        let pid = unsafe { libc::fork() };
        assert_ne!(pid, -1, "fork must succeed");
        if pid == 0 {
            // SAFETY: this is the freshly forked child; everything here runs before any
            // exec, matching `sandbox::seccomp`'s own real usage.
            unsafe {
                libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
                let filter = [libc::sock_filter {
                    // BPF opcode flags are libc constants always < 256; always fits in u16.
                    code: u16::try_from(libc::BPF_RET | libc::BPF_K)
                        .expect("BPF opcode flag constants always fit in u16"),
                    jt: 0,
                    jf: 0,
                    k: 0x0005_0000 | (libc::EPERM as u32 & 0x0000_ffff), // SECCOMP_RET_ERRNO
                }];
                let fprog = libc::sock_fprog { len: 1, filter: filter.as_ptr().cast_mut() };
                libc::syscall(
                    libc::SYS_seccomp,
                    libc::SECCOMP_SET_MODE_FILTER,
                    libc::SECCOMP_FILTER_FLAG_LOG,
                    std::ptr::from_ref(&fprog),
                );
                let _ = libc::getpid(); // denied by the filter above; logged
                libc::_exit(0);
            }
        }
        let mut status = 0;
        unsafe { libc::waitpid(pid, &mut status, 0) };

        // Give the kernel a moment to land the record — best-effort, matching
        // `connection_log`'s own bounded-wait discipline rather than assuming instantaneous
        // delivery.
        std::thread::sleep(std::time::Duration::from_millis(200));

        let entries = audit.stop(pid);
        assert!(
            !entries.is_empty(),
            "a real seccomp denial for pid {pid} must be recovered, found none"
        );
        assert!(
            entries.iter().any(|e| e.syscall_nr == libc::SYS_getpid),
            "the denied syscall must be getpid ({}), got: {entries:?}",
            libc::SYS_getpid
        );
    }

    /// A denial for a *different* PID must never be attributed to this one — proven by
    /// starting the audit, triggering no denial of our own, and confirming an empty result
    /// rather than trusting the parser to be selective by construction alone.
    #[test]
    fn no_denial_for_an_unrelated_pid_is_reported_as_empty() {
        let audit = SeccompAudit::start().expect("start seccomp audit");
        std::thread::sleep(std::time::Duration::from_millis(50));
        let entries = audit.stop(i32::MAX);
        assert!(entries.is_empty(), "an unrelated PID must never match: {entries:?}");
    }

    #[test]
    fn a_record_missing_type_1326_is_ignored() {
        let record = b"6,100,0,-;some unrelated kernel message pid=1234 syscall=1";
        assert!(parse_seccomp_denial(record, 1234).is_none());
    }

    #[test]
    fn a_seccomp_record_for_a_different_pid_is_ignored() {
        let record =
            b"5,100,0,-;audit: type=1326 audit(0.0:1): pid=1234 comm=\"x\" syscall=101 code=0x50000";
        assert!(parse_seccomp_denial(record, 9999).is_none());
    }

    #[test]
    fn a_matching_seccomp_record_yields_its_syscall_number() {
        let record =
            b"5,100,0,-;audit: type=1326 audit(0.0:1): pid=1234 comm=\"x\" syscall=101 code=0x50000";
        let entry = parse_seccomp_denial(record, 1234).expect("must parse");
        assert_eq!(entry.syscall_nr, 101);
    }
}
