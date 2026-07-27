# MCP Annotation Conformance Harness

A harness that executes an MCP tool under observation and determines whether its declared
behavioural annotations match its observed behaviour.

The Model Context Protocol defines four behavioural annotations — `readOnlyHint`,
`destructiveHint`, `idempotentHint`, `openWorldHint` — that clients use to decide whether a
tool call proceeds automatically or requires human confirmation. By specification they are
*hints rather than guarantees*: self-asserted, and unaudited across the ecosystem. A buggy or
malicious server can declare a destructive tool read-only and bypass the confirmation path
that protects the user.

This project builds the missing verification layer.

**Status:** Pre-implementation. No code yet.

## Documents

| Document | What it is |
|---|---|
| [docs/design.md](docs/design.md) | Original high-level design. Problem, trust model, verification protocols, known limitations. |
| [docs/architecture.md](docs/architecture.md) | Implementable architecture. Supersedes the design doc; assumes you have read it. Contains the ADRs. |
| [docs/tasks.md](docs/tasks.md) | Canonical implementation backlog, mirrored into GitHub Issues. |

## Shape of the work

Two pipelines. **Census** enumerates tools and measures how many declare annotations at all —
no sandbox, no execution, publishable on its own. **Conformance** executes tools inside a
hand-rolled sandbox where the containment boundary *is* the measurement instrument: the
overlayfs upper layer is the filesystem changeset, the network namespace yields the connection
log, seccomp denials reveal attempted escapes.

Three annotations admit mechanical verification. `destructiveHint` is semantic, and is
quarantined from the deterministic core rather than blended into it.

`unverifiable` is a first-class verdict. Silently treating unobservable effects as absence of
effects would be the worst possible failure mode for a trust signal.

## Non-goals

Not a policy engine, gateway, or runtime — this produces a trust signal such systems consume.
Not a general MCP test framework for server authors. Not a security scanner for prompt
injection or supply-chain risk. Not a judgement on whether a tool is useful or correct, only
on whether it behaves as annotated.

## Author

Jun Cui
