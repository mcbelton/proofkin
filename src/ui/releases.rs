//! The web UI's API for released agents (docs/agent-factory.md, sections 8 to 11), as its
//! contract says (docs/agent-factory/spec/openapi.json): releases and their sign-offs (the
//! owner's own come under **Waiting for your approval**, asked by the factory's sign-off
//! driver), deployments and their canary, rollback, deprecation and retirement, metrics and
//! budgets. Behind the web UI's token; over MCP every one of these stays with the owner.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{ApiError, AppState, Shared, internal};
use crate::factory::blueprint::Budgets;
use crate::factory::deploy::{self, DeployAsk, Host};
use crate::factory::lifecycle::{self, LifecycleRequest};
use crate::factory::release::{self, Asked, Release};
use crate::factory::store::{DeploymentRecord, FactoryStore};
use crate::factory::{RefusalKind, metrics, refusal_kind};
use crate::workers::Process;

/// A factory refusal with the status code its contract gives it.
fn refused(e: anyhow::Error) -> ApiError {
    let code = match refusal_kind(&e) {
        Some(RefusalKind::NotFound) => StatusCode::NOT_FOUND,
        Some(RefusalKind::Invalid) => StatusCode::BAD_REQUEST,
        Some(RefusalKind::Conflict) => StatusCode::CONFLICT,
        None => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (code, Json(json!({ "error": format!("{e:#}") })))
}

fn not_found(name: &str) -> ApiError {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": format!("the factory has no agent '{name}'") })),
    )
}

/// Checks `name` names one of the factory's agents.
fn known(store: &FactoryStore, name: &str) -> Result<(), ApiError> {
    if crate::profile::validate_name(name).is_err() || store.state(name).ok().flatten().is_none() {
        return Err(not_found(name));
    }
    Ok(())
}

/// A release as the contract shows it, with what the page shows of it (and the folders its
/// deployments work in unless they're told others).
pub(super) fn release_view(store: &FactoryStore, r: &Release) -> Value {
    let m = &r.manifest;
    let folders = release::blueprint_of(store, &m.agent, &m.version)
        .map(|bp| bp.deploy.folders)
        .unwrap_or_default();
    json!({
        "agent": m.agent,
        "version": m.version,
        "build": m.build,
        "state": r.state,
        "manifest_sha256": r.manifest_sha256,
        "signer_id": m.signer_id,
        "signer": m.signer,
        "approvals": r.approvals.iter().map(|a| json!({
            "approver": a.approver,
            "approved": a.approved,
            "note": a.note,
            "at": a.at,
            "covers": a.covers,
        })).collect::<Vec<_>>(),
        "increases": m.increases,
        "pending": r.pending().into_iter().map(|a| json!({ "approver": a.approver, "covers": a.covers })).collect::<Vec<_>>(),
        "why": r.why,
        "decided": r.decided,
        "title": m.title,
        "evaluation": m.evaluation,
        "change": m.change,
        "change_why": m.change_why,
        "previous": m.previous,
        "note": m.note,
        "created": m.created,
        "model": m.model.chosen,
        "capabilities": release::capabilities_text(&m.capabilities),
        "origin": m.origin,
        "files": m.files,
        "folders": folders,
    })
}

/// A deployment as the contract shows it, with whether its worker runs here.
pub(super) fn deployment_view(s: &Shared, d: &DeploymentRecord) -> Value {
    let process = s.info.workers.as_ref().and_then(|w| {
        crate::workers::worker_of(&s.info.profile, &d.worker)
            .ok()
            .map(|p| w.process(&p))
    });
    json!({
        "worker": d.worker,
        "version": d.version,
        "state": d.state,
        "previous_version": d.previous_version,
        "canary": d.canary,
        "folders": d.folders,
        "budgets": d.budgets,
        "note": d.note,
        "at": d.at,
        "running": matches!(process, Some(Process::Running { .. })),
    })
}

/// The agent as the contract summarizes it (see [`super::factory`]'s full view).
fn summary(s: &Shared, name: &str) -> Result<Value, ApiError> {
    let store = FactoryStore::of(&s.info.profile);
    let state = store
        .state(name)
        .map_err(internal)?
        .ok_or_else(|| not_found(name))?;
    let title = store
        .draft(name)
        .ok()
        .flatten()
        .and_then(|d| {
            d.blueprint
                .get("title")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_default();
    Ok(json!({
        "name": name,
        "title": title,
        "state": state.state,
        "active_version": state.active_version,
        "versions": state.versions,
        "replacement": state.replacement,
        "deployments": state.deployments.iter().map(|d| deployment_view(s, d)).collect::<Vec<_>>(),
    }))
}

/// What a release asks for.
#[derive(Debug, Deserialize)]
pub(super) struct ReleaseBody {
    pub evaluation: String,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

pub(super) async fn request_release(
    State(s): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<ReleaseBody>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let profile = s.profile_now();
    let store = FactoryStore::of(&profile);
    known(&store, &name)?;
    let key = profile.identity().map_err(internal)?;
    let made = release::request(
        &store,
        &profile.name,
        &key,
        &name,
        &Asked {
            evaluation: body.evaluation,
            version: body.version,
            note: body.note,
            by: profile.name.clone(),
            ..Asked::default()
        },
    )
    .map_err(refused)?;
    let waiting: Vec<String> = made
        .pending()
        .into_iter()
        .map(|a| {
            if a.approver == release::OWNER {
                "your sign-off".to_owned()
            } else {
                a.approver
            }
        })
        .collect();
    s.system(
        "info",
        if waiting.is_empty() {
            format!("Released {name} {}", made.manifest.version)
        } else {
            format!(
                "Release {name} {} waits for {}",
                made.manifest.version,
                waiting.join(", ")
            )
        },
    );
    s.send(json!({ "type": "factory", "agent": name }));
    Ok((StatusCode::ACCEPTED, Json(release_view(&store, &made))))
}

pub(super) async fn get_release(
    State(s): State<AppState>,
    Path((name, version)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let store = FactoryStore::of(&s.info.profile);
    known(&store, &name)?;
    let found = release::load(&store, &name, &version).map_err(refused)?;
    let mut view = release_view(&store, &found);
    if let Some(previous) = found.manifest.previous.as_deref() {
        view["diff"] = json!(release::diff(&store, &name, previous, &version).unwrap_or_default());
    }
    if let Ok(report) = release::report_of(&store, &name, &version) {
        view["eval_summary"] = json!(release::eval_summary(&report));
    }
    Ok(Json(view))
}

/// What a deployment asks for.
#[derive(Debug, Default, Deserialize)]
pub(super) struct DeployBody {
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub folders: Option<Vec<String>>,
    #[serde(default)]
    pub start: Option<bool>,
    #[serde(default)]
    pub canary: Option<bool>,
}

pub(super) async fn deploy_agent(
    State(s): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<DeployBody>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let profile = s.profile_now();
    let store = FactoryStore::of(&profile);
    known(&store, &name)?;
    let supervisor = s.info.workers.clone().ok_or_else(|| {
        refused(crate::factory::conflict(
            "this agent can't run digital workers (a digital worker can't own workers)",
        ))
    })?;
    let start = body.start.unwrap_or(true);
    let canary = body.canary.unwrap_or(true);
    let placed = deploy::deploy(
        &profile,
        s.agent.addr(),
        &s.agent.trusted(),
        &name,
        &DeployAsk {
            version: body.version,
            folders: body.folders,
            canary,
            start_with_owner: start.then_some(true),
            by: profile.name.clone(),
        },
    )
    .map_err(refused)?;
    if let Some(entry) = &placed.entry {
        s.agent.allow(entry.clone());
        s.save_trust()?;
        s.broadcast_peers();
    }
    let worker = placed.record.worker.clone();
    if start {
        // Started again, so it runs with what it was just given (its memory and role).
        supervisor.stop(&worker).await;
        supervisor
            .start(&worker)
            .map_err(|e| refused(crate::factory::conflict(format!("{e:#}"))))?;
    }
    s.system(
        "info",
        format!(
            "Deployed {name} {} on worker {worker}{}",
            placed.record.version,
            if canary && start {
                ": its canary runs first"
            } else {
                ""
            }
        ),
    );
    s.send(json!({ "type": "factory", "agent": name }));
    Ok((
        StatusCode::ACCEPTED,
        Json(deployment_view(&s, &placed.record)),
    ))
}

/// What a rollback asks for.
#[derive(Debug, Deserialize)]
pub(super) struct RollbackBody {
    #[serde(default)]
    pub to_version: Option<String>,
    pub reason: String,
}

pub(super) async fn rollback_agent(
    State(s): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<RollbackBody>,
) -> Result<Json<Value>, ApiError> {
    let profile = s.profile_now();
    let store = FactoryStore::of(&profile);
    known(&store, &name)?;
    let was = store
        .state(&name)
        .ok()
        .flatten()
        .and_then(|st| st.active_version);
    let (state, workers) = deploy::rollback(
        &profile,
        &name,
        body.to_version.as_deref(),
        &body.reason,
        &profile.name,
    )
    .map_err(refused)?;
    // Its workers take the earlier release within seconds; those running here start again,
    // for its memory and role too.
    if let Some(supervisor) = &s.info.workers {
        for worker in &workers {
            if let Ok(p) = crate::workers::worker_of(&profile, worker)
                && matches!(supervisor.process(&p), Process::Running { .. })
            {
                supervisor.stop(worker).await;
                let _ = supervisor.start(worker);
            }
        }
    }
    let now = state.active_version.as_deref().unwrap_or("none");
    let version = |v: &str| v.parse::<release::Version>().ok();
    // Back to an earlier release, or forward again to a newer one after a rollback.
    let way = match (was.as_deref().and_then(version), version(now)) {
        (Some(was), Some(now)) if now > was => "forward",
        _ => "back",
    };
    s.system(
        "warn",
        format!("Rolled {name} {way} to {now}: {}", body.reason.trim()),
    );
    s.send(json!({ "type": "factory", "agent": name }));
    Ok(Json(summary(&s, &name)?))
}

pub(super) async fn set_lifecycle(
    State(s): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<LifecycleRequest>,
) -> Result<Json<Value>, ApiError> {
    let profile = s.profile_now();
    known(&FactoryStore::of(&profile), &name)?;
    let (state, stopped) =
        lifecycle::set(&profile, &name, &body, &profile.name).map_err(refused)?;
    if let Some(supervisor) = &s.info.workers {
        for worker in &stopped {
            supervisor.stop(worker).await;
        }
    }
    s.system("info", format!("{name} is {} now", state.state));
    s.send(json!({ "type": "factory", "agent": name }));
    Ok(Json(summary(&s, &name)?))
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct Since {
    #[serde(default)]
    pub since: Option<String>,
}

pub(super) async fn get_metrics(
    State(s): State<AppState>,
    Path(name): Path<String>,
    Query(query): Query<Since>,
) -> Result<Json<Value>, ApiError> {
    let profile = s.profile_now();
    known(&FactoryStore::of(&profile), &name)?;
    let since = metrics::parse_since(query.since.as_deref().unwrap_or("7d"), chrono::Utc::now())
        .map_err(|e| super::bad_request(format!("{e:#}")))?;
    let found = metrics::metrics(&profile, &name, since).map_err(refused)?;
    Ok(Json(serde_json::to_value(found).map_err(internal)?))
}

pub(super) async fn set_budgets(
    State(s): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<Budgets>,
) -> Result<Json<Value>, ApiError> {
    let profile = s.profile_now();
    known(&FactoryStore::of(&profile), &name)?;
    let now = deploy::set_budgets(&profile, &name, body, &profile.name).map_err(refused)?;
    s.system(
        "info",
        format!("Budgets of {name}: {}", release::budgets_text(&now)),
    );
    s.send(json!({ "type": "factory", "agent": name }));
    Ok(Json(serde_json::to_value(now).map_err(internal)?))
}

/// Starts the factory's drivers for the owner's agent: sign-off requests for waiting
/// releases, and the canaries of new deployments. A digital worker owns no factory.
pub(super) fn start(s: &AppState) {
    let profile = &s.info.profile;
    if profile.config.owner.is_some() {
        return;
    }
    let shared = s.clone();
    let changed: Arc<dyn Fn(&str) + Send + Sync> =
        Arc::new(move |name: &str| shared.send(json!({ "type": "factory", "agent": name })));
    crate::factory::signoff::spawn(FactoryStore::of(profile), s.agent.clone(), changed.clone());
    if let (Some(supervisor), Ok(key)) = (s.info.workers.clone(), profile.identity()) {
        let host = Arc::new(Host {
            owner: profile.clone(),
            agent: s.agent.clone(),
            key,
            supervisor,
            ready_wait: crate::factory::runner::READY_WAIT,
        });
        tokio::spawn(crate::factory::manage::drive_canaries(host, changed));
    }
}
