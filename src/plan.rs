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

use anyhow::{Context, Result, anyhow, bail};
use iroh::EndpointId;
use serde::{Deserialize, Serialize};
use tokio::task::JoinSet;

use crate::agent::{Agent, Delivered, PeerInfo};
use crate::evidence::Evidence;
use crate::knowledge::{KnowledgeBase, check_topics};
use crate::llm::{LlmTask, Provider};
use crate::profile::validate_name;
use crate::protocol::{Origin, Phase, Progress};
use crate::skills::Skills;

/// Longest prerequisite result passed on to a dependent step.
const MAX_DEP_CHARS: usize = 8_000;

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
        validate_name(name).context("invalid squad name")?;
        if members.is_empty() {
            bail!("a squad needs at least one member");
        }
        for member in &members {
            validate_name(member).with_context(|| format!("invalid member name {member:?}"))?;
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
}

impl std::fmt::Display for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Target::Peer(name) => write!(f, "@{name}"),
            Target::Squad(name) => write!(f, "#{name}"),
        }
    }
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
        for (n, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with("//") {
                continue;
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
                parse_step(line).map(|s| steps.push(s))
            };
            if let Err(e) = parsed {
                errors.push(format!("line {}: {e}", n + 1));
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
        Ok(())
    }
}

impl Plan {
    /// The plan as it is sent: each step's `skill=` instructions and `context=`
    /// knowledge put in front of its prompt. Fails if a skill or topic doesn't exist.
    pub fn prepare(&self, kb: &KnowledgeBase, skills: &Skills) -> Result<Plan> {
        let mut plan = self.with_knowledge(kb)?;
        for step in &mut plan.steps {
            let agent = match &step.agent {
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
        let mut plan = self.clone();
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
}

/// Saves the result of every finished step with `save=` into `kb`, if its evidence
/// was verified (or it ran locally); returns the topics written.
pub fn save_knowledge(
    kb: &KnowledgeBase,
    plan: &Plan,
    states: &BTreeMap<String, StepState>,
    run_id: &str,
) -> Vec<String> {
    let mut saved = Vec::new();
    for step in &plan.steps {
        let (
            Some(topic),
            Some(StepState::Done {
                peer,
                output,
                verified: true,
                ..
            }),
        ) = (&step.save, states.get(&step.id))
        else {
            continue;
        };
        let source = format!(
            "run {run_id}, step {} by {peer} (signed, verified)",
            step.id
        );
        if kb.write(topic, output, &source).is_ok() {
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
    "agent", "levels", "timeout", "issue",
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

fn parse_step(line: &str) -> Result<Step> {
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
    let target = if target.starts_with('@') {
        Target::Peer(name.to_owned())
    } else {
        Target::Squad(name.to_owned())
    };
    let mut provider = Provider::Copilot;
    let (mut checks, mut attempts, mut claims, mut apply) = (Vec::new(), None, Vec::new(), None);
    let (mut context, mut save, mut skills, mut mcp) = (Vec::new(), None, Vec::new(), Vec::new());
    let mut agent = None;
    let (mut levels, mut timeout, mut issue) = (None, None, None);
    let mut once = std::collections::HashSet::new();
    loop {
        match words.peek().copied() {
            Some("using") => {
                words.next();
                let name = words.next().context("'using' needs a provider")?;
                provider = clap::ValueEnum::from_str(name, true)
                    .map_err(|_| anyhow!("unknown provider {name:?}"))?;
            }
            Some(word) if word.contains('=') => {
                let (key, value) = word.split_once('=').expect("contains '='");
                if matches!(
                    key,
                    "attempts" | "apply" | "save" | "agent" | "levels" | "timeout" | "issue"
                ) && !once.insert(key)
                {
                    bail!("{key}= is given twice; a step has one");
                }
                match key {
                    "verify" => add(&mut checks, value),
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
                    _ => break,
                }
                words.next();
            }
            _ => break,
        }
    }
    let prompt = words.collect::<Vec<_>>().join(" ");
    if prompt.is_empty() {
        bail!("missing the task prompt: what to do comes last, after who runs it and the options");
    }
    if levels.is_some() && !team {
        bail!("levels= is for team steps (name team @lead ...)");
    }
    Ok(Step {
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
    })
}

/// `first`, then those of `then` that aren't in it.
fn union(first: &[String], then: &[String]) -> Vec<String> {
    let mut all = first.to_vec();
    all.extend(then.iter().filter(|x| !first.contains(x)).cloned());
    all
}

/// The step as it runs as its agent: the agent's skills and MCP servers come first.
fn with_agent(step: &Step, skills: &Skills) -> Result<Step> {
    let mut step = step.clone();
    if let Some(name) = &step.agent {
        let agent = skills.agent(name)?;
        step.skills = union(&agent.skills, &step.skills);
        step.mcp = union(&agent.mcp, &step.mcp);
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
    },
    Failed {
        peer: Option<String>,
        error: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        evidence: Option<Evidence>,
        #[serde(default)]
        verified: bool,
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

/// Why `peer` can't run `step`, if it can't: provider, folder, checks, access.
fn unable(step: &Step, peer: &PeerInfo) -> Option<String> {
    use crate::llm::Access;
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

/// Checks `plan` against what is known now (connected peers, their folders, checks
/// and access, squads, knowledge and skills) so problems show before anything runs.
pub fn preflight(
    plan: &Plan,
    peers: &[PeerInfo],
    squads: &SquadStore,
    kb: &KnowledgeBase,
    skills: &Skills,
) -> Vec<Problem> {
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
    for own in &plan.steps {
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
        let candidates: Vec<&PeerInfo> = match &step.target {
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
            online
                .iter()
                .copied()
                .filter(capable)
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

/// The prompt a step runs with: the results of its prerequisites, then its task.
/// Those results (and any knowledge) are external content: other peers or models
/// wrote them, so they are wrapped and the prompt starts with the rule for them.
pub fn step_prompt(step: &Step, deps: &[(&str, &str, &str)]) -> String {
    if deps.is_empty() {
        return crate::boundary::guard(&step.prompt);
    }
    let mut prompt = String::from("Results of the earlier steps this task builds on:\n");
    for (id, peer, output) in deps {
        let mut text: String = output.chars().take(MAX_DEP_CHARS).collect();
        if output.chars().count() > MAX_DEP_CHARS {
            text += "\n[... truncated]";
        }
        prompt += &format!(
            "\n### Step {id} (done by {peer})\n{}\n",
            crate::boundary::wrap(&format!("step:{id} peer:{peer}"), &text)
        );
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

/// A plan checked and ready to run as `profile`'s agent (see [`start`]).
pub struct ReadyRun {
    text: String,
    plan: Plan,
    ready: Plan,
    squads: SquadStore,
    kb: KnowledgeBase,
    profile: crate::profile::Profile,
}

/// Checks plan `text` as `/plan` and the web UI do (preflight against connected
/// peers, folders, checks, MCP servers, squads, knowledge and skills) and prepares
/// it; fails with every problem if it can't run now.
pub fn start(agent: &Agent, profile: &crate::profile::Profile, text: &str) -> Result<ReadyRun> {
    let plan = Plan::parse(text)?;
    let kb = profile.knowledge();
    let skills = profile.skills();
    let squads = SquadStore::load(&profile.squads_path()).unwrap_or_default();
    let errors: Vec<String> = preflight(&plan, &agent.peers(), &squads, &kb, &skills)
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
    Ok(ReadyRun {
        text: text.to_owned(),
        plan,
        ready,
        squads,
        kb,
        profile: profile.clone(),
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
        save_knowledge(&self.kb, &self.plan, &states, &run.id);
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
fn movable(result: &Result<Delivered>) -> Option<String> {
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
    let mut states: BTreeMap<String, StepState> = plan
        .steps
        .iter()
        .map(|s| (s.id.clone(), StepState::Waiting))
        .collect();
    for step in &plan.steps {
        on_update(&step.id, &StepState::Waiting);
    }
    let mut running: JoinSet<(String, String, Result<Delivered>)> = JoinSet::new();
    let (progress_tx, mut progress_rx) =
        tokio::sync::mpsc::unbounded_channel::<(String, Progress)>();
    // Squad members a step moved away from, and why it last moved.
    let mut tried: HashMap<String, HashSet<String>> = HashMap::new();
    let mut moved: HashMap<String, String> = HashMap::new();
    let mut chain: Vec<String> = ctx.chain.iter().map(ToString::to_string).collect();
    chain.push(agent.id().to_string());
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
                let deps: Vec<(&str, &str, &str)> = step
                    .after
                    .iter()
                    .filter_map(|d| match &states[d] {
                        StepState::Done { peer, output, .. } => {
                            Some((d.as_str(), peer.as_str(), output.as_str()))
                        }
                        _ => None,
                    })
                    .collect();
                let prompt = step_prompt(step, &deps);
                let fail = |error: String| StepState::failed(None, error);
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
                            let (agent, title) = (agent.clone(), format!("Plan step {}", step.id));
                            // A person reads it: the rule for models is left out.
                            let text = approval_text(&prompt);
                            (
                                name,
                                Box::pin(async move {
                                    agent
                                        .request_approval_with(approver, &title, &text, preview)
                                        .await
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
                    let mut task = LlmTask::new(step.provider, prompt.clone())
                        .with_checks(step.checks.clone(), step.attempts);
                    task.mcp = step.mcp.clone();
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
                    if let Some(levels) = step.team {
                        task.team = Some(crate::team::TeamRequest {
                            levels,
                            chain: chain.clone(),
                            brief: step.own_text().to_owned(),
                        });
                    }
                    let origin = Origin {
                        run: ctx.run_id.clone(),
                        step: Some(step.id.clone()),
                        task: None,
                    };
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
                        .map(|why| Progress::new(Phase::Running, format!("moved here: {why}"))),
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
/// The person who must approve `step`: a connected peer by name, or `@me` (None).
fn choose_approver(step: &Step, peers: &[PeerInfo]) -> Result<(Option<iroh::EndpointId>, String)> {
    match &step.target {
        Target::Peer(name) if name == "me" => Ok((None, "you".to_owned())),
        Target::Peer(name) => peers
            .iter()
            .find(|p| &p.name == name)
            .map(|p| (Some(p.id), p.name.clone()))
            .ok_or_else(|| anyhow!("{name} is not connected")),
        Target::Squad(_) => bail!("approvals need a person: @name or @me"),
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
        let prompt = step_prompt(&step, &[("find", "worker", "the facts\nsecond line")]);
        assert!(prompt.contains("<external_content source="), "{prompt}");
        let text = approval_text(&prompt);
        assert!(!text.contains("external_content"), "{text}");
        assert!(!text.contains("Rule for external content"), "{text}");
        assert!(
            text.contains("### Step find (done by worker)\nthe facts\nsecond line\n"),
            "{text}"
        );
        assert!(text.ends_with("Your task:\nApprove the facts"), "{text}");
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
    fn squads_persist_and_validate() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("squads.json");
        let mut store = SquadStore::default();
        store
            .set("qa", vec!["bob".into(), "alice".into(), "bob".into()])
            .unwrap();
        assert_eq!(store.get("qa").unwrap().members, ["alice", "bob"]);
        assert!(store.set("qa", vec![]).is_err());
        assert!(store.set("../x", vec!["a".into()]).is_err());
        assert!(store.set("dev", vec!["bad name".into()]).is_err());
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
            preflight(&plan, &[], &squads, &kb, &skills)
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
        };
        let peer = |name: &str, shares: Vec<ShareInfo>| PeerInfo {
            name: name.into(),
            id: iroh::SecretKey::generate().public(),
            capabilities: vec![Provider::Copilot, Provider::Codex],
            shares,
            owner: None,
            role: None,
            load: None,
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
        let check = |text: &str| {
            preflight(&Plan::parse(text).unwrap(), &peers, &squads, &kb, &skills)
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
            preflight(&Plan::parse(text).unwrap(), &peers, &squads, &kb, &skills)
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
            preflight(&Plan::parse(text).unwrap(), &peers, &squads, &kb, &skills)
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
        };
        let dir = tempfile::tempdir().unwrap();
        let kb = KnowledgeBase::new(dir.path().join("kb"));
        let skills = Skills::new(dir.path().join("skills"));
        let problems = |peer: PeerInfo| -> Vec<String> {
            preflight(&plan, &[peer], &SquadStore::default(), &kb, &skills)
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
    }
}
