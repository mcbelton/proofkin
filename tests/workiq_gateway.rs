//! The Work IQ gateway (src/workiq/gateway.rs): a task's model calls Work IQ through an
//! address of its own on this machine. Against a fake hosted Work IQ and Entra, and end to end
//! with a digital worker whose owner's task asks for its folder's `workiq` server, through the
//! fake Work IQ CLI (tests/fixtures/workiq_cli_mcp.py). Its own test binary because the tests
//! use the file secret store, chosen through the environment.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Once};

use async_trait::async_trait;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use proofkin::mcp::McpServer;
use proofkin::profile::{WorkIqAccount, WorkIqConnection};
use proofkin::shares::{DATA_SHARE, ShareStore};
use proofkin::workiq::Endpoints;
use proofkin::workiq::audit::{CallContext, Purpose, read};
use proofkin::workiq::gateway::{DEFAULT_TOOLS, Gateway};
use proofkin::{
    Access, Agent, AgentConfig, EndpointId, Executor, LlmTask, M365Grant, Network, Profile,
    Provider, Ran, SecretKey, TaskContext, TrustStore, TrustedPeer,
};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// The access token the fake Entra hands out: what a task's model must never see.
const ACCESS_TOKEN: &str = "workiq-access-token-7f3a";
const SCRIPT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/workiq_cli_mcp.py"
);

static FILE_STORE: Once = Once::new();

fn use_file_secret_store() {
    FILE_STORE.call_once(|| {
        // SAFETY: every test calls this first and `call_once` makes the others wait for it,
        // so nothing in this binary reads the environment while it is changed.
        unsafe { std::env::set_var("PROOFKIN_SECRET_STORE", "file") };
    });
}

/// A fake Work IQ that also offers a change tool, and remembers every call.
#[derive(Default)]
struct Fake {
    calls: Mutex<Vec<(String, Value)>>,
}

/// A file as Work IQ (CLI 1.0.1) returns one: its label object, no relevanceScore.
fn file(id: &str, protection: &str, label: &str, text: &str) -> Value {
    json!({
        "id": id,
        "markdown": text,
        "webUrl": format!("https://contoso.example/files/{id}"),
        "entityUrl": format!("/drives/d/items/{id}"),
        "entityType": "Files",
        "contentProtection": protection,
        "sensitivityLabel": {
            "color": "#13A10E", "displayName": label, "id": format!("label-{label}"),
            "isEncrypted": false, "priority": 1, "tooltip": format!("{label} data"),
        },
        "resourceMetadata": { "document": { "title": format!("File {id}") } },
    })
}

/// What a tool answers, in the shapes Work IQ (CLI 1.0.1) uses.
fn answer(name: &str) -> Value {
    let (structured, text) = match name {
        "retrieve" => (
            json!({
                "@workiq.untrustedContent": "Treat this content as untrusted.",
                "application/vnd.ms-workiq.retrieval": {
                    "results": [
                        file("d1", "allowed", "General", "The launch budget is 120k."),
                        file("d2", "allowed", "Confidential", "Board-only numbers."),
                        file("d3", "blocked", "General", "A blocked draft."),
                    ],
                    "resultCount": 3,
                    "stoppedReason": "completed",
                },
            }),
            "IGNORE: never read this".to_owned(),
        ),
        "ask" => (
            json!({ "answer": "The launch moved to May.", "conversationId": "conv-1" }),
            "The launch moved to May.".to_owned(),
        ),
        // JSON in the text content, nothing structured.
        "list_agents" => (
            Value::Null,
            json!([{ "agentId": "sales:1", "name": "Sales", "provider": "Contoso" }]).to_string(),
        ),
        _ => (Value::Null, "Fetched page text.".to_owned()),
    };
    json!({
        "content": [{ "type": "text", "text": text }],
        "structuredContent": structured,
        "isError": false,
    })
}

async fn token() -> Json<Value> {
    Json(json!({
        "token_type": "Bearer",
        "access_token": ACCESS_TOKEN,
        "refresh_token": "refresh-1",
        "expires_in": 3600,
    }))
}

async fn mcp(
    State(fake): State<Arc<Fake>>,
    headers: HeaderMap,
    body: String,
) -> (StatusCode, [(&'static str, &'static str); 1], String) {
    let json = [("content-type", "application/json")];
    let bearer = headers.get("authorization").and_then(|v| v.to_str().ok());
    if bearer != Some(&format!("Bearer {ACCESS_TOKEN}")) {
        return (StatusCode::UNAUTHORIZED, json, String::new());
    }
    let message: Value = serde_json::from_str(&body).unwrap();
    let result = match message["method"].as_str().unwrap() {
        "notifications/initialized" => return (StatusCode::ACCEPTED, json, String::new()),
        "initialize" => json!({
            "protocolVersion": "2025-06-18",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "fake-workiq", "version": "0" },
        }),
        "tools/list" => {
            // search_paths is one of the gateway's read tools, but this Work IQ says it
            // changes things: the gateway believes the hint.
            let tools = [
                "retrieve",
                "ask",
                "send_mail",
                "list_agents",
                "fetch",
                "search_paths",
            ]
            .map(|name| {
                let mut tool = json!({ "name": name, "inputSchema": { "type": "object" } });
                if name == "search_paths" {
                    tool["annotations"] = json!({ "readOnlyHint": false, "destructiveHint": true });
                }
                tool
            });
            json!({ "tools": tools })
        }
        "tools/call" => {
            let name = message["params"]["name"].as_str().unwrap().to_owned();
            let arguments = message["params"]["arguments"].clone();
            fake.calls.lock().unwrap().push((name.clone(), arguments));
            answer(&name)
        }
        other => panic!("unexpected method {other}"),
    };
    let reply = json!({ "jsonrpc": "2.0", "id": message["id"], "result": result });
    (StatusCode::OK, json, reply.to_string())
}

async fn start() -> (Arc<Fake>, Endpoints) {
    let fake = Arc::new(Fake::default());
    let app = Router::new()
        .route("/{tenant}/oauth2/v2.0/token", post(token))
        .route("/mcp", post(mcp))
        .with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let endpoints = Endpoints {
        mcp_url: format!("{base}/mcp"),
        authority: base.clone(),
        discovery_url: format!("{base}/discovery"),
    };
    (fake, endpoints)
}

/// A profile signed in to the fake hosted Work IQ.
fn signed_in(root: &Path) -> Profile {
    use_file_secret_store();
    let mut profile = Profile::open_in(root, "alice").unwrap();
    profile.config.workiq = Some(WorkIqAccount {
        connection: WorkIqConnection::Hosted {
            tenant: "contoso".into(),
            client_id: "app-id-1".into(),
        },
        upn: "alice@contoso.example".into(),
        display_name: "Alice Example".into(),
        tenant_id: "contoso".into(),
        added: 1_700_000_000,
    });
    profile.set_workiq_refresh_token("refresh-0").unwrap();
    profile.save().unwrap();
    profile
}

fn gateway_call() -> CallContext {
    CallContext::local(Purpose::Gateway)
}

/// A JSON-RPC request.
fn rpc(id: u64, method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

fn call(id: u64, tool: &str, arguments: Value) -> Value {
    rpc(
        id,
        "tools/call",
        json!({ "name": tool, "arguments": arguments }),
    )
}

/// What a model's MCP client sends to `server` (the one its task got), over plain HTTP:
/// the status and the JSON answer.
async fn send(
    server: &McpServer,
    bearer: Option<&str>,
    origin: bool,
    body: &Value,
) -> (u16, Value) {
    let url = server.url.as_deref().unwrap();
    let rest = url.strip_prefix("http://").unwrap();
    let (host, path) = rest.split_once('/').unwrap();
    let body = body.to_string();
    let mut head = format!(
        "POST /{path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nContent-Length: {}\r\n",
        body.len()
    );
    if let Some(bearer) = bearer {
        head.push_str(&format!("Authorization: Bearer {bearer}\r\n"));
    }
    if origin {
        head.push_str("Origin: https://evil.example\r\n");
    }
    let mut stream = tokio::net::TcpStream::connect(host).await.unwrap();
    stream
        .write_all(format!("{head}\r\n{body}").as_bytes())
        .await
        .unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).await.unwrap();
    let (head, body) = raw.split_once("\r\n\r\n").unwrap();
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    (status, serde_json::from_str(body).unwrap_or(Value::Null))
}

/// The bearer the task's model was given for `server`.
fn bearer_for(server: &McpServer) -> String {
    server.headers["Authorization"]
        .strip_prefix("Bearer ")
        .unwrap()
        .to_owned()
}

fn text_of(reply: &Value) -> String {
    reply["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

#[tokio::test]
async fn a_tasks_session_offers_only_read_tools_and_its_model_never_gets_the_token() {
    let (fake, endpoints) = start().await;
    let root = tempfile::tempdir().unwrap();
    let gateway = Gateway::with_endpoints(signed_in(root.path()), endpoints);

    let (server, _session) = gateway
        .open(&McpServer::workiq(Vec::new()), gateway_call())
        .await
        .unwrap();
    let url = server.url.clone().unwrap();
    assert!(
        url.starts_with("http://127.0.0.1:") && url.contains("/mcp/workiq/"),
        "{url}"
    );
    assert_eq!(server.tools, DEFAULT_TOOLS.map(String::from));
    let config = server.to_json().to_string();
    assert!(
        !config.contains(ACCESS_TOKEN) && !config.contains("refresh-"),
        "{config}"
    );

    let bearer = bearer_for(&server);
    let (_, hello) = send(
        &server,
        Some(&bearer),
        false,
        &rpc(1, "initialize", json!({})),
    )
    .await;
    assert_eq!(hello["result"]["serverInfo"]["name"], "proofkin-workiq");
    let (_, listed) = send(
        &server,
        Some(&bearer),
        false,
        &rpc(2, "tools/list", json!({})),
    )
    .await;
    let names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        ["retrieve", "ask", "list_agents", "fetch"],
        "send_mail changes things, and Work IQ says search_paths does"
    );

    let (_, sent) = send(
        &server,
        Some(&bearer),
        false,
        &call(3, "send_mail", json!({})),
    )
    .await;
    assert_eq!(sent["result"]["isError"], true);
    assert!(
        text_of(&sent).contains("isn't offered to this task"),
        "{sent}"
    );
    let (_, hinted) = send(
        &server,
        Some(&bearer),
        false,
        &call(4, "search_paths", json!({ "query": "x" })),
    )
    .await;
    assert_eq!(hinted["result"]["isError"], true);
    assert!(
        text_of(&hinted).contains("says its search_paths changes things"),
        "{hinted}"
    );
    assert!(
        fake.calls.lock().unwrap().is_empty(),
        "it never reached Work IQ"
    );

    // A server may offer fewer, and a change tool can't be one of them.
    let (fewer, _fewer_session) = gateway
        .open(&McpServer::workiq(vec!["ask".into()]), gateway_call())
        .await
        .unwrap();
    let (_, listed) = send(
        &fewer,
        Some(&bearer_for(&fewer)),
        false,
        &rpc(1, "tools/list", json!({})),
    )
    .await;
    assert_eq!(listed["result"]["tools"][0]["name"], "ask");
    assert_eq!(listed["result"]["tools"].as_array().unwrap().len(), 1);
    let err = McpServer::workiq(vec!["send_mail".into()])
        .validate("workiq")
        .unwrap_err();
    assert!(err.to_string().contains("only tools that read"), "{err}");
    let err = McpServer::workiq(Vec::new()).validate("m365").unwrap_err();
    assert!(err.to_string().contains("is called 'workiq'"), "{err}");
}

#[tokio::test]
async fn what_a_session_sees_goes_through_the_content_policy_for_its_audience_and_is_audited() {
    let (fake, endpoints) = start().await;
    let root = tempfile::tempdir().unwrap();
    let alice = signed_in(root.path());
    let gateway = Gateway::with_endpoints(alice.clone(), endpoints);

    let (own, _own_session) = gateway
        .open(&McpServer::workiq(Vec::new()), gateway_call())
        .await
        .unwrap();
    let query = call(1, "retrieve", json!({ "query": "launch budget" }));
    let (_, found) = send(&own, Some(&bearer_for(&own)), false, &query).await;
    let text = text_of(&found);
    assert!(
        text.contains("The launch budget is 120k.") && text.contains("Board-only numbers."),
        "{text}"
    );
    assert!(
        !text.contains("A blocked draft.") && !text.contains("never read this"),
        "{text}"
    );
    assert!(
        text.contains("<external_content source=\"workiq:Files:d1\">"),
        "{text}"
    );
    let (_, fetched) = send(
        &own,
        Some(&bearer_for(&own)),
        false,
        &call(2, "fetch", json!({})),
    )
    .await;
    assert!(text_of(&fetched).contains("<external_content source=\"workiq:fetch\">"));
    // The agents, which Work IQ lists as JSON text.
    let (_, agents) = send(
        &own,
        Some(&bearer_for(&own)),
        false,
        &call(4, "list_agents", json!({})),
    )
    .await;
    assert!(
        text_of(&agents).contains("- Sales (agent id sales:1)"),
        "{agents}"
    );

    // A peer's task (with the --m365-tools grant) sees only its labels, and only the tools
    // whose results carry them.
    let peer = CallContext::peer("bob", vec!["General".into()], Purpose::Gateway);
    let (theirs, _their_session) = gateway
        .open(&McpServer::workiq(Vec::new()), peer)
        .await
        .unwrap();
    assert_eq!(theirs.tools, ["retrieve", "ask"]);
    let (_, found) = send(&theirs, Some(&bearer_for(&theirs)), false, &query).await;
    let text = text_of(&found);
    assert!(text.contains("The launch budget is 120k."), "{text}");
    assert!(
        !text.contains("Board-only numbers."),
        "Confidential isn't granted: {text}"
    );
    let (_, refused) = send(
        &theirs,
        Some(&bearer_for(&theirs)),
        false,
        &call(3, "fetch", json!({})),
    )
    .await;
    assert_eq!(refused["result"]["isError"], true);

    let audited = read(&alice.dir.join("workiq-audit.jsonl")).unwrap();
    let tools: Vec<(&str, &str, &str)> = audited
        .iter()
        .map(|e| (e.tool.as_str(), e.purpose.as_str(), e.audience.as_str()))
        .collect();
    assert_eq!(
        tools,
        [
            ("retrieve", "gateway", "local"),
            ("fetch", "gateway", "local"),
            ("list_agents", "gateway", "local"),
            ("retrieve", "gateway", "bob"),
        ]
    );
    assert!(
        audited
            .iter()
            .all(|e| !serde_json::to_string(e).unwrap().contains("launch budget"))
    );
    assert_eq!(fake.calls.lock().unwrap().len(), 4);
}

#[tokio::test]
async fn a_session_needs_its_bearer_takes_no_browser_and_closes_with_its_task() {
    let (_fake, endpoints) = start().await;
    let root = tempfile::tempdir().unwrap();
    let gateway = Gateway::with_endpoints(signed_in(root.path()), endpoints);
    let (server, session) = gateway
        .open(&McpServer::workiq(Vec::new()), gateway_call())
        .await
        .unwrap();
    let list = rpc(1, "tools/list", json!({}));

    assert_eq!(send(&server, None, false, &list).await.0, 401);
    assert_eq!(
        send(&server, Some("not-the-bearer"), false, &list).await.0,
        401
    );
    assert_eq!(
        send(&server, Some(&bearer_for(&server)), true, &list)
            .await
            .0,
        403
    );
    assert_eq!(
        send(&server, Some(&bearer_for(&server)), false, &list)
            .await
            .0,
        200
    );
    assert_eq!(gateway.open_sessions(), 1);

    drop(session);
    assert_eq!(gateway.open_sessions(), 0);
    assert_eq!(
        send(&server, Some(&bearer_for(&server)), false, &list)
            .await
            .0,
        404
    );
}

fn python() -> &'static str {
    if cfg!(windows) { "python" } else { "python3" }
}

/// The fake Work IQ CLI in mode `full`, at a path of its own.
fn fake_cli_at(dir: &Path, state: &Path) -> PathBuf {
    let state = state.display();
    if cfg!(windows) {
        let path = dir.join("workiq.cmd");
        let text = format!("@{} \"{SCRIPT}\" \"{state}\" full %*\r\n", python());
        std::fs::write(&path, text).unwrap();
        path
    } else {
        let path = dir.join("workiq");
        let text = format!(
            "#!/bin/sh\nexec {} '{SCRIPT}' '{state}' full \"$@\"\n",
            python()
        );
        std::fs::write(&path, text).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }
}

/// A model that, while it works, uses its task's Work IQ server as an MCP client would.
#[derive(Default)]
struct Model {
    /// The server its task got.
    seen: Mutex<Option<McpServer>>,
}

#[async_trait]
impl Executor for Model {
    fn providers(&self) -> Vec<Provider> {
        vec![Provider::Copilot]
    }

    async fn execute(&self, _task: &LlmTask, ctx: &TaskContext) -> anyhow::Result<Ran> {
        let server = ctx
            .mcp
            .get("workiq")
            .cloned()
            .expect("the task's Work IQ server");
        *self.seen.lock().unwrap() = Some(server.clone());
        let bearer = bearer_for(&server);
        send(
            &server,
            Some(&bearer),
            false,
            &rpc(1, "initialize", json!({})),
        )
        .await;
        let (_, listed) = send(
            &server,
            Some(&bearer),
            false,
            &rpc(2, "tools/list", json!({})),
        )
        .await;
        let names: Vec<String> = listed["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_owned())
            .collect();
        let query = call(3, "retrieve", json!({ "query": "launch budget" }));
        let (_, found) = send(&server, Some(&bearer), false, &query).await;
        Ok(Ran::new(
            format!("tools: {}\n{}", names.join(", "), text_of(&found)),
            None,
        ))
    }
}

fn trusted(name: &str, id: EndpointId) -> TrustedPeer {
    TrustedPeer {
        name: name.into(),
        id,
        addrs: BTreeSet::new(),
        can_delegate: true,
        tasks_per_hour: 10,
        access: Access::ReadOnly,
        shares: vec![DATA_SHARE.to_owned(), "app".to_owned()],
        m365: None,
        github: None,
        agents: Vec::new(),
    }
}

async fn agent(name: &str, key: SecretKey, trust: TrustStore) -> Agent {
    let mut config = AgentConfig::new(name, key);
    config.network = Network::Local;
    config.bind = Some("127.0.0.1:0".parse().unwrap());
    config.trust = trust;
    let (agent, mut events) = Agent::bind(config).await.unwrap();
    tokio::spawn(async move { while events.recv().await.is_some() {} });
    agent
}

#[tokio::test(flavor = "multi_thread")]
async fn a_digital_workers_owner_gets_work_iq_in_its_task_and_a_peer_only_with_its_grant() {
    use_file_secret_store();
    let dir = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let cli = fake_cli_at(dir.path(), state.path());
    // The digital worker 'helper' of 'boss', through the Work IQ CLI (no token to keep).
    let mut profile = Profile::open_in(&dir.path().join("profiles"), "helper").unwrap();
    profile.config.workiq = Some(WorkIqAccount {
        connection: WorkIqConnection::Cli {
            account: None,
            program: Some(cli),
        },
        upn: "Work IQ CLI".into(),
        display_name: "Work IQ CLI".into(),
        tenant_id: "unknown".into(),
        added: 0,
    });
    profile.save().unwrap();
    let gateway = Gateway::new(profile);

    let (boss_key, partner_key) = (SecretKey::generate(), SecretKey::generate());
    let app = dir.path().join("app");
    std::fs::create_dir_all(&app).unwrap();
    let mut shares = ShareStore::with_data(dir.path().join("data"));
    shares.add("app", &app, Access::ReadOnly).unwrap();
    shares
        .add_mcp("app", "workiq", McpServer::workiq(Vec::new()))
        .unwrap();
    let mut trust = TrustStore::default();
    trust.allow(trusted("boss", boss_key.public()));
    trust.allow(trusted("partner", partner_key.public()));
    let model = Arc::new(Model::default());
    let mut config = AgentConfig::new("helper", SecretKey::generate());
    config.network = Network::Local;
    config.bind = Some("127.0.0.1:0".parse().unwrap());
    config.trust = trust;
    config.executor = Some(model.clone());
    config.shares = shares;
    config.owner = Some("boss".into());
    config.workiq_gateway = Some(gateway.clone());
    config.worktrees_dir = dir.path().join("worktrees");
    let (helper, mut events) = Agent::bind(config).await.unwrap();
    tokio::spawn(async move { while events.recv().await.is_some() {} });

    let mut trusts_helper = TrustStore::default();
    trusts_helper.allow(trusted("helper", helper.id()));
    let boss = agent("boss", boss_key, trusts_helper.clone()).await;
    let partner = agent("partner", partner_key, trusts_helper).await;
    for agent in [&boss, &partner] {
        agent.connect(helper.addr()).await.unwrap();
    }
    for _ in 0..500 {
        if [&boss, &partner].iter().all(|a| a.peers().len() == 1) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let mut task = LlmTask::new(Provider::Copilot, "find the launch budget");
    task.share = Some("app".into());
    task.mcp = vec!["workiq".into()];

    let out = boss.delegate(helper.id(), task.clone()).await.unwrap();
    assert!(
        out.starts_with("tools: retrieve, ask, list_agents\n"),
        "{out}"
    );
    assert!(out.contains("Launch budget notes from the CLI."), "{out}");
    assert!(
        !out.contains("may not hand out"),
        "blocked by contentProtection: {out}"
    );
    let server = model.seen.lock().unwrap().clone().unwrap();
    assert!(
        server
            .url
            .as_deref()
            .unwrap()
            .starts_with("http://127.0.0.1:")
    );
    assert_eq!(gateway.open_sessions(), 0, "closed when the task ended");
    let list = rpc(1, "tools/list", json!({}));
    assert_eq!(
        send(&server, Some(&bearer_for(&server)), false, &list)
            .await
            .0,
        404
    );

    let refused = partner
        .delegate(helper.id(), task.clone())
        .await
        .unwrap_err();
    assert!(
        refused
            .to_string()
            .contains("only to its owner's own tasks and to peers granted it")
            && refused.to_string().contains("--m365-tools"),
        "{refused:#}"
    );

    // Granted --m365-tools for Public content only, the partner's task gets the tools whose
    // results carry labels, and none of the General results.
    let mut granted = helper.trusted().get(&partner.id()).unwrap().clone();
    granted.m365 = Some(M365Grant {
        labels: vec!["Public".into()],
        tools: true,
    });
    helper.allow(granted);
    let out = partner.delegate(helper.id(), task).await.unwrap();
    assert!(out.starts_with("tools: retrieve, ask\n"), "{out}");
    assert!(
        !out.contains("Launch budget notes from the CLI."),
        "General isn't granted: {out}"
    );
    assert!(out.contains("Withheld from this context"), "{out}");
    let audited = read(&dir.path().join("profiles/helper/workiq-audit.jsonl")).unwrap();
    let audiences: Vec<&str> = audited.iter().map(|e| e.audience.as_str()).collect();
    assert_eq!(audiences, ["local", "partner"], "the peer is named");
    for agent in [&boss, &partner, &helper] {
        agent.shutdown().await;
    }
}
