//! LLM agent backends: GitHub Copilot (official Rust SDK), Claude Agent SDK and
//! OpenAI Codex SDK (official TypeScript SDKs, reached through the Node bridge).

pub mod bridge;
pub mod changes;
pub mod copilot;
pub mod policy;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Result, bail};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

pub use bridge::Bridge;
pub use copilot::CopilotBackend;

use crate::auth::ProviderTools;
use crate::mcp::McpServer;
use crate::profile::Profile;

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    clap::ValueEnum,
)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    Copilot,
    Claude,
    Codex,
}

impl std::fmt::Display for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.pad(match self {
            Provider::Copilot => "copilot",
            Provider::Claude => "claude",
            Provider::Codex => "codex",
        })
    }
}

/// What the executing agent lets an LLM do. Always decided by the agent that runs
/// the task (its `--access` ceiling and each peer's grant), never by the requester.
/// File access is confined to the workspace and credential files are refused at
/// every level (see [`policy`]).
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Default,
    Serialize,
    Deserialize,
    clap::ValueEnum,
)]
#[serde(rename_all = "kebab-case")]
pub enum Access {
    /// Read and search files in the workspace.
    #[default]
    ReadOnly,
    /// Also create and edit files in the workspace (no shell commands).
    WorkspaceWrite,
    /// Also run shell commands, inside each provider's OS sandbox (writes only in the
    /// workspace, no network, no bypass).
    Full,
}

impl std::fmt::Display for Access {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.pad(match self {
            Access::ReadOnly => "read-only",
            Access::WorkspaceWrite => "workspace-write",
            Access::Full => "full",
        })
    }
}

/// A unit of LLM work, local or delegated to a peer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlmTask {
    pub provider: Provider,
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The reasoning effort it works at (one of [`crate::models::EFFORTS`]; default: the
    /// worker's for the provider, else the model's own). A model that supports fewer
    /// takes the nearest it supports below it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// For delegated tasks: the peer's shared folder to run in (default: its `data` share).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub share: Option<String>,
    /// Names of the folder's owner-defined checks the worker's runtime runs after the
    /// model, as evidence (see [`crate::evidence`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<String>,
    /// How many model attempts to make while checks fail (default 1, at most
    /// [`crate::evidence::MAX_ATTEMPTS`]); each retry gets the failures.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempts: Option<u32>,
    /// A patch (e.g. from another peer's step) applied in the task's own worktree
    /// before the model starts, so the work can be checked independently.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patch: Option<String>,
    /// Names of the folder's owner-defined MCP servers the model may use.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp: Vec<String>,
    /// A team step: the worker (a lead) runs its folder's team plan for it instead of
    /// a model (see [`crate::team`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team: Option<crate::team::TeamRequest>,
    /// The task's own first line, cut at a sentence or a word (see [`title_of`], at most
    /// 100 characters); the worker uses it as the subject of its commit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The subject of its commit when that isn't its title: the title of the issue a plan
    /// step works on (`issue=#N`, see [`crate::tracker`]). The worker then puts the title
    /// under it, in the commit's body. Older workers ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    /// Work context the requester put in the prompt (Microsoft 365 through Work IQ): where
    /// it is and what it held. The worker records its hash in the signed evidence, never its
    /// content (see [`crate::evidence::ContextUse`]). Older workers ignore it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context: Vec<crate::evidence::ContextRef>,
    /// The Microsoft 365 labels of what the task holds, as its requester's plan marks it
    /// (`unlabeled` for such content without a label, `unknown` for content whose labels
    /// aren't known): marked topics, `@workiq` answers and results that held such content.
    /// The worker's signed evidence repeats them, and its memory marks what it learns from
    /// the task with them. Older workers ignore it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub m365: Vec<String>,
    /// Issues the work is for (`owner/repo#N`, see [`crate::tracker`]): the worker's
    /// commit references them, and a pull request from its branch closes them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub issues: Vec<String>,
    /// A pull request step: the worker opens the pull request from the branch an earlier
    /// step's task committed to, and follows it until it is merged or closed, instead of
    /// running a model (see [`crate::pulls`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pull_request: Option<Box<crate::pulls::PullRequestAsk>>,
}

/// A task's title: the first line of `text`, trimmed. It ends at the first sentence end
/// (`.`, `?` or `!` followed by a space) that comes after at least 20 and within the first
/// 100 characters, without the punctuation. Otherwise a line over 100 characters is cut at
/// its last space within 99 characters (at the 99th character if that space is before the
/// 50th) and gets an ellipsis. Shorter lines stay as they are.
pub fn title_of(text: &str) -> String {
    let chars: Vec<char> = text
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .chars()
        .collect();
    let ends_sentence = |i: usize| matches!(chars[i], '.' | '?' | '!') && chars[i + 1] == ' ';
    let last = chars.len().saturating_sub(1).min(100);
    if let Some(end) = (20..last).find(|&i| ends_sentence(i)) {
        return chars[..end]
            .iter()
            .collect::<String>()
            .trim_end()
            .to_string();
    }
    if chars.len() <= 100 {
        return chars.into_iter().collect();
    }
    let cut = match chars[..99].iter().rposition(|&c| c == ' ') {
        Some(space) if space >= 49 => space,
        _ => 99,
    };
    let mut title: String = chars[..cut]
        .iter()
        .collect::<String>()
        .trim_end()
        .to_string();
    title.push('…');
    title
}

impl LlmTask {
    pub fn new(provider: Provider, prompt: impl Into<String>) -> Self {
        Self {
            provider,
            prompt: prompt.into(),
            model: None,
            effort: None,
            share: None,
            checks: Vec::new(),
            attempts: None,
            patch: None,
            mcp: Vec::new(),
            team: None,
            title: None,
            subject: None,
            context: Vec::new(),
            m365: Vec::new(),
            issues: Vec::new(),
            pull_request: None,
        }
    }

    /// Whether a model does the task: not a team step's (its lead's team does it) nor a
    /// pull request step's (the worker opens the pull request).
    pub fn runs_model(&self) -> bool {
        self.team.is_none() && self.pull_request.is_none()
    }

    pub fn in_share(mut self, share: impl Into<String>) -> Self {
        self.share = Some(share.into());
        self
    }

    pub fn with_checks(mut self, checks: Vec<String>, attempts: Option<u32>) -> Self {
        self.checks = checks;
        self.attempts = attempts;
        self
    }
}

/// Where and with what access a task runs. Decided by the agent that runs it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TaskContext {
    pub access: Access,
    /// Folder the task works in: a shared folder for peers' tasks, `--workspace` for local ones.
    pub workspace: PathBuf,
    /// The MCP servers the task may use, already checked against its folder and access.
    pub mcp: BTreeMap<String, McpServer>,
}

impl TaskContext {
    pub fn new(access: Access, workspace: impl Into<PathBuf>) -> Self {
        Self {
            access,
            workspace: workspace.into(),
            mcp: BTreeMap::new(),
        }
    }
}

/// What a model run gave back: its answer, the model that gave it, if the provider
/// says (several, comma-separated, if the run used more than one), the reasoning
/// effort it worked at, if known, and what it used, if the provider reports it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ran {
    pub output: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub usage: Option<Usage>,
}

impl Ran {
    pub fn new(output: impl Into<String>, model: Option<String>) -> Self {
        Self {
            output: output.into(),
            model: model.map(|m| clean_model(&m)).filter(|m| !m.is_empty()),
            effort: None,
            usage: None,
        }
    }

    /// The run, saying it worked at `effort` (dropped if it isn't a reasoning effort).
    pub fn with_effort(mut self, effort: Option<String>) -> Self {
        self.effort = effort
            .map(|e| e.trim().to_ascii_lowercase())
            .filter(|e| crate::models::check_effort(e).is_ok());
        self
    }

    /// The run, saying what it used (none if that says nothing; see [`Usage::checked`]).
    pub fn with_usage(mut self, usage: Option<Usage>) -> Self {
        self.usage = usage.and_then(Usage::checked);
        self
    }

    /// The model as results name it: with the effort it worked at (see
    /// [`crate::models::with_effort`]).
    pub fn shown_model(&self) -> Option<String> {
        crate::models::with_effort(self.model.clone(), self.effort.as_deref())
    }
}

/// A model name as a provider or a peer gives it, safe to show anywhere (the web UI,
/// Markdown, a terminal): only the characters model names use (letters, digits,
/// `-._:/@+()`, and spaces and commas between several), at most 100 of them.
pub fn clean_model(model: &str) -> String {
    model
        .chars()
        .filter(|c| c.is_alphanumeric() || " -._:/@+,()".contains(*c))
        .take(100)
        .collect::<String>()
        .trim()
        .to_owned()
}

impl From<String> for Ran {
    fn from(output: String) -> Self {
        Self::new(output, None)
    }
}

/// What model runs used, as their provider reported it: tokens, and the cost where the
/// provider says. What a provider doesn't report is left out, never zero, and a run
/// whose provider reports nothing has no usage at all. It never holds any text.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    /// Tokens the model read, those from its prompt cache included.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    /// Of those, the ones read from its prompt cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input_tokens: Option<u64>,
    /// Tokens it wrote, its reasoning included.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    /// GitHub Copilot premium requests, with the model's multiplier applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub premium_requests: Option<f64>,
    /// What the provider estimates it cost, in US dollars (the Claude Agent SDK does).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
}

// Its amounts are numbers: JSON has no NaN, and `from_json` keeps only finite ones.
impl Eq for Usage {}

impl Usage {
    /// Whether it says nothing.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// `self` and `other` together: each figure summed where either reports it.
    pub fn plus(&self, other: &Usage) -> Usage {
        fn tokens(a: Option<u64>, b: Option<u64>) -> Option<u64> {
            match (a, b) {
                (Some(a), Some(b)) => Some(a.saturating_add(b)),
                (a, b) => a.or(b),
            }
        }
        fn amount(a: Option<f64>, b: Option<f64>) -> Option<f64> {
            match (a, b) {
                (Some(a), Some(b)) => Some(a + b),
                (a, b) => a.or(b),
            }
        }
        Usage {
            input_tokens: tokens(self.input_tokens, other.input_tokens),
            cached_input_tokens: tokens(self.cached_input_tokens, other.cached_input_tokens),
            output_tokens: tokens(self.output_tokens, other.output_tokens),
            premium_requests: amount(self.premium_requests, other.premium_requests),
            cost_usd: amount(self.cost_usd, other.cost_usd),
        }
    }

    /// Runs' usage together; none if none of them reported any.
    pub fn sum<'a>(usages: impl IntoIterator<Item = &'a Usage>) -> Option<Usage> {
        usages
            .into_iter()
            .fold(None, |total: Option<Usage>, u| {
                Some(total.map_or_else(|| u.clone(), |t| t.plus(u)))
            })
            .filter(|u| !u.is_empty())
    }

    /// Usage in JSON as the bridge reports it (the fields above), keeping only figures
    /// that are (see [`Usage::checked`]). None if no figure is.
    pub fn from_json(value: &serde_json::Value) -> Option<Usage> {
        let tokens = |key: &str| value.get(key).and_then(serde_json::Value::as_u64);
        let amount = |key: &str| value.get(key).and_then(serde_json::Value::as_f64);
        Usage {
            input_tokens: tokens("input_tokens"),
            cached_input_tokens: tokens("cached_input_tokens"),
            output_tokens: tokens("output_tokens"),
            premium_requests: amount("premium_requests"),
            cost_usd: amount("cost_usd"),
        }
        .checked()
    }

    /// The usage with only amounts that are finite and not negative (token counts are
    /// whole numbers already); none if nothing is left. For what peers and SDKs say.
    pub fn checked(mut self) -> Option<Usage> {
        for amount in [&mut self.premium_requests, &mut self.cost_usd] {
            if amount.is_some_and(|n| !n.is_finite() || n < 0.0) {
                *amount = None;
            }
        }
        Some(self).filter(|u| !u.is_empty())
    }

    /// For people, with only what it reports: "12,345 tokens in (10,000 cached),
    /// 678 tokens out, 1 premium request, $0.0123".
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        match (self.input_tokens, self.cached_input_tokens) {
            (Some(n), Some(c)) => parts.push(format!(
                "{} tokens in ({} cached)",
                thousands(n),
                thousands(c)
            )),
            (Some(n), None) => parts.push(format!("{} tokens in", thousands(n))),
            (None, Some(c)) => parts.push(format!("{} cached tokens in", thousands(c))),
            (None, None) => {}
        }
        if let Some(n) = self.output_tokens {
            parts.push(format!("{} tokens out", thousands(n)));
        }
        if let Some(n) = self.premium_requests {
            let shown = format!("{n:.2}");
            let shown = shown.trim_end_matches('0').trim_end_matches('.');
            let s = if shown == "1" { "" } else { "s" };
            parts.push(format!("{shown} premium request{s}"));
        }
        if let Some(usd) = self.cost_usd {
            parts.push(if usd < 1.0 {
                format!("${usd:.4}")
            } else {
                format!("${usd:.2}")
            });
        }
        parts.join(", ")
    }
}

/// `n` with its thousands separated by commas.
fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Runs [`LlmTask`]s. Implemented by [`SdkExecutor`]; tests can supply fakes.
#[async_trait]
pub trait Executor: Send + Sync {
    fn providers(&self) -> Vec<Provider>;
    async fn execute(&self, task: &LlmTask, ctx: &TaskContext) -> Result<Ran>;
    /// Runs tasks with these models from now on (see [`crate::models`]).
    fn set_models(&self, _models: crate::models::Models) {}
    /// The models `provider` offers this agent's account, if it lists them.
    async fn list_models(&self, _provider: Provider) -> Result<Vec<crate::models::Offered>> {
        Ok(Vec::new())
    }
}

pub struct ExecutorConfig {
    pub providers: Vec<Provider>,
    /// Path to `bridge/index.mjs`; needed for Claude and Codex.
    pub bridge_script: PathBuf,
    /// Identity whose private provider homes and credentials this agent uses.
    pub profile: Profile,
}

/// Executes tasks with the official Copilot, Claude Agent and Codex SDKs, using
/// provider instances private to this agent's profile. Each task brings its own
/// folder and access ([`TaskContext`]).
pub struct SdkExecutor {
    providers: Vec<Provider>,
    /// The profile's role, given to the model with every task.
    role: Option<String>,
    /// The profile's models per provider: the default and the ones tasks may ask for
    /// (changed while it runs: see [`Executor::set_models`]).
    models: std::sync::RwLock<crate::models::Models>,
    copilot: Option<CopilotBackend>,
    bridge: Option<Bridge>,
    accounts: Vec<(Provider, String)>,
}

impl SdkExecutor {
    /// Starts this agent's own provider instances and verifies each is signed in;
    /// fails (with the `auth login` command to run) instead of using another account.
    pub async fn start(config: ExecutorConfig) -> Result<Arc<Self>> {
        let mut providers = config.providers;
        providers.sort();
        providers.dedup();
        let profile = &config.profile;
        let tools = ProviderTools::new(&config.bridge_script);
        let mut accounts = Vec::new();

        let copilot = if providers.contains(&Provider::Copilot) {
            let backend = CopilotBackend::start(profile.copilot_credentials()?).await?;
            accounts.push((Provider::Copilot, backend.account().await?));
            Some(backend)
        } else {
            None
        };
        let bridged: Vec<Provider> = providers
            .iter()
            .copied()
            .filter(|p| matches!(p, Provider::Claude | Provider::Codex))
            .collect();
        let bridge = if bridged.is_empty() {
            None
        } else {
            for &provider in &bridged {
                accounts.push((
                    provider,
                    tools.check(&profile.account_profile()?, provider).await?,
                ));
            }
            Some(Bridge::spawn_node(
                &config.bridge_script,
                profile.bridge_env(&bridged)?,
            )?)
        };
        Ok(Arc::new(Self {
            providers,
            role: profile.config.role.clone(),
            models: std::sync::RwLock::new(profile.models()),
            copilot,
            bridge,
            accounts,
        }))
    }

    /// Account each enabled provider is signed in as.
    pub fn accounts(&self) -> &[(Provider, String)] {
        &self.accounts
    }

    /// The models this agent's Copilot account may use (none without Copilot).
    pub async fn copilot_models(&self) -> Result<Vec<crate::models::Offered>> {
        match &self.copilot {
            Some(copilot) => copilot.list_models().await,
            None => Ok(Vec::new()),
        }
    }

    /// Stops its Copilot CLI instance, for a command that started one only for a task
    /// (the Node bridge stops when the executor is dropped).
    pub async fn stop(&self) {
        if let Some(copilot) = &self.copilot {
            copilot.stop().await;
        }
    }
}

#[async_trait]
impl Executor for SdkExecutor {
    fn providers(&self) -> Vec<Provider> {
        self.providers.clone()
    }

    fn set_models(&self, models: crate::models::Models) {
        *self.models.write().unwrap() = models;
    }

    async fn list_models(&self, provider: Provider) -> Result<Vec<crate::models::Offered>> {
        match provider {
            Provider::Copilot => self.copilot_models().await,
            // Their SDKs take any model name they know: there is no list.
            Provider::Claude | Provider::Codex => Ok(Vec::new()),
        }
    }

    async fn execute(&self, task: &LlmTask, ctx: &TaskContext) -> Result<Ran> {
        if !self.providers.contains(&task.provider) {
            bail!("provider {} is not enabled on this agent", task.provider);
        }
        if !ctx.workspace.is_dir() {
            bail!("folder {} does not exist", ctx.workspace.display());
        }
        if !ctx.mcp.is_empty() && task.provider == Provider::Codex {
            bail!("Codex tasks can't use MCP servers here; use Copilot or Claude");
        }
        let mut task = with_role(self.role.as_deref(), task);
        // The model it asked for, if this agent's owner allows it; else the default. The
        // same for the reasoning effort.
        let choice = crate::models::choice(&self.models.read().unwrap(), task.provider);
        task.model = choice.pick(task.model.as_deref())?;
        task.effort = choice.pick_effort(task.effort.as_deref())?;
        if !ctx.mcp.is_empty() {
            // MCP results reach the model as external content: the rule always leads.
            let names: Vec<String> = ctx.mcp.keys().cloned().collect();
            let body = task
                .prompt
                .strip_prefix(crate::boundary::DIRECTIVE)
                .unwrap_or(&task.prompt)
                .to_owned();
            task.prompt = format!(
                "{}{}{body}",
                crate::boundary::DIRECTIVE,
                crate::mcp::prompt_note(&names)
            );
        }
        let task = &task;
        let (mut model, mut effort, mut usage) = (None, None, None);
        let run = async {
            let ran = match task.provider {
                Provider::Copilot => {
                    self.copilot
                        .as_ref()
                        .expect("enabled")
                        .run(task, ctx.access, &ctx.workspace, &ctx.mcp, &choice.allowed)
                        .await
                }
                Provider::Claude | Provider::Codex => {
                    self.bridge
                        .as_ref()
                        .expect("enabled")
                        .run_with(task, &ctx.workspace, ctx.access, &ctx.mcp)
                        .await
                }
            }?;
            model = ran.model;
            effort = ran.effort;
            usage = ran.usage;
            Ok(ran.output)
        };
        let output = changes::track_changes(&ctx.workspace, ctx.access, run).await?;
        Ok(Ran::new(output, model)
            .with_effort(effort)
            .with_usage(usage))
    }
}

/// `task` with the agent's role put in front of the prompt, so the model acts in it.
pub fn with_role(role: Option<&str>, task: &LlmTask) -> LlmTask {
    let mut task = task.clone();
    if let Some(role) = role.map(str::trim).filter(|r| !r.is_empty()) {
        task.prompt = format!(
            "You are working as: {role}. Act in that role.\n\nTask:\n{}",
            task.prompt
        );
    }
    task
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_end_at_a_sentence_or_a_word_never_mid_word() {
        assert_eq!(
            title_of("Fix the login redirect. It loops forever when the cookie is missing."),
            "Fix the login redirect"
        );
        assert_eq!(
            title_of("Use e.g. a retry loop around the flaky network call. Then log it."),
            "Use e.g. a retry loop around the flaky network call"
        );
        let words = "word ".repeat(30);
        let long = title_of(&words);
        assert_eq!(long.chars().count(), 95);
        assert!(long.ends_with("word…"));
        let one = "x".repeat(150);
        assert_eq!(title_of(&one), format!("{}…", "x".repeat(99)));
        let early = format!("{} {}", "a".repeat(10), "b".repeat(140));
        assert_eq!(title_of(&early), format!("{}…", &early[..99]));
        assert_eq!(title_of("  Add a flag  \nmore"), "Add a flag");
        assert_eq!(title_of(""), "");
    }

    #[test]
    fn model_names_keep_only_what_model_names_use() {
        assert_eq!(clean_model(" claude-sonnet-5.5 "), "claude-sonnet-5.5");
        assert_eq!(
            clean_model("gpt-5.5, claude-opus-5.5"),
            "gpt-5.5, claude-opus-5.5"
        );
        assert_eq!(
            clean_model("x`](https://evil)\n<b>**y**</b>"),
            "x(https://evil)by/b"
        );
        assert_eq!(clean_model(&"m".repeat(300)).len(), 100);
        assert_eq!(Ran::new("out", Some("`".into())).model, None);
    }

    #[test]
    fn usage_keeps_only_what_providers_report_and_adds_up_runs() {
        let bridge = serde_json::json!({
            "input_tokens": 12_345, "cached_input_tokens": 10_000, "output_tokens": 678,
            "cost_usd": 0.0123, "premium_requests": -1, "surprise": "ignored"
        });
        let claude = Usage::from_json(&bridge).unwrap();
        assert_eq!(claude.premium_requests, None, "a negative amount isn't one");
        assert_eq!(
            claude.describe(),
            "12,345 tokens in (10,000 cached), 678 tokens out, $0.0123"
        );
        // A provider that reports nothing (or nothing usable) gives no usage, not zero.
        for nothing in [
            serde_json::json!({}),
            serde_json::json!(null),
            serde_json::json!({ "input_tokens": "many", "output_tokens": -3, "cost_usd": "1" }),
        ] {
            assert_eq!(Usage::from_json(&nothing), None, "{nothing}");
        }
        assert_eq!(
            Ran::new("out", None)
                .with_usage(Some(Usage::default()))
                .usage,
            None
        );

        let copilot = Usage {
            input_tokens: Some(1_000),
            output_tokens: Some(22),
            premium_requests: Some(1.0),
            ..Usage::default()
        };
        assert_eq!(
            copilot.describe(),
            "1,000 tokens in, 22 tokens out, 1 premium request"
        );
        // Each figure adds up where any run reports it, and stays out where none does.
        let total = Usage::sum([&copilot, &claude, &copilot]).unwrap();
        assert_eq!(total.input_tokens, Some(14_345));
        assert_eq!(total.cached_input_tokens, Some(10_000));
        assert_eq!(total.output_tokens, Some(722));
        assert_eq!(total.premium_requests, Some(2.0));
        assert_eq!(total.cost_usd, Some(0.0123));
        assert_eq!(Usage::sum([]), None);
        let premium = Usage {
            premium_requests: Some(0.33 + 1.0),
            cost_usd: Some(12.5),
            ..Usage::default()
        };
        assert_eq!(premium.describe(), "1.33 premium requests, $12.50");
        // On the wire, what isn't reported isn't sent.
        assert_eq!(
            serde_json::to_string(&copilot).unwrap(),
            r#"{"input_tokens":1000,"output_tokens":22,"premium_requests":1.0}"#
        );
    }

    #[test]
    fn role_guides_the_prompt() {
        let task = LlmTask::new(Provider::Copilot, "Review src/lib.rs").in_share("repo");
        let guided = with_role(Some("security reviewer"), &task);
        assert!(
            guided
                .prompt
                .starts_with("You are working as: security reviewer.")
        );
        assert!(guided.prompt.ends_with("Task:\nReview src/lib.rs"));
        assert_eq!(guided.share.as_deref(), Some("repo"));
        assert_eq!(with_role(None, &task), task);
        assert_eq!(with_role(Some("  "), &task), task);
    }
}
