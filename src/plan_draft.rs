//! Plans a model drafts (README, "Drafting a plan with a model"): from a goal in the
//! owner's words, an issue of the tracker's repository, or a plan and a note that says what
//! should change, the profile's own model writes a plan, knowing what this agent may use as
//! the plan check sees it ([`Available`]) and how a plan is written ([`crate::plan::FORMAT`]).
//! The plan is checked with the check Run plan makes ([`crate::plan::preflight`]); while it
//! has errors the model gets them and writes it again, at most [`TRIES`] tries in all, in
//! the factory's drafting loop ([`crate::factory::draft`]): read-only, in an empty folder,
//! with no tools. It sees no file, knowledge text, token, Microsoft 365 content or past
//! result; secrets are redacted from the goal, and an issue's text comes as external
//! content. A draft is data: drafting runs, grants, sends and changes nothing.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::agent::{Network, PeerInfo};
use crate::audit::{AuditEntry, AuditLog, Outcome};
use crate::boundary::{guard, wrap};
use crate::factory::draft::{Reading, ask_until};
use crate::knowledge::KnowledgeBase;
use crate::llm::{Access, Executor, ExecutorConfig, Provider, SdkExecutor};
use crate::plan::{FORMAT, Plan, Problem, Recipients, SquadStore, preflight};
use crate::profile::Profile;
use crate::skills::Skills;
use crate::team::{
    again_prompt, changes_prompt, clip, one_line, peers_text, skills_text, squads_text,
    written_plan,
};

pub use crate::factory::draft::TRIES;

/// The longest goal (and note), in characters.
pub const MAX_GOAL: usize = crate::factory::draft::MAX_DESCRIPTION;

/// The first line of a plan a model drafted starts with this.
const HEADER: &str = "// Drafted by ";

/// What the owner asks for, as the web UI, MCP and the CLI take it.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Ask {
    /// What the plan should do, in the owner's words.
    #[serde(default)]
    pub goal: Option<String>,
    /// The issue of the tracker's repository it is for: a number, or `#N`.
    #[serde(default, deserialize_with = "issue_of")]
    pub issue: Option<u64>,
    /// The provider whose model writes it (default: the first the profile is signed in to).
    #[serde(default)]
    pub provider: Option<Provider>,
    /// A plan to change, with `note`.
    #[serde(default)]
    pub plan: Option<String>,
    /// What should change in `plan`.
    #[serde(default)]
    pub note: Option<String>,
}

fn issue_of<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Option<u64>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Issue {
        Number(u64),
        Text(String),
    }
    match Option::<Issue>::deserialize(d)? {
        None => Ok(None),
        Some(Issue::Number(n)) => Ok(Some(n)),
        Some(Issue::Text(text)) => {
            let n = text.trim().trim_start_matches('#').trim();
            if n.is_empty() {
                return Ok(None);
            }
            n.parse()
                .map(Some)
                .map_err(|_| serde::de::Error::custom(format!("{text:?} isn't an issue: write #N")))
        }
    }
}

fn text_of(value: &Option<String>) -> &str {
    value.as_deref().map(str::trim).unwrap_or_default()
}

impl Ask {
    pub fn goal(&self) -> &str {
        text_of(&self.goal)
    }

    pub fn note(&self) -> &str {
        text_of(&self.note)
    }

    pub fn plan(&self) -> &str {
        text_of(&self.plan)
    }

    /// Why it can't be asked, before anything is read or a model is asked: it needs a
    /// goal, an issue, or a plan and a note that says what should change, and the goal and
    /// the note fit in [`MAX_GOAL`] characters.
    pub fn validate(&self) -> Result<()> {
        if self.goal().chars().count() > MAX_GOAL {
            bail!("the goal is over {MAX_GOAL} characters: say it in fewer words");
        }
        if self.note().chars().count() > MAX_GOAL {
            bail!("the note is over {MAX_GOAL} characters: say it in fewer words");
        }
        match (self.plan().is_empty(), self.note().is_empty()) {
            (true, false) => {
                bail!("a note says what should change in a plan: give the plan to change with it")
            }
            (false, true) => bail!("say what should change in the plan: write a note"),
            _ => {}
        }
        if self.goal().is_empty() && self.issue.is_none() && self.plan().is_empty() {
            bail!(
                "say what the plan should do (the goal), name an issue (#N), or give a plan \
                 and what should change in it"
            );
        }
        Ok(())
    }

    /// The request this asks for, with issue text `issue` (see [`issue_text`]).
    pub fn request(&self, issue: Option<String>) -> Request {
        Request {
            goal: self.goal().to_owned(),
            issue: self.issue.zip(issue),
            change: (!self.plan().is_empty()).then(|| Change {
                plan: self.plan().to_owned(),
                note: self.note().to_owned(),
            }),
        }
    }
}

/// What the model is asked for.
#[derive(Debug, Clone, Default)]
pub struct Request {
    /// What the plan should do, in the owner's words.
    pub goal: String,
    /// The issue it is for: its number and its text, as the tracker gives a step's `issue=`.
    pub issue: Option<(u64, String)>,
    /// A plan to write again, and what should change in it.
    pub change: Option<Change>,
}

#[derive(Debug, Clone)]
pub struct Change {
    pub plan: String,
    pub note: String,
}

/// What this agent may use, as the plan check sees it: the model is told it, and its plans
/// are checked against it.
pub struct Available {
    pub peers: Vec<PeerInfo>,
    pub squads: SquadStore,
    pub knowledge: KnowledgeBase,
    pub skills: Skills,
    pub recipients: Recipients,
    /// The tracker's repository (`owner/repo`), if the profile has one.
    pub tracker: Option<String>,
}

impl Available {
    /// What `profile` may use with `peers` connected, its `squads`, and `recipients` (see
    /// [`Recipients::of`]).
    pub fn of(
        profile: &Profile,
        peers: Vec<PeerInfo>,
        squads: SquadStore,
        recipients: Recipients,
    ) -> Self {
        Self {
            peers,
            squads,
            knowledge: profile.knowledge(),
            skills: profile.skills(),
            recipients,
            tracker: crate::tracker::TrackerConfig::load(&profile.tracker_path())
                .ok()
                .flatten()
                .map(|c| c.repo),
        }
    }

    /// The problems of plan `text`, as Run plan's check finds them.
    pub fn check(&self, text: &str) -> Vec<Problem> {
        match Plan::parse(text) {
            Ok(plan) => preflight(
                &plan,
                &self.peers,
                &self.squads,
                &self.knowledge,
                &self.skills,
                &self.recipients,
            ),
            Err(e) => vec![Problem {
                step: None,
                claim: None,
                code: None,
                error: true,
                message: format!("{e:#}"),
                fixes: Vec::new(),
            }],
        }
    }

    /// What it holds, for the model: names, never contents.
    fn text(&self) -> String {
        let mut text = String::from(
            "Peers and digital workers connected now (@peer), with their role and the \
             providers they run, and each folder they share with you (@peer:folder): the \
             access tasks get there, its checks (verify=), its MCP servers (mcp=) and the \
             steps it takes:\n",
        );
        let peers = peers_text(&self.peers, true);
        text += if peers.is_empty() {
            "- none: no peer is connected\n"
        } else {
            &peers
        };
        text += "\nSquads (#squad: a step runs on its least busy member):\n";
        text += &squads_text(&self.squads);
        text += "\nSkills (skill=name):\n";
        text += &skills_text(&self.skills.list());
        text += "\nAgents steps may act as (agent=name):\n";
        let agents = self.skills.agents();
        for agent in &agents {
            let about = clip(&one_line(&agent.description), 200);
            let version = agent
                .version
                .as_deref()
                .map(|v| format!(" (release {v})"))
                .unwrap_or_default();
            let instead = agent
                .deprecated
                .as_deref()
                .map(|r| format!("; deprecated: use {r}"))
                .unwrap_or_default();
            text += &format!("- {}{version}: {about}{instead}\n", agent.name);
        }
        if agents.is_empty() {
            text += "- none\n";
        }
        text += "\nKnowledge topics (context=topic, save=topic):\n";
        let topics: Vec<String> = self.knowledge.list().into_iter().map(|t| t.name).collect();
        if topics.is_empty() {
            text += "- none\n";
        } else {
            text += &format!("- {}\n", topics.join(", "));
        }
        text += &format!(
            "\nWork IQ (@workiq steps, workiq=): {}. Microsoft 365 changes (@workiq apply=): {}.\n",
            if self.recipients.workiq_account {
                "signed in"
            } else {
                "not signed in, so no step may use it"
            },
            if self.recipients.workiq_changes {
                "on"
            } else {
                "off, so no step may make them"
            }
        );
        text += &match &self.tracker {
            Some(repo) => format!("\nThe tracker's repository (track:, issue=#N): {repo}\n"),
            None => "\nThe tracker's repository (track:, issue=#N): none is linked, so write \
                     neither\n"
                .to_owned(),
        };
        text
    }
}

/// A plan the model drafted, with its problems as Run plan's check finds them.
#[derive(Debug, Clone, Serialize)]
pub struct Drafted {
    pub plan: String,
    pub problems: Vec<Problem>,
    pub provider: Provider,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub tries: u32,
    /// The SHA-256 of the goal, for the audit.
    #[serde(skip)]
    pub goal_sha256: String,
    #[serde(skip)]
    pub goal_chars: usize,
}

impl Drafted {
    pub fn errors(&self) -> usize {
        self.problems.iter().filter(|p| p.error).count()
    }
}

fn redacted(text: &str) -> String {
    crate::redact::redact(text.trim()).0
}

/// The prompt that asks the model for the plan `request` asks for, knowing what
/// `available` holds.
pub fn prompt(request: &Request, available: &Available) -> String {
    let mut text = String::from(
        "You write plans for Proofkin, which runs them: each step is a task for an agent's \
         model, a sign-off, a team step or a pull request, on the peers and folders listed \
         below, and checks prove the work. Write the plan the owner asks for. Don't do the \
         work yourself: this folder is empty, and you need nothing from it.\n\n",
    );
    let goal = redacted(&request.goal);
    if !goal.is_empty() {
        text += &format!("## What the plan should do\n\n{goal}\n\n");
    }
    if let Some((n, issue)) = &request.issue {
        text += &format!(
            "## The issue it is for\n\nThe plan works on issue #{n} of the tracker's \
             repository: its steps that work on it say `issue=#{n}`, and `track: #{n}` makes \
             the run work on it. Its text, as GitHub has it:\n\n{}\n\n",
            wrap(&format!("issue:{n}"), issue)
        );
    }
    text += "## How a plan is written\n\n";
    text += FORMAT;
    text += "\n\n## What you may use\n\n";
    text += &wrap("what this agent may use", &available.text());
    text += "\n\n## Examples\n\nThe playbooks: ready-made plans, each with what it is for. In \
             them, {{name}} stands for a value you write from what you may use; the first \
             lines say which ones.\n";
    for (name, about, plan) in crate::playbooks::templates() {
        text += &format!("\n### {name}: {about}\n\n```plan\n{}\n```\n", plan.trim());
    }
    text += "\nAnswer with the plan alone, in one fenced block:\n\n\
             ```plan\none claim, step or comment per line\n```\n";
    guard(&text)
}

/// `text` without the lines a draft starts with (see [`HEADER`]).
fn without_header(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let skip = lines
        .iter()
        .take_while(|l| l.trim().starts_with(HEADER.trim_end()))
        .count();
    lines[skip..].join("\n").trim().to_owned()
}

/// What an earlier draft's first line says it was for.
fn subject_of(text: &str) -> Option<String> {
    let first = text.lines().next()?.trim();
    let rest = first.strip_prefix(HEADER)?;
    let (_, subject) = rest.split_once(": ")?;
    Some(subject.trim().to_owned()).filter(|s| !s.is_empty())
}

/// The first line of the plan `request` asked for, which `provider`'s `model` wrote:
/// `// Drafted by copilot (claude-opus-5.5): <the goal's first line>`.
fn header(provider: Provider, model: Option<&str>, request: &Request) -> String {
    let line = |text: &str| {
        let first = text.lines().map(str::trim).find(|l| !l.is_empty());
        first.map(|l| clip(&one_line(&redacted(l)), 120))
    };
    let subject = line(&request.goal)
        .or_else(|| request.issue.as_ref().map(|(n, _)| format!("issue #{n}")))
        .or_else(|| request.change.as_ref().and_then(|c| subject_of(&c.plan)))
        .or_else(|| request.change.as_ref().and_then(|c| line(&c.note)))
        .unwrap_or_default();
    let model = model.map(|m| format!(" ({m})")).unwrap_or_default();
    format!("{HEADER}{provider}{model}: {subject}")
}

/// The errors of `problems`, as the model is told them.
fn errors_text(problems: &[Problem]) -> String {
    let errors: Vec<String> = problems
        .iter()
        .filter(|p| p.error)
        .map(|p| match &p.step {
            Some(step) => format!("step {step}: {}", p.message),
            None => p.message.clone(),
        })
        .collect();
    errors.join("; ")
}

/// Has `executor`'s model of `provider` (at the profile's default model and effort, as
/// `models` allows them) write the plan `request` asks for, for what `available` holds.
/// `progress` hears each try.
pub async fn write(
    executor: &dyn Executor,
    provider: Provider,
    models: &crate::models::Models,
    request: &Request,
    available: &Available,
    progress: &(dyn Fn(String) + Send + Sync),
) -> Result<Drafted> {
    let mut base = prompt(request, available);
    if let Some(change) = &request.change {
        base = changes_prompt(
            "plan",
            &base,
            &redacted(&without_header(&change.plan)),
            "The owner",
            &redacted(&change.note),
        );
    }
    let read = |output: &str| {
        let plan = without_header(&written_plan(output));
        if plan.is_empty() {
            return Reading {
                written: None,
                errors: 1,
                again: again_prompt(
                    "plan",
                    &base,
                    output.trim(),
                    "the answer has no plan: write it in one fenced block (```plan)",
                ),
            };
        }
        let problems = available.check(&plan);
        Reading {
            errors: problems.iter().filter(|p| p.error).count(),
            again: again_prompt("plan", &base, &plan, &errors_text(&problems)),
            written: Some(plan),
        }
    };
    let tried = ask_until(executor, provider, models, &base, "plan", &read, progress).await?;
    let Some(plan) = tried.written else {
        bail!("the model didn't write a plan in {TRIES} tries: its answers held none");
    };
    let plan = format!(
        "{}\n{plan}\n",
        header(provider, tried.model.as_deref(), request)
    );
    Ok(Drafted {
        problems: available.check(&plan),
        plan,
        provider,
        model: tried.model,
        effort: tried.effort,
        tries: tried.tries,
        goal_sha256: crate::evidence::sha256_hex(request.goal.trim().as_bytes()),
        goal_chars: request.goal.trim().chars().count(),
    })
}

/// The providers the profile is signed in to, whose models could write a plan.
pub fn signed_in(profile: &Profile) -> Vec<Provider> {
    let Ok(accounts) = profile.account_profile() else {
        return Vec::new();
    };
    [Provider::Copilot, Provider::Claude, Provider::Codex]
        .into_iter()
        .filter(|p| accounts.is_configured(*p))
        .collect()
}

/// The provider whose model writes the plan: `asked`, if the profile is signed in to it,
/// else the first it is signed in to.
pub fn provider_for(profile: &Profile, asked: Option<Provider>) -> Result<Provider> {
    let providers = signed_in(profile);
    match asked {
        Some(p) if providers.contains(&p) => Ok(p),
        Some(p) => bail!(
            "profile '{}' isn't signed in to {p}: {}",
            profile.name,
            profile.login_hint(p)
        ),
        None => providers.first().copied().ok_or_else(|| {
            anyhow!(
                "profile '{}' isn't signed in to a provider whose model could write the plan: proofkin auth login copilot --profile {0}",
                profile.name
            )
        }),
    }
}

/// The text of issue `n` of the profile's tracker repository, as a step with `issue=#n`
/// gets it.
pub async fn issue_text(profile: &Profile, n: u64) -> Result<String> {
    let Some(tracker) = crate::tracker::open(profile).await? else {
        bail!(
            "profile '{}' has no tracker, so it has no issue #{n}; link one with: proofkin tracker link <owner/repo> --profile {0}",
            profile.name
        );
    };
    tracker.issue_text(n).await
}

/// Records `drafted`, which `by` asked for at `started`, in the profile's audit (whose own
/// id is `own_id`): who asked, the provider, model and tries, and the goal's SHA-256, not
/// its text.
pub fn record(profile: &Profile, drafted: &Drafted, by: &str, own_id: &str, started: SystemTime) {
    AuditLog::new(profile.audit_path()).record(&AuditEntry {
        ts_ms: started
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64),
        peer: by.to_owned(),
        peer_id: own_id.to_owned(),
        task_id: 0,
        provider: drafted.provider,
        share: String::new(),
        access: Some(Access::ReadOnly),
        outcome: Outcome::Done,
        duration_ms: started.elapsed().unwrap_or_default().as_millis() as u64,
        prompt_sha256: drafted.goal_sha256.clone(),
        prompt_chars: drafted.goal_chars,
        error: None,
        origin: Some("a plan draft".into()),
        team: false,
        model: drafted.model.clone(),
        usage: None,
        agent: None,
        attempts: Some(drafted.tries),
        checks: None,
        refused_by: None,
    });
}

/// What the activity says of `drafted`, which `by` asked for.
pub fn activity(drafted: &Drafted, by: &str) -> String {
    let model = drafted
        .model
        .as_deref()
        .map(|m| format!(" ({m})"))
        .unwrap_or_default();
    let errors = drafted.errors();
    let warnings = drafted.problems.len() - errors;
    let tries = if drafted.tries == 1 { "try" } else { "tries" };
    format!(
        "Drafted a plan with {}'s model{model} in {} {tries} for {by}: {errors} error{}, \
         {warnings} warning{}",
        drafted.provider,
        drafted.tries,
        if errors == 1 { "" } else { "s" },
        if warnings == 1 { "" } else { "s" }
    )
}

/// The peers of `profile` connected within `wait` over `network`, and who may receive what,
/// for when its agent doesn't run (`proofkin plan draft`).
pub async fn connected(
    profile: &Profile,
    wait: Duration,
    network: Network,
) -> Result<(Vec<PeerInfo>, Recipients)> {
    let trust = profile.trust()?;
    if trust.peers.is_empty() {
        return Ok((Vec::new(), Recipients::with_trust(trust, profile)));
    }
    let (agent, _instance) = crate::factory::manage::peer_agent(
        profile,
        network,
        "draft the plan",
        "Plan › Draft with a model…",
    )
    .await?;
    for peer in &trust.peers {
        let (agent, addr) = (agent.clone(), peer.addr());
        tokio::spawn(async move {
            let _ = agent.connect(addr).await;
        });
    }
    let deadline = std::time::Instant::now() + wait;
    while std::time::Instant::now() < deadline && agent.peers().len() < trust.peers.len() {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    // The folders each shares come with its hello: a moment for the last ones.
    tokio::time::sleep(Duration::from_millis(500)).await;
    agent.settle_peers().await;
    let peers = agent.peers();
    let recipients = Recipients::of(&agent, profile);
    agent.shutdown().await;
    Ok((peers, recipients))
}

/// `proofkin plan draft`: the plan `ask` asks for, which the model of `ask.provider` writes
/// for `profile`, with the peers connected within `wait` over `network`; recorded in the
/// profile's audit. `progress` hears each try.
pub async fn draft_offline(
    profile: &Profile,
    ask: &Ask,
    wait: Duration,
    network: Network,
    bridge: PathBuf,
    progress: &(dyn Fn(String) + Send + Sync),
) -> Result<Drafted> {
    let started = SystemTime::now();
    ask.validate()?;
    let provider = provider_for(profile, ask.provider)?;
    let issue = match ask.issue {
        Some(n) => Some(issue_text(profile, n).await?),
        None => None,
    };
    let (peers, recipients) = connected(profile, wait, network).await?;
    let squads = SquadStore::load(&profile.squads_path()).unwrap_or_default();
    let available = Available::of(profile, peers, squads, recipients);
    let executor = SdkExecutor::start(ExecutorConfig {
        providers: vec![provider],
        bridge_script: bridge,
        profile: profile.clone(),
    })
    .await?;
    let request = ask.request(issue);
    let drafted = write(
        executor.as_ref(),
        provider,
        &profile.models(),
        &request,
        &available,
        progress,
    )
    .await;
    executor.stop().await;
    let drafted = drafted?;
    let own_id = profile
        .identity()
        .map(|key| key.public().to_string())
        .unwrap_or_default();
    record(profile, &drafted, "the owner (CLI)", &own_id, started);
    Ok(drafted)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::factory::draft::tests::Scripted;
    use crate::plan::Squad;
    use crate::shares::ShareInfo;

    const SECRET: &str = "ghp_a1B2a1B2a1B2a1B2a1B2a1B2a1B2a1B2a1B2";

    struct Fixture {
        _dir: tempfile::TempDir,
        available: Available,
    }

    /// Alice shares folder app (checks test and lint, MCP server github, team steps); the
    /// profile has squad devs, an agent of its own and knowledge topic payments.
    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let knowledge = KnowledgeBase::new(dir.path().join("knowledge"));
        knowledge
            .write("payments", "KNOWLEDGE-TEXT-NEVER-SENT", "test")
            .unwrap();
        std::fs::create_dir_all(dir.path().join("agents")).unwrap();
        std::fs::write(
            dir.path().join("agents").join("reviewer.md"),
            "---\nname: reviewer\ndescription: Reviews a change for risks\n---\nReview it.\n",
        )
        .unwrap();
        let alice = PeerInfo {
            name: "alice".into(),
            id: iroh::SecretKey::generate().public(),
            capabilities: vec![Provider::Copilot],
            shares: vec![ShareInfo {
                name: "app".into(),
                access: Access::WorkspaceWrite,
                checks: vec!["test".into(), "lint".into()],
                mcp: vec!["github".into()],
                team: true,
                ..Default::default()
            }],
            owner: Some("boss".into()),
            role: Some("backend engineer".into()),
            agent: None,
            load: None,
            models: Default::default(),
            offers: Default::default(),
        };
        let available = Available {
            peers: vec![alice],
            squads: SquadStore {
                squads: vec![Squad {
                    name: "devs".into(),
                    members: vec!["alice".into()],
                }],
            },
            knowledge,
            skills: Skills::new(dir.path().join("skills")),
            recipients: Recipients::default(),
            tracker: Some("acme/app".into()),
        };
        Fixture {
            _dir: dir,
            available,
        }
    }

    fn fenced(plan: &str) -> String {
        format!("Here is the plan you asked for.\n\n```plan\n{plan}\n```\n\nI hope it helps.\n")
    }

    fn goal(text: &str) -> Request {
        Request {
            goal: text.into(),
            ..Request::default()
        }
    }

    const GOOD: &str = "fix @alice:app Fix the parser";
    const BAD: &str = "fix @ghost:app Fix the parser";

    #[test]
    fn the_prompt_has_the_format_and_what_the_agent_may_use_but_no_secret_or_knowledge_text() {
        let f = fixture();
        let prompt = prompt(
            &goal(&format!("Fix the parser with token {SECRET}")),
            &f.available,
        );
        assert!(prompt.contains(FORMAT), "the format reference");
        for used in [
            "- @alice (backend engineer), a digital worker of boss: runs copilot",
            "  - folder app: workspace-write access; checks test, lint; MCP servers github; \
             takes team steps",
            "- #devs: alice",
            "- bootstrap-project: ",
            "- reviewer: Reviews a change for risks",
            "Knowledge topics (context=topic, save=topic):\n- payments\n",
            "Work IQ (@workiq steps, workiq=): not signed in",
            "(track:, issue=#N): acme/app",
            "### bootstrap: ",
        ] {
            assert!(prompt.contains(used), "{used:?} in {prompt}");
        }
        assert!(prompt.contains("<external_content source=\"what this agent may use\">"));
        assert!(prompt.contains("Fix the parser with token [REDACTED:"));
        assert!(!prompt.contains(SECRET), "the goal's secret is redacted");
        assert!(
            !prompt.contains("KNOWLEDGE-TEXT-NEVER-SENT"),
            "topic names only"
        );
    }

    #[tokio::test]
    async fn a_plan_with_errors_goes_back_with_them_until_one_passes() {
        let f = fixture();
        let model = Scripted::new(&[fenced(BAD), fenced(GOOD)]);
        let lines = Mutex::new(Vec::new());
        let drafted = write(
            &model,
            Provider::Copilot,
            &Default::default(),
            &goal("Fix the parser"),
            &f.available,
            &|line| lines.lock().unwrap().push(line),
        )
        .await
        .unwrap();
        assert_eq!(drafted.tries, 2);
        assert_eq!(drafted.errors(), 0, "{:?}", drafted.problems);
        assert_eq!(
            drafted.plan,
            format!("// Drafted by copilot (gpt-6-sol): Fix the parser\n{GOOD}\n")
        );
        assert_eq!(
            (drafted.model.as_deref(), drafted.effort.as_deref()),
            (Some("gpt-6-sol"), Some("medium"))
        );
        let asked = model.asked();
        let again = &asked[1].0.prompt;
        assert!(
            again.contains("## Your last plan doesn't check out"),
            "{again}"
        );
        assert!(again.contains("step fix: "), "{again}");
        assert!(again.contains("ghost"), "{again}");
        assert_eq!(asked[1].1.access, Access::ReadOnly);
        assert_eq!(
            lines.into_inner().unwrap(),
            [
                "copilot's model is writing the plan".to_owned(),
                "try 2 of 3: fixing 1 problem".to_owned(),
            ]
        );
    }

    #[tokio::test]
    async fn after_three_failing_tries_the_last_plan_comes_back_with_its_problems() {
        let f = fixture();
        let last = "fix @nobody:app Fix the parser";
        let model = Scripted::new(&[
            fenced(BAD),
            "I can't.\n\n```plan\n```\n".into(),
            fenced(last),
        ]);
        let drafted = write(
            &model,
            Provider::Copilot,
            &Default::default(),
            &goal("Fix the parser"),
            &f.available,
            &|_| {},
        )
        .await
        .unwrap();
        assert_eq!(drafted.tries, 3);
        assert!(
            drafted.plan.ends_with(&format!("\n{last}\n")),
            "{}",
            drafted.plan
        );
        assert!(drafted.errors() > 0);
        assert!(
            drafted
                .problems
                .iter()
                .any(|p| p.message.contains("nobody")),
            "{:?}",
            drafted.problems
        );
        // An answer without a plan is asked for again.
        let third = &model.asked()[2].0.prompt;
        assert!(third.contains("the answer has no plan"), "{third}");
    }

    #[tokio::test]
    async fn the_plan_is_read_from_the_fenced_block_and_the_prose_around_it_dropped() {
        let f = fixture();
        let model = Scripted::new(&[format!(
            "Sure! Here it is:\n\n```plan\n// Drafted by copilot (old): earlier\n{GOOD}\n```\n\
             Run it when you are ready."
        )]);
        let drafted = write(
            &model,
            Provider::Copilot,
            &Default::default(),
            &goal("Fix the parser\nand more words"),
            &f.available,
            &|_| {},
        )
        .await
        .unwrap();
        assert_eq!(
            drafted.plan,
            format!("// Drafted by copilot (gpt-6-sol): Fix the parser\n{GOOD}\n")
        );
    }

    #[tokio::test]
    async fn changing_a_plan_gives_the_model_the_plan_and_the_note() {
        let f = fixture();
        let model = Scripted::new(&[fenced(GOOD)]);
        let request = Ask {
            plan: Some(format!(
                "// Drafted by claude (opus): Fix the parser\n{GOOD}\n"
            )),
            note: Some("add a review by #devs before the sign-off".into()),
            ..Ask::default()
        }
        .request(None);
        let drafted = write(
            &model,
            Provider::Copilot,
            &Default::default(),
            &request,
            &f.available,
            &|_| {},
        )
        .await
        .unwrap();
        let prompt = &model.asked()[0].0.prompt;
        assert!(
            prompt.contains(&format!(
                "<external_content source=\"your last plan\">\n{GOOD}\n"
            )),
            "{prompt}"
        );
        assert!(prompt.contains("add a review by #devs before the sign-off"));
        assert!(
            !prompt.contains("Drafted by claude"),
            "its old first line is dropped"
        );
        assert!(
            drafted
                .plan
                .starts_with("// Drafted by copilot (gpt-6-sol): Fix the parser\n"),
            "{}",
            drafted.plan
        );
    }

    #[test]
    fn a_request_needs_a_goal_an_issue_or_a_plan_and_a_note() {
        let ask = |goal: &str, issue: Option<u64>, plan: &str, note: &str| Ask {
            goal: Some(goal.into()),
            issue,
            plan: Some(plan.into()),
            note: Some(note.into()),
            provider: None,
        };
        assert!(ask("Ship it", None, "", "").validate().is_ok());
        assert!(ask("", Some(4), "", "").validate().is_ok());
        assert!(ask("", None, GOOD, "add a test").validate().is_ok());
        for (bad, says) in [
            (ask("", None, "", ""), "say what the plan should do"),
            (ask("", None, "", "add a test"), "give the plan to change"),
            (ask("", None, GOOD, ""), "write a note"),
            (ask(&"x".repeat(MAX_GOAL + 1), None, "", ""), "over 4000"),
        ] {
            let err = bad.validate().unwrap_err().to_string();
            assert!(err.contains(says), "{err}");
        }
        let issue: Ask = serde_json::from_value(serde_json::json!({ "issue": "#12" })).unwrap();
        assert_eq!(issue.issue, Some(12));
    }
}
