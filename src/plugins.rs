//! Plugins: skills and MCP servers packaged together, imported from a folder, a git
//! repository or GitHub, in the layouts GitHub Copilot CLI, Claude Code and Agent
//! Plugins use (`plugin.json`, `.claude-plugin/plugin.json`, `skills/*/SKILL.md`,
//! `mcp.json` / `.mcp.json`), or picked from a marketplace catalog
//! (`marketplace.json`).
//!
//! Importing runs nothing. A plugin's skills become available to plan steps
//! (`skill=name`), and its agents too (`agent=name`: instructions with the skills they
//! follow and the MCP servers they use). Its MCP servers are templates: their owner
//! offers one in a shared folder when they choose, with the access and tools they
//! allow and the values (keys, tokens) it needs. Commands, hooks and LSP servers are
//! listed and left out; hooks never run.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::mcp::{Fill, McpInput, McpOrigin, McpServer, McpTemplate};
use crate::profile::validate_name;
use crate::shares::ShareStore;

/// The most a plugin may hold (files, bytes), and the longest skill.
const MAX_FILES: usize = 5_000;
const MAX_BYTES: u64 = 100 * 1024 * 1024;
const MAX_SKILL_BYTES: u64 = 64 * 1024;

/// Where a plugin comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A folder on this machine.
    Folder(PathBuf),
    /// A git repository (`path`: the plugin's folder inside it).
    Git {
        url: String,
        path: Option<String>,
        /// The GitHub host, for repositories given as `owner/repo`.
        host: Option<String>,
    },
}

const SOURCE_HELP: &str = "use owner/repo, owner/repo:path/to/plugin, a git URL (https://..., add #path for a folder in it) or a folder on this machine";

fn clean_subpath(path: &str) -> Result<Option<String>> {
    let path = path.trim().trim_start_matches("./").trim_matches('/');
    if path.is_empty() || path == "." {
        return Ok(None);
    }
    let p = Path::new(path);
    if p.is_absolute() || p.components().any(|c| !matches!(c, Component::Normal(_))) {
        bail!("{path:?} must be a folder inside the repository");
    }
    Ok(Some(path.replace('\\', "/")))
}

impl Source {
    pub fn parse(input: &str) -> Result<Self> {
        let input = input.trim();
        if input.is_empty() {
            bail!("give where the plugin is: {SOURCE_HELP}");
        }
        if Path::new(input).is_dir() {
            return Ok(Source::Folder(PathBuf::from(input)));
        }
        let lower = input.to_ascii_lowercase();
        if lower.starts_with("https://")
            || lower.starts_with("http://")
            || lower.starts_with("file://")
            || lower.starts_with("git@")
        {
            if lower.starts_with("http://") {
                bail!("use https:// for git URLs");
            }
            let (url, path) = match input.split_once('#') {
                Some((u, p)) => (u.to_owned(), clean_subpath(p)?),
                None => (input.to_owned(), None),
            };
            let host = github_host(&url);
            return Ok(Source::Git { url, path, host });
        }
        let (repo, path) = match input.split_once(':') {
            Some((r, p)) => (r, clean_subpath(p)?),
            None => (input, None),
        };
        let parts: Vec<&str> = repo.split('/').filter(|p| !p.is_empty()).collect();
        let ok = |s: &str| {
            !s.is_empty()
                && !s.starts_with('.')
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        };
        let (host, owner, name) = match parts.as_slice() {
            [owner, name] => ("github.com", *owner, *name),
            [host, owner, name] if host.contains('.') => (*host, *owner, *name),
            _ => bail!("{input:?} is not a plugin source: {SOURCE_HELP}"),
        };
        if !(ok(host) && ok(owner) && ok(name)) {
            bail!("{input:?} is not a plugin source: {SOURCE_HELP}");
        }
        let name = name.trim_end_matches(".git");
        Ok(Source::Git {
            url: format!("https://{host}/{owner}/{name}.git"),
            path,
            host: Some(host.to_owned()),
        })
    }

    /// The same source, pointing at `sub` inside it (a marketplace's relative entry).
    pub fn join(&self, sub: &str) -> Result<Source> {
        let Some(sub) = clean_subpath(sub)? else {
            return Ok(self.clone());
        };
        Ok(match self {
            Source::Folder(dir) => Source::Folder(dir.join(&sub)),
            Source::Git { url, path, host } => Source::Git {
                url: url.clone(),
                path: Some(match path {
                    Some(p) => format!("{p}/{sub}"),
                    None => sub,
                }),
                host: host.clone(),
            },
        })
    }

    /// How it is written back (and parsed again when the plugin updates).
    pub fn spec(&self) -> String {
        match self {
            Source::Folder(dir) => dir.display().to_string(),
            Source::Git { url, path, host } => {
                let slug = host.as_ref().and_then(|h| {
                    url.strip_prefix(&format!("https://{h}/"))
                        .map(|s| s.trim_end_matches(".git").to_owned())
                });
                match (slug, path) {
                    (Some(slug), Some(p)) if host.as_deref() == Some("github.com") => {
                        format!("{slug}:{p}")
                    }
                    (Some(slug), None) if host.as_deref() == Some("github.com") => slug,
                    (Some(slug), Some(p)) => {
                        format!("{}/{slug}:{p}", host.as_deref().unwrap_or_default())
                    }
                    (Some(slug), None) => format!("{}/{slug}", host.as_deref().unwrap_or_default()),
                    (None, Some(p)) => format!("{url}#{p}"),
                    (None, None) => url.clone(),
                }
            }
        }
    }
}

/// The GitHub host of an https git URL (`https://github.com/o/r.git` -> `github.com`).
fn github_host(url: &str) -> Option<String> {
    let rest = url.strip_prefix("https://")?;
    let host = rest.split('/').next()?;
    (host == "github.com" || host.ends_with(".ghe.com") || host.starts_with("github."))
        .then(|| host.to_ascii_lowercase())
}

/// A plugin's files, fetched for review before import (removed when dropped if
/// they were fetched into a temporary folder).
#[derive(Debug)]
pub struct Fetched {
    pub source: Source,
    /// The plugin's (or marketplace's) folder.
    pub root: PathBuf,
    /// The commit fetched (git sources).
    pub commit: Option<String>,
    /// The repository's root (git sources), for marketplace entries in it.
    checkout: Option<PathBuf>,
    temp: Option<PathBuf>,
}

impl Drop for Fetched {
    fn drop(&mut self) {
        if let Some(t) = self.temp.take() {
            let _ = std::fs::remove_dir_all(t);
        }
    }
}

fn random_id() -> String {
    data_encoding::HEXLOWER.encode(&iroh::SecretKey::generate().to_bytes()[..8])
}

/// Resolves `sub` inside `root` and checks it stays there.
fn inside(root: &Path, sub: Option<&str>) -> Result<PathBuf> {
    let path = match sub {
        Some(s) => root.join(s),
        None => root.to_path_buf(),
    };
    let real = std::fs::canonicalize(&path)
        .with_context(|| format!("{} is not in the source", sub.unwrap_or(".")))?;
    let base = std::fs::canonicalize(root)?;
    if !real.starts_with(&base) {
        bail!("{} leads outside the source", sub.unwrap_or("."));
    }
    if !real.is_dir() {
        bail!("{} is not a folder", sub.unwrap_or("."));
    }
    Ok(real)
}

/// Fetches `source` for review: a folder is read where it is; a git repository is
/// cloned (shallow) into `staging`, with `creds` for its GitHub host if given.
pub async fn fetch(
    source: &Source,
    staging: &Path,
    creds: Option<&crate::github::GithubCredentials>,
) -> Result<Fetched> {
    match source {
        Source::Folder(dir) => {
            let root = std::fs::canonicalize(dir)
                .with_context(|| format!("{} is not an existing folder", dir.display()))?;
            if !root.is_dir() {
                bail!("{} is not a folder", dir.display());
            }
            Ok(Fetched {
                source: Source::Folder(plain(root.clone())),
                root,
                commit: None,
                checkout: None,
                temp: None,
            })
        }
        Source::Git { url, path, host } => {
            std::fs::create_dir_all(staging)?;
            let temp = staging.join(random_id());
            let mut fetched = Fetched {
                source: source.clone(),
                root: temp.clone(),
                commit: None,
                checkout: Some(temp.clone()),
                temp: Some(temp.clone()),
            };
            let mut cmd = tokio::process::Command::new("git");
            cmd.args(["clone", "--depth", "1", "--quiet", "--no-tags"])
                .arg(url)
                .arg(&temp)
                .stdin(Stdio::null())
                .kill_on_drop(true)
                .env("GIT_TERMINAL_PROMPT", "0")
                .env("GIT_ASKPASS", "")
                .env("GCM_INTERACTIVE", "never");
            for var in crate::profile::INHERITED_CREDENTIAL_VARS {
                cmd.env_remove(var);
            }
            if let (Some(creds), Some(host)) = (creds, host)
                && creds.host.eq_ignore_ascii_case(host)
            {
                cmd.envs(creds.git_env());
            }
            let out = tokio::time::timeout(std::time::Duration::from_secs(180), cmd.output())
                .await
                .map_err(|_| anyhow!("fetching {url} took over 3 minutes"))?
                .context("running git (is git installed?)")?;
            if !out.status.success() {
                let err = String::from_utf8_lossy(&out.stderr);
                let err = crate::redact::redact(err.trim()).0;
                bail!(
                    "couldn't fetch {url}: {err}{}",
                    if err.contains("not found") || err.contains("Authentication") {
                        " (for a private repository, sign the profile in to GitHub first)"
                    } else {
                        ""
                    }
                );
            }
            let head = tokio::process::Command::new("git")
                .arg("-C")
                .arg(&temp)
                .args(["rev-parse", "HEAD"])
                .stdin(Stdio::null())
                .output()
                .await?;
            fetched.commit = Some(String::from_utf8_lossy(&head.stdout).trim().to_owned())
                .filter(|c| !c.is_empty());
            fetched.root = inside(&temp, path.as_deref())?;
            Ok(fetched)
        }
    }
}

impl Fetched {
    /// The plugin a marketplace entry points to: in the same repository or folder,
    /// or another source.
    pub async fn entry(
        &self,
        entry: &MarketEntry,
        staging: &Path,
        creds: Option<&crate::github::GithubCredentials>,
    ) -> Result<Fetched> {
        match &entry.source {
            MarketSource::Relative(sub) => {
                let base = self.checkout.as_deref().unwrap_or(&self.root);
                let source = match &self.source {
                    Source::Git { url, host, .. } => Source::Git {
                        url: url.clone(),
                        path: clean_subpath(sub)?,
                        host: host.clone(),
                    },
                    Source::Folder(_) => Source::Folder(base.join(sub)),
                };
                let root = inside(base, clean_subpath(sub)?.as_deref())?;
                let source = match source {
                    Source::Folder(_) => Source::Folder(plain(root.clone())),
                    other => other,
                };
                Ok(Fetched {
                    source,
                    root,
                    commit: self.commit.clone(),
                    checkout: None,
                    temp: None,
                })
            }
            MarketSource::Other(spec) => {
                Box::pin(fetch(&Source::parse(spec)?, staging, creds)).await
            }
            MarketSource::Unsupported(why) => {
                bail!("plugin '{}' can't be imported: {why}", entry.name)
            }
        }
    }
}

/// A skill a plugin contains.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginSkill {
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Its `SKILL.md`, relative to the plugin's folder.
    pub path: String,
}

/// An agent a plugin contains: a role plan steps can take (see [`crate::skills`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginAgent {
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Its file, relative to the plugin's folder.
    pub path: String,
    /// The skills it follows and the MCP servers it uses, by name.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp: Vec<String>,
}

/// An imported plugin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plugin {
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub version: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Which layout it uses (Agent Plugins, Copilot CLI, Claude Code, a skills folder...).
    pub format: String,
    /// Where it came from (folder, owner/repo[:path] or git URL): updates fetch it again.
    #[serde(default)]
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub installed_unix: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_unix: Option<i64>,
    #[serde(default)]
    pub skills: Vec<PluginSkill>,
    #[serde(default)]
    pub agents: Vec<PluginAgent>,
    /// MCP server templates by name.
    #[serde(default)]
    pub mcp: BTreeMap<String, McpTemplate>,
    /// What it contains that isn't used here, and other notes from the import.
    #[serde(default)]
    pub notes: Vec<String>,
}

impl Plugin {
    /// Its MCP server template `server`; none given: the one named `name`, or its only one.
    pub fn pick_server(&self, server: Option<&str>, name: &str) -> Result<String> {
        let names = || self.mcp.keys().cloned().collect::<Vec<_>>().join(", ");
        if self.mcp.is_empty() {
            bail!("plugin '{}' has no MCP servers", self.name);
        }
        match server {
            Some(s) if self.mcp.contains_key(s) => Ok(s.to_owned()),
            Some(s) => bail!(
                "plugin '{}' has no MCP server '{s}' (it has: {})",
                self.name,
                names()
            ),
            None if self.mcp.contains_key(name) => Ok(name.to_owned()),
            None if self.mcp.len() == 1 => Ok(self.mcp.keys().next().cloned().unwrap_or_default()),
            None => bail!(
                "plugin '{}' has several MCP servers ({}): pick one with --server",
                self.name,
                names()
            ),
        }
    }
}

fn yes() -> bool {
    true
}

/// Where a marketplace entry's plugin is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "kebab-case")]
pub enum MarketSource {
    /// A folder in the marketplace's own repository.
    Relative(String),
    /// Another source (`owner/repo`, a git URL).
    Other(String),
    Unsupported(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MarketEntry {
    pub name: String,
    pub description: String,
    pub version: String,
    pub source: MarketSource,
}

/// What a fetched source holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inspection {
    Plugin(Box<Plugin>),
    Marketplace {
        name: String,
        plugins: Vec<MarketEntry>,
    },
}

fn read_json(path: &Path) -> Result<Value> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(text.trim_start_matches('\u{feff}'))
        .with_context(|| format!("{} isn't valid JSON", path.display()))
}

fn str_field(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .trim()
        .to_owned()
}

/// A name proofkin accepts, from `name` (`My Plugin` -> `my-plugin`).
fn safe_name(name: &str) -> String {
    let mut out: String = name
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    while out.contains("--") {
        out = out.replace("--", "-");
    }
    let out: String = out.trim_matches(['-', '.']).chars().take(64).collect();
    out
}

/// `name:` and `description:` from YAML front matter (`---` ... `---`).
fn front_matter(text: &str) -> (Option<String>, String) {
    let text = text.trim_start_matches('\u{feff}');
    let Some(rest) = text.strip_prefix("---") else {
        return (None, String::new());
    };
    let Some((head, _)) = rest.split_once("\n---") else {
        return (None, String::new());
    };
    let lines: Vec<&str> = head.lines().collect();
    let value = |key: &str| -> Option<String> {
        let i = lines
            .iter()
            .position(|l| l.starts_with(&format!("{key}:")))?;
        let v = lines[i][key.len() + 1..].trim();
        let v = if matches!(v, ">" | "|" | ">-" | "|-" | ">+" | "|+") {
            lines[i + 1..]
                .iter()
                .take_while(|l| l.starts_with(' ') || l.starts_with('\t') || l.trim().is_empty())
                .map(|l| l.trim())
                .filter(|l| !l.is_empty())
                .collect::<Vec<_>>()
                .join(" ")
        } else {
            v.trim_matches(['"', '\'']).to_owned()
        };
        Some(v).filter(|v| !v.is_empty())
    };
    (value("name"), value("description").unwrap_or_default())
}

/// Paths a manifest field names (`"skills/"`, `["a", "b"]`).
fn paths(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|x| x.as_str().map(str::to_owned))
            .collect(),
        _ => Vec::new(),
    }
}

fn rel(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// The skills in `dir`: `dir/SKILL.md` itself, or `dir/*/SKILL.md`.
fn skills_in(root: &Path, dir: &Path, skills: &mut Vec<PluginSkill>, notes: &mut Vec<String>) {
    let mut add = |skill_md: &Path, folder: &Path| {
        let meta = match std::fs::symlink_metadata(skill_md) {
            Ok(m) if m.is_file() => m,
            _ => return,
        };
        let folder_name = folder
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("skill")
            .to_owned();
        if meta.len() > MAX_SKILL_BYTES {
            notes.push(format!(
                "skill {folder_name}: SKILL.md is over 64 KB, left out"
            ));
            return;
        }
        let text = std::fs::read_to_string(skill_md).unwrap_or_default();
        let (name, description) = front_matter(&text);
        let wanted = name.unwrap_or(folder_name);
        let name = safe_name(&wanted);
        if name.is_empty() || validate_name(&name).is_err() {
            notes.push(format!("skill {wanted:?}: no usable name, left out"));
            return;
        }
        if name != wanted {
            notes.push(format!("skill {wanted:?} is used as '{name}'"));
        }
        if skills.iter().any(|s| s.name == name) {
            notes.push(format!(
                "skill '{name}' appears twice; the first one is used"
            ));
            return;
        }
        let others = std::fs::read_dir(folder)
            .map(|d| d.flatten().filter(|e| e.file_name() != "SKILL.md").count())
            .unwrap_or(0);
        if others > 0 && folder != root {
            notes.push(format!(
                "skill '{name}': only SKILL.md's text reaches peers, not its {others} other file{}",
                if others == 1 { "" } else { "s" }
            ));
        }
        skills.push(PluginSkill {
            name,
            description,
            path: rel(root, skill_md),
        });
    };
    if dir.join("SKILL.md").is_file() {
        add(&dir.join("SKILL.md"), dir);
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut subdirs: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .collect();
    subdirs.sort();
    for sub in subdirs {
        add(&sub.join("SKILL.md"), &sub);
    }
}

/// MCP servers from a manifest's `mcpServers` value or a file it names.
fn mcp_from(
    root: &Path,
    v: &Value,
    known: &BTreeMap<String, McpInput>,
    servers: &mut BTreeMap<String, McpTemplate>,
    notes: &mut Vec<String>,
) {
    let mut load = |config: &Value, origin: &str| match crate::mcp::parse_config(config, known) {
        Ok((found, more)) => {
            servers.extend(found);
            notes.extend(more);
        }
        Err(e) => notes.push(format!("{origin}: {e:#}")),
    };
    match v {
        Value::String(p) => match clean_subpath(p).and_then(|p| inside_file(root, p.as_deref())) {
            Ok(file) => match read_json(&file) {
                Ok(config) => load(&config, p),
                Err(e) => notes.push(format!("{e:#}")),
            },
            Err(e) => notes.push(format!("MCP configuration {p:?}: {e:#}")),
        },
        Value::Array(items) => {
            for item in items {
                mcp_from(root, item, known, servers, notes);
            }
        }
        Value::Object(o) if o.contains_key("mcpServers") || o.contains_key("servers") => {
            load(v, "plugin.json")
        }
        Value::Object(_) => load(&serde_json::json!({ "mcpServers": v }), "plugin.json"),
        _ => {}
    }
}

fn inside_file(root: &Path, sub: Option<&str>) -> Result<PathBuf> {
    let sub = sub.context("names no file")?;
    let real =
        std::fs::canonicalize(root.join(sub)).with_context(|| format!("{sub} doesn't exist"))?;
    if !real.starts_with(std::fs::canonicalize(root)?) {
        bail!("{sub} leads outside the plugin");
    }
    Ok(real)
}

/// The values a Claude Code plugin's `userConfig` describes.
fn user_config(manifest: &Value) -> BTreeMap<String, McpInput> {
    let mut known = BTreeMap::new();
    if let Some(cfg) = manifest.get("userConfig").and_then(|v| v.as_object()) {
        for (key, spec) in cfg {
            known.insert(
                key.clone(),
                McpInput {
                    name: key.clone(),
                    description: str_field(spec, "description"),
                    default: spec
                        .get("default")
                        .and_then(|d| d.as_str())
                        .map(str::to_owned),
                    secret: spec
                        .get("sensitive")
                        .and_then(|s| s.as_bool())
                        .unwrap_or(false),
                },
            );
        }
    }
    known
}

const MANIFESTS: &[&str] = &[
    "plugin.json",
    ".plugin/plugin.json",
    ".github/plugin/plugin.json",
    ".claude-plugin/plugin.json",
];
const MARKETPLACES: &[&str] = &[
    ".github/plugin/marketplace.json",
    ".claude-plugin/marketplace.json",
    "marketplace.json",
];

/// What `root` holds: a plugin (with a manifest, or just skills and MCP servers),
/// or a marketplace catalog.
pub fn inspect(root: &Path) -> Result<Inspection> {
    let manifest_path = MANIFESTS.iter().map(|m| root.join(m)).find(|p| p.is_file());
    if manifest_path.is_none()
        && let Some(market) = MARKETPLACES
            .iter()
            .map(|m| root.join(m))
            .find(|p| p.is_file())
    {
        return marketplace(&read_json(&market)?);
    }
    let manifest = match &manifest_path {
        Some(p) => read_json(p)?,
        None => Value::Object(Default::default()),
    };
    let mut notes = Vec::new();
    let agent_plugins =
        str_field(&manifest, "$schema").starts_with("https://agent-plugins.org/schemas/");
    let format = match &manifest_path {
        _ if agent_plugins => "Agent Plugins",
        Some(p)
            if p.ends_with(".claude-plugin/plugin.json")
                || p.ends_with(".claude-plugin\\plugin.json") =>
        {
            "Claude Code plugin"
        }
        Some(_) => "Copilot CLI plugin",
        None => "skills and MCP servers folder",
    };
    let folder_name = root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("plugin")
        .to_owned();
    let wanted = Some(str_field(&manifest, "name"))
        .filter(|n| !n.is_empty())
        .unwrap_or(folder_name);
    let name = safe_name(&wanted);
    if name.is_empty() || validate_name(&name).is_err() {
        bail!("the plugin's name {wanted:?} isn't usable; name it with letters, digits and -");
    }

    // Skills.
    let mut skills = Vec::new();
    let skill_dirs: Vec<String> = if agent_plugins {
        vec!["skills".into()]
    } else {
        let declared = paths(manifest.get("skills"));
        if declared.is_empty() {
            vec!["skills".into()]
        } else {
            declared
        }
    };
    for dir in &skill_dirs {
        match clean_subpath(dir) {
            Ok(sub) => {
                let path = match &sub {
                    Some(s) => root.join(s),
                    None => root.to_path_buf(),
                };
                if path.is_dir() {
                    skills_in(root, &path, &mut skills, &mut notes);
                }
            }
            Err(e) => notes.push(format!("skills {dir:?}: {e:#}")),
        }
    }
    if skills.is_empty() && !agent_plugins && root.join("SKILL.md").is_file() {
        skills_in(root, root, &mut skills, &mut notes);
    }

    // MCP servers: the root file first, then what the manifest declares (later wins).
    let mut mcp = BTreeMap::new();
    let known = user_config(&manifest);
    let root_files: &[&str] = if agent_plugins {
        &["mcp.json"]
    } else {
        &[".mcp.json", "mcp.json"]
    };
    for file in root_files {
        let path = root.join(file);
        if path.is_file() {
            match read_json(&path) {
                Ok(config) => mcp_from(root, &config, &known, &mut mcp, &mut notes),
                Err(e) => notes.push(format!("{e:#}")),
            }
            break;
        }
    }
    if !agent_plugins && let Some(declared) = manifest.get("mcpServers") {
        mcp_from(root, declared, &known, &mut mcp, &mut notes);
    }

    // Agents: what the manifest declares, else `agents/` (Copilot CLI's own folder too).
    let mut agents = Vec::new();
    let declared = paths(manifest.get("agents"));
    let agent_paths = if declared.is_empty() {
        vec!["agents".to_owned(), "com.github.copilot/agents".to_owned()]
    } else {
        declared
    };
    for dir in &agent_paths {
        match clean_subpath(dir) {
            Ok(sub) => {
                let path = sub.map_or_else(|| root.to_path_buf(), |s| root.join(s));
                agents_in(root, &path, &mcp, &mut agents, &mut notes);
            }
            Err(e) => notes.push(format!("agents {dir:?}: {e:#}")),
        }
    }

    // What isn't used here.
    let mut unused = Vec::new();
    let has = |dir: &str| root.join(dir).is_dir();
    if has("commands") || manifest.get("commands").is_some() || has("com.github.copilot/commands") {
        unused.push("commands");
    }
    if has("hooks")
        || root.join("hooks.json").is_file()
        || manifest.get("hooks").is_some()
        || has("com.github.copilot/hooks")
    {
        unused.push("hooks (they never run here)");
    }
    if manifest.get("lspServers").is_some()
        || has("lsp-config")
        || root.join("com.github.copilot/lsp.json").is_file()
    {
        unused.push("LSP servers");
    }
    if has("com.github.copilot/rules") || has("rules") {
        unused.push("rules");
    }
    if !unused.is_empty() {
        notes.insert(0, format!("Not used here: {}.", unused.join(", ")));
    }
    if skills.is_empty() && mcp.is_empty() && agents.is_empty() {
        bail!(
            "{} has no skills (skills/NAME/SKILL.md), agents (agents/*.md) or MCP servers (.mcp.json or mcp.json) that proofkin can use{}",
            if manifest_path.is_some() {
                format!("plugin '{name}'")
            } else {
                "this folder".into()
            },
            if unused.is_empty() {
                String::new()
            } else {
                format!("; it has {}", unused.join(", "))
            }
        );
    }
    let description: String = str_field(&manifest, "description")
        .chars()
        .take(1024)
        .collect();
    Ok(Inspection::Plugin(Box::new(Plugin {
        name,
        version: str_field(&manifest, "version"),
        description,
        format: format.into(),
        source: String::new(),
        commit: None,
        enabled: true,
        installed_unix: 0,
        updated_unix: None,
        skills,
        agents,
        mcp,
        notes,
    })))
}

/// The agents in `path`, a folder of `*.md` files (`NAME.agent.md` too) or one file;
/// what can't be used is a note.
fn agents_in(
    root: &Path,
    path: &Path,
    mcp: &BTreeMap<String, McpTemplate>,
    agents: &mut Vec<PluginAgent>,
    notes: &mut Vec<String>,
) {
    let files: Vec<PathBuf> = if path.is_dir() {
        let mut files: Vec<PathBuf> = std::fs::read_dir(path)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "md"))
            .collect();
        files.sort();
        files
    } else if path.is_file() {
        vec![path.to_path_buf()]
    } else {
        return;
    };
    for file in files {
        // Not through links: a plugin's files stay inside it.
        let Ok(meta) = std::fs::symlink_metadata(&file) else {
            continue;
        };
        let stem = file
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("agent")
            .trim_end_matches(".md")
            .trim_end_matches(".agent")
            .to_owned();
        if !meta.is_file() {
            continue;
        }
        if meta.len() > MAX_SKILL_BYTES {
            notes.push(format!("agent {stem}: over 64 KB, left out"));
            continue;
        }
        let text = std::fs::read_to_string(&file).unwrap_or_default();
        let def = match crate::skills::parse_agent(&text, &safe_name(&stem)) {
            Ok(def) => def,
            Err(e) => {
                notes.push(format!("agent {stem}: {e:#}; left out"));
                continue;
            }
        };
        if agents.iter().any(|a| a.name == def.name) {
            notes.push(format!(
                "agent '{}' appears twice; the first one is used",
                def.name
            ));
            continue;
        }
        for server in def.mcp.iter().filter(|m| !mcp.contains_key(*m)) {
            notes.push(format!(
                "agent '{}' uses MCP server '{server}', which this plugin doesn't bring: steps with it need a folder that offers one by that name",
                def.name
            ));
        }
        notes.extend(
            def.notes
                .iter()
                .map(|n| format!("agent '{}': {n}", def.name)),
        );
        agents.push(PluginAgent {
            name: def.name,
            description: def.description,
            path: file
                .strip_prefix(root)
                .unwrap_or(&file)
                .to_string_lossy()
                .replace('\\', "/"),
            skills: def.skills,
            mcp: def.mcp,
        });
    }
}

fn marketplace(v: &Value) -> Result<Inspection> {
    let name = str_field(v, "name");
    let base = v
        .get("metadata")
        .map(|m| str_field(m, "pluginRoot"))
        .unwrap_or_default();
    let mut plugins = Vec::new();
    for entry in v
        .get("plugins")
        .and_then(|p| p.as_array())
        .into_iter()
        .flatten()
    {
        let source = match entry.get("source") {
            Some(Value::String(s)) if s.starts_with("https://") || s.starts_with("git@") => {
                MarketSource::Other(s.clone())
            }
            Some(Value::String(s)) => {
                let joined = if base.is_empty() {
                    s.clone()
                } else {
                    format!(
                        "{}/{}",
                        base.trim_end_matches('/'),
                        s.trim_start_matches("./")
                    )
                };
                MarketSource::Relative(joined)
            }
            Some(Value::Object(o)) => {
                let kind = o.get("source").and_then(|k| k.as_str()).unwrap_or_default();
                let text = |k: &str| {
                    o.get(k)
                        .and_then(|x| x.as_str())
                        .unwrap_or_default()
                        .to_owned()
                };
                match kind {
                    "github" if !text("repo").is_empty() => {
                        MarketSource::Other(if text("path").is_empty() {
                            text("repo")
                        } else {
                            format!("{}:{}", text("repo"), text("path"))
                        })
                    }
                    "url" | "git" if !text("url").is_empty() => {
                        MarketSource::Other(if text("path").is_empty() {
                            text("url")
                        } else {
                            format!("{}#{}", text("url"), text("path"))
                        })
                    }
                    other => MarketSource::Unsupported(format!(
                        "its source kind {other:?} isn't supported"
                    )),
                }
            }
            _ => MarketSource::Unsupported("it has no source".into()),
        };
        plugins.push(MarketEntry {
            name: str_field(entry, "name"),
            description: str_field(entry, "description"),
            version: str_field(entry, "version"),
            source,
        });
    }
    if plugins.is_empty() {
        bail!("marketplace '{name}' lists no plugins");
    }
    Ok(Inspection::Marketplace { name, plugins })
}

/// Copies `from` into `to` (which must not exist): no `.git` folder, no symbolic
/// links, at most [`MAX_FILES`] files and [`MAX_BYTES`] bytes.
fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    let (mut files, mut bytes) = (0usize, 0u64);
    let mut stack = vec![(from.to_path_buf(), to.to_path_buf())];
    std::fs::create_dir_all(to)?;
    while let Some((src, dst)) = stack.pop() {
        for entry in
            std::fs::read_dir(&src).with_context(|| format!("reading {}", src.display()))?
        {
            let entry = entry?;
            let kind = entry.file_type()?;
            let name = entry.file_name();
            if kind.is_symlink() || name == ".git" {
                continue;
            }
            let target = dst.join(&name);
            if kind.is_dir() {
                std::fs::create_dir_all(&target)?;
                stack.push((entry.path(), target));
            } else if kind.is_file() {
                files += 1;
                bytes += entry.metadata()?.len();
                if files > MAX_FILES || bytes > MAX_BYTES {
                    bail!("the plugin is too large (over {MAX_FILES} files or 100 MB)");
                }
                std::fs::copy(entry.path(), &target)?;
            }
        }
    }
    Ok(())
}

/// `C:\x` instead of `\\?\C:\x`.
fn plain(path: PathBuf) -> PathBuf {
    match path.to_str().and_then(|p| p.strip_prefix(r"\\?\")) {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => PathBuf::from(rest),
        _ => path,
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Index {
    #[serde(default)]
    plugins: Vec<Plugin>,
}

/// A profile's imported plugins: `plugins.json`, their files in `plugins/<name>/`
/// and a folder each may write to in `plugin-data/<name>/`.
#[derive(Debug, Clone)]
pub struct PluginStore {
    profile_dir: PathBuf,
}

/// What changed when a plugin was updated or removed, for its owner.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Changes {
    pub lines: Vec<String>,
    /// Whether the profile's shared folders changed (save and tell peers).
    pub shares_changed: bool,
}

impl PluginStore {
    pub fn new(profile_dir: impl Into<PathBuf>) -> Self {
        Self {
            profile_dir: profile_dir.into(),
        }
    }

    fn index_path(&self) -> PathBuf {
        self.profile_dir.join("plugins.json")
    }

    /// Where sources are fetched for review.
    pub fn staging(&self) -> PathBuf {
        self.profile_dir.join("plugins").join(".staging")
    }

    pub fn plugin_dir(&self, name: &str) -> PathBuf {
        self.profile_dir.join("plugins").join(name)
    }

    pub fn data_dir(&self, name: &str) -> PathBuf {
        self.profile_dir.join("plugin-data").join(name)
    }

    fn load(&self) -> Result<Index> {
        let path = self.index_path();
        if !path.is_file() {
            return Ok(Index::default());
        }
        let text = std::fs::read_to_string(&path)?;
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    fn save(&self, index: &Index) -> Result<()> {
        crate::store::write_atomic(&self.index_path(), &serde_json::to_vec_pretty(index)?)
    }

    pub fn list(&self) -> Result<Vec<Plugin>> {
        Ok(self.load()?.plugins)
    }

    pub fn get(&self, name: &str) -> Result<Plugin> {
        self.load()?
            .plugins
            .into_iter()
            .find(|p| p.name == name)
            .with_context(|| format!("there is no plugin '{name}' (see plugin list)"))
    }

    /// Imports the plugin in `fetched` as `rename` (default: its own name).
    pub fn install(&self, fetched: &Fetched, rename: Option<&str>) -> Result<Plugin> {
        let Inspection::Plugin(plugin) = inspect(&fetched.root)? else {
            bail!("this is a marketplace: pick one of its plugins");
        };
        let mut plugin = *plugin;
        if let Some(name) = rename {
            validate_name(name).context("invalid plugin name")?;
            plugin.name = name.to_owned();
        }
        let mut index = self.load()?;
        if index.plugins.iter().any(|p| p.name == plugin.name) {
            bail!(
                "a plugin named '{}' is imported already: update it, or import this one under another name",
                plugin.name
            );
        }
        self.copy_files(&fetched.root, &plugin.name)?;
        plugin.source = fetched.source.spec();
        plugin.commit = fetched.commit.clone();
        plugin.installed_unix = chrono::Local::now().timestamp();
        index.plugins.push(plugin.clone());
        index.plugins.sort_by(|a, b| a.name.cmp(&b.name));
        self.save(&index)?;
        Ok(plugin)
    }

    fn copy_files(&self, root: &Path, name: &str) -> Result<()> {
        let dir = self.plugin_dir(name);
        let temp = self
            .profile_dir
            .join("plugins")
            .join(format!(".new-{}", random_id()));
        if let Err(e) = copy_tree(root, &temp) {
            let _ = std::fs::remove_dir_all(&temp);
            return Err(e);
        }
        if dir.exists() {
            std::fs::remove_dir_all(&dir)
                .with_context(|| format!("replacing {}", dir.display()))?;
        }
        std::fs::rename(&temp, &dir).with_context(|| format!("writing {}", dir.display()))?;
        Ok(())
    }

    /// Replaces plugin `name`'s files with `fetched` (a newer version) and makes its
    /// offered MCP servers again from the new templates; see [`refresh_offers`].
    pub fn update(
        &self,
        name: &str,
        fetched: &Fetched,
        shares: &mut ShareStore,
    ) -> Result<(Plugin, Changes)> {
        let old = self.get(name)?;
        let Inspection::Plugin(new) = inspect(&fetched.root)? else {
            bail!(
                "{} is a marketplace now, not a plugin",
                fetched.source.spec()
            );
        };
        let mut new = *new;
        new.name = old.name.clone();
        new.enabled = old.enabled;
        new.source = fetched.source.spec();
        new.commit = fetched.commit.clone();
        new.installed_unix = old.installed_unix;
        new.updated_unix = Some(chrono::Local::now().timestamp());
        let mut changes = Changes::default();
        if old.version != new.version && !new.version.is_empty() {
            changes.lines.push(format!(
                "version {} -> {}",
                if old.version.is_empty() {
                    "?"
                } else {
                    &old.version
                },
                new.version
            ));
        }
        let names = |s: &[PluginSkill]| {
            s.iter()
                .map(|k| k.name.clone())
                .collect::<std::collections::BTreeSet<_>>()
        };
        let (before, after) = (names(&old.skills), names(&new.skills));
        for added in after.difference(&before) {
            changes.lines.push(format!("new skill {added}"));
        }
        for gone in before.difference(&after) {
            changes.lines.push(format!("skill {gone} removed"));
        }
        for added in new.mcp.keys().filter(|k| !old.mcp.contains_key(*k)) {
            changes
                .lines
                .push(format!("new MCP server template {added}"));
        }
        self.copy_files(&fetched.root, &new.name)?;
        let mut index = self.load()?;
        if let Some(slot) = index.plugins.iter_mut().find(|p| p.name == new.name) {
            *slot = new.clone();
        }
        self.save(&index)?;
        let more = refresh_offers(self, &new, shares)?;
        changes.shares_changed = more.shares_changed;
        changes.lines.extend(more.lines);
        if changes.lines.is_empty() {
            changes.lines.push("no changes".into());
        }
        Ok((new, changes))
    }

    /// Turns plugin `name` on or off: its skills and its offered MCP servers.
    pub fn set_enabled(
        &self,
        name: &str,
        enabled: bool,
        shares: &mut ShareStore,
    ) -> Result<Changes> {
        let mut index = self.load()?;
        let plugin = index
            .plugins
            .iter_mut()
            .find(|p| p.name == name)
            .with_context(|| format!("there is no plugin '{name}' (see plugin list)"))?;
        plugin.enabled = enabled;
        self.save(&index)?;
        let mut changes = Changes::default();
        for share in &mut shares.shares {
            for (server, def) in share.mcp.iter_mut() {
                if def.from.as_ref().is_some_and(|f| f.plugin == name) && def.plugin_off == enabled
                {
                    def.plugin_off = !enabled;
                    changes.shares_changed = true;
                    changes.lines.push(format!(
                        "MCP server {server} in '{}' is {}",
                        share.name,
                        if enabled {
                            "offered again"
                        } else {
                            "no longer offered"
                        }
                    ));
                }
            }
        }
        Ok(changes)
    }

    /// Removes plugin `name`, its files and the MCP servers it offered.
    pub fn remove(&self, name: &str, shares: &mut ShareStore) -> Result<Changes> {
        let mut index = self.load()?;
        let before = index.plugins.len();
        index.plugins.retain(|p| p.name != name);
        if index.plugins.len() == before {
            bail!("there is no plugin '{name}' (see plugin list)");
        }
        self.save(&index)?;
        let mut changes = Changes::default();
        for share in &mut shares.shares {
            let gone: Vec<String> = share
                .mcp
                .iter()
                .filter(|(_, d)| d.from.as_ref().is_some_and(|f| f.plugin == name))
                .map(|(n, _)| n.clone())
                .collect();
            for server in gone {
                share.mcp.remove(&server);
                changes.shares_changed = true;
                changes
                    .lines
                    .push(format!("MCP server {server} removed from '{}'", share.name));
            }
        }
        let _ = std::fs::remove_dir_all(self.plugin_dir(name));
        let _ = std::fs::remove_dir_all(self.data_dir(name));
        Ok(changes)
    }

    /// The skills of enabled plugins: (plugin, skill, its SKILL.md).
    pub fn skills(&self) -> Vec<(String, PluginSkill, PathBuf)> {
        let Ok(index) = self.load() else {
            return Vec::new();
        };
        index
            .plugins
            .into_iter()
            .filter(|p| p.enabled)
            .flat_map(|p| {
                let dir = self.plugin_dir(&p.name);
                p.skills
                    .into_iter()
                    .map(move |s| {
                        let path = dir.join(&s.path);
                        (p.name.clone(), s, path)
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// The agents of enabled plugins: (plugin, agent, its file).
    pub fn agents(&self) -> Vec<(String, String, PathBuf)> {
        let Ok(index) = self.load() else {
            return Vec::new();
        };
        index
            .plugins
            .into_iter()
            .filter(|p| p.enabled)
            .flat_map(|p| {
                let dir = self.plugin_dir(&p.name);
                p.agents
                    .into_iter()
                    .map(move |a| (p.name.clone(), a.name, dir.join(&a.path)))
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// Server `server` of plugin `plugin`, filled in for folder `folder` with
    /// `values`; the result remembers where it came from.
    pub fn instantiate(
        &self,
        plugin: &str,
        server: &str,
        folder: &Path,
        values: &BTreeMap<String, String>,
    ) -> Result<McpServer> {
        let p = self.get(plugin)?;
        if !p.enabled {
            bail!("plugin '{plugin}' is turned off; turn it on first");
        }
        let template = p.mcp.get(server).with_context(|| {
            format!(
                "plugin '{plugin}' has no MCP server '{server}' (it has: {})",
                if p.mcp.is_empty() {
                    "none".into()
                } else {
                    p.mcp.keys().cloned().collect::<Vec<_>>().join(", ")
                }
            )
        })?;
        let root = self.plugin_dir(plugin);
        let data = self.data_dir(plugin);
        let mut def = template.instantiate(&Fill {
            plugin_root: Some(&root),
            data_dir: Some(&data),
            folder,
            values,
        })?;
        def.from = Some(McpOrigin {
            plugin: plugin.to_owned(),
            server: server.to_owned(),
            values: values.clone(),
        });
        Ok(def)
    }
}

/// Makes plugin `plugin`'s offered MCP servers again from its current templates,
/// keeping each offer's name, access, tools and values. An offer whose template is
/// gone is removed; one whose command, arguments, environment, URL or headers
/// changed is paused until its owner looks at it and resumes it.
pub fn refresh_offers(
    store: &PluginStore,
    plugin: &Plugin,
    shares: &mut ShareStore,
) -> Result<Changes> {
    let mut changes = Changes::default();
    for share in &mut shares.shares {
        let folder = share.path.clone();
        let mut gone = Vec::new();
        for (name, def) in share.mcp.iter_mut() {
            let Some(origin) = def.from.clone().filter(|f| f.plugin == plugin.name) else {
                continue;
            };
            if !plugin.mcp.contains_key(&origin.server) {
                gone.push(name.clone());
                continue;
            }
            let mut fresh =
                match store.instantiate(&plugin.name, &origin.server, &folder, &origin.values) {
                    Ok(f) => f,
                    Err(e) => {
                        def.paused = true;
                        changes.shares_changed = true;
                        changes.lines.push(format!(
                            "MCP server {name} in '{}' is paused: {e:#}",
                            share.name
                        ));
                        continue;
                    }
                };
            fresh.access = def.access;
            fresh.tools = def.tools.clone();
            fresh.description = def.description.clone();
            fresh.paused = def.paused;
            fresh.plugin_off = def.plugin_off;
            if !fresh.same_connection(def) {
                fresh.paused = true;
                changes.lines.push(format!(
                    "MCP server {name} in '{}' starts differently now ({}): paused until you review and resume it",
                    share.name,
                    fresh.describe()
                ));
            }
            if fresh != *def {
                *def = fresh;
                changes.shares_changed = true;
            }
        }
        for name in gone {
            share.mcp.remove(&name);
            changes.shares_changed = true;
            changes.lines.push(format!(
                "MCP server {name} removed from '{}': the plugin no longer has it",
                share.name
            ));
        }
    }
    Ok(changes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, rel: &str, text: &str) {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// A Claude Code style plugin: manifest, a skill with a helper file, an MCP
    /// server with a key from userConfig, an agent and a hook.
    fn claude_plugin(dir: &Path) {
        write(
            dir,
            ".claude-plugin/plugin.json",
            r#"{"name": "web-tools", "version": "1.2.0", "description": "Browse and test web apps",
                "userConfig": {"api_key": {"description": "Search API key", "sensitive": true}}}"#,
        );
        write(
            dir,
            "skills/smoke-test/SKILL.md",
            "---\nname: smoke-test\ndescription: >\n  Check a web app's main pages\n  load without errors\n---\n# Smoke test\nOpen each page.",
        );
        write(dir, "skills/smoke-test/pages.txt", "/\n/login\n");
        write(
            dir,
            ".mcp.json",
            r#"{"mcpServers": {
                "browser": {"command": "node", "args": ["${CLAUDE_PLUGIN_ROOT}/server.js", "--root", "${workspaceFolder}"]},
                "search": {"type": "http", "url": "https://search.example.com/mcp", "headers": {"Authorization": "Bearer ${user_config.api_key}"}},
                "socket": {"type": "ws", "url": "wss://x.example.com"}
            }}"#,
        );
        write(
            dir,
            "agents/helper.md",
            "---\nname: helper\ndescription: Checks the site\nskills: [smoke-test]\nmcp-servers: [browser, ghost]\ntools: Read\n---\nHelp.",
        );
        write(dir, "hooks/hooks.json", "{}");
    }

    #[test]
    fn sources_are_folders_github_repositories_or_git_urls() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            Source::parse(dir.path().to_str().unwrap()).unwrap(),
            Source::Folder(dir.path().into())
        );
        let gh = Source::parse("acme/tools:plugins/web").unwrap();
        assert_eq!(
            gh,
            Source::Git {
                url: "https://github.com/acme/tools.git".into(),
                path: Some("plugins/web".into()),
                host: Some("github.com".into())
            }
        );
        assert_eq!(gh.spec(), "acme/tools:plugins/web");
        let ghe = Source::parse("acme.ghe.com/team/tools").unwrap();
        assert_eq!(ghe.spec(), "acme.ghe.com/team/tools");
        let url = Source::parse("https://git.example.com/x/y.git#sub").unwrap();
        assert_eq!(url.spec(), "https://git.example.com/x/y.git#sub");
        assert_eq!(
            Source::parse("acme/tools")
                .unwrap()
                .join("./plugins/a")
                .unwrap()
                .spec(),
            "acme/tools:plugins/a"
        );
        for bad in [
            "",
            "nope",
            "acme/tools:../x",
            "http://example.com/x.git",
            "a/b/c/d",
            "acme/.hidden",
        ] {
            assert!(Source::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn plugins_are_inspected_in_every_layout_and_import_runs_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("web-tools");
        claude_plugin(&src);
        let Inspection::Plugin(p) = inspect(&src).unwrap() else {
            panic!()
        };
        assert_eq!(
            (p.name.as_str(), p.version.as_str(), p.format.as_str()),
            ("web-tools", "1.2.0", "Claude Code plugin")
        );
        assert_eq!(p.skills.len(), 1);
        assert_eq!(
            p.skills[0].description,
            "Check a web app's main pages load without errors"
        );
        assert_eq!(p.skills[0].path, "skills/smoke-test/SKILL.md");
        assert_eq!(p.mcp.keys().collect::<Vec<_>>(), ["browser", "search"]);
        let search = &p.mcp["search"];
        assert_eq!(search.inputs.len(), 1);
        assert!(search.inputs[0].secret && search.inputs[0].description == "Search API key");
        let notes = p.notes.join("\n");
        assert!(
            notes.contains("Not used here: hooks (they never run here)"),
            "{notes}"
        );
        // Its agent, with the skills it follows and the MCP servers it uses.
        assert_eq!(
            p.agents,
            [PluginAgent {
                name: "helper".into(),
                description: "Checks the site".into(),
                path: "agents/helper.md".into(),
                skills: vec!["smoke-test".into()],
                mcp: vec!["browser".into(), "ghost".into()],
            }]
        );
        assert!(
            notes.contains(
                "agent 'helper' uses MCP server 'ghost', which this plugin doesn't bring"
            ),
            "{notes}"
        );
        assert!(
            notes.contains("agent 'helper': its tools list is left out"),
            "{notes}"
        );
        assert!(notes.contains("socket: left out: WebSocket"), "{notes}");
        assert!(
            notes.contains("only SKILL.md's text reaches peers, not its 1 other file"),
            "{notes}"
        );

        // Agent Plugins: root plugin.json with $schema, skills/ and mcp.json only.
        let ap = dir.path().join("ap");
        write(
            &ap,
            "plugin.json",
            r#"{"$schema": "https://agent-plugins.org/schemas/1.0.0/plugin.schema.json", "name": "deploy-kit"}"#,
        );
        write(
            &ap,
            "skills/deploy/SKILL.md",
            "---\nname: deploy\ndescription: Deploy safely\n---\nSteps.",
        );
        write(
            &ap,
            "mcp.json",
            r#"{"$schema": "x", "mcpServers": {"cloud": {"type": "streamable-http", "url": "https://c.example.com/mcp"}}}"#,
        );
        write(
            &ap,
            ".mcp.json",
            r#"{"mcpServers": {"ignored": {"command": "x"}}}"#,
        );
        let Inspection::Plugin(ap) = inspect(&ap).unwrap() else {
            panic!()
        };
        assert_eq!(ap.format, "Agent Plugins");
        assert_eq!(ap.mcp.keys().collect::<Vec<_>>(), ["cloud"]);

        // A bare folder of skills; a marketplace; nothing usable.
        let bare = dir.path().join("my skills");
        write(&bare, "skills/house-style/SKILL.md", "Write plainly.");
        let Inspection::Plugin(bare) = inspect(&bare).unwrap() else {
            panic!()
        };
        assert_eq!(
            (bare.name.as_str(), bare.skills[0].name.as_str()),
            ("my-skills", "house-style")
        );
        let market = dir.path().join("market");
        write(
            &market,
            ".github/plugin/marketplace.json",
            r#"{"name": "acme", "owner": {"name": "Acme"}, "plugins": [
                {"name": "web-tools", "description": "Web", "source": "./plugins/web-tools"},
                {"name": "remote", "source": {"source": "github", "repo": "acme/remote"}},
                {"name": "odd", "source": {"source": "npm", "package": "x"}}]}"#,
        );
        let Inspection::Marketplace { name, plugins } = inspect(&market).unwrap() else {
            panic!()
        };
        assert_eq!(name, "acme");
        assert_eq!(
            plugins[0].source,
            MarketSource::Relative("./plugins/web-tools".into())
        );
        assert_eq!(plugins[1].source, MarketSource::Other("acme/remote".into()));
        assert!(matches!(plugins[2].source, MarketSource::Unsupported(_)));
        let empty = dir.path().join("empty");
        write(&empty, "plugin.json", r#"{"name": "hooks-only"}"#);
        write(&empty, "hooks.json", "{}");
        let err = inspect(&empty).unwrap_err().to_string();
        assert!(err.contains("no skills") && err.contains("hooks"), "{err}");
    }

    #[tokio::test]
    async fn microsofts_work_iq_plugin_offers_work_iq_through_the_agents_own_gateway() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("workiq");
        // The shape of microsoft/work-iq:plugins/workiq.
        write(
            &src,
            ".github/plugin/plugin.json",
            r#"{"name": "workiq", "version": "3.0.0", "description": "WorkIQ for Microsoft 365", "skills": "./skills/"}"#,
        );
        write(
            &src,
            "skills/workiq/SKILL.md",
            "---\nname: workiq\ndescription: Find evidence in Microsoft 365 with retrieve\n---\nUse retrieve.",
        );
        write(
            &src,
            ".mcp.json",
            r#"{"mcpServers": {"workiq": {"type": "http", "url": "https://workiq.svc.cloud.microsoft/mcp",
                "oauthClientId": "ba081686-5d24-4bc6-a0d6-d034ecffed87", "oauthPublicClient": true,
                "auth": {"redirectPort": 12798}}}}"#,
        );
        let Inspection::Plugin(p) = inspect(&src).unwrap() else {
            panic!()
        };
        assert_eq!(p.skills.len(), 1, "its skills are kept");
        let template = &p.mcp["workiq"];
        assert!(template.workiq && template.url.is_none() && template.inputs.is_empty());
        let notes = p.notes.join("\n");
        assert!(
            notes.contains("Work IQ, offered through this agent's own Work IQ sign-in"),
            "{notes}"
        );
        assert!(!notes.contains("sign-in helpers aren't used"), "{notes}");

        // The CLI's server is Work IQ too; another server isn't.
        let (cli, _) = crate::mcp::parse_config(
            &serde_json::json!({"mcpServers": {
                "workiq": {"command": "npx", "args": ["-y", "@microsoft/workiq@latest", "mcp"]},
                "local": {"command": "workiq.cmd", "args": ["mcp"]},
                "other": {"command": "npx", "args": ["-y", "@acme/mcp"]}
            }}),
            &BTreeMap::new(),
        )
        .unwrap();
        assert!(cli["workiq"].workiq && cli["local"].workiq && !cli["other"].workiq);

        // Offered in a folder, it is this agent's Work IQ server, named workiq.
        let store = PluginStore::new(dir.path().join("profile"));
        let fetched = fetch(
            &Source::parse(src.to_str().unwrap()).unwrap(),
            &store.staging(),
            None,
        )
        .await
        .unwrap();
        store.install(&fetched, None).unwrap();
        let app = dir.path().join("app");
        std::fs::create_dir(&app).unwrap();
        let mut shares = ShareStore::with_data(dir.path().join("data"));
        shares.add("app", &app, crate::Access::ReadOnly).unwrap();
        let server = store
            .instantiate("workiq", "workiq", &app, &BTreeMap::new())
            .unwrap();
        assert!(server.workiq && server.url.is_none() && server.headers.is_empty());
        assert_eq!(server.from.as_ref().unwrap().plugin, "workiq");
        let err = shares
            .add_mcp("app", "m365", server.clone())
            .unwrap_err()
            .to_string();
        assert!(err.contains("is called 'workiq'"), "{err}");
        shares.add_mcp("app", "workiq", server).unwrap();
    }

    #[tokio::test]
    async fn plugins_install_offer_update_disable_and_remove() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("web-tools");
        claude_plugin(&src);
        write(&src, "server.js", "// a server\n");
        let profile = dir.path().join("profile");
        let store = PluginStore::new(&profile);
        let fetched = fetch(
            &Source::parse(src.to_str().unwrap()).unwrap(),
            &store.staging(),
            None,
        )
        .await
        .unwrap();
        let plugin = store.install(&fetched, None).unwrap();
        assert!(store.plugin_dir("web-tools").join("server.js").is_file());
        assert!(store.install(&fetched, None).is_err(), "names are unique");
        assert_eq!((store.skills().len(), store.agents().len()), (1, 1));
        assert_eq!(plugin.source, src.display().to_string());

        // Offer the browser in a folder: placeholders filled, origin remembered.
        let app = dir.path().join("app");
        std::fs::create_dir(&app).unwrap();
        let mut shares = ShareStore::with_data(dir.path().join("data"));
        shares.add("app", &app, crate::Access::Full).unwrap();
        let folder = shares.get("app").unwrap().path.clone();
        let browser = store
            .instantiate("web-tools", "browser", &folder, &BTreeMap::new())
            .unwrap();
        let root = store.plugin_dir("web-tools");
        assert_eq!(browser.args[0], format!("{}/server.js", root.display()));
        assert_eq!(browser.args[2], folder.display().to_string());
        assert_eq!(browser.env["PLUGIN_ROOT"], root.display().to_string());
        shares.add_mcp("app", "browser", browser).unwrap();
        let err = store
            .instantiate("web-tools", "search", &folder, &BTreeMap::new())
            .unwrap_err();
        assert!(
            err.to_string().contains("needs a value for api_key"),
            "{err}"
        );
        let key = BTreeMap::from([("api_key".to_owned(), "k-123".to_owned())]);
        let search = store
            .instantiate("web-tools", "search", &folder, &key)
            .unwrap();
        assert_eq!(search.headers["Authorization"], "Bearer k-123");
        shares.add_mcp("app", "search", search).unwrap();

        // Turning it off hides its skills and agents and stops offering its servers.
        let off = store.set_enabled("web-tools", false, &mut shares).unwrap();
        assert!(off.shares_changed && store.skills().is_empty() && store.agents().is_empty());
        assert!(
            shares.visible(&["app".into()], crate::Access::Full)[0]
                .mcp
                .is_empty()
        );
        store.set_enabled("web-tools", true, &mut shares).unwrap();
        assert_eq!(
            shares.visible(&["app".into()], crate::Access::Full)[0].mcp,
            ["browser", "search"]
        );

        // An update that changes how a server starts pauses it; a removed one goes.
        write(
            &src,
            ".mcp.json",
            r#"{"mcpServers": {"browser": {"command": "node", "args": ["${CLAUDE_PLUGIN_ROOT}/server2.js"]}}}"#,
        );
        let fetched = fetch(
            &Source::parse(src.to_str().unwrap()).unwrap(),
            &store.staging(),
            None,
        )
        .await
        .unwrap();
        let (_, changes) = store.update("web-tools", &fetched, &mut shares).unwrap();
        let text = changes.lines.join("\n");
        assert!(
            text.contains("browser in 'app' starts differently now"),
            "{text}"
        );
        assert!(text.contains("search removed from 'app'"), "{text}");
        let app_share = shares.get("app").unwrap();
        assert!(app_share.mcp["browser"].paused && !app_share.mcp.contains_key("search"));

        let removed = store.remove("web-tools", &mut shares).unwrap();
        assert!(removed.lines.iter().any(|l| l.contains("browser removed")));
        assert!(!store.plugin_dir("web-tools").exists() && store.list().unwrap().is_empty());
    }

    #[tokio::test]
    async fn git_sources_are_cloned_and_marketplace_entries_resolved() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        claude_plugin(&repo.join("plugins/web-tools"));
        write(
            &repo,
            ".claude-plugin/marketplace.json",
            r#"{"name": "acme", "owner": {"name": "Acme"}, "plugins": [{"name": "web-tools", "source": "./plugins/web-tools"}]}"#,
        );
        let git = |args: &[&str]| {
            let ok = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .output()
                .unwrap();
            assert!(
                ok.status.success(),
                "{}",
                String::from_utf8_lossy(&ok.stderr)
            );
        };
        git(&["init", "-q"]);
        git(&["add", "-A"]);
        git(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "-m",
            "init",
        ]);
        let url = format!("file://{}", repo.display().to_string().replace('\\', "/"));
        let store = PluginStore::new(dir.path().join("profile"));
        let market = fetch(&Source::parse(&url).unwrap(), &store.staging(), None)
            .await
            .unwrap();
        assert!(market.commit.as_deref().is_some_and(|c| c.len() == 40));
        let Inspection::Marketplace { plugins, .. } = inspect(&market.root).unwrap() else {
            panic!()
        };
        let entry = market
            .entry(&plugins[0], &store.staging(), None)
            .await
            .unwrap();
        let plugin = store.install(&entry, None).unwrap();
        assert_eq!(plugin.source, format!("{url}#plugins/web-tools"));
        assert_eq!(plugin.commit, market.commit);
        let staged = market.root.clone();
        drop(entry);
        drop(market);
        assert!(!staged.exists(), "the clone is removed after review");
    }
}
