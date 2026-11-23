//! Which models an agent's providers run (see `proofkin models`): per provider, the
//! profile's default model, used when a task names none, the models tasks may ask for,
//! the reasoning effort tasks work at unless they ask for another, and the efforts they
//! may ask for. Peers learn them from the agent's hello (and whenever they change), with
//! the models its account offers that they may ask for ([`for_peers`]), so they can
//! offer exactly those.

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
    /// the model's own, or the least allowed effort when only some are allowed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// The reasoning efforts tasks may ask for, least to most; empty: any.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub efforts: Vec<String>,
}

/// Every provider's models, as kept in `profile.json` and sent to peers.
pub type Models = BTreeMap<Provider, ModelChoice>;

impl ModelChoice {
    pub fn is_empty(&self) -> bool {
        self.default.is_none()
            && self.allowed.is_empty()
            && self.effort.is_none()
            && self.efforts.is_empty()
    }

    /// Whether a task may ask for `model` (names compare without case).
    pub fn allows(&self, model: &str) -> bool {
        let model = model.trim();
        self.allowed.is_empty() || self.allowed.iter().any(|m| m.eq_ignore_ascii_case(model))
    }

    /// Whether a task may ask for reasoning effort `effort` (any case).
    pub fn allows_effort(&self, effort: &str) -> bool {
        let effort = effort.trim();
        self.efforts.is_empty() || self.efforts.iter().any(|e| e.eq_ignore_ascii_case(effort))
    }

    /// Whether a task asking for `model` and `effort` (none: the defaults) would be
    /// taken, not refused.
    pub fn admits(&self, model: Option<&str>, effort: Option<&str>) -> bool {
        model.is_none_or(|m| self.allows(m)) && effort.is_none_or(|e| self.allows_effort(e))
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
    /// for, if it is allowed, else the default; `None` leaves it to the model.
    pub fn pick_effort(&self, asked: Option<&str>) -> Result<Option<String>> {
        match asked.map(str::trim).filter(|e| !e.is_empty()) {
            Some(effort) => {
                check_effort(effort)?;
                if !self.allows_effort(effort) {
                    bail!(
                        "{effort} isn't a reasoning effort allowed here: {}",
                        self.efforts.join(", ")
                    );
                }
                Ok(Some(effort.to_ascii_lowercase()))
            }
            // With only some efforts allowed, the model's own may not be one.
            None => Ok(self
                .effort
                .clone()
                .or_else(|| self.efforts.first().cloned())),
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

/// What a provider's models are set to, in words (`proofkin models`, the web UI's feed).
pub fn describe(choice: &ModelChoice) -> String {
    let default = match (&choice.default, choice.allowed.first()) {
        (Some(model), _) => format!("tasks that name no model run on {model}"),
        (None, Some(first)) => {
            format!("tasks that name no model run on {first} (the first allowed)")
        }
        (None, None) => "tasks that name no model run on the provider's own default".to_owned(),
    };
    let allowed = if choice.allowed.is_empty() {
        "any model may be asked for".to_owned()
    } else {
        format!("only {} may be asked for", choice.allowed.join(", "))
    };
    let effort = match (&choice.effort, choice.efforts.first()) {
        (Some(effort), _) => format!("tasks that ask for no effort work at {effort} reasoning"),
        (None, Some(least)) => {
            format!("tasks that ask for no effort work at {least} reasoning (the least allowed)")
        }
        (None, None) => "tasks that ask for no effort work at the model's own".to_owned(),
    };
    let efforts = if choice.efforts.is_empty() {
        "any effort may be asked for".to_owned()
    } else {
        format!(
            "only {} reasoning may be asked for",
            choice.efforts.join(", ")
        )
    };
    format!("{default}; {allowed}; {effort}; {efforts}")
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
/// (`None`: the model's own). It must be allowed.
pub fn set_effort(models: &mut Models, provider: Provider, effort: Option<&str>) -> Result<()> {
    let mut choice = choice(models, provider);
    choice.effort = match effort.map(str::trim) {
        Some(effort) => {
            check_effort(effort)?;
            if !choice.allows_effort(effort) {
                bail!(
                    "{effort} isn't one of {provider}'s allowed reasoning efforts ({}): allow it first",
                    choice.efforts.join(", ")
                );
            }
            Some(effort.to_ascii_lowercase())
        }
        None => None,
    };
    store(models, provider, choice);
    Ok(())
}

/// Sets the reasoning efforts tasks may ask `provider` for (empty, or all of them:
/// any), kept least to most. The default effort, if set, must be one of them.
pub fn set_efforts(models: &mut Models, provider: Provider, efforts: &[String]) -> Result<()> {
    let mut choice = choice(models, provider);
    let mut list: Vec<String> = Vec::new();
    for effort in efforts.iter().map(|e| e.trim()).filter(|e| !e.is_empty()) {
        check_effort(effort)?;
        let effort = effort.to_ascii_lowercase();
        if !list.contains(&effort) {
            list.push(effort);
        }
    }
    list.sort_by_key(|e| effort_rank(e));
    if list.len() == EFFORTS.len() {
        list.clear();
    }
    choice.efforts = list;
    if let Some(effort) = &choice.effort
        && !choice.allows_effort(effort)
    {
        bail!(
            "{provider}'s default reasoning effort {effort} isn't in that list: add it, or change the default first"
        );
    }
    store(models, provider, choice);
    Ok(())
}

/// Sets all of `provider`'s models at once (as the web UI's Models dialog does): the
/// names and efforts must be well-formed, and the defaults allowed.
pub fn set_choice(models: &mut Models, provider: Provider, choice: &ModelChoice) -> Result<()> {
    fn given(value: &Option<String>) -> Option<&str> {
        value.as_deref().map(str::trim).filter(|v| !v.is_empty())
    }
    let mut set = Models::new();
    set_allowed(&mut set, provider, &choice.allowed)?;
    set_efforts(&mut set, provider, &choice.efforts)?;
    set_default(&mut set, provider, given(&choice.default))?;
    set_effort(&mut set, provider, given(&choice.effort))?;
    match set.remove(&provider) {
        Some(choice) => models.insert(provider, choice),
        None => models.remove(&provider),
    };
    Ok(())
}

fn store(models: &mut Models, provider: Provider, choice: ModelChoice) {
    if choice.is_empty() {
        models.remove(&provider);
    } else {
        models.insert(provider, choice);
    }
}

/// A model a provider offers an agent's account (Copilot lists them; see
/// [`crate::llm::CopilotBackend::list_models`]). Peers learn the ones they may ask for
/// ([`for_peers`]), as they say in their hello and `models` messages.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Offered {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vendor: Option<String>,
    /// Its cost relative to the base rate, if the provider says (e.g. `1`, `0.33`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub multiplier: Option<String>,
    /// Its policy state when it isn't simply enabled (e.g. `disabled`, `unconfigured`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<String>,
    /// The reasoning efforts it supports (none: it doesn't let them be set), and its own.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub efforts: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_effort: Option<String>,
}

/// The models each provider offers an agent's account.
pub type Offers = BTreeMap<Provider, Vec<Offered>>;

/// The most models an agent tells peers about per provider.
pub const MAX_OFFERED: usize = 200;

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
                .and_then(serde_json::Value::as_f64)
                .map(|x| x.to_string()),
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

    /// As peers see it: its name and vendor short, single-line text, its multiplier a
    /// number, and only the reasoning efforts of [`EFFORTS`], least to most. `None` for
    /// one that isn't usable (not enabled) or not well-named.
    fn cleaned(mut self) -> Option<Self> {
        if self.policy.is_some() || check_name(&self.id).is_err() {
            return None;
        }
        self.name = crate::llm::clean_model(&self.name);
        if self.name.is_empty() {
            self.name = self.id.clone();
        }
        self.vendor = self
            .vendor
            .map(|v| {
                crate::llm::clean_model(&v)
                    .chars()
                    .take(50)
                    .collect::<String>()
            })
            .filter(|v| !v.is_empty());
        self.multiplier = self
            .multiplier
            .filter(|x| x.len() <= 12 && x.parse::<f64>().is_ok_and(|x| x.is_finite() && x >= 0.0));
        let mut efforts: Vec<String> = Vec::new();
        for effort in self.efforts.iter().map(|e| e.to_ascii_lowercase()) {
            if effort_rank(&effort).is_some() && !efforts.contains(&effort) {
                efforts.push(effort);
            }
        }
        efforts.sort_by_key(|e| effort_rank(e));
        self.default_effort = self
            .default_effort
            .map(|e| e.to_ascii_lowercase())
            .filter(|e| efforts.contains(e));
        self.efforts = efforts;
        Some(self)
    }
}

/// What peers learn of the models `offered` to an agent's account for a provider whose
/// settings are `choice`: the usable ones its tasks may ask for (all of them when it
/// allows any), with the reasoning efforts each supports.
pub fn for_peers(offered: &[Offered], choice: &ModelChoice) -> Vec<Offered> {
    let mut seen = std::collections::HashSet::new();
    offered
        .iter()
        .filter(|m| choice.allows(&m.id))
        .filter_map(|m| m.clone().cleaned())
        .filter(|m| seen.insert(m.id.to_ascii_lowercase()))
        .take(MAX_OFFERED)
        .collect()
}

/// A peer's offers as it says, kept only as far as they make sense (see
/// [`for_peers`]), for the providers it serves.
pub fn clean_offers(offers: Offers, serves: &[Provider]) -> Offers {
    offers
        .into_iter()
        .filter(|(provider, _)| serves.contains(provider))
        .map(|(provider, list)| (provider, for_peers(&list, &ModelChoice::default())))
        .filter(|(_, list)| !list.is_empty())
        .collect()
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
            allowed: vec!["claude-sonnet-5.5".into(), "gpt-5.5".into()],
            ..Default::default()
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
        assert_eq!(offered.multiplier.as_deref(), Some("1"));
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

    #[test]
    fn a_task_may_ask_only_for_the_reasoning_efforts_its_agent_allows() {
        let mut models = Models::new();
        set_efforts(
            &mut models,
            Provider::Copilot,
            &["MEDIUM".into(), "low".into(), "low".into()],
        )
        .unwrap();
        let only = choice(&models, Provider::Copilot);
        assert_eq!(only.efforts, ["low", "medium"], "least to most, once each");
        assert_eq!(
            only.pick_effort(Some("Medium")).unwrap().as_deref(),
            Some("medium")
        );
        assert_eq!(
            only.pick_effort(Some("max")).unwrap_err().to_string(),
            "max isn't a reasoning effort allowed here: low, medium"
        );
        // Asking for none: the default, else the least allowed (the model's own may not be).
        assert_eq!(only.pick_effort(None).unwrap().as_deref(), Some("low"));
        assert!(only.admits(Some("any-model"), Some("low")) && only.admits(None, None));
        assert!(!only.admits(None, Some("high")));
        // The default must be allowed, and the list keep it.
        let err = set_effort(&mut models, Provider::Copilot, Some("high")).unwrap_err();
        assert!(err.to_string().contains("allow it first"), "{err}");
        set_effort(&mut models, Provider::Copilot, Some("medium")).unwrap();
        let err = set_efforts(&mut models, Provider::Copilot, &["low".into()]).unwrap_err();
        assert!(
            err.to_string().contains("change the default first"),
            "{err}"
        );
        assert!(set_efforts(&mut models, Provider::Copilot, &["turbo".into()]).is_err());
        // All of them is any.
        let all: Vec<String> = EFFORTS.iter().map(|e| e.to_string()).collect();
        set_efforts(&mut models, Provider::Copilot, &all).unwrap();
        assert!(choice(&models, Provider::Copilot).efforts.is_empty());
        // The Models dialog sets everything at once, checked as a whole.
        let wanted = ModelChoice {
            default: Some(" gpt-5.5 ".into()),
            allowed: vec!["gpt-5.5".into(), "claude-opus-5.5".into()],
            effort: Some("high".into()),
            efforts: vec!["max".into(), "high".into()],
        };
        set_choice(&mut models, Provider::Copilot, &wanted).unwrap();
        assert_eq!(
            models[&Provider::Copilot],
            ModelChoice {
                default: Some("gpt-5.5".into()),
                allowed: vec!["gpt-5.5".into(), "claude-opus-5.5".into()],
                effort: Some("high".into()),
                efforts: vec!["high".into(), "max".into()],
            }
        );
        let refused = ModelChoice {
            effort: Some("low".into()),
            ..wanted.clone()
        };
        assert!(set_choice(&mut models, Provider::Copilot, &refused).is_err());
        let not_allowed = ModelChoice {
            default: Some("o9".into()),
            ..wanted
        };
        assert!(set_choice(&mut models, Provider::Copilot, &not_allowed).is_err());
        assert_eq!(
            models[&Provider::Copilot].effort.as_deref(),
            Some("high"),
            "unchanged"
        );
        set_choice(&mut models, Provider::Copilot, &ModelChoice::default()).unwrap();
        assert!(models.is_empty());
    }

    #[test]
    fn peers_learn_the_usable_models_they_may_ask_for() {
        let model = |id: &str, efforts: &[&str]| Offered {
            id: id.into(),
            name: format!("{id}\u{1b}[31m name"),
            vendor: Some("Anthropic".into()),
            multiplier: Some("3".into()),
            efforts: efforts.iter().map(|e| e.to_string()).collect(),
            default_effort: efforts.first().map(|e| e.to_string()),
            ..Default::default()
        };
        let offered = vec![
            model("claude-opus-5.5", &["max", "none", "HIGH", "low"]),
            model("gpt-5.5", &["none"]),
            Offered {
                policy: Some("disabled".into()),
                ..model("o9", &[])
            },
            model("bad name", &[]),
            model("CLAUDE-OPUS-5.5", &[]),
        ];
        let any = for_peers(&offered, &ModelChoice::default());
        let ids: Vec<&str> = any.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            ["claude-opus-5.5", "gpt-5.5"],
            "usable and well-named, once"
        );
        assert_eq!(
            any[0].efforts,
            ["low", "high", "max"],
            "known efforts, least to most"
        );
        assert_eq!(any[0].default_effort.as_deref(), Some("max"));
        assert_eq!(
            (any[1].efforts.len(), any[1].default_effort.as_deref()),
            (0, None)
        );
        assert_eq!(any[0].name, "claude-opus-5.531m name", "shown as text");
        // A worker that limits its models tells peers only those.
        let only = ModelChoice {
            allowed: vec!["gpt-5.5".into()],
            ..Default::default()
        };
        assert_eq!(for_peers(&offered, &only).len(), 1);
        // A peer's offers are cleaned the same way, for the providers it serves only.
        let said = Offers::from([
            (Provider::Copilot, offered.clone()),
            (Provider::Claude, offered.clone()),
        ]);
        let kept = clean_offers(said, &[Provider::Copilot]);
        assert_eq!(kept.keys().collect::<Vec<_>>(), [&Provider::Copilot]);
        assert_eq!(kept[&Provider::Copilot], any);
        let many: Vec<Offered> = (0..MAX_OFFERED + 5)
            .map(|i| model(&format!("m{i}"), &[]))
            .collect();
        let many = clean_offers(
            Offers::from([(Provider::Copilot, many)]),
            &[Provider::Copilot],
        );
        assert_eq!(many[&Provider::Copilot].len(), MAX_OFFERED);
        let odd = Offered {
            multiplier: Some("NaN".into()),
            vendor: Some(" ".into()),
            ..model("m", &[])
        };
        let odd = for_peers(&[odd], &ModelChoice::default()).remove(0);
        assert_eq!((odd.multiplier, odd.vendor), (None, None));
    }
}
