//! Team runs: handing work down a level. A plan step `team @lead:folder` gives its
//! task to a lead's agent, which runs its owner's team plan for that folder (plan
//! text where `{task}` is the task handed down) on its own peers, digital workers
//! and squads, under its own allowlist. The lead reports each step of its team run
//! upward as it moves on, and signs its result with the team run inside: every
//! step's own signed evidence, so whoever handed the work down (and anyone later,
//! offline) can check the whole tree.
//!
//! Limits the requester sets travel with the work: how many levels it may go down
//! (`levels=N`), how long it may take (`timeout=`, which cancels every level below
//! it), and the agents it came down from, to which the team never hands it back.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use iroh::EndpointId;
use serde::{Deserialize, Serialize};

use crate::agent::Agent;
use crate::evidence::GitCommit;
use crate::llm::LlmTask;
use crate::plan::{Plan, RunContext, SquadStore, Step, StepState};
use crate::profile::Profile;
use crate::protocol::{Phase, Progress, SubStep};
use crate::runs::RunRecord;

/// The most levels work may go down from the step that hands it to a team.
pub const MAX_LEVELS: u32 = 3;
/// The most steps a team plan may have.
pub const MAX_TEAM_STEPS: usize = 20;
/// Where the handed-down task goes in a team plan.
pub const TASK: &str = "{task}";
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
    let handed = task
        .prompt
        .strip_prefix(crate::boundary::DIRECTIVE)
        .unwrap_or(&task.prompt)
        .trim()
        .to_owned();
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
}

/// Runs the team plans of team steps handed to a profile's agent, with the profile's
/// squads, knowledge and skills; team runs are saved with its evidence runs.
pub struct TeamRunner {
    profile: Profile,
}

impl TeamRunner {
    pub fn new(profile: Profile) -> Self {
        Self { profile }
    }

    /// Runs `template` (a folder's team plan) for team step `task` from
    /// `requester`, on `agent`'s peers, reporting each change to `observe`.
    pub async fn run(
        &self,
        agent: &Agent,
        template: &str,
        task: &LlmTask,
        request: &TeamRequest,
        requester: EndpointId,
        mut observe: impl FnMut(TeamUpdate<'_>),
    ) -> Result<TeamOutcome> {
        let plan = fill(template, task, request)?;
        let kb = self.profile.knowledge();
        let skills = self.profile.skills();
        let squads = SquadStore::load(&self.profile.squads_path()).unwrap_or_default();
        let recipients = crate::plan::Recipients::of(agent, &self.profile);
        let errors: Vec<String> =
            crate::plan::preflight(&plan, &agent.peers(), &squads, &kb, &skills, &recipients)
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
            recipients,
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
        if let Err(e) = record.save(&self.profile.runs_dir()) {
            eprintln!("could not save team run {}: {e:#}", record.id);
        }
        crate::plan::save_knowledge(&kb, &plan, &finals, &record.id);

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
        Ok(TeamOutcome {
            record,
            output,
            patch,
            git,
            ok,
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

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn clip(text: &str, max: usize) -> String {
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
}
