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

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use census::coverage::{self, AnnotationTally, Tally};
use datamodel::{Annotation, Digest};
use discovery::{
    Discovery, DiscoveryError, DiscoveryPath, EraProvenance, FallbackReason, ProbeAbsence,
    ProbeEvidence, RevisionSource, StopReason, ToolPin, TruncationAttribution,
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
    "pagination",
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
    "pagination",
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

/// Why a `tools/list` cursor walk stopped, as a census observation (P0-13).
///
/// Carried through verbatim rather than recomputed, for the same reason `discovery_path` is:
/// two of the ten reasons ([`StopReason::BudgetExhausted`] and
/// [`StopReason::PageRequestFailed`]) are facts about the *run*, not about any stored page,
/// so no amount of re-reading the evidence could recover them. Everything else about a walk
/// — pages, tool counts, bytes, duplicate names — **is** derivable from the stored pages and
/// is recomputed, which is what keeps P0-10's single-derivation-path property intact.
///
/// Three states, and the third is why this is a type rather than an `Option<StopReason>`:
/// "this file predates cursor following" and "this walk was complete" are different claims,
/// and the difference decides whether a server's coverage numbers can be trusted.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum PaginationRecord {
    /// The walk's own stop reason.
    Recorded(StopReason),
    /// Written before P0-13 followed `nextCursor`. A fact about the file, not an error — and
    /// specifically **not** equivalent to `no_cursor`: such a record's tool list is page one,
    /// and whether the server had more pages is unknown and unknowable from it.
    Unrecorded,
}

impl PaginationRecord {
    /// Read a stop reason back out of a server record's `pagination` block.
    fn from_record(value: Option<&Value>) -> Self {
        value
            .and_then(|v| v.get("stopped_because"))
            .and_then(Value::as_str)
            .and_then(StopReason::parse)
            .map_or(Self::Unrecorded, Self::Recorded)
    }

    /// Stable string form for the published record and for the corpus distribution.
    fn as_str(self) -> &'static str {
        match self {
            Self::Recorded(reason) => reason.as_str(),
            Self::Unrecorded => "unrecorded",
        }
    }

    /// Whether the walk this record describes saw the server's whole tool list.
    ///
    /// `false` for [`Self::Unrecorded`], and that is the load-bearing choice: a record from
    /// before cursor following cannot assert completeness, and treating "unknown" as
    /// "complete" is the exact substitution this project refuses one layer up when it
    /// insists on `unverifiable` over `holds`.
    fn cursors_exhausted(self) -> bool {
        matches!(self, Self::Recorded(reason) if reason.cursors_exhausted())
    }

    /// Who truncated this walk, as published (B3) — the four
    /// [`TruncationAttribution`]s, `not_truncated`, or `unrecorded` for a file written
    /// before cursor following, which nobody truncated and nobody can attribute.
    fn attribution_str(self) -> &'static str {
        match self {
            Self::Recorded(reason) => reason.attribution().as_str(),
            Self::Unrecorded => "unrecorded",
        }
    }

    /// The failure category this walk is recorded under when it is truncated.
    fn truncated_category(self) -> &'static str {
        match self {
            Self::Recorded(reason) => truncated_category(reason.attribution()),
            Self::Unrecorded => TOOLS_LIST_TRUNCATED_UNRECORDED,
        }
    }

    /// Whether the *server* truncated this walk with an unfollowable cursor.
    fn is_server_nonconformance(self) -> bool {
        matches!(self, Self::Recorded(reason) if reason.attribution().is_server_nonconformance())
    }
}

/// Why a results file could not be derived from its evidence.
///
/// Two kinds, kept apart: the store failed (a missing or corrupt blob — P0-10's "the
/// evidence no longer reproduces this file"), or the file and its evidence describe
/// something no live capture could have produced, which is how a crafted bundle is refused
/// (B4) rather than derived from at whatever cost it chooses.
#[derive(Debug)]
pub(crate) enum DeriveError {
    /// The evidence store failed.
    Store(StoreError),
    /// The record or its evidence exceeds what any capture can produce.
    Malformed(String),
}

impl From<StoreError> for DeriveError {
    fn from(e: StoreError) -> Self {
        Self::Store(e)
    }
}

impl std::fmt::Display for DeriveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(e) => write!(f, "{e}"),
            Self::Malformed(msg) => write!(f, "malformed evidence: {msg}"),
        }
    }
}

impl std::error::Error for DeriveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Store(e) => Some(e),
            Self::Malformed(_) => None,
        }
    }
}

/// What one discovery attempt observed.
pub(crate) enum Observation {
    /// Discovery returned its handshake and at least one `tools/list` page, and all were
    /// persisted.
    Discovered {
        /// Which handshake produced the result (P0-09 provenance).
        discovery_path: DiscoveryPath,
        /// How that era was decided (P0-11 provenance).
        era: EraRecord,
        /// The handshake response's evidence digest.
        handshake: Digest,
        /// Every `tools/list` page's evidence digest, in walk order. Non-empty.
        tools_list_pages: Vec<Digest>,
        /// Why the cursor walk stopped (P0-13).
        pagination: PaginationRecord,
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
    ///
    /// P0-12's per-server `pacing` block rides here too. It is an observation of the
    /// *harness's own conduct* rather than of the server, so there is nothing in the
    /// evidence store to recompute it from, and carrying it verbatim is what lets
    /// [`rederive`] reproduce it byte for byte — the same treatment `generated_at_unix`
    /// gets in the header.
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
    // One blob per `tools/list` page (P0-13), in walk order. One blob per page rather than a
    // concatenation: the store is content-addressed over *bytes as received*, and splicing N
    // responses into one artifact would be a transformation at capture time — which ADR-005
    // puts on the far side of the normaliser, not here. It also means two servers (or one
    // server twice) returning an identical page share that blob for free, which is how the
    // cost of keeping N pages stays close to the cost of keeping one.
    let mut tools_list_pages = Vec::with_capacity(discovery.tools_list_pages.len());
    for page in &discovery.tools_list_pages {
        tools_list_pages.push(store.put(page)?);
    }
    Ok(Observation::Discovered {
        discovery_path: discovery.discovery_path,
        era: EraRecord::of(
            &discovery.era_provenance,
            ProbeRecord::of(&discovery.probe_raw, probe_digest),
        ),
        handshake: store.put(&discovery.handshake_raw)?,
        tools_list_pages,
        pagination: PaginationRecord::Recorded(discovery.pagination.stopped_because),
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
    // `into_inner` on poison, not `expect`: an attempt that panicked while holding this
    // lock must cost that one attempt, not every later one *and* the results file. The blob
    // store behind it is append-only and content-addressed, so a half-finished `put` leaves
    // no observable state to be confused by (F-05 writes temp-then-rename).
    let result = persist(
        &store.lock().unwrap_or_else(std::sync::PoisonError::into_inner),
        discovery,
    );
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
    /// The seed the sample selection and admission order were drawn under, for a stage that
    /// samples. `None` for a stage that does not.
    ///
    /// Published because it is the whole point: a sweep must stay exactly reproducible for
    /// anyone holding this file, while not being precomputable by anyone before it ran. An
    /// unpublished seed would buy the second property by destroying the first.
    pub(crate) sample_seed: Option<u64>,
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
            // Rendered as a decimal string, not a JSON number: a `u64` seed can exceed
            // 2^53 and would then be silently rounded by any JSON reader backed by doubles,
            // which would make the published seed fail to reproduce the sample it describes.
            "sample_seed": self.sample_seed.map(|s| format!("{s:#018x}")),
        }) else {
            unreachable!("json! of an object literal is an object")
        };
        Ok(map)
    }
}

// ---- P0-12: pacing, made auditable rather than asserted ----

/// How many per-domain rows a results file lists under `pacing.registrable_domains`.
///
/// Bounded because the key is server-influenceable: at 1,000 hosts the tail is hundreds of
/// rows with one request each, which says nothing a reader needs. The rows that matter for
/// auditing a politeness claim are the most-loaded ones, and `max_requests_to_one_
/// registrable_domain` plus `requests_sent` already bound the whole distribution.
const MAX_LISTED_DOMAINS: usize = 25;

/// The `pacing` header block for a sweep that paced its requests.
///
/// Both halves are here on purpose. The *policy* (interval, cap, reservation, how the key is
/// derived) is what a reader needs to know what was promised; the *accounting* (requests
/// sent, time waited, domains contacted, the most-loaded domains, every refusal) is what
/// they need to check it was kept. This project's convention is that a claim nobody can
/// check is not a finding, and "the sweep was polite" is exactly such a claim.
pub(crate) fn pacing_header(accounting: &discovery::Accounting) -> Value {
    let domains: Vec<Value> = accounting
        .domains
        .iter()
        .take(MAX_LISTED_DOMAINS)
        .map(|d| {
            json!({
                "registrable_domain": d.registrable_domain,
                "requests": d.requests,
                "sequences_admitted": d.sequences_admitted,
                "sequences_denied": d.sequences_denied,
                // B2. Beside `sequences_denied`, these let a reader see one host's walk
                // displacing other sequences on its domain rather than infer it: what
                // pagination took from the shared pool, what it was refused, how much of that
                // was the per-sequence share, and the most any one sequence held.
                "page_extensions_granted": d.page_extensions_granted,
                "page_extensions_denied": d.page_extensions_denied,
                "page_extensions_denied_by_sequence_share":
                    d.page_extensions_denied_by_sequence_share,
                "largest_sequence_reservation": d.largest_sequence_reservation,
            })
        })
        .collect();
    json!({
        "applies": true,
        "grouping": PACE_GROUPING,
        "min_interval_ms": millis(accounting.policy.min_interval),
        "max_requests_per_registrable_domain":
            accounting.policy.max_requests_per_registrable_domain,
        "per_discovery_reservation": accounting.per_discovery_reservation,
        // B2: the most one sequence may hold, page extensions included —
        // `max(reservation, cap / 8)`. Part of the policy, because it bounds how many
        // co-tenants one paginating listing can displace.
        "max_reservation_per_sequence": accounting.max_reservation_per_sequence,
        // Part of the policy, not a curiosity: a followed redirect is a request this block
        // cannot see. ureq's default is 10, so before B1 one POST could become 11 requests
        // with no spacing between them and only the first charged, which would make every
        // number below an undercount of real load by up to 11x. Published so that the
        // claim "these counts are the requests this sweep sent" is checkable rather than
        // assumed.
        "max_redirects": discovery::MAX_REDIRECTS,
        "requests_sent": accounting.requests_sent,
        "waited_ms_total": millis(accounting.waited),
        "registrable_domains_contacted": accounting.registrable_domains_contacted,
        "registrable_domains_seen": accounting.domains.len(),
        "max_requests_to_one_registrable_domain":
            accounting.max_requests_to_one_registrable_domain,
        "sequences_denied_by_budget": accounting.sequences_denied,
        // P0-13. Its own field, never pooled into `sequences_denied_by_budget`: a refused
        // *sequence* means a host was never contacted, while a refused page-reservation
        // means a host was contacted and its tool list came back **truncated**. The second
        // is a caveat on that server's coverage numbers; the first is their absence. Each
        // refusal here has a matching `tools_list_truncated` server record whose
        // `pagination.stopped_because` is `budget_exhausted`, and
        // `verify_pacing_self_consistent` checks that correspondence.
        "page_extensions_denied": accounting.page_extensions_denied,
        "overshoot_requests": accounting.overshoot_requests,
        "registrable_domains_listed": domains.len(),
        "registrable_domains": domains,
    })
}

/// How [`discovery::registrable_domain`] derives its key, named in the results header so a
/// reader can reproduce the grouping without reading the source. Versioned, because changing
/// the derivation changes what every published per-domain row means.
const PACE_GROUPING: &str = "heuristic-v1 (ADR-014): last two host labels, or three when the \
                             second-to-last is a generic registry second level; an IP literal \
                             is its own key";

/// The `pacing` header block for a sweep where request pacing does not apply, with the
/// reason stated rather than the block merely absent.
///
/// An absent block and a block saying "not applicable" are different claims about a results
/// file — the same distinction P0-11 drew for probe evidence (`unrecorded` versus
/// `no_response`). A reader auditing politeness across both census stages must be able to
/// tell "this sweep paced nothing" from "this sweep is older than pacing".
pub(crate) fn pacing_not_applicable(reason: &str) -> Value {
    json!({ "applies": false, "reason": reason })
}

/// A duration in whole milliseconds as a `u64`.
///
/// `Duration::as_millis` is a `u128`, and `serde_json` *fails to serialise* a `u128` above
/// `u64::MAX` — which would abort the write of an entire results file rather than record one
/// odd number. Saturating here keeps that impossible by construction.
fn millis(d: std::time::Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// One server's pacing accounting, for its record in the results file.
///
/// `reservation` is new in P0-13 and is what keeps the header's `overshoot_requests`
/// checkable. A paginating walk *extends* its reservation one page at a time
/// (`Permit::try_extend`), so overshoot is no longer `requests_sent − a single constant`;
/// deriving it from the header's `per_discovery_reservation` would report every paginating
/// server as overshoot and fail the guard on a correct file. Publishing the reservation the
/// sequence actually ended up holding keeps the derivation a subtraction a reader can do by
/// hand, which is the only reason the guard is worth anything.
pub(crate) fn pacing_record(
    registrable_domain: &str,
    requests: usize,
    waited: std::time::Duration,
    reservation: usize,
) -> Value {
    json!({
        "registrable_domain": registrable_domain,
        "requests_sent": requests,
        "waited_ms": millis(waited),
        "reservation": reservation,
    })
}

/// Check a results file's `pacing` header against its own per-server records.
///
/// `Ok(())` when there is no pacing block, when pacing does not apply, or when every header
/// field that is **derivable** from the per-server rows matches them exactly. Otherwise the
/// mismatches, named.
///
/// Six fields are checked, not three. The three totals (`requests_sent`, `waited_ms_total`,
/// `sequences_denied_by_budget`) were always checked; the three added here are the ones a
/// reader actually quotes:
///
/// - **`max_requests_to_one_registrable_domain`** — the headline politeness number, the
///   one that substantiates "no third party received more than N requests". It was published
///   unchecked, which is the worst combination: load-bearing in a summary and unverified
///   against the data beneath it. It is the maximum, over domains, of the per-server
///   `requests_sent` grouped by per-server `registrable_domain`.
/// - **`registrable_domains_contacted`** — distinct domains with at least one request.
/// - **`overshoot_requests`** — `Σ max(0, requests_sent − reservation)` over the records,
///   using each record's **own** `pacing.reservation` (P0-13) and falling back to the
///   header's `per_discovery_reservation` for a P0-12-era file that has none. A paginating
///   walk extends its reservation rather than exceeding it, so subtracting the constant
///   would report every paginating server as overshoot and fail this guard on a correct
///   file; a published zero means "nothing sent a request nobody reserved for it", which is
///   what the number is for.
///
/// A seventh field is checked since P0-13:
///
/// - **`page_extensions_denied`** — checked as a **lower bound**, not an equality: it must
///   be at least the number of server records whose `pagination.stopped_because` is
///   `budget_exhausted`, since each such record is one refusal. It cannot be an equality,
///   for two reasons that each produced a false rejection of a correct file before this
///   was relaxed: (1) a record can lose its `pagination` block to a *later* failure — a walk
///   refused an extension and then hit `evidence_store_error` is recorded with no block at
///   all, yet its refusal is in the header; and (2) one record can account for **more than
///   one** refusal — `census-pin-stability` runs two walks under one permit, publishes this
///   counter, and writes no `pagination` block at all. What the bound still catches is the
///   direction that matters: a header claiming fewer refusals than its own records show
///   budget truncations.
///
/// The overshoot derivation assumes **one permit per server record**, which every sweep
/// writing this block holds by construction (Stage 1 admits once per candidate;
/// `pin_stability` deliberately covers both its sequences with a single permit for exactly
/// this reason). It does **not** assume one *walk* per record — that is the
/// `page_extensions_denied` case above. A file that ever breaks the one-permit assumption
/// must not publish this block unchanged.
///
/// Deliberately independent of [`pacing_header`], in the shape B-03's
/// `verify_report_matches_records` established: a checker that shares the aggregator's code
/// can only confirm the aggregator is self-consistent, never that the published numbers
/// describe the records beneath them.
pub(crate) fn verify_pacing_self_consistent(report: &Value) -> Result<(), Vec<String>> {
    let Some(pacing) = report.get("pacing") else { return Ok(()) };
    if pacing.get("applies").and_then(Value::as_bool) != Some(true) {
        return Ok(());
    }
    let servers = report.get("servers").and_then(Value::as_array).map_or(&[][..], |a| a.as_slice());
    let reservation =
        pacing.get("per_discovery_reservation").and_then(Value::as_u64).unwrap_or(0);

    let mut requests = 0u128;
    let mut waited = 0u128;
    let mut denied = 0u128;
    let mut overshoot = 0u128;
    let mut pages_refused = 0u128;
    let mut per_domain: BTreeMap<&str, u128> = BTreeMap::new();
    for server in servers {
        let n = |key: &str| {
            server.pointer(&format!("/pacing/{key}")).and_then(Value::as_u64).unwrap_or(0)
        };
        let sent = n("requests_sent");
        requests += u128::from(sent);
        waited += u128::from(n("waited_ms"));
        // The sequence's *own* reservation (P0-13), falling back to the header constant for a
        // P0-12-era file that has no per-server value. A paginating walk extends its
        // reservation, so subtracting the constant would count every extra page as overshoot
        // and fail this guard on a perfectly correct file.
        let own_reservation = server
            .pointer("/pacing/reservation")
            .and_then(Value::as_u64)
            .unwrap_or(reservation);
        overshoot += u128::from(sent.saturating_sub(own_reservation));
        if let Some(domain) =
            server.pointer("/pacing/registrable_domain").and_then(Value::as_str)
        {
            *per_domain.entry(domain).or_insert(0) += u128::from(sent);
        }
        if server.get("failure_category").and_then(Value::as_str) == Some(BUDGET_EXHAUSTED) {
            denied += 1;
        }
        // Each budget-truncated record is one refusal; the header may hold more (see the
        // doc comment: records that lost their block, and records covering two walks).
        if server.pointer("/pagination/stopped_because").and_then(Value::as_str)
            == Some(StopReason::BudgetExhausted.as_str())
        {
            pages_refused += 1;
        }
    }
    let max_to_one_domain = per_domain.values().copied().max().unwrap_or(0);
    let domains_contacted = per_domain.values().filter(|n| **n > 0).count() as u128;

    let mut problems = Vec::new();
    let claimed = |key: &str| pacing.get(key).and_then(Value::as_u64).map(u128::from);
    for (key, summed) in [
        ("requests_sent", requests),
        ("waited_ms_total", waited),
        ("sequences_denied_by_budget", denied),
        ("max_requests_to_one_registrable_domain", max_to_one_domain),
        ("registrable_domains_contacted", domains_contacted),
        ("overshoot_requests", overshoot),
    ] {
        match claimed(key) {
            Some(value) if value == summed => {}
            Some(value) => problems.push(format!(
                "pacing.{key} is {value} but the per-server records give {summed}"
            )),
            None => problems.push(format!("pacing.{key} is missing or not a number")),
        }
    }
    match claimed("page_extensions_denied") {
        Some(value) if value >= pages_refused => {}
        Some(value) => problems.push(format!(
            "pacing.page_extensions_denied is {value} but {pages_refused} per-server records \
             were truncated by budget_exhausted, each of which is one refusal"
        )),
        None => problems.push("pacing.page_extensions_denied is missing or not a number".into()),
    }
    if problems.is_empty() { Ok(()) } else { Err(problems) }
}

/// Failure category for a host refused because its registrable domain's request budget is
/// spent (P0-12).
///
/// A **new** category, which is what keeps July comparability: `transport`, `protocol`,
/// `rate_limited` and the tool-weighted fields all keep their meaning, and a refusal lands
/// in a bucket July never had rather than being pooled into one it did. It is also a
/// harness-side decision rather than a finding about the server — the same kind of thing as
/// P0-10's `evidence_store_error` — so a reader must be able to subtract it, which a
/// distinct name is what makes possible.
pub(crate) const BUDGET_EXHAUSTED: &str = "registrable_domain_budget_exhausted";

/// Failure category for an attempt whose worker panicked.
///
/// Like [`BUDGET_EXHAUSTED`] and P0-10's `evidence_store_error`, this is a **harness-side**
/// outcome rather than a finding about the server, so it is its own subtractable bucket and
/// never pooled into one July had.
///
/// It exists because the alternative was losing the run. `sweep::run_bounded` runs its
/// workers in a `thread::scope`, which re-panics on join: a panic in one attempt unwound
/// past the header insert and past [`finish`], so nothing was written at all and every host
/// contacted up to that point was contacted for no artifact. Catching per attempt turns a
/// single point of total loss into one record saying what happened — which, for a
/// harness whose inputs are assumed hostile (design.md §3), is the only acceptable shape
/// even though no reachable panic on server-controlled data is known today.
pub(crate) const HARNESS_PANIC: &str = "harness_panic";

/// Run one attempt, converting a panic into a [`HARNESS_PANIC`] observation.
///
/// The payload is sanitised and length-bounded exactly as server-authored text is
/// elsewhere in this module: a panic message can embed bytes a server supplied (a formatted
/// URL, a response fragment), and this string lands in a published results file.
///
/// The default panic hook still prints its own, unsanitised line to stderr before this sees
/// anything; that is pre-existing and out of scope here, and the same hazard
/// `discovery::transport`'s child-stderr relay already addresses for the one stream where
/// server text was known to flow.
pub(crate) fn guard_attempt(
    label: &str,
    attempt: impl FnOnce() -> ServerObservation,
) -> ServerObservation {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(attempt)) {
        Ok(observation) => observation,
        Err(payload) => {
            let raw = payload
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "panic payload was not a string".to_string());
            let safe: String = raw
                .chars()
                .filter(|c| *c == ' ' || *c == '\t' || (c.is_ascii_graphic()))
                .take(MAX_PANIC_DETAIL_CHARS)
                .collect();
            eprintln!("{label}: ATTEMPT PANICKED, recorded as {HARNESS_PANIC} — {safe}");
            ServerObservation {
                identity: Map::new(),
                observation: Observation::failed(HARNESS_PANIC, &safe),
            }
        }
    }
}

/// Bound on a recorded panic detail, for the same reason `failure_detail` is bounded in
/// general: the text may be server-influenced and it is published.
const MAX_PANIC_DETAIL_CHARS: usize = 300;

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
) -> Result<Value, DeriveError> {
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
            Observation::Discovered {
                discovery_path,
                era,
                handshake,
                tools_list_pages,
                pagination,
            } => {
                derive_discovered(
                    store,
                    *discovery_path,
                    era,
                    handshake,
                    tools_list_pages,
                    *pagination,
                    &mut record,
                    &mut rollup,
                )?;
            }
        }
        records.push(Value::Object(record));
    }

    rollup.write_into(&mut header);
    header.insert("servers".into(), Value::Array(records));
    Ok(Value::Object(header))
}

/// Prefix shared by every failure category for a server whose `tools/list` cursor walk was
/// truncated (P0-13). Never itself a category since the B3 review: see
/// [`truncated_category`].
///
/// **Why a truncated walk is a failure rather than a flagged success.** Its tally would be a
/// partial sample of the server's own tools, and both census readings break on that in ways
/// a flag cannot repair: the tool-weighted corpus tally would silently gain a prefix of a
/// server instead of a server, and the server-weighted `explicit.on_every_tool` would report
/// "every tool" over a subset — a strictly *more* favourable claim than the evidence
/// supports. design.md §8's rule for an unobservable effect is `unverifiable`, never `holds`;
/// this is the same rule for a partially-observed tool list.
///
/// The evidence is still kept — every page that did arrive is in the store, with the
/// `pagination` block saying how far the walk got — so a later analysis can use it
/// deliberately. What it may not do is enter a headline number by default.
///
/// **Not uniformly subtractable, and never as a whole.** Only the `_by_bound` and
/// `_by_budget` categories are this harness's doing. `_by_server` is the *server's* doing —
/// an unfollowable cursor, which costs a server one JSON key and removes it from every
/// denominator — and subtracting those records would hand any server that wants to dodge
/// measurement a free exit, with the bias running in the direction that flatters the
/// ecosystem (see [`discovery::TruncationAttribution`]). `_ambiguous` cannot be attributed
/// either way. An analyst choosing a denominator must choose per category.
pub(crate) const TOOLS_LIST_TRUNCATED: &str = "tools_list_truncated";

/// The failure category a truncated walk is recorded under, partitioned by who truncated
/// it (B3). One flat, prefix-sharing category per [`TruncationAttribution`], so
/// `failure_categories` itself carries the partition rather than one pooled count.
pub(crate) fn truncated_category(attribution: TruncationAttribution) -> &'static str {
    match attribution {
        TruncationAttribution::ServerNonconformant => "tools_list_truncated_by_server",
        TruncationAttribution::HarnessBound => "tools_list_truncated_by_bound",
        TruncationAttribution::HarnessBudget => "tools_list_truncated_by_budget",
        // `NotTruncated` is never passed here (only a walk whose cursors were not exhausted
        // is recorded as truncated); mapped rather than panicked on, because a panic in a
        // sweep discards a results file.
        TruncationAttribution::Ambiguous | TruncationAttribution::NotTruncated => {
            "tools_list_truncated_ambiguous"
        }
    }
}

/// Failure category for a pre-P0-13 record whose walk was never recorded — the fifth
/// truncation bucket, distinct from the four attributions because nobody truncated it: the
/// file simply predates cursor following.
const TOOLS_LIST_TRUNCATED_UNRECORDED: &str = "tools_list_truncated_unrecorded";

/// The most bytes one walk can legitimately hold: the walk bound plus one page, because the
/// capture-side byte check runs after a page is appended (see
/// `discovery::paginate::MAX_TOTAL_BYTES`). Re-applied at derivation so a crafted evidence
/// bundle cannot make re-derivation read more than any live capture could have stored.
const MAX_DERIVED_WALK_BYTES: usize =
    discovery::paginate::MAX_TOTAL_BYTES + discovery::paginate::MAX_PAGE_BYTES;

/// One walk, derived from its stored pages **one page at a time**.
///
/// Streamed rather than materialised (B4): every page is read, measured, classified and
/// pinned, then dropped, so peak memory is one page plus the per-tool pins rather than the
/// whole walk. That matters because the store is content-addressed — one blob referenced N
/// times keeps an evidence bundle tiny while a derivation that holds every reference
/// resident is not; a 22 KB results file and a 922 KB store measured 184 MB resident before
/// this, extrapolating to ~90 GB from a 1 MB file and a 10 MiB store.
struct WalkDerivation {
    /// Each page's length, in walk order.
    page_bytes: Vec<usize>,
    /// The first page whose coverage could not be extracted, and why.
    coverage_error: Option<(usize, String)>,
    /// Tally over the **name-deduplicated** tool set, first sighting in walk order winning.
    tally: AnnotationTally,
    /// Tools as observed — every occurrence on every page.
    tools_observed: usize,
    tools_on_first_page: usize,
    /// Distinct tool names. The census tool count.
    tools_distinct: usize,
    /// Per-tool pins over every observed tool, in walk order, or why pinning failed.
    pins: Result<Vec<ToolPin>, String>,
}

impl WalkDerivation {
    fn stream(store: &BlobStore, digests: &[Digest]) -> Result<Self, DeriveError> {
        let mut walk = Self {
            page_bytes: Vec::with_capacity(digests.len()),
            coverage_error: None,
            tally: AnnotationTally::default(),
            tools_observed: 0,
            tools_on_first_page: 0,
            tools_distinct: 0,
            pins: Ok(Vec::new()),
        };
        let mut names: HashSet<String> = HashSet::new();
        let mut bytes_total = 0usize;
        for (index, digest) in digests.iter().enumerate() {
            let page = store.get(digest)?;
            bytes_total = bytes_total.saturating_add(page.len());
            if bytes_total > MAX_DERIVED_WALK_BYTES {
                return Err(DeriveError::Malformed(format!(
                    "tools/list walk holds more than {MAX_DERIVED_WALK_BYTES} bytes by page {} — \
                     more than any capture can store; refusing to derive from this bundle",
                    index + 1
                )));
            }
            walk.page_bytes.push(page.len());

            if walk.coverage_error.is_none() {
                match coverage::tool_coverage(&page) {
                    Ok(tools) => {
                        if index == 0 {
                            walk.tools_on_first_page = tools.len();
                        }
                        walk.tools_observed += tools.len();
                        let fresh: Vec<_> =
                            tools.into_iter().filter(|t| names.insert(t.tool_name.clone())).collect();
                        walk.tools_distinct += fresh.len();
                        walk.tally += coverage::tally(&fresh);
                    }
                    Err(e) => walk.coverage_error = Some((index, e.to_string())),
                }
            }
            if let Ok(pins) = &mut walk.pins {
                match discovery::pin_page(&page) {
                    Ok(page_pins) => pins.extend(page_pins),
                    Err(e) => walk.pins = Err(e.to_string()),
                }
            }
        }
        Ok(walk)
    }

    fn bytes_total(&self) -> usize {
        self.page_bytes.iter().sum()
    }

    /// Tool occurrences beyond the first of their name — re-listings across pages.
    fn duplicate_tool_names(&self) -> usize {
        self.tools_observed - self.tools_distinct
    }

    /// Names that appeared with **more than one distinct definition** — the same name
    /// re-listed with different `annotations`, `inputSchema` or `description`. `None` when
    /// the walk could not be pinned.
    ///
    /// The case `duplicate_tool_names` cannot see on its own: a name repeated with a changed
    /// definition is a tool set changing *under the walk*, and since the tally keeps the
    /// first sighting, this is what tells a reader the second sighting said something else.
    fn conflicting_tool_definitions(&self) -> Option<usize> {
        let pins = self.pins.as_ref().ok()?;
        let mut pairs: Vec<(&str, [u8; 32])> =
            pins.iter().map(|p| (p.name.as_str(), *p.pin.as_bytes())).collect();
        pairs.sort_unstable();
        pairs.dedup();
        let mut conflicting = 0;
        let mut i = 0;
        while i < pairs.len() {
            let mut j = i + 1;
            while j < pairs.len() && pairs[j].0 == pairs[i].0 {
                j += 1;
            }
            if j - i > 1 {
                conflicting += 1;
            }
            i = j;
        }
        Some(conflicting)
    }
}

/// Derive one discovered server's record fields from its stored evidence.
#[allow(clippy::too_many_arguments)]
fn derive_discovered(
    store: &BlobStore,
    discovery_path: DiscoveryPath,
    era: &EraRecord,
    handshake: &Digest,
    tools_list_pages: &[Digest],
    pagination: PaginationRecord,
    record: &mut Map<String, Value>,
    rollup: &mut Rollup,
) -> Result<(), DeriveError> {
    let handshake_raw = store.get(handshake)?;
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

    let walk = WalkDerivation::stream(store, tools_list_pages)?;
    // Re-applied at derivation (B4): a walk that ran out of cursors with more tools than the
    // capture-side ceiling allows cannot have come from a live capture, so a bundle claiming
    // one is refused rather than tallied — the same disqualification capture applies.
    if pagination.cursors_exhausted() && walk.tools_observed > discovery::paginate::MAX_TOOLS {
        return Err(DeriveError::Malformed(format!(
            "a walk recorded as no_cursor holds {} tools, over the capture ceiling of {}",
            walk.tools_observed,
            discovery::paginate::MAX_TOOLS
        )));
    }

    rollup.record_discovered(
        discovery_path,
        era,
        revision.as_deref(),
        (handshake, handshake_raw.len()),
        (tools_list_pages, &walk.page_bytes),
        (era.probe.digest(), probe_bytes),
    );

    record.insert("discovery_path".into(), discovery_path.as_str().into());
    record.insert("era_provenance".into(), era.to_json(probe_bytes));
    record.insert("negotiated_spec_revision".into(), revision.map_or(Value::Null, Value::String));
    record.insert(
        "evidence".into(),
        json!({
            "handshake_raw": { "digest": handshake.to_string(), "bytes": handshake_raw.len() },
            // Kept, and kept meaning page one: every pre-P0-13 consumer of this key reads
            // the first `tools/list` response, which is exactly what it still is.
            "tools_list_raw": {
                "digest": tools_list_pages.first().map(ToString::to_string).map_or(Value::Null, Value::String),
                "bytes": walk.page_bytes.first().copied().unwrap_or(0),
            },
            // The authoritative walk. Always present, even for a one-page server, because an
            // absent array and an array of one are different claims about a file and only
            // one of them is checkable.
            "tools_list_pages": tools_list_pages
                .iter()
                .zip(walk.page_bytes.iter())
                .map(|(digest, bytes)| json!({ "digest": digest.to_string(), "bytes": bytes }))
                .collect::<Vec<_>>(),
        }),
    );

    // Written, and counted, on **every** exit below — including `coverage_extraction`, which
    // used to return first and so left its server out of the pagination denominator while
    // its own record claimed a finished walk.
    record.insert("pagination".into(), pagination_json(pagination, &walk));
    // The census-pipeline instance of architecture.md §5.1's accept-and-flag pattern, which
    // P4-03 carries to publication: a walk the *server* truncated with an unfollowable
    // cursor. On every discovered record, `false` included, so an absent flag can never be
    // read as a clear one.
    let flagged = pagination.is_server_nonconformance();
    record.insert("nonconformance_flag".into(), flagged.into());
    if flagged {
        record.insert("nonconformance_reason".into(), pagination.as_str().into());
    }
    rollup.record_pagination(pagination, &walk);

    if let Some((index, e)) = &walk.coverage_error {
        rollup.record_failure("coverage_extraction");
        record.insert("outcome".into(), "failed".into());
        record.insert("failure_category".into(), "coverage_extraction".into());
        record.insert(
            "failure_detail".into(),
            bounded(&format!("page {}: {e}", index + 1), MAX_DETAIL_CHARS).into(),
        );
        return Ok(());
    }

    // A truncated walk never becomes a coverage number. See `TOOLS_LIST_TRUNCATED`.
    if !pagination.cursors_exhausted() {
        let category = pagination.truncated_category();
        rollup.record_failure(category);
        record.insert("outcome".into(), "failed".into());
        record.insert("failure_category".into(), category.into());
        record.insert(
            "failure_detail".into(),
            format!(
                "tools/list walk incomplete: stopped after {} page(s) because {} ({}); {} \
                 tool(s) and {} byte(s) were observed and are kept in the evidence store, but \
                 a partial tool list must not be tallied as a complete one",
                walk.page_bytes.len(),
                pagination.as_str(),
                pagination.attribution_str(),
                walk.tools_observed,
                walk.bytes_total()
            )
            .into(),
        );
        return Ok(());
    }

    // The census tool count is the **distinct** tool set (B1): a name re-listed across pages
    // is one tool, counted and tallied once, first sighting winning. Summing per-page
    // tallies instead let forty pages of the same two tools publish as eighty tools with one
    // annotation inflated 40x, recorded as a clean success. The raw occurrence count stays
    // published in `pagination.tools_total` beside it, so the de-duplication is visible.
    let tool_count = walk.tools_distinct;
    let engagement = Engagement::of(tool_count, &walk.tally);
    rollup.record_success(tool_count, &walk.tally, engagement);

    record.insert("outcome".into(), "success".into());
    record.insert("tool_count".into(), tool_count.into());
    record.insert("annotation_tally".into(), tally_json(&walk.tally));
    record.insert("annotations_object".into(), engagement.as_str().into());
    // P0-02's pin binds this record to the exact tool set observed — every occurrence on
    // every page since P0-13 (ADR-015), *not* the de-duplicated set: a pin is over what was
    // received, never normalised. `server_pin_unordered` sits beside it so a reader can tell
    // "reordered" (only the primary moved) from "changed" (both moved). A response coverage
    // can read but the pinner cannot (a tool with no `inputSchema`) is still a census
    // success, as it was in July — the pin is recorded as absent with its reason.
    match &walk.pins {
        Ok(pins) => {
            record.insert("server_pin".into(), discovery::server_pin_of(pins).to_string().into());
            record.insert(
                "server_pin_unordered".into(),
                discovery::unordered_server_pin_of(pins).to_string().into(),
            );
        }
        Err(e) => {
            record.insert("server_pin".into(), Value::Null);
            record.insert("server_pin_unordered".into(), Value::Null);
            record.insert("pin_error".into(), bounded(e, MAX_DETAIL_CHARS).into());
        }
    }
    Ok(())
}

/// One server's `pagination` block (P0-13).
///
/// Everything here except `stopped_because` (and the attribution derived from it) is
/// recomputed from the stored pages, so a re-derivation reproduces it without trusting the
/// live run's arithmetic — and `tools_beyond_first_page` is the number this whole task
/// exists to make visible: it is exactly what July's tool-weighted counts were missing for
/// this server. On a `coverage_extraction` failure the tool figures are 0: no tool count is
/// derivable from a walk with an unreadable page.
fn pagination_json(record: PaginationRecord, walk: &WalkDerivation) -> Value {
    let readable = walk.coverage_error.is_none();
    let (observed, beyond, distinct) = if readable {
        (walk.tools_observed, walk.tools_observed - walk.tools_on_first_page, walk.tools_distinct)
    } else {
        (0, 0, 0)
    };
    json!({
        "pages_fetched": walk.page_bytes.len(),
        // **Not** "complete". It asserts that the server stopped offering cursors — never
        // that the harness saw the server's whole catalogue: a curated subset ending without
        // a cursor reads `true` here with no mechanical signal against it.
        "cursors_exhausted": record.cursors_exhausted(),
        "stopped_because": record.as_str(),
        // Who truncated the walk (B3): `server_nonconformant`, `harness_bound`,
        // `harness_budget`, `ambiguous`, `unrecorded`, or `not_truncated`.
        "truncation_attribution": record.attribution_str(),
        // Every occurrence on every page, as received.
        "tools_total": observed,
        "tools_beyond_first_page": beyond,
        // The census tool count: distinct names, first sighting winning.
        "tools_distinct": distinct,
        // Occurrences beyond the first of their name. **One-sided**: it catches a tool
        // re-listed across pages, but not a tool *dropped* between pages, and a server that
        // never repeats a name avoids it entirely. Re-listings are excluded from the tally.
        "duplicate_tool_names": if readable { walk.duplicate_tool_names() } else { 0 },
        // Names seen with more than one distinct per-tool pin — a tool redefined mid-walk.
        // `null` when the walk could not be pinned.
        "conflicting_tool_definitions": if readable {
            walk.conflicting_tool_definitions().map_or(Value::Null, Value::from)
        } else {
            Value::Null
        },
        "bytes_total": walk.bytes_total(),
    })
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
    // Pagination (P0-13), over every server whose tools/list returned at least one page.
    paginated_servers: usize,
    servers_paginating: usize,
    pages_fetched_total: usize,
    max_pages_fetched: usize,
    /// Tools seen across every page of every server with at least one page — **including**
    /// servers whose walk was truncated, which `tools_discovered` deliberately excludes
    /// (they are recorded as failures). This is the honest denominator for the share of
    /// tools that lay beyond page one.
    tools_observed_total: usize,
    tools_beyond_first_page: usize,
    duplicate_tool_names: usize,
    /// Names redefined mid-walk, summed over every walk that could be pinned.
    conflicting_tool_definitions: usize,
    servers_truncated: usize,
    /// `servers_truncated`, partitioned by who truncated the walk (B3).
    servers_truncated_by: BTreeMap<&'static str, usize>,
    /// Discovered records carrying `nonconformance_flag: true`.
    servers_nonconformance_flagged: usize,
    stop_reasons: BTreeMap<&'static str, usize>,
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
        tools_list: (&[Digest], &[usize]),
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
        // Every page of the walk, not just the first (P0-13): `tools_list_bytes` means
        // "bytes of `tools/list` evidence this corpus holds", and a one-page server's figure
        // is unchanged.
        self.tools_list_bytes += tools_list.1.iter().map(|len| *len as u64).sum::<u64>();
        self.probe_bytes += probe.1.unwrap_or(0) as u64;
        for (digest, len) in [Some(handshake), probe.0.zip(probe.1)].into_iter().flatten() {
            self.distinct_blobs.insert(*digest, len as u64);
        }
        for (digest, len) in tools_list.0.iter().zip(tools_list.1.iter()) {
            self.distinct_blobs.insert(*digest, *len as u64);
        }
    }

    /// One server's pagination outcome (P0-13).
    ///
    /// Counted over servers whose `tools/list` returned at least one page — the same
    /// denominator as `provenance`, and deliberately *not* "successful servers", because a
    /// truncated walk is recorded as a failure and is exactly the population a reader needs
    /// this block to size.
    ///
    /// Called on every exit of [`derive_discovered`], `coverage_extraction` included, so the
    /// per-server `pages_fetched` values always sum to `pages_fetched_total`. The tool
    /// figures it adds are exactly the ones the server's own `pagination` block publishes
    /// (zero for an unreadable walk), so the two can never disagree.
    fn record_pagination(&mut self, record: PaginationRecord, walk: &WalkDerivation) {
        let pages = walk.page_bytes.len();
        self.paginated_servers += 1;
        if pages > 1 {
            self.servers_paginating += 1;
        }
        self.pages_fetched_total += pages;
        self.max_pages_fetched = self.max_pages_fetched.max(pages);
        if walk.coverage_error.is_none() {
            self.tools_observed_total += walk.tools_observed;
            self.tools_beyond_first_page += walk.tools_observed - walk.tools_on_first_page;
            self.duplicate_tool_names += walk.duplicate_tool_names();
            self.conflicting_tool_definitions += walk.conflicting_tool_definitions().unwrap_or(0);
        }
        if !record.cursors_exhausted() {
            self.servers_truncated += 1;
            *self.servers_truncated_by.entry(record.attribution_str()).or_insert(0) += 1;
        }
        if record.is_server_nonconformance() {
            self.servers_nonconformance_flagged += 1;
        }
        *self.stop_reasons.entry(record.as_str()).or_insert(0) += 1;
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

        // Seeded with every reason plus `unrecorded`, for the same reason the two
        // distributions above are: a bound that fired once in a thousand servers is a
        // finding, and a bucket that is merely absent cannot be told from one that is zero.
        let mut stop_reasons: BTreeMap<&str, usize> = StopReason::all()
            .iter()
            .map(|r| (r.as_str(), 0))
            .chain([("unrecorded", 0)])
            .collect();
        stop_reasons.extend(self.stop_reasons);

        // Seeded with every truncating attribution plus `unrecorded`, for the same reason.
        let mut truncated_by: BTreeMap<&str, usize> = TruncationAttribution::all_truncating()
            .iter()
            .map(|a| (a.as_str(), 0))
            .chain([("unrecorded", 0)])
            .collect();
        truncated_by.extend(self.servers_truncated_by);

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
            // P0-13. The point of this block is comparability, not novelty: July's counts
            // were tool-weighted and a paginating server was under-counted by an unknown
            // amount, so the re-run must be able to say *how much* was previously missing.
            // `tools_beyond_first_page` is exactly that quantity, corpus-wide.
            "pagination": {
                "denominator": "servers whose tools/list returned at least one page",
                "servers": self.paginated_servers,
                "servers_paginating": self.servers_paginating,
                "pct_servers_paginating": pct_2dp(self.servers_paginating, self.paginated_servers),
                "pages_fetched_total": self.pages_fetched_total,
                "max_pages_fetched": self.max_pages_fetched,
                // What July missed, in tools. A zero here means the July figures were
                // already complete, and that is itself the answer to an open question
                // ("how many servers paginate is UNVERIFIED", O-01), not a null result.
                "tools_observed_total": self.tools_observed_total,
                "tools_beyond_first_page": self.tools_beyond_first_page,
                "pct_of_tools_beyond_first_page":
                    pct_2dp(self.tools_beyond_first_page, self.tools_observed_total),
                "servers_truncated": self.servers_truncated,
                // B3. Who truncated each walk. Only `harness_bound` and `harness_budget` are
                // this harness's to subtract from a denominator; `server_nonconformant` is a
                // server declining to be measured, and the servers with a motive to do that
                // are the ones that declare annotations — so excluding them biases `absent`
                // upward. An offline sensitivity bound over these records' retained page-one
                // evidence is computable but not built (ADR-015 "Flagged").
                "servers_truncated_by": truncated_by,
                "servers_nonconformance_flagged": self.servers_nonconformance_flagged,
                // Raw re-listings. Excluded from every tally and tool count (B1), published
                // so the de-duplication is visible rather than silent.
                "duplicate_tool_names": self.duplicate_tool_names,
                "conflicting_tool_definitions": self.conflicting_tool_definitions,
                "stopped_because": stop_reasons,
                // The bounds in force, because `page_limit` means nothing without them and a
                // reader cannot reproduce a truncation decision from the reason alone.
                "bounds": {
                    "max_pages": discovery::paginate::MAX_PAGES,
                    "max_tools": discovery::paginate::MAX_TOOLS,
                    "max_tools_enforced": "after every page, cursor or not",
                    "max_total_bytes": discovery::paginate::MAX_TOTAL_BYTES,
                    "max_page_bytes": discovery::paginate::MAX_PAGE_BYTES,
                    "max_cursor_bytes": discovery::paginate::MAX_CURSOR_BYTES,
                },
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
    // P0-13's headline, printed because it is the number this task exists to produce: how
    // much the pre-pagination census was missing, and whether any walk was cut short.
    eprintln!(
        "{label}: pagination — {} of {} servers paginated ({} pages total, max {}); {} tool(s) \
         lay beyond page one of {} observed ({:.1}%); {} walk(s) truncated — by server {} \
         (flagged nonconformant), by harness bound {}, by harness budget {}, ambiguous {}, \
         unrecorded {}",
        n("/pagination/servers_paginating"),
        n("/pagination/servers"),
        n("/pagination/pages_fetched_total"),
        n("/pagination/max_pages_fetched"),
        n("/pagination/tools_beyond_first_page"),
        n("/pagination/tools_observed_total"),
        pct(
            n("/pagination/tools_beyond_first_page") as usize,
            n("/pagination/tools_observed_total") as usize,
        ),
        n("/pagination/servers_truncated"),
        n("/pagination/servers_truncated_by/server_nonconformant"),
        n("/pagination/servers_truncated_by/harness_bound"),
        n("/pagination/servers_truncated_by/harness_budget"),
        n("/pagination/servers_truncated_by/ambiguous"),
        n("/pagination/servers_truncated_by/unrecorded"),
    );
    let with_evidence = n("/evidence_summary/servers_with_evidence");
    eprintln!(
        "{label}: evidence — {} bytes in {} distinct blobs for {with_evidence} servers ({:.0} bytes/server)",
        n("/evidence_summary/bytes_distinct"),
        n("/evidence_summary/distinct_blobs"),
        n("/evidence_summary/bytes_referenced") as f64 / with_evidence.max(1) as f64,
    );
    if report.pointer("/pacing/applies").and_then(Value::as_bool) == Some(true) {
        eprintln!(
            "{label}: pacing — {} requests over {} registrable domains at {} ms spacing \
             (cap {}/domain), {} ms waited, most-loaded domain {} requests, {} host(s) refused \
             for budget",
            n("/pacing/requests_sent"),
            n("/pacing/registrable_domains_contacted"),
            n("/pacing/min_interval_ms"),
            n("/pacing/max_requests_per_registrable_domain"),
            n("/pacing/waited_ms_total"),
            n("/pacing/max_requests_to_one_registrable_domain"),
            n("/pacing/sequences_denied_by_budget"),
        );
        // Separate line, because it is a different event from a refused host: this is the
        // count of tool lists *this harness's own budget* cut short (P0-13).
        let pages_refused = n("/pacing/page_extensions_denied");
        if pages_refused > 0 {
            eprintln!(
                "{label}: pacing — {pages_refused} tools/list walk(s) truncated by the domain \
                 budget; those servers' coverage numbers are withheld, not partial"
            );
        }
    }
    // Checked here as well as in `run_rederive`, so a published file's politeness claim is
    // verified against its own records before the file is written, not only if someone later
    // thinks to re-derive it. A mismatch is a harness bug, so it is loud and does not fail
    // the sweep — the data already collected is worth keeping.
    if let Err(problems) = verify_pacing_self_consistent(&report) {
        for problem in problems {
            eprintln!("{label}: PACING ACCOUNTING INCONSISTENT — {problem}");
        }
    }
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
    // Independent of whether the file reproduces: a pacing claim that does not match the
    // records beneath it is wrong whichever way the byte comparison went, and an offline
    // re-derivation is exactly where a reader auditing a published sweep would look.
    //
    // **And here it fails the command**, unlike in `finish`. The asymmetry is deliberate:
    // not failing a *live sweep* is right, because the data already collected is worth
    // keeping and third-party contact has already been spent. The offline audit path has
    // nothing at risk, and returning success while printing
    // `PACING ACCOUNTING INCONSISTENT` means a CI job or a reviewer's script checking a
    // published file would pass on a file whose politeness claim is demonstrably wrong.
    let mut pacing_consistent = true;
    if let Err(problems) =
        verify_pacing_self_consistent(&serde_json::from_str(&outcome.regenerated)?)
    {
        pacing_consistent = false;
        for problem in problems {
            eprintln!("census-rederive: PACING ACCOUNTING INCONSISTENT — {problem}");
        }
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
    // Both conditions, not just reproduction: a file whose published pacing does not
    // describe its own records fails this command even when the bytes reproduce exactly.
    Ok(outcome.byte_identical && pacing_consistent)
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
        // The whole walk, in order (P0-13). A file written before cursor following has no
        // `tools_list_pages` array, and that is a fact about the file rather than an error:
        // it is read as the single page it recorded, and its `pagination` block comes back
        // `unrecorded` — which, per `PaginationRecord::cursors_exhausted`, is *not* treated as
        // complete, so re-deriving such a file reclassifies its servers as truncated rather
        // than silently asserting their tool lists were whole. Disclosed in P0-13's entry;
        // no such file exists in `results/`, which is why this is the cheap direction.
        let tools_list_pages = match evidence.get("tools_list_pages") {
            Some(Value::Array(entries)) => {
                if entries.is_empty() {
                    return Err("evidence.tools_list_pages is empty — a discovery always has \
                                at least one tools/list page"
                        .into());
                }
                // B4: a walk longer than capture can produce is a malformed record, refused
                // before a single blob is read. The store is content-addressed, so one blob
                // referenced thousands of times keeps a crafted bundle tiny while its
                // derivation is not — the measured case was 200 pages against a capture
                // bound of 40, from a 22 KB file.
                if entries.len() > discovery::paginate::MAX_PAGES {
                    return Err(format!(
                        "evidence.tools_list_pages has {} entries, more than the {} any capture \
                         can produce",
                        entries.len(),
                        discovery::paginate::MAX_PAGES
                    ));
                }
                // The declared sizes are untrusted — derivation re-measures every blob and
                // refuses past the same ceiling — but a file that *declares* more than any
                // capture can hold is refused here, before anything is read.
                let declared: u64 = entries
                    .iter()
                    .filter_map(|e| e.get("bytes").and_then(Value::as_u64))
                    .fold(0u64, u64::saturating_add);
                if declared > MAX_DERIVED_WALK_BYTES as u64 {
                    return Err(format!(
                        "evidence.tools_list_pages declares {declared} bytes, more than the \
                         {MAX_DERIVED_WALK_BYTES} any capture can store"
                    ));
                }
                entries
                    .iter()
                    .enumerate()
                    .map(|(i, entry)| {
                        entry
                            .get("digest")
                            .and_then(Value::as_str)
                            .and_then(Digest::from_hex)
                            .ok_or_else(|| {
                                format!(
                                    "evidence.tools_list_pages[{i}].digest is missing or not a \
                                     64-character lowercase hex digest"
                                )
                            })
                    })
                    .collect::<Result<Vec<_>, String>>()?
            }
            Some(_) => return Err("evidence.tools_list_pages is not an array".into()),
            None => vec![digest("tools_list_raw")?],
        };
        return Ok(Observation::Discovered {
            discovery_path,
            // Absent in a file written before P0-11, which is a fact about the file rather
            // than an error: re-derivation still works, and reports the era as unrecorded.
            era: EraRecord::from_record(record.get("era_provenance")),
            handshake: digest("handshake_raw")?,
            tools_list_pages,
            pagination: PaginationRecord::from_record(record.get("pagination")),
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
        walk(path, revision, &[tools_json], StopReason::NoCursor)
    }

    /// One `tools/list` page, in the legacy result shape.
    fn page(tools_json: &str, next_cursor: Option<&str>) -> Vec<u8> {
        let cursor = match next_cursor {
            Some(c) => format!(r#","nextCursor":"{c}""#),
            None => String::new(),
        };
        format!(r#"{{"jsonrpc":"2.0","id":1,"result":{{"tools":[{tools_json}]{cursor}}}}}"#)
            .into_bytes()
    }

    /// A discovery whose `tools/list` arrived across several pages (P0-13), ending for the
    /// stated reason. Every page but the last carries a cursor, because that is what a real
    /// walk of this length looks like and what the stop reason has to be consistent with.
    fn walk(
        path: DiscoveryPath,
        revision: &str,
        pages: &[&str],
        stopped_because: StopReason,
    ) -> Discovery {
        let last = pages.len().saturating_sub(1);
        let tools_list_pages: Vec<Vec<u8>> = pages
            .iter()
            .enumerate()
            .map(|(i, tools)| {
                let cursor = (i < last || !stopped_because.cursors_exhausted())
                    .then(|| format!("cursor-{}", i + 1));
                page(tools, cursor.as_deref())
            })
            .collect();
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
            pagination: discovery::PaginationOutcome {
                pages_fetched: tools_list_pages.len(),
                stopped_because,
            },
            tools_list_pages,
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
        no_tools_array.tools_list_pages =
            vec![br#"{"jsonrpc":"2.0","id":1,"result":{}}"#.to_vec()];
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
            sample_seed: None,
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
            sample_seed: None,
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
            every_tool.extend(
                coverage::tool_coverage(
                    &discovery(DiscoveryPath::Initialize, "x", body).tools_list_pages[0],
                )
                .expect("parse"),
            );
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
        // B1: no HTTP client in this harness follows a redirect any more, so a 3xx arrives
        // as an ordinary response and fails `call_raw`'s status gate. It must land in
        // `transport` — which is where the committed July file's 15 `redirect failed`
        // entries already sit, inside its 435 `transport` failures. Only the detail string
        // changes, from `redirect failed` to `http status: 30x`, so the July split (435
        // transport against 312 protocol) keeps its meaning.
        for status in [301, 302, 303, 307, 308] {
            assert_eq!(
                http_status_category(status),
                "transport",
                "an unfollowed {status} must stay in the bucket a `redirect failed` was in"
            );
            assert_eq!(http_status_detail(status, None), format!("http status: {status}"));
            // No `Retry-After` suffix outside 429, so a 3xx that happens to carry the
            // header cannot produce a detail July never produced.
            assert_eq!(
                http_status_detail(status, Some("30")),
                format!("http status: {status}")
            );
        }
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
            sample_seed: Some(0x1234_5678_9abc_def0),
        }
        .into_map()
        .expect("clock");
        assert_eq!(header["complete"], false);
        assert_eq!(header["not_attempted"], 6);
        // Published as a hex *string*, never a JSON number: a seed above 2^53 would be
        // silently rounded by any reader backed by doubles, and a seed that does not
        // reproduce its own sample is worse than no seed at all.
        assert_eq!(header["sample_seed"], "0x123456789abcdef0");
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

    // ---- P0-12: the pacing block, and whether a reader can check it ----

    /// A results header must carry the policy *and* the accounting, because the policy alone
    /// is a promise and the accounting alone is a number with no claim attached.
    /// A panicking attempt becomes one record, not a lost results file.
    ///
    /// This is the whole point of `guard_attempt`: `sweep::run_bounded` runs workers in a
    /// `thread::scope`, which re-panics on join, so before this a single panic unwound past
    /// the header insert and past `finish` — nothing written, and every host contacted
    /// up to that point contacted for no artifact.
    #[test]
    fn a_panicking_attempt_becomes_a_harness_panic_record_rather_than_losing_the_run() {
        let failure = |observed: ServerObservation| match observed.observation {
            Observation::Failed { category, detail } => (category, detail),
            Observation::Discovered { .. } => panic!("expected a failure observation"),
        };

        let (category, detail) = failure(guard_attempt("test", || panic!("worker exploded")));
        assert_eq!(category, HARNESS_PANIC, "a panic must land in its own harness-side bucket");
        assert!(detail.contains("exploded"), "the detail must say what happened: {detail:?}");

        // A non-string payload is still a record, not a second panic.
        let (category, _) = failure(guard_attempt("test", || std::panic::panic_any(7u32)));
        assert_eq!(category, HARNESS_PANIC);

        // And the happy path is untouched.
        let (category, detail) = failure(guard_attempt("test", || ServerObservation {
            identity: Map::new(),
            observation: Observation::failed("transport", "http status: 503"),
        }));
        assert_eq!((category.as_str(), detail.as_str()), ("transport", "http status: 503"));
    }

    /// A panic detail is server-influenceable text in a published file, so it is sanitised
    /// and bounded the same way `failure_detail` is in general — no escape introducers,
    /// no unbounded length.
    #[test]
    fn a_panic_detail_is_sanitised_and_bounded_before_it_is_recorded() {
        let payload = format!(
            "start\u{1b}[31m\u{9b}2K\nnewline\u{0}nul{}",
            "A".repeat(5_000)
        );
        let hostile = guard_attempt("test", || panic!("{payload}"));
        let Observation::Failed { detail, .. } = hostile.observation else {
            panic!("expected a failure observation")
        };
        assert!(
            detail.chars().count() <= MAX_PANIC_DETAIL_CHARS,
            "detail is {} chars, unbounded in a published file",
            detail.chars().count()
        );
        assert!(
            detail.chars().all(|c| c == ' ' || c == '\t' || c.is_ascii_graphic()),
            "detail still carries control bytes: {detail:?}"
        );
        assert!(detail.starts_with("start"), "the useful prefix survives: {detail:?}");
        assert!(
            !detail.contains('\u{1b}') && !detail.contains('\u{9b}'),
            "an escape introducer survived into a published detail: {detail:?}"
        );
    }

    #[test]
    fn the_pacing_header_reports_both_the_policy_and_the_accounting() {
        let pacer = std::sync::Arc::new(discovery::Pacer::new(discovery::PacePolicy {
            min_interval: std::time::Duration::ZERO,
            max_requests_per_registrable_domain: 10,
        }));
        for host in ["https://a.vendor.test/mcp", "https://b.vendor.test/mcp"] {
            let permit = pacer.admit(host).expect("budget");
            for _ in 0..4 {
                permit.before_request();
            }
        }
        // Cap 10, 8 charged: a third sequence cannot reserve 5 and is refused.
        pacer.admit("https://c.vendor.test/mcp").expect_err("budget spent");

        let block = pacing_header(&pacer.accounting());
        assert_eq!(block["applies"], true);
        assert_eq!(block["min_interval_ms"], 0);
        assert_eq!(block["max_requests_per_registrable_domain"], 10);
        assert_eq!(block["per_discovery_reservation"], discovery::MAX_REQUESTS_PER_DISCOVERY);
        assert_eq!(
            block["max_redirects"], 0,
            "a followed redirect would be a request this block cannot see"
        );
        assert_eq!(block["requests_sent"], 8);
        assert_eq!(block["sequences_denied_by_budget"], 1);
        assert_eq!(block["max_requests_to_one_registrable_domain"], 8);
        assert_eq!(block["registrable_domains_contacted"], 1, "all three share one key");
        assert_eq!(block["overshoot_requests"], 0);
        assert!(block["grouping"].as_str().is_some_and(|g| g.contains("ADR-014")));
        let rows = block["registrable_domains"].as_array().expect("per-domain rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["registrable_domain"], "vendor.test");
        assert_eq!(rows[0]["sequences_admitted"], 2);
        assert_eq!(rows[0]["sequences_denied"], 1);
    }

    /// Pacing accounting must be checkable *against the records beneath it*, not merely
    /// internally consistent — the discipline B-03 set for cross-oracle aggregation. Both
    /// directions: the guard accepts a report whose header matches its records, and names
    /// the field when it does not.
    #[test]
    fn the_pacing_guard_accepts_a_consistent_report_and_names_an_inflated_field() {
        let consistent = json!({
            "pacing": {
                "applies": true,
                "per_discovery_reservation": 5,
                "requests_sent": 6,
                "waited_ms_total": 1_500,
                "sequences_denied_by_budget": 1,
                "max_requests_to_one_registrable_domain": 4,
                "registrable_domains_contacted": 2,
                "overshoot_requests": 0,
                "page_extensions_denied": 0,
            },
            "servers": [
                {
                    "name": "a",
                    "pacing": {
                        "registrable_domain": "one.test",
                        "requests_sent": 4,
                        "waited_ms": 1_000,
                    },
                },
                {
                    "name": "b",
                    "pacing": {
                        "registrable_domain": "two.test",
                        "requests_sent": 2,
                        "waited_ms": 500,
                    },
                },
                {
                    "name": "c",
                    "outcome": "failed",
                    "failure_category": BUDGET_EXHAUSTED,
                    "pacing": {
                        "registrable_domain": "one.test",
                        "requests_sent": 0,
                        "waited_ms": 0,
                    },
                },
            ],
        });
        assert_eq!(verify_pacing_self_consistent(&consistent), Ok(()));

        // A header claiming fewer requests than were made — the direction that would make a
        // sweep look politer than it was, which is the one worth catching.
        let mut understated = consistent.clone();
        understated["pacing"]["requests_sent"] = json!(2);
        let problems = verify_pacing_self_consistent(&understated).expect_err("must be caught");
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("requests_sent is 2"), "{problems:?}");
        assert!(problems[0].contains("give 6"), "{problems:?}");

        // A refusal dropped from the header's count.
        let mut hidden_refusal = consistent.clone();
        hidden_refusal["pacing"]["sequences_denied_by_budget"] = json!(0);
        let problems = verify_pacing_self_consistent(&hidden_refusal).expect_err("must be caught");
        assert!(problems[0].contains("sequences_denied_by_budget"), "{problems:?}");

        // **The headline politeness number.** `max_requests_to_one_registrable_domain` is
        // the field that substantiates "no third party received more than N requests", and
        // it was published without ever being checked against the records. Understating it
        // is the direction that matters: it is the claim a reader quotes.
        let mut understated_max = consistent.clone();
        understated_max["pacing"]["max_requests_to_one_registrable_domain"] = json!(2);
        let problems =
            verify_pacing_self_consistent(&understated_max).expect_err("must be caught");
        assert!(
            problems[0].contains("max_requests_to_one_registrable_domain is 2")
                && problems[0].contains("give 4"),
            "{problems:?}"
        );

        // Grouping is by registrable domain, not per record: `one.test` appears on two
        // records and must be summed, not maxed per record.
        let mut two_records_one_domain = consistent.clone();
        two_records_one_domain["servers"][2]["pacing"]["requests_sent"] = json!(3);
        two_records_one_domain["pacing"]["requests_sent"] = json!(9);
        let problems =
            verify_pacing_self_consistent(&two_records_one_domain).expect_err("must be caught");
        assert!(
            problems.iter().any(|p| p.contains("max_requests_to_one_registrable_domain is 4")
                && p.contains("give 7")),
            "two records on one domain must sum to 7, not stay at 4: {problems:?}"
        );

        // Domains contacted counts domains with at least one request, so the refused host's
        // zero does not inflate it.
        let mut wrong_domain_count = consistent.clone();
        wrong_domain_count["pacing"]["registrable_domains_contacted"] = json!(3);
        assert!(
            verify_pacing_self_consistent(&wrong_domain_count)
                .expect_err("must be caught")
                .iter()
                .any(|p| p.contains("registrable_domains_contacted is 3")),
        );

        // Overshoot is `Σ max(0, requests_sent − reservation)`, the number that is supposed
        // to go non-zero the day `tools/list` pagination lands. A published zero has to mean
        // something, so a record that overshot and a header that says zero is a mismatch.
        let mut overshot = consistent.clone();
        overshot["servers"][0]["pacing"]["requests_sent"] = json!(8);
        overshot["pacing"]["requests_sent"] = json!(10);
        overshot["pacing"]["max_requests_to_one_registrable_domain"] = json!(8);
        let problems = verify_pacing_self_consistent(&overshot).expect_err("must be caught");
        assert!(
            problems.iter().any(|p| p.contains("overshoot_requests is 0") && p.contains("give 3")),
            "{problems:?}"
        );

        // A missing field is a problem, not a zero.
        let mut missing = consistent.clone();
        missing["pacing"].as_object_mut().expect("object").remove("waited_ms_total");
        assert!(
            verify_pacing_self_consistent(&missing)
                .expect_err("must be caught")
                .iter()
                .any(|p| p.contains("waited_ms_total is missing")),
        );
        let mut missing_max = consistent.clone();
        missing_max["pacing"]
            .as_object_mut()
            .expect("object")
            .remove("max_requests_to_one_registrable_domain");
        assert!(
            verify_pacing_self_consistent(&missing_max)
                .expect_err("must be caught")
                .iter()
                .any(|p| p.contains("max_requests_to_one_registrable_domain is missing")),
        );
    }

    /// A sweep that did not pace says so, and the guard does not invent a claim to check.
    #[test]
    fn a_sweep_where_pacing_does_not_apply_records_that_rather_than_omitting_it() {
        let block = pacing_not_applicable("stdio to a local container");
        assert_eq!(block["applies"], false);
        assert_eq!(block["reason"], "stdio to a local container");
        let report = json!({ "pacing": block, "servers": [] });
        assert_eq!(verify_pacing_self_consistent(&report), Ok(()));
        // And a file written before P0-12 has no block at all, which is not an error.
        assert_eq!(verify_pacing_self_consistent(&json!({ "servers": [] })), Ok(()));
    }

    /// **July comparability.** A budget refusal is its own category: the `transport`,
    /// `protocol` and `rate_limited` counts a re-run produces still mean exactly what they
    /// meant in July, and the harness-side refusals are a bucket a reader can subtract.
    #[test]
    fn a_budget_refusal_is_its_own_category_and_leaves_the_july_buckets_untouched() {
        let dir = tempfile::tempdir().expect("tempdir");
        let evidence_dir = dir.path().join("evidence");
        let store = BlobStore::open(&evidence_dir).expect("store");

        let servers = vec![
            ServerObservation {
                identity: identity("throttled"),
                observation: Observation::from_discovery_error(
                    DiscoveryError::HttpStatus { status: 429, retry_after: None },
                    "io",
                ),
            },
            ServerObservation {
                identity: identity("auth-gated"),
                observation: Observation::from_discovery_error(
                    DiscoveryError::HttpStatus { status: 401, retry_after: None },
                    "io",
                ),
            },
            ServerObservation {
                identity: identity("sse-only"),
                observation: Observation::from_discovery_error(
                    DiscoveryError::Protocol("server responded with an SSE stream".into()),
                    "io",
                ),
            },
            ServerObservation {
                identity: identity("refused"),
                observation: Observation::failed(
                    BUDGET_EXHAUSTED,
                    "registrable-domain request budget exhausted for vendor.test: 120 sent, \
                     0 reserved, cap 120 (no request was made to this host)",
                ),
            },
        ];
        let header = RunHeader {
            stage: "test",
            sample_size_requested: 4,
            candidates_selected: 4,
            attempted: 4,
            stop_reason: None,
            evidence_store: &evidence_dir,
            sample_seed: None,
        }
        .into_map()
        .expect("clock");
        let report = build_report(&store, header, &servers).expect("build");

        let categories = report["failure_categories"].as_object().expect("categories");
        assert_eq!(categories["rate_limited"], 1, "429 keeps its own P0-11 bucket");
        assert_eq!(categories["transport"], 1, "401 is still `transport`, as in July");
        assert_eq!(categories["protocol"], 1, "an SSE upgrade is still `protocol`");
        assert_eq!(categories[BUDGET_EXHAUSTED], 1, "the refusal is in a bucket of its own");
        assert_eq!(categories.len(), 4, "no bucket was merged: {categories:?}");
        // And the refusal says why, in the record, rather than the host simply being absent.
        let refused = &report["servers"][3];
        assert_eq!(refused["outcome"], "failed");
        assert!(
            refused["failure_detail"]
                .as_str()
                .is_some_and(|d| d.contains("no request was made to this host")),
            "{refused}"
        );
    }

    /// A results file carrying per-server pacing accounting still re-derives byte for byte.
    /// The accounting is wall-clock-dependent, so it rides as carried-through observation
    /// rather than a derived value — and this is what proves the choice actually holds.
    #[test]
    fn a_results_file_with_pacing_records_rederives_byte_for_byte() {
        let dir = tempfile::tempdir().expect("tempdir");
        let evidence_dir = dir.path().join("evidence");
        let store = Mutex::new(BlobStore::open(&evidence_dir).expect("store"));
        let stop = StopFlag::default();

        let mut servers = Vec::new();
        for (i, (name, requests, waited_ms)) in
            [("paced-a", 4, 1_500u64), ("paced-b", 2, 500)].into_iter().enumerate()
        {
            let mut ident = identity(name);
            let waited = std::time::Duration::from_millis(waited_ms);
            ident.insert(
                "pacing".into(),
                pacing_record("example.test", requests, waited, discovery::MAX_REQUESTS_PER_DISCOVERY),
            );
            let observation = if i == 0 {
                record_discovery(
                    &store,
                    &discovery(DiscoveryPath::Initialize, "2025-11-25", ALL),
                    &stop,
                )
            } else {
                record_discovery(
                    &store,
                    &discovery(DiscoveryPath::ServerDiscover, "2026-07-28", NONE),
                    &stop,
                )
            };
            servers.push(ServerObservation { identity: ident, observation });
        }
        let mut refused = identity("refused");
        refused.insert(
            "pacing".into(),
            pacing_record("example.test", 0, std::time::Duration::ZERO, 0),
        );
        servers.push(ServerObservation {
            identity: refused,
            observation: Observation::failed(BUDGET_EXHAUSTED, "budget exhausted for example.test"),
        });

        let mut header = RunHeader {
            stage: "test",
            sample_size_requested: 3,
            candidates_selected: 3,
            attempted: 3,
            stop_reason: None,
            evidence_store: &evidence_dir,
            sample_seed: None,
        }
        .into_map()
        .expect("clock");
        header.insert(
            "pacing".into(),
            json!({
                "applies": true,
                "grouping": PACE_GROUPING,
                "min_interval_ms": 500,
                "max_requests_per_registrable_domain": 120,
                "per_discovery_reservation": discovery::MAX_REQUESTS_PER_DISCOVERY,
                "requests_sent": 6,
                "waited_ms_total": 2_000,
                "registrable_domains_contacted": 1,
                "registrable_domains_seen": 1,
                "max_requests_to_one_registrable_domain": 6,
                "sequences_denied_by_budget": 1,
                "page_extensions_denied": 0,
                "overshoot_requests": 0,
                "registrable_domains_listed": 1,
                "registrable_domains": [{
                    "registrable_domain": "example.test",
                    "requests": 6,
                    "sequences_admitted": 2,
                    "sequences_denied": 1,
                }],
            }),
        );
        let report =
            build_report(&store.into_inner().expect("lock"), header, &servers).expect("build");
        let results = dir.path().join("results.json");
        write_report(&results, &report).expect("write");

        // The published header's claim matches the records beneath it...
        assert_eq!(verify_pacing_self_consistent(&read(&results)), Ok(()));
        // ...and an offline re-derivation reproduces the whole file, pacing included.
        let outcome = rederive(&results, None).expect("rederive");
        assert!(outcome.byte_identical, "differs at {:?}", outcome.differences);
        let regenerated: Value = serde_json::from_str(&outcome.regenerated).expect("json");
        assert_eq!(regenerated["servers"][0]["pacing"]["requests_sent"], 4);
        assert_eq!(regenerated["servers"][2]["pacing"]["requests_sent"], 0);
        assert_eq!(regenerated["pacing"]["requests_sent"], 6);
    }

    // ---- P0-13: pagination through the derivation path ----

    /// Three pages of two tools each, plus a one-page server for contrast, driven through the
    /// real live path: `record_discovery` into a real store, `build_report` back out of it.
    fn paginated_run(dir: &Path) -> (PathBuf, PathBuf, Value) {
        let evidence_dir = dir.join("evidence");
        let store = Mutex::new(BlobStore::open(&evidence_dir).expect("open store"));
        let stop = StopFlag::default();
        let init = DiscoveryPath::Initialize;

        let servers = vec![
            ServerObservation {
                identity: identity("three-pages"),
                observation: record_discovery(
                    &store,
                    &walk(init, "2025-11-25", &[ALL, NONE, SOME], StopReason::NoCursor),
                    &stop,
                ),
            },
            ServerObservation {
                identity: identity("one-page"),
                observation: record_discovery(&store, &discovery(init, "2025-11-25", ALL), &stop),
            },
            // Cut off by this harness's own page bound: a *partial* tool list, which must not
            // become a coverage number.
            ServerObservation {
                identity: identity("truncated"),
                observation: record_discovery(
                    &store,
                    &walk(init, "2025-11-25", &[ALL, NONE], StopReason::PageLimit),
                    &stop,
                ),
            },
        ];

        let header = RunHeader {
            stage: "test",
            sample_size_requested: 3,
            candidates_selected: 3,
            attempted: 3,
            stop_reason: None,
            evidence_store: &evidence_dir,
            sample_seed: Some(7),
        }
        .into_map()
        .expect("header");

        let store = store.into_inner().expect("lock");
        let report = build_report(&store, header, &servers).expect("build");
        let results = dir.join("results.json");
        write_report(&results, &report).expect("write");
        (results, evidence_dir, report)
    }

    /// Every page is tallied, every page is in the evidence, and the pin covers the whole
    /// walk — the three things that were silently false before P0-13.
    #[test]
    fn a_multi_page_server_tallies_every_page_and_pins_the_whole_walk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (_, _, report) = paginated_run(dir.path());
        let server = &report["servers"][0];

        assert_eq!(server["name"], "three-pages");
        assert_eq!(server["outcome"], "success");
        // Two tools per page across three pages. Page one alone would have said 2.
        assert_eq!(server["tool_count"], 6);
        assert_eq!(server["pagination"]["pages_fetched"], 3);
        assert_eq!(server["pagination"]["cursors_exhausted"], true);
        assert_eq!(server["pagination"]["stopped_because"], "no_cursor");
        assert_eq!(server["pagination"]["tools_total"], 6);
        assert_eq!(
            server["pagination"]["tools_beyond_first_page"], 4,
            "this is the per-server quantity July was missing"
        );
        assert_eq!(server["pagination"]["duplicate_tool_names"], 0);

        // One evidence blob per page, in walk order, and `tools_list_raw` still means page
        // one — so a pre-P0-13 reader of that key sees exactly what it always saw.
        let pages = server["evidence"]["tools_list_pages"].as_array().expect("pages array");
        assert_eq!(pages.len(), 3);
        assert_eq!(server["evidence"]["tools_list_raw"]["digest"], pages[0]["digest"]);
        assert_eq!(server["evidence"]["tools_list_raw"]["bytes"], pages[0]["bytes"]);
        assert_eq!(
            server["pagination"]["bytes_total"].as_u64().expect("bytes"),
            pages.iter().map(|p| p["bytes"].as_u64().expect("bytes")).sum::<u64>()
        );

        // The pin is over the walk, and is *not* the pin page one alone would have produced.
        let fixture = walk(DiscoveryPath::Initialize, "2025-11-25", &[ALL, NONE, SOME], StopReason::NoCursor);
        let (_, whole_walk) = discovery::pin_pages(&fixture.tools_list_pages).expect("pin");
        let (_, page_one_only) =
            discovery::pin_tools(&fixture.tools_list_pages[0]).expect("pin page one");
        assert_eq!(server["server_pin"], whole_walk.to_string());
        assert_ne!(
            server["server_pin"], page_one_only.to_string(),
            "before P0-13 every published pin was the page-one one"
        );

        // The single-page server beside it is unchanged in every respect.
        let single = &report["servers"][1];
        assert_eq!(single["tool_count"], 2);
        assert_eq!(single["pagination"]["pages_fetched"], 1);
        assert_eq!(single["pagination"]["tools_beyond_first_page"], 0);
        assert_eq!(single["pagination"]["cursors_exhausted"], true);
    }

    /// **The rule this task turns on.** A truncated walk is recorded as a failure with its
    /// own category, and contributes no tally, no tool count and no pin — because a partial
    /// tool list filed as a complete one would corrupt both census readings: the
    /// tool-weighted tally would gain a prefix of a server, and the server-weighted
    /// `explicit.on_every_tool` would claim "every tool" over a subset, which is a *more*
    /// favourable claim than the evidence supports.
    #[test]
    fn a_truncated_walk_is_a_failure_rather_than_a_partial_coverage_number() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (_, _, report) = paginated_run(dir.path());
        let truncated = &report["servers"][2];

        assert_eq!(truncated["name"], "truncated");
        assert_eq!(truncated["outcome"], "failed");
        assert_eq!(truncated["failure_category"], "tools_list_truncated_by_bound");
        assert_eq!(truncated["pagination"]["cursors_exhausted"], false);
        assert_eq!(truncated["pagination"]["stopped_because"], "page_limit");
        assert_eq!(truncated["pagination"]["truncation_attribution"], "harness_bound");
        assert_eq!(
            truncated["nonconformance_flag"], false,
            "a bound this harness chose is not the server's nonconformance"
        );
        // What it *did* see is still stated, and its pages are still in the evidence store —
        // withheld from the headline numbers, not discarded.
        assert_eq!(truncated["pagination"]["tools_total"], 4);
        assert_eq!(truncated["evidence"]["tools_list_pages"].as_array().expect("pages").len(), 2);
        let detail = truncated["failure_detail"].as_str().expect("detail");
        assert!(detail.contains("page_limit"), "{detail}");
        assert!(detail.contains("must not be tallied"), "{detail}");

        for withheld in ["tool_count", "annotation_tally", "annotations_object", "server_pin"] {
            assert!(
                truncated.get(withheld).is_none(),
                "a truncated walk must contribute no {withheld}"
            );
        }

        // Corpus-wide: its own partitioned bucket, and its tools are outside
        // `tools_discovered`.
        assert_eq!(report["failure_categories"]["tools_list_truncated_by_bound"], 1);
        assert_eq!(report["pagination"]["servers_truncated_by"]["harness_bound"], 1);
        assert_eq!(report["succeeded"], 2);
        assert_eq!(report["tools_discovered"], 8, "6 from the walk plus 2 from the one-pager");
        assert_eq!(
            report["pagination"]["tools_observed_total"], 12,
            "the pagination denominator *includes* the truncated server, which is why it is \
             a different number from tools_discovered"
        );
    }

    /// The corpus-wide block, which is what a reader needs to size the correction rather
    /// than have it applied silently.
    #[test]
    fn the_pagination_header_sizes_what_was_previously_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (_, _, report) = paginated_run(dir.path());
        let block = &report["pagination"];

        assert_eq!(block["servers"], 3);
        assert_eq!(block["servers_paginating"], 2);
        assert_eq!(block["pct_servers_paginating"], 66.67);
        assert_eq!(block["pages_fetched_total"], 6);
        assert_eq!(block["max_pages_fetched"], 3);
        assert_eq!(block["tools_beyond_first_page"], 6, "4 from the walk, 2 from the truncated one");
        assert_eq!(block["pct_of_tools_beyond_first_page"], 50.0);
        assert_eq!(block["servers_truncated"], 1);
        assert_eq!(block["duplicate_tool_names"], 0);

        // Seeded with every reason, so a bound that never fired reads as zero rather than
        // being absent — and the bounds in force are published, because `page_limit` means
        // nothing without them.
        let reasons = block["stopped_because"].as_object().expect("reasons");
        assert_eq!(reasons.len(), StopReason::all().len() + 1, "every reason plus `unrecorded`");
        assert_eq!(reasons["no_cursor"], 2);
        assert_eq!(reasons["page_limit"], 1);
        assert_eq!(reasons["cursor_repeated"], 0);
        assert_eq!(reasons["unrecorded"], 0);
        assert_eq!(block["bounds"]["max_pages"], discovery::paginate::MAX_PAGES);
        assert_eq!(block["bounds"]["max_tools"], discovery::paginate::MAX_TOOLS);
        assert_eq!(block["bounds"]["max_total_bytes"], discovery::paginate::MAX_TOTAL_BYTES);
        assert_eq!(block["bounds"]["max_cursor_bytes"], discovery::paginate::MAX_CURSOR_BYTES);
    }

    /// **P0-10's central claim, with pagination in it.** A multi-page results file —
    /// including a truncated walk, whose stop reason is the one part of the block that is
    /// *not* derivable from any stored page — regenerates byte for byte from stored evidence
    /// with no network.
    #[test]
    fn a_multi_page_results_file_rederives_byte_for_byte() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (results, _, _) = paginated_run(dir.path());

        let outcome = rederive(&results, None).expect("rederive");
        assert!(
            outcome.byte_identical,
            "multi-page evidence must re-derive exactly; differs at {:?}",
            outcome.differences
        );
        assert_eq!(outcome.servers, 3);
        assert_eq!(outcome.servers_with_evidence, 3);
    }

    /// A page blob missing from the store fails the re-derivation loudly, exactly as a
    /// missing handshake blob does — so "the evidence no longer reproduces the file" cannot
    /// be mistaken for "that server had fewer pages".
    #[test]
    fn a_missing_later_page_blob_fails_rederivation_loudly() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (results, evidence_dir, report) = paginated_run(dir.path());

        let third = report["servers"][0]["evidence"]["tools_list_pages"][2]["digest"]
            .as_str()
            .expect("digest")
            .to_string();
        let path = evidence_dir.join(&third);
        assert!(path.is_file(), "page blob must exist at {}", path.display());
        std::fs::remove_file(&path).expect("remove the third page's blob");

        let err = match rederive(&results, None) {
            Err(e) => e,
            Ok(_) => panic!("a missing page blob must fail the re-derivation"),
        };
        assert!(err.to_string().contains(&third), "the error must name the digest: {err}");
    }

    /// A results file written **before** cursor following has no `tools_list_pages` and no
    /// `pagination` block. Re-deriving it must read its single response as one page and
    /// report the walk as `unrecorded` — **not** as `no_cursor`.
    ///
    /// The distinction is the whole point: such a file's tool list is page one, and whether
    /// the server had more pages is unknowable from it. Calling that "complete" would be the
    /// same substitution this project refuses when it insists on `unverifiable` over
    /// `holds`. The cost is stated rather than hidden: such a file no longer re-derives
    /// byte-for-byte (its servers are reclassified as truncated), which is acceptable
    /// precisely because no file of that shape exists in `results/` — the committed July
    /// data predates evidence persistence entirely and is already refused.
    #[test]
    fn a_pre_pagination_results_file_is_read_as_one_page_and_never_asserted_complete() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (results, _, _) = paginated_run(dir.path());

        // Strip the two P0-13 keys from the one-page server's record, leaving the P0-12 shape.
        let mut file: Value =
            serde_json::from_str(&std::fs::read_to_string(&results).expect("read")).expect("json");
        let record = file["servers"][1].as_object_mut().expect("record");
        record.remove("pagination");
        record["evidence"].as_object_mut().expect("evidence").remove("tools_list_pages");
        let legacy_shape = dir.path().join("legacy.json");
        std::fs::write(&legacy_shape, serde_json::to_string_pretty(&file).expect("write"))
            .expect("write");

        let outcome = rederive(&legacy_shape, None).expect("a pre-P0-13 file must still re-derive");
        let regenerated: Value = serde_json::from_str(&outcome.regenerated).expect("json");
        let server = &regenerated["servers"][1];

        assert_eq!(server["pagination"]["pages_fetched"], 1, "its one response is one page");
        assert_eq!(
            server["pagination"]["stopped_because"], "unrecorded",
            "this file predates cursor following and cannot claim it ran out of cursors"
        );
        assert_eq!(server["pagination"]["cursors_exhausted"], false);
        assert_eq!(
            server["failure_category"], "tools_list_truncated_unrecorded",
            "an unrecorded walk is not a complete one, so its tally is withheld rather than \
             published as if the page-one list were the whole list"
        );
    }

    /// Tool names repeated across pages are counted and published rather than swallowed.
    ///
    /// Not a bound and not an error: a cursor walk has no snapshot isolation in the
    /// protocol, so a server mutating its tool set mid-walk may legitimately re-list a tool.
    /// It is the one mechanical signal available that this happened, so it is a number on
    /// the record.
    #[test]
    fn tool_names_repeated_across_pages_are_counted_and_published() {
        let dir = tempfile::tempdir().expect("tempdir");
        let evidence_dir = dir.path().join("evidence");
        let store = Mutex::new(BlobStore::open(&evidence_dir).expect("open store"));
        let stop = StopFlag::default();

        // The same two tools on both pages, plus one genuinely new one.
        let servers = vec![ServerObservation {
            identity: identity("repeats"),
            observation: record_discovery(
                &store,
                &walk(
                    DiscoveryPath::Initialize,
                    "2025-11-25",
                    &[ALL, &format!("{ALL},{UNPINNABLE}")],
                    StopReason::NoCursor,
                ),
                &stop,
            ),
        }];
        let header = RunHeader {
            stage: "test",
            sample_size_requested: 1,
            candidates_selected: 1,
            attempted: 1,
            stop_reason: None,
            evidence_store: &evidence_dir,
            sample_seed: None,
        }
        .into_map()
        .expect("header");
        let report = build_report(&store.into_inner().expect("lock"), header, &servers)
            .expect("build");

        let server = &report["servers"][0];
        assert_eq!(server["outcome"], "success", "a repeat is a finding, not a failure");
        assert_eq!(server["tool_count"], 3, "distinct tools, each counted once");
        assert_eq!(server["pagination"]["tools_total"], 5, "every occurrence, as received");
        assert_eq!(server["pagination"]["tools_distinct"], 3);
        assert_eq!(server["pagination"]["duplicate_tool_names"], 2);
        assert_eq!(report["pagination"]["duplicate_tool_names"], 2);
        assert_eq!(report["tools_discovered"], 3);
    }

    /// Run one discovery through the live path and build its report.
    fn one_server_report(dir: &Path, name: &str, discovery: &Discovery) -> Value {
        let evidence_dir = dir.join("evidence");
        let store = Mutex::new(BlobStore::open(&evidence_dir).expect("open store"));
        let stop = StopFlag::default();
        let servers = vec![ServerObservation {
            identity: identity(name),
            observation: record_discovery(&store, discovery, &stop),
        }];
        let header = RunHeader {
            stage: "test",
            sample_size_requested: 1,
            candidates_selected: 1,
            attempted: 1,
            stop_reason: None,
            evidence_store: &evidence_dir,
            sample_seed: None,
        }
        .into_map()
        .expect("header");
        build_report(&store.into_inner().expect("lock"), header, &servers).expect("build")
    }

    /// **B1(a), the measured attack closed.** Forty pages serving the same two tools used to
    /// publish as `tool_count = 80`, `tools_discovered = 80` and `readOnlyHint.explicit = 80`
    /// — two real tools as eighty, one annotation inflated 40x — recorded as a clean
    /// success. The census now counts and tallies distinct names; the raw occurrence count is
    /// still published, beside it, so the de-duplication is visible rather than silent.
    #[test]
    fn forty_pages_of_the_same_two_tools_publish_as_two_tools() {
        let two = r#"{"name":"r","inputSchema":{},"annotations":{"readOnlyHint":true}},{"name":"w","inputSchema":{},"annotations":{"readOnlyHint":false}}"#;
        let dir = tempfile::tempdir().expect("tempdir");
        let pages = vec![two; discovery::paginate::MAX_PAGES];
        let report = one_server_report(
            dir.path(),
            "replayer",
            &walk(DiscoveryPath::Initialize, "2025-11-25", &pages, StopReason::NoCursor),
        );

        let server = &report["servers"][0];
        assert_eq!(server["outcome"], "success");
        assert_eq!(server["tool_count"], 2);
        assert_eq!(server["annotation_tally"]["readOnlyHint"]["explicit"], 2);
        assert_eq!(report["tools_discovered"], 2);
        assert_eq!(report["annotation_tally"]["readOnlyHint"]["explicit"], 2);
        assert_eq!(server["pagination"]["tools_total"], 80);
        assert_eq!(server["pagination"]["duplicate_tool_names"], 78);
        assert_eq!(server["pagination"]["conflicting_tool_definitions"], 0);
        // The pin is still over what was *received*, never the de-duplicated set.
        let (_, received_pin) = discovery::pin_pages(
            &pages.iter().map(|p| page(p, None)).collect::<Vec<_>>(),
        )
        .expect("pin");
        assert_eq!(server["server_pin"], received_pin.to_string());
    }

    /// **Also-fix 5.** A name re-listed with a *different* definition is counted once in the
    /// tally (first sighting wins) and surfaced as a conflict, which `duplicate_tool_names`
    /// alone cannot distinguish from a harmless re-listing.
    #[test]
    fn a_tool_redefined_mid_walk_is_tallied_once_and_published_as_a_conflict() {
        let first = r#"{"name":"t","inputSchema":{},"annotations":{"readOnlyHint":true}}"#;
        let second = r#"{"name":"t","inputSchema":{},"annotations":{"readOnlyHint":false}}"#;
        let dir = tempfile::tempdir().expect("tempdir");
        let report = one_server_report(
            dir.path(),
            "redefiner",
            &walk(DiscoveryPath::Initialize, "2025-11-25", &[first, second], StopReason::NoCursor),
        );
        let server = &report["servers"][0];
        assert_eq!(server["tool_count"], 1);
        assert_eq!(server["annotation_tally"]["readOnlyHint"]["explicit"], 1);
        assert_eq!(server["pagination"]["duplicate_tool_names"], 1);
        assert_eq!(server["pagination"]["conflicting_tool_definitions"], 1);
        assert_eq!(report["pagination"]["conflicting_tool_definitions"], 1);
    }

    /// **B3.** A walk its own server truncated with an unfollowable cursor is filed under
    /// `tools_list_truncated_by_server`, carries `nonconformance_flag: true` with the reason,
    /// and is counted under `server_nonconformant` — never pooled with the harness-side
    /// buckets, because only those are the harness's to subtract.
    #[test]
    fn a_server_truncated_walk_is_partitioned_and_flagged() {
        for reason in [
            StopReason::CursorEmpty,
            StopReason::CursorNotAString,
            StopReason::CursorTooLong,
            StopReason::CursorRepeated,
        ] {
            let dir = tempfile::tempdir().expect("tempdir");
            let report = one_server_report(
                dir.path(),
                "dodger",
                &walk(DiscoveryPath::Initialize, "2025-11-25", &[ALL], reason),
            );
            let server = &report["servers"][0];
            assert_eq!(server["failure_category"], "tools_list_truncated_by_server", "{reason}");
            assert_eq!(server["nonconformance_flag"], true, "{reason}");
            assert_eq!(server["nonconformance_reason"], reason.as_str());
            assert_eq!(server["pagination"]["truncation_attribution"], "server_nonconformant");
            let by = &report["pagination"]["servers_truncated_by"];
            assert_eq!(by["server_nonconformant"], 1, "{reason}");
            for other in ["harness_bound", "harness_budget", "ambiguous", "unrecorded"] {
                assert_eq!(by[other], 0, "{reason}: seeded, and not pooled into {other}");
            }
            assert_eq!(report["pagination"]["servers_nonconformance_flagged"], 1);
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let report = one_server_report(
            dir.path(),
            "budget",
            &walk(DiscoveryPath::Initialize, "2025-11-25", &[ALL], StopReason::BudgetExhausted),
        );
        assert_eq!(report["servers"][0]["failure_category"], "tools_list_truncated_by_budget");
        assert_eq!(report["servers"][0]["nonconformance_flag"], false);
        assert_eq!(report["pagination"]["servers_truncated_by"]["harness_budget"], 1);
    }

    /// **Also-fix 1.** A `coverage_extraction` failure used to return before its walk was
    /// counted, so it was absent from the pagination denominator while its own record said
    /// the walk was finished, and per-server `pages_fetched` no longer summed to
    /// `pages_fetched_total`. Every exit now counts it.
    #[test]
    fn a_coverage_extraction_failure_is_still_in_the_pagination_denominator() {
        let dir = tempfile::tempdir().expect("tempdir");
        let report = one_server_report(
            dir.path(),
            "unreadable",
            &walk(DiscoveryPath::Initialize, "2025-11-25", &[ALL, "not a tool"], StopReason::NoCursor),
        );
        let server = &report["servers"][0];
        assert_eq!(server["failure_category"], "coverage_extraction");
        assert_eq!(server["pagination"]["pages_fetched"], 2);
        assert_eq!(report["pagination"]["servers"], 1);
        assert_eq!(report["pagination"]["pages_fetched_total"], 2);
        assert_eq!(report["pagination"]["stopped_because"]["no_cursor"], 1);
    }

    /// **B4, the measured amplification refused.** A results file whose record references
    /// one tiny blob far more times than any capture can produce — 200 pages against a
    /// capture bound of 40 — used to re-derive, holding every reference resident (184 MB
    /// from a 22 KB file and a 922 KB store). It is now refused as a malformed record before
    /// a blob is read; so is a record *declaring* more bytes than any capture can store.
    #[test]
    fn a_crafted_bundle_cannot_amplify_rederivation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (results, _, _) = paginated_run(dir.path());
        let original: Value =
            serde_json::from_str(&std::fs::read_to_string(&results).expect("read")).expect("json");
        let first = original["servers"][0]["evidence"]["tools_list_pages"][0].clone();

        let mut crafted = original.clone();
        crafted["servers"][0]["evidence"]["tools_list_pages"] =
            Value::Array(vec![first.clone(); 200]);
        let err = split_results(crafted).err().expect("200 pages must be refused");
        assert!(err.contains("more than the 40"), "{err}");

        let mut at_bound = original.clone();
        at_bound["servers"][0]["evidence"]["tools_list_pages"] =
            Value::Array(vec![first.clone(); discovery::paginate::MAX_PAGES]);
        assert!(split_results(at_bound).is_ok(), "exactly the capture bound is still readable");

        let mut overstated = original;
        let mut huge = first;
        huge["bytes"] = Value::from(u64::MAX / 2);
        overstated["servers"][0]["evidence"]["tools_list_pages"] = Value::Array(vec![huge]);
        let err = split_results(overstated).err().expect("declared bytes must be refused");
        assert!(err.contains("declares"), "{err}");
    }

    /// **Also-fix 6.** `page_extensions_denied` is a lower bound on the budget-truncated
    /// records, not an equality: a record can lose its `pagination` block to a later failure,
    /// and `census-pin-stability` runs two walks under one permit and writes no block at
    /// all. Both used to make the guard reject a correct file. The direction that matters —
    /// a header under-reporting the refusals its own records show — is still caught.
    #[test]
    fn the_page_refusal_guard_is_a_lower_bound_not_an_equality() {
        let report = |claimed: u64, servers: Value| {
            json!({
                "pacing": {
                    "applies": true,
                    "per_discovery_reservation": 5,
                    "requests_sent": 0,
                    "waited_ms_total": 0,
                    "sequences_denied_by_budget": 0,
                    "max_requests_to_one_registrable_domain": 0,
                    "registrable_domains_contacted": 0,
                    "overshoot_requests": 0,
                    "page_extensions_denied": claimed,
                },
                "servers": servers,
            })
        };
        // pin_stability's shape: two refusals under one permit, and no pagination block.
        assert!(verify_pacing_self_consistent(&report(2, json!([{ "name": "a" }]))).is_ok());
        // A refusal followed by evidence_store_error: the header has it, the record does not.
        assert!(
            verify_pacing_self_consistent(&report(
                1,
                json!([{ "failure_category": "evidence_store_error" }])
            ))
            .is_ok()
        );
        let truncated = json!([{ "pagination": { "stopped_because": "budget_exhausted" } }]);
        assert!(verify_pacing_self_consistent(&report(1, truncated.clone())).is_ok());
        let problems = verify_pacing_self_consistent(&report(0, truncated))
            .expect_err("a header under-reporting its own records' refusals is rejected");
        assert!(problems[0].contains("page_extensions_denied"), "{problems:?}");
    }

    /// A malformed later page fails the whole server as `coverage_extraction`, naming the
    /// page — not silently tallying the pages that did parse, which would publish a partial
    /// list under a complete-looking record.
    #[test]
    fn a_malformed_later_page_fails_the_server_and_names_the_page() {
        let dir = tempfile::tempdir().expect("tempdir");
        let evidence_dir = dir.path().join("evidence");
        let store = Mutex::new(BlobStore::open(&evidence_dir).expect("open store"));
        let stop = StopFlag::default();

        let mut fixture =
            walk(DiscoveryPath::Initialize, "2025-11-25", &[ALL, NONE], StopReason::NoCursor);
        fixture.tools_list_pages[1] = br#"{"jsonrpc":"2.0","id":1,"result":{}}"#.to_vec();
        let servers = vec![ServerObservation {
            identity: identity("bad-second-page"),
            observation: record_discovery(&store, &fixture, &stop),
        }];
        let header = RunHeader {
            stage: "test",
            sample_size_requested: 1,
            candidates_selected: 1,
            attempted: 1,
            stop_reason: None,
            evidence_store: &evidence_dir,
            sample_seed: None,
        }
        .into_map()
        .expect("header");
        let report = build_report(&store.into_inner().expect("lock"), header, &servers)
            .expect("build");

        let server = &report["servers"][0];
        assert_eq!(server["outcome"], "failed");
        assert_eq!(server["failure_category"], "coverage_extraction");
        assert!(
            server["failure_detail"].as_str().expect("detail").starts_with("page 2:"),
            "the detail must say which page: {}",
            server["failure_detail"]
        );
        assert!(server.get("tool_count").is_none());
        assert_eq!(report["tools_discovered"], 0);
    }

}
