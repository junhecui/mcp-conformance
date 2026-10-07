//! Stage 2 disk hygiene (P0-10 scope item 6): remove the OCI images a census sweep pulled,
//! and only those, and find the volume Docker actually stores images on so the free-space
//! floor guards it.
//!
//! # What is removed
//!
//! Only an `oci` candidate's own image reference, and only when all of these hold:
//!
//! 1. The reference was **absent before the sweep's attempts began** — Docker itself said
//!    `No such image` for it (checked after the wrapper images were pre-pulled, so a candidate
//!    naming `node:22-alpine` under any spelling is never owned).
//! 2. The pre-sweep **image-ID snapshot** (`docker image ls --all --no-trunc --quiet`) was
//!    taken successfully. If it could not be taken, removal is disabled for the whole sweep:
//!    a sweep must never delete an image it cannot prove it pulled.
//! 3. At removal time the reference resolves to an image ID that is **not protected** —
//!    neither in that snapshot nor one of the wrapper images. A brand-new *name* for content
//!    that already existed (`foo:1.0` pulled when `foo:latest` with the same digest was
//!    already there) is therefore left alone, not untagged: on the classic graph driver an
//!    untag is harmless, but if the pre-existing copy were dangling, `docker image rm` of the
//!    new name would delete the pre-existing content too. Leaving one stray tag behind is the
//!    cheap side of that trade.
//!
//! Wrapper images (`node:22-alpine`, the `uv` image) pre-pulled by the sweep's own setup are
//! deliberately **kept**: every npm/pypi attempt shares them, they are "pulled by the sweep",
//! not "pulled by an attempt", and re-downloading them per run would cost more than their
//! footprint. The report lists any the sweep pulled so the operator can remove them by hand.
//!
//! Anything else that appeared in Docker's image list during the sweep is reported as
//! `unattributed_new_image_ids` and **never removed**: Docker is shared host state, and an
//! image this sweep cannot attribute to one of its own references may be the operator's.
//!
//! # Concurrency (`--jobs N`)
//!
//! Every `oci` attempt brackets its container's whole life with [`ImageHygiene::acquire`] /
//! [`ImageHygiene::release`] on its reference. Removal happens in `release`, only when the
//! last in-flight attempt on that reference finishes, and *while holding the hygiene lock* —
//! so no other attempt on that reference can be between "acquire" and "docker run" while its
//! image is being deleted (it blocks in `acquire` until removal is done, then re-pulls). The
//! lock is held only by `oci` attempts, for the length of one `docker image rm`.
//!
//! Removing per attempt rather than only at the end of the sweep is the point: at sweep scale
//! the OCI images are what eat the disk the 3 GiB floor protects. Two races remain, and both
//! are closed by a final pass ([`ImageHygiene::finish`]) that re-checks every owned reference
//! after all attempts have finished:
//!
//! - two references naming the same content (a tag and a digest) — Docker refuses to remove
//!   an image a running container uses (`conflict ... is being used`); that failure is
//!   recorded and the final pass retries it;
//! - a watchdog-killed `docker run` whose daemon-side pull outlives the CLI process can land
//!   the image *after* the attempt's removal ran; the final pass finds and removes it.
//!
//! Every removal failure is non-fatal and reported in the results file's `image_hygiene`
//! block; a sweep never aborts over a dirty image store.
//!
//! # containerd snapshotter caveat
//!
//! Docker Desktop on this project's Windows host runs the **containerd image store**
//! (`UseContainerdSnapshotter: true`, docs/HANDOFF.md §3.1), not the classic graph driver.
//! There an image is a *name* pointing at an OCI index or manifest; its ID is that index's
//! digest, `docker image ls` can show one row per platform variant, and content blobs (for
//! every platform the pull fetched) are garbage-collected by containerd once no name
//! references them. This module is written to be correct under both stores:
//!
//! - identity is always the full, untruncated ID as Docker reports it (`sha256:…`), compared
//!   as a set — duplicate per-platform rows collapse, and `docker image inspect --format
//!   {{.Id}}` returns the same index digest that `docker image ls --no-trunc` lists;
//! - removal is by the **reference**, never by ID: removing by ID fails outright when the
//!   content has more than one name ("must be forced"), and would remove *every* name — which
//!   could include a pre-existing one;
//! - success is not inferred from `docker image rm`'s exit status alone: the reference is
//!   re-inspected afterwards and only counted as removed once Docker says `No such image`.
//!   Single-platform removal under the containerd store was confirmed working on the dev host;
//!   multi-platform removal was not yet exercised (HANDOFF.md §3.1), which is exactly why the
//!   result is verified rather than assumed.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use serde_json::{Value, json};

/// Bound on Docker-supplied error text kept per image in the results file.
const MAX_REASON_CHARS: usize = 512;

/// Bound on how many unattributed IDs are listed (the count is always exact).
const MAX_UNATTRIBUTED_LISTED: usize = 50;

/// The few Docker image operations hygiene needs — a trait so the decision logic is tested
/// against a fake, never against the host's real image store.
pub(crate) trait ImageStore: Sync {
    /// The full image ID `reference` resolves to locally, `Ok(None)` only when Docker
    /// positively answers `No such image`, `Err` for any other outcome.
    fn image_id(&self, reference: &str) -> Result<Option<String>, String>;
    /// Every image ID currently in the local store.
    fn list_ids(&self) -> Result<BTreeSet<String>, String>;
    /// Remove the name `reference` (and its content, if that was its last name).
    fn remove(&self, reference: &str) -> Result<(), String>;
}

/// [`ImageStore`] over the `docker` CLI.
pub(crate) struct DockerCli;

fn stderr_of(output: &std::process::Output) -> String {
    crate::census_report::bounded(String::from_utf8_lossy(&output.stderr).trim(), MAX_REASON_CHARS)
}

impl ImageStore for DockerCli {
    fn image_id(&self, reference: &str) -> Result<Option<String>, String> {
        let output = Command::new("docker")
            .args(["image", "inspect", "--format", "{{.Id}}", "--", reference])
            .output()
            .map_err(|e| format!("failed to run docker image inspect: {e}"))?;
        if output.status.success() {
            let id = String::from_utf8_lossy(&output.stdout).trim().to_string();
            // One line per argument; a single reference must give exactly one non-empty ID.
            return match id.lines().count() {
                1 if !id.is_empty() => Ok(Some(id)),
                _ => Err(format!("unexpected docker image inspect output: {id:?}")),
            };
        }
        let stderr = stderr_of(&output);
        // Classic and containerd stores both phrase not-found this way ("Error response from
        // daemon: No such image: …" / "Error: No such image: …"); compared case-insensitively.
        if stderr.to_ascii_lowercase().contains("no such image") { Ok(None) } else { Err(stderr) }
    }

    fn list_ids(&self) -> Result<BTreeSet<String>, String> {
        let output = Command::new("docker")
            .args(["image", "ls", "--all", "--no-trunc", "--quiet"])
            .output()
            .map_err(|e| format!("failed to run docker image ls: {e}"))?;
        if !output.status.success() {
            return Err(stderr_of(&output));
        }
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect())
    }

    fn remove(&self, reference: &str) -> Result<(), String> {
        let output = Command::new("docker")
            .args(["image", "rm", "--", reference])
            .output()
            .map_err(|e| format!("failed to run docker image rm: {e}"))?;
        if output.status.success() { Ok(()) } else { Err(stderr_of(&output)) }
    }
}

/// What finally happened to one owned reference.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Fate {
    /// Not yet released by any attempt (or never launched — the sweep stopped first).
    Pending,
    /// Removed and verified gone.
    Removed { image_id: String },
    /// Not in the store when checked — the pull never happened or failed.
    NotPresent,
    /// Resolved to protected content; left in place on purpose.
    KeptProtected { image_id: String },
    /// Removal failed or could not be verified; the image may still be on the host.
    Failed { reason: String },
}

struct State {
    /// In-flight attempts per reference.
    in_flight: HashMap<String, usize>,
    /// Owned references and what has happened to each.
    owned: BTreeMap<String, Fate>,
}

/// Image hygiene for one Stage 2 sweep. See the module docs for the full contract.
pub(crate) struct ImageHygiene<'s, S: ImageStore> {
    store: &'s S,
    /// `None` when the pre-sweep snapshot failed: removal is then disabled entirely.
    protected_ids: Option<BTreeSet<String>>,
    snapshot_error: Option<String>,
    preexisting_count: usize,
    /// Wrapper images the sweep's setup pulled (absent before it), reported, never removed.
    wrapper_images_pulled: Vec<String>,
    state: Mutex<State>,
}

impl<'s, S: ImageStore> ImageHygiene<'s, S> {
    /// Snapshot the image store. Call **before** anything (wrapper pre-pull included) can pull.
    pub(crate) fn snapshot(store: &'s S) -> Self {
        let (protected_ids, snapshot_error) = match store.list_ids() {
            Ok(ids) => (Some(ids), None),
            Err(e) => (None, Some(e)),
        };
        Self {
            store,
            preexisting_count: protected_ids.as_ref().map_or(0, BTreeSet::len),
            protected_ids,
            snapshot_error,
            wrapper_images_pulled: Vec::new(),
            state: Mutex::new(State { in_flight: HashMap::new(), owned: BTreeMap::new() }),
        }
    }

    /// Whether removal is enabled at all (the snapshot succeeded).
    pub(crate) fn enabled(&self) -> bool {
        self.protected_ids.is_some()
    }

    /// After the wrapper images are pre-pulled: add their IDs to the protected set so no
    /// candidate reference that happens to alias one can ever remove it, and note any that
    /// were not there before the sweep. A wrapper that cannot be resolved is simply not
    /// protected by ID — it cannot then be the target of an owned reference either, because
    /// ownership requires its reference to be absent *after* this point (see [`Self::mark_owned`]).
    pub(crate) fn protect_wrappers(&mut self, wrappers: &[&str]) {
        for wrapper in wrappers {
            if let Ok(Some(id)) = self.store.image_id(wrapper) {
                if let Some(protected) = &mut self.protected_ids {
                    if protected.insert(id) {
                        self.wrapper_images_pulled.push((*wrapper).to_string());
                    }
                }
            }
        }
    }

    /// Decide, before any attempt launches, which references this sweep will own: those
    /// Docker positively reports absent right now. Wrapper references are never owned.
    /// Returns the owned set (for logging); a no-op when removal is disabled.
    pub(crate) fn mark_owned<'r>(&self, references: impl IntoIterator<Item = &'r str>, wrappers: &[&str]) -> usize {
        if !self.enabled() {
            return 0;
        }
        let mut state = self.state.lock().expect("hygiene lock poisoned");
        for reference in references {
            if wrappers.contains(&reference) || state.owned.contains_key(reference) {
                continue;
            }
            if let Ok(None) = self.store.image_id(reference) {
                state.owned.insert(reference.to_string(), Fate::Pending);
            }
        }
        state.owned.len()
    }

    /// An attempt on `reference` is about to launch its container.
    pub(crate) fn acquire(&self, reference: &str) {
        let mut state = self.state.lock().expect("hygiene lock poisoned");
        *state.in_flight.entry(reference.to_string()).or_insert(0) += 1;
    }

    /// An attempt on `reference` has finished and its container is gone. If it was the last
    /// in-flight attempt on an owned reference, remove the image now — under the lock, so no
    /// other attempt on it can launch mid-removal.
    pub(crate) fn release(&self, reference: &str) {
        let mut state = self.state.lock().expect("hygiene lock poisoned");
        let remaining = match state.in_flight.get_mut(reference) {
            Some(n) => {
                *n = n.saturating_sub(1);
                *n
            }
            None => 0,
        };
        if remaining > 0 {
            return;
        }
        state.in_flight.remove(reference);
        if state.owned.contains_key(reference) {
            let fate = self.try_remove(reference);
            log_fate(reference, &fate);
            state.owned.insert(reference.to_string(), fate);
        }
    }

    /// One removal attempt, verified. Never panics, never errors out of the sweep.
    fn try_remove(&self, reference: &str) -> Fate {
        let Some(protected) = &self.protected_ids else {
            return Fate::Failed { reason: "removal disabled: no pre-sweep image snapshot".into() };
        };
        let image_id = match self.store.image_id(reference) {
            Ok(None) => return Fate::NotPresent,
            Ok(Some(id)) => id,
            Err(e) => return Fate::Failed { reason: format!("could not inspect before removal: {e}") },
        };
        if protected.contains(&image_id) {
            return Fate::KeptProtected { image_id };
        }
        if let Err(e) = self.store.remove(reference) {
            return Fate::Failed { reason: e };
        }
        match self.store.image_id(reference) {
            Ok(None) => Fate::Removed { image_id },
            Ok(Some(_)) => Fate::Failed { reason: "docker image rm succeeded but the reference still resolves".into() },
            Err(e) => Fate::Failed { reason: format!("could not verify removal: {e}") },
        }
    }

    /// After every attempt has finished: re-check every owned reference that is not already
    /// verified gone (a failed removal, a late daemon-side pull, an attempt never launched),
    /// then report. Must only be called once the worker pool has returned.
    pub(crate) fn finish(self) -> Value {
        let mut state = self.state.lock().expect("hygiene lock poisoned");
        let references: Vec<String> = state.owned.keys().cloned().collect();
        for reference in references {
            let previous = state.owned[&reference].clone();
            if matches!(previous, Fate::KeptProtected { .. }) {
                continue;
            }
            let fate = match (&previous, self.try_remove(&reference)) {
                // Already verified gone and still gone: keep the record of the removal.
                (Fate::Removed { .. }, Fate::NotPresent) => previous.clone(),
                (_, fate) => fate,
            };
            if fate != previous {
                log_fate(&reference, &fate);
            }
            state.owned.insert(reference, fate);
        }

        let unattributed: Option<Vec<String>> = match (&self.protected_ids, self.store.list_ids()) {
            (Some(protected), Ok(now)) => Some(now.difference(protected).cloned().collect()),
            _ => None,
        };
        report(&self, &state.owned, unattributed)
    }
}

fn log_fate(reference: &str, fate: &Fate) {
    match fate {
        Fate::Removed { .. } => eprintln!("census-stage2-class-a:   removed image {reference} (pulled by this sweep)"),
        Fate::Failed { reason } => eprintln!("census-stage2-class-a:   could not remove image {reference}: {reason}"),
        Fate::KeptProtected { .. } => {
            eprintln!("census-stage2-class-a:   kept image {reference}: it resolves to content present before the sweep")
        }
        Fate::Pending | Fate::NotPresent => {}
    }
}

fn report<S: ImageStore>(hygiene: &ImageHygiene<'_, S>, owned: &BTreeMap<String, Fate>, unattributed: Option<Vec<String>>) -> Value {
    let mut removed = Vec::new();
    let mut failed = Vec::new();
    let mut kept = Vec::new();
    let mut not_present = Vec::new();
    for (reference, fate) in owned {
        match fate {
            Fate::Removed { image_id } => removed.push(json!({ "reference": reference, "image_id": image_id })),
            Fate::Failed { reason } => failed.push(json!({ "reference": reference, "reason": reason })),
            Fate::KeptProtected { image_id } => kept.push(json!({ "reference": reference, "image_id": image_id })),
            Fate::NotPresent | Fate::Pending => not_present.push(Value::String(reference.clone())),
        }
    }
    json!({
        "removal_enabled": hygiene.enabled(),
        "snapshot_error": hygiene.snapshot_error,
        "preexisting_image_ids": hygiene.preexisting_count,
        "owned_references": owned.len(),
        "removed": removed,
        "removal_failures": failed,
        "kept_preexisting_content": kept,
        "owned_but_absent_at_end": not_present,
        "wrapper_images_pulled_and_kept": hygiene.wrapper_images_pulled,
        "unattributed_new_image_ids": unattributed.as_ref().map(|ids| {
            json!({
                "count": ids.len(),
                "listed": ids.iter().take(MAX_UNATTRIBUTED_LISTED).collect::<Vec<_>>(),
                "note": "appeared during the sweep but match no reference this sweep owns; never removed",
            })
        }),
    })
}

/// Environment override: a local path on the volume that holds Docker's image storage.
pub(crate) const DOCKER_DATA_PATH_ENV: &str = "MCPCONF_DOCKER_DATA_PATH";

/// Facts about the host [`resolve_docker_data_path`] decides from — gathered by
/// [`docker_data_path`], passed in so the decision itself is testable.
pub(crate) struct DockerHostFacts {
    /// `MCPCONF_DOCKER_DATA_PATH`, if set.
    pub(crate) env_override: Option<OsString>,
    /// `docker info`'s `DockerRootDir`.
    pub(crate) root_dir: String,
    /// `docker info`'s `OperatingSystem` (`Docker Desktop` for Desktop's VM).
    pub(crate) operating_system: String,
    /// Running under WSL (`/proc/sys/kernel/osrelease` mentions Microsoft).
    pub(crate) is_wsl: bool,
    /// `$HOME`.
    pub(crate) home: Option<PathBuf>,
}

/// Which local path's volume to hold the free-space floor against for Docker's storage.
///
/// - `MCPCONF_DOCKER_DATA_PATH` wins (any existing directory).
/// - A native engine: `DockerRootDir` itself, if it exists on this host.
/// - **Docker Desktop**: `DockerRootDir` is inside Desktop's VM and not visible here; the VM's
///   disk image grows into a file on the host OS's own filesystem — on WSL, by default under
///   `%LOCALAPPDATA%\Docker` on `C:` (`/mnt/c`); on macOS, under
///   `~/Library/Containers/com.docker.docker`. That volume is what fills up, so it is what is
///   guarded. Caveat: the VM disk also has its own maximum size, and its inner filesystem can
///   fill before the host volume does; that is not detected here. If Desktop's disk image was
///   relocated, set the override.
///
/// Anything else (a remote `DOCKER_HOST`, rootless with an unreachable root) is an error, not
/// a guess: an unguarded Docker volume is exactly the failure the floor exists to prevent.
pub(crate) fn resolve_docker_data_path(facts: &DockerHostFacts, is_dir: impl Fn(&Path) -> bool) -> Result<PathBuf, String> {
    if let Some(path) = facts.env_override.as_ref().filter(|v| !v.is_empty()).map(PathBuf::from) {
        return if is_dir(&path) {
            Ok(path)
        } else {
            Err(format!("{DOCKER_DATA_PATH_ENV}={} is not a directory", path.display()))
        };
    }
    let unresolved = |why: &str| {
        Err(format!(
            "cannot locate the volume holding Docker's image storage ({why}); set {DOCKER_DATA_PATH_ENV} \
             to a directory on that volume so the free-space floor can guard it"
        ))
    };
    if facts.operating_system.contains("Docker Desktop") {
        let candidate = if facts.is_wsl {
            Some(PathBuf::from("/mnt/c"))
        } else {
            facts.home.as_ref().map(|h| h.join("Library/Containers/com.docker.docker"))
        };
        return match candidate {
            Some(path) if is_dir(&path) => Ok(path),
            _ => unresolved("Docker Desktop's disk image location is not where it is by default"),
        };
    }
    let root = PathBuf::from(facts.root_dir.trim());
    if !root.as_os_str().is_empty() && is_dir(&root) {
        Ok(root)
    } else {
        unresolved(&format!("DockerRootDir {:?} is not a local directory", facts.root_dir.trim()))
    }
}

/// [`resolve_docker_data_path`] against the real host.
pub(crate) fn docker_data_path() -> Result<PathBuf, String> {
    let output = Command::new("docker")
        .args(["info", "--format", "{{.DockerRootDir}}\n{{.OperatingSystem}}"])
        .output()
        .map_err(|e| format!("failed to run docker info: {e}"))?;
    if !output.status.success() {
        return Err(format!("docker info failed: {}", stderr_of(&output)));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut lines = stdout.lines();
    let facts = DockerHostFacts {
        env_override: std::env::var_os(DOCKER_DATA_PATH_ENV),
        root_dir: lines.next().unwrap_or_default().to_string(),
        operating_system: lines.next().unwrap_or_default().to_string(),
        is_wsl: std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .is_ok_and(|r| r.to_ascii_lowercase().contains("microsoft")),
        home: std::env::var_os("HOME").map(PathBuf::from),
    };
    resolve_docker_data_path(&facts, Path::is_dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// An in-memory image store: reference -> ID, plus scripted removal failures.
    #[derive(Default)]
    struct FakeStore {
        images: Mutex<BTreeMap<String, String>>,
        fail_removal: Mutex<BTreeSet<String>>,
        fail_list: AtomicBool,
        removals: Mutex<Vec<String>>,
    }

    impl FakeStore {
        fn with(images: &[(&str, &str)]) -> Self {
            let store = Self::default();
            for (r, id) in images {
                store.pull(r, id);
            }
            store
        }
        fn pull(&self, reference: &str, id: &str) {
            self.images.lock().unwrap().insert(reference.into(), id.into());
        }
        fn has(&self, reference: &str) -> bool {
            self.images.lock().unwrap().contains_key(reference)
        }
    }

    impl ImageStore for FakeStore {
        fn image_id(&self, reference: &str) -> Result<Option<String>, String> {
            Ok(self.images.lock().unwrap().get(reference).cloned())
        }
        fn list_ids(&self) -> Result<BTreeSet<String>, String> {
            if self.fail_list.load(Ordering::SeqCst) {
                return Err("daemon unreachable".into());
            }
            Ok(self.images.lock().unwrap().values().cloned().collect())
        }
        fn remove(&self, reference: &str) -> Result<(), String> {
            self.removals.lock().unwrap().push(reference.into());
            if self.fail_removal.lock().unwrap().contains(reference) {
                return Err("conflict: image is being used by running container".into());
            }
            self.images.lock().unwrap().remove(reference);
            Ok(())
        }
    }

    const WRAPPERS: &[&str] = &["node:22-alpine", "uv:latest"];

    #[test]
    fn an_image_pulled_by_an_attempt_is_removed_and_a_preexisting_one_never_is() {
        let store = FakeStore::with(&[("mine-before:1", "sha256:old"), ("node:22-alpine", "sha256:node")]);
        let mut hygiene = ImageHygiene::snapshot(&store);
        hygiene.protect_wrappers(WRAPPERS);
        assert_eq!(hygiene.mark_owned(["fresh:1", "mine-before:1"], WRAPPERS), 1, "only the absent reference is owned");

        for reference in ["fresh:1", "mine-before:1"] {
            hygiene.acquire(reference);
            store.pull(reference, if reference == "fresh:1" { "sha256:new" } else { "sha256:old" });
            hygiene.release(reference);
        }
        assert!(!store.has("fresh:1"));
        assert!(store.has("mine-before:1"));
        assert_eq!(*store.removals.lock().unwrap(), vec!["fresh:1".to_string()]);

        let report = hygiene.finish();
        assert_eq!(report["removed"], json!([{ "reference": "fresh:1", "image_id": "sha256:new" }]));
        assert_eq!(report["removal_failures"], json!([]));
    }

    /// A new name for content that already existed is left in place, not untagged.
    #[test]
    fn an_owned_reference_resolving_to_preexisting_content_is_kept() {
        let store = FakeStore::with(&[("foo:latest", "sha256:same")]);
        let hygiene = ImageHygiene::snapshot(&store);
        hygiene.mark_owned(["foo:1.0"], WRAPPERS);
        hygiene.acquire("foo:1.0");
        store.pull("foo:1.0", "sha256:same");
        hygiene.release("foo:1.0");
        assert!(store.removals.lock().unwrap().is_empty(), "never even attempted");
        let report = hygiene.finish();
        assert_eq!(report["kept_preexisting_content"][0]["reference"], "foo:1.0");
    }

    /// A wrapper image pulled by the sweep's own setup is protected even under an alias.
    #[test]
    fn wrappers_are_never_owned_or_removed() {
        let store = FakeStore::default();
        let mut hygiene = ImageHygiene::snapshot(&store);
        store.pull("node:22-alpine", "sha256:node"); // the pre-pull
        hygiene.protect_wrappers(WRAPPERS);
        // An OCI candidate naming the wrapper verbatim is not owned; one aliasing its content is
        // owned by reference but kept by ID.
        assert_eq!(hygiene.mark_owned(["node:22-alpine", "docker.io/library/node:22-alpine"], WRAPPERS), 1);
        hygiene.acquire("docker.io/library/node:22-alpine");
        store.pull("docker.io/library/node:22-alpine", "sha256:node");
        hygiene.release("docker.io/library/node:22-alpine");
        assert!(store.removals.lock().unwrap().is_empty());
        let report = hygiene.finish();
        assert_eq!(report["wrapper_images_pulled_and_kept"], json!(["node:22-alpine"]));
    }

    /// With two attempts on one reference in flight, the first to finish must not remove the
    /// image out from under the second.
    #[test]
    fn removal_waits_for_the_last_in_flight_attempt_on_a_reference() {
        let store = FakeStore::default();
        let hygiene = ImageHygiene::snapshot(&store);
        hygiene.mark_owned(["shared:1"], WRAPPERS);
        hygiene.acquire("shared:1");
        hygiene.acquire("shared:1");
        store.pull("shared:1", "sha256:s");
        hygiene.release("shared:1");
        assert!(store.has("shared:1"), "still in use by the second attempt");
        hygiene.release("shared:1");
        assert!(!store.has("shared:1"));
    }

    /// A failed removal is reported, does not abort anything, and is retried at sweep end.
    #[test]
    fn a_removal_failure_is_non_fatal_reported_and_retried_at_the_end() {
        let store = FakeStore::default();
        let hygiene = ImageHygiene::snapshot(&store);
        hygiene.mark_owned(["busy:1", "stuck:1"], WRAPPERS);
        store.fail_removal.lock().unwrap().extend(["busy:1".to_string(), "stuck:1".to_string()]);
        for r in ["busy:1", "stuck:1"] {
            hygiene.acquire(r);
            store.pull(r, &format!("sha256:{r}"));
            hygiene.release(r);
        }
        assert!(store.has("busy:1") && store.has("stuck:1"));

        store.fail_removal.lock().unwrap().remove("busy:1"); // the conflicting container is gone now
        let report = hygiene.finish();
        assert!(!store.has("busy:1"), "retried and removed at sweep end");
        assert_eq!(report["removed"][0]["reference"], "busy:1");
        assert_eq!(report["removal_failures"][0]["reference"], "stuck:1");
        assert!(report["removal_failures"][0]["reason"].as_str().unwrap().contains("being used"));
    }

    /// An image that lands after its attempt's removal ran (a daemon-side pull outliving a
    /// watchdog-killed CLI) is caught by the final pass.
    #[test]
    fn a_late_pull_is_caught_by_the_final_pass() {
        let store = FakeStore::default();
        let hygiene = ImageHygiene::snapshot(&store);
        hygiene.mark_owned(["late:1"], WRAPPERS);
        hygiene.acquire("late:1");
        hygiene.release("late:1"); // nothing there yet
        store.pull("late:1", "sha256:late");
        let report = hygiene.finish();
        assert!(!store.has("late:1"));
        assert_eq!(report["removed"][0]["reference"], "late:1");
    }

    /// Without a pre-sweep snapshot nothing can be proven pulled, so nothing is removed.
    #[test]
    fn a_failed_snapshot_disables_removal_entirely() {
        let store = FakeStore::default();
        store.fail_list.store(true, Ordering::SeqCst);
        let hygiene = ImageHygiene::snapshot(&store);
        assert!(!hygiene.enabled());
        assert_eq!(hygiene.mark_owned(["x:1"], WRAPPERS), 0);
        hygiene.acquire("x:1");
        store.pull("x:1", "sha256:x");
        hygiene.release("x:1");
        assert!(store.has("x:1"));
        let report = hygiene.finish();
        assert_eq!(report["removal_enabled"], false);
        assert_eq!(report["snapshot_error"], "daemon unreachable");
    }

    /// Images that appeared but are not ours are reported, never removed.
    #[test]
    fn unattributed_new_images_are_reported_not_removed() {
        let store = FakeStore::default();
        let hygiene = ImageHygiene::snapshot(&store);
        store.pull("operator-pulled:1", "sha256:op");
        let report = hygiene.finish();
        assert!(store.has("operator-pulled:1"));
        assert_eq!(report["unattributed_new_image_ids"]["count"], 1);
    }

    fn facts(os: &str, root: &str, wsl: bool) -> DockerHostFacts {
        DockerHostFacts {
            env_override: None,
            root_dir: root.into(),
            operating_system: os.into(),
            is_wsl: wsl,
            home: Some(PathBuf::from("/home/u")),
        }
    }

    #[test]
    fn docker_data_path_resolution() {
        let all = |_: &Path| true;
        let none = |_: &Path| false;
        assert_eq!(
            resolve_docker_data_path(&facts("Docker Desktop", "/var/lib/docker", true), all),
            Ok(PathBuf::from("/mnt/c"))
        );
        assert_eq!(
            resolve_docker_data_path(&facts("Docker Desktop", "/var/lib/docker", false), all),
            Ok(PathBuf::from("/home/u/Library/Containers/com.docker.docker"))
        );
        assert_eq!(
            resolve_docker_data_path(&facts("Ubuntu 24.04 LTS", "/var/lib/docker", false), all),
            Ok(PathBuf::from("/var/lib/docker"))
        );
        // Not locatable: an error naming the override, never a guess.
        let err = resolve_docker_data_path(&facts("Ubuntu 24.04 LTS", "/var/lib/docker", false), none).unwrap_err();
        assert!(err.contains(DOCKER_DATA_PATH_ENV), "{err}");
        assert!(resolve_docker_data_path(&facts("Docker Desktop", "/var/lib/docker", true), none).is_err());

        let mut overridden = facts("Docker Desktop", "", true);
        overridden.env_override = Some("/data/docker".into());
        assert_eq!(resolve_docker_data_path(&overridden, all), Ok(PathBuf::from("/data/docker")));
        assert!(resolve_docker_data_path(&overridden, none).is_err(), "a bad override is an error, not ignored");
    }
}
