//! Releases (docs/agent-factory.md, section 8; ADR-0004): a build whose evaluation passed
//! the gate, with a semantic version and a manifest of its files' hashes signed with the
//! owner's identity key, accepted by the approvers its blueprint names.
//!
//! ```text
//! factory/agents/<name>/releases/<version>/
//!   release.json        the signed manifest, the release's state and its signed approvals
//!   files/              the build's files: agent.md, blueprint.json, evals.json, skills/, team.plan
//!   fixtures/           the files its eval cases work on (its canary runs its smoke cases on them)
//!   report.json         the evaluation that passed, as its evaluating agent signed it
//! ```
//!
//! A release is immutable: its manifest lists the SHA-256 of every other file in its folder,
//! so a changed, added or removed byte fails [`verify`], and a release that doesn't verify
//! can't be deployed or used in plans. Only its state and approvals change, and each approval
//! is an answer signed by its approver.
//!
//! Versions are never reused. The default is the next patch; new requirements or more
//! capabilities than the active release make it minor, and a changed or removed requirement
//! (the agent's output contract) major. The owner may choose a higher version, never a lower
//! one.

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::str::FromStr;

use anyhow::{Context, Result, anyhow, bail};
use iroh::{EndpointId, SecretKey, Signature};
use serde::{Deserialize, Serialize};

use crate::evidence::{Evidence, EvidenceKind, sha256_hex};
use crate::factory::blueprint::{Blueprint, Capabilities};
use crate::factory::build::{AGENT_FILE, BLUEPRINT_FILE, Build};
use crate::factory::report::{EvalState, Evaluation};
use crate::factory::routing::Route;
use crate::factory::store::{
    AgentState, FactoryStore, Lifecycle, Provenance, now, read_json, validate_agent_name,
    write_json,
};
use crate::factory::{capabilities, conflict, invalid, not_found, runner};

/// The release's own file: its manifest, signature, state and approvals.
pub const RELEASE_FILE: &str = "release.json";
/// The evaluation that passed, as its evaluating agent signed it.
pub const REPORT_FILE: &str = "report.json";
/// Where the build's files are in a release.
pub const FILES_DIR: &str = "files";
/// Where its cases' fixtures are.
pub const FIXTURES_DIR: &str = "fixtures";
pub const SCHEMA: &str = "proofkin.agent-release/v1";
/// What a manifest's signature covers starts with this, so it can't pass for anything else.
const DOMAIN: &[u8] = b"proofkin-agent-release-v1\n";
/// The approver that is the agent's owner.
pub const OWNER: &str = "me";
/// An agent's first version.
pub const FIRST: Version = Version {
    major: 1,
    minor: 0,
    patch: 0,
};

/// A semantic version: `MAJOR.MINOR.PATCH`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl FromStr for Version {
    type Err = anyhow::Error;

    fn from_str(text: &str) -> Result<Self> {
        let parts: Vec<&str> = text.split('.').collect();
        let number = |p: &str| {
            (!p.is_empty()
                && p.len() <= 9
                && p.bytes().all(|b| b.is_ascii_digit())
                && (p == "0" || !p.starts_with('0')))
            .then(|| p.parse::<u64>().ok())
            .flatten()
        };
        match parts[..] {
            [major, minor, patch] => match (number(major), number(minor), number(patch)) {
                (Some(major), Some(minor), Some(patch)) => Ok(Version {
                    major,
                    minor,
                    patch,
                }),
                _ => bail!("{text:?} isn't a semantic version (MAJOR.MINOR.PATCH, e.g. 1.2.0)"),
            },
            _ => bail!("{text:?} isn't a semantic version (MAJOR.MINOR.PATCH, e.g. 1.2.0)"),
        }
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

impl Version {
    /// The next version after this one for a change of kind `bump`.
    pub fn bumped(self, bump: Bump) -> Self {
        match bump {
            Bump::Patch => Version {
                patch: self.patch + 1,
                ..self
            },
            Bump::Minor => Version {
                minor: self.minor + 1,
                patch: 0,
                ..self
            },
            Bump::Major => Version {
                major: self.major + 1,
                minor: 0,
                patch: 0,
            },
        }
    }
}

/// What kind of change a release makes, which decides its version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Bump {
    Patch,
    Minor,
    Major,
}

impl fmt::Display for Bump {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Bump::Patch => "patch",
            Bump::Minor => "minor",
            Bump::Major => "major",
        })
    }
}

/// What `after` changes from `before` (the active release's blueprint; none for a first
/// release), with `increases` its capability increases: the kind of change, and why.
pub fn bump_of(
    before: Option<&Blueprint>,
    after: &Blueprint,
    increases: &[String],
) -> (Bump, String) {
    let Some(before) = before else {
        return (Bump::Major, "its first release".into());
    };
    let changed: Vec<String> = before
        .requirements
        .iter()
        .filter_map(
            |old| match after.requirements.iter().find(|r| r.id == old.id) {
                None => Some(format!("{} removed", old.id)),
                Some(new) if one_line(&new.text) != one_line(&old.text) => {
                    Some(format!("{} changed", old.id))
                }
                Some(_) => None,
            },
        )
        .collect();
    if !changed.is_empty() {
        return (
            Bump::Major,
            format!("its output contract changed: {}", changed.join(", ")),
        );
    }
    let added: Vec<&str> = after
        .requirements
        .iter()
        .filter(|r| !before.requirements.iter().any(|old| old.id == r.id))
        .map(|r| r.id.as_str())
        .collect();
    let mut why = Vec::new();
    if !added.is_empty() {
        why.push(format!("new requirements: {}", added.join(", ")));
    }
    if !increases.is_empty() {
        why.push(format!("more capabilities: {}", increases.join("; ")));
    }
    if why.is_empty() {
        (Bump::Patch, "no new requirement or capability".into())
    } else {
        (Bump::Minor, why.join("; "))
    }
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The model a release runs on: the candidate its evaluation recommended, and every
/// candidate that passed (its deployments may run on those, and no other).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelPin {
    pub chosen: Route,
    pub passing: Vec<Route>,
}

/// Where a release came from.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Origin {
    /// How its draft was first written (by a model, from a template or a file).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draft: Option<Provenance>,
    /// For an agent kept as code: the folder it was released from, and its commit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
}

/// What a release is, signed by its owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub schema: String,
    pub agent: String,
    pub version: String,
    pub title: String,
    pub purpose: String,
    /// The build's hash (see [`crate::factory::build`]).
    pub build: String,
    /// The evaluation that passed the gate for exactly that build.
    pub evaluation: String,
    /// Every other file in the release's folder, by path, with its SHA-256.
    pub files: BTreeMap<String, String>,
    pub model: ModelPin,
    pub capabilities: Capabilities,
    /// What it asks for beyond the release active when it was requested (beyond nothing,
    /// for a first release): the owner signs these off, whoever else approves.
    pub increases: Vec<String>,
    /// The release active when it was requested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous: Option<String>,
    /// The kind of change it makes, and why: what decided its version.
    pub change: Bump,
    pub change_why: String,
    /// Who must accept it: `me`, or peers on the owner's allowlist.
    pub approvers: Vec<String>,
    pub origin: Origin,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub created: String,
    /// Who released it, and their identity key (the endpoint id it signs with).
    pub signer: String,
    pub signer_id: String,
}

impl Manifest {
    fn signed_bytes(&self) -> Vec<u8> {
        let mut bytes = DOMAIN.to_vec();
        bytes.extend(serde_json::to_vec(self).expect("a manifest serializes"));
        bytes
    }

    /// The manifest's SHA-256: what a release is known by.
    pub fn sha256(&self) -> String {
        sha256_hex(&self.signed_bytes())
    }
}

/// Where a release is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReleaseState {
    PendingApproval,
    Released,
    Rejected,
}

impl fmt::Display for ReleaseState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(match self {
            ReleaseState::PendingApproval => "pending-approval",
            ReleaseState::Released => "released",
            ReleaseState::Rejected => "rejected",
        })
    }
}

/// What an approval accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Cover {
    /// The release.
    Release,
    /// Its capability increases.
    Capabilities,
}

/// An approver's answer to a sign-off request, as they signed it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Approval {
    pub approver: String,
    /// The key the answer is signed with (the owner's, for `me`).
    pub approver_id: String,
    pub approved: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
    pub at: String,
    pub covers: Vec<Cover>,
    /// The answer, as its evidence vouches for it ("Approved by carol: ...").
    pub output: String,
    pub evidence: Evidence,
}

/// A release: its signed manifest, and where it is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Release {
    pub manifest: Manifest,
    pub manifest_sha256: String,
    /// The owner's signature (hex) over the manifest.
    pub signature: String,
    pub state: ReleaseState,
    #[serde(default)]
    pub approvals: Vec<Approval>,
    /// When it was released or rejected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided: Option<String>,
    /// Why it was rejected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
}

/// Someone whose answer a pending release waits for, and what they are asked to accept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ask {
    pub approver: String,
    pub covers: Vec<Cover>,
}

impl Release {
    pub fn version(&self) -> Version {
        self.manifest
            .version
            .parse()
            .expect("a kept release's version was checked")
    }

    /// Checks the manifest's hash and its owner's signature.
    pub fn verify_signature(&self) -> Result<()> {
        if self.manifest.schema != SCHEMA {
            bail!("it isn't a {SCHEMA} release");
        }
        if self.manifest.sha256() != self.manifest_sha256 {
            bail!("its manifest doesn't match its hash: it was changed after it was signed");
        }
        let signer: EndpointId = self
            .manifest
            .signer_id
            .parse()
            .map_err(|_| anyhow!("its signer id is malformed"))?;
        let bytes: [u8; 64] = data_encoding::HEXLOWER
            .decode(self.signature.as_bytes())
            .ok()
            .and_then(|b| b.try_into().ok())
            .ok_or_else(|| anyhow!("its signature is malformed"))?;
        signer
            .verify(
                &self.manifest.signed_bytes(),
                &Signature::from_bytes(&bytes),
            )
            .map_err(|_| {
                anyhow!("its signature doesn't verify: it was changed after it was signed")
            })
    }

    /// Whose answers it waits for now. The other approvers answer first; then the owner,
    /// as an approver and for the capability increases, after every other approver
    /// accepted it.
    pub fn pending(&self) -> Vec<Ask> {
        if self.state != ReleaseState::PendingApproval {
            return Vec::new();
        }
        let accepted = |who: &str, cover: Cover| {
            self.approvals
                .iter()
                .any(|a| a.approver == who && a.approved && a.covers.contains(&cover))
        };
        let others: Vec<Ask> = self
            .manifest
            .approvers
            .iter()
            .filter(|a| *a != OWNER && !accepted(a, Cover::Release))
            .map(|a| Ask {
                approver: a.clone(),
                covers: vec![Cover::Release],
            })
            .collect();
        if !others.is_empty() {
            return others;
        }
        let mut covers = Vec::new();
        if self.manifest.approvers.iter().any(|a| a == OWNER) && !accepted(OWNER, Cover::Release) {
            covers.push(Cover::Release);
        }
        if !self.manifest.increases.is_empty() && !accepted(OWNER, Cover::Capabilities) {
            covers.push(Cover::Capabilities);
        }
        if covers.is_empty() {
            Vec::new()
        } else {
            vec![Ask {
                approver: OWNER.into(),
                covers,
            }]
        }
    }

    /// Its state from its approvals: rejected by any no, released once nobody is left to
    /// answer.
    fn decide(&mut self) {
        if self.state != ReleaseState::PendingApproval {
            return;
        }
        if let Some(no) = self.approvals.iter().find(|a| !a.approved) {
            self.state = ReleaseState::Rejected;
            self.decided = Some(now());
            self.why = Some(format!(
                "{} {}{}",
                no.approver,
                if no.output.starts_with(crate::agent::CHANGES_ASKED) {
                    "asked for changes"
                } else {
                    "rejected it"
                },
                if no.note.is_empty() {
                    String::new()
                } else {
                    format!(": {}", no.note)
                }
            ));
        } else if self.pending().is_empty() {
            self.state = ReleaseState::Released;
            self.decided = Some(now());
        }
    }
}

/// The title and text of the sign-off request `ask` gets for `release`, whose evaluation is
/// `report`: what it is, its eval summary, its capability diff, its requirements and where it
/// came from. The same release gives the same text, which its approval's evidence hashes.
pub fn request_text(release: &Release, report: &Evaluation, ask: &Ask) -> (String, String) {
    let m = &release.manifest;
    let title = match (
        ask.covers.contains(&Cover::Release),
        ask.covers.contains(&Cover::Capabilities),
    ) {
        (true, true) => format!(
            "Release agent {} {}, with more capabilities?",
            m.agent, m.version
        ),
        (false, true) => format!(
            "Sign off agent {} {}'s new capabilities?",
            m.agent, m.version
        ),
        _ => format!("Release agent {} {}?", m.agent, m.version),
    };
    let mut text = format!(
        "{} asks to release agent {} {} ({}): {}\n{}\n\n",
        m.signer, m.agent, m.version, m.title, m.purpose, m.change_why
    );
    text += &format!(
        "Version: {} ({}){}\n",
        m.version,
        m.change,
        m.previous
            .as_deref()
            .map(|p| format!(", after {p}"))
            .unwrap_or_default()
    );
    text += &format!("Evaluation: {}\n", eval_summary(report));
    text += &format!("Model: {}\n", m.model.chosen);
    text += &format!("Asks for: {}\n", capabilities_text(&m.capabilities));
    text += &format!(
        "More than {}: {}\n",
        m.previous
            .as_deref()
            .map(|p| format!("release {p}"))
            .unwrap_or_else(|| "nothing (its first release)".into()),
        if m.increases.is_empty() {
            "nothing".to_owned()
        } else {
            m.increases.join("; ")
        }
    );
    text += &format!("Written: {}\n", origin_text(&m.origin));
    if let Some(note) = &m.note {
        text += &format!("Note: {note}\n");
    }
    text += &format!(
        "Build {}, manifest {}, signed by {} ({}).\n",
        &m.build[..12.min(m.build.len())],
        &release.manifest_sha256[..12.min(release.manifest_sha256.len())],
        m.signer,
        &m.signer_id[..10.min(m.signer_id.len())]
    );
    if ask.covers.contains(&Cover::Capabilities) {
        text += "\nYou are asked to accept these capabilities too: it gets them where its folders and your grants offer them.\n";
    }
    (title, text)
}

/// An evaluation in a line: its id, whether it passed, and the recommended candidate's
/// results.
pub fn eval_summary(e: &Evaluation) -> String {
    let chosen = e
        .recommended
        .as_ref()
        .and_then(|r| e.results.iter().find(|c| &c.candidate == r))
        .or_else(|| e.results.first());
    match chosen {
        Some(c) => format!(
            "{} {}: {:.0}% of {} runs passed, adversarial {}/{}, p95 {:.0}s{}",
            e.id,
            e.state,
            c.pass_rate * 100.0,
            c.runs.total,
            c.adversarial.passed,
            c.adversarial.total,
            c.p95_secs,
            c.judge_score
                .map(|s| format!(", judge {s:.1}"))
                .unwrap_or_default()
        ),
        None => format!("{} {}", e.id, e.state),
    }
}

/// What `caps` asks for, in a line.
pub fn capabilities_text(caps: &Capabilities) -> String {
    let mut parts = vec![format!("{} access", caps.access)];
    if !caps.mcp.is_empty() {
        let servers: Vec<String> = caps
            .mcp
            .iter()
            .map(|m| format!("{} ({})", m.server, m.tools.join(", ")))
            .collect();
        parts.push(format!("MCP servers {}", servers.join(", ")));
    }
    if !caps.checks.is_empty() {
        parts.push(format!("checks {}", caps.checks.join(", ")));
    }
    if !caps.workiq.is_empty() {
        parts.push(format!("Microsoft 365 {}", caps.workiq.join(", ")));
    }
    parts.push(if caps.delegate {
        "delegation".into()
    } else {
        "no delegation".into()
    });
    parts.join("; ")
}

fn origin_text(origin: &Origin) -> String {
    let mut text = match &origin.draft {
        Some(p) => match p.source.as_str() {
            "model" => format!(
                "drafted by {}'s model{}",
                p.provider.map(|x| x.to_string()).unwrap_or_default(),
                p.model
                    .as_deref()
                    .map(|m| format!(" ({m})"))
                    .unwrap_or_default()
            ),
            "template" => format!(
                "from template {}",
                p.template.as_deref().unwrap_or("unknown")
            ),
            other => format!("from {other}"),
        },
        None => "as code".into(),
    };
    if let Some(commit) = &origin.commit {
        text += &format!(
            ", released from commit {}{}",
            &commit[..12.min(commit.len())],
            origin
                .dir
                .as_deref()
                .map(|d| format!(" of {d}"))
                .unwrap_or_default()
        );
    }
    text
}

/// A release asked for.
#[derive(Debug, Clone, Default)]
pub struct Asked {
    /// The evaluation that passed for exactly the build to release.
    pub evaluation: String,
    /// The version to give it (default: what its change needs, see [`bump_of`]).
    pub version: Option<String>,
    pub note: Option<String>,
    /// Who asked: the owner's profile, or another agent of theirs.
    pub by: String,
    /// For an agent kept as code: the folder and commit it is released from.
    pub dir: Option<String>,
    pub commit: Option<String>,
}

/// The most a release note says.
pub const MAX_NOTE: usize = 1000;

/// Releases agent `name` of `store`'s owner (`owner`, whose identity is `key`) as `asked`
/// says: from an evaluation that passed the gate for exactly its build. It is signed and
/// kept, and waits for its sign-offs; with no approver and no capability increase it is
/// released, and active, at once. Refusals say why ([`crate::factory::Refusal`]).
pub fn request(
    store: &FactoryStore,
    owner: &str,
    key: &SecretKey,
    name: &str,
    asked: &Asked,
) -> Result<Release> {
    validate_agent_name(name).map_err(|e| invalid(format!("{e:#}")))?;
    let state = store
        .state(name)?
        .ok_or_else(|| not_found(format!("the factory has no agent '{name}'")))?;
    if state.state == Lifecycle::Retired {
        return Err(conflict(format!(
            "{name} is retired: a retired agent isn't released again"
        )));
    }
    if asked
        .note
        .as_deref()
        .is_some_and(|n| n.chars().count() > MAX_NOTE)
    {
        return Err(invalid(format!(
            "a release note is at most {MAX_NOTE} characters"
        )));
    }
    let evaluation = store
        .evaluation(&asked.evaluation)
        .map_err(|e| invalid(format!("{e:#}")))?
        .ok_or_else(|| not_found(format!("there is no evaluation {}", asked.evaluation)))?;
    if evaluation.agent != name {
        return Err(invalid(format!(
            "evaluation {} is of {}, not {name}",
            evaluation.id, evaluation.agent
        )));
    }
    if let Some(of) = &evaluation.canary {
        return Err(conflict(format!(
            "evaluation {} is the canary of {name} {} on {}: its smoke cases only, without the built-in attacks; release from a full evaluation",
            evaluation.id, of.version, of.worker
        )));
    }
    if evaluation.state != EvalState::Passed {
        let why = if evaluation.gate.reasons.is_empty() {
            format!("it is {}", evaluation.state)
        } else {
            evaluation.gate.reasons.join("; ")
        };
        return Err(conflict(format!(
            "evaluation {} didn't pass its gate: {why}",
            evaluation.id
        )));
    }
    evaluation.verify().map_err(|e| {
        conflict(format!(
            "evaluation {} doesn't verify: {e:#}",
            evaluation.id
        ))
    })?;
    let build = store.build(name, &evaluation.build)?.ok_or_else(|| {
        conflict(format!(
            "build {} of {name}, which evaluation {} is of, isn't kept",
            &evaluation.build[..12.min(evaluation.build.len())],
            evaluation.id
        ))
    })?;
    let blueprint = runner::blueprint_of(&build)?;
    let chosen = evaluation
        .recommended
        .clone()
        .ok_or_else(|| conflict(format!("evaluation {} recommends no model", evaluation.id)))?;
    let passing: Vec<Route> = evaluation
        .results
        .iter()
        .filter(|r| r.gate.passed)
        .map(|r| r.candidate.clone())
        .collect();
    // The capability increases its sign-off covers are over the active release (what
    // runs); its version counts from the newest release, whatever became of it.
    let active = match &state.active_version {
        Some(v) => Some(load(store, name, v).with_context(|| {
            format!(
                "its active release {v} doesn't verify: roll back to one that does, or verify it"
            )
        })?),
        None => None,
    };
    let active_bp = match &active {
        Some(release) => Some(blueprint_of(store, name, &release.manifest.version)?),
        None => None,
    };
    let increases = capabilities::increases(
        &active_bp
            .clone()
            .unwrap_or_else(|| nothing_like(&blueprint)),
        &blueprint,
    );
    let mut used: Vec<Version> = store
        .release_versions(name)?
        .iter()
        .filter_map(|v| v.parse().ok())
        .collect();
    used.sort();
    let (change, change_why, needed) = match used.last() {
        None => (Bump::Major, "its first release".to_owned(), FIRST),
        Some(newest) => {
            let base = blueprint_of(store, name, &newest.to_string())?;
            let (change, why) = bump_of(
                Some(&base),
                &blueprint,
                &capabilities::increases(&base, &blueprint),
            );
            (change, why, newest.bumped(change))
        }
    };
    let version = match &asked.version {
        None => needed,
        Some(text) => {
            let chosen: Version = text.parse().map_err(|e| invalid(format!("{e:#}")))?;
            if used.contains(&chosen) {
                return Err(invalid(format!("{chosen} is already a release of {name}")));
            }
            if chosen < needed {
                return Err(invalid(format!(
                    "{chosen} is lower than {needed}, the version this release needs ({change}: {change_why})"
                )));
            }
            chosen
        }
    };
    // The release's files: the build's, its cases' fixtures, and the report.
    let mut files: BTreeMap<String, Vec<u8>> = build
        .files
        .iter()
        .map(|(path, text)| (format!("{FILES_DIR}/{path}"), text.as_bytes().to_vec()))
        .collect();
    let fixtures = store.fixtures_dir(name);
    for fixture in blueprint.evals.cases.iter().flat_map(|c| &c.fixtures) {
        let path = fixtures.join(fixture);
        if path.is_file() {
            let bytes =
                std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
            files.insert(format!("{FIXTURES_DIR}/{fixture}"), bytes);
        }
    }
    files.insert(
        REPORT_FILE.into(),
        (serde_json::to_string_pretty(&evaluation)? + "\n").into_bytes(),
    );
    let manifest = Manifest {
        schema: SCHEMA.into(),
        agent: name.into(),
        version: version.to_string(),
        title: blueprint.title.clone(),
        purpose: blueprint.purpose.clone(),
        build: build.hash.clone(),
        evaluation: evaluation.id.clone(),
        files: files
            .iter()
            .map(|(path, bytes)| (path.clone(), sha256_hex(bytes)))
            .collect(),
        model: ModelPin { chosen, passing },
        capabilities: blueprint.capabilities.clone(),
        increases,
        previous: state.active_version.clone(),
        change,
        change_why,
        approvers: blueprint.approvers.clone(),
        origin: Origin {
            draft: store.draft(name)?.map(|d| d.provenance),
            dir: asked.dir.clone(),
            commit: asked.commit.clone(),
        },
        note: asked.note.clone().filter(|n| !n.trim().is_empty()),
        created: now(),
        signer: owner.into(),
        signer_id: key.public().to_string(),
    };
    let signature = key.sign(&manifest.signed_bytes());
    let mut release = Release {
        manifest_sha256: manifest.sha256(),
        manifest,
        signature: data_encoding::HEXLOWER.encode(&signature.to_bytes()),
        state: ReleaseState::PendingApproval,
        approvals: Vec::new(),
        decided: None,
        why: None,
    };
    release.decide();
    files.insert(
        RELEASE_FILE.into(),
        (serde_json::to_string_pretty(&release)? + "\n").into_bytes(),
    );
    store
        .save_release(name, &release.manifest.version, &files)
        .map_err(|e| invalid(format!("{e:#}")))?;
    let version = release.manifest.version.clone();
    let waiting = release
        .pending()
        .iter()
        .map(|a| a.approver.clone())
        .collect::<Vec<_>>();
    store.update_state(name, |s| {
        s.versions.push(version.clone());
        sort_versions(&mut s.versions);
        let what = match release.state {
            ReleaseState::Released => format!("released {version}"),
            _ => format!(
                "asked to release {version}, waiting for {}",
                waiting.join(", ")
            ),
        };
        s.changed(&asked.by, what, release.manifest.note.as_deref());
        if release.state == ReleaseState::Released {
            activate(s, &version);
        }
        Ok(())
    })?;
    Ok(release)
}

/// A blueprint like `bp` that asks for nothing: what a first release's increases are
/// counted from.
fn nothing_like(bp: &Blueprint) -> Blueprint {
    let mut nothing = bp.clone();
    nothing.capabilities = Capabilities {
        access: crate::llm::Access::ReadOnly,
        mcp: Vec::new(),
        checks: Vec::new(),
        workiq: Vec::new(),
        delegate: false,
    };
    nothing.team = None;
    nothing
}

/// Versions newest first; ones that aren't versions last.
pub fn sort_versions(versions: &mut Vec<String>) {
    versions.sort_by(|a, b| match (a.parse::<Version>(), b.parse::<Version>()) {
        (Ok(a), Ok(b)) => b.cmp(&a),
        (Ok(_), Err(_)) => std::cmp::Ordering::Less,
        (Err(_), Ok(_)) => std::cmp::Ordering::Greater,
        (Err(_), Err(_)) => a.cmp(b),
    });
    versions.dedup();
}

/// Makes `version` the active release, unless a later one is active already: an earlier
/// release accepted late doesn't roll the agent back.
fn activate(state: &mut AgentState, version: &str) {
    let newer = state
        .active_version
        .as_deref()
        .and_then(|v| v.parse::<Version>().ok())
        .zip(version.parse::<Version>().ok())
        .is_some_and(|(active, new)| active > new);
    if !newer {
        state.active_version = Some(version.to_owned());
    }
    if matches!(state.state, Lifecycle::Draft | Lifecycle::Imported) {
        state.state = Lifecycle::Active;
    }
}

/// Release `version` of agent `name`, verified (see [`verify_dir`]): a release that fails
/// can't be used. An unknown version is a refusal naming the versions there are.
pub fn load(store: &FactoryStore, name: &str, version: &str) -> Result<Release> {
    let dir = store.release_dir(name, version);
    if version.parse::<Version>().is_err() || !dir.join(RELEASE_FILE).is_file() {
        let mut versions = store.release_versions(name).unwrap_or_default();
        sort_versions(&mut versions);
        return Err(not_found(if versions.is_empty() {
            format!("{name} has no release {version} (it has none)")
        } else {
            format!(
                "{name} has no release {version} (it has {})",
                versions.join(", ")
            )
        }));
    }
    verify_dir(&dir, name, version)
        .with_context(|| format!("release {version} of {name} doesn't verify"))
}

/// Checks the release in `dir` offline: its manifest's hash and signature, every file
/// against its hash (and nothing more in its folder), the build's hash, its report
/// (signed, of exactly that build, passed), and each approval: signed by its approver for
/// the request it answers, the owner's with the owner's key. A released release must have
/// every answer it needed.
pub fn verify_dir(dir: &Path, name: &str, version: &str) -> Result<Release> {
    let release: Release = read_json(&dir.join(RELEASE_FILE))?
        .ok_or_else(|| anyhow!("{} is missing", RELEASE_FILE))?;
    let m = &release.manifest;
    release.verify_signature()?;
    if m.agent != name || m.version != version {
        bail!(
            "its manifest is of {} {}, not {name} {version}",
            m.agent,
            m.version
        );
    }
    let mut files = BTreeMap::new();
    read_bytes(dir, "", &mut files)?;
    files.remove(RELEASE_FILE);
    for (path, hash) in &m.files {
        match files.get(path) {
            None => bail!("{path} is missing"),
            Some(bytes) if sha256_hex(bytes) != *hash => {
                bail!("{path} doesn't match its hash: it was changed after it was released")
            }
            Some(_) => {}
        }
    }
    if let Some(extra) = files.keys().find(|p| !m.files.contains_key(*p)) {
        bail!("{extra} isn't in its manifest: it was added after it was released");
    }
    let built: BTreeMap<String, String> = files
        .iter()
        .filter_map(|(path, bytes)| {
            let path = path.strip_prefix(&format!("{FILES_DIR}/"))?;
            Some((path.to_owned(), String::from_utf8(bytes.clone()).ok()?))
        })
        .collect();
    if Build::of(name, built).hash != m.build {
        bail!(
            "its files aren't build {}",
            &m.build[..12.min(m.build.len())]
        );
    }
    let report: Evaluation = serde_json::from_slice(
        files
            .get(REPORT_FILE)
            .ok_or_else(|| anyhow!("{REPORT_FILE} is missing"))?,
    )
    .with_context(|| format!("{REPORT_FILE} isn't an evaluation"))?;
    if report.id != m.evaluation || report.build != m.build || report.state != EvalState::Passed {
        bail!(
            "its report isn't evaluation {} passing build {}",
            m.evaluation,
            &m.build[..12.min(m.build.len())]
        );
    }
    report
        .verify()
        .with_context(|| format!("its report {} doesn't verify", report.id))?;
    for approval in &release.approvals {
        check_approval(&release, &report, approval)
            .with_context(|| format!("{}'s answer", approval.approver))?;
    }
    if release.state == ReleaseState::Released {
        let mut replay = release.clone();
        replay.state = ReleaseState::PendingApproval;
        replay.decided = None;
        if let Some(ask) = replay.pending().first() {
            bail!(
                "it says it is released, but {} didn't accept it",
                ask.approver
            );
        }
        if replay.approvals.iter().any(|a| !a.approved) {
            bail!("it says it is released, but an approver didn't accept it");
        }
    }
    Ok(release)
}

/// Checks `approval` of `release`: signed by its approver (the owner's key for `me`), for
/// exactly the request its approver got, saying what it says.
fn check_approval(release: &Release, report: &Evaluation, approval: &Approval) -> Result<()> {
    let evidence = &approval.evidence;
    if evidence.kind != EvidenceKind::Approval {
        bail!("its evidence isn't an approval");
    }
    if approval.approver == OWNER && approval.approver_id != release.manifest.signer_id {
        bail!("the owner's answer isn't signed with the owner's key");
    }
    if evidence.signer_id != approval.approver_id {
        bail!("it isn't signed by {}", approval.approver);
    }
    evidence
        .verify_self(&approval.output)
        .map_err(|e| anyhow!("its signature doesn't verify: {e:#}"))?;
    if evidence.approved != Some(approval.approved) {
        bail!("its evidence says otherwise");
    }
    let ask = Ask {
        approver: approval.approver.clone(),
        covers: approval.covers.clone(),
    };
    let (_, text) = request_text(release, report, &ask);
    if evidence.prompt_sha256 != sha256_hex(text.as_bytes()) {
        bail!("it answers another request");
    }
    Ok(())
}

/// The files under `dir`, by their `/`-separated paths below it.
fn read_bytes(dir: &Path, prefix: &str, files: &mut BTreeMap<String, Vec<u8>>) -> Result<()> {
    for entry in crate::factory::store::read_dir(dir)? {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        let relative = format!("{prefix}{name}");
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            bail!("{relative} is a link: a release has only files");
        }
        if kind.is_dir() {
            read_bytes(&path, &format!("{relative}/"), files)?;
        } else {
            files.insert(
                relative,
                std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?,
            );
        }
    }
    Ok(())
}

/// The build release `version` of `name` is, from its verified files.
pub fn build_of(store: &FactoryStore, name: &str, version: &str) -> Result<Build> {
    load(store, name, version)?;
    let mut files = BTreeMap::new();
    crate::factory::store::read_tree(
        &store.release_dir(name, version).join(FILES_DIR),
        "",
        &mut files,
    )?;
    Ok(Build::of(name, files))
}

/// The blueprint of release `version` of `name`.
pub fn blueprint_of(store: &FactoryStore, name: &str, version: &str) -> Result<Blueprint> {
    let text = std::fs::read_to_string(
        store
            .release_dir(name, version)
            .join(FILES_DIR)
            .join(BLUEPRINT_FILE),
    )?;
    crate::factory::blueprint::parse(&text).map_err(|p| anyhow!("{}", p.message))
}

/// The evaluation report release `version` of `name` was made from, as kept in it.
pub fn report_of(store: &FactoryStore, name: &str, version: &str) -> Result<Evaluation> {
    let text = std::fs::read_to_string(store.release_dir(name, version).join(REPORT_FILE))?;
    Ok(serde_json::from_str(&text)?)
}

/// The agent file and the instructions of its skills, as a task gets them, of release
/// `version` of `name` (verified).
pub fn instructions_of(store: &FactoryStore, name: &str, version: &str) -> Result<String> {
    let build = build_of(store, name, version)?;
    instructions(&build, version)
}

/// What a task acting as `build`, released as `version`, is told first: its agent file's
/// body and its skills', under the agent's heading.
pub fn instructions(build: &Build, version: &str) -> Result<String> {
    Ok(format!(
        "\n### Agent: {} {version}\n{}\n",
        build.agent,
        body(build)?
    ))
}

/// A build's agent file's instructions, then each of its skills' under its heading.
pub fn body(build: &Build) -> Result<String> {
    let file = build
        .files
        .get(AGENT_FILE)
        .ok_or_else(|| anyhow!("its build has no {AGENT_FILE}"))?;
    let agent = crate::skills::parse_agent(file, &build.agent)?;
    let mut text = agent.instructions.clone();
    for (path, skill) in &build.files {
        if let Some(skill_name) = path
            .strip_prefix("skills/")
            .and_then(|p| p.strip_suffix(".md"))
        {
            let (_, body) = crate::skills::front_matter(skill);
            text += &format!("\n\n### Skill: {skill_name}\n{}", body.trim());
        }
    }
    Ok(text)
}

/// Every release of agent `name`, newest first: each one verified, or why it doesn't.
pub fn releases(store: &FactoryStore, name: &str) -> Result<Vec<(String, Result<Release>)>> {
    let mut versions = store.release_versions(name)?;
    sort_versions(&mut versions);
    Ok(versions
        .into_iter()
        .map(|v| {
            let release = load(store, name, &v);
            (v, release)
        })
        .collect())
}

/// The release of `name` waiting for sign-offs: `version`, or the only one there is.
pub fn pending_release(store: &FactoryStore, name: &str, version: Option<&str>) -> Result<Release> {
    if let Some(version) = version {
        let release = load(store, name, version)?;
        if release.state != ReleaseState::PendingApproval {
            return Err(conflict(format!(
                "release {version} of {name} is {}: it waits for no answer",
                release.state
            )));
        }
        return Ok(release);
    }
    let pending: Vec<Release> = releases(store, name)?
        .into_iter()
        .filter_map(|(_, r)| r.ok())
        .filter(|r| r.state == ReleaseState::PendingApproval)
        .collect();
    match pending.len() {
        0 => Err(not_found(format!(
            "no release of {name} waits for a sign-off"
        ))),
        1 => Ok(pending.into_iter().next().expect("one")),
        _ => Err(invalid(format!(
            "{} releases of {name} wait for sign-offs ({}): say which",
            pending.len(),
            pending
                .iter()
                .map(|r| r.manifest.version.clone())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

/// The owner's own answer to `release`, signed with their identity `key`, as their agent
/// signs an approval: for the command line, where no agent asks them.
pub fn own_answer(
    owner: &str,
    key: &SecretKey,
    release: &Release,
    report: &Evaluation,
    approved: bool,
    note: &str,
) -> Result<Approval> {
    let ask = release
        .pending()
        .into_iter()
        .find(|a| a.approver == OWNER)
        .ok_or_else(|| {
            let waiting: Vec<String> = release.pending().into_iter().map(|a| a.approver).collect();
            conflict(if waiting.is_empty() {
                format!(
                    "release {} of {} waits for no answer from you",
                    release.manifest.version, release.manifest.agent
                )
            } else {
                format!(
                    "release {} of {} waits for {} first: you sign it off last",
                    release.manifest.version,
                    release.manifest.agent,
                    waiting.join(", ")
                )
            })
        })?;
    let (_, text) = request_text(release, report, &ask);
    let note = note.trim();
    let output = format!(
        "{}{owner}{}",
        if approved {
            "Approved by "
        } else {
            "Rejected by "
        },
        if note.is_empty() {
            String::new()
        } else {
            format!(": {note}")
        }
    );
    let mut evidence = Evidence::new(EvidenceKind::Approval, owner, key.public(), key.public(), 0);
    evidence.prompt_sha256 = sha256_hex(text.as_bytes());
    evidence.approved = Some(approved);
    Ok(Approval {
        approver: OWNER.into(),
        approver_id: key.public().to_string(),
        approved,
        note: note.to_owned(),
        at: now(),
        covers: ask.covers,
        evidence: evidence.sign(&output, key),
        output,
    })
}

/// The approval `ask`'s approver gave through their agent: its signed answer `output` and
/// `evidence` (see [`crate::agent::Agent::answer`]).
pub fn approval_of(
    ask: &Ask,
    approver_id: EndpointId,
    output: &str,
    evidence: Evidence,
) -> Approval {
    let approved = evidence.approved == Some(true);
    let note = output
        .split_once(": ")
        .map(|(_, note)| note.trim().to_owned())
        .unwrap_or_default();
    Approval {
        approver: ask.approver.clone(),
        approver_id: approver_id.to_string(),
        approved,
        note,
        at: now(),
        covers: ask.covers.clone(),
        output: output.to_owned(),
        evidence,
    }
}

/// Records `approval` of release `version` of `name`, by `by`: it must answer what the
/// release waits for now, signed as [`verify_dir`] checks. The release is rejected by a no,
/// and released (and active) once nobody is left to answer.
pub fn answer(
    store: &FactoryStore,
    name: &str,
    version: &str,
    approval: Approval,
) -> Result<Release> {
    let dir = store.release_dir(name, version);
    // The lock is next to the release's folder: everything in it is in its manifest.
    let _lock = crate::store::FileLock::acquire(
        &store.releases_dir(name).join(format!(".{version}.answers")),
    )?;
    let mut release = load(store, name, version)?;
    let ask = release
        .pending()
        .into_iter()
        .find(|a| a.approver == approval.approver)
        .ok_or_else(|| {
            conflict(format!(
                "release {version} of {name} waits for no answer from {}",
                approval.approver
            ))
        })?;
    if ask.covers != approval.covers {
        return Err(conflict(format!(
            "{}'s answer doesn't cover what release {version} asks of them",
            approval.approver
        )));
    }
    let report = report_of(store, name, version)?;
    check_approval(&release, &report, &approval)?;
    let who = approval.approver.clone();
    let (approved, note) = (approval.approved, approval.note.clone());
    release.approvals.push(approval);
    release.decide();
    write_json(&dir.join(RELEASE_FILE), &release)?;
    let state = release.state;
    store.update_state(name, |s| {
        let what = match (state, approved) {
            (ReleaseState::Released, _) => format!("{who} accepted {version}: released"),
            (ReleaseState::Rejected, _) => format!("{who} rejected {version}"),
            _ => format!("{who} accepted {version}"),
        };
        s.changed(&who, what, (!note.is_empty()).then_some(note.as_str()));
        if state == ReleaseState::Released {
            activate(s, version);
        }
        Ok(())
    })?;
    Ok(release)
}

/// What changed from release `from` of `name` to release `to`: requirements, instructions,
/// skills, capabilities both ways, the model, budgets and files.
pub fn diff(store: &FactoryStore, name: &str, from: &str, to: &str) -> Result<Vec<String>> {
    let (a, b) = (load(store, name, from)?, load(store, name, to)?);
    let (old, new) = (
        blueprint_of(store, name, from)?,
        blueprint_of(store, name, to)?,
    );
    let mut lines = Vec::new();
    for r in &new.requirements {
        match old.requirements.iter().find(|o| o.id == r.id) {
            None => lines.push(format!("+ requirement {}: {}", r.id, r.text)),
            Some(o) if one_line(&o.text) != one_line(&r.text) => {
                lines.push(format!("~ requirement {}: {} → {}", r.id, o.text, r.text))
            }
            Some(_) => {}
        }
    }
    for o in old
        .requirements
        .iter()
        .filter(|o| !new.requirements.iter().any(|r| r.id == o.id))
    {
        lines.push(format!("- requirement {}: {}", o.id, o.text));
    }
    if one_line(&old.instructions) != one_line(&new.instructions) {
        lines.push(format!(
            "~ instructions ({} → {} lines)",
            old.instructions.lines().count(),
            new.instructions.lines().count()
        ));
    }
    for s in new.skills.iter().filter(|s| !old.skills.contains(s)) {
        lines.push(format!("+ skill {s}"));
    }
    for s in old.skills.iter().filter(|s| !new.skills.contains(s)) {
        lines.push(format!("- skill {s}"));
    }
    for more in capabilities::increases(&old, &new) {
        lines.push(format!("+ {more}"));
    }
    for fewer in capabilities::increases(&new, &old) {
        lines.push(format!("- {fewer}"));
    }
    if a.manifest.model.chosen != b.manifest.model.chosen {
        lines.push(format!(
            "~ model: {} → {}",
            a.manifest.model.chosen, b.manifest.model.chosen
        ));
    }
    if old.budgets != new.budgets {
        lines.push(format!(
            "~ budgets: {} → {}",
            budgets_text(&old.budgets),
            budgets_text(&new.budgets)
        ));
    }
    let files = |r: &Release| -> BTreeMap<String, String> {
        r.manifest
            .files
            .iter()
            .filter(|(p, _)| p.starts_with(&format!("{FILES_DIR}/")))
            .map(|(p, h)| (p.clone(), h.clone()))
            .collect()
    };
    let (fa, fb) = (files(&a), files(&b));
    let changed: Vec<String> = fb
        .iter()
        .filter(|(p, h)| fa.get(*p) != Some(*h))
        .map(|(p, _)| p.trim_start_matches("files/").to_owned())
        .chain(
            fa.keys()
                .filter(|p| !fb.contains_key(*p))
                .map(|p| format!("{} (removed)", p.trim_start_matches("files/"))),
        )
        .collect();
    if !changed.is_empty() {
        lines.push(format!("files changed: {}", changed.join(", ")));
    }
    Ok(lines)
}

/// Budgets in a line.
pub fn budgets_text(b: &crate::factory::blueprint::Budgets) -> String {
    let mut parts = Vec::new();
    if let Some(n) = b.tasks_per_day {
        parts.push(format!("{n} tasks a day"));
    }
    if let Some(n) = b.attempts {
        parts.push(format!("{n} attempts"));
    }
    if let Some(s) = b.timeout_secs {
        parts.push(format!("{} minutes a task", s.div_ceil(60)));
    }
    if parts.is_empty() {
        "none".into()
    } else {
        parts.join(", ")
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::factory::blueprint::from_value;
    use crate::factory::blueprint::tests::sample;
    use crate::factory::report::RunState;
    use crate::factory::store::Draft;
    use serde_json::{Value, json};

    /// The owner, its evaluation worker, and its factory with draft `fin-analyst`.
    pub(crate) struct Fixture {
        pub dir: tempfile::TempDir,
        pub store: FactoryStore,
        pub owner: SecretKey,
        pub worker: SecretKey,
        n: std::cell::Cell<u32>,
    }

    impl Fixture {
        pub fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let store = FactoryStore::new(dir.path().join("factory"));
            Self::with(dir, store)
        }

        /// The fixture on `store` (a profile's).
        pub fn in_store(store: FactoryStore) -> Self {
            Self::with(tempfile::tempdir().unwrap(), store)
        }

        fn with(dir: tempfile::TempDir, store: FactoryStore) -> Self {
            let draft = Draft {
                blueprint: sample(),
                provenance: Provenance {
                    source: "template".into(),
                    template: Some("financial-analyst".into()),
                    at: now(),
                    ..Provenance::default()
                },
                tries: 0,
            };
            store
                .create_draft("fin-analyst", &draft, "alice", "drafted")
                .unwrap();
            Self {
                dir,
                store,
                owner: SecretKey::generate(),
                worker: SecretKey::generate(),
                n: std::cell::Cell::new(0),
            }
        }

        /// Builds the blueprint `change` makes of the sample, and evaluates the build: every
        /// run passes unless `fail`.
        pub fn evaluated(&self, change: impl FnOnce(&mut Value), fail: bool) -> Evaluation {
            let mut value = sample();
            change(&mut value);
            let bp = from_value(&value).unwrap();
            let skills = crate::skills::Skills::new(self.dir.path().join("skills"));
            let build = crate::factory::build::build(&bp, &skills).unwrap();
            self.store.save_build(&build).unwrap();
            let mut e = crate::factory::report::tests::evaluation();
            self.n.set(self.n.get() + 1);
            e.id = format!("ev-20261124-0915{:02}-fin-analyst", self.n.get());
            e.agent = "fin-analyst".into();
            e.build = build.hash.clone();
            e.worker_id = self.worker.public().to_string();
            for (i, run) in e.runs.iter_mut().enumerate() {
                run.state = RunState::Done;
                run.passed = !fail;
                run.secs = 10.0;
                run.attempts = 1;
                run.finished = Some("2026-11-24T09:20:00Z".into());
                let output = format!("ok, run {i}");
                let mut evidence = Evidence::new(
                    EvidenceKind::Task,
                    "alice-eval",
                    self.worker.public(),
                    self.owner.public(),
                    i as u64,
                );
                evidence.attempts = 1;
                run.task_id = Some(i as u64);
                run.evidence = Some(evidence.sign(&output, &self.worker));
                run.keep_output(&output, &e.seeds);
            }
            e.finish(&[Some(0.33), Some(1.0)], None);
            e.sign("alice", &self.owner);
            self.store.save_evaluation(&e).unwrap();
            e
        }

        pub fn release(&self, evaluation: &str, version: Option<&str>) -> Result<Release> {
            request(
                &self.store,
                "alice",
                &self.owner,
                "fin-analyst",
                &Asked {
                    evaluation: evaluation.into(),
                    version: version.map(Into::into),
                    by: "alice".into(),
                    ..Asked::default()
                },
            )
        }

        /// The owner accepts what release `version` waits for from them.
        pub fn approve(&self, version: &str) -> Release {
            let release = load(&self.store, "fin-analyst", version).unwrap();
            let report = report_of(&self.store, "fin-analyst", version).unwrap();
            let yes = own_answer("alice", &self.owner, &release, &report, true, "ok").unwrap();
            answer(&self.store, "fin-analyst", version, yes).unwrap()
        }
    }

    fn approvers(list: &[&str]) -> impl FnOnce(&mut Value) {
        let list: Vec<String> = list.iter().map(|s| s.to_string()).collect();
        move |v: &mut Value| v["approvers"] = json!(list)
    }

    fn refusal(r: Result<Release>) -> (crate::factory::RefusalKind, String) {
        let e = r.unwrap_err();
        (
            crate::factory::refusal_kind(&e).expect("a refusal"),
            format!("{e:#}"),
        )
    }

    #[test]
    fn a_release_is_made_only_from_a_passed_evaluation_of_exactly_its_build_and_signed() {
        use crate::factory::RefusalKind;
        let f = Fixture::new();
        let failed = f.evaluated(approvers(&["me"]), true);
        let (kind, why) = refusal(f.release(&failed.id, None));
        assert_eq!(kind, RefusalKind::Conflict);
        assert!(why.contains("didn't pass its gate"), "{why}");
        // A deployment's canary passed its smoke cases only: no release is made from it.
        let mut canary = f.evaluated(approvers(&["me"]), false);
        canary.canary = Some(crate::factory::report::CanaryOf {
            worker: "fin-analyst".into(),
            version: "1.0.0".into(),
        });
        canary.sign("alice", &f.owner);
        f.store.save_evaluation(&canary).unwrap();
        let (kind, why) = refusal(f.release(&canary.id, None));
        assert_eq!(kind, RefusalKind::Conflict);
        assert!(why.contains("is the canary of fin-analyst 1.0.0"), "{why}");
        let (kind, _) = refusal(f.release("ev-20261124-091599-fin-analyst", None));
        assert_eq!(kind, RefusalKind::NotFound);

        let passed = f.evaluated(approvers(&["me"]), false);
        let release = f.release(&passed.id, None).unwrap();
        assert_eq!(release.manifest.version, "1.0.0");
        assert_eq!(release.state, ReleaseState::PendingApproval);
        assert_eq!(release.manifest.build, passed.build);
        assert_eq!(release.manifest.signer_id, f.owner.public().to_string());
        // Every artifact is in the manifest with its hash: the build's files and the report.
        for path in [
            "files/agent.md",
            "files/blueprint.json",
            "files/evals.json",
            "report.json",
        ] {
            assert!(release.manifest.files.contains_key(path), "{path}");
        }
        assert_eq!(
            release.manifest.model.chosen,
            passed.recommended.clone().unwrap()
        );
        assert_eq!(release.pending()[0].approver, "me");
        let state = f.store.state("fin-analyst").unwrap().unwrap();
        assert_eq!(state.versions, ["1.0.0"]);
        assert_eq!(
            state.active_version, None,
            "not released until it is accepted"
        );

        let released = f.approve("1.0.0");
        assert_eq!(released.state, ReleaseState::Released);
        let state = f.store.state("fin-analyst").unwrap().unwrap();
        assert_eq!(state.active_version.as_deref(), Some("1.0.0"));
        assert_eq!(state.state, Lifecycle::Active);
        load(&f.store, "fin-analyst", "1.0.0").unwrap();
        assert!(
            f.store
                .audit_lines(Some("fin-analyst"))
                .unwrap()
                .iter()
                .any(|l| l.what.contains("released")),
            "the release is audited"
        );

        // With no approver and no capability increase, it is released at once.
        let quiet = f.evaluated(
            |v| {
                v["approvers"] = json!([]);
                v["instructions"] = json!("Analyze reports more carefully.");
            },
            false,
        );
        let patch = f.release(&quiet.id, None).unwrap();
        assert_eq!(patch.manifest.version, "1.0.1");
        assert_eq!(patch.state, ReleaseState::Released);
        assert_eq!(
            f.store
                .state("fin-analyst")
                .unwrap()
                .unwrap()
                .active_version
                .as_deref(),
            Some("1.0.1")
        );
    }

    #[test]
    fn versions_follow_the_change_are_never_reused_and_never_lower() {
        let f = Fixture::new();
        let first = f.evaluated(approvers(&[]), false);
        assert_eq!(
            f.release(&first.id, None).unwrap().manifest.version,
            "1.0.0"
        );
        // A requirement added: minor.
        let more = f.evaluated(
            |v| {
                v["approvers"] = json!([]);
                v["requirements"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!({ "id": "R9", "text": "Lists the sources it read" }));
                let cases = v["evals"]["cases"].as_array_mut().unwrap();
                let mut case = cases[0].clone();
                case["id"] = json!("E9");
                case["requirement"] = json!("R9");
                cases.push(case);
            },
            false,
        );
        let minor = f.release(&more.id, None).unwrap();
        assert_eq!(minor.manifest.version, "1.1.0");
        assert_eq!(minor.manifest.change, Bump::Minor);
        // A requirement changed: its output contract, so major.
        let changed = f.evaluated(
            |v| {
                v["approvers"] = json!([]);
                v["requirements"][0]["text"] = json!("Something else entirely");
            },
            false,
        );
        let (kind, why) = refusal(f.release(&changed.id, Some("1.2.0")));
        assert_eq!(kind, crate::factory::RefusalKind::Invalid);
        assert!(why.contains("lower than 2.0.0"), "{why}");
        let (_, why) = refusal(f.release(&changed.id, Some("1.1.0")));
        assert!(why.contains("already a release"), "{why}");
        let (_, why) = refusal(f.release(&changed.id, Some("v2")));
        assert!(why.contains("isn't a semantic version"), "{why}");
        let major = f.release(&changed.id, Some("3.0.0")).unwrap();
        assert_eq!(
            major.manifest.version, "3.0.0",
            "a higher version is the owner's to choose"
        );
        assert_eq!(major.manifest.change, Bump::Major);
        assert!(
            major.manifest.change_why.contains("R1 changed"),
            "{}",
            major.manifest.change_why
        );
    }

    #[test]
    fn a_changed_byte_makes_a_release_fail_to_verify_and_unusable() {
        let f = Fixture::new();
        let e = f.evaluated(approvers(&[]), false);
        f.release(&e.id, None).unwrap();
        let dir = f.store.release_dir("fin-analyst", "1.0.0");
        load(&f.store, "fin-analyst", "1.0.0").unwrap();
        instructions_of(&f.store, "fin-analyst", "1.0.0").unwrap();
        let tamper = |path: &str, change: &dyn Fn(String) -> String| {
            let file = dir.join(path);
            let before = std::fs::read_to_string(&file).unwrap();
            std::fs::write(&file, change(before.clone())).unwrap();
            let failed = load(&f.store, "fin-analyst", "1.0.0").unwrap_err();
            std::fs::write(&file, before).unwrap();
            format!("{failed:#}")
        };
        let agent = tamper("files/agent.md", &|t| t.replace("Rules", "Ruled"));
        assert!(
            agent.contains("files/agent.md doesn't match its hash"),
            "{agent}"
        );
        let report = tamper("report.json", &|t| t.replacen("passed", "passed ", 1));
        assert!(
            report.contains("report.json doesn't match its hash"),
            "{report}"
        );
        let manifest = tamper(RELEASE_FILE, &|t| {
            t.replace("\"title\": \"", "\"title\": \"X")
        });
        assert!(
            manifest.contains("changed after it was signed"),
            "{manifest}"
        );
        std::fs::write(dir.join("files/extra.md"), "more").unwrap();
        let added = format!("{:#}", load(&f.store, "fin-analyst", "1.0.0").unwrap_err());
        assert!(added.contains("isn't in its manifest"), "{added}");
        assert!(
            instructions_of(&f.store, "fin-analyst", "1.0.0").is_err(),
            "unusable"
        );
        std::fs::remove_file(dir.join("files/extra.md")).unwrap();
        load(&f.store, "fin-analyst", "1.0.0").unwrap();
        let unknown = format!("{:#}", load(&f.store, "fin-analyst", "9.9.9").unwrap_err());
        assert!(
            unknown.contains("has no release 9.9.9 (it has 1.0.0)"),
            "{unknown}"
        );
    }

    #[test]
    fn approvers_answer_first_then_the_owner_signs_off_the_capability_increases() {
        let f = Fixture::new();
        let carol = SecretKey::generate();
        // The first release asks for a check and an MCP server: capability increases.
        let e = f.evaluated(approvers(&["carol"]), false);
        let release = f.release(&e.id, None).unwrap();
        assert!(
            !release.manifest.increases.is_empty(),
            "{:?}",
            release.manifest.increases
        );
        assert_eq!(
            release.pending(),
            [Ask {
                approver: "carol".into(),
                covers: vec![Cover::Release]
            }]
        );
        // The owner can't answer before carol.
        let report = report_of(&f.store, "fin-analyst", "1.0.0").unwrap();
        let early = own_answer("alice", &f.owner, &release, &report, true, "").unwrap_err();
        assert!(
            format!("{early:#}").contains("waits for carol first"),
            "{early:#}"
        );
        // Carol's agent answers the request it got, signed with her key.
        let ask = release.pending().remove(0);
        let (_, text) = request_text(&release, &report, &ask);
        assert!(
            text.contains("More than nothing (its first release)"),
            "{text}"
        );
        let sign = |key: &SecretKey, approved: bool, text: &str| {
            let output = format!(
                "{} by carol: fine",
                if approved { "Approved" } else { "Rejected" }
            );
            let mut evidence = Evidence::new(
                EvidenceKind::Approval,
                "carol",
                key.public(),
                f.owner.public(),
                7,
            );
            evidence.prompt_sha256 = sha256_hex(text.as_bytes());
            evidence.approved = Some(approved);
            (output.clone(), evidence.sign(&output, key))
        };
        // An answer signed with another key, or to another request, isn't hers.
        let (output, forged) = sign(&SecretKey::generate(), true, &text);
        let wrong = approval_of(&ask, carol.public(), &output, forged);
        assert!(answer(&f.store, "fin-analyst", "1.0.0", wrong).is_err());
        let (output, other) = sign(&carol, true, "another request");
        let wrong = approval_of(&ask, carol.public(), &output, other);
        assert!(answer(&f.store, "fin-analyst", "1.0.0", wrong).is_err());
        let (output, evidence) = sign(&carol, true, &text);
        let accepted = answer(
            &f.store,
            "fin-analyst",
            "1.0.0",
            approval_of(&ask, carol.public(), &output, evidence),
        )
        .unwrap();
        // Every other approver accepted it: it still waits for the owner, on the increases.
        assert_eq!(accepted.state, ReleaseState::PendingApproval);
        assert_eq!(
            accepted.pending(),
            [Ask {
                approver: "me".into(),
                covers: vec![Cover::Capabilities]
            }]
        );
        let released = f.approve("1.0.0");
        assert_eq!(released.state, ReleaseState::Released);
        assert_eq!(released.approvals.len(), 2);
        load(&f.store, "fin-analyst", "1.0.0").unwrap();

        // A rejection keeps the active release.
        let next = f.evaluated(
            |v| {
                v["approvers"] = json!(["carol"]);
                v["instructions"] = json!("Analyze reports more carefully.");
            },
            false,
        );
        let pending = f.release(&next.id, None).unwrap();
        assert_eq!(pending.manifest.version, "1.0.1");
        assert!(
            pending.manifest.increases.is_empty(),
            "nothing more than 1.0.0"
        );
        let report = report_of(&f.store, "fin-analyst", "1.0.1").unwrap();
        let ask = pending.pending().remove(0);
        let (_, text) = request_text(&pending, &report, &ask);
        let (output, evidence) = sign(&carol, false, &text);
        let rejected = answer(
            &f.store,
            "fin-analyst",
            "1.0.1",
            approval_of(&ask, carol.public(), &output, evidence),
        )
        .unwrap();
        assert_eq!(rejected.state, ReleaseState::Rejected);
        assert_eq!(rejected.why.as_deref(), Some("carol rejected it: fine"));
        let state = f.store.state("fin-analyst").unwrap().unwrap();
        assert_eq!(state.active_version.as_deref(), Some("1.0.0"));
        load(&f.store, "fin-analyst", "1.0.1").unwrap();
    }

    #[test]
    fn a_diff_says_what_changed_between_two_releases() {
        let f = Fixture::new();
        let a = f.evaluated(approvers(&[]), false);
        f.release(&a.id, None).unwrap();
        let b = f.evaluated(
            |v| {
                v["approvers"] = json!([]);
                v["instructions"] = json!("New\ninstructions");
                v["capabilities"]["delegate"] = json!(true);
                v["budgets"] = json!({ "tasks_per_day": 20 });
            },
            false,
        );
        let second = f.release(&b.id, None).unwrap();
        assert_eq!(
            second.manifest.version, "1.1.0",
            "{}",
            second.manifest.change_why
        );
        let lines = diff(&f.store, "fin-analyst", "1.0.0", "1.1.0").unwrap();
        for want in [
            "~ instructions",
            "+ delegation to other agents",
            "~ budgets: 50 tasks a day, 2 attempts, 20 minutes a task → 20 tasks a day",
            "files changed: agent.md, blueprint.json",
        ] {
            assert!(
                lines.iter().any(|l| l.starts_with(want)),
                "{want}: {lines:?}"
            );
        }
    }
}
