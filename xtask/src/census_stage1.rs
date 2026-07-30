//! Stage 1 census: run `initialize` + `tools/list` against a sample of live Class B
//! (remote-endpoint) servers from the registry, and aggregate annotation coverage.
//!
//! No local execution — Class B servers are, by definition, remote HTTP endpoints. This
//! connects to each one exactly the way any MCP client would on first use, and no
//! differently. Sequential requests, not concurrent: these are unrelated third-party hosts
//! and there is no reason to hit them in a burst.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use census::coverage;
use datamodel::ContainabilityClass;
use discovery::{DiscoveryClient, DiscoveryError};
use intake::catalogue::{self, IngestOutcome, ResolvedTarget};
use intake::classify;
use intake::registry::RegistryClient;

/// `pub(crate)`, not private: `probe_stage1` (Track B) reuses this exact sampling logic
/// (the candidate shape, the classification-driven filter, and — via [`stable_hash`] — the
/// unbiased selection) rather than re-deriving a second Class B sample independently. Two
/// scripts scanning the same registry with two different sampling methodologies would make
/// their results incomparable for no reason.
pub(crate) struct Candidate {
    pub(crate) name: String,
    pub(crate) url: String,
    pub(crate) transport_type: String,
}

/// If this ingest outcome classifies as Class B, its name and first declared remote
/// endpoint. `Unresolvable` entries can never be Class B (`classify()` maps them to
/// `Unclassifiable`), so this returns `None` for those without inspecting them further.
pub(crate) fn class_b_candidate(outcome: &IngestOutcome) -> Option<Candidate> {
    if classify::classify(outcome).class != ContainabilityClass::B {
        return None;
    }
    let IngestOutcome::Resolved(server) = outcome else {
        return None;
    };
    server.targets.iter().find_map(|t| match t {
        ResolvedTarget::Endpoint { transport_type, url } => Some(Candidate {
            name: server.name.clone(),
            url: url.clone(),
            transport_type: transport_type.clone(),
        }),
        ResolvedTarget::Package { .. } => None,
    })
}

enum Attempt {
    Success { tool_count: usize, tools: Vec<coverage::ToolCoverage> },
    Failed { category: &'static str, detail: String },
}

fn attempt(url: &str) -> Attempt {
    // Shorter than the 30s default: at sample sizes in the hundreds or thousands, a
    // handful of genuinely unresponsive hosts at 30s each would dominate total run time.
    // Servers that are actually going to answer do so in low seconds at most, per the 100
    // -server sample this was tuned against.
    let mut client = DiscoveryClient::http_with_timeout(url.to_string(), std::time::Duration::from_secs(12));
    match client.discover() {
        Ok(discovery) => match coverage::tool_coverage(&discovery.tools_list_raw) {
            Ok(tools) => Attempt::Success { tool_count: tools.len(), tools },
            Err(e) => Attempt::Failed { category: "coverage_extraction", detail: e.to_string() },
        },
        Err(DiscoveryError::Transport(msg)) => Attempt::Failed { category: "transport", detail: msg },
        Err(DiscoveryError::Io(e)) => Attempt::Failed { category: "io", detail: e.to_string() },
        Err(DiscoveryError::Protocol(msg)) => Attempt::Failed { category: "protocol", detail: msg },
        Err(DiscoveryError::ServerError { code, message }) => {
            Attempt::Failed { category: "server_error", detail: format!("{code}: {message}") }
        }
    }
}

/// Deterministically hash `name` into a `u64` — `DefaultHasher`'s keys are fixed (unlike
/// `HashMap`'s `RandomState`, which is randomised per-process to resist `HashDoS`), so this
/// is stable across runs and processes, not just within one.
pub(crate) fn stable_hash(name: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    name.hash(&mut hasher);
    hasher.finish()
}

/// Run the Stage 1 census: sample `sample_size` Class B servers from the live registry and
/// attempt discovery against each.
///
/// The registry returns entries in what looks like roughly alphabetical order by name
/// (every sample drawn from "the first N" clustered under `a*` prefixes) — taking the first
/// N candidates would bias the sample toward whatever namespace happens to sort first, not
/// give a representative cross-section. Instead this scans every Class B candidate in the
/// registry, then selects `sample_size` of them by a deterministic hash of their name. Hash
/// -order is not alphabetical, unbiased with respect to namespace, and — unlike a
/// randomised selection — reproducible: the same registry snapshot always yields the same
/// sample, which matters for comparing across runs.
pub fn run(sample_size: usize) -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("census-stage1: fetching the registry to find all Class B candidates...");

    let registry = RegistryClient::new();
    let mut all_class_b = Vec::new();
    registry.fetch_all(100, std::time::Duration::from_millis(200), |page| {
        for raw in &page.entries_raw {
            let outcome = catalogue::ingest(raw);
            if let Some(candidate) = class_b_candidate(&outcome) {
                all_class_b.push(candidate);
            }
        }
        true // scan the whole registry — early-stopping here is exactly the bias to avoid
    })?;

    eprintln!(
        "census-stage1: {} Class B candidates found; selecting {sample_size} by stable hash...",
        all_class_b.len()
    );
    all_class_b.sort_by_key(|c| stable_hash(&c.name));
    let candidates: Vec<Candidate> = all_class_b.into_iter().take(sample_size).collect();

    eprintln!("census-stage1: attempting discovery against {} Class B servers...", candidates.len());

    let mut all_tools: Vec<coverage::ToolCoverage> = Vec::new();
    let mut server_results = Vec::new();
    let mut succeeded = 0usize;
    let mut failure_categories: std::collections::BTreeMap<&'static str, usize> = std::collections::BTreeMap::new();

    for (i, candidate) in candidates.iter().enumerate() {
        eprintln!(
            "census-stage1: [{}/{}] {} ({})",
            i + 1,
            candidates.len(),
            candidate.name,
            candidate.url
        );
        match attempt(&candidate.url) {
            Attempt::Success { tool_count, tools } => {
                succeeded += 1;
                all_tools.extend(tools);
                server_results.push(serde_json::json!({
                    "name": candidate.name,
                    "url": candidate.url,
                    "transport_type": candidate.transport_type,
                    "outcome": "success",
                    "tool_count": tool_count,
                }));
            }
            Attempt::Failed { category, detail } => {
                *failure_categories.entry(category).or_insert(0) += 1;
                server_results.push(serde_json::json!({
                    "name": candidate.name,
                    "url": candidate.url,
                    "transport_type": candidate.transport_type,
                    "outcome": "failed",
                    "failure_category": category,
                    "failure_detail": detail,
                }));
            }
        }
    }

    let attempted = candidates.len();
    let failed = attempted - succeeded;
    eprintln!(
        "census-stage1: {attempted} attempted, {succeeded} succeeded ({:.1}%), {failed} failed",
        pct(succeeded, attempted)
    );
    for (category, count) in &failure_categories {
        eprintln!("census-stage1:   {category}: {count}");
    }

    let tally = coverage::tally(&all_tools);
    let generated_at_unix = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();

    let output = serde_json::json!({
        "generated_at_unix": generated_at_unix,
        "stage": "1 — initialize+tools/list against live Class B servers",
        "sample_size_requested": sample_size,
        "attempted": attempted,
        "succeeded": succeeded,
        "failed": failed,
        "failure_categories": failure_categories,
        "tools_discovered": all_tools.len(),
        "annotation_tally": tally_json(&tally),
        "servers": server_results,
    });

    let out_dir = Path::new("results/census");
    std::fs::create_dir_all(out_dir)?;
    let out_path = out_dir.join("class_b_annotation_coverage.json");
    std::fs::write(&out_path, serde_json::to_string_pretty(&output)?)?;
    eprintln!("census-stage1: wrote {}", out_path.display());

    Ok(())
}

fn tally_json(tally: &coverage::AnnotationTally) -> serde_json::Value {
    fn one(t: coverage::Tally) -> serde_json::Value {
        serde_json::json!({ "explicit": t.explicit, "defaulted": t.defaulted, "absent": t.absent })
    }
    serde_json::json!({
        "readOnlyHint": one(tally.read_only_hint),
        "destructiveHint": one(tally.destructive_hint),
        "idempotentHint": one(tally.idempotent_hint),
        "openWorldHint": one(tally.open_world_hint),
    })
}

fn pct(count: usize, total: usize) -> f64 {
    if total == 0 { 0.0 } else { 100.0 * count as f64 / total as f64 }
}
