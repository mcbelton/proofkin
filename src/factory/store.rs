//! The factory's store in a profile (`<profile>/factory/`, private to its owner):
//!
//! ```text
//! factory/
//!   drafts/<name>.json          an agent's draft: its blueprint, as written or edited, and how it was first written
//!   agents/<name>/state.json    its lifecycle: state, active version, deployments, and the history of every change
//!   agents/<name>/fixtures/     files its eval cases work on
//!   agents/<name>/builds/<hash>/  its builds, by hash (see crate::factory::build)
//!   agents/<name>/evaluations/<id>.json  its evaluations: each one's journal while it runs, its signed report when it ends
//!   agents/<name>/releases/<version>/    its releases: the build's files, its report and the signed manifest (see crate::factory::release)
//!   evaluation/                 the evaluation that runs (one at a time): its folder, and the one next to it
//!   audit.jsonl                 every lifecycle change of every agent, one line each
//! ```
//!
//! Files are written atomically, and changes to an agent hold its lock
//! ([`crate::store`]), so the CLI, the web UI and an agent can work at once without
//! losing a change. A file that can't be read is reported with its path and left as it
//! is, never replaced.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::factory::build::Build;
use crate::factory::report::{self, Evaluation};
use crate::llm::Provider;
use crate::profile::{Profile, validate_name};
use crate::store::{FileLock, write_atomic};

/// The lock of the evaluation that runs (see [`FactoryStore::lock_evaluation`]).
#[derive(Debug)]
pub struct EvaluationLock {
    _file: std::fs::File,
}

/// An agent's draft: its blueprint, which may still have problems, and how it was
/// first written.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Draft {
    /// The blueprint, as written or last edited (JSON, checked when it is shown or used).
    pub blueprint: Value,
    pub provenance: Provenance,
    /// How many tries the model needed (0: no model wrote it).
    #[serde(default)]
    pub tries: u32,
}

/// How a draft was first written: from a template, by a model, or from a file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    /// `template`, `model` or `file`.
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<Provider>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// The SHA-256 of the description the model was given; never its text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description_sha256: Option<String>,
    /// When (RFC 3339, UTC).
    pub at: String,
}

/// Where an agent is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lifecycle {
    /// Not released yet.
    Draft,
    /// Released: plans and deployments use its active version.
    Active,
    /// Plans are warned and pointed to its replacement.
    Deprecated,
    /// Plans refuse it; its releases are kept for audit.
    Retired,
    /// Published by a peer, not yet evaluated here.
    Imported,
}

impl std::fmt::Display for Lifecycle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.pad(match self {
            Lifecycle::Draft => "draft",
            Lifecycle::Active => "active",
            Lifecycle::Deprecated => "deprecated",
            Lifecycle::Retired => "retired",
            Lifecycle::Imported => "imported",
        })
    }
}

/// An agent's lifecycle and history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentState {
    pub name: String,
    pub state: Lifecycle,
    /// The release `agent=name` uses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_version: Option<String>,
    /// Its releases, newest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub versions: Vec<String>,
    /// The digital workers that run it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deployments: Vec<DeploymentRecord>,
    /// The agent plans are pointed to, once it is deprecated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replacement: Option<String>,
    /// Once retired with `--purge`: when its releases, reports and builds may go.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purge_after: Option<String>,
    /// Its drift watch: its active release evaluated on its deployment on a schedule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watch: Option<DriftWatch>,
    /// The releases its owner publishes to the peers granted it (see
    /// [`crate::factory::catalog`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publication: Option<Publication>,
    /// For an agent imported from a peer: whose it is, and the versions imported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub imported: Option<ImportOrigin>,
    /// Every change, oldest first.
    #[serde(default)]
    pub history: Vec<Change>,
}

impl AgentState {
    fn new(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            state: Lifecycle::Draft,
            active_version: None,
            versions: Vec::new(),
            deployments: Vec::new(),
            replacement: None,
            purge_after: None,
            watch: None,
            publication: None,
            imported: None,
            history: Vec::new(),
        }
    }

    /// Its deployment on worker `worker`, if it has one.
    pub fn deployment(&self, worker: &str) -> Option<&DeploymentRecord> {
        self.deployments.iter().find(|d| d.worker == worker)
    }

    pub fn deployment_mut(&mut self, worker: &str) -> Option<&mut DeploymentRecord> {
        self.deployments.iter_mut().find(|d| d.worker == worker)
    }

    /// Records a change: who, what and why, now.
    pub fn changed(&mut self, by: &str, what: impl Into<String>, why: Option<&str>) {
        self.history.push(Change {
            at: now(),
            by: by.to_owned(),
            what: what.into(),
            why: why.map(str::to_owned),
        });
    }
}

/// Where a deployment is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DeployState {
    /// Made, its worker not started yet.
    Starting,
    /// Its worker runs the release's smoke cases before it takes work.
    Canary,
    /// It takes work.
    Running,
    /// Its canary failed: it went back to the release before.
    RolledBack,
    /// Its worker is stopped: its canary failed with no release to go back to, or the
    /// agent was retired.
    Stopped,
}

impl std::fmt::Display for DeployState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.pad(match self {
            DeployState::Starting => "starting",
            DeployState::Canary => "canary",
            DeployState::Running => "running",
            DeployState::RolledBack => "rolled-back",
            DeployState::Stopped => "stopped",
        })
    }
}

/// What a deployment's canary found.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Canary {
    pub passed: bool,
    /// How many smoke cases it ran.
    pub cases: usize,
    /// The smoke cases that failed, each with why.
    #[serde(default)]
    pub failed: Vec<String>,
    /// The evaluation that ran them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evaluation: Option<String>,
}

/// A digital worker that runs a release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeploymentRecord {
    pub worker: String,
    pub version: String,
    pub state: DeployState,
    /// The release it ran before this one, which a failed canary goes back to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canary: Option<Canary>,
    /// The shared folders it works in.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub folders: Vec<String>,
    /// Its budgets, where its owner changed the release's (see [`crate::factory::deploy`]).
    #[serde(
        default,
        skip_serializing_if = "crate::factory::blueprint::Budgets::is_empty"
    )]
    pub budgets: crate::factory::blueprint::Budgets,
    /// Why its last change happened (a rollback's reason, a failed canary's cases).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// When it last changed.
    #[serde(default)]
    pub at: String,
}

/// A change to an agent: who, when, what and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    pub at: String,
    pub by: String,
    pub what: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
}

/// A drift watch (see [`crate::factory::drift`]): when its evaluations run, how many failed
/// runs in a row open an incident, and where it is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriftWatch {
    /// When its evaluations run, as schedules say it (`daily 03:00`, `every 6h`).
    pub when: String,
    /// Failed runs in a row that open an incident, 1 to 10.
    pub fail_after: u32,
    /// A paused watch runs nothing.
    pub enabled: bool,
    /// When its schedule counts from (it was set or resumed), unless it ran later.
    pub since: String,
    /// Its last run: passed, failed, or why it was skipped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<DriftRun>,
    /// Failed runs in a row.
    #[serde(default)]
    pub failures: u32,
    /// The ops incident it opened, while it is open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incident: Option<u64>,
    /// A due run that couldn't start (another evaluation ran): when it is tried again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<String>,
}

/// How a drift watch's run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DriftOutcome {
    /// Its evaluation passed the release's gate.
    Passed,
    /// It didn't.
    Failed,
    /// It didn't run (no deployment runs the active release, its budget is spent...).
    Skipped,
}

impl std::fmt::Display for DriftOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.pad(match self {
            DriftOutcome::Passed => "passed",
            DriftOutcome::Failed => "failed",
            DriftOutcome::Skipped => "skipped",
        })
    }
}

/// One run of a drift watch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriftRun {
    pub at: String,
    /// The release it evaluated, and the worker it ran on.
    pub version: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub worker: String,
    pub outcome: DriftOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evaluation: Option<String>,
    /// The runs of the agent's own cases that passed, and of the built-in attacks.
    #[serde(default)]
    pub own: report::Tally,
    #[serde(default)]
    pub adversarial: report::Tally,
    /// The cases that failed, each with why.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed: Vec<String>,
    /// Why it was skipped, or what its gate missed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// The releases an agent's owner publishes: the peers granted the agent on the allowlist
/// see these versions in their catalog, and may import them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Publication {
    /// Released versions, newest first.
    pub versions: Vec<String>,
    pub at: String,
}

/// Where an imported agent comes from: its publisher, as this profile's allowlist names it
/// and by key, and each version imported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportOrigin {
    pub publisher: String,
    pub publisher_id: String,
    /// Newest first.
    pub versions: Vec<ImportedVersion>,
}

/// A version imported: the bundle kept under `factory/imported/`, verified.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportedVersion {
    pub version: String,
    /// The publisher's manifest hash: what was imported, exactly.
    pub manifest_sha256: String,
    /// Its build's hash: a local release of exactly this build makes it active.
    pub build: String,
    pub at: String,
    /// The publisher's report was accepted instead of an evaluation here.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub accepted_report: bool,
}

/// A lifecycle change as the factory's audit log keeps it (`factory/audit.jsonl`): every
/// release, sign-off, deployment, rollback, budget, deprecation and retirement, with no
/// text of any task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditLine {
    pub at: String,
    pub agent: String,
    pub by: String,
    pub what: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
}

/// One of the factory's agents, as listed.
#[derive(Debug, Clone, PartialEq)]
pub struct Listed {
    pub name: String,
    pub state: Lifecycle,
    /// Its draft's title, if it has a draft that names one.
    pub title: Option<String>,
    /// When it last changed.
    pub changed: Option<String>,
    /// Its draft, if it has one.
    pub draft: Option<Draft>,
}

/// Now, as the store writes times: RFC 3339 in UTC, to the second.
pub fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// A profile's factory store.
#[derive(Debug, Clone)]
pub struct FactoryStore {
    root: PathBuf,
}

impl FactoryStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// `profile`'s store (`<profile>/factory`).
    pub fn of(profile: &Profile) -> Self {
        Self::new(profile.dir.join("factory"))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn draft_path(&self, name: &str) -> PathBuf {
        self.root.join("drafts").join(format!("{name}.json"))
    }

    pub fn agent_dir(&self, name: &str) -> PathBuf {
        self.root.join("agents").join(name)
    }

    pub fn state_path(&self, name: &str) -> PathBuf {
        self.agent_dir(name).join("state.json")
    }

    /// The folder agent `name`'s eval cases take their fixtures from.
    pub fn fixtures_dir(&self, name: &str) -> PathBuf {
        self.agent_dir(name).join("fixtures")
    }

    /// Where agent `name`'s build `hash` is kept.
    pub fn build_dir(&self, name: &str, hash: &str) -> PathBuf {
        self.agent_dir(name).join("builds").join(hash)
    }

    /// Keeps `build` by its hash and returns its folder. A build kept already is left as it
    /// is once it checks out against its hash; one that doesn't is an error naming it.
    pub fn save_build(&self, build: &Build) -> Result<PathBuf> {
        validate_agent_name(&build.agent)?;
        self.prepare()?;
        let _lock = FileLock::acquire(&self.state_path(&build.agent))?;
        let dir = self.build_dir(&build.agent, &build.hash);
        if dir.exists() {
            self.build(&build.agent, &build.hash)?;
            return Ok(dir);
        }
        let builds = dir.parent().expect("a build's folder is in builds/");
        std::fs::create_dir_all(builds)
            .with_context(|| format!("creating {}", builds.display()))?;
        // Written whole, then renamed into place: a build is there complete, or not at all.
        let partial = builds.join(format!(".partial-{}", uuid::Uuid::new_v4().simple()));
        let written = (|| -> Result<()> {
            for (path, text) in &build.files {
                let file = partial.join(path);
                if let Some(parent) = file.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&file, text)?;
            }
            rename_dir(&partial, &dir)
        })();
        if written.is_err() {
            let _ = std::fs::remove_dir_all(&partial);
        }
        written.with_context(|| {
            format!(
                "keeping build {} of {} in {}",
                build.short_hash(),
                build.agent,
                builds.display()
            )
        })?;
        Ok(dir)
    }

    /// Agent `name`'s build `hash`, if it is kept, checked against its hash: one changed
    /// since it was built is an error naming its folder.
    pub fn build(&self, name: &str, hash: &str) -> Result<Option<Build>> {
        validate_agent_name(name)?;
        if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("{hash:?} isn't a build's hash");
        }
        let dir = self.build_dir(name, hash);
        if !dir.is_dir() {
            return Ok(None);
        }
        let mut files = std::collections::BTreeMap::new();
        read_tree(&dir, "", &mut files)?;
        let build = Build::of(name, files);
        if build.hash != hash {
            bail!(
                "{} doesn't match its hash: it was changed after it was built; remove it to build it again",
                dir.display()
            );
        }
        Ok(Some(build))
    }

    /// The hashes of agent `name`'s builds, sorted.
    pub fn builds(&self, name: &str) -> Result<Vec<String>> {
        validate_agent_name(name)?;
        let mut hashes: Vec<String> = read_dir(&self.agent_dir(name).join("builds"))?
            .into_iter()
            .filter(|e| e.path().is_dir())
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.len() == 64 && n.bytes().all(|b| b.is_ascii_hexdigit()))
            .collect();
        hashes.sort();
        Ok(hashes)
    }

    /// Where the evaluation that runs works (see [`crate::factory::runner`]).
    pub fn evaluation_dir(&self) -> PathBuf {
        self.root.join("evaluation")
    }

    /// Takes the lock only one running evaluation holds at a time, released when dropped
    /// (or when its process ends, however it ends). Fails while another one holds it.
    pub fn lock_evaluation(&self) -> Result<EvaluationLock> {
        self.prepare()?;
        let dir = self.evaluation_dir();
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let path = dir.join("lock");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        match file.try_lock() {
            Ok(()) => Ok(EvaluationLock { _file: file }),
            Err(std::fs::TryLockError::WouldBlock) => {
                bail!("another evaluation is running; wait for it to end, or cancel it")
            }
            Err(std::fs::TryLockError::Error(e)) => {
                Err(e).with_context(|| format!("locking {}", path.display()))
            }
        }
    }

    /// Whether an evaluation runs now, in any process.
    pub fn evaluation_running(&self) -> bool {
        std::fs::File::open(self.evaluation_dir().join("lock"))
            .is_ok_and(|file| matches!(file.try_lock(), Err(std::fs::TryLockError::WouldBlock)))
    }

    pub fn evaluations_dir(&self, name: &str) -> PathBuf {
        self.agent_dir(name).join("evaluations")
    }

    pub fn evaluation_path(&self, name: &str, id: &str) -> PathBuf {
        self.evaluations_dir(name).join(format!("{id}.json"))
    }

    /// Keeps `evaluation`: its journal while it runs, its report when it ends.
    pub fn save_evaluation(&self, evaluation: &Evaluation) -> Result<()> {
        validate_agent_name(&evaluation.agent)?;
        if report::agent_of(&evaluation.id) != Some(evaluation.agent.as_str()) {
            bail!(
                "{:?} isn't an id of an evaluation of {}",
                evaluation.id,
                evaluation.agent
            );
        }
        self.prepare()?;
        write_json(
            &self.evaluation_path(&evaluation.agent, &evaluation.id),
            evaluation,
        )
    }

    /// Evaluation `id` (its agent is in its id), if it is kept. The outputs of an ended
    /// one's runs go after [`report::OUTPUT_DAYS`] days, when it is read; their hashes stay.
    pub fn evaluation(&self, id: &str) -> Result<Option<Evaluation>> {
        let agent = report::agent_of(id)
            .ok_or_else(|| anyhow::anyhow!("{id:?} isn't an evaluation's id (ev-...)"))?;
        let path = self.evaluation_path(agent, id);
        let Some(mut evaluation) = read_json::<Evaluation>(&path)? else {
            return Ok(None);
        };
        if evaluation.state != report::EvalState::Running
            && evaluation.prune_outputs(chrono::Utc::now())
        {
            write_json(&path, &evaluation)?;
        }
        Ok(Some(evaluation))
    }

    /// Agent `name`'s evaluations, newest first.
    pub fn evaluations(&self, name: &str) -> Result<Vec<Evaluation>> {
        validate_agent_name(name)?;
        let mut ids: Vec<String> = read_dir(&self.evaluations_dir(name))?
            .into_iter()
            .filter_map(|e| e.file_name().into_string().ok())
            .filter_map(|f| f.strip_suffix(".json").map(str::to_owned))
            .filter(|id| report::agent_of(id) == Some(name))
            .collect();
        // Ids sort by when they started.
        ids.sort_by(|a, b| b.cmp(a));
        ids.iter()
            .filter_map(|id| self.evaluation(id).transpose())
            .collect()
    }

    /// Agent `name`'s draft, if it has one.
    pub fn draft(&self, name: &str) -> Result<Option<Draft>> {
        validate_agent_name(name)?;
        read_json(&self.draft_path(name))
    }

    /// Agent `name`'s lifecycle and history, if it is one of the factory's.
    pub fn state(&self, name: &str) -> Result<Option<AgentState>> {
        validate_agent_name(name)?;
        read_json(&self.state_path(name))
    }

    /// Makes a new agent `name` from `draft`; `by` made it, as `what` says. Fails if the
    /// factory has an agent or draft of that name.
    pub fn create_draft(&self, name: &str, draft: &Draft, by: &str, what: &str) -> Result<()> {
        validate_agent_name(name)?;
        self.prepare()?;
        let _lock = FileLock::acquire(&self.state_path(name))?;
        if self.draft_path(name).exists() || self.state_path(name).exists() {
            bail!(
                "the factory already has an agent or draft '{name}' (proofkin factory show {name})"
            );
        }
        self.write_draft(name, draft, by, what)
    }

    /// Saves `draft` as agent `name`'s draft, made or changed by `by` as `what` says
    /// (recorded in its history). A new agent starts as a draft.
    pub fn save_draft(&self, name: &str, draft: &Draft, by: &str, what: &str) -> Result<()> {
        validate_agent_name(name)?;
        self.prepare()?;
        let _lock = FileLock::acquire(&self.state_path(name))?;
        self.write_draft(name, draft, by, what)
    }

    /// Writes the draft and its history entry; the caller holds the agent's lock.
    fn write_draft(&self, name: &str, draft: &Draft, by: &str, what: &str) -> Result<()> {
        let mut state = read_json(&self.state_path(name))?.unwrap_or_else(|| AgentState::new(name));
        write_json(&self.draft_path(name), draft)?;
        state.history.push(Change {
            at: now(),
            by: by.to_owned(),
            what: what.to_owned(),
            why: None,
        });
        write_json(&self.state_path(name), &state)
    }

    /// Changes agent `name`'s state with `change`, under its lock, and returns it.
    pub fn update_state(
        &self,
        name: &str,
        change: impl FnOnce(&mut AgentState) -> Result<()>,
    ) -> Result<AgentState> {
        validate_agent_name(name)?;
        self.prepare()?;
        let _lock = FileLock::acquire(&self.state_path(name))?;
        let mut state = read_json(&self.state_path(name))?.unwrap_or_else(|| AgentState::new(name));
        let before = state.history.len();
        change(&mut state)?;
        write_json(&self.state_path(name), &state)?;
        // What changed goes to the factory's audit log too.
        for change in &state.history[before.min(state.history.len())..] {
            self.audit(&AuditLine {
                at: change.at.clone(),
                agent: name.to_owned(),
                by: change.by.clone(),
                what: change.what.clone(),
                why: change.why.clone(),
            });
        }
        Ok(state)
    }

    /// The factory's audit log: every lifecycle change of its agents, one JSON line each.
    pub fn audit_path(&self) -> PathBuf {
        self.root.join("audit.jsonl")
    }

    /// Appends `line` to the audit log; a failure is reported, never failing the change.
    pub fn audit(&self, line: &AuditLine) {
        let written = (|| -> Result<()> {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.audit_path())?;
            let mut bytes = serde_json::to_vec(line)?;
            bytes.push(b'\n');
            file.write_all(&bytes)?;
            Ok(())
        })();
        if let Err(e) = written {
            eprintln!(
                "warning: could not write {}: {e:#}",
                self.audit_path().display()
            );
        }
    }

    /// The audit log's lines about agent `name` (every agent's with `None`), oldest first.
    pub fn audit_lines(&self, name: Option<&str>) -> Result<Vec<AuditLine>> {
        let text = match std::fs::read_to_string(self.audit_path()) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => {
                return Err(e).with_context(|| format!("reading {}", self.audit_path().display()));
            }
        };
        Ok(text
            .lines()
            .filter_map(|l| serde_json::from_str::<AuditLine>(l).ok())
            .filter(|l| name.is_none_or(|n| l.agent == n))
            .collect())
    }

    /// Where agent `name`'s releases are kept, a folder each.
    pub fn releases_dir(&self, name: &str) -> PathBuf {
        self.agent_dir(name).join("releases")
    }

    /// Where agent `name`'s release `version` is kept (see [`crate::factory::release`]).
    pub fn release_dir(&self, name: &str, version: &str) -> PathBuf {
        self.releases_dir(name).join(version)
    }

    /// The versions agent `name` has a release folder for, whatever their state.
    pub fn release_versions(&self, name: &str) -> Result<Vec<String>> {
        validate_agent_name(name)?;
        Ok(read_dir(&self.releases_dir(name))?
            .into_iter()
            .filter(|e| {
                e.path()
                    .join(crate::factory::release::RELEASE_FILE)
                    .is_file()
            })
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|v| v.parse::<crate::factory::release::Version>().is_ok())
            .collect())
    }

    /// Keeps a new release of agent `name`: its folder written whole from `files` (paths
    /// in it, `/`-separated), then renamed into place. Fails if `version` has a folder.
    pub fn save_release(
        &self,
        name: &str,
        version: &str,
        files: &std::collections::BTreeMap<String, Vec<u8>>,
    ) -> Result<PathBuf> {
        validate_agent_name(name)?;
        self.prepare()?;
        let dir = self.release_dir(name, version);
        if dir.exists() {
            bail!("{version} is already a release of {name}");
        }
        let releases = self.releases_dir(name);
        std::fs::create_dir_all(&releases)
            .with_context(|| format!("creating {}", releases.display()))?;
        let partial = releases.join(format!(".partial-{}", uuid::Uuid::new_v4().simple()));
        let written = (|| -> Result<()> {
            for (path, bytes) in files {
                let file = partial.join(path);
                if let Some(parent) = file.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&file, bytes)?;
            }
            rename_dir(&partial, &dir)
        })();
        if written.is_err() {
            let _ = std::fs::remove_dir_all(&partial);
        }
        written.with_context(|| format!("keeping release {version} of {name}"))?;
        Ok(dir)
    }

    /// Deletes agent `name`'s draft and records (`false` if it had neither), and an import's
    /// bundles. An agent with releases isn't deleted, so its records are kept: it is to be
    /// retired.
    pub fn delete(&self, name: &str) -> Result<bool> {
        validate_agent_name(name)?;
        let _lock = FileLock::acquire(&self.state_path(name))?;
        let state = read_json::<AgentState>(&self.state_path(name))?;
        if let Some(state) = &state
            && !state.versions.is_empty()
        {
            bail!(
                "'{name}' has releases ({}): retire it instead, so its records are kept",
                state.versions.join(", ")
            );
        }
        let draft = self.draft_path(name);
        let existed = draft.exists() || self.state_path(name).exists();
        if draft.exists() {
            std::fs::remove_file(&draft)
                .with_context(|| format!("deleting {}", draft.display()))?;
        }
        // Nothing of an import ran: its bundles go with it.
        if let Some(origin) = state.as_ref().and_then(|s| s.imported.as_ref())
            && validate_name(&origin.publisher).is_ok()
        {
            let bundles = self
                .root
                .join(crate::factory::catalog::IMPORTED_DIR)
                .join(&origin.publisher)
                .join(name);
            if bundles.exists() {
                std::fs::remove_dir_all(&bundles)
                    .with_context(|| format!("deleting {}", bundles.display()))?;
            }
        }
        // Its folder goes too, with the lock file taking the lock made in it.
        let dir = self.agent_dir(name);
        if dir.exists() {
            std::fs::remove_dir_all(&dir).with_context(|| format!("deleting {}", dir.display()))?;
        }
        Ok(existed)
    }

    /// The names of the factory's agents and drafts, sorted.
    pub fn names(&self) -> Result<Vec<String>> {
        let mut names = std::collections::BTreeSet::new();
        for entry in read_dir(&self.root.join("drafts"))? {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "json")
                && let Some(name) = path.file_stem().and_then(|s| s.to_str())
                && validate_name(name).is_ok()
            {
                names.insert(name.to_owned());
            }
        }
        for entry in read_dir(&self.root.join("agents"))? {
            if entry.path().join("state.json").is_file()
                && let Some(name) = entry.file_name().to_str()
                && validate_name(name).is_ok()
            {
                names.insert(name.to_owned());
            }
        }
        Ok(names.into_iter().collect())
    }

    /// The factory's agents, sorted by name, each with its draft.
    pub fn list(&self) -> Result<Vec<Listed>> {
        self.names()?
            .into_iter()
            .map(|name| {
                let draft = self.draft(&name)?;
                let state = self.state(&name)?;
                Ok(Listed {
                    title: draft
                        .as_ref()
                        .and_then(|d| d.blueprint.get("title"))
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    state: state.as_ref().map_or(Lifecycle::Draft, |s| s.state),
                    changed: state.and_then(|s| s.history.last().map(|c| c.at.clone())),
                    draft,
                    name,
                })
            })
            .collect()
    }

    /// Creates the store's folder, private to its owner.
    fn prepare(&self) -> Result<()> {
        std::fs::create_dir_all(&self.root)
            .with_context(|| format!("creating {}", self.root.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.root, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }
}

pub(crate) fn validate_agent_name(name: &str) -> Result<()> {
    validate_name(name).map_err(|_| {
        anyhow::anyhow!(
            "{name:?} can't name an agent: use letters, digits, '-', '_' and '.' (at most 64)"
        )
    })
}

pub(crate) fn read_dir(dir: &Path) -> Result<Vec<std::fs::DirEntry>> {
    match std::fs::read_dir(dir) {
        Ok(entries) => Ok(entries.flatten().collect()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e).with_context(|| format!("reading {}", dir.display())),
    }
}

/// The text files under `dir`, by their `/`-separated paths below it (after `prefix`).
pub(crate) fn read_tree(
    dir: &Path,
    prefix: &str,
    files: &mut std::collections::BTreeMap<String, String>,
) -> Result<()> {
    for entry in read_dir(dir)? {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        let relative = format!("{prefix}{name}");
        if entry.file_type()?.is_dir() {
            read_tree(&path, &format!("{relative}/"), files)?;
        } else {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            files.insert(relative, text);
        }
    }
    Ok(())
}

/// Renames folder `from` to `to`, trying again for a moment while Windows still holds the
/// files just written in it.
pub(crate) fn rename_dir(from: &Path, to: &Path) -> Result<()> {
    let mut tries = 0;
    loop {
        match std::fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e) if tries < 20 && e.kind() == std::io::ErrorKind::PermissionDenied => {
                tries += 1;
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => return Err(e.into()),
        }
    }
}

/// The JSON file at `path` (`None` if there is none). One that can't be read or
/// parsed is an error naming it, so it is never written over by mistake.
pub(crate) fn read_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    serde_json::from_str(&text).map(Some).map_err(|e| {
        anyhow::anyhow!(
            "{} is damaged ({e}): fix or remove it; it was left as it is",
            path.display()
        )
    })
}

pub(crate) fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    write_atomic(
        path,
        (serde_json::to_string_pretty(value)? + "\n").as_bytes(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn draft(title: &str) -> Draft {
        Draft {
            blueprint: json!({ "schema": crate::factory::blueprint::SCHEMA, "name": "fin", "title": title }),
            provenance: Provenance {
                source: "template".into(),
                template: Some("financial-analyst".into()),
                at: now(),
                ..Provenance::default()
            },
            tries: 0,
        }
    }

    #[test]
    fn drafts_and_their_history_survive_a_restart_and_concurrent_writers() {
        let dir = tempfile::tempdir().unwrap();
        let store = FactoryStore::new(dir.path().join("factory"));
        store
            .create_draft(
                "fin",
                &draft("v0"),
                "alice",
                "drafted from template financial-analyst",
            )
            .unwrap();
        let writers: Vec<_> = (1..=8)
            .map(|i| {
                let store = store.clone();
                std::thread::spawn(move || {
                    store
                        .save_draft(
                            "fin",
                            &draft(&format!("v{i}")),
                            "alice",
                            &format!("edit {i}"),
                        )
                        .unwrap();
                })
            })
            .collect();
        for writer in writers {
            writer.join().unwrap();
        }
        // A new store on the same folder: as after a restart.
        let again = FactoryStore::new(dir.path().join("factory"));
        let state = again.state("fin").unwrap().unwrap();
        assert_eq!(state.state, Lifecycle::Draft);
        assert_eq!(state.history.len(), 9, "no change was lost");
        assert_eq!(
            state.history[0].what,
            "drafted from template financial-analyst"
        );
        let mut edits: Vec<&str> = state.history[1..].iter().map(|c| c.what.as_str()).collect();
        edits.sort();
        assert_eq!(
            edits,
            (1..=8).map(|i| format!("edit {i}")).collect::<Vec<_>>()
        );
        let title = again.draft("fin").unwrap().unwrap().blueprint["title"].clone();
        assert_eq!(
            title,
            json!(state.history.last().unwrap().what.replace("edit ", "v"))
        );
        assert_eq!(again.names().unwrap(), ["fin"]);
        let listed = again.list().unwrap();
        assert_eq!(listed[0].state, Lifecycle::Draft);
        assert!(listed[0].title.is_some() && listed[0].changed.is_some());
    }

    #[test]
    fn a_damaged_file_is_reported_with_its_path_and_left_as_it_is() {
        let dir = tempfile::tempdir().unwrap();
        let store = FactoryStore::new(dir.path());
        store
            .create_draft("fin", &draft("v0"), "alice", "drafted")
            .unwrap();
        let state = store.state_path("fin");
        std::fs::write(&state, "{ not json").unwrap();
        let err = store
            .save_draft("fin", &draft("v1"), "alice", "edit")
            .unwrap_err();
        assert!(
            err.to_string().contains(&state.display().to_string()),
            "{err}"
        );
        assert!(err.to_string().contains("damaged"), "{err}");
        assert_eq!(std::fs::read_to_string(&state).unwrap(), "{ not json");
        assert_eq!(
            store.draft("fin").unwrap().unwrap().blueprint["title"],
            json!("v0"),
            "the draft wasn't changed either"
        );
        std::fs::write(store.draft_path("fin"), "[").unwrap();
        let err = store.draft("fin").unwrap_err();
        assert!(err.to_string().contains("fin.json"), "{err}");
    }

    #[test]
    fn builds_are_kept_by_hash_and_checked_when_read() {
        let dir = tempfile::tempdir().unwrap();
        let store = FactoryStore::new(dir.path().join("factory"));
        let files = [
            (
                "agent.md".to_owned(),
                "---\nname: fin\n---\nDo it.\n".to_owned(),
            ),
            ("skills/fin.x.md".to_owned(), "x\n".to_owned()),
        ]
        .into();
        let build = Build::of("fin", files);
        let kept = store.save_build(&build).unwrap();
        assert_eq!(kept, store.build_dir("fin", &build.hash));
        assert_eq!(store.save_build(&build).unwrap(), kept, "kept once");
        assert_eq!(
            store.build("fin", &build.hash).unwrap(),
            Some(build.clone())
        );
        assert_eq!(
            store.builds("fin").unwrap(),
            std::slice::from_ref(&build.hash)
        );
        assert_eq!(store.build("fin", &"0".repeat(64)).unwrap(), None);
        assert!(store.build("fin", "../x").is_err());

        std::fs::write(kept.join("skills").join("fin.x.md"), "changed\n").unwrap();
        let error = store.build("fin", &build.hash).unwrap_err().to_string();
        assert!(error.contains("doesn't match its hash"), "{error}");
        assert!(
            store.save_build(&build).is_err(),
            "a changed build is reported, never written over"
        );
        assert_eq!(
            std::fs::read_to_string(kept.join("skills").join("fin.x.md")).unwrap(),
            "changed\n"
        );
    }

    #[test]
    fn a_name_has_one_agent_and_deleting_keeps_released_ones() {
        let dir = tempfile::tempdir().unwrap();
        let store = FactoryStore::new(dir.path());
        assert!(
            store.names().unwrap().is_empty(),
            "an empty store lists nothing"
        );
        store
            .create_draft("fin", &draft("v0"), "alice", "drafted")
            .unwrap();
        let again = store
            .create_draft("fin", &draft("v1"), "alice", "drafted")
            .unwrap_err();
        assert!(again.to_string().contains("already has"), "{again}");
        assert!(
            store
                .create_draft("../x", &draft("v0"), "alice", "drafted")
                .is_err()
        );
        assert!(store.draft("a/b").is_err());

        store
            .create_draft("old", &draft("v0"), "alice", "drafted")
            .unwrap();
        store
            .update_state("old", |s| {
                s.state = Lifecycle::Active;
                s.versions.push("1.0.0".into());
                Ok(())
            })
            .unwrap();
        let kept = store.delete("old").unwrap_err();
        assert!(kept.to_string().contains("retire it instead"), "{kept}");

        assert!(store.delete("fin").unwrap());
        assert!(!store.delete("fin").unwrap());
        assert_eq!(store.names().unwrap(), ["old"]);
    }
}
