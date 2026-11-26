//! Per-agent metrics by version (docs/agent-factory.md, section 11), from the audit logs of
//! the workers its releases were deployed on: tasks, done, failed, refused (by what refused
//! them), p50 and p95 duration, attempts, check pass rate, model mix and usage where
//! providers report it. Audit entries hold no prompt or output text, so neither do these.

use std::collections::BTreeMap;

use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::audit::{AuditEntry, AuditLog, Outcome};
use crate::factory::not_found;
use crate::factory::store::{AgentState, FactoryStore};
use crate::llm::Usage;
use crate::profile::Profile;

/// One release's metrics.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct VersionMetrics {
    pub version: String,
    pub tasks: usize,
    pub done: usize,
    pub failed: usize,
    pub cancelled: usize,
    pub refused: usize,
    /// What refused them: `budget`, `canary`, `stopped`, `release`, or `other`.
    pub refused_by: BTreeMap<String, usize>,
    pub p50_secs: f64,
    pub p95_secs: f64,
    /// Model attempts per task that ran, on average.
    pub attempts: f64,
    /// The share of checks that passed, of the tasks that ran checks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub check_pass_rate: Option<f64>,
    /// Tasks by the model that did the work.
    pub models: BTreeMap<String, usize>,
    /// What the tasks used, added up, where providers report it.
    pub usage: Usage,
}

/// An agent's metrics since a time, by version, newest first.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Metrics {
    pub agent: String,
    pub since: String,
    pub versions: Vec<VersionMetrics>,
}

/// The `p`-quantile of `secs` (nearest rank); 0 for none.
fn quantile(secs: &[f64], p: f64) -> f64 {
    if secs.is_empty() {
        return 0.0;
    }
    let mut sorted = secs.to_vec();
    sorted.sort_by(f64::total_cmp);
    let rank = ((p * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len());
    sorted[rank - 1]
}

/// The metrics of agent `name` from `entries` (any worker's audit), those since `since`.
pub fn of_entries(name: &str, entries: &[AuditEntry], since: DateTime<Utc>) -> Metrics {
    let prefix = format!("{name}@");
    let mut by_version: BTreeMap<String, Vec<&AuditEntry>> = BTreeMap::new();
    for entry in entries {
        let Some(version) = entry.agent.as_deref().and_then(|a| a.strip_prefix(&prefix)) else {
            continue;
        };
        if (entry.ts_ms as i64) < since.timestamp_millis() {
            continue;
        }
        by_version
            .entry(version.to_owned())
            .or_default()
            .push(entry);
    }
    let mut versions: Vec<VersionMetrics> = by_version
        .into_iter()
        .map(|(version, list)| {
            let mut m = VersionMetrics {
                version,
                tasks: list.len(),
                ..VersionMetrics::default()
            };
            let mut secs = Vec::new();
            let (mut attempts, mut ran) = (0u32, 0u32);
            let (mut passed, mut checks) = (0u32, 0u32);
            for e in &list {
                match e.outcome {
                    Outcome::Done => m.done += 1,
                    Outcome::Failed => m.failed += 1,
                    Outcome::Cancelled => m.cancelled += 1,
                    Outcome::Refused => {
                        m.refused += 1;
                        *m.refused_by
                            .entry(e.refused_by.clone().unwrap_or_else(|| "other".into()))
                            .or_default() += 1;
                        continue;
                    }
                }
                secs.push(e.duration_ms as f64 / 1000.0);
                if let Some(n) = e.attempts {
                    attempts += n;
                    ran += 1;
                }
                if let Some(tally) = e.checks {
                    passed += tally.passed;
                    checks += tally.total;
                }
                if let Some(model) = &e.model {
                    *m.models.entry(model.clone()).or_default() += 1;
                }
                if let Some(usage) = &e.usage {
                    m.usage = m.usage.plus(usage);
                }
            }
            m.p50_secs = quantile(&secs, 0.5);
            m.p95_secs = quantile(&secs, 0.95);
            m.attempts = if ran == 0 {
                0.0
            } else {
                f64::from(attempts) / f64::from(ran)
            };
            m.check_pass_rate = (checks > 0).then(|| f64::from(passed) / f64::from(checks));
            m
        })
        .collect();
    versions.sort_by(|a, b| {
        match (
            a.version.parse::<crate::factory::release::Version>(),
            b.version.parse::<crate::factory::release::Version>(),
        ) {
            (Ok(a), Ok(b)) => b.cmp(&a),
            _ => b.version.cmp(&a.version),
        }
    });
    Metrics {
        agent: name.to_owned(),
        since: since.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        versions,
    }
}

/// Agent `name`'s metrics since `since`, from the audit logs of `owner`'s workers it is
/// (or was) deployed on.
pub fn metrics(owner: &Profile, name: &str, since: DateTime<Utc>) -> Result<Metrics> {
    let store = FactoryStore::of(owner);
    let state = store
        .state(name)?
        .ok_or_else(|| not_found(format!("the factory has no agent '{name}'")))?;
    of_state(owner, &state, since)
}

fn of_state(owner: &Profile, state: &AgentState, since: DateTime<Utc>) -> Result<Metrics> {
    let mut entries = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for deployment in &state.deployments {
        if !seen.insert(deployment.worker.clone()) {
            continue;
        }
        if let Ok(worker) = crate::workers::worker_of(owner, &deployment.worker) {
            entries.extend(AuditLog::read(&worker.audit_path(), usize::MAX)?);
        }
    }
    Ok(of_entries(&state.name, &entries, since))
}

/// The ops report's section on the agents `owner` deployed (`proofkin ops report`): each
/// one's deployments now, and its tasks since `since` by version. None when it deployed none.
pub fn ops_section(owner: &Profile, since: DateTime<Utc>) -> Result<Option<String>> {
    let store = FactoryStore::of(owner);
    let mut agents = Vec::new();
    for name in store.names()? {
        if let Some(state) = store.state(&name)?
            && !state.deployments.is_empty()
        {
            // An audit it can't read leaves that agent's tasks out, not the whole report.
            let metrics = of_state(owner, &state, since)
                .unwrap_or_else(|_| of_entries(&state.name, &[], since));
            agents.push((state, metrics));
        }
    }
    Ok((!agents.is_empty()).then(|| section(&agents)))
}

fn section(agents: &[(AgentState, Metrics)]) -> String {
    let mut md = String::from(
        "\n## Agents\n\nThe factory's deployed agents, their tasks by release (`proofkin factory metrics NAME`).\n\n\
         | Agent | Release | Deployed on | Tasks | Done | Failed | Refused | p95 | Attempts | Checks passed |\n\
         | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |\n",
    );
    for (state, metrics) in agents {
        let deployed = |version: &str| {
            state
                .deployments
                .iter()
                .filter(|d| d.version == version)
                .map(|d| format!("{} ({})", d.worker, d.state))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let mut versions: Vec<&VersionMetrics> = metrics.versions.iter().collect();
        let idle: Vec<VersionMetrics> = state
            .deployments
            .iter()
            .filter(|d| !metrics.versions.iter().any(|v| v.version == d.version))
            .map(|d| VersionMetrics {
                version: d.version.clone(),
                ..VersionMetrics::default()
            })
            .collect();
        for v in &idle {
            if !versions.iter().any(|x| x.version == v.version) {
                versions.push(v);
            }
        }
        for v in versions {
            let refused = if v.refused == 0 {
                "0".to_owned()
            } else {
                let by: Vec<String> = v
                    .refused_by
                    .iter()
                    .map(|(why, n)| format!("{why} {n}"))
                    .collect();
                format!("{} ({})", v.refused, by.join(", "))
            };
            let ran = v.tasks > v.refused;
            md += &format!(
                "| {} | {}{} | {} | {} | {} | {} | {refused} | {} | {} | {} |\n",
                state.name,
                v.version,
                if state.active_version.as_deref() == Some(v.version.as_str()) {
                    " (active)"
                } else {
                    ""
                },
                match deployed(&v.version) {
                    d if d.is_empty() => "-".to_owned(),
                    d => d,
                },
                v.tasks,
                v.done,
                v.failed,
                if ran {
                    format!("{:.0}s", v.p95_secs)
                } else {
                    "-".into()
                },
                if ran {
                    format!("{:.1}", v.attempts)
                } else {
                    "-".into()
                },
                v.check_pass_rate
                    .map(|r| format!("{:.0}%", r * 100.0))
                    .unwrap_or_else(|| "-".into()),
            );
        }
    }
    md
}

/// `since` as the command line and the API take it: `7d`, `24h` or `30m` before now, or a
/// date (`2026-11-17`) or time (RFC 3339).
pub fn parse_since(text: &str, now: DateTime<Utc>) -> Result<DateTime<Utc>> {
    let text = text.trim();
    let ago = |n: i64, unit: char| match unit {
        'd' => Some(chrono::Duration::days(n)),
        'h' => Some(chrono::Duration::hours(n)),
        'm' => Some(chrono::Duration::minutes(n)),
        _ => None,
    };
    if let Some(unit) = text.chars().last()
        && let Ok(n) = text[..text.len() - 1].parse::<i64>()
        && let Some(span) = ago(n, unit)
        && n > 0
    {
        return Ok(now - span);
    }
    if let Ok(t) = DateTime::parse_from_rfc3339(text) {
        return Ok(t.with_timezone(&Utc));
    }
    if let Ok(day) = chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d") {
        return Ok(day.and_hms_opt(0, 0, 0).expect("midnight").and_utc());
    }
    bail!("{text:?} isn't a time: give 7d, 24h, 30m, a date (2026-11-17) or a time (RFC 3339)")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::CheckTally;
    use crate::llm::{LlmTask, Provider};

    fn entry(agent: &str, outcome: Outcome, secs: u64, ts_ms: u64) -> AuditEntry {
        let task = LlmTask::new(Provider::Copilot, "secret prompt text");
        let id = iroh::SecretKey::generate().public();
        let mut e = AuditEntry::new(
            "bob",
            &id,
            1,
            &task,
            (outcome != Outcome::Refused).then_some(crate::llm::Access::ReadOnly),
            &Ok(String::new()),
            std::time::SystemTime::now(),
        );
        e.outcome = outcome;
        e.duration_ms = secs * 1000;
        e.ts_ms = ts_ms;
        e.agent = Some(agent.to_owned());
        e
    }

    #[test]
    fn metrics_are_per_version_from_the_audit_and_hold_no_text() {
        let day = 86_400_000u64;
        let now = DateTime::from_timestamp_millis((30 * day) as i64).unwrap();
        let mut entries = Vec::new();
        for secs in [10, 20, 30, 40] {
            let mut e = entry("fin-analyst@1.2.0", Outcome::Done, secs, 29 * day);
            e.model = Some("gpt-6-sol".into());
            e.attempts = Some(if secs == 40 { 2 } else { 1 });
            e.checks = Some(CheckTally {
                passed: if secs == 40 { 0 } else { 1 },
                total: 1,
            });
            e.usage = Some(Usage {
                input_tokens: Some(100),
                output_tokens: Some(10),
                ..Usage::default()
            });
            entries.push(e);
        }
        entries.push(entry("fin-analyst@1.2.0", Outcome::Failed, 50, 29 * day));
        let mut refused = entry("fin-analyst@1.2.0", Outcome::Refused, 0, 29 * day);
        refused.refused_by = Some("budget".into());
        entries.push(refused);
        entries.push(entry("fin-analyst@1.1.0", Outcome::Done, 5, 29 * day));
        // Too old, and another agent's: not counted.
        entries.push(entry("fin-analyst@1.1.0", Outcome::Done, 5, day));
        entries.push(entry("other@1.0.0", Outcome::Done, 5, 29 * day));
        let since = parse_since("7d", now).unwrap();
        let m = of_entries("fin-analyst", &entries, since);
        let [new, old] = &m.versions[..] else {
            panic!("{m:?}")
        };
        assert_eq!(new.version, "1.2.0");
        assert_eq!((new.tasks, new.done, new.failed, new.refused), (6, 4, 1, 1));
        assert_eq!(new.refused_by.get("budget"), Some(&1));
        assert_eq!((new.p50_secs, new.p95_secs), (30.0, 50.0));
        assert_eq!(new.attempts, 1.25);
        assert_eq!(new.check_pass_rate, Some(0.75));
        assert_eq!(new.models.get("gpt-6-sol"), Some(&4));
        assert_eq!(new.usage.input_tokens, Some(400));
        assert_eq!((old.version.as_str(), old.tasks), ("1.1.0", 1));
        let json = serde_json::to_string(&m).unwrap();
        assert!(!json.contains("secret prompt text"), "{json}");
    }

    #[test]
    fn the_ops_report_lists_each_deployed_agents_releases_with_their_tasks() {
        use crate::factory::store::{DeployState, DeploymentRecord, Lifecycle};
        let day = 86_400_000u64;
        let now = DateTime::from_timestamp_millis((30 * day) as i64).unwrap();
        let mut entries = vec![entry("fin-analyst@1.1.0", Outcome::Done, 20, 29 * day)];
        entries[0].attempts = Some(1);
        let mut e = entry("fin-analyst@1.1.0", Outcome::Done, 40, 29 * day);
        e.attempts = Some(2);
        e.checks = Some(CheckTally {
            passed: 1,
            total: 2,
        });
        entries.push(e);
        let mut refused = entry("fin-analyst@1.1.0", Outcome::Refused, 0, 29 * day);
        refused.refused_by = Some("budget".into());
        entries.push(refused);
        let deployed = |worker: &str, version: &str, state| DeploymentRecord {
            worker: worker.into(),
            version: version.into(),
            state,
            previous_version: None,
            canary: None,
            folders: Vec::new(),
            budgets: Default::default(),
            note: None,
            at: String::new(),
        };
        let state = AgentState {
            name: "fin-analyst".into(),
            state: Lifecycle::Active,
            active_version: Some("1.2.0".into()),
            versions: vec!["1.2.0".into(), "1.1.0".into()],
            deployments: vec![
                deployed("fin-analyst", "1.1.0", DeployState::Running),
                deployed("fin-canary", "1.2.0", DeployState::Canary),
            ],
            replacement: None,
            purge_after: None,
            history: Vec::new(),
        };
        let metrics = of_entries("fin-analyst", &entries, parse_since("7d", now).unwrap());
        let md = section(&[(state, metrics)]);
        for row in [
            "| fin-analyst | 1.1.0 | fin-analyst (running) | 3 | 2 | 0 | 1 (budget 1) | 40s | 1.5 | 50% |",
            "| fin-analyst | 1.2.0 (active) | fin-canary (canary) | 0 | 0 | 0 | 0 | - | - | - |",
        ] {
            assert!(md.contains(row), "{row:?} isn't in:\n{md}");
        }
        assert!(md.starts_with("\n## Agents\n"), "{md}");
    }

    #[test]
    fn since_takes_spans_dates_and_times() {
        let now = DateTime::parse_from_rfc3339("2026-11-24T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(
            parse_since("7d", now).unwrap().to_rfc3339(),
            "2026-11-17T12:00:00+00:00"
        );
        assert_eq!(
            parse_since("2026-11-17", now).unwrap().to_rfc3339(),
            "2026-11-17T00:00:00+00:00"
        );
        assert!(parse_since("soon", now).is_err());
        assert!(parse_since("0d", now).is_err());
    }
}
