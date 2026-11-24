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
    let mut config = AgentConfig::new("solo", SecretKey::generate());
    config.network = Network::Local;
    config.bind = Some("127.0.0.1:0".parse().unwrap());
    let (agent, events) = Agent::bind(config).await.unwrap();
    let info = UiInfo {
        profile: Profile::open_in(home, "solo").unwrap(),
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
        ops["open_incidents"], 1,
        "one ongoing problem, however often it was opened"
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
            "rehearse"
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
    assert_eq!(ops["open_incidents"], 0);
    assert!(
        proofkin::ops::open_incidents(&store.incidents(0)).is_empty(),
        "the removed monitor's incidents are closed"
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
    assert!(page.contains(r#"<script src="/plan-text.js" defer>"#));
    assert!(page.contains(r#"<script src="/plan-builder.js" defer>"#));
    assert!(page.contains(r#"<script src="/workers.js" defer>"#));
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
        "/tools.js",
        "/plan-text.js",
        "/plan-builder.js",
        "/workers.js",
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
/// say, and the builder's model writes back the plan it read. The cases
/// (tests/plan_text.test.cjs) run in node, which loads the script as the page does.
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
        Some(Arc::new(SearchResults(results))),
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
