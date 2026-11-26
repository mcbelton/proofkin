//! Deployed factory agents over real iroh endpoints on localhost (`Network::Local`): a
//! digital worker pinned to a release gives every task the release's instructions first,
//! works at no more access than the agent asks for, hands work to nobody unless its release
//! delegates, keeps to its budgets, and takes only its owner's canary tasks until its canary
//! passes (docs/agent-factory.md, sections 9 and 11).

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use proofkin::audit::{AuditLog, Outcome};
use proofkin::factory::blueprint::Budgets;
use proofkin::factory::deploy::{Deployed, Pinned};
use proofkin::factory::runner::SHARE;
use proofkin::factory::store::DeployState;
use proofkin::shares::{DATA_SHARE, ShareStore};
use proofkin::team::TeamRequest;
use proofkin::{
    Access, Agent, AgentConfig, Executor, LlmTask, Network, Provider, Ran, SecretKey, TaskContext,
    TrustStore, TrustedPeer,
};

/// A model that keeps what it was asked and the access it had.
#[derive(Default)]
struct Model {
    calls: Mutex<Vec<(String, Access)>>,
}

#[async_trait]
impl Executor for Model {
    fn providers(&self) -> Vec<Provider> {
        vec![Provider::Copilot]
    }

    async fn execute(&self, task: &LlmTask, ctx: &TaskContext) -> anyhow::Result<Ran> {
        self.calls
            .lock()
            .unwrap()
            .push((task.prompt.clone(), ctx.access));
        Ok(Ran::new("Revenue: 12M (p. 4).", task.model.clone()))
    }
}

fn pinned(state: DeployState, delegate: bool, budgets: Budgets) -> Pinned {
    Pinned {
        agent: "fin-analyst".into(),
        version: "1.2.0".into(),
        state,
        instructions: "\n### Agent: fin-analyst 1.2.0\nYou analyze financial reports.\n".into(),
        budgets,
        delegate,
        access: Access::ReadOnly,
    }
}

fn allowed(name: &str, key: &SecretKey, shares: &[&str]) -> TrustedPeer {
    TrustedPeer {
        name: name.into(),
        id: key.public(),
        addrs: BTreeSet::new(),
        can_delegate: true,
        tasks_per_hour: 100,
        // Granted full access: the folder and the release keep it lower.
        access: Access::Full,
        shares: shares.iter().map(|s| s.to_string()).collect(),
        m365: None,
        github: None,
        agents: Vec::new(),
    }
}

/// The deployed worker `fin-analyst` (running `deployed`), its owner `alice` and a peer
/// `bob`, connected. The worker shares `reports` read-only, as a deployment of a read-only
/// agent does, and the folder of its canary.
struct World {
    _tmp: tempfile::TempDir,
    model: Arc<Model>,
    owner: Agent,
    bob: Agent,
    worker: Agent,
    audit: std::path::PathBuf,
}

async fn world(deployed: Arc<Deployed>) -> World {
    let tmp = tempfile::tempdir().unwrap();
    let (owner_key, bob_key, worker_key) = (
        SecretKey::generate(),
        SecretKey::generate(),
        SecretKey::generate(),
    );
    let folder = |name: &str| {
        let dir = tmp.path().join(name);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    };
    let mut shares = ShareStore::with_data(folder("data"));
    shares
        .add_with(
            "reports",
            &folder("reports"),
            Access::ReadOnly,
            Default::default(),
            false,
        )
        .unwrap();
    shares
        .add_with(
            SHARE,
            &folder("canary"),
            Access::ReadOnly,
            Default::default(),
            false,
        )
        .unwrap();
    let mut trust = TrustStore::default();
    trust.allow(allowed(
        "alice",
        &owner_key,
        &[DATA_SHARE, "reports", SHARE],
    ));
    trust.allow(allowed("bob", &bob_key, &["reports"]));
    let model = Arc::new(Model::default());
    let audit = tmp.path().join("audit.jsonl");
    let mut config = AgentConfig::new("fin-analyst", worker_key.clone());
    config.network = Network::Local;
    config.bind = Some("127.0.0.1:0".parse().unwrap());
    config.trust = trust;
    config.executor = Some(model.clone());
    config.shares = shares;
    config.audit = Some(Arc::new(AuditLog::new(audit.clone())));
    config.worktrees_dir = tmp.path().join("worktrees");
    config.owner = Some("alice".into());
    config.owner_id = Some(owner_key.public());
    config.deployed = Some(deployed);
    let (worker, _) = Agent::bind(config).await.unwrap();
    let peer = |name: &str, key: SecretKey| {
        let mut config = AgentConfig::new(name, key);
        config.network = Network::Local;
        config.bind = Some("127.0.0.1:0".parse().unwrap());
        let mut trust = TrustStore::default();
        trust.allow(TrustedPeer {
            can_delegate: false,
            ..allowed("fin-analyst", &worker_key, &[])
        });
        config.trust = trust;
        config
    };
    let (owner, _) = Agent::bind(peer("alice", owner_key)).await.unwrap();
    let (bob, _) = Agent::bind(peer("bob", bob_key)).await.unwrap();
    for a in [&owner, &bob] {
        a.connect(worker.addr()).await.unwrap();
    }
    wait(|| owner.peers().len() == 1 && bob.peers().len() == 1).await;
    World {
        _tmp: tmp,
        model,
        owner,
        bob,
        worker,
        audit,
    }
}

async fn wait(done: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(20), async {
        while !done() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("timed out");
}

fn audited(path: &Path) -> Vec<proofkin::audit::AuditEntry> {
    AuditLog::read(path, 100).unwrap()
}

/// Gives `task` to `to` from `from`, which must refuse it before running it; returns why.
async fn refused(from: &Agent, to: proofkin::EndpointId, task: LlmTask) -> String {
    let delivered = from.delegate_verified(to, task).await.unwrap();
    assert!(!delivered.ok && delivered.refused, "{delivered:?}");
    delivered.output
}

/// AC-18, TM-5 and AC-7: every task the deployed worker serves gets its release's
/// instructions first, runs at the agent's read-only access whatever the peer was granted,
/// gets no MCP server its folder doesn't offer, and is named by the release in its signed
/// evidence, its audit and the worker's hello; a team step is refused, its release not
/// delegating, and the worker can hand work to no peer that hasn't granted it.
#[tokio::test]
async fn a_deployed_worker_runs_its_release_on_every_task_at_the_access_it_asks_for() {
    let w = world(Deployed::fixed(pinned(
        DeployState::Running,
        false,
        Budgets::default(),
    )))
    .await;
    let id = w.worker.id();
    assert_eq!(
        w.bob.peers()[0].agent.as_deref(),
        Some("fin-analyst@1.2.0"),
        "peers see the agent and its version"
    );
    let task =
        LlmTask::new(Provider::Copilot, "Extract the revenue from q3.md.").in_share("reports");
    let done = w.bob.delegate_verified(id, task.clone()).await.unwrap();
    assert!(
        done.ok && done.verified,
        "signed by the worker for this output"
    );
    let evidence = done.evidence.unwrap();
    assert_eq!(evidence.agent.as_deref(), Some("fin-analyst@1.2.0"));
    let (prompt, access) = w.model.calls.lock().unwrap()[0].clone();
    assert!(
        prompt.starts_with(
            "Follow these instructions:\n\n### Agent: fin-analyst 1.2.0\nYou analyze financial reports.\n\n---\n"
        ),
        "{prompt}"
    );
    assert!(
        prompt.ends_with("Extract the revenue from q3.md."),
        "{prompt}"
    );
    assert_eq!(
        access,
        Access::ReadOnly,
        "granted full, the folder keeps it read-only"
    );

    let mut tools = task.clone();
    tools.mcp = vec!["pdf".into()];
    let why = refused(&w.bob, id, tools).await;
    assert!(why.contains("offers no MCP server named 'pdf'"), "{why}");

    let mut team = task.clone();
    team.team = Some(TeamRequest {
        levels: 1,
        chain: Vec::new(),
        brief: "Review it".into(),
    });
    let why = refused(&w.owner, id, team).await;
    assert!(
        why.contains("fin-analyst@1.2.0 doesn't hand work to other agents"),
        "{why}"
    );
    // Delegating itself grants nothing: bob hasn't allowed it to give him work.
    let to_bob = w
        .worker
        .delegate_verified(w.bob.id(), LlmTask::new(Provider::Copilot, "hi"))
        .await;
    assert!(
        to_bob.map_or(true, |d| !d.ok && d.refused),
        "bob never granted it a task"
    );

    let entries = audited(&w.audit);
    let done = entries.iter().find(|e| e.outcome == Outcome::Done).unwrap();
    assert_eq!(done.agent.as_deref(), Some("fin-analyst@1.2.0"));
    assert_eq!(done.attempts, Some(1));
    assert_eq!(
        w.model.calls.lock().unwrap().len(),
        1,
        "refused tasks ran no model"
    );
}

/// AC-23: a task over the deployment's budgets is refused before any model call, with a
/// line naming the budget, and audited as refused by it.
#[tokio::test]
async fn a_deployed_worker_refuses_what_its_budgets_dont_allow_before_any_model_call() {
    let budgets = Budgets {
        tasks_per_day: Some(1),
        attempts: Some(1),
        timeout_secs: None,
    };
    let w = world(Deployed::fixed(pinned(
        DeployState::Running,
        false,
        budgets,
    )))
    .await;
    let id = w.worker.id();
    let task = LlmTask::new(Provider::Copilot, "Summarize q3.md.").in_share("reports");
    let retries = task.clone().with_checks(Vec::new(), Some(3));
    let why = refused(&w.bob, id, retries).await;
    assert!(
        why.contains("the task asks for 3 attempts; fin-analyst@1.2.0's budget allows 1"),
        "{why}"
    );
    assert!(w.bob.delegate_verified(id, task.clone()).await.unwrap().ok);
    let why = refused(&w.bob, id, task).await;
    assert!(
        why.contains("fin-analyst@1.2.0 took its budget of 1 task today"),
        "{why}"
    );
    assert_eq!(w.model.calls.lock().unwrap().len(), 1);
    let entries = audited(&w.audit);
    let by_budget: Vec<_> = entries
        .iter()
        .filter(|e| e.outcome == Outcome::Refused && e.refused_by.as_deref() == Some("budget"))
        .collect();
    assert_eq!(by_budget.len(), 2, "{entries:?}");
    assert!(
        by_budget
            .iter()
            .all(|e| e.agent.as_deref() == Some("fin-analyst@1.2.0"))
    );
}

/// AC-19: during its canary the deployed worker refuses peers' tasks (so a squad step moves
/// to another member) and takes its owner's canary tasks.
#[tokio::test]
async fn a_deployment_in_its_canary_takes_only_its_owners_canary_tasks() {
    let w = world(Deployed::fixed(pinned(
        DeployState::Canary,
        false,
        Budgets::default(),
    )))
    .await;
    let id = w.worker.id();
    let task = LlmTask::new(Provider::Copilot, "Summarize q3.md.").in_share("reports");
    let why = refused(&w.bob, id, task.clone()).await;
    assert!(why.contains("fin-analyst 1.2.0 is in its canary"), "{why}");
    let why = refused(&w.owner, id, task).await;
    assert!(
        why.contains("in its canary"),
        "only in the canary's folder: {why}"
    );
    let canary = LlmTask::new(Provider::Copilot, "Summarize q3.md.").in_share(SHARE);
    assert!(w.owner.delegate_verified(id, canary).await.unwrap().ok);
    let entries = audited(&w.audit);
    assert_eq!(
        entries
            .iter()
            .filter(|e| e.refused_by.as_deref() == Some("canary"))
            .count(),
        2
    );
}
