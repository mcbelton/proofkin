use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, oneshot};

use super::{Access, LlmTask, Provider, Ran};
use crate::mcp::McpServer;
use crate::profile::INHERITED_CREDENTIAL_VARS;

type Pending = Arc<Mutex<Option<HashMap<u64, oneshot::Sender<Result<Ran, String>>>>>>;

#[derive(Serialize)]
struct Request<'a> {
    id: u64,
    provider: Provider,
    prompt: &'a str,
    cwd: &'a Path,
    access: Access,
    model: Option<&'a str>,
    /// MCP servers by name, as `{type, command|url, ..., tools}`.
    #[serde(skip_serializing_if = "serde_json::Map::is_empty")]
    mcp: serde_json::Map<String, serde_json::Value>,
}

#[derive(Deserialize)]
struct Response {
    id: u64,
    ok: bool,
    #[serde(default)]
    text: String,
    #[serde(default)]
    error: String,
    /// The model that answered, if the SDK says.
    #[serde(default)]
    model: Option<String>,
}

/// Client for `bridge/index.mjs`, a long-lived Node process that runs tasks
/// with the official Claude Agent SDK and OpenAI Codex SDK. Requests are
/// multiplexed by id, so several tasks can run concurrently.
pub struct Bridge {
    stdin: Arc<tokio::sync::Mutex<ChildStdin>>,
    pending: Pending,
    next_id: AtomicU64,
    /// Cancel lines, written by a background task (`Drop` can't await the stdin lock).
    cancels: mpsc::UnboundedSender<Vec<u8>>,
    _child: Child,
}

impl Bridge {
    /// Starts `node <script>` with inherited credential variables removed and
    /// `env` (the agent profile's config homes and secrets) set instead.
    pub fn spawn_node(script: &Path, env: Vec<(String, OsString)>) -> Result<Self> {
        if !script.is_file() {
            return Err(anyhow!(
                "bridge script {} not found; run `proofkin setup` (needed for claude and codex)",
                script.display()
            ));
        }
        let mut cmd = Command::new("node");
        cmd.arg(script);
        for var in INHERITED_CREDENTIAL_VARS {
            cmd.env_remove(var);
        }
        cmd.envs(env);
        Self::spawn(cmd)
    }

    pub fn spawn(mut cmd: Command) -> Result<Self> {
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .context("starting the Node bridge (is Node.js installed?)")?;
        let stdin = child.stdin.take().expect("piped");
        let stdout = child.stdout.take().expect("piped");
        let pending: Pending = Arc::new(Mutex::new(Some(HashMap::new())));

        let reader_pending = pending.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(resp) = serde_json::from_str::<Response>(&line) else {
                    eprintln!("bridge: unexpected output: {line}");
                    continue;
                };
                let waiter = reader_pending
                    .lock()
                    .unwrap()
                    .as_mut()
                    .and_then(|p| p.remove(&resp.id));
                if let Some(waiter) = waiter {
                    let _ = waiter.send(if resp.ok {
                        Ok(Ran::new(resp.text, resp.model))
                    } else {
                        Err(resp.error)
                    });
                }
            }
            // Bridge exited: dropping the waiters fails in-flight requests; `None` refuses new ones.
            reader_pending.lock().unwrap().take();
        });

        let stdin = Arc::new(tokio::sync::Mutex::new(stdin));
        let (cancels, mut cancel_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let cancel_stdin = stdin.clone();
        tokio::spawn(async move {
            while let Some(line) = cancel_rx.recv().await {
                let mut stdin = cancel_stdin.lock().await;
                if stdin.write_all(&line).await.is_err() || stdin.flush().await.is_err() {
                    break;
                }
            }
        });

        Ok(Self {
            stdin,
            pending,
            next_id: AtomicU64::new(1),
            cancels,
            _child: child,
        })
    }

    pub async fn run(&self, task: &LlmTask, cwd: &Path, access: Access) -> Result<Ran> {
        self.run_with(task, cwd, access, &BTreeMap::new()).await
    }

    /// [`Bridge::run`] with the MCP servers the model may use.
    pub async fn run_with(
        &self,
        task: &LlmTask,
        cwd: &Path,
        access: Access,
        mcp: &BTreeMap<String, McpServer>,
    ) -> Result<Ran> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap()
            .as_mut()
            .ok_or_else(|| anyhow!("bridge process exited"))?
            .insert(id, tx);

        let request = Request {
            id,
            provider: task.provider,
            prompt: &task.prompt,
            cwd,
            access,
            model: task.model.as_deref(),
            mcp: mcp
                .iter()
                .map(|(name, server)| (name.clone(), server.to_json()))
                .collect(),
        };
        let mut line = serde_json::to_vec(&request)?;
        line.push(b'\n');
        let written = {
            let mut stdin = self.stdin.lock().await;
            match stdin.write_all(&line).await {
                Ok(()) => stdin.flush().await,
                Err(e) => Err(e),
            }
        };
        if let Err(e) = written {
            if let Some(p) = self.pending.lock().unwrap().as_mut() {
                p.remove(&id);
            }
            return Err(e).context("writing to bridge");
        }

        let mut guard = CancelOnDrop {
            bridge: self,
            id,
            armed: true,
        };
        let result = rx.await;
        guard.armed = false;
        match result {
            Ok(Ok(ran)) => Ok(ran),
            Ok(Err(error)) => Err(anyhow!(error)),
            Err(_) => Err(anyhow!("bridge process exited")),
        }
    }

    /// Tells the bridge to abort request `id` (best effort; never blocks).
    fn cancel(&self, id: u64) {
        if let Some(p) = self.pending.lock().unwrap().as_mut() {
            p.remove(&id);
        }
        let mut line = serde_json::to_vec(&serde_json::json!({ "cancel": id })).unwrap();
        line.push(b'\n');
        let _ = self.cancels.send(line);
    }
}

/// Cancels a bridge request whose future was dropped before the reply arrived.
struct CancelOnDrop<'a> {
    bridge: &'a Bridge,
    id: u64,
    armed: bool,
}

impl Drop for CancelOnDrop<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.bridge.cancel(self.id);
        }
    }
}
