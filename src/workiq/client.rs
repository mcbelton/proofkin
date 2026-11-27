//! MCP client for Work IQ.
//!
//! The [`Client`] owns the MCP session (`initialize`, `Mcp-Session-Id`, renewal when the server
//! forgets it) and caches `tools/list`. A [`Transport`] carries one JSON-RPC message at a time:
//! [`HttpTransport`] here for the hosted endpoint, and [`super::stdio::StdioTransport`] for
//! Microsoft's CLI.

use std::borrow::Cow;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use super::audit::{Audit, CallContext};
use super::document::number;

/// Work IQ's hosted MCP endpoint.
pub const DEFAULT_URL: &str = "https://workiq.svc.cloud.microsoft/mcp";
/// The MCP protocol version the client asks for.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const ERROR_BODY_CHARS: usize = 300;
/// The longest Proofkin waits for a throttled call; a longer delay is reported instead.
pub const MAX_THROTTLE_WAIT: Duration = Duration::from_secs(60);
/// How many times a throttled call, and a transient failure, is sent again.
pub const MAX_RETRIES: u32 = 2;
const BACKOFF_BASE: Duration = Duration::from_millis(500);

/// The tools that only read. Only these are sent again after a transient failure. Every other
/// tool, the change tools and any tool not listed, is never sent again.
pub const READ_TOOLS: [&str; 8] = [
    "retrieve",
    "ask",
    "fetch",
    "fetch_blob",
    "search_paths",
    "get_schema",
    "list_agents",
    "discover",
];

/// Where bearer tokens come from. Asked once per request, so a refreshed token is used at once.
#[async_trait]
pub trait TokenSource: Send + Sync {
    async fn token(&self) -> Result<String>;

    /// Gets a new token after Work IQ rejected the current one (401). Does nothing by default.
    async fn refresh(&self) -> Result<()> {
        Ok(())
    }
}

/// Waits between attempts. Injected so tests don't wait.
#[async_trait]
pub trait Sleeper: Send + Sync {
    async fn sleep(&self, duration: Duration);
}

/// Sleeps for real.
pub struct TokioSleeper;

#[async_trait]
impl Sleeper for TokioSleeper {
    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }
}

/// A token that never changes (tests).
pub struct FixedToken(pub String);

#[async_trait]
impl TokenSource for FixedToken {
    async fn token(&self) -> Result<String> {
        Ok(self.0.clone())
    }
}

/// The ids Work IQ puts on a response, for audit and support.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RequestIds {
    pub request_id: Option<String>,
    pub ms_request_id: Option<String>,
    /// The response's `client-request-id`, else the one that was sent.
    pub client_request_id: Option<String>,
    pub correlation_id: Option<String>,
}

impl RequestIds {
    fn from_headers(headers: &reqwest::header::HeaderMap, sent_client_request_id: &str) -> Self {
        let get = |name: &str| {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        RequestIds {
            request_id: get("request-id"),
            ms_request_id: get("x-ms-request-id"),
            client_request_id: get("client-request-id")
                .or_else(|| Some(sent_client_request_id.to_string())),
            correlation_id: get("x-ms-correlation-id"),
        }
    }
}

impl fmt::Display for RequestIds {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let parts = [
            ("request-id", &self.request_id),
            ("x-ms-request-id", &self.ms_request_id),
            ("client-request-id", &self.client_request_id),
            ("x-ms-correlation-id", &self.correlation_id),
        ];
        let mut first = true;
        for (name, value) in parts {
            if let Some(value) = value {
                if !first {
                    f.write_str(", ")?;
                }
                first = false;
                write!(f, "{name} {value}")?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkIqErrorKind {
    /// The server answered with this HTTP status.
    Http(u16),
    /// The server answered with a JSON-RPC error of this code.
    Rpc(i64),
    /// The request could not be sent or the reply not read.
    Transport,
    /// The reply was not the MCP message that was expected.
    Protocol,
    /// The tool ran and reported a failure.
    Tool,
    /// The call was refused before it was sent, because a budget is used up.
    Budget,
}

/// What kind of failure this is, which decides what Proofkin does about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Class {
    /// The token was rejected, or a claims challenge needs a new sign-in.
    SignInAgain,
    /// An admin has to consent to the app.
    Consent,
    /// A policy (Conditional Access, assignment, the Work IQ MCP policy) denies the call.
    DeniedByPolicy,
    /// Usage-based billing or the spending policy stops the call.
    Billing,
    /// Too many requests; the server's delay, when it gave one.
    Throttled(Option<Duration>),
    /// A failure that may pass: 5xx, timeouts, lost connections.
    Transient,
    /// A change that timed out or lost its connection: it may or may not have been made.
    UncertainChange,
    /// Any other error from the tool or the protocol.
    Tool,
    /// A call budget is used up; the call was not sent.
    OverBudget,
}

/// Who can do what is needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Who {
    User,
    Admin,
    Support,
}

impl fmt::Display for Who {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Who::User => "you",
            Who::Admin => "admin",
            Who::Support => "support",
        })
    }
}

/// What to do about an error, and who can do it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fix {
    pub who: Who,
    pub action: String,
}

/// A failed call, with the ids to give to support.
#[derive(Debug, Clone)]
pub struct WorkIqError {
    pub kind: WorkIqErrorKind,
    pub class: Class,
    /// What happened, on one line.
    pub message: String,
    pub fix: Fix,
    pub ids: RequestIds,
    /// What the tool said when it reported the failure itself (`isError`, `stoppedReason:
    /// "error"`): on one line, redacted, at most 300 characters; empty when it said nothing.
    /// `None` for any other failure.
    pub tool_text: Option<String>,
}

impl WorkIqError {
    pub(crate) fn new(
        kind: WorkIqErrorKind,
        class: Class,
        message: impl AsRef<str>,
        ids: RequestIds,
    ) -> Self {
        let message = one_line(message.as_ref());
        let (class, message) = if mentions_eula(&message) {
            (Class::Tool, EULA_MESSAGE.to_string())
        } else {
            (class, message)
        };
        let fix = fix_for(&class, &message);
        WorkIqError {
            kind,
            class,
            message,
            fix,
            ids,
            tool_text: None,
        }
    }

    /// A failure to send the request or read the reply.
    pub(crate) fn transport(message: impl AsRef<str>, ids: RequestIds) -> Self {
        Self::new(WorkIqErrorKind::Transport, Class::Transient, message, ids)
    }

    /// A reply that is not the MCP message that was expected.
    pub(crate) fn protocol(message: impl AsRef<str>, ids: RequestIds) -> Self {
        Self::new(WorkIqErrorKind::Protocol, Class::Tool, message, ids)
    }

    /// A non-success HTTP answer, classified from the status, the `WWW-Authenticate` and
    /// `Retry-After` headers and the body.
    fn http(
        status: reqwest::StatusCode,
        www_authenticate: Option<&str>,
        retry_after: Option<&str>,
        body: &str,
        ids: RequestIds,
    ) -> Self {
        let snippet: String = body.chars().take(ERROR_BODY_CHARS).collect();
        let payload = serde_json::from_str::<Value>(body)
            .map(|json| Payload::of(Some(&json)))
            .unwrap_or_default();
        let challenged = www_authenticate.is_some_and(|header| {
            let header = normalized(header);
            header.contains("invalidtoken") || header.contains("insufficientclaims")
        });
        let class = if challenged {
            Class::SignInAgain
        } else {
            let retry_after = retry_after.and_then(parse_retry_after);
            class_of_status(status.as_u16(), &snippet, &payload, retry_after)
        };
        let mut message = format!("the server answered {status}: {}", snippet.trim());
        if let Some(header) = www_authenticate.filter(|_| challenged) {
            message.push_str(&format!(" (WWW-Authenticate: {header})"));
        }
        Self::new(WorkIqErrorKind::Http(status.as_u16()), class, message, ids)
    }

    /// A failed status inside a tool's answer (`fetch` gives one per entity), classified as
    /// an HTTP answer with that status would be.
    pub(crate) fn entity(tool: &str, status: u16, detail: &str, ids: RequestIds) -> Self {
        let detail: String = one_line(detail).chars().take(ERROR_BODY_CHARS).collect();
        let class = class_of_status(status, &detail, &Payload::default(), None);
        let mut message = format!("the {tool} tool answered {status}");
        if !detail.is_empty() {
            message.push_str(&format!(": {detail}"));
        }
        Self::new(WorkIqErrorKind::Tool, class, message, ids)
    }
}

impl fmt::Display for WorkIqError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Work IQ: {}. Fix ({}): {}",
            self.message.trim_end_matches('.'),
            self.fix.who,
            self.fix.action
        )?;
        if self.ids != RequestIds::default() {
            write!(f, " ({})", self.ids)?;
        }
        Ok(())
    }
}

impl std::error::Error for WorkIqError {}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Lowercase without separators, so `consent_required` and `ConsentRequired` compare equal.
fn normalized(text: &str) -> String {
    text.to_ascii_lowercase().replace(['_', '-', ' '], "")
}

fn parse_retry_after(value: &str) -> Option<Duration> {
    let seconds: f64 = value.trim().parse().ok()?;
    Duration::try_from_secs_f64(seconds).ok()
}

/// Signals in error text that decide the class whatever the status was.
fn class_from_text(text: &str) -> Option<Class> {
    let text = normalized(text);
    let has = |words: &[&str]| words.iter().any(|word| text.contains(word));
    if has(&["aadsts65001", "consentrequired"]) {
        Some(Class::Consent)
    } else if has(&["aadsts50105", "aadsts53003"]) {
        Some(Class::DeniedByPolicy)
    } else if has(&["spending", "billing"]) {
        Some(Class::Billing)
    } else if has(&["invalidgrant", "invalidtoken", "insufficientclaims"]) {
        Some(Class::SignInAgain)
    } else {
        None
    }
}

/// The fields Work IQ adds to a failed call.
#[derive(Default)]
struct Payload {
    stopped_with_error: bool,
    error_code: Option<String>,
    recovery_action: Option<String>,
    retry_after: Option<Duration>,
}

impl Payload {
    /// Reads the fields from `value` under `application/vnd.ms-workiq.retrieval` and at its
    /// top level.
    fn of(value: Option<&Value>) -> Self {
        let Some(value) = value else {
            return Payload::default();
        };
        let field = |name: &str| {
            value
                .get(RETRIEVAL_TYPE)
                .and_then(|inner| inner.get(name))
                .filter(|v| !v.is_null())
                .or_else(|| value.get(name).filter(|v| !v.is_null()))
        };
        let text = |name: &str| field(name).and_then(Value::as_str).map(str::to_string);
        Payload {
            stopped_with_error: text("stoppedReason").as_deref() == Some("error"),
            error_code: text("errorCode"),
            recovery_action: text("recoveryAction"),
            retry_after: field("retryAfterSeconds")
                .and_then(Value::as_f64)
                .and_then(|seconds| Duration::try_from_secs_f64(seconds).ok()),
        }
    }
}

const RETRIEVAL_TYPE: &str = "application/vnd.ms-workiq.retrieval";

/// The class of a failure with HTTP status `status`: signals in `text` decide first, then the
/// status, then the words and payload fields.
fn class_of_status(
    status: u16,
    text: &str,
    payload: &Payload,
    retry_after: Option<Duration>,
) -> Class {
    if let Some(class) = class_from_text(text) {
        return class;
    }
    match status {
        401 => Class::SignInAgain,
        402 => Class::Billing,
        403 => Class::DeniedByPolicy,
        429 => Class::Throttled(retry_after.or(payload.retry_after)),
        500..=599 => Class::Transient,
        _ => classify(text, payload),
    }
}

/// The class of a failure Work IQ reported in words and payload fields.
fn classify(text: &str, payload: &Payload) -> Class {
    let codes = format!(
        "{} {}",
        payload.error_code.as_deref().unwrap_or(""),
        payload.recovery_action.as_deref().unwrap_or("")
    );
    if let Some(class) = class_from_text(&format!("{text} {codes}")) {
        return class;
    }
    let codes = normalized(&codes);
    let has = |words: &[&str]| words.iter().any(|word| codes.contains(word));
    if payload.retry_after.is_some() || has(&["throttl", "ratelimit", "toomanyrequests"]) {
        Class::Throttled(payload.retry_after)
    } else if has(&["policy", "denied", "forbidden", "blocked", "notallowed"]) {
        Class::DeniedByPolicy
    } else if has(&["signin", "unauthorized", "reauth"]) {
        Class::SignInAgain
    } else if has(&["retry", "unavailable", "timeout", "transient"]) {
        Class::Transient
    } else {
        Class::Tool
    }
}

const EULA_MESSAGE: &str = "the Work IQ CLI says its EULA is not accepted";

/// Whether the Work IQ CLI is complaining about its EULA (it mentions the EULA or
/// `accept-eula`).
fn mentions_eula(text: &str) -> bool {
    text.to_ascii_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|word| word == "eula" || word == "accepteula")
}

fn fix_for(class: &Class, text: &str) -> Fix {
    if text == EULA_MESSAGE {
        return Fix {
            who: Who::User,
            action: "run `workiq accept-eula` yourself; Proofkin never accepts it for you"
                .to_string(),
        };
    }
    let text = normalized(text);
    let (who, action) = match class {
        Class::SignInAgain => (
            Who::User,
            "run `proofkin auth login workiq` to sign in again",
        ),
        Class::Consent => (
            Who::Admin,
            "grant admin consent to the Work IQ app in Microsoft Entra, then sign in again",
        ),
        Class::DeniedByPolicy if text.contains("aadsts50105") => (
            Who::Admin,
            "assign the user to the Work IQ app in Microsoft Entra (Enterprise applications, \
             Users and groups)",
        ),
        Class::DeniedByPolicy if text.contains("aadsts53003") => (
            Who::Admin,
            "review the Conditional Access policy that blocked the sign-in in Microsoft Entra",
        ),
        Class::DeniedByPolicy => (
            Who::Admin,
            "change the Work IQ MCP policy in the Microsoft 365 admin center (it takes up to \
             24 hours to apply); Proofkin does not retry",
        ),
        Class::Billing => (
            Who::Admin,
            "set up usage-based billing and a spending policy that covers this user and the \
             Work IQ API",
        ),
        Class::Throttled(Some(delay)) => {
            return Fix {
                who: Who::User,
                action: format!("wait {} seconds and try again", delay.as_secs_f64().ceil()),
            };
        }
        Class::Throttled(None) => (Who::User, "wait a little and try again"),
        Class::Transient => (
            Who::User,
            "try again in a moment; if it keeps failing, give the ids to support",
        ),
        Class::UncertainChange => (
            Who::User,
            "check in Microsoft 365 whether the change was made before asking for it again; \
             Proofkin does not send it again",
        ),
        Class::Tool => (
            Who::Support,
            "check the request; if it looks right, give the ids to support",
        ),
        Class::OverBudget => (
            Who::User,
            "raise the budget with `proofkin workiq budget`, or wait for it to reset",
        ),
    };
    Fix {
        who,
        action: action.to_string(),
    }
}

/// One message to send.
pub struct Outgoing {
    pub message: Value,
    /// The JSON-RPC id of the response to wait for; `None` for a notification.
    pub expect_id: Option<u64>,
    pub session_id: Option<String>,
    /// Sent as `MCP-Protocol-Version` once the session is initialized.
    pub protocol_version: Option<String>,
    pub bearer: String,
    pub client_request_id: String,
}

/// What came back.
pub struct Incoming {
    /// The JSON-RPC response with the expected id; `None` for a notification.
    pub body: Option<Value>,
    pub session_id: Option<String>,
    pub ids: RequestIds,
}

/// Carries MCP messages to Work IQ (streamable HTTP, or the CLI over stdio).
#[async_trait]
pub trait Transport: Send + Sync {
    async fn send(&self, outgoing: Outgoing) -> Result<Incoming, WorkIqError>;
}

/// Streamable HTTP: a POST per message, answered with JSON or a Server-Sent Events stream.
pub struct HttpTransport {
    http: reqwest::Client,
    url: String,
}

impl HttpTransport {
    pub fn new(url: &str) -> Result<Self> {
        Self::with_timeout(url, REQUEST_TIMEOUT)
    }

    /// A transport that gives up on a request after `timeout`.
    pub fn with_timeout(url: &str, timeout: Duration) -> Result<Self> {
        Ok(HttpTransport {
            http: http_client(timeout)?,
            url: url.to_string(),
        })
    }
}

pub(crate) fn http_client(timeout: Duration) -> Result<reqwest::Client> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let roots = rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("could not set up TLS")?
        .with_root_certificates(roots)
        .with_no_client_auth();
    reqwest::Client::builder()
        .tls_backend_preconfigured(tls)
        .timeout(timeout)
        .build()
        .context("could not build the HTTP client")
}

#[async_trait]
impl Transport for HttpTransport {
    async fn send(&self, outgoing: Outgoing) -> Result<Incoming, WorkIqError> {
        let sent_ids = RequestIds {
            client_request_id: Some(outgoing.client_request_id.clone()),
            ..RequestIds::default()
        };
        let transport_error = |e: reqwest::Error| {
            let what = if e.is_timeout() {
                "the request timed out".to_string()
            } else {
                format!("the request failed: {}", e.without_url())
            };
            WorkIqError::transport(what, sent_ids.clone())
        };
        let body = serde_json::to_vec(&outgoing.message)
            .map_err(|e| WorkIqError::protocol(e.to_string(), sent_ids.clone()))?;
        let mut request = self
            .http
            .post(&self.url)
            .bearer_auth(&outgoing.bearer)
            .header("Accept", "application/json, text/event-stream")
            .header("Content-Type", "application/json")
            .header("client-request-id", &outgoing.client_request_id)
            .body(body);
        if let Some(version) = &outgoing.protocol_version {
            request = request.header("MCP-Protocol-Version", version);
        }
        if let Some(session) = &outgoing.session_id {
            request = request.header("Mcp-Session-Id", session);
        }
        let response = request.send().await.map_err(transport_error)?;
        let status = response.status();
        let ids = RequestIds::from_headers(response.headers(), &outgoing.client_request_id);
        let session_id = response
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        let header_text = |name: &str| {
            response
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        let www_authenticate = header_text("www-authenticate");
        let retry_after = header_text("retry-after");
        let bytes = response.bytes().await.map_err(transport_error)?;
        if !status.is_success() {
            return Err(WorkIqError::http(
                status,
                www_authenticate.as_deref(),
                retry_after.as_deref(),
                &String::from_utf8_lossy(&bytes),
                ids,
            ));
        }
        let Some(expect_id) = outgoing.expect_id else {
            return Ok(Incoming {
                body: None,
                session_id,
                ids,
            });
        };
        let protocol_error = |message: String| WorkIqError::protocol(message, ids.clone());
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| protocol_error("the reply is not UTF-8 text".to_string()))?;
        let body = if content_type.contains("text/event-stream") {
            find_sse_response(text, expect_id).map_err(protocol_error)?
        } else if content_type.contains("application/json") {
            serde_json::from_str(text)
                .map_err(|e| protocol_error(format!("the reply is not JSON: {e}")))?
        } else {
            return Err(protocol_error(format!(
                "unexpected content type {content_type:?}"
            )));
        };
        Ok(Incoming {
            body: Some(body),
            session_id,
            ids,
        })
    }
}

/// The JSON-RPC response with `id` in a Server-Sent Events body. Events are split by a blank
/// line and their `data:` lines joined; notifications and other messages are skipped.
fn find_sse_response(text: &str, id: u64) -> Result<Value, String> {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    for event in text.split("\n\n") {
        let data: Vec<&str> = event
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(|data| data.strip_prefix(' ').unwrap_or(data))
            .collect();
        if data.is_empty() {
            continue;
        }
        let Ok(message) = serde_json::from_str::<Value>(&data.join("\n")) else {
            continue;
        };
        let answers = message.get("id") == Some(&json!(id));
        if answers && (message.get("result").is_some() || message.get("error").is_some()) {
            return Ok(message);
        }
    }
    Err(format!("the event stream has no response to request {id}"))
}

/// A tool Work IQ offers.
#[derive(Debug, Clone, Deserialize)]
pub struct Tool {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, rename = "inputSchema")]
    pub input_schema: Value,
    /// MCP's hints about what it does (`readOnlyHint`, `destructiveHint`, ...).
    #[serde(default)]
    pub annotations: Option<Value>,
}

impl Tool {
    /// Whether Work IQ says the tool changes things: not read only, or destructive. A tool
    /// without hints says nothing either way.
    pub fn says_it_changes(&self) -> bool {
        let hint = |name: &str| {
            self.annotations
                .as_ref()
                .and_then(|a| a.get(name))
                .and_then(Value::as_bool)
        };
        hint("readOnlyHint") == Some(false) || hint("destructiveHint") == Some(true)
    }
}

/// The answer to `tools/call`.
#[derive(Debug, Clone)]
pub struct ToolResult {
    pub content: Vec<Value>,
    pub structured_content: Option<Value>,
    pub is_error: bool,
    pub ids: RequestIds,
}

impl ToolResult {
    /// Work IQ's structured answer (or JSON in its text), and its status when given.
    pub(crate) fn answer_with_status(&self) -> (Cow<'_, Value>, Option<u16>) {
        let answer = match &self.structured_content {
            Some(answer) => Cow::Borrowed(answer),
            None => self
                .content
                .iter()
                .filter_map(|item| item.get("text")?.as_str())
                .find_map(|text| serde_json::from_str(text).ok())
                .map_or(Cow::Borrowed(&Value::Null), Cow::Owned),
        };
        let status = ["status", "statusCode", "httpStatusCode"]
            .iter()
            .find_map(|key| number(answer.get(*key)))
            .map(|status| u16::try_from(status).unwrap_or(u16::MAX));
        (answer, status)
    }
}

#[derive(Clone, PartialEq, Eq)]
struct Session {
    id: Option<String>,
    protocol_version: String,
}

struct Reply {
    result: Value,
    ids: RequestIds,
    session_id: Option<String>,
}

/// An MCP client for Work IQ.
pub struct Client {
    transport: Arc<dyn Transport>,
    tokens: Arc<dyn TokenSource>,
    session: Mutex<Option<Session>>,
    tools: Mutex<Option<Vec<Tool>>>,
    sleeper: Arc<dyn Sleeper>,
    next_id: AtomicU64,
    audit: Option<Arc<Audit>>,
    context: CallContext,
}

/// Whether a call may be sent again after a failure that leaves it unknown whether it ran.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Repeat {
    /// It only reads.
    Safe,
    /// It changes things in Microsoft 365, or isn't known to only read.
    Never,
}

impl Repeat {
    fn of_tool(name: &str) -> Self {
        if READ_TOOLS.contains(&name) {
            Repeat::Safe
        } else {
            Repeat::Never
        }
    }
}

impl Client {
    /// A client for the MCP endpoint at `url` ([`DEFAULT_URL`] for the hosted service).
    pub fn new(url: &str, tokens: Arc<dyn TokenSource>) -> Result<Self> {
        Ok(Self::with_transport(
            Arc::new(HttpTransport::new(url)?),
            tokens,
        ))
    }

    pub fn with_transport(transport: Arc<dyn Transport>, tokens: Arc<dyn TokenSource>) -> Self {
        Client {
            transport,
            tokens,
            session: Mutex::new(None),
            tools: Mutex::new(None),
            sleeper: Arc::new(TokioSleeper),
            next_id: AtomicU64::new(1),
            audit: None,
            context: CallContext::command(),
        }
    }

    /// Audits every tool call in `audit`, and refuses the calls its budgets don't allow.
    pub fn with_audit(mut self, audit: Arc<Audit>) -> Self {
        self.audit = Some(audit);
        self
    }

    /// The purpose and audience of the calls that don't give their own (see
    /// [`Client::call_tool_in`]); a command for the owner when not set.
    pub fn with_context(mut self, context: CallContext) -> Self {
        self.context = context;
        self
    }

    /// Waits between attempts with `sleeper` instead of the clock.
    pub fn with_sleeper(mut self, sleeper: Arc<dyn Sleeper>) -> Self {
        self.sleeper = sleeper;
        self
    }

    /// The tools Work IQ offers. Asked once per client.
    pub async fn list_tools(&self) -> Result<Vec<Tool>> {
        let mut cached = self.tools.lock().await;
        if let Some(tools) = cached.as_ref() {
            return Ok(tools.clone());
        }
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let params = match &cursor {
                Some(cursor) => json!({ "cursor": cursor }),
                None => json!({}),
            };
            let reply = self.rpc("tools/list", params, Repeat::Safe).await?;
            let page = reply.result.get("tools").cloned().unwrap_or(json!([]));
            let page: Vec<Tool> = serde_json::from_value(page)
                .context("Work IQ sent a tools list that doesn't parse")?;
            tools.extend(page);
            cursor = reply
                .result
                .get("nextCursor")
                .and_then(Value::as_str)
                .map(str::to_string);
            if cursor.is_none() {
                break;
            }
        }
        *cached = Some(tools.clone());
        Ok(tools)
    }

    /// Calls a tool. A tool that reports a failure (`isError`, `stoppedReason: "error"`) is an
    /// `Err` with a [`WorkIqError`]. Only the tools in [`READ_TOOLS`] are sent again after a
    /// transient failure; any other call is sent once.
    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<ToolResult> {
        self.call_tool_in(&self.context, name, arguments).await
    }

    /// [`Client::call_tool`] for a given purpose and audience. With an audit, the call is
    /// refused before it is sent when a budget is used up, and is recorded however it ends.
    pub async fn call_tool_in(
        &self,
        context: &CallContext,
        name: &str,
        arguments: Value,
    ) -> Result<ToolResult> {
        let Some(audit) = &self.audit else {
            return self.send_tool(name, arguments).await;
        };
        let pending = audit.begin(context, name, &arguments)?;
        let result = self.send_tool(name, arguments).await;
        pending.finish(&result);
        result
    }

    async fn send_tool(&self, name: &str, arguments: Value) -> Result<ToolResult> {
        let reply = self
            .rpc(
                "tools/call",
                json!({ "name": name, "arguments": arguments }),
                Repeat::of_tool(name),
            )
            .await?;
        let result = reply.result;
        let content = match result.get("content") {
            Some(Value::Array(items)) => items.clone(),
            _ => Vec::new(),
        };
        Ok(ToolResult {
            content,
            structured_content: result
                .get("structuredContent")
                .filter(|v| !v.is_null())
                .cloned(),
            is_error: result
                .get("isError")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            ids: reply.ids,
        })
    }

    /// Cited search over Microsoft 365. `strategy` is `grounding` (Work IQ's default when
    /// `None`) or `copilot`; `capabilities` are Work IQ capability names (`Email`, `Meetings`,
    /// `OneDriveAndSharePoint`, `TeamsMessages`, `People`, `GraphConnectors`), none for all.
    pub async fn retrieve(
        &self,
        query: &str,
        strategy: Option<&str>,
        capabilities: &[&str],
    ) -> Result<ToolResult> {
        self.retrieve_in(&self.context, query, strategy, capabilities)
            .await
    }

    /// [`Client::retrieve`] for a given purpose and audience.
    pub async fn retrieve_in(
        &self,
        context: &CallContext,
        query: &str,
        strategy: Option<&str>,
        capabilities: &[&str],
    ) -> Result<ToolResult> {
        let mut arguments = json!({ "query": query });
        if let Some(strategy) = strategy {
            arguments["strategy"] = json!(strategy);
        }
        if !capabilities.is_empty() {
            let capabilities: Vec<Value> = capabilities
                .iter()
                .map(|name| json!({ "name": name }))
                .collect();
            arguments["capabilities"] = json!(capabilities);
        }
        self.call_offered_in(context, "retrieve", arguments).await
    }

    /// [`Client::ask`] for a given purpose and audience.
    pub async fn ask_in(
        &self,
        context: &CallContext,
        question: &str,
        agent_id: Option<&str>,
        conversation_id: Option<&str>,
    ) -> Result<ToolResult> {
        let mut arguments = json!({ "question": question });
        if let Some(agent_id) = agent_id {
            arguments["agentId"] = json!(agent_id);
        }
        if let Some(conversation_id) = conversation_id {
            arguments["conversationId"] = json!(conversation_id);
        }
        self.call_offered_in(context, "ask", arguments).await
    }

    /// An answer from Microsoft 365 Copilot, or from the declarative agent `agent_id`.
    pub async fn ask(
        &self,
        question: &str,
        agent_id: Option<&str>,
        conversation_id: Option<&str>,
    ) -> Result<ToolResult> {
        self.ask_in(&self.context, question, agent_id, conversation_id)
            .await
    }

    /// Entities by their Work IQ paths (Microsoft Graph paths such as `/me/messages`).
    pub async fn fetch(&self, entity_urls: &[&str]) -> Result<ToolResult> {
        self.fetch_in(&self.context, entity_urls).await
    }

    /// [`Client::fetch`] for a given purpose and audience.
    pub async fn fetch_in(
        &self,
        context: &CallContext,
        entity_urls: &[&str],
    ) -> Result<ToolResult> {
        self.call_offered_in(context, "fetch", json!({ "entityUrls": entity_urls }))
            .await
    }

    /// A file's bytes by its Work IQ path (such as `/shares/{id}/driveItem/content`), for a
    /// given purpose and audience; Work IQ gives them base64-encoded, up to 4 MB.
    pub async fn fetch_blob_in(&self, context: &CallContext, path: &str) -> Result<ToolResult> {
        self.call_offered_in(context, "fetch_blob", json!({ "path": path }))
            .await
    }

    pub async fn list_agents(&self) -> Result<ToolResult> {
        self.call_offered("list_agents", json!({})).await
    }

    async fn call_offered(&self, name: &str, arguments: Value) -> Result<ToolResult> {
        self.call_offered_in(&self.context, name, arguments).await
    }

    async fn call_offered_in(
        &self,
        context: &CallContext,
        name: &str,
        arguments: Value,
    ) -> Result<ToolResult> {
        let tools = self.list_tools().await?;
        if !tools.iter().any(|tool| tool.name == name) {
            let offered: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
            bail!(
                "Work IQ does not offer the {name} tool (it offers: {})",
                offered.join(", ")
            );
        }
        self.call_tool_in(context, name, arguments).await
    }

    /// A request in the current session. A 404 on a session means the server forgot it: a new
    /// session is started once and the request sent again.
    async fn rpc(&self, method: &str, params: Value, repeat: Repeat) -> Result<Reply> {
        let mut renewed = false;
        loop {
            let session = self.session().await?;
            let outcome = self
                .retrying(repeat, || self.call(&session, method, &params))
                .await;
            match outcome {
                Err(e) if !renewed && session.id.is_some() && is_http_status(&e, 404) => {
                    renewed = true;
                    self.forget(&session).await;
                }
                other => return other,
            }
        }
    }

    async fn call(&self, session: &Session, method: &str, params: &Value) -> Result<Reply> {
        let reply = self.request(Some(session), method, params.clone()).await?;
        if method == "tools/call" {
            let tool = params.get("name").and_then(Value::as_str).unwrap_or("");
            check_tool_result(tool, &reply.result, &reply.ids)?;
        }
        Ok(reply)
    }

    /// Runs `attempt`, sending it again where that is safe:
    /// - a 401 asks the token source to refresh, once, and the call is sent once more;
    /// - a throttled read waits for the server's delay if it is at most [`MAX_THROTTLE_WAIT`],
    ///   at most [`MAX_RETRIES`] times;
    /// - a transient failure of a read backs off with jitter, at most [`MAX_RETRIES`] times;
    /// - a change that timed out or lost its connection is reported as uncertain, not sent
    ///   again. Every other failure is returned as it is.
    async fn retrying<T, F, Fut>(&self, repeat: Repeat, mut attempt: F) -> Result<T>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        let (mut refreshed, mut throttled, mut transient) = (false, 0, 0);
        loop {
            let error = match attempt().await {
                Ok(value) => return Ok(value),
                Err(error) => error,
            };
            let Some(failure) = error.downcast_ref::<WorkIqError>() else {
                return Err(error);
            };
            let (class, kind) = (failure.class.clone(), failure.kind.clone());
            match class {
                Class::SignInAgain if !refreshed && kind == WorkIqErrorKind::Http(401) => {
                    refreshed = true;
                    if let Err(e) = self.tokens.refresh().await {
                        let message =
                            format!("{}; refreshing the token failed: {e:#}", failure.message);
                        let failure = WorkIqError::new(kind, class, message, failure.ids.clone());
                        return Err(failure.into());
                    }
                }
                Class::Throttled(Some(delay))
                    if repeat == Repeat::Safe
                        && throttled < MAX_RETRIES
                        && delay <= MAX_THROTTLE_WAIT =>
                {
                    throttled += 1;
                    self.sleeper.sleep(delay).await;
                }
                Class::Transient if repeat == Repeat::Safe && transient < MAX_RETRIES => {
                    self.sleeper.sleep(backoff(transient)).await;
                    transient += 1;
                }
                Class::Transient
                    if repeat == Repeat::Never && kind == WorkIqErrorKind::Transport =>
                {
                    let message = format!(
                        "the change may or may not have been made, because {}",
                        failure.message
                    );
                    let failure = WorkIqError::new(
                        kind,
                        Class::UncertainChange,
                        message,
                        failure.ids.clone(),
                    );
                    return Err(failure.into());
                }
                _ => return Err(error),
            }
        }
    }

    async fn session(&self) -> Result<Session> {
        let mut current = self.session.lock().await;
        if let Some(session) = current.as_ref() {
            return Ok(session.clone());
        }
        let session = self.initialize().await?;
        *current = Some(session.clone());
        Ok(session)
    }

    // Only drops the session that failed: a concurrent request may have renewed it already.
    async fn forget(&self, failed: &Session) {
        let mut current = self.session.lock().await;
        if current.as_ref() == Some(failed) {
            *current = None;
        }
    }

    async fn initialize(&self) -> Result<Session> {
        let params = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "proofkin", "version": env!("CARGO_PKG_VERSION") },
        });
        let reply = self
            .retrying(Repeat::Safe, || {
                self.request(None, "initialize", params.clone())
            })
            .await?;
        let protocol_version = reply
            .result
            .get("protocolVersion")
            .and_then(Value::as_str)
            .unwrap_or(PROTOCOL_VERSION)
            .to_string();
        let session = Session {
            id: reply.session_id,
            protocol_version,
        };
        let initialized = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        self.retrying(Repeat::Safe, || {
            self.post(initialized.clone(), None, Some(&session))
        })
        .await?;
        Ok(session)
    }

    async fn request(
        &self,
        session: Option<&Session>,
        method: &str,
        params: Value,
    ) -> Result<Reply> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let message = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let incoming = self.post(message, Some(id), session).await?;
        let ids = incoming.ids;
        let body = incoming.body.ok_or_else(|| {
            WorkIqError::protocol(format!("no response to {method}"), ids.clone())
        })?;
        if let Some(error) = body.get("error") {
            let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
            let class = classify(message, &Payload::of(error.get("data")));
            return Err(WorkIqError::new(
                WorkIqErrorKind::Rpc(code),
                class,
                format!("{method} failed with error {code}: {message}"),
                ids,
            )
            .into());
        }
        let result = body.get("result").cloned().ok_or_else(|| {
            WorkIqError::protocol(
                format!("the response to {method} has no result"),
                ids.clone(),
            )
        })?;
        Ok(Reply {
            result,
            ids,
            session_id: incoming.session_id,
        })
    }

    async fn post(
        &self,
        message: Value,
        expect_id: Option<u64>,
        session: Option<&Session>,
    ) -> Result<Incoming> {
        let bearer = self
            .tokens
            .token()
            .await
            .context("could not get a Work IQ token")?;
        let outgoing = Outgoing {
            message,
            expect_id,
            session_id: session.and_then(|s| s.id.clone()),
            protocol_version: session.map(|s| s.protocol_version.clone()),
            bearer,
            client_request_id: uuid::Uuid::new_v4().to_string(),
        };
        Ok(self.transport.send(outgoing).await?)
    }
}

fn is_http_status(error: &anyhow::Error, status: u16) -> bool {
    matches!(
        error.downcast_ref::<WorkIqError>(),
        Some(WorkIqError { kind: WorkIqErrorKind::Http(s), .. }) if *s == status
    )
}

/// How long to wait before sending a read again after its `attempt`th transient failure
/// (counting from 0): 0.5 s, then 1 s, each cut by up to half at random.
fn backoff(attempt: u32) -> Duration {
    let full = BACKOFF_BASE * 2u32.pow(attempt);
    let half = full / 2;
    let jitter = uuid::Uuid::new_v4().as_u128() % (half.as_nanos() + 1);
    half + Duration::from_nanos(jitter as u64)
}

/// A tool result that says the call failed (`isError`, or `stoppedReason: "error"` in the
/// payload) is an error, classified from its text and payload fields, that keeps what the
/// tool said (`tool_text`).
fn check_tool_result(tool: &str, result: &Value, ids: &RequestIds) -> Result<()> {
    let payload = Payload::of(result.get("structuredContent"));
    let is_error = result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !is_error && !payload.stopped_with_error {
        return Ok(());
    }
    let content = result
        .get("content")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let detail = reported_text(content);
    let mut message = format!("the {tool} tool reported an error");
    if !detail.is_empty() {
        message.push_str(&format!(": {detail}"));
    }
    if let Some(code) = &payload.error_code {
        message.push_str(&format!(" (error code {code})"));
    }
    if let Some(action) = &payload.recovery_action {
        message.push_str(&format!(" (recovery action: {action})"));
    }
    // Classified by all the tool said, before it is redacted and cut.
    let class = classify(&content_text(content), &payload);
    let mut failure = WorkIqError::new(WorkIqErrorKind::Tool, class, message, ids.clone());
    failure.tool_text = Some(detail);
    Err(failure.into())
}

/// What a tool said in the text of its `content` when it reported a failure, for an error
/// message: on one line, without control characters, its secrets redacted, at most
/// [`ERROR_BODY_CHARS`] characters.
pub(crate) fn reported_text(content: &[Value]) -> String {
    let (text, _) = crate::redact::redact(&content_text(content));
    let text: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let text: String = one_line(&text).chars().take(ERROR_BODY_CHARS).collect();
    text.trim_end().to_owned()
}

/// The text parts of a tool's `content`, joined with spaces.
fn content_text(content: &[Value]) -> String {
    content
        .iter()
        .filter_map(|item| item.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use std::collections::{HashSet, VecDeque};
    use std::sync::atomic::{AtomicBool, AtomicUsize};

    use axum::Router;
    use axum::extract::State;
    use axum::http::{HeaderMap, HeaderValue, StatusCode};
    use axum::response::{IntoResponse, Response};
    use axum::routing::post;

    use super::*;

    struct Seen {
        method: String,
        headers: HeaderMap,
        params: Value,
    }

    enum Answer {
        Http(u16, String),
        Result(Value),
        RpcError(i64, String),
    }

    /// The scripted answer to one `tools/call`.
    struct Canned {
        answer: Answer,
        headers: Vec<(&'static str, String)>,
        delay_ms: u64,
    }

    impl Canned {
        fn new(answer: Answer) -> Self {
            Canned {
                answer,
                headers: Vec::new(),
                delay_ms: 0,
            }
        }

        fn http(status: u16, body: &str) -> Self {
            Self::new(Answer::Http(status, body.to_string()))
        }

        fn result(result: Value) -> Self {
            Self::new(Answer::Result(result))
        }

        fn ok() -> Self {
            Self::result(json!({ "content": [{ "type": "text", "text": "ok" }], "isError": false }))
        }

        fn failed(text: &str) -> Self {
            Self::result(json!({ "content": [{ "type": "text", "text": text }], "isError": true }))
        }

        fn rpc_error(code: i64, message: &str) -> Self {
            Self::new(Answer::RpcError(code, message.to_string()))
        }

        fn header(mut self, name: &'static str, value: &str) -> Self {
            self.headers.push((name, value.to_string()));
            self
        }

        fn delayed(mut self, ms: u64) -> Self {
            self.delay_ms = ms;
            self
        }
    }

    struct Fake {
        seen: std::sync::Mutex<Vec<Seen>>,
        sessions: std::sync::Mutex<Vec<String>>,
        tools: std::sync::Mutex<Vec<String>>,
        canned: std::sync::Mutex<VecDeque<Canned>>,
        counter: AtomicU64,
        sse: AtomicBool,
        always_404: AtomicBool,
    }

    impl Fake {
        fn count(&self, method: &str) -> usize {
            self.seen
                .lock()
                .unwrap()
                .iter()
                .filter(|s| s.method == method)
                .count()
        }

        fn header(&self, index: usize, name: &str) -> Option<String> {
            header_of(&self.seen.lock().unwrap()[index].headers, name)
        }
    }

    fn header_of(headers: &HeaderMap, name: &str) -> Option<String> {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    }

    async fn handle(State(fake): State<Arc<Fake>>, headers: HeaderMap, body: String) -> Response {
        let message: Value = serde_json::from_str(&body).unwrap();
        let method = message["method"].as_str().unwrap().to_string();
        let n = fake.counter.fetch_add(1, Ordering::SeqCst) + 1;
        fake.seen.lock().unwrap().push(Seen {
            method: method.clone(),
            headers: headers.clone(),
            params: message["params"].clone(),
        });

        let mut out = HeaderMap::new();
        for (name, value) in [
            ("request-id", format!("req-{n}")),
            ("x-ms-request-id", format!("ms-req-{n}")),
            ("x-ms-correlation-id", format!("corr-{n}")),
        ] {
            out.insert(name, HeaderValue::from_str(&value).unwrap());
        }
        if let Some(sent) = headers.get("client-request-id") {
            out.insert("client-request-id", sent.clone());
        }

        if method == "notifications/initialized" {
            return (StatusCode::ACCEPTED, out).into_response();
        }
        let id = message["id"].clone();
        let reply = if method == "initialize" {
            let session = format!("session-{n}");
            fake.sessions.lock().unwrap().push(session.clone());
            out.insert("mcp-session-id", HeaderValue::from_str(&session).unwrap());
            json!({ "jsonrpc": "2.0", "id": id, "result": {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "fake-workiq", "version": "0" },
            }})
        } else {
            let session = header_of(&headers, "mcp-session-id").unwrap_or_default();
            let known = fake.sessions.lock().unwrap().contains(&session);
            if fake.always_404.load(Ordering::SeqCst) || !known {
                return (StatusCode::NOT_FOUND, out, "unknown session").into_response();
            }
            let canned = if method == "tools/call" {
                fake.canned.lock().unwrap().pop_front()
            } else {
                None
            };
            if let Some(canned) = canned {
                if canned.delay_ms > 0 {
                    tokio::time::sleep(Duration::from_millis(canned.delay_ms)).await;
                }
                for (name, value) in canned.headers {
                    out.insert(name, HeaderValue::from_str(&value).unwrap());
                }
                let (status, body) = match canned.answer {
                    Answer::Http(status, body) => (status, body),
                    Answer::Result(result) => (
                        200,
                        json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string(),
                    ),
                    Answer::RpcError(code, message) => (
                        200,
                        json!({ "jsonrpc": "2.0", "id": id,
                            "error": { "code": code, "message": message } })
                        .to_string(),
                    ),
                };
                out.insert("content-type", HeaderValue::from_static("application/json"));
                return (StatusCode::from_u16(status).unwrap(), out, body).into_response();
            }
            match method.as_str() {
                "tools/list" => {
                    let tools: Vec<Value> = fake
                        .tools
                        .lock()
                        .unwrap()
                        .iter()
                        .map(|name| json!({ "name": name, "inputSchema": { "type": "object" } }))
                        .collect();
                    json!({ "jsonrpc": "2.0", "id": id, "result": { "tools": tools } })
                }
                "tools/call" if message["params"]["name"] == "boom" => {
                    json!({ "jsonrpc": "2.0", "id": id,
                        "error": { "code": -32000, "message": "boom" } })
                }
                "tools/call" => json!({ "jsonrpc": "2.0", "id": id, "result": {
                    "content": [{ "type": "text", "text": "ok" }],
                    "structuredContent": message["params"].clone(),
                    "isError": false,
                }}),
                other => panic!("unexpected method {other}"),
            }
        };

        if fake.sse.load(Ordering::SeqCst) {
            let progress = json!({ "jsonrpc": "2.0", "method": "notifications/progress",
                "params": { "progressToken": "t", "progress": 1 } });
            let other = json!({ "jsonrpc": "2.0", "id": 9999, "result": { "decoy": true } });
            let stream = format!(
                "event: message\ndata: {progress}\n\nevent: message\ndata: {other}\n\n\
                 id: 1\nevent: message\ndata: {reply}\n\n"
            );
            out.insert(
                "content-type",
                HeaderValue::from_static("text/event-stream"),
            );
            (out, stream).into_response()
        } else {
            out.insert("content-type", HeaderValue::from_static("application/json"));
            (out, reply.to_string()).into_response()
        }
    }

    async fn start() -> (Arc<Fake>, String) {
        let fake = Arc::new(Fake {
            seen: Default::default(),
            sessions: Default::default(),
            tools: std::sync::Mutex::new(
                ["retrieve", "ask", "fetch", "list_agents"]
                    .map(String::from)
                    .to_vec(),
            ),
            canned: Default::default(),
            counter: AtomicU64::new(0),
            sse: AtomicBool::new(false),
            always_404: AtomicBool::new(false),
        });
        let app = Router::new()
            .route("/mcp", post(handle))
            .with_state(fake.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (fake, url)
    }

    fn client(url: &str) -> Client {
        Client::new(url, Arc::new(FixedToken("test-token".to_string()))).unwrap()
    }

    #[tokio::test]
    async fn every_request_sends_the_bearer_and_content_headers_with_a_fresh_client_request_id() {
        let (fake, url) = start().await;
        let client = client(&url);
        client.list_tools().await.unwrap();
        client.call_tool("retrieve", json!({})).await.unwrap();

        assert_eq!(fake.seen.lock().unwrap().len(), 4);
        let mut ids = HashSet::new();
        for index in 0..4 {
            let header = |name: &str| fake.header(index, name);
            assert_eq!(
                header("authorization").as_deref(),
                Some("Bearer test-token")
            );
            assert_eq!(
                header("accept").as_deref(),
                Some("application/json, text/event-stream")
            );
            assert_eq!(header("content-type").as_deref(), Some("application/json"));
            let id = header("client-request-id").unwrap();
            let parsed = uuid::Uuid::parse_str(&id).unwrap();
            assert_eq!((id.len(), parsed.get_version_num()), (36, 4));
            assert!(ids.insert(id), "client-request-id repeated");
        }
    }

    #[tokio::test]
    async fn initialize_announces_the_protocol_and_client_then_sends_initialized() {
        let (fake, url) = start().await;
        client(&url).list_tools().await.unwrap();

        let seen = fake.seen.lock().unwrap();
        let methods: Vec<&str> = seen.iter().map(|s| s.method.as_str()).collect();
        assert_eq!(
            methods,
            ["initialize", "notifications/initialized", "tools/list"]
        );
        assert_eq!(seen[0].params["protocolVersion"], "2025-06-18");
        assert_eq!(seen[0].params["clientInfo"]["name"], "proofkin");
        assert_eq!(
            seen[0].params["clientInfo"]["version"],
            env!("CARGO_PKG_VERSION")
        );
        assert_eq!(header_of(&seen[0].headers, "mcp-protocol-version"), None);
        for later in &seen[1..] {
            assert_eq!(
                header_of(&later.headers, "mcp-protocol-version").as_deref(),
                Some("2025-06-18")
            );
        }
    }

    #[tokio::test]
    async fn the_session_id_is_kept_and_sent_on_later_requests() {
        let (fake, url) = start().await;
        let client = client(&url);
        client.list_tools().await.unwrap();
        client.call_tool("retrieve", json!({})).await.unwrap();

        assert_eq!(fake.header(0, "mcp-session-id"), None);
        for index in 1..4 {
            assert_eq!(
                fake.header(index, "mcp-session-id").as_deref(),
                Some("session-1")
            );
        }
        assert_eq!(fake.count("initialize"), 1);
    }

    #[tokio::test]
    async fn a_forgotten_session_is_renewed_and_the_request_sent_again() {
        let (fake, url) = start().await;
        let client = client(&url);
        client.call_tool("retrieve", json!({})).await.unwrap();
        fake.sessions.lock().unwrap().clear();

        let result = client
            .call_tool("retrieve", json!({"query": "q"}))
            .await
            .unwrap();
        assert!(!result.is_error);
        assert_eq!(fake.count("initialize"), 2);
        assert_eq!(fake.count("tools/call"), 3);
        let seen = fake.seen.lock().unwrap();
        let last = seen.last().unwrap();
        assert_eq!(last.params["arguments"]["query"], "q");
        let renewed = header_of(&last.headers, "mcp-session-id").unwrap();
        assert_ne!(renewed, "session-1");
        assert!(fake.sessions.lock().unwrap().contains(&renewed));
    }

    #[tokio::test]
    async fn a_session_that_is_forgotten_again_fails_after_one_renewal() {
        let (fake, url) = start().await;
        let client = client(&url);
        client.call_tool("retrieve", json!({})).await.unwrap();
        fake.always_404.store(true, Ordering::SeqCst);

        let error = client.call_tool("retrieve", json!({})).await.unwrap_err();
        let error = error.downcast_ref::<WorkIqError>().unwrap();
        assert_eq!(error.kind, WorkIqErrorKind::Http(404));
        assert!(error.ids.correlation_id.is_some());
        assert_eq!(fake.count("initialize"), 2);
        assert_eq!(fake.count("tools/call"), 3);
    }

    #[tokio::test]
    async fn json_replies_are_read_as_one_response() {
        let (_fake, url) = start().await;
        let client = client(&url);
        let result = client
            .retrieve(
                "budget",
                Some("grounding"),
                &["Email", "OneDriveAndSharePoint"],
            )
            .await
            .unwrap();

        assert!(!result.is_error);
        assert_eq!(result.content[0]["text"], "ok");
        let call = result.structured_content.unwrap();
        assert_eq!(call["name"], "retrieve");
        assert_eq!(call["arguments"]["query"], "budget");
        assert_eq!(call["arguments"]["strategy"], "grounding");
        assert_eq!(
            call["arguments"]["capabilities"],
            json!([{ "name": "Email" }, { "name": "OneDriveAndSharePoint" }])
        );

        let plain = client.retrieve("budget", None, &[]).await.unwrap();
        assert_eq!(
            plain.structured_content.unwrap()["arguments"],
            json!({ "query": "budget" })
        );
        let fetched = client.fetch(&["/me/events"]).await.unwrap();
        assert_eq!(
            fetched.structured_content.unwrap()["arguments"],
            json!({ "entityUrls": ["/me/events"] })
        );
    }

    #[tokio::test]
    async fn sse_replies_skip_notifications_and_find_the_matching_response() {
        let (fake, url) = start().await;
        fake.sse.store(true, Ordering::SeqCst);
        let client = client(&url);
        let result = client
            .ask("what did we decide?", Some("agent-1"), Some("conv-1"))
            .await
            .unwrap();

        let call = result.structured_content.unwrap();
        assert_eq!(call["name"], "ask");
        assert_eq!(call["arguments"]["question"], "what did we decide?");
        assert_eq!(call["arguments"]["agentId"], "agent-1");
        assert_eq!(call["arguments"]["conversationId"], "conv-1");
        assert!(result.ids.request_id.is_some());
    }

    #[tokio::test]
    async fn tools_list_is_cached_per_client() {
        let (fake, url) = start().await;
        let first = client(&url);
        first.list_tools().await.unwrap();
        first.fetch(&["a/b"]).await.unwrap();
        first.list_agents().await.unwrap();
        assert_eq!(fake.count("tools/list"), 1);

        let names: Vec<String> = first
            .list_tools()
            .await
            .unwrap()
            .into_iter()
            .map(|t| t.name)
            .collect();
        assert_eq!(names, ["retrieve", "ask", "fetch", "list_agents"]);
        assert_eq!(fake.count("tools/list"), 1);

        client(&url).list_tools().await.unwrap();
        assert_eq!(fake.count("tools/list"), 2);
    }

    #[tokio::test]
    async fn a_tool_the_server_does_not_offer_is_a_clear_error() {
        let (fake, url) = start().await;
        fake.tools.lock().unwrap().retain(|name| name != "fetch");

        let error = client(&url).fetch(&["a/b"]).await.unwrap_err().to_string();
        assert!(error.contains("does not offer the fetch tool"), "{error}");
        assert!(error.contains("retrieve"), "{error}");
        assert_eq!(fake.count("tools/call"), 0);
    }

    #[tokio::test]
    async fn request_and_correlation_ids_are_kept_with_results_and_errors() {
        let (fake, url) = start().await;
        let client = client(&url);
        let result = client.call_tool("retrieve", json!({})).await.unwrap();

        let n = fake.counter.load(Ordering::SeqCst);
        let sent = fake.header(n as usize - 1, "client-request-id");
        assert_eq!(result.ids.request_id, Some(format!("req-{n}")));
        assert_eq!(result.ids.ms_request_id, Some(format!("ms-req-{n}")));
        assert_eq!(result.ids.correlation_id, Some(format!("corr-{n}")));
        assert_eq!(result.ids.client_request_id, sent);

        let error = client.call_tool("boom", json!({})).await.unwrap_err();
        let n = fake.counter.load(Ordering::SeqCst);
        let typed = error.downcast_ref::<WorkIqError>().unwrap();
        assert_eq!(typed.kind, WorkIqErrorKind::Rpc(-32000));
        assert_eq!(typed.ids.request_id, Some(format!("req-{n}")));
        assert_eq!(typed.ids.correlation_id, Some(format!("corr-{n}")));
        assert!(error.to_string().contains(&format!("corr-{n}")), "{error}");
        assert!(error.to_string().contains("boom"), "{error}");
    }

    #[test]
    fn sse_events_split_on_blank_lines_and_join_data_lines() {
        let stream = "id: 0\r\ndata:\r\n\r\n\
                      : keep-alive\r\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\r\n\r\n\
                      data: {\"jsonrpc\":\"2.0\",\"id\":8,\"result\":{}}\r\n\r\n\
                      data: {\"jsonrpc\":\"2.0\",\r\ndata: \"id\":7,\"result\":{\"ok\":true}}\r\n\r\n";
        let found = find_sse_response(stream, 7).unwrap();
        assert_eq!(found["result"]["ok"], true);
        assert!(find_sse_response(stream, 6).is_err());
    }

    #[derive(Default)]
    struct Recorder(std::sync::Mutex<Vec<Duration>>);

    #[async_trait]
    impl Sleeper for Recorder {
        async fn sleep(&self, duration: Duration) {
            self.0.lock().unwrap().push(duration);
        }
    }

    struct Tokens {
        current: std::sync::Mutex<String>,
        refreshes: AtomicUsize,
        refresh_fails: AtomicBool,
    }

    #[async_trait]
    impl TokenSource for Tokens {
        async fn token(&self) -> Result<String> {
            Ok(self.current.lock().unwrap().clone())
        }

        async fn refresh(&self) -> Result<()> {
            self.refreshes.fetch_add(1, Ordering::SeqCst);
            if self.refresh_fails.load(Ordering::SeqCst) {
                bail!("the refresh token has expired");
            }
            *self.current.lock().unwrap() = "refreshed-token".to_string();
            Ok(())
        }
    }

    /// One `tools/call` to a fake server that answers with `canned`, then with `ok`.
    struct Run {
        fake: Arc<Fake>,
        recorder: Arc<Recorder>,
        tokens: Arc<Tokens>,
        outcome: Result<ToolResult>,
    }

    impl Run {
        fn error(&self) -> &WorkIqError {
            self.outcome
                .as_ref()
                .unwrap_err()
                .downcast_ref::<WorkIqError>()
                .unwrap()
        }

        fn sent(&self) -> usize {
            self.fake.count("tools/call")
        }

        fn slept(&self) -> Vec<Duration> {
            self.recorder.0.lock().unwrap().clone()
        }

        fn refreshes(&self) -> usize {
            self.tokens.refreshes.load(Ordering::SeqCst)
        }

        fn call_bearers(&self) -> Vec<String> {
            let seen = self.fake.seen.lock().unwrap();
            seen.iter()
                .filter(|s| s.method == "tools/call")
                .filter_map(|s| header_of(&s.headers, "authorization"))
                .collect()
        }
    }

    /// The request timeout of [`run_with`]'s client, and a reply delay beyond it.
    const TEST_TIMEOUT: Duration = Duration::from_secs(5);
    const TIMED_OUT_MS: u64 = 7_000;

    async fn run(tool: &str, canned: Vec<Canned>) -> Run {
        run_with(tool, canned, false).await
    }

    async fn run_with(tool: &str, canned: Vec<Canned>, refresh_fails: bool) -> Run {
        let (fake, url) = start().await;
        fake.canned.lock().unwrap().extend(canned);
        let recorder = Arc::new(Recorder::default());
        let tokens = Arc::new(Tokens {
            current: std::sync::Mutex::new("test-token".to_string()),
            refreshes: AtomicUsize::new(0),
            refresh_fails: AtomicBool::new(refresh_fails),
        });
        // Long enough that a request to the local fake server never times out under a loaded
        // test run; the timeout tests delay their reply beyond it.
        let transport = HttpTransport::with_timeout(&url, TEST_TIMEOUT).unwrap();
        let client = Client::with_transport(Arc::new(transport), tokens.clone())
            .with_sleeper(recorder.clone());
        let outcome = client.call_tool(tool, json!({})).await;
        Run {
            fake,
            recorder,
            tokens,
            outcome,
        }
    }

    const CHANGE_TOOLS: [&str; 6] = [
        "create_entity",
        "update_entity",
        "delete_entity",
        "do_action",
        "call_function",
        "a_tool_added_later",
    ];

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[tokio::test]
    async fn a_401_refreshes_the_token_once_and_sends_the_call_again() {
        let run = run("retrieve", vec![Canned::http(401, "expired"), Canned::ok()]).await;

        assert!(run.outcome.is_ok());
        assert_eq!(run.refreshes(), 1);
        assert_eq!(run.sent(), 2);
        assert_eq!(
            run.call_bearers(),
            ["Bearer test-token", "Bearer refreshed-token"]
        );
        assert!(run.slept().is_empty());
    }

    #[tokio::test]
    async fn a_change_rejected_with_401_is_sent_again_after_the_refresh() {
        let run = run("create_entity", vec![Canned::http(401, ""), Canned::ok()]).await;

        assert!(run.outcome.is_ok());
        assert_eq!((run.refreshes(), run.sent()), (1, 2));
    }

    #[tokio::test]
    async fn a_401_after_the_refresh_is_sign_in_again_and_not_refreshed_twice() {
        let challenge = r#"Bearer error="invalid_token", error_description="expired""#;
        let rejected = || Canned::http(401, "").header("www-authenticate", challenge);
        let run = run("retrieve", vec![rejected(), rejected(), Canned::ok()]).await;

        assert_eq!((run.refreshes(), run.sent()), (1, 2));
        let error = run.error();
        assert_eq!(error.class, Class::SignInAgain);
        assert_eq!(error.fix.who, Who::User);
        let text = error.to_string();
        assert!(text.contains("proofkin auth login workiq"), "{text}");
    }

    #[tokio::test]
    async fn a_failed_refresh_is_reported_with_its_reason() {
        let run = run_with("retrieve", vec![Canned::http(401, ""), Canned::ok()], true).await;

        assert_eq!((run.refreshes(), run.sent()), (1, 1));
        assert_eq!(run.error().class, Class::SignInAgain);
        let text = run.error().to_string();
        assert!(text.contains("the refresh token has expired"), "{text}");
    }

    #[tokio::test]
    async fn a_claims_challenge_is_sign_in_again_and_sent_once() {
        let challenge = r#"Bearer error="insufficient_claims", claims="eyJhY2Nlc3NfdG9rZW4i""#;
        let canned = Canned::http(403, "").header("www-authenticate", challenge);
        let run = run("retrieve", vec![canned, Canned::ok()]).await;

        assert_eq!(run.error().class, Class::SignInAgain);
        assert_eq!((run.refreshes(), run.sent()), (0, 1));
        assert!(run.slept().is_empty());
    }

    #[tokio::test]
    async fn a_policy_denial_is_sent_exactly_once_and_names_the_admin() {
        let canned = vec![Canned::http(403, r#"{"error":"forbidden"}"#), Canned::ok()];
        let run = run("retrieve", canned).await;

        assert_eq!(run.sent(), 1);
        assert!(run.slept().is_empty());
        let error = run.error();
        assert_eq!(error.class, Class::DeniedByPolicy);
        assert_eq!(error.fix.who, Who::Admin);
        assert_eq!(error.kind, WorkIqErrorKind::Http(403));
    }

    #[tokio::test]
    async fn aadsts_and_consent_codes_in_error_text_are_classified_and_never_retried() {
        let cases = [
            (
                400,
                "AADSTS65001: the user has not consented",
                Class::Consent,
                "consent",
            ),
            (
                401,
                "AADSTS65001: the user has not consented",
                Class::Consent,
                "consent",
            ),
            (400, "error: consent_required", Class::Consent, "consent"),
            (
                400,
                "AADSTS50105: user is not assigned",
                Class::DeniedByPolicy,
                "assign the user",
            ),
            (
                400,
                "AADSTS53003: blocked",
                Class::DeniedByPolicy,
                "Conditional Access",
            ),
        ];
        for (status, text, class, fix) in cases {
            let run = run("retrieve", vec![Canned::http(status, text), Canned::ok()]).await;
            assert_eq!(run.sent(), 1, "{text}");
            assert_eq!(run.refreshes(), 0, "{text}");
            assert!(run.slept().is_empty(), "{text}");
            let error = run.error();
            assert_eq!(error.class, class, "{text}");
            assert_eq!(error.fix.who, Who::Admin, "{text}");
            assert!(
                error.fix.action.contains(fix),
                "{text}: {}",
                error.fix.action
            );
        }
    }

    #[tokio::test]
    async fn spending_policy_and_billing_errors_are_billing_and_never_retried() {
        let cases = [
            Canned::failed("The spending policy for this user has no budget left"),
            Canned::http(402, "payment required"),
            Canned::http(400, "Billing is not set up for this tenant"),
        ];
        for canned in cases {
            let run = run("retrieve", vec![canned, Canned::ok()]).await;
            assert_eq!(run.sent(), 1);
            assert!(run.slept().is_empty());
            let error = run.error();
            assert_eq!(error.class, Class::Billing);
            assert_eq!(error.fix.who, Who::Admin);
            assert!(error.fix.action.contains("spending policy"));
        }
    }

    #[tokio::test]
    async fn a_429_with_a_two_second_delay_is_sent_again_after_it() {
        let throttled = Canned::http(429, "slow down").header("retry-after", "2");
        let run = run("retrieve", vec![throttled, Canned::ok()]).await;

        assert!(run.outcome.is_ok());
        assert_eq!(run.sent(), 2);
        assert_eq!(run.slept(), [secs(2)]);
    }

    #[tokio::test]
    async fn retry_after_seconds_in_the_payload_is_waited_for_in_both_places() {
        let fields = json!({ "stoppedReason": "error", "errorCode": "Throttled",
            "retryAfterSeconds": 3 });
        let shapes = [
            json!({ "application/vnd.ms-workiq.retrieval": fields.clone() }),
            fields,
        ];
        for structured in shapes {
            let throttled = Canned::result(json!({ "content": [], "isError": false,
                "structuredContent": structured }));
            let run = run("retrieve", vec![throttled, Canned::ok()]).await;

            assert!(run.outcome.is_ok(), "{structured}");
            assert_eq!(run.sent(), 2, "{structured}");
            assert_eq!(run.slept(), [secs(3)], "{structured}");
        }
    }

    #[tokio::test]
    async fn throttling_is_waited_for_at_most_twice_then_reported() {
        let throttled = || Canned::http(429, "").header("retry-after", "1");
        let run = run("retrieve", vec![throttled(), throttled(), throttled()]).await;

        assert_eq!(run.sent(), 3);
        assert_eq!(run.slept(), [secs(1), secs(1)]);
        assert_eq!(run.error().class, Class::Throttled(Some(secs(1))));
    }

    #[tokio::test]
    async fn a_delay_over_60_seconds_or_none_at_all_is_reported_without_waiting() {
        let too_long = Canned::http(429, "").header("retry-after", "61");
        let run_long = run("retrieve", vec![too_long, Canned::ok()]).await;
        assert_eq!(run_long.error().class, Class::Throttled(Some(secs(61))));

        let no_hint = run("retrieve", vec![Canned::http(429, ""), Canned::ok()]).await;
        assert_eq!(no_hint.error().class, Class::Throttled(None));

        let at_the_limit = Canned::http(429, "").header("retry-after", "60");
        let run_limit = run("retrieve", vec![at_the_limit, Canned::ok()]).await;
        assert!(run_limit.outcome.is_ok());
        assert_eq!(run_limit.slept(), [secs(60)]);

        for run in [&run_long, &no_hint] {
            assert_eq!(run.sent(), 1);
            assert!(run.slept().is_empty());
        }
    }

    #[tokio::test]
    async fn a_transient_failure_of_a_read_is_retried_with_backoff() {
        let run = run("retrieve", vec![Canned::http(503, "busy"), Canned::ok()]).await;

        assert!(run.outcome.is_ok());
        assert_eq!(run.sent(), 2);
        let slept = run.slept();
        assert_eq!(slept.len(), 1);
        assert!(slept[0] >= Duration::from_millis(250) && slept[0] <= Duration::from_millis(500));
    }

    #[tokio::test]
    async fn a_read_is_retried_at_most_twice_with_growing_backoff() {
        let busy = || Canned::http(503, "busy");
        let run = run("retrieve", vec![busy(), busy(), busy(), Canned::ok()]).await;

        assert_eq!(run.sent(), 3);
        assert_eq!(run.error().class, Class::Transient);
        assert_eq!(run.error().kind, WorkIqErrorKind::Http(503));
        let slept = run.slept();
        assert_eq!(slept.len(), 2);
        let range = |low, high| Duration::from_millis(low)..=Duration::from_millis(high);
        assert!(range(250, 500).contains(&slept[0]), "{slept:?}");
        assert!(range(500, 1000).contains(&slept[1]), "{slept:?}");
    }

    #[tokio::test]
    async fn a_read_that_timed_out_is_retried() {
        let run = run(
            "retrieve",
            vec![Canned::ok().delayed(TIMED_OUT_MS), Canned::ok()],
        )
        .await;

        assert!(run.outcome.is_ok());
        assert_eq!(run.sent(), 2);
        assert_eq!(run.slept().len(), 1);
    }

    #[tokio::test]
    async fn every_read_tool_is_retried_after_a_transient_failure() {
        for tool in READ_TOOLS {
            let run = run(tool, vec![Canned::http(502, ""), Canned::ok()]).await;
            assert!(run.outcome.is_ok(), "{tool}");
            assert_eq!(run.sent(), 2, "{tool}");
        }
    }

    #[tokio::test]
    async fn a_change_that_timed_out_is_sent_exactly_once_and_reported_uncertain() {
        // At once, so the test waits for one timeout rather than one per tool.
        let runs = futures_util::future::join_all(
            CHANGE_TOOLS
                .map(|tool| run(tool, vec![Canned::ok().delayed(TIMED_OUT_MS), Canned::ok()])),
        )
        .await;
        for (tool, run) in CHANGE_TOOLS.into_iter().zip(runs) {
            assert_eq!(run.sent(), 1, "{tool}");
            assert!(run.slept().is_empty(), "{tool}");
            let error = run.error();
            assert_eq!(error.class, Class::UncertainChange, "{tool}");
            let text = error.to_string();
            assert!(text.contains("may or may not have been made"), "{text}");
            assert!(text.contains("client-request-id"), "{text}");
        }
    }

    #[tokio::test]
    async fn change_tools_are_never_sent_again_after_a_5xx_or_a_throttle() {
        for tool in CHANGE_TOOLS {
            let run = run(tool, vec![Canned::http(503, ""), Canned::ok()]).await;
            assert_eq!(
                (run.sent(), run.error().class.clone()),
                (1, Class::Transient)
            );
            assert!(run.slept().is_empty(), "{tool}");

            let throttled = Canned::http(429, "").header("retry-after", "1");
            let run = run_with(tool, vec![throttled, Canned::ok()], false).await;
            assert_eq!(run.sent(), 1, "{tool}");
            assert_eq!(run.error().class, Class::Throttled(Some(secs(1))), "{tool}");
            assert!(run.slept().is_empty(), "{tool}");
        }
    }

    #[tokio::test]
    async fn a_tool_that_reports_an_error_is_a_tool_error_sent_once() {
        let run = run(
            "retrieve",
            vec![Canned::failed("The search\nbroke"), Canned::ok()],
        )
        .await;

        assert_eq!(run.sent(), 1);
        let error = run.error();
        assert_eq!(error.class, Class::Tool);
        assert_eq!(error.kind, WorkIqErrorKind::Tool);
        assert!(
            error.message.contains("The search broke"),
            "{}",
            error.message
        );
    }

    #[tokio::test]
    async fn stopped_reason_error_with_a_policy_code_is_a_denial_sent_once() {
        let blocked = json!({ "content": [], "isError": false, "structuredContent": {
            "application/vnd.ms-workiq.retrieval": { "stoppedReason": "error",
                "errorCode": "PolicyBlocked",
                "recoveryAction": "Ask your tenant admin to allow this action" } } });
        let run = run("retrieve", vec![Canned::result(blocked), Canned::ok()]).await;

        assert_eq!(run.sent(), 1);
        let error = run.error();
        assert_eq!(error.class, Class::DeniedByPolicy);
        assert_eq!(error.fix.who, Who::Admin);
        assert!(error.message.contains("PolicyBlocked"), "{}", error.message);
        assert!(
            error.message.contains("Ask your tenant admin"),
            "{}",
            error.message
        );
    }

    #[tokio::test]
    async fn a_stopped_reason_that_is_not_error_is_not_a_failure() {
        let fine = json!({ "content": [], "isError": false,
            "structuredContent": { "stoppedReason": "completed" } });
        let run = run("retrieve", vec![Canned::result(fine)]).await;

        assert!(run.outcome.is_ok());
    }

    #[tokio::test]
    async fn json_rpc_errors_are_classified_from_their_text_and_sent_once() {
        let consent = Canned::rpc_error(-32001, "AADSTS65001: consent is needed");
        let run_consent = run("retrieve", vec![consent, Canned::ok()]).await;
        assert_eq!(run_consent.error().class, Class::Consent);
        assert_eq!(run_consent.error().kind, WorkIqErrorKind::Rpc(-32001));

        let other = run(
            "retrieve",
            vec![Canned::rpc_error(-32603, "boom"), Canned::ok()],
        )
        .await;
        assert_eq!(other.error().class, Class::Tool);

        assert_eq!((run_consent.sent(), other.sent()), (1, 1));
    }

    #[tokio::test]
    async fn an_error_is_one_line_with_what_happened_the_fix_and_the_ids() {
        let run = run("retrieve", vec![Canned::http(403, "no\r\naccess\n")]).await;

        let text = run.error().to_string();
        assert!(!text.contains('\n') && !text.contains('\r'), "{text:?}");
        assert!(text.starts_with("Work IQ: the server answered 403 Forbidden: no access."));
        assert!(text.contains("Fix (admin): "), "{text}");
        assert!(text.contains("request-id req-"), "{text}");
        assert!(text.contains("x-ms-correlation-id corr-"), "{text}");
        assert!(text.contains("client-request-id "), "{text}");
        let ids = &run.error().ids;
        assert!(ids.request_id.is_some() && ids.correlation_id.is_some());
    }

    use crate::profile::{WorkIqBudget, WorkIqNotices};
    use crate::workiq::audit::{self, Audit, CallContext, Clock, Purpose};
    use crate::workiq::notice::{Notifier, SignOff};
    use chrono::{DateTime, Local, TimeZone};
    use sha2::{Digest, Sha256};

    const SECRET_QUERY: &str = "salary review for project nightingale";

    struct TestClock(std::sync::Mutex<DateTime<Local>>);

    impl TestClock {
        fn at_noon(day: u32) -> Arc<Self> {
            let noon = Local
                .with_ymd_and_hms(2026, 11, day, 12, 0, 0)
                .single()
                .unwrap();
            Arc::new(TestClock(std::sync::Mutex::new(noon)))
        }

        fn next_day(&self) {
            let mut now = self.0.lock().unwrap();
            *now += chrono::Duration::days(1);
        }
    }

    impl Clock for TestClock {
        fn now(&self) -> DateTime<Local> {
            *self.0.lock().unwrap()
        }
    }

    fn budget(per_day: u32, per_run: u32) -> WorkIqBudget {
        WorkIqBudget {
            calls_per_day: Some(per_day),
            calls_per_run: Some(per_run),
        }
    }

    /// A client whose calls are audited in `dir`.
    fn audited(url: &str, dir: &std::path::Path, budget: WorkIqBudget) -> (Client, Arc<Audit>) {
        let audit = Arc::new(Audit::new(dir.join(audit::FILE), "alice", budget));
        (client(url).with_audit(audit.clone()), audit)
    }

    fn step(run: &str) -> CallContext {
        CallContext::local(Purpose::Step {
            run: run.to_string(),
            step: "research".to_string(),
        })
    }

    fn retrieval(results: Value) -> Canned {
        Canned::result(json!({
            "content": [],
            "structuredContent": { "application/vnd.ms-workiq.retrieval": { "results": results } },
            "isError": false,
        }))
    }

    async fn query(client: &Client, context: &CallContext) -> Result<ToolResult> {
        let arguments = json!({ "query": SECRET_QUERY });
        client.call_tool_in(context, "retrieve", arguments).await
    }

    #[tokio::test]
    async fn every_call_is_one_audit_line_and_none_holds_the_query_text() {
        let (fake, url) = start().await;
        let dir = tempfile::tempdir().unwrap();
        let (client, audit) = audited(&url, dir.path(), WorkIqBudget::default());
        fake.canned.lock().unwrap().extend([
            retrieval(json!([
                { "contentProtection": "allowed", "sensitivityLabel": "General" },
                { "contentProtection": "allowed" },
                { "contentProtection": "blocked", "sensitivityLabel": "Secret" },
            ])),
            Canned::failed("the tool broke"),
            Canned::http(403, r#"{"error":"forbidden"}"#),
        ]);
        let peer = CallContext::peer("bob", vec!["General".to_string()], Purpose::Gateway);

        assert!(query(&client, &step("run-1")).await.is_ok());
        assert!(query(&client, &CallContext::command()).await.is_err());
        assert!(query(&client, &peer).await.is_err());

        let text = std::fs::read_to_string(audit.path()).unwrap();
        assert_eq!(text.lines().count(), 3);
        assert!(
            !text.contains("nightingale") && !text.contains("salary"),
            "{text}"
        );
        let entries = audit::read(audit.path()).unwrap();
        let outcomes: Vec<&str> = entries.iter().map(|e| e.outcome.as_str()).collect();
        assert_eq!(outcomes, ["ok", "tool", "denied-by-policy"]);

        let sha = data_encoding::HEXLOWER.encode(&Sha256::digest(SECRET_QUERY.as_bytes()));
        let first = &entries[0];
        assert_eq!(
            (first.tool.as_str(), first.purpose.as_str()),
            ("retrieve", "step")
        );
        assert_eq!(first.run.as_deref(), Some("run-1"));
        assert_eq!(first.step.as_deref(), Some("research"));
        assert_eq!(first.audience, "local");
        assert_eq!(first.query_sha256, sha);
        assert_eq!(first.query_chars, SECRET_QUERY.chars().count());
        assert_eq!((first.results, first.withheld), (Some(3), Some(1)));
        assert_eq!(first.labels, ["General", "Secret", "unknown"]);
        assert!(first.request_id.as_deref().unwrap().starts_with("req-"));
        assert!(
            first
                .correlation_id
                .as_deref()
                .unwrap()
                .starts_with("corr-")
        );
        assert!(first.client_request_id.is_some() && first.ts_ms > 0);

        assert_eq!(entries[1].purpose, "command");
        assert_eq!(entries[1].results, None);
        assert_eq!(entries[2].audience, "bob");
        assert!(entries[2].request_id.is_some());
    }

    #[tokio::test]
    async fn answered_statuses_set_audit_outcomes_and_still_count_against_budgets() {
        let (fake, url) = start().await;
        let dir = tempfile::tempdir().unwrap();
        let (client, audit) = audited(&url, dir.path(), WorkIqBudget::default());
        let client = Arc::new(client);
        fake.canned.lock().unwrap().extend([
            Canned::result(json!({ "structuredContent": { "statusCode": 403 } })),
            Canned::result(json!({ "structuredContent": { "statusCode": 201 } })),
            Canned::result(json!({
                "content": [{ "type": "text", "text": r#"{"httpStatusCode":"503"}"# }],
            })),
        ]);

        let notices = Notifier::new(client.clone(), WorkIqNotices::Mail, "alice@example.com");
        let error = notices
            .send(&SignOff {
                agent: "alice".into(),
                title: "Sign off the draft".into(),
                run: Some("run-1".into()),
                step: Some("review".into()),
                ..SignOff::default()
            })
            .await
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<WorkIqError>().unwrap().class,
            Class::DeniedByPolicy
        );
        let context = step("run-1");
        client
            .call_tool_in(
                &context,
                "create_entity",
                json!({ "parentUrl": "/me/events", "jsonBody": { "subject": "Sync" } }),
            )
            .await
            .unwrap();
        client
            .call_tool_in(&context, "get_schema", json!({}))
            .await
            .unwrap();

        let entries = audit::read(audit.path()).unwrap();
        let outcomes: Vec<&str> = entries.iter().map(|e| e.outcome.as_str()).collect();
        assert_eq!(outcomes, ["denied-by-policy", "ok", "transient"]);
        assert_eq!(entries[0].purpose, "notice");
        assert!(entries.iter().all(|e| e.request_id.is_some()));
        assert_eq!(fake.count("tools/call"), 3);
        let usage = audit.usage(Some("run-1"));
        assert_eq!(usage.today, 3);
        assert_eq!(usage.run, Some(3));
    }

    #[tokio::test]
    async fn the_daily_budget_refuses_the_next_call_before_it_is_sent() {
        let (fake, url) = start().await;
        let dir = tempfile::tempdir().unwrap();
        let (client, audit) = audited(&url, dir.path(), budget(2, 25));
        let context = CallContext::command();

        assert!(query(&client, &context).await.is_ok());
        assert!(query(&client, &context).await.is_ok());
        let refused = query(&client, &context).await.unwrap_err();

        let failure = refused.downcast_ref::<WorkIqError>().unwrap();
        assert_eq!(failure.class, Class::OverBudget);
        assert!(failure.message.contains("daily"), "{failure}");
        assert!(failure.message.contains("2 calls"), "{failure}");
        assert!(
            failure.message.contains("proofkin workiq budget --per-day"),
            "{failure}"
        );
        assert!(failure.message.contains("--profile alice"), "{failure}");
        assert_eq!(fake.count("tools/call"), 2);
        let entries = audit::read(audit.path()).unwrap();
        let outcomes: Vec<&str> = entries.iter().map(|e| e.outcome.as_str()).collect();
        assert_eq!(outcomes, ["ok", "ok", "refused"]);
        assert_eq!(audit.usage(None).today, 2);
    }

    #[tokio::test]
    async fn the_per_run_budget_refuses_the_next_call_of_that_run_only() {
        let (fake, url) = start().await;
        let dir = tempfile::tempdir().unwrap();
        let (client, audit) = audited(&url, dir.path(), budget(100, 2));

        assert!(query(&client, &step("run-1")).await.is_ok());
        assert!(query(&client, &step("run-1")).await.is_ok());
        let refused = query(&client, &step("run-1")).await.unwrap_err();

        let failure = refused.downcast_ref::<WorkIqError>().unwrap();
        assert_eq!(failure.class, Class::OverBudget);
        assert!(failure.message.contains("per run"), "{failure}");
        assert!(failure.message.contains("run run-1"), "{failure}");
        assert!(
            failure.message.contains("proofkin workiq budget --per-run"),
            "{failure}"
        );
        assert_eq!(fake.count("tools/call"), 2);

        assert!(query(&client, &step("run-2")).await.is_ok());
        assert!(query(&client, &CallContext::command()).await.is_ok());
        let outcomes: Vec<String> = audit::read(audit.path())
            .unwrap()
            .into_iter()
            .map(|e| e.outcome)
            .collect();
        assert_eq!(outcomes, ["ok", "ok", "refused", "ok", "ok"]);
        assert_eq!(audit.usage(Some("run-1")).run, Some(2));
    }

    #[tokio::test]
    async fn a_new_day_resets_the_daily_count() {
        let (_fake, url) = start().await;
        let dir = tempfile::tempdir().unwrap();
        let clock = TestClock::at_noon(20);
        let audit = Arc::new(
            Audit::new(dir.path().join(audit::FILE), "alice", budget(1, 25))
                .with_clock(clock.clone()),
        );
        let client = client(&url).with_audit(audit.clone());
        let context = CallContext::command();

        assert!(query(&client, &context).await.is_ok());
        assert!(query(&client, &context).await.is_err());
        assert_eq!(audit.usage(None).today, 1);

        clock.next_day();
        assert_eq!(audit.usage(None).today, 0);
        assert!(query(&client, &context).await.is_ok());
        assert!(query(&client, &context).await.is_err());
        assert_eq!(audit.usage(None).today, 1);
        assert_eq!(audit.report().unwrap().today, 1);
    }
}
