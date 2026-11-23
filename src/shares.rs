//! Folders an agent shares with its peers. Peer tasks run only inside a shared
//! folder the owner granted to that peer; the agent's own working folder is never
//! exposed. Every profile has a built-in read-only `data` share, its own
//! `.proofkin-data/<profile>` folder, for information the owner wants peers to see.
//! Other folders (e.g. a repository for coding tasks) are shared explicitly with
//! `proofkin share add`, each with the most access any peer's task may get there.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::llm::Access;
use crate::mcp::McpServer;
use crate::profile::validate_name;

/// Name of the built-in share (the profile's `.proofkin-data` folder).
pub const DATA_SHARE: &str = "data";

/// A folder shared with peers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Share {
    pub name: String,
    pub path: PathBuf,
    /// Most a peer's task may do here, whatever the peer was granted.
    pub max_access: Access,
    /// Owner-defined commands (name -> shell command) a task may ask the runtime to
    /// run after the model, as evidence (e.g. `test` -> `cargo test`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub checks: BTreeMap<String, String>,
    /// Run each editing task in its own git worktree and branch, and commit its work.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub worktrees: bool,
    /// Owner-defined MCP servers tasks here may use (see [`crate::mcp`]).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub mcp: BTreeMap<String, McpServer>,
    /// The owner's team plan for team steps peers hand this folder (see
    /// [`crate::team`]): plan text whose `{task}` is the task handed down.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team: Option<String>,
    /// Peers with edit access may end a plan with a pull request step here: this agent
    /// pushes the branch of their tasks' work and opens the pull request with the owner's
    /// GitHub account (a linked repository shared with worktrees; see [`crate::pulls`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pull_requests: bool,
}

/// What a peer learns about a share: its name, the access it would get and the
/// names of its checks and of the MCP servers it may use. Local paths, commands
/// and server definitions are never sent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareInfo {
    pub name: String,
    pub access: Access,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub worktrees: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp: Vec<String>,
    /// What each of those MCP servers is for, in its owner's words.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub mcp_about: BTreeMap<String, String>,
    /// The folder has a team plan: team steps may be handed to it (its text stays
    /// with its owner).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub team: bool,
    /// The folder takes pull request steps (its owner allows them; older agents never
    /// say so, so they are never sent one).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pull_requests: bool,
}

/// How an owner offers an MCP server in a shared folder (see [`ShareStore::offer_mcp`]).
#[derive(Debug, Clone)]
pub struct McpOffer {
    pub share: String,
    /// The name tasks use for it.
    pub name: String,
    /// The least access a peer's task needs to use it.
    pub access: Access,
    /// The tools tasks may call (empty: the server's own list, else all).
    pub tools: Vec<String>,
    /// What it is for (peers see it).
    pub description: String,
    /// Replace a server the share offers under that name.
    pub replace: bool,
}

/// Parses `name=command` check definitions.
pub fn parse_checks(defs: &[String]) -> Result<BTreeMap<String, String>> {
    let mut checks = BTreeMap::new();
    for def in defs {
        let (name, command) = def
            .split_once('=')
            .with_context(|| format!("check {def:?} must look like name=command"))?;
        let (name, command) = (name.trim(), command.trim());
        validate_name(name).with_context(|| format!("invalid check name {name:?}"))?;
        if command.is_empty() {
            bail!("check '{name}' has no command");
        }
        checks.insert(name.to_owned(), command.to_owned());
    }
    Ok(checks)
}

/// The access a peer's task gets in a share.
pub fn effective_access(peer_grant: Access, share_max: Access) -> Access {
    peer_grant.min(share_max)
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareStore {
    #[serde(default)]
    pub shares: Vec<Share>,
}

impl ShareStore {
    /// Only the built-in read-only `data` share at `data`.
    pub fn with_data(data: PathBuf) -> Self {
        Self {
            shares: vec![Share {
                name: DATA_SHARE.into(),
                path: data,
                max_access: Access::ReadOnly,
                checks: BTreeMap::new(),
                worktrees: false,
                mcp: BTreeMap::new(),
                team: None,
                pull_requests: false,
            }],
        }
    }

    /// The owner's shares from `path` plus the built-in `data` share at `data`.
    pub fn load(path: &Path, data: PathBuf) -> Result<Self> {
        let mut store = Self::with_data(data);
        if path.is_file() {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading {}", path.display()))?;
            let saved: ShareStore = serde_json::from_str(&text)
                .with_context(|| format!("parsing {}", path.display()))?;
            store
                .shares
                .extend(saved.shares.into_iter().filter(|s| s.name != DATA_SHARE));
        }
        Ok(store)
    }

    /// Saves the owner's shares (the built-in `data` share is implied).
    pub fn save(&self, path: &Path) -> Result<()> {
        let owned = ShareStore {
            shares: self
                .shares
                .iter()
                .filter(|s| s.name != DATA_SHARE)
                .cloned()
                .collect(),
        };
        crate::store::write_atomic(path, &serde_json::to_vec_pretty(&owned)?)
    }

    /// Loads the shares at `path`, changes them with `change` and saves them, holding
    /// their lock, so the web UI and the CLI never overwrite each other's changes.
    /// Returns `change`'s result and the saved shares (to give the running agent).
    pub fn update<T>(
        path: &Path,
        data: PathBuf,
        change: impl FnOnce(&mut ShareStore) -> Result<T>,
    ) -> Result<(T, ShareStore)> {
        let _lock = crate::store::FileLock::acquire(path)?;
        let mut store = Self::load(path, data)?;
        let out = change(&mut store)?;
        store.save(path)?;
        Ok((out, store))
    }

    /// Shares `path` (an existing folder) as `name`, replacing a share of that name
    /// (its MCP servers are kept).
    pub fn add(&mut self, name: &str, path: &Path, max_access: Access) -> Result<&Share> {
        self.add_with(name, path, max_access, BTreeMap::new(), false)
    }

    /// [`ShareStore::add`] with checks and per-task git worktrees (needs a git repository).
    pub fn add_with(
        &mut self,
        name: &str,
        path: &Path,
        max_access: Access,
        checks: BTreeMap<String, String>,
        worktrees: bool,
    ) -> Result<&Share> {
        validate_name(name).context("invalid share name")?;
        if name == DATA_SHARE {
            bail!("'{DATA_SHARE}' is the built-in share (the profile's .proofkin-data folder)");
        }
        let path = std::fs::canonicalize(path)
            .with_context(|| format!("{} is not an existing folder", path.display()))?;
        if !path.is_dir() {
            bail!("{} is not a folder", path.display());
        }
        let path = plain_path(path);
        if worktrees && !path.join(".git").exists() {
            bail!(
                "--worktrees needs a git repository; {} has no .git",
                path.display()
            );
        }
        let mcp = self.get(name).map(|s| s.mcp.clone()).unwrap_or_default();
        let team = self.get(name).and_then(|s| s.team.clone());
        let pull_requests = worktrees && self.get(name).is_some_and(|s| s.pull_requests);
        self.shares.retain(|s| s.name != name);
        self.shares.push(Share {
            name: name.to_owned(),
            path,
            max_access,
            checks,
            worktrees,
            mcp,
            team,
            pull_requests,
        });
        Ok(self.shares.last().expect("just pushed"))
    }

    /// Lets peers with edit access end their plans with pull request steps in share
    /// `share` (`on`), or stops letting them. The folder must be shared with worktrees,
    /// as pull requests come from its tasks' branches.
    pub fn set_pull_requests(&mut self, share: &str, on: bool) -> Result<()> {
        if share == DATA_SHARE && on {
            bail!("the built-in '{DATA_SHARE}' share has no branches to open pull requests from");
        }
        let share = self
            .shares
            .iter_mut()
            .find(|s| s.name == share)
            .with_context(|| format!("no share named '{share}'"))?;
        if on && !share.worktrees {
            bail!(
                "'{}' isn't shared with --worktrees: pull requests come from the branches its tasks commit to",
                share.name
            );
        }
        share.pull_requests = on;
        Ok(())
    }

    /// Sets (or with `None` removes) the team plan of share `share`; the plan must be
    /// valid (see [`crate::team::validate_template`]).
    pub fn set_team(&mut self, share: &str, plan: Option<String>) -> Result<()> {
        if share == DATA_SHARE && plan.is_some() {
            bail!("the built-in '{DATA_SHARE}' share can't have a team plan; share a folder");
        }
        if let Some(text) = &plan {
            crate::team::validate_template(text)?;
        }
        let share = self
            .shares
            .iter_mut()
            .find(|s| s.name == share)
            .with_context(|| format!("no share named '{share}'"))?;
        share.team = plan;
        Ok(())
    }

    /// Offers MCP server `server` as `name` to tasks in share `share`, replacing a
    /// server of that name.
    pub fn add_mcp(&mut self, share: &str, name: &str, server: McpServer) -> Result<()> {
        if share == DATA_SHARE {
            bail!("the built-in '{DATA_SHARE}' share can't offer MCP servers; share a folder");
        }
        server.validate(name)?;
        let share = self
            .shares
            .iter_mut()
            .find(|s| s.name == share)
            .with_context(|| format!("no share named '{share}'"))?;
        share.mcp.insert(name.to_owned(), server);
        Ok(())
    }

    /// Offers the server `make` returns for the share's folder as `offer.name`, with
    /// the offer's access, tools (none: the server's own list) and description.
    /// Fails if the share offers that name already, unless `offer.replace`.
    pub fn offer_mcp(
        &mut self,
        offer: &McpOffer,
        make: impl FnOnce(&Path) -> Result<McpServer>,
    ) -> Result<McpServer> {
        let share = self
            .get(&offer.share)
            .with_context(|| format!("no share named '{}'", offer.share))?;
        if !offer.replace && share.mcp.contains_key(&offer.name) {
            bail!(
                "'{}' already offers an MCP server named '{}': pick another name, or change that one",
                offer.share,
                offer.name
            );
        }
        let mut server = make(&share.path)?;
        server.access = offer.access;
        if !offer.tools.is_empty() {
            server.tools = offer.tools.clone();
        }
        server.description = offer.description.trim().to_owned();
        self.add_mcp(&offer.share, &offer.name, server.clone())?;
        Ok(server)
    }

    /// Changes MCP server `name` offered in `share` with `change`; the result must
    /// still be valid.
    pub fn edit_mcp(
        &mut self,
        share: &str,
        name: &str,
        change: impl FnOnce(&mut McpServer) -> Result<()>,
    ) -> Result<()> {
        let server = self
            .shares
            .iter_mut()
            .find(|s| s.name == share)
            .with_context(|| format!("no share named '{share}'"))?
            .mcp
            .get_mut(name)
            .with_context(|| format!("'{share}' offers no MCP server named '{name}'"))?;
        let mut changed = server.clone();
        change(&mut changed)?;
        changed.validate(name)?;
        *server = changed;
        Ok(())
    }

    /// Stops offering MCP server `name` in share `share`; whether it was offered.
    pub fn remove_mcp(&mut self, share: &str, name: &str) -> Result<bool> {
        let share = self
            .shares
            .iter_mut()
            .find(|s| s.name == share)
            .with_context(|| format!("no share named '{share}'"))?;
        Ok(share.mcp.remove(name).is_some())
    }

    pub fn remove(&mut self, name: &str) -> Result<bool> {
        if name == DATA_SHARE {
            bail!("the built-in '{DATA_SHARE}' share can't be removed");
        }
        let before = self.shares.len();
        self.shares.retain(|s| s.name != name);
        Ok(self.shares.len() != before)
    }

    pub fn get(&self, name: &str) -> Option<&Share> {
        self.shares.iter().find(|s| s.name == name)
    }

    /// Shares among `granted` names, with the access a peer granted `grant` gets.
    pub fn visible(&self, granted: &[String], grant: Access) -> Vec<ShareInfo> {
        self.shares
            .iter()
            .filter(|s| granted.contains(&s.name))
            .map(|s| {
                let access = effective_access(grant, s.max_access);
                let offered: Vec<(&String, &McpServer)> = s
                    .mcp
                    .iter()
                    .filter(|(_, server)| server.offered(access))
                    .collect();
                ShareInfo {
                    name: s.name.clone(),
                    access,
                    checks: s.checks.keys().cloned().collect(),
                    worktrees: s.worktrees,
                    mcp: offered.iter().map(|(name, _)| (*name).clone()).collect(),
                    mcp_about: offered
                        .iter()
                        .filter(|(_, server)| !server.description.is_empty())
                        .map(|(name, server)| ((*name).clone(), server.description.clone()))
                        .collect(),
                    team: s.team.is_some(),
                    pull_requests: s.pull_requests && s.worktrees,
                }
            })
            .collect()
    }
}

/// `C:\x` instead of Windows' verbatim `\\?\C:\x` from `canonicalize`, for display.
fn plain_path(path: PathBuf) -> PathBuf {
    match path.to_str().and_then(|p| p.strip_prefix(r"\\?\")) {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => PathBuf::from(rest),
        _ => path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_folders_with_worktrees_take_pull_request_steps() {
        let dir = tempfile::tempdir().unwrap();
        let (repo, plain) = (dir.path().join("repo"), dir.path().join("plain"));
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(&plain).unwrap();
        let mut store = ShareStore::with_data(dir.path().join("data"));
        store
            .add_with("repo", &repo, Access::WorkspaceWrite, BTreeMap::new(), true)
            .unwrap();
        store.add("plain", &plain, Access::WorkspaceWrite).unwrap();
        let refused = store.set_pull_requests("plain", true).unwrap_err();
        assert!(
            refused
                .to_string()
                .contains("isn't shared with --worktrees")
        );
        assert!(store.set_pull_requests(DATA_SHARE, true).is_err());
        store.set_pull_requests("repo", true).unwrap();
        assert!(store.visible(&["repo".to_owned()], Access::Full)[0].pull_requests);
        // Shared again, it keeps taking them, unless it no longer has worktrees.
        store
            .add_with("repo", &repo, Access::Full, BTreeMap::new(), true)
            .unwrap();
        assert!(store.get("repo").unwrap().pull_requests);
        store
            .add_with("repo", &repo, Access::Full, BTreeMap::new(), false)
            .unwrap();
        assert!(!store.get("repo").unwrap().pull_requests);
    }

    #[test]
    fn data_folder_is_built_in_read_only_and_per_profile() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("profiles");
        let worker = crate::Profile::open_in(&root, "worker").unwrap();
        let other = crate::Profile::open_in(&root, "other").unwrap();
        assert_ne!(worker.data_folder(), other.data_folder());
        assert!(
            worker
                .data_folder()
                .ends_with(Path::new(".proofkin-data").join("worker"))
        );
        assert!(!worker.data_folder().starts_with(&worker.dir));
        // Providers deny tasks the profiles folder, so task worktrees live outside it.
        assert!(!worker.worktrees_dir().starts_with(&root));
        assert_ne!(worker.worktrees_dir(), other.worktrees_dir());

        let store = ShareStore::load(&dir.path().join("none.json"), worker.data_folder()).unwrap();
        assert_eq!(store.shares.len(), 1);
        assert_eq!(store.get(DATA_SHARE).unwrap().max_access, Access::ReadOnly);
        let mut store = store;
        assert!(store.remove(DATA_SHARE).is_err());
        assert!(store.add(DATA_SHARE, dir.path(), Access::Full).is_err());
    }

    #[test]
    fn shares_persist_and_names_are_validated() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let file = dir.path().join("shares.json");
        let mut store = ShareStore::with_data(dir.path().join("data"));
        store.add("repo", &repo, Access::WorkspaceWrite).unwrap();
        assert!(store.add("../x", &repo, Access::ReadOnly).is_err());
        assert!(
            store
                .add("missing", &dir.path().join("nope"), Access::ReadOnly)
                .is_err()
        );
        store.save(&file).unwrap();
        assert!(!std::fs::read_to_string(&file).unwrap().contains("\"data\""));

        let loaded = ShareStore::load(&file, dir.path().join("data")).unwrap();
        let names: Vec<&str> = loaded.shares.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, [DATA_SHARE, "repo"]);
        assert_eq!(
            loaded.get("repo").unwrap().path,
            plain_path(std::fs::canonicalize(&repo).unwrap())
        );
        assert!(
            !loaded
                .get("repo")
                .unwrap()
                .path
                .to_string_lossy()
                .starts_with(r"\\?\")
        );
        let mut loaded = loaded;
        assert!(loaded.remove("repo").unwrap());
        assert!(!loaded.remove("repo").unwrap());
    }

    #[test]
    fn effective_access_is_the_lower_of_grant_and_folder() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = ShareStore::with_data(dir.path().to_path_buf());
        store
            .add("repo", dir.path(), Access::WorkspaceWrite)
            .unwrap();
        assert_eq!(
            effective_access(Access::Full, Access::ReadOnly),
            Access::ReadOnly
        );
        assert_eq!(
            effective_access(Access::ReadOnly, Access::Full),
            Access::ReadOnly
        );

        let granted = vec![DATA_SHARE.to_owned(), "repo".to_owned(), "gone".to_owned()];
        let seen = store.visible(&granted, Access::Full);
        assert_eq!(
            seen,
            [
                ShareInfo {
                    name: DATA_SHARE.into(),
                    access: Access::ReadOnly,
                    ..Default::default()
                },
                ShareInfo {
                    name: "repo".into(),
                    access: Access::WorkspaceWrite,
                    ..Default::default()
                },
            ]
        );
        assert!(
            store.visible(&["repo".to_owned()], Access::ReadOnly)[0].access == Access::ReadOnly
        );
        assert!(store.visible(&[], Access::Full).is_empty());
    }

    #[test]
    fn mcp_servers_are_offered_by_name_only_at_enough_access() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = ShareStore::with_data(dir.path().join("data"));
        store.add("repo", dir.path(), Access::Full).unwrap();
        let mut browser = McpServer::local("npx", vec!["@playwright/mcp".into()]);
        browser.access = Access::ReadOnly;
        store.add_mcp("repo", "browser", browser).unwrap();
        store
            .add_mcp(
                "repo",
                "tickets",
                McpServer::remote("https://t.example/mcp", false),
            )
            .unwrap();
        assert!(
            store
                .add_mcp(DATA_SHARE, "browser", McpServer::local("x", vec![]))
                .is_err()
        );
        assert!(
            store
                .add_mcp("gone", "browser", McpServer::local("x", vec![]))
                .is_err()
        );
        assert!(
            store
                .add_mcp("repo", "bad", McpServer::local("", vec![]))
                .is_err()
        );

        let granted = ["repo".to_owned()];
        assert_eq!(
            store.visible(&granted, Access::Full)[0].mcp,
            ["browser", "tickets"]
        );
        assert_eq!(
            store.visible(&granted, Access::ReadOnly)[0].mcp,
            ["browser"]
        );

        // Re-sharing the folder (e.g. to change its checks) keeps its servers.
        store
            .add("repo", dir.path(), Access::WorkspaceWrite)
            .unwrap();
        assert_eq!(store.get("repo").unwrap().mcp.len(), 2);
        assert!(store.remove_mcp("repo", "tickets").unwrap());
        assert!(!store.remove_mcp("repo", "tickets").unwrap());
        let file = dir.path().join("shares.json");
        store.save(&file).unwrap();
        let loaded = ShareStore::load(&file, dir.path().join("data")).unwrap();
        assert_eq!(
            loaded.get("repo").unwrap().mcp,
            store.get("repo").unwrap().mcp
        );
    }
}
