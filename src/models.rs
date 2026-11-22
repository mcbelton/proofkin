//! Which models an agent's providers run (see `proofkin models`): per provider, the
//! profile's default model, used when a task names none, the models tasks may ask for,
//! and the reasoning effort tasks work at unless they ask for another. Peers learn them
//! from the agent's hello, so they can offer them.

use std::collections::BTreeMap;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::llm::Provider;

/// Reasoning efforts, least to most: what Copilot, the Claude Agent SDK and Codex call
/// them. A model takes the ones it supports (see [`fit_effort`]).
pub const EFFORTS: &[&str] = &["minimal", "low", "medium", "high", "xhigh", "max"];

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
    /// The reasoning effort tasks that ask for none work at (one of [`EFFORTS`]; none:
    /// the model's own).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

/// Every provider's models, as kept in `profile.json` and sent to peers.
pub type Models = BTreeMap<Provider, ModelChoice>;

impl ModelChoice {
    pub fn is_empty(&self) -> bool {
        self.default.is_none() && self.allowed.is_empty() && self.effort.is_none()
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

    /// The reasoning effort a task that asked for `asked` works at: the one it asked
    /// for, else the default; `None` leaves it to the model.
    pub fn pick_effort(&self, asked: Option<&str>) -> Result<Option<String>> {
        match asked.map(str::trim).filter(|e| !e.is_empty()) {
            Some(effort) => {
                check_effort(effort)?;
                Ok(Some(effort.to_ascii_lowercase()))
            }
            None => Ok(self.effort.clone()),
        }
    }
}

/// Checks a reasoning effort's name: one of [`EFFORTS`] (in any case).
pub fn check_effort(effort: &str) -> Result<()> {
    if !EFFORTS
        .iter()
        .any(|e| e.eq_ignore_ascii_case(effort.trim()))
    {
        bail!(
            "{effort:?} isn't a reasoning effort: use one of {}",
            EFFORTS.join(", ")
        );
    }
    Ok(())
}

/// Where `effort` stands among [`EFFORTS`] (none for a name that isn't one).
fn effort_rank(effort: &str) -> Option<usize> {
    EFFORTS.iter().position(|e| e.eq_ignore_ascii_case(effort))
}

/// `effort` as a model that supports only `supported` takes it: itself if supported,
/// else the most it supports below it, else the least it supports; `None` when it
/// supports none (it doesn't reason, or doesn't let it be set).
pub fn fit_effort(effort: &str, supported: &[String]) -> Option<String> {
    if supported.iter().any(|s| s.eq_ignore_ascii_case(effort)) {
        return Some(effort.to_ascii_lowercase());
    }
    let want = effort_rank(effort)?;
    let mut known: Vec<(usize, &String)> = supported
        .iter()
        .filter_map(|s| effort_rank(s).map(|rank| (rank, s)))
        .collect();
    known.sort();
    known
        .iter()
        .rev()
        .find(|(rank, _)| *rank <= want)
        .or(known.first())
        .map(|(_, s)| s.to_ascii_lowercase())
}

/// What a result names as the model that did the work: the model and the reasoning
/// effort it worked at, e.g. `claude-opus-5.5 (max reasoning)`. Only characters model
/// names use, so every version of Proofkin shows it as it is.
pub fn with_effort(model: Option<String>, effort: Option<&str>) -> Option<String> {
    match (model, effort.map(str::trim).filter(|e| !e.is_empty())) {
        (Some(model), Some(effort)) => Some(format!("{model} ({effort} reasoning)")),
        (None, Some(effort)) => Some(format!("{effort} reasoning")),
        (model, None) => model,
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

/// Sets the reasoning effort `provider`'s tasks work at unless they ask for another
/// (`None`: the model's own).
pub fn set_effort(models: &mut Models, provider: Provider, effort: Option<&str>) -> Result<()> {
    let mut choice = choice(models, provider);
    choice.effort = match effort.map(str::trim) {
        Some(effort) => {
            check_effort(effort)?;
            Some(effort.to_ascii_lowercase())
        }
        None => None,
    };
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
    /// The reasoning efforts it supports (none: it doesn't let them be set), and its own.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub efforts: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_effort: Option<String>,
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
            efforts: model
                .get("supportedReasoningEfforts")
                .and_then(serde_json::Value::as_array)
                .map(|list| {
                    list.iter()
                        .filter_map(text)
                        .filter(|e| e.len() <= 20 && e.chars().all(|c| c.is_ascii_alphanumeric()))
                        .take(20)
                        .collect()
                })
                .unwrap_or_default(),
            default_effort: model
                .get("defaultReasoningEffort")
                .and_then(text)
                .filter(|e| e.len() <= 20 && e.chars().all(|c| c.is_ascii_alphanumeric())),
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
                ..Default::default()
            }
        );
        // Unset, the provider leaves profile.json.
        set_default(&mut models, Provider::Copilot, None).unwrap();
        assert!(models.is_empty());
        assert!(set_default(&mut models, Provider::Claude, Some("bad name")).is_err());
        assert!(set_allowed(&mut models, Provider::Claude, &["`x`".into()]).is_err());
    }

    #[test]
    fn a_task_works_at_the_effort_it_asks_for_else_the_default_fitted_to_its_model() {
        let mut models = Models::new();
        set_effort(&mut models, Provider::Copilot, Some("MAX")).unwrap();
        let choice = choice(&models, Provider::Copilot);
        assert_eq!(choice.effort.as_deref(), Some("max"));
        assert_eq!(choice.pick_effort(None).unwrap().as_deref(), Some("max"));
        assert_eq!(
            choice.pick_effort(Some(" Low ")).unwrap().as_deref(),
            Some("low")
        );
        let err = choice.pick_effort(Some("turbo")).unwrap_err().to_string();
        assert_eq!(
            err,
            "\"turbo\" isn't a reasoning effort: use one of minimal, low, medium, high, xhigh, max"
        );
        assert!(set_effort(&mut models, Provider::Copilot, Some("turbo")).is_err());
        assert_eq!(ModelChoice::default().pick_effort(None).unwrap(), None);
        // A model takes what it supports: the asked effort, else the most below it, else
        // its least; none if it supports none.
        let three = ["low".to_owned(), "medium".into(), "high".into()];
        assert_eq!(fit_effort("medium", &three).as_deref(), Some("medium"));
        assert_eq!(fit_effort("max", &three).as_deref(), Some("high"));
        assert_eq!(fit_effort("minimal", &three).as_deref(), Some("low"));
        assert_eq!(
            fit_effort("max", &["xhigh".to_owned(), "odd".into()]).as_deref(),
            Some("xhigh")
        );
        assert_eq!(fit_effort("high", &[]), None);
        // A result names the effort with the model, in characters every version shows.
        assert_eq!(
            with_effort(Some("claude-opus-5.5".into()), Some("max")).as_deref(),
            Some("claude-opus-5.5 (max reasoning)")
        );
        let shown = with_effort(Some("claude-opus-5.5".into()), Some("max")).unwrap();
        assert_eq!(crate::llm::clean_model(&shown), shown);
        assert_eq!(
            with_effort(None, Some("low")).as_deref(),
            Some("low reasoning")
        );
        assert_eq!(with_effort(Some("m".into()), None).as_deref(), Some("m"));
        // Unset, the provider leaves profile.json.
        set_effort(&mut models, Provider::Copilot, None).unwrap();
        assert!(models.is_empty());
    }

    #[test]
    fn a_task_runs_on_the_model_it_asks_for_if_allowed_else_the_default() {
        let any = ModelChoice::default();
        assert_eq!(any.pick(None).unwrap(), None, "the provider's own default");
        assert_eq!(any.pick(Some(" o9 ")).unwrap().as_deref(), Some("o9"));
        let some = ModelChoice {
            default: None,
            allowed: vec!["claude-sonnet-5.5".into(), "gpt-5.5".into()],
            effort: None,
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
            "capabilities": {},
            "supportedReasoningEfforts": ["low", "medium", "high", "not valid!"],
            "defaultReasoningEffort": "medium"
        });
        let offered = Offered::from_json(&model).unwrap();
        assert_eq!(offered.id, "claude-sonnet-5.5");
        assert_eq!(offered.multiplier, Some(1.0));
        assert_eq!(offered.policy, None);
        assert_eq!(offered.efforts, ["low", "medium", "high"]);
        assert_eq!(offered.default_effort.as_deref(), Some("medium"));
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
