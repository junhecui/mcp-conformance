//! P3-05: `openWorldHint`'s decision protocol (architecture.md §4.4, `verdict::open_world_
//! hint`) driven end to end against a real sandboxed run. `egress_attempted` comes from the
//! instrumented arm's own classified destinations (`destination::egress_attempted`, over
//! `network::run_network_isolated_and_bridged`'s output — see `verdict::open_world_hint`'s
//! own doc comment for why this is evaluated from the instrumented arm rather than the
//! strict arm alone). `tool_succeeded` comes from the run's own exit status.
//!
//! **Must not:** run the mock backend (P3-03). A mocked response would make an external
//! connection attempt *look* like it "succeeded" from the sandboxed side's own point of
//! view, but this protocol's `tool_succeeded` question is about the sandboxed process's own
//! exit, not what a mock chose to answer with — mixing the two would make `tool_succeeded`
//! reflect P3-03's mock instead of the tool's actual behaviour.

use std::path::PathBuf;
use std::time::Duration;

use sandbox::OverlaySpec;
use verdict::Assessment;

use crate::destination::{classify_observed_destinations, egress_attempted};
use crate::network::{run_network_isolated_and_bridged, NetworkSessionError};

/// Run `program`/`args` under the instrumented arm (P3-01's network isolation plus P3-02's
/// veth bridge and connection log) and decide `openWorldHint` against `declared`.
///
/// # Errors
/// Whatever `run_network_isolated_and_bridged` itself can fail with.
pub fn assess_open_world_hint(
    declared: bool,
    overlay: OverlaySpec,
    program: PathBuf,
    args: Vec<String>,
    timeout: Duration,
    stdin_release: &[u8],
) -> Result<Assessment, NetworkSessionError> {
    let (outcome, entries) =
        run_network_isolated_and_bridged(overlay, program, args, timeout, stdin_release)?;

    let classified = classify_observed_destinations(&entries);
    let egress = egress_attempted(&classified);
    let tool_succeeded =
        !outcome.timed_out && outcome.exit_status.is_some_and(|status| status.success());

    Ok(verdict::open_world_hint(declared, egress, tool_succeeded))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sandbox::build;

    fn overlay(scratch: &std::path::Path, lower: &std::path::Path) -> OverlaySpec {
        OverlaySpec {
            lower: lower.to_path_buf(),
            upper: scratch.join("upper"),
            work: scratch.join("work"),
            mountpoint: scratch.join("merged"),
        }
    }

    fn run(
        declared: bool,
        script: &str,
    ) -> Assessment {
        let lower_dir = tempfile::tempdir().expect("tempdir");
        build(lower_dir.path(), &[]).expect("build base layer");
        let scratch = tempfile::tempdir().expect("tempdir");

        assess_open_world_hint(
            declared,
            overlay(scratch.path(), lower_dir.path()),
            PathBuf::from("/usr/local/bin/python3"),
            vec!["-c".to_string(), script.to_string()],
            Duration::from_secs(10),
            b"go\n",
        )
        .expect("assess open_world_hint")
    }

    /// architecture.md §4.4's literal exit criterion, first branch, over a real sandboxed
    /// process: a tool declaring `openWorldHint = false` actually attempts an external
    /// connection — `Violated`.
    #[test]
    fn a_tool_that_declares_closed_world_but_reaches_out_is_violated() {
        let script = "\
import socket, sys
sys.stdin.readline()
try:
    socket.create_connection(('93.184.216.34', 80), timeout=5)
except OSError:
    pass
sys.exit(0)
";
        assert_eq!(run(false, script), verdict::Assessment::violated(datamodel::Oracle::KernelChangeset));
    }

    /// Second branch: no egress attempted, and the tool finished its work — `Holds`.
    #[test]
    fn a_tool_that_declares_closed_world_and_never_reaches_out_holds() {
        let script = "\
import sys
sys.stdin.readline()
sys.exit(0)
";
        assert_eq!(run(false, script), verdict::Assessment::holds(datamodel::Oracle::KernelChangeset));
    }

    /// Third branch: no egress attempted, but the tool failed anyway (for a reason
    /// unrelated to networking, here a bare non-zero exit) — genuinely ambiguous, so
    /// `Unverifiable` with the specific rerun reason, never silently folded into `Holds` or
    /// `Violated`.
    #[test]
    fn a_tool_that_never_reaches_out_but_fails_anyway_is_unverifiable() {
        let script = "\
import sys
sys.stdin.readline()
sys.exit(1)
";
        assert_eq!(
            run(false, script),
            verdict::Assessment::unverifiable(
                datamodel::Oracle::KernelChangeset,
                datamodel::ReasonCode::EgressAmbiguousRerunInstrumented
            )
        );
    }
}
