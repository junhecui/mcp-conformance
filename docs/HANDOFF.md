# Handoff — 2026-10-06 (macOS → Windows PC)

Work moved to a Windows PC because the macOS host's disk was 98% full. This note carries
everything a fresh Claude Code session needs to continue. Delete it (or replace it) once the
queue below is worked through. `docs/tasks.md` stays the canonical backlog; this note only
sequences it and records decisions not yet written there.

**Read first:** `CLAUDE.md` (especially the *manager, not individual contributor* working
model: delegate implementation to subagents, two independent review passes per task — code
reviewer + security specialist — and one commit per task after review), then
`docs/architecture.md` and `docs/tasks.md`.

---

## 1. State at handoff

- `main` is green on macOS: `cargo test --workspace` 156 passed (re-run 2026-10-06); clippy
  `-D warnings` and `cargo purity` clean per commit 832d990.
- `docs/tasks.md` is accurate. Done: F-00…F-07, P0-01…P0-09, B-01…B-03, P1-01, P1-02 (22 of
  60 tasks). Open halves: P0-07 Class A at ≥1,000 servers; P0-09's live-server checkbox.
- Nothing from P1-03 onward exists. `sandbox`, `observe`, `integrity`, `planner`, `argsynth`,
  `orchestrator`, `destructive` are doc-comment placeholders. `normalise` and `verdict` are
  `todo!()` stubs whose messages ("blocked on ADR-008/009") are stale — both ADRs are
  accepted and `crates/evtree` exists. `datamodel::{RawEvidence, Ruleset, CanonicalChangeset}`
  are empty placeholders. `rulesets/` holds only `.gitkeep`.
- Branch **`wip/p0-10-census-evidence`** (pushed): unreviewed P0-10 work, see §4.2. Builds,
  185 tests pass, clippy and purity clean. Not smoke-tested, not reviewed.
- The macOS Lima VM from F-00 (`.lima/mcp-conformance.yaml`) still exists on the Mac, stopped.
  The Lima config stays in the repo for macOS contributors; it is not used on Windows.

## 2. Assessment summary (computed 2026-10-06 from `results/`)

Census data was collected 2026-07-26/27, before MCP spec `2026-07-28` shipped.

| | Class B (remote, live HTTP) | Class A (Docker) |
|---|---|---|
| Sampled → reachable | 1,000 → 250 (25%) | 100 → 57 (57%) |
| Tools discovered | 3,183 | 956 |
| No `annotations` object | 48.6% | 41.7% |
| `readOnlyHint` explicit | 49.2% | 52.2% |
| `destructiveHint` explicit | 29.5% | 34.8% |
| `idempotentHint` explicit | 21.6% | 36.5% |
| `openWorldHint` explicit | 37.1% | 48.0% |
| Top-5 servers' share of all tools | 26.1% | 39.3% |

Registry (18,664 servers): Class A 51.0%, Class B 44.4%, unclassifiable 4.5%. Track B probe:
207 verdicts — 192 `unverifiable`, 12 `holds`, 3 `violated`, of which one (pricetik) is a
likely false positive from UI-render resources and two come from one unconfirmed server, so
**zero confirmed violations to date**.

Problems to fix before publishing the census:
1. **Tool-weighted, not server-weighted.** A few large servers dominate. Open question 1 asks
   about *servers*, and the stored results can't answer it: only corpus tallies and a per-server
   `tool_count` were saved, and no raw `tools/list` bytes were persisted. → P0-10.
2. **Stale.** Predates `2026-07-28` (initialize handshake removed). → re-run after P0-10.
3. **O-02 not done.** The backlog requires a prior-art re-survey before P0-07 is published.

## 3. Environment on Windows

The workspace does **not** build natively on Windows (`discovery`, `store` and `world` use
`std::os::unix`), and the sandbox is Linux kernel work. Use Linux, in two tiers:

- **Now — WSL2, Ubuntu 24.04** (`wsl --install -d Ubuntu-24.04`). Run Claude Code *inside*
  WSL2 and clone into the Linux filesystem (`~/`, not `/mnt/c` — slow, and Windows checkouts
  risk CRLF conversion of byte-exact fixtures; set `git config core.autocrlf false`). Install
  `build-essential` (rusqlite builds bundled SQLite) and `rustup`; `rust-toolchain.toml` pins
  1.85.1. Enable Docker Desktop's WSL integration for the Stage 2 census. This is sufficient
  for everything in §4 up to and including P1-07: per F-00, the kernel pin only matters for
  observation-grade overlayfs work (P1-03+).
- **Before P1-03 — the pinned VM (new task F-08, §4.1).** WSL2 runs Microsoft's kernel, not
  ADR-010's pinned Ubuntu 6.8 GA kernel, so it must not be used for sandbox/observation work.

### 3.1 Windows provisioning record (2026-10-06)

Tier 1 (WSL2) is **provisioned and verified**; §3's plan above is what was followed, and this
records what actually exists so a later session need not re-derive it.

Host: Windows 11 **Home** 10.0.26200, WSL 2.6.3.0 (Microsoft kernel 6.6.87.2-1), 150 GiB free
on `C:`.

- **Distro.** `Ubuntu-24.04.5 LTS`, installed with
  `wsl --install -d Ubuntu-24.04 --no-launch` — **no elevation needed**, because the WSL
  platform was already enabled by a pre-existing Docker Desktop install. It is now the default
  distro (previously `docker-desktop`). `/etc/wsl.conf` sets `default=cjunh` and `systemd=true`.
- **User.** `cjunh` (uid 1000, `sudo` group) with NOPASSWD sudo via `/etc/sudoers.d/90-cjunh`.
  This removes no security boundary: the Windows user can already run `wsl -u root`
  unauthenticated in their own distro.
- **Clone.** `/home/cjunh/mcp-conformance` on ext4 (953 GiB free), `origin` over SSH using the
  key already present in the Windows profile (copied to `~/.ssh/id_ed25519`, mode 600;
  authenticates as `junhecui`, push dry-run clean). `core.autocrlf=false` set globally.
- **Toolchain.** Distro `rustup` 1.26.0 (preferred over a piped installer, per §4.1's standing
  preference) to **1.85.1** per `rust-toolchain.toml`, with `clippy` and `rustfmt`;
  `build-essential` 13.3.0 for rusqlite's bundled SQLite.
- **Verified on `main` (f1da15e):** `cargo build`, `cargo test --workspace` **156 passed**,
  `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo purity` all clean. The
  156 figure matches §1's macOS count exactly, so the suite is confirmed reproducible across
  macOS/arm64 and Linux/amd64.
- **Verified on `wip/p0-10-census-evidence` (0a4f4d6)**, in worktree `~/wt/p0-10`: **185
  passed**, clippy and purity clean — §1's claim about that branch confirmed on this host.
- **Docker.** Desktop 29.2.1; WSL integration enabled by adding `IntegratedWslDistros` and
  `EnableIntegrationWithDefaultWslDistro` to Docker Desktop's `settings-store.json` under
  `%APPDATA%` (a `.bak-preclaude` backup is kept alongside it). `docker run` from inside WSL
  works. Two caveats for Stage 2: Docker Desktop's `AutoStart` is **false**, so it must be
  started manually before a census sweep; and it runs the **containerd snapshotter**
  (`UseContainerdSnapshotter: true`), whose image listing/removal semantics differ from the
  classic graph driver for multi-platform images — relevant to P0-10 scope item 6, where
  `docker image rm` of a single-platform image is confirmed working but multi-platform removal
  is not yet exercised.
- **Claude Code** 2.1.292 at `~/.local/bin/claude`. Run it from inside WSL.

Two consequences for the queue, not yet reflected in §4:

- **§4.1 (F-08) cannot use Hyper-V.** Windows 11 Home has no Hyper-V, so the pinned-VM
  hypervisor choice resolves to QEMU or VirtualBox. ADR-010's amd64, dated-serial and
  checksum-verification requirements are unaffected.
- **§4's shared `CARGO_TARGET_DIR` suggestion is deliberately not followed.** It existed to
  relieve the macOS host's disk pressure; with 953 GiB free, separate per-worktree target
  directories are preferable, since cargo locks a target directory and a shared one would
  serialise the parallel worktree builds §4 asks for.

**Do not use the Windows-side clone** on the Desktop under `OneDrive`. It has
`core.autocrlf=true` (the Git-for-Windows system default) and the repo has no
`.gitattributes`, so its working tree genuinely holds CRLF text files — precisely the hazard
§3 warns about for byte-exact fixtures — and it additionally sits inside OneDrive. It is
redundant with the WSL clone; the committed bytes are unaffected.

## 4. Queue, in order

Parallelise where independent (each in its own git worktree). On a disk-constrained host,
point every worktree at one shared `CARGO_TARGET_DIR` so dependencies build once. Long live
sweeps are run by the manager, never by an implementer subagent.

### 4.1 F-08 — Pinned Linux VM on Windows (new task; add to Track F in tasks.md)

Exit: a VM matching ADR-010 is a working dev/test loop for Linux-only code, documented.

- Boot the **Ubuntu 24.04 cloud image, amd64**, same dated serial as F-00
  (`releases/noble/release-20260725/`), never the moving `release/` symlink. Verify its sha256
  against the signed `SHA256SUMS` with `gpg --verify` against Canonical's UEC signing key, as
  ADR-010's provisioning record did for arm64 — do not take the checksum from a search result.
  ADR-010 already deems the namespace/overlayfs/cgroups behaviour in scope
  architecture-insensitive.
- Hypervisor: Hyper-V Gen2 if available (Windows Pro/Enterprise), otherwise QEMU or
  VirtualBox. Cloud-init seed (NoCloud) for user + SSH key.
- Kernel: GA `linux-image-generic` 6.8 series, **not** HWE. Overlay options per F-00:
  `redirect_dir=off,metacopy=off,index=off`.
- Provision: `build-essential`, `rustup` (prefer the distro package over `curl | sh`), toolchain
  from `rust-toolchain.toml`. `CARGO_TARGET_DIR` VM-local, never on a shared mount.
- A small script (e.g. `scripts/vm-test.sh` or `.ps1`) that syncs/mounts the repo and runs
  build / test / clippy / `cargo purity` inside the VM.
- Re-run the F-00 probes inside it: `uname -r` → `6.8.0-*-generic`; `cgroup.controllers` lists
  memory/cpu/pids; six-namespace `unshare` exits 0; overlay mount with the pinned options works.
- Update ADR-010's provisioning record with the Windows/amd64 path; keep the Lima config.

### 4.2 P0-10 — finish `wip/p0-10-census-evidence` (new task; add to Phase 0 after P0-09)

Exit: census runs persist raw evidence, report server-weighted metrics and spec-revision
provenance, and every number can be re-derived offline from stored bytes.

Original scope (audit the WIP branch against it):
1. Persist `handshake_raw` (named `initialize_raw` until P0-11 renamed it; may hold
   `server/discover` bytes — see `discovery_path`), `probe_raw` (P0-11) and
   `tools_list_raw` for every successful discovery via `store::BlobStore`; digests on each
   server record. Store root defaults to `results/census/evidence/`, overridable. JSON must stay
   useful even if blobs aren't committed.
2. One derivation path: coverage numbers computed from bytes *read back from the blob store*.
   Offline `cargo xtask census-rederive <results.json>`, plus a test that rederivation
   reproduces the live run exactly. Reuse `census::coverage::tally`.
3. Per-server tallies; top-level server-weighted metrics (% servers with no / all / some tools
   annotated; per annotation, % servers with ≥1 tool explicit and with all tools explicit).
   Keep the existing tool-weighted fields unchanged so July and October compare directly.
4. Per-server `negotiated_spec_revision` and `discovery_path`, plus distributions.
5. Stage 2 only: `--jobs N` (default 1, capped). Stage 1 stays sequential (politeness rule,
   P0-07). Unique `--name` + unconditional `docker rm -f` per container (P0-06 orphan bug),
   45 s watchdog per attempt, deterministic output order.
6. Stage 2 disk hygiene: remove OCI images pulled by an attempt that weren't present before
   the sweep; below 3 GiB free, stop launching attempts and write `"complete": false`.
7. Keep every hostile-input protection (10 MiB recv cap, timeouts, watchdog, `pin_failed`, no
   bare-host execution, `execution_provenance`) and the hash-based sampling unchanged.

Known gaps in the WIP: item 6's image removal appears absent; no P0-10 entry in tasks.md; smoke
runs (`census-stage1 5`, `census-stage2-class-a 3 --jobs 2`, then rederive-matches) not done;
no reviews yet. Smoke runs overwrite `results/census/*.json` — restore with
`git checkout -- results/` and delete smoke blobs. Squash the WIP into one reviewed commit.

### 4.3 Census re-run (manager runs; after P0-10 lands)

`cargo xtask census` (Stage 0 refresh), `census-stage1 1000`, `census-stage2-class-a 1000
--jobs 4`. Measure blob size before deciding whether to commit `results/census/evidence/`.
Compare against the July numbers in §2. Closes P0-07's Class A half; closes P0-09's last
checkbox if any server negotiates via `server/discover`. Stage 1 contacts ~1,000 unrelated
third-party hosts — sequential, identifying `User-Agent`, as before.

### 4.4 O-01 / O-02 research (docs only; can run in parallel with everything)

Output: `docs/prior-art-resurvey-2026-10.md`, plus dated updates to the O-01 and O-02 sections
of tasks.md. Primary sources only; mark unverified claims; ignore SEO sites.

- O-01: did `2026-07-28` ship final with initialize/initialized removed? Exact wire shape of
  `server/discover`; whether post-handshake requests *require*
  `_meta["io.modelcontextprotocol/protocolVersion"]` (P0-09 extrapolated the request shape and
  doesn't send `_meta` — describe any needed client change precisely). Quote the current
  `ToolAnnotations` schema lines. Status of SEP-1913, SEP-1984, SEP-1862, SEP-2417 and any new
  annotation/trust SEPs; Tool Annotations IG activity. Any public server speaking only
  `2026-07-28` (connect-level requests only, never a tool call; record what was sent).
- O-02: since ~March 2026, any published ecosystem-wide annotation-coverage census,
  annotation-conformance audit, or sandboxed behavioural audit of MCP servers? Per item:
  citation, date, what/how measured, corpus size, overlap. Explicit conclusion per design.md
  §11: does the census need reframing as extension/replication; does anything change the
  conformance plan?

Reviews for a research task: a fact-check pass (citations) and a security-relevance pass
(does any spec change alter the harness's trust model or discovery client).

### 4.5 P1-06 — Normaliser + ruleset v1, then P1-07 — `readOnlyHint` verdict engine

Both are pure functions over settled inputs (ADR-008 taxonomy, ADR-009 `evtree1` format), so
they need neither Linux nor the gate to exist; building them before P1-03…P1-05 deliberately
shortens the critical path. Don't build or stub the gate.

P1-06 design points to resolve and document:
- a. **Purity.** `normalise`/`datamodel` stay `#![no_std]` (+`alloc`). If `normalise` decodes
  `evtree1`, `evtree` must become `no_std + alloc` and join `cargo purity`'s `PURE_ALLOWLIST`
  — a `no_std` crate linking a std dependency quietly defeats F-04 Layer 3. Glob matching
  without std. Ruleset parsing that needs std lives outside the pure crates. Ruleset version
  identity must be tamper-evident (digest of the ruleset bytes or bound to one).
- b. **Overlay semantics.** Whiteout (char dev 0/0) = deletion; opaque dir xattr = directory
  replaced; regular file/symlink = create or modify; directories copied up only to hold a
  changed child are structural, a genuinely new empty directory is a mutation. Decide whether
  the base-layer capture is a second input; if not, every ambiguity resolves so a tool looks
  *less* read-only, never more (ADR-008).
- c. **Output.** Changeset partitioned user_state / server_internal / ephemeral (default
  user_state), deterministic order, each change as (path, kind, content digest/metadata) so
  P2-08 can set-compare D1 Δ D1′. State how mtimes/inodes are treated.
- d. **Hostile input.** Total (no panics) and bounded on anything `evtree::decode` accepts.
- Ruleset v1 thin: only ADR-008's seeded allowlists, no speculative rules (P2-10).

P1-07 invariants:
- Pure; cannot take a model, network client or clock (F-04).
- Emits `holds` / `violated` / `unverifiable` with `reason_code` and `oracle =
  kernel_changeset`.
- **An empty changeset from a failed invocation (`isError: true`, crash, timeout) is
  `unverifiable`, never `holds`** — commit 832d990 fixed exactly this false-holds gap in Track
  B; don't reintroduce it in Track A.
- Only gate-passed evidence may reach it (ADR-004); prefer making that a type-level guarantee.
- Declared-`false` / defaulted semantics consistent with `probe::protocol::assess_read_only`,
  or the difference justified.

### 4.6 Then P1-03 → P1-05, P1-08, P1-09 (in the F-08 VM)

Gaps the backlog doesn't yet record — resolve in P1-03's design:
- **Lower layer for real servers.** P1-02 builds small synthetic trees; a real server needs a
  rootfs with Node/Python plus the package, and `npm install` isn't byte-reproducible.
  Recommendation: an OCI image unpacked by digest as the immutable lower layer, P1-02 specs only
  for seeded data on top — every arm then shares literally the same lower bytes.
- **Privileged vs rootless mounts** — ADR-010 left `userxattr` conditional on this.
- **P1-08 target:** a trusted reference server (e.g. `@modelcontextprotocol/server-filesystem`)
  with hand-written arguments. Phase 1 containment is mount namespace + timeout only (no
  net/PID/user namespaces), so arbitrary registry code doesn't belong in it yet; argument
  synthesis is P2-06.

After Phase 1, Phase 2 (P2-01…P2-11) is the first publishable *conformance* result: noise floor
across ≥50 tools. Expect P2-06 (semantic argument validity) to be the scale risk — budget for
curated per-server fixtures.
