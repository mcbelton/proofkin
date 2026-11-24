//! The step's checks as a tool for its model: a small MCP server over stdio,
//! `proofkin task-checks`, which a worker starts for a task that asks for checks its
//! folder defines (see [`crate::agent`]). Its one tool, [`TOOL`], runs one of them in the
//! task's workspace and says how it went, so the model learns of a mistake while it works
//! instead of from a failed attempt. What it learns is never evidence: the runtime runs
//! the checks again after the model, and only those runs count.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

use crate::mcp::McpServer;

/// The server's name in the task's MCP servers.
pub const SERVER: &str = "proofkin-checks";
/// Its one tool.
pub const TOOL: &str = "run_check";
/// The most times a task's model may run checks.
pub const MAX_RUNS: u32 = 10;
/// The longest one run may take.
pub const RUN_TIMEOUT: Duration = Duration::from_secs(600);
/// The MCP versions it speaks, newest first.
const VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

/// The MCP server a worker gives a task's model: `program` (this program) serving
/// `checks` (name, command) in the task's workspace `dir`.
pub fn server(program: &Path, dir: &Path, checks: &[(String, String)]) -> McpServer {
    let mut args = vec![
        "task-checks".to_owned(),
        "--dir".to_owned(),
        dir.display().to_string(),
    ];
    for (name, command) in checks {
        args.push("--check".to_owned());
        args.push(format!("{name}={command}"));
    }
    let mut server = McpServer::local(program.display().to_string(), args);
    server.tools = vec![TOOL.to_owned()];
    server
}

/// The server: the checks (name → command), where they run, and how many runs were made.
pub struct Checks {
    dir: PathBuf,
    checks: BTreeMap<String, String>,
    runs: u32,
}

impl Checks {
    pub fn new(dir: PathBuf, checks: BTreeMap<String, String>) -> Self {
        Self {
            dir,
            checks,
            runs: 0,
        }
    }

    /// The reply to JSON-RPC message `message`, if it takes one (notifications don't).
    pub async fn handle(&mut self, message: &Value) -> Option<Value> {
        let id = message.get("id")?.clone();
        let method = message["method"].as_str().unwrap_or_default();
        let result = match method {
            "initialize" => {
                let asked = message["params"]["protocolVersion"]
                    .as_str()
                    .unwrap_or_default();
                let version = VERSIONS
                    .iter()
                    .find(|v| **v == asked)
                    .unwrap_or(&VERSIONS[0]);
                json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": { "name": SERVER, "version": env!("CARGO_PKG_VERSION") },
                })
            }
            "ping" => json!({}),
            "tools/list" => self.tools(),
            "tools/call" => self.call(&message["params"]).await,
            _ => {
                return Some(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32601, "message": format!("Method not found: {method}") },
                }));
            }
        };
        Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
    }

    fn tools(&self) -> Value {
        let names: Vec<&String> = self.checks.keys().collect();
        json!({ "tools": [{
            "name": TOOL,
            "title": "Run one of the step's checks",
            "description": format!(
                "Runs one of the checks this step is checked with ({}) in your workspace, as the runtime will after you finish, and says whether it passed, with the end of its output. Use it to find mistakes while you work. At most {MAX_RUNS} runs, each up to {} minutes; only the runs after you finish count.",
                names.iter().map(|n| n.as_str()).collect::<Vec<_>>().join(", "),
                RUN_TIMEOUT.as_secs() / 60
            ),
            "inputSchema": {
                "type": "object",
                "properties": { "name": { "type": "string", "enum": names, "description": "The check to run" } },
                "required": ["name"],
                "additionalProperties": false,
            },
            "annotations": { "readOnlyHint": false, "openWorldHint": false },
        }] })
    }

    async fn call(&mut self, params: &Value) -> Value {
        let said = |text: String, error: bool| json!({ "content": [{ "type": "text", "text": text }], "isError": error });
        if params["name"].as_str() != Some(TOOL) {
            return said(format!("there is no tool {}", params["name"]), true);
        }
        let name = params["arguments"]["name"].as_str().unwrap_or_default();
        let Some(command) = self.checks.get(name) else {
            let names: Vec<&str> = self.checks.keys().map(String::as_str).collect();
            return said(
                format!(
                    "there is no check {name:?}; this step's checks are: {}",
                    names.join(", ")
                ),
                true,
            );
        };
        if self.runs >= MAX_RUNS {
            return said(
                format!(
                    "you ran checks {MAX_RUNS} times already, the most a task may: finish your work, and the runtime runs them after you"
                ),
                true,
            );
        }
        self.runs += 1;
        let r = crate::evidence::run_check(name, command, &self.dir, RUN_TIMEOUT).await;
        let how = match r.exit_code {
            Some(code) => format!("exit code {code}"),
            None => "it timed out or couldn't start".to_owned(),
        };
        let output = crate::redact::redact(&r.output).0;
        said(
            format!(
                "check {name} {} ({how}, {:.1} s); {} of {MAX_RUNS} runs left\n\n{}",
                if r.passed { "passed" } else { "failed" },
                r.millis as f64 / 1000.0,
                MAX_RUNS - self.runs,
                if output.trim().is_empty() {
                    "(no output)"
                } else {
                    output.trim_end()
                }
            ),
            !r.passed,
        )
    }
}

/// Serves `checks` over stdin and stdout, a JSON-RPC message per line, until stdin closes
/// (`proofkin task-checks`).
pub async fn serve(dir: PathBuf, checks: BTreeMap<String, String>) -> Result<()> {
    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();
    let mut server = Checks::new(dir, checks);
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(message) => server.handle(&message).await,
            Err(e) => Some(json!({
                "jsonrpc": "2.0",
                "id": null,
                "error": { "code": -32700, "message": format!("Parse error: {e}") },
            })),
        };
        if let Some(reply) = reply {
            stdout.write_all(format!("{reply}\n").as_bytes()).await?;
            stdout.flush().await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checks(dir: &Path) -> Checks {
        let (pass, fail) = if cfg!(windows) {
            ("echo all good", "echo broken & exit /b 3")
        } else {
            ("echo all good", "echo broken; exit 3")
        };
        Checks::new(
            dir.to_path_buf(),
            [
                ("test".to_owned(), pass.to_owned()),
                ("lint".to_owned(), fail.to_owned()),
            ]
            .into(),
        )
    }

    async fn call(server: &mut Checks, name: &str) -> Value {
        let message = json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": { "name": TOOL, "arguments": { "name": name } } });
        server.handle(&message).await.unwrap()["result"].clone()
    }

    #[tokio::test]
    async fn the_model_runs_the_steps_checks_while_it_works_a_few_times() {
        let dir = tempfile::tempdir().unwrap();
        let mut server = checks(dir.path());
        let init = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": { "protocolVersion": "2025-03-26" } });
        let reply = server.handle(&init).await.unwrap();
        assert_eq!(reply["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(reply["result"]["serverInfo"]["name"], SERVER);
        // Notifications get no reply; unknown methods an error.
        let note = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        assert!(server.handle(&note).await.is_none());
        let odd = json!({ "jsonrpc": "2.0", "id": 9, "method": "resources/list" });
        assert_eq!(server.handle(&odd).await.unwrap()["error"]["code"], -32601);

        let list = json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" });
        let tools = server.handle(&list).await.unwrap()["result"]["tools"].clone();
        assert_eq!(tools[0]["name"], TOOL);
        assert_eq!(
            tools[0]["inputSchema"]["properties"]["name"]["enum"],
            json!(["lint", "test"])
        );

        let passed = call(&mut server, "test").await;
        let text = passed["content"][0]["text"].as_str().unwrap();
        assert_eq!(passed["isError"], false);
        assert!(
            text.starts_with("check test passed (exit code 0, ")
                && text.contains("; 9 of 10 runs left\n\nall good"),
            "{text}"
        );
        let failed = call(&mut server, "lint").await;
        let text = failed["content"][0]["text"].as_str().unwrap();
        assert_eq!(failed["isError"], true);
        assert!(
            text.starts_with("check lint failed (exit code 3, ") && text.contains("broken"),
            "{text}"
        );
        let unknown = call(&mut server, "deploy").await;
        assert_eq!(
            unknown["content"][0]["text"],
            "there is no check \"deploy\"; this step's checks are: lint, test"
        );
        // Runs are counted: an unknown check isn't one.
        for _ in 2..MAX_RUNS {
            call(&mut server, "test").await;
        }
        let spent = call(&mut server, "test").await;
        assert_eq!(spent["isError"], true);
        assert!(
            spent["content"][0]["text"]
                .as_str()
                .unwrap()
                .starts_with("you ran checks 10 times already"),
            "{spent}"
        );
    }

    #[test]
    fn a_worker_starts_it_with_the_steps_checks_in_the_tasks_workspace() {
        let server = server(
            Path::new("/opt/proofkin"),
            Path::new("/work/tree"),
            &[("test".into(), "cargo test".into())],
        );
        assert_eq!(server.command.as_deref(), Some("/opt/proofkin"));
        assert_eq!(
            server.args,
            [
                "task-checks",
                "--dir",
                "/work/tree",
                "--check",
                "test=cargo test"
            ]
        );
        assert_eq!(server.tools, [TOOL]);
    }
}
