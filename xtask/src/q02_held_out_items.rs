//! Q-02: build the real held-out item set `docs/labelling_protocol.md` specifies, so a human
//! rater has something real to label. This module produces items only — it never labels
//! anything itself and never invents a `destructiveHint` ground truth; see this crate's own
//! [`crate::q02_held_out_items`] doc comment continuation below for the corpus this draws on.
//!
//! # Corpus: server-everything widened to its full tool list, plus server-filesystem
//!
//! `@modelcontextprotocol/server-everything` (already used by `first_verdict`/`ruleset_v2`/
//! `fixture_generality`) is a protocol-demonstration server — P2-10/P3-06 already found its
//! own tools mostly produce empty changesets, so it alone cannot supply the destructive/
//! additive diversity the protocol's stratification asks for. `@modelcontextprotocol/
//! server-filesystem` (the official reference filesystem server, confirmed by hand via a real
//! `tools/list` round trip before this module was written) is added specifically for that:
//! its `write_file`/`edit_file`/`move_file` tools declare real `destructiveHint: true`
//! annotations and, pointed at a directory seeded with real pre-existing files, produce real
//! deletions/overwrites — not just additions.
//!
//! # Never shown to a rater
//!
//! Per `docs/labelling_protocol.md`, an item's record never includes the tool's own declared
//! `destructiveHint`/`idempotentHint`/`readOnlyHint` — this module does not even read those
//! fields out of the `tools/list` response for inclusion here, so there is nothing to
//! accidentally leak into a rater-facing file.
//!
//! # What "shown to rater" excludes, and why
//!
//! Per the protocol, an item whose mechanical partition is empty (nothing in `user_state`
//! changed at all) is `Additive` by definition and is not put to a rater — recorded in this
//! module's output for transparency (so the corpus's real shape is visible, not hidden), but
//! marked `shown_to_rater: false`.

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use discovery::jsonrpc;
use sandbox::{EntryKind, EntrySpec, OverlaySpec, SandboxSpec};
use serde_json::{json, Value};

const RESULT_PATH: &str = "results/conformance/q02_held_out_items.json";
const RULESET_PATH: &str = "rulesets/v1.json";
const PER_TOOL_TIMEOUT: Duration = Duration::from_secs(20);
const EVERYTHING_PACKAGE: &str = "@modelcontextprotocol/server-everything";
const FILESYSTEM_PACKAGE: &str = "@modelcontextprotocol/server-filesystem";

/// Why building the held-out item set failed outright. A single tool call misbehaving is not
/// this — that becomes a recorded `skipped` entry instead, the same discipline
/// `fixture_generality`'s own driver already established.
#[derive(Debug)]
pub enum Q02Error {
    /// A filesystem operation failed.
    Io(std::io::Error),
    /// A JSON message wasn't the expected shape.
    Json(serde_json::Error),
    /// Constructing or launching the sandbox failed.
    Sandbox(sandbox::SpawnError),
    /// Loading `rulesets/v1.json` failed.
    Ruleset(orchestrator::LoadRulesetError),
    /// Storing or reading back evidence failed.
    Store(store::StoreError),
}

impl std::fmt::Display for Q02Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::Json(e) => write!(f, "JSON error: {e}"),
            Self::Sandbox(e) => write!(f, "sandbox error: {e}"),
            Self::Ruleset(e) => write!(f, "ruleset load error: {e}"),
            Self::Store(e) => write!(f, "evidence store error: {e}"),
        }
    }
}

impl std::error::Error for Q02Error {}

impl From<std::io::Error> for Q02Error {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}
impl From<serde_json::Error> for Q02Error {
    fn from(e: serde_json::Error) -> Self {
        Self::Json(e)
    }
}
impl From<sandbox::SpawnError> for Q02Error {
    fn from(e: sandbox::SpawnError) -> Self {
        Self::Sandbox(e)
    }
}
impl From<orchestrator::LoadRulesetError> for Q02Error {
    fn from(e: orchestrator::LoadRulesetError) -> Self {
        Self::Ruleset(e)
    }
}
impl From<store::StoreError> for Q02Error {
    fn from(e: store::StoreError) -> Self {
        Self::Store(e)
    }
}

/// One real assessed run, either ready for a rater or (if its mechanical partition came back
/// empty) auto-classified `Additive` per the protocol's own rule and excluded from the
/// rater-facing subset.
struct HeldOutItem {
    item_id: String,
    source_server: String,
    resolution: String,
    tool_name: String,
    tool_description: String,
    tool_arguments: Value,
    deletions_or_overwrites: Vec<(String, &'static str)>,
    pure_additions: Vec<(String, &'static str)>,
    shown_to_rater: bool,
    note: Option<String>,
}

impl HeldOutItem {
    fn to_json(&self) -> Value {
        json!({
            "item_id": self.item_id,
            "source_server": self.source_server,
            "resolution": self.resolution,
            // Untrusted display text — never executed, never treated as instructions. See
            // docs/labelling_protocol.md's "Untrusted input" section.
            "tool_name": self.tool_name,
            "tool_description": self.tool_description,
            "tool_arguments": self.tool_arguments,
            "mechanical_partition": {
                "deletions_or_overwrites": self.deletions_or_overwrites.iter().map(|(p, k)| json!({
                    "path": p,
                    "change": k,
                })).collect::<Vec<_>>(),
                "pure_additions": self.pure_additions.iter().map(|(p, k)| json!({
                    "path": p,
                    "change": k,
                })).collect::<Vec<_>>(),
            },
            "shown_to_rater": self.shown_to_rater,
            "note": self.note,
        })
    }
}

fn change_kind_str(kind: destructive::ChangeKind) -> &'static str {
    match kind {
        destructive::ChangeKind::Deletion => "Deletion",
        destructive::ChangeKind::Overwrite => "Overwrite",
        destructive::ChangeKind::Addition => "Addition",
    }
}

/// Everything about one item except its computed mechanical partition — grouped into its own
/// type rather than passed as separate arguments to [`item_from_partition`], which clippy's
/// `too_many_arguments` lint (correctly) objects to once the note field is added on top of
/// the five identifying fields every item already needs.
struct ItemContext {
    item_id: String,
    source_server: String,
    resolution: String,
    tool_name: String,
    tool_description: String,
    tool_arguments: Value,
    note: Option<String>,
}

fn item_from_partition(
    context: ItemContext,
    partition: &destructive::MechanicalProxyPartition,
) -> HeldOutItem {
    let ItemContext {
        item_id,
        source_server,
        resolution,
        tool_name,
        tool_description,
        tool_arguments,
        note,
    } = context;
    let deletions_or_overwrites: Vec<(String, &'static str)> = partition
        .deletions_or_overwrites
        .iter()
        .map(|p| (String::from_utf8_lossy(&p.path).into_owned(), change_kind_str(p.change)))
        .collect();
    let pure_additions: Vec<(String, &'static str)> = partition
        .pure_additions
        .iter()
        .map(|p| (String::from_utf8_lossy(&p.path).into_owned(), change_kind_str(p.change)))
        .collect();
    let shown_to_rater = !deletions_or_overwrites.is_empty() || !pure_additions.is_empty();
    let note = if shown_to_rater {
        note
    } else {
        let auto_additive_note = "empty mechanical partition: auto-classified Additive per \
                                   docs/labelling_protocol.md, not shown to a rater.";
        Some(match note {
            Some(existing) => format!("{existing} {auto_additive_note}"),
            None => auto_additive_note.to_string(),
        })
    };
    HeldOutItem {
        item_id,
        source_server,
        resolution,
        tool_name,
        tool_description,
        tool_arguments,
        deletions_or_overwrites,
        pure_additions,
        shown_to_rater,
        note,
    }
}

/// A tiny, one-off newline-delimited JSON-RPC round trip — same shape as
/// `xtask::first_verdict::RawClient`/`xtask::fixture_generality::RawClient`, kept as its own
/// local copy for the same reason those two are: each `xtask` driver is a deliberately
/// disposable one-off (see `fixture_generality`'s own doc comment).
struct RawClient {
    stdin: std::process::ChildStdin,
    reader: BufReader<std::process::ChildStdout>,
    next_id: u64,
}

impl RawClient {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, Q02Error> {
        let id = self.next_id;
        self.next_id += 1;
        let bytes = jsonrpc::encode_request(id, method, params);
        self.stdin.write_all(&bytes)?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()?;
        loop {
            let mut line = String::new();
            let n = self.reader.read_line(&mut line)?;
            if n == 0 {
                return Err(Q02Error::Io(std::io::Error::other(format!(
                    "server closed stdout before responding to {method}"
                ))));
            }
            let envelope: Value = serde_json::from_str(&line)?;
            if envelope.get("id").is_none() {
                continue; // an unsolicited notification — keep reading
            }
            if envelope.get("id").and_then(Value::as_u64) != Some(id) {
                return Err(Q02Error::Io(std::io::Error::other(format!(
                    "response id mismatch for {method}: {line}"
                ))));
            }
            return Ok(envelope.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<(), Q02Error> {
        let bytes = jsonrpc::encode_notification(method, params);
        self.stdin.write_all(&bytes)?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()?;
        Ok(())
    }
}

/// List `program args...`'s tools over its own stdio, skipping unsolicited notifications —
/// generalised from `fixture_generality::list_tools` (same reasoning: `discovery::
/// DiscoveryClient` correctly rejects these servers' unsolicited interleaved notifications as
/// a protocol violation, so a small local client is used instead), parameterised over which
/// server this run needs since this module discovers two, not one.
fn list_tools(program: &str, args: &[String]) -> Result<Vec<Value>, Q02Error> {
    let mut child = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    let stdin = child.stdin.take().expect("piped stdin");
    let reader = BufReader::new(child.stdout.take().expect("piped stdout"));
    let mut client = RawClient { stdin, reader, next_id: 0 };

    client.call(
        "initialize",
        json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": { "name": "mcp-conformance-q02", "version": env!("CARGO_PKG_VERSION") },
        }),
    )?;
    client.notify("notifications/initialized", json!({}))?;
    let tools_result = client.call("tools/list", json!({}))?;

    drop(client);
    let _ = child.kill();
    let _ = child.wait();

    Ok(tools_result.get("tools").and_then(Value::as_array).cloned().unwrap_or_default())
}

/// Capture a directory's own real path set — the same mechanism
/// `mechanical_proxy_real_sandbox.rs` already proved out for a base layer, reused here for
/// both corpora's base layers.
fn capture_paths(root: &Path) -> Result<BTreeSet<Vec<u8>>, Q02Error> {
    let bytes = observe::evtree::capture(root)?;
    let evidence = observe::evtree::decode(&bytes)
        .map_err(|e| Q02Error::Io(std::io::Error::other(e.to_string())))?;
    Ok(evidence.entries.iter().map(|e| e.path.clone()).collect())
}

fn harvest_evidence(
    upper: &Path,
    outcome: &sandbox::SandboxOutcome,
    blob_store: &store::BlobStore,
) -> Result<datamodel::RawEvidence, Q02Error> {
    let observation = observe::harvest(
        upper,
        outcome.exit_status,
        outcome.timed_out,
        outcome.orphans_impossible,
        blob_store,
    )
    .map_err(|e| Q02Error::Io(std::io::Error::other(e)))?;
    let bytes = blob_store.get(&observation.upper_layer_digest)?;
    observe::evtree::decode(&bytes).map_err(|e| Q02Error::Io(std::io::Error::other(e.to_string())))
}

/// Run every one of `@modelcontextprotocol/server-everything`'s discovered tools once (real
/// argument synthesis, real sandboxed `tools/call`), computing Q-01's mechanical partition
/// for each. Widening from P1-08's single `echo` tool to the server's full tool list is
/// exactly the option-1 half of the brainstormed corpus.
fn run_server_everything_items(
    ruleset: &datamodel::Ruleset,
    blob_store: &store::BlobStore,
    scratch_root: &Path,
) -> Result<Vec<HeldOutItem>, Q02Error> {
    let resolution = format!("npx -y {EVERYTHING_PACKAGE} stdio");
    println!("[server-everything] discovering tools: {resolution}");
    let tools = list_tools("npx", &["-y".into(), EVERYTHING_PACKAGE.into(), "stdio".into()])?;
    println!("[server-everything] discovered {} tools", tools.len());

    let base_layer = scratch_root.join("everything-base");
    sandbox::build(&base_layer, &[]).map_err(|e| Q02Error::Io(std::io::Error::other(e)))?;
    let base_layer_paths = capture_paths(&base_layer)?;

    let mut items = Vec::new();
    for tool in &tools {
        let Some(name) = tool.get("name").and_then(Value::as_str) else { continue };
        let description =
            tool.get("description").and_then(Value::as_str).unwrap_or_default().to_string();
        let schema = tool.get("inputSchema").cloned().unwrap_or(Value::Null);

        let arguments = match argsynth::synthesize(&schema, &argsynth::FixtureBindings::new()) {
            Ok(s) => s.arguments,
            Err(e) => {
                println!("[server-everything] `{name}`: argument synthesis failed: {e} — skipped");
                continue;
            }
        };

        let program = orchestrator::ArmProgram {
            base_layer: base_layer.clone(),
            program: PathBuf::from("npx"),
            args: vec!["-y".into(), EVERYTHING_PACKAGE.into(), "stdio".into()],
            tool_name: name.to_string(),
            arguments: arguments.clone(),
            timeout: PER_TOOL_TIMEOUT,
        };
        let tool_scratch = scratch_root.join(format!("everything-{name}"));
        match orchestrator::run_arm_1_prime(&program, &tool_scratch, blob_store) {
            Ok(run) => {
                let partition = destructive::partition(&run.evidence, &base_layer_paths, ruleset);
                println!(
                    "[server-everything] `{name}`: {} deletions/overwrites, {} pure additions",
                    partition.deletions_or_overwrites.len(),
                    partition.pure_additions.len()
                );
                items.push(item_from_partition(
                    ItemContext {
                        item_id: format!("server-everything/{name}"),
                        source_server: EVERYTHING_PACKAGE.to_string(),
                        resolution: resolution.clone(),
                        tool_name: name.to_string(),
                        tool_description: description,
                        tool_arguments: arguments,
                        note: None,
                    },
                    &partition,
                ));
            }
            Err(e) => {
                println!("[server-everything] `{name}`: run failed: {e} — skipped");
            }
        }
    }
    Ok(items)
}

/// Every seed file `build_fs_seed_base_layer` creates, and the exact content each one holds —
/// named as constants so both the base-layer builder and every hand-crafted tool argument
/// below reference the identical strings, rather than two independently maintained copies
/// that could silently drift apart.
const SEED_EXISTING_FILE: &str = "existing_file.txt";
const SEED_EXISTING_CONTENT: &[u8] = b"original content\n";
const SEED_TO_MOVE_FILE: &str = "to_move.txt";
const SEED_TO_MOVE_CONTENT: &[u8] = b"will be moved\n";

fn build_fs_seed_base_layer(root: &Path) -> Result<(), Q02Error> {
    // `sandbox::build` requires entries pre-sorted ascending by raw path bytes, with each
    // directory preceding anything nested under it — `"existing_file.txt" < "subdir" <
    // "subdir/nested.txt" < "to_move.txt"` in byte order (`e` < `s` < `t`), so `subdir` and
    // its child must come before `to_move.txt`, not after.
    sandbox::build(
        root,
        &[
            EntrySpec {
                path: PathBuf::from(SEED_EXISTING_FILE),
                kind: EntryKind::File(SEED_EXISTING_CONTENT.to_vec()),
                mode: 0o644,
            },
            EntrySpec { path: PathBuf::from("subdir"), kind: EntryKind::Directory, mode: 0o755 },
            EntrySpec {
                path: PathBuf::from("subdir/nested.txt"),
                kind: EntryKind::File(b"nested content\n".to_vec()),
                mode: 0o644,
            },
            EntrySpec {
                path: PathBuf::from(SEED_TO_MOVE_FILE),
                kind: EntryKind::File(SEED_TO_MOVE_CONTENT.to_vec()),
                mode: 0o644,
            },
        ],
    )
    .map_err(|e| Q02Error::Io(std::io::Error::other(e)))
}

/// One `server-filesystem` tool call: its own fresh seeded base layer (identical seed every
/// time, so results are comparable across tools), pointed-at mountpoint passed as the CLI
/// allowed-directory argument, one MCP handshake, one `tools/call`.
fn run_filesystem_tool_call(
    scratch_root: &Path,
    label: &str,
    tool_name: &str,
    arguments: Value,
) -> Result<(datamodel::RawEvidence, BTreeSet<Vec<u8>>), Q02Error> {
    let root = scratch_root.join(label);
    let base_layer = root.join("base");
    build_fs_seed_base_layer(&base_layer)?;
    let base_layer_paths = capture_paths(&base_layer)?;

    let overlay = OverlaySpec {
        lower: base_layer,
        upper: root.join("upper"),
        work: root.join("work"),
        mountpoint: root.join("merged"),
    };
    let mountpoint_arg = overlay.mountpoint.to_string_lossy().into_owned();
    // The overlay is mounted in a private mount namespace, not chrooted (Phase 1's documented
    // containment scope — see `first_verdict`'s own doc comment) — the mountpoint's absolute
    // host path is exactly what the sandboxed process itself sees, so it is exactly what
    // `server-filesystem`'s own CLI argument (an allow-listed root directory) must be.
    std::fs::create_dir_all(&overlay.mountpoint)?;
    let spec = SandboxSpec {
        overlay: overlay.clone(),
        program: "npx".into(),
        args: vec!["-y".into(), FILESYSTEM_PACKAGE.into(), mountpoint_arg],
        timeout: PER_TOOL_TIMEOUT,
        network_isolated: false,
    };
    let (handle, stdin, stdout) = sandbox::spawn(&spec)?;
    let mut client = RawClient { stdin, reader: BufReader::new(stdout), next_id: 0 };

    client.call(
        "initialize",
        json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": { "name": "mcp-conformance-q02", "version": env!("CARGO_PKG_VERSION") },
        }),
    )?;
    client.notify("notifications/initialized", json!({}))?;
    client.call("tools/call", json!({ "name": tool_name, "arguments": arguments }))?;

    drop(client);
    let outcome = handle.wait()?;

    let store_dir = root.join("evidence-store");
    let blob_store = store::BlobStore::open(&store_dir)?;
    let evidence = harvest_evidence(&overlay.upper, &outcome, &blob_store)?;
    Ok((evidence, base_layer_paths))
}

/// Every `server-filesystem` item this module builds: real destructive/additive/mixed/empty
/// shapes, hand-picked (not schema-generic) precisely because the whole point of adding this
/// server is to exercise its real pre-existing-state mutations — a generic schema-only
/// argument synthesis would just as likely target a path that doesn't exist yet, producing
/// another pure addition instead of the overwrite/deletion diversity option 2 was for.
fn filesystem_tool_plan() -> Vec<(&'static str, &'static str, Value)> {
    vec![
        (
            "write_file_overwrite",
            "write_file",
            json!({
                "path": SEED_EXISTING_FILE,
                "content": "overwritten by Q-02 held-out set generation\n",
            }),
        ),
        (
            "write_file_new",
            "write_file",
            json!({
                "path": "brand_new_file.txt",
                "content": "brand new content for Q-02 held-out set generation\n",
            }),
        ),
        (
            "edit_file",
            "edit_file",
            json!({
                "path": SEED_EXISTING_FILE,
                "edits": [{
                    "oldText": String::from_utf8_lossy(SEED_EXISTING_CONTENT),
                    "newText": "edited content\n",
                }],
                // `argsynth`'s generic boolean synthesis always produces `true`
                // (`argsynth::synthesize_at`'s own `"boolean" => Ok(Value::Bool(true))`) —
                // explicit here because a dry run would write nothing, defeating the whole
                // point of this item.
                "dryRun": false,
            }),
        ),
        (
            "move_file",
            "move_file",
            json!({
                "source": SEED_TO_MOVE_FILE,
                "destination": "moved_target.txt",
            }),
        ),
        (
            "create_directory",
            "create_directory",
            json!({ "path": "new_dir/nested_subdir" }),
        ),
        (
            "list_directory_readonly",
            "list_directory",
            json!({ "path": "." }),
        ),
        (
            "read_text_file_readonly",
            "read_text_file",
            json!({ "path": SEED_EXISTING_FILE }),
        ),
    ]
}

fn run_server_filesystem_items(
    ruleset: &datamodel::Ruleset,
    tool_descriptions: &std::collections::HashMap<String, String>,
    scratch_root: &Path,
) -> Result<Vec<HeldOutItem>, Q02Error> {
    let resolution_template = format!("npx -y {FILESYSTEM_PACKAGE} <mountpoint>");
    let mut items = Vec::new();
    for (label, tool_name, arguments) in filesystem_tool_plan() {
        match run_filesystem_tool_call(scratch_root, label, tool_name, arguments.clone()) {
            Ok((evidence, base_layer_paths)) => {
                let partition = destructive::partition(&evidence, &base_layer_paths, ruleset);
                println!(
                    "[server-filesystem] `{label}` ({tool_name}): {} deletions/overwrites, {} \
                     pure additions",
                    partition.deletions_or_overwrites.len(),
                    partition.pure_additions.len()
                );
                let description = tool_descriptions.get(tool_name).cloned().unwrap_or_default();
                items.push(item_from_partition(
                    ItemContext {
                        item_id: format!("server-filesystem/{label}"),
                        source_server: FILESYSTEM_PACKAGE.to_string(),
                        resolution: resolution_template.clone(),
                        tool_name: tool_name.to_string(),
                        tool_description: description,
                        tool_arguments: arguments,
                        note: None,
                    },
                    &partition,
                ));
            }
            Err(e) => {
                println!("[server-filesystem] `{label}` ({tool_name}): run failed: {e} — skipped");
            }
        }
    }
    Ok(items)
}

/// Build the real held-out item set and write [`RESULT_PATH`].
///
/// # Errors
///
/// Returns [`Q02Error`] if loading the ruleset, discovering either server's tools, or setting
/// up shared scratch state fails. A single tool call's own run failing is not an error here —
/// it is logged and that tool is skipped, the same discipline `fixture_generality` already
/// established.
pub fn run() -> Result<(), Q02Error> {
    let ruleset = orchestrator::load_ruleset(Path::new(RULESET_PATH))?;
    let scratch = tempfile::tempdir()?;

    let everything_blob_store = store::BlobStore::open(scratch.path().join("everything-store"))?;
    let mut items =
        run_server_everything_items(&ruleset, &everything_blob_store, scratch.path())?;

    println!("[server-filesystem] discovering tools: npx -y {FILESYSTEM_PACKAGE} <probe-dir>");
    let probe_dir = scratch.path().join("fs-schema-probe");
    std::fs::create_dir_all(&probe_dir)?;
    let fs_tools = list_tools(
        "npx",
        &["-y".into(), FILESYSTEM_PACKAGE.into(), probe_dir.to_string_lossy().into_owned()],
    )?;
    let tool_descriptions: std::collections::HashMap<String, String> = fs_tools
        .iter()
        .filter_map(|t| {
            let name = t.get("name").and_then(Value::as_str)?.to_string();
            let description = t.get("description").and_then(Value::as_str)?.to_string();
            Some((name, description))
        })
        .collect();

    items.extend(run_server_filesystem_items(&ruleset, &tool_descriptions, scratch.path())?);

    write_result(&items)
}

fn write_result(items: &[HeldOutItem]) -> Result<(), Q02Error> {
    let shown_to_rater = items.iter().filter(|i| i.shown_to_rater).count();
    let record = json!({
        "task": "Q-02",
        "protocol": "docs/labelling_protocol.md",
        "note": "Item set only — no labels, no agreement statistic. Producing those requires \
                 real independent human raters (see docs/labelling_protocol.md); this file is \
                 what they label. Every item's tool_name/tool_description/tool_arguments are \
                 untrusted display text, never executed. The tool's own declared \
                 destructiveHint/idempotentHint/readOnlyHint are deliberately not included \
                 anywhere in this file, to avoid a rater anchoring on them.",
        "corpus": [EVERYTHING_PACKAGE, FILESYSTEM_PACKAGE],
        "total_items": items.len(),
        "shown_to_rater": shown_to_rater,
        "auto_additive_skipped": items.len() - shown_to_rater,
        "items": items.iter().map(HeldOutItem::to_json).collect::<Vec<_>>(),
    });
    if let Some(parent) = Path::new(RESULT_PATH).parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(RESULT_PATH, serde_json::to_string_pretty(&record)?)?;
    println!(
        "wrote {RESULT_PATH}: {} items total, {shown_to_rater} shown to a rater, {} \
         auto-classified Additive (empty partition)",
        items.len(),
        items.len() - shown_to_rater
    );
    Ok(())
}
