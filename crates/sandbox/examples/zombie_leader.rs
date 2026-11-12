//! Test helper for `sandbox`'s root tests; not part of the harness.
//!
//! Starts a thread that keeps appending a line to the file named by its first argument, then
//! ends the **main** thread with a raw `SYS_exit`. The process's thread-group leader becomes a
//! zombie (`State: Z`, `Threads: 2`, `/proc/<pid>/ns/mnt` unresolvable) while the other thread
//! keeps running — the round-3 review's false-clean case, in which every `/proc` scan treated
//! the process as dead.
//!
//! An example target rather than a binary so it never ships with the crate; `cargo test`
//! builds examples, and the tests find it next to their own executable.

use std::io::Write;
use std::time::Duration;

fn main() {
    let path = std::env::args().nth(1).expect("usage: zombie_leader <file>");
    std::thread::spawn(move || {
        loop {
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
                let _ = f.write_all(b"z\n");
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    });
    std::thread::sleep(Duration::from_millis(300));
    exit_main_thread();
}

/// End only the calling (main) thread; the process lives on in the other one.
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn exit_main_thread() -> ! {
    // SAFETY: `SYS_exit` (not `exit_group`) terminates the calling thread only. It does not
    // return, so nothing on this thread observes the skipped destructors, and the other
    // thread holds only its own owned data.
    unsafe { libc::syscall(libc::SYS_exit, 0) };
    unreachable!("SYS_exit returned")
}

#[cfg(not(target_os = "linux"))]
fn exit_main_thread() -> ! {
    std::process::exit(0)
}
