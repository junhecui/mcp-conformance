# MCP Annotation Conformance Harness

A harness that executes an MCP tool under observation and determines whether its declared
behavioural annotations (`readOnlyHint`, `destructiveHint`, `idempotentHint`, `openWorldHint`)
match its observed behaviour. Status: pre-implementation, no code yet.

The documents below are the project's source of truth. `architecture.md` supersedes
`design.md` but assumes it as read; `tasks.md` is the canonical backlog and wins over GitHub
Issues on disagreement. Read them before proposing design changes or starting implementation
work — decisions already made there (e.g. the ADRs in architecture.md §9, the purity rule for
`normalise`/`verdict`, `unverifiable` as a first-class verdict) should not be re-litigated
without cause.

## Original design

@docs/design.md

## Implementable architecture (supersedes the above)

@docs/architecture.md

## Implementation backlog (canonical task list)

@docs/tasks.md

## Working orchestration model

When continuing backlog work in this repo (e.g. "continue on the task list"), operate as a
manager, not an individual contributor:

- **Do not read or write code directly.** Pick the next task from `tasks.md` by its own
  dependency graph and ⚑-blocking markers, then delegate implementation to subagents. Use an
  Explore/general-purpose subagent even for reconnaissance (checking repo state, whether a
  dependency is actually satisfied, whether something is already partially started) rather
  than reading files yourself.
- **Every task gets at least two review passes before being considered done**, run as
  separate subagents so their judgments aren't contaminated by the implementer's framing:
  - One subagent as a **pure code reviewer** — correctness, adherence to this project's
    invariants (purity rules for `normalise`/`verdict`, the `unverifiable`-not-`holds`
    discipline, oracle tagging, etc.), test coverage, simplicity.
  - One subagent as a **security specialist** — this project executes untrusted, actively
    hostile third-party code and code that touches live third-party infrastructure
    (design.md §3's trust model); review for containment gaps, injection, unsafe
    deserialization, and anywhere untrusted input (tool descriptions, server responses)
    could reach a privileged path.
- **Commit whenever a new feature/task lands**, after review feedback has been addressed —
  don't batch multiple tasks into one commit. Follow the repo's existing commit style
  (see `git log`).
- If a candidate task turns out to be blocked on missing infrastructure (e.g. P1-02 needs a
  Linux VM per F-00 that doesn't exist yet), say so and pick the next unblocked task rather
  than attempting it anyway.
