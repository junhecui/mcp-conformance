//! Pin-stability check (P0-06's remaining "re-run discovery... confirm pins are stable"
//! item): re-run discovery against the same servers twice, back to back, and confirm each
//! server's metadata pin is identical both times.
//!
//! Reads the successful-server URL list from a prior Stage 1 output rather than
//! re-deriving it from the registry, so this checks exactly the servers that run actually
//! succeeded against — not a fresh, possibly-different sample.
//!
//! P0-12: **paced, and it needed it more than Stage 1 did.** This check deliberately runs
//! two full discovery sequences back to back against each host — that repetition is the
//! measurement — and it runs them against exactly the subset of hosts a prior sweep found
//! *responsive*, i.e. the population most likely to have a rate limiter in front of it and
//! to actually answer fast enough to trip one. Unpaced that was up to 8 requests to one host
//! at roughly 4 per second, above the ~3-in-one-second rate that cost this project an O-01
//! transcript to a 429. One permit covers both sequences, reserved at
//! [`PIN_CHECK_RESERVATION`], so a server record and a permit stay one-to-one the way the
//! Stage 1 accounting assumes.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use discovery::{DiscoveryClient, MAX_REQUESTS_PER_DISCOVERY, PacePolicy, Pacer, Permit};

use crate::census_report;

/// Per-request timeout, matching `census_stage1`'s: these are the same hosts, and a handful
/// of newly-unresponsive ones must not dominate the run.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(12);

/// Requests one candidate's pin check may send: two full discovery sequences through one
/// client, so twice [`MAX_REQUESTS_PER_DISCOVERY`].
const PIN_CHECK_RESERVATION: usize = 2 * MAX_REQUESTS_PER_DISCOVERY;

/// One discovery-and-pin attempt: the server pin and its tool count, or why it failed.
type PinAttempt = Result<(String, usize), String>;

struct Candidate {
    name: String,
    url: String,
}

/// Both discoveries for one candidate, through **one** paced client.
///
/// One client rather than two because `DiscoveryClient::discover` is re-entrant over HTTP
/// (it re-probes and re-handshakes from scratch each call), and because a permit is owned by
/// the client that spends it — so one permit covering both sequences is the only shape that
/// keeps one permit to one server record. The two sequences are still independent in the
/// only sense this check cares about: each performs its own full handshake and its own
/// `tools/list`.
fn discover_and_pin_twice(url: &str, permit: Permit) -> (PinAttempt, PinAttempt) {
    let mut client = DiscoveryClient::http_paced(url.to_string(), REQUEST_TIMEOUT, permit);
    let first = pin_once(&mut client);
    let second = pin_once(&mut client);
    (first, second)
}

fn pin_once(client: &mut DiscoveryClient) -> PinAttempt {
    let discovery = client.discover().map_err(|e| e.to_string())?;
    let (tool_pins, server_pin) =
        discovery::pin_tools(&discovery.tools_list_raw).map_err(|e| e.to_string())?;
    Ok((server_pin.to_string(), tool_pins.len()))
}

/// Read the successful-server list out of a prior Stage 1 JSON output (as produced by
/// `xtask::census_stage1::run`).
fn read_successful_servers(input_path: &str) -> Result<Vec<Candidate>, Box<dyn std::error::Error>> {
    let text = std::fs::read_to_string(input_path)?;
    let parsed: serde_json::Value = serde_json::from_str(&text)?;
    let servers = parsed["servers"]
        .as_array()
        .ok_or("input file has no top-level \"servers\" array")?;

    Ok(servers
        .iter()
        .filter(|s| s["outcome"] == "success")
        .filter_map(|s| {
            let name = s["name"].as_str()?.to_string();
            let url = s["url"].as_str()?.to_string();
            Some(Candidate { name, url })
        })
        .collect())
}

/// One server's two discovery attempts, checked against each other.
///
/// The budget is consulted *before* a client exists, so a host whose registrable domain is
/// spent costs no connection at all — and is **recorded** as a refusal rather than dropped,
/// the same discipline ADR-014 §4 sets for Stage 1: a candidate absent from a results file
/// is indistinguishable from one that was never sampled.
fn check_one(candidate: &Candidate, pacer: &Arc<Pacer>) -> serde_json::Value {
    let registrable_domain = discovery::registrable_domain_of_endpoint(&candidate.url);
    let permit = match pacer.admit_with_reservation(&candidate.url, PIN_CHECK_RESERVATION) {
        Ok(permit) => permit,
        Err(denied) => {
            eprintln!("census-pin-stability:   refused: {denied}");
            return serde_json::json!({
                "name": candidate.name,
                "url": candidate.url,
                "result": "refused_by_domain_budget",
                "detail": denied.to_string(),
                "pacing": census_report::pacing_record(&registrable_domain, 0, Duration::ZERO),
            });
        }
    };
    // The permit moves into the client that spends it; `stats` is the handle this
    // candidate's own counters are read back through afterwards.
    let stats = permit.stats();
    let (first, second) = discover_and_pin_twice(&candidate.url, permit);

    let (result, extra) = match (first, second) {
        (Ok((pin1, count1)), Ok((pin2, _))) if pin1 == pin2 => {
            ("stable", serde_json::json!({ "server_pin": pin1, "tool_count": count1 }))
        }
        (Ok((pin1, _)), Ok((pin2, _))) => {
            ("unstable", serde_json::json!({ "server_pin_first": pin1, "server_pin_second": pin2 }))
        }
        (Err(detail), _) => ("first_discovery_failed", serde_json::json!({ "detail": detail })),
        (_, Err(detail)) => ("second_discovery_failed", serde_json::json!({ "detail": detail })),
    };

    let mut record = serde_json::json!({
        "name": candidate.name,
        "url": candidate.url,
        "result": result,
        // Same per-server shape Stage 1 publishes, so this file's politeness is checkable
        // against its own header rather than asserted.
        "pacing": census_report::pacing_record(
            &registrable_domain,
            stats.requests_sent(),
            stats.waited(),
        ),
    });
    if let (Some(record_obj), Some(extra_obj)) = (record.as_object_mut(), extra.as_object()) {
        record_obj.extend(extra_obj.clone());
    }
    record
}

/// Re-discover every server a prior Stage 1 run marked successful, twice each, and check
/// whether the metadata pin is stable across the two discoveries. Writes
/// `results/census/pin_stability.json`.
pub fn run(input_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let candidates = read_successful_servers(input_path)?;
    // The same policy and the same run-wide per-domain budget Stage 1 uses, so the two
    // sweeps' conduct is comparable and a domain cannot be hit hard by one just because the
    // other was polite to it. Not operator-configurable: this command takes one positional
    // argument, and a politeness control that applies only when someone remembers a flag is
    // not a control.
    let pacer = Arc::new(Pacer::new(PacePolicy::CENSUS_DEFAULT));
    eprintln!(
        "census-pin-stability: re-discovering {} servers from {input_path}, twice each; \
         pacing {} ms between requests to one registrable domain, cap {} requests/domain",
        candidates.len(),
        PacePolicy::CENSUS_DEFAULT.min_interval.as_millis(),
        PacePolicy::CENSUS_DEFAULT.max_requests_per_registrable_domain,
    );

    let mut stable = 0usize;
    let mut unstable = 0usize;
    let mut failed = 0usize;
    let mut refused = 0usize;
    let mut server_results = Vec::with_capacity(candidates.len());

    for (i, candidate) in candidates.iter().enumerate() {
        eprintln!("census-pin-stability: [{}/{}] {}", i + 1, candidates.len(), candidate.name);
        let record = check_one(candidate, &pacer);
        match record["result"].as_str() {
            Some("stable") => stable += 1,
            Some("unstable") => unstable += 1,
            Some("refused_by_domain_budget") => refused += 1,
            _ => failed += 1,
        }
        server_results.push(record);
    }

    eprintln!(
        "census-pin-stability: {} checked — {stable} stable, {unstable} unstable, \
         {failed} failed to re-discover, {refused} refused by the domain budget",
        candidates.len()
    );

    let generated_at_unix = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let output = serde_json::json!({
        "generated_at_unix": generated_at_unix,
        "input_file": input_path,
        "checked": candidates.len(),
        "stable": stable,
        "unstable": unstable,
        "failed_to_rediscover": failed,
        "refused_by_domain_budget": refused,
        // Policy *and* accounting, built by the same function Stage 1 uses, so this file's
        // politeness claim can be checked rather than taken on trust.
        "pacing": census_report::pacing_header(&pacer.accounting()),
        "servers": server_results,
    });

    // Checkable, and checked: one permit per server record (see `check_one`), so the
    // header's totals must be exactly the per-server sums. Loud, not fatal — the data this
    // run collected is worth keeping either way, and `census-rederive` is the path that
    // fails hard.
    if let Err(problems) = census_report::verify_pacing_self_consistent(&output) {
        for problem in problems {
            eprintln!("census-pin-stability: PACING ACCOUNTING INCONSISTENT — {problem}");
        }
    }

    let out_dir = Path::new("results/census");
    std::fs::create_dir_all(out_dir)?;
    let out_path = out_dir.join("pin_stability.json");
    std::fs::write(&out_path, serde_json::to_string_pretty(&output)?)?;
    eprintln!("census-pin-stability: wrote {}", out_path.display());

    Ok(())
}
