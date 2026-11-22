//! Other agents hand work to a profile's peers over MCP and A2A, with the profile's
//! integration token (never the web UI's), and get the results and their evidence;
//! the spec import fills in plans from Spec Kit features in the UI and over MCP.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use proofkin::shares::ShareStore;
use proofkin::ui::{self, UiInfo};
use proofkin::{
    Access, Agent, AgentConfig, Executor, LlmTask, Network, Profile, Provider, SecretKey,
    TaskContext, TrustedPeer,
};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

struct Echo;

/// Lets the task "gated" finish (it waits for this).
static GATE: tokio::sync::Notify = tokio::sync::Notify::const_new();

#[async_trait]
impl Executor for Echo {
    fn providers(&self) -> Vec<Provider> {
        vec![Provider::Copilot]
    }

    async fn execute(&self, task: &LlmTask, _ctx: &TaskContext) -> anyhow::Result<String> {
        if task.prompt == "hang" {
            std::future::pending::<()>().await;
        }
        if task.prompt == "slow" {
            tokio::time::sleep(Duration::from_millis(1500)).await;
        }
        if task.prompt == "gated" {
            GATE.notified().await;
        }
        Ok(format!("echo: {}", task.prompt))
    }
}

struct Response {
    status: u16,
    body: String,
}

impl Response {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or_else(|_| panic!("JSON body: {}", self.body))
    }
}

async fn request(
    port: u16,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> Response {
    let mut req =
        format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n");
    for (name, value) in headers {
        req += &format!("{name}: {value}\r\n");
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
    Response {
        status: head.split(' ').nth(1).unwrap().parse().unwrap(),
        body: body.to_owned(),
    }
}

/// A JSON-RPC request.
fn rpc(method: &str, params: Value) -> String {
    json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }).to_string()
}

/// Sends an A2A streaming request, lets the gated task finish once `until` has
/// arrived, and returns the response head and the events of the stream to its end.
async fn follow_stream(
    port: u16,
    auth: &str,
    method: &str,
    params: Value,
    until: &str,
) -> (String, Vec<Value>) {
    let body = rpc(method, params);
    let req = format!(
        "POST /a2a HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\
         Authorization: {auth}\r\nA2A-Version: 1.0\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    let mut chunk = [0u8; 4096];
    tokio::time::timeout(Duration::from_secs(10), async {
        while !String::from_utf8_lossy(&raw)
            .split_once("statusUpdate")
            .is_some_and(|(_, update)| update.contains(until))
        {
            let n = stream.read(&mut chunk).await.unwrap();
            assert!(n > 0, "the stream ended before {until:?}");
            raw.extend_from_slice(&chunk[..n]);
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no {until:?} in {}", String::from_utf8_lossy(&raw)));
    GATE.notify_one();
    tokio::time::timeout(Duration::from_secs(10), stream.read_to_end(&mut raw))
        .await
        .expect("the stream closes when the task ends")
        .unwrap();
    parse_events(raw)
}

/// Splits a response read to its end into its head and the events of its stream.
fn parse_events(raw: Vec<u8>) -> (String, Vec<Value>) {
    let (head, text) = split_response(raw);
    (head, sse_events(&text))
}

/// The JSON data of a stream's events.
fn sse_events(text: &str) -> Vec<Value> {
    text.split("\n\n")
        .flat_map(|event| event.lines())
        .filter_map(|line| line.strip_prefix("data:"))
        .map(|data| serde_json::from_str(data.trim()).expect("JSON event"))
        .collect()
}

/// A response read to its end: its head, and its body (its chunks joined).
fn split_response(raw: Vec<u8>) -> (String, String) {
    let raw = String::from_utf8(raw).unwrap();
    let (head, body) = raw.split_once("\r\n\r\n").expect("HTTP response");
    let mut text = String::new();
    if head.to_lowercase().contains("transfer-encoding: chunked") {
        let mut rest = body;
        while let Some((size, after)) = rest.split_once("\r\n") {
            let size = usize::from_str_radix(size.trim(), 16).unwrap();
            if size == 0 {
                break;
            }
            text += &after[..size];
            rest = &after[size + 2..];
        }
    } else {
        text = body.to_owned();
    }
    (head.to_owned(), text)
}

/// Checks a followed task: it starts as the task, says what it is doing, and ends as
/// the finished task, with nothing after. Returns the task's id.
fn assert_followed(head: &str, events: &[Value], result: &str) -> String {
    assert!(
        head.to_lowercase()
            .contains("content-type: text/event-stream"),
        "{head}"
    );
    assert!(events.iter().all(|e| e["jsonrpc"] == "2.0" && e["id"] == 1));
    let first = &events[0]["result"]["task"];
    assert_eq!(first["status"]["state"], "TASK_STATE_WORKING", "{first}");
    let id = first["id"].as_str().unwrap().to_owned();
    let doing = events.iter().any(|e| {
        let update = &e["result"]["statusUpdate"];
        update["taskId"] == id.as_str()
            && update["status"]["state"] == "TASK_STATE_WORKING"
            && update["status"]["message"]["role"] == "ROLE_AGENT"
            && update["status"]["message"]["parts"][0]["text"]
                .as_str()
                .is_some_and(|t| t.contains("worker: the model is working"))
    });
    assert!(
        doing,
        "a status update says what the peer is doing: {events:?}"
    );
    let last = &events.last().unwrap()["result"]["task"];
    assert_eq!(last["id"], id.as_str());
    assert_eq!(last["status"]["state"], "TASK_STATE_COMPLETED", "{last}");
    assert_eq!(last["artifacts"][0]["parts"][0]["text"], result);
    assert_eq!(
        events
            .iter()
            .filter(|e| e["result"].get("task").is_some())
            .count(),
        2,
        "only the first and the last event are the task"
    );
    id
}

/// A request of MCP's stateless revision: the version in `_meta`, mirrored in headers.
fn modern(method: &str, mut params: Value) -> String {
    params["_meta"]["io.modelcontextprotocol/protocolVersion"] = json!("2026-07-28");
    params["_meta"]["io.modelcontextprotocol/clientCapabilities"] = json!({});
    rpc(method, params)
}

/// The home both tests use: the environment is set once, before any thread reads it.
fn shared_home() -> &'static Path {
    static HOME: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    HOME.get_or_init(|| {
        let home = tempfile::tempdir().unwrap();
        // Integration tokens go to files under the temporary home, never the OS store.
        unsafe {
            std::env::set_var("PROOFKIN_HOME", home.path());
            std::env::set_var("PROOFKIN_SECRET_STORE", "file");
        }
        home
    })
    .path()
}

#[tokio::test]
async fn other_agents_hand_work_to_peers_over_mcp_and_a2a() {
    let home = shared_home();
    let (boss_key, worker_key) = (SecretKey::generate(), SecretKey::generate());
    let trust = |name: &str, key: &SecretKey, delegate: bool| TrustedPeer {
        name: name.into(),
        id: key.public(),
        addrs: Default::default(),
        can_delegate: delegate,
        tasks_per_hour: 100,
        access: Access::ReadOnly,
        shares: vec!["data".into()],
    };
    let mut worker = AgentConfig::new("worker", worker_key.clone());
    worker.network = Network::Local;
    worker.bind = Some("127.0.0.1:0".parse().unwrap());
    worker.trust.allow(trust("boss", &boss_key, true));
    worker.executor = Some(Arc::new(Echo));
    worker.shares = ShareStore::with_data(home.join("data"));
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
    let profiles = home.join("profiles");
    let info = UiInfo {
        profile: Profile::open_in(&profiles, "boss").unwrap(),
        network: Network::Local,
        serving: false,
        access: Access::ReadOnly,
        workspace: home.to_path_buf(),
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
    let (port, ui_token) = rest.split_once("/#t=").unwrap();
    let port: u16 = port.parse().unwrap();
    let ui_auth = format!("Bearer {ui_token}");
    let admin = |method: &'static str, path: &'static str, body: Option<String>| {
        let auth = ui_auth.clone();
        async move {
            request(
                port,
                method,
                path,
                &[("Authorization", auth.as_str())],
                body.as_deref(),
            )
            .await
        }
    };

    // Off until the owner turns it on: the endpoints refuse and say how.
    let off = request(port, "POST", "/mcp", &[], Some(&rpc("ping", json!({})))).await;
    assert_eq!(off.status, 401);
    assert!(
        off.body.contains("proofkin connect --profile boss"),
        "{}",
        off.body
    );
    let card = request(port, "GET", "/.well-known/agent-card.json", &[], None).await;
    assert_eq!(card.status, 404);

    let on = admin("POST", "/api/connect", None).await.json();
    let token = on["token"].as_str().unwrap().to_owned();
    assert!(token.starts_with("pfk_") && token.len() == 68, "{token}");
    assert_eq!(on["mcp"], format!("http://127.0.0.1:{port}/mcp"));
    let auth = format!("Bearer {token}");
    let mcp = |extra: Vec<(&'static str, String)>, body: String| {
        let auth = auth.clone();
        async move {
            let mut headers = vec![
                ("Authorization", auth),
                ("Accept", "application/json, text/event-stream".to_owned()),
            ];
            headers.extend(extra);
            let headers: Vec<(&str, &str)> =
                headers.iter().map(|(n, v)| (*n, v.as_str())).collect();
            request(port, "POST", "/mcp", &headers, Some(&body)).await
        }
    };
    // Each token works only where it belongs; other sites' pages are refused.
    let ui_token_on_mcp = request(
        port,
        "POST",
        "/mcp",
        &[("Authorization", ui_auth.as_str())],
        Some(&rpc("ping", json!({}))),
    )
    .await;
    assert_eq!(ui_token_on_mcp.status, 401);
    let token_on_ui = request(
        port,
        "GET",
        "/api/state",
        &[("Authorization", auth.as_str())],
        None,
    )
    .await;
    assert_eq!(token_on_ui.status, 401);
    let page = request(
        port,
        "POST",
        "/mcp",
        &[
            ("Authorization", auth.as_str()),
            ("Origin", "https://evil.example"),
        ],
        Some(&rpc("ping", json!({}))),
    )
    .await;
    assert_eq!(page.status, 403);

    // MCP before 2026-07-28: initialize, then tools.
    let init = mcp(
        vec![],
        rpc(
            "initialize",
            json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "t", "version": "1" } }),
        ),
    )
    .await
    .json();
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(init["result"]["serverInfo"]["name"], "proofkin");
    let note = mcp(
        vec![],
        json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }).to_string(),
    )
    .await;
    assert_eq!(note.status, 202);
    let list = mcp(vec![], rpc("tools/list", json!({}))).await.json();
    let names: Vec<&str> = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "list_peers",
            "delegate",
            "get_task",
            "cancel_task",
            "check_plan",
            "run_plan",
            "get_run",
            "import_spec"
        ]
    );
    let peers = mcp(vec![], rpc("tools/call", json!({ "name": "list_peers" })))
        .await
        .json();
    assert_eq!(
        peers["result"]["structuredContent"]["peers"][0]["providers"],
        json!(["copilot"])
    );
    let delegated = mcp(
        vec![],
        rpc(
            "tools/call",
            json!({ "name": "delegate", "arguments": { "prompt": "hello", "peer": "worker", "wait_seconds": 20 } }),
        ),
    )
    .await
    .json();
    let result = &delegated["result"]["structuredContent"];
    assert_eq!(result["state"], "completed", "{delegated}");
    assert_eq!(result["output"], "echo: hello");
    assert_eq!(result["peer"], "worker");
    assert_eq!(result["evidence"], "verified", "signed by the worker");
    // It shows up in the owner's Tasks, marked as asked by another agent.
    let state = admin("GET", "/api/state", None).await.json();
    assert!(
        state["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["via"] == "MCP" && t["output"] == "echo: hello"),
        "{}",
        state["tasks"]
    );
    // A problem is the calling model's to see, not a protocol error.
    let nobody = mcp(
        vec![],
        rpc(
            "tools/call",
            json!({ "name": "delegate", "arguments": { "prompt": "x", "peer": "nobody" } }),
        ),
    )
    .await
    .json();
    assert_eq!(nobody["result"]["isError"], true);
    assert!(
        nobody["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("nobody is not connected (connected: worker)")
    );
    let long = mcp(
        vec![],
        rpc(
            "tools/call",
            json!({ "name": "delegate", "arguments": { "prompt": "x".repeat(200_001) } }),
        ),
    )
    .await
    .json();
    assert_eq!(long["result"]["isError"], true);
    assert!(
        long["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("prompt is too long")
    );

    // MCP 2026-07-28: stateless; the headers must match the body.
    let headers = |method: &str, name: Option<&str>| {
        let mut h = vec![
            ("MCP-Protocol-Version", "2026-07-28".to_owned()),
            ("Mcp-Method", method.to_owned()),
        ];
        if let Some(name) = name {
            h.push(("Mcp-Name", name.to_owned()));
        }
        h
    };
    let discover = mcp(
        headers("server/discover", None),
        modern("server/discover", json!({})),
    )
    .await
    .json();
    assert_eq!(
        discover["result"]["supportedVersions"],
        json!(["2026-07-28"])
    );
    assert_eq!(discover["result"]["resultType"], "complete");
    assert_eq!(
        discover["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
        "proofkin"
    );
    let plan = "say @worker say hi";
    let check = mcp(
        headers("tools/call", Some("check_plan")),
        modern(
            "tools/call",
            json!({ "name": "check_plan", "arguments": { "plan": plan } }),
        ),
    )
    .await
    .json();
    assert_eq!(check["result"]["structuredContent"]["ok"], true, "{check}");
    // Base64 in Mcp-Name is decoded before it is compared.
    let ran = mcp(
        headers("tools/call", Some("=?base64?cnVuX3BsYW4=?=")),
        modern(
            "tools/call",
            json!({ "name": "run_plan", "arguments": { "plan": plan, "wait_seconds": 30 } }),
        ),
    )
    .await
    .json();
    let run = &ran["result"]["structuredContent"];
    assert_eq!(run["state"], "completed", "{ran}");
    assert_eq!(run["steps"][0]["output"], "echo: say hi");
    assert_eq!(run["steps"][0]["evidence"], "verified");
    let report = mcp(
        headers("tools/call", Some("get_run")),
        modern(
            "tools/call",
            json!({ "name": "get_run", "arguments": { "run_id": run["run_id"] } }),
        ),
    )
    .await
    .json();
    assert!(
        report["result"]["structuredContent"]["report"]
            .as_str()
            .unwrap()
            .contains("say"),
        "{report}"
    );
    let mismatch = mcp(
        headers("tools/call", Some("delegate")),
        modern("tools/call", json!({ "name": "list_peers" })),
    )
    .await;
    assert_eq!(mismatch.status, 400);
    assert_eq!(mismatch.json()["error"]["code"], -32020);
    let mut old = headers("tools/list", None);
    old[0].1 = "2099-01-01".into();
    let mut body: Value = serde_json::from_str(&modern("tools/list", json!({}))).unwrap();
    body["params"]["_meta"]["io.modelcontextprotocol/protocolVersion"] = json!("2099-01-01");
    let unsupported = mcp(old, body.to_string()).await;
    assert_eq!(unsupported.status, 400);
    let unsupported = unsupported.json();
    assert_eq!(unsupported["error"]["code"], -32022);
    assert!(
        unsupported["error"]["data"]["supported"]
            .as_array()
            .unwrap()
            .contains(&json!("2025-11-25"))
    );
    let unknown = mcp(headers("ping", None), modern("ping", json!({}))).await;
    assert_eq!(unknown.status, 404, "ping is gone in 2026-07-28");
    let stream = request(
        port,
        "GET",
        "/mcp",
        &[("Authorization", auth.as_str())],
        None,
    )
    .await;
    assert_eq!(stream.status, 405);

    // Spec Kit features become plans over MCP and in the UI, the spec saved as knowledge.
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let feature = fixtures.join("speckit/specs/001-photo-albums");
    let imported = mcp(
        vec![],
        rpc(
            "tools/call",
            json!({ "name": "import_spec", "arguments": { "path": feature, "peer": "worker" } }),
        ),
    )
    .await
    .json();
    let imported = &imported["result"]["structuredContent"];
    assert!(
        imported["plan"]
            .as_str()
            .unwrap()
            .contains("claim: US-1 User story 1"),
        "{imported}"
    );
    assert_eq!(imported["knowledge"], json!(["spec-001-photo-albums"]));
    assert!(
        profiles
            .join("boss/knowledge/spec-001-photo-albums.md")
            .is_file()
    );
    let several = admin(
        "POST",
        "/api/playbooks/implement-speckit",
        Some(json!({ "vars": { "spec": fixtures, "peer": "worker" } }).to_string()),
    )
    .await;
    assert_eq!(several.status, 400);
    assert_eq!(several.json()["candidates"].as_array().unwrap().len(), 2);
    let filled = admin(
        "POST",
        "/api/playbooks/implement-speckit",
        Some(
            json!({ "vars": { "spec": fixtures.join("openspec/openspec/changes/add-dark-mode"), "peer": "worker" } })
                .to_string(),
        ),
    )
    .await
    .json();
    assert!(
        filled["text"]
            .as_str()
            .unwrap()
            .contains("claim: ui-1 Theme selection")
    );
    assert!(
        filled["notes"][0]
            .as_str()
            .unwrap()
            .starts_with("OpenSpec change openspec/changes/add-dark-mode")
    );

    // A2A: the agent card says where and how; version 1.0 only.
    let card = request(port, "GET", "/.well-known/agent-card.json", &[], None)
        .await
        .json();
    assert_eq!(
        card["supportedInterfaces"][0]["url"],
        format!("http://127.0.0.1:{port}/a2a")
    );
    assert_eq!(card["supportedInterfaces"][0]["protocolVersion"], "1.0");
    assert_eq!(
        card["securitySchemes"]["bearer"]["httpAuthSecurityScheme"]["scheme"],
        "Bearer"
    );
    let a2a = |method: &'static str, params: Value, version: Option<&'static str>| {
        let auth = auth.clone();
        async move {
            let mut headers = vec![("Authorization", auth.as_str())];
            if let Some(v) = version {
                headers.push(("A2A-Version", v));
            }
            request(port, "POST", "/a2a", &headers, Some(&rpc(method, params)))
                .await
                .json()
        }
    };
    let message = |text: &str, meta: Value| {
        json!({ "message": {
            "messageId": "m-1", "role": "ROLE_USER", "parts": [{ "text": text }], "metadata": meta,
        }})
    };
    let v03 = a2a("SendMessage", message("hi", json!({})), None).await;
    assert_eq!(v03["error"]["code"], -32009, "no version means 0.3");
    let sent = a2a(
        "SendMessage",
        message("hello a2a", json!({ "peer": "worker" })),
        Some("1.0"),
    )
    .await;
    let task = &sent["result"]["task"];
    assert_eq!(task["status"]["state"], "TASK_STATE_COMPLETED", "{sent}");
    assert_eq!(task["artifacts"][0]["parts"][0]["text"], "echo: hello a2a");
    assert_eq!(task["history"][0]["messageId"], "m-1");
    let id = task["id"].as_str().unwrap().to_owned();
    let got = a2a("GetTask", json!({ "id": id }), Some("1.0")).await;
    assert_eq!(got["result"]["status"]["state"], "TASK_STATE_COMPLETED");
    let ended = a2a("CancelTask", json!({ "id": id }), Some("1.0")).await;
    assert_eq!(ended["error"]["code"], -32002);
    let listed = a2a(
        "ListTasks",
        json!({ "contextId": task["contextId"] }),
        Some("1.0"),
    )
    .await;
    assert_eq!(listed["result"]["totalSize"], 1);
    assert!(
        listed["result"]["tasks"][0].get("artifacts").is_none(),
        "artifacts only when asked for"
    );
    // A plan as the message text runs as a plan.
    let planned = a2a("SendMessage", message(plan, json!({})), Some("1.0")).await;
    let planned = &planned["result"]["task"];
    assert_eq!(
        planned["status"]["state"], "TASK_STATE_COMPLETED",
        "{planned}"
    );
    assert_eq!(planned["metadata"]["kind"], "plan");
    assert!(
        planned["artifacts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["artifactId"] == "evidence")
    );
    // Work that's still running can be cancelled.
    let mut hang = message("hang", json!({ "peer": "worker" }));
    hang["configuration"] = json!({ "returnImmediately": true });
    let hanging = a2a("SendMessage", hang, Some("1.0")).await;
    assert_eq!(
        hanging["result"]["task"]["status"]["state"],
        "TASK_STATE_WORKING"
    );
    let hanging_id = hanging["result"]["task"]["id"].clone();
    let cancelled = a2a("CancelTask", json!({ "id": hanging_id }), Some("1.0")).await;
    assert_eq!(
        cancelled["result"]["status"]["state"], "TASK_STATE_CANCELED",
        "{cancelled}"
    );
    let rejected = a2a(
        "SendMessage",
        message("x", json!({ "peer": "nobody" })),
        Some("1.0"),
    )
    .await;
    assert_eq!(
        rejected["result"]["task"]["status"]["state"],
        "TASK_STATE_REJECTED"
    );
    let missing = a2a("GetTask", json!({ "id": "t-nope" }), Some("1.0")).await;
    assert_eq!(missing["error"]["code"], -32001);
    let mut file = message("see attached", json!({}));
    file["message"]["parts"]
        .as_array_mut()
        .unwrap()
        .push(json!({ "url": "https://example.com/a.txt" }));
    let file = a2a("SendMessage", file, Some("1.0")).await;
    assert_eq!(file["error"]["code"], -32005);

    // Streaming: the task, then what the peer is doing, then the finished task.
    assert_eq!(card["capabilities"]["streaming"], true);
    let (head, events) = follow_stream(
        port,
        &auth,
        "SendStreamingMessage",
        message("gated", json!({ "peer": "worker" })),
        "the model is working",
    )
    .await;
    let streamed = assert_followed(&head, &events, "echo: gated");
    // The same task is looked up as usual, and there is nothing left to subscribe to.
    let got = a2a("GetTask", json!({ "id": streamed }), Some("1.0")).await;
    assert_eq!(got["result"]["status"]["state"], "TASK_STATE_COMPLETED");
    let over = a2a("SubscribeToTask", json!({ "id": streamed }), Some("1.0")).await;
    assert_eq!(over["error"]["code"], -32004, "{over}");
    assert_eq!(over["error"]["data"][0]["reason"], "UNSUPPORTED_OPERATION");
    let unknown = a2a("SubscribeToTask", json!({ "id": "t-nope" }), Some("1.0")).await;
    assert_eq!(unknown["error"]["code"], -32001);
    // A running task is followed from where it is.
    let mut gated = message("gated", json!({ "peer": "worker" }));
    gated["configuration"] = json!({ "returnImmediately": true });
    let gated = a2a("SendMessage", gated, Some("1.0")).await;
    let gated_id = gated["result"]["task"]["id"].clone();
    let (head, events) = follow_stream(
        port,
        &auth,
        "SubscribeToTask",
        json!({ "id": gated_id }),
        "the model is working",
    )
    .await;
    let subscribed = assert_followed(&head, &events, "echo: gated");
    assert_eq!(gated_id, subscribed.as_str());
    // Problems found before a stream would start are plain errors.
    let no_message = a2a("SendStreamingMessage", json!({}), Some("1.0")).await;
    assert_eq!(no_message["error"]["code"], -32602);
    let no_peer = a2a(
        "SendStreamingMessage",
        message("x", json!({ "peer": "nobody" })),
        Some("1.0"),
    )
    .await;
    assert_eq!(no_peer["error"]["code"], -32602, "{no_peer}");
    // The card says it has no extended card, so asking for one is unsupported.
    let extended = a2a("GetExtendedAgentCard", json!({}), Some("1.0")).await;
    assert_eq!(extended["error"]["code"], -32004);

    // Turned off: the token stops working at once, and the card is gone.
    let off = admin("DELETE", "/api/connect", None).await.json();
    assert_eq!(off["on"], false);
    let refused = mcp(vec![], rpc("tools/list", json!({}))).await;
    assert_eq!(refused.status, 401);
    let card = request(port, "GET", "/.well-known/agent-card.json", &[], None).await;
    assert_eq!(card.status, 404);
    boss.shutdown().await;
    worker.shutdown().await;
}

/// Posts to /mcp and reads the response to its end: its head, and the events if it is a
/// stream (a JSON body is the one event).
async fn post_mcp(port: u16, headers: &[(&str, &str)], body: &str) -> (String, Vec<Value>) {
    let mut req = format!("POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n");
    for (name, value) in headers {
        req += &format!("{name}: {value}\r\n");
    }
    req += &format!(
        "Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    tokio::time::timeout(Duration::from_secs(20), stream.read_to_end(&mut raw))
        .await
        .expect("the response ends")
        .unwrap();
    let (head, text) = split_response(raw);
    if head.to_lowercase().contains("text/event-stream") {
        return (head, sse_events(&text));
    }
    let body = serde_json::from_str(&text).unwrap_or_else(|_| panic!("JSON body: {text}"));
    (head, vec![body])
}

#[tokio::test]
async fn mcp_clients_follow_a_task_with_progress_notifications() {
    let home = shared_home();
    let (boss_key, worker_key) = (SecretKey::generate(), SecretKey::generate());
    let trust = |name: &str, key: &SecretKey, delegate: bool| TrustedPeer {
        name: name.into(),
        id: key.public(),
        addrs: Default::default(),
        can_delegate: delegate,
        tasks_per_hour: 100,
        access: Access::ReadOnly,
        shares: vec!["data".into()],
    };
    let mut worker = AgentConfig::new("slowworker", worker_key.clone());
    worker.network = Network::Local;
    worker.bind = Some("127.0.0.1:0".parse().unwrap());
    worker.trust.allow(trust("progressboss", &boss_key, true));
    worker.executor = Some(Arc::new(Echo));
    worker.shares = ShareStore::with_data(home.join("progress-data"));
    let (worker, _we) = Agent::bind(worker).await.unwrap();
    let mut config = AgentConfig::new("progressboss", boss_key);
    config.network = Network::Local;
    config.bind = Some("127.0.0.1:0".parse().unwrap());
    config.trust.allow(trust("slowworker", &worker_key, false));
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
        profile: Profile::open_in(&home.join("progress-profiles"), "progressboss").unwrap(),
        network: Network::Local,
        serving: false,
        access: Access::ReadOnly,
        workspace: home.to_path_buf(),
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
    let (port, ui_token) = rest.split_once("/#t=").unwrap();
    let port: u16 = port.parse().unwrap();
    let on = request(
        port,
        "POST",
        "/api/connect",
        &[("Authorization", &format!("Bearer {ui_token}"))],
        None,
    )
    .await
    .json();
    let auth = format!("Bearer {}", on["token"].as_str().unwrap());
    let call = |prompt: &str, token: Option<Value>| {
        let mut params = json!({
            "name": "delegate",
            "arguments": { "prompt": prompt, "peer": "slowworker", "wait_seconds": 30 },
        });
        if let Some(token) = token {
            params["_meta"] = json!({ "progressToken": token });
        }
        params
    };
    let stream = [
        ("Authorization", auth.as_str()),
        ("Accept", "application/json, text/event-stream"),
    ];

    // The legacy revision: progress notifications, then the result, then the end.
    let (head, events) = post_mcp(
        port,
        &stream,
        &rpc("tools/call", call("slow", Some(json!("t-1")))),
    )
    .await;
    assert!(
        head.to_lowercase()
            .contains("content-type: text/event-stream"),
        "{head}"
    );
    let (result, notes) = events.split_last().unwrap();
    assert!(notes.len() >= 2, "{events:?}");
    let mut last = 0;
    for note in notes {
        assert_eq!(note["method"], "notifications/progress", "{note}");
        assert_eq!(note["params"]["progressToken"], "t-1");
        let progress = note["params"]["progress"].as_u64().unwrap();
        assert!(progress > last, "progress increases: {events:?}");
        last = progress;
        assert!(
            note["params"]["message"]
                .as_str()
                .is_some_and(|m| !m.is_empty())
        );
    }
    assert!(
        notes[0]["params"]["message"]
            .as_str()
            .unwrap()
            .contains("Running on slowworker"),
        "{notes:?}"
    );
    assert_eq!(result["id"], 1);
    assert_eq!(result["result"]["isError"], false, "{result}");
    assert_eq!(result["result"]["structuredContent"]["state"], "completed");
    assert_eq!(
        result["result"]["structuredContent"]["output"],
        "echo: slow"
    );
    assert!(result["result"].get("resultType").is_none());

    // The modern revision: a numeric token, and the result with its resultType.
    let modern_stream = [
        stream[0],
        stream[1],
        ("MCP-Protocol-Version", "2026-07-28"),
        ("Mcp-Method", "tools/call"),
        ("Mcp-Name", "delegate"),
    ];
    let (head, events) = post_mcp(
        port,
        &modern_stream,
        &modern("tools/call", call("slow", Some(json!(7)))),
    )
    .await;
    assert!(
        head.to_lowercase()
            .contains("content-type: text/event-stream"),
        "{head}"
    );
    let (result, notes) = events.split_last().unwrap();
    assert!(notes.len() >= 2, "{events:?}");
    assert!(notes.iter().all(|n| n["params"]["progressToken"] == 7));
    assert_eq!(result["result"]["resultType"], "complete");
    assert_eq!(result["result"]["structuredContent"]["state"], "completed");

    // Without a token, or without text/event-stream in Accept: one JSON body.
    let (head, events) = post_mcp(port, &stream, &rpc("tools/call", call("plain", None))).await;
    assert!(head.to_lowercase().contains("application/json"), "{head}");
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0]["result"]["structuredContent"]["output"],
        "echo: plain"
    );
    let json_only = [
        ("Authorization", auth.as_str()),
        ("Accept", "application/json"),
    ];
    let (head, events) = post_mcp(
        port,
        &json_only,
        &rpc("tools/call", call("plain", Some(json!("t-2")))),
    )
    .await;
    assert!(head.to_lowercase().contains("application/json"), "{head}");
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0]["result"]["structuredContent"]["output"],
        "echo: plain"
    );
    boss.shutdown().await;
    worker.shutdown().await;
}
