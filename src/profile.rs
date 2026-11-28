//! Per-agent identities. Each profile has its own Copilot, Claude and Codex home
//! directories and its own credentials, so agents can be signed in to different
//! accounts and never fall back to the machine-wide sign-in.
//!
//! Layout: `<root>/profiles/<name>/{profile.json, copilot/, claude/, codex/}` where
//! `<root>` is `$PROOFKIN_HOME` or `~/.proofkin`. Secrets (GitHub tokens, Anthropic
//! keys) live in the OS credential store under service `proofkin`, user `<name>/<provider>`.
//!
//! Installs from before the rename keep working: `~/.p2pagents` stays the root while
//! there is no `~/.proofkin`, `P2PAGENTS_*` settings still apply when the `PROOFKIN_*`
//! one is unset, and secrets stored under service `p2pagents` are read (and copied
//! forward), so agent identities and their endpoint ids don't change.

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::llm::Provider;

/// Inherited credential/config variables that must not leak into an agent's
/// provider processes; each agent sets its own instead.
pub const INHERITED_CREDENTIAL_VARS: &[&str] = &[
    "COPILOT_GITHUB_TOKEN",
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "GH_ENTERPRISE_TOKEN",
    "GITHUB_ENTERPRISE_TOKEN",
    "COPILOT_HOME",
    "COPILOT_GH_HOST",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CLAUDE_CONFIG_DIR",
    "OPENAI_API_KEY",
    "CODEX_API_KEY",
    "CODEX_HOME",
];

/// Where profile secrets (identity key, tokens, API keys) are kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretStore {
    /// The OS credential store (Windows Credential Manager, macOS Keychain,
    /// Linux Secret Service).
    Keyring,
    /// Owner-only files in `<profile>/secrets/`: used when no OS credential store is
    /// available (headless Linux, WSL, containers) or `PROOFKIN_SECRET_STORE=file`.
    Files,
}

impl SecretStore {
    pub fn current() -> Self {
        match env_setting("SECRET_STORE").as_deref() {
            Some("file") => Self::Files,
            Some("keyring") => Self::Keyring,
            _ if keyring::Entry::store_status().is_ok() => Self::Keyring,
            _ => Self::Files,
        }
    }

    pub fn describe(self) -> &'static str {
        match self {
            Self::Keyring => "OS credential store",
            Self::Files => "owner-only files in each profile's secrets folder",
        }
    }
}

/// Writes `value` to `path`, readable only by the current user (0600 on Unix,
/// inheriting the user-profile ACL on Windows).
fn write_private_file(path: &Path, value: &str) -> Result<()> {
    let dir = path.parent().expect("secret files live in a folder");
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("writing {}", path.display()))?;
        file.write_all(value.as_bytes())?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, value).with_context(|| format!("writing {}", path.display()))
    }
}

/// Setting `PROOFKIN_<name>` from the environment, or `P2PAGENTS_<name>` (its name
/// before the rename) when that's unset. Empty values count as unset.
pub fn env_setting(name: &str) -> Option<String> {
    env_setting_os(name).map(|v| v.to_string_lossy().into_owned())
}

/// [`env_setting`] for values that may not be Unicode (paths).
pub fn env_setting_os(name: &str) -> Option<OsString> {
    [format!("PROOFKIN_{name}"), format!("P2PAGENTS_{name}")]
        .iter()
        .find_map(|var| std::env::var_os(var).filter(|v| !v.is_empty()))
}

/// OS credential store service names: the current one, and the one secrets were
/// stored under before the rename. Profiles under a custom `PROOFKIN_HOME` get
/// their own namespace so they never collide with the default profiles.
fn keyring_services() -> [String; 2] {
    match env_setting("HOME") {
        Some(root) => [format!("proofkin:{root}"), format!("p2pagents:{root}")],
        None => ["proofkin".to_owned(), "p2pagents".to_owned()],
    }
}
pub const DEFAULT_GITHUB_HOST: &str = "github.com";

/// Secret store key of the integration token (see [`Profile::integration_token`]).
const INTEGRATION_TOKEN: &str = "integration-token";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "kebab-case")]
pub enum CopilotAuth {
    /// Token of an account signed in to the GitHub CLI, fetched at startup.
    Gh { user: String, host: String },
    /// Token stored in the OS credential store.
    Token { host: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "kebab-case")]
pub enum ClaudeAuth {
    /// `claude auth login` inside this profile's Claude config dir.
    Login,
    /// Anthropic API key in the OS credential store.
    ApiKey,
    /// Long-lived OAuth token (`claude setup-token`) in the OS credential store.
    OauthToken,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "kebab-case")]
pub enum CodexAuth {
    /// `codex login` (ChatGPT or API key) inside this profile's Codex home.
    Login,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "kebab-case")]
pub enum GithubAuth {
    /// An account signed in to the GitHub CLI; its token is fetched when needed.
    Gh { user: String, host: String },
}

/// Secret store key of the Work IQ refresh token.
const WORKIQ_TOKEN: &str = "workiq";

/// How a profile reaches Work IQ.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum WorkIqConnection {
    /// Proofkin signs in to Entra ID itself, with the organization's own client app.
    Hosted { tenant: String, client_id: String },
    /// Microsoft's Work IQ CLI over stdio; it signs in by itself.
    Cli {
        /// The account the CLI signs in with (`workiq mcp --account`); none: the CLI's own
        /// default account.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account: Option<String>,
        /// Where the CLI is, as found when the profile signed in: an agent's PATH may not
        /// have it (npm keeps global commands in its own folder, which a Node version manager
        /// leaves off the PATH). None: looked for when it starts.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        program: Option<PathBuf>,
    },
}

/// A profile's Work IQ account. Nothing secret: the refresh token is in the secret store
/// (see [`Profile::workiq_refresh_token`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkIqAccount {
    pub connection: WorkIqConnection,
    /// The user's principal name (UPN).
    pub upn: String,
    pub display_name: String,
    pub tenant_id: String,
    /// When the account was added, in seconds since the Unix epoch.
    pub added: u64,
}

/// Most Work IQ calls a day, when the profile sets none.
pub const DEFAULT_WORKIQ_CALLS_PER_DAY: u32 = 200;
/// Most Work IQ calls in one run, when the profile sets none.
pub const DEFAULT_WORKIQ_CALLS_PER_RUN: u32 = 25;

/// A profile's Work IQ call budgets (see docs/work-iq.md, section 9). A limit that is not
/// set has its default.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkIqBudget {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calls_per_day: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calls_per_run: Option<u32>,
}

impl WorkIqBudget {
    pub fn per_day(&self) -> u32 {
        self.calls_per_day.unwrap_or(DEFAULT_WORKIQ_CALLS_PER_DAY)
    }

    pub fn per_run(&self) -> u32 {
        self.calls_per_run.unwrap_or(DEFAULT_WORKIQ_CALLS_PER_RUN)
    }
}

/// How long a change in a digital worker's task waits for its owner's sign-off, in minutes of
/// awake time, when the profile sets none.
pub const DEFAULT_WORKIQ_CHANGE_WAIT_MINUTES: u32 = 5;
/// The shortest wait for a change's sign-off, in minutes.
pub const MIN_WORKIQ_CHANGE_WAIT_MINUTES: u32 = 1;
/// The longest wait for a change's sign-off, in minutes.
pub const MAX_WORKIQ_CHANGE_WAIT_MINUTES: u32 = 10;

/// That a profile allows Microsoft 365 changes (see [`ProfileConfig::workiq_changes`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkIqChanges {
    /// How long a change in a digital worker's task for its owner waits for the owner's
    /// sign-off, in minutes (1 to 10; 5 when not set).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_minutes: Option<u32>,
}

impl WorkIqChanges {
    /// The wait for a change's sign-off in minutes, within 1 to 10.
    pub fn wait_minutes(&self) -> u32 {
        self.wait_minutes
            .unwrap_or(DEFAULT_WORKIQ_CHANGE_WAIT_MINUTES)
            .clamp(
                MIN_WORKIQ_CHANGE_WAIT_MINUTES,
                MAX_WORKIQ_CHANGE_WAIT_MINUTES,
            )
    }

    /// The wait for a change's sign-off.
    pub fn wait(&self) -> std::time::Duration {
        std::time::Duration::from_secs(u64::from(self.wait_minutes()) * 60)
    }

    /// The wait `text` gives, in minutes: `5m` or `5`, from 1 to 10.
    pub fn parse_wait(text: &str) -> std::result::Result<u32, String> {
        let minutes = text.trim();
        let minutes = minutes.strip_suffix('m').unwrap_or(minutes).trim();
        let range = MIN_WORKIQ_CHANGE_WAIT_MINUTES..=MAX_WORKIQ_CHANGE_WAIT_MINUTES;
        match minutes.parse::<u32>() {
            Ok(minutes) if range.contains(&minutes) => Ok(minutes),
            _ => Err(format!(
                "the wait for a change's sign-off is {MIN_WORKIQ_CHANGE_WAIT_MINUTES} to \
                 {MAX_WORKIQ_CHANGE_WAIT_MINUTES} minutes, like 5m, not {text:?}"
            )),
        }
    }
}

/// Where a profile's sign-off notices go (see [`ProfileConfig::workiq_notices`]): in
/// profile.json `{"to": "mail"}` or `{"to": "teams", "chat": "<id>"}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "to", rename_all = "lowercase")]
pub enum WorkIqNotices {
    /// Mail to the signed-in account's own address.
    Mail,
    /// A message in a Teams chat the account is a member of.
    Teams { chat: String },
}

impl WorkIqNotices {
    /// Where notices go, in words: `by mail to <address>` or `in Teams chat <id>`.
    pub fn describe(&self, address: &str) -> String {
        match self {
            WorkIqNotices::Mail => format!("by mail to {address}"),
            WorkIqNotices::Teams { chat } => format!("in Teams chat {chat}"),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copilot: Option<CopilotAuth>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude: Option<ClaudeAuth>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex: Option<CodexAuth>,
    /// GitHub account for repositories (clone, sync, push branches, open PRs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github: Option<GithubAuth>,
    /// Set for a digital worker: the profile whose accounts it runs on (see
    /// [`Profile::account_profile`]). It has no sign-ins of its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// What this agent is for (e.g. "test engineer"); shown to peers and given to
    /// the model with every task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// For a digital worker: the providers it runs when its owner's agent starts it
    /// (see [`crate::workers`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<Provider>,
    /// For a digital worker: its owner's agent starts it whenever it starts.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub start_with_owner: bool,
    /// Microsoft 365 work context through Work IQ (see docs/work-iq.md).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workiq: Option<WorkIqAccount>,
    /// For a digital worker: its owner turned on its use of the owner's Work IQ account.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub workiq_from_owner: bool,
    /// Limits on the Work IQ calls this profile makes. Kept apart from the account so they
    /// survive signing out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workiq_budget: Option<WorkIqBudget>,
    /// Set, this profile allows Microsoft 365 changes (docs/work-iq.md, section 7): a plan's
    /// `@workiq apply=STEP` step makes the changes step STEP proposed, once its owner signed
    /// them off, and a digital worker's task for its owner, in a folder whose Work IQ server
    /// allows changes, makes each change its owner signs off. Off by default
    /// (`proofkin workiq changes on|off`); the tenant's policy still applies on top.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workiq_changes: Option<WorkIqChanges>,
    /// Set, a sign-off that waits for this profile's owner is also told in Microsoft 365, by
    /// mail or in a Teams chat, with the profile's Work IQ account (docs/work-iq.md, section
    /// 7). Off by default (`proofkin workiq notices off|mail|teams`). A digital worker that
    /// uses its owner's Work IQ uses its owner's setting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workiq_notices: Option<WorkIqNotices>,
    /// Per provider: the model tasks that name none run on, and the models tasks may ask
    /// for (see [`crate::models`] and `proofkin models`).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub models: crate::models::Models,
    /// What this agent remembers when it serves tasks (see [`crate::factory::memory`]): the
    /// knowledge topics put in front of each, the topic the lessons of its verified work
    /// go to, and how many days they are kept.
    #[serde(
        default,
        skip_serializing_if = "crate::factory::memory::Memory::is_empty"
    )]
    pub memory: crate::factory::memory::Memory,
}

/// Held while an agent runs with a profile, so a profile runs in one agent at a
/// time (two would share one identity). Released when dropped or when the process
/// ends, however it ends.
#[derive(Debug)]
pub struct InstanceLock {
    _file: std::fs::File,
}

#[derive(Debug, Clone)]
pub struct Profile {
    pub name: String,
    pub dir: PathBuf,
    pub config: ProfileConfig,
}

/// Credentials for one agent's private Copilot CLI instance.
#[derive(Clone)]
pub struct CopilotCredentials {
    pub token: String,
    pub host: String,
    pub home: PathBuf,
}

impl fmt::Debug for CopilotCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CopilotCredentials")
            .field("token", &"<redacted>")
            .field("host", &self.host)
            .field("home", &self.home)
            .finish()
    }
}

/// Root folder for profiles and the installed bridge: `$PROOFKIN_HOME`, else
/// `~/.proofkin` (or `~/.p2pagents`, from before the rename, while that's all there is).
pub fn data_root() -> Result<PathBuf> {
    if let Some(root) = env_setting_os("HOME") {
        return Ok(PathBuf::from(root));
    }
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .context("cannot find the home directory; set PROOFKIN_HOME")?;
    Ok(current_or_legacy(
        Path::new(&home).join(".proofkin"),
        Path::new(&home).join(".p2pagents"),
    ))
}

/// `current`, unless only `legacy` (its name before the rename) exists: existing
/// installs keep their folders, and the absolute paths saved in them stay valid.
fn current_or_legacy(current: PathBuf, legacy: PathBuf) -> PathBuf {
    if !current.exists() && legacy.exists() {
        legacy
    } else {
        current
    }
}

pub fn profiles_root() -> Result<PathBuf> {
    Ok(data_root()?.join("profiles"))
}

/// Whether `name` can name a profile, peer or squad: letters, digits, '-', '_' and '.',
/// at most 64 characters, not starting with '.'.
pub fn name_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !name.starts_with('.')
}

pub fn validate_name(name: &str) -> Result<()> {
    if !name_ok(name) {
        bail!("invalid profile name {name:?}: use letters, digits, '-', '_' or '.'");
    }
    Ok(())
}

impl Profile {
    pub fn open(name: &str) -> Result<Self> {
        Self::open_in(&profiles_root()?, name)
    }

    pub fn open_in(root: &Path, name: &str) -> Result<Self> {
        validate_name(name)?;
        let dir = root.join(name);
        let file = dir.join("profile.json");
        let config = if file.is_file() {
            let text = std::fs::read_to_string(&file)
                .with_context(|| format!("reading {}", file.display()))?;
            serde_json::from_str(&text).with_context(|| format!("parsing {}", file.display()))?
        } else {
            ProfileConfig::default()
        };
        Ok(Self {
            name: name.to_owned(),
            dir,
            config,
        })
    }

    pub fn list() -> Result<Vec<String>> {
        Ok(Self::list_in(&profiles_root()?))
    }

    pub(crate) fn list_in(root: &Path) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(root) else {
            return Vec::new();
        };
        let mut names: Vec<String> = entries
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path().join("profile.json").is_file() || e.path().join("peers.json").is_file()
            })
            .filter_map(|e| e.file_name().into_string().ok())
            .collect();
        names.sort();
        names
    }

    pub fn save(&self) -> Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let file = self.dir.join("profile.json");
        std::fs::write(&file, serde_json::to_string_pretty(&self.config)? + "\n")
            .with_context(|| format!("writing {}", file.display()))
    }

    /// This profile's private config/state directory for `provider` (created on demand).
    pub fn home(&self, provider: Provider) -> Result<PathBuf> {
        let dir = self.dir.join(provider.to_string());
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        Ok(dir)
    }

    pub fn is_configured(&self, provider: Provider) -> bool {
        match provider {
            Provider::Copilot => self.config.copilot.is_some(),
            Provider::Claude => self.config.claude.is_some(),
            Provider::Codex => self.config.codex.is_some(),
        }
    }

    pub fn login_hint(&self, provider: Provider) -> String {
        format!("proofkin auth login {provider} --profile {}", self.name)
    }

    /// Digital workers can't be signed in: they always use their owner's accounts.
    pub fn check_can_sign_in(&self) -> Result<()> {
        match &self.config.owner {
            Some(owner) => bail!(
                "'{}' is a digital worker of '{owner}' and runs on its accounts; sign in '{owner}' instead",
                self.name
            ),
            None => Ok(()),
        }
    }

    pub fn not_signed_in(&self, provider: Provider) -> anyhow::Error {
        anyhow!(
            "{provider} is not signed in for profile '{}'; run: {}",
            self.name,
            self.login_hint(provider)
        )
    }

    fn secret_get(&self, key: &str) -> Result<Option<String>> {
        match SecretStore::current() {
            SecretStore::Keyring => {
                let [current, legacy] = keyring_services();
                if let Some(value) = self.keyring_get(&current, key)? {
                    return Ok(Some(value));
                }
                // Stored before the rename: copy it forward, so the profile keeps its
                // identity (endpoint id) and sign-ins. A failed copy is retried next time.
                let value = self.keyring_get(&legacy, key)?;
                if let Some(value) = &value {
                    let _ = self.keyring_entry(&current, key)?.set_password(value);
                }
                Ok(value)
            }
            SecretStore::Files => {
                let path = self.secret_file(key);
                match std::fs::read_to_string(&path) {
                    Ok(v) => Ok(Some(v.trim().to_owned())),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                    Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
                }
            }
        }
    }

    fn keyring_get(&self, service: &str, key: &str) -> Result<Option<String>> {
        match self.keyring_entry(service, key)?.get_password() {
            Ok(v) => Ok(Some(v)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(e).context("reading the OS credential store"),
        }
    }

    fn secret_set(&self, key: &str, value: &str) -> Result<()> {
        match SecretStore::current() {
            SecretStore::Keyring => self
                .keyring_entry(&keyring_services()[0], key)?
                .set_password(value)
                .context("saving to the OS credential store"),
            SecretStore::Files => write_private_file(&self.secret_file(key), value),
        }
    }

    fn secret_delete(&self, key: &str) -> Result<()> {
        match SecretStore::current() {
            SecretStore::Keyring => {
                // Legacy first: a read in between finds neither, instead of copying
                // the legacy secret forward again (see `secret_get`).
                for service in keyring_services().into_iter().rev() {
                    match self.keyring_entry(&service, key)?.delete_credential() {
                        Ok(()) | Err(keyring::Error::NoEntry) => {}
                        Err(e) => return Err(e).context("deleting from the OS credential store"),
                    }
                }
                Ok(())
            }
            SecretStore::Files => match std::fs::remove_file(self.secret_file(key)) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e).context("deleting a stored secret"),
            },
        }
    }

    fn keyring_entry(&self, service: &str, key: &str) -> Result<keyring::Entry> {
        keyring::Entry::new(service, &format!("{}/{key}", self.name))
            .context("opening the OS credential store")
    }

    fn secret_file(&self, key: &str) -> PathBuf {
        self.dir.join("secrets").join(key)
    }

    pub fn set_secret(&self, provider: Provider, secret: &str) -> Result<()> {
        self.secret_set(&provider.to_string(), secret)
    }

    pub fn secret(&self, provider: Provider) -> Result<String> {
        self.secret_get(&provider.to_string())?
            .ok_or_else(|| self.not_signed_in(provider))
    }

    pub fn delete_secret(&self, provider: Provider) -> Result<()> {
        self.secret_delete(&provider.to_string())
    }

    /// This profile's agent identity (ed25519 key), created on first use and
    /// kept in the secret store. Its public half is the endpoint id.
    pub fn identity(&self) -> Result<iroh::SecretKey> {
        match self.secret_get("identity")? {
            Some(hex) => hex.parse().map_err(|e| {
                anyhow!(
                    "stored identity for profile '{}' is invalid: {e}",
                    self.name
                )
            }),
            None => {
                let key = iroh::SecretKey::generate();
                self.secret_set("identity", &data_encoding::HEXLOWER.encode(&key.to_bytes()))
                    .context("saving the agent identity")?;
                Ok(key)
            }
        }
    }

    pub fn delete_identity(&self) -> Result<()> {
        self.secret_delete("identity")
    }

    /// Whether the profile exists: it has settings, an allowlist or an identity.
    pub fn exists(&self) -> Result<bool> {
        Ok(self.dir.join("profile.json").is_file()
            || self.dir.join("peers.json").is_file()
            || self.secret_get("identity")?.is_some())
    }

    /// The token other agents use to connect to this profile's agent over MCP and A2A
    /// (`proofkin connect`), if connecting is on.
    pub fn integration_token(&self) -> Result<Option<String>> {
        self.secret_get(INTEGRATION_TOKEN)
    }

    /// Turns connecting other agents on with a new token (the old one stops working).
    pub fn new_integration_token(&self) -> Result<String> {
        if self.config.owner.is_some() {
            bail!("a digital worker takes work from its owner only");
        }
        let bytes = iroh::SecretKey::generate().to_bytes();
        let token = format!("pfk_{}", data_encoding::HEXLOWER.encode(&bytes));
        self.secret_set(INTEGRATION_TOKEN, &token)
            .context("saving the integration token")?;
        Ok(token)
    }

    /// Turns connecting other agents off.
    pub fn delete_integration_token(&self) -> Result<()> {
        self.secret_delete(INTEGRATION_TOKEN)
    }

    pub fn workiq_login_hint(&self) -> String {
        format!("proofkin auth login workiq --profile {}", self.name)
    }

    /// The profile whose Work IQ account this profile uses: itself, or for a digital worker
    /// its owner, when the owner turned that on for it.
    pub fn workiq_profile(&self) -> Result<Profile> {
        let Some(owner) = &self.config.owner else {
            return Ok(self.clone());
        };
        if !self.config.workiq_from_owner {
            bail!(
                "digital worker '{}' may not use '{owner}''s Work IQ account; its owner turns that \
                 on when creating it: proofkin worker create {} --owner {owner} --workiq",
                self.name,
                self.name
            );
        }
        self.account_profile()
    }

    /// The Work IQ account this profile uses (see [`Profile::workiq_profile`]), with the
    /// profile that holds it and its token.
    pub fn workiq_account(&self) -> Result<(Profile, WorkIqAccount)> {
        let holder = self.workiq_profile()?;
        let account = holder.config.workiq.clone().ok_or_else(|| {
            anyhow!(
                "Work IQ is not signed in for profile '{}'; run: {}",
                holder.name,
                holder.workiq_login_hint()
            )
        })?;
        Ok((holder, account))
    }

    /// The Work IQ refresh token of this profile's own account.
    pub fn workiq_refresh_token(&self) -> Result<String> {
        self.secret_get(WORKIQ_TOKEN)?.ok_or_else(|| {
            anyhow!(
                "the Work IQ token of profile '{}' is missing; run: {}",
                self.name,
                self.workiq_login_hint()
            )
        })
    }

    pub fn set_workiq_refresh_token(&self, token: &str) -> Result<()> {
        self.secret_set(WORKIQ_TOKEN, token)
            .context("saving the Work IQ token")
    }

    pub fn delete_workiq_token(&self) -> Result<()> {
        self.secret_delete(WORKIQ_TOKEN)
    }

    /// Where the running agent's web UI listens (other agents connect there).
    pub fn ui_port_path(&self) -> PathBuf {
        self.dir.join("ui-port")
    }

    /// Allowlist of peers this profile's agents talk to.
    pub fn trust_path(&self) -> PathBuf {
        self.dir.join("peers.json")
    }

    /// Log of tasks peers ran on this profile's agents (see [`crate::audit`]).
    pub fn audit_path(&self) -> PathBuf {
        self.dir.join("audit.jsonl")
    }

    /// Folders this profile shares with peers, besides the built-in data folder.
    pub fn shares_path(&self) -> PathBuf {
        self.dir.join("shares.json")
    }

    /// Squads this profile groups its peers into (see [`crate::plan`]).
    pub fn squads_path(&self) -> PathBuf {
        self.dir.join("squads.json")
    }

    /// Saved evidence runs (plan runs with their reports).
    pub fn runs_dir(&self) -> PathBuf {
        self.dir.join("runs")
    }

    /// The web UI's latest tasks, kept across restarts.
    pub fn tasks_path(&self) -> PathBuf {
        self.dir.join("tasks.json")
    }

    /// The web UI's plan runs in progress, one file each, so they go on after a restart.
    pub fn runs_in_progress_dir(&self) -> PathBuf {
        self.dir.join("runs-in-progress")
    }

    /// Where the next id for the tasks this agent gives peers is kept (see
    /// [`crate::agent::AgentConfig::task_ids`]).
    pub fn task_ids_path(&self) -> PathBuf {
        self.dir.join("next-task-id")
    }

    /// The knowledge base (private to this profile).
    pub fn knowledge(&self) -> crate::knowledge::KnowledgeBase {
        crate::knowledge::KnowledgeBase::new(self.dir.join("knowledge"))
    }

    /// Built-in skills, this profile's own (`skills/`) and those of its enabled plugins, with
    /// its agents: its own, its agent factory's released ones and its plugins'.
    pub fn skills(&self) -> crate::skills::Skills {
        crate::skills::Skills::new(self.dir.join("skills"))
            .with_plugins(self.plugins().skills())
            .with_plugin_agents(self.plugins().agents())
            .with_factory(crate::factory::FactoryStore::of(self))
    }

    /// Plugins imported into this profile (see [`crate::plugins`]).
    pub fn plugins(&self) -> crate::plugins::PluginStore {
        crate::plugins::PluginStore::new(&self.dir)
    }

    /// Metrics and incident logs.
    pub fn ops_store(&self) -> crate::ops::OpsStore {
        crate::ops::OpsStore::new(self.dir.join("ops"))
    }

    pub fn monitors_path(&self) -> PathBuf {
        self.dir.join("monitors.json")
    }

    /// Plans this profile's agent runs on its own at set times (see [`crate::schedule`]).
    pub fn schedules_path(&self) -> PathBuf {
        self.dir.join("schedules.json")
    }

    /// Where this profile's linked repositories are cloned: next to the data folder,
    /// outside the profiles folder, like task worktrees.
    pub fn repos_dir(&self) -> PathBuf {
        self.outside_folder("repos").join(&self.name)
    }

    /// The profile as saved now (another process or the web UI may have changed it).
    pub fn reload(&self) -> Result<Profile> {
        let root = self.dir.parent().context("profile folder has no parent")?;
        Profile::open_in(root, &self.name)
    }

    /// Uses GitHub CLI account `user@host` for this profile's repositories, after
    /// checking GitHub accepts its token; returns the login.
    pub fn github_login(&mut self, user: &str, host: &str) -> Result<String> {
        if self.config.owner.is_some() {
            bail!("digital workers use their owner's GitHub account");
        }
        let token = gh_token(user, host)?;
        let login = github_login(&token, host)
            .with_context(|| format!("GitHub didn't accept {user}'s token on {host}"))?;
        self.config.github = Some(GithubAuth::Gh {
            user: user.to_owned(),
            host: host.to_owned(),
        });
        self.save()?;
        Ok(login)
    }

    pub fn repos_path(&self) -> PathBuf {
        self.dir.join("repos.json")
    }

    /// Where this profile tracks its plan runs: an issue repository and a GitHub
    /// Project (see [`crate::tracker`]).
    pub fn tracker_path(&self) -> PathBuf {
        self.dir.join("tracker.json")
    }

    pub fn tasks_tracker_path(&self) -> PathBuf {
        self.dir.join("tracker-tasks.json")
    }

    /// The GitHub account for repositories (a digital worker uses its owner's).
    pub fn github(&self) -> Result<crate::github::GithubCredentials> {
        if self.config.owner.is_some() {
            return self.account_profile()?.github();
        }
        match &self.config.github {
            Some(GithubAuth::Gh { user, host }) => Ok(crate::github::GithubCredentials {
                user: user.clone(),
                host: host.clone(),
                token: gh_token(user, host)?,
            }),
            None => bail!(
                "profile '{}' is not signed in to GitHub; run: proofkin github login --gh-user <login> --profile {}",
                self.name,
                self.name
            ),
        }
    }

    /// The login of [`Profile::github`]'s account, without fetching its token. `None`:
    /// the profile isn't signed in to GitHub.
    pub fn github_user(&self) -> Option<String> {
        if self.config.owner.is_some() {
            return self.account_profile().ok()?.github_user();
        }
        self.config
            .github
            .as_ref()
            .map(|GithubAuth::Gh { user, .. }| user.clone())
    }

    /// Where per-task git worktrees are created: outside the profiles folder (like
    /// the data folder), since providers deny tasks access to profiles.
    pub fn worktrees_dir(&self) -> PathBuf {
        self.outside_folder("worktrees").join(&self.name)
    }

    /// The profile's data folder: the built-in read-only `data` share.
    /// `~/.proofkin-data/<profile>`, or `$PROOFKIN_HOME/.proofkin-data/<profile>`.
    pub fn data_folder(&self) -> PathBuf {
        self.outside_folder("data").join(&self.name)
    }

    /// Folder `.proofkin-<kind>` next to the profiles folder, or `.p2p<kind>` where an
    /// install from before the rename has one (and no new one).
    fn outside_folder(&self, kind: &str) -> PathBuf {
        let base = self.outside_base();
        current_or_legacy(
            base.join(format!(".proofkin-{kind}")),
            base.join(format!(".p2p{kind}")),
        )
    }

    /// Next to the profiles folder, never inside it: `~/.proofkin` -> `~`, so peers'
    /// tasks can work there while the credential-holding profiles stay denied.
    fn outside_base(&self) -> &Path {
        let root = self
            .dir
            .parent()
            .and_then(Path::parent)
            .unwrap_or(&self.dir);
        // ~/.proofkin -> ~/.proofkin-data; a custom PROOFKIN_HOME keeps it inside.
        if root
            .file_name()
            .is_some_and(|n| n == ".proofkin" || n == ".p2pagents")
        {
            root.parent().unwrap_or(root)
        } else {
            root
        }
    }

    /// The folders this profile shares (always including `data`).
    pub fn shares(&self) -> Result<crate::shares::ShareStore> {
        crate::shares::ShareStore::load(&self.shares_path(), self.data_folder())
    }

    /// Changes this profile's shares under their lock (see
    /// [`crate::shares::ShareStore::update`]); returns `change`'s result and the saved shares.
    pub fn update_shares<T>(
        &self,
        change: impl FnOnce(&mut crate::shares::ShareStore) -> Result<T>,
    ) -> Result<(T, crate::shares::ShareStore)> {
        crate::shares::ShareStore::update(&self.shares_path(), self.data_folder(), change)
    }

    /// Deletes the profile: its identity, stored secrets and folder (including
    /// the provider homes and their sign-ins).
    pub fn delete(self) -> Result<()> {
        for provider in [Provider::Copilot, Provider::Claude, Provider::Codex] {
            self.delete_secret(provider)?;
        }
        self.delete_integration_token()?;
        self.delete_workiq_token()?;
        self.delete_identity()?;
        if self.dir.exists() {
            std::fs::remove_dir_all(&self.dir)
                .with_context(|| format!("deleting {}", self.dir.display()))?;
        }
        Ok(())
    }

    pub fn trust(&self) -> Result<crate::trust::TrustStore> {
        crate::trust::TrustStore::load(&self.trust_path())
    }

    fn instance_lock_path(&self) -> PathBuf {
        self.dir.join("agent.lock")
    }

    /// Takes this profile's [`InstanceLock`]; fails while an agent runs with it.
    pub fn lock_instance(&self) -> Result<InstanceLock> {
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("creating {}", self.dir.display()))?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.instance_lock_path())
            .with_context(|| format!("opening {}", self.instance_lock_path().display()))?;
        match file.try_lock() {
            Ok(()) => Ok(InstanceLock { _file: file }),
            Err(std::fs::TryLockError::WouldBlock) => bail!(
                "profile '{}' is already running in another proofkin agent",
                self.name
            ),
            Err(std::fs::TryLockError::Error(e)) => {
                Err(e).with_context(|| format!("locking profile '{}'", self.name))
            }
        }
    }

    /// Whether an agent runs with this profile now, in any process.
    pub fn is_running(&self) -> bool {
        std::fs::File::open(self.instance_lock_path())
            .is_ok_and(|file| matches!(file.try_lock(), Err(std::fs::TryLockError::WouldBlock)))
    }

    /// The profile whose accounts this profile's tasks run on: itself, or for a
    /// digital worker its owner (which must exist and can't be a digital worker).
    pub fn account_profile(&self) -> Result<Profile> {
        let Some(owner) = &self.config.owner else {
            return Ok(self.clone());
        };
        let root = self.dir.parent().context("profile has no parent folder")?;
        let account = Profile::open_in(root, owner)?;
        if !account.dir.join("profile.json").is_file() {
            bail!(
                "digital worker '{}' belongs to profile '{owner}', which does not exist",
                self.name
            );
        }
        if account.config.owner.is_some() {
            bail!(
                "digital worker '{}' belongs to '{owner}', which is itself a digital worker",
                self.name
            );
        }
        Ok(account)
    }

    /// The models this profile's providers run (see [`crate::models`]): its own
    /// settings, and for a digital worker, its owner's for each provider it sets
    /// nothing for (it runs on the owner's accounts, so on the owner's choice too).
    pub fn models(&self) -> crate::models::Models {
        let mut models = self.config.models.clone();
        if self.config.owner.is_some()
            && let Ok(owner) = self.account_profile()
        {
            for (provider, choice) in owner.config.models {
                models.entry(provider).or_insert(choice);
            }
        }
        models
    }

    /// The providers whose models this profile takes from its owner (a digital worker
    /// that sets nothing for them).
    pub fn inherited_models(&self) -> Vec<crate::llm::Provider> {
        if self.config.owner.is_none() {
            return Vec::new();
        }
        let own = &self.config.models;
        self.models()
            .into_keys()
            .filter(|p| !own.contains_key(p))
            .collect()
    }

    /// Creates digital worker `name` owned by this profile: its own identity,
    /// allowlist, folders and audit log, but this profile's accounts.
    pub fn create_digital_worker(&self, name: &str, role: Option<String>) -> Result<Profile> {
        if self.config.owner.is_some() {
            bail!(
                "'{}' is a digital worker; only owners can create workers",
                self.name
            );
        }
        if !self.dir.join("profile.json").is_file() {
            bail!(
                "profile '{}' has no sign-ins yet; sign it in first (proofkin auth login copilot --profile {})",
                self.name,
                self.name
            );
        }
        let root = self.dir.parent().context("profile has no parent folder")?;
        let mut worker = Profile::open_in(root, name)?;
        if worker.dir.exists() {
            bail!("profile '{name}' already exists");
        }
        worker.config = ProfileConfig {
            owner: Some(self.name.clone()),
            role,
            ..ProfileConfig::default()
        };
        worker.save()?;
        Ok(worker)
    }

    pub fn copilot_credentials(&self) -> Result<CopilotCredentials> {
        if self.config.owner.is_some() {
            // The owner's sign-in, in the worker's own Copilot home (its own sessions).
            let mut creds = self.account_profile()?.copilot_credentials()?;
            creds.home = self.home(Provider::Copilot)?;
            return Ok(creds);
        }
        let (token, host) = match &self.config.copilot {
            None => return Err(self.not_signed_in(Provider::Copilot)),
            Some(CopilotAuth::Gh { user, host }) => (gh_token(user, host)?, host.clone()),
            Some(CopilotAuth::Token { host }) => (self.secret(Provider::Copilot)?, host.clone()),
        };
        Ok(CopilotCredentials {
            token,
            host,
            home: self.home(Provider::Copilot)?,
        })
    }

    /// Environment for this profile's Claude/Codex processes (the Node bridge):
    /// isolated config homes plus this profile's own secrets.
    pub fn bridge_env(&self, providers: &[Provider]) -> Result<Vec<(String, OsString)>> {
        if self.config.owner.is_some() {
            // Claude/Codex sign-ins live in the owner's config homes.
            return self.account_profile()?.bridge_env(providers);
        }
        let mut env = Vec::new();
        if providers.contains(&Provider::Claude) {
            env.push((
                "CLAUDE_CONFIG_DIR".into(),
                self.home(Provider::Claude)?.into_os_string(),
            ));
            match self.config.claude {
                None => return Err(self.not_signed_in(Provider::Claude)),
                Some(ClaudeAuth::Login) => {}
                Some(ClaudeAuth::ApiKey) => env.push((
                    "ANTHROPIC_API_KEY".into(),
                    self.secret(Provider::Claude)?.into(),
                )),
                Some(ClaudeAuth::OauthToken) => env.push((
                    "CLAUDE_CODE_OAUTH_TOKEN".into(),
                    self.secret(Provider::Claude)?.into(),
                )),
            }
        }
        if providers.contains(&Provider::Codex) {
            if self.config.codex.is_none() {
                return Err(self.not_signed_in(Provider::Codex));
            }
            env.push((
                "CODEX_HOME".into(),
                self.home(Provider::Codex)?.into_os_string(),
            ));
        }
        Ok(env)
    }
}

/// GitHub login that `token` belongs to, looked up with `curl` (token passed on
/// stdin, never on the command line). `None` if curl is unavailable or the call fails.
pub fn github_login(token: &str, host: &str) -> Option<String> {
    use std::io::Write;
    let api = if host == DEFAULT_GITHUB_HOST {
        "https://api.github.com/user".to_owned()
    } else {
        format!("https://api.{host}/user")
    };
    let mut child = Command::new(if cfg!(windows) { "curl.exe" } else { "curl" })
        .args(["-sf", "-H", "@-", "-H", "User-Agent: proofkin", &api])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    child
        .stdin
        .take()?
        .write_all(format!("Authorization: Bearer {token}\n").as_bytes())
        .ok()?;
    let out = child.wait_with_output().ok()?;
    let user: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    user.get("login")?.as_str().map(str::to_owned)
}

/// Token for `user@host` from the GitHub CLI (supports several signed-in accounts).
pub fn gh_token(user: &str, host: &str) -> Result<String> {
    let mut cmd = Command::new(crate::github::gh_program());
    cmd.args(["auth", "token", "--user", user, "--hostname", host]);
    for var in INHERITED_CREDENTIAL_VARS {
        cmd.env_remove(var);
    }
    let out = cmd
        .output()
        .context("running `gh auth token` (is the GitHub CLI installed?)")?;
    let token = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if !out.status.success() || token.is_empty() {
        bail!(
            "no GitHub CLI token for {user} on {host} ({}); sign that account in with: gh auth login --hostname {host}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_path_like_names() {
        for bad in ["", "..", "../x", "a/b", "a\\b", ".hidden", "a b"] {
            assert!(validate_name(bad).is_err(), "{bad:?}");
        }
        validate_name("worker-1_a.b").unwrap();
    }

    #[test]
    fn installs_from_before_the_rename_keep_their_folders() {
        let home = tempfile::tempdir().unwrap();
        let (new_root, old_root) = (
            home.path().join(".proofkin"),
            home.path().join(".p2pagents"),
        );
        assert_eq!(
            current_or_legacy(new_root.clone(), old_root.clone()),
            new_root
        );

        std::fs::create_dir_all(old_root.join("profiles")).unwrap();
        std::fs::create_dir_all(home.path().join(".p2pdata")).unwrap();
        assert_eq!(
            current_or_legacy(new_root.clone(), old_root.clone()),
            old_root
        );
        let boss = Profile::open_in(&old_root.join("profiles"), "boss").unwrap();
        // Existing outside folders keep their old names; new ones get the new name.
        assert_eq!(
            boss.data_folder(),
            home.path().join(".p2pdata").join("boss")
        );
        assert_eq!(
            boss.worktrees_dir(),
            home.path().join(".proofkin-worktrees").join("boss")
        );
        assert_eq!(
            boss.repos_dir(),
            home.path().join(".proofkin-repos").join("boss")
        );

        std::fs::create_dir_all(&new_root).unwrap();
        assert_eq!(current_or_legacy(new_root.clone(), old_root), new_root);

        // A custom home keeps everything inside it.
        let custom = tempfile::tempdir().unwrap();
        let p = Profile::open_in(&custom.path().join("profiles"), "w").unwrap();
        assert_eq!(
            p.data_folder(),
            custom.path().join(".proofkin-data").join("w")
        );
    }

    #[test]
    fn config_round_trips_and_homes_are_per_profile() {
        let root = tempfile::tempdir().unwrap();
        let mut a = Profile::open_in(root.path(), "a").unwrap();
        assert!(!a.is_configured(Provider::Copilot));
        a.config.copilot = Some(CopilotAuth::Gh {
            user: "octo".into(),
            host: "github.com".into(),
        });
        a.config.codex = Some(CodexAuth::Login);
        a.save().unwrap();

        let json = std::fs::read_to_string(root.path().join("a/profile.json")).unwrap();
        assert!(json.contains(r#""source": "gh""#), "{json}");
        let a = Profile::open_in(root.path(), "a").unwrap();
        assert_eq!(
            a.config.copilot,
            Some(CopilotAuth::Gh {
                user: "octo".into(),
                host: "github.com".into()
            })
        );

        let b = Profile::open_in(root.path(), "b").unwrap();
        assert_ne!(
            a.home(Provider::Codex).unwrap(),
            b.home(Provider::Codex).unwrap()
        );
        assert!(root.path().join("a/codex").is_dir());
    }

    #[test]
    fn unconfigured_providers_fail_with_login_hint() {
        let root = tempfile::tempdir().unwrap();
        let p = Profile::open_in(root.path(), "w").unwrap();
        let err = p.copilot_credentials().unwrap_err().to_string();
        assert!(
            err.contains("proofkin auth login copilot --profile w"),
            "{err}"
        );
        let err = p.bridge_env(&[Provider::Codex]).unwrap_err().to_string();
        assert!(err.contains("auth login codex --profile w"), "{err}");
    }

    #[test]
    fn bridge_env_points_each_provider_at_the_profile_home() {
        let root = tempfile::tempdir().unwrap();
        let mut p = Profile::open_in(root.path(), "w").unwrap();
        p.config.claude = Some(ClaudeAuth::Login);
        p.config.codex = Some(CodexAuth::Login);
        let env = p.bridge_env(&[Provider::Claude, Provider::Codex]).unwrap();
        let get = |k: &str| {
            env.iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| PathBuf::from(v))
        };
        assert_eq!(get("CLAUDE_CONFIG_DIR"), Some(root.path().join("w/claude")));
        assert_eq!(get("CODEX_HOME"), Some(root.path().join("w/codex")));
        assert!(get("ANTHROPIC_API_KEY").is_none());
    }

    /// A digital worker runs on its owner's model settings for each provider it sets
    /// nothing for, and on its own where it does; its owner's are read as they are now.
    #[test]
    fn a_digital_worker_runs_on_its_owners_models_unless_it_sets_its_own() {
        let root = tempfile::tempdir().unwrap();
        let mut alice = Profile::open_in(root.path(), "alice").unwrap();
        crate::models::set_default(&mut alice.config.models, Provider::Copilot, Some("opus"))
            .unwrap();
        crate::models::set_effort(&mut alice.config.models, Provider::Copilot, Some("max"))
            .unwrap();
        crate::models::set_default(&mut alice.config.models, Provider::Claude, Some("sonnet"))
            .unwrap();
        alice.save().unwrap();
        alice.create_digital_worker("helper", None).unwrap();
        let mut helper = Profile::open_in(root.path(), "helper").unwrap();
        let models = helper.models();
        assert_eq!(models[&Provider::Copilot].default.as_deref(), Some("opus"));
        assert_eq!(models[&Provider::Copilot].effort.as_deref(), Some("max"));
        assert_eq!(
            helper.inherited_models(),
            [Provider::Copilot, Provider::Claude]
        );
        // Its own setting for a provider wins, whole; the others stay its owner's.
        crate::models::set_effort(&mut helper.config.models, Provider::Claude, Some("low"))
            .unwrap();
        helper.save().unwrap();
        let models = helper.models();
        assert_eq!(models[&Provider::Claude].effort.as_deref(), Some("low"));
        assert_eq!(models[&Provider::Claude].default, None);
        assert_eq!(helper.inherited_models(), [Provider::Copilot]);
        // The owner's own models are its own: it inherits nothing.
        assert!(alice.inherited_models().is_empty());
        assert_eq!(alice.models(), alice.config.models);
    }

    #[test]
    fn digital_worker_uses_its_owners_accounts() {
        let root = tempfile::tempdir().unwrap();
        let mut alice = Profile::open_in(root.path(), "alice").unwrap();
        alice.config.claude = Some(ClaudeAuth::Login);
        alice.config.codex = Some(CodexAuth::Login);
        alice.save().unwrap();
        alice
            .create_digital_worker("helper", Some("test engineer".into()))
            .unwrap();
        let helper = Profile::open_in(root.path(), "helper").unwrap();
        assert_eq!(helper.config.owner.as_deref(), Some("alice"));
        assert_eq!(helper.config.role.as_deref(), Some("test engineer"));
        assert!(helper.config.claude.is_none(), "no sign-ins of its own");
        assert_eq!(helper.account_profile().unwrap().name, "alice");

        // Claude/Codex run in the owner's signed-in homes.
        let env = helper
            .bridge_env(&[Provider::Claude, Provider::Codex])
            .unwrap();
        let get = |k: &str| {
            env.iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| PathBuf::from(v))
        };
        assert_eq!(
            get("CLAUDE_CONFIG_DIR"),
            Some(root.path().join("alice/claude"))
        );
        assert_eq!(get("CODEX_HOME"), Some(root.path().join("alice/codex")));
        // Not signed in to Copilot: the hint names the owner, the account that matters.
        let err = helper.copilot_credentials().unwrap_err().to_string();
        assert!(err.contains("--profile alice"), "{err}");
        // Its own identity, folders and data stay separate from the owner's.
        assert_ne!(helper.data_folder(), alice.data_folder());
        assert_ne!(helper.trust_path(), alice.trust_path());
    }

    #[test]
    fn digital_workers_cannot_sign_in_or_chain() {
        let root = tempfile::tempdir().unwrap();
        let mut alice = Profile::open_in(root.path(), "alice").unwrap();
        alice.config.codex = Some(CodexAuth::Login);
        alice.save().unwrap();
        let helper = alice.create_digital_worker("helper", None).unwrap();
        assert!(
            helper.create_digital_worker("sub", None).is_err(),
            "no chains"
        );
        assert!(
            alice.create_digital_worker("helper", None).is_err(),
            "exists"
        );
        assert!(alice.create_digital_worker("../x", None).is_err());
        let fresh = Profile::open_in(root.path(), "fresh").unwrap();
        assert!(
            fresh.create_digital_worker("w", None).is_err(),
            "owner must exist"
        );
        assert!(helper.check_can_sign_in().is_err());
        assert!(alice.check_can_sign_in().is_ok());

        // An owner that became a digital worker can't lend its accounts.
        let mut alice = Profile::open_in(root.path(), "alice").unwrap();
        alice.config.owner = Some("someone".into());
        alice.save().unwrap();
        assert!(helper.account_profile().is_err());
    }

    /// `workiq changes on --wait` takes 1 to 10 minutes, as `5m` or `5`; a profile that sets
    /// none waits 5, and one whose file says more than 10 waits 10.
    #[test]
    fn a_changes_sign_off_waits_1_to_10_minutes_given_as_5m_or_5() {
        assert_eq!(WorkIqChanges::parse_wait("5m"), Ok(5));
        assert_eq!(WorkIqChanges::parse_wait(" 10 "), Ok(10));
        assert_eq!(WorkIqChanges::parse_wait("1m"), Ok(1));
        for bad in ["0", "11m", "5h", "m", "", "-1", "5 minutes"] {
            let error = WorkIqChanges::parse_wait(bad).unwrap_err();
            assert!(error.contains("1 to 10 minutes"), "{bad:?}: {error}");
        }
        let unset = WorkIqChanges::default();
        assert_eq!(unset.wait(), std::time::Duration::from_secs(300));
        let edited = WorkIqChanges {
            wait_minutes: Some(60),
        };
        assert_eq!(edited.wait_minutes(), 10);
        let config: ProfileConfig = serde_json::from_str(r#"{"workiq_changes":{}}"#).unwrap();
        assert_eq!(config.workiq_changes, Some(WorkIqChanges::default()));
        let set = serde_json::to_value(WorkIqChanges {
            wait_minutes: Some(3),
        })
        .unwrap();
        assert_eq!(set, serde_json::json!({ "wait_minutes": 3 }));
    }
}
