//! `proofkin factory` for released agents: release, approve, verify and diff releases;
//! deploy, roll back, retire; metrics and budgets. [`crate::factory::cli`] parses the
//! commands and calls these.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use iroh::EndpointAddr;

use crate::agent::{Agent, AgentConfig, Network};
use crate::factory::blueprint::{Budgets, from_value};
use crate::factory::deploy::{self, DeployAsk, Host};
use crate::factory::release::{self, Asked, ReleaseState};
use crate::factory::report::{EvalState, Evaluation};
use crate::factory::store::{DeployState, FactoryStore};
use crate::factory::{catalog, code, conflict, drift, invalid, lifecycle, metrics, not_found};
use crate::profile::Profile;
use crate::trust::TrustStore;
use crate::workers::{Launch, Supervisor};

/// The next command for profile `profile`.
fn hint(profile: &Profile, command: &str) -> String {
    hint_as(&profile.name, command)
}

fn hint_as(profile: &str, command: &str) -> String {
    format!("proofkin factory {command} --profile {profile}")
}

/// Releases agent `name` from evaluation `evaluation` (or, with `from`, from the agent kept
/// as code in that folder, at its commit, with the passed report in its `agent/reports`).
pub fn release(
    profile: &Profile,
    name: &str,
    evaluation: Option<String>,
    version: Option<String>,
    note: Option<String>,
    from: Option<&Path>,
) -> Result<()> {
    let store = FactoryStore::of(profile);
    let key = profile.identity()?;
    let mut asked = Asked {
        version,
        note,
        by: profile.name.clone(),
        ..Asked::default()
    };
    match (evaluation, from) {
        (_, Some(dir)) => {
            let me = key.public().to_string();
            let (id, commit) = from_folder(&profile.name, &me, &store, name, dir)?;
            asked.evaluation = id;
            // The folder by its name, not its path: releases may be published to peers.
            asked.dir = Some(
                std::path::absolute(dir)
                    .ok()
                    .and_then(|d| d.file_name().map(|n| n.to_string_lossy().into_owned()))
                    .unwrap_or_else(|| "its folder".into()),
            );
            asked.commit = Some(commit);
        }
        (Some(id), None) => asked.evaluation = id,
        (None, None) => bail!(
            "give --evaluation ID (a passed evaluation: {}) or --from FOLDER",
            hint(profile, &format!("show {name}"))
        ),
    }
    let released = release::request(&store, &profile.name, &key, name, &asked)?;
    let m = &released.manifest;
    println!(
        "{name} {}: {} ({}: {}); manifest {}, signed by {}",
        m.version,
        released.state,
        m.change,
        m.change_why,
        &released.manifest_sha256[..12],
        profile.name
    );
    if !m.increases.is_empty() {
        println!("  more capabilities: {}", m.increases.join("; "));
    }
    if let Some(commit) = &m.origin.commit {
        println!(
            "  from commit {commit} of {}",
            m.origin.dir.as_deref().unwrap_or("its folder")
        );
    }
    for ask in released.pending() {
        if ask.approver == release::OWNER {
            println!(
                "  waiting for you: {}",
                hint(profile, &format!("approve {name} --version {}", m.version))
            );
        } else {
            println!(
                "  waiting for {}: asked when your agent runs and they are connected (proofkin --name {} --ui)",
                ask.approver, profile.name
            );
        }
    }
    if released.state == ReleaseState::Released {
        println!(
            "  active now: plans use it as agent={name}; {}",
            hint(profile, &format!("deploy {name} --start"))
        );
    }
    Ok(())
}

/// The evaluation and commit to release agent `name` kept as code in `dir` from: the
/// folder's build, its passed report signed by its owner (`me`, profile `owner`; kept in the
/// factory if it isn't), and its commit, which must hold every file of its agent.
fn from_folder(
    owner: &str,
    me: &str,
    store: &FactoryStore,
    name: &str,
    dir: &Path,
) -> Result<(String, String)> {
    let value = code::blueprint_value(dir)?;
    let bp = from_value(&value).map_err(|p| anyhow!("{}", p.message))?;
    if bp.name != name {
        bail!("the folder's agent is {}, not {name}", bp.name);
    }
    if store.state(name)?.is_none() {
        return Err(not_found(format!(
            "the factory has no agent '{name}': {}",
            hint_as(owner, &format!("new {name} --from {}", dir.display()))
        )));
    }
    let build =
        crate::factory::build::build(&bp, &crate::skills::Skills::new(dir.join(code::SKILLS)))?;
    let mut reports: Vec<Evaluation> = std::fs::read_dir(dir.join(code::REPORTS))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .filter_map(|text| serde_json::from_str::<Evaluation>(&text).ok())
        .filter(|e| {
            e.agent == name
                && e.build == build.hash
                && e.state == EvalState::Passed
                && e.canary.is_none()
                && e.signer_id == me
                && e.verify().is_ok()
        })
        .collect();
    reports.sort_by(|a, b| b.id.cmp(&a.id));
    let report = reports.into_iter().next().ok_or_else(|| {
        conflict(format!(
            "{} has no passed evaluation of build {} signed by you: {} and keep its report there",
            code::REPORTS,
            build.short_hash(),
            hint_as(owner, &format!("evaluate {name} --yes"))
        ))
    })?;
    let git = |args: &[&str]| -> Result<String> {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .context("running git (is it installed and on PATH?)")?;
        if !out.status.success() {
            bail!(
                "git {} failed in {}: {}",
                args.join(" "),
                dir.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    };
    let commit = git(&["rev-parse", "HEAD"])?;
    let dirty = git(&["status", "--porcelain", "--", "agent"])?;
    if !dirty.is_empty() {
        return Err(conflict(format!(
            "agent/ in {} has changes that aren't committed: commit them, so the release is exactly a commit",
            dir.display()
        )));
    }
    store.save_build(&build)?;
    if store.evaluation(&report.id)?.is_none() {
        store.save_evaluation(&report)?;
    }
    code::copy_fixtures_from(&dir.join(code::FIXTURES), &store.fixtures_dir(name))?;
    Ok((report.id, commit))
}

/// The owner signs off (or rejects) the release of `name` that waits for them.
pub fn approve(
    profile: &Profile,
    name: &str,
    version: Option<&str>,
    reject: bool,
    note: &str,
) -> Result<()> {
    let store = FactoryStore::of(profile);
    let waiting = release::pending_release(&store, name, version)?;
    let v = waiting.manifest.version.clone();
    let report = release::report_of(&store, name, &v)?;
    let answer = release::own_answer(
        &profile.name,
        &profile.identity()?,
        &waiting,
        &report,
        !reject,
        note,
    )?;
    let after = release::answer(&store, name, &v, answer)?;
    println!("{name} {v}: {}", after.state);
    if let Some(why) = &after.why {
        println!("  {why}");
    }
    for ask in after.pending() {
        println!("  waiting for {}", ask.approver);
    }
    Ok(())
}

/// Verifies a release offline (`name@version`), or every release of `name`.
pub fn verify(profile: &Profile, target: &str) -> Result<()> {
    let store = FactoryStore::of(profile);
    let (name, version) = match target.split_once('@') {
        Some((n, v)) => (n, Some(v)),
        None => (target, None),
    };
    let versions: Vec<String> = match version {
        Some(v) => vec![v.to_owned()],
        None => {
            let mut all = store.release_versions(name)?;
            release::sort_versions(&mut all);
            if all.is_empty() {
                return Err(not_found(format!("{name} has no release")));
            }
            all
        }
    };
    let mut failed = 0;
    for v in versions {
        match release::load(&store, name, &v) {
            Ok(r) => println!(
                "{name} {v}: verified ({}; manifest {}, {} files, signed by {} ({}), {} approval{})",
                r.state,
                &r.manifest_sha256[..12],
                r.manifest.files.len(),
                r.manifest.signer,
                &r.manifest.signer_id[..10],
                r.approvals.len(),
                if r.approvals.len() == 1 { "" } else { "s" }
            ),
            Err(e) => {
                failed += 1;
                println!("{name} {v}: DOESN'T VERIFY: {e:#}");
            }
        }
    }
    if failed > 0 {
        bail!(
            "{failed} release(s) don't verify: don't deploy or use them; roll back to one that does"
        );
    }
    Ok(())
}

/// What changed from release `from` to `to`.
pub fn diff(profile: &Profile, name: &str, from: &str, to: &str) -> Result<()> {
    let lines = release::diff(&FactoryStore::of(profile), name, from, to)?;
    println!("{name} {from} → {to}:");
    if lines.is_empty() {
        println!("  nothing changed but its version");
    }
    for line in lines {
        println!("  {line}");
    }
    Ok(())
}

/// The owner's agent for one command: on this machine only, with its workers' supervisor.
async fn host(
    profile: &Profile,
    what: &str,
    bridge: Option<PathBuf>,
) -> Result<(Host, crate::profile::InstanceLock)> {
    let instance = profile.lock_instance().map_err(|_| {
        anyhow!(
            "profile '{}' runs in a proofkin agent: {what} in its web UI (Agent factory), or stop that agent first",
            profile.name
        )
    })?;
    let key = profile.identity()?;
    let mut config = AgentConfig::new(profile.name.clone(), key.clone());
    config.network = Network::Local;
    config.bind = Some("127.0.0.1:0".parse()?);
    config.trust = profile.trust()?;
    config.task_ids = Some(profile.task_ids_path());
    config.worktrees_dir = profile.worktrees_dir();
    let (agent, _events) = Agent::bind(config).await?;
    let bridge = crate::setup::resolve_bridge(
        bridge.or_else(|| crate::profile::env_setting_os("BRIDGE").map(PathBuf::from)),
    );
    let supervisor = Supervisor::new(
        profile.clone(),
        agent.clone(),
        Launch {
            exe: std::env::current_exe()?,
            network: Network::Local,
            ip: None,
            bridge: Some(bridge),
        },
    );
    Ok((
        Host {
            owner: profile.clone(),
            agent,
            key,
            supervisor,
            ready_wait: crate::factory::runner::READY_WAIT,
        },
        instance,
    ))
}

/// [`deploy::deploy`], its refusal of a release that names no folders saying the flag that
/// names them (the web UI's words name none).
fn deploy_as_asked(
    profile: &Profile,
    addr: EndpointAddr,
    allowlist: &TrustStore,
    name: &str,
    ask: &DeployAsk,
) -> Result<deploy::Placed> {
    deploy::deploy(profile, addr, allowlist, name, ask).map_err(|e| {
        if e.to_string() == deploy::no_folders(name) {
            invalid(format!("{e} (--folders A,B)"))
        } else {
            e
        }
    })
}

/// Deploys agent `name` (its active release, or `version`) as a digital worker. With
/// `start`, its worker starts here and runs its canary (unless `no_canary`); then it starts
/// with your agent.
#[allow(clippy::too_many_arguments)]
pub async fn deploy(
    profile: &Profile,
    name: &str,
    version: Option<String>,
    folders: Option<Vec<String>>,
    no_canary: bool,
    start: bool,
    bridge: Option<PathBuf>,
) -> Result<()> {
    let ask = DeployAsk {
        version,
        folders,
        canary: !no_canary,
        start_with_owner: start.then_some(true),
        by: profile.name.clone(),
    };
    if !start {
        if profile.is_running() {
            bail!(
                "profile '{}' runs in a proofkin agent: deploy {name} in its web UI (Agent factory)",
                profile.name
            );
        }
        let id = profile.identity()?.public();
        let placed = deploy_as_asked(profile, id.into(), &profile.trust()?, name, &ask)?;
        if let Some(entry) = placed.entry {
            let mut trust = profile.trust()?;
            trust.allow(entry);
            trust.save(&profile.trust_path())?;
        }
        println!(
            "{name} {}: deployed on worker {} in {} ({}); it starts when you start it with your agent (proofkin --name {} --ui){}",
            placed.record.version,
            placed.record.worker,
            placed.record.folders.join(", "),
            placed.record.state,
            profile.name,
            if no_canary {
                ""
            } else {
                ", and runs its canary first"
            }
        );
        return Ok(());
    }
    let (host, _instance) = host(profile, &format!("deploy {name}"), bridge).await?;
    let result = async {
        let placed = deploy_as_asked(
            profile,
            host.agent.addr(),
            &host.agent.trusted(),
            name,
            &ask,
        )?;
        if let Some(entry) = placed.entry.clone() {
            host.agent.allow(entry.clone());
            let mut trust = profile.trust()?;
            trust.allow(entry);
            trust.save(&profile.trust_path())?;
        }
        let worker = placed.record.worker.clone();
        println!(
            "{name} {}: deployed on worker {worker} in {}",
            placed.record.version,
            placed.record.folders.join(", ")
        );
        if no_canary {
            host.supervisor.start(&worker)?;
            println!("{worker} takes work; it starts with your agent from now on");
            return Ok(());
        }
        println!("{worker}: its canary runs its smoke cases before it takes work");
        let printed = std::sync::Mutex::new(std::collections::HashSet::new());
        let record = deploy::run_canary(&host, name, &worker, |e: &Evaluation| {
            let mut printed = printed.lock().unwrap();
            for run in e
                .runs
                .iter()
                .filter(|r| r.state == crate::factory::report::RunState::Done)
            {
                if printed.insert(run.case.clone()) {
                    println!(
                        "  {} {}",
                        run.case,
                        if run.passed { "passed" } else { "FAILED" }
                    );
                }
            }
        })
        .await?;
        print_deployment(&record);
        if record.state != DeployState::Running {
            bail!("{name}'s canary failed");
        }
        println!(
            "{worker} takes work; it starts with your agent (proofkin --name {} --ui)",
            profile.name
        );
        Ok(())
    }
    .await;
    host.supervisor.stop_all().await;
    host.agent.shutdown().await;
    result
}

fn print_deployment(d: &crate::factory::store::DeploymentRecord) {
    println!(
        "  {} runs {} ({}){}",
        d.worker,
        d.version,
        d.state,
        d.previous_version
            .as_deref()
            .map(|p| format!(", before {p}"))
            .unwrap_or_default()
    );
    if let Some(c) = &d.canary {
        println!(
            "    canary: {} ({} smoke case{}){}",
            if c.passed { "passed" } else { "failed" },
            c.cases,
            if c.cases == 1 { "" } else { "s" },
            if c.failed.is_empty() {
                String::new()
            } else {
                format!(": {}", c.failed.join("; "))
            }
        );
    }
    if let Some(note) = &d.note {
        println!("    {note}");
    }
}

/// Makes an earlier release active on every deployment of `name`.
pub fn rollback(profile: &Profile, name: &str, to: Option<&str>, reason: &str) -> Result<()> {
    let (state, workers) = deploy::rollback(profile, name, to, reason, &profile.name)?;
    println!(
        "{name}: {} is active{}",
        state.active_version.as_deref().unwrap_or("none"),
        if workers.is_empty() {
            String::new()
        } else {
            format!(
                "; {} run{} it within seconds",
                workers.join(", "),
                if workers.len() == 1 { "s" } else { "" }
            )
        }
    );
    Ok(())
}

pub fn deprecate(
    profile: &Profile,
    name: &str,
    replacement: &str,
    reason: Option<&str>,
) -> Result<()> {
    lifecycle::deprecate(profile, name, replacement, reason, &profile.name)?;
    println!("{name}: deprecated; plans that use it are told to use {replacement}");
    Ok(())
}

pub fn reactivate(profile: &Profile, name: &str, reason: Option<&str>) -> Result<()> {
    lifecycle::reactivate(profile, name, reason, &profile.name)?;
    println!("{name}: active again");
    Ok(())
}

pub fn retire(profile: &Profile, name: &str, reason: &str, purge: bool) -> Result<()> {
    if profile.is_running() {
        bail!(
            "profile '{}' runs in a proofkin agent: retire {name} in its web UI (Agent factory), so its deployments stop there",
            profile.name
        );
    }
    let (state, stopped) = lifecycle::retire(profile, name, reason, purge, &profile.name)?;
    println!(
        "{name}: retired; plans can't use it{}; its releases and reports {}",
        if stopped.is_empty() {
            String::new()
        } else {
            format!(", and {} stopped", stopped.join(", "))
        },
        match &state.purge_after {
            Some(when) => format!("go after {when}"),
            None => "are kept for audit".into(),
        }
    );
    Ok(())
}

/// Prints agent `name`'s metrics per version since `since`.
pub fn print_metrics(profile: &Profile, name: &str, since: &str, json: bool) -> Result<()> {
    let since = metrics::parse_since(since, chrono::Utc::now())?;
    let m = metrics::metrics(profile, name, since)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&m)?);
        return Ok(());
    }
    println!("{name} since {}:", m.since);
    if m.versions.is_empty() {
        println!("  no task yet (its deployments' audits name none)");
    }
    for v in &m.versions {
        let refused: Vec<String> = v
            .refused_by
            .iter()
            .map(|(k, n)| format!("{n} {k}"))
            .collect();
        println!(
            "  {}: {} tasks: {} done, {} failed, {} refused{}; p50 {:.0}s, p95 {:.0}s; {:.1} attempts; checks {}",
            v.version,
            v.tasks,
            v.done,
            v.failed,
            v.refused,
            if refused.is_empty() {
                String::new()
            } else {
                format!(" ({})", refused.join(", "))
            },
            v.p50_secs,
            v.p95_secs,
            v.attempts,
            v.check_pass_rate
                .map(|r| format!("{:.0}% passed", r * 100.0))
                .unwrap_or_else(|| "none".into())
        );
        if !v.models.is_empty() {
            let models: Vec<String> = v.models.iter().map(|(m, n)| format!("{m} {n}")).collect();
            println!("    models: {}", models.join(", "));
        }
        let u = &v.usage;
        if u.input_tokens.is_some() || u.output_tokens.is_some() {
            println!(
                "    usage: {} input, {} output tokens{}{}",
                u.input_tokens.unwrap_or(0),
                u.output_tokens.unwrap_or(0),
                u.premium_requests
                    .map(|p| format!(", {p} premium requests"))
                    .unwrap_or_default(),
                u.cost_usd.map(|c| format!(", ${c:.2}")).unwrap_or_default()
            );
        }
    }
    Ok(())
}

/// Sets the budgets of `name`'s deployments (`clear`: back to its release's).
pub fn budget(
    profile: &Profile,
    name: &str,
    tasks_per_day: Option<u32>,
    attempts: Option<u32>,
    timeout: Option<&str>,
    clear: bool,
) -> Result<()> {
    let timeout_secs = timeout
        .map(crate::plan::parse_timeout)
        .transpose()
        .map_err(|e| anyhow!("--timeout: {e:#}"))?;
    let store = FactoryStore::of(profile);
    let current = store
        .state(name)?
        .and_then(|s| s.deployments.first().map(|d| d.budgets.clone()))
        .unwrap_or_default();
    let budgets = if clear {
        Budgets::default()
    } else {
        Budgets {
            tasks_per_day: tasks_per_day.or(current.tasks_per_day),
            attempts: attempts.or(current.attempts),
            timeout_secs: timeout_secs.or(current.timeout_secs),
        }
    };
    let now = deploy::set_budgets(profile, name, budgets, &profile.name)?;
    println!(
        "{name}: {}; its workers apply them within seconds",
        release::budgets_text(&now)
    );
    Ok(())
}

/// What `factory watch` asks to change.
#[derive(Debug, Clone, Default)]
pub struct WatchChange {
    pub when: Option<String>,
    pub fail_after: Option<u32>,
    pub off: bool,
    pub now: bool,
}

/// A drift watch in a few lines.
fn print_watch(name: &str, w: &crate::factory::store::DriftWatch) {
    println!(
        "{name}: drift watch {}, an incident after {} failed run{} in a row; {}",
        w.when,
        w.fail_after,
        if w.fail_after == 1 { "" } else { "s" },
        match drift::next_run(w) {
            Some(next) => format!("next run {}", next.format("%Y-%m-%d %H:%M")),
            None => "paused".into(),
        }
    );
    if let Some(last) = &w.last {
        println!(
            "  last run ({} at {}): {}{}",
            last.version,
            last.at,
            drift::run_text(last),
            last.evaluation
                .as_deref()
                .map(|e| format!(" ({e})"))
                .unwrap_or_default()
        );
    }
    if w.failures > 0 {
        println!("  {} failed run(s) in a row", w.failures);
    }
    if w.incident.is_some() {
        println!(
            "  incident open: {} (proofkin ops report)",
            drift::monitor(name)
        );
    }
}

/// Shows, sets, pauses or runs (`change.now`, here, with the profile's agent stopped) agent
/// `name`'s drift watch.
pub async fn watch(
    profile: &Profile,
    name: &str,
    change: WatchChange,
    bridge: Option<PathBuf>,
) -> Result<()> {
    let store = FactoryStore::of(profile);
    let state = store
        .state(name)?
        .ok_or_else(|| not_found(format!("the factory has no agent '{name}'")))?;
    let current = state.watch.clone();
    if change.off || change.when.is_some() || change.fail_after.is_some() {
        let when = match (&change.when, &current) {
            (Some(when), _) => when.clone(),
            (None, Some(w)) => w.when.clone(),
            (None, None) if change.off => bail!("{name} has no drift watch to pause"),
            (None, None) => bail!("say when it runs: --when \"daily 03:00\""),
        };
        let ask = drift::WatchAsk {
            when,
            fail_after: change
                .fail_after
                .or(current.as_ref().map(|w| w.fail_after))
                .unwrap_or(drift::DEFAULT_FAIL_AFTER),
            enabled: !change.off,
        };
        let set = drift::set(profile, name, &ask, &profile.name)?;
        print_watch(name, &set);
        if set.enabled && !change.now {
            println!(
                "  your agent runs it while it runs (proofkin --name {} --ui); run it now: {}",
                profile.name,
                hint(profile, &format!("watch {name} --now"))
            );
        }
    } else if !change.now {
        match &current {
            Some(w) => print_watch(name, w),
            None => println!(
                "{name}: no drift watch ({})",
                hint(profile, &format!("watch {name} --when \"daily 03:00\""))
            ),
        }
        return Ok(());
    }
    if !change.now {
        return Ok(());
    }
    let (host, _instance) = host(profile, &format!("run {name}'s drift watch"), bridge).await?;
    let printed = std::sync::Mutex::new(std::collections::HashSet::new());
    let notify = |e: &crate::ops::IncidentEvent| {
        println!(
            "! [{}] {:?}: {}",
            e.monitor,
            e.kind,
            e.text.lines().next().unwrap_or_default()
        )
    };
    println!("{name}: its drift watch evaluates its active release on its deployment now");
    let ran = drift::run(
        &host,
        name,
        true,
        |e: &Evaluation| {
            let mut printed = printed.lock().unwrap();
            for run in e
                .runs
                .iter()
                .filter(|r| r.state == crate::factory::report::RunState::Done)
            {
                if printed.insert(run.case.clone()) {
                    println!(
                        "  {} {}",
                        run.case,
                        if run.passed { "passed" } else { "FAILED" }
                    );
                }
            }
        },
        &notify,
    )
    .await;
    host.agent.shutdown().await;
    let ran = ran?;
    println!(
        "{name} {}: drift watch {}{}",
        ran.version,
        drift::run_text(&ran),
        ran.evaluation
            .as_deref()
            .map(|e| format!(" ({e})"))
            .unwrap_or_default()
    );
    if ran.outcome == crate::factory::store::DriftOutcome::Failed {
        bail!("{name} failed its drift watch");
    }
    Ok(())
}

/// Publishes `versions` of agent `name` (none: its active release) to peers `to` (none: those
/// it is published to), or unpublishes it (`off`): the grants in the allowlist, which the
/// profile's agent holds while it runs.
pub fn publish(
    profile: &Profile,
    name: &str,
    to: Vec<String>,
    versions: Vec<String>,
    off: bool,
) -> Result<()> {
    if profile.is_running() {
        bail!(
            "profile '{}' runs in a proofkin agent: publish {name} in its web UI (Agent factory), or stop that agent first",
            profile.name
        );
    }
    let store = FactoryStore::of(profile);
    let mut trust = profile.trust()?;
    let now = catalog::publication(&store, &trust, name)?;
    let (peers, versions) = if off {
        (Vec::new(), Vec::new())
    } else {
        let peers = if to.is_empty() { now.peers } else { to };
        let versions = if versions.is_empty() {
            let active = store.state(name)?.and_then(|s| s.active_version);
            match active {
                Some(v) => vec![v],
                None => bail!(
                    "{name} has no active release to publish: release it first, or name --versions"
                ),
            }
        } else {
            versions
        };
        if peers.is_empty() {
            bail!("say who to publish it to: --to PEER,... (names on your allowlist)");
        }
        (peers, versions)
    };
    let (shared, _) = catalog::publish(&store, &mut trust, name, &peers, &versions, &profile.name)?;
    trust.save(&profile.trust_path())?;
    if shared.versions.is_empty() || shared.peers.is_empty() {
        println!("{name}: unpublished; no peer sees it in their catalog any more");
    } else {
        println!(
            "{name}: published {} to {}; they see it in their catalog when your agent runs (proofkin --name {} ...)",
            shared.versions.join(", "),
            shared.peers.join(", "),
            profile.name
        );
    }
    Ok(())
}

/// The profile's agent for a moment, to reach its peers over `network` (`what` it is for,
/// should the profile's own agent run: two would share one identity).
async fn peer_agent(
    profile: &Profile,
    network: Network,
    what: &str,
) -> Result<(Agent, crate::profile::InstanceLock)> {
    let instance = profile.lock_instance().map_err(|_| {
        anyhow!(
            "profile '{}' runs in a proofkin agent: {what} in its web UI (Agent factory › Catalog), or stop that agent first",
            profile.name
        )
    })?;
    let mut config = AgentConfig::new(profile.name.clone(), profile.identity()?);
    config.network = network;
    if network == Network::Local {
        config.bind = Some("0.0.0.0:0".parse()?);
    }
    config.trust = profile.trust()?;
    config.task_ids = Some(profile.task_ids_path());
    config.worktrees_dir = profile.worktrees_dir();
    let (agent, _events) = Agent::bind(config).await?;
    Ok((agent, instance))
}

/// Lists what the peers connected within `wait` seconds publish to this profile.
pub async fn catalog(profile: &Profile, wait: u64, network: Network) -> Result<()> {
    let trust = profile.trust()?;
    if trust.peers.is_empty() {
        println!(
            "profile '{}' has no allowlisted peers: no one publishes to it",
            profile.name
        );
        return Ok(());
    }
    let (agent, _instance) = peer_agent(profile, network, "list the catalog").await?;
    for peer in &trust.peers {
        let (agent, addr) = (agent.clone(), peer.addr());
        tokio::spawn(async move {
            let _ = agent.connect(addr).await;
        });
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(wait);
    while std::time::Instant::now() < deadline && agent.peers().len() < trust.peers.len() {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    // Catalogs come with the hello: a moment for the last ones.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let connected = agent.peers().len();
    let published = agent.catalogs();
    agent.shutdown().await;
    let listed = catalog::listed(&FactoryStore::of(profile), &published);
    println!(
        "catalog of profile '{}' ({connected} of {} peers connected in {wait}s):",
        profile.name,
        trust.peers.len()
    );
    if listed.is_empty() {
        println!("  nothing: no connected peer publishes an agent to you");
    }
    for (entry, here) in listed {
        let s = &entry.item.eval_summary;
        println!(
            "  {}/{}@{}  {}  {:.0}% of its own runs passed, {} case{} of its own, the built-in attacks {}  (key {}){}",
            entry.publisher,
            entry.item.name,
            entry.item.version,
            entry.item.access,
            s.pass_rate * 100.0,
            s.cases,
            if s.cases == 1 { "" } else { "s" },
            if s.adversarial_passed {
                "resisted"
            } else {
                "NOT all resisted"
            },
            &entry.publisher_id[..10],
            match here {
                Some(state) => format!(
                    "; {}",
                    serde_json::to_value(state)
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_owned))
                        .unwrap_or_default()
                ),
                None => String::new(),
            }
        );
        println!("      {}", entry.item.description);
        if here.is_none() {
            println!(
                "      import it: {}",
                hint(
                    profile,
                    &format!(
                        "import {}/{}@{}",
                        entry.publisher, entry.item.name, entry.item.version
                    )
                )
            );
        }
    }
    Ok(())
}

/// Imports `target` (`publisher/name@version`) from the publisher, connected within `wait`
/// seconds: its bundle verified and kept, nothing run; active at once with `accept`.
pub async fn import(
    profile: &Profile,
    target: &str,
    accept: bool,
    wait: u64,
    network: Network,
) -> Result<()> {
    let (publisher, name, version) = catalog::parse_target(target)?;
    let trust = profile.trust()?;
    let peer = trust.get_by_name(&publisher).cloned().ok_or_else(|| {
        anyhow!(
            "'{publisher}' isn't on your allowlist (proofkin peers list --profile {})",
            profile.name
        )
    })?;
    let (agent, _instance) = peer_agent(profile, network, &format!("import {target}")).await?;
    let fetched = async {
        let _ = tokio::time::timeout(Duration::from_secs(wait), agent.connect(peer.addr())).await;
        let deadline = std::time::Instant::now() + Duration::from_secs(wait);
        while !agent.peers().iter().any(|p| p.id == peer.id) {
            if std::time::Instant::now() > deadline {
                bail!("{publisher} didn't connect in {wait}s: is its agent running?");
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        println!("{publisher} connected: asking for {name} {version}");
        agent
            .request_bundle(peer.id, &name, &version, Duration::from_secs(120))
            .await
    }
    .await;
    agent.shutdown().await;
    let bundle = fetched?;
    if bundle.agent != name || bundle.version != version {
        bail!(
            "{publisher} sent {} {}, not {name} {version}",
            bundle.agent,
            bundle.version
        );
    }
    let files = bundle.files.len();
    let imported = catalog::import(
        &FactoryStore::of(profile),
        &profile.name,
        &profile.identity()?,
        bundle,
        &catalog::ImportAsk {
            publisher: publisher.clone(),
            publisher_id: peer.id.to_string(),
            accept_publisher_report: accept,
            by: profile.name.clone(),
        },
    )?;
    println!(
        "{name} {version}: imported from {publisher} (key {}): {files} files, signed by its key, every hash verified; nothing ran",
        &imported.publisher_id[..10]
    );
    match (&imported.release, imported.state) {
        (_, catalog::ImportState::Active) => println!(
            "  active: you accepted {publisher}'s report; plans use it as agent={name}; {}",
            hint(profile, &format!("deploy {name} --start"))
        ),
        (Some(r), _) => println!(
            "  you accepted {publisher}'s report; its release here waits for your sign-off of what it asks for ({}): {}",
            r.manifest.increases.join("; "),
            hint(profile, &format!("approve {name}"))
        ),
        (None, _) => println!(
            "  it becomes active once it passes an evaluation on your accounts: {}",
            hint(profile, &format!("evaluate {name} --yes"))
        ),
    }
    Ok(())
}

/// Prints `name`'s releases and deployments, for `factory show`.
pub fn print_releases(profile: &Profile, name: &str) -> Result<()> {
    let store = FactoryStore::of(profile);
    let releases = release::releases(&store, name)?;
    if !releases.is_empty() {
        println!("  releases ({}):", hint(profile, &format!("verify {name}")));
    }
    let state = store.state(name)?;
    let active = state.as_ref().and_then(|s| s.active_version.clone());
    for (version, found) in &releases {
        match found {
            Ok(r) => {
                let waiting: Vec<String> = r.pending().into_iter().map(|a| a.approver).collect();
                println!(
                    "    {version}{}  {}{}  {} ({})",
                    if active.as_deref() == Some(version.as_str()) {
                        " (active)"
                    } else {
                        ""
                    },
                    r.state,
                    if waiting.is_empty() {
                        String::new()
                    } else {
                        format!(", waiting for {}", waiting.join(", "))
                    },
                    r.manifest.evaluation,
                    r.manifest.change
                );
            }
            Err(e) => println!("    {version}  DOESN'T VERIFY: {e:#}"),
        }
    }
    if let Some(state) = &state {
        if let Some(r) = &state.replacement {
            println!("  replacement: {r}");
        }
        if !state.deployments.is_empty() {
            println!("  deployments:");
            for d in &state.deployments {
                print_deployment(d);
            }
        }
        if let Some(w) = &state.watch {
            print!("  ");
            print_watch(name, w);
        }
        if let Some(p) = &state.publication {
            let peers = catalog::publication(&store, &profile.trust()?, name)
                .map(|s| s.peers)
                .unwrap_or_default();
            println!(
                "  published: {} to {}",
                p.versions.join(", "),
                if peers.is_empty() {
                    "no peer".to_owned()
                } else {
                    peers.join(", ")
                }
            );
        }
        if let Some(o) = &state.imported {
            println!(
                "  imported from {} (key {}):",
                o.publisher,
                &o.publisher_id[..10.min(o.publisher_id.len())]
            );
            for v in &o.versions {
                let here = catalog::import_state(&store, name, &v.version);
                println!(
                    "    {} {}{}",
                    v.version,
                    serde_json::to_value(here)
                        .ok()
                        .and_then(|x| x.as_str().map(str::to_owned))
                        .unwrap_or_default(),
                    if v.accepted_report {
                        " (its publisher's report accepted)"
                    } else {
                        ""
                    }
                );
            }
        }
    }
    Ok(())
}

/// The waiting canaries' host, for a running agent's driver: runs each in turn; one that
/// couldn't run is tried again a minute later.
pub async fn drive_canaries(host: Arc<Host>, changed: Arc<dyn Fn(&str) + Send + Sync>) {
    let mut failed: std::collections::HashMap<(String, String), std::time::Instant> =
        std::collections::HashMap::new();
    loop {
        for (name, worker) in deploy::waiting_canaries(&host.owner) {
            let key = (name.clone(), worker.clone());
            if failed
                .get(&key)
                .is_some_and(|at| at.elapsed() < Duration::from_secs(60))
            {
                continue;
            }
            match deploy::run_canary(&host, &name, &worker, |_| changed(&name)).await {
                Ok(_) => {
                    failed.remove(&key);
                    changed(&name);
                }
                Err(e) => {
                    failed.insert(key, std::time::Instant::now());
                    eprintln!("the canary of {name} on {worker} didn't run: {e:#}");
                    changed(&name);
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::factory::RefusalKind;
    use crate::factory::blueprint::tests::sample;
    use crate::factory::release::tests::Fixture;
    use iroh::SecretKey;
    use serde_json::{Value, json};

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=Alice",
                "-c",
                "user.email=alice@example.com",
            ])
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    fn write(path: &Path, value: &Value) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, serde_json::to_string_pretty(value).unwrap()).unwrap();
    }

    /// AC-33: `factory release --from` releases exactly the folder's commit, from the passed
    /// report of exactly its build that its owner signed and kept there.
    #[test]
    fn an_agent_kept_as_code_is_released_from_its_folders_commit_and_signed_report() {
        let f = Fixture::new();
        let passed = f.evaluated(|_| {}, false);
        let me = f.owner.public().to_string();
        let repo = f.dir.path().join("repo");
        let mut blueprint = sample();
        let evals = blueprint.as_object_mut().unwrap().remove("evals").unwrap();
        write(&repo.join(code::BLUEPRINT), &blueprint);
        write(&repo.join(code::EVALS), &evals);
        std::fs::create_dir_all(repo.join(code::FIXTURES)).unwrap();
        std::fs::write(repo.join(code::FIXTURES).join("q3.pdf"), "%PDF q3").unwrap();
        git(&repo, &["init", "-q"]);
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "fin-analyst"]);
        let from = |name: &str| from_folder("alice", &me, &f.store, name, &repo);
        let refused = |name: &str| {
            let e = from(name).unwrap_err();
            (crate::factory::refusal_kind(&e), format!("{e:#}"))
        };

        let (_, why) = refused("other");
        assert!(
            why.contains("the folder's agent is fin-analyst, not other"),
            "{why}"
        );
        let (kind, why) = refused("fin-analyst");
        assert_eq!(kind, Some(RefusalKind::Conflict));
        assert!(why.contains("has no passed evaluation of build"), "{why}");

        let mut theirs = passed.clone();
        theirs.sign("mallory", &SecretKey::generate());
        write(
            &repo.join(code::REPORTS).join("theirs.json"),
            &serde_json::to_value(&theirs).unwrap(),
        );
        let (_, why) = refused("fin-analyst");
        assert!(
            why.contains("signed by you"),
            "someone else's report: {why}"
        );

        // A deployment's canary ran its smoke cases only: it proves no release.
        let mut canary = passed.clone();
        canary.canary = Some(crate::factory::report::CanaryOf {
            worker: "fin-analyst".into(),
            version: "1.0.0".into(),
            drift: false,
        });
        canary.sign("alice", &f.owner);
        write(
            &repo.join(code::REPORTS).join("canary.json"),
            &serde_json::to_value(&canary).unwrap(),
        );
        let (_, why) = refused("fin-analyst");
        assert!(
            why.contains("has no passed evaluation of build"),
            "a canary's report: {why}"
        );

        write(
            &repo.join(code::REPORTS).join(format!("{}.json", passed.id)),
            &serde_json::to_value(&passed).unwrap(),
        );
        let (kind, why) = refused("fin-analyst");
        assert_eq!(kind, Some(RefusalKind::Conflict));
        assert!(why.contains("changes that aren't committed"), "{why}");

        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "its report"]);
        let head = git(&repo, &["rev-parse", "HEAD"]);
        let (id, commit) = from("fin-analyst").unwrap();
        assert_eq!((&id, &commit), (&passed.id, &head));
        assert!(f.store.fixtures_dir("fin-analyst").join("q3.pdf").is_file());
        let asked = Asked {
            evaluation: id,
            by: "alice".into(),
            dir: Some(repo.display().to_string()),
            commit: Some(commit),
            ..Asked::default()
        };
        let made = release::request(&f.store, "alice", &f.owner, "fin-analyst", &asked).unwrap();
        assert_eq!(made.manifest.origin.commit.as_deref(), Some(head.as_str()));
        release::load(&f.store, "fin-analyst", &made.manifest.version).unwrap();

        blueprint["title"] = "Retitled".into();
        write(&repo.join(code::BLUEPRINT), &blueprint);
        git(&repo, &["commit", "-q", "-am", "retitled"]);
        let (_, why) = refused("fin-analyst");
        assert!(
            why.contains("has no passed evaluation of build"),
            "another build: {why}"
        );
    }

    /// `factory deploy` of a release that names no folders, without --folders: refused in
    /// the web UI's words and the flag that names them; a folder you don't share keeps its own.
    #[test]
    fn deploying_a_release_that_names_no_folders_says_the_folders_flag() {
        let home = tempfile::tempdir().unwrap();
        let owner = Profile::open_in(home.path(), "alice").unwrap();
        owner.save().unwrap();
        let f = Fixture::in_store(FactoryStore::of(&owner));
        let passed = f.evaluated(
            |v| {
                v["approvers"] = json!([]);
                v["capabilities"]["mcp"] = json!([]);
                v["model"]["needs_mcp"] = json!(false);
                v["memory"] = json!({});
                v["deploy"]["folders"] = json!([]);
            },
            false,
        );
        f.release(&passed.id, None).unwrap();
        let refused = |folders: Option<Vec<String>>| {
            let ask = DeployAsk {
                folders,
                by: "alice".into(),
                ..DeployAsk::default()
            };
            let to = f.owner.public().into();
            let trust = TrustStore::default();
            let e = deploy_as_asked(&owner, to, &trust, "fin-analyst", &ask).unwrap_err();
            (crate::factory::refusal_kind(&e), format!("{e:#}"))
        };

        let (kind, why) = refused(None);
        assert_eq!(kind, Some(RefusalKind::Invalid));
        assert_eq!(
            why,
            "fin-analyst's release names no folders to work in: pick at least one folder you \
             share (--folders A,B)"
        );
        let (kind, why) = refused(Some(vec!["ledger".into()]));
        assert_eq!(kind, Some(RefusalKind::Invalid));
        assert_eq!(why, "you share no folder 'ledger'");
    }
}
