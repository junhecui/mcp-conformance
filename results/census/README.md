# Census results

The JSON files here are the curated, committed census artifacts. Every number in them is
re-derivable offline from the evidence store by `cargo xtask census-rederive`.

## `pacing`, and how to check it

A Stage 1 results file carries a top-level `pacing` block (P0-12,
[ADR-014](../../docs/adr/014-request-pacing-and-domain-budget.md)) holding both the policy the
sweep ran under — the minimum interval between consecutive requests to one registrable domain,
the per-domain request cap, the per-discovery reservation, `max_redirects`, and the named key
derivation — and what that policy actually produced: requests sent, time waited, domains
contacted, the most-loaded domain, refusals, overshoot, and the 25 most-loaded per-domain
rows. Each server record carries its own `pacing` (`registrable_domain`, `requests_sent`,
`waited_ms`, `reservation`, and — in files written after the October census — `waited_us`).

`max_redirects` is `0`, and it belongs with the policy rather than as trivia: a followed
redirect would be a request the accounting below it cannot see. No HTTP client in this harness
follows one, so the request counts in this block are the requests the sweep actually sent. A
server answering a 3xx is not followed; its status lands in the `transport` bucket with the
detail `http status: 30x`, the same bucket the July file's 15 `redirect failed` entries
already occupy.

The two halves are meant to be cross-checked rather than believed. Five integer header fields
must equal what the per-server records give: `requests_sent`, `sequences_denied_by_budget`,
`registrable_domains_contacted`, `overshoot_requests`, and — the one that substantiates "no
third party received more than N requests" — `max_requests_to_one_registrable_domain`, derived
by grouping per-server `requests_sent` by per-server `registrable_domain` and taking the
maximum.

The sixth, time waited, is a summed duration, and a millisecond total cannot in general equal
a sum of per-record millisecond truncations — the October Class B file
(`class_b_annotation_coverage_2026-10.json`) is correct and publishes 231,793 ms against a
record sum of 231,557 ms. So it is checked two ways, chosen by the file's own content:

- **Files written since that was fixed** also carry `pacing.waited_us_total` and a per-record
  `waited_us`, in whole microseconds. The pacer truncates each wait to microseconds once and
  adds the same integer to the run total and to the record's own counter, so the header must
  equal `Σ waited_us` **exactly** — 1 µs off is a failure — and every `waited_ms` must be the
  whole milliseconds of its own `waited_us`.
- **Files without `waited_us_total`** (the October file is one) can only be checked against
  what truncation allows: `Σ waited_ms ≤ waited_ms_total` and
  `1000 × (waited_ms_total − Σ waited_ms) < 999 × R + Q`, where `R` is the records with
  `requests_sent > 0` and `Q` is `Σ requests_sent`. This cannot see an unattributed wait that
  fits inside that slack — for the October file, up to roughly 700 ms beyond its observed
  236 ms gap.

Which check applies is decided by whether the microsecond fields are present, and a file
carries no harness version that would date it, so any file that drops every microsecond field
gets the legacy check.

This guard is a self-consistency check on what a file asserts, not an authenticity check. A
file with `pacing` absent or `applies: false` skips it, and `census-rederive` copies pacing
through verbatim rather than re-deriving it from evidence, because pacing is not recorded in
evidence. A header and records fabricated together, consistently, would pass.

A live sweep prints
`PACING ACCOUNTING INCONSISTENT` and keeps its data; `cargo xtask census-rederive` prints the
same and **exits non-zero**, because the offline audit path has nothing at risk and is the
path a reader auditing a published file uses.

`failure_category: "registrable_domain_budget_exhausted"` is a **harness-side** outcome, not a
finding about the server: that host's registrable domain had spent its request budget, so no
request was made to it at all. It is a bucket of its own precisely so a reader can subtract it
— `transport`, `protocol`, `rate_limited` and the tool-weighted fields mean exactly what they
meant in the July files beside it. A Stage 2 file carries `pacing: {"applies": false, …}`,
because it discovers over stdio to a local container and so has no HTTP request to pace.

**Comparing a paced sweep to July: add the refusals back first.** A refusal is subtractable
from the *total*, but against a per-category count it is a **displacement** — it replaces
whatever category that host would otherwise have landed in. Worked example, projected over the
committed July corpus: the success rate is unchanged at exactly 25.0%, because all 149
`pipeworx.io` entries were `protocol` failures in July, but `failure_categories.protocol` moves
**312 → 221**. A reader who sees that and asks *"SSE-only dropped by 91, did the ecosystem
change?"* has been misled: 91 of those hosts were never contacted. The refusal record cannot
say which category it displaced, because by construction nothing was learned about that host —
which is why the refused hosts are listed individually and why the July file is worth keeping
beside the new one.

`sample_seed` in the header is the seed the sample selection and admission order were drawn
under, as a hex string (never a JSON number — a `u64` above 2^53 would be silently rounded,
and a seed that does not reproduce its own sample is worse than no seed). Pass it back as
`--sample-seed` to reproduce a sweep's exact sample. It exists because a keyless hash made
sample membership and position precomputable offline, which made starving a chosen domain out
of the census practical; see ADR-014 §3a, including the measured finding that the registry
does not bind an entry's URL to its publisher's verified namespace.

`failure_detail` holds server-authored text in the general case and is length-bounded for that
reason; a budget-refusal detail is written by the harness.

## `evidence/` is untrusted and deliberately uncommitted

`evidence/` (git-ignored; see the repo root `.gitignore`) is P0-10's content-addressed
store of **unmodified third-party response bytes** — the verbatim `initialize` /
`server/discover` and `tools/list` payloads each server returned, addressed by SHA-256
digest with no file extension.

Those bytes are **evidence, never instruction**. Per design.md §3 the servers that produced
them are assumed actively hostile, and architecture.md §1 draws the trust boundary once:
everything arriving from a server under test is data. MCP responses carry server-authored
prose in fields like `instructions` and tool `description`, and some of it is written
imperatively, addressed at whatever model reads it — an established prompt-injection vector
(architecture.md §0, MCPTox). Committed into a public repository, these blobs would become
live payloads sitting exactly where a future agent session reads the tree as project
content. Hence: not committed.

Whether to commit a curated evidence bundle later is still open — measured cost is roughly
15.6 KB/server for Class B and 9.4 KB/server for Class A, so a few MB for a full sweep.
Ignoring it is the reversible default, not a decision against it.
