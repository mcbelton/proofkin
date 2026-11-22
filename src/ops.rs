//! Operations: keep a running system healthy and learn from it.
//!
//! - **Monitors**: a folder owner's check (e.g. `health` = `curl -f http://localhost:8080/health`)
//!   run on a schedule by the owner's agent. Every run is a metric sample
//!   (`ops/metrics.jsonl`: ok, latency, exit code).
//! - **Incidents and alerts**: after `fail_after` failures in a row an incident
//!   opens, and the agent alerts (its user, and its peers by chat).
//! - **Auto-fix with TSGs**: troubleshooting guides in the knowledge base whose
//!   monitor and failure text match run their fix check, then the verify check.
//! - **Debugging agent**: if no guide fixes it, the agent's model diagnoses the
//!   failure read-only (with the knowledge base). If it names one of the owner's
//!   checks as the fix with at least [`CONFIDENT`] percent confidence, that check is
//!   run; if the monitor then passes, the diagnosis becomes a new active TSG, so the
//!   next occurrence is fixed without the model. Otherwise a draft TSG is written
//!   for a person to review and the incident is escalated.
//! - **Reports**: availability against each monitor's SLO, latency, incidents and
//!   how they were resolved, peer task and evidence run statistics.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::audit::{AuditEntry, Outcome};
use crate::evidence::{CHECK_TIMEOUT, CheckResult, run_check};
use crate::knowledge::{KnowledgeBase, Tsg};
use crate::llm::{Access, Executor, LlmTask, TaskContext};
use crate::profile::validate_name;
use crate::runs::{RunSummary, unix_now, utc};
use crate::shares::ShareStore;

/// Least confidence (percent) for the debugging agent's fix to be tried.
pub const CONFIDENT: u8 = 80;
/// Output a failure alert or prompt carries.
const OUTPUT_CHARS: usize = 3_000;

fn default_fail_after() -> u32 {
    3
}
fn default_slo() -> f64 {
    99.0
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Monitor {
    pub name: String,
    /// Shared folder whose check it runs (and where fixes run).
    pub share: String,
    pub check: String,
    pub every_secs: u64,
    #[serde(default = "default_fail_after")]
    pub fail_after: u32,
    /// Availability objective, percent.
    #[serde(default = "default_slo")]
    pub slo: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MonitorStore {
    #[serde(default)]
    pub monitors: Vec<Monitor>,
}

impl MonitorStore {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.is_file() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path)?;
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        crate::store::write_atomic(path, &serde_json::to_vec_pretty(self)?)
    }

    /// Loads the store at `path`, changes it with `change` and saves it, holding its
    /// lock, so changes from the web UI and the CLI never overwrite each other.
    pub fn update<T>(
        path: &Path,
        change: impl FnOnce(&mut MonitorStore) -> Result<T>,
    ) -> Result<T> {
        let _lock = crate::store::FileLock::acquire(path)?;
        let mut store = Self::load(path)?;
        let out = change(&mut store)?;
        store.save(path)?;
        Ok(out)
    }

    /// Adds or replaces a monitor; its folder and check must exist in `shares`.
    pub fn set(&mut self, monitor: Monitor, shares: &ShareStore) -> Result<()> {
        validate_name(&monitor.name).context("invalid monitor name")?;
        let share = shares
            .get(&monitor.share)
            .with_context(|| format!("there is no shared folder '{}'", monitor.share))?;
        if !share.checks.contains_key(&monitor.check) {
            bail!(
                "folder '{}' has no check '{}'; add it with share add ... --check {}=<command>",
                monitor.share,
                monitor.check,
                monitor.check
            );
        }
        if monitor.every_secs < 10 {
            bail!("monitors run at most every 10 seconds");
        }
        if !(0.0..=100.0).contains(&monitor.slo) || monitor.fail_after == 0 {
            bail!("the SLO must be a percentage and fail-after at least 1");
        }
        self.monitors.retain(|m| m.name != monitor.name);
        self.monitors.push(monitor);
        self.monitors.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(())
    }

    pub fn remove(&mut self, name: &str) -> bool {
        let before = self.monitors.len();
        self.monitors.retain(|m| m.name != name);
        before != self.monitors.len()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sample {
    pub ts: u64,
    pub monitor: String,
    pub ok: bool,
    pub ms: u64,
    pub exit: Option<i32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IncidentKind {
    /// The monitor failed `fail_after` times in a row.
    Opened,
    /// A troubleshooting guide's fix was tried.
    Tsg,
    /// The debugging agent diagnosed it.
    Assessed,
    /// A fix the debugging agent found worked and became a TSG.
    Learned,
    /// It needs a person.
    Escalated,
    Resolved,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncidentEvent {
    pub ts: u64,
    /// The `ts` of the event that opened the incident.
    pub incident: u64,
    pub monitor: String,
    pub kind: IncidentKind,
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<CheckResult>,
}

/// The ops logs of a profile (`ops/metrics.jsonl`, `ops/incidents.jsonl`).
#[derive(Debug, Clone)]
pub struct OpsStore {
    dir: PathBuf,
}

fn append(path: &Path, value: &impl Serialize) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    file.write_all(&line)?;
    Ok(())
}

fn read_since<T: for<'de> Deserialize<'de>>(path: &Path, keep: impl Fn(&T) -> bool) -> Vec<T> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<T>(l).ok())
        .filter(|v| keep(v))
        .collect()
}

impl OpsStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn record_sample(&self, sample: &Sample) -> Result<()> {
        append(&self.dir.join("metrics.jsonl"), sample)
    }

    pub fn record_incident(&self, event: &IncidentEvent) -> Result<()> {
        append(&self.dir.join("incidents.jsonl"), event)
    }

    pub fn samples(&self, since: u64) -> Vec<Sample> {
        read_since(&self.dir.join("metrics.jsonl"), |s: &Sample| s.ts >= since)
    }

    pub fn incidents(&self, since: u64) -> Vec<IncidentEvent> {
        read_since(&self.dir.join("incidents.jsonl"), |e: &IncidentEvent| {
            e.incident >= since || e.ts >= since
        })
    }

    /// Closes `monitor`'s open incidents saying `why` (nothing watches them any more);
    /// how many it closed.
    pub fn close_incidents(&self, monitor: &str, why: &str) -> Result<usize> {
        let open: Vec<u64> = open_incidents(&self.incidents(0))
            .into_iter()
            .filter(|(_, m)| m == monitor)
            .map(|(incident, _)| incident)
            .collect();
        for incident in &open {
            self.record_incident(&IncidentEvent {
                ts: unix_now(),
                incident: *incident,
                monitor: monitor.to_owned(),
                kind: IncidentKind::Resolved,
                text: why.to_owned(),
                checks: vec![],
            })?;
        }
        Ok(open.len())
    }
}

/// Removes monitor `name` from the profile's `monitors` file and closes its open
/// incidents; false if there is no such monitor.
pub fn remove_monitor(monitors: &Path, store: &OpsStore, name: &str) -> Result<bool> {
    if !MonitorStore::update(monitors, |list| Ok(list.remove(name)))? {
        return Ok(false);
    }
    store.close_incidents(name, "Closed: the monitor was removed")?;
    Ok(true)
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MonitorStats {
    pub name: String,
    pub samples: usize,
    pub availability: Option<f64>,
    pub slo: f64,
    pub meets_slo: Option<bool>,
    pub p50_ms: Option<u64>,
    pub p95_ms: Option<u64>,
    pub last_ok: Option<bool>,
    pub last_ts: Option<u64>,
    pub open_incident: bool,
    /// Why it can't run (its folder or check is gone), if so.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

/// Why monitor `m` can't run with `shares`: its folder isn't shared or has no such
/// check any more. Such a monitor records no samples and opens no incidents.
pub fn problem(m: &Monitor, shares: &ShareStore) -> Option<String> {
    match shares.get(&m.share) {
        None => Some(format!("folder '{}' isn't shared any more", m.share)),
        Some(share) if !share.checks.contains_key(&m.check) => Some(format!(
            "folder '{}' has no check '{}' any more",
            m.share, m.check
        )),
        Some(_) => None,
    }
}

fn percentile(sorted: &[u64], p: f64) -> Option<u64> {
    if sorted.is_empty() {
        return None;
    }
    let i = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    Some(sorted[i.min(sorted.len() - 1)])
}

/// Incident ids still open (opened, not resolved).
pub fn open_incidents(events: &[IncidentEvent]) -> BTreeMap<u64, String> {
    let mut open = BTreeMap::new();
    for e in events {
        match e.kind {
            IncidentKind::Opened => {
                open.insert(e.incident, e.monitor.clone());
            }
            IncidentKind::Resolved => {
                open.remove(&e.incident);
            }
            _ => {}
        }
    }
    open
}

/// Each monitor's health from `samples`. `incidents` should reach back far enough to
/// hold the incidents still open (e.g. the whole log); `shares` tells which monitors
/// can't run.
pub fn monitor_stats(
    monitors: &[Monitor],
    samples: &[Sample],
    incidents: &[IncidentEvent],
    shares: &ShareStore,
) -> Vec<MonitorStats> {
    let open = open_incidents(incidents);
    monitors
        .iter()
        .map(|m| {
            let mine: Vec<&Sample> = samples.iter().filter(|s| s.monitor == m.name).collect();
            let ok = mine.iter().filter(|s| s.ok).count();
            let availability = (!mine.is_empty()).then(|| ok as f64 * 100.0 / mine.len() as f64);
            let mut ms: Vec<u64> = mine.iter().filter(|s| s.ok).map(|s| s.ms).collect();
            ms.sort_unstable();
            MonitorStats {
                name: m.name.clone(),
                samples: mine.len(),
                availability,
                slo: m.slo,
                meets_slo: availability.map(|a| a >= m.slo),
                p50_ms: percentile(&ms, 0.5),
                p95_ms: percentile(&ms, 0.95),
                last_ok: mine.last().map(|s| s.ok),
                last_ts: mine.last().map(|s| s.ts),
                open_incident: open.values().any(|n| n == &m.name),
                problem: problem(m, shares),
            }
        })
        .collect()
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct TaskStats {
    pub total: usize,
    pub done: usize,
    pub failed: usize,
    pub refused: usize,
    pub cancelled: usize,
    pub avg_secs: Option<f64>,
    /// Tasks by peer, and by provider.
    pub by_peer: BTreeMap<String, usize>,
    pub by_provider: BTreeMap<String, usize>,
}

pub fn task_stats(entries: &[AuditEntry]) -> TaskStats {
    let mut stats = TaskStats {
        total: entries.len(),
        ..Default::default()
    };
    let mut secs = Vec::new();
    for e in entries {
        match e.outcome {
            Outcome::Done => stats.done += 1,
            Outcome::Failed => stats.failed += 1,
            Outcome::Refused => stats.refused += 1,
            Outcome::Cancelled => stats.cancelled += 1,
        }
        if e.outcome != Outcome::Refused {
            secs.push(e.duration_ms as f64 / 1000.0);
        }
        *stats.by_peer.entry(e.peer.clone()).or_default() += 1;
        *stats.by_provider.entry(e.provider.to_string()).or_default() += 1;
    }
    stats.avg_secs = (!secs.is_empty()).then(|| secs.iter().sum::<f64>() / secs.len() as f64);
    stats
}

/// The ops report: health against SLOs, incidents, peer tasks and evidence runs.
pub fn report(
    title: &str,
    hours: u64,
    monitors: &[Monitor],
    shares: &ShareStore,
    store: &OpsStore,
    audit: &[AuditEntry],
    runs: &[RunSummary],
) -> String {
    let since = unix_now().saturating_sub(hours * 3600);
    let samples = store.samples(since);
    let incidents = store.incidents(since);
    let mut md = format!(
        "# Ops report: {title}\n\nLast {hours} hours, up to {} UTC.\n\n## Health\n\n",
        utc(unix_now())
    );
    if monitors.is_empty() {
        md += "No monitors. Add one with `proofkin monitor add`.\n";
    } else {
        md += "| Monitor | Availability | SLO | p50 | p95 | Now |\n| --- | --- | --- | --- | --- | --- |\n";
        // Incidents opened before the window may still be open.
        for s in monitor_stats(monitors, &samples, &store.incidents(0), shares) {
            let now = match (&s.problem, s.open_incident, s.last_ok) {
                (Some(why), _, _) => format!("⚠️ can't run: {why}"),
                (None, true, _) => "🔥 incident open".into(),
                (None, false, Some(true)) => "up".into(),
                (None, false, Some(false)) => "failing".into(),
                (None, false, None) => "unknown".into(),
            };
            md += &format!(
                "| {} | {} | {}{} | {} | {} | {now} |\n",
                s.name,
                s.availability
                    .map(|a| format!("{a:.2}% ({} samples)", s.samples))
                    .unwrap_or_else(|| "no data".into()),
                s.slo,
                match s.meets_slo {
                    Some(true) => "% ✅",
                    Some(false) => "% ❌ missed",
                    None => "%",
                },
                s.p50_ms.map(|m| format!("{m} ms")).unwrap_or_default(),
                s.p95_ms.map(|m| format!("{m} ms")).unwrap_or_default(),
            );
        }
    }
    md += "\n## Incidents\n\n";
    let opened: Vec<&IncidentEvent> = incidents
        .iter()
        .filter(|e| e.kind == IncidentKind::Opened)
        .collect();
    if opened.is_empty() {
        md += "None.\n";
    }
    for o in opened {
        let events: Vec<&IncidentEvent> = incidents
            .iter()
            .filter(|e| e.incident == o.incident)
            .collect();
        let resolved = events.iter().find(|e| e.kind == IncidentKind::Resolved);
        md += &format!(
            "- **{}** at {} UTC: {}\n",
            o.monitor,
            utc(o.ts),
            match resolved {
                Some(r) => format!("resolved after {}s ({})", r.ts.saturating_sub(o.ts), r.text),
                None => "**open**".into(),
            }
        );
        for e in events
            .iter()
            .filter(|e| !matches!(e.kind, IncidentKind::Opened | IncidentKind::Resolved))
        {
            md += &format!(
                "  - {:?}: {}\n",
                e.kind,
                e.text.lines().next().unwrap_or_default()
            );
        }
    }
    let tasks = task_stats(audit);
    md += &format!(
        "\n## Peer tasks\n\n{} tasks: {} done, {} failed, {} refused, {} cancelled{}.\n",
        tasks.total,
        tasks.done,
        tasks.failed,
        tasks.refused,
        tasks.cancelled,
        tasks
            .avg_secs
            .map(|s| format!("; {s:.1}s on average"))
            .unwrap_or_default()
    );
    for (peer, n) in &tasks.by_peer {
        md += &format!("- {peer}: {n}\n");
    }
    let recent: Vec<&RunSummary> = runs.iter().filter(|r| r.started_unix >= since).collect();
    md += &format!(
        "\n## Evidence runs\n\n{} runs, {} passed.\n",
        recent.len(),
        recent.iter().filter(|r| r.passed).count()
    );
    md
}

/// What the debugging agent concluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assessment {
    pub confidence: Option<u8>,
    pub fix: Option<String>,
}

/// Reads `Confidence: NN%` and `Fix: <check>` lines from the model's diagnosis.
pub fn parse_assessment(text: &str) -> Assessment {
    let mut a = Assessment {
        confidence: None,
        fix: None,
    };
    for line in text.lines() {
        let clean = line
            .trim()
            .trim_start_matches(['-', '*', '#', ' '])
            .replace('*', "");
        let lower = clean.to_lowercase();
        if let Some(rest) = lower.strip_prefix("confidence:") {
            let digits: String = rest
                .trim()
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            a.confidence = digits.parse::<u8>().ok().filter(|c| *c <= 100);
        } else if lower.starts_with("fix:") {
            let value = clean[4..].trim().trim_matches('`').trim();
            let word = value.split_whitespace().next().unwrap_or_default();
            a.fix = Some(word.trim_matches('`').to_owned())
                .filter(|w| !w.is_empty() && !w.eq_ignore_ascii_case("none"));
        }
    }
    a
}

fn clip(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_owned();
    }
    text.chars().skip(count - max).collect()
}

/// A distinctive line of a failure's output, to recognize it next time.
fn signature(output: &str) -> Option<String> {
    output
        .lines()
        .map(str::trim)
        .filter(|l| l.len() >= 8 && !l.starts_with("[..."))
        .find(|l| l.to_lowercase().contains("error") || l.to_lowercase().contains("fail"))
        .or_else(|| output.lines().map(str::trim).find(|l| l.len() >= 8))
        .map(|l| l.chars().take(80).collect())
}

/// The monitors as they are now (they can change while the agent runs).
pub type MonitorList = Arc<dyn Fn() -> Vec<Monitor> + Send + Sync>;

/// Everything a monitor's agent needs to watch and heal.
pub struct Ops {
    pub agent_name: String,
    pub monitors: MonitorList,
    /// How often the monitors are read again: added, removed or changed ones are
    /// followed without restarting the agent.
    pub reload: Duration,
    /// The agent's shared folders as they are now (they can change while it runs).
    pub shares: Arc<dyn Fn() -> ShareStore + Send + Sync>,
    pub store: OpsStore,
    pub kb: KnowledgeBase,
    /// Skills; the debugging agent follows `incident-debugging`.
    pub skills: crate::skills::Skills,
    /// The agent's own model, for the debugging agent (None: escalate instead).
    pub executor: Option<Arc<dyn Executor>>,
    /// Called for every incident event (alerting).
    pub notify: Arc<dyn Fn(&IncidentEvent) + Send + Sync>,
}

impl Ops {
    fn event(
        &self,
        incident: u64,
        monitor: &str,
        kind: IncidentKind,
        text: String,
        checks: Vec<CheckResult>,
    ) {
        // Alerts go to peers by chat: secrets in check output stay here.
        let checks = checks
            .into_iter()
            .map(|mut c| {
                c.output = crate::redact::redact(&c.output).0;
                c
            })
            .collect();
        let event = IncidentEvent {
            ts: unix_now(),
            incident,
            monitor: monitor.to_owned(),
            kind,
            text: crate::redact::redact(&text).0,
            checks,
        };
        if let Err(e) = self.store.record_incident(&event) {
            eprintln!("warning: could not record the incident: {e:#}");
        }
        (self.notify)(&event);
    }

    async fn run_named(&self, m: &Monitor, check: &str) -> Option<CheckResult> {
        let shares = (self.shares)();
        let share = shares.get(&m.share)?;
        let command = share.checks.get(check)?;
        Some(run_check(check, command, &share.path, CHECK_TIMEOUT).await)
    }

    /// Runs `monitor`'s check once and records the sample.
    pub async fn probe(&self, m: &Monitor) -> CheckResult {
        let result = match self.run_named(m, &m.check).await {
            Some(r) => r,
            None => CheckResult {
                name: m.check.clone(),
                command: String::new(),
                exit_code: None,
                passed: false,
                millis: 0,
                output: format!("folder '{}' or its check '{}' is gone", m.share, m.check),
            },
        };
        let sample = Sample {
            ts: unix_now(),
            monitor: m.name.clone(),
            ok: result.passed,
            ms: result.millis,
            exit: result.exit_code,
        };
        if let Err(e) = self.store.record_sample(&sample) {
            eprintln!("warning: could not record a metric: {e:#}");
        }
        result
    }

    /// Opens an incident for `failure` and tries to fix it: TSGs first, then the
    /// debugging agent. Returns true if it is resolved.
    pub async fn handle_incident(&self, m: &Monitor, failure: &CheckResult) -> bool {
        self.handle(unix_now(), m, failure).await
    }

    async fn handle(&self, incident: u64, m: &Monitor, failure: &CheckResult) -> bool {
        self.event(
            incident,
            &m.name,
            IncidentKind::Opened,
            format!(
                "{} failed {} times in a row (check {}, {}): {}",
                m.name,
                m.fail_after,
                m.check,
                failure
                    .exit_code
                    .map(|c| format!("exit {c}"))
                    .unwrap_or_else(|| "no exit code".into()),
                clip(failure.output.trim(), 300)
            ),
            vec![failure.clone()],
        );
        for tsg in self
            .kb
            .tsgs()
            .into_iter()
            .filter(|t| t.applies(&m.name, &failure.output))
        {
            if self
                .try_fix(
                    incident,
                    m,
                    &tsg.fix,
                    tsg.verify.as_deref(),
                    &format!("TSG {}", tsg.topic),
                )
                .await
            {
                return true;
            }
        }
        self.debug(incident, m, failure).await
    }

    /// Runs fix check `fix`, then the verify check; resolves the incident if it passes.
    async fn try_fix(
        &self,
        incident: u64,
        m: &Monitor,
        fix: &str,
        verify: Option<&str>,
        how: &str,
    ) -> bool {
        let Some(fixed) = self.run_named(m, fix).await else {
            self.event(
                incident,
                &m.name,
                IncidentKind::Tsg,
                format!("{how}: folder '{}' has no check '{fix}'", m.share),
                vec![],
            );
            return false;
        };
        let verify = verify.unwrap_or(&m.check);
        let verified = self.run_named(m, verify).await;
        let ok = verified.as_ref().is_some_and(|v| v.passed);
        let mut checks = vec![fixed.clone()];
        checks.extend(verified);
        self.event(
            incident,
            &m.name,
            IncidentKind::Tsg,
            format!(
                "{how}: ran {fix} ({}), then {verify}: {}",
                if fixed.passed { "ok" } else { "failed" },
                if ok { "passed" } else { "still failing" }
            ),
            checks,
        );
        if ok {
            self.event(
                incident,
                &m.name,
                IncidentKind::Resolved,
                format!("auto-fixed with {how}"),
                vec![],
            );
        }
        ok
    }

    async fn debug(&self, incident: u64, m: &Monitor, failure: &CheckResult) -> bool {
        let Some(executor) = self.executor.clone() else {
            self.event(incident, &m.name, IncidentKind::Escalated, "no troubleshooting guide fixed it and this agent has no model to debug it: needs a person".into(), vec![]);
            return false;
        };
        let shares = (self.shares)();
        let Some(share) = shares.get(&m.share) else {
            return false;
        };
        let Some(provider) = executor.providers().first().copied() else {
            return false;
        };
        let knowledge = self.kb.context(&["all".into()]).unwrap_or_default();
        let skill = self
            .skills
            .instructions(&["incident-debugging".into()])
            .unwrap_or_default();
        let checks: Vec<String> = share
            .checks
            .iter()
            .map(|(name, cmd)| format!("- `{name}`: `{cmd}`"))
            .collect();
        let prompt = crate::boundary::guard(&format!(
            "You are the debugging agent of a running service.\n{skill}\n---\nMonitor `{}` (check `{}`) is failing.\n\
             Its latest output:\n{}\n\
             Knowledge about this system:\n{}\n\n\
             Investigate (read-only: inspect files and logs in this folder; change nothing). \
             Explain the most likely root cause with evidence. The folder owner allows only these \
             commands to change the system:\n{}\n\n\
             End with exactly two lines:\nConfidence: <0-100>%\nFix: <one command name from the list, or none>",
            m.name,
            m.check,
            crate::boundary::wrap(
                &format!("check:{} output", m.check),
                &clip(failure.output.trim(), OUTPUT_CHARS)
            ),
            if knowledge.is_empty() {
                "(none yet)".to_owned()
            } else {
                knowledge
            },
            checks.join("\n")
        ));
        let ctx = TaskContext::new(Access::ReadOnly, share.path.clone());
        let diagnosis = match executor
            .execute(&LlmTask::new(provider, prompt), &ctx)
            .await
        {
            Ok(ran) => ran.output,
            Err(e) => {
                self.event(
                    incident,
                    &m.name,
                    IncidentKind::Escalated,
                    format!("the debugging agent failed ({e:#}): needs a person"),
                    vec![],
                );
                return false;
            }
        };
        let assessment = parse_assessment(&diagnosis);
        self.event(
            incident,
            &m.name,
            IncidentKind::Assessed,
            format!(
                "debugging agent: confidence {}, fix {}\n{}",
                assessment
                    .confidence
                    .map(|c| format!("{c}%"))
                    .unwrap_or_else(|| "unknown".into()),
                assessment.fix.as_deref().unwrap_or("none"),
                clip(diagnosis.trim(), OUTPUT_CHARS)
            ),
            vec![],
        );
        let fix = assessment
            .fix
            .filter(|f| share.checks.contains_key(f) && f != &m.check);
        let confident = assessment.confidence.is_some_and(|c| c >= CONFIDENT);
        let tsg = |status: &str, fix: &str| Tsg {
            topic: format!("tsg-{}-{incident}", m.name),
            monitor: Some(m.name.clone()),
            matches: signature(&failure.output),
            fix: fix.to_owned(),
            verify: None,
            status: status.to_owned(),
        };
        let body = format!(
            "# {} failing: learned by {} on {} UTC\n\nSymptom (check `{}`):\n```text\n{}\n```\n\nDiagnosis by the debugging agent:\n\n{}\n",
            m.name,
            self.agent_name,
            utc(incident),
            m.check,
            clip(failure.output.trim(), 1_000),
            diagnosis.trim()
        );
        match fix {
            Some(fix) if confident => {
                if self
                    .try_fix(incident, m, &fix, None, "the debugging agent's fix")
                    .await
                {
                    let guide = tsg("active", &fix);
                    let _ = self.kb.write(&guide.topic, &guide.render(&body), "");
                    self.event(incident, &m.name, IncidentKind::Learned, format!("new troubleshooting guide {} (active): next time {fix} runs without the model", guide.topic), vec![]);
                    return true;
                }
                let guide = tsg("draft", &fix);
                let _ = self.kb.write(&guide.topic, &guide.render(&body), "");
                self.event(
                    incident,
                    &m.name,
                    IncidentKind::Escalated,
                    format!(
                        "the suggested fix {fix} didn't help; draft guide {} needs a person",
                        guide.topic
                    ),
                    vec![],
                );
                false
            }
            fix => {
                if let Some(fix) = fix {
                    let guide = tsg("draft", &fix);
                    let _ = self.kb.write(&guide.topic, &guide.render(&body), "");
                }
                self.event(incident, &m.name, IncidentKind::Escalated, "not confident enough to fix it automatically: needs a person (see the assessment)".into(), vec![]);
                false
            }
        }
    }

    /// Watches every monitor until the future is dropped, following the monitors as
    /// they change: a new one starts, a removed one stops, a changed one starts over.
    pub async fn run(self: Arc<Self>) {
        let mut tasks = tokio::task::JoinSet::new();
        let mut watching: Vec<(Monitor, tokio::task::AbortHandle)> = Vec::new();
        loop {
            let now = (self.monitors)();
            watching.retain(|(m, watch)| {
                let current = now.contains(m) && !watch.is_finished();
                if !current {
                    watch.abort();
                }
                current
            });
            for m in now {
                if !watching.iter().any(|(w, _)| *w == m) {
                    let ops = self.clone();
                    let watched = m.clone();
                    watching.push((m, tasks.spawn(async move { ops.watch(watched).await })));
                }
            }
            while tasks.try_join_next().is_some() {}
            tokio::time::sleep(self.reload).await;
        }
    }

    async fn watch(&self, m: Monitor) {
        let mut state = self.resume(&m);
        // Until it is removed or changed (then `run` starts the new one).
        while (self.monitors)().contains(&m) {
            self.tick(&m, &mut state).await;
            tokio::time::sleep(Duration::from_secs(m.every_secs)).await;
        }
    }

    /// Where watching `m` resumes after the agent (re)starts: its incidents still
    /// open in the log stay open, so no second one opens for the same failure.
    pub fn resume(&self, m: &Monitor) -> Watch {
        Watch {
            open: open_incidents(&self.store.incidents(0))
                .into_iter()
                .filter(|(_, monitor)| monitor == &m.name)
                .map(|(incident, _)| incident)
                .collect(),
            ..Watch::default()
        }
    }

    /// One check of `m`: records the sample, opens an incident after `fail_after`
    /// failures in a row (unless one is open) and resolves open ones when it passes.
    /// A monitor whose folder or check is gone is skipped: it records nothing and
    /// opens nothing (it isn't the service failing), and its owner is told once.
    pub async fn tick(&self, m: &Monitor, state: &mut Watch) {
        if let Some(why) = problem(m, &(self.shares)()) {
            if !state.cant_run {
                eprintln!(
                    "monitor {} can't run: {why}; fix it, or remove the monitor (under Health in the web UI, or: proofkin monitor remove {})",
                    m.name, m.name
                );
            }
            state.cant_run = true;
            state.failures = 0;
            return;
        }
        state.cant_run = false;
        let result = self.probe(m).await;
        if result.passed {
            state.failures = 0;
            for incident in state.open.drain(..) {
                self.event(
                    incident,
                    &m.name,
                    IncidentKind::Resolved,
                    "recovered".into(),
                    vec![],
                );
            }
            return;
        }
        state.failures += 1;
        if state.failures >= m.fail_after && state.open.is_empty() {
            let incident = unix_now();
            if !self.handle(incident, m, &result).await {
                state.open.push(incident);
            }
            state.failures = 0;
        }
    }
}

/// What watching a monitor remembers between checks.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Watch {
    /// Failures in a row.
    pub failures: u32,
    /// Its incidents still open.
    pub open: Vec<u64>,
    /// Its folder or check is gone (its owner was told).
    pub cant_run: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::Mutex;

    #[test]
    fn assessments_are_parsed_leniently() {
        let a = parse_assessment(
            "Root cause: the cache dir is full.\n**Confidence:** 85%\nFix: `clear-cache`\n",
        );
        assert_eq!(
            a,
            Assessment {
                confidence: Some(85),
                fix: Some("clear-cache".into())
            }
        );
        let none = parse_assessment("Confidence: 40 %\nFix: none");
        assert_eq!(
            none,
            Assessment {
                confidence: Some(40),
                fix: None
            }
        );
        assert_eq!(parse_assessment("no idea").confidence, None);
        assert_eq!(parse_assessment("Confidence: 400%").confidence, None);
    }

    #[test]
    fn stats_and_reports_cover_slos_incidents_and_tasks() {
        let dir = tempfile::tempdir().unwrap();
        let (_service, shares) = service(true);
        let store = OpsStore::new(dir.path());
        let now = unix_now();
        for (i, ok) in [true, true, true, false].iter().enumerate() {
            store
                .record_sample(&Sample {
                    ts: now - 10 + i as u64,
                    monitor: "api".into(),
                    ok: *ok,
                    ms: 100 * (i as u64 + 1),
                    exit: Some(if *ok { 0 } else { 7 }),
                })
                .unwrap();
        }
        store
            .record_incident(&IncidentEvent {
                ts: now,
                incident: now,
                monitor: "api".into(),
                kind: IncidentKind::Opened,
                text: "api failed".into(),
                checks: vec![],
            })
            .unwrap();
        let monitors = vec![Monitor {
            name: "api".into(),
            share: "app".into(),
            check: "health".into(),
            every_secs: 60,
            fail_after: 3,
            slo: 99.0,
        }];
        let stats = monitor_stats(&monitors, &store.samples(0), &store.incidents(0), &shares);
        assert_eq!(stats[0].availability, Some(75.0));
        assert_eq!(stats[0].meets_slo, Some(false));
        assert_eq!(stats[0].p50_ms, Some(200));
        assert!(stats[0].open_incident && stats[0].last_ok == Some(false));
        assert_eq!(stats[0].problem, None);
        let md = report("worker", 24, &monitors, &shares, &store, &[], &[]);
        assert!(
            md.contains("| api | 75.00% (4 samples) | 99% ❌ missed"),
            "{md}"
        );
        assert!(
            md.contains("**open**") && md.contains("| 🔥 incident open |"),
            "{md}"
        );
        store
            .record_incident(&IncidentEvent {
                ts: now + 5,
                incident: now,
                monitor: "api".into(),
                kind: IncidentKind::Resolved,
                text: "recovered".into(),
                checks: vec![],
            })
            .unwrap();
        let md = report("worker", 24, &monitors, &shares, &store, &[], &[]);
        assert!(md.contains("resolved after 5s (recovered)"), "{md}");

        // A monitor whose check is gone says so instead of looking like an outage.
        let gone = ShareStore::default();
        let stats = monitor_stats(&monitors, &store.samples(0), &store.incidents(0), &gone);
        assert_eq!(
            stats[0].problem.as_deref(),
            Some("folder 'app' isn't shared any more")
        );
        let md = report("worker", 24, &monitors, &gone, &store, &[], &[]);
        assert!(
            md.contains("⚠️ can't run: folder 'app' isn't shared any more"),
            "{md}"
        );
    }

    /// A model that diagnoses with a fixed answer.
    struct Doctor(String);

    #[async_trait]
    impl Executor for Doctor {
        fn providers(&self) -> Vec<crate::llm::Provider> {
            vec![crate::llm::Provider::Copilot]
        }
        async fn execute(&self, task: &LlmTask, ctx: &TaskContext) -> Result<crate::llm::Ran> {
            assert_eq!(
                ctx.access,
                Access::ReadOnly,
                "the debugging agent only reads"
            );
            assert!(task.prompt.contains("disk full"), "it sees the failure");
            assert!(
                task.prompt.contains("### Skill: incident-debugging"),
                "it follows the skill"
            );
            Ok(self.0.clone().into())
        }
    }

    /// A service folder whose `health` check fails until `fix` creates `ok.txt`.
    fn service(fix_works: bool) -> (tempfile::TempDir, ShareStore) {
        let dir = tempfile::tempdir().unwrap();
        let (health, fix) = if cfg!(windows) {
            (
                "if exist ok.txt (exit 0) else (echo error: disk full & exit 1)",
                if fix_works {
                    "echo ok> ok.txt"
                } else {
                    "exit 0"
                },
            )
        } else {
            (
                "test -f ok.txt || { echo 'error: disk full'; exit 1; }",
                if fix_works { "touch ok.txt" } else { "true" },
            )
        };
        let mut shares = ShareStore::default();
        let checks = [("health", health), ("clean", fix), ("unbreak", "exit 0")]
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        shares
            .add_with("app", dir.path(), Access::Full, checks, false)
            .unwrap();
        (dir, shares)
    }

    fn ops(
        shares: ShareStore,
        kb: KnowledgeBase,
        store: OpsStore,
        doctor: Option<&str>,
    ) -> (Ops, Arc<Mutex<Vec<IncidentKind>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        let ops = Ops {
            agent_name: "worker".into(),
            monitors: Arc::new(Vec::new),
            reload: Duration::from_millis(50),
            shares: Arc::new(move || shares.clone()),
            store,
            kb,
            skills: crate::skills::Skills::new(std::env::temp_dir().join("p2p-no-skills")),
            executor: doctor.map(|d| Arc::new(Doctor(d.into())) as Arc<dyn Executor>),
            notify: Arc::new(move |e: &IncidentEvent| log.lock().unwrap().push(e.kind)),
        };
        (ops, seen)
    }

    fn monitor() -> Monitor {
        Monitor {
            name: "api".into(),
            share: "app".into(),
            check: "health".into(),
            every_secs: 60,
            fail_after: 1,
            slo: 99.0,
        }
    }

    #[tokio::test]
    async fn a_matching_tsg_fixes_the_incident_without_the_model() {
        let (_dir, shares) = service(true);
        let home = tempfile::tempdir().unwrap();
        let kb = KnowledgeBase::new(home.path().join("kb"));
        let guide = Tsg {
            topic: "disk".into(),
            monitor: Some("api".into()),
            matches: Some("DISK FULL".into()),
            fix: "clean".into(),
            verify: None,
            status: "active".into(),
        };
        kb.write("disk", &guide.render("# Disk full\nClean it."), "")
            .unwrap();
        let (ops, seen) = ops(shares, kb, OpsStore::new(home.path().join("ops")), None);
        let failure = ops.probe(&monitor()).await;
        assert!(!failure.passed);
        assert!(ops.handle_incident(&monitor(), &failure).await);
        assert_eq!(
            *seen.lock().unwrap(),
            [
                IncidentKind::Opened,
                IncidentKind::Tsg,
                IncidentKind::Resolved
            ]
        );
        assert!(
            ops.probe(&monitor()).await.passed,
            "the service is healthy again"
        );
        let events = ops.store.incidents(0);
        assert!(events.last().unwrap().text.contains("TSG disk"));
        assert_eq!(ops.store.samples(0).len(), 2);
    }

    #[tokio::test]
    async fn a_confident_verified_diagnosis_becomes_a_new_tsg() {
        let (_dir, shares) = service(true);
        let home = tempfile::tempdir().unwrap();
        let kb = KnowledgeBase::new(home.path().join("kb"));
        let (ops, seen) = ops(
            shares,
            kb.clone(),
            OpsStore::new(home.path().join("ops")),
            Some("The disk is full of temp files.\nConfidence: 90%\nFix: clean"),
        );
        let failure = ops.probe(&monitor()).await;
        assert!(ops.handle_incident(&monitor(), &failure).await);
        assert_eq!(
            *seen.lock().unwrap(),
            [
                IncidentKind::Opened,
                IncidentKind::Assessed,
                IncidentKind::Tsg,
                IncidentKind::Resolved,
                IncidentKind::Learned
            ]
        );
        let learned = kb.tsgs();
        assert_eq!(learned.len(), 1);
        assert_eq!(learned[0].status, "active");
        assert_eq!(learned[0].fix, "clean");
        assert!(
            learned[0].applies("api", "error: disk full"),
            "{:?}",
            learned[0]
        );
        assert!(
            kb.read(&learned[0].topic)
                .unwrap()
                .contains("The disk is full of temp files.")
        );
    }

    #[tokio::test]
    async fn unsure_or_failed_fixes_escalate_with_a_draft_tsg() {
        // Not confident: nothing runs, a draft guide is written, a person is needed.
        let (_dir, shares) = service(true);
        let home = tempfile::tempdir().unwrap();
        let kb = KnowledgeBase::new(home.path().join("kb"));
        let (ops1, seen) = ops(
            shares,
            kb.clone(),
            OpsStore::new(home.path().join("ops")),
            Some("Maybe disk.\nConfidence: 50%\nFix: clean"),
        );
        let failure = ops1.probe(&monitor()).await;
        assert!(!ops1.handle_incident(&monitor(), &failure).await);
        assert_eq!(
            *seen.lock().unwrap(),
            [
                IncidentKind::Opened,
                IncidentKind::Assessed,
                IncidentKind::Escalated
            ]
        );
        assert_eq!(kb.tsgs()[0].status, "draft");
        assert!(!ops1.probe(&monitor()).await.passed, "nothing was changed");

        // Confident but wrong: the fix is tried, the service still fails, escalated.
        let (_dir2, shares2) = service(false);
        let home2 = tempfile::tempdir().unwrap();
        let kb2 = KnowledgeBase::new(home2.path().join("kb"));
        let (ops2, seen2) = ops(
            shares2,
            kb2.clone(),
            OpsStore::new(home2.path().join("ops")),
            Some("Confidence: 95%\nFix: clean"),
        );
        let failure = ops2.probe(&monitor()).await;
        assert!(!ops2.handle_incident(&monitor(), &failure).await);
        assert_eq!(
            *seen2.lock().unwrap(),
            [
                IncidentKind::Opened,
                IncidentKind::Assessed,
                IncidentKind::Tsg,
                IncidentKind::Escalated
            ]
        );
        assert_eq!(kb2.tsgs()[0].status, "draft");

        // No model and no guide: straight to a person.
        let (_dir3, shares3) = service(true);
        let home3 = tempfile::tempdir().unwrap();
        let (ops3, seen3) = ops(
            shares3,
            KnowledgeBase::new(home3.path().join("kb")),
            OpsStore::new(home3.path().join("ops")),
            None,
        );
        let failure = ops3.probe(&monitor()).await;
        assert!(!ops3.handle_incident(&monitor(), &failure).await);
        assert_eq!(
            *seen3.lock().unwrap(),
            [IncidentKind::Opened, IncidentKind::Escalated]
        );
    }

    #[test]
    fn monitors_need_an_existing_folder_and_check() {
        let (_dir, shares) = service(true);
        let mut store = MonitorStore::default();
        store.set(monitor(), &shares).unwrap();
        let bad = Monitor {
            check: "nope".into(),
            ..monitor()
        };
        assert!(
            store
                .set(bad, &shares)
                .unwrap_err()
                .to_string()
                .contains("no check 'nope'")
        );
        let fast = Monitor {
            every_secs: 1,
            ..monitor()
        };
        assert!(store.set(fast, &shares).is_err());
        assert!(store.remove("api") && store.monitors.is_empty());
    }

    #[tokio::test]
    async fn monitors_added_or_removed_while_the_agent_runs_are_followed() {
        let (_dir, shares) = service(true);
        let home = tempfile::tempdir().unwrap();
        let store = OpsStore::new(home.path().join("ops"));
        let (mut ops, _seen) = ops(
            shares,
            KnowledgeBase::new(home.path().join("kb")),
            store.clone(),
            None,
        );
        let up = |name: &str| Monitor {
            name: name.into(),
            check: "unbreak".into(),
            every_secs: 1,
            ..monitor()
        };
        let list = Arc::new(Mutex::new(vec![up("api")]));
        let now = list.clone();
        ops.monitors = Arc::new(move || now.lock().unwrap().clone());
        let running = tokio::spawn(Arc::new(ops).run());
        let samples = |name: &str| {
            store
                .samples(0)
                .iter()
                .filter(|s| s.monitor == name)
                .count()
        };
        let until = |name: &'static str, n: usize| {
            let samples = &samples;
            async move {
                tokio::time::timeout(Duration::from_secs(20), async {
                    while samples(name) < n {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                })
                .await
                .unwrap_or_else(|_| panic!("{name} wasn't checked {n} times"))
            }
        };
        until("api", 1).await;
        // Removed while the agent runs: it stops. Added: it starts.
        *list.lock().unwrap() = vec![up("web")];
        until("web", 1).await;
        let api = samples("api");
        until("web", 3).await;
        assert_eq!(
            samples("api"),
            api,
            "a removed monitor isn't checked any more"
        );
        running.abort();
    }

    #[tokio::test]
    async fn incidents_stay_open_across_restarts_and_monitors_without_their_check_wait() {
        let (dir, shares) = service(false);
        let home = tempfile::tempdir().unwrap();
        let current = Arc::new(Mutex::new(shares.clone()));
        let (mut ops, seen) = ops(
            shares.clone(),
            KnowledgeBase::new(home.path().join("kb")),
            OpsStore::new(home.path().join("ops")),
            None,
        );
        let now_shares = current.clone();
        ops.shares = Arc::new(move || now_shares.lock().unwrap().clone());
        let m = monitor();
        // Before this fix, every restart opened the same incident again: two are open.
        for incident in [1, 2] {
            ops.store
                .record_incident(&IncidentEvent {
                    ts: incident,
                    incident,
                    monitor: "api".into(),
                    kind: IncidentKind::Opened,
                    text: "api failed".into(),
                    checks: vec![],
                })
                .unwrap();
        }

        // A restart resumes the open incidents: failing again opens no new one.
        let mut state = ops.resume(&m);
        assert_eq!(state.open, [1, 2]);
        ops.tick(&m, &mut state).await;
        ops.tick(&m, &mut state).await;
        assert!(
            seen.lock().unwrap().is_empty(),
            "no new incident, no model call, no alert"
        );
        assert_eq!(ops.store.samples(0).len(), 2, "failures are still measured");

        // Its check is removed while the agent runs: it records nothing and opens nothing.
        let mut without = shares.clone();
        without.shares[0].checks.remove("health");
        *current.lock().unwrap() = without;
        ops.tick(&m, &mut state).await;
        assert!(state.cant_run);
        assert_eq!(
            ops.store.samples(0).len(),
            2,
            "a monitor that can't run records no samples"
        );

        // The check is back and the service recovers: every open incident resolves.
        *current.lock().unwrap() = shares;
        std::fs::write(dir.path().join("ok.txt"), "ok").unwrap();
        ops.tick(&m, &mut state).await;
        assert!(!state.cant_run && state.open.is_empty());
        assert_eq!(
            *seen.lock().unwrap(),
            [IncidentKind::Resolved, IncidentKind::Resolved]
        );
        assert!(open_incidents(&ops.store.incidents(0)).is_empty());
    }
}
