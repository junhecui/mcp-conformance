# ADR-011: Normaliser semantics, glob dialect, and ruleset identity

**Status:** Accepted
**Date:** 2026-10-07
**Author:** Jun Cui
**Closes:** [P1-06](../tasks.md#p1-06-normaliser--ruleset-v1)
**Blocks:** P1-07 (`readOnlyHint` verdict engine), P1-09 (replay test), P2-08 (noise floor),
P2-10 (ruleset v2)
**Related:** ADR-005 (pure, offline derivation), ADR-008 (path taxonomy), ADR-009 (`evtree1`),
architecture.md §4.3 and §6, design.md §8, `docs/HANDOFF.md` §4.5

---

## Context

ADR-008 decided *what the classification rules say* and *where they live*. ADR-009 decided
*what bytes an evidence capture is*. Between the two sits the function architecture.md §3.1
specifies as `(raw_evidence, ruleset_version) → canonical_changeset`, pure and deterministic
— and almost every substantive question about it was still open when P1-06 started:

- `datamodel::{RawEvidence, Ruleset, CanonicalChangeset}` were empty `#[non_exhaustive]`
  placeholders. Nothing said what a "change" *is*.
- ADR-008 lists its allowlists as literal glob strings (`/tmp/**`, `**/.cache/**`, …) and
  never says what `**` means. Two reasonable readings of those eleven strings classify real
  paths differently.
- HANDOFF §4.5(b) left one input question explicitly open: *"Decide whether the base-layer
  capture is a second input; if not, every ambiguity resolves so a tool looks less read-only,
  never more."*
- ADR-009 §"How this fits the data model" assumed `RawEvidence` would be *decoded* entries,
  deserialised outside the pure closure. P1-06 found that assumption costly (see Decision 2)
  and reversed it, which is a change to a prior accepted ADR and therefore has to be written
  down rather than absorbed silently.

Everything below is implemented in `crates/normalise` (plus `crates/store/src/ruleset.rs` for
the loader and `rulesets/v1.yaml` for the data). This ADR records the decisions; the code
and the ruleset data already cite it in 21 places across 9 files, which is why it exists. Each decision is stated
with the alternative that was rejected, because in most cases the alternative is the one a
reader would assume.

---

## Decision

### 1. The base layer is a **required second input**

`RawEvidence` carries two `evtree1` captures: `upper_layer` (the kernel-provided changeset)
and `base_layer` (the overlay's lower directory as mounted). Both are mandatory
(`crates/datamodel/src/lib.rs:193-204`). Every upper-layer entry is classified *relative to
the base*: absent → `Created`, present with a different POSIX type → `Replaced`, present with
the same type → `Modified` with `content_changed` / `metadata_changed` flags
(`crates/normalise/src/lib.rs:233-259`).

**Rejected: upper layer alone.** HANDOFF §4.5(b) offered this, with the compensating rule
that every ambiguity must then resolve so the tool looks *less* read-only. It is cheaper — no
second tree walk, no second blob — and it is sound for `readOnlyHint`, where any non-empty
`user_state` partition already decides the verdict. It fails for everything after Phase 1:

- **It cannot distinguish a structural copy-up from a mutation at all.** overlayfs copies up
  every ancestor directory of a modified file. Without a base to compare against, writing one
  file at `/a/b/c/d` yields four indistinguishable "changes", three of which are mechanical.
  Under the conservative-resolution rule all four are reported, so `readOnlyHint` still works
  — but `D1 Δ D1′` (P2-08's measured noise floor) is then dominated by ancestor chains, and
  ruleset v2 would be derived (P2-10) from an artefact of the missing input rather than from
  observed tool behaviour.
- **It cannot distinguish `Created` from `Modified`.** Track Q's mechanical proxy (Q-01)
  partitions a changeset into `deletions ∪ overwrites` versus `pure additions`. "Overwrite" is
  not expressible without knowing what was there before. Deferring the base input would mean
  re-capturing evidence later, and re-capture means re-executing tools — exactly what ADR-005
  exists to avoid.

**Relationship to ADR-008's conservative-direction rule.** Adding the base input does not
retire that rule; it narrows where it has to be invoked. Where the two captures disagree for
reasons that are *not* the tool's doing — most plausibly a uid/gid view mismatch between the
base capture and the upper capture, under a remapped user namespace — the disagreement
surfaces as `metadata_changed: true`, which both reports the entry and defeats its structural
omission. That is the ADR-008 direction (less read-only, never more), and it is pinned by
`ownership_change_on_a_copied_up_directory_defeats_structural_omission`
(`crates/normalise/src/tests.rs:304-312`). The requirement this imposes on P1-03/P1-04 is
recorded on the field itself: base and upper must be captured the same way, in the same ID
view.

### 2. `RawEvidence` carries **bytes**, and `normalise` decodes them — superseding ADR-009's note

ADR-009 said `RawEvidence` would be "the *parsed* form of one capture — a `Vec` of decoded
entries", with deserialisation sitting outside the pure closure. P1-06 inverts that: the
fields are `Vec<u8>` of raw `evtree1`, and `evtree::decode` runs *inside* `normalise`
(`crates/normalise/src/lib.rs:171-173`).

Two reasons, both load-bearing:

- **Replay becomes literal.** P1-09 must regenerate the whole verdict table from stored
  evidence plus a ruleset version, executing no tool. With bytes as the input type, that is
  `normalise(BlobStore::get(digest), ruleset)` with nothing in between. With decoded entries
  as the input type, some I/O-side component owns a decode step whose behaviour is part of the
  derivation but outside the function ADR-005 made pure — and a bug or a version skew there is
  invisible to the replay test.
- **Totality moves inside the trust boundary.** The evidence blob is derived from a hostile
  tool's writes. If decoding happens upstream, "malformed blob" is an error in an I/O crate
  that must then be mapped to a verdict; if it happens here, it is
  `NormaliseError::MalformedUpperLayer` / `MalformedBaseLayer` — a typed value a verdict
  reports as `unverifiable`, never as `holds`.

Cost: `evtree` had to join the purity allowlist (Decision 10).

### 3. Overlay semantics → `ChangeKind`

Per ADR-009 the codec interprets nothing, so this is where overlay conventions first acquire
meaning (`crates/normalise/src/lib.rs:233-259`, `334-343`):

| Upper-layer entry | `ChangeKind` |
|---|---|
| Character device with `dev_major == 0 && dev_minor == 0` | `Deleted { was }` — `was` is the base node's type, or `None` if the base had nothing there |
| Directory with a `trusted.overlay.opaque` or `user.overlay.opaque` xattr, **any value** | `DirectoryReplaced`, and the base is treated as absent for the whole subtree beneath it |
| Anything else, no base entry | `Created` |
| Anything else, base entry of a different type | `Replaced` |
| Anything else, base entry of the same type | `Modified { content_changed, metadata_changed }` |

Three sub-decisions inside that table:

- **Any opaque value counts**, not just `"y"`. Kernels ≥ 6.7 also write `"x"` (a directory
  holding xwhiteouts). Treating every value as opaque over-reports — it can call a structural
  copy-up a replacement — which is the ADR-008 direction. *Rejected:* matching `"y"` exactly,
  which would silently under-report on a newer kernel than ADR-010 pins.
- **A whiteout is always reported**, including the anomalous case where the base has nothing at
  that path (`Deleted { was: None }`). *Rejected:* dropping it as meaningless; a whiteout is a
  write by construction, and an anomaly in hostile evidence is a finding, not noise.
- **`Modified` with both flags `false` is still reported.** A same-type entry present in the
  upper layer was copied up, which means the kernel saw a write-intent open (or a metadata
  operation whose only trace is an excluded field — see Decision 5). Presence is itself
  evidence of a write. *Rejected:* suppressing it as "no observable difference", which is the
  false-`holds` direction. See Consequences for the cost this carries.

### 4. The structural-omission rule — the only omission there is

`canonical(upper)` reports one `Change` per upper-layer entry, with exactly one exception
(`crates/normalise/src/lib.rs:232-258`, omission at `254-257`): **a directory that exists as a
directory in the base, whose compared metadata is unchanged, and which has at least one
descendant in the upper layer.** Such a directory was copied up only to hold a changed child.

The rule is stated this narrowly so that a single invariant holds: **every omitted entry is
backed by at least one reported descendant.** Nothing disappears from the changeset without
something else in the changeset pointing at why it was there. In tree order, a directory with
any descendant is immediately followed by one (`tree_cmp`, `lib.rs:280-289`, ranks `/` below
every other byte precisely so this is a one-step check), and the deepest element of any such
chain is either a non-directory or a childless directory — so it is reported, and the invariant
terminates. It is checked, not asserted: the property test
`random_trees_satisfy_the_structural_omission_invariant`
(`crates/normalise/src/tests.rs:661-728`) runs 2,000 random base/upper pairs, finds every upper
path missing from the output, and fails unless it is a directory, is a directory in the base,
and has a reported descendant — with a floor (`omitted > 50`) so a generator that stopped
exercising the branch fails rather than passes quietly.

*Rejected: omitting any unchanged entry.* That is the obvious generalisation ("if nothing
differs, say nothing") and it is wrong here for the reason in Decision 3: for a non-directory or
a childless directory there is no mechanical explanation for the copy-up, so silence would
assert something stronger than the evidence supports. *Also rejected: omitting nothing at all*,
which leaves every `readOnlyHint` verdict reading as `violated` for any tool that writes one
file anywhere, and buries P2-08's noise floor in ancestor chains.

**One consequence worth stating because it is not obvious.** The guarantee is
*cross-partition*: the omitted directory and its reported descendant can land in different
ADR-008 classes. A base directory classified `user_state` whose only upper descendant is an
`*.lock` file is omitted, and its descendant is reported under `ephemeral` — so the
`user_state` partition can be empty even though a directory inside it was copied up. That is
the intended reading of ADR-008 (a pattern's presence in an allowlist is a claim that matching
it is *never* diagnostic, and a parent directory's copy-up is a mechanical consequence of
creating the child), but P1-07 and P2-10 should know the interaction exists. It is visible in
`adr_008_classification` (`crates/normalise/src/tests.rs:426-446`), where `/srv` is omitted
while `/srv/app.lock` is reported as ephemeral.

### 5. `mtime` and `inode` are excluded; overlay-private xattrs are stripped

`Node` (`crates/datamodel/src/lib.rs:289-309`) carries file type, mode, uid, gid, device
numbers, non-overlay xattrs, and content. It does **not** carry `mtime` or `inode`, and
`same_metadata` (`crates/normalise/src/lib.rs`) does not compare them. The overlay's own
bookkeeping xattrs are removed from the node and ignored in the comparison, their meaning
having been captured by `ChangeKind` — matched as the **exact leaf names the kernel writes**
(`opaque`, `impure`, `origin`, `uuid`, `redirect`, `nlink`, `upper`, `metacopy`, `protattr`)
under either the `trusted.overlay.` or the `user.overlay.` namespace, and **never by prefix**
(`OVERLAY_NAMESPACES` / `OVERLAY_PRIVATE_NAMES`, `crates/normalise/src/lib.rs`). An
unrecognised name such as `user.overlay.stolen` is an ordinary xattr and is reported like any
other. The first draft of this decision stripped by prefix, which was a containment hole
rather than a tidiness question; the open questions below carry the argument and the
live-mount evidence.

The justification is P2-08, not tidiness. `inode` is kernel-assigned and no two runs agree on
it; `mtime` is the wall clock. Keeping either would put **every** touched path into
`N = D1 Δ D1′`, making the measured noise floor a function of the clock rather than of the
tool, and making `idempotentHint` undecidable for every tool that writes anything. Overlay
bookkeeping xattrs (`origin`, `impure`, …) are in the same category: they record that a
copy-up happened, which `ChangeKind` already says, and their byte values are not stable.
Pinned by `mtime_and_inode_never_affect_the_changeset`,
`overlay_private_xattrs_are_stripped_and_never_a_metadata_change`,
`every_real_overlay_private_name_is_stripped`,
`an_unrecognised_overlay_namespace_xattr_is_reported_not_stripped` and
`an_unknown_overlay_xattr_cannot_launder_a_user_state_directory_out_of_the_changeset`
(`crates/normalise/src/tests.rs`). The set is finite and knowable rather than open-ended: read
as root, a real upper layer produced by the live mount described below carried exactly
`trusted.overlay.uuid`, `trusted.overlay.impure` and `trusted.overlay.opaque`.

**This is not the "normalisation discards evidence" problem ADR-009 forbids.** That
prohibition is about *capture*: ADR-009 §"Losslessness" requires the walker to store
nanosecond mtimes, inode numbers and the full xattr set, precisely so that no derivation
decision is baked in irreversibly. It is still all there, in the blob, addressed by digest.
What happens here is the pure derivation step ADR-005 defines — a function of stored evidence,
re-runnable over that same stored evidence if the decision changes. The line ADR-009 draws is
"interpretation happens in `normalise`, nowhere earlier", and excluding a field *inside*
`normalise` is on the correct side of it. The honest caveat is in the open questions: these
exclusions live in code rather than in ruleset data, so re-deriving under a new *ruleset*
cannot reintroduce them.

### 6. Full content bytes are retained, not a content digest

HANDOFF §4.5(c) asks for "each change as (path, kind, content digest/metadata)". This
deviates: `Node.data` holds the complete file content (or symlink target) verbatim
(`crates/datamodel/src/lib.rs:308`).

The in-code rationale is that hashing would pull a digest algorithm into the `no_std` pure
closure. That is true but thin on its own — a hash could be computed upstream by `observe`, or
hand-rolled. The reasons that actually decide it:

- **A digest computed upstream is not a function of the evidence** as far as `normalise` can
  tell; it would have to be trusted, and ADR-005's whole point is that the derivation depends
  on nothing but `(evidence, ruleset)`.
- **A non-cryptographic hash is attacker-chosen.** The tool under test is assumed hostile
  (design.md §3). Two different files colliding under a weak hash would make `D2 Δ D1` look
  empty — a fabricated `idempotentHint: holds`. So the only acceptable digest is a
  cryptographic one, and that means either a dependency (`sha2` pulls eight packages into
  `normalise`'s closure — measured, see Decision 10) or a hand-written SHA-256 in the one
  crate whose correctness the entire result rests on. Neither is a good trade for what it
  buys.
- **Exact bytes are what P2-08 needs anyway.** `Change` derives `Ord`/`Hash`, so `D1 Δ D1′` is
  an exact set difference with no collision caveat to disclose in the paper.

*Rejected: a content digest in the node.* The cost of the choice is memory, recorded honestly
under Consequences.

### 7. `**` matches zero-or-more segments in **every** position, including trailing

`/tmp/**` matches `/tmp` itself as well as everything under it. `**/.cache/**` matches the
`.cache` directory itself. This is a **deliberate divergence from gitignore and `globset`**,
where a trailing `/**` requires at least one following component. It falls out of the matcher's
construction (`crates/normalise/src/glob.rs:121-127`, `228`) and is asserted at `glob.rs:243`
and `261`.

**ADR-008 does not specify this.** It lists eleven glob strings and says nothing about `**`
semantics, so this ADR is the only place the meaning of its own ruleset is written down.

Why zero-or-more: ADR-008's `server_internal` patterns name *directories* as tool-owned
(`.cache`, `__pycache__`, `node_modules/.cache`). Under one-or-more semantics,
`**/__pycache__/**` would not match `__pycache__` itself, so the directory CPython creates on
first import would classify as `user_state` while every file inside it classified as
`server_internal` — a rule that fires on the contents but not on the container, which is
nearly useless for the thing the rule exists to describe.

**Stated plainly, because it cuts against this project's usual direction:** this is one of the
few places where a choice resolves *away* from ADR-008's conservative rule. One-or-more
semantics would make tools look *less* read-only (the created cache directory would be
`user_state`), and that is normally the direction to prefer. It is rejected here because
ADR-008 sets a specific bar for membership in these lists — "a pattern's presence here is a
claim that matching it is *never* diagnostic of a read-only violation" — and a claim that
`.cache/**` is never diagnostic entails the same claim about `.cache`. Honouring the bar and
then exempting the directory the bar was written about would be incoherent. P2-10 can revisit
it against measured noise, as it can revisit any ruleset decision; what it must not do is
change the dialect while leaving the eleven strings alone.

**What changing it would break.** Switching to one-or-more would: reclassify the creation of
every conventionally-named cache directory from `server_internal` to `user_state`, producing
`readOnlyHint: violated` on the first import of any Python tool with a bundled package; change
the classification of `/tmp` and `/run` themselves; invalidate the digest pinned in
`PUBLISHED` if the strings were rewritten to compensate (`/tmp/**` → `/tmp` plus `/tmp/**`),
hence a new ruleset version; and silently change historic verdicts on replay, since the
dialect is code, not ruleset data. That last point is the strongest argument for writing the
dialect down here rather than leaving it implicit.

The rest of the dialect is deliberately smaller than any standard one: `*` and `?` within a
component, everything else literal, and character classes, braces, escapes, `**` inside a
larger component, `.`/`..` components, empty components and unanchored patterns are all
**parse errors** rather than literals (`glob.rs:100-143`). A ruleset written for a richer
dialect fails loudly at load time instead of matching nothing. Matching is non-backtracking
(single-resume-point wildcard, `glob.rs:197-229`), `O(L · P)`, allocation-free, with
`no_exponential_blowup_on_hostile_inputs` (`glob.rs:341-354`) as the guard.

### 8. Classification order, and forged path components

`ephemeral` is matched first, then `server_internal`, then the `user_state` default — so a lock
file inside a cache directory is `ephemeral` (`crates/normalise/src/lib.rs:126-144`). Before
any matching, a path containing an empty, `.` or `..` component is classified `user_state`
unconditionally (`lib.rs:134`). No real capture produces such a path; the purpose is that
forged evidence cannot launder `/tmp/../home/secret` into `ephemeral`.

The guard is enforced **in the matcher as well**, which is where it belongs: `Glob::matches_components`
(`crates/normalise/src/glob.rs`) refuses any component that is empty, `.`, `..`, or that itself
contains a `/`, so `Glob::matches_path("/tmp/**", "/tmp/../home/secret")` is `false` rather
than `true`, and a component-with-slashes cannot make `/a/*` match `b/c/d`. It was originally
only at the classifier's own entry point, which left this decision advertising a property of
classification generally that one of the crate's two public matching entry points did not
have. No caller was affected — nothing outside `glob`'s own tests called `matches_path` — but a
public API on the crate whose output *is* the verdict should not disagree with itself about a
security property, and no test pinned the difference. Pinned now by
`forged_path_components_never_match` (`glob.rs`) and
`classify_and_the_matcher_agree_on_forged_components` (`tests.rs`).

*Rejected: normalising the path first.* Resolving `..` would be a second, subtly different
path-canonicalisation implementation living in the one crate whose output is the verdict;
refusing to classify such a path at all is smaller and fails in the safe direction. Pinned by
`forged_dot_dot_and_empty_components_cannot_launder_into_an_allowlist`
(`crates/normalise/src/tests.rs:455-461`).

### 9. Ruleset identity is tamper-evident; the loader is a hand-written strict-YAML subset

`Ruleset::identity()` is `"<version>+sha256:<hex>"` (`crates/datamodel/src/lib.rs:239`),
computed from `source_digest` — the SHA-256 of the exact ruleset file bytes, computed by the
**loader**, never supplied by the file. `store::ruleset::PUBLISHED`
(`crates/store/src/ruleset.rs:51-52`) pins
`("v1", "48e55850021a462d5710d72e06b5bebe256b1a8107d5a70b0202a1f0b63c1128")`, which is the
current SHA-256 of `rulesets/v1.yaml`; `load()` refuses a file claiming a published label whose
bytes hash differently, and `load_draft()` refuses any file claiming a published label. The
identity is carried onto every `CanonicalChangeset` (`ruleset_identity`,
`crates/datamodel/src/lib.rs:359`), so a derived changeset always names the exact rule text it
was derived under, not just a label someone could reuse.
`any_edit_to_a_published_ruleset_is_refused` (`crates/store/src/ruleset.rs:295-307`) proves it
for a comment-only edit as well as a rule edit.

*Rejected: a bare version label.* `"v1"` is a name, and ADR-005's reproducibility claim — hand
a reviewer the evidence and the ruleset and they reproduce every verdict — is only as strong as
the binding between the two. A label can be reused over edited bytes; a digest cannot.

**Parsing is hand-written** (`crates/store/src/ruleset.rs:179-259`): exactly three top-level
keys each appearing once, every scalar double-quoted with no escapes or inner quotes, list
items indented by exactly two spaces, `#` comments on their own lines only, no carriage returns
or tabs, and every pattern must compile under `normalise::glob` at load time. Anything else is
a `Syntax` error naming the line.

*Rejected: a YAML library.* Three reasons, in order of weight. (1) The file's shape is fixed
and tiny; a real parser buys flexibility nobody wants, and flexibility in a file whose bytes
are digest-pinned is a liability — anchors, aliases, flow style, implicit typing and
multi-document streams are all ways for two byte-different files to mean the same thing, or for
one file to mean something unintended. (2) The subset *rejects* rather than half-understands: a
ruleset written against a richer YAML dialect fails at load instead of being partially applied.
(3) YAML's own attack surface (alias expansion, type coercion) is unnecessary here. The honest
counterpoint: this file is operator-authored and therefore trusted, so (3) is defence in depth
rather than a live threat.

The no-carriage-returns rule is not cosmetic. A CRLF checkout changes every line of
`rulesets/v1.yaml`, which changes its digest, which makes the pinned `v1` unloadable. The
parser refuses `\r` loudly rather than mis-parsing — `crlf_checkout_is_refused_loudly`
(`crates/store/src/ruleset.rs:309-313`) — and `.gitattributes` marks the path `-text` so the
situation does not arise in the first place.

### 10. `PURE_ALLOWLIST` widened from `["datamodel"]` to `["datamodel", "evtree"]`

Decision 2 put `evtree::decode` inside `normalise`, so `evtree` is in a pure crate's dependency
closure and `cargo purity` (F-04) had to be told about it (`xtask/src/purity.rs:35-42`).

**This does not weaken F-04**, and the reason is structural rather than a matter of discipline:

- **The check is a transitive-closure subset test, not a per-crate exemption**
  (`xtask/src/purity.rs:68-81`, documented at `purity.rs:10-14`). `cargo tree` flattens the
  closure, so a dependency acquired by `evtree` is indistinguishable from one acquired by
  `normalise` directly and is reported against `normalise`. Allowlisting `evtree` permits
  *that one package*, not its future dependencies. `transitive_offenders_are_caught`
  (`xtask/tests/purity.rs:59-73`) is the test of that property.
- **Verified, not assumed.** Adding `sha2 = "0.11.0"` to `crates/evtree/Cargo.toml` and running
  `cargo purity` fails with exit 1 and eight violations: `cfg-if`, `const-oid`, `cpufeatures`,
  `crypto-common`, `digest`, `hybrid-array`, `sha2` and `typenum`, each reported as
  "`normalise` depends on X, which is not in the pure allowlist (ADR-005)" — against
  `normalise`, not against `evtree`. Demonstrated and reverted (checksums re-verified), the
  same way F-04's own entry demonstrated the `verdict` → `store` edge.
- **`evtree` was held to the same build-level guarantee as `normalise` itself**, which is the
  trap HANDOFF §4.5(a) named: *"a `no_std` crate linking a `std` dependency quietly defeats
  F-04 Layer 3."* It is now `#![no_std]` + `extern crate alloc`, with `extern crate std` gated
  to `#[cfg(test)]`, and an empty `[dependencies]` table (`crates/evtree/Cargo.toml`,
  `crates/evtree/src/lib.rs:44-56`). The `cfg(test)`-gated `std` is consistent with the check's
  existing policy of excluding dev-dependencies (`purity.rs:85-86`): test code does not ship in
  the derived artifact.

*Rejected: decoding outside `normalise` to avoid widening the allowlist.* That is Decision 2's
rejected option, re-arrived at from the other direction; the allowlist entry is the price of
the replay property, and it is cheaper than the alternative.

One gap, disclosed: `evtree` has no per-crate `clippy.toml` banning clock/path/file/socket
types, as `normalise` and `verdict` do (F-04 Layer 2). Under `#![no_std]` those types are not
nameable outside `cfg(test)`, so Layer 3 covers it — but if `evtree` ever relaxes to `std`,
Layer 2 must be added to it in the same change.

### 11. `#[non_exhaustive]` removed from `RawEvidence`, `Ruleset`, `CanonicalChangeset`

All three now have fields, and all three are constructed by struct literal from crates other
than `datamodel` — `normalise`'s tests build `RawEvidence` and `Ruleset`, `normalise` itself
builds `CanonicalChangeset`, `store::ruleset` builds `Ruleset`. `#[non_exhaustive]` forbids
exactly that from outside the defining crate, so keeping it would mean a constructor or builder
per type, for no benefit: `publish = false` across the workspace (`Cargo.toml:9`) means there
are no external consumers to protect from a breaking change, and adding a field later is a
compile error at a list of call sites the compiler enumerates — which is the desired behaviour,
not a hazard.

**Intended to be permanent for as long as the workspace stays unpublished.** If any of these
crates is ever published, `#[non_exhaustive]` should come back on the two types that cross an
API boundary as *inputs* (`RawEvidence`, `Ruleset`); `CanonicalChangeset` is produced by this
workspace and consumed by it, and gains nothing from it either way.

---

## Options considered

The table covers the three decisions a reader is most likely to have expected to go the other
way. The rest are argued inline above.

| # | Question | Chosen | Rejected alternative | Why |
|---|---|---|---|---|
| 1 | Inputs to `normalise` | Upper **and** base capture | Upper layer alone, with conservative resolution (HANDOFF §4.5(b)) | Upper-only cannot separate structural copy-ups from mutations, which poisons P2-08's noise floor, and cannot express "overwrite" for Q-01. `readOnlyHint` alone would have been fine. |
| 2 | `RawEvidence` shape | Raw `evtree1` bytes, decoded inside `normalise` | Decoded entries, deserialised upstream (ADR-009's own assumption) | Makes P1-09 replay literally `normalise(stored bytes, ruleset)`, and moves totality over hostile bytes inside the pure boundary. Costs one allowlist entry. |
| 3 | Node content | Full bytes | Cryptographic content digest (HANDOFF §4.5(c)) | A weak hash is attacker-collidable into a false `holds`; a strong one means a dependency in the pure closure or a hand-rolled primitive. Exact bytes are what P2-08 compares anyway. Costs memory. |

---

## Consequences

**Good.**

- `(evidence, ruleset) → changeset` is now a total, pure, deterministic function whose only
  inputs are two byte strings and a parsed ruleset — the literal shape ADR-005 requires, and
  the shape P1-09's replay test can exercise without a sandbox existing.
- The noise floor P2-08 measures is a property of the tool, not of the clock: with `mtime` and
  `inode` excluded and ancestor copy-ups omitted, two identical runs of a well-behaved tool
  produce byte-equal changesets, so `N` starts empty and every element that appears in it is a
  real candidate normalisation rule (ADR-003's discipline, as P2-10 needs it).
- Every ambiguity that remains resolves toward *less* read-only: unchanged-but-copied-up
  entries are reported, any opaque xattr value counts, anomalous whiteouts are reported, a
  base/upper view mismatch surfaces as a metadata change, and a forged path component defeats
  allowlist matching. ADR-008's direction is preserved in all of them.
- Ruleset identity cannot be forged or silently drifted: a published ruleset is immutable by
  construction, and a changeset names the exact bytes it was derived under.
- Hostile input is bounded and total, tested rather than argued: every truncation offset of a
  valid capture is rejected (`every_truncation_of_valid_evidence_is_rejected_cleanly`), 6,000
  bit-flipped and random byte strings never panic (`random_bytes_and_bit_flips_never_panic`),
  and a 1,000-deep chain plus 20,000 siblings complete quickly
  (`large_and_deep_trees_are_handled`) — `crates/normalise/src/tests.rs:552-568`, `585-616`,
  `732-752`.

**Costs, accepted.**

- **Memory is `O(total evidence bytes)`, with a multiplier closer to 20× than to 2×.**
  `evtree::decode` materialises the whole capture, including every file's full content, and
  `to_node` clones that content into the `Node`. "Roughly twice over" — this ADR's first
  estimate — holds only for the one-big-file case. Measured by the P1-06 review pass (release
  build, peak RSS ÷ input bytes): **3.0×** for a single 16 MB file, **7.3×** for 100k small
  entries, **4.0×** for a 10,000-deep path chain, and **21×** for a 1 MB path consisting only
  of `/` separators. The last is the real bound, and it is not the file contents at all:
  `classify` builds a per-entry `Vec<&[u8]>` of 16-byte fat pointers, one per component
  (`crates/normalise/src/lib.rs:129`), so a component-dense path costs ~16 bytes of resident
  memory per path byte. It remains bounded and **linear in the evidence size with no
  superlinear term** — a hostile capture can inflate the constant, not the exponent. Streaming
  or digesting would fix the constant; both were rejected above. **Consequence: the evidence
  size cap deferred to P1-04/P5-02 must budget ~20× of input bytes, not 2×** (and must yield
  an `unverifiable` reason code, never a silent truncation). Not a Phase 1 problem; it is a
  Phase 5 scale problem.
- **A copy-up with no observable difference reads as a mutation.** overlayfs copies a file up on
  a write-intent open even if nothing is written, and a metadata operation whose only trace is
  `mtime` leaves `content_changed: false, metadata_changed: false`. Decision 3 reports those,
  so `readOnlyHint: violated` is reachable for a tool that opened a file for writing and wrote
  nothing. This is the single most likely source of a `violated` verdict a maintainer would
  dispute, and it is deliberate. P2-08 will quantify how often it happens; if it is common,
  P2-10 has the evidence to act on it — and the disclosure workflow (P5-03) should expect this
  specific objection.
- **The noise exclusions are code, not ruleset data.** ADR-008 made the taxonomy data precisely
  so corrections do not need a recompile. The `mtime`/`inode`/overlay-xattr exclusions, the
  glob dialect, and the structural-omission rule are all in `normalise`'s source instead.
  Consequence: a verdict is reproducible from `(evidence, ruleset_identity)` **plus the
  `normalise` version**, and nothing in the schema records the latter — `VERDICT` has
  `ruleset_version` and `protocol_version`, and only `RUN` has `harness_version`. See the open
  questions.
- **The glob dialect is non-standard.** Anyone reading `rulesets/v1.yaml` with gitignore
  intuitions will mis-predict `**`. Mitigated by stating it in the file's own header comment, in
  `normalise::glob`'s module doc, and here — but it is a real footgun for a contributor writing
  ruleset v2.
- **Two captures per run, not one.** The base layer must be walked and stored as well as the
  upper layer. The storage cost is smaller than it looks: the base is byte-reproducible by
  construction (P1-02) and the evidence store is content-addressed (F-05), so every arm and
  every tool sharing a base layer shares one blob. The walk itself is not amortised that way and
  is a real P1-04 cost on a large rootfs.

---

## Open questions and follow-on decisions this ADR does not make

- **Does `VERDICT` need to record the normaliser's code version?** The cost above says a verdict
  depends on it. ADR-005's promise ("hand a reviewer the evidence and the ruleset") is currently
  true only if the harness version is also fixed. P1-07 and P5-02 should decide whether
  `VERDICT` gains a `normaliser_version` (or reuses `harness_version`) before any verdict is
  published; it is cheap now, in the F-06 spirit of adding `embargo_state` early.
- **Should `VERDICT.ruleset_version` hold the label or the full identity?**
  `VerdictRecord::ruleset_version` is an `Option<&str>` FK to `RULESET.ruleset_version`, and the
  tamper-evident value is `identity()` (`"v1+sha256:…"`), not `"v1"`. Recommend storing the
  identity, as the `CanonicalChangeset` already does — but it is a P1-07 decision, not one to
  make here without the insertion path in front of it.
- **Should the noise exclusions become ruleset data?** P2-10 is where this would be decided,
  with measured `N` in hand. The evidence retains `mtime`, `inode` and overlay xattrs (ADR-009's
  losslessness), so moving them into the ruleset is a pure code change re-derivable over all
  historical evidence — not a re-capture.
- **`user.overlay.*` is attacker-writable *today*, in trusted mode — the risk was never
  contingent on `userxattr`.** This entry previously read as though the hazard depended on
  P1-03 choosing a rootless mount. It does not. The first draft of Decision 5 stripped overlay
  xattrs by *prefix*, justified as removing kernel bookkeeping, which silently assumed every
  name under `trusted.overlay.` and `user.overlay.` is kernel-written. `user.*` is not: POSIX
  gives that namespace to the file's owner, and the tool under test owns everything it creates
  in the upper layer. Verified on a live overlay — kernel 6.6, ADR-010's pinned
  `redirect_dir=off,metacopy=off,index=off`, **trusted mode, no `userxattr`**, unprivileged
  process writing through the merged mount — where setting `user.overlay.stolen` and
  `user.overlay.opaque` both **succeeded** and the bytes reached the upper layer verbatim.
  Under prefix stripping that handed a hostile tool a write `normalise` deleted from the `Node`
  *and* ignored in `same_metadata`, leaving a copied-up user-state directory with
  `metadata_changed == false` and therefore eligible for Decision 4's structural-omission rule.
  Demonstrated end to end: a tool declaring `readOnlyHint: true` stashes 448 bytes on a
  user-state directory and creates one `scratch.lock`, giving `user_state = []`,
  `server_internal = []`, `ephemeral = [/home/u/scratch.lock]` — `readOnlyHint` reads as
  `holds` with the payload in **no partition at all**, which is worse than the name-laundering
  gap below, where the change is at least still reported somewhere a reviewer can see it. The
  same mechanism is a covert equality channel for `idempotentHint`: two runs differing only in
  a `user.overlay.*` value normalise to equal changesets. **Fixed** by Decision 5's exact-name
  set; what remains below is P1-03's to decide, not this ADR's.

  The two directions are asymmetric, and the safe one invites complacency. *Adding* a marker is
  conservative: `classify_change` returns `DirectoryReplaced` before any omission branch and
  hidden descendants become `Created`, both always reported, so a tool that writes
  `user.overlay.opaque` onto its own directory only makes itself look *less* read-only.
  *Removing* a real marker is the attack. A useful negative result from the same live mount:
  after `rm -rf Documents && mkdir Documents` the kernel writes `trusted.overlay.opaque=y`, and
  an unprivileged tool **cannot** remove it — `EPERM` both through the merged mount and
  directly on the upperdir. So in trusted mode the removal attack is closed by the kernel, and
  it stays live only under `userxattr`, where the marker sits in the tool's own namespace.
  **Consequences for P1-03:** prefer the privileged (trusted-xattr) mount; and in either case
  **assert that the upperdir path is not reachable from inside the sandbox**, since the EPERM
  above is the kernel refusing a `trusted.*` write, not a guarantee that an upper-layer file is
  out of the tool's reach.
- **Proposed amendment to ADR-008: classify on `(path, ChangeKind)`, not on path alone.**
  ADR-008's name-based allowlists launder **destructive** changes, not only additive ones. A
  tool that *deletes* a base-layer file whose name matches an allowlist yields
  `user_state = []`, `ephemeral = [/srv/app.lock]` → `readOnlyHint: holds`. ADR-008's
  membership bar — "a pattern's presence here is a claim that matching it is *never* diagnostic
  of a read-only violation" — was argued with the example of a tool *touching* a lock file at
  startup; that the creation of a lock file is non-diagnostic does not entail that its
  *deletion* is. The hardening: let an allowlist suppress `Created` and `Modified` only, and
  send `Deleted`, `Replaced` and `DirectoryReplaced` always to `user_state`. It needs no change
  to the eleven glob strings and no new ruleset version. **Deliberately not implemented in
  P1-06**: it is a semantic change to ADR-008's taxonomy and deserves its own task and its own
  review pass rather than being slipped into the normaliser. Also recorded in `docs/tasks.md`
  under P1-06's carry-forward findings.
- **A fixture-authoring constraint this implies for P2-05.** Until that amendment lands, a
  generic or bespoke fixture's *user-state* files must never be named `*.lock`, `*.pid` or
  `*.sock`, nor nested under `.cache`, `.config`, `.local/state`, `__pycache__` or
  `node_modules/.cache` — otherwise a tool can delete real fixture state and still be read as
  `holds`.
- **A category correction for ADR-008 and P5-05.** ADR-008 files name-based laundering under
  design.md §8's "observation evasion", which §8 declares out of scope. That is a category
  error. §8's evasion requires the tool to *detect* the sandbox and change its behaviour,
  whereas choosing a filename suffix is always-on, costs the tool nothing, needs no detection,
  and works on first contact against a harness behaving exactly as designed. It belongs in
  P5-05's published limitations as its own named limitation, not folded into evasion.
- **`world` provisioner overrides (ADR-008 §"`world` provisioner interaction").** Still
  unspecified, still a P2-05 concern. Nothing here forecloses it: classification is a single
  function of `(ruleset, path)` and an override would enter at the same point.
- **P1-07** consumes `CanonicalChangeset` and decides `readOnlyHint` against the `user_state`
  partition. Two constraints from this ADR carry into it: a `NormaliseError` is never `holds`
  (it is `unverifiable` with a reason code), and an empty changeset from a *failed* invocation
  is also never `holds` — the false-`holds` gap commit 832d990 closed in Track B.
