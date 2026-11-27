//! The Work IQ CLI over stdio (see docs/work-iq.md, section 1): [`StdioTransport`] starts
//! `workiq mcp` and speaks newline-delimited JSON-RPC to it. The CLI signs in by itself, so
//! there is no bearer ([`NoToken`]).

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use super::client::{
    Incoming, Outgoing, READ_TOOLS, RequestIds, TokenSource, Transport, WorkIqError,
};
use crate::profile::INHERITED_CREDENTIAL_VARS;

/// The longest Proofkin waits for the CLI to answer one message.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
/// The longest Proofkin waits for npm or the CLI to say where the CLI is or whose account it
/// uses.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(30);
/// How long Proofkin waits for a stopped CLI to finish, to read what it said on stderr.
const EXIT_WAIT: Duration = Duration::from_secs(2);
const STDERR_KEPT: usize = 600;

/// The token source of a CLI account: there is no bearer, the CLI signs in by itself.
pub struct NoToken;

#[async_trait]
impl TokenSource for NoToken {
    async fn token(&self) -> Result<String> {
        Ok(String::new())
    }
}

struct Running {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    /// Ends with the CLI's last words on stderr when the CLI stops.
    stderr: JoinHandle<String>,
}

#[derive(Default)]
struct State {
    running: Option<Running>,
    /// What the CLI was told to start its session: `initialize` and `initialized`. A CLI that
    /// is started again gets them again before the message that was being sent.
    handshake: Vec<Value>,
    /// The result of the last successful `initialize`.
    initialized: Option<Value>,
}

/// Why a message did not get through, and whether a new CLI process may try it again.
struct Failure {
    text: String,
    retry: bool,
}

/// Runs the Work IQ CLI as a child process and talks to it over its stdin and stdout. One
/// message at a time; the child is started again once if it exits, and killed when the
/// transport is dropped.
pub struct StdioTransport {
    program: OsString,
    /// What comes before the CLI's own arguments (`-y @microsoft/workiq` for npx).
    base: Vec<OsString>,
    account: Option<String>,
    state: Mutex<State>,
}

impl StdioTransport {
    /// The Work IQ CLI (see [`cli_command`]) running `mcp`, with `--account` when the account
    /// is given.
    pub fn new(program: Option<&Path>, account: Option<&str>) -> Self {
        let (program, base) = cli_command(program);
        Self::with_command(program, &base, account)
    }

    /// Starts `program` with `args` in place of the CLI, for tests: then `mcp` and
    /// `--account`, as for the CLI.
    pub fn with_command(
        program: impl Into<OsString>,
        args: &[&str],
        account: Option<&str>,
    ) -> Self {
        StdioTransport {
            program: program.into(),
            base: args.iter().map(OsString::from).collect(),
            account: account.map(str::to_string),
            state: Mutex::new(State::default()),
        }
    }

    /// The account the CLI signs in with when it is given none: `defaultAccount` in what
    /// `workiq config show` prints, if it says (best effort).
    pub async fn default_account(&self) -> Option<String> {
        let mut command = Command::new(&self.program);
        command.args(&self.base).args(["config", "show"]);
        for var in INHERITED_CREDENTIAL_VARS {
            command.env_remove(var);
        }
        command
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let out = tokio::time::timeout(LOOKUP_TIMEOUT, command.output())
            .await
            .ok()?
            .ok()?;
        out.status
            .success()
            .then(|| default_account_in(&String::from_utf8_lossy(&out.stdout)))
            .flatten()
    }

    /// The user the CLI reported in its `initialize` reply (`serverInfo.user`, else
    /// `serverInfo.upn`), when it did.
    pub async fn reported_user(&self) -> Option<String> {
        let state = self.state.lock().await;
        let info = state.initialized.as_ref()?.get("serverInfo")?;
        ["user", "upn"]
            .iter()
            .find_map(|key| info.get(*key).and_then(Value::as_str))
            .map(str::trim)
            .filter(|user| !user.is_empty())
            .map(str::to_string)
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command.args(&self.base).arg("mcp");
        if let Some(account) = &self.account {
            command.arg("--account").arg(account);
        }
        for var in INHERITED_CREDENTIAL_VARS {
            command.env_remove(var);
        }
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command
    }

    fn spawn(&self) -> Result<Running, Failure> {
        let failed = |text: String| Failure { text, retry: false };
        let mut child = self
            .command()
            .spawn()
            .map_err(|e| failed(start_error(&self.program, &e)))?;
        let pipes = (child.stdin.take(), child.stdout.take(), child.stderr.take());
        let (Some(stdin), Some(stdout), Some(stderr)) = pipes else {
            return Err(failed("could not connect to the Work IQ CLI".to_string()));
        };
        Ok(Running {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            stderr: tokio::spawn(read_tail(stderr)),
        })
    }

    /// Sends `message` and waits for its response, starting the CLI first when it isn't
    /// running. A freshly started CLI first gets the handshake again when `replay`.
    async fn attempt(
        &self,
        state: &mut State,
        message: &Value,
        expect_id: Option<u64>,
        replay: bool,
        written: &mut bool,
    ) -> Result<Option<Value>, Failure> {
        let exited = state
            .running
            .as_mut()
            .is_some_and(|run| !matches!(run.child.try_wait(), Ok(None)));
        if exited {
            state.running = None;
        }
        let fresh = state.running.is_none();
        let handshake = if fresh && replay {
            state.handshake.clone()
        } else {
            Vec::new()
        };
        let run = match &mut state.running {
            Some(run) => run,
            empty => empty.insert(self.spawn()?),
        };
        let exchange = exchange(run, &handshake, message, expect_id, written);
        match tokio::time::timeout(REQUEST_TIMEOUT, exchange).await {
            Ok(Ok(body)) => Ok(body),
            Ok(Err(error)) => Err(stopped(state, &error).await),
            Err(_) => {
                state.running = None;
                Err(Failure {
                    text: format!(
                        "the Work IQ CLI did not answer in {} seconds",
                        REQUEST_TIMEOUT.as_secs()
                    ),
                    retry: false,
                })
            }
        }
    }
}

#[async_trait]
impl Transport for StdioTransport {
    fn is_cli(&self) -> bool {
        true
    }

    async fn send(&self, outgoing: Outgoing) -> Result<Incoming, WorkIqError> {
        let ids = RequestIds {
            client_request_id: Some(outgoing.client_request_id.clone()),
            ..RequestIds::default()
        };
        let message = &outgoing.message;
        let method = message.get("method").and_then(Value::as_str).unwrap_or("");
        let replay = method != "initialize";
        let mut state = self.state.lock().await;
        let mut written = false;
        let mut outcome = self
            .attempt(
                &mut state,
                message,
                outgoing.expect_id,
                replay,
                &mut written,
            )
            .await;
        // A change that may have reached the CLI before it stopped is not sent again.
        let again = outcome
            .as_ref()
            .err()
            .is_some_and(|failure| failure.retry && !(written && is_change(message)));
        if again {
            written = false;
            outcome = self
                .attempt(
                    &mut state,
                    message,
                    outgoing.expect_id,
                    replay,
                    &mut written,
                )
                .await;
        }
        let body = outcome.map_err(|failure| WorkIqError::transport(failure.text, ids.clone()))?;
        let result = body.as_ref().and_then(|body| body.get("result")).cloned();
        match method {
            "initialize" if result.is_some() => {
                state.handshake = vec![message.clone()];
                state.initialized = result;
            }
            "notifications/initialized" => state.handshake.push(message.clone()),
            _ => {}
        }
        Ok(Incoming {
            body,
            session_id: None,
            ids,
        })
    }
}

/// Writes the handshake (and reads the answer to `initialize`), then `message`, and reads the
/// response to it. `written` says whether `message` was written.
async fn exchange(
    run: &mut Running,
    handshake: &[Value],
    message: &Value,
    expect_id: Option<u64>,
    written: &mut bool,
) -> io::Result<Option<Value>> {
    for step in handshake {
        write_line(&mut run.stdin, step).await?;
        if let Some(id) = step.get("id").and_then(Value::as_u64) {
            read_response(&mut run.stdout, id).await?;
        }
    }
    write_line(&mut run.stdin, message).await?;
    *written = true;
    match expect_id {
        Some(id) => Ok(Some(read_response(&mut run.stdout, id).await?)),
        None => Ok(None),
    }
}

async fn write_line(stdin: &mut ChildStdin, message: &Value) -> io::Result<()> {
    let mut line = serde_json::to_vec(message)?;
    line.push(b'\n');
    stdin.write_all(&line).await?;
    stdin.flush().await
}

/// The response with `id`, skipping notifications, requests from the CLI and lines that
/// aren't JSON.
async fn read_response(stdout: &mut BufReader<ChildStdout>, id: u64) -> io::Result<Value> {
    let mut line = Vec::new();
    loop {
        line.clear();
        if stdout.read_until(b'\n', &mut line).await? == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        let Ok(message) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        let is_response = message.get("method").is_none()
            && (message.get("result").is_some() || message.get("error").is_some());
        if is_response && message.get("id") == Some(&json!(id)) {
            return Ok(message);
        }
    }
}

/// What the CLI printed on stderr last.
async fn read_tail(stderr: ChildStderr) -> String {
    let mut reader = BufReader::new(stderr);
    let mut tail: Vec<u8> = Vec::new();
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line).await {
            Ok(0) | Err(_) => break,
            Ok(_) => tail.extend_from_slice(&line),
        }
        if tail.len() > STDERR_KEPT {
            let excess = tail.len() - STDERR_KEPT;
            tail.drain(..excess);
        }
    }
    String::from_utf8_lossy(&tail).into_owned()
}

fn start_error(program: &OsStr, error: &io::Error) -> String {
    format!(
        "could not start the Work IQ CLI ({}): {error}; install it with `npm install -g \
         @microsoft/workiq` and sign in again (`proofkin auth login workiq --cli`, with \
         `--cli-path` if it isn't found), or install Node.js so that `npx` works",
        program.to_string_lossy()
    )
}

/// The CLI stopped or broke the pipe: ends it, and says what it said on stderr.
async fn stopped(state: &mut State, error: &io::Error) -> Failure {
    let mut text = "the Work IQ CLI stopped without answering".to_string();
    if error.kind() != io::ErrorKind::UnexpectedEof {
        text.push_str(&format!(" ({error})"));
    }
    if let Some(run) = state.running.take() {
        let Running {
            mut child,
            stdin,
            stderr,
            ..
        } = run;
        drop(stdin);
        let _ = tokio::time::timeout(EXIT_WAIT, child.wait()).await;
        if let Ok(Ok(tail)) = tokio::time::timeout(EXIT_WAIT, stderr).await {
            let tail = tail.trim();
            if !tail.is_empty() {
                text.push_str(&format!(": {tail}"));
            }
        }
    }
    Failure { text, retry: true }
}

/// Whether `message` is a call of a tool that may change things in Microsoft 365, or isn't
/// known to only read.
fn is_change(message: &Value) -> bool {
    let is_call = message.get("method").and_then(Value::as_str) == Some("tools/call");
    let reads = message
        .pointer("/params/name")
        .and_then(Value::as_str)
        .is_some_and(|name| READ_TOOLS.contains(&name));
    is_call && !reads
}

/// The first file called `name` in a PATH directory (with the extensions Windows runs).
fn find_on_path(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    let extensions: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string())
            .split(';')
            .filter(|ext| !ext.is_empty())
            .map(str::to_string)
            .collect()
    } else {
        vec![String::new()]
    };
    for dir in std::env::split_paths(&paths) {
        for ext in &extensions {
            let path = dir.join(format!("{name}{ext}"));
            if path.is_file() {
                return Some(path);
            }
        }
    }
    None
}

/// The command that runs the Work IQ CLI, and what comes before its own arguments: the CLI
/// at `program` if it is still there, else `workiq` on the PATH, else `npx -y
/// @microsoft/workiq` (which fetches it from the npm registry).
pub fn cli_command(program: Option<&Path>) -> (OsString, Vec<&'static str>) {
    let found = program
        .filter(|path| path.is_file())
        .map(Path::to_path_buf)
        .or_else(|| find_on_path("workiq"));
    match found {
        Some(path) => (path.into_os_string(), Vec::new()),
        None => {
            let npx = find_on_path("npx").unwrap_or_else(|| PathBuf::from("npx"));
            (npx.into_os_string(), vec!["-y", "@microsoft/workiq"])
        }
    }
}

/// Where the Work IQ CLI is: `workiq` on the PATH, else where npm installs global commands
/// (`npm prefix -g`), which a Node version manager may keep off the PATH.
pub async fn find_cli() -> Option<PathBuf> {
    if let Some(path) = find_on_path("workiq") {
        return Some(path);
    }
    let npm = find_on_path("npm")?;
    let mut command = Command::new(npm);
    command
        .args(["prefix", "-g"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let out = tokio::time::timeout(LOOKUP_TIMEOUT, command.output())
        .await
        .ok()?
        .ok()?;
    let prefix = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !prefix.is_empty())
        .then(|| in_npm_prefix(Path::new(&prefix)))
        .flatten()
}

/// The Work IQ CLI npm installed globally under `prefix`, if it is there: `workiq.cmd` in
/// it on Windows, `bin/workiq` elsewhere.
pub fn in_npm_prefix(prefix: &Path) -> Option<PathBuf> {
    let path = if cfg!(windows) {
        prefix.join("workiq.cmd")
    } else {
        prefix.join("bin").join("workiq")
    };
    path.is_file().then_some(path)
}

/// The `defaultAccount` that `workiq config show` printed (`key=value` lines), if any.
pub fn default_account_in(shown: &str) -> Option<String> {
    shown
        .lines()
        .find_map(|line| line.trim().strip_prefix("defaultAccount="))
        .map(str::trim)
        .filter(|account| {
            !account.is_empty() && account.len() <= 256 && !account.contains(char::is_whitespace)
        })
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cli_is_found_where_npm_installs_it_and_names_its_default_account() {
        let prefix = tempfile::tempdir().unwrap();
        assert_eq!(in_npm_prefix(prefix.path()), None);
        let cli = if cfg!(windows) {
            prefix.path().join("workiq.cmd")
        } else {
            std::fs::create_dir(prefix.path().join("bin")).unwrap();
            prefix.path().join("bin").join("workiq")
        };
        std::fs::write(&cli, "").unwrap();
        assert_eq!(in_npm_prefix(prefix.path()), Some(cli.clone()));
        // A CLI found before is used while it is there.
        assert_eq!(cli_command(Some(&cli)), (cli.into_os_string(), Vec::new()));

        let shown = "isMSITTenant=true\r\ndefaultAccount=alice@contoso.example\r\n";
        assert_eq!(
            default_account_in(shown).as_deref(),
            Some("alice@contoso.example")
        );
        for none in [
            "",
            "isMSITTenant=true",
            "defaultAccount=",
            "defaultAccount=a b",
        ] {
            assert_eq!(default_account_in(none), None, "{none:?}");
        }
    }
}
