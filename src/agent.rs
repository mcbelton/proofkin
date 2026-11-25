use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use iroh::endpoint::{Connection, RecvStream, SendStream, presets};
use iroh::{Endpoint, EndpointAddr, EndpointId, SecretKey};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{Notify, Semaphore, mpsc, oneshot, watch};

use crate::audit::{AuditEntry, AuditLog, Outcome};
use crate::evidence::{
    CHECK_TIMEOUT, Evidence, EvidenceKind, MAX_ATTEMPTS, retry_prompt, run_built_in, run_check,
    sha256_hex,
};
use crate::llm::{Access, Executor, LlmTask, Provider, TaskContext};
use crate::protocol::{ALPN, Message, Origin, Phase, Progress, write_message};
use crate::shares::{DATA_SHARE, Share, ShareInfo, ShareStore, effective_access};
use crate::trust::{TrustStore, TrustedPeer};
use crate::worktree::{Worktree, branch_part};

/// Maximum number of peer-delegated tasks an agent runs at once; more are queued.
pub const MAX_CONCURRENT_TASKS: usize = 2;
const HOUR: Duration = Duration::from_secs(3600);
/// How long a dropped peer may take to reconnect before `PeerDisconnected` is reported.
const DISCONNECT_GRACE: Duration = Duration::from_secs(2);
/// How long tasks wait for a dropped peer to come back before they fail (delegated
/// to it) or stop (asked by it), by default.
pub const TASK_GRACE: Duration = Duration::from_secs(60);

/// How the agent reaches peers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum Network {
    /// Over the internet: NAT hole punching, public relays as fallback, and
    /// lookup of peers by endpoint id (n0 relay and DNS services).
    #[default]
    Internet,
    /// Direct IP connections only (LAN, VPN, tests). No relays or lookup services.
    Local,
}

pub struct AgentConfig {
    /// Name this agent announces to peers.
    pub name: String,
    /// Identity; the public half is the agent's endpoint id.
    pub secret_key: SecretKey,
    pub network: Network,
    /// UDP socket to bind (default: any port on all interfaces).
    pub bind: Option<SocketAddr>,
    /// Peers this agent accepts connections from and dials.
    pub trust: TrustStore,
    /// Runs LLM tasks that allowlisted peers delegate (none: serve nothing).
    pub executor: Option<Arc<dyn Executor>>,
    /// Where to record tasks peers ask this agent to run (none: no audit log).
    pub audit: Option<Arc<AuditLog>>,
    /// Folders peers' tasks may run in (each peer only in those granted to it).
    pub shares: ShareStore,
    /// For a digital worker: the owner profile, shown to peers.
    pub owner: Option<String>,
    /// What this agent is for, shown to peers.
    pub role: Option<String>,
    /// Its providers' models: the defaults and the ones tasks may ask for, told to peers
    /// (and, see [`crate::models`], the reason a task asking for another is refused).
    pub models: crate::models::Models,
    /// Where per-task git worktrees are created (folders shared with `--worktrees`).
    pub worktrees_dir: PathBuf,
    /// Runs the team plans of team steps peers hand this agent (none: it refuses them).
    pub team: Option<Arc<crate::team::TeamRunner>>,
    /// Opens and follows the pull requests of peers' pull request steps, in folders whose
    /// owner allows them (none: it refuses them; see [`crate::pulls`]).
    pub pulls: Option<Arc<dyn crate::pulls::PullRequests>>,
    /// The program that serves a task's checks to its model while it works (this program;
    /// see [`crate::checks_mcp`]). None: the model gets no such tool.
    pub task_checks: Option<PathBuf>,
    /// How long tasks wait for a dropped peer to come back (see [`TASK_GRACE`]).
    pub task_grace: Duration,
    /// Where the next id for the tasks it gives peers is kept, so ids go on after a
    /// restart (none: they start at 1). A peer still running a task from before must
    /// never take a new task for it, nor a plan resumed after a restart a new one's.
    pub task_ids: Option<PathBuf>,
}

impl AgentConfig {
    pub fn new(name: impl Into<String>, secret_key: SecretKey) -> Self {
        Self {
            name: name.into(),
            secret_key,
            network: Network::Internet,
            bind: None,
            trust: TrustStore::default(),
            executor: None,
            audit: None,
            shares: ShareStore::default(),
            owner: None,
            role: None,
            models: crate::models::Models::new(),
            worktrees_dir: std::env::temp_dir().join("proofkin-worktrees"),
            team: None,
            pulls: None,
            task_checks: None,
            task_grace: TASK_GRACE,
            task_ids: None,
        }
    }
}

/// Notifications surfaced to the application embedding an [`Agent`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    PeerConnected {
        name: String,
        id: EndpointId,
    },
    PeerDisconnected {
        name: String,
        id: EndpointId,
    },
    /// A connection from (or to) a key that is not on the allowlist was refused.
    PeerRejected {
        id: EndpointId,
    },
    /// A peer's load changed (see [`PeerInfo::load`]).
    PeerBusy {
        id: EndpointId,
    },
    /// A peer's models, or the ones its account offers, changed (see
    /// [`PeerInfo::models`] and [`PeerInfo::offers`]).
    PeerModels {
        id: EndpointId,
    },
    /// This agent's models, or the ones its accounts offer, changed (see
    /// [`Agent::set_models`] and [`Agent::set_offers`]).
    ModelsChanged,
    Chat {
        from: String,
        text: String,
    },
    /// A peer delegated a task to this agent and it was accepted.
    TaskReceived {
        from: String,
        task_id: u64,
        provider: Provider,
        /// What it is part of on the peer's side (its run and step), if it says.
        origin: Option<Origin>,
        /// A team step: this agent's team runs it (with its folder's team plan), not
        /// a model of `provider`.
        team: bool,
    },
    /// A team step a peer handed this agent (task `task_id` from `from`) started its
    /// team run `run`, with these steps.
    TeamRun {
        from: String,
        task_id: u64,
        run: String,
        steps: Vec<crate::plan::Step>,
    },
    /// A step of that team run moved on.
    TeamStep {
        from: String,
        task_id: u64,
        step: String,
        state: Box<crate::plan::StepState>,
    },
    /// A task received from a peer finished (done, failed or cancelled) and its
    /// result was sent back.
    TaskCompleted {
        from: String,
        task_id: u64,
        outcome: Outcome,
    },
    /// A peer's task was not run (not allowed to delegate, over its hourly limit,
    /// or a provider this agent doesn't serve).
    TaskRefused {
        from: String,
        task_id: u64,
        reason: String,
    },
    /// Someone (a peer, or this agent's own plan) asks this agent's user to approve
    /// a step; answer with [`Agent::answer_approval`].
    ApprovalRequested {
        id: u64,
        from: String,
        title: String,
    },
    /// An approval request was answered or withdrawn.
    ApprovalClosed {
        id: u64,
    },
}

/// A peer's result for a delegated task or approval.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Delivered {
    /// Done (and every check passed), or approved.
    pub ok: bool,
    pub output: String,
    pub evidence: Option<Evidence>,
    /// The task's commit as a patch, if it ran in a worktree and changed files.
    pub patch: Option<String>,
    /// The evidence is signed by the peer the task was sent to, for exactly this
    /// output (and patch).
    pub verified: bool,
    /// The task didn't run to the end there: the peer refused it, or stopped first.
    pub refused: bool,
    /// The model that did the work, as the peer says (older peers don't).
    pub model: Option<String>,
}

/// The first id for the tasks an agent gives peers: the one kept at `path` (see
/// [`AgentConfig::task_ids`]), else 1.
fn first_task_id(path: Option<&Path>) -> u64 {
    path.and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|text| text.trim().parse::<u64>().ok())
        .unwrap_or(1)
        .max(1)
}

/// A delegated task failed because its peer dropped and didn't come back in time.
#[derive(Debug)]
pub struct PeerGone(pub String);

impl std::fmt::Display for PeerGone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PeerGone {}

/// How busy a peer says it is with other agents' tasks (see [`Message::Load`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct PeerLoad {
    pub running: u32,
    pub queued: u32,
    pub slots: u32,
    /// Of those, this agent's.
    pub yours: u32,
}

/// An approval request waiting for this agent's user.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ApprovalInfo {
    pub id: u64,
    pub from: String,
    pub title: String,
    pub text: String,
    /// It comes with something to preview: "prototype" or "review pack" (see
    /// [`Agent::approval_preview`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview: Option<&'static str>,
    /// It may also be answered by asking for changes (see [`Answer::Changes`]).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub changes: bool,
}

/// How a person answers an approval request (see [`Agent::answer`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    Approve,
    Reject,
    /// Not yet: the work is done again with their note, then they are asked again. Only
    /// for a request that offers it ([`ApprovalInfo::changes`]).
    Changes,
}

/// How the signed answer to an approval request that asks for changes starts; the
/// approver's name and note follow (see [`Agent::answer`]).
pub const CHANGES_ASKED: &str = "Changes asked for by ";

/// An approval request: what the approver sees, and how they may answer it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApprovalAsk {
    pub title: String,
    pub text: String,
    /// What they can look at before answering, by path in the project: the UX prototype,
    /// review decks and design documents (see [`crate::preview`]).
    pub preview: BTreeMap<String, String>,
    /// They may also ask for changes (see [`Answer::Changes`]).
    pub changes: bool,
}

struct IncomingApproval {
    info: ApprovalInfo,
    requester: EndpointId,
    task_id: u64,
    preview: crate::preview::Sources,
}

/// Most open approval requests one peer may have with this agent.
const MAX_OPEN_APPROVALS: usize = 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerInfo {
    /// Local alias from the allowlist.
    pub name: String,
    pub id: EndpointId,
    pub capabilities: Vec<Provider>,
    /// Folders this peer shares with us for tasks (name and access we get).
    pub shares: Vec<ShareInfo>,
    /// Set if the peer is a digital worker: the profile that owns it.
    pub owner: Option<String>,
    /// What the peer is for, as its owner describes it.
    pub role: Option<String>,
    /// How busy it says it is (newer peers report it).
    pub load: Option<PeerLoad>,
    /// Its providers' default models and the ones tasks may ask for (empty: it didn't
    /// say, as older peers don't, or any model).
    pub models: crate::models::Models,
    /// The models its accounts offer that tasks may ask for, with the reasoning efforts
    /// each supports (empty: it lists none, as older peers and Claude and Codex don't).
    pub offers: crate::models::Offers,
}

struct PeerHandle {
    conn_id: u64,
    name: String,
    addr: EndpointAddr,
    capabilities: Vec<Provider>,
    shares: Vec<ShareInfo>,
    owner: Option<String>,
    role: Option<String>,
    load: Option<PeerLoad>,
    models: crate::models::Models,
    offers: crate::models::Offers,
    tx: mpsc::UnboundedSender<Message>,
    close: Arc<Notify>,
}

/// A task this agent delegated, waiting for `peer`'s result. Tied to the peer, not
/// a connection, so it survives a duplicate connection being replaced.
struct PendingTask {
    peer: EndpointId,
    reply: oneshot::Sender<Result<Delivered, String>>,
    /// The request, re-sent over every new connection to `peer` until answered: a
    /// request queued on a connection that is then replaced would otherwise be lost.
    request: Option<Message>,
    /// Where the peer's progress reports go (none for approvals).
    progress: Option<mpsc::UnboundedSender<Progress>>,
}

/// How long a served task's result is kept to answer a re-sent request.
const RECENT_RESULTS: Duration = Duration::from_secs(600);

/// The most alerts held for an allowlisted peer that isn't connected, and for how
/// long: they are sent when it connects.
const HELD_ALERTS: usize = 20;
const HOLD_ALERTS_FOR: Duration = Duration::from_secs(24 * 3600);

/// A served task, the result sent for it, and when.
type RecentResult = (LlmTask, Message, Instant);

/// What serving a peer's task produced.
struct Served {
    output: String,
    evidence: Evidence,
    patch: Option<String>,
    /// It did what was asked: every check passed (or the team run passed).
    ok: bool,
    /// The model that did the work, if the provider said (none for a team step).
    model: Option<String>,
}

/// A served task's place: waiting for a slot, or holding one (see
/// [`Inner::take_slot`]). Leaving it tells the queue and peers.
struct Place<'a> {
    inner: &'a Inner,
    key: (EndpointId, u64),
    holding: bool,
}

impl Drop for Place<'_> {
    fn drop(&mut self) {
        {
            let mut slots = self.inner.slots.lock().unwrap();
            if self.holding {
                slots.holding.remove(&self.key);
            } else {
                slots.queue.retain(|k| *k != self.key);
            }
        }
        self.inner.report_queue();
        self.inner.broadcast_load();
    }
}

/// Sends `TaskCancel` to the peer if a delegated task's future is dropped before
/// the result arrives (and withdraws a local approval request).
struct CancelOnDrop {
    inner: Arc<Inner>,
    peer: EndpointId,
    task_id: u64,
    armed: bool,
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // A run's step this agent leaves running while it stops (see
        // [`Agent::keep_on_exit`]): the run waits for it again after the restart.
        let kept = self.inner.kept.lock().unwrap().remove(&self.task_id);
        if kept && *self.inner.shutdown.borrow() {
            return;
        }
        self.inner.pending.lock().unwrap().remove(&self.task_id);
        if self.peer == self.inner.endpoint.id() {
            self.inner.withdraw_approval(&self.peer, self.task_id);
        } else if let Some(handle) = self.inner.peers.lock().unwrap().get(&self.peer) {
            let _ = handle.tx.send(Message::TaskCancel {
                task_id: self.task_id,
            });
        }
    }
}

/// A task sent to a peer, whose result (and progress) is still to come; dropping it
/// tells the peer to stop. See [`Agent::send_task`].
pub struct Delegation {
    guard: CancelOnDrop,
    reply: oneshot::Receiver<Result<Delivered, String>>,
    progress: mpsc::UnboundedReceiver<Progress>,
}

impl Delegation {
    /// The task's id on the wire (what the peer's log calls it).
    pub fn task_id(&self) -> u64 {
        self.guard.task_id
    }

    /// Waits for the result, giving `on_progress` what the peer reports meanwhile.
    pub async fn finish(mut self, mut on_progress: impl FnMut(Progress)) -> Result<Delivered> {
        let result = loop {
            tokio::select! {
                result = &mut self.reply => break result,
                Some(progress) = self.progress.recv() => on_progress(progress),
            }
        };
        self.guard.armed = false;
        reply_result(result)
    }
}

/// A pending task's reply as the caller sees it: failures from the channel mean the
/// peer is gone ([`PeerGone`]).
fn reply_result(
    result: Result<Result<Delivered, String>, oneshot::error::RecvError>,
) -> Result<Delivered> {
    match result {
        Ok(Ok(delivered)) => Ok(delivered),
        Ok(Err(error)) => Err(anyhow::Error::new(PeerGone(error))),
        Err(_) => Err(anyhow!("delegated task was dropped")),
    }
}

/// A peer's task this agent serves: whether it got a slot, for load reports.
#[derive(Default)]
struct Slots {
    /// Tasks waiting for a slot, in order.
    queue: VecDeque<(EndpointId, u64)>,
    /// Tasks holding a slot.
    holding: HashSet<(EndpointId, u64)>,
}

struct Inner {
    name: String,
    endpoint: Endpoint,
    trust: Mutex<TrustStore>,
    peers: Mutex<HashMap<EndpointId, PeerHandle>>,
    dialing: Mutex<HashSet<EndpointId>>,
    events: mpsc::UnboundedSender<Event>,
    next_conn_id: AtomicU64,
    shutdown: watch::Sender<bool>,
    executor: Option<Arc<dyn Executor>>,
    audit: Option<Arc<AuditLog>>,
    shares: Mutex<ShareStore>,
    owner: Option<String>,
    role: Option<String>,
    /// Its providers' models, as now set (see [`Agent::set_models`]).
    models: Mutex<crate::models::Models>,
    /// The models its accounts offer, as last listed (see [`Agent::set_offers`]).
    offers: Mutex<crate::models::Offers>,
    task_slots: Semaphore,
    task_log: Mutex<HashMap<EndpointId, VecDeque<Instant>>>,
    pending: Mutex<HashMap<u64, PendingTask>>,
    /// Cancel senders of tasks peers asked us to run, by (peer, task id); dropping
    /// one cancels its task.
    serving: Mutex<HashMap<(EndpointId, u64), oneshot::Sender<()>>>,
    /// Results of recently served tasks, with the task, by (peer, task id): a re-sent
    /// request for a task already done gets its result again instead of a re-run.
    recent: Mutex<HashMap<(EndpointId, u64), RecentResult>>,
    /// Peers reported via `PeerConnected` and not yet via `PeerDisconnected`.
    announced: Mutex<HashSet<EndpointId>>,
    /// Alerts for allowlisted peers that weren't connected, with when they were
    /// raised; sent when the peer connects (see [`Agent::alert`]).
    held: Mutex<HashMap<EndpointId, VecDeque<(Instant, String)>>>,
    next_task_id: AtomicU64,
    /// Where `next_task_id` is kept (see [`AgentConfig::task_ids`]), and a lock so the
    /// file always ends up with the latest value.
    task_ids: Option<PathBuf>,
    task_ids_write: Mutex<()>,
    /// Tasks (and approval requests) left running when this agent stops (see
    /// [`Agent::keep_on_exit`]).
    kept: Mutex<HashSet<u64>>,
    worktrees_dir: PathBuf,
    /// Approval requests waiting for this agent's user, by local id.
    approvals: Mutex<BTreeMap<u64, IncomingApproval>>,
    /// Peers' tasks waiting for a slot and holding one.
    slots: Mutex<Slots>,
    /// What each served task last reported, by (peer, task id): sent again when its
    /// requester asks again after a reconnect.
    last_progress: Mutex<HashMap<(EndpointId, u64), Progress>>,
    team: Option<Arc<crate::team::TeamRunner>>,
    pulls: Option<Arc<dyn crate::pulls::PullRequests>>,
    task_checks: Option<PathBuf>,
    task_grace: Duration,
    /// When peers that dropped were last seen, until they come back or are given up.
    left: Mutex<HashMap<EndpointId, Instant>>,
}

/// A peer-to-peer agent on an iroh QUIC endpoint. Cheap to clone; all clones
/// share the same node.
#[derive(Clone)]
pub struct Agent {
    inner: Arc<Inner>,
}

type Bound = (Agent, mpsc::UnboundedReceiver<Event>);

impl Agent {
    pub async fn bind(config: AgentConfig) -> Result<Bound> {
        let builder = match config.network {
            Network::Internet => Endpoint::builder(presets::N0),
            Network::Local => Endpoint::builder(presets::Minimal),
        };
        let mut builder = builder
            .secret_key(config.secret_key)
            .alpns(vec![ALPN.to_vec()]);
        if let Some(addr) = config.bind {
            builder = builder
                .bind_addr(addr)
                .map_err(|e| anyhow!("invalid bind address {addr}: {e}"))?;
        }
        let endpoint = builder.bind().await.context("starting the iroh endpoint")?;
        let (events, rx) = mpsc::unbounded_channel();
        let inner = Arc::new(Inner {
            name: config.name,
            endpoint,
            trust: Mutex::new(config.trust),
            peers: Mutex::new(HashMap::new()),
            dialing: Mutex::new(HashSet::new()),
            events,
            next_conn_id: AtomicU64::new(0),
            shutdown: watch::Sender::new(false),
            executor: config.executor,
            audit: config.audit,
            shares: Mutex::new(config.shares),
            owner: config.owner,
            role: config.role,
            models: Mutex::new(config.models),
            offers: Mutex::new(crate::models::Offers::new()),
            task_slots: Semaphore::new(MAX_CONCURRENT_TASKS),
            task_log: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
            serving: Mutex::new(HashMap::new()),
            recent: Mutex::new(HashMap::new()),
            announced: Mutex::new(HashSet::new()),
            held: Mutex::new(HashMap::new()),
            next_task_id: AtomicU64::new(first_task_id(config.task_ids.as_deref())),
            task_ids: config.task_ids,
            task_ids_write: Mutex::new(()),
            kept: Mutex::new(HashSet::new()),
            worktrees_dir: config.worktrees_dir,
            approvals: Mutex::new(BTreeMap::new()),
            slots: Mutex::new(Slots::default()),
            last_progress: Mutex::new(HashMap::new()),
            team: config.team,
            pulls: config.pulls,
            task_checks: config.task_checks,
            task_grace: config.task_grace,
            left: Mutex::new(HashMap::new()),
        });
        tokio::spawn(accept_loop(inner.clone()));
        Ok((Self { inner }, rx))
    }

    pub fn name(&self) -> &str {
        &self.inner.name
    }

    /// This agent's public key; how peers identify and allowlist it.
    pub fn id(&self) -> EndpointId {
        self.inner.endpoint.id()
    }

    /// Current addresses (relay URL and/or IPs) other agents can dial.
    pub fn addr(&self) -> EndpointAddr {
        self.inner.endpoint.addr()
    }

    /// Waits until the endpoint is reachable through a relay (internet mode).
    pub async fn online(&self, timeout: Duration) -> bool {
        tokio::time::timeout(timeout, self.inner.endpoint.online())
            .await
            .is_ok()
    }

    /// Providers this agent serves to peers (empty without an executor).
    pub fn capabilities(&self) -> Vec<Provider> {
        self.inner.capabilities()
    }

    /// Folders this agent shares (peers see only those granted to them).
    pub fn shares(&self) -> ShareStore {
        self.inner.shares.lock().unwrap().clone()
    }

    /// Dials an allowlisted peer. Returns once the connection is established;
    /// the handshake completes asynchronously and is reported via [`Event::PeerConnected`].
    pub async fn connect(&self, addr: impl Into<EndpointAddr>) -> Result<()> {
        dial(self.inner.clone(), addr.into()).await
    }

    /// Opens a fresh connection to a connected peer (e.g. after a network change).
    /// Tasks in flight either way carry on over whichever connection is kept.
    pub async fn reconnect(&self, peer: EndpointId) -> Result<()> {
        let addr = self
            .inner
            .peers
            .lock()
            .unwrap()
            .get(&peer)
            .map(|p| p.addr.clone())
            .ok_or_else(|| anyhow!("not connected to {}", peer.fmt_short()))?;
        dial_with(self.inner.clone(), addr, true).await
    }

    /// Dials every allowlisted peer that isn't connected, now and then every
    /// `interval`, so the mesh heals after restarts and network changes.
    pub fn keep_connected(&self, interval: Duration) {
        let inner = self.inner.clone();
        tokio::spawn(async move {
            let mut shutdown = inner.shutdown.subscribe();
            loop {
                let trusted: Vec<TrustedPeer> = inner.trust.lock().unwrap().peers.clone();
                for peer in trusted {
                    let inner = inner.clone();
                    tokio::spawn(async move {
                        let _ = dial(inner, peer.addr()).await;
                    });
                }
                tokio::select! {
                    _ = tokio::time::sleep(interval) => {}
                    _ = shutdown.wait_for(|stopped| *stopped) => return,
                }
            }
        });
    }

    pub fn peers(&self) -> Vec<PeerInfo> {
        let mut peers: Vec<_> = self
            .inner
            .peers
            .lock()
            .unwrap()
            .iter()
            .map(|(id, p)| PeerInfo {
                name: p.name.clone(),
                id: *id,
                capabilities: p.capabilities.clone(),
                shares: p.shares.clone(),
                owner: p.owner.clone(),
                role: p.role.clone(),
                load: p.load,
                models: p.models.clone(),
                offers: p.offers.clone(),
            })
            .collect();
        peers.sort_by(|a, b| a.name.cmp(&b.name));
        peers
    }

    /// Tasks (and approvals) this agent sent `peer` and is still waiting for.
    pub fn pending_to(&self, peer: EndpointId) -> usize {
        self.inner
            .pending
            .lock()
            .unwrap()
            .values()
            .filter(|p| p.peer == peer)
            .count()
    }

    /// Closes the connection to `peer` without forgetting it: tasks in flight wait
    /// for it to reconnect (up to the task grace). For tools and tests.
    pub fn disconnect(&self, peer: EndpointId) -> bool {
        match self.inner.peers.lock().unwrap().get(&peer) {
            Some(handle) => {
                handle.close.notify_one();
                true
            }
            None => false,
        }
    }

    /// First connected peer (by name) that serves `provider`.
    pub fn find_peer(&self, provider: Provider) -> Option<PeerInfo> {
        self.peers()
            .into_iter()
            .find(|p| p.capabilities.contains(&provider))
    }

    /// The current allowlist.
    pub fn trusted(&self) -> TrustStore {
        self.inner.trust.lock().unwrap().clone()
    }

    /// Replaces the shared folders (e.g. a newly linked repository); connected peers
    /// learn what they may now use right away. Persisting them is up to the caller.
    pub fn set_shares(&self, shares: ShareStore) {
        *self.inner.shares.lock().unwrap() = shares;
        let ids: Vec<EndpointId> = self.inner.peers.lock().unwrap().keys().copied().collect();
        for id in ids {
            let shares = self.inner.shares_for(&id);
            if let Some(handle) = self.inner.peers.lock().unwrap().get(&id) {
                let _ = handle.tx.send(Message::Shares { shares });
            }
        }
    }

    /// Reloads the shared folders from `path` (the built-in data folder is at `data`)
    /// whenever the file changes, checking every `every`; e.g. `proofkin mcp remove`
    /// or `share remove` run while the agent runs take effect for peers right away.
    pub fn watch_shares(&self, path: PathBuf, data: PathBuf, every: Duration) {
        let agent = self.clone();
        tokio::spawn(async move {
            let mut shutdown = agent.inner.shutdown.subscribe();
            let modified =
                |p: &std::path::Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
            let (mut seen, mut reported) = (modified(&path), None);
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(every) => {}
                    _ = shutdown.wait_for(|stopped| *stopped) => return,
                }
                let now = modified(&path);
                if now == seen {
                    continue;
                }
                match ShareStore::load(&path, data.clone()) {
                    Ok(shares) => {
                        seen = now;
                        if shares != agent.shares() {
                            agent.set_shares(shares);
                        }
                    }
                    // Tried again next time (a writer may hold the file for a moment).
                    Err(e) if reported != now => {
                        reported = now;
                        eprintln!("could not reload the shared folders: {e:#}");
                    }
                    Err(_) => {}
                }
            }
        });
    }

    /// This agent's providers' models, as now set.
    pub fn models(&self) -> crate::models::Models {
        self.inner.models.lock().unwrap().clone()
    }

    /// The models each provider offers this agent's accounts, as last listed (all of
    /// them, the ones it doesn't allow and those its account can't use too).
    pub fn listed(&self) -> crate::models::Offers {
        self.inner.offers.lock().unwrap().clone()
    }

    /// The models its accounts offer that its tasks may ask for, per provider listed:
    /// what peers learn for the providers it serves (see [`crate::models::for_peers`]).
    pub fn offers(&self) -> crate::models::Offers {
        self.inner.offers_for(None)
    }

    /// Changes this agent's models (e.g. from the web UI, or `profile.json` changed):
    /// its tasks run with them from now on, and connected peers learn them right
    /// away. Persisting them is up to the caller.
    pub fn set_models(&self, models: crate::models::Models) {
        {
            let mut now = self.inner.models.lock().unwrap();
            if *now == models {
                return;
            }
            *now = models.clone();
        }
        if let Some(executor) = &self.inner.executor {
            executor.set_models(models);
        }
        self.inner.models_changed();
    }

    /// Replaces the models `provider` offers this agent's account (as it lists them);
    /// connected peers learn the ones they may ask for right away.
    pub fn set_offers(&self, provider: Provider, offered: Vec<crate::models::Offered>) {
        {
            let mut offers = self.inner.offers.lock().unwrap();
            if offers.get(&provider) == Some(&offered) {
                return;
            }
            if offered.is_empty() {
                offers.remove(&provider);
            } else {
                offers.insert(provider, offered);
            }
        }
        self.inner.models_changed();
    }

    /// Lists the models each of `executor`'s providers offers this agent's accounts
    /// (see [`Agent::set_offers`]); returns why a list failed, per provider.
    pub async fn learn_offers(&self, executor: &dyn Executor) -> BTreeMap<Provider, String> {
        let mut failed = BTreeMap::new();
        for provider in executor.providers() {
            match executor.list_models(provider).await {
                Ok(offered) => self.set_offers(provider, offered),
                Err(e) => {
                    failed.insert(provider, format!("{e:#}"));
                }
            }
        }
        failed
    }

    /// Applies `profile`'s models (with its owner's, for a digital worker) whenever its
    /// `profile.json`, or its owner's, changes, checking every `every`: e.g. `proofkin
    /// models allow` run while the agent runs, or the owner's Models dialog, take effect
    /// for peers right away. `executor` runs this agent's own tasks, if it isn't the
    /// agent's.
    pub fn watch_models(
        &self,
        profile: crate::profile::Profile,
        executor: Option<Arc<dyn Executor>>,
        every: Duration,
    ) {
        let agent = self.clone();
        let mut files = vec![profile.dir.join("profile.json")];
        if let Some(owner) = &profile.config.owner
            && let Some(root) = profile.dir.parent()
        {
            files.push(root.join(owner).join("profile.json"));
        }
        let modified = |files: &[PathBuf]| -> Vec<Option<std::time::SystemTime>> {
            files
                .iter()
                .map(|f| std::fs::metadata(f).and_then(|m| m.modified()).ok())
                .collect()
        };
        // As they are now: a change from here on applies.
        let (mut seen, mut reported) = (modified(&files), None);
        tokio::spawn(async move {
            let mut shutdown = agent.inner.shutdown.subscribe();
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(every) => {}
                    _ = shutdown.wait_for(|stopped| *stopped) => return,
                }
                let now = modified(&files);
                if now == seen {
                    continue;
                }
                match profile.reload() {
                    Ok(fresh) => {
                        seen = now;
                        let models = fresh.models();
                        if let Some(executor) = &executor {
                            executor.set_models(models.clone());
                        }
                        agent.set_models(models);
                    }
                    // Tried again next time (a writer may hold the file for a moment).
                    Err(e) if reported.as_ref() != Some(&now) => {
                        reported = Some(now);
                        eprintln!("could not reload the model settings: {e:#}");
                    }
                    Err(_) => {}
                }
            }
        });
    }

    /// Adds or updates an allowlisted peer and dials it. Persisting the allowlist
    /// is up to the caller.
    pub fn allow(&self, peer: TrustedPeer) {
        let (id, addr) = (peer.id, peer.addr());
        self.inner.trust.lock().unwrap().allow(peer);
        // A connected peer learns its new folder grants right away.
        let shares = self.inner.shares_for(&id);
        if let Some(handle) = self.inner.peers.lock().unwrap().get(&id) {
            let _ = handle.tx.send(Message::Shares { shares });
        }
        let inner = self.inner.clone();
        tokio::spawn(async move {
            let _ = dial(inner, addr).await;
        });
    }

    /// Removes a peer (by name or id) from the allowlist and disconnects it.
    /// Returns whether it was on the allowlist.
    pub fn remove_trusted(&self, name_or_id: &str) -> bool {
        let ids: Vec<EndpointId> = {
            let mut trust = self.inner.trust.lock().unwrap();
            let ids = trust
                .peers
                .iter()
                .filter(|p| p.name == name_or_id || p.id.to_string() == name_or_id)
                .map(|p| p.id)
                .collect();
            trust.remove(name_or_id);
            ids
        };
        let peers = self.inner.peers.lock().unwrap();
        for id in &ids {
            if let Some(peer) = peers.get(id) {
                peer.close.notify_one();
            }
        }
        !ids.is_empty()
    }

    /// Sends a chat message to every connected peer; returns how many it was sent to.
    pub fn broadcast(&self, text: impl Into<String>) -> usize {
        let msg = Message::Chat {
            from: self.inner.name.clone(),
            text: text.into(),
        };
        self.inner
            .peers
            .lock()
            .unwrap()
            .values()
            .filter(|p| p.tx.send(msg.clone()).is_ok())
            .count()
    }

    /// Sends an alert, a chat message the team must see, to every connected peer and
    /// holds it for the allowlisted peers that aren't connected: each gets it when it
    /// connects (the last [`HELD_ALERTS`], up to [`HOLD_ALERTS_FOR`] old, while this
    /// agent runs). Returns how many peers it was sent to now.
    pub fn alert(&self, text: impl Into<String>) -> usize {
        let text = text.into();
        let allowlisted: Vec<EndpointId> = self
            .inner
            .trust
            .lock()
            .unwrap()
            .peers
            .iter()
            .map(|p| p.id)
            .collect();
        let msg = Message::Chat {
            from: self.inner.name.clone(),
            text: text.clone(),
        };
        // Held under the peers lock: a peer registering now gets it either way.
        let peers = self.inner.peers.lock().unwrap();
        let sent = peers
            .values()
            .filter(|p| p.tx.send(msg.clone()).is_ok())
            .count();
        let now = Instant::now();
        let mut held = self.inner.held.lock().unwrap();
        for id in allowlisted.into_iter().filter(|id| !peers.contains_key(id)) {
            let queue = held.entry(id).or_default();
            queue.retain(|(at, _)| now.duration_since(*at) < HOLD_ALERTS_FOR);
            queue.push_back((now, text.clone()));
            while queue.len() > HELD_ALERTS {
                queue.pop_front();
            }
        }
        sent
    }

    /// Delegates an LLM task to a connected peer and waits for its result.
    /// Fails if the peer doesn't serve the provider, refuses the task, its checks
    /// fail, or it disconnects.
    pub async fn delegate(&self, peer: EndpointId, task: LlmTask) -> Result<String> {
        let delivered = self.delegate_verified(peer, task).await?;
        if delivered.ok {
            Ok(delivered.output)
        } else {
            Err(anyhow!("{}", delivered.output))
        }
    }

    /// Delegates a task and returns everything the peer sent back, including its
    /// signed evidence, whether the task succeeded or not. Errs only if there is no
    /// result (not connected, refused before running, disconnected).
    pub async fn delegate_verified(&self, peer: EndpointId, task: LlmTask) -> Result<Delivered> {
        self.send_task(peer, task, None)?.finish(|_| {}).await
    }

    /// Sends `peer` a task now (so it counts as this agent's work for it at once) and
    /// returns its [`Delegation`], to wait for the result and progress. `origin` says
    /// what it is part of. Fails if not connected, or the peer doesn't serve the
    /// task's provider (a team step needs none).
    pub fn send_task(
        &self,
        peer: EndpointId,
        task: LlmTask,
        origin: Option<Origin>,
    ) -> Result<Delegation> {
        let task_id = self.inner.new_task_id();
        let (reply, rx) = oneshot::channel();
        let (progress_tx, progress) = mpsc::unbounded_channel();
        let peers = self.inner.peers.lock().unwrap();
        let handle = peers
            .get(&peer)
            .ok_or_else(|| anyhow!("not connected to {}", peer.fmt_short()))?;
        if task.runs_model() && !handle.capabilities.contains(&task.provider) {
            bail!("{} does not serve {}", handle.name, task.provider);
        }
        let request = Message::TaskRequest {
            task_id,
            task,
            origin,
        };
        self.inner.pending.lock().unwrap().insert(
            task_id,
            PendingTask {
                peer,
                reply,
                request: Some(request.clone()),
                progress: Some(progress_tx),
            },
        );
        if handle.tx.send(request).is_err() {
            self.inner.pending.lock().unwrap().remove(&task_id);
            bail!("connection to {} closed", handle.name);
        }
        Ok(Delegation {
            guard: CancelOnDrop {
                inner: self.inner.clone(),
                peer,
                task_id,
                armed: true,
            },
            reply: rx,
            progress,
        })
    }

    /// Waits for the reply to `task_id`; dropping this future withdraws the request.
    async fn await_reply(
        &self,
        peer: EndpointId,
        task_id: u64,
        rx: oneshot::Receiver<Result<Delivered, String>>,
    ) -> Result<Delivered> {
        // Dropping this future (e.g. the user cancels) tells the peer to stop the task.
        let mut guard = CancelOnDrop {
            inner: self.inner.clone(),
            peer,
            task_id,
            armed: true,
        };
        let result = rx.await;
        guard.armed = false;
        reply_result(result)
    }

    /// Asks a team member (a connected peer, or with `None` this agent's own user) to
    /// approve something, and waits for the signed answer (`ok` = approved).
    pub async fn request_approval(
        &self,
        peer: Option<EndpointId>,
        title: &str,
        text: &str,
    ) -> Result<Delivered> {
        self.request_approval_with(peer, title, text, BTreeMap::new())
            .await
    }

    /// Like [`Agent::request_approval`], with what the approver can preview before
    /// answering: the UX prototype, review decks and design documents, by path in the
    /// project (see [`crate::preview`]).
    pub async fn request_approval_with(
        &self,
        peer: Option<EndpointId>,
        title: &str,
        text: &str,
        preview: BTreeMap<String, String>,
    ) -> Result<Delivered> {
        let task_id = self.inner.new_task_id();
        let ask = ApprovalAsk {
            title: title.to_owned(),
            text: text.to_owned(),
            preview,
            changes: false,
        };
        self.request_approval_as(peer, task_id, &ask).await
    }

    /// Asks for approval `ask` as request `task_id` (see [`Agent::new_task_id`]): a plan
    /// run keeps it, to wait for the same request again after a restart (the approver
    /// keeps one request per id, so it isn't asked twice).
    pub async fn request_approval_as(
        &self,
        peer: Option<EndpointId>,
        task_id: u64,
        ask: &ApprovalAsk,
    ) -> Result<Delivered> {
        self.ask_approval(peer, task_id, ask, false).await
    }

    /// Waits again for approval request `task_id`, which a plan run of this agent sent
    /// before it restarted (see [`crate::plan::Resume`]): it is asked again, of an
    /// approver not connected yet when it connects; it fails if the approver isn't back
    /// within the task grace.
    pub async fn rejoin_approval(
        &self,
        peer: Option<EndpointId>,
        task_id: u64,
        ask: &ApprovalAsk,
    ) -> Result<Delivered> {
        self.ask_approval(peer, task_id, ask, true).await
    }

    async fn ask_approval(
        &self,
        peer: Option<EndpointId>,
        task_id: u64,
        ask: &ApprovalAsk,
        rejoin: bool,
    ) -> Result<Delivered> {
        self.inner.reserve_task_id(task_id);
        let (reply, rx) = oneshot::channel();
        let me = self.id();
        let target = peer.unwrap_or(me);
        let request = (target != me).then(|| Message::ApprovalRequest {
            task_id,
            title: ask.title.clone(),
            text: ask.text.clone(),
            preview: ask.preview.clone(),
            prototype: BTreeMap::new(),
            changes: ask.changes,
        });
        self.inner.pending.lock().unwrap().insert(
            task_id,
            PendingTask {
                peer: target,
                reply,
                request: request.clone(),
                progress: None,
            },
        );
        if target == me {
            self.inner
                .add_approval(me, self.inner.name.clone(), task_id, ask.clone());
        } else {
            let sent = self
                .inner
                .peers
                .lock()
                .unwrap()
                .get(&target)
                .map(|h| request.clone().is_some_and(|r| h.tx.send(r).is_ok()));
            if rejoin {
                // Not connected yet: it goes out when the approver connects.
                self.inner.give_up_unless_back(target, task_id);
            } else if sent != Some(true) {
                self.inner.pending.lock().unwrap().remove(&task_id);
                bail!("not connected to {}", target.fmt_short());
            }
        }
        self.await_reply(target, task_id, rx).await
    }

    /// Waits again for task `task_id`, which this agent gave `peer` before it restarted
    /// (see [`crate::plan::Resume`]): the same request goes to `peer` again, now or when
    /// it connects, and `peer` answers it as a re-sent request: it goes on with the task
    /// if it is still running it, sends the result it kept if it is done, or runs it if
    /// it has neither. Fails if `peer` isn't back within the task grace.
    pub fn rejoin_task(
        &self,
        peer: EndpointId,
        task_id: u64,
        task: LlmTask,
        origin: Option<Origin>,
    ) -> Delegation {
        self.inner.reserve_task_id(task_id);
        let (reply, rx) = oneshot::channel();
        let (progress_tx, progress) = mpsc::unbounded_channel();
        let request = Message::TaskRequest {
            task_id,
            task,
            origin,
        };
        self.inner.pending.lock().unwrap().insert(
            task_id,
            PendingTask {
                peer,
                reply,
                request: Some(request.clone()),
                progress: Some(progress_tx),
            },
        );
        if let Some(handle) = self.inner.peers.lock().unwrap().get(&peer) {
            let _ = handle.tx.send(request);
        }
        self.inner.give_up_unless_back(peer, task_id);
        Delegation {
            guard: CancelOnDrop {
                inner: self.inner.clone(),
                peer,
                task_id,
                armed: true,
            },
            reply: rx,
            progress,
        }
    }

    /// A new id for a task or approval request (see [`Agent::request_approval_as`]).
    pub fn new_task_id(&self) -> u64 {
        self.inner.new_task_id()
    }

    /// Leaves task (or approval request) `task_id` running when this agent stops,
    /// instead of cancelling it: a plan run waits for it again after a restart (see
    /// [`Agent::rejoin_task`]). Its peer stops it if this agent isn't back within the
    /// task grace.
    pub fn keep_on_exit(&self, task_id: u64) {
        self.inner.kept.lock().unwrap().insert(task_id);
    }

    /// Approval requests waiting for this agent's user.
    pub fn approvals(&self) -> Vec<ApprovalInfo> {
        self.inner
            .approvals
            .lock()
            .unwrap()
            .values()
            .map(|a| a.info.clone())
            .collect()
    }

    /// The preview site of what came with open approval request `id`, if anything.
    pub fn approval_preview(&self, id: u64) -> Option<crate::preview::Files> {
        let sources = self
            .inner
            .approvals
            .lock()
            .unwrap()
            .get(&id)
            .map(|a| a.preview.clone())?;
        crate::preview::label(&sources).map(|_| crate::preview::site(&sources))
    }

    /// Approves or rejects request `id`; the answer is signed with this agent's key.
    pub async fn answer_approval(&self, id: u64, approved: bool, note: &str) -> Result<()> {
        let answer = if approved {
            Answer::Approve
        } else {
            Answer::Reject
        };
        self.answer(id, answer, note).await
    }

    /// Answers approval request `id` (see [`Answer`]); the answer is signed with this
    /// agent's key. Asking for changes needs a note saying what to change, and a request
    /// that offers it.
    pub async fn answer(&self, id: u64, answer: Answer, note: &str) -> Result<()> {
        let note = note.trim();
        if answer == Answer::Changes {
            let offered = self
                .inner
                .approvals
                .lock()
                .unwrap()
                .get(&id)
                .map(|a| a.info.changes);
            match offered {
                None => bail!("there is no open approval request {id}"),
                Some(false) => bail!("request {id} can only be approved or rejected"),
                Some(true) if note.is_empty() => bail!("say what to change"),
                Some(true) => {}
            }
        }
        let request = self
            .inner
            .approvals
            .lock()
            .unwrap()
            .remove(&id)
            .ok_or_else(|| anyhow!("there is no open approval request {id}"))?;
        let _ = self.inner.events.send(Event::ApprovalClosed { id });
        let approved = answer == Answer::Approve;
        let output = format!(
            "{}{}{}",
            match answer {
                Answer::Approve => "Approved by ",
                Answer::Reject => "Rejected by ",
                Answer::Changes => CHANGES_ASKED,
            },
            self.inner.name,
            if note.is_empty() {
                String::new()
            } else {
                format!(": {note}")
            }
        );
        let mut evidence = Evidence::new(
            EvidenceKind::Approval,
            &self.inner.name,
            self.id(),
            request.requester,
            request.task_id,
        );
        evidence.prompt_sha256 = sha256_hex(request.info.text.as_bytes());
        evidence.approved = Some(approved);
        let evidence = evidence.sign(&output, self.inner.endpoint.secret_key());
        if request.requester == self.id() {
            self.inner.complete_pending(
                &request.requester,
                request.task_id,
                Ok(Delivered {
                    ok: approved,
                    output,
                    evidence: Some(evidence),
                    patch: None,
                    verified: true,
                    refused: false,
                    model: None,
                }),
            );
        } else {
            let msg = Message::TaskResult {
                task_id: request.task_id,
                ok: approved,
                output,
                evidence: Some(Box::new(evidence)),
                patch: None,
                refused: false,
                model: None,
            };
            self.inner.deliver(request.requester, msg).await;
        }
        Ok(())
    }

    /// Stops accepting peers, closes every connection and the endpoint. Peers learn
    /// first: the tasks this agent gave them are cancelled, and those it was running
    /// for them stop (reported as not done here, so another worker may take them).
    pub async fn shutdown(&self) {
        if self.inner.say_goodbye() {
            // Let the connections' writers send it before they close.
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        self.inner.shutdown.send_replace(true);
        for peer in self.inner.peers.lock().unwrap().values() {
            peer.close.notify_one();
        }
        self.inner.endpoint.close().await;
    }
}

async fn accept_loop(inner: Arc<Inner>) {
    let mut shutdown = inner.shutdown.subscribe();
    loop {
        let incoming = tokio::select! {
            incoming = inner.endpoint.accept() => incoming,
            _ = shutdown.wait_for(|stopped| *stopped) => return,
        };
        let Some(incoming) = incoming else { return };
        let inner = inner.clone();
        tokio::spawn(async move {
            let Ok(conn) = incoming.await else { return };
            let id = conn.remote_id();
            if !inner.is_trusted(&id) {
                let _ = inner.events.send(Event::PeerRejected { id });
                conn.close(1u32.into(), b"not on allowlist");
                return;
            }
            let Ok((send, recv)) = conn.accept_bi().await else {
                return;
            };
            let _ = run_connection(inner, conn, send, recv, false).await;
        });
    }
}

// Boxed because dial -> run_connection -> (gossip) dial is recursive.
fn dial(inner: Arc<Inner>, addr: EndpointAddr) -> Pin<Box<dyn Future<Output = Result<()>> + Send>> {
    dial_with(inner, addr, false)
}

/// Dials `addr`; with `force`, even if already connected (the simultaneous-dial
/// tie-break then keeps one of the two connections).
fn dial_with(
    inner: Arc<Inner>,
    addr: EndpointAddr,
    force: bool,
) -> Pin<Box<dyn Future<Output = Result<()>> + Send>> {
    Box::pin(async move {
        if *inner.shutdown.borrow() {
            bail!("agent is shut down");
        }
        let id = addr.id;
        if id == inner.endpoint.id() || (!force && inner.peers.lock().unwrap().contains_key(&id)) {
            return Ok(());
        }
        if !inner.is_trusted(&id) {
            bail!("{id} is not on the allowlist; add it with: proofkin peers allow <ticket-or-id>");
        }
        if !inner.dialing.lock().unwrap().insert(id) {
            return Ok(());
        }
        let result = async {
            let conn = inner.endpoint.connect(addr, ALPN).await?;
            let (send, recv) = conn.open_bi().await?;
            anyhow::Ok((conn, send, recv))
        }
        .await;
        inner.dialing.lock().unwrap().remove(&id);
        let (conn, send, recv) =
            result.with_context(|| format!("connecting to {}", id.fmt_short()))?;
        tokio::spawn(async move {
            let _ = run_connection(inner, conn, send, recv, true).await;
        });
        Ok(())
    })
}

enum Registration {
    New,
    Replaced,
    Rejected,
}

impl Inner {
    /// Folders `peer` may run tasks in, by name and access (only if it may delegate
    /// and this agent serves tasks or team plans).
    fn shares_for(&self, peer: &EndpointId) -> Vec<ShareInfo> {
        if self.executor.is_none() && self.team.is_none() {
            return Vec::new();
        }
        let trust = self.trust.lock().unwrap();
        match trust.get(peer) {
            Some(p) if p.can_delegate => self.shares.lock().unwrap().visible(&p.shares, p.access),
            _ => Vec::new(),
        }
    }

    fn capabilities(&self) -> Vec<Provider> {
        self.executor
            .as_ref()
            .map(|e| e.providers())
            .unwrap_or_default()
    }

    /// The models of the providers this agent serves, as its hello tells peers.
    fn models_offered(&self) -> crate::models::Models {
        let serves = self.capabilities();
        self.models
            .lock()
            .unwrap()
            .iter()
            .filter(|(p, _)| serves.contains(p))
            .map(|(p, c)| (*p, c.clone()))
            .collect()
    }

    /// The models its accounts offer that its tasks may ask for, per provider listed
    /// among `serves` (none: every provider listed).
    fn offers_for(&self, serves: Option<&[Provider]>) -> crate::models::Offers {
        let models = self.models.lock().unwrap().clone();
        self.offers
            .lock()
            .unwrap()
            .iter()
            .filter(|(p, _)| serves.is_none_or(|s| s.contains(p)))
            .map(|(p, list)| {
                let choice = crate::models::choice(&models, *p);
                (*p, crate::models::for_peers(list, &choice))
            })
            .filter(|(_, list)| !list.is_empty())
            .collect()
    }

    /// Tells connected peers (if it serves them) and the application that this agent's
    /// models, or the ones its accounts offer, changed.
    fn models_changed(&self) {
        let serves = self.capabilities();
        if !serves.is_empty() {
            let message = Message::Models {
                models: self.models_offered(),
                offers: self.offers_for(Some(&serves)),
            };
            for handle in self.peers.lock().unwrap().values() {
                let _ = handle.tx.send(message.clone());
            }
        }
        let _ = self.events.send(Event::ModelsChanged);
    }

    fn is_trusted(&self, id: &EndpointId) -> bool {
        self.trust.lock().unwrap().get(id).is_some()
    }

    fn known_addrs(&self) -> Vec<EndpointAddr> {
        self.peers
            .lock()
            .unwrap()
            .values()
            .map(|p| p.addr.clone())
            .collect()
    }

    /// Registers a handshaken connection. If both sides dialed each other, both
    /// keep the connection initiated by the lower endpoint id.
    fn register(&self, id: EndpointId, handle: PeerHandle, outbound: bool) -> Registration {
        let me = self.endpoint.id();
        let mut peers = self.peers.lock().unwrap();
        let outcome = match peers.get(&id) {
            None => Registration::New,
            Some(old) => {
                let initiator = if outbound { me } else { id };
                if initiator != me.min(id) {
                    return Registration::Rejected;
                }
                old.close.notify_one();
                Registration::Replaced
            }
        };
        peers.insert(id, handle);
        outcome
    }

    fn unregister(&self, id: EndpointId, conn_id: u64) -> bool {
        let mut peers = self.peers.lock().unwrap();
        if peers.get(&id).is_some_and(|p| p.conn_id == conn_id) {
            peers.remove(&id);
            return true;
        }
        false
    }

    /// Sends `id` the alerts raised while it wasn't connected, saying how long ago.
    fn send_held(&self, id: EndpointId, tx: &mpsc::UnboundedSender<Message>) {
        let Some(queue) = self.held.lock().unwrap().remove(&id) else {
            return;
        };
        let now = Instant::now();
        for (at, text) in queue {
            let ago = now.duration_since(at);
            if ago >= HOLD_ALERTS_FOR {
                continue;
            }
            let ago = match ago.as_secs() {
                s if s < 60 => "less than a minute".to_owned(),
                s if s < 3600 => format!("{} min", s / 60),
                s => format!("{} h", s / 3600),
            };
            let _ = tx.send(Message::Chat {
                from: self.name.clone(),
                text: format!("{text} (raised {ago} ago, while you weren't connected)"),
            });
        }
    }

    /// Fails tasks delegated to `peer` once it is really gone.
    fn fail_pending(&self, peer: &EndpointId) {
        let mut pending = self.pending.lock().unwrap();
        let ids: Vec<u64> = pending
            .iter()
            .filter(|(_, p)| &p.peer == peer)
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            if let Some(task) = pending.remove(&id) {
                let _ = task
                    .reply
                    .send(Err("peer disconnected before returning a result".into()));
            }
        }
    }

    /// A new id for a task (or approval request) this agent gives a peer; ids go on
    /// after a restart where they are kept (see [`AgentConfig::task_ids`]).
    fn new_task_id(&self) -> u64 {
        let id = self.next_task_id.fetch_add(1, Ordering::Relaxed);
        self.keep_task_ids();
        id
    }

    /// Makes sure no new id is `id` or below it (a task from before a restart has it).
    fn reserve_task_id(&self, id: u64) {
        self.next_task_id
            .fetch_max(id.saturating_add(1), Ordering::Relaxed);
        self.keep_task_ids();
    }

    /// Fails `task_id`, which `peer` was given before this agent restarted, unless
    /// `peer` is connected once the task grace has passed: it would never answer.
    fn give_up_unless_back(self: &Arc<Self>, peer: EndpointId, task_id: u64) {
        let inner = Arc::downgrade(self);
        let grace = self.task_grace;
        tokio::spawn(async move {
            tokio::time::sleep(grace).await;
            let Some(inner) = inner.upgrade() else {
                return;
            };
            if inner.peers.lock().unwrap().contains_key(&peer) {
                return;
            }
            inner.complete_pending(
                &peer,
                task_id,
                Err("it didn't come back after this agent restarted".into()),
            );
        });
    }

    fn keep_task_ids(&self) {
        if let Some(path) = &self.task_ids {
            // The latest value, whichever write lands last.
            let _write = self.task_ids_write.lock().unwrap();
            let next = self.next_task_id.load(Ordering::Relaxed);
            if let Err(e) = crate::store::write_atomic(path, next.to_string().as_bytes()) {
                eprintln!(
                    "  could not keep the next task id in {}: {e:#}",
                    path.display()
                );
            }
        }
    }

    fn complete_pending(&self, peer: &EndpointId, task_id: u64, result: Result<Delivered, String>) {
        let mut pending = self.pending.lock().unwrap();
        // Only the peer the task was sent to may answer it.
        if pending.get(&task_id).is_some_and(|p| &p.peer == peer)
            && let Some(task) = pending.remove(&task_id)
        {
            self.kept.lock().unwrap().remove(&task_id);
            let _ = task.reply.send(result);
        }
    }

    /// Stops the tasks `peer` asked this agent to run (it left for good).
    fn cancel_served(&self, peer: &EndpointId) {
        self.serving.lock().unwrap().retain(|(p, _), _| p != peer);
    }

    /// Tells peers this agent is stopping: the tasks it gave them are cancelled (but
    /// those a plan run waits for again after a restart, see [`Agent::keep_on_exit`]),
    /// and those it runs for them end as not done (so another worker may take them).
    /// Whether there was anything to say.
    fn say_goodbye(&self) -> bool {
        let peers = self.peers.lock().unwrap();
        let mut said = false;
        let kept = self.kept.lock().unwrap().clone();
        let pending: Vec<(EndpointId, u64)> = self
            .pending
            .lock()
            .unwrap()
            .iter()
            .filter(|(id, _)| !kept.contains(id))
            .map(|(id, p)| (p.peer, *id))
            .collect();
        for (peer, task_id) in pending {
            if let Some(handle) = peers.get(&peer) {
                said |= handle.tx.send(Message::TaskCancel { task_id }).is_ok();
            }
        }
        let served: Vec<(EndpointId, u64)> = self.serving.lock().unwrap().keys().copied().collect();
        for (peer, task_id) in served {
            if let Some(handle) = peers.get(&peer) {
                said |= handle
                    .tx
                    .send(Message::TaskResult {
                        task_id,
                        ok: false,
                        output: format!("{} stopped before it finished the task", self.name),
                        evidence: None,
                        patch: None,
                        refused: true,
                        model: None,
                    })
                    .is_ok();
            }
        }
        said
    }

    /// Reports `progress` of served task `task_id` to `peer`, which asked for it (and
    /// keeps it, to report again after a reconnect).
    fn send_progress(&self, peer: &EndpointId, task_id: u64, progress: Progress) {
        self.last_progress
            .lock()
            .unwrap()
            .insert((*peer, task_id), progress.clone());
        if let Some(handle) = self.peers.lock().unwrap().get(peer) {
            let _ = handle.tx.send(Message::TaskProgress { task_id, progress });
        }
    }

    /// How busy this agent is with peers' tasks, as `peer` is told.
    fn load_for(&self, peer: &EndpointId) -> Message {
        let slots = self.slots.lock().unwrap();
        let yours = slots
            .queue
            .iter()
            .chain(slots.holding.iter())
            .filter(|(p, _)| p == peer)
            .count();
        Message::Load {
            running: slots.holding.len() as u32,
            queued: slots.queue.len() as u32,
            slots: MAX_CONCURRENT_TASKS as u32,
            yours: yours as u32,
        }
    }

    /// Tells every connected peer how busy this agent is now (it serves tasks).
    fn broadcast_load(&self) {
        if self.executor.is_none() {
            return;
        }
        let peers: Vec<(EndpointId, mpsc::UnboundedSender<Message>)> = self
            .peers
            .lock()
            .unwrap()
            .iter()
            .map(|(id, h)| (*id, h.tx.clone()))
            .collect();
        for (id, tx) in peers {
            let _ = tx.send(self.load_for(&id));
        }
    }

    /// Tells the tasks waiting for `peer` that it dropped and they wait for it.
    fn report_reconnecting(&self, peer: &EndpointId, name: &str) {
        let text = format!(
            "lost the connection to {name}; waiting up to {} s for it to come back",
            self.task_grace.as_secs()
        );
        for task in self.pending.lock().unwrap().values() {
            if &task.peer == peer
                && let Some(progress) = &task.progress
            {
                let _ = progress.send(Progress::new(Phase::Reconnecting, text.clone()));
            }
        }
    }

    /// Queues approval request `ask` for this agent's user; false if `requester` has too
    /// many open requests. A preview over the limits, or of files that aren't
    /// previewable, is left out.
    fn add_approval(
        &self,
        requester: EndpointId,
        from: String,
        task_id: u64,
        ask: ApprovalAsk,
    ) -> bool {
        let ApprovalAsk {
            title,
            text,
            preview,
            changes,
        } = ask;
        let id = self.next_task_id.fetch_add(1, Ordering::Relaxed);
        let size: usize = preview.iter().map(|(k, v)| k.len() + v.len()).sum();
        let preview: crate::preview::Sources =
            if size <= crate::preview::MAX_BYTES && preview.len() <= crate::preview::MAX_FILES {
                preview
                    .into_iter()
                    .filter(|(path, _)| crate::preview::previewable(path))
                    .map(|(path, text)| (path, text.into_bytes()))
                    .collect()
            } else {
                crate::preview::Sources::new()
            };
        {
            let mut approvals = self.approvals.lock().unwrap();
            // A re-sent request (after a connection was replaced) is already queued.
            if approvals
                .values()
                .any(|a| a.requester == requester && a.task_id == task_id)
            {
                return true;
            }
            if approvals
                .values()
                .filter(|a| a.requester == requester)
                .count()
                >= MAX_OPEN_APPROVALS
            {
                return false;
            }
            approvals.insert(
                id,
                IncomingApproval {
                    info: ApprovalInfo {
                        id,
                        from: from.clone(),
                        title: title.chars().take(200).collect(),
                        text: text.chars().take(20_000).collect(),
                        preview: crate::preview::label(&preview),
                        changes,
                    },
                    requester,
                    task_id,
                    preview,
                },
            );
        }
        let _ = self.events.send(Event::ApprovalRequested {
            id,
            from,
            title: title.chars().take(200).collect(),
        });
        true
    }

    /// Drops `requester`'s approval request for its `task_id` (withdrawn).
    fn withdraw_approval(&self, requester: &EndpointId, task_id: u64) {
        let ids: Vec<u64> = {
            let mut approvals = self.approvals.lock().unwrap();
            let ids: Vec<u64> = approvals
                .iter()
                .filter(|(_, a)| &a.requester == requester && a.task_id == task_id)
                .map(|(id, _)| *id)
                .collect();
            for id in &ids {
                approvals.remove(id);
            }
            ids
        };
        for id in ids {
            let _ = self.events.send(Event::ApprovalClosed { id });
        }
    }

    /// Sends `msg` to `peer` over its current connection, waiting out a brief
    /// reconnect (e.g. a duplicate connection being replaced).
    async fn deliver(&self, peer: EndpointId, msg: Message) {
        let deadline = Instant::now() + DISCONNECT_GRACE * 2;
        loop {
            let tx = self.peers.lock().unwrap().get(&peer).map(|h| h.tx.clone());
            if let Some(tx) = tx
                && tx.send(msg.clone()).is_ok()
            {
                return;
            }
            if Instant::now() >= deadline {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Checks the allowlist permission, the shared folder and the hourly quota for
    /// a task from `id`; returns where and with what access it may run.
    fn admit_task(&self, id: &EndpointId, task: &LlmTask) -> Result<(TaskContext, Share)> {
        let (name, can_delegate, per_hour, grant, granted_shares) = {
            let trust = self.trust.lock().unwrap();
            let peer = trust
                .get(id)
                .ok_or_else(|| anyhow!("not on the allowlist"))?;
            (
                peer.name.clone(),
                peer.can_delegate,
                peer.tasks_per_hour,
                peer.access,
                peer.shares.clone(),
            )
        };
        if !can_delegate {
            bail!(
                "{} does not accept tasks from {name} (not allowed to delegate)",
                self.name
            );
        }
        let share_name = task.share.as_deref().unwrap_or(DATA_SHARE);
        let share = self
            .shares
            .lock()
            .unwrap()
            .get(share_name)
            .filter(|_| granted_shares.iter().any(|s| s == share_name))
            .cloned()
            .ok_or_else(|| {
                anyhow!(
                    "{} has not shared a folder named '{share_name}' with {name}",
                    self.name
                )
            })?;
        let access = effective_access(grant, share.max_access);
        // Built-in spec checks only read files, so any access may ask for them.
        let owner_checks: Vec<&String> = task
            .checks
            .iter()
            .filter(|c| !crate::spec::is_built_in(c))
            .collect();
        for check in &owner_checks {
            if !share.checks.contains_key(*check) {
                bail!(
                    "folder '{}' on {} has no check named '{check}' (it has: {}; built in: {}){}",
                    share.name,
                    self.name,
                    if share.checks.is_empty() {
                        "none".to_owned()
                    } else {
                        share.checks.keys().cloned().collect::<Vec<_>>().join(", ")
                    },
                    crate::spec::BUILT_IN_CHECKS.join(", "),
                    crate::spec::group_hint(&[check.as_str()])
                );
            }
        }
        if !owner_checks.is_empty() && access < Access::Full {
            bail!(
                "checks run the folder's code with {}'s rights, so they need full access; {name} has {access} in '{}'",
                self.name,
                share.name
            );
        }
        if task.patch.is_some() && !(share.worktrees && access >= Access::WorkspaceWrite) {
            bail!(
                "applying a patch needs edit access in a folder shared with --worktrees; '{}' on {} isn't",
                share.name,
                self.name
            );
        }
        // The work context its signed evidence will record must be in its prompt.
        if let Err(e) = crate::evidence::context_uses(&task.prompt, &task.context) {
            bail!("{} refused the task: {e:#}", self.name);
        }
        let mut mcp = BTreeMap::new();
        for server in &task.mcp {
            let Some(def) = share.mcp.get(server) else {
                bail!(
                    "folder '{}' on {} offers no MCP server named '{server}'",
                    share.name,
                    self.name
                );
            };
            if !def.allows(access) {
                bail!(
                    "MCP server '{server}' in '{}' on {} needs {} access; {name} has {access}",
                    share.name,
                    self.name,
                    def.access
                );
            }
            if def.paused || def.plugin_off {
                bail!(
                    "MCP server '{server}' in '{}' on {} is paused by its owner",
                    share.name,
                    self.name
                );
            }
            mcp.insert(server.clone(), def.clone());
        }
        if !mcp.is_empty() && task.provider == Provider::Codex {
            bail!("Codex tasks can't use MCP servers; ask for copilot or claude");
        }
        // The model it asks for must be one this agent's owner allows, and its effort a
        // reasoning effort it allows (team and pull request steps run no model).
        if task.runs_model() {
            let choice = crate::models::choice(&self.models.lock().unwrap(), task.provider);
            if let Err(e) = choice
                .pick(task.model.as_deref())
                .and_then(|_| choice.pick_effort(task.effort.as_deref()))
            {
                bail!("{} refused the task: {e:#}", self.name);
            }
        }
        let now = Instant::now();
        let mut log = self.task_log.lock().unwrap();
        let recent = log.entry(*id).or_default();
        while recent
            .front()
            .is_some_and(|t| now.duration_since(*t) > HOUR)
        {
            recent.pop_front();
        }
        if recent.len() >= per_hour as usize {
            bail!(
                "{} refused the task: {name} reached its limit of {per_hour} tasks per hour",
                self.name
            );
        }
        recent.push_back(now);
        let mut ctx = TaskContext::new(access, share.path.clone());
        ctx.mcp = mcp;
        Ok((ctx, share))
    }

    /// Runs a peer's task until it finishes or `cancel` fires (the peer sent
    /// `TaskCancel` or disconnected), then audits it and reports the outcome. Also
    /// says whether the task never ran (refused, so another worker may take it).
    async fn serve_task(
        self: &Arc<Self>,
        id: &EndpointId,
        from: &str,
        task_id: u64,
        task: LlmTask,
        origin: Option<Origin>,
        cancel: oneshot::Receiver<()>,
    ) -> (Result<Served>, bool) {
        let clock = std::time::SystemTime::now();
        let mut granted = None;
        let mut cancelled = false;
        let result = self
            .run_peer_task(
                id,
                from,
                task_id,
                &task,
                origin.as_ref(),
                cancel,
                &mut granted,
                &mut cancelled,
            )
            .await;
        self.last_progress.lock().unwrap().remove(&(*id, task_id));
        let refused = granted.is_none();
        let audited = match &result {
            Ok(served) if served.ok => Ok(String::new()),
            Ok(served) if served.evidence.team.is_some() => {
                Err(anyhow!("the team run did not pass"))
            }
            Ok(_) if task.pull_request.is_some() => {
                Err(anyhow!("its pull request was closed without being merged"))
            }
            Ok(served) => Err(anyhow!(
                "checks failed: {}",
                served
                    .evidence
                    .checks
                    .iter()
                    .filter(|c| !c.passed)
                    .map(|c| c.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
            Err(e) => Err(anyhow!("{e:#}")),
        };
        let mut entry = AuditEntry::new(from, id, task_id, &task, granted, &audited, clock);
        if cancelled {
            entry.outcome = Outcome::Cancelled;
        }
        entry.origin = origin.as_ref().and_then(Origin::describe);
        entry.model = result.as_ref().ok().and_then(|s| s.model.clone());
        if let Some(audit) = &self.audit {
            audit.record(&entry);
        }
        let event = match (&audited, entry.outcome) {
            (Err(e), Outcome::Refused) => Event::TaskRefused {
                from: from.to_owned(),
                task_id,
                reason: format!("{e:#}"),
            },
            (_, outcome) => Event::TaskCompleted {
                from: from.to_owned(),
                task_id,
                outcome,
            },
        };
        let _ = self.events.send(event);
        (result, refused)
    }

    /// Checks and runs a peer's task; sets `granted` once it is admitted.
    #[allow(clippy::too_many_arguments)]
    async fn run_peer_task(
        self: &Arc<Self>,
        id: &EndpointId,
        from: &str,
        task_id: u64,
        task: &LlmTask,
        origin: Option<&Origin>,
        cancel: oneshot::Receiver<()>,
        granted: &mut Option<Access>,
        cancelled: &mut bool,
    ) -> Result<Served> {
        if let Some(request) = &task.team {
            return self
                .lead_team(
                    id, from, task_id, task, request, origin, cancel, granted, cancelled,
                )
                .await;
        }
        if let Some(ask) = &task.pull_request {
            return self
                .open_pull_request(
                    id, from, task_id, task, ask, origin, cancel, granted, cancelled,
                )
                .await;
        }
        let executor = self
            .executor
            .as_ref()
            .ok_or_else(|| anyhow!("{} does not serve tasks", self.name))?;
        if !executor.providers().contains(&task.provider) {
            bail!("{} does not serve {}", self.name, task.provider);
        }
        let (ctx, share) = self.admit_task(id, task)?;
        *granted = Some(ctx.access);
        let _ = self.events.send(Event::TaskReceived {
            from: from.to_owned(),
            task_id,
            provider: task.provider,
            origin: origin.cloned(),
            team: false,
        });
        let work = async {
            let _slot = self.take_slot(id, task_id).await?;
            self.attempt_task(executor.as_ref(), id, from, task_id, task, ctx, &share)
                .await
        };
        // Dropping `work` stops the provider run (each backend aborts on drop) and
        // any check, and removes the task's worktree.
        tokio::select! {
            result = work => result,
            _ = cancel => {
                *cancelled = true;
                Err(anyhow!("task cancelled by {from}"))
            }
        }
    }

    /// Asks this agent's own user to approve `ask` and waits for the signed answer (`ok` =
    /// approved). Dropping the wait withdraws the request.
    async fn ask_own_user(self: &Arc<Self>, ask: ApprovalAsk) -> Result<Delivered> {
        let task_id = self.new_task_id();
        let me = self.endpoint.id();
        let (reply, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(
            task_id,
            PendingTask {
                peer: me,
                reply,
                request: None,
                progress: None,
            },
        );
        self.add_approval(me, self.name.clone(), task_id, ask);
        let mut guard = CancelOnDrop {
            inner: self.clone(),
            peer: me,
            task_id,
            armed: true,
        };
        let result = rx.await;
        guard.armed = false;
        reply_result(result)
    }

    /// Runs pull request step `task` from `from` (see [`crate::pulls`]): opens the pull
    /// request from the branch an earlier task of `from`'s committed to in this folder (or
    /// finds the one opened for it before), asks the step's reviewers to review one it
    /// opened, and follows it until it is merged (done) or closed (failed), saying where it
    /// is. It runs no model and takes no task slot.
    #[allow(clippy::too_many_arguments)]
    async fn open_pull_request(
        self: &Arc<Self>,
        id: &EndpointId,
        from: &str,
        task_id: u64,
        task: &LlmTask,
        ask: &crate::pulls::PullRequestAsk,
        origin: Option<&Origin>,
        cancel: oneshot::Receiver<()>,
        granted: &mut Option<Access>,
        cancelled: &mut bool,
    ) -> Result<Served> {
        let pulls = self
            .pulls
            .clone()
            .ok_or_else(|| anyhow!("{} doesn't open pull requests", self.name))?;
        let (ctx, share) = self.admit_task(id, task)?;
        if !share.pull_requests {
            bail!(
                "folder '{}' on {} doesn't take pull request steps; its owner allows them with: proofkin share pull-requests {}",
                share.name,
                self.name,
                share.name
            );
        }
        if !share.worktrees || ctx.access < Access::WorkspaceWrite {
            bail!(
                "a pull request step needs edit access in a folder shared with --worktrees; {from} has {} in '{}' on {}",
                ctx.access,
                share.name,
                self.name
            );
        }
        crate::pulls::check_own_work(
            &share.path,
            ask,
            &id.to_string(),
            &self.endpoint.id().to_string(),
        )
        .await?;
        *granted = Some(ctx.access);
        let _ = self.events.send(Event::TaskReceived {
            from: from.to_owned(),
            task_id,
            provider: task.provider,
            origin: origin.cloned(),
            team: false,
        });
        let work = async {
            // Only the sign-offs that check out go on the pull request.
            let mut checked = ask.clone();
            checked.signoffs.retain(|s| s.checks_out(&id.to_string()));
            let left_out = ask.signoffs.len() - checked.signoffs.len();
            let mut asked = crate::pulls::Asked {
                by: from.to_owned(),
                run: origin.and_then(|o| o.run.clone()),
                step: origin.and_then(|o| o.step.clone()),
                approval: None,
            };
            // Its owner approves each one first, unless an earlier try opened it already.
            if share.pull_request_approval && pulls.find(&share.name, &checked).await?.is_none() {
                self.send_progress(
                    id,
                    task_id,
                    Progress::new(
                        Phase::PullRequest,
                        format!(
                            "waiting for {} to approve opening the pull request from {}",
                            self.name, ask.branch
                        ),
                    ),
                );
                let mut text = format!(
                    "{from} asks you to open a pull request in '{}' with your GitHub account, with work this agent did for them:\n\n- Title: {}\n- Branch: `{}` (commit `{}`): the GitHub card's task branches can show its changes\n",
                    share.name,
                    checked.title,
                    checked.branch,
                    checked.commit.chars().take(12).collect::<String>()
                );
                if !checked.signoffs.is_empty() {
                    text += &format!(
                        "- Sign-offs it lists: {}\n",
                        checked
                            .signoffs
                            .iter()
                            .map(|s| format!("{} ({})", s.evidence.signer, s.step))
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
                if !checked.reviewers.is_empty() {
                    text += &format!("- Asked to review: {}\n", checked.reviewers.join(", "));
                }
                if !task.issues.is_empty() {
                    text += &format!("- It closes: {}\n", task.issues.join(", "));
                }
                text += "\nApproving pushes the branch and opens the pull request, which says you approved it.";
                let answer = self
                    .ask_own_user(ApprovalAsk {
                        title: format!("Open a pull request in {} for {from}?", share.name),
                        text,
                        ..ApprovalAsk::default()
                    })
                    .await?;
                let note = answer
                    .output
                    .split_once(": ")
                    .map(|(_, note)| note.trim().to_owned())
                    .unwrap_or_default();
                if !answer.ok {
                    bail!(
                        "{} didn't approve opening the pull request{}",
                        self.name,
                        if note.is_empty() {
                            String::new()
                        } else {
                            format!(": {note}")
                        }
                    );
                }
                asked.approval = Some(crate::pulls::OwnerApproval {
                    by: self.name.clone(),
                    note,
                });
            }
            self.send_progress(
                id,
                task_id,
                Progress::new(
                    Phase::PullRequest,
                    format!("opening the pull request from {}", ask.branch),
                ),
            );
            let opened = pulls
                .open(&share.name, &checked, &task.issues, &asked)
                .await?;
            // Reviewers are asked once, when it is opened: asking again would ask those who
            // reviewed it already to review it again.
            let mut reviews = Vec::new();
            if !opened.existed {
                let (logins, not) = crate::pulls::reviewers_to_ask(&checked);
                if !logins.is_empty() {
                    self.send_progress(
                        id,
                        task_id,
                        Progress::new(
                            Phase::PullRequest,
                            format!(
                                "asking {} to review pull request #{}",
                                logins.join(", "),
                                opened.number
                            ),
                        ),
                    );
                    reviews = pulls
                        .request_reviews(&share.name, opened.number, &logins)
                        .await;
                }
                reviews.extend(not);
            }
            let reviewing: Vec<&str> = reviews
                .iter()
                .filter(|r| r.not_asked.is_none())
                .map(|r| r.login.as_str())
                .collect();
            let mut state = opened.state.clone();
            let mut said = String::new();
            while state != "MERGED" && state != "CLOSED" {
                if said != state {
                    self.send_progress(
                        id,
                        task_id,
                        Progress::new(
                            Phase::PullRequest,
                            format!(
                                "pull request #{} is open{}: waiting for it to be merged ({})",
                                opened.number,
                                if reviewing.is_empty() {
                                    String::new()
                                } else {
                                    format!(", with a review asked of {}", reviewing.join(", "))
                                },
                                opened.url
                            ),
                        ),
                    );
                    said = state.clone();
                }
                tokio::time::sleep(pulls.poll()).await;
                // GitHub not answering isn't an answer: it is asked again next time.
                if let Ok(now) = pulls.state(&share.name, opened.number).await {
                    state = now;
                }
            }
            let merged = state == "MERGED";
            let plural =
                |n: usize, what: &str| format!("{n} {what}{}", if n == 1 { "" } else { "s" });
            let mut with = Vec::new();
            if opened.proof {
                with.push("its signed proof".to_owned());
            }
            if !opened.existed && !checked.signoffs.is_empty() {
                with.push(plural(checked.signoffs.len(), "sign-off"));
            }
            let mut notes = Vec::new();
            if left_out > 0 {
                notes.push(format!(
                    "{} left out: {}",
                    plural(left_out, "sign-off"),
                    if left_out == 1 {
                        "its record doesn't check out"
                    } else {
                        "their records don't check out"
                    }
                ));
            }
            if opened.existed {
                notes.push(
                    if asked.step.is_some() {
                        "an earlier try of this step opened it"
                    } else {
                        "it was opened before, from the same work"
                    }
                    .to_owned(),
                );
            }
            if let Some(approval) = &asked.approval {
                notes.push(format!("{} approved opening it", approval.by));
            }
            let output = format!(
                "Pull request #{} {}: {}\n\nIt has the work of branch `{}` (commit `{}`){}{}.{}",
                opened.number,
                if merged {
                    "was merged"
                } else {
                    "was closed without being merged"
                },
                opened.url,
                ask.branch,
                ask.commit.chars().take(12).collect::<String>(),
                if with.is_empty() {
                    String::new()
                } else {
                    format!(", with {}", with.join(" and "))
                },
                notes.iter().map(|n| format!("; {n}")).collect::<String>(),
                crate::pulls::reviews_text(&reviews)
            );
            let mut evidence = Evidence::new(
                EvidenceKind::Task,
                &self.name,
                self.endpoint.id(),
                *id,
                task_id,
            );
            evidence.share = Some(share.name.clone());
            evidence.prompt_sha256 = sha256_hex(task.prompt.as_bytes());
            let evidence = evidence.sign(&output, self.endpoint.secret_key());
            Ok(Served {
                ok: merged,
                output,
                evidence,
                patch: None,
                model: None,
            })
        };
        tokio::select! {
            result = work => result,
            _ = cancel => {
                *cancelled = true;
                Err(anyhow!("task cancelled by {from}"))
            }
        }
    }

    /// Runs team step `task` from `from`: its folder's team plan, or one this agent's
    /// model writes for it (see [`Inner::write_team_plan`]), on this agent's own peers
    /// (never one the work came down from), reporting each of its steps upward.
    /// The signed evidence holds the team run with every step's own signed evidence,
    /// so the requester (and anyone later) can check the whole tree.
    #[allow(clippy::too_many_arguments)]
    async fn lead_team(
        self: &Arc<Self>,
        id: &EndpointId,
        from: &str,
        task_id: u64,
        task: &LlmTask,
        request: &crate::team::TeamRequest,
        origin: Option<&Origin>,
        cancel: oneshot::Receiver<()>,
        granted: &mut Option<Access>,
        cancelled: &mut bool,
    ) -> Result<Served> {
        let runner = self
            .team
            .clone()
            .ok_or_else(|| anyhow!("{} doesn't run team plans", self.name))?;
        let (ctx, share) = self.admit_task(id, task)?;
        // The folder's own team plan, else one its model writes, with this agent's first
        // provider.
        let writer = if share.team.is_some() {
            None
        } else if share.team_model {
            let provider = self
                .executor
                .as_ref()
                .and_then(|e| e.providers().first().copied());
            let writer = self.executor.clone().zip(provider).ok_or_else(|| {
                anyhow!(
                    "{} runs no models, so it can't write a team plan for '{}'",
                    self.name,
                    share.name
                )
            })?;
            Some(writer)
        } else {
            bail!(
                "folder '{}' on {} has no team plan for team steps",
                share.name,
                self.name
            );
        };
        if ctx.access < Access::WorkspaceWrite {
            bail!(
                "a team step needs edit access in '{}' on {}; {from} has {}",
                share.name,
                self.name,
                ctx.access
            );
        }
        if request
            .chain
            .iter()
            .any(|c| *c == self.endpoint.id().to_string())
        {
            bail!(
                "this work came down through {} already; taking it again would loop",
                self.name
            );
        }
        *granted = Some(ctx.access);
        let _ = self.events.send(Event::TaskReceived {
            from: from.to_owned(),
            task_id,
            provider: task.provider,
            origin: origin.cloned(),
            team: true,
        });
        let agent = Agent {
            inner: self.clone(),
        };
        let observe = |update: crate::team::TeamUpdate<'_>| {
            let event = match update.step {
                None => Event::TeamRun {
                    from: from.to_owned(),
                    task_id,
                    run: update.run.to_owned(),
                    steps: update.plan.steps.clone(),
                },
                Some((step, state)) => Event::TeamStep {
                    from: from.to_owned(),
                    task_id,
                    step: step.to_owned(),
                    state: Box::new(state.clone()),
                },
            };
            let _ = self.events.send(event);
            self.send_progress(id, task_id, crate::team::progress(&update));
        };
        let work = async {
            let (template, notes) = match &writer {
                Some((executor, provider)) => {
                    self.write_team_plan(
                        executor.as_ref(),
                        *provider,
                        &runner,
                        id,
                        from,
                        task_id,
                        task,
                        request,
                        &share,
                    )
                    .await?
                }
                None => (share.team.clone().unwrap_or_default(), Vec::new()),
            };
            let mut outcome = runner
                .run(&agent, &template, task, request, *id, observe)
                .await?;
            for note in notes {
                outcome.output = format!("{}\n\n{note}", outcome.output.trim_end());
            }
            anyhow::Ok(outcome)
        };
        let outcome = tokio::select! {
            outcome = work => outcome?,
            _ = cancel => {
                *cancelled = true;
                bail!("task cancelled by {from}");
            }
        };
        let output = crate::redact::redact_with_note(&outcome.output, &self.name);
        let mut evidence = Evidence::new(
            EvidenceKind::Task,
            &self.name,
            self.endpoint.id(),
            *id,
            task_id,
        );
        evidence.share = Some(share.name.clone());
        evidence.prompt_sha256 = sha256_hex(task.prompt.as_bytes());
        // Checked when the team step was admitted.
        evidence.context =
            crate::evidence::context_uses(&task.prompt, &task.context).unwrap_or_default();
        evidence.git = outcome.git;
        evidence.team = Some(Box::new(outcome.record));
        Ok(Served {
            evidence: evidence.sign(&output, self.endpoint.secret_key()),
            output,
            patch: outcome.patch,
            ok: outcome.ok,
            model: None,
        })
    }

    /// Has `executor`'s model write the team plan for team step `task` from `from` in
    /// folder `share`, whose owner leaves team plans to it: with `provider` and the
    /// owner's default model and reasoning effort for it, read-only in the folder, in one
    /// of this agent's task slots while it writes. A plan that doesn't check out as
    /// [`crate::team::TeamRunner::run`] checks one goes back to it with what is wrong, up
    /// to [`crate::team::PLAN_TRIES`] tries in all; then this agent's own user approves
    /// it first if the owner asked for that, and may ask for changes instead, up to
    /// [`crate::team::PLAN_CHANGE_ROUNDS`] times: the model writes it again with their
    /// note (with as many tries), and they are asked again. Returns the plan to run,
    /// which says first (in a comment) who wrote it, and what the lead's answer adds: who
    /// wrote it, and a note if the folder couldn't be brought up to date first.
    #[allow(clippy::too_many_arguments)]
    async fn write_team_plan(
        self: &Arc<Self>,
        executor: &dyn Executor,
        provider: Provider,
        runner: &crate::team::TeamRunner,
        id: &EndpointId,
        from: &str,
        task_id: u64,
        task: &LlmTask,
        request: &crate::team::TeamRequest,
        share: &Share,
    ) -> Result<(String, Vec<String>)> {
        let agent = Agent {
            inner: self.clone(),
        };
        // A linked repository's clone first gets the repository's latest commit.
        let kept = self.update_folder(id, task_id, share).await;
        let prompt = runner.prompt(&agent, task, request, *id);
        let mut ask = LlmTask::new(provider, prompt.clone());
        let choice = crate::models::choice(&self.models.lock().unwrap(), provider);
        ask.model = choice.pick(None)?;
        ask.effort = choice.pick_effort(None)?;
        let ctx = TaskContext::new(Access::ReadOnly, &share.path);
        // What the model is asked: the prompt, with the changes its owner last asked for
        // (`asked`, in round `rounds` of changes).
        let (mut base, mut asked, mut rounds) = (prompt.clone(), None::<String>, 0);
        let (plan, model, approved) = loop {
            ask.prompt = base.clone();
            let mut tries = 0;
            let (plan, model) = loop {
                tries += 1;
                let text = match (tries, &asked) {
                    (1, None) => format!("{}'s model is writing the team plan", self.name),
                    (1, Some(_)) => format!(
                        "{0}'s model is writing the team plan again, with the changes {0} \
                         asked for",
                        self.name
                    ),
                    _ => format!(
                        "{}'s model is writing the team plan again, told what was wrong with it",
                        self.name
                    ),
                };
                self.send_progress(id, task_id, Progress::new(Phase::Running, text));
                let ran = {
                    let _slot = self.take_slot(id, task_id).await?;
                    executor.execute(&ask, &ctx).await.with_context(|| {
                        format!("{}'s model couldn't write the team plan", self.name)
                    })?
                };
                let plan = crate::team::written_plan(&ran.output);
                match runner.check(&agent, &plan, task, request) {
                    Ok(_) => break (plan, ran.model.or_else(|| ask.model.clone())),
                    Err(e) if tries < crate::team::PLAN_TRIES => {
                        let problems = format!("{e:#}");
                        ask.prompt = crate::team::plan_again_prompt(&base, &plan, &problems);
                    }
                    Err(e) => bail!(
                        "{}'s model couldn't write a team plan that checks out: {e:#}",
                        self.name
                    ),
                }
            };
            if !share.team_approval {
                break (plan, model, None);
            }
            self.send_progress(
                id,
                task_id,
                Progress::new(
                    Phase::Team,
                    format!("waiting for {} to approve the team plan", self.name),
                ),
            );
            let changes = rounds < crate::team::PLAN_CHANGE_ROUNDS;
            let text = crate::team::approval_text(
                from,
                &share.name,
                request,
                &plan,
                asked.as_deref().map(|note| (rounds, note)),
            );
            let answer = self
                .ask_own_user(ApprovalAsk {
                    title: format!("Run this team plan for {from}?"),
                    text,
                    changes,
                    ..ApprovalAsk::default()
                })
                .await?;
            if answer.ok {
                break (plan, model, Some(rounds));
            }
            let note = answer
                .output
                .split_once(": ")
                .map(|(_, note)| note.trim().to_owned())
                .unwrap_or_default();
            if changes && answer.output.starts_with(CHANGES_ASKED) {
                rounds += 1;
                base = crate::team::plan_changes_prompt(&prompt, &plan, &self.name, &note);
                asked = Some(note);
                continue;
            }
            bail!(
                "{} didn't approve the team plan{}",
                self.name,
                if note.is_empty() {
                    String::new()
                } else {
                    format!(": {note}")
                }
            );
        };
        let by = crate::team::written_by(&self.name, provider, model.as_deref(), from, approved);
        let notes = std::iter::once(format!("{by}.")).chain(kept).collect();
        Ok((format!("// {by}\n{plan}"), notes))
    }

    /// Waits for a task slot for `peer`'s task `task_id`, telling it its place in the
    /// queue meanwhile. The slot is free again when the returned pair is dropped.
    async fn take_slot(
        &self,
        peer: &EndpointId,
        task_id: u64,
    ) -> Result<(tokio::sync::SemaphorePermit<'_>, Place<'_>)> {
        let key = (*peer, task_id);
        if let Ok(permit) = self.task_slots.try_acquire() {
            self.slots.lock().unwrap().holding.insert(key);
            self.broadcast_load();
            return Ok((
                permit,
                Place {
                    inner: self,
                    key,
                    holding: true,
                },
            ));
        }
        self.slots.lock().unwrap().queue.push_back(key);
        let mut place = Place {
            inner: self,
            key,
            holding: false,
        };
        self.report_queue();
        self.broadcast_load();
        let permit = self.task_slots.acquire().await?;
        {
            let mut slots = self.slots.lock().unwrap();
            slots.queue.retain(|k| *k != key);
            slots.holding.insert(key);
        }
        place.holding = true;
        self.report_queue();
        self.broadcast_load();
        Ok((permit, place))
    }

    /// Tells each queued task's requester its place in the queue.
    fn report_queue(&self) {
        let queue: Vec<(EndpointId, u64)> =
            self.slots.lock().unwrap().queue.iter().copied().collect();
        for (ahead, (peer, task_id)) in queue.into_iter().enumerate() {
            let place = match ahead {
                0 => "next in line".to_owned(),
                1 => "1 task ahead".to_owned(),
                n => format!("{n} tasks ahead"),
            };
            let text = format!("waiting for a free slot on {} ({place})", self.name);
            self.send_progress(&peer, task_id, Progress::new(Phase::Queued, text));
        }
    }
    /// Brings folder `share`'s clone up to its repository's latest commit before task
    /// `task_id` from `id` starts in it, if it is a linked repository's clone, telling the
    /// requester. Returns a note for the task's result when the task starts from the
    /// folder as it was instead.
    async fn update_folder(&self, id: &EndpointId, task_id: u64, share: &Share) -> Option<String> {
        use crate::github::CloneUpdate;
        let pulls = self.pulls.as_ref()?;
        let name = &share.name;
        let (text, note) = match pulls.update(name).await {
            Ok(CloneUpdate::NotLinked | CloneUpdate::Current { .. }) => return None,
            Ok(CloneUpdate::Moved { head, commits }) => (
                format!(
                    "updated folder '{name}' to its repository's latest commit {head} ({commits} newer commit{})",
                    if commits == 1 { "" } else { "s" }
                ),
                None,
            ),
            Ok(CloneUpdate::Kept { head, why }) => (
                format!("starting from commit {head} of folder '{name}' as it was: {why}"),
                Some(format!(
                    "Note: this task started from commit {head} of folder '{name}' as it was, not brought up to date with its repository: {why}."
                )),
            ),
            Err(e) => (
                format!("starting from folder '{name}' as it was: {e:#}"),
                Some(format!(
                    "Note: this task started from folder '{name}' as it was; it couldn't be brought up to date with its repository: {e:#}."
                )),
            ),
        };
        self.send_progress(id, task_id, Progress::new(Phase::Running, text));
        note
    }

    /// Runs an admitted task: in its own worktree if the folder uses them, applying
    /// the requester's patch, then the model and the requested checks, retrying
    /// with the failures while attempts are left. Returns the signed evidence.
    #[allow(clippy::too_many_arguments)]
    async fn attempt_task(
        &self,
        executor: &dyn Executor,
        id: &EndpointId,
        from: &str,
        task_id: u64,
        task: &LlmTask,
        mut ctx: TaskContext,
        share: &Share,
    ) -> Result<Served> {
        // A linked repository's clone first gets the repository's latest commit.
        let kept = self.update_folder(id, task_id, share).await;
        let worktree = if share.worktrees && ctx.access >= Access::WorkspaceWrite {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let branch = format!("p2p/{}/{task_id}-{stamp}", branch_part(from));
            let wt = Worktree::create(&share.path, &self.worktrees_dir, &branch)
                .await
                .context("creating the task's worktree")?;
            ctx.workspace = wt.path.clone();
            Some(wt)
        } else {
            None
        };
        if let (Some(patch), Some(wt)) = (&task.patch, &worktree) {
            wt.apply(patch).await?;
        }
        // (name, owner command); built-in checks have no command.
        let checks: Vec<(&String, Option<&String>)> = task
            .checks
            .iter()
            .filter_map(|name| {
                if crate::spec::is_built_in(name) {
                    Some((name, None))
                } else {
                    share.checks.get(name).map(|command| (name, Some(command)))
                }
            })
            .collect();
        // The model may run those checks itself while it works; they run again after it,
        // and only those runs count (see crate::checks_mcp).
        let commands: Vec<(String, String)> = checks
            .iter()
            .filter_map(|(name, command)| command.map(|c| ((*name).clone(), c.clone())))
            .collect();
        if let Some(program) = &self.task_checks
            && !commands.is_empty()
            && task.provider != Provider::Codex
        {
            ctx.mcp.insert(
                crate::checks_mcp::SERVER.to_owned(),
                crate::checks_mcp::server(program, &ctx.workspace, &commands),
            );
        }
        let attempts = task.attempts.unwrap_or(1).clamp(1, MAX_ATTEMPTS);
        let mut prompt = task.clone();
        // The model it asked for (allowed: checked when it was admitted), else the
        // default; the same for its reasoning effort. Settings changed since it was
        // admitted apply: it fails if they no longer allow what it asks for.
        let choice = crate::models::choice(&self.models.lock().unwrap(), task.provider);
        prompt.model = choice.pick(task.model.as_deref())?;
        prompt.effort = choice.pick_effort(task.effort.as_deref())?;
        let mut results = Vec::<crate::evidence::CheckResult>::new();
        let (mut reports, mut used) = (Vec::<String>::new(), 0);
        let mut failed_before = Vec::<String>::new();
        // The last attempt ended because the model's run stopped, not because checks failed.
        let mut stopped_before = false;
        // The model that did the work, and its effort: the latest attempt's, as its
        // provider says.
        let (mut model, mut effort) = (None, None);
        while used < attempts {
            used += 1;
            let text = match (attempts, used) {
                (1, _) => "the model is working".to_owned(),
                (n, 1) => format!("attempt 1 of {n}: the model is working"),
                (n, k) if stopped_before => format!(
                    "attempt {k} of {n}: the model is working, going on from where its last run stopped"
                ),
                (n, k) => format!(
                    "attempt {k} of {n}: the model is working, told what failed ({})",
                    failed_before.join(", ")
                ),
            };
            self.send_progress(
                id,
                task_id,
                Progress {
                    attempt: Some(used),
                    attempts: Some(attempts),
                    ..Progress::new(Phase::Running, text)
                },
            );
            let ran = match executor.execute(&prompt, &ctx).await {
                Ok(ran) => ran,
                // The run stopped before the model finished (it ran out of time, or its
                // provider failed): the next attempt goes on from what it changed.
                Err(e) if used < attempts => {
                    let why = format!("{e:#}");
                    reports.push(format!("The model's run stopped before it finished: {why}"));
                    reports.push(format!(
                        "Attempt {} of {attempts}, after the model's run stopped",
                        used + 1
                    ));
                    stopped_before = true;
                    prompt.prompt = crate::evidence::resume_prompt(&task.prompt, &why);
                    continue;
                }
                Err(e) => {
                    let mut why = format!("{e:#}");
                    // What the model changed stays on the task's branch, to look at or go on from.
                    if let Some(wt) = &worktree {
                        let (subject, own) = commit_subject(task);
                        let message = format!(
                            "{subject} (unfinished)\n\n{}Task {task_id} from {from}: the model's run stopped before it finished. Kept by {} with Proofkin.",
                            own.map(|o| format!("{o}\n\n")).unwrap_or_default(),
                            self.name
                        );
                        if let Ok((record, _)) = wt.commit(&message).await
                            && let Some(commit) = record.commit
                        {
                            why += &format!(
                                " (what it changed is on branch {}, commit {})",
                                wt.branch,
                                commit.chars().take(12).collect::<String>()
                            );
                        }
                    }
                    bail!("{why}");
                }
            };
            model = ran.model.or(model);
            effort = ran.effort.or(effort);
            reports.push(ran.output);
            results.clear();
            for (i, (name, command)) in checks.iter().enumerate() {
                let mut text = match attempts {
                    1 => String::new(),
                    n => format!("attempt {used} of {n}: "),
                };
                text += &match checks.len() {
                    1 => format!("running check {name}"),
                    n => format!("running check {name} ({} of {n})", i + 1),
                };
                if !results.is_empty() {
                    let earlier: Vec<String> = results
                        .iter()
                        .map(|r| {
                            format!("{} {}", r.name, if r.passed { "passed" } else { "failed" })
                        })
                        .collect();
                    text += &format!("; {}", earlier.join(", "));
                }
                self.send_progress(
                    id,
                    task_id,
                    Progress {
                        attempt: Some(used),
                        attempts: Some(attempts),
                        ..Progress::new(Phase::Checking, text)
                    },
                );
                results.push(match command {
                    Some(command) => run_check(name, command, &ctx.workspace, CHECK_TIMEOUT).await,
                    None => run_built_in(name, &ctx.workspace).await,
                });
            }
            if results.iter().all(|r| r.passed) {
                break;
            }
            failed_before = results
                .iter()
                .filter(|r| !r.passed)
                .map(|r| r.name.clone())
                .collect();
            stopped_before = false;
            if used < attempts {
                reports.push(format!(
                    "Attempt {} of {attempts}, after these checks failed: {}",
                    used + 1,
                    failed_before.join(", ")
                ));
            }
            prompt.prompt = retry_prompt(&task.prompt, &results);
        }
        // Each later attempt's report follows a line saying why it was made.
        let mut output = String::new();
        for (i, part) in reports.iter().enumerate() {
            if i > 0 {
                output += "\n\n";
            }
            output += if i == 0 {
                part.as_str()
            } else {
                part.trim_end()
            };
        }
        if let Some(note) = &kept {
            output = format!("{}\n\n{note}", output.trim_end());
        }
        // Results leave this machine: secrets the model or a check printed stay here.
        output = crate::redact::redact_with_note(&output, &self.name);
        for r in &mut results {
            r.output = crate::redact::redact(&r.output).0;
        }
        let mut evidence = Evidence::new(
            EvidenceKind::Task,
            &self.name,
            self.endpoint.id(),
            *id,
            task_id,
        );
        evidence.provider = Some(task.provider);
        evidence.share = Some(share.name.clone());
        evidence.prompt_sha256 = sha256_hex(task.prompt.as_bytes());
        // Checked when the task was admitted.
        evidence.context =
            crate::evidence::context_uses(&task.prompt, &task.context).unwrap_or_default();
        evidence.attempts = used;
        evidence.checks = results;
        let mut patch = None;
        if let Some(wt) = &worktree {
            self.send_progress(
                id,
                task_id,
                Progress::new(Phase::Committing, "committing the work"),
            );
            let (subject, own) = commit_subject(task);
            let mut message = subject;
            if let Some(own) = own {
                message += &format!("\n\n{own}");
            }
            message += &format!(
                "\n\nTask {task_id} from {from}, done by {} with Proofkin.",
                self.name
            );
            // The issues it is for (checked: they come from the peer), as trailers a
            // pull request from the branch closes them by.
            let refs: Vec<&String> = task
                .issues
                .iter()
                .filter(|r| crate::tracker::valid_ref(r))
                .take(20)
                .collect();
            if !refs.is_empty() {
                message += "\n";
                for r in refs {
                    message += &format!("\nRefs: {r}");
                }
            }
            let (commit, text) = wt.commit(&message).await?;
            evidence.git = Some(commit);
            patch = text;
        }
        // The model is named in the signed output, with its reasoning effort, so the
        // record vouches for them too (older requesters verify it as before: it is part
        // of the output's hash).
        let mut summary = model
            .as_deref()
            .map(|m| {
                format!(
                    "**Model** `{m}` ({}){}\n",
                    task.provider,
                    effort
                        .as_deref()
                        .map(|e| format!(", {e} reasoning"))
                        .unwrap_or_default()
                )
            })
            .unwrap_or_default();
        summary += &evidence.summary();
        if !summary.is_empty() {
            output = format!("{}\n\n---\n{summary}", output.trim_end());
        }
        let evidence = evidence.sign(&output, self.endpoint.secret_key());
        // The signed record stays with the commit, so a pull request from the branch
        // can carry it. Best effort: the requester has the record either way.
        if let (Some(wt), Some(commit)) = (
            &worktree,
            evidence.git.as_ref().and_then(|g| g.commit.as_deref()),
        ) && let Ok(record) = serde_json::to_string(&evidence)
        {
            let _ = wt.note(commit, &record).await;
        }
        Ok(Served {
            ok: evidence.checks_passed(),
            output,
            evidence,
            patch,
            // Results name the model with its effort: every version shows it as it is.
            model: crate::models::with_effort(model, effort.as_deref()),
        })
    }
}

/// The subject of the commit of `task`'s work, and the line under it, if any: the title of
/// the issue it works on with its own title under it, else its own title (else its first
/// line's).
fn commit_subject(task: &LlmTask) -> (String, Option<String>) {
    let first = task.prompt.lines().next().unwrap_or_default();
    let own = task
        .title
        .as_deref()
        .map(crate::llm::title_of)
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| crate::llm::title_of(first));
    match task
        .subject
        .as_deref()
        .map(crate::llm::title_of)
        .filter(|s| !s.is_empty() && *s != own)
    {
        Some(subject) => (subject, Some(own).filter(|o| !o.is_empty())),
        None => (own, None),
    }
}

/// A peer's models as its hello says, kept only as far as they make sense: well-formed
/// names, at most 50 per provider, a default only if it is one of the allowed, and
/// reasoning efforts only if they are ones (the default only if allowed).
fn clean_models(models: crate::models::Models) -> crate::models::Models {
    let ok = |m: &String| crate::models::check_name(m).is_ok();
    let effort_ok = |e: &String| crate::models::check_effort(e).is_ok();
    models
        .into_iter()
        .map(|(provider, choice)| {
            let allowed: Vec<String> = choice.allowed.into_iter().filter(ok).take(50).collect();
            let default = choice.default.filter(ok).filter(|d| {
                allowed.is_empty() || allowed.iter().any(|m| m.eq_ignore_ascii_case(d))
            });
            let mut efforts: Vec<String> = Vec::new();
            for effort in choice.efforts.iter().filter(|e| effort_ok(e)) {
                let effort = effort.to_ascii_lowercase();
                if !efforts.contains(&effort) {
                    efforts.push(effort);
                }
            }
            efforts.sort_by_key(|e| crate::models::EFFORTS.iter().position(|x| x == e));
            let effort = choice
                .effort
                .filter(effort_ok)
                .map(|e| e.to_ascii_lowercase())
                .filter(|e| efforts.is_empty() || efforts.contains(e));
            (
                provider,
                crate::models::ModelChoice {
                    default,
                    allowed,
                    effort,
                    efforts,
                },
            )
        })
        .filter(|(_, choice)| !choice.is_empty())
        .collect()
}

async fn run_connection(
    inner: Arc<Inner>,
    conn: Connection,
    mut send: SendStream,
    recv: RecvStream,
    outbound: bool,
) -> Result<()> {
    let peer_id = conn.remote_id();
    let mut lines = BufReader::new(recv).lines();

    let hello = Message::Hello {
        name: inner.name.clone(),
        addr: inner.endpoint.addr(),
        capabilities: inner.capabilities(),
        shares: inner.shares_for(&peer_id),
        owner: inner.owner.clone(),
        role: inner.role.clone(),
        models: inner.models_offered(),
        offers: inner.offers_for(Some(&inner.capabilities())),
    };
    write_message(&mut send, &hello).await?;
    let first = lines
        .next_line()
        .await?
        .context("peer closed before hello")?;
    let Message::Hello {
        name: announced,
        addr,
        capabilities,
        shares,
        owner,
        role,
        models,
        offers,
    } = serde_json::from_str(&first)?
    else {
        bail!("expected hello, got {first}");
    };
    // Gossiped addresses must belong to the authenticated peer.
    let addr = EndpointAddr::from_parts(peer_id, addr.addrs);
    let name = inner
        .trust
        .lock()
        .unwrap()
        .get(&peer_id)
        .map(|p| p.name.clone())
        .unwrap_or(announced);

    let (tx, mut rx) = mpsc::unbounded_channel();
    let close = Arc::new(Notify::new());
    let conn_id = inner.next_conn_id.fetch_add(1, Ordering::Relaxed);
    let handle = PeerHandle {
        conn_id,
        name: name.clone(),
        addr,
        offers: crate::models::clean_offers(offers, &capabilities),
        capabilities,
        shares,
        owner,
        role,
        load: None,
        models: clean_models(models),
        tx: tx.clone(),
        close: close.clone(),
    };
    let known = inner.known_addrs();
    match inner.register(peer_id, handle, outbound) {
        Registration::Rejected => return Ok(()),
        Registration::Replaced => {}
        Registration::New => {
            // A peer that reconnects within the disconnect grace period was never
            // reported as gone, so it isn't reported as joining again either.
            if inner.announced.lock().unwrap().insert(peer_id) {
                let _ = inner.events.send(Event::PeerConnected {
                    name: name.clone(),
                    id: peer_id,
                });
            }
        }
    }
    // Back before its tasks gave up on it.
    inner.left.lock().unwrap().remove(&peer_id);
    inner.send_held(peer_id, &tx);
    // A shutdown that raced with this handshake missed our close handle.
    if *inner.shutdown.borrow() {
        close.notify_one();
    }

    let writer_task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if write_message(&mut send, &msg).await.is_err() {
                break;
            }
        }
    });
    let _ = tx.send(Message::Peers { addrs: known });
    if inner.executor.is_some() {
        let _ = tx.send(inner.load_for(&peer_id));
    }
    // Requests still waiting for this peer may have been queued on a connection that
    // was replaced: send them again (the peer ignores ones it already has).
    let resend: Vec<Message> = inner
        .pending
        .lock()
        .unwrap()
        .values()
        .filter(|p| p.peer == peer_id)
        .filter_map(|p| p.request.clone())
        .collect();
    for msg in resend {
        let _ = tx.send(msg);
    }

    let result = async {
        loop {
            let line = tokio::select! {
                line = lines.next_line() => line?,
                _ = close.notified() => break,
            };
            let Some(line) = line else { break };
            match serde_json::from_str::<Message>(&line)? {
                Message::Chat { from, text } => {
                    let _ = inner.events.send(Event::Chat { from, text });
                }
                Message::Peers { addrs } => {
                    // Only allowlisted peers are dialed; dial() enforces it.
                    for addr in addrs.into_iter().filter(|a| inner.is_trusted(&a.id)) {
                        let inner = inner.clone();
                        tokio::spawn(async move {
                            let _ = dial(inner, addr).await;
                        });
                    }
                }
                Message::TaskRequest {
                    task_id,
                    task,
                    origin,
                } => {
                    let key = (peer_id, task_id);
                    // A re-sent request: it's running (its requester hears where it is
                    // again), or its result is sent again.
                    if inner.serving.lock().unwrap().contains_key(&key) {
                        let last = inner.last_progress.lock().unwrap().get(&key).cloned();
                        if let Some(progress) = last {
                            let _ = tx.send(Message::TaskProgress { task_id, progress });
                        }
                        continue;
                    }
                    let done = inner
                        .recent
                        .lock()
                        .unwrap()
                        .get(&key)
                        .filter(|(t, _, at)| t == &task && at.elapsed() < RECENT_RESULTS)
                        .map(|(_, msg, _)| msg.clone());
                    if let Some(msg) = done {
                        let _ = tx.send(msg);
                        continue;
                    }
                    let (inner, from) = (inner.clone(), name.clone());
                    let original = task.clone();
                    let (cancel_tx, cancel_rx) = oneshot::channel();
                    inner
                        .serving
                        .lock()
                        .unwrap()
                        .insert((peer_id, task_id), cancel_tx);
                    tokio::spawn(async move {
                        let (result, refused) = inner
                            .serve_task(&peer_id, &from, task_id, task, origin, cancel_rx)
                            .await;
                        let msg = match result {
                            Ok(served) => Message::TaskResult {
                                task_id,
                                ok: served.ok,
                                output: served.output,
                                evidence: Some(Box::new(served.evidence)),
                                patch: served.patch,
                                refused: false,
                                model: served.model,
                            },
                            Err(e) => Message::TaskResult {
                                task_id,
                                ok: false,
                                output: crate::redact::redact(&format!("{e:#}")).0,
                                evidence: None,
                                patch: None,
                                refused,
                                model: None,
                            },
                        };
                        {
                            // Remember the result before the task stops counting as
                            // running, so a re-sent request never runs it twice.
                            let mut recent = inner.recent.lock().unwrap();
                            recent.retain(|_, (_, _, at)| at.elapsed() < RECENT_RESULTS);
                            recent.insert(
                                (peer_id, task_id),
                                (original, msg.clone(), Instant::now()),
                            );
                        }
                        inner.serving.lock().unwrap().remove(&(peer_id, task_id));
                        inner.deliver(peer_id, msg).await;
                    });
                }
                Message::ApprovalRequest {
                    task_id,
                    title,
                    text,
                    mut preview,
                    prototype,
                    changes,
                } => {
                    if preview.is_empty() {
                        preview = prototype
                            .into_iter()
                            .map(|(path, text)| {
                                (format!("{}{path}", crate::preview::PROTOTYPE_DIR), text)
                            })
                            .collect();
                    }
                    let ask = ApprovalAsk {
                        title,
                        text,
                        preview,
                        changes,
                    };
                    if !inner.add_approval(peer_id, name.clone(), task_id, ask) {
                        let msg = Message::TaskResult {
                            task_id,
                            ok: false,
                            output: format!(
                                "{} has too many open approval requests from you",
                                inner.name
                            ),
                            evidence: None,
                            patch: None,
                            refused: true,
                            model: None,
                        };
                        let _ = tx.send(msg);
                    }
                }
                Message::TaskCancel { task_id } => {
                    inner.withdraw_approval(&peer_id, task_id);
                    let cancel = inner.serving.lock().unwrap().remove(&(peer_id, task_id));
                    if let Some(cancel) = cancel {
                        let _ = cancel.send(());
                    }
                }
                Message::TaskResult {
                    task_id,
                    ok,
                    output,
                    evidence,
                    patch,
                    refused,
                    model,
                } => {
                    let verified = evidence.as_ref().is_some_and(|e| {
                        e.verify(&peer_id, &output).is_ok()
                            && e.git
                                .as_ref()
                                .and_then(|g| g.patch_sha256.as_deref())
                                .is_none_or(|hash| {
                                    patch
                                        .as_ref()
                                        .is_none_or(|p| sha256_hex(p.as_bytes()) == hash)
                                })
                            // A team run is checked through: every step's evidence.
                            && e.team.as_ref().is_none_or(|run| {
                                crate::runs::verify_team(run, &peer_id.to_string()).is_ok()
                            })
                    });
                    inner.complete_pending(
                        &peer_id,
                        task_id,
                        Ok(Delivered {
                            ok,
                            output,
                            evidence: evidence.map(|e| *e),
                            patch,
                            verified,
                            refused,
                            // A peer's word, shown as such: a short, single-line name.
                            model: model
                                .map(|m| crate::llm::clean_model(&m))
                                .filter(|m| !m.is_empty()),
                        }),
                    );
                }
                Message::TaskProgress { task_id, progress } => {
                    if let Some(task) = inner.pending.lock().unwrap().get(&task_id)
                        && task.peer == peer_id
                        && let Some(tx) = &task.progress
                    {
                        let _ = tx.send(progress);
                    }
                }
                Message::Load {
                    running,
                    queued,
                    slots,
                    yours,
                } => {
                    if let Some(handle) = inner.peers.lock().unwrap().get_mut(&peer_id)
                        && handle.conn_id == conn_id
                    {
                        let load = Some(PeerLoad {
                            running,
                            queued,
                            slots,
                            yours,
                        });
                        if handle.load != load {
                            handle.load = load;
                            let _ = inner.events.send(Event::PeerBusy { id: peer_id });
                        }
                    }
                }
                Message::Shares { shares } => {
                    if let Some(handle) = inner.peers.lock().unwrap().get_mut(&peer_id)
                        && handle.conn_id == conn_id
                    {
                        handle.shares = shares;
                    }
                }
                Message::Models { models, offers } => {
                    if let Some(handle) = inner.peers.lock().unwrap().get_mut(&peer_id)
                        && handle.conn_id == conn_id
                    {
                        let models = clean_models(models);
                        let offers = crate::models::clean_offers(offers, &handle.capabilities);
                        if handle.models != models || handle.offers != offers {
                            handle.models = models;
                            handle.offers = offers;
                            let _ = inner.events.send(Event::PeerModels { id: peer_id });
                        }
                    }
                }
                Message::Hello { .. } => bail!("unexpected second hello"),
                Message::Unknown => {}
            }
        }
        Ok(())
    }
    .await;

    writer_task.abort();
    conn.close(0u32.into(), b"bye");
    if inner.unregister(peer_id, conn_id) {
        // Only if the peer doesn't come back quickly (e.g. this connection lost the
        // simultaneous-dial tie-break) is it reported gone. Its tasks wait longer (the
        // task grace) for it to come back: then the tasks delegated to it fail and the
        // tasks it asked this agent to run stop.
        let inner = inner.clone();
        tokio::spawn(async move {
            tokio::time::sleep(DISCONNECT_GRACE).await;
            let gone = !inner.peers.lock().unwrap().contains_key(&peer_id);
            if !gone {
                return;
            }
            let left = Instant::now();
            inner.left.lock().unwrap().insert(peer_id, left);
            if inner.announced.lock().unwrap().remove(&peer_id) {
                let _ = inner.events.send(Event::PeerDisconnected {
                    name: name.clone(),
                    id: peer_id,
                });
            }
            inner.report_reconnecting(&peer_id, &name);
            tokio::time::sleep(inner.task_grace.saturating_sub(DISCONNECT_GRACE)).await;
            // Still gone since then (not back, nor gone again later).
            if inner.left.lock().unwrap().get(&peer_id) != Some(&left) {
                return;
            }
            inner.left.lock().unwrap().remove(&peer_id);
            inner.fail_pending(&peer_id);
            inner.cancel_served(&peer_id);
            inner
                .recent
                .lock()
                .unwrap()
                .retain(|(p, _), _| p != &peer_id);
            let gone_requests: Vec<(EndpointId, u64)> = inner
                .approvals
                .lock()
                .unwrap()
                .values()
                .filter(|a| a.requester == peer_id)
                .map(|a| (a.requester, a.task_id))
                .collect();
            for (requester, task_id) in gone_requests {
                inner.withdraw_approval(&requester, task_id);
            }
        });
    }
    result
}
