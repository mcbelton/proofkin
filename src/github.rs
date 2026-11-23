//! GitHub: a profile signs in with an account of the GitHub CLI, links repositories
//! (cloned with that account into `~/.proofkin-repos/<profile>/<name>` and shared with
//! peers as a folder with a git worktree per task), keeps them in sync, pushes the
//! branches agents' tasks committed to (`p2p/...`) and opens pull requests.
//!
//! The token never appears on a command line or in the clone's `.git/config`: git
//! gets it as an HTTP header through `GIT_CONFIG_*` environment variables (like
//! `actions/checkout`), and the GitHub CLI through `GH_TOKEN`.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::profile::{DEFAULT_GITHUB_HOST, INHERITED_CREDENTIAL_VARS, validate_name};

#[derive(Clone)]
pub struct GithubCredentials {
    pub user: String,
    pub host: String,
    pub token: String,
}

impl std::fmt::Debug for GithubCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GithubCredentials")
            .field("user", &self.user)
            .field("host", &self.host)
            .field("token", &"<redacted>")
            .finish()
    }
}

/// A repository linked to a profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Repo {
    /// Folder name peers use (the share name).
    pub name: String,
    /// `owner/repo`.
    pub slug: String,
    pub host: String,
    pub path: PathBuf,
    /// The branch the clone tracks (the repository's default branch).
    pub branch: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoStore {
    #[serde(default)]
    pub repos: Vec<Repo>,
}

impl RepoStore {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.is_file() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path)?;
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }

    pub fn get(&self, name: &str) -> Result<&Repo> {
        self.repos
            .iter()
            .find(|r| r.name == name)
            .with_context(|| format!("there is no linked repository '{name}' (see repo list)"))
    }
}

/// `(host, owner/repo)` from `owner/repo`, `host/owner/repo` or an https URL.
pub fn parse_repo(input: &str, default_host: &str) -> Result<(String, String)> {
    let trimmed = input
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .trim_end_matches(".git");
    let parts: Vec<&str> = trimmed.split('/').filter(|p| !p.is_empty()).collect();
    let (host, owner, repo) = match parts.as_slice() {
        [owner, repo] => (default_host, *owner, *repo),
        [host, owner, repo] if host.contains('.') => (*host, *owner, *repo),
        _ => bail!("expected owner/repo or https://{default_host}/owner/repo, got {input:?}"),
    };
    let ok = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            && !s.starts_with('.')
    };
    if !ok(owner) || !ok(repo) || !ok(host) {
        bail!("{input:?} is not a valid repository");
    }
    Ok((host.to_owned(), format!("{owner}/{repo}")))
}

/// GitHub CLI accounts signed in on this machine, with their token's scopes.
pub fn gh_accounts() -> Vec<crate::gh_auth::GhAccount> {
    let out = std::process::Command::new(gh_program())
        .args(["auth", "status", "--json", "hosts"])
        .stdin(Stdio::null())
        .output();
    let Ok(out) = out else { return Vec::new() };
    let hosts: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
    crate::gh_auth::parse_accounts(&hosts)
}

/// The GitHub CLI (`PROOFKIN_GH` overrides it, e.g. for tests).
pub(crate) fn gh_program() -> PathBuf {
    crate::profile::env_setting_os("GH")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("gh"))
}

impl GithubCredentials {
    /// git configuration (through the environment) that authenticates to the host.
    pub(crate) fn git_env(&self) -> Vec<(String, String)> {
        let basic =
            data_encoding::BASE64.encode(format!("x-access-token:{}", self.token).as_bytes());
        vec![
            ("GIT_TERMINAL_PROMPT".into(), "0".into()),
            ("GIT_CONFIG_COUNT".into(), "1".into()),
            (
                "GIT_CONFIG_KEY_0".into(),
                format!("http.https://{}/.extraheader", self.host),
            ),
            (
                "GIT_CONFIG_VALUE_0".into(),
                format!("AUTHORIZATION: basic {basic}"),
            ),
        ]
    }

    async fn git(&self, dir: Option<&Path>, args: &[&str]) -> Result<String> {
        let mut cmd = Command::new("git");
        if let Some(dir) = dir {
            cmd.arg("-C").arg(dir);
        }
        cmd.args(args).stdin(Stdio::null()).kill_on_drop(true);
        for var in INHERITED_CREDENTIAL_VARS {
            cmd.env_remove(var);
        }
        cmd.envs(self.git_env());
        let out = cmd.output().await.context("running git")?;
        if !out.status.success() {
            bail!(
                "git {} failed: {}",
                args.first().copied().unwrap_or_default(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    }

    /// The https URL of `slug` on this account's host.
    pub fn url(&self, slug: &str) -> String {
        format!("https://{}/{slug}.git", self.host)
    }

    /// Clones `url` into `dir` (which must not exist); returns the checked-out branch.
    pub async fn clone_into(&self, url: &str, dir: &Path) -> Result<String> {
        if dir.exists() {
            bail!("{} already exists", dir.display());
        }
        if let Some(parent) = dir.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let target = dir.to_string_lossy().into_owned();
        self.git(None, &["clone", "-q", url, &target])
            .await
            .with_context(|| format!("cloning {url} as {}", self.user))?;
        self.git(Some(dir), &["rev-parse", "--abbrev-ref", "HEAD"])
            .await
    }

    /// Fetches and fast-forwards the clone's branch; returns the new head commit.
    pub async fn sync(&self, repo: &Repo) -> Result<String> {
        self.git(Some(&repo.path), &["fetch", "-q", "--prune", "origin"])
            .await?;
        let upstream = format!("origin/{}", repo.branch);
        self.git(Some(&repo.path), &["merge", "-q", "--ff-only", &upstream])
            .await
            .context("the clone has local changes on its branch; resolve them in the folder")?;
        self.git(Some(&repo.path), &["rev-parse", "--short", "HEAD"])
            .await
    }

    /// Fetches the repository's branch into the clone (`origin/<branch>`) unless the
    /// clone fetched within `max_age`, so the task branches' states are checked against
    /// what the repository has now. Returns whether it fetched.
    pub async fn refresh_base(&self, repo: &Repo, max_age: std::time::Duration) -> Result<bool> {
        let dir = Some(repo.path.as_path());
        let fetched = self
            .git(dir, &["rev-parse", "--git-path", "FETCH_HEAD"])
            .await
            .map(|p| repo.path.join(p))
            .ok()
            .and_then(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
            .and_then(|t| t.elapsed().ok());
        if fetched.is_some_and(|age| age < max_age) {
            return Ok(false);
        }
        self.git(dir, &["fetch", "-q", "origin", &repo.branch])
            .await?;
        Ok(true)
    }

    /// Pushes local branch `branch` to the repository.
    pub async fn push(&self, repo: &Repo, branch: &str) -> Result<()> {
        if !branch.starts_with("p2p/") {
            bail!("only branches agents' tasks made (p2p/...) are pushed from here");
        }
        let refspec = format!("refs/heads/{branch}:refs/heads/{branch}");
        self.git(Some(&repo.path), &["push", "-q", "origin", &refspec])
            .await
            .map(drop)
    }

    /// Deletes task branch `branch`: on GitHub first (if it was pushed), then in the
    /// clone, so a failure leaves it listed to try again.
    pub async fn delete_branch(&self, repo: &Repo, branch: &str) -> Result<()> {
        if !branch.starts_with("p2p/") {
            bail!("only branches agents' tasks made (p2p/...) are deleted from here");
        }
        let head = format!("refs/heads/{branch}");
        let dir = Some(repo.path.as_path());
        if !self
            .git(dir, &["ls-remote", "--heads", "origin", &head])
            .await?
            .is_empty()
        {
            self.git(dir, &["push", "-q", "origin", "--delete", &head])
                .await?;
        }
        self.git(dir, &["branch", "-D", branch]).await.map(drop)
    }

    /// Publishes the signed record of `branch`'s head commit, and only that one, to the
    /// repository's notes ref: it is added to a copy of the repository's notes, kept in
    /// a separate local ref, which is then pushed.
    pub async fn push_notes(&self, repo: &Repo, branch: &str) -> Result<()> {
        let dir = Some(repo.path.as_path());
        let notes = format!("refs/notes/{}", crate::worktree::NOTES_REF);
        let published = format!("{notes}-published");
        let head = self
            .git(
                dir,
                &["rev-parse", &format!("refs/heads/{branch}^{{commit}}")],
            )
            .await?;
        let record = self
            .git(dir, &["notes", "--ref", &notes, "list", &head])
            .await
            .with_context(|| format!("{branch}'s head commit has no record"))?;
        if self
            .git(dir, &["ls-remote", "origin", &notes])
            .await?
            .is_empty()
        {
            self.git(dir, &["update-ref", "-d", &published]).await.ok();
        } else {
            self.git(
                dir,
                &["fetch", "-q", "origin", &format!("+{notes}:{published}")],
            )
            .await?;
        }
        self.git(
            dir,
            &[
                "-c",
                "user.name=proofkin",
                "-c",
                "user.email=proofkin@localhost",
                "notes",
                "--ref",
                &published,
                "add",
                "-f",
                "-C",
                &record,
                &head,
            ],
        )
        .await?;
        self.git(
            dir,
            &["push", "-q", "origin", &format!("{published}:{notes}")],
        )
        .await
        .map(drop)
    }

    /// [`branch_proof`] for a pull request about to be opened: publishes the branch's
    /// record first (best effort) so the proof can say where it is.
    pub async fn pr_proof(
        &self,
        repo: &Repo,
        branch: &str,
        profile: &crate::Profile,
    ) -> Result<Option<Proof>> {
        let pushed = self.push_notes(repo, branch).await.is_ok();
        branch_proof(&repo.path, branch, profile, pushed).await
    }

    /// Opens a pull request from `head` into the repository's branch; returns its URL.
    pub async fn create_pr(
        &self,
        repo: &Repo,
        head: &str,
        title: &str,
        body: &str,
    ) -> Result<String> {
        let mut cmd = Command::new(gh_program());
        cmd.args([
            "pr",
            "create",
            "--repo",
            &format!("{}/{}", repo.host, repo.slug),
            "--base",
            &repo.branch,
            "--head",
            head,
            "--title",
            title,
            "--body",
            body,
        ])
        .stdin(Stdio::null())
        .kill_on_drop(true);
        for var in INHERITED_CREDENTIAL_VARS {
            cmd.env_remove(var);
        }
        cmd.env("GH_TOKEN", &self.token)
            .env("GH_HOST", &self.host)
            .env("GH_PROMPT_DISABLED", "1");
        let out = cmd
            .output()
            .await
            .context("running the GitHub CLI (gh) to open the pull request")?;
        if !out.status.success() {
            bail!(
                "opening the pull request failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let text = String::from_utf8_lossy(&out.stdout);
        Ok(text
            .lines()
            .rev()
            .find(|l| l.starts_with("https://"))
            .unwrap_or(text.trim())
            .to_owned())
    }
}

/// A branch an agent's task committed to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TaskBranch {
    pub name: String,
    pub unix: u64,
    pub subject: String,
    /// The issues its commit is for (its `Refs:` trailers, see [`crate::tracker`]): a
    /// pull request from it closes them.
    pub issues: Vec<String>,
}

/// A pull request from a task branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BranchPr {
    pub number: u64,
    /// `OPEN`, `MERGED` or `CLOSED`.
    pub state: String,
    pub url: String,
    /// Its head commit.
    #[serde(skip)]
    pub head: String,
    /// It has the branch as it is: the branch has no commits since.
    pub current: bool,
}

/// Where a task branch's work is (see [`branch_states`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct BranchState {
    /// The newest pull request from it, if there is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pull_request: Option<BranchPr>,
    /// Its commit is on the repository's branch already (merged some other way).
    pub in_base: bool,
    /// It is for issues of the repository (its `Refs:`), and all of them are closed.
    pub issues_closed: bool,
    /// Its commits that aren't on the repository's branch.
    pub ahead: u32,
    /// What a pull request from it would change: files, and lines added and removed.
    pub files: u32,
    pub added: u64,
    pub removed: u64,
    /// The repository's branch has the same content in every file it changes: its work
    /// is there, merged some other way (squashed or cherry-picked).
    pub same_as_base: bool,
    /// Deleting it loses nothing: its work is on the repository's branch, or a pull
    /// request merged it as it is.
    pub safe_to_delete: bool,
    /// Pull requests from other branches that closed its issues: they may have its work,
    /// done again (by a step that applied it, say), to compare before deleting it.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub closed_by: Vec<ClosingPr>,
    /// Its head commit.
    #[serde(skip)]
    pub head: String,
}

/// A pull request from another branch that closed one of a task branch's issues.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClosingPr {
    pub number: u64,
    pub url: String,
    /// The branch it was from.
    pub branch: String,
}

/// The most issues one [`branch_states`] query asks about.
const MAX_BRANCH_ISSUES: usize = 50;

/// Where the work of each of `branches` (of `repo`) is: the newest of the repository's
/// last 100 pull requests from it (and whether it has the branch as it is), whether its
/// commit or its changes are on the repository's branch, what a pull request from it
/// would change, whether the issues it is for are closed (and by which pull requests
/// from other branches), and so whether deleting it loses anything. One GraphQL query
/// for all of them.
pub async fn branch_states(
    api: &dyn crate::tracker::Api,
    repo: &Repo,
    branches: &[TaskBranch],
) -> Result<std::collections::BTreeMap<String, BranchState>> {
    let (owner, name) = repo
        .slug
        .split_once('/')
        .with_context(|| format!("{} isn't owner/name", repo.slug))?;
    // The issues of this repository its branches are for, by number.
    let own = |r: &str| {
        let (slug, n) = r.split_once('#')?;
        slug.eq_ignore_ascii_case(&repo.slug)
            .then(|| n.parse::<u64>().ok())
            .flatten()
    };
    let numbers: std::collections::BTreeSet<u64> = branches
        .iter()
        .flat_map(|b| b.issues.iter().filter_map(|r| own(r)))
        .take(MAX_BRANCH_ISSUES)
        .collect();
    let mut fields = String::from(
        "pullRequests(first: 100, orderBy: {field: UPDATED_AT, direction: DESC}) { nodes { number state url headRefName headRefOid } }",
    );
    for n in &numbers {
        fields += &format!(
            " i{n}: issueOrPullRequest(number: {n}) {{ ... on Issue {{ state timelineItems(itemTypes: [CLOSED_EVENT], last: 1) {{ nodes {{ ... on ClosedEvent {{ closer {{ ... on PullRequest {{ number url headRefName }} }} }} }} }} }} ... on PullRequest {{ state }} }}"
        );
    }
    let query = format!(
        "query BranchStates($owner: String!, $name: String!) {{ repository(owner: $owner, name: $name) {{ {fields} }} }}"
    );
    let reply = api
        .graphql(&query, serde_json::json!({ "owner": owner, "name": name }))
        .await?;
    let data = &reply.data["repository"];
    if data.is_null() {
        bail!(
            "{}",
            reply
                .errors
                .first()
                .map(|e| e.message.clone())
                .unwrap_or_else(|| format!("GitHub has no repository {}", repo.slug))
        );
    }
    let prs = data["pullRequests"]["nodes"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let closed = |n: u64| {
        data[format!("i{n}")]["state"]
            .as_str()
            .is_some_and(|s| s != "OPEN")
    };
    // The pull request that closed issue `n`, if one did.
    let closer = |n: u64| {
        let pr = data[format!("i{n}")]
            .pointer("/timelineItems/nodes/0/closer")
            .filter(|c| c.get("number").is_some())?;
        Some(ClosingPr {
            number: pr["number"].as_u64()?,
            url: pr["url"].as_str()?.to_owned(),
            branch: pr["headRefName"].as_str().unwrap_or_default().to_owned(),
        })
    };
    let mut states = std::collections::BTreeMap::new();
    // What git says about each branch, a few at a time.
    let base = base_ref(repo).await;
    let names: Vec<String> = branches.iter().map(|b| b.name.clone()).collect();
    let local: Vec<BranchState> = {
        use futures_util::StreamExt;
        futures_util::stream::iter(names)
            .map(|name| {
                let base = base.clone();
                async move { local_state(repo, base.as_deref(), &name).await }
            })
            .buffered(8)
            .collect()
            .await
    };
    for (branch, local) in branches.iter().zip(local) {
        let mut pull_request = prs
            .iter()
            .find(|p| p["headRefName"].as_str() == Some(branch.name.as_str()))
            .and_then(|p| {
                Some(BranchPr {
                    number: p["number"].as_u64()?,
                    state: p["state"].as_str()?.to_owned(),
                    url: p["url"].as_str()?.to_owned(),
                    head: p["headRefOid"].as_str().unwrap_or_default().to_owned(),
                    current: false,
                })
            });
        if let Some(pr) = pull_request.as_mut() {
            // Its head is the branch's, or comes after it (pushed from elsewhere).
            pr.current = !local.head.is_empty()
                && !pr.head.is_empty()
                && (pr.head == local.head
                    || local_git(
                        &repo.path,
                        &["merge-base", "--is-ancestor", &local.head, &pr.head],
                    )
                    .await
                    .is_ok_and(|(ok, _)| ok));
        }
        let refs: Vec<Option<u64>> = branch.issues.iter().map(|r| own(r)).collect();
        let issues_closed = !refs.is_empty()
            && refs
                .iter()
                .all(|n| n.is_some_and(|n| numbers.contains(&n) && closed(n)));
        let merged = pull_request
            .as_ref()
            .is_some_and(|p| p.state == "MERGED" && p.current);
        let mut closed_by: Vec<ClosingPr> = Vec::new();
        for c in refs.iter().flatten().filter_map(|n| closer(*n)) {
            if c.branch != branch.name && !closed_by.iter().any(|b| b.number == c.number) {
                closed_by.push(c);
            }
        }
        states.insert(
            branch.name.clone(),
            BranchState {
                pull_request,
                issues_closed,
                safe_to_delete: local.in_base || local.same_as_base || merged,
                closed_by,
                ..local
            },
        );
    }
    Ok(states)
}

/// The ref of the repository's branch in the clone: as last fetched, else its own.
async fn base_ref(repo: &Repo) -> Option<String> {
    for base in [
        format!("refs/remotes/origin/{}", repo.branch),
        format!("refs/heads/{}", repo.branch),
    ] {
        if local_git(&repo.path, &["rev-parse", "--verify", "--quiet", &base])
            .await
            .is_ok_and(|(ok, _)| ok)
        {
            return Some(base);
        }
    }
    None
}

/// Runs git in `dir` (local operations only: no credentials); its success and output.
async fn local_git(dir: &Path, args: &[&str]) -> Result<(bool, String)> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "core.quotepath=off"])
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .context("running git")?;
    Ok((
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    ))
}

/// The files a pull request from `head` onto `base` would change: path, lines added
/// and removed (none for a binary file).
async fn changed_files(dir: &Path, base: &str, head: &str) -> Vec<(String, Option<(u64, u64)>)> {
    let range = format!("{base}...{head}");
    let (_, out) = local_git(dir, &["diff", "--numstat", "-M", &range])
        .await
        .unwrap_or_default();
    out.lines()
        .filter_map(|l| {
            let mut parts = l.splitn(3, '\t');
            let (a, r, path) = (parts.next()?, parts.next()?, parts.next()?);
            Some((path.to_owned(), a.parse().ok().zip(r.parse().ok())))
        })
        .collect()
}

/// Most files compared with the repository's branch to tell whether it has a task
/// branch's changes already.
const MAX_COMPARED_FILES: usize = 200;

/// What git says about task branch `branch`: whether its commit is on the repository's
/// branch (`base`, see [`base_ref`]), its commits that aren't, what a pull request from
/// it would change, and whether the repository's branch has that content already.
async fn local_state(repo: &Repo, base: Option<&str>, branch: &str) -> BranchState {
    let head = format!("refs/heads/{branch}");
    let mut state = BranchState {
        in_base: in_base(repo, branch).await,
        head: local_git(&repo.path, &["rev-parse", "--verify", "--quiet", &head])
            .await
            .map(|(_, sha)| sha.trim().to_owned())
            .unwrap_or_default(),
        ..BranchState::default()
    };
    let Some(base) = base else {
        return state;
    };
    let range = format!("{base}..{head}");
    state.ahead = local_git(&repo.path, &["rev-list", "--count", &range])
        .await
        .ok()
        .and_then(|(_, n)| n.trim().parse().ok())
        .unwrap_or(0);
    let files = changed_files(&repo.path, base, &head).await;
    state.files = files.len() as u32;
    for (_, lines) in &files {
        let (a, r) = lines.unwrap_or_default();
        state.added += a;
        state.removed += r;
    }
    state.same_as_base = state.in_base
        // Nothing to lose: it changes no files.
        || files.is_empty()
        || (files.len() <= MAX_COMPARED_FILES && {
            // Renamed files are named `old => new`: their new path is compared.
            let paths: Vec<String> = files.iter().map(|(p, _)| new_path(p)).collect();
            let mut args = vec!["diff", "--quiet", head.as_str(), base, "--"];
            args.extend(paths.iter().map(String::as_str));
            local_git(&repo.path, &args).await.is_ok_and(|(same, _)| same)
        });
    state
}

/// A `--numstat` path as it is after the change: `a/{b => c}/d` is `a/c/d`, `a => b` is `b`.
fn new_path(path: &str) -> String {
    match (path.find('{'), path.find(" => "), path.find('}')) {
        (Some(open), Some(arrow), Some(close)) if open < arrow && arrow < close => {
            let joined = format!(
                "{}{}{}",
                &path[..open],
                &path[arrow + 4..close],
                &path[close + 1..]
            );
            joined.replace("//", "/")
        }
        (_, Some(arrow), _) => path[arrow + 4..].to_owned(),
        _ => path.to_owned(),
    }
}

/// A commit on a task branch that isn't on the repository's branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BranchCommit {
    pub commit: String,
    pub unix: u64,
    pub subject: String,
}

/// A file a pull request from a task branch would change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileChange {
    pub path: String,
    /// Lines added and removed (none for a binary file).
    pub added: Option<u64>,
    pub removed: Option<u64>,
}

/// What a pull request from a task branch would carry, to review before opening it
/// (or deleting the branch).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BranchChanges {
    pub branch: String,
    /// The repository's branch it is compared with.
    pub base: String,
    /// Its commits that aren't on the repository's branch, newest first.
    pub commits: Vec<BranchCommit>,
    pub files: Vec<FileChange>,
    /// The unified diff, as a pull request shows it (from where the branches split).
    pub diff: String,
    /// The diff was cut at [`MAX_DIFF_BYTES`].
    pub truncated: bool,
}

/// The most diff a review shows.
pub const MAX_DIFF_BYTES: usize = 400_000;

/// The changes on task branch `branch` of `repo`: its commits, files and diff against
/// the repository's branch (as last fetched).
pub async fn branch_changes(repo: &Repo, branch: &str) -> Result<BranchChanges> {
    if !branch.starts_with("p2p/") {
        bail!("only branches agents' tasks made (p2p/...) are shown here");
    }
    let head = format!("refs/heads/{branch}");
    if !local_git(&repo.path, &["rev-parse", "--verify", "--quiet", &head])
        .await?
        .0
    {
        bail!("{} has no task branch {branch}", repo.slug);
    }
    let base = base_ref(repo)
        .await
        .with_context(|| format!("the clone of {} has no branch {}", repo.slug, repo.branch))?;
    let range = format!("{base}..{head}");
    let (_, log) = local_git(
        &repo.path,
        &["log", "-n", "50", "--format=%h%x09%ct%x09%s", &range],
    )
    .await?;
    let commits = log
        .lines()
        .filter_map(|l| {
            let mut parts = l.splitn(3, '\t');
            Some(BranchCommit {
                commit: parts.next()?.to_owned(),
                unix: parts.next()?.parse().ok()?,
                subject: parts.next().unwrap_or_default().to_owned(),
            })
        })
        .collect();
    let files = changed_files(&repo.path, &base, &head)
        .await
        .into_iter()
        .map(|(path, lines)| FileChange {
            path,
            added: lines.map(|(a, _)| a),
            removed: lines.map(|(_, r)| r),
        })
        .collect();
    let split = format!("{base}...{head}");
    let (_, mut diff) = local_git(
        &repo.path,
        &["diff", "-M", "--no-color", "--no-ext-diff", &split],
    )
    .await?;
    let truncated = diff.len() > MAX_DIFF_BYTES;
    if truncated {
        let mut cut = MAX_DIFF_BYTES;
        while !diff.is_char_boundary(cut) {
            cut -= 1;
        }
        // At a line's end, so the last line shown is whole.
        cut = diff[..cut].rfind('\n').map_or(cut, |n| n + 1);
        diff.truncate(cut);
    }
    Ok(BranchChanges {
        branch: branch.to_owned(),
        base: repo.branch.clone(),
        commits,
        files,
        diff,
        truncated,
    })
}

/// Whether `branch`'s commit is on the repository's branch (as last fetched, or the
/// clone's own).
async fn in_base(repo: &Repo, branch: &str) -> bool {
    let head = format!("refs/heads/{branch}");
    for base in [
        format!("refs/remotes/origin/{}", repo.branch),
        format!("refs/heads/{}", repo.branch),
    ] {
        let status = Command::new("git")
            .arg("-C")
            .arg(&repo.path)
            .args(["merge-base", "--is-ancestor", &head, &base])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
        if status.is_ok_and(|s| s.success()) {
            return true;
        }
    }
    false
}

/// The clone's `p2p/...` branches, newest first.
pub async fn task_branches(repo: &Repo) -> Result<Vec<TaskBranch>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(&repo.path)
        .args([
            "for-each-ref",
            "--sort=-committerdate",
            "--format=%(refname:short)%09%(committerdate:unix)%09%(trailers:key=Refs,valueonly,separator=%x2C)%09%(subject)",
            "refs/heads/p2p/",
        ])
        .stdin(Stdio::null())
        .output()
        .await
        .context("running git")?;
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let mut parts = l.splitn(4, '\t');
            Some(TaskBranch {
                name: parts.next()?.to_owned(),
                unix: parts.next()?.parse().ok()?,
                issues: parts
                    .next()?
                    .split(',')
                    .map(str::trim)
                    .filter(|r| crate::tracker::valid_ref(r))
                    .map(str::to_owned)
                    .collect(),
                subject: parts.next().unwrap_or_default().to_owned(),
            })
        })
        .collect())
}

/// A folder name for `slug` (`owner/web-app` -> `web-app`).
pub fn default_name(slug: &str) -> Result<String> {
    let name = slug.rsplit('/').next().unwrap_or(slug).replace('.', "-");
    validate_name(&name).with_context(|| format!("pick a name with --as for {slug}"))?;
    Ok(name)
}

/// Links `input` (`owner/repo` or a URL) to `profile`: clones it with the profile's
/// GitHub account and shares it as folder `name` (default: the repository name)
/// with a worktree per task. Returns the repository and the profile's new shares.
pub async fn link(
    profile: &crate::Profile,
    input: &str,
    name: Option<&str>,
    access: crate::Access,
    checks: std::collections::BTreeMap<String, String>,
) -> Result<(Repo, crate::shares::ShareStore)> {
    let creds = profile.github()?;
    let (host, slug) = parse_repo(input, &creds.host)?;
    if host != creds.host {
        bail!(
            "{slug} is on {host}, but profile '{}' is signed in to {}",
            profile.name,
            creds.host
        );
    }
    let url = creds.url(&slug);
    link_url(profile, &creds, &url, &host, &slug, name, access, checks).await
}

/// [`link`] from an explicit clone URL.
#[allow(clippy::too_many_arguments)]
pub async fn link_url(
    profile: &crate::Profile,
    creds: &GithubCredentials,
    url: &str,
    host: &str,
    slug: &str,
    name: Option<&str>,
    access: crate::Access,
    checks: std::collections::BTreeMap<String, String>,
) -> Result<(Repo, crate::shares::ShareStore)> {
    let name = match name {
        Some(n) => {
            validate_name(n).context("invalid folder name")?;
            n.to_owned()
        }
        None => default_name(slug)?,
    };
    let mut repos = RepoStore::load(&profile.repos_path())?;
    if repos.repos.iter().any(|r| r.name == name) {
        bail!("a repository named '{name}' is already linked");
    }
    let mut shares = profile.shares()?;
    if shares.get(&name).is_some() {
        bail!(
            "profile '{}' already shares a folder named '{name}'; pick another name",
            profile.name
        );
    }
    let path = profile.repos_dir().join(&name);
    let branch = creds.clone_into(url, &path).await?;
    shares.add_with(&name, &path, access, checks, true)?;
    shares.save(&profile.shares_path())?;
    let repo = Repo {
        name,
        slug: slug.to_owned(),
        host: host.to_owned(),
        path: shares.shares.last().map(|s| s.path.clone()).unwrap_or(path),
        branch,
    };
    repos.repos.push(repo.clone());
    repos.save(&profile.repos_path())?;
    Ok((repo, shares))
}

/// Unlinks repository `name`: stops sharing it and forgets it; with `delete`, also
/// removes the clone. Returns the profile's new shares.
pub fn unlink(
    profile: &crate::Profile,
    name: &str,
    delete: bool,
) -> Result<crate::shares::ShareStore> {
    let mut repos = RepoStore::load(&profile.repos_path())?;
    let repo = repos.get(name)?.clone();
    repos.repos.retain(|r| r.name != name);
    repos.save(&profile.repos_path())?;
    let mut shares = profile.shares()?;
    shares.remove(name)?;
    shares.save(&profile.shares_path())?;
    if delete && repo.path.starts_with(profile.repos_dir()) {
        std::fs::remove_dir_all(&repo.path)
            .with_context(|| format!("removing {}", repo.path.display()))?;
    }
    Ok(shares)
}

/// The default host for repositories (github.com).
pub fn default_host() -> &'static str {
    DEFAULT_GITHUB_HOST
}

/// What a pull request from a task's branch carries as proof (see [`branch_proof`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Proof {
    /// In a line, for the web UI: who signed it and how its checks went.
    pub summary: String,
    /// The section appended to the pull request's description.
    pub markdown: String,
}

/// Who asked for the work and what it was part of, and whether the record was pushed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProofContext {
    /// The name this agent's allowlist has for the record's requester.
    pub requester: Option<String>,
    /// What the task was part of on the requester's side (its audit entry's origin),
    /// e.g. "step ui of run 20261121-183724-726e7".
    pub origin: Option<String>,
    /// Whether the notes ref was pushed to the repository with the branch.
    pub notes_pushed: bool,
}

/// The audit entry's origin of the task record `e` is for: the latest entry of the same
/// requester and task id that started no later than the second of its commit (commit
/// times have whole-second resolution).
fn task_origin(
    entries: &[crate::audit::AuditEntry],
    requester_id: &str,
    task_id: u64,
    commit_unix: u64,
) -> Option<String> {
    entries
        .iter()
        .filter(|a| {
            a.peer_id == requester_id
                && a.task_id == task_id
                && a.ts_ms < commit_unix.saturating_add(1).saturating_mul(1000)
        })
        .max_by_key(|a| a.ts_ms)
        .and_then(|a| a.origin.clone())
}

/// The proof for a pull request from task branch `branch` of the clone at `repo`:
/// the record the agent that made its head commit signed for it (kept as a git note
/// on the commit, see [`crate::worktree::NOTES_REF`]), checked here, with who asked
/// for the work (by `profile`'s allowlist and audit log) and the claims of this
/// profile's evidence run that has the record, if any. `notes_pushed` says whether the
/// notes ref went to the repository. None if the head commit has no record (a task of
/// an older version, or someone else's).
pub async fn branch_proof(
    repo: &Path,
    branch: &str,
    profile: &crate::Profile,
    notes_pushed: bool,
) -> Result<Option<Proof>> {
    use crate::evidence::Evidence;
    if !branch.starts_with("p2p/") {
        bail!("only branches agents' tasks made (p2p/...) carry proof");
    }
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--verify", "--quiet"])
        .arg(format!("refs/heads/{branch}^{{commit}}"))
        .stdin(Stdio::null())
        .output()
        .await
        .context("running git")?;
    if !out.status.success() {
        bail!("there is no branch {branch}");
    }
    let head = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    let Some(note) = crate::worktree::read_note(repo, &head).await? else {
        return Ok(None);
    };
    let evidence: Evidence =
        serde_json::from_str(&note).context("reading the commit's signed record")?;
    evidence
        .verify_record()
        .context("the commit's signed record doesn't verify")?;
    let git = evidence
        .git
        .as_ref()
        .filter(|g| g.commit.as_deref() == Some(head.as_str()))
        .with_context(|| format!("the signed record is for another commit than {branch}'s"))?;
    let run = crate::runs::RunRecord::with_evidence(&profile.runs_dir(), &evidence.signature);
    // Names and origins only decorate the proof: without them it says less, not nothing.
    let requester = profile.trust().ok().and_then(|t| {
        t.peers
            .iter()
            .find(|p| p.id.to_string() == evidence.requester_id)
            .map(|p| p.name.clone())
    });
    let commit_unix = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["show", "-s", "--format=%ct", &head])
        .stdin(Stdio::null())
        .output()
        .await
        .ok()
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .trim()
                .parse::<u64>()
                .ok()
        });
    let origin = commit_unix.and_then(|unix| {
        let entries = crate::audit::AuditLog::read(&profile.audit_path(), 100_000).ok()?;
        task_origin(&entries, &evidence.requester_id, evidence.task_id, unix)
    });
    let ctx = ProofContext {
        requester,
        origin,
        notes_pushed,
    };
    let checks = if evidence.checks.is_empty() {
        "no checks ran".to_owned()
    } else {
        evidence
            .checks
            .iter()
            .map(|c| format!("{} {}", if c.passed { "✓" } else { "✗" }, c.name))
            .collect::<Vec<_>>()
            .join(", ")
    };
    Ok(Some(Proof {
        summary: format!(
            "Signed by {} for {}: {checks}{}{}{}",
            evidence.signer,
            requester_label(&evidence, &ctx),
            run.as_ref()
                .map(|r| format!("; evidence run {}: {}", r.id, r.verdict_line()))
                .unwrap_or_default(),
            ctx.origin
                .as_ref()
                .map(|o| format!("; as {o}"))
                .unwrap_or_default(),
            if ctx.notes_pushed {
                ""
            } else {
                "; the record stays on this agent's machine"
            }
        ),
        markdown: crate::redact::redact(&proof_markdown(&evidence, git, run.as_ref(), &ctx)).0,
    }))
}

/// How to name the requester of `e`: its allowlisted name, else the key's start.
fn requester_label(e: &crate::evidence::Evidence, ctx: &ProofContext) -> String {
    let prefix: String = e.requester_id.chars().take(10).collect();
    match ctx.requester.as_deref() {
        Some(name) => format!("{} ({prefix})", clean_name(name)),
        None => prefix,
    }
}

fn clean_name(name: &str) -> String {
    name.replace(['`', '*', '\r', '\n'], "")
}

/// "step ui of boss's evidence run R (ask boss for it: ...)" for origin "step ui of run R".
fn origin_sentence(origin: &str, requester: Option<&str>) -> String {
    let who = match requester {
        Some(n) => clean_name(n),
        None => "the requester".to_owned(),
    };
    match origin.split_once(" of run ") {
        Some((what, run)) => format!(
            "as {what} of {who}'s evidence run {run} (ask {who} for it: `proofkin runs verify --file`)"
        ),
        None => format!("as {origin}"),
    }
}

/// The pull request section for commit record `e` and the evidence run that has it.
fn proof_markdown(
    e: &crate::evidence::Evidence,
    git: &crate::evidence::GitCommit,
    run: Option<&crate::runs::RunRecord>,
    ctx: &ProofContext,
) -> String {
    let code = |text: &str| text.replace('`', "'");
    let plural = |n: usize| if n == 1 { "" } else { "s" };
    let commit = git.commit.as_deref().unwrap_or_default();
    let prefix: String = e.requester_id.chars().take(10).collect();
    let requester = match ctx.requester.as_deref() {
        Some(name) => format!("**{}** ({prefix})", clean_name(name)),
        None => format!("agent `{prefix}`"),
    };
    let mut md = format!(
        "\n\n---\n\n### Proof\n\nCommit `{}` ({} file{}) was made by the agent **{}**{} for {requester}, \
         and signed with its key `{}`.\n\n",
        commit.chars().take(12).collect::<String>(),
        git.files.len(),
        plural(git.files.len()),
        code(&e.signer),
        e.provider.map(|p| format!(" ({p})")).unwrap_or_default(),
        e.signer_id
    );
    if let Some(origin) = &ctx.origin {
        md += &format!(
            "The task was done {}.\n\n",
            origin_sentence(origin, ctx.requester.as_deref())
        );
    }

    if e.checks.is_empty() {
        md += "No checks ran after the model: the change rests on the model's word and your review.\n";
    } else {
        md += &format!(
            "Checks its runtime ran on the change after the model finished ({} attempt{}):\n\n",
            e.attempts,
            plural(e.attempts as usize)
        );
        for c in &e.checks {
            md += &format!(
                "- {} `{}`: {} in {:.1}s\n",
                if c.passed { "✅" } else { "❌" },
                code(&c.name),
                c.exit_code
                    .map(|code| format!("exit {code}"))
                    .unwrap_or_else(|| "no exit code".into()),
                c.millis as f64 / 1000.0
            );
        }
    }
    if let Some(run) = run {
        md += &format!(
            "\nEvidence run `{}` of {}: **{}**\n",
            run.id,
            code(&run.boss),
            run.verdict_line()
        );
        let results = run.claim_results();
        if !results.is_empty() {
            md += "\n| Claim | Statement | Verdict |\n| --- | --- | --- |\n";
            for r in &results {
                md += &format!(
                    "| {} | {} | {} |\n",
                    r.claim.id,
                    r.claim.text.replace('|', "\\|").replace(['\r', '\n'], " "),
                    r.verdict.label()
                );
            }
        }
    }
    md += if ctx.notes_pushed {
        "\n<sub>Proofkin checked the record's Ed25519 signature before attaching it. The record is the git note \
         `refs/notes/proofkin` on the commit in this repository; fetch it with \
         `git fetch origin refs/notes/proofkin:refs/notes/proofkin`.</sub>\n"
    } else {
        "\n<sub>Proofkin checked the record's Ed25519 signature before attaching it. The record is the git note \
         `refs/notes/proofkin` on the commit, but pushing it to this repository failed: it stays on this \
         agent's machine.</sub>\n"
    };
    md
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renamed_files_are_compared_at_their_new_path() {
        assert_eq!(new_path("src/a.rs"), "src/a.rs");
        assert_eq!(new_path("old.txt => new.txt"), "new.txt");
        assert_eq!(new_path("src/{old => new}/a.rs"), "src/new/a.rs");
        assert_eq!(new_path("src/{ => sub}/a.rs"), "src/sub/a.rs");
        assert_eq!(new_path("src/{sub => }/a.rs"), "src/a.rs");
    }

    #[test]
    fn repository_names_are_parsed_strictly() {
        let gh = "github.com";
        assert_eq!(
            parse_repo("mcbelton/proofkin", gh).unwrap(),
            (gh.into(), "mcbelton/proofkin".into())
        );
        assert_eq!(
            parse_repo("https://github.com/mcbelton/proofkin.git", gh).unwrap(),
            (gh.into(), "mcbelton/proofkin".into())
        );
        assert_eq!(
            parse_repo("corp.ghe.com/team/app", gh).unwrap(),
            ("corp.ghe.com".into(), "team/app".into())
        );
        for bad in [
            "proofkin",
            "a/b/c/d",
            "owner/../x",
            "owner/re po",
            "https://github.com/o/r?x=1",
            "-x/../y",
        ] {
            assert!(parse_repo(bad, gh).is_err(), "{bad}");
        }
        assert_eq!(default_name("owner/web.app").unwrap(), "web-app");
        let creds = GithubCredentials {
            user: "u".into(),
            host: "github.com".into(),
            token: "s3cret".into(),
        };
        assert!(!format!("{creds:?}").contains("s3cret"));
    }

    fn record() -> (crate::evidence::Evidence, crate::evidence::GitCommit) {
        let key = iroh::SecretKey::generate();
        let requester = iroh::SecretKey::generate();
        let e = crate::evidence::Evidence::new(
            crate::evidence::EvidenceKind::Task,
            "worker",
            key.public(),
            requester.public(),
            7,
        );
        let git = crate::evidence::GitCommit {
            branch: "p2p/boss/7-1".into(),
            base: "abc".into(),
            commit: Some("0123456789abcdef".into()),
            files: vec!["a.txt".into()],
            patch_sha256: None,
        };
        (e, git)
    }

    #[test]
    fn the_proof_names_who_asked_what_it_was_part_of_and_where_the_record_is() {
        let (e, git) = record();
        let prefix: String = e.requester_id.chars().take(10).collect();
        let ctx = ProofContext {
            requester: Some("boss".into()),
            origin: Some("step ui of run 20261121-183724-726e7".into()),
            notes_pushed: true,
        };
        let md = proof_markdown(&e, &git, None, &ctx);
        assert!(md.contains(&format!("for **boss** ({prefix})")), "{md}");
        assert!(
            md.contains(
                "as step ui of boss's evidence run 20261121-183724-726e7 \
                 (ask boss for it: `proofkin runs verify --file`)"
            ),
            "{md}"
        );
        assert!(
            md.contains("on the commit in this repository")
                && md.contains("git fetch origin refs/notes/proofkin:refs/notes/proofkin"),
            "{md}"
        );
        assert!(!md.contains("stays on this agent's machine"), "{md}");
    }

    #[test]
    fn the_proof_without_a_name_or_origin_falls_back_and_admits_an_unpushed_record() {
        let (e, git) = record();
        let prefix: String = e.requester_id.chars().take(10).collect();
        let md = proof_markdown(&e, &git, None, &ProofContext::default());
        assert!(md.contains(&format!("for agent `{prefix}`")), "{md}");
        assert!(!md.contains("The task was done"), "{md}");
        assert!(
            md.contains("it stays on this agent's machine") && !md.contains("git fetch"),
            "{md}"
        );
        // An origin that isn't an evidence run has nothing to ask for.
        let ctx = ProofContext {
            origin: Some("task #3".into()),
            ..ProofContext::default()
        };
        let md = proof_markdown(&e, &git, None, &ctx);
        assert!(md.contains("The task was done as task #3."), "{md}");
        let ctx = ProofContext {
            origin: Some("step fix of run r1".into()),
            ..ProofContext::default()
        };
        assert!(
            proof_markdown(&e, &git, None, &ctx).contains("ask the requester for it"),
            "unnamed requester"
        );
    }

    #[test]
    fn the_task_a_record_is_for_is_the_latest_one_that_started_before_its_commit() {
        let entry =
            |peer_id: &str, task_id: u64, ts_ms: u64, origin: &str| crate::audit::AuditEntry {
                ts_ms,
                peer: "boss".into(),
                peer_id: peer_id.into(),
                task_id,
                provider: crate::llm::Provider::Copilot,
                share: String::new(),
                access: None,
                outcome: crate::audit::Outcome::Done,
                duration_ms: 1,
                prompt_sha256: String::new(),
                prompt_chars: 0,
                error: None,
                origin: Some(origin.into()),
                team: false,
                model: None,
            };
        let entries = [
            entry("a", 7, 1_000_000, "step old of run r0"),
            entry("a", 7, 2_000_000, "step ui of run r1"),
            entry("a", 7, 3_001_500, "step later of run r2"),
            entry("b", 7, 2_500_000, "step other of run r3"),
            entry("a", 8, 2_500_000, "step other of run r4"),
        ];
        let origin = |id: &str, task: u64, commit: u64| task_origin(&entries, id, task, commit);
        assert_eq!(origin("a", 7, 3_000).as_deref(), Some("step ui of run r1"));
        assert_eq!(
            origin("a", 7, 4_000).as_deref(),
            Some("step later of run r2")
        );
        assert_eq!(origin("a", 7, 500), None);
        assert_eq!(origin("c", 7, 4_000), None);
    }
}
