//! Sharing agents between peers (docs/agent-factory.md, section 13; ADR-0007): an owner
//! publishes chosen releases to chosen peers, who see them in their catalog and may import
//! them.
//!
//! - **Grants** live in the allowlist: a peer's `agents` (`peers allow ... --agents`), set
//!   too by `factory publish --to`. An agent's **publication** (in its state) says which of
//!   its released versions the peers granted it see.
//! - **Catalog entries** (name, version, description, most access and an eval summary) go to
//!   each granted peer in the hello and whenever they change
//!   ([`crate::protocol::Message::Catalog`]); never knowledge, eval outputs or paths. The
//!   receiver keeps them while it runs, under the publisher's name on its own allowlist and
//!   its key: what a peer says about itself never names it.
//! - **Importing** asks the publisher's agent for the release's bundle over iroh (at most
//!   [`MAX_BUNDLE_BYTES`]): its files, and its report without its runs' outputs (each run's
//!   signed evidence stays: its checks' results, redacted and capped). The manifest's signer
//!   must be the publisher's key and every hash must match; nothing runs. The bundle is kept
//!   under `factory/imported/<publisher>/<name>/<version>/`.
//! - An import becomes **active** only through a local release of exactly its build, signed
//!   by the importer, which keeps its publisher's version: from an evaluation that passed on
//!   the importer's own accounts, or, when the importer says so, from the publisher's report
//!   (recorded in the agent's history). It asks for capabilities as any first release does:
//!   beyond read-only with nothing, its owner signs them off.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};
use serde::Serialize;

use crate::factory::build::Build;
use crate::factory::release::{self, Release, ReleaseState};
use crate::factory::report::{EvalState, Evaluation};
use crate::factory::store::{
    FactoryStore, ImportOrigin, ImportedVersion, Lifecycle, Publication, now, read_json,
};
use crate::factory::{conflict, invalid, not_found, unprocessable};
use crate::profile::Profile;
use crate::protocol::{Bundle, CatalogItem, EvalSummary};
use crate::trust::{TrustStore, TrustedPeer};

/// The largest bundle sent or taken: its files, before base64.
pub const MAX_BUNDLE_BYTES: usize = 8 * 1024 * 1024;
/// The most entries a publisher's catalog may have, and the longest description.
pub const MAX_ENTRIES: usize = 100;
pub const MAX_DESCRIPTION: usize = 200;
/// Where imported bundles are kept in the factory's store.
pub const IMPORTED_DIR: &str = "imported";

/// What an owner's agent publishes: the catalog each peer gets, and the bundles peers ask
/// for (see [`crate::agent::AgentConfig::publisher`]).
pub trait Publisher: Send + Sync {
    /// The catalog `peer` (on the owner's allowlist) gets.
    fn entries_for(&self, peer: &TrustedPeer) -> Vec<CatalogItem>;
    /// The bundle of release `version` of `agent`, if it is published to `peer`; else why
    /// not, in words for the peer.
    fn bundle_for(&self, peer: &TrustedPeer, agent: &str, version: &str) -> Result<Bundle, String>;
}

/// A profile's factory, as its agent publishes it.
pub struct ProfilePublisher {
    store: FactoryStore,
    /// The profile's name, as peers' answers name it.
    owner: String,
    /// Releases are immutable: each one's entry, by its manifest's hash.
    items: Mutex<HashMap<String, CatalogItem>>,
}

impl ProfilePublisher {
    pub fn new(profile: Profile) -> Arc<Self> {
        Self::of_store(FactoryStore::of(&profile), &profile.name)
    }

    /// The factory in `store`, of profile `owner`.
    pub fn of_store(store: FactoryStore, owner: &str) -> Arc<Self> {
        Arc::new(Self {
            store,
            owner: owner.to_owned(),
            items: Mutex::new(HashMap::new()),
        })
    }
}

impl Publisher for ProfilePublisher {
    fn entries_for(&self, peer: &TrustedPeer) -> Vec<CatalogItem> {
        let store = &self.store;
        let mut items = self.items.lock().unwrap();
        published(store, peer)
            .into_iter()
            .filter_map(|(name, version)| {
                // The signed manifest only, so a change of any of its files shows at once.
                let path = store
                    .release_dir(&name, &version)
                    .join(release::RELEASE_FILE);
                let r: Release = read_json(&path).ok().flatten()?;
                if r.state != ReleaseState::Released || r.verify_signature().is_err() {
                    return None;
                }
                if let Some(item) = items.get(&r.manifest_sha256) {
                    return Some(item.clone());
                }
                let item = item_of(store, &r)?;
                items.insert(r.manifest_sha256.clone(), item.clone());
                Some(item)
            })
            .take(MAX_ENTRIES)
            .collect()
    }

    fn bundle_for(&self, peer: &TrustedPeer, agent: &str, version: &str) -> Result<Bundle, String> {
        bundle(&self.store, &self.owner, peer, agent, version)
    }
}

/// The releases of `store` published to `peer`: (agent, version) of every agent it is
/// granted that isn't retired, each version its publication names.
fn published(store: &FactoryStore, peer: &TrustedPeer) -> Vec<(String, String)> {
    let mut found = Vec::new();
    for name in &peer.agents {
        let Some(state) = store.state(name).ok().flatten() else {
            continue;
        };
        if matches!(state.state, Lifecycle::Retired | Lifecycle::Imported) {
            continue;
        }
        for version in state.publication.iter().flat_map(|p| &p.versions) {
            found.push((name.clone(), version.clone()));
        }
    }
    found
}

/// Release `r`'s catalog entry: from its manifest and the report it was released from.
fn item_of(store: &FactoryStore, r: &Release) -> Option<CatalogItem> {
    let m = &r.manifest;
    let report = release::report_of(store, &m.agent, &m.version).ok()?;
    Some(CatalogItem {
        name: m.agent.clone(),
        version: m.version.clone(),
        description: one_line(&m.purpose, MAX_DESCRIPTION),
        access: m.capabilities.access,
        eval_summary: summary_of(&report),
    })
}

/// How evaluation `e` went, in a catalog entry.
pub fn summary_of(e: &Evaluation) -> EvalSummary {
    let chosen = e
        .recommended
        .as_ref()
        .and_then(|r| e.results.iter().find(|c| &c.candidate == r))
        .or_else(|| e.results.first());
    EvalSummary {
        pass_rate: chosen.map(|c| c.pass_rate).unwrap_or(0.0),
        cases: e.cases.iter().filter(|c| c.attack.is_none()).count(),
        adversarial_passed: chosen.is_some_and(|c| c.adversarial.passed == c.adversarial.total),
    }
}

/// `text` on one line, without control characters, at most `max` characters.
fn one_line(text: &str, max: usize) -> String {
    let clean: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let words = clean.split_whitespace().collect::<Vec<_>>().join(" ");
    words.chars().take(max).collect()
}

/// What a peer said it publishes, as this agent keeps it: valid names and versions only, a
/// rate between 0 and 1, a short description on one line, at most [`MAX_ENTRIES`].
pub fn clean(items: Vec<CatalogItem>) -> Vec<CatalogItem> {
    let mut kept: Vec<CatalogItem> = Vec::new();
    for mut item in items {
        let fine = crate::profile::name_ok(&item.name)
            && item.version.parse::<release::Version>().is_ok()
            && item.eval_summary.pass_rate.is_finite()
            && (0.0..=1.0).contains(&item.eval_summary.pass_rate)
            && item.eval_summary.cases <= 10_000;
        if !fine
            || kept
                .iter()
                .any(|k| k.name == item.name && k.version == item.version)
        {
            continue;
        }
        item.description = one_line(&item.description, MAX_DESCRIPTION);
        kept.push(item);
        if kept.len() == MAX_ENTRIES {
            break;
        }
    }
    kept
}

/// An entry of this profile's catalog: what `publisher` (its key `publisher_id`) publishes.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CatalogEntry {
    pub publisher: String,
    pub publisher_id: String,
    #[serde(flatten)]
    pub item: CatalogItem,
}

/// Agent `name`'s publication as the contract has it: the peers granted it (by their names
/// on `trust`) and the versions they see.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct Shared {
    pub peers: Vec<String>,
    pub versions: Vec<String>,
}

/// Agent `name`'s publication now.
pub fn publication(store: &FactoryStore, trust: &TrustStore, name: &str) -> Result<Shared> {
    let state = store
        .state(name)?
        .ok_or_else(|| not_found(format!("the factory has no agent '{name}'")))?;
    Ok(Shared {
        peers: trust
            .peers
            .iter()
            .filter(|p| p.agents.iter().any(|a| a == name))
            .map(|p| p.name.clone())
            .collect(),
        versions: state.publication.map(|p| p.versions).unwrap_or_default(),
    })
}

/// Publishes `versions` (released versions of agent `name`; none: it is unpublished) to
/// `peers` (names on `trust`), by `by`: the grants in `trust` (the caller saves it, or gives
/// it to its running agent), and the publication in the agent's state, in its history.
/// Peers on `trust` not in `peers` lose the grant. Returns the publication and the peers
/// whose grant changed.
pub fn publish(
    store: &FactoryStore,
    trust: &mut TrustStore,
    name: &str,
    peers: &[String],
    versions: &[String],
    by: &str,
) -> Result<(Shared, Vec<TrustedPeer>)> {
    let state = store
        .state(name)?
        .ok_or_else(|| not_found(format!("the factory has no agent '{name}'")))?;
    match state.state {
        Lifecycle::Retired => {
            return Err(invalid(format!(
                "{name} is retired: a retired agent isn't published"
            )));
        }
        Lifecycle::Imported => {
            return Err(invalid(format!(
                "{name} is an import: only its publisher publishes it"
            )));
        }
        _ => {}
    }
    if state.imported.is_some() {
        return Err(invalid(format!(
            "{name} is an import: only its publisher publishes it"
        )));
    }
    for peer in peers {
        if trust.get_by_name(peer).is_none() {
            return Err(invalid(format!("'{peer}' isn't on your allowlist")));
        }
    }
    let mut kept: Vec<String> = Vec::new();
    for version in versions {
        match release::load(store, name, version) {
            Ok(r) if r.state == ReleaseState::Released => {
                if !kept.contains(version) {
                    kept.push(version.clone());
                }
            }
            Ok(r) => {
                return Err(invalid(format!(
                    "{name} {version} is {}: only released versions are published",
                    r.state
                )));
            }
            Err(e) => return Err(invalid(format!("{e:#}"))),
        }
    }
    release::sort_versions(&mut kept);
    let mut changed = Vec::new();
    for entry in &mut trust.peers {
        let granted = peers.contains(&entry.name);
        let has = entry.agents.iter().any(|a| a == name);
        if granted != has {
            if granted {
                entry.agents.push(name.to_owned());
                entry.agents.sort();
            } else {
                entry.agents.retain(|a| a != name);
            }
            changed.push(entry.clone());
        }
    }
    let to: Vec<&String> = peers.iter().collect();
    store.update_state(name, |s| {
        let before = s
            .publication
            .clone()
            .map(|p| p.versions)
            .unwrap_or_default();
        s.publication = (!kept.is_empty()).then(|| Publication {
            versions: kept.clone(),
            at: now(),
        });
        if before != kept || !changed.is_empty() {
            let what = if kept.is_empty() {
                "unpublished".to_owned()
            } else if to.is_empty() {
                format!("published {} to no peer", kept.join(", "))
            } else {
                format!(
                    "published {} to {}",
                    kept.join(", "),
                    to.iter().map(|p| p.as_str()).collect::<Vec<_>>().join(", ")
                )
            };
            s.changed(by, what, None);
        }
        Ok(())
    })?;
    Ok((
        Shared {
            peers: trust
                .peers
                .iter()
                .filter(|p| p.agents.iter().any(|a| a == name))
                .map(|p| p.name.clone())
                .collect(),
            versions: kept,
        },
        changed,
    ))
}

/// The files of release `version` of `agent` in `store` as its bundle has them: every file
/// of its folder, its report without its runs' outputs.
pub fn bundle_files(
    store: &FactoryStore,
    agent: &str,
    version: &str,
) -> Result<BTreeMap<String, Vec<u8>>> {
    release::load(store, agent, version)?;
    let dir = store.release_dir(agent, version);
    let mut files = BTreeMap::new();
    read_bytes(&dir, "", &mut files)?;
    let mut report = release::report_of(store, agent, version)?;
    for run in &mut report.runs {
        run.output = None;
    }
    files.insert(
        release::REPORT_FILE.into(),
        (serde_json::to_string_pretty(&report)? + "\n").into_bytes(),
    );
    Ok(files)
}

fn read_bytes(dir: &Path, prefix: &str, files: &mut BTreeMap<String, Vec<u8>>) -> Result<()> {
    for entry in crate::factory::store::read_dir(dir)? {
        let name = entry.file_name().to_string_lossy().into_owned();
        let relative = format!("{prefix}{name}");
        if entry.file_type()?.is_dir() {
            read_bytes(&entry.path(), &format!("{relative}/"), files)?;
        } else {
            files.insert(
                relative,
                std::fs::read(entry.path())
                    .with_context(|| format!("reading {}", entry.path().display()))?,
            );
        }
    }
    Ok(())
}

/// The bundle of release `version` of `agent` in `store` (profile `owner`'s), for `peer`, if
/// it is published to the peer, released, verifies and isn't too big; else why not, for the
/// peer. Sending it is recorded in the agent's history.
pub fn bundle(
    store: &FactoryStore,
    owner: &str,
    peer: &TrustedPeer,
    agent: &str,
    version: &str,
) -> Result<Bundle, String> {
    let refused = || format!("{owner} publishes no {agent} {version} to you");
    if !published(store, peer)
        .iter()
        .any(|(a, v)| a == agent && v == version)
    {
        return Err(refused());
    }
    let files = bundle_files(store, agent, version).map_err(|e| {
        format!(
            "{agent} {version} can't be sent: it doesn't verify on its publisher's side ({e:#})"
        )
    })?;
    let size: usize = files.values().map(Vec::len).sum();
    if size > MAX_BUNDLE_BYTES {
        return Err(format!(
            "{agent} {version} is too big to send: {:.1} MB of files, at most {} MB",
            size as f64 / 1_048_576.0,
            MAX_BUNDLE_BYTES / 1_048_576
        ));
    }
    let _ = store.update_state(agent, |s| {
        s.changed(
            owner,
            format!("sent {version} to {} (an import)", peer.name),
            None,
        );
        Ok(())
    });
    Ok(Bundle {
        agent: agent.to_owned(),
        version: version.to_owned(),
        files: files
            .into_iter()
            .map(|(path, bytes)| (path, data_encoding::BASE64.encode(&bytes)))
            .collect(),
    })
}

/// Who an import is from, and how it becomes active.
#[derive(Debug, Clone)]
pub struct ImportAsk {
    /// The publisher, as this profile's allowlist names it, and its key.
    pub publisher: String,
    pub publisher_id: String,
    /// Activate it from the publisher's report instead of an evaluation here.
    pub accept_publisher_report: bool,
    pub by: String,
}

/// Where an import is: kept, its evaluation running, or active (a local release of it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ImportState {
    Imported,
    Evaluating,
    Active,
}

/// An import, as the contract has it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Imported {
    pub name: String,
    pub version: String,
    pub publisher_id: String,
    pub verified: bool,
    pub state: ImportState,
    /// Its local release, once one was made (it may wait for its owner's sign-off).
    #[serde(skip)]
    pub release: Option<Release>,
}

/// Where publisher `publisher`'s agent `name` (its version `version`) is kept.
pub fn imported_dir(store: &FactoryStore, publisher: &str, name: &str, version: &str) -> PathBuf {
    store
        .root()
        .join(IMPORTED_DIR)
        .join(publisher)
        .join(name)
        .join(version)
}

/// A path of a bundle's file: relative, `/`-separated, without `.` or `..`, in its folder.
fn bundle_path_ok(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 300
        && !path.starts_with('/')
        && !path.contains('\\')
        && !path.contains(':')
        && path.split('/').all(|part| {
            !part.is_empty() && part != "." && part != ".." && !part.chars().any(|c| c.is_control())
        })
        && (path == release::RELEASE_FILE
            || path == release::REPORT_FILE
            || path.starts_with(&format!("{}/", release::FILES_DIR))
            || path.starts_with(&format!("{}/", release::FIXTURES_DIR)))
}

/// Takes in `bundle`, the release `ask.publisher` sent: checks its paths and size, writes
/// it under `factory/imported/` and verifies it there (the manifest signed by the
/// publisher's key, every file's hash, the report the publisher signed), or refuses it (a
/// changed file, a manifest re-signed by another key, a missing artifact). Nothing runs. Its
/// build is kept, and its fixtures, for its evaluation here; with
/// `ask.accept_publisher_report`, the publisher's report makes it active (a local release of
/// its build, signed with `key`, recorded as accepted). `owner` is this profile's name.
pub fn import(
    store: &FactoryStore,
    owner: &str,
    key: &iroh::SecretKey,
    bundle: Bundle,
    ask: &ImportAsk,
) -> Result<Imported> {
    let (name, version) = (bundle.agent.clone(), bundle.version.clone());
    let what = format!("{name} {version} from {}", ask.publisher);
    if !crate::profile::name_ok(&name) || version.parse::<release::Version>().is_err() {
        return Err(unprocessable(format!(
            "{what}: its bundle doesn't name an agent and a version"
        )));
    }
    let mut files: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut size = 0;
    for (path, text) in &bundle.files {
        if !bundle_path_ok(path) {
            return Err(unprocessable(format!(
                "{what}: {path:?} isn't a file of a release"
            )));
        }
        let bytes = data_encoding::BASE64
            .decode(text.as_bytes())
            .map_err(|_| unprocessable(format!("{what}: {path} isn't base64")))?;
        size += bytes.len();
        if size > MAX_BUNDLE_BYTES {
            return Err(unprocessable(format!(
                "{what}: its bundle is over {} MB",
                MAX_BUNDLE_BYTES / 1_048_576
            )));
        }
        files.insert(path.clone(), bytes);
    }
    // Its name must be free here, or an import of the same publisher's.
    let state = store.state(&name)?;
    let theirs = state
        .as_ref()
        .and_then(|s| s.imported.as_ref())
        .is_some_and(|o| o.publisher_id == ask.publisher_id);
    if (state.is_some() || store.draft(&name)?.is_some()) && !theirs {
        return Err(invalid(format!(
            "you have an agent '{name}' already: an import keeps its publisher's name"
        )));
    }
    if state
        .as_ref()
        .is_some_and(|s| s.state == Lifecycle::Retired)
    {
        return Err(invalid(format!(
            "you retired {name}: it isn't imported again"
        )));
    }
    let dir = imported_dir(store, &ask.publisher, &name, &version);
    let parent = dir.parent().expect("an import's folder has a parent");
    std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let partial = parent.join(format!(".partial-{}", uuid::Uuid::new_v4().simple()));
    let checked = (|| -> Result<Release> {
        for (path, bytes) in &files {
            let file = partial.join(path);
            if let Some(dir) = file.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&file, bytes)?;
        }
        release::verify_bundle(&partial, &name, &version, &ask.publisher_id)
    })();
    let release = match checked {
        Ok(r) => r,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&partial);
            return Err(unprocessable(format!("{what}: {e:#}")));
        }
    };
    if release.state != ReleaseState::Released {
        let _ = std::fs::remove_dir_all(&partial);
        return Err(unprocessable(format!(
            "{what}: it is {}, not released",
            release.state
        )));
    }
    let known = state
        .as_ref()
        .and_then(|s| s.imported.as_ref())
        .and_then(|o| o.versions.iter().find(|v| v.version == version).cloned());
    match &known {
        Some(v) if v.manifest_sha256 != release.manifest_sha256 => {
            let _ = std::fs::remove_dir_all(&partial);
            return Err(unprocessable(format!(
                "{what}: it isn't the {version} you imported from them before (another manifest)"
            )));
        }
        Some(_) => {
            let _ = std::fs::remove_dir_all(&partial);
        }
        None => {
            if dir.exists() {
                std::fs::remove_dir_all(&dir)
                    .with_context(|| format!("replacing {}", dir.display()))?;
            }
            crate::factory::store::rename_dir(&partial, &dir)?;
        }
    }
    // Its build and fixtures, for its evaluation here.
    let mut built = BTreeMap::new();
    crate::factory::store::read_tree(&dir.join(release::FILES_DIR), "", &mut built)?;
    let build = Build::of(&name, built);
    store.save_build(&build)?;
    let fixtures = store.fixtures_dir(&name);
    for (path, bytes) in files
        .iter()
        .filter_map(|(p, b)| Some((p.strip_prefix(&format!("{}/", release::FIXTURES_DIR))?, b)))
    {
        let to = fixtures.join(path);
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&to, bytes).with_context(|| format!("writing {}", to.display()))?;
    }
    let m = &release.manifest;
    if known.is_none() {
        let entry = ImportedVersion {
            version: version.clone(),
            manifest_sha256: release.manifest_sha256.clone(),
            build: build.hash.clone(),
            at: now(),
            accepted_report: false,
        };
        store.update_state(&name, |s| {
            if s.imported.is_none() && s.history.is_empty() {
                s.state = Lifecycle::Imported;
            }
            let origin = s.imported.get_or_insert_with(|| ImportOrigin {
                publisher: ask.publisher.clone(),
                publisher_id: ask.publisher_id.clone(),
                versions: Vec::new(),
            });
            origin.publisher = ask.publisher.clone();
            origin.versions.insert(0, entry.clone());
            s.changed(
                &ask.by,
                format!(
                    "imported {version} from {} (key {}, manifest {}): its signature and every hash verified; nothing ran",
                    ask.publisher,
                    &ask.publisher_id[..10.min(ask.publisher_id.len())],
                    &release.manifest_sha256[..12]
                ),
                None,
            );
            Ok(())
        })?;
    }
    let mut local = store
        .state(&name)?
        .filter(|s| s.versions.contains(&version))
        .and_then(|_| release::load(store, &name, &version).ok());
    if ask.accept_publisher_report && local.is_none() {
        let mut report = release::report_of_dir(&dir)?;
        for run in &mut report.runs {
            run.output = None;
        }
        if report.id != m.evaluation || report.state != EvalState::Passed {
            return Err(unprocessable(format!("{what}: its report didn't pass")));
        }
        if store.evaluation(&report.id)?.is_none() {
            store.save_evaluation(&report)?;
        }
        store.update_state(&name, |s| {
            if let Some(v) = s
                .imported
                .as_mut()
                .and_then(|o| o.versions.iter_mut().find(|v| v.version == version))
            {
                v.accepted_report = true;
            }
            s.changed(
                &ask.by,
                format!(
                    "accepted {}'s report {} for {version} instead of an evaluation here",
                    ask.publisher, report.id
                ),
                None,
            );
            Ok(())
        })?;
        local = Some(release::request(
            store,
            owner,
            key,
            &name,
            &release::Asked {
                evaluation: report.id.clone(),
                version: Some(version.clone()),
                note: Some(format!(
                    "Imported from {}: its report accepted",
                    ask.publisher
                )),
                by: ask.by.clone(),
                ..release::Asked::default()
            },
        )?);
    }
    Ok(Imported {
        state: import_state(store, &name, &version),
        name,
        version,
        publisher_id: ask.publisher_id.clone(),
        verified: true,
        release: local,
    })
}

/// Where version `version` of imported agent `name` is.
pub fn import_state(store: &FactoryStore, name: &str, version: &str) -> ImportState {
    let state = store.state(name).ok().flatten();
    let released = state
        .as_ref()
        .is_some_and(|s| s.versions.iter().any(|v| v == version))
        && release::load(store, name, version).is_ok_and(|r| r.state == ReleaseState::Released);
    if released {
        return ImportState::Active;
    }
    let build = state
        .as_ref()
        .and_then(|s| s.imported.as_ref())
        .and_then(|o| o.versions.iter().find(|v| v.version == version))
        .map(|v| v.build.clone());
    let evaluating = store.evaluation_running()
        && build.is_some_and(|b| {
            store
                .evaluations(name)
                .unwrap_or_default()
                .iter()
                .any(|e| e.state == EvalState::Running && e.build == b)
        });
    if evaluating {
        ImportState::Evaluating
    } else {
        ImportState::Imported
    }
}

/// The version of imported agent `name` an evaluation would evaluate: the newest one not
/// released here yet, with its blueprint and build (from its bundle).
pub fn pending(store: &FactoryStore, name: &str) -> Result<Option<(String, Build)>> {
    let Some(state) = store.state(name)? else {
        return Ok(None);
    };
    let Some(origin) = &state.imported else {
        return Ok(None);
    };
    let Some(v) = origin
        .versions
        .iter()
        .find(|v| !state.versions.contains(&v.version))
    else {
        return Ok(None);
    };
    let build = store.build(name, &v.build)?.ok_or_else(|| {
        conflict(format!(
            "the build of {name} {} isn't kept: import it again",
            v.version
        ))
    })?;
    Ok(Some((v.version.clone(), build)))
}

/// After evaluation `e` ended: if it passed for exactly the build of a version of an
/// imported agent not released here yet, that version becomes active through a local
/// release (signed with `key`; it may wait for its owner's sign-off). Returns it.
pub fn after_evaluation(
    store: &FactoryStore,
    owner: &str,
    key: &iroh::SecretKey,
    e: &Evaluation,
) -> Result<Option<Release>> {
    if e.state != EvalState::Passed || e.canary.is_some() {
        return Ok(None);
    }
    let Some(state) = store.state(&e.agent)? else {
        return Ok(None);
    };
    let Some(v) = state
        .imported
        .as_ref()
        .and_then(|o| o.versions.iter().find(|v| v.build == e.build))
        .filter(|v| !state.versions.contains(&v.version))
        .cloned()
    else {
        return Ok(None);
    };
    let publisher = state
        .imported
        .as_ref()
        .map(|o| o.publisher.clone())
        .unwrap_or_default();
    release::request(
        store,
        owner,
        key,
        &e.agent,
        &release::Asked {
            evaluation: e.id.clone(),
            version: Some(v.version.clone()),
            note: Some(format!(
                "Imported from {publisher}: evaluated here ({})",
                e.id
            )),
            by: owner.to_owned(),
            ..release::Asked::default()
        },
    )
    .map(Some)
}

/// The catalog `entries` of `published` as a profile lists them, each with how it stands
/// here: imported (`imported`, `evaluating`, `active`) or not.
pub fn listed(
    store: &FactoryStore,
    published: &[(iroh::EndpointId, crate::agent::Published)],
) -> Vec<(CatalogEntry, Option<ImportState>)> {
    let mut entries = Vec::new();
    for (id, p) in published {
        for item in &p.entries {
            let state = store
                .state(&item.name)
                .ok()
                .flatten()
                .filter(|s| {
                    s.imported
                        .as_ref()
                        .is_some_and(|o| o.publisher_id == id.to_string())
                })
                .and_then(|s| {
                    s.imported
                        .as_ref()?
                        .versions
                        .iter()
                        .any(|v| v.version == item.version)
                        .then(|| import_state(store, &item.name, &item.version))
                });
            entries.push((
                CatalogEntry {
                    publisher: p.name.clone(),
                    publisher_id: id.to_string(),
                    item: item.clone(),
                },
                state,
            ));
        }
    }
    entries
}

/// Parses an import's target: `publisher/name@version`.
pub fn parse_target(text: &str) -> Result<(String, String, String)> {
    let (publisher, rest) = text.split_once('/').ok_or_else(|| {
        anyhow!("{text:?} isn't publisher/name@version, like alice/fin-analyst@1.2.0")
    })?;
    let (name, version) = rest
        .split_once('@')
        .ok_or_else(|| anyhow!("{text:?} names no version: publisher/name@version"))?;
    if !crate::profile::name_ok(publisher) || !crate::profile::name_ok(name) {
        return Err(anyhow!("{text:?} isn't publisher/name@version"));
    }
    version
        .parse::<release::Version>()
        .map_err(|e| anyhow!("{text:?}: {e:#}"))?;
    Ok((publisher.to_owned(), name.to_owned(), version.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evidence::{Evidence, EvidenceKind};
    use crate::factory::RefusalKind;
    use crate::factory::release::tests::Fixture;
    use crate::factory::report::RunState;
    use iroh::SecretKey;
    use serde_json::{Value, json};

    fn peer(name: &str) -> TrustedPeer {
        TrustedPeer {
            name: name.into(),
            id: SecretKey::generate().public(),
            addrs: Default::default(),
            can_delegate: false,
            tasks_per_hour: 0,
            access: crate::llm::Access::ReadOnly,
            shares: Vec::new(),
            m365: None,
            github: None,
            agents: Vec::new(),
        }
    }

    /// alice's factory, fin-analyst 1.0.0 released in it, and her allowlist: bob and dana.
    fn released() -> (Fixture, TrustStore) {
        let f = Fixture::new();
        std::fs::create_dir_all(f.store.fixtures_dir("fin-analyst")).unwrap();
        std::fs::write(
            f.store.fixtures_dir("fin-analyst").join("q3.pdf"),
            "%PDF q3",
        )
        .unwrap();
        let e = f.evaluated(|v: &mut Value| v["approvers"] = json!(["me"]), false);
        f.release(&e.id, None).unwrap();
        f.approve("1.0.0");
        let mut trust = TrustStore::default();
        trust.allow(peer("bob"));
        trust.allow(peer("dana"));
        (f, trust)
    }

    fn refusal(e: anyhow::Error) -> (Option<RefusalKind>, String) {
        (crate::factory::refusal_kind(&e), format!("{e:#}"))
    }

    /// AC-26 and TM-7: a peer sees only the releases published to it: name, version,
    /// description, most access and an eval summary, never knowledge, outputs or paths.
    #[test]
    fn a_peer_sees_only_what_is_published_to_it_never_knowledge_outputs_or_paths() {
        let (f, mut trust) = released();
        let publisher = ProfilePublisher::of_store(f.store.clone(), "alice");
        let bob = |t: &TrustStore| t.get_by_name("bob").unwrap().clone();
        assert!(
            publisher.entries_for(&bob(&trust)).is_empty(),
            "nothing published yet"
        );
        let (kind, why) = refusal(
            publish(
                &f.store,
                &mut trust,
                "fin-analyst",
                &["mallory".into()],
                &["1.0.0".into()],
                "alice",
            )
            .unwrap_err(),
        );
        assert_eq!(
            (kind, why.as_str()),
            (
                Some(RefusalKind::Invalid),
                "'mallory' isn't on your allowlist"
            )
        );
        let (kind, why) = refusal(
            publish(
                &f.store,
                &mut trust,
                "fin-analyst",
                &["bob".into()],
                &["9.9.9".into()],
                "alice",
            )
            .unwrap_err(),
        );
        assert_eq!(kind, Some(RefusalKind::Invalid));
        assert!(why.contains("has no release 9.9.9"), "{why}");

        let (shared, changed) = publish(
            &f.store,
            &mut trust,
            "fin-analyst",
            &["bob".into()],
            &["1.0.0".into()],
            "alice",
        )
        .unwrap();
        assert_eq!(
            shared,
            Shared {
                peers: vec!["bob".into()],
                versions: vec!["1.0.0".into()]
            }
        );
        assert_eq!(
            changed.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
            ["bob"]
        );
        assert_eq!(
            bob(&trust).agents,
            ["fin-analyst"],
            "the grant is in the allowlist"
        );
        let entries = publisher.entries_for(&bob(&trust));
        assert_eq!(entries.len(), 1);
        let item = &entries[0];
        assert_eq!(
            (item.name.as_str(), item.version.as_str()),
            ("fin-analyst", "1.0.0")
        );
        assert_eq!(item.access, crate::llm::Access::ReadOnly);
        assert!(item.description.starts_with("Reads quarterly reports"));
        assert_eq!(item.eval_summary.pass_rate, 1.0);
        assert!(item.eval_summary.adversarial_passed);
        assert_eq!(item.eval_summary.cases, 2, "its own cases, not the attacks");
        let wire = serde_json::to_string(&entries).unwrap();
        let root = f.store.root().display().to_string();
        for never in [
            "finance-glossary",
            "fin-analyst-lessons",
            "ok, run",
            "q3.pdf",
            "instructions",
            &root,
        ] {
            assert!(!wire.contains(never), "{never:?} in {wire}");
        }
        assert!(
            publisher
                .entries_for(trust.get_by_name("dana").unwrap())
                .is_empty(),
            "dana isn't granted it"
        );
        // A retired agent is offered to no one.
        f.store
            .update_state("fin-analyst", |s| {
                s.state = Lifecycle::Retired;
                Ok(())
            })
            .unwrap();
        assert!(
            ProfilePublisher::of_store(f.store.clone(), "alice")
                .entries_for(&bob(&trust))
                .is_empty()
        );
        // A peer's catalog is kept only as it may be: valid names and versions, one line.
        let mut said = entries.clone();
        said.push(CatalogItem {
            name: "../evil".into(),
            ..entries[0].clone()
        });
        said[0].description = "Reads\nreports\u{7}".into();
        let kept = clean(said);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].description, "Reads reports");
    }

    /// TM-2: a bundle is taken whole or not at all: a changed file, a manifest re-signed by
    /// another key, a missing or added artifact, a path outside its folder are refused, and
    /// nothing of them is kept. One that verifies is kept; nothing runs.
    #[test]
    fn a_bundle_is_taken_only_whole_and_signed_by_its_publisher_and_nothing_runs() {
        let (f, mut trust) = released();
        publish(
            &f.store,
            &mut trust,
            "fin-analyst",
            &["bob".into()],
            &["1.0.0".into()],
            "alice",
        )
        .unwrap();
        let bob = trust.get_by_name("bob").unwrap().clone();
        let dana = trust.get_by_name("dana").unwrap().clone();
        assert_eq!(
            bundle(&f.store, "alice", &dana, "fin-analyst", "1.0.0").unwrap_err(),
            "alice publishes no fin-analyst 1.0.0 to you"
        );
        let sent = bundle(&f.store, "alice", &bob, "fin-analyst", "1.0.0").unwrap();
        let file = |b: &Bundle, path: &str| {
            String::from_utf8(
                data_encoding::BASE64
                    .decode(b.files[path].as_bytes())
                    .unwrap(),
            )
            .unwrap()
        };
        assert!(
            !file(&sent, "report.json").contains("ok, run"),
            "never its outputs"
        );
        assert!(sent.files.contains_key("fixtures/q3.pdf"));

        let here = tempfile::tempdir().unwrap();
        let store = FactoryStore::new(here.path().join("factory"));
        let key = SecretKey::generate();
        let ask = ImportAsk {
            publisher: "alice".into(),
            publisher_id: f.owner.public().to_string(),
            accept_publisher_report: false,
            by: "bob".into(),
        };
        let refused = |b: Bundle| refusal(import(&store, "bob", &key, b, &ask).unwrap_err());
        let encode = |text: &str| data_encoding::BASE64.encode(text.as_bytes());

        let mut changed = sent.clone();
        let agent = file(&sent, "files/agent.md").replace("Quote figures exactly", "Quote nothing");
        changed
            .files
            .insert("files/agent.md".into(), encode(&agent));
        let (kind, why) = refused(changed);
        assert_eq!(kind, Some(RefusalKind::Unprocessable));
        assert!(
            why.contains("files/agent.md doesn't match its hash"),
            "{why}"
        );

        let mut resigned = sent.clone();
        let mallory = SecretKey::generate();
        let mut r: Release = serde_json::from_str(&file(&sent, "release.json")).unwrap();
        r.manifest.signer = "mallory".into();
        r.manifest.signer_id = mallory.public().to_string();
        r.manifest_sha256 = r.manifest.sha256();
        r.signature =
            data_encoding::HEXLOWER.encode(&mallory.sign(&r.manifest.signed_bytes()).to_bytes());
        resigned.files.insert(
            "release.json".into(),
            encode(&serde_json::to_string(&r).unwrap()),
        );
        let (kind, why) = refused(resigned);
        assert_eq!(kind, Some(RefusalKind::Unprocessable));
        assert!(why.contains("signed by mallory's key"), "{why}");

        let mut missing = sent.clone();
        missing.files.remove("files/blueprint.json");
        let (_, why) = refused(missing);
        assert!(why.contains("files/blueprint.json is missing"), "{why}");

        let mut added = sent.clone();
        added
            .files
            .insert("files/skills/extra.md".into(), encode("Do more"));
        let (_, why) = refused(added);
        assert!(
            why.contains("files/skills/extra.md isn't in its manifest"),
            "{why}"
        );

        let mut outside = sent.clone();
        outside
            .files
            .insert("files/../../evil.md".into(), encode("x"));
        let (_, why) = refused(outside);
        assert!(why.contains("isn't a file of a release"), "{why}");
        assert!(
            store.state("fin-analyst").unwrap().is_none(),
            "nothing of them is kept"
        );

        let imported = import(&store, "bob", &key, sent.clone(), &ask).unwrap();
        assert_eq!(
            (imported.verified, imported.state),
            (true, ImportState::Imported)
        );
        let state = store.state("fin-analyst").unwrap().unwrap();
        assert_eq!(state.state, Lifecycle::Imported);
        assert_eq!(state.active_version, None, "it isn't active");
        assert!(state.deployments.is_empty(), "nothing runs");
        assert!(state.history.last().unwrap().what.contains("nothing ran"));
        let dir = imported_dir(&store, "alice", "fin-analyst", "1.0.0");
        release::verify_bundle(&dir, "fin-analyst", "1.0.0", &ask.publisher_id).unwrap();
        assert!(store.fixtures_dir("fin-analyst").join("q3.pdf").is_file());
        let (version, build) = pending(&store, "fin-analyst").unwrap().unwrap();
        let manifest = release::load(&f.store, "fin-analyst", "1.0.0")
            .unwrap()
            .manifest;
        assert_eq!(
            (version.as_str(), build.hash.as_str()),
            ("1.0.0", manifest.build.as_str())
        );
        // Its name is its publisher's here now.
        let theirs = ImportAsk {
            publisher: "mallory".into(),
            publisher_id: mallory.public().to_string(),
            ..ask.clone()
        };
        let (kind, why) = refusal(import(&store, "bob", &key, sent, &theirs).unwrap_err());
        assert_eq!(kind, Some(RefusalKind::Invalid));
        assert!(
            why.contains("you have an agent 'fin-analyst' already"),
            "{why}"
        );
        // Deleted before it is active, its bundle goes too: nothing of it ran.
        assert!(store.delete("fin-analyst").unwrap());
        assert!(!dir.exists(), "{} is left", dir.display());
    }

    /// `store`'s evaluation of `build` of fin-analyst, every run passing unless `fail`,
    /// signed by `owner` (whose worker is `worker`).
    fn evaluated_here(
        store: &FactoryStore,
        build: &str,
        owner: &SecretKey,
        worker: &SecretKey,
        n: u32,
        fail: bool,
    ) -> Evaluation {
        let mut e = crate::factory::report::tests::evaluation();
        e.id = format!("ev-20261126-1015{n:02}-fin-analyst");
        e.agent = "fin-analyst".into();
        e.build = build.into();
        e.worker_id = worker.public().to_string();
        for (i, run) in e.runs.iter_mut().enumerate() {
            run.state = RunState::Done;
            run.passed = !fail;
            run.secs = 10.0;
            run.attempts = 1;
            run.finished = Some("2026-11-26T10:20:00Z".into());
            let output = format!("ok here, run {i}");
            let mut evidence = Evidence::new(
                EvidenceKind::Task,
                "bob-eval",
                worker.public(),
                owner.public(),
                i as u64,
            );
            evidence.attempts = 1;
            run.task_id = Some(i as u64);
            run.evidence = Some(evidence.sign(&output, worker));
            run.keep_output(&output, &e.seeds);
        }
        e.finish(&[Some(0.33), Some(1.0)], None);
        e.sign("bob", owner);
        store.save_evaluation(&e).unwrap();
        e
    }

    /// AC-27: an import can't be active until it passes an evaluation on the importer's own
    /// accounts: then a release of exactly its build, signed by the importer, makes it
    /// active (with the importer's sign-off of what it asks for); or the importer accepts its
    /// publisher's report on purpose, recorded in its history.
    #[test]
    fn an_import_is_active_only_after_an_evaluation_here_or_its_publishers_report_on_purpose() {
        let (f, mut trust) = released();
        publish(
            &f.store,
            &mut trust,
            "fin-analyst",
            &["bob".into()],
            &["1.0.0".into()],
            "alice",
        )
        .unwrap();
        let sent = bundle(
            &f.store,
            "alice",
            trust.get_by_name("bob").unwrap(),
            "fin-analyst",
            "1.0.0",
        )
        .unwrap();
        let ask = ImportAsk {
            publisher: "alice".into(),
            publisher_id: f.owner.public().to_string(),
            accept_publisher_report: false,
            by: "bob".into(),
        };
        let here = tempfile::tempdir().unwrap();
        let store = FactoryStore::new(here.path().join("factory"));
        let (key, worker) = (SecretKey::generate(), SecretKey::generate());
        import(&store, "bob", &key, sent.clone(), &ask).unwrap();
        let (_, build) = pending(&store, "fin-analyst").unwrap().unwrap();

        let failed = evaluated_here(&store, &build.hash, &key, &worker, 1, true);
        assert!(
            after_evaluation(&store, "bob", &key, &failed)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            import_state(&store, "fin-analyst", "1.0.0"),
            ImportState::Imported
        );

        let passed = evaluated_here(&store, &build.hash, &key, &worker, 2, false);
        let local = after_evaluation(&store, "bob", &key, &passed)
            .unwrap()
            .unwrap();
        let m = &local.manifest;
        assert_eq!(
            (m.version.as_str(), m.build.as_str()),
            ("1.0.0", build.hash.as_str())
        );
        assert_eq!(
            m.signer_id,
            key.public().to_string(),
            "signed here, by the importer"
        );
        assert_eq!(m.evaluation, passed.id);
        assert!(
            m.approvers.is_empty(),
            "the publisher's approvers aren't asked here"
        );
        assert_eq!(
            m.origin.imported.as_ref().map(|o| o.publisher.as_str()),
            Some("alice")
        );
        // It asks for an MCP server: its owner signs that off, then it is active.
        assert_eq!(local.state, ReleaseState::PendingApproval);
        assert_eq!(
            import_state(&store, "fin-analyst", "1.0.0"),
            ImportState::Imported
        );
        let report = release::report_of(&store, "fin-analyst", "1.0.0").unwrap();
        let yes = release::own_answer("bob", &key, &local, &report, true, "ok").unwrap();
        release::answer(&store, "fin-analyst", "1.0.0", yes).unwrap();
        assert_eq!(
            import_state(&store, "fin-analyst", "1.0.0"),
            ImportState::Active
        );
        let state = store.state("fin-analyst").unwrap().unwrap();
        assert_eq!(state.state, Lifecycle::Active);
        assert_eq!(state.active_version.as_deref(), Some("1.0.0"));
        assert!(pending(&store, "fin-analyst").unwrap().is_none());
        // It stays its publisher's: it isn't improved here.
        let Err(fork) = crate::factory::improve::material_in(
            &store,
            "fin-analyst",
            &[],
            Some("shorter"),
            Vec::new(),
        ) else {
            panic!("an import isn't improved here");
        };
        assert!(
            fork.to_string().contains("only its publisher improves it"),
            "{fork}"
        );

        // Another importer accepts the publisher's report on purpose.
        let there = tempfile::tempdir().unwrap();
        let dana = FactoryStore::new(there.path().join("factory"));
        let accepting = ImportAsk {
            accept_publisher_report: true,
            by: "dana".into(),
            ..ask
        };
        let imported = import(&dana, "dana", &SecretKey::generate(), sent, &accepting).unwrap();
        let local = imported
            .release
            .expect("a release here, from the publisher's report");
        assert_eq!(
            local.manifest.evaluation,
            release::report_of_dir(&imported_dir(&dana, "alice", "fin-analyst", "1.0.0"))
                .unwrap()
                .id
        );
        let state = dana.state("fin-analyst").unwrap().unwrap();
        assert!(
            state
                .history
                .iter()
                .any(|c| c.what.contains("accepted alice's report")),
            "{:?}",
            state.history
        );
        assert!(state.imported.unwrap().versions[0].accepted_report);
    }

    /// An earlier version imported after a later one keeps its publisher's version, and
    /// rolls nothing back: it is there to roll back to.
    #[test]
    fn an_earlier_version_imported_after_a_later_one_keeps_its_version_and_rolls_nothing_back() {
        let (f, mut trust) = released();
        let e = f.evaluated(
            |v: &mut Value| {
                v["approvers"] = json!(["me"]);
                v["instructions"] = json!("Quote figures exactly, with their page.");
            },
            false,
        );
        let later = f.release(&e.id, None).unwrap().manifest.version;
        f.approve(&later);
        let versions = [later.clone(), "1.0.0".to_owned()];
        publish(
            &f.store,
            &mut trust,
            "fin-analyst",
            &["bob".into()],
            &versions,
            "alice",
        )
        .unwrap();
        let bob = trust.get_by_name("bob").unwrap().clone();
        let here = tempfile::tempdir().unwrap();
        let store = FactoryStore::new(here.path().join("factory"));
        let key = SecretKey::generate();
        let ask = ImportAsk {
            publisher: "alice".into(),
            publisher_id: f.owner.public().to_string(),
            accept_publisher_report: true,
            by: "bob".into(),
        };
        for version in &versions {
            let sent = bundle(&f.store, "alice", &bob, "fin-analyst", version).unwrap();
            let local = import(&store, "bob", &key, sent, &ask)
                .unwrap()
                .release
                .unwrap();
            assert_eq!(&local.manifest.version, version, "its publisher's version");
            if local.state != ReleaseState::Released {
                let report = release::report_of(&store, "fin-analyst", version).unwrap();
                let yes = release::own_answer("bob", &key, &local, &report, true, "ok").unwrap();
                release::answer(&store, "fin-analyst", version, yes).unwrap();
            }
        }
        let state = store.state("fin-analyst").unwrap().unwrap();
        assert_eq!(state.versions, versions);
        assert_eq!(
            state.active_version,
            Some(later),
            "1.0.0 rolls nothing back"
        );
    }

    async fn until(what: &str, done: impl Fn() -> bool) {
        tokio::time::timeout(std::time::Duration::from_secs(20), async {
            while !done() {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
    }

    /// AC-26 over iroh: a granted peer gets its catalog in the hello and whenever it changes,
    /// under its own name for the publisher (never one the publisher gives itself), and
    /// takes the bundle over the connection; one not published to it is refused.
    #[tokio::test]
    async fn a_granted_peer_gets_its_catalog_in_the_hello_and_on_change_and_the_bundle() {
        use crate::agent::{Agent, AgentConfig, Network};
        let (f, _) = released();
        let bob_key = SecretKey::generate();
        let mut alice_trust = TrustStore::default();
        alice_trust.allow(TrustedPeer {
            id: bob_key.public(),
            ..peer("bob")
        });
        publish(
            &f.store,
            &mut alice_trust,
            "fin-analyst",
            &["bob".into()],
            &["1.0.0".into()],
            "alice",
        )
        .unwrap();
        let start = |name: &str,
                     key: SecretKey,
                     trust: TrustStore,
                     publisher: Option<Arc<dyn Publisher>>| {
            let mut config = AgentConfig::new(name, key);
            config.network = Network::Local;
            config.bind = Some("127.0.0.1:0".parse().unwrap());
            config.trust = trust;
            config.publisher = publisher;
            Agent::bind(config)
        };
        let publisher: Arc<dyn Publisher> = ProfilePublisher::of_store(f.store.clone(), "alice");
        let (alice, _alice_events) = start(
            "alice",
            f.owner.clone(),
            alice_trust.clone(),
            Some(publisher),
        )
        .await
        .unwrap();
        let mut bob_trust = TrustStore::default();
        bob_trust.allow(TrustedPeer {
            id: f.owner.public(),
            ..peer("al")
        });
        let (bob, _bob_events) = start("bob", bob_key.clone(), bob_trust, None)
            .await
            .unwrap();
        bob.connect(alice.addr()).await.unwrap();
        until("the hello's catalog", || !bob.catalogs().is_empty()).await;
        let catalogs = bob.catalogs();
        assert_eq!(catalogs[0].0, alice.id(), "known by its key");
        assert_eq!(catalogs[0].1.name, "al", "and by bob's name for it");
        assert_eq!(catalogs[0].1.entries[0].name, "fin-analyst");

        // Unpublished: bob's catalog empties.
        publish(
            &f.store,
            &mut alice_trust,
            "fin-analyst",
            &["bob".into()],
            &[],
            "alice",
        )
        .unwrap();
        alice.refresh_catalog();
        until("the catalog to empty", || bob.catalogs().is_empty()).await;
        // Published again: bob takes the bundle over the connection.
        publish(
            &f.store,
            &mut alice_trust,
            "fin-analyst",
            &["bob".into()],
            &["1.0.0".into()],
            "alice",
        )
        .unwrap();
        alice.refresh_catalog();
        until("the catalog again", || !bob.catalogs().is_empty()).await;
        let wait = std::time::Duration::from_secs(20);
        let refused = bob
            .request_bundle(alice.id(), "fin-analyst", "9.9.9", wait)
            .await
            .unwrap_err();
        assert_eq!(
            refused.to_string(),
            "alice publishes no fin-analyst 9.9.9 to you"
        );
        let sent = bob
            .request_bundle(alice.id(), "fin-analyst", "1.0.0", wait)
            .await
            .unwrap();
        let here = tempfile::tempdir().unwrap();
        let store = FactoryStore::new(here.path().join("factory"));
        let imported = import(
            &store,
            "bob",
            &bob_key,
            sent,
            &ImportAsk {
                publisher: "al".into(),
                publisher_id: alice.id().to_string(),
                accept_publisher_report: false,
                by: "bob".into(),
            },
        )
        .unwrap();
        assert!(imported.verified);
        let history = f.store.state("fin-analyst").unwrap().unwrap().history;
        assert!(
            history
                .iter()
                .any(|c| c.what == "sent 1.0.0 to bob (an import)")
        );
        bob.shutdown().await;
        alice.shutdown().await;
    }
}
