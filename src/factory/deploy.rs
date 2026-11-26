//! Deployments (docs/agent-factory.md, section 9; ADR-0005): a release runs as a digital
//! worker pinned to it. The worker is named after the agent; its profile gets a copy of the
//! release, checked against the release's signed manifest whenever the worker starts or its
//! pin changes, and every task it serves gets the release's instructions and skills first.
//! Its model settings are the release's (the evaluated models only), its memory the
//! blueprint's, and its folders are shared as its owner shares them, at no more access than
//! the agent asks for, with only the MCP servers and tools it asks for.
//!
//! ```text
//! <worker profile>/release/
//!   pin.json        the release it runs, its state (canary, running, stopped) and budgets
//!   <version>/      the release, as kept in the owner's factory (see crate::factory::release)
//! ```
//!
//! A new deployment runs a **canary** first: the release's smoke cases, on the worker, through
//! the eval runner; meanwhile the worker refuses every task but its owner's canary tasks, so
//! squad steps move to another member. If they fail, the deployment goes back to the release
//! it ran before, or its worker stops when there was none. A **rollback** pins an earlier
//! release on every deployment, and a deployment's **budgets** (tasks a day, attempts and
//! minutes a task) are refused or stopped by its worker before they are exceeded.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use iroh::{EndpointAddr, EndpointId, SecretKey};
use serde::{Deserialize, Serialize};

use crate::agent::Agent;
use crate::factory::blueprint::{
    Blueprint, Budgets, Catalog, MAX_TASKS_PER_DAY, MAX_TIMEOUT_SECS, MIN_TIMEOUT_SECS,
};
use crate::factory::release::{self, Release, ReleaseState};
use crate::factory::report::{EvalState, Evaluation};
use crate::factory::store::{
    AgentState, Canary, DeployState, DeploymentRecord, FactoryStore, Lifecycle, now, read_json,
    write_json,
};
use crate::factory::{capabilities, conflict, invalid, not_found, routing, runner};
use crate::llm::{Access, LlmTask, Provider};
use crate::profile::Profile;
use crate::shares::{DATA_SHARE, Share};
use crate::trust::{TrustStore, TrustedPeer};
use crate::workers::{Process, Supervisor, WorkerSpec};

/// Where a worker keeps the release it runs.
pub const RELEASE_DIR: &str = "release";
pub const PIN_FILE: &str = "pin.json";
/// The most attempts a budget allows a task (as a task may ask for).
pub const MAX_ATTEMPTS: u32 = crate::evidence::MAX_ATTEMPTS;
/// How often a worker looks at its pin.
pub const WATCH_EVERY: Duration = Duration::from_secs(2);

/// What a worker runs: the release, as its owner pinned it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pin {
    pub agent: String,
    pub version: String,
    /// The release's manifest hash: the copy must be exactly that release.
    pub manifest_sha256: String,
    pub state: DeployState,
    /// Its budgets: the release's, with its owner's changes.
    #[serde(default, skip_serializing_if = "Budgets::is_empty")]
    pub budgets: Budgets,
}

/// A worker's pinned release, verified, as its tasks use it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pinned {
    pub agent: String,
    pub version: String,
    pub state: DeployState,
    /// Its agent file's instructions and its skills', as each task gets them first.
    pub instructions: String,
    pub budgets: Budgets,
    /// It may hand work to other agents (team steps).
    pub delegate: bool,
    /// The most access its tasks get.
    pub access: Access,
}

impl Pinned {
    /// How the agent is named in evidence, the audit and the hello: `fin-analyst@1.2.0`.
    pub fn label(&self) -> String {
        format!("{}@{}", self.agent, self.version)
    }

    /// `task` with the release's instructions first.
    pub fn prompt(&self, task: &str) -> String {
        format!(
            "Follow these instructions:\n{}\n---\n{task}",
            self.instructions
        )
    }
}

/// The release pinned on `worker`, as its owner wrote it (not verified: for showing it).
pub fn pin_of(worker: &Profile) -> Option<Pin> {
    read_json::<Pin>(&worker.dir.join(RELEASE_DIR).join(PIN_FILE))
        .ok()
        .flatten()
}

/// The tasks a worker whose audit log is `audit` took today (UTC) as release `label`
/// (`name@version`): what its budget of tasks a day counts.
pub fn tasks_today(audit: &Path, label: &str) -> u32 {
    let day = chrono::Utc::now().date_naive();
    crate::audit::AuditLog::read(audit, usize::MAX)
        .unwrap_or_default()
        .iter()
        .filter(|e| {
            e.outcome != crate::audit::Outcome::Refused
                && e.agent.as_deref() == Some(label)
                && chrono::DateTime::from_timestamp_millis(e.ts_ms as i64)
                    .is_some_and(|t| t.date_naive() == day)
        })
        .count() as u32
}

/// Reads and verifies the release pinned in a worker's `dir` (its `release/` folder): the
/// copy must be exactly the pinned release, signed by `owner` (its owner's endpoint id, when
/// known). `None`: it pins none.
pub fn read_pin(dir: &Path, owner: Option<&EndpointId>) -> Result<Option<Pinned>> {
    let Some(pin) = read_json::<Pin>(&dir.join(PIN_FILE))? else {
        return Ok(None);
    };
    let copy = dir.join(&pin.version);
    let release = release::verify_dir(&copy, &pin.agent, &pin.version)
        .with_context(|| format!("its release {} {} doesn't verify", pin.agent, pin.version))?;
    if release.manifest_sha256 != pin.manifest_sha256 {
        bail!(
            "its copy of {} {} isn't the release its owner pinned",
            pin.agent,
            pin.version
        );
    }
    if let Some(owner) = owner
        && release.manifest.signer_id != owner.to_string()
    {
        bail!(
            "its release {} {} isn't its owner's",
            pin.agent,
            pin.version
        );
    }
    if release.state != ReleaseState::Released {
        bail!(
            "its release {} {} is {}",
            pin.agent,
            pin.version,
            release.state
        );
    }
    let mut files = BTreeMap::new();
    crate::factory::store::read_tree(&copy.join(release::FILES_DIR), "", &mut files)?;
    let build = crate::factory::build::Build::of(&pin.agent, files);
    let blueprint = runner::blueprint_of(&build)?;
    Ok(Some(Pinned {
        instructions: release::instructions(&build, &pin.version)?,
        agent: pin.agent,
        version: pin.version,
        state: pin.state,
        budgets: pin.budgets,
        delegate: blueprint.capabilities.delegate,
        access: blueprint.capabilities.access,
    }))
}

/// The release a worker runs, as it serves tasks: reloaded whenever its pin changes, its
/// budgets counted.
pub struct Deployed {
    dir: Option<PathBuf>,
    owner: Option<EndpointId>,
    audit: Option<PathBuf>,
    seen: Mutex<Option<Vec<u8>>>,
    current: Mutex<Result<Arc<Pinned>, String>>,
    /// The day (UTC) and the tasks it took that day.
    today: Mutex<(chrono::NaiveDate, u32)>,
}

impl Deployed {
    /// The deployment `worker` runs, if it pins a release (whose owner is `owner`).
    pub fn of(worker: &Profile, owner: Option<EndpointId>) -> Option<Arc<Self>> {
        let dir = worker.dir.join(RELEASE_DIR);
        if !dir.join(PIN_FILE).is_file() {
            return None;
        }
        let deployed = Arc::new(Self {
            dir: Some(dir),
            owner,
            audit: Some(worker.audit_path()),
            seen: Mutex::new(None),
            current: Mutex::new(Err("its release isn't loaded yet".into())),
            today: Mutex::new((chrono::Utc::now().date_naive(), 0)),
        });
        deployed.reload();
        Some(deployed)
    }

    /// A deployment of `pinned`, with no folder behind it (for tests and embedders).
    pub fn fixed(pinned: Pinned) -> Arc<Self> {
        Arc::new(Self {
            dir: None,
            owner: None,
            audit: None,
            seen: Mutex::new(None),
            current: Mutex::new(Ok(Arc::new(pinned))),
            today: Mutex::new((chrono::Utc::now().date_naive(), 0)),
        })
    }

    /// The release it runs, or why it can't run it (then it refuses every task).
    pub fn current(&self) -> Result<Arc<Pinned>, String> {
        self.current.lock().unwrap().clone()
    }

    /// Reads its pin again; when it changed, loads and verifies the release it names.
    /// Returns whether it changed.
    pub fn reload(&self) -> bool {
        let Some(dir) = &self.dir else {
            return false;
        };
        let bytes = std::fs::read(dir.join(PIN_FILE)).ok();
        if *self.seen.lock().unwrap() == bytes {
            return false;
        }
        let loaded = match read_pin(dir, self.owner.as_ref()) {
            Ok(Some(pinned)) => Ok(Arc::new(pinned)),
            Ok(None) => Err("it pins no release".to_owned()),
            Err(e) => Err(format!("{e:#}")),
        };
        if let (Ok(pinned), Some(audit)) = (&loaded, &self.audit) {
            // Today's tasks so far, after a restart.
            let day = chrono::Utc::now().date_naive();
            let taken = tasks_today(audit, &pinned.label());
            *self.today.lock().unwrap() = (day, taken);
        }
        *self.seen.lock().unwrap() = bytes;
        *self.current.lock().unwrap() = loaded;
        true
    }

    /// Reloads its pin every `every` for as long as `alive` says so.
    pub fn watch(self: &Arc<Self>, every: Duration) {
        let this = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(every).await;
                let Some(this) = this.upgrade() else {
                    return;
                };
                if this.reload()
                    && let Err(why) = this.current()
                {
                    eprintln!("its release can't run: {why}");
                }
            }
        });
    }

    /// Whether a task from a peer may run, before any model call: the release must load,
    /// the deployment take work (in its canary only its owner's canary tasks do), and the
    /// task fit its budgets. `owners_evaluation` says the task is its owner's, in the
    /// evaluation folder: its canary's tasks run outside its budgets, its drift watch's (on
    /// a deployment that takes work) within them. Counts the task against today's budget
    /// when it may.
    pub fn admit(
        &self,
        task: &LlmTask,
        owners_evaluation: bool,
    ) -> Result<Arc<Pinned>, (&'static str, String)> {
        let pinned = self
            .current()
            .map_err(|why| ("release", format!("its release can't run: {why}")))?;
        let in_canary = matches!(pinned.state, DeployState::Canary | DeployState::Starting);
        match pinned.state {
            DeployState::Stopped => {
                return Err((
                    "stopped",
                    format!(
                        "{} {} is stopped: it takes no work",
                        pinned.agent, pinned.version
                    ),
                ));
            }
            DeployState::Canary | DeployState::Starting if !owners_evaluation => {
                return Err((
                    "canary",
                    format!(
                        "{} {} is in its canary: it takes work once its smoke cases pass",
                        pinned.agent, pinned.version
                    ),
                ));
            }
            _ => {}
        }
        if owners_evaluation && in_canary {
            return Ok(pinned);
        }
        if task.runs_model()
            && let (Some(limit), Some(asked)) = (pinned.budgets.attempts, task.attempts)
            && asked > limit
        {
            return Err((
                "budget",
                format!(
                    "the task asks for {asked} attempts; {}'s budget allows {limit}",
                    pinned.label()
                ),
            ));
        }
        if let Some(limit) = pinned.budgets.tasks_per_day {
            let mut today = self.today.lock().unwrap();
            let day = chrono::Utc::now().date_naive();
            if today.0 != day {
                *today = (day, 0);
            }
            if today.1 >= limit {
                return Err((
                    "budget",
                    format!(
                        "{} took its budget of {limit} task{} today; it takes work again tomorrow (UTC)",
                        pinned.label(),
                        if limit == 1 { "" } else { "s" }
                    ),
                ));
            }
            today.1 += 1;
        }
        Ok(pinned)
    }
}

/// A deployment asked for.
#[derive(Debug, Clone, Default)]
pub struct DeployAsk {
    /// The release (default: the active one).
    pub version: Option<String>,
    /// The shared folders it works in (none, or an empty list: its release's deploy folders).
    pub folders: Option<Vec<String>>,
    /// Run the release's smoke cases on it before it takes work.
    pub canary: bool,
    /// It starts whenever its owner's agent starts (default: as its blueprint says).
    pub start_with_owner: Option<bool>,
    pub by: String,
}

/// Why agent `name` isn't deployed when neither the deployment nor its release names a folder
/// it works in (its own data folder doesn't count): the web UI's words, which name no flags.
pub fn no_folders(name: &str) -> String {
    format!("{name}'s release names no folders to work in: pick at least one folder you share")
}

/// A deployment made or changed: its record and its worker.
#[derive(Debug)]
pub struct Placed {
    pub record: DeploymentRecord,
    pub worker: Profile,
    pub id: EndpointId,
    /// For a worker made now: its entry for the owner's allowlist (the caller adds it, to
    /// the running agent's or to the saved allowlist).
    pub entry: Option<TrustedPeer>,
}

/// Deploys agent `name` of `owner`'s (whose agent is at `owner_addr`, with `allowlist`) as
/// `ask` says: its release verified and what it asks for found in its folders (else a
/// refusal naming the command that offers it), then its worker made or changed and pinned to
/// it. The worker starts its canary (or takes work, without one) when it runs.
pub fn deploy(
    owner: &Profile,
    owner_addr: EndpointAddr,
    allowlist: &TrustStore,
    name: &str,
    ask: &DeployAsk,
) -> Result<Placed> {
    let store = FactoryStore::of(owner);
    let state = store
        .state(name)?
        .ok_or_else(|| not_found(format!("the factory has no agent '{name}'")))?;
    if state.state == Lifecycle::Retired {
        return Err(conflict(format!(
            "{name} is retired: it isn't deployed again"
        )));
    }
    let version = match (&ask.version, &state.active_version) {
        (Some(v), _) => v.clone(),
        (None, Some(v)) => v.clone(),
        (None, None) => {
            return Err(conflict(format!(
                "{name} has no active release: release it first (proofkin factory release {name} --evaluation ID --profile {})",
                owner.name
            )));
        }
    };
    let release = release::load(&store, name, &version).map_err(|e| invalid(format!("{e:#}")))?;
    if release.state != ReleaseState::Released {
        return Err(conflict(format!(
            "release {version} of {name} is {}: only a released release is deployed",
            release.state
        )));
    }
    let bp = release::blueprint_of(&store, name, &version)?;
    let folders = match &ask.folders {
        Some(folders) if !folders.is_empty() => folders.clone(),
        _ => bp.deploy.folders.clone(),
    };
    if folders.is_empty() {
        return Err(invalid(no_folders(name)));
    }
    let shares = owner.shares()?;
    for folder in &folders {
        if folder == DATA_SHARE || shares.get(folder).is_none() {
            return Err(invalid(format!("you share no folder '{folder}'")));
        }
    }
    let catalog = Catalog::of(owner)?.without(name);
    let resolution = capabilities::resolve(&bp, &folders, &catalog);
    if let Some(problem) = resolution.problems.iter().find(|p| p.error) {
        return Err(conflict(match &problem.fix {
            Some(fix) => format!("{}: {fix}", problem.message),
            None => problem.message.clone(),
        }));
    }
    let existing = crate::workers::worker_of(owner, name).ok();
    if let Some(worker) = &existing
        && read_json::<Pin>(&worker.dir.join(RELEASE_DIR).join(PIN_FILE))?
            .is_none_or(|p| p.agent != name)
    {
        return Err(conflict(format!(
            "'{name}' is a digital worker already, and runs no release of {name}: remove it, or rename the agent"
        )));
    }
    // Unless asked, a deployment keeps whether it starts with its owner; a new one does as
    // its blueprint says.
    let start_with_owner = ask.start_with_owner.unwrap_or_else(|| {
        existing
            .as_ref()
            .map_or(bp.deploy.start_with_owner, |w| w.config.start_with_owner)
    });
    let (mut worker, id, entry) = match existing {
        Some(worker) => {
            let id = worker.identity()?.public();
            (worker, id, None)
        }
        None => {
            let created = crate::workers::create(
                owner,
                owner_addr,
                allowlist,
                &WorkerSpec {
                    name: name.to_owned(),
                    role: Some(role_of(&bp)),
                    providers: providers_of(&release),
                    folders: Vec::new(),
                    start_with_owner,
                    workiq: !bp.capabilities.workiq.is_empty(),
                },
            )
            .map_err(|e| conflict(format!("{e:#}")))?;
            (created.worker, created.id, Some(created.entry))
        }
    };
    worker.config.start_with_owner = start_with_owner;
    let before = state.deployment(name).cloned();
    let budgets = before
        .as_ref()
        .map(|d| d.budgets.clone())
        .unwrap_or_default();
    let target = if ask.canary {
        DeployState::Canary
    } else {
        DeployState::Running
    };
    place(
        owner,
        &mut worker,
        &release,
        &bp,
        &folders,
        &budgets,
        target,
    )
    .map_err(|e| conflict(format!("{e:#}")))?;
    let record = DeploymentRecord {
        worker: name.to_owned(),
        version: version.clone(),
        state: target,
        previous_version: before
            .as_ref()
            .filter(|d| d.version != version)
            .map(|d| d.version.clone())
            .or_else(|| before.as_ref().and_then(|d| d.previous_version.clone())),
        canary: None,
        folders: folders.clone(),
        budgets,
        note: None,
        at: now(),
    };
    let kept = record.clone();
    store.update_state(name, |s| {
        s.deployments.retain(|d| d.worker != kept.worker);
        s.deployments.push(kept.clone());
        s.changed(
            &ask.by,
            format!(
                "deployed {version} on worker {name} in {}{}",
                folders.join(", "),
                if ask.canary { ", canary first" } else { "" }
            ),
            None,
        );
        Ok(())
    })?;
    Ok(Placed {
        record,
        worker,
        id,
        entry,
    })
}

/// A deployed agent's role: what older peers see, its purpose.
fn role_of(bp: &Blueprint) -> String {
    bp.purpose
        .chars()
        .take(crate::workers::MAX_ROLE_CHARS)
        .collect()
}

/// The providers a release runs on: those of the models that passed.
fn providers_of(release: &Release) -> Vec<Provider> {
    let mut providers: Vec<Provider> = release
        .manifest
        .model
        .passing
        .iter()
        .chain(std::iter::once(&release.manifest.model.chosen))
        .map(|r| r.provider)
        .collect();
    providers.sort();
    providers.dedup();
    providers
}

/// The budgets a deployment works within: the release's, with `changed`'s where set.
pub fn effective(release: &Budgets, changed: &Budgets) -> Budgets {
    Budgets {
        tasks_per_day: changed.tasks_per_day.or(release.tasks_per_day),
        attempts: changed.attempts.or(release.attempts),
        timeout_secs: changed.timeout_secs.or(release.timeout_secs),
    }
}

/// Pins `release` (of blueprint `bp`) on `worker`, owned by `owner`, in `state`: its copy of
/// the release, its pin, role, providers, model settings and memory, and its folders at no
/// more access than the agent asks for, with only the MCP servers and tools it asks for.
fn place(
    owner: &Profile,
    worker: &mut Profile,
    release: &Release,
    bp: &Blueprint,
    folders: &[String],
    budgets: &Budgets,
    state: DeployState,
) -> Result<()> {
    let store = FactoryStore::of(owner);
    let (name, version) = (&release.manifest.agent, &release.manifest.version);
    worker.config.role = Some(role_of(bp));
    worker.config.providers = providers_of(release);
    worker.config.models = routing::pin(
        &release.manifest.model.chosen,
        &release.manifest.model.passing,
    )?;
    worker.config.workiq_from_owner = !bp.capabilities.workiq.is_empty();
    worker.save()?;
    crate::factory::memory::configure(worker, bp.memory.clone()).context("setting its memory")?;
    let shares = owner.shares()?;
    let access = bp.capabilities.access;
    worker.update_shares(|mine| {
        mine.shares.retain(|s| s.name == DATA_SHARE);
        for folder in folders {
            let theirs = shares
                .get(folder)
                .ok_or_else(|| anyhow!("you share no folder '{folder}'"))?;
            let mut mcp = BTreeMap::new();
            for asked in &bp.capabilities.mcp {
                if let Some(offer) = theirs.mcp.get(&asked.server) {
                    let mut offer = offer.clone();
                    if !asked.tools.iter().any(|t| t == "*") {
                        offer.tools = asked
                            .tools
                            .iter()
                            .filter(|t| offer.allows_tool(t))
                            .cloned()
                            .collect();
                    }
                    mcp.insert(asked.server.clone(), offer);
                }
            }
            mine.shares.push(Share {
                name: theirs.name.clone(),
                path: theirs.path.clone(),
                max_access: theirs.max_access.min(access),
                checks: theirs.checks.clone(),
                worktrees: theirs.worktrees,
                mcp,
                team: None,
                team_model: false,
                team_approval: false,
                pull_requests: false,
                pull_request_approval: false,
            });
        }
        Ok(())
    })?;
    // Its owner (on its allowlist since it was made) gives it tasks in its folders, and its
    // canary's, at most at the agent's access.
    let mut trust = worker.trust()?;
    let mut granted = vec![DATA_SHARE.to_owned(), runner::SHARE.to_owned()];
    granted.extend(folders.iter().cloned());
    if let Some(entry) = trust.peers.iter_mut().find(|p| p.name == owner.name) {
        entry.can_delegate = true;
        entry.tasks_per_hour = entry.tasks_per_hour.max(runner::TASKS_PER_HOUR);
        entry.access = access;
        entry.shares = granted;
    }
    trust.save(&worker.trust_path())?;
    // Its copy of the release, then the pin that names it.
    let dir = worker.dir.join(RELEASE_DIR);
    let copy = dir.join(version);
    if !copy.join(release::RELEASE_FILE).is_file()
        || release::verify_dir(&copy, name, version).is_err()
    {
        let partial = dir.join(format!(".partial-{}", uuid::Uuid::new_v4().simple()));
        copy_tree(&store.release_dir(name, version), &partial)?;
        let _ = std::fs::remove_dir_all(&copy);
        crate::factory::store::rename_dir(&partial, &copy)?;
    }
    write_json(
        &dir.join(PIN_FILE),
        &Pin {
            agent: name.clone(),
            version: version.clone(),
            manifest_sha256: release.manifest_sha256.clone(),
            state,
            budgets: effective(&bp.budgets, budgets),
        },
    )?;
    // The copies it no longer runs go, but the one before (a rollback goes back to it).
    let keep: Vec<String> = [Some(version.clone()), previous_copy(&dir, version)]
        .into_iter()
        .flatten()
        .collect();
    for entry in crate::factory::store::read_dir(&dir)? {
        let file = entry.file_name().to_string_lossy().into_owned();
        if entry.path().is_dir() && !keep.contains(&file) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
    Ok(())
}

/// The newest copy in `dir` older than `version`, if any.
fn previous_copy(dir: &Path, version: &str) -> Option<String> {
    let current: release::Version = version.parse().ok()?;
    crate::factory::store::read_dir(dir)
        .ok()?
        .into_iter()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter_map(|n| n.parse::<release::Version>().ok())
        .filter(|v| *v < current)
        .max()
        .map(|v| v.to_string())
}

fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to).with_context(|| format!("creating {}", to.display()))?;
    for entry in crate::factory::store::read_dir(from)? {
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)
                .with_context(|| format!("copying {}", entry.path().display()))?;
        }
    }
    Ok(())
}

/// Changes the state `worker`'s pin says (its release stays).
pub fn set_pin_state(worker: &Profile, state: DeployState) -> Result<()> {
    let path = worker.dir.join(RELEASE_DIR).join(PIN_FILE);
    let mut pin: Pin =
        read_json(&path)?.ok_or_else(|| anyhow!("{} pins no release", worker.name))?;
    pin.state = state;
    write_json(&path, &pin)
}

/// Ends the deployments on `owner`'s digital worker `worker`, which is being deleted: each
/// one not stopped yet is stopped, saying why in its agent's history and the audit log, so
/// a rollback, the drift watch and the factory card pass it by. Returns the agents it ran.
pub fn worker_deleted(owner: &Profile, worker: &str, by: &str) -> Result<Vec<String>> {
    let store = FactoryStore::of(owner);
    let live = |s: &AgentState| {
        s.deployment(worker)
            .is_some_and(|d| d.state != DeployState::Stopped)
    };
    let mut ended = Vec::new();
    for name in store.names()? {
        if !store.state(&name)?.is_some_and(|s| live(&s)) {
            continue;
        }
        store.update_state(&name, |s| {
            if let Some(d) = s
                .deployment_mut(worker)
                .filter(|d| d.state != DeployState::Stopped)
            {
                d.state = DeployState::Stopped;
                d.note = Some("its worker was deleted".into());
                d.at = now();
                s.changed(
                    by,
                    format!("deployment on {worker} ended: its worker was deleted"),
                    None,
                );
            }
            Ok(())
        })?;
        ended.push(name);
    }
    Ok(ended)
}

/// Pins release `version` on every deployment of agent `name` of `owner`'s that isn't
/// stopped, and makes it the active release: an earlier release, after a bad one. Their
/// workers take it within seconds (they watch their pin). Returns the agent's state and the
/// workers changed (whoever runs them restarts them, for their memory and role).
pub fn rollback(
    owner: &Profile,
    name: &str,
    to: Option<&str>,
    reason: &str,
    by: &str,
) -> Result<(AgentState, Vec<String>)> {
    let reason = reason.trim();
    if reason.is_empty() {
        return Err(invalid("say why it is rolled back"));
    }
    if reason.chars().count() > 500 {
        return Err(invalid("a reason is at most 500 characters"));
    }
    let store = FactoryStore::of(owner);
    let state = store
        .state(name)?
        .ok_or_else(|| not_found(format!("the factory has no agent '{name}'")))?;
    if state.state == Lifecycle::Retired {
        return Err(conflict(format!("{name} is retired")));
    }
    let active = state
        .active_version
        .clone()
        .ok_or_else(|| conflict(format!("{name} has no active release to roll back")))?;
    let current: release::Version = active.parse()?;
    let target = match to {
        Some(v) => {
            let release = release::load(&store, name, v).map_err(|e| invalid(format!("{e:#}")))?;
            if release.state != ReleaseState::Released {
                return Err(conflict(format!(
                    "release {v} of {name} is {}: only a released release is rolled back to",
                    release.state
                )));
            }
            if v == active {
                return Err(conflict(format!("{v} is {name}'s active release already")));
            }
            release
        }
        None => release::releases(&store, name)?
            .into_iter()
            .filter_map(|(_, r)| r.ok())
            .find(|r| r.state == ReleaseState::Released && r.version() < current)
            .ok_or_else(|| conflict(format!("{name} has no release before {active}")))?,
    };
    let version = target.manifest.version.clone();
    let bp = release::blueprint_of(&store, name, &version)?;
    let mut changed = Vec::new();
    for deployment in state
        .deployments
        .iter()
        .filter(|d| d.state != DeployState::Stopped)
    {
        let mut worker = crate::workers::worker_of(owner, &deployment.worker)?;
        place(
            owner,
            &mut worker,
            &target,
            &bp,
            &deployment.folders,
            &deployment.budgets,
            DeployState::Running,
        )?;
        changed.push(deployment.worker.clone());
    }
    let state = store.update_state(name, |s| {
        s.active_version = Some(version.clone());
        for d in s
            .deployments
            .iter_mut()
            .filter(|d| changed.contains(&d.worker))
        {
            d.previous_version = Some(d.version.clone());
            d.version = version.clone();
            d.state = DeployState::Running;
            d.note = Some(format!("rolled back: {reason}"));
            d.at = now();
        }
        s.changed(
            by,
            format!("rolled back from {active} to {version}"),
            Some(reason),
        );
        Ok(())
    })?;
    Ok((state, changed))
}

/// Checks `budgets`: a value out of range is a refusal saying the range.
pub fn check_budgets(budgets: &Budgets) -> Result<()> {
    if let Some(n) = budgets.tasks_per_day
        && !(1..=MAX_TASKS_PER_DAY).contains(&n)
    {
        return Err(invalid(format!(
            "tasks_per_day is 1 to {MAX_TASKS_PER_DAY}"
        )));
    }
    if let Some(n) = budgets.attempts
        && !(1..=MAX_ATTEMPTS).contains(&n)
    {
        return Err(invalid(format!("attempts is at most {MAX_ATTEMPTS}")));
    }
    if let Some(n) = budgets.timeout_secs
        && !(MIN_TIMEOUT_SECS..=MAX_TIMEOUT_SECS).contains(&n)
    {
        return Err(invalid(format!(
            "timeout_secs is {MIN_TIMEOUT_SECS} to {MAX_TIMEOUT_SECS}"
        )));
    }
    Ok(())
}

/// Sets the budgets of every deployment of agent `name` of `owner`'s: `budgets`' values
/// replace the release's (the ones it leaves out stay the release's). Their workers apply
/// them within seconds. Returns the budgets now in force.
pub fn set_budgets(owner: &Profile, name: &str, budgets: Budgets, by: &str) -> Result<Budgets> {
    check_budgets(&budgets)?;
    let store = FactoryStore::of(owner);
    let state = store
        .state(name)?
        .ok_or_else(|| not_found(format!("the factory has no agent '{name}'")))?;
    if state.deployments.is_empty() {
        return Err(invalid(format!(
            "{name} isn't deployed: deploy it first (proofkin factory deploy {name} --profile {})",
            owner.name
        )));
    }
    let mut now_in_force = Budgets::default();
    for deployment in &state.deployments {
        let worker = crate::workers::worker_of(owner, &deployment.worker)?;
        let path = worker.dir.join(RELEASE_DIR).join(PIN_FILE);
        let Some(mut pin) = read_json::<Pin>(&path)? else {
            continue;
        };
        let bp = release::blueprint_of(&store, name, &pin.version)?;
        pin.budgets = effective(&bp.budgets, &budgets);
        now_in_force = pin.budgets.clone();
        write_json(&path, &pin)?;
    }
    let text = release::budgets_text(&now_in_force);
    store.update_state(name, |s| {
        for d in &mut s.deployments {
            d.budgets = budgets.clone();
            d.at = now();
        }
        s.changed(by, format!("budgets: {text}"), None);
        Ok(())
    })?;
    Ok(now_in_force)
}

/// Where a deployment's canary runs from: the owner's agent, its key, and what runs its
/// workers.
#[derive(Clone)]
pub struct Host {
    pub owner: Profile,
    pub agent: Agent,
    pub key: SecretKey,
    pub supervisor: Arc<Supervisor>,
    /// How long the worker gets to connect and share the canary's folder.
    pub ready_wait: Duration,
}

/// Runs the canary of agent `name`'s deployment on worker `worker` (or goes on with the one
/// that didn't end), whose record says it is in its canary: the release's smoke cases on the
/// worker, started first if it doesn't run. Then it takes work; or, if a case failed, it goes
/// back to the release it ran before, or its worker stops when there was none. `on_change`
/// gets the canary's evaluation as its runs end. Returns the deployment's record.
pub async fn run_canary(
    host: &Host,
    name: &str,
    worker: &str,
    on_change: impl Fn(&Evaluation),
) -> Result<DeploymentRecord> {
    let store = FactoryStore::of(&host.owner);
    let _running = store.lock_evaluation()?;
    let state = store
        .state(name)?
        .ok_or_else(|| not_found(format!("the factory has no agent '{name}'")))?;
    let record = state
        .deployment(worker)
        .cloned()
        .ok_or_else(|| not_found(format!("{name} has no deployment on '{worker}'")))?;
    if !matches!(record.state, DeployState::Canary | DeployState::Starting) {
        return Ok(record);
    }
    let release = release::load(&store, name, &record.version)?;
    let bp = release::blueprint_of(&store, name, &record.version)?;
    let build = release::build_of(&store, name, &record.version)?;
    store.save_build(&build)?;
    let profile = crate::workers::worker_of(&host.owner, worker)?;
    let id = profile.identity()?.public();
    if !matches!(host.supervisor.process(&profile), Process::Running { .. }) {
        host.supervisor.start(worker)?;
    }
    let smoke = bp.evals.cases.iter().filter(|c| c.smoke).count();
    let canary = if smoke == 0 {
        Canary {
            passed: true,
            cases: 0,
            failed: Vec::new(),
            evaluation: None,
        }
    } else {
        let folders = host.owner.shares()?;
        let resumable = record
            .canary
            .as_ref()
            .and_then(|c| c.evaluation.as_deref())
            .and_then(|id| store.evaluation(id).ok().flatten())
            .filter(|e| e.state == EvalState::Running);
        let setup = match resumable {
            Some(e) => runner::resume(&store, e, &bp, &folders)?,
            None => runner::prepare_canary(
                &store,
                runner::CanaryRequest {
                    blueprint: &bp,
                    build: &build,
                    version: record.version.clone(),
                    route: release.manifest.model.chosen.clone(),
                    by: host.owner.name.clone(),
                    worker: worker.to_owned(),
                    worker_id: id,
                    folders: &folders,
                    drift: false,
                },
            )?,
        };
        // Journaled: a restart goes on with this canary.
        let journal = setup.evaluation.id.clone();
        store.update_state(name, |s| {
            if let Some(d) = s.deployment_mut(worker) {
                d.state = DeployState::Canary;
                d.canary = Some(Canary {
                    passed: false,
                    cases: smoke,
                    failed: Vec::new(),
                    evaluation: Some(journal.clone()),
                });
            }
            Ok(())
        })?;
        set_pin_state(&profile, DeployState::Canary)?;
        runner::install(&profile, &setup.share)?;
        let runner = runner::Runner {
            agent: host.agent.clone(),
            key: host.key.clone(),
            signer: host.owner.name.clone(),
            worker: id,
            store: store.clone(),
            judges: runner::Judges::default(),
            ready_wait: host.ready_wait,
        };
        let ran = runner.run(setup.evaluation, &setup.share, on_change).await;
        let _ = runner::clean(&store, Some(&profile));
        verdict(&ran?)
    };
    decide_canary(host, name, worker, &record, canary).await
}

/// What a canary's evaluation found: every smoke case must have passed.
pub fn verdict(e: &Evaluation) -> Canary {
    let failed: Vec<String> = e
        .runs
        .iter()
        .filter(|r| !r.passed)
        .map(|r| {
            let why = r.error.clone().or_else(|| {
                r.grades
                    .iter()
                    .find(|g| g.passed == Some(false))
                    .map(|g| g.note.clone())
            });
            match why {
                Some(why) => format!("{}: {why}", r.case),
                None => r.case.clone(),
            }
        })
        .collect();
    Canary {
        passed: failed.is_empty() && !e.runs.is_empty(),
        cases: e.cases.len(),
        failed,
        evaluation: Some(e.id.clone()),
    }
}

/// What became of a deployment after its canary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Settled {
    /// It takes work.
    Running,
    /// It went back to this release.
    BackTo(String),
    /// Its worker must stop: there was no release to go back to.
    Stop,
}

/// Applies a canary's verdict to agent `name`'s deployment on `worker` (of `owner`'s): it
/// takes work, goes back to the release it ran before, or its worker stops (it no longer
/// starts with its owner; whoever runs it stops it). Returns its record and what became of
/// it.
pub fn settle_canary(
    owner: &Profile,
    name: &str,
    worker: &str,
    canary: Canary,
) -> Result<(DeploymentRecord, Settled)> {
    let store = FactoryStore::of(owner);
    let record = store
        .state(name)?
        .and_then(|s| s.deployment(worker).cloned())
        .ok_or_else(|| not_found(format!("{name} has no deployment on '{worker}'")))?;
    let mut profile = crate::workers::worker_of(owner, worker)?;
    let version = record.version.clone();
    let settled = if canary.passed {
        set_pin_state(&profile, DeployState::Running)?;
        Settled::Running
    } else {
        let previous = record.previous_version.as_deref().and_then(|v| {
            release::load(&store, name, v)
                .ok()
                .filter(|r| r.state == ReleaseState::Released)
        });
        match previous {
            Some(earlier) => {
                let bp = release::blueprint_of(&store, name, &earlier.manifest.version)?;
                place(
                    owner,
                    &mut profile,
                    &earlier,
                    &bp,
                    &record.folders,
                    &record.budgets,
                    DeployState::Running,
                )?;
                Settled::BackTo(earlier.manifest.version.clone())
            }
            None => {
                set_pin_state(&profile, DeployState::Stopped)?;
                profile.config.start_with_owner = false;
                profile.save()?;
                Settled::Stop
            }
        }
    };
    let failed = canary.failed.join("; ");
    let updated = store.update_state(name, |s| {
        let what = match &settled {
            Settled::Running => {
                format!("the canary of {version} on {worker} passed: it takes work")
            }
            Settled::BackTo(earlier) => {
                format!("the canary of {version} on {worker} failed ({failed}): back to {earlier}")
            }
            Settled::Stop => {
                format!("the canary of {version} on {worker} failed ({failed}): its worker stopped")
            }
        };
        if let Some(d) = s.deployment_mut(worker) {
            d.canary = Some(canary.clone());
            d.at = now();
            match &settled {
                Settled::Running => d.state = DeployState::Running,
                Settled::BackTo(earlier) => {
                    d.state = DeployState::RolledBack;
                    d.previous_version = Some(version.clone());
                    d.version = earlier.clone();
                    d.note = Some(format!("canary of {version} failed: {failed}"));
                }
                Settled::Stop => {
                    d.state = DeployState::Stopped;
                    d.note = Some(format!("canary of {version} failed: {failed}"));
                }
            }
        }
        s.changed(&owner.name, what, None);
        Ok(())
    })?;
    let record = updated
        .deployment(worker)
        .cloned()
        .ok_or_else(|| anyhow!("{name}'s deployment on '{worker}' went away"))?;
    Ok((record, settled))
}

/// Applies a canary's verdict (see [`settle_canary`]), stops the worker when it must stop,
/// and alerts the owner when it failed.
async fn decide_canary(
    host: &Host,
    name: &str,
    worker: &str,
    record: &DeploymentRecord,
    canary: Canary,
) -> Result<DeploymentRecord> {
    let failed = canary.failed.join("; ");
    let passed = canary.passed;
    let (updated, settled) = settle_canary(&host.owner, name, worker, canary)?;
    if settled == Settled::Stop {
        host.supervisor.stop(worker).await;
    }
    if !passed {
        let _ = host.agent.alert(format!(
            "[{}] the canary of {name} {} failed on {worker}: {failed}",
            host.owner.name, record.version
        ));
    }
    Ok(updated)
}

/// The deployments of `owner`'s agents waiting for their canary: (agent, worker).
pub fn waiting_canaries(owner: &Profile) -> Vec<(String, String)> {
    let store = FactoryStore::of(owner);
    store
        .names()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|name| store.state(&name).ok().flatten())
        .flat_map(|s| {
            s.deployments
                .iter()
                .filter(|d| matches!(d.state, DeployState::Canary | DeployState::Starting))
                .map(|d| (s.name.clone(), d.worker.clone()))
                .collect::<Vec<_>>()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::factory::release::tests::Fixture;
    use serde_json::json;

    #[test]
    fn a_worker_runs_exactly_its_pinned_release_and_a_changed_copy_refuses_work() {
        let f = Fixture::new();
        let home = tempfile::tempdir().unwrap();
        let e = f.evaluated(
            |v| {
                v["approvers"] = json!([]);
                v["capabilities"]["mcp"] = json!([]);
                v["model"]["needs_mcp"] = json!(false);
                v["memory"] = json!({});
            },
            false,
        );
        let release = f.release(&e.id, None).unwrap();
        assert_eq!(release.state, ReleaseState::Released);
        // The worker's copy of the release, and its pin.
        let dir = home.path().join("fin-analyst").join(RELEASE_DIR);
        copy_tree(
            &f.store.release_dir("fin-analyst", "1.0.0"),
            &dir.join("1.0.0"),
        )
        .unwrap();
        let pin = Pin {
            agent: "fin-analyst".into(),
            version: "1.0.0".into(),
            manifest_sha256: release.manifest_sha256.clone(),
            state: DeployState::Running,
            budgets: Budgets {
                tasks_per_day: Some(2),
                attempts: Some(1),
                timeout_secs: None,
            },
        };
        write_json(&dir.join(PIN_FILE), &pin).unwrap();
        let pinned = read_pin(&dir, Some(&f.owner.public())).unwrap().unwrap();
        assert_eq!(pinned.label(), "fin-analyst@1.0.0");
        assert!(pinned.instructions.contains("### Agent: fin-analyst 1.0.0"));
        assert!(
            pinned
                .instructions
                .contains("### Skill: fin-analyst.risk-review")
        );
        assert!(pinned.prompt("Do it").ends_with("\n---\nDo it"));
        // Another owner's key: not its owner's release.
        let other = SecretKey::generate().public();
        assert!(read_pin(&dir, Some(&other)).is_err());
        // A changed copy doesn't run.
        let agent_file = dir.join("1.0.0/files/agent.md");
        let text = std::fs::read_to_string(&agent_file).unwrap();
        std::fs::write(&agent_file, text.replace("Rules", "No rules")).unwrap();
        let broken = format!("{:#}", read_pin(&dir, None).unwrap_err());
        assert!(broken.contains("doesn't verify"), "{broken}");
    }

    #[test]
    fn budgets_refuse_tasks_before_any_model_call_and_a_canary_takes_only_its_owners() {
        let pinned = Pinned {
            agent: "fin-analyst".into(),
            version: "1.2.0".into(),
            state: DeployState::Running,
            instructions: String::new(),
            budgets: Budgets {
                tasks_per_day: Some(2),
                attempts: Some(2),
                timeout_secs: Some(600),
            },
            delegate: false,
            access: Access::ReadOnly,
        };
        let deployed = Deployed::fixed(pinned.clone());
        let task = LlmTask::new(Provider::Copilot, "hi");
        let many = task.clone().with_checks(vec!["test".into()], Some(3));
        let (why, text) = deployed.admit(&many, false).unwrap_err();
        assert_eq!(why, "budget");
        assert_eq!(
            text,
            "the task asks for 3 attempts; fin-analyst@1.2.0's budget allows 2"
        );
        deployed.admit(&task, false).unwrap();
        deployed.admit(&task, false).unwrap();
        let (why, text) = deployed.admit(&task, false).unwrap_err();
        assert_eq!(why, "budget");
        assert!(text.contains("took its budget of 2 tasks today"), "{text}");

        let canary = Deployed::fixed(Pinned {
            state: DeployState::Canary,
            ..pinned.clone()
        });
        let (why, text) = canary.admit(&task, false).unwrap_err();
        assert_eq!(why, "canary");
        assert!(text.contains("in its canary"), "{text}");
        for _ in 0..3 {
            canary.admit(&task, true).unwrap();
        }

        // Its owner's drift watch runs on a deployment that takes work: within its budget.
        let watched = Deployed::fixed(pinned);
        watched.admit(&task, true).unwrap();
        watched.admit(&task, false).unwrap();
        let (why, _) = watched.admit(&task, true).unwrap_err();
        assert_eq!(why, "budget", "the owner's evaluation tasks count too");
    }

    #[test]
    fn budgets_out_of_range_are_refused_saying_the_range() {
        let ok = Budgets {
            tasks_per_day: Some(50),
            attempts: Some(2),
            timeout_secs: Some(1200),
        };
        check_budgets(&ok).unwrap();
        for (bad, says) in [
            (
                Budgets {
                    attempts: Some(6),
                    ..ok.clone()
                },
                "attempts is at most 5",
            ),
            (
                Budgets {
                    tasks_per_day: Some(0),
                    ..ok.clone()
                },
                "tasks_per_day is 1 to",
            ),
            (
                Budgets {
                    timeout_secs: Some(10),
                    ..ok.clone()
                },
                "timeout_secs is 60 to",
            ),
        ] {
            let e = check_budgets(&bad).unwrap_err();
            assert!(format!("{e}").contains(says), "{e}");
        }
        assert_eq!(
            effective(
                &ok,
                &Budgets {
                    attempts: Some(1),
                    ..Budgets::default()
                }
            ),
            Budgets {
                attempts: Some(1),
                ..ok
            }
        );
    }

    /// A blueprint asking for nothing: read-only, no MCP server, no memory, no approver.
    fn plain(v: &mut serde_json::Value) {
        v["approvers"] = json!([]);
        v["capabilities"]["mcp"] = json!([]);
        v["model"]["needs_mcp"] = json!(false);
        v["memory"] = json!({});
    }

    /// alice (in a temporary home) with releases 1.0.0 and 1.0.1 of fin-analyst, and
    /// `workers` of hers pinned to 1.0.1 in its canary (1.0.0 before it, with `previous`).
    fn deployed(workers: &[&str], previous: bool) -> (tempfile::TempDir, Profile, Fixture) {
        let home = tempfile::tempdir().unwrap();
        let owner = Profile::open_in(home.path(), "alice").unwrap();
        owner.save().unwrap();
        let f = Fixture::in_store(FactoryStore::of(&owner));
        let first = f.evaluated(plain, false);
        f.release(&first.id, None).unwrap();
        let second = f.evaluated(
            |v| {
                plain(v);
                v["instructions"] = json!("Analyze the reports again, more carefully.");
            },
            false,
        );
        assert_eq!(
            f.release(&second.id, None).unwrap().manifest.version,
            "1.0.1"
        );
        let store = FactoryStore::of(&owner);
        let release = release::load(&store, "fin-analyst", "1.0.1").unwrap();
        let bp = release::blueprint_of(&store, "fin-analyst", "1.0.1").unwrap();
        for name in workers {
            let mut worker = Profile::open_in(home.path(), name).unwrap();
            worker.config.owner = Some("alice".into());
            worker.save().unwrap();
            let mut trust = worker.trust().unwrap();
            trust.allow(TrustedPeer {
                name: "alice".into(),
                id: f.owner.public(),
                addrs: Default::default(),
                can_delegate: true,
                tasks_per_hour: 20,
                access: Access::ReadOnly,
                shares: vec![DATA_SHARE.into()],
                m365: None,
                github: None,
                agents: Vec::new(),
            });
            trust.save(&worker.trust_path()).unwrap();
            place(
                &owner,
                &mut worker,
                &release,
                &bp,
                &[],
                &Budgets::default(),
                DeployState::Canary,
            )
            .unwrap();
            let pinned = Deployed::of(&worker, None).unwrap().current().unwrap();
            assert_eq!(
                (pinned.label(), pinned.state),
                ("fin-analyst@1.0.1".into(), DeployState::Canary)
            );
        }
        store
            .update_state("fin-analyst", |s| {
                for name in workers {
                    s.deployments.push(DeploymentRecord {
                        worker: (*name).into(),
                        version: "1.0.1".into(),
                        state: DeployState::Canary,
                        previous_version: previous.then(|| "1.0.0".into()),
                        canary: None,
                        folders: Vec::new(),
                        budgets: Budgets::default(),
                        note: None,
                        at: now(),
                    });
                }
                Ok(())
            })
            .unwrap();
        (home, owner, f)
    }

    fn pinned_on(owner: &Profile, worker: &str) -> Arc<Pinned> {
        let worker = crate::workers::worker_of(owner, worker).unwrap();
        Deployed::of(&worker, None).unwrap().current().unwrap()
    }

    /// Deploying another release to a worker, without saying whether it starts with its
    /// owner, keeps what its owner chose: only a new worker does as its blueprint says.
    #[test]
    fn a_redeployment_keeps_whether_its_worker_starts_with_its_owner() {
        let (home, owner, _f) = deployed(&["fin-analyst"], true);
        let reports = home.path().join("reports");
        std::fs::create_dir_all(&reports).unwrap();
        let mut shares = owner.shares().unwrap();
        shares.add("reports", &reports, Access::ReadOnly).unwrap();
        shares.save(&owner.shares_path()).unwrap();
        let starts = || {
            crate::workers::worker_of(&owner, "fin-analyst")
                .unwrap()
                .config
                .start_with_owner
        };
        let mut worker = crate::workers::worker_of(&owner, "fin-analyst").unwrap();
        worker.config.start_with_owner = false;
        worker.save().unwrap();
        let ask = DeployAsk {
            version: Some("1.0.0".into()),
            folders: Some(vec!["reports".into()]),
            canary: false,
            start_with_owner: None,
            by: "alice".into(),
        };
        let at = || EndpointAddr::from(SecretKey::generate().public());
        let placed = deploy(&owner, at(), &TrustStore::default(), "fin-analyst", &ask).unwrap();
        assert_eq!(placed.record.version, "1.0.0");
        assert!(
            !starts(),
            "its blueprint says it starts with its owner; its owner said no"
        );
        let asked = DeployAsk {
            start_with_owner: Some(true),
            ..ask
        };
        deploy(&owner, at(), &TrustStore::default(), "fin-analyst", &asked).unwrap();
        assert!(starts(), "unless asked");
    }

    #[test]
    fn a_failed_canary_goes_back_to_the_release_before_or_stops_its_worker() {
        let failed = Canary {
            passed: false,
            cases: 1,
            failed: vec!["E1: it doesn't say \"operating margin\"".into()],
            evaluation: None,
        };
        // With a release before it: back to it, and the owner sees why.
        let (_home, owner, _f) = deployed(&["fin-analyst"], true);
        let (record, settled) =
            settle_canary(&owner, "fin-analyst", "fin-analyst", failed.clone()).unwrap();
        assert_eq!(settled, Settled::BackTo("1.0.0".into()));
        assert_eq!(
            (record.state, record.version.as_str()),
            (DeployState::RolledBack, "1.0.0")
        );
        assert_eq!(
            record.note.as_deref(),
            Some("canary of 1.0.1 failed: E1: it doesn't say \"operating margin\"")
        );
        let pinned = pinned_on(&owner, "fin-analyst");
        assert_eq!(
            (pinned.version.as_str(), pinned.state),
            ("1.0.0", DeployState::Running)
        );
        let history = FactoryStore::of(&owner)
            .state("fin-analyst")
            .unwrap()
            .unwrap()
            .history;
        assert!(
            history
                .iter()
                .any(|c| c.what.contains("failed (E1") && c.what.ends_with("back to 1.0.0")),
            "{history:?}"
        );

        // With none: its worker stops, and no longer starts with its owner.
        let (_home, owner, _f) = deployed(&["fin-analyst"], false);
        let (record, settled) =
            settle_canary(&owner, "fin-analyst", "fin-analyst", failed).unwrap();
        assert_eq!(
            (settled, record.state),
            (Settled::Stop, DeployState::Stopped)
        );
        let worker = crate::workers::worker_of(&owner, "fin-analyst").unwrap();
        assert!(!worker.config.start_with_owner);
        let stopped = Deployed::of(&worker, None).unwrap();
        let task = LlmTask::new(Provider::Copilot, "hi");
        assert_eq!(stopped.admit(&task, true).unwrap_err().0, "stopped");

        // A canary that passed: it takes work.
        let (_home, owner, _f) = deployed(&["fin-analyst"], true);
        let passed = Canary {
            passed: true,
            cases: 1,
            ..Canary::default()
        };
        let (record, _) = settle_canary(&owner, "fin-analyst", "fin-analyst", passed).unwrap();
        assert_eq!(record.state, DeployState::Running);
        assert_eq!(pinned_on(&owner, "fin-analyst").state, DeployState::Running);
    }

    #[test]
    fn a_rollback_pins_the_release_before_on_every_deployment_and_is_audited() {
        let (_home, owner, _f) = deployed(&["fin-a", "fin-b"], true);
        let store = FactoryStore::of(&owner);
        assert_eq!(
            store
                .state("fin-analyst")
                .unwrap()
                .unwrap()
                .active_version
                .as_deref(),
            Some("1.0.1")
        );
        let worker = crate::workers::worker_of(&owner, "fin-a").unwrap();
        let running = Deployed::of(&worker, None).unwrap();
        assert_eq!(running.current().unwrap().version, "1.0.1");
        let (state, workers) = rollback(
            &owner,
            "fin-analyst",
            None,
            "Summaries got longer than 200 words",
            "alice",
        )
        .unwrap();
        assert_eq!(state.active_version.as_deref(), Some("1.0.0"));
        assert_eq!(workers, ["fin-a", "fin-b"]);
        for name in ["fin-a", "fin-b"] {
            let pinned = pinned_on(&owner, name);
            assert_eq!(
                (pinned.version.as_str(), pinned.state),
                ("1.0.0", DeployState::Running)
            );
        }
        // A running worker takes it at its next look at its pin.
        assert!(running.reload());
        assert_eq!(running.current().unwrap().version, "1.0.0");
        let audit = store.audit_lines(Some("fin-analyst")).unwrap();
        assert!(
            audit
                .iter()
                .any(|l| l.what == "rolled back from 1.0.1 to 1.0.0"
                    && l.why.as_deref() == Some("Summaries got longer than 200 words")),
            "{audit:?}"
        );
        let none = rollback(&owner, "fin-analyst", None, "again", "alice").unwrap_err();
        assert!(
            format!("{none:#}").contains("has no release before 1.0.0"),
            "{none:#}"
        );
        assert!(
            rollback(&owner, "fin-analyst", None, " ", "alice").is_err(),
            "a reason is needed"
        );
    }

    /// A worker being deleted ends its deployment: stopped, saying why in the agent's
    /// history and the audit log, once. With the worker gone a rollback passes it by, and
    /// the agent's other deployments stay as they were.
    #[test]
    fn a_deleted_workers_deployment_ends_and_a_rollback_passes_it_by() {
        let (_home, owner, _f) = deployed(&["fin-a", "fin-b"], true);
        assert_eq!(
            worker_deleted(&owner, "fin-a", "alice").unwrap(),
            ["fin-analyst"]
        );
        let store = FactoryStore::of(&owner);
        let state = store.state("fin-analyst").unwrap().unwrap();
        let ended = state.deployment("fin-a").unwrap();
        assert_eq!(
            (ended.state, ended.note.as_deref()),
            (DeployState::Stopped, Some("its worker was deleted"))
        );
        assert_eq!(
            state.deployment("fin-b").unwrap().state,
            DeployState::Canary
        );
        let audit = store.audit_lines(Some("fin-analyst")).unwrap();
        assert!(
            audit.iter().any(
                |l| l.what == "deployment on fin-a ended: its worker was deleted"
                    && l.by == "alice"
            ),
            "{audit:?}"
        );
        assert!(worker_deleted(&owner, "fin-a", "alice").unwrap().is_empty());

        let gone = crate::workers::worker_of(&owner, "fin-a").unwrap().dir;
        std::fs::remove_dir_all(gone).unwrap();
        let (_, workers) = rollback(
            &owner,
            "fin-analyst",
            None,
            "Summaries got longer than 200 words",
            "alice",
        )
        .unwrap();
        assert_eq!(workers, ["fin-b"]);
    }
}
