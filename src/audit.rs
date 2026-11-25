//! Audit log of tasks peers asked this agent to run: one JSON object per line in the
//! profile's `audit.jsonl`. Records who asked, which provider, the access granted,
//! the outcome and how long it took. The prompt itself is not stored, only its
//! SHA-256 and length, so the log can be kept without holding peers' prompts.

use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use iroh::EndpointId;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::llm::{Access, LlmTask, Provider, Usage};

const MAX_ERROR_CHARS: usize = 300;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Ran and returned a result.
    Done,
    /// Ran and failed.
    Failed,
    /// Stopped because the requesting peer cancelled it or disconnected.
    Cancelled,
    /// Not run: not allowed to delegate, over the hourly limit, or provider not served.
    Refused,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEntry {
    /// Unix time in milliseconds when the request arrived.
    pub ts_ms: u64,
    pub peer: String,
    pub peer_id: String,
    pub task_id: u64,
    pub provider: Provider,
    /// Shared folder the task asked for.
    #[serde(default)]
    pub share: String,
    /// Access the task ran with (`None` when refused).
    pub access: Option<Access>,
    pub outcome: Outcome,
    pub duration_ms: u64,
    pub prompt_sha256: String,
    pub prompt_chars: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// What it was part of on the peer's side, e.g. "step fix of run 20261121-…".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    /// A team step: it ran the folder's team plan on this agent's own peers rather
    /// than a model of `provider`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub team: bool,
    /// The model that did the work, as its provider said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// What the work used, as its provider reported it (none if it reported none).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

impl AuditEntry {
    /// `started` is when the request arrived; the duration is measured up to now.
    pub fn new(
        peer: &str,
        peer_id: &EndpointId,
        task_id: u64,
        task: &LlmTask,
        access: Option<Access>,
        result: &anyhow::Result<String>,
        started: SystemTime,
    ) -> Self {
        let duration = started.elapsed().unwrap_or_default();
        let outcome = match (access, result) {
            (None, _) => Outcome::Refused,
            (Some(_), Ok(_)) => Outcome::Done,
            (Some(_), Err(_)) => Outcome::Failed,
        };
        let error = result.as_ref().err().map(|e| {
            let text = format!("{e:#}");
            text.chars().take(MAX_ERROR_CHARS).collect()
        });
        Self {
            ts_ms: started
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_millis() as u64),
            peer: peer.to_owned(),
            peer_id: peer_id.to_string(),
            task_id,
            provider: task.provider,
            share: task
                .share
                .clone()
                .unwrap_or_else(|| crate::shares::DATA_SHARE.to_owned()),
            access,
            outcome,
            duration_ms: duration.as_millis() as u64,
            prompt_sha256: data_encoding::HEXLOWER.encode(&Sha256::digest(task.prompt.as_bytes())),
            prompt_chars: task.prompt.chars().count(),
            error,
            origin: None,
            team: task.team.is_some(),
            model: None,
            usage: None,
        }
    }
}

/// What one peer's tasks on one provider used, summed from audit entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageTotal {
    pub peer: String,
    pub provider: Provider,
    /// How many of its tasks reported usage.
    pub tasks: usize,
    pub usage: Usage,
}

/// The usage of `entries` summed per peer and provider, by peer then provider. Tasks
/// that reported none count in none.
pub fn usage_totals(entries: &[AuditEntry]) -> Vec<UsageTotal> {
    let mut totals: BTreeMap<(&str, Provider), (usize, Usage)> = BTreeMap::new();
    for entry in entries {
        if let Some(usage) = &entry.usage {
            let total = totals.entry((&entry.peer, entry.provider)).or_default();
            total.0 += 1;
            total.1 = total.1.plus(usage);
        }
    }
    totals
        .into_iter()
        .map(|((peer, provider), (tasks, usage))| UsageTotal {
            peer: peer.to_owned(),
            provider,
            tasks,
            usage,
        })
        .collect()
}

/// Append-only audit log file.
pub struct AuditLog {
    path: PathBuf,
    lock: Mutex<()>,
}

impl AuditLog {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            lock: Mutex::new(()),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Appends `entry`; failures are reported but never fail the task.
    pub fn record(&self, entry: &AuditEntry) {
        let _guard = self.lock.lock().unwrap();
        let result = (|| -> std::io::Result<()> {
            if let Some(dir) = self.path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)?;
            let mut line = serde_json::to_vec(entry)?;
            line.push(b'\n');
            file.write_all(&line)
        })();
        if let Err(e) = result {
            eprintln!(
                "warning: could not write audit log {}: {e}",
                self.path.display()
            );
        }
    }

    /// The last `limit` entries, oldest first. Unreadable lines are skipped.
    pub fn read(path: &Path, limit: usize) -> std::io::Result<Vec<AuditEntry>> {
        let file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let mut entries: Vec<AuditEntry> = std::io::BufReader::new(file)
            .lines()
            .map_while(Result::ok)
            .filter_map(|line| serde_json::from_str(&line).ok())
            .collect();
        let skip = entries.len().saturating_sub(limit);
        Ok(entries.split_off(skip))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_outcomes_without_the_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let log = AuditLog::new(dir.path().join("audit.jsonl"));
        let id = iroh::SecretKey::generate().public();
        let task = LlmTask::new(Provider::Copilot, "delete everything secret-xyz");
        let now = SystemTime::now();
        let ok: anyhow::Result<String> = Ok("out".into());
        let err: anyhow::Result<String> = Err(anyhow::anyhow!("model error"));
        log.record(&AuditEntry::new(
            "bob",
            &id,
            1,
            &task,
            Some(Access::Full),
            &ok,
            now,
        ));
        log.record(&AuditEntry::new(
            "bob",
            &id,
            2,
            &task,
            Some(Access::ReadOnly),
            &err,
            now,
        ));
        log.record(&AuditEntry::new("bob", &id, 3, &task, None, &err, now));

        let text = std::fs::read_to_string(log.path()).unwrap();
        assert!(!text.contains("secret-xyz"), "prompts must not be stored");
        let entries = AuditLog::read(log.path(), 10).unwrap();
        let outcomes: Vec<Outcome> = entries.iter().map(|e| e.outcome).collect();
        assert_eq!(outcomes, [Outcome::Done, Outcome::Failed, Outcome::Refused]);
        assert_eq!(entries[0].access, Some(Access::Full));
        assert_eq!(entries[0].prompt_chars, 28);
        assert_eq!(entries[0].prompt_sha256.len(), 64);
        assert_eq!(entries[1].error.as_deref(), Some("model error"));
        assert_eq!(AuditLog::read(log.path(), 1).unwrap()[0].task_id, 3);
        assert!(
            AuditLog::read(&dir.path().join("none"), 5)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn usage_is_recorded_without_text_and_summed_per_peer_and_provider() {
        let dir = tempfile::tempdir().unwrap();
        let log = AuditLog::new(dir.path().join("audit.jsonl"));
        let id = iroh::SecretKey::generate().public();
        let ok: anyhow::Result<String> = Ok("the secret answer".into());
        let used = |input, premium| Usage {
            input_tokens: Some(input),
            output_tokens: Some(10),
            premium_requests: premium,
            ..Usage::default()
        };
        let record = |peer: &str, provider, usage: Option<Usage>| {
            let task = LlmTask::new(provider, "summarize secret-xyz");
            let mut entry = AuditEntry::new(
                peer,
                &id,
                1,
                &task,
                Some(Access::ReadOnly),
                &ok,
                SystemTime::now(),
            );
            entry.usage = usage;
            log.record(&entry);
        };
        record("bob", Provider::Copilot, Some(used(100, Some(1.0))));
        record("bob", Provider::Copilot, Some(used(50, Some(0.33))));
        record("bob", Provider::Codex, Some(used(7, None)));
        record("bob", Provider::Copilot, None);
        record("amy", Provider::Copilot, Some(used(1, Some(1.0))));

        let text = std::fs::read_to_string(log.path()).unwrap();
        assert!(!text.contains("secret"), "no prompt or output text: {text}");
        assert!(
            text.contains(
                r#""usage":{"input_tokens":100,"output_tokens":10,"premium_requests":1.0}"#
            )
        );
        let entries = AuditLog::read(log.path(), 10).unwrap();
        assert_eq!(
            entries[3].usage, None,
            "none reported: none recorded, not zero"
        );
        let totals = usage_totals(&entries);
        let summed: Vec<(&str, Provider, usize, Option<u64>)> = totals
            .iter()
            .map(|t| (t.peer.as_str(), t.provider, t.tasks, t.usage.input_tokens))
            .collect();
        assert_eq!(
            summed,
            [
                ("amy", Provider::Copilot, 1, Some(1)),
                ("bob", Provider::Copilot, 2, Some(150)),
                ("bob", Provider::Codex, 1, Some(7)),
            ]
        );
        assert_eq!(totals[1].usage.premium_requests, Some(1.33));
        assert_eq!(totals[2].usage.premium_requests, None);
    }
}
