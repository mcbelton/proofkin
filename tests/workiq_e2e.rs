//! The Work IQ commands through the program itself, as a user runs them: against a fake Work
//! IQ and a fake Entra that the test-only settings point it at, and against the fake Work IQ
//! CLI (tests/fixtures/workiq_cli_mcp.py) through `--cli-path`. A plan's `@workiq` and
//! `workiq=` steps run in the program's own agent, which sends the `workiq=` step to a peer
//! this test runs. Each run of the program gets its settings in its own environment; this
//! process's environment is never changed.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::extract::{Path as UrlPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use proofkin::shares::{DATA_SHARE, ShareStore};
use proofkin::workiq::audit::read;
use proofkin::workiq::doctor::PROBE_QUERY;
use proofkin::workiq::{TEST_ENTRA_URL, TEST_WORKIQ_URL};
use proofkin::{
    Access, Agent, AgentConfig, EndpointId, Executor, LlmTask, Network, Provider, Ran, SecretKey,
    TaskContext, Ticket, TrustStore, TrustedPeer,
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

const EXE: &str = env!("CARGO_BIN_EXE_proofkin");
const SCRIPT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/workiq_cli_mcp.py"
);
const TENANT_ID: &str = "11111111-2222-3333-4444-555555555555";
/// The spec document on SharePoint that `fetch_blob` gives: Markdown, labelled General.
const SPEC_DOC: &str = "# Albums\n\nFR-001: Users can share an album.\n";
/// How long a running agent has to print what a test waits for.
const WAIT: Duration = Duration::from_secs(120);
/// The Teams chat the signed-in account is a member of, in the fake's answers.
const CREW_CHAT: &str = "19:crew@thread.v2";

/// A fake Entra (device code and token endpoints) and Work IQ (discovery document and MCP
/// endpoint) on one loopback address.
#[derive(Default)]
struct Fake {
    /// The access tokens handed out: the only bearers the MCP endpoint takes.
    issued: Mutex<BTreeSet<String>>,
    /// The refresh token that works now: Entra rotates them, so an old one is refused.
    refresh: Mutex<Option<String>>,
    /// The grant type of each token request.
    grants: Mutex<Vec<String>>,
    /// The name and arguments of each tool call.
    calls: Mutex<Vec<(String, Value)>>,
}

impl Fake {
    async fn start() -> (Arc<Fake>, String) {
        let fake = Arc::new(Fake::default());
        let app = Router::new()
            .route(
                "/.well-known/oauth-protected-resource/mcp",
                get(|| async {
                    Json(json!({
                        "resource": "https://workiq.svc.cloud.microsoft/mcp",
                        "authorization_servers": ["https://login.microsoftonline.com/contoso/v2.0"],
                    }))
                }),
            )
            .route("/{tenant}/oauth2/v2.0/devicecode", post(device_code))
            .route("/{tenant}/oauth2/v2.0/token", post(token))
            .route("/mcp", post(mcp))
            .with_state(fake.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (fake, base)
    }

    fn calls(&self) -> Vec<(String, Value)> {
        self.calls.lock().unwrap().clone()
    }

    fn grants(&self) -> Vec<String> {
        self.grants.lock().unwrap().clone()
    }
}

fn jwt(claims: &Value) -> String {
    let part = |value: &Value| data_encoding::BASE64URL_NOPAD.encode(value.to_string().as_bytes());
    format!("{}.{}.sig", part(&json!({ "alg": "none" })), part(claims))
}

/// A field of a form body (only the ones this fake reads, whose values need no decoding but
/// `:`).
fn field(body: &str, name: &str) -> Option<String> {
    body.split('&')
        .find_map(|pair| pair.strip_prefix(name)?.strip_prefix('='))
        .map(|value| value.replace("%3A", ":"))
}

async fn device_code(UrlPath(_tenant): UrlPath<String>) -> Json<Value> {
    Json(json!({
        "device_code": "device-1",
        "user_code": "FAKE-CODE",
        "verification_uri": "https://microsoft.com/devicelogin",
        "interval": 0,
        "expires_in": 60,
        "message": "To sign in, enter FAKE-CODE at https://microsoft.com/devicelogin",
    }))
}

async fn token(
    State(fake): State<Arc<Fake>>,
    UrlPath(_tenant): UrlPath<String>,
    body: String,
) -> (StatusCode, Json<Value>) {
    let grant = field(&body, "grant_type").unwrap_or_default();
    let n = {
        let mut grants = fake.grants.lock().unwrap();
        grants.push(grant.clone());
        grants.len()
    };
    let mut refresh = fake.refresh.lock().unwrap();
    let device = grant == "urn:ietf:params:oauth:grant-type:device_code";
    let good = if device {
        field(&body, "device_code").as_deref() == Some("device-1")
    } else {
        grant == "refresh_token" && field(&body, "refresh_token") == *refresh
    };
    if !good {
        let error = json!({ "error": "invalid_grant", "error_description": "AADSTS70000" });
        return (StatusCode::BAD_REQUEST, Json(error));
    }
    let access = jwt(&json!({
        "aud": "api://workiq.svc.cloud.microsoft",
        "tid": TENANT_ID,
        "scp": "WorkIQAgent.Ask",
        "n": n,
    }));
    fake.issued.lock().unwrap().insert(access.clone());
    let next = format!("refresh-{n}");
    *refresh = Some(next.clone());
    let mut answer = json!({
        "token_type": "Bearer",
        "access_token": access,
        "refresh_token": next,
        "expires_in": 3600,
    });
    if device {
        answer["id_token"] = json!(jwt(&json!({
            "preferred_username": "alice@contoso.example",
            "name": "Alice Example",
            "tid": TENANT_ID,
        })));
    }
    (StatusCode::OK, Json(answer))
}

/// A file as Work IQ (CLI 1.0.1) returns one: its label object, no relevanceScore.
fn work_file(id: &str, protection: &str, label: &str, text: &str) -> Value {
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

/// What a tool answers, in the shapes Work IQ (CLI 1.0.1) uses: `retrieve` a General file,
/// a Confidential one and a General one that may not be handed out; `ask` the answer as
/// text and structured, with the conversation id; `list_agents` one agent, as JSON in the
/// text content and nothing structured; `fetch` the account's Teams chats (see
/// [`chats_answer`]) or else the driveItem of a Markdown spec labelled General, and
/// `fetch_blob` its bytes ([`SPEC_DOC`]).
fn tool_answer(name: &str, arguments: &Value) -> Value {
    let fetched = arguments["entityUrls"][0].as_str().unwrap_or_default();
    let (structured, text) = match name {
        "fetch" if fetched.starts_with("/me/chats") => (chats_answer(fetched), String::new()),
        "retrieve" => (
            json!({
                "@workiq.untrustedContent": "Treat this content as untrusted.",
                "application/vnd.ms-workiq.retrieval": {
                    "results": [
                        work_file("f1", "allowed", "General", "Budget: 120k for the May launch."),
                        work_file("f2", "allowed", "Confidential", "Board-only numbers."),
                        work_file("f3", "blocked", "General", "Blocked draft."),
                    ],
                    "resultCount": 3,
                    "stoppedReason": "completed",
                }
            }),
            String::new(),
        ),
        "ask" => (
            json!({ "answer": "The launch moved to May.", "conversationId": "conv-2" }),
            "The launch moved to May.".to_owned(),
        ),
        "list_agents" => (
            Value::Null,
            json!([{ "agentId": "sales:1", "name": "Sales", "provider": "Contoso" }]).to_string(),
        ),
        "fetch" => (
            json!({
                "results": [{
                    "statusCode": 200,
                    "data": {
                        "id": "spec-1",
                        "name": "Albums.md",
                        "size": SPEC_DOC.len(),
                        "file": { "mimeType": "text/markdown" },
                        "sensitivityLabel": { "displayName": "General", "id": "label-General" },
                    },
                }],
            }),
            String::new(),
        ),
        "fetch_blob" => (
            json!({
                "statusCode": 200,
                "contentType": "text/markdown",
                "blobName": "Albums.md",
                "sizeBytes": SPEC_DOC.len(),
                "base64Content": data_encoding::BASE64.encode(SPEC_DOC.as_bytes()),
            }),
            String::new(),
        ),
        other => panic!("unexpected tool {other}"),
    };
    let content: Vec<Value> = if text.is_empty() {
        Vec::new()
    } else {
        vec![json!({ "type": "text", "text": text })]
    };
    json!({
        "content": content,
        "structuredContent": structured,
        "isError": false,
    })
}

/// What `fetch` gives for the account's Teams chats, as Microsoft Graph would: its recent
/// chats (`/me/chats`), among them its chat with itself; [`CREW_CHAT`], which it is a member
/// of (`/me/chats/{id}`); and no other chat.
fn chats_answer(path: &str) -> Value {
    let crew = json!({ "id": CREW_CHAT, "topic": "Launch crew", "chatType": "group" });
    let entity = if path.starts_with("/me/chats?") {
        let notes = json!({ "id": "48:notes", "topic": null, "chatType": "oneOnOne" });
        let ana = json!({ "id": "19:alice_ana@unq.gbl.spaces", "chatType": "oneOnOne" });
        json!({ "statusCode": 200, "data": { "value": [crew, notes, ana] } })
    } else if path.starts_with(&format!("/me/chats/{CREW_CHAT}?")) {
        json!({ "statusCode": 200, "data": crew })
    } else {
        let error = json!({ "code": "NotFound", "message": "Chat not found." });
        json!({ "statusCode": 404, "data": { "error": error } })
    };
    json!({ "results": [entity] })
}

async fn mcp(
    State(fake): State<Arc<Fake>>,
    headers: HeaderMap,
    body: String,
) -> (StatusCode, [(&'static str, &'static str); 1], String) {
    let json = [("content-type", "application/json")];
    let bearer = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or_default();
    if !fake.issued.lock().unwrap().contains(bearer) {
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
            let tools = ["retrieve", "ask", "list_agents", "fetch", "fetch_blob"]
                .map(|name| json!({ "name": name, "inputSchema": { "type": "object" } }));
            json!({ "tools": tools })
        }
        "tools/call" => {
            let name = message["params"]["name"].as_str().unwrap().to_owned();
            let arguments = message["params"]["arguments"].clone();
            let call = (name.clone(), arguments.clone());
            fake.calls.lock().unwrap().push(call);
            tool_answer(&name, &arguments)
        }
        other => panic!("unexpected method {other}"),
    };
    let reply = json!({ "jsonrpc": "2.0", "id": message["id"], "result": result });
    (StatusCode::OK, json, reply.to_string())
}

/// The settings each run of the program gets: its own profiles folder, secrets in files.
#[derive(Clone)]
struct Env {
    vars: Vec<(&'static str, String)>,
}

impl Env {
    fn new(home: &Path) -> Self {
        Env {
            vars: vec![
                ("PROOFKIN_HOME", home.display().to_string()),
                ("PROOFKIN_SECRET_STORE", "file".to_owned()),
            ],
        }
    }

    fn with(mut self, var: &'static str, value: &str) -> Self {
        self.vars.retain(|(v, _)| *v != var);
        self.vars.push((var, value.to_owned()));
        self
    }

    fn command(&self, args: &[&str]) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(EXE);
        for var in [TEST_WORKIQ_URL, TEST_ENTRA_URL] {
            command.env_remove(var);
        }
        command
            .envs(self.vars.iter().map(|(var, value)| (*var, value.as_str())))
            .args(args)
            .stdin(Stdio::null())
            .kill_on_drop(true);
        command
    }

    /// Runs the program to the end: whether it succeeded, and what it printed.
    async fn run(&self, args: &[&str]) -> (bool, String) {
        let out = self.command(args).output().await.unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (out.status.success(), text)
    }

    async fn ok(&self, args: &[&str]) -> String {
        let (success, text) = self.run(args).await;
        assert!(success, "proofkin {} failed:\n{text}", args.join(" "));
        text
    }
}

/// A running agent (`proofkin --name ...`), driven through its prompt.
struct Repl {
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    lines: mpsc::UnboundedReceiver<String>,
    seen: Vec<String>,
}

impl Repl {
    fn start(env: &Env, args: &[&str]) -> Self {
        let mut child = env
            .command(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let (tx, lines) = mpsc::unbounded_channel();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let out = tx.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                let _ = out.send(line);
            }
        });
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                let _ = tx.send(format!("stderr: {line}"));
            }
        });
        let stdin = child.stdin.take().unwrap();
        Repl {
            child,
            stdin,
            lines,
            seen: Vec::new(),
        }
    }

    /// Waits for a line with `text`, and returns it.
    async fn until(&mut self, text: &str) -> String {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            match tokio::time::timeout_at(deadline, self.lines.recv()).await {
                Ok(Some(line)) => {
                    self.seen.push(line.clone());
                    if line.contains(text) {
                        return line;
                    }
                }
                Ok(None) => panic!(
                    "the agent ended without {text:?}:\n{}",
                    self.seen.join("\n")
                ),
                Err(_) => panic!("no {text:?} in {WAIT:?}:\n{}", self.seen.join("\n")),
            }
        }
    }

    async fn send(&mut self, line: &str) {
        self.stdin
            .write_all(format!("{line}\n").as_bytes())
            .await
            .unwrap();
        self.stdin.flush().await.unwrap();
    }

    async fn quit(mut self) {
        self.send("/quit").await;
        let stopped = tokio::time::timeout(WAIT, self.child.wait()).await;
        assert!(stopped.is_ok(), "the agent did not stop on /quit");
    }
}

/// A peer's model: it keeps the prompts it gets.
#[derive(Default)]
struct Model {
    prompts: Mutex<Vec<String>>,
}

#[async_trait]
impl Executor for Model {
    fn providers(&self) -> Vec<Provider> {
        vec![Provider::Copilot]
    }

    async fn execute(&self, task: &LlmTask, _ctx: &TaskContext) -> anyhow::Result<Ran> {
        self.prompts.lock().unwrap().push(task.prompt.clone());
        Ok(Ran::new("The launch budget is 120k.".to_owned(), None))
    }
}

/// A peer named helper that takes tasks from `boss`.
async fn helper(boss: EndpointId, dir: &Path) -> (Agent, Arc<Model>) {
    let model = Arc::new(Model::default());
    let mut trust = TrustStore::default();
    trust.allow(TrustedPeer {
        name: "boss".into(),
        id: boss,
        addrs: BTreeSet::new(),
        can_delegate: true,
        tasks_per_hour: 10,
        access: Access::ReadOnly,
        shares: vec![DATA_SHARE.to_owned()],
        m365: None,
        github: None,
        agents: Vec::new(),
    });
    let data = dir.join("data");
    std::fs::create_dir_all(&data).unwrap();
    let mut config = AgentConfig::new("helper", SecretKey::generate());
    config.network = Network::Local;
    config.bind = Some("127.0.0.1:0".parse().unwrap());
    config.trust = trust;
    config.executor = Some(model.clone());
    config.shares = ShareStore::with_data(data);
    config.worktrees_dir = dir.join("worktrees");
    let (agent, mut events) = Agent::bind(config).await.unwrap();
    tokio::spawn(async move { while events.recv().await.is_some() {} });
    (agent, model)
}

fn endpoint_id(peers_id: &str) -> EndpointId {
    peers_id
        .lines()
        .find_map(|line| line.trim().strip_prefix("endpoint id:"))
        .unwrap_or_else(|| panic!("no endpoint id in:\n{peers_id}"))
        .trim()
        .parse()
        .unwrap()
}

/// Every value under `key`, anywhere in `value`.
fn values_of(value: &Value, key: &str) -> Vec<Value> {
    match value {
        Value::Object(map) => map
            .iter()
            .flat_map(|(k, v)| {
                let mut found = if k == key {
                    vec![v.clone()]
                } else {
                    Vec::new()
                };
                found.extend(values_of(v, key));
                found
            })
            .collect(),
        Value::Array(items) => items.iter().flat_map(|v| values_of(v, key)).collect(),
        _ => Vec::new(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_work_iq_commands_run_through_the_program_with_a_hosted_account() {
    let (fake, base) = Fake::start().await;
    let home = tempfile::tempdir().unwrap();
    let env = Env::new(home.path())
        .with(TEST_WORKIQ_URL, &base)
        .with(TEST_ENTRA_URL, &base);

    let out = env
        .ok(&[
            "auth",
            "login",
            "workiq",
            "--tenant",
            "contoso",
            "--client-id",
            "app-1",
            "--device-code",
            "--profile",
            "boss",
        ])
        .await;
    assert!(out.contains("enter FAKE-CODE"), "{out}");
    assert!(
        out.contains("Work IQ signed in as Alice Example (alice@contoso.example)"),
        "{out}"
    );

    // Each run below refreshes the token, which Entra rotates: each must use the one the run
    // before it saved.
    let out = env.ok(&["workiq", "doctor", "--profile", "boss"]).await;
    assert!(out.contains("Work IQ is ready."), "{out}");

    let out = env.ok(&["workiq", "agents", "--profile", "boss"]).await;
    assert!(out.contains("Sales  sales:1"), "{out}");

    let import = [
        "kb",
        "import",
        "workiq",
        "launch",
        "--query",
        "launch budget",
        "--profile",
        "boss",
    ];
    let out = env.ok(&import).await;
    assert!(
        out.contains("saved topic launch from Work IQ: 2 result(s) kept")
            && out.contains("1 withheld"),
        "{out}"
    );
    let topic = env.ok(&["kb", "show", "launch", "--profile", "boss"]).await;
    assert!(topic.contains("120k for the May launch"), "{topic}");
    assert!(topic.contains("Board-only numbers"), "{topic}");
    assert!(!topic.contains("Blocked draft"), "{topic}");

    // A plan in the program's agent: an @workiq step it runs itself, and a workiq= step for a
    // peer it grants General content only.
    let boss_id = endpoint_id(&env.ok(&["peers", "id", "--profile", "boss"]).await);
    let peer_dir = tempfile::tempdir().unwrap();
    let (helper, model) = helper(boss_id, peer_dir.path()).await;
    let ticket = Ticket {
        name: "helper".into(),
        addr: helper.addr(),
    }
    .encode();
    env.ok(&[
        "peers",
        "allow",
        &ticket,
        "--profile",
        "boss",
        "--m365",
        "General",
    ])
    .await;
    let plan = home.path().join("launch.plan");
    std::fs::write(
        &plan,
        "a @workiq What changed in the launch?\nb @helper workiq=files Find the launch budget\n",
    )
    .unwrap();
    let mut boss = Repl::start(
        &env,
        &[
            "--name",
            "boss",
            "--profile",
            "boss",
            "--network",
            "local",
            "--listen",
            "127.0.0.1:0",
            "--peer",
            &ticket,
        ],
    );
    boss.until("+ helper").await;
    boss.send(&format!("/plan {}", plan.display())).await;
    let finished = boss.until("=== plan finished").await;
    assert!(
        finished.contains("2/2 steps done"),
        "{}",
        boss.seen.join("\n")
    );
    boss.until("report:").await;
    boss.quit().await;
    helper.shutdown().await;

    let prompts = model.prompts.lock().unwrap().clone();
    assert_eq!(prompts.len(), 1, "{prompts:?}");
    assert!(
        prompts[0].contains("120k for the May launch"),
        "{}",
        prompts[0]
    );
    assert!(!prompts[0].contains("Board-only numbers"), "not granted");
    assert!(
        !prompts[0].contains("Blocked draft"),
        "may not be handed out"
    );

    let runs = env.ok(&["runs", "list", "--profile", "boss"]).await;
    let id = runs.split_whitespace().next().unwrap().to_owned();
    let verified = env.ok(&["runs", "verify", &id, "--profile", "boss"]).await;
    assert!(verified.contains("signed records verify"), "{verified}");
    let shown = env.ok(&["runs", "show", &id, "--profile", "boss"]).await;
    assert!(shown.contains("The launch moved to May."), "{shown}");

    // The workiq= step's signed evidence records the context it was sent by its hash only,
    // which is the hash of the section the run kept for it.
    let record = home
        .path()
        .join(format!("profiles/boss/runs/{id}/run.json"));
    let record: Value = serde_json::from_str(&std::fs::read_to_string(record).unwrap()).unwrap();
    let kept = &record["workiq"]["b"];
    // Steps have a `context` too (their knowledge topics), empty in this plan.
    let contexts: Vec<Value> = values_of(&record, "context")
        .into_iter()
        .filter(|v| v.as_array().is_some_and(|list| !list.is_empty()))
        .collect();
    assert_eq!(contexts.len(), 1, "{record:#}");
    assert_eq!(
        contexts[0],
        json!([{
            "source": "workiq",
            "sha256": kept["sha256"],
            "results": 1,
            "labels": ["General"],
        }])
    );
    // The context is kept next to run.json, never in it.
    assert!(kept.get("text").is_none(), "{kept:#}");
    let text = std::fs::read_to_string(
        home.path()
            .join(format!("profiles/boss/runs/{id}/b.workiq.md")),
    )
    .unwrap();
    assert!(text.contains("120k for the May launch"), "{text}");
    assert_eq!(
        kept["sha256"].as_str().unwrap(),
        proofkin::evidence::sha256_hex(text.as_bytes())
    );

    let usage = env.ok(&["workiq", "usage", "--profile", "boss"]).await;
    assert!(usage.contains("calls today    5 of"), "{usage}");
    let mut tools: Vec<String> = fake.calls().into_iter().map(|(name, _)| name).collect();
    tools.sort();
    assert_eq!(
        tools,
        ["ask", "list_agents", "retrieve", "retrieve", "retrieve"]
    );
    let queries: Vec<String> = fake
        .calls()
        .iter()
        .filter_map(|(_, arguments)| arguments["query"].as_str().map(str::to_owned))
        .collect();
    assert!(queries.contains(&PROBE_QUERY.to_owned()), "{queries:?}");
    assert!(queries.contains(&"launch budget".to_owned()), "{queries:?}");
    let grants = fake.grants();
    assert!(grants[0].ends_with("device_code"), "{grants:?}");
    assert!(
        grants.len() >= 5 && grants[1..].iter().all(|g| g == "refresh_token"),
        "{grants:?}"
    );

    // A test setting off this machine is refused, never used: no token leaves it.
    let away = env
        .clone()
        .with(TEST_WORKIQ_URL, "https://workiq.example.com");
    let (worked, out) = away.run(&["workiq", "agents", "--profile", "boss"]).await;
    assert!(!worked && out.contains("only for tests"), "{out}");
    let (worked, out) = away.run(&["workiq", "doctor", "--profile", "boss"]).await;
    assert!(!worked && out.contains("only for tests"), "{out}");
    assert_eq!(fake.calls().len(), 5);

    // A spec document on SharePoint, read through Work IQ: its requirement is the plan's claim,
    // and it is saved as a topic marked with its label. Both calls are audited as spec-import.
    let link = "https://contoso.sharepoint.com/sites/Shop/Shared%20Documents/Albums.md";
    let from = [
        "spec",
        "import",
        "--from",
        link,
        "--profile",
        "boss",
        "--set",
        "peer=helper",
    ];
    let out = env.ok(&from).await;
    let claim = "claim: FR-001 Users can share an album.";
    assert!(out.contains(claim), "{out}");
    assert!(out.contains(&format!("Imported from {link}.")), "{out}");
    assert!(out.contains("Its sensitivity label is General"), "{out}");
    let topic = home.path().join("profiles/boss/knowledge/spec-albums.md");
    let topic = std::fs::read_to_string(topic).unwrap();
    assert_eq!(
        proofkin::knowledge::parse_mark(&topic),
        Some(vec!["General".to_owned()])
    );
    let requirement = "FR-001: Users can share an album.";
    assert!(topic.contains(requirement), "{topic}");
    let audit = home.path().join("profiles/boss/workiq-audit.jsonl");
    let audit = std::fs::read_to_string(audit).unwrap();
    let rows: Vec<Value> = audit
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|row| row["purpose"] == "spec-import")
        .collect();
    let tools: Vec<&str> = rows.iter().map(|r| r["tool"].as_str().unwrap()).collect();
    assert_eq!(tools, ["fetch", "fetch_blob"], "{audit}");
    assert!(rows.iter().all(|row| row["outcome"] == "ok"), "{audit}");

    // Refused before any call: a link that isn't https, and a folder as well as a link.
    let mut http = from;
    http[3] = "http://contoso.sharepoint.com/sites/Shop/Shared%20Documents/Albums.md";
    let (worked, out) = env.run(&http).await;
    let off = "is not a SharePoint or OneDrive link";
    assert!(!worked && out.contains(off), "{out}");
    let both = [
        "spec",
        "import",
        "specs/001-albums",
        "--from",
        link,
        "--profile",
        "boss",
        "--set",
        "peer=helper",
    ];
    let (worked, out) = env.run(&both).await;
    assert!(!worked && out.contains("not both"), "{out}");
    let calls = fake.calls();
    let names: Vec<&str> = calls[5..].iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(names, ["fetch", "fetch_blob"]);

    env.ok(&["auth", "logout", "workiq", "--profile", "boss"])
        .await;
    let (worked, out) = env.run(&["workiq", "doctor", "--profile", "boss"]).await;
    assert!(!worked && out.contains("FAILED"), "{out}");
}

fn python() -> &'static str {
    if cfg!(windows) { "python" } else { "python3" }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_work_iq_sign_in_made_while_the_agent_runs_counts_for_its_next_plan() {
    let (_fake, base) = Fake::start().await;
    let home = tempfile::tempdir().unwrap();
    let env = Env::new(home.path())
        .with(TEST_WORKIQ_URL, &base)
        .with(TEST_ENTRA_URL, &base);
    let plan = home.path().join("ask.plan");
    std::fs::write(&plan, "a @workiq What changed in the launch?\n").unwrap();
    let mut boss = Repl::start(
        &env,
        &[
            "--name",
            "boss",
            "--network",
            "local",
            "--listen",
            "127.0.0.1:0",
        ],
    );
    boss.until("ticket:").await;

    // Signed in from another terminal while the agent runs: its next plan uses it.
    env.ok(&[
        "auth",
        "login",
        "workiq",
        "--tenant",
        "contoso",
        "--client-id",
        "app-1",
        "--device-code",
        "--profile",
        "boss",
    ])
    .await;
    boss.send(&format!("/plan {}", plan.display())).await;
    let finished = boss.until("=== plan finished").await;
    assert!(
        finished.contains("1/1 steps done"),
        "{}",
        boss.seen.join("\n")
    );
    boss.quit().await;
}

/// Runs `proofkin workiq notices ARGS --profile boss`: whether it succeeded, and what it
/// printed.
async fn notices(env: &Env, args: &[&str]) -> (bool, String) {
    let mut command = vec!["workiq", "notices"];
    command.extend_from_slice(args);
    command.extend_from_slice(&["--profile", "boss"]);
    env.run(&command).await
}

/// Sign-off notices are off until the owner opts in, by mail to the account's own address
/// or in a Teams chat: `--list` shows the account's recent chats to choose from, never its
/// chat with itself, and a chat is kept only once Microsoft Graph says the account is a
/// member of it. `workiq doctor` says where notices go. Choosing reads (each `fetch` audited
/// as a command) and sends nothing.
#[tokio::test(flavor = "multi_thread")]
async fn sign_off_notices_go_by_mail_or_to_a_teams_chat_the_account_is_a_member_of() {
    let (fake, base) = Fake::start().await;
    let home = tempfile::tempdir().unwrap();
    let env = Env::new(home.path())
        .with(TEST_WORKIQ_URL, &base)
        .with(TEST_ENTRA_URL, &base);
    env.ok(&[
        "auth",
        "login",
        "workiq",
        "--tenant",
        "contoso",
        "--client-id",
        "app-1",
        "--device-code",
        "--profile",
        "boss",
    ])
    .await;
    let boss = home.path().join("profiles").join("boss");
    let setting = || {
        let text = std::fs::read_to_string(boss.join("profile.json")).unwrap();
        serde_json::from_str::<Value>(&text).unwrap()["workiq_notices"].clone()
    };

    assert_eq!(setting(), Value::Null);
    let out = env.ok(&["workiq", "doctor", "--profile", "boss"]).await;
    assert!(
        out.contains("Sign-off notices: off (turn them on with: proofkin workiq notices mail"),
        "{out}"
    );

    // The chats to choose from, as Microsoft Graph lists them, but the chat with yourself.
    // Listing them keeps nothing.
    let (ok, out) = notices(&env, &["teams", "--list"]).await;
    assert!(ok, "{out}");
    let listed: Vec<&str> = out.lines().filter(|l| l.starts_with("  ")).collect();
    assert_eq!(
        listed,
        [
            "  19:crew@thread.v2  group  \"Launch crew\"",
            "  19:alice_ana@unq.gbl.spaces  oneOnOne",
        ],
        "{out}"
    );
    let list = json!({ "entityUrls": ["/me/chats?$select=id,topic,chatType&$top=20"] });
    assert_eq!(fake.calls().last(), Some(&("fetch".to_owned(), list)));
    assert_eq!(setting(), Value::Null);

    // A chat the account isn't a member of is refused, and isn't kept.
    let (ok, out) = notices(&env, &["teams", "--chat", "19:other@thread.v2"]).await;
    assert!(!ok, "{out}");
    assert!(
        out.contains("you aren't a member of Teams chat 19:other@thread.v2"),
        "{out}"
    );
    let path = "/me/chats/19:other@thread.v2?$select=id,topic,chatType";
    let checked = ("fetch".to_owned(), json!({ "entityUrls": [path] }));
    assert_eq!(fake.calls().last(), Some(&checked));
    assert_eq!(setting(), Value::Null);

    // Teams' chat with yourself isn't offered: refused without a call.
    let before = fake.calls().len();
    let (ok, out) = notices(&env, &["teams", "--chat", "48:notes"]).await;
    assert!(
        !ok && out.contains("isn't in the Microsoft Graph reference"),
        "{out}"
    );
    assert_eq!(fake.calls().len(), before);
    assert_eq!(setting(), Value::Null);

    // A chat it is a member of is kept.
    let (ok, out) = notices(&env, &["teams", "--chat", CREW_CHAT]).await;
    assert!(ok, "{out}");
    assert!(
        out.contains("profile 'boss': sign-off notices go in Teams chat 19:crew@thread.v2:"),
        "{out}"
    );
    let path = "/me/chats/19:crew@thread.v2?$select=id,topic,chatType";
    let checked = ("fetch".to_owned(), json!({ "entityUrls": [path] }));
    assert_eq!(fake.calls().last(), Some(&checked));
    assert_eq!(setting(), json!({ "to": "teams", "chat": CREW_CHAT }));
    let out = env.ok(&["workiq", "doctor", "--profile", "boss"]).await;
    assert!(
        out.contains("Sign-off notices: in Teams chat 19:crew@thread.v2 (turn them off with:"),
        "{out}"
    );

    // By mail, to the account's own address; --chat is for Teams.
    let (ok, out) = notices(&env, &["mail", "--chat", CREW_CHAT]).await;
    assert!(!ok && out.contains("--chat and --list are for"), "{out}");
    let (ok, out) = notices(&env, &["mail"]).await;
    assert!(ok, "{out}");
    assert!(
        out.contains("sign-off notices go by mail to alice@contoso.example:"),
        "{out}"
    );
    assert_eq!(setting(), json!({ "to": "mail" }));
    let out = env.ok(&["workiq", "doctor", "--profile", "boss"]).await;
    assert!(
        out.contains("Sign-off notices: by mail to alice@contoso.example (turn them off with:"),
        "{out}"
    );

    let (ok, out) = notices(&env, &["off"]).await;
    assert!(ok && out.contains("sign-off notices are off"), "{out}");
    assert_eq!(setting(), Value::Null);

    // The three reads, audited as the command's own.
    let audited = read(&boss.join("workiq-audit.jsonl")).unwrap();
    let fetched: Vec<&str> = audited
        .iter()
        .filter(|e| e.tool == "fetch")
        .map(|e| e.purpose.as_str())
        .collect();
    assert_eq!(fetched, ["command"; 3], "{audited:?}");
}

/// The fake CLI in mode `full`, started as the real one is: `workiq.cmd` on Windows, `workiq`
/// elsewhere, in a folder of its own (not on the PATH).
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

#[tokio::test]
async fn the_work_iq_commands_run_through_the_program_with_the_work_iq_cli() {
    let home = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let bin = tempfile::tempdir().unwrap();
    let cli = fake_cli_at(bin.path(), state.path());
    let env = Env::new(home.path());

    let login = [
        "auth",
        "login",
        "workiq",
        "--cli",
        "--cli-path",
        cli.to_str().unwrap(),
        "--profile",
        "carol",
    ];
    let out = env.ok(&login).await;
    assert!(out.contains("as bob@contoso.example"), "{out}");

    let out = env.ok(&["workiq", "doctor", "--profile", "carol"]).await;
    assert!(out.contains("Work IQ is ready."), "{out}");

    let out = env.ok(&["workiq", "agents", "--profile", "carol"]).await;
    assert!(out.contains("Sales  sales:1"), "{out}");

    let import = [
        "kb",
        "import",
        "workiq",
        "notes",
        "--query",
        "launch budget",
        "--profile",
        "carol",
    ];
    let out = env.ok(&import).await;
    assert!(
        out.contains("1 result(s) kept (General), 1 withheld"),
        "{out}"
    );
    let topic = env.ok(&["kb", "show", "notes", "--profile", "carol"]).await;
    assert!(
        topic.contains("Launch budget notes from the CLI."),
        "{topic}"
    );
    assert!(!topic.contains("may not hand out"), "{topic}");

    let usage = env.ok(&["workiq", "usage", "--profile", "carol"]).await;
    assert!(usage.contains("calls today    3 of"), "{usage}");
}
