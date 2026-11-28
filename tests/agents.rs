//! Multi-agent tests over real iroh QUIC endpoints on localhost (`Network::Local`:
//! direct IPs only, no relays or lookup services, so no internet is needed).

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use proofkin::audit::{AuditLog, Outcome};
use proofkin::mcp::McpServer;
use proofkin::plan::{Plan, RunContext, SquadStore, StepState, run_plan, run_plan_with};
use proofkin::protocol::{Phase, Progress};
use proofkin::pulls::{Asked, Opened, PullRequestAsk, PullRequests, Review};
use proofkin::runs::{RunRecord, Verdict};
use proofkin::shares::{DATA_SHARE, ShareInfo, ShareStore, parse_checks, parse_task_limit};
use proofkin::{
    Access, Agent, AgentConfig, EndpointId, Event, Executor, LlmTask, Network, Profile, Provider,
    Ran, SecretKey, TaskContext, TrustStore, TrustedPeer, Usage,
};
use tokio::sync::Notify;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;

/// A node's built-in data folder (the fake executor never touches it).
fn data_folder(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join("p2p-test-data").join(name)
}

/// Echoes the prompt; "hang" never completes (and counts in `dropped` when its run
/// is dropped, i.e. stopped), "fail" errors, "access" returns the access the agent
/// granted the task and "where" the folder it runs in.
struct FakeExecutor {
    providers: Vec<Provider>,
    dropped: Arc<AtomicUsize>,
}

/// Increments the counter when dropped.
struct CountDrop(Arc<AtomicUsize>);

impl Drop for CountDrop {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl Executor for FakeExecutor {
    fn providers(&self) -> Vec<Provider> {
        self.providers.clone()
    }

    async fn execute(&self, task: &LlmTask, ctx: &TaskContext) -> anyhow::Result<Ran> {
        // The model it ran on, as a provider says; "which model": a provider that says
        // which model answered a task that named none. It works at the effort it is given.
        let model = task.model.clone().or_else(|| {
            task.prompt
                .ends_with("which model")
                .then(|| "fake-model-7".to_owned())
        });
        // "report-usage": a provider that reports what each run used.
        let usage = task.prompt.contains("report-usage").then(fake_usage);
        Ok(Ran::new(self.answer(task, ctx).await?, model)
            .with_effort(task.effort.clone())
            .with_usage(usage))
    }

    /// Copilot lists three models: one with reasoning efforts, one without, and one
    /// its account can't use.
    async fn list_models(
        &self,
        provider: Provider,
    ) -> anyhow::Result<Vec<proofkin::models::Offered>> {
        let model = |id: &str, efforts: &[&str], policy: Option<&str>| proofkin::models::Offered {
            id: id.into(),
            name: id.to_uppercase(),
            vendor: Some("Fake Labs".into()),
            multiplier: Some("0.33".into()),
            policy: policy.map(Into::into),
            efforts: efforts.iter().map(|e| e.to_string()).collect(),
            default_effort: efforts.first().map(|e| e.to_string()),
        };
        Ok(match provider {
            Provider::Copilot => vec![
                model("fake-big", &["low", "medium", "high", "max"], None),
                model("fake-small", &[], None),
                model("fake-off", &[], Some("disabled")),
            ],
            Provider::Claude | Provider::Codex => Vec::new(),
        })
    }
}

impl FakeExecutor {
    async fn answer(&self, task: &LlmTask, ctx: &TaskContext) -> anyhow::Result<String> {
        // A lead's model writing the team plan of a team step: only read-only and with no
        // MCP servers, it has the member build the task, noting the model and effort it
        // planned on, the notes it read in the folder and the changes its owner asked for.
        // "plan-badly" in the task makes its first plan name a peer that isn't there, and
        // "plan-badly-always" every one (the next one each time: nobody, nobody2, ...).
        if task.prompt.contains("## How a team plan is written") {
            if ctx.access != Access::ReadOnly || !ctx.mcp.is_empty() {
                return Ok("I could change things here, so I won't plan".into());
            }
            let told = task
                .prompt
                .contains("## Your last team plan doesn't check out");
            if task.prompt.contains("plan-badly-always")
                || (task.prompt.contains("plan-badly") && !told)
            {
                let nobody = match task.prompt.split_once("@nobody") {
                    None => "nobody".to_owned(),
                    Some((_, after)) => {
                        let n: u32 = after.split(':').next().unwrap().parse().unwrap_or(1);
                        format!("nobody{}", n + 1)
                    }
                };
                return Ok(format!("```plan\nwork @{nobody}:code {{task}}\n```"));
            }
            let notes = std::fs::read_to_string(ctx.workspace.join("notes.txt"))?;
            let changed = task
                .prompt
                .split_once("and asked for changes:\n\n")
                .map(|(_, note)| format!("// changed: {}\n", note.lines().next().unwrap()))
                .unwrap_or_default();
            return Ok(format!(
                "Here is the plan:\n\n```plan\nclaim: T-1 {{task}} passes the member's check\n\
                 // planned on {} at {}, with notes: {}\n\
                 {changed}build @member:code verify=ok claims=T-1 {{task}}\n```\n\nThat's all.",
                task.model.as_deref().unwrap_or("its default"),
                task.effort.as_deref().unwrap_or("its effort"),
                notes.trim()
            ));
        }
        // Its run stops before it finishes (as one that runs out of time does) unless it was
        // told its last run stopped: then it finds what that run changed, and finishes.
        if task.prompt.ends_with("stops-once") {
            if task.prompt.contains("stopped before you finished") {
                let earlier =
                    std::fs::read_to_string(ctx.workspace.join("started.txt")).unwrap_or_default();
                std::fs::write(ctx.workspace.join("finished.txt"), "finished\n")?;
                return Ok(format!("finished it; the last run had left {earlier:?}"));
            }
            std::fs::write(ctx.workspace.join("started.txt"), "started\n")?;
            anyhow::bail!("Copilot task failed: timed out after 1200s");
        }
        // Done again for the changes a sign-off asked for: it says whether its earlier work
        // is there, and writes down what it was asked.
        if let Some((_, asked)) = task.prompt.split_once("## Changes asked for\n\n") {
            let earlier =
                std::fs::read_to_string(ctx.workspace.join("feature.txt")).unwrap_or_default();
            std::fs::write(ctx.workspace.join("changes.txt"), asked)?;
            return Ok(format!("changed it; its earlier work said {earlier:?}"));
        }
        // "create <file>" as a plan step's own task (after the earlier steps' results).
        let own = task
            .prompt
            .rsplit("Your task:\n")
            .next()
            .unwrap_or(&task.prompt);
        // Skills put in front of the task's own words.
        let own = match own.rsplit_once("\n---\n") {
            Some((_, text)) if own.starts_with("Follow these instructions:") => text,
            _ => own,
        };
        if let Some(file) = task
            .prompt
            .strip_prefix("write ")
            .or_else(|| own.strip_prefix("create "))
        {
            let path = ctx.workspace.join(file);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, "made by the model\n")?;
            return Ok(format!("wrote {file}"));
        }
        // "show <file>": what the file in its workspace says.
        if let Some(file) = own.strip_prefix("show ") {
            return Ok(match std::fs::read_to_string(ctx.workspace.join(file)) {
                Ok(text) => format!("{file} says: {text}"),
                Err(_) => format!("{file} is missing"),
            });
        }
        // Runs the step's check "test" through the tool its worker gave it, as a model
        // would: it starts the MCP server it was given and calls the tool.
        if own == "use the checks tool" {
            let Some(server) = ctx.mcp.get(proofkin::checks_mcp::SERVER) else {
                return Ok("no checks tool".into());
            };
            let mut child = tokio::process::Command::new(server.command.as_deref().unwrap())
                .args(&server.args)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true)
                .spawn()?;
            let call = serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": { "name": proofkin::checks_mcp::TOOL, "arguments": { "name": "test" } } });
            let reply_line = {
                use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
                let mut stdin = child.stdin.take().unwrap();
                stdin.write_all(format!("{call}\n").as_bytes()).await?;
                let mut line = String::new();
                BufReader::new(child.stdout.take().unwrap())
                    .read_line(&mut line)
                    .await?;
                line
            };
            let out = child.wait_with_output().await?;
            anyhow::ensure!(
                out.status.success(),
                "the task-checks process exited with {}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
            );
            let reply: serde_json::Value = serde_json::from_str(&reply_line)?;
            return Ok(reply["result"]["content"][0]["text"]
                .as_str()
                .unwrap_or("no answer")
                .to_owned());
        }
        // Fails its checks the first time; the retry (which gets the failures) fixes it.
        if task.prompt.ends_with("fix-on-retry") {
            if task.prompt.contains("did not pass the checks") {
                std::fs::write(ctx.workspace.join("fixed.txt"), "fixed\n")?;
                return Ok("fixed it".into());
            }
            return Ok("first try".into());
        }
        match task.prompt.as_str() {
            "hang" => {
                let _count = CountDrop(self.dropped.clone());
                std::future::pending().await
            }
            "fail" => anyhow::bail!("model error"),
            "slow" => {
                tokio::time::sleep(Duration::from_millis(1500)).await;
                Ok("slow done".into())
            }
            "access" => Ok(ctx.access.to_string()),
            "where" => Ok(ctx.workspace.display().to_string()),
            "mcp" => Ok(ctx.mcp.keys().cloned().collect::<Vec<_>>().join(",")),
            "leak" => Ok(format!(
                "The deploy token is ghp_{} (keep it safe).",
                "Zx9".repeat(12)
            )),
            prompt => Ok(format!("{} did: {prompt}", task.provider)),
        }
    }
}

struct Node {
    name: &'static str,
    key: SecretKey,
    trust: TrustStore,
    executor: Option<Arc<dyn Executor>>,
    audit: Option<Arc<AuditLog>>,
    /// Hanging task runs that were stopped (see [`FakeExecutor`]).
    stopped: Arc<AtomicUsize>,
    shares: ShareStore,
    owner: Option<String>,
    role: Option<String>,
    /// How long its tasks wait for a dropped peer (short, so tests don't wait long).
    grace: Duration,
    team: Option<Arc<proofkin::team::TeamRunner>>,
    /// Opens the pull requests of pull request steps (see [`FakePulls`]).
    pulls: Option<Arc<dyn PullRequests>>,
    /// Its providers' default models and the ones tasks may ask for.
    models: proofkin::models::Models,
    /// What it knows and learns (see [`worker_memory`]).
    memory: Option<Arc<proofkin::factory::memory::WorkerMemory>>,
    /// For a digital worker: its owner's endpoint id.
    owner_id: Option<EndpointId>,
}

impl Node {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            key: SecretKey::generate(),
            trust: TrustStore::default(),
            executor: None,
            audit: None,
            stopped: Arc::default(),
            shares: ShareStore::with_data(data_folder(name)),
            owner: None,
            role: None,
            grace: Duration::from_secs(2),
            team: None,
            pulls: None,
            models: Default::default(),
            memory: None,
            owner_id: None,
        }
    }

    fn id(&self) -> EndpointId {
        self.key.public()
    }

    fn serving(mut self, providers: Vec<Provider>) -> Self {
        self.executor = Some(Arc::new(FakeExecutor {
            providers,
            dropped: self.stopped.clone(),
        }));
        self
    }

    /// Allowlists `other`, optionally letting it delegate up to `per_hour` tasks.
    fn trusts(&mut self, other: &Node, delegate: Option<u32>) {
        self.trust.allow(TrustedPeer {
            name: other.name.into(),
            id: other.id(),
            addrs: BTreeSet::new(),
            can_delegate: delegate.is_some(),
            tasks_per_hour: delegate.unwrap_or(0),
            access: Access::ReadOnly,
            shares: vec![DATA_SHARE.to_owned()],
            m365: None,
            github: None,
            agents: Vec::new(),
        });
    }

    async fn start(self) -> (Agent, UnboundedReceiver<Event>) {
        let mut config = AgentConfig::new(self.name, self.key.clone());
        config.network = Network::Local;
        config.bind = Some("127.0.0.1:0".parse().unwrap());
        config.trust = self.trust;
        config.executor = self.executor;
        config.audit = self.audit;
        config.shares = self.shares;
        config.owner = self.owner;
        config.role = self.role;
        config.models = self.models;
        config.task_grace = self.grace;
        config.team = self.team;
        config.pulls = self.pulls;
        config.memory = self.memory;
        config.owner_id = self.owner_id;
        // The real program serves a task's checks to its model.
        config.task_checks = Some(env!("CARGO_BIN_EXE_proofkin").into());
        config.worktrees_dir = std::env::temp_dir()
            .join("p2p-test-worktrees")
            .join(format!("{}-{}", self.name, self.key.public().fmt_short()));
        Agent::bind(config).await.unwrap()
    }

    /// Sets what `peer` may do here: its access and the folders it may use.
    fn grant(&mut self, peer: &str, access: Access, shares: &[&str]) {
        let entry = self
            .trust
            .peers
            .iter_mut()
            .find(|p| p.name == peer)
            .expect("allowlisted");
        entry.access = access;
        entry.shares = shares.iter().map(|s| (*s).to_owned()).collect();
    }
}

/// A shell command that succeeds only if `file` exists in the current folder.
fn exists_check(file: &str) -> String {
    if cfg!(windows) {
        format!("type {file}")
    } else {
        format!("cat {file}")
    }
}

fn git(dir: &std::path::Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

/// A git repository with one commit.
fn git_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("README.md"), "demo\n").unwrap();
    git(dir.path(), &["init", "-q"]);
    git(dir.path(), &["config", "core.autocrlf", "false"]);
    git(dir.path(), &["add", "-A"]);
    git(
        dir.path(),
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "-m",
            "init",
        ],
    );
    dir
}

fn mutual(a: &mut Node, b: &mut Node) {
    a.trusts(b, Some(100));
    b.trusts(a, Some(100));
}

async fn next_matching(
    events: &mut UnboundedReceiver<Event>,
    pred: impl Fn(&Event) -> bool,
) -> Event {
    // Generous: with the whole suite running, a step's git work waits for other tests'.
    timeout(Duration::from_secs(30), async {
        loop {
            let event = events.recv().await.expect("event channel closed");
            if pred(&event) {
                return event;
            }
        }
    })
    .await
    .expect("timed out waiting for event")
}

async fn wait_for_peers(agent: &Agent, count: usize) {
    timeout(Duration::from_secs(10), async {
        while agent.peers().len() != count {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "{} has {:?}, expected {count} peers",
            agent.name(),
            agent.peers()
        )
    });
}

#[tokio::test]
async fn peers_discover_each_other_via_gossip_and_chat() {
    let (mut a, mut b, mut c) = (Node::new("a"), Node::new("b"), Node::new("c"));
    mutual(&mut a, &mut b);
    mutual(&mut a, &mut c);
    mutual(&mut b, &mut c);
    let (a, _ae) = a.start().await;
    let (b, _be) = b.start().await;
    let (c, mut c_events) = c.start().await;

    b.connect(a.addr()).await.unwrap();
    wait_for_peers(&a, 1).await;
    // c only dials a; it learns b's address from a's gossip.
    c.connect(a.addr()).await.unwrap();
    for agent in [&a, &b, &c] {
        wait_for_peers(agent, 2).await;
    }

    assert_eq!(b.broadcast("hello"), 2);
    let event = next_matching(&mut c_events, |e| matches!(e, Event::Chat { .. })).await;
    assert_eq!(
        event,
        Event::Chat {
            from: "b".into(),
            text: "hello".into()
        }
    );
}

#[tokio::test]
async fn alerts_reach_teammates_who_connect_later() {
    let (mut a, mut b) = (Node::new("a"), Node::new("b"));
    mutual(&mut a, &mut b);
    let (a, _ae) = a.start().await;
    // b isn't connected: a holds its alerts (the last 20) until it is.
    for n in 1..=22 {
        assert_eq!(a.alert(format!("[a alert] api opened: failure {n}")), 0);
    }
    assert_eq!(a.broadcast("just chat"), 0);
    let (b, mut b_events) = b.start().await;
    b.connect(a.addr()).await.unwrap();
    let chat = |e: &Event| matches!(e, Event::Chat { .. });
    let mut held = Vec::new();
    for _ in 0..20 {
        let Event::Chat { from, text } = next_matching(&mut b_events, chat).await else {
            unreachable!()
        };
        assert_eq!(from, "a");
        held.push(text);
    }
    assert_eq!(
        held[0],
        "[a alert] api opened: failure 3 (raised less than a minute ago, while you weren't connected)"
    );
    assert!(held[19].starts_with("[a alert] api opened: failure 22 (raised"));
    // Connected now: alerts arrive at once, and none of the held ones again.
    assert_eq!(a.alert("[a alert] api resolved"), 1);
    assert_eq!(
        next_matching(&mut b_events, chat).await,
        Event::Chat {
            from: "a".into(),
            text: "[a alert] api resolved".into()
        }
    );
}

#[tokio::test]
async fn rejects_peers_that_are_not_allowlisted() {
    let mut a = Node::new("a");
    let mut stranger = Node::new("stranger");
    stranger.trusts(&a, None);
    let a_id = a.id();
    let mut b = Node::new("b");
    a.trusts(&b, None);
    let (a, mut a_events) = a.start().await;
    let (stranger, _se) = stranger.start().await;

    // stranger trusts a, but a does not trust stranger: a drops the connection.
    let _ = stranger.connect(a.addr()).await;
    let event = next_matching(&mut a_events, |e| matches!(e, Event::PeerRejected { .. })).await;
    assert_eq!(event, Event::PeerRejected { id: stranger.id() });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(a.peers().is_empty());
    assert!(stranger.peers().is_empty());

    // An agent refuses to dial keys it has not allowlisted.
    b.trust = TrustStore::default();
    let (b, _be) = b.start().await;
    let err = b.connect(a.addr()).await.unwrap_err();
    assert!(err.to_string().contains("not on the allowlist"), "{err}");
    assert_eq!(a.id(), a_id);
}

#[tokio::test]
async fn gossip_does_not_connect_peers_that_do_not_trust_each_other() {
    let (mut a, mut b, mut c) = (Node::new("a"), Node::new("b"), Node::new("c"));
    mutual(&mut a, &mut b);
    mutual(&mut a, &mut c);
    let (a, _ae) = a.start().await;
    let (b, _be) = b.start().await;
    let (c, _ce) = c.start().await;

    b.connect(a.addr()).await.unwrap();
    wait_for_peers(&a, 1).await;
    c.connect(a.addr()).await.unwrap();
    wait_for_peers(&a, 2).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(b.peers().len(), 1);
    assert_eq!(c.peers().len(), 1);
}

#[tokio::test]
async fn simultaneous_dials_converge_on_one_connection() {
    let (mut a, mut b) = (Node::new("a"), Node::new("b"));
    mutual(&mut a, &mut b);
    let (a, _ae) = a.start().await;
    let (b, mut b_events) = b.start().await;

    let (ra, rb) = tokio::join!(a.connect(b.addr()), b.connect(a.addr()));
    ra.unwrap();
    rb.unwrap();
    wait_for_peers(&a, 1).await;
    wait_for_peers(&b, 1).await;
    // Longer than the disconnect grace period, so a spurious leave would show up.
    tokio::time::sleep(Duration::from_millis(2500)).await;

    let mut joins = 0;
    while let Ok(event) = b_events.try_recv() {
        assert!(
            !matches!(event, Event::PeerDisconnected { .. }),
            "dial race reported a disconnect"
        );
        joins += matches!(event, Event::PeerConnected { .. }) as usize;
    }
    assert_eq!(joins, 1, "dial race reported more than one join");
    assert_eq!(a.peers().len(), 1);
    assert_eq!(b.peers().len(), 1);
    assert_eq!(a.broadcast("ping"), 1);
    let event = next_matching(&mut b_events, |e| matches!(e, Event::Chat { .. })).await;
    assert_eq!(
        event,
        Event::Chat {
            from: "a".into(),
            text: "ping".into()
        }
    );
}

/// A peer being dialed isn't dialed again until that connection's handshake ends, as when
/// the program dials a `--peer` and `keep_connected` dials it right after: two connections
/// from one side, in their handshakes at once, could be kept differently by the two sides,
/// each closing the one the other kept. This peer never says hello, so the first dial
/// stays in its handshake until the peer closes it.
#[tokio::test]
async fn a_peer_being_dialed_is_not_dialed_again_until_its_handshake_ends() {
    let key = SecretKey::generate();
    let silent = iroh::Endpoint::builder(iroh::endpoint::presets::Minimal)
        .secret_key(key.clone())
        .alpns(vec![proofkin::protocol::ALPN.to_vec()])
        .bind_addr("127.0.0.1:0".parse::<std::net::SocketAddr>().unwrap())
        .unwrap()
        .bind()
        .await
        .unwrap();
    let held = Arc::new(Mutex::new(Vec::new()));
    tokio::spawn({
        let (silent, held) = (silent.clone(), held.clone());
        async move {
            while let Some(incoming) = silent.accept().await {
                if let Ok(conn) = incoming.await {
                    held.lock().unwrap().push(conn);
                }
            }
        }
    });
    let accepted = || held.lock().unwrap().len();
    let mut a = Node::new("a");
    a.trust.allow(TrustedPeer {
        name: "silent".into(),
        id: key.public(),
        addrs: BTreeSet::new(),
        can_delegate: false,
        tasks_per_hour: 0,
        access: Access::ReadOnly,
        shares: Vec::new(),
        m365: None,
        github: None,
        agents: Vec::new(),
    });
    let (a, _ae) = a.start().await;

    a.connect(silent.addr()).await.unwrap();
    timeout(Duration::from_secs(10), async {
        while accepted() == 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the first dial reached the peer");
    a.connect(silent.addr()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(accepted(), 1, "a second connection was opened");

    // Closed before its hello, the dial ends: the peer is dialed again.
    for conn in held.lock().unwrap().iter() {
        conn.close(0u32.into(), b"bye");
    }
    timeout(Duration::from_secs(10), async {
        while accepted() < 2 {
            a.connect(silent.addr()).await.unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the peer was dialed again once the handshake failed");
}

/// A plan checked while a peer reconnects waits for it (`settle_peers`), instead of
/// finding it not connected: here its connection closed and it dials again. One that
/// doesn't come back is given up on after the disconnect grace.
#[tokio::test]
async fn settling_waits_for_a_reconnecting_peer_and_gives_up_on_a_gone_one() {
    let (mut a, mut b) = (Node::new("a"), Node::new("b"));
    mutual(&mut a, &mut b);
    let (a, _ae) = a.start().await;
    let (b, _be) = b.start().await;
    b.connect(a.addr()).await.unwrap();
    wait_for_peers(&a, 1).await;
    wait_for_peers(&b, 1).await;

    assert!(a.disconnect(b.id()));
    wait_for_peers(&a, 0).await;
    let redial = tokio::spawn({
        let (b, addr) = (b.clone(), a.addr());
        async move {
            wait_for_peers(&b, 0).await;
            b.connect(addr).await
        }
    });
    a.settle_peers().await;
    assert_eq!(a.peers().len(), 1, "it waited for b to come back");
    redial.await.unwrap().unwrap();

    assert!(a.disconnect(b.id()));
    wait_for_peers(&a, 0).await;
    let started = std::time::Instant::now();
    a.settle_peers().await;
    assert!(a.peers().is_empty());
    let waited = started.elapsed();
    assert!(waited < Duration::from_secs(5), "{waited:?}");
}

#[tokio::test]
async fn shutdown_disconnects_peers() {
    let (mut a, mut b) = (Node::new("a"), Node::new("b"));
    mutual(&mut a, &mut b);
    let b_id = b.id();
    let (a, mut a_events) = a.start().await;
    let (b, _be) = b.start().await;

    b.connect(a.addr()).await.unwrap();
    next_matching(&mut a_events, |e| matches!(e, Event::PeerConnected { .. })).await;

    b.shutdown().await;
    let event = next_matching(&mut a_events, |e| {
        matches!(e, Event::PeerDisconnected { .. })
    })
    .await;
    assert_eq!(
        event,
        Event::PeerDisconnected {
            name: "b".into(),
            id: b_id
        }
    );
    assert!(a.peers().is_empty());
    assert!(b.connect(a.addr()).await.is_err());
}

#[tokio::test]
async fn delegates_tasks_to_a_peer_that_serves_the_provider() {
    let mut worker = Node::new("worker").serving(vec![Provider::Claude, Provider::Codex]);
    let mut boss = Node::new("boss");
    mutual(&mut worker, &mut boss);
    let (worker, mut worker_events) = worker.start().await;
    let (boss, _be) = boss.start().await;
    boss.connect(worker.addr()).await.unwrap();
    wait_for_peers(&boss, 1).await;

    let peer = boss
        .find_peer(Provider::Codex)
        .expect("worker advertises codex");
    assert_eq!(peer.capabilities, vec![Provider::Claude, Provider::Codex]);
    assert!(boss.find_peer(Provider::Copilot).is_none());

    let access = boss
        .delegate(peer.id, LlmTask::new(Provider::Codex, "access"))
        .await
        .unwrap();
    assert_eq!(
        access, "read-only",
        "peers get read-only unless granted more"
    );
    let mut grant = worker.trusted().get(&boss.id()).unwrap().clone();
    grant.access = Access::WorkspaceWrite;
    worker.allow(grant);
    let access = boss
        .delegate(peer.id, LlmTask::new(Provider::Codex, "access"))
        .await
        .unwrap();
    assert_eq!(
        access, "read-only",
        "the data folder caps every peer at read-only"
    );

    let out = boss
        .delegate(peer.id, LlmTask::new(Provider::Codex, "add tests"))
        .await
        .unwrap();
    assert_eq!(out, "codex did: add tests");
    let received = next_matching(&mut worker_events, |e| {
        matches!(e, Event::TaskReceived { .. })
    })
    .await;
    assert!(
        matches!(received, Event::TaskReceived { from, provider: Provider::Codex, .. } if from == "boss")
    );

    let err = boss
        .delegate(peer.id, LlmTask::new(Provider::Claude, "fail"))
        .await
        .unwrap_err();
    assert_eq!(err.to_string(), "model error");

    // Secrets a model printed never leave the worker; the signature covers what did.
    let leaked = boss
        .delegate_verified(peer.id, LlmTask::new(Provider::Claude, "leak"))
        .await
        .unwrap();
    assert!(leaked.verified, "{leaked:?}");
    assert!(
        leaked
            .output
            .starts_with("The deploy token is [REDACTED:github-token] (keep it safe).")
            && leaked.output.contains("worker redacted 1 secret")
            && !leaked.output.contains("Zx9Zx9"),
        "{}",
        leaked.output
    );
}

#[tokio::test]
async fn the_worker_says_which_model_did_the_work() {
    let mut worker = Node::new("worker").serving(vec![Provider::Copilot]);
    let mut boss = Node::new("boss");
    mutual(&mut worker, &mut boss);
    let audit_dir = tempfile::tempdir().unwrap();
    let audit_path = audit_dir.path().join("audit.jsonl");
    worker.audit = Some(Arc::new(AuditLog::new(audit_path.clone())));
    let (worker, _we) = worker.start().await;
    let (boss, _be) = boss.start().await;
    boss.connect(worker.addr()).await.unwrap();
    wait_for_peers(&boss, 1).await;

    // The result names the model, and so does its signed output (so older requesters,
    // which don't read the field, verify it as before).
    let done = boss
        .delegate_verified(worker.id(), LlmTask::new(Provider::Copilot, "which model"))
        .await
        .unwrap();
    assert_eq!(done.model.as_deref(), Some("fake-model-7"));
    assert!(done.verified, "{done:?}");
    assert_eq!(
        done.output,
        "copilot did: which model\n\n---\n**Model** `fake-model-7` (copilot)\n"
    );
    // A provider that doesn't say leaves it out, and the output as it was.
    let plain = boss
        .delegate_verified(worker.id(), LlmTask::new(Provider::Copilot, "hello"))
        .await
        .unwrap();
    assert_eq!(plain.model, None);
    assert_eq!(plain.output, "copilot did: hello");
    // The worker's audit log records it.
    let entries = AuditLog::read(&audit_path, 10).unwrap();
    assert_eq!(entries[0].model.as_deref(), Some("fake-model-7"));
    assert_eq!(entries[1].model, None);
    // A plan step keeps it with its result.
    let plan = Plan::parse("a @worker which model").unwrap();
    let states = run_plan(&boss, &SquadStore::default(), &plan, |_, _| {}).await;
    assert_eq!(states["a"].model(), Some("fake-model-7"));
}

/// What the fake provider says each "report-usage" run used.
fn fake_usage() -> Usage {
    Usage {
        input_tokens: Some(1_000),
        cached_input_tokens: Some(600),
        output_tokens: Some(50),
        premium_requests: Some(1.0),
        cost_usd: None,
    }
}

#[tokio::test]
async fn the_worker_reports_what_its_provider_said_the_work_used() {
    let dir = tempfile::tempdir().unwrap();
    let mut worker = Node::new("worker").serving(vec![Provider::Copilot]);
    let mut boss = Node::new("boss");
    mutual(&mut worker, &mut boss);
    let checks = parse_checks(&[format!("fixed={}", exists_check("fixed.txt"))]).unwrap();
    worker
        .shares
        .add_with("repo", dir.path(), Access::Full, checks, false)
        .unwrap();
    worker.grant("boss", Access::Full, &["data", "repo"]);
    let audit_dir = tempfile::tempdir().unwrap();
    let audit_path = audit_dir.path().join("audit.jsonl");
    worker.audit = Some(Arc::new(AuditLog::new(audit_path.clone())));
    let (worker, _we) = worker.start().await;
    let (boss, _be) = boss.start().await;
    boss.connect(worker.addr()).await.unwrap();
    wait_for_peers(&boss, 1).await;

    let once = fake_usage();
    let done = boss
        .delegate_verified(worker.id(), LlmTask::new(Provider::Copilot, "report-usage"))
        .await
        .unwrap();
    assert_eq!(done.usage, Some(once.clone()));
    // A provider that reports none: none, not zero.
    let plain = boss
        .delegate_verified(worker.id(), LlmTask::new(Provider::Copilot, "hello"))
        .await
        .unwrap();
    assert_eq!(plain.usage, None);
    // Every attempt's usage counts.
    let retried = LlmTask::new(Provider::Copilot, "report-usage fix-on-retry")
        .in_share("repo")
        .with_checks(vec!["fixed".into()], Some(2));
    let retried = boss.delegate_verified(worker.id(), retried).await.unwrap();
    assert!(retried.ok, "{}", retried.output);
    assert_eq!(retried.usage, Some(once.plus(&once)));
    // The worker's audit records it, and neither the prompt nor the output.
    let entries = AuditLog::read(&audit_path, 10).unwrap();
    let used: Vec<Option<Usage>> = entries.iter().map(|e| e.usage.clone()).collect();
    assert_eq!(used, [Some(once.clone()), None, Some(once.plus(&once))]);
    let text = std::fs::read_to_string(&audit_path).unwrap();
    assert!(
        !text.contains("report-usage") && !text.contains("fixed it"),
        "{text}"
    );
}

#[tokio::test]
async fn owners_set_the_default_model_and_which_ones_tasks_may_ask_for() {
    let mut worker = Node::new("worker").serving(vec![Provider::Copilot]);
    proofkin::models::set_allowed(
        &mut worker.models,
        Provider::Copilot,
        &["fake-small".into(), "fake-big".into()],
    )
    .unwrap();
    proofkin::models::set_default(&mut worker.models, Provider::Copilot, Some("fake-big")).unwrap();
    // Models of a provider it doesn't serve aren't told.
    proofkin::models::set_default(&mut worker.models, Provider::Claude, Some("x")).unwrap();
    let mut boss = Node::new("boss");
    mutual(&mut worker, &mut boss);
    let (worker, _we) = worker.start().await;
    let (boss, _be) = boss.start().await;
    boss.connect(worker.addr()).await.unwrap();
    wait_for_peers(&boss, 1).await;

    // Peers learn them from the worker's hello.
    let peer = boss.peers().into_iter().next().unwrap();
    assert_eq!(peer.models.keys().collect::<Vec<_>>(), [&Provider::Copilot]);
    assert_eq!(
        peer.models[&Provider::Copilot].default.as_deref(),
        Some("fake-big")
    );
    assert_eq!(
        peer.models[&Provider::Copilot].allowed,
        ["fake-small", "fake-big"]
    );
    // A task that names no model runs on the default.
    let plain = boss
        .delegate_verified(worker.id(), LlmTask::new(Provider::Copilot, "hello"))
        .await
        .unwrap();
    assert_eq!(plain.model.as_deref(), Some("fake-big"));
    // One that asks for an allowed model runs on it.
    let mut small = LlmTask::new(Provider::Copilot, "hello");
    small.model = Some("fake-small".into());
    let done = boss.delegate_verified(worker.id(), small).await.unwrap();
    assert_eq!(done.model.as_deref(), Some("fake-small"));
    // Another is refused (so a squad step can go to another member), naming the allowed.
    let mut other = LlmTask::new(Provider::Copilot, "hello");
    other.model = Some("o9".into());
    let refused = boss.delegate_verified(worker.id(), other).await.unwrap();
    assert!(refused.refused && !refused.ok, "{refused:?}");
    assert_eq!(
        refused.output,
        "worker refused the task: o9 isn't a model allowed here: fake-small, fake-big"
    );
}

/// A step that works on an issue (issue=#N) gets the issue's text, as its tracker
/// has it, in front of its task as external content, so the model needn't look it up.
#[tokio::test]
async fn a_step_gets_the_text_of_the_issue_it_works_on() {
    let repo = git_repo();
    let mut worker = Node::new("worker").serving(vec![Provider::Copilot]);
    let mut boss = Node::new("boss");
    mutual(&mut worker, &mut boss);
    worker
        .shares
        .add_with(
            "app",
            repo.path(),
            Access::WorkspaceWrite,
            Default::default(),
            true,
        )
        .unwrap();
    worker.grant("boss", Access::WorkspaceWrite, &["data", "app"]);
    let (worker, _we) = worker.start().await;
    let (boss, _be) = boss.start().await;
    boss.connect(worker.addr()).await.unwrap();
    wait_for_peers(&boss, 1).await;
    let plan = Plan::parse(
        "docs @worker issue=#12 Document it\n\
         plain @worker Say hi\n\
         api @worker:app issue=#13 create api.md",
    )
    .unwrap();
    let ctx = RunContext {
        issue_texts: [
            (
                "docs".to_owned(),
                "acme/app#12 Document the API\n\nEvery endpoint.".to_owned(),
            ),
            (
                "api".to_owned(),
                "acme/app#13 Add the orders API\n\nGET /orders.".to_owned(),
            ),
        ]
        .into(),
        ..RunContext::default()
    };
    let states = run_plan_with(&boss, &SquadStore::default(), &plan, &ctx, |_, _| {}).await;
    let output = |id: &str| match &states[id] {
        StepState::Done { output, .. } => output.clone(),
        other => panic!("{id}: {other:?}"),
    };
    let docs = output("docs");
    assert!(
        docs.contains(
            "<external_content source=\"issue:12\">\nacme/app#12 Document the API\n\nEvery endpoint.\n</external_content>"
        ),
        "{docs}"
    );
    assert!(docs.ends_with("Your task:\nDocument it"), "{docs}");
    assert_eq!(output("plain"), "copilot did: Say hi");
    // Its commit is titled with the issue's title, with the step's own words under it.
    let StepState::Done {
        evidence: Some(evidence),
        ..
    } = &states["api"]
    else {
        panic!("{:?}", states["api"])
    };
    let commit = evidence.git.as_ref().unwrap().commit.clone().unwrap();
    let log = std::process::Command::new("git")
        .arg("-C")
        .arg(repo.path())
        .args(["log", "-1", "--format=%B", &commit])
        .output()
        .unwrap();
    let message = String::from_utf8_lossy(&log.stdout).replace("\r\n", "\n");
    assert!(
        message.starts_with("Add the orders API\n\ncreate api.md\n\nTask "),
        "{message}"
    );
}

/// An owner sets the reasoning effort its tasks work at; a task may ask for another.
/// The result names it with the model (in characters every version shows as they are),
/// its signed output says it, and a name that isn't an effort is refused.
#[tokio::test]
async fn tasks_work_at_the_owners_reasoning_effort_unless_they_ask_for_another() {
    let mut worker = Node::new("worker").serving(vec![Provider::Copilot]);
    proofkin::models::set_default(&mut worker.models, Provider::Copilot, Some("fake-big")).unwrap();
    proofkin::models::set_effort(&mut worker.models, Provider::Copilot, Some("max")).unwrap();
    let mut boss = Node::new("boss");
    mutual(&mut worker, &mut boss);
    let (worker, _we) = worker.start().await;
    let (boss, _be) = boss.start().await;
    boss.connect(worker.addr()).await.unwrap();
    wait_for_peers(&boss, 1).await;

    // Peers learn the effort from the hello.
    let peer = boss.peers().into_iter().next().unwrap();
    assert_eq!(
        peer.models[&Provider::Copilot].effort.as_deref(),
        Some("max")
    );
    let plain = boss
        .delegate_verified(worker.id(), LlmTask::new(Provider::Copilot, "hello"))
        .await
        .unwrap();
    assert_eq!(plain.model.as_deref(), Some("fake-big (max reasoning)"));
    assert!(plain.verified, "{plain:?}");
    assert_eq!(
        plain.output,
        "copilot did: hello\n\n---\n**Model** `fake-big` (copilot), max reasoning\n"
    );
    let mut low = LlmTask::new(Provider::Copilot, "hello");
    low.effort = Some("low".into());
    let done = boss.delegate_verified(worker.id(), low).await.unwrap();
    assert_eq!(done.model.as_deref(), Some("fake-big (low reasoning)"));
    let mut odd = LlmTask::new(Provider::Copilot, "hello");
    odd.effort = Some("turbo".into());
    let refused = boss.delegate_verified(worker.id(), odd).await.unwrap();
    assert!(refused.refused && !refused.ok, "{refused:?}");
    assert!(
        refused
            .output
            .starts_with("worker refused the task: \"turbo\" isn't a reasoning effort"),
        "{}",
        refused.output
    );
}

#[tokio::test]
async fn plan_defaults_reach_the_worker_and_its_signed_result() {
    let mut worker = Node::new("worker").serving(vec![Provider::Copilot]);
    let mut boss = Node::new("boss");
    mutual(&mut worker, &mut boss);
    let (worker, _we) = worker.start().await;
    let (boss, _be) = boss.start().await;
    boss.connect(worker.addr()).await.unwrap();
    wait_for_peers(&boss, 1).await;

    let plan = Plan::parse(
        "model: fake-big\neffort: high\na @worker Hello\nb @worker model=fake-other effort=low Hi",
    )
    .unwrap();
    let states = run_plan(&boss, &SquadStore::default(), &plan, |_, _| {}).await;
    assert_eq!(states["a"].model(), Some("fake-big (high reasoning)"));
    assert_eq!(states["b"].model(), Some("fake-other (low reasoning)"));
    let StepState::Done { output, .. } = &states["a"] else {
        panic!("the inherited choice should run: {:?}", states["a"]);
    };
    assert!(output.contains("**Model** `fake-big` (copilot), high reasoning"));
}

/// A worker lets tasks ask only for some reasoning efforts (it refuses others, and one
/// that asks for none works at the least of them), and tells peers the models its
/// account offers and can use, with the efforts each supports.
#[tokio::test]
async fn workers_allow_only_some_efforts_and_tell_peers_the_models_they_offer() {
    let mut worker = Node::new("worker").serving(vec![Provider::Copilot]);
    proofkin::models::set_default(&mut worker.models, Provider::Copilot, Some("fake-big")).unwrap();
    proofkin::models::set_efforts(
        &mut worker.models,
        Provider::Copilot,
        &["medium".into(), "low".into()],
    )
    .unwrap();
    let mut boss = Node::new("boss");
    mutual(&mut worker, &mut boss);
    let executor = worker.executor.clone().unwrap();
    let (worker, _we) = worker.start().await;
    // It lists them when it starts (here before its peer connects).
    assert!(worker.learn_offers(executor.as_ref()).await.is_empty());
    assert_eq!(
        worker.listed()[&Provider::Copilot].len(),
        3,
        "all, as listed"
    );
    let (boss, _be) = boss.start().await;
    boss.connect(worker.addr()).await.unwrap();
    wait_for_peers(&boss, 1).await;

    let peer = boss.peers().into_iter().next().unwrap();
    assert_eq!(peer.models[&Provider::Copilot].efforts, ["low", "medium"]);
    let offers = &peer.offers[&Provider::Copilot];
    let ids: Vec<&str> = offers.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(
        ids,
        ["fake-big", "fake-small"],
        "the ones its account can use"
    );
    assert_eq!(offers[0].efforts, ["low", "medium", "high", "max"]);
    assert_eq!(
        (offers[0].name.as_str(), offers[0].multiplier.as_deref()),
        ("FAKE-BIG", Some("0.33"))
    );
    assert!(offers[1].efforts.is_empty(), "it takes no reasoning effort");
    // Asking for none: the least allowed effort; an allowed one: it; another: refused.
    let plain = boss
        .delegate_verified(worker.id(), LlmTask::new(Provider::Copilot, "hello"))
        .await
        .unwrap();
    assert_eq!(plain.model.as_deref(), Some("fake-big (low reasoning)"));
    let mut medium = LlmTask::new(Provider::Copilot, "hello");
    medium.effort = Some("medium".into());
    let done = boss.delegate_verified(worker.id(), medium).await.unwrap();
    assert_eq!(done.model.as_deref(), Some("fake-big (medium reasoning)"));
    let mut max = LlmTask::new(Provider::Copilot, "hello");
    max.effort = Some("max".into());
    let refused = boss.delegate_verified(worker.id(), max).await.unwrap();
    assert!(refused.refused && !refused.ok, "{refused:?}");
    assert_eq!(
        refused.output,
        "worker refused the task: max isn't a reasoning effort allowed here: low, medium"
    );
}

/// A worker's models changed while it runs (its owner limited them), and the models it
/// lists, reach connected peers at once (the models it offers cut to the allowed ones),
/// and its next task runs with them.
#[tokio::test]
async fn model_changes_reach_connected_peers_at_once() {
    let mut worker = Node::new("worker").serving(vec![Provider::Copilot]);
    let mut boss = Node::new("boss");
    mutual(&mut worker, &mut boss);
    let executor = worker.executor.clone().unwrap();
    let (worker, mut worker_events) = worker.start().await;
    let (boss, mut boss_events) = boss.start().await;
    boss.connect(worker.addr()).await.unwrap();
    wait_for_peers(&boss, 1).await;
    assert!(boss.peers()[0].offers.is_empty(), "not listed yet");

    worker.learn_offers(executor.as_ref()).await;
    next_matching(&mut worker_events, |e| matches!(e, Event::ModelsChanged)).await;
    next_matching(&mut boss_events, |e| matches!(e, Event::PeerModels { .. })).await;
    assert_eq!(boss.peers()[0].offers[&Provider::Copilot].len(), 2);

    // Its owner allows only fake-small, at high or max reasoning (max unless asked).
    let mut models = proofkin::models::Models::new();
    let only = proofkin::models::ModelChoice {
        allowed: vec!["fake-small".into()],
        effort: Some("max".into()),
        efforts: vec!["high".into(), "max".into()],
        ..Default::default()
    };
    proofkin::models::set_choice(&mut models, Provider::Copilot, &only).unwrap();
    worker.set_models(models.clone());
    assert_eq!(worker.models(), models);
    next_matching(&mut boss_events, |e| matches!(e, Event::PeerModels { .. })).await;
    let peer = boss.peers().into_iter().next().unwrap();
    assert_eq!(peer.models, models);
    let ids: Vec<&str> = peer.offers[&Provider::Copilot]
        .iter()
        .map(|m| m.id.as_str())
        .collect();
    assert_eq!(ids, ["fake-small"], "only the allowed ones");
    let plain = boss
        .delegate_verified(worker.id(), LlmTask::new(Provider::Copilot, "hello"))
        .await
        .unwrap();
    assert_eq!(plain.model.as_deref(), Some("fake-small (max reasoning)"));
    let mut big = LlmTask::new(Provider::Copilot, "hello");
    big.model = Some("fake-big".into());
    let refused = boss.delegate_verified(worker.id(), big).await.unwrap();
    assert!(refused.refused && !refused.ok, "{refused:?}");
}

/// A running agent applies its profile's models when its profile.json changes, and a
/// digital worker its owner's for providers it sets nothing for: no restart.
#[tokio::test]
async fn running_agents_apply_profile_model_changes_and_workers_their_owners() {
    let root = tempfile::tempdir().unwrap();
    let mut owner = proofkin::profile::Profile::open_in(root.path(), "owner").unwrap();
    proofkin::models::set_default(
        &mut owner.config.models,
        Provider::Copilot,
        Some("fake-big"),
    )
    .unwrap();
    owner.save().unwrap();
    let mut profile = proofkin::profile::Profile::open_in(root.path(), "helper").unwrap();
    profile.config.owner = Some("owner".into());
    profile.save().unwrap();
    let mut worker = Node::new("worker").serving(vec![Provider::Copilot]);
    worker.models = profile.models();
    let mut boss = Node::new("boss");
    mutual(&mut worker, &mut boss);
    let (worker, _we) = worker.start().await;
    worker.watch_models(profile.clone(), None, Duration::from_millis(50));
    let (boss, mut boss_events) = boss.start().await;
    boss.connect(worker.addr()).await.unwrap();
    wait_for_peers(&boss, 1).await;
    let said = |boss: &Agent| boss.peers()[0].models[&Provider::Copilot].clone();
    assert_eq!(
        said(&boss).default.as_deref(),
        Some("fake-big"),
        "its owner's"
    );

    // The owner sets max reasoning: the worker uses it, and its peers learn it.
    proofkin::models::set_effort(&mut owner.config.models, Provider::Copilot, Some("max")).unwrap();
    owner.save().unwrap();
    next_matching(&mut boss_events, |e| matches!(e, Event::PeerModels { .. })).await;
    assert_eq!(said(&boss).effort.as_deref(), Some("max"));
    let done = boss
        .delegate_verified(worker.id(), LlmTask::new(Provider::Copilot, "hello"))
        .await
        .unwrap();
    assert_eq!(done.model.as_deref(), Some("fake-big (max reasoning)"));
    // Its own settings for a provider replace its owner's.
    let mut helper = profile.reload().unwrap();
    proofkin::models::set_default(
        &mut helper.config.models,
        Provider::Copilot,
        Some("fake-small"),
    )
    .unwrap();
    helper.save().unwrap();
    next_matching(&mut boss_events, |e| matches!(e, Event::PeerModels { .. })).await;
    let now = said(&boss);
    assert_eq!(
        (now.default.as_deref(), now.effort.as_deref()),
        (Some("fake-small"), None)
    );
}

#[tokio::test]
async fn delegation_needs_permission_and_respects_the_hourly_limit() {
    let mut worker = Node::new("worker").serving(vec![Provider::Copilot]);
    let mut chatty = Node::new("chatty");
    let mut limited = Node::new("limited");
    worker.trusts(&chatty, None);
    worker.trusts(&limited, Some(1));
    chatty.trusts(&worker, None);
    limited.trusts(&worker, None);
    let audit_dir = tempfile::tempdir().unwrap();
    let audit_path = audit_dir.path().join("audit.jsonl");
    worker.audit = Some(Arc::new(AuditLog::new(audit_path.clone())));
    let (worker, _we) = worker.start().await;
    let (chatty, _ce) = chatty.start().await;
    let (limited, _le) = limited.start().await;
    chatty.connect(worker.addr()).await.unwrap();
    limited.connect(worker.addr()).await.unwrap();
    wait_for_peers(&worker, 2).await;
    wait_for_peers(&chatty, 1).await;
    wait_for_peers(&limited, 1).await;

    let err = chatty
        .delegate(worker.id(), LlmTask::new(Provider::Copilot, "hi"))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not allowed to delegate"), "{err}");

    limited
        .delegate(worker.id(), LlmTask::new(Provider::Copilot, "one"))
        .await
        .unwrap();
    let err = limited
        .delegate(worker.id(), LlmTask::new(Provider::Copilot, "two"))
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("limit of 1 tasks per hour"),
        "{err}"
    );

    // Every request is audited on the worker: who, outcome and access, no prompt text.
    let entries = AuditLog::read(&audit_path, 10).unwrap();
    let summary: Vec<(&str, Outcome)> = entries
        .iter()
        .map(|e| (e.peer.as_str(), e.outcome))
        .collect();
    assert_eq!(
        summary,
        [
            ("chatty", Outcome::Refused),
            ("limited", Outcome::Done),
            ("limited", Outcome::Refused),
        ]
    );
    assert_eq!(entries[1].access, Some(Access::ReadOnly));
    assert_eq!(entries[1].peer_id, limited.id().to_string());
    assert!(entries[0].error.as_deref().unwrap().contains("not allowed"));
    let raw = std::fs::read_to_string(&audit_path).unwrap();
    assert!(!raw.contains("\"one\"") && !raw.contains("\"two\""));
}

#[tokio::test]
async fn refuses_providers_the_peer_does_not_serve() {
    let mut worker = Node::new("worker").serving(vec![Provider::Claude]);
    let mut plain = Node::new("plain");
    mutual(&mut worker, &mut plain);
    let (worker, _we) = worker.start().await;
    let (plain, _pe) = plain.start().await;
    plain.connect(worker.addr()).await.unwrap();
    wait_for_peers(&plain, 1).await;
    wait_for_peers(&worker, 1).await;

    let err = plain
        .delegate(worker.id(), LlmTask::new(Provider::Copilot, "hi"))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("does not serve copilot"), "{err}");

    // An agent without an executor advertises nothing, so nothing can be delegated to it.
    assert!(plain.capabilities().is_empty());
    let err = worker
        .delegate(plain.id(), LlmTask::new(Provider::Claude, "hi"))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("does not serve claude"), "{err}");
}

/// A worker that stops says so: its tasks end at once, as not done there (so a
/// squad step could move to another member), instead of waiting for it to return.
#[tokio::test]
async fn delegated_task_fails_at_once_when_the_worker_stops() {
    let mut worker = Node::new("worker").serving(vec![Provider::Copilot]);
    let mut boss = Node::new("boss");
    // However long tasks would wait for it to come back.
    boss.grace = Duration::from_secs(60);
    mutual(&mut worker, &mut boss);
    let (worker, mut worker_events) = worker.start().await;
    let (boss, _be) = boss.start().await;
    boss.connect(worker.addr()).await.unwrap();
    wait_for_peers(&boss, 1).await;

    let pending = tokio::spawn({
        let boss = boss.clone();
        let id = worker.id();
        async move {
            boss.delegate_verified(id, LlmTask::new(Provider::Copilot, "hang"))
                .await
        }
    });
    next_matching(&mut worker_events, |e| {
        matches!(e, Event::TaskReceived { .. })
    })
    .await;
    worker.shutdown().await;

    let delivered = timeout(Duration::from_secs(10), pending)
        .await
        .expect("no waiting for the grace")
        .unwrap()
        .unwrap();
    assert!(!delivered.ok && delivered.refused, "{delivered:?}");
    assert_eq!(
        delivered.output,
        "worker stopped before it finished the task"
    );
}

/// A peer that drops and doesn't come back within the task grace: the tasks given
/// to it fail (as gone) and the tasks it asked for stop.
#[tokio::test]
async fn delegated_task_fails_when_peer_disconnects() {
    let mut worker = Node::new("worker").serving(vec![Provider::Copilot]);
    let mut boss = Node::new("boss");
    mutual(&mut worker, &mut boss);
    let stopped = worker.stopped.clone();
    let (worker, mut worker_events) = worker.start().await;
    let (boss, _be) = boss.start().await;
    boss.connect(worker.addr()).await.unwrap();
    wait_for_peers(&boss, 1).await;

    let pending = tokio::spawn({
        let boss = boss.clone();
        let id = worker.id();
        async move {
            boss.delegate(id, LlmTask::new(Provider::Copilot, "hang"))
                .await
        }
    });
    next_matching(&mut worker_events, |e| {
        matches!(e, Event::TaskReceived { .. })
    })
    .await;
    assert!(boss.disconnect(worker.id()));

    let err = timeout(Duration::from_secs(10), pending)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(err.to_string().contains("disconnected"), "{err}");
    assert!(err.downcast_ref::<proofkin::agent::PeerGone>().is_some());
    wait_stopped(&stopped, 1).await;
}

/// Waits until `count` hanging runs on the worker have been stopped.
async fn wait_stopped(stopped: &AtomicUsize, count: usize) {
    timeout(Duration::from_secs(10), async {
        while stopped.load(Ordering::SeqCst) < count {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "only {} of {count} runs stopped",
            stopped.load(Ordering::SeqCst)
        )
    });
}

#[tokio::test]
async fn a_requester_gone_past_the_task_grace_stops_its_tasks_with_a_reason() {
    let mut worker = Node::new("worker").serving(vec![Provider::Copilot]);
    let mut boss = Node::new("boss");
    worker.grace = Duration::from_secs(3);
    // Keep waiting on the requester's end so we can see the worker's result on return.
    boss.grace = Duration::from_secs(60);
    mutual(&mut worker, &mut boss);
    let stopped = worker.stopped.clone();
    let (worker, mut worker_events) = worker.start().await;
    let (boss, _be) = boss.start().await;
    boss.connect(worker.addr()).await.unwrap();
    wait_for_peers(&boss, 1).await;

    let pending = boss
        .send_task(worker.id(), LlmTask::new(Provider::Copilot, "hang"), None)
        .unwrap();
    let task_id = pending.task_id();
    let received = Event::TaskReceived {
        from: "boss".into(),
        task_id,
        provider: Provider::Copilot,
        origin: None,
        team: false,
    };
    next_matching(&mut worker_events, |e| e == &received).await;

    let lost = std::time::Instant::now();
    assert!(boss.disconnect(worker.id()));
    let completed = Event::TaskCompleted {
        from: "boss".into(),
        task_id,
        outcome: Outcome::Cancelled,
        stop: Some("stopped: boss was gone for 3 s".into()),
    };
    next_matching(&mut worker_events, |e| e == &completed).await;
    assert!(lost.elapsed() >= Duration::from_secs(3));
    wait_stopped(&stopped, 1).await;

    boss.connect(worker.addr()).await.unwrap();
    let result = timeout(Duration::from_secs(10), pending.finish(|_| {}))
        .await
        .unwrap()
        .unwrap();
    assert!(!result.ok && !result.refused, "{result:?}");
    assert_eq!(result.output, "task stopped: boss was gone for 3 s");
}

#[tokio::test]
async fn cancelling_a_delegated_task_stops_it_on_the_worker() {
    let mut worker = Node::new("worker").serving(vec![Provider::Copilot]);
    let mut boss = Node::new("boss");
    mutual(&mut worker, &mut boss);
    let audit_dir = tempfile::tempdir().unwrap();
    let audit_path = audit_dir.path().join("audit.jsonl");
    worker.audit = Some(Arc::new(AuditLog::new(audit_path.clone())));
    let stopped = worker.stopped.clone();
    let (worker, mut worker_events) = worker.start().await;
    let (boss, _be) = boss.start().await;
    boss.connect(worker.addr()).await.unwrap();
    wait_for_peers(&boss, 1).await;

    // Fill both task slots with hanging tasks, then cancel them from the requester.
    let request = LlmTask::new(Provider::Copilot, "hang");
    let mut hanging = Vec::new();
    let mut task_ids = Vec::new();
    for _ in 0..2 {
        let delegation = boss.send_task(worker.id(), request.clone(), None).unwrap();
        task_ids.push(delegation.task_id());
        hanging.push(tokio::spawn(delegation.finish(|_| {})));
        next_matching(&mut worker_events, |e| {
            matches!(e, Event::TaskReceived { .. })
        })
        .await;
    }
    for task in &hanging {
        task.abort();
    }
    wait_stopped(&stopped, 2).await;
    for _ in 0..2 {
        let completed = next_matching(&mut worker_events, |e| {
            matches!(e, Event::TaskCompleted { .. })
        })
        .await;
        assert!(
            matches!(
                completed,
                Event::TaskCompleted {
                    outcome: Outcome::Cancelled,
                    ref from,
                    stop: Some(ref stop),
                    task_id: _,
                } if from == "boss" && stop == "cancelled by boss"
            ),
            "{completed:?}"
        );
    }

    // Dropping the waits sent TaskCancel. The worker keeps that result for replays.
    for task_id in task_ids {
        let delegation = boss.rejoin_task(worker.id(), task_id, request.clone(), None);
        let result = timeout(Duration::from_secs(10), delegation.finish(|_| {}))
            .await
            .unwrap()
            .unwrap();
        assert!(!result.ok && !result.refused, "{result:?}");
        assert_eq!(result.output, "task cancelled by boss");
    }

    // The slots are free again: a new task runs right away.
    let out = timeout(
        Duration::from_secs(10),
        boss.delegate(worker.id(), LlmTask::new(Provider::Copilot, "after")),
    )
    .await
    .expect("slots were freed")
    .unwrap();
    assert_eq!(out, "copilot did: after");

    let outcomes: Vec<Outcome> = AuditLog::read(&audit_path, 10)
        .unwrap()
        .iter()
        .map(|e| e.outcome)
        .collect();
    assert_eq!(
        outcomes,
        [Outcome::Cancelled, Outcome::Cancelled, Outcome::Done]
    );
}

#[tokio::test]
async fn worker_cancels_tasks_when_the_requester_disconnects() {
    let mut worker = Node::new("worker").serving(vec![Provider::Copilot]);
    let mut boss = Node::new("boss");
    mutual(&mut worker, &mut boss);
    let stopped = worker.stopped.clone();
    let (worker, mut worker_events) = worker.start().await;
    let (boss, _be) = boss.start().await;
    boss.connect(worker.addr()).await.unwrap();
    wait_for_peers(&boss, 1).await;

    let _pending = tokio::spawn({
        let (boss, id) = (boss.clone(), worker.id());
        async move {
            boss.delegate(id, LlmTask::new(Provider::Copilot, "hang"))
                .await
        }
    });
    next_matching(&mut worker_events, |e| {
        matches!(e, Event::TaskReceived { .. })
    })
    .await;
    boss.shutdown().await;

    wait_stopped(&stopped, 1).await;
    let completed = next_matching(&mut worker_events, |e| {
        matches!(e, Event::TaskCompleted { .. })
    })
    .await;
    assert!(
        matches!(
            completed,
            Event::TaskCompleted {
                outcome: Outcome::Cancelled,
                ..
            }
        ),
        "{completed:?}"
    );
}

#[tokio::test]
async fn peer_tasks_run_in_the_shared_data_folder_by_default() {
    let mut worker = Node::new("worker").serving(vec![Provider::Copilot]);
    let mut boss = Node::new("boss");
    mutual(&mut worker, &mut boss);
    let (worker, _we) = worker.start().await;
    let (boss, _be) = boss.start().await;
    boss.connect(worker.addr()).await.unwrap();
    wait_for_peers(&boss, 1).await;

    let peer = boss.find_peer(Provider::Copilot).unwrap();
    assert_eq!(
        peer.shares,
        [ShareInfo {
            name: DATA_SHARE.into(),
            access: Access::ReadOnly,
            ..Default::default()
        }],
        "peers learn folder names and access, never paths"
    );
    let folder = boss
        .delegate(peer.id, LlmTask::new(Provider::Copilot, "where"))
        .await
        .unwrap();
    assert_eq!(folder, data_folder("worker").display().to_string());
    let access = boss
        .delegate(peer.id, LlmTask::new(Provider::Copilot, "access"))
        .await
        .unwrap();
    assert_eq!(access, "read-only");
}

#[tokio::test]
async fn peers_reach_only_folders_shared_with_them() {
    let dir = tempfile::tempdir().unwrap();
    for name in ["code", "secret"] {
        std::fs::create_dir(dir.path().join(name)).unwrap();
    }
    let mut worker = Node::new("worker").serving(vec![Provider::Copilot]);
    worker
        .shares
        .add("code", &dir.path().join("code"), Access::WorkspaceWrite)
        .unwrap();
    worker
        .shares
        .add("secret", &dir.path().join("secret"), Access::Full)
        .unwrap();
    let mut boss = Node::new("boss");
    let mut other = Node::new("other");
    mutual(&mut worker, &mut boss);
    mutual(&mut worker, &mut other);
    // boss may use data and code, with full access requested; other only data.
    let boss_grant = worker
        .trust
        .peers
        .iter_mut()
        .find(|p| p.name == "boss")
        .unwrap();
    boss_grant.access = Access::Full;
    boss_grant.shares = vec![DATA_SHARE.into(), "code".into()];
    let (worker, _we) = worker.start().await;
    let (boss, _be) = boss.start().await;
    let (other, _oe) = other.start().await;
    boss.connect(worker.addr()).await.unwrap();
    other.connect(worker.addr()).await.unwrap();
    wait_for_peers(&boss, 1).await;
    wait_for_peers(&other, 1).await;

    let seen: Vec<(String, Access)> = boss.peers()[0]
        .shares
        .iter()
        .map(|s| (s.name.clone(), s.access))
        .collect();
    assert_eq!(
        seen,
        [
            (DATA_SHARE.to_owned(), Access::ReadOnly),
            ("code".to_owned(), Access::WorkspaceWrite)
        ],
        "the unshared folder isn't even listed; access is capped per folder"
    );

    let in_code = |prompt: &str| LlmTask::new(Provider::Copilot, prompt).in_share("code");
    let folder = boss.delegate(worker.id(), in_code("where")).await.unwrap();
    assert_eq!(
        std::fs::canonicalize(&folder).unwrap(),
        std::fs::canonicalize(dir.path().join("code")).unwrap()
    );
    let access = boss.delegate(worker.id(), in_code("access")).await.unwrap();
    assert_eq!(
        access, "workspace-write",
        "lower of full grant and folder max"
    );

    for (who, share) in [(&boss, "secret"), (&other, "code"), (&boss, "missing")] {
        let task = LlmTask::new(Provider::Copilot, "where").in_share(share);
        let err = who.delegate(worker.id(), task).await.unwrap_err();
        assert!(err.to_string().contains("has not shared a folder"), "{err}");
    }

    // Narrowing a connected peer's grant reaches it right away.
    let mut grant = worker.trusted().get(&boss.id()).unwrap().clone();
    grant.shares = vec![DATA_SHARE.into()];
    worker.allow(grant);
    timeout(Duration::from_secs(10), async {
        while boss.peers()[0].shares.len() != 1 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("boss saw its narrowed folder list");
    let err = boss
        .delegate(worker.id(), in_code("where"))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("has not shared a folder"), "{err}");
}

#[tokio::test]
async fn peers_use_only_the_mcp_servers_a_folder_offers_them() {
    let dir = tempfile::tempdir().unwrap();
    let mut worker = Node::new("worker").serving(vec![Provider::Copilot, Provider::Codex]);
    worker.shares.add("app", dir.path(), Access::Full).unwrap();
    let mut browser = McpServer::local("npx", vec!["@playwright/mcp".into()]);
    browser.access = Access::ReadOnly;
    browser.env.insert("TOKEN".into(), "s3cret".into());
    worker.shares.add_mcp("app", "browser", browser).unwrap();
    worker
        .shares
        .add_mcp("app", "deploy", McpServer::local("deploy-mcp", vec![]))
        .unwrap();
    let mut boss = Node::new("boss");
    mutual(&mut worker, &mut boss);
    let grant = worker
        .trust
        .peers
        .iter_mut()
        .find(|p| p.name == "boss")
        .unwrap();
    grant.access = Access::WorkspaceWrite;
    grant.shares = vec!["app".into()];
    let (worker, _we) = worker.start().await;
    let (boss, _be) = boss.start().await;
    boss.connect(worker.addr()).await.unwrap();
    wait_for_peers(&boss, 1).await;

    let app = &boss.peers()[0].shares[0];
    assert_eq!(
        app.mcp,
        ["browser"],
        "deploy needs full access; boss has workspace-write"
    );
    let wire = serde_json::to_string(app).unwrap();
    assert!(!wire.contains("npx") && !wire.contains("s3cret"), "{wire}");

    let using = |provider, servers: &[&str]| {
        let mut task = LlmTask::new(provider, "mcp").in_share("app");
        task.mcp = servers.iter().map(|s| (*s).to_owned()).collect();
        task
    };
    let got = boss
        .delegate(worker.id(), using(Provider::Copilot, &["browser"]))
        .await
        .unwrap();
    assert_eq!(
        got, "browser",
        "the task got exactly the server it asked for"
    );
    let none = boss
        .delegate(worker.id(), using(Provider::Copilot, &[]))
        .await
        .unwrap();
    assert_eq!(none, "", "no servers unless asked for");
    for (task, why) in [
        (using(Provider::Copilot, &["deploy"]), "needs full access"),
        (
            using(Provider::Copilot, &["github"]),
            "offers no MCP server named 'github'",
        ),
        (
            using(Provider::Codex, &["browser"]),
            "Codex tasks can't use MCP servers",
        ),
    ] {
        let err = boss.delegate(worker.id(), task).await.unwrap_err();
        assert!(err.to_string().contains(why), "{err}");
    }

    // Its owner pauses it and describes it: peers see that at once, and the worker
    // refuses tasks that ask for a paused server.
    let mut shares = worker.shares();
    shares
        .edit_mcp("app", "browser", |s| {
            s.paused = true;
            s.description = "A headless browser".into();
            Ok(())
        })
        .unwrap();
    worker.set_shares(shares.clone());
    wait_until(|| boss.peers()[0].shares[0].mcp.is_empty()).await;
    let err = boss
        .delegate(worker.id(), using(Provider::Copilot, &["browser"]))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("is paused by its owner"), "{err}");

    // Changes to the shares file (e.g. `proofkin mcp resume` while the agent runs)
    // reach the running agent and its peers.
    let file = dir.path().join("shares.json");
    shares.save(&file).unwrap();
    worker.watch_shares(
        file.clone(),
        dir.path().join("data"),
        Duration::from_millis(50),
    );
    tokio::time::sleep(Duration::from_millis(120)).await;
    shares
        .edit_mcp("app", "browser", |s| {
            s.paused = false;
            Ok(())
        })
        .unwrap();
    shares.save(&file).unwrap();
    wait_until(|| boss.peers()[0].shares[0].mcp == ["browser"]).await;
    assert_eq!(
        boss.peers()[0].shares[0].mcp_about["browser"],
        "A headless browser"
    );
}

/// Waits up to 10 s for `done`.
async fn wait_until(done: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !done() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("in time");
}

#[tokio::test]
async fn peers_see_a_digital_workers_owner_and_role() {
    let mut helper = Node::new("helper").serving(vec![Provider::Copilot]);
    helper.owner = Some("alice".into());
    helper.role = Some("test engineer".into());
    let mut boss = Node::new("boss");
    mutual(&mut helper, &mut boss);
    let (helper, _he) = helper.start().await;
    let (boss, _be) = boss.start().await;
    boss.connect(helper.addr()).await.unwrap();
    wait_for_peers(&boss, 1).await;
    let peer = &boss.peers()[0];
    assert_eq!(peer.owner.as_deref(), Some("alice"));
    assert_eq!(peer.role.as_deref(), Some("test engineer"));
    assert_eq!(
        helper.peers()[0].owner,
        None,
        "boss is a person, not a worker"
    );
}

/// Starts `boss` connected to `workers`, all allowlisted to each other.
async fn boss_with(workers: Vec<Node>) -> (Agent, Vec<Agent>) {
    let mut boss = Node::new("boss");
    let mut workers = workers;
    for w in &mut workers {
        mutual(w, &mut boss);
    }
    let mut started = Vec::new();
    for w in workers {
        started.push(w.start().await.0);
    }
    let (boss, _be) = boss.start().await;
    for w in &started {
        boss.connect(w.addr()).await.unwrap();
    }
    wait_for_peers(&boss, started.len()).await;
    (boss, started)
}

#[tokio::test]
async fn plans_run_dependent_steps_across_squads() {
    let (boss, _workers) = boss_with(vec![
        Node::new("lead").serving(vec![Provider::Copilot]),
        Node::new("qa1").serving(vec![Provider::Copilot]),
        Node::new("qa2").serving(vec![Provider::Copilot]),
    ])
    .await;
    let mut squads = SquadStore::default();
    squads.set("qa", vec!["qa1".into(), "qa2".into()]).unwrap();
    let plan = Plan::parse(
        "api @lead summarize the api\n\
         unit after api #qa write unit tests\n\
         e2e after api #qa write e2e tests\n\
         report after unit,e2e @lead report",
    )
    .unwrap();

    let mut log = Vec::new();
    let states = run_plan(&boss, &squads, &plan, |id, state| {
        log.push((id.to_owned(), state.clone()));
    })
    .await;

    let done = |id: &str| match &states[id] {
        StepState::Done { peer, output, .. } => (peer.clone(), output.clone()),
        other => panic!("{id}: {other:?}"),
    };
    let (api_peer, api_out) = done("api");
    assert_eq!(api_peer, "lead");
    assert_eq!(api_out, "copilot did: summarize the api");
    let (unit_peer, unit_out) = done("unit");
    let (e2e_peer, e2e_out) = done("e2e");
    assert_ne!(
        unit_peer, e2e_peer,
        "the squad spreads parallel steps over its members"
    );
    assert!(
        [unit_peer.as_str(), e2e_peer.as_str()]
            .iter()
            .all(|p| p.starts_with("qa"))
    );
    // Each step got the task and result of the step it depends on, as external
    // content under the rule for it (another peer's model wrote it).
    assert!(
        unit_out.contains(
            "### Step api (done by lead)\n<external_content source=\"step:api peer:lead\">\nIts task:\nsummarize the api\n\nIts result:\ncopilot did: summarize the api\n</external_content>"
        ),
        "{unit_out}"
    );
    assert!(unit_out.starts_with("copilot did: ## Rule for external content"));
    assert!(e2e_out.ends_with("Your task:\nwrite e2e tests"));
    let (_, report_out) = done("report");
    assert!(report_out.contains("### Step unit") && report_out.contains("### Step e2e"));

    // Dependents start only after their prerequisites finished.
    let at = |id: &str, pred: fn(&StepState) -> bool| {
        log.iter()
            .position(|(i, s)| i == id && pred(s))
            .unwrap_or_else(|| panic!("no matching update for {id}"))
    };
    let started = |id: &str| at(id, |s| matches!(s, StepState::Running { .. }));
    let finished = |id: &str| at(id, |s| matches!(s, StepState::Done { .. }));
    assert!(finished("api") < started("unit"));
    assert!(finished("api") < started("e2e"));
    assert!(finished("unit") < started("report"));
    assert!(finished("e2e") < started("report"));
}

#[tokio::test]
async fn plan_steps_after_a_failure_are_skipped() {
    let (boss, _workers) =
        boss_with(vec![Node::new("solo").serving(vec![Provider::Copilot])]).await;
    let plan = Plan::parse(
        "broken @solo fail\n\
         after_broken after broken @solo never runs\n\
         chained after after_broken @solo never runs either\n\
         independent @solo still runs\n\
         away @nobody cannot run",
    )
    .unwrap();
    let states = run_plan(&boss, &SquadStore::default(), &plan, |_, _| {}).await;
    assert!(matches!(&states["broken"], StepState::Failed { error, .. } if error == "model error"));
    assert!(
        matches!(&states["after_broken"], StepState::Skipped { reason } if reason.contains("broken"))
    );
    assert!(matches!(&states["chained"], StepState::Skipped { .. }));
    assert!(matches!(&states["independent"], StepState::Done { .. }));
    assert!(
        matches!(&states["away"], StepState::Failed { peer: None, error, .. } if error.contains("not connected"))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_served_task_runs_once_through_replacement_and_repeated_disconnects() {
    #[derive(Default)]
    struct GatedRun {
        starts: AtomicUsize,
        finish: Notify,
    }

    #[async_trait]
    impl Executor for GatedRun {
        fn providers(&self) -> Vec<Provider> {
            vec![Provider::Copilot]
        }

        async fn execute(&self, _task: &LlmTask, _ctx: &TaskContext) -> anyhow::Result<Ran> {
            self.starts.fetch_add(1, Ordering::SeqCst);
            self.finish.notified().await;
            Ok(Ran::new("finished the original run", None))
        }
    }

    let run = Arc::new(GatedRun::default());
    let mut worker = Node::new("worker");
    worker.executor = Some(run.clone());
    let mut boss = Node::new("boss");
    worker.grace = Duration::from_secs(15);
    boss.grace = worker.grace;
    mutual(&mut worker, &mut boss);
    let (worker, mut worker_events) = worker.start().await;
    let (boss, mut boss_events) = boss.start().await;
    let (low, high) = if boss.id() < worker.id() {
        (&boss, &worker)
    } else {
        (&worker, &boss)
    };
    high.connect(low.addr()).await.unwrap();
    // Wait until the initial handshake's replay pass is done before sending the task.
    wait_until(|| {
        boss.peers()
            .iter()
            .any(|peer| peer.id == worker.id() && peer.load.is_some())
    })
    .await;

    let delegation = boss
        .send_task(worker.id(), LlmTask::new(Provider::Copilot, "wait"), None)
        .unwrap();
    let task_id = delegation.task_id();
    let reports = Arc::new(AtomicUsize::new(0));
    let running = reports.clone();
    let task = tokio::spawn(delegation.finish(move |p| {
        if p.phase == Phase::Running && p.attempt == Some(1) {
            running.fetch_add(1, Ordering::SeqCst);
        }
    }));
    wait_until(|| run.starts.load(Ordering::SeqCst) == 1 && reports.load(Ordering::SeqCst) == 1)
        .await;
    // The second connection replaces the first while its reader is still live.
    // Each replay of the model's progress proves the request reached the worker again.
    low.reconnect(high.id()).await.unwrap();
    wait_until(|| reports.load(Ordering::SeqCst) == 2).await;

    let lost_boss = Event::PeerDisconnected {
        name: "boss".into(),
        id: boss.id(),
    };
    let lost_worker = Event::PeerDisconnected {
        name: "worker".into(),
        id: worker.id(),
    };
    for copies in [3, 4, 5] {
        assert!(boss.disconnect(worker.id()));
        next_matching(&mut worker_events, |e| e == &lost_boss).await;
        next_matching(&mut boss_events, |e| e == &lost_worker).await;
        boss.connect(worker.addr()).await.unwrap();
        wait_until(|| reports.load(Ordering::SeqCst) == copies).await;
    }

    run.finish.notify_one();
    let delivered = timeout(Duration::from_secs(10), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(delivered.ok && delivered.verified, "{delivered:?}");
    assert_eq!(delivered.output, "finished the original run");
    assert_eq!(
        run.starts.load(Ordering::SeqCst),
        1,
        "task #{task_id} ran twice"
    );
}

#[tokio::test]
async fn delegated_tasks_survive_a_connection_being_replaced() {
    let mut worker = Node::new("worker").serving(vec![Provider::Copilot]);
    let mut boss = Node::new("boss");
    mutual(&mut worker, &mut boss);
    let (worker, mut worker_events) = worker.start().await;
    let (boss, _be) = boss.start().await;
    // Connect from the higher id, so a dial from the lower id replaces the connection.
    let (low, high) = if boss.id() < worker.id() {
        (&boss, &worker)
    } else {
        (&worker, &boss)
    };
    high.connect(low.addr()).await.unwrap();
    wait_for_peers(&boss, 1).await;
    wait_for_peers(&worker, 1).await;

    let task = tokio::spawn({
        let (boss, id) = (boss.clone(), worker.id());
        async move {
            boss.delegate(id, LlmTask::new(Provider::Copilot, "slow"))
                .await
        }
    });
    next_matching(&mut worker_events, |e| {
        matches!(e, Event::TaskReceived { .. })
    })
    .await;
    // A second connection wins the tie-break while the task runs; the first closes.
    low.reconnect(high.id()).await.unwrap();

    let out = timeout(Duration::from_secs(10), task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        out.unwrap(),
        "slow done",
        "the result arrives over the new connection"
    );
    let completed = next_matching(&mut worker_events, |e| {
        matches!(e, Event::TaskCompleted { .. })
    })
    .await;
    assert!(
        matches!(
            completed,
            Event::TaskCompleted {
                outcome: Outcome::Done,
                ..
            }
        ),
        "{completed:?}"
    );
    assert_eq!(boss.peers().len(), 1);
}

#[tokio::test]
async fn checks_run_by_the_worker_prove_results_and_failures_are_retried() {
    let dir = tempfile::tempdir().unwrap();
    let mut worker = Node::new("worker").serving(vec![Provider::Copilot]);
    let (mut boss, mut intern) = (Node::new("boss"), Node::new("intern"));
    mutual(&mut worker, &mut boss);
    mutual(&mut worker, &mut intern);
    let checks = parse_checks(&[format!("fixed={}", exists_check("fixed.txt"))]).unwrap();
    worker
        .shares
        .add_with("repo", dir.path(), Access::Full, checks, false)
        .unwrap();
    worker.grant("boss", Access::Full, &["data", "repo"]);
    worker.grant("intern", Access::WorkspaceWrite, &["data", "repo"]);
    let (worker, _we) = worker.start().await;
    let (boss, _be) = boss.start().await;
    let (intern, _ie) = intern.start().await;
    boss.connect(worker.addr()).await.unwrap();
    intern.connect(worker.addr()).await.unwrap();
    wait_for_peers(&boss, 1).await;
    wait_for_peers(&intern, 1).await;
    let repo = boss.peers()[0]
        .shares
        .iter()
        .find(|s| s.name == "repo")
        .cloned();
    assert_eq!(
        repo.map(|s| s.checks),
        Some(vec!["fixed".to_owned()]),
        "peers see check names, never commands"
    );

    let task = LlmTask::new(Provider::Copilot, "fix-on-retry")
        .in_share("repo")
        .with_checks(vec!["fixed".into()], None);
    // One attempt: the check fails, and the signed result says so.
    let first = boss
        .delegate_verified(worker.id(), task.clone())
        .await
        .unwrap();
    assert!(!first.ok && first.verified, "{first:?}");
    let evidence = first.evidence.unwrap();
    assert_eq!(evidence.attempts, 1);
    assert_eq!(evidence.checks[0].exit_code.map(|c| c != 0), Some(true));

    // Two attempts: the retry gets the failure, fixes it and the check passes.
    let task = task.with_checks(vec!["fixed".into()], Some(2));
    let second = boss.delegate_verified(worker.id(), task).await.unwrap();
    assert!(second.ok && second.verified, "{}", second.output);
    let evidence = second.evidence.as_ref().unwrap();
    assert_eq!(evidence.attempts, 2);
    assert!(evidence.checks_passed());
    assert_eq!(evidence.signer_id, worker.id().to_string());
    evidence.verify(&worker.id(), &second.output).unwrap();
    assert!(
        second.output.starts_with(
            "first try\n\nAttempt 2 of 2, after these checks failed: fixed\n\nfixed it"
        ),
        "{}",
        second.output
    );
    assert!(
        second.output.contains("✅ `fixed`: passed"),
        "{}",
        second.output
    );

    let unknown = LlmTask::new(Provider::Copilot, "x")
        .in_share("repo")
        .with_checks(vec!["deploy".into()], None);
    let err = boss.delegate(worker.id(), unknown).await.unwrap_err();
    assert!(
        format!("{err}").contains("no check named 'deploy'"),
        "{err}"
    );
    // Checks run the folder's code with the owner's rights: full access only.
    let task = LlmTask::new(Provider::Copilot, "x")
        .in_share("repo")
        .with_checks(vec!["fixed".into()], None);
    let err = intern.delegate(worker.id(), task).await.unwrap_err();
    assert!(format!("{err}").contains("need full access"), "{err}");
}

#[tokio::test]
async fn built_in_spec_checks_prove_a_spec_package_with_read_only_access() {
    let dir = tempfile::tempdir().unwrap();
    proofkin::spec::init(dir.path(), "Wishlist", false).unwrap();
    let mut worker = Node::new("worker").serving(vec![Provider::Copilot]);
    let mut boss = Node::new("boss");
    mutual(&mut worker, &mut boss);
    let checks = parse_checks(&[format!("fixed={}", exists_check("fixed.txt"))]).unwrap();
    worker
        .shares
        .add_with("app", dir.path(), Access::ReadOnly, checks, false)
        .unwrap();
    worker.grant("boss", Access::ReadOnly, &["app"]);
    let (worker, _we) = worker.start().await;
    let (boss, _be) = boss.start().await;
    boss.connect(worker.addr()).await.unwrap();
    wait_for_peers(&boss, 1).await;

    let all: Vec<String> = proofkin::spec::SPEC_CHECKS
        .iter()
        .map(|c| (*c).to_owned())
        .collect();
    let task = LlmTask::new(Provider::Copilot, "review the spec")
        .in_share("app")
        .with_checks(all, None);
    let proven = boss
        .delegate_verified(worker.id(), task.clone())
        .await
        .unwrap();
    assert!(proven.ok && proven.verified, "{}", proven.output);
    let evidence = proven.evidence.as_ref().unwrap();
    assert_eq!(evidence.checks.len(), 4);
    assert!(evidence.checks_passed());
    evidence.verify(&worker.id(), &proven.output).unwrap();
    assert!(
        proven.output.contains("✅ `spec:trace`: passed"),
        "{}",
        proven.output
    );

    // A criterion nobody claims: the signed evidence says what is wrong.
    let claims = dir.path().join("evidence/claims.json");
    let text = std::fs::read_to_string(&claims).unwrap();
    std::fs::write(&claims, text.replace("\"AC-4\"", "\"AC-40\"")).unwrap();
    let failed = boss.delegate_verified(worker.id(), task).await.unwrap();
    assert!(!failed.ok && failed.verified, "{failed:?}");
    let trace = &failed.evidence.unwrap().checks[3];
    assert!(
        !trace.passed
            && trace
                .output
                .contains("evidence/claims.json has no claim AC-4"),
        "{trace:?}"
    );

    // The folder's own checks still need full access.
    let own = LlmTask::new(Provider::Copilot, "x")
        .in_share("app")
        .with_checks(vec!["fixed".into()], None);
    let err = boss.delegate(worker.id(), own).await.unwrap_err();
    assert!(format!("{err}").contains("need full access"), "{err}");
}

#[tokio::test]
async fn worktree_tasks_commit_on_a_branch_and_another_peer_verifies_the_patch() {
    let dev_repo = git_repo();
    let review_repo = tempfile::tempdir().unwrap();
    git(
        review_repo.path(),
        &["clone", "-q", &dev_repo.path().to_string_lossy(), "."],
    );
    let mut dev = Node::new("dev").serving(vec![Provider::Copilot]);
    let mut reviewer = Node::new("reviewer").serving(vec![Provider::Copilot]);
    let mut boss = Node::new("boss");
    mutual(&mut dev, &mut boss);
    mutual(&mut reviewer, &mut boss);
    dev.shares
        .add_with(
            "app",
            dev_repo.path(),
            Access::Full,
            Default::default(),
            true,
        )
        .unwrap();
    dev.grant("boss", Access::WorkspaceWrite, &["app"]);
    let checks = parse_checks(&[format!("feature={}", exists_check("feature.txt"))]).unwrap();
    reviewer
        .shares
        .add_with("app", review_repo.path(), Access::Full, checks, true)
        .unwrap();
    reviewer.grant("boss", Access::Full, &["app"]);
    let (dev, _de) = dev.start().await;
    let (reviewer, _re) = reviewer.start().await;
    let (boss, _be) = boss.start().await;
    boss.connect(dev.addr()).await.unwrap();
    boss.connect(reviewer.addr()).await.unwrap();
    wait_for_peers(&boss, 2).await;

    let task = LlmTask::new(Provider::Copilot, "write feature.txt").in_share("app");
    let built = boss.delegate_verified(dev.id(), task).await.unwrap();
    assert!(built.ok && built.verified, "{}", built.output);
    let git_record = built.evidence.as_ref().unwrap().git.clone().unwrap();
    assert!(git_record.branch.starts_with("p2p/boss/"), "{git_record:?}");
    assert!(git_record.commit.is_some());
    assert_eq!(git_record.files, vec!["feature.txt"]);
    let patch = built
        .patch
        .clone()
        .expect("the commit comes back as a patch");
    assert!(patch.contains("feature.txt"));
    assert!(
        !dev_repo.path().join("feature.txt").exists(),
        "the task worked on its own branch, not in the shared folder"
    );

    // Another peer applies the patch in its own worktree and checks it independently.
    let mut review = LlmTask::new(Provider::Copilot, "review it")
        .in_share("app")
        .with_checks(vec!["feature".into()], None);
    review.patch = Some(patch);
    let checked = boss.delegate_verified(reviewer.id(), review).await.unwrap();
    assert!(checked.ok && checked.verified, "{}", checked.output);
    let evidence = checked.evidence.unwrap();
    assert!(evidence.checks_passed());
    assert_eq!(evidence.signer_id, reviewer.id().to_string());
    assert!(!review_repo.path().join("feature.txt").exists());
}

#[tokio::test]
async fn a_task_in_a_linked_repository_starts_from_its_latest_commit() {
    // "GitHub" is a bare repository; the worker's folder is its clone, and someone pushed
    // a commit after it was cloned.
    let tmp = tempfile::tempdir().unwrap();
    let origin = tmp.path().join("origin.git").to_string_lossy().into_owned();
    let clone = tmp.path().join("clone");
    let commit = |dir: &std::path::Path, file: &str, text: &str| {
        std::fs::write(dir.join(file), text).unwrap();
        git(dir, &["add", "-A"]);
        git(
            dir,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "-m",
                file,
            ],
        );
    };
    let seed = git_repo();
    git(seed.path(), &["branch", "-q", "-M", "main"]);
    git(
        tmp.path(),
        &["init", "-q", "--bare", "--initial-branch=main", &origin],
    );
    git(seed.path(), &["push", "-q", &origin, "main"]);
    git(
        tmp.path(),
        &["clone", "-q", "--branch", "main", &origin, "clone"],
    );
    commit(seed.path(), "feature.txt", "new on main\n");
    git(seed.path(), &["push", "-q", &origin, "main"]);

    let pulls = Arc::new(FakePulls::default());
    *pulls.linked.lock().unwrap() = Some(proofkin::github::Repo {
        name: "app".into(),
        slug: "acme/app".into(),
        host: "github.com".into(),
        path: clone.clone(),
        branch: "main".into(),
    });
    let mut worker = Node::new("worker").serving(vec![Provider::Copilot]);
    let mut boss = Node::new("boss");
    mutual(&mut worker, &mut boss);
    worker
        .shares
        .add_with(
            "app",
            &clone,
            Access::WorkspaceWrite,
            Default::default(),
            true,
        )
        .unwrap();
    worker.grant("boss", Access::WorkspaceWrite, &["app"]);
    worker.pulls = Some(pulls.clone());
    let (worker, _we) = worker.start().await;
    let (boss, _be) = boss.start().await;
    boss.connect(worker.addr()).await.unwrap();
    wait_for_peers(&boss, 1).await;
    let show = || LlmTask::new(Provider::Copilot, "show feature.txt").in_share("app");

    // The clone is brought up to date first, so the task sees the commit pushed after it
    // was cloned, and its requester is told.
    let mut seen = Vec::new();
    let done = boss
        .send_task(worker.id(), show(), None)
        .unwrap()
        .finish(|p| seen.push(p.text))
        .await
        .unwrap();
    assert!(
        done.ok && done.output.contains("feature.txt says: new on main"),
        "{}",
        done.output
    );
    assert!(!done.output.contains("Note:"), "{}", done.output);
    assert!(
        seen.iter().any(|t| t
            .starts_with("updated folder 'app' to its repository's latest commit ")
            && t.ends_with(" (1 newer commit)")),
        "{seen:?}"
    );
    assert!(
        clone.join("feature.txt").is_file(),
        "the clone itself was fast-forwarded"
    );

    // A clone whose branch has a commit the repository hasn't is left as it is: the task
    // starts from it, and its result says so and why.
    commit(&clone, "local.txt", "mine\n");
    let kept = boss
        .send_task(worker.id(), show(), None)
        .unwrap()
        .finish(|_| {})
        .await
        .unwrap();
    assert!(
        kept.ok
            && kept.output.contains("feature.txt says: new on main")
            && kept.output.contains(
                "as it was, not brought up to date with its repository: its main has 1 commit the repository's hasn't."
            ),
        "{}",
        kept.output
    );
}

#[tokio::test]
async fn a_model_run_that_stops_is_a_failed_attempt_and_keeps_its_work() {
    let repo = git_repo();
    let mut worker = Node::new("worker").serving(vec![Provider::Copilot]);
    let mut boss = Node::new("boss");
    mutual(&mut worker, &mut boss);
    worker
        .shares
        .add_with(
            "app",
            repo.path(),
            Access::WorkspaceWrite,
            Default::default(),
            true,
        )
        .unwrap();
    worker.grant("boss", Access::WorkspaceWrite, &["app"]);
    let (worker, _we) = worker.start().await;
    let (boss, _be) = boss.start().await;
    boss.connect(worker.addr()).await.unwrap();
    wait_for_peers(&boss, 1).await;
    let task = |attempts| {
        LlmTask::new(Provider::Copilot, "Build it stops-once")
            .in_share("app")
            .with_checks(Vec::new(), Some(attempts))
    };

    // With an attempt left, the next one goes on from what the stopped run changed.
    let mut seen = Vec::new();
    let done = boss
        .send_task(worker.id(), task(2), None)
        .unwrap()
        .finish(|p| seen.push(p.text))
        .await
        .unwrap();
    assert!(
        done.ok
            && done.output.starts_with(
                "The model's run stopped before it finished: Copilot task failed: timed out after 1200s\n\n\
                 Attempt 2 of 2, after the model's run stopped\n\n\
                 finished it; the last run had left \"started\\n\""
            ),
        "{}",
        done.output
    );
    assert!(
        seen.iter().any(|t| t
            == "attempt 2 of 2: the model is working, going on from where its last run stopped"),
        "{seen:?}"
    );
    let evidence = done.evidence.unwrap();
    assert_eq!(evidence.attempts, 2);
    assert_eq!(evidence.git.unwrap().files, ["finished.txt", "started.txt"]);

    // When its last attempt's run stops, the task fails, and what it changed stays on the
    // task's branch.
    let failed = boss
        .send_task(worker.id(), task(1), None)
        .unwrap()
        .finish(|_| {})
        .await
        .unwrap();
    let (why, kept) = failed
        .output
        .split_once(" (what it changed is on branch ")
        .unwrap_or_else(|| panic!("{}", failed.output));
    assert!(!failed.ok);
    assert_eq!(why, "Copilot task failed: timed out after 1200s");
    let branch = kept.split_once(", commit ").unwrap().0;
    assert!(branch.starts_with("p2p/boss/"), "{branch}");
    let shown = std::process::Command::new("git")
        .arg("-C")
        .arg(repo.path())
        .args(["show", &format!("{branch}:started.txt")])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&shown.stdout), "started\n");
}

/// A call [`FakePulls`] answered: the folder, the ask as it came (with the sign-offs that
/// checked out), the issues, and who asked and how.
type OpenCall = (String, PullRequestAsk, Vec<String>, Asked);

/// Pull requests kept in memory: each one opened is open until the test merges or closes
/// it, and opening the same work again finds it.
#[derive(Default)]
struct FakePulls {
    calls: Mutex<Vec<OpenCall>>,
    /// The work of each one opened (its branch and commit), by number (from 1).
    opened: Mutex<Vec<(String, String)>>,
    /// Each one's state, by number.
    states: Mutex<std::collections::BTreeMap<u64, String>>,
    /// The reviewers asked for: the pull request's number and the logins.
    reviews: Mutex<Vec<(u64, Vec<String>)>>,
    /// The folder that is a linked repository's clone: tasks there bring it up to date
    /// first, as with a real GitHub account (here a local "GitHub").
    linked: Mutex<Option<proofkin::github::Repo>>,
}

#[async_trait]
impl PullRequests for FakePulls {
    async fn open(
        &self,
        share: &str,
        ask: &PullRequestAsk,
        issues: &[String],
        asked: &Asked,
    ) -> anyhow::Result<Opened> {
        self.calls.lock().unwrap().push((
            share.into(),
            ask.clone(),
            issues.to_vec(),
            asked.clone(),
        ));
        let mut opened = self.opened.lock().unwrap();
        let mut states = self.states.lock().unwrap();
        let work = (ask.branch.clone(), ask.commit.clone());
        let (number, existed) = match opened.iter().position(|w| *w == work) {
            Some(i) => (i as u64 + 1, true),
            None => {
                opened.push(work);
                states.insert(opened.len() as u64, "OPEN".into());
                (opened.len() as u64, false)
            }
        };
        Ok(Opened {
            number,
            url: format!("https://github.com/acme/app/pull/{number}"),
            state: states[&number].clone(),
            existed,
            proof: true,
        })
    }

    async fn find(&self, _share: &str, ask: &PullRequestAsk) -> anyhow::Result<Option<Opened>> {
        let opened = self.opened.lock().unwrap();
        let work = (ask.branch.clone(), ask.commit.clone());
        Ok(opened.iter().position(|w| *w == work).map(|i| {
            let number = i as u64 + 1;
            Opened {
                number,
                url: format!("https://github.com/acme/app/pull/{number}"),
                state: self.states.lock().unwrap()[&number].clone(),
                existed: true,
                proof: false,
            }
        }))
    }

    async fn state(&self, _share: &str, number: u64) -> anyhow::Result<String> {
        Ok(self.states.lock().unwrap()[&number].clone())
    }

    async fn request_reviews(&self, _share: &str, number: u64, logins: &[String]) -> Vec<Review> {
        self.reviews.lock().unwrap().push((number, logins.to_vec()));
        // As GitHub does, it can't ask an account that isn't the repository's collaborator.
        logins
            .iter()
            .map(|l| match l.as_str() {
                "stranger" => Review::not_asked(l, "not a collaborator of acme/app"),
                _ => Review::asked(l),
            })
            .collect()
    }

    fn poll(&self) -> Duration {
        Duration::from_millis(50)
    }

    async fn update(&self, share: &str) -> anyhow::Result<proofkin::github::CloneUpdate> {
        let linked = self.linked.lock().unwrap().clone();
        match linked {
            Some(repo) if repo.name == share => {
                let creds = proofkin::github::GithubCredentials {
                    user: "worker-gh".into(),
                    host: "github.com".into(),
                    token: "t0k3n".into(),
                };
                creds.update_clone(&repo, proofkin::github::FETCH_AGE).await
            }
            _ => Ok(proofkin::github::CloneUpdate::NotLinked),
        }
    }
}

impl FakePulls {
    /// Merges or closes a pull request only once the caller sees that it is waiting.
    fn finish_when_waiting(&self, number: u64, state: &str, progress: &Progress) {
        if progress.phase == Phase::PullRequest
            && progress
                .text
                .starts_with(&format!("pull request #{number} is open"))
        {
            *self
                .states
                .lock()
                .unwrap()
                .get_mut(&number)
                .expect("the pull request was opened") = state.into();
        }
    }
}

#[tokio::test]
async fn a_pull_request_step_opens_the_work_before_it_and_waits_until_it_is_merged() {
    let repo = git_repo();
    let pulls = Arc::new(FakePulls::default());
    let mut worker = Node::new("worker").serving(vec![Provider::Copilot]);
    let (mut boss, mut eve) = (Node::new("boss"), Node::new("eve"));
    mutual(&mut worker, &mut boss);
    mutual(&mut worker, &mut eve);
    worker
        .shares
        .add_with(
            "app",
            repo.path(),
            Access::WorkspaceWrite,
            Default::default(),
            true,
        )
        .unwrap();
    worker.grant("boss", Access::WorkspaceWrite, &["app"]);
    worker.grant("eve", Access::WorkspaceWrite, &["app"]);
    worker.pulls = Some(pulls.clone());
    let mut allowed = worker.shares.clone();
    allowed.set_pull_requests("app", true, false).unwrap();
    let (worker, _we) = worker.start().await;
    let (boss, _be) = boss.start().await;
    let (eve, _ee) = eve.start().await;
    boss.connect(worker.addr()).await.unwrap();
    eve.connect(worker.addr()).await.unwrap();
    wait_for_peers(&boss, 1).await;
    wait_for_peers(&eve, 1).await;
    let steps = "impl @worker:app create feature.txt\n\
         check after impl @worker:app apply=impl Check the feature\n\
         ok after check approve @me Ship it?\n";
    let plan = Plan::parse(&format!("{steps}ship after ok pr @worker:app")).unwrap();
    let squads = SquadStore::default();
    // You sign off on each run's work as it comes.
    let signer = {
        let boss = boss.clone();
        tokio::spawn(async move {
            loop {
                for a in boss.approvals() {
                    let _ = boss.answer_approval(a.id, true, "Looks good").await;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
    };

    // Its owner doesn't let peers open pull requests there yet: the step isn't sent.
    let states = timeout(
        Duration::from_secs(60),
        run_plan(&boss, &squads, &plan, |_, _| {}),
    )
    .await
    .unwrap();
    assert!(
        matches!(states["check"], StepState::Done { .. }),
        "{states:?}"
    );
    assert!(
        matches!(&states["ship"], StepState::Failed { error, .. } if error.contains("doesn't take pull request steps")),
        "{:?}",
        states["ship"]
    );
    assert!(pulls.calls.lock().unwrap().is_empty());

    // Allowed: it opens the pull request from check's branch (it has impl's work too),
    // named after the work, asks the reviewers the step names (you are boss-gh on GitHub)
    // and waits until it is merged.
    worker.set_shares(allowed);
    timeout(Duration::from_secs(10), async {
        while !boss.peers()[0].shares.iter().any(|s| s.pull_requests) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("boss learns the folder takes pull request steps");
    let mut said = Vec::new();
    let recipients = proofkin::plan::Recipients {
        github: Some("boss-gh".into()),
        ..Default::default()
    };
    let ctx = RunContext {
        run_id: Some("run-2".into()),
        recipients: recipients.clone(),
        ..Default::default()
    };
    let named = Plan::parse(&format!(
        "{steps}ship after ok pr @worker:app reviewers=me,carol-gh,stranger"
    ))
    .unwrap();
    let states = {
        let run = timeout(
            Duration::from_secs(60),
            run_plan_with(&boss, &squads, &named, &ctx, |id, state| {
                if let (
                    "ship",
                    StepState::Running {
                        progress: Some(p), ..
                    },
                ) = (id, state)
                {
                    said.push(p.text.clone());
                    pulls.finish_when_waiting(1, "MERGED", p);
                }
            }),
        )
        .await;
        run.unwrap_or_else(|_| panic!("the plan did not finish; ship progress: {said:?}"))
    };
    let StepState::Done {
        evidence: Some(checked),
        ..
    } = &states["check"]
    else {
        panic!("{:?}", states["check"]);
    };
    let branch = checked.git.clone().unwrap();
    let (share, ask, issues, asked) = pulls.calls.lock().unwrap()[0].clone();
    assert_eq!(
        (
            share.as_str(),
            asked.by.as_str(),
            asked.run.as_deref(),
            asked.step.as_deref(),
            asked.approval
        ),
        ("app", "boss", Some("run-2"), Some("ship"), None)
    );
    assert!(issues.is_empty(), "{issues:?}");
    assert_eq!(
        (Some(ask.branch.clone()), Some(ask.commit.clone())),
        (Some(branch.branch.clone()), branch.commit.clone())
    );
    assert_eq!(ask.title, "create feature.txt");
    // Its sign-off goes with it, as you signed it.
    assert_eq!(
        ask.signoffs
            .iter()
            .map(|s| (
                s.step.as_str(),
                s.output.as_str(),
                s.evidence.signer.as_str()
            ))
            .collect::<Vec<_>>(),
        [("ok", "Approved by boss: Looks good", "boss")]
    );
    assert_eq!(ask.reviewers, ["boss-gh", "carol-gh", "stranger"]);
    let shown = std::process::Command::new("git")
        .arg("-C")
        .arg(repo.path())
        .args(["show", &format!("{}:feature.txt", ask.branch)])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&shown.stdout),
        "made by the model\n"
    );
    match &states["ship"] {
        StepState::Done {
            output,
            evidence: Some(e),
            verified,
            ..
        } => {
            assert!(
                output.starts_with(
                    "Pull request #1 was merged: https://github.com/acme/app/pull/1\n"
                ) && output.contains("with its signed proof and 1 sign-off.")
                    && output.ends_with(
                        " Asked to review: boss-gh, carol-gh. Not asked to review: stranger (not a collaborator of acme/app)."
                    ),
                "{output}"
            );
            assert!(
                *verified && e.signer == "worker" && e.git.is_none(),
                "{e:?}"
            );
        }
        other => panic!("{other:?}"),
    }
    assert!(
        said.iter().any(|t| t.contains(
            "pull request #1 is open, with a review asked of boss-gh, carol-gh: waiting for it to be merged"
        )),
        "{said:?}"
    );

    // A pull request closed without being merged fails the step. Naming no one, it asks
    // those who signed off: you.
    let ctx = RunContext {
        recipients,
        ..Default::default()
    };
    let states = timeout(
        Duration::from_secs(60),
        run_plan_with(&boss, &squads, &plan, &ctx, |id, state| {
            if id == "ship"
                && let StepState::Running {
                    peer: _,
                    progress: Some(p),
                } = state
            {
                pulls.finish_when_waiting(2, "CLOSED", p);
            }
        }),
    )
    .await
    .unwrap();
    assert!(
        matches!(&states["ship"], StepState::Failed { error, .. } if error.contains("#2 was closed without being merged: https://github.com/acme/app/pull/2")),
        "{:?}",
        states["ship"]
    );
    assert_eq!(
        *pulls.reviews.lock().unwrap(),
        [
            (
                1,
                vec!["boss-gh".into(), "carol-gh".into(), "stranger".into()]
            ),
            (2, vec!["boss-gh".to_owned()])
        ]
    );

    // Only the peer the work was done for opens it, and only as it was done.
    let mut task = LlmTask::new(Provider::Copilot, "open it").in_share("app");
    task.pull_request = Some(Box::new(ask.clone()));
    let refused = eve
        .delegate_verified(worker.id(), task.clone())
        .await
        .unwrap();
    assert!(
        !refused.ok && refused.output.contains("was done for another peer"),
        "{}",
        refused.output
    );
    task.pull_request = Some(Box::new(PullRequestAsk {
        commit: "0".repeat(40),
        ..ask.clone()
    }));
    let refused = boss.delegate_verified(worker.id(), task).await.unwrap();
    assert!(
        !refused.ok && refused.output.contains("moved on from the step's commit"),
        "{}",
        refused.output
    );

    // A sign-off whose record doesn't check out stays off it. (Sent again as the step,
    // as a run that goes on after a restart sends it.)
    let signed = ask.signoffs[0].clone();
    let forged = proofkin::pulls::SignOff {
        output: "Approved by boss: anything".into(),
        ..signed.clone()
    };
    let mut again = LlmTask::new(Provider::Copilot, "open it").in_share("app");
    again.pull_request = Some(Box::new(PullRequestAsk {
        signoffs: vec![signed.clone(), forged],
        ..ask.clone()
    }));
    let step = proofkin::protocol::Origin {
        run: Some("run-2".into()),
        step: Some("ship".into()),
        task: None,
    };
    let reopened = boss
        .send_task(worker.id(), again, Some(step))
        .unwrap()
        .finish(|_| {})
        .await
        .unwrap();
    assert!(
        reopened.ok
            && reopened
                .output
                .contains("1 sign-off left out: its record doesn't check out; an earlier try of this step opened it."),
        "{}",
        reopened.output
    );
    let call = pulls.calls.lock().unwrap().last().unwrap().1.clone();
    assert_eq!(call.signoffs, [signed]);
    assert_eq!(pulls.opened.lock().unwrap().len(), 2);
    // An earlier try opened it: its reviewers aren't asked again.
    assert_eq!(pulls.reviews.lock().unwrap().len(), 2);
    signer.abort();
}

#[tokio::test]
async fn a_folder_owner_approves_each_pull_request_and_a_run_card_asks_for_one() {
    let repo = git_repo();
    let pulls = Arc::new(FakePulls::default());
    let mut worker = Node::new("worker").serving(vec![Provider::Copilot]);
    let mut boss = Node::new("boss");
    mutual(&mut worker, &mut boss);
    worker
        .shares
        .add_with(
            "app",
            repo.path(),
            Access::WorkspaceWrite,
            Default::default(),
            true,
        )
        .unwrap();
    worker.shares.set_pull_requests("app", true, true).unwrap();
    worker.grant("boss", Access::WorkspaceWrite, &["app"]);
    worker.pulls = Some(pulls.clone());
    let (worker, _we) = worker.start().await;
    let (boss, _be) = boss.start().await;
    boss.connect(worker.addr()).await.unwrap();
    wait_for_peers(&boss, 1).await;
    assert!(
        boss.peers()[0]
            .shares
            .iter()
            .any(|s| s.pull_requests && s.pull_request_approval),
        "boss learns the folder's owner approves each one"
    );
    // The folder's owner answers the next request it gets.
    let answer = |approve: bool, note: &'static str| {
        let worker = worker.clone();
        tokio::spawn(async move {
            let request = timeout(Duration::from_secs(30), async {
                loop {
                    if let Some(a) = worker.approvals().pop() {
                        return a;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .expect("the folder's owner is asked");
            worker
                .answer_approval(request.id, approve, note)
                .await
                .unwrap();
            request
        })
    };

    // Not approved: nothing is pushed or opened, and the step says why.
    let text = "impl @worker:app create feature.txt\nship after impl pr @worker:app";
    let plan = Plan::parse(text).unwrap();
    let no = answer(false, "not yet");
    let ctx = RunContext {
        run_id: Some("run-1".into()),
        ..Default::default()
    };
    let states = timeout(
        Duration::from_secs(60),
        run_plan_with(&boss, &SquadStore::default(), &plan, &ctx, |_, _| {}),
    )
    .await
    .unwrap();
    let request = no.await.unwrap();
    assert_eq!(request.title, "Open a pull request in app for boss?");
    assert!(
        request
            .text
            .starts_with("boss asks you to open a pull request in 'app' with your GitHub account")
            && request.text.contains("- Branch: `p2p/boss/"),
        "{}",
        request.text
    );
    assert!(
        matches!(&states["ship"], StepState::Failed { error, .. } if error.contains("worker didn't approve opening the pull request: not yet")),
        "{:?}",
        states["ship"]
    );
    assert!(pulls.calls.lock().unwrap().is_empty());

    // From the run's card: the work of step impl, which no pull request has yet.
    let record = RunRecord::new("boss", &boss.id().to_string(), text, &plan, &states, 0);
    let work = proofkin::plan::run_work(&record);
    assert_eq!(
        work.iter()
            .map(|w| (
                w.step.as_str(),
                w.peer.as_str(),
                w.folder.as_str(),
                w.title.as_str()
            ))
            .collect::<Vec<_>>(),
        [("impl", "worker", "app", "create feature.txt")]
    );
    let (_, ask) = proofkin::plan::run_pull_request(
        &record,
        "impl",
        Some("Add the feature"),
        None,
        &boss.id(),
        &proofkin::plan::Recipients::default(),
    )
    .unwrap();
    let mut task = LlmTask::new(Provider::Copilot, "open it").in_share("app");
    task.pull_request = Some(Box::new(ask));
    let origin = proofkin::protocol::Origin {
        run: Some("run-1".into()),
        task: Some(1),
        step: None,
    };
    let yes = answer(true, "fine");
    let done = timeout(
        Duration::from_secs(60),
        boss.send_task(worker.id(), task.clone(), Some(origin.clone()))
            .unwrap()
            .finish(|p| pulls.finish_when_waiting(1, "MERGED", &p)),
    )
    .await
    .unwrap()
    .unwrap();
    yes.await.unwrap();
    assert!(
        done.ok && done.output.contains("; worker approved opening it."),
        "{}",
        done.output
    );
    let (_, opened, _, asked) = pulls.calls.lock().unwrap()[0].clone();
    assert_eq!(opened.title, "Add the feature");
    assert_eq!(
        (asked.run.as_deref(), asked.step, asked.approval),
        (
            Some("run-1"),
            None,
            Some(proofkin::pulls::OwnerApproval {
                by: "worker".into(),
                note: "fine".into()
            })
        )
    );

    // Asked again (say after a restart), it follows the one it opened: no new request.
    let again = timeout(
        Duration::from_secs(60),
        boss.send_task(worker.id(), task, Some(origin))
            .unwrap()
            .finish(|_| {}),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        again.ok
            && again
                .output
                .contains("it was opened before, from the same work"),
        "{}",
        again.output
    );
    assert!(worker.approvals().is_empty());
    assert_eq!(pulls.opened.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_sign_off_asks_for_changes_and_the_work_is_done_again_from_where_it_was() {
    use proofkin::agent::Answer;
    let repo = git_repo();
    let mut worker = Node::new("worker").serving(vec![Provider::Copilot]);
    let mut boss = Node::new("boss");
    mutual(&mut worker, &mut boss);
    worker
        .shares
        .add_with(
            "app",
            repo.path(),
            Access::WorkspaceWrite,
            Default::default(),
            true,
        )
        .unwrap();
    worker.grant("boss", Access::WorkspaceWrite, &["app"]);
    let (worker, _we) = worker.start().await;
    let (boss, _be) = boss.start().await;
    boss.connect(worker.addr()).await.unwrap();
    wait_for_peers(&boss, 1).await;
    let plan = Plan::parse(
        "impl @worker:app create feature.txt\n\
         ok after impl approve @me changes=1 Ship it?",
    )
    .unwrap();
    // You ask for changes; asked again, with no rounds left, you may only approve or reject.
    let answers = {
        let boss = boss.clone();
        tokio::spawn(async move {
            let mut seen: Vec<proofkin::agent::ApprovalInfo> = Vec::new();
            while seen.len() < 2 {
                for a in boss.approvals() {
                    if a.changes {
                        assert!(boss.answer(a.id, Answer::Changes, "  ").await.is_err());
                        boss.answer(a.id, Answer::Changes, "Add a changelog line")
                            .await
                            .unwrap();
                    } else {
                        let refused = boss.answer(a.id, Answer::Changes, "More").await;
                        assert!(
                            refused
                                .unwrap_err()
                                .to_string()
                                .contains("can only be approved or rejected")
                        );
                        boss.answer(a.id, Answer::Approve, "Good now")
                            .await
                            .unwrap();
                    }
                    seen.push(a);
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            seen
        })
    };
    let said = Arc::new(Mutex::new(Vec::<String>::new()));
    let states = {
        let said = said.clone();
        timeout(
            Duration::from_secs(60),
            run_plan_with(
                &boss,
                &SquadStore::default(),
                &plan,
                &RunContext::default(),
                move |id, state| {
                    if let StepState::Running {
                        progress: Some(p), ..
                    } = state
                    {
                        said.lock().unwrap().push(format!("{id}: {}", p.text));
                    }
                },
            ),
        )
        .await
        .unwrap()
    };
    let seen = timeout(Duration::from_secs(10), answers)
        .await
        .unwrap()
        .unwrap();
    assert!(seen[0].changes && !seen[1].changes, "{seen:?}");
    assert!(
        seen[1]
            .text
            .starts_with("**Asked again** (round 1 of changes)")
            && seen[1].text.contains("> Add a changelog line"),
        "{}",
        seen[1].text
    );
    // impl was done again from its earlier work, with your note, as its card said.
    let StepState::Done {
        output,
        evidence: Some(e),
        ..
    } = &states["impl"]
    else {
        panic!("{:?}", states["impl"]);
    };
    assert!(
        output.starts_with("changed it; its earlier work said \"made by the model\\n\"\n"),
        "{output}"
    );
    let git = e.git.as_ref().unwrap();
    assert_eq!(git.files, ["changes.txt", "feature.txt"]);
    // Each card said what was going on.
    let said = said.lock().unwrap().clone();
    for text in [
        "ok: boss asked for changes (round 1 of 1): Add a changelog line; asks again once impl is done again",
        "impl: done again: boss asked for changes at ok: Add a changelog line",
    ] {
        assert!(said.contains(&text.to_owned()), "{text}: {said:?}");
    }
    assert!(
        matches!(&states["ok"], StepState::Done { output, .. } if output == "Approved by boss: Good now"),
        "{:?}",
        states["ok"]
    );
}

#[tokio::test]
async fn a_step_starts_from_an_earlier_runs_unmerged_commit() {
    let repo = git_repo();
    let mut worker = Node::new("worker").serving(vec![Provider::Copilot]);
    let mut boss = Node::new("boss");
    mutual(&mut worker, &mut boss);
    worker
        .shares
        .add_with(
            "app",
            repo.path(),
            Access::WorkspaceWrite,
            Default::default(),
            true,
        )
        .unwrap();
    worker.grant("boss", Access::WorkspaceWrite, &["app"]);
    let (worker, _we) = worker.start().await;
    let (boss, _be) = boss.start().await;
    boss.connect(worker.addr()).await.unwrap();
    wait_for_peers(&boss, 1).await;
    let squads = SquadStore::default();

    // A run whose work isn't merged: its commit is on a task branch only.
    let text = "impl @worker:app create feature.txt";
    let first = Plan::parse(text).unwrap();
    let states = timeout(
        Duration::from_secs(60),
        run_plan(&boss, &squads, &first, |_, _| {}),
    )
    .await
    .unwrap();
    let runs = tempfile::tempdir().unwrap();
    let mut record = RunRecord::new("boss", &boss.id().to_string(), text, &first, &states, 0);
    record.id = "run-1".into();
    record
        .peer_ids
        .insert("worker".into(), worker.id().to_string());
    record.save(runs.path()).unwrap();
    assert!(!repo.path().join("feature.txt").exists(), "it isn't merged");

    // A new plan's step starts from it, and its report says so.
    let recipients = proofkin::plan::Recipients {
        runs_dir: Some(runs.path().to_path_buf()),
        ..Default::default()
    };
    let ctx = RunContext {
        recipients: recipients.clone(),
        ..Default::default()
    };
    let text = "next @worker:app base=run-1:impl show feature.txt";
    let next = Plan::parse(text).unwrap();
    let states = timeout(
        Duration::from_secs(60),
        run_plan_with(&boss, &squads, &next, &ctx, |_, _| {}),
    )
    .await
    .unwrap();
    assert!(
        matches!(&states["next"], StepState::Done { output, .. } if output.starts_with("feature.txt says: made by the model")),
        "{:?}",
        states["next"]
    );
    let report = RunRecord::new("boss", "id", text, &next, &states, 0).render_markdown();
    assert!(
        report.contains("- Started from the commit of step impl of evidence run run-1\n"),
        "{report}"
    );

    // Work that isn't there fails the step, and the plan check says so first.
    let missing = Plan::parse("next @worker:app base=run-1:nope show feature.txt").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let problems = proofkin::plan::preflight(
        &missing,
        &boss.peers(),
        &squads,
        &proofkin::knowledge::KnowledgeBase::new(dir.path().join("kb")),
        &proofkin::skills::Skills::new(dir.path().join("skills")),
        &recipients,
    );
    assert!(
        problems.iter().any(|p| p.error
            && p.message == "base=run-1:nope: evidence run run-1 has no step nope"),
        "{problems:?}"
    );
    let states = timeout(
        Duration::from_secs(60),
        run_plan_with(&boss, &squads, &missing, &ctx, |_, _| {}),
    )
    .await
    .unwrap();
    assert!(
        matches!(&states["next"], StepState::Failed { error, .. } if error == "base=run-1:nope: evidence run run-1 has no step nope"),
        "{:?}",
        states["next"]
    );
}

#[tokio::test]
async fn a_tasks_model_runs_its_checks_while_it_works() {
    let repo = git_repo();
    let mut worker = Node::new("worker").serving(vec![Provider::Copilot, Provider::Codex]);
    let mut boss = Node::new("boss");
    mutual(&mut worker, &mut boss);
    worker
        .shares
        .add_with(
            "app",
            repo.path(),
            Access::Full,
            [("test".to_owned(), "echo checked here".to_owned())].into(),
            true,
        )
        .unwrap();
    worker.grant("boss", Access::Full, &["app"]);
    let (worker, _we) = worker.start().await;
    let (boss, _be) = boss.start().await;
    boss.connect(worker.addr()).await.unwrap();
    wait_for_peers(&boss, 1).await;

    // Asked for the folder's check, its model gets a tool that runs it in its workspace;
    // the runtime still runs it after the model, and only that run is evidence.
    let task = |provider| {
        LlmTask::new(provider, "use the checks tool")
            .in_share("app")
            .with_checks(vec!["test".into()], None)
    };
    let done = boss
        .delegate_verified(worker.id(), task(Provider::Copilot))
        .await
        .unwrap();
    assert!(done.ok && done.verified, "{}", done.output);
    assert!(
        done.output.starts_with("check test passed (exit code 0, ")
            && done.output.contains("; 9 of 10 runs left\n\nchecked here"),
        "{}",
        done.output
    );
    let evidence = done.evidence.unwrap();
    assert_eq!(evidence.checks.len(), 1);
    assert!(evidence.checks[0].passed);
    // Codex takes no MCP servers: its model works without the tool.
    let codex = boss
        .delegate_verified(worker.id(), task(Provider::Codex))
        .await
        .unwrap();
    assert!(
        codex.output.starts_with("no checks tool"),
        "{}",
        codex.output
    );
    // Without checks, there is nothing to run.
    let plain = boss
        .delegate(
            worker.id(),
            LlmTask::new(Provider::Copilot, "use the checks tool").in_share("app"),
        )
        .await
        .unwrap();
    assert!(plain.starts_with("no checks tool"), "{plain}");
}

#[tokio::test]
async fn approvers_preview_the_prototype_and_review_pack_earlier_steps_created() {
    let repo = git_repo();
    let mut worker = Node::new("worker").serving(vec![Provider::Copilot]);
    let (mut boss, mut alice) = (Node::new("boss"), Node::new("alice"));
    mutual(&mut worker, &mut boss);
    alice.trusts(&boss, None);
    boss.trusts(&alice, None);
    worker
        .shares
        .add_with(
            "repo",
            repo.path(),
            Access::WorkspaceWrite,
            Default::default(),
            true,
        )
        .unwrap();
    worker.grant("boss", Access::WorkspaceWrite, &["repo"]);
    let (worker, _we) = worker.start().await;
    let (boss, _be) = boss.start().await;
    let (alice, mut alice_events) = alice.start().await;
    boss.connect(worker.addr()).await.unwrap();
    boss.connect(alice.addr()).await.unwrap();
    wait_for_peers(&boss, 2).await;

    let plan = Plan::parse(
        "ux @worker:repo create prototype/index.html\n\
         pack after ux @worker:repo apply=ux create review/leadership.md\n\
         product-ok after ux approve @alice Accept the prototype\n\
         lt-ok after pack,product-ok approve @alice Accept the plan",
    )
    .unwrap();
    let reviewer = {
        let alice = alice.clone();
        tokio::spawn(async move {
            for _ in 0..2 {
                let Event::ApprovalRequested { id, title, .. } =
                    next_matching(&mut alice_events, |e| {
                        matches!(e, Event::ApprovalRequested { .. })
                    })
                    .await
                else {
                    unreachable!()
                };
                let request = alice.approvals().into_iter().find(|a| a.id == id).unwrap();
                let site = alice.approval_preview(id).unwrap();
                if title.ends_with("product-ok") {
                    assert_eq!(request.preview, Some("prototype"), "{request:?}");
                    assert_eq!(site["index.html"], b"made by the model\n");
                } else {
                    // Through product-ok too: everything the earlier steps created.
                    assert_eq!(request.preview, Some("review pack"), "{request:?}");
                    let index = String::from_utf8_lossy(&site["index.html"]).into_owned();
                    assert!(
                        index.contains("href=\"review/leadership.html\"")
                            && index.contains("href=\"prototype/index.html\""),
                        "{index}"
                    );
                    assert!(site.contains_key("review/leadership.html"));
                }
                alice
                    .answer_approval(id, true, "clicked through it")
                    .await
                    .unwrap();
                assert!(alice.approval_preview(id).is_none(), "gone once answered");
            }
        })
    };
    let states = timeout(
        Duration::from_secs(60),
        run_plan(&boss, &SquadStore::default(), &plan, |_, _| {}),
    )
    .await
    .expect("the plan finished");
    reviewer.await.unwrap();
    for step in ["product-ok", "lt-ok"] {
        assert!(
            matches!(&states[step], StepState::Done { output, .. } if output.starts_with("Approved by alice")),
            "{step}: {:?}",
            states[step]
        );
    }
}

#[tokio::test]
async fn plans_prove_claims_with_checks_and_a_team_members_approval() {
    let dir = tempfile::tempdir().unwrap();
    let mut worker = Node::new("worker").serving(vec![Provider::Copilot]);
    let (mut boss, mut alice) = (Node::new("boss"), Node::new("alice"));
    mutual(&mut worker, &mut boss);
    // Alice only approves: she needs no providers and boss may not delegate to her.
    alice.trusts(&boss, None);
    boss.trusts(&alice, None);
    let checks = parse_checks(&["ok=exit 0".to_owned()]).unwrap();
    worker
        .shares
        .add_with("repo", dir.path(), Access::Full, checks, false)
        .unwrap();
    worker.grant("boss", Access::Full, &["repo"]);
    let (worker, _we) = worker.start().await;
    let (boss, _be) = boss.start().await;
    let (alice, mut alice_events) = alice.start().await;
    boss.connect(worker.addr()).await.unwrap();
    boss.connect(alice.addr()).await.unwrap();
    wait_for_peers(&boss, 2).await;

    let text = "claim: C-1 the build passes its checks\n\
                claim: C-2 a reviewer signed it off\n\
                impl @worker:repo verify=ok claims=C-1 build it\n\
                signoff after impl approve @alice claims=C-2 Please approve the build";
    let plan = Plan::parse(text).unwrap();
    let squads = SquadStore::default();
    let reviewer = {
        let alice = alice.clone();
        tokio::spawn(async move {
            let id = match next_matching(&mut alice_events, |e| {
                matches!(e, Event::ApprovalRequested { .. })
            })
            .await
            {
                Event::ApprovalRequested { id, from, .. } => {
                    assert_eq!(from, "boss");
                    id
                }
                _ => unreachable!(),
            };
            let request = alice.approvals().into_iter().find(|a| a.id == id).unwrap();
            assert!(
                request.text.contains("copilot did: build it")
                    && request.text.contains("Please approve"),
                "the approver sees the work and the request: {}",
                request.text
            );
            alice.answer_approval(id, true, "looks good").await.unwrap();
            alice_events
        })
    };
    let states = timeout(
        Duration::from_secs(60),
        run_plan(&boss, &squads, &plan, |_, _| {}),
    )
    .await
    .expect("the plan finished (the reviewer answered)");
    let mut alice_events = reviewer.await.unwrap();
    match &states["signoff"] {
        StepState::Done {
            output, verified, ..
        } => {
            assert_eq!(output, "Approved by alice: looks good");
            assert!(verified);
        }
        other => panic!("{other:?}"),
    }
    let run = RunRecord::new("boss", &boss.id().to_string(), text, &plan, &states, 0);
    assert!(run.passed(), "{}", run.render_markdown());
    assert!(run.verify(None).iter().all(|(_, r)| r.is_ok()));

    // A rejection fails its claim (and the run).
    let rejecting = {
        let alice = alice.clone();
        tokio::spawn(async move {
            if let Event::ApprovalRequested { id, .. } = next_matching(&mut alice_events, |e| {
                matches!(e, Event::ApprovalRequested { .. })
            })
            .await
            {
                alice
                    .answer_approval(id, false, "needs tests")
                    .await
                    .unwrap();
            }
            alice_events
        })
    };
    let states = timeout(
        Duration::from_secs(60),
        run_plan(&boss, &squads, &plan, |_, _| {}),
    )
    .await
    .expect("the plan finished (the reviewer answered)");
    let mut alice_events = rejecting.await.unwrap();
    let run = RunRecord::new("boss", "b", text, &plan, &states, 0);
    assert!(!run.passed());
    assert_eq!(
        run.claim_results()
            .iter()
            .map(|r| r.verdict)
            .collect::<Vec<_>>(),
        vec![Verdict::Proven, Verdict::Failed]
    );

    // A request is withdrawn when the plan that made it is cancelled.
    let pending = tokio::spawn({
        let boss = boss.clone();
        let id = alice.id();
        async move {
            boss.request_approval(Some(id), "Plan step x", "please")
                .await
        }
    });
    next_matching(&mut alice_events, |e| {
        matches!(e, Event::ApprovalRequested { .. })
    })
    .await;
    assert_eq!(alice.approvals().len(), 1);
    pending.abort();
    next_matching(&mut alice_events, |e| {
        matches!(e, Event::ApprovalClosed { .. })
    })
    .await;
    assert!(alice.approvals().is_empty());
}

#[tokio::test]
async fn plans_save_verified_results_as_knowledge_and_use_it_later() {
    let mut worker = Node::new("worker").serving(vec![Provider::Copilot]);
    let mut boss = Node::new("boss");
    mutual(&mut worker, &mut boss);
    let (worker, _we) = worker.start().await;
    let (boss, _be) = boss.start().await;
    boss.connect(worker.addr()).await.unwrap();
    wait_for_peers(&boss, 1).await;
    let home = tempfile::tempdir().unwrap();
    let kb = proofkin::knowledge::KnowledgeBase::new(home.path());

    // A discovery run saves the worker's (signed, verified) result as a topic.
    let first = Plan::parse("arch @worker save=architecture map the services").unwrap();
    let states = run_plan(
        &boss,
        &SquadStore::default(),
        &first.with_knowledge(&kb).unwrap(),
        |_, _| {},
    )
    .await;
    assert_eq!(
        proofkin::plan::save_knowledge(&kb, &first, &states, "run-1"),
        ["architecture"]
    );
    assert!(
        kb.read("architecture")
            .unwrap()
            .contains("copilot did: map the services")
    );

    // A later run gets that knowledge in its prompt.
    let second = Plan::parse("design @worker context=architecture design the change").unwrap();
    let states = run_plan(
        &boss,
        &SquadStore::default(),
        &second.with_knowledge(&kb).unwrap(),
        |_, _| {},
    )
    .await;
    match &states["design"] {
        StepState::Done { output, .. } => {
            assert!(output.contains("### Knowledge: architecture"), "{output}");
            assert!(output.contains("copilot did: map the services"), "{output}");
            assert!(output.ends_with("design the change"), "{output}");
        }
        other => panic!("{other:?}"),
    }
    let _ = worker;
}

#[tokio::test]
async fn a_step_with_microsoft_365_content_is_checked_again_when_its_task_is_sent() {
    use proofkin::knowledge::{KnowledgeBase, with_mark};
    use proofkin::plan::Recipients;
    use proofkin::{M365Grant, TrustedPeer};

    let mut worker = Node::new("worker").serving(vec![Provider::Copilot]);
    let mut boss = Node::new("boss");
    mutual(&mut worker, &mut boss);
    let grant = |labels: &[&str]| {
        Some(M365Grant {
            labels: labels.iter().map(|l| (*l).to_owned()).collect(),
            tools: false,
        })
    };
    boss.trust.peers[0].m365 = grant(&["General", "HR"]);
    let worker_id = worker.id();
    let (worker, _we) = worker.start().await;
    let (boss, _be) = boss.start().await;
    boss.connect(worker.addr()).await.unwrap();
    wait_for_peers(&boss, 1).await;
    let home = tempfile::tempdir().unwrap();
    let kb = KnowledgeBase::new(home.path());
    kb.write("hr-notes", &with_mark("Pay bands", &["HR".into()]), "")
        .unwrap();
    let plan = Plan::parse("a @worker context=hr-notes Summarize\nb after a @worker Review it")
        .unwrap()
        .with_knowledge(&kb)
        .unwrap();

    // Granted when the plan is checked and when it runs.
    let states = run_plan(&boss, &SquadStore::default(), &plan, |_, _| {}).await;
    assert!(
        states.values().all(|s| matches!(s, StepState::Done { .. })),
        "{states:?}"
    );

    // The owner takes HR out of the grant while the plan is waiting to run.
    boss.allow(TrustedPeer {
        name: "worker".into(),
        id: worker_id,
        addrs: BTreeSet::new(),
        can_delegate: true,
        tasks_per_hour: 100,
        access: Access::ReadOnly,
        shares: vec![DATA_SHARE.to_owned()],
        m365: grant(&["General"]),
        github: None,
        agents: Vec::new(),
    });
    let states = run_plan(&boss, &SquadStore::default(), &plan, |_, _| {}).await;
    let expected = Recipients::new(boss.trusted(), Vec::new())
        .refusal(&plan.steps[0], "worker", &worker_id)
        .expect("the grant no longer covers HR");
    match &states["a"] {
        StepState::Failed { peer, error, .. } => {
            assert_eq!(peer.as_deref(), Some("worker"));
            assert_eq!(error, &expected);
            assert!(
                error.contains("step 'a'") && error.contains("labels: HR"),
                "{error}"
            );
            assert!(error.contains("--m365 General,HR"), "{error}");
        }
        other => panic!("{other:?}"),
    }
    assert!(
        matches!(states["b"], StepState::Skipped { .. }),
        "{:?}",
        states["b"]
    );

    // The owner's own digital workers may have all of it.
    let ctx = RunContext {
        recipients: Recipients::new(TrustStore::default(), vec![worker_id]),
        ..RunContext::default()
    };
    let states = run_plan_with(&boss, &SquadStore::default(), &plan, &ctx, |_, _| {}).await;
    assert!(
        states.values().all(|s| matches!(s, StepState::Done { .. })),
        "{states:?}"
    );
}

/// A Work IQ result for tests: a file with `text`, protected as `allowed`, labelled `label`.
fn workiq_file(id: &str, label: &str, text: &str, score: f64) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "markdown": text,
        "webUrl": format!("https://contoso.example/files/{id}"),
        "entityType": "file",
        "contentProtection": "allowed",
        "sensitivityLabel": { "displayName": label },
        "resourceMetadata": { "document": { "title": format!("File {id}") } },
        "relevanceScore": score,
    })
}

/// The `structuredContent` of a `retrieve` answer with `files`.
fn workiq_retrieval(files: Vec<serde_json::Value>) -> serde_json::Value {
    serde_json::json!({
        "application/vnd.ms-workiq.retrieval": {
            "results": files,
            "resultCount": files.len(),
            "stoppedReason": "completed",
            "conversationId": "conv-1",
        }
    })
}

/// What a step asked a [`FakeWork`] for.
#[derive(Debug, Clone, PartialEq, Eq)]
struct WorkAsk {
    query: String,
    strategy: String,
    capabilities: Vec<String>,
    /// The peer the content is for; none for the owner's own workers.
    peer: Option<String>,
    run: Option<String>,
    step: Option<String>,
}

/// A Work IQ that answers every search with the same results and remembers the asks.
#[derive(Debug)]
struct FakeWork {
    reply: serde_json::Value,
    /// What `ask` answers (its `structuredContent`).
    answer: serde_json::Value,
    asks: Mutex<Vec<WorkAsk>>,
    /// The question and the agent id of each `ask`.
    questions: Mutex<Vec<(String, Option<String>)>>,
    /// Each change made: its tool, its arguments and the step it was made for.
    changes: Mutex<Vec<(String, serde_json::Value, Option<String>)>>,
    /// How the changes go, in order: what Work IQ answers, or why it fails; past the list,
    /// done.
    change_replies: Mutex<std::collections::VecDeque<Result<serde_json::Value, String>>>,
}

impl FakeWork {
    fn new(files: Vec<serde_json::Value>) -> Arc<Self> {
        Self::answering(
            files,
            serde_json::json!({
                "answer": "The launch moved to May.",
                "citations": [
                    { "title": "Launch plan", "sensitivityLabel": { "displayName": "General" } },
                ],
            }),
        )
    }

    fn answering(files: Vec<serde_json::Value>, answer: serde_json::Value) -> Arc<Self> {
        Arc::new(Self {
            reply: workiq_retrieval(files),
            answer,
            asks: Mutex::default(),
            questions: Mutex::default(),
            changes: Mutex::default(),
            change_replies: Mutex::default(),
        })
    }

    fn questions(&self) -> Vec<(String, Option<String>)> {
        self.questions.lock().unwrap().clone()
    }

    fn asks(&self) -> Vec<WorkAsk> {
        self.asks.lock().unwrap().clone()
    }

    fn changes(&self) -> Vec<(String, serde_json::Value, Option<String>)> {
        self.changes.lock().unwrap().clone()
    }
}

#[async_trait]
impl proofkin::workiq::context::WorkContext for FakeWork {
    async fn retrieve(
        &self,
        context: &proofkin::workiq::audit::CallContext,
        query: &str,
        strategy: &str,
        capabilities: &[&str],
    ) -> anyhow::Result<proofkin::workiq::ToolResult> {
        use proofkin::workiq::audit::Purpose;
        let (run, step) = match &context.purpose {
            Purpose::Step { run, step } => (Some(run.clone()), Some(step.clone())),
            _ => (None, None),
        };
        self.asks.lock().unwrap().push(WorkAsk {
            query: query.to_owned(),
            strategy: strategy.to_owned(),
            capabilities: capabilities.iter().map(|c| (*c).to_owned()).collect(),
            peer: context.peer.clone(),
            run,
            step,
        });
        Ok(proofkin::workiq::ToolResult {
            content: Vec::new(),
            structured_content: Some(self.reply.clone()),
            is_error: false,
            ids: proofkin::workiq::RequestIds::default(),
        })
    }

    async fn ask(
        &self,
        _context: &proofkin::workiq::audit::CallContext,
        question: &str,
        agent_id: Option<&str>,
    ) -> anyhow::Result<proofkin::workiq::ToolResult> {
        self.questions
            .lock()
            .unwrap()
            .push((question.to_owned(), agent_id.map(str::to_owned)));
        Ok(proofkin::workiq::ToolResult {
            content: Vec::new(),
            structured_content: Some(self.answer.clone()),
            is_error: false,
            ids: proofkin::workiq::RequestIds::default(),
        })
    }

    async fn change(
        &self,
        context: &proofkin::workiq::audit::CallContext,
        tool: &str,
        arguments: serde_json::Value,
    ) -> anyhow::Result<proofkin::workiq::ToolResult> {
        use proofkin::workiq::audit::Purpose;
        let step = match &context.purpose {
            Purpose::Step { step, .. } => Some(step.clone()),
            _ => None,
        };
        self.changes
            .lock()
            .unwrap()
            .push((tool.to_owned(), arguments, step));
        let reply = self.change_replies.lock().unwrap().pop_front();
        let structured = match reply {
            Some(Err(why)) => anyhow::bail!("{why}"),
            Some(Ok(value)) => value,
            None => serde_json::json!({}),
        };
        Ok(proofkin::workiq::ToolResult {
            content: Vec::new(),
            structured_content: Some(structured),
            is_error: false,
            ids: proofkin::workiq::RequestIds::default(),
        })
    }
}

/// A boss connected to `helper`, one of its own digital workers, and `partner`, another
/// person's agent that the boss grants Microsoft 365 content with `labels`.
async fn boss_with_helper_and_partner(labels: &[&str]) -> (Agent, Vec<Agent>, EndpointId) {
    let mut helper = Node::new("helper").serving(vec![Provider::Copilot]);
    let mut partner = Node::new("partner").serving(vec![Provider::Copilot]);
    let mut boss = Node::new("boss");
    mutual(&mut helper, &mut boss);
    mutual(&mut partner, &mut boss);
    boss.trust
        .peers
        .iter_mut()
        .find(|p| p.name == "partner")
        .unwrap()
        .m365 = Some(proofkin::M365Grant {
        labels: labels.iter().map(|l| (*l).to_owned()).collect(),
        tools: false,
    });
    let helper_id = helper.id();
    let (helper, _he) = helper.start().await;
    let (partner, _pe) = partner.start().await;
    let (boss, _be) = boss.start().await;
    boss.connect(helper.addr()).await.unwrap();
    boss.connect(partner.addr()).await.unwrap();
    wait_for_peers(&boss, 2).await;
    (boss, vec![helper, partner], helper_id)
}

fn workiq_run(
    helper_id: EndpointId,
    work: Arc<dyn proofkin::workiq::context::WorkContext>,
) -> RunContext {
    RunContext {
        run_id: Some("r1".into()),
        recipients: proofkin::plan::Recipients::new(TrustStore::default(), vec![helper_id]),
        work_context: Some(work),
        ..RunContext::default()
    }
}

fn done_output(states: &std::collections::BTreeMap<String, StepState>, id: &str) -> String {
    match &states[id] {
        StepState::Done { output, .. } => output.clone(),
        other => panic!("{id}: {other:?}"),
    }
}

#[tokio::test]
async fn a_local_step_gets_the_work_iq_section_in_front_of_its_task() {
    let (boss, _agents, helper_id) = boss_with_helper_and_partner(&["General"]).await;
    let work = FakeWork::new(vec![
        workiq_file("f1", "Confidential", "Budget is four million.", 0.9),
        workiq_file("f2", "General", "Launch is in May.", 0.5),
    ]);
    let ctx = workiq_run(helper_id, work.clone());
    let plan = Plan::parse("a @helper workiq=email,files Find the launch budget").unwrap();
    let states = run_plan_with(&boss, &SquadStore::default(), &plan, &ctx, |_, _| {}).await;

    let output = done_output(&states, "a");
    let section = output
        .find("## Work context from Microsoft 365")
        .expect(&output);
    let task = output
        .find("Your task:\nFind the launch budget")
        .expect(&output);
    assert!(
        section < task,
        "the section comes before the task: {output}"
    );
    assert!(output.contains("Budget is four million."), "{output}");
    assert!(output.contains("Launch is in May."), "{output}");
    assert!(
        output.starts_with("copilot did: ## Rule for external content"),
        "the prompt stays guarded: {output}"
    );

    // It asked for the step's own words, for the owner's worker, in this run and step.
    assert_eq!(
        work.asks(),
        [WorkAsk {
            query: "Find the launch budget".into(),
            strategy: "grounding".into(),
            capabilities: vec!["Email".into(), "OneDriveAndSharePoint".into()],
            peer: None,
            run: Some("r1".into()),
            step: Some("a".into()),
        }]
    );
    let record = &ctx.work_log.records()["a"];
    assert_eq!((record.kept, record.withheld), (2, 0));
    assert_eq!(record.labels, ["Confidential", "General"]);
    assert_eq!(record.sources.len(), 2);
    assert_eq!(
        record.sources[0].title, "File f1",
        "the most relevant first"
    );
    assert_eq!(
        record.sources[0].link.as_deref(),
        Some("https://contoso.example/files/f1")
    );
}

#[tokio::test]
async fn a_granted_peer_gets_only_the_work_iq_results_its_grant_allows() {
    let (boss, _agents, helper_id) = boss_with_helper_and_partner(&["General"]).await;
    let work = FakeWork::new(vec![
        workiq_file("f1", "Confidential", "Budget is four million.", 0.9),
        workiq_file("f2", "General", "Launch is in May.", 0.5),
    ]);
    let ctx = workiq_run(helper_id, work.clone());
    let plan = Plan::parse("a @partner workiq=all Find the launch dates").unwrap();
    let states = run_plan_with(&boss, &SquadStore::default(), &plan, &ctx, |_, _| {}).await;

    let output = done_output(&states, "a");
    assert!(output.contains("Launch is in May."), "{output}");
    assert!(
        !output.contains("Budget is four million."),
        "a label the grant doesn't include is withheld: {output}"
    );
    let asks = work.asks();
    assert_eq!(asks.len(), 1, "{asks:?}");
    assert_eq!(asks[0].peer.as_deref(), Some("partner"));
    assert_eq!(asks[0].strategy, "copilot", "workiq=all");
    assert!(
        asks[0].capabilities.is_empty(),
        "{:?}",
        asks[0].capabilities
    );
    let record = &ctx.work_log.records()["a"];
    assert_eq!((record.kept, record.withheld), (1, 1));
    assert_eq!(record.labels, ["General"]);
}

/// The peer's signed evidence says which Work IQ context its task had (its hash, how many
/// results and their labels) but never what it said; the run keeps that, and `runs verify`
/// checks that the two agree.
#[tokio::test]
async fn a_steps_signed_evidence_records_its_work_iq_context_by_hash_never_its_content() {
    let (boss, _agents, helper_id) = boss_with_helper_and_partner(&["General"]).await;
    let work = FakeWork::new(vec![workiq_file("f2", "General", "Launch is in May.", 0.5)]);
    let ctx = workiq_run(helper_id, work);
    let text = "a @partner workiq=files Find the launch dates";
    let plan = Plan::parse(text).unwrap();
    let started = proofkin::runs::unix_now();
    let states = run_plan_with(&boss, &SquadStore::default(), &plan, &ctx, |_, _| {}).await;

    let StepState::Done {
        evidence: Some(evidence),
        output,
        ..
    } = &states["a"]
    else {
        panic!("{:?}", states["a"])
    };
    let kept = ctx.work_log.records()["a"].clone();
    assert!(kept.text.contains("Launch is in May."), "{}", kept.text);
    assert_eq!(
        kept.sha256,
        proofkin::evidence::sha256_hex(kept.text.as_bytes())
    );
    assert_eq!(
        evidence.context,
        [proofkin::evidence::ContextUse {
            source: "workiq".into(),
            sha256: kept.sha256.clone(),
            results: 1,
            labels: vec!["General".into()],
        }]
    );
    let signed = serde_json::to_string(evidence).unwrap();
    assert!(!signed.contains("Launch is in May"), "{signed}");
    assert!(
        output.contains("**Work context** from Microsoft 365 (Work IQ): 1 result (General)"),
        "{output}"
    );

    let mut run = RunRecord::new(
        "boss",
        &boss.id().to_string(),
        text,
        &plan,
        &states,
        started,
    );
    run.peer_ids = proofkin::runs::peer_ids(&boss);
    run.workiq = ctx.work_log.records();
    let checks = run.verify(None);
    assert!(checks.iter().all(|(_, r)| r.is_ok()), "{checks:?}");
    // Kept context that isn't what the evidence records fails it.
    let mut edited = run.clone();
    edited.workiq.get_mut("a").unwrap().text.push('!');
    let failed = edited.verify(None);
    assert!(
        format!("{:#}", failed[0].1.as_ref().unwrap_err()).contains("doesn't have the hash"),
        "{failed:?}"
    );
    let mut other = run.clone();
    let record = other.workiq.get_mut("a").unwrap();
    record.text = "## Other context\n".into();
    record.sha256 = proofkin::evidence::sha256_hex(record.text.as_bytes());
    let failed = other.verify(None);
    assert!(
        format!("{:#}", failed[0].1.as_ref().unwrap_err())
            .contains("doesn't record the Work IQ context it was sent"),
        "{failed:?}"
    );

    // Saved, its run.json has only the hash (it is the file shared to be verified); the
    // context is next to it, and checked from there.
    let runs = tempfile::tempdir().unwrap();
    run.save(runs.path()).unwrap();
    let dir = runs.path().join(&run.id);
    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("run.json")).unwrap()).unwrap();
    assert!(saved["workiq"]["a"].get("text").is_none(), "{saved:#}");
    assert_eq!(saved["workiq"]["a"]["sha256"], kept.sha256.as_str());
    assert_eq!(
        std::fs::read_to_string(dir.join("a.workiq.md")).unwrap(),
        kept.text
    );
    let loaded = RunRecord::load(runs.path(), &run.id).unwrap();
    assert!(loaded.workiq["a"].text.is_empty());
    let checks = loaded.verify(Some(&dir));
    assert!(checks.iter().all(|(_, r)| r.is_ok()), "{checks:?}");
    let alone = loaded.verify(None);
    assert!(alone.iter().all(|(_, r)| r.is_ok()), "{alone:?}");
    std::fs::write(dir.join("a.workiq.md"), "## Other context\n").unwrap();
    let failed = loaded.verify(Some(&dir));
    assert!(
        format!("{:#}", failed[0].1.as_ref().unwrap_err()).contains("doesn't have the hash"),
        "{failed:?}"
    );

    // A task that says its prompt has context it hasn't is refused.
    let partner = boss
        .peers()
        .into_iter()
        .find(|p| p.name == "partner")
        .unwrap()
        .id;
    let mut task = LlmTask::new(Provider::Copilot, "hello");
    task.context = vec![proofkin::evidence::ContextRef {
        source: "workiq".into(),
        start: 2,
        len: 50,
        results: 1,
        labels: Vec::new(),
    }];
    let refused = boss.delegate(partner, task).await.unwrap_err();
    assert!(
        format!("{refused:#}").contains("work context part 1 of the task"),
        "{refused:#}"
    );
}

#[tokio::test]
async fn a_later_step_is_refused_at_send_time_when_what_was_found_exceeds_its_grant() {
    let (boss, _agents, helper_id) = boss_with_helper_and_partner(&["General"]).await;
    let work = FakeWork::new(vec![
        workiq_file("f1", "Confidential", "Budget is four million.", 0.9),
        workiq_file("f2", "General", "Launch is in May.", 0.5),
    ]);
    let ctx = workiq_run(helper_id, work.clone());
    // `a` runs on the owner's own worker and gets all of it; what it found isn't known
    // until it runs, so `b` is checked when it is sent.
    let plan = Plan::parse("a @helper workiq=all Find it\nb after a @partner Write it up").unwrap();
    let states = run_plan_with(&boss, &SquadStore::default(), &plan, &ctx, |_, _| {}).await;

    assert!(
        matches!(states["a"], StepState::Done { .. }),
        "{:?}",
        states["a"]
    );
    match &states["b"] {
        StepState::Failed { peer, error, .. } => {
            assert_eq!(peer.as_deref(), Some("partner"));
            assert!(
                error.contains("step 'b'") && error.contains("grant doesn't include Confidential"),
                "{error}"
            );
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(work.asks().len(), 1, "only `a` searched");

    // With only what its grant allows found, the same step is sent.
    let narrow = FakeWork::new(vec![workiq_file("f2", "General", "Launch is in May.", 0.5)]);
    let ctx = workiq_run(helper_id, narrow);
    let states = run_plan_with(&boss, &SquadStore::default(), &plan, &ctx, |_, _| {}).await;
    assert!(
        states.values().all(|s| matches!(s, StepState::Done { .. })),
        "{states:?}"
    );
}

#[tokio::test]
async fn a_work_iq_error_fails_the_step_with_its_one_line_message() {
    #[derive(Debug)]
    struct Down;

    #[async_trait]
    impl proofkin::workiq::context::WorkContext for Down {
        async fn retrieve(
            &self,
            _: &proofkin::workiq::audit::CallContext,
            _: &str,
            _: &str,
            _: &[&str],
        ) -> anyhow::Result<proofkin::workiq::ToolResult> {
            anyhow::bail!("Work IQ is unavailable (503)")
        }
    }

    let (boss, _agents, helper_id) = boss_with_helper_and_partner(&["General"]).await;
    let ctx = workiq_run(helper_id, Arc::new(Down));
    let plan = Plan::parse("a @helper workiq=all Find it\nb after a @helper Use it").unwrap();
    let states = run_plan_with(&boss, &SquadStore::default(), &plan, &ctx, |_, _| {}).await;
    match &states["a"] {
        StepState::Failed { error, .. } => assert_eq!(error, "Work IQ is unavailable (503)"),
        other => panic!("{other:?}"),
    }
    assert!(
        matches!(states["b"], StepState::Skipped { .. }),
        "{:?}",
        states["b"]
    );
}

type RetrieveError = fn(&serde_json::Value) -> Option<serde_json::Value>;

/// A Work IQ MCP endpoint that answers `retrieve` with two files by default and logs its calls.
/// Microsoft Graph accepts a change (a sign-off's notice), unless `refuse` is set.
#[derive(Default)]
struct FakeMcp {
    calls: Mutex<Vec<serde_json::Value>>,
    refuse: AtomicBool,
    retrieve_error: Option<RetrieveError>,
}

async fn fake_mcp(
    axum::extract::State(fake): axum::extract::State<Arc<FakeMcp>>,
    body: String,
) -> impl axum::response::IntoResponse {
    use serde_json::json;
    let message: serde_json::Value = serde_json::from_str(&body).unwrap();
    let id = message["id"].clone();
    let reply = match message["method"].as_str().unwrap() {
        "notifications/initialized" => {
            return (
                axum::http::StatusCode::ACCEPTED,
                [("content-type", "application/json".to_string())],
                String::new(),
            );
        }
        "initialize" => json!({ "jsonrpc": "2.0", "id": id, "result": {
            "protocolVersion": "2025-06-18",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "fake-workiq", "version": "0" },
        }}),
        "tools/list" => json!({ "jsonrpc": "2.0", "id": id, "result": { "tools": [
            { "name": "retrieve", "inputSchema": { "type": "object" } },
            { "name": "ask", "inputSchema": { "type": "object" } },
            { "name": "list_agents", "inputSchema": { "type": "object" } },
        ]}}),
        "tools/call" => {
            fake.calls.lock().unwrap().push(message["params"].clone());
            if message["params"]["name"] == "retrieve"
                && let Some(result) = fake
                    .retrieve_error
                    .and_then(|fail| fail(&message["params"]["arguments"]))
            {
                let reply = json!({ "jsonrpc": "2.0", "id": id, "result": result });
                return (
                    axum::http::StatusCode::OK,
                    [("content-type", "application/json".to_string())],
                    reply.to_string(),
                );
            }
            let change = matches!(
                message["params"]["name"].as_str(),
                Some("do_action" | "create_entity")
            );
            if change && fake.refuse.load(Ordering::SeqCst) {
                let reply = json!({ "jsonrpc": "2.0", "id": id, "result": {
                    "content": [{ "type": "text", "text": "Forbidden: denied by policy" }],
                    "isError": true,
                }});
                return (
                    axum::http::StatusCode::OK,
                    [("content-type", "application/json".to_string())],
                    reply.to_string(),
                );
            }
            let structured = match message["params"]["name"].as_str() {
                Some("list_agents") => json!({ "agents": [
                    { "id": "sales-agent:v1", "name": "Sales helper" },
                    { "agentId": "hr_agent", "displayName": "HR" },
                    { "name": "no id" },
                ]}),
                Some("ask") => json!({
                    "answer": "The launch moved to May.",
                    "citations": [{ "title": "Launch plan", "url": "https://contoso.example/p" }],
                }),
                _ if change => json!({ "statusCode": 202 }),
                _ => workiq_retrieval(vec![
                    workiq_file("f1", "General", "Launch is in May.", 0.9),
                    workiq_file("f2", "Confidential", "Budget is four million.", 0.5),
                ]),
            };
            json!({ "jsonrpc": "2.0", "id": id, "result": {
                "content": [{ "type": "text", "text": "retrieved" }],
                "structuredContent": structured,
            }})
        }
        other => panic!("unexpected method {other}"),
    };
    (
        axum::http::StatusCode::OK,
        [("content-type", "application/json".to_string())],
        reply.to_string(),
    )
}

const GENERIC_RETRIEVE_ERROR: &str = "An error occurred invoking 'retrieve'";
const FALLBACK_NOTE: &str = concat!(
    "Work IQ's copilot search failed with its generic error, ",
    "so it searched again with grounding, without connectors.",
);
const GROUNDING_CAPABILITIES: &[&str] = &[
    "Email",
    "Meetings",
    "OneDriveAndSharePoint",
    "TeamsMessages",
    "People",
];

fn retrieve_error(text: &str, code: Option<&str>) -> serde_json::Value {
    use serde_json::json;
    let mut result = json!({
        "content": [{ "type": "text", "text": text }],
        "isError": true,
    });
    if let Some(code) = code {
        result["structuredContent"] = json!({ "errorCode": code });
    }
    result
}

fn generic_copilot_error(arguments: &serde_json::Value) -> Option<serde_json::Value> {
    (arguments["strategy"] == "copilot").then(|| retrieve_error(GENERIC_RETRIEVE_ERROR, None))
}

fn retrieval(query: &str, strategy: &str, capabilities: &[&str]) -> serde_json::Value {
    use serde_json::json;
    let mut arguments = json!({ "query": query, "strategy": strategy });
    if !capabilities.is_empty() {
        let named: Vec<_> = capabilities
            .iter()
            .map(|name| json!({ "name": name }))
            .collect();
        arguments["capabilities"] = json!(named);
    }
    json!({ "name": "retrieve", "arguments": arguments })
}

async fn failing_workiq(
    audit_path: std::path::PathBuf,
    fail: RetrieveError,
) -> (
    Arc<FakeMcp>,
    Arc<proofkin::workiq::audit::Audit>,
    Arc<proofkin::workiq::context::ClientWork>,
) {
    use proofkin::workiq::audit::Audit;
    use proofkin::workiq::client::{Client, FixedToken};
    use proofkin::workiq::context::ClientWork;

    let (fake, url) = serve_mcp(FakeMcp {
        retrieve_error: Some(fail),
        ..FakeMcp::default()
    })
    .await;
    let audit = Arc::new(Audit::new(audit_path, "boss", Default::default()));
    let client = Client::new(&url, Arc::new(FixedToken("token".into())))
        .unwrap()
        .with_audit(audit.clone());
    (fake, audit, Arc::new(ClientWork::new(client)))
}

#[tokio::test]
async fn a_workiq_all_step_uses_grounding_after_the_generic_copilot_error_and_records_both() {
    let dir = tempfile::tempdir().unwrap();
    let (fake, audit, work) =
        failing_workiq(dir.path().join("audit.jsonl"), generic_copilot_error).await;
    let (boss, _agents, helper_id) = boss_with_helper_and_partner(&["General"]).await;
    let ctx = workiq_run(helper_id, work);
    let query = "Find the launch dates";
    let text = format!("find @partner workiq=all {query}");
    let plan = Plan::parse(&text).unwrap();
    let mut progress = Vec::new();
    let states = run_plan_with(&boss, &SquadStore::default(), &plan, &ctx, |id, state| {
        if let StepState::Running {
            progress: Some(update),
            ..
        } = state
        {
            progress.push((id.to_owned(), update.text.clone()));
        }
    })
    .await;

    let output = done_output(&states, "find");
    assert!(output.contains("Launch is in May."), "{output}");
    assert!(
        !output.contains("Budget is four million."),
        "the peer's grant still applies"
    );
    assert!(
        progress.contains(&("find".into(), FALLBACK_NOTE.into())),
        "{progress:?}"
    );
    assert_eq!(
        *fake.calls.lock().unwrap(),
        [
            retrieval(query, "copilot", &[]),
            retrieval(query, "grounding", GROUNDING_CAPABILITIES),
        ]
    );

    let records = ctx.work_log.records();
    assert!(records["find"].grounding_fallback);
    assert_eq!((records["find"].kept, records["find"].withheld), (1, 1));
    let mut run = RunRecord::new("boss", &boss.id().to_string(), &text, &plan, &states, 0);
    run.workiq = records;
    run.save(dir.path()).unwrap();
    let shown = RunRecord::load(dir.path(), &run.id)
        .unwrap()
        .render_markdown();
    assert!(
        shown.contains(&format!("Microsoft 365 sources: {FALLBACK_NOTE}")),
        "{shown}"
    );

    let rows = proofkin::workiq::audit::read(audit.path()).unwrap();
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[0].outcome, "tool");
    assert_eq!(rows[1].outcome, "ok");
    for row in &rows {
        assert_eq!(row.tool, "retrieve");
        assert_eq!(row.purpose, "step");
        assert_eq!(row.audience, "partner");
        assert_eq!(row.run.as_deref(), Some("r1"));
        assert_eq!(row.step.as_deref(), Some("find"));
        assert_eq!(row.query_sha256, rows[0].query_sha256);
    }
    assert_eq!(audit.usage(Some("r1")).run, Some(2));
    assert_eq!(audit.usage(None).today, 2);
}

#[tokio::test]
async fn workiq_steps_do_not_fall_back_for_policy_specific_errors_connectors_only_or_grounding() {
    let dir = tempfile::tempdir().unwrap();
    let (fake, _audit, work) = failing_workiq(dir.path().join("audit.jsonl"), |arguments| {
        let reply = match arguments["query"].as_str().unwrap() {
            "Policy case" => retrieve_error(GENERIC_RETRIEVE_ERROR, Some("DeniedByPolicy")),
            "Specific case" => {
                retrieve_error("An error occurred invoking 'retrieve': bad query", None)
            }
            _ => retrieve_error(GENERIC_RETRIEVE_ERROR, None),
        };
        Some(reply)
    })
    .await;
    let (boss, _agents, helper_id) = boss_with_helper_and_partner(&["General"]).await;
    let ctx = workiq_run(helper_id, work);
    let plan = Plan::parse(concat!(
        "policy @helper workiq=all Policy case\n",
        "specific @helper workiq=all Specific case\n",
        "connectors @helper workiq=connectors Find a connector item\n",
        "grounding @helper workiq=files Search files",
    ))
    .unwrap();
    let states = run_plan_with(&boss, &SquadStore::default(), &plan, &ctx, |_, _| {}).await;
    for step in ["policy", "specific", "connectors", "grounding"] {
        assert!(
            matches!(states[step], StepState::Failed { .. }),
            "{states:?}"
        );
    }
    assert!(ctx.work_log.records().is_empty());
    assert_eq!(
        *fake.calls.lock().unwrap(),
        [
            retrieval("Policy case", "copilot", &[]),
            retrieval("Specific case", "copilot", &[]),
            retrieval("Find a connector item", "copilot", &["GraphConnectors"]),
            retrieval("Search files", "grounding", &["OneDriveAndSharePoint"]),
        ]
    );
}

#[tokio::test]
async fn a_failed_grounding_fallback_names_both_errors_without_searching_again() {
    use proofkin::knowledge::KnowledgeBase;
    use proofkin::workiq::client::WorkIqError;
    use proofkin::workiq::import::import_knowledge;

    const GROUNDING_ERROR: &str = "No search index for grounding";
    let dir = tempfile::tempdir().unwrap();
    let (fake, _audit, work) = failing_workiq(dir.path().join("audit.jsonl"), |arguments| {
        generic_copilot_error(arguments).or_else(|| Some(retrieve_error(GROUNDING_ERROR, None)))
    })
    .await;
    let (boss, _agents, helper_id) = boss_with_helper_and_partner(&["General"]).await;
    let ctx = workiq_run(helper_id, work.clone());
    let query = "Find the launch dates";
    let plan = Plan::parse(&format!("find @helper workiq=all {query}")).unwrap();
    let states = run_plan_with(&boss, &SquadStore::default(), &plan, &ctx, |_, _| {}).await;
    let StepState::Failed { error, .. } = &states["find"] else {
        panic!("{states:?}")
    };
    assert!(
        error.contains("grounding search failed after its copilot search failed"),
        "{error}"
    );
    assert!(error.contains(GENERIC_RETRIEVE_ERROR), "{error}");
    assert!(error.contains(GROUNDING_ERROR), "{error}");
    assert!(ctx.work_log.records()["find"].grounding_fallback);

    let kb = KnowledgeBase::new(dir.path().join("knowledge"));
    let failure = import_knowledge(work.as_ref(), &kb, "launch", query, Some("all"), false)
        .await
        .unwrap_err();
    assert_eq!(
        failure
            .downcast_ref::<WorkIqError>()
            .unwrap()
            .tool_text
            .as_deref(),
        Some(GROUNDING_ERROR)
    );
    assert!(
        failure
            .to_string()
            .contains("after its copilot search failed")
    );
    assert!(kb.list().is_empty());
    assert_eq!(
        *fake.calls.lock().unwrap(),
        [
            retrieval(query, "copilot", &[]),
            retrieval(query, "grounding", GROUNDING_CAPABILITIES),
            retrieval(query, "copilot", &[]),
            retrieval(query, "grounding", GROUNDING_CAPABILITIES),
        ]
    );
}

#[tokio::test]
async fn kb_import_reports_and_audits_the_grounding_fallback_for_all_and_mixed_sources() {
    use proofkin::knowledge::KnowledgeBase;
    use proofkin::workiq::import::import_knowledge;

    let dir = tempfile::tempdir().unwrap();
    let (fake, audit, work) =
        failing_workiq(dir.path().join("audit.jsonl"), generic_copilot_error).await;
    let kb = KnowledgeBase::new(dir.path().join("knowledge"));
    let query = "launch budget";
    for (topic, from) in [("launch", "all"), ("launch-files", "files,connectors")] {
        let done = import_knowledge(work.as_ref(), &kb, topic, query, Some(from), false)
            .await
            .unwrap();
        assert_eq!(done.fallback_note, Some(FALLBACK_NOTE));
        assert_eq!((done.kept, done.withheld), (2, 0));
        assert!(kb.read(topic).unwrap().contains("Launch is in May."));
    }
    assert_eq!(
        *fake.calls.lock().unwrap(),
        [
            retrieval(query, "copilot", &[]),
            retrieval(query, "grounding", GROUNDING_CAPABILITIES),
            retrieval(
                query,
                "copilot",
                &["OneDriveAndSharePoint", "GraphConnectors"]
            ),
            retrieval(query, "grounding", &["OneDriveAndSharePoint"]),
        ]
    );
    let rows = proofkin::workiq::audit::read(audit.path()).unwrap();
    assert!(
        rows.iter()
            .all(|row| row.purpose == "knowledge-import" && row.audience == "local")
    );
    let outcomes: Vec<_> = rows.iter().map(|row| row.outcome.as_str()).collect();
    assert_eq!(outcomes, ["tool", "ok", "tool", "ok"]);
}

#[tokio::test]
async fn workiq_steps_search_through_the_audited_client_and_stop_at_the_run_budget() {
    use proofkin::workiq::audit::Audit;
    use proofkin::workiq::client::{Client, FixedToken};
    use proofkin::workiq::context::ClientWork;

    let fake = Arc::new(FakeMcp::default());
    let app = axum::Router::new()
        .route("/mcp", axum::routing::post(fake_mcp))
        .with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let dir = tempfile::tempdir().unwrap();
    let budget = proofkin::profile::WorkIqBudget {
        calls_per_run: Some(1),
        ..Default::default()
    };
    let audit = Arc::new(Audit::new(dir.path().join("audit.jsonl"), "boss", budget));
    let client = Client::new(&url, Arc::new(FixedToken("token".into())))
        .unwrap()
        .with_audit(audit.clone());

    let (boss, _agents, helper_id) = boss_with_helper_and_partner(&["General"]).await;
    let ctx = workiq_run(helper_id, Arc::new(ClientWork::new(client)));
    let plan = Plan::parse(
        "a @helper workiq=email Find the launch budget\nb after a @helper workiq=files Find more",
    )
    .unwrap();
    let states = run_plan_with(&boss, &SquadStore::default(), &plan, &ctx, |_, _| {}).await;

    let output = done_output(&states, "a");
    assert!(output.contains("Launch is in May."), "{output}");
    assert!(
        output.contains("Budget is four million."),
        "the owner's worker gets both"
    );
    match &states["b"] {
        StepState::Failed { error, .. } => {
            assert!(
                error.contains("proofkin workiq budget --per-run"),
                "{error}"
            );
            assert!(!error.contains('\n'), "one line: {error}");
        }
        other => panic!("{other:?}"),
    }

    // Only the first search was sent: grounding over the one capability.
    let calls = fake.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(calls[0]["name"], "retrieve");
    let arguments = &calls[0]["arguments"];
    assert_eq!(arguments["query"], "Find the launch budget");
    assert_eq!(arguments["strategy"], "grounding");
    assert_eq!(
        arguments["capabilities"],
        serde_json::json!([{ "name": "Email" }])
    );
    assert_eq!(
        audit.usage(Some("r1")).run,
        Some(1),
        "the refused call doesn't count"
    );

    // What the run keeps of it: sources, labels, counts and the request ids.
    let records = ctx.work_log.records();
    assert_eq!(records.len(), 1, "{records:?}");
    let record = &records["a"];
    assert_eq!((record.kept, record.withheld), (2, 0));
    assert_eq!(record.labels, ["Confidential", "General"]);
    assert_eq!(record.sources.len(), 2);
    assert!(
        record.request_ids.contains("client-request-id"),
        "{}",
        record.request_ids
    );
}

#[tokio::test]
async fn a_workiq_step_asks_for_its_words_and_hands_its_answer_to_later_steps() {
    let (boss, _agents, helper_id) = boss_with_helper_and_partner(&["General"]).await;
    let work = FakeWork::new(Vec::new());
    let ctx = workiq_run(helper_id, work.clone());
    let plan = Plan::parse(
        "a @helper Collect the dates\n\
         b after a @workiq agent=sales-agent:v1.2@acme What changed in the launch?\n\
         c after b @helper Write it up",
    )
    .unwrap();
    let states = run_plan_with(&boss, &SquadStore::default(), &plan, &ctx, |_, _| {}).await;

    // It asked Work IQ's agent, with its own words and what `a` produced, not a peer.
    let questions = work.questions();
    assert_eq!(questions.len(), 1, "{questions:?}");
    assert_eq!(questions[0].1.as_deref(), Some("sales-agent:v1.2@acme"));
    assert!(
        questions[0].0.contains("What changed in the launch?")
            && questions[0].0.contains("Collect the dates"),
        "{}",
        questions[0].0
    );
    assert!(work.asks().is_empty(), "it asked, not searched");

    // Shown as done by workiq:<agent id>, with no evidence and not verified.
    match &states["b"] {
        StepState::Done {
            peer,
            output,
            evidence,
            verified,
            ..
        } => {
            assert_eq!(peer, "workiq:sales-agent:v1.2@acme");
            assert!(output.contains("The launch moved to May."), "{output}");
            assert!(output.contains("Launch plan"), "{output}");
            assert!(evidence.is_none() && !verified);
        }
        other => panic!("{other:?}"),
    }
    // The answer is in the next step's prompt, and the cited label is on record.
    let next = done_output(&states, "c");
    assert!(next.contains("The launch moved to May."), "{next}");
    let record = &ctx.work_log.records()["b"];
    assert_eq!(record.labels, ["General"]);
    assert_eq!(record.sources.len(), 1);
}

#[tokio::test]
async fn an_agentless_workiq_step_sends_no_agent_id() {
    let (boss, _agents, helper_id) = boss_with_helper_and_partner(&["General"]).await;
    let work = FakeWork::new(Vec::new());
    let ctx = workiq_run(helper_id, work.clone());
    let plan = Plan::parse("a @workiq What changed?").unwrap();
    let states = run_plan_with(&boss, &SquadStore::default(), &plan, &ctx, |_, _| {}).await;
    assert_eq!(work.questions(), [("What changed?".to_owned(), None)]);
    match &states["a"] {
        StepState::Done { peer, .. } => assert_eq!(peer, "workiq"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn a_peer_signoff_cannot_have_a_workiq_step_redone() {
    use proofkin::agent::Answer;
    use proofkin::plan::{SentStep, SentWork};

    let (mut boss, mut alice) = (Node::new("boss"), Node::new("alice"));
    alice.trusts(&boss, None);
    boss.trusts(&alice, None);
    boss.trust.peers[0].m365 = Some(proofkin::M365Grant {
        labels: vec!["General".into()],
        tools: false,
    });
    let (boss, _be) = boss.start().await;
    let (alice, _ae) = alice.start().await;
    boss.connect(alice.addr()).await.unwrap();
    wait_for_peers(&boss, 1).await;

    let plan = Plan::parse(
        "draft @workiq What changed in the launch?\n\
         ok after draft approve @alice changes=1 Is this ready?",
    )
    .unwrap();
    let squads = SquadStore::default();
    let work = FakeWork::new(Vec::new());
    let mut ctx = RunContext {
        work_context: Some(work.clone()),
        ..RunContext::default()
    };
    let wait_for_approval = || async {
        loop {
            if let Some(request) = alice.approvals().into_iter().next() {
                return request;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    let note = "Check whether the date changed again";
    let review = async {
        let request = wait_for_approval().await;
        assert!(request.text.contains("The launch moved to May."));
        let refused = alice
            .answer(request.id, Answer::Changes, note)
            .await
            .unwrap_err();
        assert!(
            refused
                .to_string()
                .contains("can only be approved or rejected")
        );
        alice
            .answer(request.id, Answer::Approve, "Looks good")
            .await
            .unwrap();
        request
    };
    let run = run_plan_with(&boss, &squads, &plan, &ctx, |_, _| {});
    let (mut states, request) =
        timeout(Duration::from_secs(30), async { tokio::join!(run, review) })
            .await
            .unwrap();
    assert_eq!(done_output(&states, "ok"), "Approved by alice: Looks good");

    // A sign-off sent before the rule changed could still offer changes.
    ctx.resume
        .finished
        .insert("draft".into(), states.remove("draft").unwrap());
    ctx.resume.sent.insert(
        "ok".into(),
        SentStep {
            peer: "alice".into(),
            peer_id: Some(alice.id().to_string()),
            task_id: boss.new_task_id(),
            work: SentWork::Approval {
                title: request.title,
                text: request.text,
                changes: true,
            },
        },
    );
    let review = async {
        let request = wait_for_approval().await;
        alice
            .answer(request.id, Answer::Changes, note)
            .await
            .unwrap();
    };
    let run = run_plan_with(&boss, &squads, &plan, &ctx, |_, _| {});
    let (states, ()) = timeout(Duration::from_secs(30), async { tokio::join!(run, review) })
        .await
        .unwrap();
    let StepState::Failed {
        error,
        verified,
        peer: _,
        evidence: _,
        model: _,
    } = &states["ok"]
    else {
        panic!("{:?}", states["ok"]);
    };
    assert!(*verified);
    assert!(error.starts_with(&format!("Changes asked for by alice: {note}")));
    assert!(error.ends_with("It allows no more rounds of changes (changes=1)."));
    assert_eq!(
        work.questions(),
        [("What changed in the launch?".to_owned(), None)]
    );
}

#[tokio::test]
async fn brief_reworks_the_draft_and_gets_owner_signoff_without_making_m365_changes() {
    use proofkin::agent::Answer;

    let (boss, _events) = Node::new("boss").start().await;
    let work = FakeWork::new(Vec::new());
    let ctx = RunContext {
        work_context: Some(work.clone()),
        ..RunContext::default()
    };
    let vars = [
        ("subject".into(), "Launch timing".into()),
        ("audience".into(), "Team leads".into()),
    ]
    .into_iter()
    .collect();
    let text = proofkin::playbooks::render("brief", &vars).unwrap();
    let plan = Plan::parse(&text).unwrap();
    let squads = SquadStore::default();
    let run = run_plan_with(&boss, &squads, &plan, &ctx, |_, _| {});
    let review = async {
        for (step, changes, decision, note) in [
            ("review", true, Answer::Changes, "Lead with the launch date"),
            ("review", false, Answer::Approve, "Keep the source link"),
            ("signoff", false, Answer::Approve, "Ready for leadership"),
        ] {
            let ask = loop {
                if let Some(ask) = boss.approvals().into_iter().next() {
                    break ask;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            };
            assert_eq!(ask.title, format!("Plan step {step}"));
            assert_eq!(ask.changes, changes);
            assert!(
                ask.text.contains("The launch moved to May."),
                "{}",
                ask.text
            );
            boss.answer(ask.id, decision, note).await.unwrap();
        }
    };
    let (states, ()) = timeout(Duration::from_secs(30), async { tokio::join!(run, review) })
        .await
        .unwrap();
    let questions = work.questions();
    assert_eq!(questions.len(), 4, "{questions:?}");
    assert!(questions[2].0.contains("Lead with the launch date"));
    assert!(questions[2].0.contains(&proofkin::boundary::wrap(
        "earlier Work IQ answer",
        &done_output(&states, "draft"),
    )));
    assert!(questions[3].0.contains("Keep the source link"));
    assert!(done_output(&states, "signoff").contains("Ready for leadership"));
    assert!(work.changes().is_empty());
}

/// A plan proposes Microsoft 365 changes in one step, you sign them off, and a later step
/// makes exactly those, once and in order: the sign-off shows each change and the list's
/// SHA-256, a rejected sign-off makes none, and a change that fails stops the ones after it.
#[tokio::test]
async fn a_plan_makes_the_microsoft_365_changes_you_sign_off_once_and_in_order() {
    let mail = r#"{"tool":"do_action","path":"/me/sendMail","body":{"message":{"subject":"Re: the outage","body":{"contentType":"Text","content":"It is fixed."},"toRecipients":[{"emailAddress":{"address":"ana@contoso.com"}}]}}}"#;
    let update = r#"{"tool":"update_entity","path":"/me/events/AAMk1","body":{"subject":"Moved"}}"#;
    let proposing = || {
        FakeWork::answering(
            Vec::new(),
            serde_json::json!({
                "answer": format!("Here are the replies.\n\n```m365-changes\n{mail}\n{update}\n```"),
                "citations": [
                    { "title": "Escalations", "sensitivityLabel": { "displayName": "General" } },
                ],
            }),
        )
    };
    let plan = Plan::parse(
        "draft @workiq Draft the replies to the open escalations\n\
         ok after draft approve @me Send these replies?\n\
         send after ok @workiq apply=draft",
    )
    .unwrap();
    let (boss, _be) = Node::new("boss").start().await;
    let ctx = |work: &Arc<FakeWork>| RunContext {
        run_id: Some("r1".into()),
        recipients: proofkin::plan::Recipients {
            workiq_changes: true,
            ..Default::default()
        },
        work_context: Some(work.clone()),
        ..RunContext::default()
    };
    // You answer each sign-off as `approve` says; what it showed is kept.
    let sign = |approve: bool| {
        let shown = Arc::new(Mutex::new(Vec::<String>::new()));
        let (boss, seen) = (boss.clone(), shown.clone());
        let task = tokio::spawn(async move {
            loop {
                for a in boss.approvals() {
                    seen.lock().unwrap().push(a.text.clone());
                    let _ = boss.answer_approval(a.id, approve, "as shown").await;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });
        (task, shown)
    };
    let run = |work: Arc<FakeWork>| {
        let (boss, plan) = (boss.clone(), plan.clone());
        async move {
            let ctx = ctx(&work);
            timeout(
                Duration::from_secs(30),
                run_plan_with(&boss, &SquadStore::default(), &plan, &ctx, |_, _| {}),
            )
            .await
            .unwrap()
        }
    };

    // Approved: both are made, in order, with exactly the proposed arguments.
    let work = proposing();
    let (signer, shown) = sign(true);
    let states = run(work.clone()).await;
    signer.abort();
    let question = &work.questions()[0].0;
    assert!(
        question.contains("```m365-changes") && question.contains("Don't change Microsoft 365"),
        "{question}"
    );
    let shown = shown.lock().unwrap().clone();
    assert_eq!(shown.len(), 1, "{shown:?}");
    assert!(
        shown[0].starts_with("### Microsoft 365 changes step draft proposes")
            && shown[0].contains("SHA-256 of the list: `")
            && shown[0].contains("To: ana@contoso.com")
            && shown[0].contains("Subject: Re: the outage")
            && shown[0].contains("update_entity /me/events/AAMk1"),
        "{}",
        shown[0]
    );
    let made = work.changes();
    assert_eq!(made.len(), 2, "{made:?}");
    assert_eq!(made[0].0, "do_action");
    assert_eq!(made[0].1["actionUrl"], "/me/sendMail");
    assert_eq!(
        made[0].1["jsonBody"]["message"]["subject"],
        "Re: the outage"
    );
    assert_eq!(
        made[1],
        (
            "update_entity".to_owned(),
            serde_json::json!({ "entityUrl": "/me/events/AAMk1", "jsonBody": { "subject": "Moved" } }),
            Some("send".to_owned())
        )
    );
    let output = done_output(&states, "send");
    assert!(
        output.contains("1. Action /me/sendMail: done")
            && output.contains("2. Update /me/events/AAMk1: done"),
        "{output}"
    );

    // Rejected: none is made, and the step that would make them is skipped.
    let work = proposing();
    let (signer, _) = sign(false);
    let states = run(work.clone()).await;
    signer.abort();
    assert!(work.changes().is_empty());
    assert!(
        matches!(states["send"], StepState::Skipped { .. }),
        "{:?}",
        states["send"]
    );

    // A change that fails stops the ones after it, and the step fails saying which.
    let work = proposing();
    work.change_replies
        .lock()
        .unwrap()
        .push_back(Err("denied by policy".into()));
    let (signer, _) = sign(true);
    let states = run(work.clone()).await;
    signer.abort();
    assert_eq!(work.changes().len(), 1);
    match &states["send"] {
        StepState::Failed { error, .. } => assert!(
            error.contains("1. Action /me/sendMail: failed: denied by policy")
                && error.contains("Not made, after change 1: 2."),
            "{error}"
        ),
        other => panic!("{other:?}"),
    }
}

/// A plan's sign-off that waits for you is also told by mail once the profile turned notices
/// on: one `do_action` on `/me/sendMail` to the account's own address, holding the sign-off's
/// title, run and step and nothing of the changes it shows, audited with purpose `notice`;
/// the sign-off still waits for your answer, and the changes are made as you sign them off.
/// With notices off there is no notice call. One that fails is told in the activity, is
/// audited, and isn't sent again, and the sign-off still waits for your answer.
#[tokio::test]
async fn a_plans_sign_off_is_also_told_by_mail_once_and_never_what_it_signs_off() {
    use proofkin::profile::WorkIqNotices;
    use proofkin::workiq::audit::{Audit, read};
    use proofkin::workiq::client::{Client, FixedToken};
    use proofkin::workiq::notice::Notifier;

    let mail = r#"{"tool":"do_action","path":"/me/sendMail","body":{"message":{"subject":"Re: the outage","body":{"contentType":"Text","content":"It is fixed."},"toRecipients":[{"emailAddress":{"address":"ana@contoso.com"}}]}}}"#;
    let update = r#"{"tool":"update_entity","path":"/me/events/AAMk1","body":{"subject":"Moved"}}"#;
    let plan = Plan::parse(
        "draft @workiq Draft the replies to the open escalations\n\
         ok after draft approve @me Send these replies?\n\
         send after ok @workiq apply=draft",
    )
    .unwrap();
    let (fake, url) = serve_fake_mcp().await;
    let dir = tempfile::tempdir().unwrap();
    let audit_path = dir.path().join("audit.jsonl");
    let audit = Audit::new(audit_path.clone(), "boss", Default::default());
    let client = Client::new(&url, Arc::new(FixedToken("token".into())))
        .unwrap()
        .with_audit(Arc::new(audit));
    let notices = Arc::new(Notifier::new(
        Arc::new(client),
        WorkIqNotices::Mail,
        "boss@contoso.example",
    ));
    let (boss, mut events) = Node::new("boss").start().await;
    // You sign off what you are asked once Work IQ got `notices` notice calls in all.
    let sign = |notices: usize| {
        let (boss, fake) = (boss.clone(), fake.clone());
        tokio::spawn(async move {
            loop {
                let told = fake.calls.lock().unwrap().len() >= notices;
                if told {
                    for a in boss.approvals() {
                        let _ = boss.answer_approval(a.id, true, "as shown").await;
                    }
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
    };
    // The audit's lines, once there are `n`.
    let audit_lines = |n: usize| {
        let path = audit_path.clone();
        async move {
            timeout(Duration::from_secs(10), async {
                loop {
                    let entries = read(&path).unwrap();
                    if entries.len() >= n {
                        return entries;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .expect("the notice is audited")
        }
    };
    let run = |notices: Option<Arc<Notifier>>| {
        let (boss, plan) = (boss.clone(), plan.clone());
        async move {
            let work = FakeWork::answering(
                Vec::new(),
                serde_json::json!({
                    "answer": format!("Replies:\n\n```m365-changes\n{mail}\n{update}\n```"),
                    "citations": [
                        { "title": "Escalations", "sensitivityLabel": { "displayName": "General" } },
                    ],
                }),
            );
            let ctx = RunContext {
                run_id: Some("r1".into()),
                recipients: proofkin::plan::Recipients {
                    workiq_changes: true,
                    ..Default::default()
                },
                work_context: Some(work.clone()),
                notices,
                ..RunContext::default()
            };
            let states = timeout(
                Duration::from_secs(30),
                run_plan_with(&boss, &SquadStore::default(), &plan, &ctx, |_, _| {}),
            )
            .await
            .unwrap();
            assert!(
                matches!(states["send"], StepState::Done { .. }),
                "{:?}",
                states["send"]
            );
            assert_eq!(work.changes().len(), 2);
        }
    };

    // Off: no notice call.
    let signer = sign(0);
    run(None).await;
    signer.abort();
    assert!(fake.calls.lock().unwrap().is_empty());

    // By mail: one call, to the account's own address, with the title, run and step only.
    let signer = sign(1);
    run(Some(notices.clone())).await;
    signer.abort();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let calls = fake.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 1, "{calls:?}");
    let text = "boss waits for your sign-off of \"Plan step ok\" (run r1, step ok). Answer it \
                in Proofkin's web UI or with /approve in the agent's terminal. This message \
                holds nothing of what you are asked to sign off.";
    assert_eq!(
        calls[0],
        serde_json::json!({
            "name": "do_action",
            "arguments": {
                "actionUrl": "/me/sendMail",
                "jsonBody": {
                    "message": {
                        "subject": "Proofkin: sign-off waiting: Plan step ok",
                        "body": { "contentType": "Text", "content": text },
                        "toRecipients": [
                            { "emailAddress": { "address": "boss@contoso.example" } }
                        ],
                    },
                    "saveToSentItems": false,
                },
            },
        })
    );
    let sent = calls[0].to_string();
    for shown in [
        "ana@contoso.com",
        "Re: the outage",
        "It is fixed",
        "Moved",
        "AAMk1",
        "Send these replies",
        "SHA-256",
    ] {
        assert!(!sent.contains(shown), "the notice holds {shown:?}: {sent}");
    }
    let audited = audit_lines(1).await;
    assert_eq!(audited.len(), 1, "{audited:?}");
    let entry = &audited[0];
    assert_eq!(
        (
            entry.tool.as_str(),
            entry.purpose.as_str(),
            entry.outcome.as_str()
        ),
        ("do_action", "notice", "ok")
    );
    assert_eq!(
        (entry.run.as_deref(), entry.step.as_deref()),
        (Some("r1"), Some("ok"))
    );

    // Refused: the activity says so, the audit has it, it isn't sent again, and the sign-off
    // still waited for your answer.
    fake.calls.lock().unwrap().clear();
    fake.refuse.store(true, Ordering::SeqCst);
    let signer = sign(1);
    run(Some(notices.clone())).await;
    signer.abort();
    let Event::NoticeFailed { title, error } =
        next_matching(&mut events, |e| matches!(e, Event::NoticeFailed { .. })).await
    else {
        unreachable!()
    };
    assert_eq!(title, "Plan step ok");
    assert!(error.contains("denied by policy"), "{error}");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(fake.calls.lock().unwrap().len(), 1);
    let audited = audit_lines(2).await;
    assert_eq!(audited.len(), 2, "{audited:?}");
    assert_eq!(audited[1].purpose, "notice");
    assert_ne!(audited[1].outcome, "ok");
}

#[tokio::test]
async fn a_later_peer_step_is_refused_when_the_answer_exceeds_its_grant_or_has_no_labels() {
    let plan = Plan::parse("a @workiq What changed?\nb after a @partner Write it up").unwrap();

    // Cited General, granted General: sent.
    let (boss, _agents, helper_id) = boss_with_helper_and_partner(&["General"]).await;
    let ctx = workiq_run(helper_id, FakeWork::new(Vec::new()));
    let states = run_plan_with(&boss, &SquadStore::default(), &plan, &ctx, |_, _| {}).await;
    let output = done_output(&states, "b");
    assert!(output.contains("The launch moved to May."), "{output}");

    // Granted a different label: refused.
    let (boss, _agents, helper_id) = boss_with_helper_and_partner(&["Public"]).await;
    let ctx = workiq_run(helper_id, FakeWork::new(Vec::new()));
    let states = run_plan_with(&boss, &SquadStore::default(), &plan, &ctx, |_, _| {}).await;
    match &states["b"] {
        StepState::Failed { error, .. } => {
            assert!(error.contains("grant doesn't include General"), "{error}")
        }
        other => panic!("{other:?}"),
    }

    // A citation without label information is `unknown`, which no grant includes.
    let unlabelled = serde_json::json!({
        "answer": "The launch moved to May.",
        "citations": [{ "title": "Launch plan" }],
    });
    let (boss, _agents, helper_id) = boss_with_helper_and_partner(&["General", "Public"]).await;
    let ctx = workiq_run(helper_id, FakeWork::answering(Vec::new(), unlabelled));
    let states = run_plan_with(&boss, &SquadStore::default(), &plan, &ctx, |_, _| {}).await;
    match &states["b"] {
        StepState::Failed { error, .. } => {
            assert!(error.contains("unknown"), "{error}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(ctx.work_log.records()["a"].labels, ["unknown"]);
}

#[tokio::test]
async fn proofkin_workiq_agents_lists_the_agents_work_iq_offers() {
    use proofkin::workiq::client::{Client, FixedToken};
    use proofkin::workiq::context::agents_listing;

    let fake = Arc::new(FakeMcp::default());
    let app = axum::Router::new()
        .route("/mcp", axum::routing::post(fake_mcp))
        .with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let client = Client::new(&url, Arc::new(FixedToken("token".into()))).unwrap();
    let listing = agents_listing(&client).await.unwrap();
    assert_eq!(
        listing,
        "Sales helper  sales-agent:v1\nHR            hr_agent\n"
    );
}

#[tokio::test]
async fn a_workiq_step_asks_through_the_audited_client_with_agent_id() {
    use proofkin::workiq::audit::Audit;
    use proofkin::workiq::client::{Client, FixedToken};
    use proofkin::workiq::context::ClientWork;

    let fake = Arc::new(FakeMcp::default());
    let app = axum::Router::new()
        .route("/mcp", axum::routing::post(fake_mcp))
        .with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let dir = tempfile::tempdir().unwrap();
    let audit = Arc::new(Audit::new(
        dir.path().join("audit.jsonl"),
        "boss",
        Default::default(),
    ));
    let client = Client::new(&url, Arc::new(FixedToken("token".into())))
        .unwrap()
        .with_audit(audit);

    let (boss, _agents, helper_id) = boss_with_helper_and_partner(&["General"]).await;
    let ctx = workiq_run(helper_id, Arc::new(ClientWork::new(client)));
    let plan = Plan::parse("a @workiq agent=sales-agent:v1 What changed?").unwrap();
    let states = run_plan_with(&boss, &SquadStore::default(), &plan, &ctx, |_, _| {}).await;
    let output = done_output(&states, "a");
    assert!(output.contains("The launch moved to May."), "{output}");

    let calls = fake.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(calls[0]["name"], "ask");
    assert_eq!(calls[0]["arguments"]["question"], "What changed?");
    assert_eq!(calls[0]["arguments"]["agentId"], "sales-agent:v1");
    assert_eq!(ctx.work_log.records()["a"].labels, ["unknown"]);
}

/// A fake Work IQ endpoint on localhost: what it was called with, and its URL.
async fn serve_fake_mcp() -> (Arc<FakeMcp>, String) {
    serve_mcp(FakeMcp::default()).await
}

async fn serve_mcp(fake: FakeMcp) -> (Arc<FakeMcp>, String) {
    let fake = Arc::new(fake);
    let app = axum::Router::new()
        .route("/mcp", axum::routing::post(fake_mcp))
        .with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (fake, url)
}

#[tokio::test]
async fn kb_import_workiq_saves_the_topic_with_its_mark_query_date_and_sources() {
    use proofkin::knowledge::{KnowledgeBase, parse_mark};
    use proofkin::workiq::audit::Audit;
    use proofkin::workiq::client::{Client, FixedToken};
    use proofkin::workiq::context::ClientWork;
    use proofkin::workiq::import::import_knowledge;

    let (fake, url) = serve_fake_mcp().await;
    let dir = tempfile::tempdir().unwrap();
    let audit_path = dir.path().join("audit.jsonl");
    let audit = Arc::new(Audit::new(audit_path.clone(), "boss", Default::default()));
    let client = Client::new(&url, Arc::new(FixedToken("token".into())))
        .unwrap()
        .with_audit(audit);
    let work = ClientWork::new(client);
    let kb = KnowledgeBase::new(dir.path().join("knowledge"));

    let done = import_knowledge(
        &work,
        &kb,
        "launch",
        "What is\nthe launch  budget?",
        Some("files"),
        false,
    )
    .await
    .unwrap();
    assert_eq!((done.kept, done.withheld), (2, 0));
    assert_eq!(done.labels, ["Confidential", "General"]);

    // Searched as a knowledge import, with the --from sources' strategy and capabilities.
    let calls = fake.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(calls[0]["name"], "retrieve");
    assert_eq!(calls[0]["arguments"]["query"], "What is the launch budget?");
    assert_eq!(calls[0]["arguments"]["strategy"], "grounding");
    assert_eq!(
        calls[0]["arguments"]["capabilities"],
        serde_json::json!([{ "name": "OneDriveAndSharePoint" }])
    );
    let audited = std::fs::read_to_string(audit_path).unwrap();
    assert!(audited.contains("knowledge-import"), "{audited}");

    // The topic: front matter first (mark, query, date), then the source note and the results.
    let text = kb.read("launch").unwrap();
    let today = chrono::Local::now().format("%Y-%m-%d");
    assert!(text.starts_with("---\n"), "{text}");
    assert!(
        text.contains("query: What is the launch budget?\n"),
        "{text}"
    );
    assert!(text.contains(&format!("retrieved: {today}\n")), "{text}");
    assert!(text.contains("<!-- source: Work IQ retrieve -->"), "{text}");
    assert!(text.contains("Launch is in May."), "{text}");
    assert!(text.contains("Budget is four million."), "{text}");
    assert_eq!(parse_mark(&text), Some(done.labels.clone()));

    // It works with context=, and its mark goes through the plan check to the steps using it.
    assert!(
        kb.context(&["launch".into()])
            .unwrap()
            .contains("Launch is in May.")
    );
    let plan = Plan::parse("a @helper context=launch Write it up")
        .unwrap()
        .marked(&kb);
    assert_eq!(plan.steps[0].m365.as_deref(), Some(&done.labels[..]));
}

#[tokio::test]
async fn kb_import_workiq_needs_replace_to_overwrite_a_topic_and_refuses_bad_sources() {
    use proofkin::knowledge::KnowledgeBase;
    use proofkin::workiq::import::import_knowledge;

    let dir = tempfile::tempdir().unwrap();
    let kb = KnowledgeBase::new(dir.path().join("knowledge"));
    let work = FakeWork::new(vec![workiq_file("f1", "General", "Launch is in May.", 0.9)]);

    import_knowledge(work.as_ref(), &kb, "launch", "launch", None, false)
        .await
        .unwrap();
    assert_eq!(work.asks()[0].strategy, "copilot", "no --from means all");
    let first = kb.read("launch").unwrap();

    let error = import_knowledge(work.as_ref(), &kb, "launch", "again", None, false)
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("--replace"), "{error:#}");
    assert_eq!(kb.read("launch").unwrap(), first, "left as it was");
    assert_eq!(work.asks().len(), 1, "refused before Work IQ was asked");

    import_knowledge(work.as_ref(), &kb, "launch", "again", None, true)
        .await
        .unwrap();
    assert!(kb.read("launch").unwrap().contains("query: again\n"));

    let error = import_knowledge(work.as_ref(), &kb, "other", "x", Some("email,slack"), false)
        .await
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("doesn't know the source \"slack\""),
        "{error:#}"
    );
    assert_eq!(work.asks().len(), 2, "no search for bad sources");
    assert!(kb.read("other").is_err());
}

#[tokio::test]
async fn kb_import_workiq_writes_nothing_when_everything_was_withheld() {
    use proofkin::knowledge::KnowledgeBase;
    use proofkin::workiq::import::import_knowledge;

    let dir = tempfile::tempdir().unwrap();
    let kb = KnowledgeBase::new(dir.path().join("knowledge"));
    let mut blocked = workiq_file("f1", "General", "Secret.", 0.9);
    blocked["contentProtection"] = serde_json::json!("blocked");
    let mut unprotected = workiq_file("f2", "General", "Unprotected.", 0.8);
    unprotected
        .as_object_mut()
        .unwrap()
        .remove("contentProtection");
    let work = FakeWork::new(vec![blocked, unprotected]);

    let error = import_knowledge(work.as_ref(), &kb, "launch", "launch", None, false)
        .await
        .unwrap_err();
    let error = format!("{error:#}");
    assert!(
        error.contains("2 in all") && error.contains("1 blocked"),
        "{error}"
    );
    assert!(error.contains("1 with no protection status"), "{error}");
    assert!(kb.list().is_empty(), "nothing written");

    let none = FakeWork::new(Vec::new());
    let error = import_knowledge(none.as_ref(), &kb, "launch", "launch", None, false)
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("found nothing"), "{error:#}");
    assert!(kb.list().is_empty());
}

#[tokio::test]
async fn an_imported_result_without_label_information_is_marked_unknown() {
    use proofkin::knowledge::{KnowledgeBase, parse_mark};
    use proofkin::workiq::import::import_knowledge;

    let dir = tempfile::tempdir().unwrap();
    let kb = KnowledgeBase::new(dir.path().join("knowledge"));
    let mut file = workiq_file("f1", "General", "Launch is in May.", 0.9);
    file.as_object_mut().unwrap().remove("sensitivityLabel");
    let work = FakeWork::new(vec![file]);
    let done = import_knowledge(work.as_ref(), &kb, "launch", "launch", None, false)
        .await
        .unwrap();
    assert_eq!(done.labels, ["unknown"]);
    assert_eq!(
        parse_mark(&kb.read("launch").unwrap()),
        Some(vec!["unknown".to_owned()])
    );
}

#[tokio::test]
async fn requests_are_resent_on_a_new_connection_and_run_only_once() {
    let mut worker = Node::new("worker").serving(vec![Provider::Copilot]);
    let mut boss = Node::new("boss");
    mutual(&mut worker, &mut boss);
    let (worker, mut worker_events) = worker.start().await;
    let (boss, _be) = boss.start().await;
    let (low, high) = if boss.id() < worker.id() {
        (&boss, &worker)
    } else {
        (&worker, &boss)
    };
    high.connect(low.addr()).await.unwrap();
    wait_for_peers(&boss, 1).await;
    wait_for_peers(&worker, 1).await;
    let task = tokio::spawn({
        let (boss, id) = (boss.clone(), worker.id());
        async move {
            boss.delegate(id, LlmTask::new(Provider::Copilot, "slow"))
                .await
        }
    });
    next_matching(&mut worker_events, |e| {
        matches!(e, Event::TaskReceived { .. })
    })
    .await;
    // A new connection: the boss sends its pending request again over it.
    low.reconnect(high.id()).await.unwrap();
    let out = timeout(Duration::from_secs(10), task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(out.unwrap(), "slow done");
    // The worker ran it once: no second TaskReceived for the re-sent request.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let mut received = 0;
    while let Ok(e) = worker_events.try_recv() {
        if matches!(e, Event::TaskReceived { .. }) {
            received += 1;
        }
    }
    assert_eq!(received, 0, "the re-sent request was not run again");
}

#[tokio::test]
async fn tasks_delegated_while_peers_dial_each_other_are_not_lost() {
    let mut pending = Vec::new();
    for round in 0..8 {
        let mut worker = Node::new("worker").serving(vec![Provider::Copilot]);
        let mut boss = Node::new("boss");
        mutual(&mut worker, &mut boss);
        let (worker, _we) = worker.start().await;
        let (boss, _be) = boss.start().await;
        // Both dial at once; delegate as soon as the boss has any connection, while
        // the two connections are still converging on one.
        let (a, b) = tokio::join!(boss.connect(worker.addr()), worker.connect(boss.addr()));
        let _ = (a, b);
        timeout(Duration::from_secs(10), async {
            while boss.peers().is_empty() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        let id = worker.id();
        let job = tokio::spawn({
            let boss = boss.clone();
            async move {
                boss.delegate(
                    id,
                    LlmTask::new(Provider::Copilot, format!("round {round}")),
                )
                .await
            }
        });
        pending.push((round, job, boss, worker));
    }
    for (round, job, _boss, _worker) in pending {
        let out = timeout(Duration::from_secs(15), job)
            .await
            .unwrap_or_else(|_| panic!("round {round}: the task was lost"))
            .unwrap()
            .unwrap();
        assert_eq!(out, format!("copilot did: round {round}"));
    }
}

/// A worker says where a delegated task is while it runs: its place in the queue
/// for a slot, then the model working; and how busy it is, counting this boss's.
#[tokio::test]
async fn delegated_tasks_report_their_place_in_the_queue_and_the_worker_its_load() {
    let mut worker = Node::new("worker").serving(vec![Provider::Copilot]);
    let mut boss = Node::new("boss");
    mutual(&mut worker, &mut boss);
    let (worker, _we) = worker.start().await;
    let (boss, _be) = boss.start().await;
    boss.connect(worker.addr()).await.unwrap();
    wait_for_peers(&boss, 1).await;

    // Two slow tasks fill both slots; the third waits for one.
    let slow = || LlmTask::new(Provider::Copilot, "slow");
    let first: Vec<_> = (0..2)
        .map(|_| boss.send_task(worker.id(), slow(), None).unwrap())
        .collect();
    let third = boss.send_task(worker.id(), slow(), None).unwrap();
    assert_eq!(boss.pending_to(worker.id()), 3, "counted as sent at once");
    wait_until(|| {
        boss.peers()[0]
            .load
            .is_some_and(|l| l.running == 2 && l.queued == 1 && l.yours == 3 && l.slots == 2)
    })
    .await;
    let mut seen = Vec::new();
    let delivered = third.finish(|p| seen.push(p)).await.unwrap();
    assert_eq!(delivered.output, "slow done");
    assert_eq!(seen[0].phase, Phase::Queued, "{seen:?}");
    assert_eq!(
        seen[0].text,
        "waiting for a free slot on worker (next in line)"
    );
    let last = seen.last().unwrap();
    assert_eq!(
        (last.phase, last.text.as_str()),
        (Phase::Running, "the model is working")
    );
    for d in first {
        d.finish(|_| {}).await.unwrap();
    }
    wait_until(|| {
        boss.peers()[0]
            .load
            .is_some_and(|l| l.running == 0 && l.queued == 0)
    })
    .await;
}

struct EditStarted {
    workspace: std::path::PathBuf,
    finish: oneshot::Sender<()>,
}

struct HeldEdits(mpsc::UnboundedSender<EditStarted>);

#[async_trait]
impl Executor for HeldEdits {
    fn providers(&self) -> Vec<Provider> {
        vec![Provider::Copilot]
    }

    async fn execute(&self, task: &LlmTask, ctx: &TaskContext) -> anyhow::Result<Ran> {
        if task.prompt == "hold edit" {
            let (finish, held) = oneshot::channel();
            assert!(
                self.0
                    .send(EditStarted {
                        workspace: ctx.workspace.clone(),
                        finish
                    })
                    .is_ok()
            );
            held.await?;
        }
        Ok(Ran::new(ctx.access.to_string(), None))
    }
}

#[tokio::test]
async fn a_folder_limit_queues_edits_but_leaves_the_other_slot_free() {
    let app = git_repo();
    let other = tempfile::tempdir().unwrap();
    let (started, mut edits) = mpsc::unbounded_channel();
    let mut worker = Node::new("worker");
    worker.executor = Some(Arc::new(HeldEdits(started)));
    worker
        .shares
        .add_with("app", app.path(), Access::Full, Default::default(), true)
        .unwrap();
    worker
        .shares
        .set_task_limit("app", parse_task_limit("1").unwrap())
        .unwrap();
    worker
        .shares
        .add("other", other.path(), Access::Full)
        .unwrap();
    let mut boss = Node::new("boss");
    mutual(&mut worker, &mut boss);
    worker.grant("boss", Access::Full, &["app", "other"]);
    let (worker, _we) = worker.start().await;
    let (boss, _be) = boss.start().await;
    boss.connect(worker.addr()).await.unwrap();
    wait_for_peers(&boss, 1).await;

    let deadline = Duration::from_secs(30);
    let send = |share, text| {
        boss.send_task(
            worker.id(),
            LlmTask::new(Provider::Copilot, text).in_share(share),
            None,
        )
        .unwrap()
    };
    let first = send("app", "hold edit");
    let edit = timeout(deadline, edits.recv()).await.unwrap().unwrap();
    assert!(edit.workspace.join(".git").is_file(), "edits in a worktree");
    let second = send("app", "edit next");
    let (progress, mut live) = mpsc::unbounded_channel();
    let second = tokio::spawn(async move {
        second
            .finish(|p| {
                let _ = progress.send(p);
            })
            .await
    });
    let queued = timeout(deadline, live.recv()).await.unwrap().unwrap();
    assert_eq!(queued.phase, Phase::Queued);
    assert_eq!(
        queued.text,
        "waiting for folder app: another task works there (next in line)"
    );

    let profile = Profile::open_in(other.path(), "boss").unwrap();
    let problems = proofkin::plan::preflight(
        &Plan::parse("edit @worker:app update it").unwrap(),
        &boss.peers(),
        &SquadStore::default(),
        &profile.knowledge(),
        &profile.skills(),
        &Default::default(),
    );
    assert!(
        problems.is_empty(),
        "the busy folder still checks out: {problems:?}"
    );

    let abandoned = send("app", "cancel while queued");
    let load = |queued| {
        boss.peers()[0]
            .load
            .is_some_and(|l| l.running == 1 && l.queued == queued)
    };
    wait_until(|| load(2)).await;
    drop(abandoned);
    wait_until(|| load(1)).await;

    let parallel = timeout(deadline, send("other", "edit elsewhere").finish(|_| {}))
        .await
        .unwrap()
        .unwrap();
    assert!(parallel.ok && parallel.output.starts_with("full"));

    // The limit counts effective edit access, not all tasks sent to the folder.
    let mut grant = worker.trusted().get_by_name("boss").unwrap().clone();
    grant.access = Access::ReadOnly;
    worker.allow(grant);
    let reader = timeout(deadline, send("app", "read now").finish(|_| {}))
        .await
        .unwrap()
        .unwrap();
    assert!(reader.ok && reader.output.starts_with("read-only"));
    assert!(!second.is_finished(), "the next edit still waits");

    edit.finish.send(()).unwrap();
    assert!(
        timeout(deadline, first.finish(|_| {}))
            .await
            .unwrap()
            .unwrap()
            .ok
    );
    let second = timeout(deadline, second).await.unwrap().unwrap().unwrap();
    assert!(second.ok && second.output.starts_with("full"));
}

#[tokio::test]
async fn edits_overlap_by_default_and_the_queue_uses_the_current_folder_limit() {
    let app = git_repo();
    let (started, mut edits) = mpsc::unbounded_channel();
    let mut worker = Node::new("worker");
    worker.executor = Some(Arc::new(HeldEdits(started)));
    worker
        .shares
        .add_with("app", app.path(), Access::Full, Default::default(), true)
        .unwrap();
    let mut boss = Node::new("boss");
    mutual(&mut worker, &mut boss);
    worker.grant("boss", Access::Full, &["app"]);
    let (worker, _we) = worker.start().await;
    let (boss, _be) = boss.start().await;
    boss.connect(worker.addr()).await.unwrap();
    wait_for_peers(&boss, 1).await;

    let deadline = Duration::from_secs(30);
    let send = || {
        let task = LlmTask::new(Provider::Copilot, "hold edit").in_share("app");
        boss.send_task(worker.id(), task, None).unwrap()
    };
    let first = send();
    let first_edit = timeout(deadline, edits.recv()).await.unwrap().unwrap();
    let second = send();
    let second_edit = timeout(deadline, edits.recv()).await.unwrap().unwrap();
    assert_ne!(first_edit.workspace, second_edit.workspace);

    let set = |limit| {
        let mut shares = worker.shares();
        shares
            .set_task_limit("app", parse_task_limit(limit).unwrap())
            .unwrap();
        worker.set_shares(shares);
    };
    set("1");
    let third = send();
    first_edit.finish.send(()).unwrap();
    assert!(
        timeout(deadline, first.finish(|_| {}))
            .await
            .unwrap()
            .unwrap()
            .ok
    );
    wait_until(|| {
        boss.peers()[0]
            .load
            .is_some_and(|l| l.running == 1 && l.queued == 1)
    })
    .await;
    set("2");
    let third_edit = timeout(deadline, edits.recv()).await.unwrap().unwrap();
    for (task, edit) in [(second, second_edit), (third, third_edit)] {
        edit.finish.send(()).unwrap();
        assert!(
            timeout(deadline, task.finish(|_| {}))
                .await
                .unwrap()
                .unwrap()
                .ok
        );
    }
}

/// While checks fail and attempts are left, each attempt and check is reported.
#[tokio::test]
async fn delegated_tasks_report_each_attempt_and_check() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("base.txt"), "base\n").unwrap();
    let mut worker = Node::new("worker").serving(vec![Provider::Copilot]);
    let mut boss = Node::new("boss");
    worker
        .shares
        .add_with(
            "app",
            dir.path(),
            Access::Full,
            parse_checks(&[
                format!("base={}", exists_check("base.txt")),
                format!("fixed={}", exists_check("fixed.txt")),
            ])
            .unwrap(),
            false,
        )
        .unwrap();
    mutual(&mut worker, &mut boss);
    worker.grant("boss", Access::Full, &["data", "app"]);
    let (worker, _we) = worker.start().await;
    let (boss, _be) = boss.start().await;
    boss.connect(worker.addr()).await.unwrap();
    wait_for_peers(&boss, 1).await;

    let task = LlmTask::new(Provider::Copilot, "fix-on-retry")
        .in_share("app")
        .with_checks(vec!["base".into(), "fixed".into()], Some(2));
    let mut seen = Vec::new();
    let delivered = boss
        .send_task(worker.id(), task, None)
        .unwrap()
        .finish(|p| seen.push((p.phase, p.text)))
        .await
        .unwrap();
    assert!(delivered.ok && delivered.verified, "{delivered:?}");
    assert!(
        delivered.output.starts_with(
            "first try\n\nAttempt 2 of 2, after these checks failed: fixed\n\nfixed it"
        ),
        "{}",
        delivered.output
    );
    assert_eq!(
        seen,
        [
            (
                Phase::Running,
                "attempt 1 of 2: the model is working".to_owned()
            ),
            (
                Phase::Checking,
                "attempt 1 of 2: running check base (1 of 2)".to_owned()
            ),
            (
                Phase::Checking,
                "attempt 1 of 2: running check fixed (2 of 2); base passed".to_owned()
            ),
            (
                Phase::Running,
                "attempt 2 of 2: the model is working, told what failed (fixed)".to_owned()
            ),
            (
                Phase::Checking,
                "attempt 2 of 2: running check base (1 of 2)".to_owned()
            ),
            (
                Phase::Checking,
                "attempt 2 of 2: running check fixed (2 of 2); base passed".to_owned()
            ),
        ]
    );
}

/// A task's commit has its title as the subject, whatever skills were put in front
/// of the prompt, says who asked and who did it, and references the issues the task
/// is for (only well-formed ones: they come from the peer), which the branch's pull
/// request then closes.
#[tokio::test]
async fn a_task_with_skills_commits_with_its_title_as_the_subject() {
    let repo = git_repo();
    let mut worker = Node::new("worker").serving(vec![Provider::Copilot]);
    let mut boss = Node::new("boss");
    worker
        .shares
        .add_with("app", repo.path(), Access::Full, Default::default(), true)
        .unwrap();
    mutual(&mut worker, &mut boss);
    worker.grant("boss", Access::WorkspaceWrite, &["data", "app"]);
    let (worker, _we) = worker.start().await;
    let (boss, _be) = boss.start().await;
    boss.connect(worker.addr()).await.unwrap();
    wait_for_peers(&boss, 1).await;

    let mut task = LlmTask::new(
        Provider::Copilot,
        "Follow these instructions:\nBe careful.\n---\ncreate note.txt",
    )
    .in_share("app");
    task.title = Some("create note.txt".into());
    task.issues = vec![
        "acme/app#24".into(),
        "acme/app#25\nSigned-off-by: someone".into(),
        "other/lib#3".into(),
    ];
    let done = boss.delegate_verified(worker.id(), task).await.unwrap();
    assert!(done.ok && done.verified, "{}", done.output);
    let git_record = done.evidence.as_ref().unwrap().git.clone().unwrap();
    let log = std::process::Command::new("git")
        .arg("-C")
        .arg(repo.path())
        .args(["log", "-1", "--format=%B", &git_record.branch])
        .output()
        .unwrap();
    assert!(log.status.success());
    let message = String::from_utf8_lossy(&log.stdout).replace("\r\n", "\n");
    let message = message.trim_end();
    assert!(message.starts_with("create note.txt\n\nTask "), "{message}");
    assert!(
        message.ends_with(
            " from boss, done by worker with Proofkin.\n\nRefs: acme/app#24\nRefs: other/lib#3"
        ),
        "{message}"
    );
    let linked = proofkin::github::Repo {
        name: "app".into(),
        slug: "acme/app".into(),
        host: "github.com".into(),
        path: repo.path().to_path_buf(),
        branch: "main".into(),
    };
    let branches = proofkin::github::task_branches(&linked).await.unwrap();
    let branch = branches
        .iter()
        .find(|b| b.name == git_record.branch)
        .unwrap();
    assert_eq!(branch.issues, ["acme/app#24", "other/lib#3"]);
    assert_eq!(branch.subject, "create note.txt");
    assert_eq!(
        proofkin::tracker::closing_lines(&branch.issues, &linked.slug),
        "Closes #24\nCloses other/lib#3\n"
    );

    // Given another subject (the title of the issue a step works on), it is the commit's
    // subject and the title goes under it; it comes from the peer, so only its first line.
    let mut task = LlmTask::new(Provider::Copilot, "create other.txt").in_share("app");
    task.title = Some("create other.txt".into());
    task.subject = Some("Add the other page\nSigned-off-by: someone".into());
    let done = boss.delegate_verified(worker.id(), task).await.unwrap();
    let branch = done.evidence.unwrap().git.unwrap().branch;
    let log = std::process::Command::new("git")
        .arg("-C")
        .arg(repo.path())
        .args(["log", "-1", "--format=%B", &branch])
        .output()
        .unwrap();
    let message = String::from_utf8_lossy(&log.stdout).replace("\r\n", "\n");
    assert!(
        message.starts_with("Add the other page\n\ncreate other.txt\n\nTask ")
            && !message.contains("Signed-off-by"),
        "{message}"
    );
}

/// A dropped connection isn't the end of a task: while the peer comes back within
/// the task grace, the task waits (saying so), then its result arrives.
#[tokio::test]
async fn tasks_survive_a_dropped_connection_when_the_peer_comes_back_in_time() {
    let mut worker = Node::new("worker").serving(vec![Provider::Copilot]);
    let mut boss = Node::new("boss");
    worker.grace = Duration::from_secs(20);
    boss.grace = Duration::from_secs(20);
    mutual(&mut worker, &mut boss);
    let (worker, _we) = worker.start().await;
    let (boss, mut boss_events) = boss.start().await;
    boss.connect(worker.addr()).await.unwrap();
    wait_for_peers(&boss, 1).await;

    let delegation = boss
        .send_task(worker.id(), LlmTask::new(Provider::Copilot, "slow"), None)
        .unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let result = tokio::spawn(delegation.finish(move |p| log.lock().unwrap().push(p.phase)));
    assert!(boss.disconnect(worker.id()));
    next_matching(&mut boss_events, |e| {
        matches!(e, Event::PeerDisconnected { .. })
    })
    .await;
    wait_until(|| seen.lock().unwrap().contains(&Phase::Reconnecting)).await;
    boss.connect(worker.addr()).await.unwrap();
    let delivered = timeout(Duration::from_secs(15), result)
        .await
        .expect("the result arrives once it is back")
        .unwrap()
        .unwrap();
    assert!(delivered.ok && delivered.verified, "{delivered:?}");
    assert_eq!(delivered.output, "slow done");
}

/// A squad step its member never runs (here it refuses: over its hourly limit) goes
/// to another member, which says why it came.
#[tokio::test]
async fn squad_steps_move_to_another_member_when_theirs_refuses_them() {
    let mut boss = Node::new("boss");
    let mut m1 = Node::new("m1").serving(vec![Provider::Copilot]);
    let mut m2 = Node::new("m2").serving(vec![Provider::Copilot]);
    m1.trusts(&boss, Some(0));
    boss.trusts(&m1, Some(100));
    mutual(&mut m2, &mut boss);
    let (m1, _e1) = m1.start().await;
    let (m2, _e2) = m2.start().await;
    let (boss, _be) = boss.start().await;
    boss.connect(m1.addr()).await.unwrap();
    boss.connect(m2.addr()).await.unwrap();
    wait_for_peers(&boss, 2).await;
    let mut squads = SquadStore::default();
    squads.set("devs", vec!["m1".into(), "m2".into()]).unwrap();
    let plan = Plan::parse("work #devs do it").unwrap();

    let mut log = Vec::new();
    let states = run_plan(&boss, &squads, &plan, |_, s| log.push(s.clone())).await;
    assert!(
        matches!(&states["work"], StepState::Done { peer, .. } if peer == "m2"),
        "{:?}",
        states["work"]
    );
    // It went to m1 first (the tie goes by name), then moved, saying why.
    let moved = log
        .iter()
        .find_map(|s| match s {
            StepState::Running {
                peer,
                progress: Some(p),
            } if peer == "m2" => Some(p.text.clone()),
            _ => None,
        })
        .expect("it says it moved");
    assert_eq!(
        moved,
        "moved here: m1: m1 refused the task: boss reached its limit of 0 tasks per hour"
    );
}

/// A squad step goes to the member least busy with everyone's tasks (as members
/// report them), not only with the boss's own.
#[tokio::test]
async fn squad_steps_go_to_the_member_least_busy_with_everyones_tasks() {
    let mut boss = Node::new("boss");
    let mut other = Node::new("other");
    let mut m1 = Node::new("m1").serving(vec![Provider::Copilot]);
    let mut m2 = Node::new("m2").serving(vec![Provider::Copilot]);
    mutual(&mut m1, &mut boss);
    mutual(&mut m2, &mut boss);
    mutual(&mut m1, &mut other);
    let (m1, _e1) = m1.start().await;
    let (m2, _e2) = m2.start().await;
    let (boss, _be) = boss.start().await;
    let (other, _oe) = other.start().await;
    boss.connect(m1.addr()).await.unwrap();
    boss.connect(m2.addr()).await.unwrap();
    other.connect(m1.addr()).await.unwrap();
    wait_for_peers(&boss, 2).await;
    wait_for_peers(&other, 1).await;
    // Another peer keeps m1 busy, and m1 tells the boss.
    let _busy = other
        .send_task(m1.id(), LlmTask::new(Provider::Copilot, "hang"), None)
        .unwrap();
    wait_until(|| {
        boss.peers()
            .iter()
            .any(|p| p.name == "m1" && p.load.is_some_and(|l| l.running == 1 && l.yours == 0))
    })
    .await;
    let mut squads = SquadStore::default();
    squads.set("devs", vec!["m1".into(), "m2".into()]).unwrap();
    let plan = Plan::parse("work #devs do it").unwrap();
    let states = run_plan(&boss, &squads, &plan, |_, _| {}).await;
    assert!(
        matches!(&states["work"], StepState::Done { peer, .. } if peer == "m2"),
        "m1 (first by name) is busy with another peer's task: {:?}",
        states["work"]
    );
}

/// `timeout=` cancels a step that takes too long (on its peer too).
#[tokio::test]
async fn steps_that_take_longer_than_their_timeout_are_cancelled() {
    let worker = Node::new("worker").serving(vec![Provider::Copilot]);
    let stopped = worker.stopped.clone();
    let (boss, _workers) = boss_with(vec![worker]).await;
    let plan = Plan::parse(
        "slowpoke @worker timeout=1s hang\n\
         next after slowpoke @worker never runs",
    )
    .unwrap();
    let states = timeout(
        Duration::from_secs(10),
        run_plan(&boss, &SquadStore::default(), &plan, |_, _| {}),
    )
    .await
    .unwrap();
    assert!(
        matches!(&states["slowpoke"], StepState::Failed { error, .. } if error == "cancelled: it took longer than its timeout=1s"),
        "{:?}",
        states["slowpoke"]
    );
    assert!(matches!(&states["next"], StepState::Skipped { .. }));
    wait_stopped(&stopped, 1).await;
}

/// A team step hands work down a level: the lead's agent runs its team plan on its
/// own member (whom the boss doesn't know), reports the team's steps as they move
/// on, and signs its answer with the team run inside, so the boss's claim is proven
/// through the whole tree, which re-verifies offline. Work never goes back up.
#[tokio::test]
async fn team_steps_hand_work_down_a_level_and_prove_it_through_the_whole_tree() {
    let home = tempfile::tempdir().unwrap();
    let lead_profile = proofkin::Profile::open_in(home.path(), "lead").unwrap();
    let app = tempfile::tempdir().unwrap();
    let code = tempfile::tempdir().unwrap();
    std::fs::write(code.path().join("ok.txt"), "ok").unwrap();
    let mut boss = Node::new("boss").serving(vec![Provider::Copilot]);
    let mut lead = Node::new("lead").serving(vec![Provider::Copilot]);
    let mut member = Node::new("member").serving(vec![Provider::Copilot]);
    lead.team = Some(Arc::new(proofkin::team::TeamRunner::new(
        lead_profile.clone(),
    )));
    lead.shares.add("app", app.path(), Access::Full).unwrap();
    lead.shares
        .set_team(
            "app",
            Some(
                "claim: T-1 {task} passes the member's check\n\
                 build @member:code verify=ok claims=T-1 {task}"
                    .into(),
            ),
        )
        .unwrap();
    lead.shares.add("loop", app.path(), Access::Full).unwrap();
    lead.shares
        .set_team("loop", Some("back @boss {task}".into()))
        .unwrap();
    member
        .shares
        .add_with(
            "code",
            code.path(),
            Access::Full,
            parse_checks(&[format!("ok={}", exists_check("ok.txt"))]).unwrap(),
            false,
        )
        .unwrap();
    mutual(&mut boss, &mut lead);
    mutual(&mut lead, &mut member);
    let audit_path = home.path().join("lead-audit.jsonl");
    lead.audit = Some(Arc::new(AuditLog::new(audit_path.clone())));
    lead.grant("boss", Access::WorkspaceWrite, &["data", "app", "loop"]);
    member.grant("lead", Access::Full, &["data", "code"]);
    let (member, _me) = member.start().await;
    let (lead, mut lead_events) = lead.start().await;
    let (boss, _be) = boss.start().await;
    lead.connect(member.addr()).await.unwrap();
    boss.connect(lead.addr()).await.unwrap();
    wait_for_peers(&lead, 2).await;
    wait_for_peers(&boss, 1).await;
    wait_until(|| {
        boss.peers()[0]
            .shares
            .iter()
            .any(|s| s.name == "app" && s.team)
    })
    .await;

    let plan = Plan::parse(
        "claim: C-1 The team built it\n\
         build team @lead:app claims=C-1 make the thing",
    )
    .unwrap();
    let ctx = RunContext {
        run_id: Some("boss-run".into()),
        ..RunContext::default()
    };
    let mut seen = Vec::new();
    let states = run_plan_with(&boss, &SquadStore::default(), &plan, &ctx, |_, s| {
        seen.push(s.clone())
    })
    .await;
    let StepState::Done {
        peer,
        evidence: Some(e),
        verified,
        output,
        ..
    } = &states["build"]
    else {
        panic!("{:?}", states["build"]);
    };
    assert_eq!(peer, "lead");
    assert!(*verified, "the whole tree checks out");
    let run = e.team.as_ref().expect("the team run comes with it");
    assert_eq!(run.boss, "lead");
    assert!(run.passed(), "{}", run.verdict_line());
    assert!(
        output.starts_with(&format!("Team run {} by lead: PASS", run.id)),
        "{output}"
    );
    // The member did the work, for the lead, and signed it.
    let StepState::Done {
        peer,
        evidence: Some(sub),
        verified,
        ..
    } = &run.steps[0].state
    else {
        panic!("{:?}", run.steps[0].state);
    };
    assert_eq!(peer, "member");
    assert!(*verified);
    assert_eq!(sub.requester_id, lead.id().to_string());
    // While it ran, the boss saw the team's steps.
    assert!(
        seen.iter().any(|s| matches!(s,
            StepState::Running { progress: Some(p), .. }
                if p.phase == Phase::Team
                    && p.steps.iter().any(|t| t.id == "build" && t.target == "member"))),
        "{seen:?}"
    );
    // The boss's run proves C-1 through the team run, and re-verifies offline.
    let mut record = RunRecord::new("boss", &boss.id().to_string(), "", &plan, &states, 0);
    record.peer_ids = proofkin::runs::peer_ids(&boss);
    assert!(record.passed(), "{}", record.verdict_line());
    assert!(
        record.verify(None).iter().all(|(_, r)| r.is_ok()),
        "{:?}",
        record.verify(None)
    );
    // A team run changed after the lead signed it doesn't check out.
    let mut forged = (**run).clone();
    if let StepState::Done { output, .. } = &mut forged.steps[0].state {
        output.push_str(" and more");
    }
    assert!(proofkin::runs::verify_team(&forged, &lead.id().to_string()).is_err());
    // The lead kept the team run, and showed it as it went.
    assert_eq!(
        RunRecord::list(&lead_profile.runs_dir()).unwrap()[0].id,
        run.id
    );
    next_matching(&mut lead_events, |e| {
        matches!(e, Event::TaskReceived { team: true, .. })
    })
    .await;
    next_matching(
        &mut lead_events,
        |e| matches!(e, Event::TeamRun { from, .. } if from == "boss"),
    )
    .await;
    // Its audit log has it as a team step (no model of its own ran it).
    let audited = AuditLog::read(&audit_path, 10).unwrap();
    assert!(
        audited.len() == 1 && audited[0].team && audited[0].outcome == Outcome::Done,
        "{audited:?}"
    );

    // Work never goes back up: a team plan naming the boss fails there.
    let back = Plan::parse("again team @lead:loop do it").unwrap();
    let states = run_plan(&boss, &SquadStore::default(), &back, |_, _| {}).await;
    let StepState::Failed { error, .. } = &states["again"] else {
        panic!("{:?}", states["again"]);
    };
    assert!(
        error.contains("boss handed this work down, so it isn't given back to it"),
        "{error}"
    );
}

/// A lead whose folder leaves team plans to its model: for each team step the model
/// writes the plan (read-only in the folder, on the owner's default model and effort),
/// is told what is wrong with a plan that doesn't check out and writes it again, and the
/// owner approves it before it runs, or asks for changes (twice at most), which the
/// model makes. The team run's plan and the lead's answer say who wrote it. A plan the
/// owner rejects, or that the model gets wrong three times, fails the step without a
/// team run; a lead that runs no models refuses it.
#[tokio::test]
async fn a_leads_model_writes_the_team_plan_of_each_team_step_and_its_owner_approves_it() {
    use proofkin::agent::Answer;
    let home = tempfile::tempdir().unwrap();
    let lead_profile = proofkin::Profile::open_in(home.path(), "lead").unwrap();
    let solo_profile = proofkin::Profile::open_in(home.path(), "solo").unwrap();
    let app = tempfile::tempdir().unwrap();
    std::fs::write(app.path().join("notes.txt"), "the member builds it").unwrap();
    let code = tempfile::tempdir().unwrap();
    std::fs::write(code.path().join("ok.txt"), "ok").unwrap();
    let mut boss = Node::new("boss").serving(vec![Provider::Copilot]);
    let mut lead = Node::new("lead").serving(vec![Provider::Copilot]);
    let mut solo = Node::new("solo");
    let mut member = Node::new("member").serving(vec![Provider::Copilot]);
    lead.models = [(
        Provider::Copilot,
        proofkin::models::ModelChoice {
            default: Some("planner-1".into()),
            effort: Some("high".into()),
            ..Default::default()
        },
    )]
    .into();
    for (node, profile) in [(&mut lead, &lead_profile), (&mut solo, &solo_profile)] {
        node.team = Some(Arc::new(proofkin::team::TeamRunner::new(profile.clone())));
        node.shares.add("app", app.path(), Access::Full).unwrap();
        node.shares.set_team_model("app", true).unwrap();
        node.trusts(&boss, Some(100));
        boss.trusts(node, Some(100));
        node.grant("boss", Access::WorkspaceWrite, &["data", "app"]);
    }
    member
        .shares
        .add_with(
            "code",
            code.path(),
            Access::Full,
            parse_checks(&[format!("ok={}", exists_check("ok.txt"))]).unwrap(),
            false,
        )
        .unwrap();
    mutual(&mut lead, &mut member);
    member.grant("lead", Access::Full, &["data", "code"]);
    let (member, _me) = member.start().await;
    let (lead, _le) = lead.start().await;
    let (solo, _se) = solo.start().await;
    let (boss, _be) = boss.start().await;
    lead.connect(member.addr()).await.unwrap();
    boss.connect(lead.addr()).await.unwrap();
    boss.connect(solo.addr()).await.unwrap();
    wait_for_peers(&lead, 2).await;
    wait_for_peers(&boss, 2).await;
    wait_until(|| {
        boss.peers()
            .iter()
            .filter(|p| p.shares.iter().any(|s| s.name == "app" && s.team))
            .count()
            == 2
    })
    .await;
    // The lead's owner answers the next requests to approve a team plan, in turn.
    let answer = |answers: Vec<(Answer, &'static str)>| {
        let lead = lead.clone();
        tokio::spawn(async move {
            let mut seen = Vec::new();
            for (how, note) in answers {
                let request = timeout(Duration::from_secs(30), async {
                    loop {
                        if let Some(a) = lead.approvals().pop() {
                            return a;
                        }
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                })
                .await
                .expect("the lead's owner is asked");
                lead.answer(request.id, how, note).await.unwrap();
                seen.push(request);
            }
            seen
        })
    };

    // Its first plan names a peer the lead doesn't have: told so, the model writes one
    // that checks out. Its owner asks for changes twice, which it makes (each time
    // after a plan that doesn't check out), and approves it once they may only approve
    // or reject it.
    let yes = answer(vec![
        (Answer::Changes, "say who checks it"),
        (Answer::Changes, "and when"),
        (Answer::Approve, "go ahead"),
    ]);
    let plan = Plan::parse(
        "claim: C-1 The team built it\n\
         build team @lead:app claims=C-1 make the thing plan-badly",
    )
    .unwrap();
    let ctx = RunContext {
        run_id: Some("boss-run".into()),
        ..RunContext::default()
    };
    let mut seen = Vec::new();
    let states = run_plan_with(&boss, &SquadStore::default(), &plan, &ctx, |_, s| {
        if let StepState::Running {
            progress: Some(p), ..
        } = s
        {
            seen.push(p.text.clone());
        }
    })
    .await;
    let requests = yes.await.unwrap();
    assert!(
        requests
            .iter()
            .all(|r| r.title == "Run this team plan for boss?"),
        "{requests:?}"
    );
    let offered: Vec<bool> = requests.iter().map(|r| r.changes).collect();
    assert_eq!(offered, [true, true, false]);
    assert!(
        requests[0]
            .text
            .starts_with("boss handed a team step to 'app': make the thing plan-badly\n")
            && requests[0]
                .text
                .contains("\nbuild @member:code verify=ok claims=T-1 {task}\n"),
        "{}",
        requests[0].text
    );
    for (round, (request, note)) in requests[1..]
        .iter()
        .zip(["say who checks it", "and when"])
        .enumerate()
    {
        let text = &request.text;
        assert!(
            text.starts_with(&format!("**Asked again** (round {} of changes)", round + 1))
                && text.contains(&format!("\n\n> {note}\n\n---\n\nboss handed a team step"))
                && text.contains(&format!("\n// changed: {note}\nbuild @member:code")),
            "{text}"
        );
    }
    let StepState::Done {
        peer,
        output,
        evidence: Some(e),
        verified,
        ..
    } = &states["build"]
    else {
        panic!("{:?}", states["build"]);
    };
    assert_eq!(peer, "lead");
    assert!(*verified, "the whole tree checks out");
    let run = e.team.as_ref().expect("the team run comes with it");
    assert!(run.passed(), "{}", run.verdict_line());
    let by = "This team plan was written by lead's model (copilot, planner-1) for boss's \
              team step, and approved by lead after 2 rounds of changes";
    assert_eq!(
        run.plan_text,
        format!(
            "// {by}\n\
             claim: T-1 make the thing plan-badly passes the member's check\n\
             // planned on planner-1 at high, with notes: the member builds it\n\
             // changed: and when\n\
             build @member:code verify=ok claims=T-1 make the thing plan-badly"
        )
    );
    assert!(
        output.trim_end().ends_with(&format!("\n\n{by}.")),
        "{output}"
    );
    for text in [
        "lead's model is writing the team plan",
        "lead's model is writing the team plan again, told what was wrong with it",
        "waiting for lead to approve the team plan",
        "lead's model is writing the team plan again, with the changes lead asked for",
    ] {
        assert!(seen.iter().any(|s| s == text), "{text}: {seen:?}");
    }

    // A plan its owner rejects fails the step, saying why.
    let no = answer(vec![(Answer::Reject, "not now")]);
    let plan = Plan::parse("build team @lead:app make the thing").unwrap();
    let states = run_plan(&boss, &SquadStore::default(), &plan, |_, _| {}).await;
    no.await.unwrap();
    assert!(
        matches!(&states["build"], StepState::Failed { error, .. }
            if error.contains("lead didn't approve the team plan: not now")),
        "{:?}",
        states["build"]
    );
    // So does one its model gets wrong three times, which its owner isn't asked about.
    let plan = Plan::parse("build team @lead:app make the thing plan-badly-always").unwrap();
    let states = run_plan(&boss, &SquadStore::default(), &plan, |_, _| {}).await;
    assert!(
        matches!(&states["build"], StepState::Failed { error, .. }
            if error.contains("lead's model couldn't write a team plan that checks out")
                && error.contains("step work: nobody3 is not connected")),
        "{:?}",
        states["build"]
    );
    assert!(lead.approvals().is_empty());
    assert_eq!(RunRecord::list(&lead_profile.runs_dir()).unwrap().len(), 1);
    // A lead that runs no models can't write one.
    let plan = Plan::parse("build team @solo:app make the thing").unwrap();
    let states = run_plan(&boss, &SquadStore::default(), &plan, |_, _| {}).await;
    assert!(
        matches!(&states["build"], StepState::Failed { error, .. }
            if error.contains("solo runs no models, so it can't write a team plan for 'app'")),
        "{:?}",
        states["build"]
    );
}

/// A worker's memory in a knowledge folder of its own: `topics` (name, text and, for one
/// made from Microsoft 365 content, its labels) put in front of every task, and its
/// lessons topic.
fn worker_memory(
    name: &str,
    topics: &[(&str, &str, &[&str])],
    lessons: Option<&str>,
) -> (
    tempfile::TempDir,
    Arc<proofkin::factory::memory::WorkerMemory>,
) {
    use proofkin::factory::memory::{Memory, WorkerMemory};
    use proofkin::knowledge::{KnowledgeBase, with_mark};
    let dir = tempfile::tempdir().unwrap();
    let kb = KnowledgeBase::new(dir.path());
    for (topic, text, labels) in topics {
        let text = if labels.is_empty() {
            (*text).to_owned()
        } else {
            let labels: Vec<String> = labels.iter().map(|l| (*l).to_owned()).collect();
            with_mark(text, &labels)
        };
        kb.write(topic, &text, "").unwrap();
    }
    let settings = Memory {
        context: topics
            .iter()
            .map(|(topic, ..)| (*topic).to_owned())
            .collect(),
        lessons: lessons.map(Into::into),
        retention_days: None,
    };
    let memory = WorkerMemory::new(name, KnowledgeBase::new(dir.path()), settings);
    (dir, Arc::new(memory))
}

/// A digital worker's memory goes in front of each task it serves for a requester who may
/// have it: its owner (known by its key, not its name) all of it, a peer the topics its
/// grant's labels cover. The signed evidence carries the labels of what it held, so the
/// requester marks what it does with the result: a later step goes only to a peer granted
/// them, which its requester tells what the task holds, and what it saves is marked.
#[tokio::test]
async fn a_workers_memory_goes_in_front_of_tasks_for_whoever_may_have_it() {
    use proofkin::knowledge::{KnowledgeBase, parse_mark};
    use proofkin::{M365Grant, TrustedPeer};

    let (_knowledge, memory) = worker_memory(
        "analyst",
        &[
            ("glossary", "EBITDA: earnings before interest.", &[]),
            ("q3-board", "Revenue was 12.", &["Confidential"]),
        ],
        None,
    );
    let mut analyst = Node::new("analyst").serving(vec![Provider::Copilot]);
    let mut boss = Node::new("boss");
    analyst.owner = Some("boss".into());
    analyst.owner_id = Some(boss.id());
    analyst.memory = Some(memory);
    let mut partner = Node::new("partner");
    let mut reviewer = Node::new("reviewer").serving(vec![Provider::Copilot]);
    mutual(&mut analyst, &mut boss);
    mutual(&mut analyst, &mut partner);
    mutual(&mut boss, &mut reviewer);
    let (partner_id, reviewer_id) = (partner.id(), reviewer.id());
    let (analyst, _ae) = analyst.start().await;
    let (boss, _be) = boss.start().await;
    let (partner, _pe) = partner.start().await;
    let (reviewer, _re) = reviewer.start().await;
    boss.connect(analyst.addr()).await.unwrap();
    boss.connect(reviewer.addr()).await.unwrap();
    partner.connect(analyst.addr()).await.unwrap();
    wait_for_peers(&boss, 2).await;
    wait_for_peers(&partner, 1).await;

    // Its owner gets all of it; the signed evidence says what it held, never its text.
    let plan =
        Plan::parse("a @analyst save=q3-summary Summarize Q3\nb after a @reviewer Review it")
            .unwrap();
    let states = run_plan(&boss, &SquadStore::default(), &plan, |_, _| {}).await;
    let StepState::Done {
        output,
        evidence: Some(e),
        verified: true,
        ..
    } = &states["a"]
    else {
        panic!("{:?}", states["a"]);
    };
    for part in [
        "copilot did: ## Rule for external content",
        "### Knowledge: glossary",
        "EBITDA: earnings before interest.",
        "### Knowledge: q3-board",
        "Revenue was 12.",
        "**Knowledge** from analyst's memory: 2 topics (Confidential)",
    ] {
        assert!(output.contains(part), "{part:?} isn't in:\n{output}");
    }
    assert_eq!(e.context.len(), 1, "{:?}", e.context);
    assert_eq!(
        (e.context[0].source.as_str(), e.context[0].results),
        ("knowledge", 2)
    );
    assert_eq!(e.context[0].labels, ["Confidential"]);
    assert_eq!(
        e.prompt_sha256,
        proofkin::evidence::sha256_hex(b"Summarize Q3")
    );
    // The result holds Confidential content: it goes no further than the grants allow.
    let StepState::Failed { error, .. } = &states["b"] else {
        panic!("{:?}", states["b"]);
    };
    assert!(
        error.contains(
            "step 'b' gets Microsoft 365 content (labels: Confidential) that reviewer may not receive"
        ) && error.contains("--m365 Confidential"),
        "{error}"
    );
    let home = tempfile::tempdir().unwrap();
    let kb = KnowledgeBase::new(home.path());
    assert_eq!(
        proofkin::plan::save_knowledge(&kb, &plan, &states, "run-1"),
        ["q3-summary"]
    );
    assert_eq!(
        parse_mark(&kb.read("q3-summary").unwrap()),
        Some(vec!["Confidential".to_owned()])
    );
    // Granted, the reviewer gets it, told what it holds; its signed evidence says so too.
    boss.allow(TrustedPeer {
        name: "reviewer".into(),
        id: reviewer_id,
        addrs: BTreeSet::new(),
        can_delegate: true,
        tasks_per_hour: 100,
        access: Access::ReadOnly,
        shares: vec![DATA_SHARE.to_owned()],
        m365: Some(M365Grant {
            labels: vec!["Confidential".into()],
            tools: false,
        }),
        github: None,
        agents: Vec::new(),
    });
    let states = run_plan(&boss, &SquadStore::default(), &plan, |_, _| {}).await;
    let StepState::Done {
        evidence: Some(e),
        output,
        ..
    } = &states["b"]
    else {
        panic!("{:?}", states["b"]);
    };
    let marked = e
        .context
        .iter()
        .find(|c| c.source == "marked")
        .expect(output);
    assert_eq!(marked.labels, ["Confidential"]);
    assert!(
        output
            .contains("**Microsoft 365 content** its requester marked in the task (Confidential)"),
        "{output}"
    );

    // A peer without a grant gets what isn't Microsoft 365 content.
    let task = LlmTask::new(Provider::Copilot, "Summarize Q3");
    let asked = partner
        .delegate_verified(analyst.id(), task.clone())
        .await
        .unwrap();
    assert!(asked.verified, "{}", asked.output);
    assert!(
        asked.output.contains("EBITDA") && !asked.output.contains("Revenue was 12."),
        "{}",
        asked.output
    );
    // Knowledge without labels is in its signed output only: requesters from before work
    // context verify its record as before.
    assert!(
        asked
            .output
            .contains("**Knowledge** from analyst's memory: 1 topic, SHA-256"),
        "{}",
        asked.output
    );
    let e = asked.evidence.unwrap();
    assert!(e.context.is_empty(), "{:?}", e.context);

    // Granted the label (names compare without case), it gets that topic too.
    let entry = |name: &str, m365: Option<M365Grant>| TrustedPeer {
        name: name.into(),
        id: partner_id,
        addrs: BTreeSet::new(),
        can_delegate: true,
        tasks_per_hour: 100,
        access: Access::ReadOnly,
        shares: vec![DATA_SHARE.to_owned()],
        m365,
        github: None,
        agents: Vec::new(),
    };
    analyst.allow(entry(
        "partner",
        Some(M365Grant {
            labels: vec!["confidential".into()],
            tools: false,
        }),
    ));
    let asked = partner
        .delegate_verified(analyst.id(), task.clone())
        .await
        .unwrap();
    assert!(asked.output.contains("Revenue was 12."), "{}", asked.output);
    assert_eq!(asked.evidence.unwrap().context[0].labels, ["Confidential"]);

    // Allowlisted under its owner's name, a peer still isn't its owner.
    analyst.allow(entry("boss", None));
    let asked = partner.delegate_verified(analyst.id(), task).await.unwrap();
    assert!(
        asked.output.contains("EBITDA") && !asked.output.contains("Revenue was 12."),
        "{}",
        asked.output
    );
    let _ = reviewer;
}

/// A task its checks verified teaches its worker a lesson, kept once in its lessons topic
/// and marked with the labels of what the task was given (its memory's, and what its
/// requester says the task holds); one whose checks failed teaches it nothing.
#[tokio::test]
async fn a_task_its_checks_verified_teaches_its_worker_a_lesson_once() {
    use proofkin::knowledge::{KnowledgeBase, parse_mark};

    let (knowledge, memory) = worker_memory(
        "analyst",
        &[("q3-board", "Revenue was 12.", &["Confidential"])],
        Some("analyst-lessons"),
    );
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("ok.txt"), "ok").unwrap();
    let mut analyst = Node::new("analyst").serving(vec![Provider::Copilot]);
    let mut boss = Node::new("boss");
    analyst.owner = Some("boss".into());
    analyst.owner_id = Some(boss.id());
    analyst.memory = Some(memory);
    let checks = parse_checks(&[
        format!("ok={}", exists_check("ok.txt")),
        format!("missing={}", exists_check("missing.txt")),
    ])
    .unwrap();
    analyst
        .shares
        .add_with("repo", repo.path(), Access::Full, checks, false)
        .unwrap();
    mutual(&mut analyst, &mut boss);
    analyst.grant("boss", Access::Full, &["data", "repo"]);
    let (analyst, _ae) = analyst.start().await;
    let (boss, _be) = boss.start().await;
    boss.connect(analyst.addr()).await.unwrap();
    wait_for_peers(&boss, 1).await;
    let kb = KnowledgeBase::new(knowledge.path());
    let lessons = || kb.read("analyst-lessons").unwrap_or_default();
    let task = |prompt: &str, check: &str| {
        LlmTask::new(Provider::Copilot, prompt)
            .in_share("repo")
            .with_checks(vec![check.into()], None)
    };

    let failed = boss
        .delegate_verified(analyst.id(), task("Summarize Q3", "missing"))
        .await
        .unwrap();
    assert!(!failed.ok && failed.verified, "{}", failed.output);
    assert_eq!(
        lessons(),
        "",
        "a result its checks didn't verify teaches nothing"
    );

    let passed = boss
        .delegate_verified(analyst.id(), task("Summarize Q3", "ok"))
        .await
        .unwrap();
    assert!(passed.ok && passed.verified, "{}", passed.output);
    let text = lessons();
    assert_eq!(text.matches("<!-- lesson sha256=").count(), 1, "{text}");
    for part in [
        "### Summarize Q3",
        "From boss, ",
        "its checks passed: ok.",
        "copilot did: ",
        "Revenue was 12.",
    ] {
        assert!(text.contains(part), "{part:?} isn't in:\n{text}");
    }
    assert_eq!(parse_mark(&text), Some(vec!["Confidential".to_owned()]));

    boss.delegate_verified(analyst.id(), task("Summarize Q3", "ok"))
        .await
        .unwrap();
    assert_eq!(lessons(), text, "the same lesson is kept once");

    // A task its requester says holds HR content: its lesson is marked HR too.
    let mut hr = task("Summarize the pay bands", "ok");
    hr.m365 = vec!["HR".into()];
    let passed = boss.delegate_verified(analyst.id(), hr).await.unwrap();
    assert!(passed.ok, "{}", passed.output);
    let e = passed.evidence.unwrap();
    assert!(
        e.context
            .iter()
            .any(|c| c.source == "marked" && c.labels == ["HR"]),
        "{:?}",
        e.context
    );
    let text = lessons();
    assert_eq!(text.matches("<!-- lesson sha256=").count(), 2, "{text}");
    assert_eq!(
        parse_mark(&text),
        Some(vec!["Confidential".to_owned(), "HR".to_owned()])
    );
}

/// A lead's team results go back only to a requester who may have what they hold: what
/// its members' signed evidence names, and the Microsoft 365 content the lead's own run
/// gives its steps (here, its marked topic). Its own evidence then carries the labels.
#[tokio::test]
async fn a_teams_results_go_back_only_to_a_requester_who_may_have_what_they_hold() {
    use proofkin::knowledge::with_mark;
    use proofkin::{M365Grant, TrustedPeer};

    let home = tempfile::tempdir().unwrap();
    let lead_profile = proofkin::Profile::open_in(home.path(), "lead").unwrap();
    lead_profile
        .knowledge()
        .write("hr-notes", &with_mark("Pay bands", &["HR".into()]), "")
        .unwrap();
    let app = tempfile::tempdir().unwrap();
    let (_knowledge, memory) = worker_memory(
        "helper",
        &[("q3-board", "Revenue was 12.", &["Confidential"])],
        None,
    );
    let mut boss = Node::new("boss");
    let mut lead = Node::new("lead").serving(vec![Provider::Copilot]);
    let mut helper = Node::new("helper").serving(vec![Provider::Copilot]);
    helper.owner = Some("lead".into());
    helper.owner_id = Some(lead.id());
    helper.memory = Some(memory);
    lead.team = Some(Arc::new(proofkin::team::TeamRunner::new(lead_profile)));
    for (folder, team) in [
        ("app", "work @helper {task}"),
        ("hr", "work @helper context=hr-notes {task}"),
    ] {
        lead.shares.add(folder, app.path(), Access::Full).unwrap();
        lead.shares.set_team(folder, Some(team.into())).unwrap();
    }
    mutual(&mut boss, &mut lead);
    mutual(&mut lead, &mut helper);
    lead.grant("boss", Access::WorkspaceWrite, &["data", "app", "hr"]);
    lead.trust
        .peers
        .iter_mut()
        .find(|p| p.name == "helper")
        .unwrap()
        .m365 = Some(M365Grant {
        labels: vec!["HR".into()],
        tools: false,
    });
    let boss_id = boss.id();
    let (helper, _he) = helper.start().await;
    let (lead, _le) = lead.start().await;
    let (boss, _be) = boss.start().await;
    lead.connect(helper.addr()).await.unwrap();
    boss.connect(lead.addr()).await.unwrap();
    wait_for_peers(&lead, 2).await;
    wait_for_peers(&boss, 1).await;
    wait_until(|| {
        let shares = &boss.peers()[0].shares;
        ["app", "hr"]
            .iter()
            .all(|f| shares.iter().any(|s| s.name == *f && s.team))
    })
    .await;

    let plan = Plan::parse("build team @lead:app Summarize Q3").unwrap();
    let refused = |states: &std::collections::BTreeMap<String, StepState>, labels: &str| {
        let StepState::Failed { error, .. } = &states["build"] else {
            panic!("{:?}", states["build"]);
        };
        assert!(
            error.contains(&format!(
                "the team's results hold Microsoft 365 content (labels: {labels}) that boss may not receive"
            )),
            "{error}"
        );
        error.clone()
    };
    let states = run_plan(&boss, &SquadStore::default(), &plan, |_, _| {}).await;
    assert!(refused(&states, "Confidential").contains("--m365 Confidential"));

    lead.allow(TrustedPeer {
        name: "boss".into(),
        id: boss_id,
        addrs: BTreeSet::new(),
        can_delegate: true,
        tasks_per_hour: 100,
        access: Access::WorkspaceWrite,
        shares: vec![DATA_SHARE.to_owned(), "app".into(), "hr".into()],
        m365: Some(M365Grant {
            labels: vec!["Confidential".into()],
            tools: false,
        }),
        github: None,
        agents: Vec::new(),
    });
    let states = run_plan(&boss, &SquadStore::default(), &plan, |_, _| {}).await;
    let StepState::Done {
        output,
        evidence: Some(e),
        verified: true,
        ..
    } = &states["build"]
    else {
        panic!("{:?}", states["build"]);
    };
    assert!(output.contains("Revenue was 12."), "{output}");
    let team = e
        .context
        .iter()
        .find(|c| c.source == "team")
        .expect("the labels come with the result");
    assert_eq!(
        (team.results, team.labels.clone()),
        (1, vec!["Confidential".to_owned()])
    );

    // The lead's own HR topic is in that team's results too.
    let hr = Plan::parse("build team @lead:hr Summarize the pay bands").unwrap();
    let states = run_plan(&boss, &SquadStore::default(), &hr, |_, _| {}).await;
    let error = refused(&states, "HR, Confidential");
    assert!(error.contains("--m365 Confidential,HR"), "{error}");
    let _ = helper;
}
