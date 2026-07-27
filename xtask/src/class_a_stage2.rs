//! Stage 2 census (P0-06's Class A half): discover live Class A (locally-launchable) MCP
//! servers by actually executing their declared package inside a Docker container and
//! speaking `initialize`+`tools/list` over its stdio.
//!
//! Per the "Staging" note at the top of the Phase 0 block in tasks.md: discovering a Class A
//! server means executing it, unlike Stage 0/1 — but this needs only *containment*, not the
//! observation machinery Phase 1+'s hand-rolled sandbox exists for. No changeset is taken,
//! no syscalls are watched; this module reads exactly what `tools/list` says, the same as
//! Stage 1 does for Class B, and never what the tool does. A stock container runtime
//! (`docker run`, resource-capped) is adequate containment for that narrower job — this is a
//! deliberate architectural choice (architecture.md's Stage 2 note), not a shortcut around
//! ADR-007's hand-rolled-sandbox decision, which is about *observation* quality.
//!
//! Every attempt here is containerized; there is no bare-host execution path in this module.
//! Every result record carries `"execution_provenance": "containerized_docker"` so Stage 2
//! data is never silently pooled with Stage 0/1 (registry-only / Class B) results, matching
//! the discipline ADR-002 already requires between oracles.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use census::coverage;
use datamodel::ContainabilityClass;
use discovery::{DiscoveryClient, DiscoveryError};
use intake::catalogue::{self, IngestOutcome, ResolvedTarget};
use intake::classify;
use intake::registry::RegistryClient;

/// Pre-pulled once before the sweep, not per server — a multi-hundred-MB base image
/// re-downloaded on every npm/pypi candidate would dominate sweep time for no reason.
/// `docker pull` of an already-cached image is fast, so re-running this xtask doesn't
/// re-download anything either.
const NODE_IMAGE: &str = "node:22-alpine";
const UV_IMAGE: &str = "ghcr.io/astral-sh/uv:python3.12-alpine";

/// Per-attempt hard wall-clock deadline, enforced by [`DiscoveryClient::stdio_with_timeout`].
/// Measured empirically against a real npm-published MCP server
/// (`@modelcontextprotocol/server-everything`) with the base image already cached: a cold
/// `npx` install plus the full `initialize`/`tools/list` round trip completed in well under
/// 15 seconds. 45s leaves headroom for a slower package or registry without letting one hung
/// or deliberately stalling server dominate the sweep — this is a watchdog for the discovery
/// call, not a containment mechanism (the resource caps below are that).
const PER_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(45);

/// Resource caps applied to every container, regardless of registry type. design.md §3: the
/// tool under test is assumed actively hostile, and Stage 2 executes real, arbitrary,
/// unauthenticated third-party code to do its job — these bound a fork bomb or a memory hog
/// without needing the full cgroup/seccomp machinery Phase 2/4 build. Not a substitute for
/// that machinery; adequate for a census that only ever reads `tools/list`.
const RESOURCE_CAPS: [&str; 3] = ["--memory=256m", "--pids-limit=256", "--cpus=1"];

struct Candidate {
    name: String,
    registry_type: String,
    identifier: String,
    version: String,
}

/// If this ingest outcome classifies as Class A, its name and first declared package
/// target. Mirrors `census_stage1::class_b_candidate`'s shape for the opposite class.
fn class_a_candidate(outcome: &IngestOutcome) -> Option<Candidate> {
    if classify::classify(outcome).class != ContainabilityClass::A {
        return None;
    }
    let IngestOutcome::Resolved(server) = outcome else {
        return None;
    };
    server.targets.iter().find_map(|t| match t {
        ResolvedTarget::Package { registry_type, identifier, version, .. } => Some(Candidate {
            name: server.name.clone(),
            registry_type: registry_type.clone(),
            identifier: identifier.clone(),
            version: version.clone(),
        }),
        ResolvedTarget::Endpoint { .. } => None,
    })
}

/// Build the `docker run` argument vector that launches one candidate's declared package
/// inside a fresh, capped, uniquely-named container speaking MCP over stdio — or `None` if
/// this registry type has no container invocation defined yet. `None` is recorded as an
/// honest `unsupported_registry_type` failure, never silently skipped from the sample.
///
/// `--name` is load-bearing, not decoration: see [`cleanup_container`] for why a name this
/// module controls, rather than a daemon-assigned one, is what makes guaranteed teardown
/// possible.
fn docker_args(candidate: &Candidate, container_name: &str) -> Option<Vec<String>> {
    let mut args = vec![
        "run".to_string(),
        "-i".to_string(),
        "--rm".to_string(),
        "--name".to_string(),
        container_name.to_string(),
    ];
    args.extend(RESOURCE_CAPS.iter().map(|s| (*s).to_string()));

    match candidate.registry_type.as_str() {
        "npm" => {
            args.push(NODE_IMAGE.to_string());
            args.push("npx".to_string());
            args.push("-y".to_string());
            args.push(format!("{}@{}", candidate.identifier, candidate.version));
            Some(args)
        }
        "pypi" => {
            args.push(UV_IMAGE.to_string());
            args.push("uvx".to_string());
            args.push(format!("{}=={}", candidate.identifier, candidate.version));
            Some(args)
        }
        "oci" => {
            // The identifier *is* the image reference; no wrapper image or installer step
            // — run the declared image directly and let its own entrypoint speak MCP.
            args.push(candidate.identifier.clone());
            Some(args)
        }
        // cargo, nuget, mcpb, and anything future: no container invocation built yet.
        _ => None,
    }
}

enum Attempt {
    Success { tool_count: usize, tools: Vec<coverage::ToolCoverage> },
    Failed { category: &'static str, detail: String },
}

/// Force-remove a container by the name this module assigned it, ignoring the result.
///
/// This is the real fix for a containment bug found running the seed sample: the
/// [`DiscoveryClient::stdio_with_timeout`] watchdog kills the *host-side `docker run` CLI
/// process* on timeout via `kill -9`. That unblocks discovery correctly, but a SIGKILL'd
/// CLI process gets no chance to tell the daemon to honour `--rm` — the container it
/// launched keeps running, orphaned, on the host. Four such containers (all from timed-out
/// attempts in this exact sweep) were found still `Up 3 hours` after the run finished and
/// had to be cleaned up by hand. `docker rm -f` by name, called unconditionally after every
/// attempt regardless of outcome, is independent of whatever state the CLI process or the
/// container's own process ended up in — it is the actual teardown guarantee design.md §3
/// requires ("containment is a correctness requirement, not a convenience"), where relying
/// on `--rm` alone was not.
fn cleanup_container(name: &str) {
    // Errors are expected and fine: a container that exited/was removed cleanly via its own
    // `--rm` never existed to be force-removed. Only the removal-when-needed case matters.
    let _ = std::process::Command::new("docker").args(["rm", "-f", name]).output();
}

fn attempt(candidate: &Candidate, container_name: &str) -> Attempt {
    let Some(args) = docker_args(candidate, container_name) else {
        return Attempt::Failed {
            category: "unsupported_registry_type",
            detail: candidate.registry_type.clone(),
        };
    };
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();

    let mut client = match DiscoveryClient::stdio_with_timeout("docker", &arg_refs, PER_ATTEMPT_TIMEOUT) {
        Ok(c) => c,
        Err(e) => return Attempt::Failed { category: "spawn", detail: e.to_string() },
    };
    let result = match client.discover() {
        Ok(discovery) => match coverage::tool_coverage(&discovery.tools_list_raw) {
            Ok(tools) => Attempt::Success { tool_count: tools.len(), tools },
            Err(e) => Attempt::Failed { category: "coverage_extraction", detail: e.to_string() },
        },
        Err(DiscoveryError::Transport(msg)) => Attempt::Failed { category: "transport", detail: msg },
        // A killed-at-timeout container's closed pipe surfaces here too (the watchdog
        // kills the process; recv_line then sees EOF) — indistinguishable from an
        // ordinary early exit at this layer, so it is reported as one category rather than
        // guessed apart.
        Err(DiscoveryError::Io(e)) => Attempt::Failed { category: "io_or_timeout", detail: e.to_string() },
        Err(DiscoveryError::Protocol(msg)) => Attempt::Failed { category: "protocol", detail: msg },
        Err(DiscoveryError::ServerError { code, message }) => {
            Attempt::Failed { category: "server_error", detail: format!("{code}: {message}") }
        }
    };
    // Unconditional, regardless of how discover() came out — see cleanup_container's doc.
    cleanup_container(container_name);
    result
}

/// Deterministically hash `name` into a `u64` — same rationale and construction as
/// `census_stage1::stable_hash`: `DefaultHasher`'s keys are fixed, so this is stable across
/// runs and processes, unlike `HashMap`'s randomised `RandomState`.
fn stable_hash(name: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    name.hash(&mut hasher);
    hasher.finish()
}

/// Best-effort pre-pull of the two wrapper base images, once, before the sweep. Failure here
/// is not fatal to the run — npm/pypi attempts against an unpulled image would simply pull
/// on first use and pay the cost per-attempt instead of up front — but pre-pulling keeps
/// per-attempt timing comparable to what `PER_ATTEMPT_TIMEOUT` was tuned against.
fn prepull_base_images() {
    for image in [NODE_IMAGE, UV_IMAGE] {
        eprintln!("census-stage2-class-a: pre-pulling {image}...");
        match std::process::Command::new("docker").args(["pull", image]).status() {
            Ok(status) if status.success() => {}
            Ok(status) => eprintln!("census-stage2-class-a:   pull of {image} exited {status}, continuing anyway"),
            Err(e) => eprintln!("census-stage2-class-a:   failed to run docker pull for {image}: {e}, continuing anyway"),
        }
    }
}

/// Run the Stage 2 census: sample `sample_size` Class A servers from the live registry and
/// attempt discovery against each by actually launching its declared package in a fresh,
/// capped Docker container. Sequential, not concurrent — same politeness posture as Stage 1,
/// and one container at a time keeps resource accounting simple.
pub fn run(sample_size: usize) -> Result<(), Box<dyn std::error::Error>> {
    if std::process::Command::new("docker").arg("info").output().map(|o| !o.status.success()).unwrap_or(true) {
        return Err("docker is not available or the daemon is not running (`docker info` failed) \
                     — Stage 2 needs a working container runtime; see docs/tasks.md P0-06 for the \
                     blocker this produces if it stays down"
            .into());
    }

    prepull_base_images();

    eprintln!("census-stage2-class-a: fetching the registry to find all Class A candidates...");
    let registry = RegistryClient::new();
    let mut all_class_a = Vec::new();
    registry.fetch_all(100, Duration::from_millis(200), |page| {
        for raw in &page.entries_raw {
            let outcome = catalogue::ingest(raw);
            if let Some(candidate) = class_a_candidate(&outcome) {
                all_class_a.push(candidate);
            }
        }
        true // scan the whole registry — early-stopping biases the sample, per Stage 1's fix
    })?;

    eprintln!(
        "census-stage2-class-a: {} Class A candidates found; selecting {sample_size} by stable hash...",
        all_class_a.len()
    );
    all_class_a.sort_by_key(|c| stable_hash(&c.name));
    let candidates: Vec<Candidate> = all_class_a.into_iter().take(sample_size).collect();

    eprintln!(
        "census-stage2-class-a: attempting containerized discovery against {} Class A servers...",
        candidates.len()
    );

    // Disambiguates container names across separate invocations of this xtask (e.g. the
    // pilot run and the full sweep both running index 0..N) so a name collision can never
    // make cleanup_container remove the wrong run's container.
    let sweep_id = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();

    let mut all_tools: Vec<coverage::ToolCoverage> = Vec::new();
    let mut server_results = Vec::new();
    let mut succeeded = 0usize;
    let mut failure_categories: std::collections::BTreeMap<&'static str, usize> = std::collections::BTreeMap::new();

    for (i, candidate) in candidates.iter().enumerate() {
        eprintln!(
            "census-stage2-class-a: [{}/{}] {} ({}:{})",
            i + 1,
            candidates.len(),
            candidate.name,
            candidate.registry_type,
            candidate.identifier
        );
        // A name this module controls (not daemon-assigned) is what makes cleanup_container
        // able to target the right container after a watchdog kill — see its doc comment.
        // Includes the loop index for readability and generated_at_unix for uniqueness
        // across separate runs of this xtask that might otherwise race on stale names.
        let container_name = format!("mcp-conf-stage2-{i}-{}", sweep_id);
        match attempt(candidate, &container_name) {
            Attempt::Success { tool_count, tools } => {
                succeeded += 1;
                all_tools.extend(tools);
                server_results.push(serde_json::json!({
                    "name": candidate.name,
                    "registry_type": candidate.registry_type,
                    "identifier": candidate.identifier,
                    "version": candidate.version,
                    "execution_provenance": "containerized_docker",
                    "outcome": "success",
                    "tool_count": tool_count,
                }));
            }
            Attempt::Failed { category, detail } => {
                *failure_categories.entry(category).or_insert(0) += 1;
                server_results.push(serde_json::json!({
                    "name": candidate.name,
                    "registry_type": candidate.registry_type,
                    "identifier": candidate.identifier,
                    "version": candidate.version,
                    "execution_provenance": "containerized_docker",
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
        "census-stage2-class-a: {attempted} attempted, {succeeded} succeeded ({:.1}%), {failed} failed",
        pct(succeeded, attempted)
    );
    for (category, count) in &failure_categories {
        eprintln!("census-stage2-class-a:   {category}: {count}");
    }

    let tally = coverage::tally(&all_tools);
    let generated_at_unix = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();

    let output = serde_json::json!({
        "generated_at_unix": generated_at_unix,
        "stage": "2 — containerized execution (docker run) against live Class A servers",
        "execution_provenance": "containerized_docker",
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
    let out_path = out_dir.join("class_a_annotation_coverage.json");
    std::fs::write(&out_path, serde_json::to_string_pretty(&output)?)?;
    eprintln!("census-stage2-class-a: wrote {}", out_path.display());

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
