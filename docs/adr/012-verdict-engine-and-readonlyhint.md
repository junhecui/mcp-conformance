# ADR-012: Verdict engine shape, the gate precondition as a type, and `readOnlyHint`

**Status:** Accepted
**Date:** 2026-10-07
**Author:** Jun Cui
**Closes:** [P1-07](../tasks.md#p1-07-verdict-engine--readonlyhint)
**Blocks:** P1-05 (integrity gate v1 — decision 2 is the interface it must implement),
P1-08 (first real verdict), P1-09 (replay test), P2-09 (`idempotentHint`), P2-11 (reason-code
taxonomy), P5-02 (offline derivation), P5-04 (aggregate reporting)
**Related:** ADR-002 (containability / oracle tagging), ADR-004 (integrity gate as a hard
precondition), ADR-005 (pure, offline derivation), ADR-008 (path taxonomy), ADR-011
(normaliser semantics), architecture.md §4.3, §5.1 and §6, design.md §1, §3 and §8,
`crates/probe/src/protocol.rs`, commit `832d990`

---

## Context

P1-06 landed the normaliser, so the verdict engine's input exists: a `CanonicalChangeset`
partitioned `user_state` / `server_internal` / `ephemeral`, derived purely from
`(evidence, ruleset)`. `crates/verdict` was a `todo!()` with one placeholder signature,
`read_only_hint(declared: bool, d1: &CanonicalChangeset)`.

That signature is wrong in a specific and instructive way, and fixing it is most of this
ADR. It can be called with a changeset from a run that timed out, from a run whose tool call
returned `isError: true`, or from a run the integrity gate never looked at — and in each of
those cases an empty `user_state` partition reads as `readOnlyHint: holds`, which design.md
§8 names as *"the worst possible failure mode for a trust signal"*. Commit `832d990` fixed
exactly that bug in Track B: `probe::runner::invoke` treated a `tools/call` result carrying
`isError: true` as a successful invocation, so a tool that rejected its placeholder arguments
and therefore changed nothing produced a false `holds`.

The lesson from that bug is not "remember to check `isError`". It is that a bare pair of
`(declaration, changeset)` is a type that *permits* the mistake, and the fix was a branch
someone had to think to write. Three things also arrived as assignments from P1-06's review
passes: nothing in the schema records which `normalise` produced a verdict, it was undecided
whether `VERDICT.ruleset_version` holds a label or the tamper-evident identity, and a
derivation abort leaves no row at all while a `NormaliseError` has no verdict path either.

Everything below is implemented in `crates/verdict` (the engine), `crates/datamodel` (the
shared vocabulary the purity rule forces the boundary types into), `crates/normalise` (one
`From` impl), and `crates/store` (migration `0002` and the typed insertion path).

---

## Decision

### 1. The engine's input is a *run*, not a changeset

```rust
pub fn read_only_hint(declared: Declared, run: &GatedRun<'_>) -> Assessment
```

`GatedRun` borrows a `datamodel::GateAttestation` and owns an `Observation`, and
`Observation::new` — the only constructor — takes **both** halves of what happened:

```rust
Observation::new(
    call: InvocationResult,                                   // Completed | ToolReportedError | NoResult
    derived: Result<&CanonicalChangeset, DerivationFailure>,
)
```

There is no way to hand the engine a changeset while omitting how the call went, and no way
to hand it anything at all without an attestation. The three invariants the engine exists to
uphold are then properties of the types rather than of its body: gate-passed only (decision
2), `holds` only from a completed invocation (decision 4), and `unverifiable` only with a
reason (decision 9).

*Rejected: keeping `(declared: bool, d1: &CanonicalChangeset)` and adding the checks inside
`read_only_hint`.* It is smaller, and it is the shape that produced `832d990`'s bug one
oracle over. The checks would be correct on the day they were written and the signature would
keep inviting the next caller to skip them.

### 2. The gate precondition is a type the gate mints — and the gate is not built

ADR-004 makes the integrity gate a hard precondition: no evidence reaches the verdict engine
until it passes, and the gate *"is not configurable off"*. P1-05 has not been built, and
`docs/HANDOFF.md` §4.5 says explicitly not to build or stub it. So what P1-07 owes is the
*interface*, designed so that standing it up needs none of the gate's logic.

`datamodel::GateAttestation` is an opaque value identifying one `RUN.run_id` — the same key
`INTEGRITY` is itself keyed on in architecture.md §6, because a gate decision is per-run. Its
field is private, it is deliberately not `Clone`, and its only constructor is the default
body of:

```rust
pub trait IntegrityGate {
    fn attest_gate_passed(&self, run_id: &str) -> GateAttestation { /* ... */ }
}
```

**The attestation carries no gate logic and must not acquire any.** Which runs pass, which
of architecture.md §5.1's four branches produced which reason code, and what facts the gate
inspects are P1-05's and P2-03's to decide against real runs. Writing
`attest(clean_teardown && caps_respected && !timed_out)` here would be implementing that task
from an invented field list, before `sandbox`, `observe` or `integrity` exist. What the type
does instead is make the dependency unavoidable while the gate is still absent — the gate
will have exactly one thing to add (`impl IntegrityGate for …` on its pass branch) and no
engine signature to change.

**The residual gap, stated plainly rather than overstated.** Rust cannot restrict
construction to one crate. A Cargo feature unifies across the dependency graph, so
feature-gating the constructor would expose it to everything; there is no `friend`
visibility; and abusing `unsafe fn` for a non-memory invariant would be worse than the
problem. What the trait buys over a free function is that minting an attestation requires
*declaring yourself the integrity gate* — a conspicuous, greppable `impl` at a named call
site — rather than being reachable from any `&CanonicalChangeset` a caller happens to hold.
Both of this change's test suites do exactly that, in the open, with a `FakeGate` whose doc
comment says what it is standing in for.

*Rejected: an `xtask` check (in `cargo purity`'s style) asserting that only
`crates/integrity` implements the trait.* Tempting, and the repo has precedent for enforcing
what the type system cannot. It was dropped because the check must exempt test code — which
legitimately needs to mint — and a grep that has to distinguish `#[cfg(test)]` modules from
live code is exactly the kind of approximate rule that passes while being wrong. Reconsider
once the gate exists and a real implementor is there to compare against.

### 3. `readOnlyHint: false` with nothing observed is `unverifiable`, not `holds`

The completed-invocation table is, row for row, the same shape as Track B's
`probe::protocol::assess_read_only`:

| declared (effective) | `user_state` changed | outcome | Track B's row |
|---|---|---|---|
| `true` | no | `holds` | `holds` |
| `true` | yes | `violated` | `violated` |
| `false` | yes | `holds` | `holds` |
| `false` | no | `unverifiable` | `unverifiable` |

The fourth row is the one worth arguing about, and the two oracles agree on the outcome while
differing on the code: Track B says `probe_surface_incomplete`, this engine says
`no_user_state_change`. The outcome is the same for structurally the same reason — nothing
was contradicted and nothing was confirmed — but the *why* differs, and a shared code would
misdescribe one of them. Track B's names a weakness of its oracle (it sees only the state the
server chose to expose as a resource). Here the oracle is strong for local filesystem writes;
what fails is the declaration's falsifiability: `false` is the conservative spec default, so
there is nothing for an absence of writes to contradict.

Three reasons it is not `holds`:

- **`false` is not a claim.** Per design.md §1, `readOnlyHint` defaults to `false`, and
  48.6% of census-era tools declare no annotations at all. A `holds` for every one of them
  that wrote nothing locally in one arm would fill the published `holds` column with rows
  that confirm nothing — the mirror of the false-confidence failure, arriving through the
  front door.
- **Phase 1 cannot see the places those tools act.** design.md §8's external-state
  invisibility is at its sharpest exactly here: a tool wrapping a remote API mutates nothing
  locally, and Phase 1 has no network observation at all (that is P3-01). Reporting `holds`
  would be *"silently treating unobservable effects as absence of effects"*, which §8 names
  as the worst available failure mode.
- **It is not `violated` either.** Nobody reads `false` as a positive claim "I do modify
  state", and a *defaulted* `false` is not a claim in any sense. Over-claiming is what this
  harness exists to catch; under-claiming is not a safety problem.

### 4. A contradiction survives a failed invocation; a confirmation does not

This is the sharper form of *"an empty changeset from a failed invocation is `unverifiable`,
never `holds`"*, and the asymmetry is deliberate.

- A change **present** in `user_state` is a write the kernel recorded. A tool declaring
  `readOnlyHint: true` that wrote there has contradicted itself whether or not its call then
  reported an error, so `violated` is reachable from a failed invocation. Suppressing it
  would discard a real finding to be tidy.
- A change **absent**, or a `false` declaration **confirmed**, are both conclusions a failed
  invocation cannot license: the tool may simply never have run its effectful path.

Made structural rather than conventional. `verdict::observation::Completion` is a zero-sized
token with a private field, minted only by `Observation::completion()` — which lives in the
same module, so no other module in the crate can construct one either — and
`Assessment::holds` takes one **by value**. `Outcome::Holds` is therefore **unreachable
through this engine** for a run whose invocation did not complete; it is a type error, not a
missing branch. `Assessment::violated` deliberately takes no witness.
`holds_is_unreachable_for_every_invocation_result_but_completed` checks the whole reachable
table anyway, with a floor asserting the loop actually reaches `holds`, because the test is
what catches someone widening `completion()` later.

**"Unreachable through this engine", not "unconstructible"** — the first draft of this ADR
said the latter, and a review pass falsified it by compiling an external crate that forged a
`holds` three ways: a bare `Assessment { outcome: Outcome::Holds, .. }` struct literal, a
fake `impl datamodel::IntegrityGate` (disclosed below as decision 2's residual gap), and
`a.outcome = Outcome::Holds; a.reason = None;` applied to an `Assessment` the engine had just
produced. The third was disclosed nowhere and is the most plausible of the three, because a
driver post-processing a result never has to write `Outcome::Holds` beside a struct literal
to reach it.

`Assessment`'s fields are therefore **private, with accessors** (`outcome()`, `reason()`,
`call()`, `reported()`, `ruleset_identity()`), which closes all three for every crate but
`verdict` itself. That was weighed against leaving them public to match
`probe::protocol::ProbeAssessment`'s shape, and the asymmetry is the right way round:
`Assessment` has a structural invariant to protect and `ProbeAssessment` does not. Four
accessors is the whole cost, and the integration test reads them.

What that does **not** buy, stated rather than implied:

- **`store::db::insert_verdict` is the real forgery surface**, and no signature in `verdict`
  can guard it. A driver can write any row it likes without going near an `Assessment`. The
  harness operator is trusted (design.md §3); what these types defend against is a *buggy*
  driver, which is also what the `Completion` token defends against.
- **Invariant 3 is enforced at construction but not *maintained* by the database.** The
  `CHECK` catches an `unverifiable` row with a cleared `reason_code` at insert time. It
  cannot catch a driver that flipped an `Unverifiable` assessment to `Holds` *and* dropped
  the reason, because the resulting row is internally consistent. Private fields are what
  stop that happening to an engine-produced value; nothing stops it happening to a
  hand-built row.
- **`probe::protocol::ProbeAssessment` still has public fields.** Left alone deliberately —
  it has no witness to bypass — but when P2-11 single-sources the reason-code taxonomy and
  the two assessment types converge, this is a decision to revisit rather than inherit.

### 5. A `NormaliseError` is an `unverifiable` verdict; a derivation *abort* is not

P1-06's carry-forward findings left this open. Decided: **yes**, and the distinction is
whether the failure is inside the pure closure.

A `NormaliseError` is a function of `(evidence, ruleset)` and nothing else. Re-running the
derivation reaches the same classification, so there is a stable, reproducible fact to
record, and recording it is strictly better than a missing row — a malformed capture from a
hostile tool is a finding. A derivation job that *aborts* (killed, out of memory against
ADR-011's measured ~20× multiplier) is not in the closure, is not reproducible from the
stored inputs, and has no representation: it leaves no row, which is indistinguishable from
"not yet derived", and must be counted in P5-04's no-verdict fraction. The code admits this
rather than papering over it — `DerivationFailure`'s doc comment says so.

The classification is `datamodel::DerivationFailure` — `MalformedEvidence` |
`MalformedBaseLayer` | `InvalidRuleset` — and it lives in `datamodel` because ADR-005 leaves
no other home: `verdict` may not depend on `normalise` and `normalise` may not depend on
`verdict` (neither is on `cargo purity`'s allowlist). The single
`impl From<&NormaliseError> for DerivationFailure` lives in `normalise`, next to the error it
maps, so two derivation drivers cannot classify the same error differently.

The three variants keep **distinct reason codes**, and the split is by *whose fault the
failure is* rather than by where in the pipeline it surfaced, because the reason code reaches
publication:

| variant | code | whose fault |
|---|---|---|
| `MalformedEvidence` | `malformed_evidence` | the tool's — it wrote the upper-layer tree |
| `MalformedBaseLayer` | `malformed_base_layer` | the harness's |
| `InvalidRuleset` | `invalid_ruleset` | the harness's |

`MalformedBaseLayer` is a **correction made during P1-07's review passes**, not an original
decision. The first implementation mapped both `NormaliseError::MalformedUpperLayer` and
`::MalformedBaseLayer` to `MalformedEvidence`, whose doc comment says *"the tool under test is
hostile by assumption, so this is a finding about the evidence, not an internal error"*. That
is true of the upper layer and false of the base layer: the base is built by the operator
(`world::base_layer`, P1-02) and mounted read-only beneath the tool, which has no way to
write to it. So an undecodable base layer is a harness fault — identical in kind to the
uncompilable ruleset that `InvalidRuleset` was deliberately split out to avoid publishing
against a server. As written, a harness bug published as `malformed_evidence` against a
server and, read the other way, handed any server deniability for a real malformed capture.

### 6. `VERDICT.derivation_version` added now

ADR-011's own disclosed cost: the `mtime`/`inode` exclusions, the overlay-private xattr name
set, the glob dialect and the structural-omission rule all live in `normalise`'s source
rather than in ruleset data. So a verdict is reproducible from `(evidence, ruleset_identity)`
**plus the code that derived it**, and F-06's schema recorded only the first two —
`harness_version` exists on `RUN`, which is the *execution*, not the derivation, and ADR-005
exists precisely so the two can happen at different times on different machines.

Added as a nullable column by migration `0002`, with `store::db::derivation_version()`
supplying the value: `MCP_CONFORMANCE_BUILD_ID` (a commit SHA, set by CI or the orchestrator)
read at compile time, falling back to `"<pkg version>+unpinned"`. The fallback is the point —
the workspace version is a static `0.1.0` that would look authoritative while identifying
nothing, so an unidentified build says so in every row it writes.

*Rejected: reusing `RUN.harness_version`.* It records the binary that executed the tool. A
re-derivation months later under a new ruleset runs different code against the same run, and
that is the whole premise of ADR-005.

### 7. `VERDICT.ruleset_identity` holds the full identity — a two-table change

`CanonicalChangeset.ruleset_identity` already carries the tamper-evident form,
`"v1+sha256:<hex of the exact ruleset file bytes>"` (ADR-011 decision 9). Storing the bare
label `"v1"` on the verdict would discard exactly the property that makes ADR-005's claim —
hand a reviewer the evidence and the ruleset and they reproduce every verdict — stronger than
a promise: a label can be reused over edited rules, a digest cannot.

Because `VERDICT.ruleset_version` is a foreign key into `RULESET`, this is necessarily a
two-table decision, as P1-06's note flagged. `RULESET`'s primary key becomes the identity
too, and migration `0002` renames **both** columns to `ruleset_identity` so the name never
disagrees with the contents. `ALTER TABLE … RENAME COLUMN` is used rather than a 12-step
table rebuild — since SQLite 3.25 it rewrites references to the column in other tables'
foreign-key clauses, which is what is needed here — and
`migration_0002_preserves_rows_and_keeps_the_fk` proves the rewrite happened on the SQLite
`rusqlite` actually bundles, against real 0001-era data, rather than trusting the
documentation.

*Rejected: keeping the column names and silently changing their contents.* It is a smaller
diff and leaves a column called `_version` holding a digest, which is the kind of wart that
costs a reader an hour at a time.

### 8. Everything a kernel-changeset verdict depends on travels as one value

`VerdictRecord`'s `oracle` + `ruleset_version` + the new `derivation_version` are not
independent: a `kernel_changeset` verdict is *defined* as the output of normalising evidence
under a named ruleset with a particular build of the derivation code, and a `protocol_probe`
verdict has no changeset and therefore neither. As three settable fields, "kernel changeset,
ruleset unknown" is a representable row nobody can reproduce. They are now one
`VerdictProvenance` enum, so that row does not typecheck.

SQLite cannot express the same constraint — a conditional `CHECK` cannot be added by `ALTER
TABLE` — so the type is where it lives. This is the same move B-02 made for the `oracle`
column itself (*"these functions are the one place that mapping is allowed to live"*), one
level up.

**P1-07's review passes found three more things that belong in that value, and the finding
has one shape in all three cases: a distinction that exists correctly in the types and is
destroyed at the storage boundary, so the published artefact cannot support a claim this
design makes.** Migration `0002` was still uncommitted, which is why the schema work landed
here rather than in P1-08.

- **Partition counts.** `read_only_hint` computes `PartitionCounts::of(changeset)` for every
  outcome, and nothing stored them. Demonstrated consequence: a tool declaring
  `readOnlyHint: true` that wrote `~/.cache/stolen-notes.md`, `~/.config/ssh-key-copy` and
  `~/invoice-2026.pdf.lock` — overwriting a real document through an allowlisted suffix —
  stored a row **identical in every column** to a tool that touched nothing. Both `holds`,
  both with no reason. So architecture.md §4.3's promise (*"report the other two partitions,
  so critics have something to argue with that isn't the verdict itself"*) was true of the
  type and false of the artefact, and every known ADR-008 laundering route went from visible
  and arguable to invisible. `VERDICT.{user_state,server_internal,ephemeral}_count`, with
  `counts` **required** on `VerdictProvenance::KernelChangeset` so the row cannot be written
  without them. `PartitionCounts` moved to `datamodel` for this, the same forced move
  `DerivationFailure` made in decision 5: `store` has to name it and ADR-005 forbids an edge
  between `store` and `verdict` in either direction.
- **The invocation result.** Decision 4 deliberately allows a `violated` resting on a
  *failed* invocation, and this ADR intends to keep it — but with nothing recording the call,
  such a row is byte-identical in storage to a `violated` from a clean successful call. P5-03
  could not triage the disclosure, P5-04 could not report the populations separately, and the
  objection decision 4's own cost section predicts (*"your harness called my tool a violation
  when the call errored"*) could not be answered from the record. `Assessment` now carries
  `call: InvocationResult`, `VERDICT.invocation_result` stores it, and it is required on both
  kernel-changeset provenance variants. `InvocationResult` moved to `datamodel` alongside
  `PartitionCounts`, for the same reason and with `verdict` re-exporting it.
- **The run, and hence the evidence.** architecture.md §6 declares
  `EVIDENCE ||--o{ VERDICT : supports` and no migration implemented it: `VERDICT` carried
  neither `run_id` nor an evidence digest, so there was no path from a verdict row to the two
  blobs that produced it — and a tampered verdict row could not be caught by re-derivation,
  because nothing said which evidence it claimed. This ADR's own consequence section claimed
  a verdict *"names everything it depends on"*; it named two of three.
  `VERDICT.run_id`, a nullable FK to `RUN`, closes it. The join goes through `RUN` rather
  than a digest column because a kernel-changeset verdict rests on **two** blobs (base and
  upper layer) and `EVIDENCE` is already keyed by `run_id`; a single `evidence_digest` could
  only ever name one of them. `GatedRun::run_id()` is what a driver puts there, which is the
  id the gate attested — and it is a *separate field* from `provenance` rather than part of
  it, because both oracles can have runs even though only one has a derivation.

**And a fourth row shape the three-field version could not express at all.**
`read_only_hint` returns `Assessment::unverifiable(code, call, None, None)` on a derivation
failure — no ruleset identity, correctly, since decision 7's whole point is that the identity
comes *off the changeset* and there is no changeset. But `KernelChangeset` requires
`ruleset_identity` non-optionally, so a driver holding a `malformed_evidence` assessment had
three bad choices and no good one: write `ProtocolProbe` (a false oracle, exactly what
ADR-002 and B-03 exist to prevent), restate the loader's identity (contradicting the premise
that the caller must not), or drop the row (defeating decision 5). Hence
`VerdictProvenance::KernelChangesetDerivationFailed { derivation_version, call }`: the oracle
stays truthful and both absences are structural. It carries the derivation build and the
invocation result **and nothing else**, deliberately — there is nothing to count and no
identity to name, while *which build* decided the bytes were malformed is the most useful
fact about such a row, and the invocation result is a fact about the run rather than about
the derivation. That row shape had no storage test anywhere in the tree; it has two now
(`a_derivation_failure_row_keeps_the_kernel_oracle_with_no_ruleset` in `store::db`, and
`a_derivation_failure_is_stored_with_a_truthful_oracle_and_no_ruleset` end to end).

### 8a. `declared` is part of the aggregation key, not just of the row

Same shape as the above, one layer out: the distinction was in the row and destroyed in the
*report*. `store::aggregate` dropped `declared` from `VerdictSummary` and grouped by
`(annotation, oracle, outcome)`.

Decision 3 is right that declared-`false` plus an observed mutation is `holds` — but the
published aggregate then pools that with a genuinely verified read-only tool. The attack
needs no effort: declare `readOnlyHint: false`, **or declare nothing at all**
(`Declared::Defaulted` reaches the same arm, and 48.6% of census-era tools declare nothing),
touch one `user_state` path, return successfully. Every tool yields `holds`. Four different
realities — one quiet tool, one that laundered three user-facing writes, two that merely
admitted they write — publish as the single number `Holds = 4`. B-03's cross-oracle guard
passes it, because it checks oracle disclosure and not this axis.

So `declared` is now handled exactly as `oracle` is: part of `aggregate`'s grouping key, a
mandatory field on `AggregateRow`, and an `Option` on `ReportRow` specifically so an
undisclosed declaration is representable and then rejected
(`AggregationError::DeclaredNotDisclosed`, alongside `OracleNotDisclosed`). A
`readOnlyHint / holds` count that does not say whether the declaration was `true` is as
uninterpretable as one that does not say which oracle.

**Not hypothetical on the data already published.** Re-deriving Track B's committed sweep
through the new aggregator splits its `readOnlyHint / holds = 5` into **4 declared-`true` and
1 declared-`false`**, and `idempotentHint / holds = 7` into **5 and 2**. The pooled form
overstates "verified read-only" by 1 and "verified idempotent" by 2 — small absolutely, and
the point is that the published table could not have told you.

### 9. The reason codes this engine emits, and one duplicated spelling

`invocation_failed`, `malformed_evidence`, `invalid_ruleset`, `no_user_state_change` — named
constants in `verdict::reason`, so the spelling cannot drift between the engine, its tests
and the stored row.

`invocation_failed` is deliberately the **same spelling** Track B uses for the same
situation, so the two oracles' records read the same way where they mean the same thing. It
is **not single-sourced**: `verdict` and `probe` cannot share a constant without an edge
ADR-005 forbids. P2-11 — *"closed set of reason codes, documented"* — is where the taxonomy
should become one shared artefact, probably in `datamodel` alongside `ReasonCode` itself.

All three non-`Completed` invocation results map to that one code today, even though
`InvocationResult` names three cases. The variants exist so the *caller* has to classify the
call (which is what `832d990` showed gets skipped otherwise) and so P2-11 can split the code
later with no API change; minting `invocation_no_result` now, before a single real run has
produced one, would be guessing at a code, which is ADR-003's discipline applied to the
taxonomy instead of to the ruleset.

### 10. `Declared` is an enum, not a `bool`

`Declared::{Explicit(bool), Defaulted}` with `effective(default) -> bool`, plus a named
`READ_ONLY_HINT_DEFAULT = false` constant citing design.md §1.

The two likeliest call-site errors are both invisible in a `bool`: passing *whether the
annotation was present* instead of its value, and hardcoding the wrong spec default — the
four annotations do not share one (`destructiveHint` and `openWorldHint` default to `true`),
so a stray `false` in the wrong protocol silently inverts a safety-relevant claim. The enum
also carries P0-05's explicit-versus-defaulted distinction into the verdict instead of
flattening it on the way in, even though both reach the same outcome: *that* difference is
the census's headline (design.md's open question 1 is about absence, not just mismatch), and
a type that erases it invites a later protocol to erase it too.

---

## Options considered

The three a reader is most likely to have expected to go the other way.

| # | Question | Chosen | Rejected alternative | Why |
|---|---|---|---|---|
| 1 | Expressing "gate-passed only" | A token type the gate mints, `impl`-gated | A `bool`/`IntegrityFindings` argument the engine checks | An argument the engine checks is a branch; the whole lesson of `832d990` is that a branch is what gets skipped. And checking findings *is* P1-05's logic, which §4.5 forbids writing here. |
| 2 | `declared: false`, nothing observed | `unverifiable` + `no_user_state_change` | `holds` — no contradiction is possible with a `false` declaration | Technically true and useless: it would make `holds` the modal outcome for the ~49% of tools that declare nothing, on the strength of an observation that confirms nothing, in the exact case design.md §8's external-state invisibility bites hardest. |
| 3 | A failed invocation with a non-empty `user_state` | `violated` when `readOnlyHint: true` | `unverifiable`, uniformly, for any failed invocation | Uniform is simpler to explain and discards a real finding: the kernel recorded the write, and a tool that wrote to user state and then errored still wrote to user state. The asymmetry (contradiction survives, confirmation does not) is the honest rule. |

---

## Consequences

**Good.**

- The false-`holds` gap is unreachable through this engine rather than guarded:
  `Assessment::holds` requires a `Completion` token no failed invocation can produce, the
  changeset cannot be supplied without the invocation result beside it, and `Assessment`'s
  fields are private so an engine-produced result cannot be mutated into a `holds` either
  (decision 4 — which also states what this does not buy).
- ADR-004's precondition has an interface before the gate exists, so P1-05 adds one `impl`
  and changes no signature — and nothing of P1-05's decisions has been pre-empted.
- A verdict now names everything it depends on, and all of it survives into the row: the
  evidence (via `run_id`, which joins to `EVIDENCE`'s digests), the exact ruleset *bytes*, the
  build of the derivation code, what the call did, and how many changes landed in each
  partition. ADR-005's reproducibility claim is checkable rather than aspirational, and
  `cargo purity` is still green with `verdict`'s closure at `{datamodel}` alone.
- The partitions a critic is invited to argue with are in the published artefact rather than
  only in the type, and a report cannot pool `holds` counts across declarations any more than
  across oracles (decisions 8 and 8a).
- Both oracles' `readOnlyHint` tables are the same shape, so a cross-oracle report (P5-04,
  B-03) is comparing like with like — and where they differ, the difference is one reason
  code with a written justification rather than a silent divergence.
- Derivation failures are verdicts with distinct reason codes instead of missing rows, and
  the one class of failure that genuinely cannot be a verdict (an abort) is named as such and
  handed to P5-04's no-verdict metric.

**Costs, accepted.**

- **"Only the gate can mint an attestation" is a convention backed by conspicuousness, not
  by the compiler.** See decision 2. Any crate can `impl IntegrityGate`. The gap is real and
  is why the `xtask` check is only deferred rather than rejected outright.
- **⚑ Flagged for P1-03 and P2-04: Arm 0's absence is exploitable in both directions, and
  the `holds` direction was undisclosed.** This ADR already noted that Arm 0 (base only, no
  invocation) is what separates server-startup writes from tool writes, and filed it under
  the dispute a maintainer might raise. It is worse than a dispute, and it runs both ways.

  *Forward.* A server that writes one `user_state` path at startup — unconditionally,
  whichever tool is called — makes **every** tool declaring `readOnlyHint: true` come back
  `violated`, including genuinely read-only ones, and needs no completed invocation to do it
  (decision 4 lets a contradiction survive a failed call). That is a reproducible, publicly
  demonstrable false-positive generator aimed at this harness's credibility. It also arrives
  *accidentally*, via an `atexit`/`SIGTERM` handler that writes on teardown — which does not
  fail the integrity gate, being neither a timeout nor a cap hit.

  *Reverse.* Declare `readOnlyHint: false` or nothing, write one `user_state` path at
  startup, let the call succeed → `holds` for every tool, with no tool having done anything.
  Combined with decision 8a's pooling that was an unlimited supply of published `holds` rows;
  with 8a fixed they are at least confined to the declared-`false` column, which is the
  column that means the least.

  **Requirement, not a suggestion: Arm 0's changeset must be *subtracted* before assessment,
  not merely reported beside it.** Reporting it leaves the subtraction as something a future
  driver has to remember, which is the failure mode this whole ADR is written against.
- **⚑ Flagged for P1-04 (observation collector): the capture walk must be *total*.**
  Anything it cannot represent has to be recorded as a distinguishable entry — never silently
  dropped, and never an error. A hostile tool cannot force a decode failure through file
  *content* alone (checked: `evtree` lengths are `u64`, the xattr count is `u32`, and Linux
  provides neither 2^32 xattrs nor a 2^64-byte path), but it can present nodes designed to
  make a walk fail or skip: a path unlinked mid-walk, a node that cannot be `stat`ed, an
  unexpected file type. An error hands the attacker `malformed_evidence`; a silent skip hands
  it something strictly better, laundered out of every partition and out of the counts
  decision 8 just made visible.
- **⚑ Flagged for P5-04: an aborted derivation launders into *absence*, which is better for
  an attacker than `unverifiable`.** Decision 5 says an abort leaves no row and belongs in
  P5-04's no-verdict fraction. The gap is that there is no derivation-*attempt* entity, so an
  abort is indistinguishable from "not yet derived" and P5-04 cannot compute that fraction
  from stored data at all — while `unverifiable` is at least counted. A hostile tool can
  reach it deliberately by emitting a large-but-perfectly-valid tree sized against ADR-011's
  measured ~20× memory multiplier. Two options worth recording rather than choosing here: a
  derivation-attempt row written *before* the pure call, or a decoded-evidence size cap with
  its own reason code instead of an abort.
- **⚑ `VERDICT.protocol_version` is server-controlled, unvalidated and unbounded.**
  `discovery`'s `extract_negotiated_version` accepts any JSON string with no length cap and
  no allowlist, and `protocol_version` is an axis P5-04 reports by. Latent today — nothing
  wires the two together and every test hardcodes the value — but it is the shortest path
  from a server-controlled string to a published grouping key. `declared` and `reason_code`
  are likewise free strings at the storage boundary; `VERDICT.reason_code` wants a `CHECK`
  once P2-11 closes the taxonomy.
- **⚑ Flagged for P2-08, and for ADR-003's threat surface.** Because the noise floor `N` is
  measured *per tool* from that tool's own `D1`/`D1'`, a tool with a timer-driven background
  writer **inflates its own tolerance**, and `D2 Δ D1 ⊆ N` then swallows its own second-call
  effect. Same Arm-0-shaped mechanism as above: cheap, always-on, needs no sandbox detection.
  Found while reviewing P1-07, recorded here so it is not lost before P2-08 starts.
- **Nothing binds an attestation to the evidence it attests.** `GateAttestation` carries a
  `run_id`, and the engine cannot check that the changeset it was handed came from that run —
  verifying that would mean hashing inside the pure closure, which ADR-011 decision 6 ruled
  out for good reasons. A buggy driver could pair run A's attestation with run B's changeset.
  Mitigation is P1-08's and P5-02's: one derivation step per run, with the `run_id` flowing
  from the same record that produced the evidence digests.
- **`violated` is reachable from a failed invocation, which maintainers will dispute.**
  Combined with ADR-011's copy-up consequence (overlayfs copies a file up on a write-intent
  open even if nothing is written), the most disputable verdict this engine can produce is
  "your tool errored out and we called it a read-only violation". It is the correct reading
  of the evidence and P5-03's disclosure workflow should expect the objection; Arm 0 (base
  only, no invocation) is what separates server-startup writes from tool writes, and P1-03
  owes that arm.
- **One reason code for three invocation failures.** `invocation_failed` covers a
  tool-level error, a JSON-RPC error, a crash and a kill. Honest today (nothing has been
  measured) and a loss of resolution P2-11 should recover.
- **Two spellings of `invocation_failed` in the tree.** Decision 9. A rename in one and not
  the other would silently split a published category; P2-11 must single-source it.

---

## Open questions this ADR does not close

- **Should the `IntegrityGate` trait acquire a required item once P1-05 exists?** A required
  method (say, the gate's own version, recorded onto the attestation as provenance) would
  make an accidental implementor less likely and give `VERDICT` a second provenance axis. It
  is not addable usefully before there is a real gate to shape it. Today's consequence is
  pinned by `an_attestation_identifies_the_run_not_the_gate_that_minted_it`
  (`crates/datamodel/src/lib.rs`): two implementors attesting one run produce **equal**
  attestations, so a verdict cannot say which gate licensed it.
- **Should `verdict` own the `Assessment → VerdictRecord` conversion?** It cannot (`store` is
  outside the purity closure and `verdict` cannot depend on it), and `store` deliberately does
  not depend on `verdict` either — the conversion lives at the driver, which today means
  P1-08's `xtask` and eventually P5-02's batch job. If a third driver appears, the mapping
  should move into one of them rather than being written a third time.
- **Where does the reason-code taxonomy live?** P2-11. `datamodel`, next to `ReasonCode`, is
  the only place both oracles can reach.
- **Does `Assessment` need the `adversarial_flag`?** ADR-004 accepts evidence from a run that
  attempted an escape and flags it, and P4-03 requires the flag to reach publication. It is
  an attribute of the *run*, not of the assessment, so it travels on `INTEGRITY` today and
  nothing here forecloses adding it to the attestation when P4-03 needs it.
- **The `(path, ChangeKind)` amendment to ADR-008 is still unimplemented**, and it bites
  here: a tool that *deletes* a base-layer file whose name matches an allowlist yields
  `user_state = []`, so this engine reads it as `holds` (or `no_user_state_change`). The
  engine is behaving exactly as ADR-008 specifies; the specification is what needs the
  amendment, which has its own task and its own review passes. It is now pinned where it
  actually happens — `normalise::tests::deleting_an_allowlisted_path_lands_in_ephemeral_today_not_user_state`
  runs a real whiteout of `/srv/app.lock` through `normalise` — so the amendment has to land
  as a visible test change. Until P1-07's review that claim was credited to a `verdict` test
  which hand-places a `Deleted` change into the `ephemeral` partition and never calls
  `normalise`, and a sweep found no test anywhere exercising the classifier on a deletion, so
  the amendment could have landed with every test green.
- **A precision note on ADR-011's omission invariant.** *"Every omission is backed by at
  least one reported descendant"* does not mean a **decisive** descendant: the backing change
  may itself be in `server_internal` or `ephemeral`. That is logically fine — the ancestor
  directory's change is wholly explained by the child — but the invariant reads stronger than
  it is, and what makes it harmless is that the counts are published, which is decision 8.
