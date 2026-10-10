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
use discovery::{
    Discovery, DiscoveryError, DiscoveryPath, EraProvenance, FallbackReason, ProbeAbsence,
    ProbeEvidence, RevisionSource,
};
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
    "era_provenance",
    "negotiated_spec_revision",
    "evidence",
    "tool_count",
    "server_pin",
    "pin_error",
    "annotations_object",
    "annotation_tally",
];

/// How the era was negotiated, as a census observation.
///
/// P0-11: a server can steer *which* era the harness records about it, by stalling or by
/// answering `server/discover` with garbage. Recording only `discovery_path` would make that
/// influence invisible in published distributions, so the offer, the choice and the fallback
/// reason travel with every record. Every field is re-validated on read-back, so the bound
/// holds for a results file this process did not write.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct EraRecord {
    policy: String,
    offered_revisions: Vec<String>,
    chosen_revision: String,
    /// Whether `chosen_revision` was negotiated with the server or assumed by the client —
    /// `None` for a record written before this was captured. Without it, a populated
    /// `chosen_revision` beside an empty `offered_revisions` reads as a contradiction.
    revision_source: Option<String>,
    fallback_reason: Option<String>,
    /// The probe response's evidence — the preimage of the whole era classification.
    probe: ProbeRecord,
}

/// The `server/discover` probe's evidence, as a census observation.
///
/// Three states, and the third is why this is a type rather than an `Option<Digest>`: "no
/// bytes ever existed" and "this record predates probe persistence" are different claims
/// about the evidence store, and neither is "bytes existed and were thrown away" — which
/// nothing may now produce (architecture.md §6 invariant 2: a derived field needs a
/// replayable preimage).
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum ProbeRecord {
    /// The probe bytes are in the evidence store under this digest.
    Captured(Digest),
    /// No bytes were ever read, for the stated reason.
    Absent(ProbeAbsence),
    /// Written before P0-11 persisted the probe. A fact about the file, not an error.
    Unrecorded,
}

impl ProbeRecord {
    fn of(evidence: &ProbeEvidence, digest: Option<Digest>) -> Self {
        match (evidence, digest) {
            (ProbeEvidence::Captured(_), Some(digest)) => Self::Captured(digest),
            // `persist` always stores the bytes when there are any, so the `None` arm is
            // unreachable from a live run; written out rather than unwrapped.
            (ProbeEvidence::Captured(_), None) => Self::Unrecorded,
            (ProbeEvidence::Absent(absence), _) => Self::Absent(*absence),
        }
    }

    /// Read one back out of an `era_provenance` block, re-validating rather than trusting:
    /// a digest is parsed strictly before it can name a path inside the store, and an
    /// unrecognised absence reason is dropped rather than copied through as a new bucket.
    fn from_json(value: Option<&Value>) -> Self {
        let Some(state) = value.and_then(|v| v.get("state")).and_then(Value::as_str) else {
            return Self::Unrecorded;
        };
        if state == "captured" {
            return value
                .and_then(|v| v.get("digest"))
                .and_then(Value::as_str)
                .and_then(Digest::from_hex)
                .map_or(Self::Unrecorded, Self::Captured);
        }
        ProbeAbsence::parse(state).map_or(Self::Unrecorded, Self::Absent)
    }

    /// This record as JSON. `bytes` is re-read from the store by the caller, so it is
    /// recomputed from the evidence on every derivation rather than carried through.
    fn to_json(&self, bytes: Option<usize>) -> Value {
        match self {
            Self::Captured(digest) => json!({
                "state": "captured",
                "digest": digest.to_string(),
                "bytes": bytes,
            }),
            Self::Absent(absence) => json!({ "state": absence.as_str() }),
            Self::Unrecorded => json!({ "state": "unrecorded" }),
        }
    }

    fn digest(&self) -> Option<&Digest> {
        match self {
            Self::Captured(digest) => Some(digest),
            Self::Absent(_) | Self::Unrecorded => None,
        }
    }
}

/// Cap on `offered_revisions` as re-read from a results file, matching the cap
/// `discovery::era` applies when observing. Each entry is additionally required to be
/// revision-shaped, which bounds it to ten bytes.
const MAX_OFFERED_REVISIONS: usize = 64;

impl EraRecord {
    fn of(provenance: &EraProvenance, probe: ProbeRecord) -> Self {
        Self {
            policy: provenance.policy.to_string(),
            offered_revisions: provenance.offered_revisions.clone(),
            chosen_revision: provenance.chosen_revision.to_string(),
            revision_source: Some(provenance.revision_source.as_str().to_string()),
            fallback_reason: provenance.fallback_reason.map(|r| r.as_str().to_string()),
            probe,
        }
    }

    /// What a record written before P0-11 implies: the era was not recorded, so it must not
    /// be reported as anything in particular.
    fn unrecorded() -> Self {
        Self {
            policy: "unrecorded".into(),
            offered_revisions: Vec::new(),
            chosen_revision: String::new(),
            revision_source: None,
            fallback_reason: None,
            probe: ProbeRecord::Unrecorded,
        }
    }

    /// Read one back out of a server record, re-applying every bound rather than trusting
    /// the file. An absent block is [`Self::unrecorded`]; a malformed field degrades to its
    /// empty value rather than failing the whole re-derivation.
    fn from_record(value: Option<&Value>) -> Self {
        let Some(value) = value else { return Self::unrecorded() };
        let text = |key: &str| {
            value.get(key).and_then(Value::as_str).map(|s| bounded(s, MAX_REVISION_CHARS))
        };
        Self {
            policy: text("policy").unwrap_or_else(|| "unrecorded".into()),
            offered_revisions: value
                .get("offered_revisions")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .filter(|s| discovery::era::is_revision_shaped(s))
                        .take(MAX_OFFERED_REVISIONS)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
            chosen_revision: text("chosen_revision")
                .filter(|s| discovery::era::is_revision_shaped(s))
                .unwrap_or_default(),
            // Closed taxonomies, both: anything the parser does not recognise is dropped
            // rather than copied through into a published distribution as a new bucket.
            revision_source: value
                .get("revision_source")
                .and_then(Value::as_str)
                .and_then(RevisionSource::parse)
                .map(|s| s.as_str().to_string()),
            fallback_reason: value
                .get("fallback_reason")
                .and_then(Value::as_str)
                .and_then(FallbackReason::parse)
                .map(|r| r.as_str().to_string()),
            probe: ProbeRecord::from_json(value.get("probe_raw")),
        }
    }

    fn to_json(&self, probe_bytes: Option<usize>) -> Value {
        json!({
            "policy": self.policy,
            "offered_revisions": self.offered_revisions,
            "chosen_revision": self.chosen_revision,
            "revision_source": self.revision_source.clone().map_or(Value::Null, Value::String),
            "fallback_reason": self.fallback_reason.clone().map_or(Value::Null, Value::String),
            "probe_raw": self.probe.to_json(probe_bytes),
        })
    }

    /// Was the revision on this record assumed by the client rather than negotiated with
    /// the server? See [`discovery::RevisionSource`].
    fn revision_is_assumed(&self) -> bool {
        self.revision_source.as_deref() == Some(RevisionSource::Assumed.as_str())
    }
}

/// What one discovery attempt observed.
pub(crate) enum Observation {
    /// Discovery returned both responses, and both were persisted.
    Discovered {
        /// Which handshake produced the result (P0-09 provenance).
        discovery_path: DiscoveryPath,
        /// How that era was decided (P0-11 provenance).
        era: EraRecord,
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
    ///
    /// `DiscoveryError::HttpStatus` is new in P0-11 and is handled so that the taxonomy does
    /// **not** change meaning. Before P0-11 every non-2xx arrived as
    /// `Transport("http status: NNN")` because `ureq`'s `http_status_as_error` was on; that
    /// flag had to be turned off to read a 4xx body at all (see
    /// `discovery::transport`), which would otherwise have let HTML error bodies drift into
    /// the `protocol` bucket and broken comparability with the July split (435 `transport`
    /// against 312 `protocol`). So the category stays keyed on the status and the detail
    /// string stays byte-identical — with one deliberate exception, `429`, which July could
    /// not tell apart from a dead host and now gets its own category.
    pub(crate) fn from_discovery_error(err: DiscoveryError, io_category: &str) -> Self {
        match err {
            DiscoveryError::Transport(msg) => Self::failed("transport", &msg),
            DiscoveryError::HttpStatus { status, retry_after } => Self::failed(
                http_status_category(status),
                &http_status_detail(status, retry_after.as_deref()),
            ),
            DiscoveryError::Io(e) => Self::failed(io_category, &e.to_string()),
            DiscoveryError::Protocol(msg) => Self::failed("protocol", &msg),
            DiscoveryError::ServerError { code, message } => {
                Self::failed("server_error", &format!("{code}: {message}"))
            }
        }
    }
}

/// The census failure category for an HTTP status.
///
/// `429` is its own category rather than part of `transport`: the July Class B results hold
/// two `http status: 429` responses that are indistinguishable there from dead hosts, which
/// makes self-inflicted throttling unmeasurable. Shared with `probe_stage1` so the two
/// sweeps cannot disagree about what a throttled host is called.
pub(crate) fn http_status_category(status: u16) -> &'static str {
    if status == 429 { "rate_limited" } else { "transport" }
}

/// The census failure *detail* for an HTTP status — `http status: NNN`, byte-identical to
/// what `ureq::Error::StatusCode` rendered through `Transport(e.to_string())` and therefore
/// to what the July results files literally contain.
///
/// The `Retry-After` suffix is appended for `429` **only**. Appending it for any status that
/// happened to carry the header produced details July never produced — a 503 with a
/// `Retry-After` recorded as `http status: 503 (retry-after: 30)`, silently un-comparable
/// with July's `http status: 503` — and `Retry-After` is a perfectly ordinary header on a 503
/// or a 3xx. `429` is the one status whose category already diverges from July on purpose,
/// so it is the one status where a diverging detail is intended rather than drift.
///
/// Shared with `probe_stage1`, like [`http_status_category`], so the two sweeps cannot
/// disagree about what a throttled host's record looks like.
pub(crate) fn http_status_detail(status: u16, retry_after: Option<&str>) -> String {
    match retry_after {
        Some(after) if status == 429 => format!("http status: {status} (retry-after: {after})"),
        _ => format!("http status: {status}"),
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

/// Write every raw response of a successful discovery into the evidence store.
///
/// Three blobs now, not two: the `server/discover` probe response joins the handshake and
/// the `tools/list`, wherever any probe bytes existed at all. The probe is the preimage of
/// the era classification, which is otherwise the one derived field in the system with
/// nothing to replay it against — and a dual-era server that answers the probe with garbage
/// is *choosing* which of its handlers the harness ends up talking to, which must be visible
/// in the evidence rather than only in the client's own `fallback_reason` assertion.
///
/// Content addressing makes the third blob nearly free: on the modern paths the probe bytes
/// and the handshake bytes are the same bytes, so they share one blob, and real probe bodies
/// are 100–300 bytes besides.
pub(crate) fn persist(store: &BlobStore, discovery: &Discovery) -> Result<Observation, StoreError> {
    let probe_digest = match discovery.probe_raw.bytes() {
        Some(bytes) => Some(store.put(bytes)?),
        None => None,
    };
    Ok(Observation::Discovered {
        discovery_path: discovery.discovery_path,
        era: EraRecord::of(
            &discovery.era_provenance,
            ProbeRecord::of(&discovery.probe_raw, probe_digest),
        ),
        handshake: store.put(&discovery.handshake_raw)?,
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
            Observation::Discovered { discovery_path, era, handshake, tools_list } => {
                derive_discovered(store, *discovery_path, era, handshake, tools_list, &mut record, &mut rollup)?;
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
    era: &EraRecord,
    handshake: &Digest,
    tools_list: &Digest,
    record: &mut Map<String, Value>,
    rollup: &mut Rollup,
) -> Result<(), StoreError> {
    let handshake_raw = store.get(handshake)?;
    let tools_list_raw = store.get(tools_list)?;
    // Read back, not carried through: the byte count in the file is recomputed from the
    // blob, so a tampered count is caught by the same re-derivation that catches a tampered
    // tally, and a missing probe blob fails loudly like any other missing evidence.
    let probe_bytes = match era.probe.digest() {
        Some(digest) => Some(store.get(digest)?.len()),
        None => None,
    };

    // Same parser the live client used to negotiate (`discovery::negotiated_spec_revision`),
    // which since P0-11 reads both eras: a legacy `result.protocolVersion`, or a modern
    // `DiscoverResult`'s `supportedVersions` re-intersected with the client's own allowlist.
    // `null` covers the one shape that carries neither — the `modern_without_discover` path,
    // whose handshake evidence is a 404 error body; `era_provenance.chosen_revision` is
    // where that record's *assumed* revision lives, labelled as assumed.
    let revision = discovery::negotiated_spec_revision(&handshake_raw)
        .ok()
        .map(|r| bounded(&r, MAX_REVISION_CHARS));
    rollup.record_discovered(
        discovery_path,
        era,
        revision.as_deref(),
        (handshake, handshake_raw.len()),
        (tools_list, tools_list_raw.len()),
        (era.probe.digest(), probe_bytes),
    );

    record.insert("discovery_path".into(), discovery_path.as_str().into());
    record.insert("era_provenance".into(), era.to_json(probe_bytes));
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
    era_policies: BTreeMap<String, usize>,
    fallback_reasons: BTreeMap<String, usize>,
    revisions: BTreeMap<String, usize>,
    // Evidence volume.
    handshake_bytes: u64,
    tools_list_bytes: u64,
    probe_bytes: u64,
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
        era: &EraRecord,
        revision: Option<&str>,
        handshake: (&Digest, usize),
        tools_list: (&Digest, usize),
        probe: (Option<&Digest>, Option<usize>),
    ) {
        self.discovered += 1;
        *self.discovery_paths.entry(path.as_str()).or_insert(0) += 1;
        *self.era_policies.entry(era.policy.clone()).or_insert(0) += 1;
        // Counted, not just stored per server: a corpus-wide spike in one reason is how a
        // coordinated attempt to steer the published era distribution would show up.
        *self
            .fallback_reasons
            .entry(era.fallback_reason.clone().unwrap_or_else(|| "none".into()))
            .or_insert(0) += 1;
        // Three distinct buckets, because "the server named this revision", "no revision was
        // ever negotiated" and "something was there and could not be read" are three
        // different findings. `<none>` used to be pooled into `<unparseable>`, which claimed
        // a parse failure where nothing had been there to parse — and it is specifically the
        // `modern_without_discover` path, where the revision is assumed, that lands here.
        let bucket = match revision {
            Some(revision) => revision,
            None if era.revision_is_assumed() || path == DiscoveryPath::ModernWithoutDiscover => {
                "<none>"
            }
            None => "<unparseable>",
        };
        *self.revisions.entry(bucket.to_string()).or_insert(0) += 1;
        self.handshake_bytes += handshake.1 as u64;
        self.tools_list_bytes += tools_list.1 as u64;
        self.probe_bytes += probe.1.unwrap_or(0) as u64;
        let blobs = [Some(handshake), Some(tools_list), probe.0.zip(probe.1)];
        for (digest, len) in blobs.into_iter().flatten() {
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
            DiscoveryPath::all().iter().map(|p| (p.as_str(), 0)).collect();
        discovery_paths.extend(self.discovery_paths);

        // Seeded with every reason plus `none`, for the same reason `discovery_path` is: this
        // distribution exists to make a coordinated steering attempt visible as a spike, and
        // a bucket that is merely absent cannot be told from one that is zero.
        let mut fallback_reasons: BTreeMap<String, usize> = FallbackReason::all()
            .iter()
            .map(|r| (r.as_str().to_string(), 0))
            .chain([("none".to_string(), 0)])
            .collect();
        fallback_reasons.extend(self.fallback_reasons);

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
                "era_policy": self.era_policies,
                "fallback_reason": fallback_reasons,
                "negotiated_spec_revision": self.revisions,
            },
            "evidence_summary": {
                "servers_with_evidence": self.discovered,
                "handshake_bytes": self.handshake_bytes,
                "tools_list_bytes": self.tools_list_bytes,
                "probe_bytes": self.probe_bytes,
                "bytes_referenced": self.handshake_bytes + self.tools_list_bytes + self.probe_bytes,
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
            // Absent in a file written before P0-11, which is a fact about the file rather
            // than an error: re-derivation still works, and reports the era as unrecorded.
            era: EraRecord::from_record(record.get("era_provenance")),
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

    /// A legacy-only server's `-32601` answer to the modern probe — the probe evidence on
    /// every [`DiscoveryPath::Initialize`] fixture below, and the commonest real shape.
    const LEGACY_PROBE: &str =
        r#"{"jsonrpc":"2.0","id":0,"error":{"code":-32601,"message":"method not found"}}"#;

    /// One discovery, with era provenance that **matches the path it claims**.
    ///
    /// The previous version of this helper gave every fixture
    /// `fallback_reason: Some(NonModernErrorBody)` — including the `server_discover` one,
    /// which cannot have fallen back at all. The cost was invisible and real: the `"none"`
    /// bucket that *every* successful modern discovery produces was never exercised by any
    /// report test, so no assertion covered the shape the modern path actually writes.
    fn discovery(path: DiscoveryPath, revision: &str, tools_json: &str) -> Discovery {
        let tools_list_raw =
            format!(r#"{{"jsonrpc":"2.0","id":1,"result":{{"tools":[{tools_json}]}}}}"#)
                .into_bytes();
        let legacy_handshake = format!(
            r#"{{"jsonrpc":"2.0","id":0,"result":{{"protocolVersion":"{revision}","capabilities":{{}},"serverInfo":{{"name":"fake","version":"0"}}}}}}"#
        )
        .into_bytes();
        let discover_result = format!(
            r#"{{"jsonrpc":"2.0","id":0,"result":{{"resultType":"complete","supportedVersions":["{revision}"],"capabilities":{{}},"ttlMs":0,"cacheScope":"private"}}}}"#
        )
        .into_bytes();
        // `2026-07-28`'s unknown-method shape: the handshake evidence on this path is an
        // error body carrying no revision at all, which is exactly why the record's revision
        // is `None`/assumed rather than re-derivable.
        let unknown_method = br#"{"jsonrpc":"2.0","id":0,"error":{"code":-32601,"message":"Method not found"}}"#.to_vec();

        let (handshake_raw, probe_raw, negotiated, offered, chosen, source, fallback) = match path {
            DiscoveryPath::Initialize => (
                legacy_handshake,
                ProbeEvidence::Captured(LEGACY_PROBE.as_bytes().to_vec()),
                Some(revision.to_string()),
                Vec::new(),
                "2025-11-25",
                RevisionSource::Negotiated,
                Some(FallbackReason::NonModernErrorBody),
            ),
            DiscoveryPath::ServerDiscover => (
                discover_result.clone(),
                // On a modern path the probe response *is* the handshake: one blob, two
                // references, which is what content addressing is for.
                ProbeEvidence::Captured(discover_result),
                Some(revision.to_string()),
                vec![revision.to_string()],
                "2026-07-28",
                RevisionSource::Negotiated,
                None,
            ),
            DiscoveryPath::ModernWithoutDiscover => (
                unknown_method.clone(),
                ProbeEvidence::Captured(unknown_method),
                None,
                Vec::new(),
                "2026-07-28",
                RevisionSource::Assumed,
                None,
            ),
        };

        Discovery {
            handshake_raw,
            tools_list_raw,
            probe_raw,
            negotiated_spec_revision: negotiated,
            discovery_path: path,
            era_provenance: EraProvenance {
                policy: "modern_first",
                offered_revisions: offered,
                chosen_revision: chosen,
                revision_source: source,
                fallback_reason: fallback,
            },
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
        // The optimistic-modern path: its handshake evidence is a 404 error body, so there is
        // no revision in the bytes to re-derive and the record must land in the `<none>`
        // revision bucket rather than be mislabelled `<unparseable>`.
        push(
            "modern-without-discover",
            record_discovery(
                &store,
                &discovery(DiscoveryPath::ModernWithoutDiscover, "2026-07-28", ""),
                &stop,
            ),
        );
        assert_eq!(stop.check(), Ok(()), "no store fault in a healthy run");

        let header = RunHeader {
            stage: "test",
            sample_size_requested: 10,
            candidates_selected: 9,
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
        assert_eq!(outcome.servers, 9);
        assert_eq!(outcome.servers_with_evidence, 8);
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
            // A legacy-only server, which under P0-11's modern-first policy is probed with
            // `server/discover` before anything else and answers it the way every pre-
            // `2026-07-28` SDK does: JSON-RPC `-32601`. Request ids, in order: 0 the probe,
            // 1 `initialize`, then the id-less `notifications/initialized`, then 2
            // `tools/list`.
            let probe = r#"{"jsonrpc":"2.0","id":0,"error":{"code":-32601,"message":"method not found"}}"#;
            let init = format!(
                r#"{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":"{revision}","capabilities":{{}},"serverInfo":{{"name":"fake","version":"0"}}}}}}"#
            );
            let list = format!(r#"{{"jsonrpc":"2.0","id":2,"result":{{"tools":[{tools}]}}}}"#);
            format!(
                "sleep {}; read -r _; printf '%s\\n' '{probe}'; read -r _; printf '%s\\n' '{init}'; \
                 read -r _; read -r _; printf '%s\\n' '{list}'",
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
        for body in [ALL, NONE, SOME, "", UNPINNABLE, NONE, ""] {
            every_tool.extend(coverage::tool_coverage(&discovery(DiscoveryPath::Initialize, "x", body).tools_list_raw).expect("parse"));
        }
        assert_eq!(report["tools_discovered"], every_tool.len());
        assert_eq!(report["annotation_tally"], tally_json(&coverage::tally(&every_tool)));
        assert_eq!(report["attempted"], 9);
        assert_eq!(report["succeeded"], 7);
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
        assert_eq!(weighted["succeeded_with_zero_tools"], 2);
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

        assert_eq!(report["provenance"]["servers"], 8);
        // Every `DiscoveryPath` variant is seeded with an explicit zero, so a path a run
        // happened not to take is visibly absent rather than silently missing from the map.
        assert_eq!(
            report["provenance"]["discovery_path"],
            json!({ "initialize": 6, "server_discover": 1, "modern_without_discover": 1 })
        );
        // Three revision buckets, not two. `<none>` is the `modern_without_discover` record:
        // no revision was negotiated, so there is nothing to parse — which is a different
        // finding from `<unparseable>`, where something was there and could not be read.
        assert_eq!(
            report["provenance"]["negotiated_spec_revision"],
            json!({ "2025-11-25": 6, "2026-07-28": 1, "<none>": 1 })
        );
        // P0-11 provenance: the policy that produced these records, and why any fallback
        // happened. One policy across all of them, which is the point — a mixed-policy
        // report would be the thing that must never be pooled silently.
        assert_eq!(report["provenance"]["era_policy"], json!({ "modern_first": 8 }));
        // Seeded with every reason plus `none`, for the same reason `discovery_path` is:
        // this distribution exists so a steering attempt shows up as a spike, and an absent
        // bucket cannot be told from a zero one. `none` is what every successful modern
        // discovery produces, and it is exercised here precisely because the fixtures now
        // give each path the provenance it can actually have.
        assert_eq!(
            report["provenance"]["fallback_reason"],
            json!({
                "none": 2,
                "no_response": 0,
                "non_modern_error_body": 6,
                "malformed_response": 0,
                "only_legacy_revisions_offered": 0,
                "only_legacy_revisions_after_unsupported_version": 0,
                "modern_without_discover_tools_list_failed": 0,
            })
        );

        let zero = &servers[3];
        assert_eq!(zero["discovery_path"], "server_discover");
        assert_eq!(zero["negotiated_spec_revision"], "2026-07-28");
        assert_eq!(zero["annotations_object"], "no_tools");
        assert_eq!(zero["era_provenance"]["policy"], "modern_first");
        assert_eq!(zero["era_provenance"]["offered_revisions"], json!(["2026-07-28"]));
        assert_eq!(zero["era_provenance"]["chosen_revision"], "2026-07-28");
        assert_eq!(zero["era_provenance"]["revision_source"], "negotiated");
        assert_eq!(zero["era_provenance"]["fallback_reason"], Value::Null);
        // The probe's own bytes, kept. On a modern path they *are* the handshake bytes, so
        // the digest matches and content addressing stores them once.
        assert_eq!(zero["era_provenance"]["probe_raw"]["state"], "captured");
        assert_eq!(
            zero["era_provenance"]["probe_raw"]["digest"],
            zero["evidence"]["handshake_raw"]["digest"],
            "a DiscoverResult is both the probe response and the handshake"
        );

        // The optimistic-modern record: a revision this client *assumed*, labelled as such,
        // next to an empty offer — which is coherent only because `revision_source` says so.
        let assumed = &servers[8];
        assert_eq!(assumed["discovery_path"], "modern_without_discover");
        assert_eq!(
            assumed["negotiated_spec_revision"], Value::Null,
            "nothing was negotiated, so nothing is published as negotiated"
        );
        assert_eq!(assumed["era_provenance"]["revision_source"], "assumed");
        assert_eq!(assumed["era_provenance"]["chosen_revision"], "2026-07-28");
        assert_eq!(assumed["era_provenance"]["offered_revisions"], json!([]));
        assert_eq!(assumed["era_provenance"]["probe_raw"]["state"], "captured");

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
        assert_eq!(summary["servers_with_evidence"], 8);
        assert!(summary["bytes_distinct"].as_u64() < summary["bytes_referenced"].as_u64());
        assert!(
            summary["probe_bytes"].as_u64().is_some_and(|b| b > 0),
            "probe evidence is counted in the volume summary like any other blob"
        );
    }

    /// **The comparability requirement P0-11 had to not break.** Turning off
    /// `http_status_as_error` was mandatory to read a 4xx body at all, and it would
    /// otherwise have moved HTTP-level failures into the `protocol` bucket, making the July
    /// split (435 `transport` against 312 `protocol`) incomparable with every later run. The
    /// category stays keyed on the status, and the detail string stays byte-identical to what
    /// `ureq::Error::StatusCode` rendered — which is literally what the July Class B results
    /// file contains.
    #[test]
    fn an_http_status_failure_keeps_the_july_transport_category_and_detail_string() {
        for status in [400u16, 401, 403, 404, 500, 502] {
            let observation = Observation::from_discovery_error(
                DiscoveryError::HttpStatus { status, retry_after: None },
                "io",
            );
            let Observation::Failed { category, detail } = observation else {
                panic!("an HTTP status failure is a Failed observation");
            };
            assert_eq!(category, "transport", "status {status} must stay in the July bucket");
            assert_eq!(detail, format!("http status: {status}"));
        }
    }

    /// `429` is the one deliberate departure: July recorded two of them as plain
    /// `transport`, indistinguishable from a dead host, which makes self-inflicted throttling
    /// unmeasurable. It gets its own category, and `Retry-After` is carried so a sweep can
    /// skip the host instead of retrying it.
    #[test]
    fn a_429_is_its_own_failure_category_and_never_pooled_into_transport() {
        let observation = Observation::from_discovery_error(
            DiscoveryError::HttpStatus { status: 429, retry_after: Some("120".into()) },
            "io",
        );
        let Observation::Failed { category, detail } = observation else {
            panic!("a 429 is a Failed observation");
        };
        assert_eq!(category, "rate_limited");
        assert_ne!(category, "transport", "pooling this back into transport is the regression");
        assert_eq!(detail, "http status: 429 (retry-after: 120)");
        // And the sweeps must agree on the name, since they share one mapping.
        assert_eq!(http_status_category(429), "rate_limited");
        assert_eq!(http_status_category(503), "transport");
    }

    /// The `Retry-After` suffix belongs to `429` and nothing else.
    ///
    /// `Retry-After` is a perfectly ordinary header on a 503 or a 3xx, and appending the
    /// suffix for any status that carried one produced details July never produced — a 503
    /// recorded as `http status: 503 (retry-after: 30)` where July recorded
    /// `http status: 503`, silently un-comparable. The old tests could not catch it because
    /// every one of them passed `retry_after: None`, so this one passes a value for each
    /// status it checks.
    #[test]
    fn the_retry_after_suffix_is_appended_for_429_only() {
        for status in [301u16, 400, 403, 404, 500, 502, 503] {
            let observation = Observation::from_discovery_error(
                DiscoveryError::HttpStatus { status, retry_after: Some("30".into()) },
                "io",
            );
            let Observation::Failed { detail, .. } = observation else {
                panic!("an HTTP status failure is a Failed observation");
            };
            assert_eq!(
                detail,
                format!("http status: {status}"),
                "status {status} carrying a Retry-After must still read exactly as July's \
                 detail string, with no suffix"
            );
            assert_eq!(http_status_detail(status, Some("30")), detail, "and via the helper");
        }
        // The one status where a diverging detail is intended rather than drift.
        assert_eq!(http_status_detail(429, Some("30")), "http status: 429 (retry-after: 30)");
        assert_eq!(http_status_detail(429, None), "http status: 429");
    }

    /// Era provenance read back out of a results file is re-validated rather than trusted:
    /// a file is as untrusted as the server whose bytes produced it (design.md §3), and
    /// these values become map keys in a published distribution.
    #[test]
    fn era_provenance_read_back_from_a_file_is_revalidated_and_bounded() {
        let hostile = json!({
            "policy": "x".repeat(10_000),
            "offered_revisions": (0..500).map(|i| json!(format!("2026-07-{:02}", i % 28 + 1)))
                .chain([json!("not-a-revision"), json!(12345), json!({"a": 1})])
                .collect::<Vec<_>>(),
            "chosen_revision": "../../etc/passwd",
            "fallback_reason": "invented_reason",
            "revision_source": "invented_source",
            "probe_raw": { "state": "captured", "digest": "../../../../etc/passwd" },
        });
        let record = EraRecord::from_record(Some(&hostile));
        assert!(record.policy.chars().count() <= MAX_REVISION_CHARS + 40, "{}", record.policy);
        assert!(record.offered_revisions.len() <= MAX_OFFERED_REVISIONS);
        assert!(record.offered_revisions.iter().all(|r| discovery::era::is_revision_shaped(r)));
        assert_eq!(record.chosen_revision, "", "an unshaped revision is dropped, not echoed");
        assert_eq!(record.fallback_reason, None, "the reason taxonomy is closed");
        assert_eq!(record.revision_source, None, "so is the revision-source taxonomy");
        assert_eq!(
            record.probe, ProbeRecord::Unrecorded,
            "a probe digest is parsed strictly before it could ever name a path, and a \
             rejected one degrades to `unrecorded` rather than to a read outside the store"
        );
        // And the absence taxonomy is closed too, with the real values accepted.
        for (state, expected) in [
            ("no_response", ProbeRecord::Absent(ProbeAbsence::NoResponse)),
            ("body_not_read", ProbeRecord::Absent(ProbeAbsence::BodyNotRead)),
            ("invented_state", ProbeRecord::Unrecorded),
        ] {
            assert_eq!(
                ProbeRecord::from_json(Some(&json!({ "state": state }))),
                expected,
                "probe state {state:?}"
            );
        }

        // A record written before P0-11 has no era block at all. That is a fact about the
        // file, not an error: re-derivation still works and reports the era as unrecorded.
        assert_eq!(EraRecord::from_record(None), EraRecord::unrecorded());
        let unrecorded = EraRecord::unrecorded();
        assert_eq!(unrecorded.policy, "unrecorded");
        assert_eq!(unrecorded.fallback_reason, None);
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
