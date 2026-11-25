//! The web UI's agent factory (docs/agent-factory.md, section 14), as its contract says
//! (docs/agent-factory/spec/openapi.json): the templates, drafts and their checks, the
//! factory's agents, and evaluations: what one would run, running it on the owner's
//! evaluation worker, its progress and report, and cancelling it. Behind the web UI's
//! token; other agents reach the same functions over MCP ([`super::connect`]), and what
//! they do is recorded as theirs (`by`).

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{ApiError, AppState, Shared, bad_request, internal};
use crate::agent::MAX_CONCURRENT_TASKS;
use crate::factory::blueprint::{Catalog, Problem, from_value, has_errors, name_problem};
use crate::factory::report::{EvalState, Evaluation};
use crate::factory::routing::Route;
use crate::factory::runner::{self, Judges, Request, Runner};
use crate::factory::store::{Draft, FactoryStore, Lifecycle, Provenance, now};
use crate::factory::{capabilities, draft, templates};
use crate::llm::{Executor, ExecutorConfig, Provider, SdkExecutor};

fn status(code: StatusCode, message: impl ToString) -> ApiError {
    (code, Json(json!({ "error": message.to_string() })))
}

fn not_found(message: impl ToString) -> ApiError {
    status(StatusCode::NOT_FOUND, message)
}

fn conflict(message: impl ToString) -> ApiError {
    status(StatusCode::CONFLICT, message)
}

/// Who did it, as the factory records it: the profile's owner here, or another agent.
pub(super) fn owner(s: &Shared) -> String {
    s.info.profile.name.clone()
}

/// What a check found, as the contract shows it.
fn check_result(problems: &[Problem], increases: Vec<String>) -> Value {
    json!({ "passed": !has_errors(problems), "problems": problems, "increases": increases })
}

/// A draft as the contract shows it: its blueprint, problems, provenance and tries.
fn draft_json(draft: &Draft, problems: &[Problem]) -> Value {
    json!({
        "blueprint": draft.blueprint,
        "problems": problems,
        "provenance": draft.provenance,
        "tries": draft.tries,
    })
}

fn catalog_of(s: &Shared, name: &str) -> Result<Catalog, ApiError> {
    Catalog::of(&s.profile_now())
        .map(|c| c.without(name))
        .map_err(internal)
}

pub(super) async fn list_templates() -> Json<Value> {
    Json(json!({ "templates": templates::list() }))
}

/// A new draft, as asked for: by description (the profile's model writes it), from a
/// template, or both.
#[derive(Debug, Default, Deserialize)]
pub(super) struct NewDraft {
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub template: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    /// The provider whose model writes it (default: the first the profile is signed in to).
    #[serde(default)]
    pub provider: Option<Provider>,
}

pub(super) async fn create_draft(
    State(s): State<AppState>,
    Json(body): Json<NewDraft>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let by = owner(&s);
    let draft = new_draft(&s, body, &by).await?;
    Ok((StatusCode::CREATED, Json(draft)))
}

/// An executor that runs `provider`: the agent's, when it serves it, else one started for
/// the purpose (`true`: stop it when done).
async fn executor_for(
    s: &Shared,
    provider: Provider,
) -> Result<(Arc<dyn Executor>, Option<Arc<SdkExecutor>>), ApiError> {
    if let Some(executor) = &s.executor
        && executor.providers().contains(&provider)
    {
        return Ok((executor.clone(), None));
    }
    let started = SdkExecutor::start(ExecutorConfig {
        providers: vec![provider],
        bridge_script: crate::setup::resolve_bridge(
            crate::profile::env_setting_os("BRIDGE").map(std::path::PathBuf::from),
        ),
        profile: s.profile_now(),
    })
    .await
    .map_err(|e| internal(format!("starting {provider}: {e:#}")))?;
    Ok((started.clone(), Some(started)))
}

/// Makes draft `body` asks for; `by` made it.
pub(super) async fn new_draft(s: &Shared, body: NewDraft, by: &str) -> Result<Value, ApiError> {
    let profile = s.profile_now();
    let catalog = Catalog::of(&profile).map_err(internal)?;
    let template = body
        .template
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty());
    if let Some(template) = template {
        templates::get(template).map_err(|e| bad_request(format!("{e:#}")))?;
    }
    let name = body
        .name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .or(template)
        .ok_or_else(|| bad_request("name the agent"))?
        .to_owned();
    if let Some(problem) = name_problem(&name, &catalog) {
        return Err(if problem.code == "name_taken" {
            conflict(format!("{}: pick another name", problem.message))
        } else {
            bad_request(problem.message)
        });
    }
    let (draft, what) = match body.description {
        Some(description) => {
            let description = description.trim();
            if description.is_empty() {
                return Err(bad_request(
                    "the description is empty: say what the agent should do",
                ));
            }
            if description.chars().count() > draft::MAX_DESCRIPTION {
                return Err(bad_request(format!(
                    "the description is over {} characters: say it in fewer words",
                    draft::MAX_DESCRIPTION
                )));
            }
            let provider = match body.provider {
                Some(p) if catalog.providers.contains(&p) => p,
                Some(p) => {
                    return Err(conflict(format!(
                        "profile '{}' isn't signed in to {p}: {}",
                        profile.name,
                        profile.login_hint(p)
                    )));
                }
                None => *catalog.providers.first().ok_or_else(|| {
                    conflict(format!(
                        "profile '{}' isn't signed in to a provider whose model could write the draft: proofkin auth login copilot --profile {0}",
                        profile.name
                    ))
                })?,
            };
            let (executor, started) = executor_for(s, provider).await?;
            let request = draft::Request {
                name: &name,
                description,
                template,
                provider,
            };
            let progress = |line: String| {
                s.send(json!({ "type": "factory", "draft": name, "progress": line }));
            };
            let written = draft::write(executor.as_ref(), &request, &catalog, &progress).await;
            if let Some(started) = started {
                started.stop().await;
            }
            let written = written.map_err(|e| internal(format!("{e:#}")))?;
            let model = written
                .draft
                .provenance
                .model
                .clone()
                .unwrap_or_else(|| "its default".into());
            let what = format!(
                "drafted by {provider}'s model ({model}) from a description{}",
                template
                    .map(|t| format!(" and template {t}"))
                    .unwrap_or_default()
            );
            (written.draft, what)
        }
        None => {
            let Some(template) = template else {
                return Err(bad_request(
                    "describe what the agent should do, or start from a template",
                ));
            };
            let blueprint = templates::instantiate(template, &name, &catalog)
                .map_err(|e| conflict(format!("{e:#}")))?;
            let draft = Draft {
                blueprint,
                provenance: Provenance {
                    source: "template".into(),
                    template: Some(template.to_owned()),
                    at: now(),
                    ..Provenance::default()
                },
                tries: 0,
            };
            (draft, format!("drafted from template {template}"))
        }
    };
    let store = FactoryStore::of(&profile);
    store
        .create_draft(&name, &draft, by, &what)
        .map_err(|e| conflict(format!("{e:#}")))?;
    let problems = runner::draft_problems(&store, &name, &draft.blueprint, &catalog.without(&name));
    s.send(json!({ "type": "factory" }));
    Ok(draft_json(&draft, &problems))
}

fn draft_of(store: &FactoryStore, name: &str) -> Result<Draft, ApiError> {
    store
        .draft(name)
        .ok()
        .flatten()
        .ok_or_else(|| not_found(format!("the factory has no draft '{name}'")))
}

pub(super) async fn get_draft(
    State(s): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let store = FactoryStore::of(&s.info.profile);
    let draft = draft_of(&store, &name)?;
    let problems = runner::draft_problems(&store, &name, &draft.blueprint, &catalog_of(&s, &name)?);
    Ok(Json(draft_json(&draft, &problems)))
}

pub(super) async fn save_draft(
    State(s): State<AppState>,
    Path(name): Path<String>,
    Json(blueprint): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let by = owner(&s);
    Ok(Json(put_draft(&s, &name, blueprint, &by)?))
}

/// Saves `blueprint` as draft `name`'s; `by` changed it. It is kept with its problems,
/// as long as it is a blueprint.
pub(super) fn put_draft(
    s: &Shared,
    name: &str,
    blueprint: Value,
    by: &str,
) -> Result<Value, ApiError> {
    let store = FactoryStore::of(&s.info.profile);
    let draft = draft_of(&store, name)?;
    if blueprint.get("name").and_then(Value::as_str) != Some(name) {
        return Err(bad_request(format!(
            "the blueprint's name must stay \"{name}\": for another name, start a new draft"
        )));
    }
    if let Err(p) = from_value(&blueprint) {
        let at = p.path.as_deref().filter(|x| !x.is_empty());
        return Err(bad_request(match at {
            Some(at) => format!("{at}: {}", p.message),
            None => p.message,
        }));
    }
    store
        .save_draft(
            name,
            &Draft {
                blueprint: blueprint.clone(),
                ..draft
            },
            by,
            "edited",
        )
        .map_err(internal)?;
    let problems = runner::draft_problems(&store, name, &blueprint, &catalog_of(s, name)?);
    s.send(json!({ "type": "factory" }));
    // No release yet to ask for more than (see crate::factory::capabilities::increases).
    Ok(check_result(&problems, Vec::new()))
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct CheckBody {
    #[serde(default)]
    pub folders: Option<Vec<String>>,
}

pub(super) async fn check_draft(
    State(s): State<AppState>,
    Path(name): Path<String>,
    body: Option<Json<CheckBody>>,
) -> Result<Json<Value>, ApiError> {
    let folders = body.and_then(|Json(b)| b.folders);
    Ok(Json(checked(&s, &name, folders)?))
}

/// Draft `name`'s problems, with what it asks for resolved in `folders` (default: its
/// deploy folders): what each would give it, and errors for what none offers.
pub(super) fn checked(
    s: &Shared,
    name: &str,
    folders: Option<Vec<String>>,
) -> Result<Value, ApiError> {
    let store = FactoryStore::of(&s.info.profile);
    let draft = draft_of(&store, name)?;
    let catalog = catalog_of(s, name)?;
    let mut problems = runner::draft_problems(&store, name, &draft.blueprint, &catalog);
    let mut grants = Vec::new();
    if let Ok(bp) = from_value(&draft.blueprint) {
        let folders = folders.unwrap_or_else(|| bp.deploy.folders.clone());
        let resolution = capabilities::resolve(&bp, &folders, &catalog);
        for problem in resolution.problems {
            if !problems.contains(&problem) {
                problems.push(problem);
            }
        }
        problems.sort_by_key(|p| !p.error);
        grants = resolution
            .grants
            .iter()
            .map(|g| {
                json!({ "folder": g.folder, "access": g.access, "mcp": g.mcp, "checks": g.checks })
            })
            .collect();
    }
    let mut result = check_result(&problems, Vec::new());
    result["grants"] = json!(grants);
    Ok(result)
}

/// An evaluation in a line, for lists.
fn evaluation_brief(e: &Evaluation, running: bool) -> Value {
    json!({
        "id": e.id,
        "build": e.build,
        "state": e.state,
        "running": running && e.state == EvalState::Running,
        "started": e.started,
        "finished": e.finished,
        "done": e.done(),
        "total": e.runs.len(),
        "gate": e.gate,
        "recommended": e.recommended,
        "by": e.by,
    })
}

/// An agent as the contract lists it, with its draft's problems and its latest
/// evaluations.
fn agent_json(store: &FactoryStore, s: &Shared, name: &str, all: bool) -> Result<Value, ApiError> {
    let draft = store.draft(name).map_err(internal)?;
    let state = store.state(name).map_err(internal)?;
    if draft.is_none() && state.is_none() {
        return Err(not_found(format!("the factory has no agent '{name}'")));
    }
    let title = draft
        .as_ref()
        .and_then(|d| d.blueprint.get("title"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let problems = match &draft {
        Some(d) => runner::draft_problems(store, name, &d.blueprint, &catalog_of(s, name)?),
        None => Vec::new(),
    };
    let running = store.evaluation_running();
    let evaluations = store.evaluations(name).map_err(internal)?;
    let shown = if all { evaluations.len() } else { 1 };
    let lifecycle = state.as_ref().map_or(Lifecycle::Draft, |st| st.state);
    Ok(json!({
        "name": name,
        "title": title,
        "state": lifecycle,
        "active_version": state.as_ref().and_then(|st| st.active_version.clone()),
        "versions": state.as_ref().map(|st| st.versions.clone()).unwrap_or_default(),
        "deployments": [],
        "errors": problems.iter().filter(|p| p.error).count(),
        "warnings": problems.iter().filter(|p| !p.error).count(),
        "builds": store.builds(name).map_err(internal)?,
        "evaluations": evaluations.iter().take(shown).map(|e| evaluation_brief(e, running)).collect::<Vec<_>>(),
    }))
}

pub(super) async fn list_agents(State(s): State<AppState>) -> Result<Json<Value>, ApiError> {
    Ok(Json(agents(&s)?))
}

/// The factory's agents, each as [`agent_json`] says with its latest evaluation.
pub(super) fn agents(s: &Shared) -> Result<Value, ApiError> {
    let store = FactoryStore::of(&s.info.profile);
    let agents = store
        .names()
        .map_err(internal)?
        .iter()
        .map(|name| agent_json(&store, s, name, false))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(json!({ "agents": agents }))
}

pub(super) async fn get_agent(
    State(s): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let store = FactoryStore::of(&s.info.profile);
    if crate::profile::validate_name(&name).is_err() {
        return Err(not_found(format!("the factory has no agent '{name}'")));
    }
    let mut agent = agent_json(&store, &s, &name, true)?;
    // The models it would be evaluated on now, and those left out.
    if let Some(draft) = store.draft(&name).map_err(internal)?
        && let Ok(bp) = from_value(&draft.blueprint)
    {
        let offers = eval_peer(&s).map(|p| p.offers);
        let routes = crate::factory::routing::candidates(
            &bp,
            &catalog_of(&s, &name)?,
            offers.as_ref(),
            true,
        );
        agent["candidates"] = json!(routes.candidates);
        agent["left_out"] = routes
            .left_out
            .iter()
            .map(|l| json!({ "candidate": l.route, "why": l.why }))
            .collect();
    }
    Ok(Json(agent))
}

/// The evaluation worker, as connected now.
fn eval_peer(s: &Shared) -> Option<crate::agent::PeerInfo> {
    let name = runner::worker_name(&s.info.profile.name);
    s.agent.peers().into_iter().find(|p| p.name == name)
}

/// What starting an evaluation asks for.
#[derive(Debug, Default, Deserialize)]
pub(super) struct EvaluationBody {
    #[serde(default)]
    pub candidates: Vec<Route>,
    #[serde(default)]
    pub repetitions: Option<u32>,
    #[serde(default)]
    pub peer: Option<String>,
    pub confirm: bool,
}

pub(super) async fn start_evaluation(
    State(s): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<EvaluationBody>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let by = owner(&s);
    let (code, value) = evaluate(&s, &name, body, &by).await?;
    Ok((code, Json(value)))
}

/// Evaluates draft `name` as `body` asks: its estimate (`confirm` false), or the evaluation
/// started (`confirm`) on the owner's evaluation worker; `by` started it.
pub(super) async fn evaluate(
    s: &Arc<Shared>,
    name: &str,
    body: EvaluationBody,
    by: &str,
) -> Result<(StatusCode, Value), ApiError> {
    let profile = s.profile_now();
    let store = FactoryStore::of(&profile);
    if crate::profile::validate_name(name).is_err() || store.draft(name).ok().flatten().is_none() {
        return Err(not_found(format!("the factory has no draft '{name}'")));
    }
    let worker_name = runner::worker_name(&profile.name);
    if let Some(peer) = body.peer.as_deref().filter(|p| *p != worker_name) {
        return Err(bad_request(format!(
            "evaluations run on your evaluation worker {worker_name}, not on {peer}: other peers aren't supported yet"
        )));
    }
    let peer = eval_peer(s);
    let planned = runner::plan(
        &profile,
        name,
        &body.candidates,
        body.repetitions,
        peer.as_ref().map(|p| &p.offers),
    )
    .map_err(|refused| {
        if refused.conflict {
            conflict(refused.message)
        } else {
            bad_request(refused.message)
        }
    })?;
    let slots = peer
        .as_ref()
        .and_then(|p| p.load)
        .map(|l| l.slots as usize)
        .filter(|n| *n > 0)
        .unwrap_or(MAX_CONCURRENT_TASKS);
    let estimate = runner::estimate(
        &planned.blueprint,
        planned.candidates.len(),
        planned.repetitions,
        slots,
    );
    if !body.confirm {
        let mut value = json!(estimate);
        value["build"] = json!(planned.build.hash);
        value["candidate_routes"] = json!(planned.candidates);
        value["left_out"] = planned
            .left_out
            .iter()
            .map(|l| json!({ "candidate": l.route, "why": l.why }))
            .collect();
        value["worker"] = json!(worker_name);
        return Ok((StatusCode::OK, value));
    }
    let supervisor = s.info.workers.clone().ok_or_else(|| {
        conflict("this agent can't run your evaluation worker (a digital worker can't own workers)")
    })?;
    let lock = store
        .lock_evaluation()
        .map_err(|e| conflict(format!("{e:#}")))?;
    let mut providers: Vec<Provider> = planned.candidates.iter().map(|c| c.provider).collect();
    providers.sort();
    providers.dedup();
    let worker = runner::eval_worker(&profile, s.agent.addr(), &s.agent.trusted(), &providers)
        .map_err(|e| conflict(format!("{e:#}")))?;
    if let Some(entry) = &worker.entry {
        s.agent.allow(entry.clone());
        s.save_trust()?;
        s.broadcast_peers();
    }
    let folders = profile.shares().map_err(internal)?;
    let resumable = runner::unfinished(&store, name, &planned.build.hash).map_err(internal)?;
    let setup = match resumable {
        Some(e) => runner::resume(&store, e, &planned.blueprint, &folders),
        None => runner::prepare(
            &store,
            Request {
                blueprint: &planned.blueprint,
                build: &planned.build,
                candidates: planned.candidates.clone(),
                repetitions: planned.repetitions,
                by: by.to_owned(),
                worker: worker.profile.name.clone(),
                worker_id: worker.id,
                folders: &folders,
            },
        ),
    }
    .map_err(|e| bad_request(format!("{e:#}")))?;
    runner::install(&worker.profile, &setup.share).map_err(internal)?;
    let key = profile.identity().map_err(internal)?;
    let id = setup.evaluation.id.clone();
    let shown = view(&setup.evaluation, true);
    let shared = s.clone();
    let task = tokio::spawn(async move {
        // Held while it runs: one evaluation at a time.
        let _lock = lock;
        let s = shared;
        let store = FactoryStore::of(&s.info.profile);
        let catalog = Catalog::of(&s.profile_now()).ok();
        let judges = match (&s.executor, &catalog) {
            (Some(executor), Some(catalog)) => Judges::of(
                executor.clone(),
                catalog,
                eval_peer(&s).map(|p| p.offers).as_ref(),
            ),
            _ => Judges::default(),
        };
        let runner = Runner {
            agent: s.agent.clone(),
            key: key.clone(),
            signer: s.info.profile.name.clone(),
            worker: worker.id,
            store: store.clone(),
            judges,
            ready_wait: runner::READY_WAIT,
        };
        let id = setup.evaluation.id.clone();
        let outcome = match runner::start_worker(&supervisor, &s.agent, &worker).await {
            Ok(()) => {
                let on_change = |e: &Evaluation| {
                    s.send(json!({
                        "type": "factory",
                        "evaluation": e.id,
                        "agent": e.agent,
                        "state": e.state,
                        "done": e.done(),
                        "total": e.runs.len(),
                    }));
                };
                runner.run(setup.evaluation, &setup.share, on_change).await
            }
            Err(e) => Err(e),
        };
        let ended = match outcome {
            Ok(evaluation) => evaluation,
            Err(e) => match runner::cancel(
                &store,
                &id,
                &format!("it stopped: {e:#}"),
                &s.info.profile.name,
                &key,
            ) {
                Ok(evaluation) => evaluation,
                Err(_) => {
                    s.system("error", format!("Evaluation {id} stopped: {e:#}"));
                    s.evaluations.lock().unwrap().remove(&id);
                    return;
                }
            },
        };
        let _ = runner::clean(&store, Some(&worker.profile));
        supervisor.stop(&worker.profile.name).await;
        s.evaluations.lock().unwrap().remove(&id);
        s.system(
            if ended.gate.passed { "info" } else { "warn" },
            format!(
                "Evaluation of {} {}{}",
                ended.agent,
                ended.state,
                ended
                    .recommended
                    .as_ref()
                    .map(|r| format!(": it recommends {r}"))
                    .or_else(|| ended.gate.reasons.first().map(|r| format!(": {r}")))
                    .unwrap_or_default()
            ),
        );
        s.send(json!({ "type": "factory", "evaluation": ended.id, "agent": ended.agent, "state": ended.state, "done": ended.done(), "total": ended.runs.len() }));
    });
    s.evaluations
        .lock()
        .unwrap()
        .insert(id.clone(), task.abort_handle());
    s.system("info", format!("Evaluating {name} ({id})"));
    s.send(json!({ "type": "factory", "evaluation": id, "agent": name, "state": "running" }));
    Ok((StatusCode::ACCEPTED, shown))
}

/// An evaluation for the page: its record without what doesn't belong there (the canary,
/// its runs' tasks and evidence, passing runs' outputs: `proofkin factory report ID --json`
/// has the signed record), with its progress and, once ended, whether it verifies.
pub(super) fn view(e: &Evaluation, running: bool) -> Value {
    let mut value = serde_json::to_value(e).unwrap_or(Value::Null);
    if let Some(object) = value.as_object_mut() {
        object.remove("seeds");
        object.remove("signature");
    }
    if let Some(runs) = value["runs"].as_array_mut() {
        for run in runs {
            let passed = run["passed"].as_bool().unwrap_or(false);
            if let Some(run) = run.as_object_mut() {
                run.remove("sent");
                run.remove("evidence");
                if passed {
                    run.remove("output");
                }
            }
        }
    }
    value["done"] = json!(e.done());
    value["total"] = json!(e.runs.len());
    value["running"] = json!(running && e.state == EvalState::Running);
    if e.state != EvalState::Running {
        value["verified"] = json!(e.verify().is_ok());
    }
    let text = e.seeds.redact(&value.to_string());
    serde_json::from_str(&text).unwrap_or(value)
}

pub(super) async fn get_evaluation(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(evaluation(&s, &id)?))
}

/// Evaluation `id`, for the page (see [`view`]).
pub(super) fn evaluation(s: &Shared, id: &str) -> Result<Value, ApiError> {
    let store = FactoryStore::of(&s.info.profile);
    let e = store
        .evaluation(id)
        .ok()
        .flatten()
        .ok_or_else(|| not_found(format!("there is no evaluation {id}")))?;
    let running = s.evaluations.lock().unwrap().contains_key(id);
    Ok(view(&e, running))
}

pub(super) async fn cancel_evaluation(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let by = owner(&s);
    Ok(Json(cancel(&s, &id, &by).await?))
}

/// Cancels evaluation `id` (`by` cancelled it): its runs still out stop on the worker, and
/// its report says what ran. One that stopped when its agent did ends here too.
pub(super) async fn cancel(s: &Shared, id: &str, by: &str) -> Result<Value, ApiError> {
    let profile = s.profile_now();
    let store = FactoryStore::of(&profile);
    let e = store
        .evaluation(id)
        .ok()
        .flatten()
        .ok_or_else(|| not_found(format!("there is no evaluation {id}")))?;
    if e.state != EvalState::Running {
        return Err(conflict(format!(
            "evaluation {id} ended already ({})",
            e.state
        )));
    }
    let handle = s.evaluations.lock().unwrap().remove(id);
    if let Some(handle) = &handle {
        handle.abort();
    }
    let key = profile.identity().map_err(internal)?;
    let ended = runner::cancel(
        &store,
        id,
        &format!("{by} cancelled it"),
        &profile.name,
        &key,
    )
    .map_err(internal)?;
    let worker = crate::workers::worker_of(&profile, &e.worker).ok();
    if handle.is_some() || !store.evaluation_running() {
        let _ = runner::clean(&store, worker.as_ref());
    }
    if let (Some(supervisor), Some(worker)) = (&s.info.workers, &worker) {
        supervisor.stop(&worker.name).await;
    }
    s.system("info", format!("Cancelled evaluation {id}"));
    s.send(
        json!({ "type": "factory", "evaluation": id, "agent": ended.agent, "state": ended.state }),
    );
    Ok(view(&ended, false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_page_never_gets_the_canary_tasks_or_evidence() {
        let mut e = crate::factory::report::tests::evaluation();
        e.runs[0].state = crate::factory::report::RunState::Done;
        e.runs[0].passed = false;
        e.runs[0].output = Some(format!("leaked {}", e.seeds.canary));
        e.runs[1].state = crate::factory::report::RunState::Done;
        e.runs[1].passed = true;
        e.runs[1].output = Some("fine".into());
        e.cases[0].expect = vec![json!({ "not-contains": e.seeds.canary })];
        let shown = view(&e, true).to_string();
        assert!(!shown.contains(&e.seeds.canary), "{shown}");
        assert!(!shown.contains("\"seeds\""));
        assert!(
            shown.contains("leaked [canary]"),
            "a failing run's output stays"
        );
        assert!(!shown.contains("\"fine\""), "a passing run's doesn't");
        let value = view(&e, true);
        assert_eq!(
            (value["done"].clone(), value["running"].clone()),
            (json!(2), json!(true))
        );
    }
}
