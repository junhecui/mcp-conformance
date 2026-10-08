# Implementation Backlog

**Derived from:** [architecture.md](architecture.md), which supersedes [design.md](design.md).
**Status:** Pre-implementation
**Last updated:** July 2026

This is the canonical task list. It is mirrored into GitHub Issues; every issue title
carries its task ID so the two can be reconciled mechanically. When they disagree, this
file wins.

---

## How to read this

- **Tracks** run in parallel. **Phases** are sequential and match architecture.md §10.
- `Depends on` lists task IDs that must be complete, not merely started.
- `Exit` is the observable condition that closes the task. If it cannot be observed, it is
  not an exit criterion and the task needs rewriting.
- Tasks marked **⚑ blocking** gate an entire phase.

```mermaid
graph LR
    F["Track F<br/>Foundations"]
    P0["Phase 0<br/>Census"]
    B["Track B<br/>Class B probe"]
    P1["Phase 1<br/>Thinnest verdict"]
    P2["Phase 2<br/>Deterministic core"]
    P3["Phase 3<br/>Network"]
    P4["Phase 4<br/>Hardening"]
    P5["Phase 5<br/>Audit"]
    Q["Track Q<br/>destructiveHint"]

    F --> P0
    F --> P1
    P0 --> B
    P0 --> P1
    P1 --> P2
    P2 --> P3
    P3 --> P4
    P4 --> P5
    B --> P5
    P2 -.evidence only.-> Q
    Q -.secondary.-> P5

    style Q fill:#FAEEDA,stroke:#854F0B
    style P0 fill:#EEEDFE,stroke:#534AB7
    style P5 fill:#E1F5EE,stroke:#0F6E56
```

---

## Track F — Foundations

Cross-cutting work that gates Phase 0 and Phase 1. None of it produces a finding; all of it
prevents rework later.

### F-00 Pinned Linux development and CI target

**Depends on:** —
**Exit:** A reproducible Linux environment (fixed kernel version, fixed overlayfs mount
options) that development and CI can both target, documented as such.
**Status:** Done — [ADR-010](adr/010-pinned-linux-environment.md). Target: a pinned Ubuntu
24.04 LTS ("Noble Numbat") cloud image, referenced by an exact dated build serial (e.g.
`releases/noble/release-20260705/` via Canonical's permanent archive, never the `release/`
symlink that repoints over time) rather than a dedicated bare-metal/cloud box — the
trade-off table in the ADR turned on the box option not actually solving F-00's own
"contributor local access" checklist item (it gives remote access to one shared machine, not
local access to a reproducible one) and reintroducing the "one sandbox at a time" concurrency
constraint architecture.md §7 already imposes at worker-pool scale, now at the individual
contributor's desk too. GA kernel series pinned at **6.8** (`linux-image-generic`, not the
HWE track, which deliberately jumps series over the LTS lifecycle — exactly the drift being
prevented).

**Provisioned 2026-07-27** (previously the ADR recorded only the decision, not an actual
boot — closed now, full record in
[ADR-010's "Provisioning record"](adr/010-pinned-linux-environment.md#provisioning-record)).
Lima 2.2.0 installed via Homebrew; serial `20260725` of the Noble arm64 cloud image fetched
from its dated (non-symlink) path, checksum
`sha256:2eaec7286c49fdea713dddabcf5012cafa7097a658e916acb48f4bc5fdc8e419` independently
verified both via `gpg --verify` against Canonical's UEC image signing key and via local
`shasum -a 256` against the signed `SHA256SUMS` — not asserted from a search result, per
this task's own requirement. Pinned into the new `.lima/mcp-conformance.yaml`. Booted
(`vz` driver) and all four prerequisite probes passed inside the VM: `uname -r` →
`6.8.0-136-generic` (GA track confirmed via `dpkg -l`/`apt-cache policy`, no HWE kernel
package present); `cgroup.controllers` lists `memory`/`cpu`/`pids`; six-namespace
`unshare --mount --uts --ipc --net --pid --user --fork` exits 0; an overlay mount using the
pinned `redirect_dir=off,metacopy=off,index=off` options against a throwaway lower/upper
pair mounts, reflects a pre-existing lower-layer file through the merge, captures a
post-mount write into `upper`, and unmounts cleanly. VM left running
(`limactl list` → `mcp-conformance ... Running`) for immediate follow-on use; stop with
`limactl stop mcp-conformance` when not in use.

Referenced as blocking in [ADR-007](adr/007-implementation-language.md) ("sandbox is
unbuildable on the macOS host by construction... this is now blocking") and in
`crates/sandbox/src/lib.rs` ("untestable on the primary development host. That is F-00's
problem") since the language decision was made, but never actually added to this backlog
until now — a real gap between the docs and the tracked work, found while scoping the
census staging below.

**Gates P1-02 onward, not Phase 0.** Stage 0/1 census (registry metadata; `initialize` +
`tools/list` against remote HTTP servers) touches no sandbox code. Stage 2 census (Class A
servers, which requires locally executing the server to discover it) needs *containment*
but not *this* — a stock container runtime is adequate for containment-without-observation,
per the reasoning in the P0-06/P0-07 notes below. F-00 is specifically about the
fixed-kernel-version precision that *observation-grade* overlayfs work (Phase 1+) needs,
which containerized execution without observation does not.

**CI gap, disclosed rather than papered over.** GitHub's hosted `ubuntu-24.04` runner label
pins the Ubuntu release, not the point-in-time kernel build inside it — GitHub updates
images under a stable label over time, and hosted runners don't expose nested
virtualisation/KVM, so CI cannot simply boot the pinned VM image itself today. The ADR
records a self-hosted-runner-on-the-pinned-image escalation path but deliberately does not
build it yet (no Phase 1+ sandbox code exists to need it). What ships now: F-03's existing
kernel-prerequisite probe step in `.github/workflows/ci.yml` prints `uname -r` so drift
against the ADR-010 pin is visible in every CI run's log rather than silent — it does not
fail the build on a mismatch, since the hosted runner was never claimed to satisfy the pin
exactly and there is no owned fallback yet to fail over to.

- [x] Choose the target: a pinned VM image (recommended — a container shares the host
      kernel, which defeats "pinned" if the host itself isn't fixed) vs. a dedicated
      bare-metal/cloud Linux box — VM image chosen, full trade-off table in ADR-010
- [x] Pin the kernel version and record it — Ubuntu 24.04 LTS GA kernel series 6.8
      (`linux-image-generic`), image referenced by dated archive serial
- [x] Pin overlayfs mount options — `redirect_dir=off`, `metacopy=off` (security-load-bearing:
      the kernel's own docs warn against `metacopy=on` with untrusted layers, which is
      exactly design.md §3's hostile-tool trust model), `index=off` (no layer reuse/export
      exists to protect, per architecture.md §4.1's "arms are never reused"), `userxattr`
      conditional on P1-03's not-yet-made privileged-vs-rootless mount choice
- [x] Document how a contributor gets local access matching CI's environment — Lima (or
      Vagrant+libvirt/qemu/UTM) booting the pinned, checksum-verified image locally on any
      host OS/architecture; arm64 natively for Apple Silicon contributors since none of the
      namespace/overlayfs/cgroups behaviour in scope is architecture-sensitive

### F-01 ⚑ Choose implementation language and workspace layout — ADR-007

**Depends on:** —
**Exit:** ADR-007 merged in `docs/adr/`, recording the choice and the rejected options.
**Status:** Done — [ADR-007](adr/007-implementation-language.md). **Rust**, edition 2024,
single Cargo workspace, hand-rolled MCP client.

architecture.md §8 deliberately leaves this open (`crates/ (or packages/)`). It cannot stay
open — the purity constraint in ADR-005 is materially easier to enforce in a language with a
real module/dependency graph, and the sandbox layer is Linux syscall work.

- [x] Evaluate against: syscall ergonomics (namespaces, overlayfs, cgroups, seccomp), ability
      to enforce the `normalise`/`verdict` purity rule statically, ~~MCP client library
      maturity~~ — **criterion struck.** SDK maturity points the wrong way: mature SDKs
      discard wire bytes during deserialisation, which breaks P0-01's byte-exact capture and
      therefore P0-02's pin, and they expose a tool-call method that P0-01 forbids
      structurally. Decision rests on the first two criteria.
- [x] Record the decision and consequences as ADR-007
- [x] Note explicitly which parts, if any, are permitted to be a second language — fixtures /
      mock backends / P4-05 hostile server (any language, never in the harness build graph),
      and `results/` analysis (Python). Everything else is Rust, `destructive` included.

### F-02 Scaffold the repository skeleton

**Depends on:** F-01
**Exit:** Every directory in architecture.md §8 exists with a placeholder module that builds.
**Status:** Done — `cargo build --workspace` and `cargo clippy --workspace --all-targets`
both clean on the macOS host.

- [x] `intake` `discovery` `census` `planner` `world` `argsynth` `sandbox` `observe`
      `integrity` `normalise` `verdict` `destructive` `store` `orchestrator`
- [x] `rulesets/` `fixtures/generic/` `fixtures/per-server/` `results/census/`
      `results/conformance/` `docs/adr/`
- [x] `sandbox` gated as Linux-only at the build level, not by runtime check —
      `#![cfg(target_os = "linux")]` at the crate root, so off Linux it compiles to an empty
      crate and the workspace still builds
- [x] **One crate added beyond architecture.md §8: `datamodel`.** Shared vocabulary, pure
      data types, no behaviour, `no_std`. It is what lets `normalise` and `verdict` name
      their inputs without reaching for an I/O crate. Named `datamodel` rather than `model`
      because "model" already means *language model* throughout these docs — and this crate
      sits inside the pure allowlist, where that ambiguity would be actively dangerous.
- [x] `xtask/` added for workspace tooling (hosts the F-04 check)

### F-03 CI: build, test, lint

**Depends on:** F-02
**Exit:** CI green on a trivial PR; Linux runner exercises the `sandbox` module.
**Status:** Done — `.github/workflows/ci.yml`, `ubuntu-24.04` (version-pinned, matching the
ADR-007 posture rather than `ubuntu-latest`). Triggers on push to `main` and on pull
requests. Local baseline confirmed clean before landing: `cargo build`, `cargo test`, and
`cargo clippy --workspace --all-targets -- -D warnings` all pass with zero warnings on
1.85.1.

- [x] Build + test + lint on every push — checkout → cache → `rustup show` (installs the
      pinned toolchain from `rust-toolchain.toml`) → build → test → clippy → `cargo purity`
- [x] Linux runner with the kernel features the sandbox needs (overlayfs, cgroups v2, user
      ns) — a dedicated smoke-check step probes all three (`/sys/fs/cgroup/cgroup.controllers`,
      `sudo unshare` across mount/uts/ipc/net/pid/user, a real overlay mount/unmount) and
      fails the build loudly if the runner image lacks one, rather than letting P1-03
      discover it silently later. Probes run under `sudo` deliberately — they check kernel
      *subsystem* availability, independent of the unprivileged-userns policy question,
      which is P1-03's design concern, not this check's.
- [x] Fail the build on warnings in `normalise` and `verdict` — implemented as
      `cargo clippy --workspace --all-targets -- -D warnings`, a strict superset of the
      minimum ask. Workspace is at zero warnings today; the one known future cost is that
      `unsafe_code = "warn"` (workspace lint) becomes a hard error once P1-03 adds real
      syscall code to `sandbox`, forcing an explicit `#[allow(unsafe_code)]` per block —
      consistent with this codebase's existing pattern of demanding explicit justification
      for risky code (the ADR-005 purity allowlist), so treated as intended friction rather
      than a defect.

### F-04 ⚑ Enforce the purity rule in CI

**Depends on:** F-02
**Exit:** A deliberately-added edge from `verdict` to `store` fails CI. Demonstrate it, then
revert.
**Status:** Done — `cargo purity`. Demonstrated: with `store.workspace = true` added to
`crates/verdict/Cargo.toml`, `cargo build --workspace` **succeeds** (the edge is legal Rust —
which is precisely why the check must exist) and `cargo purity` **fails with exit 1**, naming
the offending edge. Reverted; check green.

ADR-005 is the invariant that makes reproducibility real rather than aspirational.
architecture.md §8: *"If that edge ever appears in the dependency graph, reproducibility is
gone."* A rule nobody checks is a comment.

- [x] Dependency-graph assertion — implemented as an **allowlist**, not the denylist this
      task's wording implies. A pure crate's transitive closure must be a *subset* of
      `PURE_ALLOWLIST` (currently `{datamodel}`). Strictly stronger: a denylist passes for
      any I/O crate nobody thought to name.
- [x] Negative test proving the check fires — `xtask/tests/purity.rs`, 6 tests. Run against
      *synthetic* graphs, so proving the rule bites does not require leaving a broken edge in
      the tree. Includes the literal `verdict` → `store` case and unforeseen offenders
      (`tokio`, `reqwest`, `chrono`, …) that are caught without being enumerated.
- [x] Layer 2 — per-crate `clippy.toml` in `normalise` and `verdict` banning clock, path,
      file, and socket types. A crate can have an empty dependency tree and still call `std`.
- [x] Layer 3 — `#![no_std]` on `datamodel`, `normalise`, and `verdict`. Taking a clock as a
      dependency is not a mistake CI catches after the fact; it is code that does not link.
      For `normalise` this is provisional, per ADR-007 — P1-06 tests whether real path
      matching can stay `no_std`.

**Note for F-03:** `cargo purity` must be its own CI step, not a test. `cargo tree` inside
`cargo test` is a recursive cargo invocation contending for the same package-cache lock.

### F-05 Content-addressed evidence store

**Depends on:** F-01
**Exit:** Blob written, addressed by digest, read back byte-identical; re-writing identical
content is a no-op.
**Status:** Done — `crates/store::BlobStore`, plus `datamodel::Digest` (a pure 32-byte value
type; hashing itself lives in `store`, not `datamodel`, so `sha2` never enters the
`normalise`/`verdict` purity closure). Algorithm: SHA-256, chosen as the unopinionated
well-audited default since no doc mandates one. 8 unit tests, `cargo purity` still clean.

architecture.md §12 item 4 — stand this up *before* any sandbox work so Phase 1 evidence is
replayable from day one.

- [x] Content addressing over raw bytes — `put` hashes the input itself; the caller never
      chooses the address
- [x] Immutability enforced at the API level, not by convention — no update/delete method
      exists at all; writes go through a temp-file-then-rename so a partial write is never
      observable; a digest whose on-disk content doesn't match what the address claims
      (`get` or `put`) is a hard `StoreError::Corrupt`, never silently accepted
- [x] Local filesystem backend; object-store backend deferred to P5-01
- [x] `re-put of identical content is a no-op` proven, not assumed — the test revokes write
      permission on the store root after the first `put`, so a second `put` of the same
      bytes can only pass if it truly skips the write

### F-06 Metadata DB schema

**Depends on:** F-01
**Exit:** Migrations apply cleanly; all seven entities from architecture.md §6 present with
their foreign keys.
**Status:** Done — `crates/store::db`, SQLite via `rusqlite` (`bundled` feature, same
reproducibility posture as the pinned toolchain and F-05's blob store). One migration,
`crates/store/migrations/0001_initial_schema.sql`, applied by a ~20-line hand-rolled runner
(a framework would be pure overhead for one file). `FIXTURE`'s columns were unspecified in
architecture.md §6 — filled in and mirrored back into that doc in the same change. 7 tests,
including that FK enforcement actually rejects a dangling reference, that a second
`open_and_migrate` against the same on-disk DB doesn't re-apply the migration (checked via
the `schema_migrations` row count, not just "no error"), and that every invariant below is
exercised in both directions (accepted when it should be, rejected when it shouldn't).

- [x] `SERVER` `TOOL_SNAPSHOT` `RUN` `INTEGRITY` `EVIDENCE` `VERDICT` `RULESET` `FIXTURE`
- [x] `VERDICT` keys on `snapshot_id`, never `(server_id, tool_name)` — invariant 1;
      `verdict` has no `server_id`/`tool_name` columns at all
- [x] `VERDICT.reason_code` non-null whenever `outcome = 'unverifiable'` — invariant 3,
      enforced as a `CHECK` constraint
- [x] `VERDICT.embargo_state` and `VERDICT.disclosed_at` included now (§12 item 6 — cheap
      today, expensive in Phase 5)
- [x] `VERDICT` table is derivable and safe to truncate; `EVIDENCE` is not — `evidence` has
      `BEFORE UPDATE`/`BEFORE DELETE` triggers that hard-fail, mirroring F-05's blob store
      (no update/delete method there either) so immutability holds on both sides of the
      evidence/metadata split

### F-07 Canonical evidence-tree serialisation — ADR-009

**Depends on:** F-05
**Exit:** ADR-009 merged defining how a directory tree (the overlay upper layer) serialises
into the byte blob F-05's content-addressed store actually stores.
**Status:** Done — [ADR-009](adr/009-evidence-tree-serialisation.md). Bespoke, sorted,
**single-blob** format (`evtree1`): one capture is one `Vec<u8>` handed straight to
`BlobStore::put`, matching architecture.md §6's one-`EVIDENCE`-row-one-`blob_ref` shape
exactly, rather than a git-tree-style multi-blob Merkle DAG (rejected — no repeated-history
use case here to amortise the extra indirection against; see the ADR's "On B specifically"
note) or an external tar/PAX format (rejected — classic `ustar` mtime resolution is 1 second,
lossy against the nanosecond-precision losslessness requirement). Entries are POSIX file
types generically (regular/directory/symlink/fifo/char-device/block-device/socket) plus a
full captured xattr set, sorted by raw path bytes with fixed-width big-endian fields — no
overlay-specific "whiteout" tag exists in the format itself, since a whiteout is fully
representable as a generic char-device entry with `dev_major=0`/`dev_minor=0`, and hardcoding
the special case would be interpretation, which architecture.md §3.1 forbids this layer from
doing (interpretation stays in `normalise`, per ADR-005).

Referenced in [ADR-007](adr/007-implementation-language.md) ("the primary evidence artifact
is a directory tree, not a byte string, and F-05's content addressing needs a tree format
before it means anything") and in `datamodel`'s `RawEvidence` doc comment ("shape is
deferred to ADR-009") since F-05 landed, but never actually added to this backlog until
now — found alongside the F-00 gap while scoping the census work below. Blocks P1-04
(Observation collector), the first component with an actual directory tree to harvest.

- [x] Decide the serialisation format (git-tree-like, tar-like, or bespoke) and its
      reproducibility properties — ordering, mtimes, whiteouts; design.md §9's overlayfs
      semantics apply directly — bespoke chosen; full options table and whiteout/opaque
      handling in the ADR
- [x] The format must stay lossless per `RawEvidence`'s existing contract: *"capture is
      lossless... discarding [mtimes/inode data] at capture time is normalisation, and
      ADR-005 requires normalisation to be a pure function of stored evidence"* — mode,
      uid/gid, nanosecond mtime, inode, dev major/minor, full xattr set, and complete file
      content are all retained verbatim; nothing is filtered or rounded at capture time
- [x] Two independent captures of the same directory tree must serialise to byte-identical
      output — this is what makes F-05's content addressing meaningful for tree evidence,
      not just single blobs, the same property P1-02 proves for the base layer itself —
      follows directly from the format's forced sort order and the absence of any
      capture-time-clock/PID/hostname field; the ADR states precisely how this is a
      narrower, cheaper claim than P1-02's own (harder) construction-reproducibility
      property, and how P1-02 reuses this format's digest-comparison test rather than
      inventing its own

---

## Phase 0 — Census

No sandbox code. Ships a publishable finding on its own (ADR-001), which is what inverts the
project's risk profile.

**Staging, added once P0-01–P0-05 landed and it became clear the phase decomposes further
than P0-06/07/08 originally implied:**

- **Stage 0 — registry-metadata census.** `registry::fetch_all` → `catalogue::ingest` →
  `classify::classify` → `census::coverage::tally`, over `server.json` documents alone.
  **Zero contact with any third-party MCP server** — the Class A/B/`unclassifiable` ratio
  needs nothing else. This decouples P0-08 from P0-07: the ratio does not need the full
  annotation census to exist first, only the registry listing, so it can and should ship
  before Stage 1/2 do.
- **Stage 1 — Class B annotation census.** `initialize` + `tools/list` over HTTP against
  live remote (Class B) servers. No local execution, no tool calls — exactly what any MCP
  client does on connect. Needs politeness controls (rate limiting, a `User-Agent`
  identifying the study, honest failure/timeout reporting per P0-07's own requirement).
- **Stage 2 — Class A annotation census.** Same as Stage 1, but discovering a Class A
  (locally-launchable) server means executing it (`npx`, `uvx`, a container image, ...) to
  speak stdio to it — this is execution, unlike Stage 0/1. It needs *containment* (don't let
  an adversarial server hurt the host) but, critically, **not the observation** the Phase
  1+ sandbox exists for (no changeset, no noise floor — census reads only what
  `tools/list` says, never what the tool does). A stock container runtime is adequate
  containment for that narrower job, and using one for Stage 2 does not compromise the
  hand-rolled-sandbox decision in ADR-007, which is about observation quality, not
  containment per se. Record how each server was discovered (bare host vs. containerized)
  as provenance on the result, so census data is never silently pooled across the two the
  way ADR-002 already forbids pooling across oracles.

### P0-01 ⚑ Discovery client

**Depends on:** F-02
**Exit:** `initialize` + `tools/list` succeeds against both a stdio server and a remote HTTP
server; raw JSON persisted verbatim.
**Status:** Done — `crates/discovery`. Hand-rolled JSON-RPC 2.0 (per ADR-007; no `rmcp`),
`serde`/`serde_json` for wire encoding and `ureq` (rustls, no native-tls/openssl) for the
HTTP transport. Verified against the live MCP spec before implementing rather than assuming
a revision: current stable is `2025-11-25` (a `2026-07-28` revision is in release-candidate
status and removes the `initialize` handshake entirely — a shape change for this whole
module, not a version bump; flagged for O-01, not addressed here). 15 tests: unit tests for
JSON-RPC framing and response routing (id mismatch, JSON-RPC error objects, malformed JSON,
peer-closed-without-responding — all via a real bidirectional `UnixStream` pair, not a
mock), plus true end-to-end integration tests against a real spawned subprocess (stdio) and
a real hand-rolled `TcpListener`-based HTTP/1.1 server (no external network calls, fully
hermetic and CI-reproducible). Re-ran 5x locally to rule out flakiness in the
thread/socket-timing-sensitive tests.

- [x] stdio transport — newline-delimited JSON-RPC over a spawned child's stdio; the
      `Child` is reaped on drop (kill + wait) so a discovery target that never exits can't
      accumulate as a zombie across a census run
- [x] Streamable HTTP transport — the single-JSON-response case; a server that upgrades to
      `text/event-stream` gets a clear rejection rather than silent mishandling (out of
      scope for this thinnest path, not silently broken)
- [x] Raw response captured **byte-exact** before any parsing — the pin depends on this;
      `Discovery.initialize_raw` / `.tools_list_raw` are the untouched wire bytes, and
      nothing in this crate parses them any further than routing the JSON-RPC envelope
      (id, result vs. error) to decide success/failure
- [x] Must not call any tool. Enforce structurally, not by discipline — the `Transport`
      trait (the only thing that can send an arbitrary MCP method string) is `pub(crate)`;
      nothing outside this crate can name it. `DiscoveryClient::discover` is the only
      public entry point, and its three method names (`initialize`,
      `notifications/initialized`, `tools/list`) are literals in its body, never parameters
- [x] Record the negotiated spec revision into `TOOL_SNAPSHOT.spec_revision` — taken from
      the server's own `initialize` response, not the version the client asked for

### P0-02 ⚑ Metadata pinner

**Depends on:** P0-01, F-05
**Exit:** Pin is stable across repeated discovery of an unchanged server, and changes when
any byte of a tool's name, schema, annotations, or description changes.
**Status:** Done — `crates/discovery::pin`, built on P0-01's byte-exact capture and reusing
F-05's `datamodel::Digest`/SHA-256 (one digest format across the system, not a second one
invented here). Every field is kept as `serde_json::value::RawValue` end to end — never
routed through `serde_json::Value`, whose `BTreeMap`-backed object type would silently sort
keys back into canonical order on re-serialisation and defeat the whole point. Per-tool pin
is a hash-of-hashes over `(name, inputSchema, annotations, description)`, each with a
presence marker byte so an absent field can never collide with a present-but-empty one; the
server pin hashes the per-tool pins in response order, so a server that reorders its own
tool list between two discoveries changes its pin too. 8 tests, including the literal exit
line ("reorder JSON keys → pin changes") and one that documents rather than papers over a
real boundary: serde's `Option<T>` collapses an explicit JSON `null` and an absent key
before `RawValue` ever sees either, so this module can't and doesn't try to tell them apart.

Defends against rug pulls (architecture.md §0). *"A verdict without a pin is meaningless."*

- [x] Per-tool hash over `(name, inputSchema, annotations, description)` as received
- [x] Per-server hash over the tool set
- [x] **No normalisation before hashing** — the pin is over bytes, not semantics
- [x] Test: reorder JSON keys → pin changes. That is correct behaviour, not a bug.

### P0-03 Catalogue ingest

**Depends on:** F-02
**Exit:** A registry entry resolves to either an installable artifact or an endpoint, with
provenance recorded.
**Status:** Done — `crates/intake::catalogue`. Targets the real, current schema (verified
via web search rather than assumed): the official MCP Registry's `server.json` format,
schema `2025-12-11`, `packages[]` (npm/pypi/cargo/nuget/oci/mcpb) and `remotes[]`
(streamable-http/sse), which the spec explicitly allows to coexist on one entry. `ingest`
takes already-fetched bytes — fetching from a live registry is a separate, later concern
this task's contract doesn't include. 9 tests, including one against the official schema
doc's own minimal example rather than an invented fixture.

- [x] Resolve registry entries to source, package, image, or HTTP endpoint — every
      `packages[]`/`remotes[]` entry with its required fields becomes a `ResolvedTarget`;
      both arrays are processed, not just whichever one is checked first
- [x] Must not execute anything, including package install scripts — there is no code path
      in this module that spawns a process or invokes a package manager; it only parses JSON
- [x] Unresolvable entries are recorded, not dropped — `ingest` returns `IngestOutcome`
      directly (never wrapped in a `Result`), so there is no `Err` arm a caller could
      discard; a malformed sub-entry (e.g. a package missing `identifier`) is recorded in
      `skipped` rather than sinking an otherwise-resolvable entry, and an entry with zero
      usable targets becomes `Unresolvable` rather than an empty `Resolved`

### P0-04 Containability classifier

**Depends on:** P0-03
**Exit:** Every corpus server carries Class A, Class B, or `unclassifiable`, plus the reason.
**Status:** Done — `crates/intake::classify`. Reuses `datamodel::ContainabilityClass`
(already scaffolded in F-02) rather than a second parallel type, so classification can
never drift from what F-06's `SERVER.containability_class` column and its `CHECK`
constraint accept. A package target always wins over a coexisting remote — the registry
schema explicitly allows `packages` and `remotes` on the same entry, and "launchable
locally" only needs one to be true. 6 tests, including one that constructs the
"resolved but empty" state directly (bypassing `ingest()`'s own invariant that `Resolved`
implies a non-empty target list) to prove the classifier doesn't blindly trust an
invariant it can't see enforced — it degrades to `Unclassifiable` rather than panicking.

architecture.md §12 item 3 — the Class A/B ratio gates how ambitious Phase 5 can be.

- [x] Class A: launchable locally — any `ResolvedTarget::Package`, regardless of what else
      the entry also declares
- [x] Class B: remote HTTP endpoint only — `ResolvedTarget::Endpoint` present, no package
- [x] `unclassifiable` is a real class, not a fallback — never guess — every classification
      traces to a concrete fact from `catalogue::ingest`'s output (an `Unresolvable` reason,
      or the target list's actual contents), never to absence of information defaulting
      silently to one class
- [x] Reason string stored alongside the class — always human-readable prose; no closed
      reason-code taxonomy at this layer the way the verdict engine has one (P2-11)

### P0-05 Coverage aggregator

**Depends on:** P0-02
**Exit:** Per annotation, per tool, per server: `explicit` / `defaulted` / `absent`.
**Status:** Done — `crates/census::coverage`. `Defaulted` (the tool's `annotations` object
exists but omits this key) and `Absent` (no `annotations` object at all) are kept as
separate buckets rather than collapsed into one "not set" — both reach the same spec
default from a client's point of view, but they're different findings for design.md's open
question 1 ("mismatch" vs. "absence"). Per-server and corpus-wide rollup are the same
`tally()` function applied to different-sized input slices — a tally has no notion of a
server boundary, only the caller's choice of which tools to include does, so a second
rollup function would have been pure duplication. 6 tests, including one that proves the
corpus-wide claim directly: tallying two servers separately and tallying their concatenated
tools together produce the same combined counts.

- [x] Distinguish explicitly-declared from spec-defaulted from wholly absent
- [x] Must not touch behavioural evidence — operates only on the `tools/list` response's
      `annotations` objects; no dependency on `sandbox`, `observe`, or `store` anywhere in
      this module
- [x] Roll up to per-server and corpus-wide — one `tally()` function, scope is just which
      tools you pass it

### P0-06 Seed corpus run — 100 servers

**Depends on:** P0-04, P0-05
**Exit:** Census completes over 100 servers; pin stability and coverage taxonomy validated
against hand inspection of a sample.
**Status:** Done — both halves complete.

**Class A half** (this being the half that was open): `cargo xtask census-stage2-class-a
100` against 100 live Class A (locally-launchable) servers sampled from the registry by the
same stable-hash selection Stage 1 uses, executed via Docker (`docker run`, `--memory=256m
--pids-limit=256 --cpus=1`, no other flags) rather than the hand-rolled Phase 1+ sandbox —
per the Staging note above, Stage 2 needs containment, not the observation machinery that
sandbox exists for, and a stock container runtime is adequate for that narrower job.
`results/census/class_a_annotation_coverage.json`, every record tagged
`"execution_provenance": "containerized_docker"` so this can never be silently pooled with
Stage 0/1 (registry-only / Class B) data. **57 succeeded (57.0%), 43 failed, 956 tools
discovered.** Failure breakdown: `io_or_timeout` 36 (real npm/uvx crashes — bad Node-engine
requirements, ESM/CJS mismatches, missing required env vars, wrong `uvx` entry-point names
— and genuinely hung servers the watchdog killed, indistinguishable at the transport layer
from each other, reported as one honest category rather than guessed apart), `protocol` 5,
`unsupported_registry_type` 2 (`nuget` — no container invocation built for it; `cargo` and
`mcpb` would land in the same bucket, not silently skipped from the sample). npm and pypi
are the only registry types this run's container invocations cover (`npx`/`uvx` inside
`node:22-alpine` / `ghcr.io/astral-sh/uv:python3.12-alpine`) plus `oci` (image reference run
directly) — the actual Class A registry-type mix turned out to be npm-dominant (see the
100-entry sample pulled while scoping this: 30 npm, 2 oci, 1 pypi), so this covers the
overwhelming majority of the class as it exists today.

Two real bugs found running this against live, arbitrary, unauthenticated third-party
package code rather than only fakes — same discipline the Class B half's commit already
established:

1. `ChildProcessTransport` had no timeout at all — a hung or deliberately stalling
   containerized server would block `recv_line` forever. Fixed by adding
   `DiscoveryClient::stdio_with_timeout` / `ChildProcessTransport::spawn_with_timeout`
   (`crates/discovery/src/{client,transport}.rs`): a watcher thread sends the child a `kill
   -9` after a 45s deadline. Regression test added:
   `discover_is_unblocked_by_the_watchdog_when_the_server_never_responds`
   (`crates/discovery/tests/stdio_discovery.rs`), against a new `hang` mode in the fake
   stdio server that never responds — asserts on wall-clock time, not just the error
   variant, so a regression that silently dropped the watchdog would hang the test rather
   than pass it.
2. That same watchdog's `kill -9` targets the *host-side `docker run` CLI process*, not the
   container. A SIGKILL'd CLI process gets no chance to tell the daemon to honour `--rm`,
   so the container it launched keeps running, orphaned. Found by hand: `docker ps` after
   the first full run showed four containers still `Up 3 hours` from timed-out attempts in
   that exact sweep. Fixed in `xtask/src/class_a_stage2.rs` by giving every attempt a unique
   `--name` and calling `docker rm -f` on it unconditionally after every attempt, success or
   failure — independent of whatever state the CLI process or container ended up in. This
   is a host-resource-hygiene bug, not a data-correctness one: it affects leftover
   containers, not what `tools/list` returned, so the published coverage numbers above
   (from the run that found the bug, before the fix) are unaffected and are being kept
   rather than discarded — a re-run to regenerate them with the fix already in place would
   be re-doing Stage 1/2 work for a cosmetic reason, and the fix itself is what matters for
   every run after this one. All orphaned containers from that run were found and removed by
   hand before this was closed out.

Hand-verified 5 of the 57 successful servers (245 tools total) by re-invoking their
container directly and comparing raw `tools/list` JSON against the recorded
[`census::coverage`] classification: `tiktapdown-mcp` (npm, 4 tools), `unreal-engine-mcp
-server` (npm, 23), `mcp-slack-crunchtools` (pypi, 15), and `@arielbk/anki-mcp` (npm, 91) —
all four tool-count-for-tool-count matches, and all had no `annotations` object on any tool
(uniformly `Absent`, matching the raw JSON exactly) — plus `mcparmory-apify` (pypi, 112
tools), which exercised the interesting case directly: every tool has an `annotations`
object but with varying keys (e.g. `create_actor` → `{"openWorldHint": true}` only,
correctly `Defaulted` for `readOnlyHint`/`destructiveHint`/`idempotentHint` and `Explicit`
for `openWorldHint`), and the per-tool `readOnlyHint` split (54 explicit + 58 defaulted, 0
absent) sums exactly to its recorded `tool_count`. Nothing needed fixing.

- [x] Hand-verify the taxonomy on a sample of Class A results, the same way the Class B
      half required — done above: 5 servers, 245 tools, spanning both the uniformly
      `Absent` case (4 servers) and the `Explicit`/`Defaulted` mix case (`mcparmory-apify`),
      every tool-count and per-tool classification matched the raw JSON by hand.
- [x] Every result record carries `execution_provenance` — `"containerized_docker"` on all
      100 attempts, checked as a `CHECK`-equivalent by inspection of the output file rather
      than a separate assertion; there is no code path in `class_a_stage2.rs` that emits a
      server record without it.
- [x] No bare-host execution path exists for Class A discovery — the only program this
      module ever spawns is `docker`; `npx`/`uvx` and the target package run *inside* the
      container it constructs, never on the runner host.

**Class B half** (done previously): `cargo xtask census-stage1 100` against 100 live Class B servers,
`results/census/class_b_annotation_coverage.json`. 26 succeeded (74 failed — mostly `401`,
i.e. auth required, plus 14 genuine SSE-only servers correctly out of this transport's
declared scope; see that commit for the full breakdown), 289 tools discovered. Two real
bugs were found and fixed by running this against live servers rather than only fakes:
`HttpTransport` was missing `text/event-stream` from its `Accept` header (a spec violation
that got 14 servers spuriously rejected with `406`, not a real reachability problem), and
`RegistryClient::fetch_all` had no way to stop early once a caller had enough matching
entries. Both fixed and tested before this data was produced.

architecture.md §12 item 1. This is Stage 1/2 work (see the Phase 0 staging note above) —
it needs real `initialize`/`tools/list` exchanges with live servers, not just registry
metadata.

- [x] Hand-verify the taxonomy on ≥20 tools; fix the taxonomy, not the data — 47 tools
      inspected by hand across three servers (`cargo xtask dump-tools`), spanning all three
      coverage states. 34 were uniformly `Absent` (no `annotations` object at all — matches
      the raw JSON). 13 from a fourth server exercised the interesting case directly:
      `search_docs` declares `readOnlyHint`/`idempotentHint`/`openWorldHint` but omits
      `destructiveHint` → correctly `Explicit`×3 + `Defaulted`×1; `warmup_docs_cache`
      inverts that (declares `destructiveHint`, omits `readOnlyHint`); `openWorldHint:false`
      on `get_start_path` correctly classifies as `Explicit`, not confused with absence.
      Every record checked matched by hand. Nothing needed fixing.
- [x] Re-run discovery on the same 100 and confirm pins are stable — done against the 26
      that actually succeeded (`cargo xtask census-pin-stability`,
      `results/census/pin_stability.json`): each re-discovered twice, back to back. 26/26
      stable, 0 unstable, 0 failed to re-discover. Caveat noted in that commit: this is
      immediate back-to-back re-discovery, not separated by real elapsed time, so it
      confirms the pinning mechanism is deterministic given identical bytes, not that pins
      survive longer-interval drift or deployment churn.

### P0-07 Full census — ≥1,000 servers

**Depends on:** P0-06
**Exit:** Coverage numbers over ≥1,000 servers written to `results/census/`. **Publishable.**
**Status:** Class B half done — `results/census/class_b_annotation_coverage.json`, 1,000
servers, `cargo xtask census-stage1 1000`. **250 succeeded (25.0%), 3,183 tools
discovered.** Class A half explicitly **not attempted** — out of scope for the work that
closed out P0-06. Stage 2 (containerized `docker run` execution) now exists and is
validated at the P0-06 seed scale (100 servers, see above), so the tool to do this run
exists; scaling it to ≥1,000 servers the way Stage 1 scaled is deliberately left as a
separate step, per the same validate-then-scale discipline the Class B half already
followed (100 before 1,000) — this is real, until it's actually run.

Sampling was fixed before this ran, not after: taking "the first N" Class B candidates in
registry order clusters under whichever namespace sorts first alphabetically (every earlier
sample was all `a*` prefixes) — replaced with a deterministic hash-based selection over the
*entire* Class B population, so the sample is unbiased with respect to namespace while
staying reproducible run to run.

This is the Phase 0 exit criterion and plausibly the headline result — open question 1 asks
whether the story is about *mismatch* or about *absence*. Stage 1/2 work, same as P0-06.
The `absent` bucket is identical (1,548) across all four annotations at this scale, same as
the 100-server sample — 48.6% of discovered tools never engage with the annotation system
at all, which is itself the strongest signal toward *absence* over *mismatch* so far.

- [x] Throughput profile suitable for the corpus size — sequential requests (never
      concurrent — unrelated third-party hosts, no reason to burst them), 12s per-request
      timeout tuned down from the 30s default after the 100-server sample showed responsive
      servers answer in low seconds; 1,000 servers completed well inside an hour
- [x] Failure/timeout rate reported alongside the coverage rate — full categorized
      breakdown in the results file and that commit: dominant reasons are `401` (auth
      required, reachable but access-gated) and genuine SSE-only servers (out of this
      transport's declared scope, not a failure of it), plus a long tail of real-world
      causes (dead hosts, malformed third-party JSON-RPC, TLS misconfiguration) — verified
      category by category before treating the data as valid, per that commit

### P0-08 Class A / Class B ratio report

**Depends on:** P0-04 — *not* P0-07, as originally listed. See "Staging" under Phase 0
above: the ratio needs only registry metadata (Stage 0), never a `tools/list` exchange with
any server, so it does not need to wait for the full annotation census to exist. The
dependency on P0-07 in the original phasing conflated "publish the ratio" with "publish it
*alongside* the full coverage numbers," which is a presentation choice, not a data
dependency.
**Exit:** Ratio published with the census.
**Status:** Done — `results/census/registry_class_ratio.json`, generated by
`cargo xtask census` against the live registry on 2026-07-26. **18,664 distinct servers**
(after fixing a real bug found in the process — the client's first run, before filtering to
`version=latest`, returned every historical version of every server as a separate entry:
59,484 records for 18,664 actual servers, some names over 1,000 times). **Class A: 9,526
(51.0%). Class B: 8,294 (44.4%). Unclassifiable: 844 (4.5%).**

This runs the *opposite* direction from the design note below: a slight majority of the
registry is locally launchable, not remote-only. Worth stating plainly alongside the number
wherever it's cited: this measures what a `server.json` entry *declares* as installable, not
whether that install path actually works — Stage 2 (executing Class A servers to discover
them) will be the first check on whether the declaration holds.

architecture.md §2 design note: if most public servers are remote-only, *"most of the
ecosystem is unauditable by any third party"* is a stronger claim than a mismatch rate. The
data says the opposite is true, which is itself worth publishing.

---

### P0-09 `server/discover` fallback for MCP spec `2026-07-28`

**Depends on:** P0-01
**Exit:** `DiscoveryClient` successfully discovers a server that speaks only the
`2026-07-28` handshake, with the discovery path (`initialize` vs `server/discover`) recorded
as provenance on the result — a second, orthogonal provenance axis alongside Stage 2's
bare-host-vs-containerized flag.
**Status:** Done — `crates/discovery/src/client.rs` (plus `lib.rs`, `transport.rs`, and
new tests in `crates/discovery/tests/`). `DiscoveryClient::discover()` falls back to a
`server/discover` JSON-RPC call when `initialize` fails with either `DiscoveryError::Io`
(stdio: no response at all) or `DiscoveryError::ServerError { code: -32601, .. }` (explicit
"method not found"). On the fallback path it skips `notifications/initialized` (no
equivalent lifecycle notification exists for the new handshake) and proceeds straight to
`tools/list`. A new `DiscoveryPath` enum (`Initialize` | `ServerDiscover`) is recorded on
the `Discovery` result struct — the provenance checklist item — and
`negotiated_spec_revision` is sourced from whichever response actually succeeded, never
stale data from a failed attempt. The `initialize_raw` field name was kept as-is rather than
renamed (it now sometimes holds `server/discover` response bytes instead); its doc comment
was updated to point readers to `discovery_path` to disambiguate — a known, disclosed minor
wart, not a bug. P0-01's structural guarantee is preserved: `Transport` stays `pub(crate)`,
`discover()` remains the only public entry point with no method-name parameter, and
`"server/discover"` is a compile-time literal alongside the other three method names, never
caller-influenced. P0-02's pin mechanism needed no changes — confirmed `pin_tools()` only
ever consumes `tools_list_raw`, never `initialize_raw` or anything discovery-mechanics
-related.

**A real bug was found and fixed during review**, in the same spirit as this file's
convention (P0-06, B-01) of calling out bugs surfaced by testing against realistic
conditions rather than only fakes: the first implementation classified *any*
`DiscoveryError::Transport` (which covers every HTTP-level connection failure — DNS,
connection refused, TLS, and critically, hitting the configured request timeout) as a
fallback trigger. Since `xtask/src/census_stage1.rs` deliberately tunes a 12s HTTP timeout
to keep unresponsive hosts from dominating sweep time at corpus scale (per P0-07's own
numbers: 1,000 servers in under an hour), this meant every genuinely unresponsive Class B
host would eat a second full 12s round-trip to the same unreachable endpoint before
`discover()` gave up — silently doubling the cost of exactly the failure population that
dominates census sweeps, for zero benefit, since a connection-level failure can't produce a
different outcome on retry to the same host. Fixed by narrowing the fallback trigger to
`Io` and the explicit `-32601` case only, excluding `Transport` entirely. A regression test
(`discover_does_not_fall_back_after_a_transport_level_timeout` in
`crates/discovery/tests/http_discovery.rs`) proves a transport-level timeout now surfaces
promptly with exactly one connection attempt, not two.

Test suite: `crates/discovery` has 28 tests passing (up from the prior count), covering:
successful `initialize` (unchanged happy path, now also asserting
`discovery_path == Initialize`), fallback via `-32601` over stdio (real spawned subprocess)
and over HTTP (real `TcpListener`-based server, also verifying the negotiated version
propagates onto the `MCP-Protocol-Version` header of the subsequent `tools/list` request), a
negative test proving fallback does NOT trigger on an unrelated `ServerError` code
(`-32000`), and the transport-timeout negative test above. Verified clean:
`cargo build --workspace`, `cargo test --workspace` (113 passed),
`cargo clippy --workspace --all-targets -- -D warnings`, and `cargo purity` (discovery was
never on the purity allowlist and nothing here changes that). Reviewed by two independent
subagents (a correctness pass and a security pass) before landing — the security pass found
no issues: fallback is a single bounded retry, not a loop; timeout/watchdog protection is
inherited unchanged from the existing transport implementations on both paths; `-32601` is
parsed via typed serde deserialization with malformed responses failing safe
(`DiscoveryError::Protocol`, never a panic or a false-positive fallback); and no
untrusted server-controlled string ever influences which method gets sent.

**Caveat, disclosed rather than hidden:** the exact wire shape of the `server/discover`
request was extrapolated from the O-01 research note (mirroring `initialize`'s
`protocolVersion`/`capabilities`/`clientInfo` params), since no authoritative example
request/response pair was available. The new spec's
`_meta["io.modelcontextprotocol/protocolVersion"]` mechanism is not yet threaded onto
post-handshake requests like `tools/list` — only the existing HTTP header mechanism is used.
A real `2026-07-28` server that requires the `_meta` field would currently fail after a
successful `server/discover`. This is exactly why the last checklist item below is left
unattempted rather than checked off on faith.

- [x] Attempt `server/discover` when `initialize` gets no response, or an error indicating
      an unrecognized method, instead of treating that as a bare discovery failure
- [x] Record which handshake path succeeded as provenance on the result
- [x] `TOOL_SNAPSHOT.spec_revision` (already captured per P0-01) reflects whichever revision
      was actually negotiated, regardless of which handshake produced it
- [ ] Re-run against a real `2026-07-28` server once one exists in the wild, not just a
      hand-built fixture, before trusting this at census scale — blocked on such a server
      existing; none does yet as of this writing

### P0-10 Census evidence persistence, server-weighted metrics, offline re-derivation

**Depends on:** P0-06, P0-07, P0-09, F-05
**Exit:** Census runs persist raw evidence, report server-weighted metrics and spec-revision
provenance, and every number can be re-derived offline from stored bytes.
**Status:** Done — both mandated review passes came back clean (nothing blocking), the five
fixes they agreed on are applied, and the smoke runs were performed by the manager rather
than an implementer subagent, per the rule that anything contacting live third-party
infrastructure is the manager's to run.

Why: the July census (P0-06/P0-07) kept corpus tallies and a per-server `tool_count` only —
no raw `tools/list` bytes. It is tool-weighted, a few large servers dominate it, and design.md
open question 1 asks about *servers*; none of that could be re-sliced without re-contacting
every server. It also predates MCP `2026-07-28`, so it records no handshake provenance.

- [x] Both raw responses (`initialize_raw` — which may hold `server/discover` bytes, see
      `discovery_path` — and `tools_list_raw`) of every successful discovery written to F-05's
      `BlobStore`; each server record carries both digests and byte counts. Store root
      `results/census/evidence/`, overridable by `--evidence-dir` or
      `MCPCONF_CENSUS_EVIDENCE_DIR`. The JSON stays useful without the blobs: per-server
      tally, tool count, pin, revision and path are all inline (`xtask/src/census_report.rs`)
- [x] One derivation path: `census_report::build_report` computes every number from bytes
      read back out of the store, reusing `census::coverage::tally` (summed per server via new
      `AddAssign` impls, proven equal to tallying the concatenation). `cargo xtask
      census-rederive <results.json>` reruns it offline and compares byte-for-byte; tests
      prove a live run — including one through real `DiscoveryClient`s over stdio, the
      bounded pool at `jobs = 3`, and the sweep's own `finish` — reproduces exactly, and that
      an edited number, a missing blob, or a pre-P0-10 file is caught loudly
- [x] Per-server tallies; top-level `server_weighted` block (% of servers declaring tools
      whose tools carry an `annotations` object on none / some / all; per annotation, % with
      ≥1 tool explicit and with every tool explicit). Tool-weighted `tools_discovered` /
      `annotation_tally` unchanged in meaning, so July and October compare directly
- [x] Per-server `negotiated_spec_revision` (parsed by the same function the live client
      uses) and `discovery_path`, with top-level distributions under `provenance`
- [x] Stage 2 only: `--jobs N` (default 1, capped at 8; Stage 1 rejects the flag — it stays
      sequential per P0-07). Unique `--name` per container (sweep id includes the PID) and
      unconditional `docker rm -f -v`, 45 s watchdog unchanged, output in candidate order
      regardless of completion order
- [x] Stage 2 disk hygiene (`xtask/src/image_hygiene.rs`): an `oci` candidate's image is
      removed only if its reference was absent before the sweep and it resolves to an image
      ID outside a pre-sweep `docker image ls` snapshot (plus the wrapper images); removal
      happens when the last in-flight attempt on that reference finishes, under a lock, with a
      final pass for removals Docker refused or late pulls; failures are non-fatal and
      reported in the results header's `image_hygiene` block. Wrapper images are kept by
      design. Free-space floor of 3 GiB guards the repo, evidence, and Docker-storage volumes
      (on Docker Desktop under WSL, `/mnt/c`; override `MCPCONF_DOCKER_DATA_PATH`); below it
      no attempt launches and the file says `"complete": false` with the reason.
      containerd-snapshotter caveat documented in-module; multi-platform removal is verified
      by re-inspection, not assumed
- [x] Hostile-input protections unchanged (10 MiB recv cap, timeouts, watchdog, pin failures
      non-fatal, no bare-host execution, `execution_provenance`), hash-based sampling
      unchanged; server-controlled text in results is now length-bounded, and digests read
      back from a results file are parsed strictly (`Digest::from_hex`) before becoming paths
- [x] Smoke runs (manager) and two independent reviews (code + security) — `census-stage1 5`
      reached 3 of 5 sampled servers and `census-stage2-class-a 3 --jobs 2` reached 2 of 3;
      `census-rederive` then reproduced **both** live results byte-for-byte from stored
      evidence (`cmp` clean), which is this task's central claim. Both sweeps ran with
      `--out`/`--evidence-dir` pointed outside the repo, so `results/` was never written and
      the July data stands untouched — which also exercised item 1's override paths rather
      than leaving them untested. Image hygiene was validated against real pre-existing host
      state: `postgres:latest` and `pgvector/pgvector:pg17` both survived, no container
      leaked, and the two wrapper images were pulled and kept by design. Evidence volume,
      measured for the still-open commit decision: ~15.6 KB/server (Class B), ~9.4 KB/server
      (Class A)

**Review outcome and carry-forward findings.** Both mandated review passes ran as separate
subagents (a pure code review and a security specialist, per CLAUDE.md's orchestration
model) and neither found anything blocking. Five fixes they agreed on were applied:

1. **`docker run` flag parsing is now terminated with `--`** before the image operand in all
   three `docker_args` arms (`xtask/src/class_a_stage2.rs`). Containment previously rested on
   an accident of *ordering* — a registry-supplied `identifier` is the last argv element, so a
   flag-shaped value like `--privileged` left `docker run` with no image operand and merely
   errored. That margin was one token wide, and the obvious next feature closes it: the
   dominant Stage 2 failure cause recorded in P0-06 is a missing required env var, so the
   registry's `environmentVariables`/`runtimeArguments` are the natural thing to forward, and
   appended in the natural place (with the other `docker` flags, ahead of the image) an entry
   named `--privileged`, or `-v` plus `/:/host`, would have been immediate and total
   containment loss. Regression test:
   `a_flag_shaped_oci_identifier_lands_after_the_flag_parsing_terminator`.
2. **The evidence store is genuinely git-ignored** (`/results/**/evidence/`). None of
   `.gitignore`'s three prior patterns reached it — `/evidence/` is root-anchored, `raw/` is
   the wrong leaf name, and a blob is addressed by a bare 64-hex digest with no extension — so
   verbatim third-party bytes sat untracked-but-un-ignored in a public repo's working tree, one
   `git add -A` from being committed. That is not tidiness: those bytes include `instructions`
   prose written imperatively at a model (a separate research pass found three of five probed
   public servers returning exactly that on the first connect-level request), which committed
   would become a live prompt-injection payload sitting where a future agent session reads the
   tree as project content. `results/census/README.md` now fences the directory in prose as
   evidence-never-instruction.
3. The watchdog doc comment in `crates/discovery/src/transport.rs` no longer claims a liveness
   check that does not exist (see below).
4. A `private_intra_doc_links` rustdoc warning (links to `pub(crate)` items in private
   modules) removed by dropping the link brackets. Promoting the modules would not have
   cleared it — the linked items are themselves `pub(crate)` — and widening that API surface
   is out of this task's scope.

Deliberately **deferred**, recorded here rather than left in a review nobody reads again:

- **The stdio watchdog can fire after the child is reaped** (`crates/discovery/src/transport.rs`).
  The detached thread sleeps the full timeout and then runs `kill -9` unconditionally — no
  liveness check, no cancellation — which is why the smoke run printed
  `kill: (14571): No such process` twice (the benign branch: `Drop` had already reaped the
  child). PID reuse is possible in principle; quantified on this host as needing ~2,200
  process creations/second sustained against `pid_max` 99999 inside the 45 s window, roughly
  three orders of magnitude beyond what a sweep generates — so remote, but real, and `--jobs`
  raises exposure up to 8×. Blast radius if it ever fired: a SIGKILL'd sibling `docker run`
  producing a spurious `io_or_timeout` **attributed to the wrong server**, which is a
  data-quality failure this project treats seriously. Fix shape: a shared "reaped" flag taken
  under one lock across {check, kill} and {set, wait}.
- **No panic safety around container cleanup** (`xtask/src/class_a_stage2.rs`).
  `cleanup_container` and `hygiene.release` are plain statements, not `Drop` guards, so an
  unwind leaks a container (P0-06's bug, returning) and leaves a reference `in_flight`
  forever; and a worker panic propagates out of `thread::scope`, discarding the whole results
  file. No panic reachable from server-controlled input was found, so this is an unconfirmed
  robustness gap, not a live bug.
- **Container hardening not applied.** Containers join the default bridge with unrestricted
  egress (reaching the LAN and host-published ports), and there is no `--cap-drop=ALL`,
  `--security-opt=no-new-privileges`, `--user`, or `--read-only`. Phase 0 deliberately accepts
  stock-container containment (see this phase's Staging note), but each of these is one argv
  element.
- **`--out`'s volume is not disk-guarded**, unlike `--evidence-dir` and the Docker volume.
- **`bounded()` limits characters, not bytes**, so a 2,048-character multi-byte value can
  reach ~8 KB. The bound still holds; the test's tolerance only passes because its flood is
  ASCII.
- **`docker image rm` for a multi-platform image under Docker Desktop's containerd
  snapshotter is unexercised.** The code mitigates by verifying removal via re-inspection
  rather than trusting exit status.
- **Docker Desktop's WSL CLI injection is fragile across Docker restarts.** `/usr/bin/docker`
  survives as a symlink while its target mount (`/mnt/wsl/docker-desktop/cli-tools/...`) does
  not, so every invocation fails with "could not be found in this WSL 2 distro" rather than a
  connection error; repairing it needs the distro restarted. Worth knowing precisely because
  a sweep that loses Docker mid-run would surface as a wave of per-server `io_or_timeout`
  failures — a host problem misread as an ecosystem finding, which is the exact misattribution
  this task's provenance work exists to prevent. A sweep interrupted this way should be
  discarded, not published.

---

## Track B — Class B protocol-probe oracle

Runs after Phase 0, independently of the sandbox. Narrow exception carved out in
architecture.md §2.

### B-01 Protocol-probe protocol

**Depends on:** P0-04
**Exit:** probe → invoke → probe decides `readOnlyHint` for a Class B server that exposes
resources or state-reflecting read-only tools.
**Status:** Done — new crate `crates/probe` (not in architecture.md §8's original list;
added the same way `datamodel` was added beyond §8 in F-02, noted there for the same
reason). Surface is MCP **resources** only (`resources/list` + `resources/read`) — the
"state-reflecting read-only tool" half of architecture.md §2's exception is explicitly out
of scope for this pass, since using a tool-as-probe would need real semantic argument
synthesis (P2-06, unbuilt) rather than B-01's own placeholder-only
`probe::synthesize_arguments`. A resources/read snapshot is taken before and after
invocation and hashed (`probe::snapshot`); `probe::protocol::assess_read_only` /
`assess_idempotent` are pure decision functions over two digests, unit-tested against the
full truth table (holds/violated/unverifiable × declared true/false) with no network
involved. `probe::ProbeClient` (`crates/probe/src/client.rs`) is a second, separate
JSON-RPC-over-HTTP client from `discovery::DiscoveryClient` — deliberately: P0-01's
contract is "must not call any tool," enforced structurally by never exposing a
method-taking call, and this crate's entire job is to call one. Reuses
`discovery::jsonrpc::encode_request`/`encode_notification` (promoted from `pub(crate)` to
`pub` for exactly this) rather than duplicating correct JSON-RPC framing, but does not and
cannot reuse `discovery::transport` (stays `pub(crate)` to `discovery`, untouched). 45 new
unit/integration tests across `probe`'s five modules, including true end-to-end tests
against a real `TcpListener`-based fake HTTP server (same technique `discovery`'s own HTTP
tests and `intake::registry`'s tests already use) that drive `probe_read_only_hint`/
`probe_idempotent_hint` through every branch of the decision table.

- [x] Identify servers with a usable probe surface; the rest stay `unverifiable` —
      `probe::snapshot::discover_surface` treats a `resources/list` JSON-RPC "method not
      found" (-32601) or an empty list as `ProbeSurface::None`, propagating any other
      failure as a real error rather than silently guessing. A run that never reaches
      resources at all, and one whose invocation itself fails (placeholder arguments
      rejected — design.md §8's "semantic argument validity" limitation, landing here as a
      concrete `invocation_failed` reason code), both degrade to `Unverifiable` with a
      distinct reason code, never a forced verdict.
- [x] Extend to `idempotentHint` where the probe surface supports it —
      `probe::probe_idempotent_hint` runs invoke → probe (`s1`) → invoke again with
      identical synthesised arguments → probe (`s2`), and `assess_idempotent` decides from
      `s1` vs. `s2` alone (architecture.md §4.2's `D1`/`D2` shape, without the noise-floor
      or restart arms — see the caveat below on why not).

**Validated against live data**, not just synthetic tests: `cargo xtask probe-stage1 500`
against 500 live Class B servers sampled from the registry by the same stable-hash
methodology P0-06/P0-07 established (so the sample is comparable to, not a different
methodology from, the census data). 128/500 discovered successfully (25.6% — consistent
with P0-07's 25.0% over a similarly-sized Class B sample, cross-validating that this sample
isn't systematically different from the census one). Of those, 127 had ≥1 declared tool and
were probed (the 128th hit a genuine third-party bug: a `tools/list` response whose
`result` was valid JSON-RPC but didn't actually contain a `tools` array — found by this run
crashing the whole sweep the first time, since `discovery::pin_tools` fails hard on that
shape and the script originally propagated it with `?`; fixed to record it as a
`pin_failed` discovery-failure category and move on, per the "ASSUMED HOSTILE" trust model
— a malformed-but-not-erroring response is exactly the kind of thing that model predicts,
and one bad server must never abort a 500-server sweep).

Exactly one probe protocol per tool, exactly one tool per server (the first one declared),
to bound both third-party request volume and the number of real invocations — see the
crate's own doc comment for why that bound exists (this is the one crate in the workspace
whose job is to invoke tools against live infrastructure this project doesn't control).
254 probe protocols run (127 servers × 2 protocols). Of those, 207 produced a verdict; 47
hit a hard `ProbeError` mid-run (24 for `readOnlyHint`, 23 for `idempotentHint` — mostly a
`401`/`429`/`400` arriving between the surface-discovery call and the invocation, or a
malformed non-JSON-RPC response to a call that should have succeeded) and produced no
verdict at all, logged as a failure rather than coerced into one. Every one of the 207
verdicts carries `oracle = protocol_probe` (checked directly against the database, not just
trusted from the code — see B-02).

Outcome breakdown (`results/conformance/track_b_protocol_probe.json`,
`results/conformance/track_b_probe.sqlite3`):

| annotation | holds | unverifiable | violated |
|---|---|---|---|
| `readOnlyHint` | 5 | 96 | 2 |
| `idempotentHint` | 7 | 96 | 1 |

The dominant outcome is `unverifiable` (96/103 and 96/104) — expected and correct, not a
weak result: most Class B servers either don't support `resources/list` at all, or support
it but expose nothing the probe can use to decide a `false`-declared tool's behaviour
(§ "probe_surface_incomplete" in `probe::protocol`). A harness that mostly returned `holds`
here would be the "worst available failure mode" design.md §8 warns about, one layer up.

**Hand-verified a sample of the decisive (`holds`/`violated`) outcomes** — 8 servers total,
checked directly against the live server outside the harness (raw `curl` against
`initialize` + `resources/list`), not just re-read from the database:

- `racecalendar.app` (`get_f1_season_schedule`, declared `true`/`true`) and
  `proflightsearch.com` (`get_airport_delay_status`, declared `true`/`true`) both `holds`
  for both annotations — plausible on its face (a schedule/status lookup with no
  observable side effect) and the kind of case this oracle is supposed to catch cleanly.
- `api.mnemom.ai` (`claim_agent`, declared `false`/`false`) — `holds` for both, i.e. state
  *did* change — consistent with a tool literally named "claim" not being read-only.
- **A real, useful negative finding**: `mcp.pricetik.com`'s `pricetik_search` (declared
  `readOnlyHint: true`) came back `violated`. Manually inspecting the server's
  `resources/list` shows its resources are `ui://pricetik/deal-grid` etc. —
  MCP-UI *render templates* (`mimeType: text/html;profile=mcp-app`), not durable state.
  These very plausibly re-render with each tool's own output baked in, meaning a "search"
  tool changing the rendered deal-grid content is expected UI behaviour, not evidence
  against `readOnlyHint`. **This is a real limitation of B-01 as built, found by hand
  -verification rather than assumed**: the probe surface is "any listed resource,"
  undifferentiated between state-reflecting resources and UI-render resources, and the
  latter is close to guaranteed to look mutated after any successful tool call regardless
  of true idempotence or read-only-ness. Recorded here rather than quietly fixed, in the
  same spirit as design.md §8's other named limitations (external state invisibility,
  normalisation sensitivity, the caching confound) — a candidate refinement for whoever
  picks up B-01 next is restricting the probe surface to resources whose `mimeType` isn't
  a UI-render type, or requiring a resource to be read-stable across two immediate reads
  with no intervening call before trusting it as a probe surface at all.
- `api.isittrustready.ai`'s `get_agent` (declared `readOnlyHint: true`,
  `idempotentHint: true`) also came back `violated` for both. Its resources
  (`mnemom://catalog/values`, `mnemom://rubric/reputation`, `mnemom://jwks`, ...) read as
  genuine reference/state data, not render templates — unlike the pricetik case, this one
  does not have an obvious methodological explanation and is left as a plausible real
  finding, not confirmed further (a live trust-scoring API updating something after an
  agent lookup is not an implausible mechanism). Flagged here as *not conclusively
  resolved* rather than asserted either way — an example of the class of finding
  disclosure (P5-03) would eventually need to route to the server's maintainer.

**Caveats, stated plainly:** (1) no noise-floor arm (ADR-003's control) and no
restart-interleaved caching check — architecture.md §4.2's full multi-arm protocol assumes
independent runs from a byte-identical base, which a live third party offers neither of;
Track B's protocol is deliberately the simpler two-/three-probe version, and the
pricetik/isittrustready findings above are exactly the kind of ambiguity that gap leaves
open. (2) One tool probed per server, not every tool — a scale/politeness bound, not a
claim that the untested tools on a probed server behave the same way. (3) Sample size is
127 probed servers out of an ecosystem of thousands; treat the outcome table as indicative,
not a headline mismatch rate the way P0-07's census numbers are.

### B-02 Oracle tagging

**Depends on:** B-01, F-06
**Exit:** Every verdict carries `oracle` = `kernel_changeset` or `protocol_probe`.
**Status:** Done — the `VERDICT.oracle` column and its `CHECK` constraint already existed
(F-06); this task was entirely about populating it correctly, per the task brief. Added
`crates/store/src/db.rs::{ServerRecord, ToolSnapshotRecord, VerdictRecord}` plus
`insert_server`/`insert_tool_snapshot`/`insert_verdict`/`list_verdicts` — the first typed
row-insertion API this schema has ever had (every prior row, in every F-06 test, came from
hand-written raw SQL). `datamodel::{Oracle, Outcome, Annotation}` each gained
`as_db_str`/`from_db_str`/`Display`, so the exact TEXT written for `oracle` can never drift
from what `VERDICT`'s `CHECK` constraint accepts — the mapping lives in one place
(`datamodel`), not re-derived at every call site. 2 new `store::db` tests, including one
that inserts a `protocol_probe` verdict and a sibling `kernel_changeset` verdict on two
different snapshots and confirms both round-trip correctly and distinctly — the literal
exit criterion, exercised through the typed API, not just asserted against a raw string.

**Validated against the same 500-server run as B-01**: queried
`results/conformance/track_b_probe.sqlite3` directly (`SELECT DISTINCT oracle FROM
verdict`) — all 207 real verdicts this run produced carry `oracle = 'protocol_probe'`, with
no other value present. No Class A (`kernel_changeset`) verdicts exist yet in this database
or anywhere in the codebase — P1-07 (the Class A verdict engine) is still `todo!()`,
blocked on ADR-008/ADR-009 per its own status — so B-02's "every verdict carries an oracle"
claim is currently proven for the one oracle that exists in practice; the `kernel_changeset`
side of the `CHECK` constraint and of `insert_verdict`'s type signature is exercised only by
the `store::db` unit test referenced above, not yet by a real Class A run. That gap closes
naturally when P1-08 lands.

### B-03 ⚑ Cross-oracle aggregation guard

**Depends on:** B-02
**Exit:** Any report that mixes oracles without separating them fails a test.
**Status:** Done — `crates/store/src/aggregate.rs`. `aggregate()` is the only correct way
to build a report from raw verdicts in this codebase: it groups strictly by
`(annotation, oracle, outcome)`, so oracle is part of the grouping key by construction and
there is no code path that could merge two oracles' counts. The actual guard,
`verify_report_matches_records`, is independent of `aggregate()` on purpose — it takes an
arbitrary reported row (`ReportRow`, with an `Option<Oracle>` specifically so it can
represent the buggy shape a careless aggregator would produce) plus the raw source records,
and rejects a row whose oracle is undisclosed or whose count doesn't match exactly the
records sharing that one oracle.

ADR-002: *"an easy invariant to state and an easy one to violate in a summary table."* Made
literally a test, not a habit: `verify_report_matches_records_rejects_a_report_with_no_oracle_disclosed`
and `verify_report_matches_records_rejects_a_pooled_count_mislabelled_under_one_oracle`
(`crates/store/src/aggregate.rs`) each construct a deliberately mixed report — 5
`kernel_changeset` + 3 `protocol_probe` verdicts pooled into one count of 8, once with no
oracle disclosed at all and once mislabelled under `kernel_changeset` — and assert the
guard rejects both, with the specific `AggregationError` naming exactly what's wrong. A
third test (`verify_report_matches_records_accepts_aggregates_own_output`) proves the guard
never rejects `aggregate()`'s own correct output — the guard has teeth in both directions,
not just a rejection path with no corresponding acceptance path.

**Dogfooded against the real B-01 data, not only synthetic records**: `probe-stage1`
reads back all 207 verdicts this run wrote, converts them to `VerdictSummary` via
`store::db::VerdictRow`'s `From` impl, aggregates them, and calls
`verify_report_matches_records` on the result before the run is allowed to finish
(`xtask/src/probe_stage1.rs`) — it passed, as it must, since every verdict this run ever
produces is tagged `protocol_probe` by construction (`ProbeAssessment::ORACLE` is a `const`,
not a per-call field). The interesting adversarial case — an actual report that *does* mix
`kernel_changeset` and `protocol_probe` — can't be dogfooded yet for the same reason noted
in B-02: no Class A verdicts exist in this codebase until P1-08 lands. The guard is proven
against synthetic mixed data now and will get its first real mixed-oracle input the day a
Class A run and a Class B run land in the same report.

---

## Phase 1 — Thinnest verdict

Target from design.md §10: a real verdict on a real tool within two weeks. The anti-goal —
building the full containment stack before the first verdict — is what this phase exists to
prevent.

### P1-01 ⚑ Path taxonomy decision — ADR-008

**Depends on:** —
**Exit:** ADR-008 merged defining `user_state` / `server_internal` / `ephemeral`.
**Status:** Done — [ADR-008](adr/008-path-taxonomy.md). Rules are versioned glob lists in
`rulesets/`, not Rust code (P1-06's constraint, applied here). Default is `user_state`
(the conservative direction — misclassifying something as user state makes a tool look
*less* read-only than it is, never more, mirroring `unverifiable` over false `holds`).
Both allowlists (`ephemeral`: `/tmp/**`, lockfiles, PID files, sockets; `server_internal`:
XDG-style cache/config/state dirs, `__pycache__`, npm's cache layout) are seeded from
external naming conventions rather than invented, per ADR-003's discipline that an
unobserved pattern is a guess, not a finding. Explicitly left coarse and empirically
revisable by P2-10, and explicitly leaves room for the P2-05 world provisioner to override
the default classification for bespoke fixtures without specifying that interface yet.

architecture.md §12 item 2 — **this blocks ruleset v1.** §4.3 recommends emitting the verdict
against `user_state` while reporting the other two, so critics have something to argue with
that is not the verdict itself.

### P1-02 ⚑ Overlayfs base-layer builder

**Depends on:** F-02
**Exit:** Two independent constructions of the same base produce byte-identical layers.
Prove it in a test.
**Status:** Done — two new crates. **`crates/evtree`** implements the `evtree1` wire format
from [ADR-009](adr/009-evidence-tree-serialisation.md), which F-07 had merged as a decided
spec but never as code (`datamodel::RawEvidence` was still an empty placeholder). Built now
rather than inline, on the reasoning that P1-04 (`observe`) needs the identical
encoder/decoder later and this avoids duplicating a bespoke format twice. Pure,
dependency-free (`Cargo.toml`'s `[dependencies]` block is empty), no I/O/clock/network — not
yet on `cargo purity`'s allowlist, correctly, since nothing in `normalise`/`verdict` depends
on it yet. Implements every clause of the spec, checked field-for-field in an independent
review pass: entries sorted by raw path bytes (no locale-aware comparison is even reachable —
`Vec<u8>::cmp` has no such variant), fixed-width big-endian integers, generic POSIX entry
types with whiteouts as a plain char-device entry (`dev_major=0`/`dev_minor=0`) rather than a
dedicated tag, single-blob output, full xattr capture. `evtree::decode` is bounds-checked
throughout against malformed/truncated input — every length-prefixed read is checked against
remaining buffer size before slicing, pre-allocations are capped, no panic is reachable via
truncation — proven by a test that fuzzes every truncation offset of a valid encoding. 16
tests.

**`crates/world/src/base_layer.rs`** is the actual deliverable. `BaseLayerSpec`: a
declarative, `BTreeMap`-backed tree spec (`SpecNode::{Directory,Regular,Symlink,CharDevice}`,
the last covering both whiteouts via `SpecNode::whiteout()` and opaque directories via
`SpecNode::opaque_dir()`/`opaque_dir_userxattr()`, using the
`trusted.overlay.opaque`/`user.overlay.opaque` xattr convention). `to_entries()` converts a
spec to `evtree::Entry`s purely — fixed `uid=0`/`gid=0`/`mtime=0`/`inode=0` unless overridden,
no ambient state read — proven reproducible by encoding two independently-built,
differently-ordered specs and asserting byte-identical output. `materialize()` writes a spec
to real disk (whiteout/opaque char-device entries are intentionally skipped, since real
device nodes need `CAP_MKNOD`/root, which this builder deliberately doesn't require) and pins
mtime to `SystemTime::UNIX_EPOCH` via `std::fs::File::set_times` (stable since Rust 1.75,
inside this project's pinned 1.85.1 toolchain) — files pin inline via their open write
handle, directories are pinned in a required second pass after all children exist, since the
kernel bumps a directory's own mtime on every child creation and pinning inline would just
get overwritten by the next sibling. `capture()` walks a real directory back into
`evtree::Entry`s via `fs::symlink_metadata`, never dereferencing symlinks — structurally
immune to symlink-cycle recursion or escaping the walk root through a symlink, per security
review. The reproducibility proof
(`two_independent_real_constructions_produce_byte_identical_layers`) materializes the same
spec twice into two separate temp dirs, captures both, and diffs. Disclosed rather than
glossed: inode numbers are kernel-assigned and no userspace call can make two
independently-created files agree on one, so a `#[cfg(test)] pub(crate)` helper
(`strip_construction_noise` — deliberately not `pub`, see below) zeroes `inode` before the
final comparison; the test
separately asserts, on the raw unstripped captures, that mtimes for every non-symlink entry
already agree between the two builds and already equal the pinned epoch value, proving the
mtime pin actually works rather than hiding behind normalisation. Symlinks are the one
genuinely-unpinned exception — `std::fs::File::set_times` has no `AT_SYMLINK_NOFOLLOW` mode
and `File::open` on a symlink follows it — documented explicitly, and
`strip_construction_noise` strips mtime only for `Payload::Symlink` entries accordingly. 20
tests.

Two independent review passes (code-correctness and security) found and fixed three real
issues before this was considered done:

1. *(Security)* `materialize()` had no path validation — a spec entry with a leading `/` or a
   buried `..` component could write outside the destination directory via unnormalized
   `Path::join` semantics. Latent today (spec construction is programmatic), but certain to
   become live once P2-05's per-server fixture binding builds on this exact builder with
   less-trusted input. Fixed: `BaseLayerSpec::add()` now rejects empty paths, absolute paths,
   and any path containing a `..` component (checked per path segment, not by substring, so
   `a/../../etc/passwd` is caught as well as a bare leading `..`); `materialize()` also
   asserts `target.starts_with(dest)` immediately before every write, as defense in depth. 4
   new tests cover absolute paths, a leading `..`, a buried `..`, and legitimate nested paths.
2. *(Correctness)* The module doc originally claimed mtime couldn't be pinned because "`std`
   has no stable API for it" — independently verified false on this project's pinned 1.85.1
   toolchain. Fixed as described above, which is what let `strip_construction_noise` narrow
   from "inode and mtime" down to "inode always, mtime only for symlinks" — materially
   stronger than what originally shipped, and closer to the literal "byte-identical layers"
   exit criterion.
3. *(API hygiene)* `strip_construction_noise` was originally `pub fn`, reachable from any
   future crate depending on `world`, despite its own doc comment warning it "must never be
   reused as a stand-in for the real normaliser" — ADR-005 draws a hard boundary between raw
   evidence capture and the separate, pure `normalise` crate that this would have blurred.
   Fixed: now `#[cfg(test)] pub(crate) fn`, matching this codebase's established pattern of
   enforcing this class of boundary structurally (`cargo purity`'s allowlist,
   `discovery`'s `pub(crate) Transport`) rather than by comment alone.

**Not used:** the Linux dev VM provisioned for F-00 was deliberately not touched — nothing
here calls a Linux-only syscall; `capture()`/`materialize()` use only `std::fs` and
`std::os::unix::fs`, which macOS implements identically for the paths exercised (regular
files, dirs, symlinks). Whiteout/opaque-directory representation is proven at the format
level (synthetic `evtree::Entry` construction and round-trip), not via real
`mknod`/`setxattr` — real device-node creation needs `CAP_MKNOD`/root and is out of scope for
this builder, deferred to whichever task actually needs it (flagged in-code as future P1-04
scope, since `observe` will need to capture real device nodes/xattrs from an actual overlay
upper layer, unlike this synthetic base-layer builder).

**Verification:** `cargo build --workspace` clean; `cargo test --workspace` → 149 passed, 0
failed (up from 113 before this task; 36 new — 16 `evtree` + 20 `world`); `cargo clippy
--workspace --all-targets -- -D warnings` clean; `cargo purity` clean (`normalise`/`verdict`
dependency closures unaffected — neither `evtree` nor `world` is in them).

architecture.md §12 item 5 — everything downstream depends on this. A nondeterministic base
silently poisons every diff, and the failure is invisible in the output.

- [x] Deterministic construction: no timestamps, no random ordering, no ambient state —
      `to_entries()` fixes `uid`/`gid`/`mtime`/`inode` unless overridden; `materialize()`
      pins mtime to `UNIX_EPOCH` on disk rather than merely at the spec level
- [x] Byte-reproducibility test across two constructions —
      `two_independent_real_constructions_produce_byte_identical_layers`: two real,
      independently-materialized, differently-ordered on-disk trees capture and encode to
      identical bytes (inode stripped as kernel-assigned noise; mtime asserted equal, not
      stripped)
- [x] Whiteout and opaque-directory semantics understood and documented (design.md §9) —
      whiteouts as char-device `0/0` entries, opaque directories via the
      `trusted.overlay.opaque`/`user.overlay.opaque` xattr convention, both representable in
      `evtree1` and exercised by `SpecNode::whiteout()`/`opaque_dir()`; real kernel-level
      `mknod`/`setxattr` construction deferred, format-level round-trip proven instead

### P1-03 Sandbox supervisor — mount namespace, overlayfs, timeout

**Depends on:** P1-02
**Exit:** Tool launches inside a mount namespace over an overlay, is killed at timeout, and
tears down cleanly.

- [ ] Mount namespace + overlayfs upper layer
- [ ] Hard timeout
- [ ] MCP **client stays on the host**, server runs in the sandbox, protocol crosses over
      stdio (architecture.md §5 — keeps protocol handling outside the blast radius)
- [ ] Must not emit any verdict

### P1-04 Observation collector — upper layer

**Depends on:** P1-03
**Exit:** Upper layer harvested into the evidence store, content-addressed.

- [ ] Harvest and store; **interpret nothing**
- [ ] Record exit status and orphan-PID state for the gate

### P1-05 Integrity gate v1

**Depends on:** P1-04
**Exit:** Clean-teardown and timeout checks; a failed run yields `unverifiable` with a reason
code and no verdict.

ADR-004 — the gate is a hard precondition and **not configurable off**. A timed-out run with
an empty changeset is not `readOnlyHint: holds`; that is the worst available failure mode.

- [ ] `containment_uncertain` on orphan PIDs
- [ ] `timeout` on hard-timeout kill
- [ ] Not bypassable by configuration. Test that it cannot be disabled.

### P1-06 Normaliser + ruleset v1

**Depends on:** P1-01, P1-05
**Exit:** `(raw_evidence, ruleset_version) → canonical_changeset`, pure and deterministic.
**Status:** Done — semantics recorded in
[ADR-011](adr/011-normaliser-semantics.md). `crates/normalise` is now a real pure function:
`normalise(&RawEvidence, &Ruleset) -> Result<CanonicalChangeset, NormaliseError>`, plus
`CompiledRuleset`/`normalise_compiled` so one ruleset compiles once and serves a whole replay
batch. New `crates/normalise/src/glob.rs` (a small `no_std` matcher) and
`crates/normalise/src/tests.rs`; new `crates/store/src/ruleset.rs` (the loader, on the std
side); `rulesets/v1.yaml` carrying exactly ADR-008's two allowlists and nothing else;
`datamodel`'s three placeholder types (`RawEvidence`, `Ruleset`, `CanonicalChangeset`) filled
in along with the `PathClass`/`FileType`/`Node`/`ChangeKind`/`Change` vocabulary they needed.

**Built ahead of its own stated dependency, deliberately.** P1-06 lists P1-05 (integrity gate
v1), which does not exist — nor do P1-03/P1-04. Per `docs/HANDOFF.md` §4.5 this is a pure
function over inputs that are already settled (ADR-008's taxonomy, ADR-009's `evtree1`
format), so it needs neither Linux nor the gate to exist, and building it first shortens the
critical path to P1-08. The gate was **not** built or stubbed, per that same note. The
consequence, stated rather than buried: ADR-004's "only gate-passed evidence may reach the
verdict engine" is not yet expressible anywhere, because neither the gate nor the verdict
engine exists. That remains P1-05's and P1-07's to make a type-level guarantee.

**Decisions ADR-011 records**, each of which was only a code comment before it existed (and
which the code and `rulesets/v1.yaml` cite in 21 places across 9 files): the **base layer is a required second
input**, resolving the question HANDOFF §4.5(b) left open — upper-layer-alone would have been
sufficient for `readOnlyHint` but cannot separate a structural copy-up from a mutation, which
would make P2-08's noise floor an artefact of the missing input rather than a measurement of
the tool; **`RawEvidence` carries raw `evtree1` bytes and `normalise` decodes them itself**,
inverting ADR-009's own assumption that decoding would happen upstream, so that P1-09's replay
is literally `normalise(stored bytes, ruleset)` and totality over hostile bytes lives inside
the pure boundary; the overlay-convention mapping (whiteout → `Deleted`, opaque xattr of **any**
value → `DirectoryReplaced` with the base hidden beneath it, same-type → `Modified` with
`content_changed`/`metadata_changed` flags); the **structural-omission rule** — the only entry
ever dropped is an unchanged base directory with ≥1 upper descendant, with the invariant that
every omission is backed by at least one *reported* descendant; `mtime`/`inode` excluded from
the comparison and overlay-private xattrs stripped (capture stays lossless per ADR-009 — this
is the pure derivation step, re-runnable over the same stored bytes); **full content bytes
retained rather than a digest**, deviating from §4.5(c)'s wording because a weak hash is
attacker-collidable into a false `holds` and a strong one means a cryptographic dependency
inside the pure closure; and ruleset identity bound to the SHA-256 of the exact file bytes.

**Glob `**` matches zero-or-more path segments in every position, including trailing** — a
deliberate divergence from gitignore/`globset`, and **ADR-008 does not specify it** (it lists
only the eleven glob strings), so ADR-011 §7 is the only place that semantic is written down.
Without it, `**/__pycache__/**` would classify the `__pycache__` directory CPython creates on
first import as `user_state` while classifying every file inside it as `server_internal`.
Flagged in ADR-011 rather than slipped past: this is one of the few decisions in the project
that resolves *away* from ADR-008's conservative direction, justified by ADR-008's own
membership bar (a pattern's presence is a claim that matching it is *never* diagnostic, which
entails the same claim about the named directory itself), and revisable by P2-10 against
measured noise.

**Ruleset identity is tamper-evident.** `Ruleset::identity()` is `"v1+sha256:<hex>"`;
`store::ruleset::PUBLISHED` pins `rulesets/v1.yaml`'s exact SHA-256
(`48e55850…b63c1128`, verified against `sha256sum` on the file as committed), `load` refuses a
file claiming a published label whose bytes hash differently, `load_draft` refuses any file
claiming a published label, and the identity is carried onto every `CanonicalChangeset` — so a
label can never be reused over edited rules. Parsing is a hand-written strict-YAML-subset
parser on the std side, deliberately not a YAML library: the shape is fixed and tiny, and
flexibility in a file whose bytes are digest-pinned (anchors, flow style, implicit typing,
multi-document streams) is a liability rather than a feature. Anything outside the subset is
rejected with a line number instead of half-understood.

**`cargo purity`'s `PURE_ALLOWLIST` widened from `["datamodel"]` to
`["datamodel", "evtree"]`**, which is the trap HANDOFF §4.5(a) warned about, handled the way
it asked: `evtree` was converted to `#![no_std]` + `extern crate alloc` with `extern crate std`
gated to `#[cfg(test)]`, and its `[dependencies]` table is empty. F-04 is not weakened,
because the check is a transitive-closure *subset* test rather than a per-crate exemption —
allowlisting `evtree` permits that one package, not anything it might later acquire.
Demonstrated rather than asserted, in F-04's own style: adding `sha2 = "0.11.0"` to
`crates/evtree/Cargo.toml` makes `cargo purity` exit 1 with eight violations
(`normalise depends on cfg-if / const-oid / cpufeatures / crypto-common / digest /
hybrid-array / sha2 / typenum`), each named against `normalise`, not `evtree`. Reverted
byte-for-byte (checksums re-verified) and the check is green.

**`.gitattributes` added** (and broadened beyond the `rulesets/**` line P1-06 itself needed):
`-text` on `rulesets/**`, `fixtures/**` and `results/census/evidence/**`. All three are
byte-exact paths where a CRLF checkout silently changes a hash — the published ruleset digest
above, P1-02's byte-reproducibility property that P2-05's seed data will depend on, and
P0-10's content-addressed evidence blobs whose address *is* their content hash. The ruleset
parser also refuses `\r` loudly (`crlf_checkout_is_refused_loudly`) rather than mis-parsing, so
that one failure is visible even without the attribute.

**Verification:** `cargo build --workspace` clean; `cargo test --workspace` → **212 passed, 0
failed** (up from 156 on `main` at f1da15e, so **56 new**: 48 in `normalise` — 9 glob-dialect
tests plus 39 semantics/property tests — and 8 in `store::ruleset`; `evtree`'s 20 are unchanged
by the `no_std` conversion); `cargo clippy --workspace --all-targets -- -D warnings` clean;
`cargo purity` clean (`["normalise", "verdict"] depend only on ["datamodel", "evtree"]`).
Hostile input is covered by tests rather than argument: every truncation offset of a valid
capture is rejected (`every_truncation_of_valid_evidence_is_rejected_cleanly`), 6,000
bit-flipped and random byte strings never panic (`random_bytes_and_bit_flips_never_panic`), a
1,000-deep chain and 20,000 siblings complete quickly (`large_and_deep_trees_are_handled`), and
`no_exponential_blowup_on_hostile_inputs` pins the glob matcher's non-backtracking bound. The
structural-omission invariant is a property test over 2,000 random base/upper pairs
(`random_trees_satisfy_the_structural_omission_invariant`) with a floor on how many omissions
the generator must actually produce, so a generator that stopped exercising the branch fails
rather than passes quietly.

**Caveats, disclosed rather than hidden:**

- **Never run against a real overlay upper layer.** Every piece of evidence in these tests is
  synthetic `evtree::encode` output. Real whiteout device nodes, real `trusted.overlay.*`
  xattrs and a real copied-up directory tree first reach this code at P1-04/P1-08 — the same
  boundary P1-02 drew for its own synthetic whiteout/opaque handling. Expect the first real
  capture to find something.
- **Reproducibility is `(evidence, ruleset_identity)` *plus the `normalise` version*.** The
  `mtime`/`inode` exclusions, the glob dialect and the structural-omission rule are code, not
  ruleset data, and nothing in the schema records which `normalise` produced a verdict —
  `VERDICT` has `ruleset_version` and `protocol_version`; only `RUN` has `harness_version`.
  Raised as an open question in ADR-011 for P1-07/P5-02 to close while it is still cheap, in
  the same spirit as F-06 adding `embargo_state` early.
- **A copy-up with no observable difference reads as a mutation.** overlayfs copies a file up on
  a write-intent open even if nothing is written, so `Modified { content_changed: false,
  metadata_changed: false }` is reachable and is reported, not suppressed. That is the
  conservative direction and it is intended, but it is the likeliest source of a `violated`
  verdict a maintainer would dispute; P2-08 will quantify how often it happens.
- **Memory is `O(evidence bytes)` with file contents held more than once** — the price of
  retaining exact bytes instead of a digest. A tool that writes a 2 GiB file produces a capture
  that large and a derivation holding it roughly twice over, plus the decoded base. A size cap
  (yielding `unverifiable`, never a silent truncation) is a P1-04/P5-02 decision, not made here.
- **`evtree` has no per-crate `clippy.toml`** (F-04 Layer 2) the way `normalise` and `verdict`
  do. Under `#![no_std]` the banned types are not nameable outside `cfg(test)`, so Layer 3
  covers it today; if `evtree` ever relaxes to `std`, Layer 2 must be added in the same change.
- **P1-07 is untouched** — `verdict::read_only_hint` is still `todo!()`, with only its stale
  "blocked on ADR-008" message corrected. (Both mandated review passes have now run; see
  "Review outcome and carry-forward findings" below.)

- [x] Rulesets are versioned data in `rulesets/`, not code — `rulesets/v1.yaml`; `normalise`
      contains no path pattern of its own, only the matcher. The glob *dialect* is code, which
      ADR-011 §7 records explicitly as a divergence from this item's spirit rather than
      leaving implicit.
- [x] Reads nothing outside its inputs — no clock, no filesystem, no network — three layers,
      all green: `#![no_std]` (nothing to call), the per-crate `clippy.toml` bans, and
      `cargo purity`'s closure subset test, now re-demonstrated against the widened allowlist
- [x] Ruleset v1 kept deliberately thin; v2 gets derived from measured noise in P2-10 —
      exactly ADR-008's six `ephemeral` and five `server_internal` patterns, no speculative
      additions; `v1_loads_with_exactly_adr_008s_allowlists` asserts the lists verbatim so a
      speculative rule cannot be slipped in without a failing test and a new digest
- [x] Semantics written down where a reader can find them, not only in code comments —
      [ADR-011](adr/011-normaliser-semantics.md), covering all eleven decisions, the three
      rejected alternatives a reader would most likely have expected, and the open questions
      it deliberately does not close

**Review outcome and carry-forward findings.** Both mandated review passes ran as separate
subagents (a pure code review and a security specialist, per CLAUDE.md's orchestration model).
The code review found **nothing blocking**, and independently spot-checked ADR-011 against the
code at ~25 line citations, finding them accurate. The security review found **one blocking
issue**, since fixed.

**The blocking issue: overlay-private xattrs were stripped by *prefix*, not by name.**
`is_overlay_private` matched anything under `user.overlay.` or `trusted.overlay.`, and ADR-011
decision 5 justified that as removing kernel bookkeeping — which assumes every name under
those prefixes is kernel-written. `user.*` is not: POSIX gives that namespace to the file's
owner, and the tool under test owns everything it creates in its own upper layer. Confirmed by
mounting a real overlay with ADR-010's pinned options **in trusted mode — not `userxattr`**,
the mode the ADR had assumed safe: an unprivileged process writing through the merged mount
set `user.overlay.stolen` and `user.overlay.opaque` successfully, and the bytes reached the
upper layer verbatim. `normalise` then deleted them from `Node.xattrs` *and* ignored them in
`same_metadata`, so a copied-up directory carried `metadata_changed == false` and became
eligible for the structural-omission rule. Demonstrated end to end: a tool declaring
`readOnlyHint: true` stashes 448 bytes on a user-state directory and creates one
`scratch.lock`, giving `user_state = []`, `server_internal = []`,
`ephemeral = [/home/u/scratch.lock]` — **`readOnlyHint` reads as `holds` with the payload in
no partition at all**, which is strictly worse than the name-laundering gap recorded below,
where the change is at least still reported somewhere a reviewer can see it. The same
mechanism is a covert equality channel for `idempotentHint`: two runs differing only in a
`user.overlay.*` value normalise to equal changesets. **Fixed** by matching the finite set of
leaf names the kernel actually writes — `opaque`, `impure`, `origin`, `uuid`, `redirect`,
`nlink`, `upper`, `metacopy`, `protattr` — in either namespace and nowhere else, so an
unrecognised name is an ordinary xattr whose change is reported. The useful asymmetry is
preserved deliberately: *adding* `user.overlay.opaque` still gains an attacker nothing
(`classify_change` returns `DirectoryReplaced` before any omission branch and hidden
descendants become `Created`, both always reported, so it only makes a tool look *less*
read-only), while *removing* a real marker is the attack — and on the same live mount an
unprivileged tool could not remove the kernel's own `trusted.overlay.opaque` (`EPERM` through
the merged mount and directly on the upperdir), so that direction is closed by the kernel in
trusted mode and stays live only under `userxattr`. Three new tests pin it, and all three fail
against the old prefix behaviour: `every_real_overlay_private_name_is_stripped`,
`an_unrecognised_overlay_namespace_xattr_is_reported_not_stripped`, and
`an_unknown_overlay_xattr_cannot_launder_a_user_state_directory_out_of_the_changeset` (the
exploit shape above, asserted as `user_state == ["/home/u"]` rather than empty).

Three further fixes applied:

1. **The forged-component guard moved into the matcher.** Both reviews flagged it
   independently: `Glob::matches_path("/tmp/**", "/tmp/../home/secret")` returned `true` while
   `CompiledRuleset::classify` correctly returned `UserState`, because the guard existed only
   at `classify`'s own entry point — and ADR-011 decision 8 advertises it as a property of
   classification generally. Nothing was exploitable (nothing outside `glob`'s own tests called
   `matches_path`), but a `pub` API on the crate whose output *is* the verdict should not
   disagree with itself about a security property, and no test pinned the difference. The guard
   now lives in `Glob::matches_components`, which both public entry points go through — chosen
   over making `matches_path` `#[cfg(test)]` because `matches_components` is the function
   `classify` actually calls, so test-gating the other one would have left the hazard on the
   live path. The same change closes the related footgun the code review noted: a component
   containing a `/` is also refused, so `matches_components(&["a", "b/c/d"])` no longer matches
   `/a/*`. Pinned by `forged_path_components_never_match` and
   `classify_and_the_matcher_agree_on_forged_components`.
2. **ADR-011 corrected where the review proved it wrong** — decision 5's open question no
   longer presents the `user.overlay.*` risk as contingent on P1-03 choosing `userxattr` (it is
   not; see above), restates the dangerous direction as xattr *removal* rather than addition,
   records the live-mount evidence including the `EPERM` negative result, and notes the
   consequence for P1-03 (prefer the privileged mount; assert the upperdir is unreachable from
   inside the sandbox). The memory consequence's "roughly twice over" is corrected to a
   measured **~20×** worst case — 3.0× for one 16 MB file, 7.3× for 100k small entries, 4.0×
   for a 10,000-deep chain, 21× for a 1 MB path of nothing but `/` separators, because
   `classify` builds a per-entry `Vec<&[u8]>` of 16-byte fat pointers — still bounded and
   linear with no superlinear term, but the size cap deferred to P1-04/P5-02 must budget ~20×
   of input bytes, not 2×.
3. **`RulesetError::Syntax` no longer prints "ruleset line 0:"** against a field documented as
   1-based (`crates/store/src/ruleset.rs`). `line: 0` is the whole-file fault (not UTF-8, or a
   missing top-level key); the doc comment now says so and `Display` renders it as `ruleset:
   <message>`. Cosmetic, fixed because it was in reach.

Recorded rather than implemented, each with its argument so whoever picks it up does not have
to rediscover it:

- **⚑ Proposed amendment to ADR-008: classify on `(path, ChangeKind)`, not on path alone.**
  The name-based allowlists launder **destructive** changes, not only additive ones. A tool
  that *deletes* a base-layer file whose name matches an allowlist yields `user_state = []`,
  `ephemeral = [/srv/app.lock]` → `readOnlyHint: holds`. ADR-008's membership bar ("matching it
  is never diagnostic of a read-only violation") was argued with the example of a tool
  *touching* a lock file at startup; that the *creation* of a lock file is non-diagnostic does
  not entail that its *deletion* is. The hardening: let an allowlist suppress `Created` and
  `Modified` only, and send `Deleted`, `Replaced` and `DirectoryReplaced` always to
  `user_state`. It needs no change to the eleven glob strings and no new ruleset version.
  **Deliberately not implemented in P1-06** — it is a semantic change to ADR-008's taxonomy and
  deserves its own task and its own two review passes rather than being slipped into the
  normaliser. Also recorded in ADR-011's open questions.
- **A fixture-authoring constraint for P2-05.** Until that amendment lands, fixture
  *user-state* files must never be named `*.lock`, `*.pid` or `*.sock`, nor nested under
  `.cache`, `.config`, `.local/state`, `__pycache__` or `node_modules/.cache` — otherwise a
  tool can delete real fixture state and still be read as `holds`.
- **A category correction for P5-05.** ADR-008 files name-based laundering under design.md
  §8's "observation evasion", which §8 declares out of scope. That is a category error: §8's
  evasion requires the tool to *detect* the sandbox and change behaviour, whereas choosing a
  filename suffix is always-on, costs the tool nothing, needs no detection, and works on first
  contact against a harness behaving exactly as designed. It belongs in the published
  limitations as its own named limitation, not folded into evasion.
- **`VERDICT` records no normaliser version.** A verdict is reproducible from
  `(evidence, ruleset_identity, normalise version)` and only the first two are recordable
  today — `VERDICT` has `ruleset_version` and `protocol_version`, and only `RUN` has
  `harness_version`. The `mtime`/`inode` exclusions, the overlay-private name set, the glob
  dialect and the structural-omission rule are all code, not ruleset data. P1-07/P5-02 should
  close this while it is still cheap, in the F-06 spirit of adding `embargo_state` early.
- **`VERDICT.ruleset_version`: label or full identity? — and note it is a *two-table*
  decision.** The tamper-evident value is `Ruleset::identity()` (`"v1+sha256:…"`), not `"v1"`,
  and `CanonicalChangeset` already carries the identity. But `VerdictRecord::ruleset_version`
  is an FK to `RULESET.ruleset_version`, so storing the identity means the `RULESET` primary
  key must become the identity string too — not a one-column change. P1-07's to make, with the
  insertion path in front of it.
- **An abort during derivation produces no `VERDICT` row at all.** A `NormaliseError` has an
  `unverifiable`-with-a-reason-code path; a derivation job that dies (OOM against the ~20×
  multiplier above, a killed batch, a crash) does not — it simply leaves the row absent, which
  is indistinguishable from "not yet derived". P5-04 already reports the no-verdict fraction as
  a metric in its own right (ADR-004 predicts it may be large early); these aborts must be
  counted in it rather than silently dropping out of both the numerator and the denominator.

### P1-07 Verdict engine + `readOnlyHint`

**Depends on:** P1-06
**Exit:** `canonical(D1)` non-empty over `user_state` contradicts a `true` declaration.

- [ ] Pure: cannot take a model, network client, or clock as a dependency (F-04 enforces)
- [ ] Emits `holds` / `violated` / `unverifiable` with `reason_code` and `oracle`

### P1-08 ⚑ End-to-end: first real verdict

**Depends on:** P1-07, P0-01
**Exit:** One real `readOnlyHint` verdict on one real tool from one real MCP server, end to
end. **This is the Phase 1 exit criterion.**

### P1-09 ⚑ Replay test

**Depends on:** P1-08, F-05
**Exit:** An integration test regenerates the full verdict table from stored evidence plus a
ruleset version, executing no tool.

architecture.md §6 invariant 2: *"it is worth an integration test that literally does it."*
This is the property that makes ruleset iteration safe.

---

## Phase 2 — Deterministic core

### P2-01 Sandbox — PID and user namespaces

**Depends on:** P1-03
**Exit:** Tool is PID 1; namespace teardown kills all descendants; orphans detected.

### P2-02 Sandbox — cgroups v2

**Depends on:** P2-01
**Exit:** `memory.max`, `cpu.max`, `pids.max` enforced; a fork bomb is contained; per-run
resource cost recorded.

### P2-03 Full integrity gate

**Depends on:** P2-02
**Exit:** All four gate branches from architecture.md §5.1 implemented.

- [ ] `execution_truncated` on resource-cap hit — a capped run's empty changeset proves
      nothing
- [ ] Escape-class denied syscall sets `adversarial_flag` but **accepts** the evidence;
      attempted escapes are among the most interesting findings the harness can produce
- [ ] Flag follows the record all the way into publication

### P2-04 Run planner

**Depends on:** P2-03
**Exit:** A request for `{readOnly, idempotent, openWorld}` compiles to the minimal
deduplicated arm set from architecture.md §4.1.

- [ ] Arm 1 serves as `readOnlyHint` evidence, the `D1` idempotency arm, *and* the strict
      `openWorldHint` observation
- [ ] Must **not** reorder or share arms that are required to be independent
- [ ] Every arm gets a freshly constructed sandbox from a byte-identical base
- [ ] Arms are never reused across tools

### P2-05 World provisioner — generic fixtures

**Depends on:** P1-02
**Exit:** Seeded FS and seeded DB, byte-reproducible across constructions.

### P2-06 Argument synthesiser

**Depends on:** P2-05
**Exit:** Schema-driven generation + fixture binding + cache-busting variants.

- [ ] Structural validity from the input schema
- [ ] Semantic validity via fixture binding to entities that actually exist
- [ ] Cache-busting variants for the P2-09 caching branch
- [ ] If an argument is reused across arms that must be identical, **record that it was**

### P2-07 Arms 1′, 2, and 2R

**Depends on:** P2-04, P2-06
**Exit:** Repeat single-call, double-call in-process, and call/restart/call arms all produce
independent changesets.

### P2-08 ⚑ Noise floor

**Depends on:** P2-07
**Exit:** `N = D1 Δ D1′` computed per tool, per run.

ADR-003. Measured, never assumed. Comparing one call against two without first establishing
how much two *identical* single-call runs differ is measuring noise plus signal and reporting
it as signal.

### P2-09 `idempotentHint` multi-arm protocol

**Depends on:** P2-08
**Exit:** The decision tree in architecture.md §4.2 implemented.

- [ ] `D2 Δ D1 ⊄ N` → `violated`
- [ ] `D2R Δ D1 ⊄ N` → `unverifiable`, reason `caching_suppressed_in_process`
- [ ] Both within `N` → `holds`
- [ ] Framed as an equivalence metamorphic relation with `N` as the tolerance — say it that
      way in the paper

### P2-10 Ruleset v2, derived from measured noise

**Depends on:** P2-08
**Exit:** Noise floor measured across ≥50 tools; ruleset v2 derived from it. **Publishable.**

- [ ] Every element of an observed `N` is a candidate normalisation rule
- [ ] **A rule that never appears in any observed `N` should not exist.** Audit v1 against
      this and delete what fails.
- [ ] Re-run P1-09 replay under v2 and diff the verdict tables

### P2-11 Reason-code taxonomy

**Depends on:** P2-09
**Exit:** Closed set of reason codes, documented.

architecture.md §6 invariant 3: *"`unverifiable` without a reason is not a finding, it is a
shrug."* These codes are what the paper reports.

---

## Phase 3 — Network

### P3-01 Network namespace — strict mode

**Depends on:** P2-03
**Exit:** No route out; egress attempts fail and are logged.

### P3-02 veth pair + intercepting proxy

**Depends on:** P3-01
**Exit:** Every connection logged with its destination.

### P3-03 Mock backend redirection

**Depends on:** P3-02, P2-05
**Exit:** A tool needing an external API is transparently served by an in-sandbox mock.

### P3-04 Destination classification

**Depends on:** P3-02
**Exit:** Each destination classified in-sandbox versus external.

### P3-05 `openWorldHint` protocol

**Depends on:** P3-04
**Exit:** The decision tree in architecture.md §4.4 implemented.

- [ ] Egress attempted → `openWorld = true`, contradicts a `false` declaration
- [ ] No egress + tool succeeded → consistent with closed world
- [ ] No egress + tool failed → ambiguous, rerun instrumented

### P3-06 Fixture-generality metric

**Depends on:** P3-03
**Exit:** Ratio of tools working against a generic mock versus needing bespoke fixtures.

Answers open question 2 empirically, and is a publishable result in its own right. It is also
the primary determinant of achievable audit scale.

---

## Phase 4 — Hardening

### P4-01 seccomp-bpf filter

**Depends on:** P2-02
**Exit:** `mount`, `ptrace`, `bpf`, `kexec` and similar denied.

### P4-02 Denied-syscall audit log

**Depends on:** P4-01
**Exit:** Denials harvested into evidence and surfaced to the integrity gate.

### P4-03 Adversarial flagging through to publication

**Depends on:** P4-02, P2-03
**Exit:** `adversarial_flag` present on the published record, not just in the DB.

### P4-04 Worker re-imaging

**Depends on:** P2-03
**Exit:** Workers re-imaged **between servers**, not between tools — bounds the damage from a
successful escape (architecture.md §7).

### P4-05 Hostile test server

**Depends on:** P4-01, P4-04
**Exit:** A deliberately hostile server attempting escape, exfiltration, resource exhaustion,
and hangs is contained; every attempt appears in evidence. **Phase 4 exit criterion.**

- [ ] Note in results that observation *evasion* remains out of scope (design.md §8)

---

## Phase 5 — Audit and publication

### P5-01 Orchestrator scale-out

**Depends on:** P4-05
**Exit:** Queue + worker pool over Linux hosts; object-store backend for evidence.

- [ ] **One sandbox per worker slot at a time.** Concurrent sandboxes share a kernel and page
      cache, and the timing coupling is exactly the noise P2-08 is trying to measure. Scale
      out, not up.

### P5-02 Offline derivation job

**Depends on:** P5-01, P1-09
**Exit:** Batch job over the object store regenerates all verdicts; it is not a step in the
run loop.

### P5-03 Disclosure workflow

**Depends on:** F-06
**Exit:** Embargo state machine, maintainer contact path, disclosure timestamps.

Open question 4, now a component rather than an afterthought. Open question 3 recommendation:
aggregate by default, named on violation after disclosure — the metadata pin is what makes
named publication defensible, since a claim is bound to an exact observed snapshot.

### P5-04 Aggregate reporting

**Depends on:** P5-02, B-03
**Exit:** Rates by annotation, by containability class, and by oracle, written to
`results/conformance/`.

- [ ] Never aggregate across oracles without disclosure (ADR-002)
- [ ] Report the no-verdict fraction as a metric in its own right — ADR-004 predicts it may
      be large early on, and it is a useful signal about harness maturity

### P5-05 Publish ruleset and limitations

**Depends on:** P5-04
**Exit:** Normalisation ruleset and every limitation from design.md §8 published alongside
the results.

Design constraints, not deferred work: external state invisibility, semantic argument
validity, normalisation sensitivity, caching confound, observation evasion. Each must appear
in published results.

---

## Track Q — `destructiveHint` (quarantined)

Runs out-of-band on stored evidence. **Never in the run loop, never in the verdict engine.**
ADR-006.

### Q-01 Mechanical proxy

**Depends on:** P2-09
**Exit:** Canonical changeset partitioned into `deletions ∪ overwrites` versus `pure
additions`.

### Q-02 Human-labelled held-out set

**Depends on:** Q-01
**Exit:** Labelled set with documented labelling protocol and inter-rater agreement.

### Q-03 Model classifier — untrusted input

**Depends on:** Q-02
**Exit:** Classifier runs out-of-band over stored evidence.

This is **the one place in the system exposed to tool poisoning** — it reads tool
descriptions, an established prompt-injection vector, and feeds them to a model.

- [ ] Descriptions treated as untrusted data, never as instruction
- [ ] Runs offline over stored evidence; never in the run loop
- [ ] **Cannot write into the deterministic verdict path.** Enforce structurally.

### Q-04 Agreement statistics

**Depends on:** Q-03
**Exit:** Agreement between mechanical proxy, model classification, and human labels.

The headline number is the **agreement statistic, not a mismatch rate**. Reported as a
secondary result; must not contaminate the deterministic three.

---

## Ongoing

### O-01 Track the Tool Annotations Interest Group

**Exit:** No exit — standing task.

Open question 5. Five SEPs are open and the IG is actively debating runtime evaluation.
`TOOL_SNAPSHOT.spec_revision` and `VERDICT.protocol_version` exist so results survive a spec
change, but someone has to notice the change.

**Checked 2026-07-27.**

**⚑ Flagged for other tasks, not just this one: P0-01's `2026-07-28` warning is confirmed
real, not a false alarm.** Verified three independent ways, not just the blog post: (1) the
official RC announcement states the `initialize`/`initialized` handshake "is removed" —
[blog.modelcontextprotocol.io/posts/2026-07-28-release-candidate](https://blog.modelcontextprotocol.io/posts/2026-07-28-release-candidate/);
(2) the live `schema/draft/schema.ts` in `modelcontextprotocol/modelcontextprotocol` (fetched
today, `LATEST_PROTOCOL_VERSION = "2026-07-28"`) contains **zero** occurrences of
`initialize`/`InitializeRequest`/`initialized` — the types are simply gone, not deprecated;
(3) what replaces it is in the same file: protocol version now travels as
`_meta["io.modelcontextprotocol/protocolVersion"]` on every request, and an optional
`server/discover` request (`method: "server/discover"`) replaces the old upfront capability
exchange. RC was locked 2026-05-21; final ships **2026-07-28 — tomorrow, as of this check**.
As of today the negotiated-and-stable revision genuinely in use across the corpus is still
`2025-11-25`; nothing needs to change today. But this is a structural break, exactly as
flagged, not a version bump: **P0-01's `DiscoveryClient` hardcodes `initialize` +
`notifications/initialized` + `tools/list` as literal method names** (by design, per its own
status note, to keep tool-calling structurally unreachable) and has no fallback path for a
server that only speaks `server/discover`. Every server that upgrades to `2026-07-28` becomes
undiscoverable by the current harness — silently, since there's no `initialize` for it to
fail loudly against; it'll just get whatever error the server returns for an unrecognized
method. This will first show up as unexplained new failures in P0-06/P0-07 Class A/B census
runs and in P1-08's first-verdict target server, well before anyone thinks to check the spec
revision. Recommend a follow-up task (not created here, since O-01 is check-and-report only):
teach discovery to attempt `server/discover` when `initialize` gets no response, and record
which path succeeded as provenance, mirroring the Stage 2 bare-host-vs-containerized
provenance pattern already used in the Phase 0 staging note.

**Tool annotations themselves (the four this project verifies): unchanged.** Confirmed
directly against `ToolAnnotations` in the same draft `schema.ts` (lines ~1899–1939, GitHub
`main`, checked today): `readOnlyHint` (default `false`), `destructiveHint` (default `true`,
"meaningful only when `readOnlyHint == false`"), `idempotentHint` (default `false`, same
caveat), `openWorldHint` (default `true`) — names, semantics, and defaults are byte-identical
to design.md §1's table and to the `2025-11-25` schema. One low-quality secondary source
(an SEO content site, not cited further here) implied `destructiveHint`'s default might have
moved to `false`; checked directly against the authoritative schema and that is false — not
propagating it. No client-requirement changes found either. design.md §1's warning ("verify
against the current spec revision before implementation") is satisfied for today; re-check
after `2026-07-28` actually ships and again before P1-07 (`readOnlyHint` verdict engine) is
implemented for real.

**IG and SEP status.** [Tool Annotations Interest Group charter](https://modelcontextprotocol.io/community/interest-groups/tool-annotations)
is real and active, chartered 2026-04-20. Facilitators: Sam Morrow (GitHub), Robert Reichel
(OpenAI); participants from Microsoft, Cloudflare, GitHub, Nordstrom. Meeting cadence is
listed as "TBD" — no public schedule or minutes exist to check; that part of this task is
genuinely unknowable via search, not being guessed at. Of the five SEPs architecture.md §11
counted as open in March 2026 (SEP-1913, SEP-1984, SEP-1561 `unsafeOutputHint`, SEP-1560
`secretHint`, SEP-1487 `trustedHint`), three are now **closed** — checked live via
`gh api repos/modelcontextprotocol/modelcontextprotocol/issues/{1561,1560,1487}`, all
`"state":"closed"`, each closed by a maintainer as "dormant per SEP guidelines" after ~90
days of inactivity (automated bot reminder, no sponsor, then closure), not merged or accepted
into the spec. The other two remain open and undrafted-into-spec (`gh api .../pulls/{1913,1984}`,
both `"state":"open"`, `"merged":false`): SEP-1913 "Trust and Sensitivity Annotations" and
SEP-1984 "Comprehensive Tool Annotations for Enhanced Governance and UX" are now the IG's
flagship proposals, per its charter's own "Active SEPs Under Discussion" table, alongside two
newer, annotation-adjacent-but-not-hint proposals not in architecture.md's original five:
SEP-1862 (Tool Resolution / preflight checks) and SEP-2417 (Model Preferences for Tools). Net
effect: the field of proposals narrowed from three single-purpose hint additions plus two
broad ones, down to the two broad ones — nothing has shipped, and none of it touches the four
existing annotations this project verifies. The charter's own open-questions list still
explicitly asks "should runtime annotations... be added to the protocol?", confirming
architecture.md's "actively debating runtime evaluation" is still accurate today, unresolved
either way.

### O-02 Prior-art re-survey before publication

**Exit:** Re-run before each publishable milestone (P0-07, P2-10, P5-04).

design.md §11: if an ecosystem-wide conformance audit already exists, this work reframes as
an extension or a replication with a different containment approach.
