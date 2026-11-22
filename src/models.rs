//! Which models an agent's providers run (see `proofkin models`): per provider, the
//! profile's default model, used when a task names none, and the models tasks may
//! ask for. Peers learn them from the agent's hello, so they can offer them.

use std::collections::BTreeMap;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::llm::Provider;

/// A provider's models, as the profile's owner set them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelChoice {
    /// The model tasks that name none run on (none: the provider's own default, or
    /// the first allowed model when only some are allowed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    /// The models tasks may ask for; empty: any.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed: Vec<String>,
}

/// Every provider's models, as kept in `profile.json` and sent to peers.
pub type Models = BTreeMap<Provider, ModelChoice>;

impl ModelChoice {
    pub fn is_empty(&self) -> bool {
        self.default.is_none() && self.allowed.is_empty()
    }

    /// Whether a task may ask for `model` (names compare without case).
    pub fn allows(&self, model: &str) -> bool {
        let model = model.trim();
        self.allowed.is_empty() || self.allowed.iter().any(|m| m.eq_ignore_ascii_case(model))
    }

    /// The model a task that asked for `asked` runs on: the one it asked for, if it is
    /// allowed, else the default; `None` leaves it to the provider.
    pub fn pick(&self, asked: Option<&str>) -> Result<Option<String>> {
        match asked.map(str::trim).filter(|m| !m.is_empty()) {
            Some(model) if self.allows(model) => Ok(Some(model.to_owned())),
            Some(model) => bail!(
                "{model} isn't a model allowed here: {}",
                self.allowed.join(", ")
            ),
            // With only some models allowed, the provider's own default may not be one.
            None => Ok(self
                .default
                .clone()
                .or_else(|| self.allowed.first().cloned())),
        }
    }
}

/// `provider`'s models in `models` (nothing set: any model, the provider's default).
pub fn choice(models: &Models, provider: Provider) -> ModelChoice {
    models.get(&provider).cloned().unwrap_or_default()
}

/// Checks a model's name: letters, digits and `-._:/@+`, at most 100 characters.
pub fn check_name(model: &str) -> Result<()> {
    let ok = !model.is_empty()
        && model.len() <= 100
        && model
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-._:/@+".contains(c));
    if !ok {
        bail!("{model:?} isn't a model name (letters, digits and -._:/@+, e.g. claude-sonnet-5.5)");
    }
    Ok(())
}

/// Sets `provider`'s default model (`None`: the provider's own). It must be allowed.
pub fn set_default(models: &mut Models, provider: Provider, model: Option<&str>) -> Result<()> {
    let mut choice = choice(models, provider);
    match model.map(str::trim) {
        Some(model) => {
            check_name(model)?;
            if !choice.allows(model) {
                bail!(
                    "{model} isn't one of {provider}'s allowed models ({}): allow it first",
                    choice.allowed.join(", ")
                );
            }
            choice.default = Some(model.to_owned());
        }
        None => choice.default = None,
    }
    store(models, provider, choice);
    Ok(())
}

/// Sets the models tasks may ask `provider` for (empty: any). The default, if set,
/// must be one of them.
pub fn set_allowed(models: &mut Models, provider: Provider, allowed: &[String]) -> Result<()> {
    let mut choice = choice(models, provider);
    let mut list: Vec<String> = Vec::new();
    for model in allowed.iter().map(|m| m.trim()).filter(|m| !m.is_empty()) {
        check_name(model)?;
        if !list.iter().any(|m| m.eq_ignore_ascii_case(model)) {
            list.push(model.to_owned());
        }
    }
    choice.allowed = list;
    if let Some(default) = &choice.default
        && !choice.allows(default)
    {
        bail!(
            "{provider}'s default model {default} isn't in that list: add it, or change the default first"
        );
    }
    store(models, provider, choice);
    Ok(())
}

fn store(models: &mut Models, provider: Provider, choice: ModelChoice) {
    if choice.is_empty() {
        models.remove(&provider);
    } else {
        models.insert(provider, choice);
    }
}

/// A model a provider offers this agent's account (Copilot lists them; see
/// [`crate::llm::CopilotBackend::list_models`]).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Offered {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vendor: Option<String>,
    /// Its cost relative to the base rate, if the provider says.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub multiplier: Option<f64>,
    /// Its policy state when it isn't simply enabled (e.g. `disabled`, `unconfigured`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy: Option<String>,
}

impl Offered {
    /// Reads what the Copilot SDK says about a model, as JSON (its types are
    /// experimental, so only the fields used here are read).
    pub fn from_json(model: &serde_json::Value) -> Option<Self> {
        let text = |v: &serde_json::Value| v.as_str().map(str::to_owned);
        let id = model.get("id").and_then(text)?;
        Some(Self {
            name: model
                .get("name")
                .and_then(text)
                .unwrap_or_else(|| id.clone()),
            vendor: model.get("vendor").and_then(text),
            multiplier: model
                .get("billing")
                .and_then(|b| b.get("multiplier"))
                .and_then(serde_json::Value::as_f64),
            policy: model
                .get("policy")
                .and_then(|p| p.get("state"))
                .and_then(text)
                .filter(|s| s != "enabled"),
            id,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_default_must_be_allowed_and_allowing_keeps_the_default() {
        let mut models = Models::new();
        set_allowed(
            &mut models,
            Provider::Copilot,
            &[
                "claude-sonnet-5.5".into(),
                "gpt-5.5".into(),
                "GPT-5.5".into(),
            ],
        )
        .unwrap();
        assert_eq!(
            models[&Provider::Copilot].allowed,
            ["claude-sonnet-5.5", "gpt-5.5"]
        );
        let err = set_default(&mut models, Provider::Copilot, Some("o9")).unwrap_err();
        assert!(err.to_string().contains("allow it first"), "{err}");
        set_default(&mut models, Provider::Copilot, Some("gpt-5.5")).unwrap();
        let err = set_allowed(
            &mut models,
            Provider::Copilot,
            &["claude-sonnet-5.5".into()],
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("change the default first"),
            "{err}"
        );
        // Any model again: the default stays.
        set_allowed(&mut models, Provider::Copilot, &[]).unwrap();
        assert_eq!(
            models[&Provider::Copilot],
            ModelChoice {
                default: Some("gpt-5.5".into()),
                allowed: Vec::new()
            }
        );
        // Unset, the provider leaves profile.json.
        set_default(&mut models, Provider::Copilot, None).unwrap();
        assert!(models.is_empty());
        assert!(set_default(&mut models, Provider::Claude, Some("bad name")).is_err());
        assert!(set_allowed(&mut models, Provider::Claude, &["`x`".into()]).is_err());
    }

    #[test]
    fn a_task_runs_on_the_model_it_asks_for_if_allowed_else_the_default() {
        let any = ModelChoice::default();
        assert_eq!(any.pick(None).unwrap(), None, "the provider's own default");
        assert_eq!(any.pick(Some(" o9 ")).unwrap().as_deref(), Some("o9"));
        let some = ModelChoice {
            default: None,
            allowed: vec!["claude-sonnet-5.5".into(), "gpt-5.5".into()],
        };
        // The provider's own default may not be allowed: the first allowed one is used.
        assert_eq!(
            some.pick(None).unwrap().as_deref(),
            Some("claude-sonnet-5.5")
        );
        assert_eq!(
            some.pick(Some("GPT-5.5")).unwrap().as_deref(),
            Some("GPT-5.5")
        );
        let err = some.pick(Some("o9")).unwrap_err().to_string();
        assert_eq!(
            err,
            "o9 isn't a model allowed here: claude-sonnet-5.5, gpt-5.5"
        );
        let with_default = ModelChoice {
            default: Some("gpt-5.5".into()),
            ..some
        };
        assert_eq!(
            with_default.pick(Some("")).unwrap().as_deref(),
            Some("gpt-5.5")
        );
    }

    #[test]
    fn settings_round_trip_in_profile_json_and_are_left_out_when_unset() {
        let mut config = crate::profile::ProfileConfig::default();
        assert!(!serde_json::to_string(&config).unwrap().contains("models"));
        set_default(
            &mut config.models,
            Provider::Claude,
            Some("claude-opus-5.5"),
        )
        .unwrap();
        set_allowed(&mut config.models, Provider::Codex, &["gpt-5.5".into()]).unwrap();
        let json = serde_json::to_string(&config).unwrap();
        assert!(
            json.contains(
                r#""models":{"claude":{"default":"claude-opus-5.5"},"codex":{"allowed":["gpt-5.5"]}}"#
            ),
            "{json}"
        );
        let back: crate::profile::ProfileConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.models, config.models);
    }

    #[test]
    fn what_copilot_says_about_a_model_is_read_leniently() {
        let model = serde_json::json!({
            "id": "claude-sonnet-5.5", "name": "Claude Sonnet 5.5", "vendor": "Anthropic",
            "billing": { "multiplier": 1.0 }, "policy": { "state": "enabled" },
            "capabilities": {}
        });
        let offered = Offered::from_json(&model).unwrap();
        assert_eq!(offered.id, "claude-sonnet-5.5");
        assert_eq!(offered.multiplier, Some(1.0));
        assert_eq!(offered.policy, None);
        let bare = Offered::from_json(
            &serde_json::json!({ "id": "o9", "policy": { "state": "disabled" } }),
        )
        .unwrap();
        assert_eq!(
            (bare.name.as_str(), bare.policy.as_deref()),
            ("o9", Some("disabled"))
        );
        assert!(Offered::from_json(&serde_json::json!({ "name": "no id" })).is_none());
    }
}
