//! Skills: reusable instructions for agents (how to bootstrap a project, review
//! security, debug an incident, ...). A plan step names them with `skill=a,b` and
//! the boss puts their text in the step's prompt, so every peer gets the same
//! procedure whichever provider runs it. Built-in skills ship with proofkin
//! (`skills/*.md`); a profile's own skills (`<profile>/skills/<name>.md`) are added
//! to them and replace built-ins of the same name. Enabled plugins add theirs
//! (`SKILL.md`): by name when no other skill has it, always as `<plugin>.<skill>`.
//!
//! A skill is Markdown with front matter:
//!
//! ```text
//! ---
//! name: security-review
//! description: Security review of a change against common vulnerability classes
//! ---
//! # Security review
//! ...
//! ```
//!
//! An agent is a role a plan step can take (`agent=name`): Markdown instructions with
//! front matter naming the skills it follows and the MCP servers it uses, the layout
//! of GitHub Copilot and Claude Code agents (`tools` and `model` are left out: what a
//! step may do is set by the folder's owner, and the peer picks its model):
//!
//! ```text
//! ---
//! name: security-reviewer
//! description: Reviews a change for security problems
//! skills: [security-review, code-review]
//! mcp-servers: [semgrep]
//! ---
//! You review changes the way a security engineer would...
//! ```
//!
//! A profile's own agents live in `<profile>/agents/<name>.md`; enabled plugins add
//! theirs (`agents/*.md`), by name when no other agent has it, always as
//! `<plugin>.<agent>`. A plugin agent's skills are its plugin's skills first.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::profile::validate_name;

/// The longest skill read from a file (it goes into prompts).
const MAX_SKILL_BYTES: u64 = 64 * 1024;

const BUILT_IN: &[(&str, &str)] = &[
    (
        "bootstrap-project",
        include_str!("../skills/bootstrap-project.md"),
    ),
    (
        "reverse-engineer",
        include_str!("../skills/reverse-engineer.md"),
    ),
    (
        "build-run-discovery",
        include_str!("../skills/build-run-discovery.md"),
    ),
    ("risk-review", include_str!("../skills/risk-review.md")),
    ("design-doc", include_str!("../skills/design-doc.md")),
    (
        "implement-change",
        include_str!("../skills/implement-change.md"),
    ),
    ("code-review", include_str!("../skills/code-review.md")),
    (
        "security-review",
        include_str!("../skills/security-review.md"),
    ),
    (
        "privacy-review",
        include_str!("../skills/privacy-review.md"),
    ),
    (
        "compliance-check",
        include_str!("../skills/compliance-check.md"),
    ),
    ("ops-insights", include_str!("../skills/ops-insights.md")),
    ("tsg-writing", include_str!("../skills/tsg-writing.md")),
    (
        "incident-debugging",
        include_str!("../skills/incident-debugging.md"),
    ),
    ("write-spec", include_str!("../skills/write-spec.md")),
    (
        "agent-blueprint",
        include_str!("../skills/agent-blueprint.md"),
    ),
    ("agent-evals", include_str!("../skills/agent-evals.md")),
    ("api-contract", include_str!("../skills/api-contract.md")),
    ("prototype-ux", include_str!("../skills/prototype-ux.md")),
    (
        "spec-to-claims",
        include_str!("../skills/spec-to-claims.md"),
    ),
    (
        "architecture-design",
        include_str!("../skills/architecture-design.md"),
    ),
    ("adr", include_str!("../skills/adr.md")),
    ("data-design", include_str!("../skills/data-design.md")),
    (
        "threat-modeling",
        include_str!("../skills/threat-modeling.md"),
    ),
    ("test-strategy", include_str!("../skills/test-strategy.md")),
    (
        "operability-design",
        include_str!("../skills/operability-design.md"),
    ),
    (
        "review-technical",
        include_str!("../skills/review-technical.md"),
    ),
    (
        "review-product",
        include_str!("../skills/review-product.md"),
    ),
    (
        "review-leadership",
        include_str!("../skills/review-leadership.md"),
    ),
    (
        "review-customer",
        include_str!("../skills/review-customer.md"),
    ),
    (
        "stakeholder-simulation",
        include_str!("../skills/stakeholder-simulation.md"),
    ),
    (
        "feedback-synthesis",
        include_str!("../skills/feedback-synthesis.md"),
    ),
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkillInfo {
    pub name: String,
    pub description: String,
    /// `built-in`, `profile` or `plugin`.
    pub source: &'static str,
    /// The plugin it comes from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugin: Option<String>,
}

/// A skill of an enabled plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginSkillRef {
    pub plugin: String,
    pub name: String,
    pub description: String,
    pub path: PathBuf,
}

/// Reads a skill to import: a `SKILL.md`, a folder with one, or a Markdown file.
/// Returns its name (from its front matter, else its folder or file name) and text.
pub fn read_skill_file(path: &Path) -> Result<(String, String)> {
    let file = if path.is_dir() {
        path.join("SKILL.md")
    } else {
        path.to_path_buf()
    };
    let meta = std::fs::metadata(&file).with_context(|| {
        format!(
            "{} has no SKILL.md or isn't a Markdown file",
            path.display()
        )
    })?;
    if meta.len() > MAX_SKILL_BYTES {
        bail!(
            "{} is over 64 KB; skills go into prompts, keep them short",
            file.display()
        );
    }
    let text =
        std::fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
    let text = text.trim_start_matches('\u{feff}').to_owned();
    let fallback = if file.file_name().is_some_and(|n| n == "SKILL.md") {
        file.parent().and_then(|p| p.file_name())
    } else {
        file.file_stem()
    }
    .and_then(|n| n.to_str())
    .unwrap_or_default()
    .to_owned();
    let name = front_matter_name(&text).unwrap_or(fallback);
    validate_name(&name)
        .with_context(|| format!("the skill's name {name:?} isn't usable; rename it"))?;
    Ok((name, text))
}

/// The `name:` in a skill's front matter, if it has one.
pub fn front_matter_name(text: &str) -> Option<String> {
    text.trim_start_matches('\u{feff}')
        .strip_prefix("---")
        .and_then(|r| r.split_once("\n---"))
        .and_then(|(head, _)| {
            head.lines()
                .find_map(|l| l.strip_prefix("name:"))
                .map(|n| n.trim().trim_matches(['"', '\'']).to_owned())
        })
        .filter(|n| !n.is_empty())
}

/// Splits front matter from the body; returns (description, body).
fn parse(text: &str) -> (String, &str) {
    let Some(rest) = text.strip_prefix("---") else {
        return (String::new(), text);
    };
    let Some((head, body)) = rest.split_once("\n---") else {
        return (String::new(), text);
    };
    let description = head
        .lines()
        .find_map(|l| l.strip_prefix("description:"))
        .map(|d| d.trim().to_owned())
        .unwrap_or_default();
    (description, body.trim_start_matches(['\r', '\n']))
}

/// Front matter as `key -> values`, and the body after it. A value is `key: value`
/// (a list for list keys: `[a, b]` or `a, b`), or a block under the key: `- item`
/// lines, or a map whose keys are the values (`mcp-servers:` with a server per key).
pub fn front_matter(text: &str) -> (BTreeMap<String, Vec<String>>, &str) {
    const ONE_VALUE: &[&str] = &["name", "description", "model"];
    let text = text.trim_start_matches('\u{feff}');
    let mut fields: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let Some((head, body)) = text
        .strip_prefix("---")
        .and_then(|rest| rest.split_once("\n---"))
    else {
        return (fields, text);
    };
    let unquote = |v: &str| v.trim().trim_matches(['"', '\'']).trim().to_owned();
    let list = |v: &str| -> Vec<String> {
        v.split(',')
            .map(unquote)
            .filter(|v| !v.is_empty())
            .collect()
    };
    let mut key: Option<String> = None;
    let mut indent_of_block: Option<usize> = None;
    for line in head.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        // A list may sit at the key's own indent (`skills:` then `- a`).
        if indent == 0 && !trimmed.starts_with('-') {
            indent_of_block = None;
            key = None;
            let Some((k, v)) = trimmed.split_once(':') else {
                continue;
            };
            let (k, v) = (k.trim().to_owned(), v.trim());
            let values = if let Some(inner) = v.strip_prefix('[').and_then(|v| v.strip_suffix(']'))
            {
                list(inner)
            } else if v.is_empty() {
                Vec::new()
            } else if ONE_VALUE.contains(&k.as_str()) {
                vec![unquote(v)]
            } else {
                list(v)
            };
            fields.insert(k.clone(), values);
            key = Some(k);
        } else if let Some(k) = &key {
            if indent_of_block.is_some_and(|i| indent > i) {
                continue;
            }
            indent_of_block = Some(indent);
            let entry = fields.entry(k.clone()).or_default();
            if let Some(item) = trimmed.strip_prefix('-') {
                entry.push(unquote(item));
            } else if let Some((map_key, _)) = trimmed.split_once(':') {
                entry.push(unquote(map_key));
            }
        }
    }
    // The body starts on the line after the closing `---`.
    let body = body.split_once('\n').map_or("", |(_, rest)| rest);
    (fields, body.trim_start_matches(['\r', '\n']))
}

/// A role a plan step can take: its instructions, the skills it follows and the MCP
/// servers it uses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentDef {
    pub name: String,
    pub description: String,
    pub skills: Vec<String>,
    pub mcp: Vec<String>,
    pub instructions: String,
    /// The model its file names (`model:`), for steps with it that name none (if their
    /// peer allows it); `inherit`, and names that aren't model names, are left out.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// `profile`, `factory` or `plugin`.
    pub source: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugin: Option<String>,
    /// For a factory agent: the release it is (its active one, or the one named).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// For a deprecated factory agent: what plans should use instead.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deprecated: Option<String>,
    /// What its file has that isn't used here.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// Reads an agent file: its front matter (name, description, skills, MCP servers) and
/// instructions. `fallback` names it when the front matter doesn't.
pub fn parse_agent(text: &str, fallback: &str) -> Result<AgentDef> {
    let (fields, body) = front_matter(text);
    let one = |key: &str| {
        fields
            .get(key)
            .and_then(|v| v.first())
            .cloned()
            .unwrap_or_default()
    };
    let name = Some(one("name"))
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| fallback.to_owned());
    validate_name(&name)
        .with_context(|| format!("the agent's name {name:?} isn't usable; rename it"))?;
    let skills: Vec<String> = ["skills", "skill"]
        .iter()
        .flat_map(|k| fields.get(*k).cloned().unwrap_or_default())
        .collect();
    let mcp: Vec<String> = ["mcp-servers", "mcpServers", "mcp"]
        .iter()
        .flat_map(|k| fields.get(*k).cloned().unwrap_or_default())
        .collect();
    for (what, names) in [("skill", &skills), ("MCP server", &mcp)] {
        if let Some(bad) = names.iter().find(|n| validate_name(n).is_err()) {
            bail!("agent '{name}' names the {what} {bad:?}, which isn't a usable name");
        }
    }
    let mut notes = Vec::new();
    if fields.contains_key("tools") {
        notes.push(
            "its tools list is left out: what a step may do is set by the folder's owner".into(),
        );
    }
    let model = match one("model").trim() {
        "" => None,
        m if m.eq_ignore_ascii_case("inherit") => None,
        m if crate::models::check_name(m).is_ok() => Some(m.to_owned()),
        m => {
            notes.push(format!("its model {m:?} isn't a model name: left out"));
            None
        }
    };
    let instructions = body.trim().to_owned();
    if instructions.is_empty() {
        bail!("agent '{name}' has no instructions after its front matter");
    }
    Ok(AgentDef {
        name,
        description: one("description"),
        skills,
        mcp,
        instructions,
        model,
        source: "profile",
        plugin: None,
        version: None,
        deprecated: None,
        notes,
    })
}

/// An agent of an enabled plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginAgentRef {
    pub plugin: String,
    pub name: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct Skills {
    dir: PathBuf,
    plugins: Vec<PluginSkillRef>,
    /// The profile's own agents, next to its skills (`<profile>/agents`).
    agents_dir: PathBuf,
    plugin_agents: Vec<PluginAgentRef>,
    /// The profile's agent factory: its agents' releases (see [`crate::factory::release`]).
    factory: Option<crate::factory::FactoryStore>,
}

impl Skills {
    /// The skills (and agents) of a profile whose own skills live in `dir`.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        Self {
            agents_dir: dir.with_file_name("agents"),
            dir,
            plugins: Vec::new(),
            plugin_agents: Vec::new(),
            factory: None,
        }
    }

    /// With the released agents of the profile's agent factory, after its own agents and
    /// before its plugins'.
    pub fn with_factory(mut self, store: crate::factory::FactoryStore) -> Self {
        self.factory = Some(store);
        self
    }

    /// With the agents of enabled plugins: (plugin, agent, its file).
    pub fn with_plugin_agents(mut self, agents: Vec<(String, String, PathBuf)>) -> Self {
        self.plugin_agents = agents
            .into_iter()
            .map(|(plugin, name, path)| PluginAgentRef { plugin, name, path })
            .collect();
        self
    }

    /// With the skills of enabled plugins: (plugin, skill, its `SKILL.md`).
    pub fn with_plugins(
        mut self,
        skills: Vec<(String, crate::plugins::PluginSkill, PathBuf)>,
    ) -> Self {
        self.plugins = skills
            .into_iter()
            .map(|(plugin, s, path)| PluginSkillRef {
                plugin,
                name: s.name,
                description: s.description,
                path,
            })
            .collect();
        self
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn own(&self, name: &str) -> Option<String> {
        validate_name(name).ok()?;
        std::fs::read_to_string(self.dir.join(format!("{name}.md"))).ok()
    }

    fn built_in(name: &str) -> Option<&'static str> {
        BUILT_IN.iter().find(|(n, _)| *n == name).map(|(_, t)| *t)
    }

    /// The plugin skill `name` refers to: `<plugin>.<skill>`, or a short name no
    /// built-in or profile skill has.
    fn plugin(&self, name: &str) -> Option<&PluginSkillRef> {
        self.plugins
            .iter()
            .find(|s| format!("{}.{}", s.plugin, s.name) == name)
            .or_else(|| self.plugins.iter().find(|s| s.name == name))
    }

    /// The skill's full text (front matter included).
    pub fn text(&self, name: &str) -> Result<String> {
        if let Some(text) = self.own(name) {
            return Ok(text);
        }
        if let Some(text) = Self::built_in(name) {
            return Ok(text.to_owned());
        }
        if let Some(skill) = self.plugin(name) {
            let meta = std::fs::metadata(&skill.path).with_context(|| {
                format!(
                    "skill '{name}' of plugin '{}' is missing; update the plugin",
                    skill.plugin
                )
            })?;
            if meta.len() > MAX_SKILL_BYTES {
                bail!("skill '{name}' of plugin '{}' is over 64 KB", skill.plugin);
            }
            return std::fs::read_to_string(&skill.path)
                .with_context(|| format!("reading skill '{name}' of plugin '{}'", skill.plugin));
        }
        bail!("there is no skill '{name}' (see proofkin skill list)")
    }

    pub fn list(&self) -> Vec<SkillInfo> {
        let mut skills: Vec<SkillInfo> = BUILT_IN
            .iter()
            .map(|(name, text)| SkillInfo {
                name: (*name).to_owned(),
                description: parse(text).0,
                source: "built-in",
                plugin: None,
            })
            .collect();
        if let Ok(entries) = std::fs::read_dir(&self.dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let Some(name) = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .filter(|_| path.extension().is_some_and(|e| e == "md"))
                    .filter(|n| validate_name(n).is_ok())
                else {
                    continue;
                };
                let text = std::fs::read_to_string(&path).unwrap_or_default();
                skills.retain(|s| s.name != name);
                skills.push(SkillInfo {
                    name: name.to_owned(),
                    description: parse(&text).0,
                    source: "profile",
                    plugin: None,
                });
            }
        }
        for skill in &self.plugins {
            let short_taken = skills.iter().any(|s| s.name == skill.name);
            skills.push(SkillInfo {
                name: if short_taken {
                    format!("{}.{}", skill.plugin, skill.name)
                } else {
                    skill.name.clone()
                },
                description: skill.description.clone(),
                source: "plugin",
                plugin: Some(skill.plugin.clone()),
            });
        }
        skills.sort_by(|a, b| a.name.cmp(&b.name));
        skills
    }

    /// Adds or replaces a profile skill.
    pub fn add(&self, name: &str, text: &str) -> Result<()> {
        validate_name(name).context("invalid skill name")?;
        if text.trim().is_empty() {
            bail!("the skill is empty");
        }
        if text.len() as u64 > MAX_SKILL_BYTES {
            bail!("the skill is over 64 KB; skills go into prompts, keep them short");
        }
        std::fs::create_dir_all(&self.dir)?;
        std::fs::write(self.dir.join(format!("{name}.md")), text)?;
        Ok(())
    }

    /// Removes a profile skill (built-ins can't be removed, only replaced; a
    /// plugin's go with their plugin).
    pub fn remove(&self, name: &str) -> Result<bool> {
        validate_name(name).context("invalid skill name")?;
        match std::fs::remove_file(self.dir.join(format!("{name}.md"))) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if let Some(skill) = self.plugin(name) {
                    bail!(
                        "skill '{name}' comes with plugin '{}': turn the plugin off or remove it",
                        skill.plugin
                    );
                }
                if Self::built_in(name).is_some() {
                    bail!("'{name}' is built in: it can be replaced (skill add) but not removed");
                }
                Ok(false)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// The instructions for a prompt: each skill's body under a heading.
    pub fn instructions(&self, names: &[String]) -> Result<String> {
        let mut text = String::new();
        for name in names {
            let full = self.text(name)?;
            text += &format!("\n### Skill: {name}\n{}\n", parse(&full).1.trim());
        }
        Ok(text)
    }

    /// A file that goes into prompts, at most 64 KB.
    fn read_prompt_file(path: &Path, what: &str) -> Result<String> {
        let meta = std::fs::metadata(path).with_context(|| format!("{what} is missing"))?;
        if meta.len() > MAX_SKILL_BYTES {
            bail!("{what} is over 64 KB; it goes into prompts, keep it short");
        }
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {what}"))?;
        Ok(text.trim_start_matches('\u{feff}').to_owned())
    }

    fn own_agent(&self, name: &str) -> Option<PathBuf> {
        validate_name(name).ok()?;
        Some(self.agents_dir.join(format!("{name}.md"))).filter(|p| p.is_file())
    }

    /// The plugin agent `name` refers to: `<plugin>.<agent>`, or a short name no
    /// profile agent has.
    fn plugin_agent(&self, name: &str) -> Option<&PluginAgentRef> {
        self.plugin_agents
            .iter()
            .find(|a| format!("{}.{}", a.plugin, a.name) == name)
            .or_else(|| {
                (self.own_agent(name).is_none())
                    .then(|| self.plugin_agents.iter().find(|a| a.name == name))
                    .flatten()
            })
    }

    /// The agent `name` refers to (named so): the profile's own, else the factory's active
    /// release of it, else a plugin's; `name@version` is that release of a factory agent. A
    /// plugin agent's skills are its plugin's skills when it has one by that name. A retired
    /// factory agent is refused, with its replacement.
    pub fn agent(&self, name: &str) -> Result<AgentDef> {
        if let Some((base, version)) = name.split_once('@') {
            return self.factory_agent(base, Some(version))?.ok_or_else(|| {
                anyhow::anyhow!("there is no factory agent '{base}' (see proofkin factory list)")
            });
        }
        if let Some(path) = self.own_agent(name) {
            let mut def = parse_agent(
                &Self::read_prompt_file(&path, &format!("agent '{name}'"))?,
                name,
            )?;
            def.name = name.to_owned();
            return Ok(def);
        }
        if let Some(def) = self.factory_agent(name, None)? {
            return Ok(def);
        }
        let Some(a) = self.plugin_agent(name) else {
            bail!("there is no agent '{name}' (see proofkin agent list)");
        };
        let what = format!("agent '{name}' of plugin '{}'", a.plugin);
        let text = Self::read_prompt_file(&a.path, &what).context("update the plugin")?;
        let mut def = parse_agent(&text, &a.name)?;
        def.name = name.to_owned();
        def.skills = def
            .skills
            .into_iter()
            .map(|s| {
                if self
                    .plugins
                    .iter()
                    .any(|p| p.plugin == a.plugin && p.name == s)
                {
                    format!("{}.{s}", a.plugin)
                } else {
                    s
                }
            })
            .collect();
        def.source = "plugin";
        def.plugin = Some(a.plugin.clone());
        Ok(def)
    }

    /// Factory agent `name`'s release `version` (its active one by default), verified: its
    /// agent file's instructions with its skills', the MCP servers it asks for and the model
    /// it was released on. `None`: the factory has no such agent, or it has no active
    /// release. A retired agent, an unknown version and a release that doesn't verify are
    /// errors.
    fn factory_agent(&self, name: &str, version: Option<&str>) -> Result<Option<AgentDef>> {
        use crate::factory::release;
        let Some(store) = &self.factory else {
            return Ok(None);
        };
        if validate_name(name).is_err() {
            return Ok(None);
        }
        let Some(state) = store.state(name)? else {
            return Ok(None);
        };
        let note = crate::factory::lifecycle::plan_note(&state);
        if let Some((true, why)) = &note {
            bail!("{why}");
        }
        let version = match (version, &state.active_version) {
            (Some(v), _) => v.to_owned(),
            (None, Some(v)) => v.clone(),
            (None, None) => return Ok(None),
        };
        let found = release::load(store, name, &version)?;
        if found.state != release::ReleaseState::Released {
            bail!(
                "release {version} of {name} is {}: plans use released releases",
                found.state
            );
        }
        let build = release::build_of(store, name, &version)?;
        let model = found
            .manifest
            .model
            .chosen
            .model
            .clone()
            .filter(|m| crate::models::check_name(m).is_ok());
        Ok(Some(AgentDef {
            name: name.to_owned(),
            description: found.manifest.purpose.clone(),
            skills: Vec::new(),
            mcp: found
                .manifest
                .capabilities
                .mcp
                .iter()
                .map(|m| m.server.clone())
                .collect(),
            instructions: release::body(&build)?,
            model,
            source: "factory",
            plugin: None,
            version: Some(version),
            deprecated: note.map(|(_, why)| why),
            notes: Vec::new(),
        }))
    }

    /// The factory's agents plans can use: each one's active release.
    fn factory_agents(&self) -> Vec<AgentDef> {
        let Some(store) = &self.factory else {
            return Vec::new();
        };
        store
            .names()
            .unwrap_or_default()
            .iter()
            .filter_map(|name| self.factory_agent(name, None).ok().flatten())
            .collect()
    }

    /// Every agent: the profile's own, then plugins' (by short name when no other
    /// agent has it). One that can't be read is left out.
    pub fn agents(&self) -> Vec<AgentDef> {
        let mut names: Vec<String> = std::fs::read_dir(&self.agents_dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let path = e.path();
                path.extension().is_some_and(|x| x == "md").then_some(())?;
                path.file_stem()?.to_str().map(str::to_owned)
            })
            .filter(|n| validate_name(n).is_ok())
            .collect();
        names.sort();
        let mut agents: Vec<AgentDef> = names.iter().filter_map(|n| self.agent(n).ok()).collect();
        for def in self.factory_agents() {
            if !agents.iter().any(|x| x.name == def.name) {
                agents.push(def);
            }
        }
        for a in &self.plugin_agents {
            let reference = if agents.iter().any(|x| x.name == a.name) {
                format!("{}.{}", a.plugin, a.name)
            } else {
                a.name.clone()
            };
            if let Ok(def) = self.agent(&reference) {
                agents.push(def);
            }
        }
        agents
    }

    /// Adds or replaces a profile agent; the skills it follows must exist.
    pub fn add_agent(&self, name: &str, text: &str) -> Result<AgentDef> {
        validate_name(name).context("invalid agent name")?;
        if text.len() as u64 > MAX_SKILL_BYTES {
            bail!("the agent is over 64 KB; it goes into prompts, keep it short");
        }
        let def = parse_agent(text, name)?;
        for skill in &def.skills {
            self.text(skill)
                .with_context(|| format!("agent '{name}' follows skill '{skill}'"))?;
        }
        std::fs::create_dir_all(&self.agents_dir)?;
        std::fs::write(self.agents_dir.join(format!("{name}.md")), text)?;
        self.agent(name)
    }

    /// Removes a profile agent (a plugin's go with their plugin).
    pub fn remove_agent(&self, name: &str) -> Result<bool> {
        validate_name(name).context("invalid agent name")?;
        match std::fs::remove_file(self.agents_dir.join(format!("{name}.md"))) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if let Some(a) = self.plugin_agent(name) {
                    bail!(
                        "agent '{name}' comes with plugin '{}': turn the plugin off or remove it",
                        a.plugin
                    );
                }
                Ok(false)
            }
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_skills_are_listed_and_profile_skills_replace_them() {
        let dir = tempfile::tempdir().unwrap();
        let skills = Skills::new(dir.path().join("skills"));
        let all = skills.list();
        assert_eq!(all.len(), BUILT_IN.len());
        assert!(
            all.iter()
                .all(|s| !s.description.is_empty() && s.source == "built-in")
        );
        let review = skills.instructions(&["security-review".into()]).unwrap();
        assert!(review.starts_with("\n### Skill: security-review\n# Security review"));
        assert!(!review.contains("description:"), "front matter is left out");

        skills
            .add(
                "security-review",
                "---\nname: security-review\ndescription: Ours\n---\nUse our checklist.",
            )
            .unwrap();
        skills.add("house-style", "Write British English.").unwrap();
        let mine = skills.list();
        let ours = mine.iter().find(|s| s.name == "security-review").unwrap();
        assert_eq!(
            (ours.description.as_str(), ours.source),
            ("Ours", "profile")
        );
        assert!(mine.iter().any(|s| s.name == "house-style"));
        let text = skills
            .instructions(&["security-review".into(), "house-style".into()])
            .unwrap();
        assert!(text.contains("Use our checklist.") && text.contains("Write British English."));
        assert!(skills.instructions(&["nope".into()]).is_err());
        assert!(skills.add("../x", "y").is_err());
        assert!(skills.remove("security-review").unwrap());
        assert!(
            skills
                .text("security-review")
                .unwrap()
                .contains("Security review"),
            "built-in again"
        );
    }

    #[test]
    fn plugin_skills_are_found_by_name_or_as_plugin_dot_skill_and_skill_files_import() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("team");
        for (folder, text) in [
            (
                "release",
                "---\nname: release-notes\ndescription: Notes\n---\nWrite notes.",
            ),
            (
                "review",
                "---\nname: code-review\ndescription: Theirs\n---\nTheir review.",
            ),
        ] {
            std::fs::create_dir_all(root.join(folder)).unwrap();
            std::fs::write(root.join(folder).join("SKILL.md"), text).unwrap();
        }
        let skill = |name: &str, folder: &str| {
            (
                "team".to_owned(),
                crate::plugins::PluginSkill {
                    name: name.into(),
                    description: String::new(),
                    path: format!("{folder}/SKILL.md"),
                },
                root.join(folder).join("SKILL.md"),
            )
        };
        let skills = Skills::new(dir.path().join("skills")).with_plugins(vec![
            skill("release-notes", "release"),
            skill("code-review", "review"),
        ]);
        let use_ = |name: &str| skills.instructions(&[name.to_owned()]).unwrap();
        assert!(
            use_("release-notes").contains("Write notes."),
            "by name when no other skill has it"
        );
        assert!(
            use_("team.code-review").contains("Their review."),
            "always as plugin.skill"
        );
        assert!(
            !use_("code-review").contains("Their review."),
            "the built-in keeps its name"
        );
        let listed = skills.list();
        assert!(
            listed
                .iter()
                .any(|s| s.name == "release-notes" && s.plugin.as_deref() == Some("team"))
        );
        assert!(
            listed
                .iter()
                .any(|s| s.name == "team.code-review" && s.source == "plugin")
        );
        let err = skills.remove("release-notes").unwrap_err().to_string();
        assert!(err.contains("comes with plugin 'team'"), "{err}");

        // Importing: a SKILL.md, the folder holding one, or a Markdown file.
        let (name, text) = read_skill_file(&root.join("release")).unwrap();
        assert_eq!(
            (name.as_str(), text.contains("Write notes.")),
            ("release-notes", true)
        );
        std::fs::write(dir.path().join("house-style.md"), "Write plainly.").unwrap();
        assert_eq!(
            read_skill_file(&dir.path().join("house-style.md"))
                .unwrap()
                .0,
            "house-style"
        );
        std::fs::write(dir.path().join("big.md"), "x".repeat(70 * 1024)).unwrap();
        assert!(
            read_skill_file(&dir.path().join("big.md")).is_err(),
            "over 64 KB"
        );
        assert!(read_skill_file(&dir.path().join("nothing-here")).is_err());
        assert_eq!(
            front_matter_name("---\nname: \"x-y\"\n---\nz"),
            Some("x-y".into())
        );
    }

    #[test]
    fn agent_files_name_their_skills_and_mcp_servers_in_any_list_form() {
        let (fields, body) = front_matter(
            "---\nname: reviewer\ndescription: Reviews things, carefully\nskills: [a, \"b\"]\ntools: Read, Grep\nmcp-servers:\n  semgrep:\n    type: local\n    command: semgrep\n  github:\n    url: x\nskill:\n- c\n- d\n---\n- a list in the body stays\n",
        );
        assert_eq!(fields["description"], ["Reviews things, carefully"]);
        assert_eq!(fields["skills"], ["a", "b"]);
        assert_eq!(fields["skill"], ["c", "d"]);
        assert_eq!(fields["tools"], ["Read", "Grep"]);
        assert_eq!(fields["mcp-servers"], ["semgrep", "github"]);
        assert_eq!(body, "- a list in the body stays\n");

        let def = parse_agent(
            "---\ndescription: d\nskills: [a, b]\nmcp: [semgrep]\nmodel: sonnet\n---\nBe careful.",
            "fallback",
        )
        .unwrap();
        assert_eq!(
            (def.name.as_str(), def.skills.as_slice(), def.mcp.as_slice()),
            (
                "fallback",
                &["a".to_owned(), "b".to_owned()][..],
                &["semgrep".to_owned()][..]
            )
        );
        assert_eq!(def.instructions, "Be careful.");
        // Its model is kept, for steps with it that name none.
        assert_eq!(def.model.as_deref(), Some("sonnet"));
        assert!(def.notes.is_empty(), "{:?}", def.notes);
        let inherit = parse_agent("---\nmodel: inherit\n---\nHi", "x").unwrap();
        assert_eq!(inherit.model, None);
        let odd = parse_agent("---\nmodel: \"two words\"\n---\nHi", "x").unwrap();
        assert_eq!(odd.model, None);
        assert!(
            odd.notes[0].contains("isn't a model name"),
            "{:?}",
            odd.notes
        );
        assert!(
            parse_agent("---\nname: x\n---\n", "x").is_err(),
            "no instructions"
        );
        assert!(parse_agent("---\nskills: [../up]\n---\nHi", "x").is_err());
    }

    #[test]
    fn agents_come_from_the_profile_and_plugins_with_their_plugins_skills() {
        let dir = tempfile::tempdir().unwrap();
        let plugin = dir.path().join("plugin");
        std::fs::create_dir_all(plugin.join("skills/security-review")).unwrap();
        std::fs::create_dir_all(plugin.join("agents")).unwrap();
        std::fs::write(
            plugin.join("skills/security-review/SKILL.md"),
            "---\nname: security-review\ndescription: Ours\n---\nUse the plugin's checklist.",
        )
        .unwrap();
        let agent_file = plugin.join("agents/sec.md");
        std::fs::write(
            &agent_file,
            "---\nname: sec\ndescription: Security reviewer\nskills: [security-review, code-review]\nmcp-servers: [semgrep]\n---\nAct as a security engineer.",
        )
        .unwrap();
        let skill = crate::plugins::PluginSkill {
            name: "security-review".into(),
            description: "Ours".into(),
            path: "skills/security-review/SKILL.md".into(),
        };
        let skills = Skills::new(dir.path().join("profile/skills"))
            .with_plugins(vec![(
                "acme".into(),
                skill,
                plugin.join("skills/security-review/SKILL.md"),
            )])
            .with_plugin_agents(vec![("acme".into(), "sec".into(), agent_file)]);

        // A plugin agent: its plugin's own skill, not the built-in of that name.
        let sec = skills.agent("sec").unwrap();
        assert_eq!(sec.skills, ["acme.security-review", "code-review"]);
        assert_eq!(
            (sec.source, sec.plugin.as_deref()),
            ("plugin", Some("acme"))
        );
        assert!(
            skills
                .instructions(&sec.skills)
                .unwrap()
                .contains("Use the plugin's checklist.")
        );
        assert_eq!(skills.agent("acme.sec").unwrap().name, "acme.sec");

        // The profile's own agent of that name comes first; the plugin's stays reachable.
        assert!(
            skills
                .add_agent("sec", "---\nskills: [nope]\n---\nHi")
                .is_err(),
            "unknown skill"
        );
        let mine = skills
            .add_agent(
                "sec",
                "---\ndescription: Mine\nskills: [code-review]\n---\nBe brief.",
            )
            .unwrap();
        assert_eq!(
            (mine.source, mine.instructions.as_str()),
            ("profile", "Be brief.")
        );
        let names: Vec<(String, &str)> = skills
            .agents()
            .into_iter()
            .map(|a| (a.name, a.source))
            .collect();
        assert_eq!(
            names,
            [
                ("sec".to_owned(), "profile"),
                ("acme.sec".to_owned(), "plugin")
            ]
        );
        assert!(
            skills
                .remove_agent("acme.sec")
                .unwrap_err()
                .to_string()
                .contains("comes with plugin 'acme'")
        );
        assert!(skills.remove_agent("sec").unwrap());
        assert_eq!(skills.agent("sec").unwrap().source, "plugin");
        assert!(skills.agent("ghost").is_err());
    }
}
