//! The web UI's digital workers (see [`crate::workers`]): create one on this
//! profile's sign-ins with the folders it works in, start and stop it (it starts
//! and stops with this agent if its owner wants), and delete it. Workers run on
//! their owner's sign-ins, so this profile can also sign in to Copilot here, with
//! an account of the GitHub CLI.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{ApiError, AppState, Shared, bad_request, internal};
use crate::agent::PeerInfo;
use crate::auth::{LoginMethod, ProviderTools};
use crate::llm::Provider;
use crate::profile::{CopilotAuth, Profile};
use crate::shares::DATA_SHARE;
use crate::trust::TrustStore;
use crate::workers::{self, Process, Supervisor, WorkerSpec};

fn supervisor(s: &Shared) -> Result<Arc<Supervisor>, ApiError> {
    s.info.workers.clone().ok_or_else(|| {
        bad_request("this agent doesn't run digital workers (a digital worker can't own workers)")
    })
}

fn refused(e: anyhow::Error) -> ApiError {
    bad_request(format!("{e:#}"))
}

/// Which account `profile` uses for `provider`, in words.
fn account(profile: &Profile, provider: Provider) -> String {
    match (provider, &profile.config.copilot) {
        (Provider::Copilot, Some(CopilotAuth::Gh { user, host })) => format!("{user}@{host}"),
        (Provider::Copilot, Some(CopilotAuth::Token { host })) => format!("a token for {host}"),
        _ => "signed in".into(),
    }
}

fn worker_json(
    sup: &Supervisor,
    worker: &Profile,
    trusted: &TrustStore,
    peers: &[PeerInfo],
) -> Value {
    // Its id is on its owner's allowlist: no need to open its secrets.
    let id = trusted.get_by_name(&worker.name).map(|t| t.id);
    json!({
        "name": worker.name,
        "id": id.map(|id| id.to_string()),
        "role": worker.config.role,
        "providers": worker.config.providers,
        // Its own model settings (for the providers it sets nothing for: its owner's).
        "models": worker.config.models,
        "folders": workers::folders(worker),
        "start_with_owner": worker.config.start_with_owner,
        // For a deployed factory agent: the release its owner pinned on it.
        "release": crate::factory::deploy::pin_of(worker).map(|p| json!({ "agent": p.agent, "version": p.version, "state": p.state })),
        "process": sup.process(worker),
        "online": id.is_some_and(|id| peers.iter().any(|p| p.id == id)),
        "data_folder": worker.data_folder().display().to_string(),
    })
}

#[derive(Deserialize)]
pub(super) struct ListQuery {
    /// Also the GitHub CLI's accounts, to sign in to Copilot with (slower).
    #[serde(default)]
    accounts: bool,
}

/// This profile's workers, what they could run on (its sign-ins) and the folders
/// they could work in (its shared folders).
pub(super) async fn list(
    State(s): State<AppState>,
    Query(query): Query<ListQuery>,
) -> Result<Json<Value>, ApiError> {
    let me = s.info.profile.reload().map_err(internal)?;
    let signed_in: Vec<Value> = [Provider::Copilot, Provider::Claude, Provider::Codex]
        .into_iter()
        .filter(|p| me.is_configured(*p))
        .map(|p| json!({ "provider": p, "account": account(&me, p) }))
        .collect();
    let folders: Vec<Value> = me
        .shares()
        .map_err(internal)?
        .shares
        .iter()
        .filter(|f| f.name != DATA_SHARE)
        .map(|f| json!({ "name": f.name, "path": f.path.display().to_string(), "max_access": f.max_access }))
        .collect();
    let can_own = me.config.owner.is_none() && s.info.workers.is_some();
    // To sign in to Copilot here: the GitHub CLI's accounts.
    let gh_accounts: Vec<Value> =
        if query.accounts && can_own && !me.is_configured(Provider::Copilot) {
            tokio::task::spawn_blocking(crate::github::gh_accounts)
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|a| json!({ "login": a.login, "host": a.host, "active": a.active }))
                .collect()
        } else {
            Vec::new()
        };
    let list = match &s.info.workers {
        Some(sup) => {
            let (trusted, peers) = (s.agent.trusted(), s.agent.peers());
            workers::workers_of(&me)
                .map_err(internal)?
                .iter()
                .map(|w| worker_json(sup, w, &trusted, &peers))
                .collect()
        }
        None => Vec::new(),
    };
    Ok(Json(json!({
        "profile": me.name,
        "owner": me.config.owner,
        "can_own": can_own,
        "signed_in": signed_in,
        "gh_accounts": gh_accounts,
        "folders": folders,
        "workers": list,
    })))
}

/// Creates a worker; one that starts with this agent starts now.
pub(super) async fn create(
    State(s): State<AppState>,
    Json(spec): Json<WorkerSpec>,
) -> Result<Json<Value>, ApiError> {
    let sup = supervisor(&s)?;
    let me = s.info.profile.reload().map_err(internal)?;
    let created =
        workers::create(&me, s.agent.addr(), &s.agent.trusted(), &spec).map_err(refused)?;
    let name = created.worker.name.clone();
    s.agent.allow(created.entry);
    s.save_trust()?;
    let started = spec.start_with_owner.then(|| sup.start(&name));
    s.system(
        "info",
        format!(
            "Created digital worker {name}{}",
            match &started {
                Some(Ok(())) => ", starting it",
                Some(Err(_)) => ", but it didn't start",
                None => "",
            }
        ),
    );
    s.broadcast_peers();
    s.send(json!({ "type": "workers" }));
    Ok(Json(json!({
        "name": name,
        "id": created.id.to_string(),
        "started": started.as_ref().map(Result::is_ok),
        "error": started.and_then(Result::err).map(|e| format!("{e:#}")),
    })))
}

pub(super) async fn start(
    State(s): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    supervisor(&s)?.start(&name).map_err(refused)?;
    s.system("info", format!("Starting digital worker {name}"));
    Ok(Json(json!({ "started": name })))
}

pub(super) async fn stop(
    State(s): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    if !supervisor(&s)?.stop(&name).await {
        return Err(bad_request(format!(
            "'{name}' isn't running, or wasn't started by this agent"
        )));
    }
    s.system("info", format!("Stopped digital worker {name}"));
    Ok(Json(json!({ "stopped": name })))
}

#[derive(Deserialize)]
pub(super) struct EditBody {
    start_with_owner: bool,
}

/// Whether a worker starts (and stops) with this agent.
pub(super) async fn edit(
    State(s): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<EditBody>,
) -> Result<Json<Value>, ApiError> {
    supervisor(&s)?;
    let me = s.info.profile.reload().map_err(internal)?;
    let mut worker = workers::worker_of(&me, &name).map_err(refused)?;
    worker.config.start_with_owner = body.start_with_owner;
    worker.save().map_err(internal)?;
    s.send(json!({ "type": "workers" }));
    Ok(Json(
        json!({ "name": name, "start_with_owner": body.start_with_owner }),
    ))
}

/// Stops a worker this agent runs, deletes it (ending a factory agent's deployment on it),
/// and takes it off the allowlist and out of the squads. Refused while it runs outside
/// this agent, or when its process didn't end once stopped.
pub(super) async fn delete(
    State(s): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let sup = supervisor(&s)?;
    let me = s.info.profile.reload().map_err(internal)?;
    let worker = workers::worker_of(&me, &name).map_err(refused)?;
    let conflict = |message: String| (StatusCode::CONFLICT, Json(json!({ "error": message })));
    if sup.process(&worker) == Process::Elsewhere {
        return Err(conflict(format!(
            "'{name}' is running outside this agent: stop it there (/quit or Ctrl+C), then delete it"
        )));
    }
    if sup.stop(&name).await && worker.is_running() {
        return Err(conflict(format!(
            "'{name}' was stopped, but its process didn't end, so it isn't deleted: try again in a moment"
        )));
    }
    let ended = workers::delete(&me, &name).map_err(|e| conflict(format!("{e:#}")))?;
    if s.agent.remove_trusted(&name) {
        s.save_trust()?;
    }
    let left = {
        let mut squads = s.squads.lock().unwrap();
        let left = squads.remove_member(&name);
        if !left.is_empty() {
            squads.save(&me.squads_path()).map_err(internal)?;
        }
        left
    };
    s.system(
        "info",
        format!(
            "Deleted digital worker {name}{}{}",
            if left.is_empty() {
                String::new()
            } else {
                format!("; it left squad(s) {}", left.join(", "))
            },
            if ended.is_empty() {
                String::new()
            } else {
                format!("; its deployment of {} ended", ended.join(", "))
            }
        ),
    );
    s.broadcast_peers();
    s.send(json!({ "type": "workers" }));
    for agent in &ended {
        s.send(json!({ "type": "factory", "agent": agent }));
    }
    Ok(Json(
        json!({ "deleted": name, "squads": left, "deployments_ended": ended }),
    ))
}

#[derive(Deserialize)]
pub(super) struct SignInBody {
    user: String,
    host: String,
}

/// Signs this profile in to Copilot with a GitHub CLI account (checked before it is
/// kept), so its workers can run Copilot.
pub(super) async fn sign_in_copilot(
    State(s): State<AppState>,
    Json(body): Json<SignInBody>,
) -> Result<Json<Value>, ApiError> {
    let mut me = s.info.profile.reload().map_err(internal)?;
    me.check_can_sign_in().map_err(refused)?;
    let tools = ProviderTools::new(&crate::setup::resolve_bridge(None));
    let method = LoginMethod::GhUser {
        user: body.user,
        host: body.host,
    };
    let account = tools
        .login(&mut me, Provider::Copilot, method)
        .await
        .map_err(refused)?;
    s.system(
        "info",
        format!("Signed {} in to Copilot as {account}", me.name),
    );
    s.send(json!({ "type": "workers" }));
    Ok(Json(json!({ "account": account })))
}
