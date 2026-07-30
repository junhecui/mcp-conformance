//! Coverage aggregation (P0-05): per annotation, per tool, per server, corpus-wide —
//! whether each of the four MCP annotations was explicitly declared, silently defaulted,
//! or never addressed at all.
//!
//! **Must not:** touch behavioural evidence. This module only ever reads the
//! `annotations` object out of a `tools/list` response; it never runs a tool, inspects a
//! changeset, or reasons about whether a declared annotation is *true*. That is the
//! conformance pipeline's job (Phase 1+), not this one's — census answers "did the server
//! say anything," never "was the server right."

use serde::Deserialize;

/// Whether one annotation was declared, defaulted, or the tool never engaged with
/// annotations at all.
///
/// `Defaulted` and `Absent` both end up at the spec's default value from a client's point
/// of view, but they are different findings here: `Defaulted` means the server engaged
/// with annotations at all (the `annotations` object exists) and simply didn't set this
/// one; `Absent` means the tool has no `annotations` object whatsoever. Collapsing them
/// would erase exactly the distinction design.md's open question 1 asks about — whether
/// the headline finding is about *mismatch* or about *absence*.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Coverage {
    /// The key is present in the tool's `annotations` object.
    Explicit,
    /// `annotations` exists on this tool, but this particular key is not in it.
    Defaulted,
    /// This tool has no `annotations` object at all.
    Absent,
}

/// One tool's coverage across all four MCP annotations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCoverage {
    /// The tool's name, for reporting.
    pub tool_name: String,
    /// Coverage of `readOnlyHint`.
    pub read_only_hint: Coverage,
    /// Coverage of `destructiveHint`.
    pub destructive_hint: Coverage,
    /// Coverage of `idempotentHint`.
    pub idempotent_hint: Coverage,
    /// Coverage of `openWorldHint`.
    pub open_world_hint: Coverage,
}

/// Why coverage extraction failed — always a shape problem with an already-successfully
/// -discovered `tools/list` response.
#[derive(Debug)]
pub enum CoverageError {
    /// The captured bytes are not valid UTF-8.
    NotUtf8(std::str::Utf8Error),
    /// Valid UTF-8, but not the expected JSON-RPC / `tools/list` shape.
    Malformed(String),
}

impl std::fmt::Display for CoverageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotUtf8(e) => write!(f, "tools/list response is not valid UTF-8: {e}"),
            Self::Malformed(msg) => write!(f, "tools/list response is malformed: {msg}"),
        }
    }
}

impl std::error::Error for CoverageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NotUtf8(e) => Some(e),
            Self::Malformed(_) => None,
        }
    }
}

#[derive(Deserialize)]
struct Envelope {
    result: ToolsResult,
}

#[derive(Deserialize)]
struct ToolsResult {
    tools: Vec<ToolEntry>,
}

#[derive(Deserialize)]
struct ToolEntry {
    name: String,
    #[serde(default)]
    annotations: Option<Annotations>,
}

#[derive(Deserialize, Default)]
struct Annotations {
    #[serde(rename = "readOnlyHint")]
    read_only_hint: Option<bool>,
    #[serde(rename = "destructiveHint")]
    destructive_hint: Option<bool>,
    #[serde(rename = "idempotentHint")]
    idempotent_hint: Option<bool>,
    #[serde(rename = "openWorldHint")]
    open_world_hint: Option<bool>,
}

fn coverage_of(annotations: Option<&Annotations>, field: impl Fn(&Annotations) -> Option<bool>) -> Coverage {
    annotations.map_or(Coverage::Absent, |a| {
        if field(a).is_some() { Coverage::Explicit } else { Coverage::Defaulted }
    })
}

/// Extract per-tool, per-annotation coverage from a raw `tools/list` response — the same
/// bytes shape [`discovery::pin::pin_tools`] consumes, from
/// [`discovery::Discovery::tools_list_raw`].
pub fn tool_coverage(tools_list_raw: &[u8]) -> Result<Vec<ToolCoverage>, CoverageError> {
    let text = std::str::from_utf8(tools_list_raw).map_err(CoverageError::NotUtf8)?;
    let envelope: Envelope = serde_json::from_str(text)
        .map_err(|e| CoverageError::Malformed(format!("not a JSON-RPC tools/list response: {e}")))?;

    Ok(envelope
        .result
        .tools
        .into_iter()
        .map(|tool| ToolCoverage {
            read_only_hint: coverage_of(tool.annotations.as_ref(), |a| a.read_only_hint),
            destructive_hint: coverage_of(tool.annotations.as_ref(), |a| a.destructive_hint),
            idempotent_hint: coverage_of(tool.annotations.as_ref(), |a| a.idempotent_hint),
            open_world_hint: coverage_of(tool.annotations.as_ref(), |a| a.open_world_hint),
            tool_name: tool.name,
        })
        .collect())
}

/// A count of how many tools fell into each [`Coverage`] bucket for one annotation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tally {
    /// Tools that declared this annotation explicitly.
    pub explicit: usize,
    /// Tools whose `annotations` object exists but omits this key.
    pub defaulted: usize,
    /// Tools with no `annotations` object at all.
    pub absent: usize,
}

impl Tally {
    fn record(&mut self, coverage: Coverage) {
        match coverage {
            Coverage::Explicit => self.explicit += 1,
            Coverage::Defaulted => self.defaulted += 1,
            Coverage::Absent => self.absent += 1,
        }
    }
}

/// A [`Tally`] per annotation, over whatever set of tools was rolled up.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AnnotationTally {
    /// Tally for `readOnlyHint`.
    pub read_only_hint: Tally,
    /// Tally for `destructiveHint`.
    pub destructive_hint: Tally,
    /// Tally for `idempotentHint`.
    pub idempotent_hint: Tally,
    /// Tally for `openWorldHint`.
    pub open_world_hint: Tally,
}

/// Roll up coverage across any set of tools.
///
/// Pass one server's tools for a per-server rollup, or every server's tools concatenated
/// for a corpus-wide one — the same function serves both scopes in the task list, because a
/// tally has no notion of a server boundary; only the caller's choice of input slice does.
#[must_use]
pub fn tally(tools: &[ToolCoverage]) -> AnnotationTally {
    let mut result = AnnotationTally::default();
    for tool in tools {
        result.read_only_hint.record(tool.read_only_hint);
        result.destructive_hint.record(tool.destructive_hint);
        result.idempotent_hint.record(tool.idempotent_hint);
        result.open_world_hint.record(tool.open_world_hint);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tools_list(tools_json: &str) -> Vec<u8> {
        format!(r#"{{"jsonrpc":"2.0","id":1,"result":{{"tools":[{tools_json}]}}}}"#).into_bytes()
    }

    #[test]
    fn fully_explicit_annotations_are_all_explicit() {
        let raw = tools_list(
            r#"{"name":"t","inputSchema":{},"annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false}}"#,
        );
        let coverage = &tool_coverage(&raw).expect("parse")[0];
        assert_eq!(coverage.read_only_hint, Coverage::Explicit);
        assert_eq!(coverage.destructive_hint, Coverage::Explicit);
        assert_eq!(coverage.idempotent_hint, Coverage::Explicit);
        assert_eq!(coverage.open_world_hint, Coverage::Explicit);
    }

    #[test]
    fn a_present_annotations_object_with_missing_keys_is_defaulted_not_absent() {
        let raw = tools_list(
            r#"{"name":"t","inputSchema":{},"annotations":{"readOnlyHint":true}}"#,
        );
        let coverage = &tool_coverage(&raw).expect("parse")[0];
        assert_eq!(coverage.read_only_hint, Coverage::Explicit);
        assert_eq!(coverage.destructive_hint, Coverage::Defaulted);
        assert_eq!(coverage.idempotent_hint, Coverage::Defaulted);
        assert_eq!(coverage.open_world_hint, Coverage::Defaulted);
    }

    #[test]
    fn no_annotations_object_at_all_is_absent_for_every_annotation() {
        let raw = tools_list(r#"{"name":"t","inputSchema":{}}"#);
        let coverage = &tool_coverage(&raw).expect("parse")[0];
        assert_eq!(coverage.read_only_hint, Coverage::Absent);
        assert_eq!(coverage.destructive_hint, Coverage::Absent);
        assert_eq!(coverage.idempotent_hint, Coverage::Absent);
        assert_eq!(coverage.open_world_hint, Coverage::Absent);
    }

    #[test]
    fn defaulted_and_absent_are_distinct_buckets_in_a_tally() {
        let raw = tools_list(
            r#"{"name":"engaged","inputSchema":{},"annotations":{"readOnlyHint":true}},
               {"name":"unengaged","inputSchema":{}}"#,
        );
        let tools = tool_coverage(&raw).expect("parse");
        let t = tally(&tools);
        // "engaged" defaults destructiveHint (object present, key missing);
        // "unengaged" has no annotations object at all.
        assert_eq!(t.destructive_hint.defaulted, 1);
        assert_eq!(t.destructive_hint.absent, 1);
        assert_eq!(t.destructive_hint.explicit, 0);
        assert_eq!(t.read_only_hint.explicit, 1);
    }

    /// The same `tally` function serves per-server and corpus-wide rollups; this proves the
    /// corpus-wide case is exactly "concatenate every server's tools first."
    #[test]
    fn tally_composes_across_servers_for_a_corpus_wide_rollup() {
        let server_a = tools_list(r#"{"name":"a","inputSchema":{},"annotations":{"readOnlyHint":true}}"#);
        let server_b = tools_list(r#"{"name":"b","inputSchema":{}}"#);

        let tools_a = tool_coverage(&server_a).expect("parse a");
        let tools_b = tool_coverage(&server_b).expect("parse b");

        let per_server_a = tally(&tools_a);
        let per_server_b = tally(&tools_b);
        assert_eq!(per_server_a.read_only_hint.explicit, 1);
        assert_eq!(per_server_b.read_only_hint.absent, 1);

        let mut corpus_wide_tools = tools_a;
        corpus_wide_tools.extend(tools_b);
        let corpus_wide = tally(&corpus_wide_tools);
        assert_eq!(corpus_wide.read_only_hint.explicit, 1);
        assert_eq!(corpus_wide.read_only_hint.absent, 1);
    }

    #[test]
    fn malformed_response_is_rejected() {
        let err = tool_coverage(b"not json").expect_err("must reject");
        assert!(matches!(err, CoverageError::Malformed(_)));
    }
}
