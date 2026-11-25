//! Built-in agent templates (docs/agent-factory.md, section 2): blueprints with starter
//! eval suites, in `factory/templates/*.json`, embedded in the binary like the built-in
//! skills. Each asks for the least access it needs and has at least three cases,
//! covering every requirement. [`instantiate`] makes an agent's first draft from one.

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
            })
        })
        .collect()
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
