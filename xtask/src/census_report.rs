//! P0-10: census evidence persistence and the single derivation path.
//!
//! Every Stage 1/Stage 2 discovery that returns writes its two raw responses — the handshake
//! (`initialize`, or `server/discover` per P0-09) and `tools/list` — into F-05's
//! content-addressed [`BlobStore`], and the server record carries only their digests. Every
//! number in a census results file is then computed by [`build_report`] from the bytes *read
//! back out of the store*, never from the in-memory [`Discovery`] the live run happened to
//! hold. `cargo xtask census-rederive` calls the same [`build_report`] over the same stored
//! bytes with no network access, so a live run and a later re-derivation cannot disagree
//! unless the evidence itself changed — which F-05 detects ([`StoreError::Corrupt`]). This is
//! architecture.md §6 invariant 2 applied to census, where it had not held: the July results
//! kept tallies only, so no census number could be re-sliced without re-contacting servers.
//!
//! A results file splits three ways, and [`rederive`] relies on the split being mechanical:
//!
//! - **header** — run metadata (`stage`, `generated_at_unix`, `complete`, ...), carried
//!   through verbatim. Everything at the top level not in [`DERIVED_TOP_LEVEL_KEYS`].
//! - **observations** — per server, what the network gave: identity (name, endpoint or
//!   package), `discovery_path`, the evidence digests — or, for a discovery that failed and so
//!   left no bytes, its failure category and detail. Re-read, never recomputed.
//! - **derived** — everything else: each discovered server's outcome, tally, pin and
//!   annotation engagement; every corpus-wide count. Always recomputed from evidence.
//!
//! Hostile input (design.md §3): blob addresses are digests of content, never names a server
//! chose, and are parsed back strictly ([`Digest::from_hex`]) so a results file cannot steer a
//! read outside the store. Each response is already capped at 10 MiB by the discovery
//! transports, and [`build_report`] holds one server's two responses at a time, adding
//! per-server tallies rather than accumulating every tool. Free-text a server controls
//! (`failure_detail`, `negotiated_spec_revision`, pin/parse errors) is length-bounded before it
//! reaches the results file; the full bytes, where any exist, stay in the evidence store.

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use census::coverage::{self, AnnotationTally, Tally};
use datamodel::{Annotation, Digest};
use discovery::{Discovery, DiscoveryError, DiscoveryPath};
use serde_json::{Map, Value, json};
use store::{BlobStore, StoreError};

use crate::census_stage1::pct;
use crate::sweep::StopFlag;

/// Default evidence store root for both census sweeps.
pub const DEFAULT_EVIDENCE_DIR: &str = "results/census/evidence";

/// Environment override for the evidence store root (`--evidence-dir` wins over it).
pub const EVIDENCE_DIR_ENV: &str = "MCPCONF_CENSUS_EVIDENCE_DIR";

/// Bound on any server-controlled free text written into a results file. A JSON-RPC error
/// message is the server's to choose and can be up to the transport's 10 MiB cap; at sweep
/// scale that is gigabytes of "detail".
const MAX_DETAIL_CHARS: usize = 2048;

/// Bound on a negotiated spec revision. Real ones are `YYYY-MM-DD`; this one is also a map
/// key in the top-level distribution, so an unbounded one would be an unbounded key.
const MAX_REVISION_CHARS: usize = 64;

/// Top-level keys [`build_report`] computes. Everything else at the top level is header.
const DERIVED_TOP_LEVEL_KEYS: &[&str] = &[
    "attempted",
    "succeeded",
    "failed",
    "failure_categories",
    "tools_discovered",
    "annotation_tally",
    "server_weighted",
    "provenance",
    "evidence_summary",
    "servers",
];

/// Per-server keys that are observations or derived values. Everything else on a server
/// record is identity, carried through verbatim.
const SERVER_NON_IDENTITY_KEYS: &[&str] = &[
    "outcome",
    "failure_category",
    "failure_detail",
    "discovery_path",
    "negotiated_spec_revision",
    "evidence",
    "tool_count",
    "server_pin",
    "pin_error",
    "annotations_object",
    "annotation_tally",
];

/// What one discovery attempt observed.
pub(crate) enum Observation {
    /// Discovery returned both responses, and both were persisted.
    Discovered {
        /// Which handshake produced the result (P0-09 provenance).
        discovery_path: DiscoveryPath,
        /// The handshake response's evidence digest.
        handshake: Digest,
        /// The `tools/list` response's evidence digest.
        tools_list: Digest,
    },
    /// Discovery failed before both responses existed, so there are no bytes to keep.
    Failed {
        /// Failure category, e.g. `transport`, `protocol`, `io_or_timeout`.
        category: String,
        /// Human-readable detail, already length-bounded.
        detail: String,
    },
}

impl Observation {
    /// A failed observation, with `detail` bounded (it is often server-controlled).
    pub(crate) fn failed(category: &str, detail: &str) -> Self {
        Self::Failed { category: category.to_string(), detail: bounded(detail, MAX_DETAIL_CHARS) }
    }

    /// Map a discovery error onto the census failure categories both sweeps have always
    /// used. They differ only in what an I/O failure is called: plain `io` over HTTP, but
    /// `io_or_timeout` over Stage 2's stdio, where a watchdog-killed container also surfaces
    /// as a closed pipe.
    pub(crate) fn from_discovery_error(err: DiscoveryError, io_category: &str) -> Self {
        match err {
            DiscoveryError::Transport(msg) => Self::failed("transport", &msg),
            DiscoveryError::Io(e) => Self::failed(io_category, &e.to_string()),
            DiscoveryError::Protocol(msg) => Self::failed("protocol", &msg),
            DiscoveryError::ServerError { code, message } => {
                Self::failed("server_error", &format!("{code}: {message}"))
            }
        }
    }
}

/// One server's line in a sweep: who it is, and what was observed.
pub(crate) struct ServerObservation {
    /// Stage-specific identity fields (name, endpoint or package, execution provenance),
    /// opaque to derivation and carried into the record verbatim.
    pub(crate) identity: Map<String, Value>,
    /// What the attempt observed.
    pub(crate) observation: Observation,
}

/// Write both raw responses of a successful discovery into the evidence store.
pub(crate) fn persist(store: &BlobStore, discovery: &Discovery) -> Result<Observation, StoreError> {
    Ok(Observation::Discovered {
        discovery_path: discovery.discovery_path,
        handshake: store.put(&discovery.initialize_raw)?,
        tools_list: store.put(&discovery.tools_list_raw)?,
    })
}

/// [`persist`], as a sweep worker calls it.
///
/// Puts are serialised behind `store`'s lock: [`BlobStore::put`] names its temp file by
/// digest and process id, so two threads of one process putting identical bytes at the same
/// moment (two packages built from one template return byte-identical `tools/list`s) would
/// share a temp path. Serialising costs nothing next to a container launch.
///
/// A store failure is a harness fault, not a finding about the server: it is recorded as an
/// `evidence_store_error` failure for this server (honestly — its bytes were not kept) and
/// trips `stop`, so no further attempt launches into a store that cannot keep its evidence.
pub(crate) fn record_discovery(store: &Mutex<BlobStore>, discovery: &Discovery, stop: &StopFlag) -> Observation {
    let result = persist(&store.lock().expect("evidence store lock poisoned"), discovery);
    result.unwrap_or_else(|e| {
        stop.trip(format!("evidence store write failed: {e}"));
        Observation::failed("evidence_store_error", &e.to_string())
    })
}

/// The evidence store root: `--evidence-dir` if given, else [`EVIDENCE_DIR_ENV`] if set and
/// non-empty, else [`DEFAULT_EVIDENCE_DIR`]. The environment value is a parameter (callers
/// pass `std::env::var_os(EVIDENCE_DIR_ENV)`) so this stays testable without mutating the
/// process environment.
pub(crate) fn resolve_evidence_dir(flag: Option<&Path>, env: Option<OsString>) -> PathBuf {
    flag.map(Path::to_path_buf)
        .or_else(|| env.filter(|v| !v.is_empty()).map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_EVIDENCE_DIR))
}

/// Run metadata common to both sweeps.
pub(crate) struct RunHeader<'a> {
    /// The stage label, unchanged from the July results.
    pub(crate) stage: &'a str,
    /// The requested sample size.
    pub(crate) sample_size_requested: usize,
    /// How many candidates the stable-hash selection actually produced.
    pub(crate) candidates_selected: usize,
    /// How many were attempted before the sweep finished or stopped.
    pub(crate) attempted: usize,
    /// Why the sweep stopped early, if it did.
    pub(crate) stop_reason: Option<String>,
    /// The evidence store root this run wrote to.
    pub(crate) evidence_store: &'a Path,
}

impl RunHeader<'_> {
    /// The header as a JSON map, stamped with the current time.
    pub(crate) fn into_map(self) -> Result<Map<String, Value>, std::time::SystemTimeError> {
        let generated_at_unix = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let Value::Object(map) = json!({
            "generated_at_unix": generated_at_unix,
            "stage": self.stage,
            "sample_size_requested": self.sample_size_requested,
            "candidates_selected": self.candidates_selected,
            "complete": self.stop_reason.is_none(),
            "incomplete_reason": self.stop_reason,
            "not_attempted": self.candidates_selected - self.attempted,
            "evidence_store": self.evidence_store.display().to_string(),
        }) else {
            unreachable!("json! of an object literal is an object")
        };
        Ok(map)
    }
}

/// Build a complete results file from `header` and `servers`, reading every response back
/// out of `store`. The single derivation path: the live sweeps and [`rederive`] both end here.
///
/// Fails only on the store — a digest with no blob, or a blob that no longer hashes to its
/// address. Hostile or malformed response *content* never fails the build; it becomes a
/// per-server finding (`coverage_extraction`, `pin_error`, a `null` revision).
pub(crate) fn build_report(
    store: &BlobStore,
    mut header: Map<String, Value>,
    servers: &[ServerObservation],
) -> Result<Value, StoreError> {
    let mut rollup = Rollup::default();
    let mut records = Vec::with_capacity(servers.len());

    for server in servers {
        let mut record = server.identity.clone();
        match &server.observation {
            Observation::Failed { category, detail } => {
                rollup.record_failure(category);
                record.insert("outcome".into(), "failed".into());
                record.insert("failure_category".into(), category.as_str().into());
                record.insert("failure_detail".into(), detail.as_str().into());
            }
            Observation::Discovered { discovery_path, handshake, tools_list } => {
                derive_discovered(store, *discovery_path, handshake, tools_list, &mut record, &mut rollup)?;
            }
        }
        records.push(Value::Object(record));
    }

    rollup.write_into(&mut header);
    header.insert("servers".into(), Value::Array(records));
    Ok(Value::Object(header))
}

/// Derive one discovered server's record fields from its stored evidence.
fn derive_discovered(
    store: &BlobStore,
    discovery_path: DiscoveryPath,
    handshake: &Digest,
    tools_list: &Digest,
    record: &mut Map<String, Value>,
    rollup: &mut Rollup,
) -> Result<(), StoreError> {
    let handshake_raw = store.get(handshake)?;
    let tools_list_raw = store.get(tools_list)?;

    // Same parser the live client used to negotiate (`discovery::negotiated_spec_revision`).
    // It cannot fail on bytes a live discovery accepted; `null` covers it being asked to anyway.
    let revision = discovery::negotiated_spec_revision(&handshake_raw)
        .ok()
        .map(|r| bounded(&r, MAX_REVISION_CHARS));
    rollup.record_discovered(discovery_path, revision.as_deref(), (handshake, handshake_raw.len()), (tools_list, tools_list_raw.len()));

    record.insert("discovery_path".into(), discovery_path.as_str().into());
    record.insert("negotiated_spec_revision".into(), revision.map_or(Value::Null, Value::String));
    record.insert(
        "evidence".into(),
        json!({
            "handshake_raw": { "digest": handshake.to_string(), "bytes": handshake_raw.len() },
            "tools_list_raw": { "digest": tools_list.to_string(), "bytes": tools_list_raw.len() },
        }),
    );

    let tools = match coverage::tool_coverage(&tools_list_raw) {
        Ok(tools) => tools,
        Err(e) => {
            rollup.record_failure("coverage_extraction");
            record.insert("outcome".into(), "failed".into());
            record.insert("failure_category".into(), "coverage_extraction".into());
            record.insert("failure_detail".into(), bounded(&e.to_string(), MAX_DETAIL_CHARS).into());
            return Ok(());
        }
    };
    let tally = coverage::tally(&tools);
    let engagement = Engagement::of(tools.len(), &tally);
    rollup.record_success(tools.len(), &tally, engagement);

    record.insert("outcome".into(), "success".into());
    record.insert("tool_count".into(), tools.len().into());
    record.insert("annotation_tally".into(), tally_json(&tally));
    record.insert("annotations_object".into(), engagement.as_str().into());
    // P0-02's pin binds this record to the exact tool set observed. A response coverage can
    // read but the pinner cannot (a tool with no `inputSchema`) is still a census success, as
    // it was in July — the pin is recorded as absent with its reason, never fatal.
    match discovery::pin_tools(&tools_list_raw) {
        Ok((_, server_pin)) => {
            record.insert("server_pin".into(), server_pin.to_string().into());
        }
        Err(e) => {
            record.insert("server_pin".into(), Value::Null);
            record.insert("pin_error".into(), bounded(&e.to_string(), MAX_DETAIL_CHARS).into());
        }
    }
    Ok(())
}

/// Whether a server's tools carry an `annotations` object at all — the server-weighted
/// reading of design.md open question 1.
#[derive(Clone, Copy)]
enum Engagement {
    /// The server declares no tools; neither "none" nor "all" would be honest.
    NoTools,
    /// No tool has an `annotations` object.
    None,
    /// Some tools do, some don't.
    Some,
    /// Every tool does.
    All,
}

impl Engagement {
    fn of(tool_count: usize, tally: &AnnotationTally) -> Self {
        // `absent` means "no annotations object", which is the same set of tools for all
        // four annotations by construction (census::coverage), so any one column decides it.
        let without_object = tally.read_only_hint.absent;
        match (tool_count, without_object) {
            (0, _) => Self::NoTools,
            (n, absent) if absent == n => Self::None,
            (_, 0) => Self::All,
            _ => Self::Some,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::NoTools => "no_tools",
            Self::None => "none",
            Self::Some => "some",
            Self::All => "all",
        }
    }
}

/// The four annotations in the fixed order every per-annotation array here uses. Their JSON
/// names are their MCP wire names, from `datamodel` — the one place those names live.
const ANNOTATIONS: [Annotation; 4] =
    [Annotation::ReadOnlyHint, Annotation::DestructiveHint, Annotation::IdempotentHint, Annotation::OpenWorldHint];

/// `tally`'s four columns, in [`ANNOTATIONS`] order.
fn by_annotation(tally: &AnnotationTally) -> [(&'static str, Tally); 4] {
    let columns = [tally.read_only_hint, tally.destructive_hint, tally.idempotent_hint, tally.open_world_hint];
    std::array::from_fn(|i| (ANNOTATIONS[i].as_db_str(), columns[i]))
}

/// A tally as JSON — the exact shape of the July files' `annotation_tally`, reused per server.
fn tally_json(tally: &AnnotationTally) -> Value {
    let mut map = Map::new();
    for (name, t) in by_annotation(tally) {
        map.insert(name.into(), json!({ "explicit": t.explicit, "defaulted": t.defaulted, "absent": t.absent }));
    }
    Value::Object(map)
}

/// Corpus-wide accumulators, filled one server at a time.
#[derive(Default)]
struct Rollup {
    attempted: usize,
    succeeded: usize,
    failure_categories: BTreeMap<String, usize>,
    // Tool-weighted: unchanged in meaning from July (the sum of per-server tallies is the
    // tally of every tool, per census::coverage's AddAssign).
    tools_discovered: usize,
    tool_tally: AnnotationTally,
    // Server-weighted, over successful servers declaring >= 1 tool.
    servers_with_tools: usize,
    servers_with_zero_tools: usize,
    object_none: usize,
    object_some: usize,
    object_all: usize,
    explicit_on_any_tool: [usize; 4],
    explicit_on_every_tool: [usize; 4],
    // Provenance, over every server whose handshake and tools/list both returned.
    discovered: usize,
    discovery_paths: BTreeMap<&'static str, usize>,
    revisions: BTreeMap<String, usize>,
    // Evidence volume.
    handshake_bytes: u64,
    tools_list_bytes: u64,
    distinct_blobs: HashMap<Digest, u64>,
}

impl Rollup {
    fn record_failure(&mut self, category: &str) {
        self.attempted += 1;
        *self.failure_categories.entry(category.to_string()).or_insert(0) += 1;
    }

    fn record_discovered(
        &mut self,
        path: DiscoveryPath,
        revision: Option<&str>,
        handshake: (&Digest, usize),
        tools_list: (&Digest, usize),
    ) {
        self.discovered += 1;
        *self.discovery_paths.entry(path.as_str()).or_insert(0) += 1;
        *self.revisions.entry(revision.unwrap_or("<unparseable>").to_string()).or_insert(0) += 1;
        self.handshake_bytes += handshake.1 as u64;
        self.tools_list_bytes += tools_list.1 as u64;
        for (digest, len) in [handshake, tools_list] {
            self.distinct_blobs.insert(*digest, len as u64);
        }
    }

    fn record_success(&mut self, tool_count: usize, tally: &AnnotationTally, engagement: Engagement) {
        self.attempted += 1;
        self.succeeded += 1;
        self.tools_discovered += tool_count;
        self.tool_tally += *tally;

        if tool_count == 0 {
            self.servers_with_zero_tools += 1;
            return;
        }
        self.servers_with_tools += 1;
        match engagement {
            Engagement::None => self.object_none += 1,
            Engagement::Some => self.object_some += 1,
            Engagement::All => self.object_all += 1,
            Engagement::NoTools => unreachable!("tool_count > 0"),
        }
        for (i, (_, t)) in by_annotation(tally).into_iter().enumerate() {
            if t.explicit > 0 {
                self.explicit_on_any_tool[i] += 1;
            }
            if t.explicit == tool_count {
                self.explicit_on_every_tool[i] += 1;
            }
        }
    }

    fn write_into(self, report: &mut Map<String, Value>) {
        let share = |count: usize| json!({ "count": count, "pct": pct_2dp(count, self.servers_with_tools) });

        let mut explicit = Map::new();
        for (i, annotation) in ANNOTATIONS.iter().enumerate() {
            explicit.insert(
                annotation.as_db_str().into(),
                json!({
                    "on_at_least_one_tool": share(self.explicit_on_any_tool[i]),
                    "on_every_tool": share(self.explicit_on_every_tool[i]),
                }),
            );
        }

        let mut discovery_paths: BTreeMap<&str, usize> =
            [DiscoveryPath::Initialize, DiscoveryPath::ServerDiscover].into_iter().map(|p| (p.as_str(), 0)).collect();
        discovery_paths.extend(self.discovery_paths);

        let derived = json!({
            "attempted": self.attempted,
            "succeeded": self.succeeded,
            "failed": self.attempted - self.succeeded,
            "failure_categories": self.failure_categories,
            "tools_discovered": self.tools_discovered,
            "annotation_tally": tally_json(&self.tool_tally),
            "server_weighted": {
                "denominator": "succeeded servers declaring at least one tool",
                "servers": self.servers_with_tools,
                "succeeded_with_zero_tools": self.servers_with_zero_tools,
                "annotations_object": {
                    "none": share(self.object_none),
                    "some": share(self.object_some),
                    "all": share(self.object_all),
                },
                "explicit": explicit,
            },
            "provenance": {
                "denominator": "servers whose handshake and tools/list both returned",
                "servers": self.discovered,
                "discovery_path": discovery_paths,
                "negotiated_spec_revision": self.revisions,
            },
            "evidence_summary": {
                "servers_with_evidence": self.discovered,
                "handshake_bytes": self.handshake_bytes,
                "tools_list_bytes": self.tools_list_bytes,
                "bytes_referenced": self.handshake_bytes + self.tools_list_bytes,
                "distinct_blobs": self.distinct_blobs.len(),
                "bytes_distinct": self.distinct_blobs.values().sum::<u64>(),
            },
        });
        if let Value::Object(derived) = derived {
            report.extend(derived);
        }
    }
}

/// A percentage rounded to two decimal places — deterministic, so a re-derivation
/// reproduces it to the byte.
fn pct_2dp(count: usize, total: usize) -> f64 {
    (pct(count, total) * 100.0).round() / 100.0
}

/// `s`, cut to at most `max_chars` characters with a marker saying how long it was.
pub(crate) fn bounded(s: &str, max_chars: usize) -> String {
    match s.char_indices().nth(max_chars) {
        None => s.to_string(),
        Some((cut, _)) => format!("{}… [truncated from {} bytes]", &s[..cut], s.len()),
    }
}

/// Write `report` as pretty JSON, creating the parent directory if needed.
pub(crate) fn write_report(path: &Path, report: &Value) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(report)?)?;
    Ok(())
}

/// Build, summarise to stderr, and write a sweep's results file.
pub(crate) fn finish(
    label: &str,
    header: Map<String, Value>,
    store: &BlobStore,
    servers: &[ServerObservation],
    out_path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let report = build_report(store, header, servers)?;
    let n = |pointer: &str| report.pointer(pointer).and_then(Value::as_u64).unwrap_or(0);

    let (attempted, succeeded) = (n("/attempted"), n("/succeeded"));
    eprintln!(
        "{label}: {attempted} attempted, {succeeded} succeeded ({:.1}%), {} failed",
        pct(succeeded as usize, attempted as usize),
        n("/failed"),
    );
    if let Some(categories) = report["failure_categories"].as_object() {
        for (category, count) in categories {
            eprintln!("{label}:   {category}: {count}");
        }
    }
    eprintln!(
        "{label}: of {} succeeded servers declaring tools, an annotations object is on no tool for {}, \
         some for {}, every tool for {}",
        n("/server_weighted/servers"),
        n("/server_weighted/annotations_object/none/count"),
        n("/server_weighted/annotations_object/some/count"),
        n("/server_weighted/annotations_object/all/count"),
    );
    let with_evidence = n("/evidence_summary/servers_with_evidence");
    eprintln!(
        "{label}: evidence — {} bytes in {} distinct blobs for {with_evidence} servers ({:.0} bytes/server)",
        n("/evidence_summary/bytes_distinct"),
        n("/evidence_summary/distinct_blobs"),
        n("/evidence_summary/bytes_referenced") as f64 / with_evidence.max(1) as f64,
    );
    if let Some(reason) = report["incomplete_reason"].as_str() {
        eprintln!("{label}: INCOMPLETE — {reason}");
    }

    write_report(out_path, &report)?;
    eprintln!("{label}: wrote {}", out_path.display());
    Ok(())
}

/// What [`rederive`] found.
pub struct Rederivation {
    /// Server records in the results file.
    pub servers: usize,
    /// How many of them had evidence to re-derive from.
    pub servers_with_evidence: usize,
    /// The regenerated results file, exactly as the live run would have written it.
    pub regenerated: String,
    /// Whether `regenerated` is byte-identical to the input.
    pub byte_identical: bool,
    /// Where they differ, by top-level key or server index (empty when identical).
    pub differences: Vec<String>,
}

/// Regenerate a census results file from its stored evidence — no network, no execution.
///
/// Reads the header and per-server observations back out of `results_path`, then runs the
/// same [`build_report`] the live sweep ran, over the blobs in `evidence_dir` (default: the
/// `evidence_store` the file recorded). Byte-for-byte comparison rather than a parsed-value
/// comparison on purpose: `serde_json`'s default float parsing is not guaranteed to
/// round-trip, and comparing the bytes sidesteps the question instead of depending on it.
pub fn rederive(results_path: &Path, evidence_dir: Option<&Path>) -> Result<Rederivation, Box<dyn std::error::Error>> {
    let original = std::fs::read_to_string(results_path)?;
    let parsed: Value = serde_json::from_str(&original)?;
    let (header, servers) = split_results(parsed.clone())?;

    let root = match evidence_dir {
        Some(dir) => dir.to_path_buf(),
        None => PathBuf::from(
            header
                .get("evidence_store")
                .and_then(Value::as_str)
                .ok_or("results file records no evidence_store; pass --evidence-dir")?,
        ),
    };
    // BlobStore::open would create a missing root; a re-derivation must never conjure an
    // empty store and then report every blob as merely "not found".
    if !root.is_dir() {
        return Err(format!("evidence store {} does not exist", root.display()).into());
    }
    let store = BlobStore::open(&root)?;

    let report = build_report(&store, header, &servers)?;
    let regenerated = serde_json::to_string_pretty(&report)?;
    let byte_identical = regenerated == original;
    let differences =
        if byte_identical { Vec::new() } else { differences(&parsed, &serde_json::from_str(&regenerated)?) };

    Ok(Rederivation {
        servers: servers.len(),
        servers_with_evidence: servers
            .iter()
            .filter(|s| matches!(s.observation, Observation::Discovered { .. }))
            .count(),
        regenerated,
        byte_identical,
        differences,
    })
}

/// `cargo xtask census-rederive`: re-derive, optionally write the result, report. `Ok(false)`
/// means the evidence no longer reproduces the file.
pub fn run_rederive(args: &crate::cli::RederiveArgs) -> Result<bool, Box<dyn std::error::Error>> {
    let outcome = rederive(&args.input, args.evidence_dir.as_deref())?;
    if let Some(out) = &args.out {
        if let Some(parent) = out.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(out, &outcome.regenerated)?;
        eprintln!("census-rederive: wrote {}", out.display());
    }
    if outcome.byte_identical {
        eprintln!(
            "census-rederive: {} reproduces byte-for-byte from stored evidence ({} servers, {} with evidence)",
            args.input.display(),
            outcome.servers,
            outcome.servers_with_evidence
        );
    } else {
        eprintln!("census-rederive: {} does NOT reproduce from stored evidence; differs at:", args.input.display());
        for difference in &outcome.differences {
            eprintln!("census-rederive:   {difference}");
        }
    }
    Ok(outcome.byte_identical)
}

/// Split a parsed results file into its header and its per-server observations.
fn split_results(value: Value) -> Result<(Map<String, Value>, Vec<ServerObservation>), String> {
    let Value::Object(mut top) = value else {
        return Err("results file is not a JSON object".into());
    };
    let Some(Value::Array(records)) = top.remove("servers") else {
        return Err("results file has no \"servers\" array".into());
    };
    for key in DERIVED_TOP_LEVEL_KEYS {
        top.remove(*key);
    }

    let servers = records
        .into_iter()
        .enumerate()
        .map(|(i, record)| {
            let Value::Object(mut record) = record else {
                return Err(format!("servers[{i}] is not an object"));
            };
            let observation = observation_of(&record).map_err(|e| {
                let name = record.get("name").and_then(Value::as_str).unwrap_or("?");
                format!("servers[{i}] ({name}): {e}")
            })?;
            for key in SERVER_NON_IDENTITY_KEYS {
                record.remove(*key);
            }
            Ok(ServerObservation { identity: record, observation })
        })
        .collect::<Result<_, String>>()?;
    Ok((top, servers))
}

/// Read one server record's observation back. Failure details are taken verbatim — they
/// were bounded once, when observed, and must not be re-cut here or the output would drift.
fn observation_of(record: &Map<String, Value>) -> Result<Observation, String> {
    if let Some(evidence) = record.get("evidence") {
        let digest = |field: &str| {
            evidence
                .pointer(&format!("/{field}/digest"))
                .and_then(Value::as_str)
                .and_then(Digest::from_hex)
                .ok_or_else(|| format!("evidence.{field}.digest is missing or not a 64-character lowercase hex digest"))
        };
        let discovery_path = record
            .get("discovery_path")
            .and_then(Value::as_str)
            .and_then(DiscoveryPath::parse)
            .ok_or("discovery_path is missing or unrecognised")?;
        return Ok(Observation::Discovered {
            discovery_path,
            handshake: digest("handshake_raw")?,
            tools_list: digest("tools_list_raw")?,
        });
    }

    let text = |key: &str| record.get(key).and_then(Value::as_str).map(str::to_string);
    match text("outcome").as_deref() {
        Some("failed") => Ok(Observation::Failed {
            category: text("failure_category").ok_or("failed record has no failure_category")?,
            detail: text("failure_detail").ok_or("failed record has no failure_detail")?,
        }),
        Some("success") => Err("outcome is success but there are no evidence digests — this file predates \
                                P0-10's evidence persistence and cannot be re-derived; re-run the sweep"
            .into()),
        _ => Err("outcome is missing or unrecognised".into()),
    }
}

/// Top-level keys (and, under `servers`, record indices) whose values differ.
fn differences(original: &Value, regenerated: &Value) -> Vec<String> {
    const MAX_LISTED: usize = 20;
    let (Some(a), Some(b)) = (original.as_object(), regenerated.as_object()) else {
        return vec!["<top level>".into()];
    };
    let mut keys: Vec<&String> = a.keys().chain(b.keys()).collect();
    keys.sort();
    keys.dedup();

    let mut out = Vec::new();
    for key in keys {
        match (a.get(key), b.get(key)) {
            (Some(Value::Array(x)), Some(Value::Array(y))) if key == "servers" => {
                for i in 0..x.len().max(y.len()) {
                    if x.get(i) != y.get(i) {
                        out.push(format!("servers[{i}]"));
                    }
                }
            }
            (x, y) if x != y => out.push(key.clone()),
            _ => {}
        }
    }
    if out.is_empty() {
        out.push("formatting only (values equal, bytes differ)".into());
    }
    out.truncate(MAX_LISTED);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn discovery(path: DiscoveryPath, revision: &str, tools_json: &str) -> Discovery {
        Discovery {
            initialize_raw: format!(
                r#"{{"jsonrpc":"2.0","id":0,"result":{{"protocolVersion":"{revision}","capabilities":{{}},"serverInfo":{{"name":"fake","version":"0"}}}}}}"#
            )
            .into_bytes(),
            tools_list_raw: format!(r#"{{"jsonrpc":"2.0","id":1,"result":{{"tools":[{tools_json}]}}}}"#).into_bytes(),
            negotiated_spec_revision: revision.to_string(),
            discovery_path: path,
        }
    }

    fn identity(name: &str) -> Map<String, Value> {
        let Value::Object(map) = json!({ "name": name, "url": format!("https://{name}.example/mcp"), "transport_type": "streamable-http" }) else {
            unreachable!()
        };
        map
    }

    const ALL: &str = r#"{"name":"a1","inputSchema":{},"annotations":{"readOnlyHint":true,"destructiveHint":false}},
                         {"name":"a2","inputSchema":{},"annotations":{"readOnlyHint":true}}"#;
    const NONE: &str = r#"{"name":"n1","inputSchema":{}},{"name":"n2","inputSchema":{}}"#;
    const SOME: &str = r#"{"name":"s1","inputSchema":{},"annotations":{"readOnlyHint":false}},{"name":"s2","inputSchema":{}}"#;
    /// Coverage can read this; the pinner cannot (no `inputSchema`).
    const UNPINNABLE: &str = r#"{"name":"u1","annotations":{"openWorldHint":true}}"#;

    /// Drive the live path — `record_discovery` into a real store, `build_report` back out
    /// of it, `write_report` to disk — exactly as a sweep does, minus the network.
    fn live_run(dir: &Path) -> (PathBuf, PathBuf) {
        let evidence_dir = dir.join("evidence");
        let store = Mutex::new(BlobStore::open(&evidence_dir).expect("open store"));
        let stop = StopFlag::default();
        let init = DiscoveryPath::Initialize;

        let mut servers = Vec::new();
        let mut push = |name: &str, observation: Observation| {
            servers.push(ServerObservation { identity: identity(name), observation });
        };
        push("all", record_discovery(&store, &discovery(init, "2025-11-25", ALL), &stop));
        push("none", record_discovery(&store, &discovery(init, "2025-11-25", NONE), &stop));
        push("some", record_discovery(&store, &discovery(init, "2025-11-25", SOME), &stop));
        push(
            "zero-tools-new-handshake",
            record_discovery(&store, &discovery(DiscoveryPath::ServerDiscover, "2026-07-28", ""), &stop),
        );
        let mut no_tools_array = discovery(init, "2025-11-25", "");
        no_tools_array.tools_list_raw = br#"{"jsonrpc":"2.0","id":1,"result":{}}"#.to_vec();
        push("malformed-tools-list", record_discovery(&store, &no_tools_array, &stop));
        push("unpinnable", record_discovery(&store, &discovery(init, "2025-11-25", UNPINNABLE), &stop));
        push(
            "unreachable",
            Observation::from_discovery_error(DiscoveryError::Transport("http status: 401".into()), "io"),
        );
        // Byte-identical to "none": content addressing must store it once.
        push("none-twin", record_discovery(&store, &discovery(init, "2025-11-25", NONE), &stop));
        assert_eq!(stop.check(), Ok(()), "no store fault in a healthy run");

        let header = RunHeader {
            stage: "test",
            sample_size_requested: 10,
            candidates_selected: 8,
            attempted: servers.len(),
            stop_reason: None,
            evidence_store: &evidence_dir,
        }
        .into_map()
        .expect("clock");
        let report = build_report(&store.into_inner().expect("lock"), header, &servers).expect("build");
        let results = dir.join("results.json");
        write_report(&results, &report).expect("write");
        (results, evidence_dir)
    }

    fn read(path: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(path).expect("read")).expect("json")
    }

    /// The P0-10 exit criterion: re-derivation from stored evidence reproduces the live
    /// run's results file exactly — to the byte, not merely to equal numbers.
    #[test]
    fn rederivation_reproduces_the_live_results_byte_for_byte() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (results, _) = live_run(dir.path());

        let outcome = rederive(&results, None).expect("rederive");
        assert!(outcome.byte_identical, "differs at {:?}", outcome.differences);
        assert_eq!(outcome.regenerated, std::fs::read_to_string(&results).expect("read"));
        assert_eq!(outcome.servers, 8);
        assert_eq!(outcome.servers_with_evidence, 7);
    }

    /// The same exit criterion through the whole live wiring rather than a hand-assembled
    /// run: real [`discovery::DiscoveryClient`]s speaking stdio to real subprocesses (the
    /// transport Stage 2 uses, minus `docker`), driven by the sweep's own bounded pool with
    /// `jobs > 1` so completion order is scrambled, written by the sweep's own [`finish`] —
    /// then re-derived offline, which must reproduce the file byte for byte.
    #[test]
    fn rederivation_reproduces_a_concurrent_sweep_through_the_real_client_and_finish() {
        use crate::sweep::run_bounded;

        fn fake_server(revision: &str, tools: &str, delay_ms: u32) -> String {
            let init = format!(
                r#"{{"jsonrpc":"2.0","id":0,"result":{{"protocolVersion":"{revision}","capabilities":{{}},"serverInfo":{{"name":"fake","version":"0"}}}}}}"#
            );
            let list = format!(r#"{{"jsonrpc":"2.0","id":1,"result":{{"tools":[{tools}]}}}}"#);
            // initialize -> response; notifications/initialized; tools/list -> response.
            format!(
                "sleep {}; read -r _; printf '%s\\n' '{init}'; read -r _; read -r _; printf '%s\\n' '{list}'",
                f64::from(delay_ms) / 1000.0
            )
        }

        let one_line = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
        let scripts = vec![
            ("slow-all", fake_server("2025-11-25", &one_line(ALL), 300)),
            ("fast-none", fake_server("2025-11-25", &one_line(NONE), 0)),
            ("some", fake_server("2025-06-18", &one_line(SOME), 150)),
            ("dies", "exit 3".to_string()),
            ("zero", fake_server("2025-11-25", "", 50)),
        ];

        let dir = tempfile::tempdir().expect("tempdir");
        let evidence_dir = dir.path().join("evidence");
        let store = Mutex::new(BlobStore::open(&evidence_dir).expect("store"));
        let stop = StopFlag::default();

        let pool = run_bounded(&scripts, 3, || stop.check(), |_, (name, script)| {
            let mut client = discovery::DiscoveryClient::stdio_with_timeout(
                "sh",
                &["-c", script],
                std::time::Duration::from_secs(20),
            )
            .expect("spawn sh");
            let observation = match client.discover() {
                Ok(discovered) => record_discovery(&store, &discovered, &stop),
                Err(e) => Observation::from_discovery_error(e, "io_or_timeout"),
            };
            ServerObservation { identity: identity(name), observation }
        });
        assert_eq!(pool.stop_reason, None);

        let header = RunHeader {
            stage: "test",
            sample_size_requested: scripts.len(),
            candidates_selected: scripts.len(),
            attempted: pool.completed.len(),
            stop_reason: None,
            evidence_store: &evidence_dir,
        }
        .into_map()
        .expect("clock");
        let results = dir.path().join("results.json");
        finish("test", header, &store.into_inner().expect("lock"), &pool.completed, &results).expect("finish");

        let report = read(&results);
        let names: Vec<&str> = report["servers"].as_array().expect("servers").iter().map(|s| s["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["slow-all", "fast-none", "some", "dies", "zero"], "input order, not completion order");
        assert_eq!(report["succeeded"], 4);
        assert_eq!(report["failure_categories"], json!({ "io_or_timeout": 1 }));
        assert_eq!(report["provenance"]["negotiated_spec_revision"], json!({ "2025-06-18": 1, "2025-11-25": 3 }));

        let outcome = rederive(&results, None).expect("rederive");
        assert!(outcome.byte_identical, "differs at {:?}", outcome.differences);
        assert_eq!(outcome.servers_with_evidence, 4);
    }

    /// The tool-weighted fields keep their July meaning: `tools_discovered` and
    /// `annotation_tally` are exactly a single tally over every successful server's tools.
    #[test]
    fn tool_weighted_fields_equal_one_tally_over_every_successful_tool() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (results, _) = live_run(dir.path());
        let report = read(&results);

        let mut every_tool = Vec::new();
        for body in [ALL, NONE, SOME, "", UNPINNABLE, NONE] {
            every_tool.extend(coverage::tool_coverage(&discovery(DiscoveryPath::Initialize, "x", body).tools_list_raw).expect("parse"));
        }
        assert_eq!(report["tools_discovered"], every_tool.len());
        assert_eq!(report["annotation_tally"], tally_json(&coverage::tally(&every_tool)));
        assert_eq!(report["attempted"], 8);
        assert_eq!(report["succeeded"], 6);
        assert_eq!(report["failed"], 2);
        assert_eq!(report["failure_categories"], json!({ "coverage_extraction": 1, "transport": 1 }));
    }

    #[test]
    fn server_weighted_rollup_counts_servers_not_tools() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (results, _) = live_run(dir.path());
        let weighted = &read(&results)["server_weighted"];

        // all, none, some, unpinnable, none-twin declare tools; the zero-tool server is
        // reported separately rather than counted as vacuously "all" or "none".
        assert_eq!(weighted["servers"], 5);
        assert_eq!(weighted["succeeded_with_zero_tools"], 1);
        assert_eq!(weighted["annotations_object"]["none"], json!({ "count": 2, "pct": 40.0 }));
        assert_eq!(weighted["annotations_object"]["some"], json!({ "count": 1, "pct": 20.0 }));
        assert_eq!(weighted["annotations_object"]["all"], json!({ "count": 2, "pct": 40.0 }));
        // readOnlyHint: explicit on both of "all"'s tools and one of "some"'s.
        assert_eq!(weighted["explicit"]["readOnlyHint"]["on_at_least_one_tool"]["count"], 2);
        assert_eq!(weighted["explicit"]["readOnlyHint"]["on_every_tool"]["count"], 1);
        // destructiveHint: only one of "all"'s two tools.
        assert_eq!(weighted["explicit"]["destructiveHint"]["on_at_least_one_tool"]["count"], 1);
        assert_eq!(weighted["explicit"]["destructiveHint"]["on_every_tool"]["count"], 0);
        // openWorldHint: "unpinnable"'s single tool, so both at-least-one and every.
        assert_eq!(weighted["explicit"]["openWorldHint"]["on_every_tool"], json!({ "count": 1, "pct": 20.0 }));
    }

    #[test]
    fn provenance_pins_and_evidence_are_recorded_per_server() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (results, _) = live_run(dir.path());
        let report = read(&results);
        let servers = report["servers"].as_array().expect("servers");

        assert_eq!(report["provenance"]["servers"], 7);
        assert_eq!(report["provenance"]["discovery_path"], json!({ "initialize": 6, "server_discover": 1 }));
        assert_eq!(report["provenance"]["negotiated_spec_revision"], json!({ "2025-11-25": 6, "2026-07-28": 1 }));

        let zero = &servers[3];
        assert_eq!(zero["discovery_path"], "server_discover");
        assert_eq!(zero["negotiated_spec_revision"], "2026-07-28");
        assert_eq!(zero["annotations_object"], "no_tools");

        let malformed = &servers[4];
        assert_eq!(malformed["outcome"], "failed");
        assert_eq!(malformed["failure_category"], "coverage_extraction");
        assert!(malformed["evidence"]["tools_list_raw"]["digest"].is_string(), "malformed bytes are kept for inspection");

        let unpinnable = &servers[5];
        assert_eq!(unpinnable["outcome"], "success", "a pin failure is not a census failure (July semantics)");
        assert_eq!(unpinnable["server_pin"], Value::Null);
        assert!(unpinnable["pin_error"].is_string());
        assert!(servers[0]["server_pin"].as_str().is_some_and(|p| Digest::from_hex(p).is_some()));

        assert!(servers[6].get("evidence").is_none(), "a failed discovery has no bytes to keep");
        assert_eq!(servers[1]["evidence"], servers[7]["evidence"], "identical responses share blobs");
        let summary = &report["evidence_summary"];
        assert_eq!(summary["servers_with_evidence"], 7);
        assert!(summary["bytes_distinct"].as_u64() < summary["bytes_referenced"].as_u64());
    }

    /// The derived numbers are checked against the evidence, not trusted from the file.
    #[test]
    fn a_derived_number_edited_after_the_fact_is_caught() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (results, _) = live_run(dir.path());
        let mut report = read(&results);
        report["annotation_tally"]["readOnlyHint"]["explicit"] = json!(999);
        write_report(&results, &report).expect("rewrite");

        let outcome = rederive(&results, None).expect("rederive still runs");
        assert!(!outcome.byte_identical);
        assert_eq!(outcome.differences, vec!["annotation_tally".to_string()]);
    }

    /// Re-derivation reads the store, so missing evidence is a loud error, never a silent
    /// fallback to the numbers the file already contains.
    #[test]
    fn missing_evidence_fails_loudly() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (results, evidence_dir) = live_run(dir.path());
        let digest = read(&results)["servers"][0]["evidence"]["tools_list_raw"]["digest"]
            .as_str()
            .expect("digest")
            .to_string();
        std::fs::remove_file(evidence_dir.join(&digest)).expect("remove blob");

        let err = rederive(&results, None).err().expect("must fail").to_string();
        assert!(err.contains(&digest), "{err}");

        let elsewhere = dir.path().join("no-such-store");
        let err = rederive(&results, Some(&elsewhere)).err().expect("must fail").to_string();
        assert!(err.contains("does not exist"), "{err}");
        assert!(!elsewhere.exists(), "re-derivation must not create an empty store");
    }

    #[test]
    fn a_results_file_from_before_evidence_persistence_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("ev")).expect("mkdir");
        let july = dir.path().join("july.json");
        std::fs::write(
            &july,
            r#"{"stage":"1","evidence_store":"unused","servers":[{"name":"x","url":"u","outcome":"success","tool_count":3}]}"#,
        )
        .expect("write");
        let err = rederive(&july, Some(&dir.path().join("ev"))).err().expect("must fail").to_string();
        assert!(err.contains("predates P0-10"), "{err}");
    }

    /// A digest string is parsed strictly before it ever becomes a path, so a tampered
    /// results file cannot point a read outside the store.
    #[test]
    fn a_non_digest_evidence_address_is_rejected_before_any_read() {
        let record = json!({
            "outcome": "success",
            "discovery_path": "initialize",
            "evidence": {
                "handshake_raw": { "digest": "../../../../etc/passwd" },
                "tools_list_raw": { "digest": "ab".repeat(32) },
            },
        });
        let Value::Object(record) = record else { unreachable!() };
        let err = observation_of(&record).err().expect("must reject");
        assert!(err.contains("handshake_raw"), "{err}");
    }

    #[test]
    fn server_controlled_text_is_bounded_in_the_results_file() {
        let flood = "x".repeat(1024 * 1024);
        let Observation::Failed { detail, .. } =
            Observation::from_discovery_error(DiscoveryError::ServerError { code: -32000, message: flood }, "io")
        else {
            panic!("expected Failed")
        };
        assert!(detail.len() < MAX_DETAIL_CHARS + 64, "detail is {} bytes", detail.len());
        assert!(detail.contains("truncated from"));

        let dir = tempfile::tempdir().expect("tempdir");
        let store = BlobStore::open(dir.path()).expect("store");
        let long_revision = "9".repeat(10_000);
        let observation = persist(&store, &discovery(DiscoveryPath::Initialize, &long_revision, NONE)).expect("persist");
        let header = Map::new();
        let report =
            build_report(&store, header, &[ServerObservation { identity: identity("long"), observation }]).expect("build");
        let revision = report["servers"][0]["negotiated_spec_revision"].as_str().expect("string");
        assert!(revision.len() < MAX_REVISION_CHARS + 64, "revision is {} bytes", revision.len());
        let keys = report["provenance"]["negotiated_spec_revision"].as_object().expect("map");
        assert!(keys.keys().all(|k| k.len() < MAX_REVISION_CHARS + 64));
    }

    #[test]
    fn bounded_cuts_on_a_character_boundary() {
        assert_eq!(bounded("short", 10), "short");
        let cut = bounded("ééééé", 2);
        assert!(cut.starts_with("éé…"), "{cut}");
        assert!(cut.contains("10 bytes"), "{cut}");
    }

    /// A store that cannot keep evidence stops the sweep instead of running on without it.
    #[test]
    fn an_evidence_store_fault_records_the_server_and_trips_the_stop_flag() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let store = Mutex::new(BlobStore::open(dir.path()).expect("store"));
        let mut perms = std::fs::metadata(dir.path()).expect("stat").permissions();
        perms.set_mode(0o555);
        std::fs::set_permissions(dir.path(), perms.clone()).expect("lock dir");

        let stop = StopFlag::default();
        let observation = record_discovery(&store, &discovery(DiscoveryPath::Initialize, "2025-11-25", NONE), &stop);

        perms.set_mode(0o755);
        std::fs::set_permissions(dir.path(), perms).expect("unlock dir");

        assert!(matches!(observation, Observation::Failed { ref category, .. } if category == "evidence_store_error"));
        assert!(stop.check().is_err(), "the next launch must be refused");
    }

    /// A sweep stopped by the disk floor (or a store fault) says so in its header.
    #[test]
    fn a_stopped_sweep_is_marked_incomplete_with_its_reason() {
        let header = RunHeader {
            stage: "test",
            sample_size_requested: 10,
            candidates_selected: 10,
            attempted: 4,
            stop_reason: Some("free disk space ... under the 3 GiB floor".into()),
            evidence_store: Path::new("ev"),
        }
        .into_map()
        .expect("clock");
        assert_eq!(header["complete"], false);
        assert_eq!(header["not_attempted"], 6);
        assert!(header["incomplete_reason"].as_str().is_some_and(|r| r.contains("floor")));
    }

    #[test]
    fn evidence_dir_resolution_order_is_flag_then_env_then_default() {
        let flag = Path::new("/flag");
        assert_eq!(resolve_evidence_dir(Some(flag), Some("/env".into())), PathBuf::from("/flag"));
        assert_eq!(resolve_evidence_dir(None, Some("/env".into())), PathBuf::from("/env"));
        assert_eq!(resolve_evidence_dir(None, Some("".into())), PathBuf::from(DEFAULT_EVIDENCE_DIR));
        assert_eq!(resolve_evidence_dir(None, None), PathBuf::from(DEFAULT_EVIDENCE_DIR));
    }
}
