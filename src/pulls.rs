//! Pull request steps: `pr @peer:folder` in a plan (see [`crate::plan`]). The agent that
//! holds a linked repository's clone opens the pull request from the branch the steps
//! before it committed to, with the commit's signed proof, the plan's sign-offs before it
//! and the issues the work is for, asks its reviewers (those who signed off, or those the
//! step names) to review it, and follows it until it is merged or closed. Its owner
//! allows this per folder (`proofkin share pull-requests`): the branch is pushed and the
//! pull request opened with the owner's GitHub account, and only from a task branch whose
//! signed record this agent made for the peer that asks.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::process::Command;

use crate::evidence::{Evidence, EvidenceKind};
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
    /// The plan's sign-offs before it, as their approvers signed them: the pull request
    /// lists those that check out (see [`SignOff::checks_out`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub signoffs: Vec<SignOff>,
    /// The GitHub accounts asked to review it when it is opened (see
    /// [`crate::plan::Step::reviewers`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reviewers: Vec<String>,
}

/// A reviewer a pull request step asked for, and whether GitHub was asked to request its
/// review.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Review {
    pub login: String,
    /// Why its review wasn't requested (none: it was).
    pub not_asked: Option<String>,
}

impl Review {
    pub fn asked(login: &str) -> Self {
        Self {
            login: login.to_owned(),
            not_asked: None,
        }
    }

    pub fn not_asked(login: &str, why: impl Into<String>) -> Self {
        Self {
            login: login.to_owned(),
            not_asked: Some(why.into()),
        }
    }
}

/// The reviewers of `ask` that can be asked (GitHub logins, each once, at most
/// [`crate::plan::MAX_REVIEWERS`]), and the others with why they aren't.
pub fn reviewers_to_ask(ask: &PullRequestAsk) -> (Vec<String>, Vec<Review>) {
    let (mut ok, mut not): (Vec<String>, Vec<Review>) = (Vec::new(), Vec::new());
    for login in &ask.reviewers {
        if ok.iter().any(|l| l.eq_ignore_ascii_case(login)) {
            continue;
        }
        let clean: String = login.chars().filter(|c| !c.is_control()).take(40).collect();
        if !crate::github::login_ok(login) {
            not.push(Review::not_asked(&clean, "not a GitHub login"));
        } else if ok.len() == crate::plan::MAX_REVIEWERS {
            not.push(Review::not_asked(
                &clean,
                format!("more than {} reviewers", crate::plan::MAX_REVIEWERS),
            ));
        } else {
            ok.push(login.clone());
        }
    }
    (ok, not)
}

/// What a pull request step's output says about its reviewers: who was asked to review,
/// and who wasn't and why. Logins go without `@`, so quoting it mentions no one.
pub fn reviews_text(reviews: &[Review]) -> String {
    let asked: Vec<&str> = reviews
        .iter()
        .filter(|r| r.not_asked.is_none())
        .map(|r| r.login.as_str())
        .collect();
    let not: Vec<String> = reviews
        .iter()
        .filter_map(|r| {
            r.not_asked
                .as_ref()
                .map(|why| format!("{} ({why})", r.login))
        })
        .collect();
    let mut text = String::new();
    if !asked.is_empty() {
        text += &format!(" Asked to review: {}.", asked.join(", "));
    }
    if !not.is_empty() {
        text += &format!(" Not asked to review: {}.", not.join("; "));
    }
    text
}

/// A sign-off of the plan (an approval step it comes after), as its approver signed it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignOff {
    /// The plan step it is.
    pub step: String,
    /// What the approver answered (`Approved by carol: Ship it`): the words its record signs.
    pub output: String,
    pub evidence: Evidence,
}

impl SignOff {
    /// Whether it is an approval, signed by its approver's agent for exactly its words and
    /// asked for by `requester` (the peer that asks for the pull request).
    pub fn checks_out(&self, requester: &str) -> bool {
        crate::profile::name_ok(&self.step)
            && self.evidence.kind == EvidenceKind::Approval
            && self.evidence.approved == Some(true)
            && self.evidence.requester_id == requester
            && self.evidence.verify_self(&self.output).is_ok()
    }
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
    /// branch, closing `issues` and the issues its commit is for, with the commit's proof,
    /// who asked for it (`by`, as part of evidence run `run`) and `ask`'s sign-offs (checked
    /// already). If one was opened from the branch as it is, that one.
    async fn open(
        &self,
        share: &str,
        ask: &PullRequestAsk,
        issues: &[String],
        by: &str,
        run: Option<&str>,
    ) -> Result<Opened>;

    /// Where pull request `number` of folder `share`'s repository is: `OPEN`, `MERGED` or
    /// `CLOSED`.
    async fn state(&self, share: &str, number: u64) -> Result<String>;

    /// Asks GitHub accounts `logins` to review pull request `number` of folder `share`'s
    /// repository (one it just opened), one at a time, since GitHub refuses the whole
    /// request for one it can't ask. Its own account, the pull request's author, isn't
    /// asked.
    async fn request_reviews(&self, share: &str, number: u64, logins: &[String]) -> Vec<Review>;

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
/// `slug` by number), who asked for it (`by`), and the plan's sign-offs before it, with the
/// evidence run (`run`) that has their records.
pub fn description(
    issues: &[String],
    slug: &str,
    by: &str,
    run: Option<&str>,
    signoffs: &[SignOff],
) -> String {
    let clean = |text: &str| text.replace(['`', '*', '_', '[', ']', '\r', '\n'], "");
    let mut body = crate::tracker::closing_lines(issues, slug);
    if !body.is_empty() {
        body.push('\n');
    }
    let by = clean(by);
    body += &format!("Opened by Proofkin for **{by}**, as a pull request step of their plan.\n");
    if signoffs.is_empty() {
        return body;
    }
    body += "\n### Sign-offs\n\n";
    for s in signoffs {
        let e = &s.evidence;
        let key: String = e.signer_id.chars().take(10).collect();
        // The approver's words, as code: they mention no one and link nothing.
        let note = s
            .output
            .split_once(": ")
            .map(|(_, note)| crate::redact::redact(note).0)
            .map(|note| note.replace('`', "'").replace(['\r', '\n'], " "))
            .map(|note| note.trim().chars().take(300).collect::<String>())
            .filter(|note| !note.is_empty());
        body += &format!(
            "- `{}`: approved by **{}** (key `{key}`) on {} UTC{}\n",
            clean(&s.step),
            clean(&e.signer),
            crate::runs::utc(e.finished_unix),
            note.map(|n| format!(", noting `{n}`")).unwrap_or_default()
        );
    }
    body += &format!(
        "\n<sub>Each is a record its approver's agent signed (Ed25519), checked before it was added{}.</sub>\n",
        run.map(|r| format!(
            "; they are in {by}'s evidence run `{}`, which `proofkin runs verify` checks again",
            clean(r)
        ))
        .unwrap_or_default()
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
        run: Option<&str>,
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
        let mut body = description(&refs, &repo.slug, by, run, &ask.signoffs);
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

    async fn request_reviews(&self, share: &str, number: u64, logins: &[String]) -> Vec<Review> {
        let (repo, creds) = match self.linked(share) {
            Ok(linked) => linked,
            Err(e) => {
                let why = format!("{e:#}");
                return logins.iter().map(|l| Review::not_asked(l, &why)).collect();
            }
        };
        let mut reviews = Vec::new();
        for login in logins {
            reviews.push(if login.eq_ignore_ascii_case(&creds.user) {
                Review::not_asked(login, "the pull request's author")
            } else {
                match creds.request_reviewer(&repo, number, login).await {
                    Ok(()) => Review::asked(login),
                    Err(e) => Review::not_asked(login, format!("{e:#}")),
                }
            });
        }
        reviews
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
    fn reviewers_are_asked_once_each_and_the_output_says_who_was_not() {
        let ask = PullRequestAsk {
            branch: "p2p/boss/1".into(),
            commit: "c".into(),
            title: "t".into(),
            signoffs: Vec::new(),
            reviewers: ["carol", "Carol", "bad login", "-x", "dave"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        };
        let (ok, not) = reviewers_to_ask(&ask);
        assert_eq!(ok, ["carol", "dave"]);
        assert_eq!(
            not,
            [
                Review::not_asked("bad login", "not a GitHub login"),
                Review::not_asked("-x", "not a GitHub login")
            ]
        );
        let many = PullRequestAsk {
            reviewers: (0..12).map(|n| format!("r{n}")).collect(),
            ..ask
        };
        let (ok, not) = reviewers_to_ask(&many);
        assert_eq!((ok.len(), not.len()), (10, 2));
        assert_eq!(not[0], Review::not_asked("r10", "more than 10 reviewers"));

        assert_eq!(
            reviews_text(&[
                Review::asked("carol"),
                Review::not_asked("erin", "not a collaborator of acme/app"),
                Review::asked("dave"),
                Review::not_asked("boss", "the pull request's author"),
            ]),
            " Asked to review: carol, dave. Not asked to review: erin (not a collaborator of acme/app); boss (the pull request's author)."
        );
        assert_eq!(reviews_text(&[]), "");
    }

    #[test]
    fn a_description_closes_the_issues_and_names_who_asked() {
        let body = description(
            &["acme/app#3".into(), "other/lib#9".into(), "bad".into()],
            "acme/app",
            "bo*ss_`x`",
            None,
            &[],
        );
        assert_eq!(
            body,
            "Closes #3\nCloses other/lib#9\n\nOpened by Proofkin for **bossx**, as a pull request step of their plan.\n"
        );
        assert_eq!(
            description(&[], "acme/app", "boss", None, &[]),
            "Opened by Proofkin for **boss**, as a pull request step of their plan.\n"
        );
        assert_eq!(number_of("https://github.com/acme/app/pull/12"), Some(12));
        assert_eq!(number_of("https://github.com/acme/app/pull/12/"), Some(12));
        assert_eq!(number_of("https://github.com/acme/app/issues/12"), None);
    }

    #[test]
    fn the_sign_offs_that_check_out_go_on_the_pull_request() {
        let (carol, boss) = (iroh::SecretKey::generate(), iroh::SecretKey::generate());
        let answer = |approved: bool, output: &str| {
            let mut e = Evidence::new(
                EvidenceKind::Approval,
                "carol",
                carol.public(),
                boss.public(),
                7,
            );
            e.approved = Some(approved);
            e.finished_unix = 1_795_411_590;
            SignOff {
                step: "ok".into(),
                output: output.into(),
                evidence: e.sign(output, &carol),
            }
        };
        let good = answer(true, "Approved by carol: Ship it, @team see #12 `now`");
        let asker = boss.public().to_string();
        assert!(good.checks_out(&asker));
        // Asked for by another peer, rejected, or with other words than it signed: left out.
        assert!(!good.checks_out(&carol.public().to_string()));
        assert!(!answer(false, "Rejected by carol: not yet").checks_out(&asker));
        let forged = SignOff {
            output: "Approved by carol: anything".into(),
            ..good.clone()
        };
        assert!(!forged.checks_out(&asker));

        let body = description(
            &[],
            "acme/app",
            "boss",
            Some("20261123-064022-169d7"),
            &[good],
        );
        assert!(
            body.contains("\n### Sign-offs\n\n- `ok`: approved by **carol** (key `"),
            "{body}"
        );
        // Its words are code: they mention no one and link nothing.
        assert!(
            body.contains(" on 2026-11-23 05:26:30 UTC, noting `Ship it, @team see #12 'now'`\n"),
            "{body}"
        );
        assert!(
            body.contains("they are in boss's evidence run `20261123-064022-169d7`, which `proofkin runs verify` checks again"),
            "{body}"
        );
    }
}
