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
//!
//! P0-10 adds three things. Evidence: both raw responses of every discovery that returns are
//! persisted, and the results file is derived from the stored bytes
//! ([`crate::census_report::build_report`], shared with `census-rederive`). Bounded
//! concurrency: `--jobs N` (default 1, max [`crate::cli::MAX_JOBS`]) runs up to N containers at
//! once — these are local containers, not third-party hosts, so Stage 1's politeness rule does
//! not apply — with output order fixed by candidate index, never by completion order. Disk
//! hygiene: an OCI image this sweep pulled is removed after its attempt, and no attempt
//! launches while the host volume is under [`crate::sweep::MIN_FREE_BYTES`] free.

use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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

// Shared with the other sweeps rather than re-derived: sampling comparability across
// Stage 1 / Stage 2 / Track B depends on all of them selecting by the *same* stable hash.
use crate::census_stage1::stable_hash;

const STAGE: &str = "2 — containerized execution (docker run) against live Class A servers";
const DEFAULT_OUT: &str = "results/census/class_a_annotation_coverage.json";

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
    /// True only for an `oci` candidate whose image was proven absent before the sweep
    /// began — i.e. any copy of it on the host afterwards is one this sweep pulled, and is
    /// this sweep's to remove. See [`image_present_locally`].
    owns_oci_image: bool,
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
            owns_oci_image: false, // decided after selection, by mark_owned_oci_images
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
/// on `--rm` alone was not. `-v` also removes the anonymous volumes the container created —
/// `--rm` would have, but a SIGKILL'd CLI never got to honour it, and leaked volumes are
/// leaked disk on a host that has little to spare.
fn cleanup_container(name: &str) {
    // Errors are expected and fine: a container that exited/was removed cleanly via its own
    // `--rm` never existed to be force-removed. Only the removal-when-needed case matters.
    let _ = Command::new("docker").args(["rm", "-f", "-v", "--", name]).output();
}

/// Whether `image` is already in the local image store.
///
/// "Absent" is concluded only from Docker's own `No such image` answer. Any other outcome — a
/// daemon hiccup, a reference Docker can't parse, failing to run `docker` at all — counts as
/// *present*, because the only thing this answer decides is whether the image is deleted
/// after its attempt, and a sweep must never delete an image it cannot prove it pulled.
fn image_present_locally(image: &str) -> bool {
    match Command::new("docker").args(["image", "inspect", "--format", "{{.Id}}", "--", image]).output() {
        Ok(output) if output.status.success() => true,
        Ok(output) => !String::from_utf8_lossy(&output.stderr).contains("No such image"),
        Err(_) => true,
    }
}

/// Decide, once and before any attempt, which OCI candidates' images this sweep will own.
/// Runs after [`prepull_base_images`], so an OCI candidate that happens to name a wrapper
/// image counts as pre-existing and is never removed out from under the npm/pypi attempts.
fn mark_owned_oci_images(candidates: &mut [Candidate]) {
    for candidate in candidates.iter_mut().filter(|c| c.registry_type == "oci") {
        candidate.owns_oci_image = !image_present_locally(&candidate.identifier);
    }
}

/// Remove an image this sweep pulled. Best-effort: if a concurrent attempt is still running
/// a container from the same image, Docker refuses (`image is being used`), and whichever
/// attempt finishes last removes it instead.
fn remove_image(image: &str) {
    match Command::new("docker").args(["image", "rm", "--", image]).output() {
        Ok(output) if output.status.success() => {
            eprintln!("census-stage2-class-a:   removed image {image} (pulled by this sweep)");
        }
        Ok(_) | Err(_) => {}
    }
}

/// Launch one candidate in its container, discover it, tear everything down, and persist
/// what it returned.
fn observe(candidate: &Candidate, container_name: &str, store: &Mutex<BlobStore>, stop: &StopFlag) -> Observation {
    let Some(args) = docker_args(candidate, container_name) else {
        return Observation::failed("unsupported_registry_type", &candidate.registry_type);
    };
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();

    let mut client = match DiscoveryClient::stdio_with_timeout("docker", &arg_refs, PER_ATTEMPT_TIMEOUT) {
        Ok(c) => c,
        Err(e) => return Observation::failed("spawn", &e.to_string()),
    };
    let result = client.discover();
    // Unconditional, regardless of how discover() came out — see cleanup_container's doc.
    cleanup_container(container_name);
    drop(client); // reap the `docker run` CLI process before touching its image
    if candidate.owns_oci_image {
        remove_image(&candidate.identifier);
    }

    match result {
        Ok(discovery) => census_report::record_discovery(store, &discovery, stop),
        // A killed-at-timeout container's closed pipe surfaces as an I/O error too (the
        // watchdog kills the process; recv_line then sees EOF) — indistinguishable from an
        // ordinary early exit at this layer, so it is reported as one category rather than
        // guessed apart.
        Err(e) => Observation::from_discovery_error(e, "io_or_timeout"),
    }
}

fn identity(candidate: &Candidate) -> Map<String, Value> {
    let Value::Object(map) = json!({
        "name": candidate.name,
        "registry_type": candidate.registry_type,
        "identifier": candidate.identifier,
        "version": candidate.version,
        "execution_provenance": "containerized_docker",
    }) else {
        unreachable!("json! of an object literal is an object")
    };
    map
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
/// capped Docker container, up to `--jobs` at a time (default 1).
pub fn run(args: &SweepArgs) -> Result<(), Box<dyn std::error::Error>> {
    let sample_size = args.sample_size;
    if std::process::Command::new("docker").arg("info").output().map(|o| !o.status.success()).unwrap_or(true) {
        return Err("docker is not available or the daemon is not running (`docker info` failed) \
                     — Stage 2 needs a working container runtime; see docs/tasks.md P0-06 for the \
                     blocker this produces if it stays down"
            .into());
    }

    let evidence_dir =
        census_report::resolve_evidence_dir(args.evidence_dir.as_deref(), std::env::var_os(census_report::EVIDENCE_DIR_ENV));
    let out_path = args.out.clone().unwrap_or_else(|| PathBuf::from(DEFAULT_OUT));
    let store = Mutex::new(BlobStore::open(&evidence_dir)?);
    let disk = DiskGuard::new(&evidence_dir);
    let stop = StopFlag::default();

    // Already under the floor: no attempt will launch, so don't spend disk on wrapper images
    // either. The pool's first preflight records the reason and the run writes an empty,
    // `"complete": false` results file.
    match disk.check() {
        Ok(()) => prepull_base_images(),
        Err(reason) => eprintln!("census-stage2-class-a: skipping base-image pre-pull — {reason}"),
    }

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
    let mut candidates: Vec<Candidate> = all_class_a.into_iter().take(sample_size).collect();
    mark_owned_oci_images(&mut candidates);

    eprintln!(
        "census-stage2-class-a: attempting containerized discovery against {} Class A servers, {} at a time \
         (evidence -> {})...",
        candidates.len(),
        args.jobs,
        evidence_dir.display()
    );

    // Disambiguates container names across separate invocations of this xtask (e.g. the
    // pilot run and the full sweep both running index 0..N, or two sweeps started in the
    // same second) so a name collision can never make cleanup_container remove the wrong
    // run's container.
    let sweep_id = format!("{}-{}", SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(), std::process::id());

    let pool = sweep::run_bounded(
        &candidates,
        args.jobs,
        || stop.check().and_then(|()| disk.check()),
        |i, candidate| {
            eprintln!(
                "census-stage2-class-a: [{}/{}] {} ({}:{})",
                i + 1,
                candidates.len(),
                candidate.name,
                candidate.registry_type,
                candidate.identifier
            );
            // A name this module controls (not daemon-assigned) is what makes
            // cleanup_container able to target the right container after a watchdog kill —
            // see its doc comment. The candidate index makes it unique within this sweep
            // however many attempts run concurrently; sweep_id makes it unique across sweeps.
            let container_name = format!("mcp-conf-stage2-{sweep_id}-{i}");
            ServerObservation {
                identity: identity(candidate),
                observation: observe(candidate, &container_name, &store, &stop),
            }
        },
    );

    let mut header = RunHeader {
        stage: STAGE,
        sample_size_requested: sample_size,
        candidates_selected: candidates.len(),
        attempted: pool.completed.len(),
        stop_reason: pool.stop_reason,
        evidence_store: &evidence_dir,
    }
    .into_map()?;
    header.insert("execution_provenance".into(), "containerized_docker".into());
    header.insert("jobs".into(), args.jobs.into());
    let store = store.into_inner().map_err(|_| "evidence store lock poisoned")?;
    census_report::finish("census-stage2-class-a", header, &store, &pool.completed, &out_path)
}

