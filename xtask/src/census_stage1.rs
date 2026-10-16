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
//!
//! P0-12: sequential is necessary but was never sufficient. A host's own 2–5 requests went
//! out back to back with zero spacing, and a per-host view of politeness misses a vendor
//! sitting behind many registry entries — the committed July file holds 149 entries on one
//! host and 18 on 18 distinct hosts of one domain. So every attempt now runs under a
//! run-wide [`Pacer`]: minimum spacing between consecutive requests to one registrable
//! domain, and a hard per-domain request budget whose refusals are **recorded as such**
//! rather than skipped. Policy and accounting both land in the results header, so the
//! politeness claim is checkable from the published file.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use datamodel::ContainabilityClass;
use discovery::{DiscoveryClient, Pacer};
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

/// Per-request timeout. Shorter than the 30s default: at sample sizes in the hundreds or
/// thousands, a handful of genuinely unresponsive hosts at 30s each would dominate total run
/// time. Servers that are actually going to answer do so in low seconds at most, per the
/// 100-server sample this was tuned against.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(12);

/// Separates the admission-order hash from the selection hash under one seed, so the two
/// orderings are independent rather than the same permutation twice. Any fixed non-zero
/// constant does this; it is published implicitly by being in this source, which is fine —
/// the secret is the seed, not the salt.
const ORDER_SALT: u64 = 0x9E37_79B9_7F4A_7C15;

/// One candidate's whole attempt: budget admission, discovery, evidence persistence, and the
/// per-server pacing record. Exactly the body of the sweep's worker closure, extracted so it
/// can be driven against a local fake in this module's own tests rather than only in a live
/// sweep.
fn attempt(
    candidate: &Candidate,
    pacer: &Arc<Pacer>,
    store: &Mutex<BlobStore>,
    stop: &StopFlag,
) -> ServerObservation {
    let (observation, requests, waited, reservation) = observe(candidate, pacer, store, stop);
    let mut identity = identity(candidate);
    identity.insert(
        "pacing".into(),
        census_report::pacing_record(
            &discovery::registrable_domain_of_endpoint(&candidate.url),
            requests,
            waited,
            reservation,
        ),
    );
    ServerObservation { identity, observation }
}

/// Discover one Class B server and persist what it returned. Coverage is *not* computed
/// here — a response that discovery accepted but coverage can't read becomes a
/// `coverage_extraction` failure at derivation time, from the stored bytes, exactly as it
/// would on a re-derivation.
///
/// Returns the observation alongside the pacing accounting for this one attempt, which the
/// caller writes onto the record. A budget refusal returns zero requests, zero wait and
/// [`census_report::BUDGET_EXHAUSTED`] — honestly, because no request was made.
fn observe(
    candidate: &Candidate,
    pacer: &Arc<Pacer>,
    store: &Mutex<BlobStore>,
    stop: &StopFlag,
) -> (Observation, usize, std::time::Duration, usize) {
    // The budget is consulted *before* a client exists, so a refused host costs no
    // connection at all — and is recorded, never silently dropped from the sample.
    let permit = match pacer.admit(&candidate.url) {
        Ok(permit) => permit,
        Err(denied) => {
            eprintln!("census-stage1:   refused: {denied}");
            return (
                Observation::failed(census_report::BUDGET_EXHAUSTED, &denied.to_string()),
                0,
                std::time::Duration::ZERO,
                0,
            );
        }
    };
    // The permit moves into the client that will spend it; `stats` is the handle this
    // attempt's own counters are read back through afterwards.
    let stats = permit.stats();
    let mut client = DiscoveryClient::http_paced(candidate.url.clone(), REQUEST_TIMEOUT, permit);
    let observation = match client.discover() {
        Ok(discovery) => census_report::record_discovery(store, &discovery, stop),
        Err(e) => Observation::from_discovery_error(e, "io"),
    };
    // Dropped before the counters are read, so the reservation is returned to the budget
    // before the next candidate asks for one.
    drop(client);
    // The reservation is read *after* the client is dropped but from the `stats` handle,
    // which outlives it: a paginating walk extends the reservation as it goes (P0-13), so
    // the value that belongs on the record is the one the sequence finished holding, not the
    // five it was admitted with.
    (observation, stats.requests_sent(), stats.waited(), stats.reservation())
}

fn identity(candidate: &Candidate) -> Map<String, Value> {
    let Value::Object(map) = json!({
        "name": candidate.name,
        "url": candidate.url,
        "transport_type": candidate.transport_type,
        // The pacing and budget key, written per server so a reader can reproduce the
        // grouping in the header without re-deriving it from URLs themselves.
        "registrable_domain": discovery::registrable_domain_of_endpoint(&candidate.url),
    }) else {
        unreachable!("json! of an object literal is an object")
    };
    map
}

/// Hash `name` into a `u64` under `seed`, deterministically.
///
/// `DefaultHasher`'s own keys are fixed (unlike `HashMap`'s `RandomState`, which is
/// randomised per process to resist HashDoS), which is what makes a sweep reproducible from
/// its published seed. It is also what made the *unsalted* version a problem: with no seed,
/// membership of a sample and position within it were publicly precomputable offline, so a
/// name could be ground out that was *guaranteed* to be sampled (7 tries for a
/// 1,000-of-8,294 sample) and guaranteed to land in the first ten positions (225 tries),
/// the whole search finishing in under a millisecond. Combined with the shared-hosting
/// over-grouping ADR-014 §2 records, that is a starvation primitive: roughly 24 ground names
/// under one suffix fill that suffix's 24-sequence headroom and spend its whole budget
/// before any legitimate registrant on it is reached, for the price of ~24 registry entries.
///
/// The seed is mixed in first, so it perturbs every subsequent byte's contribution rather
/// than only the final state. It is chosen per run, published in the results header
/// (`sample_seed`), and settable with `--sample-seed` — so a sweep stays exactly
/// reproducible for anyone holding the file, and is not precomputable by anyone before it
/// runs.
pub(crate) fn stable_hash(name: &str, seed: u64) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    seed.hash(&mut hasher);
    name.hash(&mut hasher);
    hasher.finish()
}

/// A seed for [`stable_hash`]: the caller's, or a fresh unpredictable one.
///
/// Entropy comes from `RandomState`, which the standard library seeds per process from the
/// OS specifically so that hash keys are not predictable — exactly the property wanted
/// here — mixed with the wall clock so two sweeps started in one process would still
/// differ. No new dependency, and nothing here needs to be cryptographic: the bar is "not
/// computable by someone who does not yet have the results file", not "unguessable given
/// it".
pub(crate) fn resolve_sample_seed(requested: Option<u64>) -> u64 {
    use std::hash::{BuildHasher, Hasher};
    if let Some(seed) = requested {
        return seed;
    }
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u64(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64),
    );
    hasher.finish()
}

/// Run the Stage 1 census: sample `sample_size` Class B servers from the live registry and
/// attempt discovery against each.
///
/// The registry returns entries in what looks like roughly alphabetical order by name
/// (every sample drawn from "the first N" clustered under `a*` prefixes) — taking the first
/// N candidates would bias the sample toward whatever namespace happens to sort first, not
/// give a representative cross-section. Instead this scans every Class B candidate in the
/// registry, then selects `sample_size` of them by a **seeded** hash of their name
/// ([`stable_hash`]). Hash order is not alphabetical and is unbiased with respect to
/// namespace, and seeding keeps the two properties that pull in opposite directions: the
/// sample is exactly reproducible from the seed published in the results header, and it is
/// not precomputable by anyone before the run (see [`stable_hash`] for the starvation
/// primitive that bought).
///
/// Admission *order* within the sample is shuffled under the same seed, by a second,
/// differently-salted hash. Selection and order are separate because they leak separately:
/// knowing you are in a sweep is one thing, knowing you are early enough in it to consume a
/// shared suffix's budget before anyone else is the thing that does the damage.
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

    let sample_seed = resolve_sample_seed(args.sample_seed);
    eprintln!(
        "census-stage1: {} Class B candidates found; selecting {sample_size} by seeded hash \
         (seed {sample_seed:#018x}, published in the results header)...",
        all_class_b.len()
    );
    all_class_b.sort_by_key(|c| stable_hash(&c.name, sample_seed));
    let mut candidates: Vec<Candidate> = all_class_b.into_iter().take(sample_size).collect();
    // Shuffle admission order under the same seed, salted differently from selection, so
    // that landing in a sample does not also mean landing early in it.
    candidates.sort_by_key(|c| stable_hash(&c.name, sample_seed ^ ORDER_SALT));

    let evidence_dir =
        census_report::resolve_evidence_dir(args.evidence_dir.as_deref(), std::env::var_os(census_report::EVIDENCE_DIR_ENV));
    let out_path = args.out.clone().unwrap_or_else(|| PathBuf::from(DEFAULT_OUT));
    let store = Mutex::new(BlobStore::open(&evidence_dir)?);
    let disk = DiskGuard::new(&evidence_dir);
    let stop = StopFlag::default();

    let pacer = Arc::new(Pacer::new(args.pace));
    eprintln!(
        "census-stage1: attempting discovery against {} Class B servers (evidence -> {}); \
         pacing {} ms between requests to one registrable domain, cap {} requests/domain",
        candidates.len(),
        evidence_dir.display(),
        args.pace.min_interval.as_millis(),
        args.pace.max_requests_per_registrable_domain,
    );

    // jobs = 1, not configurable: unrelated third-party hosts, never burst (P0-07). The
    // pacer is correct under concurrency regardless, which is deliberate — it must not be
    // the reason this stays sequential.
    let pool = sweep::run_bounded(
        &candidates,
        1,
        || stop.check().and_then(|()| disk.check()),
        |i, candidate| {
            eprintln!("census-stage1: [{}/{}] {} ({})", i + 1, candidates.len(), candidate.name, candidate.url);
            // Per attempt, so one panicking entry costs one record rather than the whole
            // file. The identity fields are reattached below, because `guard_attempt`
            // cannot know them.
            let mut observed = census_report::guard_attempt("census-stage1", || {
                attempt(candidate, &pacer, &store, &stop)
            });
            if observed.identity.is_empty() {
                observed.identity = identity(candidate);
            }
            observed
        },
    );

    let mut header = RunHeader {
        stage: STAGE,
        sample_size_requested: sample_size,
        candidates_selected: candidates.len(),
        attempted: pool.completed.len(),
        stop_reason: pool.stop_reason,
        evidence_store: &evidence_dir,
        sample_seed: Some(sample_seed),
    }
    .into_map()?;
    // Policy *and* accounting, so a reader of the published file can check the politeness
    // claim rather than take it on trust (P0-12).
    header.insert("pacing".into(), census_report::pacing_header(&pacer.accounting()));
    // `into_inner` on poison rather than an error: a panicking attempt poisons this lock,
    // and failing here would discard the results file for every host already contacted —
    // re-creating, one layer up, exactly the total loss `guard_attempt` was added to stop.
    let store = store.into_inner().unwrap_or_else(std::sync::PoisonError::into_inner);
    census_report::finish("census-stage1", header, &store, &pool.completed, &out_path)
}

pub(crate) fn pct(count: usize, total: usize) -> f64 {
    if total == 0 { 0.0 } else { 100.0 * count as f64 / total as f64 }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two properties the seed has to hold simultaneously: **reproducible** from a
    /// published seed, and **different** under a different one. Either alone is useless —
    /// an unreproducible sample cannot be audited, and an unseeded one is precomputable
    /// before the run and therefore targetable.
    #[test]
    fn a_published_seed_reproduces_its_sample_and_a_different_seed_does_not() {
        let names: Vec<String> = (0..500).map(|i| format!("io.github.user{i}/server")).collect();
        let order = |seed: u64| {
            let mut v: Vec<&String> = names.iter().collect();
            v.sort_by_key(|n| stable_hash(n, seed));
            v.into_iter().take(50).cloned().collect::<Vec<String>>()
        };

        assert_eq!(order(0x1234), order(0x1234), "the same seed must reproduce the sample");
        assert_ne!(
            order(0x1234),
            order(0x1235),
            "a different seed must select a different sample"
        );
        // And the admission-order salt gives a genuinely different permutation of the same
        // sample, so being selected does not also mean being admitted early.
        let selected = order(0x1234);
        let mut shuffled = selected.clone();
        shuffled.sort_by_key(|n| stable_hash(n, 0x1234 ^ ORDER_SALT));
        assert_eq!(
            {
                let mut a = selected.clone();
                a.sort();
                a
            },
            {
                let mut b = shuffled.clone();
                b.sort();
                b
            },
            "shuffling order must not change membership"
        );
        assert_ne!(selected, shuffled, "admission order must not equal selection order");
    }

    /// An explicit seed is honoured exactly; an absent one is drawn fresh. The freshness
    /// claim is the security-relevant half: two runs that both drew their own seed must not
    /// agree, or the sample is precomputable again.
    #[test]
    fn resolve_sample_seed_honours_a_request_and_otherwise_draws_a_fresh_one() {
        assert_eq!(resolve_sample_seed(Some(42)), 42);
        assert_eq!(resolve_sample_seed(Some(0)), 0, "zero is a seed like any other");
        let drawn: std::collections::BTreeSet<u64> =
            (0..8).map(|_| resolve_sample_seed(None)).collect();
        assert!(
            drawn.len() >= 7,
            "eight unseeded draws collided {} times; the sample would be predictable",
            8 - drawn.len()
        );
    }
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::time::{Duration, Instant};

    use discovery::PacePolicy;

    /// A one-shot HTTP/1.1 responder, the same technique `crates/discovery`'s own HTTP tests
    /// use: read one `Content-Length`-framed request, write one scripted reply. Enough to
    /// drive the real `DiscoveryClient` with no third-party contact.
    fn serve(listener: TcpListener, replies: Vec<String>) -> std::thread::JoinHandle<usize> {
        std::thread::spawn(move || {
            for body in &replies {
                let Ok((mut stream, _)) = listener.accept() else { break };
                read_request(&mut stream);
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
                     Connection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(body.as_bytes());
                let _ = stream.flush();
            }
            replies.len()
        })
    }

    fn read_request(stream: &mut TcpStream) {
        let mut buf = Vec::new();
        let mut byte = [0u8; 1];
        while stream.read_exact(&mut byte).is_ok() {
            buf.push(byte[0]);
            if buf.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        let headers = String::from_utf8_lossy(&buf).into_owned();
        let len: usize = headers
            .lines()
            .find_map(|l| {
                l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| {
                    v.trim().parse().expect("numeric Content-Length")
                })
            })
            .unwrap_or(0);
        let mut body = vec![0u8; len];
        let _ = stream.read_exact(&mut body);
    }

    fn candidate(url: &str, name: &str) -> Candidate {
        Candidate {
            name: name.to_string(),
            url: url.to_string(),
            transport_type: "streamable-http".to_string(),
        }
    }

    const DISCOVER_RESULT: &str = r#"{"jsonrpc":"2.0","id":0,"result":{"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{},"ttlMs":0,"cacheScope":"private"}}"#;
    const TOOLS_LIST: &str = r#"{"jsonrpc":"2.0","id":1,"result":{"resultType":"complete","ttlMs":0,"cacheScope":"private","tools":[{"name":"read_file","inputSchema":{"type":"object"},"annotations":{"readOnlyHint":true}}]}}"#;

    /// **The Stage 1 wiring, end to end, against a local fake.** One admitted attempt
    /// discovers and records its own pacing; the next is refused because the cap is spent,
    /// and lands in the results file as a refusal with a reason rather than vanishing from
    /// the sample.
    #[test]
    fn an_admitted_attempt_records_its_pacing_and_a_refused_one_records_the_refusal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let evidence_dir = dir.path().join("evidence");
        let store = Mutex::new(store::BlobStore::open(&evidence_dir).expect("store"));
        let stop = StopFlag::default();

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let endpoint = format!("http://{}/mcp", listener.local_addr().expect("addr"));
        let server = serve(listener, vec![DISCOVER_RESULT.to_string(), TOOLS_LIST.to_string()]);

        // One reservation's worth of budget, and a measurable interval.
        let pacer = Arc::new(Pacer::new(PacePolicy {
            min_interval: Duration::from_millis(80),
            max_requests_per_registrable_domain: discovery::MAX_REQUESTS_PER_DISCOVERY,
        }));

        let start = Instant::now();
        let first = attempt(&candidate(&endpoint, "admitted"), &pacer, &store, &stop);
        let elapsed = start.elapsed();
        assert_eq!(server.join().expect("server thread"), 2, "two scripted replies were served");

        assert!(matches!(first.observation, Observation::Discovered { .. }));
        assert_eq!(first.identity["pacing"]["requests_sent"], 2);
        assert_eq!(first.identity["pacing"]["registrable_domain"], "127.0.0.1");
        assert_eq!(first.identity["registrable_domain"], "127.0.0.1");
        assert!(elapsed >= Duration::from_millis(80), "one gap must be paid, took {elapsed:?}");

        // The second attempt on the same registrable domain: cap spent, so refused.
        let second = attempt(&candidate(&endpoint, "refused"), &pacer, &store, &stop);
        match &second.observation {
            Observation::Failed { category, detail } => {
                assert_eq!(category, census_report::BUDGET_EXHAUSTED);
                assert!(detail.contains("no request was made to this host"), "{detail}");
            }
            Observation::Discovered { .. } => panic!("the second attempt must be refused"),
        }
        assert_eq!(second.identity["pacing"]["requests_sent"], 0);

        // The header's accounting matches the two records beneath it, which is the property
        // a reader auditing the politeness claim actually checks.
        let header_block = census_report::pacing_header(&pacer.accounting());
        assert_eq!(header_block["requests_sent"], 2);
        assert_eq!(header_block["sequences_denied_by_budget"], 1);
        let report = serde_json::json!({
            "pacing": header_block,
            "servers": [Value::Object(first.identity.clone()), {
                "pacing": second.identity["pacing"].clone(),
                "failure_category": census_report::BUDGET_EXHAUSTED,
            }],
        });
        assert_eq!(census_report::verify_pacing_self_consistent(&report), Ok(()));
        assert_eq!(stop.check(), Ok(()), "pacing must never trip the sweep's stop flag");
    }

    /// A refused attempt opens no connection: the listener is scripted with nothing and
    /// asserted to have served nothing, so the refusal is free as well as recorded.
    #[test]
    fn a_refused_attempt_sends_no_request_at_all() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Mutex::new(store::BlobStore::open(dir.path().join("ev")).expect("store"));
        let stop = StopFlag::default();

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let endpoint = format!("http://{}/mcp", listener.local_addr().expect("addr"));
        let server = serve(listener, Vec::new());

        let pacer = Arc::new(Pacer::new(PacePolicy {
            min_interval: Duration::ZERO,
            max_requests_per_registrable_domain: discovery::MAX_REQUESTS_PER_DISCOVERY,
        }));
        let held = pacer.admit(&endpoint).expect("the first reservation fits exactly");
        let refused = attempt(&candidate(&endpoint, "refused"), &pacer, &store, &stop);
        drop(held);

        assert_eq!(server.join().expect("server thread"), 0, "nothing was served");
        assert!(matches!(refused.observation, Observation::Failed { .. }));
        assert_eq!(pacer.accounting().requests_sent, 0);
        assert_eq!(pacer.accounting().sequences_denied, 1);
    }

    /// The pacing key written per server is the one the budget and the delay actually use —
    /// not a second derivation that could drift from it.
    #[test]
    fn the_per_server_key_is_the_same_one_the_budget_groups_by() {
        let c = candidate("https://barcode-generator.api.klymax402.com/mcp", "x");
        let ident = identity(&c);
        assert_eq!(ident["registrable_domain"], "klymax402.com");
        let pacer = Arc::new(Pacer::new(PacePolicy::CENSUS_DEFAULT));
        let permit = pacer.admit(&c.url).expect("budget");
        assert_eq!(permit.registrable_domain(), ident["registrable_domain"].as_str().expect("str"));
    }
}
