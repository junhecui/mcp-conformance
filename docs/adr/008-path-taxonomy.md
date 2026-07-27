# ADR-008: The `user_state` / `server_internal` / `ephemeral` path taxonomy

**Status:** Accepted
**Date:** 2026-07-26
**Author:** Jun Cui
**Closes:** [P1-01](../tasks.md#p1-01--path-taxonomy-decision--adr-008)
**Blocks:** P1-06 (ruleset v1), P2-10 (ruleset v2)
**Related:** architecture.md §4.3, design.md §9

---

## Context

`readOnlyHint` verification (architecture.md §4.3) inspects the overlayfs upper layer
produced by a single tool invocation. A naive verdict — "non-empty upper layer contradicts
`readOnlyHint: true`" — is too blunt: a tool that appends one line to its own log file, or
touches a lock file while starting up, has not violated a user's reasonable reading of "does
not modify state," even though the kernel-level changeset is non-empty.

architecture.md §4.3 already names the shape of the fix and defers the substance to this
ADR:

> Recommend: classify changeset paths into `user_state`, `server_internal`, and `ephemeral`,
> and emit the verdict against `user_state` while reporting the other two. This gives
> critics something to argue with that isn't the verdict itself.

Two things this ADR must decide that architecture.md leaves open: **where the classification
rules live**, and **what they actually say**.

---

## Decision

### Where the rules live

Classification rules are **versioned data in `rulesets/`, not Rust code** — the same
constraint P1-06 states for the normaliser generally ("Rulesets are versioned data in
`rulesets/`, not code"). Concretely: each ruleset version carries an `ephemeral` glob list
and a `server_internal` glob list; `normalise` matches a changeset path against both in
order and falls through to `user_state` as the default.

This matters for the same reason ADR-005 insists `normalise` be pure: a taxonomy is a
falsifiable empirical claim about what real tools' internal-state paths look like, and
P2-10 already commits to deriving ruleset v2 from *measured* noise across ≥50 tools. If the
taxonomy were hardcoded in `normalise`'s source, correcting a bad classification would be a
recompile-and-redeploy instead of a ruleset edit, and "audit v1 against observed `N` and
delete what fails" (P2-10) would not be possible without a code change for every deletion.

### What the rules say (ruleset v1)

**Default: `user_state`.** Any path not matched by one of the two allowlists below is
`user_state`. This is the conservative direction on purpose, mirroring `unverifiable` as a
first-class verdict rather than `holds`: misclassifying a path as `user_state` when it was
actually internal bookkeeping makes a tool look *less* read-only than it truly is. The
failure mode is a false suspicion a human can investigate and dismiss, not a false
confidence a human has no reason to question. Getting this default backwards would be
design.md §8's "worst available failure mode" again, one layer up.

**`ephemeral`** — patterns that are noise intrinsic to process execution on Linux, not
specific to any tool's declared behaviour, and therefore never diagnostic of whether *this*
tool is read-only:

```
/tmp/**
/var/tmp/**
/run/**
**/*.lock
**/*.pid
**/*.sock
```

**`server_internal`** — patterns matching conventional tool-owned state directories, by
name rather than by absolute location, since the same tool can be installed at different
paths across fixtures:

```
**/.cache/**
**/.config/**
**/.local/state/**
**/__pycache__/**
**/node_modules/.cache/**
```

Both lists are short and conservative on purpose. A pattern's presence here is a claim that
matching it is *never* diagnostic of a read-only violation — a much higher bar than "this
looks like the kind of thing a tool's cache directory is named." ADR-003's discipline
applies here too, transplanted from noise floors to path taxonomy: **a pattern that has not
been observed to actually appear in a real tool's changeset should not be in ruleset v1**,
because an unobserved pattern is a guess about what real tools do, not a finding. The lists
above are seeded from directory-naming conventions general enough to predate this project
(XDG Base Directory spec, CPython's `__pycache__`, npm's cache layout) precisely so that
seeding them does not itself constitute the guess ADR-003 warns against — every entry is a
name with an existing external specification, not an invention.

### Verdict semantics

`readOnlyHint` is decided against the `user_state`-classified subset of the changeset only.
The `server_internal` and `ephemeral` subsets are still recorded on the evidence and
reported alongside the verdict — never discarded — so a published mismatch has "the tool
also touched these paths, classified as follows" attached, and a critic's objection has
something concrete to be about.

### `world` provisioner interaction (forward reference, not decided here)

For bespoke per-server fixtures, P2-05's world provisioner may know more than a path-name
heuristic can — e.g., that a given tool's installation directory is entirely its own
internal state regardless of what the files inside are named. When that's available, it is
authoritative and takes precedence over the glob lists above. This ADR does not specify that
interface; it only establishes that the *default*, fixture-agnostic classification is
glob-based data, so P2-05 has a documented fallback to override rather than a blank slate to
design against.

---

## Options considered

| Option | Where rules live | Precision | Cost |
|---|---|---|---|
| **A: versioned glob lists in `rulesets/`** (chosen) | Data | Coarse, name-based | Low to start; improves empirically via P2-10 |
| B: taxonomy hardcoded in `normalise` | Code | Same precision as A | Violates P1-06's "rulesets are data, not code"; every correction is a recompile |
| C: per-tool taxonomy supplied by the fixture author | Data, but per-fixture | High for Class A servers with bespoke fixtures | No default for the generic-fixture / no-bespoke-fixture case P3-06 measures; doesn't scale to census-adjacent corpus sizes |
| D: infer from filesystem semantics (e.g. "was this path pre-existing in the base layer vs. newly created") | Derived from evidence, not a list | Doesn't address the actual question — a pre-existing log file being *appended to* is exactly the server-internal case this ADR exists for | Rejected: solves a different problem |

Option A is the default; Option C is not rejected outright — see the `world` provisioner
interaction above — but is deliberately out of this ADR's scope, since P2-05 hasn't landed
yet and speculating about its interface here would be exactly the kind of decision made
before the evidence that should inform it exists.

---

## Consequences

**Good.**

- P1-06 is unblocked: `normalise` has a concrete, data-driven rule to implement rather than
  an open question.
- The taxonomy inherits ADR-005's reproducibility property for free — it's ruleset data, so
  `(evidence, ruleset_version) -> canonical_changeset` stays a pure function, and P1-09's
  replay test covers taxonomy changes the same way it covers any other ruleset edit.
- Ruleset v1 is honest about being a starting point: both lists are short, sourced from
  external conventions rather than invented, and explicitly designed to be pared down or
  extended once P2-08 starts measuring real noise floors.

**Costs, accepted.**

- **Coarse today.** A tool with an unconventionally-named internal cache directory will
  have it counted as `user_state` and will look less read-only than it is, until either the
  glob lists grow (backed by observed evidence, per ADR-003's discipline) or a bespoke
  fixture overrides the classification. This is the accepted direction of error, not an
  oversight.
- **Name-based matching can be gamed.** A tool could name a directory it actually uses for
  genuine user-facing output `.cache` to launder it into `server_internal`. Out of scope for
  this ADR specifically, but it's the same category of concern design.md §8 already flags
  as "observation evasion" — out of scope for the harness generally, not newly introduced
  here.
- **No mechanism yet for the `world` provisioner override.** Left as a forward reference
  rather than specified, per the table above.
