# MCP Annotation Conformance Harness

**High-level architectural design**

**Status:** Draft / pre-implementation
**Author:** Jun Cui
**Last updated:** July 2026

---

## 1. Problem

The Model Context Protocol defines four behavioural annotations that a server may attach to each tool it exposes:

| Annotation | Claim | Conservative default |
|---|---|---|
| `readOnlyHint` | The tool does not modify state | `false` |
| `destructiveHint` | The tool's mutations are destructive rather than additive | `true` |
| `idempotentHint` | Repeated calls with identical arguments have no additional effect | `false` |
| `openWorldHint` | The tool interacts with entities outside its local environment | `true` |

These annotations are load-bearing. Major clients use them to decide whether a tool call proceeds automatically or requires human confirmation. They are also, by specification, **hints rather than guarantees** — a buggy or malicious server can declare a destructive tool read-only and thereby bypass the confirmation path that protects the user.

No public verification layer exists. Annotations are self-asserted and unaudited across the ecosystem.

**This project builds the missing verification layer**: a harness that executes a tool under observation and determines whether its declared annotations match its observed behaviour.

> Verify against the current MCP specification revision before implementation — annotation names, defaults, and client requirements have changed across revisions.

---

## 2. Goals and non-goals

### Goals

- Determine, for a given MCP tool, whether each declared annotation holds under observation.
- Produce a machine-readable conformance verdict per tool, per annotation, including an explicit `unverifiable` outcome.
- Contain untrusted tool execution safely enough to run against arbitrary third-party servers.
- Support an ecosystem-scale audit: run the harness over many public servers and publish aggregate mismatch rates.

### Non-goals

- **Not** a policy engine, gateway, or runtime. This produces a trust signal that such systems consume; it does not enforce anything.
- **Not** a general MCP test framework for server authors. Adjacent tooling exists for that.
- **Not** a security scanner for prompt injection, credential leakage, or supply-chain risk.
- **Not** a judgement on whether a tool is *useful* or *correct* — only whether it behaves as annotated.

---

## 3. Trust model

The tool under test is assumed **actively hostile**. It may attempt to escape containment, exfiltrate data, exhaust resources, hang indefinitely, or detect that it is being observed and alter its behaviour.

Two consequences shape the design:

1. **Containment is a correctness requirement, not a convenience.** A tool that escapes invalidates the result and endangers the host.
2. **Observation must occur at a boundary the tool cannot forge.** Self-reported behaviour is worthless here — that is the entire premise of the project. All measurement happens at the kernel boundary or below the tool's own abstractions.

The harness operator is trusted. The MCP server, its dependencies, and its declared metadata are not.

---

## 4. System overview

Five components:

**Discovery** — Connects to a target MCP server, enumerates tools via the protocol, and records each tool's input schema and declared annotations. Output: a test manifest.

**Argument synthesis** — Produces invocation arguments for each tool. Schema-driven generation for structural validity, plus per-server fixtures for semantic validity (referencing entities that actually exist in the seeded backend).

**Sandbox supervisor** — Constructs an isolated execution environment, launches the tool inside it, enforces timeouts and resource caps, and tears it down. Owns all containment mechanisms.

**Observation layer** — Collects evidence from the containment boundary: filesystem changeset, network connection log, denied-syscall log, resource consumption, exit status.

**Verdict engine** — Normalises observations, applies the per-annotation verification protocol, and emits a conformance record.

Data flow is linear: discovery → argument synthesis → (sandbox + observation, possibly multiple runs per tool) → verdict engine → results store.

---

## 5. Containment and observation architecture

The central design principle: **the containment boundary is the measurement instrument.** Each isolation mechanism is chosen because its own bookkeeping yields the evidence a verification protocol needs.

| Layer | Mechanism | Containment role | Observation yield |
|---|---|---|---|
| Filesystem | mount namespace + overlayfs | Writes cannot reach the host | Upper layer *is* the state changeset |
| Network | network namespace; optional veth + intercepting proxy | No uncontrolled egress | Connection attempts with destinations |
| Syscalls | seccomp-bpf filter | Blocks `mount`, `ptrace`, `bpf`, `kexec` and similar | Denied-syscall log reveals attempted escapes |
| Resources | cgroups v2 (`memory.max`, `cpu.max`, `pids.max`) | Prevents fork bombs and host exhaustion | Actual resource cost per invocation |
| Process | PID + user namespace | Tool is PID 1; namespace teardown kills all descendants | Exit status, timeout, orphan detection |

The overlayfs decision is the one that makes the project tractable. A naive design snapshots and hashes the filesystem before and after each call. Overlayfs instead hands the changeset over directly as the upper layer, provided by the kernel at no additional cost. This turns `readOnlyHint` verification into a non-emptiness check and `idempotentHint` verification into a set comparison.

---

## 6. Verification protocols

### `readOnlyHint`

Invoke once from a clean base. Inspect the overlayfs upper layer. Non-empty after normalisation contradicts a `true` declaration.

*Fully deterministic for locally-observable effects.*

### `idempotentHint`

Two independent runs from an identical read-only base layer: one invokes the tool once, the other twice with identical arguments. Capture each run's upper layer as `D1` and `D2`. If the hint holds, `D1 == D2` after normalisation. If they differ, the diff-of-diffs isolates exactly what the second call added.

*Fully deterministic. Subject to the caching confound described in §8.*

### `openWorldHint`

Run in a network namespace. Two modes:
- **Strict** — no route out. Any egress attempt fails and is logged. A tool that cannot function contradicts a `false` declaration.
- **Instrumented** — veth pair plus intercepting proxy. Every connection is logged with its destination, distinguishing closed-world traffic (a database inside the sandbox) from open-world traffic (an external API).

*Fully deterministic at the socket layer.*

### `destructiveHint`

The only annotation requiring judgement: distinguishing destructive mutation from additive mutation is semantic, not mechanical. Handled separately from the deterministic three. Approach: classify observed changesets (deletions and overwrites versus pure additions) as a mechanical proxy, and measure agreement between that proxy, model classification, and human labels on a held-out set.

*Treated as a secondary result. Do not let it contaminate the deterministic findings.*

---

## 7. Data model

A conformance record per (server, tool, annotation):

- Server and tool identity, protocol revision, harness version
- Declared value and whether it was explicit or defaulted
- Observed verdict: `holds` | `violated` | `unverifiable`
- Evidence reference (changeset digest, connection log, syscall log)
- Normalisation ruleset version applied
- Invocation arguments and fixture identity, for reproducibility

Records are append-only. Verdicts are reproducible from stored evidence plus the normalisation ruleset — a change to normalisation rules must be re-runnable over historical evidence without re-executing tools.

---

## 8. Known limitations

These are design constraints, not deferred work. Each must appear in published results.

**External state invisibility.** Tools that mutate a remote service produce no local changeset. Mitigation: seed mock backends inside the sandbox and redirect the tool to them. Where that is impractical, the verdict is `unverifiable` — not `holds`. Silently treating unobservable effects as absence of effects would be the worst possible failure mode for a trust signal.

**Semantic argument validity.** Schema-driven generation yields structurally valid arguments that frequently fail semantically. Fixtures reduce this but do not eliminate it, and coverage will be uneven across servers.

**Normalisation sensitivity.** Timestamps, UUIDs, and ordering produce spurious diffs. Over-normalising hides real mutations; under-normalising makes idempotent tools appear non-idempotent. The ruleset is a first-class, versioned artifact and a primary source of methodological criticism.

**Caching confound.** A tool may show no second-call effect due to internal caching rather than idempotent design. Distinguishing these requires cache-busting or an explicit ambiguity verdict.

**Observation evasion.** A sufficiently adversarial tool could detect the sandbox and behave differently. Out of scope, but worth stating.

---

## 9. Key design decisions

**Overlayfs upper layer over snapshot-and-diff.** Lower complexity, kernel-provided changeset, and directly expresses both `readOnlyHint` and `idempotentHint` protocols. Cost: Linux-only, and overlayfs semantics (whiteouts, opaque directories) must be understood correctly to interpret the diff.

**Hand-rolled namespaces over a container runtime.** Using Docker or an existing sandbox library would be faster and more portable, but abstracts away the layer this project exists to work at, and makes fine-grained observation harder. Cost: significantly more implementation effort and Linux-specific code.

**`unverifiable` as a first-class verdict.** A binary pass/fail forces false confidence on unobservable cases. Cost: results are harder to summarise and less headline-friendly. Accepted deliberately.

**Deterministic annotations prioritised over `destructiveHint`.** Three of four annotations admit mechanical verification. Leading with those keeps the core findings free of model-judgement softness.

---

## 10. Phasing

**Phase 1 — end-to-end thinnest path.** Mount namespace, overlayfs, timeout, one real MCP server, `readOnlyHint` only. Crudest containment that works. Target: a real verdict on a real tool within two weeks.

**Phase 2 — deterministic core.** Add PID/user namespaces, cgroups, and the `idempotentHint` protocol. Build the normalisation ruleset against observed noise rather than in anticipation of it.

**Phase 3 — network.** Network namespace, intercepting proxy, `openWorldHint` protocol, mock backend redirection.

**Phase 4 — hardening.** seccomp-bpf filter, denied-syscall logging, adversarial-input handling.

**Phase 5 — audit and publication.** Run at scale over public servers. Publish aggregate mismatch rates by annotation type, the normalisation ruleset, and the limitations above.

The project produces publishable findings from the end of Phase 2. Each subsequent phase widens coverage rather than unlocking value — a deliberate property, chosen so the work degrades gracefully under schedule pressure.

**Anti-goal for scheduling:** do not build the full containment stack before producing the first verdict. The dominant failure mode for this project is a complete sandbox with no findings attached.

---

## 11. Prior art to survey before implementation

- MCP Specification Enhancement Proposals touching annotations, trust, and tool metadata
- Existing per-server annotation testing tools (developer-side CI testing, as distinct from third-party audit)
- Published work treating annotations as a risk vocabulary for gateways and policy engines
- Container and sandbox escape literature, for the containment design
- Differential and metamorphic testing methodology, for the idempotency protocol

If an ecosystem-wide conformance audit already exists, this design should be reframed as either an extension of it or a replication with a different containment approach.

---

## 12. Open questions

1. What fraction of public MCP servers declare annotations explicitly at all? This determines whether the headline finding is about *mismatch* or about *absence*.
2. Can mock-backend redirection be made general, or does each server require bespoke fixtures? This is the primary determinant of achievable audit scale.
3. Should verdicts be published per named server, or only in aggregate? Naming servers is more useful and creates disclosure obligations.
4. Is there a responsible-disclosure path for a server whose annotations are found to be violated in a security-relevant way?