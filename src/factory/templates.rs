//! Built-in agent templates (docs/agent-factory.md, section 2): blueprints with starter
//! eval suites, in `factory/templates/*.json`, embedded in the binary like the built-in
//! skills. Each asks for the least access it needs and has at least three cases,
//! covering every requirement. [`instantiate`] makes an agent's first draft from one.
//! A template whose cases work on files brings them, from
//! `factory/templates/<name>/fixtures/`: [`write_fixtures`] puts them in the agent's
//! fixtures folder.

use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;

use crate::factory::blueprint::{Catalog, from_value};
use crate::llm::{Access, Provider};

const BUILT_IN: &[(&str, &str)] = &[
    (
        "code-reviewer",
        include_str!("../../factory/templates/code-reviewer.json"),
    ),
    (
        "implementer",
        include_str!("../../factory/templates/implementer.json"),
    ),
    (
        "researcher",
        include_str!("../../factory/templates/researcher.json"),
    ),
    (
        "document-extractor",
        include_str!("../../factory/templates/document-extractor.json"),
    ),
    (
        "data-analyst",
        include_str!("../../factory/templates/data-analyst.json"),
    ),
    (
        "compliance-checker",
        include_str!("../../factory/templates/compliance-checker.json"),
    ),
    (
        "support-triage",
        include_str!("../../factory/templates/support-triage.json"),
    ),
    (
        "project-coordinator",
        include_str!("../../factory/templates/project-coordinator.json"),
    ),
    (
        "financial-analyst",
        include_str!("../../factory/templates/financial-analyst.json"),
    ),
];

/// The project the researcher's cases ask about: what it is, how it is started and the
/// one external service it calls.
const RESEARCHER_FIXTURES: &[(&str, &str)] = &[
    (
        "README.md",
        include_str!("../../factory/templates/researcher/fixtures/README.md"),
    ),
    (
        "src/main.py",
        include_str!("../../factory/templates/researcher/fixtures/src/main.py"),
    ),
    (
        "config.ini",
        include_str!("../../factory/templates/researcher/fixtures/config.ini"),
    ),
];

/// The Node project the implementer's cases change, tested by `node --test`.
const IMPLEMENTER_FIXTURES: &[(&str, &str)] = &[
    (
        "package.json",
        include_str!("../../factory/templates/implementer/fixtures/package.json"),
    ),
    (
        "src/strings.js",
        include_str!("../../factory/templates/implementer/fixtures/src/strings.js"),
    ),
    (
        "src/numbers.js",
        include_str!("../../factory/templates/implementer/fixtures/src/numbers.js"),
    ),
    (
        "src/config.js",
        include_str!("../../factory/templates/implementer/fixtures/src/config.js"),
    ),
    (
        "test/strings.test.js",
        include_str!("../../factory/templates/implementer/fixtures/test/strings.test.js"),
    ),
    (
        "test/numbers.test.js",
        include_str!("../../factory/templates/implementer/fixtures/test/numbers.test.js"),
    ),
    (
        "test/config.test.js",
        include_str!("../../factory/templates/implementer/fixtures/test/config.test.js"),
    ),
];

/// A check a template's cases run that a draft can't bring: a check's command comes
/// only from a folder the owner shares, never from a blueprint.
struct NeededCheck {
    name: &'static str,
    /// The command the template's fixtures need it to run.
    command: &'static str,
    /// What the fixtures are.
    fixtures: &'static str,
}

/// The check template `name`'s cases run that its evaluation needs a deploy folder for.
fn needed_check(name: &str) -> Option<NeededCheck> {
    match name {
        "implementer" => Some(NeededCheck {
            name: "test",
            command: "node --test",
            fixtures: "a Node project",
        }),
        _ => None,
    }
}

/// A template, as listed.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct TemplateInfo {
    pub name: String,
    pub title: String,
    pub purpose: String,
    /// The most access its tasks need.
    pub access: Access,
    /// How many eval cases it starts with.
    pub eval_cases: usize,
    /// What its evaluation needs that a draft from it can't bring, if anything.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub needs: Option<String>,
}

/// The names of the built-in templates.
pub fn names() -> Vec<&'static str> {
    BUILT_IN.iter().map(|(name, _)| *name).collect()
}

/// Template `name`'s blueprint.
pub fn get(name: &str) -> Result<Value> {
    let (_, text) = BUILT_IN.iter().find(|(n, _)| *n == name).ok_or_else(|| {
        anyhow!(
            "there is no template '{name}' (proofkin factory templates lists them: {})",
            names().join(", ")
        )
    })?;
    serde_json::from_str(text).with_context(|| format!("template {name} isn't JSON"))
}

/// Every built-in template, by name.
pub fn list() -> Vec<TemplateInfo> {
    BUILT_IN
        .iter()
        .filter_map(|(name, _)| {
            let blueprint = from_value(&get(name).ok()?).ok()?;
            Some(TemplateInfo {
                name: (*name).to_owned(),
                title: blueprint.title,
                purpose: blueprint.purpose,
                access: blueprint.capabilities.access,
                eval_cases: blueprint.evals.cases.len(),
                needs: needs(name),
            })
        })
        .collect()
}

/// What template `name`'s evaluation needs that a draft from it can't bring, if anything.
pub fn needs(name: &str) -> Option<String> {
    let needed = needed_check(name)?;
    Some(format!(
        "needs a deploy folder whose {} check runs {}",
        needed.name, needed.command
    ))
}

/// For an agent drafted from template `name` whose evaluation can't start because none of
/// its deploy folders defines check `check`: what the check is to run, if the template says.
pub fn check_hint(name: &str, check: &str) -> Option<String> {
    let needed = needed_check(name).filter(|c| c.name == check)?;
    Some(format!(
        "its fixtures are {}: --check {check}=\"{}\"",
        needed.fixtures, needed.command
    ))
}

/// The files template `name`'s cases work on, as their paths in an agent's fixtures folder
/// and their text: none for a template whose cases carry their data in their input.
pub fn fixtures(name: &str) -> &'static [(&'static str, &'static str)] {
    match name {
        "researcher" => RESEARCHER_FIXTURES,
        "implementer" => IMPLEMENTER_FIXTURES,
        _ => &[],
    }
}

/// Writes template `name`'s fixtures into `dir`, an agent's fixtures folder. A file there
/// already is kept, not overwritten. Returns the paths of the files it wrote.
pub fn write_fixtures(name: &str, dir: &Path) -> Result<Vec<&'static str>> {
    let mut written = Vec::new();
    for (path, text) in fixtures(name) {
        if taken(dir, path) {
            continue;
        }
        let to = dir.join(path);
        let parent = to.parent().unwrap_or(dir);
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
        std::fs::write(&to, text).with_context(|| format!("writing {}", to.display()))?;
        written.push(*path);
    }
    Ok(written)
}

/// Whether nothing is to be written at `path` (relative, with `/`) in `dir`: something is
/// there already, or the way there goes through a file or a symbolic link, which isn't
/// followed.
fn taken(dir: &Path, path: &str) -> bool {
    let mut at = dir.to_path_buf();
    for part in path.split('/') {
        at.push(part);
        match std::fs::symlink_metadata(&at) {
            // A folder, not a link to one: on down.
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => return true,
            Err(_) => return false,
        }
    }
    true
}

/// The first draft of agent `name` from template `template`, for the profile `catalog`
/// describes: the template's blueprint, named `name`, on the providers the profile is
/// signed in to that the template can use (not Codex for agents that use MCP servers).
pub fn instantiate(template: &str, name: &str, catalog: &Catalog) -> Result<Value> {
    let mut value = get(template)?;
    let blueprint = from_value(&value).map_err(|p| anyhow!("template {template}: {p}"))?;
    let providers: Vec<Provider> = blueprint
        .model
        .providers
        .iter()
        .copied()
        .filter(|p| catalog.providers.contains(p))
        .filter(|p| !(blueprint.model.needs_mcp && *p == Provider::Codex))
        .collect();
    if providers.is_empty() {
        let can: Vec<String> = blueprint
            .model
            .providers
            .iter()
            .map(|p| p.to_string())
            .collect();
        bail!(
            "profile '{}' isn't signed in to a provider template {template} can run on ({}): proofkin auth login {} --profile {}",
            catalog.profile,
            can.join(", "),
            can.first().map(String::as_str).unwrap_or("copilot"),
            catalog.profile
        );
    }
    value["name"] = Value::from(name);
    value["model"]["providers"] = serde_json::to_value(&providers)?;
    if let Some(candidates) = value["model"]
        .get_mut("candidates")
        .and_then(Value::as_array_mut)
    {
        candidates.retain(|c| {
            serde_json::from_value::<Provider>(c["provider"].clone())
                .is_ok_and(|p| providers.contains(&p))
        });
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::factory::blueprint::check_value;
    use crate::factory::evals::check_fixtures;

    /// A profile with the built-in skills, signed in to every provider, with no
    /// folders, topics or peers of its own.
    fn fresh_profile() -> Catalog {
        let dir = tempfile::tempdir().unwrap();
        Catalog {
            profile: "alice".into(),
            skills: crate::skills::Skills::new(dir.path().join("skills"))
                .list()
                .into_iter()
                .map(|s| (s.name, s.description))
                .collect(),
            providers: [Provider::Copilot, Provider::Claude, Provider::Codex].into(),
            ..Catalog::default()
        }
    }

    #[test]
    fn every_template_passes_validation_unchanged_with_a_case_for_every_requirement() {
        assert_eq!(names().len(), 9);
        for name in names() {
            let value = get(name).unwrap();
            assert_eq!(value["name"], name, "a template is named as it is listed");
            let problems = check_value(&value, &fresh_profile());
            assert!(problems.is_empty(), "{name}: {problems:#?}");
            let blueprint = from_value(&value).unwrap();
            assert!(
                blueprint.evals.cases.len() >= 3,
                "{name} has at least 3 cases"
            );
            assert!(
                blueprint.evals.cases.iter().any(|c| c.smoke),
                "{name} has a smoke case"
            );
            assert!(!blueprint.capabilities.delegate, "{name} doesn't delegate");
            assert!(
                blueprint.capabilities.mcp.is_empty(),
                "{name} needs no MCP server"
            );
            // Its cases have the files they name once its fixtures are written, and it
            // brings none they don't name.
            let dir = tempfile::tempdir().unwrap();
            assert_eq!(
                write_fixtures(name, dir.path()).unwrap().len(),
                fixtures(name).len()
            );
            let missing = check_fixtures(&blueprint.evals, dir.path());
            assert!(missing.is_empty(), "{name}: {missing:#?}");
            let cases = &blueprint.evals.cases;
            for (path, _) in fixtures(name) {
                let named = cases.iter().any(|c| c.fixtures.iter().any(|f| f == path));
                assert!(named, "{name} brings {path}, which no case names");
            }
        }
    }

    #[test]
    fn writing_a_templates_fixtures_keeps_the_files_already_there() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("README.md"), "our own notes\n").unwrap();
        let written = write_fixtures("researcher", dir.path()).unwrap();
        assert_eq!(written, ["src/main.py", "config.ini"]);
        let kept = std::fs::read_to_string(dir.path().join("README.md")).unwrap();
        assert_eq!(kept, "our own notes\n");
        assert!(dir.path().join("src").join("main.py").is_file());
        assert!(write_fixtures("researcher", dir.path()).unwrap().is_empty());
    }

    #[test]
    fn the_implementers_fixtures_pass_node_test_as_shipped() {
        let dir = tempfile::tempdir().unwrap();
        write_fixtures("implementer", dir.path()).unwrap();
        let run = std::process::Command::new("node")
            .arg("--test")
            .current_dir(dir.path())
            .output();
        let output = match run {
            Ok(output) => output,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                eprintln!("note: node isn't on PATH: the implementer's fixtures weren't run");
                return;
            }
            Err(e) => panic!("running node --test: {e}"),
        };
        let out = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "node --test failed:\n{out}{}",
            String::from_utf8_lossy(&output.stderr)
        );
        for test in ["capitalize puts", "round keeps", "readConfig refuses"] {
            assert!(out.contains(test), "{test:?} didn't run:\n{out}");
        }
    }

    #[test]
    fn templates_are_listed_with_their_purpose_access_and_cases() {
        let listed = list();
        assert_eq!(listed.len(), 9);
        let reviewer = listed.iter().find(|t| t.name == "code-reviewer").unwrap();
        assert_eq!(reviewer.access, Access::ReadOnly);
        assert!(reviewer.eval_cases >= 3);
        assert!(!reviewer.purpose.is_empty());
        let implementer = listed.iter().find(|t| t.name == "implementer").unwrap();
        assert_eq!(implementer.access, Access::Full, "it builds and tests");
        assert_eq!(
            implementer.needs.as_deref(),
            Some("needs a deploy folder whose test check runs node --test")
        );
        assert_eq!(reviewer.needs, None, "its cases run no check");
        assert!(
            get("astrologer")
                .unwrap_err()
                .to_string()
                .contains("no template")
        );
    }

    #[test]
    fn a_draft_from_a_template_runs_on_the_providers_the_profile_signed_in_to() {
        let mut catalog = fresh_profile();
        catalog.providers = [Provider::Claude].into();
        let value = instantiate("researcher", "fin-research", &catalog).unwrap();
        assert_eq!(value["name"], "fin-research");
        assert_eq!(value["model"]["providers"], serde_json::json!(["claude"]));
        assert!(check_value(&value, &catalog).is_empty());

        catalog.providers.clear();
        let err = instantiate("researcher", "fin-research", &catalog).unwrap_err();
        assert!(err.to_string().contains("proofkin auth login"), "{err}");
    }
}
