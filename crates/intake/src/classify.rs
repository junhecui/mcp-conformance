//! Containability classification (P0-04): decide whether a registry entry is Class A
//! (launchable locally), Class B (remote endpoint only), or genuinely `unclassifiable`.
//!
//! **Must not:** guess. `Unclassifiable` is a real class, not a fallback — every
//! classification is driven by what [`crate::catalogue::ingest`] concretely found, never by
//! absence of information silently defaulting to one class over another.

use datamodel::ContainabilityClass;

use crate::catalogue::{IngestOutcome, ResolvedTarget};

/// A containability class, plus why.
///
/// architecture.md §12 item 3: the Class A/B ratio gates how ambitious Phase 5 can be, so
/// the reason is not optional decoration — it's why a server ended up in one bucket over
/// another, for a report that has to justify that ratio.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classification {
    /// `A`, `B`, or `Unclassifiable` — same type F-06's `SERVER.containability_class`
    /// column and its `CHECK` constraint use, so this never drifts from what gets stored.
    pub class: ContainabilityClass,
    /// Human-readable, never a code — there is no closed reason-code taxonomy at this
    /// layer the way the verdict engine has one (P2-11).
    pub reason: String,
}

/// Classify one registry entry from its ingest outcome.
///
/// A locally-installable package always wins over a remote endpoint when an entry
/// declares both — the MCP Registry schema explicitly allows `packages` and `remotes` to
/// coexist, and "launchable locally" (architecture.md §2) is satisfied the moment *any*
/// package exists, independent of what else the entry also offers.
#[must_use]
pub fn classify(outcome: &IngestOutcome) -> Classification {
    let server = match outcome {
        IngestOutcome::Unresolvable { reason, .. } => {
            return Classification {
                class: ContainabilityClass::Unclassifiable,
                reason: format!("ingest could not resolve any target: {reason}"),
            };
        }
        IngestOutcome::Resolved(server) => server,
    };

    let package_types: Vec<&str> = server
        .targets
        .iter()
        .filter_map(|t| match t {
            ResolvedTarget::Package { registry_type, .. } => Some(registry_type.as_str()),
            ResolvedTarget::Endpoint { .. } => None,
        })
        .collect();

    if !package_types.is_empty() {
        return Classification {
            class: ContainabilityClass::A,
            reason: format!("launchable locally via: {}", package_types.join(", ")),
        };
    }

    let endpoint_count =
        server.targets.iter().filter(|t| matches!(t, ResolvedTarget::Endpoint { .. })).count();
    if endpoint_count > 0 {
        return Classification {
            class: ContainabilityClass::B,
            reason: format!("{endpoint_count} remote endpoint(s) declared, no locally-installable package"),
        };
    }

    // Defense in depth, not dead code by assumption: ingest()'s own contract guarantees
    // Resolved implies a non-empty target list, but this function does not trust a
    // caller-supplied invariant it cannot see enforced — see the test that constructs this
    // case directly, bypassing ingest() entirely.
    Classification {
        class: ContainabilityClass::Unclassifiable,
        reason: "resolved with neither a package nor a remote, which ingest() should never \
                 produce"
            .into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalogue::{Provenance, ResolvedServer, ingest};

    #[test]
    fn a_package_target_classifies_as_class_a() {
        let raw = br#"{
            "name": "io.example/local",
            "packages": [{"registryType":"npm","identifier":"x","version":"1.0.0"}]
        }"#;
        let classification = classify(&ingest(raw));
        assert_eq!(classification.class, ContainabilityClass::A);
        assert!(classification.reason.contains("npm"));
    }

    #[test]
    fn an_endpoint_only_target_classifies_as_class_b() {
        let raw = br#"{
            "name": "io.example/remote",
            "remotes": [{"type":"streamable-http","url":"https://example.com/mcp"}]
        }"#;
        let classification = classify(&ingest(raw));
        assert_eq!(classification.class, ContainabilityClass::B);
    }

    #[test]
    fn a_package_wins_over_a_coexisting_remote() {
        let raw = br#"{
            "name": "io.example/dual",
            "packages": [{"registryType":"pypi","identifier":"x","version":"1.0.0"}],
            "remotes": [{"type":"sse","url":"https://example.com/sse"}]
        }"#;
        let classification = classify(&ingest(raw));
        assert_eq!(classification.class, ContainabilityClass::A);
    }

    #[test]
    fn an_unresolvable_entry_is_unclassifiable_with_the_upstream_reason_carried_through() {
        let raw = br#"{"name": "io.example/empty"}"#;
        let classification = classify(&ingest(raw));
        assert_eq!(classification.class, ContainabilityClass::Unclassifiable);
        assert!(classification.reason.contains("no packages or remotes declared"));
    }

    #[test]
    fn malformed_input_is_unclassifiable_not_a_panic() {
        let classification = classify(&ingest(b"not json"));
        assert_eq!(classification.class, ContainabilityClass::Unclassifiable);
    }

    /// Constructs the "resolved but empty" state directly, bypassing `ingest()`'s own
    /// invariant, to prove `classify` doesn't trust that invariant blindly — it degrades to
    /// `Unclassifiable` rather than panicking or guessing a class.
    #[test]
    fn a_resolved_server_with_no_targets_at_all_is_unclassifiable_not_a_panic() {
        let outcome = IngestOutcome::Resolved(ResolvedServer {
            name: "io.example/impossible".to_string(),
            provenance: Provenance::default(),
            targets: vec![],
            skipped: vec![],
        });
        let classification = classify(&outcome);
        assert_eq!(classification.class, ContainabilityClass::Unclassifiable);
    }
}
