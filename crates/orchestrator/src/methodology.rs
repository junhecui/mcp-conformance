//! P5-05: publish the normalisation ruleset and every limitation from design.md §8
//! alongside the results — the last task in the main roadmap (Phase 5's exit criterion,
//! and design.md §12's open question 3/4 groundwork, all converge here: a result is only
//! as trustworthy as the methodology published next to it).
//!
//! **The five limitations are parsed out of `docs/design.md` §8 itself, not copied into
//! Rust source as a second, hand-maintained list.** design.md's own header states them
//! plainly: "These are design constraints, not deferred work." Duplicating their exact
//! wording into this crate would create exactly the drift risk this codebase otherwise
//! goes out of its way to avoid (the same reasoning `orchestrator::load_ruleset` already
//! applies to `rulesets/v1.json` — read the real file, don't hand-transcribe it). If §8
//! ever gains a sixth limitation or edits an existing one's wording, this module's output
//! changes with it automatically, with no second file to remember to update.
//!
//! The ruleset half reuses [`crate::load_ruleset`] against the real `rulesets/v1.json` —
//! the same parser `normalise`'s own tests exercise, not a second one built solely for
//! publication.

use std::path::Path;

use serde_json::{json, Value};

/// Why building the methodology report failed.
#[derive(Debug)]
pub enum MethodologyError {
    /// Reading `design.md` failed.
    Io(std::io::Error),
    /// Loading the ruleset file failed.
    Ruleset(crate::LoadRulesetError),
    /// `design.md` doesn't contain a `## 8. Known limitations` heading at all — this
    /// module has nothing to parse, which almost certainly means the heading was renamed
    /// or the document was restructured, not that the section is legitimately empty.
    LimitationsSectionNotFound,
}

impl std::fmt::Display for MethodologyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "failed to read design.md: {e}"),
            Self::Ruleset(e) => write!(f, "ruleset load error: {e}"),
            Self::LimitationsSectionNotFound => {
                write!(f, "design.md has no `## 8. Known limitations` heading")
            }
        }
    }
}

impl std::error::Error for MethodologyError {}

impl From<std::io::Error> for MethodologyError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}
impl From<crate::LoadRulesetError> for MethodologyError {
    fn from(e: crate::LoadRulesetError) -> Self {
        Self::Ruleset(e)
    }
}

/// One design constraint from design.md §8, as published: `name` is the bolded lead-in
/// phrase, `text` is the sentence(s) following it, both exactly as written in the source
/// document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limitation {
    /// The bolded lead-in phrase (e.g. `"External state invisibility"`).
    pub name: String,
    /// The explanatory text following it, verbatim.
    pub text: String,
}

/// Parse every `**Title.** body text` paragraph out of the `## 8. Known limitations`
/// section of `design_md` (the section is bounded below by the next `## ` heading, so
/// nothing from `## 9. Key design decisions` — which uses the identical bolded-lead-in
/// style for an unrelated purpose — can leak in).
///
/// # Errors
/// [`MethodologyError::LimitationsSectionNotFound`] if no `## 8. Known limitations` heading
/// exists at all.
pub fn parse_limitations(design_md: &str) -> Result<Vec<Limitation>, MethodologyError> {
    let mut lines = design_md.lines();
    let found_heading = lines.by_ref().any(|line| line.trim() == "## 8. Known limitations");
    if !found_heading {
        return Err(MethodologyError::LimitationsSectionNotFound);
    }

    let mut limitations = Vec::new();
    for line in lines {
        if line.starts_with("## ") {
            break; // the next top-level section — §8 is over.
        }
        if let Some(limitation) = parse_limitation_line(line) {
            limitations.push(limitation);
        }
    }
    Ok(limitations)
}

/// Parse one `**Title.** body text` line, or `None` if `line` isn't shaped like one (blank
/// lines, the section's own lead-in sentence, etc.).
fn parse_limitation_line(line: &str) -> Option<Limitation> {
    let rest = line.strip_prefix("**")?;
    let (name, after_name) = rest.split_once(".**")?;
    let text = after_name.trim_start();
    Some(Limitation { name: name.to_string(), text: text.to_string() })
}

/// Build the published methodology report: the real, parsed ruleset at `ruleset_path`, plus
/// every limitation parsed out of `design_md_path`'s §8 — the exit criterion's own two
/// literal halves, assembled as JSON ready to be written under `results/conformance/`.
///
/// # Errors
/// Whatever reading `design_md_path` or [`crate::load_ruleset`] can fail with, or
/// [`MethodologyError::LimitationsSectionNotFound`].
pub fn build_methodology_report(
    design_md_path: &Path,
    ruleset_path: &Path,
) -> Result<Value, MethodologyError> {
    let design_md = std::fs::read_to_string(design_md_path)?;
    let limitations = parse_limitations(&design_md)?;
    let ruleset = crate::load_ruleset(ruleset_path)?;

    Ok(json!({
        "ruleset": {
            "version": ruleset.version,
            "ephemeral_globs": ruleset.ephemeral_globs,
            "server_internal_globs": ruleset.server_internal_globs,
        },
        "limitations": limitations.iter().map(|l| json!({
            "name": l.name,
            "text": l.text,
        })).collect::<Vec<_>>(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_DOC: &str = "\
## 7. Something Else

Not part of §8.

## 8. Known limitations

These are design constraints, not deferred work. Each must appear in published results.

**External state invisibility.** Tools that mutate a remote service produce no local changeset.

**Caching confound.** A tool may show no second-call effect due to internal caching rather than idempotent design.

---

## 9. Key design decisions

**Overlayfs upper layer over snapshot-and-diff.** This must never be parsed as a §8 limitation.
";

    #[test]
    fn parses_every_limitation_in_the_section() {
        let limitations = parse_limitations(SAMPLE_DOC).expect("parse_limitations");
        assert_eq!(limitations.len(), 2);
        assert_eq!(limitations[0].name, "External state invisibility");
        assert_eq!(
            limitations[0].text,
            "Tools that mutate a remote service produce no local changeset."
        );
        assert_eq!(limitations[1].name, "Caching confound");
    }

    /// The literal thing a naive "grab every bolded-lead-in line" parser would get wrong —
    /// proven by including a `## 9` entry in the same bolded-lead-in style in the sample
    /// document above, and asserting it never appears in the output.
    #[test]
    fn a_bolded_lead_in_after_the_next_heading_is_never_included() {
        let limitations = parse_limitations(SAMPLE_DOC).expect("parse_limitations");
        assert!(
            limitations.iter().all(|l| l.name != "Overlayfs upper layer over snapshot-and-diff"),
            "the §9 entry must not leak into §8's parsed limitations: {limitations:?}"
        );
    }

    #[test]
    fn a_document_with_no_known_limitations_heading_is_an_error() {
        let err = parse_limitations("## 1. Nothing relevant\n\nJust text.\n")
            .expect_err("must fail with no §8 heading");
        assert!(matches!(err, MethodologyError::LimitationsSectionNotFound));
    }

    /// P5-05's own exit criterion, over the real files this project actually publishes
    /// from: every one of the five limitations design.md §8 currently states, plus the
    /// real `rulesets/v1.json` content, end up in the built report exactly as those two
    /// source files say — not a hand-copied approximation of either.
    #[test]
    fn build_methodology_report_over_the_real_design_doc_and_ruleset() {
        let design_md_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/design.md");
        let ruleset_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../rulesets/v1.json");

        let report = build_methodology_report(&design_md_path, &ruleset_path)
            .expect("build_methodology_report");

        assert_eq!(report["ruleset"]["version"], "v1");
        assert!(report["ruleset"]["ephemeral_globs"].as_array().unwrap().contains(&json!("/tmp/**")));

        let limitations = report["limitations"].as_array().expect("limitations array");
        let names: Vec<&str> = limitations.iter().map(|l| l["name"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            vec![
                "External state invisibility",
                "Semantic argument validity",
                "Normalisation sensitivity",
                "Caching confound",
                "Observation evasion",
            ],
            "design.md §8's own five limitations, in the order the document states them"
        );

        let external_state = limitations
            .iter()
            .find(|l| l["name"] == "External state invisibility")
            .expect("external state invisibility entry");
        assert!(
            external_state["text"].as_str().unwrap().contains("unverifiable"),
            "the real design.md text must say the mitigation's verdict is `unverifiable`, not `holds`"
        );
    }
}
