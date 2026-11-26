//! The web UI's API for sharing agents between peers (docs/agent-factory.md, section 13), as
//! its contract says (docs/agent-factory/spec/openapi.json): publishing an agent's releases
//! to peers on the allowlist, the catalog of what peers publish to this profile, and
//! importing from it (verified, nothing run). Behind the web UI's token.

use std::time::Duration;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};

use super::releases::{known, refused};
use super::{ApiError, AppState, bad_request, internal};
use crate::factory::FactoryStore;
use crate::factory::catalog::{self, ImportAsk, ImportState, Shared};

/// How long an import waits for the publisher's bundle.
const BUNDLE_WAIT: Duration = Duration::from_secs(120);

pub(super) async fn publish_agent(
    State(s): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<Shared>,
) -> Result<Json<Value>, ApiError> {
    let profile = s.profile_now();
    let store = FactoryStore::of(&profile);
    known(&store, &name)?;
    let mut trust = s.agent.trusted();
    let (shared, changed) = catalog::publish(
        &store,
        &mut trust,
        &name,
        &body.peers,
        &body.versions,
        &profile.name,
    )
    .map_err(refused)?;
    // The running agent holds the allowlist: it takes the grants, and tells the peers.
    for entry in changed {
        s.agent.allow(entry);
    }
    s.save_trust()?;
    s.agent.refresh_catalog();
    s.system(
        "info",
        if shared.versions.is_empty() || shared.peers.is_empty() {
            format!("{name} is unpublished: no peer sees it in their catalog")
        } else {
            format!(
                "Published {name} {} to {}",
                shared.versions.join(", "),
                shared.peers.join(", ")
            )
        },
    );
    s.send(json!({ "type": "factory", "agent": name }));
    s.broadcast_peers();
    Ok(Json(json!(shared)))
}

pub(super) async fn list_catalog(State(s): State<AppState>) -> Json<Value> {
    let store = FactoryStore::of(&s.info.profile);
    let published = s.agent.catalogs();
    let entries: Vec<Value> = catalog::listed(&store, &published)
        .into_iter()
        .map(|(entry, here)| {
            let mut value = json!(entry);
            value["here"] = json!(here);
            value
        })
        .collect();
    Json(json!({ "entries": entries }))
}

/// What an import asks for, as the contract has it.
#[derive(Debug, Deserialize)]
pub(super) struct ImportBody {
    peer: String,
    name: String,
    version: String,
    #[serde(default)]
    accept_publisher_report: bool,
}

pub(super) async fn import_agent(
    State(s): State<AppState>,
    Json(body): Json<ImportBody>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    if !crate::profile::name_ok(&body.name)
        || body
            .version
            .parse::<crate::factory::release::Version>()
            .is_err()
    {
        return Err(bad_request("name an agent and its version, like 1.2.0"));
    }
    let trust = s.agent.trusted();
    let peer = trust
        .get_by_name(&body.peer)
        .cloned()
        .ok_or_else(|| bad_request(format!("'{}' isn't on your allowlist", body.peer)))?;
    if !s.agent.peers().iter().any(|p| p.id == peer.id) {
        return Err(bad_request(format!(
            "{} isn't connected: its agent must run for you to import from it",
            body.peer
        )));
    }
    s.system(
        "info",
        format!("Asking {} for {} {}", body.peer, body.name, body.version),
    );
    let bundle = s
        .agent
        .request_bundle(peer.id, &body.name, &body.version, BUNDLE_WAIT)
        .await
        .map_err(|e| bad_request(format!("{e:#}")))?;
    if bundle.agent != body.name || bundle.version != body.version {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "error": format!(
                "{} sent {} {}, not {} {}",
                body.peer, bundle.agent, bundle.version, body.name, body.version
            ) })),
        ));
    }
    let profile = s.profile_now();
    let key = profile.identity().map_err(internal)?;
    let imported = catalog::import(
        &FactoryStore::of(&profile),
        &profile.name,
        &key,
        bundle,
        &ImportAsk {
            publisher: peer.name.clone(),
            publisher_id: peer.id.to_string(),
            accept_publisher_report: body.accept_publisher_report,
            by: profile.name.clone(),
        },
    )
    .map_err(refused)?;
    s.system(
        "info",
        format!(
            "Imported {} {} from {}: verified, nothing ran; {}",
            imported.name,
            imported.version,
            peer.name,
            match (imported.state, &imported.release) {
                (ImportState::Active, _) => "it is active (you accepted its publisher's report)",
                (_, Some(_)) => "it waits for your sign-off of what it asks for",
                _ => "evaluate it on your accounts to make it active",
            }
        ),
    );
    s.send(json!({ "type": "factory", "agent": imported.name }));
    s.send(json!({ "type": "catalog" }));
    Ok((StatusCode::CREATED, Json(json!(imported))))
}
