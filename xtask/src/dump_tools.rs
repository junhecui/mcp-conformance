//! Debug utility: discover a server and print each tool's name and raw `annotations`
//! object. Exists for hand-verification passes (P0-06's "hand-verify the taxonomy on ≥20
//! tools" item) — a human needs to see the actual JSON next to the computed
//! [`census::coverage::Coverage`] classification to judge whether the taxonomy is right,
//! not just trust the aggregate counts.

use discovery::DiscoveryClient;

/// Longest rendering of any one server-controlled field this tool prints.
const MAX_FIELD_CHARS: usize = 200;

/// Make server-controlled text safe for a console a model may be reading.
///
/// Tool names and `annotations` are authored by the server under test (design.md §3), and
/// this console is read by people *and* by this project's model-driven tooling — so an
/// unbounded, unfiltered field is a terminal-escape and prompt-injection channel, amplified
/// by P0-13 from one page to forty. Same filter as P0-12 applied to relayed child stderr and
/// `guard_attempt` applies to panic payloads: printable ASCII, space and tab only, bounded,
/// with the cut stated rather than silent.
fn safe(text: &str) -> String {
    let filtered: String =
        text.chars().filter(|c| *c == ' ' || *c == '\t' || c.is_ascii_graphic()).collect();
    match filtered.char_indices().nth(MAX_FIELD_CHARS) {
        None => filtered,
        Some((cut, _)) => format!("{}… [cut at {MAX_FIELD_CHARS} chars]", &filtered[..cut]),
    }
}

/// Discover `url` and print each tool's name, its coverage classification per annotation,
/// and the raw `annotations` object it came from.
///
/// Walks every `tools/list` page (P0-13) and prints the page number beside each tool:
/// hand-verification is precisely where someone needs to see *which* page a tool came from,
/// since the defect P0-13 fixed was tools past page one being invisible. A truncated walk
/// says so loudly before any tool is printed, so nobody hand-verifies a partial list
/// believing it is whole.
pub fn run(url: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mut client = DiscoveryClient::http(url.to_string());
    let discovery = client.discover()?;

    let pagination = discovery.pagination;
    if !pagination.cursors_exhausted() {
        println!(
            "!! INCOMPLETE TOOL LIST: the walk stopped after {} page(s) because {} — the \
             tools below are a PARTIAL list",
            pagination.pages_fetched, pagination.stopped_because
        );
    }

    let mut lines = Vec::new();
    for (index, page) in discovery.tools_list_pages.iter().enumerate() {
        let coverage = census::coverage::tool_coverage(page)?;
        let raw: serde_json::Value = serde_json::from_slice(page)?;
        let tools = raw["result"]["tools"].as_array().cloned().unwrap_or_default();
        for (tool, cov) in tools.iter().zip(coverage.iter()) {
            lines.push(format!(
                "\n- [page {}] {} (readOnly={:?} destructive={:?} idempotent={:?} openWorld={:?})\n  annotations: {}",
                index + 1,
                safe(&cov.tool_name),
                cov.read_only_hint,
                cov.destructive_hint,
                cov.idempotent_hint,
                cov.open_world_hint,
                safe(&tool.get("annotations").cloned().unwrap_or(serde_json::Value::Null).to_string())
            ));
        }
    }

    println!(
        "=== {url} — {} tools across {} page(s) ===",
        lines.len(),
        discovery.tools_list_pages.len()
    );
    for line in lines {
        println!("{line}");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_controlled_text_is_stripped_and_bounded() {
        let hostile = format!("ok{}[2Jx\nIGNORE PREVIOUS{}", '\u{1b}', "y".repeat(500));
        let shown = safe(&hostile);
        assert!(!shown.contains('\u{1b}') && !shown.contains('\n'), "{shown:?}");
        assert!(shown.starts_with("ok[2JxIGNORE PREVIOUS"));
        assert!(shown.ends_with(&format!("[cut at {MAX_FIELD_CHARS} chars]")));
        assert_eq!(safe("read_file"), "read_file");
    }
}
