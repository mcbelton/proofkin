//! Digital workers an owner runs (see [`Profile::create_digital_worker`]): each is
//! its own agent, with its own key, allowlist, folders and audit log, on its
//! owner's sign-ins. This module creates them with their folders and allowlists,
//! lists and deletes them, and starts and stops them for the owner's agent: a
//! [`Supervisor`] runs each one as a background `proofkin` agent that dials its
//! owner and takes its input from it, so it stops cleanly when the owner stops it,
//! or when the owner's agent ends, however that ends.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use iroh::{EndpointAddr, EndpointId};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, ChildStdin};
use tokio::sync::{broadcast, oneshot, watch};

use crate::agent::{Agent, Network};
use crate::llm::{Access, Provider};
use crate::profile::{Profile, validate_name};
use crate::shares::{DATA_SHARE, Share};
use crate::trust::{DEFAULT_TASKS_PER_HOUR, Ticket, TrustStore, TrustedPeer};

/// Longest role, in characters (it goes into every task's prompt).
pub const MAX_ROLE_CHARS: usize = 200;
/// How long a worker gets to quit when asked, before it is ended.
const STOP_GRACE: Duration = Duration::from_secs(10);
/// Lines of a worker's log kept when it ends on its own.
const LOG_TAIL_LINES: usize = 40;

/// A worker to create: its name, what it's for, the providers it runs on its
/// owner's sign-ins, and which of its owner's shared folders it works in.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct WorkerSpec {
    pub name: String,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub providers: Vec<Provider>,
    #[serde(default)]
    pub folders: Vec<String>,
    /// Its owner's agent starts it whenever it starts.
    #[serde(default)]
    pub start_with_owner: bool,
    /// It uses its owner's Work IQ account.
    #[serde(default)]
    pub workiq: bool,
}

/// A worker just created, and the entry for its owner's allowlist (added by the
/// caller: to the running agent, or to the saved allowlist).
#[derive(Debug)]
pub struct Created {
    pub worker: Profile,
    pub id: EndpointId,
    pub entry: TrustedPeer,
}

/// Creates `owner`'s digital worker `spec.name`. `owner_addr` is where the owner's
/// agent is (its id is enough), `allowlist` the owner's allowlist. The worker
/// shares each folder in `spec.folders` as its owner does (path, most access,
/// checks, worktrees) and lets its owner give it tasks there and in its own data
/// folder; the owner's allowlist gets the worker, chat only. Nothing is left behind
/// when it fails.
pub fn create(
    owner: &Profile,
    owner_addr: EndpointAddr,
    allowlist: &TrustStore,
    spec: &WorkerSpec,
) -> Result<Created> {
    let name = spec.name.trim();
    validate_name(name).map_err(|_| {
        anyhow!("a worker's name may only use letters, digits, '-', '_' and '.' (at most 64)")
    })?;
    if name == owner.name {
        bail!("a worker can't have its owner's name");
    }
    if allowlist.get_by_name(name).is_some() {
        bail!("'{name}' is already on your allowlist; pick another name");
    }
    let role = spec
        .role
        .as_deref()
        .map(|r| r.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|r| !r.is_empty());
    if role
        .as_ref()
        .is_some_and(|r| r.chars().count() > MAX_ROLE_CHARS)
    {
        bail!("a role is at most {MAX_ROLE_CHARS} characters");
    }
    let mut providers = spec.providers.clone();
    providers.sort();
    providers.dedup();
    for provider in &providers {
        if !owner.is_configured(*provider) {
            bail!(
                "'{}' isn't signed in to {provider}, and its workers run on its sign-ins: {}",
                owner.name,
                owner.login_hint(*provider)
            );
        }
    }
    if spec.workiq && owner.config.workiq.is_none() {
        bail!(
            "'{}' isn't signed in to Work IQ, and its workers use its account: {}",
            owner.name,
            owner.workiq_login_hint()
        );
    }
    let shares = owner.shares()?;
    let mut folders: Vec<Share> = Vec::new();
    for folder in &spec.folders {
        if folder == DATA_SHARE {
            bail!("every worker has its own data folder; pick folders you share");
        }
        let share = shares
            .get(folder)
            .ok_or_else(|| anyhow!("you share no folder '{folder}'"))?;
        if !folders.iter().any(|f| f.name == share.name) {
            folders.push(share.clone());
        }
    }
    let worker = owner.create_digital_worker(name, role)?;
    finish(
        owner,
        owner_addr,
        worker.clone(),
        providers,
        folders,
        spec.start_with_owner,
        spec.workiq,
    )
    .inspect_err(|_| {
        let _ = worker.delete();
    })
}

fn finish(
    owner: &Profile,
    owner_addr: EndpointAddr,
    mut worker: Profile,
    providers: Vec<Provider>,
    folders: Vec<Share>,
    start_with_owner: bool,
    workiq: bool,
) -> Result<Created> {
    worker.config.providers = providers;
    worker.config.start_with_owner = start_with_owner;
    worker.config.workiq_from_owner = workiq;
    worker.save()?;
    if !folders.is_empty() {
        worker.update_shares(|shares| {
            for f in &folders {
                shares.add_with(
                    &f.name,
                    &f.path,
                    f.max_access,
                    f.checks.clone(),
                    f.worktrees,
                )?;
            }
            Ok(())
        })?;
    }
    let id = worker.identity()?.public();
    let mut granted = vec![DATA_SHARE.to_owned()];
    granted.extend(folders.iter().map(|f| f.name.clone()));
    let mut trust = worker.trust()?;
    trust.allow(TrustedPeer {
        name: owner.name.clone(),
        id: owner_addr.id,
        addrs: owner_addr.addrs.clone(),
        can_delegate: true,
        tasks_per_hour: DEFAULT_TASKS_PER_HOUR,
        // Each folder caps it at the most its owner allows there.
        access: folders
            .iter()
            .map(|f| f.max_access)
            .max()
            .unwrap_or(Access::ReadOnly),
        shares: granted,
        m365: None,
        github: None,
        agents: Vec::new(),
    });
    trust.save(&worker.trust_path())?;
    let entry = TrustedPeer {
        name: worker.name.clone(),
        id,
        addrs: Default::default(),
        can_delegate: false,
        tasks_per_hour: DEFAULT_TASKS_PER_HOUR,
        access: Access::ReadOnly,
        shares: vec![DATA_SHARE.to_owned()],
        m365: None,
        github: None,
        agents: Vec::new(),
    };
    Ok(Created { worker, id, entry })
}

/// `owner`'s digital worker `name`.
pub fn worker_of(owner: &Profile, name: &str) -> Result<Profile> {
    let root = owner.dir.parent().context("profile has no parent folder")?;
    let worker = Profile::open_in(root, name)?;
    if worker.config.owner.as_deref() != Some(owner.name.as_str())
        || !worker.dir.join("profile.json").is_file()
    {
        bail!("'{name}' isn't a digital worker of '{}'", owner.name);
    }
    Ok(worker)
}

/// `owner`'s digital workers, by name.
pub fn workers_of(owner: &Profile) -> Result<Vec<Profile>> {
    let root = owner.dir.parent().context("profile has no parent folder")?;
    let Ok(entries) = std::fs::read_dir(root) else {
        return Ok(Vec::new());
    };
    let mut workers: Vec<Profile> = entries
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|name| validate_name(name).is_ok())
        .filter_map(|name| Profile::open_in(root, &name).ok())
        .filter(|p| {
            p.config.owner.as_deref() == Some(owner.name.as_str())
                && p.dir.join("profile.json").is_file()
        })
        .collect();
    workers.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(workers)
}

/// The endpoint ids of `owner`'s digital workers.
pub fn worker_ids(owner: &Profile) -> Result<Vec<EndpointId>> {
    workers_of(owner)?
        .iter()
        .map(|w| Ok(w.identity()?.public()))
        .collect()
}

/// The folders `worker` shares besides its data folder.
pub fn folders(worker: &Profile) -> Vec<String> {
    worker
        .shares()
        .map(|s| {
            s.shares
                .into_iter()
                .filter(|f| f.name != DATA_SHARE)
                .map(|f| f.name)
                .collect()
        })
        .unwrap_or_default()
}

/// Deletes `owner`'s digital worker `name`: its identity, provider homes and
/// profile folder (allowlist, folder settings, audit log, runs). Its data folder
/// stays. Refused while it runs; the caller takes it off the owner's allowlist and
/// squads (see [`forget`]).
pub fn delete(owner: &Profile, name: &str) -> Result<()> {
    let worker = worker_of(owner, name)?;
    if worker.is_running() {
        bail!("'{name}' is running; stop it before deleting it");
    }
    worker.delete()
}

/// Takes deleted worker `name` off `owner`'s saved allowlist and out of its saved
/// squads (the web UI changes its running agent's instead). Returns the squads it
/// was in.
pub fn forget(owner: &Profile, name: &str) -> Result<Vec<String>> {
    let mut trust = owner.trust()?;
    if trust.remove(name) {
        trust.save(&owner.trust_path())?;
    }
    let mut squads = crate::plan::SquadStore::load(&owner.squads_path())?;
    let left = squads.remove_member(name);
    if !left.is_empty() {
        squads.save(&owner.squads_path())?;
    }
    Ok(left)
}

/// What a worker's process is doing, as far as its owner's agent knows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Process {
    /// Started by this agent and running.
    Running { since_ms: u128 },
    /// Stopped by its owner.
    Stopped { at_ms: u128 },
    /// Ended on its own, with its exit code and the end of its log.
    Exited {
        at_ms: u128,
        code: Option<i32>,
        log: String,
    },
    /// Running, but not started by this agent.
    Elsewhere,
    /// Not running.
    Off,
}

struct Running {
    since_ms: u128,
    /// The worker's input: `/quit` asks it to stop, and it stops when this closes.
    input: Option<ChildStdin>,
    kill: Option<oneshot::Sender<()>>,
    ended: watch::Receiver<bool>,
    stopping: bool,
}

/// How workers run: the `proofkin` program and the network they join.
#[derive(Debug, Clone)]
pub struct Launch {
    pub exe: PathBuf,
    pub network: Network,
    /// With [`Network::Local`], the IP they listen on (their owner's; default
    /// 127.0.0.1).
    pub ip: Option<IpAddr>,
    /// The bridge, for workers that run Claude or Codex.
    pub bridge: Option<PathBuf>,
}

/// Starts and stops an owner's digital workers as background agents of this one.
pub struct Supervisor {
    owner: Profile,
    agent: Agent,
    launch: Launch,
    running: Mutex<HashMap<String, Running>>,
    ended: Mutex<HashMap<String, Process>>,
    changed: broadcast::Sender<String>,
}

impl Supervisor {
    /// A supervisor for `owner`'s workers, which dial `agent` (the owner's).
    pub fn new(owner: Profile, agent: Agent, launch: Launch) -> Arc<Self> {
        Arc::new(Self {
            owner,
            agent,
            launch,
            running: Mutex::new(HashMap::new()),
            ended: Mutex::new(HashMap::new()),
            changed: broadcast::channel(64).0,
        })
    }

    /// Gets a worker's name whenever its process starts or ends.
    pub fn subscribe(&self) -> broadcast::Receiver<String> {
        self.changed.subscribe()
    }

    /// What `worker`'s process is doing.
    pub fn process(&self, worker: &Profile) -> Process {
        if let Some(r) = self.running.lock().unwrap().get(&worker.name) {
            return Process::Running {
                since_ms: r.since_ms,
            };
        }
        if worker.is_running() {
            return Process::Elsewhere;
        }
        self.ended
            .lock()
            .unwrap()
            .get(&worker.name)
            .cloned()
            .unwrap_or(Process::Off)
    }

    /// The command that runs `worker` as a background agent dialing the owner at
    /// `ticket`.
    fn command(&self, worker: &Profile, ticket: &str) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new(&self.launch.exe);
        cmd.arg("--name").arg(&worker.name);
        let providers = &worker.config.providers;
        if !providers.is_empty() {
            let list: Vec<String> = providers.iter().map(Provider::to_string).collect();
            cmd.arg("--providers").arg(list.join(",")).arg("--serve");
            if let Some(bridge) = &self.launch.bridge
                && providers.iter().any(|p| *p != Provider::Copilot)
            {
                cmd.arg("--bridge").arg(bridge);
            }
        }
        if self.launch.network == Network::Local {
            let ip = self.launch.ip.unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST));
            cmd.args(["--network", "local", "--listen"])
                .arg(SocketAddr::new(ip, 0).to_string());
        }
        cmd.arg("--peer").arg(ticket);
        // Its own process group: Ctrl+C in the owner's terminal is for the owner,
        // which then stops its workers cleanly.
        #[cfg(windows)]
        cmd.creation_flags(0x0000_0200); // CREATE_NEW_PROCESS_GROUP
        #[cfg(unix)]
        cmd.process_group(0);
        cmd
    }

    /// Starts `owner`'s worker `name` as a background agent that dials this one; its
    /// output goes to `agent.log` in its profile folder.
    pub fn start(self: &Arc<Self>, name: &str) -> Result<()> {
        let owner = self.owner.reload()?;
        let worker = worker_of(&owner, name)?;
        if let Some(p) = worker
            .config
            .providers
            .iter()
            .find(|p| !owner.is_configured(**p))
        {
            bail!(
                "'{}' isn't signed in to {p}, and '{name}' runs on its sign-ins: {}",
                owner.name,
                owner.login_hint(*p)
            );
        }
        let ticket = Ticket {
            name: owner.name.clone(),
            addr: self.agent.addr(),
        }
        .encode();
        // Held until it's recorded, so two starts can't both run it.
        let mut running = self.running.lock().unwrap();
        if running.contains_key(name) {
            bail!("'{name}' is already running");
        }
        if worker.is_running() {
            bail!("'{name}' is already running, started outside this agent");
        }
        let log_path = worker.dir.join("agent.log");
        let log = std::fs::File::create(&log_path)
            .with_context(|| format!("creating {}", log_path.display()))?;
        let mut child = self
            .command(&worker, &ticket)
            .stdin(Stdio::piped())
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log))
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("starting {}", self.launch.exe.display()))?;
        let (kill, kill_rx) = oneshot::channel();
        let (ended_tx, ended) = watch::channel(false);
        running.insert(
            name.to_owned(),
            Running {
                since_ms: now_ms(),
                input: child.stdin.take(),
                kill: Some(kill),
                ended,
                stopping: false,
            },
        );
        drop(running);
        self.ended.lock().unwrap().remove(name);
        let this = self.clone();
        let name = name.to_owned();
        tokio::spawn(async move {
            let status = wait(&mut child, kill_rx).await;
            this.finished(&name, status, &log_path);
            let _ = ended_tx.send(true);
        });
        let _ = self.changed.send(worker.name);
        Ok(())
    }

    fn finished(&self, name: &str, status: Option<ExitStatus>, log: &Path) {
        let stopping = self
            .running
            .lock()
            .unwrap()
            .remove(name)
            .is_some_and(|r| r.stopping);
        let at_ms = now_ms();
        let process = if stopping {
            Process::Stopped { at_ms }
        } else {
            Process::Exited {
                at_ms,
                code: status.and_then(|s| s.code()),
                log: log_tail(log, LOG_TAIL_LINES),
            }
        };
        self.ended.lock().unwrap().insert(name.to_owned(), process);
        let _ = self.changed.send(name.to_owned());
    }

    /// Stops worker `name` if this agent started it: asks it to quit (it says
    /// goodbye to its peers), and ends it if it hasn't quit after a while. Returns
    /// whether it was running here.
    pub async fn stop(&self, name: &str) -> bool {
        let (input, kill, mut ended) = {
            let mut running = self.running.lock().unwrap();
            let Some(r) = running.get_mut(name) else {
                return false;
            };
            r.stopping = true;
            (r.input.take(), r.kill.take(), r.ended.clone())
        };
        if let Some(mut input) = input {
            let _ = input.write_all(b"/quit\n").await;
            let _ = input.flush().await;
        }
        let quit = tokio::time::timeout(STOP_GRACE, ended.wait_for(|e| *e))
            .await
            .is_ok();
        if !quit {
            if let Some(kill) = kill {
                let _ = kill.send(());
            }
            let _ = tokio::time::timeout(Duration::from_secs(5), ended.wait_for(|e| *e)).await;
        }
        true
    }

    /// Starts the owner's workers that start with it. Returns each one's outcome.
    pub fn start_all(self: &Arc<Self>) -> Vec<(String, Result<()>)> {
        workers_of(&self.owner)
            .unwrap_or_default()
            .into_iter()
            .filter(|w| w.config.start_with_owner)
            .map(|w| {
                let started = self.start(&w.name);
                (w.name, started)
            })
            .collect()
    }

    /// Stops every worker this agent started (when the owner's agent stops).
    pub async fn stop_all(&self) {
        let names: Vec<String> = self.running.lock().unwrap().keys().cloned().collect();
        futures_util::future::join_all(names.iter().map(|n| self.stop(n))).await;
    }
}

/// Waits for `child` to end, ending it first when `kill` fires.
async fn wait(child: &mut Child, kill: oneshot::Receiver<()>) -> Option<ExitStatus> {
    let ended = tokio::select! {
        status = child.wait() => Some(status),
        Ok(()) = kill => None,
    };
    match ended {
        Some(status) => status.ok(),
        None => {
            let _ = child.kill().await;
            child.wait().await.ok()
        }
    }
}

/// The last `lines` lines of the log at `path` (at most 4000 characters).
fn log_tail(path: &Path, lines: usize) -> String {
    let text = std::fs::read(path)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    let all: Vec<&str> = text.lines().collect();
    let tail = all[all.len().saturating_sub(lines)..].join("\n");
    let chars: Vec<char> = tail.chars().collect();
    chars[chars.len().saturating_sub(4000)..].iter().collect()
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::SquadStore;
    use crate::profile::ProfileConfig;

    fn owner_in(root: &Path, config: ProfileConfig) -> Profile {
        let mut owner = Profile::open_in(root, "boss").unwrap();
        owner.config = config;
        owner.save().unwrap();
        owner
    }

    fn addr() -> EndpointAddr {
        EndpointAddr::new(iroh::SecretKey::generate().public())
    }

    #[test]
    fn workers_are_refused_what_they_could_not_run_with() {
        let root = tempfile::tempdir().unwrap();
        let owner = owner_in(root.path(), ProfileConfig::default());
        let spec = |name: &str| WorkerSpec {
            name: name.into(),
            ..WorkerSpec::default()
        };
        let refused = |spec: WorkerSpec, allowlist: &TrustStore| {
            create(&owner, addr(), allowlist, &spec)
                .err()
                .map(|e| e.to_string())
                .unwrap_or_default()
        };
        let none = TrustStore::default();
        assert!(refused(spec("../x"), &none).contains("letters, digits"));
        assert!(refused(spec("boss"), &none).contains("owner's name"));
        let mut allowlist = TrustStore::default();
        allowlist.allow(TrustedPeer {
            name: "alice".into(),
            id: iroh::SecretKey::generate().public(),
            addrs: Default::default(),
            can_delegate: false,
            tasks_per_hour: 1,
            access: Access::ReadOnly,
            shares: Vec::new(),
            m365: None,
            github: None,
            agents: Vec::new(),
        });
        assert!(refused(spec("alice"), &allowlist).contains("already on your allowlist"));
        let copilot = WorkerSpec {
            providers: vec![Provider::Copilot],
            ..spec("helper")
        };
        assert!(
            refused(copilot, &none).contains("isn't signed in to copilot"),
            "workers run on their owner's sign-ins"
        );
        let data = WorkerSpec {
            folders: vec![DATA_SHARE.into()],
            ..spec("helper")
        };
        assert!(refused(data, &none).contains("its own data folder"));
        let unknown = WorkerSpec {
            folders: vec!["repo".into()],
            ..spec("helper")
        };
        assert!(refused(unknown, &none).contains("no folder 'repo'"));
        let long = WorkerSpec {
            role: Some("x".repeat(MAX_ROLE_CHARS + 1)),
            ..spec("helper")
        };
        assert!(refused(long, &none).contains("at most"));
        assert!(
            !root.path().join("helper").exists(),
            "nothing is left behind"
        );

        // A digital worker can't own workers.
        let mut worker = Profile::open_in(root.path(), "aide").unwrap();
        worker.config = ProfileConfig {
            owner: Some("boss".into()),
            ..ProfileConfig::default()
        };
        worker.save().unwrap();
        assert!(
            create(&worker, addr(), &none, &spec("sub"))
                .unwrap_err()
                .to_string()
                .contains("only owners can create workers")
        );
        assert_eq!(
            workers_of(&owner)
                .unwrap()
                .iter()
                .map(|w| w.name.as_str())
                .collect::<Vec<_>>(),
            ["aide"]
        );
        assert!(worker_of(&owner, "boss").is_err(), "not a worker of boss");
    }

    #[test]
    fn a_profile_runs_in_one_agent_at_a_time() {
        let root = tempfile::tempdir().unwrap();
        let profile = Profile::open_in(root.path(), "solo").unwrap();
        assert!(!profile.is_running());
        let lock = profile.lock_instance().unwrap();
        assert!(profile.is_running());
        assert!(
            profile
                .lock_instance()
                .unwrap_err()
                .to_string()
                .contains("already running")
        );
        drop(lock);
        assert!(!profile.is_running());
        let _again = profile.lock_instance().unwrap();
    }

    #[test]
    fn leaving_squads_removes_those_left_empty() {
        let mut squads = SquadStore::default();
        squads
            .set("qa", vec!["helper".into(), "alice".into()])
            .unwrap();
        squads.set("solo", vec!["helper".into()]).unwrap();
        squads.set("dev", vec!["bob".into()]).unwrap();
        assert_eq!(squads.remove_member("helper"), ["qa", "solo"]);
        assert_eq!(squads.get("qa").unwrap().members, ["alice"]);
        assert!(squads.get("solo").is_none());
        assert_eq!(squads.get("dev").unwrap().members, ["bob"]);
        assert!(squads.remove_member("helper").is_empty());
    }

    #[test]
    fn the_last_lines_of_a_log_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("agent.log");
        let text: String = (1..=50).map(|n| format!("line {n}\n")).collect();
        std::fs::write(&log, text).unwrap();
        let tail = log_tail(&log, 3);
        assert_eq!(tail, "line 48\nline 49\nline 50");
        assert_eq!(log_tail(&dir.path().join("none"), 3), "");
    }
}
