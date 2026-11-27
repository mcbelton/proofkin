//! The web UI's Models dialog (see [`crate::models`]): the models and reasoning efforts
//! peers may ask this agent's providers for, and its defaults, picked from the models
//! its accounts offer; and the same for each of its digital workers, which use this
//! agent's settings for a provider unless they set their own. Changes apply at once:
//! this agent tells its peers, and a running digital worker applies its `profile.json`
//! when it changes (see [`crate::agent::Agent::watch_models`]).

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{ApiError, AppState, Shared, bad_request, internal};
use crate::agent::{OfferListStatus, OfferListing};
use crate::llm::{CopilotBackend, Executor, Provider};
use crate::models::{ModelChoice, Offered, Offers};
use crate::profile::Profile;

const PROVIDERS: [Provider; 3] = [Provider::Copilot, Provider::Claude, Provider::Codex];
pub(super) const LISTS_MODELS: &[Provider] = &[Provider::Copilot];

/// A provider's name as the page shows it.
fn label(provider: Provider) -> &'static str {
    match provider {
        Provider::Copilot => "Copilot",
        Provider::Claude => "Claude",
        Provider::Codex => "Codex",
    }
}

/// The providers this agent's settings are for: the ones it runs, else (it only
/// delegates) the ones its account is signed in to, which its digital workers use.
fn own_providers(s: &Shared, me: &Profile) -> Vec<Provider> {
    if !s.info.providers.is_empty() {
        return s.info.providers.clone();
    }
    PROVIDERS
        .into_iter()
        .filter(|p| me.is_configured(*p))
        .collect()
}

/// Lists with the running executor, or starts a Copilot just to get its account's list.
async fn list_provider(
    executor: Option<Arc<dyn Executor>>,
    me: Profile,
    provider: Provider,
) -> anyhow::Result<Vec<Offered>> {
    if let Some(executor) = executor
        && executor.providers().contains(&provider)
    {
        return executor.list_models(provider).await;
    }
    match provider {
        Provider::Copilot => {
            let creds = tokio::task::spawn_blocking(move || me.copilot_credentials()).await??;
            CopilotBackend::list_models_with(creds).await
        }
        Provider::Claude | Provider::Codex => Ok(Vec::new()),
    }
}

/// Starts a missing list, or joins the one already running for this provider.
fn start_listing(shared: &Shared, me: &Profile, provider: Provider) -> Option<OfferListing> {
    if !LISTS_MODELS.contains(&provider) {
        return None;
    }
    let (listing, run) = shared.agent.begin_offer_listing(provider, true)?;
    if let Some(run) = run {
        let fetch = list_provider(shared.executor.clone(), me.clone(), provider);
        tokio::spawn(async move { run.finish(fetch.await) });
    }
    Some(listing)
}

/// Describe starts the lists it needs without waiting for them. The last error stays
/// available until a list succeeds, including in the response that starts the retry.
pub(super) fn list_in_background(
    shared: &Shared,
    me: &Profile,
    providers: &[Provider],
) -> OfferListStatus {
    let mut status = OfferListStatus::default();
    for &provider in providers {
        if let Some(listing) = start_listing(shared, me, provider) {
            status.listing.push(provider);
            if let Some(error) = listing.previous_error {
                status.list_errors.insert(provider, error);
            }
        }
    }
    status
}

/// The Models dialog waits for the full list and shows any errors from that attempt.
async fn listed(
    shared: &Shared,
    me: &Profile,
    providers: &[Provider],
) -> (Offers, BTreeMap<Provider, String>) {
    let mut errors = BTreeMap::new();
    for &provider in providers {
        if let Some(listing) = start_listing(shared, me, provider)
            && let Err(error) = listing.wait().await
        {
            errors.insert(provider, error);
        }
    }
    let listed = shared
        .agent
        .listed()
        .into_iter()
        .filter(|(p, _)| providers.contains(p))
        .collect();
    (listed, errors)
}

/// A provider's settings, as the dialog saves them.
#[derive(Deserialize)]
pub(super) struct ChoiceBody {
    provider: Provider,
    /// A digital worker's: use its owner's settings for this provider.
    #[serde(default)]
    inherit: bool,
    #[serde(flatten)]
    choice: ModelChoice,
}

fn refused(e: anyhow::Error) -> ApiError {
    bad_request(format!("{e:#}"))
}

/// This agent's settings, its providers and the models they offer.
pub(super) async fn get_own(State(s): State<AppState>) -> Result<Json<Value>, ApiError> {
    let me = s.info.profile.reload().map_err(internal)?;
    let providers = own_providers(&s, &me);
    let (listed, errors) = listed(&s, &me, &providers).await;
    Ok(Json(json!({
        "name": s.agent.name(),
        "providers": providers,
        "models": me.config.models,
        "listed": listed,
        "errors": errors,
        // Its digital workers use these unless they set their own.
        "owns_workers": s.info.workers.is_some(),
    })))
}

/// Sets one of this agent's providers' settings: saved, and applied at once (its tasks,
/// its peers, and its digital workers that use them).
pub(super) async fn put_own(
    State(s): State<AppState>,
    Json(body): Json<ChoiceBody>,
) -> Result<Json<Value>, ApiError> {
    let mut me = s.info.profile.reload().map_err(internal)?;
    if !own_providers(&s, &me).contains(&body.provider) {
        return Err(bad_request(format!(
            "{} isn't one of this agent's providers",
            label(body.provider)
        )));
    }
    crate::models::set_choice(&mut me.config.models, body.provider, &body.choice)
        .map_err(refused)?;
    me.save().map_err(internal)?;
    let models = me.models();
    if let Some(executor) = &s.executor {
        executor.set_models(models.clone());
    }
    s.agent.set_models(models);
    let now = crate::models::choice(&me.config.models, body.provider);
    s.system(
        "info",
        format!(
            "{} models changed: {}",
            label(body.provider),
            crate::models::describe(&now)
        ),
    );
    Ok(Json(json!({ "models": me.config.models })))
}

fn worker_of(s: &Shared, name: &str) -> Result<(Profile, Profile), ApiError> {
    let me = s.info.profile.reload().map_err(internal)?;
    let worker = crate::workers::worker_of(&me, name).map_err(refused)?;
    Ok((me, worker))
}

/// A digital worker's own settings, its owner's (which it uses for a provider it sets
/// nothing for), and the models its providers offer (its owner's accounts).
pub(super) async fn get_worker(
    State(s): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let (me, worker) = worker_of(&s, &name)?;
    let providers = worker.config.providers.clone();
    let (listed, errors) = listed(&s, &me, &providers).await;
    Ok(Json(json!({
        "name": worker.name,
        "owner": me.name,
        "providers": providers,
        "models": worker.config.models,
        "owner_models": me.config.models,
        "listed": listed,
        "errors": errors,
        "running": worker.is_running(),
    })))
}

/// Sets one of a digital worker's providers' settings (or has it use its owner's); a
/// running worker applies them within seconds.
pub(super) async fn put_worker(
    State(s): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<ChoiceBody>,
) -> Result<Json<Value>, ApiError> {
    let (me, mut worker) = worker_of(&s, &name)?;
    if !worker.config.providers.contains(&body.provider) {
        return Err(bad_request(format!(
            "{name} doesn't run {}",
            label(body.provider)
        )));
    }
    let said = if body.inherit {
        worker.config.models.remove(&body.provider);
        format!(
            "{name} uses {}'s {} models again",
            me.name,
            label(body.provider)
        )
    } else {
        crate::models::set_choice(&mut worker.config.models, body.provider, &body.choice)
            .map_err(refused)?;
        let now = crate::models::choice(&worker.config.models, body.provider);
        format!(
            "{name}'s {} models changed: {}",
            label(body.provider),
            crate::models::describe(&now)
        )
    };
    worker.save().map_err(internal)?;
    s.system("info", said);
    s.send(json!({ "type": "workers" }));
    Ok(Json(json!({ "models": worker.config.models })))
}
