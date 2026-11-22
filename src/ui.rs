//! Local web UI (`proofkin --name me --ui`): a single-page app served on
//! 127.0.0.1 only, guarded by a random access token and a Host check, with live
//! updates over Server-Sent Events.

use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use futures_util::Stream;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc};
use tokio::task::AbortHandle;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

use crate::agent::{Agent, Event, Network};
use crate::audit::{AuditLog, Outcome};
use crate::llm::{Access, Executor, LlmTask, Provider, TaskContext};
use crate::plan::{Plan, SquadStore, StepState};
use crate::profile::{Profile, validate_name};
use crate::runs::RunRecord;
use crate::trust::{Ticket, TrustedPeer};

mod connect;
mod tools;
mod workers;

const INDEX_HTML: &str = include_str!("../ui/index.html");
const APP_CSS: &str = include_str!("../ui/app.css");
const APP_JS: &str = include_str!("../ui/app.js");
const TOOLS_JS: &str = include_str!("../ui/tools.js");
const PLAN_TEXT_JS: &str = include_str!("../ui/plan-text.js");
const PLAN_BUILDER_JS: &str = include_str!("../ui/plan-builder.js");
const WORKERS_JS: &str = include_str!("../ui/workers.js");
const CONNECT_JS: &str = include_str!("../ui/connect.js");
const FEED_LIMIT: usize = 500;
/// Most tasks kept for the page (and for other agents' lookups), besides running ones:
/// the oldest finished ones go first, a plan's steps together once the plan has ended.
const TASK_LIMIT: usize = 500;
/// The latest tasks kept in `tasks.json` across restarts, and how much of each output.
const KEPT_TASKS: usize = 200;
const KEPT_OUTPUT: usize = 32_000;
/// Why a task that was running when the agent stopped shows as cancelled.
const STOPPED: &str = "The agent stopped before this finished.";
/// How long a refused key that keeps dialing goes unreported after it was reported.
const REFUSED_AGAIN_AFTER: Duration = Duration::from_secs(600);

/// What the UI shows about this agent besides live state.
pub struct UiInfo {
    pub profile: Profile,
    pub network: Network,
    pub serving: bool,
    pub access: Access,
    pub workspace: PathBuf,
    pub providers: Vec<Provider>,
    pub accounts: Vec<(Provider, String)>,
    /// Starts and stops this profile's digital workers (none for a digital worker,
    /// which can't own workers).
    pub workers: Option<Arc<crate::workers::Supervisor>>,
}

#[derive(Debug, Clone, Serialize)]
struct TaskView {
    id: u64,
    provider: Provider,
    prompt: String,
    target: String,
    status: &'static str,
    output: Option<String>,
    started_ms: u128,
    finished_ms: Option<u128>,
    /// For plan steps: the plan, the step id and the steps it waits for.
    #[serde(skip_serializing_if = "Option::is_none")]
    plan: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    step: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    after: Vec<String>,
    /// An approval step (a person signs off) rather than an LLM task.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    approval: bool,
    /// A team step: its lead hands the work down to its own team (it runs no model).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    team_step: bool,
    /// For peers' results: "verified" (signed by the peer for this output) or "unverified".
    #[serde(skip_serializing_if = "Option::is_none")]
    evidence: Option<&'static str>,
    /// The sandboxed preview of what the task created (a UX prototype, or a review
    /// pack of decks and design documents), and which of the two it is.
    #[serde(skip_serializing_if = "Option::is_none")]
    preview: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    preview_label: Option<&'static str>,
    /// Started by another agent ("MCP" or "A2A"), not in this UI.
    #[serde(skip_serializing_if = "Option::is_none")]
    via: Option<&'static str>,
    /// The evidence run that keeps what proves it: its plan's run, or, for a task on
    /// its own, the run made of it when it asked for checks or its peer committed work.
    #[serde(skip_serializing_if = "Option::is_none")]
    run: Option<String>,
    /// What its peer last said it is doing, while it runs.
    #[serde(skip_serializing_if = "Option::is_none")]
    progress: Option<crate::protocol::Progress>,
    /// For a team step: the lead's team run and its steps, once it has ended.
    #[serde(skip_serializing_if = "Option::is_none")]
    team: Option<TeamView>,
    /// For a step of a team run this agent leads: the peer whose team step it is.
    #[serde(skip_serializing_if = "Option::is_none")]
    for_peer: Option<String>,
    /// For a tracked plan step: its issue, and its run's (see [`crate::tracker`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    issue: Option<crate::tracker::IssueRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    plan_issue: Option<crate::tracker::IssueRef>,
}

/// A team step's team run as the page shows it.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct TeamView {
    run: String,
    lead: String,
    verdict: String,
    passed: bool,
    steps: Vec<crate::protocol::SubStep>,
}

impl TeamView {
    /// The team run in `evidence`, if it carries one.
    fn of(evidence: Option<&crate::evidence::Evidence>) -> Option<Self> {
        let run = evidence?.team.as_ref()?;
        Some(Self {
            run: run.id.clone(),
            lead: run.boss.clone(),
            verdict: run.team_verdict(),
            passed: run.passed(),
            steps: crate::team::run_steps(run),
        })
    }
}

impl TaskView {
    fn finished(&self) -> bool {
        !matches!(self.status, "running" | "waiting")
    }
}

/// A task as `tasks.json` keeps it (its preview isn't kept: it lives in memory).
#[derive(Serialize, Deserialize)]
struct StoredTask {
    id: u64,
    provider: Provider,
    prompt: String,
    target: String,
    status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    output: Option<String>,
    started_ms: u128,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    finished_ms: Option<u128>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    plan: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    step: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    after: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    approval: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    team_step: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    evidence: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    via: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    run: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    team: Option<TeamView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    for_peer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    issue: Option<crate::tracker::IssueRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    plan_issue: Option<crate::tracker::IssueRef>,
}

#[derive(Default, Serialize, Deserialize)]
struct TaskFile {
    #[serde(default)]
    tasks: Vec<StoredTask>,
}

impl From<&TaskView> for StoredTask {
    fn from(t: &TaskView) -> Self {
        Self {
            id: t.id,
            provider: t.provider,
            prompt: t.prompt.clone(),
            target: t.target.clone(),
            status: t.status.to_owned(),
            output: t.output.as_deref().map(|out| match out.char_indices().nth(KEPT_OUTPUT) {
                None => out.to_owned(),
                Some((at, _)) => format!(
                    "{}\n\n… (cut: only the first {KEPT_OUTPUT} characters are kept after a restart)",
                    &out[..at]
                ),
            }),
            started_ms: t.started_ms,
            finished_ms: t.finished_ms,
            plan: t.plan,
            step: t.step.clone(),
            after: t.after.clone(),
            approval: t.approval,
            team_step: t.team_step,
            evidence: t.evidence.map(str::to_owned),
            via: t.via.map(str::to_owned),
            run: t.run.clone(),
            team: t.team.clone(),
            for_peer: t.for_peer.clone(),
            issue: t.issue.clone(),
            plan_issue: t.plan_issue.clone(),
        }
    }
}

impl StoredTask {
    /// The task as the page shows it. One that was running or waiting when the agent
    /// stopped is cancelled: its work stopped with the agent.
    fn into_view(self) -> TaskView {
        fn known(value: Option<&str>, all: &[&'static str]) -> Option<&'static str> {
            value.and_then(|v| all.iter().copied().find(|known| *known == v))
        }
        let finished = known(
            Some(self.status.as_str()),
            &["done", "failed", "cancelled", "skipped"],
        );
        TaskView {
            id: self.id,
            provider: self.provider,
            prompt: self.prompt,
            target: self.target,
            status: finished.unwrap_or("cancelled"),
            output: if finished.is_some() {
                self.output
            } else {
                Some(STOPPED.to_owned())
            },
            started_ms: self.started_ms,
            finished_ms: self.finished_ms,
            plan: self.plan,
            step: self.step,
            after: self.after,
            approval: self.approval,
            team_step: self.team_step,
            evidence: known(self.evidence.as_deref(), &["verified", "unverified"]),
            preview: None,
            preview_label: None,
            via: known(self.via.as_deref(), &["MCP", "A2A"]),
            run: self.run,
            progress: None,
            team: self.team,
            for_peer: self.for_peer,
            issue: self.issue,
            plan_issue: self.plan_issue,
        }
    }
}

/// The tasks kept from before the agent last stopped (none if there are none or the
/// file can't be read), and how many of them it stopped.
fn load_tasks(path: &std::path::Path) -> (Vec<TaskView>, usize) {
    let file = match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice::<TaskFile>(&bytes)
            .with_context(|| format!("parsing {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(TaskFile::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    };
    match file {
        Ok(file) => {
            let stopped = file
                .tasks
                .iter()
                .filter(|t| matches!(t.status.as_str(), "running" | "waiting"))
                .count();
            let tasks = file.tasks.into_iter().map(StoredTask::into_view).collect();
            (tasks, stopped)
        }
        Err(e) => {
            eprintln!("the tasks from before are not shown: {e:#}");
            (Vec::new(), 0)
        }
    }
}

struct Shared {
    agent: Agent,
    info: UiInfo,
    executor: Option<Arc<dyn Executor>>,
    token: String,
    port: AtomicU16,
    feed: Mutex<VecDeque<Value>>,
    next_feed_id: AtomicU64,
    tasks: Mutex<Vec<TaskView>>,
    /// Abort handles of running tasks (a plan's steps share one), for `DELETE /api/tasks/{id}`.
    running: Mutex<HashMap<u64, AbortHandle>>,
    next_task_id: AtomicU64,
    squads: Mutex<SquadStore>,
    tx: broadcast::Sender<String>,
    previews: crate::preview::Previews,
    scheduler: std::sync::OnceLock<crate::schedule::Scheduler>,
    staging: tools::Staging,
    /// Fires whenever a task changes (for waiting on one to finish).
    changed: tokio::sync::Notify,
    /// Fires whenever the tasks change, for saving them (see [`keep_tasks`]).
    unsaved: tokio::sync::Notify,
    /// Other agents connected over MCP and A2A: their token and the work they started.
    connect: connect::Connect,
    /// Team runs this agent leads for peers' team steps, by (peer, its task id): the
    /// run's id and the task ids of its steps here.
    team_runs: Mutex<TeamRuns>,
    /// Started with --ui-port: the address other agents use stays the same.
    fixed_port: bool,
    /// Where imports search Microsoft 365, if not Work IQ's hosted service.
    work: Option<Arc<dyn crate::workiq::context::WorkContext>>,
    /// GitHub CLI sign-ins run for the user: granting a scope, signing in an account.
    gh: Arc<crate::gh_auth::Flows>,
}

type AppState = Arc<Shared>;
type ApiError = (StatusCode, Json<Value>);
/// Team runs this agent leads, by (peer, its task id): the run's id, and the task id
/// here of each of its steps.
type TeamRuns = HashMap<(String, u64), (String, HashMap<String, u64>)>;

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn bad_request(msg: impl ToString) -> ApiError {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": msg.to_string() })),
    )
}

impl Shared {
    fn send(&self, msg: Value) {
        let _ = self.tx.send(msg.to_string());
    }

    fn push_feed(&self, mut item: Value) {
        item["id"] = json!(self.next_feed_id.fetch_add(1, Ordering::Relaxed));
        item["ts"] = json!(now_ms());
        {
            let mut feed = self.feed.lock().unwrap();
            feed.push_back(item.clone());
            while feed.len() > FEED_LIMIT {
                feed.pop_front();
            }
        }
        self.send(json!({ "type": "feed", "item": item }));
    }

    fn system(&self, level: &str, text: impl Into<String>) {
        self.push_feed(json!({ "kind": "system", "level": level, "text": text.into() }));
    }

    fn peers_json(&self) -> Value {
        let peers = self.agent.peers();
        let peers_json: Vec<Value> = peers
            .iter()
            .map(|p| {
                json!({
                    "id": p.id.to_string(),
                    "short": p.id.fmt_short().to_string(),
                    "name": p.name,
                    "capabilities": p.capabilities,
                    "owner": p.owner,
                    "role": p.role,
                    "shares": p.shares,
                    // How busy it says it is, with this agent's tasks there right now.
                    "load": p.load,
                    "mine": self.agent.pending_to(p.id),
                })
            })
            .collect();
        let trusted: Vec<Value> = self
            .agent
            .trusted()
            .peers
            .iter()
            .map(|t| {
                json!({
                    "id": t.id.to_string(),
                    "short": t.id.fmt_short().to_string(),
                    "name": t.name,
                    "can_delegate": t.can_delegate,
                    "tasks_per_hour": t.tasks_per_hour,
                    "access": t.access,
                    "shares": t.shares,
                    "m365": t.m365,
                    "connected": peers.iter().any(|p| p.id == t.id),
                })
            })
            .collect();
        let squads: Vec<Value> = self
            .squads
            .lock()
            .unwrap()
            .squads
            .iter()
            .map(|s| {
                let members: Vec<Value> = s
                    .members
                    .iter()
                    .map(|m| json!({ "name": m, "online": peers.iter().any(|p| &p.name == m) }))
                    .collect();
                json!({ "name": s.name, "members": members })
            })
            .collect();
        json!({ "peers": peers_json, "trusted": trusted, "squads": squads })
    }

    fn broadcast_peers(&self) {
        let mut msg = self.peers_json();
        msg["type"] = json!("peers");
        self.send(msg);
    }

    fn me_json(&self) -> Value {
        let addr = self.agent.addr();
        let ticket = Ticket {
            name: self.info.profile.name.clone(),
            addr: addr.clone(),
        }
        .encode();
        let accounts: Vec<Value> = self
            .info
            .accounts
            .iter()
            .map(|(p, account)| json!({ "provider": p, "account": account }))
            .collect();
        json!({
            "name": self.agent.name(),
            "id": self.agent.id().to_string(),
            "short": self.agent.id().fmt_short().to_string(),
            "profile": self.info.profile.name,
            "network": match self.info.network { Network::Internet => "internet", Network::Local => "local" },
            "relay": addr.relay_urls().next().map(|u| u.to_string()),
            "serving": self.info.serving,
            "access": self.info.access,
            "workspace": self.info.workspace.display().to_string(),
            "providers": self.info.providers,
            "accounts": accounts,
            "shares": if self.info.serving { self.shares_json() } else { json!([]) },
            "ticket": ticket,
        })
    }

    /// This agent's shared folders, each with what it can preview ("prototype" or
    /// "review pack"), if anything.
    fn shares_json(&self) -> Value {
        let shares: Vec<Value> = self
            .agent
            .shares()
            .shares
            .iter()
            .map(|share| {
                let mut v = json!(share);
                let is = |rel: &str, dir: bool| {
                    std::fs::symlink_metadata(share.path.join(rel))
                        .is_ok_and(|m| if dir { m.is_dir() } else { m.is_file() })
                };
                v["preview"] = if is("review", true) || is("design", true) {
                    json!("review pack")
                } else if is("prototype/index.html", false) {
                    json!("prototype")
                } else {
                    Value::Null
                };
                // The owner's UI shows MCP servers without their secret values.
                v["mcp"] = json!(
                    share
                        .mcp
                        .iter()
                        .map(|(name, s)| json!({
                            "name": name,
                            "access": s.access,
                            "tools": s.tools,
                            "describe": s.describe(),
                        }))
                        .collect::<Vec<_>>()
                );
                v
            })
            .collect();
        json!(shares)
    }

    /// Adds a task to the list, letting the oldest finished ones go.
    fn add_task(&self, view: TaskView) {
        let mut tasks = self.tasks.lock().unwrap();
        tasks.push(view);
        prune_tasks(&mut tasks, TASK_LIMIT);
        self.unsaved.notify_one();
    }

    fn update_task(&self, id: u64, f: impl FnOnce(&mut TaskView)) {
        let task = {
            let mut tasks = self.tasks.lock().unwrap();
            let Some(task) = tasks.iter_mut().find(|t| t.id == id) else {
                return;
            };
            f(task);
            task.clone()
        };
        self.unsaved.notify_one();
        self.send(json!({ "type": "task", "task": task }));
        self.changed.notify_waiters();
    }

    /// Shows a team run this agent leads for `from`'s team step as a plan of its
    /// own: its steps as tasks, marked whose they are. The agent runs them (they end
    /// when the team step ends, not by a cancel here).
    fn show_team_run(&self, from: &str, task_id: u64, run: &str, steps: &[crate::plan::Step]) {
        let plan_id = self.next_task_id.fetch_add(1, Ordering::Relaxed);
        let mut ids = HashMap::new();
        for step in steps {
            let id = self.next_task_id.fetch_add(1, Ordering::Relaxed);
            ids.insert(step.id.clone(), id);
            let view = TaskView {
                id,
                provider: step.provider,
                prompt: step.prompt.clone(),
                target: format!(
                    "{}{}",
                    step.target,
                    step.share
                        .as_ref()
                        .map(|f| format!(":{f}"))
                        .unwrap_or_default()
                ),
                status: "waiting",
                output: None,
                started_ms: now_ms(),
                finished_ms: None,
                plan: Some(plan_id),
                step: Some(step.id.clone()),
                after: step.after.clone(),
                approval: step.approve,
                team_step: step.team.is_some(),
                evidence: None,
                preview: None,
                preview_label: None,
                via: None,
                run: None,
                progress: None,
                team: None,
                for_peer: Some(from.to_owned()),
                issue: None,
                plan_issue: None,
            };
            self.add_task(view.clone());
            self.send(json!({ "type": "task", "task": view }));
        }
        self.team_runs
            .lock()
            .unwrap()
            .insert((from.to_owned(), task_id), (run.to_owned(), ids));
        self.system(
            "task",
            format!(
                "Team run {run} started for {from}'s team step: {} step{} on your team",
                steps.len(),
                if steps.len() == 1 { "" } else { "s" }
            ),
        );
    }

    /// `from`'s team step `task_id` ended (`verb`): its team run's steps link to the
    /// saved run, and those that hadn't finished stopped with it.
    fn end_team_run(&self, from: &str, task_id: u64, verb: &str) {
        let Some((run, ids)) = self
            .team_runs
            .lock()
            .unwrap()
            .remove(&(from.to_owned(), task_id))
        else {
            return;
        };
        for id in ids.values() {
            self.update_task(*id, |t| {
                if !t.finished() {
                    t.status = "cancelled";
                    t.output = Some(format!("The team run stopped: {from}'s team step {verb}."));
                    t.finished_ms = Some(now_ms());
                    t.progress = None;
                }
                t.run = Some(run.clone());
            });
        }
        self.send(json!({ "type": "runs" }));
    }

    fn save_trust(&self) -> Result<(), ApiError> {
        self.agent
            .trusted()
            .save(&self.info.profile.trust_path())
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": format!("{e:#}") })),
                )
            })
    }
}

/// Lets the oldest finished tasks go until at most `limit` are left (running ones always
/// stay); a plan's steps go together, once every one of them has finished.
fn prune_tasks(tasks: &mut Vec<TaskView>, limit: usize) {
    if tasks.len() <= limit {
        return;
    }
    let running: std::collections::HashSet<u64> = tasks
        .iter()
        .filter(|t| !t.finished())
        .filter_map(|t| t.plan)
        .collect();
    let mut gone = std::collections::HashSet::new();
    let mut left = tasks.len();
    for t in tasks.iter() {
        if left <= limit {
            break;
        }
        if !t.finished() || gone.contains(&t.id) {
            continue;
        }
        match t.plan {
            None => {
                gone.insert(t.id);
                left -= 1;
            }
            Some(plan) if running.contains(&plan) => {}
            Some(plan) => {
                for step in tasks.iter().filter(|s| s.plan == Some(plan)) {
                    if gone.insert(step.id) {
                        left -= 1;
                    }
                }
            }
        }
    }
    tasks.retain(|t| !gone.contains(&t.id));
}

/// Saves the latest tasks whenever they change (at most once a second), so the list
/// is still there after the agent restarts.
fn keep_tasks(shared: &AppState) {
    let s = shared.clone();
    let path = s.info.profile.tasks_path();
    tokio::spawn(async move {
        loop {
            s.unsaved.notified().await;
            let file = {
                let mut kept = s.tasks.lock().unwrap().clone();
                prune_tasks(&mut kept, KEPT_TASKS);
                TaskFile {
                    tasks: kept.iter().map(StoredTask::from).collect(),
                }
            };
            let path = path.clone();
            let saved = tokio::task::spawn_blocking(move || {
                crate::store::write_atomic(&path, &serde_json::to_vec(&file)?)
            })
            .await;
            if let Ok(Err(e)) = saved {
                eprintln!("could not keep the tasks for after a restart: {e:#}");
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    });
}

/// A running UI server.
pub struct UiServer {
    pub url: String,
}

/// Starts the UI on `bind` (use 127.0.0.1). Consumes the agent's event stream.
pub async fn start(
    agent: Agent,
    events: mpsc::UnboundedReceiver<Event>,
    info: UiInfo,
    executor: Option<Arc<dyn Executor>>,
    bind: SocketAddr,
) -> Result<UiServer> {
    start_with_work(agent, events, info, executor, bind, None).await
}

/// [`start`], with `work` as the Work IQ the UI's imports search (none: Work IQ's hosted
/// service with the profile's account).
pub async fn start_with_work(
    agent: Agent,
    mut events: mpsc::UnboundedReceiver<Event>,
    info: UiInfo,
    executor: Option<Arc<dyn Executor>>,
    bind: SocketAddr,
    work: Option<Arc<dyn crate::workiq::context::WorkContext>>,
) -> Result<UiServer> {
    let token = data_encoding::HEXLOWER.encode(&iroh::SecretKey::generate().to_bytes()[..16]);
    let (tx, _) = broadcast::channel(256);
    let squads = SquadStore::load(&info.profile.squads_path()).unwrap_or_default();
    let (tasks, stopped) = load_tasks(&info.profile.tasks_path());
    // Ids go on from the kept tasks' (a plan's id is one too).
    let next_task_id = tasks
        .iter()
        .flat_map(|t| [Some(t.id), t.plan])
        .flatten()
        .max()
        .unwrap_or(0)
        + 1;
    let shared = Arc::new(Shared {
        agent,
        info,
        executor,
        token: token.clone(),
        port: AtomicU16::new(0),
        feed: Mutex::new(VecDeque::new()),
        next_feed_id: AtomicU64::new(1),
        tasks: Mutex::new(tasks),
        squads: Mutex::new(squads),
        running: Mutex::new(HashMap::new()),
        next_task_id: AtomicU64::new(next_task_id),
        tx,
        previews: crate::preview::Previews::default(),
        scheduler: std::sync::OnceLock::new(),
        staging: tools::Staging::default(),
        changed: tokio::sync::Notify::new(),
        unsaved: tokio::sync::Notify::new(),
        connect: connect::Connect::default(),
        team_runs: Mutex::new(HashMap::new()),
        fixed_port: bind.port() != 0,
        work,
        gh: crate::gh_auth::Flows::new(Arc::new(crate::gh_auth::GhCli::new())),
    });
    if stopped > 0 {
        let (were, they, them) = if stopped == 1 {
            ("task was", "it", "it")
        } else {
            ("tasks were", "they", "them")
        };
        shared.system(
            "warn",
            format!(
                "{stopped} {were} cancelled because the agent stopped before {they} finished; run {them} again if you still need {them}"
            ),
        );
        // Kept as cancelled from now on.
        shared.unsaved.notify_one();
    }
    keep_tasks(&shared);
    start_scheduler(&shared);
    settle_tracked_runs(&shared);
    watch_workers(&shared);

    let pump = shared.clone();
    tokio::spawn(async move {
        // When each key was last reported refused: one that keeps dialing (a teammate who
        // allowlisted this agent first) is reported again only after a while.
        let mut refused: HashMap<iroh::EndpointId, std::time::Instant> = HashMap::new();
        while let Some(event) = events.recv().await {
            match event {
                Event::PeerConnected { name, .. } => {
                    pump.system("join", format!("{name} connected"));
                    pump.broadcast_peers();
                }
                Event::PeerDisconnected { name, .. } => {
                    pump.system("leave", format!("{name} disconnected"));
                    pump.broadcast_peers();
                }
                Event::PeerRejected { id }
                    if refused
                        .get(&id)
                        .is_some_and(|at| at.elapsed() < REFUSED_AGAIN_AFTER) => {}
                Event::PeerRejected { id } => {
                    if refused.len() >= 1024 {
                        refused.retain(|_, at| at.elapsed() < REFUSED_AGAIN_AFTER);
                    }
                    refused.insert(id, std::time::Instant::now());
                    pump.push_feed(json!({
                        "kind": "system",
                        "level": "warn",
                        "text": format!(
                            "Refused a connection from {} (not on the allowlist)",
                            id.fmt_short()
                        ),
                        // So the page can offer to allowlist it (its endpoint id is enough).
                        "peer": id.to_string(),
                    }));
                }
                Event::Chat { from, text } => pump.push_feed(
                    json!({ "kind": "chat", "from": from, "text": text, "self": false }),
                ),
                Event::PeerBusy { .. } => pump.broadcast_peers(),
                Event::TaskReceived {
                    from,
                    task_id,
                    provider,
                    origin,
                    team,
                } => {
                    let what = origin.as_ref().and_then(crate::protocol::Origin::describe);
                    pump.system(
                        "task",
                        match (what, team) {
                            (Some(what), true) => format!(
                                "{from} handed {what} to this agent's team (team step, task #{task_id})"
                            ),
                            (None, true) => {
                                format!("{from} handed team step #{task_id} to this agent's team")
                            }
                            (Some(what), false) => format!(
                                "{from} delegated {what} to this agent (task #{task_id}, {provider})"
                            ),
                            (None, false) => {
                                format!("{from} delegated task #{task_id} ({provider}) to this agent")
                            }
                        },
                    )
                }
                Event::TeamRun {
                    from,
                    task_id,
                    run,
                    steps,
                } => pump.show_team_run(&from, task_id, &run, &steps),
                Event::TeamStep {
                    from,
                    task_id,
                    step,
                    state,
                } => {
                    let id = pump
                        .team_runs
                        .lock()
                        .unwrap()
                        .get(&(from.clone(), task_id))
                        .and_then(|(_, steps)| steps.get(&step).copied());
                    if let Some(id) = id {
                        pump.update_task(id, |t| apply_state(&pump, t, &state));
                    }
                }
                Event::TaskCompleted {
                    from,
                    task_id,
                    outcome,
                } => {
                    let (level, verb) = match outcome {
                        Outcome::Done => ("task", "finished"),
                        Outcome::Cancelled => ("warn", "was cancelled"),
                        _ => ("warn", "failed"),
                    };
                    pump.system(level, format!("Task #{task_id} from {from} {verb}"));
                    pump.end_team_run(&from, task_id, verb);
                    pump.send(json!({ "type": "audit" }));
                }
                Event::TaskRefused {
                    from,
                    task_id,
                    reason,
                } => {
                    pump.system(
                        "warn",
                        format!("Refused task #{task_id} from {from}: {reason}"),
                    );
                    pump.send(json!({ "type": "audit" }));
                }
                Event::ApprovalRequested { id, from, title } => {
                    pump.system(
                        "task",
                        format!("{from} asks you to approve: {title} (#{id})"),
                    );
                    pump.send(json!({ "type": "approvals", "approvals": approvals_json(&pump) }));
                }
                Event::ApprovalClosed { .. } => {
                    pump.send(json!({ "type": "approvals", "approvals": approvals_json(&pump) }));
                }
            }
        }
    });

    let api = Router::new()
        .route("/api/state", get(state))
        .route("/api/events", get(events_stream))
        .route("/api/chat", post(chat))
        .route("/api/tasks", post(create_task).delete(clear_tasks))
        .route("/api/tasks/{id}", delete(cancel_task))
        .route("/api/audit", get(audit))
        .route("/api/plans", post(create_plan))
        .route("/api/schedules", get(list_schedules).post(add_schedule))
        .route(
            "/api/schedules/{name}",
            delete(remove_schedule)
                .patch(toggle_schedule)
                .put(edit_schedule),
        )
        .route("/api/schedules/{name}/run", post(run_schedule))
        .route("/api/schedule-preview", post(preview_when))
        .route("/api/runs", get(list_runs))
        .route("/api/runs/{id}", get(get_run))
        .route("/api/runs/{id}/file", get(run_file))
        .route("/api/previews", post(create_preview))
        .route("/api/approvals/{id}", post(answer_approval))
        .route("/api/ops", get(ops_state))
        .route("/api/monitors", post(add_monitor))
        .route("/api/monitors/{name}", delete(delete_monitor))
        .route("/api/playbooks", get(list_playbooks))
        .route("/api/playbooks/{name}", post(render_playbook))
        .route("/api/kb/import/workiq", post(import_workiq))
        .route("/api/kb/{topic}", get(kb_topic))
        .route("/api/plans/check", post(check_plan))
        .route("/api/github", get(github_state))
        .route("/api/github/login", post(github_sign_in))
        .route("/api/github/login/new", post(github_sign_in_new))
        .route("/api/github/grant", post(github_grant))
        .route(
            "/api/github/flow",
            get(github_flow).delete(cancel_github_flow),
        )
        .route("/api/repos", post(link_repo))
        .route("/api/repos/{name}", delete(unlink_repo))
        .route("/api/repos/{name}/sync", post(sync_repo))
        .route("/api/repos/{name}/pr", post(open_pr))
        .route("/api/repos/{name}/proof", get(branch_proof))
        .route(
            "/api/tracker",
            get(tracker_state).put(link_tracker).delete(unlink_tracker),
        )
        .route("/api/tracker/items", get(tracker_items))
        .route("/api/tracker/add", post(add_to_board))
        .route("/api/tracker/sync", post(sync_tracker))
        .route("/api/skills", get(list_skills).post(tools::add_skill))
        .route(
            "/api/skills/{name}",
            get(skill_text).delete(tools::remove_skill),
        )
        .route("/api/agents", post(tools::add_agent))
        .route(
            "/api/agents/{name}",
            get(agent_def).delete(tools::remove_agent),
        )
        .route(
            "/api/plugins",
            get(tools::list_plugins).post(tools::install_plugin),
        )
        .route("/api/plugin-preview", post(tools::preview_plugin))
        .route(
            "/api/plugins/{name}",
            delete(tools::remove_plugin).patch(tools::toggle_plugin),
        )
        .route("/api/plugins/{name}/update", post(tools::update_plugin))
        .route("/api/mcp", get(tools::mcp_state).post(tools::offer_mcp))
        .route("/api/mcp/parse", post(tools::parse_mcp))
        .route(
            "/api/mcp/{share}/{name}",
            delete(tools::remove_mcp).patch(tools::edit_mcp),
        )
        .route(
            "/api/shares/{name}/team",
            put(tools::set_team).delete(tools::remove_team),
        )
        .route("/api/squads", post(save_squad))
        .route("/api/squads/{name}", delete(remove_squad))
        .route("/api/workers", get(workers::list).post(workers::create))
        .route(
            "/api/workers/{name}",
            delete(workers::delete).patch(workers::edit),
        )
        .route("/api/workers/{name}/start", post(workers::start))
        .route("/api/workers/{name}/stop", post(workers::stop))
        .route("/api/sign-in/copilot", post(workers::sign_in_copilot))
        .route(
            "/api/connect",
            get(connect::state)
                .post(connect::turn_on)
                .delete(connect::turn_off),
        )
        .route("/api/trust", post(allow_peer))
        .route("/api/trust/{id}", delete(remove_peer))
        .route_layer(middleware::from_fn_with_state(
            shared.clone(),
            require_token,
        ));
    // Other agents (MCP, A2A): their own token, never the web UI's.
    let integrations = Router::new()
        .route(
            "/mcp",
            post(connect::mcp)
                .get(connect::mcp_no_stream)
                .delete(connect::mcp_no_stream),
        )
        .route("/a2a", post(connect::a2a))
        .route_layer(middleware::from_fn_with_state(
            shared.clone(),
            connect::require_integration_token,
        ));
    let app = Router::new()
        .route(
            "/",
            get(|| async { asset("text/html; charset=utf-8", INDEX_HTML) }),
        )
        .route(
            "/app.css",
            get(|| async { asset("text/css; charset=utf-8", APP_CSS) }),
        )
        .route(
            "/app.js",
            get(|| async { asset("text/javascript; charset=utf-8", APP_JS) }),
        )
        .route(
            "/tools.js",
            get(|| async { asset("text/javascript; charset=utf-8", TOOLS_JS) }),
        )
        .route(
            "/plan-text.js",
            get(|| async { asset("text/javascript; charset=utf-8", PLAN_TEXT_JS) }),
        )
        .route(
            "/plan-builder.js",
            get(|| async { asset("text/javascript; charset=utf-8", PLAN_BUILDER_JS) }),
        )
        .route(
            "/workers.js",
            get(|| async { asset("text/javascript; charset=utf-8", WORKERS_JS) }),
        )
        .route(
            "/connect.js",
            get(|| async { asset("text/javascript; charset=utf-8", CONNECT_JS) }),
        )
        // The A2A agent card is public (to this machine), like any agent's.
        .route("/.well-known/agent-card.json", get(connect::agent_card))
        // Outside the token check: the unguessable capability in the path is the
        // authorization, and the content runs in a CSP sandbox (see crate::preview).
        .route("/preview/{cap}", get(preview_root))
        .route("/preview/{cap}/", get(preview_index))
        .route("/preview/{cap}/{*path}", get(preview_file))
        .merge(api)
        .merge(integrations)
        .layer(middleware::from_fn_with_state(
            shared.clone(),
            require_local_host,
        ))
        .with_state(shared.clone());

    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .context("binding the UI server")?;
    let port = listener.local_addr()?.port();
    shared.port.store(port, Ordering::Relaxed);
    // Tells `proofkin connect` where other agents reach this agent.
    let _ = std::fs::write(shared.info.profile.ui_port_path(), port.to_string());
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    shared.system("info", "Agent started");
    Ok(UiServer {
        url: format!("http://127.0.0.1:{port}/#t={token}"),
    })
}

/// Tells the page when a digital worker's process starts or ends, and the feed when
/// one stops on its own.
fn watch_workers(shared: &AppState) {
    let Some(supervisor) = shared.info.workers.clone() else {
        return;
    };
    let mut changes = supervisor.subscribe();
    let pump = shared.clone();
    tokio::spawn(async move {
        loop {
            let name = match changes.recv().await {
                Ok(name) => Some(name),
                Err(broadcast::error::RecvError::Lagged(_)) => None,
                Err(broadcast::error::RecvError::Closed) => return,
            };
            if let Some(name) = name
                && let Ok(worker) = crate::workers::worker_of(&pump.info.profile, &name)
                && let crate::workers::Process::Exited { code, .. } = supervisor.process(&worker)
            {
                pump.system(
                    "warn",
                    format!(
                        "Digital worker {name} stopped on its own{}; its log is under Digital workers",
                        code.map(|c| format!(" (exit code {c})")).unwrap_or_default()
                    ),
                );
            }
            pump.send(json!({ "type": "workers" }));
        }
    });
}

fn asset(content_type: &'static str, body: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "no-store"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'self'; style-src 'self'; script-src 'self'; img-src 'self' data:; frame-ancestors 'none'",
            ),
        ],
        body,
    )
        .into_response()
}

fn is_capability(cap: &str) -> bool {
    cap.len() == 32 && cap.bytes().all(|b| b.is_ascii_hexdigit())
}

fn no_preview() -> Response {
    (
        StatusCode::NOT_FOUND,
        "There is no such preview (previews expire after 2 hours).",
    )
        .into_response()
}

async fn preview_root(Path(cap): Path<String>) -> Response {
    if !is_capability(&cap) {
        return no_preview();
    }
    axum::response::Redirect::permanent(&format!("/preview/{cap}/")).into_response()
}

async fn preview_index(State(s): State<AppState>, Path(cap): Path<String>) -> Response {
    serve_preview(&s, &cap, "")
}

async fn preview_file(
    State(s): State<AppState>,
    Path((cap, path)): Path<(String, String)>,
) -> Response {
    serve_preview(&s, &cap, &path)
}

/// A prototype file, in a CSP sandbox: an opaque origin without network access, so
/// it can't reach this UI's API or token.
fn serve_preview(s: &Shared, cap: &str, path: &str) -> Response {
    match s.previews.file(cap, path) {
        Some((kind, body)) => (
            [
                (header::CONTENT_TYPE, kind),
                (header::CACHE_CONTROL, "no-store"),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
                (header::CONTENT_SECURITY_POLICY, crate::preview::PREVIEW_CSP),
                (header::REFERRER_POLICY, "no-referrer"),
            ],
            body,
        )
            .into_response(),
        None => no_preview(),
    }
}

#[derive(Deserialize)]
struct PreviewBody {
    /// A folder this agent shares: its prototype, decks and design documents now.
    share: Option<String>,
    /// A saved run and step: what the step's patch created.
    run: Option<String>,
    step: Option<String>,
    /// An open approval request that came with something to preview.
    approval: Option<u64>,
}

/// Opens a sandboxed preview of a UX prototype or a review pack; returns its
/// capability URL.
async fn create_preview(
    State(s): State<AppState>,
    Json(body): Json<PreviewBody>,
) -> Result<Json<Value>, ApiError> {
    let files = if let Some(id) = body.approval {
        s.agent
            .approval_preview(id)
            .ok_or_else(|| bad_request("that approval request has nothing to preview"))?
    } else if let (Some(run), Some(step)) = (&body.run, &body.step) {
        validate_name(run)
            .and(validate_name(step))
            .map_err(|_| bad_request("invalid run or step"))?;
        let path = s
            .info
            .profile
            .runs_dir()
            .join(run)
            .join(format!("{step}.patch"));
        let patch = std::fs::read_to_string(path)
            .map_err(|_| bad_request(format!("run {run} has no patch for step {step}")))?;
        crate::preview::site(&crate::preview::sources_from_patch(&patch))
    } else if let Some(name) = &body.share {
        let path = s
            .agent
            .shares()
            .get(name)
            .map(|share| share.path.clone())
            .ok_or_else(|| bad_request(format!("this agent shares no folder '{name}'")))?;
        tokio::task::spawn_blocking(move || {
            crate::preview::site(&crate::preview::sources_from_dir(&path))
        })
        .await
        .map_err(internal)?
    } else {
        return Err(bad_request("give a share, a run and step, or an approval"));
    };
    let cap = s.previews.add(files).ok_or_else(|| {
        bad_request(
            "there is nothing to preview (no prototype/index.html, review/ decks or design/ documents)",
        )
    })?;
    Ok(Json(json!({ "url": format!("/preview/{cap}/") })))
}

/// Rejects requests whose Host isn't this loopback server (DNS-rebinding defense).
async fn require_local_host(State(s): State<AppState>, req: Request, next: Next) -> Response {
    let port = s.port.load(Ordering::Relaxed);
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    if host == format!("127.0.0.1:{port}") || host == format!("localhost:{port}") {
        next.run(req).await
    } else {
        (StatusCode::FORBIDDEN, "forbidden host").into_response()
    }
}

#[derive(Deserialize)]
struct TokenQuery {
    token: Option<String>,
}

/// API calls need the access token (header, or `?token=` for EventSource).
async fn require_token(
    State(s): State<AppState>,
    Query(q): Query<TokenQuery>,
    headers: HeaderMap,
    req: Request,
    next: Next,
) -> Response {
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "));
    if bearer == Some(s.token.as_str()) || q.token.as_deref() == Some(s.token.as_str()) {
        next.run(req).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "missing or wrong access token" })),
        )
            .into_response()
    }
}

async fn state(State(s): State<AppState>) -> Json<Value> {
    let mut v = s.peers_json();
    v["me"] = s.me_json();
    v["feed"] = json!(s.feed.lock().unwrap().iter().cloned().collect::<Vec<_>>());
    v["tasks"] = json!(s.tasks.lock().unwrap().clone());
    v["approvals"] = approvals_json(&s);
    v["builtin_checks"] = crate::spec::BUILT_IN_CHECKS
        .iter()
        .map(|name| json!({ "name": name, "about": crate::spec::about(name) }))
        .collect();
    Json(v)
}

async fn events_stream(
    State(s): State<AppState>,
) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let stream = BroadcastStream::new(s.tx.subscribe())
        .filter_map(|msg| msg.ok())
        .map(|msg| Ok(SseEvent::default().data(msg)));
    Sse::new(stream).keep_alive(KeepAlive::default())
}

#[derive(Deserialize)]
struct ChatBody {
    text: String,
}

async fn chat(
    State(s): State<AppState>,
    Json(body): Json<ChatBody>,
) -> Result<Json<Value>, ApiError> {
    let text = body.text.trim();
    if text.is_empty() {
        return Err(bad_request("message is empty"));
    }
    let sent = s.agent.broadcast(text);
    s.push_feed(json!({ "kind": "chat", "from": s.agent.name(), "text": text, "self": true }));
    Ok(Json(json!({ "sent": sent })))
}

#[derive(Deserialize)]
struct TaskBody {
    provider: Provider,
    prompt: String,
    /// "local", "auto" (first peer serving the provider), or a peer endpoint id.
    target: String,
    /// For peers: the shared folder to run in (default: their `data` folder).
    share: Option<String>,
    /// For peers: MCP servers of that folder the model may use.
    #[serde(default)]
    mcp: Vec<String>,
    /// For peers: the folder's checks their runtime runs after the model, as evidence.
    #[serde(default)]
    checks: Vec<String>,
    /// Model attempts while the checks fail (default 1).
    #[serde(default)]
    attempts: Option<u32>,
}

async fn create_task(
    State(s): State<AppState>,
    Json(body): Json<TaskBody>,
) -> Result<Json<Value>, ApiError> {
    start_task(&s, body, None).map(|view| Json(json!(view)))
}

/// Starts a task on this agent or a peer; it shows up as a task, updated live.
/// `via`: another agent started it (over MCP or A2A).
fn start_task(
    s: &AppState,
    body: TaskBody,
    via: Option<&'static str>,
) -> Result<TaskView, ApiError> {
    let prompt = body.prompt.trim().to_owned();
    if prompt.is_empty() {
        return Err(bad_request("prompt is empty"));
    }
    let mut task = LlmTask::new(body.provider, prompt.clone());
    task.title = Some(crate::llm::title_of(&prompt));
    if body.target != "local" {
        task.mcp = body.mcp;
    }
    if !body.checks.is_empty() || body.attempts.is_some() {
        if body.target == "local" {
            return Err(bad_request(
                "checks run on a peer's runtime: pick a peer to run the task on",
            ));
        }
        if body
            .attempts
            .is_some_and(|n| !(1..=crate::evidence::MAX_ATTEMPTS).contains(&n))
        {
            return Err(bad_request(format!(
                "attempts must be 1 to {}",
                crate::evidence::MAX_ATTEMPTS
            )));
        }
        task = task.with_checks(body.checks, body.attempts);
    }
    if let Some(share) = body
        .share
        .filter(|s| !s.is_empty() && body.target != "local")
    {
        task = task.in_share(share);
    }
    enum Route {
        Local(Arc<dyn Executor>),
        Peer(iroh::EndpointId, String),
    }
    let route = match body.target.as_str() {
        "local" => match &s.executor {
            Some(e) if e.providers().contains(&body.provider) => Route::Local(e.clone()),
            _ => {
                return Err(bad_request(format!(
                    "{} is not enabled on this agent",
                    body.provider
                )));
            }
        },
        "auto" => {
            let peer = s.agent.find_peer(body.provider).ok_or_else(|| {
                bad_request(format!("no connected peer serves {}", body.provider))
            })?;
            Route::Peer(peer.id, peer.name)
        }
        id => {
            let peer = s
                .agent
                .peers()
                .into_iter()
                .find(|p| p.id.to_string() == id)
                .ok_or_else(|| bad_request("that peer is not connected"))?;
            Route::Peer(peer.id, peer.name)
        }
    };
    let id = s.next_task_id.fetch_add(1, Ordering::Relaxed);
    let target = match &route {
        Route::Local(_) => "this agent".to_owned(),
        Route::Peer(_, name) => name.clone(),
    };
    let view = TaskView {
        id,
        provider: body.provider,
        prompt,
        target: target.clone(),
        status: "running",
        output: None,
        started_ms: now_ms(),
        finished_ms: None,
        plan: None,
        step: None,
        after: Vec::new(),
        approval: false,
        team_step: false,
        evidence: None,
        preview: None,
        preview_label: None,
        via,
        run: None,
        progress: None,
        team: None,
        for_peer: None,
        issue: None,
        plan_issue: None,
    };
    s.add_task(view.clone());
    s.send(json!({ "type": "task", "task": view }));

    let shared = s.clone();
    let handle = tokio::spawn(async move {
        let mut evidence = None;
        let mut run = None;
        let started = crate::runs::unix_now();
        let result = match route {
            Route::Local(executor) => {
                let ctx = TaskContext::new(shared.info.access, shared.info.workspace.clone());
                executor.execute(&task, &ctx).await
            }
            Route::Peer(peer, name) => {
                let origin = crate::protocol::Origin {
                    task: Some(id),
                    ..Default::default()
                };
                let sent = shared.agent.send_task(peer, task.clone(), Some(origin));
                let progress = shared.clone();
                let delivered = match sent {
                    Ok(delegation) => {
                        delegation
                            .finish(|p| progress.update_task(id, |t| t.progress = Some(p)))
                            .await
                    }
                    Err(e) => Err(e),
                };
                match delivered {
                    Ok(d) => {
                        if d.evidence.is_some() {
                            evidence = Some(if d.verified { "verified" } else { "unverified" });
                        }
                        if RunRecord::worth_keeping(&task, &d) {
                            run = save_task_run(&shared, &name, &task, d.clone(), started);
                        }
                        if d.ok {
                            Ok(d.output)
                        } else {
                            Err(anyhow::anyhow!("{}", d.output))
                        }
                    }
                    Err(e) => Err(e),
                }
            }
        };
        shared.running.lock().unwrap().remove(&id);
        shared.update_task(id, |t| {
            t.finished_ms = Some(now_ms());
            t.evidence = evidence;
            t.run = run;
            t.progress = None;
            match result {
                Ok(out) => {
                    t.status = "done";
                    t.output = Some(out);
                }
                Err(e) => {
                    t.status = "failed";
                    t.output = Some(format!("{e:#}"));
                }
            }
        });
    });
    {
        // The task removes itself when done; insert only if it's still running.
        let mut running = s.running.lock().unwrap();
        if !handle.is_finished() {
            running.insert(id, handle.abort_handle());
        }
    }
    Ok(view)
}

/// Saves a task delegated on its own as a one-step evidence run (see
/// [`RunRecord::for_task`]); the run's id, if it was saved.
fn save_task_run(
    shared: &Shared,
    peer: &str,
    task: &LlmTask,
    delivered: crate::agent::Delivered,
    started: u64,
) -> Option<String> {
    let mut run = RunRecord::for_task(
        shared.agent.name(),
        &shared.agent.id().to_string(),
        peer,
        task,
        delivered,
        started,
    );
    run.peer_ids = crate::runs::peer_ids(&shared.agent);
    match run.save(&shared.info.profile.runs_dir()) {
        Ok(_) => {
            let (level, text) = if run.claims.is_empty() {
                ("info", format!("{peer}'s signed commit is saved"))
            } else if run.passed() {
                ("info", run.verdict_line())
            } else {
                ("warn", run.verdict_line())
            };
            shared.system(level, format!("Evidence run {}: {text}", run.id));
            shared.send(json!({ "type": "runs" }));
            shared.send(json!({ "type": "ops" }));
            Some(run.id)
        }
        Err(e) => {
            shared.system(
                "warn",
                format!("Could not save the task's evidence run: {e:#}"),
            );
            None
        }
    }
}

/// Cancels a running task, or the whole plan a step belongs to. Dropping a
/// delegation tells the peer to stop it; a local task's provider run is aborted.
async fn cancel_task(
    State(s): State<AppState>,
    Path(id): Path<u64>,
) -> Result<Json<Value>, ApiError> {
    cancel_running(&s, id, "Cancelled by you.")?;
    Ok(Json(json!({ "cancelled": id })))
}

/// Lets every finished task go (a plan's steps once the whole plan has ended); running
/// ones stay. Other agents can no longer look up work of theirs that is let go.
async fn clear_tasks(State(s): State<AppState>) -> Json<Value> {
    let (cleared, tasks) = {
        let mut tasks = s.tasks.lock().unwrap();
        let before = tasks.len();
        let running: std::collections::HashSet<u64> = tasks
            .iter()
            .filter(|t| !t.finished())
            .filter_map(|t| t.plan)
            .collect();
        tasks.retain(|t| !t.finished() || t.plan.is_some_and(|p| running.contains(&p)));
        (before - tasks.len(), tasks.clone())
    };
    s.unsaved.notify_one();
    s.send(json!({ "type": "tasks", "tasks": tasks }));
    Json(json!({ "cleared": cleared, "tasks": tasks }))
}

/// Cancels running task `id` (its whole plan, for a step), saying `why`.
fn cancel_running(s: &Shared, id: u64, why: &str) -> Result<(), ApiError> {
    let handle = s.running.lock().unwrap().remove(&id).ok_or_else(|| {
        (
            StatusCode::CONFLICT,
            Json(json!({ "error": "that task is not running" })),
        )
    })?;
    handle.abort();
    let plan = s
        .tasks
        .lock()
        .unwrap()
        .iter()
        .find(|t| t.id == id)
        .and_then(|t| t.plan);
    let ids: Vec<u64> = match plan {
        Some(plan) => s
            .tasks
            .lock()
            .unwrap()
            .iter()
            .filter(|t| t.plan == Some(plan) && !t.finished())
            .map(|t| t.id)
            .collect(),
        None => vec![id],
    };
    for id in ids {
        s.running.lock().unwrap().remove(&id);
        s.update_task(id, |t| {
            t.finished_ms = Some(now_ms());
            t.status = "cancelled";
            t.output = Some(why.into());
        });
    }
    Ok(())
}

#[derive(Deserialize)]
struct PlanBody {
    text: String,
}

/// Runs a plan (see [`crate::plan`]); each step shows up as a task, updated live.
/// When it ends, the run and its evidence report are saved in the profile.
async fn create_plan(
    State(s): State<AppState>,
    Json(body): Json<PlanBody>,
) -> Result<Json<Value>, ApiError> {
    let started = start_plan(&s, body.text, None).map_err(bad_request)?;
    Ok(Json(
        json!({ "plan": started.plan, "steps": started.steps }),
    ))
}

/// A plan started by [`start_plan`].
struct StartedPlan {
    plan: u64,
    steps: HashMap<String, u64>,
    /// The saved run's verdict line and id, when the plan ends.
    done: tokio::task::JoinHandle<crate::schedule::Outcome>,
}

/// Names `peer` in the target of a plan step, keeping the folder the target already has.
fn step_target(target: &str, peer: &str) -> String {
    match target.split_once(':') {
        Some((_, folder)) => format!("{peer}:{folder}"),
        None => peer.to_owned(),
    }
}

/// Shows plan step `state` on its task `t` (a step of a plan run here, or of a team
/// run this agent leads): its peer, what it is doing, and its result and evidence.
fn apply_state(shared: &Shared, t: &mut TaskView, state: &StepState) {
    match state {
        StepState::Waiting => {}
        StepState::Running { peer, progress } => {
            // Progress updates (and a move to another squad member) keep its start.
            if t.status != "running" {
                t.started_ms = now_ms();
            }
            t.status = "running";
            t.target = step_target(&t.target, peer);
            t.progress = progress.clone();
        }
        StepState::Done {
            peer,
            output,
            evidence,
            verified,
            patch,
        } => {
            t.status = "done";
            t.target = step_target(&t.target, peer);
            t.output = Some(output.clone());
            t.finished_ms = Some(now_ms());
            t.progress = None;
            t.evidence = evidence
                .as_ref()
                .map(|_| if *verified { "verified" } else { "unverified" });
            t.team = TeamView::of(evidence.as_ref());
            if let Some(patch) = patch.as_deref() {
                let sources = crate::preview::sources_from_patch(patch);
                t.preview_label = crate::preview::label(&sources);
                t.preview = t
                    .preview_label
                    .and_then(|_| shared.previews.add(crate::preview::site(&sources)))
                    .map(|cap| format!("/preview/{cap}/"));
            }
        }
        StepState::Failed {
            peer,
            error,
            evidence,
            verified,
        } => {
            t.status = "failed";
            if let Some(peer) = peer {
                t.target = step_target(&t.target, peer);
            }
            t.output = Some(error.clone());
            t.finished_ms = Some(now_ms());
            t.progress = None;
            t.evidence = evidence
                .as_ref()
                .map(|_| if *verified { "verified" } else { "unverified" });
            t.team = TeamView::of(evidence.as_ref());
        }
        StepState::Skipped { reason } => {
            t.status = "skipped";
            t.output = Some(format!("Skipped: {reason}"));
            t.finished_ms = Some(now_ms());
            t.progress = None;
        }
    }
}

/// Shows a tracked run's issues on its steps' tasks, and its tracking problems from
/// the `known`-th on in the feed.
fn show_tracked(
    shared: &Shared,
    step_ids: &HashMap<String, u64>,
    tracked: &crate::tracker::TrackedRun,
    known: usize,
) {
    if known == 0
        && let Some(issue) = &tracked.issue
    {
        shared.system(
            "info",
            format!(
                "Tracking the run on GitHub: issue #{} ({}), a sub-issue per step",
                issue.number, issue.url
            ),
        );
    }
    for (step, id) in step_ids {
        shared.update_task(*id, |t| {
            t.issue = tracked.steps.get(step).cloned();
            t.plan_issue = tracked.issue.clone();
        });
    }
    for problem in tracked.problems.iter().skip(known) {
        shared.system("warn", format!("Tracker: {problem}"));
    }
}

/// Starts plan `text` (`via`: another agent started it, over MCP or A2A).
fn start_plan(
    s: &AppState,
    text: String,
    via: Option<&'static str>,
) -> Result<StartedPlan, String> {
    let plan = Plan::parse(&text).map_err(|e| format!("{e:#}"))?;
    let kb = s.info.profile.knowledge();
    let skills = s.info.profile.skills();
    let squads_now = s.squads.lock().unwrap().clone();
    let recipients = crate::plan::Recipients::of(&s.agent, &s.info.profile);
    let errors: Vec<String> = crate::plan::preflight(
        &plan,
        &s.agent.peers(),
        &squads_now,
        &kb,
        &skills,
        &recipients,
    )
    .into_iter()
    .filter(|p| p.error)
    .map(|p| match p.step {
        Some(step) => format!("step {step}: {}", p.message),
        None => p.message,
    })
    .collect();
    if !errors.is_empty() {
        return Err(format!("the plan was not started:\n{}", errors.join("\n")));
    }
    let ready = plan.prepare(&kb, &skills).map_err(|e| format!("{e:#}"))?;
    let work = crate::plan::work_context(&s.info.profile, &plan).map_err(|e| format!("{e:#}"))?;
    let plan_id = s.next_task_id.fetch_add(1, Ordering::Relaxed);
    let mut ids = HashMap::new();
    for step in &plan.steps {
        let id = s.next_task_id.fetch_add(1, Ordering::Relaxed);
        ids.insert(step.id.clone(), id);
        let view = TaskView {
            id,
            provider: step.provider,
            prompt: step.prompt.clone(),
            target: format!(
                "{}{}",
                step.target,
                step.share
                    .as_ref()
                    .map(|f| format!(":{f}"))
                    .unwrap_or_default()
            ),
            status: "waiting",
            output: None,
            started_ms: now_ms(),
            finished_ms: None,
            plan: Some(plan_id),
            step: Some(step.id.clone()),
            after: step.after.clone(),
            approval: step.approve,
            team_step: step.team.is_some(),
            evidence: None,
            preview: None,
            preview_label: None,
            via,
            run: None,
            progress: None,
            team: None,
            for_peer: None,
            issue: None,
            plan_issue: None,
        };
        s.add_task(view.clone());
        s.send(json!({ "type": "task", "task": view }));
    }
    let squads = s.squads.lock().unwrap().clone();
    let shared = s.clone();
    let step_ids = ids.clone();
    let handle = tokio::spawn(async move {
        let started = crate::runs::unix_now();
        // The run's id is known from the start: each step's peer is told it.
        let run_id = crate::runs::new_run_id();
        // Tracked runs get their issues before their steps start.
        let tracking =
            match crate::tracker::start(&shared.info.profile, &plan, &run_id, shared.agent.name())
                .await
            {
                Ok(tracking) => tracking,
                Err(e) => {
                    shared.system("warn", format!("The run isn't tracked on GitHub: {e:#}"));
                    None
                }
            };
        if let Some(t) = &tracking {
            show_tracked(&shared, &step_ids, &t.tracked, 0);
        }
        let ctx = crate::plan::RunContext {
            run_id: Some(run_id.clone()),
            chain: Vec::new(),
            issues: tracking
                .as_ref()
                .map(|t| t.refs.clone())
                .unwrap_or_default(),
            recipients,
            work_context: work,
            work_log: crate::plan::WorkLog::default(),
        };
        let agent = shared.agent.clone();
        let states = crate::plan::run_plan_with(&agent, &squads, &ready, &ctx, |step, state| {
            let id = step_ids[step];
            if !matches!(state, StepState::Waiting | StepState::Running { .. }) {
                shared.running.lock().unwrap().remove(&id);
            }
            shared.update_task(id, |t| apply_state(&shared, t, state));
            if let Some(t) = &tracking {
                t.update(step, state);
            }
        })
        .await;
        let mut run = RunRecord::new(
            shared.agent.name(),
            &shared.agent.id().to_string(),
            &text,
            &plan,
            &states,
            started,
        );
        run.id = run_id;
        run.peer_ids = crate::runs::peer_ids(&shared.agent);
        run.workiq = ctx.work_log.records();
        if let Some(t) = tracking {
            let known = t.tracked.problems.len();
            let tracked = t.finish(&run).await;
            show_tracked(&shared, &step_ids, &tracked, known);
            run.tracked = Some(tracked);
        }
        let saved_run = match run.save(&shared.info.profile.runs_dir()) {
            Ok(_) => {
                shared.system(
                    if run.passed() { "info" } else { "warn" },
                    format!("Evidence run {}: {}", run.id, run.verdict_line()),
                );
                Some(run.id.clone())
            }
            Err(e) => {
                shared.system("warn", format!("Could not save the run: {e:#}"));
                None
            }
        };
        if let Some(run_id) = &saved_run {
            for id in step_ids.values() {
                shared.update_task(*id, |t| t.run = Some(run_id.clone()));
            }
        }
        let saved = crate::plan::save_knowledge(
            &kb,
            &crate::plan::with_actual_labels(&plan, &ctx.work_log),
            &states,
            &run.id,
        );
        if !saved.is_empty() {
            shared.system("info", format!("Knowledge saved: {}", saved.join(", ")));
        }
        shared.send(json!({ "type": "runs" }));
        shared.send(json!({ "type": "ops" }));
        crate::schedule::Outcome {
            verdict: run.verdict_line(),
            run_id: saved_run,
        }
    });
    {
        let mut running = s.running.lock().unwrap();
        let tasks = s.tasks.lock().unwrap();
        for id in ids.values() {
            if tasks.iter().any(|t| t.id == *id && !t.finished()) {
                running.insert(*id, handle.abort_handle());
            }
        }
    }
    s.system(
        "task",
        format!(
            "Plan started: {} step{}",
            ids.len(),
            if ids.len() == 1 { "" } else { "s" }
        ),
    );
    Ok(StartedPlan {
        plan: plan_id,
        steps: ids,
        done: handle,
    })
}

/// Runs the profile's scheduled plans like plans started here, so their steps show
/// up as tasks. Checks every 30 seconds (`PROOFKIN_SCHEDULE_TICK_SECS` overrides it).
fn start_scheduler(shared: &AppState) {
    use crate::schedule::{Launch, Outcome, Schedule, Scheduler};
    let weak = Arc::downgrade(shared);
    let launch: Launch = Arc::new(move |schedule: Schedule| {
        let s = weak.upgrade().context("the agent stopped")?;
        let started = start_plan(&s, schedule.plan.clone(), None).map_err(|e| anyhow!(e))?;
        s.system(
            "task",
            format!(
                "Scheduled plan '{}' started ({})",
                schedule.name, schedule.when
            ),
        );
        Ok(Box::pin(async move {
            started.done.await.unwrap_or_else(|_| Outcome {
                verdict: "cancelled: the run was stopped".to_owned(),
                run_id: None,
            })
        }))
    });
    let weak = Arc::downgrade(shared);
    let scheduler = Scheduler::new(shared.info.profile.schedules_path(), launch).on_change(
        Arc::new(move || {
            if let Some(s) = weak.upgrade() {
                s.send(json!({ "type": "schedules" }));
            }
        }),
    );
    let tick = crate::profile::env_setting("SCHEDULE_TICK_SECS")
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);
    scheduler.spawn(Duration::from_secs(tick));
    let _ = shared.scheduler.set(scheduler);
}

fn schedules_json(s: &Shared) -> Result<Value, ApiError> {
    let store = crate::schedule::ScheduleStore::load(&s.info.profile.schedules_path())
        .map_err(|e| bad_request(format!("{e:#}")))?;
    let running = |name: &str| s.scheduler.get().is_some_and(|x| x.is_running(name));
    Ok(json!(
        store
            .schedules
            .iter()
            .map(|sch| {
                let running = running(&sch.name);
                // A run recorded as running that isn't (the agent stopped mid-run).
                let status = match sch.last_status() {
                    _ if running => Some("running"),
                    Some("running") => Some("cancelled"),
                    status => status,
                };
                json!({
                    "name": sch.name,
                    "when": sch.when,
                    "when_parts": crate::schedule::when_parts(&sch.when),
                    "source": sch.source,
                    "enabled": sch.enabled,
                    "running": running,
                    "steps": crate::plan::Plan::parse(&sch.plan).map(|p| p.steps.len()).unwrap_or(0),
                    "plan": sch.plan,
                    "next_unix": sch.next_run().map(|t| t.timestamp()),
                    "next_runs": sch.next_runs(3).iter().map(|t| t.timestamp()).collect::<Vec<_>>(),
                    "retry_unix": sch.retry_unix,
                    "last_run_unix": sch.last_run_unix,
                    "last_result": sch.last_result,
                    "last_status": status,
                    "last_run_id": sch.last_run_id,
                })
            })
            .collect::<Vec<_>>()
    ))
}

async fn list_schedules(State(s): State<AppState>) -> Result<Json<Value>, ApiError> {
    Ok(Json(json!({ "schedules": schedules_json(&s)? })))
}

/// Changes the profile's schedules under their lock, then tells the page.
fn change_schedules<T>(
    s: &Shared,
    change: impl FnOnce(&mut crate::schedule::ScheduleStore) -> anyhow::Result<T>,
) -> Result<T, ApiError> {
    let out = crate::schedule::ScheduleStore::update(&s.info.profile.schedules_path(), change)
        .map_err(|e| bad_request(format!("{e:#}")))?;
    s.send(json!({ "type": "schedules" }));
    Ok(out)
}

#[derive(Deserialize)]
struct ScheduleBody {
    name: String,
    when: String,
    plan: String,
    #[serde(default)]
    source: String,
}

async fn add_schedule(
    State(s): State<AppState>,
    Json(body): Json<ScheduleBody>,
) -> Result<Json<Value>, ApiError> {
    let source = if body.source.trim().is_empty() {
        "web UI".to_owned()
    } else {
        body.source
    };
    let schedule = crate::schedule::Schedule::new(&body.name, &body.when, &body.plan, &source)
        .map_err(|e| bad_request(format!("{e:#}")))?;
    let next = schedule.next_run();
    change_schedules(&s, |store| {
        if store.get(&schedule.name).is_some() {
            bail!(
                "a schedule named '{}' already exists: edit it, or pick another name",
                schedule.name
            );
        }
        store.add(schedule);
        Ok(())
    })?;
    s.system(
        "info",
        format!(
            "Scheduled plan '{}' ({}); next run {}",
            body.name,
            body.when.trim(),
            next.map(|t| t.format("%a %Y-%m-%d %H:%M").to_string())
                .unwrap_or_default()
        ),
    );
    Ok(Json(json!({ "schedules": schedules_json(&s)? })))
}

#[derive(Deserialize)]
struct ScheduleEdit {
    #[serde(default)]
    when: Option<String>,
    #[serde(default)]
    plan: Option<String>,
}

/// Changes when a schedule runs (counting from now) and/or the plan it runs.
async fn edit_schedule(
    State(s): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<ScheduleEdit>,
) -> Result<Json<Value>, ApiError> {
    change_schedules(&s, |store| {
        let schedule = store
            .get_mut(&name)
            .with_context(|| format!("no schedule named '{name}'"))?;
        if let Some(plan) = &body.plan {
            schedule.set_plan(plan)?;
        }
        if let Some(when) = body.when.as_deref().filter(|w| w.trim() != schedule.when) {
            schedule.set_when(when)?;
        }
        Ok(())
    })?;
    Ok(Json(json!({ "schedules": schedules_json(&s)? })))
}

#[derive(Deserialize)]
struct WhenBody {
    when: String,
}

/// What `when` means: its parts and its next three run times, or why it's invalid.
async fn preview_when(Json(body): Json<WhenBody>) -> Json<Value> {
    Json(
        match crate::schedule::upcoming(&body.when, &chrono::Local::now(), 3) {
            Ok(next) => json!({
                "ok": true,
                "parts": crate::schedule::when_parts(&body.when),
                "next": next.iter().map(|t| t.timestamp()).collect::<Vec<_>>(),
            }),
            Err(e) => json!({ "ok": false, "error": format!("{e:#}") }),
        },
    )
}

#[derive(Deserialize)]
struct ToggleBody {
    enabled: bool,
}

/// Pauses a schedule, or resumes it counting from now (paused runs are not caught up).
async fn toggle_schedule(
    State(s): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<ToggleBody>,
) -> Result<Json<Value>, ApiError> {
    change_schedules(&s, |store| {
        store
            .get_mut(&name)
            .with_context(|| format!("no schedule named '{name}'"))?
            .set_enabled(body.enabled);
        Ok(())
    })?;
    Ok(Json(json!({ "schedules": schedules_json(&s)? })))
}

async fn run_schedule(
    State(s): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let scheduler = s
        .scheduler
        .get()
        .ok_or_else(|| bad_request("schedules are not running"))?;
    scheduler
        .trigger(&name)
        .map_err(|e| bad_request(format!("{e:#}")))?;
    Ok(Json(json!({ "schedules": schedules_json(&s)? })))
}

async fn remove_schedule(
    State(s): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    change_schedules(&s, |store| {
        if !store.remove(&name) {
            bail!("no schedule named '{name}'");
        }
        Ok(())
    })?;
    Ok(Json(json!({ "schedules": schedules_json(&s)? })))
}

#[derive(Deserialize)]
struct SquadBody {
    name: String,
    members: Vec<String>,
}

async fn save_squad(
    State(s): State<AppState>,
    Json(body): Json<SquadBody>,
) -> Result<Json<Value>, ApiError> {
    let trusted = s.agent.trusted();
    if let Some(unknown) = body
        .members
        .iter()
        .find(|m| trusted.get_by_name(m).is_none())
    {
        return Err(bad_request(format!("'{unknown}' is not on the allowlist")));
    }
    let saved = {
        let mut squads = s.squads.lock().unwrap();
        squads
            .set(body.name.trim(), body.members)
            .map_err(|e| bad_request(format!("{e:#}")))?;
        squads.save(&s.info.profile.squads_path())
    };
    saved.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("{e:#}") })),
        )
    })?;
    s.system("info", format!("Squad {} saved", body.name.trim()));
    s.broadcast_peers();
    Ok(Json(json!({ "name": body.name.trim() })))
}

async fn remove_squad(
    State(s): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let saved = {
        let mut squads = s.squads.lock().unwrap();
        if !squads.remove(&name) {
            return Err(bad_request(format!("there is no squad {name}")));
        }
        squads.save(&s.info.profile.squads_path())
    };
    saved.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("{e:#}") })),
        )
    })?;
    s.broadcast_peers();
    Ok(Json(json!({ "removed": name })))
}

fn internal(e: impl std::fmt::Display) -> ApiError {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": e.to_string() })),
    )
}

/// The health dashboard: monitors against their SLOs (last 24 hours), incident
/// events, peer task statistics, evidence runs and the knowledge base topics.
async fn ops_state(State(s): State<AppState>) -> Result<Json<Value>, ApiError> {
    use crate::ops::{MonitorStore, monitor_stats, task_stats};
    let profile = &s.info.profile;
    let since = crate::runs::unix_now().saturating_sub(24 * 3600);
    let monitors = MonitorStore::load(&profile.monitors_path())
        .map_err(internal)?
        .monitors;
    let store = profile.ops_store();
    let incidents = store.incidents(since);
    // The whole log: an incident opened days ago may still be open.
    let stats = monitor_stats(
        &monitors,
        &store.samples(since),
        &store.incidents(0),
        &s.agent.shares(),
    );
    let audit: Vec<_> = AuditLog::read(&profile.audit_path(), 10_000)
        .unwrap_or_default()
        .into_iter()
        .filter(|e| e.ts_ms >= since * 1000)
        .collect();
    let runs: Vec<_> = RunRecord::list(&profile.runs_dir())
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r.started_unix >= since)
        .collect();
    let recent: Vec<_> = incidents.iter().rev().take(30).cloned().collect();
    // Folders with checks: what a new monitor can run.
    let checks: Vec<Value> = s
        .agent
        .shares()
        .shares
        .iter()
        .filter(|share| !share.checks.is_empty())
        .map(|share| json!({ "folder": share.name, "checks": share.checks }))
        .collect();
    // Each monitor's health, and what it checks how often.
    let listed: Vec<Value> = stats
        .iter()
        .map(|stat| {
            let mut v = json!(stat);
            if let Some(m) = monitors.iter().find(|m| m.name == stat.name) {
                v["share"] = json!(m.share);
                v["check"] = json!(m.check);
                v["every_secs"] = json!(m.every_secs);
                v["fail_after"] = json!(m.fail_after);
            }
            v
        })
        .collect();
    Ok(Json(json!({
        // Monitors with an open incident (one ongoing problem each).
        "open_incidents": stats.iter().filter(|m| m.open_incident).count(),
        "monitors": listed,
        "checks": checks,
        "incidents": recent,
        "tasks": task_stats(&audit),
        "runs": { "total": runs.len(), "passed": runs.iter().filter(|r| r.passed).count() },
        "knowledge": profile.knowledge().list(),
    })))
}

/// Adds a monitor of one of this agent's folders' checks; the agent starts checking it
/// within seconds.
async fn add_monitor(
    State(s): State<AppState>,
    Json(monitor): Json<crate::ops::Monitor>,
) -> Result<Json<Value>, ApiError> {
    let name = monitor.name.clone();
    let every = monitor.every_secs;
    let shares = s.agent.shares();
    crate::ops::MonitorStore::update(&s.info.profile.monitors_path(), |store| {
        if store.monitors.iter().any(|m| m.name == monitor.name) {
            bail!(
                "there is already a monitor '{}': pick another name, or remove that one first",
                monitor.name
            );
        }
        store.set(monitor, &shares)
    })
    .map_err(|e| bad_request(format!("{e:#}")))?;
    s.system(
        "info",
        format!(
            "Monitor {name} added: checked every {} while this agent runs",
            interval(every)
        ),
    );
    s.send(json!({ "type": "ops" }));
    Ok(Json(json!({ "added": name })))
}

/// How often, in words ("minute", "5 minutes").
fn interval(secs: u64) -> String {
    match secs {
        60 => "minute".to_owned(),
        3600 => "hour".to_owned(),
        s if s % 3600 == 0 => format!("{} hours", s / 3600),
        s if s % 60 == 0 => format!("{} minutes", s / 60),
        s => format!("{s} seconds"),
    }
}

/// Removes a monitor (its history stays; its open incidents are closed).
async fn delete_monitor(
    State(s): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let profile = &s.info.profile;
    let removed = crate::ops::remove_monitor(&profile.monitors_path(), &profile.ops_store(), &name)
        .map_err(|e| internal(format!("{e:#}")))?;
    if !removed {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("there is no monitor '{name}'") })),
        ));
    }
    s.system("info", format!("Monitor {name} removed"));
    s.send(json!({ "type": "ops" }));
    Ok(Json(json!({ "removed": name })))
}

/// Parses a plan and checks it against the connected peers, their folders and
/// checks, squads, knowledge and skills, without running it.
async fn check_plan(State(s): State<AppState>, Json(body): Json<PlanBody>) -> Json<Value> {
    Json(plan_check(&s, &body.text))
}

/// What's wrong with plan `text` before it runs: its syntax, and the peers, folders,
/// checks, squads, skills and knowledge it uses.
fn plan_check(s: &Shared, text: &str) -> Value {
    let plan = match Plan::parse(text) {
        Ok(plan) => plan,
        Err(e) => {
            return json!({
                "ok": false,
                "problems": [{ "step": null, "error": true, "message": format!("{e:#}") }],
            });
        }
    };
    let squads = s.squads.lock().unwrap().clone();
    let recipients = crate::plan::Recipients::of(&s.agent, &s.info.profile);
    let problems = crate::plan::preflight(
        &plan,
        &s.agent.peers(),
        &squads,
        &s.info.profile.knowledge(),
        &s.info.profile.skills(),
        &recipients,
    );
    json!({
        "ok": !problems.iter().any(|p| p.error),
        "problems": problems,
        "steps": plan.steps.len(),
        "claims": plan.claims.len(),
    })
}

/// GitHub: the profile's account, the GitHub CLI accounts it could use, and the
/// linked repositories with the branches agents' tasks made.
async fn github_state(State(s): State<AppState>) -> Result<Json<Value>, ApiError> {
    use crate::github::{RepoStore, gh_accounts, task_branches};
    let profile = s.info.profile.reload().map_err(internal)?;
    let account = match &profile.config.github {
        Some(crate::profile::GithubAuth::Gh { user, host }) => {
            json!({ "user": user, "host": host })
        }
        None if profile.config.owner.is_some() => json!({ "owner": profile.config.owner }),
        None => Value::Null,
    };
    let accounts = tokio::task::spawn_blocking(gh_accounts)
        .await
        .unwrap_or_default();
    let mut repos = Vec::new();
    for repo in RepoStore::load(&profile.repos_path())
        .map_err(internal)?
        .repos
    {
        let branches = task_branches(&repo).await.unwrap_or_default();
        repos.push(json!({
            "name": repo.name, "slug": repo.slug, "host": repo.host, "branch": repo.branch,
            "path": repo.path.display().to_string(), "branches": branches,
        }));
    }
    Ok(Json(json!({
        "account": account,
        "accounts": accounts,
        "repos": repos,
        "serving": s.info.serving,
        // A sign-in that waits for its code to be entered on GitHub, or the last one.
        "flow": s.gh.current(),
    })))
}

#[derive(Deserialize)]
struct GrantBody {
    /// Default: `project`, which the tracker's board needs.
    #[serde(default)]
    scopes: Vec<String>,
}

/// Gives the profile's GitHub account more scopes: gh is switched to it for the
/// refresh and back afterwards (see [`crate::gh_auth`]). Answers once gh shows its code.
async fn github_grant(
    State(s): State<AppState>,
    Json(body): Json<GrantBody>,
) -> Result<Json<Value>, ApiError> {
    let profile = s.info.profile.reload().map_err(internal)?;
    if profile.config.owner.is_some() {
        return Err(bad_request(
            "a digital worker uses its owner's GitHub account: grant the scope on the owner's agent",
        ));
    }
    let Some(crate::profile::GithubAuth::Gh { user, host }) = profile.config.github.clone() else {
        return Err(bad_request("sign in to GitHub first"));
    };
    let scopes = if body.scopes.is_empty() {
        vec!["project".to_owned()]
    } else {
        body.scopes
    };
    crate::gh_auth::check_scopes(&scopes).map_err(|e| bad_request(format!("{e:#}")))?;
    let kind = crate::gh_auth::Kind::Grant {
        login: user,
        scopes,
    };
    let shared = s.clone();
    let flow =
        s.gh.start(kind, &host, move |flow| report_flow(&shared, &flow))
            .await
            .map_err(|e| bad_request(format!("{e:#}")))?;
    Ok(Json(json!({ "flow": flow })))
}

#[derive(Deserialize)]
struct NewLoginBody {
    #[serde(default)]
    host: Option<String>,
    /// Scopes for the new account's token on top of gh's own (e.g. `project`, `workflow`).
    #[serde(default)]
    scopes: Vec<String>,
}

/// Signs another account in to the GitHub CLI and makes the profile use it; gh's active
/// account is switched back afterwards. Answers once gh shows its code.
async fn github_sign_in_new(
    State(s): State<AppState>,
    Json(body): Json<NewLoginBody>,
) -> Result<Json<Value>, ApiError> {
    let profile = s.info.profile.reload().map_err(internal)?;
    if profile.config.owner.is_some() {
        return Err(bad_request(
            "digital workers use their owner's GitHub account",
        ));
    }
    let host = body
        .host
        .map(|h| h.trim().to_ascii_lowercase())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| crate::profile::DEFAULT_GITHUB_HOST.to_owned());
    if !host
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
    {
        return Err(bad_request(format!("{host:?} isn't a GitHub host name")));
    }
    crate::gh_auth::check_scopes(&body.scopes).map_err(|e| bad_request(format!("{e:#}")))?;
    let kind = crate::gh_auth::Kind::SignIn {
        scopes: body.scopes,
    };
    let shared = s.clone();
    let flow =
        s.gh.start(kind, &host, move |flow| {
            let crate::gh_auth::State::Done { login } = &flow.state else {
                report_flow(&shared, &flow);
                return;
            };
            // The profile uses the new account, once GitHub accepts its token.
            let (login, host) = (login.clone(), flow.host.clone());
            tokio::spawn(async move {
                let profile = shared.info.profile.clone();
                let used = tokio::task::spawn_blocking(move || {
                    profile.reload()?.github_login(&login, &host)
                })
                .await;
                match used {
                    Ok(Ok(_)) => report_flow(&shared, &flow),
                    Ok(Err(e)) => shared.system(
                        "warn",
                        format!(
                            "Signed in to GitHub, but the profile can't use the account: {e:#}"
                        ),
                    ),
                    Err(e) => shared.system("warn", format!("{e:#}")),
                }
            });
        })
        .await
        .map_err(|e| bad_request(format!("{e:#}")))?;
    Ok(Json(json!({ "flow": flow })))
}

/// Says in the activity feed how a GitHub CLI sign-in ended.
fn report_flow(s: &Shared, flow: &crate::gh_auth::Flow) {
    use crate::gh_auth::State;
    let back = flow
        .active_before
        .as_deref()
        .map(|a| format!("; the GitHub CLI's active account is {a} again"))
        .unwrap_or_default();
    let who = flow.login.as_deref().unwrap_or("the account");
    let (level, text) = match &flow.state {
        State::Done { login } if flow.kind == "grant" => (
            "info",
            format!(
                "GitHub: {login}'s token now has the {} scope{}{back}",
                flow.scopes.join(", "),
                if flow.scopes.len() == 1 { "" } else { "s" }
            ),
        ),
        State::Done { login } => (
            "info",
            format!("Signed in to GitHub as {login}@{}{back}", flow.host),
        ),
        State::Failed { reason } if flow.kind == "grant" => (
            "warn",
            format!("Granting GitHub access to {who} failed: {reason}"),
        ),
        State::Failed { reason } => ("warn", format!("GitHub sign-in failed: {reason}")),
        State::Cancelled if flow.kind == "grant" => (
            "info",
            format!("Granting GitHub access to {who} cancelled{back}"),
        ),
        State::Cancelled => ("info", format!("GitHub sign-in cancelled{back}")),
        State::Waiting => return,
    };
    s.system(level, text);
    if let Some(note) = &flow.note {
        s.system("warn", note.clone());
    }
}

async fn github_flow(State(s): State<AppState>) -> Json<Value> {
    Json(json!({ "flow": s.gh.current() }))
}

async fn cancel_github_flow(State(s): State<AppState>) -> Json<Value> {
    Json(json!({ "cancelled": s.gh.cancel() }))
}

#[derive(Deserialize)]
struct GithubLoginBody {
    user: String,
    host: String,
}

/// The profile's tracker (see [`crate::tracker`]) and whether its board works.
async fn tracker_state(State(s): State<AppState>) -> Result<Json<Value>, ApiError> {
    let profile = s.info.profile.reload().map_err(internal)?;
    let signed_in = profile.github().is_ok();
    let Some(config) =
        crate::tracker::TrackerConfig::load(&profile.tracker_path()).map_err(internal)?
    else {
        return Ok(Json(json!({ "linked": false, "signed_in": signed_in })));
    };
    Ok(Json(match crate::tracker::open(&profile).await {
        Ok(Some(tracker)) => {
            // Reading what the open runs wait for must not hide the tracker.
            let (waiting, waiting_error) = match tracker.waiting().await {
                Ok(w) => (w, None),
                Err(e) => (Vec::new(), Some(format!("{e:#}"))),
            };
            json!({ "linked": true, "signed_in": true, "tracker": tracker.info(), "waiting": waiting, "waiting_error": waiting_error })
        }
        Ok(None) => json!({ "linked": false, "signed_in": signed_in }),
        // Linked, but GitHub can't be reached with it now: say why, keep the settings.
        Err(e) => json!({
            "linked": true, "signed_in": signed_in, "error": format!("{e:#}"),
            "tracker": { "repo": config.repo, "host": config.host, "project": config.project, "open_runs": config.open_runs },
        }),
    }))
}

#[derive(Deserialize)]
struct TrackerBody {
    repo: String,
    #[serde(default)]
    project: Option<String>,
}

async fn link_tracker(
    State(s): State<AppState>,
    Json(body): Json<TrackerBody>,
) -> Result<Json<Value>, ApiError> {
    let profile = s.info.profile.reload().map_err(internal)?;
    let tracker = crate::tracker::link(&profile, &body.repo, body.project.as_deref())
        .await
        .map_err(|e| bad_request(format!("{e:#}")))?;
    let info = tracker.info();
    s.system(
        "info",
        format!(
            "Plan runs are tracked as issues of {}{}",
            info.repo,
            match (&info.board, &info.board_problem) {
                (Some(b), _) => format!(" on the board {}", b.title),
                (None, Some(_)) => " (the board isn't updated yet: see Settings)".to_owned(),
                (None, None) => String::new(),
            }
        ),
    );
    Ok(Json(
        json!({ "linked": true, "signed_in": true, "tracker": info }),
    ))
}

async fn unlink_tracker(State(s): State<AppState>) -> Result<Json<Value>, ApiError> {
    let path = s.info.profile.tracker_path();
    if path.is_file() {
        std::fs::remove_file(&path).map_err(internal)?;
        s.system(
            "info",
            "Plan runs are no longer tracked on GitHub (their issues stay there)",
        );
    }
    Ok(Json(json!({ "linked": false })))
}

/// The tracker board's items (or the repository's open issues, without a board).
async fn tracker_items(State(s): State<AppState>) -> Result<Json<Value>, ApiError> {
    let profile = s.info.profile.reload().map_err(internal)?;
    let tracker = crate::tracker::open(&profile)
        .await
        .map_err(|e| bad_request(format!("{e:#}")))?
        .ok_or_else(|| bad_request("this agent has no tracker"))?;
    let items = tracker
        .items()
        .await
        .map_err(|e| bad_request(format!("{e:#}")))?;
    Ok(Json(json!({
        "items": items,
        "board": tracker.board.as_ref().map(|b| json!({ "title": b.title, "url": b.url })),
        "repo": tracker.config.repo,
    })))
}

/// Puts the repository's issues and pull requests on the tracker's board.
async fn add_to_board(
    State(s): State<AppState>,
    Json(body): Json<crate::tracker::AddItems>,
) -> Result<Json<Value>, ApiError> {
    let profile = s.info.profile.reload().map_err(internal)?;
    let tracker = crate::tracker::open(&profile)
        .await
        .map_err(|e| bad_request(format!("{e:#}")))?
        .ok_or_else(|| bad_request("this agent has no tracker"))?;
    let result = tracker
        .add_to_board(&body)
        .await
        .map_err(|e| bad_request(format!("{e:#}")))?;
    let new = result.added.iter().filter(|a| !a.already).count();
    s.system(
        "info",
        format!(
            "Put {new} issue{} and pull request{} on the board ({} on it already)",
            if new == 1 { "" } else { "s" },
            if new == 1 { "" } else { "s" },
            result.added.len() - new
        ),
    );
    for problem in &result.problems {
        s.system("warn", format!("Tracker: {problem}"));
    }
    Ok(Json(json!(result)))
}

/// Closes the run issues whose sub-issues are all closed, and says what the rest wait for.
async fn sync_tracker(State(s): State<AppState>) -> Result<Json<Value>, ApiError> {
    let profile = s.info.profile.reload().map_err(internal)?;
    let synced = crate::tracker::sync(&profile)
        .await
        .map_err(|e| bad_request(format!("{e:#}")))?;
    for n in &synced.closed {
        s.system(
            "info",
            format!("Run issue #{n} is closed: all its sub-issues are"),
        );
    }
    Ok(Json(json!(synced)))
}

/// Every 10 minutes (`PROOFKIN_TRACKER_SETTLE_SECS`), closes the tracked run issues
/// whose pull requests have all merged (see [`crate::tracker::settle`]).
fn settle_tracked_runs(shared: &AppState) {
    let secs = crate::profile::env_setting("TRACKER_SETTLE_SECS")
        .and_then(|v| v.parse().ok())
        .unwrap_or(600);
    let weak = Arc::downgrade(shared);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(secs));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let Some(s) = weak.upgrade() else { break };
            let profile = s.info.profile.clone();
            let waiting = crate::tracker::TrackerConfig::load(&profile.tracker_path())
                .ok()
                .flatten()
                .is_some_and(|c| !c.open_runs.is_empty());
            if !waiting {
                continue;
            }
            match crate::tracker::settle(&profile).await {
                Ok(closed) => {
                    for n in closed {
                        s.system(
                            "info",
                            format!("Run issue #{n} is closed: all its sub-issues are"),
                        );
                    }
                }
                Err(e) => eprintln!("settling tracked runs: {e:#}"),
            }
        }
    });
}

async fn github_sign_in(
    State(s): State<AppState>,
    Json(body): Json<GithubLoginBody>,
) -> Result<Json<Value>, ApiError> {
    let mut profile = s.info.profile.reload().map_err(internal)?;
    let login = tokio::task::spawn_blocking(move || {
        profile
            .github_login(&body.user, &body.host)
            .map(|l| (l, body.host))
    })
    .await
    .map_err(internal)?
    .map_err(|e| bad_request(format!("{e:#}")))?;
    s.system(
        "info",
        format!("Signed in to GitHub as {}@{}", login.0, login.1),
    );
    Ok(Json(json!({ "login": login.0 })))
}

#[derive(Deserialize)]
struct LinkBody {
    repo: String,
    name: Option<String>,
    #[serde(default)]
    access: Access,
    /// `name=command` lines.
    #[serde(default)]
    checks: Vec<String>,
    /// Allowlisted peers to grant the folder to.
    #[serde(default)]
    grant: Vec<String>,
}

async fn link_repo(
    State(s): State<AppState>,
    Json(body): Json<LinkBody>,
) -> Result<Json<Value>, ApiError> {
    let profile = s.info.profile.reload().map_err(internal)?;
    let checks: Vec<String> = body
        .checks
        .into_iter()
        .filter(|c| !c.trim().is_empty())
        .collect();
    let checks = crate::shares::parse_checks(&checks).map_err(|e| bad_request(format!("{e:#}")))?;
    let name = body.name.filter(|n| !n.trim().is_empty());
    let (repo, shares) =
        crate::github::link(&profile, &body.repo, name.as_deref(), body.access, checks)
            .await
            .map_err(|e| bad_request(format!("{e:#}")))?;
    s.agent.set_shares(shares);
    for peer in &body.grant {
        let Some(mut entry) = s.agent.trusted().get_by_name(peer).cloned() else {
            continue;
        };
        if !entry.shares.contains(&repo.name) {
            entry.shares.push(repo.name.clone());
        }
        s.agent.allow(entry);
    }
    s.save_trust()?;
    s.system(
        "info",
        format!(
            "Linked {} as folder {} (a worktree per task)",
            repo.slug, repo.name
        ),
    );
    s.broadcast_peers();
    Ok(Json(json!({ "name": repo.name, "branch": repo.branch })))
}

async fn unlink_repo(
    State(s): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let profile = s.info.profile.reload().map_err(internal)?;
    let shares =
        crate::github::unlink(&profile, &name, false).map_err(|e| bad_request(format!("{e:#}")))?;
    s.agent.set_shares(shares);
    s.system("info", format!("Unlinked {name} (the clone stays on disk)"));
    Ok(Json(json!({ "unlinked": name })))
}

fn linked(
    s: &Shared,
    name: &str,
) -> Result<(crate::github::Repo, crate::github::GithubCredentials), ApiError> {
    let profile = s.info.profile.reload().map_err(internal)?;
    let repo = crate::github::RepoStore::load(&profile.repos_path())
        .map_err(internal)?
        .get(name)
        .map_err(|e| bad_request(format!("{e:#}")))?
        .clone();
    let creds = profile
        .github()
        .map_err(|e| bad_request(format!("{e:#}")))?;
    Ok((repo, creds))
}

async fn sync_repo(
    State(s): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let (repo, creds) = linked(&s, &name)?;
    let head = creds
        .sync(&repo)
        .await
        .map_err(|e| bad_request(format!("{e:#}")))?;
    Ok(Json(json!({ "head": head, "branch": repo.branch })))
}

#[derive(Deserialize)]
struct PrBody {
    branch: String,
    title: String,
    #[serde(default)]
    body: String,
    /// Append the proof the branch's commit carries, if it has one (default: yes).
    #[serde(default = "yes")]
    proof: bool,
}

fn yes() -> bool {
    true
}

async fn open_pr(
    State(s): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<PrBody>,
) -> Result<Json<Value>, ApiError> {
    let (repo, creds) = linked(&s, &name)?;
    if body.title.trim().is_empty() {
        return Err(bad_request("the pull request needs a title"));
    }
    let mut text = body.body;
    let mut proven = false;
    creds
        .push(&repo, &body.branch)
        .await
        .map_err(|e| bad_request(format!("{e:#}")))?;
    if body.proof {
        let profile = s.info.profile.reload().map_err(internal)?;
        if let Ok(Some(proof)) = creds.pr_proof(&repo, &body.branch, &profile).await {
            text += &proof.markdown;
            proven = true;
        }
    }
    let url = creds
        .create_pr(&repo, &body.branch, body.title.trim(), &text)
        .await
        .map_err(|e| bad_request(format!("{e:#}")))?;
    s.system(
        "info",
        format!(
            "Opened pull request {url}{}",
            if proven { " with its proof" } else { "" }
        ),
    );
    Ok(Json(json!({ "url": url, "proof": proven })))
}

#[derive(Deserialize)]
struct ProofQuery {
    branch: String,
}

/// The proof a pull request from task branch `branch` would carry (see
/// [`crate::github::branch_proof`]), for the pull request form.
async fn branch_proof(
    State(s): State<AppState>,
    Path(name): Path<String>,
    Query(q): Query<ProofQuery>,
) -> Result<Json<Value>, ApiError> {
    let (repo, _) = linked(&s, &name)?;
    let profile = s.info.profile.reload().map_err(internal)?;
    Ok(Json(
        // The preview assumes the notes ref will reach the repository.
        match crate::github::branch_proof(&repo.path, &q.branch, &profile, true).await {
            Ok(Some(proof)) => json!({ "proof": proof }),
            Ok(None) => json!({
                "proof": null,
                "reason": "Its commit has no signed record (not made by a task of this version).",
            }),
            Err(e) => json!({ "proof": null, "reason": format!("{e:#}") }),
        },
    ))
}

async fn list_skills(State(s): State<AppState>) -> Json<Value> {
    let skills = s.info.profile.skills();
    Json(json!({ "skills": skills.list(), "agents": skills.agents() }))
}

async fn agent_def(
    State(s): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let agent = s.info.profile.skills().agent(&name).map_err(|e| {
        (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("{e:#}") })),
        )
    })?;
    Ok(Json(json!(agent)))
}

async fn skill_text(
    State(s): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let text = s.info.profile.skills().text(&name).map_err(|e| {
        (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("{e:#}") })),
        )
    })?;
    Ok(Json(json!({ "name": name, "text": text })))
}

async fn list_playbooks() -> Result<Json<Value>, ApiError> {
    let mut list = Vec::new();
    for (name, description) in crate::playbooks::list() {
        let vars: Vec<Value> = crate::playbooks::variables(name)
            .map_err(internal)?
            .into_iter()
            .map(|(v, d)| json!({ "name": v, "default": d }))
            .collect();
        list.push(json!({
            "name": name,
            "description": description,
            "vars": vars,
            "outline": crate::playbooks::outline(name).map_err(internal)?,
        }));
    }
    Ok(Json(json!({ "playbooks": list })))
}

#[derive(Deserialize)]
struct PlaybookBody {
    #[serde(default)]
    vars: std::collections::BTreeMap<String, String>,
}

/// Fills in a playbook. The implement-speckit playbook also saves the spec it imports
/// as knowledge; a folder with several specs lists them (`candidates`) to pick from.
async fn render_playbook(
    State(s): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<PlaybookBody>,
) -> Result<Json<Value>, ApiError> {
    let vars: std::collections::BTreeMap<String, String> = body
        .vars
        .into_iter()
        .filter(|(_, v)| !v.trim().is_empty())
        .collect();
    let kb = s.info.profile.knowledge();
    // An imported spec's files are read (and a project searched for one): that blocks.
    let done = tokio::task::spawn_blocking(move || {
        let rendered = crate::playbooks::render_full(&name, &vars)?;
        let saved = rendered.save_knowledge(&kb)?;
        anyhow::Ok((rendered, saved))
    })
    .await
    .map_err(internal)?;
    let (rendered, saved) = done.map_err(|e| {
        let mut body = json!({ "error": format!("{e:#}") });
        if let Some(several) = e.downcast_ref::<crate::spec_import::SeveralSpecs>() {
            body["candidates"] = json!(several.0);
        }
        (StatusCode::BAD_REQUEST, Json(body))
    })?;
    if !saved.is_empty() {
        // The knowledge list is part of the ops view.
        s.send(json!({ "type": "ops" }));
    }
    Ok(Json(
        json!({ "text": rendered.text, "notes": rendered.notes, "knowledge": saved }),
    ))
}

#[derive(Deserialize)]
struct ImportWorkiq {
    topic: String,
    query: String,
    /// Sources as a step's `workiq=` takes them; none: all.
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    replace: bool,
}

/// Imports what Work IQ cites for a query as a marked topic (`kb import workiq`).
async fn import_workiq(
    State(s): State<AppState>,
    Json(body): Json<ImportWorkiq>,
) -> Result<Json<Value>, ApiError> {
    let from = body
        .from
        .as_deref()
        .map(str::trim)
        .filter(|f| !f.is_empty());
    let work: Arc<dyn crate::workiq::context::WorkContext> = match &s.work {
        Some(work) => work.clone(),
        None => Arc::new(
            crate::workiq::context::ClientWork::for_profile(&s.info.profile)
                .map_err(|e| bad_request(format!("{e:#}")))?,
        ),
    };
    let done = crate::workiq::import::import_knowledge(
        work.as_ref(),
        &s.info.profile.knowledge(),
        body.topic.trim(),
        &body.query,
        from,
        body.replace,
    )
    .await
    .map_err(|e| bad_request(format!("{e:#}")))?;
    // The knowledge list is part of the ops view.
    s.send(json!({ "type": "ops" }));
    Ok(Json(json!({
        "topic": done.topic,
        "kept": done.kept,
        "withheld": done.withheld,
        "labels": done.labels,
    })))
}

async fn kb_topic(
    State(s): State<AppState>,
    Path(topic): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let text = s.info.profile.knowledge().read(&topic).map_err(|e| {
        (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("{e:#}") })),
        )
    })?;
    Ok(Json(json!({ "topic": topic, "text": text })))
}

#[derive(Deserialize)]
struct AuditQuery {
    limit: Option<usize>,
}

/// Saved evidence runs, newest first.
async fn list_runs(State(s): State<AppState>) -> Result<Json<Value>, ApiError> {
    let runs = RunRecord::list(&s.info.profile.runs_dir()).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("{e:#}") })),
        )
    })?;
    Ok(Json(json!({ "runs": runs })))
}

/// One run: its summary, the Markdown report and a fresh signature check.
async fn get_run(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    run_details(&s, &id).map(Json).map_err(|e| {
        (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("{e:#}") })),
        )
    })
}

/// Saved run `id`'s run.json as it is, to download: anyone can re-check its
/// signatures with `proofkin runs verify --file`.
async fn run_file(State(s): State<AppState>, Path(id): Path<String>) -> Result<Response, ApiError> {
    let missing = || {
        (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("there is no run {id}") })),
        )
    };
    crate::profile::validate_name(&id).map_err(|_| missing())?;
    let path = s.info.profile.runs_dir().join(&id).join("run.json");
    let bytes = tokio::fs::read(&path).await.map_err(|_| missing())?;
    Ok((
        [
            (header::CONTENT_TYPE, "application/json".to_owned()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{id}.json\""),
            ),
            (header::CACHE_CONTROL, "no-store".to_owned()),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_owned()),
        ],
        bytes,
    )
        .into_response())
}

/// Saved run `id`: its summary, verdict, Markdown report, re-checked signatures and
/// the steps with something to preview.
fn run_details(s: &Shared, id: &str) -> Result<Value> {
    let dir = s.info.profile.runs_dir();
    let run = RunRecord::load(&dir, id)?;
    let (run, checks) = run.reverified(Some(&dir.join(&run.id)));
    let checks: Vec<Value> = checks
        .into_iter()
        .map(|(step, r)| match r {
            Ok(text) => json!({ "step": step, "ok": true, "text": text }),
            Err(e) => json!({ "step": step, "ok": false, "text": format!("{e:#}") }),
        })
        .collect();
    // Steps whose patch created something to preview (POST /api/previews opens it).
    let previews: Vec<Value> = run
        .steps
        .iter()
        .map(|s| s.step.id.as_str())
        .filter_map(|step| {
            let patch =
                std::fs::read_to_string(dir.join(&run.id).join(format!("{step}.patch"))).ok()?;
            let label = crate::preview::label(&crate::preview::sources_from_patch(&patch))?;
            Some(json!({ "step": step, "label": label }))
        })
        .collect();
    Ok(json!({
        "summary": run.summary(),
        "verdict": run.verdict_line(),
        "report": run.render_markdown(),
        "signatures": checks,
        "previews": previews,
        "tracked": run.tracked,
    }))
}

fn approvals_json(s: &Shared) -> Value {
    json!(s.agent.approvals())
}

#[derive(Deserialize)]
struct ApprovalBody {
    approved: bool,
    #[serde(default)]
    note: String,
}

async fn answer_approval(
    State(s): State<AppState>,
    Path(id): Path<u64>,
    Json(body): Json<ApprovalBody>,
) -> Result<Json<Value>, ApiError> {
    if !body.approved && body.note.trim().is_empty() {
        return Err(bad_request("say why you reject it"));
    }
    s.agent
        .answer_approval(id, body.approved, &body.note)
        .await
        .map_err(|e| bad_request(format!("{e:#}")))?;
    s.system(
        "info",
        format!(
            "You {} request #{id}",
            if body.approved {
                "approved"
            } else {
                "rejected"
            }
        ),
    );
    Ok(Json(json!({ "answered": id })))
}

/// The audit log of tasks peers ran on this agent, newest first.
async fn audit(
    State(s): State<AppState>,
    Query(q): Query<AuditQuery>,
) -> Result<Json<Value>, ApiError> {
    if !s.info.serving {
        return Ok(Json(json!({ "enabled": false, "entries": [] })));
    }
    let path = s.info.profile.audit_path();
    let mut entries = AuditLog::read(&path, q.limit.unwrap_or(100).min(1000)).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("reading the audit log: {e}") })),
        )
    })?;
    entries.reverse();
    Ok(Json(json!({ "enabled": true, "entries": entries })))
}

#[derive(Deserialize)]
struct AllowBody {
    ticket: String,
    name: Option<String>,
    #[serde(default)]
    delegate: bool,
    tasks_per_hour: Option<u32>,
    #[serde(default)]
    access: Access,
    /// Folders the peer's tasks may use (default: the `data` share).
    shares: Option<Vec<String>>,
}

async fn allow_peer(
    State(s): State<AppState>,
    Json(body): Json<AllowBody>,
) -> Result<Json<Value>, ApiError> {
    let ticket = Ticket::parse(&body.ticket).map_err(bad_request)?;
    let name = body
        .name
        .map(|n| n.trim().to_owned())
        .filter(|n| !n.is_empty())
        .or_else(|| Some(ticket.name.clone()).filter(|n| !n.is_empty()))
        .ok_or_else(|| bad_request("a bare endpoint id has no name; enter a name"))?;
    validate_name(&name)
        .map_err(|_| bad_request("name may only use letters, digits, '-', '_' and '.'"))?;
    if ticket.addr.id == s.agent.id() {
        return Err(bad_request("that is this agent's own ticket"));
    }
    let shares = body
        .shares
        .unwrap_or_else(|| vec![crate::shares::DATA_SHARE.to_owned()]);
    // Folder grants only matter for peers that may delegate tasks.
    let known = s.agent.shares();
    if body.delegate
        && let Some(missing) = shares.iter().find(|n| known.get(n).is_none())
    {
        return Err(bad_request(format!(
            "this agent shares no folder named '{missing}'"
        )));
    }
    let m365 = s.agent.trusted().m365_of(&ticket.addr.id, &name);
    s.agent.allow(TrustedPeer {
        name: name.clone(),
        id: ticket.addr.id,
        addrs: ticket.addr.addrs,
        can_delegate: body.delegate,
        tasks_per_hour: body
            .tasks_per_hour
            .unwrap_or(crate::trust::DEFAULT_TASKS_PER_HOUR),
        access: if body.delegate {
            body.access
        } else {
            Access::ReadOnly
        },
        shares: shares.clone(),
        m365,
    });
    s.save_trust()?;
    s.system(
        "info",
        format!(
            "Allowlisted {name}{}",
            if body.delegate {
                format!(
                    " (may delegate tasks, {} access, folders: {})",
                    body.access,
                    shares.join(", ")
                )
            } else {
                String::new()
            }
        ),
    );
    s.broadcast_peers();
    Ok(Json(json!({ "name": name })))
}

async fn remove_peer(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let name = s
        .agent
        .trusted()
        .peers
        .iter()
        .find(|p| p.id.to_string() == id)
        .map(|p| p.name.clone());
    if !s.agent.remove_trusted(&id) {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "not on the allowlist" })),
        ));
    }
    s.save_trust()?;
    s.system(
        "info",
        format!("Removed {} from the allowlist", name.unwrap_or(id)),
    );
    s.broadcast_peers();
    Ok(Json(json!({ "removed": true })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: u64, status: &'static str, plan: Option<u64>) -> TaskView {
        TaskView {
            id,
            provider: Provider::Copilot,
            prompt: String::new(),
            target: String::new(),
            status,
            output: None,
            started_ms: 0,
            finished_ms: None,
            plan,
            step: None,
            after: Vec::new(),
            approval: false,
            team_step: false,
            evidence: None,
            preview: None,
            preview_label: None,
            via: None,
            run: None,
            progress: None,
            team: None,
            for_peer: None,
            issue: None,
            plan_issue: None,
        }
    }

    #[test]
    fn the_oldest_finished_tasks_go_first_and_a_plans_steps_together() {
        let mut tasks = vec![
            task(1, "running", None),
            task(2, "done", Some(10)),
            task(3, "failed", Some(10)),
            task(4, "done", None),
            task(5, "done", Some(20)),
            task(6, "running", Some(20)),
            task(7, "cancelled", None),
        ];
        prune_tasks(&mut tasks, 4);
        let ids: Vec<u64> = tasks.iter().map(|t| t.id).collect();
        // Plan 10 has ended (both its steps go), then task 4; plan 20 still runs.
        assert_eq!(ids, [1, 5, 6, 7]);
        prune_tasks(&mut tasks, 1);
        let ids: Vec<u64> = tasks.iter().map(|t| t.id).collect();
        assert_eq!(ids, [1, 5, 6], "running tasks and running plans stay");
    }
}
