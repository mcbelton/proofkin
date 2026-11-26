//! Other agents hand work to this agent's peers through it: an MCP server (`POST
//! /mcp`, Streamable HTTP) and an A2A agent (`/.well-known/agent-card.json`, JSON-RPC
//! at `POST /a2a`). Both take the profile's integration token (`proofkin connect`),
//! never the web UI's own, and neither runs anything on this machine: the work goes
//! to connected peers and shows up as tasks, like the owner's own, with its evidence.
//!
//! MCP: the stateless 2026-07-28 revision (the version in each request's `_meta`,
//! checked against the `MCP-Protocol-Version`, `Mcp-Method` and `Mcp-Name` headers;
//! `server/discover`), and the `initialize`-based revisions before it. A2A: version
//! 1.0 (`A2A-Version: 1.0`): `SendMessage` (waits for the result unless
//! `returnImmediately`), `SendStreamingMessage` and `SubscribeToTask` (server-sent
//! events), `GetTask`, `ListTasks` and `CancelTask`.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;

use super::{
    ApiError, AppState, Shared, TaskBody, TaskView, cancel_running, internal, now_ms, plan_check,
    run_details, start_plan, start_task,
};
use crate::llm::Provider;

/// MCP revisions served statelessly (the version in each request's `_meta`).
const MCP_MODERN: &[&str] = &["2026-07-28"];
/// Earlier MCP revisions, which start with `initialize` (newest first).
const MCP_LEGACY: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26"];
const A2A_VERSION: &str = "1.0";
/// How long an MCP tool call waits for its work to finish, by default and at most.
const DEFAULT_WAIT: Duration = Duration::from_secs(30);
const MAX_WAIT: Duration = Duration::from_secs(300);
/// How long a blocking A2A `SendMessage` waits at most.
const MAX_BLOCK: Duration = Duration::from_secs(24 * 3600);
/// How often an A2A stream says it is still open while nothing changes.
const KEEP_ALIVE: Duration = Duration::from_secs(15);
/// Work remembered for `get_task`, `GetTask` and `ListTasks` (oldest forgotten first).
const MAX_JOBS: usize = 500;
/// Longest text (a prompt, a plan) other agents may send.
const MAX_TEXT: usize = 200_000;
/// How often the integration token is read again (a new one, or none, takes effect
/// without a restart).
const TOKEN_TTL: Duration = Duration::from_secs(5);

const INSTRUCTIONS: &str = "Proofkin hands work to the agents of your teammates (peers), \
peer to peer, and proves it. list_peers: who can take work, with the folders they share \
and their checks. delegate: run a task on a peer (its result is signed by that peer). \
run_plan: run an evidence plan across peers (check it first with check_plan); its \
claims are proven only by passing checks or sign-offs. get_task: wait for a result. \
get_run: the evidence report. import_spec: turn a Spec Kit feature or OpenSpec change \
into a plan. factory_*: design agents with the owner's agent factory: draft one \
(factory_draft), check it (factory_check), and evaluate it (factory_evaluate: the estimate \
first, then confirm) and follow it (factory_status); releasing, deploying and approving \
agents stay with the owner.";

/// Other agents' access and the work they started.
#[derive(Default)]
pub(super) struct Connect {
    /// The integration token (None: connecting is off) and when it was read.
    token: Mutex<Option<(Option<String>, Instant)>>,
    jobs: Mutex<Vec<Job>>,
}

/// Work another agent started: a task on a peer, or a plan.
#[derive(Clone)]
struct Job {
    id: String,
    context_id: String,
    via: &'static str,
    work: Work,
    created_ms: u128,
    /// The A2A message that asked for it.
    message: Option<Value>,
}

#[derive(Clone)]
enum Work {
    Task(u64),
    Plan {
        steps: Vec<(String, u64)>,
        /// The saved run, once the plan has ended.
        outcome: Arc<Mutex<Option<crate::schedule::Outcome>>>,
    },
    /// It couldn't start: why (no such peer, a plan with problems).
    Rejected(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JobState {
    Working,
    Completed,
    Failed,
    Canceled,
    Rejected,
}

impl JobState {
    fn name(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Canceled => "canceled",
            Self::Rejected => "rejected",
        }
    }

    fn a2a(self) -> &'static str {
        match self {
            Self::Working => "TASK_STATE_WORKING",
            Self::Completed => "TASK_STATE_COMPLETED",
            Self::Failed => "TASK_STATE_FAILED",
            Self::Canceled => "TASK_STATE_CANCELED",
            Self::Rejected => "TASK_STATE_REJECTED",
        }
    }
}

impl Connect {
    fn add(&self, job: Job) {
        let mut jobs = self.jobs.lock().unwrap();
        jobs.push(job);
        if jobs.len() > MAX_JOBS {
            jobs.remove(0);
        }
    }

    fn get(&self, id: &str) -> Option<Job> {
        self.jobs
            .lock()
            .unwrap()
            .iter()
            .find(|j| j.id == id)
            .cloned()
    }
}

impl Shared {
    /// The integration token, if connecting other agents is on.
    fn integration_token(&self) -> Option<String> {
        let mut cached = self.connect.token.lock().unwrap();
        match &*cached {
            Some((token, at)) if at.elapsed() < TOKEN_TTL => token.clone(),
            _ => {
                let token = self.info.profile.integration_token().ok().flatten();
                *cached = Some((token.clone(), Instant::now()));
                token
            }
        }
    }

    fn cache_integration_token(&self, token: Option<String>) {
        *self.connect.token.lock().unwrap() = Some((token, Instant::now()));
    }
}

/// Compares secrets in time that doesn't depend on where they differ.
fn same_secret(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

fn new_id(prefix: &str) -> String {
    let bytes = iroh::SecretKey::generate().to_bytes();
    format!("{prefix}{}", data_encoding::HEXLOWER.encode(&bytes[..12]))
}

/// `/mcp` and `/a2a` need the integration token, and refuse other sites' pages.
pub(super) async fn require_integration_token(
    State(s): State<AppState>,
    headers: HeaderMap,
    req: Request,
    next: Next,
) -> Response {
    if let Some(origin) = headers.get(header::ORIGIN) {
        let port = s.port.load(Ordering::Relaxed);
        let own = [
            format!("http://127.0.0.1:{port}"),
            format!("http://localhost:{port}"),
        ];
        if !origin.to_str().is_ok_and(|o| own.iter().any(|x| x == o)) {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({ "error": "requests from other sites' pages are refused" })),
            )
                .into_response();
        }
    }
    let bearer = header(&headers, "authorization")
        .and_then(|h| h.strip_prefix("Bearer ").map(|t| t.trim().to_owned()));
    let token = s.integration_token();
    if let (Some(token), Some(bearer)) = (&token, &bearer)
        && same_secret(token.as_bytes(), bearer.as_bytes())
    {
        return next.run(req).await;
    }
    let error = match token {
        None => format!(
            "connecting other agents is off for profile '{0}': turn it on in the web UI (Connect other agents), or run: proofkin connect --profile {0}",
            s.info.profile.name
        ),
        Some(_) => "missing or wrong integration token: send Authorization: Bearer <token> (proofkin connect shows it)".to_owned(),
    };
    let mut res = (StatusCode::UNAUTHORIZED, Json(json!({ "error": error }))).into_response();
    res.headers_mut()
        .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    res
}

/// Where other agents connect.
fn endpoints(s: &Shared) -> Value {
    let port = s.port.load(Ordering::Relaxed);
    let base = format!("http://127.0.0.1:{port}");
    json!({
        "mcp": format!("{base}/mcp"),
        "a2a": format!("{base}/a2a"),
        "card": format!("{base}/.well-known/agent-card.json"),
        "fixed_port": s.fixed_port,
        "port": port,
        "profile": s.info.profile.name,
    })
}

#[derive(Deserialize)]
pub(super) struct ConnectQuery {
    #[serde(default)]
    reveal: bool,
}

/// Whether other agents can connect and where (`?reveal=true`: and the token).
pub(super) async fn state(
    State(s): State<AppState>,
    Query(q): Query<ConnectQuery>,
) -> Result<Json<Value>, ApiError> {
    let token = s.info.profile.integration_token().map_err(internal)?;
    s.cache_integration_token(token.clone());
    let mut v = endpoints(&s);
    v["on"] = json!(token.is_some());
    v["worker"] = json!(s.info.profile.config.owner.is_some());
    if q.reveal {
        v["token"] = json!(token);
    }
    Ok(Json(v))
}

/// Turns connecting on with a new token (the old one stops working).
pub(super) async fn turn_on(State(s): State<AppState>) -> Result<Json<Value>, ApiError> {
    let token = s.info.profile.new_integration_token().map_err(internal)?;
    s.cache_integration_token(Some(token.clone()));
    s.system(
        "info",
        "Other agents can connect (MCP and A2A), with a new token",
    );
    let mut v = endpoints(&s);
    v["on"] = json!(true);
    v["token"] = json!(token);
    Ok(Json(v))
}

pub(super) async fn turn_off(State(s): State<AppState>) -> Result<Json<Value>, ApiError> {
    s.info
        .profile
        .delete_integration_token()
        .map_err(internal)?;
    s.cache_integration_token(None);
    s.system("info", "Other agents can no longer connect");
    let mut v = endpoints(&s);
    v["on"] = json!(false);
    Ok(Json(v))
}

fn reply(status: StatusCode, body: Value) -> Response {
    (status, Json(body)).into_response()
}

fn rpc_ok(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn rpc_err(id: &Value, code: i64, message: impl Into<String>, data: Option<Value>) -> Value {
    let mut error = json!({ "code": code, "message": message.into() });
    if let Some(data) = data {
        error["data"] = data;
    }
    json!({ "jsonrpc": "2.0", "id": id, "error": error })
}

// ---------- MCP ----------

/// `GET` and `DELETE /mcp`: there are no sessions or server-sent event streams.
pub(super) async fn mcp_no_stream() -> Response {
    let mut res = (
        StatusCode::METHOD_NOT_ALLOWED,
        Json(json!({ "error": "this MCP server has no sessions or event stream: POST each request" })),
    )
        .into_response();
    res.headers_mut()
        .insert(header::ALLOW, HeaderValue::from_static("POST"));
    res
}

fn server_info() -> Value {
    json!({ "name": "proofkin", "title": "Proofkin", "version": env!("CARGO_PKG_VERSION") })
}

fn all_versions() -> Vec<&'static str> {
    MCP_MODERN.iter().chain(MCP_LEGACY).copied().collect()
}

/// A header value, decoded if it uses the `=?base64?...?=` form.
fn decode_header(value: &str) -> Option<String> {
    match value
        .strip_prefix("=?base64?")
        .and_then(|v| v.strip_suffix("?="))
    {
        Some(encoded) => data_encoding::BASE64
            .decode(encoded.as_bytes())
            .ok()
            .and_then(|b| String::from_utf8(b).ok()),
        None => Some(value.to_owned()),
    }
}

pub(super) async fn mcp(State(s): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let msg: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return reply(
                StatusCode::BAD_REQUEST,
                rpc_err(&Value::Null, -32700, format!("Parse error: {e}"), None),
            );
        }
    };
    if !msg.is_object() {
        return reply(
            StatusCode::BAD_REQUEST,
            rpc_err(
                &Value::Null,
                -32600,
                "Invalid Request: send one JSON-RPC message per request (no batches)",
                None,
            ),
        );
    }
    let (Some(method), Some(id)) = (
        msg.get("method").and_then(Value::as_str),
        msg.get("id").cloned(),
    ) else {
        // A notification (notifications/initialized) or a response: nothing to answer.
        return StatusCode::ACCEPTED.into_response();
    };
    let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));
    let version = params
        .get("_meta")
        .and_then(|m| m.get("io.modelcontextprotocol/protocolVersion"))
        .and_then(Value::as_str);
    match version {
        Some(version) => mcp_modern(&s, &headers, &id, method, &params, version).await,
        None => mcp_legacy(&s, &headers, &id, method, &params).await,
    }
}

/// A request of the stateless revision: its headers must match its body.
async fn mcp_modern(
    s: &AppState,
    headers: &HeaderMap,
    id: &Value,
    method: &str,
    params: &Value,
    version: &str,
) -> Response {
    let mismatch = |what: String| {
        reply(
            StatusCode::BAD_REQUEST,
            rpc_err(id, -32020, format!("Header mismatch: {what}"), None),
        )
    };
    match header(headers, "mcp-protocol-version") {
        None => return mismatch("the MCP-Protocol-Version header is missing".into()),
        Some(h) if h != version => {
            return mismatch(format!(
                "MCP-Protocol-Version header '{h}' does not match body value '{version}'"
            ));
        }
        _ => {}
    }
    if !MCP_MODERN.contains(&version) {
        return reply(
            StatusCode::BAD_REQUEST,
            rpc_err(
                id,
                -32022,
                "Unsupported protocol version",
                Some(json!({ "supported": all_versions(), "requested": version })),
            ),
        );
    }
    match header(headers, "mcp-method") {
        None => return mismatch("the Mcp-Method header is missing".into()),
        Some(h) if h != method => {
            return mismatch(format!(
                "Mcp-Method header '{h}' does not match body value '{method}'"
            ));
        }
        _ => {}
    }
    if method == "tools/call" {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match header(headers, "mcp-name").map(|h| decode_header(&h)) {
            None => return mismatch("the Mcp-Name header is missing".into()),
            Some(h) if h.as_deref() != Some(name) => {
                return mismatch(format!(
                    "Mcp-Name header does not match body value '{name}'"
                ));
            }
            _ => {}
        }
    }
    let result = match method {
        "server/discover" => json!({
            "supportedVersions": MCP_MODERN,
            "capabilities": { "tools": {} },
            "instructions": INSTRUCTIONS,
            "ttlMs": 3_600_000,
            "cacheScope": "private",
        }),
        "tools/list" => {
            let mut list = tools();
            list["ttlMs"] = json!(3_600_000);
            list["cacheScope"] = json!("private");
            list
        }
        "tools/call" => {
            if let Some(stream) = progress_stream(s, headers, id, params, true) {
                return stream;
            }
            if let Some(stream) = factory_stream(s, headers, id, params, true).await {
                return stream;
            }
            match call_tool(s, params).await {
                Ok(result) => result,
                Err(message) => return reply(StatusCode::OK, rpc_err(id, -32602, message, None)),
            }
        }
        _ => {
            return reply(
                StatusCode::NOT_FOUND,
                rpc_err(id, -32601, format!("Method not found: {method}"), None),
            );
        }
    };
    reply(StatusCode::OK, rpc_ok(id, modern_result(result)))
}

/// A result of the stateless revision.
fn modern_result(mut result: Value) -> Value {
    result["resultType"] = json!("complete");
    result["_meta"] = json!({ "io.modelcontextprotocol/serverInfo": server_info() });
    result
}

/// The progress token of a request (a string or a number).
fn progress_token(params: &Value) -> Option<Value> {
    params
        .get("_meta")
        .and_then(|m| m.get("progressToken"))
        .filter(|t| t.is_string() || t.is_number())
        .cloned()
}

/// Answers a `tools/call` of delegate, run_plan or get_task that waits with a stream of
/// progress notifications (see [`follow_progress`]), when the client asks for one: it
/// sent a progress token and accepts `text/event-stream`. Other calls get `None`, and
/// are answered with JSON.
fn progress_stream(
    s: &AppState,
    headers: &HeaderMap,
    id: &Value,
    params: &Value,
    modern: bool,
) -> Option<Response> {
    let token = progress_token(params)?;
    let accepts_events = header(headers, "accept")
        .is_some_and(|a| a.to_ascii_lowercase().contains("text/event-stream"));
    if !accepts_events {
        return None;
    }
    let name = params.get("name").and_then(Value::as_str)?;
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    if !args.is_object() || !matches!(name, "delegate" | "run_plan" | "get_task") {
        return None;
    }
    let limit = wait_arg(&args);
    if limit.is_zero() {
        return None;
    }
    // A call that can't start is answered with JSON, as its error.
    let job = start_job(s, name, &args).ok()?;
    let (tx, rx) = mpsc::channel(16);
    let shared = s.clone();
    let id = id.clone();
    tokio::spawn(async move {
        follow_progress(&shared, &id, &token, &job, limit, modern, tx).await;
    });
    let events = ReceiverStream::new(rx)
        .map(|event: Value| Ok::<_, Infallible>(SseEvent::default().data(event.to_string())));
    Some(
        Sse::new(events)
            .keep_alive(KeepAlive::new().interval(KEEP_ALIVE))
            .into_response(),
    )
}

/// Sends a `notifications/progress` each time `job`'s status changes (its state, or what
/// its steps' peers say they are doing), then the tool call's response when the work ends
/// or `limit` is up. Returns when that is sent, or the client has gone away (`tx` closed).
async fn follow_progress(
    s: &Shared,
    id: &Value,
    token: &Value,
    job: &Job,
    limit: Duration,
    modern: bool,
    tx: mpsc::Sender<Value>,
) {
    let deadline = tokio::time::Instant::now() + limit;
    let mut seen: Option<String> = None;
    let mut count = 0u64;
    loop {
        // Made before looking, so a change in between still wakes it.
        let changed = s.changed.notified();
        tokio::pin!(changed);
        let state = job_state(s, job);
        let task = a2a_task(s, job, Some(0), false);
        let text = task["status"]["message"]["parts"][0]["text"]
            .as_str()
            .unwrap_or(state.name())
            .to_owned();
        let now = format!("{}\n{text}", state.name());
        if seen.as_ref() != Some(&now) {
            seen = Some(now);
            count += 1;
            let note = json!({
                "jsonrpc": "2.0",
                "method": "notifications/progress",
                "params": { "progressToken": token, "progress": count, "message": text },
            });
            if tx.send(note).await.is_err() {
                return;
            }
        }
        if state != JobState::Working {
            break;
        }
        tokio::select! {
            _ = &mut changed => {}
            _ = tokio::time::sleep_until(deadline) => break,
            _ = tx.closed() => return,
        }
    }
    let result = tool_result(Ok(job_json(s, job)));
    let result = if modern {
        modern_result(result)
    } else {
        result
    };
    let _ = tx.send(rpc_ok(id, result)).await;
}

/// A request of a revision that starts with `initialize` (no sessions are kept: every
/// request stands on its own).
async fn mcp_legacy(
    s: &AppState,
    headers: &HeaderMap,
    id: &Value,
    method: &str,
    params: &Value,
) -> Response {
    if method != "initialize"
        && let Some(v) = header(headers, "mcp-protocol-version")
        && !MCP_LEGACY.contains(&v.as_str())
    {
        return reply(
            StatusCode::BAD_REQUEST,
            rpc_err(
                id,
                -32600,
                format!(
                    "unsupported MCP-Protocol-Version '{v}' (supported: {})",
                    all_versions().join(", ")
                ),
                None,
            ),
        );
    }
    let result = match method {
        "initialize" => {
            let asked = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let version = MCP_LEGACY
                .iter()
                .find(|v| **v == asked)
                .unwrap_or(&MCP_LEGACY[0]);
            json!({
                "protocolVersion": version,
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": server_info(),
                "instructions": INSTRUCTIONS,
            })
        }
        "ping" => json!({}),
        "tools/list" => tools(),
        "tools/call" => {
            if let Some(stream) = progress_stream(s, headers, id, params, false) {
                return stream;
            }
            if let Some(stream) = factory_stream(s, headers, id, params, false).await {
                return stream;
            }
            match call_tool(s, params).await {
                Ok(result) => result,
                Err(message) => return reply(StatusCode::OK, rpc_err(id, -32602, message, None)),
            }
        }
        _ => {
            return reply(
                StatusCode::OK,
                rpc_err(id, -32601, format!("Method not found: {method}"), None),
            );
        }
    };
    reply(StatusCode::OK, rpc_ok(id, result))
}

/// The tools, in a fixed order.
fn tools() -> Value {
    let tool = |name: &str,
                title: &str,
                description: &str,
                properties: Value,
                required: &[&str],
                read_only: bool| {
        let mut schema = json!({
            "type": "object",
            "properties": properties,
            "additionalProperties": false,
        });
        if !required.is_empty() {
            schema["required"] = json!(required);
        }
        json!({
            "name": name,
            "title": title,
            "description": description,
            "inputSchema": schema,
            "annotations": { "readOnlyHint": read_only, "openWorldHint": true },
        })
    };
    let wait = json!({
        "type": "integer", "minimum": 0, "maximum": 300,
        "description": "Seconds to wait for it to finish (default 30, at most 300); if it's still running, call get_task",
    });
    let task_id = json!({ "type": "string", "description": "The task_id that delegate or run_plan returned" });
    let text = |description: &str| json!({ "type": "string", "description": description });
    json!({ "tools": [
        tool("list_peers", "List peers",
            "The connected peers (teammates' agents): the providers they run tasks with (copilot, claude, codex), the folders they share with this agent (access, checks, worktrees, MCP servers) and what they are for; and the squads.",
            json!({}), &[], true),
        tool("delegate", "Delegate a task to a peer",
            "Run a task on a peer's agent, in a folder it shares with this agent; the result is signed by that peer. Without a peer, any connected peer that runs tasks (with the provider, if given) takes it. With a progress token (_meta.progressToken) and Accept listing text/event-stream, it streams notifications/progress while the task runs, then the result.",
            json!({
                "prompt": text("What to do"),
                "peer": text("The peer's name (see list_peers), or #squad: the squad's least busy member that can run it"),
                "folder": text("A folder the peer shares (default: its data folder)"),
                "provider": { "type": "string", "enum": ["copilot", "claude", "codex"], "description": "Which provider runs it (default: the peer's first)" },
                "model": text("The model it runs on (list_peers says which ones each peer allows; default: the peer's default)"),
                "effort": { "type": "string", "enum": ["minimal", "low", "medium", "high", "xhigh", "max"], "description": "The reasoning effort it works at (default: the peer's default; a model that supports fewer takes the nearest below)" },
                "mcp": { "type": "array", "items": { "type": "string" }, "description": "MCP servers of that folder the model may use" },
                "wait_seconds": wait,
            }),
            &["prompt"], false),
        tool("get_task", "Get a task's result",
            "The state and result of a task or plan started with delegate or run_plan (with each step's result and the evidence verdict for a plan). Waits for it to finish, up to wait_seconds. If the call has a progress token (_meta.progressToken) and Accept lists text/event-stream, the answer is a stream of notifications/progress (the status each time it changes) and then the result.",
            json!({ "task_id": task_id, "wait_seconds": wait }),
            &["task_id"], true),
        tool("cancel_task", "Cancel a task",
            "Cancel a running task or plan started with delegate or run_plan; the peers stop their work.",
            json!({ "task_id": task_id }),
            &["task_id"], false),
        tool("check_plan", "Check a plan",
            "Check a plan before running it: its syntax, and whether the peers, folders, checks, squads, skills and knowledge it uses are there. Lists each problem (and how to fix it).",
            json!({ "plan": text("The plan text") }),
            &["plan"], true),
        tool("run_plan", "Run an evidence plan",
            "Run a plan across peers and squads in dependency order. A plan is claim lines (claim: ID what must be true), then one step per line: name [after a,b] @peer[:folder] or #squad [skill=s] [verify=check] [attempts=N] [apply=step] [claims=IDs] what to do; `approve @person` steps are sign-offs. Claims are proven only by passing checks or sign-offs; the run's evidence report is saved (get_run). With a progress token (_meta.progressToken) and Accept listing text/event-stream, it streams notifications/progress while the plan runs, then the result.",
            json!({ "plan": text("The plan text"), "wait_seconds": wait }),
            &["plan"], false),
        tool("get_run", "Get an evidence report",
            "The evidence report of a finished plan run (the run_id get_task gives): each claim's verdict, the steps' evidence and their re-checked signatures.",
            json!({ "run_id": text("The run's id") }),
            &["run_id"], true),
        tool("import_spec", "Import a Spec Kit or OpenSpec spec",
            "Turn a Spec Kit feature (specs/NNN-name) or OpenSpec change (openspec/changes/<id>) on this computer into a plan that a peer implements: a claim per user story, requirement and success criterion, proven by the folder's tests or a sign-off. The spec is saved as knowledge the steps get. Returns the plan: check it with check_plan, run it with run_plan.",
            json!({
                "path": text("The feature's folder (or a project folder with one feature)"),
                "peer": text("The peer that implements it"),
                "folder": text("The folder it works in (default: app)"),
                "reviewers": text("A squad that reviews it independently (default: qa)"),
                "test": text("The folder's check that proves it (default: test)"),
                "approver": text("Who signs it off (default: me, this agent's owner)"),
            }),
            &["path", "peer"], false),
        tool("factory_templates", "List agent templates",
            "The agent factory's built-in templates: what each agent is for, the most access it asks for, how many eval cases it starts with and what its evaluation needs, if anything.",
            json!({}), &[], true),
        tool("factory_list", "List the factory's agents",
            "The agents and drafts of this profile's agent factory: each one's state, its draft's errors and warnings, and its latest evaluation.",
            json!({}), &[], true),
        tool("factory_draft", "Draft an agent",
            "Start a new agent: from a template, or written by the owner's model from a description (read-only, in an empty folder: it grants nothing), or both. Returns the draft: its blueprint, problems with their fixes, and how it was written. Releasing, deploying and approving stay with the owner.",
            json!({
                "name": text("The agent's name (letters, digits, '-', '_' and '.'); default: the template's"),
                "description": text("What it should do, in plain words (at most 4000 characters)"),
                "template": text("A template to start from (see factory_templates)"),
                "provider": { "type": "string", "enum": ["copilot", "claude", "codex"], "description": "Whose model writes it (default: the first the profile is signed in to)" },
            }),
            &[], false),
        tool("factory_check", "Check an agent's draft",
            "Check a draft: each problem with its fix, least-privilege warnings, and what it asks for resolved in the folders it is deployed to (or those given). Checking grants nothing.",
            json!({
                "name": text("The draft's name"),
                "folders": { "type": "array", "items": { "type": "string" }, "description": "Folders to resolve its capabilities in (default: its deploy folders)" },
            }),
            &["name"], true),
        tool("factory_evaluate", "Evaluate an agent",
            "Without confirm: the estimate of evaluating a draft (cases with the built-in attacks, runs, model calls, minutes); nothing runs. With confirm: runs it on the owner's evaluation worker, each case a few times on each candidate model, and returns the evaluation (wait_seconds waits for it; then call factory_status). With a progress token (_meta.progressToken) and Accept listing text/event-stream, it streams notifications/progress while it runs, then the result.",
            json!({
                "name": text("The draft's name"),
                "candidates": { "type": "array", "items": { "type": "string" }, "description": "Models to evaluate it on, as provider:model[:effort] (default: those its blueprint lists, else its providers' defaults)" },
                "repetitions": { "type": "integer", "minimum": 1, "maximum": 5, "description": "Runs per case and model (default: its suite's, else 3)" },
                "confirm": { "type": "boolean", "description": "Run it (default false: only the estimate)" },
                "wait_seconds": wait,
            }),
            &["name"], false),
        tool("factory_status", "Get an evaluation",
            "An evaluation's state and report: its progress, the release gate with the reasons it failed, each model's results and the failing runs. Waits for it to end, up to wait_seconds. With a progress token (_meta.progressToken) and Accept listing text/event-stream, it streams notifications/progress while it runs, then the result.",
            json!({ "id": text("The evaluation's id (ev-...)"), "wait_seconds": wait }),
            &["id"], true),
    ]})
}

/// Who the factory records as having done what another agent asked over MCP.
const BY_AGENT: &str = "another agent (MCP)";

/// The agent factory's lifecycle stays with its owner: these tools aren't offered, and a
/// call is refused with the owner's command (TM-10). So do what peers are trusted with
/// (publishing, importing), what runs on a schedule (drift watches) and what the owner's
/// model writes over a draft (improvements). Each tool, with what it would do and the
/// owner's command (`{agent}`: the agent it names).
const OWNER_ONLY: &[(&str, &str, &str)] = &[
    ("factory_release", "Releasing an agent", "release {agent}"),
    ("factory_deploy", "Deploying an agent", "deploy {agent}"),
    (
        "factory_rollback",
        "Rolling an agent back",
        "rollback {agent}",
    ),
    ("factory_retire", "Retiring an agent", "retire {agent}"),
    (
        "factory_deprecate",
        "Deprecating an agent",
        "deprecate {agent}",
    ),
    (
        "factory_approve",
        "Approving what an agent asks for",
        "approve {agent}",
    ),
    (
        "factory_publish",
        "Publishing an agent to peers",
        "publish {agent} --to PEER",
    ),
    (
        "factory_import",
        "Importing a peer's agent",
        "import PUBLISHER/{agent}@VERSION",
    ),
    (
        "factory_watch",
        "Watching an agent for drift",
        "watch {agent} --when \"daily 03:00\"",
    ),
    ("factory_improve", "Improving an agent", "improve {agent}"),
];

/// Why lifecycle tool `name` is refused: what stays with the owner, and their command.
fn owner_only(s: &Shared, name: &str, args: &Value) -> Option<String> {
    let (_, what, command) = OWNER_ONLY.iter().find(|(tool, ..)| *tool == name)?;
    let agent = opt_text(args, "name").unwrap_or_else(|| "NAME".into());
    Some(format!(
        "{what} stays with its owner, who runs: proofkin factory {} --profile {}",
        command.replace("{agent}", &agent),
        s.info.profile.name
    ))
}

/// Candidates as tools name them: `provider:model[:effort]` text, or objects.
fn candidates_arg(args: &Value) -> Result<Vec<crate::factory::routing::Route>, String> {
    args.get("candidates")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|c| match c.as_str() {
            Some(text) => {
                crate::factory::runner::parse_candidate(text).map_err(|e| format!("{e:#}"))
            }
            None => {
                serde_json::from_value(c.clone()).map_err(|e| format!("a candidate isn't one: {e}"))
            }
        })
        .collect()
}

async fn factory_draft(s: &AppState, args: &Value) -> Result<Value, String> {
    let provider = match args.get("provider") {
        None | Some(Value::Null) => None,
        Some(p) => Some(
            serde_json::from_value(p.clone())
                .map_err(|_| "provider is copilot, claude or codex".to_owned())?,
        ),
    };
    let body = super::factory::NewDraft {
        description: args
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_owned),
        template: opt_text(args, "template"),
        name: opt_text(args, "name"),
        provider,
    };
    let draft = super::factory::new_draft(s, body, BY_AGENT)
        .await
        .map_err(api_error)?;
    s.system(
        "info",
        format!(
            "Another agent (MCP) drafted {}",
            draft["blueprint"]["name"].as_str().unwrap_or("an agent")
        ),
    );
    Ok(draft)
}

/// Starts (with `confirm`) or estimates evaluating draft `name`, as `args` say.
async fn factory_start(s: &AppState, args: &Value) -> Result<Value, String> {
    let name = text_arg(args, "name")?;
    let body = super::factory::EvaluationBody {
        candidates: candidates_arg(args)?,
        repetitions: args
            .get("repetitions")
            .and_then(Value::as_u64)
            .map(|n| n.min(u64::from(u32::MAX)) as u32),
        peer: None,
        confirm: args
            .get("confirm")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    };
    let (_, value) = super::factory::evaluate(s, &name, body, BY_AGENT)
        .await
        .map_err(api_error)?;
    Ok(value)
}

async fn factory_evaluate(s: &AppState, args: &Value) -> Result<Value, String> {
    let started = factory_start(s, args).await?;
    match started["id"].as_str() {
        Some(id) => {
            wait_evaluation(s, id, wait_arg(args)).await;
            super::factory::evaluation(s, id).map_err(api_error)
        }
        // The estimate: nothing ran.
        None => Ok(started),
    }
}

async fn factory_status(s: &AppState, args: &Value) -> Result<Value, String> {
    let id = text_arg(args, "id")?;
    super::factory::evaluation(s, &id).map_err(api_error)?;
    wait_evaluation(s, &id, wait_arg(args)).await;
    super::factory::evaluation(s, &id).map_err(api_error)
}

/// Whether evaluation `id` runs now.
fn evaluating(s: &Shared, id: &str) -> bool {
    s.evaluations.lock().unwrap().contains_key(id)
}

/// Waits until evaluation `id` ends, or `limit` is up.
async fn wait_evaluation(s: &Shared, id: &str, limit: Duration) {
    let mut changes = s.tx.subscribe();
    let deadline = tokio::time::Instant::now() + limit;
    while evaluating(s, id) {
        tokio::select! {
            _ = changes.recv() => {}
            _ = tokio::time::sleep_until(deadline) => return,
        }
    }
}

/// Answers factory_evaluate (confirmed) or factory_status with a stream of progress
/// notifications while the evaluation runs, then the result, when the client asks for one
/// (see [`progress_stream`]). Other calls get `None`.
async fn factory_stream(
    s: &AppState,
    headers: &HeaderMap,
    id: &Value,
    params: &Value,
    modern: bool,
) -> Option<Response> {
    let token = progress_token(params)?;
    let accepts_events = header(headers, "accept")
        .is_some_and(|a| a.to_ascii_lowercase().contains("text/event-stream"));
    let name = params.get("name").and_then(Value::as_str)?;
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let confirmed = args.get("confirm").and_then(Value::as_bool) == Some(true);
    if !accepts_events
        || !(name == "factory_status" || (name == "factory_evaluate" && confirmed))
        || wait_arg(&args).is_zero()
    {
        return None;
    }
    // A call that can't start is answered with JSON, as its error.
    let evaluation = if name == "factory_evaluate" {
        factory_start(s, &args).await.ok()?["id"]
            .as_str()?
            .to_owned()
    } else {
        let id = opt_text(&args, "id")?;
        super::factory::evaluation(s, &id).ok()?;
        id
    };
    let (tx, rx) = mpsc::channel(16);
    let (shared, id, limit) = (s.clone(), id.clone(), wait_arg(&args));
    tokio::spawn(async move {
        follow_evaluation(&shared, &id, &token, &evaluation, limit, modern, tx).await;
    });
    let events = ReceiverStream::new(rx)
        .map(|event: Value| Ok::<_, Infallible>(SseEvent::default().data(event.to_string())));
    Some(
        Sse::new(events)
            .keep_alive(KeepAlive::new().interval(KEEP_ALIVE))
            .into_response(),
    )
}

/// Sends a `notifications/progress` each time evaluation `evaluation` moves on (a run
/// ended), then the tool call's response when it ends or `limit` is up.
async fn follow_evaluation(
    s: &Shared,
    id: &Value,
    token: &Value,
    evaluation: &str,
    limit: Duration,
    modern: bool,
    tx: mpsc::Sender<Value>,
) {
    let deadline = tokio::time::Instant::now() + limit;
    let mut changes = s.tx.subscribe();
    let mut seen = None;
    let mut count = 0u64;
    loop {
        let Ok(view) = super::factory::evaluation(s, evaluation) else {
            break;
        };
        let text = format!(
            "{} of {} runs done ({})",
            view["done"],
            view["total"],
            view["state"].as_str().unwrap_or("running")
        );
        if seen.as_ref() != Some(&text) {
            seen = Some(text.clone());
            count += 1;
            let note = json!({
                "jsonrpc": "2.0",
                "method": "notifications/progress",
                "params": { "progressToken": token, "progress": count, "message": text },
            });
            if tx.send(note).await.is_err() {
                return;
            }
        }
        if !evaluating(s, evaluation) {
            break;
        }
        tokio::select! {
            _ = changes.recv() => {}
            _ = tokio::time::sleep_until(deadline) => break,
            _ = tx.closed() => return,
        }
    }
    let result = tool_result(super::factory::evaluation(s, evaluation).map_err(api_error));
    let result = if modern {
        modern_result(result)
    } else {
        result
    };
    let _ = tx.send(rpc_ok(id, result)).await;
}

fn text_arg(args: &Value, name: &str) -> Result<String, String> {
    let text = opt_text(args, name).ok_or_else(|| format!("{name} is required"))?;
    if text.len() > MAX_TEXT {
        return Err(format!(
            "{name} is too long (at most {} KB)",
            MAX_TEXT / 1000
        ));
    }
    Ok(text)
}

fn opt_text(args: &Value, name: &str) -> Option<String> {
    args.get(name)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
}

fn text_list(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}

fn wait_arg(args: &Value) -> Duration {
    args.get("wait_seconds")
        .and_then(Value::as_u64)
        .map_or(DEFAULT_WAIT, Duration::from_secs)
        .min(MAX_WAIT)
}

/// A tool's result: its JSON as text and as structured content; problems are errors
/// the calling model sees (`isError`). Unknown tools are protocol errors.
async fn call_tool(s: &AppState, params: &Value) -> Result<Value, String> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or("tools/call needs the tool's name")?;
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    if !args.is_object() {
        return Err("the arguments must be an object".into());
    }
    let outcome = match name {
        "list_peers" => Ok(peers(s)),
        "delegate" | "run_plan" | "get_task" => match start_job(s, name, &args) {
            Ok(job) => {
                wait(s, &job, wait_arg(&args)).await;
                Ok(job_json(s, &job))
            }
            Err(e) => Err(e),
        },
        "cancel_task" => cancel_task(s, &args),
        "check_plan" => text_arg(&args, "plan").map(|plan| plan_check(s, &plan)),
        "get_run" => text_arg(&args, "run_id")
            .and_then(|id| run_details(s, &id).map_err(|e| format!("{e:#}"))),
        "import_spec" => import_spec(s, &args).await,
        "factory_templates" => Ok(json!({ "templates": crate::factory::templates::list() })),
        "factory_list" => super::factory::agents(s).map_err(api_error),
        "factory_draft" => factory_draft(s, &args).await,
        "factory_check" => text_arg(&args, "name").and_then(|name| {
            let folders = args
                .get("folders")
                .is_some()
                .then(|| text_list(args.get("folders")));
            super::factory::checked(s, &name, folders).map_err(api_error)
        }),
        "factory_evaluate" => factory_evaluate(s, &args).await,
        "factory_status" => factory_status(s, &args).await,
        _ => match owner_only(s, name, &args) {
            Some(refused) => Err(refused),
            None => return Err(format!("Unknown tool: {name}")),
        },
    };
    Ok(tool_result(outcome))
}

/// A tool's outcome as a result.
fn tool_result(outcome: Result<Value, String>) -> Value {
    match outcome {
        Ok(value) => json!({
            "content": [{ "type": "text", "text": serde_json::to_string_pretty(&value).unwrap_or_default() }],
            "structuredContent": value,
            "isError": false,
        }),
        Err(error) => json!({ "content": [{ "type": "text", "text": error }], "isError": true }),
    }
}

fn peers(s: &Shared) -> Value {
    let peers: Vec<Value> = s
        .agent
        .peers()
        .into_iter()
        .map(|p| {
            json!({
                "name": p.name,
                "providers": p.capabilities,
                // Per provider: the model tasks that name none run on, and the ones it allows.
                "models": p.models,
                "folders": p.shares,
                "role": p.role,
                "worker_of": p.owner,
            })
        })
        .collect();
    let squads: Vec<Value> = s
        .squads
        .lock()
        .unwrap()
        .squads
        .iter()
        .map(|q| json!({ "name": q.name, "members": q.members }))
        .collect();
    json!({
        "agent": s.agent.name(),
        "peers": peers,
        "squads": squads,
        "about": "Peers without providers run no tasks. A folder's checks (verify=check in a plan step) are what prove claims.",
    })
}

/// Work another agent asks for.
struct Ask {
    text: String,
    peer: Option<String>,
    folder: Option<String>,
    provider: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    mcp: Vec<String>,
}

fn names(list: impl IntoIterator<Item = impl std::fmt::Display>) -> String {
    let names: Vec<String> = list.into_iter().map(|n| n.to_string()).collect();
    if names.is_empty() {
        "none".into()
    } else {
        names.join(", ")
    }
}

fn api_error((_, Json(body)): ApiError) -> String {
    body["error"].as_str().unwrap_or("it failed").to_owned()
}

/// Starts a task on a peer for another agent (never on this machine).
fn start_delegation(s: &AppState, ask: &Ask, via: &'static str) -> Result<Work, String> {
    let provider: Option<Provider> = match &ask.provider {
        Some(p) => Some(
            serde_json::from_value(json!(p))
                .map_err(|_| format!("unknown provider '{p}' (copilot, claude or codex)"))?,
        ),
        None => None,
    };
    // A squad (`#name`): its least busy member that can run it, as a plan step would.
    if let Some(squad) = ask.peer.as_deref().filter(|p| p.starts_with('#')) {
        let body = TaskBody {
            provider: provider.unwrap_or(Provider::Copilot),
            prompt: ask.text.clone(),
            target: squad.to_owned(),
            share: ask.folder.clone(),
            mcp: ask.mcp.clone(),
            checks: Vec::new(),
            attempts: None,
            model: ask.model.clone(),
            effort: ask.effort.clone(),
        };
        let view = start_task(s, body, Some(via)).map_err(api_error)?;
        return Ok(Work::Task(view.id));
    }
    let peers = s.agent.peers();
    let peer = match &ask.peer {
        Some(name) => peers.iter().find(|p| &p.name == name).ok_or_else(|| {
            format!(
                "{name} is not connected (connected: {})",
                names(peers.iter().map(|p| &p.name))
            )
        })?,
        None => peers
            .iter()
            .find(|p| match provider {
                Some(want) => p.capabilities.contains(&want),
                None => !p.capabilities.is_empty(),
            })
            .ok_or_else(|| match provider {
                Some(want) => format!("no connected peer runs {want} tasks"),
                None => "no connected peer runs tasks".to_owned(),
            })?,
    };
    let provider = match provider {
        Some(want) if peer.capabilities.contains(&want) => want,
        Some(want) => {
            return Err(format!(
                "{} doesn't run {want} tasks (it runs: {})",
                peer.name,
                names(&peer.capabilities)
            ));
        }
        None => *peer
            .capabilities
            .first()
            .ok_or_else(|| format!("{} runs no tasks", peer.name))?,
    };
    let body = TaskBody {
        provider,
        prompt: ask.text.clone(),
        target: peer.id.to_string(),
        share: ask.folder.clone(),
        mcp: ask.mcp.clone(),
        checks: Vec::new(),
        attempts: None,
        model: ask.model.clone(),
        effort: ask.effort.clone(),
    };
    let view = start_task(s, body, Some(via)).map_err(api_error)?;
    Ok(Work::Task(view.id))
}

/// Starts plan `text` for another agent; its outcome is kept when it ends.
fn start_plan_for(s: &AppState, text: &str, via: &'static str) -> Result<Work, String> {
    let started = start_plan(s, text.to_owned(), Some(via))?;
    let mut steps: Vec<(String, u64)> = started.steps.into_iter().collect();
    steps.sort_by_key(|(_, id)| *id);
    let outcome = Arc::new(Mutex::new(None));
    let slot = outcome.clone();
    let shared = s.clone();
    let done = started.done;
    tokio::spawn(async move {
        let result = done.await.unwrap_or_else(|_| crate::schedule::Outcome {
            verdict: "cancelled: the run was stopped".to_owned(),
            run_id: None,
        });
        *slot.lock().unwrap() = Some(result);
        shared.changed.notify_waiters();
    });
    Ok(Work::Plan { steps, outcome })
}

fn new_job(
    work: Work,
    via: &'static str,
    context_id: Option<String>,
    message: Option<Value>,
) -> Job {
    Job {
        id: new_id("t-"),
        context_id: context_id.unwrap_or_else(|| new_id("ctx-")),
        via,
        work,
        created_ms: now_ms(),
        message,
    }
}

/// The tasks of `job` as they are now.
fn views(s: &Shared, job: &Job) -> Vec<TaskView> {
    let ids: Vec<u64> = match &job.work {
        Work::Task(id) => vec![*id],
        Work::Plan { steps, .. } => steps.iter().map(|(_, id)| *id).collect(),
        Work::Rejected(_) => Vec::new(),
    };
    let tasks = s.tasks.lock().unwrap();
    ids.iter()
        .filter_map(|id| tasks.iter().find(|t| t.id == *id).cloned())
        .collect()
}

fn job_state(s: &Shared, job: &Job) -> JobState {
    match &job.work {
        Work::Rejected(_) => JobState::Rejected,
        Work::Task(_) => match views(s, job).first().map(|t| t.status) {
            Some("running" | "waiting") => JobState::Working,
            Some("done") => JobState::Completed,
            Some("cancelled") => JobState::Canceled,
            _ => JobState::Failed,
        },
        Work::Plan { outcome, .. } => {
            let steps = views(s, job);
            if steps.iter().any(|t| t.status == "cancelled") {
                JobState::Canceled
            } else if outcome.lock().unwrap().is_none() {
                JobState::Working
            } else if steps.iter().all(|t| t.status == "done") {
                JobState::Completed
            } else {
                JobState::Failed
            }
        }
    }
}

/// Waits up to `limit` for `job` to end.
async fn wait(s: &Shared, job: &Job, limit: Duration) {
    let deadline = tokio::time::Instant::now() + limit;
    loop {
        // Made before looking, so a change in between still wakes it.
        let changed = s.changed.notified();
        if job_state(s, job) != JobState::Working {
            return;
        }
        if tokio::time::timeout_at(deadline, changed).await.is_err() {
            return;
        }
    }
}

/// Cancels `job`'s running work; false if nothing was running.
fn cancel_job(s: &Shared, job: &Job, why: &str) -> bool {
    let ids: Vec<u64> = match &job.work {
        Work::Task(id) => vec![*id],
        Work::Plan { steps, .. } => steps.iter().map(|(_, id)| *id).collect(),
        Work::Rejected(_) => Vec::new(),
    };
    ids.into_iter().any(|id| cancel_running(s, id, why).is_ok())
}

/// A job for an MCP client.
fn job_json(s: &Shared, job: &Job) -> Value {
    let state = job_state(s, job);
    let mut v = json!({ "task_id": job.id, "state": state.name() });
    match &job.work {
        Work::Rejected(why) => v["error"] = json!(why),
        Work::Task(_) => {
            v["kind"] = json!("task");
            if let Some(t) = views(s, job).first() {
                v["peer"] = json!(t.target);
                v["provider"] = json!(t.provider);
                if let Some(model) = &t.model {
                    v["model"] = json!(model);
                }
                if let Some(p) = &t.progress {
                    v["progress"] = json!(p.text);
                }
                if let Some(output) = &t.output {
                    let key = if state == JobState::Completed {
                        "output"
                    } else {
                        "error"
                    };
                    v[key] = json!(output);
                }
                if let Some(evidence) = t.evidence {
                    v["evidence"] = json!(evidence);
                }
            }
        }
        Work::Plan { outcome, .. } => {
            v["kind"] = json!("plan");
            v["steps"] = views(s, job)
                .iter()
                .map(|t| {
                    json!({
                        "step": t.step, "peer": t.target, "status": t.status, "model": t.model,
                        "output": t.output, "evidence": t.evidence,
                        "progress": t.progress.as_ref().map(|p| p.text.clone()),
                    })
                })
                .collect();
            if let Some(o) = &*outcome.lock().unwrap() {
                v["verdict"] = json!(o.verdict);
                v["run_id"] = json!(o.run_id);
            }
        }
    }
    if state == JobState::Working {
        v["next"] = json!(
            "It is still running: call get_task with this task_id (wait_seconds up to 300) for the result."
        );
    }
    v
}

/// Whether `job`'s work is still kept: the oldest finished tasks are let go (see
/// [`super::TASK_LIMIT`]), and cleared ones, and then other agents can no longer look
/// it up.
fn kept(s: &Shared, job: &Job) -> bool {
    matches!(job.work, Work::Rejected(_)) || !views(s, job).is_empty()
}

/// Work another agent started, while it is kept.
fn lookup(s: &Shared, id: &str) -> Option<Job> {
    s.connect.get(id).filter(|j| kept(s, j))
}

fn find_job(s: &Shared, id: &str) -> Result<Job, String> {
    lookup(s, id).ok_or_else(|| {
        format!(
            "there is no task {id} (other agents can look up only the tasks they started, and the oldest finished ones are let go)"
        )
    })
}

/// The work that delegate, run_plan or get_task is about (started, or looked up).
fn start_job(s: &AppState, name: &str, args: &Value) -> Result<Job, String> {
    match name {
        "delegate" => {
            let ask = Ask {
                text: text_arg(args, "prompt")?,
                peer: opt_text(args, "peer"),
                folder: opt_text(args, "folder"),
                provider: opt_text(args, "provider"),
                model: opt_text(args, "model"),
                effort: opt_text(args, "effort"),
                mcp: text_list(args.get("mcp")),
            };
            let job = new_job(start_delegation(s, &ask, "MCP")?, "MCP", None, None);
            s.connect.add(job.clone());
            Ok(job)
        }
        "run_plan" => {
            let work = start_plan_for(s, &text_arg(args, "plan")?, "MCP")?;
            let job = new_job(work, "MCP", None, None);
            s.connect.add(job.clone());
            Ok(job)
        }
        _ => find_job(s, &text_arg(args, "task_id")?),
    }
}

fn cancel_task(s: &Shared, args: &Value) -> Result<Value, String> {
    let job = find_job(s, &text_arg(args, "task_id")?)?;
    if !cancel_job(s, &job, "Cancelled by the agent that asked for it (MCP).") {
        return Err(format!(
            "task {} isn't running ({})",
            job.id,
            job_state(s, &job).name()
        ));
    }
    Ok(job_json(s, &job))
}

async fn import_spec(s: &AppState, args: &Value) -> Result<Value, String> {
    let mut vars = BTreeMap::new();
    vars.insert("spec".to_owned(), text_arg(args, "path")?);
    for name in ["peer", "folder", "reviewers", "test", "approver"] {
        if let Some(value) = opt_text(args, name) {
            vars.insert(name.to_owned(), value);
        }
    }
    let kb = s.info.profile.knowledge();
    // Reading the spec's files (and searching a project for one) blocks.
    let (rendered, saved) = tokio::task::spawn_blocking(move || {
        let rendered = crate::playbooks::render_full(crate::playbooks::SPEC_IMPORT, &vars)?;
        let saved = rendered.save_knowledge(&kb)?;
        anyhow::Ok((rendered, saved))
    })
    .await
    .map_err(|e| format!("the import stopped: {e}"))?
    .map_err(|e| format!("{e:#}"))?;
    s.send(json!({ "type": "ops" }));
    Ok(json!({
        "plan": rendered.text,
        "notes": rendered.notes,
        "knowledge": saved,
        "next": "Check the plan with check_plan, then run it with run_plan.",
    }))
}

// ---------- A2A ----------

/// The agent card (only while other agents may connect).
pub(super) async fn agent_card(State(s): State<AppState>, headers: HeaderMap) -> Response {
    if s.integration_token().is_none() {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "this agent takes no work from other agents; its owner can turn that on (Connect other agents)" })),
        )
            .into_response();
    }
    // The Host was checked to be this loopback server (see require_local_host).
    let host = header(&headers, "host")
        .unwrap_or_else(|| format!("127.0.0.1:{}", s.port.load(Ordering::Relaxed)));
    let card = json!({
        "name": format!("Proofkin agent {}", s.agent.name()),
        "description": "Hands work to the agents of this agent's teammates (its peers), peer to peer, and proves it: each result is signed by the peer that did it, and plans prove their claims with passing checks or sign-offs.",
        "supportedInterfaces": [{
            "url": format!("http://{host}/a2a"),
            "protocolBinding": "JSONRPC",
            "protocolVersion": A2A_VERSION,
        }],
        "provider": { "organization": "Proofkin", "url": "https://github.com/mcbelton/proofkin" },
        "version": env!("CARGO_PKG_VERSION"),
        "documentationUrl": "https://github.com/mcbelton/proofkin#connect-other-agents-mcp-and-a2a",
        "capabilities": { "streaming": true, "pushNotifications": false, "extendedAgentCard": false },
        "securitySchemes": {
            "bearer": { "httpAuthSecurityScheme": {
                "scheme": "Bearer",
                "description": format!("The integration token: proofkin connect --profile {}", s.info.profile.name),
            }},
        },
        "securityRequirements": [{ "schemes": { "bearer": { "list": [] } } }],
        "defaultInputModes": ["text/plain", "application/json"],
        "defaultOutputModes": ["text/plain", "application/json"],
        "skills": [
            {
                "id": "delegate",
                "name": "Delegate a task to a peer",
                "description": "Runs a task on a teammate's agent (Copilot, Claude or Codex) in a folder it shares; the result is signed by that peer. Say where in the message's metadata or a data part: peer, folder, provider, model, mcp. Without a peer, any connected peer that runs tasks takes it.",
                "tags": ["delegation", "coding", "peer-to-peer"],
                "examples": ["Summarize the open TODOs in README.md", "Fix the failing parser test (with metadata {\"peer\": \"alice\", \"folder\": \"app\"})"],
                "inputModes": ["text/plain", "application/json"],
                "outputModes": ["text/plain"],
            },
            {
                "id": "plan",
                "name": "Run an evidence plan",
                "description": "Runs a plan across peers and squads in dependency order: claim lines (claim: ID what must be true), then one step per line (name [after a,b] @peer[:folder] or #squad [verify=check] [claims=IDs] what to do). Claims are proven only by passing checks or sign-offs, and the evidence report is saved. Send the plan as the message text.",
                "tags": ["plan", "evidence", "verification"],
                "examples": ["claim: C-1 The parser handles empty input\nfix @alice:app skill=implement-change verify=test claims=C-1 Make the parser accept empty input"],
                "inputModes": ["text/plain"],
                "outputModes": ["text/plain", "application/json"],
            },
        ],
    });
    let body = card.to_string();
    let digest = <sha2::Sha256 as sha2::Digest>::digest(body.as_bytes());
    let etag = format!("\"{}\"", &data_encoding::HEXLOWER.encode(&digest)[..16]);
    let fresh = headers
        .get(header::IF_NONE_MATCH)
        .is_some_and(|v| v.as_bytes() == etag.as_bytes());
    let mut res = if fresh {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        ([(header::CONTENT_TYPE, "application/json")], body).into_response()
    };
    let headers = res.headers_mut();
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=300"),
    );
    if let Ok(etag) = HeaderValue::from_str(&etag) {
        headers.insert(header::ETAG, etag);
    }
    res
}

/// An A2A error: its JSON-RPC code, message and A2A reason.
struct A2aError {
    code: i64,
    message: String,
    reason: Option<&'static str>,
}

impl A2aError {
    fn invalid(message: impl Into<String>) -> Self {
        Self {
            code: -32602,
            message: message.into(),
            reason: None,
        }
    }

    fn not_found(id: &str) -> Self {
        Self {
            code: -32001,
            message: format!("Task not found: {id}"),
            reason: Some("TASK_NOT_FOUND"),
        }
    }

    fn json(self, id: &Value) -> Value {
        let data = self.reason.map(|reason| {
            json!([{
                "@type": "type.googleapis.com/google.rpc.ErrorInfo",
                "reason": reason,
                "domain": "a2a-protocol.org",
            }])
        });
        rpc_err(id, self.code, self.message, data)
    }
}

pub(super) async fn a2a(
    State(s): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<BTreeMap<String, String>>,
    body: Bytes,
) -> Response {
    let msg: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return reply(
                StatusCode::OK,
                rpc_err(
                    &Value::Null,
                    -32700,
                    format!("Invalid JSON payload: {e}"),
                    None,
                ),
            );
        }
    };
    let id = msg.get("id").cloned().unwrap_or(Value::Null);
    let Some(method) = msg.get("method").and_then(Value::as_str) else {
        return reply(
            StatusCode::OK,
            rpc_err(
                &id,
                -32600,
                "Request payload validation error: no method",
                None,
            ),
        );
    };
    // An empty version means 0.3 (A2A 1.0, section 3.6.2).
    let version = header(&headers, "a2a-version")
        .or_else(|| query.get("A2A-Version").cloned())
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "0.3".into());
    if version != A2A_VERSION {
        let error = A2aError {
            code: -32009,
            message: format!(
                "A2A {version} is not supported: this agent speaks A2A {A2A_VERSION} (send the header A2A-Version: {A2A_VERSION})"
            ),
            reason: Some("VERSION_NOT_SUPPORTED"),
        };
        return reply(StatusCode::OK, error.json(&id));
    }
    let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));
    let streamed = match method {
        "SendStreamingMessage" => Some(start_message(&s, &params, true)),
        "SubscribeToTask" => Some(subscribe_job(&s, &params)),
        _ => None,
    };
    if let Some(started) = streamed {
        return match started {
            Ok(job) => stream_job(&s, id, job),
            Err(error) => reply(StatusCode::OK, error.json(&id)),
        };
    }
    let result = match method {
        "SendMessage" => send_message(&s, &params).await,
        "GetTask" => a2a_get_task(&s, &params),
        "ListTasks" => list_tasks(&s, &params),
        "CancelTask" => a2a_cancel_task(&s, &params),
        "CreateTaskPushNotificationConfig"
        | "GetTaskPushNotificationConfig"
        | "ListTaskPushNotificationConfigs"
        | "DeleteTaskPushNotificationConfig" => Err(A2aError {
            code: -32003,
            message: "Push notifications are not supported: use GetTask".into(),
            reason: Some("PUSH_NOTIFICATION_NOT_SUPPORTED"),
        }),
        "GetExtendedAgentCard" => Err(A2aError {
            code: -32004,
            message:
                "This agent has no extended agent card (its card says extendedAgentCard: false)"
                    .into(),
            reason: Some("UNSUPPORTED_OPERATION"),
        }),
        _ => Err(A2aError {
            code: -32601,
            message: format!("Method not found: {method}"),
            reason: None,
        }),
    };
    reply(
        StatusCode::OK,
        match result {
            Ok(result) => rpc_ok(&id, result),
            Err(error) => error.json(&id),
        },
    )
}

async fn send_message(s: &AppState, params: &Value) -> Result<Value, A2aError> {
    let job = start_message(s, params, false)?;
    let config = params.get("configuration");
    let immediately = config
        .and_then(|c| c.get("returnImmediately"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !immediately {
        wait(s, &job, MAX_BLOCK).await;
    }
    let history = config
        .and_then(|c| c.get("historyLength"))
        .and_then(Value::as_i64);
    Ok(json!({ "task": a2a_task(s, &job, history, true) }))
}

/// Starts the work a `SendMessage` or `SendStreamingMessage` asks for. Work that can't
/// start comes back as a rejected task, but for a stream (`streaming`) as an error, as
/// no stream is started for it.
fn start_message(s: &AppState, params: &Value, streaming: bool) -> Result<Job, A2aError> {
    let message = params
        .get("message")
        .filter(|m| m.is_object())
        .ok_or_else(|| A2aError::invalid("message is required"))?;
    if let Some(task_id) = message
        .get("taskId")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
    {
        let job = lookup(s, task_id).ok_or_else(|| A2aError::not_found(task_id))?;
        let message = if job_state(s, &job) == JobState::Working {
            format!(
                "Task {task_id} is running and takes no more input: wait for it (GetTask) or cancel it"
            )
        } else {
            format!("Task {task_id} has ended: send new work as a new message (without taskId)")
        };
        return Err(A2aError {
            code: -32004,
            message,
            reason: Some("UNSUPPORTED_OPERATION"),
        });
    }
    let mut meta = serde_json::Map::new();
    for source in [params.get("metadata"), message.get("metadata")] {
        if let Some(Value::Object(m)) = source {
            meta.extend(m.clone());
        }
    }
    let mut text = Vec::new();
    for part in message
        .get("parts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(t) = part.get("text").and_then(Value::as_str) {
            text.push(t.to_owned());
        } else if let Some(Value::Object(data)) = part.get("data") {
            meta.extend(data.clone());
        } else if part.get("raw").is_some() || part.get("url").is_some() {
            return Err(A2aError {
                code: -32005,
                message: "Files are not supported: send the work as text".into(),
                reason: Some("CONTENT_TYPE_NOT_SUPPORTED"),
            });
        }
    }
    let text = text.join("\n").trim().to_owned();
    if text.is_empty() {
        return Err(A2aError::invalid("the message has no text: say what to do"));
    }
    if text.len() > MAX_TEXT {
        return Err(A2aError::invalid(format!(
            "the message is too long (at most {} KB of text)",
            MAX_TEXT / 1000
        )));
    }
    let meta = Value::Object(meta);
    let ask = Ask {
        text: text.clone(),
        peer: opt_text(&meta, "peer"),
        folder: opt_text(&meta, "folder"),
        provider: opt_text(&meta, "provider"),
        model: opt_text(&meta, "model"),
        effort: opt_text(&meta, "effort"),
        mcp: text_list(meta.get("mcp")),
    };
    let plan = match opt_text(&meta, "kind").as_deref() {
        Some("plan") => true,
        Some("task") => false,
        _ => crate::plan::Plan::parse(&text).is_ok(),
    };
    let work = if plan {
        start_plan_for(s, &text, "A2A")
    } else {
        start_delegation(s, &ask, "A2A")
    }
    .unwrap_or_else(Work::Rejected);
    if streaming && let Work::Rejected(why) = &work {
        return Err(A2aError::invalid(why.clone()));
    }
    let context_id = opt_text(message, "contextId");
    let job = new_job(work, "A2A", context_id, Some(message.clone()));
    s.connect.add(job.clone());
    Ok(job)
}

/// The task a `SubscribeToTask` follows: one that is still running.
fn subscribe_job(s: &Shared, params: &Value) -> Result<Job, A2aError> {
    let id = task_id(params)?;
    let job = lookup(s, &id).ok_or_else(|| A2aError::not_found(&id))?;
    if job_state(s, &job) != JobState::Working {
        return Err(A2aError {
            code: -32004,
            message: format!("Task {id} has ended: GetTask shows its result"),
            reason: Some("UNSUPPORTED_OPERATION"),
        });
    }
    Ok(job)
}

/// Answers with `job` as a stream of server-sent events (see [`follow`]), which ends
/// when the work does or the client goes away.
fn stream_job(s: &AppState, id: Value, job: Job) -> Response {
    let (tx, rx) = mpsc::channel(16);
    let shared = s.clone();
    tokio::spawn(async move { follow(&shared, &id, &job, tx).await });
    let events = ReceiverStream::new(rx)
        .map(|event: Value| Ok::<_, Infallible>(SseEvent::default().data(event.to_string())));
    Sse::new(events)
        .keep_alive(KeepAlive::new().interval(KEEP_ALIVE))
        .into_response()
}

/// Sends `job`'s task, then its status each time it changes (its state, or what its
/// steps' peers say they are doing), and last the finished task. Returns when that is
/// sent, or the client has gone away (`tx` closed).
async fn follow(s: &Shared, id: &Value, job: &Job, tx: mpsc::Sender<Value>) {
    let first = a2a_task(s, job, None, true);
    if tx.send(rpc_ok(id, json!({ "task": first }))).await.is_err() {
        return;
    }
    let mut seen: Option<(Value, Value)> = None;
    let mut count = 0;
    loop {
        // Made before looking, so a change in between still wakes it.
        let changed = s.changed.notified();
        tokio::pin!(changed);
        let task = a2a_task(s, job, None, true);
        let status = task["status"].clone();
        let now = (status["state"].clone(), status["message"]["parts"].clone());
        if seen.as_ref() != Some(&now) {
            seen = Some(now);
            count += 1;
            let mut status = status;
            status["timestamp"] = iso(now_ms());
            if status.get("message").is_some() {
                status["message"]["messageId"] = json!(format!("{}-{count}", job.id));
            }
            let update = json!({ "statusUpdate": {
                "taskId": job.id, "contextId": job.context_id, "status": status,
            }});
            if tx.send(rpc_ok(id, update)).await.is_err() {
                return;
            }
        }
        if task["status"]["state"] != JobState::Working.a2a() {
            let _ = tx.send(rpc_ok(id, json!({ "task": task }))).await;
            return;
        }
        tokio::select! {
            _ = &mut changed => {}
            _ = tx.closed() => return,
        }
    }
}

fn iso(ms: u128) -> Value {
    chrono::DateTime::from_timestamp_millis(ms as i64)
        .map(|t| json!(t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)))
        .unwrap_or(Value::Null)
}

fn text_part(text: &str) -> Value {
    json!({ "text": text, "mediaType": "text/plain" })
}

/// `job` as an A2A task (`history`: at most that many messages; `artifacts`: with them).
fn a2a_task(s: &Shared, job: &Job, history: Option<i64>, artifacts: bool) -> Value {
    let state = job_state(s, job);
    let steps = views(s, job);
    let mut outputs = Vec::new();
    let mut meta = json!({ "via": job.via });
    let status = match &job.work {
        Work::Rejected(why) => Some(why.clone()),
        Work::Task(_) => {
            meta["kind"] = json!("task");
            let t = steps.first();
            if let Some(t) = t {
                meta["peer"] = json!(t.target);
                meta["provider"] = json!(t.provider);
                if let Some(model) = &t.model {
                    meta["model"] = json!(model);
                }
            }
            match (state, t) {
                (JobState::Working, Some(t)) => Some(match &t.progress {
                    Some(p) => format!("Running on {}: {}", t.target, p.text),
                    None => format!("Running on {}", t.target),
                }),
                (JobState::Completed, Some(t)) => {
                    outputs.push(json!({
                        "artifactId": "result",
                        "name": "Result",
                        "parts": [text_part(t.output.as_deref().unwrap_or_default())],
                        "metadata": { "peer": t.target, "evidence": t.evidence },
                    }));
                    Some(format!(
                        "Done by {}{}",
                        t.target,
                        match t.evidence {
                            Some("verified") => " (its signed evidence checks out)",
                            Some(_) => " (its evidence could not be verified)",
                            None => "",
                        }
                    ))
                }
                (_, Some(t)) => t.output.clone(),
                (_, None) => Some("The task is no longer known".into()),
            }
        }
        Work::Plan { outcome, .. } => {
            meta["kind"] = json!("plan");
            for t in steps.iter().filter(|t| t.output.is_some()) {
                let step = t.step.clone().unwrap_or_default();
                outputs.push(json!({
                    "artifactId": format!("step-{step}"),
                    "name": format!("Step {step}"),
                    "parts": [text_part(t.output.as_deref().unwrap_or_default())],
                    "metadata": { "peer": t.target, "status": t.status, "evidence": t.evidence },
                }));
            }
            let outcome = outcome.lock().unwrap().clone();
            match outcome {
                Some(o) => {
                    meta["runId"] = json!(o.run_id);
                    outputs.push(json!({
                        "artifactId": "evidence",
                        "name": "Evidence verdict",
                        "parts": [text_part(&o.verdict), { "data": { "runId": o.run_id }, "mediaType": "application/json" }],
                    }));
                    Some(o.verdict)
                }
                None => {
                    let done = steps.iter().filter(|t| t.finished()).count();
                    let signing = steps
                        .iter()
                        .filter(|t| t.approval && t.status == "running")
                        .map(|t| t.target.clone())
                        .collect::<Vec<_>>();
                    // What each running step's peer last said it is doing.
                    let doing: Vec<String> = steps
                        .iter()
                        .filter(|t| t.status == "running" && !t.approval)
                        .filter_map(|t| {
                            let p = t.progress.as_ref()?;
                            Some(format!(
                                "{} on {}: {}",
                                t.step.as_deref().unwrap_or("?"),
                                t.target,
                                p.text
                            ))
                        })
                        .collect();
                    let mut text = if signing.is_empty() {
                        format!("{done} of {} steps done", steps.len())
                    } else {
                        format!(
                            "{done} of {} steps done; waiting for {} to sign off",
                            steps.len(),
                            signing.join(", ")
                        )
                    };
                    if !doing.is_empty() {
                        text += &format!(" ({})", doing.join("; "));
                    }
                    Some(text)
                }
            }
        }
    };
    let updated = steps
        .iter()
        .map(|t| t.finished_ms.unwrap_or(t.started_ms))
        .max()
        .unwrap_or(job.created_ms);
    let mut task = json!({
        "id": job.id,
        "contextId": job.context_id,
        "status": { "state": state.a2a(), "timestamp": iso(updated) },
        "metadata": meta,
    });
    if let Some(text) = status {
        task["status"]["message"] = json!({
            "messageId": format!("{}-{}", job.id, state.name()),
            "role": "ROLE_AGENT",
            "parts": [text_part(&text)],
            "contextId": job.context_id,
            "taskId": job.id,
        });
    }
    if artifacts {
        task["artifacts"] = json!(outputs);
    }
    if history != Some(0)
        && let Some(message) = &job.message
    {
        let mut message = message.clone();
        message["taskId"] = json!(job.id);
        message["contextId"] = json!(job.context_id);
        task["history"] = json!([message]);
    }
    task
}

fn task_id(params: &Value) -> Result<String, A2aError> {
    opt_text(params, "id").ok_or_else(|| A2aError::invalid("id is required"))
}

fn a2a_get_task(s: &Shared, params: &Value) -> Result<Value, A2aError> {
    let id = task_id(params)?;
    let job = lookup(s, &id).ok_or_else(|| A2aError::not_found(&id))?;
    let history = params.get("historyLength").and_then(Value::as_i64);
    Ok(a2a_task(s, &job, history, true))
}

fn a2a_cancel_task(s: &Shared, params: &Value) -> Result<Value, A2aError> {
    let id = task_id(params)?;
    let job = lookup(s, &id).ok_or_else(|| A2aError::not_found(&id))?;
    if job_state(s, &job) != JobState::Working
        || !cancel_job(s, &job, "Cancelled by the agent that asked for it (A2A).")
    {
        return Err(A2aError {
            code: -32002,
            message: format!("Task {id} can't be cancelled: it has ended"),
            reason: Some("TASK_NOT_CANCELABLE"),
        });
    }
    Ok(a2a_task(s, &job, None, true))
}

fn list_tasks(s: &Shared, params: &Value) -> Result<Value, A2aError> {
    let context = opt_text(params, "contextId");
    let status = opt_text(params, "status").filter(|v| v != "TASK_STATE_UNSPECIFIED");
    let page_size = params.get("pageSize").and_then(Value::as_i64).unwrap_or(50);
    if !(1..=100).contains(&page_size) {
        return Err(A2aError::invalid("pageSize must be from 1 to 100"));
    }
    let offset: usize = match opt_text(params, "pageToken") {
        Some(token) => token
            .parse()
            .map_err(|_| A2aError::invalid("pageToken is not one this agent gave"))?,
        None => 0,
    };
    let artifacts = params
        .get("includeArtifacts")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let history = params.get("historyLength").and_then(Value::as_i64);
    let jobs: Vec<Job> = s
        .connect
        .jobs
        .lock()
        .unwrap()
        .iter()
        .rev()
        .cloned()
        .collect();
    let matching: Vec<Job> = jobs
        .into_iter()
        .filter(|j| kept(s, j))
        .filter(|j| context.as_ref().is_none_or(|c| &j.context_id == c))
        .filter(|j| status.as_ref().is_none_or(|st| job_state(s, j).a2a() == st))
        .collect();
    let page: Vec<Value> = matching
        .iter()
        .skip(offset)
        .take(page_size as usize)
        .map(|j| a2a_task(s, j, history, artifacts))
        .collect();
    let next = if offset + page.len() < matching.len() {
        (offset + page.len()).to_string()
    } else {
        String::new()
    };
    Ok(json!({
        "tasks": page,
        "nextPageToken": next,
        "pageSize": page_size,
        "totalSize": matching.len(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_compare_whole_and_headers_decode() {
        assert!(same_secret(b"pfk_abc", b"pfk_abc"));
        assert!(!same_secret(b"pfk_abc", b"pfk_abd"));
        assert!(!same_secret(b"pfk_abc", b"pfk_ab"));
        assert_eq!(decode_header("delegate").as_deref(), Some("delegate"));
        assert_eq!(
            decode_header("=?base64?ZGVsZWdhdGU=?=").as_deref(),
            Some("delegate")
        );
        assert_eq!(decode_header("=?base64?!!?="), None);
    }
}
