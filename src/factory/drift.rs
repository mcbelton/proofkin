//! Drift watch (docs/agent-factory.md, section 11): an agent's active release evaluated on
//! its deployment on a schedule, every case once (the built-in attacks too), judged by the
//! release's gate and within the deployment's budgets. Each run's pass rate is an ops sample
//! of monitor `factory:<agent>` (see [`crate::ops`]); `fail_after` failed runs in a row open
//! an ops incident and alert the owner with the failing cases, and the next run that passes
//! resolves it.
//!
//! A due run that can't start because another evaluation runs is tried again every two
//! minutes for an hour, then counted as missed. One with nothing to evaluate (no deployment
//! runs the active release, its worker isn't running, it would go over the deployment's
//! tasks a day) is skipped: it counts neither as a pass nor as a failure.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use chrono::{DateTime, Local};

use crate::factory::deploy::{self, Host};
use crate::factory::release;
use crate::factory::report::{EvalState, Evaluation, Tally};
use crate::factory::store::{
    DeployState, DriftOutcome, DriftRun, DriftWatch, FactoryStore, Lifecycle, now,
};
use crate::factory::{conflict, invalid, not_found, runner};
use crate::ops::{IncidentEvent, IncidentKind, OpsStore, Sample};
use crate::profile::Profile;
use crate::runs::unix_now;
use crate::schedule::When;
use crate::workers::Process;

/// The most failed runs in a row a watch may wait for before it opens an incident.
pub const MAX_FAIL_AFTER: u32 = 10;
/// Failed runs in a row that open an incident unless the owner says otherwise.
pub const DEFAULT_FAIL_AFTER: u32 = 2;
/// How often the driver looks for watches that are due.
pub const TICK: Duration = Duration::from_secs(30);
/// How long after a due run couldn't start it is tried again, and for how long.
pub const RETRY_SECS: i64 = crate::schedule::RETRY_SECS;
pub const GRACE_SECS: i64 = crate::schedule::GRACE_SECS;

/// What incidents tell the owner about, per agent.
pub type Notify = Arc<dyn Fn(&IncidentEvent) + Send + Sync>;

/// The ops monitor agent `name`'s drift watch records its samples and incidents as. Monitor
/// names can't have a `:`, so it is never one of the owner's own monitors.
pub fn monitor(name: &str) -> String {
    format!("factory:{name}")
}

/// What setting a watch asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchAsk {
    pub when: String,
    pub fail_after: u32,
    pub enabled: bool,
}

/// `when`, checked as a watch's schedule.
fn schedule(when: &str) -> Result<When> {
    When::parse(when).map_err(|_| {
        invalid(format!(
            "'{when}' isn't a schedule: try daily 03:00, every 6h or mon,fri 17:00"
        ))
    })
}

/// Sets agent `name`'s drift watch as `ask` says (made, changed, paused or resumed), by
/// `by`, recorded in its history. Pausing it closes the incident it opened.
pub fn set(owner: &Profile, name: &str, ask: &WatchAsk, by: &str) -> Result<DriftWatch> {
    let store = FactoryStore::of(owner);
    let when = ask.when.trim().to_owned();
    schedule(&when)?;
    if !(1..=MAX_FAIL_AFTER).contains(&ask.fail_after) {
        return Err(invalid(format!(
            "fail-after is 1 to {MAX_FAIL_AFTER}: the failed runs in a row that open an incident"
        )));
    }
    if store.state(name)?.is_none() {
        return Err(not_found(format!("the factory has no agent '{name}'")));
    }
    let mut closed = None;
    let state = store.update_state(name, |s| {
        if s.state == Lifecycle::Retired {
            return Err(invalid(format!(
                "{name} is retired: a retired agent isn't watched"
            )));
        }
        let at = now();
        let what = match &mut s.watch {
            None => {
                s.watch = Some(DriftWatch {
                    when: when.clone(),
                    fail_after: ask.fail_after,
                    enabled: ask.enabled,
                    since: at,
                    last: None,
                    failures: 0,
                    incident: None,
                    retry: None,
                });
                format!(
                    "drift watch: {when}, an incident after {} failed run{} in a row{}",
                    ask.fail_after,
                    if ask.fail_after == 1 { "" } else { "s" },
                    if ask.enabled { "" } else { " (paused)" }
                )
            }
            Some(w) => {
                let resumed = ask.enabled && !w.enabled;
                if w.when != when || resumed {
                    w.since = at;
                    w.retry = None;
                }
                if !ask.enabled && w.enabled {
                    closed = w.incident.take();
                    w.failures = 0;
                    w.retry = None;
                }
                w.when = when.clone();
                w.fail_after = ask.fail_after;
                let was = w.enabled;
                w.enabled = ask.enabled;
                match (was, ask.enabled) {
                    (true, false) => "drift watch paused".to_owned(),
                    (false, true) => format!("drift watch resumed: {when}"),
                    _ => format!(
                        "drift watch: {when}, an incident after {} failed run{} in a row",
                        ask.fail_after,
                        if ask.fail_after == 1 { "" } else { "s" }
                    ),
                }
            }
        };
        s.changed(by, what, None);
        Ok(())
    })?;
    if let Some(incident) = closed {
        resolve(owner, name, incident, "Closed: the drift watch was paused")?;
    }
    state
        .watch
        .ok_or_else(|| anyhow!("{name}'s drift watch went away"))
}

/// Closes agent `name`'s drift incident, if one is open, saying `why` (it was retired);
/// pauses its watch. Returns whether it had a watch running.
pub fn stop(owner: &Profile, name: &str, why: &str, by: &str) -> Result<bool> {
    let store = FactoryStore::of(owner);
    let mut closed = None;
    let mut stopped = false;
    store.update_state(name, |s| {
        if let Some(w) = s.watch.as_mut().filter(|w| w.enabled) {
            w.enabled = false;
            w.failures = 0;
            w.retry = None;
            closed = w.incident.take();
            stopped = true;
            s.changed(by, "drift watch paused", Some(why));
        }
        Ok(())
    })?;
    if let Some(incident) = closed {
        resolve(owner, name, incident, &format!("Closed: {why}"))?;
    }
    Ok(stopped)
}

fn resolve(owner: &Profile, name: &str, incident: u64, text: &str) -> Result<()> {
    owner.ops_store().record_incident(&IncidentEvent {
        ts: unix_now(),
        incident,
        monitor: monitor(name),
        kind: IncidentKind::Resolved,
        text: text.to_owned(),
        checks: vec![],
    })
}

fn local(at: &str) -> Option<DateTime<Local>> {
    DateTime::parse_from_rfc3339(at)
        .ok()
        .map(|t| t.with_timezone(&Local))
}

/// When `watch` runs next (`None` while it is paused).
pub fn next_run(watch: &DriftWatch) -> Option<DateTime<Local>> {
    if !watch.enabled {
        return None;
    }
    let since = local(&watch.since)?;
    let since = match watch.last.as_ref().and_then(|l| local(&l.at)) {
        Some(last) if last > since => last,
        _ => since,
    };
    Some(When::parse(&watch.when).ok()?.next_after(&since))
}

/// Whether `watch` is due at `now`: its time has come, and a run that couldn't start isn't
/// waiting to be tried again.
pub fn due(watch: &DriftWatch, now: &DateTime<Local>) -> bool {
    next_run(watch).is_some_and(|next| next <= *now)
        && watch
            .retry
            .as_deref()
            .and_then(local)
            .is_none_or(|retry| retry <= *now)
}

/// A run that didn't evaluate anything, and why.
fn skipped(version: &str, worker: &str, why: String) -> DriftRun {
    DriftRun {
        at: now(),
        version: version.to_owned(),
        worker: worker.to_owned(),
        outcome: DriftOutcome::Skipped,
        evaluation: None,
        own: Tally::default(),
        adversarial: Tally::default(),
        failed: Vec::new(),
        note: Some(why),
    }
}

/// What an ended drift evaluation found.
pub fn verdict(e: &Evaluation, version: &str, worker: &str, started: &str) -> DriftRun {
    let mut own = Tally::default();
    let mut adversarial = Tally::default();
    for run in &e.runs {
        let tally = match e.case(&run.case) {
            Some(case) if case.is_adversarial() => &mut adversarial,
            _ => &mut own,
        };
        tally.total += 1;
        if run.passed {
            tally.passed += 1;
        }
    }
    let canary = deploy::verdict(e);
    let passed = e.state == EvalState::Passed && e.gate.passed;
    // Its one model's name needn't lead each of its gate's reasons.
    let model = e.candidates.first().map(|c| format!("{c}: "));
    let reasons: Vec<&str> = e
        .gate
        .reasons
        .iter()
        .map(|r| {
            model
                .as_deref()
                .and_then(|m| r.strip_prefix(m))
                .unwrap_or(r)
        })
        .collect();
    DriftRun {
        at: started.to_owned(),
        version: version.to_owned(),
        worker: worker.to_owned(),
        outcome: if passed {
            DriftOutcome::Passed
        } else {
            DriftOutcome::Failed
        },
        evaluation: Some(e.id.clone()),
        own,
        adversarial,
        failed: canary.failed,
        note: (!passed)
            .then(|| reasons.join("; "))
            .filter(|r| !r.is_empty()),
    }
}

/// A run in a line: `passed: 100% of its own runs (3 of 3), the built-in attacks 7 of 7`.
pub fn run_text(run: &DriftRun) -> String {
    match run.outcome {
        DriftOutcome::Skipped => format!(
            "skipped: {}",
            run.note.as_deref().unwrap_or("nothing to evaluate")
        ),
        outcome => format!("{outcome}: {}", rates_text(run)),
    }
}

/// How a run's evaluation went: `100% of its own runs (3 of 3), the built-in attacks 7 of 7`,
/// and what its gate missed.
fn rates_text(run: &DriftRun) -> String {
    let rate = if run.own.total == 0 {
        "no runs of its own cases".to_owned()
    } else {
        format!(
            "{:.0}% of its own runs ({} of {})",
            run.own.passed as f64 * 100.0 / run.own.total as f64,
            run.own.passed,
            run.own.total
        )
    };
    let mut text = format!(
        "{rate}, the built-in attacks {} of {}",
        run.adversarial.passed, run.adversarial.total
    );
    if let Some(note) = &run.note {
        text += &format!(" ({note})");
    }
    text
}

/// Evaluates agent `name`'s active release on the deployment that runs it, as its watch
/// says, records the run (its ops sample, its incident) and returns it. With `start`, its
/// worker is started first if it doesn't run (the command line's run, which stops it after);
/// otherwise a worker that doesn't run makes the run skipped. `on_change` gets the evaluation
/// as its runs end; `notify` hears of the incidents it opens and resolves.
pub async fn run(
    host: &Host,
    name: &str,
    start: bool,
    on_change: impl Fn(&Evaluation),
    notify: &(dyn Fn(&IncidentEvent) + Send + Sync),
) -> Result<DriftRun> {
    let store = FactoryStore::of(&host.owner);
    let state = store
        .state(name)?
        .ok_or_else(|| not_found(format!("the factory has no agent '{name}'")))?;
    if state.watch.is_none() {
        return Err(conflict(format!(
            "{name} has no drift watch: proofkin factory watch {name} --when \"daily 03:00\""
        )));
    }
    let ran = match evaluate(host, name, &state, start, on_change).await {
        Ok(ran) => ran,
        Err(e) => skipped(
            state.active_version.as_deref().unwrap_or_default(),
            "",
            format!("{e:#}"),
        ),
    };
    record(
        &FactoryStore::of(&host.owner),
        &host.owner.ops_store(),
        &host.owner.name,
        name,
        ran,
        notify,
    )
}

/// The run itself: what it evaluates, or why it is skipped.
async fn evaluate(
    host: &Host,
    name: &str,
    state: &crate::factory::store::AgentState,
    start: bool,
    on_change: impl Fn(&Evaluation),
) -> Result<DriftRun> {
    let store = FactoryStore::of(&host.owner);
    let Some(version) = state.active_version.clone() else {
        return Ok(skipped("", "", format!("{name} has no active release")));
    };
    if state.state == Lifecycle::Retired {
        return Ok(skipped(&version, "", format!("{name} is retired")));
    }
    let Some(deployment) = state
        .deployments
        .iter()
        .find(|d| d.version == version && d.state == DeployState::Running)
    else {
        return Ok(skipped(
            &version,
            "",
            format!("no deployment runs its active release {version}"),
        ));
    };
    let worker = deployment.worker.clone();
    let profile = crate::workers::worker_of(&host.owner, &worker)?;
    let running = matches!(host.supervisor.process(&profile), Process::Running { .. });
    if !running && !start {
        return Ok(skipped(
            &version,
            &worker,
            format!("its worker {worker} isn't running"),
        ));
    }
    let release = release::load(&store, name, &version)?;
    let bp = release::blueprint_of(&store, name, &version)?;
    let build = release::build_of(&store, name, &version)?;
    // Every case once, the built-in attacks too: what its tasks a day must still fit.
    let runs = runner::cases(&bp, &Default::default(), &store.evaluation_dir()).len() as u32;
    let budgets = deploy::pin_of(&profile)
        .map(|p| p.budgets)
        .unwrap_or_default();
    if let Some(limit) = budgets.tasks_per_day {
        let taken = deploy::tasks_today(&profile.audit_path(), &format!("{name}@{version}"));
        if taken + runs > limit {
            return Ok(skipped(
                &version,
                &worker,
                format!(
                    "its {runs} runs would go over its budget of {limit} tasks a day ({taken} taken today)"
                ),
            ));
        }
    }
    let _running = store.lock_evaluation()?;
    store.save_build(&build)?;
    if !running {
        host.supervisor.start(&worker)?;
    }
    let started = now();
    let folders = host.owner.shares()?;
    let setup = runner::prepare_canary(
        &store,
        runner::CanaryRequest {
            blueprint: &bp,
            build: &build,
            version: version.clone(),
            route: release.manifest.model.chosen.clone(),
            by: host.owner.name.clone(),
            worker: worker.clone(),
            worker_id: profile.identity()?.public(),
            folders: &folders,
            drift: true,
        },
    )?;
    runner::install(&profile, &setup.share)?;
    let runner = runner::Runner {
        agent: host.agent.clone(),
        key: host.key.clone(),
        signer: host.owner.name.clone(),
        worker: profile.identity()?.public(),
        store: store.clone(),
        judges: runner::Judges::default(),
        ready_wait: host.ready_wait,
    };
    let id = setup.evaluation.id.clone();
    let ran = runner.run(setup.evaluation, &setup.share, on_change).await;
    let _ = runner::clean(&store, Some(&profile));
    if !running {
        host.supervisor.stop(&worker).await;
    }
    let evaluation = match ran {
        Ok(e) => e,
        Err(e) => {
            let _ = runner::cancel(
                &store,
                &id,
                &format!("it stopped: {e:#}"),
                &host.owner.name,
                &host.key,
            );
            return Err(e);
        }
    };
    if evaluation.state == EvalState::Cancelled {
        return Ok(skipped(
            &version,
            &worker,
            format!(
                "evaluation {} was cancelled: {}",
                evaluation.id,
                evaluation
                    .error
                    .as_deref()
                    .unwrap_or("before its runs ended")
            ),
        ));
    }
    Ok(verdict(&evaluation, &version, &worker, &started))
}

/// Records run `ran` of agent `name`'s watch, in its owner's (`by`) factory `store` and ops
/// log `ops`: its ops sample, its place in the failures in a row, and the incident it opens
/// or resolves (told to `notify`). Returns it.
pub fn record(
    store: &FactoryStore,
    ops: &OpsStore,
    by: &str,
    name: &str,
    ran: DriftRun,
    notify: &(dyn Fn(&IncidentEvent) + Send + Sync),
) -> Result<DriftRun> {
    let ts = unix_now();
    if ran.outcome != DriftOutcome::Skipped {
        let secs = ran
            .evaluation
            .as_deref()
            .and_then(|id| store.evaluation(id).ok().flatten())
            .and_then(|e| {
                let started = DateTime::parse_from_rfc3339(&e.started).ok()?;
                let ended = DateTime::parse_from_rfc3339(e.finished.as_deref()?).ok()?;
                Some((ended - started).num_milliseconds().max(0) as u64)
            })
            .unwrap_or(0);
        ops.record_sample(&Sample {
            ts,
            monitor: monitor(name),
            ok: ran.outcome == DriftOutcome::Passed,
            ms: secs,
            exit: None,
            pass_rate: (ran.own.total > 0).then(|| ran.own.passed as f64 / ran.own.total as f64),
        })?;
    }
    let mut event = None;
    store.update_state(name, |s| {
        let Some(w) = s.watch.as_mut() else {
            return Ok(());
        };
        w.last = Some(ran.clone());
        w.retry = None;
        match ran.outcome {
            DriftOutcome::Skipped => {}
            DriftOutcome::Passed => {
                w.failures = 0;
                if let Some(incident) = w.incident.take() {
                    let text = format!(
                        "{name} {} passed its drift watch again: {}",
                        ran.version,
                        rates_text(&ran)
                    );
                    event = Some((incident, IncidentKind::Resolved, text.clone()));
                    s.changed(by, text, None);
                }
            }
            DriftOutcome::Failed => {
                w.failures += 1;
                if w.failures >= w.fail_after.max(1) && w.incident.is_none() {
                    w.incident = Some(ts);
                    let text = format!(
                        "{name} {} failed its drift watch {} time{} in a row on {}: {}; failing: {}",
                        ran.version,
                        w.failures,
                        if w.failures == 1 { "" } else { "s" },
                        ran.worker,
                        rates_text(&ran),
                        if ran.failed.is_empty() {
                            "no case (its gate missed)".to_owned()
                        } else {
                            ran.failed.join("; ")
                        }
                    );
                    event = Some((ts, IncidentKind::Opened, text.clone()));
                    s.changed(by, text, None);
                }
            }
        }
        Ok(())
    })?;
    if let Some((incident, kind, text)) = event {
        let event = IncidentEvent {
            ts,
            incident,
            monitor: monitor(name),
            kind,
            text: crate::redact::redact(&text).0,
            checks: vec![],
        };
        ops.record_incident(&event)?;
        notify(&event);
    }
    Ok(ran)
}

/// Marks a due run of agent `name`'s watch that couldn't start (`why`): tried again in two
/// minutes, or, an hour after it fell due, counted as missed.
fn not_started(owner: &Profile, name: &str, watch: &DriftWatch, why: &str) {
    let now = Local::now();
    let late = next_run(watch).is_some_and(|due| (now - due).num_seconds() > GRACE_SECS);
    let result = FactoryStore::of(owner).update_state(name, |s| {
        if let Some(w) = s.watch.as_mut() {
            if late {
                w.retry = None;
                w.last = Some(skipped(
                    s.active_version.as_deref().unwrap_or_default(),
                    "",
                    format!("missed: {why}"),
                ));
            } else {
                w.retry = Some(
                    (chrono::Utc::now() + chrono::Duration::seconds(RETRY_SECS))
                        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                );
            }
        }
        Ok(())
    });
    if let Err(e) = result {
        eprintln!("could not record {name}'s drift watch: {e:#}");
    }
}

/// The drift watches' driver, for a running agent: every [`TICK`], each due watch runs in
/// turn (one evaluation at a time). `changed` hears of each agent whose watch ran.
pub async fn drive(host: Arc<Host>, changed: Arc<dyn Fn(&str) + Send + Sync>, notify: Notify) {
    let store = FactoryStore::of(&host.owner);
    loop {
        tokio::time::sleep(TICK).await;
        let now = Local::now();
        for name in store.names().unwrap_or_default() {
            let Some(watch) = store.state(&name).ok().flatten().and_then(|s| s.watch) else {
                continue;
            };
            if !due(&watch, &now) {
                continue;
            }
            if store.evaluation_running() {
                not_started(&host.owner, &name, &watch, "another evaluation was running");
                continue;
            }
            match run(&host, &name, false, |_| changed(&name), &*notify).await {
                Ok(ran) => println!("agent factory: {name}'s drift watch {}", run_text(&ran)),
                Err(e) => eprintln!("{name}'s drift watch didn't run: {e:#}"),
            }
            changed(&name);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn watch(when: &str, since: &str) -> DriftWatch {
        DriftWatch {
            when: when.into(),
            fail_after: 2,
            enabled: true,
            since: since.into(),
            last: None,
            failures: 0,
            incident: None,
            retry: None,
        }
    }

    #[test]
    fn a_watch_is_due_on_its_schedule_from_its_last_run_and_waits_while_paused() {
        let set = Local::now() - chrono::Duration::hours(7);
        let mut w = watch(
            "every 6h",
            &set.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        );
        let now = Local::now();
        assert!(due(&w, &now), "6 hours after it was set");
        w.last = Some(skipped("1.0.0", "", "nothing ran".into()));
        assert!(!due(&w, &now), "it ran just now: next in 6 hours");
        w.last = None;
        w.retry = Some(
            (chrono::Utc::now() + chrono::Duration::minutes(2))
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        );
        assert!(!due(&w, &now), "it waits to be tried again");
        w.retry = None;
        w.enabled = false;
        assert!(!due(&w, &now) && next_run(&w).is_none(), "paused");
    }

    /// A failed run says what its gate missed once, without its one model before each reason.
    #[test]
    fn a_failed_run_says_what_its_gate_missed_without_its_model_before_each_reason() {
        let mut e = crate::factory::report::tests::evaluation();
        e.candidates.truncate(1);
        let model = e.candidates[0].to_string();
        e.state = EvalState::Failed;
        e.gate.passed = false;
        e.gate.reasons = vec![
            format!("{model}: 0% of its runs passed; the gate needs 90%"),
            format!("{model}: R1 passed 0 of 1 runs; the gate needs 67%"),
        ];
        let run = verdict(&e, "1.0.0", "notes", "2026-11-26T03:00:00Z");
        assert_eq!(run.outcome, DriftOutcome::Failed);
        assert_eq!(
            run.note.as_deref(),
            Some(
                "0% of its runs passed; the gate needs 90%; R1 passed 0 of 1 runs; the gate needs 67%"
            )
        );
        assert!(!run_text(&run).contains(&model), "{}", run_text(&run));
    }
}
