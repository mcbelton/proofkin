//! Who an agent will talk to. Every peer is identified by its public key
//! (iroh `EndpointId`, authenticated by the QUIC/TLS handshake); connections from
//! keys that are not on the profile's allowlist are dropped, and only peers marked
//! `can_delegate` may hand this agent LLM tasks.

use std::collections::BTreeSet;
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
    /// Reads `--m365`: comma-separated label names, or `off` for no grant.
    pub fn parse(spec: &str, tools: bool) -> Result<Option<Self>> {
        if spec.trim().eq_ignore_ascii_case("off") {
            if tools {
                bail!("--m365-tools can't be combined with --m365 off");
            }
            return Ok(None);
        }
        let mut labels: Vec<String> = Vec::new();
        for label in spec.split(',').map(str::trim).filter(|l| !l.is_empty()) {
            if !labels.iter().any(|l| l.eq_ignore_ascii_case(label)) {
                labels.push(label.to_owned());
            }
        }
        if labels.is_empty() {
            bail!("--m365 needs label names (General,Public,unlabeled) or off");
        }
        Ok(Some(Self { labels, tools }))
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
        self.peers
            .iter()
            .find(|p| &p.id == id)
            .or_else(|| self.get_by_name(name))
            .and_then(|p| p.m365.clone())
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
        assert!(M365Grant::parse("off", true).is_err());
        assert!(M365Grant::parse(" , ", false).is_err());
    }

    #[test]
    fn allowing_again_keeps_the_grant_unless_replaced() {
        let grant = M365Grant::parse("Public", false).unwrap();
        let bob = TrustedPeer {
            m365: grant.clone(),
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
