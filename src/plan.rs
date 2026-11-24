//! Squads and plans. A boss groups peers into squads and runs plans: steps that
//! run on a peer or on any member of a squad, in dependency order and in parallel
//! where possible. Each step gets the results of the steps it depends on, and steps
//! whose prerequisites failed are skipped.
//!
//! Plan text, one claim or step per line (`//` starts a comment):
//!
//! ```text
//! claim: C-1 The parser handles empty input
//! api @alice:repo Summarize the public API of src/agent.rs
//! fix after api @bob:app verify=test attempts=3 claims=C-1 Make the parser accept empty input
//! review after fix #qa:app apply=fix verify=test claims=C-1 Check the fix independently
//! signoff after review approve @carol claims=C-1 Approve the change for release
//! ```
//!
//! `@peer` runs on that peer and `#squad` on the least busy online member of the
//! squad; `:folder` picks a folder the peer shares with you (default: its data
//! folder) and `using <provider>` a provider (default: copilot). Options before the
//! prompt: `verify=a,b` asks the peer's runtime to run those owner-defined checks
//! after the model (as signed evidence), `attempts=N` retries with the failures,
//! `claims=C-1` says which claims the step proves, `apply=step` applies that
//! prerequisite's commit first, `mcp=a,b` lets the model use those MCP servers
//! of the folder and `agent=name` makes the step act as that agent (its
//! instructions first, its skills and MCP servers added to the step's). Repeating
//! an option that takes a list adds to it (`skill=a skill=b` is `skill=a,b`); the
//! others are given once. `approve @person` (or `@me`) asks a
//! team member to sign off instead of running a model. See [`crate::runs`] for the
//! verdicts.
//!
//! With a tracker linked (see [`crate::tracker`]), each run is tracked as a GitHub
//! issue and each step as a sub-issue: `track: #12` works on issue 12, `track: <title>`
//! names a new issue and `track: off` leaves the run untracked; `issue=#15` makes a
//! step work on an existing issue.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow, bail};
use iroh::EndpointId;
use serde::{Deserialize, Serialize};
use tokio::task::JoinSet;

use crate::agent::{Agent, Delivered, PeerInfo};
use crate::evidence::Evidence;
use crate::knowledge::{KnowledgeBase, check_topics, union_labels, with_mark};
use crate::llm::{LlmTask, Provider};
use crate::profile::validate_name;
use crate::protocol::{Origin, Phase, Progress};
use crate::skills::Skills;
use crate::trust::TrustStore;
use crate::workiq::audit::{CallContext, Purpose};
use crate::workiq::content::{
    Audience, DEFAULT_CAP, Source, UNLABELED, answer_section, retrieval_section,
};
use crate::workiq::context::{ClientWork, WorkContext};

/// Longest prerequisite result passed on to a dependent step.
const MAX_DEP_CHARS: usize = 8_000;
/// Longest prerequisite task passed on with its result.
const MAX_DEP_TASK_CHARS: usize = 1_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Squad {
    pub name: String,
    /// Allowlist names of the peers in the squad.
    pub members: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SquadStore {
    #[serde(default)]
    pub squads: Vec<Squad>,
}

impl SquadStore {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.is_file() {
            return Ok(Self::default());
        }
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, serde_json::to_vec_pretty(self)?)
            .with_context(|| format!("writing {}", path.display()))
    }

    /// Creates or replaces squad `name`.
    pub fn set(&mut self, name: &str, members: Vec<String>) -> Result<()> {
        if !crate::profile::name_ok(name) {
            bail!("squad names use letters, digits, '-', '_' or '.' (got {name:?})");
        }
        if members.is_empty() {
            bail!("a squad needs at least one member");
        }
        for member in &members {
            if !crate::profile::name_ok(member) {
                bail!(
                    "{member:?} isn't a peer's name: peer names use letters, digits, '-', '_' or '.'"
                );
            }
        }
        let mut members = members;
        members.sort();
        members.dedup();
        self.squads.retain(|s| s.name != name);
        self.squads.push(Squad {
            name: name.to_owned(),
            members,
        });
        self.squads.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(())
    }

    pub fn remove(&mut self, name: &str) -> bool {
        let before = self.squads.len();
        self.squads.retain(|s| s.name != name);
        self.squads.len() != before
    }

    /// Takes `member` out of every squad; a squad left without members is removed.
    /// Returns the squads it was in.
    pub fn remove_member(&mut self, member: &str) -> Vec<String> {
        let mut left = Vec::new();
        for squad in &mut self.squads {
            if squad.members.iter().any(|m| m == member) {
                squad.members.retain(|m| m != member);
                left.push(squad.name.clone());
            }
        }
        self.squads.retain(|s| !s.members.is_empty());
        left
    }

    pub fn get(&self, name: &str) -> Option<&Squad> {
        self.squads.iter().find(|s| s.name == name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "name", rename_all = "snake_case")]
pub enum Target {
    Peer(String),
    Squad(String),
    /// `@workiq`: the plan's own agent asks Work IQ; nothing is sent to a peer.
    Workiq,
}

/// The name `@workiq` steps are written with; no peer can be given tasks under it.
pub const WORKIQ_TARGET: &str = "workiq";

/// The longest Work IQ agent id `agent=` takes on a `@workiq` step.
const MAX_WORKIQ_AGENT_ID: usize = 200;

impl std::fmt::Display for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Target::Peer(name) => write!(f, "@{name}"),
            Target::Squad(name) => write!(f, "#{name}"),
            Target::Workiq => write!(f, "@{WORKIQ_TARGET}"),
        }
    }
}

/// Whether `id` is a Work IQ agent id: letters, digits and `-`, `_`, `.`, `:`, `@`.
fn valid_workiq_agent_id(id: &str) -> bool {
    !id.is_empty()
        && id.chars().count() <= MAX_WORKIQ_AGENT_ID
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':' | '@'))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    pub id: String,
    pub after: Vec<String>,
    pub target: Target,
    pub share: Option<String>,
    pub provider: Provider,
    pub prompt: String,
    /// The step's own words, kept when skills and knowledge are put in front of
    /// `prompt`; empty until then.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub brief: String,
    /// Owner-defined checks the peer's runtime runs after the model (`verify=a,b`).
    #[serde(default)]
    pub checks: Vec<String>,
    /// Model attempts while checks fail (`attempts=N`).
    #[serde(default)]
    pub attempts: Option<u32>,
    /// Claims this step provides evidence for (`claims=C-1,C-2`).
    #[serde(default)]
    pub claims: Vec<String>,
    /// A prerequisite whose commit (patch) is applied before this step (`apply=step`).
    #[serde(default)]
    pub apply: Option<String>,
    /// A team member's sign-off instead of an LLM task (`approve @person`).
    #[serde(default)]
    pub approve: bool,
    /// Knowledge base topics put in the prompt (`context=a,b` or `context=all`).
    #[serde(default)]
    pub context: Vec<String>,
    /// Knowledge base topic the step's verified result is saved as (`save=topic`).
    #[serde(default)]
    pub save: Option<String>,
    /// Skills (instructions) put in the prompt (`skill=a,b`).
    #[serde(default)]
    pub skills: Vec<String>,
    /// MCP servers of the peer's folder the model may use (`mcp=a,b`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp: Vec<String>,
    /// The agent the step acts as (`agent=name`): its instructions come first, and
    /// its skills and MCP servers with it (see [`crate::skills`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// A team step (`team @lead`): the lead's agent runs its folder's team plan for it,
    /// whose own team steps may hand it down this many levels in all (`levels=N`,
    /// default 1: the lead's team does it). See [`crate::team`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team: Option<u32>,
    /// The longest the step may take, in seconds (`timeout=30m`); then it is
    /// cancelled, on every level below too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
    /// The issue of the tracker's repository the step works on (`issue=#15`), instead
    /// of a sub-issue made for it (see [`crate::tracker`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue: Option<u64>,
    /// Set when the step gets Microsoft 365 (Work IQ) content: the union of its sensitivity
    /// labels (empty: not known). A marked `context=` topic sets it, so will the `workiq=`
    /// option and `@workiq` steps, and [`Plan::marked`] passes it on to the steps after.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub m365: Option<Vec<String>>,
    /// Microsoft 365 sources the step's agent searches through Work IQ before the task is
    /// sent (`workiq=all` or `workiq=email,files`, see [`WORKIQ_SOURCES`]); the results
    /// go in front of its task.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workiq: Vec<String>,
    /// The model the step's task runs on (`model=name`; with `agent=`, the agent's, if it
    /// names one): the peer refuses it if its owner doesn't allow it (see
    /// [`crate::models`]). None: the peer's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The reasoning effort its task works at (`effort=max`, see
    /// [`crate::models::EFFORTS`]). None: the peer's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// A pull request step (`pr @peer:folder [title]`): the folder's agent opens the pull
    /// request from the branch of the work before it there, and follows it until it is
    /// merged (see [`crate::pulls`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pr: bool,
    /// Who a pull request step asks to review (`reviewers=me,carol,octocat`): `me` is this
    /// profile's GitHub account, an allowlisted peer is the account the allowlist has for
    /// it, any other name is a GitHub login. Empty (`reviewers=none`): no one. None: those
    /// who signed off before it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewers: Option<Vec<String>>,
    /// How many times a sign-off's approver may ask for changes (`changes=N`, at most
    /// [`MAX_CHANGE_ROUNDS`]; 0: they only approve or reject): the work it is on is done
    /// again with their note, then they are asked again. None: [`DEFAULT_CHANGE_ROUNDS`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changes: Option<u32>,
}

/// How many times a sign-off's approver may ask for changes unless it says (`changes=`).
pub const DEFAULT_CHANGE_ROUNDS: u32 = 2;
/// The most rounds of changes a sign-off allows.
pub const MAX_CHANGE_ROUNDS: u32 = 5;

/// The most reviewers a pull request step names.
pub const MAX_REVIEWERS: usize = 10;

/// The `workiq=` sources, and the Work IQ capability each one names.
pub const WORKIQ_SOURCES: &[(&str, &str)] = &[
    ("email", "Email"),
    ("meetings", "Meetings"),
    ("files", "OneDriveAndSharePoint"),
    ("teams", "TeamsMessages"),
    ("people", "People"),
    ("connectors", "GraphConnectors"),
];

/// The Work IQ strategy and capability names for `workiq=`-style `sources` (see
/// [`Step::workiq_request`]); `None` for no sources.
pub fn workiq_request_for(sources: &[String]) -> Option<(&'static str, Vec<&'static str>)> {
    if sources.is_empty() {
        return None;
    }
    if sources.iter().any(|s| s == "all") {
        return Some(("copilot", Vec::new()));
    }
    let capabilities: Vec<&'static str> = WORKIQ_SOURCES
        .iter()
        .filter(|(name, _)| sources.iter().any(|s| s == name))
        .map(|(_, capability)| *capability)
        .collect();
    let strategy = if sources.iter().any(|s| s == "connectors") {
        "copilot"
    } else {
        "grounding"
    };
    Some((strategy, capabilities))
}

/// The sources of a `workiq=` value (also `kb import workiq --from`): `all`, or names from
/// [`WORKIQ_SOURCES`], with the same refusals as in a plan.
pub fn parse_workiq_sources(value: &str) -> Result<Vec<String>> {
    let mut list = Vec::new();
    add_workiq(&mut list, value)?;
    Ok(list)
}

/// The longest search text sent to Work IQ, in characters.
pub const WORKIQ_QUERY_CHARS: usize = 1000;

impl Step {
    /// Whether this is a `@workiq` step.
    pub fn is_workiq(&self) -> bool {
        self.target == Target::Workiq
    }

    /// Whether the step's own output or results are Microsoft 365 content whose labels are
    /// known only when it runs: a `@workiq` step, or one with `workiq=`.
    pub fn brings_m365(&self) -> bool {
        self.is_workiq() || !self.workiq.is_empty()
    }

    /// Who a `@workiq` step shows as having done it: `workiq`, or `workiq:<agent id>`.
    pub fn workiq_name(&self) -> String {
        match &self.agent {
            Some(id) => format!("{WORKIQ_TARGET}:{id}"),
            None => WORKIQ_TARGET.to_owned(),
        }
    }

    /// Why this `@workiq` step can't be, if it can't: what it has that would make an answer
    /// from Microsoft 365 look like proof, or that has no meaning for it.
    fn workiq_refusal(&self) -> Option<String> {
        if !self.is_workiq() {
            return None;
        }
        let never_proof = |what: &str| {
            format!(
                "@workiq step '{}' can't have {what}: an answer from Microsoft 365 is context and never proof",
                self.id
            )
        };
        let has = [
            ("approve", self.approve),
            ("team", self.team.is_some()),
            ("claims=", !self.claims.is_empty()),
            ("verify=", !self.checks.is_empty()),
            ("apply=", self.apply.is_some()),
            ("mcp=", !self.mcp.is_empty()),
            ("skill=", !self.skills.is_empty()),
            ("attempts=", self.attempts.is_some()),
        ];
        if let Some((what, _)) = has.iter().find(|(_, present)| *present) {
            return Some(never_proof(what));
        }
        let meaningless = [
            ("context=", !self.context.is_empty()),
            ("workiq=", !self.workiq.is_empty()),
            ("a folder", self.share.is_some()),
        ];
        meaningless.iter().find(|(_, present)| *present).map(|(what, _)| {
            format!(
                "@workiq step '{}' can't have {what}: it asks Work IQ with its own words and the results of the steps it runs after",
                self.id
            )
        })
    }

    /// The Work IQ strategy and capability names a `workiq=` list asks for: `all` is the
    /// `copilot` strategy over everything, a list with `connectors` is `copilot` over those
    /// capabilities (only it searches connectors), any other list is `grounding` over them.
    /// `None` when the step has no `workiq=`.
    pub fn workiq_request(&self) -> Option<(&'static str, Vec<&'static str>)> {
        workiq_request_for(&self.workiq)
    }

    /// What the step's agent searches Work IQ for: its own words, cut to a size Work IQ
    /// takes.
    pub fn workiq_query(&self) -> String {
        self.own_text().chars().take(WORKIQ_QUERY_CHARS).collect()
    }
}
impl Step {
    /// The step's own words: what it was written with, before anything was put in
    /// front of its prompt.
    pub fn own_text(&self) -> &str {
        if self.brief.is_empty() {
            &self.prompt
        } else {
            &self.brief
        }
    }

    /// Remembers the step's own words before its prompt is changed.
    fn keep_brief(&mut self) {
        if self.brief.is_empty() {
            self.brief = self.prompt.clone();
        }
    }
}

/// A statement the plan must prove with evidence (`claim: ID text`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimDef {
    pub id: String,
    pub text: String,
}

/// How a plan's run is tracked when the profile has a tracker (`track:` line).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Track {
    /// `track: #12`: the run works on issue 12.
    Issue(u64),
    /// `track: <title>`: a new issue with this title tracks the run.
    New(String),
    /// `track: off`: the run isn't tracked.
    Off,
}

/// The longest title of a new issue (GitHub's limit).
const MAX_ISSUE_TITLE: usize = 256;

/// An issue number written `12` or `#12`.
pub fn issue_number(text: &str) -> Option<u64> {
    text.strip_prefix('#')
        .unwrap_or(text)
        .parse()
        .ok()
        .filter(|n| *n > 0)
}

fn parse_track(rest: &str) -> Result<Track> {
    let rest = rest.trim();
    if rest.is_empty() {
        bail!("track: needs #<issue number>, a title for a new issue, or off");
    }
    if rest.eq_ignore_ascii_case("off") {
        return Ok(Track::Off);
    }
    if let Some(n) = issue_number(rest) {
        return Ok(Track::Issue(n));
    }
    if rest.starts_with('#') {
        bail!("track: #N needs an issue number, got {rest:?}");
    }
    if rest.chars().count() > MAX_ISSUE_TITLE {
        bail!("track: an issue's title has at most {MAX_ISSUE_TITLE} characters");
    }
    Ok(Track::New(rest.to_owned()))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    #[serde(default)]
    pub claims: Vec<ClaimDef>,
    pub steps: Vec<Step>,
    /// How its run is tracked (`track:`); by default, as a new issue when the profile
    /// has a tracker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track: Option<Track>,
}

impl Plan {
    /// Parses and validates plan text (see the module docs). Reports every problem
    /// with its line number.
    pub fn parse(text: &str) -> Result<Self> {
        let mut steps = Vec::new();
        let mut claims = Vec::new();
        let mut track = None;
        let mut errors = Vec::new();
        let lines: Vec<&str> = text.lines().collect();
        let mut n = 0;
        while n < lines.len() {
            let raw = lines[n];
            let line = raw.trim();
            let at = n;
            n += 1;
            if line.is_empty() || line.starts_with("//") {
                continue;
            }
            let is_step = !line.starts_with("claim:") && !line.starts_with("track:");
            let mut continuation = String::new();
            if is_step {
                let (text, next) = continuation_after(&lines, at);
                continuation = text;
                n = next;
            }
            let parsed = if let Some(rest) = line.strip_prefix("claim:") {
                parse_claim(rest).map(|c| claims.push(c))
            } else if let Some(rest) = line.strip_prefix("track:") {
                if track.is_some() {
                    Err(anyhow!("track: is given twice; a plan has one"))
                } else {
                    parse_track(rest).map(|t| track = Some(t))
                }
            } else {
                parse_step(line, &continuation).map(|s| steps.push(s))
            };
            if let Err(e) = parsed {
                errors.push(format!("line {}: {e}", at + 1));
            }
        }
        if !errors.is_empty() {
            bail!("{}", errors.join("\n"));
        }
        let plan = Plan {
            claims,
            steps,
            track,
        };
        plan.validate()?;
        Ok(plan)
    }

    /// Whether the plan names issues of a tracker (`track: #N` or `issue=`).
    pub fn names_issues(&self) -> bool {
        matches!(self.track, Some(Track::Issue(_))) || self.steps.iter().any(|s| s.issue.is_some())
    }

    /// Unique ids, known dependencies and claims, sensible options and no cycles.
    pub fn validate(&self) -> Result<()> {
        if self.steps.is_empty() {
            bail!("the plan has no steps");
        }
        let mut claim_ids = HashMap::new();
        for claim in &self.claims {
            if claim_ids.insert(claim.id.as_str(), claim).is_some() {
                bail!("claim '{}' is defined twice", claim.id);
            }
        }
        let mut seen = HashMap::new();
        for step in &self.steps {
            if seen.insert(step.id.as_str(), step).is_some() {
                bail!("step '{}' is defined twice", step.id);
            }
        }
        let mut issues = HashMap::new();
        for step in &self.steps {
            let Some(n) = step.issue else { continue };
            if self.track == Some(Track::Issue(n)) {
                bail!(
                    "step '{}' works on #{n}, the run's own issue (track: #{n}); its steps get sub-issues",
                    step.id
                );
            }
            if let Some(other) = issues.insert(n, step.id.as_str()) {
                bail!(
                    "steps '{other}' and '{}' both work on issue #{n}; a step has its own issue",
                    step.id
                );
            }
        }
        for step in &self.steps {
            if let Some(why) = step.workiq_refusal() {
                bail!("{why}");
            }
            if step.is_workiq()
                && let Some(id) = &step.agent
                && !valid_workiq_agent_id(id)
            {
                bail!(
                    "@workiq step '{}': agent= needs a Work IQ agent id, got {id:?}",
                    step.id
                );
            }
            for dep in &step.after {
                if dep == &step.id {
                    bail!("step '{}' can't depend on itself", step.id);
                }
                if !seen.contains_key(dep.as_str()) {
                    bail!("step '{}' depends on unknown step '{dep}'", step.id);
                }
            }
            if let Some(claim) = step
                .claims
                .iter()
                .find(|c| !claim_ids.contains_key(c.as_str()))
            {
                bail!(
                    "step '{}' proves unknown claim '{claim}'; define it first with a line: claim: {claim} <what must be true>",
                    step.id
                );
            }
            if let Some(apply) = &step.apply
                && !step.after.contains(apply)
            {
                bail!(
                    "step '{}' applies the work of '{apply}', so it must run after it (after {apply})",
                    step.id
                );
            }
            if step.attempts.is_some() && step.checks.is_empty() {
                bail!(
                    "step '{}' sets attempts but no checks to retry on (verify=...)",
                    step.id
                );
            }
            if step.approve
                && (!step.checks.is_empty() || step.apply.is_some() || step.share.is_some())
            {
                bail!(
                    "approval step '{}' can't have checks, a folder or a patch",
                    step.id
                );
            }
            if step.approve && step.save.is_some() {
                bail!("approval step '{}' has no result to save", step.id);
            }
            if step.approve && !step.workiq.is_empty() {
                bail!(
                    "approval step '{}' is a person's decision: it has no task to search Microsoft 365 for (workiq=)",
                    step.id
                );
            }
            if step.approve && step.agent.is_some() {
                bail!(
                    "approval step '{}' is a person's decision: it can't act as an agent",
                    step.id
                );
            }
            if step.approve && matches!(step.target, Target::Squad(_)) {
                bail!("approval step '{}' needs a person: @name or @me", step.id);
            }
            if step.team.is_some()
                && (!step.checks.is_empty()
                    || step.attempts.is_some()
                    || step.apply.is_some()
                    || !step.mcp.is_empty()
                    || step.agent.is_some())
            {
                bail!(
                    "team step '{}' is done and proven by the lead's team plan: it can't have checks, attempts, apply=, mcp= or agent=",
                    step.id
                );
            }
        }
        // Kahn's algorithm: whatever can't be ordered is on a cycle.
        let mut pending: BTreeMap<&str, usize> = self
            .steps
            .iter()
            .map(|s| (s.id.as_str(), s.after.len()))
            .collect();
        let mut ready: Vec<&str> = pending
            .iter()
            .filter(|(_, n)| **n == 0)
            .map(|(id, _)| *id)
            .collect();
        while let Some(done) = ready.pop() {
            pending.remove(done);
            for step in &self.steps {
                if step.after.iter().any(|d| d == done)
                    && let Some(n) = pending.get_mut(step.id.as_str())
                {
                    *n -= 1;
                    if *n == 0 {
                        ready.push(step.id.as_str());
                    }
                }
            }
        }
        if !pending.is_empty() {
            let stuck: Vec<&str> = pending.keys().copied().collect();
            bail!("dependency cycle between steps: {}", stuck.join(", "));
        }
        for step in self.steps.iter().filter(|s| s.pr) {
            let place = format!(
                "{}:{}",
                step.target,
                step.share.as_deref().unwrap_or_default()
            );
            let sources = pr_sources(self, step);
            if sources.is_empty() {
                bail!(
                    "pull request step '{}' comes after no step that works in {place}: there is no work to open it with",
                    step.id
                );
            }
            let here: Vec<&Step> = sources
                .into_iter()
                .filter(|s| s.target == step.target)
                .collect();
            let ends = chain_ends(self, &here);
            if ends.len() > 1 {
                bail!(
                    "pull request step '{}' comes after steps {} that work in {place} on separate branches: a step builds on another's work only with apply=; or open a pull request for each",
                    step.id,
                    ends.iter()
                        .map(|s| s.id.as_str())
                        .collect::<Vec<_>>()
                        .join(" and ")
                );
            }
        }
        Ok(())
    }
}

impl Plan {
    /// The plan as it is sent: each step's `skill=` instructions and `context=`
    /// knowledge put in front of its prompt. Fails if a skill or topic doesn't exist.
    pub fn prepare(&self, kb: &KnowledgeBase, skills: &Skills) -> Result<Plan> {
        let mut plan = self.with_knowledge(kb)?;
        for step in &mut plan.steps {
            let workiq = step.is_workiq();
            let agent = match step.agent.as_ref().filter(|_| !workiq) {
                Some(name) => Some(
                    skills
                        .agent(name)
                        .with_context(|| format!("step '{}'", step.id))?,
                ),
                None => None,
            };
            if let Some(agent) = &agent {
                step.skills = union(&agent.skills, &step.skills);
                step.mcp = union(&agent.mcp, &step.mcp);
                // The agent's model, unless the step names one.
                if step.model.is_none() && !step.approve && step.team.is_none() {
                    step.model = agent.model.clone();
                }
            }
            if step.skills.is_empty() && agent.is_none() {
                continue;
            }
            let mut instructions = agent
                .map(|a| format!("\n### Agent: {}\n{}\n", a.name, a.instructions))
                .unwrap_or_default();
            instructions += &skills
                .instructions(&step.skills)
                .with_context(|| format!("step '{}'", step.id))?;
            step.keep_brief();
            step.prompt = format!(
                "Follow these instructions:\n{instructions}\n---\n{}",
                step.prompt
            );
        }
        Ok(plan)
    }

    /// The plan with each step's `context=` knowledge put in front of its prompt.
    /// Fails if a topic doesn't exist.
    pub fn with_knowledge(&self, kb: &KnowledgeBase) -> Result<Plan> {
        let mut plan = self.marked(kb);
        for step in &mut plan.steps {
            if step.context.is_empty() {
                continue;
            }
            check_topics(kb, &step.context).with_context(|| format!("step '{}'", step.id))?;
            let knowledge = kb.context(&step.context)?;
            step.keep_brief();
            step.prompt = format!(
                "Knowledge about this system (from earlier, verified work):\n{knowledge}\n---\n{}",
                step.prompt
            );
        }
        Ok(plan)
    }

    /// The plan with its Microsoft 365 marks set: a step is marked when it already is (the
    /// `workiq=` option and `@workiq` steps set it) or when a `context=` topic is, with
    /// the union of the labels, and a step is marked when a step it runs `after` is,
    /// because it gets that step's output. Topics that don't exist are left out here;
    /// the plan check reports them.
    pub fn marked(&self, kb: &KnowledgeBase) -> Plan {
        self.marked_with(kb, &HashMap::new())
    }

    /// [`Plan::marked`] where `seeds` adds labels to steps by id: the labels a `workiq=`
    /// step's results can have, when they are known (the grant of the peer it goes to).
    /// A step with `workiq=` is always marked.
    pub fn marked_with(&self, kb: &KnowledgeBase, seeds: &HashMap<String, Vec<String>>) -> Plan {
        let mut plan = self.clone();
        for step in &mut plan.steps {
            let mut labels = kb.marks(&step.context);
            if step.brings_m365() {
                let seed = seeds.get(&step.id).map(Vec::as_slice).unwrap_or_default();
                labels = Some(union_labels(labels.as_deref().unwrap_or_default(), seed));
            }
            if let Some(labels) = labels {
                step.m365 = Some(union_labels(
                    step.m365.as_deref().unwrap_or_default(),
                    &labels,
                ));
            }
        }
        loop {
            let marked: HashMap<String, Vec<String>> = plan
                .steps
                .iter()
                .filter_map(|s| Some((s.id.clone(), s.m365.clone()?)))
                .collect();
            let mut changed = false;
            for step in &mut plan.steps {
                for labels in step.after.iter().filter_map(|d| marked.get(d)) {
                    let merged = union_labels(step.m365.as_deref().unwrap_or_default(), labels);
                    if step.m365.as_ref() != Some(&merged) {
                        step.m365 = Some(merged);
                        changed = true;
                    }
                }
            }
            if !changed {
                return plan;
            }
        }
    }
}

/// Who may receive Microsoft 365 content (docs/work-iq.md, section 4): the peers the
/// allowlist grants it to, for the labels it names, and the owner's own digital workers,
/// who may have all of it. With the GitHub accounts it knows, it also says who a pull
/// request step asks to review.
#[derive(Debug, Clone, Default)]
pub struct Recipients {
    pub trust: TrustStore,
    /// The endpoint ids of the owner's digital workers.
    pub workers: Vec<EndpointId>,
    /// The profile the grants are made in, for the command that adds one.
    pub profile: Option<String>,
    /// Whether the profile has a Work IQ account for `workiq=` steps.
    pub workiq_account: bool,
    /// The profile's own GitHub login (see [`crate::profile::Profile::github_user`]): pull
    /// request steps ask it to review for `reviewers=me` and for the owner's own sign-offs.
    pub github: Option<String>,
}

impl Recipients {
    pub fn new(trust: TrustStore, workers: Vec<EndpointId>) -> Self {
        Self {
            trust,
            workers,
            profile: None,
            workiq_account: false,
            github: None,
        }
    }

    /// `agent`'s allowlist now and `profile`'s digital workers. If the workers can't be
    /// read, none counts as one: the check gets stricter, not looser.
    pub fn of(agent: &Agent, profile: &crate::profile::Profile) -> Self {
        Self {
            trust: agent.trusted(),
            workers: crate::workers::worker_ids(profile).unwrap_or_default(),
            profile: Some(profile.name.clone()),
            workiq_account: profile.workiq_account().is_ok(),
            github: profile.github_user(),
        }
    }

    /// The audience `id` is: the owner's digital worker, a peer with a grant, or none.
    pub fn audience_of(&self, id: &EndpointId) -> Option<Audience> {
        self.trust.audience_for(id, &self.workers)
    }

    /// [`Recipients::refusal`] for a step whose content is Microsoft 365 `labels`, with
    /// `labels` empty when none are known (yet): then only the grant itself is needed.
    pub fn refusal_for_labels(
        &self,
        step: &Step,
        labels: &[String],
        name: &str,
        id: &EndpointId,
    ) -> Option<String> {
        if labels.is_empty() && matches!(self.audience_of(id), Some(Audience::Peer { .. })) {
            return None;
        }
        let mut marked = step.clone();
        marked.m365 = Some(labels.to_vec());
        self.refusal(&marked, name, id)
    }

    /// Why `step`'s content may not go to the peer `name` (`id`), if it may not: it has no
    /// grant, or a label is outside it. Names the step, the peer, the labels and the command
    /// that grants them.
    pub fn refusal(&self, step: &Step, name: &str, id: &EndpointId) -> Option<String> {
        let labels = step.m365.as_ref()?;
        let needed = if labels.is_empty() {
            vec![UNLABELED.to_owned()]
        } else {
            labels.clone()
        };
        let (granted, why) = match self.trust.audience_for(id, &self.workers) {
            Some(Audience::Local) => return None,
            Some(Audience::Peer { labels: granted }) => {
                let outside: Vec<String> = needed
                    .iter()
                    .filter(|l| !granted.iter().any(|g| g.trim().eq_ignore_ascii_case(l)))
                    .cloned()
                    .collect();
                if outside.is_empty() {
                    return None;
                }
                (
                    granted,
                    format!("its grant doesn't include {}", outside.join(", ")),
                )
            }
            None => (Vec::new(), "it has no Microsoft 365 grant".to_owned()),
        };
        let mut command = format!("proofkin peers allow {id} --name {name}");
        if let Some(profile) = &self.profile {
            command += &format!(" --profile {profile}");
        }
        let entry = self.trust.get(id);
        if let Some(peer) = entry.filter(|p| p.can_delegate) {
            command += &format!(
                " --delegate --access {} --tasks-per-hour {} --share {}",
                peer.access,
                peer.tasks_per_hour,
                peer.shares.join(",")
            );
        }
        command += &format!(" --m365 {}", union_labels(&granted, &needed).join(","));
        if entry.and_then(|p| p.m365.as_ref()).is_some_and(|g| g.tools) {
            command += " --m365-tools";
        }
        Some(format!(
            "step '{}' gets Microsoft 365 content (labels: {}) that {name} may not receive: {why}. \
             Grant it with: {command}",
            step.id,
            needed.join(", ")
        ))
    }
}

/// Saves the result of every finished step with `save=` into `kb`, if its evidence
/// was verified (or it ran locally); returns the topics written. A step with Microsoft 365
/// content saves its topic marked as such, with the step's labels. A `@workiq` step's
/// answer has no evidence to verify: it is saved as an answer from Work IQ, marked.
pub fn save_knowledge(
    kb: &KnowledgeBase,
    plan: &Plan,
    states: &BTreeMap<String, StepState>,
    run_id: &str,
) -> Vec<String> {
    let mut saved = Vec::new();
    let plan = plan.marked(kb);
    for step in &plan.steps {
        let (
            Some(topic),
            Some(StepState::Done {
                peer,
                output,
                verified,
                ..
            }),
        ) = (&step.save, states.get(&step.id))
        else {
            continue;
        };
        let source = if step.is_workiq() {
            format!(
                "run {run_id}, step {} was a Work IQ answer from {peer} (cited, unverified)",
                step.id
            )
        } else if *verified {
            format!(
                "run {run_id}, step {} by {peer} (signed, verified)",
                step.id
            )
        } else {
            continue;
        };
        let text = match &step.m365 {
            Some(labels) => with_mark(output, labels),
            None => output.clone(),
        };
        if kb.write(topic, &text, &source).is_ok() {
            saved.push(topic.clone());
        }
    }
    saved
}

fn parse_claim(rest: &str) -> Result<ClaimDef> {
    let rest = rest.trim();
    let (id, text) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    validate_name(id).map_err(|_| anyhow!("invalid claim id {id:?}"))?;
    let text = text.trim();
    if text.is_empty() {
        bail!("claim {id} needs a statement: claim: {id} <what must be true>");
    }
    Ok(ClaimDef {
        id: id.to_owned(),
        text: text.to_owned(),
    })
}

/// What a step line looks like, for error messages.
const STEP_FORM: &str = "a step line is: name [after steps] @peer[:folder] or #squad [options] what to do, e.g. fix @alice:app skill=implement-change Fix the parser";

/// What the plan editor's palette (ui/plan-text.js) writes for a new step's task and a
/// new claim's statement, for you to type over.
const TODO_TASK: &str = "What to do";
const TODO_CLAIM: &str = "What must be true at the end";

/// Step options (`name=value`, before what to do).
const STEP_OPTIONS: &[&str] = &[
    "verify", "attempts", "claims", "claim", "apply", "context", "skill", "skills", "mcp", "save",
    "agent", "levels", "timeout", "issue", "workiq", "model", "effort",
];

/// The longest `timeout=` a step may have (a week).
const MAX_TIMEOUT: u64 = 7 * 24 * 3600;

/// Seconds in a step's `timeout=` value: a number and a unit (s, m, h or d).
fn parse_timeout(value: &str) -> Result<u64> {
    let digits = value.chars().take_while(char::is_ascii_digit).count();
    let (n, unit) = value.split_at(digits);
    let n: u64 = n.parse().map_err(|_| {
        anyhow!("timeout= needs a number and a unit, e.g. 30m or 2h, got {value:?}")
    })?;
    let secs = match unit {
        "s" | "sec" | "secs" => n,
        "m" | "min" | "mins" => n * 60,
        "h" | "hr" | "hrs" => n * 3600,
        "d" | "day" | "days" => n * 86_400,
        _ => bail!("timeout= needs a unit (s, m, h or d), e.g. 30m, got {value:?}"),
    };
    if secs == 0 || secs > MAX_TIMEOUT {
        bail!("timeout= must be more than 0 and at most 7 days, got {value:?}");
    }
    Ok(secs)
}

/// `secs` as a step's timeout reads: "30m", "2h", "45s".
pub fn timeout_text(secs: u64) -> String {
    match secs {
        s if s % 86_400 == 0 => format!("{}d", s / 86_400),
        s if s % 3600 == 0 => format!("{}h", s / 3600),
        s if s % 60 == 0 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

/// Whether `word` sets a step option.
fn is_option(word: &str) -> bool {
    word.split_once('=')
        .is_some_and(|(key, value)| !value.is_empty() && STEP_OPTIONS.contains(&key))
}

/// `text` without its backtick code spans (one or more backticks, closed by a run of the
/// same length; an unclosed one quotes to the end), each replaced by a space.
fn without_code_spans(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '`' {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        let open = chars[i..].iter().take_while(|&&c| c == '`').count();
        let mut j = i + open;
        let mut end = chars.len();
        while j < chars.len() {
            if chars[j] == '`' {
                let run = chars[j..].iter().take_while(|&&c| c == '`').count();
                if run == open {
                    end = j + run;
                    break;
                }
                j += run;
            } else {
                j += 1;
            }
        }
        out.push(' ');
        i = end;
    }
    out
}

/// Adds an option's comma-separated values to `list`: repeating an option adds to it.
fn add(list: &mut Vec<String>, value: &str) {
    for v in value.split(',').map(str::trim).filter(|v| !v.is_empty()) {
        if !list.iter().any(|have| have == v) {
            list.push(v.to_owned());
        }
    }
}

/// The lines after the step line `lines[at]` that are indented more than it, with their
/// common indentation removed and blank lines between them kept, and the index of the
/// first line after them. A blank line followed by a line not indented more ends the step.
fn continuation_after(lines: &[&str], at: usize) -> (String, usize) {
    let indent = |l: &str| l.chars().take_while(|c| c.is_whitespace()).count();
    let base = indent(lines[at]);
    let mut end = at + 1;
    let mut last = at;
    while end < lines.len() {
        let l = lines[end];
        if l.trim().is_empty() {
            end += 1;
        } else if indent(l) > base {
            end += 1;
            last = end - 1;
        } else {
            break;
        }
    }
    if last == at {
        return (String::new(), at + 1);
    }
    let body = &lines[at + 1..=last];
    let common = body
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| indent(l))
        .min()
        .unwrap_or(0);
    let text = body
        .iter()
        .map(|l| {
            let l = l.trim_end();
            l.char_indices().nth(common).map_or("", |(i, _)| &l[i..])
        })
        .collect::<Vec<_>>()
        .join("\n");
    (text, last + 1)
}

fn parse_step(line: &str, continuation: &str) -> Result<Step> {
    let mut words = line.split_whitespace().peekable();
    let id = words.next().context("missing step id")?.to_owned();
    if validate_name(&id).is_err() {
        if id.contains('=') {
            bail!("starts with the option {id}, but {STEP_FORM}");
        }
        if id.starts_with(['@', '#']) {
            bail!("starts with who runs it ({id}), but {STEP_FORM}");
        }
        bail!("invalid step name {id:?}: use letters, digits, '-', '_' or '.'");
    }
    let mut after = Vec::new();
    if words.peek() == Some(&"after") {
        words.next();
        let deps = words
            .next()
            .context("'after' needs step ids, e.g. after a,b")?;
        after = deps
            .split(',')
            .filter(|d| !d.is_empty())
            .map(str::to_owned)
            .collect();
    }
    let approve = words.peek() == Some(&"approve");
    if approve {
        words.next();
    }
    let team = !approve && words.peek() == Some(&"team");
    if team {
        words.next();
    }
    let pr = !approve && !team && words.peek() == Some(&"pr");
    if pr {
        words.next();
    }
    let target = words
        .next()
        .with_context(|| format!("who runs it is missing; {STEP_FORM}"))?;
    let Some(rest) = target
        .strip_prefix('@')
        .or_else(|| target.strip_prefix('#'))
    else {
        bail!(
            "who runs it (@peer, @peer:folder or #squad) comes after the step's name, got {target:?}"
        );
    };
    let (name, share) = match rest.split_once(':') {
        Some((name, share)) => (name, Some(share.to_owned()).filter(|s| !s.is_empty())),
        None => (rest, None),
    };
    validate_name(name).map_err(|_| anyhow!("invalid target {target:?}"))?;
    let target = if target.starts_with('@') && name == WORKIQ_TARGET {
        Target::Workiq
    } else if target.starts_with('@') {
        Target::Peer(name.to_owned())
    } else {
        Target::Squad(name.to_owned())
    };
    let mut provider = Provider::Copilot;
    let mut using = false;
    let (mut checks, mut attempts, mut claims, mut apply) = (Vec::new(), None, Vec::new(), None);
    let (mut context, mut save, mut skills, mut mcp) = (Vec::new(), None, Vec::new(), Vec::new());
    let mut agent = None;
    let mut workiq = Vec::new();
    let (mut model, mut effort) = (None, None);
    let (mut levels, mut timeout, mut issue) = (None, None, None);
    let mut reviewers: Option<Vec<String>> = None;
    let mut changes: Option<u32> = None;
    let mut once = std::collections::HashSet::new();
    loop {
        match words.peek().copied() {
            Some("using") => {
                words.next();
                let name = words.next().context("'using' needs a provider")?;
                provider = clap::ValueEnum::from_str(name, true)
                    .map_err(|_| anyhow!("unknown provider {name:?}"))?;
                using = true;
            }
            Some(word) if word.contains('=') => {
                let (key, value) = word.split_once('=').expect("contains '='");
                if matches!(
                    key,
                    "attempts"
                        | "apply"
                        | "save"
                        | "agent"
                        | "levels"
                        | "timeout"
                        | "issue"
                        | "model"
                        | "effort"
                        | "changes"
                ) && !once.insert(key)
                {
                    bail!("{key}= is given twice; a step has one");
                }
                match key {
                    "verify" => add(&mut checks, value),
                    "model" => {
                        crate::models::check_name(value).map_err(|_| {
                            anyhow!(
                                "model= needs a model's name, e.g. model=claude-sonnet-5.5, got {value:?}"
                            )
                        })?;
                        model = Some(value.to_owned());
                    }
                    "effort" => {
                        crate::models::check_effort(value).map_err(|_| {
                            anyhow!(
                                "effort= needs a reasoning effort ({}), got {value:?}",
                                crate::models::EFFORTS.join(", ")
                            )
                        })?;
                        effort = Some(value.to_ascii_lowercase());
                    }
                    "attempts" => {
                        let n: u32 = value
                            .parse()
                            .map_err(|_| anyhow!("attempts must be a number, got {value:?}"))?;
                        if !(1..=crate::evidence::MAX_ATTEMPTS).contains(&n) {
                            bail!(
                                "attempts must be between 1 and {}",
                                crate::evidence::MAX_ATTEMPTS
                            );
                        }
                        attempts = Some(n);
                    }
                    "claims" | "claim" => add(&mut claims, value),
                    "apply" => apply = Some(value.to_owned()).filter(|v| !v.is_empty()),
                    "context" => add(&mut context, value),
                    "skill" | "skills" => add(&mut skills, value),
                    "workiq" => add_workiq(&mut workiq, value)?,
                    "mcp" => {
                        add(&mut mcp, value);
                        if let Some(bad) = mcp.iter().find(|m| validate_name(m).is_err()) {
                            bail!("mcp= needs MCP server names, got {bad:?}");
                        }
                    }
                    "save" => {
                        validate_name(value)
                            .map_err(|_| anyhow!("save= needs a topic name, got {value:?}"))?;
                        save = Some(value.to_owned());
                    }
                    "agent" if target == Target::Workiq => {
                        if !valid_workiq_agent_id(value) {
                            bail!(
                                "agent= on a @workiq step needs a Work IQ agent id (letters, digits and - _ . : @, at most {MAX_WORKIQ_AGENT_ID} characters), got {value:?}"
                            );
                        }
                        agent = Some(value.to_owned());
                    }
                    "agent" => {
                        validate_name(value)
                            .map_err(|_| anyhow!("agent= needs an agent's name, got {value:?}"))?;
                        agent = Some(value.to_owned());
                    }
                    "levels" => {
                        let n: u32 = value
                            .parse()
                            .map_err(|_| anyhow!("levels must be a number, got {value:?}"))?;
                        if !(1..=crate::team::MAX_LEVELS).contains(&n) {
                            bail!("levels must be between 1 and {}", crate::team::MAX_LEVELS);
                        }
                        levels = Some(n);
                    }
                    "timeout" => timeout = Some(parse_timeout(value)?),
                    "issue" => {
                        issue = Some(issue_number(value).ok_or_else(|| {
                            anyhow!("issue= needs an issue number, e.g. issue=#12, got {value:?}")
                        })?);
                    }
                    "reviewers" | "reviewer" => {
                        add(reviewers.get_or_insert_with(Vec::new), value);
                    }
                    "changes" => {
                        let n: u32 = value.parse().map_err(|_| {
                            anyhow!("changes= needs a number of rounds, got {value:?}")
                        })?;
                        if n > MAX_CHANGE_ROUNDS {
                            bail!("changes= allows at most {MAX_CHANGE_ROUNDS} rounds");
                        }
                        changes = Some(n);
                    }
                    _ => break,
                }
                words.next();
            }
            _ => break,
        }
    }
    let mut prompt = words.collect::<Vec<_>>().join(" ");
    if !continuation.is_empty() {
        let lead = continuation.trim_start_matches('\n');
        prompt = if prompt.is_empty() {
            lead.to_owned()
        } else {
            format!("{prompt}\n{continuation}")
        };
    }
    if prompt.is_empty() && !pr {
        bail!("missing the task prompt: what to do comes last, after who runs it and the options");
    }
    if pr {
        let given = [
            ("using", using),
            ("verify=", !checks.is_empty()),
            ("attempts=", attempts.is_some()),
            ("claims=", !claims.is_empty()),
            ("apply=", apply.is_some()),
            ("context=", !context.is_empty()),
            ("save=", save.is_some()),
            ("skill=", !skills.is_empty()),
            ("mcp=", !mcp.is_empty()),
            ("agent=", agent.is_some()),
            ("workiq=", !workiq.is_empty()),
            ("model=", model.is_some()),
            ("effort=", effort.is_some()),
            ("levels=", levels.is_some()),
        ];
        if let Some((what, _)) = given.iter().find(|(_, present)| *present) {
            bail!(
                "pull request step '{id}' opens a pull request with the work before it: it can't have {what}"
            );
        }
        if !matches!(&target, Target::Peer(name) if name != "me") || share.is_none() {
            bail!(
                "pull request step '{id}' needs @peer:folder: the agent that holds the folder's clone, and the folder"
            );
        }
    }
    if let Some(list) = &mut reviewers {
        if !pr {
            bail!("reviewers= is for pull request steps (name after steps pr @peer:folder)");
        }
        check_reviewers(list)?;
    }
    if changes.is_some() && !approve {
        bail!(
            "changes= is for sign-offs (name after steps approve @person): how many times the approver may ask for changes"
        );
    }
    if levels.is_some() && target == Target::Workiq {
        bail!(
            "@workiq step '{id}' can't have levels=: an answer from Microsoft 365 is context and never proof"
        );
    }
    if levels.is_some() && !team {
        bail!("levels= is for team steps (name team @lead ...)");
    }
    if (model.is_some() || effort.is_some()) && (approve || team || target == Target::Workiq) {
        bail!(
            "{}= is for steps a model runs; step '{id}' {}",
            if model.is_some() { "model" } else { "effort" },
            if approve {
                "is a person's sign-off"
            } else if team {
                "is a team step: its lead's team plan says who runs what"
            } else {
                "is answered by Work IQ"
            }
        );
    }
    let step = Step {
        id,
        after,
        target,
        share,
        provider,
        prompt,
        brief: String::new(),
        checks,
        attempts,
        claims,
        apply,
        approve,
        context,
        save,
        skills,
        mcp,
        agent,
        team: team.then(|| levels.unwrap_or(1)),
        timeout,
        issue,
        m365: None,
        workiq,
        model,
        effort,
        pr,
        reviewers,
        changes,
    };
    if let Some(why) = step.workiq_refusal() {
        bail!("{why}");
    }
    Ok(step)
}

/// Checks the reviewers a step names (see [`Step::reviewers`]): GitHub logins, peer names
/// or `me`, at most [`MAX_REVIEWERS`], or `none` alone, which leaves `list` empty.
fn check_reviewers(list: &mut Vec<String>) -> Result<()> {
    if list.is_empty() {
        bail!("reviewers= needs GitHub logins, peer names, me or none");
    }
    if let Some(bad) = list
        .iter()
        .find(|r| validate_name(r).is_err() && !crate::github::login_ok(r))
    {
        bail!("reviewers= needs GitHub logins, peer names, me or none, got {bad:?}");
    }
    if list.iter().any(|r| r.eq_ignore_ascii_case("none")) {
        if list.len() > 1 {
            bail!("reviewers=none asks no one; don't name reviewers with it");
        }
        list.clear();
    }
    if list.len() > MAX_REVIEWERS {
        bail!("reviewers= names at most {MAX_REVIEWERS} reviewers");
    }
    Ok(())
}

/// The reviewers `value` names as `reviewers=` does (commas or spaces between them, a
/// leading `@` left off), checked; empty for `none`.
pub fn parse_reviewers(value: &str) -> Result<Vec<String>> {
    let mut list = Vec::new();
    for name in value.split(|c: char| c == ',' || c.is_whitespace()) {
        add(&mut list, name.trim_start_matches('@'));
    }
    check_reviewers(&mut list)?;
    Ok(list)
}

/// Adds the sources of a `workiq=` value to `list`: `all`, or names from
/// [`WORKIQ_SOURCES`]. Unknown sources are refused, as is `all` next to others.
fn add_workiq(list: &mut Vec<String>, value: &str) -> Result<()> {
    let mut given = Vec::new();
    add(&mut given, &value.to_ascii_lowercase());
    if given.is_empty() {
        bail!("workiq= needs sources: {}", workiq_sources_text());
    }
    if let Some(bad) = given
        .iter()
        .find(|s| *s != "all" && !WORKIQ_SOURCES.iter().any(|(name, _)| name == s))
    {
        bail!(
            "workiq= doesn't know the source {bad:?}; use {}",
            workiq_sources_text()
        );
    }
    add(list, &given.join(","));
    if list.len() > 1 && list.iter().any(|s| s == "all") {
        bail!("workiq=all already searches everything; don't list sources with it");
    }
    Ok(())
}

fn workiq_sources_text() -> String {
    let names: Vec<&str> = WORKIQ_SOURCES.iter().map(|(name, _)| *name).collect();
    format!("all or a list of {}", names.join(", "))
}

/// `first`, then those of `then` that aren't in it.
fn union(first: &[String], then: &[String]) -> Vec<String> {
    let mut all = first.to_vec();
    all.extend(then.iter().filter(|x| !first.contains(x)).cloned());
    all
}

/// The step as it runs as its agent: the agent's skills and MCP servers come first, and
/// its model is the step's unless the step names one.
fn with_agent(step: &Step, skills: &Skills) -> Result<Step> {
    let mut step = step.clone();
    if step.is_workiq() {
        return Ok(step);
    }
    if let Some(name) = &step.agent {
        let agent = skills.agent(name)?;
        step.skills = union(&agent.skills, &step.skills);
        step.mcp = union(&agent.mcp, &step.mcp);
        if step.model.is_none() && !step.approve && step.team.is_none() {
            step.model = agent.model.clone();
        }
    }
    Ok(step)
}

/// Where a step is in its life.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum StepState {
    Waiting,
    Running {
        peer: String,
        /// What the peer's runtime last said it is doing (newer peers report it).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        progress: Option<crate::protocol::Progress>,
    },
    Done {
        peer: String,
        output: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        evidence: Option<Evidence>,
        /// The evidence is signed by `peer` for exactly this output.
        #[serde(default)]
        verified: bool,
        /// The step's commit as a patch (kept out of JSON; saved next to run reports).
        #[serde(skip)]
        patch: Option<String>,
        /// The model that did the work, as `peer` said.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
    },
    Failed {
        peer: Option<String>,
        error: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        evidence: Option<Evidence>,
        #[serde(default)]
        verified: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
    },
    Skipped {
        reason: String,
    },
}

impl StepState {
    pub fn finished(&self) -> bool {
        !matches!(self, StepState::Waiting | StepState::Running { .. })
    }

    pub fn failed(peer: Option<String>, error: String) -> Self {
        StepState::Failed {
            peer,
            error,
            evidence: None,
            verified: false,
            model: None,
        }
    }

    /// The model that did the step's work, if its peer said.
    pub fn model(&self) -> Option<&str> {
        match self {
            StepState::Done { model, .. } | StepState::Failed { model, .. } => model.as_deref(),
            _ => None,
        }
    }

    /// The step's state from what `peer` sent back. Evidence that isn't signed by
    /// `peer`, or missing evidence for requested checks, fails the step.
    pub(crate) fn from_delivered(peer: String, step: &Step, d: Delivered) -> Self {
        let problem = match &d.evidence {
            Some(_) if !d.verified => Some(format!("the evidence isn't validly signed by {peer}")),
            None if !step.checks.is_empty() || step.approve => {
                Some(format!("{peer} sent no evidence (an older version?)"))
            }
            _ => None,
        };
        if d.ok && problem.is_none() {
            return StepState::Done {
                peer,
                output: d.output,
                evidence: d.evidence,
                verified: d.verified,
                patch: d.patch,
                model: d.model,
            };
        }
        StepState::Failed {
            peer: Some(peer),
            error: match problem {
                Some(problem) => format!("{problem}\n\n{}", d.output),
                None => d.output,
            },
            evidence: d.evidence,
            verified: d.verified,
            model: d.model,
        }
    }
}

/// A problem `preflight` found: errors stop the plan from running.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Problem {
    pub step: Option<String>,
    /// The claim it is about, if any (the plan builder shows it there).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claim: Option<String>,
    /// What kind of problem it is, for the plan builder, which says some of them on
    /// the field itself: `todo-task`, `todo-claim`, `unchecked-claim`, `unproven-claim`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<&'static str>,
    pub error: bool,
    pub message: String,
    /// Changes to the plan that resolve it, which the web UI offers as buttons.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub fixes: Vec<Fix>,
}

/// A change to the plan that resolves a problem.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Fix {
    /// What it does, as its button says.
    pub label: String,
    /// The changes to the plan text, in order (ui/plan-text.js makes them).
    pub edits: Vec<Edit>,
}

/// One change to plan text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "do", rename_all = "snake_case")]
pub enum Edit {
    /// Adds an option (`claims=C-1`) to a step; a list option adds its values.
    Add { step: String, option: String },
    /// Removes an option's values (`claims=C-1`) from a step.
    Remove { step: String, option: String },
    /// Puts a new line after a step's line.
    AddLine { after: String, line: String },
    /// Removes a claim's line.
    RemoveClaim { claim: String },
}

/// A step name like `base` that `plan` doesn't use yet (base, base-2, ...).
fn free_id(plan: &Plan, base: &str) -> String {
    let taken = |id: &str| plan.steps.iter().any(|s| s.id == id);
    if !taken(base) {
        return base.to_owned();
    }
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|id| !taken(id))
        .expect("a free name")
}

/// How `step`, which claims `claim` but has no checks, can stop leaving it unverified:
/// let a sign-off prove it instead, or check the step's work with its folder's check.
fn unchecked_fixes(plan: &Plan, peers: &[PeerInfo], step: &Step, claim: &str) -> Vec<Fix> {
    use crate::llm::Access;
    let option = format!("claims={claim}");
    let drop = Edit::Remove {
        step: step.id.clone(),
        option: option.clone(),
    };
    let mut fixes = Vec::new();
    if let Some(s) = plan
        .steps
        .iter()
        .find(|s| s.approve && s.claims.iter().any(|c| c == claim))
    {
        fixes.push(Fix {
            label: format!("Let {} prove {claim}", s.id),
            edits: vec![drop],
        });
    } else if let Some(s) = plan
        .steps
        .iter()
        .find(|s| s.approve && s.after.contains(&step.id))
    {
        fixes.push(Fix {
            label: format!("Let {} prove {claim}", s.id),
            edits: vec![
                drop,
                Edit::Add {
                    step: s.id.clone(),
                    option,
                },
            ],
        });
    } else {
        let id = free_id(plan, "review");
        fixes.push(Fix {
            label: format!("Prove {claim} with your sign-off on {}", step.id),
            edits: vec![
                drop,
                Edit::AddLine {
                    after: step.id.clone(),
                    line: format!(
                        "{id} after {0} approve @me {option} Check the result of {0} and approve it",
                        step.id
                    ),
                },
            ],
        });
    }
    let folder = step.share.as_deref().unwrap_or(crate::shares::DATA_SHARE);
    if let Target::Peer(name) = &step.target
        && let Some(share) = peers
            .iter()
            .find(|p| &p.name == name)
            .and_then(|p| p.shares.iter().find(|s| s.name == folder))
        && share.access >= Access::Full
        && let Some(check) = share.checks.first()
    {
        fixes.push(Fix {
            label: format!("Check {}'s work with the folder's {check} check", step.id),
            edits: vec![Edit::Add {
                step: step.id.clone(),
                option: format!("verify={check}"),
            }],
        });
    }
    fixes
}

/// How `claim`, which no step proves, can be proven by a sign-off, or removed.
fn unproven_fixes(plan: &Plan, claim: &ClaimDef) -> Vec<Fix> {
    let option = format!("claims={}", claim.id);
    let mut fixes = Vec::new();
    if let Some(s) = plan.steps.iter().rev().find(|s| s.approve) {
        fixes.push(Fix {
            label: format!("Let {} prove {}", s.id, claim.id),
            edits: vec![Edit::Add {
                step: s.id.clone(),
                option,
            }],
        });
    } else if let Some(last) = plan.steps.last() {
        let id = free_id(plan, "review");
        fixes.push(Fix {
            label: format!("Prove {} with your sign-off", claim.id),
            edits: vec![Edit::AddLine {
                after: last.id.clone(),
                line: format!(
                    "{id} after {} approve @me {option} Approve if {} holds: {}",
                    last.id, claim.id, claim.text
                ),
            }],
        });
    }
    fixes.push(Fix {
        label: format!("Remove claim {}", claim.id),
        edits: vec![Edit::RemoveClaim {
            claim: claim.id.clone(),
        }],
    });
    fixes
}

/// Why `peer` can't open a pull request in its folder `folder` for you, if it can't: it
/// doesn't share it, the folder doesn't take pull request steps, or you lack edit access.
pub fn pull_request_refusal(peer: &PeerInfo, folder: &str) -> Option<String> {
    use crate::llm::Access;
    match peer.shares.iter().find(|s| s.name == folder) {
        None => Some(format!(
            "{} shares no folder '{folder}' with you, so it can't open its pull request",
            peer.name
        )),
        Some(share) if !share.pull_requests => Some(format!(
            "folder '{folder}' on {} doesn't take pull request steps (its owner allows them with: proofkin share pull-requests {folder}), or its agent is older",
            peer.name
        )),
        Some(share) if share.access < Access::WorkspaceWrite => Some(format!(
            "a pull request step needs edit access in '{folder}' on {}; you have {}",
            peer.name, share.access
        )),
        Some(_) => None,
    }
}

/// Why `peer` can't run `step`, if it can't: provider, folder, checks, access.
fn unable(step: &Step, peer: &PeerInfo) -> Option<String> {
    use crate::llm::Access;
    if step.pr {
        return pull_request_refusal(peer, step.share.as_deref().unwrap_or_default());
    }
    if step.team.is_some() {
        let folder = step.share.as_deref().unwrap_or(crate::shares::DATA_SHARE);
        return match peer.shares.iter().find(|s| s.name == folder) {
            None => Some(format!(
                "{} shares no folder '{folder}' with you, so its team can't take it",
                peer.name
            )),
            Some(share) if !share.team => Some(format!(
                "folder '{folder}' on {} has no team plan (its owner sets one with: proofkin share team {folder} FILE), or its agent is older",
                peer.name
            )),
            Some(share) if share.access < Access::WorkspaceWrite => Some(format!(
                "a team step needs edit access in '{folder}' on {}; you have {}",
                peer.name, share.access
            )),
            Some(_) => None,
        };
    }
    if !peer.capabilities.contains(&step.provider) {
        return Some(format!("{} doesn't serve {}", peer.name, step.provider));
    }
    let folder = step.share.as_deref().unwrap_or(crate::shares::DATA_SHARE);
    let Some(share) = peer.shares.iter().find(|s| s.name == folder) else {
        let shared: Vec<&str> = peer.shares.iter().map(|s| s.name.as_str()).collect();
        return Some(format!(
            "{} shares no folder '{folder}' with you (it shares: {})",
            peer.name,
            if shared.is_empty() {
                "none".into()
            } else {
                shared.join(", ")
            }
        ));
    };
    let missing: Vec<&str> = step
        .checks
        .iter()
        .filter(|c| !crate::spec::is_built_in(c) && !share.checks.contains(c))
        .map(String::as_str)
        .collect();
    if !missing.is_empty() {
        return Some(format!(
            "folder '{folder}' on {} has no check {} (it has: {}); its owner adds checks with share add --check NAME=COMMAND{}",
            peer.name,
            missing.join(", "),
            if share.checks.is_empty() {
                "none".into()
            } else {
                share.checks.join(", ")
            },
            crate::spec::group_hint(&missing)
        ));
    }
    if step.checks.iter().any(|c| !crate::spec::is_built_in(c)) && share.access < Access::Full {
        return Some(format!(
            "verify= with the folder's own checks needs full access, you have {} in '{folder}' on {} (built-in spec:, design: and review: checks need only read access)",
            share.access, peer.name
        ));
    }
    if step.apply.is_some() && !(share.worktrees && share.access >= Access::WorkspaceWrite) {
        return Some(format!(
            "apply= needs edit access in a folder shared with --worktrees; '{folder}' on {} isn't",
            peer.name
        ));
    }
    let missing: Vec<&str> = step
        .mcp
        .iter()
        .filter(|m| !share.mcp.contains(m))
        .map(String::as_str)
        .collect();
    if !missing.is_empty() {
        return Some(format!(
            "folder '{folder}' on {} offers you no MCP server {} (you may use: {}); its owner adds servers with mcp add",
            peer.name,
            missing.join(", "),
            if share.mcp.is_empty() {
                "none".into()
            } else {
                share.mcp.join(", ")
            }
        ));
    }
    if !step.mcp.is_empty() && step.provider == Provider::Codex {
        return Some("mcp= needs copilot or claude; Codex tasks can't use MCP servers".into());
    }
    None
}

/// Who `step` goes to, by name: the connected peer it names, or the members of its squad (also
/// those that are offline, as they may be sent it later).
fn receivers_of(
    step: &Step,
    peers: &[PeerInfo],
    squads: &SquadStore,
    recipients: &Recipients,
) -> Vec<(String, EndpointId)> {
    match &step.target {
        Target::Squad(name) => squads
            .get(name)
            .map(|squad| {
                squad
                    .members
                    .iter()
                    .filter_map(|member| {
                        let id = peers
                            .iter()
                            .find(|p| &p.name == member)
                            .map(|p| p.id)
                            .or_else(|| recipients.trust.get_by_name(member).map(|p| p.id))?;
                        Some((member.clone(), id))
                    })
                    .collect()
            })
            .unwrap_or_default(),
        Target::Workiq => Vec::new(),
        Target::Peer(name) => peers
            .iter()
            .filter(|p| &p.name == name)
            .map(|p| (p.name.clone(), p.id))
            .collect(),
    }
}

/// Checks `plan` against what is known now (connected peers, their folders, checks
/// and access, squads, knowledge and skills) so problems show before anything runs.
pub fn preflight(
    plan: &Plan,
    peers: &[PeerInfo],
    squads: &SquadStore,
    kb: &KnowledgeBase,
    skills: &Skills,
    recipients: &Recipients,
) -> Vec<Problem> {
    let marked = plan.marked(kb);
    // What the steps with `workiq=` bring: the labels a granted peer's grant allows are
    // known now; what an own worker's results hold is known only when the step runs.
    let workiq_steps: HashSet<String> = plan
        .steps
        .iter()
        .filter(|s| s.brings_m365())
        .map(|s| s.id.clone())
        .collect();
    let mut seeds: HashMap<String, Vec<String>> = HashMap::new();
    let mut pending: HashSet<String> = HashSet::new();
    for step in plan.steps.iter().filter(|s| s.brings_m365()) {
        let mut granted: Vec<String> = Vec::new();
        for (_, id) in receivers_of(step, peers, squads, recipients) {
            match recipients.audience_of(&id) {
                Some(Audience::Peer { labels }) => granted = union_labels(&granted, &labels),
                Some(Audience::Local) => {
                    pending.insert(step.id.clone());
                }
                None => {}
            }
        }
        if step.is_workiq() {
            // Run by this agent: what it finds is known only when it runs.
            pending.insert(step.id.clone());
        }
        seeds.insert(step.id.clone(), granted);
    }
    let seeded = plan.marked_with(kb, &seeds);
    let mut problems: Vec<Problem> = plan
        .steps
        .iter()
        .filter(|step| step.prompt == TODO_TASK)
        .map(|step| Problem {
            step: Some(step.id.clone()),
            claim: None,
            code: Some("todo-task"),
            error: true,
            message: format!("the task is still \"{TODO_TASK}\": say what the step should do"),
            fixes: Vec::new(),
        })
        .collect();
    let mut report = |step: Option<&Step>, error: bool, message: String| {
        problems.push(Problem {
            step: step.map(|s| s.id.clone()),
            claim: None,
            code: None,
            error,
            message,
            fixes: Vec::new(),
        })
    };
    for own in &marked.steps {
        let step = &match with_agent(own, skills) {
            Ok(step) => step,
            Err(e) => {
                report(Some(own), true, format!("{e:#}"));
                own.clone()
            }
        };
        let prose = without_code_spans(&step.prompt);
        if let Some(word) = prose.split_whitespace().find(|w| is_option(w)) {
            report(
                Some(step),
                false,
                format!(
                    "{word} is in the task text, so it isn't used as an option: options go before what to do; put it in backticks if it is an example"
                ),
            );
        }
        if let Err(e) = skills.instructions(&step.skills) {
            report(Some(step), true, format!("{e:#}"));
        }
        if let Err(e) = check_topics(kb, &step.context) {
            report(Some(step), true, format!("{e:#}"));
        }
        if step.brings_m365() && !recipients.workiq_account {
            let profile = recipients
                .profile
                .as_ref()
                .map(|p| format!(" --profile {p}"))
                .unwrap_or_default();
            let what = if step.is_workiq() {
                "a @workiq step"
            } else {
                "workiq="
            };
            report(
                Some(step),
                true,
                format!(
                    "{what} needs this profile's Work IQ sign-in, and there is none; run: proofkin auth login workiq{profile}"
                ),
            );
        }
        if step.pr {
            for (error, message) in reviewer_problems(plan, step, recipients) {
                report(Some(step), error, message);
            }
        }
        let candidates: Vec<&PeerInfo> = match &step.target {
            Target::Workiq => continue,
            Target::Peer(name) if name == "me" => {
                if !step.approve {
                    report(
                        Some(step),
                        true,
                        "@me can only approve (approve @me); tasks run on peers".into(),
                    );
                }
                continue;
            }
            Target::Peer(name) => {
                let found: Vec<&PeerInfo> = peers.iter().filter(|p| &p.name == name).collect();
                if found.is_empty() {
                    report(Some(step), true, format!("{name} is not connected"));
                    continue;
                }
                found
            }
            Target::Squad(name) => match squads.get(name) {
                None => {
                    report(Some(step), true, format!("there is no squad #{name}"));
                    continue;
                }
                Some(squad) => {
                    let online: Vec<&PeerInfo> = peers
                        .iter()
                        .filter(|p| squad.members.contains(&p.name))
                        .collect();
                    if online.is_empty() {
                        report(Some(step), true, format!("no member of #{name} is online"));
                        continue;
                    }
                    online
                }
            },
        };
        let via_workiq = !step.workiq.is_empty()
            || ancestors(plan, &step.id)
                .iter()
                .any(|a| workiq_steps.contains(a));
        if step.m365.is_some() || via_workiq {
            let receivers = receivers_of(step, peers, squads, recipients);
            let labels = if step.workiq.is_empty() {
                seeded
                    .steps
                    .iter()
                    .find(|s| s.id == step.id)
                    .and_then(|s| s.m365.clone())
                    .unwrap_or_default()
            } else {
                // Its own results can't be outside its receiver's grant: only what it
                // gets from the steps before it counts.
                let mut labels = step.m365.clone().unwrap_or_default();
                for before in ancestors(plan, &step.id) {
                    if let Some(found) = seeded
                        .steps
                        .iter()
                        .find(|s| s.id == before)
                        .and_then(|s| s.m365.as_ref())
                    {
                        labels = union_labels(&labels, found);
                    }
                }
                labels
            };
            let mut warned = false;
            for (name, id) in receivers {
                let why = if via_workiq {
                    recipients.refusal_for_labels(step, &labels, &name, &id)
                } else {
                    recipients.refusal(step, &name, &id)
                };
                if let Some(why) = why {
                    report(Some(step), true, why);
                }
                let own_worker = matches!(recipients.audience_of(&id), Some(Audience::Local));
                let waits_for_worker = ancestors(plan, &step.id)
                    .iter()
                    .any(|a| pending.contains(a));
                if !own_worker && waits_for_worker && !warned {
                    warned = true;
                    let answered = ancestors(plan, &step.id)
                        .iter()
                        .any(|a| plan.steps.iter().any(|s| &s.id == a && s.is_workiq()));
                    let message = if answered {
                        format!(
                            "{name} gets a Work IQ answer from a @workiq step: its labels are known when the plan runs, and checked when the step is sent; an answer with a citation that has no label stays on this machine"
                        )
                    } else {
                        format!(
                            "{name} gets Microsoft 365 content from a workiq= step on one of your own workers: its labels are known when the plan runs, and checked when the step is sent"
                        )
                    };
                    report(Some(step), false, message);
                }
            }
        }
        if step.approve {
            continue;
        }
        let reasons: Vec<String> = candidates.iter().filter_map(|p| unable(step, p)).collect();
        if reasons.len() == candidates.len() {
            let reasons = reasons.join("; ");
            // Only the agent's MCP servers stand in the way: say so.
            let message = match &step.agent {
                Some(agent) if candidates.iter().any(|p| unable(own, p).is_none()) => {
                    format!("agent {agent} can't work here: {reasons}")
                }
                _ => reasons,
            };
            report(Some(step), true, message);
        }
        // A model or reasoning effort none of its peers allows: each would refuse the
        // step (they say which they allow in their hello; peers that don't say allow any).
        if let Some(model) = &step.model {
            let refusing: Vec<String> = candidates
                .iter()
                .filter_map(|p| {
                    let choice = p.models.get(&step.provider)?;
                    (!choice.allows(model))
                        .then(|| format!("{} allows only {}", p.name, choice.allowed.join(", ")))
                })
                .collect();
            if !refusing.is_empty() && refusing.len() == candidates.len() {
                report(
                    Some(step),
                    false,
                    format!(
                        "model={model} would be refused for {}: {}",
                        step.provider,
                        refusing.join("; ")
                    ),
                );
            }
        }
        if let Some(effort) = &step.effort {
            let refusing: Vec<String> = candidates
                .iter()
                .filter_map(|p| {
                    let choice = p.models.get(&step.provider)?;
                    (!choice.allows_effort(effort))
                        .then(|| format!("{} allows only {}", p.name, choice.efforts.join(", ")))
                })
                .collect();
            if !refusing.is_empty() && refusing.len() == candidates.len() {
                report(
                    Some(step),
                    false,
                    format!(
                        "effort={effort} would be refused for {}: {}",
                        step.provider,
                        refusing.join("; ")
                    ),
                );
            }
        }
    }
    // Each claim needs a step that can prove it: one with checks, or a sign-off.
    for claim in &plan.claims {
        let mut warn =
            |step: Option<&Step>, code: &'static str, message: String, fixes: Vec<Fix>| {
                problems.push(Problem {
                    step: step.map(|s| s.id.clone()),
                    claim: Some(claim.id.clone()),
                    code: Some(code),
                    error: false,
                    message,
                    fixes,
                })
            };
        if claim.text == TODO_CLAIM {
            warn(
                None,
                "todo-claim",
                format!(
                    "claim {} still says \"{TODO_CLAIM}\": say what must be true",
                    claim.id
                ),
                Vec::new(),
            );
        }
        let proving: Vec<&Step> = plan
            .steps
            .iter()
            .filter(|s| s.claims.contains(&claim.id))
            .collect();
        if proving.is_empty() {
            warn(
                None,
                "unproven-claim",
                format!(
                    "Nothing proves {0}, so it will end unverified. Give it to a sign-off or to a step with a check (claims={0} verify=...), or remove it.",
                    claim.id
                ),
                unproven_fixes(plan, claim),
            );
        }
        let signoff = proving.iter().find(|s| s.approve);
        // A team step is proven by its team run's own claims.
        for step in proving
            .iter()
            .filter(|s| !s.approve && s.team.is_none() && s.checks.is_empty())
        {
            let outcome = match signoff {
                Some(s) => format!(
                    "so {} will end unverified even if the sign-off {} is approved",
                    claim.id, s.id
                ),
                None => format!("so {} can only end unverified", claim.id),
            };
            warn(
                Some(step),
                "unchecked-claim",
                format!(
                    "{} claims {} but nothing checks its work, {outcome}. Only a check the peer runs (verify=) or a sign-off can prove a claim.",
                    step.id, claim.id
                ),
                unchecked_fixes(plan, peers, step, &claim.id),
            );
        }
    }
    problems
}

/// Picks the peer for `step`: the named peer, or the least busy online squad member
/// that serves the provider (and the folder, if one is named). Ties go by name.
pub fn choose_peer(
    step: &Step,
    peers: &[PeerInfo],
    squads: &SquadStore,
    busy: &HashMap<String, usize>,
) -> Result<PeerInfo> {
    // The same test as the plan check: provider, folder, checks, access, MCP servers.
    let capable = |p: &&PeerInfo| unable(step, p).is_none();
    match &step.target {
        Target::Workiq => bail!("@workiq steps are asked of Work IQ, never sent to a peer"),
        Target::Peer(name) => {
            let peer = peers
                .iter()
                .find(|p| &p.name == name)
                .ok_or_else(|| anyhow!("{name} is not connected"))?;
            if let Some(why) = unable(step, peer) {
                bail!("{why}");
            }
            Ok(peer.clone())
        }
        Target::Squad(name) => {
            let squad = squads
                .get(name)
                .ok_or_else(|| anyhow!("there is no squad named {name}"))?;
            let online: Vec<&PeerInfo> = peers
                .iter()
                .filter(|p| squad.members.contains(&p.name))
                .collect();
            let able: Vec<&PeerInfo> = online.iter().copied().filter(capable).collect();
            // Members that allow the step's model and reasoning effort come first (their
            // hello says which; one that doesn't say allows any): the others would turn
            // it down.
            let allows = |p: &&PeerInfo| {
                p.models.get(&step.provider).is_none_or(|choice| {
                    choice.admits(step.model.as_deref(), step.effort.as_deref())
                })
            };
            let pool: Vec<&PeerInfo> = if able.iter().any(allows) {
                able.into_iter().filter(allows).collect()
            } else {
                able
            };
            pool.into_iter()
                .min_by_key(|p| (busy.get(&p.name).copied().unwrap_or(0), p.name.clone()))
                .cloned()
                .ok_or_else(|| {
                    let reasons: Vec<String> =
                        online.iter().filter_map(|p| unable(step, p)).collect();
                    anyhow!(
                        "no online member of squad {name} can run it{}",
                        if reasons.is_empty() {
                            String::new()
                        } else {
                            format!(": {}", reasons.join("; "))
                        }
                    )
                })
        }
    }
}

/// A finished step a later one builds on: what it was asked, and what it returned.
#[derive(Debug, Clone, Copy)]
pub struct Earlier<'a> {
    pub id: &'a str,
    pub peer: &'a str,
    /// The step's own words ([`Step::own_text`]).
    pub task: &'a str,
    pub output: &'a str,
}

/// `text` cut to `max` characters, saying so.
fn cut_text(text: &str, max: usize) -> String {
    let mut cut: String = text.chars().take(max).collect();
    if text.chars().count() > max {
        cut += "\n[... truncated]";
    }
    cut
}

/// The prompt a step runs with: the tasks and results of its prerequisites, then its
/// task. Those (and any knowledge) are external content: other peers or models wrote
/// them, so they are wrapped and the prompt starts with the rule for them.
pub fn step_prompt(step: &Step, deps: &[Earlier]) -> String {
    step_prompt_with(step, deps, "")
}

/// The issue a step works on, for its prompt (see [`step_prompt_with`]): its tracker's
/// text of it, as external content (people wrote it).
pub fn issue_section(step: &Step, text: &str) -> String {
    let source = match step.issue {
        Some(n) => format!("issue:{n}"),
        None => "issue".to_owned(),
    };
    format!(
        "The issue this task works on:\n{}\n",
        crate::boundary::wrap(&source, text)
    )
}

/// [`step_prompt`] with `work` in front of the task: the issue the step works on (see
/// [`issue_section`]) and the Microsoft 365 section of what its `workiq=` search found,
/// either or both (empty if none).
pub fn step_prompt_with(step: &Step, deps: &[Earlier], work: &str) -> String {
    if deps.is_empty() && work.is_empty() {
        return crate::boundary::guard(&step.prompt);
    }
    let mut prompt = String::new();
    if !deps.is_empty() {
        prompt += "Results of the earlier steps this task builds on:\n";
    }
    for dep in deps {
        let (id, peer) = (dep.id, dep.peer);
        let output = cut_text(dep.output, MAX_DEP_CHARS);
        // What it was asked, so a review or a follow-up knows what the result answers.
        let text = match dep.task.trim() {
            "" => output,
            task => format!(
                "Its task:\n{}\n\nIts result:\n{output}",
                cut_text(task, MAX_DEP_TASK_CHARS)
            ),
        };
        prompt += &format!(
            "\n### Step {id} (done by {peer})\n{}\n",
            crate::boundary::wrap(&format!("step:{id} peer:{peer}"), &text)
        );
    }
    if !work.is_empty() {
        prompt += &format!("\n{work}");
    }
    prompt += &format!("\n---\nYour task:\n{}", step.prompt);
    crate::boundary::guard(&prompt)
}

/// What a person reads in an approval: the prompt without the rule for models and
/// without the `<external_content>` envelope lines around earlier results.
fn approval_text(prompt: &str) -> String {
    let tag = crate::boundary::TAG;
    let (open, close) = (format!("<{tag} source="), format!("</{tag}>"));
    prompt
        .strip_prefix(crate::boundary::DIRECTIVE)
        .unwrap_or(prompt)
        .lines()
        .filter(|l| !l.starts_with(&open) && l.trim_end() != close)
        .collect::<Vec<_>>()
        .join("\n")
}

/// The steps `id` waits for, directly or through others.
fn ancestors(plan: &Plan, id: &str) -> std::collections::BTreeSet<String> {
    let mut found = std::collections::BTreeSet::new();
    let mut todo = vec![id.to_owned()];
    while let Some(current) = todo.pop() {
        if let Some(step) = plan.steps.iter().find(|s| s.id == current) {
            for dep in &step.after {
                if found.insert(dep.clone()) {
                    todo.push(dep.clone());
                }
            }
        }
    }
    found
}

/// The steps whose work pull request step `pr` may open: those it waits for (directly or
/// through others) that a model does in its folder, on its peer or on a squad (whose
/// member may be its peer).
fn pr_sources<'a>(plan: &'a Plan, pr: &Step) -> Vec<&'a Step> {
    let before = ancestors(plan, &pr.id);
    plan.steps
        .iter()
        .filter(|s| before.contains(&s.id) && !s.pr && !s.approve && s.team.is_none())
        .filter(|s| s.share == pr.share)
        .filter(|s| s.target == pr.target || matches!(s.target, Target::Squad(_)))
        .collect()
}

/// Whether the work of `step` starts from that of step `id`: its `apply=` chain reaches it.
fn applies(plan: &Plan, step: &Step, id: &str) -> bool {
    let mut seen = HashSet::new();
    let mut at = step.apply.as_deref();
    while let Some(applied) = at {
        if applied == id {
            return true;
        }
        if !seen.insert(applied) {
            break;
        }
        at = plan
            .steps
            .iter()
            .find(|s| s.id == applied)
            .and_then(|s| s.apply.as_deref());
    }
    false
}

/// Those of `steps` whose work none of the others starts from: one means its branch has
/// the work of all of them (they are on its `apply=` chain).
fn chain_ends<'a>(plan: &Plan, steps: &[&'a Step]) -> Vec<&'a Step> {
    steps
        .iter()
        .copied()
        .filter(|s| {
            !steps
                .iter()
                .any(|other| other.id != s.id && applies(plan, other, &s.id))
        })
        .collect()
}

/// What pull request step `step` asks its peer for: to open it from the branch of the work
/// before it in its folder there (that of the step whose branch has all of it), titled
/// with the step's own words, else that step's.
fn pr_ask(
    plan: &Plan,
    step: &Step,
    states: &BTreeMap<String, StepState>,
) -> Result<crate::pulls::PullRequestAsk> {
    let Target::Peer(peer) = &step.target else {
        bail!("a pull request step names its peer: @peer:folder");
    };
    let folder = step.share.as_deref().unwrap_or_default();
    let made: Vec<(&Step, &crate::evidence::GitCommit)> = pr_sources(plan, step)
        .into_iter()
        .filter_map(|s| match &states[&s.id] {
            StepState::Done {
                peer: by,
                evidence: Some(evidence),
                ..
            } if by == peer => evidence
                .git
                .as_ref()
                .filter(|g| g.commit.is_some())
                .map(|g| (s, g)),
            _ => None,
        })
        .collect();
    let steps: Vec<&Step> = made.iter().map(|(s, _)| *s).collect();
    let ends = chain_ends(plan, &steps);
    let tip = match ends.as_slice() {
        [] => bail!(
            "no step before it committed work to '{folder}' on {peer}: there is nothing to open a pull request with"
        ),
        [tip] => *tip,
        many => bail!(
            "steps {} committed to '{folder}' on {peer} on separate branches: a step has another's work only with apply=",
            many.iter()
                .map(|s| s.id.as_str())
                .collect::<Vec<_>>()
                .join(" and ")
        ),
    };
    let git = made
        .iter()
        .find(|(s, _)| s.id == tip.id)
        .map(|(_, g)| *g)
        .expect("a step with a commit");
    // Without words of its own, the title is that of the step that started the work.
    let first = std::iter::successors(Some(tip), |s| {
        s.apply
            .as_ref()
            .and_then(|a| plan.steps.iter().find(|x| &x.id == a))
    })
    .take(plan.steps.len())
    .last()
    .unwrap_or(tip);
    let words = if step.own_text().trim().is_empty() {
        first.own_text()
    } else {
        step.own_text()
    };
    // The sign-offs before it, as their approvers signed them: the pull request lists them.
    let before = ancestors(plan, &step.id);
    let signoffs = plan
        .steps
        .iter()
        .filter(|s| s.approve && before.contains(&s.id))
        .filter_map(|s| match &states[&s.id] {
            StepState::Done {
                output,
                evidence: Some(evidence),
                verified: true,
                ..
            } if evidence.kind == crate::evidence::EvidenceKind::Approval
                && evidence.approved == Some(true) =>
            {
                Some(crate::pulls::SignOff {
                    step: s.id.clone(),
                    output: output.clone(),
                    evidence: evidence.clone(),
                })
            }
            _ => None,
        })
        .collect();
    Ok(crate::pulls::PullRequestAsk {
        branch: git.branch.clone(),
        commit: git.commit.clone().unwrap_or_default(),
        title: crate::llm::title_of(words),
        signoffs,
        reviewers: Vec::new(),
    })
}

/// The GitHub accounts a pull request asks to review (see [`Step::reviewers`]): those
/// `names` says, else those of the people whose sign-offs it carries (`signoffs`), as this
/// agent (`own`) and its allowlist know them. A name without a known account is left
/// out; the plan check warned about it.
fn reviewers_for(
    names: Option<&[String]>,
    signoffs: &[crate::pulls::SignOff],
    own: &EndpointId,
    recipients: &Recipients,
) -> Vec<String> {
    let account_of = |id: &EndpointId| {
        if id == own {
            recipients.github.clone()
        } else {
            recipients.trust.get(id).and_then(|p| p.github.clone())
        }
    };
    let found: Vec<Option<String>> = match names {
        Some(names) => names
            .iter()
            .map(|name| {
                if name == "me" {
                    recipients.github.clone()
                } else if let Some(peer) = recipients.trust.get_by_name(name) {
                    peer.github.clone()
                } else {
                    Some(name.clone()).filter(|n| crate::github::login_ok(n))
                }
            })
            .collect(),
        None => signoffs
            .iter()
            .map(|s| {
                s.evidence
                    .signer_id
                    .parse::<EndpointId>()
                    .ok()
                    .and_then(|id| account_of(&id))
            })
            .collect(),
    };
    let mut logins: Vec<String> = Vec::new();
    for login in found.into_iter().flatten() {
        if !logins.iter().any(|l| l.eq_ignore_ascii_case(&login)) {
            logins.push(login);
        }
    }
    logins.truncate(MAX_REVIEWERS);
    logins
}

/// What the plan check says about who pull request step `step` asks to review, each with
/// whether it is an error: a name in its `reviewers=` (or a person signing off before it,
/// when it names none) whose GitHub account isn't known isn't asked; a name that can't be
/// anyone is an error.
fn reviewer_problems(plan: &Plan, step: &Step, recipients: &Recipients) -> Vec<(bool, String)> {
    let profile = recipients
        .profile
        .as_ref()
        .map(|p| format!(" --profile {p}"))
        .unwrap_or_default();
    let not_signed_in = |what: &str| {
        format!(
            "{what}, but this profile isn't signed in to GitHub, so you aren't asked to review the pull request; sign in with: proofkin github login --gh-user <login>{profile}"
        )
    };
    let no_account = |what: &str, peer: &str| {
        format!(
            "{what}, but the allowlist has no GitHub account for {peer}, so {peer} isn't asked to review the pull request; set one with: proofkin peers github {peer} <login>{profile}"
        )
    };
    let mut problems = Vec::new();
    match &step.reviewers {
        Some(names) => {
            for name in names {
                if name == "me" {
                    if recipients.github.is_none() {
                        problems.push((false, not_signed_in("reviewers= names me")));
                    }
                } else if let Some(peer) = recipients.trust.get_by_name(name) {
                    if peer.github.is_none() {
                        let what = format!("reviewers= names {name}");
                        problems.push((false, no_account(&what, name)));
                    }
                } else if !crate::github::login_ok(name) {
                    problems.push((
                        true,
                        format!(
                            "reviewers= names {name:?}, who isn't on the allowlist and can't be a GitHub login"
                        ),
                    ));
                }
            }
        }
        None => {
            let before = ancestors(plan, &step.id);
            for s in plan
                .steps
                .iter()
                .filter(|s| s.approve && before.contains(&s.id))
            {
                let Target::Peer(name) = &s.target else {
                    continue;
                };
                let message = if name == "me" {
                    if recipients.github.is_some() {
                        continue;
                    }
                    not_signed_in(&format!("you sign off in step '{}'", s.id))
                } else if recipients
                    .trust
                    .get_by_name(name)
                    .is_some_and(|p| p.github.is_none())
                {
                    no_account(&format!("{name} signs off in step '{}'", s.id), name)
                } else {
                    continue;
                };
                problems.push((
                    false,
                    format!("{message}; or ask no one with reviewers=none"),
                ));
            }
        }
    }
    problems
}

/// Work of a finished run that a pull request can be opened from, as [`run_work`] finds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunWork {
    /// The step whose branch has the work: no other step of the run builds on it.
    pub step: String,
    /// The peer that holds the clone, by its name on the allowlist, and the folder.
    pub peer: String,
    pub folder: String,
    pub branch: String,
    pub commit: String,
    /// The pull request's title unless you give one: that of the step that started the work.
    pub title: String,
}

/// The run's steps as a plan, to follow their `after` and `apply=` links.
fn plan_of(run: &crate::runs::RunRecord) -> Plan {
    Plan {
        claims: Vec::new(),
        steps: run.steps.iter().map(|s| s.step.clone()).collect(),
        track: None,
    }
}

/// The work of finished run `run` a pull request can be opened from: each signed commit a
/// model's step made in a peer's folder that no other step of the run builds on (through
/// `apply=`), unless a pull request step of the run saw it merged.
pub fn run_work(run: &crate::runs::RunRecord) -> Vec<RunWork> {
    let plan = plan_of(run);
    let made: Vec<(&Step, &str, &str, &crate::evidence::GitCommit)> = run
        .steps
        .iter()
        .filter(|r| !r.step.pr && !r.step.approve && r.step.team.is_none())
        .filter_map(|r| match &r.state {
            StepState::Done {
                peer,
                evidence: Some(e),
                verified: true,
                ..
            } => {
                let git = e.git.as_ref().filter(|g| g.commit.is_some())?;
                Some((&r.step, peer.as_str(), e.share.as_deref()?, git))
            }
            _ => None,
        })
        .collect();
    let steps: Vec<&Step> = made.iter().map(|m| m.0).collect();
    let ends = chain_ends(&plan, &steps);
    let merged: HashSet<String> = run
        .steps
        .iter()
        .filter(|r| r.step.pr && matches!(r.state, StepState::Done { .. }))
        .flat_map(|r| pr_sources(&plan, &r.step))
        .map(|s| s.id.clone())
        .collect();
    made.into_iter()
        .filter(|(step, ..)| ends.iter().any(|e| e.id == step.id) && !merged.contains(&step.id))
        .map(|(step, peer, folder, git)| {
            let first = std::iter::successors(Some(step), |s| {
                s.apply
                    .as_ref()
                    .and_then(|a| plan.steps.iter().find(|x| &x.id == a))
            })
            .take(plan.steps.len())
            .last()
            .unwrap_or(step);
            RunWork {
                step: step.id.clone(),
                peer: peer.to_owned(),
                folder: folder.to_owned(),
                branch: git.branch.clone(),
                commit: git.commit.clone().unwrap_or_default(),
                title: crate::llm::title_of(first.own_text()),
            }
        })
        .collect()
}

/// What to ask the agent that holds the clone, to open a pull request with the work of
/// step `step` of finished run `run` (see [`run_work`]): its branch and commit, titled
/// `title` (else as the work), with the run's sign-offs that came after any of that work,
/// asking `reviewers` (as `reviewers=` names them; else those who signed off) to review it.
pub fn run_pull_request(
    run: &crate::runs::RunRecord,
    step: &str,
    title: Option<&str>,
    reviewers: Option<&[String]>,
    own: &EndpointId,
    recipients: &Recipients,
) -> Result<(RunWork, crate::pulls::PullRequestAsk)> {
    let work = run_work(run)
        .into_iter()
        .find(|w| w.step == step)
        .with_context(|| {
            format!(
                "step {step} of run {} has no work to open a pull request with: it committed nothing in a peer's folder, another step builds on its work, or a pull request step of the run merged it",
                run.id
            )
        })?;
    let plan = plan_of(run);
    // Its work: the step and those whose work it starts from.
    let mut chain: HashSet<&str> = HashSet::new();
    let mut at = plan.steps.iter().find(|s| s.id == step);
    while let Some(s) = at {
        if !chain.insert(s.id.as_str()) {
            break;
        }
        at = s
            .apply
            .as_ref()
            .and_then(|a| plan.steps.iter().find(|x| &x.id == a));
    }
    let signoffs: Vec<crate::pulls::SignOff> = run
        .steps
        .iter()
        .filter(|r| r.step.approve)
        .filter(|r| {
            let before = ancestors(&plan, &r.step.id);
            chain.iter().any(|c| before.contains(*c))
        })
        .filter_map(|r| match &r.state {
            StepState::Done {
                output,
                evidence: Some(evidence),
                verified: true,
                ..
            } if evidence.kind == crate::evidence::EvidenceKind::Approval
                && evidence.approved == Some(true) =>
            {
                Some(crate::pulls::SignOff {
                    step: r.step.id.clone(),
                    output: output.clone(),
                    evidence: evidence.clone(),
                })
            }
            _ => None,
        })
        .collect();
    let title = title
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(crate::llm::title_of)
        .unwrap_or_else(|| work.title.clone());
    let reviewers = reviewers_for(reviewers, &signoffs, own, recipients);
    let ask = crate::pulls::PullRequestAsk {
        branch: work.branch.clone(),
        commit: work.commit.clone(),
        title,
        signoffs,
        reviewers,
    };
    Ok((work, ask))
}

/// A plan checked and ready to run as `profile`'s agent (see [`start`]).
pub struct ReadyRun {
    text: String,
    plan: Plan,
    ready: Plan,
    squads: SquadStore,
    kb: KnowledgeBase,
    recipients: Recipients,
    profile: crate::profile::Profile,
    work: Option<Arc<dyn WorkContext>>,
}

/// Checks plan `text` as `/plan` and the web UI do (preflight against connected
/// peers, folders, checks, MCP servers, squads, knowledge and skills) and prepares
/// it; fails with every problem if it can't run now.
pub fn start(agent: &Agent, profile: &crate::profile::Profile, text: &str) -> Result<ReadyRun> {
    let plan = Plan::parse(text)?;
    let kb = profile.knowledge();
    let skills = profile.skills();
    let squads = SquadStore::load(&profile.squads_path()).unwrap_or_default();
    let recipients = Recipients::of(agent, profile);
    let errors: Vec<String> = preflight(&plan, &agent.peers(), &squads, &kb, &skills, &recipients)
        .into_iter()
        .filter(|p| p.error)
        .map(|p| match p.step {
            Some(step) => format!("step {step}: {}", p.message),
            None => p.message,
        })
        .collect();
    if !errors.is_empty() {
        bail!("{}", errors.join("; "));
    }
    let ready = plan.prepare(&kb, &skills)?;
    let work = work_context(profile, &plan)?;
    Ok(ReadyRun {
        text: text.to_owned(),
        plan,
        ready,
        squads,
        kb,
        recipients,
        profile: profile.clone(),
        work,
    })
}

impl ReadyRun {
    /// Runs it (tracked on GitHub if the profile has a tracker), then saves its
    /// evidence record and the knowledge its verified steps produced. Returns the run
    /// (saved unless saving failed, which is printed, like tracking problems).
    pub async fn run(
        self,
        agent: &Agent,
        mut on_update: impl FnMut(&str, &StepState),
    ) -> crate::runs::RunRecord {
        let started = crate::runs::unix_now();
        let run_id = crate::runs::new_run_id();
        let tracking =
            match crate::tracker::start(&self.profile, &self.plan, &run_id, agent.name()).await {
                Ok(tracking) => tracking,
                Err(e) => {
                    eprintln!("the run isn't tracked on GitHub: {e:#}");
                    None
                }
            };
        if let Some(t) = &tracking {
            for line in t.tracked.describe() {
                eprintln!("{line}");
            }
        }
        let ctx = RunContext {
            run_id: Some(run_id.clone()),
            chain: Vec::new(),
            issues: tracking
                .as_ref()
                .map(|t| t.refs.clone())
                .unwrap_or_default(),
            issue_texts: tracking
                .as_ref()
                .map(|t| t.texts.clone())
                .unwrap_or_default(),
            recipients: self.recipients.clone(),
            work_context: self.work.clone(),
            work_log: WorkLog::default(),
            ..RunContext::default()
        };
        let states = run_plan_with(agent, &self.squads, &self.ready, &ctx, |step, state| {
            if let Some(t) = &tracking {
                t.update(step, state);
            }
            on_update(step, state);
        })
        .await;
        let mut run = crate::runs::RunRecord::new(
            agent.name(),
            &agent.id().to_string(),
            &self.text,
            &self.plan,
            &states,
            started,
        );
        run.id = run_id;
        run.peer_ids = crate::runs::peer_ids(agent);
        run.workiq = ctx.work_log.records();
        if let Some(t) = tracking {
            let known = t.tracked.problems.len();
            let tracked = t.finish(&run).await;
            for problem in tracked.problems.iter().skip(known) {
                eprintln!("tracker: {problem}");
            }
            run.tracked = Some(tracked);
        }
        if let Err(e) = run.save(&self.profile.runs_dir()) {
            eprintln!("could not save the run: {e:#}");
        }
        save_knowledge(
            &self.kb,
            &with_actual_labels(&self.plan, &ctx.work_log),
            &states,
            &run.id,
        );
        run
    }
}

/// [`start`] then [`ReadyRun::run`].
pub async fn execute(
    agent: &Agent,
    profile: &crate::profile::Profile,
    text: &str,
    on_update: impl FnMut(&str, &StepState),
) -> Result<crate::runs::RunRecord> {
    Ok(start(agent, profile, text)?.run(agent, on_update).await)
}

/// What a plan run is part of: its id and, for a lead's team run, the agents the work
/// came down from (see [`run_plan_with`]).
#[derive(Debug, Clone, Default)]
pub struct RunContext {
    /// Sent with each step, so its peer can tell (and show) whose work it is.
    pub run_id: Option<String>,
    /// The agents the work came down from, top first: no step runs on one of them, and
    /// team steps pass the chain on (with this agent added), so work never loops.
    pub chain: Vec<EndpointId>,
    /// The issues each step's task carries, by step (`owner/repo#N`, see
    /// [`crate::tracker`]): its commit references them, a pull request closes them.
    pub issues: HashMap<String, Vec<String>>,
    /// The text of the issue each step that names one (`issue=#N`) works on, by step
    /// (see [`crate::tracker::Tracking::texts`]): put in front of its task as external
    /// content, so the plan needn't repeat it and the model needn't look it up.
    pub issue_texts: HashMap<String, String>,
    /// Who may receive Microsoft 365 content: each marked step is checked against it
    /// (with the allowlist as it is then) before its task is sent.
    pub recipients: Recipients,
    /// Where `workiq=` steps search Microsoft 365 (see [`work_context`]); none if no step
    /// uses it.
    pub work_context: Option<Arc<dyn WorkContext>>,
    /// What those searches found, by step: for the run record, kept on this machine.
    pub work_log: WorkLog,
    /// The run as it was before its agent restarted (see [`Resume`]); empty for a new run.
    pub resume: Resume,
    /// Told each step's work as it is sent, so the run can go on after a restart (see
    /// [`SentStep`]); with one, that work is also left running when the agent stops.
    pub journal: Option<Journal>,
}

/// A step's work as it was sent: a run keeps it to go on after its agent restarts,
/// waiting for the same work instead of asking for it again (see [`Resume`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SentStep {
    /// Who it went to: a peer's name (or "you", this agent's own user).
    pub peer: String,
    /// Its endpoint id (none: this agent's own user).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_id: Option<String>,
    /// The request's id on the wire.
    pub task_id: u64,
    #[serde(flatten)]
    pub work: SentWork,
}

/// What [`SentStep`] asked for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SentWork {
    /// A task, exactly as sent: a peer that still has it (running, or done) knows it again.
    Task {
        task: Box<LlmTask>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        origin: Option<Origin>,
    },
    /// An approval request.
    Approval {
        title: String,
        text: String,
        /// The approver may ask for changes.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        changes: bool,
    },
}

/// A run going on after its agent restarted: its steps that had finished, as they ended
/// (a done step with its patch), and the work it was waiting for. Finished steps aren't
/// run again; the work is waited for again ([`Agent::rejoin_task`]); the other steps
/// start when their turn comes.
#[derive(Debug, Clone, Default)]
pub struct Resume {
    pub finished: BTreeMap<String, StepState>,
    pub sent: BTreeMap<String, SentStep>,
}

/// Told a step's id and its work as it is sent (see [`Journal`]).
pub type JournalFn = dyn Fn(&str, &SentStep) + Send + Sync;

/// Where a run tells each step's work as it is sent (see [`RunContext::journal`]).
#[derive(Clone)]
pub struct Journal(pub Arc<JournalFn>);

impl std::fmt::Debug for Journal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Journal")
    }
}

/// What a step's Work IQ search found, for the run record: the sources it got, how many
/// results were kept and withheld, and the ids of Work IQ's responses.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkRecord {
    pub sources: Vec<Source>,
    /// The labels of the kept results (`unlabeled`, `unknown`): what the step's output may
    /// hold, as later steps' send-time checks see it.
    pub labels: Vec<String>,
    pub kept: usize,
    pub withheld: usize,
    pub request_ids: String,
}

/// The [`WorkRecord`]s of a run, by step.
#[derive(Debug, Clone, Default)]
pub struct WorkLog(Arc<Mutex<BTreeMap<String, WorkRecord>>>);

impl WorkLog {
    fn put(&self, step: &str, record: WorkRecord) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(step.to_owned(), record);
    }

    /// What was found so far, by step.
    pub fn records(&self) -> BTreeMap<String, WorkRecord> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// What `plan`'s `workiq=` steps search with: Work IQ's hosted service for `profile`'s
/// account, or none if no step uses `workiq=`. Fails if the account can't be used.
pub fn work_context(
    profile: &crate::profile::Profile,
    plan: &Plan,
) -> Result<Option<Arc<dyn WorkContext>>> {
    if plan.steps.iter().all(|s| !s.brings_m365()) {
        return Ok(None);
    }
    Ok(Some(Arc::new(ClientWork::for_profile(profile)?)))
}

/// Runs `plan` with `agent`, reporting every state change to `on_update`, and
/// returns each step's final state. Dropping the future cancels running steps.
pub async fn run_plan(
    agent: &Agent,
    squads: &SquadStore,
    plan: &Plan,
    on_update: impl FnMut(&str, &StepState),
) -> BTreeMap<String, StepState> {
    run_plan_with(agent, squads, plan, &RunContext::default(), on_update).await
}

/// How busy each of `peers` is, by name: the tasks it reports running or queueing
/// for other agents, plus those this agent gave it and is waiting for.
fn load(agent: &Agent, peers: &[PeerInfo]) -> HashMap<String, usize> {
    peers
        .iter()
        .map(|p| {
            let others = p
                .load
                .map_or(0, |l| (l.running + l.queued).saturating_sub(l.yours));
            (p.name.clone(), others as usize + agent.pending_to(p.id))
        })
        .collect()
}

/// Why a squad step may move to another member: it never ran on its peer (refused,
/// or the peer stopped first), or the peer dropped and didn't come back.
pub fn movable(result: &Result<Delivered>) -> Option<String> {
    match result {
        Ok(d) if d.refused => Some(
            d.output
                .lines()
                .next()
                .unwrap_or("it refused the step")
                .to_owned(),
        ),
        Err(e) if e.downcast_ref::<crate::agent::PeerGone>().is_some() => Some(format!("{e:#}")),
        _ => None,
    }
}

/// A step's work: its result, to come.
type Work = std::pin::Pin<Box<dyn Future<Output = Result<Delivered>> + Send>>;

/// The work of `step`, sent before this agent restarted, waited for again (see
/// [`Resume`]): who it went to, and its result to come. It is left running if this
/// agent stops again.
fn rejoin(
    agent: &Agent,
    sent: SentStep,
    progress: &tokio::sync::mpsc::UnboundedSender<(String, Progress)>,
    step: &str,
) -> (String, Work) {
    let peer = sent
        .peer_id
        .as_deref()
        .and_then(|id| id.parse::<EndpointId>().ok());
    agent.keep_on_exit(sent.task_id);
    let work: Work = match (sent.work, peer) {
        (SentWork::Task { task, origin }, Some(peer)) => {
            let delegation = agent.rejoin_task(peer, sent.task_id, *task, origin);
            let (tx, id) = (progress.clone(), step.to_owned());
            Box::pin(delegation.finish(move |p| {
                let _ = tx.send((id.clone(), p));
            }))
        }
        (SentWork::Task { .. }, None) => {
            Box::pin(async { Err(anyhow!("it isn't known which peer has its task")) })
        }
        (
            SentWork::Approval {
                title,
                text,
                changes,
            },
            approver,
        ) => {
            let (agent, task_id) = (agent.clone(), sent.task_id);
            let ask = crate::agent::ApprovalAsk {
                title,
                text,
                preview: BTreeMap::new(),
                changes,
            };
            Box::pin(async move { agent.rejoin_approval(approver, task_id, &ask).await })
        }
    };
    (sent.peer, work)
}

/// `work`, cancelled (by dropping it) once `secs` have passed.
async fn within<T>(secs: Option<u64>, work: impl Future<Output = Result<T>>) -> Result<T> {
    match secs {
        None => work.await,
        Some(secs) => tokio::time::timeout(std::time::Duration::from_secs(secs), work)
            .await
            .unwrap_or_else(|_| {
                Err(anyhow!(
                    "cancelled: it took longer than its timeout={}",
                    timeout_text(secs)
                ))
            }),
    }
}

/// The owner's allowlist as it is at this moment, for a check at send time.
fn fresh_recipients(agent: &Agent, ctx: &RunContext) -> Recipients {
    Recipients {
        trust: agent.trusted(),
        ..ctx.recipients.clone()
    }
}

/// Why `step`'s Microsoft 365 content may not be sent to `name` (`id`) now, if it may not:
/// the plan check's test again, against the allowlist as it is at this moment. A step that
/// gets content from `workiq=` steps (its own, or those it waits for) is checked against the
/// labels those found in this run, joined with the labels it was marked with.
fn m365_refusal(
    agent: &Agent,
    ctx: &RunContext,
    plan: &Plan,
    step: &Step,
    name: &str,
    id: &EndpointId,
) -> Option<String> {
    let recipients = fresh_recipients(agent, ctx);
    let earlier = ancestors(plan, &step.id);
    let via_workiq = step.brings_m365()
        || plan
            .steps
            .iter()
            .any(|s| s.brings_m365() && earlier.contains(&s.id));
    if !via_workiq {
        step.m365.as_ref()?;
        return recipients.refusal(step, name, id);
    }
    let found = ctx.work_log.records();
    let mut labels = step.m365.clone().unwrap_or_default();
    for before in &earlier {
        if let Some(record) = found.get(before) {
            labels = union_labels(&labels, &record.labels);
        }
    }
    recipients.refusal_for_labels(step, &labels, name, id)
}

/// Searches Work IQ for `step` (which goes to `peer`) and returns the prompt section for
/// what `peer` may receive. The labels kept and the sources are logged in `ctx`.
async fn search_work(
    agent: &Agent,
    ctx: &RunContext,
    step: &Step,
    peer: &PeerInfo,
) -> Result<String> {
    let Some(work) = &ctx.work_context else {
        bail!("Work IQ isn't set up for this run");
    };
    let Some((strategy, capabilities)) = step.workiq_request() else {
        return Ok(String::new());
    };
    let audience = fresh_recipients(agent, ctx)
        .audience_of(&peer.id)
        .with_context(|| format!("{} has no Microsoft 365 grant", peer.name))?;
    let purpose = Purpose::Step {
        run: ctx.run_id.clone().unwrap_or_default(),
        step: step.id.clone(),
    };
    let call = match &audience {
        Audience::Local => CallContext::local(purpose),
        Audience::Peer { labels } => CallContext::peer(&peer.name, labels.clone(), purpose),
    };
    let query = step.workiq_query();
    let result = within(
        step.timeout,
        work.retrieve(&call, &query, strategy, &capabilities),
    )
    .await?;
    let section = retrieval_section(&result, &audience, DEFAULT_CAP)?;
    ctx.work_log.put(
        &step.id,
        WorkRecord {
            sources: section.sources.clone(),
            labels: section.labels.clone(),
            kept: section.kept,
            withheld: section.withheld.total(),
            request_ids: result.ids.to_string(),
        },
    );
    Ok(section.text)
}

/// Asks Work IQ for `step`, a `@workiq` step, as the owner's own agent: `question` is the
/// step's words with the results of the steps before it, and the answer with its cited sources
/// is the step's output. What the citations carry is logged in `ctx` for the run record.
fn ask_workiq(
    ctx: &RunContext,
    step: &Step,
    question: String,
) -> impl Future<Output = Result<Delivered>> + Send + 'static {
    let work = ctx.work_context.clone();
    let log = ctx.work_log.clone();
    let purpose = Purpose::Step {
        run: ctx.run_id.clone().unwrap_or_default(),
        step: step.id.clone(),
    };
    let (id, agent_id) = (step.id.clone(), step.agent.clone());
    async move {
        let Some(work) = work else {
            bail!("Work IQ isn't set up for this run");
        };
        let call = CallContext::local(purpose);
        let result = work.ask(&call, &question, agent_id.as_deref()).await?;
        let section = answer_section(&result, &Audience::Local, DEFAULT_CAP)?;
        log.put(
            &id,
            WorkRecord {
                sources: section.sources.clone(),
                labels: section.labels.clone(),
                kept: section.kept,
                withheld: section.withheld.total(),
                request_ids: result.ids.to_string(),
            },
        );
        Ok(Delivered {
            ok: true,
            output: section.text,
            evidence: None,
            patch: None,
            verified: false,
            refused: false,
            model: None,
        })
    }
}

/// `plan` with the labels each `workiq=` step's results turned out to have added to its
/// mark, so what a step saves as knowledge is marked with them.
pub fn with_actual_labels(plan: &Plan, log: &WorkLog) -> Plan {
    let found = log.records();
    let mut plan = plan.clone();
    for step in &mut plan.steps {
        if let Some(record) = found.get(&step.id) {
            step.m365 = Some(union_labels(
                step.m365.as_deref().unwrap_or_default(),
                &record.labels,
            ));
        }
    }
    plan
}

/// The peer to run task `step` on: as [`choose_peer`], but never one the work came
/// down from (`ctx`), nor a squad member the step already moved away from.
fn pick_peer(
    agent: &Agent,
    step: &Step,
    squads: &SquadStore,
    ctx: &RunContext,
    tried: Option<&HashSet<String>>,
) -> Result<PeerInfo> {
    let all = agent.peers();
    if let Target::Peer(name) = &step.target
        && let Some(peer) = all.iter().find(|p| &p.name == name)
        && ctx.chain.contains(&peer.id)
    {
        bail!("{name} handed this work down, so it isn't given back to it");
    }
    let peers: Vec<PeerInfo> = all
        .into_iter()
        .filter(|p| !ctx.chain.contains(&p.id))
        .filter(|p| !tried.is_some_and(|t| t.contains(&p.name)))
        .collect();
    choose_peer(step, &peers, squads, &load(agent, &peers))
}

/// The member of squad `squad` a task on its own goes to, as a plan step on `#squad`
/// would: the least busy online member that can run it (its provider, folder, checks
/// and MCP servers; those that allow its model first), none of those in `tried`.
pub fn squad_member(
    agent: &Agent,
    squads: &SquadStore,
    squad: &str,
    task: &LlmTask,
    tried: &HashSet<String>,
) -> Result<PeerInfo> {
    let mut step = Plan::parse(&format!("task #{squad} using {} the task", task.provider))?
        .steps
        .remove(0);
    step.share = task.share.clone();
    step.checks = task.checks.clone();
    step.mcp = task.mcp.clone();
    step.model = task.model.clone();
    step.effort = task.effort.clone();
    pick_peer(agent, &step, squads, &RunContext::default(), Some(tried))
}

/// [`run_plan`] as part of `ctx`: each step's request says which run and step it is,
/// running steps report what their peer last said it is doing, squad steps go to the
/// member with the least work (counting other agents' tasks) and move to another
/// member when theirs refuses or drops them, and `timeout=` cancels a step (on every
/// level below it too) that takes too long.
pub async fn run_plan_with(
    agent: &Agent,
    squads: &SquadStore,
    plan: &Plan,
    ctx: &RunContext,
    mut on_update: impl FnMut(&str, &StepState),
) -> BTreeMap<String, StepState> {
    // A resumed run's finished steps stay as they ended.
    let mut states: BTreeMap<String, StepState> = plan
        .steps
        .iter()
        .map(|s| {
            let before = ctx.resume.finished.get(&s.id).filter(|st| st.finished());
            (s.id.clone(), before.cloned().unwrap_or(StepState::Waiting))
        })
        .collect();
    for step in &plan.steps {
        on_update(&step.id, &states[&step.id]);
    }
    // The work it was waiting for before the restart, waited for again (once).
    let mut sent_before = ctx.resume.sent.clone();
    let mut running: JoinSet<(String, String, Result<Delivered>)> = JoinSet::new();
    let (progress_tx, mut progress_rx) =
        tokio::sync::mpsc::unbounded_channel::<(String, Progress)>();
    // Squad members a step moved away from, and why it last moved.
    let mut tried: HashMap<String, HashSet<String>> = HashMap::new();
    let mut moved: HashMap<String, String> = HashMap::new();
    let mut chain: Vec<String> = ctx.chain.iter().map(ToString::to_string).collect();
    chain.push(agent.id().to_string());
    // The rounds of changes each sign-off's approver asked for, the note of their last
    // one, and the steps to do again for them (see [`Step::changes`]).
    let mut rounds: HashMap<String, u32> = HashMap::new();
    let mut asked: HashMap<String, String> = HashMap::new();
    let mut reworks: HashMap<String, Rework> = HashMap::new();
    loop {
        // Skip steps whose prerequisites failed; start steps whose prerequisites are done.
        let mut changed = true;
        while changed {
            changed = false;
            for step in &plan.steps {
                if states[&step.id] != StepState::Waiting {
                    continue;
                }
                if let Some(dep) = step.after.iter().find(|d| {
                    matches!(
                        states[*d],
                        StepState::Failed { .. } | StepState::Skipped { .. }
                    )
                }) {
                    let state = StepState::Skipped {
                        reason: format!("step {dep} did not succeed"),
                    };
                    on_update(&step.id, &state);
                    states.insert(step.id.clone(), state);
                    changed = true;
                    continue;
                }
                if !step
                    .after
                    .iter()
                    .all(|d| matches!(states[d], StepState::Done { .. }))
                {
                    continue;
                }
                if let Some(sent) = sent_before.remove(&step.id) {
                    let (peer_name, work) = rejoin(agent, sent, &progress_tx, &step.id);
                    let state = StepState::Running {
                        peer: peer_name.clone(),
                        progress: Some(Progress::new(
                            Phase::Running,
                            "taken up again after this agent restarted",
                        )),
                    };
                    on_update(&step.id, &state);
                    states.insert(step.id.clone(), state);
                    let (id, timeout) = (step.id.clone(), step.timeout);
                    running.spawn(async move { (id, peer_name, within(timeout, work).await) });
                    continue;
                }
                let deps: Vec<Earlier> = step
                    .after
                    .iter()
                    .filter_map(|d| match &states[d] {
                        StepState::Done { peer, output, .. } => Some(Earlier {
                            id: d.as_str(),
                            peer: peer.as_str(),
                            task: plan
                                .steps
                                .iter()
                                .find(|s| &s.id == d)
                                .map(Step::own_text)
                                .unwrap_or_default(),
                            output: output.as_str(),
                        }),
                        _ => None,
                    })
                    .collect();
                // The issue it works on, as its tracker has it (people wrote it).
                let issue = ctx
                    .issue_texts
                    .get(&step.id)
                    .map(|text| issue_section(step, text))
                    .unwrap_or_default();
                let prompt = step_prompt_with(step, &deps, &issue);
                let fail = |error: String| StepState::failed(None, error);
                // What its card says while it is done again for a sign-off's changes.
                let mut again: Option<String> = None;
                let (peer_name, work): (
                    String,
                    std::pin::Pin<Box<dyn Future<Output = Result<Delivered>> + Send>>,
                ) = if step.approve {
                    // An approver sees what the steps before it created (directly or
                    // through other approvals): the prototype, decks and design documents.
                    let earlier = ancestors(plan, &step.id);
                    let preview: BTreeMap<String, String> = crate::preview::sources_from_patches(
                        plan.steps
                            .iter()
                            .filter(|s| earlier.contains(&s.id))
                            .filter_map(|s| match &states[&s.id] {
                                StepState::Done {
                                    patch: Some(patch), ..
                                } => Some(patch.as_str()),
                                _ => None,
                            }),
                    )
                    .into_iter()
                    .map(|(path, bytes)| (path, String::from_utf8_lossy(&bytes).into_owned()))
                    .collect();
                    match choose_approver(step, &agent.peers()) {
                        Ok((approver, name)) => {
                            let refused = approver
                                .and_then(|id| m365_refusal(agent, ctx, plan, step, &name, &id));
                            if let Some(why) = refused {
                                let state = fail(why);
                                on_update(&step.id, &state);
                                states.insert(step.id.clone(), state);
                                changed = true;
                                continue;
                            }
                            let (agent, title) = (agent.clone(), format!("Plan step {}", step.id));
                            // A person reads it: the rule for models is left out.
                            let mut text = approval_text(&prompt);
                            let round = rounds.get(&step.id).copied().unwrap_or(0);
                            if let Some(note) = asked.get(&step.id) {
                                text = format!(
                                    "**Asked again** (round {} of changes): you asked for changes, and the work was done again with your note; its new results are below.\n\n> {}\n\n---\n\n{text}",
                                    round,
                                    note.replace('\n', "\n> ")
                                );
                            }
                            // They may ask for changes while it allows more rounds, and there
                            // is work to do again.
                            let changes = round < step.changes.unwrap_or(DEFAULT_CHANGE_ROUNDS)
                                && !redoable(plan, step, &states).is_empty();
                            let task_id = agent.new_task_id();
                            if let Some(journal) = &ctx.journal {
                                agent.keep_on_exit(task_id);
                                (journal.0)(
                                    &step.id,
                                    &SentStep {
                                        peer: name.clone(),
                                        peer_id: approver.map(|id| id.to_string()),
                                        task_id,
                                        work: SentWork::Approval {
                                            title: title.clone(),
                                            text: text.clone(),
                                            changes,
                                        },
                                    },
                                );
                            }
                            let ask = crate::agent::ApprovalAsk {
                                title,
                                text,
                                preview,
                                changes,
                            };
                            (
                                name,
                                Box::pin(async move {
                                    agent.request_approval_as(approver, task_id, &ask).await
                                }),
                            )
                        }
                        Err(e) => {
                            let state = fail(format!("{e:#}"));
                            on_update(&step.id, &state);
                            states.insert(step.id.clone(), state);
                            changed = true;
                            continue;
                        }
                    }
                } else if step.is_workiq() {
                    // Asked of Work IQ by this agent: nothing is sent to a peer.
                    (
                        step.workiq_name(),
                        Box::pin(ask_workiq(ctx, step, prompt.clone())),
                    )
                } else {
                    let peer = match pick_peer(agent, step, squads, ctx, tried.get(&step.id)) {
                        Ok(peer) => peer,
                        Err(e) => {
                            let state = fail(format!("{e:#}"));
                            on_update(&step.id, &state);
                            states.insert(step.id.clone(), state);
                            changed = true;
                            continue;
                        }
                    };
                    if let Some(why) = m365_refusal(agent, ctx, plan, step, &peer.name, &peer.id) {
                        let state = StepState::failed(Some(peer.name), why);
                        on_update(&step.id, &state);
                        states.insert(step.id.clone(), state);
                        changed = true;
                        continue;
                    }
                    let prompt = if step.workiq.is_empty() {
                        prompt
                    } else {
                        match search_work(agent, ctx, step, &peer).await {
                            Ok(work) => step_prompt_with(step, &deps, &format!("{issue}{work}")),
                            Err(e) => {
                                let state = StepState::failed(Some(peer.name), format!("{e:#}"));
                                on_update(&step.id, &state);
                                states.insert(step.id.clone(), state);
                                changed = true;
                                continue;
                            }
                        }
                    };
                    let mut task = LlmTask::new(step.provider, prompt.clone())
                        .with_checks(step.checks.clone(), step.attempts);
                    task.mcp = step.mcp.clone();
                    task.model = step.model.clone();
                    task.effort = step.effort.clone();
                    task.title = Some(crate::llm::title_of(step.own_text()));
                    task.issues = ctx.issues.get(&step.id).cloned().unwrap_or_default();
                    if let Some(share) = &step.share {
                        task = task.in_share(share.clone());
                    }
                    task.patch = step.apply.as_ref().and_then(|a| match &states[a] {
                        StepState::Done { patch, .. } => patch.clone(),
                        _ => None,
                    });
                    if step.apply.is_some() && task.patch.is_none() {
                        let state = fail(format!(
                            "step {} left no patch to apply (it changed nothing, or its folder isn't shared with --worktrees)",
                            step.apply.as_deref().unwrap_or_default()
                        ));
                        on_update(&step.id, &state);
                        states.insert(step.id.clone(), state);
                        changed = true;
                        continue;
                    }
                    // Done again for the changes a sign-off's approver asked for: from its
                    // earlier work, with their note.
                    if let Some(rework) = reworks.remove(&step.id) {
                        task.patch = rework.patch.clone().or(task.patch.take());
                        task.prompt = format!("{}\n\n{}", task.prompt, rework_text(&rework));
                        again = Some(format!(
                            "done again: {} asked for changes at {}: {}",
                            rework.by,
                            rework.signoff,
                            crate::runs::one_line(&rework.note)
                        ));
                    }
                    if let Some(levels) = step.team {
                        task.team = Some(crate::team::TeamRequest {
                            levels,
                            chain: chain.clone(),
                            brief: step.own_text().to_owned(),
                        });
                    }
                    if step.pr {
                        match pr_ask(plan, step, &states) {
                            Ok(mut ask) => {
                                ask.reviewers = reviewers_for(
                                    step.reviewers.as_deref(),
                                    &ask.signoffs,
                                    &agent.id(),
                                    &fresh_recipients(agent, ctx),
                                );
                                task.prompt = format!(
                                    "Open a pull request from {}: {}",
                                    ask.branch, ask.title
                                );
                                task.title = Some(ask.title.clone());
                                task.pull_request = Some(Box::new(ask));
                            }
                            Err(e) => {
                                let state = StepState::failed(Some(peer.name), format!("{e:#}"));
                                on_update(&step.id, &state);
                                states.insert(step.id.clone(), state);
                                changed = true;
                                continue;
                            }
                        }
                    }
                    let origin = Origin {
                        run: ctx.run_id.clone(),
                        step: Some(step.id.clone()),
                        task: None,
                    };
                    let journaled = ctx.journal.as_ref().map(|_| (task.clone(), origin.clone()));
                    let delegation = match agent.send_task(peer.id, task, Some(origin)) {
                        Ok(delegation) => delegation,
                        Err(e) => {
                            let state = StepState::failed(Some(peer.name), format!("{e:#}"));
                            on_update(&step.id, &state);
                            states.insert(step.id.clone(), state);
                            changed = true;
                            continue;
                        }
                    };
                    if let (Some(journal), Some((task, origin))) = (&ctx.journal, journaled) {
                        agent.keep_on_exit(delegation.task_id());
                        (journal.0)(
                            &step.id,
                            &SentStep {
                                peer: peer.name.clone(),
                                peer_id: Some(peer.id.to_string()),
                                task_id: delegation.task_id(),
                                work: SentWork::Task {
                                    task: Box::new(task),
                                    origin: Some(origin),
                                },
                            },
                        );
                    }
                    let (tx, id) = (progress_tx.clone(), step.id.clone());
                    (
                        peer.name,
                        Box::pin(delegation.finish(move |progress| {
                            let _ = tx.send((id.clone(), progress));
                        })),
                    )
                };
                let state = StepState::Running {
                    peer: peer_name.clone(),
                    progress: moved
                        .remove(&step.id)
                        .map(|why| Progress::new(Phase::Running, format!("moved here: {why}")))
                        .or_else(|| again.map(|text| Progress::new(Phase::Running, text))),
                };
                on_update(&step.id, &state);
                states.insert(step.id.clone(), state);
                let (id, timeout) = (step.id.clone(), step.timeout);
                running.spawn(async move { (id, peer_name, within(timeout, work).await) });
            }
        }
        if running.is_empty() {
            break;
        }
        tokio::select! {
            joined = running.join_next() => {
                let Some(Ok((id, peer, result))) = joined else {
                    continue;
                };
                let step = plan.steps.iter().find(|s| s.id == id).expect("known step");
                // A squad step its peer never ran (or lost) goes to another member.
                if matches!(step.target, Target::Squad(_))
                    && let Some(why) = movable(&result)
                {
                    let seen = tried.entry(id.clone()).or_default();
                    seen.insert(peer.clone());
                    let seen = seen.clone();
                    if pick_peer(agent, step, squads, ctx, Some(&seen)).is_ok() {
                        moved.insert(id.clone(), format!("{peer}: {why}"));
                        states.insert(id, StepState::Waiting);
                        continue;
                    }
                }
                let state = match result {
                    Ok(delivered) if step.approve && changes_asked(&delivered).is_some() => {
                        let (by, note) = changes_asked(&delivered).unwrap_or_default();
                        let used = rounds.get(&id).copied().unwrap_or(0);
                        let allowed = step.changes.unwrap_or(DEFAULT_CHANGE_ROUNDS);
                        let redo: Vec<String> = redoable(plan, step, &states)
                            .iter()
                            .map(|s| s.id.clone())
                            .collect();
                        if used < allowed && !redo.is_empty() {
                            // The work it is on is done again with their note, then they
                            // are asked again.
                            rounds.insert(id.clone(), used + 1);
                            asked.insert(id.clone(), note.clone());
                            for s in redo {
                                let patch = match &states[&s] {
                                    StepState::Done { patch, .. } => patch.clone(),
                                    _ => None,
                                };
                                reworks.insert(
                                    s.clone(),
                                    Rework {
                                        signoff: id.clone(),
                                        by: by.clone(),
                                        note: note.clone(),
                                        patch,
                                    },
                                );
                                on_update(&s, &StepState::Waiting);
                                states.insert(s, StepState::Waiting);
                            }
                            on_update(&id, &StepState::Waiting);
                            states.insert(id, StepState::Waiting);
                            continue;
                        }
                        let mut state = StepState::from_delivered(peer, step, delivered);
                        if let StepState::Failed { error, .. } = &mut state {
                            error.push_str(&format!(
                                "\n\nIt allows no more rounds of changes (changes={allowed})."
                            ));
                        }
                        state
                    }
                    Ok(delivered) => StepState::from_delivered(peer, step, delivered),
                    Err(e) => StepState::failed(Some(peer), format!("{e:#}")),
                };
                on_update(&id, &state);
                states.insert(id, state);
            }
            Some((id, progress)) = progress_rx.recv() => {
                if let Some(StepState::Running { peer, .. }) = states.get(&id) {
                    let state = StepState::Running {
                        peer: peer.clone(),
                        progress: Some(progress),
                    };
                    on_update(&id, &state);
                    states.insert(id, state);
                }
            }
        }
    }
    debug_assert!(states.values().all(StepState::finished));
    states
}
/// A step done again because a sign-off's approver asked for changes (see
/// [`Step::changes`]).
struct Rework {
    /// The sign-off, and who answered it.
    signoff: String,
    by: String,
    /// What they asked to change.
    note: String,
    /// The step's earlier work, applied first so its model changes it rather than starting
    /// over (none: it changed no files).
    patch: Option<String>,
}

/// What the model doing a step again is told about the changes asked for.
fn rework_text(rework: &Rework) -> String {
    format!(
        "## Changes asked for\n\n{} reviewed the result of this step at sign-off {} and asked for changes:\n\n{}\n\nYour earlier work on this step is applied in your workspace already: change it as asked, and say what you changed.",
        rework.by, rework.signoff, rework.note
    )
}

/// The steps a sign-off's approver may have done again when they ask for changes: the
/// steps a model does that it waits for directly, done.
fn redoable<'a>(
    plan: &'a Plan,
    signoff: &Step,
    states: &BTreeMap<String, StepState>,
) -> Vec<&'a Step> {
    plan.steps
        .iter()
        .filter(|s| signoff.after.contains(&s.id))
        .filter(|s| !s.approve && !s.pr && s.team.is_none() && !s.is_workiq())
        .filter(|s| matches!(states.get(&s.id), Some(StepState::Done { .. })))
        .collect()
}

/// The approver and note of an answer that asks for changes, if it is one: a validly
/// signed approval they didn't give, saying so (see [`crate::agent::Answer::Changes`]).
fn changes_asked(d: &Delivered) -> Option<(String, String)> {
    let e = d.evidence.as_ref()?;
    if !d.verified || e.kind != crate::evidence::EvidenceKind::Approval || e.approved != Some(false)
    {
        return None;
    }
    let said = d.output.strip_prefix(crate::agent::CHANGES_ASKED)?;
    let note = said
        .split_once(": ")
        .map(|(_, note)| note.trim().to_owned())
        .unwrap_or_default();
    Some((e.signer.clone(), note))
}

/// The person who must approve `step`: a connected peer by name, or `@me` (None).
fn choose_approver(step: &Step, peers: &[PeerInfo]) -> Result<(Option<iroh::EndpointId>, String)> {
    match &step.target {
        Target::Peer(name) if name == "me" => Ok((None, "you".to_owned())),
        Target::Peer(name) => peers
            .iter()
            .find(|p| &p.name == name)
            .map(|p| (Some(p.id), p.name.clone()))
            .ok_or_else(|| anyhow!("{name} is not connected")),
        Target::Squad(_) | Target::Workiq => bail!("approvals need a person: @name or @me"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::Access;
    use crate::shares::ShareInfo;

    #[test]
    fn parses_plan_text() {
        let plan = Plan::parse(
            "// release notes\n\
             api @alice:repo Summarize the public API\n\
             \n\
             tests after api #qa:code using claude Write test ideas for it\n\
             report after api,tests @helper Combine both into a status report\n",
        )
        .unwrap();
        assert_eq!(plan.steps.len(), 3);
        let tests = &plan.steps[1];
        assert_eq!(tests.id, "tests");
        assert_eq!(tests.after, ["api"]);
        assert_eq!(tests.target, Target::Squad("qa".into()));
        assert_eq!(tests.share.as_deref(), Some("code"));
        assert_eq!(tests.provider, Provider::Claude);
        assert_eq!(tests.prompt, "Write test ideas for it");
        assert_eq!(plan.steps[0].target, Target::Peer("alice".into()));
        assert_eq!(plan.steps[0].provider, Provider::Copilot);
        assert_eq!(plan.steps[2].after, ["api", "tests"]);
        assert_eq!(plan.steps[2].share, None);

        let err = Plan::parse("a @x\nb nobody do it\nc @y using gpt hi").unwrap_err();
        let err = err.to_string();
        assert!(err.contains("line 1: missing the task prompt"), "{err}");
        assert!(
            err.contains("line 2: who runs it (@peer, @peer:folder or #squad) comes after the step's name, got \"nobody\""),
            "{err}"
        );
        assert!(err.contains("line 3: unknown provider"), "{err}");
        assert!(Plan::parse("// only a comment").is_err());
    }

    #[test]
    fn step_lines_say_what_is_wrong_and_repeated_options_add_up() {
        // What the plan editor makes of clicking two skills, and the same typed out.
        for text in [
            "step1 @worker:repo skill=adr,api-contract What to do",
            "step1 @worker:repo skill=adr skills=api-contract,adr What to do",
        ] {
            assert_eq!(
                Plan::parse(text).unwrap().steps[0].skills,
                ["adr", "api-contract"]
            );
        }
        let step = &Plan::parse("a @x verify=build verify=test,build mcp=web mcp=db do it")
            .unwrap()
            .steps[0];
        assert_eq!(step.checks, ["build", "test"]);
        assert_eq!(step.mcp, ["web", "db"]);

        let fails = |text: &str, expected: &str| {
            let err = Plan::parse(text).unwrap_err().to_string();
            assert!(err.contains(expected), "{text:?}: {err}");
        };
        let form = "a step line is: name [after steps] @peer[:folder] or #squad [options] what to do, e.g. fix @alice:app";
        fails(
            "skill=adr skill=api-contract",
            "line 1: starts with the option skill=adr, but a step line is",
        );
        fails("skill=adr skill=api-contract", form);
        fails(
            "@worker:repo Do it",
            "line 1: starts with who runs it (@worker:repo), but",
        );
        fails("a! @x do", "invalid step name \"a!\"");
        fails("a", "who runs it is missing; a step line is");
        fails(
            "a skill=adr do",
            "comes after the step's name, got \"skill=adr\"",
        );
        fails("a ü@x do", "comes after the step's name, got \"ü@x\"");
        fails("a @x save=one save=two do", "save= is given twice");
        fails(
            "a @x verify=t attempts=2 attempts=3 do",
            "attempts= is given twice",
        );
    }

    #[test]
    fn rejects_cycles_and_unknown_dependencies() {
        let cycle = Plan::parse("a after c @x do\nb after a @x do\nc after b @x do\nd @x ok");
        assert!(
            cycle
                .unwrap_err()
                .to_string()
                .contains("cycle between steps: a, b, c")
        );
        let unknown = Plan::parse("a after z @x do").unwrap_err().to_string();
        assert!(unknown.contains("unknown step 'z'"), "{unknown}");
        let own = Plan::parse("a after a @x do").unwrap_err().to_string();
        assert!(own.contains("itself"), "{own}");
        let twice = Plan::parse("a @x do\na @y again").unwrap_err().to_string();
        assert!(twice.contains("defined twice"), "{twice}");
    }

    #[test]
    fn parses_claims_checks_patches_and_approvals() {
        let plan = Plan::parse(
            "claim: C-1 The parser handles empty input\n\
             impl @dev:app using claude verify=test,lint attempts=3 claims=C-1 Fix the parser\n\
             check after impl #qa:app apply=impl verify=test claims=C-1 Review it\n\
             signoff after check approve @alice claims=C-1 Approve the release\n\
             note @bob verify the=prompt words stay in the prompt",
        )
        .unwrap();
        assert_eq!(
            plan.claims,
            [ClaimDef {
                id: "C-1".into(),
                text: "The parser handles empty input".into()
            }]
        );
        let impl_ = &plan.steps[0];
        assert_eq!(impl_.provider, Provider::Claude);
        assert_eq!(impl_.checks, ["test", "lint"]);
        assert_eq!(impl_.attempts, Some(3));
        assert_eq!(impl_.claims, ["C-1"]);
        assert_eq!(impl_.prompt, "Fix the parser");
        assert_eq!(plan.steps[1].apply.as_deref(), Some("impl"));
        let signoff = &plan.steps[2];
        assert!(signoff.approve && !impl_.approve);
        assert_eq!(signoff.target, Target::Peer("alice".into()));
        assert_eq!(signoff.prompt, "Approve the release");
        assert_eq!(
            plan.steps[3].prompt,
            "verify the=prompt words stay in the prompt"
        );

        let fails = |text: &str, expected: &str| {
            let err = Plan::parse(text).unwrap_err().to_string();
            assert!(err.contains(expected), "{text:?}: {err}");
        };
        fails("a @x claims=C-9 do", "unknown claim 'C-9'");
        fails("a @x do\nb @x apply=a check", "must run after it");
        fails("a @x attempts=2 do", "no checks to retry on");
        fails(
            "a @x verify=t attempts=9 do",
            "attempts must be between 1 and 5",
        );
        fails("a approve #qa ok?", "needs a person");
        fails(
            "a approve @x:repo ok?",
            "can't have checks, a folder or a patch",
        );
        fails("claim: C-1\na @x do", "needs a statement");
        fails(
            "claim: C-1 x\nclaim: C-1 y\na @x do",
            "claim 'C-1' is defined twice",
        );
        fails("a approve @me save=notes ok?", "no result to save");
        fails("a @x save=../up do", "save= needs a topic name");
    }

    #[test]
    fn a_plan_says_how_its_run_is_tracked_and_its_steps_name_their_issues() {
        let plan = Plan::parse("track: #12\na @x issue=#15 Do it\nb after a @y issue=16 Then this")
            .unwrap();
        assert_eq!(plan.track, Some(Track::Issue(12)));
        assert_eq!(plan.steps[0].issue, Some(15));
        assert_eq!(plan.steps[1].issue, Some(16));
        assert_eq!(plan.steps[1].prompt, "Then this");
        assert!(plan.names_issues());
        let track = |text: &str| Plan::parse(text).unwrap().track;
        assert_eq!(
            track("track: Ship the tracker\na @x Do it"),
            Some(Track::New("Ship the tracker".into()))
        );
        assert_eq!(track("track: OFF\na @x Do it"), Some(Track::Off));
        assert!(!Plan::parse("a @x Do it").unwrap().names_issues());
        for (text, expected) in [
            ("track: #x\na @x Do it", "track: #N needs an issue number"),
            ("track:\na @x Do it", "track: needs #<issue number>"),
            (
                "track: #1\ntrack: #2\na @x Do it",
                "line 2: track: is given twice",
            ),
            ("a @x issue=abc Do it", "issue= needs an issue number"),
            ("a @x issue=#1 issue=#2 Do it", "issue= is given twice"),
            (
                "a @x issue=#3 Do it\nb @y issue=3 Do that",
                "both work on issue #3",
            ),
            ("track: #4\na @x issue=#4 Do it", "the run's own issue"),
        ] {
            let err = Plan::parse(text).unwrap_err().to_string();
            assert!(err.contains(expected), "{text:?}: {err}");
        }
    }

    #[test]
    fn an_approvals_text_has_no_external_content_wrappers() {
        let step = Step {
            prompt: "Approve the facts".into(),
            ..Plan::parse("a @x Approve the facts").unwrap().steps[0].clone()
        };
        let prompt = step_prompt(
            &step,
            &[Earlier {
                id: "find",
                peer: "worker",
                task: "Find the facts",
                output: "the facts\nsecond line",
            }],
        );
        assert!(prompt.contains("<external_content source="), "{prompt}");
        let text = approval_text(&prompt);
        assert!(!text.contains("external_content"), "{text}");
        assert!(!text.contains("Rule for external content"), "{text}");
        assert!(
            text.contains(
                "### Step find (done by worker)\nIts task:\nFind the facts\n\nIts result:\nthe facts\nsecond line\n"
            ),
            "{text}"
        );
        assert!(text.ends_with("Your task:\nApprove the facts"), "{text}");
    }

    /// A step that works on an issue gets its tracker's text of it in front of its task,
    /// as external content; with earlier steps' results, after them.
    #[test]
    fn a_step_working_on_an_issue_gets_its_text_as_external_content() {
        let plan = Plan::parse("docs @x issue=#12 Document it").unwrap();
        let step = &plan.steps[0];
        let issue = issue_section(step, "acme/app#12 Document the API\n\nEvery endpoint.");
        let prompt = step_prompt_with(step, &[], &issue);
        assert!(prompt.starts_with(crate::boundary::DIRECTIVE), "{prompt}");
        assert!(
            prompt.contains(
                "The issue this task works on:\n<external_content source=\"issue:12\">\nacme/app#12 Document the API\n\nEvery endpoint.\n</external_content>"
            ),
            "{prompt}"
        );
        assert!(prompt.ends_with("---\nYour task:\nDocument it"), "{prompt}");
        let earlier = Earlier {
            id: "api",
            peer: "w",
            task: "Add the API",
            output: "done",
        };
        let both = step_prompt_with(step, &[earlier], &issue);
        assert!(
            both.find("### Step api").unwrap() < both.find("The issue this task works on").unwrap(),
            "{both}"
        );
    }

    /// A step gets what each step it waits for was asked (its own words, cut), with
    /// what it returned, inside the same envelope; a task-less one only its result.
    #[test]
    fn a_step_gets_the_tasks_of_the_steps_it_waits_for_with_their_results() {
        let plan =
            Plan::parse("answer @a skill=x Reply: delta\nreview after answer @a Is it right?")
                .unwrap();
        let mut answer = plan.steps[0].clone();
        answer.keep_brief();
        answer.prompt = format!(
            "Follow these instructions:\nbe terse\n---\n{}",
            answer.prompt
        );
        let long = "y".repeat(MAX_DEP_TASK_CHARS + 5);
        let prompt = step_prompt(
            &plan.steps[1],
            &[
                Earlier {
                    id: "answer",
                    peer: "a",
                    task: answer.own_text(),
                    output: "delta",
                },
                Earlier {
                    id: "long",
                    peer: "b",
                    task: &long,
                    output: "ok",
                },
                Earlier {
                    id: "bare",
                    peer: "c",
                    task: " ",
                    output: "just this",
                },
            ],
        );
        assert!(
            prompt.contains(
                "### Step answer (done by a)\n<external_content source=\"step:answer peer:a\">\nIts task:\nReply: delta\n\nIts result:\ndelta\n</external_content>"
            ),
            "{prompt}"
        );
        assert!(
            !prompt.contains("be terse"),
            "its skills stay out: {prompt}"
        );
        assert!(
            prompt.contains(&format!(
                "{}\n[... truncated]\n\nIts result:\nok",
                "y".repeat(MAX_DEP_TASK_CHARS)
            )),
            "{prompt}"
        );
        assert!(
            prompt.contains("peer:c\">\njust this\n</external_content>"),
            "{prompt}"
        );
        assert!(
            prompt.ends_with("---\nYour task:\nIs it right?"),
            "{prompt}"
        );
    }

    #[test]
    fn steps_use_and_grow_the_knowledge_base() {
        let dir = tempfile::tempdir().unwrap();
        let kb = KnowledgeBase::new(dir.path());
        kb.write("architecture", "Three services.", "").unwrap();
        let plan = Plan::parse(
            "design @x context=architecture save=design Design it\nplain @x save=other Do it\nlocal @x save=mine Note it",
        )
        .unwrap();
        let ready = plan.with_knowledge(&kb).unwrap();
        assert!(
            ready.steps[0].prompt.contains(
                "### Knowledge: architecture\n<external_content source=\"knowledge:architecture\">\nThree services.\n</external_content>"
            ),
            "knowledge is external content: {}",
            ready.steps[0].prompt
        );
        assert!(
            step_prompt(&ready.steps[0], &[]).starts_with(crate::boundary::DIRECTIVE),
            "and the step runs with the rule for it"
        );
        assert_eq!(step_prompt(&ready.steps[1], &[]), "Do it");
        assert!(ready.steps[0].prompt.ends_with("---\nDesign it"));
        assert_eq!(ready.steps[1].prompt, "Do it");
        let missing = Plan::parse("a @x context=nope do").unwrap();
        assert!(
            missing
                .with_knowledge(&kb)
                .unwrap_err()
                .to_string()
                .contains("step 'a'")
        );

        let done = |verified| StepState::Done {
            peer: "x".into(),
            output: "The design.".into(),
            evidence: None,
            verified,
            patch: None,
            model: None,
        };
        let states: BTreeMap<String, StepState> = [
            ("design".to_owned(), done(true)),
            ("plain".to_owned(), done(false)),
            ("local".to_owned(), StepState::failed(None, "no".into())),
        ]
        .into();
        assert_eq!(save_knowledge(&kb, &plan, &states, "r1"), ["design"]);
        let saved = kb.read("design").unwrap();
        assert!(
            saved.contains("source: run r1, step design by x") && saved.contains("The design.")
        );
        assert!(
            kb.read("other").is_err(),
            "unverified results aren't knowledge"
        );
    }

    #[test]
    fn steps_act_as_agents_with_their_skills_and_mcp_servers() {
        let dir = tempfile::tempdir().unwrap();
        let kb = KnowledgeBase::new(dir.path().join("kb"));
        let skills = Skills::new(dir.path().join("skills"));
        skills
            .add_agent(
                "sec",
                "---\ndescription: Security reviewer\nskills: [security-review]\nmcp: [semgrep]\n---\nAct as a security engineer.",
            )
            .unwrap();
        let plan = Plan::parse("a @x agent=sec skill=code-review Review the diff").unwrap();
        assert_eq!(plan.steps[0].agent.as_deref(), Some("sec"));
        let ready = plan.prepare(&kb, &skills).unwrap();
        let step = &ready.steps[0];
        assert_eq!(step.skills, ["security-review", "code-review"]);
        assert_eq!(step.mcp, ["semgrep"]);
        assert!(
            step.prompt.starts_with(
                "Follow these instructions:\n\n### Agent: sec\nAct as a security engineer.\n\n### Skill: security-review"
            ),
            "{}",
            step.prompt
        );
        assert!(step.prompt.ends_with("---\nReview the diff"));

        let unknown = Plan::parse("a @x agent=nope do").unwrap();
        assert!(
            unknown
                .prepare(&kb, &skills)
                .unwrap_err()
                .to_string()
                .contains("step 'a'")
        );
        let fails = |text: &str, expected: &str| {
            let err = Plan::parse(text).unwrap_err().to_string();
            assert!(err.contains(expected), "{text}: {err}");
        };
        fails("a @x agent=sec agent=sec do", "agent= is given twice");
        fails("a @x agent=../up do", "agent= needs an agent's name");
        fails("a approve @me agent=sec ok?", "can't act as an agent");
    }

    #[test]
    fn steps_name_their_model_or_take_their_agents_and_the_check_warns_when_peers_refuse_it() {
        let dir = tempfile::tempdir().unwrap();
        let kb = KnowledgeBase::new(dir.path().join("kb"));
        let skills = Skills::new(dir.path().join("skills"));
        skills
            .add_agent(
                "arch",
                "---\ndescription: Architect\nmodel: claude-opus-5.5\n---\nDesign it.",
            )
            .unwrap();
        let plan = Plan::parse(
            "a @x model=gpt-5.5 Build it\nb @x agent=arch Design it\nc @x agent=arch model=gpt-5.5 Design it fast\nd @x Plain",
        )
        .unwrap();
        assert_eq!(plan.steps[0].model.as_deref(), Some("gpt-5.5"));
        // A step with an agent and no model of its own runs on the agent's.
        let ready = plan.prepare(&kb, &skills).unwrap();
        let models: Vec<Option<&str>> = ready.steps.iter().map(|s| s.model.as_deref()).collect();
        assert_eq!(
            models,
            [
                Some("gpt-5.5"),
                Some("claude-opus-5.5"),
                Some("gpt-5.5"),
                None
            ]
        );
        let fails = |text: &str, expected: &str| {
            let err = Plan::parse(text).unwrap_err().to_string();
            assert!(err.contains(expected), "{text}: {err}");
        };
        fails("a @x model=a model=b do", "model= is given twice");
        fails("a @x model=`x` do", "model= needs a model's name");
        fails("ok approve @me model=x Fine", "is a person's sign-off");
        fails("t team @lead:app model=x Do", "is a team step");
        fails("w @workiq model=x Ask", "is answered by Work IQ");
        // effort= names one of the reasoning efforts, once, on steps a model runs.
        let effort = Plan::parse("a @x model=gpt-5.5 effort=MAX Build it\nb @x Plain").unwrap();
        assert_eq!(effort.steps[0].effort.as_deref(), Some("max"));
        assert_eq!(effort.steps[1].effort, None);
        fails("a @x effort=low effort=max do", "effort= is given twice");
        fails(
            "a @x effort=turbo do",
            "effort= needs a reasoning effort (minimal, low, medium, high, xhigh, max)",
        );
        fails(
            "ok approve @me effort=max Fine",
            "effort= is for steps a model runs",
        );
        fails("t team @lead:app effort=max Do", "is a team step");

        // The check warns when the step's peer says it doesn't allow the model.
        let mut models = crate::models::Models::new();
        crate::models::set_allowed(&mut models, Provider::Copilot, &["gpt-5.5".into()]).unwrap();
        let peer = PeerInfo {
            name: "x".into(),
            id: iroh::SecretKey::generate().public(),
            capabilities: vec![Provider::Copilot],
            shares: vec![ShareInfo {
                name: "data".into(),
                access: Access::ReadOnly,
                ..Default::default()
            }],
            owner: None,
            role: None,
            load: None,
            models,
            offers: Default::default(),
        };
        let problems = preflight(
            &plan,
            std::slice::from_ref(&peer),
            &SquadStore::default(),
            &kb,
            &skills,
            &Recipients::default(),
        );
        let about_models: Vec<(Option<&str>, bool, &str)> = problems
            .iter()
            .filter(|p| p.message.contains("model="))
            .map(|p| (p.step.as_deref(), p.error, p.message.as_str()))
            .collect();
        assert_eq!(
            about_models,
            [(
                Some("b"),
                false,
                "model=claude-opus-5.5 would be refused for copilot: x allows only gpt-5.5"
            )]
        );
        // And when it doesn't allow the reasoning effort.
        let mut efforts = crate::models::Models::new();
        crate::models::set_efforts(
            &mut efforts,
            Provider::Copilot,
            &["low".into(), "medium".into()],
        )
        .unwrap();
        let deep = Plan::parse("e @x effort=max Think hard\nf @x effort=low Quick").unwrap();
        let problems = preflight(
            &deep,
            &[PeerInfo {
                models: efforts,
                ..peer
            }],
            &SquadStore::default(),
            &kb,
            &skills,
            &Recipients::default(),
        );
        let about_efforts: Vec<(Option<&str>, bool, &str)> = problems
            .iter()
            .filter(|p| p.message.contains("effort="))
            .map(|p| (p.step.as_deref(), p.error, p.message.as_str()))
            .collect();
        assert_eq!(
            about_efforts,
            [(
                Some("e"),
                false,
                "effort=max would be refused for copilot: x allows only low, medium"
            )]
        );
    }

    #[test]
    fn squads_persist_and_validate() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("squads.json");
        let mut store = SquadStore::default();
        store
            .set("qa", vec!["bob".into(), "alice".into(), "bob".into()])
            .unwrap();
        assert_eq!(store.get("qa").unwrap().members, ["alice", "bob"]);
        assert!(store.set("qa", vec![]).is_err());
        let err = store.set("bad name!", vec!["a".into()]).unwrap_err();
        assert_eq!(
            err.to_string(),
            "squad names use letters, digits, '-', '_' or '.' (got \"bad name!\")"
        );
        assert!(store.set("../x", vec!["a".into()]).is_err());
        let err = store.set("dev", vec!["bad name".into()]).unwrap_err();
        assert!(
            err.to_string()
                .starts_with("\"bad name\" isn't a peer's name"),
            "{err}"
        );
        store.save(&path).unwrap();
        let loaded = SquadStore::load(&path).unwrap();
        assert_eq!(loaded, store);
        let mut loaded = loaded;
        assert!(loaded.remove("qa"));
        assert!(!loaded.remove("qa"));
        assert_eq!(
            SquadStore::load(&dir.path().join("none")).unwrap(),
            SquadStore::default()
        );
    }

    #[test]
    fn steps_follow_named_skills() {
        let dir = tempfile::tempdir().unwrap();
        let kb = KnowledgeBase::new(dir.path().join("kb"));
        let skills = Skills::new(dir.path().join("skills"));
        let plan =
            Plan::parse("review @x skill=security-review,code-review Review the diff").unwrap();
        assert_eq!(plan.steps[0].skills, ["security-review", "code-review"]);
        let ready = plan.prepare(&kb, &skills).unwrap();
        let prompt = &ready.steps[0].prompt;
        assert!(prompt.starts_with("Follow these instructions:\n\n### Skill: security-review"));
        assert!(
            prompt.contains("### Skill: code-review") && prompt.ends_with("---\nReview the diff")
        );
        let unknown = Plan::parse("a @x skill=nope do").unwrap();
        assert!(
            unknown
                .prepare(&kb, &skills)
                .unwrap_err()
                .to_string()
                .contains("step 'a'")
        );
    }

    #[test]
    fn indented_lines_after_a_step_continue_its_task_text() {
        let text = |plan: &str| Plan::parse(plan).unwrap().steps[0].prompt.clone();
        assert_eq!(
            text("a @x Do these:\n  - one\n  - two\nb @x Next"),
            "Do these:\n- one\n- two"
        );
        assert_eq!(
            text("a @x Intro\n  first\n\n  second\n\nb @x Next"),
            "Intro\nfirst\n\nsecond"
        );
        assert_eq!(
            text("a @x Plan:\n    - top\n      - nested\n    - more"),
            "Plan:\n- top\n  - nested\n- more"
        );
        let plan = Plan::parse("a @x One\n  more\nb @x Two").unwrap();
        assert_eq!(plan.steps.len(), 2);
        assert_eq!(plan.steps[1].prompt, "Two");
        assert_eq!(
            text("a @x\n  Starts here\n  - and a list"),
            "Starts here\n- and a list"
        );
    }

    #[test]
    fn plans_indented_uniformly_parse_as_before() {
        let plan = Plan::parse("  a @x One\n  b after a @x Two\n  // note\n").unwrap();
        let tasks: Vec<_> = plan.steps.iter().map(|s| s.prompt.as_str()).collect();
        assert_eq!(tasks, ["One", "Two"]);
    }

    #[test]
    fn option_looking_words_in_continuation_lines_are_text() {
        let plan = Plan::parse("a @x Run it\n  verify=test using claude").unwrap();
        let step = &plan.steps[0];
        assert!(step.checks.is_empty());
        assert_eq!(step.provider, Provider::Copilot);
        assert_eq!(step.prompt, "Run it\nverify=test using claude");
    }

    #[test]
    fn a_step_error_names_its_first_line() {
        let err = Plan::parse("ok @x Fine\nbad nobody text\n  more\n  lines").unwrap_err();
        let err = err.to_string();
        assert!(err.starts_with("line 2: who runs it"), "{err}");
        let err = Plan::parse("a @x Fine\n\nb @x attempts=x\n  more").unwrap_err();
        assert!(
            err.to_string().contains("line 3: attempts must be"),
            "{err}"
        );
    }

    #[test]
    fn prepare_keeps_the_steps_own_text_as_its_brief() {
        let dir = tempfile::tempdir().unwrap();
        let kb = KnowledgeBase::new(dir.path().join("kb"));
        let skills = Skills::new(dir.path().join("skills"));
        let plan = Plan::parse("review @x skill=code-review Review the diff").unwrap();
        assert_eq!(plan.steps[0].brief, "");
        let ready = plan.prepare(&kb, &skills).unwrap();
        assert_eq!(ready.steps[0].brief, "Review the diff");
        assert_eq!(ready.steps[0].own_text(), "Review the diff");
        assert_ne!(ready.steps[0].prompt, "Review the diff");
        let plain = Plan::parse("a @x Do it").unwrap();
        let plain = plain.prepare(&kb, &skills).unwrap();
        assert_eq!(plain.steps[0].own_text(), "Do it");
    }

    #[test]
    fn words_in_backticks_are_examples_not_misplaced_options() {
        let dir = tempfile::tempdir().unwrap();
        let kb = KnowledgeBase::new(dir.path().join("kb"));
        let skills = Skills::new(dir.path().join("skills"));
        let squads = SquadStore::default();
        let warned = |task: &str| -> Vec<String> {
            let plan = Plan::parse(&format!("a @alice:app {task}")).unwrap();
            preflight(&plan, &[], &squads, &kb, &skills, &Recipients::default())
                .into_iter()
                .map(|p| p.message)
                .filter(|m| m.contains("task text"))
                .collect()
        };
        assert!(warned("Explain `skill=adr` to the model").is_empty());
        assert!(warned("Explain ``a skill=adr b`` to the model").is_empty());
        assert!(warned("Explain `unclosed skill=adr").is_empty());
        assert_eq!(
            warned("Explain `skill=adr` and verify=test"),
            [
                "verify=test is in the task text, so it isn't used as an option: options go before what to do; put it in backticks if it is an example"
            ]
        );
    }

    #[test]
    fn preflight_finds_what_would_fail_before_anything_runs() {
        let dir = tempfile::tempdir().unwrap();
        let kb = KnowledgeBase::new(dir.path().join("kb"));
        let skills = Skills::new(dir.path().join("skills"));
        let share = |name: &str, access: Access, checks: &[&str], worktrees: bool| ShareInfo {
            name: name.into(),
            access,
            checks: checks.iter().map(|c| (*c).to_owned()).collect(),
            worktrees,
            mcp: if worktrees {
                vec!["browser".into()]
            } else {
                vec![]
            },
            mcp_about: Default::default(),
            team: false,
            pull_requests: false,
            pull_request_approval: false,
        };
        let peer = |name: &str, shares: Vec<ShareInfo>| PeerInfo {
            name: name.into(),
            id: iroh::SecretKey::generate().public(),
            capabilities: vec![Provider::Copilot, Provider::Codex],
            shares,
            owner: None,
            role: None,
            load: None,
            models: Default::default(),
            offers: Default::default(),
        };
        let peers = vec![
            peer("alice", vec![share("app", Access::Full, &["test"], true)]),
            peer(
                "bob",
                vec![share("app", Access::WorkspaceWrite, &["test"], false)],
            ),
        ];
        let mut squads = SquadStore::default();
        squads
            .set("qa", vec!["bob".into(), "carol".into()])
            .unwrap();
        let none = Recipients::default();
        let check = |text: &str| {
            preflight(
                &Plan::parse(text).unwrap(),
                &peers,
                &squads,
                &kb,
                &skills,
                &none,
            )
            .into_iter()
            .map(|p| {
                format!(
                    "{}{}: {}",
                    if p.error { "E " } else { "W " },
                    p.step.unwrap_or_default(),
                    p.message
                )
            })
            .collect::<Vec<_>>()
        };
        let good = "claim: C-1 tests pass\nfix @alice:app verify=test skill=implement-change claims=C-1 Fix it\nok after fix approve @me claims=C-1 Approve\nlook @alice:app mcp=browser Open the app";
        assert!(check(good).is_empty(), "{:?}", check(good));
        // An agent's MCP servers must be offered by the step's folder, like its own.
        skills
            .add_agent("web", "---\nmcp-servers: [browser]\n---\nTest the web app.")
            .unwrap();
        assert!(check("a @alice:app agent=web Test it").is_empty());
        let agents = check("b @bob:app agent=web Test it\nc @alice:app agent=ghost Do it");
        assert!(
            agents[0].starts_with(
                "E b: agent web can't work here: folder 'app' on bob offers you no MCP server browser"
            ),
            "{agents:?}"
        );
        let own_too = check("b @bob:app agent=web mcp=db Test it");
        assert!(
            own_too[0].starts_with("E b: folder 'app' on bob offers you no MCP server browser, db"),
            "the step's own servers are missing too: {own_too:?}"
        );
        assert!(
            agents
                .iter()
                .any(|p| p.starts_with("E c: there is no agent 'ghost'")),
            "{agents:?}"
        );
        let spec_on_bob = "s @bob:app verify=spec:spec,spec:trace skill=write-spec Write the spec";
        assert!(
            check(spec_on_bob).iter().all(|p| !p.starts_with("E ")),
            "built-in checks need no owner check or full access: {:?}",
            check(spec_on_bob)
        );
        // A group's name where its checks are meant says what they are called.
        let group = check("s @alice:app verify=spec Check the spec");
        assert!(
            group[0].ends_with(
                "; the built-in spec checks are spec:spec, spec:api, spec:ux, spec:trace (verify=spec:spec,spec:api,spec:ux,spec:trace)"
            ),
            "{group:?}"
        );

        let bad = check(
            "a @zed do\nb @alice:web do\nc @alice:app verify=lint do\nd @bob:app verify=test do\n\
             e after c #qa:app apply=c do\nf #ops do\ng @me do\nh @alice:app skill=nope context=gone do\n\
             i @bob:app mcp=browser do\nj @alice:app using codex mcp=browser do",
        );
        let expect = [
            "E a: zed is not connected",
            "E b: alice shares no folder 'web' with you (it shares: app)",
            "E c: folder 'app' on alice has no check lint (it has: test)",
            "E d: verify= with the folder's own checks needs full access, you have workspace-write in 'app' on bob",
            "E e: apply= needs edit access in a folder shared with --worktrees; 'app' on bob isn't",
            "E f: there is no squad #ops",
            "E g: @me can only approve",
            "E h: there is no skill 'nope'",
            "E h: the knowledge base has no topic 'gone'",
            "E i: folder 'app' on bob offers you no MCP server browser (you may use: none)",
            "E j: mcp= needs copilot or claude",
        ];
        for e in expect {
            assert!(
                bad.iter().any(|b| b.starts_with(e)),
                "missing {e:?} in {bad:#?}"
            );
        }
        let claims = check("claim: C-1 x\nclaim: C-2 y\na @alice:app claims=C-1 do");
        assert!(
            claims
                .iter()
                .any(|c| c.starts_with("W : Nothing proves C-2, so it will end unverified.")),
            "{claims:?}"
        );
        assert!(
            claims.iter().any(|c| c.starts_with(
                "W a: a claims C-1 but nothing checks its work, so C-1 can only end unverified."
            )),
            "{claims:?}"
        );

        // Claim problems come with the changes that fix them.
        let fixes = |text: &str| {
            preflight(
                &Plan::parse(text).unwrap(),
                &peers,
                &squads,
                &kb,
                &skills,
                &none,
            )
            .into_iter()
            .map(|p| (p.step, p.message, p.fixes))
            .collect::<Vec<_>>()
        };
        let remove = |step: &str, option: &str| Edit::Remove {
            step: step.into(),
            option: option.into(),
        };
        let add = |step: &str, option: &str| Edit::Add {
            step: step.into(),
            option: option.into(),
        };
        // A sign-off proves the claim too: the step without checks only spoils it.
        let both = fixes(
            "claim: C-1 x\nstep1 @alice:app claims=C-1 Write it\nreview after step1 approve @me claims=C-1 Check it",
        );
        assert_eq!(both.len(), 1, "{both:?}");
        let (step, message, offered) = &both[0];
        assert_eq!(step.as_deref(), Some("step1"));
        assert!(
            message.contains("so C-1 will end unverified even if the sign-off review is approved"),
            "{message}"
        );
        assert_eq!(
            offered,
            &[
                Fix {
                    label: "Let review prove C-1".into(),
                    edits: vec![remove("step1", "claims=C-1")],
                },
                Fix {
                    label: "Check step1's work with the folder's test check".into(),
                    edits: vec![add("step1", "verify=test")],
                },
            ]
        );
        // No check to offer on bob's folder (not full access); a sign-off after it
        // takes the claim over, or a new one is added.
        let after = fixes("claim: C-1 x\na @bob:app claims=C-1 do\nok after a approve @me Approve");
        assert_eq!(
            after[0].2,
            [Fix {
                label: "Let ok prove C-1".into(),
                edits: vec![remove("a", "claims=C-1"), add("ok", "claims=C-1")],
            }]
        );
        let alone = fixes("claim: C-1 x\nreview @bob:app do\na @bob:app claims=C-1 do");
        assert_eq!(
            alone[0].2,
            [Fix {
                label: "Prove C-1 with your sign-off on a".into(),
                edits: vec![
                    remove("a", "claims=C-1"),
                    Edit::AddLine {
                        after: "a".into(),
                        line: "review-2 after a approve @me claims=C-1 Check the result of a and approve it".into(),
                    },
                ],
            }]
        );
        let unproven = fixes("claim: C-1 x\nclaim: C-2 y\na @alice:app verify=test claims=C-1 do");
        assert_eq!(
            unproven[0].2,
            [
                Fix {
                    label: "Prove C-2 with your sign-off".into(),
                    edits: vec![Edit::AddLine {
                        after: "a".into(),
                        line: "review after a approve @me claims=C-2 Approve if C-2 holds: y"
                            .into(),
                    }],
                },
                Fix {
                    label: "Remove claim C-2".into(),
                    edits: vec![Edit::RemoveClaim {
                        claim: "C-2".into()
                    }],
                },
            ]
        );
        assert_eq!(
            serde_json::to_value(&unproven[0].2[0].edits[0]).unwrap(),
            serde_json::json!({ "do": "add_line", "after": "a", "line": "review after a approve @me claims=C-2 Approve if C-2 holds: y" })
        );
        let late = check("a @alice:app Fix it skill=implement-change");
        assert_eq!(
            late,
            [
                "W a: skill=implement-change is in the task text, so it isn't used as an option: options go before what to do; put it in backticks if it is an example"
            ]
        );
        assert!(check("a @alice:app Set the=value and verify the result").is_empty());
        // The plan editor's placeholders, never typed over.
        let todo = check(
            "claim: C-1 What must be true at the end\na @alice:app verify=test claims=C-1 What to do",
        );
        assert_eq!(
            todo,
            [
                "E a: the task is still \"What to do\": say what the step should do",
                "W : claim C-1 still says \"What must be true at the end\": say what must be true",
            ]
        );
        // Coded, for the plan builder to say on the field instead.
        let coded = |text: &str| {
            preflight(
                &Plan::parse(text).unwrap(),
                &peers,
                &squads,
                &kb,
                &skills,
                &none,
            )
            .into_iter()
            .map(|p| (p.code, p.step, p.claim))
            .collect::<Vec<_>>()
        };
        assert_eq!(
            coded("claim: C-1 What must be true at the end\na @alice:app claims=C-1 What to do"),
            [
                (Some("todo-task"), Some("a".to_owned()), None),
                (Some("todo-claim"), None, Some("C-1".to_owned())),
                (
                    Some("unchecked-claim"),
                    Some("a".to_owned()),
                    Some("C-1".to_owned())
                ),
            ]
        );
    }

    /// `team @lead` hands a step to a lead's team (as many levels down as `levels=`
    /// allows); `timeout=` limits any step. A team step's lead must offer a team plan
    /// in that folder, with edit access, and the team run proves its claims.
    #[test]
    fn team_steps_and_timeouts_parse_and_are_checked() {
        let plan = Plan::parse(
            "claim: C-1 x\n\
             build team @alice:app levels=2 timeout=90m claims=C-1 Build it\n\
             ok after build approve @me timeout=1d Check it",
        )
        .unwrap();
        assert_eq!(
            (plan.steps[0].team, plan.steps[0].timeout),
            (Some(2), Some(5400))
        );
        assert_eq!(plan.steps[1].timeout, Some(86_400));
        assert_eq!(timeout_text(5400), "90m");
        // After who runs it, "team" is a word of the task.
        let words = Plan::parse("x @alice team up with bob").unwrap();
        assert_eq!(
            (words.steps[0].team, words.steps[0].prompt.as_str()),
            (None, "team up with bob")
        );
        let err = |text: &str| Plan::parse(text).unwrap_err().to_string();
        assert!(err("x team @a verify=test do").contains("proven by the lead's team plan"));
        assert!(err("x @a levels=2 do").contains("levels= is for team steps"));
        assert!(err("x team @a levels=9 do").contains("levels must be between 1 and 3"));
        assert!(err("x @a timeout=soon do").contains("timeout= needs"));
        assert!(err("x @a timeout=8d do").contains("at most 7 days"));

        let lead = |team: bool, access: Access| PeerInfo {
            name: "alice".into(),
            id: iroh::SecretKey::generate().public(),
            capabilities: Vec::new(),
            shares: vec![ShareInfo {
                name: "app".into(),
                access,
                team,
                ..Default::default()
            }],
            owner: None,
            role: None,
            load: None,
            models: Default::default(),
            offers: Default::default(),
        };
        let dir = tempfile::tempdir().unwrap();
        let kb = KnowledgeBase::new(dir.path().join("kb"));
        let skills = Skills::new(dir.path().join("skills"));
        let problems = |peer: PeerInfo| -> Vec<String> {
            preflight(
                &plan,
                &[peer],
                &SquadStore::default(),
                &kb,
                &skills,
                &Recipients::default(),
            )
            .into_iter()
            .filter(|p| p.step.as_deref() == Some("build"))
            .map(|p| p.message)
            .collect()
        };
        assert!(problems(lead(false, Access::Full))[0].contains("has no team plan"));
        assert!(problems(lead(true, Access::ReadOnly))[0].contains("needs edit access"));
        // No provider needed, and its claim needs no check of its own.
        assert_eq!(
            problems(lead(true, Access::WorkspaceWrite)),
            Vec::<String>::new()
        );
    }

    #[test]
    fn pull_request_steps_open_the_work_before_them_in_their_folder() {
        let text = "impl @bob:app Write the parser\n\
                    check after impl @bob:app apply=impl verify=test Check it\n\
                    ok after check approve @me Ship it?\n\
                    ship after ok pr @bob:app";
        let plan = Plan::parse(text).unwrap();
        let pr = &plan.steps[3];
        assert!(pr.pr && !pr.approve && pr.team.is_none(), "{pr:?}");
        assert_eq!((pr.share.as_deref(), pr.prompt.as_str()), (Some("app"), ""));
        let titled = Plan::parse("a @bob:app do\np after a pr @bob:app Add the parser").unwrap();
        assert_eq!(titled.steps[1].prompt, "Add the parser");
        // After who runs it, "pr" is a word of the task.
        assert!(!Plan::parse("x @bob pr review").unwrap().steps[0].pr);

        let err = |text: &str| Plan::parse(text).unwrap_err().to_string();
        for (text, why) in [
            ("a @bob:app do\np after a pr @bob Go", "needs @peer:folder"),
            (
                "a #devs:app do\np after a pr #devs:app Go",
                "needs @peer:folder",
            ),
            (
                "a @bob:app do\np after a pr @me:app Go",
                "needs @peer:folder",
            ),
            (
                "a @bob:app do\np after a pr @bob:app verify=test Go",
                "can't have verify=",
            ),
            (
                "a @bob:app do\np after a pr @bob:app model=x Go",
                "can't have model=",
            ),
            (
                "a @bob:app do\np after a pr @bob:app using claude Go",
                "can't have using",
            ),
            (
                "a @bob:app do\np after a pr @bob:app apply=a Go",
                "can't have apply=",
            ),
            (
                "a @bob:other do\np after a pr @bob:app Go",
                "comes after no step that works in @bob:app",
            ),
            (
                "a @alice:app do\np after a pr @bob:app Go",
                "comes after no step",
            ),
            (
                "a @bob:app do\nok after a approve @me Fine?\np pr @bob:app Go",
                "comes after no step",
            ),
            (
                "a @bob:app do\nb @bob:app do\np after a,b pr @bob:app Go",
                "steps a and b that work in @bob:app on separate branches",
            ),
            (
                "a @bob:app do\nb after a @bob:app do\np after b pr @bob:app Go",
                "separate branches",
            ),
        ] {
            assert!(err(text).contains(why), "{text}: {}", err(text));
        }
        // A squad's member may be its peer: the squad's work counts.
        Plan::parse("a #devs:app do\np after a pr @bob:app Go").unwrap();

        // Its peer must take pull request steps in the folder, with edit access for you.
        let bob = |pull_requests: bool, access: Access| PeerInfo {
            name: "bob".into(),
            id: iroh::SecretKey::generate().public(),
            capabilities: Vec::new(),
            shares: vec![ShareInfo {
                name: "app".into(),
                access,
                worktrees: true,
                pull_requests,
                ..Default::default()
            }],
            owner: None,
            role: None,
            load: None,
            models: Default::default(),
            offers: Default::default(),
        };
        assert!(
            unable(pr, &bob(false, Access::Full))
                .unwrap()
                .contains("doesn't take pull request steps")
        );
        assert!(
            unable(pr, &bob(true, Access::ReadOnly))
                .unwrap()
                .contains("needs edit access")
        );
        // It runs no model: bob needn't serve a provider.
        assert_eq!(unable(pr, &bob(true, Access::WorkspaceWrite)), None);

        // It opens the branch that has all the work (check's: it applied impl's), titled
        // with the words of the step that started it.
        let key = iroh::SecretKey::generate();
        let done = |peer: &str, branch: &str, commit: Option<&str>| {
            let mut evidence = Evidence::new(
                crate::evidence::EvidenceKind::Task,
                peer,
                key.public(),
                key.public(),
                1,
            );
            evidence.git = Some(crate::evidence::GitCommit {
                branch: branch.into(),
                base: "base".into(),
                commit: commit.map(Into::into),
                files: vec!["parser.rs".into()],
                patch_sha256: None,
                left_out: Vec::new(),
            });
            StepState::Done {
                peer: peer.into(),
                output: "did it".into(),
                evidence: Some(evidence),
                verified: true,
                patch: None,
                model: None,
            }
        };
        let mut states: BTreeMap<String, StepState> = [
            ("impl", done("bob", "p2p/boss/1-1", Some("c1"))),
            ("check", done("bob", "p2p/boss/2-2", Some("c2"))),
            ("ok", StepState::Waiting),
            ("ship", StepState::Waiting),
        ]
        .into_iter()
        .map(|(id, s)| (id.to_owned(), s))
        .collect();
        let ask = pr_ask(&plan, pr, &states).unwrap();
        assert_eq!(
            (ask.branch.as_str(), ask.commit.as_str(), ask.title.as_str()),
            ("p2p/boss/2-2", "c2", "Write the parser")
        );
        let ask = pr_ask(&titled, &titled.steps[1], &{
            let mut s = BTreeMap::new();
            s.insert("a".to_owned(), done("bob", "p2p/boss/3-3", Some("c3")));
            s
        })
        .unwrap();
        assert_eq!(ask.title, "Add the parser");
        // Work that ran elsewhere, or committed nothing, isn't there to open.
        states.insert("check".into(), done("alice", "p2p/boss/2-2", Some("c2")));
        assert_eq!(pr_ask(&plan, pr, &states).unwrap().branch, "p2p/boss/1-1");
        states.insert("impl".into(), done("bob", "p2p/boss/1-1", None));
        assert!(
            pr_ask(&plan, pr, &states)
                .unwrap_err()
                .to_string()
                .contains("no step before it committed work to 'app' on bob"),
        );
    }

    #[test]
    fn pull_request_steps_ask_who_signed_off_or_the_reviewers_they_name() {
        let pr = |options: &str| {
            Plan::parse(&format!(
                "a @bob:app do\nok after a approve @carol Fine?\nme-ok after a approve @me Fine?\np after ok,me-ok pr @bob:app {options} Go"
            ))
            .unwrap()
        };
        assert_eq!(pr("").steps[3].reviewers, None);
        assert_eq!(
            pr("reviewers=me,carol reviewer=octocat,carol").steps[3].reviewers,
            Some(vec!["me".into(), "carol".into(), "octocat".into()])
        );
        assert_eq!(pr("reviewers=None").steps[3].reviewers, Some(Vec::new()));
        let err = |text: &str| Plan::parse(text).unwrap_err().to_string();
        for (text, why) in [
            (
                "a @bob:app reviewers=me do",
                "reviewers= is for pull request steps",
            ),
            (
                "a @bob:app do\np after a pr @bob:app reviewers=none,me Go",
                "reviewers=none asks no one",
            ),
            (
                "a @bob:app do\np after a pr @bob:app reviewers=a/b Go",
                "needs GitHub logins, peer names, me or none, got \"a/b\"",
            ),
            (
                "a @bob:app do\np after a pr @bob:app reviewers=, Go",
                "reviewers= needs GitHub logins",
            ),
            (
                "a @bob:app do\np after a pr @bob:app reviewers=a,b,c,d,e,f,g,h,i,j,k Go",
                "at most 10 reviewers",
            ),
        ] {
            assert!(err(text).contains(why), "{text}: {}", err(text));
        }

        // You are me-gh on GitHub; the allowlist knows carol's account, not dave's.
        let key = || iroh::SecretKey::generate().public();
        let (own, carol, dave) = (key(), key(), key());
        let mut trust = TrustStore::default();
        for (name, id, github) in [("carol", carol, Some("carol-gh")), ("dave", dave, None)] {
            trust.allow(crate::trust::TrustedPeer {
                name: name.into(),
                id,
                addrs: Default::default(),
                can_delegate: false,
                tasks_per_hour: 5,
                access: Access::ReadOnly,
                shares: Vec::new(),
                m365: None,
                github: github.map(Into::into),
            });
        }
        let mut recipients = Recipients::new(trust, Vec::new());
        recipients.profile = Some("boss".into());
        recipients.github = Some("me-gh".into());
        let signed = |by: EndpointId| crate::pulls::SignOff {
            step: "ok".into(),
            output: "Approved".into(),
            evidence: Evidence::new(crate::evidence::EvidenceKind::Approval, "x", by, own, 1),
        };
        // Naming no one, it asks those who signed off whose accounts are known, each once.
        let signoffs = [signed(carol), signed(own), signed(dave), signed(carol)];
        let asks = |plan: &Plan| {
            reviewers_for(
                plan.steps[3].reviewers.as_deref(),
                &signoffs,
                &own,
                &recipients,
            )
        };
        assert_eq!(asks(&pr("")), ["carol-gh", "me-gh"]);
        assert_eq!(
            asks(&pr("reviewers=me,carol,dave,octocat,ME-GH")),
            ["me-gh", "carol-gh", "octocat"]
        );
        assert!(asks(&pr("reviewers=none")).is_empty());
        assert_eq!(
            parse_reviewers("@me, carol octocat").unwrap(),
            ["me", "carol", "octocat"]
        );
        assert!(parse_reviewers(" none ").unwrap().is_empty());
        assert!(parse_reviewers("me,none").is_err() && parse_reviewers("a/b").is_err());

        // The plan check says who isn't asked, and why.
        let check = |text: &str, recipients: &Recipients| {
            let plan = Plan::parse(text).unwrap();
            let step = plan.steps.iter().find(|s| s.pr).unwrap().clone();
            reviewer_problems(&plan, &step, recipients)
        };
        let three = "a @bob:app do\nok after a approve @carol Fine?\nd-ok after a approve @dave Fine?\nme-ok after a approve @me Fine?\np after ok,d-ok,me-ok pr @bob:app";
        assert_eq!(
            check(three, &recipients),
            [(
                false,
                "dave signs off in step 'd-ok', but the allowlist has no GitHub account for dave, so dave isn't asked to review the pull request; set one with: proofkin peers github dave <login> --profile boss; or ask no one with reviewers=none".to_owned()
            )]
        );
        let mut signed_out = recipients.clone();
        signed_out.github = None;
        let found = check(three, &signed_out);
        assert!(
            found.len() == 2
                && found[1].1.starts_with(
                    "you sign off in step 'me-ok', but this profile isn't signed in to GitHub, so you aren't asked to review the pull request; sign in with: proofkin github login --gh-user <login> --profile boss"
                ),
            "{found:?}"
        );
        assert!(check(&format!("{three} reviewers=none"), &signed_out).is_empty());
        let found = check(
            "a @bob:app do\np after a pr @bob:app reviewers=me,carol,dave,octocat,x.y Go",
            &signed_out,
        );
        let errors: Vec<bool> = found.iter().map(|(error, _)| *error).collect();
        assert_eq!(errors, [false, false, true], "{found:?}");
        assert!(
            found[0]
                .1
                .starts_with("reviewers= names me, but this profile isn't signed in")
        );
        assert!(found[1].1.starts_with(
            "reviewers= names dave, but the allowlist has no GitHub account for dave"
        ));
        assert_eq!(
            found[2].1,
            "reviewers= names \"x.y\", who isn't on the allowlist and can't be a GitHub login"
        );
    }

    #[test]
    fn sign_offs_say_how_often_their_approver_may_ask_for_changes() {
        let plan = Plan::parse(
            "a @bob:app do\n\
             ok after a approve @me Fine?\n\
             strict after a approve @carol changes=0 Fine?\n\
             patient after a approve @carol changes=5 Fine?",
        )
        .unwrap();
        let rounds: Vec<Option<u32>> = plan.steps.iter().map(|s| s.changes).collect();
        assert_eq!(rounds, [None, None, Some(0), Some(5)]);
        let err = |text: &str| Plan::parse(text).unwrap_err().to_string();
        for (text, why) in [
            ("a @bob changes=1 do", "changes= is for sign-offs"),
            (
                "a @bob do\nok after a approve @me changes=6 Fine?",
                "at most 5 rounds",
            ),
            (
                "a @bob do\nok after a approve @me changes=x Fine?",
                "changes= needs a number",
            ),
            (
                "a @bob do\nok after a approve @me changes=1 changes=2 Fine?",
                "changes= is given twice",
            ),
        ] {
            assert!(err(text).contains(why), "{text}: {}", err(text));
        }
        // Only the work a model did that it waits for directly is done again.
        let plan = Plan::parse(
            "a @bob:app do\nb @bob:app do\nfirst after a approve @me Fine?\nok after b,first approve @me Fine?",
        )
        .unwrap();
        let done = |peer: &str| StepState::Done {
            peer: peer.into(),
            output: "ok".into(),
            evidence: None,
            verified: true,
            patch: None,
            model: None,
        };
        let states: BTreeMap<String, StepState> = [
            ("a", done("bob")),
            ("b", done("bob")),
            ("first", done("you")),
            ("ok", StepState::Waiting),
        ]
        .into_iter()
        .map(|(id, s)| (id.to_owned(), s))
        .collect();
        let redo: Vec<&str> = redoable(&plan, &plan.steps[3], &states)
            .iter()
            .map(|s| s.id.as_str())
            .collect();
        assert_eq!(redo, ["b"]);
        assert!(redoable(&plan, &plan.steps[2], &BTreeMap::new()).is_empty());
    }

    #[test]
    fn a_finished_runs_work_opens_with_the_sign_offs_on_it() {
        use crate::evidence::{EvidenceKind, GitCommit};
        let text = "impl @bob:app Write the parser\n\
                    check after impl @carol:app apply=impl verify=test Check it\n\
                    docs @bob:docs Write the docs\n\
                    ok after check approve @carol Ship it?\n\
                    early after impl approve @me Looks right?\n\
                    late after docs approve @me Docs fine?";
        let plan = Plan::parse(text).unwrap();
        let key = || iroh::SecretKey::generate();
        let (own, bob, carol) = (key(), key(), key());
        let made = |by: &iroh::SecretKey, peer: &str, folder: &str, branch: &str| {
            let mut e = Evidence::new(EvidenceKind::Task, peer, by.public(), own.public(), 1);
            e.share = Some(folder.into());
            e.git = Some(GitCommit {
                branch: branch.into(),
                base: "base".into(),
                commit: Some(format!("{branch}-commit")),
                files: vec!["x".into()],
                patch_sha256: None,
                left_out: Vec::new(),
            });
            StepState::Done {
                peer: peer.into(),
                output: "did it".into(),
                evidence: Some(e.sign("did it", by)),
                verified: true,
                patch: None,
                model: None,
            }
        };
        let approved = |by: &iroh::SecretKey, name: &str| {
            let output = format!("Approved by {name}: fine");
            let mut e = Evidence::new(EvidenceKind::Approval, name, by.public(), own.public(), 2);
            e.approved = Some(true);
            StepState::Done {
                peer: name.into(),
                output: output.clone(),
                evidence: Some(e.sign(&output, by)),
                verified: true,
                patch: None,
                model: None,
            }
        };
        let states: BTreeMap<String, StepState> = [
            ("impl", made(&bob, "bob", "app", "p2p/boss/1")),
            ("check", made(&carol, "carol", "app", "p2p/boss/2")),
            ("docs", made(&bob, "bob", "docs", "p2p/boss/3")),
            ("ok", approved(&carol, "carol")),
            ("early", approved(&own, "you")),
            ("late", approved(&own, "you")),
        ]
        .into_iter()
        .map(|(id, s)| (id.to_owned(), s))
        .collect();
        let run =
            crate::runs::RunRecord::new("boss", &own.public().to_string(), text, &plan, &states, 0);
        // check's branch has impl's work too: it, and docs, can be opened.
        let work = run_work(&run);
        assert_eq!(
            work.iter()
                .map(|w| (
                    w.step.as_str(),
                    w.peer.as_str(),
                    w.folder.as_str(),
                    w.title.as_str()
                ))
                .collect::<Vec<_>>(),
            [
                ("check", "carol", "app", "Write the parser"),
                ("docs", "bob", "docs", "Write the docs")
            ]
        );
        assert!(
            run_pull_request(
                &run,
                "impl",
                None,
                None,
                &own.public(),
                &Recipients::default()
            )
            .unwrap_err()
            .to_string()
            .contains("another step builds on its work")
        );
        // It carries the sign-offs on any of its work, and asks those who gave them.
        let mut trust = TrustStore::default();
        trust.allow(crate::trust::TrustedPeer {
            name: "carol".into(),
            id: carol.public(),
            addrs: Default::default(),
            can_delegate: false,
            tasks_per_hour: 5,
            access: Access::ReadOnly,
            shares: Vec::new(),
            m365: None,
            github: Some("carol-gh".into()),
        });
        let mut recipients = Recipients::new(trust, Vec::new());
        recipients.github = Some("me-gh".into());
        let (w, ask) =
            run_pull_request(&run, "check", Some("  "), None, &own.public(), &recipients).unwrap();
        assert_eq!(
            (w.branch.as_str(), ask.commit.as_str(), ask.title.as_str()),
            ("p2p/boss/2", "p2p/boss/2-commit", "Write the parser")
        );
        let signed: Vec<&str> = ask.signoffs.iter().map(|s| s.step.as_str()).collect();
        assert_eq!(signed, ["ok", "early"]);
        assert_eq!(ask.reviewers, ["carol-gh", "me-gh"]);
        let named = ["octocat".to_owned()];
        let (_, docs) = run_pull_request(
            &run,
            "docs",
            Some("Docs"),
            Some(&named),
            &own.public(),
            &recipients,
        )
        .unwrap();
        assert_eq!(
            (
                docs.title.as_str(),
                docs.signoffs.len(),
                docs.reviewers.as_slice()
            ),
            ("Docs", 1, ["octocat".to_owned()].as_slice())
        );

        // Work a pull request step of the run saw merged is in already.
        let shipped = format!("{text}\nship after ok pr @carol:app");
        let plan = Plan::parse(&shipped).unwrap();
        let mut states = states;
        states.insert(
            "ship".into(),
            StepState::Done {
                peer: "carol".into(),
                output: "Pull request #1 was merged".into(),
                evidence: None,
                verified: false,
                patch: None,
                model: None,
            },
        );
        let run = crate::runs::RunRecord::new(
            "boss",
            &own.public().to_string(),
            &shipped,
            &plan,
            &states,
            0,
        );
        let steps: Vec<String> = run_work(&run).into_iter().map(|w| w.step).collect();
        assert_eq!(steps, ["docs"]);
    }

    #[test]
    fn squad_steps_pick_the_least_busy_capable_member() {
        let peer = |name: &str, providers: Vec<Provider>, shares: &[&str]| PeerInfo {
            name: name.into(),
            id: iroh::SecretKey::generate().public(),
            capabilities: providers,
            shares: shares
                .iter()
                .map(|s| ShareInfo {
                    name: (*s).into(),
                    access: Access::ReadOnly,
                    ..Default::default()
                })
                .collect(),
            owner: None,
            role: None,
            load: None,
            models: Default::default(),
            offers: Default::default(),
        };
        let peers = vec![
            peer("alice", vec![Provider::Copilot], &["data", "code"]),
            peer("bob", vec![Provider::Copilot], &["data", "code"]),
            peer("carol", vec![Provider::Claude], &["data", "code"]),
            peer("dave", vec![Provider::Copilot], &["data"]),
            peer("outsider", vec![Provider::Copilot], &["data", "code"]),
        ];
        let mut squads = SquadStore::default();
        squads
            .set(
                "qa",
                vec![
                    "alice".into(),
                    "bob".into(),
                    "carol".into(),
                    "dave".into(),
                    "erin".into(),
                ],
            )
            .unwrap();
        let step = Plan::parse("t #qa:code test it").unwrap().steps.remove(0);

        let mut busy = HashMap::new();
        assert_eq!(
            choose_peer(&step, &peers, &squads, &busy).unwrap().name,
            "alice"
        );
        busy.insert("alice".to_owned(), 1);
        assert_eq!(
            choose_peer(&step, &peers, &squads, &busy).unwrap().name,
            "bob"
        );
        busy.insert("bob".to_owned(), 2);
        assert_eq!(
            choose_peer(&step, &peers, &squads, &busy).unwrap().name,
            "alice",
            "carol lacks copilot, dave lacks the folder, erin is offline, outsider isn't a member"
        );

        let claude = Plan::parse("t #qa using claude review")
            .unwrap()
            .steps
            .remove(0);
        assert_eq!(
            choose_peer(&claude, &peers, &squads, &busy).unwrap().name,
            "carol"
        );
        let codex = Plan::parse("t #qa using codex review")
            .unwrap()
            .steps
            .remove(0);
        assert!(choose_peer(&codex, &peers, &squads, &busy).is_err());
        let nosquad = Plan::parse("t #ops review").unwrap().steps.remove(0);
        assert!(choose_peer(&nosquad, &peers, &squads, &busy).is_err());
        let direct = Plan::parse("t @dave:code review").unwrap().steps.remove(0);
        assert!(choose_peer(&direct, &peers, &squads, &busy).is_err());
        // A squad step that needs an MCP server goes only to members that offer it.
        let mut with_mcp = peers.clone();
        with_mcp[1].shares[1].mcp = vec!["browser".into()];
        let look = Plan::parse("t #qa:code mcp=browser look")
            .unwrap()
            .steps
            .remove(0);
        assert_eq!(
            choose_peer(&look, &with_mcp, &squads, &busy).unwrap().name,
            "bob",
            "alice is less busy but offers no browser"
        );
        let err = choose_peer(&look, &peers, &squads, &busy)
            .unwrap_err()
            .to_string();
        assert!(err.contains("offers you no MCP server browser"), "{err}");

        // A step's model goes to members that allow it (their hello says which), even
        // when another is less busy; if none does, it goes as before and gets refused.
        let mut picky = peers.clone();
        picky[0].models.insert(
            Provider::Copilot,
            crate::models::ModelChoice {
                default: Some("gpt-6".into()),
                allowed: vec!["gpt-6".into()],
                ..Default::default()
            },
        );
        let busy = HashMap::new();
        let opus = Plan::parse("t #qa:code model=claude-opus-5.5 test it")
            .unwrap()
            .steps
            .remove(0);
        assert_eq!(
            choose_peer(&opus, &picky, &squads, &busy).unwrap().name,
            "bob",
            "alice is less busy but allows only gpt-6"
        );
        let gpt = Plan::parse("t #qa:code model=GPT-6 test it")
            .unwrap()
            .steps
            .remove(0);
        assert_eq!(
            choose_peer(&gpt, &picky, &squads, &busy).unwrap().name,
            "alice"
        );
        picky[1].models = picky[0].models.clone();
        assert_eq!(
            choose_peer(&opus, &picky, &squads, &busy).unwrap().name,
            "alice",
            "none allows it: the least busy, which will say why it refuses"
        );
        // The same for its reasoning effort.
        picky[0].models.insert(
            Provider::Copilot,
            crate::models::ModelChoice {
                efforts: vec!["low".into()],
                ..Default::default()
            },
        );
        picky[1].models.clear();
        let deep = Plan::parse("t #qa:code effort=max test it")
            .unwrap()
            .steps
            .remove(0);
        assert_eq!(
            choose_peer(&deep, &picky, &squads, &busy).unwrap().name,
            "bob",
            "alice is less busy but allows only low reasoning"
        );
    }

    fn step_labels(plan: &Plan, id: &str) -> Option<Vec<String>> {
        plan.steps.iter().find(|s| s.id == id).unwrap().m365.clone()
    }

    #[test]
    fn steps_with_microsoft_365_content_are_marked_through_context_and_after() {
        let dir = tempfile::tempdir().unwrap();
        let kb = KnowledgeBase::new(dir.path());
        kb.write("hr", &with_mark("Pay bands", &["HR".into()]), "")
            .unwrap();
        kb.write(
            "pub",
            &with_mark("Press", &["Public".into(), "hr".into()]),
            "",
        )
        .unwrap();
        kb.write("plain", "Architecture", "").unwrap();
        let plan = Plan::parse(
            "a @x context=hr do\nb @x context=plain do\nc after a @x do\n\
             d after c,b @x context=pub do\ne after b @x do\nf @x context=all do\n\
             g @x context=gone do",
        )
        .unwrap();
        let marked = plan.marked(&kb);
        let labels = |id: &str| step_labels(&marked, id);
        let list = |names: &[&str]| -> Option<Vec<String>> {
            Some(names.iter().map(|l| (*l).to_owned()).collect())
        };
        assert_eq!(labels("a"), list(&["HR"]), "a marked context topic");
        assert_eq!(labels("b"), None, "an unmarked topic");
        assert_eq!(labels("c"), list(&["HR"]), "after a marked step");
        assert_eq!(labels("d"), list(&["Public", "hr"]), "the union, once each");
        assert_eq!(labels("e"), None, "after an unmarked step");
        assert_eq!(labels("f"), list(&["HR", "Public"]), "context=all");
        assert_eq!(labels("g"), None, "a topic that doesn't exist");
        assert_eq!(
            step_labels(&plan, "a"),
            None,
            "the plan itself isn't changed"
        );

        // What the coming workiq= option and @workiq steps will set is passed on too.
        let mut plan = Plan::parse("w @x do\nn after w @x do\nm @x do").unwrap();
        plan.steps[0].m365 = Some(vec!["Confidential".into()]);
        let marked = plan.marked(&kb);
        assert_eq!(step_labels(&marked, "n"), list(&["Confidential"]));
        assert_eq!(step_labels(&marked, "m"), None);

        // The plan that is sent carries the marks.
        let ready = Plan::parse("a @x context=hr do\nb after a @x do")
            .unwrap()
            .with_knowledge(&kb)
            .unwrap();
        assert_eq!(step_labels(&ready, "b"), list(&["HR"]));
    }

    #[test]
    fn save_writes_a_marked_steps_mark_into_its_topic() {
        let dir = tempfile::tempdir().unwrap();
        let kb = KnowledgeBase::new(dir.path());
        kb.write("hr", &with_mark("Pay bands", &["HR".into()]), "")
            .unwrap();
        let plan = Plan::parse(
            "a @x context=hr save=notes do\nb after a @x save=more do\nc @x save=plain do",
        )
        .unwrap();
        let done = |output: &str| StepState::Done {
            peer: "x".into(),
            output: output.into(),
            evidence: None,
            verified: true,
            patch: None,
            model: None,
        };
        let states: BTreeMap<String, StepState> = [
            ("a".to_owned(), done("Bands are fixed.")),
            ("b".to_owned(), done("Follow-up.")),
            ("c".to_owned(), done("Architecture.")),
        ]
        .into();
        assert_eq!(
            save_knowledge(&kb, &plan, &states, "r1"),
            ["notes", "more", "plain"]
        );
        let hr = Some(vec!["HR".to_owned()]);
        let notes = kb.read("notes").unwrap();
        assert_eq!(crate::knowledge::parse_mark(&notes), hr);
        assert!(notes.contains("Bands are fixed."), "{notes}");
        assert_eq!(crate::knowledge::parse_mark(&kb.read("more").unwrap()), hr);
        assert_eq!(
            crate::knowledge::parse_mark(&kb.read("plain").unwrap()),
            None,
            "a step without Microsoft 365 content saves a plain topic"
        );
        // So a later step that uses the saved topic is marked too.
        let later = Plan::parse("d @x context=notes do").unwrap().marked(&kb);
        assert_eq!(step_labels(&later, "d"), hr);
    }

    #[test]
    fn a_workiq_step_parses_and_everything_that_would_make_an_answer_proof_is_refused() {
        let plan = Plan::parse(
            "a @x do\nb after a @workiq agent=sales-agent:v1.2@acme save=notes What changed?",
        )
        .unwrap();
        let step = &plan.steps[1];
        assert_eq!(step.target, Target::Workiq);
        assert!(step.is_workiq() && step.brings_m365());
        assert_eq!(step.agent.as_deref(), Some("sales-agent:v1.2@acme"));
        assert_eq!(step.workiq_name(), "workiq:sales-agent:v1.2@acme");
        assert_eq!(step.target.to_string(), "@workiq");
        assert_eq!(
            Plan::parse("a @workiq hi").unwrap().steps[0].workiq_name(),
            "workiq"
        );
        // The longest id is allowed; anything past it, or a stray character, is not.
        let longest = "a".repeat(MAX_WORKIQ_AGENT_ID);
        assert!(Plan::parse(&format!("a @workiq agent={longest} hi")).is_ok());
        for bad in [format!("{longest}a"), "bad/id".to_owned(), "a#b".to_owned()] {
            let err = Plan::parse(&format!("a @workiq agent={bad} hi"))
                .unwrap_err()
                .to_string();
            assert!(err.contains("needs a Work IQ agent id"), "{bad}: {err}");
        }

        for option in [
            "claims=C-1",
            "verify=test",
            "apply=x",
            "mcp=docs",
            "skill=adr",
            "attempts=2",
            "levels=2",
        ] {
            let err = Plan::parse(&format!("a @workiq {option} What changed?"))
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("@workiq step 'a'")
                    && err.contains(option.split('=').next().unwrap())
                    && err.contains("context and never proof"),
                "{option}: {err}"
            );
        }
        for text in ["a team @workiq do", "a approve @workiq ok?"] {
            let err = Plan::parse(text).unwrap_err().to_string();
            assert!(err.contains("context and never proof"), "{text}: {err}");
        }
        for (text, what) in [
            ("a @workiq context=hr do", "context="),
            ("a @workiq workiq=all do", "workiq="),
            ("a @workiq:app do", "a folder"),
        ] {
            let err = Plan::parse(text).unwrap_err().to_string();
            assert!(err.contains(what), "{text}: {err}");
        }
        // The round trip through JSON keeps the target.
        let json = serde_json::to_string(&plan.steps[1].target).unwrap();
        assert_eq!(json, r#"{"kind":"workiq"}"#);
    }

    #[test]
    fn a_workiq_step_is_marked_and_save_keeps_the_mark_and_says_it_was_an_answer() {
        let dir = tempfile::tempdir().unwrap();
        let kb = KnowledgeBase::new(dir.path());
        let plan = Plan::parse("a @workiq save=notes What changed?\nb after a @x use it").unwrap();
        let marked = plan.marked(&kb);
        assert_eq!(step_labels(&marked, "a"), Some(Vec::new()));
        assert_eq!(step_labels(&marked, "b"), Some(Vec::new()));

        let states: BTreeMap<String, StepState> = [(
            "a".to_owned(),
            StepState::Done {
                peer: "workiq".into(),
                output: "The launch moved to May.".into(),
                evidence: None,
                verified: false,
                patch: None,
                model: None,
            },
        )]
        .into();
        assert_eq!(save_knowledge(&kb, &plan, &states, "r1"), ["notes"]);
        let notes = kb.read("notes").unwrap();
        assert_eq!(crate::knowledge::parse_mark(&notes), Some(Vec::new()));
        assert!(
            notes.contains("step a was a Work IQ answer from workiq"),
            "{notes}"
        );
    }

    #[test]
    fn the_plan_check_needs_the_work_iq_account_for_workiq_steps() {
        let dir = tempfile::tempdir().unwrap();
        let kb = KnowledgeBase::new(dir.path().join("kb"));
        let skills = Skills::new(dir.path().join("skills"));
        let peer = PeerInfo {
            name: "plain".into(),
            id: iroh::SecretKey::generate().public(),
            capabilities: vec![Provider::Copilot],
            shares: vec![ShareInfo {
                name: crate::shares::DATA_SHARE.into(),
                access: Access::ReadOnly,
                checks: Vec::new(),
                worktrees: false,
                mcp: Vec::new(),
                mcp_about: Default::default(),
                team: false,
                pull_requests: false,
                pull_request_approval: false,
            }],
            owner: None,
            role: None,
            load: None,
            models: Default::default(),
            offers: Default::default(),
        };
        let mut trust = TrustStore::default();
        trust.allow(crate::trust::TrustedPeer {
            name: peer.name.clone(),
            id: peer.id,
            addrs: Default::default(),
            can_delegate: false,
            tasks_per_hour: 5,
            access: Access::ReadOnly,
            shares: vec![crate::shares::DATA_SHARE.into()],
            m365: None,
            github: None,
        });
        let mut recipients = Recipients::new(trust, Vec::new());
        recipients.profile = Some("boss".into());
        let errors = |text: &str, recipients: &Recipients| -> Vec<String> {
            preflight(
                &Plan::parse(text).unwrap(),
                std::slice::from_ref(&peer),
                &SquadStore::default(),
                &kb,
                &skills,
                recipients,
            )
            .into_iter()
            .filter(|p| p.error)
            .map(|p| p.message)
            .collect()
        };

        let found = errors("a @workiq What changed?", &recipients);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(
            found[0].contains("a @workiq step needs this profile's Work IQ sign-in")
                && found[0].contains("proofkin auth login workiq --profile boss"),
            "{}",
            found[0]
        );

        recipients.workiq_account = true;
        assert!(errors("a @workiq What changed?", &recipients).is_empty());

        // A peer with no Microsoft 365 grant can't take a step after it.
        let found = errors("a @workiq What changed?\nb after a @plain use", &recipients);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(
            found[0].contains("step 'b'") && found[0].contains("no Microsoft 365 grant"),
            "{}",
            found[0]
        );
    }

    #[test]
    fn workiq_sources_pick_the_strategy_and_capabilities_and_bad_ones_are_refused() {
        let request = |text: &str| {
            let plan = Plan::parse(text).unwrap();
            let (strategy, capabilities) = plan.steps[0].workiq_request().unwrap();
            (strategy, capabilities, plan.steps[0].workiq.clone())
        };
        assert_eq!(
            request("a @x workiq=all do"),
            ("copilot", Vec::new(), vec!["all".to_owned()])
        );
        assert_eq!(
            request("a @x workiq=email,files do").0,
            "grounding",
            "a list without connectors"
        );
        assert_eq!(
            request("a @x workiq=files,email do").1,
            ["Email", "OneDriveAndSharePoint"]
        );
        assert_eq!(
            request("a @x workiq=meetings,connectors do").0,
            "copilot",
            "only copilot searches connectors"
        );
        assert_eq!(
            request("a @x workiq=people workiq=teams,people do").1,
            ["TeamsMessages", "People"],
            "repeating the option adds to it"
        );
        let plain = Plan::parse("a @x do").unwrap();
        assert!(plain.steps[0].workiq_request().is_none());

        let long = format!("a @x workiq=all {}", "word ".repeat(400));
        let plan = Plan::parse(&long).unwrap();
        assert_eq!(plan.steps[0].workiq_query().chars().count(), 1000);

        for (text, expected) in [
            (
                "a @x workiq=email,slack do",
                "doesn't know the source \"slack\"",
            ),
            (
                "a @x workiq=email,slack do",
                "email, meetings, files, teams, people, connectors",
            ),
            (
                "a @x workiq=all,email do",
                "workiq=all already searches everything",
            ),
            ("a @x workiq=all workiq=files do", "workiq=all already"),
            ("a approve @me workiq=all ok?", "approval step 'a'"),
        ] {
            let err = Plan::parse(text).unwrap_err().to_string();
            assert!(err.contains(expected), "{text:?}: {err}");
        }
    }

    #[test]
    fn a_workiq_step_is_marked_and_its_grant_labels_go_to_the_steps_after() {
        let dir = tempfile::tempdir().unwrap();
        let kb = KnowledgeBase::new(dir.path());
        let plan = Plan::parse("w @x workiq=all find\nn after w @x use\nm @x other").unwrap();
        let marked = plan.marked(&kb);
        assert_eq!(
            step_labels(&marked, "w"),
            Some(Vec::new()),
            "marked, labels not known"
        );
        assert_eq!(step_labels(&marked, "n"), Some(Vec::new()));
        assert_eq!(step_labels(&marked, "m"), None);
        let seeds = HashMap::from([("w".to_owned(), vec!["General".to_owned()])]);
        let seeded = plan.marked_with(&kb, &seeds);
        assert_eq!(step_labels(&seeded, "n"), Some(vec!["General".to_owned()]));
        assert_eq!(
            step_labels(&plan, "w"),
            None,
            "the plan itself isn't changed"
        );
    }

    #[test]
    fn the_plan_check_needs_a_sign_in_and_a_recipient_for_workiq_steps() {
        use crate::trust::{M365Grant, TrustedPeer};
        let dir = tempfile::tempdir().unwrap();
        let kb = KnowledgeBase::new(dir.path().join("kb"));
        let skills = Skills::new(dir.path().join("skills"));
        let peer = |name: &str| PeerInfo {
            name: name.into(),
            id: iroh::SecretKey::generate().public(),
            capabilities: vec![Provider::Copilot],
            shares: vec![ShareInfo {
                name: crate::shares::DATA_SHARE.into(),
                access: Access::ReadOnly,
                checks: Vec::new(),
                worktrees: false,
                mcp: Vec::new(),
                mcp_about: Default::default(),
                team: false,
                pull_requests: false,
                pull_request_approval: false,
            }],
            owner: None,
            role: None,
            load: None,
            models: Default::default(),
            offers: Default::default(),
        };
        let peers = vec![peer("open"), peer("plain"), peer("wide"), peer("helper")];
        let grants = [
            Some(vec!["General", "HR"]),
            None,
            Some(vec!["General"]),
            None,
        ];
        let mut trust = TrustStore::default();
        for (p, labels) in peers.iter().zip(grants) {
            trust.allow(TrustedPeer {
                name: p.name.clone(),
                id: p.id,
                addrs: Default::default(),
                can_delegate: false,
                tasks_per_hour: 5,
                access: Access::ReadOnly,
                shares: vec![crate::shares::DATA_SHARE.into()],
                m365: labels.map(|labels| M365Grant {
                    labels: labels.iter().map(|l| (*l).to_owned()).collect(),
                    tools: false,
                }),
                github: None,
            });
        }
        let helper = peers[3].id;
        let mut recipients = Recipients::new(trust, vec![helper]);
        recipients.profile = Some("boss".into());
        let check = |text: &str, recipients: &Recipients| -> (Vec<String>, Vec<String>) {
            let problems = preflight(
                &Plan::parse(text).unwrap(),
                &peers,
                &SquadStore::default(),
                &kb,
                &skills,
                recipients,
            );
            let by = |error: bool| {
                problems
                    .iter()
                    .filter(|p| p.error == error)
                    .map(|p| p.message.clone())
                    .collect()
            };
            (by(true), by(false))
        };

        let (errors, _) = check("a @helper workiq=all find", &recipients);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].contains("proofkin auth login workiq --profile boss"),
            "{}",
            errors[0]
        );

        recipients.workiq_account = true;
        assert!(check("a @helper workiq=all find", &recipients).0.is_empty());
        assert!(check("a @wide workiq=email find", &recipients).0.is_empty());
        let (errors, _) = check("a @plain workiq=email find", &recipients);
        assert_eq!(errors.len(), 1, "{errors:?}");
        for part in [
            "step 'a'",
            "plain may not receive",
            "no Microsoft 365 grant",
            &format!("proofkin peers allow {} --name plain", peers[1].id),
        ] {
            assert!(errors[0].contains(part), "{part}: {}", errors[0]);
        }

        // A peer gets what its grant allows: a step after that goes to a peer with a
        // narrower grant is an error now.
        let (errors, _) = check("a @open workiq=all find\nb after a @wide use", &recipients);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].contains("grant doesn't include HR"),
            "{}",
            errors[0]
        );

        // From an own worker the labels are only known when it runs: a warning, and the
        // step is checked when it is sent.
        let (errors, warnings) = check(
            "a @helper workiq=all find\nb after a @wide use",
            &recipients,
        );
        assert!(errors.is_empty(), "{errors:?}");
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("checked when the step is sent")),
            "{warnings:?}"
        );
    }

    #[test]
    fn the_plan_check_stops_marked_steps_for_peers_that_may_not_have_them() {
        use crate::trust::{M365Grant, TrustedPeer};
        let dir = tempfile::tempdir().unwrap();
        let kb = KnowledgeBase::new(dir.path().join("kb"));
        let skills = Skills::new(dir.path().join("skills"));
        kb.write(
            "hr",
            &with_mark("Pay bands", &["HR".into(), "General".into()]),
            "",
        )
        .unwrap();
        let peer = |name: &str| PeerInfo {
            name: name.into(),
            id: iroh::SecretKey::generate().public(),
            capabilities: vec![Provider::Copilot],
            shares: vec![ShareInfo {
                name: crate::shares::DATA_SHARE.into(),
                access: Access::ReadOnly,
                checks: Vec::new(),
                worktrees: false,
                mcp: Vec::new(),
                mcp_about: Default::default(),
                team: false,
                pull_requests: false,
                pull_request_approval: false,
            }],
            owner: None,
            role: None,
            load: None,
            models: Default::default(),
            offers: Default::default(),
        };
        let granted = |labels: &[&str]| {
            Some(M365Grant {
                labels: labels.iter().map(|l| (*l).to_owned()).collect(),
                tools: false,
            })
        };
        let peers = vec![peer("open"), peer("plain"), peer("wide"), peer("helper")];
        let mut trust = TrustStore::default();
        let grants = [
            granted(&["general", "hr"]),
            None,
            granted(&["General"]),
            None,
        ];
        for (p, m365) in peers.iter().zip(grants) {
            trust.allow(TrustedPeer {
                name: p.name.clone(),
                id: p.id,
                addrs: Default::default(),
                can_delegate: false,
                tasks_per_hour: 5,
                access: Access::ReadOnly,
                shares: vec![crate::shares::DATA_SHARE.into()],
                m365,
                github: None,
            });
        }
        let id = |name: &str| peers.iter().find(|p| p.name == name).unwrap().id;
        let recipients = Recipients::new(trust, vec![id("helper")]);
        let mut squads = SquadStore::default();
        squads
            .set("mixed", vec!["open".into(), "wide".into()])
            .unwrap();
        squads
            .set("friends", vec!["open".into(), "helper".into()])
            .unwrap();
        let errors = |text: &str| -> Vec<String> {
            preflight(
                &Plan::parse(text).unwrap(),
                &peers,
                &squads,
                &kb,
                &skills,
                &recipients,
            )
            .into_iter()
            .filter(|p| p.error)
            .map(|p| p.message)
            .collect()
        };

        assert!(
            errors("a @open context=hr do").is_empty(),
            "labels match without case"
        );
        assert!(
            errors("a @helper context=hr do").is_empty(),
            "a digital worker is the owner's"
        );
        assert!(errors("a #friends context=hr do").is_empty());
        assert!(
            errors("a @plain do").is_empty(),
            "nothing marked, nothing to refuse"
        );

        let found = errors("a @plain context=hr do");
        assert_eq!(found.len(), 1, "{found:?}");
        for part in [
            "step 'a'".to_owned(),
            "plain may not receive".to_owned(),
            "labels: HR, General".to_owned(),
            "no Microsoft 365 grant".to_owned(),
            format!(
                "proofkin peers allow {} --name plain --m365 HR,General",
                id("plain")
            ),
        ] {
            assert!(found[0].contains(&part), "{part}: {}", found[0]);
        }

        let found = errors("a @wide context=hr do");
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(
            found[0].contains("grant doesn't include HR"),
            "{}",
            found[0]
        );
        assert!(found[0].contains("--m365 General,HR"), "{}", found[0]);

        let found = errors("a @helper context=hr do\nb after a @plain do");
        assert_eq!(found.len(), 1, "after a marked step: {found:?}");
        assert!(
            found[0].contains("step 'b'") && found[0].contains("plain"),
            "{found:?}"
        );
        let found = errors("a @helper context=hr do\nb after a approve @plain Approve it");
        assert_eq!(found.len(), 1, "a person's approval sees it too: {found:?}");

        let found = errors("a #mixed context=hr do");
        assert_eq!(found.len(), 1, "any member the squad may pick: {found:?}");
        assert!(found[0].contains("wide may not receive"), "{}", found[0]);
    }
}
