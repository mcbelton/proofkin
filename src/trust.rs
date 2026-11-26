//! Who an agent will talk to. Every peer is identified by its public key
//! (iroh `EndpointId`, authenticated by the QUIC/TLS handshake); connections from
//! keys that are not on the profile's allowlist are dropped, and only peers marked
//! `can_delegate` may hand this agent LLM tasks.

use std::collections::BTreeSet;
use std::fmt;
use std::path::Path;
use std::str::FromStr;

use anyhow::{Context, Result, anyhow, bail};
use iroh::{EndpointAddr, EndpointId, TransportAddr};
use serde::{Deserialize, Serialize};

use crate::llm::Access;
use crate::workiq::content::Audience;

pub const DEFAULT_TASKS_PER_HOUR: u32 = 20;

fn default_tasks_per_hour() -> u32 {
    DEFAULT_TASKS_PER_HOUR
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustedPeer {
    /// Local alias for the peer (used in `/delegate <provider>@<name>`).
    pub name: String,
    pub id: EndpointId,
    /// Last known addresses (relay URLs / IPs). Optional on the internet, where
    /// the id alone is resolved through address lookup.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub addrs: BTreeSet<TransportAddr>,
    /// Whether this peer may delegate LLM tasks to us (spending our accounts).
    #[serde(default)]
    pub can_delegate: bool,
    #[serde(default = "default_tasks_per_hour")]
    pub tasks_per_hour: u32,
    /// Most a delegated task from this peer may do; capped by each shared folder's maximum.
    #[serde(default)]
    pub access: Access,
    /// Names of the folders shared with this peer (see [`crate::shares`]).
    #[serde(default = "default_shares")]
    pub shares: Vec<String>,
    /// What this peer may have of this agent's Microsoft 365 data. Absent: nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub m365: Option<M365Grant>,
    /// The peer's GitHub account, as this agent's owner knows it: pull request steps ask it
    /// to review (see [`crate::pulls`]). Absent: not known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github: Option<String>,
    /// The factory agents this peer may see in its catalog and import, as their owner
    /// publishes them (see [`crate::factory::catalog`]). Empty: none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agents: Vec<String>,
}

/// A peer's grant to Microsoft 365 (Work IQ) content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct M365Grant {
    /// Sensitivity label names the peer may receive; `unlabeled` allows content
    /// without a label.
    #[serde(default)]
    pub labels: Vec<String>,
    /// Its tasks may use this agent's Work IQ gateway (read only).
    #[serde(default)]
    pub tools: bool,
}

impl M365Grant {
    /// A grant of the sensitivity labels named in `names` (`unlabeled` allows content without
    /// a label) and, with `tools`, of this agent's Work IQ. It takes label names only: at
    /// least one, never `unknown` or `off`, each kept once whatever its case (blank names are
    /// left out). What it refuses, [`M365GrantError`] says without the CLI's flags.
    pub fn from_labels(
        names: impl IntoIterator<Item = impl AsRef<str>>,
        tools: bool,
    ) -> Result<Self, M365GrantError> {
        let mut labels: Vec<String> = Vec::new();
        for name in names {
            let name = name.as_ref().trim();
            if name.eq_ignore_ascii_case(crate::workiq::content::UNKNOWN) {
                return Err(M365GrantError::Unknown(name.to_owned()));
            }
            if name.eq_ignore_ascii_case("off") {
                return Err(M365GrantError::Off(name.to_owned()));
            }
            if !name.is_empty() && !labels.iter().any(|l| l.eq_ignore_ascii_case(name)) {
                labels.push(name.to_owned());
            }
        }
        if labels.is_empty() {
            return Err(M365GrantError::NoLabels);
        }
        Ok(Self { labels, tools })
    }

    /// Reads `--m365`: comma-separated label names, as [`Self::from_labels`] takes them, or
    /// `off` on its own for no grant. What it refuses names the CLI's flags.
    pub fn parse(spec: &str, tools: bool) -> Result<Option<Self>> {
        if spec.trim().eq_ignore_ascii_case("off") {
            if tools {
                bail!("--m365-tools can't be combined with --m365 off");
            }
            return Ok(None);
        }
        match Self::from_labels(spec.split(','), tools) {
            Ok(grant) => Ok(Some(grant)),
            Err(M365GrantError::Unknown(label)) => bail!(
                "--m365 can't include '{label}': it stands for Work IQ content with no label \
                 information, whose sensitivity isn't known, so no peer gets it"
            ),
            Err(M365GrantError::Off(label)) => bail!(
                "--m365 can't include '{label}' among label names: --m365 off on its own removes \
                 the grant"
            ),
            Err(M365GrantError::NoLabels) => {
                bail!("--m365 needs label names (General,Public,unlabeled) or off")
            }
        }
    }

    /// A short description for listings.
    pub fn describe(&self) -> String {
        format!(
            "labels: {}{}",
            self.labels.join(","),
            if self.tools { ", tools" } else { "" }
        )
    }
}

/// Why [`M365Grant::from_labels`] refuses a grant; it says so without the CLI's flags, as the
/// web UI shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum M365GrantError {
    /// A label is `unknown` (as given): content with no label information, which no peer gets.
    Unknown(String),
    /// A label is `off` (as given), which isn't a label name.
    Off(String),
    /// There is no label name.
    NoLabels,
}

impl fmt::Display for M365GrantError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown(label) => write!(
                f,
                "The labels can't include '{label}': it stands for content with no label \
                 information, whose sensitivity isn't known, so no peer gets it"
            ),
            Self::Off(label) => write!(
                f,
                "The labels can't include '{label}': it isn't a label name; to give no \
                 Microsoft 365 content, remove the grant"
            ),
            Self::NoLabels => {
                f.write_str("Give at least one label name, e.g. General, Public, unlabeled")
            }
        }
    }
}

impl std::error::Error for M365GrantError {}

fn default_shares() -> Vec<String> {
    vec![crate::shares::DATA_SHARE.to_owned()]
}

impl TrustedPeer {
    pub fn addr(&self) -> EndpointAddr {
        EndpointAddr::from_parts(self.id, self.addrs.iter().cloned())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustStore {
    #[serde(default)]
    pub peers: Vec<TrustedPeer>,
}

impl TrustStore {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.is_file() {
            return Ok(Self::default());
        }
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(self)? + "\n")
            .with_context(|| format!("writing {}", path.display()))
    }

    pub fn get(&self, id: &EndpointId) -> Option<&TrustedPeer> {
        self.peers.iter().find(|p| &p.id == id)
    }

    pub fn get_by_name(&self, name: &str) -> Option<&TrustedPeer> {
        self.peers.iter().find(|p| p.name == name)
    }

    /// The grant of the entry that `allow` would replace (same id or name), so re-allowing a
    /// peer without mentioning Microsoft 365 keeps it.
    pub fn m365_of(&self, id: &EndpointId, name: &str) -> Option<M365Grant> {
        self.replaced(id, name).and_then(|p| p.m365.clone())
    }

    /// The GitHub account of the entry that `allow` would replace, so re-allowing a peer
    /// without naming one keeps it.
    pub fn github_of(&self, id: &EndpointId, name: &str) -> Option<String> {
        self.replaced(id, name).and_then(|p| p.github.clone())
    }

    /// The factory agents granted to the peer that `allow` would replace (see
    /// [`TrustedPeer::agents`]), so changing a peer's other rights keeps them.
    pub fn agents_of(&self, id: &EndpointId, name: &str) -> Vec<String> {
        self.replaced(id, name)
            .map(|p| p.agents.clone())
            .unwrap_or_default()
    }

    /// The entry `allow` would replace: the one with id `id`, else the one named `name`.
    fn replaced(&self, id: &EndpointId, name: &str) -> Option<&TrustedPeer> {
        self.peers
            .iter()
            .find(|p| &p.id == id)
            .or_else(|| self.get_by_name(name))
    }

    /// Who work context may go to when it is sent to `id`: `Local` for the owner's own
    /// digital workers (`workers`, their endpoint ids), the grant's labels for a granted
    /// peer, and `None` (refuse) for everyone else.
    pub fn audience_for(&self, id: &EndpointId, workers: &[EndpointId]) -> Option<Audience> {
        if workers.contains(id) {
            return Some(Audience::Local);
        }
        let grant = self.get(id)?.m365.as_ref()?;
        Some(Audience::peer(grant.labels.iter().cloned()))
    }

    /// Adds or updates a peer; an existing entry with the same id or name is replaced.
    pub fn allow(&mut self, peer: TrustedPeer) {
        self.peers
            .retain(|p| p.id != peer.id && p.name != peer.name);
        self.peers.push(peer);
        self.peers.sort_by(|a, b| a.name.cmp(&b.name));
    }

    /// Removes the peer with this name or id; returns whether one was removed.
    pub fn remove(&mut self, name_or_id: &str) -> bool {
        let id = EndpointId::from_str(name_or_id).ok();
        let before = self.peers.len();
        self.peers
            .retain(|p| p.name != name_or_id && Some(p.id) != id);
        self.peers.len() != before
    }
}

const TICKET_PREFIX: &str = "p2pa1";

/// What one agent gives another so it can be allowlisted and dialed: its public
/// key, its addresses and its suggested name. Encoded as `p2pa1<base64url json>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ticket {
    pub name: String,
    pub addr: EndpointAddr,
}

impl Ticket {
    pub fn encode(&self) -> String {
        let json = serde_json::to_vec(self).expect("ticket serializes");
        format!(
            "{TICKET_PREFIX}{}",
            data_encoding::BASE64URL_NOPAD.encode(&json)
        )
    }

    /// Parses a ticket, or a bare hex/base32 endpoint id (no addresses, no name).
    pub fn parse(s: &str) -> Result<Self> {
        let s = s.trim();
        if let Some(body) = s.strip_prefix(TICKET_PREFIX) {
            let json = data_encoding::BASE64URL_NOPAD
                .decode(body.as_bytes())
                .map_err(|e| anyhow!("invalid ticket: {e}"))?;
            return serde_json::from_slice(&json).context("invalid ticket");
        }
        match EndpointId::from_str(s) {
            Ok(id) => Ok(Self {
                name: String::new(),
                addr: EndpointAddr::new(id),
            }),
            Err(_) => bail!("not a proofkin ticket or endpoint id: {s}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iroh::SecretKey;

    fn peer(name: &str) -> TrustedPeer {
        TrustedPeer {
            name: name.into(),
            id: SecretKey::generate().public(),
            addrs: BTreeSet::new(),
            can_delegate: false,
            tasks_per_hour: DEFAULT_TASKS_PER_HOUR,
            access: Access::ReadOnly,
            shares: default_shares(),
            m365: None,
            github: None,
            agents: Vec::new(),
        }
    }

    #[test]
    fn nothing_is_allowed_by_default_and_absent_grants_stay_out_of_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("peers.json");
        let bob = peer("bob");
        let mut store = TrustStore::default();
        store.allow(bob.clone());
        store.save(&path).unwrap();
        assert!(!std::fs::read_to_string(&path).unwrap().contains("m365"));
        let loaded = TrustStore::load(&path).unwrap();
        assert_eq!(loaded.get(&bob.id).unwrap().m365, None);
        assert_eq!(loaded.audience_for(&bob.id, &[]), None);
    }

    #[test]
    fn m365_labels_are_stored_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("peers.json");
        let grant = M365Grant::parse("General, Public,unlabeled,general", true)
            .unwrap()
            .unwrap();
        assert_eq!(grant.labels, ["General", "Public", "unlabeled"]);
        assert!(grant.tools);
        let mut store = TrustStore::default();
        let bob = TrustedPeer {
            m365: Some(grant.clone()),
            github: None,
            agents: Vec::new(),
            ..peer("bob")
        };
        store.allow(bob.clone());
        store.save(&path).unwrap();
        let loaded = TrustStore::load(&path).unwrap();
        assert_eq!(loaded.get(&bob.id).unwrap().m365, Some(grant));
    }

    #[test]
    fn m365_off_removes_the_grant_and_bad_specs_are_refused() {
        assert_eq!(M365Grant::parse("off", false).unwrap(), None);
        assert_eq!(M365Grant::parse(" OFF ", false).unwrap(), None);
        // What `--m365` refuses names the CLI's flags.
        let refused =
            |spec: &str, tools: bool| M365Grant::parse(spec, tools).unwrap_err().to_string();
        assert_eq!(
            refused("off", true),
            "--m365-tools can't be combined with --m365 off"
        );
        assert_eq!(
            refused(" , ", false),
            "--m365 needs label names (General,Public,unlabeled) or off"
        );
        // Content without label information can't be granted to anyone.
        let error = refused("General, Unknown", false);
        assert!(
            error.starts_with("--m365 can't include 'Unknown': ")
                && error.contains("no label information"),
            "{error}"
        );
        // `off` isn't a label name: it only stands alone.
        assert_eq!(
            refused("General, off", false),
            "--m365 can't include 'off' among label names: --m365 off on its own removes the \
             grant"
        );
    }

    #[test]
    fn allowing_again_keeps_the_grant_unless_replaced() {
        let grant = M365Grant::parse("Public", false).unwrap();
        let bob = TrustedPeer {
            m365: grant.clone(),
            github: None,
            agents: Vec::new(),
            ..peer("bob")
        };
        let mut store = TrustStore::default();
        store.allow(bob.clone());
        assert_eq!(store.m365_of(&bob.id, "bob"), grant);
        assert_eq!(store.m365_of(&bob.id, "other"), grant);
        assert_eq!(store.m365_of(&peer("x").id, "bob"), grant);
        assert_eq!(store.m365_of(&peer("x").id, "x"), None);
        let again = TrustedPeer {
            can_delegate: true,
            m365: store.m365_of(&bob.id, "bob"),
            ..bob.clone()
        };
        store.allow(again);
        assert_eq!(store.get(&bob.id).unwrap().m365, grant);
        store.allow(TrustedPeer {
            m365: None,
            github: None,
            agents: Vec::new(),
            ..bob.clone()
        });
        assert_eq!(store.get(&bob.id).unwrap().m365, None);
    }

    #[test]
    fn the_audience_is_local_for_digital_workers_and_the_grant_for_peers() {
        let worker = peer("tester");
        let granted = TrustedPeer {
            m365: M365Grant::parse("General,unlabeled", false).unwrap(),
            ..peer("bob")
        };
        let plain = peer("carol");
        let mut store = TrustStore::default();
        for p in [&worker, &granted, &plain] {
            store.allow(p.clone());
        }
        let workers = [worker.id];
        assert_eq!(
            store.audience_for(&worker.id, &workers),
            Some(Audience::Local)
        );
        assert_eq!(
            store.audience_for(&granted.id, &workers),
            Some(Audience::peer(["General", "unlabeled"]))
        );
        assert_eq!(store.audience_for(&plain.id, &workers), None);
        assert_eq!(store.audience_for(&peer("stranger").id, &workers), None);
        // A worker is local even before it is on the allowlist.
        assert_eq!(
            TrustStore::default().audience_for(&worker.id, &workers),
            Some(Audience::Local)
        );
    }

    #[test]
    fn ticket_round_trips_and_bare_ids_parse() {
        let id = SecretKey::generate().public();
        let ticket = Ticket {
            name: "bob".into(),
            addr: EndpointAddr::new(id).with_ip_addr("127.0.0.1:9".parse().unwrap()),
        };
        let encoded = ticket.encode();
        assert!(encoded.starts_with("p2pa1"));
        assert_eq!(Ticket::parse(&encoded).unwrap(), ticket);
        assert_eq!(
            Ticket::parse(&id.to_string()).unwrap().addr,
            EndpointAddr::new(id)
        );
        assert!(Ticket::parse("p2pa1!!").is_err());
        assert!(Ticket::parse("hello").is_err());
    }

    #[test]
    fn allow_replaces_same_id_or_name_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("peers.json");
        let mut store = TrustStore::default();
        let bob = peer("bob");
        store.allow(bob.clone());
        store.allow(TrustedPeer {
            can_delegate: true,
            ..bob.clone()
        });
        store.allow(TrustedPeer {
            name: "bob".into(),
            ..peer("x")
        });
        assert_eq!(store.peers.len(), 1);
        assert_ne!(store.peers[0].id, bob.id);
        store.save(&path).unwrap();
        assert_eq!(TrustStore::load(&path).unwrap(), store);
        assert!(store.remove("bob"));
        assert!(!store.remove("bob"));
    }
}
