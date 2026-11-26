use anyhow::Result;
use iroh::EndpointAddr;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncWrite, AsyncWriteExt};

use crate::evidence::Evidence;
use crate::llm::{LlmTask, Provider, Usage};
use crate::shares::ShareInfo;

/// QUIC ALPN identifying this protocol; peers speaking anything else are refused.
pub const ALPN: &[u8] = b"p2pagents/1";

/// Wire messages, framed as one JSON object per line on a QUIC stream. The
/// sender's identity is not in the messages: it is the connection's
/// cryptographically verified endpoint id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
    Hello {
        name: String,
        /// The sender's current addresses, shared with other peers by gossip.
        addr: EndpointAddr,
        /// LLM providers this agent runs tasks with on behalf of its peers.
        #[serde(default)]
        capabilities: Vec<Provider>,
        /// Folders the receiving peer may run tasks in (names and access, no paths).
        #[serde(default)]
        shares: Vec<ShareInfo>,
        /// For a digital worker: the profile that owns it (and whose accounts it uses).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        owner: Option<String>,
        /// What this agent is for, as set by its owner.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        role: Option<String>,
        /// Per provider it serves: the model tasks that name none run on, and the models
        /// tasks may ask for (left out when unset; older peers don't send it).
        #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
        models: crate::models::Models,
        /// Per provider it serves: the models its account offers that tasks may ask for,
        /// with the reasoning efforts each supports (left out when it lists none, as
        /// older peers and providers without a list don't).
        #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
        offers: crate::models::Offers,
        /// For a deployed factory agent: the release it runs, `name@version` (older peers
        /// see its role).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<String>,
        /// The factory agents the sender publishes to the receiver (see
        /// [`crate::factory::catalog`]); changes come in a `catalog` message.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        catalog: Vec<CatalogItem>,
    },
    Peers {
        addrs: Vec<EndpointAddr>,
    },
    Chat {
        from: String,
        text: String,
    },
    TaskRequest {
        task_id: u64,
        task: Box<LlmTask>,
        /// What the task is part of on the requester's side (its run and step), so the
        /// worker can tell whose work it is.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        origin: Option<Origin>,
    },
    TaskResult {
        task_id: u64,
        ok: bool,
        output: String,
        /// What the worker's runtime observed, signed with its key.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        evidence: Option<Box<Evidence>>,
        /// The task's commit as a patch (folders shared with `--worktrees`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        patch: Option<String>,
        /// The task never ran: the worker refused it (or stopped before it started),
        /// so another worker may take it.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        refused: bool,
        /// The model that did the work, as its provider said (older peers don't say).
        /// The worker also names it in the signed output's evidence summary.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        /// What the work used, as its provider reported it: tokens, and premium requests
        /// or cost where it says (none if it reported none; older peers don't say).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
    },
    /// Where a task the receiver delegated is now (sent while it runs, when it moves on).
    TaskProgress {
        task_id: u64,
        progress: Progress,
    },
    /// How busy the sender is with peers' tasks (sent when it changes): tasks running
    /// and waiting for a slot, how many run at once, and how many are the receiver's.
    Load {
        running: u32,
        queued: u32,
        slots: u32,
        #[serde(default)]
        yours: u32,
    },
    /// Asks a team member to approve a plan step; answered with `TaskResult`
    /// (`ok` = approved) and withdrawn with `TaskCancel`.
    ApprovalRequest {
        task_id: u64,
        title: String,
        text: String,
        /// What to look at before answering, by path in the project: the UX
        /// prototype (`prototype/...`), review decks and design documents
        /// (`review/*.md`, `design/*.md`), shown in a sandboxed preview.
        #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
        preview: std::collections::BTreeMap<String, String>,
        /// The same for a UX prototype only, by path under `prototype/` (v0.5.0 peers).
        #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
        prototype: std::collections::BTreeMap<String, String>,
        /// The approver may also ask for changes: the work is done again with their note,
        /// then they are asked again (older approvers only approve or reject).
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        changes: bool,
    },
    /// The requester no longer wants the result of `task_id`; the worker stops it.
    TaskCancel {
        task_id: u64,
    },
    /// The sender's folders the receiver may now use (sent when its grant changes).
    Shares {
        shares: Vec<ShareInfo>,
    },
    /// The sender's models and the ones its account offers, as its hello says them
    /// (sent when they change).
    Models {
        #[serde(default)]
        models: crate::models::Models,
        #[serde(default)]
        offers: crate::models::Offers,
    },
    /// The factory agents the sender publishes to the receiver now, as its hello says them
    /// (sent when they change; empty: none any more).
    Catalog {
        #[serde(default)]
        entries: Vec<CatalogItem>,
    },
    /// Asks for the bundle of a release the receiver publishes to the sender; answered with
    /// `bundle`.
    BundleRequest {
        request_id: u64,
        agent: String,
        version: String,
    },
    /// A release's bundle, or why it isn't sent.
    Bundle {
        request_id: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bundle: Option<Box<Bundle>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// A message type from a newer version; ignored so mixed versions keep working.
    #[serde(other)]
    Unknown,
}

/// A factory release a peer publishes, as its catalog lists it: what it is for, the most
/// access it asks for and how its evaluation went. Never knowledge, outputs or paths.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogItem {
    pub name: String,
    pub version: String,
    pub description: String,
    pub access: crate::llm::Access,
    pub eval_summary: EvalSummary,
}

/// How a published release's evaluation went, as its publisher's report says.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalSummary {
    /// The share of the runs of its own cases that passed.
    pub pass_rate: f64,
    /// Its own cases: how the built-in attacks went is `adversarial_passed`.
    pub cases: usize,
    pub adversarial_passed: bool,
}

// A pass rate is a share of runs, never NaN.
impl Eq for EvalSummary {}

/// A published release's files, by their paths in its folder (see
/// [`crate::factory::release`]), each in base64: its report without its runs' outputs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bundle {
    pub agent: String,
    pub version: String,
    pub files: std::collections::BTreeMap<String, String>,
}

/// What a delegated task is part of on the requester's side.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Origin {
    /// The requester's evidence run (a plan run).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    /// The plan step it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<String>,
    /// Its number in the requester's task list (`#N` in its web UI).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<u64>,
}

impl Origin {
    /// How the worker names it, e.g. "step fix of run 20261121-…" or "task #4".
    pub fn describe(&self) -> Option<String> {
        match (&self.step, &self.run, self.task) {
            (Some(step), Some(run), _) => Some(format!("step {step} of run {run}")),
            (Some(step), None, _) => Some(format!("step {step}")),
            (None, _, Some(task)) => Some(format!("task #{task}")),
            _ => None,
        }
    }
}

/// What a delegated task is doing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Waiting for one of the worker's task slots.
    Queued,
    /// The model is working.
    #[default]
    Running,
    /// The runtime is running the task's checks.
    Checking,
    /// The runtime is committing the work.
    Committing,
    /// A lead's agent is running its team plan for it.
    Team,
    /// The folder's agent opened the task's pull request and waits for it to be merged.
    PullRequest,
    /// The connection to the worker dropped; waiting for it to come back.
    Reconnecting,
    /// A phase from a newer version.
    Other,
}

impl<'de> Deserialize<'de> for Phase {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(match String::deserialize(d)?.as_str() {
            "queued" => Phase::Queued,
            "running" => Phase::Running,
            "checking" => Phase::Checking,
            "committing" => Phase::Committing,
            "team" => Phase::Team,
            "pull_request" => Phase::PullRequest,
            "reconnecting" => Phase::Reconnecting,
            _ => Phase::Other,
        })
    }
}

/// Where a delegated task is now, as the runtime running it reports.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Progress {
    #[serde(default)]
    pub phase: Phase,
    /// In words, for people, e.g. "waiting for a free slot (1 task ahead)".
    pub text: String,
    /// The model attempt it is on, and how many it may make.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempts: Option<u32>,
    /// For a team step: the lead's team run, and where each of its steps is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<SubStep>,
}

impl Progress {
    pub fn new(phase: Phase, text: impl Into<String>) -> Self {
        Self {
            phase,
            text: text.into(),
            ..Self::default()
        }
    }
}

/// A step of a lead's team run, as the lead reports it upward.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubStep {
    pub id: String,
    /// Who runs it: the peer, or `#squad` until a member is picked.
    pub target: String,
    /// waiting, running, done, failed or skipped.
    pub status: String,
    /// What it is doing now, or why it failed or was skipped.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub after: Vec<String>,
    /// Its result is signed by the peer that did it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub signed: bool,
    /// A team step's own team run, a level further down.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<SubStep>,
}

pub async fn write_message<W: AsyncWrite + Unpin>(writer: &mut W, msg: &Message) -> Result<()> {
    let mut line = serde_json::to_vec(msg)?;
    line.push(b'\n');
    writer.write_all(&line).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uses_tagged_json() {
        let msg = Message::Chat {
            from: "a".into(),
            text: "hi".into(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(json, r#"{"type":"chat","from":"a","text":"hi"}"#);
        assert_eq!(serde_json::from_str::<Message>(&json).unwrap(), msg);
    }

    #[test]
    fn task_request_round_trips() {
        let msg = Message::TaskRequest {
            task_id: 7,
            task: Box::new(LlmTask::new(Provider::Codex, "fix it")),
            origin: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(
            json,
            r#"{"type":"task_request","task_id":7,"task":{"provider":"codex","prompt":"fix it"}}"#
        );
        assert_eq!(serde_json::from_str::<Message>(&json).unwrap(), msg);
    }

    #[test]
    fn task_cancel_round_trips_and_unknown_messages_are_ignored() {
        let msg = Message::TaskCancel { task_id: 3 };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(json, r#"{"type":"task_cancel","task_id":3}"#);
        assert_eq!(serde_json::from_str::<Message>(&json).unwrap(), msg);
        let future = r#"{"type":"task_hologram","task_id":3,"text":"half way"}"#;
        assert_eq!(
            serde_json::from_str::<Message>(future).unwrap(),
            Message::Unknown
        );
    }

    /// Progress, load, origins and models are new: older peers skip them (an unknown
    /// message type, unknown fields), and phases from newer peers still read.
    #[test]
    fn progress_load_and_origins_keep_older_peers_working() {
        let msg = Message::TaskProgress {
            task_id: 3,
            progress: Progress {
                phase: Phase::Team,
                text: "team run: 1 of 2 steps done".into(),
                run: Some("r1".into()),
                steps: vec![SubStep {
                    id: "dev".into(),
                    target: "alice-dev".into(),
                    status: "done".into(),
                    signed: true,
                    ..SubStep::default()
                }],
                ..Progress::default()
            },
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(serde_json::from_str::<Message>(&json).unwrap(), msg);
        let newer =
            r#"{"type":"task_progress","task_id":3,"progress":{"phase":"teleporting","text":"x"}}"#;
        let Message::TaskProgress { progress, .. } = serde_json::from_str(newer).unwrap() else {
            panic!("a progress message");
        };
        assert_eq!(progress.phase, Phase::Other);
        let request = r#"{"type":"task_request","task_id":1,"task":{"provider":"copilot","prompt":"p"},"origin":{"run":"r","step":"a"}}"#;
        let Message::TaskRequest { origin, .. } = serde_json::from_str(request).unwrap() else {
            panic!("a request");
        };
        assert_eq!(
            origin.and_then(|o| o.describe()).as_deref(),
            Some("step a of run r")
        );
        // What an older peer sends still reads (no origin, not refused, no `yours`, no model).
        let old = r#"{"type":"task_result","task_id":1,"ok":false,"output":"no"}"#;
        assert!(matches!(
            serde_json::from_str::<Message>(old).unwrap(),
            Message::TaskResult {
                refused: false,
                model: None,
                usage: None,
                ..
            }
        ));
        let with_model = Message::TaskResult {
            task_id: 2,
            ok: true,
            output: "done".into(),
            evidence: None,
            patch: None,
            refused: false,
            model: Some("claude-sonnet-5.5".into()),
            usage: Some(Usage {
                input_tokens: Some(1200),
                output_tokens: Some(80),
                cost_usd: Some(0.0123),
                ..Usage::default()
            }),
        };
        let json = serde_json::to_string(&with_model).unwrap();
        assert!(json.contains(r#""model":"claude-sonnet-5.5""#), "{json}");
        assert!(
            json.contains(r#""usage":{"input_tokens":1200,"output_tokens":80,"cost_usd":0.0123}"#),
            "{json}"
        );
        assert_eq!(serde_json::from_str::<Message>(&json).unwrap(), with_model);
        let load = r#"{"type":"load","running":2,"queued":1,"slots":2}"#;
        assert_eq!(
            serde_json::from_str::<Message>(load).unwrap(),
            Message::Load {
                running: 2,
                queued: 1,
                slots: 2,
                yours: 0
            }
        );
    }

    #[test]
    fn hello_with_addresses_round_trips() {
        let id = iroh::SecretKey::generate().public();
        let addr = EndpointAddr::new(id).with_ip_addr("127.0.0.1:4000".parse().unwrap());
        let mut models = crate::models::Models::new();
        models.insert(
            Provider::Claude,
            crate::models::ModelChoice {
                default: Some("claude-sonnet-5.5".into()),
                allowed: vec!["claude-sonnet-5.5".into(), "claude-opus-5.5".into()],
                effort: Some("max".into()),
                efforts: vec!["high".into(), "max".into()],
            },
        );
        let offers = crate::models::Offers::from([(
            Provider::Claude,
            vec![crate::models::Offered {
                id: "claude-opus-5.5".into(),
                name: "Claude Opus 5.5".into(),
                efforts: vec!["high".into(), "max".into()],
                ..Default::default()
            }],
        )]);
        let msg = Message::Hello {
            name: "a".into(),
            addr,
            capabilities: vec![Provider::Claude],
            shares: Vec::new(),
            owner: Some("alice".into()),
            role: None,
            models: models.clone(),
            offers: offers.clone(),
            agent: Some("fin-analyst@1.2.0".into()),
            catalog: Vec::new(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(serde_json::from_str::<Message>(&json).unwrap(), msg);
        let said = r#","offers":{"claude":[{"id":"claude-opus-5.5","name":"Claude Opus 5.5","efforts":["high","max"]}]}"#;
        assert!(json.contains(said), "{json}");
        // An older peer's hello has no models or offers: it reads as saying nothing.
        let older = json.replace(said, "").replace(
            r#","models":{"claude":{"default":"claude-sonnet-5.5","allowed":["claude-sonnet-5.5","claude-opus-5.5"],"effort":"max","efforts":["high","max"]}}"#,
            "",
        );
        assert!(
            !older.contains("models") && !older.contains("offers"),
            "{older}"
        );
        let Message::Hello {
            models: none,
            offers: nothing,
            ..
        } = serde_json::from_str(&older).unwrap()
        else {
            panic!("a hello");
        };
        assert!(none.is_empty() && nothing.is_empty());
        // One older than efforts reads as no default effort, and any allowed.
        let no_effort = json.replace(r#","effort":"max","efforts":["high","max"]"#, "");
        assert_ne!(no_effort, json);
        let Message::Hello { models: old, .. } = serde_json::from_str(&no_effort).unwrap() else {
            panic!("a hello");
        };
        assert_eq!(old[&Provider::Claude].effort, None);
        assert!(old[&Provider::Claude].efforts.is_empty());
        // Changes come in a `models` message (older peers skip it as an unknown type).
        let changed = Message::Models { models, offers };
        let json = serde_json::to_string(&changed).unwrap();
        assert!(
            json.starts_with(r#"{"type":"models","models":{"claude":"#),
            "{json}"
        );
        assert_eq!(serde_json::from_str::<Message>(&json).unwrap(), changed);
        assert_eq!(
            serde_json::from_str::<Message>(r#"{"type":"models"}"#).unwrap(),
            Message::Models {
                models: Default::default(),
                offers: Default::default()
            }
        );
    }
}
