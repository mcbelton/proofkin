//! The web UI's "Plugins & tools" API: import plugins (fetched and reviewed before
//! anything is kept), offer MCP servers in shared folders (by hand, from an MCP
//! configuration or from a plugin) and pause, change or remove them, and add or
//! remove the profile's own skills. Values servers need (keys, tokens) are kept in
//! the profile and never sent back to the page.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{ApiError, AppState, Shared, bad_request, internal};
use crate::llm::Access;
use crate::mcp::{Fill, McpServer, McpTemplate};
use crate::plugins::{Fetched, Inspection, Plugin, Source};
use crate::shares::{DATA_SHARE, McpOffer, ShareStore};

/// How long a fetched plugin waits to be imported, and how many may wait.
const STAGED_FOR: Duration = Duration::from_secs(30 * 60);
const MAX_STAGED: usize = 8;

struct Staged {
    at: Instant,
    fetched: Arc<Fetched>,
    /// The marketplace a plugin was picked from: its files may be in its checkout.
    _market: Option<Arc<Fetched>>,
}

/// Plugins and marketplaces fetched for review. Their temporary files go when they
/// are imported, replaced or expire.
#[derive(Default)]
pub(super) struct Staging(Mutex<HashMap<String, Staged>>);

impl Staging {
    fn put(&self, fetched: Arc<Fetched>, market: Option<Arc<Fetched>>) -> String {
        let token = data_encoding::HEXLOWER.encode(&iroh::SecretKey::generate().to_bytes()[..12]);
        let mut map = self.0.lock().unwrap();
        map.retain(|_, s| s.at.elapsed() < STAGED_FOR);
        while map.len() >= MAX_STAGED {
            let oldest = map.iter().min_by_key(|(_, s)| s.at).map(|(k, _)| k.clone());
            match oldest {
                Some(k) => map.remove(&k),
                None => break,
            };
        }
        map.insert(
            token.clone(),
            Staged {
                at: Instant::now(),
                fetched,
                _market: market,
            },
        );
        token
    }

    fn get(&self, token: &str) -> Option<Arc<Fetched>> {
        let map = self.0.lock().unwrap();
        map.get(token)
            .filter(|s| s.at.elapsed() < STAGED_FOR)
            .map(|s| s.fetched.clone())
    }

    fn take(&self, token: &str) -> Option<Staged> {
        self.0
            .lock()
            .unwrap()
            .remove(token)
            .filter(|s| s.at.elapsed() < STAGED_FOR)
    }
}

fn err400(e: anyhow::Error) -> ApiError {
    bad_request(format!("{e:#}"))
}

fn expired() -> ApiError {
    bad_request("that review expired: fetch the plugin again")
}

/// A plugin for the page: what it holds, how to use its skills in plans and where
/// its MCP servers are offered.
fn plugin_json(
    p: &Plugin,
    skills: Option<&crate::skills::Skills>,
    shares: Option<&ShareStore>,
) -> Value {
    let listed = skills.map(|s| s.list()).unwrap_or_default();
    let agents = skills.map(|s| s.agents()).unwrap_or_default();
    json!({
        "name": p.name,
        "version": p.version,
        "description": p.description,
        "format": p.format,
        "source": p.source,
        "commit": p.commit,
        "enabled": p.enabled,
        "installed_unix": p.installed_unix,
        "updated_unix": p.updated_unix,
        "skills": p.skills.iter().map(|s| {
            let short = skills.is_none()
                || listed.iter().any(|l| l.name == s.name && l.plugin.as_deref() == Some(p.name.as_str()));
            json!({
                "name": s.name,
                "description": s.description,
                "use": if short { s.name.clone() } else { format!("{}.{}", p.name, s.name) },
            })
        }).collect::<Vec<_>>(),
        "agents": p.agents.iter().map(|a| {
            let short = skills.is_none()
                || agents.iter().any(|l| l.name == a.name && l.plugin.as_deref() == Some(p.name.as_str()));
            json!({
                "name": a.name,
                "description": a.description,
                "skills": a.skills,
                "mcp": a.mcp,
                "use": if short { a.name.clone() } else { format!("{}.{}", p.name, a.name) },
            })
        }).collect::<Vec<_>>(),
        "mcp": p.mcp.iter().map(|(name, t)| json!({
            "name": name,
            "describe": t.describe(),
            "inputs": t.inputs,
            "tools": t.tools,
            "offered": shares.map(|sh| offered_in(sh, &p.name, name)).unwrap_or_default(),
        })).collect::<Vec<_>>(),
        "notes": p.notes,
    })
}

fn offered_in(shares: &ShareStore, plugin: &str, server: &str) -> Vec<Value> {
    shares
        .shares
        .iter()
        .flat_map(|share| {
            share
                .mcp
                .iter()
                .filter(|(_, d)| {
                    d.from
                        .as_ref()
                        .is_some_and(|f| f.plugin == plugin && f.server == server)
                })
                .map(|(name, d)| json!({ "share": share.name, "name": name, "paused": d.paused }))
        })
        .collect()
}

/// An offered server for the page, without its values.
fn server_json(name: &str, d: &McpServer) -> Value {
    json!({
        "name": name,
        "describe": d.describe(),
        "access": d.access,
        "tools": d.tools,
        "description": d.description,
        "paused": d.paused,
        "plugin_off": d.plugin_off,
        "remote": d.url.is_some(),
        "plugin": d.from.as_ref().map(|f| json!({
            "plugin": f.plugin,
            "server": f.server,
            "values": f.values.keys().collect::<Vec<_>>(),
        })),
    })
}

/// Gives the saved `shares` to the agent (peers learn what changed) and refreshes the page.
fn shares_changed(s: &Shared, shares: ShareStore, note: String) {
    s.agent.set_shares(shares);
    s.system("info", note);
    s.send(json!({ "type": "tools" }));
}

pub(super) async fn list_plugins(State(s): State<AppState>) -> Result<Json<Value>, ApiError> {
    let profile = &s.info.profile;
    let plugins = profile.plugins().list().map_err(internal)?;
    let (skills, shares) = (profile.skills(), s.agent.shares());
    Ok(Json(json!({
        "plugins": plugins.iter().map(|p| plugin_json(p, Some(&skills), Some(&shares))).collect::<Vec<_>>(),
    })))
}

#[derive(Deserialize)]
pub(super) struct PreviewBody {
    #[serde(default)]
    source: String,
    /// A marketplace fetched before, to pick `plugin` from.
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    plugin: Option<String>,
}

/// Fetches a plugin (or a marketplace's list of plugins) for review; nothing is
/// kept until it is imported with the returned token.
pub(super) async fn preview_plugin(
    State(s): State<AppState>,
    Json(body): Json<PreviewBody>,
) -> Result<Json<Value>, ApiError> {
    let profile = s.info.profile.reload().map_err(internal)?;
    let store = profile.plugins();
    let creds = profile.github().ok();
    let listing = match body.token.as_deref() {
        Some(token) => s.staging.get(token).ok_or_else(expired)?,
        None => {
            let source = Source::parse(&body.source).map_err(err400)?;
            let fetched = crate::plugins::fetch(&source, &store.staging(), creds.as_ref())
                .await
                .map_err(err400)?;
            Arc::new(fetched)
        }
    };
    let pick = body.plugin.filter(|p| !p.trim().is_empty());
    match crate::plugins::inspect(&listing.root).map_err(err400)? {
        Inspection::Plugin(_) if pick.is_some() => {
            Err(bad_request("this is a plugin, not a marketplace"))
        }
        Inspection::Plugin(p) => {
            let exists = store.get(&p.name).is_ok();
            let token = s.staging.put(listing, None);
            Ok(Json(json!({
                "kind": "plugin",
                "token": token,
                "exists": exists,
                "plugin": plugin_json(&p, None, None),
            })))
        }
        Inspection::Marketplace { name, plugins } => {
            let Some(pick) = pick else {
                let token = s.staging.put(listing, None);
                return Ok(Json(json!({
                    "kind": "marketplace",
                    "token": token,
                    "name": name,
                    "plugins": plugins,
                })));
            };
            let entry = plugins.iter().find(|e| e.name == pick).ok_or_else(|| {
                bad_request(format!("marketplace '{name}' has no plugin '{pick}'"))
            })?;
            let fetched = listing
                .entry(entry, &store.staging(), creds.as_ref())
                .await
                .map_err(err400)?;
            let Inspection::Plugin(p) = crate::plugins::inspect(&fetched.root).map_err(err400)?
            else {
                return Err(bad_request(format!("'{pick}' is a marketplace itself")));
            };
            let exists = store.get(&p.name).is_ok();
            let token = s.staging.put(Arc::new(fetched), Some(listing));
            Ok(Json(json!({
                "kind": "plugin",
                "token": token,
                "exists": exists,
                "market": name,
                "plugin": plugin_json(&p, None, None),
            })))
        }
    }
}

#[derive(Deserialize)]
pub(super) struct InstallBody {
    token: String,
    #[serde(default)]
    rename: Option<String>,
}

/// Imports a reviewed plugin: its files are copied into the profile. Nothing runs.
pub(super) async fn install_plugin(
    State(s): State<AppState>,
    Json(body): Json<InstallBody>,
) -> Result<Json<Value>, ApiError> {
    let staged = s.staging.take(&body.token).ok_or_else(expired)?;
    let store = s.info.profile.plugins();
    let rename = body.rename.filter(|r| !r.trim().is_empty());
    let plugin = tokio::task::spawn_blocking(move || {
        let installed = store.install(&staged.fetched, rename.as_deref());
        drop(staged);
        installed
    })
    .await
    .map_err(internal)?
    .map_err(err400)?;
    s.system(
        "info",
        format!(
            "Imported plugin {}: {} skill(s), {} agent(s), {} MCP server template(s). Nothing in it ran.",
            plugin.name,
            plugin.skills.len(),
            plugin.agents.len(),
            plugin.mcp.len()
        ),
    );
    s.send(json!({ "type": "tools" }));
    let profile = &s.info.profile;
    Ok(Json(json!({
        "plugin": plugin_json(&plugin, Some(&profile.skills()), Some(&s.agent.shares())),
    })))
}

/// Fetches a plugin again from its source. Offered servers that now start
/// differently are paused for review.
pub(super) async fn update_plugin(
    State(s): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let profile = s.info.profile.reload().map_err(internal)?;
    let store = profile.plugins();
    let old = store.get(&name).map_err(|e| {
        (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("{e:#}") })),
        )
    })?;
    let source = Source::parse(&old.source)
        .with_context(|| format!("can't update '{name}' from {}", old.source))
        .map_err(err400)?;
    let fetched = crate::plugins::fetch(&source, &store.staging(), profile.github().ok().as_ref())
        .await
        .map_err(err400)?;
    let ((plugin, changes), shares) = tokio::task::spawn_blocking(move || {
        profile.update_shares(|shares| store.update(&name, &fetched, shares))
    })
    .await
    .map_err(internal)?
    .map_err(err400)?;
    shares_changed(
        &s,
        shares,
        format!(
            "Updated plugin {}: {}",
            plugin.name,
            changes.lines.join("; ")
        ),
    );
    Ok(Json(
        json!({ "plugin": plugin.name, "changes": changes.lines }),
    ))
}

#[derive(Deserialize)]
pub(super) struct EnabledBody {
    enabled: bool,
}

pub(super) async fn toggle_plugin(
    State(s): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<EnabledBody>,
) -> Result<Json<Value>, ApiError> {
    let profile = &s.info.profile;
    let store = profile.plugins();
    let (changes, shares) = profile
        .update_shares(|shares| store.set_enabled(&name, body.enabled, shares))
        .map_err(err400)?;
    shares_changed(
        &s,
        shares,
        format!(
            "Plugin {name} is {}",
            if body.enabled {
                "on"
            } else {
                "off: its skills and MCP servers no longer apply"
            }
        ),
    );
    Ok(Json(json!({ "changes": changes.lines })))
}

pub(super) async fn remove_plugin(
    State(s): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let profile = &s.info.profile;
    let store = profile.plugins();
    let (changes, shares) = profile
        .update_shares(|shares| store.remove(&name, shares))
        .map_err(err400)?;
    shares_changed(&s, shares, format!("Removed plugin {name}"));
    Ok(Json(json!({ "changes": changes.lines })))
}

/// The folders that can offer MCP servers, what each offers, and the server
/// templates of enabled plugins.
pub(super) async fn mcp_state(State(s): State<AppState>) -> Json<Value> {
    let shares = s.agent.shares();
    let plugins = s.info.profile.plugins().list().unwrap_or_default();
    Json(json!({
        "shares": shares.shares.iter().filter(|sh| sh.name != DATA_SHARE).map(|sh| json!({
            "name": sh.name,
            "path": sh.path,
            "max_access": sh.max_access,
            "mcp": sh.mcp.iter().map(|(n, d)| server_json(n, d)).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "templates": plugins.iter().filter(|p| p.enabled).flat_map(|p| p.mcp.iter().map(|(n, t)| json!({
            "plugin": p.name,
            "server": n,
            "describe": t.describe(),
            "inputs": t.inputs,
            "tools": t.tools,
        }))).collect::<Vec<_>>(),
    }))
}

#[derive(Deserialize)]
pub(super) struct ParseBody {
    json: String,
}

fn parse_config_text(text: &str) -> anyhow::Result<(BTreeMap<String, McpTemplate>, Vec<String>)> {
    let value: Value = serde_json::from_str(text.trim().trim_start_matches('\u{feff}'))
        .context("that isn't valid JSON")?;
    crate::mcp::parse_config(&value, &BTreeMap::new())
}

/// The servers in a pasted MCP configuration, for review.
pub(super) async fn parse_mcp(Json(body): Json<ParseBody>) -> Result<Json<Value>, ApiError> {
    let (found, notes) = parse_config_text(&body.json).map_err(err400)?;
    Ok(Json(json!({
        "servers": found.iter().map(|(n, t)| json!({
            "server": n,
            "name": crate::mcp::server_name(n),
            "describe": t.describe(),
            "inputs": t.inputs,
            "tools": t.tools,
            "remote": t.url.is_some(),
        })).collect::<Vec<_>>(),
        "notes": notes,
    })))
}

fn clean(list: &[String]) -> Vec<String> {
    list.iter()
        .map(|t| t.trim().to_owned())
        .filter(|t| !t.is_empty())
        .collect()
}

fn filled(values: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    values
        .iter()
        .filter(|(_, v)| !v.is_empty())
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

#[derive(Deserialize)]
pub(super) struct OfferBody {
    share: String,
    name: String,
    #[serde(default)]
    access: Option<Access>,
    #[serde(default)]
    tools: Vec<String>,
    #[serde(default)]
    description: String,
    #[serde(default)]
    values: BTreeMap<String, String>,
    #[serde(default)]
    replace: bool,
    /// From a plugin's template (`server`: which one).
    #[serde(default)]
    plugin: Option<String>,
    #[serde(default)]
    server: Option<String>,
    /// From an MCP configuration's text (`server`: which one).
    #[serde(default)]
    config: Option<String>,
    /// By hand: a local server's command line, or a remote server's URL.
    #[serde(default)]
    command_line: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    sse: bool,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
}

fn nonblank(text: &Option<String>) -> Option<&str> {
    text.as_deref().map(str::trim).filter(|t| !t.is_empty())
}

/// Offers an MCP server in a shared folder.
pub(super) async fn offer_mcp(
    State(s): State<AppState>,
    Json(body): Json<OfferBody>,
) -> Result<Json<Value>, ApiError> {
    let profile = &s.info.profile;
    let offer = McpOffer {
        share: body.share.clone(),
        name: body.name.trim().to_owned(),
        access: body.access.unwrap_or(Access::Full),
        tools: clean(&body.tools),
        description: body.description.clone(),
        replace: body.replace,
    };
    let values = filled(&body.values);
    let result = if let Some(plugin) = nonblank(&body.plugin) {
        let store = profile.plugins();
        let template = store
            .get(plugin)
            .and_then(|p| p.pick_server(nonblank(&body.server), &offer.name))
            .map_err(err400)?;
        profile.update_shares(|shares| {
            shares.offer_mcp(&offer, |folder| {
                store.instantiate(plugin, &template, folder, &values)
            })
        })
    } else if let Some(config) = nonblank(&body.config) {
        let (mut found, _) = parse_config_text(config).map_err(err400)?;
        let server = nonblank(&body.server)
            .ok_or_else(|| bad_request("pick a server of the configuration"))?;
        let template = found
            .remove(server)
            .ok_or_else(|| bad_request(format!("the configuration has no server '{server}'")))?;
        profile.update_shares(|shares| {
            shares.offer_mcp(&offer, |folder| {
                template.instantiate(&Fill {
                    plugin_root: None,
                    data_dir: None,
                    folder,
                    values: &values,
                })
            })
        })
    } else {
        let mut server = match (nonblank(&body.command_line), nonblank(&body.url)) {
            (Some(line), None) => {
                let words = crate::mcp::split_command(line).map_err(err400)?;
                let (command, args) = words
                    .split_first()
                    .ok_or_else(|| bad_request("the command is empty"))?;
                McpServer::local(command.clone(), args.to_vec())
            }
            (None, Some(url)) => McpServer::remote(url, body.sse),
            (Some(_), Some(_)) => return Err(bad_request("give a command or a URL, not both")),
            (None, None) => {
                return Err(bad_request(
                    "give the server's command (a local server) or its URL (a remote server)",
                ));
            }
        };
        let pairs = |map: &BTreeMap<String, String>| -> BTreeMap<String, String> {
            map.iter()
                .filter(|(k, _)| !k.trim().is_empty())
                .map(|(k, v)| (k.trim().to_owned(), v.clone()))
                .collect()
        };
        server.env = pairs(&body.env);
        server.headers = pairs(&body.headers);
        profile.update_shares(|shares| shares.offer_mcp(&offer, |_| Ok(server)))
    };
    let (server, shares) = result.map_err(err400)?;
    shares_changed(
        &s,
        shares,
        format!(
            "'{}' offers MCP server '{}' to peers' tasks with at least {} access",
            offer.share, offer.name, offer.access
        ),
    );
    Ok(Json(json!({ "server": server_json(&offer.name, &server) })))
}

#[derive(Deserialize)]
pub(super) struct TeamBody {
    #[serde(default)]
    plan: String,
}

fn no_share(name: &str) -> ApiError {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": format!("this agent shares no folder named '{name}'") })),
    )
}

/// Sets the team plan of a shared folder.
pub(super) async fn set_team(
    State(s): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<TeamBody>,
) -> Result<Json<Value>, ApiError> {
    let steps = if name == DATA_SHARE {
        0
    } else {
        crate::team::validate_template(&body.plan)
            .map_err(err400)?
            .steps
            .len()
    };
    let (found, shares) = s
        .info
        .profile
        .update_shares(|shares| {
            if name != DATA_SHARE && shares.get(&name).is_none() {
                return Ok(false);
            }
            shares.set_team(&name, Some(body.plan.clone()))?;
            Ok(true)
        })
        .map_err(err400)?;
    if !found {
        return Err(no_share(&name));
    }
    shares_changed(
        &s,
        shares,
        format!(
            "'{name}' takes team steps (a team plan of {steps} step{})",
            if steps == 1 { "" } else { "s" }
        ),
    );
    Ok(Json(json!({ "name": name, "steps": steps })))
}

/// Removes the team plan of a shared folder.
pub(super) async fn remove_team(
    State(s): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let (found, shares) = s
        .info
        .profile
        .update_shares(|shares| {
            if shares.get(&name).is_none() {
                return Ok(false);
            }
            shares.set_team(&name, None)?;
            Ok(true)
        })
        .map_err(err400)?;
    if !found {
        return Err(no_share(&name));
    }
    shares_changed(&s, shares, format!("'{name}' no longer takes team steps"));
    Ok(Json(json!({ "name": name })))
}

#[derive(Deserialize)]
pub(super) struct PullRequestsBody {
    on: bool,
    /// You approve each one before it is opened; absent keeps how it is.
    approval: Option<bool>,
}

/// Lets peers with edit access end their plans with pull request steps in a shared
/// folder (a linked repository shared with worktrees), each opened once you approve it if
/// you like, or stops letting them.
pub(super) async fn set_pull_requests(
    State(s): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<PullRequestsBody>,
) -> Result<Json<Value>, ApiError> {
    if body.on {
        crate::github::RepoStore::load(&s.info.profile.repos_path())
            .map_err(err400)?
            .get(&name)
            .map_err(|_| {
                bad_request(format!(
                    "'{name}' isn't a linked GitHub repository: pull requests are opened in one (link it under Repositories)"
                ))
            })?;
    }
    let mut approval = false;
    let (found, shares) = s
        .info
        .profile
        .update_shares(|shares| {
            let Some(share) = shares.get(&name) else {
                return Ok(false);
            };
            approval = body.approval.unwrap_or(share.pull_request_approval);
            shares.set_pull_requests(&name, body.on, approval)?;
            Ok(true)
        })
        .map_err(err400)?;
    if !found {
        return Err(no_share(&name));
    }
    let approval = body.on && approval;
    shares_changed(
        &s,
        shares,
        if approval {
            format!(
                "'{name}' takes pull request steps from peers with edit access, each once you approve it"
            )
        } else if body.on {
            format!("'{name}' takes pull request steps from peers with edit access")
        } else {
            format!("'{name}' no longer takes pull request steps")
        },
    );
    Ok(Json(
        json!({ "name": name, "on": body.on, "approval": approval }),
    ))
}

#[derive(Deserialize)]
pub(super) struct OfferEdit {
    #[serde(default)]
    access: Option<Access>,
    #[serde(default)]
    tools: Option<Vec<String>>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    paused: Option<bool>,
    /// New values for a plugin's server (blank: keep the current one).
    #[serde(default)]
    values: BTreeMap<String, String>,
}

/// Changes an offered server: its access, tools, description, paused state or (a
/// plugin's server) values, which makes it again from the plugin's template.
pub(super) async fn edit_mcp(
    State(s): State<AppState>,
    Path((share, name)): Path<(String, String)>,
    Json(body): Json<OfferEdit>,
) -> Result<Json<Value>, ApiError> {
    let profile = &s.info.profile;
    let store = profile.plugins();
    let values = filled(&body.values);
    let (server, shares) = profile
        .update_shares(|shares| {
            let folder = shares
                .get(&share)
                .map(|sh| sh.path.clone())
                .with_context(|| format!("no share named '{share}'"))?;
            shares.edit_mcp(&share, &name, |server| {
                if !values.is_empty() {
                    let origin = server.from.clone().context(
                        "only a plugin's servers take new values here; remove this one and add it again",
                    )?;
                    let mut merged = origin.values.clone();
                    merged.extend(values);
                    let mut fresh =
                        store.instantiate(&origin.plugin, &origin.server, &folder, &merged)?;
                    fresh.access = server.access;
                    fresh.tools = server.tools.clone();
                    fresh.description = server.description.clone();
                    fresh.paused = server.paused;
                    fresh.plugin_off = server.plugin_off;
                    *server = fresh;
                }
                if let Some(access) = body.access {
                    server.access = access;
                }
                if let Some(tools) = &body.tools {
                    server.tools = clean(tools);
                }
                if let Some(description) = &body.description {
                    server.description = description.trim().to_owned();
                }
                if let Some(paused) = body.paused {
                    server.paused = paused;
                }
                Ok(())
            })?;
            Ok(shares.get(&share).and_then(|sh| sh.mcp.get(&name)).cloned())
        })
        .map_err(err400)?;
    let server = server.ok_or_else(|| internal("the server is gone"))?;
    shares_changed(
        &s,
        shares,
        format!(
            "MCP server '{name}' in '{share}' {}",
            if server.paused {
                "is paused"
            } else {
                "is offered"
            }
        ),
    );
    Ok(Json(json!({ "server": server_json(&name, &server) })))
}

pub(super) async fn remove_mcp(
    State(s): State<AppState>,
    Path((share, name)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let (_, shares) = s
        .info
        .profile
        .update_shares(|shares| {
            if !shares.remove_mcp(&share, &name)? {
                anyhow::bail!("'{share}' offers no MCP server named '{name}'");
            }
            Ok(())
        })
        .map_err(err400)?;
    shares_changed(
        &s,
        shares,
        format!("'{share}' no longer offers MCP server '{name}'"),
    );
    Ok(Json(json!({ "removed": name })))
}

#[derive(Deserialize)]
pub(super) struct SkillBody {
    #[serde(default)]
    name: String,
    text: String,
}

/// Adds (or replaces) one of the profile's own skills; its name comes from the
/// body or the skill's front matter.
pub(super) async fn add_skill(
    State(s): State<AppState>,
    Json(body): Json<SkillBody>,
) -> Result<Json<Value>, ApiError> {
    let name = Some(body.name.trim().to_owned())
        .filter(|n| !n.is_empty())
        .or_else(|| crate::skills::front_matter_name(&body.text))
        .ok_or_else(|| bad_request("give the skill a name, or a name: line in its front matter"))?;
    let skills = s.info.profile.skills();
    let replaced = skills
        .list()
        .into_iter()
        .find(|x| x.name == name)
        .map(|x| x.source);
    skills.add(&name, &body.text).map_err(err400)?;
    s.system("info", format!("Saved skill {name}"));
    s.send(json!({ "type": "tools" }));
    Ok(Json(json!({ "name": name, "replaced": replaced })))
}

pub(super) async fn remove_skill(
    State(s): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    match s.info.profile.skills().remove(&name) {
        Ok(true) => {
            s.system("info", format!("Removed skill {name}"));
            s.send(json!({ "type": "tools" }));
            Ok(Json(json!({ "removed": name })))
        }
        Ok(false) => Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("the profile has no skill of its own named '{name}'") })),
        )),
        Err(e) => Err(err400(e)),
    }
}

/// Adds or replaces one of the profile's agents (named by the body, else its front
/// matter).
pub(super) async fn add_agent(
    State(s): State<AppState>,
    Json(body): Json<SkillBody>,
) -> Result<Json<Value>, ApiError> {
    let name = Some(body.name.trim().to_owned())
        .filter(|n| !n.is_empty())
        .or_else(|| crate::skills::front_matter_name(&body.text))
        .ok_or_else(|| bad_request("give the agent a name, or a name: line in its front matter"))?;
    let skills = s.info.profile.skills();
    let replaced = skills
        .agents()
        .into_iter()
        .find(|a| a.name == name)
        .map(|a| a.source);
    let agent = skills.add_agent(&name, &body.text).map_err(err400)?;
    s.system("info", format!("Saved agent {name}"));
    s.send(json!({ "type": "tools" }));
    Ok(Json(json!({ "agent": agent, "replaced": replaced })))
}

pub(super) async fn remove_agent(
    State(s): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    match s.info.profile.skills().remove_agent(&name) {
        Ok(true) => {
            s.system("info", format!("Removed agent {name}"));
            s.send(json!({ "type": "tools" }));
            Ok(Json(json!({ "removed": name })))
        }
        Ok(false) => Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("the profile has no agent of its own named '{name}'") })),
        )),
        Err(e) => Err(err400(e)),
    }
}
