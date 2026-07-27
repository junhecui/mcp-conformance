# ADR-010: Pinned Linux development and CI target

**Status:** Accepted
**Date:** 2026-07-27
**Author:** Jun Cui
**Closes:** [F-00](../tasks.md#f-00-pinned-linux-development-and-ci-target)
**Gates:** P1-02 onward. Does not gate Phase 0 (Stage 0/1/2 census) — see "Scope" below.
**Related:** [ADR-007](007-implementation-language.md) (names this as blocking), F-03 (CI),
design.md §9 (overlayfs semantics), architecture.md §3 (trust model), §5 (containment),
[ADR-009](009-evidence-tree-serialisation.md) (consumes what this pins)

---

## Context

ADR-007 established Rust with `sandbox` gated `#![cfg(target_os = "linux")]`, and named the
consequence directly: *"this makes the sandbox untestable on the primary development machine
[macOS], which is F-00's problem, not this ADR's."* `rust-toolchain.toml` pins the compiler
to an exact patch version with a comment pointing here for "the matching kernel pin." F-03's
CI already runs on `ubuntu-24.04` (a pinned *label*, chosen explicitly over `ubuntu-latest`)
and probes for cgroups v2, six-namespace `unshare`, and a working overlay mount before
building — but a probe that the *features exist* is not the same claim as a *fixed kernel
version*, and the task this ADR closes asks for the latter.

Two things `sandbox` will depend on for correctness, not just availability:

- **Kernel version**, because namespace and overlayfs behaviour has genuinely changed across
  releases (`userxattr`, unprivileged-userns overlay mounts, cgroups v2 controller
  defaults), and a verdict's reproducibility claim (ADR-005 — "verdicts reproducible from
  stored evidence plus the normalisation ruleset") is weaker if the *evidence itself* was
  shaped by a kernel version nobody recorded.
- **Overlayfs mount options**, because architecture.md §5's whole premise — "the containment
  boundary is the measurement instrument" — depends on the upper layer meaning exactly one
  thing. Different mount options change what ends up in the upper layer and how whiteouts
  and opaque directories are represented (see ADR-009, which consumes this directly).

### Scope: this gates Phase 1+, not Phase 0

Restating the task text because it is easy to over-apply this ADR: Stage 0 (registry
metadata) and Stage 1 (Class B `initialize`/`tools/list` over HTTP) touch no sandbox code at
all and run fine from any host, including the macOS development machine — P0-06/P0-07 already
did this. Stage 2 (Class A census — locally launching a server to discover it) needs
*containment* so a hostile `tools/list` target can't damage the host, but not *this ADR* —
a stock container runtime (Docker/Podman, whatever's convenient) is adequate containment
without observation, and using one for Stage 2 doesn't compromise ADR-007's
hand-rolled-namespaces decision, which is about *observation* quality, not containment per
se. This ADR is specifically about the fixed-kernel-version precision that
*observation-grade* overlayfs work (Phase 1 onward) needs.

---

## Decision

**Target: a pinned Ubuntu 24.04 LTS ("Noble Numbat") cloud image, referenced by an exact,
archived build serial — not a floating "latest" pointer, and not the GitHub-hosted CI runner
image treated as the source of truth.**

### Why a VM image over a dedicated bare-metal/cloud box

The task text already leans this way; the reasoning, made explicit rather than asserted:

| | Pinned VM image | Dedicated bare-metal/cloud box |
|---|---|---|
| **Kernel fixed until deliberately changed** | Yes — the image *is* the pin; booting it anywhere reproduces the same kernel | Yes, but only until the box's own maintainer patches it — "pinned" is a promise about process, not a property of the artifact |
| **Contributor local access** (F-00's own checklist item) | Any contributor boots the same image locally, on any host OS, no shared-account dependency | Requires SSH/cloud-account access to one shared machine — doesn't solve "local access" at all, it solves "remote access to one thing" |
| **Concurrent use by multiple contributors** | Each contributor's VM is independent | One shared box means two contributors' sandbox runs interfere with each other — exactly the page-cache/kernel-sharing coupling architecture.md §7 already rules out for *worker* concurrency ("one sandbox per worker slot at a time... concurrent sandboxes share a kernel and a page cache") |
| **Cost while idle** | Free — boots on a contributor's existing laptop | A running cloud instance costs money continuously, or a stop/start box that still needs upkeep |
| **Relationship to later work** | N/A — this is the dev/CI parity problem | The dedicated-box shape is exactly what P5-01's worker pool needs later, at scale, for a different reason (throughput, not dev parity). Building it now for F-00 would be solving P5-01's problem early and this one badly |

A container was never seriously in the running: it shares the host kernel by construction,
which defeats "pinned" the moment the host itself isn't fixed — this is the task's own
framing, and it survives scrutiny. It's also the reason Stage 2 census (previous section)
is explicitly *not* this ADR's job: a container is the right tool for containment-without-a
-kernel-claim, which is all Stage 2 needs.

### The exact pin

- **Distribution:** Ubuntu 24.04 LTS, GA kernel series **6.8** (`linux-image-generic`, not
  an HWE point-release track — the GA package stays on the 6.8.x series for the life of
  24.04, receiving only in-series stable updates, e.g. `6.8.0-31-generic` →
  `6.8.0-40-generic`; the HWE track deliberately jumps to newer upstream series, e.g. 6.11,
  6.14, over the LTS lifecycle, which is exactly the drift this ADR exists to prevent).
- **Image:** the official Canonical cloud image for Noble, referenced by its **dated,
  archived build serial** — e.g. `https://cloud-images.ubuntu.com/releases/noble/release-20260705/`
  — never the `.../releases/noble/release/` symlink, which silently repoints to a new serial
  over time. Canonical's archive (`cloud-images-archive.ubuntu.com`) keeps every past serial
  reachable indefinitely, which is what makes a dated serial an actual pin rather than
  today's value of a moving target.
- **Verification:** the pinned serial's own `SHA256SUMS` (and `SHA256SUMS.gpg`) file, fetched
  once when the image is first provisioned and recorded alongside this ADR (as a checked-in
  checksum, the same posture `Cargo.lock` gives dependency versions) — not reproduced here as
  a literal hash, since the honest way to pin a checksum is to fetch and record it at the
  point the image is actually built, not to assert a value from a search result that was
  never independently verified against the signed sums. Whoever executes this — provisions
  the first real VM from this pin — completes that recording as part of doing so.
- **Architecture:** the amd64 build, because that is what GitHub's hosted `ubuntu-24.04`
  runner actually is, and matching it is the entire point for the CI side of "development and
  CI can both target" this. Contributors on Apple Silicon should use the **arm64** cloud
  image at the **same dated serial** rather than emulate amd64 — none of the namespace,
  overlayfs, or cgroups v2 behaviour Phase 1–4 depends on is architecture-sensitive, so a
  native arm64 boot is both faster and just as valid a target; the amd64/arm64 distinction
  only matters for exactly matching CI's own instruction set, which no contributor's local
  loop needs.

### Overlayfs mount options

Pinned regardless of who mounts (host-privileged or rootless-in-userns — that choice belongs
to P1-03, not this ADR; see "Deferred to P1-03" below):

| Option | Value | Why |
|---|---|---|
| `redirect_dir` | `off` | Kept off explicitly rather than relying on whatever the kernel/distro default happens to be, so a distro config change can't silently alter how the upper layer represents a renamed lower-layer directory out from under ADR-009's serialiser. |
| `metacopy` | `off` | **Security-load-bearing, not just a precision choice.** The kernel's own documentation warns against `metacopy=on` with untrusted upper/lower directories: a handcrafted file with forged `REDIRECT`/`METACOPY` xattrs can be used to gain access to a lower-layer file it shouldn't reach. design.md §3's trust model is explicit that the tool under test is assumed *actively hostile* — this is precisely the untrusted-layer scenario the kernel docs warn about, so `metacopy=off` is a direct consequence of the trust model, not a generic best practice borrowed from elsewhere. |
| `index` | `off` | The `index` feature exists to let copied/exported layers be safely reused or NFS-exported across mounts. Every run here gets a freshly constructed base and a freshly torn-down upper (architecture.md §4.1: "every arm runs in a freshly constructed sandbox... arms are never reused across tools") — there is no reuse or export for `index` to protect, only overhead for it to add. |
| `userxattr` | **Conditional — required if and only if the mount happens unprivileged, inside the sandbox's own user namespace** (kernel ≥5.11, which 6.8 satisfies). Ubuntu has historically carried a downstream patch permitting some unprivileged-userns overlay mounts without it, but 6.8 is well past the point where relying on that rather than the documented option is prudent. If P1-03 instead mounts overlayfs on the host as a privileged step *before* entering the namespaces, `userxattr` does not apply and must not be set. **This ADR pins the value conditionally; P1-03 makes the privileged-vs-rootless mount call and applies the matching branch.** |

Tied directly to ADR-009: whiteouts (character device, `dev_major=0`, `dev_minor=0`) are
unaffected by any of the above — they are a file type, not an xattr, so no mount option
changes how they're represented. Opaque directories are affected only in *which xattr
namespace* they land in (`trusted.overlay.opaque` vs `user.overlay.opaque`, depending on the
`userxattr` branch above) — ADR-009's serialiser captures whichever one actually appears,
generically, so this ADR's job is only to make sure the *choice* is deliberate and recorded,
not to make the serialiser aware of it.

### Contributor local access

1. Fetch the pinned serial's cloud image (amd64 or arm64 per host, same dated serial) from
   `cloud-images.ubuntu.com/releases/noble/release-<serial>/` and verify it against that
   serial's `SHA256SUMS`.
2. Boot it locally with a lightweight VM runner that supports cloud-init and both host
   architectures — [Lima](https://github.com/lima-vm/lima) is the standard current choice on
   macOS (Homebrew-installable, arm64-native on Apple Silicon, boots a real cloud image
   rather than a container). Vagrant with a `libvirt`/`qemu`/UTM provider is an equally valid
   alternative for contributors who already have that toolchain; the requirement is "boots
   the exact pinned image," not a specific launcher.
3. Inside the VM: `uname -r` should report a `6.8.0-*-generic` kernel; this is the local
   parity check, the same role `rustup show` plays for the compiler pin.
4. Clone the repo into the VM (or mount it in) and run the workspace commands there —
   `sandbox` now actually compiles and its tests actually run, unlike on the host macOS
   machine.

### CI: an honest gap, not a silent one

**GitHub's hosted `ubuntu-24.04` runner label is not a true kernel pin, and this ADR does
not pretend otherwise.** GitHub periodically updates the packages and kernel inside images
carrying that label — the label pins the Ubuntu release, not the point-in-time build.
Two consequences follow, both accepted rather than engineered around for now:

- **Nested virtualisation is not available on GitHub-hosted runners** (confirmed: hosted
  Linux runners do not expose KVM for general use), so CI cannot simply boot the same pinned
  VM image *inside* the hosted runner to get true parity. The escalation path — a
  self-hosted runner registered inside the pinned VM image itself, replacing
  `runs-on: ubuntu-24.04` with a `self-hosted` label — is real and available, but is
  deliberately **not built now**: no Phase 1+ sandbox code exists yet to need it, and
  standing up self-hosted runner infrastructure before there's anything to run on it would
  repeat the "complete sandbox with no findings attached" anti-goal design.md §10 already
  warns against, one layer down in infrastructure instead of features.
- Until that escalation happens, the **hosted runner is CI's practical target and the pinned
  VM image is the authoritative one**; they are expected to drift apart in minor ways
  (exact kernel patch level) and are not claimed to be identical. What CI *can* do cheaply
  today is make drift **visible** rather than silent — see the change below.

**Change made alongside this ADR:** F-03's existing kernel-prerequisite probe step in
`.github/workflows/ci.yml` (which already checks cgroups v2, six-namespace `unshare`, and an
overlay mount) now also prints `uname -r` for the runner, so a kernel change on GitHub's side
shows up in every CI run's log rather than being invisible until a `sandbox` test starts
behaving differently for no visible reason. It does not fail the build on a mismatch against
this ADR's pin — the hosted runner was never claimed to satisfy that pin exactly, and failing
CI on GitHub's own infrastructure churn, with no owned alternative yet to fall back to, would
be self-defeating.

---

## Options considered

| Option | Kernel actually fixed? | Solves contributor local access? | Cost |
|---|---|---|---|
| **A: pinned VM image, dated serial** (chosen) | Yes | Yes — boots anywhere | Requires a VM runner locally; image download/verify step |
| B: dedicated bare-metal/cloud box | Yes, until patched | No — solves remote access to one shared machine, not local access; also reintroduces the "one sandbox at a time" concurrency constraint at the *contributor* level | Ongoing hosting cost or maintenance burden |
| C: treat `ubuntu-24.04` hosted-runner label as the pin | **No** — this is exactly the gap this ADR exists to close; GitHub updates images under a stable label over time | Partially — nothing for local dev at all | Looks free; the cost is a false reproducibility claim, which is worse than an honest gap for a project whose entire premise is verifiable evidence |
| D: containerised dev environment (Docker) | No — shares host kernel | Yes, trivially | Rejected for the same reason a container was rejected for `sandbox` itself: it cannot pin what it doesn't control |

---

## Consequences

**Good.**

- Closes the actual gap ADR-007 flagged and left open: `sandbox` now has a real environment
  to compile and run against, independent of the macOS development host.
- The pin is at the same posture as everything else this project already pins exactly
  (`rust-toolchain.toml`, `Cargo.lock`, the `ubuntu-24.04` CI label) — a dated cloud-image
  serial plus a recorded checksum, verified once and re-verifiable indefinitely via
  Canonical's archive.
- The overlayfs mount-option pin is not just precision bookkeeping — `metacopy=off`
  specifically closes a documented privilege-escalation path against exactly the "actively
  hostile tool" trust model design.md §3 assumes, which is a genuine security consequence of
  this ADR, not only a reproducibility one.
- Honest about the CI gap instead of asserting a stronger guarantee than actually holds —
  consistent with this project's own standard (`unverifiable` as a first-class verdict
  rather than false `holds`, applied here to infrastructure claims rather than tool
  behaviour).

**Costs, accepted.**

- **CI does not yet run on the true pin.** Until a self-hosted runner is stood up (deferred,
  above), every CI run tests against whatever kernel GitHub's hosted image currently has,
  which is *usually* but not *guaranteed* to match the pinned 6.8.x series. The new
  `uname -r` log line makes this observable; it does not make it match.
- **A VM adds friction to the contributor loop** that a native toolchain wouldn't — one more
  thing to install and boot before `cargo test -p sandbox` works. Accepted as the direct
  consequence of ADR-007's Linux-only `sandbox` decision, which already named this cost and
  assigned it to this task.
- **The checksum for the pinned serial is not recorded in this document** — deliberately, to
  avoid asserting a value that was never independently verified against Canonical's signed
  sums from within this environment. Recording it is the first concrete action for whoever
  provisions the image, not a gap in the decision itself.

---

## Follow-on decisions this ADR does not make

- **P1-03** decides privileged-host-mount vs. rootless-userns-mount for the sandbox
  supervisor, which selects the `userxattr` branch pinned above.
- **A self-hosted CI runner on this exact image** — named as the escalation path, not built.
  Revisit if hosted-runner kernel drift is ever observed to actually change `sandbox`
  behaviour, or once Phase 1 has enough real findings that the infrastructure investment is
  justified rather than anticipatory.
