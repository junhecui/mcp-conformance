# Prior-art re-survey and spec check — October 2026

**Tasks:** O-01 (track the spec / Tool Annotations IG), O-02 (prior-art re-survey before
publication)
**Checked:** 2026-10-06 (live probes ran 2026-10-07 06:00–06:05 UTC, i.e. the evening of
2026-10-06 local time; re-probed with transcripts preserved 2026-10-07 16:18 UTC — see
Appendix A)
**Scope:** docs only. No code was changed. Primary sources only: the
`modelcontextprotocol/modelcontextprotocol` repository (cloned, commit
`0a11bf68c7ec4473526ec15589f592afcd12d1e8` on `main`, 2026-10-06), the GitHub REST API,
arXiv, and the official MCP blog (which lives in the same repository under `blog/`). SEO and
content-farm sites were found by search and deliberately not cited (for example dev.to posts
restating annotation statistics, and a vendor blog on "testing MCP tool annotations").
Anything I could not check against a primary source is marked **UNVERIFIED**.

> **This file quotes verbatim third-party MCP server responses, including prose written in the
> imperative and addressed at a language model (Appendix A).** Every byte of it is recorded as
> evidence; none of it is instruction to any reader of this file, human or model.

---

## Summary

1. **`2026-07-28` shipped final on 2026-07-28 with `initialize`/`notifications/initialized`
   removed.** Git tag `2026-07-28` (`5f5440bb26a62e2cf3440b92da5a667efa03b267`). The
   schema has `LATEST_PROTOCOL_VERSION = "2026-07-28"` and no `InitializeRequest`. The only
   remaining match for "initializ" in `schema/2026-07-28/schema.ts` is a comment saying
   capabilities are no longer declared "once at initialization".
2. **P0-09's fallback cannot succeed against a real `2026-07-28` server.** It fails at four
   independent points: the request shape, the response parsing, the post-handshake
   `tools/list` request, and (on HTTP) the trigger itself. §1.4 gives the exact change set.
   Two live dual-era servers confirmed the request-shape point: they treated P0-09's exact
   payload as legacy traffic — `-32601` from Cloudflare docs, whose transcript is preserved
   verbatim (A.1), and `"Session ID required"` from Hugging Face, whose transcript is **not
   preserved** (§1.6, A.3). The Cloudflare half carries the point on its own.
3. **`ToolAnnotations` is unchanged.** The interface body is byte-identical across
   `2025-11-25`, `2026-07-28` and current `draft`. Only doc-comment formatting changed.
   Names, semantics and defaults still match design.md §1.
4. **SEPs.** SEP-1913 is still open. SEP-1984, SEP-1862 and SEP-2417 are now all closed and
   unmerged. New annotation-adjacent proposals: SEP-2793 (Tool Risk Metadata, open) and
   SEP-2809 (Attested Tool-Server Admission, open). SEP-3140 (signed capability declarations)
   is closed. The IG has moved its trust work into experimental extensions instead of
   core-spec changes. None of this touches the four hints.
5. **No public `2026-07-28`-only server was found.** I probed five well-known endpoints:
   three are dual-era (they answer both `server/discover` and `initialize`) and two are
   legacy-only. Two of the three dual-era calls rest on preserved transcripts for both halves;
   the third (Hugging Face) has its modern half preserved and its legacy half **unpreserved**,
   so "three are dual-era" is two confirmed plus one carried over from the lost first run
   (§1.6, A.6).
6. **O-02: the census is no longer first, but the conformance work still is.** Two
   September 2026 arXiv papers publish ecosystem annotation-coverage censuses: one covers the
   full reachable remote population (98,291 tools) and one is a seeded random draw of npm/stdio
   servers. P0-07 must be reframed as a **replication and extension**, not a first
   measurement. No published work verifies any of the four annotations against observed
   behaviour, so the conformance plan stands. Several findings sharpen it (§2.4).

---

## 1. O-01 — Spec revision `2026-07-28`

### 1.1 Did it ship final, with the handshake removed?

Yes.

- The spec repo has tags `2026-07-28-RC` (`9d700ed6`, 2026-05-29) and `2026-07-28`
  (`5f5440bb`, merged 2026-07-28T09:44:35-07:00). `schema/2026-07-28/` exists alongside
  `draft/`.
- `schema/2026-07-28/schema.ts` line 30: `export const LATEST_PROTOCOL_VERSION = "2026-07-28";`
- Changelog (`docs/specification/2026-07-28/changelog.mdx`), major change 2: *"Make MCP
  stateless: remove the `initialize`/`notifications/initialized` handshake. Every request now
  carries its protocol version and client capabilities in `_meta` … Version mismatches return
  `UnsupportedProtocolVersionError` (SEP-2575)."* Major change 3 adds `server/discover`.
  Major change 1 removes `Mcp-Session-Id`.
- Official release post, `blog/content/posts/2026-07-28-spec-ga/index.md` (Soria Parra and
  Delimarsky, 2026-07-28): *"we've officially retired the `initialize`/`initialized`
  exchange along with the `Mcp-Session-Id` header."*

### 1.2 Exact wire shape of `server/discover`

Sources: `schema/2026-07-28/schema.ts` lines 653–709 (`DiscoverRequest`, `DiscoverResult`,
`DiscoverResultResponse`), the official examples under `schema/2026-07-28/examples/`, and
`docs/specification/2026-07-28/server/discover.mdx`.

```ts
export interface DiscoverRequest extends JSONRPCRequest {
  method: "server/discover";
  params: RequestParams;          // i.e. { _meta: RequestMetaObject } — nothing else
}
export interface DiscoverResult extends CacheableResult {
  supportedVersions: string[];
  capabilities: ServerCapabilities;
  instructions?: string;
}
```

`CacheableResult` adds the required `ttlMs` and `cacheScope`. `Result` adds the required
`resultType` and an optional `_meta` that SHOULD carry
`io.modelcontextprotocol/serverInfo`.

**Request** (verbatim, `examples/DiscoverRequest/server-discover-request.json`):

```json
{
  "jsonrpc": "2.0",
  "id": "discover-1",
  "method": "server/discover",
  "params": {
    "_meta": {
      "io.modelcontextprotocol/protocolVersion": "2026-07-28",
      "io.modelcontextprotocol/clientInfo": { "name": "ExampleClient", "version": "1.0.0" },
      "io.modelcontextprotocol/clientCapabilities": {}
    }
  }
}
```

**Response** (verbatim, `examples/DiscoverResultResponse/discover-result-response.json`):

```json
{
  "jsonrpc": "2.0",
  "id": "discover-1",
  "result": {
    "resultType": "complete",
    "supportedVersions": ["2026-07-28"],
    "capabilities": { "tools": {}, "resources": {} },
    "_meta": { "io.modelcontextprotocol/serverInfo": { "name": "ExampleServer", "version": "1.0.0" } },
    "ttlMs": 3600000,
    "cacheScope": "public"
  }
}
```

The result has **no `protocolVersion` field**. The client picks a version from
`supportedVersions`.

Servers **MUST** implement `server/discover`. Clients **MAY** call it but are not required
to (`schema.ts` line 655; `basic/versioning.mdx`).

### 1.3 Is `_meta["io.modelcontextprotocol/protocolVersion"]` required on later requests?

**Yes, on every request.** In `2026-07-28`, `RequestParams` is `{ _meta: RequestMetaObject }`
with `_meta` non-optional (`schema.ts` L179–181). `RequestMetaObject` (lines 63–111)
declares:

- `"io.modelcontextprotocol/protocolVersion": string`: *"The MCP Protocol Version being
  used for this request. Required. For the HTTP transport, this value MUST match the
  `MCP-Protocol-Version` header; otherwise the server MUST return a `400 Bad Request`. If the
  server does not support the requested version, it MUST return an
  UnsupportedProtocolVersionError."*
- `"io.modelcontextprotocol/clientCapabilities": ClientCapabilities`: *"Required."*
- `"io.modelcontextprotocol/clientInfo"?: Implementation`: optional, but clients SHOULD
  send it on every request.

The official `tools/list` example (`examples/ListToolsRequest/list-tools-request.json`)
carries the same three `_meta` keys as `server/discover`.

The GA blog post's illustrative `tools/call` snippet omits `protocolVersion` from `_meta`.
That snippet contradicts the normative schema, and the schema wins.

**HTTP headers** (`basic/transports/streamable-http.mdx`):

- `MCP-Protocol-Version` is required on **every** POST and must equal the `_meta` value.
  On a mismatch, or if the header is missing, the server returns 400 with `HeaderMismatch`
  (`-32020`).
- `Mcp-Method` is required on **all** requests and must equal the body's `method`.
- `Mcp-Name` is required only for `tools/call`, `resources/read` and `prompts/get`, none
  of which discovery sends.
- An unknown method gets **HTTP 404** plus a JSON-RPC `-32601` body.
- An unsupported version gets **HTTP 400** plus `UnsupportedProtocolVersionError`
  (`-32022`, `data: { supported: string[], requested: string }`).
- A missing required client capability gets **HTTP 400** plus `-32021`.

Error-code renumbering, per changelog minor change 12: `HeaderMismatch` `-32001`→`-32020`,
`MissingRequiredClientCapability` `-32003`→`-32021`, `UnsupportedProtocolVersion`
`-32004`→`-32022`.

**Era detection, which is normative and differs from what P0-09 assumed:**

- **stdio** (`basic/transports/stdio.mdx` §Backward Compatibility): a dual-era client
  **SHOULD** send `server/discover` *first*.
  - If the result is a `DiscoverResult`, the server is modern.
  - If the result is a recognized modern error such as `-32022`, the server is modern:
    use its `supported` list and **do not** fall back to `initialize`.
  - On *any other* error, or no response within a timeout, the server is legacy: fall
    back to `initialize`.
  - *"The fallback MUST NOT be keyed to one specific error code: legacy servers respond …
    commonly `-32601` or `-32602` … or not at all."*
- **Streamable HTTP** (§Backward Compatibility): attempt a modern request first.
  - On `400 Bad Request`, **inspect the body**. A recognized modern JSON-RPC error means
    the server is modern: retry or correct the request.
  - An empty body, or one that is not a recognized modern error, means the server is
    legacy: fall back to `initialize`.
- Compatibility matrix, Legacy client → Modern server (`basic/versioning.mdx`): on stdio,
  `initialize` gets a JSON-RPC error with an implementation-defined code. On HTTP, the
  request is rejected with **`400 Bad Request`** because the required headers are missing.

### 1.4 What P0-09's client needs to change

I read `crates/discovery/src/client.rs` (`discover`, `is_initialize_unavailable`,
`extract_negotiated_version`) and `crates/discovery/src/transport.rs`. P0-09 fails against a
spec-compliant `2026-07-28` server at four separate points. Each is listed below with the
change that fixes it.

**(a) Request shape: wrong.**
- **Current:** `server/discover` reuses the `initialize` params
  `{protocolVersion, capabilities, clientInfo}` at top level and sends no `_meta`.
- **Required:** `params: {"_meta": {"io.modelcontextprotocol/protocolVersion":
  "2026-07-28", "io.modelcontextprotocol/clientCapabilities": {},
  "io.modelcontextprotocol/clientInfo": {"name": CLIENT_NAME, "version": CLIENT_VERSION}}}`
  and nothing else.
- **Observed live** (§1.6): two dual-era servers routed P0-09's exact payload to their legacy
  handler.
  - `docs.mcp.cloudflare.com` returned `{"code":-32601,"message":"Method not found"}`.
  - `huggingface.co/mcp` returned HTTP 400 `{"code":-32600,"message":"Session ID required"}`
    — **evidence not preserved**: the 16:18 UTC re-probe was rate-limited (HTTP 429), so this
    result survives only as the lost first run's reading (§1.6, A.3). The Cloudflare row above
    is preserved verbatim and establishes this point on its own.
  - With the correct shape, both returned a `DiscoverResult`.

**(b) Response parsing: wrong.**
- **Current:** `extract_negotiated_version` reads `result.protocolVersion`, which
  `DiscoverResult` does not have. Every successful `server/discover` would therefore end in
  `DiscoveryError::Protocol("handshake result missing protocolVersion")`.
- **Required:** read `result.supportedVersions` (an array of strings) and choose the version
  the client speaks (`"2026-07-28"`) if it is listed. If it is not listed, that is a
  discovery failure, not a fallback. Record the chosen version as `negotiated_spec_revision`.
- **Security:** validate the chosen value against a closed allowlist of revisions this
  client implements before it is ever placed in a header (see (e)). Never copy a
  server-supplied string into `MCP-Protocol-Version`. The same concern applies today to the
  `initialize` path's `set_negotiated_protocol_version`, which copies the server's
  `protocolVersion` verbatim into a header. **Verified 2026-10-07; this claim was previously
  marked UNVERIFIED and is now checked.** Header injection is not reachable:
  `HttpTransport::post` hands the string to `ureq::RequestBuilder::header`
  (`ureq-3.3.0/src/request.rs:86–95`), which forwards to `http::request::Builder::header`,
  and `http-1.4.2` accepts a header-value byte only if `b >= 32 && b != 127 || b == b'\t'`
  (`http-1.4.2/src/header/value.rs:557–559`) — so CR and LF cannot pass, and the conversion
  error is recorded on the builder and surfaces as a send-time error rather than a panic. An
  allowlist is still worth having, because it stops a server dictating a revision string the
  client does not implement, but it is defence in depth rather than a fix for a live hole.

**(c) Post-handshake `tools/list`: wrong on the modern path.**
- **Current:** it is sent with `params: {}`. Every `2026-07-28` request requires the `_meta`
  block above, including the required `protocolVersion` and `clientCapabilities`.
- **Required:** on `DiscoveryPath::ServerDiscover`, send `tools/list` with the same `_meta`.
  If the server returns `nextCursor`, the follow-up page requests need `_meta` alongside
  `cursor`. Note that the current client does not paginate on either path; see the side
  note in §1.4.1.
- No `notifications/initialized` is sent on the modern path. P0-09 already gets this right.

**(d) HTTP headers: missing on the modern path.**
- **Current:** `MCP-Protocol-Version` is sent only after negotiation, and `Mcp-Method` is
  never sent.
- **Required on the modern path:** every POST, *including the first `server/discover`*,
  carries `MCP-Protocol-Version: 2026-07-28` and `Mcp-Method: <method>`
  (`server/discover`, then `tools/list`). Both values must equal the body, or the server
  answers 400/`-32020`.
- **Legacy path:** unchanged (no `Mcp-Method`; `MCP-Protocol-Version` after `initialize`,
  as now).

**(e) The fallback trigger is inverted relative to the spec, and unreachable on HTTP.**
- **Ordering:**
  - P0-09 tries `initialize` first and falls back on `Io` or `-32601`.
  - The spec's dual-era algorithm is modern-first: `server/discover` is the probe, and
    `initialize` is the fallback on a non-modern error or a timeout.
  - On stdio, P0-09's order mostly works in practice, because a modern-only server rejects
    `initialize` with some error. But the code is "implementation-defined", so keying on
    `-32601` alone will miss servers that use `-32600` or `-32602`. The spec explicitly
    forbids keying fallback on one code in the other direction, and the same reasoning
    applies here.
- **HTTP is the real defect:**
  - A modern-only HTTP server answers a legacy `initialize` with **HTTP 400**. It answers an
    unknown method with **HTTP 404**.
  - `HttpTransport` builds its `ureq::Agent` overriding only `timeout_global`, leaving
    `http_status_as_error` at its ureq 3.3.0 default of `true` (verified in the vendored
    crate, `ureq-3.3.0/src/config.rs`). So every 4xx becomes `DiscoveryError::Transport` and
    the response body is discarded.
  - `is_initialize_unavailable` deliberately excludes `Transport`, so the HTTP fallback is
    **never taken** against a spec-compliant modern-only server.
  - P0-09's HTTP fallback test passes only because its fake server returns `-32601` with
    **HTTP 200** (`crates/discovery/tests/http_discovery.rs`), which a `2026-07-28` server
    must not do.
- **Required:**
  - Build the agent with `.http_status_as_error(false)`. This is **mandatory, not
    optional**: matching `ureq::Error::StatusCode` instead cannot work, because that variant
    carries only the status code — no response, no body (`ureq-3.3.0/src/error.rs:14`) — so
    it cannot satisfy this item's own requirement to read the body. Read and size-cap the
    body on 400/404. Four hazards that flag carries are in §1.4.2.
  - Classify a JSON-RPC error body as either:
    - *modern* (`-32020`/`-32021`/`-32022`, or a 404 carrying `-32601`), or
    - *non-modern* (anything else, including an empty body, `-32600`, or a non-JSON body).
  - Keep connection-level failures (DNS, refused, TLS, timeout) as non-fallback `Transport`
    errors. That preserves P0-09's review fix against doubling the cost on unresponsive
    hosts.
- **Recommended order,** for both transports, following the spec:
  1. Send `server/discover` with full `_meta` and headers first.
  2. On `DiscoverResult`, stay on the modern path.
  3. On `-32022`, pick from `data.supported`. If that list offers only legacy revisions,
     do the legacy `initialize` at one of them.
  4. On `-32020`/`-32021`, report a real discovery failure. It is a harness bug or a
     server-side requirement, and must not be papered over.
  5. On any other error, a non-modern 4xx body, or (stdio) a timeout, fall back to legacy
     `initialize` exactly as today.
- **Census cost of modern-first:** every legacy server pays one extra round trip. On stdio
  a *silent* legacy server costs a timeout before fallback. Use a short dedicated probe
  timeout rather than the 45 s watchdog. The spec says only "a reasonable timeout" and
  names no figure.

**(f) Provenance.**
- `DiscoveryPath` should be read as "era detected", not "path that happened to work". Under
  modern-first, dual-era servers will be recorded as `ServerDiscover`; under today's
  initialize-first they are recorded as `Initialize`. That changes census comparability
  between runs, so record which policy produced the record.
- Also persist the raw `server/discover` bytes and `supportedVersions`. The struct field
  `initialize_raw` already holds them, so only the documentation needs to make this clear.

**Not needed:** `Mcp-Name` (no `tools/call`, `resources/read` or `prompts/get` is ever sent),
`subscriptions/listen`, MRTR handling, and `io.modelcontextprotocol/logLevel`.

**Trust-model check (security relevance):**
- Nothing in `2026-07-28` requires the discovery client to call a tool or send an
  arbitrary method, so P0-01's structural guarantee is unaffected. The method set becomes
  `{server/discover, tools/list}` on the modern path and
  `{initialize, notifications/initialized, tools/list}` on the legacy path.
- `DiscoverResult.instructions` is server-controlled free text whose stated purpose is
  inclusion in an LLM system prompt. That is a prompt-injection vector, so it must be stored
  as evidence only and never fed to a model. The `destructive` classifier (ADR-006) must
  treat it exactly like tool descriptions — ADR-006 and Q-03 were amended on 2026-10-07 to
  name the field.
- **The field is not new, and the exposure is not pending.** `InitializeResult.instructions`
  predates this revision, and the *legacy* `initialize` transcripts in this document carry the
  identical imperative prose (Context7, A.2; DeepWiki, A.4). What `2026-07-28` changes is
  narrower: the field now rides `DiscoverResult`, a response every server **MUST** implement,
  and version negotiation no longer gates reaching it. **The exposure is therefore
  retroactive.** It was present in every Class A and Class B census sweep already run, and the
  `initialize_raw` bytes P0-10 persists hold it for every reachable server in the corpus — so
  the mitigation must not be sequenced behind the spec migration, which is exactly what
  calling the field "new" would cause.
- `serverInfo` is now explicitly self-reported and *"SHOULD NOT [be relied on] for security
  decisions"*.

#### 1.4.1 Side notes for the census

These are not part of the P0-09 fix, but the same spec reading surfaced them.

- `ListToolsResult` is `PaginatedResult` (`nextCursor`). The discovery client sends one
  `tools/list` and never follows `nextCursor`, so tools on later pages are silently omitted
  from P0-06/P0-07 counts. How many servers paginate is **UNVERIFIED** and was not
  measured. This applies to both eras (pagination predates `2026-07-28`).
- Two of the three dual-era servers probed (Cloudflare docs, Context7) answer a **legacy**
  `initialize` with `Content-Type: text/event-stream`, which `HttpTransport` rejects by
  design. Their *modern* `server/discover` responses were `application/json`. So for some
  servers the modern path is the only one this client can complete. The P0-07 failure
  category reported as "SSE-only" may therefore include servers that were reachable over a
  path the harness does not speak (**UNVERIFIED** — no per-server records were persisted;
  see P0-10).
- Minor change 3: servers **SHOULD** return `tools/list` in a deterministic order. P0-02's
  pin is deliberately order-sensitive. This should make spurious reorder-driven pin changes
  rarer on modern servers, with no change to the pin design.
- `tools/list` results now carry the required `resultType`, `ttlMs` and `cacheScope`. They
  are present in `tools_list_raw` but outside the per-tool pin fields, so pins are unaffected.

#### 1.4.2 Implementation hazards in the change set above

Established during the 2026-10-07 security-relevance review of this note. Each is a trap
in the fix itself, not in the spec.

- **`.http_status_as_error(false)` is agent-wide, so flipping it changes every request,
  `tools/list` included.** Today a 403 or 500 on `tools/list` short-circuits as
  `DiscoveryError::Transport("http status: NNN")`; afterwards the HTML error body reaches
  `decode_and_validate` and surfaces as `Protocol(…)`. That silently reclassifies
  HTTP-level failures into the `protocol` bucket and breaks comparability with the July
  split (435 `transport` against 312 `protocol`). Capture the status explicitly and keep the
  failure category keyed on it.
- **The era classifier must be a separate read-only function, and `decode_and_validate`'s
  id check must not be relaxed to accommodate it.** `decode_and_validate` requires
  `id.as_u64()`, and *both* real 4xx error bodies in Appendix A would be rejected before
  classification — DeepWiki substitutes the string id `"server-error"` (A.4), GitMCP returns
  `"id":null` (A.5). The id check is a deliberate anti-hostile-server measure, per its own
  doc comment; classify the body alongside it, never by loosening it.
- **Size-capping the body needs an explicit limit.** `ureq`'s 10 MiB cap applies to
  `read_to_vec()` specifically. `with_config().reader()` and `read_json()` are **unbounded**
  without an explicit `.limit()`, per ureq's own documentation.

### 1.5 `ToolAnnotations` — current schema, quoted verbatim

Source:
<https://github.com/modelcontextprotocol/modelcontextprotocol/blob/5f5440bb26a62e2cf3440b92da5a667efa03b267/schema/2026-07-28/schema.ts#L1900-L1954>
(tag `2026-07-28`). The block is identical at `schema/draft/schema.ts` lines 1900–1954 on
`main` @ `0a11bf68`.

```ts
/**
 * Additional properties describing a {@link Tool} to clients.
 *
 * NOTE: all properties in `ToolAnnotations` are **hints**.
 * They are not guaranteed to provide a faithful description of
 * tool behavior (including descriptive properties like `title`).
 *
 * Clients should never make tool use decisions based on `ToolAnnotations`
 * received from untrusted servers.
 *
 * @category `tools/list`
 */
export interface ToolAnnotations {
  /**
   * A human-readable title for the tool.
   */
  title?: string;

  /**
   * If true, the tool does not modify its environment.
   *
   * Default: false
   */
  readOnlyHint?: boolean;

  /**
   * If true, the tool may perform destructive updates to its environment.
   * If false, the tool performs only additive updates.
   *
   * (This property is meaningful only when `readOnlyHint == false`)
   *
   * Default: true
   */
  destructiveHint?: boolean;

  /**
   * If true, calling the tool repeatedly with the same arguments
   * will have no additional effect on its environment.
   *
   * (This property is meaningful only when `readOnlyHint == false`)
   *
   * Default: false
   */
  idempotentHint?: boolean;

  /**
   * If true, this tool may interact with an "open world" of external
   * entities. If false, the tool's domain of interaction is closed.
   * For example, the world of a web search tool is open, whereas that
   * of a memory tool is not.
   *
   * Default: true
   */
  openWorldHint?: boolean;
}
```

A diff against `schema/2025-11-25/schema.ts` lines 1168–1222 shows three doc-comment lines
changed. Each change only adds backticks or a `{@link}` around `Tool`/`ToolAnnotations`. The
interface body is byte-identical. design.md §1's table remains correct.

### 1.6 Live probes — is any public server `2026-07-28`-only?

No server speaking only `2026-07-28` was found among the five probed.

**Policy:** connect-level requests only. `server/discover` and `initialize` were the only
methods sent. No `tools/list`, no `tools/call`, no credentials. Every request carried
`User-Agent: mcp-conformance-research (O-01 spec-revision check)`. Each server got at most
three requests. The first run's requests were sent 2026-10-07 06:00–06:05 UTC; because that
run's transcripts were not preserved, every probe was re-sent under the identical policy at
2026-10-07 16:18 UTC, and **that** run is the one recorded verbatim in Appendix A. Where the
two runs differ, the table below reports the 16:18 UTC result and says so.

The **modern** probe was the official example request (§1.2) with clientInfo
`mcp-conformance-research/0.0.1`, sent with headers `MCP-Protocol-Version: 2026-07-28` and
`Mcp-Method: server/discover`.

The **legacy** probe was
`{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{...}}}`
with no MCP headers.

| Endpoint | Modern `server/discover` | Legacy `initialize` | Era |
|---|---|---|---|
| `https://docs.mcp.cloudflare.com/mcp` | 200 JSON: `supportedVersions:["2026-07-28"]`, `resultType:"complete"`, `ttlMs:0`, `cacheScope:"private"`, serverInfo `docs-ai-search 0.4.13` | 200 **SSE**: `protocolVersion:"2025-11-25"` | dual-era |
| `https://mcp.context7.com/mcp` | 200 JSON: `supportedVersions:["2026-07-28"]` (+ `instructions`) | 200 **SSE**: `protocolVersion:"2025-11-25"` | dual-era |
| `https://huggingface.co/mcp` | 200 JSON: `supportedVersions:["2026-07-28"]`, `capabilities.extensions:{"io.modelcontextprotocol/skills":…}` | **evidence not preserved** — HTTP 429 (edge rate limit) on the 2026-10-07 16:18 UTC re-probe; the 06:0x UTC run reported 200 JSON minting `mcp-session-id`, from transcripts that no longer exist | dual-era *(modern half confirmed 16:18 UTC; legacy half unpreserved)* |
| `https://mcp.deepwiki.com/mcp` | 400 JSON: `-32600` "Bad Request: Unsupported protocol version: 2026-07-28. Supported versions: 2024-11-05, 2025-03-26, 2025-06-18, 2025-11-25" | 200 SSE: `protocolVersion:"2025-11-25"` | legacy |
| `https://gitmcp.io/docs` | 400: `-32000` "Bad Request: Mcp-Session-Id header is required" | 200 SSE: `protocolVersion:"2025-03-26"` | legacy |

Two observations bear directly on §1.4:

- **DeepWiki** rejects the modern probe with `-32600`, not the modern `-32022`. Under the
  spec's HTTP algorithm that is a *non-modern* error, so the correct client action is
  fallback to `initialize`. This confirms the classifier must key on the modern error codes,
  not on HTTP 400 alone.
- **P0-09's exact payload** (top-level `protocolVersion/capabilities/clientInfo`, no `_meta`,
  no MCP headers) was then sent once each to Cloudflare docs and Hugging Face.
  - Cloudflare docs: HTTP 200, SSE, `{"code":-32601,"message":"Method not found"}`.
  - Hugging Face: HTTP 400 `{"code":-32600,"message":"Session ID required"}` — from the
    06:0x UTC run, whose transcripts are gone. The 16:18 UTC re-probe got HTTP 429
    (rate-limited) instead, so **this row's evidence is not preserved**; see Appendix A.3.
    The Cloudflare row above is preserved and carries the same point on its own.
  - Both servers dispatch on the presence of modern `_meta`/headers. Without them,
    `server/discover` is just an unknown legacy method.

**Transcripts: Appendix A.** The 06:00–06:05 UTC run's raw transcripts were kept only in
that session's scratchpad and are **gone**, which left this table with no preserved evidence
behind it. Every probe was therefore re-sent from scratch on 2026-10-07 16:18 UTC under the
same policy, and each request and response is recorded verbatim in Appendix A. Ten of the
twelve requests reproduce the observations above exactly; the two that do not are both
Hugging Face legacy-era probes, which were rate-limited (HTTP 429) and are marked unpreserved
in the table rather than re-asserted. I did not search for a modern-only server
systematically, for example by sweeping the registry, so whether any exist in the wild
remains **UNVERIFIED**.

### 1.7 SEP status and Tool Annotations IG activity

All SEP statuses come from `api.github.com/repos/modelcontextprotocol/modelcontextprotocol/{pulls,issues}/N`,
fetched 2026-10-06. None of these SEPs is merged. The repo's `seps/` directory contains none
of them: at `0a11bf68` it holds **43** numbered SEP `.md` files (46 entries including `.keep`,
`README.md` and `TEMPLATE.md`).

| SEP | Title | State | Notes |
|---|---|---|---|
| [#1913](https://github.com/modelcontextprotocol/modelcontextprotocol/pull/1913) | Trust and Sensitivity Annotations | **open**, draft label, `roadmap/security` | Last activity 2026-10-01. Sponsor @localden pinged by the SEP bot for inactivity on 2026-09-28. The IG's disposition doc proposes keeping it as an umbrella and moving the schema into extensions. |
| [#1984](https://github.com/modelcontextprotocol/modelcontextprotocol/pull/1984) | Comprehensive Tool Annotations for Governance/UX | **closed** 2026-09-23, unmerged | Author: "Closing this - will sync with tool annotations IG and reopen if necessary." |
| [#1862](https://github.com/modelcontextprotocol/modelcontextprotocol/pull/1862) | Tool Resolution (preflight) | **closed** 2026-09-02 by `dsp-ant`, unmerged, no closing comment | The IG's 2026-08-12 priorities doc still lists it as priority 2 ("prepare for review"). Whether it will be reopened is **UNVERIFIED**. |
| [#2417](https://github.com/modelcontextprotocol/modelcontextprotocol/pull/2417) | Model Preferences for Tools | **closed** 2026-09-22, unmerged | @localden: maintainers decided "every new SEP should be developed with the help of a Working Group"; closed so it can go through that path. |

New annotation- or trust-adjacent proposals since March 2026, found by GitHub search on the
spec repo:

- [SEP-2793](https://github.com/modelcontextprotocol/modelcontextprotocol/pull/2793) **Tool
  Risk Metadata** (open, 2026-05-26). Adds graded optional `ToolAnnotations` fields:
  `riskLevel`, `category`, `blastRadius`, `reversibility`, `sideEffects`,
  `approvalRecommendation`, `minTrustLevel`. It is purely additive and leaves the four
  existing hints untouched.
- [SEP-2809](https://github.com/modelcontextprotocol/modelcontextprotocol/pull/2809)
  **Attested Tool-Server Admission (ATSA)** (open; resubmission of #2777). An admission
  layer built on offline-signed server clearance assertions and per-server tool
  allow-lists. The companion paper is [arXiv:2605.24248](https://arxiv.org/abs/2605.24248).
- [SEP-3140](https://github.com/modelcontextprotocol/modelcontextprotocol/pull/3140)
  **Signed Capability Declarations & Trustworthy Trust Labels** (closed 2026-09-22). Proposed
  a content hash and version per declaration, a JWS-signed manifest, and `list_changed`
  semantics for rug-pull detection. It closely parallels this project's metadata pin, from
  the server side.
- [SEP-2668](https://github.com/modelcontextprotocol/modelcontextprotocol/pull/2668)
  Behavioral Trust Extension for MCP Registry (closed 2026-05-06). Maintainer reason: "it's
  AI-generated without disclosure and engagement is being automated". It claimed an
  observatory tracking uptime, latency and error rates for 4,584 servers. That is
  operational telemetry, not annotation conformance, and the claim is **UNVERIFIED**.
- Closed issues #2745 (advisory policy hints), #3024 (capability trust tiers) and #3025
  (side-effect classification) all raise "annotations are only hints" as the motivating
  gap. None became a SEP.
- [PR #2924](https://github.com/modelcontextprotocol/modelcontextprotocol/pull/2924)
  (closed 2026-06-16) proposed a Security-IG rug-pull/tool-definition-drift test corpus.
  Its "capability-surface" cases (annotation escalation `readOnly → destructive` after
  approval) are exactly what P0-02's pin detects, so it is a candidate external test set
  for the pin. Where the corpus ended up after the PR closed is **UNVERIFIED**.

**IG activity:**

- Charter: `docs/community/interest-groups/tool-annotations.mdx`, last changed 2026-08-06
  (added a member).
- Met on 2026-04-23 and 2026-05-28 (per [PR #2818](https://github.com/modelcontextprotocol/modelcontextprotocol/pull/2818)).
  The meeting cadence is still "TBD" in the charter.
- The incubation repo
  [`modelcontextprotocol/experimental-ext-tool-annotations`](https://github.com/modelcontextprotocol/experimental-ext-tool-annotations)
  is active: latest commit 2026-08-12, `docs/next-spec-iteration-priorities.md`.
  - It drafts two **experimental extensions**, not core-spec changes:
    `io.modelcontextprotocol/trust-annotations` (`sensitive`/`untrusted` labels on *result*
    `_meta`, plus an `evidenceRef` pointer) and `io.modelcontextprotocol/action-metadata`
    (input/return metadata on `ToolAnnotations`; successor to the closed SEP-2061).
  - Per `docs/sep-disposition.md`, the IG "aligned on 2026-05-28 … to pursue this work as an
    experimental extension first". The reference cited for this is GitHub Discussion #2820,
    which I did not open (**UNVERIFIED** beyond the repo doc).
- The charter's open question "Should runtime annotations … be added to the protocol?"
  remains unresolved.
- The 2026-08-22 roadmap post (`blog/content/posts/2026-08-22-mcp-roadmap.md`) does not
  mention annotations.

**Net for this project:** the four hints are unchanged and nothing is close to changing
them. The active direction is additive, out-of-band trust evidence (extensions with
`evidenceRef`). A behavioural conformance record bound to a metadata pin is plausibly
exactly the kind of evidence such a slot could reference. That is an observation, not
something the IG has said.

---

## 2. O-02 — Prior-art re-survey (since ~March 2026)

Search method:
- the arXiv API (`export.arxiv.org/api/query`) for MCP together with each of: annotations,
  readOnlyHint, census, conformance, sandbox, measurement;
- two web searches to seed candidates;
- reference-chasing through the bibliographies of the papers found.

Every row below was checked against the arXiv abstract page or the full HTML text. Papers
from before March 2026 that architecture.md §0 already treats as orthogonal (MCPDiFF,
arXiv:2602.03580, 2026-02-03) are listed only where a follow-up appeared.

### 2.1 Annotation-coverage censuses — directly overlaps P0-05/06/07

**[A] Trofimov & Novikov, "When Tool Calls Succeed but Workflows Fail: Anomalies at the
Agent–Tool Boundary", [arXiv:2609.15397](https://arxiv.org/abs/2609.15397) (v1, 2026-09-14),
§5 "What the Boundary Declares Today".** Artifact:
[github.com/flame-stream/mcp-annotation-census](https://github.com/flame-stream/mcp-annotation-census)
(created 2026-07-30).

- **What and how:**
  - Full snapshot of the official registry on 2026-07-27: 59,625 entries, 18,688 distinct
    servers at latest version.
  - 9,454 servers with no remote endpoint were excluded.
  - Anonymous `tools/list` was sent to all 9,234 remote targets. 4,838 returned ≥1 tool,
    4,318 failed to connect, 74 timed out — note those three sum to 9,230, four short of the
    9,234 probed, a gap in the paper's own breakdown rather than in this reading of it. Worth
    raising before the breakdown is quoted as a comparison baseline.
  - Result: **98,291 tools** (median 11 per server). No tool was called.
  - The census records all four hints and keeps explicit `false` distinct from an omitted
    field.
  - The paper says no responding target rejected discovery with 401/403, but connection
    failures were not classified.
- **Numbers (tool-weighted):**
  - 74.0% of tools serialize ≥1 annotation field; 61.7% serialize all four.
  - **26.0% carry no annotation at all.**
  - The commonest signature (readOnly, non-destructive, idempotent, open-world) covers
    39.9% of tools. 66 of 81 possible signatures appear.
  - Per server, **the median dominant signature covers 79.4% of a server's tools** — a
    server-level concentration measure, and the one per-server distribution the paper does
    report.
  - `destructiveHint` is set on 65.8% of tools, but is *applicable* (i.e.
    `readOnlyHint != true`) on only 12.9%. Only 3.1% assert a destructive operation.
  - The authors caution that emitted values "may originate in SDK defaults or server
    templates rather than deliberate declaration".
- **Overlap:**
  - Same population, same instrument class and same no-execution rule as our Stage 1
    (P0-07 Class B), at about 19× our reachable-server count (4,838 vs 250) and about 31× our
    tool count (98,291 vs 3,183). The snapshot was taken the same day as ours (P0-08 ran
    2026-07-26; P0-07 ran 2026-07-26/27).
  - **Its absent rate disagrees with ours:** 26.0% vs our 48.6% of tools with no
    `annotations` object.
  - Its reachability also disagrees: 52% of targets answered, vs 25% for our 1,000-server
    hash-sampled run, where 401 was the dominant failure.
  - The causes are **UNVERIFIED**. Candidate explanations, none tested:
    - our sample is 1,000 of about 8,300, not the full population;
    - our transport rejects SSE responses;
    - definitional differences ("no annotation at all" vs "no `annotations` object");
    - tool-weighting by different large servers.
- No Class A coverage, no metadata pin, and no per-server **annotation-coverage**
  distribution. It does report one per-server distribution — the 79.4% median
  dominant-signature concentration above — but not the share of each server's tools that
  carry annotations at all, which is the server-weighted view P0-10 exists to make
  producible.

**[B] Haseeb Mohammed Afsar, "What a Random Draw from the MCP Registry Contains, and What Tool-Use
Benchmarks Contain Instead", [arXiv:2609.10962](https://arxiv.org/abs/2609.10962) (v1,
2026-09-10).** Tool `mcp-probe` at
[github.com/itguruhaseeb/mcp-probe](https://github.com/itguruhaseeb/mcp-probe); data at Zenodo
DOI 10.5281/zenodo.21347997.

- **What and how:**
  - Registry census tier (no third-party code run): 16,548 servers on 2026-07-14 and 24,135
    on 2026-08-22.
  - A seeded probability draw (seed 20260819; frame pinned by SHA-256) of **400 npm/stdio
    servers** from 7,258 candidates. Each was launched once via `npx`, with no repair, no
    credentials and no retry. The probe ran `initialize` + `tools/list` and validated the
    JSON Schema.
  - "no side-effecting tool is invoked". The paper does not describe any sandbox or
    containment for running the drawn packages; whether one was used is **UNVERIFIED**.
- **Numbers:**
  - 48.8% of draws completed `initialize` (vs 66.7% on a hand-curated frame). 37.5% never
    started; 13.3% needed credentials.
  - 195 ran, advertising 2,766 tools. **58.8% of those tools carry no annotations** (curated
    frame: 41.5%).
  - Server level: of 194 servers with ≥1 tool, **72 annotate every tool and 122 annotate
    none**, with zero partial servers (95% upper bound on partial prevalence: 1.53%).
  - Negotiated versions: `2025-06-18` on 192 of 195.
  - Deployment-model split moved between the two snapshots:
    - package-only: 50.4% → 43.6%;
    - remote-only: 42.6% → **49.7%** ("Remote-only overtook package-only in this window");
    - both: 5.1% (unchanged).
- **Overlap:**
  - The closest analogue to our Stage 2 (Class A), which ran 57 of 100 servers in Docker
    and found 41.7% of tools with no `annotations` object. Afsar's random-draw rate (58.8%)
    is higher. His curated-frame rate (41.5%) matches ours, which suggests our hash-based
    Class A sample may skew toward servers that start and annotate (**UNVERIFIED**; our
    sample is small).
  - His server-level bimodality result is exactly the server-weighted view HANDOFF §2 says
    our stored results cannot produce (→ P0-10).
  - His deployment-model trend bears directly on P0-08 (2026-07-26: Class A 51.0% /
    Class B 44.4%; we count "both" as A). Our ratio is consistent with his 2026-07-14
    snapshot, so the ratio has probably moved since our run. Re-measure before publishing.

**Related registry-drift work** (overlaps the metadata pin, not coverage):

- **Bharti, "Registry Descriptions Go Stale Unevenly",
  [arXiv:2608.00997](https://arxiv.org/abs/2608.00997) (2026-08-02).** 120 registry
  observations over 88.6 days covering 19,099 servers. 8.6% of servers ever rewrite a
  registry description. It recommends "content-binding — revalidate the moment a
  description's hash moves". This measures registry `server.json` text, not `tools/list`.
- **Kraishan, "Same Name, Different Server: A Security Census of Silent Drift in the MCP
  Ecosystem", [arXiv:2609.14119](https://arxiv.org/abs/2609.14119) (2026-09-12).** 21,643
  servers / 72,606 version records (August 2026), with source fetched for 14,353.
  - "51.1% of multi-version servers changed what they advertise between versions, 40.6% did
    so silently."
  - 4.2% redirected their remote endpoint to a different host.
  - It recommends client-side pinning.
  - Both papers independently support architecture.md §0's case for the metadata pin. Neither
    pins `tools/list` bytes per tool.

### 2.2 Annotation-conformance audits (declared vs observed behaviour)

**None found.** No paper, SEP or IG artifact found in this survey executes tools and compares
observed behaviour against `readOnlyHint` / `destructiveHint` / `idempotentHint` /
`openWorldHint`. The nearest neighbours:

- **Description-vs-code (static, plus an LLM judge), already orthogonal per architecture.md
  §0.** Shi et al., "Description-Code Inconsistency in Real-world MCP Servers" (DCIChecker),
  [arXiv:2606.04769](https://arxiv.org/abs/2606.04769) (2026-06-03): 19,200
  description-code pairs from 2,214 servers; 9.93% inconsistent. MCPDiFF
  ([arXiv:2602.03580](https://arxiv.org/abs/2602.03580), Feb 2026): 10,240 servers, about 13%
  substantial mismatch. Both compare *descriptions* with code; neither checks annotations
  or executes anything.
- **Annotation *enforcement* (not verification).** A comment on SEP-1913 by @cgrtml
  (2026-10-01,
  [link](https://github.com/modelcontextprotocol/modelcontextprotocol/pull/1913#issuecomment-5937282079))
  reports replaying AgentDojo traces (609 user×injection pairs) through an annotation-enforcing
  gate (repo `github.com/cgrtml/reasongate`, not inspected). It *assumes* annotations are
  truthful and measures gate effectiveness. It is a GitHub comment, not a peer-reviewed
  publication; numbers **UNVERIFIED**.
- Trofimov & Novikov (§2.1 [A]) analyse what annotations *can express*, not whether they are
  true.

### 2.3 Sandboxed / dynamic behavioural audits of MCP servers

| Item | Date | What / how | Corpus | Overlap with this project |
|---|---|---|---|---|
| Chen et al., "Rethinking MCP Security: A Large-Scale Study of Runtime MCP Servers and Security Scanner Reliability" (MCPZoo), [arXiv:2607.11086](https://arxiv.org/abs/2607.11086) | 2026-07-13 | A multi-agent LLM pipeline generates and repairs Dockerfiles until servers run, with protocol-level verification; then evaluates *security-scanner* reliability. Quoted from the primary abstract (v1, fetched 2026-10-07), replacing an earlier paraphrase taken via [B]'s summary of it: existing scanners "report that 96.89% of servers are risky", but "manual validation shows that less than 50% of sampled alerts are true positives, and scanner outputs exhibit clear inconsistency across scanners". Authors: Pei Chen, Baichao An, Mengying Wu, Binwang Wan, Geng Hong, Jinsong Chen, Xudong Pan, Jiarun Dai, Min Yang. | 64,611 unique servers; 37,288 support dynamic analysis | Containerised Class A execution at scale, but the goal is scanner validation, not annotations. Repair-until-running changes the population (Afsar's critique). It is a possible source of runnable Class A fixtures; dataset availability and licence are **UNVERIFIED**. |
| "Auditing MCP Servers for Over-Privileged Tool Capabilities" (mcp-sec-audit), [arXiv:2603.21641](https://arxiv.org/abs/2603.21641) | 2026-03-23 | Static regex pipeline plus a dynamic pipeline: tool runs in a Docker sandbox; a protocol fuzzer injects malicious payloads; an eBPF monitor captures syscalls, file I/O and network into CEF logs. | MCPTox (45 servers, 491 samples), a 9-implementation deliberately-vulnerable-server lab, and one synthesised malicious tool; no ecosystem corpus. **Corpus confirmed from the paper's full text 2026-10-07** — its abstract names only the Docker+eBPF mechanism, so this was previously cited on trust. | The closest *mechanism* to ours (kernel-level observation of an executed tool). It detects capabilities and risk scores, does not compare against annotations, has no noise floor or idempotency arms, and uses Docker rather than hand-rolled namespaces. Tool paper, not an audit. |
| Padilla, "Exposed by Design: A Dynamic Security Assessment of Internet-Facing MCP Servers at Scale" (Corvus), [arXiv:2608.00150](https://arxiv.org/abs/2608.00150) | 2026-07-31 | Passive discovery from 11 sources, then 34 active test modules over 10 vulnerability classes. Testing is proof-of-concept level — "the minimum interaction necessary to confirm vulnerability existence", no data exfiltration, "we do not carry exploitation to post-exploitation stages" (full text §VII, fetched 2026-10-07). An earlier draft of this row paraphrased that as "does not modify server state", which is **stronger than the paper claims** and has been withdrawn. Disclosure: "a 90-day embargo … from the date of maintainer notification", findings filed as GitHub Security Advisories. | 640 confirmed servers, 414 dynamically audited; 68 vulnerabilities, filed as 68 **GHSAs, not CVEs** | Remote (Class B) and vulnerability-oriented. No annotations. Its 90-day embargo, GHSA filing route and 41.6% three-day churn are relevant to P5-03 and to re-run cadence. |
| Zhou et al., "A First Measurement Study on Authentication Security in Real-World Remote MCP Servers", [arXiv:2605.22333](https://arxiv.org/abs/2605.22333) | 2026-05-21 | Identifies live remote servers; semi-automated OAuth flaw detection. | 7,973 live remote servers; 40.55% expose tools unauthenticated | Cross-check for our Class B reachability and 401 rates. No annotations. |

Not counted (they do not audit servers): DynamicMCPBench
([arXiv:2607.20531](https://arxiv.org/abs/2607.20531), an agent benchmark over live servers),
ATSA ([arXiv:2605.24248](https://arxiv.org/abs/2605.24248), an admission protocol), and Li &
Gao ([arXiv:2510.16558](https://arxiv.org/abs/2510.16558), 2025, pre-window static
classification of registry entries).

### 2.4 Conclusion per design.md §11

**Census (P0-05/06/07): reframe as replication and extension. A "first measurement" claim is
no longer available.**

[A] already published a full-population Class B annotation-coverage census from a snapshot
taken the same day as ours. [B] published a seeded random-draw Class A-style census with
server-level bimodality. The publishable framing for P0-07 is now:

1. **Replication with a disagreement to explain.**
   - Class B: our 48.6% no-annotation rate vs [A]'s 26.0%.
   - Class A: our 41.7% vs [B]'s 58.8% random draw.
   - The disagreements are the interesting part. They must be explained before publishing
     (sampling, SSE handling, definition of "absent", tool vs server weighting), or the
     census is not credible next to these two.
2. **Extension along axes neither paper covers:**
   - both containability classes measured by one instrument under one taxonomy;
   - the explicit / **defaulted** / absent three-way split (P0-05). [A] distinguishes
     explicit-false from omitted, but neither paper separates "annotations object present,
     key missing" from "no object";
   - **containerised** Class A execution covering pypi and oci as well as npm. [B] covers
     npm only and says nothing about containment;
   - the metadata pin, with stability measured (P0-06);
   - an explicit `execution_provenance` per record.
3. **Server-weighted results are now table stakes.** [B]'s 72/122 all-or-nothing split is the
   server-level answer to open question 1. P0-10 (persist raw `tools/list` bytes per server)
   is a prerequisite for comparability, not polish.
4. **P0-08's class ratio is stale.** [B]'s data says remote-only overtook package-only between
   2026-07-14 and 2026-08-22. Re-measure before citing 51.0%/44.4%, and note that
   architecture.md §2's design note ("if most public servers are remote-only…") may now
   apply after all.
5. The census re-run is required anyway because of the `2026-07-28` changes (§1.4). It should
   also fix pagination (§1.4.1) before producing numbers that will be compared against [A]
   and [B].

**Conformance plan (Phases 1–5): no reframing needed. Nothing found verifies annotations
against behaviour, at any scale or with any containment.**

The differentiators in architecture.md hold: kernel-boundary evidence, the multi-arm
idempotency protocol with a measured noise floor, the integrity gate, and `unverifiable` as a
verdict. Specific adjustments worth carrying into the plan:

- **`destructiveHint` / Track Q:** [A] finds `destructiveHint` *applicable* on only 12.9% of
  tools and *asserted* destructive on only 3.1%. Q-01/Q-04 should report the applicable
  denominator, not all tools. Otherwise the agreement statistic is dominated by
  `readOnlyHint: true` tools where the hint is meaningless by spec.
- **SDK-default emission:** [A] notes that values may come from SDK defaults or templates. A
  `holds`/`violated` rate per *declared* value should be stratified by whether the value is
  plausibly template-emitted. A cheap proxy is the signature concentration per server — which
  is [A]'s own measure, not a new one here: it reports a median dominant signature covering
  79.4% of a server's tools (§2.1 [A]). This bears on how violation rates are interpreted,
  not on the verdict engine.
- **Class A population:** MCPZoo (37,288 runnable servers) is the largest existing pool of
  servers known to start in containers, and could seed the P5 audit corpus (availability
  **UNVERIFIED**). Its repair-until-running bias must be disclosed if used, per [B]'s
  critique. [B]'s 37.5% never-start rate also predicts a large `unverifiable`/no-verdict
  fraction for P5-04, which ADR-004 already anticipates.
- **Mechanism prior art to cite:** mcp-sec-audit is a Docker+eBPF precedent for kernel-level
  observation of MCP tools. The paper should say how this harness differs: overlayfs
  changeset as the evidence object, hand-rolled namespaces (ADR-007), annotation-specific
  protocols, and replayable pure derivation (ADR-005). This is a citation obligation, not a
  design change.
- **Disclosure (P5-03):** two usable precedents, but not the same one, and an earlier draft
  of this bullet collapsed them wrongly. **Corvus** applies "a 90-day embargo … from the date
  of maintainer notification" and files **GitHub Security Advisories — 68 of them, not CVEs**
  (confirmed in its full text, fetched 2026-10-07). **Zhou et al.** obtained **9 CVE IDs**
  through responsible disclosure (confirmed in its abstract); its embargo window is not stated
  there and is **UNVERIFIED**. Both inform the embargo state machine; neither supports a
  single "both ran 90-day disclosure and obtained CVEs" claim.

**O-02 exit status for P0-07:** this survey satisfies the "re-run before P0-07" requirement.
P0-07 is publishable only under the replication/extension framing above. The next re-survey
is due before P2-10.

---

## Sources

**Spec repository** (`github.com/modelcontextprotocol/modelcontextprotocol`; cloned 2026-10-06,
`main` @ `0a11bf68c7ec4473526ec15589f592afcd12d1e8`; tag `2026-07-28` @ `5f5440bb26a62e2cf3440b92da5a667efa03b267`)

- `schema/2026-07-28/schema.ts`: `RequestMetaObject` (L63–111), error codes (L434–530),
  `DiscoverRequest`/`DiscoverResult` (L653–709), `ToolAnnotations` (L1900–1954)
- `schema/2025-11-25/schema.ts` L1168–1222; `schema/draft/schema.ts` L1900–1954
- `schema/2026-07-28/examples/{DiscoverRequest,DiscoverResult,DiscoverResultResponse,UnsupportedProtocolVersionError,HeaderMismatchError,ListToolsRequest}/`
- `docs/specification/2026-07-28/{changelog.mdx, server/discover.mdx, basic/versioning.mdx, basic/transports/stdio.mdx, basic/transports/streamable-http.mdx}`
- `docs/community/interest-groups/tool-annotations.mdx`
- Blog: `blog/content/posts/2026-07-28-spec-ga/index.md`,
  `2026-03-16-tool-annotations.md`, `2026-05-21-mcp-2026-07-28-rc.md`, `2026-08-22-mcp-roadmap.md`

**GitHub** (REST API, 2026-10-06)

- PRs/issues #1913, #1984, #1862, #2417, #2793, #2809, #3140, #2668, #2745, #3024, #3025,
  #2924, #2818, #2487 in `modelcontextprotocol/modelcontextprotocol`
- `modelcontextprotocol/experimental-ext-tool-annotations`: README,
  `docs/next-spec-iteration-priorities.md`, `docs/sep-disposition.md`, `docs/related-work.md`,
  commit log
- `flame-stream/mcp-annotation-census` (repo metadata only)

**arXiv**

- 2609.15397 (Trofimov & Novikov), 2609.10962 (Mohammed Afsar), 2608.00997 (Bharti),
  2609.14119 (Kraishan), 2606.04769 (Shi et al., DCIChecker), 2602.03580 (MCPDiFF),
  2607.11086 (Chen et al., MCPZoo), 2603.21641 (mcp-sec-audit), 2608.00150 (Padilla, Corvus),
  2605.22333 (Zhou et al.), 2605.24248 (ATSA), 2607.20531 (DynamicMCPBench), 2510.16558 (Li & Gao)

**This repository**

- `crates/discovery/src/client.rs`, `crates/discovery/src/transport.rs`,
  `crates/discovery/tests/http_discovery.rs`; vendored `ureq-3.3.0/src/config.rs`

**Live probes:** five endpoints listed in §1.6 — 2026-10-07 06:00–06:05 UTC (transcripts
lost) and re-probed 2026-10-07 16:18 UTC (transcripts preserved verbatim in Appendix A).

**Added 2026-10-07, primary sources fetched to close second-hand citations**

- `schema/2026-07-28/schema.ts` re-read at tag `5f5440bb` via
  `raw.githubusercontent.com`, confirming independently: `LATEST_PROTOCOL_VERSION`,
  the `DiscoverRequest`/`DiscoverResult`/`CacheableResult`/`Result` declarations (and that
  `Result` carries an index signature `[key: string]: unknown`, which is what makes Hugging
  Face's extra `digest` field legal), `ToolAnnotations` with all four defaults, and the
  absence of `InitializeRequest`/`InitializedNotification`.
- arXiv:2607.11086 abstract (MCPZoo) fetched directly, so §2.3's scanner-reliability figure
  is no longer quoted via [B].
- `ureq-3.3.0/src/request.rs:86–95` and `http-1.4.2/src/header/value.rs:557–559`, closing
  §1.4(b)'s header-validation UNVERIFIED mark.
- arXiv:2608.00150 (Corvus) — abstract **and** HTML full text fetched, closing §2.3's and
  §2.4's disclosure claims: the 90-day embargo is confirmed; the findings are GHSAs, not CVEs;
  and the "does not modify server state" paraphrase is not what the paper says.
- arXiv:2605.22333 (Zhou et al.) abstract — 9 CVE IDs confirmed, no disclosure window stated.
- arXiv:2603.21641 (mcp-sec-audit) abstract **and** HTML full text — corpus composition
  (MCPTox 45 servers / 491 samples, a 9-implementation vulnerable lab, one synthesised tool)
  confirmed rather than taken on trust.

---

## Appendix A — Live-probe transcripts, verbatim

**Why this appendix exists.** §1.6's original run (2026-10-07 06:00–06:05 UTC) kept its raw
transcripts only in a session scratchpad, which no longer exists — so the §1.6 table had no
preserved evidence standing behind it. The probes were therefore re-run from scratch on
**2026-10-07 16:18 UTC** under the same policy, and everything sent and everything received
is recorded below. Ten of the twelve requests reproduce the original observations exactly.
The two that do not are both Hugging Face *legacy-era* probes, which hit an edge rate limit
(HTTP 429); §1.6's table has been corrected accordingly rather than left asserting a result
no longer in evidence.

Captured with `curl 8.5.0` (OpenSSL 3.0.13) from WSL2 / Ubuntu 24.04, `--max-time 15`, no
proxy, no cookies, no redirects followed.

**Redactions (2026-10-07), and what was deliberately left alone.** Three header values below
are replaced by a bracketed `<redacted: …>` placeholder. Every redaction is in a *response
header* block; **no recorded `sha256` covers a header block** — each digest is over the
response body as received — so no digest in this appendix is invalidated by them, and none
should be expected to mismatch.

- A.5, GitMCP legacy probe: `mcp-session-id`, a 64-hex session identifier GitMCP minted for
  this anonymous, unauthenticated probe. Near-certainly long expired, but a session token in a
  public repository is a bad precedent whatever its state.
- A.3, Hugging Face modern probe: `x-proxied-host` (an RFC1918 internal address) and
  `x-proxied-replica` (an internal replica name), leaked by that server's own response. Not
  this project's secret, and republishing it has no research value.

Left unredacted as a recorded decision, not an oversight: the `cf-ray`, `x-amz-cf-id` and
`x-amz-cf-pop` CDN point-of-presence headers. They disclose only which edge node served a
request from a coffee-shop-grade network location; the marginal disclosure was judged small
against the value of transcripts that are complete as received.

**Policy actually enforced**, unchanged from §1.6 and from this project's own trust model:
connect-level requests only — `server/discover` and `initialize`, nothing else. No
`tools/list`. No `tools/call`. No credentials, no `Authorization` header, no API key, no
session reuse. At most **three requests per host**; five hosts, twelve requests total. Every
request carried `User-Agent: mcp-conformance-research (O-01 spec-revision check)`.

**How these responses are treated.** Every byte below is *data*. Three of the five servers
return `instructions` prose written in the imperative at a language model ("Use this server
to fetch current documentation …", "Direct the User to set their HF_TOKEN …", "only use when
explicitly requested by the user"). None of it was followed, and none of it is instruction to
a reader of this document. It is recorded because it is evidence — and because it is the
prompt-injection surface §1.4's trust-model note flags, now observed to arrive on the very
first connect-level request, before a single tool has been listed, let alone called.

### A.0 The three request shapes used

All three were sent to a `/mcp`-style endpoint by HTTP POST. Nothing else was sent to any
host.

1. **Modern probe** — the official example request from
   `schema/2026-07-28/examples/DiscoverRequest/server-discover-request.json` (§1.2), with this
   study's `clientInfo`, plus the two headers `2026-07-28` requires on every POST
   (`MCP-Protocol-Version`, `Mcp-Method`).
2. **Legacy probe** — a `2025-11-25` `initialize`, with no MCP headers at all.
3. **P0-09's exact payload** — `method: "server/discover"` carrying `initialize`-shaped
   top-level params (`protocolVersion`/`capabilities`/`clientInfo`), no `_meta` and no MCP
   headers, i.e. precisely what `crates/discovery/src/client.rs` sends today on its fallback
   path. Sent to two hosts only, both of which answer a correctly-shaped `server/discover`.

### A.1 `https://docs.mcp.cloudflare.com/mcp`

#### modern probe (`server/discover`, correct `2026-07-28` shape)

`POST https://docs.mcp.cloudflare.com/mcp` — sent 2026-10-07T16:18:04Z, completed 2026-10-07T16:18:04Z.

Request headers:

```http
Content-Type: application/json
Accept: application/json, text/event-stream
MCP-Protocol-Version: 2026-07-28
Mcp-Method: server/discover
User-Agent: mcp-conformance-research (O-01 spec-revision check)
```

Request body (verbatim, as one line):

```json
{"jsonrpc":"2.0","id":"discover-1","method":"server/discover","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientInfo":{"name":"mcp-conformance-research","version":"0.0.1"},"io.modelcontextprotocol/clientCapabilities":{}}}}
```

Response: **HTTP 200**, `curl` exit 0, 0.068816 s. Headers verbatim:

```http
HTTP/2 200 
date: Wed, 07 Oct 2026 16:18:04 GMT
content-type: application/json
content-length: 307
access-control-allow-origin: *
access-control-allow-headers: Content-Type, Accept, Authorization, MCP-Protocol-Version, Mcp-Method, Mcp-Name, cf-account-id
access-control-allow-methods: POST, OPTIONS
access-control-expose-headers: MCP-Protocol-Version
access-control-max-age: 86400
server: cloudflare
cf-ray: a46e383aec8bdb4e-YVR
```

Response body (307 bytes, sha256 `cf4928ed9f750e72e02b6609f7f49ec68947c9990a78c8f8cbf9c8a5f74add25`):

```
{"result":{"supportedVersions":["2026-07-28"],"capabilities":{"tools":{"listChanged":true},"prompts":{"listChanged":true}},"resultType":"complete","ttlMs":0,"cacheScope":"private","_meta":{"io.modelcontextprotocol/serverInfo":{"name":"docs-ai-search","version":"0.4.13"}}},"jsonrpc":"2.0","id":"discover-1"}
```

#### legacy probe (`initialize`, no MCP headers)

`POST https://docs.mcp.cloudflare.com/mcp` — sent 2026-10-07T16:18:04Z, completed 2026-10-07T16:18:04Z.

Request headers:

```http
Content-Type: application/json
Accept: application/json, text/event-stream
User-Agent: mcp-conformance-research (O-01 spec-revision check)
```

Request body (verbatim, as one line):

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"mcp-conformance-research","version":"0.0.1"}}}
```

Response: **HTTP 200**, `curl` exit 0, 0.069852 s. Headers verbatim:

```http
HTTP/2 200 
date: Wed, 07 Oct 2026 16:18:04 GMT
content-type: text/event-stream
access-control-allow-origin: *
cache-control: no-cache, no-transform
access-control-allow-headers: Content-Type, Accept, Authorization, MCP-Protocol-Version, Mcp-Method, Mcp-Name, cf-account-id
access-control-allow-methods: POST, OPTIONS
access-control-expose-headers: MCP-Protocol-Version
access-control-max-age: 86400
server: cloudflare
cf-ray: a46e383b99e3703b-SEA
```

Response body (224 bytes, sha256 `0e289d0dfa5dbfd34698d645b815906e94de76e8995960b6726670e53be0139b`):

```
event: message
data: {"result":{"protocolVersion":"2025-11-25","capabilities":{"tools":{"listChanged":true},"prompts":{"listChanged":true}},"serverInfo":{"name":"docs-ai-search","version":"0.4.13"}},"jsonrpc":"2.0","id":1}
```

#### P0-09's exact payload (`server/discover`, legacy-shaped params, no `_meta`, no MCP headers)

`POST https://docs.mcp.cloudflare.com/mcp` — sent 2026-10-07T16:18:04Z, completed 2026-10-07T16:18:04Z.

Request headers:

```http
Content-Type: application/json
Accept: application/json, text/event-stream
User-Agent: mcp-conformance-research (O-01 spec-revision check)
```

Request body (verbatim, as one line):

```json
{"jsonrpc":"2.0","id":1,"method":"server/discover","params":{"protocolVersion":"2026-07-28","capabilities":{},"clientInfo":{"name":"mcp-conformance-research","version":"0.0.1"}}}
```

Response: **HTTP 200**, `curl` exit 0, 0.050305 s. Headers verbatim:

```http
HTTP/2 200 
date: Wed, 07 Oct 2026 16:18:04 GMT
content-type: text/event-stream
access-control-allow-origin: *
cache-control: no-cache, no-transform
access-control-allow-headers: Content-Type, Accept, Authorization, MCP-Protocol-Version, Mcp-Method, Mcp-Name, cf-account-id
access-control-allow-methods: POST, OPTIONS
access-control-expose-headers: MCP-Protocol-Version
access-control-max-age: 86400
server: cloudflare
cf-ray: a46e383c482e18fa-YVR
```

Response body (100 bytes, sha256 `a1a3d137d285bef304039bd573475671f7af86ce29d9ed5f7a97d06c3e9af3ea`):

```
event: message
data: {"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"Method not found"}}
```

Reproduces §1.6 and is the load-bearing evidence for §1.4(a): the same host that answers
a correctly-shaped `server/discover` with a `DiscoverResult` (first probe above) answers
P0-09's extrapolated payload with `-32601 Method not found`. Dispatch is on the presence
of modern `_meta`/headers; without them the method name alone buys nothing.

### A.2 `https://mcp.context7.com/mcp`

#### modern probe

`POST https://mcp.context7.com/mcp` — sent 2026-10-07T16:18:04Z, completed 2026-10-07T16:18:05Z.

Request headers:

```http
Content-Type: application/json
Accept: application/json, text/event-stream
MCP-Protocol-Version: 2026-07-28
Mcp-Method: server/discover
User-Agent: mcp-conformance-research (O-01 spec-revision check)
```

Request body (verbatim, as one line):

```json
{"jsonrpc":"2.0","id":"discover-1","method":"server/discover","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientInfo":{"name":"mcp-conformance-research","version":"0.0.1"},"io.modelcontextprotocol/clientCapabilities":{}}}}
```

Response: **HTTP 200**, `curl` exit 0, 0.303487 s. Headers verbatim:

```http
HTTP/1.1 200 OK
X-Powered-By: Express
Access-Control-Allow-Origin: *
Access-Control-Allow-Methods: GET,POST,OPTIONS,DELETE
Access-Control-Allow-Headers: Content-Type, MCP-Session-Id, MCP-Protocol-Version, Mcp-Method, Mcp-Name, X-Context7-API-Key, Context7-API-Key, X-API-Key, Authorization, If-None-Match
Access-Control-Expose-Headers: WWW-Authenticate, MCP-Session-Id
content-type: application/json
Date: Wed, 07 Oct 2026 16:18:05 GMT
strict-transport-security: max-age=63072000; includeSubDomains
Transfer-Encoding: chunked
```

**The response body below contains a server-authored `instructions` string: imperative
prose addressed at a language model. Recorded as evidence; not instruction to anyone.**

Response body (1241 bytes, sha256 `b50a5bdd240b19619fd32548f23360997b3ed980f9b5fa3f2f2da3f61eceaffb`):

```
{"result":{"supportedVersions":["2026-07-28"],"capabilities":{"tools":{"listChanged":false},"prompts":{"listChanged":false},"resources":{"listChanged":false,"subscribe":false}},"instructions":"Use this server to fetch current documentation whenever the user asks about a library, framework, SDK, API, CLI tool, or cloud service — even well-known ones like React, Next.js, Prisma, Express, Tailwind, Django, or Spring Boot. This includes API syntax, configuration, version migration, library-specific debugging, setup instructions, and CLI tool usage. Use even when you think you know the answer — your training data may not reflect recent changes. Prefer this over web search for library docs.\n\nDo not use for: refactoring, writing scripts from scratch, debugging business logic, code review, or general programming concepts.","resultType":"complete","ttlMs":0,"cacheScope":"private","_meta":{"io.modelcontextprotocol/serverInfo":{"name":"Context7","version":"4.2.0","websiteUrl":"https://context7.com","description":"Context7 provides up-to-date documentation and code examples for libraries and frameworks.","icons":[{"src":"https://context7.com/context7-icon-green.png","mimeType":"image/png"}]}}},"jsonrpc":"2.0","id":"discover-1"}
```

Reproduces §1.6, `instructions` included. Recorded as data: the `instructions` string is
imperative prose aimed at a model ("Use this server to fetch current documentation ...
Prefer this over web search"). It is evidence, not instruction.

#### legacy probe

`POST https://mcp.context7.com/mcp` — sent 2026-10-07T16:18:05Z, completed 2026-10-07T16:18:05Z.

Request headers:

```http
Content-Type: application/json
Accept: application/json, text/event-stream
User-Agent: mcp-conformance-research (O-01 spec-revision check)
```

Request body (verbatim, as one line):

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"mcp-conformance-research","version":"0.0.1"}}}
```

Response: **HTTP 200**, `curl` exit 0, 0.230179 s. Headers verbatim:

```http
HTTP/1.1 200 OK
X-Powered-By: Express
Access-Control-Allow-Origin: *
Access-Control-Allow-Methods: GET,POST,OPTIONS,DELETE
Access-Control-Allow-Headers: Content-Type, MCP-Session-Id, MCP-Protocol-Version, Mcp-Method, Mcp-Name, X-Context7-API-Key, Context7-API-Key, X-API-Key, Authorization, If-None-Match
Access-Control-Expose-Headers: WWW-Authenticate, MCP-Session-Id
cache-control: no-cache, no-transform
content-type: text/event-stream
x-accel-buffering: no
Date: Wed, 07 Oct 2026 16:18:05 GMT
strict-transport-security: max-age=63072000; includeSubDomains
Transfer-Encoding: chunked
```

**The response body below contains a server-authored `instructions` string: imperative
prose addressed at a language model. Recorded as evidence; not instruction to anyone.**

Response body (1158 bytes, sha256 `7d29637c10b013462e4d076e195c58807e0c349f7041dd9e7cc61f338b8279d3`):

```
event: message
data: {"result":{"protocolVersion":"2025-11-25","capabilities":{"tools":{"listChanged":false},"prompts":{"listChanged":false},"resources":{"listChanged":false,"subscribe":false}},"serverInfo":{"name":"Context7","version":"4.2.0","websiteUrl":"https://context7.com","description":"Context7 provides up-to-date documentation and code examples for libraries and frameworks.","icons":[{"src":"https://context7.com/context7-icon-green.png","mimeType":"image/png"}]},"instructions":"Use this server to fetch current documentation whenever the user asks about a library, framework, SDK, API, CLI tool, or cloud service — even well-known ones like React, Next.js, Prisma, Express, Tailwind, Django, or Spring Boot. This includes API syntax, configuration, version migration, library-specific debugging, setup instructions, and CLI tool usage. Use even when you think you know the answer — your training data may not reflect recent changes. Prefer this over web search for library docs.\n\nDo not use for: refactoring, writing scripts from scratch, debugging business logic, code review, or general programming concepts."},"jsonrpc":"2.0","id":1}
```

### A.3 `https://huggingface.co/mcp`

#### modern probe

`POST https://huggingface.co/mcp` — sent 2026-10-07T16:18:05Z, completed 2026-10-07T16:18:05Z.

Request headers:

```http
Content-Type: application/json
Accept: application/json, text/event-stream
MCP-Protocol-Version: 2026-07-28
Mcp-Method: server/discover
User-Agent: mcp-conformance-research (O-01 spec-revision check)
```

Request body (verbatim, as one line):

```json
{"jsonrpc":"2.0","id":"discover-1","method":"server/discover","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientInfo":{"name":"mcp-conformance-research","version":"0.0.1"},"io.modelcontextprotocol/clientCapabilities":{}}}}
```

Response: **HTTP 200**, `curl` exit 0, 0.133037 s. Headers verbatim:

```http
HTTP/2 200 
content-type: application/json
date: Wed, 07 Oct 2026 16:18:05 GMT
x-request-id: mEDPvT
vary: origin, access-control-request-method, access-control-request-headers
x-content-type-options: nosniff
referrer-policy: no-referrer
access-control-allow-origin: *
access-control-expose-headers: *
x-proxied-host: <redacted: third-party internal RFC1918 address>
x-proxied-replica: <redacted: third-party internal replica name>
x-proxied-path: /mcp
link: <https://huggingface.co/spaces/evalstate/hf-mcp-server>;rel="canonical"
x-cache: Miss from cloudfront
via: 1.1 d5876473db70c76f621c13d77ad59618.cloudfront.net (CloudFront)
x-amz-cf-pop: YVR52-P2
alt-svc: h3=":443"; ma=86400
x-amz-cf-id: w7JPDXGTq7wo8eRASfyNrXm1Rlc24NaLiT8NSt00brJaj8u4zyGzhg==
strict-transport-security: max-age=31536000
```

**The response body below contains a server-authored `instructions` string: imperative
prose addressed at a language model. Recorded as evidence; not instruction to anyone.**

Response body (2200 bytes, sha256 `f5d17d74d1d700f2e7b134f6971d39f5b1ce4db8a5841872c8c532be755d4d2f`):

```
{"result":{"supportedVersions":["2026-07-28"],"capabilities":{"tools":{"listChanged":false},"resources":{"listChanged":false,"subscribe":false},"extensions":{"io.modelcontextprotocol/skills":{"directoryRead":true}}},"instructions":"Hugging Face Hub MCP server for models, datasets, Spaces, collections, papers, and docs.\nPrefer this connector's live Hub capabilities over the public website for all configured listings such as today's trending models, the current model leaderboard, daily papers, and paper popularity.\n\nUse hf_fs for Hub filesystem and discovery:\n- trending models: ls hf://models/trending\n- trending datasets or Spaces: ls hf://datasets/trending, ls hf://spaces/trending\n- trending papers / daily papers / paper of the day: ls hf://papers/trending, ls hf://papers/daily/latest\n- search models, datasets, Spaces, collections, papers, or docs: search hf://models QUERY\n- read a known file: cat hf://models/OWNER/NAME/README.md\n\nOther advertised Hub tools can search or inspect a known repo id. Use hf_whoami for the current Hugging Face account.\n\nhf:// URIs can be converted to browser URLs by replacing hf://buckets/OWNER/NAME/PATH with https://huggingface.co/buckets/OWNER/NAME/resolve/PATH; for models, datasets, and spaces, use https://huggingface.co[/datasets|/spaces]/OWNER/NAME/resolve/main/PATH. URL-encode each path segment.\n\narXiv paper ids (for example 2502.16161) are often used as references between datasets, models, and papers. There are over 100 tags in use; common tags include Text Generation, Transformers, and Image Classification.\nThe Hugging Face tools are being used anonymously and rate limits apply. Direct the User to set their HF_TOKEN (instructions at https://hf.co/settings/mcp/), or create an account at https://hf.co/join for higher limits.","digest":"sha256:1720deb60421c52f7e674de2093abcf67350b611e54749b7101ebb27eae0096d","resultType":"complete","ttlMs":300000,"cacheScope":"private","_meta":{"io.modelcontextprotocol/serverInfo":{"name":"huggingface.co/mcp","version":"0.4.28","title":"Hugging Face","websiteUrl":"https://huggingface.co/mcp","icons":[{"src":"https://huggingface.co/favicon.ico"}]}}},"jsonrpc":"2.0","id":"discover-1"}
```

Reproduces §1.6, including `capabilities.extensions["io.modelcontextprotocol/skills"]`.
Two observations. (i) The result carries a `digest` field, which `DiscoverResult` does
not declare — permitted, because the base `Result` interface has an index signature
`[key: string]: unknown` (verified in the pinned schema), so this is a legal server
extension rather than a spec violation. (ii) Its `instructions` string contains
imperative prose aimed at a model, including "Direct the User to set their HF_TOKEN".
Recorded as data; not acted on. This is exactly the surface §1.4's trust-model note
flags — reached on the *first* connect-level request, before any tool is listed.

#### legacy probe

`POST https://huggingface.co/mcp` — sent 2026-10-07T16:18:05Z, completed 2026-10-07T16:18:05Z.

Request headers:

```http
Content-Type: application/json
Accept: application/json, text/event-stream
User-Agent: mcp-conformance-research (O-01 spec-revision check)
```

Request body (verbatim, as one line):

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"mcp-conformance-research","version":"0.0.1"}}}
```

Response: **HTTP 429**, `curl` exit 0, 0.126654 s. Headers verbatim:

```http
HTTP/2 429 
content-type: text/html
content-length: 3855
server: awselb/2.0
date: Wed, 07 Oct 2026 16:18:05 GMT
x-cache: Error from cloudfront
via: 1.1 b18063ae8504c990a40db9d8d53e01b2.cloudfront.net (CloudFront)
x-amz-cf-pop: YVR52-P2
alt-svc: h3=":443"; ma=86400
x-amz-cf-id: Qaeon3-a58cXpgNGcw1wQ7sM0MUkJ3rKF6v-vO1vHR5-zEB26KLlVw==
strict-transport-security: max-age=31536000
vary: Origin
```

Response body (3855 bytes, sha256 `8c3b4819f1b3125e919d5d4ca45340472d4fba3a135b756b22c0c395efcad95b`):

```
<!doctype html>
<html class="" lang="en">
    <head>
        <meta charset="utf-8" />
        <meta
            name="viewport"
            content="width=device-width, initial-scale=1.0, user-scalabl
[... truncated here; 3855 bytes total, sha256 8c3b4819f1b3125e919d5d4ca45340472d4fba3a135b756b22c0c395efcad95b]
```

**Not the response §1.6 recorded.** The original run reported HTTP 200 with a JSON body
minting an `mcp-session-id`. Today this came back as HTTP **429** from `server:
awselb/2.0` with Hugging Face's generic HTML error page — an edge rate-limit response,
not an MCP-layer answer, and most likely self-inflicted: all three of this host's probes
were sent inside the same second. The three-request per-host budget was already spent,
so it was **not** retried, and the legacy half of this row is therefore recorded as not
preserved rather than re-asserted. The host's own modern response (A.3, first probe)
independently corroborates the `instructions` text's remark that "The Hugging Face tools
are being used anonymously and rate limits apply."

#### P0-09's exact payload

`POST https://huggingface.co/mcp` — sent 2026-10-07T16:18:05Z, completed 2026-10-07T16:18:05Z.

Request headers:

```http
Content-Type: application/json
Accept: application/json, text/event-stream
User-Agent: mcp-conformance-research (O-01 spec-revision check)
```

Request body (verbatim, as one line):

```json
{"jsonrpc":"2.0","id":1,"method":"server/discover","params":{"protocolVersion":"2026-07-28","capabilities":{},"clientInfo":{"name":"mcp-conformance-research","version":"0.0.1"}}}
```

Response: **HTTP 429**, `curl` exit 0, 0.128349 s. Headers verbatim:

```http
HTTP/2 429 
content-type: text/html
content-length: 3855
server: awselb/2.0
date: Wed, 07 Oct 2026 16:18:05 GMT
x-cache: Error from cloudfront
via: 1.1 d13d02cbda3d9f87796479cd273941a4.cloudfront.net (CloudFront)
x-amz-cf-pop: YVR52-P2
alt-svc: h3=":443"; ma=86400
x-amz-cf-id: fjG33RH7ACnb_3C7Ma_JZ0D4OE8GE0jXvCFhHA-yZv6ZlQv53IM0xQ==
strict-transport-security: max-age=31536000
vary: Origin
```

Response body (3855 bytes, sha256 `8c3b4819f1b3125e919d5d4ca45340472d4fba3a135b756b22c0c395efcad95b`):

```
<!doctype html>
<html class="" lang="en">
    <head>
        <meta charset="utf-8" />
        <meta
            name="viewport"
            content="width=device-width, initial-scale=1.0, user-scalabl
[... truncated here; 3855 bytes total, sha256 8c3b4819f1b3125e919d5d4ca45340472d4fba3a135b756b22c0c395efcad95b]
```

**Also rate-limited, same cause.** The original run reported HTTP 400
`{"code":-32600,"message":"Session ID required"}`, which is the observation §1.4(a)
cites. It is **not reproduced here** and is not preserved. Note that the Cloudflare
transcript in A.1 independently demonstrates the same point (`-32601 Method not found`
for P0-09's payload against a server whose modern probe succeeds), so §1.4(a)'s
conclusion does not rest on this row alone.

### A.4 `https://mcp.deepwiki.com/mcp`

#### modern probe

`POST https://mcp.deepwiki.com/mcp` — sent 2026-10-07T16:18:05Z, completed 2026-10-07T16:18:06Z.

Request headers:

```http
Content-Type: application/json
Accept: application/json, text/event-stream
MCP-Protocol-Version: 2026-07-28
Mcp-Method: server/discover
User-Agent: mcp-conformance-research (O-01 spec-revision check)
```

Request body (verbatim, as one line):

```json
{"jsonrpc":"2.0","id":"discover-1","method":"server/discover","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientInfo":{"name":"mcp-conformance-research","version":"0.0.1"},"io.modelcontextprotocol/clientCapabilities":{}}}}
```

Response: **HTTP 400**, `curl` exit 0, 0.126304 s. Headers verbatim:

```http
HTTP/2 400 
date: Wed, 07 Oct 2026 16:18:06 GMT
content-type: application/json
content-length: 195
server: uvicorn
```

Response body (195 bytes, sha256 `28232be0c986a8e5d4a9f4e45d3e19bd69555c936637ce01fcb2aa64628ea539`):

```
{"jsonrpc":"2.0","id":"server-error","error":{"code":-32600,"message":"Bad Request: Unsupported protocol version: 2026-07-28. Supported versions: 2024-11-05, 2025-03-26, 2025-06-18, 2025-11-25"}}
```

Reproduces §1.6 exactly, including the point that matters for §1.4(e): the rejection is
`-32600`, **not** the modern `-32022` `UnsupportedProtocolVersionError`. Under the
spec's HTTP dual-era algorithm that is a *non-modern* error body, so the correct client
action is to fall back to `initialize` — which is what makes "classify the body, do not
key on HTTP 400" the right rule. Note also that the server replaces the request `id`
with the string `"server-error"` rather than echoing `"discover-1"`.

#### legacy probe

`POST https://mcp.deepwiki.com/mcp` — sent 2026-10-07T16:18:06Z, completed 2026-10-07T16:18:06Z.

Request headers:

```http
Content-Type: application/json
Accept: application/json, text/event-stream
User-Agent: mcp-conformance-research (O-01 spec-revision check)
```

Request body (verbatim, as one line):

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"mcp-conformance-research","version":"0.0.1"}}}
```

Response: **HTTP 200**, `curl` exit 0, 0.064013 s. Headers verbatim:

```http
HTTP/2 200 
date: Wed, 07 Oct 2026 16:18:06 GMT
content-type: text/event-stream
server: uvicorn
cache-control: no-cache, no-transform
x-accel-buffering: no
```

**The response body below contains a server-authored `instructions` string: imperative
prose addressed at a language model. Recorded as evidence; not instruction to anyone.**

Response body (3670 bytes, sha256 `6e08e3dfbf54d2db38b2459dd42a82d82f623b48fce597a1cc83a33a1b27091a`):

```
event: message
data: {"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-11-25","capabilities":{"experimental":{},"prompts":{"listChanged":true},"resources":{"subscribe":false,"listChanged":true},"tools":{"listChanged":true}},"serverInfo":{"name":"DeepWiki","version":"2.14.3"},"instructions":"DeepWiki MCP provides AI-powered documentation for GitHub repositories.\n\nAvailable tools:\n- read_wiki_structure: Get a list of documentation topics for a repository\n- read_wiki_contents: View full documentation about a repository\n- ask_wiki_question: Ask any question about a repository's codebase and get an AI-powered answer from its wiki\n- list_wiki_repos: List the repositories that have a DeepWiki index (private mode only)\n- generate_wiki: Generate a codebase wiki for a repository — only use when explicitly requested by the user (private mode only)\n- devin_automation_manage: Manage Devin automations — list, get, create, update, delete, or fetch the trigger event schemas (private mode only)\n- devin_billing_tag_manage: Manage Devin billing tags (groupings of Devin sessions for usage tracking) — list, get, create, assign sessions, look up session tags (private mode only)\n
[... truncated here; 3670 bytes total, sha256 6e08e3dfbf54d2db38b2459dd42a82d82f623b48fce597a1cc83a33a1b27091a]
```

Reproduces §1.6 (`protocolVersion: "2025-11-25"`). Two recording notes. (i) DeepWiki is the
one host whose SSE framing used **CRLF** line endings; the three CR bytes have been normalised
to LF in the block above so this file stays LF-only, and the sha256 is over the raw bytes as
received, not over the normalised text. (ii) The `instructions` string enumerates this
server's tools in imperative prose, including "only use when explicitly requested by the
user". Recorded as data; not acted on.

### A.5 `https://gitmcp.io/docs`

#### modern probe

`POST https://gitmcp.io/docs` — sent 2026-10-07T16:18:06Z, completed 2026-10-07T16:18:06Z.

Request headers:

```http
Content-Type: application/json
Accept: application/json, text/event-stream
MCP-Protocol-Version: 2026-07-28
Mcp-Method: server/discover
User-Agent: mcp-conformance-research (O-01 spec-revision check)
```

Request body (verbatim, as one line):

```json
{"jsonrpc":"2.0","id":"discover-1","method":"server/discover","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientInfo":{"name":"mcp-conformance-research","version":"0.0.1"},"io.modelcontextprotocol/clientCapabilities":{}}}}
```

Response: **HTTP 400**, `curl` exit 0, 0.167334 s. Headers verbatim:

```http
HTTP/2 400 
date: Wed, 07 Oct 2026 16:18:06 GMT
content-type: text/plain;charset=UTF-8
content-length: 110
report-to: {"group":"cf-nel","max_age":604800,"endpoints":[{"url":"https://a.nel.cloudflare.com/report/v4?s=zS1xoGnUjFNZoo0Js%2B84e50OkipMwjXT%2FeziVq9rwfTS6Xp9l11f0MTvP9W1O4Ki%2F%2BtShah0NI2VW4QExcnLRllZksQz25g08lv18RhL5KFCrrFdICcnP2bMHjQ%3D"}]}
nel: {"report_to":"cf-nel","success_fraction":0.0,"max_age":604800}
server: cloudflare
cf-ray: a46e38468bc076f2-SEA
alt-svc: h3=":443"; ma=86400
```

Response body (110 bytes, sha256 `b001151d802e2a9ae708e48870654ab7501203250b6643cb78348f508fd6168d`):

```
{"jsonrpc":"2.0","error":{"code":-32000,"message":"Bad Request: Mcp-Session-Id header is required"},"id":null}
```

Reproduces §1.6. The `-32000` "Mcp-Session-Id header is required" response is a
legacy-era demand for a header `2026-07-28` deleted, which is itself a clean era signal
— and, like DeepWiki, a non-modern error body behind an HTTP 400.

#### legacy probe

`POST https://gitmcp.io/docs` — sent 2026-10-07T16:18:06Z, completed 2026-10-07T16:18:17Z.

Request headers:

```http
Content-Type: application/json
Accept: application/json, text/event-stream
User-Agent: mcp-conformance-research (O-01 spec-revision check)
```

Request body (verbatim, as one line):

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"mcp-conformance-research","version":"0.0.1"}}}
```

Response: **HTTP 200**, `curl` exit 0, 10.825669 s. Headers verbatim:

```http
HTTP/2 200 
date: Wed, 07 Oct 2026 16:18:07 GMT
content-type: text/event-stream
access-control-allow-origin: *
cache-control: no-cache
access-control-allow-headers: Content-Type, mcp-session-id
access-control-allow-methods: GET, POST, OPTIONS
access-control-expose-headers: mcp-session-id
access-control-max-age: 86400
mcp-session-id: <redacted: 64-hex session id>
report-to: {"group":"cf-nel","max_age":604800,"endpoints":[{"url":"https://a.nel.cloudflare.com/report/v4?s=P7C%2Bocw9bAjPjqunSwLago%2FL%2FOCmwIkz4DL9NdwDRG8ZOOHQScoGBaAUEbPVCuSYWTk0yWTJKKkGL%2FDAps%2B9VM0wjIWZOYBJbvrv9FVsyYY5GdbMgAECgzr2T04%3D"}]}
nel: {"report_to":"cf-nel","success_fraction":0.0,"max_age":604800}
server: cloudflare
cf-ray: a46e38474aeec4b4-YVR
alt-svc: h3=":443"; ma=86400
```

Response body (184 bytes, sha256 `88afbe328d4c966f0ac51318cbde494d4a978f79cda6f100832c35af6f61f240`):

```
event: message
data: {"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-03-26","capabilities":{"tools":{"listChanged":true}},"serverInfo":{"name":"GitMCP","version":"1.1.0"}}}
```

Reproduces §1.6 (`protocolVersion: "2025-03-26"`). `time_total` of 10.8 s is the SSE
stream being held open after the single `event: message`, not latency.

### A.6 What the re-probe changes

- **§1.6's era column stands for four of five hosts**, on today's evidence alone: Cloudflare
  docs and Context7 are dual-era (modern `DiscoverResult` *and* a legacy `initialize` that
  answers over SSE); DeepWiki and GitMCP are legacy-only (both reject the modern probe with a
  non-modern error body behind an HTTP 400).
- **Hugging Face is dual-era on today's evidence only for the modern half.** Its
  `server/discover` returns a `DiscoverResult` with `supportedVersions: ["2026-07-28"]`; its
  legacy behaviour could not be re-verified within the three-request budget and is marked
  unpreserved in §1.6.
- **No `2026-07-28`-only server was found**, consistent with §1.6. All three modern-capable
  hosts here are dual-era. Whether a modern-only public server exists remains **UNVERIFIED**:
  no registry-wide sweep was run, and a sweep is not possible with the current discovery
  client anyway (§1.4(e)).
- **Three of five well-known public endpoints already speak `2026-07-28` today.** That is the
  number which retires P0-09's "blocked on such a server existing" caveat: a dual-era server
  is a sufficient target for testing the modern path, and three are reachable without
  credentials.
