# ADR-014: Request pacing, and a request budget keyed on the registrable domain

**Status:** Accepted
**Date:** 2026-10-12
**Author:** Jun Cui
**Closes:** [P0-12](../tasks.md#p0-12-intra-host-pacing-and-an-etld1-request-budget)
**Related:** [P0-07](../tasks.md#p0-07-full-census--1000-servers) (the sweep this gates),
[P0-11](../tasks.md#p0-11-correct-dual-era-discovery-for-mcp-2026-07-28) and
[ADR-013](013-era-negotiation.md) (modern-first, which added a request to every legacy host,
and which recorded this as a prerequisite), [O-01](../tasks.md#o-01-track-the-tool-annotations-interest-group)
(the 2026-10-07 carry-forward block that specified the two controls), design.md §3,
architecture.md §7

---

## Context

The next census sweep contacts roughly 1,000 unrelated third parties under an identifying
`User-Agent`. Three facts made the controls below a prerequisite rather than a refinement,
and all three are this project's own measurements, not general principle:

1. **`xtask/src/census_stage1.rs` had no intra-host delay at all.** A host's 2–5 requests
   went out back to back. `DiscoveryClient::discover` fires its sequence immediately, and
   P0-11's modern-first ordering added a fourth request to the legacy majority (~75% of
   reachable hosts).
2. **This project has already been throttled for exactly that.** The O-01 probe lost a
   transcript to an HTTP 429 caused by three requests landing inside one second against one
   host. P0-11's live verification therefore imposed a 1.2 s floor **by hand** — a discipline
   the sweep code did not have. And the committed
   `results/census/class_b_annotation_coverage.json` holds two `http status: 429` records
   from July, which that sweep could not distinguish from dead hosts (P0-11 fixed the
   *categorisation* half; this ADR is the *conduct* half).
3. **A per-host budget is the wrong unit.** Grouping the committed July corpus's 1,000 Class
   B entries by host gives 774 hosts, and the largest is `gateway.pipeworx.io` with **149
   registry entries on one host**. Grouping by registrable domain gives 619 groups, and
   surfaces the case a per-host budget cannot see at all: **18 entries on 18 distinct hosts**
   under `klymax402.com` (`barcode-generator.api.…`, `base64-codec.api.…`, …), plus
   `alpic.live` (10), `wishpool.app` (9), `clauxel.com` (8), `mcpize.run` (7). Every per-host
   budget stays satisfied while the vendor absorbs tens to hundreds of requests.

---

## Decision

### 1. Both controls live at the one point every HTTP request is built, and the unit is the request

`crates/discovery/src/pace.rs` holds a `Pacer`, shared across the run behind an `Arc`, and
`HttpTransport::post` — the single function through which `call_raw` and `notify` both send —
consults it immediately before handing the body to `ureq`.

That placement is the decision. It makes "the unit is *requests to this host*, not *hosts
processed*" true by construction: the modern probe, the legacy `initialize`, the
`notifications/initialized` POST (a notification, and so the one most easily missed) and
`tools/list` all pass through it, and so would any request a later task adds. Pacing in the
sweep loop instead would have spaced only the gap between one `discover()` call and the next,
which is the thing that was never the problem.

The wait is taken *before* `ureq` is given the request, so it sits outside the agent's
`timeout_global` and never eats into a request's own time budget. It also means the previous
request's time on the wire counts toward the interval: a slow host costs no extra spacing at
all, which is why the cost below is an upper bound rather than an estimate.

**A hook at that point is only sound if nothing below it can send a second request, and
originally something could.** Both HTTP clients in this workspace built their own
`ureq::Agent` and neither set `max_redirects`, so `ureq`'s default of 10 applied *inside*
the call, below the hook. One `post` under a redirect chain was measured at 11 network
requests in roughly 20 ms with no spacing at all; a cross-host `302` returned HTTP 200
carrying the other host's bytes; and with this harness's exact agent configuration a
`Location` header reached and read a JSON body from a service on the operator's own machine
that no registry entry named. From a sweep's vantage point that reaches loopback, the
WSL host and subnet, RFC1918, `169.254.169.254` and the local public /21.

Narrower than a careless description would suggest, and worth stating precisely: `301`/`302`/
`303` are followed with the method converted to **GET** and the body dropped, so the SSRF was
**GET-only and body-less**; `307`/`308` failed outright because `ureq` cannot replay a
consumed POST body; non-HTTP schemes were rejected; and `timeout_global` does bound a whole
chain, so there was no wall-clock amplification.

Five consequences made this a correctness problem and not only a conduct one:
architecture.md §1's "evidence, never instruction" was violated, a `Location` header being
the one server input the census path acted on as an instruction; P0-02's pin could attest to
the **wrong principal**, since the record names the listed server while the bytes came from
whatever answered and `save_redirect_history` was off so nothing recorded which; the
120-request cap permitted up to **1,320** requests; `pacing.requests_sent` could understate
real load ~11x with the redirect target's share appearing under **no key at all**, which
`verify_pacing_self_consistent` could not detect because it cross-checks two views of the
same undercount; and foreign bytes entered the content-addressed evidence store at a
server's direction.

**Decision: agent construction moved into `pace`, and obtaining a configured `ureq::Agent`
now requires a `Permit`.** `Permit::into_agent` is the only constructor, it consumes the
permit, and it never hands the agent back — the only way to send is `PacedAgent::post`, which
paces first. `max_redirects(0)` is baked in there, so it cannot be forgotten at a call site.
An explicitly-named `PacedAgent::unpaced` remains for unit tests against local fakes, and is
still redirect-free, because no-redirects is a property of every client this module builds
rather than part of the pacing bargain. That makes an unpaced or redirect-following HTTP
client hard to *write* rather than merely discouraged, and it fixed `discovery` and `probe` in
one place.

**Rejected:** adding `.max_redirects(0)` at the two call sites. It would have closed the
hole on the day and left the next HTTP client to inherit `ureq`'s default again, which is
exactly how both of these got it.

`max_redirects` is published in the policy block (§5), because a followed redirect is a
request the accounting cannot see, and a reader checking "these counts are the requests this
sweep sent" needs to know the number is zero.

**Comparability:** with no redirect followed, a 3xx arrives as an ordinary response, fails
`call_raw`'s status gate as `DiscoveryError::HttpStatus { status: 30x }`, and
`census_report::http_status_category` maps any non-`429` status to `transport` — which is
where the committed July file's 15 `redirect failed` entries already sit. Only the detail
string changes, from `redirect failed` to `http status: 302`. Verified by test
(`a_redirect_is_not_followed_costs_one_request_and_surfaces_as_a_status`), which also asserts
the redirect target is never contacted and the single request is charged exactly once.

**If following a redirect is ever wanted, this is the design** — recorded, not built. Do it
**hop by hop at the call site**, never inside the agent: each hop re-keyed through
`Pacer::admit`/`Permit::before_request` so it is spaced and charged against *its own*
registrable domain; `save_redirect_history(true)` so the hop list is evidence; and a
`final_url` identity field on the record so a pin says which principal it attests to. A hop
to a different registrable domain should be a recorded refusal by default, not a silent
follow.

**Rejected:** a delay inside `DiscoveryClient::discover`. It would have to be threaded through
every branch of a five-arm era algorithm, and a branch added later would silently miss it.

**Rejected:** pacing in `xtask`. `xtask` cannot see the requests — it sees one `discover()`
call per host.

### 2. The registrable domain is derived by a documented heuristic, not a vendored public-suffix list

`registrable_domain(host)`: the last two labels of the host, or the last three when the
second-to-last is one of a fixed list of 31 generic registry second levels (`co`, `com`,
`net`, `org`, `ac`, `go`, `ne`, `or`, `sch`, `gov`, …). An IP literal, and any host with
fewer than three labels, is its own key. The result is lowercased, stripped of the DNS root
dot, bounded to 253 bytes and reduced to `[a-z0-9.\-_:\[\]]`, because the endpoint it came
from is registry-supplied and therefore untrusted and the key is published and appears in a
failure detail.

Two precisions, both corrections to earlier wording in this ADR:

- **The key is never a JSON map key.** It is published as a *value*, under
  `"registrable_domain"`, in each per-domain row and on each server record. That is the safer
  shape and it is deliberate: a server-influenced string used as an object key can collide
  with a structural field name, and whether a duplicate key is last-wins or an error is
  reader-dependent. As a value it is inert.
- **Bounding keeps the *last* 253 bytes, not the first.** The registrable domain is a
  *suffix*. Truncating from the front would have split one over-long registrant into an
  unbounded family of distinct keys — *finer* grouping, each key carrying its own full
  budget — which is the one direction this control must not fail in. The case is
  **unreachable**, not merely coarse: 253 bytes is the DNS presentation limit, so such a host
  cannot be resolved and this transport could never have connected to it.

**This is a politeness control, not an anti-abuse control**, and the distinction should be
stated rather than left for a reader to discover. Splitting load across registrable domains
is free *by design* — that is what makes the key the right unit for "how much did one vendor
receive" — and no key derivation can fix it. Verified several ways while reviewing: `0x7f.0.0.1`
keys as `0.1`, the integer form `2130706433` becomes its own key, and `127.0.0.1.nip.io` keys
as `nip.io`. **Do not tighten the parser in response.** A parser hardened against an
adversary who can simply register another domain buys nothing and costs the auditability the
heuristic was chosen for. What bounds adversarial load is the per-run seed (§3) plus the fact
that every refusal is recorded and attributable.

**Why not a vendored, pinned public-suffix snapshot**, which is otherwise exactly this
project's reproducibility posture (F-00's pinned kernel, the pinned toolchain, F-05's
content addressing):

- **A list this harness cannot verify is worse than an honest heuristic.** Vendoring means
  committing bytes fetched from publicsuffix.org. F-00's own standard for a pinned artifact
  is a checksum *independently verified* — "not asserted from a search result" — and there is
  no authoritative digest for a dated PSL snapshot to verify one against. Hand-writing a list
  from memory and labelling it a pinned snapshot would be P0-09's mistake exactly: an
  extrapolated artifact presented as a checked one.
- **The PSL's *private* section, which is what a library would apply by default, is the wrong
  grouping for this purpose anyway** — see §3. Getting the right answer would mean vendoring
  the list *and* applying only its ICANN section, which is a third decision to defend.
- **The heuristic's error direction is the safe one** for everything except a narrow case this
  ADR names in §3, and its grouping is *auditable from the published results file* (§5), so it
  can be upgraded from a real sweep's data rather than from a guess. That is ADR-003's
  discipline — measure the noise floor, do not assume it — applied to a different control.

**What the heuristic gets wrong**, stated precisely, with the July corpus as the evidence:

| case | heuristic | what a PSL would say | direction | consequence |
|---|---|---|---|---|
| `moema.sampa.br`, `campobelo.sampa.br`, `vilabuarque.sampa.br` (3 July entries) | one group, `sampa.br` | three groups (`sampa.br` is an ICANN suffix the list omits) | **coarser** | three registrants share one budget; harmless at any realistic cap |
| `app.eno.cx.ua` and other long-tail ccTLD second levels | `cx.ua` | `eno.cx.ua` | **coarser** | same |
| `*.vercel.app` (22), `*.workers.dev` (18), `*.railway.app` (15), `*.onrender.com` (14), `*.fly.dev` (8), `*.apify.actor` (9), `*.trycloudflare.com` (6), `*.run.app` (3) | one group each | one group *per customer* (PSL private section) | **coarser** | ~95 July entries grouped into 8 pseudo-vendors. This is the failure that can actually cost data, and it is why the default cap is 120 rather than small — see §3 |
| `x.me.com`, `y.co.com` and similar | `x.me.com` | `me.com`, `co.com` | **finer** | the one under-cautious direction. Narrow (it needs a registry label used as a *private* second level), absent from the July corpus, and still bounded by the delay, which is keyed on the same key |
| `münchen.de` (never punycode-encoded) | `m_nchen.de` | n/a | coarser | an ugly key that still groups deterministically; this transport could not have connected to such a host anyway |

**Upgrade path, with its trigger:** vendor the PSL's ICANN section and apply it instead, if
and when a real sweep's published per-domain accounting (§5) shows the grouping mis-binding —
most likely as refusals concentrated on a shared-hosting suffix. The key's derivation is
named in the results header as `heuristic-v1` precisely so that a change to it is a visible
version bump rather than a silent redefinition of every published per-domain row.

### 3. The budget caps requests per registrable domain, at 120 by default, and the cap is hard

`Pacer::admit(endpoint)` is called once per discovery sequence, **before the client exists**.
It reserves `MAX_REQUESTS_PER_DISCOVERY` (5, equal to `discover()`'s own documented request
bound) against the domain and succeeds only if `charged + reserved + 5 ≤ cap`. Reserving up
front rather than charging as the sequence runs is what makes the cap *hard* rather than
overshootable, and what makes it correct under concurrency: two admissions cannot both see
the same remaining headroom. A dropped permit returns its reservation, so an interrupted or
panicking sequence cannot strand budget.

**120 was derived from the committed July corpus, not picked.** Grouping its 1,000 Class B
entries gives 619 groups; after the one outlier, the largest holds 22 entries. A cap of 120
with a 5-request reservation admits at least 24 full sequences per group — so every group in
that corpus is attempted in full **except** `gateway.pipeworx.io`, where 58 entries are
admitted and 91 refused, taking that one host from ~298 requests to 116. The cap binds
exactly where the measured risk is and nowhere else. It is also what keeps the shared-hosting
over-grouping in §2 from costing data: the largest such group is 22 entries, comfortably
inside 24.

**Rejected:** a cap tight enough to sound polite in the abstract (10–40 requests). Replaying
the July corpus through the same admission arithmetic:

| cap | entries refused | domains refusing | requests sent |
|---|---|---|---|
| 10 | 311 | 26 | 1,557 |
| 20 | 240 | 13 | 1,741 |
| 40 | 169 | 8 | 1,935 |
| 60 | 135 | 3 | 2,037 |
| 100 | 103 | 2 | 2,133 |
| **120** | **91** | **1** | **2,163** |
| 200 | 51 | 1 | 2,243 |

120 is the point at which exactly one domain binds — the measured outlier. Below about 60 the
refusals stop being that outlier and start being legitimate distinct registrants behind a
shared PaaS suffix the heuristic over-groups (§2). A control that destroys the measurement it
is protecting is not a safety property.

**Rejected:** throttling by *rate* per domain instead of a total. A rate limit bounds the
burst but not the volume, and 596 unsolicited requests to one third-party host in one sweep
is indefensible however slowly they arrive.

### 3a. The sample is selected under a per-run seed, published in the results header

Sample selection hashes each candidate's name with `DefaultHasher`, whose keys are fixed
(unlike `HashMap`'s `RandomState`) so a sweep is reproducible. Keyless, that also made sweep
membership **and order** precomputable offline by anyone. Measured: a name guaranteed to be
in a 1,000-of-8,294 sample was found in **7 tries**; a name guaranteed to land in the first
**10 positions** in **225 tries**; the whole search ran in under a millisecond.

Combined with the shared-hosting over-grouping in §2, that is a deterministic **starvation
primitive**: roughly 24 ground names under one suffix fill that suffix's 24-sequence headroom
and consume its entire budget before any legitimate registrant on it is reached, for the price
of about 24 registry entries.

**Decision.** Mix a per-run seed into the hash, publish it in the results header as
`sample_seed`, and accept it back with `--sample-seed`. A sweep stays exactly reproducible for
anyone holding the file, and is not precomputable by anyone before it runs. Admission *order*
within the sample is shuffled under the same seed with a different salt, because the two leak
separately: knowing you are in a sweep is one thing, knowing you are early enough in it to
consume a shared suffix's budget before anyone else is what does the damage.

The seed is published as a hex **string**, not a JSON number: a `u64` above 2^53 would be
silently rounded by any reader backed by doubles, and a published seed that does not reproduce
its own sample is worse than none. Entropy comes from `RandomState` (OS-seeded per process,
precisely so hash keys are unpredictable) mixed with the wall clock. Nothing here needs to be
cryptographic: the bar is "not computable by someone who does not yet have the results file".

**The victim-targeted variant is available, and this is now a measured fact rather than an
open question.** The registry does **not** bind `remotes[].url` to the publisher's verified
namespace. Of the 1,000 entries in the committed July file, **518 use `io.github.*`
namespaces** — verified against a GitHub account, not against the URL's domain — with
endpoints pointing at arbitrary unrelated hosts (`io.github.pinalmdave` →
`bookrails-api.azurewebsites.net`, `io.github.idtitanium` → `useclippy.cc`,
`io.github.srotzin` → `hive-mcp-audit-readiness.onrender.com`, `io.github.taru0208` →
`aligned-kettledrum--openfda-mcp-server.apify.actor`). So anyone with a GitHub account can
publish entries under their own verified namespace whose URLs point at a **third party's**
domain, consume that domain's census budget, and starve the victim's genuine servers out of
the sweep — and each such entry charges a slot even when DNS fails.

What already limits it, and is kept for that reason: a refusal is its own recorded category
with a detail naming the domain, the charged count and the cap; per-domain rows carry
admitted and refused counts; and records carry the publisher namespace. So starvation is
visible *and attributable by inspection* rather than silent. §4's refusal to drop refused
candidates is what buys that. The seed raises the cost of landing entries in a sample from
"grind a few names" to "publish enough entries to beat the sampling rate".

**Rejected (recorded, not implemented):** keying the budget on the **(publisher namespace,
registrable domain)** pair rather than the domain alone, so that one publisher cannot consume
a domain's whole headroom and a victim's own entries retain theirs. This is the better answer
than tightening the key, and it is a design change with its own trade-offs — it changes what
every published per-domain row means, and it needs a view on what a *legitimate* vendor with
many namespaces should get. It needs its own task and its own review.

### 4. A refusal is a recorded outcome with its own failure category, never a skipped host

A host whose domain budget is spent gets a server record with
`failure_category: "registrable_domain_budget_exhausted"` and a detail naming the domain, the
counts, the cap, and the fact that **no request was made to this host**. It never opens a
connection.

**A new category, which is what preserves July comparability.** `transport`, `protocol`,
`rate_limited` and the tool-weighted fields all keep their meaning exactly; a refusal lands in
a bucket July never had rather than being pooled into one it did. It is also a *harness-side
decision*, not a finding about the server — the same kind of thing as P0-10's
`evidence_store_error` — so a reader must be able to subtract it, which is what a distinct name
makes possible.

**But "subtractable" is not "free", and the earlier phrasing here pointed at the wrong
field.** A refusal **displaces** whatever category that host would otherwise have landed in.
The *success rate* is unchanged — exactly 25.0%, matching P0-07 — but only because all 149
`pipeworx.io` entries were `protocol` failures in July. What moves is
`failure_categories.protocol`: **312 → 221**. That is the comparison a reader actually makes
— *"SSE-only dropped by 91, did the ecosystem change?"* — and the answer is no: 91 of those
hosts were never contacted at all.

So, as a rule for anyone comparing a paced sweep to July: **add the refusals back into
whichever category the refused hosts would have landed in before comparing any category count
to a July count.** A refusal is only subtractable from the *total*; against a per-category
count it is a displacement, and the category it displaced is not recorded in the refusal
record (nothing was learned about that host, by construction — no request was made). The
refused hosts are individually listed, so the displacement is reconstructable from a prior
unpaced sweep of the same hosts, which is what makes the July file worth keeping beside the
new one.

**Rejected:** dropping refused candidates from the sample. A candidate absent from a results
file is indistinguishable from one that was never sampled, and the sweep's `attempted` count
would then silently disagree with `candidates_selected` for a reason nothing recorded. P0-10's
prefix-shaped `not_attempted` accounting exists for a *different* condition (the disk floor,
which stops the sweep) and conflating the two would misreport both.

**Rejected:** refusing mid-sequence, i.e. charging per request and failing the request that
would exceed the cap. It would surface as a `DiscoveryError` indistinguishable in the taxonomy
from the server's own behaviour, which is the damage P0-11 took trouble to avoid. Refusal
belongs at admission, where it can be attributed to the harness.

### 5. Policy *and* accounting are published, because a politeness claim nobody can check is not a finding

Every Stage 1 results file carries a top-level `pacing` block holding the policy (interval,
cap, reservation, and the named key derivation) **and** the accounting (requests sent, total
time waited, domains contacted, the most-loaded domain, every refusal, and the 25 most-loaded
per-domain rows with their admitted and refused counts). Every server record carries its own
`pacing` block: the key, the requests this attempt sent, and the milliseconds it waited.

`census_report::verify_pacing_self_consistent` then checks the header against the records
beneath it, and runs both when a sweep writes its file and when `census-rederive` reads one
back offline. It is deliberately independent of the code that *builds* the block, in the
shape B-03's `verify_report_matches_records` established: a checker sharing the aggregator's
code can only confirm the aggregator is self-consistent, never that the published numbers
describe the records beneath them.

**Six fields, not three.** The three totals (`requests_sent`, `waited_ms_total`,
`sequences_denied_by_budget`) were checked from the start. Three more are derivable from the
per-server records and were being published unchecked, including the one that matters most:

| field | derivation | why it has to be checked |
|---|---|---|
| `max_requests_to_one_registrable_domain` | max over domains of Σ per-server `requests_sent`, grouped by per-server `registrable_domain` | **The headline politeness number** — the one that substantiates "no third party received more than N requests". Load-bearing in any summary and, until now, unverified against the data beneath it |
| `registrable_domains_contacted` | distinct domains with ≥1 request | a denominator for every per-domain claim |
| `overshoot_requests` | Σ max(0, `requests_sent` − `per_discovery_reservation`) | the number that is supposed to go non-zero the day `tools/list` pagination lands, so a published zero has to mean something |

The overshoot derivation assumes **one permit per server record**. Every sweep that writes
this block holds that by construction — Stage 1 admits once per candidate, and
`census-pin-stability` deliberately covers *both* of its back-to-back sequences with a single
permit reserved at `2 × MAX_REQUESTS_PER_DISCOVERY` for exactly this reason. A future file
that breaks the assumption must not publish this block unchanged.

**`census-rederive` now exits non-zero on an inconsistency**, where a live sweep still only
prints loudly. The asymmetry is deliberate: a live sweep has already spent its third-party
contact and the data it collected is worth keeping, whereas the offline audit path has
nothing at risk and is the path a reader auditing a published file uses. Returning success
while printing `PACING ACCOUNTING INCONSISTENT` meant a CI job or a reviewer's script would
pass on a file whose politeness claim is demonstrably wrong.

The per-server accounting is wall-clock-dependent, so it rides as a carried-through
observation rather than a derived value — the same treatment `generated_at_unix` gets — which
keeps P0-10's byte-for-byte re-derivation property intact. There is a test that proves exactly
that.

### 6. Stage 2 is not paced, and says so in its results file

Stage 2 discovers Class A servers over stdio to a local container. There is no HTTP request to
a third-party MCP server in its sequence, so the request pacer has nothing to pace, and
`--jobs` is untouched. Its results header carries
`pacing: { "applies": false, "reason": … }` rather than no block at all, because an absent
block and a block saying "not applicable" are different claims about a file — the same
distinction P0-11 drew between `unrecorded` and `no_response` probe evidence.

**The politeness question Stage 2 does have is a different one, and is named rather than
quietly omitted:** `npx`/`uvx` inside each container fetch from npm and PyPI, and `--jobs`
allows up to 8 of those concurrently. That is real third-party load this harness causes, it is
outside the harness's own request path, and it is not governed here. Recorded as a limitation,
not fixed, because the right control for it is a package-fetch cache or a registry mirror
rather than a request pacer.

The two stages' option sets are disjoint and each rejects the other's flags
(`--jobs` on Stage 1, `--min-interval-ms`/`--max-requests-per-domain` on Stage 2), because a
flag that silently does nothing is worse than one that errors.

---

## Consequences

- **"Polite client" becomes true rather than aspirational**, on the sweep that was blocked on
  it. Projected over the committed July corpus (619 groups, the implemented admission
  algorithm, projected per-entry request counts under modern-first): **2,163 requests instead
  of 2,345**, 909 entries attempted and 91 refused, and the most-loaded domain falls from 298
  requests to 116.
- **The projected wall-clock cost is bounded and small.** The upper bound is
  `(requests − groups contacted) × interval` = `(2163 − 619) × 500 ms` ≈ **772 s ≈ 12.9
  minutes** added to a sweep P0-07 measured at "well inside an hour". In practice it is less,
  because the interval is measured from the previous request's slot and a request's own time
  on the wire counts toward it: the true cost is `Σ max(0, interval − latency)`, and the July
  sweep's responsive hosts answered in low seconds. The budget also removes 182 requests, so
  the net change may be close to zero.
- **No delay is ever paid between unrelated hosts.** A domain's first request never waits, and
  619 of the 1,000 July entries are the first in their group. Asserted by test.
- **91 of 1,000 July candidates would be refused, all on one host.** That is a deliberate,
  recorded reduction in coverage traded against not sending ~600 unsolicited requests to a
  single third party. It is visible in `failure_categories`, subtractable from the *total*
  (though a displacement against any per-category count — see §4), and raisable with
  `--max-requests-per-domain` as an explicit operator choice.
- **The second-most-loaded group is a single host, and the default cap admits all of it.**
  This is the number a conduct reviewer will ask about, and unstated is worse than defended.
  `server.smithery.ai` holds 22 of the July corpus's entries — all 22 on that one host, all
  of them the `@`-in-path URL shape — and the admission floor was chosen precisely so a
  22-entry group is **not** refused (§3). So every one of those sequences is admitted, and
  three numbers describe the exposure, each under a stated model:
  - **120** — what the policy permits to one registrable domain, and therefore, when that
    domain resolves to one host as this one does, to a single host. This needs no projection;
    it is the cap.
  - **110** — what 22 admitted sequences can spend (22 × the 5-request reservation).
  - **106** — projected from what those entries actually returned in July: 21 sequences
    spending the full reservation plus the single `401` entry, which fails on the
    modern-first probe and so costs one request. (Under the cheaper model this ADR uses
    elsewhere — an era-signalling `404` costing a probe plus one legacy step — the realistic
    figure is **43**. The pessimistic number is the one worth publishing.)

  All three are a large amount of unsolicited traffic to one third-party host, and all three
  are the direct, intended price of not refusing legitimate 22-entry groups. The alternative
  — a cap tight enough to bind here — refuses distinct registrants behind a shared suffix,
  which §3 rejects on the grounds that a control destroying the measurement it protects is
  not a safety property.
- **No HTTP client in the workspace can follow a redirect**, and `max_redirects: 0` is
  published in the policy block so the request counts can be read as the requests actually
  sent. See §1.
- **`census-pin-stability` and `probe-stage1` are paced too**, under the same policy and the
  same run-wide budget. `pin_stability` was the worst offender of the three by rate — two
  full sequences back to back, up to 8 requests at roughly 4 per second, against precisely
  the subset of hosts a prior sweep found *responsive* — and it is the sweep most likely to
  run next, since P0-06 and P0-07 both ran it on Stage 1's output as methodology.
  `probe-stage1` is the higher-consequence one, because its job is `tools/call` against live
  third parties; it reserves `probe::MAX_REQUESTS_PER_PROBE` (26) per protocol run rather
  than discovery's five, since one run is a handshake plus two `resources/read` sweeps plus
  two invocations, and admitting it against a five-request reservation would have left the
  cap soft for whatever ran next on that domain.
- **One hostile entry can no longer discard a whole sweep.** Every attempt runs under
  `catch_unwind` and a panic becomes a `harness_panic` record; the pacer's and the sweep
  pool's locks are read back with `PoisonError::into_inner` rather than `expect`, the one in
  `Permit::drop` most urgently, since a panic in a drop during unwinding aborts the process.
  Before this, `sweep::run_bounded`'s `thread::scope` re-panicked on join and unwound past
  the header insert and `finish()`: nothing written, and up to 1,000 hosts of third-party
  contact spent for no artifact. No reachable panic on server-controlled data is known, so
  this was latent — but it was a single point of total loss on a sweep about to run.
- **The pacing controls cannot be switched off by a value that looks like a default.**
  `--min-interval-ms 0` is rejected; zero spacing has to be spelled `1`, which is a visible
  number in the published header. A cap below the per-discovery reservation is rejected too,
  since it would refuse every host sampled.
- **Pacing changes a sweep's timing and nothing else.** An unpaced client is byte-for-byte the
  client P0-11 shipped, and every request-count test in `crates/discovery/tests/http_discovery.rs`
  still passes unchanged. The paced tests assert the same counts.
- **`Permit::before_request` is public**, so Track B's `probe` — the workspace's second
  JSON-RPC-over-HTTP client, and the one that actually *invokes* tools against live
  third-party infrastructure — can take the same control without a second implementation. It
  grants nothing: it sleeps and counts, and cannot send anything, so P0-01's structural
  guarantee that no caller outside `discovery` can name an MCP method is untouched.

---

## Flagged, recorded rather than fixed

**✓ `probe` (Track B) is now paced — this flag is closed.** It was the one crate whose job
is to invoke tools against live third-party servers, and B-01's own notes record
`401`/`429`/`400` arriving mid-protocol during its 500-server run. `ProbeClient::paced` takes
a `Permit`, `ProbeTarget` carries one, and `probe-stage1` admits its discovery sequence at
`MAX_REQUESTS_PER_DISCOVERY` and each of its two protocol runs at
`probe::MAX_REQUESTS_PER_PROBE` (26), under `PacePolicy::CENSUS_DEFAULT` — the same policy and
the same run-wide budget Stage 1 uses, so the two sweeps' conduct is comparable and a domain
cannot be hit hard by one because the other was polite to it. It publishes the same `pacing`
block, built by the same function, into `results/conformance/`.

**✓ `census-pin-stability` is now paced too**, which was not flagged and should have been: it
runs two full sequences back to back, against exactly the hosts a prior sweep found
*responsive*, and it is the sweep most likely to run next. One permit covers both of a
candidate's sequences, reserved at `2 × MAX_REQUESTS_PER_DISCOVERY`, which also keeps one
permit to one server record — the assumption §5's overshoot derivation depends on.

**⚑ `tools/list` pagination will overshoot the per-discovery reservation.** Pagination is
unfollowed today (P0-11's disclosed limitation) and adds *unbounded* requests when it lands.
The reservation assumes `discover()`'s five-request bound, so a paginating discovery would push
`charged` past `charged + reserved` and erode the cap for *later* hosts on the same domain.
Rather than guess a larger reservation, `Permit` counts the excess and the results header
publishes `overshoot_requests`, so the day pagination lands this appears as a non-zero number
in a results file instead of a quietly weakened control. Whoever lands pagination must revisit
the reservation.

**⚑ The delay is keyed on the registrable domain, which is broader than the task required.**
P0-12 asked for a delay within one host's sequence; keying it on the same unit as the budget is
strictly stronger (a host's requests all share its domain key) and costs one concept instead of
two. The price is paid by the shared-hosting over-grouping in §2: 22 `*.vercel.app` entries are
serialised against each other even though they are distinct registrants. At 500 ms and ~2
requests each that is ~22 s across a whole sweep, which did not seem worth a second key — but
it is a cost, and if the PSL upgrade in §2 ever happens it disappears.

**⚑ The 500 ms default is not a rate limit a server asked for.** It is a floor this project
chose from its own 429 history (three requests in one second failed; 500 ms permits two per
second; P0-11's hand-imposed floor was 1.2 s). Nothing reads a server's own published rate
policy, and `Retry-After` is still used only to *skip* a host within a run, never to sleep and
retry — P0-11's rule, unchanged.

**⚑ The budget is a politeness control, not an anti-abuse control.** Stated here as well as
in §2 because it is the thing most likely to be mistaken for a security property. Splitting
load across registrable domains is free by design, and no key derivation can fix that. The
adversarial bound is the per-run seed (§3a) plus recorded, attributable refusals — not the
key.

**⚑ Keying the budget on (publisher namespace, registrable domain).** Recorded in §3a as the
better answer to the victim-targeted starvation variant than tightening the key. Needs its own
task and its own review; it changes what every published per-domain row means.

**⚑ Following a redirect, if it is ever wanted, must be hop by hop at the call site.** The
design is in §1: each hop re-keyed through `admit`/`before_request`,
`save_redirect_history(true)`, a `final_url` identity field on the record, and a cross-domain
hop recorded as a refusal rather than silently followed. Recorded, not built.

**⚑ `RunHeader.evidence_store` publishes an absolute `/home/<user>/…` path** when
`--evidence-dir` or the environment variable is used. Pre-existing from P0-10, not introduced
here, and harmless for a local run — but it leaks an operator's username into a file intended
for publication, and a published sweep should carry a relative or redacted path instead.

**⚑ There is no overall sweep deadline.** Only per-request timeouts (12 s) and the free-disk
floor can stop a sweep. A corpus where a large fraction of hosts time out at 12 s has no
bound other than `candidates × requests × 12 s`, and the pacer's own spacing adds to it. The
July sweep finished well inside an hour so this has never bound in practice, but a sweep that
needs to stop at a wall-clock budget cannot be told to.

**⚑ `intake::registry`'s HTTP client is outside the `pace` agent, and still follows up to ten
redirects.** Deliberately not changed here. It is a different risk class: a single configured
first-party endpoint (`registry.modelcontextprotocol.io`) reached by GET, where following a
`Location` requires the registry itself to be hostile — and the registry's response already
determines every URL the sweep contacts, so the marginal exposure is small. Routing it through
`pace` would create an `intake → discovery` crate edge that architecture.md §8's component
split does not want, and tightening its redirects is an untested behavioural change to the
one client a sweep cannot start without. Worth revisiting with the crate-edge question, not
on the eve of a sweep.
