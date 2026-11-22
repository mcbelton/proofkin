//! Evidence runs: the durable record of a plan run. Each run is saved in the boss's
//! profile (`runs/<id>/run.json`, a Markdown `report.md` and the patch of each
//! step that committed work) with a verdict for every claim:
//!
//! - **proven**: every step that claims it finished, each backed by runtime
//!   evidence signed by the peer that did the work: checks that all passed, or a
//!   team member's approval;
//! - **failed**: a step that claims it failed or was skipped;
//! - **unverified**: it rests on a model's word only (no checks or approval), or
//!   no step claims it;
//! - **pending**: the run stopped before its steps finished.
//!
//! The run passes only if every claim is proven. Anyone with `run.json` can
//! re-check every signature (`proofkin runs verify`), without trusting the boss.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::evidence::{Evidence, EvidenceKind, sha256_hex};
use crate::plan::{ClaimDef, Plan, Step, StepState};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Proven,
    Failed,
    Unverified,
    Pending,
}

impl Verdict {
    pub fn label(self) -> &'static str {
        match self {
            Verdict::Proven => "✅ proven",
            Verdict::Failed => "❌ failed",
            Verdict::Unverified => "⚠️ unverified",
            Verdict::Pending => "⏳ pending",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepRecord {
    pub step: Step,
    pub state: StepState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRecord {
    pub id: String,
    /// The agent that ran the plan, and its endpoint id.
    pub boss: String,
    pub boss_id: String,
    pub started_unix: u64,
    pub finished_unix: u64,
    /// The plan as written.
    pub plan_text: String,
    pub claims: Vec<ClaimDef>,
    pub steps: Vec<StepRecord>,
    /// Endpoint id of each peer (by the boss's allowlist name; `you` is the boss) the
    /// steps ran on, so re-verification knows whose key must have signed each step.
    #[serde(default)]
    pub peer_ids: BTreeMap<String, String>,
    /// The GitHub issues the run was tracked on (see [`crate::tracker`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tracked: Option<crate::tracker::TrackedRun>,
    /// What the steps with `workiq=` found, by step: sources, counts and request ids. It
    /// stays on this machine: it is left out of a team run that goes in signed evidence.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub workiq: BTreeMap<String, crate::plan::WorkRecord>,
}

/// How far (seconds) evidence timestamps may lie outside the run, for clock skew.
const CLOCK_SKEW: u64 = 300;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClaimResult {
    pub claim: ClaimDef,
    pub verdict: Verdict,
    pub steps: Vec<String>,
}

/// Short summary of a saved run, for listings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunSummary {
    pub id: String,
    /// What the run was about, in a line (see [`RunRecord::title`]).
    pub title: String,
    pub started_unix: u64,
    pub passed: bool,
    pub proven: usize,
    pub claims: usize,
    pub steps: usize,
    pub steps_done: usize,
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The endpoint id of every peer a plan can name (`you` is the agent itself), for
/// [`RunRecord::peer_ids`].
pub fn peer_ids(agent: &crate::Agent) -> BTreeMap<String, String> {
    let mut ids: BTreeMap<String, String> = agent
        .trusted()
        .peers
        .iter()
        .map(|p| (p.name.clone(), p.id.to_string()))
        .collect();
    ids.insert("you".into(), agent.id().to_string());
    ids
}

/// `YYYY-MM-DD HH:MM:SS` (UTC) for `secs` since the Unix epoch.
pub fn utc(secs: u64) -> String {
    let (days, rem) = ((secs / 86_400) as i64, secs % 86_400);
    // Howard Hinnant's civil-from-days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// What a finished step proves on its own.
fn step_verdict(record: &StepRecord) -> Verdict {
    match &record.state {
        StepState::Waiting | StepState::Running { .. } => Verdict::Pending,
        StepState::Failed { .. } | StepState::Skipped { .. } => Verdict::Failed,
        StepState::Done {
            evidence, verified, ..
        } => match evidence {
            Some(e)
                if *verified && e.kind == EvidenceKind::Approval && e.approved == Some(true) =>
            {
                Verdict::Proven
            }
            // A team step: proven if its team run (checked through) proves its claims.
            Some(e) if e.team.is_some() => match &e.team {
                Some(run) if *verified && run.passed() => Verdict::Proven,
                _ => Verdict::Unverified,
            },
            Some(e) if *verified && !e.checks.is_empty() && e.checks_passed() => Verdict::Proven,
            _ => Verdict::Unverified,
        },
    }
}

/// A new run id: the time it is made, to the millisecond, so ids sort by time.
pub fn new_run_id() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let rand = data_encoding::HEXLOWER.encode(&iroh::SecretKey::generate().to_bytes()[..1]);
    let stamp: String = utc(now / 1000)
        .chars()
        .filter(char::is_ascii_digit)
        .collect();
    format!("{}-{}-{:03}{rand}", &stamp[..8], &stamp[8..], now % 1000)
}

/// Checks a lead's team run (embedded in its signed evidence) through: it is the
/// lead's (`lead_id`), and every step's evidence is valid and belongs to it (on every
/// level below too).
pub fn verify_team(run: &RunRecord, lead_id: &str) -> Result<()> {
    if run.boss_id != lead_id {
        bail!("the team run is {}'s, not the lead's", short(&run.boss_id));
    }
    for (step, check) in run.verify(None) {
        if let Err(e) = check {
            bail!("team step {step}: {e:#}");
        }
    }
    Ok(())
}

impl RunRecord {
    pub fn new(
        boss: &str,
        boss_id: &str,
        plan_text: &str,
        plan: &Plan,
        states: &std::collections::BTreeMap<String, StepState>,
        started_unix: u64,
    ) -> Self {
        Self {
            id: new_run_id(),
            boss: boss.to_owned(),
            boss_id: boss_id.to_owned(),
            started_unix,
            finished_unix: unix_now(),
            plan_text: plan_text.to_owned(),
            claims: plan.claims.clone(),
            peer_ids: BTreeMap::new(),
            tracked: None,
            workiq: BTreeMap::new(),
            steps: plan
                .steps
                .iter()
                .map(|s| StepRecord {
                    step: s.clone(),
                    state: states.get(&s.id).cloned().unwrap_or(StepState::Waiting),
                })
                .collect(),
        }
    }

    pub fn claim_results(&self) -> Vec<ClaimResult> {
        self.claims
            .iter()
            .map(|claim| {
                let steps: Vec<&StepRecord> = self
                    .steps
                    .iter()
                    .filter(|s| s.step.claims.contains(&claim.id))
                    .collect();
                let verdicts: Vec<Verdict> = steps.iter().map(|s| step_verdict(s)).collect();
                let verdict = if verdicts.is_empty() {
                    Verdict::Unverified
                } else if verdicts.contains(&Verdict::Failed) {
                    Verdict::Failed
                } else if verdicts.contains(&Verdict::Pending) {
                    Verdict::Pending
                } else if verdicts.contains(&Verdict::Unverified) {
                    Verdict::Unverified
                } else {
                    Verdict::Proven
                };
                ClaimResult {
                    claim: claim.clone(),
                    verdict,
                    steps: steps.iter().map(|s| s.step.id.clone()).collect(),
                }
            })
            .collect()
    }

    /// A run passes when it has claims and every one is proven.
    pub fn passed(&self) -> bool {
        let results = self.claim_results();
        !results.is_empty() && results.iter().all(|r| r.verdict == Verdict::Proven)
    }

    pub fn summary(&self) -> RunSummary {
        let results = self.claim_results();
        RunSummary {
            id: self.id.clone(),
            title: self.title(),
            started_unix: self.started_unix,
            passed: self.passed(),
            proven: results
                .iter()
                .filter(|r| r.verdict == Verdict::Proven)
                .count(),
            claims: results.len(),
            steps: self.steps.len(),
            steps_done: self
                .steps
                .iter()
                .filter(|s| matches!(s.state, StepState::Done { .. }))
                .count(),
        }
    }

    /// What the run was about, in a line: its first claim, else its first step's task.
    pub fn title(&self) -> String {
        let text = self
            .claims
            .first()
            .map(|c| c.text.as_str())
            .or_else(|| self.steps.first().map(|s| s.step.prompt.as_str()))
            .unwrap_or_default();
        clip(&one_line(text), 100)
    }

    /// The keys that signed this run's evidence, once each in step order, by the name
    /// the boss's allowlist gives them (else the name the signer announced).
    pub fn signers(&self) -> Vec<(String, String)> {
        let mut signers: Vec<(String, String)> = Vec::new();
        for s in &self.steps {
            let (StepState::Done {
                evidence: Some(e), ..
            }
            | StepState::Failed {
                evidence: Some(e), ..
            }) = &s.state
            else {
                continue;
            };
            if signers.iter().any(|(_, id)| *id == e.signer_id) {
                continue;
            }
            let name = self
                .peer_ids
                .iter()
                .find(|(_, id)| **id == e.signer_id)
                .map(|(name, _)| name.clone())
                .unwrap_or_else(|| e.signer.clone());
            signers.push((name, e.signer_id.clone()));
        }
        signers
    }

    /// A one-step run for a task delegated on its own (outside a plan) whose peer sent
    /// back something to prove: the checks its runtime ran, or a commit. With checks,
    /// the run claims they pass after the task.
    pub fn for_task(
        boss: &str,
        boss_id: &str,
        peer: &str,
        task: &crate::llm::LlmTask,
        delivered: crate::agent::Delivered,
        started_unix: u64,
    ) -> Self {
        let prompt = one_line(&task.prompt);
        let claims = if task.checks.is_empty() {
            Vec::new()
        } else {
            vec![ClaimDef {
                id: "T-1".into(),
                text: format!(
                    "{} pass{} after: {}",
                    task.checks.join(", "),
                    if task.checks.len() == 1 { "es" } else { "" },
                    clip(&prompt, 100)
                ),
            }]
        };
        let step = Step {
            id: "task".into(),
            after: Vec::new(),
            target: crate::plan::Target::Peer(peer.to_owned()),
            share: task.share.clone(),
            provider: task.provider,
            prompt: task.prompt.clone(),
            brief: String::new(),
            checks: task.checks.clone(),
            attempts: task.attempts,
            claims: claims.iter().map(|c| c.id.clone()).collect(),
            apply: None,
            approve: false,
            context: Vec::new(),
            save: None,
            skills: Vec::new(),
            mcp: task.mcp.clone(),
            agent: None,
            team: None,
            timeout: None,
            issue: None,
            m365: None,
            workiq: Vec::new(),
        };
        // The task as the plan step it amounts to.
        let mut text: String = claims
            .iter()
            .map(|c| format!("claim: {} {}\n", c.id, c.text))
            .collect();
        text += &format!(
            "task @{peer}{} using {}",
            task.share
                .as_ref()
                .map(|s| format!(":{s}"))
                .unwrap_or_default(),
            task.provider
        );
        if !task.checks.is_empty() {
            text += &format!(" verify={} claims=T-1", task.checks.join(","));
        }
        if let Some(n) = task.attempts {
            text += &format!(" attempts={n}");
        }
        text += &format!(" {prompt}");
        let plan = Plan {
            claims,
            steps: vec![step.clone()],
            track: None,
        };
        let state = StepState::from_delivered(peer.to_owned(), &step, delivered);
        let states = BTreeMap::from([("task".to_owned(), state)]);
        Self::new(boss, boss_id, &text, &plan, &states, started_unix)
    }

    /// Whether a task delegated on its own brought back something to keep as an
    /// evidence run: it asked for checks, or its peer committed work.
    pub fn worth_keeping(task: &crate::llm::LlmTask, delivered: &crate::agent::Delivered) -> bool {
        !task.checks.is_empty()
            || delivered
                .evidence
                .as_ref()
                .and_then(|e| e.git.as_ref())
                .is_some_and(|g| g.commit.is_some())
    }

    pub fn verdict_line(&self) -> String {
        let s = self.summary();
        if s.claims == 0 {
            format!(
                "NO CLAIMS: {}/{} steps done (add claim: lines to prove something)",
                s.steps_done, s.steps
            )
        } else {
            format!(
                "{}: {}/{} claims proven, {}/{} steps done",
                if s.passed { "PASS" } else { "FAIL" },
                s.proven,
                s.claims,
                s.steps_done,
                s.steps
            )
        }
    }

    /// The verdict as the peer whose team step this run did reads it: without the
    /// tip to the plan's author (the lead's user) about claims.
    pub fn team_verdict(&self) -> String {
        let s = self.summary();
        if s.claims == 0 {
            format!("{}/{} steps done, no claims", s.steps_done, s.steps)
        } else {
            self.verdict_line()
        }
    }

    /// Re-checks every step's signed evidence: the signature and output (and patch
    /// file), and that it belongs to this run: requested by the boss, signed by the
    /// key of the peer the step ran on, of the right kind, produced during the run,
    /// and not reused by another step.
    pub fn verify(&self, dir: Option<&Path>) -> Vec<(String, Result<String>)> {
        let mut seen = std::collections::HashSet::new();
        self.steps
            .iter()
            .filter_map(|s| {
                let (evidence, output, peer) = match &s.state {
                    StepState::Done {
                        evidence: Some(e),
                        output,
                        peer,
                        ..
                    } => (e, output, Some(peer)),
                    StepState::Failed {
                        evidence: Some(e),
                        error,
                        peer,
                        ..
                    } => (e, error, peer.as_ref()),
                    _ => return None,
                };
                let check = self
                    .belongs(s, evidence, peer)
                    .and_then(|()| {
                        if seen.insert(evidence.signature.clone()) {
                            Ok(())
                        } else {
                            Err(anyhow::anyhow!("the same evidence is used by another step"))
                        }
                    })
                    .and_then(|()| verify_step(evidence, output, &s.step.id, dir));
                Some((s.step.id.clone(), check))
            })
            .collect()
    }

    fn belongs(&self, s: &StepRecord, e: &Evidence, peer: Option<&String>) -> Result<()> {
        if e.requester_id != self.boss_id {
            bail!(
                "requested by {}, not by this run's boss",
                short(&e.requester_id)
            );
        }
        let kind = if s.step.approve {
            EvidenceKind::Approval
        } else {
            EvidenceKind::Task
        };
        if e.kind != kind {
            bail!("the evidence is for {:?}, not {kind:?}", e.kind);
        }
        if let Some(expected) = peer.and_then(|p| self.peer_ids.get(p))
            && &e.signer_id != expected
        {
            bail!(
                "signed by {}, but {} is {}",
                short(&e.signer_id),
                peer.map(String::as_str).unwrap_or_default(),
                short(expected)
            );
        }
        if e.finished_unix + CLOCK_SKEW < self.started_unix
            || e.finished_unix > self.finished_unix + CLOCK_SKEW
        {
            bail!("produced at {} UTC, outside this run", utc(e.finished_unix));
        }
        Ok(())
    }

    /// A copy whose `verified` flags come from re-checking the evidence now (not from
    /// what was stored), with the check results; verdicts of the copy are trustworthy.
    pub fn reverified(&self, dir: Option<&Path>) -> (Self, Vec<(String, Result<String>)>) {
        let checks = self.verify(dir);
        let mut copy = self.clone();
        for s in &mut copy.steps {
            let ok = checks.iter().any(|(id, r)| id == &s.step.id && r.is_ok());
            match &mut s.state {
                StepState::Done { verified, .. } | StepState::Failed { verified, .. } => {
                    *verified = ok
                }
                _ => {}
            }
        }
        (copy, checks)
    }

    pub fn render_markdown(&self) -> String {
        let mut md = format!("# Evidence run {}\n\n", self.id);
        md += &format!(
            "- **Verdict: {}**\n- Run by {} (`{}`)\n- Started {} UTC, finished {} UTC\n\n",
            self.verdict_line(),
            self.boss,
            short(&self.boss_id),
            utc(self.started_unix),
            utc(self.finished_unix)
        );
        md += "## Claims\n\n";
        let results = self.claim_results();
        if results.is_empty() {
            md += "No claims. Add `claim: ID what must be true` lines and `claims=ID` on the steps that prove them.\n\n";
        } else {
            md += "| Claim | Statement | Verdict | Steps |\n| --- | --- | --- | --- |\n";
            for r in &results {
                md += &format!(
                    "| {} | {} | {} | {} |\n",
                    r.claim.id,
                    r.claim.text.replace('|', "\\|"),
                    r.verdict.label(),
                    if r.steps.is_empty() {
                        "none".to_owned()
                    } else {
                        r.steps.join(", ")
                    }
                );
            }
            md += "\n";
        }
        md += "## Steps\n";
        for s in &self.steps {
            md += &self.render_step(s);
        }
        md += &format!("\n## Plan\n\n```text\n{}\n```\n", self.plan_text.trim_end());
        md
    }

    fn render_step(&self, s: &StepRecord) -> String {
        let step = &s.step;
        let kind = if step.approve {
            "approval".to_owned()
        } else if step.team.is_some() {
            "team".to_owned()
        } else {
            step.provider.to_string()
        };
        let (status, peer) = match &s.state {
            StepState::Waiting => ("waiting", None),
            StepState::Running { peer, .. } => ("running", Some(peer.clone())),
            StepState::Done { peer, .. } => ("done", Some(peer.clone())),
            StepState::Failed { peer, .. } => ("failed", peer.clone()),
            StepState::Skipped { .. } => ("skipped", None),
        };
        let mut md = format!(
            "\n### {} ({kind} → {}{}) {}: {status}\n\n",
            step.id,
            step.target,
            step.share
                .as_ref()
                .map(|f| format!(":{f}"))
                .unwrap_or_default(),
            peer.map(|p| format!("on {p}")).unwrap_or_default()
        );
        if !step.after.is_empty() {
            md += &format!("- After: {}\n", step.after.join(", "));
        }
        if !step.claims.is_empty() {
            md += &format!(
                "- Claims: {} ({})\n",
                step.claims.join(", "),
                step_verdict(s).label()
            );
        }
        if let Some(apply) = &step.apply {
            md += &format!("- Applied the commit of step {apply}\n");
        }
        md += &format!("- Task: {}\n", step.prompt);
        if let Some(model) = s.state.model() {
            md += &format!("- Model: `{}`\n", crate::llm::clean_model(model));
        }
        let (evidence, verified, text) = match &s.state {
            StepState::Done {
                evidence,
                verified,
                output,
                ..
            } => (evidence.as_ref(), *verified, Some(output.as_str())),
            StepState::Failed {
                evidence,
                verified,
                error,
                ..
            } => (evidence.as_ref(), *verified, Some(error.as_str())),
            StepState::Skipped { reason } => {
                md += &format!("- Skipped: {reason}\n");
                (None, false, None)
            }
            _ => (None, false, None),
        };
        if let Some(e) = evidence {
            md += &format!(
                "- Evidence signed by {} (`{}`): {}\n",
                e.signer,
                short(&e.signer_id),
                if verified {
                    "signature ✓ verified"
                } else {
                    "signature ✗ NOT valid"
                }
            );
            if let Some(run) = &e.team {
                md += &format!(
                    "- Team run `{}` by {} (its plan, with every step's evidence, is in the signed record): {}\n",
                    run.id,
                    run.boss,
                    run.team_verdict()
                );
                for t in &run.steps {
                    md += &format!("  - {}\n", team_line(t));
                }
            } else if e.kind == EvidenceKind::Task {
                md += &format!(
                    "- Model attempts: {}; checks run by the worker's runtime:\n",
                    e.attempts
                );
                if e.checks.is_empty() {
                    md += "  - none requested (the result rests on the model's word)\n";
                }
                for c in &e.checks {
                    md += &format!(
                        "  - {} `{}` (`{}`): {} in {:.1}s\n",
                        if c.passed { "✅" } else { "❌" },
                        c.name,
                        c.command,
                        c.exit_code
                            .map(|code| format!("exit {code}"))
                            .unwrap_or_else(|| "no exit code".into()),
                        c.millis as f64 / 1000.0
                    );
                }
            }
            if let Some(git) = &e.git {
                match &git.commit {
                    Some(commit) => {
                        md += &format!(
                            "- Commit `{commit}` on branch `{}` (from `{}`), files: {}; patch: `{}.patch`\n",
                            git.branch,
                            short(&git.base),
                            git.files.join(", "),
                            step.id
                        )
                    }
                    None => md += &format!("- No changes committed (branch `{}`)\n", git.branch),
                }
            }
            for c in e.checks.iter().filter(|c| !c.passed) {
                md += &format!(
                    "\n<details><summary>Output of failed check {}</summary>\n\n```text\n{}\n```\n</details>\n",
                    c.name,
                    c.output.trim_end()
                );
            }
        } else if matches!(s.state, StepState::Done { .. }) {
            md += "- No evidence (the peer runs an older version)\n";
        }
        if let Some(text) = text {
            md += &format!(
                "\n<details><summary>Result</summary>\n\n{}\n\n</details>\n",
                text.trim_end()
            );
        }
        md
    }

    /// Saves `run.json`, `report.md` and step patches in `runs_dir/<id>/`.
    pub fn save(&self, runs_dir: &Path) -> Result<PathBuf> {
        let dir = runs_dir.join(&self.id);
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        for s in &self.steps {
            if let StepState::Done {
                patch: Some(patch), ..
            } = &s.state
            {
                std::fs::write(dir.join(format!("{}.patch", s.step.id)), patch)?;
            }
        }
        std::fs::write(dir.join("run.json"), serde_json::to_vec_pretty(self)?)?;
        let report = dir.join("report.md");
        std::fs::write(&report, self.render_markdown())?;
        Ok(report)
    }

    pub fn load(runs_dir: &Path, id: &str) -> Result<Self> {
        crate::profile::validate_name(id).context("invalid run id")?;
        let path = runs_dir.join(id).join("run.json");
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("there is no run {id} ({})", path.display()))?;
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    /// Saved runs, newest first.
    pub fn list(runs_dir: &Path) -> Result<Vec<RunSummary>> {
        let mut runs = Vec::new();
        let Ok(entries) = std::fs::read_dir(runs_dir) else {
            return Ok(runs);
        };
        for entry in entries.flatten() {
            let id = entry.file_name().to_string_lossy().into_owned();
            if let Ok(run) = Self::load(runs_dir, &id) {
                runs.push(run.summary());
            }
        }
        runs.sort_by(|a, b| b.id.cmp(&a.id));
        Ok(runs)
    }

    /// The newest saved run with a step backed by the evidence record signed
    /// `signature`, re-verified (its verdicts come from checking the evidence now).
    pub fn with_evidence(runs_dir: &Path, signature: &str) -> Option<Self> {
        let mut ids: Vec<String> = std::fs::read_dir(runs_dir)
            .ok()?
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        ids.sort_unstable_by(|a, b| b.cmp(a));
        ids.iter()
            .filter_map(|id| Self::load(runs_dir, id).ok())
            .find(|run| {
                run.steps.iter().any(|s| match &s.state {
                    StepState::Done {
                        evidence: Some(e), ..
                    }
                    | StepState::Failed {
                        evidence: Some(e), ..
                    } => e.signature == signature,
                    _ => false,
                })
            })
            .map(|run| {
                let dir = runs_dir.join(&run.id);
                run.reverified(Some(&dir)).0
            })
    }
}

fn verify_step(
    evidence: &Evidence,
    output: &str,
    step: &str,
    dir: Option<&Path>,
) -> Result<String> {
    evidence.verify_self(output)?;
    if let Some(run) = &evidence.team {
        verify_team(run, &evidence.signer_id)?;
        return Ok(format!(
            "signed by {} ({}), output intact; its team run {} checked through ({} step{})",
            evidence.signer,
            short(&evidence.signer_id),
            run.id,
            run.steps.len(),
            if run.steps.len() == 1 { "" } else { "s" }
        ));
    }
    if let (Some(dir), Some(hash)) = (
        dir,
        evidence.git.as_ref().and_then(|g| g.patch_sha256.as_ref()),
    ) {
        let path = dir.join(format!("{step}.patch"));
        if let Ok(patch) = std::fs::read(&path) {
            if &sha256_hex(&patch) != hash {
                bail!("{} doesn't match the signed patch hash", path.display());
            }
            return Ok(format!(
                "signed by {} ({}), output and patch intact",
                evidence.signer,
                short(&evidence.signer_id)
            ));
        }
    }
    Ok(format!(
        "signed by {} ({}), output intact",
        evidence.signer,
        short(&evidence.signer_id)
    ))
}

/// The first 10 characters (never splits a character).
fn short(id: &str) -> String {
    id.chars().take(10).collect()
}

/// A team run's step in a line of the report: where it ran, how it ended, whether
/// its evidence is signed, and its own team run, if it is a team step too.
pub(crate) fn team_line(s: &StepRecord) -> String {
    let (status, peer, signed, nested) = match &s.state {
        StepState::Done {
            peer,
            evidence,
            verified,
            ..
        } => (
            "done",
            Some(peer.as_str()),
            *verified,
            evidence.as_ref().and_then(|e| e.team.as_ref()),
        ),
        StepState::Failed {
            peer,
            evidence,
            verified,
            ..
        } => (
            "failed",
            peer.as_deref(),
            *verified,
            evidence.as_ref().and_then(|e| e.team.as_ref()),
        ),
        StepState::Skipped { .. } => ("skipped", None, false, None),
        _ => ("not finished", None, false, None),
    };
    let mut line = format!(
        "{} on {}: {status}{}",
        s.step.id,
        peer.map_or_else(|| s.step.target.to_string(), str::to_owned),
        if signed { ", signed ✓" } else { "" }
    );
    match &s.state {
        StepState::Failed { error, .. } => {
            line += &format!(" ({})", clip(error.lines().next().unwrap_or_default(), 200))
        }
        StepState::Skipped { reason } => line += &format!(" ({reason})"),
        _ => {}
    }
    if let Some(run) = nested {
        line += &format!(" (team run {}: {})", run.id, run.team_verdict());
    }
    line
}

/// `text` on one line, each run of whitespace a single space.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `text` cut to at most `max` characters, ending with an ellipsis if it was longer.
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
    use crate::evidence::CheckResult;
    use iroh::SecretKey;
    use std::collections::BTreeMap;

    fn task_evidence(key: &SecretKey, output: &str, passed: bool) -> Evidence {
        let mut e = Evidence::new(EvidenceKind::Task, "worker", key.public(), key.public(), 1);
        e.attempts = 1;
        e.checks.push(CheckResult {
            name: "test".into(),
            command: "cargo test".into(),
            exit_code: Some(if passed { 0 } else { 101 }),
            passed,
            millis: 1500,
            output: if passed { "ok" } else { "boom" }.into(),
        });
        e.sign(output, key)
    }

    fn done(output: &str, evidence: Option<Evidence>, verified: bool) -> StepState {
        StepState::Done {
            peer: "worker".into(),
            output: output.into(),
            evidence,
            verified,
            patch: None,
            model: None,
        }
    }

    #[test]
    fn claims_are_proven_only_by_verified_runtime_evidence() {
        let text = "claim: C-1 the parser handles empty input\n\
                    claim: C-2 the docs describe it\n\
                    claim: C-3 nobody signs it off\n\
                    impl @worker:repo verify=test attempts=2 claims=C-1 Fix the parser\n\
                    docs after impl @worker claims=C-2 Write the docs\n\
                    signoff after impl approve @me claims=C-3 Review the change";
        let plan = Plan::parse(text).unwrap();
        let key = SecretKey::generate();
        let mut states = BTreeMap::new();
        states.insert(
            "impl".to_owned(),
            done("fixed", Some(task_evidence(&key, "fixed", true)), true),
        );
        // A model-only result proves nothing.
        states.insert("docs".to_owned(), done("written", None, false));
        states.insert(
            "signoff".to_owned(),
            StepState::failed(Some("you".into()), "Rejected by boss".into()),
        );
        let run = RunRecord::new("boss", &key.public().to_string(), text, &plan, &states, 0);
        let verdicts: Vec<Verdict> = run.claim_results().iter().map(|r| r.verdict).collect();
        assert_eq!(
            verdicts,
            vec![Verdict::Proven, Verdict::Unverified, Verdict::Failed]
        );
        assert!(!run.passed());
        assert!(run.verdict_line().starts_with("FAIL: 1/3 claims proven"));

        // Failing checks or an unverified signature don't prove a claim either.
        states.insert(
            "impl".to_owned(),
            done("fixed", Some(task_evidence(&key, "fixed", false)), true),
        );
        let run = RunRecord::new("boss", "x", text, &plan, &states, 0);
        assert_eq!(run.claim_results()[0].verdict, Verdict::Unverified);
        states.insert(
            "impl".to_owned(),
            done("fixed", Some(task_evidence(&key, "fixed", true)), false),
        );
        let run = RunRecord::new("boss", "x", text, &plan, &states, 0);
        assert_eq!(run.claim_results()[0].verdict, Verdict::Unverified);
    }

    #[test]
    fn runs_are_saved_listed_reported_and_reverified() {
        let text = "claim: C-1 tests pass\nimpl @worker verify=test claims=C-1 Fix it";
        let plan = Plan::parse(text).unwrap();
        let key = SecretKey::generate();
        let mut states = BTreeMap::new();
        states.insert(
            "impl".to_owned(),
            done("fixed", Some(task_evidence(&key, "fixed", true)), true),
        );
        let mut run = RunRecord::new("boss", &key.public().to_string(), text, &plan, &states, 0);
        run.peer_ids
            .insert("worker".into(), key.public().to_string());
        assert!(run.passed());
        let dir = tempfile::tempdir().unwrap();
        let report = run.save(dir.path()).unwrap();
        let md = std::fs::read_to_string(&report).unwrap();
        assert!(md.contains("PASS: 1/1 claims proven"), "{md}");
        assert!(
            md.contains("| C-1 | tests pass | ✅ proven | impl |"),
            "{md}"
        );
        assert!(md.contains("signature ✓ verified"));
        assert!(
            md.contains("✅ `test` (`cargo test`): exit 0 in 1.5s"),
            "{md}"
        );

        let listed = RunRecord::list(dir.path()).unwrap();
        assert_eq!(listed.len(), 1);
        assert!(listed[0].passed && listed[0].id == run.id);
        let loaded = RunRecord::load(dir.path(), &run.id).unwrap();
        assert_eq!(loaded, run);
        let checks = loaded.verify(Some(&dir.path().join(&run.id)));
        assert!(checks[0].1.is_ok(), "{checks:?}");

        // Someone edits the stored result: re-verification catches it.
        let mut tampered = loaded.clone();
        if let StepState::Done { output, .. } = &mut tampered.steps[0].state {
            *output = "fixed (trust me)".into();
        }
        assert!(tampered.verify(None)[0].1.is_err());

        // A boss can't vouch with evidence that isn't the step's: a key it made up
        // signing as "worker", evidence requested by someone else, or old evidence.
        let forger = SecretKey::generate();
        let mut forged = loaded.clone();
        if let StepState::Done { evidence, .. } = &mut forged.steps[0].state {
            *evidence = Some(task_evidence(&forger, "fixed", true));
        }
        forged.boss_id = forger.public().to_string();
        let err = forged.verify(None)[0].1.as_ref().unwrap_err().to_string();
        assert!(err.contains("but worker is"), "{err}");
        let (rechecked, _) = forged.reverified(None);
        assert!(forged.passed(), "the stored flags claim a pass");
        assert!(!rechecked.passed(), "re-verification decides the verdict");

        let mut foreign = loaded.clone();
        foreign.boss_id = forger.public().to_string();
        let err = foreign.verify(None)[0].1.as_ref().unwrap_err().to_string();
        assert!(err.contains("not by this run's boss"), "{err}");
        let mut later = loaded.clone();
        later.started_unix = later.finished_unix + 3_600;
        later.finished_unix += 3_600;
        let err = later.verify(None)[0].1.as_ref().unwrap_err().to_string();
        assert!(err.contains("outside this run"), "{err}");
    }

    #[test]
    fn tasks_on_their_own_keep_what_proves_them_as_one_step_runs() {
        use crate::agent::Delivered;
        use crate::llm::{LlmTask, Provider};
        let key = SecretKey::generate();
        let task = LlmTask::new(Provider::Copilot, "Fix the\nparser")
            .in_share("app")
            .with_checks(vec!["test".into()], Some(2));
        let delivered = Delivered {
            ok: true,
            output: "fixed".into(),
            evidence: Some(task_evidence(&key, "fixed", true)),
            patch: None,
            verified: true,
            refused: false,
            model: Some("claude-sonnet-5.5".into()),
        };
        assert!(RunRecord::worth_keeping(&task, &delivered));
        let mut run = RunRecord::for_task(
            "boss",
            &key.public().to_string(),
            "worker",
            &task,
            delivered,
            0,
        );
        run.peer_ids
            .insert("worker".into(), key.public().to_string());
        assert!(run.passed(), "{}", run.verdict_line());
        // The record and its report name the model that did the work.
        assert!(
            matches!(&run.steps[0].state, StepState::Done { model: Some(m), .. } if m == "claude-sonnet-5.5")
        );
        assert!(
            run.render_markdown()
                .contains("- Model: `claude-sonnet-5.5`"),
            "{}",
            run.render_markdown()
        );
        assert_eq!(run.title(), "test passes after: Fix the parser");
        assert_eq!(run.summary().title, run.title());
        assert_eq!(
            run.plan_text,
            "claim: T-1 test passes after: Fix the parser\n\
             task @worker:app using copilot verify=test claims=T-1 attempts=2 Fix the parser"
        );
        assert!(run.verify(None)[0].1.is_ok());
        // Only keys that signed something are signers, by their allowlist name.
        run.peer_ids
            .insert("idle".into(), SecretKey::generate().public().to_string());
        assert_eq!(
            run.signers(),
            [("worker".to_owned(), key.public().to_string())]
        );

        // A question answered without checks or a commit isn't kept.
        let question = LlmTask::new(Provider::Copilot, "What does it do?");
        let answer = Delivered {
            ok: true,
            output: "It parses.".into(),
            evidence: Some(
                Evidence::new(EvidenceKind::Task, "worker", key.public(), key.public(), 2)
                    .sign("It parses.", &key),
            ),
            patch: None,
            verified: true,
            refused: false,
            model: None,
        };
        assert!(!RunRecord::worth_keeping(&question, &answer));
        // Without checks, a run's title is its task.
        let plan = Plan::parse("a @worker Summarize   the README").unwrap();
        let run = RunRecord::new("boss", "x", "", &plan, &BTreeMap::new(), 0);
        assert_eq!(run.title(), "Summarize the README");
    }

    #[test]
    fn utc_formats_dates() {
        assert_eq!(utc(0), "1970-01-01 00:00:00");
        assert_eq!(utc(951_782_400), "2000-02-29 00:00:00");
        assert_eq!(utc(1_794_786_000), "2026-11-15 23:40:00");
    }
}
