//! Sweep mechanics shared by the census sweeps (P0-10): a bounded, order-preserving worker
//! pool, a stop flag, and the free-disk-space floor checked before every launch.
//!
//! Nothing here knows what an attempt *is* — Stage 1 hands it HTTP discoveries with
//! `jobs = 1`, Stage 2 hands it containerized ones with `jobs` up to [`crate::cli::MAX_JOBS`].
//! Keeping the pool generic is what lets its two load-bearing properties be tested against
//! synthetic work rather than live servers: output order never depends on completion order,
//! and once launching stops, the attempted set is always a contiguous prefix of the input.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

/// Below this much free space on any volume the sweep writes to, no new attempt launches.
///
/// The host this runs on has been near-full (tasks.md P0-10); a Stage 2 attempt can pull a
/// multi-hundred-MB image and every attempt persists up to ~20 MiB of evidence (two responses,
/// each capped at 10 MiB by the discovery transports). 3 GiB leaves room for in-flight
/// attempts to finish and for the results file to be written after the floor is hit.
pub(crate) const MIN_FREE_BYTES: u64 = 3 * 1024 * 1024 * 1024;

/// A one-way "stop launching" signal with the first reason that tripped it.
///
/// Distinct from the pool's own stop state on purpose: the pool stops when its preflight
/// fails, and this is one input to that preflight — a harness-side fault discovered *inside*
/// an attempt (an evidence-store write failing) has to be able to stop the next launch too.
#[derive(Default)]
pub(crate) struct StopFlag(Mutex<Option<String>>);

impl StopFlag {
    /// Record `reason`, unless an earlier reason already tripped the flag (first one wins —
    /// it is the cause; later ones are usually its consequences).
    pub(crate) fn trip(&self, reason: String) {
        let mut slot = self.0.lock().expect("stop flag lock poisoned");
        if slot.is_none() {
            *slot = Some(reason);
        }
    }

    /// `Err(reason)` once tripped, for use directly as (part of) a pool preflight.
    pub(crate) fn check(&self) -> Result<(), String> {
        match &*self.0.lock().expect("stop flag lock poisoned") {
            Some(reason) => Err(reason.clone()),
            None => Ok(()),
        }
    }
}

/// The free-space floor, checked against every volume a sweep writes to: the one holding
/// the repo (results file), the one holding the evidence store, which `--evidence-dir` may
/// have pointed elsewhere, and — Stage 2 — the one holding Docker's image storage.
pub(crate) struct DiskGuard {
    paths: Vec<PathBuf>,
    min_free_bytes: u64,
}

impl DiskGuard {
    /// Guard the current directory (the repo root, where xtask runs) and `evidence_dir`.
    pub(crate) fn new(evidence_dir: &Path) -> Self {
        Self { paths: vec![PathBuf::from("."), evidence_dir.to_path_buf()], min_free_bytes: MIN_FREE_BYTES }
    }

    /// Also guard the volume holding `path` — Stage 2 adds the one holding Docker's image
    /// storage (`image_hygiene::docker_data_path`), which on Docker Desktop is not
    /// the repo's volume at all.
    pub(crate) fn guard(&mut self, path: PathBuf) {
        self.paths.push(path);
    }

    /// `Err(reason)` if any guarded volume is under the floor — or if its free space cannot
    /// be determined at all. Not knowing is treated as "too low": the point of the floor is
    /// to never fill the disk, and an unmeasurable disk is not evidence of a safe one.
    pub(crate) fn check(&self) -> Result<(), String> {
        for path in &self.paths {
            let free = free_bytes(path).map_err(|e| {
                format!(
                    "could not determine free disk space for {}: {e} — refusing to launch further \
                     attempts without it",
                    path.display()
                )
            })?;
            if free < self.min_free_bytes {
                return Err(format!(
                    "free disk space on the volume holding {} fell to {:.2} GiB, under the {:.0} GiB \
                     floor — stopped launching new attempts",
                    path.display(),
                    free as f64 / (1u64 << 30) as f64,
                    self.min_free_bytes as f64 / (1u64 << 30) as f64,
                ));
            }
        }
        Ok(())
    }
}

/// Free bytes available to an unprivileged user on the volume holding `path`, via POSIX
/// `df -P -k` (identical output contract on macOS and Linux). Shelling out rather than
/// calling `statvfs` keeps `unsafe` out of xtask for one number.
fn free_bytes(path: &Path) -> Result<u64, String> {
    let output = Command::new("df")
        .args(["-P", "-k", "--"])
        .arg(path)
        .output()
        .map_err(|e| format!("failed to run df: {e}"))?;
    if !output.status.success() {
        return Err(format!("df exited {}: {}", output.status, String::from_utf8_lossy(&output.stderr).trim()));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_df_available_kib(&stdout)
        .map(|kib| kib.saturating_mul(1024))
        .ok_or_else(|| format!("unrecognised df output: {}", stdout.trim()))
}

/// The `Available` column (in KiB) from `df -P -k` output.
///
/// Parsed by shape, not by column position: a filesystem name (`map auto_home` on macOS) or a
/// mount point can contain spaces, so the data row is scanned for the first run of
/// `<blocks> <used> <available> <capacity>%` after the filesystem name instead.
fn parse_df_available_kib(output: &str) -> Option<u64> {
    let row = output.lines().rev().find(|line| !line.trim().is_empty())?;
    let tokens: Vec<&str> = row.split_whitespace().collect();
    (1..tokens.len().saturating_sub(3)).find_map(|i| {
        let numeric = |t: &str| t.parse::<u64>().ok();
        let (_, _, available) = (numeric(tokens[i])?, numeric(tokens[i + 1])?, numeric(tokens[i + 2])?);
        tokens[i + 3].ends_with('%').then_some(available)
    })
}

/// What [`run_bounded`] got through.
pub(crate) struct PoolOutcome<T> {
    /// One result per attempted item, in *input* order — always items `0..completed.len()`.
    pub(crate) completed: Vec<T>,
    /// Why launching stopped before every item was attempted; `None` if none were skipped.
    pub(crate) stop_reason: Option<String>,
}

/// Run `work` over `items` with at most `jobs` attempts in flight, calling `preflight` before
/// each launch. The first `Err` from `preflight` stops all further launches; attempts already
/// running finish normally and their results are kept.
///
/// Two guarantees, both independent of thread scheduling:
///
/// - **Deterministic order.** Results are slotted by item index and returned in input order,
///   so a completion order scrambled by `jobs > 1` never reaches the output.
/// - **Prefix-shaped stopping.** The preflight check and the claim of the next index happen
///   under one lock, so an index is claimed only if its preflight passed, and every claimed
///   index runs. The attempted set is therefore always `0..n` — never a gappy subset a reader
///   would have to reconstruct.
///
/// `jobs` is clamped to at least 1. With `jobs == 1` this is strictly sequential.
pub(crate) fn run_bounded<I, T, P, W>(items: &[I], jobs: usize, preflight: P, work: W) -> PoolOutcome<T>
where
    I: Sync,
    T: Send,
    P: Fn() -> Result<(), String> + Sync,
    W: Fn(usize, &I) -> T + Sync,
{
    struct Claims {
        next: usize,
        stop_reason: Option<String>,
    }
    let claims = Mutex::new(Claims { next: 0, stop_reason: None });
    let slots: Vec<Mutex<Option<T>>> = items.iter().map(|_| Mutex::new(None)).collect();

    std::thread::scope(|scope| {
        for _ in 0..jobs.clamp(1, items.len().max(1)) {
            scope.spawn(|| {
                loop {
                    let index = {
                        let mut claims = claims.lock().expect("claims lock poisoned");
                        if claims.stop_reason.is_some() || claims.next >= items.len() {
                            return;
                        }
                        if let Err(reason) = preflight() {
                            claims.stop_reason = Some(reason);
                            return;
                        }
                        claims.next += 1;
                        claims.next - 1
                    };
                    let result = work(index, &items[index]);
                    *slots[index].lock().expect("slot lock poisoned") = Some(result);
                }
            });
        }
    });

    let claims = claims.into_inner().expect("claims lock poisoned");
    let completed = slots
        .into_iter()
        .take(claims.next)
        .map(|slot| {
            slot.into_inner()
                .expect("slot lock poisoned")
                .expect("every claimed index is filled before its worker exits")
        })
        .collect();
    PoolOutcome { completed, stop_reason: claims.stop_reason }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// Later items finish first; the output must still be in input order.
    #[test]
    fn output_order_is_input_order_regardless_of_completion_order() {
        let items: Vec<u64> = (0..12).collect();
        let in_flight = AtomicUsize::new(0);
        let max_in_flight = AtomicUsize::new(0);

        let outcome = run_bounded(&items, 4, || Ok(()), |index, item| {
            let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            max_in_flight.fetch_max(now, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(5 * (12 - *item)));
            in_flight.fetch_sub(1, Ordering::SeqCst);
            (index, item * 10)
        });

        assert_eq!(outcome.stop_reason, None);
        let expected: Vec<(usize, u64)> = (0..12).map(|i| (i as usize, i * 10)).collect();
        assert_eq!(outcome.completed, expected);
        let max = max_in_flight.load(Ordering::SeqCst);
        assert!(max <= 4, "at most `jobs` attempts may run at once, saw {max}");
        assert!(max >= 2, "jobs = 4 with long-running work must actually overlap, saw {max}");
    }

    #[test]
    fn one_job_is_strictly_sequential() {
        let items: Vec<u32> = (0..6).collect();
        let in_flight = AtomicUsize::new(0);
        let max_in_flight = AtomicUsize::new(0);
        let outcome = run_bounded(&items, 1, || Ok(()), |_, item| {
            let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            max_in_flight.fetch_max(now, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(2));
            in_flight.fetch_sub(1, Ordering::SeqCst);
            *item
        });
        assert_eq!(outcome.completed, items);
        assert_eq!(max_in_flight.load(Ordering::SeqCst), 1);
    }

    /// The disk-floor scenario: preflight starts failing mid-sweep. No new attempt may
    /// launch after that, in-flight ones finish and are kept, and what was attempted is a
    /// contiguous prefix of the input.
    #[test]
    fn a_failing_preflight_stops_launches_but_keeps_in_flight_results_as_a_prefix() {
        let items: Vec<u32> = (0..10).collect();
        let preflights = AtomicUsize::new(0);
        let launched = AtomicUsize::new(0);

        let outcome = run_bounded(
            &items,
            3,
            || {
                if preflights.fetch_add(1, Ordering::SeqCst) < 4 { Ok(()) } else { Err("disk low".to_string()) }
            },
            |_, item| {
                launched.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(20));
                *item
            },
        );

        assert_eq!(outcome.stop_reason.as_deref(), Some("disk low"));
        assert_eq!(outcome.completed, vec![0, 1, 2, 3], "exactly the four preflight-approved items, in order");
        assert_eq!(launched.load(Ordering::SeqCst), 4, "no attempt may launch after the preflight fails");
    }

    #[test]
    fn an_empty_input_runs_nothing() {
        let items: Vec<u8> = Vec::new();
        let outcome = run_bounded(&items, 8, || Err("never asked".to_string()), |_, item| *item);
        assert!(outcome.completed.is_empty());
        assert_eq!(outcome.stop_reason, None, "preflight is only consulted before an actual launch");
    }

    #[test]
    fn stop_flag_keeps_the_first_reason() {
        let flag = StopFlag::default();
        assert_eq!(flag.check(), Ok(()));
        flag.trip("first".into());
        flag.trip("second".into());
        assert_eq!(flag.check(), Err("first".to_string()));
    }

    #[test]
    fn df_output_parses_on_macos_and_linux() {
        let macos = "Filesystem   1024-blocks      Used Available Capacity  Mounted on\n\
                     /dev/disk3s5   482797652 424879896  10356976    98%    /System/Volumes/Data\n";
        assert_eq!(parse_df_available_kib(macos), Some(10_356_976));

        let linux = "Filesystem     1024-blocks    Used Available Capacity Mounted on\n\
                     /dev/vda1         20134592 5123456  15000000      26% /\n";
        assert_eq!(parse_df_available_kib(linux), Some(15_000_000));
    }

    #[test]
    fn df_output_with_spaces_in_names_still_parses() {
        let spaced = "Filesystem 1024-blocks Used Available Capacity Mounted on\n\
                      map auto_home 0 0 0 100% /System/Volumes/Data/home dir\n";
        assert_eq!(parse_df_available_kib(spaced), Some(0));
    }

    #[test]
    fn unrecognised_df_output_is_none_not_a_guess() {
        assert_eq!(parse_df_available_kib(""), None);
        assert_eq!(parse_df_available_kib("Filesystem 1024-blocks Used Available Capacity Mounted on\n"), None);
        assert_eq!(parse_df_available_kib("garbage line with no numbers"), None);
    }

    /// Against the real host: the floor is checked, and an absurd floor trips it with a
    /// reason rather than an error or a panic.
    #[test]
    fn disk_guard_trips_with_a_reason_when_the_floor_is_unreachable() {
        let guard = DiskGuard { paths: vec![PathBuf::from(".")], min_free_bytes: u64::MAX };
        let reason = guard.check().expect_err("no volume has u64::MAX bytes free");
        assert!(reason.contains("under the"), "{reason}");

        let permissive = DiskGuard { paths: vec![PathBuf::from(".")], min_free_bytes: 0 };
        assert_eq!(permissive.check(), Ok(()));
    }
}
