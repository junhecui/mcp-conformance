//! Argument parsing for the census sweeps and `census-rederive` (P0-10).
//!
//! Same discipline the original `[sample_size]` parser established: an argument that is
//! *present but wrong* is an error, never silently replaced by a default. A typo in
//! `--jobs 80` must not quietly run at 8, and a typo in `1000` must not quietly run 100.
//!
//! P0-12 adds `--min-interval-ms` and `--max-requests-per-domain` to Stage 1 only. Both are
//! *bounded* rather than free: the pacing controls are on by default and these flags tune
//! them, so a value that would switch a control off, or set a cap under the per-discovery
//! reservation (which would refuse every host sampled), is rejected rather than honoured.

use std::path::PathBuf;
use std::time::Duration;

use discovery::PacePolicy;

/// Upper bound on Stage 2's `--jobs`. Each job is one capped container (256 MiB, 1 CPU) plus
/// a `docker run` CLI process; past a handful, a laptop-class Docker host starts timing
/// attempts out against the 45 s watchdog for reasons that have nothing to do with the server
/// under test, which would show up as spurious `io_or_timeout` failures.
pub const MAX_JOBS: usize = 8;

/// Floor on `--min-interval-ms` (P0-12). Not zero: the flag exists so an operator can make a
/// sweep *more* polite than the default or trade politeness for time on a small run, not so
/// the control can be switched off. A sweep that wants no spacing at all has to say
/// `--min-interval-ms 1`, which is a visible choice recorded in the results header, rather
/// than a `0` that reads like a default.
pub const MIN_INTERVAL_FLOOR_MS: u64 = 1;

/// Ceiling on `--min-interval-ms`: 60 s per request would make a 1,000-host sweep take days,
/// which is far more likely a typo than an intention.
pub const MAX_INTERVAL_MS: u64 = 60_000;

const DEFAULT_SAMPLE_SIZE: usize = 100;

/// Which sweep is being parsed. The two stages take disjoint option sets and that is
/// deliberate, not an oversight: Stage 1 is sequential by construction (P0-07) so it has no
/// `--jobs`, and Stage 2 speaks stdio to a local container so it has no HTTP pacing to
/// configure (P0-12). A flag that silently does nothing is worse than one that is rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Stage 1 — HTTP discovery against live Class B servers.
    ClassB,
    /// Stage 2 — containerized stdio discovery of Class A servers.
    ClassA,
}

/// Options shared by `census-stage1` and `census-stage2-class-a`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SweepArgs {
    /// How many candidates to sample (by stable hash). Default 100.
    pub sample_size: usize,
    /// Concurrent attempts. Always 1 for Stage 1; `--jobs` (Stage 2 only) sets it, max [`MAX_JOBS`].
    pub jobs: usize,
    /// Where to write the results JSON; `None` keeps the stage's historical default path.
    pub out: Option<PathBuf>,
    /// Evidence store root; `None` falls back to the environment, then the default.
    pub evidence_dir: Option<PathBuf>,
    /// P0-12 pacing policy. Meaningful for [`Stage::ClassB`] only; left at
    /// [`PacePolicy::CENSUS_DEFAULT`] for Stage 2, which never consults it.
    pub pace: PacePolicy,
    /// Seed for the sample selection and admission order; `None` draws a fresh one per run
    /// and publishes it in the results header. Pass a prior run's published seed to
    /// reproduce that sample exactly.
    pub sample_seed: Option<u64>,
}

/// Parse a sweep's arguments: `[sample_size] [--out PATH] [--evidence-dir PATH]`, plus
/// `[--jobs N]` for [`Stage::ClassA`] and
/// `[--min-interval-ms N] [--max-requests-per-domain N]` for [`Stage::ClassB`].
pub fn parse_sweep_args(args: &[String], stage: Stage) -> Result<SweepArgs, String> {
    let allow_jobs = stage == Stage::ClassA;
    let allow_pacing = stage == Stage::ClassB;
    let mut sample_size = None;
    let mut jobs = None;
    let mut out = None;
    let mut evidence_dir = None;
    let mut min_interval_ms = None;
    let mut max_requests_per_domain = None;
    let mut sample_seed = None;

    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--out" => set_once(&mut out, "--out", PathBuf::from(flag_value(&mut iter, "--out")?))?,
            "--evidence-dir" => set_once(
                &mut evidence_dir,
                "--evidence-dir",
                PathBuf::from(flag_value(&mut iter, "--evidence-dir")?),
            )?,
            "--jobs" if allow_jobs => {
                let raw = flag_value(&mut iter, "--jobs")?;
                let n: usize = raw
                    .parse()
                    .map_err(|_| format!("invalid --jobs `{raw}` — expected an integer from 1 to {MAX_JOBS}"))?;
                if !(1..=MAX_JOBS).contains(&n) {
                    return Err(format!("--jobs {n} is out of range — expected 1 to {MAX_JOBS}"));
                }
                set_once(&mut jobs, "--jobs", n)?;
            }
            "--min-interval-ms" if allow_pacing => {
                let n = bounded_number(
                    flag_value(&mut iter, "--min-interval-ms")?,
                    "--min-interval-ms",
                    MIN_INTERVAL_FLOOR_MS,
                    MAX_INTERVAL_MS,
                )?;
                set_once(&mut min_interval_ms, "--min-interval-ms", n)?;
            }
            "--max-requests-per-domain" if allow_pacing => {
                // The floor is `MAX_REQUESTS_PER_DISCOVERY`: below it `Pacer::admit` can
                // never succeed, so the sweep would refuse every host it sampled and
                // produce a results file of nothing but budget refusals.
                let n = bounded_number(
                    flag_value(&mut iter, "--max-requests-per-domain")?,
                    "--max-requests-per-domain",
                    discovery::MAX_REQUESTS_PER_DISCOVERY as u64,
                    u64::from(u32::MAX),
                )?;
                set_once(&mut max_requests_per_domain, "--max-requests-per-domain", n as usize)?;
            }
            "--sample-seed" => {
                // Accepts the `0x…` form the header publishes, and plain decimal. There is
                // nothing to range-check: every `u64` is a valid seed, so an operator can
                // pick a different sample but never a less valid one.
                let raw = flag_value(&mut iter, "--sample-seed")?;
                let parsed = raw
                    .strip_prefix("0x")
                    .or_else(|| raw.strip_prefix("0X"))
                    .map_or_else(|| raw.parse::<u64>(), |hex| u64::from_str_radix(hex, 16))
                    .map_err(|_| {
                        format!(
                            "invalid --sample-seed `{raw}` — expected a u64, decimal or 0x-hex"
                        )
                    })?;
                set_once(&mut sample_seed, "--sample-seed", parsed)?;
            }
            flag if flag.starts_with("--") => return Err(format!("unknown option `{flag}`")),
            positional => {
                let n = positional
                    .parse()
                    .map_err(|_| format!("invalid sample size `{positional}` — expected a positive integer"))?;
                set_once(&mut sample_size, "sample size", n)?;
            }
        }
    }

    let default = PacePolicy::CENSUS_DEFAULT;
    Ok(SweepArgs {
        sample_size: sample_size.unwrap_or(DEFAULT_SAMPLE_SIZE),
        jobs: jobs.unwrap_or(1),
        out,
        evidence_dir,
        pace: PacePolicy {
            min_interval: min_interval_ms.map_or(default.min_interval, Duration::from_millis),
            max_requests_per_registrable_domain: max_requests_per_domain
                .unwrap_or(default.max_requests_per_registrable_domain),
        },
        sample_seed,
    })
}

/// A flag value parsed as an integer and required to be in `[min, max]`. Present-but-wrong
/// is an error, never a silent default — the discipline this module's doc comment states.
fn bounded_number(raw: &str, flag: &str, min: u64, max: u64) -> Result<u64, String> {
    let n: u64 = raw
        .parse()
        .map_err(|_| format!("invalid {flag} `{raw}` — expected an integer from {min} to {max}"))?;
    if !(min..=max).contains(&n) {
        return Err(format!("{flag} {n} is out of range — expected {min} to {max}"));
    }
    Ok(n)
}

/// Options for `census-rederive`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RederiveArgs {
    /// The results JSON to re-derive.
    pub input: PathBuf,
    /// Evidence store root; `None` uses the `evidence_store` the results file recorded.
    pub evidence_dir: Option<PathBuf>,
    /// Where to write the regenerated JSON, if anywhere.
    pub out: Option<PathBuf>,
}

/// Parse `census-rederive <results.json> [--evidence-dir PATH] [--out PATH]`.
pub fn parse_rederive_args(args: &[String]) -> Result<RederiveArgs, String> {
    let mut input = None;
    let mut out = None;
    let mut evidence_dir = None;

    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--out" => set_once(&mut out, "--out", PathBuf::from(flag_value(&mut iter, "--out")?))?,
            "--evidence-dir" => set_once(
                &mut evidence_dir,
                "--evidence-dir",
                PathBuf::from(flag_value(&mut iter, "--evidence-dir")?),
            )?,
            flag if flag.starts_with("--") => return Err(format!("unknown option `{flag}`")),
            positional => set_once(&mut input, "results file", PathBuf::from(positional))?,
        }
    }

    Ok(RederiveArgs {
        input: input.ok_or("missing the results file to re-derive")?,
        evidence_dir,
        out,
    })
}

fn flag_value<'a>(iter: &mut impl Iterator<Item = &'a String>, flag: &str) -> Result<&'a str, String> {
    match iter.next() {
        Some(value) if !value.starts_with("--") => Ok(value),
        _ => Err(format!("{flag} needs a value")),
    }
}

fn set_once<T>(slot: &mut Option<T>, what: &str, value: T) -> Result<(), String> {
    if slot.is_some() {
        return Err(format!("{what} given more than once"));
    }
    *slot = Some(value);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    /// `--sample-seed` takes the exact `0x…` form the results header publishes, so
    /// reproducing a sweep is copy-and-paste rather than a base conversion, and decimal too.
    #[test]
    fn sample_seed_accepts_the_published_hex_form_and_decimal() {
        let hex = parse_sweep_args(&args(&["--sample-seed", "0x123456789abcdef0"]), Stage::ClassB)
            .expect("hex is valid");
        assert_eq!(hex.sample_seed, Some(0x1234_5678_9abc_def0));
        let dec =
            parse_sweep_args(&args(&["--sample-seed", "42"]), Stage::ClassB).expect("decimal");
        assert_eq!(dec.sample_seed, Some(42));
        // u64::MAX round-trips: the whole range is valid, which is why nothing is bounded.
        let max = parse_sweep_args(&args(&["--sample-seed", "0xffffffffffffffff"]), Stage::ClassB)
            .expect("u64::MAX is a seed");
        assert_eq!(max.sample_seed, Some(u64::MAX));
        assert!(
            parse_sweep_args(&args(&["--sample-seed", "nonsense"]), Stage::ClassB).is_err(),
            "a non-numeric seed must be rejected, not silently defaulted"
        );
        assert!(
            parse_sweep_args(&args(&["--sample-seed", "0x1_0000000000000000"]), Stage::ClassB)
                .is_err(),
            "a seed that does not fit a u64 must be rejected"
        );
    }

    #[test]
    fn defaults_match_the_historical_behaviour() {
        let parsed = parse_sweep_args(&[], Stage::ClassA).expect("empty is valid");
        assert_eq!(
            parsed,
            SweepArgs {
                sample_size: 100,
                jobs: 1,
                out: None,
                evidence_dir: None,
                pace: PacePolicy::CENSUS_DEFAULT,
                sample_seed: None,
            }
        );
    }

    #[test]
    fn every_option_parses_in_any_order() {
        let parsed = parse_sweep_args(
            &args(&["--jobs", "2", "3", "--out", "/tmp/x.json", "--evidence-dir", "/tmp/ev"]),
            Stage::ClassA,
        )
        .expect("valid");
        assert_eq!(parsed.sample_size, 3);
        assert_eq!(parsed.jobs, 2);
        assert_eq!(parsed.out, Some(PathBuf::from("/tmp/x.json")));
        assert_eq!(parsed.evidence_dir, Some(PathBuf::from("/tmp/ev")));

        let paced = parse_sweep_args(
            &args(&["--min-interval-ms", "250", "7", "--max-requests-per-domain", "40"]),
            Stage::ClassB,
        )
        .expect("valid");
        assert_eq!(paced.sample_size, 7);
        assert_eq!(paced.pace.min_interval, Duration::from_millis(250));
        assert_eq!(paced.pace.max_requests_per_registrable_domain, 40);
    }

    /// Omitting the pacing flags must give the documented default, not "no pacing" — the
    /// controls are on by default and the flags only tune them.
    #[test]
    fn pacing_defaults_are_the_census_policy_not_an_absence_of_one() {
        let parsed = parse_sweep_args(&[], Stage::ClassB).expect("empty is valid");
        assert_eq!(parsed.pace, PacePolicy::CENSUS_DEFAULT);
        assert_eq!(parsed.pace.min_interval, Duration::from_millis(500));
        assert_eq!(parsed.pace.max_requests_per_registrable_domain, 120);
    }

    #[test]
    fn present_but_wrong_is_an_error_never_a_default() {
        assert!(parse_sweep_args(&args(&["1OOO"]), Stage::ClassB).is_err(), "typo'd sample size");
        assert!(parse_sweep_args(&args(&["--jobs", "0"]), Stage::ClassA).is_err(), "zero jobs");
        assert!(parse_sweep_args(&args(&["--jobs", "9"]), Stage::ClassA).is_err(), "over the cap");
        assert!(parse_sweep_args(&args(&["--jobs", "two"]), Stage::ClassA).is_err(), "non-numeric");
        assert!(parse_sweep_args(&args(&["--out"]), Stage::ClassB).is_err(), "flag w/o a value");
        assert!(
            parse_sweep_args(&args(&["--out", "--jobs"]), Stage::ClassA).is_err(),
            "flag as a value"
        );
        assert!(parse_sweep_args(&args(&["5", "6"]), Stage::ClassB).is_err(), "two sample sizes");
        assert!(parse_sweep_args(&args(&["--frobnicate"]), Stage::ClassB).is_err(), "unknown flag");
    }

    /// The pacing controls cannot be switched off by a value that reads like a default, and
    /// they cannot be set to a value that would refuse every host in the sample.
    #[test]
    fn the_pacing_controls_cannot_be_disabled_by_an_out_of_range_value() {
        assert!(
            parse_sweep_args(&args(&["--min-interval-ms", "0"]), Stage::ClassB).is_err(),
            "zero spacing must be an explicit 1 ms, not a 0 that looks like a default"
        );
        assert!(
            parse_sweep_args(&args(&["--min-interval-ms", "600000"]), Stage::ClassB).is_err(),
            "ten minutes per request is a typo, not an intention"
        );
        let err = parse_sweep_args(&args(&["--max-requests-per-domain", "4"]), Stage::ClassB)
            .expect_err("a cap under the per-discovery reservation refuses every host");
        assert!(err.contains("out of range"), "{err}");
        assert!(
            parse_sweep_args(&args(&["--max-requests-per-domain", "5"]), Stage::ClassB).is_ok(),
            "exactly one reservation is the floor, and is allowed"
        );
    }

    /// Each stage rejects the other's flags rather than accepting one that would do nothing.
    #[test]
    fn each_stage_rejects_the_other_stages_flags() {
        let err = parse_sweep_args(&args(&["--jobs", "2"]), Stage::ClassB)
            .expect_err("stage 1 is sequential by construction and has no --jobs");
        assert!(err.contains("unknown option"), "{err}");

        for flag in ["--min-interval-ms", "--max-requests-per-domain"] {
            let err = parse_sweep_args(&args(&[flag, "100"]), Stage::ClassA)
                .map(|_| ())
                .expect_err("Stage 2 sends no HTTP request, so it must reject a pacing flag");
            assert!(err.contains("unknown option"), "{flag}: {err}");
        }
    }

    #[test]
    fn rederive_needs_exactly_one_input() {
        let parsed = parse_rederive_args(&args(&["r.json", "--evidence-dir", "ev"])).expect("valid");
        assert_eq!(parsed.input, PathBuf::from("r.json"));
        assert_eq!(parsed.evidence_dir, Some(PathBuf::from("ev")));
        assert!(parse_rederive_args(&[]).is_err());
        assert!(parse_rederive_args(&args(&["a.json", "b.json"])).is_err());
    }
}
