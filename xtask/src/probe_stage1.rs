//! Track B validation run: sample live Class B servers (same sampling methodology as
//! `census_stage1`), and for each one that discovers successfully and declares at least one
//! tool, run B-01's protocol-probe protocol (`probe::probe_read_only_hint` /
//! `probe::probe_idempotent_hint`) against its first tool. Persists every server,
//! tool-snapshot, and verdict produced into a real F-06 metadata DB (`store::db`), so B-02's
//! claim — "every verdict carries `oracle`" — is demonstrated against real rows, not just
//! asserted in a unit test.
//!
//! **This script invokes tools on live, third-party servers.** Unlike `census_stage1`
//! (discovery only — `initialize` + `tools/list`, exactly what any MCP client does on
//! connect), a probe run that finds a usable surface calls `tools/call` for real, with
//! placeholder arguments ([`probe::synthesize_arguments`]), against infrastructure this
//! project does not control and cannot contain (architecture.md §2 — Class B servers are
//! remote, uncontainable by construction). That is Track B's entire premise, authorised at
//! the project level (design.md §3's trust model), not something this script second-guesses
//! — but it is why the sample stays small, why only one tool per server is ever invoked
//! (not every tool the server declares), and why every attempted invocation is logged here,
//! never silent.
//!
//! Sequential requests, not concurrent — same posture as `census_stage1`, for the same
//! reason: unrelated third-party hosts, no reason to burst them.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use datamodel::{Annotation, ContainabilityClass, Oracle};
use discovery::{DiscoveryClient, DiscoveryError};
use intake::catalogue;
use intake::registry::RegistryClient;
use probe::{ProbeError, ProbeTarget};
use serde_json::Value;
use store::aggregate::{self, VerdictSummary};
use store::db::{self, ServerRecord, ToolSnapshotRecord, VerdictRecord};

use crate::census_stage1::{Candidate, class_b_candidate, stable_hash};

/// Per-request timeout, tuned the same way `census_stage1` tuned its own: short enough
/// that a handful of unresponsive hosts don't dominate total run time at sample sizes in
/// the hundreds.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(12);

struct ToolEntry {
    name: String,
    input_schema: Value,
    annotations: Value,
}

fn declared_bool(annotations: &Value, key: &str) -> bool {
    annotations.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn explicit(annotations: &Value, key: &str) -> bool {
    annotations.get(key).is_some()
}

/// Parse just enough of a raw `tools/list` response for this script's own purposes —
/// deliberately not reusing `census::coverage`'s private `Annotations` shape, since this
/// needs the full raw `annotations` value (for `TOOL_SNAPSHOT.annotations_raw`) and the
/// `inputSchema` (for argument synthesis), neither of which that module exposes. Same
/// re-parse pattern `xtask::dump_tools` already uses for its own, different, purpose.
fn parse_tools(tools_list_raw: &[u8]) -> Result<Vec<ToolEntry>, Box<dyn std::error::Error>> {
    let envelope: Value = serde_json::from_slice(tools_list_raw)?;
    let tools = envelope["result"]["tools"].as_array().cloned().unwrap_or_default();
    Ok(tools
        .into_iter()
        .filter_map(|t| {
            let name = t.get("name")?.as_str()?.to_string();
            let input_schema =
                t.get("inputSchema").cloned().unwrap_or_else(|| Value::Object(serde_json::Map::default()));
            let annotations = t.get("annotations").cloned().unwrap_or(Value::Null);
            Some(ToolEntry { name, input_schema, annotations })
        })
        .collect())
}

enum DiscoverOutcome {
    Success { tools_list_raw: Vec<u8>, spec_revision: String, tools: Vec<ToolEntry> },
    Failed { category: &'static str, detail: String },
}

fn discover(url: &str) -> DiscoverOutcome {
    let mut client = DiscoveryClient::http_with_timeout(url.to_string(), REQUEST_TIMEOUT);
    match client.discover() {
        Ok(discovery) => match parse_tools(&discovery.tools_list_raw) {
            Ok(tools) => DiscoverOutcome::Success {
                tools_list_raw: discovery.tools_list_raw,
                spec_revision: discovery.negotiated_spec_revision,
                tools,
            },
            Err(e) => DiscoverOutcome::Failed { category: "tool_parse", detail: e.to_string() },
        },
        Err(DiscoveryError::Transport(msg)) => DiscoverOutcome::Failed { category: "transport", detail: msg },
        Err(DiscoveryError::Io(e)) => DiscoverOutcome::Failed { category: "io", detail: e.to_string() },
        Err(DiscoveryError::Protocol(msg)) => DiscoverOutcome::Failed { category: "protocol", detail: msg },
        Err(DiscoveryError::ServerError { code, message }) => {
            DiscoverOutcome::Failed { category: "server_error", detail: format!("{code}: {message}") }
        }
    }
}

fn now_iso() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    // Not a full ISO-8601 formatter (no chrono dependency for one call site) — a
    // sortable, human-legible-enough stand-in; every other `_at` column in this schema is
    // free-form `TEXT` for exactly this reason (F-06 doesn't mandate a format).
    format!("unix:{secs}")
}

/// Run one probe protocol against `target`, log the attempt, and — only for `Ok` — insert
/// the resulting verdict. `Err` is a run that produced nothing (see the `probe::runner`
/// module doc comment on why that's not itself an `Unverifiable` outcome) and is logged as
/// a failure, never coerced into a verdict either way.
#[allow(clippy::too_many_arguments)]
fn run_and_record(
    conn: &rusqlite::Connection,
    label: &str,
    protocol_version: &str,
    run: impl FnOnce() -> Result<probe::ProbeAssessment, ProbeError>,
    snapshot_id: &str,
    annotation: Annotation,
    declared: bool,
    verdict_id: &str,
    invocations_attempted: &mut usize,
    outcomes: &mut Vec<(Annotation, datamodel::Outcome, Option<String>)>,
) {
    *invocations_attempted += 1;
    match run() {
        Ok(assessment) => {
            eprintln!(
                "probe-stage1:     {label}: {:?} (reason: {:?})",
                assessment.outcome, assessment.reason
            );
            let reason_code = assessment.reason.as_ref().map(|r| r.as_db_str().to_string());
            db::insert_verdict(
                conn,
                &VerdictRecord {
                    verdict_id,
                    snapshot_id,
                    annotation,
                    declared: if declared { "true" } else { "false" },
                    outcome: assessment.outcome,
                    reason_code: reason_code.as_deref(),
                    oracle: Oracle::ProtocolProbe,
                    ruleset_version: None,
                    protocol_version,
                    derived_at: &now_iso(),
                },
            )
            .expect("insert_verdict");
            outcomes.push((annotation, assessment.outcome, reason_code));
        }
        Err(e) => {
            eprintln!("probe-stage1:     {label}: FAILED — {e}");
        }
    }
}

/// Run the Track B validation sweep: sample `sample_size` Class B servers (same
/// methodology as `census_stage1::run`), probe one tool per server that discovers
/// successfully and declares at least one tool, and persist every server, tool snapshot,
/// and verdict into `results/conformance/track_b_probe.sqlite3`. Writes a summary to
/// `results/conformance/track_b_protocol_probe.json`.
///
/// # Errors
/// A registry fetch failing outright, opening/migrating the metadata DB failing, or writing
/// the summary file failing. A single sampled server's own discovery or probe failing is not
/// an error here — it is logged and excluded from the summary instead.
///
/// # Panics
/// Panics if the metadata DB path (a fixed literal this function itself constructs) has no
/// parent directory or is not valid UTF-8 — both unreachable in practice.
pub fn run(sample_size: usize) -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("probe-stage1: fetching the registry to find all Class B candidates...");

    let registry = RegistryClient::new();
    let mut all_class_b: Vec<Candidate> = Vec::new();
    registry.fetch_all(100, Duration::from_millis(200), |page| {
        for raw in &page.entries_raw {
            let outcome = catalogue::ingest(raw);
            if let Some(candidate) = class_b_candidate(&outcome) {
                all_class_b.push(candidate);
            }
        }
        true
    })?;

    eprintln!(
        "probe-stage1: {} Class B candidates found; selecting {sample_size} by stable hash \
         (same methodology as census_stage1)...",
        all_class_b.len()
    );
    all_class_b.sort_by_key(|c| stable_hash(&c.name));
    let candidates: Vec<Candidate> = all_class_b.into_iter().take(sample_size).collect();

    let db_path = Path::new("results/conformance/track_b_probe.sqlite3");
    let _ = std::fs::remove_file(db_path); // fresh DB each run — this script's own dataset, not cumulative
    std::fs::create_dir_all(db_path.parent().expect("has a parent"))?;
    let conn = db::open_and_migrate(db_path.to_str().expect("utf8 path"))?;

    let mut discovery_attempted = 0usize;
    let mut discovery_succeeded = 0usize;
    let mut discovery_failure_categories: std::collections::BTreeMap<&'static str, usize> = std::collections::BTreeMap::new();
    let mut servers_with_zero_tools = 0usize;
    let mut servers_probed = 0usize;
    let mut invocations_attempted = 0usize;
    let mut outcomes: Vec<(Annotation, datamodel::Outcome, Option<String>)> = Vec::new();
    let mut server_reports = Vec::new();

    for (i, candidate) in candidates.iter().enumerate() {
        discovery_attempted += 1;
        eprintln!("probe-stage1: [{}/{}] {} ({})", i + 1, candidates.len(), candidate.name, candidate.url);

        let outcome = discover(&candidate.url);
        let (tools_list_raw, spec_revision, tools) = match outcome {
            DiscoverOutcome::Success { tools_list_raw, spec_revision, tools } => {
                discovery_succeeded += 1;
                (tools_list_raw, spec_revision, tools)
            }
            DiscoverOutcome::Failed { category, detail } => {
                *discovery_failure_categories.entry(category).or_insert(0) += 1;
                server_reports.push(serde_json::json!({
                    "name": candidate.name, "url": candidate.url,
                    "outcome": "discovery_failed", "category": category, "detail": detail,
                }));
                continue;
            }
        };

        // The trust model is "ASSUMED HOSTILE" (design.md §3) — a server that returns a
        // JSON-RPC *success* envelope whose `result` doesn't actually match the
        // `tools/list` shape `pin_tools` requires (missing `tools`, wrong types, ...) is
        // exactly the kind of malformed-but-not-erroring response that model predicts.
        // This must degrade to a per-server failure like every other discovery problem,
        // never abort the whole sweep — found the hard way running this against the live
        // registry: without this branch, one such server killed a 500-server run at #213.
        let tool_pins = match discovery::pin_tools(&tools_list_raw) {
            Ok((pins, _server_pin)) => pins,
            Err(e) => {
                *discovery_failure_categories.entry("pin_failed").or_insert(0) += 1;
                server_reports.push(serde_json::json!({
                    "name": candidate.name, "url": candidate.url,
                    "outcome": "discovery_failed", "category": "pin_failed", "detail": e.to_string(),
                }));
                continue;
            }
        };

        let server_id = format!("probe-b-{i}");
        db::insert_server(
            &conn,
            &ServerRecord {
                server_id: &server_id,
                source_uri: &candidate.url,
                containability_class: ContainabilityClass::B,
                spec_revision: &spec_revision,
            },
        )?;

        let observed_at = now_iso();
        let mut snapshot_ids = Vec::with_capacity(tools.len());
        for (j, (tool, pin)) in tools.iter().zip(tool_pins.iter()).enumerate() {
            let snapshot_id = format!("{server_id}-tool-{j}");
            db::insert_tool_snapshot(
                &conn,
                &ToolSnapshotRecord {
                    snapshot_id: &snapshot_id,
                    server_id: &server_id,
                    tool_name: &tool.name,
                    metadata_pin: &pin.pin.to_string(),
                    annotations_raw: &tool.annotations.to_string(),
                    readonly_explicit: explicit(&tool.annotations, "readOnlyHint"),
                    destructive_explicit: explicit(&tool.annotations, "destructiveHint"),
                    idempotent_explicit: explicit(&tool.annotations, "idempotentHint"),
                    openworld_explicit: explicit(&tool.annotations, "openWorldHint"),
                    observed_at: &observed_at,
                },
            )?;
            snapshot_ids.push(snapshot_id);
        }

        if tools.is_empty() {
            servers_with_zero_tools += 1;
            server_reports.push(serde_json::json!({
                "name": candidate.name, "url": candidate.url, "outcome": "no_tools_declared",
            }));
            continue;
        }

        // Probe exactly one tool per server — the first one declared — to bound both the
        // request volume against any one third party and the number of real invocations
        // this run performs in total. See the module doc comment.
        servers_probed += 1;
        let probed_tool = &tools[0];
        let snapshot_id = snapshot_ids[0].clone();
        eprintln!("probe-stage1:   probing tool `{}`...", probed_tool.name);

        let declared_read_only = declared_bool(&probed_tool.annotations, "readOnlyHint");
        let declared_idempotent = declared_bool(&probed_tool.annotations, "idempotentHint");

        let ro_target = ProbeTarget {
            endpoint: &candidate.url,
            tool_name: &probed_tool.name,
            input_schema: &probed_tool.input_schema,
            timeout: REQUEST_TIMEOUT,
        };
        run_and_record(
            &conn,
            "readOnlyHint",
            &spec_revision,
            || probe::probe_read_only_hint(&ro_target, declared_read_only),
            &snapshot_id,
            Annotation::ReadOnlyHint,
            declared_read_only,
            &format!("{snapshot_id}-readOnlyHint-probe"),
            &mut invocations_attempted,
            &mut outcomes,
        );

        let idem_target = ProbeTarget {
            endpoint: &candidate.url,
            tool_name: &probed_tool.name,
            input_schema: &probed_tool.input_schema,
            timeout: REQUEST_TIMEOUT,
        };
        run_and_record(
            &conn,
            "idempotentHint",
            &spec_revision,
            || probe::probe_idempotent_hint(&idem_target, declared_idempotent),
            &snapshot_id,
            Annotation::IdempotentHint,
            declared_idempotent,
            &format!("{snapshot_id}-idempotentHint-probe"),
            &mut invocations_attempted,
            &mut outcomes,
        );

        server_reports.push(serde_json::json!({
            "name": candidate.name, "url": candidate.url, "outcome": "probed",
            "probed_tool": probed_tool.name, "tool_count": tools.len(),
        }));
    }

    // Dogfood B-03's guard against the real rows this run just wrote: read every verdict
    // back from the DB, aggregate it correctly, and prove the aggregate matches the
    // records exactly — the same property the unit tests prove against synthetic data,
    // now checked against this run's actual output.
    let stored_rows = db::list_verdicts(&conn)?;
    let summaries: Vec<VerdictSummary> = stored_rows.into_iter().map(Into::into).collect();
    let aggregate_rows = aggregate::aggregate(&summaries);
    let report = aggregate::to_report_rows(&aggregate_rows);
    aggregate::verify_report_matches_records(&report, &summaries)
        .expect("B-03 guard: this run's own report must never mix oracles — it only ever produces protocol_probe");

    eprintln!(
        "probe-stage1: {discovery_attempted} attempted, {discovery_succeeded} discovered, \
         {servers_with_zero_tools} with zero tools, {servers_probed} probed, \
         {invocations_attempted} probe protocols run"
    );
    for (category, count) in &discovery_failure_categories {
        eprintln!("probe-stage1:   discovery failure — {category}: {count}");
    }
    for row in &aggregate_rows {
        eprintln!(
            "probe-stage1:   {} / {} / {}: {}",
            row.annotation, row.oracle, row.outcome, row.count
        );
    }

    let generated_at_unix = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let output = serde_json::json!({
        "generated_at_unix": generated_at_unix,
        "track": "B — protocol-probe oracle (B-01/B-02/B-03 validation)",
        "sample_size_requested": sample_size,
        "class_b_candidates_found": candidates.len(),
        "discovery_attempted": discovery_attempted,
        "discovery_succeeded": discovery_succeeded,
        "discovery_failure_categories": discovery_failure_categories,
        "servers_with_zero_tools": servers_with_zero_tools,
        "servers_probed": servers_probed,
        "probe_protocols_run": invocations_attempted,
        "aggregate": aggregate_rows.iter().map(|r| serde_json::json!({
            "annotation": r.annotation.to_string(),
            "oracle": r.oracle.to_string(),
            "outcome": r.outcome.to_string(),
            "count": r.count,
        })).collect::<Vec<_>>(),
        "servers": server_reports,
    });

    let out_path = Path::new("results/conformance/track_b_protocol_probe.json");
    std::fs::write(out_path, serde_json::to_string_pretty(&output)?)?;
    eprintln!("probe-stage1: wrote {}", out_path.display());
    eprintln!("probe-stage1: wrote {}", db_path.display());

    Ok(())
}
