//! Model policy (docs/agent-factory.md, section 5): the models an agent may be evaluated
//! and run on, which one a release takes, and how a deployment keeps its tasks to the
//! models it was evaluated on.
//!
//! [`candidates`] are the blueprint's own (else the profile's default for each of its
//! providers), each left out, with the reason, when the profile doesn't allow it, it
//! can't meet `needs_mcp` or `max_multiplier`, or its Microsoft 365 content can't go
//! where it would run. [`recommend`] picks among those an evaluation passed, as the
//! blueprint prefers; [`pin`] turns the pick into a deployment's model settings.

use std::cmp::Ordering;
use std::fmt;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::factory::blueprint::{Blueprint, Catalog, Prefer};
use crate::llm::Provider;
use crate::models::{ModelChoice, Models, Offered, Offers};

/// A model an agent may run on: a provider, its model (none: the provider's own default)
/// and its reasoning effort (none: the model's own).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Route {
    pub provider: Provider,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

impl fmt::Display for Route {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.model, &self.effort) {
            (Some(model), Some(effort)) => {
                write!(f, "{} {model} ({effort} reasoning)", self.provider)
            }
            (Some(model), None) => write!(f, "{} {model}", self.provider),
            (None, Some(effort)) => {
                write!(
                    f,
                    "{} (its default model, {effort} reasoning)",
                    self.provider
                )
            }
            (None, None) => write!(f, "{} (its default model)", self.provider),
        }
    }
}

/// A model left out of the candidates, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeftOut {
    pub route: Route,
    pub why: String,
}

/// The models an agent is evaluated on, and those left out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Routes {
    pub candidates: Vec<Route>,
    pub left_out: Vec<LeftOut>,
}

/// The models `bp` may be evaluated on for the profile `catalog` describes. `offers` are
/// the models its accounts offer, if known (Copilot lists them, with their cost
/// multipliers, once it runs; `max_multiplier` is checked against them); `own_worker`
/// says whether its tasks would run on one of the owner's own workers.
pub fn candidates(
    bp: &Blueprint,
    catalog: &Catalog,
    offers: Option<&Offers>,
    own_worker: bool,
) -> Routes {
    let policy = &bp.model;
    let listed: Vec<Route> = if policy.candidates.is_empty() {
        policy
            .providers
            .iter()
            .map(|provider| {
                let choice = crate::models::choice(&catalog.models, *provider);
                Route {
                    provider: *provider,
                    model: choice.pick(None).ok().flatten(),
                    effort: choice.pick_effort(None).ok().flatten(),
                }
            })
            .collect()
    } else {
        policy
            .candidates
            .iter()
            .map(|c| Route {
                provider: c.provider,
                model: Some(c.model.trim().to_owned()),
                effort: c.effort.as_deref().map(|e| e.trim().to_ascii_lowercase()),
            })
            .collect()
    };
    // Its tasks get Microsoft 365 content: work context, or a marked topic it reads.
    let m365 = !bp.capabilities.workiq.is_empty()
        || bp
            .memory
            .context
            .iter()
            .any(|t| catalog.marked.contains_key(t));
    let mut routes = Routes::default();
    for route in listed {
        let why = if routes.candidates.contains(&route) {
            Some("it is listed twice".to_owned())
        } else {
            left_out(&route, bp, catalog, offers, own_worker, m365)
        };
        match why {
            Some(why) => routes.left_out.push(LeftOut { route, why }),
            None => routes.candidates.push(route),
        }
    }
    routes
}

fn left_out(
    route: &Route,
    bp: &Blueprint,
    catalog: &Catalog,
    offers: Option<&Offers>,
    own_worker: bool,
    m365: bool,
) -> Option<String> {
    let provider = route.provider;
    if !bp.model.providers.contains(&provider) {
        return Some(format!("{provider} isn't one of its providers"));
    }
    if !catalog.providers.contains(&provider) {
        return Some(format!("the profile isn't signed in to {provider}"));
    }
    if bp.model.needs_mcp && provider == Provider::Codex {
        return Some("it needs MCP servers, which Codex can't use here".to_owned());
    }
    if m365 && !own_worker {
        return Some(
            "it works with Microsoft 365 content, which goes only to your own workers".to_owned(),
        );
    }
    let choice = crate::models::choice(&catalog.models, provider);
    if let Some(model) = &route.model
        && !choice.allows(model)
    {
        return Some(format!("the profile doesn't allow {model} on {provider}"));
    }
    if let Some(effort) = &route.effort {
        if crate::models::check_effort(effort).is_err() {
            return Some(format!("{effort:?} isn't a reasoning effort"));
        }
        if !choice.allows_effort(effort) {
            return Some(format!(
                "the profile doesn't allow {effort} reasoning on {provider}"
            ));
        }
    }
    // What the account offers, where it says (Copilot lists its models).
    let offered = offers
        .and_then(|o| o.get(&provider))
        .filter(|list| !list.is_empty())?;
    let Some(model) = &route.model else {
        return bp.model.max_multiplier.map(|_| {
            format!(
                "max_multiplier can't be checked for {provider}'s own default model: name the model"
            )
        });
    };
    let Some(found) = offered.iter().find(|m| m.id.eq_ignore_ascii_case(model)) else {
        return Some(format!("your {provider} account doesn't offer {model}"));
    };
    if let Some(policy) = &found.policy {
        return Some(format!("{model} is {policy} for your {provider} account"));
    }
    let most = bp.model.max_multiplier?;
    match multiplier(found) {
        Some(x) if x > most => Some(format!("it costs {x}x, over its max_multiplier of {most}x")),
        Some(_) => None,
        None => Some(format!(
            "{provider} gives no cost multiplier for {model}, so max_multiplier can't be checked"
        )),
    }
}

/// An offered model's cost multiplier, as a number.
pub fn multiplier(model: &Offered) -> Option<f64> {
    model
        .multiplier
        .as_deref()?
        .trim()
        .trim_end_matches('x')
        .parse::<f64>()
        .ok()
        .filter(|x| x.is_finite() && *x >= 0.0)
}

/// How a candidate did in an evaluation, as the recommendation weighs it.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub route: Route,
    /// It passed the release gate.
    pub passed: bool,
    /// The share of its runs that passed.
    pub pass_rate: f64,
    pub p95_secs: Option<f64>,
    pub multiplier: Option<f64>,
    /// The median judge score (1 to 5), if judges scored it.
    pub judge_score: Option<f64>,
}

/// The candidate a release takes: among those that passed the gate, the one `prefer`
/// favors (the cheapest, the fastest or the best), the others breaking ties, then the
/// order they were listed in. None when none passed.
pub fn recommend(outcomes: &[Outcome], prefer: Prefer) -> Option<&Outcome> {
    // An unknown cost or duration is never taken for a low one.
    let high = |x: Option<f64>| x.unwrap_or(f64::INFINITY);
    let cheaper = |a: &Outcome, b: &Outcome| high(a.multiplier).total_cmp(&high(b.multiplier));
    let faster = |a: &Outcome, b: &Outcome| high(a.p95_secs).total_cmp(&high(b.p95_secs));
    let better = |a: &Outcome, b: &Outcome| b.pass_rate.total_cmp(&a.pass_rate);
    let judged = |a: &Outcome, b: &Outcome| {
        let score = |x: Option<f64>| x.unwrap_or(f64::NEG_INFINITY);
        score(b.judge_score).total_cmp(&score(a.judge_score))
    };
    let order = |a: &&Outcome, b: &&Outcome| -> Ordering {
        match prefer {
            Prefer::Cost => cheaper(a, b).then(better(a, b)).then(faster(a, b)),
            Prefer::Latency => faster(a, b).then(better(a, b)).then(cheaper(a, b)),
            Prefer::Quality => better(a, b)
                .then(judged(a, b))
                .then(faster(a, b))
                .then(cheaper(a, b)),
        }
    };
    outcomes.iter().filter(|o| o.passed).min_by(order)
}

/// The model settings a deployment of a release runs on: `chosen`'s model and effort are
/// the defaults, and only the `passing` candidates' models are allowed, so its tasks can't
/// ask for a model it wasn't evaluated on. Efforts are kept to the passing candidates'
/// when each of them names one. Every candidate must name its model (an evaluation
/// records the model that ran).
pub fn pin(chosen: &Route, passing: &[Route]) -> Result<Models> {
    if !passing.contains(chosen) {
        bail!("{chosen} didn't pass, so it can't be pinned");
    }
    if let Some(route) = passing.iter().find(|r| r.model.is_none()) {
        bail!("{route} names no model; pin the model its evaluation ran on");
    }
    let mut models = Models::new();
    for route in passing {
        if models.contains_key(&route.provider) {
            continue;
        }
        let routes: Vec<&Route> = passing
            .iter()
            .filter(|r| r.provider == route.provider)
            .collect();
        let lead = if chosen.provider == route.provider {
            chosen
        } else {
            routes[0]
        };
        let mut allowed: Vec<String> = Vec::new();
        for model in routes.iter().filter_map(|r| r.model.clone()) {
            if !allowed.iter().any(|m| m.eq_ignore_ascii_case(&model)) {
                allowed.push(model);
            }
        }
        let mut efforts: Vec<String> = Vec::new();
        if routes.iter().all(|r| r.effort.is_some()) {
            for effort in routes.iter().filter_map(|r| r.effort.clone()) {
                if !efforts.contains(&effort) {
                    efforts.push(effort);
                }
            }
            efforts.sort_by_key(|e| crate::models::EFFORTS.iter().position(|x| *x == e.as_str()));
        }
        models.insert(
            route.provider,
            ModelChoice {
                default: lead.model.clone(),
                allowed,
                effort: lead.effort.clone(),
                efforts,
            },
        );
    }
    Ok(models)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::factory::blueprint::from_value;
    use crate::factory::blueprint::tests::{catalog, sample};
    use serde_json::{Value, json};

    fn blueprint(change: impl FnOnce(&mut Value)) -> Blueprint {
        let mut value = sample();
        change(&mut value);
        from_value(&value).unwrap()
    }

    fn route(provider: Provider, model: Option<&str>, effort: Option<&str>) -> Route {
        Route {
            provider,
            model: model.map(Into::into),
            effort: effort.map(Into::into),
        }
    }

    fn offered(id: &str, multiplier: Option<&str>, policy: Option<&str>) -> Offered {
        Offered {
            id: id.into(),
            multiplier: multiplier.map(Into::into),
            policy: policy.map(Into::into),
            ..Offered::default()
        }
    }

    fn why(routes: &Routes) -> Vec<(String, &str)> {
        routes
            .left_out
            .iter()
            .map(|l| (l.route.to_string(), l.why.as_str()))
            .collect()
    }

    #[test]
    fn candidates_the_profile_or_the_agent_rules_out_are_left_out_with_the_reason() {
        let bp = blueprint(|v| {
            v["model"]["providers"] = json!(["copilot", "claude", "codex"]);
            v["model"]["candidates"] = json!([
                { "provider": "copilot", "model": "gpt-6-sol", "effort": "medium" },
                { "provider": "copilot", "model": "gpt-5-mini" },
                { "provider": "copilot", "model": "claude-sonnet-5.5", "effort": "max" },
                { "provider": "codex", "model": "gpt-6-codex" },
                { "provider": "claude", "model": "claude-opus-5.5" },
                { "provider": "copilot", "model": "gpt-6-sol", "effort": "medium" }
            ]);
        });
        let mut catalog = catalog();
        catalog.providers.insert(Provider::Codex);
        let routes = candidates(&bp, &catalog, None, true);
        assert_eq!(
            routes.candidates,
            [
                route(Provider::Copilot, Some("gpt-6-sol"), Some("medium")),
                route(Provider::Claude, Some("claude-opus-5.5"), None),
            ]
        );
        assert_eq!(
            why(&routes),
            [
                (
                    "copilot gpt-5-mini".into(),
                    "the profile doesn't allow gpt-5-mini on copilot"
                ),
                (
                    "copilot claude-sonnet-5.5 (max reasoning)".into(),
                    "the profile doesn't allow max reasoning on copilot"
                ),
                (
                    "codex gpt-6-codex".into(),
                    "it needs MCP servers, which Codex can't use here"
                ),
                (
                    "copilot gpt-6-sol (medium reasoning)".into(),
                    "it is listed twice"
                ),
            ]
        );

        // The account's offers: what it doesn't offer, a model its policy turned off, and
        // one that costs more than max_multiplier (1.5x) is left out.
        let mut catalog = crate::factory::blueprint::tests::catalog();
        catalog.models.clear();
        let bp = blueprint(|v| {
            v["model"]["candidates"] = json!([
                { "provider": "copilot", "model": "gpt-6-sol" },
                { "provider": "copilot", "model": "gpt-6-ultra" },
                { "provider": "copilot", "model": "o9" },
                { "provider": "copilot", "model": "gpt-7" },
                { "provider": "copilot", "model": "mystery" }
            ]);
        });
        let offers: Offers = [(
            Provider::Copilot,
            vec![
                offered("gpt-6-sol", Some("1"), None),
                offered("gpt-6-ultra", Some("3"), None),
                offered("o9", Some("0.33"), Some("disabled")),
                offered("mystery", None, None),
            ],
        )]
        .into();
        let routes = candidates(&bp, &catalog, Some(&offers), true);
        assert_eq!(
            routes.candidates,
            [route(Provider::Copilot, Some("gpt-6-sol"), None)]
        );
        assert_eq!(
            why(&routes),
            [
                (
                    "copilot gpt-6-ultra".into(),
                    "it costs 3x, over its max_multiplier of 1.5x"
                ),
                (
                    "copilot o9".into(),
                    "o9 is disabled for your copilot account"
                ),
                (
                    "copilot gpt-7".into(),
                    "your copilot account doesn't offer gpt-7"
                ),
                (
                    "copilot mystery".into(),
                    "copilot gives no cost multiplier for mystery, so max_multiplier can't be checked"
                ),
            ]
        );

        // No candidates: each provider's default. Microsoft 365 content goes only to the
        // owner's own workers.
        let plain = blueprint(|v| {
            v["model"]["candidates"] = json!([]);
            v["capabilities"]["workiq"] = json!(["email"]);
        });
        let own = candidates(
            &plain,
            &crate::factory::blueprint::tests::catalog(),
            None,
            true,
        );
        assert_eq!(
            own.candidates,
            [route(Provider::Copilot, Some("gpt-6-sol"), Some("low"))],
            "the default model, at the least effort allowed"
        );
        let elsewhere = candidates(
            &plain,
            &crate::factory::blueprint::tests::catalog(),
            None,
            false,
        );
        assert!(elsewhere.candidates.is_empty());
        assert_eq!(
            elsewhere.left_out[0].why,
            "it works with Microsoft 365 content, which goes only to your own workers"
        );
    }

    fn outcome(model: &str, passed: bool, rate: f64, secs: f64, cost: f64) -> Outcome {
        Outcome {
            route: route(Provider::Copilot, Some(model), None),
            passed,
            pass_rate: rate,
            p95_secs: Some(secs),
            multiplier: Some(cost),
            judge_score: None,
        }
    }

    #[test]
    fn the_recommendation_follows_prefer_among_the_candidates_that_passed() {
        let outcomes = [
            outcome("cheap", true, 0.92, 300.0, 0.33),
            outcome("fast", true, 0.95, 60.0, 1.0),
            outcome("best", true, 1.0, 200.0, 3.0),
            outcome("cheapest", false, 0.5, 30.0, 0.1),
        ];
        let pick = |prefer| recommend(&outcomes, prefer).map(|o| o.route.model.clone().unwrap());
        assert_eq!(pick(Prefer::Cost).as_deref(), Some("cheap"));
        assert_eq!(pick(Prefer::Latency).as_deref(), Some("fast"));
        assert_eq!(pick(Prefer::Quality).as_deref(), Some("best"));
        // Ties go to the better pass rate, then to the first listed; nothing passed: none.
        let tied = [
            outcome("a", true, 0.9, 100.0, 1.0),
            outcome("b", true, 0.95, 100.0, 1.0),
            outcome("c", true, 0.95, 100.0, 1.0),
        ];
        assert_eq!(
            recommend(&tied, Prefer::Cost)
                .unwrap()
                .route
                .model
                .as_deref(),
            Some("b")
        );
        let unknown = [
            Outcome {
                multiplier: None,
                ..outcome("unknown", true, 1.0, 10.0, 0.0)
            },
            outcome("known", true, 0.9, 100.0, 2.0),
        ];
        assert_eq!(
            recommend(&unknown, Prefer::Cost)
                .unwrap()
                .route
                .model
                .as_deref(),
            Some("known"),
            "an unknown cost isn't a low one"
        );
        assert_eq!(recommend(&outcomes[3..], Prefer::Cost), None);
    }

    #[test]
    fn pinned_settings_keep_tasks_to_the_models_that_passed() {
        let chosen = route(Provider::Copilot, Some("gpt-6-sol"), Some("medium"));
        let passing = [
            route(Provider::Copilot, Some("claude-sonnet-5.5"), Some("high")),
            chosen.clone(),
            route(Provider::Claude, Some("claude-opus-5.5"), None),
        ];
        let pinned = pin(&chosen, &passing).unwrap();
        let copilot = crate::models::choice(&pinned, Provider::Copilot);
        assert_eq!(copilot.pick(None).unwrap().as_deref(), Some("gpt-6-sol"));
        assert_eq!(
            copilot.pick_effort(None).unwrap().as_deref(),
            Some("medium")
        );
        assert_eq!(copilot.efforts, ["medium", "high"]);
        assert!(copilot.pick(Some("claude-sonnet-5.5")).is_ok());
        let refused = copilot.pick(Some("gpt-5-mini")).unwrap_err().to_string();
        assert!(
            refused.contains("gpt-5-mini isn't a model allowed here"),
            "{refused}"
        );
        assert!(copilot.pick_effort(Some("max")).is_err());
        let claude = crate::models::choice(&pinned, Provider::Claude);
        assert_eq!(claude.allowed, ["claude-opus-5.5"]);
        assert!(
            claude.efforts.is_empty(),
            "it passed at the model's own effort"
        );
        assert!(crate::models::choice(&pinned, Provider::Codex).is_empty());

        let unnamed = route(Provider::Copilot, None, None);
        assert!(pin(&unnamed, std::slice::from_ref(&unnamed)).is_err());
        assert!(
            pin(&chosen, &passing[..1]).is_err(),
            "only a candidate that passed"
        );
    }
}
