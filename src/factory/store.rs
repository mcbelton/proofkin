//! The factory's store in a profile (`<profile>/factory/`, private to its owner):
//!
//! ```text
//! factory/
//!   drafts/<name>.json          an agent's draft: its blueprint, as written or edited, and how it was first written
//!   agents/<name>/state.json    its lifecycle: state, active version, deployments, and the history of every change
//!   agents/<name>/fixtures/     files its eval cases work on
//! ```
//!
//! Builds, evaluations and releases go next to the state as the factory grows
//! (docs/agent-factory/design/data-dictionary.md). Files are written atomically, and
//! changes to an agent hold its lock ([`crate::store`]), so the CLI, the web UI and an
//! agent can work at once without losing a change. A file that can't be read is
//! reported with its path and left as it is, never replaced.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::llm::Provider;
use crate::profile::{Profile, validate_name};
use crate::store::{FileLock, write_atomic};

/// An agent's draft: its blueprint, which may still have problems, and how it was
/// first written.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Draft {
    /// The blueprint, as written or last edited (JSON, checked when it is shown or used).
    pub blueprint: Value,
    pub provenance: Provenance,
    /// How many tries the model needed (0: no model wrote it).
    #[serde(default)]
    pub tries: u32,
}

/// How a draft was first written: from a template, by a model, or from a file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    /// `template`, `model` or `file`.
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<Provider>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// The SHA-256 of the description the model was given; never its text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description_sha256: Option<String>,
    /// When (RFC 3339, UTC).
    pub at: String,
}

/// Where an agent is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lifecycle {
    /// Not released yet.
    Draft,
    /// Released: plans and deployments use its active version.
    Active,
    /// Plans are warned and pointed to its replacement.
    Deprecated,
    /// Plans refuse it; its releases are kept for audit.
    Retired,
    /// Published by a peer, not yet evaluated here.
    Imported,
}

impl std::fmt::Display for Lifecycle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.pad(match self {
            Lifecycle::Draft => "draft",
            Lifecycle::Active => "active",
            Lifecycle::Deprecated => "deprecated",
            Lifecycle::Retired => "retired",
            Lifecycle::Imported => "imported",
        })
    }
}

/// An agent's lifecycle and history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentState {
    pub name: String,
    pub state: Lifecycle,
    /// The release `agent=name` uses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_version: Option<String>,
    /// Its releases, newest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub versions: Vec<String>,
    /// The digital workers that run it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deployments: Vec<DeploymentRecord>,
    /// Every change, oldest first.
    #[serde(default)]
    pub history: Vec<Change>,
}

impl AgentState {
    fn new(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            state: Lifecycle::Draft,
            active_version: None,
            versions: Vec::new(),
            deployments: Vec::new(),
            history: Vec::new(),
        }
    }
}

/// A digital worker that runs a release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeploymentRecord {
    pub worker: String,
    pub version: String,
    pub state: String,
}

/// A change to an agent: who, when, what and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    pub at: String,
    pub by: String,
    pub what: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
}

/// One of the factory's agents, as listed.
#[derive(Debug, Clone, PartialEq)]
pub struct Listed {
    pub name: String,
    pub state: Lifecycle,
    /// Its draft's title, if it has a draft that names one.
    pub title: Option<String>,
    /// When it last changed.
    pub changed: Option<String>,
    /// Its draft, if it has one.
    pub draft: Option<Draft>,
}

/// Now, as the store writes times: RFC 3339 in UTC, to the second.
pub fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// A profile's factory store.
#[derive(Debug, Clone)]
pub struct FactoryStore {
    root: PathBuf,
}

impl FactoryStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// `profile`'s store (`<profile>/factory`).
    pub fn of(profile: &Profile) -> Self {
        Self::new(profile.dir.join("factory"))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn draft_path(&self, name: &str) -> PathBuf {
        self.root.join("drafts").join(format!("{name}.json"))
    }

    pub fn agent_dir(&self, name: &str) -> PathBuf {
        self.root.join("agents").join(name)
    }

    pub fn state_path(&self, name: &str) -> PathBuf {
        self.agent_dir(name).join("state.json")
    }

    /// The folder agent `name`'s eval cases take their fixtures from.
    pub fn fixtures_dir(&self, name: &str) -> PathBuf {
        self.agent_dir(name).join("fixtures")
    }

    /// Agent `name`'s draft, if it has one.
    pub fn draft(&self, name: &str) -> Result<Option<Draft>> {
        validate_agent_name(name)?;
        read_json(&self.draft_path(name))
    }

    /// Agent `name`'s lifecycle and history, if it is one of the factory's.
    pub fn state(&self, name: &str) -> Result<Option<AgentState>> {
        validate_agent_name(name)?;
        read_json(&self.state_path(name))
    }

    /// Makes a new agent `name` from `draft`; `by` made it, as `what` says. Fails if the
    /// factory has an agent or draft of that name.
    pub fn create_draft(&self, name: &str, draft: &Draft, by: &str, what: &str) -> Result<()> {
        validate_agent_name(name)?;
        self.prepare()?;
        let _lock = FileLock::acquire(&self.state_path(name))?;
        if self.draft_path(name).exists() || self.state_path(name).exists() {
            bail!(
                "the factory already has an agent or draft '{name}' (proofkin factory show {name})"
            );
        }
        self.write_draft(name, draft, by, what)
    }

    /// Saves `draft` as agent `name`'s draft, made or changed by `by` as `what` says
    /// (recorded in its history). A new agent starts as a draft.
    pub fn save_draft(&self, name: &str, draft: &Draft, by: &str, what: &str) -> Result<()> {
        validate_agent_name(name)?;
        self.prepare()?;
        let _lock = FileLock::acquire(&self.state_path(name))?;
        self.write_draft(name, draft, by, what)
    }

    /// Writes the draft and its history entry; the caller holds the agent's lock.
    fn write_draft(&self, name: &str, draft: &Draft, by: &str, what: &str) -> Result<()> {
        let mut state = read_json(&self.state_path(name))?.unwrap_or_else(|| AgentState::new(name));
        write_json(&self.draft_path(name), draft)?;
        state.history.push(Change {
            at: now(),
            by: by.to_owned(),
            what: what.to_owned(),
            why: None,
        });
        write_json(&self.state_path(name), &state)
    }

    /// Changes agent `name`'s state with `change`, under its lock, and returns it.
    pub fn update_state(
        &self,
        name: &str,
        change: impl FnOnce(&mut AgentState) -> Result<()>,
    ) -> Result<AgentState> {
        validate_agent_name(name)?;
        self.prepare()?;
        let _lock = FileLock::acquire(&self.state_path(name))?;
        let mut state = read_json(&self.state_path(name))?.unwrap_or_else(|| AgentState::new(name));
        change(&mut state)?;
        write_json(&self.state_path(name), &state)?;
        Ok(state)
    }

    /// Deletes agent `name`'s draft and records (`false` if it had neither). An agent
    /// with releases isn't deleted, so its records are kept: it is to be retired.
    pub fn delete(&self, name: &str) -> Result<bool> {
        validate_agent_name(name)?;
        let _lock = FileLock::acquire(&self.state_path(name))?;
        if let Some(state) = read_json::<AgentState>(&self.state_path(name))?
            && !state.versions.is_empty()
        {
            bail!(
                "'{name}' has releases ({}): retire it instead, so its records are kept",
                state.versions.join(", ")
            );
        }
        let draft = self.draft_path(name);
        let existed = draft.exists() || self.state_path(name).exists();
        if draft.exists() {
            std::fs::remove_file(&draft)
                .with_context(|| format!("deleting {}", draft.display()))?;
        }
        // Its folder goes too, with the lock file taking the lock made in it.
        let dir = self.agent_dir(name);
        if dir.exists() {
            std::fs::remove_dir_all(&dir).with_context(|| format!("deleting {}", dir.display()))?;
        }
        Ok(existed)
    }

    /// The names of the factory's agents and drafts, sorted.
    pub fn names(&self) -> Result<Vec<String>> {
        let mut names = std::collections::BTreeSet::new();
        for entry in read_dir(&self.root.join("drafts"))? {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "json")
                && let Some(name) = path.file_stem().and_then(|s| s.to_str())
                && validate_name(name).is_ok()
            {
                names.insert(name.to_owned());
            }
        }
        for entry in read_dir(&self.root.join("agents"))? {
            if entry.path().join("state.json").is_file()
                && let Some(name) = entry.file_name().to_str()
                && validate_name(name).is_ok()
            {
                names.insert(name.to_owned());
            }
        }
        Ok(names.into_iter().collect())
    }

    /// The factory's agents, sorted by name, each with its draft.
    pub fn list(&self) -> Result<Vec<Listed>> {
        self.names()?
            .into_iter()
            .map(|name| {
                let draft = self.draft(&name)?;
                let state = self.state(&name)?;
                Ok(Listed {
                    title: draft
                        .as_ref()
                        .and_then(|d| d.blueprint.get("title"))
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    state: state.as_ref().map_or(Lifecycle::Draft, |s| s.state),
                    changed: state.and_then(|s| s.history.last().map(|c| c.at.clone())),
                    draft,
                    name,
                })
            })
            .collect()
    }

    /// Creates the store's folder, private to its owner.
    fn prepare(&self) -> Result<()> {
        std::fs::create_dir_all(&self.root)
            .with_context(|| format!("creating {}", self.root.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.root, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }
}

fn validate_agent_name(name: &str) -> Result<()> {
    validate_name(name).map_err(|_| {
        anyhow::anyhow!(
            "{name:?} can't name an agent: use letters, digits, '-', '_' and '.' (at most 64)"
        )
    })
}

fn read_dir(dir: &Path) -> Result<Vec<std::fs::DirEntry>> {
    match std::fs::read_dir(dir) {
        Ok(entries) => Ok(entries.flatten().collect()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e).with_context(|| format!("reading {}", dir.display())),
    }
}

/// The JSON file at `path` (`None` if there is none). One that can't be read or
/// parsed is an error naming it, so it is never written over by mistake.
fn read_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    serde_json::from_str(&text).map(Some).map_err(|e| {
        anyhow::anyhow!(
            "{} is damaged ({e}): fix or remove it; it was left as it is",
            path.display()
        )
    })
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    write_atomic(
        path,
        (serde_json::to_string_pretty(value)? + "\n").as_bytes(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn draft(title: &str) -> Draft {
        Draft {
            blueprint: json!({ "schema": crate::factory::blueprint::SCHEMA, "name": "fin", "title": title }),
            provenance: Provenance {
                source: "template".into(),
                template: Some("financial-analyst".into()),
                at: now(),
                ..Provenance::default()
            },
            tries: 0,
        }
    }

    #[test]
    fn drafts_and_their_history_survive_a_restart_and_concurrent_writers() {
        let dir = tempfile::tempdir().unwrap();
        let store = FactoryStore::new(dir.path().join("factory"));
        store
            .create_draft(
                "fin",
                &draft("v0"),
                "alice",
                "drafted from template financial-analyst",
            )
            .unwrap();
        let writers: Vec<_> = (1..=8)
            .map(|i| {
                let store = store.clone();
                std::thread::spawn(move || {
                    store
                        .save_draft(
                            "fin",
                            &draft(&format!("v{i}")),
                            "alice",
                            &format!("edit {i}"),
                        )
                        .unwrap();
                })
            })
            .collect();
        for writer in writers {
            writer.join().unwrap();
        }
        // A new store on the same folder: as after a restart.
        let again = FactoryStore::new(dir.path().join("factory"));
        let state = again.state("fin").unwrap().unwrap();
        assert_eq!(state.state, Lifecycle::Draft);
        assert_eq!(state.history.len(), 9, "no change was lost");
        assert_eq!(
            state.history[0].what,
            "drafted from template financial-analyst"
        );
        let mut edits: Vec<&str> = state.history[1..].iter().map(|c| c.what.as_str()).collect();
        edits.sort();
        assert_eq!(
            edits,
            (1..=8).map(|i| format!("edit {i}")).collect::<Vec<_>>()
        );
        let title = again.draft("fin").unwrap().unwrap().blueprint["title"].clone();
        assert_eq!(
            title,
            json!(state.history.last().unwrap().what.replace("edit ", "v"))
        );
        assert_eq!(again.names().unwrap(), ["fin"]);
        let listed = again.list().unwrap();
        assert_eq!(listed[0].state, Lifecycle::Draft);
        assert!(listed[0].title.is_some() && listed[0].changed.is_some());
    }

    #[test]
    fn a_damaged_file_is_reported_with_its_path_and_left_as_it_is() {
        let dir = tempfile::tempdir().unwrap();
        let store = FactoryStore::new(dir.path());
        store
            .create_draft("fin", &draft("v0"), "alice", "drafted")
            .unwrap();
        let state = store.state_path("fin");
        std::fs::write(&state, "{ not json").unwrap();
        let err = store
            .save_draft("fin", &draft("v1"), "alice", "edit")
            .unwrap_err();
        assert!(
            err.to_string().contains(&state.display().to_string()),
            "{err}"
        );
        assert!(err.to_string().contains("damaged"), "{err}");
        assert_eq!(std::fs::read_to_string(&state).unwrap(), "{ not json");
        assert_eq!(
            store.draft("fin").unwrap().unwrap().blueprint["title"],
            json!("v0"),
            "the draft wasn't changed either"
        );
        std::fs::write(store.draft_path("fin"), "[").unwrap();
        let err = store.draft("fin").unwrap_err();
        assert!(err.to_string().contains("fin.json"), "{err}");
    }

    #[test]
    fn a_name_has_one_agent_and_deleting_keeps_released_ones() {
        let dir = tempfile::tempdir().unwrap();
        let store = FactoryStore::new(dir.path());
        assert!(
            store.names().unwrap().is_empty(),
            "an empty store lists nothing"
        );
        store
            .create_draft("fin", &draft("v0"), "alice", "drafted")
            .unwrap();
        let again = store
            .create_draft("fin", &draft("v1"), "alice", "drafted")
            .unwrap_err();
        assert!(again.to_string().contains("already has"), "{again}");
        assert!(
            store
                .create_draft("../x", &draft("v0"), "alice", "drafted")
                .is_err()
        );
        assert!(store.draft("a/b").is_err());

        store
            .create_draft("old", &draft("v0"), "alice", "drafted")
            .unwrap();
        store
            .update_state("old", |s| {
                s.state = Lifecycle::Active;
                s.versions.push("1.0.0".into());
                Ok(())
            })
            .unwrap();
        let kept = store.delete("old").unwrap_err();
        assert!(kept.to_string().contains("retire it instead"), "{kept}");

        assert!(store.delete("fin").unwrap());
        assert!(!store.delete("fin").unwrap());
        assert_eq!(store.names().unwrap(), ["old"]);
    }
}
