//! An agent's lifecycle after its first release (docs/agent-factory.md, section 9): it may
//! be deprecated in favor of another agent (plan checks warn and name it), reactivated, and
//! retired (plan checks refuse it, its deployments stop, and its releases and reports stay
//! for audit). A retirement with `purge` deletes them once the retention has passed.

use anyhow::Result;
use chrono::{DateTime, Utc};

use crate::factory::deploy::{self, RELEASE_DIR};
use crate::factory::store::{AgentState, DeployState, FactoryStore, Lifecycle, now};
use crate::factory::{conflict, invalid, not_found};
use crate::profile::Profile;

/// How long a purged agent's releases and reports are kept after it was retired.
pub const PURGE_AFTER_DAYS: i64 = 90;
/// The longest reason.
pub const MAX_REASON: usize = 500;

fn reason_of(reason: Option<&str>) -> Result<Option<String>> {
    let reason = reason.map(str::trim).filter(|r| !r.is_empty());
    if reason.is_some_and(|r| r.chars().count() > MAX_REASON) {
        return Err(invalid(format!(
            "a reason is at most {MAX_REASON} characters"
        )));
    }
    Ok(reason.map(str::to_owned))
}

fn state_of(store: &FactoryStore, name: &str) -> Result<AgentState> {
    store
        .state(name)?
        .ok_or_else(|| not_found(format!("the factory has no agent '{name}'")))
}

/// Deprecates agent `name` of `owner`'s in favor of `replacement` (an agent plans can use):
/// plan checks warn about it and name its replacement; it keeps working.
pub fn deprecate(
    owner: &Profile,
    name: &str,
    replacement: &str,
    reason: Option<&str>,
    by: &str,
) -> Result<AgentState> {
    let store = FactoryStore::of(owner);
    let state = state_of(&store, name)?;
    let reason = reason_of(reason)?;
    match state.state {
        Lifecycle::Active | Lifecycle::Deprecated => {}
        Lifecycle::Retired => return Err(invalid(format!("{name} is retired already"))),
        other => {
            return Err(invalid(format!(
                "{name} is {other}: only a released agent is deprecated"
            )));
        }
    }
    if replacement == name {
        return Err(invalid("an agent can't replace itself"));
    }
    crate::profile::validate_name(replacement)
        .map_err(|_| invalid(format!("{replacement:?} isn't an agent's name")))?;
    owner.skills().agent(replacement).map_err(|e| {
        invalid(format!(
            "its replacement must be an agent plans can use: {e:#}"
        ))
    })?;
    store.update_state(name, |s| {
        s.state = Lifecycle::Deprecated;
        s.replacement = Some(replacement.to_owned());
        s.changed(
            by,
            format!("deprecated: use {replacement}"),
            reason.as_deref(),
        );
        Ok(())
    })
}

/// Makes deprecated agent `name` active again.
pub fn reactivate(
    owner: &Profile,
    name: &str,
    reason: Option<&str>,
    by: &str,
) -> Result<AgentState> {
    let store = FactoryStore::of(owner);
    let state = state_of(&store, name)?;
    let reason = reason_of(reason)?;
    match state.state {
        Lifecycle::Deprecated => {}
        Lifecycle::Active => return Err(invalid(format!("{name} is active already"))),
        Lifecycle::Retired => {
            return Err(invalid("a retired agent can't become active again"));
        }
        other => {
            return Err(invalid(format!(
                "{name} is {other}: only a deprecated agent is reactivated"
            )));
        }
    }
    store.update_state(name, |s| {
        s.state = Lifecycle::Active;
        s.replacement = None;
        s.changed(by, "reactivated", reason.as_deref());
        Ok(())
    })
}

/// Retires agent `name`: plan checks refuse it, each of its deployments stops (its worker's
/// pin says so, and it no longer starts with its owner; whoever runs the worker stops it),
/// and its releases and reports are kept for audit; with `purge`, they are deleted once
/// [`PURGE_AFTER_DAYS`] have passed. Returns its state and the workers to stop.
pub fn retire(
    owner: &Profile,
    name: &str,
    reason: &str,
    purge: bool,
    by: &str,
) -> Result<(AgentState, Vec<String>)> {
    let store = FactoryStore::of(owner);
    let state = state_of(&store, name)?;
    let reason = reason_of(Some(reason))?.ok_or_else(|| invalid("say why it is retired"))?;
    if state.state == Lifecycle::Retired {
        return Err(invalid(format!("{name} is retired already")));
    }
    if state.versions.is_empty() {
        return Err(invalid(format!(
            "{name} has no release: delete its draft instead (proofkin factory delete {name} --profile {})",
            owner.name
        )));
    }
    let mut stopped = Vec::new();
    for deployment in state
        .deployments
        .iter()
        .filter(|d| d.state != DeployState::Stopped)
    {
        if let Ok(mut worker) = crate::workers::worker_of(owner, &deployment.worker) {
            if worker
                .dir
                .join(RELEASE_DIR)
                .join(deploy::PIN_FILE)
                .is_file()
            {
                deploy::set_pin_state(&worker, DeployState::Stopped)?;
            }
            worker.config.start_with_owner = false;
            worker.save()?;
        }
        stopped.push(deployment.worker.clone());
    }
    let purge_after = purge.then(|| {
        (Utc::now() + chrono::Duration::days(PURGE_AFTER_DAYS))
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    });
    let mut incident = None;
    let state = store.update_state(name, |s| {
        s.state = Lifecycle::Retired;
        s.purge_after = purge_after.clone();
        for d in s
            .deployments
            .iter_mut()
            .filter(|d| stopped.contains(&d.worker))
        {
            d.state = DeployState::Stopped;
            d.note = Some("retired".into());
            d.at = now();
        }
        s.changed(
            by,
            match &purge_after {
                Some(when) => format!("retired; its releases and reports go after {when}"),
                None => "retired; its releases and reports are kept".into(),
            },
            Some(&reason),
        );
        // Nothing runs it any more: nothing watches it, and no peer is offered it.
        if let Some(w) = s.watch.as_mut().filter(|w| w.enabled) {
            w.enabled = false;
            w.failures = 0;
            w.retry = None;
            incident = w.incident.take();
            s.changed(by, "drift watch paused: it was retired", None);
        }
        if s.publication.take().is_some_and(|p| !p.versions.is_empty()) {
            s.changed(by, "unpublished: it was retired", None);
        }
        Ok(())
    })?;
    if let Some(incident) = incident {
        owner
            .ops_store()
            .record_incident(&crate::ops::IncidentEvent {
                ts: crate::runs::unix_now(),
                incident,
                monitor: crate::factory::drift::monitor(name),
                kind: crate::ops::IncidentKind::Resolved,
                text: format!("Closed: {name} was retired"),
                checks: vec![],
            })?;
    }
    Ok((state, stopped))
}

/// What the lifecycle request asks for, as the web UI's API takes it.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct LifecycleRequest {
    pub state: String,
    #[serde(default)]
    pub replacement: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub purge: bool,
}

/// Moves agent `name` to the state `request` asks for; returns its state and the workers
/// to stop.
pub fn set(
    owner: &Profile,
    name: &str,
    request: &LifecycleRequest,
    by: &str,
) -> Result<(AgentState, Vec<String>)> {
    match request.state.as_str() {
        "deprecated" => {
            let replacement = request
                .replacement
                .as_deref()
                .filter(|r| !r.trim().is_empty())
                .ok_or_else(|| invalid("deprecating needs a replacement"))?;
            Ok((
                deprecate(owner, name, replacement, request.reason.as_deref(), by)?,
                Vec::new(),
            ))
        }
        "active" => Ok((
            reactivate(owner, name, request.reason.as_deref(), by)?,
            Vec::new(),
        )),
        "retired" => retire(
            owner,
            name,
            request.reason.as_deref().unwrap_or_default(),
            request.purge,
            by,
        ),
        other => Err(invalid(format!(
            "{other:?} isn't a state an agent moves to: deprecated, active or retired"
        ))),
    }
}

/// Deletes the releases, builds, evaluations and fixtures of retired agents whose retention
/// has passed at `at` (their state and history stay). Returns their names.
pub fn purge_due(store: &FactoryStore, at: DateTime<Utc>) -> Result<Vec<String>> {
    let mut purged = Vec::new();
    for name in store.names()? {
        let Some(state) = store.state(&name)? else {
            continue;
        };
        let due = state.state == Lifecycle::Retired
            && state
                .purge_after
                .as_deref()
                .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
                .is_some_and(|t| t <= at);
        if !due {
            continue;
        }
        let dir = store.agent_dir(&name);
        for part in ["releases", "builds", "evaluations", "fixtures"] {
            let path = dir.join(part);
            if path.exists() {
                std::fs::remove_dir_all(&path)?;
            }
        }
        store.update_state(&name, |s| {
            s.purge_after = None;
            s.versions.clear();
            s.active_version = None;
            s.changed("proofkin", "purged its releases and reports", None);
            Ok(())
        })?;
        purged.push(name);
    }
    Ok(purged)
}

/// Why a plan may not use agent `name` (retired), or what it should use instead
/// (deprecated): `(error, message)`.
pub fn plan_note(state: &AgentState) -> Option<(bool, String)> {
    let replacement = state
        .replacement
        .as_deref()
        .map(|r| format!(": use {r}"))
        .unwrap_or_default();
    match state.state {
        Lifecycle::Retired => Some((
            true,
            format!("agent {} is retired{replacement}", state.name),
        )),
        Lifecycle::Deprecated => Some((
            false,
            format!("agent {} is deprecated{replacement}", state.name),
        )),
        _ => None,
    }
}

/// Guards against a conflicting lifecycle move: kept for the API's 409 cases.
pub fn retired(name: &str) -> anyhow::Error {
    conflict(format!("{name} is retired"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::factory::release::tests::Fixture;
    use serde_json::json;

    #[test]
    fn retired_agents_are_purged_only_after_the_retention() {
        let f = Fixture::new();
        let e = f.evaluated(
            |v| {
                v["approvers"] = json!([]);
                v["capabilities"]["mcp"] = json!([]);
            },
            false,
        );
        f.release(&e.id, None).unwrap();
        f.store
            .update_state("fin-analyst", |s| {
                s.state = Lifecycle::Retired;
                s.purge_after = Some("2026-12-01T00:00:00Z".into());
                Ok(())
            })
            .unwrap();
        let before = DateTime::parse_from_rfc3339("2026-11-30T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert!(purge_due(&f.store, before).unwrap().is_empty());
        assert!(f.store.release_dir("fin-analyst", "1.0.0").exists());
        let after = DateTime::parse_from_rfc3339("2026-12-02T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(purge_due(&f.store, after).unwrap(), ["fin-analyst"]);
        assert!(!f.store.release_dir("fin-analyst", "1.0.0").exists());
        let state = f.store.state("fin-analyst").unwrap().unwrap();
        assert!(state.history.iter().any(|c| c.what.contains("purged")));
        assert_eq!(state.state, Lifecycle::Retired);
    }

    #[test]
    fn plans_are_warned_about_a_deprecated_agent_and_refused_a_retired_one() {
        let f = Fixture::new();
        let mut state = f.store.state("fin-analyst").unwrap().unwrap();
        assert_eq!(plan_note(&state), None);
        state.state = Lifecycle::Deprecated;
        state.replacement = Some("fin-analyst-2".into());
        assert_eq!(
            plan_note(&state),
            Some((
                false,
                "agent fin-analyst is deprecated: use fin-analyst-2".into()
            ))
        );
        state.state = Lifecycle::Retired;
        assert_eq!(
            plan_note(&state),
            Some((
                true,
                "agent fin-analyst is retired: use fin-analyst-2".into()
            ))
        );
    }
}
