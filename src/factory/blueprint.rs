//! Agent blueprints (docs/agent-factory.md, section 1): an agent's whole definition, as
//! data, in the `proofkin.agent-blueprint/v1` format. A blueprint can't run anything,
//! and it grants nothing: it asks for what its agent needs (access, MCP servers and
//! their tools, checks, Work IQ sources, delegation), and the owner's shares, MCP
//! offers and grants decide (ADR-0002).
//!
//! [`validate`] checks a blueprint against the profile it is for (a [`Catalog`]):
//! every skill, knowledge topic, folder, provider, model, effort and approver it names
//! must exist, and its name must be free. It also lints for least privilege: asking
//! for more than an agent is likely to need is a warning that names the narrower
//! choice. Like the plan check's, each [`Problem`] has a stable code, a one-line
//! message, the fix and the JSON pointer of the field.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use anyhow::Result;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::factory::evals::Suite;
use crate::llm::{Access, Provider};
use crate::models::Models;
use crate::profile::{Profile, name_ok};

/// The format this module reads and writes.
pub const SCHEMA: &str = "proofkin.agent-blueprint/v1";
/// The longest title, in characters.
pub const MAX_TITLE: usize = 80;
/// The longest purpose, in characters: it becomes a deployed agent's role.
pub const MAX_PURPOSE: usize = crate::workers::MAX_ROLE_CHARS;
/// The largest instructions, in bytes: they go into every task's prompt.
pub const MAX_INSTRUCTIONS: usize = 32 * 1024;
/// The most requirements a blueprint states.
pub const MAX_REQUIREMENTS: usize = 50;
/// The longest requirement, in characters.
pub const MAX_REQUIREMENT: usize = 500;
/// The most days lessons may be kept.
pub const MAX_RETENTION_DAYS: u32 = 3650;
/// The most tasks a day a budget may allow.
pub const MAX_TASKS_PER_DAY: u32 = 10_000;
/// The shortest task timeout a budget may set, in seconds.
pub const MIN_TIMEOUT_SECS: u64 = 60;
/// The longest task timeout a budget may set, in seconds (a week).
pub const MAX_TIMEOUT_SECS: u64 = 7 * 24 * 3600;
/// The largest team plan, in bytes.
pub const MAX_TEAM_PLAN: usize = 16 * 1024;
/// Names no agent may take: plan steps and approvals give them other meanings.
const RESERVED: &[&str] = &["me", crate::plan::WORKIQ_TARGET];

/// An agent's whole definition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Blueprint {
    /// Always [`SCHEMA`].
    pub schema: String,
    pub name: String,
    pub title: String,
    /// One sentence: what the agent is for. A deployed agent's role.
    pub purpose: String,
    /// What it must do, testably; eval cases prove each one.
    pub requirements: Vec<Requirement>,
    /// How its model works: the agent file's body.
    pub instructions: String,
    /// Skills it follows (built-in, the profile's own or its plugins').
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<String>,
    pub capabilities: Capabilities,
    /// A team plan for the team steps handed to it, as a folder's team plan is written
    /// (plan text whose `{task}` is the task handed down; see `proofkin share team`). It
    /// hands work to other agents, so it needs `delegate`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team: Option<String>,
    pub model: ModelPolicy,
    #[serde(default, skip_serializing_if = "Memory::is_empty")]
    pub memory: Memory,
    pub evals: Suite,
    #[serde(default, skip_serializing_if = "Budgets::is_empty")]
    pub budgets: Budgets,
    #[serde(default, skip_serializing_if = "Deploy::is_empty")]
    pub deploy: Deploy,
    /// Who must accept each release: `me`, or peers on the allowlist.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub approvers: Vec<String>,
}

/// Something the agent must do: `R1`, `R2`...
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Requirement {
    pub id: String,
    pub text: String,
}

/// What the agent asks for. Asking grants nothing: the folders it is deployed to, their
/// MCP offers and the owner's grants decide what its tasks get.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capabilities {
    /// The most access its tasks need.
    pub access: Access,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp: Vec<McpAsk>,
    /// Folder checks its tasks run (need full access).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<String>,
    /// Microsoft 365 sources (`workiq=`): email, meetings, files, teams, people,
    /// connectors, or all.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workiq: Vec<String>,
    /// Whether it may hand work to other agents.
    pub delegate: bool,
}

/// An MCP server the agent asks for, by the name its folders offer it under.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpAsk {
    pub server: String,
    /// The tools it uses; `"*"` asks for all of them.
    pub tools: Vec<String>,
    /// Why it needs the server; the owner reads it before offering one.
    pub why: String,
}

/// Which models may run the agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelPolicy {
    pub providers: Vec<Provider>,
    /// Models to evaluate; none: the profile's default for each provider.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub candidates: Vec<Candidate>,
    /// It uses MCP servers, which Codex can't here.
    pub needs_mcp: bool,
    /// The highest cost multiplier a candidate may have.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_multiplier: Option<f64>,
    /// What decides between candidates that all pass.
    pub prefer: Prefer,
}

/// A model to evaluate the agent on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Candidate {
    pub provider: Provider,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Prefer {
    Quality,
    Cost,
    Latency,
}

/// The knowledge the agent reads, and where what it learns goes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Memory {
    /// Knowledge topics put in every task's prompt.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context: Vec<String>,
    /// The topic its lessons go to, from verified results only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lessons: Option<String>,
    /// How long lessons are kept (90 days by default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention_days: Option<u32>,
}

impl Memory {
    pub fn is_empty(&self) -> bool {
        self.context.is_empty() && self.lessons.is_none() && self.retention_days.is_none()
    }
}

/// Limits a deployed agent's worker enforces before any model call.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Budgets {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tasks_per_day: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempts: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

impl Budgets {
    pub fn is_empty(&self) -> bool {
        self.tasks_per_day.is_none() && self.attempts.is_none() && self.timeout_secs.is_none()
    }
}

/// Where a deployment works: shared folders, by name.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Deploy {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub folders: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub start_with_owner: bool,
}

impl Deploy {
    pub fn is_empty(&self) -> bool {
        self.folders.is_empty() && !self.start_with_owner
    }
}

/// Something wrong with a blueprint: errors stop it from being built, warnings don't.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Problem {
    /// What kind of problem it is, stable for clients: `unknown_skill`,
    /// `least_privilege`...
    pub code: &'static str,
    pub error: bool,
    /// One line naming what is wrong.
    pub message: String,
    /// How to fix it: a command or an edit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
    /// The JSON pointer of the field, e.g. `/capabilities/mcp/0`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

impl Problem {
    pub fn error(code: &'static str, path: &str, message: impl Into<String>) -> Self {
        Self::new(code, true, path, message)
    }

    pub fn warning(code: &'static str, path: &str, message: impl Into<String>) -> Self {
        Self::new(code, false, path, message)
    }

    fn new(code: &'static str, error: bool, path: &str, message: impl Into<String>) -> Self {
        Self {
            code,
            error,
            message: message.into(),
            fix: None,
            path: (!path.is_empty()).then(|| path.to_owned()),
        }
    }

    /// The problem, with how to fix it.
    pub fn fix(mut self, fix: impl Into<String>) -> Self {
        self.fix = Some(fix.into());
        self
    }
}

impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)?;
        if let Some(fix) = &self.fix {
            write!(f, " (fix: {fix})")?;
        }
        Ok(())
    }
}

/// Whether any of `problems` is an error.
pub fn has_errors(problems: &[Problem]) -> bool {
    problems.iter().any(|p| p.error)
}

/// `problems` counted: "1 error, 2 warnings", or "no problems".
pub fn summary(problems: &[Problem]) -> String {
    let errors = problems.iter().filter(|p| p.error).count();
    let warnings = problems.len() - errors;
    let count = |n: usize, what: &str| format!("{n} {what}{}", if n == 1 { "" } else { "s" });
    match (errors, warnings) {
        (0, 0) => "no problems".into(),
        (e, 0) => count(e, "error"),
        (0, w) => count(w, "warning"),
        (e, w) => format!("{}, {}", count(e, "error"), count(w, "warning")),
    }
}

/// What a blueprint may name: what the profile it is for has.
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    /// The profile's name, for fixes.
    pub profile: String,
    /// Skills, by the names plan steps use, with what each is for.
    pub skills: BTreeMap<String, String>,
    /// Knowledge topics.
    pub topics: BTreeSet<String>,
    /// The topics made from Microsoft 365 content, with their labels (`unlabeled` for
    /// such content without a label).
    pub marked: BTreeMap<String, Vec<String>>,
    /// Shared folders, the built-in data folder too.
    pub folders: BTreeMap<String, Folder>,
    /// Providers its accounts are signed in to.
    pub providers: BTreeSet<Provider>,
    /// Its model settings.
    pub models: Models,
    /// Whether it is signed in to Work IQ.
    pub workiq: bool,
    /// Names on its allowlist.
    pub peers: BTreeSet<String>,
    /// Names its agents use: its own and its plugins'.
    pub agents: BTreeSet<String>,
    /// Names profiles on this machine use (a deployed agent is a profile).
    pub profiles: BTreeSet<String>,
    /// Names its factory uses: drafts and factory agents.
    pub factory: BTreeSet<String>,
}

/// A shared folder: where it is, the most access it gives, its checks and the MCP
/// servers it offers tasks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Folder {
    pub path: std::path::PathBuf,
    pub access: Access,
    pub checks: BTreeSet<String>,
    pub mcp: BTreeMap<String, Offer>,
}

/// An MCP server a folder offers tasks: the least access they need, the tools they may
/// call (none listed: all of them), and why it isn't offered now, if it isn't.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offer {
    pub access: Access,
    pub tools: Vec<String>,
    /// Its owner paused it.
    pub paused: bool,
    /// The plugin it comes with, when that plugin is turned off.
    pub plugin_off: Option<String>,
}

impl Offer {
    /// Whether tasks are offered it now (not paused, its plugin on).
    pub fn live(&self) -> bool {
        !self.paused && self.plugin_off.is_none()
    }

    /// Whether tasks may call `tool`.
    pub fn allows_tool(&self, tool: &str) -> bool {
        self.tools.is_empty() || self.tools.iter().any(|t| t == tool)
    }
}

impl Catalog {
    /// What `profile` has.
    pub fn of(profile: &Profile) -> Result<Self> {
        let skills = profile.skills();
        let accounts = profile.account_profile()?;
        let shares = profile.shares()?;
        let topics = profile.knowledge().list();
        Ok(Self {
            profile: profile.name.clone(),
            skills: skills
                .list()
                .into_iter()
                .map(|s| (s.name, s.description))
                .collect(),
            marked: topics
                .iter()
                .filter_map(|t| Some((t.name.clone(), labels_of_mark(t.workiq.as_deref()?))))
                .collect(),
            topics: topics.into_iter().map(|t| t.name).collect(),
            folders: shares
                .shares
                .iter()
                .map(|s| {
                    let mcp = s
                        .mcp
                        .iter()
                        .map(|(name, server)| {
                            let offer = Offer {
                                access: server.access,
                                tools: server.tools.clone(),
                                paused: server.paused,
                                plugin_off: server.plugin_off.then(|| {
                                    server.from.as_ref().map_or_else(
                                        || "its plugin".to_owned(),
                                        |o| o.plugin.clone(),
                                    )
                                }),
                            };
                            (name.clone(), offer)
                        })
                        .collect();
                    let folder = Folder {
                        path: s.path.clone(),
                        access: s.max_access,
                        checks: s.checks.keys().cloned().collect(),
                        mcp,
                    };
                    (s.name.clone(), folder)
                })
                .collect(),
            providers: [Provider::Copilot, Provider::Claude, Provider::Codex]
                .into_iter()
                .filter(|p| accounts.is_configured(*p))
                .collect(),
            models: profile.models(),
            workiq: profile.workiq_account().is_ok(),
            peers: profile.trust()?.peers.into_iter().map(|p| p.name).collect(),
            agents: skills.agents().into_iter().map(|a| a.name).collect(),
            profiles: Profile::list().unwrap_or_default().into_iter().collect(),
            factory: crate::factory::FactoryStore::of(profile)
                .names()?
                .into_iter()
                .collect(),
        })
    }

    /// The catalog for checking `name`'s own draft: its name is free for it.
    pub fn without(mut self, name: &str) -> Self {
        self.factory.remove(name);
        self
    }
}

/// The labels of a Microsoft 365 mark as audiences are checked against: its labels, or
/// `unlabeled` for content without one.
pub fn labels_of_mark(labels: &[String]) -> Vec<String> {
    if labels.is_empty() {
        vec![crate::workiq::content::UNLABELED.to_owned()]
    } else {
        labels.to_vec()
    }
}

/// Parses blueprint JSON: its syntax and the v1 format. A blueprint that isn't one is
/// a single problem saying why.
pub fn parse(text: &str) -> Result<Blueprint, Problem> {
    let value: Value = serde_json::from_str(text.trim_start_matches('\u{feff}'))
        .map_err(|e| Problem::error("invalid_json", "", format!("this isn't JSON: {e}")))?;
    from_value(&value)
}

/// The blueprint `value` is, if it is one in the v1 format.
pub fn from_value(value: &Value) -> Result<Blueprint, Problem> {
    let Some(object) = value.as_object() else {
        return Err(Problem::error(
            "invalid_blueprint",
            "",
            "a blueprint is a JSON object",
        ));
    };
    match object.get("schema").and_then(Value::as_str) {
        Some(SCHEMA) => {}
        Some(other) => {
            return Err(Problem::error(
                "unknown_schema",
                "/schema",
                format!("{other:?} isn't a blueprint format this proofkin reads"),
            )
            .fix(format!("use \"schema\": \"{SCHEMA}\"")));
        }
        None => {
            return Err(Problem::error(
                "unknown_schema",
                "/schema",
                "the blueprint names no format",
            )
            .fix(format!("add \"schema\": \"{SCHEMA}\"")));
        }
    }
    serde_json::from_value(value.clone()).map_err(|e| {
        Problem::error(
            "invalid_blueprint",
            "",
            format!("the blueprint doesn't fit {SCHEMA}: {e}"),
        )
        .fix("see the fields in docs/agent-factory.md (Blueprints)")
    })
}

/// Everything wrong with `blueprint` for the profile `catalog` describes: errors first,
/// then warnings; none when it can be built.
pub fn validate(blueprint: &Blueprint, catalog: &Catalog) -> Vec<Problem> {
    let mut found = Vec::new();
    check_name(blueprint, catalog, &mut found);
    check_texts(blueprint, &mut found);
    check_requirements(blueprint, &mut found);
    check_skills(blueprint, catalog, &mut found);
    check_capabilities(blueprint, catalog, &mut found);
    check_team(blueprint, &mut found);
    check_model(blueprint, catalog, &mut found);
    check_memory(blueprint, catalog, &mut found);
    check_budgets(blueprint, &mut found);
    check_deploy(blueprint, catalog, &mut found);
    check_approvers(blueprint, catalog, &mut found);
    lint_instructions(blueprint, &mut found);
    found.extend(crate::factory::evals::check(blueprint));
    found.sort_by_key(|p| !p.error);
    found
}

/// The problems of a blueprint given as JSON: the one that keeps it from being a v1
/// blueprint, else [`validate`]'s.
pub fn check_value(value: &Value, catalog: &Catalog) -> Vec<Problem> {
    match from_value(value) {
        Ok(blueprint) => validate(&blueprint, catalog),
        Err(problem) => vec![problem],
    }
}

fn check_name(bp: &Blueprint, catalog: &Catalog, found: &mut Vec<Problem>) {
    found.extend(name_problem(&bp.name, catalog));
}

/// Why `name` can't name a new agent of the profile `catalog` describes, if it can't:
/// it isn't a usable name, or something has it already.
pub fn name_problem(name: &str, catalog: &Catalog) -> Option<Problem> {
    if !name_ok(name) {
        return Some(
            Problem::error(
                "invalid_name",
                "/name",
                format!("{name:?} can't name an agent"),
            )
            .fix("use letters, digits, '-', '_' and '.', at most 64, not starting with '.'"),
        );
    }
    let taken = if RESERVED.contains(&name) {
        Some("a reserved name in plans")
    } else if catalog.factory.contains(name) {
        Some("another agent or draft of the factory")
    } else if catalog.agents.contains(name) {
        Some("an agent of this profile (proofkin agent list)")
    } else if name == catalog.profile || catalog.profiles.contains(name) {
        Some("a profile on this machine")
    } else if catalog.peers.contains(name) {
        Some("a peer on the allowlist")
    } else {
        None
    };
    taken.map(|what| {
        Problem::error("name_taken", "/name", format!("'{name}' is already {what}"))
            .fix("pick another name")
    })
}

fn check_texts(bp: &Blueprint, found: &mut Vec<Problem>) {
    for (field, text, most) in [
        ("title", &bp.title, MAX_TITLE),
        ("purpose", &bp.purpose, MAX_PURPOSE),
    ] {
        let at = format!("/{field}");
        if text.trim().is_empty() {
            found.push(Problem::error(
                "empty",
                &at,
                format!("the {field} is empty"),
            ));
        } else if text.contains('\n') {
            found.push(
                Problem::error(
                    "multiline",
                    &at,
                    format!("the {field} is more than one line"),
                )
                .fix("keep it to one line"),
            );
        } else if text.chars().count() > most {
            found.push(
                Problem::error(
                    "too_long",
                    &at,
                    format!("the {field} is over {most} characters"),
                )
                .fix(format!("shorten it to at most {most} characters")),
            );
        }
    }
    let instructions = bp.instructions.trim_start_matches('\u{feff}').trim_start();
    if instructions.trim().is_empty() {
        found.push(
            Problem::error("empty", "/instructions", "the instructions are empty")
                .fix("say how the agent works: its procedure, rules and output"),
        );
    } else if bp.instructions.len() > MAX_INSTRUCTIONS {
        found.push(
            Problem::error(
                "too_long",
                "/instructions",
                "the instructions are over 32 KB; they go into every task's prompt",
            )
            .fix("shorten them, or move procedures into skills"),
        );
    }
    if instructions.starts_with("---") {
        found.push(
            Problem::error(
                "front_matter",
                "/instructions",
                "the instructions start with front matter",
            )
            .fix("remove it: the factory writes the agent file's front matter from skills and capabilities"),
        );
    }
}

static REQUIREMENT_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^R[1-9][0-9]{0,3}$").expect("valid regex"));

fn check_requirements(bp: &Blueprint, found: &mut Vec<Problem>) {
    if bp.requirements.is_empty() {
        found.push(
            Problem::error(
                "no_requirements",
                "/requirements",
                "the blueprint states no requirement",
            )
            .fix("add what the agent must do, testably: {\"id\": \"R1\", \"text\": \"...\"}"),
        );
    } else if bp.requirements.len() > MAX_REQUIREMENTS {
        found.push(Problem::error(
            "too_many",
            "/requirements",
            format!("the blueprint states over {MAX_REQUIREMENTS} requirements"),
        ));
    }
    let mut ids = BTreeSet::new();
    for (i, r) in bp.requirements.iter().enumerate() {
        let at = format!("/requirements/{i}");
        if !REQUIREMENT_ID.is_match(&r.id) {
            found.push(
                Problem::error(
                    "invalid_requirement_id",
                    &format!("{at}/id"),
                    format!("{:?} isn't a requirement id", r.id),
                )
                .fix("number requirements R1, R2, R3..."),
            );
        } else if !ids.insert(r.id.as_str()) {
            found.push(
                Problem::error(
                    "duplicate_requirement",
                    &format!("{at}/id"),
                    format!("{} is stated twice", r.id),
                )
                .fix("give each requirement its own id"),
            );
        }
        if r.text.trim().is_empty() {
            found.push(Problem::error(
                "empty",
                &format!("{at}/text"),
                format!("{} says nothing", r.id),
            ));
        } else if r.text.chars().count() > MAX_REQUIREMENT {
            found.push(
                Problem::error(
                    "too_long",
                    &format!("{at}/text"),
                    format!("{} is over {MAX_REQUIREMENT} characters", r.id),
                )
                .fix("split it into requirements a case can prove one by one"),
            );
        }
    }
}

fn check_skills(bp: &Blueprint, catalog: &Catalog, found: &mut Vec<Problem>) {
    for (i, skill) in bp.skills.iter().enumerate() {
        let at = format!("/skills/{i}");
        if !catalog.skills.contains_key(skill) {
            found.push(
                Problem::error("unknown_skill", &at, format!("there is no skill '{skill}'")).fix(
                    format!(
                        "proofkin skill list --profile {} shows the skills there are",
                        catalog.profile
                    ),
                ),
            );
        } else if name_ok(&bp.name) && !name_ok(&crate::factory::build::skill_name(&bp.name, skill))
        {
            found.push(
                Problem::error(
                    "skill_name_too_long",
                    &at,
                    format!(
                        "its builds keep skill '{skill}' as '{}', which is over 64 characters",
                        crate::factory::build::skill_name(&bp.name, skill)
                    ),
                )
                .fix("give the agent a shorter name"),
            );
        }
    }
}

fn check_team(bp: &Blueprint, found: &mut Vec<Problem>) {
    let Some(plan) = &bp.team else {
        return;
    };
    if plan.len() > MAX_TEAM_PLAN {
        found.push(Problem::error(
            "too_long",
            "/team",
            "its team plan is over 16 KB",
        ));
    } else if let Err(e) = crate::team::validate_template(plan) {
        found.push(
            Problem::error("invalid_team_plan", "/team", format!("{e:#}"))
                .fix("write it as `proofkin share team` takes a team plan (see proofkin share team --help)"),
        );
    }
    if !bp.capabilities.delegate {
        found.push(
            Problem::error(
                "team_needs_delegate",
                "/capabilities/delegate",
                "its team plan hands work to other agents, and it doesn't ask to delegate",
            )
            .fix("set delegate to true, or remove the team plan"),
        );
    }
}

fn check_capabilities(bp: &Blueprint, catalog: &Catalog, found: &mut Vec<Problem>) {
    let caps = &bp.capabilities;
    let mut servers = BTreeSet::new();
    for (i, ask) in caps.mcp.iter().enumerate() {
        let at = format!("/capabilities/mcp/{i}");
        let server = &ask.server;
        if !name_ok(server) || server.contains("__") {
            found.push(
                Problem::error(
                    "invalid_name",
                    &format!("{at}/server"),
                    format!("{server:?} can't name an MCP server"),
                )
                .fix("use the name its folder offers it under: letters, digits, '-', '_' and '.'"),
            );
        } else if !servers.insert(server.as_str()) {
            found.push(Problem::error(
                "duplicate_mcp",
                &format!("{at}/server"),
                format!("it asks for '{server}' twice"),
            ));
        }
        if ask.tools.is_empty() {
            found.push(
                Problem::error(
                    "no_tools",
                    &format!("{at}/tools"),
                    format!("it names no tool of '{server}'"),
                )
                .fix("list the tools it uses"),
            );
        } else if ask.tools.iter().any(|t| t.trim() == "*") {
            found.push(
                Problem::warning(
                    "least_privilege",
                    &format!("{at}/tools"),
                    format!("it asks for every tool of '{server}'"),
                )
                .fix("list only the tools it uses instead of \"*\""),
            );
        } else if ask.tools.iter().any(|t| t.trim().is_empty()) {
            found.push(Problem::error(
                "invalid_tool",
                &format!("{at}/tools"),
                format!("a tool of '{server}' has no name"),
            ));
        }
        if ask.why.trim().is_empty() {
            found.push(
                Problem::error(
                    "no_reason",
                    &format!("{at}/why"),
                    format!("it doesn't say why it needs '{server}'"),
                )
                .fix("say why in a sentence: the owner reads it before offering the server"),
            );
        }
    }
    for (i, check) in caps.checks.iter().enumerate() {
        if !name_ok(check) {
            found.push(Problem::error(
                "invalid_name",
                &format!("/capabilities/checks/{i}"),
                format!("{check:?} can't name a check"),
            ));
        }
    }
    if !caps.checks.is_empty() && caps.access != Access::Full {
        found.push(
            Problem::error(
                "checks_need_full_access",
                "/capabilities/access",
                "its checks run with the folder's shell, which needs full access",
            )
            .fix("set access to full, or remove the checks"),
        );
    }
    if caps.access == Access::Full && caps.checks.is_empty() {
        found.push(
            Problem::warning(
                "least_privilege",
                "/capabilities/access",
                "full access lets its model run shell commands, and it runs no check",
            )
            .fix("use workspace-write to edit files without a shell, or name the checks it runs"),
        );
    }
    for (i, source) in caps.workiq.iter().enumerate() {
        let at = format!("/capabilities/workiq/{i}");
        if source == "all" {
            found.push(
                Problem::warning("least_privilege", &at, "it asks for every Microsoft 365 source")
                    .fix("name the sources it needs: email, meetings, files, teams, people or connectors"),
            );
        } else if !crate::plan::WORKIQ_SOURCES
            .iter()
            .any(|(name, _)| name == source)
        {
            found.push(
                Problem::error(
                    "unknown_workiq_source",
                    &at,
                    format!("{source:?} isn't a Microsoft 365 source"),
                )
                .fix("use email, meetings, files, teams, people, connectors or all"),
            );
        }
    }
    if !caps.workiq.is_empty() && !catalog.workiq {
        found.push(
            Problem::error(
                "workiq_not_signed_in",
                "/capabilities/workiq",
                "it asks for Microsoft 365 sources, and the profile isn't signed in to Work IQ",
            )
            .fix(format!(
                "proofkin auth login workiq --profile {} (see docs/work-iq.md)",
                catalog.profile
            )),
        );
    }
    if caps.delegate {
        found.push(
            Problem::warning(
                "least_privilege",
                "/capabilities/delegate",
                "it asks to hand work to other agents",
            )
            .fix("set delegate to false unless it leads other agents; your allowlist still decides whom it reaches"),
        );
    }
}

fn check_model(bp: &Blueprint, catalog: &Catalog, found: &mut Vec<Problem>) {
    let policy = &bp.model;
    if policy.providers.is_empty() {
        found.push(
            Problem::error(
                "no_providers",
                "/model/providers",
                "it names no provider to run on",
            )
            .fix("name copilot, claude or codex"),
        );
    }
    for (i, provider) in policy.providers.iter().enumerate() {
        if !catalog.providers.contains(provider) {
            found.push(
                Problem::error(
                    "provider_not_signed_in",
                    &format!("/model/providers/{i}"),
                    format!("the profile isn't signed in to {provider}"),
                )
                .fix(format!(
                    "proofkin auth login {provider} --profile {}, or remove it",
                    catalog.profile
                )),
            );
        }
    }
    if !bp.capabilities.mcp.is_empty() && !policy.needs_mcp {
        found.push(
            Problem::error(
                "needs_mcp",
                "/model/needs_mcp",
                "it asks for MCP servers, so it needs a provider that can use them",
            )
            .fix("set needs_mcp to true"),
        );
    }
    let codex = |p: &Provider| *p == Provider::Codex;
    if policy.needs_mcp && !policy.providers.is_empty() {
        if policy.providers.iter().all(codex) {
            found.push(
                Problem::error(
                    "codex_no_mcp",
                    "/model/providers",
                    "it needs MCP servers, which Codex can't use here",
                )
                .fix("add copilot or claude"),
            );
        } else if policy.providers.iter().any(codex) {
            found.push(
                Problem::warning(
                    "codex_no_mcp",
                    "/model/providers",
                    "it needs MCP servers, which Codex can't use here: its Codex tasks would be refused",
                )
                .fix("remove codex"),
            );
        }
    }
    for (i, candidate) in policy.candidates.iter().enumerate() {
        let at = format!("/model/candidates/{i}");
        let provider = candidate.provider;
        if !policy.providers.contains(&provider) {
            found.push(
                Problem::error(
                    "candidate_provider",
                    &format!("{at}/provider"),
                    format!("{provider} isn't one of its providers"),
                )
                .fix(format!(
                    "add {provider} to providers, or remove the candidate"
                )),
            );
        }
        if policy.needs_mcp && provider == Provider::Codex {
            found.push(Problem::error(
                "codex_no_mcp",
                &format!("{at}/provider"),
                "a Codex candidate can't use the MCP servers it needs",
            ));
        }
        let choice = crate::models::choice(&catalog.models, provider);
        if crate::models::check_name(&candidate.model).is_err() {
            found.push(Problem::error(
                "invalid_model",
                &format!("{at}/model"),
                format!("{:?} isn't a model name", candidate.model),
            ));
        } else if !choice.allows(&candidate.model) {
            found.push(
                Problem::error(
                    "model_not_allowed",
                    &format!("{at}/model"),
                    format!(
                        "the profile doesn't allow {} on {provider}",
                        candidate.model
                    ),
                )
                .fix(format!(
                    "proofkin models allow {provider} {},... --profile {} (or pick an allowed one: {})",
                    candidate.model,
                    catalog.profile,
                    choice.allowed.join(", ")
                )),
            );
        }
        if let Some(effort) = &candidate.effort {
            if let Err(e) = crate::models::check_effort(effort) {
                found.push(Problem::error(
                    "invalid_effort",
                    &format!("{at}/effort"),
                    format!("{e:#}"),
                ));
            } else if !choice.allows_effort(effort) {
                found.push(
                    Problem::error(
                        "effort_not_allowed",
                        &format!("{at}/effort"),
                        format!("the profile doesn't allow {effort} reasoning on {provider}"),
                    )
                    .fix(format!(
                        "pick one of {}, or proofkin models allow {provider} --efforts ... --profile {}",
                        choice.efforts.join(", "),
                        catalog.profile
                    )),
                );
            }
        }
    }
    if let Some(most) = policy.max_multiplier
        && (!most.is_finite() || most < 0.0)
    {
        found.push(Problem::error(
            "out_of_range",
            "/model/max_multiplier",
            "max_multiplier is a cost multiplier of 0 or more",
        ));
    }
}

fn check_memory(bp: &Blueprint, catalog: &Catalog, found: &mut Vec<Problem>) {
    let memory = &bp.memory;
    for (i, topic) in memory.context.iter().enumerate() {
        if !catalog.topics.contains(topic) {
            found.push(
                Problem::error(
                    "unknown_topic",
                    &format!("/memory/context/{i}"),
                    format!("there is no knowledge topic '{topic}'"),
                )
                .fix(format!(
                    "proofkin kb list --profile {} shows the topics there are",
                    catalog.profile
                )),
            );
        }
    }
    if let Some(topic) = &memory.lessons
        && !name_ok(topic)
    {
        found.push(Problem::error(
            "invalid_name",
            "/memory/lessons",
            format!("{topic:?} can't name a knowledge topic"),
        ));
    }
    if let Some(days) = memory.retention_days
        && (days == 0 || days > MAX_RETENTION_DAYS)
    {
        found.push(Problem::error(
            "out_of_range",
            "/memory/retention_days",
            format!("lessons are kept 1 to {MAX_RETENTION_DAYS} days"),
        ));
    }
}

fn check_budgets(bp: &Blueprint, found: &mut Vec<Problem>) {
    let budgets = &bp.budgets;
    if let Some(n) = budgets.tasks_per_day
        && (n == 0 || n > MAX_TASKS_PER_DAY)
    {
        found.push(Problem::error(
            "out_of_range",
            "/budgets/tasks_per_day",
            format!("tasks_per_day is 1 to {MAX_TASKS_PER_DAY}"),
        ));
    }
    if let Some(n) = budgets.attempts
        && (n == 0 || n > crate::evidence::MAX_ATTEMPTS)
    {
        found.push(Problem::error(
            "out_of_range",
            "/budgets/attempts",
            format!("attempts is 1 to {}", crate::evidence::MAX_ATTEMPTS),
        ));
    }
    if let Some(secs) = budgets.timeout_secs
        && !(MIN_TIMEOUT_SECS..=MAX_TIMEOUT_SECS).contains(&secs)
    {
        found.push(Problem::error(
            "out_of_range",
            "/budgets/timeout_secs",
            format!("timeout_secs is {MIN_TIMEOUT_SECS} (a minute) to {MAX_TIMEOUT_SECS} (a week)"),
        ));
    }
}

fn check_deploy(bp: &Blueprint, catalog: &Catalog, found: &mut Vec<Problem>) {
    for (i, folder) in bp.deploy.folders.iter().enumerate() {
        let at = format!("/deploy/folders/{i}");
        if folder == crate::shares::DATA_SHARE {
            found.push(
                Problem::error(
                    "data_folder",
                    &at,
                    "every deployed agent has its own data folder",
                )
                .fix("name folders you share (proofkin share list)"),
            );
        } else if !catalog.folders.contains_key(folder) {
            found.push(
                Problem::error(
                    "unknown_folder",
                    &at,
                    format!("you share no folder '{folder}'"),
                )
                .fix(format!(
                    "proofkin share add {folder} <path> --profile {}",
                    catalog.profile
                )),
            );
        }
    }
}

fn check_approvers(bp: &Blueprint, catalog: &Catalog, found: &mut Vec<Problem>) {
    for (i, approver) in bp.approvers.iter().enumerate() {
        if approver != "me" && !catalog.peers.contains(approver) {
            found.push(
                Problem::error(
                    "unknown_approver",
                    &format!("/approvers/{i}"),
                    format!("'{approver}' isn't on your allowlist"),
                )
                .fix("approvers are me, or peers on your allowlist (proofkin peers list)"),
            );
        }
    }
}

static RUNS_COMMANDS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(run|execute|invoke)\b[^.\n]{0,40}?\b(commands?|scripts?|shell|terminal|builds?|tests?|cargo|npm|pytest|make)\b|\b(powershell|bash)\b",
    )
    .expect("valid regex")
});
static USES_WEB: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(search|browse) (the )?(web|internet)\b|\bweb search\b|\blook (it |them |this )?up online\b|\b(open|fetch|visit) (the |a |any )?(urls?|links?|web ?pages?|websites?)\b",
    )
    .expect("valid regex")
});

/// Instructions that ask for tools the capabilities don't give: commands need full
/// access, and tasks never have the web.
fn lint_instructions(bp: &Blueprint, found: &mut Vec<Problem>) {
    if bp.capabilities.access != Access::Full && RUNS_COMMANDS.is_match(&bp.instructions) {
        found.push(
            Problem::warning(
                "instructions_ask_for_tools",
                "/instructions",
                "the instructions ask it to run commands, which needs full access",
            )
            .fix(
                "give it full access and the checks it runs, or take that out of the instructions",
            ),
        );
    }
    if USES_WEB.is_match(&bp.instructions) {
        found.push(
            Problem::warning(
                "instructions_ask_for_tools",
                "/instructions",
                "the instructions ask it to use the web, which tasks never have",
            )
            .fix(
                "ask for an MCP server that does (a browser) in capabilities.mcp, or take that out",
            ),
        );
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    /// A blueprint every check passes for [`catalog`].
    pub(crate) fn sample() -> Value {
        json!({
            "schema": SCHEMA,
            "name": "fin-analyst",
            "title": "Financial analyst",
            "purpose": "Reads quarterly reports, extracts key metrics, flags risks and writes an executive summary",
            "requirements": [
                { "id": "R1", "text": "Extracts revenue, operating margin and free cash flow, with the page each came from" },
                { "id": "R2", "text": "Flags risks with the sentence that supports each" }
            ],
            "instructions": "You analyze financial reports for an executive audience. Quote figures exactly.",
            "skills": ["risk-review"],
            "capabilities": {
                "access": "read-only",
                "mcp": [{ "server": "pdf", "tools": ["read_pdf"], "why": "Reports are PDF files" }],
                "delegate": false
            },
            "model": {
                "providers": ["copilot"],
                "candidates": [{ "provider": "copilot", "model": "gpt-6-sol", "effort": "medium" }],
                "needs_mcp": true,
                "max_multiplier": 1.5,
                "prefer": "cost"
            },
            "memory": { "context": ["finance-glossary"], "lessons": "fin-analyst-lessons", "retention_days": 90 },
            "evals": {
                "repetitions": 3,
                "thresholds": { "pass_rate": 0.9 },
                "cases": [
                    { "id": "E1", "requirement": "R1", "input": "Analyze q3.pdf", "fixtures": ["q3.pdf"],
                      "expect": [{ "contains": "operating margin" }, { "regex": "p\\. ?[0-9]+" }], "smoke": true },
                    { "id": "E2", "requirement": "R2", "input": "List the risks in q3.pdf", "fixtures": ["q3.pdf"],
                      "expect": [{ "regex": "(?i)risk" }, { "judge": "Each risk quotes its sentence", "min": 4 }],
                      "timeout": "10m" }
                ]
            },
            "budgets": { "tasks_per_day": 50, "attempts": 2, "timeout_secs": 1200 },
            "deploy": { "folders": ["reports"], "start_with_owner": true },
            "approvers": ["me", "carol"]
        })
    }

    pub(crate) fn catalog() -> Catalog {
        let pdf = Offer {
            access: Access::ReadOnly,
            tools: vec!["read_pdf".into(), "search_pdf".into()],
            paused: false,
            plugin_off: None,
        };
        let folder = |access| Folder {
            path: std::path::PathBuf::from("/srv/reports"),
            access,
            checks: ["test".to_owned()].into(),
            mcp: [("pdf".to_owned(), pdf.clone())].into(),
        };
        let mut models = Models::new();
        models.insert(
            Provider::Copilot,
            crate::models::ModelChoice {
                default: Some("gpt-6-sol".into()),
                allowed: vec!["gpt-6-sol".into(), "claude-sonnet-5.5".into()],
                effort: None,
                efforts: vec!["low".into(), "medium".into(), "high".into()],
            },
        );
        Catalog {
            profile: "alice".into(),
            skills: ["risk-review", "code-review", "security-review"]
                .map(|s| (s.to_owned(), format!("The {s} procedure")))
                .into(),
            topics: ["finance-glossary".to_owned()].into(),
            marked: BTreeMap::new(),
            folders: [
                ("data".to_owned(), folder(Access::ReadOnly)),
                ("reports".to_owned(), folder(Access::Full)),
            ]
            .into(),
            providers: [Provider::Copilot, Provider::Claude].into(),
            models,
            workiq: false,
            peers: ["carol".to_owned()].into(),
            agents: ["sec-reviewer".to_owned()].into(),
            profiles: ["alice".to_owned(), "tester".to_owned()].into(),
            factory: ["old-agent".to_owned()].into(),
        }
    }

    fn problems_of(changed: impl FnOnce(&mut Value)) -> Vec<Problem> {
        let mut value = sample();
        changed(&mut value);
        check_value(&value, &catalog())
    }

    fn codes(problems: &[Problem]) -> Vec<(&'static str, bool, String)> {
        problems
            .iter()
            .map(|p| (p.code, p.error, p.path.clone().unwrap_or_default()))
            .collect()
    }

    #[test]
    fn a_valid_blueprint_has_no_problems_and_round_trips() {
        let value = sample();
        assert_eq!(check_value(&value, &catalog()), Vec::new());
        let blueprint = from_value(&value).unwrap();
        assert_eq!(serde_json::to_value(&blueprint).unwrap(), value);
        assert_eq!(summary(&[]), "no problems");
    }

    #[test]
    fn text_that_is_not_a_v1_blueprint_is_one_problem_saying_why() {
        let invalid = parse("{ \"schema\": ").unwrap_err();
        assert_eq!(invalid.code, "invalid_json");
        assert!(invalid.message.contains("line 1"), "{invalid}");

        let other = problems_of(|v| v["schema"] = json!("proofkin.agent-blueprint/v9"));
        assert_eq!(codes(&other), [("unknown_schema", true, "/schema".into())]);
        assert!(other[0].fix.as_deref().unwrap().contains(SCHEMA));

        let typo = problems_of(|v| {
            let caps = v["capabilities"].take();
            v["capabilites"] = caps;
            v.as_object_mut().unwrap().remove("capabilities");
        });
        assert_eq!(typo.len(), 1);
        assert_eq!(typo[0].code, "invalid_blueprint");
        assert!(typo[0].message.contains("capabilites"), "{}", typo[0]);

        let missing = problems_of(|v| {
            v.as_object_mut().unwrap().remove("purpose");
        });
        assert!(
            missing[0].message.contains("missing field `purpose`"),
            "{}",
            missing[0]
        );
    }

    #[test]
    fn references_the_profile_lacks_are_errors_with_their_fix() {
        let found = problems_of(|v| {
            v["skills"] = json!(["risk-review", "astrology"]);
            v["memory"]["context"] = json!(["finance-glossary", "secret-plans"]);
            v["deploy"]["folders"] = json!(["reports", "data", "ledger"]);
            v["model"]["providers"] = json!(["copilot", "codex"]);
            v["model"]["needs_mcp"] = json!(false);
            v["capabilities"]["mcp"] = json!([]);
            v["model"]["candidates"] = json!([
                { "provider": "copilot", "model": "gpt-9-ultra" },
                { "provider": "copilot", "model": "gpt-6-sol", "effort": "max" },
                { "provider": "copilot", "model": "gpt-6-sol", "effort": "hard" }
            ]);
            v["approvers"] = json!(["me", "mallory"]);
            v["capabilities"]["workiq"] = json!(["meetings", "fax"]);
        });
        let expected = [
            ("unknown_skill", "/skills/1"),
            ("unknown_workiq_source", "/capabilities/workiq/1"),
            ("workiq_not_signed_in", "/capabilities/workiq"),
            ("provider_not_signed_in", "/model/providers/1"),
            ("model_not_allowed", "/model/candidates/0/model"),
            ("effort_not_allowed", "/model/candidates/1/effort"),
            ("invalid_effort", "/model/candidates/2/effort"),
            ("unknown_topic", "/memory/context/1"),
            ("data_folder", "/deploy/folders/1"),
            ("unknown_folder", "/deploy/folders/2"),
            ("unknown_approver", "/approvers/1"),
        ];
        assert_eq!(
            codes(&found),
            expected.map(|(c, p)| (c, true, p.to_owned())).to_vec()
        );
        let fix = |code: &str| {
            found
                .iter()
                .find(|p| p.code == code)
                .and_then(|p| p.fix.clone())
                .unwrap()
        };
        assert!(fix("unknown_skill").contains("proofkin skill list --profile alice"));
        assert!(
            fix("provider_not_signed_in").contains("proofkin auth login codex --profile alice")
        );
        assert!(fix("model_not_allowed").contains("proofkin models allow copilot gpt-9-ultra"));
        assert!(fix("unknown_folder").contains("proofkin share add ledger"));
        assert!(fix("workiq_not_signed_in").contains("proofkin auth login workiq"));
        assert!(fix("unknown_topic").contains("proofkin kb list"));
    }

    #[test]
    fn a_name_in_use_or_reserved_is_refused() {
        for (name, why) in [
            ("old-agent", "another agent or draft of the factory"),
            ("sec-reviewer", "an agent of this profile"),
            ("tester", "a profile on this machine"),
            ("alice", "a profile on this machine"),
            ("carol", "a peer on the allowlist"),
            ("me", "a reserved name"),
            ("workiq", "a reserved name"),
        ] {
            let found = problems_of(|v| v["name"] = json!(name));
            assert_eq!(
                codes(&found),
                [("name_taken", true, "/name".into())],
                "{name}"
            );
            assert!(found[0].message.contains(why), "{name}: {}", found[0]);
        }
        let bad = problems_of(|v| v["name"] = json!("../fin"));
        assert_eq!(codes(&bad), [("invalid_name", true, "/name".into())]);
        // An agent's own draft doesn't take its name from itself.
        let mut own = sample();
        own["name"] = json!("old-agent");
        assert!(check_value(&own, &catalog().without("old-agent")).is_empty());
    }

    #[test]
    fn requirements_are_numbered_unique_and_stated() {
        let found = problems_of(|v| {
            v["requirements"] = json!([
                { "id": "R1", "text": "Extracts the metrics" },
                { "id": "R1", "text": "Flags risks" },
                { "id": "req-3", "text": "Summarizes" },
                { "id": "R4", "text": " " }
            ]);
            v["evals"]["cases"][1]["requirement"] = json!("R1");
        });
        let found = codes(&found);
        assert!(found.contains(&("duplicate_requirement", true, "/requirements/1/id".into())));
        assert!(found.contains(&("invalid_requirement_id", true, "/requirements/2/id".into())));
        assert!(found.contains(&("empty", true, "/requirements/3/text".into())));
        let none = problems_of(|v| v["requirements"] = json!([]));
        assert!(codes(&none).contains(&("no_requirements", true, "/requirements".into())));
    }

    #[test]
    fn least_privilege_lint_warns_and_names_the_narrower_choice() {
        let found = problems_of(|v| {
            v["capabilities"]["access"] = json!("full");
            v["capabilities"]["mcp"][0]["tools"] = json!(["*"]);
            v["capabilities"]["delegate"] = json!(true);
            v["instructions"] =
                json!("Run the tests with cargo, then search the web for each risk.");
        });
        assert!(found.iter().all(|p| !p.error), "only warnings: {found:?}");
        let warned = codes(&found);
        assert_eq!(
            warned,
            [
                ("least_privilege", false, "/capabilities/mcp/0/tools".into()),
                ("least_privilege", false, "/capabilities/access".into()),
                ("least_privilege", false, "/capabilities/delegate".into()),
                ("instructions_ask_for_tools", false, "/instructions".into()),
            ]
        );
        assert!(
            found[0]
                .fix
                .as_deref()
                .unwrap()
                .contains("instead of \"*\"")
        );
        assert!(found[1].fix.as_deref().unwrap().contains("workspace-write"));
        assert!(found[3].message.contains("web"));

        let mut signed_in = catalog();
        signed_in.workiq = true;
        let mut value = sample();
        value["capabilities"]["workiq"] = json!(["all"]);
        let all = check_value(&value, &signed_in);
        assert_eq!(
            codes(&all),
            [("least_privilege", false, "/capabilities/workiq/0".into())]
        );
        assert!(all[0].fix.as_deref().unwrap().contains("meetings"));

        let commands = problems_of(|v| v["instructions"] = json!("Execute the build script."));
        assert!(commands[0].message.contains("full access"), "{commands:?}");
        assert_eq!(summary(&found), "4 warnings");
    }

    #[test]
    fn capabilities_must_hang_together() {
        let found = problems_of(|v| {
            v["capabilities"]["checks"] = json!(["test"]);
            v["model"]["needs_mcp"] = json!(false);
            v["capabilities"]["mcp"][0]["why"] = json!("");
            v["instructions"] = json!("---\nname: x\n---\nDo it.");
        });
        let found = codes(&found);
        assert!(found.contains(&(
            "checks_need_full_access",
            true,
            "/capabilities/access".into()
        )));
        assert!(found.contains(&("needs_mcp", true, "/model/needs_mcp".into())));
        assert!(found.contains(&("no_reason", true, "/capabilities/mcp/0/why".into())));
        assert!(found.contains(&("front_matter", true, "/instructions".into())));

        let codex = problems_of(|v| v["model"]["providers"] = json!(["codex"]));
        let codex = codes(&codex);
        assert!(codex.contains(&("codex_no_mcp", true, "/model/providers".into())));
        assert!(codex.contains(&(
            "candidate_provider",
            true,
            "/model/candidates/0/provider".into()
        )));
    }

    #[test]
    fn texts_budgets_and_memory_stay_in_range() {
        let found = problems_of(|v| {
            v["title"] = json!("x".repeat(MAX_TITLE + 1));
            v["purpose"] = json!("two\nlines");
            v["budgets"] = json!({ "tasks_per_day": 0, "attempts": 9, "timeout_secs": 5 });
            v["memory"]["retention_days"] = json!(0);
            v["model"]["max_multiplier"] = json!(-1);
        });
        let found = codes(&found);
        for expected in [
            ("too_long", "/title"),
            ("multiline", "/purpose"),
            ("out_of_range", "/budgets/tasks_per_day"),
            ("out_of_range", "/budgets/attempts"),
            ("out_of_range", "/budgets/timeout_secs"),
            ("out_of_range", "/memory/retention_days"),
            ("out_of_range", "/model/max_multiplier"),
        ] {
            assert!(
                found.contains(&(expected.0, true, expected.1.into())),
                "{expected:?} in {found:?}"
            );
        }
    }

    #[test]
    fn errors_come_before_warnings() {
        let found = problems_of(|v| {
            v["capabilities"]["delegate"] = json!(true);
            v["skills"] = json!(["astrology"]);
        });
        assert_eq!(
            codes(&found),
            [
                ("unknown_skill", true, "/skills/0".into()),
                ("least_privilege", false, "/capabilities/delegate".into()),
            ]
        );
        assert!(has_errors(&found));
        assert_eq!(summary(&found), "1 error, 1 warning");
    }

    #[test]
    fn a_team_plan_is_a_valid_team_plan_and_needs_delegation() {
        let lead = |plan: &str, delegate: bool| {
            let found = problems_of(|v| {
                v["team"] = json!(plan);
                v["capabilities"]["delegate"] = json!(delegate);
            });
            codes(&found)
        };
        assert_eq!(
            lead("review @carol {task}", true),
            [("least_privilege", false, "/capabilities/delegate".into())],
            "a team plan with {{task}} is fine; delegation is still linted"
        );
        assert!(lead("review @carol Check it", true).contains(&(
            "invalid_team_plan",
            true,
            "/team".into()
        )));
        assert!(lead("review @carol {task}", false).contains(&(
            "team_needs_delegate",
            true,
            "/capabilities/delegate".into()
        )));
        let blueprint = from_value(&{
            let mut v = sample();
            v["team"] = json!("review @carol {task}");
            v
        })
        .unwrap();
        assert_eq!(blueprint.team.as_deref(), Some("review @carol {task}"));
    }

    #[test]
    fn a_skill_kept_under_the_agents_name_must_fit_a_name() {
        let long = "a".repeat(60);
        let found = problems_of(|v| v["name"] = json!(long));
        assert_eq!(
            codes(&found),
            [("skill_name_too_long", true, "/skills/0".into())]
        );
        assert!(found[0].message.contains(&format!("'{long}.risk-review'")));
    }
}
