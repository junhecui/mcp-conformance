# ADR-010: Pinned Linux development and CI target

**Status:** Accepted
**Date:** 2026-07-27
**Author:** Jun Cui
**Closes:** [F-00](../tasks.md#f-00-pinned-linux-development-and-ci-target),
[F-08](../tasks.md#f-08-pinned-linux-vm-on-the-windows-development-host) (the Windows/amd64 boot of the same pin)
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

## Provisioning record

### First provisioning: macOS / Apple Silicon (arm64) (2026-07-27), closing [F-00](../tasks.md#f-00-pinned-linux-development-and-ci-target)

**The first real boot of this pin, closing the gap this ADR itself left open** ("the checksum
for the pinned serial is not recorded in this document — deliberately... recording it is the
first concrete action for whoever provisions the image").

**Date:** 2026-07-27. **Host:** macOS / Apple Silicon (arm64), the primary dev machine named
throughout this ADR.

- **Serial used:** `20260725` — the serial `.../releases/noble/release/` pointed to at
  provisioning time, referenced by its own dated, non-symlink path
  (`https://cloud-images.ubuntu.com/releases/noble/release-20260725/`), confirmed by that
  path's own page title (`Ubuntu 24.04 LTS (Noble Numbat) release [20260725]`) rather than by
  trusting the symlink resolution.
- **Image:** `ubuntu-24.04-server-cloudimg-arm64.img` (arm64, per this ADR's "use arm64
  natively on Apple Silicon" guidance — no amd64 image was fetched for this boot).
- **Checksum, independently verified, not copied from a search result:**
  `sha256:2eaec7286c49fdea713dddabcf5012cafa7097a658e916acb48f4bc5fdc8e419`. Two checks, both
  necessary:
  1. `SHA256SUMS.gpg` for serial `20260725` verified with `gpg --verify` against the "UEC Image
     Automatic Signing Key <cdimage@ubuntu.com>" (fingerprint `D2EB 4462 6FDD C30B 513D 5BB7
     1A5D 6C4C 7DB8 7C81`, fetched over HTTPS from `keyserver.ubuntu.com` — not the CD-image
     signing key with a similar name and a different fingerprint, which signs a different
     `SHA256SUMS`). Result: `gpg: Good signature from "UEC Image Automatic Signing Key
     <cdimage@ubuntu.com>"`.
  2. The downloaded image's own `shasum -a 256` computed locally and compared byte-for-byte
     against the signed `SHA256SUMS` entry for `ubuntu-24.04-server-cloudimg-arm64.img`. Exact
     match.
  - Caveat, stated plainly: the signing key itself was fetched over TLS from a keyserver, not
    verified out-of-band against a second independent source (e.g. a physically distributed
    keyring or a second, unrelated mirror). This is the standard level of trust most
    cloud-image consumers operate at, but it is weaker than a true web-of-trust check, and is
    recorded here rather than silently assumed to be stronger than it is.
- **Pinned into:** `.lima/mcp-conformance.yaml` (new file, this change) — the exact URL and
  `sha256:` digest above, as a Lima `images:` entry, so `limactl start` re-derives the same
  bytes on any contributor's machine without re-deriving the serial choice or re-verifying the
  signature by hand.
- **Booted with:** Lima 2.2.0 (Homebrew), `vz` VM driver (Apple's native Virtualization
  framework — not QEMU), 4 CPUs / 4GiB / 30GB disk, `virtiofs` mount of the repo.

**The four probes this ADR and F-03's CI step both care about, run inside the booted VM,
verbatim commands and output:**

1. `uname -r` → `6.8.0-136-generic`. GA series confirmed further: `dpkg -l | grep linux-image`
   shows `linux-image-6.8.0-136-generic` and `linux-image-virtual` installed, no
   `linux-image-generic-hwe-24.04` or any HWE kernel package present (the one HWE-named
   package present, `systemd-hwe-hwdb`, is a udev hardware database, unrelated to kernel
   series). `apt-cache policy linux-image-generic` shows candidate `6.8.0-136.136` from
   `noble-updates`/`noble-security` — the GA metapackage's own update track, not a jump to a
   newer upstream series.
2. `cat /sys/fs/cgroup/cgroup.controllers` → `cpuset cpu io memory hugetlb pids rdma misc`
   — cgroups v2 unified hierarchy present with the controllers `sandbox` will need
   (`memory`, `cpu`, `pids`).
3. `sudo unshare --mount --uts --ipc --net --pid --user --fork -- true` → exit 0. All six
   namespaces usable, matching F-03's CI probe exactly.
4. Overlay mount using this ADR's pinned options
   (`redirect_dir=off,metacopy=off,index=off`) against a throwaway `lower`/`upper`/`work`/
   `merged` set: mount succeeded, a file written pre-mount in `lower` was visible through
   `merged`, a file written into `merged` post-mount landed in `upper` (confirming the upper
   layer is the changeset, per design.md §5), clean `umount` succeeded. `grep overlay
   /proc/filesystems` also confirms the filesystem type is registered (`nodev overlay`).

**All four passed.** This is the first time any of this ADR's claims were checked against a
real boot rather than asserted from the pin's specification — the pin now has a real
provisioning behind it, not only a decision.

### Second provisioning: Windows 11 Home / amd64 (2026-10-09), closing [F-08](../tasks.md#f-08-pinned-linux-vm-on-the-windows-development-host)

The macOS/arm64 record above is **kept, not replaced** — it remains the provisioning path for
Apple Silicon contributors. This section records the second host the pin has been booted on,
and the first time the **amd64** branch of this ADR's "Architecture" clause was exercised,
which matters because amd64 is the arch this ADR names as the CI-matching one.

**Date:** 2026-10-09. **Host:** Windows 11 **Home** 10.0.26200, 24 logical CPUs, 15.9 GiB RAM,
135 GiB free on `C:`. WSL2 (Microsoft kernel 6.6.87.2-1) already present and in use for the
non-sandbox workspace — see HANDOFF.md §3.1. WSL2 is **the hypervisor host, never the kernel
under test**: it runs Microsoft's own 6.6 kernel, which cannot satisfy this pin, so the
pinned Ubuntu 6.8 GA kernel runs in a real guest *inside* it. See "Hypervisor" below.

- **Serial used:** `20260725` — **the same dated serial as the macOS/arm64 boot**, which was
  the goal rather than a coincidence: a single serial across both architectures is what makes
  the two hosts the same pin instead of two adjacent ones. Verified present and still served
  before relying on it (`HTTP 200` for
  `https://cloud-images.ubuntu.com/releases/noble/release-20260725/SHA256SUMS`, and that
  path's own page title reads `Ubuntu 24.04 LTS (Noble Numbat) release [20260725]`). No
  substitution was needed. The moving `.../releases/noble/release/` symlink was never
  fetched; for the record, `releases/noble/` now lists **five** serials *newer* than
  `20260725` (`20260801`, `20260814`, `20260826`, `20260911`, `20260926`), which is exactly
  the drift the dated path exists to be immune to. The `release/` symlink is not a sixth —
  it currently targets the fifth of those, so counting it separately double-counts.
- **Image:** `ubuntu-24.04-server-cloudimg-amd64.img` from
  `https://cloud-images.ubuntu.com/releases/noble/release-20260725/ubuntu-24.04-server-cloudimg-amd64.img`
- **Checksum, independently verified, not copied from a search result:**
  `sha256:d1940f7d69d343355e183dff1e08a59852d32e7309baa7a4bad8365b11b005ac`. Same two checks
  the arm64 record ran, with one deliberate improvement:
  1. `SHA256SUMS.gpg` for serial `20260725` verified with
     `gpgv --keyring /usr/share/keyrings/ubuntu-cloudimage-keyring.gpg SHA256SUMS.gpg SHA256SUMS`
     → exit 0, verbatim:
     ```
     gpgv: Signature made Sat Jul 25 15:34:07 2026 PDT
     gpgv:                using RSA key D2EB44626FDDC30B513D5BB71A5D6C4C7DB87C81
     gpgv: Good signature from "UEC Image Automatic Signing Key <cdimage@ubuntu.com>"
     ```
     The signing key's fingerprint is `D2EB 4462 6FDD C30B 513D  5BB7 1A5D 6C4C 7DB8 7C81`,
     byte-identical to the fingerprint the arm64 record pinned, and the uid is the same
     "UEC Image Automatic Signing Key <cdimage@ubuntu.com>" — *not* the similarly-named
     CD-image signing key that signs a different `SHA256SUMS`.
     **The improvement:** the key came from the distro-packaged
     `ubuntu-cloudimage-keyring` (2023.11.28.1), installed through `apt` and therefore
     already authenticated by Ubuntu's own archive signing key, rather than fetched over TLS
     from `keyserver.ubuntu.com` the way the arm64 boot did. This directly narrows the caveat
     the arm64 record disclosed ("fetched over TLS from a keyserver, not verified
     out-of-band against a second independent source"): the apt path is a second,
     independent trust root, and the two agree on the fingerprint. It is still not a
     web-of-trust check against a physically distributed keyring, so the caveat is reduced,
     not eliminated. Recorded as such. (A keyserver fetch was attempted first and failed —
     `gpg: keyserver receive failed: Server indicated a failure` — which is what prompted
     looking for the better path rather than the worse one.)
  2. The downloaded image's own `sha256sum` computed locally and compared against the signed
     `SHA256SUMS` entry for `ubuntu-24.04-server-cloudimg-amd64.img`. Exact match.

- **Artifacts live outside the repo**, under `~/vm/f-08/` on the WSL distro's own ext4
  (943 GiB free), not on `/mnt/c` (drvfs/9p — wrong performance profile for a qcow2 under
  active I/O) and not in the worktree. `image/` holds the pinned base plus the `SHA256SUMS`
  and `SHA256SUMS.gpg` it was verified against, so the verification is re-runnable in place
  rather than only recorded here; `disk.qcow2` is a **copy-on-write overlay backed by** that
  base, which keeps the pinned image byte-identical and effectively read-only for the guest's
  whole life. Nothing large lands in git: the repo gains only `scripts/`.

#### Hypervisor: QEMU + KVM *inside* WSL2, not QEMU on Windows

This diverges from HANDOFF.md §4.1's menu ("Hyper-V Gen2 if available, otherwise QEMU or
VirtualBox"), and from the plan this task started with, so the reasoning is recorded here
rather than left implicit.

One Windows-side option is closed outright; the other was **available but not chosen**, and
the distinction matters enough to state precisely, because an earlier draft of this section
asserted that *both* were closed by privilege while disclosing, three sentences later, an
unelevated QEMU that had already run with WHPX. That was a contradiction, and it corrupted the
escalation path at the bottom of this section by overstating what a future reader has to
acquire.

- **Hyper-V is absent.** Windows 11 *Home* does not ship it; §4.1 already anticipated this.
  This one genuinely is closed by the SKU, not by a preference.
- **QEMU on Windows was viable and was declined on artifact quality, not on privilege.**
  What is true about privilege: the *supported installer* path wanted elevation and that
  elevation was declined — the `winget` install of `SoftwareFreedomConservancy.QEMU` 11.1.0
  reached `Starting package install...` and then returned
  `0x800704c7 : The operation was canceled by the user`. And
  `Get-WindowsOptionalFeature` genuinely cannot even *query* whether the `HypervisorPlatform`
  feature (WHPX's prerequisite — distinct from the `VirtualMachinePlatform` feature WSL2
  uses) is enabled without an elevated token, let alone enable it; that was verified, and it
  means WHPX's availability was **unknowable in advance** from an unelevated shell.
  What is equally true: a **portable, no-admin extraction** of the same QEMU 11.1.0 Windows
  build was tried and **did start with `accel=whpx`** — so the install elevation was
  bypassable by extraction, and WHPX was evidently already available on this host rather than
  needing to be enabled. Neither elevation turned out to be *needed*. It was still abandoned
  in favour of the path below, for a reason that is a preference rather than a blocker:
  **depending on a hand-extracted installer payload is a worse artifact than a distro
  package.** It has no package manager behind it, no upgrade or integrity story, and a
  version nobody else on this project can reproduce by name — against
  `apt-get install qemu-system-x86` from the pinned distro, which this ADR's whole posture
  (pinned image, pinned serial, pinned toolchain, signature-verified base) is built out of.
  Recorded as a preference grounded in artifact quality, not as an impossibility.
- **VirtualBox** was not attempted: on a host where the Windows hypervisor is already
  running (it is — WSL2 and Docker Desktop both depend on it) VirtualBox 7 falls back to its
  Hyper-V backend, which has well-known performance problems, and it needs elevation to
  install regardless.

For the record of what a TCG fallback would have cost, had WHPX actually been unavailable: a
QEMU without it falls back to pure instruction emulation, and a full `cargo test --workspace`
under TCG is an order of magnitude slower — which would not plausibly satisfy this task's own
exit criterion of a *working dev loop*. That cost is why WHPX mattered; it is not evidence
that WHPX was unobtainable.

**What is available with no elevation at all:** the WSL2 `Ubuntu-24.04` distro already
exposes `/dev/kvm` (`crw-rw---- 1 root kvm 10, 232`) with `svm` in `/proc/cpuinfo` — nested
virtualisation is live by default on this host, with no `.wslconfig` change. So the VM runs
as `qemu-system-x86_64 -machine q35,accel=kvm -cpu host` inside that distro
(`qemu-system-x86` 8.2.2, from the distro archive), reached over an SSH port-forward bound
to `127.0.0.1:2222`.

**Why this does not weaken the pin.** The nesting is L0 Microsoft/Hyper-V → L1 WSL2 →
**L2 the pinned Ubuntu 24.04 guest**. Everything this ADR pins — the GA 6.8 kernel,
cgroups v2 controllers, the six namespaces, overlayfs and its mount options — belongs to
**L2's own kernel**, which is the kernel `sandbox` compiles and runs against. WSL2's 6.6
kernel is demoted to hypervisor host and is never the kernel under test. The decision in
"Why a VM image over a dedicated bare-metal/cloud box" above is untouched; only the choice
of *which VM runner boots the image* changed, which that section explicitly left open
("the requirement is 'boots the exact pinned image,' not a specific launcher").

**Costs, disclosed rather than buried.**

- **Two layers of nesting cost throughput.** Expect builds slower than a bare L1 guest
  would be. Acceptable: nothing in Phase 1–4 is throughput-bound, and architecture.md §7
  already constrains the *real* throughput story to "one sandbox per worker slot at a time,
  scale out not up" — a contributor's local loop was never the scaling story.
- **If the WSL service dies, the VM dies with it.** Two such failures happened during this
  project's work on 2026-10-08. This is the specific objection that originally argued
  *against* QEMU-in-WSL, and it is a real operational caveat, not a resolved one. Note what
  it is now weighed against: not a hard blocker (see the correction above — the Windows-side
  path was viable), but a preference for a distro-packaged hypervisor over a hand-extracted
  one. If this reliability cost ever starts dominating, that trade should be re-made rather
  than treated as settled. Mitigated meanwhile in two ways: all VM state lives in
  `~/vm/f-08/` and survives a `wsl --shutdown` or a distro restart, and
  `scripts/vm.sh start` is idempotent, so recovery is one command and never a reprovision.
- **The escalation path, concretely, if either cost ever bites.** Ordered cheapest-first,
  since the cheapest step needs no elevation at all:
  1. **Re-try the portable extraction**, which already worked once: extract the QEMU for
     Windows build and run `qemu-system-x86_64.exe -machine q35,accel=whpx`. No elevation,
     no feature enable — on this host WHPX was already available. Check it with
     `qemu-system-x86_64.exe -accel help`, or just launch and see whether it starts with
     `accel=whpx` instead of falling back. This is the step the earlier draft of this
     section wrongly described as closed.
  2. **If WHPX turns out to be absent on some other host**, then and only then:
     `Enable-WindowsOptionalFeature -Online -FeatureName HypervisorPlatform` from an
     elevated shell, and reboot. The query itself needs elevation too, so on an unelevated
     shell step 1 *is* the probe.
  3. **Prefer a supported install over the extraction** once elevation is available at all:
     `winget install SoftwareFreedomConservancy.QEMU`, which is the artifact-quality
     objection answered rather than worked around.

  In every case the port is small and bounded: the image pin, the signed-sums verification,
  `seed.iso`, the qcow2 overlay and the whole test loop are unchanged. Only
  `scripts/vm.sh`'s `qemu-system-x86_64 … -machine q35,accel=kvm` line is
  hypervisor-specific, plus translating `$VM_DIR` paths and the `127.0.0.1:2222` forward
  across the WSL/Windows filesystem boundary. Named, not built.

**Guest:** `dev@mcp-conformance-vm`, 4 vCPU / 4096 MiB / 40 GiB qcow2 overlay, seeded by a
`CIDATA`-labelled NoCloud ISO (`scripts/vm/user-data.template` + `meta-data`). Password login
is impossible by construction (`lock_passwd: true`, `ssh_pwauth: false`, `disable_root: true`);
the only credential is the per-host ed25519 keypair `scripts/vm-provision.sh` generates, and
SSH is reachable only on a loopback-bound forward (`127.0.0.1:2222`), never on a LAN-visible
address. Toolchain: `build-essential`, distro `rustup`, and `1.85.1` + `clippy` from
`rust-toolchain.toml`. `CARGO_TARGET_DIR=/home/dev/cargo-target` — VM-local, never on a
shared mount and never synced.

`linux-image-generic` is installed **as a cloud-init package** rather than inferred from
whatever the image happened to ship, which makes the GA metapackage the thing actually
present rather than a property hoped for. It pulls a newer in-series kernel than the image
booted, so cloud-init reboots once into it (`power_state: reboot`, conditional on
`/var/run/reboot-required`) — that reboot is expected behaviour on first boot, not a fault.

**The four probes, re-run inside this guest.** They are now a **re-runnable script**
(`scripts/vm.sh probes`) that exits non-zero if the guest stops satisfying the pin, rather
than a transcript that can only ever describe one past moment. Verbatim output, exit 0 —
note the two prefixes, which are structural rather than decorative: every byte relayed from
the guest is tagged `  [guest] ` *host-side* (bounded and control-character-stripped; see
"Guest output is bounded and sanitised" below), and `==> [host]` is a line the host wrote
itself from the exit status, in a slot a guest cannot reach:

```
  [guest] ### probe 1: kernel is GA 6.8 series, not HWE
  [guest] 6.8.0-146-generic
  [guest] linux-image-6.8.0-136-generic 6.8.0-136.136
  [guest] linux-image-6.8.0-146-generic 6.8.0-146.146
  [guest] linux-image-generic 6.8.0-146.146
  [guest] linux-image-virtual 6.8.0-146.146
  [guest] -- HWE kernel packages present (expect none):
  [guest] (none)
  [guest] linux-image-generic:
  [guest]   Installed: 6.8.0-146.146
  [guest]   Candidate: 6.8.0-146.146
  [guest]   Version table:
  [guest]
  [guest] ### probe 2: cgroups v2 controllers include memory, cpu, pids
  [guest] cpuset cpu io memory hugetlb pids rdma misc
  [guest]
  [guest] ### probe 3: six-namespace unshare
  [guest] unshare exit=0
  [guest]
  [guest] ### probe 4: overlay mount with ADR-010's pinned options
  [guest] -- overlay filesystem registered (module loaded by the mount above):
  [guest] nodev	overlay
  [guest] -- negative control: an unknown overlay option must be REJECTED
  [guest]    rejected, as required
  [guest] -- mount options as the kernel reports them:
  [guest] overlay /tmp/tmp.9K4XHZsocy/merged overlay rw,relatime,lowerdir=/tmp/tmp.9K4XHZsocy/lower,upperdir=/tmp/tmp.9K4XHZsocy/upper,workdir=/tmp/tmp.9K4XHZsocy/work,uuid=on,nouserxattr 0 0
  [guest]    nouserxattr: present, as expected for a privileged mount
  [guest] -- overlay module defaults for the pinned options (asserted, not merely printed):
  [guest]    redirect_dir  N
  [guest]    metacopy      N
  [guest]    index         N
  [guest]    xino_auto     Y   (not pinned; informational)
  [guest] -- lower-layer file read through the merge:
  [guest] lower-layer-content
  [guest] -- post-mount write captured in upper:
  [guest] total 4
  [guest] -rw-r--r-- 1 root root 20 Oct 10 06:46 new-file.txt
  [guest] written-after-mount
  [guest] umount: clean
  [guest]
  [guest] ALL FOUR PROBES PASSED
==> [host] probes: PASS (guest exited 0; this line is the host's own, derived from that status)
```

Four things in that output are worth reading carefully rather than skimming:

- **The kernel is `6.8.0-146-generic`, not the `6.8.0-136-generic` the arm64 boot reported.**
  That is the GA metapackage's *in-series* update track doing exactly what this ADR says it
  does (`6.8.0-31` → `6.8.0-40` → …), not drift: same 6.8 series, and
  `apt-cache policy linux-image-generic` confirms `6.8.0-146.146` is the GA metapackage's own
  candidate. No HWE kernel package is installed, checked positively (a `grep` for
  `^linux-(image|generic|headers).*hwe` over installed packages, which prints `(none)`) rather
  than inferred from the absence of a complaint. Both `6.8.0-136` and `6.8.0-146` are present
  because the image shipped the former and cloud-init installed the latter; the *running*
  kernel is `-146`.
- **The absence of `redirect_dir` / `metacopy` / `index` from that `/proc/mounts` line is now
  treated as positive evidence, and asserted.** An earlier version of probe 4 leaned on the
  module-parameter block as a proxy, on the belief that the kernel never echoes those options
  back. That belief was too weak. Measured in this guest rather than assumed: mounting with
  `redirect_dir=on,metacopy=on,index=on` yields
  `…,redirect_dir=on,index=on,uuid=on,metacopy=on,nouserxattr`, i.e. the kernel **does** echo
  them when they are set non-default — so their absence on the pinned mount is evidence they
  are *off*, not merely evidence that they parsed. The probe now asserts that absence.
  It also carries a **negative control**: an unknown option (`mcp_bogus_option=42`) must be
  rejected, because if this kernel silently ignored unrecognised options then "the mount was
  accepted with our three" would prove nothing at all about them. It is rejected, so
  acceptance really is parse evidence — tested, not inferred.
- **The module-parameter block is asserted now, not merely printed.** It was printed before,
  which meant a guest whose kernel-wide defaults had flipped to `Y` would have printed `Y` and
  still reported `ALL FOUR PROBES PASSED`. Demonstrated by flipping
  `/sys/module/overlay/parameters/redirect_dir` to `Y` in this guest: the probe now prints
  `FAIL: kernel-wide default for redirect_dir is Y, expected N` and exits 1 (restored to `N`
  afterwards). That flip also surfaced something worth recording — with the kernel-wide
  default at `Y`, the pinned mount displays `redirect_dir=follow` rather than nothing, because
  overlayfs maps a requested `off` onto `follow` while `redirect_always_follow` is `Y`
  (meaning: create no new redirects, but follow pre-existing ones). The two assertions are
  therefore complementary, not redundant: the token check catches an `=on`, the
  module-parameter check catches the `follow` case. `xino_auto` is printed but deliberately
  **not** asserted — this ADR does not pin it.
- **`nouserxattr`.** This is the expected branch: the probe mounts privileged, from the host
  side of the guest, so ADR-010's conditional `userxattr` must *not* be set, and the kernel
  confirms it is not — now asserted rather than eyeballed. The other branch becomes live only
  if P1-03 chooses a rootless-in-userns mount.

**The dev loop, proven end to end rather than asserted.** `scripts/vm-test.sh` rsyncs the
worktree over SSH (never a shared mount — HANDOFF.md §3 flags CRLF conversion of byte-exact
fixtures as a live hazard on this host, and `evtree`/`world` assert byte-identical captures)
and runs the same four gates HANDOFF.md §3.1 reports for WSL2 and for macOS, so the results
are directly comparable:

```
==> environment
  [guest] kernel:   6.8.0-146-generic
  [guest] rustc:    rustc 1.85.1 (4eb161250 2025-03-15)
  [guest] cargo:    cargo 1.85.1 (d73d2caf9 2024-12-31)
  [guest] CARGO_TARGET_DIR=/home/dev/cargo-target
==> asserting the guest's CARGO_TARGET_DIR
  [guest] CARGO_TARGET_DIR=/home/dev/cargo-target is absolute and outside the synced tree
==> [host] CARGO_TARGET_DIR: OK

==> gate: purity
  [guest] purity: OK  ["normalise", "verdict"] depend only on ["datamodel", "evtree"]
==> [host] gate purity: PASS

==> summary (inside the pinned VM; host-side verdicts)
==> [host] PASS  build
==> [host] PASS  test
==> [host] PASS  clippy
==> [host] PASS  purity
```

Two details in that transcript are deliberate. `CARGO_TARGET_DIR` is **asserted**, not just
printed: the gates would otherwise still print `PASS` while building into the synced tree's
`./target` if `/etc/profile.d/99-mcp-conformance.sh` ever went missing — and rsync both
excludes that path *and*, by excluding it, protects it from `--delete`, which is precisely the
stale-artifact state this sync model exists to prevent. And `purity: OK  [...]` is missing its
em-dash because the guest-output relay drops every non-ASCII byte along with the control
characters; that fidelity cost is disclosed in `scripts/vm-lib.sh` rather than discovered
later.

**345 tests passed, 0 failed** — the same count `main` produces on WSL2/amd64, so the suite
is now confirmed reproducible across three environments (macOS/arm64, WSL2/amd64, and this
pinned Ubuntu 6.8 guest). `cargo build --workspace` finished in 23.4s and
`cargo clippy --workspace --all-targets -- -D warnings` in 8.0s on a warm target dir, which
settles the throughput worry about double nesting as a non-issue in practice at this
workspace's size.

**Driving it** (all from inside the WSL `Ubuntu-24.04` distro, from a checkout):

```
scripts/vm-provision.sh      # once: verify + fetch image, build seed.iso and disk.qcow2
scripts/vm.sh start          # boot, detached (idempotent; no-op if already running)
scripts/vm.sh wait           # block until cloud-init has finished (first boot only)
scripts/vm.sh probes         # re-run the four probes above; non-zero exit = pin violated
scripts/vm-test.sh           # sync + build/test/clippy/purity inside the guest
scripts/vm.sh ssh            # interactive shell as dev@
scripts/vm.sh status         # pid + ssh reachability + kernel
scripts/vm.sh stop           # poweroff -> SIGTERM -> SIGKILL, each step verified
```

Environment knobs, all validated before they reach a QEMU argument: `MCP_VM_DIR`,
`MCP_VM_SSH_PORT`, `MCP_VM_CPUS`, `MCP_VM_MEM`, `MCP_VM_DISK_SIZE`, `MCP_VM_RESTRICT`,
`MCP_VM_MAX_GUEST_BYTES`, `MCP_VM_SSH_STDIN`.

**The stdin bug was found twice, and the second time is the more useful record.** Every
internal `ssh` call inherited the caller's stdin, so invoking these scripts from a script that
was itself being fed on stdin (`bash -s < script` — how a non-interactive agent or a CI step
naturally drives them) made `ssh` swallow the remainder of the calling script; the caller then
stopped executing silently, with a **zero** exit status and no error. The first fix routed
every non-interactive call in `scripts/vm.sh` through an `ssh -n -o BatchMode=yes` variant,
with one explicit `MCP_VM_SSH_STDIN=1` opt-in for `cmd_probes`, which legitimately pipes a
heredoc in — and this section said so. But `scripts/vm-test.sh`, *the script this record
elsewhere calls the dev loop*, still had two bare `ssh` calls: its reachability probe and
`run_remote`, which carries the environment dump and all four gates. Driving
`./scripts/vm-test.sh purity` from a piped caller printed `PASS purity` and then silently
dropped the caller's next two lines, exiting 0 — the same green-looking no-op, in the entry
point most likely to be driven by an agent. Closed by moving the definition of "a
non-interactive ssh call" into **one** shared `scripts/vm-lib.sh` that both entry points
source, so the claim "every non-interactive call in both scripts carries `-n`" is structural
rather than a coincidence between two copies. That file also enumerates the complete set of
exceptions so the claim is auditable by grepping for `ssh ` under `scripts/`: the interactive
`vm.sh ssh` with no arguments, the one `MCP_VM_SSH_STDIN=1` opt-in (`cmd_probes`, which pipes
a heredoc in), and `vm-test.sh`'s `rsync -e` command — rsync drives its own pipes into that
ssh child's stdin, so it cannot swallow the caller's and `-n` would break the transport
outright. Verified the way the bug was found: a piped driver with marker `echo`s after the
call, which now print.

**Guest output is bounded and sanitised.** Nothing the guest writes used to be bounded or
stripped: a guest emitting `ESC[2J`, SGR colour, an OSC title-set and an alt-screen switch had
every escape byte arrive intact host-side, so it could clear scrollback and repaint a clean
transcript — and in this project's workflow that console is read by a model. This is the same
output-surface class as the Stage 2 census's unbounded container stderr, which P0-11 fixed
with a bounded, escape-stripped drain (`crates/discovery/src/transport.rs`), so the same
remedy applies here: relayed bytes are capped (`MCP_VM_MAX_GUEST_BYTES`, default 256 KiB, with
one truncation notice), C0-except-LF/TAB plus CR, DEL and all bytes ≥ 0x80 are dropped, record
length is bounded so a newline-free gigabyte cannot grow the host-side buffer, and every guest
line is prefixed `  [guest] ` host-side. The harness's own verdicts are printed from the exit
status with an `==> [host]` sigil that a relayed line cannot occupy. `scripts/vm.sh ssh`
remains a deliberate raw passthrough, because an interactive shell needs its pty and its
escapes. **What this is not:** an integrity boundary. `ssh`'s exit status *is* the remote exit
status, so guest output and guest exit code are one guest-controlled channel — see the
precondition list below and on F-08.

**Three smaller corrections from the same pass**, all cheap and all with a real failure behind
them: `MCP_VM_SSH_PORT='2222-:22,hostfwd=tcp::12345'` used to build a *second* QEMU host
forward bound to `0.0.0.0`, publishing the guest's sshd off-host from one environment
variable (now a digits-only check, as for every other numeric); `vm.sh stop` sent one SIGTERM,
waited, then removed the pidfile and said "stopped" with no SIGKILL escalation and no liveness
re-check, so a guest that ignored poweroff left a running VM invisible to `running()` and the
next `start` would attach a second QEMU to the same qcow2 — the same failure class as P0-06's
orphaned containers, and exactly what ADR-004's integrity gate forbids one level down (it now
escalates poweroff → SIGTERM → SIGKILL, verifies the process is gone before touching the
pidfile, and `die`s rather than claiming success); and `gpgv --keyring
ubuntu-cloudimage-keyring.gpg` accepts a signature from **either** key that keyring holds
(`1A5D6C4C7DB87C81` UEC Image Automatic Signing Key and `7FF3F408476CF100` Ubuntu Cloud Image
Builder — both confirmed present), so the re-runnable check asserted only "signed by some key
in Canonical's keyring" while this record asserts the stronger "signed by
`D2EB44626FDDC30B513D5BB71A5D6C4C7DB87C81`, not the similarly-named other key". The script now
greps `gpgv --status-fd`'s `VALIDSIG` line for that exact fingerprint and refuses otherwise.

A second, unrelated self-inflicted failure is worth naming because it will bite anyone who
repeats it: **editing `vm.sh` while a copy of it was mid-run** produced
`line 130: point:: command not found`, because bash reads a script incrementally from the file
rather than slurping it — not a defect in the script.

---

## Follow-on decisions this ADR does not make

- **P1-03** decides privileged-host-mount vs. rootless-userns-mount for the sandbox
  supervisor, which selects the `userxattr` branch pinned above.
- **A self-hosted CI runner on this exact image** — named as the escalation path, not built.
  Revisit if hosted-runner kernel drift is ever observed to actually change `sandbox`
  behaviour, or once Phase 1 has enough real findings that the infrastructure investment is
  justified rather than anticipatory.
- **Getting the whole repo into this VM is a P1-03+ blast-radius question, not decided
  here.** Nothing hostile runs in this guest today — it exists to compile and run `sandbox`'s
  own namespace/overlayfs/cgroups code, per this file's own "no container runtime needed"
  note. That changes once P1-03 starts actually launching untrusted MCP server processes
  inside it: a full copy of the repo (build scripts, `cargo` config, evidence store) sitting
  next to that execution widens what a hostile tool under test could reach if containment
  inside the guest were ever incomplete. (The **mechanism is per host**, and the earlier
  wording here named the wrong one: the macOS/Lima record uses a writable **virtiofs mount**,
  while the Windows/amd64 path uses **`rsync -a --delete` over SSH** — `scripts/vm-test.sh`
  explains at length why a shared mount is the one thing this loop must not do casually. The
  substance applies unchanged to the rsync'd copy, and the rsync form makes a partial or
  read-only sync a strictly easier change than a mount would have been.) Revisit then —
  options include syncing only what a run needs, or moving untrusted execution to a separate
  scratch VM instead of this development one. Not a defect in the current pin; a note so it
  isn't forgotten when P1-03 lands.

### Preconditions on the transition to running untrusted code

**Not F-08 defects.** Every item below is sound as the guest stands today, because nothing
hostile runs in it or near it. They bind when untrusted registry code arrives — P2-06 against
real servers, and P4-05's deliberately hostile server — not at P1-03's first commit. Two
things this project already got right and that these preconditions build on rather than
correct: HANDOFF.md §4.6 already scopes P1-08 to a **trusted** reference server
(`@modelcontextprotocol/server-filesystem`) with hand-written arguments and states that Phase
1 containment is mount namespace plus timeout only, and the bullet above already defers the
blast-radius question to P1-03 rather than pretending it is answered.

The canonical list, with the measurements behind each item, lives on
[F-08 in tasks.md](../tasks.md#f-08-pinned-linux-vm-on-the-windows-development-host) and is
cross-referenced from P1-03. In priority order:

1. **Network isolation.** `-netdev user` with no `restrict=` tunnels the guest into the WSL
   distro's loopback and gives it unrestricted egress. `MCP_VM_RESTRICT=1` now exists as an
   opt-in; a `tap` device in a host-side netns is the proper fix, and is what Phase 3's
   strict/instrumented arms need anyway.
2. **A separate scratch VM for untrusted execution**, not this development guest. Promoted
   from the recommendation in the bullet above to a requirement: QEMU runs as the
   contributor's own user, so an L2 escape lands in **L1, the WSL distro** — the most
   valuable square in the chain.
3. **Snapshot/revert around every untrusted run, and a non-sudo execution user.**
4. **Guest-reported results are untrusted once untrusted code has run.** `ssh`'s exit status
   *is* the remote exit status, so the probe and gate verdicts are evidence about the pin and
   the build only on a guest that has run nothing untrusted.
5. **Verify `disk.qcow2`, not only the base image it is backed by.**
6. **Bound the `-serial file:` sink**, an unbounded guest→host-disk write channel.
7. **Narrow the device surface** for untrusted runs (`-nodefaults -vga none`, QEMU's own
   `-sandbox on`, a dedicated unprivileged QEMU user).
8. **Pin a real `known_hosts`** instead of `StrictHostKeyChecking=no` plus
   `UserKnownHostsFile=/dev/null`.
9. **Disable TCP/agent/X11 forwarding in the guest's sshd** for an untrusted-execution guest.
