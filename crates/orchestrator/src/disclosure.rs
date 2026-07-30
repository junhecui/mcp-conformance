//! P5-03: the responsible-disclosure workflow design.md's open question 4 asks for — an
//! embargo state machine over `VERDICT.embargo_state`/`disclosed_at`, and a maintainer
//! contact path derived from a server's own registry provenance.
//!
//! architecture.md §12 item 6's own framing: "Records need an embargo state and a
//! disclosure timestamp." Both columns landed on `VERDICT` back in F-06, ahead of Phase 5,
//! specifically so this task would have something real to give behaviour to rather than
//! adding schema and workflow in the same change.
//!
//! **Scope, stated plainly:** this module computes *where to send a report*
//! ([`maintainer_contact_path`]) and *whether a state transition is allowed*
//! ([`advance_embargo`]). It never sends anything anywhere on its own — filing a real issue
//! or emailing a real maintainer is a human decision with real consequences for a real
//! third party, made once per finding, not something a batch job should ever do
//! unattended. The same posture P4-05 already took with observation evasion: a boundary
//! this task does not cross, disclosed here rather than discovered by its absence.
//!
//! **Why the state machine only allows `None -> Embargoed -> Disclosed`, never `None ->
//! Disclosed` directly:** design.md/architecture.md's own recommendation for open question 3
//! is "aggregate by default, named on violation after disclosure." A verdict that will only
//! ever be published in aggregate never needs to enter this state machine at all — it just
//! stays [`datamodel::EmbargoState::None`] forever. The only reason a verdict *does* enter
//! this machine is that someone has decided to name it, and "disclosed" is supposed to mean
//! "the maintainer-contact step actually happened," not "we skipped straight to publishing."
//! Allowing `None -> Disclosed` would let that meaning quietly rot.

use datamodel::EmbargoState;
use intake::catalogue::Provenance;
use rusqlite::Connection;

/// Where to reach a server's maintainer for responsible disclosure, derived from whatever
/// provenance the registry entry itself declared (`intake::catalogue::Provenance`, P0-03).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContactPath {
    /// File an issue against the server's own repository — the de facto responsible-
    /// disclosure channel for an open-source project with no separately published security
    /// contact. Carries the issues URL directly (`{repository_url}/issues`), not just the
    /// repository URL, so a human acting on this doesn't have to reconstruct it.
    GitHubIssues(String),
    /// The registry entry's provenance doesn't name a source this function knows how to
    /// derive a contact path from (a non-GitHub source, or none recorded at all). Not an
    /// error — plenty of real registry entries are exactly this incomplete; a human
    /// deciding to disclose against one of these has to find a contact path some other way.
    Unknown,
}

/// Derive [`ContactPath`] from `provenance`. Pure: no network access, no guessing beyond
/// what the registry entry itself already declared.
#[must_use]
pub fn maintainer_contact_path(provenance: &Provenance) -> ContactPath {
    match (provenance.repository_source.as_deref(), provenance.repository_url.as_deref()) {
        (Some("github"), Some(url)) if !url.is_empty() => {
            ContactPath::GitHubIssues(format!("{}/issues", url.trim_end_matches('/')))
        }
        _ => ContactPath::Unknown,
    }
}

/// Why an embargo transition was refused.
#[derive(Debug, PartialEq)]
pub enum DisclosureError {
    /// No `VERDICT` row exists for the given `verdict_id`.
    UnknownVerdict,
    /// The requested transition isn't one this state machine allows — see this module's
    /// own doc comment for exactly which transitions are legal and why.
    InvalidTransition {
        /// The verdict's current state.
        from: EmbargoState,
        /// The state that was requested.
        to: EmbargoState,
    },
    /// A transition to [`EmbargoState::Disclosed`] was requested with no timestamp, or a
    /// transition to any other state was requested *with* one — the schema's own `CHECK`
    /// (`0004_verdict_embargo_consistency.sql`) would catch this too, but this module
    /// reports the specific mistake rather than surfacing a raw constraint-violation error.
    TimestampMismatch,
    /// The metadata DB reported an error.
    Db(rusqlite::Error),
}

impl std::fmt::Display for DisclosureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownVerdict => write!(f, "no verdict exists for that verdict_id"),
            Self::InvalidTransition { from, to } => {
                write!(f, "cannot transition embargo state from {from} to {to}")
            }
            Self::TimestampMismatch => {
                write!(f, "disclosed_at must be set if and only if the target state is `disclosed`")
            }
            Self::Db(e) => write!(f, "metadata DB error: {e}"),
        }
    }
}

impl std::error::Error for DisclosureError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Db(e) => Some(e),
            Self::UnknownVerdict | Self::InvalidTransition { .. } | Self::TimestampMismatch => None,
        }
    }
}

impl From<rusqlite::Error> for DisclosureError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Db(e)
    }
}

/// Whether `from -> to` is a legal embargo transition — see this module's own doc comment
/// for why the machine is exactly `None -> Embargoed -> Disclosed`, one-way, no shortcuts.
#[must_use]
pub const fn is_valid_transition(from: EmbargoState, to: EmbargoState) -> bool {
    matches!(
        (from, to),
        (EmbargoState::None, EmbargoState::Embargoed)
            | (EmbargoState::Embargoed, EmbargoState::Disclosed)
    )
}

/// Attempt to move `verdict_id`'s embargo state to `target`, writing the change only if the
/// transition is legal and `disclosed_at`'s presence agrees with `target`.
///
/// # Errors
/// [`DisclosureError::UnknownVerdict`] if `verdict_id` doesn't exist;
/// [`DisclosureError::InvalidTransition`] if the current state cannot move to `target`;
/// [`DisclosureError::TimestampMismatch`] if `disclosed_at.is_some()` disagrees with
/// `target == EmbargoState::Disclosed`; otherwise whatever the metadata DB itself fails
/// with.
pub fn advance_embargo(
    conn: &Connection,
    verdict_id: &str,
    target: EmbargoState,
    disclosed_at: Option<&str>,
) -> Result<(), DisclosureError> {
    let current = store::db::get_verdict_embargo_state(conn, verdict_id)?
        .ok_or(DisclosureError::UnknownVerdict)?;

    if !is_valid_transition(current.embargo_state, target) {
        return Err(DisclosureError::InvalidTransition { from: current.embargo_state, to: target });
    }
    if disclosed_at.is_some() != (target == EmbargoState::Disclosed) {
        return Err(DisclosureError::TimestampMismatch);
    }

    store::db::set_verdict_embargo_state(conn, verdict_id, target, disclosed_at)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed_verdict(conn: &Connection) {
        conn.execute_batch(
            "INSERT INTO server (server_id, source_uri, containability_class, spec_revision)
             VALUES ('srv-1', 'stdio://tool', 'A', '2026-06-18');
             INSERT INTO tool_snapshot
             (snapshot_id, server_id, tool_name, metadata_pin, annotations_raw,
              readonly_explicit, destructive_explicit, idempotent_explicit,
              openworld_explicit, observed_at)
             VALUES ('snap-1', 'srv-1', 'read_file', 'pin-1', '{}', 1, 0, 0, 0, 'now');",
        )
        .expect("seed server/tool_snapshot");
        store::db::insert_verdict(
            conn,
            &store::db::VerdictRecord {
                verdict_id: "v-1",
                snapshot_id: "snap-1",
                annotation: datamodel::Annotation::ReadOnlyHint,
                declared: "true",
                outcome: datamodel::Outcome::Violated,
                reason_code: None,
                oracle: datamodel::Oracle::KernelChangeset,
                ruleset_version: None,
                protocol_version: "2026-06-18",
                derived_at: "unix:0",
            },
        )
        .expect("insert_verdict");
    }

    // ---- Contact path ----

    #[test]
    fn a_github_sourced_repository_derives_an_issues_url() {
        let provenance = Provenance {
            repository_url: Some("https://github.com/example/mcp-server".to_string()),
            repository_source: Some("github".to_string()),
        };
        assert_eq!(
            maintainer_contact_path(&provenance),
            ContactPath::GitHubIssues("https://github.com/example/mcp-server/issues".to_string())
        );
    }

    #[test]
    fn a_trailing_slash_on_the_repository_url_does_not_double_up() {
        let provenance = Provenance {
            repository_url: Some("https://github.com/example/mcp-server/".to_string()),
            repository_source: Some("github".to_string()),
        };
        assert_eq!(
            maintainer_contact_path(&provenance),
            ContactPath::GitHubIssues("https://github.com/example/mcp-server/issues".to_string())
        );
    }

    #[test]
    fn a_non_github_source_has_no_derivable_contact_path() {
        let provenance = Provenance {
            repository_url: Some("https://gitlab.com/example/mcp-server".to_string()),
            repository_source: Some("gitlab".to_string()),
        };
        assert_eq!(maintainer_contact_path(&provenance), ContactPath::Unknown);
    }

    #[test]
    fn no_recorded_provenance_at_all_has_no_derivable_contact_path() {
        assert_eq!(maintainer_contact_path(&Provenance::default()), ContactPath::Unknown);
    }

    // ---- Embargo state machine ----

    /// The exit criterion's own literal words, over a real DB: `none -> embargoed ->
    /// disclosed` succeeds, and `disclosed_at` reads back exactly as set.
    #[test]
    fn the_full_embargo_lifecycle_succeeds_in_order() {
        let conn = store::db::open_and_migrate(":memory:").expect("open_and_migrate");
        seed_verdict(&conn);

        advance_embargo(&conn, "v-1", EmbargoState::Embargoed, None).expect("embargo");
        let mid = store::db::get_verdict_embargo_state(&conn, "v-1").unwrap().unwrap();
        assert_eq!(mid.embargo_state, EmbargoState::Embargoed);
        assert_eq!(mid.disclosed_at, None);

        advance_embargo(&conn, "v-1", EmbargoState::Disclosed, Some("unix:100")).expect("disclose");
        let done = store::db::get_verdict_embargo_state(&conn, "v-1").unwrap().unwrap();
        assert_eq!(done.embargo_state, EmbargoState::Disclosed);
        assert_eq!(done.disclosed_at.as_deref(), Some("unix:100"));
    }

    #[test]
    fn skipping_straight_to_disclosed_is_rejected() {
        let conn = store::db::open_and_migrate(":memory:").expect("open_and_migrate");
        seed_verdict(&conn);

        let err = advance_embargo(&conn, "v-1", EmbargoState::Disclosed, Some("unix:0"))
            .expect_err("None -> Disclosed must be rejected");
        assert_eq!(
            err,
            DisclosureError::InvalidTransition { from: EmbargoState::None, to: EmbargoState::Disclosed }
        );

        // And the DB must be unchanged — a rejected transition must not partially apply.
        let still = store::db::get_verdict_embargo_state(&conn, "v-1").unwrap().unwrap();
        assert_eq!(still.embargo_state, EmbargoState::None);
    }

    #[test]
    fn reverting_a_disclosed_verdict_is_rejected() {
        let conn = store::db::open_and_migrate(":memory:").expect("open_and_migrate");
        seed_verdict(&conn);
        advance_embargo(&conn, "v-1", EmbargoState::Embargoed, None).expect("embargo");
        advance_embargo(&conn, "v-1", EmbargoState::Disclosed, Some("unix:0")).expect("disclose");

        let err = advance_embargo(&conn, "v-1", EmbargoState::Embargoed, None)
            .expect_err("Disclosed -> Embargoed must be rejected");
        assert_eq!(
            err,
            DisclosureError::InvalidTransition {
                from: EmbargoState::Disclosed,
                to: EmbargoState::Embargoed
            }
        );
    }

    #[test]
    fn cancelling_an_embargo_back_to_none_is_rejected() {
        let conn = store::db::open_and_migrate(":memory:").expect("open_and_migrate");
        seed_verdict(&conn);
        advance_embargo(&conn, "v-1", EmbargoState::Embargoed, None).expect("embargo");

        let err = advance_embargo(&conn, "v-1", EmbargoState::None, None)
            .expect_err("Embargoed -> None must be rejected");
        assert_eq!(
            err,
            DisclosureError::InvalidTransition { from: EmbargoState::Embargoed, to: EmbargoState::None }
        );
    }

    #[test]
    fn disclosing_without_a_timestamp_is_rejected() {
        let conn = store::db::open_and_migrate(":memory:").expect("open_and_migrate");
        seed_verdict(&conn);
        advance_embargo(&conn, "v-1", EmbargoState::Embargoed, None).expect("embargo");

        let err = advance_embargo(&conn, "v-1", EmbargoState::Disclosed, None)
            .expect_err("Disclosed with no timestamp must be rejected");
        assert_eq!(err, DisclosureError::TimestampMismatch);
    }

    #[test]
    fn embargoing_with_a_timestamp_is_rejected() {
        let conn = store::db::open_and_migrate(":memory:").expect("open_and_migrate");
        seed_verdict(&conn);

        let err = advance_embargo(&conn, "v-1", EmbargoState::Embargoed, Some("unix:0"))
            .expect_err("Embargoed with a timestamp must be rejected");
        assert_eq!(err, DisclosureError::TimestampMismatch);
    }

    #[test]
    fn advancing_an_unknown_verdict_is_reported_distinctly() {
        let conn = store::db::open_and_migrate(":memory:").expect("open_and_migrate");
        let err = advance_embargo(&conn, "no-such-verdict", EmbargoState::Embargoed, None)
            .expect_err("must fail");
        assert_eq!(err, DisclosureError::UnknownVerdict);
    }
}
