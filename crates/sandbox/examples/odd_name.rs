//! Test helper for `sandbox`'s root tests; not part of the harness.
//!
//! Gives a thread a `comm` that is not UTF-8 (`prctl(PR_SET_NAME, "\xff\xfe")`), then keeps
//! appending a line to the file named by its first argument. The kernel escapes `comm` in
//! `/proc/<pid>/status` with only `ESCAPE_SPACE | ESCAPE_SPECIAL`, so the raw bytes appear in the
//! `Name:` line — and a UTF-8 read of that status fails. The round-4 review showed the scanner
//! then treated the process as absent: a false clean.
//!
//! `odd_name <file>` renames the main thread (the process's own status is non-UTF-8);
//! `odd_name <file> thread` renames only a second thread (only `task/<tid>/status` is).
//! An example target rather than a binary so it never ships with the crate.

use std::io::Write;
use std::time::Duration;

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: odd_name <file> [thread]");
    let per_thread = args.next().as_deref() == Some("thread");
    let work = move || {
        loop {
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
                let _ = f.write_all(b"o\n");
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    };
    if per_thread {
        let worker = std::thread::spawn(move || {
            set_own_name();
            work()
        });
        let _ = worker.join();
    } else {
        set_own_name();
        work();
    }
}

/// Set the calling thread's `comm` to the two bytes `0xFF 0xFE`.
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn set_own_name() {
    let name: [u8; 3] = [0xff, 0xfe, 0];
    // SAFETY: `PR_SET_NAME` reads a NUL-terminated string of at most 16 bytes from the pointer;
    // `name` is a live 3-byte stack array ending in NUL.
    let rc = unsafe { libc::prctl(libc::PR_SET_NAME, name.as_ptr(), 0_u64, 0_u64, 0_u64) };
    assert_eq!(rc, 0, "PR_SET_NAME failed");
}

#[cfg(not(target_os = "linux"))]
fn set_own_name() {}
