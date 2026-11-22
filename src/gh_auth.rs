//! GitHub CLI sign-ins that Proofkin runs for its user, so the web UI (and `proofkin
//! github grant` and `proofkin github login --new`) can give the profile's GitHub account a
//! missing scope, or sign in another account, without a terminal.
//!
//! Both use gh's device flow: gh prints a one-time code, the user enters it on GitHub, and
//! gh keeps the new token. `gh auth refresh` only works on gh's active account, so a grant
//! switches gh to the profile's account first, and a new sign-in makes the new account the
//! active one. Either way gh is switched back to the account that was active before,
//! whatever happens: when the flow ends, fails, is cancelled or times out, and (blocking)
//! when the agent stops while it waits. One flow runs at a time.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use regex::Regex;
use serde::Serialize;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::sync::{Notify, mpsc, oneshot};

use crate::profile::INHERITED_CREDENTIAL_VARS;

/// How long gh has to print its one-time code.
const CODE_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the user has to approve on GitHub (gh's device codes last 15 minutes).
const FLOW_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// gh's last output lines kept to say why a flow failed.
const TAIL_LINES: usize = 8;
const MAX_REASON_CHARS: usize = 400;

/// A GitHub CLI account on this machine (`gh auth status --json hosts`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GhAccount {
    pub login: String,
    pub host: String,
    pub active: bool,
    /// The token's OAuth scopes, e.g. `repo`, `workflow` and `project`.
    pub scopes: Vec<String>,
}

impl GhAccount {
    pub fn has_scope(&self, scope: &str) -> bool {
        self.scopes.iter().any(|s| s == scope)
    }
}

/// The accounts in `gh auth status --json hosts` output that are signed in fine.
pub fn parse_accounts(json: &Value) -> Vec<GhAccount> {
    let mut accounts = Vec::new();
    let Some(hosts) = json.get("hosts").and_then(Value::as_object) else {
        return accounts;
    };
    for (host, list) in hosts {
        for a in list.as_array().into_iter().flatten() {
            if a.get("state").and_then(Value::as_str) != Some("success") {
                continue;
            }
            let Some(login) = a.get("login").and_then(Value::as_str) else {
                continue;
            };
            // gh gives the scopes as one comma-separated string.
            let scopes = match a.get("scopes") {
                Some(Value::String(s)) => s
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect(),
                Some(Value::Array(list)) => list
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect(),
                _ => Vec::new(),
            };
            accounts.push(GhAccount {
                login: login.to_owned(),
                host: host.clone(),
                active: a.get("active").and_then(Value::as_bool).unwrap_or(false),
                scopes,
            });
        }
    }
    accounts
}

/// Fails unless every scope looks like an OAuth scope (`project`, `read:org`).
pub fn check_scopes(scopes: &[String]) -> Result<()> {
    if scopes.len() > 10 {
        bail!("ask for at most 10 scopes at a time");
    }
    for scope in scopes {
        let ok = !scope.is_empty()
            && scope.len() <= 40
            && scope
                .chars()
                .all(|c| c.is_ascii_lowercase() || c == ':' || c == '_');
        if !ok {
            bail!("{scope:?} isn't a GitHub scope (e.g. project, workflow, read:org)");
        }
    }
    Ok(())
}

/// A gh command started by [`Gh::start`].
pub struct Started {
    /// Its output, stdout and stderr, line by line.
    pub lines: mpsc::UnboundedReceiver<String>,
    /// Whether it exited with success, sent once it has ended (also after a kill).
    pub ended: oneshot::Receiver<bool>,
    /// Stops it; dropping it stops it too.
    pub kill: oneshot::Sender<()>,
}

/// What a device flow needs of the GitHub CLI.
#[async_trait]
pub trait Gh: Send + Sync {
    async fn accounts(&self) -> Result<Vec<GhAccount>>;
    /// Makes `login` gh's active account on `host`.
    async fn switch(&self, host: &str, login: &str) -> Result<()>;
    /// Starts `gh <args>` with its stdin closed.
    async fn start(&self, args: &[String]) -> Result<Started>;
    /// [`Gh::switch`] without waiting on the async runtime, for when the agent stops
    /// while a flow waits.
    fn switch_now(&self, host: &str, login: &str);
}

/// The GitHub CLI itself (`PROOFKIN_GH` overrides which program, see
/// [`crate::github::gh_program`]). It never gets the agent's tokens: gh refuses to refresh
/// while one is set, and must store what it signs in.
pub struct GhCli {
    program: PathBuf,
    /// Arguments before gh's own (tests run a script through its interpreter).
    prefix: Vec<String>,
}

impl Default for GhCli {
    fn default() -> Self {
        Self::new()
    }
}

impl GhCli {
    pub fn new() -> Self {
        Self::with_program(crate::github::gh_program(), Vec::new())
    }

    pub fn with_program(program: impl Into<PathBuf>, prefix: Vec<String>) -> Self {
        Self {
            program: program.into(),
            prefix,
        }
    }

    fn command(&self, args: &[&str]) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new(&self.program);
        cmd.args(&self.prefix)
            .args(args)
            .stdin(Stdio::null())
            .kill_on_drop(true);
        for var in INHERITED_CREDENTIAL_VARS {
            cmd.env_remove(var);
        }
        cmd
    }
}

fn read_lines(stream: impl AsyncRead + Unpin + Send + 'static, tx: mpsc::UnboundedSender<String>) {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stream).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
}

#[async_trait]
impl Gh for GhCli {
    async fn accounts(&self) -> Result<Vec<GhAccount>> {
        let out = self
            .command(&["auth", "status", "--json", "hosts"])
            .output()
            .await
            .context("running the GitHub CLI (gh)")?;
        // gh exits with an error when one account has a problem; its JSON still lists all.
        let json: Value = serde_json::from_slice(&out.stdout).with_context(|| {
            format!(
                "couldn't read the GitHub CLI's accounts: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )
        })?;
        Ok(parse_accounts(&json))
    }

    async fn switch(&self, host: &str, login: &str) -> Result<()> {
        let out = self
            .command(&["auth", "switch", "--hostname", host, "--user", login])
            .output()
            .await
            .context("running the GitHub CLI (gh)")?;
        if !out.status.success() {
            bail!(
                "gh couldn't make {login} its active account on {host}: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }

    async fn start(&self, args: &[String]) -> Result<Started> {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let mut child = self
            .command(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("starting the GitHub CLI (gh)")?;
        let (tx, lines) = mpsc::unbounded_channel();
        if let Some(out) = child.stdout.take() {
            read_lines(out, tx.clone());
        }
        if let Some(err) = child.stderr.take() {
            read_lines(err, tx);
        }
        let (ended_tx, ended) = oneshot::channel();
        let (kill, kill_rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            let ok = tokio::select! {
                status = child.wait() => status.map(|s| s.success()).unwrap_or(false),
                // Sent, or dropped: either way gh is stopped.
                _ = kill_rx => {
                    let _ = child.kill().await;
                    false
                }
            };
            let _ = ended_tx.send(ok);
        });
        Ok(Started { lines, ended, kill })
    }

    fn switch_now(&self, host: &str, login: &str) {
        let mut cmd = std::process::Command::new(&self.program);
        cmd.args(&self.prefix)
            .args(["auth", "switch", "--hostname", host, "--user", login])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        for var in INHERITED_CREDENTIAL_VARS {
            cmd.env_remove(var);
        }
        let _ = cmd.status();
    }
}

static ANSI: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\x1b\[[0-9;?]*[A-Za-z]").expect("valid regex"));
static CODE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b([A-Z0-9]{4}-[A-Z0-9]{4})\b").expect("valid regex"));
static DEVICE_URL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"https?://[^\s]+/login/device[^\s]*").expect("valid regex"));

/// The one-time code in a line of gh's output (`! First copy your one-time code: 1A2B-3C4D`).
fn device_code(line: &str) -> Option<String> {
    CODE.captures(line).map(|c| c[1].to_owned())
}

/// Where gh says to enter the code.
fn device_url(line: &str) -> Option<String> {
    DEVICE_URL
        .find(line)
        .map(|m| m.as_str().trim_end_matches(['.', ',', ')']).to_owned())
}

/// What a flow does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// Gives account `login` more scopes (`gh auth refresh`).
    Grant { login: String, scopes: Vec<String> },
    /// Signs in another account (`gh auth login --web`), asking for `scopes` too.
    SignIn { scopes: Vec<String> },
}

/// Where a flow is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum State {
    /// Waiting for the user to enter the code on GitHub.
    Waiting,
    /// Done: the account with the new scopes, or the account signed in.
    Done {
        login: String,
    },
    Failed {
        reason: String,
    },
    Cancelled,
}

/// A device flow, as the web UI shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Flow {
    /// `grant` or `sign_in`.
    pub kind: &'static str,
    pub host: String,
    /// The account given the scopes; for a sign-in, the new account once it is done.
    pub login: Option<String>,
    pub scopes: Vec<String>,
    /// The one-time code to enter on GitHub.
    pub code: String,
    /// Where to enter it.
    pub url: String,
    /// gh's active account on `host` before the flow, made active again after it.
    pub active_before: Option<String>,
    #[serde(flatten)]
    pub state: State,
    /// Something to know once it has ended, e.g. that gh couldn't be switched back.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Makes `to` gh's active account on `host` again once a flow has changed it: on
/// [`SwitchBack::restore`], or when dropped (if the flow's task is stopped).
struct SwitchBack {
    gh: Arc<dyn Gh>,
    host: String,
    to: Option<String>,
    armed: bool,
}

impl SwitchBack {
    async fn restore(&mut self) -> Result<()> {
        if !std::mem::take(&mut self.armed) {
            return Ok(());
        }
        let Some(to) = &self.to else {
            return Ok(());
        };
        let accounts = self.gh.accounts().await?;
        let active = accounts
            .iter()
            .find(|a| a.host == self.host && a.active)
            .map(|a| a.login.as_str());
        if active.is_some_and(|a| a.eq_ignore_ascii_case(to)) {
            return Ok(());
        }
        self.gh.switch(&self.host, to).await.with_context(|| {
            format!(
                "switch it back yourself: gh auth switch --hostname {} --user {to}",
                self.host
            )
        })
    }
}

impl Drop for SwitchBack {
    fn drop(&mut self) {
        if self.armed
            && let Some(to) = &self.to
        {
            self.gh.switch_now(&self.host, to);
        }
    }
}

struct Current {
    flow: Flow,
    cancel: Option<oneshot::Sender<()>>,
}

/// Runs device flows one at a time, and keeps the latest for the web UI to show.
pub struct Flows {
    gh: Arc<dyn Gh>,
    current: Mutex<Option<Current>>,
    starting: AtomicBool,
    changed: Notify,
    code_timeout: Duration,
    flow_timeout: Duration,
}

/// How a flow's gh command ended.
enum Outcome {
    Exited(bool),
    Cancelled,
    TimedOut,
}

fn same(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

fn reason(tail: &VecDeque<String>, fallback: &str) -> String {
    let text = tail
        .iter()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let text = if text.is_empty() {
        fallback.to_owned()
    } else {
        text
    };
    crate::redact::redact(&text)
        .0
        .chars()
        .take(MAX_REASON_CHARS)
        .collect()
}

fn keep(tail: &mut VecDeque<String>, line: String) {
    if tail.len() == TAIL_LINES {
        tail.pop_front();
    }
    tail.push_back(line);
}

impl Flows {
    pub fn new(gh: Arc<dyn Gh>) -> Arc<Self> {
        Self::with_timeouts(gh, CODE_TIMEOUT, FLOW_TIMEOUT)
    }

    pub fn with_timeouts(gh: Arc<dyn Gh>, code: Duration, flow: Duration) -> Arc<Self> {
        Arc::new(Self {
            gh,
            current: Mutex::new(None),
            starting: AtomicBool::new(false),
            changed: Notify::new(),
            code_timeout: code,
            flow_timeout: flow,
        })
    }

    /// The flow that waits, or the last one that ended.
    pub fn current(&self) -> Option<Flow> {
        self.current
            .lock()
            .unwrap()
            .as_ref()
            .map(|c| c.flow.clone())
    }

    /// Cancels the flow that waits; false if none does.
    pub fn cancel(&self) -> bool {
        let cancel = self
            .current
            .lock()
            .unwrap()
            .as_mut()
            .filter(|c| c.flow.state == State::Waiting)
            .and_then(|c| c.cancel.take());
        cancel.is_some_and(|c| c.send(()).is_ok())
    }

    /// The flow once it has ended (none if no flow was started).
    pub async fn ended(&self) -> Option<Flow> {
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            match self.current() {
                Some(f) if f.state == State::Waiting => {}
                other => return other,
            }
            notified.await;
        }
    }

    /// Starts `kind` on `host` and returns once gh shows its code; the flow then waits
    /// for the user in the background, and `on_end` gets it when it has ended.
    pub async fn start(
        self: &Arc<Self>,
        kind: Kind,
        host: &str,
        on_end: impl FnOnce(Flow) + Send + 'static,
    ) -> Result<Flow> {
        if let Some(c) = self
            .current
            .lock()
            .unwrap()
            .as_ref()
            .filter(|c| c.flow.state == State::Waiting)
        {
            bail!(
                "a GitHub sign-in is already waiting: enter {} at {}, or cancel it first",
                c.flow.code,
                c.flow.url
            );
        }
        if self.starting.swap(true, Ordering::SeqCst) {
            bail!("a GitHub sign-in is starting; wait for its code");
        }
        let started = self.begin(kind, host, on_end).await;
        self.starting.store(false, Ordering::SeqCst);
        started
    }

    async fn begin(
        self: &Arc<Self>,
        kind: Kind,
        host: &str,
        on_end: impl FnOnce(Flow) + Send + 'static,
    ) -> Result<Flow> {
        let accounts = self.gh.accounts().await?;
        let active_before = accounts
            .iter()
            .find(|a| a.host == host && a.active)
            .map(|a| a.login.clone());
        let mut back = SwitchBack {
            gh: self.gh.clone(),
            host: host.to_owned(),
            to: active_before.clone(),
            armed: false,
        };
        let (name, login, scopes, args): (&'static str, Option<String>, Vec<String>, Vec<String>) =
            match &kind {
                Kind::Grant { login, scopes } => {
                    let Some(account) = accounts
                        .iter()
                        .find(|a| a.host == host && same(&a.login, login))
                    else {
                        bail!("the GitHub CLI has no account {login} on {host}: sign it in first");
                    };
                    // gh only refreshes its active account.
                    if !account.active {
                        self.gh.switch(host, &account.login).await?;
                        back.armed = true;
                    }
                    let args: Vec<String> = ["auth", "refresh", "--hostname", host, "--scopes"]
                        .map(str::to_owned)
                        .into_iter()
                        .chain([scopes.join(",")])
                        .collect();
                    ("grant", Some(account.login.clone()), scopes.clone(), args)
                }
                Kind::SignIn { scopes } => {
                    // The new account becomes gh's active one.
                    back.armed = true;
                    let mut args: Vec<String> = [
                        "auth",
                        "login",
                        "--hostname",
                        host,
                        "--web",
                        "--git-protocol",
                        "https",
                        "--skip-ssh-key",
                    ]
                    .map(str::to_owned)
                    .into();
                    if !scopes.is_empty() {
                        args.extend(["--scopes".to_owned(), scopes.join(",")]);
                    }
                    ("sign_in", None, scopes.clone(), args)
                }
            };
        let mut started = match self.gh.start(&args).await {
            Ok(started) => started,
            Err(e) => {
                let _ = back.restore().await;
                return Err(e);
            }
        };
        // Read gh's output until it shows its code (and where to enter it).
        let mut tail = VecDeque::new();
        let (mut code, mut url) = (None, None);
        let deadline = tokio::time::sleep(self.code_timeout);
        tokio::pin!(deadline);
        while code.is_none() || url.is_none() {
            let line = tokio::select! {
                line = started.lines.recv() => line,
                _ = &mut deadline => None,
                // Once the code is there, the address follows at once, if gh prints one.
                _ = tokio::time::sleep(Duration::from_secs(2)), if code.is_some() => None,
            };
            let Some(line) = line else { break };
            let line = ANSI.replace_all(&line, "").into_owned();
            code = code.or_else(|| device_code(&line));
            url = url.or_else(|| device_url(&line));
            keep(&mut tail, line);
        }
        let Some(code) = code else {
            let _ = started.kill.send(());
            let _ = back.restore().await;
            bail!(
                "the GitHub CLI didn't show a sign-in code: {}",
                reason(&tail, "it said nothing")
            );
        };
        let flow = Flow {
            kind: name,
            host: host.to_owned(),
            login,
            scopes,
            code,
            url: url.unwrap_or_else(|| format!("https://{host}/login/device")),
            active_before,
            state: State::Waiting,
            note: None,
        };
        let (cancel, cancelled) = oneshot::channel();
        *self.current.lock().unwrap() = Some(Current {
            flow: flow.clone(),
            cancel: Some(cancel),
        });
        self.changed.notify_waiters();
        let this = self.clone();
        let flow_host = flow.host.clone();
        tokio::spawn(async move {
            let ended = this
                .finish(kind, started, tail, cancelled, back, flow_host)
                .await;
            on_end(ended);
        });
        Ok(flow)
    }

    async fn finish(
        &self,
        kind: Kind,
        started: Started,
        mut tail: VecDeque<String>,
        cancelled: oneshot::Receiver<()>,
        mut back: SwitchBack,
        host: String,
    ) -> Flow {
        let Started {
            mut lines,
            ended,
            kill,
        } = started;
        let mut kill = Some(kill);
        let timeout = tokio::time::sleep(self.flow_timeout);
        tokio::pin!(timeout, ended, cancelled);
        let outcome = loop {
            tokio::select! {
                Some(line) = lines.recv() => keep(&mut tail, ANSI.replace_all(&line, "").into_owned()),
                ok = &mut ended => break Outcome::Exited(ok.unwrap_or(false)),
                Ok(()) = &mut cancelled => {
                    if let Some(k) = kill.take() {
                        let _ = k.send(());
                    }
                    let _ = (&mut ended).await;
                    break Outcome::Cancelled;
                }
                _ = &mut timeout => {
                    if let Some(k) = kill.take() {
                        let _ = k.send(());
                    }
                    let _ = (&mut ended).await;
                    break Outcome::TimedOut;
                }
            }
        };
        // What gh printed last, up to when its output closes, says why it failed.
        while let Ok(Some(line)) = tokio::time::timeout(Duration::from_secs(1), lines.recv()).await
        {
            keep(&mut tail, ANSI.replace_all(&line, "").into_owned());
        }
        let mut login = None;
        let mut state = match outcome {
            Outcome::Cancelled => State::Cancelled,
            Outcome::TimedOut => State::Failed {
                reason: "nobody entered the code on GitHub in time".to_owned(),
            },
            Outcome::Exited(false) => State::Failed {
                reason: reason(&tail, "the GitHub CLI stopped without saying why"),
            },
            Outcome::Exited(true) => match self.gh.accounts().await {
                Err(e) => State::Failed {
                    reason: format!("{e:#}"),
                },
                Ok(accounts) => match &kind {
                    Kind::Grant { login: who, scopes } => {
                        let account = accounts
                            .iter()
                            .find(|a| a.host == host && same(&a.login, who));
                        let missing: Vec<&str> = scopes
                            .iter()
                            .filter(|s| !account.is_some_and(|a| a.has_scope(s)))
                            .map(String::as_str)
                            .collect();
                        if missing.is_empty() {
                            login = account.map(|a| a.login.clone());
                            State::Done {
                                login: login.clone().unwrap_or_else(|| who.clone()),
                            }
                        } else {
                            State::Failed {
                                reason: format!(
                                    "the GitHub CLI finished, but {who}'s token still lacks {}",
                                    missing.join(", ")
                                ),
                            }
                        }
                    }
                    // The new account is gh's active one until it is switched back.
                    Kind::SignIn { .. } => {
                        match accounts.iter().find(|a| a.host == host && a.active) {
                            Some(a) => {
                                login = Some(a.login.clone());
                                State::Done {
                                    login: a.login.clone(),
                                }
                            }
                            None => State::Failed {
                                reason: format!(
                                    "the GitHub CLI finished, but has no active account on {host}"
                                ),
                            },
                        }
                    }
                },
            },
        };
        // Back to the account that was active before, whatever happened.
        let note = back.restore().await.err().map(|e| {
            format!(
                "gh's active account couldn't be switched back to {}: {e:#}",
                back.to.as_deref().unwrap_or("the one before")
            )
        });
        if let (State::Failed { reason }, Some(note)) = (&mut state, &note) {
            *reason = format!("{reason}; {note}");
        }
        let flow = {
            let mut current = self.current.lock().unwrap();
            let Some(c) = current.as_mut() else {
                unreachable!("a flow's state is set while it runs")
            };
            c.flow.state = state;
            if login.is_some() {
                c.flow.login = login;
            }
            c.flow.note = note;
            c.cancel = None;
            c.flow.clone()
        };
        self.changed.notify_waiters();
        flow
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Run {
        /// gh succeeds: the grant's scopes are added, or the new account signs in.
        Succeed,
        Fail,
        /// gh waits until it is killed.
        Hang,
        /// gh prints no code and exits.
        NoCode,
    }

    struct FakeGh {
        accounts: Arc<Mutex<Vec<GhAccount>>>,
        calls: Arc<Mutex<Vec<String>>>,
        run: Run,
        lines: Vec<String>,
    }

    fn account(login: &str, active: bool, scopes: &[&str]) -> GhAccount {
        GhAccount {
            login: login.into(),
            host: "github.com".into(),
            active,
            scopes: scopes.iter().map(|s| s.to_string()).collect(),
        }
    }

    impl FakeGh {
        fn new(run: Run) -> Arc<Self> {
            Arc::new(Self {
                accounts: Arc::new(Mutex::new(vec![
                    account("beltoalb", true, &["gist", "read:org", "repo"]),
                    account("mcbelton", false, &["gist", "read:org", "repo", "workflow"]),
                ])),
                calls: Arc::default(),
                run,
                lines: vec![
                    "! First copy your one-time code: \u{1b}[1m1A2B-3C4D\u{1b}[0m".into(),
                    "Open this URL to continue in your web browser: https://github.com/login/device"
                        .into(),
                ],
            })
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }

        fn active(&self) -> String {
            let accounts = self.accounts.lock().unwrap();
            accounts.iter().find(|a| a.active).unwrap().login.clone()
        }
    }

    fn activate(accounts: &mut [GhAccount], login: &str) {
        for a in accounts.iter_mut() {
            a.active = a.login == login;
        }
    }

    #[async_trait]
    impl Gh for FakeGh {
        async fn accounts(&self) -> Result<Vec<GhAccount>> {
            Ok(self.accounts.lock().unwrap().clone())
        }

        async fn switch(&self, host: &str, login: &str) -> Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("switch {host} {login}"));
            activate(&mut self.accounts.lock().unwrap(), login);
            Ok(())
        }

        async fn start(&self, args: &[String]) -> Result<Started> {
            self.calls.lock().unwrap().push(args.join(" "));
            let (tx, lines) = mpsc::unbounded_channel();
            if self.run != Run::NoCode {
                for line in &self.lines {
                    tx.send(line.clone()).unwrap();
                }
            } else {
                tx.send("error: something broke".into()).unwrap();
            }
            let (ended_tx, ended) = oneshot::channel();
            let (kill, kill_rx) = oneshot::channel::<()>();
            let (run, accounts, calls, args) = (
                self.run,
                self.accounts.clone(),
                self.calls.clone(),
                args.to_vec(),
            );
            tokio::spawn(async move {
                let out = tx;
                let ok = match run {
                    Run::Succeed => {
                        let mut accounts = accounts.lock().unwrap();
                        if args[1] == "refresh" {
                            let scopes = args.last().unwrap().split(',');
                            let a = accounts.iter_mut().find(|a| a.active).unwrap();
                            a.scopes.extend(scopes.map(str::to_owned));
                        } else {
                            accounts.push(account("carol", false, &["repo", "project"]));
                            activate(&mut accounts, "carol");
                        }
                        true
                    }
                    Run::Fail => {
                        let _ = out.send("error: authentication failed".into());
                        false
                    }
                    Run::NoCode => false,
                    Run::Hang => {
                        let _ = kill_rx.await;
                        calls.lock().unwrap().push("killed".into());
                        false
                    }
                };
                let _ = ended_tx.send(ok);
                drop(out);
            });
            Ok(Started { lines, ended, kill })
        }

        fn switch_now(&self, host: &str, login: &str) {
            self.calls
                .lock()
                .unwrap()
                .push(format!("switch now {host} {login}"));
            activate(&mut self.accounts.lock().unwrap(), login);
        }
    }

    fn grant() -> Kind {
        Kind::Grant {
            login: "mcbelton".into(),
            scopes: vec!["project".into()],
        }
    }

    #[test]
    fn accounts_are_read_with_their_scopes() {
        let json: Value = serde_json::json!({ "hosts": {
            "github.com": [
                { "state": "success", "active": true, "login": "beltoalb", "scopes": "gist, read:org, repo" },
                { "state": "success", "active": false, "login": "mcbelton", "scopes": "gist, read:org, repo, workflow" },
                { "state": "error", "active": false, "login": "gone", "scopes": "" },
            ],
            "acme.ghe.com": [{ "state": "success", "active": true, "login": "dev", "scopes": ["repo", "project"] }],
        }});
        let accounts = parse_accounts(&json);
        assert_eq!(accounts.len(), 3);
        let mc = accounts.iter().find(|a| a.login == "mcbelton").unwrap();
        assert_eq!(mc.scopes, ["gist", "read:org", "repo", "workflow"]);
        assert!(!mc.active && mc.has_scope("workflow") && !mc.has_scope("project"));
        let dev = accounts.iter().find(|a| a.host == "acme.ghe.com").unwrap();
        assert!(dev.active && dev.has_scope("project"));
        assert!(check_scopes(&["project".into(), "read:org".into()]).is_ok());
        assert!(check_scopes(&["project; rm".into()]).is_err());
        assert!(check_scopes(&["".into()]).is_err());
    }

    #[test]
    fn gh_output_gives_the_code_and_where_to_enter_it() {
        let line = ANSI.replace_all(
            "! First copy your one-time code: \u{1b}[1m1A2B-3C4D\u{1b}[0m",
            "",
        );
        assert_eq!(device_code(&line).as_deref(), Some("1A2B-3C4D"));
        assert_eq!(device_code("no code here: abcd-1234"), None);
        assert_eq!(
            device_url(
                "Open this URL to continue in your web browser: https://acme.ghe.com/login/device."
            )
            .as_deref(),
            Some("https://acme.ghe.com/login/device")
        );
    }

    #[tokio::test]
    async fn a_grant_switches_to_the_account_refreshes_and_switches_back() {
        let gh = FakeGh::new(Run::Succeed);
        let flows = Flows::new(gh.clone());
        let (tx, rx) = oneshot::channel();
        let flow = flows
            .start(grant(), "github.com", move |f| {
                let _ = tx.send(f);
            })
            .await
            .unwrap();
        assert_eq!(flow.code, "1A2B-3C4D");
        assert_eq!(flow.url, "https://github.com/login/device");
        assert_eq!(flow.login.as_deref(), Some("mcbelton"));
        assert_eq!(flow.active_before.as_deref(), Some("beltoalb"));

        let ended = rx.await.unwrap();
        assert_eq!(
            ended.state,
            State::Done {
                login: "mcbelton".into()
            }
        );
        assert_eq!(ended.note, None);
        assert_eq!(
            gh.calls(),
            [
                "switch github.com mcbelton",
                "auth refresh --hostname github.com --scopes project",
                "switch github.com beltoalb",
            ]
        );
        assert_eq!(gh.active(), "beltoalb");
        let accounts = gh.accounts.lock().unwrap().clone();
        assert!(
            accounts
                .iter()
                .any(|a| a.login == "mcbelton" && a.has_scope("project"))
        );
        assert_eq!(flows.current().unwrap().state, ended.state);
    }

    #[tokio::test]
    async fn a_grant_for_the_active_account_switches_nothing() {
        let gh = FakeGh::new(Run::Succeed);
        let flows = Flows::new(gh.clone());
        let kind = Kind::Grant {
            login: "BeltoAlb".into(),
            scopes: vec!["project".into()],
        };
        flows.start(kind, "github.com", |_| {}).await.unwrap();
        let ended = flows.ended().await.unwrap();
        assert!(matches!(ended.state, State::Done { .. }), "{ended:?}");
        assert_eq!(
            gh.calls(),
            ["auth refresh --hostname github.com --scopes project"]
        );
    }

    #[tokio::test]
    async fn gh_is_switched_back_when_a_grant_fails_is_cancelled_or_times_out() {
        // Fails.
        let gh = FakeGh::new(Run::Fail);
        let flows = Flows::new(gh.clone());
        flows.start(grant(), "github.com", |_| {}).await.unwrap();
        let ended = flows.ended().await.unwrap();
        let State::Failed { reason } = &ended.state else {
            panic!("{ended:?}")
        };
        assert!(reason.contains("authentication failed"), "{reason}");
        assert_eq!(gh.active(), "beltoalb");
        assert_eq!(gh.calls().last().unwrap(), "switch github.com beltoalb");

        // Cancelled while it waits: gh is stopped, and the flow says so.
        let gh = FakeGh::new(Run::Hang);
        let flows = Flows::new(gh.clone());
        flows.start(grant(), "github.com", |_| {}).await.unwrap();
        assert_eq!(flows.current().unwrap().state, State::Waiting);
        assert_eq!(gh.active(), "mcbelton");
        assert!(flows.cancel());
        let ended = flows.ended().await.unwrap();
        assert_eq!(ended.state, State::Cancelled);
        assert_eq!(gh.active(), "beltoalb");
        assert!(gh.calls().contains(&"killed".to_owned()));
        assert!(!flows.cancel(), "nothing waits any more");

        // Nobody enters the code in time.
        let gh = FakeGh::new(Run::Hang);
        let flows = Flows::with_timeouts(gh.clone(), CODE_TIMEOUT, Duration::from_millis(50));
        flows.start(grant(), "github.com", |_| {}).await.unwrap();
        let ended = flows.ended().await.unwrap();
        assert!(
            matches!(&ended.state, State::Failed { reason } if reason.contains("in time")),
            "{ended:?}"
        );
        assert_eq!(gh.active(), "beltoalb");

        // gh shows no code at all.
        let gh = FakeGh::new(Run::NoCode);
        let flows = Flows::new(gh.clone());
        let error = flows
            .start(grant(), "github.com", |_| {})
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("didn't show a sign-in code"), "{error}");
        assert!(error.contains("something broke"), "{error}");
        assert_eq!(gh.active(), "beltoalb");
    }

    #[tokio::test]
    async fn one_flow_waits_at_a_time_and_unknown_accounts_are_refused() {
        let gh = FakeGh::new(Run::Hang);
        let flows = Flows::new(gh.clone());
        flows.start(grant(), "github.com", |_| {}).await.unwrap();
        let error = flows
            .start(grant(), "github.com", |_| {})
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("already waiting") && error.contains("1A2B-3C4D"),
            "{error}"
        );
        flows.cancel();
        flows.ended().await.unwrap();

        let unknown = Kind::Grant {
            login: "nobody".into(),
            scopes: vec!["project".into()],
        };
        let error = flows
            .start(unknown, "github.com", |_| {})
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("no account nobody"), "{error}");
    }

    #[tokio::test]
    async fn the_agent_stopping_mid_flow_still_switches_gh_back() {
        let gh = FakeGh::new(Run::Hang);
        gh.switch("github.com", "mcbelton").await.unwrap();
        // A flow's task that is stopped drops its switch-back while it is still armed.
        drop(SwitchBack {
            gh: gh.clone(),
            host: "github.com".into(),
            to: Some("beltoalb".into()),
            armed: true,
        });
        assert_eq!(gh.active(), "beltoalb");
        assert_eq!(gh.calls().last().unwrap(), "switch now github.com beltoalb");
    }

    #[tokio::test]
    async fn signing_in_another_account_switches_gh_back_and_names_the_new_account() {
        let gh = FakeGh::new(Run::Succeed);
        let flows = Flows::new(gh.clone());
        let kind = Kind::SignIn {
            scopes: vec!["project".into(), "workflow".into()],
        };
        let flow = flows.start(kind, "github.com", |_| {}).await.unwrap();
        assert_eq!(flow.kind, "sign_in");
        assert_eq!(flow.login, None);
        let ended = flows.ended().await.unwrap();
        assert_eq!(
            ended.state,
            State::Done {
                login: "carol".into()
            }
        );
        assert_eq!(ended.login.as_deref(), Some("carol"));
        assert_eq!(
            gh.calls(),
            [
                "auth login --hostname github.com --web --git-protocol https --skip-ssh-key --scopes project,workflow",
                "switch github.com beltoalb",
            ]
        );
        assert_eq!(gh.active(), "beltoalb");
    }

    /// The real GitHub CLI runner, with a script in gh's place: it gets gh's arguments,
    /// and its output is read line by line until it ends.
    #[tokio::test]
    async fn the_github_cli_runner_passes_the_arguments_and_reads_the_output() {
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("args.txt");
        let (program, prefix) = if cfg!(windows) {
            let script = dir.path().join("gh.cmd");
            std::fs::write(
                &script,
                format!(
                    "@echo off\r\necho %*> \"{}\"\r\necho ! First copy your one-time code: 9Z8Y-7X6W 1>&2\r\necho Open this URL to continue in your web browser: https://github.com/login/device\r\nexit /b 0\r\n",
                    record.display()
                ),
            )
            .unwrap();
            (script, Vec::new())
        } else {
            let script = dir.path().join("gh.sh");
            std::fs::write(
                &script,
                format!(
                    "echo \"$@\" > '{}'\necho '! First copy your one-time code: 9Z8Y-7X6W' >&2\necho 'Open this URL to continue in your web browser: https://github.com/login/device'\nexit 0\n",
                    record.display()
                ),
            )
            .unwrap();
            (PathBuf::from("sh"), vec![script.display().to_string()])
        };
        let gh = GhCli::with_program(program, prefix);
        let args: Vec<String> = [
            "auth",
            "refresh",
            "--hostname",
            "github.com",
            "--scopes",
            "project",
        ]
        .map(str::to_owned)
        .into();
        let mut started = gh.start(&args).await.unwrap();
        let mut lines = Vec::new();
        while let Some(line) = started.lines.recv().await {
            lines.push(line);
        }
        assert!(started.ended.await.unwrap());
        let all = lines.join("\n");
        assert_eq!(device_code(&all).as_deref(), Some("9Z8Y-7X6W"), "{all}");
        assert_eq!(
            device_url(&all).as_deref(),
            Some("https://github.com/login/device")
        );
        let recorded = std::fs::read_to_string(&record).unwrap();
        assert_eq!(
            recorded.trim(),
            "auth refresh --hostname github.com --scopes project"
        );
    }
}
