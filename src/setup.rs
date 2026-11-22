//! Installing the Node bridge on machines without the source code, and `doctor`
//! checks of everything an agent needs.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::agent::{Agent, AgentConfig};
use crate::llm::Provider;
use crate::llm::copilot::find_copilot_cli;
use crate::profile::{Profile, data_root};

/// Bridge sources compiled into the binary; `setup` writes them out and runs `npm ci`.
const BRIDGE_FILES: &[(&str, &str)] = &[
    ("index.mjs", include_str!("../bridge/index.mjs")),
    ("package.json", include_str!("../bridge/package.json")),
    (
        "package-lock.json",
        include_str!("../bridge/package-lock.json"),
    ),
];

pub fn installed_bridge_dir() -> Result<PathBuf> {
    Ok(data_root()?.join("bridge"))
}

/// Bridge script to use: `explicit` (`--bridge` / `PROOFKIN_BRIDGE`), else the one
/// installed by `setup`, else a source checkout's `bridge/` (development builds).
pub fn resolve_bridge(explicit: Option<PathBuf>) -> PathBuf {
    if let Some(path) = explicit {
        return path;
    }
    let installed = installed_bridge_dir().map(|d| d.join("index.mjs"));
    if let Ok(path) = &installed
        && path.is_file()
    {
        return path.clone();
    }
    let dev = Path::new(env!("CARGO_MANIFEST_DIR")).join("bridge");
    if dev.join("node_modules").is_dir() {
        return dev.join("index.mjs");
    }
    installed.unwrap_or(dev.join("index.mjs"))
}

/// `npm` (a `.cmd` shim on Windows, which `Command` can't run directly).
fn npm() -> Command {
    if cfg!(windows) {
        let mut cmd = Command::new("cmd");
        cmd.args(["/C", "npm"]);
        cmd
    } else {
        Command::new("npm")
    }
}

/// Writes the bridge to `~/.proofkin/bridge` and installs the pinned Claude Agent
/// SDK and Codex SDK packages (`npm ci`). `registry` overrides the npm registry.
pub fn install_bridge(registry: Option<&str>) -> Result<PathBuf> {
    let dir = installed_bridge_dir()?;
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    for (name, contents) in BRIDGE_FILES {
        std::fs::write(dir.join(name), contents).with_context(|| format!("writing {name}"))?;
    }
    let mut cmd = npm();
    cmd.args(["ci", "--omit=dev", "--no-audit", "--no-fund"])
        .current_dir(&dir);
    if let Some(registry) = registry {
        cmd.args(["--registry", registry]);
    }
    let status = cmd
        .status()
        .context("running npm (is Node.js installed?)")?;
    if !status.success() {
        bail!(
            "npm ci failed in {} ({status}); if your network blocks registry.npmjs.org, pass --registry <your-mirror>",
            dir.display()
        );
    }
    Ok(dir.join("index.mjs"))
}

fn tool_version(program: &Path, arg: &str) -> Option<String> {
    let out = Command::new(program).arg(arg).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .next()
        .map(|l| l.trim().to_owned())
        .filter(|l| !l.is_empty())
}

fn on_path(name: &str) -> Option<PathBuf> {
    let exts: &[&str] = if cfg!(windows) {
        &[".exe", ".cmd"]
    } else {
        &[""]
    };
    std::env::var_os("PATH")
        .iter()
        .flat_map(std::env::split_paths)
        .flat_map(|dir| exts.iter().map(move |ext| dir.join(format!("{name}{ext}"))))
        .find(|p| p.is_file())
}

fn report(ok: bool, what: &str, detail: impl AsRef<str>) -> bool {
    println!(
        "  {} {what:<22} {}",
        if ok { "OK  " } else { "MISS" },
        detail.as_ref()
    );
    ok
}

/// Checks the machine for everything agents need; returns whether the core parts are ready.
pub async fn doctor(bridge: &Path) -> Result<bool> {
    println!(
        "proofkin {} (data: {})",
        env!("CARGO_PKG_VERSION"),
        data_root()?.display()
    );
    let mut ready = true;

    ready &= match find_copilot_cli() {
        Ok(cli) => {
            let version =
                tool_version(&cli, "--version").unwrap_or_else(|| cli.display().to_string());
            let version = version.trim_end_matches('.');
            // It updates itself: a version ahead of this build's protocol can't be used.
            match crate::llm::copilot::check_cli().await {
                Ok((_, None)) => report(true, "copilot CLI", version),
                Ok((_, Some(note))) => report(true, "copilot CLI", format!("{version}: {note}")),
                Err(e) => report(false, "copilot CLI", format!("{version}: {e:#}")),
            }
        }
        Err(_) => report(
            false,
            "copilot CLI",
            "install: winget install GitHub.Copilot  (or npm install -g @github/copilot)",
        ),
    };
    match on_path("gh").and_then(|gh| tool_version(&gh, "--version")) {
        Some(v) => report(true, "gh CLI (accounts)", v),
        None => report(
            false,
            "gh CLI (accounts)",
            "optional; easiest Copilot sign-in: winget install GitHub.cli, then gh auth login",
        ),
    };
    match on_path("node").and_then(|node| tool_version(&node, "--version")) {
        Some(v) => report(true, "node (claude/codex)", v),
        None => report(
            false,
            "node (claude/codex)",
            "optional; for claude/codex: install Node.js 18+, then proofkin setup",
        ),
    };
    let bridge_ok = bridge.is_file()
        && bridge
            .parent()
            .is_some_and(|d| d.join("node_modules").is_dir());
    report(
        bridge_ok,
        "bridge (claude/codex)",
        if bridge_ok {
            bridge.display().to_string()
        } else {
            "optional; run: proofkin setup".into()
        },
    );

    let mut config = AgentConfig::new("doctor", iroh::SecretKey::generate());
    config.bind = None;
    let relay = match Agent::bind(config).await {
        Ok((agent, _events)) => {
            let online = agent.online(Duration::from_secs(15)).await;
            let relay = agent.addr().relay_urls().next().map(|u| u.to_string());
            agent.shutdown().await;
            relay.filter(|_| online)
        }
        Err(_) => None,
    };
    ready &= match relay {
        Some(url) => report(true, "internet relay", format!("reachable ({url})")),
        None => report(
            false,
            "internet relay",
            "not reachable: allow outbound HTTPS to *.iroh.link (and UDP), or use --network local",
        ),
    };

    let profiles = Profile::list()?;
    println!(
        "  info secret store         {}",
        crate::profile::SecretStore::current().describe()
    );
    if profiles.is_empty() {
        println!("  no profiles yet; start your agent with: proofkin --name <you> --ui");
    }
    for name in profiles {
        let profile = Profile::open(&name)?;
        let signed: Vec<String> = [Provider::Copilot, Provider::Claude, Provider::Codex]
            .into_iter()
            .filter(|p| profile.is_configured(*p))
            .map(|p| p.to_string())
            .collect();
        let peers = profile.trust().map(|t| t.peers.len()).unwrap_or(0);
        println!(
            "  profile {name:<14} providers: [{}], allowlisted peers: {peers}",
            signed.join(", ")
        );
        println!(
            "  workiq  {name:<14} {}",
            crate::workiq::account::describe(&profile)
        );
    }
    println!(
        "{}",
        if ready {
            "Ready."
        } else {
            "Fix the MISS items marked above that you need."
        }
    );
    Ok(ready)
}
