//! Web UI server: access token, Host check, security headers and the task API.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use proofkin::audit::{AuditEntry, AuditLog};
use proofkin::models::Offered;
use proofkin::shares::ShareStore;
use proofkin::ui::{self, UiInfo};
use proofkin::{
    Access, Agent, AgentConfig, Executor, LlmTask, Network, Profile, Provider, Ran, SecretKey,
    TaskContext, TrustedPeer,
};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

struct Echo;

#[async_trait]
impl Executor for Echo {
    fn providers(&self) -> Vec<Provider> {
        vec![Provider::Copilot]
    }

    async fn execute(&self, task: &LlmTask, _ctx: &TaskContext) -> anyhow::Result<Ran> {
        if task.prompt == "hang" {
            std::future::pending::<()>().await;
        }
        // The provider says which model answered (only for "hello", so peers' results
        // elsewhere keep their exact output).
        let model = (task.prompt == "hello").then(|| "echo-model-1".to_owned());
        Ok(Ran::new(format!("echo: {}", task.prompt), model))
    }

    /// Copilot offers two models: one with reasoning levels, one without.
    async fn list_models(&self, provider: Provider) -> anyhow::Result<Vec<Offered>> {
        let model = |id: &str, efforts: &[&str]| Offered {
            id: id.into(),
            name: id.into(),
            efforts: efforts.iter().map(|e| e.to_string()).collect(),
            ..Default::default()
        };
        Ok(match provider {
            Provider::Copilot => vec![
                model("echo-big", &["low", "high", "max"]),
                model("echo-small", &[]),
            ],
            Provider::Claude | Provider::Codex => Vec::new(),
        })
    }
}

struct Response {
    status: u16,
    headers: String,
    body: String,
}

impl Response {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).expect("JSON body")
    }
}

async fn request(
    port: u16,
    method: &str,
    path: &str,
    host: &str,
    token: Option<&str>,
    body: Option<&str>,
) -> Response {
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n");
    if let Some(token) = token {
        req += &format!("Authorization: Bearer {token}\r\n");
    }
    let body = body.unwrap_or("");
    if !body.is_empty() {
        req += &format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        );
    }
    req += "\r\n";
    req += body;
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).await.unwrap();
    let (head, body) = raw.split_once("\r\n\r\n").expect("HTTP response");
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    Response {
        status,
        headers: head.to_ascii_lowercase(),
        body: body.to_owned(),
    }
}

#[tokio::test]
async fn ui_requires_token_and_local_host_and_runs_tasks() {
    let home = tempfile::tempdir().unwrap();
    let mut config = AgentConfig::new("solo", SecretKey::generate());
    config.network = Network::Local;
    config.bind = Some("127.0.0.1:0".parse().unwrap());
    let executor: Arc<dyn Executor> = Arc::new(Echo);
    config.executor = Some(executor.clone());
    let (agent, events) = Agent::bind(config).await.unwrap();
    let info = UiInfo {
        profile: Profile::open_in(home.path(), "solo").unwrap(),
        network: Network::Local,
        serving: false,
        access: Access::ReadOnly,
        workspace: home.path().to_path_buf(),
        providers: vec![Provider::Copilot],
        accounts: Vec::new(),
        workers: None,
    };
    let server = ui::start(
        agent.clone(),
        events,
        info,
        Some(executor),
        "127.0.0.1:0".parse().unwrap(),
    )
    .await
    .unwrap();

    let rest = server.url.strip_prefix("http://127.0.0.1:").unwrap();
    let (port, token) = rest.split_once("/#t=").unwrap();
    let port: u16 = port.parse().unwrap();
    assert_eq!(token.len(), 32);
    let host = format!("127.0.0.1:{port}");

    let page = request(port, "GET", "/", &host, None, None).await;
    assert_eq!(page.status, 200);
    assert!(
        page.headers
            .contains("content-security-policy: default-src 'self'")
    );
    assert!(page.headers.contains("x-content-type-options: nosniff"));

    let anonymous = request(port, "GET", "/api/state", &host, None, None).await;
    assert_eq!(anonymous.status, 401);
    let wrong = request(port, "GET", "/api/state", &host, Some("0123"), None).await;
    assert_eq!(wrong.status, 401);
    let rebound = request(
        port,
        "GET",
        "/api/state",
        "evil.example:80",
        Some(token),
        None,
    )
    .await;
    assert_eq!(rebound.status, 403, "foreign Host must be refused");

    let state = request(port, "GET", "/api/state", &host, Some(token), None).await;
    assert_eq!(state.status, 200);
    assert_eq!(state.json()["me"]["name"], "solo");
    let localhost = format!("localhost:{port}");
    let state = request(port, "GET", "/api/state", &localhost, Some(token), None).await;
    assert_eq!(state.status, 200);

    let empty = r#"{"provider":"copilot","prompt":"  ","target":"local"}"#;
    let rejected = request(port, "POST", "/api/tasks", &host, Some(token), Some(empty)).await;
    assert_eq!(rejected.status, 400);

    let task = r#"{"provider":"copilot","prompt":"hello","target":"local"}"#;
    let created = request(port, "POST", "/api/tasks", &host, Some(token), Some(task)).await;
    assert_eq!(created.status, 200, "{}", created.body);

    let done = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let state = request(port, "GET", "/api/state", &host, Some(token), None)
                .await
                .json();
            let task = state["tasks"][0].clone();
            if task["status"] != "running" {
                return task;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("task finished");
    assert_eq!(done["status"], "done");
    assert_eq!(done["output"], "echo: hello");
    // The task names the model that did the work, as its provider said.
    assert_eq!(done["model"], "echo-model-1");
    agent.shutdown().await;
}

#[tokio::test]
async fn delegate_only_agents_allowlist_chat_peers_from_the_ui() {
    let home = tempfile::tempdir().unwrap();
    // The models it sets are what its digital workers run; the page selects them.
    let mut profile = Profile::open_in(home.path(), "boss").unwrap();
    proofkin::models::set_default(
        &mut profile.config.models,
        Provider::Copilot,
        Some("claude-opus-5.5"),
    )
    .unwrap();
    proofkin::models::set_effort(&mut profile.config.models, Provider::Copilot, Some("max"))
        .unwrap();
    profile.save().unwrap();
    let mut config = AgentConfig::new("boss", SecretKey::generate());
    config.network = Network::Local;
    config.bind = Some("127.0.0.1:0".parse().unwrap());
    config.shares = ShareStore::with_data(home.path().join("data"));
    // As an agent starts: with its profile's models.
    config.models = profile.models();
    let (agent, events) = Agent::bind(config).await.unwrap();
    let info = UiInfo {
        profile,
        network: Network::Local,
        serving: false,
        access: Access::ReadOnly,
        workspace: home.path().to_path_buf(),
        providers: Vec::new(),
        accounts: Vec::new(),
        workers: None,
    };
    let server = ui::start(
        agent.clone(),
        events,
        info,
        None,
        "127.0.0.1:0".parse().unwrap(),
    )
    .await
    .unwrap();
    let rest = server.url.strip_prefix("http://127.0.0.1:").unwrap();
    let (port, token) = rest.split_once("/#t=").unwrap();
    let port: u16 = port.parse().unwrap();
    let host = format!("127.0.0.1:{port}");
    let peer = SecretKey::generate().public();

    // A peer that isn't on the allowlist is refused, and the page learns its endpoint
    // id, to offer to allowlist it (a teammate who allowlisted this agent first).
    let mut stranger = AgentConfig::new("stranger", SecretKey::generate());
    stranger.network = Network::Local;
    stranger.bind = Some("127.0.0.1:0".parse().unwrap());
    // It allowlisted this agent (it only dials keys it allowlisted); not the other way.
    stranger.trust.allow(TrustedPeer {
        name: "boss".into(),
        id: agent.id(),
        addrs: Default::default(),
        can_delegate: false,
        tasks_per_hour: 0,
        access: Access::ReadOnly,
        shares: Vec::new(),
        m365: None,
        github: None,
        agents: Vec::new(),
    });
    let (stranger, _stranger_events) = Agent::bind(stranger).await.unwrap();
    let _ = stranger.connect(agent.addr()).await;
    let refused = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let state = request(port, "GET", "/api/state", &host, Some(token), None)
                .await
                .json();
            let item = state["feed"]
                .as_array()
                .unwrap()
                .iter()
                .find(|i| i["peer"].is_string())
                .cloned();
            if let Some(item) = item {
                return item;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the refused connection is reported");
    assert_eq!(refused["peer"], stranger.id().to_string());
    assert_eq!(refused["level"], "warn");
    assert!(agent.peers().is_empty());
    stranger.shutdown().await;

    let chat = format!(r#"{{"ticket":"{peer}","name":"pal"}}"#);
    let me = request(port, "GET", "/api/state", &host, Some(token), None)
        .await
        .json();
    assert_eq!(
        me["me"]["models"]["copilot"],
        serde_json::json!({ "default": "claude-opus-5.5", "effort": "max" })
    );
    let added = request(port, "POST", "/api/trust", &host, Some(token), Some(&chat)).await;
    assert_eq!(added.status, 200, "{}", added.body);
    assert!(agent.trusted().get(&peer).is_some_and(|p| !p.can_delegate));

    let delegate =
        format!(r#"{{"ticket":"{peer}","name":"pal","delegate":true,"shares":["nope"]}}"#);
    let refused = request(
        port,
        "POST",
        "/api/trust",
        &host,
        Some(token),
        Some(&delegate),
    )
    .await;
    assert_eq!(refused.status, 400);
    assert!(
        refused.body.contains("no folder named 'nope'"),
        "{}",
        refused.body
    );

    // Change… lets pal delegate without removing it: its name stays, and it is saved.
    let put = |id: String, body: &'static str| {
        let (host, token) = (host.clone(), token.to_owned());
        async move {
            let path = format!("/api/trust/{id}");
            request(port, "PUT", &path, &host, Some(&token), Some(body)).await
        }
    };
    let grant =
        r#"{"delegate":true,"tasks_per_hour":5,"access":"workspace-write","shares":["data"]}"#;
    let changed = put(peer.to_string(), grant).await;
    assert_eq!(changed.status, 200, "{}", changed.body);
    let pal = agent.trusted().get(&peer).cloned().unwrap();
    assert_eq!(
        (
            pal.name.as_str(),
            pal.can_delegate,
            pal.tasks_per_hour,
            pal.access,
            pal.shares.as_slice()
        ),
        (
            "pal",
            true,
            5,
            Access::WorkspaceWrite,
            ["data".to_owned()].as_slice()
        )
    );
    let saved = Profile::open_in(home.path(), "boss")
        .unwrap()
        .trust()
        .unwrap();
    assert!(
        saved
            .get(&peer)
            .is_some_and(|p| p.can_delegate && p.tasks_per_hour == 5)
    );
    // Back to chat only; a folder this agent doesn't share, or a peer that isn't on the
    // allowlist, is refused.
    assert_eq!(
        put(peer.to_string(), r#"{"delegate":false}"#).await.status,
        200
    );
    assert!(
        agent
            .trusted()
            .get(&peer)
            .is_some_and(|p| !p.can_delegate && p.access == Access::ReadOnly)
    );
    let bad = put(peer.to_string(), r#"{"delegate":true,"shares":["nope"]}"#).await;
    assert_eq!(bad.status, 400, "{}", bad.body);
    let unknown = SecretKey::generate().public().to_string();
    assert_eq!(put(unknown, r#"{"delegate":false}"#).await.status, 404);

    // Change… also sets its GitHub account (pull request steps ask it to review); without
    // one given, it is kept, also when pal is allowlisted again; empty forgets it.
    let github = |id: &iroh::EndpointId| agent.trusted().get(id).and_then(|p| p.github.clone());
    let set = put(
        peer.to_string(),
        r#"{"delegate":false,"github":"@octo-cat"}"#,
    )
    .await;
    assert_eq!(set.status, 200, "{}", set.body);
    assert_eq!(github(&peer).as_deref(), Some("octo-cat"));
    assert_eq!(
        put(peer.to_string(), r#"{"delegate":false}"#).await.status,
        200
    );
    let again = request(port, "POST", "/api/trust", &host, Some(token), Some(&chat)).await;
    assert_eq!(again.status, 200, "{}", again.body);
    assert_eq!(github(&peer).as_deref(), Some("octo-cat"));
    let state = request(port, "GET", "/api/state", &host, Some(token), None)
        .await
        .json();
    assert_eq!(state["trusted"][0]["github"], "octo-cat", "{state}");
    let bad = put(
        peer.to_string(),
        r#"{"delegate":false,"github":"no such login"}"#,
    )
    .await;
    assert_eq!(bad.status, 400, "{}", bad.body);
    assert!(bad.body.contains("isn't a GitHub login"), "{}", bad.body);
    assert_eq!(
        put(peer.to_string(), r#"{"delegate":false,"github":""}"#)
            .await
            .status,
        200
    );
    assert_eq!(github(&peer), None);

    agent.shutdown().await;
}

#[tokio::test]
async fn a_run_card_offers_its_work_for_a_pull_request_and_says_why_it_cannot_now() {
    use proofkin::evidence::{Evidence, EvidenceKind, GitCommit};
    use proofkin::plan::StepState;
    let home = tempfile::tempdir().unwrap();
    let profile = Profile::open_in(home.path(), "boss").unwrap();
    let (boss, alice) = (SecretKey::generate(), SecretKey::generate());
    // A run whose step committed work in alice's folder shop, as alice's agent signed it.
    let text = "impl @alice:shop Add CSV export to the report page";
    let plan = proofkin::plan::Plan::parse(text).unwrap();
    let mut evidence = Evidence::new(
        EvidenceKind::Task,
        "alice",
        alice.public(),
        boss.public(),
        1,
    );
    evidence.share = Some("shop".into());
    evidence.git = Some(GitCommit {
        branch: "p2p/boss/1-1".into(),
        base: "base".into(),
        commit: Some("c0ffee".repeat(7)),
        files: vec!["report.rs".into()],
        patch_sha256: None,
        left_out: Vec::new(),
    });
    let states: std::collections::BTreeMap<String, StepState> = [(
        "impl".to_owned(),
        StepState::Done {
            peer: "alice".into(),
            output: "did it".into(),
            evidence: Some(evidence.sign("did it", &alice)),
            verified: true,
            patch: None,
            model: None,
        },
    )]
    .into_iter()
    .collect();
    let started = proofkin::runs::unix_now() - 60;
    let mut run = proofkin::runs::RunRecord::new(
        "boss",
        &boss.public().to_string(),
        text,
        &plan,
        &states,
        started,
    );
    run.id = "run-1".into();
    run.peer_ids
        .insert("alice".into(), alice.public().to_string());
    run.save(&profile.runs_dir()).unwrap();

    let mut config = AgentConfig::new("boss", boss);
    config.network = Network::Local;
    config.bind = Some("127.0.0.1:0".parse().unwrap());
    let (agent, events) = Agent::bind(config).await.unwrap();
    let info = UiInfo {
        profile,
        network: Network::Local,
        serving: false,
        access: Access::ReadOnly,
        workspace: home.path().to_path_buf(),
        providers: Vec::new(),
        accounts: Vec::new(),
        workers: None,
    };
    let server = ui::start(
        agent.clone(),
        events,
        info,
        None,
        "127.0.0.1:0".parse().unwrap(),
    )
    .await
    .unwrap();
    let rest = server.url.strip_prefix("http://127.0.0.1:").unwrap();
    let (port, token) = rest.split_once("/#t=").unwrap();
    let port: u16 = port.parse().unwrap();
    let host = format!("127.0.0.1:{port}");

    let details = request(port, "GET", "/api/runs/run-1", &host, Some(token), None)
        .await
        .json();
    assert_eq!(
        details["pull_requests"],
        serde_json::json!([{
            "step": "impl",
            "peer": "alice",
            "folder": "shop",
            "branch": "p2p/boss/1-1",
            "commit": "c0ffeec0ffee",
            "title": "Add CSV export to the report page",
            "why": "alice isn't connected",
            "approval": false,
        }])
    );
    let post = |path: &'static str, body: &'static str| {
        let (host, token) = (host.clone(), token.to_owned());
        async move { request(port, "POST", path, &host, Some(&token), Some(body)).await }
    };
    for (body, why) in [
        (
            r#"{"step":"impl"}"#,
            "alice isn't connected: its agent holds the clone of 'shop' with the work",
        ),
        (
            r#"{"step":"nope"}"#,
            "step nope of run run-1 has no work to open a pull request with",
        ),
        (
            r#"{"step":"impl","reviewers":"me none"}"#,
            "reviewers=none asks no one",
        ),
    ] {
        let refused = post("/api/runs/run-1/pull-request", body).await;
        assert_eq!(refused.status, 400, "{body}: {}", refused.body);
        assert!(refused.body.contains(why), "{body}: {}", refused.body);
    }
    assert_eq!(
        post("/api/runs/missing/pull-request", r#"{"step":"impl"}"#)
            .await
            .status,
        404
    );

    agent.shutdown().await;
}

#[tokio::test]
async fn ui_cancels_running_tasks_and_shows_the_audit_log() {
    let home = tempfile::tempdir().unwrap();
    let profile = Profile::open_in(home.path(), "solo").unwrap();
    // Two entries a peer left in this profile's audit log.
    let log = AuditLog::new(profile.audit_path());
    let peer = SecretKey::generate().public();
    let task = LlmTask::new(Provider::Copilot, "private prompt text");
    let started = std::time::SystemTime::now();
    log.record(&AuditEntry::new(
        "bob",
        &peer,
        1,
        &task,
        Some(Access::ReadOnly),
        &Ok("x".into()),
        started,
    ));
    log.record(&AuditEntry::new(
        "bob",
        &peer,
        2,
        &task,
        None,
        &Err(anyhow::anyhow!("not allowed")),
        started,
    ));

    let mut config = AgentConfig::new("solo", SecretKey::generate());
    config.network = Network::Local;
    config.bind = Some("127.0.0.1:0".parse().unwrap());
    let executor: Arc<dyn Executor> = Arc::new(Echo);
    let (agent, events) = Agent::bind(config).await.unwrap();
    let info = UiInfo {
        profile,
        network: Network::Local,
        serving: true,
        access: Access::ReadOnly,
        workspace: home.path().to_path_buf(),
        providers: vec![Provider::Copilot],
        accounts: Vec::new(),
        workers: None,
    };
    let server = ui::start(
        agent.clone(),
        events,
        info,
        Some(executor),
        "127.0.0.1:0".parse().unwrap(),
    )
    .await
    .unwrap();
    let rest = server.url.strip_prefix("http://127.0.0.1:").unwrap();
    let (port, token) = rest.split_once("/#t=").unwrap();
    let port: u16 = port.parse().unwrap();
    let host = format!("127.0.0.1:{port}");

    // Cancel a running task.
    let hang = r#"{"provider":"copilot","prompt":"hang","target":"local"}"#;
    let created = request(port, "POST", "/api/tasks", &host, Some(token), Some(hang)).await;
    assert_eq!(created.status, 200, "{}", created.body);
    let id = created.json()["id"].as_u64().unwrap();
    let path = format!("/api/tasks/{id}");
    assert_eq!(
        request(port, "DELETE", &path, &host, None, None)
            .await
            .status,
        401
    );
    let cancelled = request(port, "DELETE", &path, &host, Some(token), None).await;
    assert_eq!(cancelled.status, 200, "{}", cancelled.body);
    let state = request(port, "GET", "/api/state", &host, Some(token), None)
        .await
        .json();
    assert_eq!(state["tasks"][0]["status"], "cancelled");
    let again = request(port, "DELETE", &path, &host, Some(token), None).await;
    assert_eq!(again.status, 409, "a finished task can't be cancelled");

    // The audit log, newest first, only with the token, never with prompts.
    assert_eq!(
        request(port, "GET", "/api/audit", &host, None, None)
            .await
            .status,
        401
    );
    let audit = request(port, "GET", "/api/audit", &host, Some(token), None).await;
    assert_eq!(audit.status, 200);
    let body = audit.json();
    assert_eq!(body["enabled"], true);
    assert_eq!(body["entries"][0]["outcome"], "refused");
    assert_eq!(body["entries"][1]["outcome"], "done");
    assert!(!audit.body.contains("private prompt text"));

    agent.shutdown().await;
}

/// Starts the web UI of profile `solo` in `home`, running its tasks with [`Echo`]:
/// its agent, port and access token.
async fn solo_ui(home: &std::path::Path) -> (Agent, u16, String) {
    ui_of(home, "solo").await
}

/// Starts the web UI of profile `name` in `home`, running its tasks with [`Echo`]:
/// its agent, port and access token.
async fn ui_of(home: &std::path::Path, name: &str) -> (Agent, u16, String) {
    let mut config = AgentConfig::new(name, SecretKey::generate());
    config.network = Network::Local;
    config.bind = Some("127.0.0.1:0".parse().unwrap());
    let (agent, events) = Agent::bind(config).await.unwrap();
    let info = UiInfo {
        profile: Profile::open_in(home, name).unwrap(),
        network: Network::Local,
        serving: true,
        access: Access::ReadOnly,
        workspace: home.to_path_buf(),
        providers: vec![Provider::Copilot],
        accounts: Vec::new(),
        workers: None,
    };
    let server = ui::start(
        agent.clone(),
        events,
        info,
        Some(Arc::new(Echo)),
        "127.0.0.1:0".parse().unwrap(),
    )
    .await
    .unwrap();
    let rest = server.url.strip_prefix("http://127.0.0.1:").unwrap();
    let (port, token) = rest.split_once("/#t=").unwrap();
    (agent, port.parse().unwrap(), token.to_owned())
}

/// Waits until the profile's `tasks.json` keeps `want` (each task's "id:status").
async fn wait_kept(path: &std::path::Path, want: &str) {
    let kept = || {
        std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .and_then(|file| file["tasks"].as_array().cloned())
            .unwrap_or_default()
            .iter()
            .map(|t| format!("{}:{}", t["id"], t["status"].as_str().unwrap()))
            .collect::<Vec<_>>()
            .join(" ")
    };
    for _ in 0..200 {
        if kept() == want {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("tasks.json keeps {:?}, not {want:?}", kept());
}

/// The task list outlives a restart: finished tasks come back as they were (long
/// outputs cut), one the agent stopped in the middle comes back cancelled saying
/// why, ids go on, and the finished ones can be cleared.
#[tokio::test]
async fn tasks_outlive_a_restart_and_finished_ones_can_be_cleared() {
    let home = tempfile::tempdir().unwrap();
    let kept = Profile::open_in(home.path(), "solo").unwrap().tasks_path();
    let (agent, port, token) = solo_ui(home.path()).await;
    let host = format!("127.0.0.1:{port}");
    let long = serde_json::json!({
        "provider": "copilot", "prompt": "é".repeat(33_000), "target": "local",
    })
    .to_string();
    let hang = r#"{"provider":"copilot","prompt":"hang","target":"local"}"#;
    let first = request(port, "POST", "/api/tasks", &host, Some(&token), Some(&long)).await;
    assert_eq!(first.status, 200, "{}", first.body);
    let second = request(port, "POST", "/api/tasks", &host, Some(&token), Some(hang)).await;
    assert_eq!(second.json()["id"], 2);
    wait_kept(&kept, "1:done 2:running").await;
    agent.shutdown().await;
    // Two agents of one profile never run at once: let this one's last save land.
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let (agent, port, token) = solo_ui(home.path()).await;
    let host = format!("127.0.0.1:{port}");
    let state = request(port, "GET", "/api/state", &host, Some(&token), None)
        .await
        .json();
    let tasks = state["tasks"].as_array().unwrap();
    assert_eq!(tasks.len(), 2);
    assert_eq!(tasks[0]["status"], "done");
    let output = tasks[0]["output"].as_str().unwrap();
    assert!(output.starts_with("echo: éé"));
    assert!(output.ends_with("characters are kept after a restart)"));
    assert_eq!(
        output.chars().filter(|c| *c == 'é').count(),
        32_000 - "echo: ".len()
    );
    assert_eq!(tasks[1]["status"], "cancelled");
    assert_eq!(
        tasks[1]["output"],
        "The agent stopped before this finished."
    );
    assert!(tasks[1]["finished_ms"].is_null(), "{}", tasks[1]);
    assert!(
        state["feed"].as_array().unwrap().iter().any(|f| f["text"]
            .as_str()
            .is_some_and(|t| t.starts_with("1 task was cancelled because the agent stopped"))),
        "{}",
        state["feed"]
    );
    // Kept as cancelled from now on, so the next restart doesn't report it again.
    wait_kept(&kept, "1:done 2:cancelled").await;

    let again = r#"{"provider":"copilot","prompt":"again","target":"local"}"#;
    let third = request(port, "POST", "/api/tasks", &host, Some(&token), Some(again)).await;
    assert_eq!(third.json()["id"], 3, "ids go on");
    wait_kept(&kept, "1:done 2:cancelled 3:done").await;
    let fourth = request(port, "POST", "/api/tasks", &host, Some(&token), Some(hang)).await;
    assert_eq!(fourth.json()["id"], 4);

    // Clearing lets the finished tasks go; the running one stays.
    assert_eq!(
        request(port, "DELETE", "/api/tasks", &host, None, None)
            .await
            .status,
        401
    );
    let cleared = request(port, "DELETE", "/api/tasks", &host, Some(&token), None).await;
    assert_eq!(cleared.status, 200, "{}", cleared.body);
    assert_eq!(cleared.json()["cleared"], 3);
    assert_eq!(cleared.json()["tasks"][0]["id"], 4, "what is left");
    let state = request(port, "GET", "/api/state", &host, Some(&token), None)
        .await
        .json();
    assert_eq!(state["tasks"].as_array().unwrap().len(), 1);
    assert_eq!(state["tasks"][0]["id"], 4);
    wait_kept(&kept, "4:running").await;
    agent.shutdown().await;
}

/// Runs the tasks it is given like [`Echo`], holding those whose prompt has a gate until
/// it opens; it notes each task as it starts and ends.
struct Gated {
    gates: std::collections::HashMap<String, Arc<tokio::sync::Semaphore>>,
    started: std::sync::Mutex<Vec<String>>,
    ended: std::sync::Mutex<Vec<String>>,
}

#[async_trait]
impl Executor for Gated {
    fn providers(&self) -> Vec<Provider> {
        vec![Provider::Copilot]
    }

    async fn execute(&self, task: &LlmTask, _ctx: &TaskContext) -> anyhow::Result<Ran> {
        self.started.lock().unwrap().push(task.prompt.clone());
        if let Some(gate) = self.gates.get(&task.prompt) {
            gate.acquire().await.unwrap().forget();
        }
        self.ended.lock().unwrap().push(task.prompt.clone());
        Ok(Ran::new(format!("echo: {}", task.prompt), None))
    }
}

/// A plan run goes on after its agent restarts: a step that ended before keeps its
/// result, one its peer finished while the agent was away gets the result the peer kept,
/// one still running is waited for again (neither runs twice), a sign-off is asked
/// again, and the steps after them run. The run is saved once, as the same run; task ids
/// go on; nothing is kept for a restart once it has ended.
#[tokio::test]
async fn a_plan_goes_on_after_its_agent_restarts() {
    let home = tempfile::tempdir().unwrap();
    let (boss_key, worker_key) = (SecretKey::generate(), SecretKey::generate());
    let trust = |name: &str, key: &SecretKey, delegate: bool| TrustedPeer {
        name: name.into(),
        id: key.public(),
        addrs: Default::default(),
        can_delegate: delegate,
        tasks_per_hour: 100,
        access: Access::ReadOnly,
        shares: vec!["data".into()],
        m365: None,
        github: None,
        agents: Vec::new(),
    };
    let gate = || Arc::new(tokio::sync::Semaphore::new(0));
    let (early, mid, late) = (gate(), gate(), gate());
    let worked = Arc::new(Gated {
        gates: [
            ("early".to_owned(), early.clone()),
            ("mid".to_owned(), mid.clone()),
            ("late".to_owned(), late.clone()),
        ]
        .into(),
        started: Default::default(),
        ended: Default::default(),
    });
    let mut worker = AgentConfig::new("worker", worker_key.clone());
    worker.network = Network::Local;
    worker.bind = Some("127.0.0.1:0".parse().unwrap());
    worker.trust.allow(trust("boss", &boss_key, true));
    worker.executor = Some(worked.clone());
    worker.shares = ShareStore::with_data(home.path().join("data"));
    let (worker, _we) = Agent::bind(worker).await.unwrap();

    let profile = Profile::open_in(home.path(), "boss").unwrap();
    let in_progress = profile.runs_in_progress_dir();
    // The boss, and the same boss again after a restart (its key and profile).
    let start_boss = || {
        let (key, home, worker_key, worker) = (
            boss_key.clone(),
            home.path().to_owned(),
            worker_key.clone(),
            worker.clone(),
        );
        async move {
            let profile = Profile::open_in(&home, "boss").unwrap();
            let mut config = AgentConfig::new("boss", key);
            config.network = Network::Local;
            config.bind = Some("127.0.0.1:0".parse().unwrap());
            config.trust.allow(trust("worker", &worker_key, false));
            config.task_ids = Some(profile.task_ids_path());
            // Its runs' steps never give up on the worker in this test: a boss that
            // stopped has no run going on.
            config.task_grace = Duration::from_secs(3600);
            let (boss, events) = Agent::bind(config).await.unwrap();
            let info = UiInfo {
                profile,
                network: Network::Local,
                serving: false,
                access: Access::ReadOnly,
                workspace: home.clone(),
                providers: Vec::new(),
                accounts: Vec::new(),
                workers: None,
            };
            let server = ui::start(
                boss.clone(),
                events,
                info,
                None,
                "127.0.0.1:0".parse().unwrap(),
            )
            .await
            .unwrap();
            // After a restart the peers connect again; the run waits for them.
            boss.connect(worker.addr()).await.unwrap();
            let rest = server.url.strip_prefix("http://127.0.0.1:").unwrap();
            let (port, token) = rest.split_once("/#t=").unwrap();
            (boss, port.parse::<u16>().unwrap(), token.to_owned())
        }
    };
    let state = |port: u16, token: String| async move {
        let host = format!("127.0.0.1:{port}");
        request(port, "GET", "/api/state", &host, Some(&token), None)
            .await
            .json()
    };
    let until = |what: &'static str, mut done: Box<dyn FnMut() -> bool + Send>| async move {
        tokio::time::timeout(Duration::from_secs(20), async {
            while !done() {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{what}"));
    };
    let kept = || {
        std::fs::read_dir(&in_progress)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| std::fs::read(e.path()).ok())
            .filter_map(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .collect::<Vec<_>>()
    };

    let (boss, port, token) = start_boss().await;
    let host = format!("127.0.0.1:{port}");
    while boss.peers().is_empty() {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let text = serde_json::json!({ "text": "early @worker early\nmid @worker mid\nlate @worker late\nok after early approve @me Sign it\nlast after ok,mid,late @worker last" });
    let started = request(
        port,
        "POST",
        "/api/plans",
        &host,
        Some(&token),
        Some(&text.to_string()),
    )
    .await;
    assert_eq!(started.status, 200, "{}", started.body);
    let ran = worked.clone();
    let (p, t) = (port, token.clone());
    // Two run at once on the worker: `late` waits for a slot.
    if tokio::time::timeout(Duration::from_secs(20), async {
        while ran.started.lock().unwrap().len() < 2 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .is_err()
    {
        let tasks = state(p, t).await["tasks"].clone();
        panic!(
            "the first steps didn't start: {:?}\n{tasks}",
            worked.started.lock().unwrap(),
        );
    }
    // `early` ends: `late` gets its slot, and the sign-off after it is asked for.
    early.add_permits(1);
    let (p, t, ran) = (port, token.clone(), worked.clone());
    tokio::time::timeout(Duration::from_secs(20), async {
        while state(p, t.clone()).await["approvals"]
            .as_array()
            .is_none_or(|a| a.is_empty())
            || !ran.started.lock().unwrap().contains(&"late".to_owned())
        {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("late started and the sign-off asked for");
    // Kept for a restart: the step that ended, and the work it waits for.
    let file = kept().pop().expect("the run is kept while it goes");
    assert_eq!(file["finished"]["early"]["status"], "done");
    for step in ["mid", "late", "ok"] {
        assert!(file["sent"][step]["task_id"].as_u64().is_some(), "{file}");
    }
    assert_eq!(file["sent"]["ok"]["kind"], "approval");
    assert_eq!(file["sent"]["mid"]["task"]["prompt"], "mid");
    let run_id = file["run_id"].as_str().unwrap().to_owned();
    let late_id = file["sent"]["late"]["task_id"].as_u64().unwrap();

    // The boss stops; `mid` ends while it is away.
    boss.shutdown().await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    mid.add_permits(1);
    let ran = worked.clone();
    until(
        "mid ended while the boss was away",
        Box::new(move || ran.ended.lock().unwrap().contains(&"mid".to_owned())),
    )
    .await;

    let (boss, port, token) = start_boss().await;
    let host = format!("127.0.0.1:{port}");
    let now = state(port, token.clone()).await;
    let feed = now["feed"].to_string();
    assert!(
        feed.contains(&format!(
            "Plan run {run_id} goes on after the restart: 1 of 5 steps had ended"
        )),
        "{feed}"
    );
    assert!(
        !feed.contains("cancelled because the agent stopped"),
        "{feed}"
    );
    let status = |state: &Value, step: &str| {
        state["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["step"] == step)
            .map(|t| t["status"].as_str().unwrap().to_owned())
            .unwrap()
    };
    assert_eq!(status(&now, "early"), "done");
    assert_ne!(status(&now, "late"), "cancelled", "{now}");
    // The sign-off is asked for again.
    let (p, t) = (port, token.clone());
    let approval = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Some(a) = state(p, t.clone()).await["approvals"]
                .as_array()
                .and_then(|a| a.first().cloned())
            {
                return a;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("sign-off asked for again");
    let answer = request(
        port,
        "POST",
        &format!("/api/approvals/{}", approval["id"]),
        &host,
        Some(&token),
        Some(r#"{"approved":true,"note":"ok"}"#),
    )
    .await;
    assert_eq!(answer.status, 200, "{}", answer.body);
    late.add_permits(1);
    let (p, t) = (port, token.clone());
    let done = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let now = state(p, t.clone()).await;
            let tasks = now["tasks"].as_array().unwrap().clone();
            if tasks.iter().all(|t| t["status"] == "done")
                && tasks.iter().all(|t| t["run"].is_string())
            {
                return now;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the run ended");
    let tasks = done["tasks"].as_array().unwrap();
    assert_eq!(tasks.len(), 5);
    assert!(
        tasks.iter().all(|t| t["run"] == run_id.as_str()),
        "the same run"
    );
    let find = |step: &str| tasks.iter().find(|t| t["step"] == step).unwrap().clone();
    assert_eq!(
        find("mid")["output"].as_str().unwrap().lines().next(),
        Some("echo: mid")
    );
    assert_eq!(
        find("late")["output"].as_str().unwrap().lines().next(),
        Some("echo: late")
    );
    // Each ran once: the worker gave the kept result, or went on with the task.
    let mut started = worked.started.lock().unwrap().clone();
    started.sort();
    assert_eq!(started.len(), 4, "{started:?}");
    for prompt in ["early", "mid", "late"] {
        assert_eq!(
            started.iter().filter(|p| *p == prompt).count(),
            1,
            "{prompt}"
        );
    }
    let runs = request(port, "GET", "/api/runs", &host, Some(&token), None)
        .await
        .json();
    assert_eq!(runs["runs"].as_array().unwrap().len(), 1, "saved once");
    assert_eq!(runs["runs"][0]["id"], run_id.as_str());
    assert!(kept().is_empty(), "nothing kept once it ended");
    // Ids went on from where they were: the step sent after the restart has a newer one.
    let next: u64 = std::fs::read_to_string(profile.task_ids_path())
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(next > late_id + 1, "{next} after {late_id}");
    boss.shutdown().await;
}

/// A task on its own can go to a squad (`#name`): its least busy member that can run
/// it, as a plan step would; it moves on to another member when that one refuses it,
/// and its card names the squad and the member that ran it.
#[tokio::test]
async fn ui_gives_tasks_to_a_squad_member_and_moves_them_when_one_refuses() {
    let home = tempfile::tempdir().unwrap();
    let boss_key = SecretKey::generate();
    let grant = |name: &str, id: iroh::EndpointId, delegate: bool, per_hour: u32| TrustedPeer {
        name: name.into(),
        id,
        addrs: Default::default(),
        can_delegate: delegate,
        tasks_per_hour: per_hour,
        access: Access::ReadOnly,
        shares: vec!["data".into()],
        m365: None,
        github: None,
        agents: Vec::new(),
    };
    let mut config = AgentConfig::new("boss", boss_key.clone());
    config.network = Network::Local;
    config.bind = Some("127.0.0.1:0".parse().unwrap());
    let mut members = Vec::new();
    // m1 lets the boss delegate 0 tasks an hour, so it refuses them; m2 takes them.
    for (name, per_hour) in [("m1", 0), ("m2", 100)] {
        let key = SecretKey::generate();
        let mut member = AgentConfig::new(name, key.clone());
        member.network = Network::Local;
        member.bind = Some("127.0.0.1:0".parse().unwrap());
        member
            .trust
            .allow(grant("boss", boss_key.public(), true, per_hour));
        member.executor = Some(Arc::new(Echo));
        member.shares = ShareStore::with_data(home.path().join(name));
        config.trust.allow(grant(name, key.public(), false, 100));
        members.push(Agent::bind(member).await.unwrap());
    }
    let (boss, events) = Agent::bind(config).await.unwrap();
    for (member, _) in &members {
        boss.connect(member.addr()).await.unwrap();
    }
    tokio::time::timeout(Duration::from_secs(10), async {
        while boss.peers().len() < 2 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let info = UiInfo {
        profile: Profile::open_in(home.path(), "boss").unwrap(),
        network: Network::Local,
        serving: false,
        access: Access::ReadOnly,
        workspace: home.path().to_path_buf(),
        providers: Vec::new(),
        accounts: Vec::new(),
        workers: None,
    };
    let server = ui::start(
        boss.clone(),
        events,
        info,
        None,
        "127.0.0.1:0".parse().unwrap(),
    )
    .await
    .unwrap();
    let rest = server.url.strip_prefix("http://127.0.0.1:").unwrap();
    let (port, token) = rest.split_once("/#t=").unwrap();
    let port: u16 = port.parse().unwrap();
    let host = format!("127.0.0.1:{port}");
    let post = |path: &'static str, body: &'static str| {
        let host = host.clone();
        let token = token.to_owned();
        async move { request(port, "POST", path, &host, Some(&token), Some(body)).await }
    };
    let made = post("/api/squads", r#"{"name":"devs","members":["m1","m2"]}"#).await;
    assert_eq!(made.status, 200, "{}", made.body);

    let started = post(
        "/api/tasks",
        r##"{"provider":"copilot","target":"#devs","prompt":"count the files"}"##,
    )
    .await;
    assert_eq!(started.status, 200, "{}", started.body);
    let started = started.json();
    assert_eq!(
        (&started["target"], &started["squad"]),
        (&Value::from("m1"), &Value::from("devs")),
        "both are idle: the tie goes by name"
    );
    let task = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let state = request(port, "GET", "/api/state", &host, Some(token), None)
                .await
                .json();
            let task = state["tasks"][0].clone();
            if task["status"] != "running" {
                return task;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the task finishes");
    assert_eq!(task["status"], "done", "{task}");
    assert_eq!(task["target"], "m2", "m1 refused it, so it moved: {task}");
    assert_eq!(task["squad"], "devs");
    assert!(
        task["output"]
            .as_str()
            .unwrap()
            .contains("echo: count the files"),
        "{task}"
    );

    // A squad that doesn't exist, or that nobody in it can run the task for, is refused.
    let unknown = post(
        "/api/tasks",
        r##"{"provider":"copilot","target":"#ops","prompt":"x"}"##,
    )
    .await;
    assert_eq!(unknown.status, 400);
    assert!(
        unknown.body.contains("no squad named ops"),
        "{}",
        unknown.body
    );
    let claude = post(
        "/api/tasks",
        r##"{"provider":"claude","target":"#devs","prompt":"x"}"##,
    )
    .await;
    assert_eq!(claude.status, 400);
    assert!(
        claude
            .body
            .contains("no online member of squad devs can run it"),
        "{}",
        claude.body
    );
    boss.shutdown().await;
    for (member, _) in members {
        member.shutdown().await;
    }
}

fn delegating(name: &str, id: iroh::EndpointId) -> TrustedPeer {
    TrustedPeer {
        name: name.into(),
        id,
        addrs: Default::default(),
        can_delegate: true,
        tasks_per_hour: 100,
        access: Access::ReadOnly,
        shares: vec!["data".into()],
        m365: None,
        github: None,
        agents: Vec::new(),
    }
}

async fn wait_until(what: &str, done: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !done() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting until {what}"));
}

/// The owner picks the models and reasoning levels peers may ask this agent for in the
/// Models dialog, from those its account offers: checked, saved to profile.json and
/// applied at once (connected peers learn them). The same for a digital worker, which
/// may use its owner's settings instead.
#[tokio::test]
async fn owners_choose_the_models_peers_may_pick_in_the_ui() {
    let home = tempfile::tempdir().unwrap();
    let profile = Profile::open_in(home.path(), "worker").unwrap();
    profile.save().unwrap();
    let (worker_key, boss_key) = (SecretKey::generate(), SecretKey::generate());
    let mut config = AgentConfig::new("worker", worker_key.clone());
    config.network = Network::Local;
    config.bind = Some("127.0.0.1:0".parse().unwrap());
    config.models = profile.models();
    config.executor = Some(Arc::new(Echo));
    config.shares = ShareStore::with_data(home.path().join("data"));
    config.trust.allow(delegating("boss", boss_key.public()));
    let (worker, events) = Agent::bind(config).await.unwrap();
    let mut boss = AgentConfig::new("boss", boss_key);
    boss.network = Network::Local;
    boss.bind = Some("127.0.0.1:0".parse().unwrap());
    boss.trust.allow(delegating("worker", worker_key.public()));
    let (boss, _boss_events) = Agent::bind(boss).await.unwrap();
    boss.connect(worker.addr()).await.unwrap();
    wait_until("the boss is connected", || boss.peers().len() == 1).await;
    // A digital worker of it, on its accounts.
    let mut helper = Profile::open_in(home.path(), "helper").unwrap();
    helper.config.owner = Some("worker".into());
    helper.config.providers = vec![Provider::Copilot];
    helper.save().unwrap();
    let executor: Arc<dyn Executor> = Arc::new(Echo);
    let info = UiInfo {
        profile,
        network: Network::Local,
        serving: true,
        access: Access::ReadOnly,
        workspace: home.path().to_path_buf(),
        providers: vec![Provider::Copilot],
        accounts: Vec::new(),
        workers: None,
    };
    let server = ui::start(
        worker.clone(),
        events,
        info,
        Some(executor),
        "127.0.0.1:0".parse().unwrap(),
    )
    .await
    .unwrap();
    let rest = server.url.strip_prefix("http://127.0.0.1:").unwrap();
    let (port, token) = rest.split_once("/#t=").unwrap();
    let port: u16 = port.parse().unwrap();
    let host = format!("127.0.0.1:{port}");
    let call = |method: &'static str, path: &'static str, body: Option<&'static str>| {
        let (host, token) = (host.clone(), token.to_owned());
        async move { request(port, method, path, &host, Some(&token), body).await }
    };
    let saved_models = |name: &str| Profile::open_in(home.path(), name).unwrap().config.models;

    // The dialog lists what its account offers.
    let got = call("GET", "/api/models", None).await;
    assert_eq!(got.status, 200, "{}", got.body);
    let got = got.json();
    assert_eq!(got["providers"], serde_json::json!(["copilot"]));
    let listed: Vec<&str> = got["listed"]["copilot"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(listed, ["echo-big", "echo-small"]);

    // Only echo-big, at low or high reasoning (high unless asked): saved and applied, and
    // the boss learns it, with only that model offered.
    let only = r#"{"provider":"copilot","default":"echo-big","allowed":["echo-big"],"effort":"high","efforts":["high","low"]}"#;
    let saved = call("PUT", "/api/models", Some(only)).await;
    assert_eq!(saved.status, 200, "{}", saved.body);
    let kept = saved_models("worker");
    assert_eq!(kept[&Provider::Copilot].efforts, ["low", "high"]);
    assert_eq!(worker.models(), kept);
    wait_until("the boss learns them", || {
        let peer = &boss.peers()[0];
        peer.models == kept && peer.offers[&Provider::Copilot].len() == 1
    })
    .await;
    let me = call("GET", "/api/state", None).await.json()["me"].clone();
    assert_eq!(
        me["models"]["copilot"]["efforts"],
        serde_json::json!(["low", "high"])
    );
    assert_eq!(me["offers"]["copilot"][0]["id"], "echo-big");

    // A default it doesn't allow, or a provider it doesn't run, is refused: nothing changes.
    let bad = call(
        "PUT",
        "/api/models",
        Some(r#"{"provider":"copilot","default":"echo-small","allowed":["echo-big"]}"#),
    )
    .await;
    assert_eq!(bad.status, 400);
    assert!(bad.body.contains("allow it first"), "{}", bad.body);
    let claude = call("PUT", "/api/models", Some(r#"{"provider":"claude"}"#)).await;
    assert_eq!(claude.status, 400, "{}", claude.body);
    assert_eq!(saved_models("worker"), kept);

    // A digital worker: its own settings, or its owner's.
    let got = call("GET", "/api/workers/helper/models", None).await;
    assert_eq!(got.status, 200, "{}", got.body);
    let got = got.json();
    assert_eq!(got["owner"], "worker");
    assert_eq!(got["models"], serde_json::json!({}));
    assert_eq!(got["owner_models"]["copilot"]["default"], "echo-big");
    assert_eq!(got["listed"]["copilot"].as_array().unwrap().len(), 2);
    let own = r#"{"provider":"copilot","allowed":["echo-small"]}"#;
    let set = call("PUT", "/api/workers/helper/models", Some(own)).await;
    assert_eq!(set.status, 200, "{}", set.body);
    assert_eq!(
        saved_models("helper")[&Provider::Copilot].allowed,
        ["echo-small"]
    );
    let inherit = r#"{"provider":"copilot","inherit":true}"#;
    let set = call("PUT", "/api/workers/helper/models", Some(inherit)).await;
    assert_eq!(set.status, 200, "{}", set.body);
    assert!(saved_models("helper").is_empty(), "its owner's again");
    let nobody = call("GET", "/api/workers/nobody/models", None).await;
    assert_eq!(nobody.status, 400, "{}", nobody.body);
    boss.shutdown().await;
    worker.shutdown().await;
}

/// A task for Any peer that names a model goes to a peer that allows it (their hello
/// says which), not simply the first.
#[tokio::test]
async fn any_peer_tasks_go_to_a_peer_that_allows_their_model() {
    let home = tempfile::tempdir().unwrap();
    let boss_key = SecretKey::generate();
    let mut config = AgentConfig::new("boss", boss_key.clone());
    config.network = Network::Local;
    config.bind = Some("127.0.0.1:0".parse().unwrap());
    let mut members = Vec::new();
    for (name, allowed) in [("m1", vec!["echo-a".to_owned()]), ("m2", Vec::new())] {
        let key = SecretKey::generate();
        let mut member = AgentConfig::new(name, key.clone());
        member.network = Network::Local;
        member.bind = Some("127.0.0.1:0".parse().unwrap());
        member.trust.allow(delegating("boss", boss_key.public()));
        member.executor = Some(Arc::new(Echo));
        member.shares = ShareStore::with_data(home.path().join(name));
        proofkin::models::set_allowed(&mut member.models, Provider::Copilot, &allowed).unwrap();
        config.trust.allow(delegating(name, key.public()));
        members.push(Agent::bind(member).await.unwrap());
    }
    let (boss, events) = Agent::bind(config).await.unwrap();
    for (member, _) in &members {
        boss.connect(member.addr()).await.unwrap();
    }
    wait_until("both are connected", || boss.peers().len() == 2).await;
    let info = UiInfo {
        profile: Profile::open_in(home.path(), "boss").unwrap(),
        network: Network::Local,
        serving: false,
        access: Access::ReadOnly,
        workspace: home.path().to_path_buf(),
        providers: Vec::new(),
        accounts: Vec::new(),
        workers: None,
    };
    let server = ui::start(
        boss.clone(),
        events,
        info,
        None,
        "127.0.0.1:0".parse().unwrap(),
    )
    .await
    .unwrap();
    let rest = server.url.strip_prefix("http://127.0.0.1:").unwrap();
    let (port, token) = rest.split_once("/#t=").unwrap();
    let port: u16 = port.parse().unwrap();
    let host = format!("127.0.0.1:{port}");
    let start = |body: &'static str| {
        let (host, token) = (host.clone(), token.to_owned());
        async move {
            let res = request(port, "POST", "/api/tasks", &host, Some(&token), Some(body)).await;
            assert_eq!(res.status, 200, "{}", res.body);
            res.json()["target"].clone()
        }
    };
    let picky = start(r#"{"provider":"copilot","target":"auto","prompt":"x","model":"echo-b"}"#);
    assert_eq!(picky.await, "m2", "m1 allows only echo-a");
    let plain = start(r#"{"provider":"copilot","target":"auto","prompt":"x"}"#);
    assert_eq!(plain.await, "m1", "any of them: the first");
    boss.shutdown().await;
    for (member, _) in members {
        member.shutdown().await;
    }
}

#[tokio::test]
async fn ui_runs_plans_and_shows_squads() {
    let home = tempfile::tempdir().unwrap();
    let (boss_key, worker_key) = (SecretKey::generate(), SecretKey::generate());
    let trust = |name: &str, key: &SecretKey, delegate: bool| TrustedPeer {
        name: name.into(),
        id: key.public(),
        addrs: Default::default(),
        can_delegate: delegate,
        tasks_per_hour: 100,
        access: Access::ReadOnly,
        shares: vec!["data".into()],
        m365: None,
        github: None,
        agents: Vec::new(),
    };
    let mut worker = AgentConfig::new("worker", worker_key.clone());
    worker.network = Network::Local;
    worker.bind = Some("127.0.0.1:0".parse().unwrap());
    worker.trust.allow(trust("boss", &boss_key, true));
    worker.executor = Some(Arc::new(Echo));
    worker.shares = ShareStore::with_data(home.path().join("data"));
    let (worker, _we) = Agent::bind(worker).await.unwrap();

    let mut config = AgentConfig::new("boss", boss_key);
    config.network = Network::Local;
    config.bind = Some("127.0.0.1:0".parse().unwrap());
    config.trust.allow(trust("worker", &worker_key, false));
    let (boss, events) = Agent::bind(config).await.unwrap();
    boss.connect(worker.addr()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while boss.peers().is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let info = UiInfo {
        profile: Profile::open_in(home.path(), "boss").unwrap(),
        network: Network::Local,
        serving: false,
        access: Access::ReadOnly,
        workspace: home.path().to_path_buf(),
        providers: Vec::new(),
        accounts: Vec::new(),
        workers: None,
    };
    let server = ui::start(
        boss.clone(),
        events,
        info,
        None,
        "127.0.0.1:0".parse().unwrap(),
    )
    .await
    .unwrap();
    let rest = server.url.strip_prefix("http://127.0.0.1:").unwrap();
    let (port, token) = rest.split_once("/#t=").unwrap();
    let port: u16 = port.parse().unwrap();
    let host = format!("127.0.0.1:{port}");
    let post = |path: &'static str, body: String| {
        let host = host.clone();
        let token = token.to_owned();
        async move { request(port, "POST", path, &host, Some(&token), Some(&body)).await }
    };

    // Squads: only allowlisted members; shown with who is online.
    let bad = post(
        "/api/squads",
        r#"{"name":"qa","members":["stranger"]}"#.into(),
    )
    .await;
    assert_eq!(bad.status, 400);
    let made = post(
        "/api/squads",
        r#"{"name":"qa","members":["worker"]}"#.into(),
    )
    .await;
    assert_eq!(made.status, 200, "{}", made.body);
    let state = request(port, "GET", "/api/state", &host, Some(token), None)
        .await
        .json();
    assert_eq!(state["squads"][0]["name"], "qa");
    assert_eq!(state["squads"][0]["members"][0]["online"], true);
    assert!(
        home.path().join("boss/squads.json").is_file(),
        "saved to the profile"
    );

    // Plans: invalid ones are rejected before anything runs.
    let cycle = post(
        "/api/plans",
        r#"{"text":"a after b @worker x\nb after a @worker y"}"#.into(),
    )
    .await;
    assert_eq!(cycle.status, 400);
    assert!(cycle.body.contains("cycle"), "{}", cycle.body);
    let text = serde_json::json!({ "text": "find @worker find the facts\nsum after find #qa summarize them" });
    let started = post("/api/plans", text.to_string()).await;
    assert_eq!(started.status, 200, "{}", started.body);

    let tasks = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let state = request(port, "GET", "/api/state", &host, Some(token), None)
                .await
                .json();
            let tasks = state["tasks"].as_array().unwrap().clone();
            if tasks.len() == 2 && tasks.iter().all(|t| t["status"] == "done") {
                return tasks;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("plan finished");
    let step = |id: &str| tasks.iter().find(|t| t["step"] == id).unwrap().clone();
    assert_eq!(step("find")["target"], "worker");
    assert_eq!(step("sum")["after"][0], "find");
    let sum = step("sum")["output"].as_str().unwrap().to_owned();
    assert!(
        sum.contains(
            "### Step find (done by worker)\n<external_content source=\"step:find peer:worker\">\nIts task:\nfind the facts\n\nIts result:\necho: find the facts\n</external_content>"
        ),
        "{sum}"
    );
    assert_eq!(step("find")["evidence"], "verified", "signed by the worker");

    // An evidence run with a sign-off by this agent's own user (@me).
    let runs = || {
        let host = host.clone();
        let token = token.to_owned();
        async move {
            request(port, "GET", "/api/runs", &host, Some(&token), None)
                .await
                .json()["runs"]
                .as_array()
                .unwrap()
                .clone()
        }
    };
    assert_eq!(runs().await.len(), 1, "the first plan was saved as a run");
    let text = serde_json::json!({
        "text": "claim: C-1 the facts are reviewed\nfind @worker find the facts\nok after find approve @me claims=C-1 Approve the facts"
    });
    assert_eq!(post("/api/plans", text.to_string()).await.status, 200);
    let approval = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let state = request(port, "GET", "/api/state", &host, Some(token), None)
                .await
                .json();
            if let Some(a) = state["approvals"].as_array().and_then(|a| a.first()) {
                return a.clone();
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("approval requested");
    assert!(
        approval["text"]
            .as_str()
            .unwrap()
            .contains("echo: find the facts")
    );
    let path: &'static str =
        Box::leak(format!("/api/approvals/{}", approval["id"]).into_boxed_str());
    let no_reason = post(path, r#"{"approved":false,"note":" "}"#.into()).await;
    assert_eq!(no_reason.status, 400, "a rejection needs a reason");
    let approved = post(path, r#"{"approved":true,"note":"checked"}"#.into()).await;
    assert_eq!(approved.status, 200, "{}", approved.body);
    let run = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let runs = runs().await;
            if runs.len() == 2 {
                return runs[0].clone();
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("run saved");
    assert_eq!(run["passed"], true, "{run}");
    let id = run["id"].as_str().unwrap();
    let detail = request(
        port,
        "GET",
        &format!("/api/runs/{id}"),
        &host,
        Some(token),
        None,
    )
    .await
    .json();
    assert!(
        detail["verdict"]
            .as_str()
            .unwrap()
            .starts_with("PASS: 1/1 claims proven")
    );
    assert!(
        detail["report"]
            .as_str()
            .unwrap()
            .contains("Approved by boss: checked")
    );
    let signatures = detail["signatures"].as_array().unwrap();
    assert_eq!(signatures.len(), 2);
    assert!(signatures.iter().all(|s| s["ok"] == true), "{signatures:?}");
    assert!(
        home.path()
            .join("boss/runs")
            .join(id)
            .join("report.md")
            .is_file()
    );
    let missing = request(port, "GET", "/api/runs/nope", &host, Some(token), None).await;
    assert_eq!(missing.status, 404);

    // Scheduled plans: added here, run on request as evidence runs, paused, deleted.
    let schedule = |when: &str| {
        serde_json::json!({ "name": "nightly", "when": when, "plan": "find @worker find the facts" })
            .to_string()
    };
    let too_often = post("/api/schedules", schedule("every 1m")).await;
    assert_eq!(too_often.status, 400, "{}", too_often.body);
    let preview = post("/api/schedule-preview", r#"{"when":"every 2h"}"#.into())
        .await
        .json();
    assert_eq!(preview["ok"], true, "{preview}");
    assert_eq!(preview["parts"]["every_minutes"], 120);
    assert_eq!(preview["next"].as_array().unwrap().len(), 3);
    let bad = post("/api/schedule-preview", r#"{"when":"every 1m"}"#.into())
        .await
        .json();
    assert!(
        bad["error"].as_str().unwrap().contains("5 minutes"),
        "{bad}"
    );
    let added = post("/api/schedules", schedule("daily 02:00")).await;
    assert_eq!(added.status, 200, "{}", added.body);
    let added = added.json();
    assert_eq!(added["schedules"][0]["enabled"], true);
    assert!(added["schedules"][0]["next_unix"].as_i64().is_some());
    assert_eq!(
        added["schedules"][0]["next_runs"].as_array().unwrap().len(),
        3
    );
    assert_eq!(added["schedules"][0]["when_parts"]["time"], "02:00");
    let twice = post("/api/schedules", schedule("daily 03:00")).await;
    assert!(twice.body.contains("already exists"), "{}", twice.body);
    let edit = |body: &'static str| {
        request(
            port,
            "PUT",
            "/api/schedules/nightly",
            &host,
            Some(token),
            Some(body),
        )
    };
    let edited = edit(r#"{"when":"weekdays 08:30"}"#).await.json();
    assert_eq!(edited["schedules"][0]["when"], "weekdays 08:30");
    assert_eq!(edited["schedules"][0]["when_parts"]["days"][0], "mon");
    assert_eq!(edit(r#"{"plan":"not a plan"}"#).await.status, 400);
    let ran = post("/api/schedules/nightly/run", String::new()).await;
    assert_eq!(ran.status, 200, "{}", ran.body);
    let last = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let listed = request(port, "GET", "/api/schedules", &host, Some(token), None)
                .await
                .json();
            let last = listed["schedules"][0]["last_result"].clone();
            if last.as_str().is_some_and(|l| l != "running") {
                return last;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("scheduled run finished");
    assert!(last.as_str().unwrap().contains("1/1 steps done"), "{last}");
    assert_eq!(
        runs().await.len(),
        3,
        "the scheduled run is an evidence run"
    );
    let paused = request(
        port,
        "PATCH",
        "/api/schedules/nightly",
        &host,
        Some(token),
        Some(r#"{"enabled":false}"#),
    )
    .await
    .json();
    assert_eq!(paused["schedules"][0]["enabled"], false);
    assert!(paused["schedules"][0]["next_unix"].is_null());
    let deleted = request(
        port,
        "DELETE",
        "/api/schedules/nightly",
        &host,
        Some(token),
        None,
    )
    .await
    .json();
    assert_eq!(deleted["schedules"], serde_json::json!([]));

    // A task on its own that asks for checks is kept as a one-step evidence run: its
    // card links to it, and it downloads as the run.json anyone can verify.
    proofkin::spec::init(&home.path().join("data"), "Demo", true).unwrap();
    let worker_id = worker.id().to_string();
    let task = |target: &str, attempts: u32| {
        serde_json::json!({
            "provider": "copilot", "target": target, "prompt": "check the spec",
            "checks": ["spec:spec"], "attempts": attempts,
        })
        .to_string()
    };
    let local = post("/api/tasks", task("local", 1)).await;
    assert!(local.body.contains("peer's runtime"), "{}", local.body);
    assert_eq!(post("/api/tasks", task(&worker_id, 9)).await.status, 400);
    let started = post("/api/tasks", task(&worker_id, 2)).await;
    assert_eq!(started.status, 200, "{}", started.body);
    let id = started.json()["id"].clone();
    let done = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let state = request(port, "GET", "/api/state", &host, Some(token), None)
                .await
                .json();
            let task = state["tasks"]
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["id"] == id && t["run"].is_string())
                .cloned();
            if let Some(task) = task {
                return (task, state);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the task's run was saved");
    let (done, state) = done;
    assert_eq!(done["status"], "done", "{done}");
    assert_eq!(done["evidence"], "verified");
    assert!(
        state["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|t| t["step"].is_string())
            .all(|t| t["run"].is_string()),
        "plan steps link to their run too"
    );
    let run_id = done["run"].as_str().unwrap().to_owned();
    let run = runs()
        .await
        .into_iter()
        .find(|r| r["id"] == run_id.as_str())
        .unwrap();
    assert_eq!(run["passed"], true, "{run}");
    assert_eq!(run["title"], "spec:spec passes after: check the spec");
    let path = format!("/api/runs/{run_id}/file");
    assert_eq!(
        request(port, "GET", &path, &host, None, None).await.status,
        401
    );
    let file = request(port, "GET", &path, &host, Some(token), None).await;
    assert_eq!(file.status, 200);
    assert!(
        file.headers.contains(&format!(
            "content-disposition: attachment; filename=\"{run_id}.json\""
        )),
        "{}",
        file.headers
    );
    let saved =
        std::fs::read_to_string(home.path().join("boss/runs").join(&run_id).join("run.json"))
            .unwrap();
    assert_eq!(file.body, saved, "as saved, so its signatures still verify");
    let traversal = "/api/runs/..%2Fboss/file";
    assert_eq!(
        request(port, "GET", traversal, &host, Some(token), None)
            .await
            .status,
        404
    );

    let removed = request(port, "DELETE", "/api/squads/qa", &host, Some(token), None).await;
    assert_eq!(removed.status, 200);
    boss.shutdown().await;
    worker.shutdown().await;
}

#[tokio::test]
async fn a_running_plan_step_keeps_its_folder_in_its_target() {
    let home = tempfile::tempdir().unwrap();
    let docs = tempfile::tempdir().unwrap();
    let (boss_key, worker_key) = (SecretKey::generate(), SecretKey::generate());
    let trust = |name: &str, key: &SecretKey, delegate: bool| TrustedPeer {
        name: name.into(),
        id: key.public(),
        addrs: Default::default(),
        can_delegate: delegate,
        tasks_per_hour: 100,
        access: Access::ReadOnly,
        shares: vec!["data".into(), "docs".into()],
        m365: None,
        github: None,
        agents: Vec::new(),
    };
    let mut worker = AgentConfig::new("worker", worker_key.clone());
    worker.network = Network::Local;
    worker.bind = Some("127.0.0.1:0".parse().unwrap());
    worker.trust.allow(trust("boss", &boss_key, true));
    worker.executor = Some(Arc::new(Echo));
    worker.shares = ShareStore::with_data(home.path().join("data"));
    worker
        .shares
        .add("docs", docs.path(), Access::ReadOnly)
        .unwrap();
    let (worker, _we) = Agent::bind(worker).await.unwrap();

    let mut config = AgentConfig::new("boss", boss_key);
    config.network = Network::Local;
    config.bind = Some("127.0.0.1:0".parse().unwrap());
    config.trust.allow(trust("worker", &worker_key, false));
    let (boss, events) = Agent::bind(config).await.unwrap();
    boss.connect(worker.addr()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while boss.peers().is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let info = UiInfo {
        profile: Profile::open_in(home.path(), "boss").unwrap(),
        network: Network::Local,
        serving: false,
        access: Access::ReadOnly,
        workspace: home.path().to_path_buf(),
        providers: Vec::new(),
        accounts: Vec::new(),
        workers: None,
    };
    let server = ui::start(boss, events, info, None, "127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let rest = server.url.strip_prefix("http://127.0.0.1:").unwrap();
    let (port, token) = rest.split_once("/#t=").unwrap();
    let port: u16 = port.parse().unwrap();
    let host = format!("127.0.0.1:{port}");

    let text = serde_json::json!({ "text": "hold @worker:docs hang" }).to_string();
    let started = request(port, "POST", "/api/plans", &host, Some(token), Some(&text)).await;
    assert_eq!(started.status, 200, "{}", started.body);
    let target = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let state = request(port, "GET", "/api/state", &host, Some(token), None)
                .await
                .json();
            let running = state["tasks"]
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["status"] == "running");
            if let Some(t) = running {
                return t["target"].clone();
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the step started");
    assert_eq!(target, "worker:docs");

    // A finished step keeps its folder too.
    let text = serde_json::json!({ "text": "fin @worker:docs hello" }).to_string();
    let started = request(port, "POST", "/api/plans", &host, Some(token), Some(&text)).await;
    assert_eq!(started.status, 200, "{}", started.body);
    let target = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let state = request(port, "GET", "/api/state", &host, Some(token), None)
                .await
                .json();
            let done = state["tasks"]
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["status"] == "done");
            if let Some(t) = done {
                return t["target"].clone();
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the step finished");
    assert_eq!(target, "worker:docs");
}

#[tokio::test]
async fn ui_shows_health_knowledge_and_fills_in_playbooks() {
    use proofkin::ops::{IncidentEvent, IncidentKind, Monitor, MonitorStore, Sample};
    let home = tempfile::tempdir().unwrap();
    let profile = Profile::open_in(home.path(), "solo").unwrap();
    let app = tempfile::tempdir().unwrap();
    let mut shares = ShareStore::with_data(home.path().join("data"));
    let checks = [
        ("health".to_owned(), "exit 0".to_owned()),
        ("ping".to_owned(), "exit 0".to_owned()),
    ]
    .into();
    shares
        .add_with("app", app.path(), Access::Full, checks, false)
        .unwrap();
    let mut monitors = MonitorStore::default();
    let monitor = Monitor {
        name: "api".into(),
        share: "app".into(),
        check: "health".into(),
        every_secs: 60,
        fail_after: 3,
        slo: 99.0,
    };
    monitors.set(monitor.clone(), &shares).unwrap();
    let web = Monitor {
        name: "web".into(),
        check: "ping".into(),
        ..monitor
    };
    monitors.set(web, &shares).unwrap();
    monitors.save(&profile.monitors_path()).unwrap();
    // The 'ping' check is removed later: the 'web' monitor can't run any more.
    shares
        .add_with(
            "app",
            app.path(),
            Access::Full,
            [("health".to_owned(), "exit 0".to_owned())].into(),
            false,
        )
        .unwrap();
    let now = proofkin::runs::unix_now();
    let store = profile.ops_store();
    for ok in [true, false] {
        store
            .record_sample(&Sample {
                ts: now,
                monitor: "api".into(),
                ok,
                ms: 120,
                exit: Some(if ok { 0 } else { 1 }),
                pass_rate: None,
            })
            .unwrap();
    }
    // The same failure opened twice (as agents before 0.8.1 did after a restart).
    for incident in [now - 60, now] {
        store
            .record_incident(&IncidentEvent {
                ts: incident,
                incident,
                monitor: "api".into(),
                kind: IncidentKind::Opened,
                text: "api failed".into(),
                checks: vec![],
            })
            .unwrap();
    }
    // A factory agent's drift watch opened one too; a monitor removed since left one behind.
    for (incident, monitor) in [(now - 20, "factory:notes"), (now - 10, "gone")] {
        store
            .record_incident(&IncidentEvent {
                ts: incident,
                incident,
                monitor: monitor.into(),
                kind: IncidentKind::Opened,
                text: format!("{monitor} failed"),
                checks: vec![],
            })
            .unwrap();
    }
    profile
        .knowledge()
        .write("architecture", "Two services.", "test")
        .unwrap();
    // Like the rest of the card, evidence runs count for the last 24 hours only.
    let plan = proofkin::plan::Plan::parse("a @x Do it").unwrap();
    for (id, started) in [("old-run", now - 3 * 86_400), ("new-run", now - 60)] {
        let mut run = proofkin::runs::RunRecord::new(
            "solo",
            "id",
            "a @x Do it",
            &plan,
            &Default::default(),
            started,
        );
        run.id = id.into();
        run.save(&profile.runs_dir()).unwrap();
    }

    let mut config = AgentConfig::new("solo", SecretKey::generate());
    config.network = Network::Local;
    config.bind = Some("127.0.0.1:0".parse().unwrap());
    config.shares = shares;
    let (agent, events) = Agent::bind(config).await.unwrap();
    let info = UiInfo {
        profile,
        network: Network::Local,
        serving: false,
        access: Access::ReadOnly,
        workspace: home.path().to_path_buf(),
        providers: Vec::new(),
        accounts: Vec::new(),
        workers: None,
    };
    let server = ui::start(
        agent.clone(),
        events,
        info,
        None,
        "127.0.0.1:0".parse().unwrap(),
    )
    .await
    .unwrap();
    let rest = server.url.strip_prefix("http://127.0.0.1:").unwrap();
    let (port, token) = rest.split_once("/#t=").unwrap();
    let port: u16 = port.parse().unwrap();
    let host = format!("127.0.0.1:{port}");
    let get = |path: &str| {
        let (host, token, path) = (host.clone(), token.to_owned(), path.to_owned());
        async move { request(port, "GET", &path, &host, Some(&token), None).await }
    };

    assert_eq!(
        request(port, "GET", "/api/ops", &host, None, None)
            .await
            .status,
        401
    );
    let ops = get("/api/ops").await.json();
    assert_eq!(ops["monitors"][0]["name"], "api");
    assert_eq!(ops["monitors"][0]["availability"], 50.0);
    assert_eq!(ops["monitors"][0]["meets_slo"], false);
    assert!(ops["monitors"][0]["problem"].is_null());
    assert_eq!(
        ops["open_incidents"], 2,
        "one ongoing problem per monitor, however often it was opened, and the drift watch's"
    );
    assert_eq!(
        ops["monitors"][1]["problem"],
        "folder 'app' has no check 'ping' any more"
    );
    assert_eq!(ops["incidents"][0]["kind"], "opened");
    assert_eq!(
        ops["runs"]["total"], 1,
        "only the last 24 hours' runs: {}",
        ops["runs"]
    );
    assert_eq!(
        (
            &ops["runs"]["passed"],
            &ops["runs"]["failed"],
            &ops["runs"]["unclaimed"]
        ),
        (
            &serde_json::json!(0),
            &serde_json::json!(0),
            &serde_json::json!(1)
        ),
        "a run without claims neither passed nor failed: {}",
        ops["runs"]
    );
    assert_eq!(ops["knowledge"][0]["name"], "architecture");
    let topic = get("/api/kb/architecture").await.json();
    assert!(topic["text"].as_str().unwrap().contains("Two services."));
    assert_eq!(get("/api/kb/missing").await.status, 404);

    let books = get("/api/playbooks").await.json();
    let names: Vec<&str> = books["playbooks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "bootstrap",
            "discover",
            "sdlc",
            "learn",
            "spec",
            "design",
            "implement-spec",
            "implement-speckit",
            "rehearse",
            "agent-factory"
        ]
    );
    let discover = &books["playbooks"][1]["outline"];
    assert!(
        discover["steps"][0]
            .as_str()
            .unwrap()
            .contains("the peer you pick")
    );
    assert_eq!(discover["claims"].as_array().unwrap().len(), 4);
    let post = |path: &str, body: &str| {
        let (host, token, path, body) = (
            host.clone(),
            token.to_owned(),
            path.to_owned(),
            body.to_owned(),
        );
        async move { request(port, "POST", &path, &host, Some(&token), Some(&body)).await }
    };
    let filled = post(
        "/api/playbooks/discover",
        r#"{"vars":{"peer":"alice","folder":"repo"}}"#,
    )
    .await;
    assert_eq!(filled.status, 200, "{}", filled.body);
    assert!(
        filled.json()["text"]
            .as_str()
            .unwrap()
            .contains("@alice:repo")
    );
    let missing = post("/api/playbooks/sdlc", r#"{"vars":{"peer":"alice"}}"#).await;
    assert_eq!(missing.status, 400);
    assert!(missing.body.contains("needs: change"), "{}", missing.body);

    // Plans can only use knowledge that exists.
    let unknown = post("/api/plans", r#"{"text":"a @alice context=nope do it"}"#).await;
    assert_eq!(unknown.status, 400);
    assert!(unknown.body.contains("no topic 'nope'"), "{}", unknown.body);

    // The plan builder offers the built-in checks, each with what it proves.
    let builtin = get("/api/state").await.json()["builtin_checks"].clone();
    assert!(
        builtin
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["name"] == "spec:api"
                && c["about"].as_str().unwrap().contains("spec/openapi.json")),
        "{builtin}"
    );

    // The plan check lists every problem without running anything.
    let checked = post(
        "/api/plans/check",
        r#"{"text":"claim: C-1 x\na @alice skill=nope do it"}"#,
    )
    .await
    .json();
    assert_eq!(checked["ok"], false);
    let messages: Vec<String> = checked["problems"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["message"].as_str().unwrap().to_owned())
        .collect();
    assert!(
        messages
            .iter()
            .any(|m| m.contains("alice is not connected")),
        "{messages:?}"
    );
    assert!(
        messages.iter().any(|m| m.contains("no skill 'nope'")),
        "{messages:?}"
    );
    assert!(
        messages
            .iter()
            .any(|m| m.starts_with("Nothing proves C-1, so it will end unverified.")),
        "{messages:?}"
    );
    // With what fixes it, for the page to offer as buttons.
    let unproven = checked["problems"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| {
            p["message"]
                .as_str()
                .unwrap()
                .starts_with("Nothing proves C-1")
        })
        .unwrap();
    assert_eq!(
        unproven["fixes"],
        serde_json::json!([
            { "label": "Prove C-1 with your sign-off", "edits": [{ "do": "add_line", "after": "a",
                "line": "review after a approve @me claims=C-1 Approve if C-1 holds: x" }] },
            { "label": "Remove claim C-1", "edits": [{ "do": "remove_claim", "claim": "C-1" }] },
        ])
    );
    let ok = post(
        "/api/plans/check",
        r#"{"text":"ok approve @me Approve it"}"#,
    )
    .await
    .json();
    assert_eq!(ok["ok"], true, "{ok}");
    let broken = post("/api/plans/check", r#"{"text":"nonsense"}"#)
        .await
        .json();
    assert_eq!(broken["ok"], false);

    // Skills: built-in ones, viewable.
    let skills = get("/api/skills").await.json();
    assert!(
        skills["skills"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["name"] == "security-review")
    );
    let skill = get("/api/skills/security-review").await.json();
    assert!(
        skill["text"]
            .as_str()
            .unwrap()
            .contains("# Security review")
    );
    assert_eq!(get("/api/skills/nope").await.status, 404);

    // Monitors are managed in the Health card: each says what it checks, the card
    // offers the folders' checks a new one can run, and monitors are added and
    // removed there (a removed one's open incidents close; the agent follows both).
    assert_eq!(ops["monitors"][0]["share"], "app");
    assert_eq!(ops["monitors"][0]["check"], "health");
    assert_eq!(ops["monitors"][0]["every_secs"], 60);
    assert_eq!(
        ops["checks"],
        serde_json::json!([{ "folder": "app", "checks": { "health": "exit 0" } }])
    );
    let unknown = post(
        "/api/monitors",
        r#"{"name":"db","share":"app","check":"nope","every_secs":60}"#,
    )
    .await;
    assert_eq!(unknown.status, 400);
    assert!(unknown.body.contains("no check 'nope'"), "{}", unknown.body);
    let taken = post(
        "/api/monitors",
        r#"{"name":"api","share":"app","check":"health","every_secs":60}"#,
    )
    .await;
    assert_eq!(taken.status, 400);
    assert!(
        taken.body.contains("already a monitor 'api'"),
        "{}",
        taken.body
    );
    let added = post(
        "/api/monitors",
        r#"{"name":"db","share":"app","check":"health","every_secs":300}"#,
    )
    .await;
    assert_eq!(added.status, 200, "{}", added.body);
    let ops = get("/api/ops").await.json();
    let db = ops["monitors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["name"] == "db")
        .unwrap()
        .clone();
    assert_eq!(
        (
            db["every_secs"].as_u64(),
            db["fail_after"].as_u64(),
            db["slo"].as_f64()
        ),
        (Some(300), Some(3), Some(99.0)),
        "{db}"
    );
    assert_eq!(
        request(port, "DELETE", "/api/monitors/api", &host, None, None)
            .await
            .status,
        401
    );
    let removed = request(
        port,
        "DELETE",
        "/api/monitors/api",
        &host,
        Some(token),
        None,
    )
    .await;
    assert_eq!(removed.status, 200, "{}", removed.body);
    let again = request(
        port,
        "DELETE",
        "/api/monitors/api",
        &host,
        Some(token),
        None,
    )
    .await;
    assert_eq!(again.status, 404);
    let ops = get("/api/ops").await.json();
    let names: Vec<&str> = ops["monitors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["db", "web"]);
    assert_eq!(ops["open_incidents"], 1, "the drift watch's is still open");
    let open = proofkin::ops::open_incidents(&store.incidents(0));
    assert!(
        !open.values().any(|m| m == "api"),
        "the removed monitor's incidents are closed: {open:?}"
    );

    // GitHub: not signed in yet, so repositories can't be linked.
    let github = get("/api/github").await.json();
    assert!(github["account"].is_null() && github["repos"].as_array().unwrap().is_empty());
    assert!(github["flow"].is_null(), "no GitHub CLI sign-in waits");
    let link = post("/api/repos", r#"{"repo":"acme/app"}"#).await;
    assert_eq!(link.status, 400);
    assert!(
        link.body.contains("not signed in to GitHub"),
        "{}",
        link.body
    );
    // Only a linked repository's task branches are reviewed.
    let changes = get("/api/repos/app/changes?branch=p2p%2Fboss%2F1-1").await;
    assert_eq!(changes.status, 400);
    assert!(
        changes.body.contains("no linked repository 'app'"),
        "{}",
        changes.body
    );
    // Granting a scope needs a sign-in too, and a new sign-in's host and scopes are
    // checked before the GitHub CLI starts.
    let grant = post("/api/github/grant", r#"{"scopes":["project"]}"#).await;
    assert_eq!(grant.status, 400);
    assert!(
        grant.body.contains("sign in to GitHub first"),
        "{}",
        grant.body
    );
    for body in [
        r#"{"host":"github.com --web"}"#,
        r#"{"scopes":["project --admin"]}"#,
    ] {
        let new = post("/api/github/login/new", body).await;
        assert_eq!(new.status, 400, "{body}: {}", new.body);
    }
    assert!(get("/api/github/flow").await.json()["flow"].is_null());
    // No tracker linked: the plan's "Existing issue" choice can't look an issue up.
    let lookup = get("/api/tracker/issues/12").await;
    assert_eq!(lookup.status, 400);
    assert!(
        lookup.body.contains("no tracker is linked"),
        "{}",
        lookup.body
    );
    assert_eq!(get("/api/tracker/issues/twelve").await.status, 400);
    agent.shutdown().await;
}

#[tokio::test]
async fn ui_previews_prototypes_and_review_packs_in_a_sandbox_by_capability() {
    let home = tempfile::tempdir().unwrap();
    let app = tempfile::tempdir().unwrap();
    proofkin::spec::init(app.path(), "Wishlist", false).unwrap();
    let mut shares = ShareStore::with_data(home.path().join("data"));
    shares
        .add_with(
            "app",
            app.path(),
            Access::ReadOnly,
            Default::default(),
            false,
        )
        .unwrap();
    let mut browser = proofkin::mcp::McpServer::local("npx", vec!["@playwright/mcp".into()]);
    browser
        .env
        .insert("API_TOKEN".into(), "mcp-secret-6620".into());
    shares.add_mcp("app", "browser", browser).unwrap();
    let mut config = AgentConfig::new("solo", SecretKey::generate());
    config.network = Network::Local;
    config.bind = Some("127.0.0.1:0".parse().unwrap());
    config.shares = shares;
    let (agent, events) = Agent::bind(config).await.unwrap();
    let info = UiInfo {
        profile: Profile::open_in(home.path(), "solo").unwrap(),
        network: Network::Local,
        serving: true,
        access: Access::ReadOnly,
        workspace: home.path().to_path_buf(),
        providers: Vec::new(),
        accounts: Vec::new(),
        workers: None,
    };
    let server = ui::start(
        agent.clone(),
        events,
        info,
        None,
        "127.0.0.1:0".parse().unwrap(),
    )
    .await
    .unwrap();
    let rest = server.url.strip_prefix("http://127.0.0.1:").unwrap();
    let (port, token) = rest.split_once("/#t=").unwrap();
    let port: u16 = port.parse().unwrap();
    let host = format!("127.0.0.1:{port}");

    let state = request(port, "GET", "/api/state", &host, Some(token), None)
        .await
        .json();
    // The owner's UI lists the folder's MCP servers, never their secret values.
    assert_eq!(state["me"]["shares"][1]["mcp"][0]["name"], "browser");
    assert!(!state.to_string().contains("mcp-secret-6620"));
    let marked: Vec<(String, Value)> = state["me"]["shares"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| (s["name"].as_str().unwrap().to_owned(), s["preview"].clone()))
        .collect();
    assert_eq!(
        marked,
        [
            ("data".to_owned(), Value::Null),
            ("app".to_owned(), Value::from("prototype"))
        ],
        "folders holding a prototype are marked for preview"
    );

    let share = r#"{"share":"app"}"#;
    let anonymous = request(port, "POST", "/api/previews", &host, None, Some(share)).await;
    assert_eq!(anonymous.status, 401, "creating a preview needs the token");
    let missing = request(
        port,
        "POST",
        "/api/previews",
        &host,
        Some(token),
        Some(r#"{"share":"nope"}"#),
    )
    .await;
    assert_eq!(missing.status, 400);
    let made = request(
        port,
        "POST",
        "/api/previews",
        &host,
        Some(token),
        Some(share),
    )
    .await;
    assert_eq!(made.status, 200, "{}", made.body);
    let url = made.json()["url"].as_str().unwrap().to_owned();
    let cap = url
        .trim_start_matches("/preview/")
        .trim_end_matches('/')
        .to_owned();
    assert_eq!(cap.len(), 32);

    // The capability is the authorization; the content runs sandboxed, offline.
    let page = request(port, "GET", &url, &host, None, None).await;
    assert_eq!(page.status, 200);
    assert!(page.body.contains("data-story=\"US-1\""));
    for header in [
        "content-security-policy: sandbox allow-scripts",
        "connect-src 'none'",
        "form-action 'none'",
        "x-content-type-options: nosniff",
        "content-type: text/html",
    ] {
        assert!(
            page.headers.contains(header),
            "{header} in {}",
            page.headers
        );
    }
    assert!(!page.headers.contains("allow-same-origin"));
    let bare = request(port, "GET", &format!("/preview/{cap}"), &host, None, None).await;
    assert_eq!(bare.status, 308);
    assert!(bare.headers.contains(&format!("location: /preview/{cap}/")));
    let foreign = request(port, "GET", &url, "evil.example:80", None, None).await;
    assert_eq!(foreign.status, 403, "the Host check covers previews");
    let other = request(
        port,
        "GET",
        &format!("/preview/{}/", "0".repeat(32)),
        &host,
        None,
        None,
    )
    .await;
    assert_eq!(other.status, 404);
    let escape = request(
        port,
        "GET",
        &format!("{url}..%2F..%2Fspec%2FSPEC.md"),
        &host,
        None,
        None,
    )
    .await;
    assert_eq!(escape.status, 404, "only prototype files are served");

    // With a design package, the folder previews as a review pack: decks as slide
    // shows, design documents as pages, the prototype, all in the same sandbox.
    proofkin::design::init(app.path(), "Wishlist", false).unwrap();
    let made = request(
        port,
        "POST",
        "/api/previews",
        &host,
        Some(token),
        Some(share),
    )
    .await;
    assert_eq!(made.status, 200, "{}", made.body);
    let pack = made.json()["url"].as_str().unwrap().to_owned();
    let index = request(port, "GET", &pack, &host, None, None).await;
    for link in [
        "href=\"prototype/index.html\"",
        "href=\"review/leadership.html\">Leadership review (deck)",
        "href=\"design/threat-model.html\"",
        "href=\"design/adr/ADR-0001-cursor-paging.html\"",
    ] {
        assert!(index.body.contains(link), "{link} in {}", index.body);
    }
    let deck = request(
        port,
        "GET",
        &format!("{pack}review/leadership.html"),
        &host,
        None,
        None,
    )
    .await;
    assert_eq!(deck.status, 200);
    assert!(
        deck.headers
            .contains("content-security-policy: sandbox allow-scripts")
    );
    assert_eq!(deck.body.matches("<section class=\"slide").count(), 6);
    let state = request(port, "GET", "/api/state", &host, Some(token), None)
        .await
        .json();
    assert_eq!(state["me"]["shares"][1]["preview"], "review pack");

    // An approval that comes with a prototype can be previewed before answering.
    let files = [(
        "prototype/index.html".to_owned(),
        "<h1>Proto</h1>".to_owned(),
    )]
    .into();
    let asker = agent.clone();
    let waiting = tokio::spawn(async move {
        asker
            .request_approval_with(None, "Plan step product-ok", "Accept the prototype", files)
            .await
    });
    let approval = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let state = request(port, "GET", "/api/state", &host, Some(token), None)
                .await
                .json();
            if let Some(a) = state["approvals"].as_array().and_then(|a| a.first()) {
                return a.clone();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(approval["preview"], "prototype");
    let body = format!(r#"{{"approval":{}}}"#, approval["id"]);
    let made = request(
        port,
        "POST",
        "/api/previews",
        &host,
        Some(token),
        Some(&body),
    )
    .await;
    assert_eq!(made.status, 200, "{}", made.body);
    let page = request(
        port,
        "GET",
        made.json()["url"].as_str().unwrap(),
        &host,
        None,
        None,
    )
    .await;
    assert_eq!(page.body, "<h1>Proto</h1>");
    // This request can only be approved or rejected; asking for changes is refused.
    let answer = |id: &Value, body: &'static str| {
        let (host, token, path) = (
            host.clone(),
            token.to_owned(),
            format!("/api/approvals/{id}"),
        );
        async move { request(port, "POST", &path, &host, Some(&token), Some(body)).await }
    };
    let refused = answer(
        &approval["id"],
        r#"{"approved":false,"changes":true,"note":"Bigger"}"#,
    )
    .await;
    assert_eq!(refused.status, 400, "{}", refused.body);
    assert!(refused.body.contains("can only be approved or rejected"));
    assert_eq!(
        answer(&approval["id"], r#"{"approved":true}"#).await.status,
        200
    );
    assert!(waiting.await.unwrap().unwrap().ok);

    // One that offers it: asking for changes needs a note, and is signed as not approved.
    let asker = agent.clone();
    let waiting = tokio::spawn(async move {
        let ask = proofkin::agent::ApprovalAsk {
            title: "Plan step ok".into(),
            text: "Ship it?".into(),
            changes: true,
            ..Default::default()
        };
        let id = asker.new_task_id();
        asker.request_approval_as(None, id, &ask).await
    });
    let approval = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let state = request(port, "GET", "/api/state", &host, Some(token), None)
                .await
                .json();
            if let Some(a) = state["approvals"].as_array().and_then(|a| a.first()) {
                return a.clone();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(approval["changes"], true);
    let bare = answer(&approval["id"], r#"{"approved":false,"changes":true}"#).await;
    assert_eq!(bare.status, 400);
    assert!(bare.body.contains("say what to change"), "{}", bare.body);
    let asked = answer(
        &approval["id"],
        r#"{"approved":false,"changes":true,"note":"Make it bigger"}"#,
    )
    .await;
    assert_eq!(asked.status, 200, "{}", asked.body);
    let answered = waiting.await.unwrap().unwrap();
    assert!(!answered.ok && answered.verified);
    assert!(
        answered.output.ends_with(" Make it bigger")
            && answered.output.starts_with(proofkin::agent::CHANGES_ASKED),
        "{}",
        answered.output
    );
    agent.shutdown().await;
}

/// Copies the fixture plugin (with the echo MCP server) into `dir`.
fn fixture_plugin(dir: &std::path::Path) {
    fn copy(from: &std::path::Path, to: &std::path::Path) {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap().flatten() {
            let target = to.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy(&entry.path(), &target);
            } else {
                std::fs::copy(entry.path(), target).unwrap();
            }
        }
    }
    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    copy(&fixtures.join("plugin"), dir);
    std::fs::create_dir_all(dir.join("server")).unwrap();
    std::fs::copy(fixtures.join("echo_mcp.py"), dir.join("server/echo_mcp.py")).unwrap();
}

#[tokio::test]
async fn ui_imports_plugins_and_configures_mcp_servers_and_skills() {
    let home = tempfile::tempdir().unwrap();
    let app = tempfile::tempdir().unwrap();
    let sources = tempfile::tempdir().unwrap();
    let profile = Profile::open_in(home.path(), "owner").unwrap();
    let mut shares = profile.shares().unwrap();
    shares.add("app", app.path(), Access::ReadOnly).unwrap();
    shares.save(&profile.shares_path()).unwrap();
    let mut config = AgentConfig::new("owner", SecretKey::generate());
    config.network = Network::Local;
    config.bind = Some("127.0.0.1:0".parse().unwrap());
    config.shares = shares;
    let (agent, events) = Agent::bind(config).await.unwrap();
    let info = UiInfo {
        profile: profile.clone(),
        network: Network::Local,
        serving: true,
        access: Access::ReadOnly,
        workspace: home.path().to_path_buf(),
        providers: Vec::new(),
        accounts: Vec::new(),
        workers: None,
    };
    let server = ui::start(
        agent.clone(),
        events,
        info,
        None,
        "127.0.0.1:0".parse().unwrap(),
    )
    .await
    .unwrap();
    let rest = server.url.strip_prefix("http://127.0.0.1:").unwrap();
    let (port, token) = rest.split_once("/#t=").unwrap();
    let port: u16 = port.parse().unwrap();
    let host = format!("127.0.0.1:{port}");
    let call = |method: &'static str, path: String, body: Value| {
        let (host, token) = (host.clone(), token.to_owned());
        async move {
            let body = (!body.is_null()).then(|| body.to_string());
            request(port, method, &path, &host, Some(&token), body.as_deref()).await
        }
    };
    let offered_to_peers = || {
        agent.shares().visible(&["app".into()], Access::ReadOnly)[0]
            .mcp
            .clone()
    };

    // A marketplace lists its plugins; picking one fetches it for review. Nothing
    // is kept until it is imported, and nothing in it runs.
    let market = sources.path().join("team-market");
    fixture_plugin(&market.join("plugins/echo"));
    std::fs::create_dir_all(market.join(".claude-plugin")).unwrap();
    std::fs::write(
        market.join(".claude-plugin/marketplace.json"),
        r#"{"name": "team", "plugins": [{"name": "echo-tools", "description": "Echo", "source": "./plugins/echo"},
            {"name": "elsewhere", "source": {"source": "npm", "package": "x"}}]}"#,
    )
    .unwrap();
    let listing = call(
        "POST",
        "/api/plugin-preview".into(),
        serde_json::json!({ "source": market }),
    )
    .await;
    assert_eq!(listing.status, 200, "{}", listing.body);
    let listing = listing.json();
    assert_eq!(
        (listing["kind"].as_str(), listing["name"].as_str()),
        (Some("marketplace"), Some("team"))
    );
    assert_eq!(listing["plugins"][1]["source"]["kind"], "unsupported");
    let review = call(
        "POST",
        "/api/plugin-preview".into(),
        serde_json::json!({ "token": listing["token"], "plugin": "echo-tools" }),
    )
    .await;
    assert_eq!(review.status, 200, "{}", review.body);
    let review = review.json();
    assert_eq!(
        (review["kind"].as_str(), review["market"].as_str()),
        (Some("plugin"), Some("team"))
    );
    let plugin = &review["plugin"];
    assert_eq!(plugin["name"], "echo-tools");
    assert_eq!(plugin["skills"][0]["name"], "release-notes");
    let inputs = &plugin["mcp"][0]["inputs"];
    assert!(
        inputs
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["name"] == "ECHO_WORD" && i["secret"] == true),
        "{inputs}"
    );
    assert!(
        plugin["notes"][0]
            .as_str()
            .unwrap()
            .contains("hooks (they never run here)")
    );
    let listed = call("GET", "/api/plugins".into(), Value::Null).await.json();
    assert_eq!(
        listed["plugins"],
        serde_json::json!([]),
        "nothing kept before import"
    );

    let installed = call(
        "POST",
        "/api/plugins".into(),
        serde_json::json!({ "token": review["token"] }),
    )
    .await;
    assert_eq!(installed.status, 200, "{}", installed.body);
    assert_eq!(
        installed.json()["plugin"]["skills"][0]["use"],
        "release-notes"
    );
    let again = call(
        "POST",
        "/api/plugins".into(),
        serde_json::json!({ "token": review["token"] }),
    )
    .await;
    assert_eq!(again.status, 400, "a review is imported once");
    assert!(
        walkdir(home.path())
            .iter()
            .all(|p| !p.ends_with("HOOK-RAN.txt")),
        "the plugin's hook never ran"
    );
    let skills = call("GET", "/api/skills".into(), Value::Null).await.json();
    assert!(
        skills["skills"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["name"] == "release-notes" && s["source"] == "plugin"),
        "{skills}"
    );

    // The plugin's agent comes with it: its own skill and the MCP server it uses.
    assert_eq!(plugin["agents"][0]["name"], "reviewer");
    let reviewer = skills["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["name"] == "reviewer")
        .unwrap_or_else(|| panic!("{skills}"))
        .clone();
    assert_eq!(
        (&reviewer["source"], &reviewer["plugin"]),
        (&Value::from("plugin"), &Value::from("echo-tools"))
    );
    assert_eq!(
        (reviewer["skills"].clone(), reviewer["mcp"].clone()),
        (
            serde_json::json!(["echo-tools.release-notes"]),
            serde_json::json!(["echo"])
        )
    );
    // The profile's own agents: added, shown, used in plans and removed.
    let added = call(
        "POST",
        "/api/agents".into(),
        serde_json::json!({ "name": "", "text": "---\nname: editor\nskills: [release-notes]\n---\nEdit carefully." }),
    )
    .await;
    assert_eq!(added.status, 200, "{}", added.body);
    let shown = call("GET", "/api/agents/editor".into(), Value::Null)
        .await
        .json();
    assert_eq!(shown["instructions"], "Edit carefully.");
    for (step, unknown) in [("agent=editor", false), ("agent=ghost", true)] {
        let checked = call(
            "POST",
            "/api/plans/check".into(),
            serde_json::json!({ "text": format!("a @owner:app {step} Edit it") }),
        )
        .await
        .json();
        assert_eq!(
            checked["problems"]
                .to_string()
                .contains("there is no agent"),
            unknown,
            "{step}: {checked}"
        );
    }
    let gone = call("DELETE", "/api/agents/editor".into(), Value::Null).await;
    assert_eq!(gone.status, 200, "{}", gone.body);
    let plugin_own = call("DELETE", "/api/agents/reviewer".into(), Value::Null).await;
    assert_eq!(
        plugin_own.status, 400,
        "a plugin's agent goes with its plugin"
    );

    // Offering a plugin's MCP server in a folder, with the values it needs; the
    // values stay in the profile, peers learn the name and the description.
    let offer = |values: Value| {
        serde_json::json!({
            "share": "app", "name": "echo", "plugin": "echo-tools", "access": "read-only",
            "description": "Says the word", "values": values,
        })
    };
    let missing = call("POST", "/api/mcp".into(), offer(serde_json::json!({}))).await;
    assert_eq!(missing.status, 400);
    assert!(
        missing.body.contains("needs a value for ECHO_WORD"),
        "{}",
        missing.body
    );
    let made = call(
        "POST",
        "/api/mcp".into(),
        offer(serde_json::json!({ "ECHO_WORD": "PLUM-7781", "PYTHON": "python" })),
    )
    .await;
    assert_eq!(made.status, 200, "{}", made.body);
    assert!(!made.body.contains("PLUM-7781"));
    assert_eq!(
        made.json()["server"]["plugin"]["values"],
        serde_json::json!(["ECHO_WORD", "PYTHON"])
    );
    let duplicate = call(
        "POST",
        "/api/mcp".into(),
        offer(serde_json::json!({ "ECHO_WORD": "x" })),
    )
    .await;
    assert!(
        duplicate.body.contains("already offers"),
        "{}",
        duplicate.body
    );
    assert_eq!(offered_to_peers(), ["echo"]);
    assert_eq!(
        agent.shares().visible(&["app".into()], Access::ReadOnly)[0].mcp_about["echo"],
        "Says the word"
    );
    let state = call("GET", "/api/mcp".into(), Value::Null).await;
    assert!(
        !state.body.contains("PLUM-7781"),
        "values are never sent to the page"
    );
    let state = state.json();
    assert_eq!(state["shares"][0]["mcp"][0]["name"], "echo");
    assert_eq!(state["templates"][0]["server"], "echo");

    // Pausing stops offering it; new values make it again from the template.
    let paused = call(
        "PATCH",
        "/api/mcp/app/echo".into(),
        serde_json::json!({ "paused": true }),
    )
    .await;
    assert_eq!(paused.status, 200, "{}", paused.body);
    assert!(offered_to_peers().is_empty());
    let resumed = call(
        "PATCH",
        "/api/mcp/app/echo".into(),
        serde_json::json!({ "paused": false, "values": { "ECHO_WORD": "NEW-WORD-1", "PYTHON": "" } }),
    )
    .await;
    assert_eq!(resumed.status, 200, "{}", resumed.body);
    assert_eq!(offered_to_peers(), ["echo"]);
    let saved = std::fs::read_to_string(profile.shares_path()).unwrap();
    assert!(
        saved.contains("NEW-WORD-1") && !saved.contains("PLUM-7781"),
        "{saved}"
    );
    assert!(
        saved.contains("\"PYTHON\": \"python\""),
        "a blank value keeps the current one"
    );

    // Turning the plugin off withdraws its skills and servers until it is on again.
    let off = call(
        "PATCH",
        "/api/plugins/echo-tools".into(),
        serde_json::json!({ "enabled": false }),
    )
    .await;
    assert_eq!(off.status, 200, "{}", off.body);
    assert!(offered_to_peers().is_empty());
    let skills = call("GET", "/api/skills".into(), Value::Null).await.json();
    assert!(!skills.to_string().contains("release-notes"));
    call(
        "PATCH",
        "/api/plugins/echo-tools".into(),
        serde_json::json!({ "enabled": true }),
    )
    .await;
    assert_eq!(offered_to_peers(), ["echo"]);

    // An MCP configuration (VS Code format) is reviewed, then a server offered.
    let config = r#"{"inputs": [{"id": "docs-token", "description": "Docs token", "password": true}],
        "servers": {"docs": {"type": "http", "url": "https://docs.example.com/mcp",
        "headers": {"Authorization": "Bearer ${input:docs-token}"}}}}"#;
    let parsed = call(
        "POST",
        "/api/mcp/parse".into(),
        serde_json::json!({ "json": config }),
    )
    .await
    .json();
    assert_eq!(parsed["servers"][0]["name"], "docs");
    assert_eq!(parsed["servers"][0]["inputs"][0]["secret"], true);
    let docs = call(
        "POST",
        "/api/mcp".into(),
        serde_json::json!({ "share": "app", "name": "docs", "config": config, "server": "docs",
            "values": { "docs-token": "tok-5531" } }),
    )
    .await;
    assert_eq!(docs.status, 200, "{}", docs.body);
    assert_eq!(
        docs.json()["server"]["describe"],
        "https://docs.example.com/mcp [Authorization=***]"
    );

    // By hand: a command line, with the tools tasks may call.
    let web = call(
        "POST",
        "/api/mcp".into(),
        serde_json::json!({ "share": "app", "name": "web", "command_line": "npx -y \"@playwright/mcp\" --headless",
            "access": "full", "tools": ["browser_snapshot", " "] }),
    )
    .await;
    assert_eq!(web.status, 200, "{}", web.body);
    let web = web.json();
    assert_eq!(
        web["server"]["describe"],
        "npx -y @playwright/mcp --headless"
    );
    assert_eq!(
        web["server"]["tools"],
        serde_json::json!(["browser_snapshot"])
    );
    let data = call(
        "POST",
        "/api/mcp".into(),
        serde_json::json!({ "share": "data", "name": "web", "command_line": "npx x" }),
    )
    .await;
    assert_eq!(data.status, 400, "the data folder offers no servers");
    assert_eq!(
        call("DELETE", "/api/mcp/app/web".into(), Value::Null)
            .await
            .status,
        200
    );
    assert_eq!(
        call("DELETE", "/api/mcp/app/web".into(), Value::Null)
            .await
            .status,
        400
    );
    let all = call("GET", "/api/mcp".into(), Value::Null).await;
    assert!(!all.body.contains("tok-5531"));

    // The profile's own skills: named by their front matter, removable; built-ins aren't.
    let added = call(
        "POST",
        "/api/skills".into(),
        serde_json::json!({ "text": "---\nname: house-style\ndescription: How we write\n---\nWrite plainly." }),
    )
    .await;
    assert_eq!(added.json()["name"], "house-style");
    let skill = call("GET", "/api/skills/house-style".into(), Value::Null)
        .await
        .json();
    assert!(skill["text"].as_str().unwrap().contains("Write plainly."));
    let nameless = call(
        "POST",
        "/api/skills".into(),
        serde_json::json!({ "text": "No name." }),
    )
    .await;
    assert_eq!(nameless.status, 400);
    assert_eq!(
        call("DELETE", "/api/skills/house-style".into(), Value::Null)
            .await
            .status,
        200
    );
    assert_eq!(
        call("DELETE", "/api/skills/house-style".into(), Value::Null)
            .await
            .status,
        404
    );
    let built_in = call("DELETE", "/api/skills/security-review".into(), Value::Null).await;
    assert!(built_in.body.contains("built in"), "{}", built_in.body);

    // Removing the plugin removes the servers it offered, and only those.
    let removed = call("DELETE", "/api/plugins/echo-tools".into(), Value::Null).await;
    assert_eq!(removed.status, 200, "{}", removed.body);
    assert!(
        removed.body.contains("MCP server echo removed from 'app'"),
        "{}",
        removed.body
    );
    let left: Vec<String> = agent
        .shares()
        .get("app")
        .unwrap()
        .mcp
        .keys()
        .cloned()
        .collect();
    assert_eq!(left, ["docs"]);
    assert!(
        offered_to_peers().is_empty(),
        "docs needs full access; this folder allows read-only"
    );
    agent.shutdown().await;
}

/// The page follows docs/ux-guidelines.md where a machine can tell: its theme color,
/// in-page confirmations (never window.confirm), no HTML built from strings.
#[tokio::test]
async fn the_page_follows_the_ux_guidelines() {
    let home = tempfile::tempdir().unwrap();
    let mut config = AgentConfig::new("solo", SecretKey::generate());
    config.network = Network::Local;
    config.bind = Some("127.0.0.1:0".parse().unwrap());
    let (agent, events) = Agent::bind(config).await.unwrap();
    let info = UiInfo {
        profile: Profile::open_in(home.path(), "solo").unwrap(),
        network: Network::Local,
        serving: false,
        access: Access::ReadOnly,
        workspace: home.path().to_path_buf(),
        providers: Vec::new(),
        accounts: Vec::new(),
        workers: None,
    };
    let server = ui::start(
        agent.clone(),
        events,
        info,
        None,
        "127.0.0.1:0".parse().unwrap(),
    )
    .await
    .unwrap();
    let rest = server.url.strip_prefix("http://127.0.0.1:").unwrap();
    let port: u16 = rest.split_once("/#t=").unwrap().0.parse().unwrap();
    let host = format!("127.0.0.1:{port}");
    let page = request(port, "GET", "/", &host, None, None).await.body;
    assert!(page.contains(r#"<meta name="theme-color""#));
    assert!(page.contains(r#"<script src="/tools.js" defer>"#));
    assert!(page.contains(r#"<script src="/markdown.js" defer>"#));
    assert!(page.contains(r#"<script src="/models.js" defer>"#));
    assert!(page.contains(r#"<script src="/branches.js" defer>"#));
    assert!(page.contains(r#"<script src="/m365.js" defer>"#));
    assert!(page.contains(r#"<script src="/plan-text.js" defer>"#));
    assert!(page.contains(r#"<script src="/plan-builder.js" defer>"#));
    assert!(page.contains(r#"<script src="/workers.js" defer>"#));
    assert!(page.contains(r#"<script src="/factory.js" defer>"#));
    assert!(page.contains(r#"<script src="/factory-share.js" defer>"#));
    assert!(page.contains(r#"<script src="/connect.js" defer>"#));
    for id in [
        "confirm-dialog",
        "plan-editor",
        "tools-dialog",
        "when-repeat",
        "when-preview",
        "playbook-pick",
        "playbook-choices",
        "plan-view",
        "plan-builder",
        "worker-list",
        "worker-dialog",
        "factory-list",
        "factory-new-dialog",
        "factory-agent-dialog",
        "factory-edit-mode",
        "fpanel-releases",
        "fpanel-deployments",
        "fpanel-metrics",
        "fpanel-drift",
        "fpanel-improve",
        "fpanel-share",
        "factory-catalog-btn",
        "factory-catalog-dialog",
        "connect",
        "connect-toggle",
        "check-options",
        "attempts",
        "run-download",
        "pr-proof",
        "clear-tasks-btn",
        "add-monitor-btn",
        "monitor-dialog",
        "team-dialog",
        "models-dialog",
        "branch-dialog",
        "pr-changes",
    ] {
        assert!(page.contains(&format!(r#"id="{id}""#)), "{id}");
    }
    // Playbooks need explaining, so they're picked from cards, not a native dropdown.
    assert!(!page.contains(r#"<select id="playbook""#));
    let confirm = regex::Regex::new(r"\bconfirm\(").unwrap();
    let html =
        regex::Regex::new(r"\.(inner|outer)HTML\b|insertAdjacentHTML|document\.write").unwrap();
    // Every dialog closes with Escape (and a stray backdrop click never loses input).
    let app = request(port, "GET", "/app.js", &host, None, None)
        .await
        .body;
    let guarded = regex::Regex::new(r"for \(const id of \[([^\]]+)\]\) guardDialog")
        .unwrap()
        .captures(&app)
        .expect("app.js guards its dialogs")[1]
        .to_owned();
    let dialogs = regex::Regex::new(r#"<dialog[^>]*\bid="([^"]+)""#).unwrap();
    assert!(dialogs.captures_iter(&page).count() >= 10);
    for d in dialogs.captures_iter(&page) {
        assert!(
            guarded.contains(&format!("\"#{}\"", &d[1])),
            "{} isn't guarded",
            &d[1]
        );
    }
    // One name, one meaning: the page's scripts share one scope, where a second
    // declaration of a function silently replaces the first, and a const, let or class
    // of a name declared before stops the later script (a SyntaxError).
    let declared = regex::Regex::new(
        r"(?m)^(?:(?:async )?function\*? (\w+)\(|(?:const|let|var|class) (\w+)\b)",
    )
    .unwrap();
    let mut declarations = std::collections::HashMap::new();
    for script in [
        "/app.js",
        "/markdown.js",
        "/models.js",
        "/branches.js",
        "/m365.js",
        "/tools.js",
        "/plan-text.js",
        "/plan-builder.js",
        "/workers.js",
        "/factory.js",
        "/factory-share.js",
        "/connect.js",
    ] {
        let js = request(port, "GET", script, &host, None, None).await;
        assert_eq!(js.status, 200, "{script}");
        assert!(
            js.headers.contains("content-type: text/javascript"),
            "{script}"
        );
        assert!(
            !confirm.is_match(&js.body),
            "{script} asks with window.confirm"
        );
        assert!(
            !html.is_match(&js.body),
            "{script} builds HTML from strings"
        );
        for name in declared
            .captures_iter(&js.body)
            .filter_map(|c| c.get(1).or_else(|| c.get(2)).map(|m| m.as_str().to_owned()))
        {
            if let Some(first) = declarations.insert(name.clone(), script) {
                panic!("{name} is declared in {first} and again in {script}");
            }
        }
    }
    let guide = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/ux-guidelines.md"),
    )
    .unwrap();
    assert!(guide.contains("## Definition of done"));
    // Below 1180 px the conversation keeps the window's height and stays in view, so
    // its composer never scrolls off; empty states read as sentences.
    let css = request(port, "GET", "/app.css", &host, None, None)
        .await
        .body;
    let narrow = css
        .split("@media (max-width: 1180px)")
        .nth(1)
        .and_then(|rest| rest.split("@media").next())
        .unwrap();
    assert!(
        narrow.contains(".main { position: sticky;") && narrow.contains("height: calc(100vh"),
        "{narrow}"
    );
    assert!(css.contains(".list li.empty { display: block; }"));
    // Times show their day unless it's today (lists span days); counts agree.
    let stamp = app
        .split("function stamp(ms) {")
        .nth(1)
        .and_then(|rest| rest.split("\n}\n").next())
        .expect("app.js formats times with stamp()");
    assert_eq!(
        app.matches("clock(").count(),
        1 + stamp.matches("clock(").count(),
        "only stamp() shows a time on its own (it adds the day when it isn't today)"
    );
    assert!(!app.contains("length} steps`"), "\"1 steps\"");
    // A new agent shows its first steps in place of the empty conversation.
    assert!(app.contains("function getStartedNode()"));
    agent.shutdown().await;
}

/// The plan editor (ui/plan-text.js) keeps plans valid: its palette puts each part
/// where the plan syntax expects it, the plan check's fixes change the text as they
/// say, and the builder's model writes back the plan it read, each edit changing only
/// its own options (among them a step's Microsoft 365 sources, `workiq=`, and a
/// `@workiq` step's agent). The cases (tests/plan_text.test.cjs) run in node, which
/// loads the script as the page does.
#[test]
fn the_plan_editor_keeps_plans_valid() {
    let cases = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/plan_text.test.cjs");
    let out = std::process::Command::new("node")
        .arg(&cases)
        .output()
        .expect("node must be installed to run the plan editor's tests");
    let (stdout, stderr) = (
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(out.status.success(), "{stdout}{stderr}");
    assert!(stdout.starts_with("ok: "), "{stdout}");
}

/// Task and step text shows as Markdown: backticks become code, lists and blank-line
/// paragraphs become list items and paragraphs, and markup in the text stays text. The
/// cases (tests/markdown.test.cjs) run in node, which loads ui/markdown.js as the page does.
#[test]
fn task_text_shows_as_markdown_and_markup_stays_text() {
    let cases = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/markdown.test.cjs");
    let out = std::process::Command::new("node")
        .arg(&cases)
        .output()
        .expect("node must be installed to run the Markdown tests");
    let (stdout, stderr) = (
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(out.status.success(), "{stdout}{stderr}");
    assert!(stdout.starts_with("ok: "), "{stdout}");
}

/// The Run task form and the plan builder offer what a worker lets peers pick: the
/// models it allows, else all its account offers (by name only when it lists none), what
/// several allow together, and the reasoning levels it allows that the model supports,
/// with its defaults as the model takes them. The squad dialog and card say what a peer
/// takes as a squad member, or why it takes no steps (it only delegates, or shares no
/// folder). The cases (tests/models.test.cjs) run in node, which loads ui/models.js as
/// the page does.
#[test]
fn task_forms_offer_the_models_and_levels_the_worker_allows() {
    let cases = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/models.test.cjs");
    let out = std::process::Command::new("node")
        .arg(&cases)
        .output()
        .expect("node must be installed to run the model list tests");
    let (stdout, stderr) = (
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(out.status.success(), "{stdout}{stderr}");
    assert!(stdout.starts_with("ok: "), "{stdout}");
}

/// The GitHub card says where each task branch's work is and offers what fits it: only
/// Delete branch when deleting it loses nothing (its work on the repository's branch, or a
/// pull request merged it as it is), so nothing is left to review; Review changes and
/// Delete branch once its pull request or its issues are closed, which don't make it safe:
/// the confirmation says what would be lost; and Open PR while there is work to merge.
/// Review changes reads the diff into its files' lines by the hunks' counts. The cases
/// (tests/branches.test.cjs) run in node, which loads ui/branches.js as the page does.
#[test]
fn task_branches_say_whether_deleting_them_loses_work() {
    let cases = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/branches.test.cjs");
    let out = std::process::Command::new("node")
        .arg(&cases)
        .output()
        .expect("node must be installed to run the task branch tests");
    let (stdout, stderr) = (
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(out.status.success(), "{stdout}{stderr}");
    assert!(stdout.starts_with("ok: "), "{stdout}");
}

/// The page words Microsoft 365 content plainly: a label Work IQ gave no information for
/// reads "no label information", never the internal `unknown` (in a run's counts and sources
/// and an import's result); a `@workiq` step's task reads as Microsoft 365 Copilot's (or its
/// agent's) through Work IQ, not a provider's; a topic imported from Microsoft 365 shows its
/// labels and who it may go to in the Knowledge list; and a route's error shows as a
/// sentence. The cases (tests/m365.test.cjs) run in node, which loads ui/m365.js as the page
/// does.
#[test]
fn the_page_words_microsoft_365_labels_and_work_iq_steps_plainly() {
    let cases = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/m365.test.cjs");
    let out = std::process::Command::new("node")
        .arg(&cases)
        .output()
        .expect("node must be installed to run the Microsoft 365 wording tests");
    let (stdout, stderr) = (
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(out.status.success(), "{stdout}{stderr}");
    assert!(stdout.starts_with("ok: "), "{stdout}");
}

/// The agent factory's Deployments tab says why it can't deploy before anything is sent:
/// with no folder to offer and a release that names none, it says to share one and Deploy is
/// disabled; with folders offered, none ticked and a release that names none, it refuses
/// before it sends; its hint says where a deployment works (the folders ticked, else those
/// its release names); and the route's refusal shows in its words. Evaluate shows how the
/// last evaluation ended until another starts, with the way to its report, and approving a
/// release there redraws Deployments at once. The cases (tests/factory.test.cjs) run in
/// node, which loads ui/factory.js as the page does, on a fake page.
#[test]
fn the_deploy_form_says_why_it_cant_deploy_before_it_sends() {
    let cases = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/factory.test.cjs");
    let out = std::process::Command::new("node")
        .arg(&cases)
        .output()
        .expect("node must be installed to run the agent factory's page tests");
    let (stdout, stderr) = (
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(out.status.success(), "{stdout}{stderr}");
    assert!(stdout.starts_with("ok: "), "{stdout}");
}

fn walkdir(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(walkdir(&path));
        } else {
            found.push(path);
        }
    }
    found
}

/// The owner's model, as the Plan editor's Describe view has it draft a plan: it keeps each
/// prompt, and answers with prose around a plan whose first one has a step on a peer that
/// isn't connected.
#[derive(Default)]
struct Planner {
    prompts: std::sync::Mutex<Vec<String>>,
}

#[async_trait]
impl Executor for Planner {
    fn providers(&self) -> Vec<Provider> {
        vec![Provider::Copilot]
    }

    async fn execute(&self, task: &LlmTask, ctx: &TaskContext) -> anyhow::Result<Ran> {
        assert_eq!(ctx.access, Access::ReadOnly, "a draft only reads");
        let mut prompts = self.prompts.lock().unwrap();
        prompts.push(task.prompt.clone());
        let plan = if prompts.len() == 1 {
            "fix @ghost:app Fix the bug"
        } else {
            "ok approve @me Ship the fix?"
        };
        Ok(Ran::new(
            format!("Here is the plan.\n\n```plan\n{plan}\n```\n\nReview it first."),
            task.model.clone(),
        )
        .with_effort(task.effort.clone()))
    }
}

async fn plan_draft_ui(home: &std::path::Path) -> (Profile, Agent, Arc<Planner>, u16, String) {
    let dir = home.join("owner");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("profile.json"),
        r#"{
            "copilot": {"source": "token", "host": "github.com"},
            "models": {"copilot": {
                "allowed": ["claude-opus-5.5", "planner-1"],
                "effort": "medium", "efforts": ["low", "medium"]
            }}
        }"#,
    )
    .unwrap();
    let profile = Profile::open_in(home, "owner").unwrap();
    let mut config = AgentConfig::new("owner", SecretKey::generate());
    config.network = Network::Local;
    config.bind = Some("127.0.0.1:0".parse().unwrap());
    let (agent, events) = Agent::bind(config).await.unwrap();
    let info = UiInfo {
        profile: profile.clone(),
        network: Network::Local,
        serving: false,
        access: Access::ReadOnly,
        workspace: home.to_path_buf(),
        providers: Vec::new(),
        accounts: Vec::new(),
        workers: None,
    };
    let planner = Arc::new(Planner::default());
    let server = ui::start(
        agent.clone(),
        events,
        info,
        Some(planner.clone()),
        "127.0.0.1:0".parse().unwrap(),
    )
    .await
    .unwrap();
    let rest = server.url.strip_prefix("http://127.0.0.1:").unwrap();
    let (port, token) = rest.split_once("/#t=").unwrap();
    (profile, agent, planner, port.parse().unwrap(), token.into())
}

/// `POST /api/plans/draft` has the chosen model write a plan: the model is told what
/// the check found wrong and writes it again. The answer has the plan with its problems,
/// provider, model, effort and tries, and the draft is recorded in the activity and audit
/// (with the goal's hash, not its text). Drafting starts no task and no run; a request
/// without a goal, or for a provider the profile isn't signed in to, is refused.
#[tokio::test]
async fn the_owner_drafts_a_plan_with_the_profiles_model_through_the_api() {
    let home = tempfile::tempdir().unwrap();
    let (profile, agent, planner, port, token) = plan_draft_ui(home.path()).await;
    let host = format!("127.0.0.1:{port}");
    let call = |method: &'static str, body: Value| {
        let (host, token) = (host.clone(), token.to_owned());
        async move {
            let body = (!body.is_null()).then(|| body.to_string());
            request(
                port,
                method,
                "/api/plans/draft",
                &host,
                Some(&token),
                body.as_deref(),
            )
            .await
        }
    };

    let options = call("GET", Value::Null).await;
    assert_eq!(options.status, 200, "{}", options.body);
    assert_eq!(options.json()["providers"][0]["provider"], "copilot");
    assert_eq!(options.json()["max_goal"], 4000);

    let goal = "Fix the login bug, then let me sign it off";
    let drafted = call(
        "POST",
        serde_json::json!({ "goal": goal, "model": "planner-1", "effort": "low" }),
    )
    .await;
    assert_eq!(drafted.status, 200, "{}", drafted.body);
    let drafted = drafted.json();
    assert_eq!(
        drafted["plan"],
        format!("// Drafted by copilot (planner-1): {goal}\nok approve @me Ship the fix?\n")
    );
    assert!(
        drafted["problems"]
            .as_array()
            .unwrap()
            .iter()
            .all(|p| p["error"] == false),
        "{drafted}"
    );
    assert_eq!(drafted["provider"], "copilot");
    assert_eq!(drafted["model"], "planner-1");
    assert_eq!(drafted["effort"], "low");
    assert_eq!(drafted["tries"], 2);
    assert!(drafted.get("goal_sha256").is_none(), "{drafted}");
    {
        let prompts = planner.prompts.lock().unwrap();
        assert_eq!(prompts.len(), 2);
        assert!(prompts[0].contains(goal), "{}", prompts[0]);
        assert!(
            prompts[1].contains("ghost is not connected"),
            "{}",
            prompts[1]
        );
    }

    let state = request(port, "GET", "/api/state", &host, Some(&token), None)
        .await
        .json();
    assert_eq!(
        state["tasks"],
        serde_json::json!([]),
        "drafting starts no task"
    );
    assert!(
        state["feed"].as_array().unwrap().iter().any(|f| f["text"]
            .as_str()
            .unwrap_or_default()
            .starts_with(concat!(
                "Drafted a plan with copilot's model (planner-1) at low reasoning ",
                "in 2 tries for owner: 0 errors"
            ))),
        "{}",
        state["feed"]
    );
    assert!(
        !profile.runs_dir().exists()
            || std::fs::read_dir(profile.runs_dir())
                .unwrap()
                .next()
                .is_none(),
        "drafting starts no run"
    );
    let audit = AuditLog::read(&profile.audit_path(), 10).unwrap();
    let entry = audit
        .iter()
        .find(|e| e.origin.as_deref() == Some("a plan draft"))
        .expect("the draft is in the audit");
    assert_eq!(entry.peer, "owner");
    assert_eq!(entry.model.as_deref(), Some("planner-1 (low reasoning)"));
    assert_eq!(entry.attempts, Some(2));
    assert_eq!(
        entry.prompt_sha256,
        proofkin::evidence::sha256_hex(goal.as_bytes())
    );

    let empty = call("POST", serde_json::json!({ "goal": "  " })).await;
    assert_eq!(empty.status, 400);
    assert!(
        empty.json()["error"]
            .as_str()
            .unwrap()
            .contains("say what the plan should do"),
        "{}",
        empty.body
    );
    let unsigned = call(
        "POST",
        serde_json::json!({ "goal": goal, "provider": "claude" }),
    )
    .await;
    assert_eq!(unsigned.status, 409);
    assert!(
        unsigned.json()["error"]
            .as_str()
            .unwrap()
            .contains("proofkin auth login claude --profile owner"),
        "{}",
        unsigned.body
    );
    assert_eq!(planner.prompts.lock().unwrap().len(), 2);
    agent.shutdown().await;
}

#[tokio::test]
async fn the_owner_sets_and_clears_a_folder_task_limit_in_the_web_ui() {
    let home = tempfile::tempdir().unwrap();
    let (agent, port, token) = solo_ui(home.path()).await;
    let profile = Profile::open_in(home.path(), "solo").unwrap();
    let (_, shares) = profile
        .update_shares(|shares| {
            shares.add("app", home.path(), Access::Full)?;
            Ok(())
        })
        .unwrap();
    agent.set_shares(shares);
    let host = format!("127.0.0.1:{port}");
    let put = |body: &'static str| {
        request(
            port,
            "PUT",
            "/api/shares/app/tasks",
            &host,
            Some(&token),
            Some(body),
        )
    };
    let shown = || async {
        request(port, "GET", "/api/state", &host, Some(&token), None)
            .await
            .json()["me"]["shares"]
            .as_array()
            .unwrap()
            .iter()
            .find(|share| share["name"] == "app")
            .unwrap()["task_limit"]
            .clone()
    };
    assert!(shown().await.is_null(), "no limit by default");

    let set = put(r#"{"tasks":"1"}"#).await;
    assert_eq!(set.status, 200, "{}", set.body);
    assert_eq!(
        set.json(),
        serde_json::json!({ "name": "app", "task_limit": 1 })
    );
    assert_eq!(shown().await, 1);
    let saved = || {
        profile
            .shares()
            .unwrap()
            .get("app")
            .unwrap()
            .task_limit
            .map(|n| n.get())
    };
    assert_eq!(saved(), Some(1));
    let invalid = put(r#"{"tasks":"0"}"#).await;
    assert_eq!(invalid.status, 400, "{}", invalid.body);
    assert!(
        invalid.json()["error"]
            .as_str()
            .unwrap()
            .contains("1 or more")
    );
    assert_eq!(saved(), Some(1));
    assert_eq!(shown().await, 1);

    let cleared = put(r#"{"tasks":"any"}"#).await;
    assert_eq!(cleared.status, 200, "{}", cleared.body);
    assert!(cleared.json()["task_limit"].is_null());
    assert!(shown().await.is_null());
    assert_eq!(saved(), None);
    agent.shutdown().await;
}

#[tokio::test]
async fn plan_draft_refuses_disallowed_models_and_efforts_before_reading_an_issue() {
    let home = tempfile::tempdir().unwrap();
    let (_, agent, planner, port, token) = plan_draft_ui(home.path()).await;
    let host = format!("127.0.0.1:{port}");
    for (body, message) in [
        (
            serde_json::json!({ "issue": 42, "model": "gpt-5.5" }),
            "gpt-5.5 isn't a model allowed here: claude-opus-5.5, planner-1",
        ),
        (
            serde_json::json!({ "issue": 42, "effort": "turbo" }),
            r#""turbo" isn't a reasoning effort: use one of"#,
        ),
        (
            serde_json::json!({ "issue": 42, "effort": "high" }),
            "high isn't a reasoning effort allowed here: low, medium",
        ),
    ] {
        // There is no tracker: a later check would report that instead of the choice.
        let response = request(
            port,
            "POST",
            "/api/plans/draft",
            &host,
            Some(&token),
            Some(&body.to_string()),
        )
        .await;
        assert_eq!(response.status, 400, "{}", response.body);
        assert!(
            response.json()["error"].as_str().unwrap().contains(message),
            "{}",
            response.body
        );
    }
    assert!(planner.prompts.lock().unwrap().is_empty());
    agent.shutdown().await;
}

/// An owner sets, replaces and removes a shared folder's team plan through the API;
/// the running agent and `shares.json` follow, and bad plans change nothing.
#[tokio::test]
async fn the_owner_sets_and_removes_a_folders_team_plan_through_the_api() {
    let home = tempfile::tempdir().unwrap();
    let app = tempfile::tempdir().unwrap();
    let profile = Profile::open_in(home.path(), "owner").unwrap();
    let mut shares = profile.shares().unwrap();
    shares.add("app", app.path(), Access::ReadOnly).unwrap();
    shares.save(&profile.shares_path()).unwrap();
    let mut config = AgentConfig::new("owner", SecretKey::generate());
    config.network = Network::Local;
    config.bind = Some("127.0.0.1:0".parse().unwrap());
    config.shares = shares;
    let (agent, events) = Agent::bind(config).await.unwrap();
    let info = UiInfo {
        profile: profile.clone(),
        network: Network::Local,
        serving: true,
        access: Access::ReadOnly,
        workspace: home.path().to_path_buf(),
        providers: Vec::new(),
        accounts: Vec::new(),
        workers: None,
    };
    let server = ui::start(
        agent.clone(),
        events,
        info,
        None,
        "127.0.0.1:0".parse().unwrap(),
    )
    .await
    .unwrap();
    let rest = server.url.strip_prefix("http://127.0.0.1:").unwrap();
    let (port, token) = rest.split_once("/#t=").unwrap();
    let port: u16 = port.parse().unwrap();
    let host = format!("127.0.0.1:{port}");
    let call = |method: &'static str, path: &'static str, body: Value| {
        let (host, token) = (host.clone(), token.to_owned());
        async move {
            let body = (!body.is_null()).then(|| body.to_string());
            request(port, method, path, &host, Some(&token), body.as_deref()).await
        }
    };
    let plan_of = |shares: Vec<proofkin::shares::Share>| {
        shares
            .into_iter()
            .find(|s| s.name == "app")
            .and_then(|s| s.team)
    };
    let in_state = || async {
        let state = call("GET", "/api/state", Value::Null).await.json();
        state["me"]["shares"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == "app")
            .map(|s| s["team"].clone())
            .unwrap()
    };
    let team = "/api/shares/app/team";

    let set = call(
        "PUT",
        team,
        serde_json::json!({ "plan": "build @bob {task}" }),
    )
    .await;
    assert_eq!(set.status, 200, "{}", set.body);
    assert_eq!(set.json(), serde_json::json!({ "name": "app", "steps": 1 }));
    assert_eq!(in_state().await, "build @bob {task}");
    assert_eq!(
        plan_of(profile.shares().unwrap().shares).as_deref(),
        Some("build @bob {task}")
    );
    assert_eq!(
        plan_of(agent.shares().shares).as_deref(),
        Some("build @bob {task}")
    );

    let two = "build @bob {task}\ncheck @bob review {task}";
    let replaced = call("PUT", team, serde_json::json!({ "plan": two })).await;
    assert_eq!(replaced.status, 200, "{}", replaced.body);
    assert_eq!(replaced.json()["steps"], 2);
    assert_eq!(in_state().await, two);

    let no_task = call("PUT", team, serde_json::json!({ "plan": "build @bob now" })).await;
    assert_eq!(no_task.status, 400, "{}", no_task.body);
    assert!(
        no_task.json()["error"].as_str().unwrap().contains("{task}"),
        "{}",
        no_task.body
    );
    let blank = call("PUT", team, serde_json::json!({ "plan": "  " })).await;
    assert_eq!(blank.status, 400, "{}", blank.body);
    let data = call(
        "PUT",
        "/api/shares/data/team",
        serde_json::json!({ "plan": "build @bob {task}" }),
    )
    .await;
    assert_eq!(data.status, 400, "{}", data.body);
    assert!(
        data.json()["error"].as_str().unwrap().contains("data"),
        "{}",
        data.body
    );
    assert_eq!(in_state().await, two);
    assert_eq!(
        plan_of(profile.shares().unwrap().shares).as_deref(),
        Some(two)
    );

    let unknown = call(
        "PUT",
        "/api/shares/nope/team",
        serde_json::json!({ "plan": "build @bob {task}" }),
    )
    .await;
    assert_eq!(unknown.status, 404, "{}", unknown.body);
    assert!(unknown.json()["error"].is_string());
    let unknown = call("DELETE", "/api/shares/nope/team", Value::Null).await;
    assert_eq!(unknown.status, 404, "{}", unknown.body);

    let removed = call("DELETE", team, Value::Null).await;
    assert_eq!(removed.status, 200, "{}", removed.body);
    assert_eq!(removed.json(), serde_json::json!({ "name": "app" }));
    assert!(in_state().await.is_null());
    assert_eq!(plan_of(profile.shares().unwrap().shares), None);
    assert_eq!(plan_of(agent.shares().shares), None);
}

/// An owner has a shared folder's model write the team plan of each team step through
/// the API, approved by them first if they like: the page's shares, the running agent
/// and `shares.json` follow and the activity says so; a written plan replaces it (and it
/// the plan), removing the team plan stops it, and a body that says neither, or both, is
/// refused with what to send.
#[tokio::test]
async fn the_owner_lets_a_folders_model_write_its_team_plans_through_the_api() {
    let home = tempfile::tempdir().unwrap();
    let app = tempfile::tempdir().unwrap();
    let profile = Profile::open_in(home.path(), "owner").unwrap();
    let mut shares = profile.shares().unwrap();
    shares.add("app", app.path(), Access::ReadOnly).unwrap();
    shares.save(&profile.shares_path()).unwrap();
    let mut config = AgentConfig::new("owner", SecretKey::generate());
    config.network = Network::Local;
    config.bind = Some("127.0.0.1:0".parse().unwrap());
    config.shares = shares;
    let (agent, events) = Agent::bind(config).await.unwrap();
    let info = UiInfo {
        profile: profile.clone(),
        network: Network::Local,
        serving: true,
        access: Access::ReadOnly,
        workspace: home.path().to_path_buf(),
        providers: Vec::new(),
        accounts: Vec::new(),
        workers: None,
    };
    let server = ui::start(
        agent.clone(),
        events,
        info,
        None,
        "127.0.0.1:0".parse().unwrap(),
    )
    .await
    .unwrap();
    let rest = server.url.strip_prefix("http://127.0.0.1:").unwrap();
    let (port, token) = rest.split_once("/#t=").unwrap();
    let port: u16 = port.parse().unwrap();
    let host = format!("127.0.0.1:{port}");
    let call = |method: &'static str, path: &'static str, body: Value| {
        let (host, token) = (host.clone(), token.to_owned());
        async move {
            let body = (!body.is_null()).then(|| body.to_string());
            request(port, method, path, &host, Some(&token), body.as_deref()).await
        }
    };
    // What the folder does with team steps: its plan, whether its model writes them, and
    // whether its owner approves each, as the page has it, with the activity's last line
    // about it.
    let in_state = || async {
        let state = call("GET", "/api/state", Value::Null).await.json();
        let app = state["me"]["shares"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == "app")
            .cloned()
            .unwrap();
        let said = state["feed"]
            .as_array()
            .unwrap()
            .iter()
            .rev()
            .filter_map(|f| f["text"].as_str())
            .find(|t| t.starts_with("'app'"))
            .unwrap_or_default()
            .to_owned();
        (
            app["team"].clone(),
            app["team_model"].clone(),
            app["team_approval"].clone(),
            said,
        )
    };
    let kept = |shares: Vec<proofkin::shares::Share>| {
        let app = shares.into_iter().find(|s| s.name == "app").unwrap();
        (app.team, app.team_model, app.team_approval)
    };
    let team = "/api/shares/app/team";
    let yes = Value::from(true);
    let model_writes = "'app' takes team steps; its model writes the team plan for each one";
    let approved = format!("{model_writes}, approved by you");

    let set = call(
        "PUT",
        team,
        serde_json::json!({ "model": true, "approve": true }),
    )
    .await;
    assert_eq!(set.status, 200, "{}", set.body);
    assert_eq!(
        set.json(),
        serde_json::json!({ "name": "app", "model": true, "approve": true })
    );
    assert_eq!(
        in_state().await,
        (Value::Null, yes.clone(), yes.clone(), approved.clone())
    );
    assert_eq!(kept(profile.shares().unwrap().shares), (None, true, true));
    assert_eq!(kept(agent.shares().shares), (None, true, true));

    let unapproved = call("PUT", team, serde_json::json!({ "model": true })).await;
    assert_eq!(unapproved.status, 200, "{}", unapproved.body);
    assert_eq!(unapproved.json()["approve"], false);
    assert_eq!(
        in_state().await,
        (
            Value::Null,
            yes.clone(),
            Value::Null,
            model_writes.to_owned()
        )
    );
    assert_eq!(kept(agent.shares().shares), (None, true, false));

    // A written plan replaces it, and the model replaces the plan.
    let plan = "build @bob {task}";
    let written = call("PUT", team, serde_json::json!({ "plan": plan })).await;
    assert_eq!(written.status, 200, "{}", written.body);
    let (team_plan, team_model, team_approval, _) = in_state().await;
    assert_eq!(
        (team_plan, team_model, team_approval),
        (Value::from(plan), Value::Null, Value::Null)
    );
    assert_eq!(
        kept(profile.shares().unwrap().shares),
        (Some(plan.to_owned()), false, false)
    );
    let again = call(
        "PUT",
        team,
        serde_json::json!({ "model": true, "approve": true }),
    )
    .await;
    assert_eq!(again.status, 200, "{}", again.body);
    assert_eq!(kept(agent.shares().shares), (None, true, true));

    // A body that says neither, or both, is refused with what to send, and changes nothing.
    for body in [
        serde_json::json!({}),
        serde_json::json!({ "approve": true }),
        serde_json::json!({ "plan": plan, "model": true }),
        serde_json::json!({ "plan": plan, "approve": true }),
    ] {
        let refused = call("PUT", team, body.clone()).await;
        assert_eq!(refused.status, 400, "{body}: {}", refused.body);
        assert!(
            refused.json()["error"]
                .as_str()
                .unwrap()
                .contains("\"model\": true"),
            "{body}: {}",
            refused.body
        );
    }
    assert_eq!(in_state().await, (Value::Null, yes.clone(), yes, approved));
    assert_eq!(kept(profile.shares().unwrap().shares), (None, true, true));
    let data = call(
        "PUT",
        "/api/shares/data/team",
        serde_json::json!({ "model": true }),
    )
    .await;
    assert_eq!(data.status, 400, "{}", data.body);
    assert!(
        data.json()["error"].as_str().unwrap().contains("data"),
        "{}",
        data.body
    );
    let unknown = call(
        "PUT",
        "/api/shares/nope/team",
        serde_json::json!({ "model": true }),
    )
    .await;
    assert_eq!(unknown.status, 404, "{}", unknown.body);

    // Removing the team plan stops its model writing them.
    let removed = call("DELETE", team, Value::Null).await;
    assert_eq!(removed.status, 200, "{}", removed.body);
    assert_eq!(
        in_state().await,
        (
            Value::Null,
            Value::Null,
            Value::Null,
            "'app' no longer takes team steps".to_owned()
        )
    );
    assert_eq!(kept(profile.shares().unwrap().shares), (None, false, false));
    assert_eq!(kept(agent.shares().shares), (None, false, false));
}

/// A team step in the web UI: the boss's task shows the lead's team run and its
/// steps (signed), the lead's page shows that team run as a plan of its own, marked
/// for the boss, and the boss sees how busy the lead is.
#[tokio::test]
async fn team_runs_show_on_the_bosss_step_and_the_leads_page() {
    let home = tempfile::tempdir().unwrap();
    let app = tempfile::tempdir().unwrap();
    let (boss_key, lead_key, member_key) = (
        SecretKey::generate(),
        SecretKey::generate(),
        SecretKey::generate(),
    );
    let trust = |name: &str, key: &SecretKey, delegate: bool, access: Access, shares: &[&str]| {
        TrustedPeer {
            name: name.into(),
            id: key.public(),
            addrs: Default::default(),
            can_delegate: delegate,
            tasks_per_hour: 100,
            access,
            shares: shares.iter().map(|s| (*s).to_owned()).collect(),
            m365: None,
            github: None,
            agents: Vec::new(),
        }
    };
    let local = |config: &mut AgentConfig| {
        config.network = Network::Local;
        config.bind = Some("127.0.0.1:0".parse().unwrap());
    };
    let mut member = AgentConfig::new("member", member_key.clone());
    local(&mut member);
    member
        .trust
        .allow(trust("lead", &lead_key, true, Access::ReadOnly, &["data"]));
    member.executor = Some(Arc::new(Echo));
    member.shares = ShareStore::with_data(home.path().join("member-data"));
    let (member, _me) = Agent::bind(member).await.unwrap();

    let lead_profile = Profile::open_in(home.path(), "lead").unwrap();
    let mut lead = AgentConfig::new("lead", lead_key.clone());
    local(&mut lead);
    lead.trust.allow(trust(
        "boss",
        &boss_key,
        true,
        Access::WorkspaceWrite,
        &["data", "app"],
    ));
    lead.trust
        .allow(trust("member", &member_key, false, Access::ReadOnly, &[]));
    lead.executor = Some(Arc::new(Echo));
    lead.shares = ShareStore::with_data(home.path().join("lead-data"));
    lead.shares.add("app", app.path(), Access::Full).unwrap();
    lead.shares
        .set_team("app", Some("dev @member {task}".into()))
        .unwrap();
    lead.team = Some(Arc::new(proofkin::team::TeamRunner::new(
        lead_profile.clone(),
    )));
    let (lead, lead_events) = Agent::bind(lead).await.unwrap();

    let mut boss = AgentConfig::new("boss", boss_key);
    local(&mut boss);
    boss.trust
        .allow(trust("lead", &lead_key, false, Access::ReadOnly, &[]));
    let (boss, boss_events) = Agent::bind(boss).await.unwrap();
    lead.connect(member.addr()).await.unwrap();
    boss.connect(lead.addr()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while lead.peers().len() < 2 || boss.peers().is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();

    let ui_for = |agent: &Agent, events, profile: Profile| {
        let info = UiInfo {
            profile,
            network: Network::Local,
            serving: true,
            access: Access::ReadOnly,
            workspace: home.path().to_path_buf(),
            providers: Vec::new(),
            accounts: Vec::new(),
            workers: None,
        };
        ui::start(
            agent.clone(),
            events,
            info,
            None,
            "127.0.0.1:0".parse().unwrap(),
        )
    };
    let address = |url: &str| {
        let rest = url.strip_prefix("http://127.0.0.1:").unwrap();
        let (port, token) = rest.split_once("/#t=").unwrap();
        (port.parse::<u16>().unwrap(), token.to_owned())
    };
    let boss_ui = ui_for(
        &boss,
        boss_events,
        Profile::open_in(home.path(), "boss").unwrap(),
    )
    .await
    .unwrap();
    let lead_ui = ui_for(&lead, lead_events, lead_profile).await.unwrap();
    let (boss_port, boss_token) = address(&boss_ui.url);
    let (lead_port, lead_token) = address(&lead_ui.url);
    let state = |port: u16, token: String| async move {
        request(
            port,
            "GET",
            "/api/state",
            &format!("127.0.0.1:{port}"),
            Some(&token),
            None,
        )
        .await
        .json()
    };

    let text = serde_json::json!({ "text": "build team @lead:app make the thing" });
    let started = request(
        boss_port,
        "POST",
        "/api/plans",
        &format!("127.0.0.1:{boss_port}"),
        Some(&boss_token),
        Some(&text.to_string()),
    )
    .await;
    assert_eq!(started.status, 200, "{}", started.body);
    let step = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let s = state(boss_port, boss_token.clone()).await;
            if let Some(t) = s["tasks"]
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["step"] == "build" && t["status"] == "done")
            {
                return t.clone();
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the team step ends");
    assert_eq!(step["evidence"], "verified", "{step}");
    assert_eq!(
        step["team_step"], true,
        "shown as a team step, not a model's task"
    );
    assert_eq!(step["team"]["lead"], "lead");
    let dev = &step["team"]["steps"][0];
    assert_eq!(
        (&dev["id"], &dev["status"], &dev["target"], &dev["signed"]),
        (
            &serde_json::json!("dev"),
            &serde_json::json!("done"),
            &serde_json::json!("member"),
            &serde_json::json!(true)
        ),
        "{step}"
    );
    let run = step["team"]["run"].as_str().unwrap().to_owned();

    // The lead's page has the team run as a plan of its own, for the boss.
    let ran = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let s = state(lead_port, lead_token.clone()).await;
            if let Some(t) = s["tasks"]
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["step"] == "dev" && t["run"] == run.as_str())
            {
                return t.clone();
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the lead shows its team run");
    assert_eq!(
        (&ran["for_peer"], &ran["status"], &ran["target"]),
        (
            &serde_json::json!("boss"),
            &serde_json::json!("done"),
            &serde_json::json!("member")
        ),
        "{ran}"
    );
    assert!(ran["plan"].is_u64());
    // The boss sees how busy the lead is (it serves tasks).
    let peers = state(boss_port, boss_token.clone()).await["peers"].clone();
    assert_eq!(peers[0]["load"]["slots"], 2, "{peers}");
    boss.shutdown().await;
    lead.shutdown().await;
    member.shutdown().await;
}

/// A Work IQ that answers every search with `results` (the `structuredContent` of `retrieve`).
#[derive(Debug)]
struct SearchResults(Vec<Value>);

#[async_trait]
impl proofkin::workiq::context::WorkContext for SearchResults {
    async fn retrieve(
        &self,
        _context: &proofkin::workiq::audit::CallContext,
        _query: &str,
        _strategy: &str,
        _capabilities: &[&str],
    ) -> anyhow::Result<proofkin::workiq::ToolResult> {
        Ok(proofkin::workiq::ToolResult {
            content: Vec::new(),
            structured_content: Some(serde_json::json!({
                "application/vnd.ms-workiq.retrieval": {
                    "results": self.0,
                    "stoppedReason": "completed",
                }
            })),
            is_error: false,
            ids: proofkin::workiq::RequestIds::default(),
        })
    }
}

fn found_file(id: &str, label: &str, text: &str, protection: &str) -> Value {
    serde_json::json!({
        "id": id,
        "markdown": text,
        "entityType": "file",
        "contentProtection": protection,
        "sensitivityLabel": { "displayName": label },
        "relevanceScore": 0.5,
    })
}

/// A UI whose Work IQ answers with `results`: its profile, agent, port and token.
async fn ui_with_work_results(
    home: &std::path::Path,
    results: Vec<Value>,
) -> (Profile, Agent, u16, String) {
    ui_with_work(home, Arc::new(SearchResults(results))).await
}

/// A UI whose Work IQ is `work`: its profile, agent, port and token.
async fn ui_with_work(
    home: &std::path::Path,
    work: Arc<dyn proofkin::workiq::context::WorkContext>,
) -> (Profile, Agent, u16, String) {
    let profile = Profile::open_in(home, "owner").unwrap();
    let mut config = AgentConfig::new("owner", SecretKey::generate());
    config.network = Network::Local;
    config.bind = Some("127.0.0.1:0".parse().unwrap());
    let (agent, events) = Agent::bind(config).await.unwrap();
    let info = UiInfo {
        profile: profile.clone(),
        network: Network::Local,
        serving: false,
        access: Access::ReadOnly,
        workspace: home.to_path_buf(),
        providers: Vec::new(),
        accounts: Vec::new(),
        workers: None,
    };
    let server = ui::start_with_work(
        agent.clone(),
        events,
        info,
        None,
        "127.0.0.1:0".parse().unwrap(),
        Some(work),
    )
    .await
    .unwrap();
    let rest = server.url.strip_prefix("http://127.0.0.1:").unwrap();
    let (port, token) = rest.split_once("/#t=").unwrap();
    (profile, agent, port.parse().unwrap(), token.to_owned())
}

async fn import_workiq(port: u16, token: Option<&str>, body: Value) -> Response {
    let host = format!("127.0.0.1:{port}");
    let body = body.to_string();
    request(
        port,
        "POST",
        "/api/kb/import/workiq",
        &host,
        token,
        Some(&body),
    )
    .await
}

#[tokio::test]
async fn the_workiq_import_route_saves_a_marked_topic_and_needs_replace_to_overwrite() {
    let home = tempfile::tempdir().unwrap();
    let (profile, agent, port, token) = ui_with_work_results(
        home.path(),
        vec![
            found_file("f1", "General", "Launch is in May.", "allowed"),
            found_file("f2", "Confidential", "Budget is four million.", "allowed"),
            found_file("f3", "General", "Hidden.", "blocked"),
        ],
    )
    .await;
    let token = Some(token.as_str());

    let anonymous = import_workiq(port, None, serde_json::json!({})).await;
    assert_eq!(anonymous.status, 401, "token-guarded");

    let body = serde_json::json!({ "topic": "launch", "query": "the launch", "from": "files" });
    let done = import_workiq(port, token, body).await;
    assert_eq!(done.status, 200, "{}", done.body);
    let answer = done.json();
    assert_eq!(answer["topic"], "launch");
    assert_eq!(
        (answer["kept"].as_u64(), answer["withheld"].as_u64()),
        (Some(2), Some(1))
    );
    assert_eq!(
        answer["labels"],
        serde_json::json!(["Confidential", "General"])
    );
    let text = profile.knowledge().read("launch").unwrap();
    assert_eq!(
        proofkin::knowledge::parse_mark(&text),
        Some(vec!["Confidential".to_owned(), "General".to_owned()])
    );
    assert!(text.contains("query: the launch\n"), "{text}");
    assert!(text.contains("<!-- source: Work IQ retrieve -->"), "{text}");

    let again = import_workiq(
        port,
        token,
        serde_json::json!({ "topic": "launch", "query": "more" }),
    )
    .await;
    assert_eq!(again.status, 400);
    assert!(again.json()["error"].as_str().unwrap().contains("replace"));
    assert_eq!(profile.knowledge().read("launch").unwrap(), text);

    let replaced = import_workiq(
        port,
        token,
        serde_json::json!({ "topic": "launch", "query": "more", "replace": true }),
    )
    .await;
    assert_eq!(replaced.status, 200, "{}", replaced.body);
    let text = profile.knowledge().read("launch").unwrap();
    assert!(text.contains("query: more\n"), "{text}");

    let bad = import_workiq(
        port,
        token,
        serde_json::json!({ "topic": "x", "query": "q", "from": "slack" }),
    )
    .await;
    assert_eq!(bad.status, 400);
    assert!(bad.json()["error"].as_str().unwrap().contains("slack"));
    agent.shutdown().await;
}

const SPEC_DOCUMENT: &str = "# Albums\n\nFR-001: Users can share an album.\n";

/// A Work IQ whose every document is `SPEC_DOCUMENT`, a Markdown spec labelled General.
#[derive(Debug)]
struct SpecDocument;

/// A tool result whose structured content is `content`.
fn structured(content: Value) -> proofkin::workiq::ToolResult {
    proofkin::workiq::ToolResult {
        content: Vec::new(),
        structured_content: Some(content),
        is_error: false,
        ids: proofkin::workiq::RequestIds::default(),
    }
}

#[async_trait]
impl proofkin::workiq::context::WorkContext for SpecDocument {
    async fn retrieve(
        &self,
        _context: &proofkin::workiq::audit::CallContext,
        _query: &str,
        _strategy: &str,
        _capabilities: &[&str],
    ) -> anyhow::Result<proofkin::workiq::ToolResult> {
        anyhow::bail!("this Work IQ only has documents")
    }

    async fn fetch(
        &self,
        _context: &proofkin::workiq::audit::CallContext,
        _entity_urls: &[&str],
    ) -> anyhow::Result<proofkin::workiq::ToolResult> {
        let data = serde_json::json!({
            "name": "Albums.md", "size": SPEC_DOCUMENT.len(),
            "file": { "mimeType": "text/markdown" },
            "sensitivityLabel": { "displayName": "General" },
        });
        let item = serde_json::json!({ "statusCode": 200, "data": data });
        Ok(structured(serde_json::json!({ "results": [item] })))
    }

    async fn fetch_blob(
        &self,
        _context: &proofkin::workiq::audit::CallContext,
        _path: &str,
    ) -> anyhow::Result<proofkin::workiq::ToolResult> {
        let content = data_encoding::BASE64.encode(SPEC_DOCUMENT.as_bytes());
        Ok(structured(serde_json::json!({
            "statusCode": 200, "contentType": "text/markdown", "blobName": "Albums.md",
            "sizeBytes": SPEC_DOCUMENT.len(), "base64Content": content,
        })))
    }
}

#[tokio::test]
async fn plan_mode_imports_a_spec_document_from_sharepoint_through_work_iq() {
    let home = tempfile::tempdir().unwrap();
    let (profile, agent, port, token) = ui_with_work(home.path(), Arc::new(SpecDocument)).await;
    let host = format!("127.0.0.1:{port}");
    let path = "/api/playbooks/implement-speckit";
    let link = "https://contoso.sharepoint.com/sites/Team/Shared%20Documents/Albums.md";
    let vars = serde_json::json!({ "spec": link, "peer": "alice" });
    let body = serde_json::json!({ "vars": vars }).to_string();
    let filled = request(port, "POST", path, &host, Some(&token), Some(&body)).await;
    assert_eq!(filled.status, 200, "{}", filled.body);

    // Its requirement is the plan's claim, and it is saved as a topic marked with its label.
    let filled = filled.json();
    let text = filled["text"].as_str().unwrap();
    let claim = "claim: FR-001 Users can share an album.\n";
    assert!(text.contains(claim), "{text}");
    let imported = format!("// Imported from {link}. ");
    assert!(text.contains(&imported), "{text}");
    assert_eq!(filled["knowledge"], serde_json::json!(["spec-albums"]));
    let topic = profile.knowledge().read("spec-albums").unwrap();
    assert_eq!(
        proofkin::knowledge::parse_mark(&topic),
        Some(vec!["General".to_owned()])
    );
    let summary = filled["notes"][0].as_str().unwrap();
    let label = "Its sensitivity label is General";
    assert!(summary.contains(label), "{summary}");

    // A link off SharePoint and OneDrive is refused, in the server's words.
    let vars = serde_json::json!({ "spec": "https://example.com/Albums.md", "peer": "alice" });
    let body = serde_json::json!({ "vars": vars }).to_string();
    let refused = request(port, "POST", path, &host, Some(&token), Some(&body)).await;
    assert_eq!(refused.status, 400);
    let error = refused.json()["error"].as_str().unwrap().to_owned();
    let off = "is not a SharePoint or OneDrive link";
    assert!(error.contains(off), "{error}");
    agent.shutdown().await;
}

#[tokio::test]
async fn the_workiq_import_route_writes_nothing_when_every_result_was_withheld() {
    let home = tempfile::tempdir().unwrap();
    let (profile, agent, port, token) = ui_with_work_results(
        home.path(),
        vec![found_file("f1", "General", "Hidden.", "blocked")],
    )
    .await;
    let refused = import_workiq(
        port,
        Some(&token),
        serde_json::json!({ "topic": "launch", "query": "q" }),
    )
    .await;
    assert_eq!(refused.status, 400);
    let error = refused.json()["error"].as_str().unwrap().to_owned();
    assert!(
        error.contains("1 in all") && error.contains("blocked"),
        "{error}"
    );
    assert!(profile.knowledge().list().is_empty());
    agent.shutdown().await;
}

/// The Knowledge section imports from Microsoft 365: its button opens a dialog with the
/// topic, what to look for, where to look and whether to replace the topic, which says the
/// topic keeps where it came from, closes with Escape and Cancel, shows the route's answer or
/// error, and sends what the import route takes (by default all of Microsoft 365); the
/// knowledge list then has the topic, with its labels.
#[tokio::test]
async fn the_knowledge_section_imports_from_microsoft_365_through_a_dialog() {
    let home = tempfile::tempdir().unwrap();
    let (_, agent, port, token) = ui_with_work_results(
        home.path(),
        vec![
            found_file("f1", "General", "Launch is in May.", "allowed"),
            found_file("f2", "General", "Hidden.", "blocked"),
        ],
    )
    .await;
    let host = format!("127.0.0.1:{port}");
    let page = request(port, "GET", "/", &host, None, None).await.body;
    let at = |needle: &str| page.find(needle).expect(needle);
    let kb = at(r#"<section class="runs-panel" aria-labelledby="kb-h">"#);
    let section = &page[kb..kb + page[kb..].find("</section>").unwrap()];
    assert!(
        section.contains(r#"id="kb-import-btn">Import from Microsoft 365</button>"#),
        "{section}"
    );
    let start = at(r#"<dialog id="kb-import-dialog""#);
    let dialog = &page[start..start + page[start..].find("</dialog>").unwrap()];
    for words in [
        "<span>Topic name</span>",
        "<span>What to look for</span>",
        r#"maxlength="1000""#,
        "<legend>Where to look</legend>",
        "<span>Replace the topic if it exists</span>",
        "The topic keeps where it came from: Microsoft 365, with the sensitivity labels of what it",
        "it goes only to peers whose Microsoft 365 grant covers those labels",
        r#"<p class="small" id="kb-import-result" role="status"></p>"#,
        r#"<p class="form-error" id="kb-import-error" role="alert"></p>"#,
        r#"<button type="button" class="btn" id="kb-import-cancel">Cancel</button>"#,
        r#"<button type="submit" class="btn primary" id="kb-import-submit">Import topic</button>"#,
    ] {
        assert!(dialog.contains(words), "{words}: {dialog}");
    }
    let app = request(port, "GET", "/app.js", &host, None, None)
        .await
        .body;
    let guarded = regex::Regex::new(r"for \(const id of \[([^\]]+)\]\) guardDialog")
        .unwrap()
        .captures(&app)
        .expect("app.js guards its dialogs")[1]
        .to_owned();
    assert!(guarded.contains("\"#kb-import-dialog\""), "{guarded}");
    let closes = concat!(
        r##"$("#kb-import-cancel").addEventListener("click", "##,
        r##"() => $("#kb-import-dialog").close());"##,
    );
    assert!(app.contains(closes), "Cancel doesn't close the dialog");
    // An element app.js looks up that isn't on the page stops it starting: the button is in
    // the Knowledge section, the rest in the dialog.
    let ids = regex::Regex::new(r##""#(kb-import[\w-]*)"##)
        .unwrap()
        .captures_iter(&app)
        .map(|c| c[1].to_owned())
        .collect::<std::collections::BTreeSet<_>>();
    assert!(ids.len() >= 10, "{ids:?}");
    for id in ids.iter().filter(|id| *id != "kb-import-btn") {
        assert!(
            dialog.contains(&format!(r#"id="{id}""#)),
            "#{id} isn't in the dialog"
        );
    }
    assert!(
        app.contains(r#"api("/api/kb/import/workiq""#),
        "the dialog doesn't import"
    );

    // What the dialog sends unless told otherwise: all of Microsoft 365, not replacing.
    let body = serde_json::json!({
        "topic": "launch",
        "query": "the launch",
        "from": "all",
        "replace": false,
    });
    let done = import_workiq(port, Some(&token), body).await;
    assert_eq!(done.status, 200, "{}", done.body);
    assert_eq!(
        done.json(),
        serde_json::json!({ "topic": "launch", "kept": 1, "withheld": 1, "labels": ["General"] })
    );
    let ops = call_ui(port, Some(&token), "GET", "/api/ops", Value::Null)
        .await
        .json();
    let topic = ops["knowledge"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "launch")
        .cloned()
        .expect("the knowledge list has the topic");
    assert_eq!(topic["workiq"], serde_json::json!(["General"]));
    agent.shutdown().await;
}

/// A saved run answers, for each of its steps that had Microsoft 365 work context, where it
/// came from: the sources, their labels, and how many results were kept and withheld, which
/// the run dialog shows. Never the context itself: the run keeps it in its folder, and
/// nothing of it reaches the page.
#[tokio::test]
async fn a_run_answers_its_steps_microsoft_365_sources_but_never_what_they_said() {
    use proofkin::plan::WorkRecord;
    use proofkin::workiq::content::Source;
    let home = tempfile::tempdir().unwrap();
    let profile = Profile::open_in(home.path(), "owner").unwrap();
    let text = "find @alice workiq=files,email Summarize the launch\n\
                notes after find @alice Write the launch notes";
    let plan = proofkin::plan::Plan::parse(text).unwrap();
    let mut run = proofkin::runs::RunRecord::new(
        "owner",
        &SecretKey::generate().public().to_string(),
        text,
        &plan,
        &std::collections::BTreeMap::new(),
        proofkin::runs::unix_now() - 60,
    );
    run.id = "run-1".into();
    // What step find's search found: two results kept, whose text went into its prompt, and
    // one withheld.
    let context = "### From Microsoft 365\nThe budget is four million; the launch moves to May.";
    let link = "https://contoso.sharepoint.com/sites/launch/Plan.docx";
    run.workiq.insert(
        "find".into(),
        WorkRecord {
            sources: vec![
                Source {
                    title: "Launch plan.docx".into(),
                    kind: "file".into(),
                    label: "General".into(),
                    link: Some(link.into()),
                },
                Source {
                    title: "Re: launch date".into(),
                    kind: "email".into(),
                    label: "General".into(),
                    link: None,
                },
            ],
            labels: vec!["General".into()],
            kept: 2,
            withheld: 1,
            request_ids: "req-1".into(),
            sha256: proofkin::evidence::sha256_hex(context.as_bytes()),
            text: context.into(),
        },
    );
    run.save(&profile.runs_dir()).unwrap();
    let kept = profile.runs_dir().join("run-1").join("find.workiq.md");
    assert_eq!(std::fs::read_to_string(kept).unwrap(), context);

    let (agent, port, token) = ui_of(home.path(), "owner").await;
    let path = "/api/runs/run-1";
    let anonymous = call_ui(port, None, "GET", path, Value::Null).await;
    assert_eq!(anonymous.status, 401, "{}", anonymous.body);
    let answer = call_ui(port, Some(&token), "GET", path, Value::Null).await;
    assert_eq!(answer.status, 200, "{}", answer.body);
    assert_eq!(
        answer.json()["workiq"],
        serde_json::json!([{
            "step": "find",
            "sources": [
                { "title": "Launch plan.docx", "kind": "file", "label": "General", "link": link },
                { "title": "Re: launch date", "kind": "email", "label": "General", "link": null },
            ],
            "labels": ["General"],
            "kept": 2,
            "withheld": 1,
        }]),
        "only the step that had some, without its text"
    );
    for words in ["four million", "moves to May", "From Microsoft 365"] {
        assert!(!answer.body.contains(words), "{words}: {}", answer.body);
    }

    // The run dialog shows them between the run's work and its report.
    let page = call_ui(port, None, "GET", "/", Value::Null).await.body;
    let at = |needle: &str| page.find(needle).expect(needle);
    let place = at(r#"<div id="run-workiq""#);
    assert!(at(r#"<dialog id="run-dialog""#) < at(r#"id="run-pulls""#));
    assert!(at(r#"id="run-pulls""#) < place);
    assert!(place < at(r#"id="run-report""#));
    let app = call_ui(port, None, "GET", "/app.js", Value::Null)
        .await
        .body;
    assert!(
        app.contains("renderRunWorkiq(run.workiq ?? [])"),
        "the run dialog doesn't show them"
    );
    agent.shutdown().await;
}

/// The owner signs in to Work IQ while the agent runs (`proofkin auth login workiq` in
/// another process): the web UI's plan check, plans and imports use the sign-in at once.
#[tokio::test]
async fn a_workiq_sign_in_made_while_the_agent_runs_counts_at_once() {
    let home = tempfile::tempdir().unwrap();
    let (_, agent, port, token) = ui_with_work_results(home.path(), Vec::new()).await;
    let check = || async {
        let host = format!("127.0.0.1:{port}");
        let body = serde_json::json!({ "text": "decisions @workiq What did we decide?" });
        request(
            port,
            "POST",
            "/api/plans/check",
            &host,
            Some(&token),
            Some(&body.to_string()),
        )
        .await
        .json()
    };
    let before = check().await;
    assert_eq!(before["ok"], false, "{before}");
    assert!(
        before["problems"][0]["message"]
            .as_str()
            .unwrap()
            .contains("needs this profile's Work IQ sign-in"),
        "{before}"
    );

    let mut signed_in = Profile::open_in(home.path(), "owner").unwrap();
    signed_in.config.workiq = Some(proofkin::profile::WorkIqAccount {
        connection: proofkin::profile::WorkIqConnection::Cli {
            account: Some("ada@contoso.example".into()),
            program: None,
        },
        upn: "ada@contoso.example".into(),
        display_name: "ada@contoso.example".into(),
        tenant_id: "unknown".into(),
        added: 0,
    });
    signed_in.save().unwrap();
    let after = check().await;
    assert_eq!(after["ok"], true, "{after}");
    agent.shutdown().await;
}

/// The fake Work IQ CLI (`workiq mcp` over stdio) of tests/workiq_cli.rs.
const FAKE_WORKIQ_CLI: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/workiq_cli_mcp.py"
);

/// A Work IQ CLI at a path of its own (not on the PATH), started as the real one is:
/// `workiq.cmd` on Windows, `workiq` elsewhere, in one of the fake's modes (`ok` offers
/// retrieve, send_mail and inspect; `full` retrieve, ask and list_agents). It logs what it
/// was asked in `state`.
fn fake_workiq_cli_at(
    dir: &std::path::Path,
    state: &std::path::Path,
    mode: &str,
) -> std::path::PathBuf {
    let (script, state) = (FAKE_WORKIQ_CLI, state.display());
    if cfg!(windows) {
        let path = dir.join("workiq.cmd");
        let text = format!("@python \"{script}\" \"{state}\" {mode} %*\r\n");
        std::fs::write(&path, text).unwrap();
        path
    } else {
        let path = dir.join("workiq");
        let text = format!("#!/bin/sh\nexec python3 '{script}' '{state}' {mode} \"$@\"\n");
        std::fs::write(&path, text).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }
}

/// Asks the web UI at `port`, with its access `token` unless it is `None`, sending `body`
/// unless it is null.
async fn call_ui(
    port: u16,
    token: Option<&str>,
    method: &str,
    path: &str,
    body: Value,
) -> Response {
    let host = format!("127.0.0.1:{port}");
    let body = (!body.is_null()).then(|| body.to_string());
    request(port, method, path, &host, token, body.as_deref()).await
}

/// The page's Work IQ account, only with the access token: signed in through a Work IQ CLI
/// at a path of its own, checked by the doctor, given budgets, and signed out again.
#[tokio::test]
async fn the_workiq_api_signs_in_through_the_cli_runs_the_doctor_sets_budgets_and_signs_out() {
    // A name of its own: where there is an OS credential store, signing in and out deletes
    // the profile's Work IQ token there (this one never had one).
    let name = "proofkin-ui-test-workiq";
    let home = tempfile::tempdir().unwrap();
    let (bin, cli_state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let cli = fake_workiq_cli_at(bin.path(), cli_state.path(), "ok");
    let (agent, port, token) = ui_of(home.path(), name).await;
    let call = |method: &'static str, path: &'static str, body: Value| {
        let token = token.clone();
        async move { call_ui(port, Some(&token), method, path, body).await }
    };
    let sign_in = serde_json::json!({
        "account": "ada@contoso.example",
        "cli_path": cli.display().to_string(),
    });
    let budget = serde_json::json!({ "per_day": 50, "per_run": 5 });

    for (method, path, body) in [
        ("GET", "/api/workiq", Value::Null),
        ("POST", "/api/workiq/sign-in", sign_in.clone()),
        ("POST", "/api/workiq/doctor", Value::Null),
        ("PUT", "/api/workiq/budget", budget.clone()),
        ("DELETE", "/api/workiq", Value::Null),
    ] {
        let anonymous = call_ui(port, None, method, path, body).await;
        assert_eq!(anonymous.status, 401, "{method} {path}: {}", anonymous.body);
    }

    let before = call("GET", "/api/workiq", Value::Null).await.json();
    assert_eq!(before["signed_in"], false, "{before}");
    for field in ["line", "connection", "user", "cli", "not_used"] {
        assert!(before[field].is_null(), "{field}: {before}");
    }
    assert_eq!(before["holder"], name);
    assert_eq!(before["own"], true);
    assert_eq!(before["changes"], false, "{before}");
    assert_eq!(
        before["hint"],
        format!("proofkin auth login workiq --profile {name}")
    );

    let signed_in = call("POST", "/api/workiq/sign-in", sign_in).await;
    assert_eq!(signed_in.status, 200, "{}", signed_in.body);
    let line = "ada@contoso.example through the Work IQ CLI";
    assert_eq!(
        signed_in.json(),
        serde_json::json!({ "who": "ada@contoso.example", "line": line })
    );

    let after = call("GET", "/api/workiq", Value::Null).await.json();
    assert_eq!(after["signed_in"], true, "{after}");
    assert_eq!(after["line"], line);
    assert_eq!(after["connection"], "cli");
    assert_eq!(after["user"], "ada@contoso.example");
    let path = cli.display().to_string();
    assert_eq!(
        after["cli"],
        serde_json::json!({ "account": "ada@contoso.example", "path": path })
    );
    let usage = &after["usage"];
    assert_eq!(usage["today"], 0, "{after}");
    assert_eq!(usage["per_day"], 200, "{after}");
    assert_eq!(usage["per_run"], 25, "{after}");
    assert_eq!(usage["runs"], serde_json::json!({}), "{after}");
    let day = usage["day"].as_str().unwrap();
    assert!(
        chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d").is_ok(),
        "{after}"
    );

    let doctor = call("POST", "/api/workiq/doctor", Value::Null).await;
    assert_eq!(doctor.status, 200, "{}", doctor.body);
    let report = doctor.json();
    assert_eq!(report["profile"], name);
    assert_eq!(report["ok"], true, "{report}");
    let checks = report["checks"].as_array().unwrap();
    let column = |key: &str| -> Vec<Value> { checks.iter().map(|c| c[key].clone()).collect() };
    assert_eq!(
        column("name"),
        ["discovery", "sign-in", "token", "tools", "retrieve"],
        "{report}"
    );
    assert_eq!(
        column("result"),
        ["skipped", "ok", "skipped", "ok", "ok"],
        "{report}"
    );
    assert_eq!(report["checks"][1]["detail"], line);

    let neither = call("PUT", "/api/workiq/budget", serde_json::json!({})).await;
    assert_eq!(neither.status, 400, "{}", neither.body);
    assert!(neither.body.contains("--per-day"), "{}", neither.body);
    let zero = serde_json::json!({ "per_day": 0, "per_run": 5 });
    let zero = call("PUT", "/api/workiq/budget", zero).await;
    assert_eq!(zero.status, 400, "{}", zero.body);
    assert!(zero.body.contains("at least one call"), "{}", zero.body);
    let set = call("PUT", "/api/workiq/budget", budget).await;
    assert_eq!(set.status, 200, "{}", set.body);
    assert_eq!(
        set.json(),
        serde_json::json!({ "per_day": 50, "per_run": 5 })
    );
    let budgeted = call("GET", "/api/workiq", Value::Null).await.json();
    let usage = &budgeted["usage"];
    assert_eq!(usage["per_day"], 50, "{budgeted}");
    assert_eq!(usage["per_run"], 5, "{budgeted}");
    // The doctor's retrieve counts against the budget (unless the day changed since).
    if usage["day"] == after["usage"]["day"] {
        assert_eq!(usage["today"], 1, "{budgeted}");
    }

    let state = call("GET", "/api/state", Value::Null).await.json();
    let feed = state["feed"].to_string();
    assert!(
        feed.contains("Work IQ signed in through the Work IQ CLI as ada@contoso.example"),
        "{feed}"
    );

    let signed_out = call("DELETE", "/api/workiq", Value::Null).await;
    assert_eq!(signed_out.status, 200, "{}", signed_out.body);
    let gone = call("GET", "/api/workiq", Value::Null).await.json();
    assert_eq!(gone["signed_in"], false, "{gone}");
    assert!(gone["line"].is_null() && gone["cli"].is_null(), "{gone}");
    // The budgets outlive the account.
    assert_eq!(gone["usage"]["per_day"], 50, "{gone}");
    let again = call("DELETE", "/api/workiq", Value::Null).await;
    assert_eq!(again.status, 400, "{}", again.body);
    assert!(again.body.contains("not signed in"), "{}", again.body);
    let state = call("GET", "/api/state", Value::Null).await.json();
    let feed = state["feed"].to_string();
    assert!(feed.contains("Work IQ signed out"), "{feed}");
    agent.shutdown().await;
}

/// The declarative agents a `@workiq` step can ask, for the plan builder's Agent list (which
/// asks this route): those Work IQ lists for the profile's account, only with the access
/// token, and a 400 that says why, on one line, when the profile isn't signed in or Work IQ
/// can't list them.
#[tokio::test]
async fn the_workiq_agents_route_lists_the_agents_work_iq_offers_and_says_why_it_cannot() {
    let home = tempfile::tempdir().unwrap();
    let (agent, port, token) = ui_of(home.path(), "owner").await;
    let path = "/api/workiq/agents";
    let anonymous = call_ui(port, None, "GET", path, Value::Null).await;
    assert_eq!(anonymous.status, 401, "{}", anonymous.body);

    let not_signed_in = call_ui(port, Some(&token), "GET", path, Value::Null).await;
    assert_eq!(not_signed_in.status, 400, "{}", not_signed_in.body);
    let hint = "proofkin auth login workiq --profile owner";
    assert_eq!(
        not_signed_in.json()["error"],
        format!("Work IQ is not signed in for profile 'owner'; run: {hint}")
    );

    // Signed in through a Work IQ CLI at a path of its own: the account a sign-in saves.
    let sign_in = |cli: std::path::PathBuf| {
        let mut profile = Profile::open_in(home.path(), "owner").unwrap();
        profile.config.workiq = Some(proofkin::profile::WorkIqAccount {
            connection: proofkin::profile::WorkIqConnection::Cli {
                account: Some("ada@contoso.example".into()),
                program: Some(cli),
            },
            upn: "ada@contoso.example".into(),
            display_name: "ada@contoso.example".into(),
            tenant_id: "unknown".into(),
            added: 0,
        });
        profile.save().unwrap();
    };
    let (ok_bin, ok_state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    sign_in(fake_workiq_cli_at(ok_bin.path(), ok_state.path(), "ok"));
    let unoffered = call_ui(port, Some(&token), "GET", path, Value::Null).await;
    assert_eq!(unoffered.status, 400, "{}", unoffered.body);
    let why = unoffered.json()["error"].as_str().unwrap().to_owned();
    assert!(
        why.contains("Work IQ does not offer the list_agents tool") && !why.contains('\n'),
        "{why}"
    );

    let (full_bin, full_state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    sign_in(fake_workiq_cli_at(
        full_bin.path(),
        full_state.path(),
        "full",
    ));
    let listed = call_ui(port, Some(&token), "GET", path, Value::Null).await;
    assert_eq!(listed.status, 200, "{}", listed.body);
    assert_eq!(
        listed.json(),
        serde_json::json!({ "agents": [{ "name": "Sales", "id": "sales:1" }] })
    );
    let builder = call_ui(port, None, "GET", "/plan-builder.js", Value::Null).await;
    assert!(
        builder.body.contains(&format!("api(\"{path}\")")),
        "the plan builder doesn't ask {path}"
    );
    agent.shutdown().await;
}

/// A `@workiq` step run from the web UI reads as answered by Microsoft 365 Copilot (or the
/// Work IQ agent it asks), not by its plan's provider: its task says it is one and names who
/// answered it as it is (`workiq:<agent id>`, an id with colons of its own). Its run's report
/// heads it "(Work IQ)" and says why it has no evidence, and the run's counts keep the
/// answer's `unknown` label, which the page words as "no label information".
#[tokio::test]
async fn a_workiq_steps_task_and_report_say_work_iq_answered_it() {
    let home = tempfile::tempdir().unwrap();
    let (bin, cli_state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let cli = fake_workiq_cli_at(bin.path(), cli_state.path(), "full");
    let mut profile = Profile::open_in(home.path(), "owner").unwrap();
    profile.config.workiq = Some(proofkin::profile::WorkIqAccount {
        connection: proofkin::profile::WorkIqConnection::Cli {
            account: Some("ada@contoso.example".into()),
            program: Some(cli),
        },
        upn: "ada@contoso.example".into(),
        display_name: "ada@contoso.example".into(),
        tenant_id: "unknown".into(),
        added: 0,
    });
    profile.save().unwrap();
    let (agent, port, token) = ui_of(home.path(), "owner").await;
    let call = |method: &'static str, path: &str, body: Value| {
        let (token, path) = (token.clone(), path.to_owned());
        async move { call_ui(port, Some(&token), method, &path, body).await }
    };

    let body = serde_json::json!({ "text": "ask @workiq agent=sales:1 What changed?" });
    let started = call("POST", "/api/plans", body).await;
    assert_eq!(started.status, 200, "{}", started.body);
    let task = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let state = call("GET", "/api/state", Value::Null).await.json();
            let ended = state["tasks"]
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["step"] == "ask" && !t["finished_ms"].is_null())
                .cloned();
            if let Some(t) = ended {
                return t;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the step ended");
    assert_eq!(task["status"], "done", "{task}");
    assert_eq!(task["workiq"], true, "{task}");
    // Not workiq:sales:1:sales:1, as if its agent id were a folder.
    assert_eq!(task["target"], "workiq:sales:1", "{task}");
    let answer = task["output"].as_str().unwrap();
    assert!(answer.contains("the launch moved to May"), "{answer}");

    let id = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let runs = call("GET", "/api/runs", Value::Null).await.json();
            if let Some(id) = runs["runs"][0]["id"].as_str() {
                return id.to_owned();
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the run was saved");
    let path = format!("/api/runs/{id}");
    let run = call("GET", &path, Value::Null).await.json();
    let report = run["report"].as_str().unwrap();
    assert!(
        report.contains("\n### ask (Work IQ) on workiq:sales:1: done\n"),
        "{report}"
    );
    assert!(
        report.contains(
            "\n- No evidence: an answer from Microsoft 365 Copilot is context, never proof\n"
        ),
        "{report}"
    );
    assert!(
        !report.contains("copilot →") && !report.contains("older version"),
        "{report}"
    );
    // An answer without citations has no label information.
    assert_eq!(
        run["workiq"],
        serde_json::json!([{
            "step": "ask",
            "sources": [],
            "labels": ["unknown"],
            "kept": 1,
            "withheld": 0,
        }])
    );
    let app = call("GET", "/app.js", Value::Null).await.body;
    for wording in ["workiqWho(t.target)", "m365Label(s.label)"] {
        assert!(app.contains(wording), "the page doesn't use {wording}");
    }
    agent.shutdown().await;
}

/// A digital worker's page shows the Work IQ account it uses, its owner's, and where its
/// sign-off notices go, as its owner set them; it leaves signing in, signing out and budgets
/// to the owner; a worker its owner didn't let use the account says why.
#[tokio::test]
async fn a_digital_worker_shows_its_owners_workiq_account_and_leaves_changes_to_the_owner() {
    let home = tempfile::tempdir().unwrap();
    let mut owner = Profile::open_in(home.path(), "owner").unwrap();
    owner.config.workiq = Some(proofkin::profile::WorkIqAccount {
        connection: proofkin::profile::WorkIqConnection::Cli {
            account: Some("ada@contoso.example".into()),
            program: None,
        },
        upn: "ada@contoso.example".into(),
        display_name: "ada@contoso.example".into(),
        tenant_id: "unknown".into(),
        added: 0,
    });
    owner.config.workiq_notices = Some(proofkin::profile::WorkIqNotices::Mail);
    owner.save().unwrap();
    let mut worker = Profile::open_in(home.path(), "helper").unwrap();
    worker.config.owner = Some("owner".into());
    worker.config.workiq_from_owner = true;
    worker.save().unwrap();
    let (agent, port, token) = ui_of(home.path(), "helper").await;
    let call = |method: &'static str, path: &'static str, body: Value| {
        let token = token.clone();
        async move { call_ui(port, Some(&token), method, path, body).await }
    };

    let shown = call("GET", "/api/workiq", Value::Null).await.json();
    assert_eq!(shown["signed_in"], true, "{shown}");
    assert_eq!(shown["line"], "ada@contoso.example through the Work IQ CLI");
    assert_eq!(shown["holder"], "owner");
    assert_eq!(shown["own"], false);
    assert!(shown["not_used"].is_null(), "{shown}");
    assert_eq!(shown["hint"], "proofkin auth login workiq --profile owner");
    assert_eq!(shown["notices_line"], "by mail to ada@contoso.example");
    assert_eq!(
        shown["notices"],
        serde_json::json!({ "to": "mail", "address": "ada@contoso.example" })
    );

    let sign_in = serde_json::json!({ "account": "bob@contoso.example" });
    let budget = serde_json::json!({ "per_day": 5 });
    for (method, path, body, instead) in [
        ("POST", "/api/workiq/sign-in", sign_in, "sign in there"),
        ("DELETE", "/api/workiq", Value::Null, "sign out there"),
        ("PUT", "/api/workiq/budget", budget, "set its budget there"),
    ] {
        let refused = call(method, path, body).await;
        assert_eq!(refused.status, 400, "{method} {path}: {}", refused.body);
        assert_eq!(
            refused.json()["error"],
            format!("helper uses owner's Work IQ account; {instead}")
        );
    }
    // Nothing changed: the owner is still signed in, and the worker has no budgets of its own.
    let owner = Profile::open_in(home.path(), "owner").unwrap();
    assert!(owner.config.workiq.is_some());
    let mut helper = Profile::open_in(home.path(), "helper").unwrap();
    assert!(helper.config.workiq_budget.is_none());

    helper.config.workiq_from_owner = false;
    helper.save().unwrap();
    let not_used = call("GET", "/api/workiq", Value::Null).await.json();
    assert_eq!(not_used["signed_in"], false, "{not_used}");
    let why = not_used["not_used"].as_str().unwrap_or_default();
    assert!(
        why.contains("may not use 'owner''s Work IQ account"),
        "{not_used}"
    );
    assert!(not_used["notices"].is_null(), "{not_used}");
    agent.shutdown().await;
}

/// The page has the Work IQ card after the GitHub card, every element app.js looks up for
/// it, and the card's two dialogs (sign in, budget), which close with Escape and Cancel like
/// the others; the card loads from and acts through the Work IQ API.
#[tokio::test]
async fn the_page_has_the_workiq_card_after_the_github_card_and_guards_its_dialogs() {
    let home = tempfile::tempdir().unwrap();
    let (agent, port, _token) = solo_ui(home.path()).await;
    let host = format!("127.0.0.1:{port}");
    let page = request(port, "GET", "/", &host, None, None).await.body;
    let at = |needle: &str| page.find(needle).expect(needle);
    let card = at(r#"<section class="card" aria-labelledby="workiq-h">"#);
    assert!(at(r#"aria-labelledby="github-h""#) < card);
    assert!(card < at(r#"aria-labelledby="tracker-h""#));
    let section = &page[card..card + page[card..].find("</section>").unwrap()];
    assert!(section.contains(">Work IQ</h2>"), "{section}");
    assert!(section.contains(r#"id="workiq""#), "{section}");
    let app = request(port, "GET", "/app.js", &host, None, None)
        .await
        .body;
    let guarded = regex::Regex::new(r"for \(const id of \[([^\]]+)\]\) guardDialog")
        .unwrap()
        .captures(&app)
        .expect("app.js guards its dialogs")[1]
        .to_owned();
    for (dialog, cancel) in [
        ("workiq-dialog", "workiq-cancel"),
        ("workiq-budget-dialog", "workiq-budget-cancel"),
    ] {
        assert!(page.contains(&format!(r#"<dialog id="{dialog}""#)));
        assert!(
            guarded.contains(&format!("\"#{dialog}\"")),
            "{dialog} isn't guarded"
        );
        let closes =
            format!(r##"$("#{cancel}").addEventListener("click", () => $("#{dialog}").close());"##);
        assert!(app.contains(&closes), "Cancel doesn't close {dialog}");
    }
    // An element app.js looks up that isn't on the page stops it starting.
    let ids = regex::Regex::new(r##""#(workiq[\w-]*)""##)
        .unwrap()
        .captures_iter(&app)
        .map(|c| c[1].to_owned())
        .collect::<std::collections::BTreeSet<_>>();
    assert!(ids.len() >= 15, "{ids:?}");
    for id in &ids {
        assert!(
            page.contains(&format!(r#"id="{id}""#)),
            "#{id} isn't on the page"
        );
    }
    for path in [
        "/api/workiq",
        "/api/workiq/sign-in",
        "/api/workiq/doctor",
        "/api/workiq/budget",
    ] {
        assert!(app.contains(&format!("\"{path}\"")), "{path}");
    }
    agent.shutdown().await;
}

/// Calls the web UI, which must answer 200, then reads the Microsoft 365 grant of the
/// allowlist's first peer as the page shows it (checking that `peers.json` keeps the same),
/// and the activity's last line.
async fn m365_after(
    port: u16,
    token: &str,
    peers_file: &std::path::Path,
    method: &str,
    path: &str,
    body: Value,
) -> (Value, String) {
    let res = call_ui(port, Some(token), method, path, body).await;
    assert_eq!(res.status, 200, "{method} {path}: {}", res.body);
    let state = call_ui(port, Some(token), "GET", "/api/state", Value::Null)
        .await
        .json();
    let shown = state["trusted"][0]["m365"].clone();
    let text = std::fs::read_to_string(peers_file).unwrap();
    let saved: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(saved["peers"][0]["m365"], shown, "peers.json: {saved}");
    let said = state["feed"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|i| i["kind"] == "system")
        .and_then(|i| i["text"].as_str())
        .unwrap_or_default()
        .to_owned();
    (shown, said)
}

/// The allowlist sets, changes and removes a peer's Microsoft 365 grant as `peers allow
/// --m365 <labels> [--m365-tools]` does, and the page, `peers.json` and the activity say
/// it: adding or changing the peer without `m365` keeps its grant, another grant replaces
/// it (each label once, whatever its case), and null removes it.
#[tokio::test]
async fn the_allowlist_sets_changes_and_removes_a_peers_microsoft_365_grant() {
    let home = tempfile::tempdir().unwrap();
    let (agent, port, token) = ui_of(home.path(), "boss").await;
    let profile = Profile::open_in(home.path(), "boss").unwrap();
    let file = profile.trust_path();
    let after = |method: &'static str, path: &str, body: Value| {
        let (token, file, path) = (token.clone(), file.clone(), path.to_owned());
        async move { m365_after(port, &token, &file, method, &path, body).await }
    };
    let peer = SecretKey::generate().public();
    let change = format!("/api/trust/{peer}");
    // Adding pal, with `m365` when given (null too).
    let add = |m365: Option<Value>| {
        let mut body = serde_json::json!({ "ticket": peer.to_string(), "name": "pal" });
        if let Some(m365) = m365 {
            body["m365"] = m365;
        }
        body
    };
    let chat = "Changed what pal may do: chat only (it may not delegate tasks)";

    // Added with a grant: the page shows it, peers.json keeps it and the activity says it.
    let general = serde_json::json!({ "labels": ["General", "Public"], "tools": true });
    let words = "Microsoft 365: General, Public (and its tasks may use Work IQ)";
    let added = (general.clone(), format!("Allowlisted pal ({words})"));
    let first = after("POST", "/api/trust", add(Some(general.clone()))).await;
    assert_eq!(first, added);
    // Added again, or changed, without one: it keeps its grant.
    assert_eq!(after("POST", "/api/trust", add(None)).await, added);
    let kept = after("PUT", &change, serde_json::json!({ "delegate": false })).await;
    assert_eq!(kept, (general, format!("{chat}; {words}")));

    // Another grant replaces it, each label once whatever its case.
    let labels = ["Confidential", " confidential ", "unlabeled", ""];
    let body = serde_json::json!({ "m365": { "labels": labels } });
    let other = after("PUT", &change, body).await;
    let once = serde_json::json!({ "labels": ["Confidential", "unlabeled"], "tools": false });
    let said = format!("{chat}; Microsoft 365: Confidential, unlabeled");
    assert_eq!(other, (once, said));

    // Null removes it, from peers.json too; so does adding pal again with null.
    let removed = after("PUT", &change, serde_json::json!({ "m365": null })).await;
    assert_eq!(removed, (Value::Null, chat.to_owned()));
    assert!(!std::fs::read_to_string(&file).unwrap().contains("m365"));
    let public = serde_json::json!({ "labels": ["Public"] });
    let again = after("POST", "/api/trust", add(Some(public))).await;
    let granted = serde_json::json!({ "labels": ["Public"], "tools": false });
    let said = "Allowlisted pal (Microsoft 365: Public)".to_owned();
    assert_eq!(again, (granted, said));
    let off = after("POST", "/api/trust", add(Some(Value::Null))).await;
    assert_eq!(off, (Value::Null, "Allowlisted pal".to_owned()));
    agent.shutdown().await;
}

/// The allowlist holds a Microsoft 365 grant to the CLI's rules but says what is wrong in the
/// dialog's words, without the CLI's flags: it takes label names only, so `unknown` (content
/// with no label information), `off` (which isn't read as `--m365 off`, alone or among other
/// labels, with Work IQ tools or without) and a grant without labels are refused with a 400,
/// whether adding or changing the peer, which keeps the grant it had, in `peers.json` too.
#[tokio::test]
async fn microsoft_365_grants_with_unknown_or_without_labels_are_refused() {
    let home = tempfile::tempdir().unwrap();
    let (agent, port, token) = ui_of(home.path(), "boss").await;
    let call = |method: &'static str, path: &str, body: Value| {
        let (token, path) = (token.clone(), path.to_owned());
        async move { call_ui(port, Some(&token), method, &path, body).await }
    };
    let peer = SecretKey::generate().public();
    let (ticket, change) = (peer.to_string(), format!("/api/trust/{peer}"));
    let grant = serde_json::json!({ "labels": ["Public"], "tools": false });
    let body = serde_json::json!({ "ticket": ticket, "name": "pal", "m365": grant });
    let added = call("POST", "/api/trust", body).await;
    assert_eq!(added.status, 200, "{}", added.body);
    let profile = Profile::open_in(home.path(), "boss").unwrap();
    let before = std::fs::read_to_string(profile.trust_path()).unwrap();
    let unknown = |label: &str| {
        format!(
            "The labels can't include '{label}': it stands for content with no label \
             information, whose sensitivity isn't known, so no peer gets it"
        )
    };
    let off = |label: &str| {
        format!(
            "The labels can't include '{label}': it isn't a label name; to give no \
             Microsoft 365 content, remove the grant"
        )
    };
    let none = "Give at least one label name, e.g. General, Public, unlabeled";
    let cases: [(&[&str], bool, String); 7] = [
        (&["General", "unknown"], true, unknown("unknown")),
        (&["Unknown"], false, unknown("Unknown")),
        (&["off"], false, off("off")),
        (&[" Off "], true, off("Off")),
        (&["General", "OFF"], false, off("OFF")),
        (&[], true, none.to_owned()),
        (&[" ", ""], false, none.to_owned()),
    ];
    for (labels, tools, words) in cases {
        let m365 = serde_json::json!({ "labels": labels, "tools": tools });
        let add_body = serde_json::json!({ "ticket": ticket, "name": "pal", "m365": m365 });
        let change_body = serde_json::json!({ "m365": m365 });
        for (method, path, body) in [
            ("POST", "/api/trust", add_body),
            ("PUT", change.as_str(), change_body),
        ] {
            let refused = call(method, path, body).await;
            assert_eq!(refused.status, 400, "{labels:?}: {}", refused.body);
            assert_eq!(refused.json()["error"], words, "{method} {labels:?}");
        }
    }
    let state = call("GET", "/api/state", Value::Null).await.json();
    assert_eq!(state["trusted"][0]["m365"], grant);
    let after = std::fs::read_to_string(profile.trust_path()).unwrap();
    assert_eq!(after, before);
    agent.shutdown().await;
}

/// The allowlist dialog offers a peer's Microsoft 365 grant after its folders: receiving
/// Microsoft 365 content, the sensitivity labels it may receive (what `unlabeled` means, and
/// that content without label information never goes to a peer) and Work IQ tools; each of
/// those fields app.js looks up is there.
#[tokio::test]
async fn the_allowlist_dialog_offers_the_microsoft_365_grant_after_the_folders() {
    let home = tempfile::tempdir().unwrap();
    let (agent, port, _token) = solo_ui(home.path()).await;
    let host = format!("127.0.0.1:{port}");
    let page = request(port, "GET", "/", &host, None, None).await.body;
    let at = |needle: &str| page.find(needle).expect(needle);
    let folders = at(r#"id="shares-field""#);
    let grant = at(r#"<fieldset class="field m365-grant" id="m365-field">"#);
    let error = at(r#"id="add-error""#);
    assert!(at(r#"<dialog id="add-dialog""#) < folders);
    assert!(folders < grant && grant < error);
    let fieldset = &page[grant..grant + page[grant..].find("</fieldset>").unwrap()];
    for words in [
        "Let this peer receive Microsoft 365 content (Work IQ)",
        "<span>Sensitivity labels it may receive</span>",
        r#"placeholder="General, Public, unlabeled…""#,
        r#"<span class="mono">unlabeled</span> allows content with no label"#,
        "content without label information never goes to a peer",
        "Its tasks may also use this agent's Work IQ (read only)",
    ] {
        assert!(fieldset.contains(words), "{words}: {fieldset}");
    }
    // An element app.js looks up that isn't on the page stops it starting.
    let app = request(port, "GET", "/app.js", &host, None, None)
        .await
        .body;
    let ids = regex::Regex::new(r##""#((?:add-)?m365[\w-]*)""##)
        .unwrap()
        .captures_iter(&app)
        .map(|c| c[1].to_owned())
        .collect::<std::collections::BTreeSet<_>>();
    assert!(ids.len() >= 4, "{ids:?}");
    for id in &ids {
        assert!(
            fieldset.contains(&format!(r#"id="{id}""#)),
            "#{id} isn't in the grant's fields"
        );
    }
    agent.shutdown().await;
}

/// The agent factory's API (docs/agent-factory/spec/openapi.json): each operation answers
/// with the status codes its contract lists and a body that fits its schema, and nothing
/// works without the web UI's token (TM-11).
#[tokio::test]
async fn the_factory_api_answers_as_its_contract_says() {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join("solo");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("profile.json"),
        r#"{"codex": {"source": "login"}}"#,
    )
    .unwrap();
    let mut config = AgentConfig::new("solo", SecretKey::generate());
    config.network = Network::Local;
    config.bind = Some("127.0.0.1:0".parse().unwrap());
    let (agent, events) = Agent::bind(config).await.unwrap();
    let info = UiInfo {
        profile: Profile::open_in(home.path(), "solo").unwrap(),
        network: Network::Local,
        serving: false,
        access: Access::ReadOnly,
        workspace: home.path().to_path_buf(),
        providers: Vec::new(),
        accounts: Vec::new(),
        workers: None,
    };
    let server = ui::start(
        agent.clone(),
        events,
        info,
        None,
        "127.0.0.1:0".parse().unwrap(),
    )
    .await
    .unwrap();
    let rest = server.url.strip_prefix("http://127.0.0.1:").unwrap();
    let (port, token) = rest.split_once("/#t=").unwrap();
    let port: u16 = port.parse().unwrap();
    let host = format!("127.0.0.1:{port}");
    let contract: Value = serde_json::from_str(
        &std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("docs/agent-factory/spec/openapi.json"),
        )
        .unwrap(),
    )
    .unwrap();
    // Calls `method path` with `body`; asserts its status is `code`, one its contract
    // (operation `op` at `template`) lists, with a body that fits that response's schema.
    let call = |method: &'static str,
                path: String,
                template: &'static str,
                body: Option<Value>,
                code: u16| {
        let (host, contract) = (host.clone(), contract.clone());
        async move {
            let sent = body.map(|b| b.to_string());
            let got = request(port, method, &path, &host, Some(token), sent.as_deref()).await;
            assert_eq!(got.status, code, "{method} {path}: {}", got.body);
            let value = got.json();
            let response = &contract["paths"][template][method.to_ascii_lowercase()]["responses"]
                [code.to_string()];
            assert!(!response.is_null(), "{method} {template} lists no {code}");
            let response = match response["$ref"].as_str() {
                Some(r) => contract.pointer(r.strip_prefix('#').unwrap()).unwrap(),
                None => response,
            };
            let schema = &response["content"]["application/json"]["schema"];
            let wrong = proofkin::spec::contract_mismatches(&value, schema, &contract);
            assert!(
                wrong.is_empty(),
                "{method} {path} {code}: {wrong:?}\n{value:#}"
            );
            value
        }
    };

    // Nothing without the web UI's token.
    for (method, path) in [
        ("GET", "/api/factory/templates"),
        ("POST", "/api/factory/drafts"),
        ("GET", "/api/factory/drafts/x"),
        ("GET", "/api/factory/agents"),
        ("POST", "/api/factory/agents/x/evaluations"),
        ("GET", "/api/factory/evaluations/ev-20261124-091500-x"),
        ("DELETE", "/api/factory/evaluations/ev-20261124-091500-x"),
    ] {
        let got = request(port, method, path, &host, None, Some("{}")).await;
        assert_eq!(got.status, 401, "{method} {path}");
    }

    let templates = call(
        "GET",
        "/api/factory/templates".into(),
        "/api/factory/templates",
        None,
        200,
    )
    .await;
    assert!(
        templates["templates"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["name"] == "researcher")
    );
    let implementer = templates["templates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "implementer")
        .unwrap();
    assert_eq!(
        implementer["needs"],
        "needs a deploy folder whose test check runs node --test"
    );

    // Drafts: from a template (no model), refused when they can't be one.
    let drafts = "/api/factory/drafts";
    let draft = call(
        "POST",
        drafts.into(),
        drafts,
        Some(serde_json::json!({ "template": "researcher", "name": "ask" })),
        201,
    )
    .await;
    assert_eq!(draft["provenance"]["source"], "template");
    assert_eq!(
        draft["blueprint"]["model"]["providers"],
        serde_json::json!(["codex"])
    );
    assert_eq!(draft["problems"], serde_json::json!([]));
    // Its cases' files, from the template, are in its fixtures folder.
    let fixtures = dir.join("factory/agents/ask/fixtures");
    for file in ["README.md", "src/main.py", "config.ini"] {
        assert!(fixtures.join(file).is_file(), "{file}");
    }
    let taken = call(
        "POST",
        drafts.into(),
        drafts,
        Some(serde_json::json!({ "template": "researcher", "name": "ask" })),
        409,
    )
    .await;
    assert!(
        taken["error"]
            .as_str()
            .unwrap()
            .contains("'ask' is already"),
        "{taken}"
    );
    for (body, why) in [
        (
            serde_json::json!({ "name": "x", "description": "   " }),
            "the description is empty",
        ),
        (
            serde_json::json!({ "name": "x", "template": "nope" }),
            "there is no template 'nope'",
        ),
        (
            serde_json::json!({ "name": "x" }),
            "describe what the agent should do, or start from a template",
        ),
        (
            serde_json::json!({ "name": "../x", "template": "researcher" }),
            "can't name an agent",
        ),
    ] {
        let refused = call("POST", drafts.into(), drafts, Some(body), 400).await;
        assert!(
            refused["error"].as_str().unwrap().contains(why),
            "{refused}"
        );
    }

    let one = "/api/factory/drafts/{name}";
    let got = call("GET", "/api/factory/drafts/ask".into(), one, None, 200).await;
    assert_eq!(got["blueprint"], draft["blueprint"]);
    call("GET", "/api/factory/drafts/nope".into(), one, None, 404).await;
    let mut blueprint = draft["blueprint"].clone();
    blueprint["title"] = "Asker".into();
    let saved = call(
        "PUT",
        "/api/factory/drafts/ask".into(),
        one,
        Some(blueprint.clone()),
        200,
    )
    .await;
    assert_eq!(
        (saved["passed"].clone(), saved["increases"].clone()),
        (true.into(), serde_json::json!([]))
    );
    let mut renamed = blueprint.clone();
    renamed["name"] = "other".into();
    call(
        "PUT",
        "/api/factory/drafts/ask".into(),
        one,
        Some(renamed),
        400,
    )
    .await;
    call(
        "PUT",
        "/api/factory/drafts/ask".into(),
        one,
        Some(serde_json::json!({ "name": "ask" })),
        400,
    )
    .await;
    let checked = call(
        "POST",
        "/api/factory/drafts/ask/check".into(),
        "/api/factory/drafts/{name}/check",
        Some(serde_json::json!({ "folders": ["nowhere"] })),
        200,
    )
    .await;
    assert_eq!(checked["passed"], false);
    assert!(
        checked["problems"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["code"] == "unknown_folder"),
        "{checked}"
    );

    let agents = call(
        "GET",
        "/api/factory/agents".into(),
        "/api/factory/agents",
        None,
        200,
    )
    .await;
    assert_eq!(agents["agents"][0]["name"], "ask");
    assert_eq!(agents["agents"][0]["title"], "Asker");
    let shown = call(
        "GET",
        "/api/factory/agents/ask".into(),
        "/api/factory/agents/{name}",
        None,
        200,
    )
    .await;
    assert_eq!(shown["state"], "draft");
    assert_eq!(shown["candidates"][0]["provider"], "codex");
    call(
        "GET",
        "/api/factory/agents/nope".into(),
        "/api/factory/agents/{name}",
        None,
        404,
    )
    .await;

    // An evaluation: its estimate first, and nothing runs.
    let evaluations = "/api/factory/agents/{name}/evaluations";
    let estimate = call(
        "POST",
        "/api/factory/agents/ask/evaluations".into(),
        evaluations,
        Some(serde_json::json!({ "confirm": false })),
        200,
    )
    .await;
    assert_eq!(
        (
            estimate["cases"].clone(),
            estimate["repetitions"].clone(),
            estimate["candidates"].clone(),
            estimate["runs"].clone()
        ),
        (10.into(), 3.into(), 1.into(), 30.into())
    );
    let fewer = call("POST", "/api/factory/agents/ask/evaluations".into(), evaluations,
        Some(serde_json::json!({ "confirm": false, "repetitions": 1, "candidates": [{ "provider": "codex", "model": "gpt-5", "effort": "high" }] })), 200).await;
    assert_eq!(
        (
            fewer["runs"].clone(),
            fewer["candidate_routes"][0]["model"].clone()
        ),
        (10.into(), "gpt-5".into())
    );
    assert!(
        !home.path().join("solo-eval").exists(),
        "nothing ran: no evaluation worker"
    );
    call(
        "POST",
        "/api/factory/agents/ask/evaluations".into(),
        evaluations,
        Some(serde_json::json!({ "confirm": false, "peer": "someone" })),
        400,
    )
    .await;
    call(
        "POST",
        "/api/factory/agents/nope/evaluations".into(),
        evaluations,
        Some(serde_json::json!({ "confirm": false })),
        404,
    )
    .await;
    // Without a way to run its evaluation worker, it can't be confirmed.
    let refused = call(
        "POST",
        "/api/factory/agents/ask/evaluations".into(),
        evaluations,
        Some(serde_json::json!({ "confirm": true })),
        409,
    )
    .await;
    assert!(
        refused["error"]
            .as_str()
            .unwrap()
            .contains("can't run your evaluation worker"),
        "{refused}"
    );
    // A suite that doesn't prove R2 is the request's problem; other errors the draft's.
    let mut unproven = blueprint.clone();
    unproven["evals"]["cases"]
        .as_array_mut()
        .unwrap()
        .retain(|c| c["requirement"] != "R2");
    call(
        "PUT",
        "/api/factory/drafts/ask".into(),
        one,
        Some(unproven),
        200,
    )
    .await;
    let invalid = call(
        "POST",
        "/api/factory/agents/ask/evaluations".into(),
        evaluations,
        Some(serde_json::json!({ "confirm": false })),
        400,
    )
    .await;
    assert!(
        invalid["error"].as_str().unwrap().contains("R2"),
        "{invalid}"
    );
    let mut broken = blueprint.clone();
    broken["skills"] = serde_json::json!(["no-such-skill"]);
    call(
        "PUT",
        "/api/factory/drafts/ask".into(),
        one,
        Some(broken),
        200,
    )
    .await;
    let errors = call(
        "POST",
        "/api/factory/agents/ask/evaluations".into(),
        evaluations,
        Some(serde_json::json!({ "confirm": false })),
        409,
    )
    .await;
    assert_eq!(
        errors["error"],
        "the draft has 1 error: fix it, then evaluate"
    );

    let id = "/api/factory/evaluations/{id}";
    call(
        "GET",
        "/api/factory/evaluations/ev-20261124-091500-ask".into(),
        id,
        None,
        404,
    )
    .await;
    call(
        "GET",
        "/api/factory/evaluations/not-an-id".into(),
        id,
        None,
        404,
    )
    .await;
    call(
        "DELETE",
        "/api/factory/evaluations/ev-20261124-091500-ask".into(),
        id,
        None,
        404,
    )
    .await;
    agent.shutdown().await;
}
