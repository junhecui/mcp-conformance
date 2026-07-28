//! P4-01: a seccomp-bpf filter denying escape-class syscalls, installed in the real target's
//! own process image just before its `execvp` (`run_sandboxed_init`'s second-fork child —
//! see that function's own doc comment for why this project's fork/exec shape has a
//! dedicated child branch to install this into). Seccomp filters persist across `execve` by
//! kernel design, so the target program itself cannot shed this once installed — the last
//! thing this process does before becoming the real target's code is arm the one restriction
//! that code can never remove.
//!
//! Unconditional, unlike [`crate::SandboxSpec::network_isolated`]: none of the denied
//! syscalls below (`mount`, `ptrace`, `bpf`, `kexec_load`, and similar) are ones any
//! legitimate MCP tool has a reason to call, so there is no compatibility trade-off here the
//! way there was for network isolation — every [`crate::spawn`] call gets this filter,
//! with no opt-out field, the same "not configurable off" posture ADR-004 already requires
//! of the integrity gate that consumes what this filter's denials imply.
//!
//! # Non-fatal denial (`SECCOMP_RET_ERRNO`), not a crash (`SECCOMP_RET_TRAP`/`KILL`)
//!
//! A denied syscall fails with `EPERM` and the process **keeps running** — verified directly,
//! not assumed, against three candidate actions before choosing one: `SECCOMP_RET_TRAP`
//! delivers `SIGSYS`, which (with no handler installed, which no ordinary tool installs)
//! terminates the process on the very *first* escape attempt; `SECCOMP_RET_KILL_PROCESS` is
//! more of the same. Both would make P4-05's "every attempt appears in evidence" impossible
//! to fulfil for a hostile server that tries several different escape-class syscalls across
//! its lifetime — the first one would end the run before any of the others could be
//! attempted, let alone recorded. `SECCOMP_RET_ERRNO` lets a hostile process try as many
//! different escape-class syscalls as it wants, each one denied and each one (P4-02) logged
//! independently.
//!
//! # `SECCOMP_FILTER_FLAG_LOG` and the kernel audit subsystem — verified, not assumed, to be
//! observable in this project's own container
//!
//! Every denial is installed with `SECCOMP_FILTER_FLAG_LOG`, which makes the kernel emit a
//! real `AUDIT_SECCOMP` (`type=1326`) record through the audit subsystem for every action
//! this filter takes. Confirmed directly, before writing this module, that such records are
//! genuinely observable in this project's own dev container with no `auditd` running at all:
//! with `/proc/sys/kernel/dmesg_restrict` at `0`, the kernel's audit-record fallback path
//! lands the exact same record in the kernel ring buffer, readable from `/dev/kmsg` — the
//! mechanism `observe::seccomp_audit` (P4-02) is built on. Also confirmed directly: the
//! record's `pid=` field reports the denying process's PID **in the initial (host) PID
//! namespace**, not its namespace-local self-view (which would be `1`, since the real target
//! is PID 1 of its own namespace per P2-01) — exactly the PID this crate's own supervisor
//! already tracks internally for `waitpid`, so attributing a denial record to the right run
//! needs no new bookkeeping.
//!
//! # Escape-class syscall list — representative and disclosed, not claimed exhaustive
//!
//! Matches this task's own framing ("mount, ptrace, bpf, kexec **and similar**") as a
//! denylist over a small, well-understood set of syscalls with no legitimate use inside a
//! sandboxed MCP tool, grouped by the kind of escape each category represents:
//! - **Filesystem/namespace escape:** `mount`, `umount2`, `pivot_root`, `chroot`, `unshare`,
//!   `setns` — every one of this crate's own namespace/overlay primitives, available here to
//!   a hostile *target* rather than the supervisor that's supposed to own them exclusively.
//! - **Process introspection/injection:** `ptrace`, `process_vm_readv`, `process_vm_writev`
//!   — reading or writing another process's memory or registers.
//! - **Kernel-level backdoor:** `bpf` — loading further BPF programs (including, notably,
//!   seccomp filters more permissive than this one, or reading kernel memory via a
//!   vulnerable verifier).
//! - **Persistence/code execution beyond this process:** `kexec_load`, `kexec_file_load`,
//!   `init_module`, `finit_module`, `delete_module`, `reboot`.
//!
//! Not an allowlist (which would be far more thorough but would risk breaking legitimate
//! tool behaviour this project has not catalogued) and not claimed complete — a smaller,
//! disclosed, denylist matching what the task itself asks for.

use std::io;

/// `AUDIT_ARCH_X86_64` from `<linux/audit.h>` (`EM_X86_64` with the 64-bit and
/// little-endian bits set) — not exposed by the mainline `libc` crate. Checked on every
/// syscall the filter evaluates so a 32-bit-ABI syscall (a classic seccomp-bypass technique
/// on a 64-bit kernel) can never be confused with its 64-bit counterpart; any other
/// architecture value kills the process outright rather than silently falling through.
const AUDIT_ARCH_X86_64: u32 = 0xC000_003E;

/// `SECCOMP_RET_*` action values from `<linux/seccomp.h>` — not exposed by the mainline
/// `libc` crate (unlike `SECCOMP_SET_MODE_FILTER`/`SECCOMP_FILTER_FLAG_LOG`, which are).
const SECCOMP_RET_KILL_PROCESS: u32 = 0x8000_0000;
const SECCOMP_RET_ERRNO: u32 = 0x0005_0000;
const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
/// The low 16 bits of a `SECCOMP_RET_ERRNO` return value carry the errno to report.
const SECCOMP_RET_DATA_MASK: u32 = 0x0000_ffff;

/// Byte offsets into the kernel's `struct seccomp_data` a classic-BPF program can load via
/// `BPF_LD | BPF_W | BPF_ABS` — `nr` (the syscall number) at `0`, `arch` at `4`.
const SECCOMP_DATA_OFFSET_NR: u32 = 0;
const SECCOMP_DATA_OFFSET_ARCH: u32 = 4;

/// The escape-class syscalls this filter denies — see this module's own doc comment for the
/// grouped rationale behind each one.
const DENIED_SYSCALLS: &[i64] = &[
    libc::SYS_mount,
    libc::SYS_umount2,
    libc::SYS_pivot_root,
    libc::SYS_chroot,
    libc::SYS_unshare,
    libc::SYS_setns,
    libc::SYS_ptrace,
    libc::SYS_process_vm_readv,
    libc::SYS_process_vm_writev,
    libc::SYS_bpf,
    libc::SYS_kexec_load,
    libc::SYS_kexec_file_load,
    libc::SYS_init_module,
    libc::SYS_finit_module,
    libc::SYS_delete_module,
    libc::SYS_reboot,
];

fn stmt(code: u16, k: u32) -> libc::sock_filter {
    libc::sock_filter { code, jt: 0, jf: 0, k }
}

fn jump(code: u16, k: u32, jt: u8, jf: u8) -> libc::sock_filter {
    libc::sock_filter { code, jt, jf, k }
}

/// Build the classic-BPF program: verify the architecture, then test the syscall number
/// against every entry in [`DENIED_SYSCALLS`] in turn, denying a match and allowing anything
/// else.
///
/// One `LD` of `nr` is reused across every comparison (classic BPF's accumulator register
/// persists across instructions) — each `JEQ` either falls through to the next candidate or
/// jumps forward exactly far enough to land on the single shared `RET` (`SECCOMP_RET_ERRNO`)
/// instruction at the end, skipping the `RET SECCOMP_RET_ALLOW` immediately before it. This
/// exact jump arithmetic — and the whole program's behaviour end to end, including that a
/// syscall *not* in the list still executes normally afterward — was verified directly
/// against a real kernel before this function was written, not derived from the classic-BPF
/// spec alone.
fn build_program() -> Vec<libc::sock_filter> {
    let mut program = Vec::with_capacity(4 + DENIED_SYSCALLS.len());
    program.push(stmt((libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16, SECCOMP_DATA_OFFSET_ARCH));
    program.push(jump((libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16, AUDIT_ARCH_X86_64, 1, 0));
    program.push(stmt((libc::BPF_RET | libc::BPF_K) as u16, SECCOMP_RET_KILL_PROCESS));
    program.push(stmt((libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16, SECCOMP_DATA_OFFSET_NR));

    let denied_count = DENIED_SYSCALLS.len();
    for (i, &nr) in DENIED_SYSCALLS.iter().enumerate() {
        // Jump forward exactly far enough to skip every remaining candidate check plus the
        // `RET ALLOW` instruction, landing on the shared `RET ERRNO` at the very end.
        let jump_to_deny = u8::try_from(denied_count - i).expect("fewer than 256 denied syscalls");
        program.push(jump((libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16, nr as u32, jump_to_deny, 0));
    }
    program.push(stmt((libc::BPF_RET | libc::BPF_K) as u16, SECCOMP_RET_ALLOW));
    program.push(stmt(
        (libc::BPF_RET | libc::BPF_K) as u16,
        SECCOMP_RET_ERRNO | (libc::EPERM as u32 & SECCOMP_RET_DATA_MASK),
    ));
    program
}

/// Install [`build_program`]'s filter in the calling process, to take effect for it and
/// every process it ever `execve`s into. Must be called after any setup this process itself
/// still needs one of the denied syscalls for (there is none, by construction — `mount`
/// already happened in fork-1, before this crate's second fork even exists) and before the
/// real target's own `execvp`.
///
/// # `unsafe_code`
///
/// `seccomp(2)` has no safe wrapper in this dependency tree (`libc` exposes the
/// `SECCOMP_SET_MODE_FILTER`/`SECCOMP_FILTER_FLAG_LOG` constants and the `sock_filter`/
/// `sock_fprog` structs, but not a function to call it with) — a raw `syscall()` is the only
/// way to reach it. `prctl(PR_SET_NO_NEW_PRIVS)` similarly has no typed wrapper for this
/// exact operation in `nix` at the version this crate depends on.
#[allow(unsafe_code)]
pub(crate) fn install_escape_class_denylist() -> Result<(), io::Error> {
    // SAFETY: `PR_SET_NO_NEW_PRIVS` takes no pointer arguments; the trailing zeros are
    // ignored by the kernel for this operation. Required (independent of this process's
    // actual capability set) before an unprivileged seccomp filter install is permitted at
    // all.
    let no_new_privs = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
    if no_new_privs != 0 {
        return Err(io::Error::last_os_error());
    }

    let program = build_program();
    let fprog = libc::sock_fprog {
        len: u16::try_from(program.len()).expect("fewer than 65536 BPF instructions"),
        filter: program.as_ptr().cast_mut(),
    };
    // SAFETY: `fprog` borrows `program`, which outlives this call; `seccomp(2)` only reads
    // through the pointer for the duration of the syscall and does not retain it afterward.
    let ret = unsafe {
        libc::syscall(
            libc::SYS_seccomp,
            libc::SECCOMP_SET_MODE_FILTER,
            libc::SECCOMP_FILTER_FLAG_LOG,
            std::ptr::from_ref(&fprog),
        )
    };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
