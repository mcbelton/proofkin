//! The Work IQ audit log and call budgets (docs/work-iq.md, section 9).
//!
//! Every call through [`Client::call_tool_in`](super::client::Client::call_tool_in), done,
//! failed or refused, is one JSON line in `<profile>/workiq-audit.jsonl`. Like the task audit
//! log (`crate::audit`) it is append only, written behind a lock, and a write error only
//! warns. The query is not stored, only its SHA-256 and length.
//!
//! The log is also where usage comes from: a call counts for the day it was made (the local
//! date) and for its run. A call over a budget is refused before it is sent.

use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use anyhow::{Result, bail};
use chrono::{DateTime, Local, NaiveDate, TimeZone};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::client::{Class, RequestIds, ToolResult, WorkIqError, WorkIqErrorKind};
use super::content::{Audience, summarize};
use crate::profile::{Profile, WorkIqBudget};

/// The audit file's name in a profile's folder.
pub const FILE: &str = "workiq-audit.jsonl";

const LOCAL: &str = "local";
const REFUSED: &str = "refused";

/// Appends are serialized across clients of the same process.
static FILE_LOCK: Mutex<()> = Mutex::new(());

/// The time, injectable so tests can pass a day.
pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Local>;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Local> {
        Local::now()
    }
}

/// What a call is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Purpose {
    /// A step of a run; the run's budget counts it.
    Step {
        run: String,
        step: String,
    },
    KnowledgeImport,
    /// `proofkin spec import --from`: a spec read from a SharePoint or OneDrive document.
    SpecImport,
    /// A task of a peer or a worker, through this agent's Work IQ gateway.
    Gateway,
    Doctor,
    /// A command the owner ran.
    Command,
    /// A sign-off notice in Microsoft 365 (docs/work-iq.md, section 7): for a plan's sign-off,
    /// its run (whose budget counts it) and step.
    Notice {
        run: Option<String>,
        step: Option<String>,
    },
}

impl Purpose {
    fn name(&self) -> &'static str {
        match self {
            Purpose::Step { .. } => "step",
            Purpose::KnowledgeImport => "knowledge-import",
            Purpose::SpecImport => "spec-import",
            Purpose::Gateway => "gateway",
            Purpose::Doctor => "doctor",
            Purpose::Command => "command",
            Purpose::Notice { .. } => "notice",
        }
    }

    fn run(&self) -> Option<&str> {
        match self {
            Purpose::Step { run, .. } => Some(run),
            Purpose::Notice { run, .. } => run.as_deref(),
            _ => None,
        }
    }

    fn step(&self) -> Option<&str> {
        match self {
            Purpose::Step { step, .. } => Some(step),
            Purpose::Notice { step, .. } => step.as_deref(),
            _ => None,
        }
    }
}

/// Why a call is made and for whom it is made.
#[derive(Debug, Clone)]
pub struct CallContext {
    pub purpose: Purpose,
    pub audience: Audience,
    /// The peer's name; `None` for the owner and its own workers.
    pub peer: Option<String>,
}

impl CallContext {
    pub fn local(purpose: Purpose) -> Self {
        CallContext {
            purpose,
            audience: Audience::Local,
            peer: None,
        }
    }

    /// A call for a command the owner runs.
    pub fn command() -> Self {
        Self::local(Purpose::Command)
    }

    /// A call for `peer`, who may receive the content with these labels.
    pub fn peer(name: &str, labels: Vec<String>, purpose: Purpose) -> Self {
        CallContext {
            purpose,
            audience: Audience::peer(labels),
            peer: Some(name.to_owned()),
        }
    }
}

/// One line of the audit file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// Unix time in milliseconds when the call began.
    pub ts_ms: i64,
    pub tool: String,
    /// `step`, `knowledge-import`, `spec-import`, `gateway`, `doctor`, `command` or `notice`.
    pub purpose: String,
    /// The run a step (or a plan's sign-off notice) belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<String>,
    /// `local`, or the peer's name.
    pub audience: String,
    pub query_sha256: String,
    pub query_chars: usize,
    /// `ok`, `refused`, `cancelled`, or the error's class (`throttled`, `denied-by-policy`, ...).
    pub outcome: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub results: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub withheld: Option<usize>,
    /// The sensitivity labels of the results (`unlabeled` for none, `unknown` for a result
    /// without label information).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ms_request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    pub duration_ms: u64,
}

impl Entry {
    fn new(at: DateTime<Local>, tool: &str, context: &CallContext, query: &str) -> Self {
        Entry {
            ts_ms: at.timestamp_millis(),
            tool: tool.to_owned(),
            purpose: context.purpose.name().to_owned(),
            run: context.purpose.run().map(str::to_owned),
            step: context.purpose.step().map(str::to_owned),
            audience: context.peer.clone().unwrap_or_else(|| LOCAL.to_owned()),
            query_sha256: data_encoding::HEXLOWER.encode(&Sha256::digest(query.as_bytes())),
            query_chars: query.chars().count(),
            outcome: String::new(),
            results: None,
            withheld: None,
            labels: Vec::new(),
            request_id: None,
            ms_request_id: None,
            client_request_id: None,
            correlation_id: None,
            duration_ms: 0,
        }
    }

    fn set_ids(&mut self, ids: &RequestIds) {
        self.request_id = ids.request_id.clone();
        self.ms_request_id = ids.ms_request_id.clone();
        self.client_request_id = ids.client_request_id.clone();
        self.correlation_id = ids.correlation_id.clone();
    }
}

/// The text the call asks, whose hash is logged.
fn query_of(arguments: &Value) -> String {
    // A change keeps its path's SHA-256, never the path.
    for key in ["query", "question", "parentUrl", "entityUrl", "actionUrl"] {
        if let Some(Value::String(text)) = arguments.get(key) {
            return text.clone();
        }
    }
    arguments.to_string()
}

fn class_name(class: &Class) -> &'static str {
    match class {
        Class::SignInAgain => "sign-in-again",
        Class::Consent => "consent",
        Class::DeniedByPolicy => "denied-by-policy",
        Class::Billing => "billing",
        Class::Throttled(_) => "throttled",
        Class::Transient => "transient",
        Class::UncertainChange => "uncertain-change",
        Class::Tool => "tool",
        Class::OverBudget => "over-budget",
    }
}

/// Reads the entries of an audit file, oldest first. Unreadable lines are skipped.
pub fn read(path: &Path) -> std::io::Result<Vec<Entry>> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    Ok(std::io::BufReader::new(file)
        .lines()
        .map_while(std::result::Result::ok)
        .filter_map(|line| serde_json::from_str(&line).ok())
        .collect())
}

/// Calls made on a day and in a run, from the log. Refused calls were not sent and don't count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Usage {
    pub day: NaiveDate,
    pub today: u32,
    /// The calls of the run asked about.
    pub run: Option<u32>,
}

/// The day's use, with the runs that made calls that day.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub day: NaiveDate,
    pub today: u32,
    pub runs: BTreeMap<String, u32>,
    pub budget: WorkIqBudget,
}

impl Report {
    pub fn describe(&self, profile: &str) -> String {
        let mut text = format!(
            "profile '{profile}': Work IQ use on {} (local date)\n  calls today    {} of {}\n  \
             calls per run  at most {}\n",
            self.day,
            self.today,
            self.budget.per_day(),
            self.budget.per_run(),
        );
        for (run, count) in &self.runs {
            text.push_str(&format!(
                "  run {run}  {count} of {}\n",
                self.budget.per_run()
            ));
        }
        text
    }
}

#[derive(Default)]
struct InFlight {
    total: u32,
    per_run: HashMap<String, u32>,
}

/// A profile's Work IQ audit log and budgets.
pub struct Audit {
    path: PathBuf,
    profile: String,
    budget: WorkIqBudget,
    clock: Arc<dyn Clock>,
    in_flight: Mutex<InFlight>,
}

impl Audit {
    pub fn new(path: PathBuf, profile: &str, budget: WorkIqBudget) -> Self {
        Audit {
            path,
            profile: profile.to_owned(),
            budget,
            clock: Arc::new(SystemClock),
            in_flight: Mutex::new(InFlight::default()),
        }
    }

    /// The log and budgets of `profile`.
    pub fn for_profile(profile: &Profile) -> Self {
        Self::new(
            profile.dir.join(FILE),
            &profile.name,
            profile.config.workiq_budget.clone().unwrap_or_default(),
        )
    }

    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn budget(&self) -> &WorkIqBudget {
        &self.budget
    }

    /// The calls made today, and by `run`.
    pub fn usage(&self, run: Option<&str>) -> Usage {
        self.usage_on(self.clock.now().date_naive(), run)
    }

    fn usage_on(&self, day: NaiveDate, run: Option<&str>) -> Usage {
        let mut usage = Usage {
            day,
            today: 0,
            run: run.map(|_| 0),
        };
        for entry in self.entries().iter().filter(|e| e.outcome != REFUSED) {
            if entry_day(entry) == Some(day) {
                usage.today += 1;
            }
            if entry.run.is_some() && entry.run.as_deref() == run {
                usage.run = usage.run.map(|count| count + 1);
            }
        }
        usage
    }

    /// Today's use against the budgets, for `proofkin workiq usage`.
    pub fn report(&self) -> Result<Report> {
        let day = self.clock.now().date_naive();
        let mut report = Report {
            day,
            today: 0,
            runs: BTreeMap::new(),
            budget: self.budget.clone(),
        };
        for entry in read(&self.path)?.iter().filter(|e| e.outcome != REFUSED) {
            if entry_day(entry) == Some(day) {
                report.today += 1;
                if let Some(run) = &entry.run {
                    *report.runs.entry(run.clone()).or_default() += 1;
                }
            }
        }
        Ok(report)
    }

    fn entries(&self) -> Vec<Entry> {
        read(&self.path).unwrap_or_else(|e| {
            eprintln!(
                "warning: could not read Work IQ audit log {}: {e}",
                self.path.display()
            );
            Vec::new()
        })
    }

    /// Starts a call: refuses it, recording that, when a budget is used up; otherwise holds
    /// its place until the returned [`Pending`] is finished or dropped.
    // `WorkIqError` is the client's one error type, returned as is by every call.
    #[allow(clippy::result_large_err)]
    pub fn begin(
        &self,
        context: &CallContext,
        tool: &str,
        arguments: &Value,
    ) -> std::result::Result<Pending<'_>, WorkIqError> {
        let now = self.clock.now();
        let mut entry = Entry::new(now, tool, context, &query_of(arguments));
        let run = context.purpose.run();
        let refusal = {
            let mut held = self
                .in_flight
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let usage = self.usage_on(now.date_naive(), run);
            let run_held = run.map_or(0, |r| held.per_run.get(r).copied().unwrap_or(0));
            let refusal = if usage.today + held.total >= self.budget.per_day() {
                Some(format!(
                    "the daily Work IQ budget of {} calls is used up for {}, so the call was not \
                     sent; raise it with `proofkin workiq budget --per-day <calls> --profile {}`",
                    self.budget.per_day(),
                    usage.day,
                    self.profile
                ))
            } else if run.is_some() && usage.run.unwrap_or(0) + run_held >= self.budget.per_run() {
                Some(format!(
                    "the Work IQ budget of {} calls per run is used up for run {}, so the call \
                     was not sent; raise it with `proofkin workiq budget --per-run <calls> \
                     --profile {}`",
                    self.budget.per_run(),
                    run.unwrap_or_default(),
                    self.profile
                ))
            } else {
                None
            };
            if refusal.is_none() {
                held.total += 1;
                if let Some(run) = run {
                    *held.per_run.entry(run.to_owned()).or_default() += 1;
                }
            }
            refusal
        };
        if let Some(message) = refusal {
            entry.outcome = REFUSED.to_owned();
            self.append(&entry);
            return Err(WorkIqError::new(
                WorkIqErrorKind::Budget,
                Class::OverBudget,
                message,
                RequestIds::default(),
            ));
        }
        Ok(Pending {
            audit: self,
            entry,
            audience: context.audience.clone(),
            run: run.map(str::to_owned),
            started: Instant::now(),
        })
    }

    fn release(&self, run: Option<&str>) {
        let mut held = self
            .in_flight
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        held.total = held.total.saturating_sub(1);
        if let Some(count) = run.and_then(|run| held.per_run.get_mut(run)) {
            *count = count.saturating_sub(1);
        }
    }

    /// Appends `entry`; failures are reported but never fail the call.
    fn append(&self, entry: &Entry) {
        let _guard = FILE_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        let result = (|| -> std::io::Result<()> {
            if let Some(dir) = self.path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)?;
            let mut line = serde_json::to_vec(entry)?;
            line.push(b'\n');
            file.write_all(&line)
        })();
        if let Err(e) = result {
            eprintln!(
                "warning: could not write Work IQ audit log {}: {e}",
                self.path.display()
            );
        }
    }
}

fn entry_day(entry: &Entry) -> Option<NaiveDate> {
    Local
        .timestamp_millis_opt(entry.ts_ms)
        .single()
        .map(|time| time.date_naive())
}

/// A call that was allowed: written to the log when finished, or, if its future is dropped
/// first, as `cancelled`.
pub struct Pending<'a> {
    audit: &'a Audit,
    entry: Entry,
    audience: Audience,
    run: Option<String>,
    started: Instant,
}

impl Pending<'_> {
    /// Records how the call ended.
    pub fn finish(mut self, result: &Result<ToolResult>) {
        match result {
            Ok(done) => {
                self.entry.outcome = "ok".to_owned();
                self.entry.set_ids(&done.ids);
                if let Some(summary) = summarize(done, &self.audience) {
                    self.entry.results = Some(summary.results);
                    self.entry.withheld = Some(summary.withheld);
                    self.entry.labels = summary.labels;
                }
            }
            Err(error) => match error.downcast_ref::<WorkIqError>() {
                Some(failure) => {
                    self.entry.outcome = class_name(&failure.class).to_owned();
                    self.entry.set_ids(&failure.ids);
                }
                None => self.entry.outcome = "error".to_owned(),
            },
        }
    }
}

impl Drop for Pending<'_> {
    fn drop(&mut self) {
        if self.entry.outcome.is_empty() {
            self.entry.outcome = "cancelled".to_owned();
        }
        self.entry.duration_ms = self.started.elapsed().as_millis() as u64;
        self.audit.release(self.run.as_deref());
        self.audit.append(&self.entry);
    }
}

/// Changes the budgets in `profile.json`; a limit that is `None` keeps its value.
pub fn set_budget(
    profile: &mut Profile,
    per_day: Option<u32>,
    per_run: Option<u32>,
) -> Result<WorkIqBudget> {
    if per_day.is_none() && per_run.is_none() {
        bail!("give --per-day and/or --per-run");
    }
    if per_day == Some(0) || per_run == Some(0) {
        bail!("a budget must allow at least one call");
    }
    let mut budget = profile.config.workiq_budget.clone().unwrap_or_default();
    budget.calls_per_day = per_day.or(budget.calls_per_day);
    budget.calls_per_run = per_run.or(budget.calls_per_run);
    profile.config.workiq_budget = Some(budget.clone());
    profile.save()?;
    Ok(budget)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::Profile;

    #[test]
    fn budgets_are_saved_in_profile_json_and_a_limit_left_out_keeps_its_value() {
        let root = tempfile::tempdir().unwrap();
        let mut profile = Profile::open_in(root.path(), "alice").unwrap();
        assert_eq!(Audit::for_profile(&profile).budget().per_day(), 200);
        assert_eq!(Audit::for_profile(&profile).budget().per_run(), 25);

        set_budget(&mut profile, Some(50), None).unwrap();
        let budget = set_budget(&mut profile, None, Some(5)).unwrap();
        assert_eq!((budget.per_day(), budget.per_run()), (50, 5));

        let saved = Profile::open_in(root.path(), "alice").unwrap();
        let audit = Audit::for_profile(&saved);
        assert_eq!(
            (audit.budget().per_day(), audit.budget().per_run()),
            (50, 5)
        );
        assert_eq!(audit.path(), saved.dir.join(FILE));

        assert!(set_budget(&mut profile, Some(0), None).is_err());
        assert!(set_budget(&mut profile, None, None).is_err());
    }
}
