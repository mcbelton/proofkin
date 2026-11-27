//! Team runs: handing work down a level. A plan step `team @lead:folder` gives its
//! task to a lead's agent, which runs its owner's team plan for that folder (plan
//! text where `{task}` is the task handed down) on its own peers, digital workers
//! and squads, under its own allowlist. Instead of writing that plan, the owner may
//! let the agent's model write one for each team step (see [`plan_prompt`]): it is
//! checked as an owner's plan is, and approved by the owner first if they ask for
//! that (they may ask for changes, which the model makes). The team run's plan and
//! the lead's answer say who wrote it (see [`written_by`]). The lead reports each step
//! of its team run upward as it moves on, and signs its result with the team run
//! inside: every step's own signed evidence, so whoever handed the work down (and
//! anyone later, offline) can check the whole tree.
//!
//! Limits the requester sets travel with the work: how many levels it may go down
//! (`levels=N`), how long it may take (`timeout=`, which cancels every level below
//! it), and the agents it came down from, to which the team never hands it back.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use iroh::EndpointId;
use serde::{Deserialize, Serialize};

use crate::agent::{Agent, PeerInfo};
use crate::evidence::GitCommit;
use crate::knowledge::{KnowledgeBase, labels_of_mark, union_labels};
use crate::llm::LlmTask;
use crate::plan::{Plan, Recipients, RunContext, SquadStore, Step, StepState};
use crate::profile::Profile;
use crate::protocol::{Phase, Progress, SubStep};
use crate::runs::RunRecord;
use crate::skills::{SkillInfo, Skills};

/// The most levels work may go down from the step that hands it to a team.
pub const MAX_LEVELS: u32 = 3;
/// The most steps a team plan may have.
pub const MAX_TEAM_STEPS: usize = 20;
/// Where the handed-down task goes in a team plan.
pub const TASK: &str = "{task}";
/// How many times a lead's model may write the team plan of a team step: a plan that
/// doesn't check out goes back to it with what is wrong, until one does or it wrote
/// this many.
pub const PLAN_TRIES: usize = 3;
/// How many times the lead's owner may ask for changes to the team plan its model
/// wrote, before they approve or reject it.
pub const PLAN_CHANGE_ROUNDS: usize = 2;
/// The most of each final step's result the lead's answer carries.
const MAX_RESULT_CHARS: usize = 3_000;

/// What a team step asks of the lead it is handed to (in [`LlmTask::team`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamRequest {
    /// How many levels the work may go down from the lead: 1, the lead's team does
    /// it; 2, the team plan's own team steps may hand it one level further.
    pub levels: u32,
    /// The agents it came down from (endpoint ids, top first, the sender last).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chain: Vec<String>,
    /// The step's own words (without earlier steps' results), for the team plan's
    /// claims.
    pub brief: String,
}

/// Checks a team plan as its owner sets it: a valid plan of at most
/// [`MAX_TEAM_STEPS`] steps whose tasks say `{task}` somewhere.
pub fn validate_template(text: &str) -> Result<Plan> {
    let plan = Plan::parse(text).context("the team plan isn't a valid plan")?;
    if plan.steps.len() > MAX_TEAM_STEPS {
        bail!(
            "a team plan has at most {MAX_TEAM_STEPS} steps; this one has {}",
            plan.steps.len()
        );
    }
    if !plan.steps.iter().any(|s| s.prompt.contains(TASK)) {
        bail!(
            "no step of the team plan says {TASK}, so the team would never learn what to do: put {TASK} where the handed-down task goes, e.g. dev #devs:app verify=test {TASK}"
        );
    }
    Ok(plan)
}

/// The team plan filled in for `task`: `{task}` in a step's task is the task as
/// handed down (with the earlier steps' results), in a claim the step's own words.
/// Its team steps go down no more levels than the request allows.
pub fn fill(template: &str, task: &LlmTask, request: &TeamRequest) -> Result<Plan> {
    let mut plan = validate_template(template)?;
    let handed = handed_down(task);
    let brief = clip(&one_line(&request.brief), 200);
    for claim in &mut plan.claims {
        claim.text = claim.text.replace(TASK, &brief);
    }
    for step in &mut plan.steps {
        step.prompt = step.prompt.replace(TASK, &handed);
        if let Some(levels) = step.team {
            if request.levels <= 1 {
                bail!(
                    "step {} of the team plan hands the work a level further down, but it may go only {} level{} down (levels=N on the team step that handed it down)",
                    step.id,
                    request.levels,
                    if request.levels == 1 { "" } else { "s" }
                );
            }
            step.team = Some(levels.min(request.levels - 1));
        }
    }
    Ok(plan)
}

/// Team step `task`'s task as handed down (with the earlier steps' results), without
/// the rule for external content that the prompts made of it add again.
fn handed_down(task: &LlmTask) -> String {
    task.prompt
        .strip_prefix(crate::boundary::DIRECTIVE)
        .unwrap_or(&task.prompt)
        .trim()
        .to_owned()
}

/// The prompt that asks a lead's model for the team plan of team step `task`, which
/// `requester` handed down: the task as handed down and its brief, how a team plan is
/// written, and the team it may use: `peers` (but the requester and the agents the
/// work came down from), `squads` and `skills`.
pub fn plan_prompt(
    task: &LlmTask,
    request: &TeamRequest,
    requester: EndpointId,
    peers: &[PeerInfo],
    squads: &SquadStore,
    skills: &[SkillInfo],
) -> String {
    let mut text = format!(
        "You lead a team of agents, and were handed a team step: write the team plan your \
         team runs to do it. Don't do the task yourself: read the files in this folder if \
         they help you plan it, and change nothing.\n\n\
         ## The task handed down\n\n{}\n\nIn a line: {}\n\n",
        handed_down(task),
        clip(&one_line(&request.brief), 200)
    );
    text += "## How a team plan is written\n\n\
             One claim or step per line:\n\
             - `claim: ID text`: a claim the plan proves.\n\
             - `id [after a,b] @peer:folder|#squad:folder [verify=c1,c2] [attempts=N] \
             [apply=step] [skill=name] [claims=ID] task text`: a step. `after a,b` runs it \
             once steps a and b are done, and gives it their results; `@peer:folder` runs it \
             on that peer, in a folder it shares with you, and `#squad:folder` on the least \
             busy member of the squad (with copilot, unless `using claude` or `using codex` \
             comes after who runs it); `verify=` runs those checks of the folder after its \
             model, as proof of the work; `attempts=N` lets its model try again while they \
             fail; `apply=step` starts it from the work of an earlier step; `skill=` gives \
             its model a skill to follow; `claims=` names the claims it proves.\n\
             - `id after x approve @person text`: a sign-off: that person approves the work \
             so far before the steps after it go on.\n";
    if request.levels > 1 {
        text += "- `id team @lead:folder text`: a team step: it hands its task down to that \
                 lead, whose own team does it, in a folder that takes team steps.\n";
    }
    text += &format!(
        "- `// comment`\n\n\
         At most {MAX_TEAM_STEPS} steps. Write {TASK} where the task handed down goes, in the \
         text of at least one step: that is how your team learns what to do. In a claim, \
         {TASK} is the task in a line.\n\n\
         ## Your team\n\n\
         Your peers, the folders each shares with you and the access its tasks get there:\n"
    );
    let mut team = peers_text(
        peers
            .iter()
            .filter(|p| p.id != requester && !request.chain.contains(&p.id.to_string())),
        false,
    );
    if team.is_empty() {
        team = "- none: no peer you may hand this work to is connected\n".into();
    }
    text += &crate::boundary::wrap("your peers", &team);
    text += "\n\nYour squads (a step for #squad runs on its least busy member):\n";
    text += &squads_text(squads);
    text += "\nYour skills (skill=name):\n";
    text += &skills_text(skills);
    text += "\nAnswer with the team plan alone, in one fenced block:\n\n\
             ```plan\none claim or step per line\n```\n";
    crate::boundary::guard(&text)
}

/// `peers` as a prompt that asks for a plan lists them: each with its role and the
/// providers it runs, and each folder it shares with this agent, with the access its tasks
/// get there, its checks and whether it takes team or pull request steps. `detailed` adds
/// whose digital worker a peer is and each folder's MCP servers, by name.
pub(crate) fn peers_text<'a>(
    peers: impl IntoIterator<Item = &'a PeerInfo>,
    detailed: bool,
) -> String {
    let mut text = String::new();
    for peer in peers {
        let role = peer
            .role
            .as_deref()
            .map(one_line)
            .filter(|r| !r.is_empty())
            .map(|r| format!(" ({})", clip(&r, 100)))
            .unwrap_or_default();
        let worker = match &peer.owner {
            Some(owner) if detailed => format!(", a digital worker of {owner}"),
            _ => String::new(),
        };
        let runs = if peer.capabilities.is_empty() {
            "no models".to_owned()
        } else {
            let providers: Vec<String> = peer.capabilities.iter().map(|p| p.to_string()).collect();
            providers.join(", ")
        };
        text += &format!("- @{}{role}{worker}: runs {runs}\n", peer.name);
        for share in &peer.shares {
            text += &format!("  - folder {}: {} access", share.name, share.access);
            if !share.checks.is_empty() {
                text += &format!("; checks {}", share.checks.join(", "));
            }
            if detailed && !share.mcp.is_empty() {
                text += &format!("; MCP servers {}", share.mcp.join(", "));
            }
            if share.team {
                text += "; takes team steps";
            }
            if share.pull_requests {
                text += "; takes pull request steps";
            }
            text += "\n";
        }
    }
    text
}

/// `squads` as a prompt that asks for a plan lists them: each with its members.
pub(crate) fn squads_text(squads: &SquadStore) -> String {
    let mut text = String::new();
    for squad in &squads.squads {
        text += &format!("- #{}: {}\n", squad.name, squad.members.join(", "));
    }
    if squads.squads.is_empty() {
        text += "- none\n";
    }
    text
}

/// `skills` as a prompt that asks for a plan lists them: each with what it is for.
pub(crate) fn skills_text(skills: &[SkillInfo]) -> String {
    let mut text = String::new();
    for skill in skills {
        let about = clip(&one_line(&skill.description), 200);
        text += &format!("- {}: {about}\n", skill.name);
    }
    if skills.is_empty() {
        text += "- none\n";
    }
    text
}

/// `prompt` (see [`plan_prompt`]) again, after the model answered it with `plan`, which
/// doesn't check out: `problems` says why.
pub fn plan_again_prompt(prompt: &str, plan: &str, problems: &str) -> String {
    again_prompt("team plan", prompt, plan, problems)
}

/// `prompt` again, after the model answered it with `plan` (a `noun`: a team plan, a
/// plan), which doesn't check out: `problems` says why.
pub(crate) fn again_prompt(noun: &str, prompt: &str, plan: &str, problems: &str) -> String {
    crate::boundary::guard(&format!(
        "{prompt}\n\n## Your last {noun} doesn't check out\n\n{}\n\n\
         What is wrong with it: {problems}\n\n\
         Write the whole {noun} again, with that fixed, in one fenced block (```plan).",
        crate::boundary::wrap(&format!("your last {noun}"), plan)
    ))
}

/// `prompt` (see [`plan_prompt`]) again, after `by`, the lead's owner, read `plan`, the
/// team plan the model wrote, and asked for changes to it: `note` says which.
pub fn plan_changes_prompt(prompt: &str, plan: &str, by: &str, note: &str) -> String {
    changes_prompt("team plan", prompt, plan, by, note)
}

/// `prompt` again, after `by` read `plan` (a `noun`: a team plan, a plan) before it ran,
/// and asked for changes to it: `note` says which.
pub(crate) fn changes_prompt(noun: &str, prompt: &str, plan: &str, by: &str, note: &str) -> String {
    crate::boundary::guard(&format!(
        "{prompt}\n\n## {by} asked for changes to your {noun}\n\n{}\n\n\
         {by} read this {noun} before it ran, and asked for changes:\n\n{}\n\n\
         Write the whole {noun} again, with those changes, in one fenced block (```plan).",
        crate::boundary::wrap(&format!("your last {noun}"), plan),
        note.trim()
    ))
}

/// The team plan in a model's `answer`: its first fenced block, else the whole answer.
pub fn written_plan(answer: &str) -> String {
    let lines: Vec<&str> = answer.lines().collect();
    // The backticks the block's first line starts with: the line that closes it has as
    // many or more, and nothing else.
    let Some((start, open)) = lines.iter().enumerate().find_map(|(i, line)| {
        let line = line.trim();
        let ticks = line.len() - line.trim_start_matches('`').len();
        (ticks >= 3).then_some((i, ticks))
    }) else {
        return answer.trim().to_owned();
    };
    let closes = |line: &&str| {
        let line = line.trim();
        line.len() >= open && line.chars().all(|c| c == '`')
    };
    lines[start + 1..]
        .iter()
        .copied()
        .take_while(|line| !closes(line))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_owned()
}

/// What a lead's owner is asked to approve: `plan`, the team plan their agent's model
/// wrote for the team step `from` handed to folder `folder`; `asked`, if the model wrote
/// it again because they asked for changes: that round of changes and their note.
pub fn approval_text(
    from: &str,
    folder: &str,
    request: &TeamRequest,
    plan: &str,
    asked: Option<(usize, &str)>,
) -> String {
    let longest = plan.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(longest.max(2) + 1);
    let text = format!(
        "{from} handed a team step to '{folder}': {}\n\n\
         Your model wrote this team plan for it ({TASK} is the task as handed down):\n\n\
         {fence}text\n{}\n{fence}\n\n\
         Approving runs it on your team.",
        clip(&one_line(&request.brief), 200),
        plan.trim_end()
    );
    match asked {
        None => text,
        Some((round, note)) => format!(
            "**Asked again** (round {round} of changes): you asked for changes, and your \
             model wrote the team plan again with your note; the new plan is below.\n\n\
             > {}\n\n---\n\n{text}",
            note.trim().replace('\n', "\n> ")
        ),
    }
}

/// Who wrote the team plan of a team run, as its plan (in a comment) and its lead's
/// answer say: `lead`'s model, with `provider` (on `model`, if it said which), for
/// `from`'s team step; `approved`: how many rounds of changes `lead` asked for before
/// approving it, if they were asked to.
pub fn written_by(
    lead: &str,
    provider: crate::llm::Provider,
    model: Option<&str>,
    from: &str,
    approved: Option<usize>,
) -> String {
    let model = model.map(|m| format!(", {m}")).unwrap_or_default();
    let approved = match approved {
        None => String::new(),
        Some(0) => format!(", and approved by {lead}"),
        Some(1) => format!(", and approved by {lead} after 1 round of changes"),
        Some(n) => format!(", and approved by {lead} after {n} rounds of changes"),
    };
    format!(
        "This team plan was written by {lead}'s model ({provider}{model}) for {from}'s team \
         step{approved}"
    )
}

/// A team run moving on, as [`TeamRunner::run`] reports it.
pub struct TeamUpdate<'a> {
    pub run: &'a str,
    /// The team plan as filled in.
    pub plan: &'a Plan,
    /// The step that moved on (none: the run just started).
    pub step: Option<(&'a str, &'a StepState)>,
    /// Every step's state now.
    pub states: &'a BTreeMap<String, StepState>,
}

/// What a team run hands back up.
pub struct TeamOutcome {
    pub record: RunRecord,
    /// The verdict, each step's outcome, and the results of its final steps.
    pub output: String,
    /// The work of its last step that committed any, and that commit.
    pub patch: Option<String>,
    pub git: Option<GitCommit>,
    /// Every step is done, and the run proves its claims (if it has any).
    pub ok: bool,
    /// The Microsoft 365 labels its steps' results hold (see [`TeamRunner::run`]): its
    /// output and record hold them too. How many of its steps hold such content.
    pub labels: Vec<String>,
    pub marked: usize,
}

/// Runs the team plans of team steps handed to a profile's agent, with the profile's
/// squads, knowledge and skills; team runs are saved with its evidence runs.
pub struct TeamRunner {
    profile: Profile,
}

/// What a lead's team plans run with: its profile's squads, knowledge and skills, and
/// who may receive their steps' content.
struct Kit {
    squads: SquadStore,
    kb: KnowledgeBase,
    skills: Skills,
    recipients: Recipients,
}

/// `plan` with each step marked with the Microsoft 365 labels of team step `task` (those its
/// requester marked it with, and its work context's): a step's task holds the team step's,
/// so it goes only where they may.
fn seeded(mut plan: Plan, task: &LlmTask) -> Plan {
    let mut given = task.m365.clone();
    for part in &task.context {
        given = union_labels(&given, &part.labels);
    }
    if given.is_empty() {
        return plan;
    }
    for step in &mut plan.steps {
        // A `workiq=` step's mark is the labels its results turn out to have.
        let own = if step.brings_m365() {
            step.m365.clone().unwrap_or_default()
        } else {
            step.m365.as_deref().map(labels_of_mark).unwrap_or_default()
        };
        step.m365 = Some(union_labels(&own, &given));
    }
    plan
}

/// `text` (a team plan) filled in for team step `task` (see [`fill`]), if it can run
/// now: [`crate::plan::preflight`] finds no error in it on `agent`'s peers with `kit`.
fn checked(
    agent: &Agent,
    kit: &Kit,
    text: &str,
    task: &LlmTask,
    request: &TeamRequest,
) -> Result<Plan> {
    let plan = seeded(fill(text, task, request)?, task);
    let errors: Vec<String> = crate::plan::preflight(
        &plan,
        &agent.peers(),
        &kit.squads,
        &kit.kb,
        &kit.skills,
        &kit.recipients,
    )
    .into_iter()
    .filter(|p| p.error)
    .map(|p| match p.step {
        Some(step) => format!("step {step}: {}", p.message),
        None => p.message,
    })
    .collect();
    if !errors.is_empty() {
        bail!(
            "{}'s team plan can't run now: {}",
            agent.name(),
            errors.join("; ")
        );
    }
    Ok(plan)
}

impl TeamRunner {
    pub fn new(profile: Profile) -> Self {
        Self { profile }
    }

    fn kit(&self, agent: &Agent) -> Kit {
        Kit {
            squads: SquadStore::load(&self.profile.squads_path()).unwrap_or_default(),
            kb: self.profile.knowledge(),
            skills: self.profile.skills(),
            recipients: self.recipients(agent),
        }
    }

    /// Who may receive its team runs' content now (see [`Recipients::of`]).
    pub fn recipients(&self, agent: &Agent) -> Recipients {
        Recipients::of(agent, &self.profile)
    }

    /// The prompt that asks `agent`'s model for the team plan of team step `task` from
    /// `requester` (see [`plan_prompt`]): its team is `agent`'s peers now and the
    /// profile's squads and skills.
    pub fn prompt(
        &self,
        agent: &Agent,
        task: &LlmTask,
        request: &TeamRequest,
        requester: EndpointId,
    ) -> String {
        let squads = SquadStore::load(&self.profile.squads_path()).unwrap_or_default();
        let skills = self.profile.skills().list();
        plan_prompt(task, request, requester, &agent.peers(), &squads, &skills)
    }

    /// Checks `text` as the team plan of team step `task` as [`TeamRunner::run`] does
    /// before it runs one: it fills in (see [`fill`]) and has no error
    /// [`crate::plan::preflight`] finds on `agent`'s peers now, with the profile's
    /// squads, knowledge and skills.
    pub fn check(
        &self,
        agent: &Agent,
        text: &str,
        task: &LlmTask,
        request: &TeamRequest,
    ) -> Result<Plan> {
        checked(agent, &self.kit(agent), text, task, request)
    }

    /// Runs `template` (a folder's team plan, or the one its model wrote) for team
    /// step `task` from `requester`, on `agent`'s peers, reporting each change to
    /// `observe`.
    pub async fn run(
        &self,
        agent: &Agent,
        template: &str,
        task: &LlmTask,
        request: &TeamRequest,
        requester: EndpointId,
        mut observe: impl FnMut(TeamUpdate<'_>),
    ) -> Result<TeamOutcome> {
        let kit = self.kit(agent);
        agent.settle_peers().await;
        let plan = checked(agent, &kit, template, task, request)?;
        let Kit {
            squads,
            kb,
            skills,
            recipients,
        } = kit;
        let ready = plan.prepare(&kb, &skills)?;
        let mut chain: Vec<EndpointId> = request
            .chain
            .iter()
            .filter_map(|c| c.parse().ok())
            .collect();
        if !chain.contains(&requester) {
            chain.push(requester);
        }
        let run_id = crate::runs::new_run_id();
        let mut states: BTreeMap<String, StepState> = plan
            .steps
            .iter()
            .map(|s| (s.id.clone(), StepState::Waiting))
            .collect();
        observe(TeamUpdate {
            run: &run_id,
            plan: &plan,
            step: None,
            states: &states,
        });
        let started = crate::runs::unix_now();
        let ctx = RunContext {
            run_id: Some(run_id.clone()),
            chain,
            // The team's commits reference the issues of the step they do together.
            issues: plan
                .steps
                .iter()
                .map(|s| (s.id.clone(), task.issues.clone()))
                .filter(|(_, issues)| !issues.is_empty())
                .collect(),
            // The team step's own task already has its issue's text in it.
            issue_texts: Default::default(),
            recipients,
            work_context: crate::plan::work_context(&self.profile, &plan)?,
            work_log: crate::plan::WorkLog::default(),
            notices: crate::plan::notices(&self.profile, &plan),
            ..RunContext::default()
        };
        let finals = crate::plan::run_plan_with(agent, &squads, &ready, &ctx, |id, state| {
            states.insert(id.to_owned(), state.clone());
            observe(TeamUpdate {
                run: &run_id,
                plan: &plan,
                step: Some((id, state)),
                states: &states,
            });
        })
        .await;
        let brief = clip(&one_line(&request.brief), 200);
        let mut record = RunRecord::new(
            agent.name(),
            &agent.id().to_string(),
            &template.replace(TASK, &brief),
            &plan,
            &finals,
            started,
        );
        record.id = run_id;
        record.peer_ids = crate::runs::peer_ids(agent);
        record.workiq = ctx.work_log.records();
        if let Err(e) = record.save(&self.profile.runs_dir()) {
            eprintln!("could not save team run {}: {e:#}", record.id);
        }
        // What Work IQ found stays on this machine: the record goes to the requester.
        record.workiq.clear();
        crate::plan::save_knowledge(
            &kb,
            &crate::plan::with_actual_labels(&plan, &ctx.work_log),
            &finals,
            &record.id,
        );

        let mut output = format!(
            "Team run {} by {}: {}\n",
            record.id,
            agent.name(),
            record.team_verdict()
        );
        for s in &record.steps {
            output += &format!("- {}\n", crate::runs::team_line(s));
        }
        // The results of the steps nothing else waits for.
        for s in plan
            .steps
            .iter()
            .filter(|s| !plan.steps.iter().any(|o| o.after.contains(&s.id)))
        {
            if let Some(StepState::Done {
                peer, output: out, ..
            }) = finals.get(&s.id)
            {
                output += &format!(
                    "\n### {} (done by {peer})\n{}\n",
                    s.id,
                    clip(out.trim(), MAX_RESULT_CHARS)
                );
            }
        }
        let committed = plan
            .steps
            .iter()
            .rev()
            .find_map(|s| match finals.get(&s.id) {
                Some(StepState::Done {
                    patch: Some(patch),
                    evidence,
                    ..
                }) => Some((patch.clone(), evidence.as_ref().and_then(|e| e.git.clone()))),
                _ => None,
            });
        let ok = finals.values().all(|s| matches!(s, StepState::Done { .. }))
            && (record.claims.is_empty() || record.passed());
        let (patch, git) = committed.map_or((None, None), |(p, g)| (Some(p), g));
        // What its output and record hold: the results and errors of the steps that did
        // work, with the Microsoft 365 content their peers' signed evidence names and that
        // the run gave them (`workiq=` and `@workiq` steps, marked topics, the team step's
        // own, and the steps after those).
        let held = crate::plan::with_result_labels(
            &crate::plan::with_actual_labels(&plan, &ctx.work_log),
            &finals,
        )
        .marked(&kb);
        let (mut labels, mut marked) = (Vec::new(), 0);
        for step in &held.steps {
            let worked = match finals.get(&step.id) {
                Some(StepState::Done { .. }) => true,
                Some(StepState::Failed { evidence, .. }) => evidence.is_some(),
                _ => false,
            };
            if let (true, Some(mark)) = (worked, &step.m365) {
                labels = union_labels(&labels, &labels_of_mark(mark));
                marked += 1;
            }
        }
        Ok(TeamOutcome {
            record,
            output,
            patch,
            git,
            ok,
            labels,
            marked,
        })
    }
}

/// Where a team run is, as the lead reports it upward.
pub fn progress(update: &TeamUpdate<'_>) -> Progress {
    let steps = sub_steps(&update.plan.steps, update.states);
    let count = |status: &str| steps.iter().filter(|s| s.status == status).count();
    let (done, running) = (count("done"), count("running"));
    let mut text = format!(
        "team run: {done} of {} step{} done",
        steps.len(),
        if steps.len() == 1 { "" } else { "s" }
    );
    if running > 0 {
        text += &format!(", {running} running");
    }
    let failed = count("failed") + count("skipped");
    if failed > 0 {
        text += &format!(", {failed} failed or skipped");
    }
    Progress {
        run: Some(update.run.to_owned()),
        steps,
        ..Progress::new(Phase::Team, text)
    }
}

/// Each of `steps` as reported upward, with its state.
pub fn sub_steps(steps: &[Step], states: &BTreeMap<String, StepState>) -> Vec<SubStep> {
    steps
        .iter()
        .map(|s| sub_step(s, states.get(&s.id).unwrap_or(&StepState::Waiting)))
        .collect()
}

/// The steps of a finished team run (from its record).
pub fn run_steps(run: &RunRecord) -> Vec<SubStep> {
    run.steps
        .iter()
        .map(|r| sub_step(&r.step, &r.state))
        .collect()
}

/// `step` in `state` as reported upward: where it runs, how it is doing, and its own
/// team run's steps if it is a team step.
pub fn sub_step(step: &Step, state: &StepState) -> SubStep {
    let mut sub = SubStep {
        id: step.id.clone(),
        target: format!(
            "{}{}",
            step.target,
            step.share
                .as_ref()
                .map(|f| format!(":{f}"))
                .unwrap_or_default()
        ),
        after: step.after.clone(),
        ..SubStep::default()
    };
    let nested = |e: &Option<crate::evidence::Evidence>| {
        e.as_ref()
            .and_then(|e| e.team.as_ref())
            .map(|run| run_steps(run))
            .unwrap_or_default()
    };
    match state {
        StepState::Waiting => sub.status = "waiting".into(),
        StepState::Running { peer, progress } => {
            sub.status = "running".into();
            sub.target = peer.clone();
            if let Some(p) = progress {
                sub.text = p.text.clone();
                sub.steps = p.steps.clone();
            }
        }
        StepState::Done {
            peer,
            verified,
            evidence,
            ..
        } => {
            sub.status = "done".into();
            sub.target = peer.clone();
            sub.signed = *verified;
            sub.steps = nested(evidence);
        }
        StepState::Failed {
            peer,
            error,
            evidence,
            ..
        } => {
            sub.status = "failed".into();
            if let Some(peer) = peer {
                sub.target = peer.clone();
            }
            sub.text = clip(error.lines().next().unwrap_or_default(), 200);
            sub.steps = nested(evidence);
        }
        StepState::Skipped { reason } => {
            sub.status = "skipped".into();
            sub.text = reason.clone();
        }
    }
    sub
}

pub(crate) fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(crate) fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut cut: String = text.chars().take(max.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::Provider;

    const TEAM: &str = "claim: T-1 {task} works\n\
        dev #devs:app verify=test attempts=2 claims=T-1 {task}\n\
        review after dev @carol:app apply=dev verify=test claims=T-1 Review this change: {task}";

    fn request(levels: u32) -> TeamRequest {
        TeamRequest {
            levels,
            chain: Vec::new(),
            brief: "Build the\nlogin page".into(),
        }
    }

    #[test]
    fn a_team_plan_is_filled_in_with_the_task_handed_down() {
        let task = LlmTask::new(
            Provider::Copilot,
            crate::boundary::guard(
                "Results of the earlier steps…\n---\nYour task:\nBuild the login page",
            ),
        );
        let plan = fill(TEAM, &task, &request(1)).unwrap();
        // Claims get the step's own words; tasks the task as handed down (with the
        // earlier steps' results), without the rule a lead's step adds again.
        assert_eq!(plan.claims[0].text, "Build the login page works");
        assert!(
            plan.steps[0]
                .prompt
                .starts_with("Results of the earlier steps")
        );
        assert!(
            plan.steps[1]
                .prompt
                .starts_with("Review this change: Results")
        );
        assert!(!plan.steps[0].prompt.contains(crate::boundary::DIRECTIVE));
    }

    #[test]
    fn team_plans_need_the_task_and_stay_within_the_levels_allowed() {
        let err = validate_template("dev @alice do something").unwrap_err();
        assert!(
            format!("{err:#}").contains("never learn what to do"),
            "{err:#}"
        );
        let deeper = "sub team @bob:app levels=2 {task}";
        let task = LlmTask::new(Provider::Copilot, "x");
        let err = fill(deeper, &task, &request(1)).unwrap_err();
        assert!(
            format!("{err:#}").contains("may go only 1 level down"),
            "{err:#}"
        );
        // With two levels allowed, the team step below gets the one that is left.
        assert_eq!(
            fill(deeper, &task, &request(2)).unwrap().steps[0].team,
            Some(1)
        );
        let big: String = (0..=MAX_TEAM_STEPS)
            .map(|i| format!("s{i} @a {TASK}\n"))
            .collect();
        assert!(
            validate_template(&big)
                .unwrap_err()
                .to_string()
                .contains("at most")
        );
    }

    #[test]
    fn a_leads_model_is_asked_for_a_plan_for_the_task_handed_down_with_the_team_it_may_use() {
        use crate::llm::Access;
        use crate::plan::Squad;
        use crate::shares::ShareInfo;
        let peer = |name: &str, role: Option<&str>, providers, shares| PeerInfo {
            name: name.into(),
            id: iroh::SecretKey::generate().public(),
            capabilities: providers,
            shares,
            owner: None,
            role: role.map(Into::into),
            agent: None,
            load: None,
            models: Default::default(),
            offers: Default::default(),
        };
        let app = ShareInfo {
            name: "app".into(),
            access: Access::WorkspaceWrite,
            checks: vec!["test".into(), "lint".into()],
            team: true,
            ..Default::default()
        };
        let docs = ShareInfo {
            name: "docs".into(),
            access: Access::ReadOnly,
            pull_requests: true,
            ..Default::default()
        };
        let peers = vec![
            peer(
                "alice",
                Some("test\nengineer"),
                vec![Provider::Copilot],
                vec![app],
            ),
            peer(
                "bob",
                None,
                vec![Provider::Claude, Provider::Codex],
                vec![docs],
            ),
            peer("boss", None, vec![Provider::Copilot], Vec::new()),
            peer("top", None, vec![Provider::Copilot], Vec::new()),
        ];
        let requester = peers[2].id;
        let mut request = request(1);
        request.chain = vec![peers[3].id.to_string(), requester.to_string()];
        let squads = SquadStore {
            squads: vec![Squad {
                name: "devs".into(),
                members: vec!["alice".into(), "bob".into()],
            }],
        };
        let skills = [SkillInfo {
            name: "review".into(),
            description: "Reviews a\nchange".into(),
            source: "profile",
            plugin: None,
        }];
        let task = LlmTask::new(
            Provider::Copilot,
            crate::boundary::guard(&format!(
                "Results of the earlier steps:\n{}\n---\nYour task:\nBuild the login page",
                crate::boundary::wrap("step:design", "a form")
            )),
        );
        let prompt = plan_prompt(&task, &request, requester, &peers, &squads, &skills);
        // The task as handed down, once, with its own words in a line.
        assert!(prompt.starts_with(crate::boundary::DIRECTIVE), "{prompt}");
        assert_eq!(prompt.matches(crate::boundary::DIRECTIVE).count(), 1);
        assert!(prompt.contains(
            "## The task handed down\n\nResults of the earlier steps:\n\
             <external_content source=\"step:design\">"
        ));
        let line = "Your task:\nBuild the login page\n\nIn a line: Build the login page\n";
        assert!(prompt.contains(line));
        // How a plan is written, without team steps when the work may go no further down.
        for syntax in [
            "`claim: ID text`",
            "`id [after a,b] @peer:folder|#squad:folder [verify=c1,c2] [attempts=N] \
             [apply=step] [skill=name] [claims=ID] task text`",
            "`id after x approve @person text`",
            "`// comment`",
            "At most 20 steps. Write {task} where the task handed down goes",
        ] {
            assert!(prompt.contains(syntax), "{syntax}: {prompt}");
        }
        assert!(!prompt.contains("`id team @lead:folder text`"));
        request.levels = 2;
        let deeper = plan_prompt(&task, &request, requester, &peers, &squads, &skills);
        assert!(deeper.contains("`id team @lead:folder text`"));
        // Its team, with what its peers say of themselves as data, but not the agents the
        // work came down from.
        let team = "<external_content source=\"your peers\">\n\
                    - @alice (test engineer): runs copilot\n  \
                    - folder app: workspace-write access; checks test, lint; takes team steps\n\
                    - @bob: runs claude, codex\n  \
                    - folder docs: read-only access; takes pull request steps\n\
                    </external_content>";
        assert!(prompt.contains(team), "{prompt}");
        assert!(!prompt.contains("@boss") && !prompt.contains("@top"));
        assert!(prompt.contains("- #devs: alice, bob\n"));
        assert!(prompt.contains("- review: Reviews a change\n"));
        assert!(prompt.contains("in one fenced block:\n\n```plan\n"));

        // Asked again, it is told what is wrong with the plan it wrote.
        let again = plan_again_prompt(&prompt, "dev @nobody {task}", "step dev: no peer 'nobody'");
        assert!(again.starts_with(&prompt));
        assert!(again.contains(
            "<external_content source=\"your last team plan\">\n\
             dev @nobody {task}\n</external_content>"
        ));
        assert!(again.contains("What is wrong with it: step dev: no peer 'nobody'"));

        // Its owner asked for changes: it is given their note with the plan they read.
        let changes = plan_changes_prompt(&prompt, "dev @alice {task}", "lead", "Add a review\n");
        assert!(changes.starts_with(&prompt));
        assert!(
            changes.contains(
                "## lead asked for changes to your team plan\n\n\
                 <external_content source=\"your last team plan\">\n\
                 dev @alice {task}\n</external_content>\n\n\
                 lead read this team plan before it ran, and asked for changes:\n\n\
                 Add a review\n\n"
            ),
            "{changes}"
        );
    }

    #[test]
    fn the_plan_a_model_writes_is_its_answers_first_fenced_block_else_the_whole_answer() {
        let answer = "Here it is:\n```plan\nclaim: T-1 {task} works\ndev @alice:app {task}\n```\n\
                      And another:\n```\nother @bob {task}\n```";
        assert_eq!(
            written_plan(answer),
            "claim: T-1 {task} works\ndev @alice:app {task}"
        );
        assert_eq!(written_plan("\n  dev @alice {task}\n"), "dev @alice {task}");
        // A longer fence holds shorter ones, and one left open runs to the end.
        assert_eq!(
            written_plan("````\ndev @alice {task}\n```\n````\nafter"),
            "dev @alice {task}\n```"
        );
        assert_eq!(
            written_plan("```plan\ndev @alice {task}"),
            "dev @alice {task}"
        );
        // Its owner sees it in a block the plan can't close.
        let text = approval_text(
            "boss",
            "app",
            &request(1),
            "dev @alice {task} ```x```",
            None,
        );
        assert!(text.starts_with("boss handed a team step to 'app': Build the login page\n"));
        assert!(
            text.contains("\n````text\ndev @alice {task} ```x```\n````\n"),
            "{text}"
        );
        // Written again after they asked for changes, it comes after their note.
        let asked = Some((2, "Add a\nreview "));
        let again = approval_text("boss", "app", &request(1), "dev @alice {task}", asked);
        assert!(
            again.starts_with(
                "**Asked again** (round 2 of changes): you asked for changes, and your model \
                 wrote the team plan again with your note; the new plan is below.\n\n\
                 > Add a\n> review\n\n---\n\nboss handed a team step to 'app': "
            ),
            "{again}"
        );
    }
}
