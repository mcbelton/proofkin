//! Tracked work: a profile's plan runs as GitHub issues, on a GitHub Project's board.
//!
//! A profile links a tracker (`proofkin tracker link`): the repository its issues go
//! to and, optionally, a GitHub Project. Then every plan run it starts is tracked
//! (unless its plan says `track: off`):
//!
//! - the run gets an issue (`track: #N` works on an existing one, `track: <title>`
//!   names a new one), and each step a sub-issue (`issue=#N` uses an existing issue).
//!   A step's issue is "blocked by" the issues of the steps it runs after, assigned to
//!   the profile's GitHub account, and on the board with the agent that does it (the
//!   board's `Agent` text field, added if it has none);
//! - the board's Status follows the run: In progress while a step runs, In review while
//!   its work waits for a pull request (or a person's sign-off), Done when it is
//!   closed. A step that changed no files is closed when it is done; a failed or
//!   skipped one goes back to Todo. Results, failures and the run's verdict are
//!   commented on the issues;
//! - each step's task carries its issue and those of the steps whose work it applies
//!   ([`crate::plan::RunContext::issues`]): the worker's commit references them
//!   (`Refs: owner/repo#N`) and a pull request from its branch closes them, so the
//!   issues show the pull request. A run issue whose work waits for pull requests is
//!   closed by [`Tracker::settle`] once all its sub-issues are.
//!
//! Everything goes through GitHub's GraphQL API with the GitHub CLI (`gh api graphql`,
//! the request on stdin and the profile's token in `GH_TOKEN`). Issues need the
//! account's `repo` scope and the board its `project` scope: without it, tracking keeps
//! to the issues and says how to grant it. Tracking never fails a plan or holds up its
//! steps: what goes wrong is reported with the run.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::evidence::Evidence;
use crate::github::GithubCredentials;
use crate::plan::{Plan, Step, StepState, Target, Track};
use crate::profile::{INHERITED_CREDENTIAL_VARS, Profile};
use crate::runs::RunRecord;

/// The board's text field that says which agent does an item.
pub const AGENT_FIELD: &str = "Agent";
/// The longest step text put in its issue, and result in a comment.
const MAX_TEXT: usize = 6_000;
const MAX_RESULT: usize = 2_000;
/// Mutations sent in one request.
const BATCH: usize = 20;
/// The longest one GitHub call may take.
const CALL_TIMEOUT: Duration = Duration::from_secs(60);
/// The longest the end of a run waits for its tracking to catch up.
const FINISH_TIMEOUT: Duration = Duration::from_secs(180);
/// The most problems a run keeps.
const MAX_PROBLEMS: usize = 50;

/// A profile's tracker: where its plan runs are tracked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackerConfig {
    /// `owner/repo` the issues go to.
    pub repo: String,
    pub host: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<ProjectRef>,
    /// Run issues left open while their work waits for pull requests; closed by
    /// [`Tracker::settle`] once all their sub-issues are.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub open_runs: Vec<u64>,
}

/// A GitHub Project (Projects v2) by its owner and number.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectRef {
    /// The user or organization that owns it.
    pub owner: String,
    pub number: u64,
}

impl TrackerConfig {
    pub fn load(path: &Path) -> Result<Option<Self>> {
        if !path.is_file() {
            return Ok(None);
        }
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_str(&text)
            .map(Some)
            .with_context(|| format!("parsing {}", path.display()))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, serde_json::to_vec_pretty(self)?)
            .with_context(|| format!("writing {}", path.display()))
    }

    /// `owner/repo#n`.
    pub fn issue_ref(&self, n: u64) -> String {
        format!("{}#{n}", self.repo)
    }
}

/// `(owner, number)` of a GitHub Project from its URL
/// (`https://github.com/users/alice/projects/4`, `.../orgs/acme/projects/7`) or
/// `owner/number`.
pub fn parse_project(input: &str, host: &str) -> Result<ProjectRef> {
    let trimmed = input
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/');
    let trimmed = trimmed
        .strip_prefix(host)
        .map(|rest| rest.trim_start_matches('/'))
        .unwrap_or(trimmed);
    let parts: Vec<&str> = trimmed.split('/').filter(|p| !p.is_empty()).collect();
    let (owner, number) = match parts.as_slice() {
        [kind, owner, "projects", number, ..] if matches!(*kind, "users" | "orgs") => {
            (*owner, *number)
        }
        [owner, number] => (*owner, *number),
        _ => bail!(
            "expected a project's URL, like https://{host}/users/<owner>/projects/<number>, or <owner>/<number>; got {input:?}"
        ),
    };
    let number: u64 = number
        .parse()
        .ok()
        .filter(|n| *n > 0)
        .with_context(|| format!("{number:?} is not a project number"))?;
    if owner.is_empty()
        || !owner
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    {
        bail!("{owner:?} is not a GitHub user or organization");
    }
    Ok(ProjectRef {
        owner: owner.to_owned(),
        number,
    })
}

/// Whether `r` is an issue reference a task may carry: `owner/repo#N`. Workers check
/// what peers send before it goes in a commit message.
pub fn valid_ref(r: &str) -> bool {
    let Some((repo, n)) = r.split_once('#') else {
        return false;
    };
    let ok = |s: &str| {
        !s.is_empty()
            && s.len() <= 100
            && !s.starts_with('.')
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    matches!(repo.split_once('/'), Some((owner, name)) if ok(owner) && ok(name))
        && n.len() <= 12
        && n.parse::<u64>().is_ok_and(|n| n > 0)
}

/// The lines a pull request into `slug` closes issues `refs` with: `Closes #N` for its
/// own repository's, `Closes owner/repo#N` for others'.
pub fn closing_lines(refs: &[String], slug: &str) -> String {
    refs.iter()
        .filter(|r| valid_ref(r))
        .map(|r| match r.split_once('#') {
            Some((repo, n)) if repo.eq_ignore_ascii_case(slug) => format!("Closes #{n}\n"),
            _ => format!("Closes {r}\n"),
        })
        .collect()
}

/// GitHub's GraphQL API (the GitHub CLI; a fake in tests).
#[async_trait]
pub trait Api: Send + Sync {
    /// The response's `data` and `errors` (GraphQL answers what it can: both may come).
    async fn graphql(&self, query: &str, variables: Value) -> Result<Reply>;
}

#[derive(Debug, Clone, Default)]
pub struct Reply {
    pub data: Value,
    pub errors: Vec<GqlError>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct GqlError {
    #[serde(default)]
    pub message: String,
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    #[serde(default)]
    pub path: Vec<Value>,
}

impl Reply {
    pub fn from_value(v: Value) -> Reply {
        let mut errors: Vec<GqlError> = v
            .get("errors")
            .cloned()
            .and_then(|e| serde_json::from_value(e).ok())
            .unwrap_or_default();
        // An HTTP error (bad credentials, rate limit) has a message instead.
        if errors.is_empty()
            && v.get("data").is_none()
            && let Some(message) = v.get("message").and_then(Value::as_str)
        {
            errors.push(GqlError {
                message: message.to_owned(),
                ..GqlError::default()
            });
        }
        Reply {
            data: v.get("data").cloned().unwrap_or(Value::Null),
            errors,
        }
    }

    fn ok(self) -> Result<Value> {
        match self.errors.first() {
            Some(e) => Err(anyhow!("{}", e.message)),
            None => Ok(self.data),
        }
    }

    /// The first error of the field with alias `alias`, else the first error.
    fn error_for(&self, alias: &str) -> String {
        self.errors
            .iter()
            .find(|e| e.path.iter().any(|p| p.as_str() == Some(alias)))
            .or(self.errors.first())
            .map(|e| e.message.clone())
            .unwrap_or_else(|| "GitHub didn't say why".to_owned())
    }

    fn lacks_scope(&self) -> bool {
        self.errors.iter().any(|e| {
            e.kind.as_deref() == Some("INSUFFICIENT_SCOPES") || e.message.contains("scope")
        })
    }
}

/// [`Api`] through the GitHub CLI with an account's token.
pub struct GhApi {
    creds: GithubCredentials,
}

impl GhApi {
    pub fn new(creds: GithubCredentials) -> Self {
        Self { creds }
    }
}

#[async_trait]
impl Api for GhApi {
    async fn graphql(&self, query: &str, variables: Value) -> Result<Reply> {
        let body = serde_json::to_vec(&json!({ "query": query, "variables": variables }))?;
        let mut cmd = Command::new(crate::github::gh_program());
        cmd.args([
            "api",
            "graphql",
            "--hostname",
            &self.creds.host,
            "--input",
            "-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
        for var in INHERITED_CREDENTIAL_VARS {
            cmd.env_remove(var);
        }
        cmd.env("GH_TOKEN", &self.creds.token)
            .env("GH_HOST", &self.creds.host)
            .env("GH_PROMPT_DISABLED", "1");
        let mut child = cmd.spawn().context("running the GitHub CLI (gh)")?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(&body)
                .await
                .context("sending the request to the GitHub CLI")?;
        }
        let out = tokio::time::timeout(CALL_TIMEOUT, child.wait_with_output())
            .await
            .map_err(|_| anyhow!("GitHub didn't answer within {}s", CALL_TIMEOUT.as_secs()))?
            .context("running the GitHub CLI (gh)")?;
        let reply: Value = serde_json::from_slice(&out.stdout).map_err(|_| {
            anyhow!(
                "the GitHub CLI failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )
        })?;
        Ok(Reply::from_value(reply))
    }
}

/// Where an item is on the board.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Status {
    Todo,
    InProgress,
    InReview,
    Done,
}

impl Status {
    /// Option names boards give it (lowercase letters and digits only), most usual
    /// first.
    fn names(self) -> &'static [&'static str] {
        match self {
            Status::Todo => &["todo", "ready", "backlog", "new", "open", "planned"],
            Status::InProgress => &["inprogress", "doing", "started", "active", "wip", "working"],
            Status::InReview => &[
                "inreview",
                "review",
                "reviewing",
                "readyforreview",
                "waitingforreview",
                "toreview",
            ],
            Status::Done => &[
                "done",
                "complete",
                "completed",
                "closed",
                "shipped",
                "finished",
            ],
        }
    }
}

/// `name` in lowercase letters and digits ("👀 In review" -> "inreview").
fn norm(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

#[derive(Debug, Clone)]
struct SelectField {
    id: String,
    name: String,
    options: Vec<(String, String)>,
}

impl SelectField {
    /// The option for `status` (In review falls back to In progress).
    fn option(&self, status: Status) -> Option<&str> {
        let find = |s: Status| {
            s.names().iter().find_map(|wanted| {
                self.options
                    .iter()
                    .find(|(_, name)| norm(name) == *wanted)
                    .map(|(id, _)| id.as_str())
            })
        };
        find(status).or_else(|| {
            (status == Status::InReview)
                .then(|| find(Status::InProgress))
                .flatten()
        })
    }

    fn named(&self, name: &str) -> Option<&str> {
        self.options
            .iter()
            .find(|(_, n)| n == name)
            .map(|(id, _)| id.as_str())
    }
}

/// The GitHub Project the tracker puts items on.
#[derive(Debug, Clone)]
pub struct Board {
    pub id: String,
    pub title: String,
    pub url: String,
    status: Option<SelectField>,
    agent: Option<String>,
}

/// An issue as tracking needs it.
#[derive(Debug, Clone, Default)]
struct Issue {
    id: String,
    number: u64,
    url: String,
    open: bool,
    body: String,
    parent: Option<u64>,
    /// Its item on the board, and the item's status.
    item: Option<String>,
    status: Option<String>,
    subs: Vec<Issue>,
    /// Closed and all sub-issues.
    summary: (u64, u64),
}

/// An issue's number and URL.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueRef {
    pub number: u64,
    pub url: String,
}

/// One mutation of a batch: `field(input: $m) { select }`.
struct Op {
    field: &'static str,
    input_type: &'static str,
    input: Value,
    select: &'static str,
}

fn op(field: &'static str, input_type: &'static str, input: Value) -> Op {
    Op {
        field,
        input_type,
        input,
        select: "clientMutationId",
    }
}

/// A tracker connected to GitHub with an account.
pub struct Tracker {
    api: Arc<dyn Api>,
    pub config: TrackerConfig,
    /// The account's login (issues are assigned to it).
    pub login: String,
    viewer: String,
    repo_id: String,
    pub repo_url: String,
    pub board: Option<Board>,
    /// Why the board isn't used, e.g. the account lacks the `project` scope.
    pub board_problem: Option<String>,
}

const REPO_QUERY: &str = "query Repo($owner: String!, $name: String!) { viewer { id login } repository(owner: $owner, name: $name) { id url } }";

const BOARD_QUERY: &str = "query Board($login: String!, $number: Int!) { repositoryOwner(login: $login) { ... on ProjectV2Owner { projectV2(number: $number) { id title url fields(first: 50) { nodes { ... on ProjectV2FieldCommon { id name dataType } ... on ProjectV2SingleSelectField { options { id name } } } } } } } }";

const AGENT_FIELD_MUTATION: &str = "mutation AgentField($input: CreateProjectV2FieldInput!) { createProjectV2Field(input: $input) { projectV2Field { ... on ProjectV2Field { id } } } }";

impl Tracker {
    /// Connects `config` with `api`: checks the repository and opens the board.
    pub async fn connect(api: Arc<dyn Api>, config: TrackerConfig) -> Result<Tracker> {
        let (owner, name) = config
            .repo
            .split_once('/')
            .with_context(|| format!("{:?} is not owner/repo", config.repo))?;
        let data = api
            .graphql(REPO_QUERY, json!({ "owner": owner, "name": name }))
            .await?
            .ok()
            .with_context(|| format!("reading {} on GitHub", config.repo))?;
        let repo = data
            .get("repository")
            .filter(|r| !r.is_null())
            .with_context(|| {
                format!(
                    "{} isn't on {}, or the account can't see it",
                    config.repo, config.host
                )
            })?;
        let text = |v: &Value, key: &str| {
            v.get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        let viewer = data.get("viewer").cloned().unwrap_or_default();
        let mut tracker = Tracker {
            login: text(&viewer, "login"),
            viewer: text(&viewer, "id"),
            repo_id: text(repo, "id"),
            repo_url: text(repo, "url"),
            api,
            config,
            board: None,
            board_problem: None,
        };
        if let Some(project) = tracker.config.project.clone() {
            match tracker.open_board(&project).await {
                Ok(board) => tracker.board = Some(board),
                Err(e) => tracker.board_problem = Some(format!("{e:#}")),
            }
        }
        Ok(tracker)
    }

    async fn open_board(&self, project: &ProjectRef) -> Result<Board> {
        let reply = self
            .api
            .graphql(
                BOARD_QUERY,
                json!({ "login": project.owner, "number": project.number }),
            )
            .await?;
        if reply.lacks_scope() {
            bail!(
                "the GitHub account {} can't use projects yet: grant its token the project scope with \
                 `gh auth refresh --hostname {} --scopes project` while {} is the GitHub CLI's active account",
                self.login,
                self.config.host,
                self.login
            );
        }
        let data = reply.ok()?;
        let board = data
            .pointer("/repositoryOwner/projectV2")
            .filter(|p| !p.is_null())
            .with_context(|| {
                format!(
                    "there is no project {} of {}, or the account can't see it",
                    project.number, project.owner
                )
            })?;
        let text = |v: &Value, key: &str| {
            v.get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        let fields = board
            .pointer("/fields/nodes")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let status = fields
            .iter()
            .find(|f| {
                text(f, "dataType") == "SINGLE_SELECT"
                    && text(f, "name").eq_ignore_ascii_case("status")
            })
            .map(|f| SelectField {
                id: text(f, "id"),
                name: text(f, "name"),
                options: f
                    .get("options")
                    .and_then(Value::as_array)
                    .map(|o| o.iter().map(|o| (text(o, "id"), text(o, "name"))).collect())
                    .unwrap_or_default(),
            });
        let mut board = Board {
            id: text(board, "id"),
            title: text(board, "title"),
            url: text(board, "url"),
            status,
            agent: fields
                .iter()
                .find(|f| text(f, "dataType") == "TEXT" && text(f, "name") == AGENT_FIELD)
                .map(|f| text(f, "id")),
        };
        if board.agent.is_none() {
            // Best effort: without it, items just don't say which agent does them.
            let created = self
                .api
                .graphql(
                    AGENT_FIELD_MUTATION,
                    json!({ "input": { "projectId": board.id, "dataType": "TEXT", "name": AGENT_FIELD } }),
                )
                .await
                .ok()
                .and_then(|r| r.ok().ok());
            board.agent = created
                .and_then(|d| {
                    d.pointer("/createProjectV2Field/projectV2Field/id")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .filter(|id| !id.is_empty());
        }
        Ok(board)
    }

    /// What the tracker is and whether its board works, for `tracker status` and
    /// the web UI.
    pub fn info(&self) -> TrackerInfo {
        TrackerInfo {
            repo: self.config.repo.clone(),
            host: self.config.host.clone(),
            repo_url: self.repo_url.clone(),
            login: self.login.clone(),
            project: self.config.project.clone(),
            board: self.board.as_ref().map(|b| BoardInfo {
                title: b.title.clone(),
                url: b.url.clone(),
                statuses: b
                    .status
                    .as_ref()
                    .map(|s| s.options.iter().map(|(_, n)| n.clone()).collect())
                    .unwrap_or_default(),
                agent_field: b.agent.is_some(),
            }),
            board_problem: self.board_problem.clone(),
            open_runs: self.config.open_runs.clone(),
        }
    }

    fn issue_fields(&self) -> String {
        let mut fields =
            "id number url state body parent { number } subIssuesSummary { total completed } \
                          subIssues(first: 100) { nodes { id number url state body } }"
                .to_owned();
        if let Some(board) = &self.board {
            let status = board
                .status
                .as_ref()
                .map(|s| s.name.as_str())
                .unwrap_or("Status");
            fields += &format!(
                " projectItems(first: 20) {{ nodes {{ id project {{ id }} fieldValueByName(name: {}) {{ ... on ProjectV2ItemFieldSingleSelectValue {{ name }} }} }} }}",
                Value::String(status.to_owned())
            );
        }
        fields
    }

    fn parse_issue(&self, v: &Value) -> Option<Issue> {
        let text = |v: &Value, key: &str| {
            v.get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        let number = v.get("number")?.as_u64()?;
        let item = self.board.as_ref().and_then(|board| {
            v.pointer("/projectItems/nodes")
                .and_then(Value::as_array)?
                .iter()
                .find(|i| i.pointer("/project/id").and_then(Value::as_str) == Some(&board.id))
                .cloned()
        });
        Some(Issue {
            id: text(v, "id"),
            number,
            url: text(v, "url"),
            open: text(v, "state") == "OPEN",
            body: text(v, "body"),
            parent: v.pointer("/parent/number").and_then(Value::as_u64),
            status: item.as_ref().and_then(|i| {
                i.pointer("/fieldValueByName/name")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            }),
            item: item.as_ref().map(|i| text(i, "id")),
            subs: v
                .pointer("/subIssues/nodes")
                .and_then(Value::as_array)
                .map(|subs| subs.iter().filter_map(|s| self.parse_issue(s)).collect())
                .unwrap_or_default(),
            summary: (
                v.pointer("/subIssuesSummary/completed")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                v.pointer("/subIssuesSummary/total")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
            ),
        })
    }

    /// The tracker repository's issues `numbers`; fails if one doesn't exist.
    async fn issues(&self, numbers: &[u64]) -> Result<HashMap<u64, Issue>> {
        let mut found = HashMap::new();
        if numbers.is_empty() {
            return Ok(found);
        }
        let (owner, name) = self.config.repo.split_once('/').unwrap_or_default();
        let fields = self.issue_fields();
        let wanted: std::collections::BTreeSet<u64> = numbers.iter().copied().collect();
        let query = format!(
            "query Issues($owner: String!, $name: String!) {{ repository(owner: $owner, name: $name) {{ {} }} }}",
            wanted
                .iter()
                .map(|n| format!("n{n}: issue(number: {n}) {{ {fields} }}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        let reply = self
            .api
            .graphql(&query, json!({ "owner": owner, "name": name }))
            .await?;
        for n in &wanted {
            match reply
                .data
                .pointer(&format!("/repository/n{n}"))
                .and_then(|v| self.parse_issue(v))
            {
                Some(issue) => {
                    found.insert(*n, issue);
                }
                None => bail!(
                    "there is no issue #{n} in {} ({})",
                    self.config.repo,
                    reply.error_for(&format!("n{n}"))
                ),
            }
        }
        Ok(found)
    }

    /// Runs `ops` (in order, `BATCH` to a request); each one's result by position.
    async fn run_ops(&self, name: &str, ops: Vec<Op>) -> Vec<Result<Value>> {
        let mut results = Vec::with_capacity(ops.len());
        for chunk in ops.chunks(BATCH) {
            let mut vars = serde_json::Map::new();
            let (mut decl, mut body) = (Vec::new(), Vec::new());
            for (i, op) in chunk.iter().enumerate() {
                decl.push(format!("$m{i}: {}!", op.input_type));
                body.push(format!(
                    "m{i}: {}(input: $m{i}) {{ {} }}",
                    op.field, op.select
                ));
                vars.insert(format!("m{i}"), op.input.clone());
            }
            let query = format!(
                "mutation {name}({}) {{ {} }}",
                decl.join(", "),
                body.join(" ")
            );
            match self.api.graphql(&query, Value::Object(vars)).await {
                Ok(reply) => {
                    for i in 0..chunk.len() {
                        let alias = format!("m{i}");
                        results.push(match reply.data.get(&alias) {
                            Some(v) if !v.is_null() => Ok(v.clone()),
                            _ => Err(anyhow!("{}", reply.error_for(&alias))),
                        });
                    }
                }
                Err(e) => {
                    let message = format!("{e:#}");
                    results.extend(chunk.iter().map(|_| Err(anyhow!("{message}"))));
                }
            }
        }
        results
    }

    /// Creates issues `(title, body, parent id)`, assigned to the account.
    async fn create(&self, new: &[(String, String, Option<String>)]) -> Vec<Result<Issue>> {
        let ops = |assign: bool| {
            new.iter()
                .map(|(title, body, parent)| {
                    let mut input =
                        json!({ "repositoryId": self.repo_id, "title": title, "body": body });
                    if assign {
                        input["assigneeIds"] = json!([self.viewer]);
                    }
                    if let Some(parent) = parent {
                        input["parentIssueId"] = json!(parent);
                    }
                    Op {
                        field: "createIssue",
                        input_type: "CreateIssueInput",
                        input,
                        select: "issue { id number url }",
                    }
                })
                .collect::<Vec<_>>()
        };
        let mut results = self.run_ops("CreateIssues", ops(true)).await;
        // Someone who can't be assigned in the repository still gets the issues.
        if !results.is_empty() && results.iter().all(|r| r.is_err()) {
            results = self.run_ops("CreateIssues", ops(false)).await;
        }
        results
            .into_iter()
            .zip(new)
            .map(|(r, (_, body, _))| {
                let v = r?;
                let issue = v.get("issue").context("GitHub didn't return the issue")?;
                Ok(Issue {
                    id: issue
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    number: issue.get("number").and_then(Value::as_u64).unwrap_or(0),
                    url: issue
                        .get("url")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    open: true,
                    body: body.clone(),
                    ..Issue::default()
                })
            })
            .collect()
    }

    fn status_op(&self, item: &str, status: Status) -> Option<Op> {
        let board = self.board.as_ref()?;
        let field = board.status.as_ref()?;
        let option = field.option(status)?;
        Some(Self::select_op(board, field, item, option))
    }

    fn status_named_op(&self, item: &str, name: &str) -> Option<Op> {
        let board = self.board.as_ref()?;
        let field = board.status.as_ref()?;
        Some(Self::select_op(board, field, item, field.named(name)?))
    }

    fn select_op(board: &Board, field: &SelectField, item: &str, option: &str) -> Op {
        op(
            "updateProjectV2ItemFieldValue",
            "UpdateProjectV2ItemFieldValueInput",
            json!({ "projectId": board.id, "itemId": item, "fieldId": field.id, "value": { "singleSelectOptionId": option } }),
        )
    }

    fn agent_op(&self, item: &str, agent: &str) -> Option<Op> {
        let board = self.board.as_ref()?;
        Some(op(
            "updateProjectV2ItemFieldValue",
            "UpdateProjectV2ItemFieldValueInput",
            json!({ "projectId": board.id, "itemId": item, "fieldId": board.agent.as_ref()?, "value": { "text": agent } }),
        ))
    }

    /// Puts issues (by node id) on the board; their item ids, in order.
    async fn board_add(&self, issues: &[String]) -> Vec<Result<String>> {
        let Some(board) = &self.board else {
            return issues
                .iter()
                .map(|_| Err(anyhow!("there is no board")))
                .collect();
        };
        let ops = issues
            .iter()
            .map(|id| Op {
                field: "addProjectV2ItemById",
                input_type: "AddProjectV2ItemByIdInput",
                input: json!({ "projectId": board.id, "contentId": id }),
                select: "item { id }",
            })
            .collect();
        self.run_ops("BoardAdd", ops)
            .await
            .into_iter()
            .map(|r| {
                r?.pointer("/item/id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .context("GitHub didn't return the board item")
            })
            .collect()
    }

    /// Closes the run issues left open whose sub-issues are all closed now (see
    /// [`TrackerConfig::open_runs`]); returns those it closed. Keeps the rest in
    /// `config.open_runs` (the caller saves it).
    pub async fn settle(&mut self) -> Result<Vec<u64>> {
        let numbers = self.config.open_runs.clone();
        if numbers.is_empty() {
            return Ok(Vec::new());
        }
        let issues = match self.issues(&numbers).await {
            Ok(issues) => issues,
            // One of them is gone: check them one by one.
            Err(_) => {
                let mut found = HashMap::new();
                for n in &numbers {
                    if let Ok(mut one) = self.issues(&[*n]).await {
                        found.extend(one.drain());
                    }
                }
                found
            }
        };
        let (mut ops, mut closed, mut open) = (Vec::new(), Vec::new(), Vec::new());
        for n in numbers {
            let Some(issue) = issues.get(&n) else {
                continue;
            };
            if !issue.open {
                continue;
            }
            let (completed, total) = issue.summary;
            if total > 0 && completed == total {
                ops.push(op(
                    "addComment",
                    "AddCommentInput",
                    json!({ "subjectId": issue.id, "body": "All of this run's sub-issues are closed, so it is done." }),
                ));
                ops.push(op(
                    "closeIssue",
                    "CloseIssueInput",
                    json!({ "issueId": issue.id, "stateReason": "COMPLETED" }),
                ));
                if let Some(o) = issue
                    .item
                    .as_deref()
                    .and_then(|item| self.status_op(item, Status::Done))
                {
                    ops.push(o);
                }
                closed.push(n);
            } else {
                open.push(n);
            }
        }
        for r in self.run_ops("Settle", ops).await {
            r?;
        }
        self.config.open_runs = open;
        Ok(closed)
    }

    /// The board's items (or, without a board, the repository's open issues).
    pub async fn items(&self) -> Result<Vec<Item>> {
        let Some(board) = &self.board else {
            let (owner, name) = self.config.repo.split_once('/').unwrap_or_default();
            let query = format!(
                "query OpenIssues($owner: String!, $name: String!) {{ repository(owner: $owner, name: $name) {{ issues(first: 100, states: OPEN, orderBy: {{ field: CREATED_AT, direction: ASC }}) {{ nodes {{ {ITEM_ISSUE} }} }} }} }}"
            );
            let data = self
                .api
                .graphql(&query, json!({ "owner": owner, "name": name }))
                .await?
                .ok()?;
            return Ok(data
                .pointer("/repository/issues/nodes")
                .and_then(Value::as_array)
                .map(|nodes| {
                    nodes
                        .iter()
                        .map(|n| self.item(n, None, None, "Issue"))
                        .collect()
                })
                .unwrap_or_default());
        };
        let status = board
            .status
            .as_ref()
            .map(|s| s.name.clone())
            .unwrap_or_else(|| "Status".into());
        let query = format!(
            "query Items($id: ID!, $after: String) {{ node(id: $id) {{ ... on ProjectV2 {{ items(first: 100, after: $after) {{ pageInfo {{ hasNextPage endCursor }} nodes {{ isArchived \
             status: fieldValueByName(name: {}) {{ ... on ProjectV2ItemFieldSingleSelectValue {{ name }} }} \
             agent: fieldValueByName(name: {}) {{ ... on ProjectV2ItemFieldTextValue {{ text }} }} \
             content {{ __typename ... on Issue {{ {ITEM_ISSUE} }} ... on PullRequest {{ number title url state repository {{ nameWithOwner }} assignees(first: 5) {{ nodes {{ login }} }} }} ... on DraftIssue {{ title }} }} }} }} }} }} }}",
            Value::String(status),
            Value::String(AGENT_FIELD.into())
        );
        let mut items = Vec::new();
        let mut after = Value::Null;
        for _ in 0..10 {
            let data = self
                .api
                .graphql(&query, json!({ "id": board.id, "after": after }))
                .await?
                .ok()?;
            let page = data.pointer("/node/items").cloned().unwrap_or_default();
            for node in page
                .get("nodes")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if node.get("isArchived").and_then(Value::as_bool) == Some(true) {
                    continue;
                }
                let content = node.get("content").cloned().unwrap_or_default();
                let kind = content
                    .get("__typename")
                    .and_then(Value::as_str)
                    .unwrap_or("DraftIssue")
                    .to_owned();
                items.push(self.item(
                    &content,
                    node.pointer("/status/name").and_then(Value::as_str),
                    node.pointer("/agent/text").and_then(Value::as_str),
                    &kind,
                ));
            }
            match page
                .pointer("/pageInfo/hasNextPage")
                .and_then(Value::as_bool)
            {
                Some(true) => {
                    after = page
                        .pointer("/pageInfo/endCursor")
                        .cloned()
                        .unwrap_or_default()
                }
                _ => break,
            }
        }
        Ok(items)
    }

    fn item(&self, v: &Value, status: Option<&str>, agent: Option<&str>, kind: &str) -> Item {
        let repo = v
            .pointer("/repository/nameWithOwner")
            .and_then(Value::as_str)
            .filter(|r| !r.eq_ignore_ascii_case(&self.config.repo))
            .map(str::to_owned);
        Item {
            kind: match kind {
                "Issue" => "issue",
                "PullRequest" => "pull_request",
                _ => "draft",
            },
            number: v.get("number").and_then(Value::as_u64),
            title: v
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            url: v.get("url").and_then(Value::as_str).map(str::to_owned),
            open: !matches!(
                v.get("state").and_then(Value::as_str),
                Some("CLOSED" | "MERGED")
            ),
            repo,
            status: status.map(str::to_owned),
            agent: agent.filter(|a| !a.is_empty()).map(str::to_owned),
            assignees: v
                .pointer("/assignees/nodes")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|n| n.get("login").and_then(Value::as_str))
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
            parent: v.pointer("/parent/number").and_then(Value::as_u64),
            sub_issues: v
                .pointer("/subIssuesSummary/total")
                .and_then(Value::as_u64)
                .filter(|t| *t > 0)
                .map(|total| {
                    (
                        v.pointer("/subIssuesSummary/completed")
                            .and_then(Value::as_u64)
                            .unwrap_or(0),
                        total,
                    )
                }),
            pull_requests: v
                .pointer("/closedByPullRequestsReferences/nodes")
                .and_then(Value::as_array)
                .map(|p| {
                    p.iter()
                        .filter_map(|n| n.get("number").and_then(Value::as_u64))
                        .collect()
                })
                .unwrap_or_default(),
        }
    }
}

const ITEM_ISSUE: &str = "number title url state repository { nameWithOwner } assignees(first: 5) { nodes { login } } parent { number } subIssuesSummary { total completed } closedByPullRequestsReferences(first: 5) { nodes { number } }";

/// A tracker's settings and whether its board works.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TrackerInfo {
    pub repo: String,
    pub host: String,
    pub repo_url: String,
    pub login: String,
    pub project: Option<ProjectRef>,
    pub board: Option<BoardInfo>,
    pub board_problem: Option<String>,
    pub open_runs: Vec<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BoardInfo {
    pub title: String,
    pub url: String,
    /// The Status field's options.
    pub statuses: Vec<String>,
    /// Whether it has the `Agent` field.
    pub agent_field: bool,
}

/// An item on the board (or an open issue, without a board).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Item {
    /// `issue`, `pull_request` or `draft`.
    pub kind: &'static str,
    pub number: Option<u64>,
    pub title: String,
    pub url: Option<String>,
    pub open: bool,
    /// Its repository, when it isn't the tracker's.
    pub repo: Option<String>,
    pub status: Option<String>,
    pub agent: Option<String>,
    pub assignees: Vec<String>,
    pub parent: Option<u64>,
    /// Closed and all of its sub-issues.
    pub sub_issues: Option<(u64, u64)>,
    /// Pull requests that close it.
    pub pull_requests: Vec<u64>,
}

/// Links `profile`'s tracker: issues in `repo` (`owner/repo` or a URL) and, with
/// `project`, items on that GitHub Project. Checks both with the profile's GitHub
/// account and saves it; a board that doesn't work is in the returned tracker's
/// `board_problem`.
pub async fn link(profile: &Profile, repo: &str, project: Option<&str>) -> Result<Tracker> {
    let creds = profile.github()?;
    let (host, slug) = crate::github::parse_repo(repo, &creds.host)?;
    if host != creds.host {
        bail!(
            "{slug} is on {host}, but profile '{}' is signed in to {}",
            profile.name,
            creds.host
        );
    }
    let project = project
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(|p| parse_project(p, &host))
        .transpose()?;
    let mut config = TrackerConfig {
        repo: slug,
        host,
        project,
        open_runs: Vec::new(),
    };
    if let Some(old) = TrackerConfig::load(&profile.tracker_path())?
        && old.repo == config.repo
    {
        config.open_runs = old.open_runs;
    }
    let tracker = Tracker::connect(Arc::new(GhApi::new(creds)), config).await?;
    tracker.config.save(&profile.tracker_path())?;
    Ok(tracker)
}

/// `profile`'s tracker, connected with its GitHub account; None if it has none.
pub async fn open(profile: &Profile) -> Result<Option<Tracker>> {
    let Some(config) = TrackerConfig::load(&profile.tracker_path())? else {
        return Ok(None);
    };
    let creds = profile.github()?;
    if creds.host != config.host {
        bail!(
            "the tracker is on {}, but profile '{}' is signed in to {}",
            config.host,
            profile.name,
            creds.host
        );
    }
    Ok(Some(
        Tracker::connect(Arc::new(GhApi::new(creds)), config).await?,
    ))
}

/// Closes `profile`'s run issues whose sub-issues are all closed now and saves what
/// is left open; returns those it closed.
pub async fn settle(profile: &Profile) -> Result<Vec<u64>> {
    let Some(mut tracker) = open(profile).await? else {
        return Ok(Vec::new());
    };
    let closed = tracker.settle().await?;
    if !closed.is_empty() {
        save_open_runs(&profile.tracker_path(), &tracker.config.open_runs, &closed)?;
    }
    Ok(closed)
}

/// Saves `open` as the tracker's open run issues, keeping any added since it was
/// read, but not `closed`.
fn save_open_runs(path: &Path, open: &[u64], closed: &[u64]) -> Result<()> {
    let Some(mut config) = TrackerConfig::load(path)? else {
        return Ok(());
    };
    let mut runs: Vec<u64> = open.to_vec();
    for n in config.open_runs {
        if !runs.contains(&n) && !closed.contains(&n) {
            runs.push(n);
        }
    }
    config.open_runs = runs;
    config.save(path)
}

/// The issues a tracked run is on, kept with the run.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackedRun {
    /// The run's issue.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue: Option<IssueRef>,
    /// Each step's issue, by step.
    #[serde(default)]
    pub steps: BTreeMap<String, IssueRef>,
    /// The board the issues are on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub board: Option<String>,
    /// What went wrong while tracking it (the run went on).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub problems: Vec<String>,
}

impl TrackedRun {
    /// Lines saying where the run is tracked, and what didn't work.
    pub fn describe(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if let Some(issue) = &self.issue {
            lines.push(format!(
                "tracked on GitHub: issue #{} ({}), a sub-issue per step{}",
                issue.number,
                issue.url,
                self.board
                    .as_ref()
                    .map(|b| format!(", on the board {b}"))
                    .unwrap_or_default()
            ));
        }
        lines.extend(self.problems.iter().map(|p| format!("tracker: {p}")));
        lines
    }
}

/// A step's issue during a run.
#[derive(Debug, Clone)]
struct StepIssue {
    id: String,
    number: u64,
    url: String,
    item: Option<String>,
    open: bool,
}

impl From<&Issue> for StepIssue {
    fn from(i: &Issue) -> Self {
        StepIssue {
            id: i.id.clone(),
            number: i.number,
            url: i.url.clone(),
            item: i.item.clone(),
            open: i.open,
        }
    }
}

/// A run being tracked.
struct Run {
    tracker: Tracker,
    run_id: String,
    /// The agent that runs the plan.
    agent: String,
    issue: StepIssue,
    /// The run issue's status on the board before the run, by name.
    previous: Option<String>,
    steps: BTreeMap<String, StepIssue>,
    problems: Vec<String>,
    /// Each step's last status and agent on the board.
    last: HashMap<String, (Status, String)>,
    config_path: Option<PathBuf>,
}

/// The marker in a step issue's body, so a later run on the same issue finds it.
fn step_marker(step: &str) -> String {
    format!("<!-- proofkin:step={step} -->")
}

impl Run {
    fn problem(&mut self, what: impl Into<String>) {
        if self.problems.len() < MAX_PROBLEMS {
            self.problems.push(what.into());
        }
    }

    /// Runs `ops`; their failures (except `ignore`d ones) become problems.
    async fn apply_ops(&mut self, name: &str, ops: Vec<Op>, what: &str) {
        let results = self.tracker.run_ops(name, ops).await;
        for e in results.into_iter().filter_map(Result::err) {
            let message = format!("{e:#}");
            if !message.to_lowercase().contains("already") {
                self.problem(format!("{what}: {message}"));
            }
        }
    }

    fn tracked(&self) -> TrackedRun {
        TrackedRun {
            issue: Some(IssueRef {
                number: self.issue.number,
                url: self.issue.url.clone(),
            }),
            steps: self
                .steps
                .iter()
                .map(|(id, s)| {
                    (
                        id.clone(),
                        IssueRef {
                            number: s.number,
                            url: s.url.clone(),
                        },
                    )
                })
                .collect(),
            board: self.tracker.board.as_ref().map(|b| b.url.clone()),
            problems: self.problems.clone(),
        }
    }

    /// Who an approval's `peer` is on GitHub: you (the plan's owner) is the account.
    fn person(&self, peer: &str) -> String {
        if peer == "you" || peer == "me" {
            self.tracker.login.clone()
        } else {
            peer.to_owned()
        }
    }

    /// Who does `step`, as the board's Agent field says before it runs.
    fn planned_agent(&self, step: &Step) -> String {
        match &step.target {
            Target::Peer(name) if step.approve => self.person(name),
            Target::Peer(name) => name.clone(),
            Target::Squad(name) => format!("#{name}"),
        }
    }

    /// Sets up the run's issues (see the module docs).
    async fn begin(
        tracker: Tracker,
        plan: &Plan,
        run_id: &str,
        agent: &str,
        config_path: Option<PathBuf>,
    ) -> Result<Run> {
        let mut wanted: Vec<u64> = plan.steps.iter().filter_map(|s| s.issue).collect();
        if let Some(Track::Issue(n)) = plan.track {
            wanted.push(n);
        }
        let existing = tracker.issues(&wanted).await?;
        let mut problems = Vec::new();
        let run_issue = match &plan.track {
            Some(Track::Issue(n)) => existing[n].clone(),
            other => {
                let title = match other {
                    Some(Track::New(title)) => title.clone(),
                    _ => default_title(plan),
                };
                tracker
                    .create(&[(title, run_body(plan, run_id, agent), None)])
                    .await
                    .remove(0)
                    .context("creating the run's issue")?
            }
        };
        let mut ops = Vec::new();
        if !run_issue.open {
            ops.push(op(
                "reopenIssue",
                "ReopenIssueInput",
                json!({ "issueId": run_issue.id }),
            ));
        }
        // The steps' issues: named ones, earlier runs' sub-issues, else new ones.
        let mut steps: BTreeMap<String, Issue> = BTreeMap::new();
        let mut new = Vec::new();
        for step in &plan.steps {
            if let Some(n) = step.issue {
                let issue = existing[&n].clone();
                match issue.parent {
                    Some(p) if p == run_issue.number => {}
                    Some(p) => problems.push(format!(
                        "#{n} (step {}) is a sub-issue of #{p}, so it stays there",
                        step.id
                    )),
                    None => ops.push(op(
                        "addSubIssue",
                        "AddSubIssueInput",
                        json!({ "issueId": run_issue.id, "subIssueId": issue.id }),
                    )),
                }
                steps.insert(step.id.clone(), issue);
            } else if let Some(sub) = run_issue
                .subs
                .iter()
                .find(|s| s.open && s.body.contains(&step_marker(&step.id)))
            {
                steps.insert(step.id.clone(), sub.clone());
            } else {
                new.push((
                    step.id.clone(),
                    (
                        step_title(step),
                        step_body(step, run_id, agent),
                        Some(run_issue.id.clone()),
                    ),
                ));
            }
        }
        let made = tracker
            .create(&new.iter().map(|(_, n)| n.clone()).collect::<Vec<_>>())
            .await;
        for ((step, _), result) in new.iter().zip(made) {
            match result {
                Ok(issue) => {
                    steps.insert(step.clone(), issue);
                }
                Err(e) => problems.push(format!("step {step} has no issue: {e:#}")),
            }
        }
        // Issues that were there already get the owner as assignee too.
        let assign: Vec<&Issue> = std::iter::once(&run_issue)
            .filter(|_| matches!(plan.track, Some(Track::Issue(_))))
            .chain(
                plan.steps
                    .iter()
                    .filter(|s| s.issue.is_some())
                    .filter_map(|s| steps.get(&s.id)),
            )
            .collect();
        for issue in assign {
            ops.push(op(
                "addAssigneesToAssignable",
                "AddAssigneesToAssignableInput",
                json!({ "assignableId": issue.id, "assigneeIds": [tracker.viewer] }),
            ));
        }
        for step in &plan.steps {
            let Some(issue) = steps.get(&step.id) else {
                continue;
            };
            for dep in &step.after {
                if let Some(blocking) = steps.get(dep) {
                    ops.push(op(
                        "addBlockedBy",
                        "AddBlockedByInput",
                        json!({ "issueId": issue.id, "blockingIssueId": blocking.id }),
                    ));
                }
            }
        }
        let mut run = Run {
            issue: StepIssue::from(&run_issue),
            previous: run_issue.status.clone(),
            steps: steps
                .iter()
                .map(|(id, i)| (id.clone(), StepIssue::from(i)))
                .collect(),
            tracker,
            run_id: run_id.to_owned(),
            agent: agent.to_owned(),
            problems,
            last: HashMap::new(),
            config_path,
        };
        run.issue.open = true;
        run.apply_ops("LinkIssues", ops, "linking the issues").await;
        if run.tracker.board.is_some() {
            run.put_on_board(plan).await;
        }
        Ok(run)
    }

    /// Puts the run's issues on the board: the run in progress (by its agent), its
    /// steps to do (by who will do them).
    async fn put_on_board(&mut self, plan: &Plan) {
        let mut missing: Vec<(Option<String>, String)> = Vec::new();
        if self.issue.item.is_none() {
            missing.push((None, self.issue.id.clone()));
        }
        for (step, issue) in &self.steps {
            if issue.item.is_none() {
                missing.push((Some(step.clone()), issue.id.clone()));
            }
        }
        let ids: Vec<String> = missing.iter().map(|(_, id)| id.clone()).collect();
        let added = self.tracker.board_add(&ids).await;
        for ((step, _), result) in missing.into_iter().zip(added) {
            match (step, result) {
                (None, Ok(item)) => self.issue.item = Some(item),
                (Some(step), Ok(item)) => {
                    if let Some(s) = self.steps.get_mut(&step) {
                        s.item = Some(item);
                    }
                }
                (step, Err(e)) => self.problem(format!(
                    "putting {} on the board: {e:#}",
                    step.map(|s| format!("step {s}'s issue"))
                        .unwrap_or_else(|| "the run's issue".into())
                )),
            }
        }
        let mut ops = Vec::new();
        if let Some(item) = &self.issue.item {
            ops.extend(self.tracker.status_op(item, Status::InProgress));
            ops.extend(self.tracker.agent_op(item, &self.agent));
        }
        for step in &plan.steps {
            let Some(item) = self.steps.get(&step.id).and_then(|s| s.item.clone()) else {
                continue;
            };
            ops.extend(self.tracker.status_op(&item, Status::Todo));
            ops.extend(self.tracker.agent_op(&item, &self.planned_agent(step)));
        }
        self.apply_ops("BoardFields", ops, "setting the board's fields")
            .await;
    }

    /// Follows step `step` to `state` on its issue.
    async fn update(&mut self, step: &Step, state: &StepState) {
        let Some(issue) = self.steps.get(&step.id).cloned() else {
            return;
        };
        let mut ops = Vec::new();
        let comment = |body: String| {
            op(
                "addComment",
                "AddCommentInput",
                json!({ "subjectId": issue.id, "body": body }),
            )
        };
        let close = |reason: &str| {
            op(
                "closeIssue",
                "CloseIssueInput",
                json!({ "issueId": issue.id, "stateReason": reason }),
            )
        };
        let (status, who, open) = match state {
            StepState::Waiting => return,
            StepState::Running { peer, .. } => {
                let (status, who) = if step.approve {
                    (Status::InReview, self.person(peer))
                } else {
                    (Status::InProgress, peer.clone())
                };
                if self.last.get(&step.id) == Some(&(status, who.clone())) {
                    return;
                }
                if !issue.open {
                    ops.push(op(
                        "reopenIssue",
                        "ReopenIssueInput",
                        json!({ "issueId": issue.id }),
                    ));
                }
                (status, who, true)
            }
            StepState::Done {
                peer,
                output,
                evidence,
                verified,
                ..
            } => {
                let who = if step.approve {
                    self.person(peer)
                } else {
                    peer.clone()
                };
                let to_merge = !step.approve
                    && evidence
                        .as_ref()
                        .and_then(|e| e.git.as_ref())
                        .is_some_and(|g| g.commit.is_some());
                ops.push(comment(done_comment(
                    step,
                    &who,
                    output,
                    evidence.as_ref(),
                    *verified,
                    &self.run_id,
                )));
                if to_merge {
                    (Status::InReview, who, true)
                } else {
                    ops.push(close("COMPLETED"));
                    (Status::Done, who, false)
                }
            }
            StepState::Failed {
                peer,
                error,
                evidence,
                verified,
            } => {
                let who = peer
                    .as_deref()
                    .map(|p| self.person(p))
                    .unwrap_or_else(|| self.planned_agent(step));
                ops.push(comment(failed_comment(
                    step,
                    peer.as_deref().map(|_| who.as_str()),
                    error,
                    evidence.as_ref(),
                    *verified,
                    &self.run_id,
                )));
                (Status::Todo, who, issue.open)
            }
            StepState::Skipped { reason } => {
                ops.push(comment(format!(
                    "⏭️ Step `{}` of run `{}` was skipped: {}. This issue is back to do.",
                    step.id,
                    self.run_id,
                    safe(reason, 500)
                )));
                (Status::Todo, self.planned_agent(step), issue.open)
            }
        };
        if let Some(item) = &issue.item {
            ops.extend(self.tracker.status_op(item, status));
            ops.extend(self.tracker.agent_op(item, &who));
        }
        self.last.insert(step.id.clone(), (status, who));
        if let Some(s) = self.steps.get_mut(&step.id) {
            s.open = open;
        }
        self.apply_ops(
            "StepUpdate",
            ops,
            &format!("updating step {}'s issue", step.id),
        )
        .await;
    }

    /// Ends the run on its issue: the verdict, then done (nothing left to merge), in
    /// review (its steps' work waits for pull requests) or back where it was.
    async fn finish(&mut self, run: &RunRecord) {
        let succeeded = run.passed()
            || (run.claims.is_empty()
                && run
                    .steps
                    .iter()
                    .all(|s| matches!(s.state, StepState::Done { .. })));
        let mut ops = vec![op(
            "addComment",
            "AddCommentInput",
            json!({ "subjectId": self.issue.id, "body": run_comment(run, &self.steps, &self.tracker.login) }),
        )];
        let open_subs: Vec<u64> = match self.tracker.issues(&[self.issue.number]).await {
            Ok(now) => now
                .get(&self.issue.number)
                .map(|i| i.subs.iter().filter(|s| s.open).map(|s| s.number).collect())
                .unwrap_or_default(),
            Err(e) => {
                self.problem(format!("reading the run's issue: {e:#}"));
                Vec::new()
            }
        };
        let ours: HashSet<u64> = self.steps.values().map(|s| s.number).collect();
        let mut keep_open = false;
        let status = if succeeded && open_subs.is_empty() {
            ops.push(op(
                "closeIssue",
                "CloseIssueInput",
                json!({ "issueId": self.issue.id, "stateReason": "COMPLETED" }),
            ));
            Some(Status::Done)
        } else if succeeded && open_subs.iter().all(|n| ours.contains(n)) {
            keep_open = true;
            Some(Status::InReview)
        } else {
            None
        };
        if let Some(item) = self.issue.item.clone() {
            let set = match status {
                Some(status) => self.tracker.status_op(&item, status),
                None => self
                    .previous
                    .as_deref()
                    .and_then(|name| self.tracker.status_named_op(&item, name))
                    .or_else(|| self.tracker.status_op(&item, Status::Todo)),
            };
            ops.extend(set);
        }
        self.apply_ops("RunEnd", ops, "ending the run's issue")
            .await;
        if let Some(path) = self.config_path.clone() {
            if keep_open {
                let saved = TrackerConfig::load(&path).and_then(|config| match config {
                    Some(mut config) if !config.open_runs.contains(&self.issue.number) => {
                        config.open_runs.push(self.issue.number);
                        config.save(&path)
                    }
                    _ => Ok(()),
                });
                if let Err(e) = saved {
                    self.problem(format!("saving the tracker: {e:#}"));
                }
            }
            // Earlier runs whose pull requests have merged since.
            let mut others = self.tracker.config.clone();
            if let Ok(Some(now)) = TrackerConfig::load(&path) {
                others.open_runs = now
                    .open_runs
                    .into_iter()
                    .filter(|n| *n != self.issue.number)
                    .collect();
            }
            if !others.open_runs.is_empty() {
                let mut tracker = Tracker {
                    api: self.tracker.api.clone(),
                    config: others,
                    login: self.tracker.login.clone(),
                    viewer: self.tracker.viewer.clone(),
                    repo_id: self.tracker.repo_id.clone(),
                    repo_url: self.tracker.repo_url.clone(),
                    board: self.tracker.board.clone(),
                    board_problem: None,
                };
                if let Ok(closed) = tracker.settle().await
                    && !closed.is_empty()
                    && let Err(e) = save_open_runs(&path, &tracker.config.open_runs, &closed)
                {
                    self.problem(format!("saving the tracker: {e:#}"));
                }
            }
        }
    }
}

/// A new run issue's title: the plan's first claim, else its first step's title.
fn default_title(plan: &Plan) -> String {
    let text = plan
        .claims
        .first()
        .map(|c| c.text.clone())
        .or_else(|| {
            plan.steps
                .first()
                .map(|s| crate::llm::title_of(s.own_text()))
        })
        .unwrap_or_else(|| "Plan run".into());
    clip(&text.split_whitespace().collect::<Vec<_>>().join(" "), 200)
}

fn step_title(step: &Step) -> String {
    let title = crate::llm::title_of(step.own_text());
    if step.approve {
        format!("Sign-off: {title}")
    } else {
        title
    }
}

/// `text` cut to at most `max` characters, ending with an ellipsis if it was longer.
fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut cut: String = text.chars().take(max.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

/// `text` without secrets, at most `max` characters.
fn safe(text: &str, max: usize) -> String {
    clip(&crate::redact::redact(text).0, max)
}

/// `text` in a fenced block its own backticks can't close: nothing in it mentions
/// people or links issues.
fn fenced(text: &str) -> String {
    let longest = text.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(longest.max(2) + 1);
    format!("{fence}text\n{}\n{fence}", text.trim_end())
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

fn run_body(plan: &Plan, run_id: &str, agent: &str) -> String {
    let mut md = format!(
        "**{agent}**'s plan run `{run_id}`, tracked by Proofkin: each step is a sub-issue, and the board follows the run.\n"
    );
    if !plan.claims.is_empty() {
        md += "\n**What it must prove**\n\n";
        for c in &plan.claims {
            md += &format!(
                "- `{}`: {}\n",
                c.id,
                safe(&c.text, 500).replace(['\r', '\n'], " ")
            );
        }
    }
    md += "\n**Steps**\n\n";
    for s in &plan.steps {
        md += &format!(
            "- `{}`{} `{}{}`: {}\n",
            s.id,
            if s.after.is_empty() {
                String::new()
            } else {
                format!(" (after {})", s.after.join(", "))
            },
            s.target,
            s.share
                .as_deref()
                .map(|f| format!(":{f}"))
                .unwrap_or_default(),
            safe(&step_title(s), 200).replace('`', "'")
        );
    }
    md += &format!("\n<!-- proofkin:run={run_id} -->\n");
    md
}

fn step_body(step: &Step, run_id: &str, agent: &str) -> String {
    let target = format!(
        "`{}{}`",
        step.target,
        step.share
            .as_deref()
            .map(|f| format!(":{f}"))
            .unwrap_or_default()
    );
    let who = if step.approve {
        if step.target == Target::Peer("me".into()) {
            "The plan's owner signs it off.".to_owned()
        } else {
            format!("{target} signs it off.")
        }
    } else if step.team.is_some() {
        format!("{target}'s team does it.")
    } else if matches!(step.target, Target::Squad(_)) {
        format!("A member of squad {target} does it with {}.", step.provider)
    } else {
        format!("{target} does it with {}.", step.provider)
    };
    let mut md = format!(
        "Step `{}` of **{agent}**'s plan run `{run_id}`. {who}\n\n{}\n",
        step.id,
        fenced(&safe(step.own_text(), MAX_TEXT))
    );
    let list = |items: &[String]| {
        items
            .iter()
            .map(|i| format!("`{i}`"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut facts = Vec::new();
    if !step.checks.is_empty() {
        facts.push(format!(
            "Checks: {}{}",
            list(&step.checks),
            step.attempts
                .filter(|a| *a > 1)
                .map(|a| format!(" (up to {a} attempts)"))
                .unwrap_or_default()
        ));
    }
    if !step.claims.is_empty() {
        facts.push(format!("Proves: {}", list(&step.claims)));
    }
    if !step.after.is_empty() {
        facts.push(format!(
            "After: {} (blocked by {})",
            list(&step.after),
            if step.after.len() == 1 {
                "its issue"
            } else {
                "their issues"
            }
        ));
    }
    if let Some(apply) = &step.apply {
        facts.push(format!("Starts from the work of `{apply}`"));
    }
    if let Some(secs) = step.timeout {
        facts.push(format!("Time limit: {}", crate::plan::timeout_text(secs)));
    }
    if !facts.is_empty() {
        md += "\n";
        for f in facts {
            md += &format!("- {f}\n");
        }
    }
    md += &format!(
        "\n<sub>Proofkin keeps this issue's status in step with the run.</sub>\n{}\n",
        step_marker(&step.id)
    );
    md
}

/// "Signed by worker (verified): ✓ fmt, ✗ test; 2 attempts."
fn evidence_line(e: &Evidence, verified: bool) -> String {
    let checks = if e.checks.is_empty() {
        "no checks ran".to_owned()
    } else {
        e.checks
            .iter()
            .map(|c| format!("{} `{}`", if c.passed { "✓" } else { "✗" }, c.name))
            .collect::<Vec<_>>()
            .join(", ")
    };
    format!(
        "Signed by **{}** ({}): {checks}{}.",
        e.signer,
        if verified { "verified" } else { "NOT verified" },
        if e.attempts > 1 {
            format!("; {} attempts", e.attempts)
        } else {
            String::new()
        }
    )
}

fn done_comment(
    step: &Step,
    who: &str,
    output: &str,
    evidence: Option<&Evidence>,
    verified: bool,
    run_id: &str,
) -> String {
    if step.approve {
        let mut md = format!(
            "✅ **{who}** signed off step `{}` of run `{run_id}`.",
            step.id
        );
        let note = output.trim();
        if !note.is_empty() {
            md += &format!("\n\n{}", fenced(&safe(note, MAX_RESULT)));
        }
        return md;
    }
    let mut md = format!("✅ **{who}** did step `{}` of run `{run_id}`.", step.id);
    if let Some(e) = evidence {
        md += &format!("\n\n{}", evidence_line(e, verified));
    }
    match evidence.and_then(|e| e.git.as_ref().map(|g| (e, g))) {
        Some((e, g)) if g.commit.is_some() => {
            md += &format!(
                "\n\nIts commit `{}` ({} file{}) is on branch `{}` in **{who}**'s folder `{}`: a pull request from that branch closes this issue.",
                g.commit
                    .as_deref()
                    .unwrap_or_default()
                    .chars()
                    .take(12)
                    .collect::<String>(),
                g.files.len(),
                plural(g.files.len()),
                g.branch,
                e.share.as_deref().unwrap_or("?")
            );
        }
        _ => md += "\n\nIt changed no files, so this issue is done.",
    }
    md += &format!(
        "\n\n<details><summary>Result</summary>\n\n{}\n\n</details>",
        fenced(&safe(output, MAX_RESULT))
    );
    md
}

fn failed_comment(
    step: &Step,
    who: Option<&str>,
    error: &str,
    evidence: Option<&Evidence>,
    verified: bool,
    run_id: &str,
) -> String {
    let mut md = if step.approve {
        format!(
            "❌ Step `{}` of run `{run_id}` wasn't signed off{}. This issue is back to do.",
            step.id,
            who.map(|w| format!(" by **{w}**")).unwrap_or_default()
        )
    } else {
        format!(
            "❌ Step `{}` of run `{run_id}` failed{}. This issue is back to do.",
            step.id,
            who.map(|w| format!(" on **{w}**")).unwrap_or_default()
        )
    };
    if let Some(e) = evidence {
        md += &format!("\n\n{}", evidence_line(e, verified));
    }
    md += &format!(
        "\n\n<details><summary>Why</summary>\n\n{}\n\n</details>",
        fenced(&safe(error, MAX_RESULT))
    );
    md
}

fn run_comment(run: &RunRecord, steps: &BTreeMap<String, StepIssue>, login: &str) -> String {
    let mut md = format!(
        "{} Run `{}` of **{}** ended: **{}**.\n",
        if run.passed() { "✅" } else { "🏁" },
        run.id,
        run.boss,
        run.verdict_line()
    );
    let results = run.claim_results();
    if !results.is_empty() {
        md += "\n| Claim | Statement | Verdict |\n| --- | --- | --- |\n";
        for r in &results {
            md += &format!(
                "| `{}` | {} | {} |\n",
                r.claim.id,
                safe(&r.claim.text, 300)
                    .replace('|', "\\|")
                    .replace(['\r', '\n'], " "),
                r.verdict.label()
            );
        }
    }
    md += "\n";
    for s in &run.steps {
        let state = match &s.state {
            // "you" is the plan's owner: on GitHub, its account.
            StepState::Done { peer, .. } if peer == "you" => format!("done by {login}"),
            StepState::Done { peer, .. } => format!("done by {peer}"),
            StepState::Failed { .. } => "failed".to_owned(),
            StepState::Skipped { .. } => "skipped".to_owned(),
            _ => "unfinished".to_owned(),
        };
        md += &format!(
            "- `{}`: {state}{}\n",
            s.step.id,
            steps
                .get(&s.step.id)
                .map(|i| format!(" (#{})", i.number))
                .unwrap_or_default()
        );
    }
    md += &format!(
        "\nIts evidence is with **{}**: `proofkin runs verify {}` re-checks every signature.",
        run.boss, run.id
    );
    md
}

/// Each step's issue references for its task: its own, then those of the steps
/// whose work it applies.
fn issue_refs(plan: &Plan, run: &Run) -> HashMap<String, Vec<String>> {
    let mut refs = HashMap::new();
    for step in &plan.steps {
        let mut list = Vec::new();
        let mut seen = HashSet::new();
        let mut at = Some(step);
        while let Some(s) = at {
            if !seen.insert(s.id.as_str()) {
                break;
            }
            if let Some(issue) = run.steps.get(&s.id) {
                list.push(run.tracker.config.issue_ref(issue.number));
            }
            at = s
                .apply
                .as_ref()
                .and_then(|a| plan.steps.iter().find(|x| &x.id == a));
        }
        if !list.is_empty() {
            refs.insert(step.id.clone(), list);
        }
    }
    refs
}

enum Msg {
    Step(String, Box<StepState>),
    Finish(Box<RunRecord>),
}

/// A run being tracked: set up by [`start`], told about each step's state with
/// [`Tracking::update`] (which never waits) and ended with [`Tracking::finish`].
pub struct Tracking {
    tx: tokio::sync::mpsc::UnboundedSender<Msg>,
    task: tokio::task::JoinHandle<TrackedRun>,
    /// The issue references each step's task carries (see [`crate::plan::RunContext`]).
    pub refs: HashMap<String, Vec<String>>,
    /// The issues, as set up when the run started.
    pub tracked: TrackedRun,
}

impl Tracking {
    pub fn update(&self, step: &str, state: &StepState) {
        if !matches!(state, StepState::Waiting) {
            let _ = self
                .tx
                .send(Msg::Step(step.to_owned(), Box::new(state.clone())));
        }
    }

    /// Ends the run on its issues (waiting for the updates before it) and returns
    /// where it is tracked, with every problem.
    pub async fn finish(self, run: &RunRecord) -> TrackedRun {
        let mut fallback = self.tracked.clone();
        let _ = self.tx.send(Msg::Finish(Box::new(run.clone())));
        drop(self.tx);
        match tokio::time::timeout(FINISH_TIMEOUT, self.task).await {
            Ok(Ok(tracked)) => tracked,
            _ => {
                fallback
                    .problems
                    .push("tracking didn't catch up in time: the issues may be behind".into());
                fallback
            }
        }
    }
}

/// Starts tracking `plan`'s run `run_id` (by agent `agent`) with `profile`'s tracker
/// and GitHub account: sets up its issues before its steps start. None if the plan
/// says `track: off` or the profile has no tracker.
pub async fn start(
    profile: &Profile,
    plan: &Plan,
    run_id: &str,
    agent: &str,
) -> Result<Option<Tracking>> {
    if plan.track == Some(Track::Off) {
        return Ok(None);
    }
    let path = profile.tracker_path();
    let Some(config) = TrackerConfig::load(&path)? else {
        if plan.names_issues() {
            bail!(
                "the plan names issues (track: or issue=), but profile '{}' has no tracker; link one with: proofkin tracker link <owner/repo> --profile {}",
                profile.name,
                profile.name
            );
        }
        return Ok(None);
    };
    let creds = profile.github()?;
    if creds.host != config.host {
        bail!(
            "the tracker is on {}, but profile '{}' is signed in to {}",
            config.host,
            profile.name,
            creds.host
        );
    }
    start_with(
        Arc::new(GhApi::new(creds)),
        config,
        plan,
        run_id,
        agent,
        Some(path),
    )
    .await
    .map(Some)
}

/// [`start`] with `api` and `config` (saved at `config_path`, if any).
pub async fn start_with(
    api: Arc<dyn Api>,
    config: TrackerConfig,
    plan: &Plan,
    run_id: &str,
    agent: &str,
    config_path: Option<PathBuf>,
) -> Result<Tracking> {
    let tracker = Tracker::connect(api, config).await?;
    let board_problem = tracker.board_problem.clone();
    let mut run = Run::begin(tracker, plan, run_id, agent, config_path).await?;
    if let Some(p) = board_problem {
        run.problems
            .insert(0, format!("the board isn't updated: {p}"));
    }
    let refs = issue_refs(plan, &run);
    let tracked = run.tracked();
    let steps: HashMap<String, Step> = plan
        .steps
        .iter()
        .map(|s| (s.id.clone(), s.clone()))
        .collect();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            match msg {
                Msg::Step(id, state) => {
                    if let Some(step) = steps.get(&id) {
                        run.update(step, &state).await;
                    }
                }
                Msg::Finish(record) => run.finish(&record).await,
            }
        }
        run.tracked()
    });
    Ok(Tracking {
        tx,
        task,
        refs,
        tracked,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Debug, Clone, Default)]
    struct FakeIssue {
        number: u64,
        open: bool,
        body: String,
        parent: Option<u64>,
        item: Option<String>,
        status: Option<String>,
    }

    /// GitHub as far as tracking uses it: issues, sub-issues and a board, with every
    /// mutation recorded as `field input`.
    #[derive(Default)]
    struct Fake {
        issues: Mutex<BTreeMap<u64, FakeIssue>>,
        calls: Mutex<Vec<(String, Value)>>,
        no_project_scope: bool,
        review_column: bool,
    }

    const STATUSES: &[(&str, &str)] = &[
        ("o-todo", "Todo"),
        ("o-prog", "🏗 In progress"),
        ("o-rev", "👀 In review"),
        ("o-done", "Done"),
    ];

    impl Fake {
        fn with(issues: &[FakeIssue]) -> Arc<Fake> {
            Arc::new(Fake {
                issues: Mutex::new(issues.iter().map(|i| (i.number, i.clone())).collect()),
                review_column: true,
                ..Fake::default()
            })
        }

        fn issue_json(&self, n: u64) -> Value {
            let issues = self.issues.lock().unwrap();
            let Some(i) = issues.get(&n) else {
                return Value::Null;
            };
            let subs: Vec<&FakeIssue> = issues.values().filter(|s| s.parent == Some(n)).collect();
            let node = |i: &FakeIssue| {
                json!({ "id": format!("I{}", i.number), "number": i.number, "url": format!("https://github.com/acme/app/issues/{}", i.number),
                        "state": if i.open { "OPEN" } else { "CLOSED" }, "body": i.body })
            };
            let mut v = node(i);
            v["parent"] = i
                .parent
                .map(|p| json!({ "number": p }))
                .unwrap_or(Value::Null);
            v["subIssues"] = json!({ "nodes": subs.iter().map(|s| node(s)).collect::<Vec<_>>() });
            v["subIssuesSummary"] = json!({ "total": subs.len(), "completed": subs.iter().filter(|s| !s.open).count() });
            v["projectItems"] = json!({ "nodes": i.item.iter().map(|item| json!({ "id": item, "project": { "id": "P1" },
                "fieldValueByName": i.status.as_ref().map(|s| json!({ "name": s })).unwrap_or(Value::Null) })).collect::<Vec<_>>() });
            v
        }

        fn number(id: &str) -> u64 {
            id.trim_start_matches('I').parse().unwrap()
        }

        fn mutate(&self, field: &str, input: &Value) -> Value {
            self.calls
                .lock()
                .unwrap()
                .push((field.to_owned(), input.clone()));
            let mut issues = self.issues.lock().unwrap();
            let s = |key: &str| input[key].as_str().unwrap_or_default().to_owned();
            match field {
                "createIssue" => {
                    let number = issues.keys().max().copied().unwrap_or(0) + 1;
                    let parent = input
                        .get("parentIssueId")
                        .and_then(Value::as_str)
                        .map(Fake::number);
                    issues.insert(
                        number,
                        FakeIssue {
                            number,
                            open: true,
                            body: s("body"),
                            parent,
                            ..FakeIssue::default()
                        },
                    );
                    json!({ "issue": { "id": format!("I{number}"), "number": number, "url": format!("https://github.com/acme/app/issues/{number}") } })
                }
                "addSubIssue" => {
                    let child = Fake::number(&s("subIssueId"));
                    issues.get_mut(&child).unwrap().parent = Some(Fake::number(&s("issueId")));
                    json!({ "clientMutationId": null })
                }
                "addProjectV2ItemById" => {
                    let n = Fake::number(&s("contentId"));
                    let item = format!("PVTI{n}");
                    issues.get_mut(&n).unwrap().item = Some(item.clone());
                    json!({ "item": { "id": item } })
                }
                "updateProjectV2ItemFieldValue" => {
                    if let Some(option) = input
                        .pointer("/value/singleSelectOptionId")
                        .and_then(Value::as_str)
                    {
                        let n: u64 = s("itemId").trim_start_matches("PVTI").parse().unwrap();
                        let name = STATUSES.iter().find(|(id, _)| *id == option).unwrap().1;
                        issues.get_mut(&n).unwrap().status = Some(name.to_owned());
                    }
                    json!({ "clientMutationId": null })
                }
                "closeIssue" => {
                    issues.get_mut(&Fake::number(&s("issueId"))).unwrap().open = false;
                    json!({ "clientMutationId": null })
                }
                "reopenIssue" => {
                    issues.get_mut(&Fake::number(&s("issueId"))).unwrap().open = true;
                    json!({ "clientMutationId": null })
                }
                _ => json!({ "clientMutationId": null }),
            }
        }

        /// The calls to `field`, their inputs.
        fn calls(&self, field: &str) -> Vec<Value> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(f, _)| f == field)
                .map(|(_, v)| v.clone())
                .collect()
        }

        fn status(&self, n: u64) -> Option<String> {
            self.issues.lock().unwrap()[&n].status.clone()
        }

        fn open(&self, n: u64) -> bool {
            self.issues.lock().unwrap()[&n].open
        }
    }

    #[async_trait]
    impl Api for Fake {
        async fn graphql(&self, query: &str, variables: Value) -> Result<Reply> {
            let name = query
                .split(['(', '{'])
                .next()
                .unwrap()
                .split_whitespace()
                .nth(1)
                .unwrap_or_default();
            let data = match name {
                "Repo" => {
                    json!({ "viewer": { "id": "U1", "login": "alice" }, "repository": { "id": "R1", "url": "https://github.com/acme/app" } })
                }
                "Board" if self.no_project_scope => {
                    return Ok(Reply::from_value(
                        json!({ "data": { "repositoryOwner": null }, "errors": [{
                        "type": "INSUFFICIENT_SCOPES",
                        "message": "Your token has not been granted the required scopes to execute this query. The 'projectV2' field requires one of the following scopes: ['read:project']" }] }),
                    ));
                }
                "Board" => {
                    let options: Vec<Value> = STATUSES
                        .iter()
                        .filter(|(id, _)| self.review_column || *id != "o-rev")
                        .map(|(id, name)| json!({ "id": id, "name": name }))
                        .collect();
                    json!({ "repositoryOwner": { "projectV2": { "id": "P1", "title": "Tracker", "url": "https://github.com/users/alice/projects/4", "fields": { "nodes": [
                        { "id": "F-title", "name": "Title", "dataType": "TITLE" },
                        { "id": "F-status", "name": "Status", "dataType": "SINGLE_SELECT", "options": options }
                    ] } } } })
                }
                "AgentField" => {
                    self.calls
                        .lock()
                        .unwrap()
                        .push(("createProjectV2Field".into(), variables["input"].clone()));
                    json!({ "createProjectV2Field": { "projectV2Field": { "id": "F-agent" } } })
                }
                "Issues" => {
                    let mut repo = serde_json::Map::new();
                    for part in query.split(": issue(number: ").skip(1) {
                        let n: u64 = part.split(')').next().unwrap().parse().unwrap();
                        repo.insert(format!("n{n}"), self.issue_json(n));
                    }
                    json!({ "repository": repo })
                }
                _ if query.starts_with("mutation") => {
                    let mut data = serde_json::Map::new();
                    for part in query.split(" m").skip(1) {
                        let Some((i, rest)) = part.split_once(": ") else {
                            continue;
                        };
                        let Ok(i) = i.parse::<usize>() else {
                            continue;
                        };
                        let field = rest.split('(').next().unwrap();
                        let input = variables[format!("m{i}")].clone();
                        data.insert(format!("m{i}"), self.mutate(field, &input));
                    }
                    Value::Object(data)
                }
                other => panic!("unexpected query {other}: {query}"),
            };
            Ok(Reply {
                data,
                errors: Vec::new(),
            })
        }
    }

    fn config() -> TrackerConfig {
        TrackerConfig {
            repo: "acme/app".into(),
            host: "github.com".into(),
            project: Some(ProjectRef {
                owner: "alice".into(),
                number: 4,
            }),
            open_runs: Vec::new(),
        }
    }

    fn evidence(commit: Option<&str>) -> Evidence {
        let key = iroh::SecretKey::generate();
        let mut e = Evidence::new(
            crate::evidence::EvidenceKind::Task,
            "worker",
            key.public(),
            key.public(),
            7,
        );
        e.share = Some("app".into());
        e.attempts = 1;
        e.checks.push(crate::evidence::CheckResult {
            name: "test".into(),
            command: "cargo test".into(),
            exit_code: Some(0),
            passed: true,
            millis: 1500,
            output: "ok".into(),
        });
        e.git = Some(crate::evidence::GitCommit {
            branch: "p2p/boss/7-1".into(),
            base: "abc".into(),
            commit: commit.map(str::to_owned),
            files: vec!["src/lib.rs".into()],
            patch_sha256: None,
        });
        e
    }

    fn done(peer: &str, evidence: Option<Evidence>) -> StepState {
        StepState::Done {
            peer: peer.into(),
            output: "Did it.".into(),
            evidence,
            verified: true,
            patch: None,
        }
    }

    fn running(peer: &str) -> StepState {
        StepState::Running {
            peer: peer.into(),
            progress: None,
        }
    }

    const PLAN: &str = "claim: C-1 The tracker shows plan runs\n\
        api @worker:app verify=test claims=C-1 Add the API\n\
        ui after api @worker:app apply=api Show it in the web UI\n\
        signoff after ui approve @me Approve the change";

    #[tokio::test]
    async fn a_tracked_run_has_an_issue_with_a_sub_issue_per_step_and_the_board_follows_it() {
        let fake = Fake::with(&[]);
        let plan = Plan::parse(PLAN).unwrap();
        let tracking = start_with(fake.clone(), config(), &plan, "r1", "boss", None)
            .await
            .unwrap();

        // The run's issue, titled by its claim, and a sub-issue per step, assigned.
        let created = fake.calls("createIssue");
        assert_eq!(created.len(), 4);
        assert_eq!(created[0]["title"], "The tracker shows plan runs");
        assert!(
            created[0]["body"]
                .as_str()
                .unwrap()
                .contains("plan run `r1`")
        );
        assert_eq!(created[1]["title"], "Add the API");
        assert_eq!(created[3]["title"], "Sign-off: Approve the change");
        for c in &created[1..] {
            assert_eq!(c["parentIssueId"], "I1");
        }
        assert!(created.iter().all(|c| c["assigneeIds"] == json!(["U1"])));
        let body = created[2]["body"].as_str().unwrap();
        assert!(
            body.contains("Show it in the web UI") && body.contains("<!-- proofkin:step=ui -->"),
            "{body}"
        );
        assert!(
            body.contains("`@worker:app` does it with copilot"),
            "{body}"
        );
        // Dependencies are "blocked by"; the board gets an Agent field and every issue.
        let blocked: Vec<(Value, Value)> = fake
            .calls("addBlockedBy")
            .iter()
            .map(|c| (c["issueId"].clone(), c["blockingIssueId"].clone()))
            .collect();
        assert_eq!(
            blocked,
            [(json!("I3"), json!("I2")), (json!("I4"), json!("I3"))]
        );
        assert_eq!(fake.calls("createProjectV2Field")[0]["name"], "Agent");
        assert_eq!(fake.calls("addProjectV2ItemById").len(), 4);
        assert_eq!(fake.status(1).as_deref(), Some("🏗 In progress"));
        assert_eq!(fake.status(2).as_deref(), Some("Todo"));
        let agents: Vec<Value> = fake
            .calls("updateProjectV2ItemFieldValue")
            .iter()
            .filter_map(|c| c.pointer("/value/text").cloned())
            .collect();
        assert_eq!(
            agents,
            [
                json!("boss"),
                json!("worker"),
                json!("worker"),
                json!("alice")
            ]
        );
        // Tasks carry their issue, and the issues of the work they apply.
        assert_eq!(tracking.refs["api"], ["acme/app#2"]);
        assert_eq!(tracking.refs["ui"], ["acme/app#3", "acme/app#2"]);
        assert_eq!(tracking.tracked.issue.as_ref().unwrap().number, 1);
        assert_eq!(tracking.tracked.steps["signoff"].number, 4);
        assert!(
            tracking.tracked.problems.is_empty(),
            "{:?}",
            tracking.tracked.problems
        );

        // The board follows the steps.
        tracking.update("api", &running("worker"));
        tracking.update("api", &running("worker"));
        tracking.update(
            "api",
            &done("worker", Some(evidence(Some("0123456789abcdef")))),
        );
        tracking.update("ui", &running("worker"));
        tracking.update("ui", &done("worker", Some(evidence(None))));
        tracking.update("signoff", &running("you"));
        tracking.update("signoff", &done("you", None));
        let mut states = BTreeMap::new();
        for (id, state) in [
            (
                "api",
                done("worker", Some(evidence(Some("0123456789abcdef")))),
            ),
            ("ui", done("worker", Some(evidence(None)))),
            ("signoff", done("you", None)),
        ] {
            states.insert(id.to_owned(), state);
        }
        let mut record = RunRecord::new("boss", "id", PLAN, &plan, &states, 0);
        record.id = "r1".into();
        let tracked = tracking.finish(&record).await;
        assert!(tracked.problems.is_empty(), "{:?}", tracked.problems);

        // Work to merge waits in review; the rest is done and closed.
        assert!(fake.open(2) && fake.status(2).as_deref() == Some("👀 In review"));
        assert!(!fake.open(3) && fake.status(3).as_deref() == Some("Done"));
        assert!(!fake.open(4) && fake.status(4).as_deref() == Some("Done"));
        let comments: Vec<String> = fake
            .calls("addComment")
            .iter()
            .map(|c| c["body"].as_str().unwrap().to_owned())
            .collect();
        assert!(
            comments[0].starts_with("✅ **worker** did step `api` of run `r1`."),
            "{}",
            comments[0]
        );
        assert!(
            comments[0].contains("Signed by **worker** (verified): ✓ `test`."),
            "{}",
            comments[0]
        );
        assert!(
            comments[0].contains("commit `0123456789ab` (1 file) is on branch `p2p/boss/7-1`")
                && comments[0].contains("a pull request from that branch closes this issue"),
            "{}",
            comments[0]
        );
        assert!(
            comments[1].contains("It changed no files"),
            "{}",
            comments[1]
        );
        assert!(
            comments[2].starts_with("✅ **alice** signed off step `signoff`"),
            "{}",
            comments[2]
        );
        let end = comments.last().unwrap();
        assert!(
            end.contains("PASS: 1/1 claims proven")
                && end.contains("- `api`: done by worker (#2)")
                && end.contains("- `signoff`: done by alice (#4)"),
            "{end}"
        );
        // Its run waits in review for the pull request.
        assert!(fake.open(1) && fake.status(1).as_deref() == Some("👀 In review"));
        // "worker" moved to In progress once, though it said so twice.
        let progress = fake
            .calls("updateProjectV2ItemFieldValue")
            .iter()
            .filter(|c| {
                c["itemId"] == "PVTI2"
                    && c.pointer("/value/singleSelectOptionId") == Some(&json!("o-prog"))
            })
            .count();
        assert_eq!(progress, 1);

        // Once the pull request closes its last sub-issue, settling closes the run.
        fake.issues.lock().unwrap().get_mut(&2).unwrap().open = false;
        let mut tracker = Tracker::connect(
            fake.clone(),
            TrackerConfig {
                open_runs: vec![1],
                ..config()
            },
        )
        .await
        .unwrap();
        assert_eq!(tracker.settle().await.unwrap(), [1]);
        assert!(tracker.config.open_runs.is_empty());
        assert!(!fake.open(1) && fake.status(1).as_deref() == Some("Done"));
    }

    #[tokio::test]
    async fn without_the_project_scope_tracking_keeps_to_issues_and_says_how_to_fix_it() {
        let fake = Arc::new(Fake {
            no_project_scope: true,
            ..Fake::default()
        });
        let plan = Plan::parse("a @worker Do it").unwrap();
        let tracking = start_with(fake.clone(), config(), &plan, "r2", "boss", None)
            .await
            .unwrap();
        assert_eq!(fake.calls("createIssue").len(), 2);
        assert!(fake.calls("addProjectV2ItemById").is_empty());
        let problem = &tracking.tracked.problems[0];
        assert!(
            problem.starts_with(
                "the board isn't updated: the GitHub account alice can't use projects yet"
            ) && problem.contains("gh auth refresh --hostname github.com --scopes project"),
            "{problem}"
        );
        assert_eq!(tracking.tracked.board, None);
        tracking.update("a", &done("worker", None));
        let mut record = RunRecord::new(
            "boss",
            "id",
            "a @worker Do it",
            &plan,
            &BTreeMap::from([("a".to_owned(), done("worker", None))]),
            0,
        );
        record.id = "r2".into();
        let tracked = tracking.finish(&record).await;
        // A plan without claims that did every step is done: nothing waits for a merge.
        assert!(!fake.open(1) && !fake.open(2));
        assert_eq!(tracked.problems.len(), 1, "{:?}", tracked.problems);
    }

    #[tokio::test]
    async fn a_run_on_an_issue_uses_its_steps_issues_and_a_failure_sends_them_back_to_do() {
        let fake = Fake::with(&[
            FakeIssue {
                number: 10,
                open: true,
                body: "The feature".into(),
                status: Some("Todo".into()),
                item: Some("PVTI10".into()),
                ..FakeIssue::default()
            },
            FakeIssue {
                number: 11,
                open: true,
                body: format!("Earlier run\n{}", step_marker("api")),
                parent: Some(10),
                item: Some("PVTI11".into()),
                ..FakeIssue::default()
            },
            FakeIssue {
                number: 12,
                open: false,
                body: "Docs".into(),
                ..FakeIssue::default()
            },
            FakeIssue {
                number: 13,
                open: true,
                body: "Another part".into(),
                parent: Some(10),
                ..FakeIssue::default()
            },
        ]);
        let text = "track: #10\napi @worker:app Add the API\ndocs after api @worker:app issue=#12 Document it";
        let plan = Plan::parse(text).unwrap();
        let tracking = start_with(fake.clone(), config(), &plan, "r3", "boss", None)
            .await
            .unwrap();
        assert!(
            fake.calls("createIssue").is_empty(),
            "{:?}",
            fake.calls("createIssue")
        );
        assert_eq!(fake.calls("addSubIssue")[0]["subIssueId"], "I12");
        assert_eq!(fake.calls("addAssigneesToAssignable").len(), 2);
        assert_eq!(tracking.refs["docs"], ["acme/app#12"]);
        assert_eq!(fake.status(10).as_deref(), Some("🏗 In progress"));

        tracking.update(
            "api",
            &StepState::failed(Some("worker".into()), "check test failed".into()),
        );
        tracking.update(
            "docs",
            &StepState::Skipped {
                reason: "step api did not succeed".into(),
            },
        );
        let states = BTreeMap::from([
            (
                "api".to_owned(),
                StepState::failed(Some("worker".into()), "check test failed".into()),
            ),
            (
                "docs".to_owned(),
                StepState::Skipped {
                    reason: "step api did not succeed".into(),
                },
            ),
        ]);
        let mut record = RunRecord::new("boss", "id", text, &plan, &states, 0);
        record.id = "r3".into();
        let tracked = tracking.finish(&record).await;
        assert!(tracked.problems.is_empty(), "{:?}", tracked.problems);
        // Both stay open, back to do; #12 was closed and stays so (it never ran).
        assert!(fake.open(11) && fake.status(11).as_deref() == Some("Todo"));
        assert_eq!(fake.status(12).as_deref(), Some("Todo"));
        let comments = fake.calls("addComment");
        assert!(
            comments[0]["body"]
                .as_str()
                .unwrap()
                .starts_with("❌ Step `api` of run `r3` failed on **worker**")
        );
        assert!(
            comments[1]["body"]
                .as_str()
                .unwrap()
                .starts_with("⏭️ Step `docs` of run `r3` was skipped")
        );
        // The run's issue goes back to where it was, and isn't left for settling.
        assert!(fake.open(10) && fake.status(10).as_deref() == Some("Todo"));
    }

    #[test]
    fn projects_refs_and_statuses_are_read_strictly() {
        let gh = "github.com";
        let p = |s: &str| parse_project(s, gh).map(|p| (p.owner, p.number));
        assert_eq!(
            p("https://github.com/users/mcbelton/projects/4").unwrap(),
            ("mcbelton".into(), 4)
        );
        assert_eq!(
            p("https://github.com/orgs/acme/projects/7/views/2").unwrap(),
            ("acme".into(), 7)
        );
        assert_eq!(p("acme/7").unwrap(), ("acme".into(), 7));
        for bad in [
            "https://github.com/users/x/projects/0",
            "acme",
            "acme/x",
            "a b/3",
            "https://github.com/acme/app/issues/3",
        ] {
            assert!(p(bad).is_err(), "{bad}");
        }
        assert!(valid_ref("mcbelton/proofkin#24"));
        for bad in [
            "#24",
            "proofkin#24",
            "a/b#0",
            "a/b#x",
            "a/b#24\nRefs: x",
            "a/.b#1",
            "a/b c#1",
        ] {
            assert!(!valid_ref(bad), "{bad}");
        }
        assert_eq!(
            closing_lines(
                &["acme/app#3".into(), "other/lib#9".into(), "bad".into()],
                "Acme/App"
            ),
            "Closes #3\nCloses other/lib#9\n"
        );
        let field = SelectField {
            id: "f".into(),
            name: "Status".into(),
            options: vec![
                ("1".into(), "📋 Backlog".into()),
                ("2".into(), "In Progress".into()),
                ("3".into(), "✅ Done".into()),
            ],
        };
        assert_eq!(field.option(Status::Todo), Some("1"));
        // No review column: work waiting for review stays in progress.
        assert_eq!(field.option(Status::InReview), Some("2"));
        assert_eq!(field.option(Status::Done), Some("3"));
        assert_eq!(fenced("a ```b``` c"), "````text\na ```b``` c\n````");
    }
}
