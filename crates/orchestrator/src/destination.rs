//! P3-04: classify every connection `network::run_network_isolated_and_bridged` observed
//! (P3-02) as `InSandbox` or `External` (`normalise::classify_destination`), against
//! `sandbox::netns::BRIDGE_NETWORK` — the veth bridge's own subnet, supplied here rather
//! than hardcoded in `normalise` (see that module's own doc comment for why). The one place
//! permitted to depend on `sandbox`, `observe`, and `normalise` at once, the same role this
//! crate already plays for every other cross-crate P2/P3 wiring.
//!
//! **Must not:** decide what a classification means for a verdict — that is P3-05's
//! `openWorldHint` job, over the classifications this module produces.

use datamodel::{ClassifiedDestination, DestinationClass, ObservedDestination};
use observe::connection_log::ConnectionLogEntry;

/// Classify every entry `observe::connection_log::ConnectionLog` recorded, in the order
/// accepted, against `sandbox::netns::BRIDGE_NETWORK`.
#[must_use]
pub fn classify_observed_destinations(entries: &[ConnectionLogEntry]) -> Vec<ClassifiedDestination> {
    let destinations: Vec<ObservedDestination> =
        entries.iter().map(|entry| ObservedDestination::from(*entry)).collect();
    normalise::classify_destinations(&destinations, sandbox::BRIDGE_NETWORK)
}

/// P3-05's `egress_attempted`: whether *any* classified destination was [`DestinationClass::External`].
///
/// This is the one place that answers `verdict::open_world_hint`'s "was egress attempted"
/// question — see that function's own doc comment for why it is evaluated from the
/// instrumented arm's classified destinations rather than inferred from the strict arm
/// alone (P4-02's seccomp/syscall audit log, which would let a future version answer this
/// more cheaply, does not exist yet).
#[must_use]
pub fn egress_attempted(classified: &[ClassifiedDestination]) -> bool {
    classified.iter().any(|c| c.class == DestinationClass::External)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::run_network_isolated_and_bridged;
    use datamodel::DestinationClass;
    use sandbox::{build, OverlaySpec};
    use std::path::PathBuf;
    use std::time::Duration;

    /// P3-04's literal exit criterion, proven against a real sandboxed process's real
    /// connection attempts, not synthetic addresses: a network-isolated process attempts one
    /// connection to an arbitrary external-looking host and one directly to the bridge's own
    /// host-side address (`sandbox::netns`'s `HOST_IP`, the gateway the sandboxed side's
    /// default route already points at) — and classification correctly tells them apart,
    /// using only `sandbox::BRIDGE_NETWORK`'s own published value, never a hardcoded guess.
    #[test]
    fn a_real_external_attempt_and_a_real_bridge_directed_attempt_are_classified_correctly() {
        let lower_dir = tempfile::tempdir().expect("tempdir");
        build(lower_dir.path(), &[]).expect("build base layer");
        let scratch = tempfile::tempdir().expect("tempdir");

        let overlay = OverlaySpec {
            lower: lower_dir.path().to_path_buf(),
            upper: scratch.path().join("upper"),
            work: scratch.path().join("work"),
            mountpoint: scratch.path().join("merged"),
        };

        let script = "\
import socket, sys
sys.stdin.readline()
for host, port in [('93.184.216.34', 80), ('10.200.0.1', 12345)]:
    try:
        socket.create_connection((host, port), timeout=5)
    except OSError:
        pass
print('done')
";

        let (outcome, entries) = run_network_isolated_and_bridged(
            overlay,
            PathBuf::from("/usr/local/bin/python3"),
            vec!["-c".to_string(), script.to_string()],
            Duration::from_secs(10),
            b"go\n",
        )
        .expect("run network-isolated and bridged session");
        assert!(!outcome.timed_out);

        let classified = classify_observed_destinations(&entries);
        let classes: Vec<(std::net::Ipv4Addr, DestinationClass)> = classified
            .iter()
            .map(|c| (std::net::Ipv4Addr::from(c.destination.address), c.class))
            .collect();

        assert_eq!(
            classes,
            vec![
                (std::net::Ipv4Addr::new(93, 184, 216, 34), DestinationClass::External),
                (std::net::Ipv4Addr::new(10, 200, 0, 1), DestinationClass::InSandbox),
            ],
            "the external address must classify as External and the bridge's own gateway \
             address must classify as InSandbox: {classes:?}"
        );
    }
}
