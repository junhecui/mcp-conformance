//! Catalogue ingest (P0-03): resolve one registry entry — a `server.json` document, per the
//! official MCP Registry schema (<https://registry.modelcontextprotocol.io>,
//! schema `2025-12-11`) — into installable artifacts and/or remote endpoints, with
//! provenance recorded.
//!
//! **Must not:** execute anything. There is no code path in this module that spawns a
//! process, invokes a package manager, or fetches anything — it only parses a `server.json`
//! document already in hand into structured data. Fetching documents from a live registry
//! is a separate, later concern; this module's contract starts once bytes already exist.

use serde::Deserialize;

/// One resolved registry entry: everything P0-03 could extract about how to reach or
/// install a server, plus where the entry itself came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedServer {
    /// The server's reverse-DNS-style registry name (e.g. `io.example/sample-server`).
    pub name: String,
    /// Where this entry says its source lives.
    pub provenance: Provenance,
    /// Every package and/or remote the entry declared with enough fields to act on. The
    /// spec allows `packages` and `remotes` to coexist — both end up here.
    pub targets: Vec<ResolvedTarget>,
    /// Sub-entries present in the source document but skipped for missing required
    /// fields — recorded, not silently dropped, same discipline as the top-level
    /// [`IngestOutcome::Unresolvable`] case one level up.
    pub skipped: Vec<String>,
}

/// Where a registry entry says its source lives.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Provenance {
    /// `repository.url` from the source document.
    pub repository_url: Option<String>,
    /// `repository.source` from the source document (e.g. `"github"`).
    pub repository_source: Option<String>,
}

/// One way to reach or install a server, as declared by the registry entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedTarget {
    /// An installable package (`packages[]` in the source document).
    Package {
        /// `npm`, `pypi`, `cargo`, `nuget`, `oci`, `mcpb`, ...
        registry_type: String,
        /// The registry endpoint this package is published to, if the entry says.
        registry_base_url: Option<String>,
        /// The package name/path within its registry.
        identifier: String,
        /// The package version.
        version: String,
        /// The declared transport (`stdio`, ...), if the entry says.
        transport: Option<String>,
    },
    /// A remote, already-running endpoint (`remotes[]` in the source document).
    Endpoint {
        /// `streamable-http`, `sse`, ...
        transport_type: String,
        /// The endpoint URL. May contain unresolved template variables — resolving those
        /// is a later concern, not this module's.
        url: String,
    },
}

/// The outcome of ingesting one registry entry.
///
/// Deliberately not a `Result`: `Unresolvable` is a recorded outcome, not an error a
/// caller could discard. A binary success/failure would force a caller to choose between
/// crashing on bad registry data and silently swallowing it — the same false choice
/// `unclassifiable` (P0-04) and `unverifiable` (the verdict engine) exist elsewhere in this
/// project to avoid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IngestOutcome {
    /// At least one package or remote was usable.
    Resolved(ResolvedServer),
    /// Nothing usable could be extracted. `name` is populated whenever the document got
    /// far enough to have one, even though everything else about it failed.
    Unresolvable {
        /// The entry's declared name, if parsing got that far.
        name: Option<String>,
        /// Why. Always human-readable prose, never a code — this crate has no closed
        /// taxonomy of failure reasons the way `unverifiable` does downstream.
        reason: String,
    },
}

/// Resolve one registry entry from its raw `server.json` bytes.
///
/// Infallible by construction: every input, however malformed, produces an
/// [`IngestOutcome`] rather than an `Err` a caller could forget to persist.
#[must_use]
pub fn ingest(raw: &[u8]) -> IngestOutcome {
    let text = match std::str::from_utf8(raw) {
        Ok(t) => t,
        Err(e) => {
            return IngestOutcome::Unresolvable { name: None, reason: format!("not valid UTF-8: {e}") };
        }
    };

    let doc: ServerJson = match serde_json::from_str(text) {
        Ok(d) => d,
        Err(e) => {
            return IngestOutcome::Unresolvable { name: None, reason: format!("not valid JSON: {e}") };
        }
    };

    let Some(name) = doc.name else {
        return IngestOutcome::Unresolvable {
            name: None,
            reason: "missing required field `name`".into(),
        };
    };

    let provenance = Provenance {
        repository_url: doc.repository.as_ref().and_then(|r| r.url.clone()),
        repository_source: doc.repository.as_ref().and_then(|r| r.source.clone()),
    };

    let mut targets = Vec::new();
    let mut skipped = Vec::new();

    for (i, pkg) in doc.packages.unwrap_or_default().into_iter().enumerate() {
        match (pkg.registry_type, pkg.identifier, pkg.version) {
            (Some(registry_type), Some(identifier), Some(version)) => {
                targets.push(ResolvedTarget::Package {
                    registry_type,
                    registry_base_url: pkg.registry_base_url,
                    identifier,
                    version,
                    transport: pkg.transport.and_then(|t| t.transport_type),
                });
            }
            _ => skipped.push(format!("packages[{i}]: missing registryType, identifier, or version")),
        }
    }

    for (i, remote) in doc.remotes.unwrap_or_default().into_iter().enumerate() {
        match (remote.transport_type, remote.url) {
            (Some(transport_type), Some(url)) => {
                targets.push(ResolvedTarget::Endpoint { transport_type, url });
            }
            _ => skipped.push(format!("remotes[{i}]: missing type or url")),
        }
    }

    if targets.is_empty() {
        let reason = if skipped.is_empty() {
            "no packages or remotes declared".to_string()
        } else {
            format!("no usable packages or remotes ({} entries skipped for missing fields)", skipped.len())
        };
        return IngestOutcome::Unresolvable { name: Some(name), reason };
    }

    IngestOutcome::Resolved(ResolvedServer { name, provenance, targets, skipped })
}

// ---- wire shape, private to this module — deliberately permissive (every field
// Option<T>) so one missing field degrades to a skip/Unresolvable instead of rejecting the
// whole document; registry data from third parties is not assumed well-formed. ----

#[derive(Deserialize)]
struct ServerJson {
    name: Option<String>,
    repository: Option<RepositoryJson>,
    packages: Option<Vec<PackageJson>>,
    remotes: Option<Vec<RemoteJson>>,
}

#[derive(Deserialize)]
struct RepositoryJson {
    url: Option<String>,
    source: Option<String>,
}

#[derive(Deserialize)]
struct PackageJson {
    #[serde(rename = "registryType")]
    registry_type: Option<String>,
    #[serde(rename = "registryBaseUrl")]
    registry_base_url: Option<String>,
    identifier: Option<String>,
    version: Option<String>,
    transport: Option<TransportJson>,
}

#[derive(Deserialize)]
struct TransportJson {
    #[serde(rename = "type")]
    transport_type: Option<String>,
}

#[derive(Deserialize)]
struct RemoteJson {
    #[serde(rename = "type")]
    transport_type: Option<String>,
    url: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The official schema's own minimal example
    /// (docs/reference/server-json/generic-server-json.md), not an invented fixture.
    const SPEC_MINIMAL_EXAMPLE: &str = r#"{
        "$schema": "https://static.modelcontextprotocol.io/schemas/2025-12-11/server.schema.json",
        "name": "io.example/sample-server",
        "description": "Sample MCP server",
        "version": "1.0.0",
        "packages": [
            {
                "registryType": "npm",
                "registryBaseUrl": "https://registry.npmjs.org",
                "identifier": "@example/mcp-server",
                "version": "1.0.0",
                "transport": { "type": "stdio" }
            }
        ]
    }"#;

    #[test]
    fn resolves_the_spec_minimal_example() {
        let outcome = ingest(SPEC_MINIMAL_EXAMPLE.as_bytes());
        let IngestOutcome::Resolved(server) = outcome else {
            panic!("expected Resolved, got {outcome:?}");
        };
        assert_eq!(server.name, "io.example/sample-server");
        assert_eq!(server.targets.len(), 1);
        assert!(server.skipped.is_empty());
        match &server.targets[0] {
            ResolvedTarget::Package { registry_type, identifier, version, transport, .. } => {
                assert_eq!(registry_type, "npm");
                assert_eq!(identifier, "@example/mcp-server");
                assert_eq!(version, "1.0.0");
                assert_eq!(transport.as_deref(), Some("stdio"));
            }
            other => panic!("expected Package, got {other:?}"),
        }
    }

    #[test]
    fn resolves_a_remote_endpoint() {
        let raw = br#"{
            "name": "io.example/remote-server",
            "remotes": [{ "type": "streamable-http", "url": "https://example.com/mcp" }]
        }"#;
        let IngestOutcome::Resolved(server) = ingest(raw) else {
            panic!("expected Resolved");
        };
        assert_eq!(server.targets, vec![ResolvedTarget::Endpoint {
            transport_type: "streamable-http".to_string(),
            url: "https://example.com/mcp".to_string(),
        }]);
    }

    #[test]
    fn packages_and_remotes_coexist_per_spec() {
        let raw = br#"{
            "name": "io.example/dual-server",
            "packages": [{"registryType":"pypi","identifier":"example-mcp","version":"2.0.0"}],
            "remotes": [{"type":"sse","url":"https://example.com/sse"}]
        }"#;
        let IngestOutcome::Resolved(server) = ingest(raw) else {
            panic!("expected Resolved");
        };
        assert_eq!(server.targets.len(), 2);
    }

    #[test]
    fn provenance_is_captured_from_the_repository_object() {
        let raw = br#"{
            "name": "io.example/with-repo",
            "repository": { "url": "https://github.com/example/mcp-server", "source": "github" },
            "packages": [{"registryType":"npm","identifier":"x","version":"1.0.0"}]
        }"#;
        let IngestOutcome::Resolved(server) = ingest(raw) else {
            panic!("expected Resolved");
        };
        assert_eq!(server.provenance.repository_url.as_deref(), Some("https://github.com/example/mcp-server"));
        assert_eq!(server.provenance.repository_source.as_deref(), Some("github"));
    }

    #[test]
    fn an_entry_with_neither_packages_nor_remotes_is_unresolvable_not_dropped() {
        let raw = br#"{"name": "io.example/empty-server"}"#;
        let outcome = ingest(raw);
        assert_eq!(
            outcome,
            IngestOutcome::Unresolvable {
                name: Some("io.example/empty-server".to_string()),
                reason: "no packages or remotes declared".to_string(),
            }
        );
    }

    #[test]
    fn a_malformed_package_is_skipped_but_a_valid_sibling_target_still_resolves() {
        let raw = br#"{
            "name": "io.example/partial-server",
            "packages": [{"registryType":"npm"}],
            "remotes": [{"type":"sse","url":"https://example.com/sse"}]
        }"#;
        let IngestOutcome::Resolved(server) = ingest(raw) else {
            panic!("expected Resolved");
        };
        assert_eq!(server.targets.len(), 1);
        assert_eq!(server.skipped.len(), 1);
        assert!(server.skipped[0].contains("packages[0]"));
    }

    #[test]
    fn missing_name_is_unresolvable() {
        let raw = br#"{"packages": [{"registryType":"npm","identifier":"x","version":"1.0.0"}]}"#;
        let outcome = ingest(raw);
        match outcome {
            IngestOutcome::Unresolvable { name: None, reason } => {
                assert!(reason.contains("name"));
            }
            other => panic!("expected Unresolvable with no name, got {other:?}"),
        }
    }

    #[test]
    fn malformed_json_is_unresolvable_not_a_panic() {
        let outcome = ingest(b"not json at all");
        match outcome {
            IngestOutcome::Unresolvable { name: None, reason } => assert!(reason.contains("JSON")),
            other => panic!("expected Unresolvable, got {other:?}"),
        }
    }

    #[test]
    fn non_utf8_bytes_are_unresolvable_not_a_panic() {
        let outcome = ingest(&[0xFF, 0xFE, 0xFD]);
        match outcome {
            IngestOutcome::Unresolvable { name: None, reason } => assert!(reason.contains("UTF-8")),
            other => panic!("expected Unresolvable, got {other:?}"),
        }
    }
}
