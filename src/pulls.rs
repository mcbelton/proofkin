//! Pull request steps: `pr @peer:folder` in a plan (see [`crate::plan`]). The agent that
//! holds a linked repository's clone opens the pull request from the branch the steps
//! before it committed to, with the commit's signed proof and the issues the work is for,
//! and follows it until it is merged or closed. Its owner allows this per folder
//! (`proofkin share pull-requests`): the branch is pushed and the pull request opened with
//! the owner's GitHub account, and only from a task branch whose signed record this agent
//! made for the peer that asks.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::process::Command;

use crate::evidence::Evidence;
use crate::github::{GithubCredentials, Repo};
use crate::tracker::Api;

/// How often an open pull request's state is asked of GitHub.
pub const POLL: Duration = Duration::from_secs(60);

/// What a pull request step asks the folder's agent for (see [`crate::llm::LlmTask`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestAsk {
    /// The task branch the work is on (`p2p/...`).
    pub branch: String,
    /// Its head commit, as the signed record of the step that made it has it: the pull
    /// request carries exactly that work.
    pub commit: String,
    /// The pull request's title.
    pub title: String,
}

/// A pull request a step opened, or found opened from its branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Opened {
    pub number: u64,
    pub url: String,
    /// `OPEN`, `MERGED` or `CLOSED`.
    pub state: String,
    /// It was there already: an earlier try of the step opened it.
    pub existed: bool,
    /// It carries the commit's signed proof.
    pub proof: bool,
}

/// Opens and follows the pull requests of peers' pull request steps. The host sets it
/// (see [`crate::agent::AgentConfig::pulls`]); tests use a fake.
#[async_trait]
pub trait PullRequests: Send + Sync {
    /// Opens a pull request from `ask.branch` of folder `share`'s repository into its
    /// branch, closing `issues` and the issues its commit is for, with the commit's proof
    /// and who asked for it (`by`). If one was opened from the branch as it is, that one.
    async fn open(
        &self,
        share: &str,
        ask: &PullRequestAsk,
        issues: &[String],
        by: &str,
    ) -> Result<Opened>;

    /// Where pull request `number` of folder `share`'s repository is: `OPEN`, `MERGED` or
    /// `CLOSED`.
    async fn state(&self, share: &str, number: u64) -> Result<String>;

    /// How long to wait before asking again about an open pull request.
    fn poll(&self) -> Duration {
        POLL
    }
}

/// Whether `branch` can be a task branch: `p2p/`, then letters, digits, `-`, `_`, `.` and
/// `/`, without `..` or a part starting with `.` or `-`.
pub fn task_branch_ok(branch: &str) -> bool {
    let Some(rest) = branch.strip_prefix("p2p/") else {
        return false;
    };
    branch.len() <= 200
        && !rest.is_empty()
        && rest.split('/').all(|part| {
            !part.is_empty()
                && !part.starts_with(['.', '-'])
                && !part.ends_with(".lock")
                && part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        })
        && !branch.contains("..")
}

/// Checks that `ask` is work this agent (`me`) did for `requester` in the clone at `repo`:
/// its branch is a task branch whose head is `ask.commit`, and the commit's signed record
/// (its git note) verifies, is this agent's, and was made for `requester`. A peer opens
/// pull requests only for work it asked for, as it was done.
pub async fn check_own_work(
    repo: &Path,
    ask: &PullRequestAsk,
    requester: &str,
    me: &str,
) -> Result<()> {
    if !task_branch_ok(&ask.branch) {
        bail!("{:?} isn't a branch a task made (p2p/...)", ask.branch);
    }
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--verify", "--quiet"])
        .arg(format!("refs/heads/{}^{{commit}}", ask.branch))
        .stdin(Stdio::null())
        .output()
        .await
        .context("running git")?;
    if !out.status.success() {
        bail!("there is no branch {} in this folder any more", ask.branch);
    }
    let head = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if head != ask.commit {
        bail!(
            "branch {} moved on from the step's commit {}: its pull request would carry other work",
            ask.branch,
            ask.commit.chars().take(12).collect::<String>()
        );
    }
    let note = crate::worktree::read_note(repo, &head)
        .await?
        .with_context(|| format!("the head commit of {} has no signed record", ask.branch))?;
    let record: Evidence =
        serde_json::from_str(&note).context("reading the commit's signed record")?;
    record
        .verify_record()
        .context("the commit's signed record doesn't verify")?;
    if record.signer_id != me {
        bail!("the commit's signed record isn't this agent's");
    }
    if record.git.as_ref().and_then(|g| g.commit.as_deref()) != Some(head.as_str()) {
        bail!("the commit's signed record is for another commit");
    }
    if record.requester_id != requester {
        bail!(
            "the work on {} was done for another peer: a pull request step opens only work you asked for",
            ask.branch
        );
    }
    Ok(())
}

/// The number of the pull request at `url` (`.../pull/12`).
pub fn number_of(url: &str) -> Option<u64> {
    url.trim_end_matches('/')
        .rsplit_once("/pull/")
        .and_then(|(_, n)| n.parse().ok())
}

/// The description of a pull request a step opens: the lines closing `issues` (those of
/// `slug` by number) and who asked for it.
pub fn description(issues: &[String], slug: &str, by: &str) -> String {
    let mut body = crate::tracker::closing_lines(issues, slug);
    if !body.is_empty() {
        body.push('\n');
    }
    body += &format!(
        "Opened by Proofkin for **{}**, as a pull request step of their plan.\n",
        by.replace(['`', '*', '_', '[', ']', '\r', '\n'], "")
    );
    body
}

/// [`PullRequests`] with a profile's linked repositories (a folder is the repository linked
/// under its name) and its GitHub account.
pub struct ProfilePulls {
    profile: crate::Profile,
}

impl ProfilePulls {
    pub fn new(profile: crate::Profile) -> Self {
        Self { profile }
    }

    fn linked(&self, share: &str) -> Result<(Repo, GithubCredentials)> {
        let profile = self.profile.reload()?;
        let repo = crate::github::RepoStore::load(&profile.repos_path())?
            .get(share)
            .with_context(|| format!("folder '{share}' isn't a linked GitHub repository"))?
            .clone();
        Ok((repo, profile.github()?))
    }
}

#[async_trait]
impl PullRequests for ProfilePulls {
    async fn open(
        &self,
        share: &str,
        ask: &PullRequestAsk,
        issues: &[String],
        by: &str,
    ) -> Result<Opened> {
        let (repo, creds) = self.linked(share)?;
        let api = crate::tracker::GhApi::new(creds.clone());
        if let Some(found) = opened_from(&api, &repo, ask).await? {
            return Ok(found);
        }
        let mut refs = crate::github::task_branches(&repo)
            .await?
            .into_iter()
            .find(|b| b.name == ask.branch)
            .map(|b| b.issues)
            .unwrap_or_default();
        for issue in issues {
            if !refs.contains(issue) {
                refs.push(issue.clone());
            }
        }
        let mut body = description(&refs, &repo.slug, by);
        creds.push(&repo, &ask.branch).await?;
        let profile = self.profile.reload()?;
        let proof = match creds.pr_proof(&repo, &ask.branch, &profile).await {
            Ok(Some(proof)) => {
                body += &proof.markdown;
                true
            }
            _ => false,
        };
        let url = creds
            .create_pr(&repo, &ask.branch, &ask.title, &body)
            .await?;
        let number = number_of(&url)
            .with_context(|| format!("the GitHub CLI answered {url:?}, not a pull request"))?;
        Ok(Opened {
            number,
            url,
            state: "OPEN".into(),
            existed: false,
            proof,
        })
    }

    async fn state(&self, share: &str, number: u64) -> Result<String> {
        let (repo, creds) = self.linked(share)?;
        let (owner, name) = repo
            .slug
            .split_once('/')
            .with_context(|| format!("{} isn't owner/name", repo.slug))?;
        let reply = crate::tracker::GhApi::new(creds)
            .graphql(
                "query PullRequestState($owner: String!, $name: String!, $number: Int!) { repository(owner: $owner, name: $name) { pullRequest(number: $number) { state } } }",
                json!({ "owner": owner, "name": name, "number": number }),
            )
            .await?;
        reply.data["repository"]["pullRequest"]["state"]
            .as_str()
            .map(str::to_owned)
            .with_context(|| {
                reply
                    .errors
                    .first()
                    .map(|e| e.message.clone())
                    .unwrap_or_else(|| {
                        format!("GitHub has no pull request #{number} in {}", repo.slug)
                    })
            })
    }
}

/// The newest pull request from `ask.branch` whose head is `ask.commit`, if there is one:
/// an earlier try of the step opened it.
async fn opened_from(api: &dyn Api, repo: &Repo, ask: &PullRequestAsk) -> Result<Option<Opened>> {
    let (owner, name) = repo
        .slug
        .split_once('/')
        .with_context(|| format!("{} isn't owner/name", repo.slug))?;
    let reply = api
        .graphql(
            "query OpenedFrom($owner: String!, $name: String!, $branch: String!) { repository(owner: $owner, name: $name) { pullRequests(headRefName: $branch, first: 10, orderBy: {field: CREATED_AT, direction: DESC}) { nodes { number state url headRefOid } } } }",
            json!({ "owner": owner, "name": name, "branch": ask.branch }),
        )
        .await?;
    let Some(nodes) = reply.data["repository"]["pullRequests"]["nodes"].as_array() else {
        bail!(
            "{}",
            reply
                .errors
                .first()
                .map(|e| e.message.clone())
                .unwrap_or_else(|| format!("GitHub has no repository {}", repo.slug))
        );
    };
    Ok(nodes
        .iter()
        .find(|pr| pr["headRefOid"].as_str() == Some(ask.commit.as_str()))
        .and_then(|pr| {
            Some(Opened {
                number: pr["number"].as_u64()?,
                url: pr["url"].as_str()?.to_owned(),
                state: pr["state"].as_str()?.to_owned(),
                existed: true,
                proof: false,
            })
        }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_task_branches_are_asked_for() {
        for ok in ["p2p/boss/12-1795411590", "p2p/a.b/c_d/9"] {
            assert!(task_branch_ok(ok), "{ok}");
        }
        for bad in [
            "main",
            "p2p/",
            "p2p/../main",
            "p2p/boss/../../x",
            "p2p/boss/-x",
            "p2p/boss/.hidden",
            "p2p/boss/x.lock",
            "p2p/boss x",
            "p2p//x",
            "p2p/boss/x;rm",
        ] {
            assert!(!task_branch_ok(bad), "{bad}");
        }
    }

    #[test]
    fn a_description_closes_the_issues_and_names_who_asked() {
        let body = description(
            &["acme/app#3".into(), "other/lib#9".into(), "bad".into()],
            "acme/app",
            "bo*ss_`x`",
        );
        assert_eq!(
            body,
            "Closes #3\nCloses other/lib#9\n\nOpened by Proofkin for **bossx**, as a pull request step of their plan.\n"
        );
        assert_eq!(
            description(&[], "acme/app", "boss"),
            "Opened by Proofkin for **boss**, as a pull request step of their plan.\n"
        );
        assert_eq!(number_of("https://github.com/acme/app/pull/12"), Some(12));
        assert_eq!(number_of("https://github.com/acme/app/pull/12/"), Some(12));
        assert_eq!(number_of("https://github.com/acme/app/issues/12"), None);
    }
}
