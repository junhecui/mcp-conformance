//! Argument parsing for the census sweeps and `census-rederive` (P0-10).
//!
//! Same discipline the original `[sample_size]` parser established: an argument that is
//! *present but wrong* is an error, never silently replaced by a default. A typo in
//! `--jobs 80` must not quietly run at 8, and a typo in `1000` must not quietly run 100.

use std::path::PathBuf;

/// Upper bound on Stage 2's `--jobs`. Each job is one capped container (256 MiB, 1 CPU) plus
/// a `docker run` CLI process; past a handful, a laptop-class Docker host starts timing
/// attempts out against the 45 s watchdog for reasons that have nothing to do with the server
/// under test, which would show up as spurious `io_or_timeout` failures.
pub const MAX_JOBS: usize = 8;

const DEFAULT_SAMPLE_SIZE: usize = 100;

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
}

/// Parse a sweep's arguments: `[sample_size] [--out PATH] [--evidence-dir PATH]`, plus
/// `[--jobs N]` when `allow_jobs` (Stage 2 only — Stage 1 contacts unrelated third-party
/// hosts and stays sequential by construction, per P0-07, so it does not even accept the flag).
pub fn parse_sweep_args(args: &[String], allow_jobs: bool) -> Result<SweepArgs, String> {
    let mut sample_size = None;
    let mut jobs = None;
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
            flag if flag.starts_with("--") => return Err(format!("unknown option `{flag}`")),
            positional => {
                let n = positional
                    .parse()
                    .map_err(|_| format!("invalid sample size `{positional}` — expected a positive integer"))?;
                set_once(&mut sample_size, "sample size", n)?;
            }
        }
    }

    Ok(SweepArgs {
        sample_size: sample_size.unwrap_or(DEFAULT_SAMPLE_SIZE),
        jobs: jobs.unwrap_or(1),
        out,
        evidence_dir,
    })
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

    #[test]
    fn defaults_match_the_historical_behaviour() {
        let parsed = parse_sweep_args(&[], true).expect("empty is valid");
        assert_eq!(parsed, SweepArgs { sample_size: 100, jobs: 1, out: None, evidence_dir: None });
    }

    #[test]
    fn every_option_parses_in_any_order() {
        let parsed = parse_sweep_args(
            &args(&["--jobs", "2", "3", "--out", "/tmp/x.json", "--evidence-dir", "/tmp/ev"]),
            true,
        )
        .expect("valid");
        assert_eq!(parsed.sample_size, 3);
        assert_eq!(parsed.jobs, 2);
        assert_eq!(parsed.out, Some(PathBuf::from("/tmp/x.json")));
        assert_eq!(parsed.evidence_dir, Some(PathBuf::from("/tmp/ev")));
    }

    #[test]
    fn present_but_wrong_is_an_error_never_a_default() {
        assert!(parse_sweep_args(&args(&["1OOO"]), false).is_err(), "typo'd sample size");
        assert!(parse_sweep_args(&args(&["--jobs", "0"]), true).is_err(), "zero jobs");
        assert!(parse_sweep_args(&args(&["--jobs", "9"]), true).is_err(), "over the cap");
        assert!(parse_sweep_args(&args(&["--jobs", "two"]), true).is_err(), "non-numeric jobs");
        assert!(parse_sweep_args(&args(&["--out"]), false).is_err(), "flag without a value");
        assert!(parse_sweep_args(&args(&["--out", "--jobs"]), true).is_err(), "flag as a value");
        assert!(parse_sweep_args(&args(&["5", "6"]), false).is_err(), "two sample sizes");
        assert!(parse_sweep_args(&args(&["--frobnicate"]), false).is_err(), "unknown flag");
    }

    /// Stage 1 must not even accept `--jobs`: it is sequential by construction, not by default.
    #[test]
    fn jobs_is_rejected_where_concurrency_is_not_allowed() {
        let err = parse_sweep_args(&args(&["--jobs", "2"]), false).expect_err("stage 1 has no --jobs");
        assert!(err.contains("unknown option"), "{err}");
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
