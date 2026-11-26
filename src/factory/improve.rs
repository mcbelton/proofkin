//! Improvement (docs/agent-factory.md, section 12): the owner's model proposes a better
//! version of an agent from what failed: its failing eval cases with their outputs
//! (redacted and capped), the outcomes of its deployments' failed tasks (from their workers'
//! audits, never their text) and the owner's note. The proposal is a **new draft version**:
//! the blueprint of the release that runs, with the model's instructions and skills, and any
//! requirement it adds; its requirements (its output contract) and every case it had kept
//! exactly as they were, and a regression case for each failing case (the model's, or else a
//! copy of the case). It is shown as a diff next to the active release and evaluated next on
//! that suite.
//!
//! A proposal asks for nothing more than the release it improves: its capabilities, model
//! policy, memory, budgets, deploy folders and approvers stay the release's. And it changes
//! nothing that runs: only a release, through the gate and its sign-offs, does (TM-13).

use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::audit::{AuditLog, CheckTally, Outcome};
use crate::factory::blueprint::{Blueprint, Catalog, Problem, from_value};
use crate::factory::draft;
use crate::factory::release;
use crate::factory::report::{EvalState, Evaluation};
use crate::factory::store::{
    Draft, FactoryStore, Lifecycle, Provenance, now, read_json, write_json,
};
use crate::factory::{conflict, not_found, runner};
use crate::llm::{Executor, Provider};
use crate::profile::Profile;

/// The longest note, in characters.
pub const MAX_NOTE: usize = 2000;
/// How much of a failing run's output the model is shown.
const OUTPUT_CHARS: usize = 1_500;
/// The most failed tasks it is shown, and how far back they go.
const MAX_TASKS: usize = 20;
const TASK_DAYS: i64 = 30;
/// A regression case is named after the case it comes from: `E2-regression`.
pub const REGRESSION: &str = "-regression";

/// A failing run, as the model is shown it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Failure {
    pub case: String,
    /// Why it failed: the expectation it missed, or its error.
    pub why: String,
    /// What the agent answered, redacted and capped (none once outputs are pruned).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
}

/// A failed task of one of its deployments, as its worker's audit keeps it: no text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailedTask {
    pub at: String,
    /// The release it ran as (`name@version`), and its worker.
    pub agent: String,
    pub worker: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempts: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checks: Option<CheckTally>,
    pub secs: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// What a proposal is made from.
#[derive(Debug, Clone)]
pub struct Material {
    pub name: String,
    /// The blueprint it improves (the active release's, else the draft's), and which it is.
    pub basis: Blueprint,
    pub basis_of: String,
    /// The evaluation its failures come from.
    pub evaluation: Option<String>,
    /// The cases to improve: those named, else every one that failed.
    pub cases: Vec<String>,
    pub failures: Vec<Failure>,
    pub tasks: Vec<FailedTask>,
    pub note: Option<String>,
}

/// A proposal, as kept next to the agent (`agents/<name>/proposals/<id>.json`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Proposal {
    pub id: String,
    pub at: String,
    pub by: String,
    /// What it improves (`release 1.2.0`, `the draft`) and the evaluation its failures
    /// come from.
    pub basis: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evaluation: Option<String>,
    pub cases: Vec<String>,
    pub failures: Vec<Failure>,
    pub tasks: Vec<FailedTask>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The regression cases it adds.
    pub regression: Vec<String>,
    /// What it changes from its basis, a line each, and its instructions line by line.
    pub diff: Vec<String>,
    pub instructions: Vec<String>,
    /// Errors and warnings its draft has.
    pub errors: usize,
    pub warnings: usize,
    /// The model that wrote it.
    pub provenance: Provenance,
    /// The draft it replaced, if there was one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_draft: Option<Draft>,
    /// Its build's hash (when its draft builds): its evaluations are those of this build.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<String>,
}

fn clip(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_owned();
    }
    let kept: String = text.chars().take(max).collect();
    format!("{kept}\n[... {} more characters]", count - max)
}

/// What a proposal for agent `name` of `owner`'s would be made from (see [`material_in`]),
/// with its deployments' failed tasks.
pub fn material(
    owner: &Profile,
    name: &str,
    cases: &[String],
    note: Option<&str>,
) -> Result<Material> {
    let store = FactoryStore::of(owner);
    let state = if crate::profile::validate_name(name).is_ok() {
        store.state(name)?
    } else {
        None
    };
    let tasks = failed_tasks(owner, name, state.as_ref());
    material_in(&store, name, cases, note, tasks)
}

/// What a proposal for agent `name` in `store` would be made from: its active release's
/// blueprint (else its draft's), the failing runs of `cases` (none: every failing case) in
/// the last evaluation of what runs, its deployments' failed `tasks`, and `note`. Refused
/// when there is nothing to learn from.
pub fn material_in(
    store: &FactoryStore,
    name: &str,
    cases: &[String],
    note: Option<&str>,
    tasks: Vec<FailedTask>,
) -> Result<Material> {
    if crate::profile::validate_name(name).is_err() {
        return Err(not_found(format!("the factory has no agent '{name}'")));
    }
    let state = store.state(name)?;
    let draft = store.draft(name)?;
    if state.is_none() && draft.is_none() {
        return Err(not_found(format!("the factory has no agent '{name}'")));
    }
    if let Some(s) = &state {
        if s.state == Lifecycle::Retired {
            return Err(conflict(format!(
                "{name} is retired: a retired agent isn't improved"
            )));
        }
        // A version made here would be neither its publisher's nor take their next one.
        if let Some(o) = &s.imported {
            return Err(conflict(format!(
                "{name} is an import: only its publisher improves it; tell {} what failed, and import the version they publish next",
                o.publisher
            )));
        }
    }
    let note = note.map(str::trim).filter(|n| !n.is_empty());
    if note.is_some_and(|n| n.chars().count() > MAX_NOTE) {
        return Err(conflict(format!("a note is at most {MAX_NOTE} characters")));
    }
    let active = state.as_ref().and_then(|s| s.active_version.clone());
    let (basis, basis_of, build) = match &active {
        Some(version) => (
            release::blueprint_of(store, name, version)?,
            format!("release {version}"),
            Some(release::load(store, name, version)?.manifest.build),
        ),
        None => {
            let draft = draft
                .ok_or_else(|| conflict(format!("{name} has neither a release nor a draft")))?;
            let basis = from_value(&draft.blueprint).map_err(|p| {
                conflict(format!(
                    "its draft isn't a valid blueprint ({p}): fix it, then improve it"
                ))
            })?;
            (basis, "the draft".to_owned(), None)
        }
    };
    let known: Vec<&str> = basis.evals.cases.iter().map(|c| c.id.as_str()).collect();
    if let Some(unknown) = cases.iter().find(|c| !known.contains(&c.as_str())) {
        return Err(conflict(format!(
            "{name} has no case {unknown}: its cases are {}",
            known.join(", ")
        )));
    }
    // The last evaluation of what runs (its build): its release's, its canary or drift watch.
    let evaluation: Option<Evaluation> = store
        .evaluations(name)?
        .into_iter()
        .filter(|e| e.state != EvalState::Running)
        .find(|e| build.as_ref().is_none_or(|b| &e.build == b));
    let mut failures: Vec<Failure> = Vec::new();
    if let Some(e) = &evaluation {
        for run in e.runs.iter().filter(|r| !r.passed) {
            if failures.iter().any(|f| f.case == run.case)
                || (!cases.is_empty() && !cases.contains(&run.case))
            {
                continue;
            }
            let why = run
                .error
                .clone()
                .or_else(|| {
                    run.grades
                        .iter()
                        .find(|g| g.passed == Some(false))
                        .map(|g| g.note.clone())
                })
                .unwrap_or_else(|| "it didn't pass".into());
            failures.push(Failure {
                case: run.case.clone(),
                why: crate::redact::redact(&why).0,
                output: run
                    .output
                    .as_deref()
                    .map(|o| clip(&crate::redact::redact(runner::answer_of(o)).0, OUTPUT_CHARS)),
            });
        }
    }
    if failures.is_empty() && tasks.is_empty() && note.is_none() {
        return Err(conflict(format!(
            "{name} has no failing case or failed task to improve on: evaluate it, or say what to improve with a note"
        )));
    }
    let cases = if cases.is_empty() {
        failures.iter().map(|f| f.case.clone()).collect()
    } else {
        cases.to_vec()
    };
    Ok(Material {
        name: name.to_owned(),
        basis,
        basis_of,
        evaluation: evaluation.map(|e| e.id),
        cases,
        failures,
        tasks,
        note: note.map(str::to_owned),
    })
}

/// The tasks agent `name`'s deployments failed in the last [`TASK_DAYS`] days, newest first
/// (at most [`MAX_TASKS`]): from their workers' audits, outcomes only.
fn failed_tasks(
    owner: &Profile,
    name: &str,
    state: Option<&crate::factory::store::AgentState>,
) -> Vec<FailedTask> {
    let since = (chrono::Utc::now() - chrono::Duration::days(TASK_DAYS)).timestamp_millis() as u64;
    let prefix = format!("{name}@");
    let mut tasks: Vec<(u64, FailedTask)> = Vec::new();
    for d in state.map(|s| s.deployments.as_slice()).unwrap_or_default() {
        let Ok(worker) = crate::workers::worker_of(owner, &d.worker) else {
            continue;
        };
        for e in AuditLog::read(&worker.audit_path(), usize::MAX).unwrap_or_default() {
            if e.outcome != Outcome::Failed || e.ts_ms < since {
                continue;
            }
            let Some(agent) = e.agent.clone().filter(|a| a.starts_with(&prefix)) else {
                continue;
            };
            tasks.push((
                e.ts_ms,
                FailedTask {
                    at: chrono::DateTime::from_timestamp_millis(e.ts_ms as i64)
                        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
                        .unwrap_or_default(),
                    agent,
                    worker: d.worker.clone(),
                    attempts: e.attempts,
                    checks: e.checks,
                    secs: e.duration_ms / 1000,
                    model: e.model.clone(),
                },
            ));
        }
    }
    tasks.sort_by_key(|t| std::cmp::Reverse(t.0));
    tasks.into_iter().take(MAX_TASKS).map(|(_, t)| t).collect()
}

const RULES: &str = "\
## What you may change

- `instructions`: the agent's procedure, rules and the form of its answer. Most fixes go here.
- `skills`, from the list above. Keep every requirement as it is: they are what the agent promises. If what failed shows one is missing, add it (R2, R3...) with a case.
- `evals.cases`: keep every case exactly as it is. For each failing case, add one regression case with the id `<case>-regression` (like `E2-regression`) that tests the same requirement from another angle, with a self-contained input and expectations that decide by themselves (contains, not-contains, regex, json-schema, max-words, files-changed or check). Use only the fixtures the failing cases use.

Everything else stays as it is (title, purpose, capabilities, model, memory, budgets, deploy, approvers): an improvement asks for nothing more than the release it improves.
";

/// The prompt that asks the owner's model for a proposal from `m`.
fn prompt(m: &Material, catalog: &Catalog) -> String {
    let basis = serde_json::to_string_pretty(&m.basis).unwrap_or_default();
    let mut failed = String::new();
    for f in &m.failures {
        let case = m.basis.evals.cases.iter().find(|c| c.id == f.case);
        match case {
            Some(c) => {
                failed += &format!(
                    "### Case {} (requirement {})\n\nInput:\n{}\n\nExpected: {}\n\nWhy it failed: {}\n\n",
                    c.id,
                    c.requirement,
                    c.input,
                    serde_json::to_string(&c.expect).unwrap_or_default(),
                    f.why
                )
            }
            None => {
                failed += &format!(
                    "### Built-in attack {}\n\nThe agent didn't resist it: {}\n\n",
                    f.case, f.why
                )
            }
        }
        if let Some(output) = &f.output {
            failed += &format!("Its answer:\n```text\n{output}\n```\n\n");
        }
    }
    for named in m
        .cases
        .iter()
        .filter(|c| !m.failures.iter().any(|f| &f.case == *c))
    {
        failed += &format!("### Case {named}: the owner asks to improve it, though it passed\n\n");
    }
    let mut tasks = String::new();
    for t in &m.tasks {
        tasks += &format!(
            "- {} on {} ({}): failed{}{}, {}s{}\n",
            t.at,
            t.worker,
            t.agent,
            t.attempts
                .map(|a| format!(" after {a} attempt{}", if a == 1 { "" } else { "s" }))
                .unwrap_or_default(),
            t.checks
                .map(|c| format!(", checks {} of {} passed", c.passed, c.total))
                .unwrap_or_default(),
            t.secs,
            t.model
                .as_deref()
                .map(|m| format!(", model {m}"))
                .unwrap_or_default()
        );
    }
    let mut text = format!(
        "You improve an agent for Proofkin. Below are its blueprint and what it failed. Write \
         its whole blueprint again, improved so that what failed passes. Don't do the agent's \
         work, and don't read or change files: everything you need is here.\n\n\
         ## Its blueprint ({})\n\n```json\n{basis}\n```\n\n",
        m.basis_of
    );
    if !failed.is_empty() {
        text += &format!(
            "## What failed{}\n\n{}\n",
            m.evaluation
                .as_deref()
                .map(|e| format!(" in evaluation {e}"))
                .unwrap_or_default(),
            crate::boundary::wrap("the failing cases and the agent's answers", &failed)
        );
    }
    if !tasks.is_empty() {
        text += &format!(
            "## Tasks its deployments failed (their outcomes only: their text is never kept)\n\n{tasks}\n"
        );
    }
    if let Some(note) = &m.note {
        text += &format!(
            "## The owner's note\n\n{}\n\n",
            crate::redact::redact(note).0
        );
    }
    text += &format!(
        "## What the blueprint may name\n\n{}\n\n{RULES}\nAnswer with the whole blueprint in one ```json block.",
        crate::boundary::wrap(
            "this profile's skills, knowledge, folders and models",
            &draft::catalog_text(catalog)
        )
    );
    crate::boundary::guard(&text)
}

/// The proposal the model's `answer` makes of `basis` (the blueprint of the release it
/// improves): the answer's instructions and skills; every requirement and case of `basis` as
/// it was, then the answer's new ones; a regression case for each of the `failing` cases it
/// has none for (a copy of the case). Everything else is `basis`'s.
pub fn merge(basis: &Blueprint, answer: &Value, failing: &[String]) -> Value {
    let mut merged = serde_json::to_value(basis).unwrap_or(Value::Null);
    let Some(object) = merged.as_object_mut() else {
        return merged;
    };
    if let Some(instructions) = answer
        .get("instructions")
        .and_then(Value::as_str)
        .filter(|i| !i.trim().is_empty())
    {
        object.insert("instructions".into(), json!(instructions));
    }
    if let Some(skills) = answer.get("skills").filter(|s| s.is_array()) {
        object.insert("skills".into(), skills.clone());
    }
    let mut requirements: Vec<Value> = basis
        .requirements
        .iter()
        .map(|r| serde_json::to_value(r).unwrap_or(Value::Null))
        .collect();
    for asked in answer
        .get("requirements")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(id) = asked.get("id").and_then(Value::as_str) else {
            continue;
        };
        // What it promises stays: a new requirement only.
        if !requirements
            .iter()
            .any(|r| r.get("id").and_then(Value::as_str) == Some(id))
        {
            requirements.push(asked.clone());
        }
    }
    object.insert("requirements".into(), json!(requirements));
    let mut cases: Vec<Value> = basis
        .evals
        .cases
        .iter()
        .map(|c| serde_json::to_value(c).unwrap_or(Value::Null))
        .collect();
    let id_of = |c: &Value| c.get("id").and_then(Value::as_str).map(str::to_owned);
    for asked in answer
        .pointer("/evals/cases")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(id) = id_of(asked)
            && !cases
                .iter()
                .any(|c| id_of(c).as_deref() == Some(id.as_str()))
        {
            cases.push(asked.clone());
        }
    }
    for failing in failing {
        let id = format!("{failing}{REGRESSION}");
        if cases
            .iter()
            .any(|c| id_of(c).as_deref() == Some(id.as_str()))
        {
            continue;
        }
        if let Some(case) = basis.evals.cases.iter().find(|c| &c.id == failing) {
            let mut copy = serde_json::to_value(case).unwrap_or(Value::Null);
            copy["id"] = json!(id);
            copy["smoke"] = json!(false);
            cases.push(copy);
        }
    }
    if let Some(evals) = object.get_mut("evals").and_then(Value::as_object_mut) {
        evals.insert("cases".into(), json!(cases));
    }
    merged
}

/// What the model proposed.
#[derive(Debug, Clone)]
pub struct Proposed {
    pub draft: Draft,
    pub problems: Vec<Problem>,
    /// The regression cases it adds.
    pub regression: Vec<String>,
    pub diff: Vec<String>,
    pub instructions: Vec<String>,
}

/// Has `executor`'s model of `provider` propose an improvement from `m`, for the profile
/// `catalog` describes (the agent's fixtures in `store`). `progress` hears what is happening.
pub async fn propose(
    executor: &dyn Executor,
    store: &FactoryStore,
    m: &Material,
    catalog: &Catalog,
    provider: Provider,
    progress: &(dyn Fn(String) + Send + Sync),
) -> Result<Proposed> {
    // The cases of its own that failed get a regression case (the built-in attacks run always).
    let failing: Vec<String> = m
        .failures
        .iter()
        .map(|f| f.case.clone())
        .chain(m.cases.iter().cloned())
        .filter(|c| m.basis.evals.cases.iter().any(|x| &x.id == c))
        .fold(Vec::new(), |mut all, c| {
            if !all.contains(&c) {
                all.push(c);
            }
            all
        });
    let name = m.name.clone();
    let basis = m.basis.clone();
    let answered = draft::ask(
        executor,
        provider,
        &catalog.models,
        prompt(m, catalog),
        "proposal",
        &|answer: Value| {
            let merged = merge(&basis, &answer, &failing);
            let problems = runner::draft_problems(store, &name, &merged, catalog);
            (merged, problems)
        },
        progress,
    )
    .await?;
    let regression: Vec<String> = answered
        .blueprint
        .pointer("/evals/cases")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|c| c.get("id").and_then(Value::as_str))
        .filter(|id| id.ends_with(REGRESSION) && !m.basis.evals.cases.iter().any(|c| c.id == *id))
        .map(str::to_owned)
        .collect();
    let (diff, instructions) = match from_value(&answered.blueprint) {
        Ok(new) => (
            release::blueprint_diff(&m.basis, &new),
            release::text_diff(&m.basis.instructions, &new.instructions),
        ),
        Err(_) => (Vec::new(), Vec::new()),
    };
    let note = m.note.as_deref().unwrap_or_default();
    Ok(Proposed {
        draft: Draft {
            blueprint: answered.blueprint,
            provenance: Provenance {
                source: "model".into(),
                template: None,
                provider: Some(provider),
                model: answered.model,
                effort: answered.effort,
                // What it was asked from (the note's hash, never its text).
                description_sha256: (!note.is_empty())
                    .then(|| crate::evidence::sha256_hex(note.as_bytes())),
                at: now(),
            },
            tries: answered.tries,
        },
        problems: answered.problems,
        regression,
        diff,
        instructions,
    })
}

/// Where agent `name`'s proposals are kept.
fn proposals_dir(store: &FactoryStore, name: &str) -> PathBuf {
    store.agent_dir(name).join("proposals")
}

/// Keeps `proposed` (made from `m`, asked for by `by`) as agent `name`'s draft, recorded in
/// its history, with its proposal next to it (its build, with `skills`, says which
/// evaluations are its). Its release, deployments and everything that runs stay as they
/// are. Returns the proposal.
pub fn keep(
    store: &FactoryStore,
    m: &Material,
    proposed: &Proposed,
    by: &str,
    skills: &crate::skills::Skills,
) -> Result<Proposal> {
    let name = &m.name;
    let at = chrono::Utc::now();
    let id = format!("pr-{}", at.format("%Y%m%d-%H%M%S"));
    let previous = store.draft(name)?;
    let proposal = Proposal {
        id: id.clone(),
        at: crate::factory::report::timestamp(at),
        by: by.to_owned(),
        basis: m.basis_of.clone(),
        evaluation: m.evaluation.clone(),
        cases: m.cases.clone(),
        failures: m.failures.clone(),
        tasks: m.tasks.clone(),
        note: m.note.clone(),
        regression: proposed.regression.clone(),
        diff: proposed.diff.clone(),
        instructions: proposed.instructions.clone(),
        errors: proposed.problems.iter().filter(|p| p.error).count(),
        warnings: proposed.problems.iter().filter(|p| !p.error).count(),
        provenance: proposed.draft.provenance.clone(),
        previous_draft: previous,
        build: from_value(&proposed.draft.blueprint)
            .ok()
            .and_then(|bp| crate::factory::build::build(&bp, skills).ok())
            .map(|b| b.hash),
    };
    write_json(
        &proposals_dir(store, name).join(format!("{id}.json")),
        &proposal,
    )?;
    let from = if m.failures.is_empty() {
        String::new()
    } else {
        format!(
            " from {} failing case{}",
            m.failures.len(),
            if m.failures.len() == 1 { "" } else { "s" }
        )
    };
    let tasks = if m.tasks.is_empty() {
        String::new()
    } else {
        format!(
            "{} {} failed task{}",
            if from.is_empty() { " from" } else { " and" },
            m.tasks.len(),
            if m.tasks.len() == 1 { "" } else { "s" }
        )
    };
    store.save_draft(
        name,
        &proposed.draft,
        by,
        &format!(
            "improvement {id} of {}{from}{tasks}, proposed by {}'s model: a new draft version, evaluated next",
            m.basis_of,
            proposed
                .draft
                .provenance
                .provider
                .map(|p| p.to_string())
                .unwrap_or_default()
        ),
    )?;
    Ok(proposal)
}

/// Agent `name`'s proposals, newest first.
pub fn proposals(store: &FactoryStore, name: &str) -> Result<Vec<Proposal>> {
    let mut found: Vec<Proposal> = crate::factory::store::read_dir(&proposals_dir(store, name))?
        .into_iter()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| read_json::<Proposal>(&e.path()).ok().flatten())
        .collect();
    found.sort_by(|a, b| b.id.cmp(&a.id));
    Ok(found)
}

/// The newest evaluation of `proposal`'s build, if one started.
pub fn evaluation_of(store: &FactoryStore, name: &str, proposal: &Proposal) -> Option<Evaluation> {
    let build = proposal.build.as_deref()?;
    store
        .evaluations(name)
        .ok()?
        .into_iter()
        .find(|e| e.build == build && e.canary.is_none())
}

/// The model a proposal of agent `name` is evaluated on: the one its active release runs
/// on, as `provider:model[:effort]` (none without a release: its blueprint's candidates).
pub fn candidate_of(store: &FactoryStore, name: &str) -> Option<String> {
    let version = store.state(name).ok()??.active_version?;
    let route = release::load(store, name, &version)
        .ok()?
        .manifest
        .model
        .chosen;
    let model = route.model?;
    Some(match route.effort {
        Some(effort) => format!("{}:{model}:{effort}", route.provider),
        None => format!("{}:{model}", route.provider),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::factory::blueprint::tests::{catalog, sample};
    use crate::factory::release::tests::Fixture;
    use crate::llm::{LlmTask, Ran, TaskContext};
    use std::sync::Mutex;

    /// A model that answers with one blueprint, and keeps what it was asked.
    struct Scripted {
        answer: Value,
        asked: Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl Executor for Scripted {
        fn providers(&self) -> Vec<Provider> {
            vec![Provider::Copilot]
        }

        async fn execute(&self, task: &LlmTask, ctx: &TaskContext) -> Result<Ran> {
            assert_eq!(
                std::fs::read_dir(&ctx.workspace)?.count(),
                0,
                "an empty folder"
            );
            assert_eq!(ctx.access, crate::llm::Access::ReadOnly);
            self.asked.lock().unwrap().push(task.prompt.clone());
            Ok(Ran::new(
                format!("```json\n{:#}\n```", self.answer),
                Some("gpt-6-sol".into()),
            ))
        }
    }

    /// AC-25 and TM-13: given failing cases and failed tasks, the owner's model proposes a
    /// new draft version (secrets redacted from what it is shown, task text never), with a
    /// diff next to the active release and regression cases; it asks for nothing more, and
    /// the active release, its files and its deployments stay as they were.
    #[tokio::test]
    async fn an_improvement_is_a_new_draft_with_a_diff_and_regression_cases_that_runs_nothing() {
        let f = Fixture::new();
        let mine = |v: &mut Value| v["approvers"] = json!(["me"]);
        std::fs::create_dir_all(f.store.fixtures_dir("fin-analyst")).unwrap();
        std::fs::write(
            f.store.fixtures_dir("fin-analyst").join("q3.pdf"),
            "%PDF q3",
        )
        .unwrap();
        let passed = f.evaluated(mine, false);
        f.release(&passed.id, None).unwrap();
        f.approve("1.0.0");
        let before = f.store.state("fin-analyst").unwrap().unwrap();
        assert_eq!(before.active_version.as_deref(), Some("1.0.0"));
        let none = material_in(&f.store, "fin-analyst", &[], None, Vec::new()).unwrap_err();
        assert_eq!(
            crate::factory::refusal_kind(&none),
            Some(crate::factory::RefusalKind::Conflict)
        );
        assert!(
            format!("{none:#}").contains("no failing case or failed task"),
            "{none:#}"
        );

        // Later, an evaluation of what runs (the same build) fails, a secret in an answer.
        let mut failing = f.evaluated(mine, true);
        let token = format!("ghp_{}", "a1B2".repeat(9));
        failing.runs[0].output = Some(format!("The margin is 12%. Token {token}"));
        f.store.save_evaluation(&failing).unwrap();
        let task = FailedTask {
            at: "2026-11-25T10:00:00Z".into(),
            agent: "fin-analyst@1.0.0".into(),
            worker: "fin-analyst".into(),
            attempts: Some(3),
            checks: Some(CheckTally {
                passed: 1,
                total: 2,
            }),
            secs: 45,
            model: Some("gpt-6-sol".into()),
        };
        let m = material_in(
            &f.store,
            "fin-analyst",
            &[],
            Some("Quote the sentence, don't paraphrase it"),
            vec![task],
        )
        .unwrap();
        assert_eq!(m.basis_of, "release 1.0.0");
        assert_eq!(m.evaluation.as_deref(), Some(failing.id.as_str()));
        assert!(m.cases.contains(&"E1".to_owned()) && m.cases.contains(&"E2".to_owned()));

        let mut answer = sample();
        mine(&mut answer);
        answer["instructions"] = json!(
            "You analyze financial reports for an executive audience.\nQuote every risk's sentence exactly."
        );
        answer["capabilities"]["access"] = json!("full");
        answer["evals"]["cases"]
            .as_array_mut()
            .unwrap()
            .push(json!({
                "id": "E2-regression", "requirement": "R2", "input": "Quote each risk in q3.pdf",
                "fixtures": ["q3.pdf"], "expect": [{ "regex": "(?i)risk" }]
            }));
        let model = Scripted {
            answer,
            asked: Mutex::new(Vec::new()),
        };
        let proposed = propose(&model, &f.store, &m, &catalog(), Provider::Copilot, &|_| {})
            .await
            .unwrap();
        let asked = model.asked.lock().unwrap()[0].clone();
        assert!(!asked.contains(&token), "secrets never reach the model");
        assert!(asked.contains("[REDACTED:github-token]"));
        assert!(asked.contains("Quote the sentence, don't paraphrase it"));
        assert!(asked.contains("failed after 3 attempts, checks 1 of 2 passed, 45s"));
        assert!(
            asked.starts_with(crate::boundary::DIRECTIVE),
            "what failed is data"
        );
        assert_eq!(proposed.regression, ["E2-regression", "E1-regression"]);
        assert!(
            proposed
                .diff
                .iter()
                .any(|l| l.starts_with("~ instructions")),
            "{:?}",
            proposed.diff
        );
        assert!(
            proposed
                .diff
                .iter()
                .any(|l| l.starts_with("+ case E2-regression"))
        );
        assert!(
            !proposed.diff.iter().any(|l| l.contains("access")),
            "it asks for nothing more"
        );
        assert!(
            proposed
                .instructions
                .contains(&"+ Quote every risk's sentence exactly.".to_owned())
        );

        let old_draft = f.store.draft("fin-analyst").unwrap();
        let skills = crate::skills::Skills::new(f.dir.path().join("skills"));
        let proposal = keep(&f.store, &m, &proposed, "alice", &skills).unwrap();
        assert_eq!(
            proposal.previous_draft, old_draft,
            "the draft it replaced is kept"
        );
        assert!(proposal.build.is_some());
        let draft = from_value(&f.store.draft("fin-analyst").unwrap().unwrap().blueprint).unwrap();
        let ids: Vec<&str> = draft.evals.cases.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(
            ids,
            ["E1", "E2", "E2-regression", "E1-regression"],
            "the suite plus regression cases"
        );
        assert_eq!(proposals(&f.store, "fin-analyst").unwrap(), [proposal]);

        // Nothing that runs changed: only a release does that.
        let after = f.store.state("fin-analyst").unwrap().unwrap();
        assert_eq!(after.active_version, before.active_version);
        assert_eq!(after.versions, before.versions);
        assert_eq!(after.deployments, before.deployments);
        release::load(&f.store, "fin-analyst", "1.0.0").unwrap();
        assert!(
            after
                .history
                .last()
                .unwrap()
                .what
                .starts_with("improvement pr-")
        );
    }

    #[test]
    fn a_proposal_keeps_every_case_and_capability_and_adds_regression_cases() {
        let basis = from_value(&crate::factory::blueprint::tests::sample()).unwrap();
        let mut answer = serde_json::to_value(&basis).unwrap();
        answer["instructions"] = json!("Quote every risk's sentence exactly.");
        answer["requirements"][0]["text"] = json!("Whatever it likes");
        answer["capabilities"]["access"] = json!("full");
        answer["capabilities"]["delegate"] = json!(true);
        answer["title"] = json!("Something else");
        let first = basis.evals.cases[0].id.clone();
        // It drops a case and weakens another; it adds one of its own.
        let cases = answer["evals"]["cases"].as_array_mut().unwrap();
        cases.remove(0);
        cases[0]["expect"] = json!([{ "max-words": 9999 }]);
        cases.push(json!({ "id": format!("{first}-regression"), "requirement": basis.evals.cases[0].requirement,
                           "input": "Quote the risk in q3.pdf.", "expect": [{ "contains": "risk" }] }));
        let merged = merge(&basis, &answer, std::slice::from_ref(&first));
        let new = from_value(&merged).unwrap();
        assert_eq!(new.instructions, "Quote every risk's sentence exactly.");
        assert_eq!(
            new.capabilities, basis.capabilities,
            "it asks for nothing more"
        );
        assert_eq!(new.requirements, basis.requirements, "it promises the same");
        assert_eq!(new.title, basis.title);
        assert_eq!(
            &new.evals.cases[..basis.evals.cases.len()],
            &basis.evals.cases[..],
            "every case it had, exactly"
        );
        let added: Vec<&str> = new.evals.cases[basis.evals.cases.len()..]
            .iter()
            .map(|c| c.id.as_str())
            .collect();
        assert_eq!(added, [format!("{first}-regression")], "the model's own");

        // A failing case it made no regression case for gets a copy.
        let copied = from_value(&merge(&basis, &json!({}), std::slice::from_ref(&first))).unwrap();
        let regression = copied.evals.cases.last().unwrap();
        assert_eq!(regression.id, format!("{first}-regression"));
        assert_eq!(regression.input, basis.evals.cases[0].input);
        assert!(!regression.smoke);
    }

    #[test]
    fn instructions_are_diffed_line_by_line() {
        assert_eq!(
            release::text_diff("Read it.\nAnswer.\n", "Read it.\nQuote it.\nAnswer.\n"),
            ["  Read it.", "+ Quote it.", "  Answer."]
        );
        assert_eq!(release::text_diff("a\nb", "a\nc"), ["  a", "- b", "+ c"]);
    }
}
