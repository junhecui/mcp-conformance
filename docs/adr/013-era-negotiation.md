# ADR-013: MCP era negotiation, the revision allowlist, and status-keyed failures

**Status:** Accepted
**Date:** 2026-10-08
**Author:** Jun Cui
**Closes:** [P0-11](../tasks.md#p0-11-correct-dual-era-discovery-for-mcp-2026-07-28)
**Supersedes in part:** [P0-09](../tasks.md#p0-09-serverdiscover-fallback-for-mcp-spec-2026-07-28)
(its extrapolated `server/discover` shape and its fallback trigger)
**Related:** P0-01 (discovery client), P0-02 (metadata pin), P0-10 (census evidence and
provenance), ADR-002 (containability as a corpus partition), design.md §3,
[`prior-art-resurvey-2026-10.md`](../prior-art-resurvey-2026-10.md) §1.3–§1.4

---

## Context

MCP revision `2026-07-28` shipped final and removed the `initialize`/`notifications/initialized`
handshake. P0-09 added a `server/discover` fallback written from extrapolation, before the
spec's example request/response pairs were available, and it is wrong on four independent
axes: the request params, the response parsing, the post-handshake `tools/list` params, and
the HTTP headers. Over HTTP the fallback was also unreachable — a conformant modern-only
server answers the legacy `initialize` with HTTP 400, `ureq` turned that into
`DiscoveryError::Transport` because `http_status_as_error` defaults to `true`, and
`is_initialize_unavailable` deliberately excluded `Transport`. The net effect was that the
harness could not discover a conformant `2026-07-28` server at all.

The spec's own backward-compatibility sections prescribe the algorithm, and the re-survey
read them clause by clause (§1.3) and probed five public endpoints (§1.6, Appendix A). What
is left open is a set of choices the spec does not make for us, and those are what this ADR
records. Everything here is implemented in `crates/discovery` (`era.rs`, `client.rs`,
`transport.rs`) and in `xtask/src/census_report.rs`.

---

## Decision

### 1. Era is probed modern-first, and decided from the response — never from a single error code

`server/discover` goes out first, carrying the full `_meta` block and (over HTTP) both
`MCP-Protocol-Version` and `Mcp-Method`. A `DiscoverResult` or a recognised modern error
(`-32020`, `-32021`, `-32022`, or an HTTP 404 carrying `-32601`) means the server is modern
and **must not** be downgraded. Anything else — a non-modern error code, an unreadable or
empty body, an SSE upgrade, or silence over stdio — falls back to `initialize`.

**Rejected:** keeping P0-09's initialize-first order. It mostly works over stdio but the
spec forbids keying the era decision on one code (`-32601`), real legacy servers use
`-32600` and `-32000` instead (observed: DeepWiki, GitMCP), and over HTTP the fallback was
structurally unreachable.

**Two divergences from the re-survey's §1.4, both deliberate:**

- §1.4(e) lists the modern-first order but does not say what to do with a `DiscoverResult`
  whose `supportedVersions` names only pre-`2026-07-28` revisions. That is decidable rather
  than ambiguous: use the legacy handshake at the revision the server named, recorded as
  `FallbackReason::OnlyLegacyRevisionsOffered`. It is a downgrade the *server* asked for,
  which is different in kind from one provoked by garbage — and separate from
  `OnlyLegacyRevisionsAfterUnsupportedVersion`, the same outcome reached by a server that
  *rejected* the probe with `-32022` and named only legacy revisions in `data.supported`. Two
  distinct server behaviours get two codes; one code left them indistinguishable in a
  published distribution.
- §1.4(e)'s classifier calls an HTTP 404 carrying `-32601` *modern*, but does not say how
  discovery then proceeds — there is no `DiscoverResult` to read. This client stays modern
  and sends `tools/list` with the modern `_meta` at its own preferred modern revision, as a
  third `DiscoveryPath`, `modern_without_discover`.

  **The justification this ADR originally gave for that was wrong, and wrong in the same way
  P0-09 was — by misreading the spec.** It argued that "since `schema.ts` makes
  `server/discover` a method clients MAY call rather than must, the coherent action is to
  stay modern". That MAY governs the **client's** obligation to call the method; **servers
  MUST implement it** (`schema.ts` line 655, `basic/versioning.mdx`, quoted in the re-survey
  §1.2). So a 404/`-32601` to `server/discover` is a **non-conformant server**, not a
  conformant modern server exercising an option, and the cited clause supplies no basis at
  all for the phrase "speaks modern framing but has not implemented the (client-optional)
  discovery method" — there is no such category. Stated plainly rather than quietly edited,
  because a spec misreading is the defect class this task exists to correct.

  The behaviour stands. The defensible argument for it is:

  1. HTTP 404 plus `-32601` is specifically what `2026-07-28` prescribes for an unknown
     method, and the spec forbids downgrading on a recognised modern signal. Legacy servers
     were *observed* using HTTP **200** for `-32601` instead (Appendix A.1, Cloudflare), and
     the re-survey's own §1.4(e) classifier calls 404/`-32601` modern.
  2. Proceeding optimistically is safe because the failure mode is clean rather than
     silently corrupting: a legacy server reached this way answers the following
     `tools/list` with an error and the guess is *corrected* (§6 below), while a modern
     server answers `-32020`/`-32022` if the request is wrong.

  That argument also forces an admission the old wording hid: **on this path the server is
  broken, and the revision is assumed, never negotiated.** §7 records what the harness
  publishes as a result.

### 2. Every revision this client sends comes from a closed client-side allowlist

`era::CLIENT_SUPPORTED_REVISIONS` is an ordered, hardcoded list of the five revisions whose
lifecycles this binary implements. A server's `supportedVersions` (or an `-32022`'s
`data.supported`) is **intersected** with it, and the client takes its own most-preferred
member of the intersection. An intersection that is empty is a **discovery failure, not a
downgrade**: the alternative is proceeding at a revision the client does not implement, on a
string the server chose.

So a server can narrow what this client uses; it can never introduce a value. The one place
a server-supplied revision string is still echoed is the legacy path's
`MCP-Protocol-Version` header, which must carry whatever `initialize` negotiated — and that
value is gated on `era::is_revision_shaped`, i.e. exactly ten bytes of ASCII digits and two
dashes. Header injection was verified unreachable (`http` rejects CR and LF in header
values), so the gate is defence in depth; an unshaped revision is still *recorded* as the
fact it is, it simply never becomes a header.

**Rejected:** allowlisting the echoed legacy value too. That would turn a currently-working
discovery against a server at some future revision into a failure, for no security gain over
the shape gate.

**Known cost, accepted:** on the modern path the revision is *re-chosen* from the allowlist
rather than read out of the stored bytes, so `negotiated_spec_revision()` re-derives it the
same way the live run did. Adding a revision to the allowlist could therefore change what a
re-derivation reports for an old `DiscoverResult` that offered several. Census provenance sits
outside the pure, ruleset-versioned derivation path ADR-005 governs, so this is a documented
property rather than a violated invariant — and it is why the list is ordered and
append-averse.

### 3. The era classifier is a separate read-only function; `decode_and_validate` is untouched

`era::classify_discover_outcome(status, bytes)` is a pure function of an HTTP status and a
body. It never inspects the JSON-RPC `id`.

This is forced. `transport::decode_and_validate` requires `id.as_u64()` as a deliberate
anti-hostile-server measure, and *both* real 4xx bodies captured from public servers violate
it — DeepWiki substitutes the string id `"server-error"`, GitMCP returns `"id": null`. Those
bodies must still be classified, so the only two options were to relax the id check or to
classify alongside it.

**Rejected:** relaxing `decode_and_validate`. The id check is what stops a hostile server
answering a question the client did not ask; weakening it to read an error body would trade a
real protection for a parsing convenience. A separate function costs one module and keeps both
properties. `transport.rs` carries a test
(`decode_and_validate_rejects_the_real_4xx_bodies_the_era_classifier_must_read`) that pins the
reason: it asserts those two bodies *are* rejected there, so a future reader cannot mistake the
duplication for an oversight.

Everything the classifier takes from a server response is type- and length-bounded before use:
revision strings must be revision-shaped (ten bytes) and are capped at 64 per response; a
modern-era error message is capped at 512 characters.

### 4. HTTP statuses are captured explicitly, and the failure taxonomy stays keyed on them

The agent is built with `.http_status_as_error(false)`. This is mandatory, not a preference:
`ureq::Error::StatusCode` carries only a number — no response, no body — so a 4xx body, which
the dual-era algorithm requires reading, is unreachable while the flag is on.

The flag is agent-wide, so it changes every request including `tools/list`. Left unmanaged, a
403 or 500 HTML error body would reach `decode_and_validate` and surface as `Protocol(…)`
where it previously short-circuited as `Transport`, silently moving HTTP-level failures into
the `protocol` bucket and breaking comparability with the July census split (435 `transport`
against 312 `protocol`). So:

- a new `DiscoveryError::HttpStatus { status, retry_after }` carries the status deliberately;
- its `Display` renders exactly `http status: NNN`, byte-identical to what
  `ureq::Error::StatusCode` rendered through `Transport(e.to_string())`, because that string
  is literally what the July results files contain;
- `xtask::census_report::http_status_category` maps it back to `transport`, shared with
  `probe_stage1` so the two sweeps cannot disagree;
- only the two **era-signalling** statuses (400, 404) are handed to the classifier as data.
  Every other non-2xx fails without its body being read — a 500's HTML is of no use, and not
  reading it is also the cheapest thing to do at sweep scale.

The one deliberate departure from July's taxonomy: **`429` becomes its own category**,
`rate_limited`. July recorded two of them as plain `transport`, indistinguishable there from a
dead host, which makes self-inflicted throttling unmeasurable. `Retry-After` is captured
(sanitised and length-bounded) so a sweep can skip the host; nothing retries inside a run.

### 5. Era provenance travels with every record, because the fallback is server-influenceable

A hostile server can choose which era the harness records about it — by stalling, or by
answering `server/discover` with garbage. No security control is relaxed by that downgrade:
the four annotations are byte-identical across revisions and P0-02's pin is
revision-independent. But P0-10 publishes `discovery_path` distributions, and a server must
not be able to skew published provenance about itself invisibly.

So `Discovery::era_provenance` records the policy (`modern_first`), what the server offered,
what the client chose, and the `FallbackReason` — a closed taxonomy — and
`xtask::census_report` writes all five fields per server and counts the policies and reasons
corpus-wide, seeding every reason with an explicit zero so a bucket that is absent cannot be
mistaken for one that is empty. A coordinated attempt to steer the era distribution then
shows up as a spike in one reason rather than as an unexplained shift in a histogram. The
policy field also exists because initialize-first (P0-09) and modern-first (P0-11) classify a
*dual-era* server differently, so records produced under the two policies are not comparable
and must not be pooled — the same discipline ADR-002 imposes across oracles.

---

### 6. An unconfirmed modern path is correctable; a confirmed one is not

`modern_without_discover` is the one modern path reached **without the server ever confirming
it is modern** — see §1. Mapping JSON-RPC `-32601` onto HTTP 404 is a documented
JSON-RPC-over-HTTP convention rather than a `2026-07-28` invention, so a legacy server can
present exactly the shape the classifier reads as modern, and such a server was previously
locked onto the modern path: it never got an `initialize` at all, and so **failed discovery
where P0-09 succeeded**. Sized from the committed July Class B file, 60 of 1,000 servers
returned `http status: 404`, so the exposure was 0–60 per 1,000 and *undeterminable* — that
sweep predates evidence persistence.

**Decision.** When the `tools/list` that follows `modern_without_discover` fails, retry the
legacy handshake once and record `FallbackReason::ModernWithoutDiscoverToolsListFailed`. Every
*confirmed* modern path — a `DiscoverResult`, or a `-32022` re-negotiation — propagates its
`tools/list` failure untouched, because downgrading a server that told us it is modern is
precisely what the spec forbids. Requiring a well-formed JSON-RPC envelope (§7) narrows the
trap usefully on its own, since a legacy server using the plain convention often emits no
envelope, but it does not close it.

**Consequences.** The optimistic guess costs one wasted `tools/list` when wrong, and the
population that pays it is now countable by reason code rather than invisible. The correction
is one shot and cannot loop. A legacy server that happens to *answer* `tools/list` without an
`initialize` is still recorded as `modern_without_discover` — a misclassification, and one
whose revision is labelled `assumed` rather than claimed as negotiated, which is why §7 is
what makes §6 tolerable.

One good asymmetry preserved deliberately: a 404 carrying an **HTML** body already degrades to
the legacy fallback, because it is not a JSON-RPC message at all.

---

### 7. A revision is published as negotiated only if it was negotiated

**Context.** `modern_without_discover` filled `negotiated_spec_revision` from
`MODERN_PREFERRED` — a client-side constant — for a server that named no revision. The
resulting record claimed `negotiated_spec_revision: "2026-07-28"` about a server that said no
such thing, and *could not be contradicted offline*: `discovery::negotiated_spec_revision()`
errors on that same stored 404 body, so the live claim and its own evidence disagreed and the
evidence lost. It was also a mutually inconsistent record — a populated `chosen_revision`
beside an empty `offered_revisions`. The field feeds `TOOL_SNAPSHOT.spec_revision`.

Proven by PoC before the fix: a fake answering one HTTP POST with status 404 and the 25-byte
body `{"error":{"code":-32601}}` — no `jsonrpc` member, no `id`, nothing that requires a
JSON-RPC implementation to produce — was recorded as `discovery_path=modern_without_discover`,
`negotiated_spec_revision="2026-07-28"`, `offered_revisions=[]`, `fallback_reason=None`. That
is the cheapest branch in the system to forge, and §5's own justification says a server must
not be able to skew published provenance about itself invisibly.

**Decision, two parts.**

1. `Discovery::negotiated_spec_revision` is an `Option<String>`, `None` on this path, and
   `EraProvenance::revision_source` is a closed `negotiated | assumed`. The assumed value
   stays visible in `chosen_revision`, and the results file and the DB column now distinguish
   negotiated from assumed the way `discovery_path` already distinguished the paths. A
   populated `chosen_revision` next to an empty `offered_revisions` is coherent exactly when
   the source is `assumed`. The census revision distribution gains a `<none>` bucket for it,
   rather than mislabelling it `<unparseable>` — nothing was unparseable; there was no
   revision.
2. A 400/404 counts as a modern signal only if its body is a **well-formed JSON-RPC message**
   (`"jsonrpc": "2.0"` present). A bare `error` object is not evidence of modern framing. The
   `id` is still deliberately not examined — §3's reason for that is unchanged.

**Rejected:** keeping the client's preference in the field and documenting the caveat. A
caveat in a doc comment does not travel with a row in a results file.

**Also bounded where it leaves the crate, not per consumer.** `era::bounded_revision` is the
single gate for a server-echoed legacy revision: rejected outright if it carries anything that
is not ASCII-graphic-or-space (a revision is an identifier, and this value becomes a terminal
line, a JSON map key and a `TEXT` column), otherwise truncated to 64 characters. Both the live
handshake and `negotiated_spec_revision()` call it, so a re-derivation cannot drift from the
live run. The old arrangement — "consumers bound it themselves" — leaked exactly as such
arrangements do: `xtask::probe_stage1` moved the raw string into `SERVER.spec_revision` and
`VERDICT.protocol_version` with no bound at all, inside a **git-tracked** SQLite file. Both
columns now also carry `CHECK (length(...) <= 64)` as a backstop.

Note this gate is deliberately *weaker* than `is_revision_shaped`, which still guards the one
sink that needs ten-bytes-of-digits strictness: an HTTP header value.

---

### 8. The probe response is evidence, and an absence of it is recorded as one

**Context.** Under modern-first the era is a *derived* field, and on every fallback the
`server/discover` probe bytes were discarded. That made `fallback_reason` an unauditable
client assertion and left the era classification as the one derived field in the system with
no replayable preimage — against architecture.md §6 invariant 2. Proven by PoC: marker strings
in a probe body appear nowhere in `Discovery::handshake_raw`.

The attacker gain is sharper than hiding modern capability. A dual-era server's modern and
legacy handlers may expose **different tool sets with different annotations**, so answering the
probe with garbage *steers the harness onto the handler the server chose*. P0-02's pin keeps
the verdict honest about the snapshot that was tested, but the published claim then describes
a face the server selected and nothing in the evidence store shows that a selection happened.

**Decision.** `Discovery::probe_raw` is a `ProbeEvidence`: either the captured bytes, or an
explicit `ProbeAbsence` — `no_response` (nothing was ever received: closed stdio pipe, or a
watchdog kill) or `body_not_read` (a response arrived and the transport rejected it before
reading the body, i.e. an SSE upgrade). `census_report::persist` writes the bytes as a third
evidence blob and `era_provenance.probe_raw` carries `{state, digest, bytes}`; re-derivation
re-reads the blob, so a missing probe blob fails as loudly as a missing `tools/list`.

**Nothing may now represent "evidence existed and was not kept."** That is the point of the
type: an absent field cannot tell that apart from "no evidence existed."

**Cost:** one more blob per discovery. Near zero in practice — the store is content-addressed,
so on a modern path the probe response *is* the handshake response and they share one blob,
and real probe bodies are 100–300 bytes.

---

### 9. Server-authored prose does not reach a committed results file

A `-32020`/`-32021` body is the **first** thing a server gets to say, pre-handshake, and up to
512 characters of it travelled into the census `server_error` failure detail and so into
`results/census/*.json`, which is committed. Confirmed by PoC with an
`IGNORE PREVIOUS INSTRUCTIONS…` payload. That is exactly the position
`results/census/README.md` keeps evidence blobs *out of*, for exactly the reason it gives:
committed into a public repository those bytes sit where a future agent session reads the tree
as project content.

**Decision.** `DiscoverClass::ModernFatal` carries `message_sha256`, not the message. The
published value is the code plus the digest; the text stays in the uncommitted evidence blob,
where it remains recoverable and still matchable against another server's. Applied to this one
path because it is the new exposure — a post-handshake `ServerError` from `initialize` or
`tools/list` carried prose in July too, so narrowing it there is a separate change with a
comparability cost, not a regression fix.

The same reasoning applies to this repository's own docs: `docs/tasks.md` is auto-imported
into every session, so verbatim server prose belongs in a research note under `docs/` with its
own redaction policy. P0-11's live-verification table was moved accordingly.

---

## Consequences

- **The harness can discover a conformant `2026-07-28` server**, which it could not before.
  Verified against three live modern-capable public endpoints, and pinned by a replay test
  over a real captured `DiscoverResult`.
- **Some servers become reachable that were not.** Two of the three modern-capable hosts
  answer a *legacy* `initialize` over SSE, which this transport does not speak, while
  answering `server/discover` as `application/json`. Part of the July census's "SSE-only"
  failure population is therefore reachable now.
- **Every legacy server costs one extra round trip.** That is the price of modern-first and
  the spec accepts it. The populations that dominate census failures are unaffected: a
  connection-level failure and any non-era-signalling status (401 above all) still cost
  exactly one request, asserted by request-count tests.
- **A stdio server that destroys its channel answering the probe is discovered again.** A
  legacy server that *exits* on an unknown method left `initialize` writing to a closed stdin
  (`Broken pipe`), so the fallback failed and the server could not be discovered at all —
  strictly worse than initialize-first. The legacy fallback now re-spawns the child, once, and
  only when the probe produced **no bytes whatsoever** (`ProbeAbsence::NoResponse`), which is
  exactly that case and the watchdog-killed-silent-server case. A server that *answered* the
  probe has a live channel and is never re-spawned; re-spawning those would double the cost of
  every legacy stdio server in a sweep.
- **The silent-stdio-server case is improved but not solved.** It still spends the full
  watchdog before the fallback; the re-spawn is what makes the fallback itself succeed
  afterwards. §1.4(e)'s short dedicated probe read deadline is still the real fix and
  `StdioTransport` still has no read deadline (adding one needs a reader thread). At 45 s per
  host, and with N registry entries able to point at one implementation, that is 45N seconds
  — so sizing the exit-on-unknown-method and silent-on-unknown-method populations is a
  prerequisite for the next Class A sweep, not a curiosity.
- **A spawned child's stderr is piped, bounded and escape-stripped, not inherited.** Under
  Stage 2 that pipe carries an arbitrary registry package's stderr (proxied out of
  `docker run`), and inheriting it handed that package the sweep console: unbounded volume and
  raw ANSI/OSC sequences, on a surface this project's workflow has a model read. `Stdio::null()`
  was rejected — P0-06's `EBADENGINE` and entry-point diagnoses came out of exactly these
  bytes — so they are kept, capped at 8 KiB per child, filtered to printable ASCII and tab, and
  prefixed `[server stderr]`. Draining continues past the cap so a full pipe never blocks the
  child.
- **`tools/list` pagination is still not followed** in either era. Unchanged by this ADR,
  real, and its own task.

---

## Flagged, recorded rather than fixed

**⚑ Flagged for the next ≥1,000-host sweep: the conduct disclosure, with its size.**
Modern-first sends an **unsolicited pre-handshake request to every host in the corpus**, about
75% of which is legacy, taking those hosts from 3 requests to 4. And
`xtask/src/census_stage1.rs` still has **no intra-host delay**, so a host's 2–4 requests go out
back to back with zero spacing. This project's own O-01 probe lost a transcript to a 429 caused
by three requests inside one second, and P0-11's live verification imposed a 1.2 s floor **by
hand** that the sweep code does not have. Landing the intra-host delay and the eTLD+1 request
budget is therefore a **prerequisite** for the next ≥1,000-host sweep, not a nice-to-have: it
is what makes "polite client" true rather than aspirational. Note also that the defence of
modern-first rests on *"the newer revision of the same protocol prescribes this ordering"* —
**not** on a MUST; see §1, where the MUST/MAY misreading is corrected.

**⚑ Flagged: the `{400,404}` × SSE taxonomy asymmetry.** A probe answered with an SSE upgrade
behind a 400 or a 404 is rejected by the transport before its status is considered, so it lands
in the `protocol` failure bucket where July's taxonomy would have put it in `transport`. The
real tension in "just make it a status failure" is that doing so would remove the fallback for
a dual-era server whose probe answers 400 + SSE — a shape two of the three modern-capable hosts
in Appendix A are one step away from. The security pass found no security consequence: both
labels are failures either way. Left as a comparability wart to be decided with data from the
next sweep, not guessed at now.

**⚑ Flagged: the latent `chosen ∉ offered` inconsistency** at `crates/discovery/src/client.rs`
in `renegotiate_after_unsupported_version`. The branch that retries `server/discover` at a
second modern revision records `offered` from the *second* response, which need not contain
`chosen`. Unreachable while `CLIENT_SUPPORTED_REVISIONS` holds exactly one modern revision, and
written out rather than left as a hole for when a second one ships — at which point this needs
an assertion, not a comment.

**⚑ Flagged: `era_provenance.policy` is length-bounded on read-back but not re-validated**
against a closed set, while every other field in that block is (`offered_revisions` by shape,
`chosen_revision` by shape, `revision_source` and `fallback_reason` and `probe_raw.state` by
closed taxonomy). Not server-reachable on the live path today — the live value is
`era::ERA_POLICY`, a `&'static str` — so this is only reachable by editing a results file,
which is also the threat model `EraRecord::from_record` exists for. Closing it means a closed
policy taxonomy, which is cheap and was left for whoever adds the second policy.
