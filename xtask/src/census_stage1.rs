//! Stage 1 census: run `initialize` + `tools/list` against a sample of live Class B
//! (remote-endpoint) servers from the registry, and aggregate annotation coverage.
//!
//! No local execution — Class B servers are, by definition, remote HTTP endpoints. This
//! connects to each one exactly the way any MCP client would on first use, and no
//! differently. Sequential requests, not concurrent: these are unrelated third-party hosts
//! and there is no reason to hit them in a burst.
//!
//! P0-10: both raw responses of every discovery that returns are persisted to the evidence
//! store, and every number in the results file is derived from those stored bytes by
//! [`crate::census_report::build_report`] — the same function `census-rederive` runs offline.

use std::path::PathBuf;
use std::sync::Mutex;

use datamodel::ContainabilityClass;
use discovery::DiscoveryClient;
use intake::catalogue::{self, IngestOutcome, ResolvedTarget};
use intake::classify;
use intake::registry::RegistryClient;
use serde_json::{Map, Value, json};
use store::BlobStore;

use crate::census_report::{self, Observation, RunHeader, ServerObservation};
use crate::cli::SweepArgs;
use crate::sweep::{self, DiskGuard, StopFlag};

const STAGE: &str = "1 — initialize+tools/list against live Class B servers";
const DEFAULT_OUT: &str = "results/census/class_b_annotation_coverage.json";

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
/// endpoint. `Unresolvable` entries can never be Class B (classify() maps them to
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

/// Discover one Class B server and persist what it returned. Coverage is *not* computed
/// here — a response that discovery accepted but coverage can't read becomes a
/// `coverage_extraction` failure at derivation time, from the stored bytes, exactly as it
/// would on a re-derivation.
fn observe(url: &str, store: &Mutex<BlobStore>, stop: &StopFlag) -> Observation {
    // Shorter than the 30s default: at sample sizes in the hundreds or thousands, a
    // handful of genuinely unresponsive hosts at 30s each would dominate total run time.
    // Servers that are actually going to answer do so in low seconds at most, per the 100
    // -server sample this was tuned against.
    let mut client = DiscoveryClient::http_with_timeout(url.to_string(), std::time::Duration::from_secs(12));
    match client.discover() {
        Ok(discovery) => census_report::record_discovery(store, &discovery, stop),
        Err(e) => Observation::from_discovery_error(e, "io"),
    }
}

fn identity(candidate: &Candidate) -> Map<String, Value> {
    let Value::Object(map) = json!({
        "name": candidate.name,
        "url": candidate.url,
        "transport_type": candidate.transport_type,
    }) else {
        unreachable!("json! of an object literal is an object")
    };
    map
}

/// Deterministically hash `name` into a `u64` — `DefaultHasher`'s keys are fixed (unlike
/// `HashMap`'s `RandomState`, which is randomised per-process to resist HashDoS), so this
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
///
/// Before each attempt the free-disk floor ([`sweep::MIN_FREE_BYTES`]) is checked; if it
/// trips, the sweep stops and writes what it has with `"complete": false` rather than filling
/// the disk with evidence.
pub fn run(args: &SweepArgs) -> Result<(), Box<dyn std::error::Error>> {
    let sample_size = args.sample_size;
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

    let evidence_dir =
        census_report::resolve_evidence_dir(args.evidence_dir.as_deref(), std::env::var_os(census_report::EVIDENCE_DIR_ENV));
    let out_path = args.out.clone().unwrap_or_else(|| PathBuf::from(DEFAULT_OUT));
    let store = Mutex::new(BlobStore::open(&evidence_dir)?);
    let disk = DiskGuard::new(&evidence_dir);
    let stop = StopFlag::default();

    eprintln!(
        "census-stage1: attempting discovery against {} Class B servers (evidence -> {})...",
        candidates.len(),
        evidence_dir.display()
    );

    // jobs = 1, not configurable: unrelated third-party hosts, never burst (P0-07).
    let pool = sweep::run_bounded(
        &candidates,
        1,
        || stop.check().and_then(|()| disk.check()),
        |i, candidate| {
            eprintln!("census-stage1: [{}/{}] {} ({})", i + 1, candidates.len(), candidate.name, candidate.url);
            ServerObservation { identity: identity(candidate), observation: observe(&candidate.url, &store, &stop) }
        },
    );

    let header = RunHeader {
        stage: STAGE,
        sample_size_requested: sample_size,
        candidates_selected: candidates.len(),
        attempted: pool.completed.len(),
        stop_reason: pool.stop_reason,
        evidence_store: &evidence_dir,
    }
    .into_map()?;
    let store = store.into_inner().map_err(|_| "evidence store lock poisoned")?;
    census_report::finish("census-stage1", header, &store, &pool.completed, &out_path)
}

pub(crate) fn pct(count: usize, total: usize) -> f64 {
    if total == 0 { 0.0 } else { 100.0 * count as f64 / total as f64 }
}
