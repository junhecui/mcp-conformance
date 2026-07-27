//! Debug utility: discover a server and print each tool's name and raw `annotations`
//! object. Exists for hand-verification passes (P0-06's "hand-verify the taxonomy on ≥20
//! tools" item) — a human needs to see the actual JSON next to the computed
//! [`census::coverage::Coverage`] classification to judge whether the taxonomy is right,
//! not just trust the aggregate counts.

use discovery::DiscoveryClient;

/// Discover `url` and print each tool's name, its coverage classification per annotation,
/// and the raw `annotations` object it came from.
pub fn run(url: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mut client = DiscoveryClient::http(url.to_string());
    let discovery = client.discover()?;

    let coverage = census::coverage::tool_coverage(&discovery.tools_list_raw)?;
    let raw: serde_json::Value = serde_json::from_slice(&discovery.tools_list_raw)?;
    let tools = raw["result"]["tools"].as_array().cloned().unwrap_or_default();

    println!("=== {url} — {} tools ===", tools.len());
    for (tool, cov) in tools.iter().zip(coverage.iter()) {
        println!(
            "\n- {} (readOnly={:?} destructive={:?} idempotent={:?} openWorld={:?})",
            cov.tool_name, cov.read_only_hint, cov.destructive_hint, cov.idempotent_hint, cov.open_world_hint
        );
        println!("  annotations: {}", tool.get("annotations").cloned().unwrap_or(serde_json::Value::Null));
    }

    Ok(())
}
