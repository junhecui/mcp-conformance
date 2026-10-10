//! Dual-era discovery — `server/discover` or `initialize`, then `tools/list` — with
//! byte-exact response capture (P0-01) and structural prevention of any tool call.
//!
//! **Must not:** call any tool. Structural, not disciplinary: [`crate::transport::Transport`]
//! is private to this crate, so nothing outside it can reach a method taking an arbitrary
//! MCP method string. [`DiscoveryClient::discover`] is the only public entry point, and the
//! four method names it can send — `server/discover`, `initialize`,
//! `notifications/initialized` and `tools/list` — are literals in this module's body, not
//! parameters. There is no public function anywhere in this crate that accepts a method
//! name.
//!
//! # Era negotiation (P0-11)
//!
//! MCP revision `2026-07-28` shipped final on its own date and **removed** the
//! `initialize`/`notifications/initialized` handshake. Protocol version and client
//! capabilities now ride `_meta` on every request, and `server/discover` replaces the
//! upfront capability exchange. The spec prescribes that a dual-era client probe
//! **modern-first**, and that its fallback not be keyed on any single error code; see
//! [`crate::era`] for the classifier and `docs/prior-art-resurvey-2026-10.md` §1.3–§1.4 for
//! the clause-by-clause reading. The sequence:
//!
//! 1. `server/discover`, with the full `_meta` block and (over HTTP) both
//!    `MCP-Protocol-Version` and `Mcp-Method`.
//! 2. A `DiscoverResult` means modern. Its `supportedVersions` is intersected with
//!    [`crate::era::CLIENT_SUPPORTED_REVISIONS`] — a closed client-side allowlist — and the
//!    chosen revision is what every subsequent request declares. A list offering nothing
//!    this client implements is a **discovery failure, not a downgrade**.
//! 3. A recognised modern error means modern too, and must not provoke a downgrade:
//!    `-32022` re-chooses from `data.supported`; `-32020`/`-32021` are real failures (a
//!    harness bug or an unmet server requirement) and are surfaced, not papered over.
//! 4. Anything else — a non-modern error code, a body that is not a well-formed JSON-RPC
//!    message, an unreadable or empty body, an SSE upgrade, or silence over stdio — falls
//!    back to `initialize`, with the reason recorded.
//! 5. A connection-level failure (DNS, refused, TLS, timeout) or a non-era-signalling HTTP
//!    status (401, 429, 5xx, …) is **never** a fallback trigger: the request either never
//!    reached a server or was refused at a level that says nothing about which lifecycle it
//!    speaks, so a second round trip to the same host could only double the cost of the
//!    failure population that dominates census sweeps. This preserves P0-09's review fix.
//! 6. One modern path is reached *without* the server confirming it is modern — an HTTP 404
//!    carrying `-32601`, which is also a documented JSON-RPC-over-HTTP convention a legacy
//!    server may use. That guess alone is correctable: if its `tools/list` fails, the legacy
//!    handshake is retried and the reason recorded. Every confirmed modern path propagates
//!    its failure untouched, because downgrading a server that said it is modern is what the
//!    spec forbids.
//!
//! Nothing a server returns is treated as a claim about itself without evidence to replay
//! it against: [`Discovery::probe_raw`] keeps the bytes the era decision was made on, and
//! [`EraProvenance::revision_source`] distinguishes a revision the server *negotiated* from
//! one this client merely *assumed*.
//!
//! What P0-09 got wrong, and why this is a rewrite rather than a patch, is recorded in
//! `docs/tasks.md` under P0-09 and P0-11.

use std::ffi::{OsStr, OsString};
use std::time::Duration;

use serde_json::{Value, json};

use crate::era::{
    self, CLIENT_SUPPORTED_REVISIONS, DiscoverClass, ERA_POLICY, Era, FallbackReason,
    LEGACY_PREFERRED, MODERN_PREFERRED,
};
use crate::transport::{ChildProcessTransport, HttpTransport, Transport};

const CLIENT_NAME: &str = "mcp-conformance-harness";
const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Everything discovery captured about one server.
///
/// `handshake_raw` and `tools_list_raw` are the only fields P0-02's metadata pin may ever
/// hash — "the pin is over bytes, not semantics" (architecture.md §3.1). Nothing in this
/// crate parses them any further than routing the response envelope; that's the pinner's
/// and census's job. Pinning is unaffected by era: the pin hashes only `tools_list_raw`,
/// and only the per-tool `(name, inputSchema, annotations, description)` fields within it,
/// so `2026-07-28`'s new required result fields (`resultType`, `ttlMs`, `cacheScope`) sit
/// outside the preimage and a tool's pin is identical whichever lifecycle produced it.
#[derive(Debug, Clone)]
pub struct Discovery {
    /// The exact bytes of the handshake response, before any parsing — the
    /// `server/discover` response on the modern path, or the `initialize` response on the
    /// legacy one. Consult [`discovery_path`](Self::discovery_path) to know which, and
    /// [`era_provenance`](Self::era_provenance) to know why that path was taken.
    ///
    /// Renamed from `initialize_raw` by P0-11: under modern-first these are usually *not*
    /// `initialize` bytes, and a field name that says otherwise is a trap for anyone
    /// reading evidence back out of the store.
    pub handshake_raw: Vec<u8>,
    /// The exact bytes of the `tools/list` response, before any parsing.
    pub tools_list_raw: Vec<u8>,
    /// The exact bytes of the `server/discover` probe response — the preimage of the era
    /// classification — or an explicit statement that no bytes ever existed.
    ///
    /// Modern-first makes the era a *derived* field, and before this existed it was the one
    /// derived field in the system with no replayable preimage: on every fallback the probe
    /// bytes were dropped on the floor, leaving `fallback_reason` an unauditable client
    /// assertion and cutting against architecture.md §6 invariant 2. The attacker gain is
    /// sharper than hiding modern capability: a dual-era server's modern and legacy handlers
    /// may expose **different tool sets with different annotations**, so answering the probe
    /// with garbage *steers the harness onto the handler the server picked*. P0-02's pin
    /// keeps the verdict honest about the snapshot that was tested, but without these bytes
    /// nothing in the evidence store shows that a selection happened at all.
    ///
    /// On a modern path these are the same bytes as [`handshake_raw`](Self::handshake_raw);
    /// the evidence store is content-addressed, so storing both costs one blob.
    pub probe_raw: ProbeEvidence,
    /// The revision actually negotiated for this discovery, or `None` when none was —
    /// chosen by the client from the server's `supportedVersions` on the modern path, or
    /// echoed by the server from its `initialize` result on the legacy one. Fills
    /// `TOOL_SNAPSHOT.spec_revision` (architecture.md §6).
    ///
    /// `None` on [`DiscoveryPath::ModernWithoutDiscover`], where the server named no
    /// revision and the client *assumed* one. Read
    /// [`era_provenance.revision_source`](EraProvenance::revision_source) to tell a
    /// negotiated revision from an assumed one, and
    /// [`era_provenance.chosen_revision`](EraProvenance::chosen_revision) for the value the
    /// client actually used on the wire.
    ///
    /// Bounded and shape-gated by [`crate::era::bounded_revision`] before it gets here, so
    /// no consumer has to remember to do it. `None` therefore also covers a server that
    /// echoed a revision too long or too malformed to store; the bytes it sent remain in
    /// `handshake_raw` either way.
    pub negotiated_spec_revision: Option<String>,
    /// Which handshake produced this result. See [`DiscoveryPath`].
    pub discovery_path: DiscoveryPath,
    /// How the era was decided — what the server offered, what this client chose, and why
    /// any fallback happened. See [`EraProvenance`].
    pub era_provenance: EraProvenance,
}

/// The `server/discover` probe response as evidence, including the two cases where there is
/// honestly nothing to keep.
///
/// The distinction matters and is the whole point of the type: "no evidence exists" and
/// "evidence existed and was not kept" are different claims, and an absent field cannot tell
/// them apart. Nothing in this crate may produce the second.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeEvidence {
    /// The probe response, exactly as received.
    Captured(Vec<u8>),
    /// No bytes were ever read, and why.
    Absent(ProbeAbsence),
}

impl ProbeEvidence {
    /// The bytes, if any were captured.
    #[must_use]
    pub fn bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Captured(bytes) => Some(bytes),
            Self::Absent(_) => None,
        }
    }

    /// Stable, lowercase-snake-case state for a provenance field: `captured`, or the
    /// absence reason.
    #[must_use]
    pub fn state(&self) -> &'static str {
        match self {
            Self::Captured(_) => "captured",
            Self::Absent(absence) => absence.as_str(),
        }
    }
}

/// Why a `server/discover` probe produced no bytes at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeAbsence {
    /// No response: a closed stdio pipe, or a watchdog kill. Nothing was ever received.
    NoResponse,
    /// A response arrived but its body was never read — the transport rejected it first
    /// (an SSE upgrade, which this transport does not speak) or could not read it within
    /// its size cap. The bytes existed on the wire and this client does not have them.
    BodyNotRead,
}

impl ProbeAbsence {
    /// Stable, lowercase-snake-case string form, for provenance fields in published results.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoResponse => "no_response",
            Self::BodyNotRead => "body_not_read",
        }
    }

    /// The inverse of [`Self::as_str`], for reading provenance back out of a results file.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "no_response" => Some(Self::NoResponse),
            "body_not_read" => Some(Self::BodyNotRead),
            _ => None,
        }
    }
}

/// Whether a recorded revision was *negotiated* with the server or *assumed* by the client.
///
/// Exists because one discovery path has no negotiation in it at all.
/// [`DiscoveryPath::ModernWithoutDiscover`] is reached from an HTTP 404 plus `-32601`: the
/// server named no revision, and the client proceeds at its own preferred modern one. Filling
/// `negotiated_spec_revision` from a client-side constant on that path published a claim about
/// the server that the server never made — and one that could not be contradicted offline,
/// since [`crate::negotiated_spec_revision`] errors on that same stored evidence.
///
/// So the two are different fields with different names, and this says which is which. A
/// populated `chosen_revision` alongside an empty `offered_revisions` is coherent exactly
/// when this is [`Self::Assumed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevisionSource {
    /// The server named the revision: a legacy `initialize` echoed it, or a modern
    /// `DiscoverResult` offered a list this client intersected with its own allowlist.
    Negotiated,
    /// No revision was negotiated. The client assumed its own preferred modern revision and
    /// proceeded; `negotiated_spec_revision` is `None`.
    Assumed,
}

impl RevisionSource {
    /// Stable, lowercase-snake-case string form, for provenance fields in published results.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Negotiated => "negotiated",
            Self::Assumed => "assumed",
        }
    }

    /// The inverse of [`Self::as_str`], for reading provenance back out of a results file.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "negotiated" => Some(Self::Negotiated),
            "assumed" => Some(Self::Assumed),
            _ => None,
        }
    }
}

impl std::fmt::Display for RevisionSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which handshake [`DiscoveryClient::discover`] used to reach a [`Discovery`] result.
///
/// Mirrors the Stage 2 census's `execution_provenance` pattern (bare-host vs.
/// containerized, `docs/tasks.md` P0-06): record *how* a result was produced on the result
/// itself, so census/audit data is never silently pooled across a discovery-mechanics
/// change the way ADR-002 already forbids pooling across oracles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscoveryPath {
    /// The `initialize` + `notifications/initialized` lifecycle (spec `2025-11-25` and
    /// earlier). Under the modern-first policy this is always a *fallback*, reached because
    /// `server/discover` produced something non-modern — see
    /// [`EraProvenance::fallback_reason`] for which branch.
    Initialize,
    /// A `DiscoverResult` from `server/discover`: the `2026-07-28`+ handshake-free
    /// lifecycle, with the protocol version and client capabilities on every request.
    ServerDiscover,
    /// The modern lifecycle without a `DiscoverResult`: the server answered
    /// `server/discover` with HTTP 404 plus `-32601`, which is `2026-07-28`'s unknown-method
    /// framing, so this client stays modern and sends `tools/list` with the modern `_meta` at
    /// its own preferred modern revision.
    ///
    /// **The server is non-conformant on this path, and the revision is assumed rather than
    /// negotiated.** `server/discover` is a method clients MAY call and servers **MUST**
    /// implement, so "modern server that did not implement the discovery method" is not a
    /// thing the spec allows — see [`crate::era::DiscoverClass::ModernWithoutDiscover`] for
    /// the argument that nonetheless justifies proceeding optimistically, and for why the
    /// failure mode of guessing wrong is clean rather than silent.
    ///
    /// Two consequences when reading census data:
    ///
    /// - `negotiated_spec_revision` is `None` and
    ///   [`EraProvenance::revision_source`] is [`RevisionSource::Assumed`]. The revision this
    ///   client actually put on the wire is in [`EraProvenance::chosen_revision`].
    /// - `handshake_raw` is that 404 body — honest about what the server actually said — so
    ///   [`negotiated_spec_revision`] correctly declines to re-derive a revision from it, and
    ///   the live record and the re-derived one now *agree* on that.
    ///
    /// If the `tools/list` that follows fails, the optimistic guess is corrected: discovery
    /// retries the legacy handshake and the record becomes [`Self::Initialize`] with
    /// [`FallbackReason::ModernWithoutDiscoverToolsListFailed`].
    ModernWithoutDiscover,
}

impl DiscoveryPath {
    /// Stable, lowercase-snake-case string form, for provenance fields in published results
    /// (JSON, DB columns) rather than `Debug` formatting.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Initialize => "initialize",
            Self::ServerDiscover => "server_discover",
            Self::ModernWithoutDiscover => "modern_without_discover",
        }
    }

    /// The inverse of [`Self::as_str`], for reading provenance back out of a published
    /// result (P0-10's offline re-derivation). `None` for any other string.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "initialize" => Some(Self::Initialize),
            "server_discover" => Some(Self::ServerDiscover),
            "modern_without_discover" => Some(Self::ModernWithoutDiscover),
            _ => None,
        }
    }

    /// Every variant, so a report can seed a distribution with explicit zeroes instead of
    /// omitting the paths a run happened not to take.
    #[must_use]
    pub fn all() -> &'static [Self] {
        &[Self::Initialize, Self::ServerDiscover, Self::ModernWithoutDiscover]
    }
}

impl std::fmt::Display for DiscoveryPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How the era was decided for one discovery: what the server offered, what this client
/// chose, and why any fallback was taken.
///
/// This exists for a security reason rather than for bookkeeping. P0-10 publishes
/// `discovery_path` distributions, and the fallback branches let a *server* influence which
/// era the harness records about it — by stalling, or by answering `server/discover` with
/// garbage. No security control is relaxed by that downgrade (the four annotations are
/// byte-identical across revisions, and P0-02's pin is revision-independent), but a server
/// must not be able to skew published provenance about itself without that skew being
/// visible. Recording the offer, the choice and the reason makes the influence auditable:
/// a corpus-wide spike in one [`FallbackReason`] is then a finding, not an invisible shift
/// in a `discovery_path` histogram.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EraProvenance {
    /// The era-selection policy that produced this record — always
    /// [`crate::era::ERA_POLICY`] today. Recorded because initialize-first (P0-09) and
    /// modern-first (P0-11) classify a dual-era server differently, so two runs under
    /// different policies are not comparable and must not be pooled.
    pub policy: &'static str,
    /// The revisions the server offered, type- and length-bounded, as filtered by
    /// [`crate::era`]. Empty when the server never got to offer any (a legacy fallback, or
    /// the [`DiscoveryPath::ModernWithoutDiscover`] path).
    pub offered_revisions: Vec<String>,
    /// The revision this client chose to use: its pick from `offered_revisions` on the
    /// modern path, or the revision it requested in `initialize` on the legacy one. Always
    /// a member of [`crate::era::CLIENT_SUPPORTED_REVISIONS`] — never a server-supplied
    /// string.
    pub chosen_revision: &'static str,
    /// Whether [`chosen_revision`](Self::chosen_revision) was negotiated with the server or
    /// assumed by this client. See [`RevisionSource`] — this is what makes a populated
    /// `chosen_revision` alongside an empty `offered_revisions` a coherent record rather
    /// than a contradictory one.
    pub revision_source: RevisionSource,
    /// Why the legacy path was taken, or `None` when the modern path succeeded.
    pub fallback_reason: Option<FallbackReason>,
}

/// Why discovery failed.
#[derive(Debug)]
pub enum DiscoveryError {
    /// A transport-level failure: connection refused, DNS, TLS, timeout. The request never
    /// reached a server, so nothing about the server follows from it.
    Transport(String),
    /// A non-2xx HTTP status that decides nothing about the server's era.
    ///
    /// Separate from [`Self::Transport`] since P0-11: `http_status_as_error(false)` is
    /// mandatory for reading a 4xx body (see `transport.rs`), so the status no longer
    /// arrives as a `ureq` error and has to be carried deliberately. Callers key their
    /// failure taxonomy on `status` — in particular `429`, which July's census could not
    /// tell apart from a dead host.
    HttpStatus {
        /// The HTTP status code.
        status: u16,
        /// A sanitised `Retry-After` value, when the server sent a usable one. A sweep
        /// should skip the host rather than retry within the run.
        retry_after: Option<String>,
    },
    /// An underlying I/O error (process spawn, stdio pipe read/write).
    Io(std::io::Error),
    /// The response was not well-formed JSON-RPC, or its shape was unexpected. Includes a
    /// mismatched response id — treated as a protocol violation, not tolerated, since
    /// discovery runs against a server the trust model assumes may be actively hostile —
    /// and a version negotiation that could not be completed.
    Protocol(String),
    /// The server returned a JSON-RPC error object.
    ServerError {
        /// The JSON-RPC error code.
        code: i64,
        /// The JSON-RPC error message.
        message: String,
    },
}

impl std::fmt::Display for DiscoveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(msg) => write!(f, "transport error: {msg}"),
            // Deliberately byte-identical to what `ureq::Error::StatusCode` used to render
            // through `Transport(e.to_string())`, so a census failure detail recorded
            // before and after P0-11 reads the same for the same server behaviour.
            Self::HttpStatus { status, .. } => write!(f, "http status: {status}"),
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::Protocol(msg) => write!(f, "protocol error: {msg}"),
            Self::ServerError { code, message } => {
                write!(f, "server returned error {code}: {message}")
            }
        }
    }
}

impl std::error::Error for DiscoveryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Transport(_)
            | Self::HttpStatus { .. }
            | Self::Protocol(_)
            | Self::ServerError { .. } => None,
        }
    }
}

/// What the modern probe concluded: proceed modern, or fall back to legacy.
enum EraDecision {
    /// The server is modern. `handshake_raw` is the response that proved it.
    Modern {
        path: DiscoveryPath,
        chosen: &'static str,
        offered: Vec<String>,
        revision_source: RevisionSource,
        handshake_raw: Vec<u8>,
    },
    /// The server is legacy (or offered only legacy revisions). `requested` is the revision
    /// `initialize` will ask for.
    Legacy { requested: &'static str, offered: Vec<String>, reason: FallbackReason },
}

/// How to re-create a stdio transport, kept so the legacy fallback can re-spawn a child the
/// modern probe destroyed. See [`DiscoveryClient::discover_legacy`] for the one case that
/// uses it and why it is gated so narrowly.
struct StdioRespawn {
    program: OsString,
    args: Vec<String>,
    timeout: Option<Duration>,
}

/// Speaks one of the two MCP lifecycles plus `tools/list`, over one transport.
///
/// Construct via [`DiscoveryClient::stdio`] or [`DiscoveryClient::http`]; the only other
/// public method is [`discover`](Self::discover). There is deliberately no way to send an
/// arbitrary MCP request through this type.
pub struct DiscoveryClient {
    transport: Box<dyn Transport>,
    respawn: Option<StdioRespawn>,
}

impl DiscoveryClient {
    /// Spawn `program` and speak MCP over its stdin/stdout.
    pub fn stdio(program: impl AsRef<OsStr>, args: &[&str]) -> Result<Self, DiscoveryError> {
        Self::stdio_inner(program, args, None)
    }

    /// Same as [`Self::stdio`], with a hard wall-clock deadline on the child process's
    /// lifetime — see [`crate::transport::ChildProcessTransport::spawn_with_timeout`]. For a
    /// stdio target this harness does not control the code of (e.g. a containerized Class A
    /// package under Stage 2 census), unbounded blocking on `recv_line` is not acceptable at
    /// sweep scale.
    pub fn stdio_with_timeout(
        program: impl AsRef<OsStr>,
        args: &[&str],
        timeout: Duration,
    ) -> Result<Self, DiscoveryError> {
        Self::stdio_inner(program, args, Some(timeout))
    }

    fn stdio_inner(
        program: impl AsRef<OsStr>,
        args: &[&str],
        timeout: Option<Duration>,
    ) -> Result<Self, DiscoveryError> {
        let transport = ChildProcessTransport::spawn_with_timeout(&program, args, timeout)?;
        Ok(Self {
            transport: Box::new(transport),
            respawn: Some(StdioRespawn {
                program: program.as_ref().to_os_string(),
                args: args.iter().map(|a| (*a).to_string()).collect(),
                timeout,
            }),
        })
    }

    /// Speak MCP Streamable HTTP against `endpoint` (the single-JSON-response case; see
    /// [`crate::transport::HttpTransport`] for the SSE-streaming exclusion). 30s timeout.
    #[must_use]
    pub fn http(endpoint: impl Into<String>) -> Self {
        Self { transport: Box::new(HttpTransport::new(endpoint.into())), respawn: None }
    }

    /// Same as [`Self::http`], with a caller-chosen timeout instead of the 30s default —
    /// for a large sequential sweep (a census over hundreds of servers) where a handful of
    /// unresponsive hosts at the default timeout would dominate total run time.
    #[must_use]
    pub fn http_with_timeout(endpoint: impl Into<String>, timeout: Duration) -> Self {
        Self {
            transport: Box::new(HttpTransport::with_timeout(endpoint.into(), timeout)),
            respawn: None,
        }
    }

    /// Run the full discovery sequence and capture its evidence. Never calls a tool.
    ///
    /// Modern-first, per the module doc comment: `server/discover`, then either
    /// `tools/list` with the modern `_meta` block, or a fallback to
    /// `initialize` + `notifications/initialized` + `tools/list`.
    ///
    /// **Request budget: at most five, and the fallback is always a single bounded step,
    /// never a loop.** Four on an ordinary legacy fallback (probe, then the three legacy
    /// requests); two on a modern success; five only on the one correctable path, where an
    /// optimistic `modern_without_discover` guess spends a `tools/list` before the legacy
    /// handshake corrects it (see [`FallbackReason::ModernWithoutDiscoverToolsListFailed`]).
    /// A connection-level failure and any non-era-signalling HTTP status still cost exactly
    /// one — the populations that dominate census failures are untouched.
    ///
    /// Every test that scripts HTTP responses asserts the exact request count, because this
    /// budget is part of the contract and P0-09's own regression was a doubled count that no
    /// assertion caught.
    pub fn discover(&mut self) -> Result<Discovery, DiscoveryError> {
        let (decision, probe_raw) = self.probe_modern()?;
        match decision {
            EraDecision::Modern { path, chosen, offered, revision_source, handshake_raw } => {
                // Re-affirmed rather than assumed: `probe_modern` may have chosen a
                // different revision than the one it probed with.
                self.transport.configure_http_headers(Some(chosen), true);
                let tools = match self.transport.call("tools/list", modern_params(chosen)) {
                    Ok(tools) => tools,
                    // The optimistic-modern guess, corrected. `ModernWithoutDiscover` is the
                    // only modern path reached without the server ever confirming it is
                    // modern — a 404 carrying `-32601` is also a documented
                    // JSON-RPC-over-HTTP convention, not a `2026-07-28` invention, so a
                    // legacy server can present it. Retrying the legacy handshake here is
                    // what stops such a server being locked onto the modern path and failing
                    // discovery where initialize-first succeeded. Every *confirmed* modern
                    // path (a `DiscoverResult`, a `-32022` re-negotiation) propagates the
                    // error untouched: downgrading a server that told us it is modern is
                    // what the spec forbids.
                    Err(_) if path == DiscoveryPath::ModernWithoutDiscover => {
                        return self.discover_legacy(
                            LEGACY_PREFERRED,
                            Vec::new(),
                            FallbackReason::ModernWithoutDiscoverToolsListFailed,
                            probe_raw,
                        );
                    }
                    Err(e) => return Err(e),
                };
                Ok(Discovery {
                    handshake_raw,
                    tools_list_raw: tools.bytes,
                    probe_raw,
                    // Only a *negotiated* revision is reported as one. On the assumed path
                    // this stays `None`, so a published record cannot claim the server named
                    // a revision it never named — see [`RevisionSource`].
                    negotiated_spec_revision: match revision_source {
                        RevisionSource::Negotiated => Some(chosen.to_string()),
                        RevisionSource::Assumed => None,
                    },
                    discovery_path: path,
                    era_provenance: EraProvenance {
                        policy: ERA_POLICY,
                        offered_revisions: offered,
                        chosen_revision: chosen,
                        revision_source,
                        fallback_reason: None,
                    },
                })
            }
            EraDecision::Legacy { requested, offered, reason } => {
                self.discover_legacy(requested, offered, reason, probe_raw)
            }
        }
    }

    /// Send the modern probe and decide the era from what came back, alongside the probe's
    /// own bytes as evidence.
    fn probe_modern(&mut self) -> Result<(EraDecision, ProbeEvidence), DiscoveryError> {
        self.transport.configure_http_headers(Some(MODERN_PREFERRED), true);
        let (class, probe_raw) =
            match self.transport.call_raw("server/discover", modern_params(MODERN_PREFERRED)) {
                Ok(outcome) => (
                    era::classify_discover_outcome(outcome.status, &outcome.bytes),
                    ProbeEvidence::Captured(outcome.bytes),
                ),
                // Never a fallback trigger. A connection-level failure means nothing
                // reached a server; a non-era-signalling status means the server refused
                // the request at a level that says nothing about its lifecycle. Either way
                // a second round trip to the same host cannot produce a different outcome,
                // and at sweep scale it would double the cost of the dominant failure
                // population (P0-09's review finding, preserved).
                Err(err @ (DiscoveryError::Transport(_) | DiscoveryError::HttpStatus { .. })) => {
                    return Err(err);
                }
                // stdio: closed pipe, or a watchdog kill. Indistinguishable here from a
                // server that does not answer unknown methods, so it is worth the fallback.
                Err(DiscoveryError::Io(_)) => (
                    DiscoverClass::NotModern { reason: FallbackReason::NoResponse },
                    ProbeEvidence::Absent(ProbeAbsence::NoResponse),
                ),
                // Not JSON-RPC at all, or an SSE upgrade — which is how a dual-era server
                // dispatching on the absence of modern framing answered P0-09's payload
                // (re-survey Appendix A.1). The transport rejected the response before
                // reading its body, so the bytes existed and this client does not have
                // them — recorded as exactly that, never as "no evidence".
                Err(DiscoveryError::Protocol(_)) => (
                    DiscoverClass::NotModern { reason: FallbackReason::MalformedResponse },
                    ProbeEvidence::Absent(ProbeAbsence::BodyNotRead),
                ),
                // `call_raw` never decodes a JSON-RPC envelope, so it cannot raise this.
                // Handled rather than `unreachable!()`d: the contract is a doc comment, not
                // a type, and a panic in a census sweep is strictly worse than a fallback.
                Err(DiscoveryError::ServerError { .. }) => (
                    DiscoverClass::NotModern { reason: FallbackReason::NonModernErrorBody },
                    ProbeEvidence::Absent(ProbeAbsence::BodyNotRead),
                ),
            };

        let decision = self.decide(class, probe_raw.bytes().unwrap_or_default().to_vec())?;
        Ok((decision, probe_raw))
    }

    /// Turn a classification into a decision, re-negotiating once if the server asked for it.
    fn decide(
        &mut self,
        class: DiscoverClass,
        handshake_raw: Vec<u8>,
    ) -> Result<EraDecision, DiscoveryError> {
        match class {
            DiscoverClass::Result { offered } => match era::choose_revision(&offered) {
                Some((chosen, Era::Modern)) => Ok(EraDecision::Modern {
                    path: DiscoveryPath::ServerDiscover,
                    chosen,
                    offered,
                    revision_source: RevisionSource::Negotiated,
                    handshake_raw,
                }),
                // The server implements `server/discover` but supports only revisions whose
                // lifecycle is the legacy one. Odd, and decidable: use the legacy handshake
                // at the revision it named.
                Some((requested, Era::Legacy)) => Ok(EraDecision::Legacy {
                    requested,
                    offered,
                    reason: FallbackReason::OnlyLegacyRevisionsOffered,
                }),
                None => Err(negotiation_failed("server/discover offered", &offered)),
            },
            DiscoverClass::UnsupportedVersion { offered } => {
                self.renegotiate_after_unsupported_version(offered)
            }
            // `-32020` HeaderMismatch / `-32021` MissingRequiredClientCapability. The server
            // is modern and says the *request* was wrong, which is a harness bug or an
            // unmet server-side requirement. Surfacing it is the point: a silent downgrade
            // here would hide exactly the defect P0-11 exists to fix.
            // The message is replaced by its digest, not merely bounded: this error arrives
            // from the very first request, pre-handshake, and its text ended up in a
            // committed `results/census/*.json`. See `era::DiscoverClass::ModernFatal`.
            DiscoverClass::ModernFatal { code, message_sha256 } => {
                Err(DiscoveryError::ServerError {
                    code,
                    message: format!(
                        "pre-handshake modern-era error; message withheld from published \
                         output, sha256:{message_sha256}"
                    ),
                })
            }
            DiscoverClass::ModernWithoutDiscover => Ok(EraDecision::Modern {
                path: DiscoveryPath::ModernWithoutDiscover,
                chosen: MODERN_PREFERRED,
                offered: Vec::new(),
                // The server named no revision. This one is the client's own, and the record
                // must not read as though the server negotiated it.
                revision_source: RevisionSource::Assumed,
                handshake_raw,
            }),
            DiscoverClass::NotModern { reason } => Ok(EraDecision::Legacy {
                requested: LEGACY_PREFERRED,
                offered: Vec::new(),
                reason,
            }),
        }
    }

    /// Handle `-32022 UnsupportedProtocolVersionError`: the server is modern but refused the
    /// revision asked for and named the ones it accepts.
    ///
    /// At most one further `server/discover` goes out, and only at a revision drawn from the
    /// client's own allowlist — so this cannot loop, and a server cannot drive it with a
    /// value the client does not implement.
    fn renegotiate_after_unsupported_version(
        &mut self,
        offered: Vec<String>,
    ) -> Result<EraDecision, DiscoveryError> {
        match era::choose_revision(&offered) {
            Some((chosen, Era::Modern)) => {
                if chosen == MODERN_PREFERRED {
                    // The server rejected the very revision it lists as supported. That is
                    // a contradiction in the server, not something to retry or downgrade
                    // around.
                    return Err(DiscoveryError::Protocol(format!(
                        "version negotiation failed: server rejected {MODERN_PREFERRED} with \
                         -32022 while listing it as supported"
                    )));
                }
                // Unreachable until the allowlist holds a second modern revision, and
                // written out rather than left as a hole for when it does.
                self.transport.configure_http_headers(Some(chosen), true);
                let outcome =
                    self.transport.call_raw("server/discover", modern_params(chosen))?;
                match era::classify_discover_outcome(outcome.status, &outcome.bytes) {
                    DiscoverClass::Result { offered } => Ok(EraDecision::Modern {
                        path: DiscoveryPath::ServerDiscover,
                        chosen,
                        offered,
                        revision_source: RevisionSource::Negotiated,
                        handshake_raw: outcome.bytes,
                    }),
                    other => Err(DiscoveryError::Protocol(format!(
                        "version negotiation failed: server asked for {chosen} and then did \
                         not return a DiscoverResult for it ({other:?})"
                    ))),
                }
            }
            Some((requested, Era::Legacy)) => Ok(EraDecision::Legacy {
                requested,
                offered,
                reason: FallbackReason::OnlyLegacyRevisionsAfterUnsupportedVersion,
            }),
            None => Err(negotiation_failed("server rejected the modern probe and offered", &offered)),
        }
    }

    /// The legacy lifecycle: `initialize`, the `notifications/initialized` notification the
    /// `2025-11-25`-and-earlier spec requires before any other request is valid, then
    /// `tools/list`.
    fn discover_legacy(
        &mut self,
        requested: &'static str,
        offered: Vec<String>,
        reason: FallbackReason,
        probe_raw: ProbeEvidence,
    ) -> Result<Discovery, DiscoveryError> {
        // The modern probe set `MCP-Protocol-Version: 2026-07-28` and `Mcp-Method`. Neither
        // belongs on a legacy request: `Mcp-Method` is undefined before `2026-07-28`, and
        // announcing a revision the server has just declined to speak invites an outright
        // rejection of `initialize`.
        self.transport.configure_http_headers(None, false);
        // The modern probe may have destroyed the only channel to a stdio server: a legacy
        // server that *exits* on an unknown method leaves `initialize` writing to a closed
        // stdin (`Broken pipe`), and a silent one is killed by the watchdog with the same
        // effect — so the fallback failed and such a server could not be discovered at all,
        // where initialize-first discovered it. One re-spawn, only when the probe produced no
        // bytes whatsoever, which is exactly those two cases: a server that *answered* the
        // probe has a live channel and must not be re-spawned (that would double the cost of
        // every legacy stdio server in a sweep, and re-launch a container that is still up).
        if matches!(probe_raw, ProbeEvidence::Absent(ProbeAbsence::NoResponse)) {
            self.respawn_stdio()?;
        }

        let init = self.transport.call("initialize", legacy_params(requested))?;
        // Bounded and shape-gated once, here, by the same function
        // `crate::negotiated_spec_revision` uses — so the live record and a later
        // re-derivation from the stored bytes cannot disagree. `None` means the server's
        // revision was not storable (empty, or carrying something that is not printable
        // ASCII); the bytes it actually sent stay in `handshake_raw` regardless.
        let negotiated = era::bounded_revision(&legacy_revision(&init.bytes)?);
        // Defence in depth (re-survey §1.4(b)): this is the one place a server-supplied
        // string could reach an HTTP header. Header injection is not reachable — `http`
        // rejects CR and LF in header values — but a value this client echoes should still
        // be gated to ten bytes of digits and dashes before it gets there.
        if let Some(revision) = negotiated.as_deref().filter(|r| era::is_revision_shaped(r)) {
            self.transport.configure_http_headers(Some(revision), false);
        }
        self.transport.notify("notifications/initialized", json!({}))?;
        let tools = self.transport.call("tools/list", json!({}))?;

        Ok(Discovery {
            handshake_raw: init.bytes,
            tools_list_raw: tools.bytes,
            probe_raw,
            negotiated_spec_revision: negotiated,
            discovery_path: DiscoveryPath::Initialize,
            era_provenance: EraProvenance {
                policy: ERA_POLICY,
                offered_revisions: offered,
                chosen_revision: requested,
                // The legacy handshake negotiates by construction: the revision on the
                // record is whatever `initialize` came back with.
                revision_source: RevisionSource::Negotiated,
                fallback_reason: Some(reason),
            },
        })
    }

    /// Replace a dead stdio transport with a freshly spawned one. A no-op over HTTP, where
    /// each request opens its own connection and nothing needs re-creating.
    ///
    /// One shot, and only from [`Self::discover_legacy`]: `discover` calls that function at
    /// most once, so this cannot loop no matter what a server does.
    fn respawn_stdio(&mut self) -> Result<(), DiscoveryError> {
        let Some(recipe) = &self.respawn else { return Ok(()) };
        let args: Vec<&str> = recipe.args.iter().map(String::as_str).collect();
        let transport =
            ChildProcessTransport::spawn_with_timeout(&recipe.program, &args, recipe.timeout)?;
        self.transport = Box::new(transport);
        Ok(())
    }
}

/// `params` for any `2026-07-28`+ request: a `_meta` block and nothing else.
///
/// `RequestParams` in that revision is exactly `{ _meta: RequestMetaObject }`, with `_meta`
/// non-optional and its `protocolVersion`/`clientCapabilities` members required
/// (`schema.ts` L63–111, L179–181). This applies to **every** request, `tools/list`
/// included — which is the part P0-09 missed, having extrapolated `initialize`-shaped
/// top-level params instead.
fn modern_params(revision: &str) -> Value {
    json!({
        "_meta": {
            "io.modelcontextprotocol/protocolVersion": revision,
            "io.modelcontextprotocol/clientCapabilities": {},
            "io.modelcontextprotocol/clientInfo": {
                "name": CLIENT_NAME,
                "version": CLIENT_VERSION,
            },
        },
    })
}

/// `params` for a `2025-11-25`-and-earlier `initialize`. Unchanged from P0-01.
fn legacy_params(revision: &str) -> Value {
    json!({
        "protocolVersion": revision,
        "capabilities": {},
        "clientInfo": { "name": CLIENT_NAME, "version": CLIENT_VERSION },
    })
}

/// A negotiation that could not be completed, naming both sides of the failed intersection
/// so the record says *why* rather than merely that it failed. `offered` is already
/// type- and length-bounded by [`crate::era`], so this string is bounded too.
fn negotiation_failed(what: &str, offered: &[String]) -> DiscoveryError {
    let implemented: Vec<&str> = CLIENT_SUPPORTED_REVISIONS.iter().map(|(r, _)| *r).collect();
    DiscoveryError::Protocol(format!(
        "version negotiation failed: {what} [{}], none of which this client implements [{}]",
        offered.join(", "),
        implemented.join(", ")
    ))
}

/// The `protocolVersion` a raw handshake response negotiated, for either era — the exact
/// function [`DiscoveryClient::discover`] uses to fill
/// [`Discovery::negotiated_spec_revision`], exposed so a consumer re-deriving from stored
/// [`Discovery::handshake_raw`] bytes (P0-10's census replay) reads the revision through the
/// same code path the live run did, rather than a second parser that could drift from it.
///
/// Handles both shapes:
///
/// - **Legacy** `InitializeResult`: `result.protocolVersion`, returned verbatim.
/// - **Modern** `DiscoverResult`: no such field exists in `2026-07-28` — it returns
///   `supportedVersions` and the *client* chooses. The choice is re-made here through
///   [`crate::era::choose_revision`], the same allowlist intersection the live run used, so
///   the two agree by construction rather than by coincidence.
///
/// One disclosed consequence: because the modern choice is re-derived from the client's
/// allowlist rather than read out of the bytes, adding a revision to
/// [`crate::era::CLIENT_SUPPORTED_REVISIONS`] could change what a re-derivation reports for
/// an old `DiscoverResult` that offered several revisions. Census provenance is outside the
/// pure, ruleset-versioned derivation path ADR-005 governs, so this is a documented
/// property rather than a violated invariant — but it is why the allowlist is ordered and
/// append-averse.
pub fn negotiated_spec_revision(handshake_raw: &[u8]) -> Result<String, DiscoveryError> {
    let value: Value = serde_json::from_slice(handshake_raw).map_err(|e| {
        DiscoveryError::Protocol(format!("handshake result is not valid JSON: {e}"))
    })?;
    let Some(result) = value.get("result") else {
        return Err(DiscoveryError::Protocol("handshake response carries no result".into()));
    };

    if let Some(revision) = result.get("protocolVersion").and_then(Value::as_str) {
        return era::bounded_revision(revision).ok_or_else(|| {
            DiscoveryError::Protocol(
                "handshake result's protocolVersion is not storable as a revision".into(),
            )
        });
    }

    let offered = era::offered_revisions(result.get("supportedVersions"));
    if let Some((chosen, _)) = era::choose_revision(&offered) {
        return Ok(chosen.to_string());
    }

    Err(DiscoveryError::Protocol(
        "handshake result carries neither a protocolVersion nor a supportedVersions entry \
         this client implements"
            .into(),
    ))
}

/// The legacy `initialize` result's own `protocolVersion`, verbatim.
///
/// Unbounded by design *at this layer only*: it is the raw observation, and its single
/// caller immediately hands it to [`crate::era::bounded_revision`] before it can escape the
/// crate. The old arrangement — "consumers bound it themselves" — leaked exactly as that
/// kind of arrangement does: `xtask::probe_stage1` moved the raw string into a git-tracked
/// SQLite file with no bound at all.
fn legacy_revision(bytes: &[u8]) -> Result<String, DiscoveryError> {
    let value: Value = serde_json::from_slice(bytes).map_err(|e| {
        DiscoveryError::Protocol(format!("initialize result is not valid JSON: {e}"))
    })?;
    value
        .get("result")
        .and_then(|r| r.get("protocolVersion"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| {
            DiscoveryError::Protocol("initialize result missing protocolVersion".into())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modern_params_carry_only_a_meta_block_with_the_three_required_members() {
        let params = modern_params("2026-07-28");
        let object = params.as_object().expect("params must be an object");
        assert_eq!(
            object.keys().collect::<Vec<_>>(),
            vec!["_meta"],
            "RequestParams in 2026-07-28 is exactly {{ _meta }} — nothing else may be sent"
        );
        let meta = &params["_meta"];
        assert_eq!(meta["io.modelcontextprotocol/protocolVersion"], "2026-07-28");
        assert!(meta["io.modelcontextprotocol/clientCapabilities"].is_object());
        assert_eq!(meta["io.modelcontextprotocol/clientInfo"]["name"], CLIENT_NAME);
        assert!(params.get("protocolVersion").is_none(), "no top-level protocolVersion");
        assert!(params.get("capabilities").is_none(), "no top-level capabilities");
        assert!(params.get("clientInfo").is_none(), "no top-level clientInfo");
    }

    #[test]
    fn negotiated_spec_revision_reads_a_legacy_initialize_result() {
        let bytes = br#"{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":"2025-11-25"}}"#;
        assert_eq!(negotiated_spec_revision(bytes).expect("must parse"), "2025-11-25");
    }

    /// `DiscoverResult` has no `protocolVersion` at all — the single most consequential of
    /// P0-09's four errors, since it meant every successful `server/discover` ended in
    /// `Protocol("handshake result missing protocolVersion")`.
    #[test]
    fn negotiated_spec_revision_chooses_from_a_modern_discover_result() {
        let bytes = br#"{"jsonrpc":"2.0","id":0,"result":{"resultType":"complete","supportedVersions":["2025-11-25","2026-07-28"],"capabilities":{},"ttlMs":0,"cacheScope":"private"}}"#;
        assert_eq!(
            negotiated_spec_revision(bytes).expect("must parse"),
            "2026-07-28",
            "the client chooses, preferring the newest revision it implements"
        );
    }

    #[test]
    fn negotiated_spec_revision_rejects_a_result_with_nothing_usable() {
        for bytes in [
            &br#"{"jsonrpc":"2.0","id":0,"result":{"supportedVersions":["2031-01-01"]}}"#[..],
            &br#"{"jsonrpc":"2.0","id":0,"result":{}}"#[..],
            &br#"{"jsonrpc":"2.0","id":0,"error":{"code":-32601,"message":"x"}}"#[..],
            &b"not json"[..],
        ] {
            assert!(
                negotiated_spec_revision(bytes).is_err(),
                "must not invent a revision for {:?}",
                String::from_utf8_lossy(bytes)
            );
        }
    }

    #[test]
    fn discovery_path_round_trips_through_its_string_form() {
        for path in DiscoveryPath::all() {
            assert_eq!(DiscoveryPath::parse(path.as_str()), Some(*path));
        }
        assert_eq!(DiscoveryPath::parse("something_else"), None);
    }

    #[test]
    fn http_status_renders_the_same_detail_the_july_census_recorded() {
        // `xtask::census_report` keys its `transport` category and its `failure_detail` on
        // this rendering; July's Class B results contain literal "http status: 401" strings.
        let err = DiscoveryError::HttpStatus { status: 401, retry_after: None };
        assert_eq!(err.to_string(), "http status: 401");
        let throttled =
            DiscoveryError::HttpStatus { status: 429, retry_after: Some("120".into()) };
        assert_eq!(throttled.to_string(), "http status: 429");
    }

    #[test]
    fn negotiation_failed_names_both_sides_of_the_intersection() {
        let message = negotiation_failed("x offered", &["2031-01-01".to_string()]).to_string();
        assert!(message.contains("2031-01-01"), "{message}");
        assert!(message.contains(MODERN_PREFERRED), "{message}");
    }
}
