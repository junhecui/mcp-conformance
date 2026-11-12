//! Test helper for `sandbox`'s root tests; not part of the harness.
//!
//! Reproduces the round-5 race: a thread-group leader that reads `State: Z`/`X` with
//! `Threads: 0` while the process is still alive. Each iteration spawns a worker thread, ends
//! the **main** thread with a raw `SYS_exit` (so the leader becomes a zombie), and has the
//! worker `execve` this same binary. `de_thread` then swaps the worker onto the tgid and
//! `release_task`s the old leader; a `/proc/<pid>/status` read that took its reference just
//! before the swap prints the released leader with `Threads: 0`. The process keeps its pid
//! across the `execve`, so it stays the same pid throughout — a scanner that read `Threads: 0`
//! as "gone" would miss a live descendant (round-5 review).
//!
//! The re-exec targets the binary's **resolved absolute path**, read at startup while the
//! process is healthy, not `/proc/self/exe`: once the leader is a zombie, `execve` of the
//! magic symlink returns `ENOENT` and the loop would die after one iteration.
//!
//! An example target rather than a binary so it never ships with the crate.

#[cfg(target_os = "linux")]
fn main() {
    use std::time::Duration;
    // Resolve the executable now, single-threaded with a live leader, so the symlink resolves.
    let exe = std::env::current_exe().expect("current_exe");
    std::thread::spawn(move || {
        // Long enough for the main thread to have SYS_exited and become a zombie leader; the
        // exec from this non-leader thread then triggers de_thread and the Threads: 0 window.
        std::thread::sleep(Duration::from_micros(200));
        exec_path(&exe);
    });
    exit_main_thread();
}

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn exec_path(path: &std::path::Path) -> ! {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).expect("path has no NUL");
    let argv = [c.as_ptr(), std::ptr::null()];
    // SAFETY: `execv` reads a NUL-terminated path and a NULL-terminated argv of valid C
    // strings; both live for the call. On success it never returns; on failure we exit.
    unsafe {
        libc::execv(c.as_ptr(), argv.as_ptr());
        libc::_exit(0)
    }
}

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn exit_main_thread() -> ! {
    // SAFETY: `SYS_exit` terminates the calling thread only (not the group); the worker thread
    // keeps running. It does not return.
    unsafe { libc::syscall(libc::SYS_exit, 0) };
    unreachable!("SYS_exit returned")
}

#[cfg(not(target_os = "linux"))]
fn main() {}
