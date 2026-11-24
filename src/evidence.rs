//! Evidence about delegated work that doesn't rest on the model's word: the checks
//! the worker's runtime ran after the model finished (owner-defined commands such
//! as `cargo test`, with their exit codes and output), the git commit it made, or
//! a team member's approval. Every record is signed with the producing agent's
//! identity key, so the requester, and anyone it later shares a run report with,
//! can verify who produced it and that the output wasn't changed.

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow, bail};
use iroh::{EndpointId, SecretKey, Signature};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::llm::Provider;
use crate::profile::INHERITED_CREDENTIAL_VARS;

/// Most model attempts a task may ask for (each failed check run starts another).
pub const MAX_ATTEMPTS: u32 = 5;
/// How long one check may run.
pub const CHECK_TIMEOUT: Duration = Duration::from_secs(900);
/// Characters of a check's output kept (its end, where failures are reported).
const OUTPUT_TAIL: usize = 4_000;
const DOMAIN: &[u8] = b"p2pagents-evidence-v1\n";

pub fn sha256_hex(data: &[u8]) -> String {
    data_encoding::HEXLOWER.encode(&Sha256::digest(data))
}

/// The result of one owner-defined check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckResult {
    pub name: String,
    pub command: String,
    /// None if it timed out or couldn't start.
    pub exit_code: Option<i32>,
    pub passed: bool,
    pub millis: u64,
    /// The end of its combined stdout and stderr.
    pub output: String,
}

/// The commit a task made on its own branch (folders shared with `--worktrees`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitCommit {
    pub branch: String,
    pub base: String,
    /// None if the task changed nothing.
    pub commit: Option<String>,
    pub files: Vec<String>,
    /// SHA-256 of the patch (`git diff base commit --binary`), sent alongside.
    pub patch_sha256: Option<String>,
    /// Build and tool caches the task made that the commit left out (folders end with
    /// `/`), for the task's report. Not part of the record: older peers verify it as before.
    #[serde(skip)]
    pub left_out: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    Task,
    Approval,
}

/// A signed record of what an agent's runtime observed about one task or approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    pub kind: EvidenceKind,
    /// Name the producing agent announces, and its endpoint id (the signing key).
    pub signer: String,
    pub signer_id: String,
    /// Endpoint id of the agent that asked for the work.
    pub requester_id: String,
    pub task_id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<Provider>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub share: Option<String>,
    pub prompt_sha256: String,
    /// Hash of the exact output text returned with this record.
    pub output_sha256: String,
    #[serde(default)]
    pub attempts: u32,
    #[serde(default)]
    pub checks: Vec<CheckResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<GitCommit>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved: Option<bool>,
    pub finished_unix: u64,
    /// For a team step: the lead's team run, with every step's own signed evidence
    /// (absent otherwise, so other records sign as before).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team: Option<Box<crate::runs::RunRecord>>,
    /// Ed25519 signature (hex) over everything above.
    #[serde(default)]
    pub signature: String,
}

impl Evidence {
    pub fn new(
        kind: EvidenceKind,
        signer: &str,
        signer_id: EndpointId,
        requester_id: EndpointId,
        task_id: u64,
    ) -> Self {
        Self {
            kind,
            signer: signer.to_owned(),
            signer_id: signer_id.to_string(),
            requester_id: requester_id.to_string(),
            task_id,
            provider: None,
            share: None,
            prompt_sha256: String::new(),
            output_sha256: String::new(),
            attempts: 0,
            checks: Vec::new(),
            git: None,
            approved: None,
            finished_unix: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            team: None,
            signature: String::new(),
        }
    }

    fn signed_bytes(&self) -> Vec<u8> {
        let mut unsigned = self.clone();
        unsigned.signature.clear();
        let mut bytes = DOMAIN.to_vec();
        bytes.extend(serde_json::to_vec(&unsigned).expect("evidence serializes"));
        bytes
    }

    /// Binds `output` to the record and signs it with `key` (the signer's identity).
    pub fn sign(mut self, output: &str, key: &SecretKey) -> Self {
        self.output_sha256 = sha256_hex(output.as_bytes());
        self.signature.clear();
        let signature = key.sign(&self.signed_bytes());
        self.signature = data_encoding::HEXLOWER.encode(&signature.to_bytes());
        self
    }

    /// Checks that `signer` produced this record for exactly `output`.
    pub fn verify(&self, signer: &EndpointId, output: &str) -> Result<()> {
        if self.signer_id != signer.to_string() {
            bail!("signed by {}, not by {}", self.signer_id, signer);
        }
        if self.output_sha256 != sha256_hex(output.as_bytes()) {
            bail!("the output doesn't match the signed hash");
        }
        self.verify_signature(signer)
    }

    /// Checks the signature alone, with the signer taken from the record: the record
    /// is as its signer made it (without the output it vouches for, which is checked
    /// by [`Evidence::verify`]).
    pub fn verify_record(&self) -> Result<()> {
        let id: EndpointId = self
            .signer_id
            .parse()
            .map_err(|_| anyhow!("malformed signer id"))?;
        self.verify_signature(&id)
    }

    fn verify_signature(&self, signer: &EndpointId) -> Result<()> {
        let bytes: [u8; 64] = data_encoding::HEXLOWER
            .decode(self.signature.as_bytes())
            .ok()
            .and_then(|b| b.try_into().ok())
            .ok_or_else(|| anyhow!("malformed signature"))?;
        signer
            .verify(&self.signed_bytes(), &Signature::from_bytes(&bytes))
            .map_err(|_| anyhow!("invalid signature"))
    }

    /// Like [`Evidence::verify`], with the signer taken from the record itself.
    pub fn verify_self(&self, output: &str) -> Result<()> {
        let id: EndpointId = self
            .signer_id
            .parse()
            .map_err(|_| anyhow!("malformed signer id"))?;
        self.verify(&id, output)
    }

    pub fn checks_passed(&self) -> bool {
        self.checks.iter().all(|c| c.passed)
    }

    /// Markdown summary appended to a task's output.
    pub fn summary(&self) -> String {
        let mut text = String::new();
        if !self.checks.is_empty() {
            text += &format!(
                "**Checks run by {} ({} attempt{}):**\n",
                self.signer,
                self.attempts,
                if self.attempts == 1 { "" } else { "s" }
            );
            for c in &self.checks {
                text += &format!(
                    "- {} `{}`: {} ({}, {:.1}s)\n",
                    if c.passed { "✅" } else { "❌" },
                    c.name,
                    if c.passed { "passed" } else { "FAILED" },
                    c.exit_code
                        .map(|code| format!("exit {code}"))
                        .unwrap_or_else(|| "no exit code".into()),
                    c.millis as f64 / 1000.0
                );
            }
        }
        if let Some(git) = &self.git {
            match &git.commit {
                Some(commit) => {
                    text += &format!(
                        "**Commit** `{}` on branch `{}` ({} file{}): {}\n",
                        commit.chars().take(12).collect::<String>(),
                        git.branch,
                        git.files.len(),
                        if git.files.len() == 1 { "" } else { "s" },
                        code_list(&git.files)
                    )
                }
                None => text += &format!("**No changes** committed (branch `{}`)\n", git.branch),
            }
            if !git.left_out.is_empty() {
                text += &format!(
                    "**Left out** of the commit (build and tool caches the task made): {}\n",
                    code_list(&git.left_out)
                );
            }
        }
        text
    }
}

/// Paths as Markdown code, the first 20 of them, then how many more.
fn code_list(paths: &[String]) -> String {
    const LISTED: usize = 20;
    let mut list: Vec<String> = paths
        .iter()
        .take(LISTED)
        .map(|p| format!("`{}`", p.replace('`', "'")))
        .collect();
    if paths.len() > LISTED {
        list.push(format!("and {} more", paths.len() - LISTED));
    }
    list.join(", ")
}

/// Runs built-in check `name` (see [`crate::spec`]): it only reads files in `dir`,
/// in this process, so it needs no shell and no more than read access.
pub async fn run_built_in(name: &str, dir: &Path) -> CheckResult {
    let started = Instant::now();
    let (check, folder) = (name.to_owned(), dir.to_path_buf());
    let report = tokio::task::spawn_blocking(move || crate::spec::run(&check, &folder))
        .await
        .unwrap_or_else(|e| crate::spec::Report {
            problems: vec![format!("the check failed: {e}")],
            notes: Vec::new(),
        });
    let passed = report.passed();
    CheckResult {
        name: name.to_owned(),
        command: format!("built-in {name}"),
        exit_code: Some(if passed { 0 } else { 1 }),
        passed,
        millis: started.elapsed().as_millis() as u64,
        output: tail(&crate::redact::redact(&report.text()).0, OUTPUT_TAIL),
    }
}

/// Runs `command` (an owner-defined check) in `dir` with the shell, without the
/// inherited credential variables, for at most `timeout`. If it times out or the
/// task is cancelled, the whole process tree is killed.
pub async fn run_check(name: &str, command: &str, dir: &Path, timeout: Duration) -> CheckResult {
    let started = Instant::now();
    let mut cmd = shell(command);
    cmd.current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for var in INHERITED_CREDENTIAL_VARS {
        cmd.env_remove(var);
    }
    let (exit_code, output) = match cmd.spawn() {
        Err(e) => (None, format!("could not start the check: {e}")),
        Ok(mut child) => {
            // Kills the tree unless the check finishes (timeout or cancellation).
            let mut guard = TreeGuard(child.id());
            let (mut stdout, mut stderr) = (child.stdout.take(), child.stderr.take());
            let work = async {
                let (mut out, mut err) = (Vec::new(), Vec::new());
                let read_out = async {
                    if let Some(s) = stdout.as_mut() {
                        let _ = s.read_to_end(&mut out).await;
                    }
                };
                let read_err = async {
                    if let Some(s) = stderr.as_mut() {
                        let _ = s.read_to_end(&mut err).await;
                    }
                };
                let (_, _, status) = tokio::join!(read_out, read_err, child.wait());
                (out, err, status)
            };
            match tokio::time::timeout(timeout, work).await {
                Ok((out, err, Ok(status))) => {
                    guard.0 = None;
                    let mut text = String::from_utf8_lossy(&out).into_owned();
                    text += &String::from_utf8_lossy(&err);
                    (status.code(), text)
                }
                Ok((_, _, Err(e))) => (None, format!("the check failed to run: {e}")),
                Err(_) => (None, format!("timed out after {}s", timeout.as_secs())),
            }
        }
    };
    // Results leave this machine: redact the whole output before keeping its end,
    // so a secret cut in half (e.g. a key block's BEGIN line) can't slip through.
    CheckResult {
        name: name.to_owned(),
        command: crate::redact::redact(command).0,
        exit_code,
        passed: exit_code == Some(0),
        millis: started.elapsed().as_millis() as u64,
        output: tail(&crate::redact::redact(&output).0, OUTPUT_TAIL),
    }
}

/// Kills a check's process and everything it started, when dropped while armed.
struct TreeGuard(Option<u32>);

impl Drop for TreeGuard {
    fn drop(&mut self) {
        let Some(pid) = self.0 else { return };
        #[cfg(windows)]
        {
            let _ = std::process::Command::new("taskkill")
                .args(["/T", "/F", "/PID", &pid.to_string()])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        #[cfg(unix)]
        {
            // The shell leads its own process group (see `shell`): signal exactly that
            // group. Never through an external `kill`, whose handling of a negative
            // id differs between distributions (some signal unrelated processes).
            if let Ok(pgid) = libc::pid_t::try_from(pid)
                && pgid > 1
            {
                // SAFETY: kill(2) with a negative id only signals that process group.
                unsafe {
                    libc::kill(-pgid, libc::SIGKILL);
                }
            }
        }
    }
}

fn shell(command: &str) -> Command {
    #[cfg(windows)]
    {
        let mut cmd = Command::new("cmd");
        cmd.arg("/D")
            .arg("/S")
            .arg("/C")
            .raw_arg(format!("\"{command}\""));
        cmd
    }
    #[cfg(not(windows))]
    {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(command).process_group(0);
        cmd
    }
}

fn tail(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_owned();
    }
    let rest: String = text.chars().skip(count - max).collect();
    format!("[... {} characters cut]\n{rest}", count - max)
}

/// The prompt for another attempt after checks failed: what failed, then the task.
pub fn retry_prompt(task: &str, checks: &[CheckResult]) -> String {
    let mut prompt = String::from(
        "Your previous attempt at this task did not pass the checks the folder's owner runs \
         after every attempt. Fix the cause of these failures (don't weaken or skip the checks), \
         then stop.\n",
    );
    for c in checks.iter().filter(|c| !c.passed) {
        // The output comes from the folder's code, which the model may have written.
        prompt += &format!(
            "\n### Check `{}` (`{}`) {}\n{}\n",
            c.name,
            c.command,
            c.exit_code
                .map(|code| format!("exited with {code}"))
                .unwrap_or_else(|| "did not finish".into()),
            crate::boundary::wrap(&format!("check:{} output", c.name), c.output.trim_end())
        );
    }
    let task = task
        .strip_prefix(crate::boundary::DIRECTIVE)
        .unwrap_or(task);
    crate::boundary::guard(&(prompt + "\n---\nThe task:\n" + task))
}

/// The prompt for another attempt after the model's last run on the task stopped before it
/// finished (`why`: it ran out of time, or its provider failed): go on from the work it left.
pub fn resume_prompt(task: &str, why: &str) -> String {
    let prompt = format!(
        "Your previous attempt at this task stopped before you finished:\n{}\nWhat it changed is \
         still in your folder: check it, finish the task, then stop.\n",
        crate::boundary::wrap("why the run stopped", why.trim())
    );
    let task = task
        .strip_prefix(crate::boundary::DIRECTIVE)
        .unwrap_or(task);
    crate::boundary::guard(&(prompt + "\n---\nThe task:\n" + task))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(key: &SecretKey) -> Evidence {
        let requester = SecretKey::generate().public();
        let mut e = Evidence::new(EvidenceKind::Task, "worker", key.public(), requester, 7);
        e.attempts = 1;
        e.checks.push(CheckResult {
            name: "test".into(),
            command: "cargo test".into(),
            exit_code: Some(0),
            passed: true,
            millis: 10,
            output: "ok".into(),
        });
        e
    }

    #[test]
    fn signed_evidence_verifies_and_detects_tampering() {
        let key = SecretKey::generate();
        let signed = record(&key).sign("all good", &key);
        signed.verify(&key.public(), "all good").unwrap();
        signed.verify_self("all good").unwrap();

        // Through JSON, as it travels and is stored in run reports.
        let json = serde_json::to_string(&signed).unwrap();
        let back: Evidence = serde_json::from_str(&json).unwrap();
        back.verify(&key.public(), "all good").unwrap();

        assert!(
            signed.verify(&key.public(), "all good!").is_err(),
            "output changed"
        );
        let other = SecretKey::generate();
        assert!(
            signed.verify(&other.public(), "all good").is_err(),
            "other signer"
        );
        let mut forged = signed.clone();
        forged.checks[0].passed = false;
        assert!(
            forged.verify(&key.public(), "all good").is_err(),
            "record changed"
        );
        let mut resigned = record(&key).sign("all good", &other);
        resigned.signer_id = key.public().to_string();
        assert!(
            resigned.verify(&key.public(), "all good").is_err(),
            "wrong key"
        );
    }

    #[tokio::test]
    async fn checks_report_exit_codes_and_output() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("marker.txt"), "here").unwrap();
        let (list, fail) = if cfg!(windows) {
            ("type marker.txt", "echo broken & exit 3")
        } else {
            ("cat marker.txt", "echo broken; exit 3")
        };
        let ok = run_check("list", list, dir.path(), CHECK_TIMEOUT).await;
        assert!(ok.passed, "{ok:?}");
        assert_eq!(ok.exit_code, Some(0));
        assert!(ok.output.contains("here"), "runs in the folder: {ok:?}");
        let bad = run_check("bad", fail, dir.path(), CHECK_TIMEOUT).await;
        assert!(!bad.passed);
        assert_eq!(bad.exit_code, Some(3));
        assert!(bad.output.contains("broken"));
        let prompt = retry_prompt("Fix the parser", &[ok, bad]);
        assert!(prompt.contains("### Check `bad`") && prompt.contains("exited with 3"));
        assert!(
            !prompt.contains("### Check `list`"),
            "only failures are reported"
        );
        assert!(prompt.ends_with("The task:\nFix the parser"));

        // A key printed before a lot of other output, and a password in the command.
        let key = format!(
            "-----BEGIN RSA PRIVATE KEY-----\n{}\n-----END RSA PRIVATE KEY-----",
            "MIIEowIBAAKCAQEA".repeat(200)
        );
        std::fs::write(
            dir.path().join("key.txt"),
            format!("{key}\n{}", "noise line\n".repeat(90)),
        )
        .unwrap();
        let show = if cfg!(windows) {
            "type key.txt & rem pass="
        } else {
            "cat key.txt # pass="
        };
        let leak = run_check(
            "leak",
            &format!("{show}x && echo postgres://app:S3cretPw@db/app"),
            dir.path(),
            CHECK_TIMEOUT,
        )
        .await;
        assert!(
            !leak.output.contains("MIIEow"),
            "the key body never survives the tail"
        );
        assert!(
            leak.command.contains("[REDACTED:url-password]") && !leak.command.contains("S3cretPw")
        );
    }

    #[tokio::test]
    async fn checks_time_out() {
        let dir = tempfile::tempdir().unwrap();
        let slow = if cfg!(windows) {
            "ping -n 30 127.0.0.1 >NUL"
        } else {
            "sleep 30"
        };
        let r = run_check("slow", slow, dir.path(), Duration::from_millis(300)).await;
        assert!(!r.passed);
        assert_eq!(r.exit_code, None);
        assert!(r.output.contains("timed out"));
    }

    /// A timed-out check's whole process group dies (checked on Linux), and nothing
    /// else does (everywhere).
    #[tokio::test]
    async fn timed_out_checks_kill_only_their_own_processes() {
        let dir = tempfile::tempdir().unwrap();
        let mut bystander = if cfg!(windows) {
            std::process::Command::new("ping")
                .args(["-n", "30", "127.0.0.1"])
                .stdout(Stdio::null())
                .spawn()
        } else {
            std::process::Command::new("sleep").arg("30").spawn()
        }
        .unwrap();
        let slow = if cfg!(windows) {
            "ping -n 30 127.0.0.1 >NUL"
        } else {
            "sleep 30 & echo $! > bg.pid; wait"
        };
        let r = run_check("slow", slow, dir.path(), Duration::from_millis(500)).await;
        assert!(!r.passed);
        tokio::time::sleep(Duration::from_millis(300)).await;
        #[cfg(target_os = "linux")]
        {
            let bg = std::fs::read_to_string(dir.path().join("bg.pid")).unwrap();
            let alive = std::fs::read_to_string(format!("/proc/{}/stat", bg.trim()))
                .is_ok_and(|stat| stat.split_whitespace().nth(2) != Some("Z"));
            assert!(!alive, "the check's background process was killed");
        }
        assert!(
            bystander.try_wait().unwrap().is_none(),
            "an unrelated process lives on"
        );
        let _ = bystander.kill();
        let _ = bystander.wait();
    }
}
