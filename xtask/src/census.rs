//! Stage 0 census (P0-08): fetch the full MCP Registry listing, classify each entry's
//! containability, and write the aggregate ratio — plus per-server detail, for the P0-06
//! hand-verification step later — to `results/census/`.
//!
//! Zero contact with any third-party MCP server. Only the registry itself is queried.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use datamodel::ContainabilityClass;
use intake::catalogue::{self, IngestOutcome};
use intake::classify::{self};
use intake::registry::RegistryClient;

struct ServerRecord {
    name: Option<String>,
    class: ContainabilityClass,
    reason: String,
}

fn class_str(class: ContainabilityClass) -> &'static str {
    match class {
        ContainabilityClass::A => "A",
        ContainabilityClass::B => "B",
        ContainabilityClass::Unclassifiable => "unclassifiable",
    }
}

/// Run the Stage 0 census against the live registry and write results to
/// `results/census/registry_class_ratio.json`.
pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("census: fetching the MCP Registry listing (registry.modelcontextprotocol.io)...");

    let client = RegistryClient::new();
    let mut servers = Vec::new();
    let mut pages = 0usize;

    client.fetch_all(100, Duration::from_millis(200), |page| {
        pages += 1;
        eprintln!("census: page {pages}, {} entries", page.entries_raw.len());
        for raw in &page.entries_raw {
            let outcome = catalogue::ingest(raw);
            let classification = classify::classify(&outcome);
            let name = match &outcome {
                IngestOutcome::Resolved(server) => Some(server.name.clone()),
                IngestOutcome::Unresolvable { name, .. } => name.clone(),
            };
            servers.push(ServerRecord { name, class: classification.class, reason: classification.reason });
        }
    })?;

    let total = servers.len();
    let class_a = servers.iter().filter(|s| s.class == ContainabilityClass::A).count();
    let class_b = servers.iter().filter(|s| s.class == ContainabilityClass::B).count();
    let unclassifiable =
        servers.iter().filter(|s| s.class == ContainabilityClass::Unclassifiable).count();

    eprintln!(
        "census: {total} servers — Class A: {class_a} ({:.1}%), Class B: {class_b} ({:.1}%), \
         unclassifiable: {unclassifiable} ({:.1}%)",
        pct(class_a, total),
        pct(class_b, total),
        pct(unclassifiable, total),
    );

    let generated_at_unix = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();

    let output = serde_json::json!({
        "generated_at_unix": generated_at_unix,
        "registry_base_url": intake::registry::DEFAULT_BASE_URL,
        "stage": "0 — registry metadata only, no contact with any third-party MCP server",
        "total_servers": total,
        "class_a": class_a,
        "class_b": class_b,
        "unclassifiable": unclassifiable,
        "servers": servers.iter().map(|s| serde_json::json!({
            "name": s.name,
            "class": class_str(s.class),
            "reason": s.reason,
        })).collect::<Vec<_>>(),
    });

    let out_dir = Path::new("results/census");
    std::fs::create_dir_all(out_dir)?;
    let out_path = out_dir.join("registry_class_ratio.json");
    std::fs::write(&out_path, serde_json::to_string_pretty(&output)?)?;
    eprintln!("census: wrote {}", out_path.display());

    Ok(())
}

fn pct(count: usize, total: usize) -> f64 {
    if total == 0 { 0.0 } else { 100.0 * count as f64 / total as f64 }
}
