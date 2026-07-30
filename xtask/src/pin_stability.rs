//! Pin-stability check (P0-06's remaining "re-run discovery... confirm pins are stable"
//! item): re-run discovery against the same servers twice, back to back, and confirm each
//! server's metadata pin is identical both times.
//!
//! Reads the successful-server URL list from a prior Stage 1 output rather than
//! re-deriving it from the registry, so this checks exactly the servers that run actually
//! succeeded against — not a fresh, possibly-different sample.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use discovery::DiscoveryClient;

struct Candidate {
    name: String,
    url: String,
}

fn discover_and_pin(url: &str) -> Result<(String, usize), String> {
    let mut client = DiscoveryClient::http(url.to_string());
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
fn check_one(candidate: &Candidate) -> serde_json::Value {
    let first = discover_and_pin(&candidate.url);
    let second = discover_and_pin(&candidate.url);

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
    });
    if let (Some(record_obj), Some(extra_obj)) = (record.as_object_mut(), extra.as_object()) {
        record_obj.extend(extra_obj.clone());
    }
    record
}

/// Re-discover every server a prior Stage 1 run marked successful, twice each, and check
/// whether the metadata pin is stable across the two discoveries. Writes
/// `results/census/pin_stability.json`.
///
/// # Errors
/// Reading or parsing `input_path` failing, or writing the result file failing. A single
/// candidate's own discovery failing is not an error here — it becomes a recorded
/// `first_discovery_failed`/`second_discovery_failed` result instead.
pub fn run(input_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let candidates = read_successful_servers(input_path)?;
    eprintln!(
        "census-pin-stability: re-discovering {} servers from {input_path}, twice each...",
        candidates.len()
    );

    let mut stable = 0usize;
    let mut unstable = 0usize;
    let mut failed = 0usize;
    let mut server_results = Vec::with_capacity(candidates.len());

    for (i, candidate) in candidates.iter().enumerate() {
        eprintln!("census-pin-stability: [{}/{}] {}", i + 1, candidates.len(), candidate.name);
        let record = check_one(candidate);
        match record["result"].as_str() {
            Some("stable") => stable += 1,
            Some("unstable") => unstable += 1,
            _ => failed += 1,
        }
        server_results.push(record);
    }

    eprintln!(
        "census-pin-stability: {} checked — {stable} stable, {unstable} unstable, {failed} failed to re-discover",
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
        "servers": server_results,
    });

    let out_dir = Path::new("results/census");
    std::fs::create_dir_all(out_dir)?;
    let out_path = out_dir.join("pin_stability.json");
    std::fs::write(&out_path, serde_json::to_string_pretty(&output)?)?;
    eprintln!("census-pin-stability: wrote {}", out_path.display());

    Ok(())
}
