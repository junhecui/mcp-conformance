# `destructiveHint` human-labelling protocol (Q-02)

**Status: protocol specified, tooling built (`destructive::agreement`). No labelling has
been performed.** Producing the actual labelled set and its inter-rater agreement number
requires real, independent human raters — this document specifies the protocol precisely
enough for that work to happen, and the agreement statistic it will be scored with already
exists and is tested against known-correct arithmetic, but neither this document nor any
code in this repository can substitute for a human actually doing the labelling.
Fabricating labels or an agreement number here would misrepresent a real empirical claim as
one this project never made — exactly what every other result in `results/` goes out of its
way to avoid.

## Why this exists (ADR-006)

`destructiveHint` ("the tool's mutations are destructive rather than additive") is semantic,
not mechanical. Q-01's `destructive::partition` already answers a narrower, purely
structural question — did a path get deleted, overwritten, or newly created — but "is this
mutation destructive" is a judgement about what the mutation *means*, which the mechanical
proxy explicitly does not attempt (see its own doc comment). This protocol is how that
semantic judgement gets a ground truth to check Q-03's model classifier against, later, in
Q-04.

## What gets labelled

One item is one assessed run: the tool's `name` and `description` (**untrusted text — see
below**), the arguments it was invoked with, and Q-01's own mechanical partition of its
observed changeset (which `user_state` paths were added, versus deleted/overwritten — paths
only, not file contents). A rater is **not** shown the tool's own declared `destructiveHint`
value while labelling, to avoid anchoring on the very claim this label will eventually help
evaluate; it can be revealed afterward, for discussion, once both independent labels are
already recorded.

## Label set

Three categories, not two — an item a rater genuinely cannot decide is real information,
not a forced coin flip:

- **Destructive** — the observed mutation removes or replaces something a reasonable user
  would consider their own data or state.
- **Additive** — the observed mutation only introduces new state, nothing existing is lost
  or replaced in a way that matters to the user.
- **Ambiguous** — the rater cannot decide from what's shown (e.g. a mechanical overwrite of
  what looks like a cache or lock artifact the mechanical proxy's own ruleset-based
  exclusion should arguably have already filtered, but didn't; or genuinely insufficient
  context to judge intent).

`Ambiguous` is its own category in the agreement statistic, not silently folded into either
of the other two — two raters both saying "I can't tell" is agreement; a `Destructive`/
`Additive` split is real disagreement; and a `Destructive`/`Ambiguous` split is a different,
weaker kind of disagreement than the first. Collapsing the label set to binary before
computing agreement would erase that distinction.

## Untrusted input, stated the same way Q-03 states it

Tool descriptions are an established prompt-injection vector. Whatever tooling presents an
item to a rater must render the tool's `name`/`description`/arguments as inert display
text — never executed, never treated as instructions to the labelling tool or to a person
skimming quickly. This is the same posture Q-03's model classifier is required to take,
extended to the human step that precedes it.

## Held-out set construction

Recommended (not yet executed): a **stratified** sample, not a simple random one, so the
labelled set actually spans the interesting cases rather than mostly landing on empty
changesets:

- Across containability class (Class A and Class B both represented, where Class B has any
  observable mutation at all via the protocol-probe oracle).
- Across Q-01's own partition shape: some items with only additions, some with only
  deletions/overwrites, some with both, and — deliberately included, not filtered out —
  some with an empty partition (nothing in `user_state` changed at all, which is itself a
  judgement-relevant case: is "did nothing" additive by default, or does the rater need a
  fourth option? This protocol's answer: an empty partition is always **Additive** by
  definition, not put to a rater at all — there is nothing to judge as destructive when
  nothing was removed or replaced).
- A target size of **at least 100 items** is recommended before treating a computed kappa as
  stable enough to report — small samples produce a kappa with too wide a confidence
  interval to support any claim about labelling-protocol quality.

If any item's source server is under an active embargo (P5-03's `EmbargoState::Embargoed`),
it must not be shown to a rater outside the project until that embargo clears — the
disclosure workflow's own rule applies to a human rater exactly as it applies to any other
form of publication.

## Raters

- **At least two independent raters per item.** Cohen's kappa (below) is defined for
  exactly two raters; this protocol is written around that, not a larger simultaneous panel.
- Raters must not confer, compare notes, or see each other's labels before both have
  submitted independently. This is the entire point of measuring agreement — a number
  computed after raters were allowed to converge measures conformity, not agreement.
- **A disagreement does not get silently resolved into the recorded label.** Where the two
  raters disagree, a third party may adjudicate for the purpose of producing a final,
  reported ground-truth label for Q-04's downstream agreement comparison — but the
  **inter-rater agreement statistic itself is computed from the two independent labels as
  submitted**, before any adjudication. Overwriting a disagreement before scoring it would
  make the reported agreement number describe the adjudication process, not the raters.

## Label record schema

Each label is one record: `item_id`, `rater_id`, `label` (`Destructive` | `Additive` |
`Ambiguous`), `labelled_at`, and an optional free-text `rationale` (never fed back into any
automated pipeline — for human review only, exactly like every other place in this project a
model or a human reads untrusted or free-text content and nothing downstream trusts it
structurally).

## Agreement statistic

Cohen's kappa, `destructive::agreement::cohens_kappa`, over the two primary raters' full
label sequences (same item order, all three categories intact — see above for why
`Ambiguous` is never collapsed away first). Reported alongside the raw agreement counts per
category pair (a bare kappa number with no confusion matrix behind it is exactly the kind of
"summary table that erases how it was computed" this project's aggregate-reporting work
(B-03, P5-04) has already refused to publish anywhere else).

## What this protocol does not decide

Who the raters are, and when this actually gets run, are project-management decisions this
document does not make. What it fixes is: what gets shown, what stays hidden until after
scoring, how many raters, what the categories mean, and exactly which statistic — and how —
scores them, so that whenever real labelling happens, it produces a real, reportable number
rather than an ad hoc one invented after the fact.
