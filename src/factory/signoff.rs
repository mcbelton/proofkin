//! Release sign-offs while the owner's agent runs (docs/agent-factory.md, section 8): each
//! release waiting for approvers gets their answers through the agent's approval requests,
//! as a plan's `approve` step does. Peers on the owner's allowlist are asked when they are
//! connected (again after they reconnect); the owner is asked under **Waiting for your
//! approval** (or answers with `proofkin factory approve`), last, with the capability
//! increases. An answer is the approver's signed approval, kept with the release (see
//! [`crate::factory::release::answer`]); a request whose answer came another way is
//! withdrawn.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::agent::{Agent, ApprovalAsk};
use crate::factory::release::{self, OWNER};
use crate::factory::store::FactoryStore;

/// How often waiting releases are looked at.
const EVERY: Duration = Duration::from_secs(3);

/// A request out now: the agent, version and approver it is for.
type Key = (String, String, String);

/// Asks for the sign-offs of the waiting releases in `store` (the profile's own factory) with
/// `agent` (the profile's own, running), for as long as it runs; `changed` gets an agent's
/// name when its release got an answer.
pub fn spawn(
    store: FactoryStore,
    agent: Agent,
    changed: Arc<dyn Fn(&str) + Send + Sync>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let out: Arc<Mutex<HashMap<Key, tokio::task::AbortHandle>>> = Arc::default();
        loop {
            ask(&store, &agent, &out, &changed);
            tokio::time::sleep(EVERY).await;
        }
    })
}

/// Sends the requests waiting releases need and nobody has, and withdraws those no
/// release needs any more.
fn ask(
    store: &FactoryStore,
    agent: &Agent,
    out: &Arc<Mutex<HashMap<Key, tokio::task::AbortHandle>>>,
    changed: &Arc<dyn Fn(&str) + Send + Sync>,
) {
    let mut wanted = HashSet::new();
    let connected: HashSet<_> = agent.peers().into_iter().map(|p| p.id).collect();
    let trusted = agent.trusted();
    for name in store.names().unwrap_or_default() {
        let Ok(list) = release::releases(store, &name) else {
            continue;
        };
        for (version, found) in list {
            let Ok(waiting) = found else {
                continue;
            };
            for ask in waiting.pending() {
                let key = (name.clone(), version.clone(), ask.approver.clone());
                wanted.insert(key.clone());
                if out.lock().unwrap().contains_key(&key) {
                    continue;
                }
                let peer = if ask.approver == OWNER {
                    None
                } else {
                    match trusted.get_by_name(&ask.approver) {
                        // Asked once they are connected.
                        Some(p) if connected.contains(&p.id) => Some(p.id),
                        _ => continue,
                    }
                };
                let Ok(report) = release::report_of(store, &name, &version) else {
                    continue;
                };
                let (title, text) = release::request_text(&waiting, &report, &ask);
                let request = ApprovalAsk {
                    title,
                    text,
                    preview: Default::default(),
                    changes: true,
                };
                let (agent, store, out_, changed) =
                    (agent.clone(), store.clone(), out.clone(), changed.clone());
                let (name, version, key_) = (name.clone(), version.clone(), key.clone());
                let mut slot = out.lock().unwrap();
                let task = tokio::spawn(async move {
                    let task_id = agent.new_task_id();
                    if let Ok(delivered) = agent.request_approval_as(peer, task_id, &request).await
                        && let Some(evidence) = delivered.evidence
                    {
                        let approval = release::approval_of(
                            &ask,
                            peer.unwrap_or_else(|| agent.id()),
                            &delivered.output,
                            evidence,
                        );
                        match release::answer(&store, &name, &version, approval) {
                            Ok(_) => changed(&name),
                            Err(e) => eprintln!(
                                "the answer to release {version} of {name} wasn't kept: {e:#}"
                            ),
                        }
                    }
                    // Not answered (its approver left): asked again when they are back.
                    out_.lock().unwrap().remove(&key_);
                });
                slot.insert(key, task.abort_handle());
            }
        }
    }
    out.lock().unwrap().retain(|key, task| {
        let keep = wanted.contains(key) && !task.is_finished();
        if !keep {
            task.abort();
        }
        keep
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{AgentConfig, Network};
    use crate::factory::release::tests::Fixture;
    use crate::factory::release::{Cover, ReleaseState};
    use crate::llm::Access;
    use crate::trust::{TrustStore, TrustedPeer};

    async fn agent(name: &str, key: iroh::SecretKey, trust: TrustStore) -> Agent {
        let mut config = AgentConfig::new(name, key);
        config.network = Network::Local;
        config.bind = Some("127.0.0.1:0".parse().unwrap());
        config.trust = trust;
        Agent::bind(config).await.unwrap().0
    }

    fn peer(name: &str, id: iroh::EndpointId) -> TrustedPeer {
        TrustedPeer {
            name: name.into(),
            id,
            addrs: Default::default(),
            can_delegate: false,
            tasks_per_hour: 0,
            access: Access::ReadOnly,
            shares: Vec::new(),
            m365: None,
            github: None,
        }
    }

    async fn until(done: impl Fn() -> bool) {
        tokio::time::timeout(std::time::Duration::from_secs(20), async {
            while !done() {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("timed out");
    }

    /// AC-16, AC-17: each approver gets a sign-off request through their own agent and
    /// answers it signed; then the owner signs off the capability increases; the release
    /// keeps both answers and verifies.
    #[tokio::test]
    async fn approvers_sign_off_through_their_agents_then_the_owner_on_the_new_capabilities() {
        let f = Fixture::new();
        let e = f.evaluated(|v| v["approvers"] = serde_json::json!(["carol"]), false);
        let release = f.release(&e.id, None).unwrap();
        assert!(
            !release.manifest.increases.is_empty(),
            "the pdf server is new"
        );
        let carol_key = iroh::SecretKey::generate();
        let mut owner_trust = TrustStore::default();
        owner_trust.allow(peer("carol", carol_key.public()));
        let mut carol_trust = TrustStore::default();
        carol_trust.allow(peer("alice", f.owner.public()));
        let owner = agent("alice", f.owner.clone(), owner_trust).await;
        let carol = agent("carol", carol_key, carol_trust).await;
        owner.connect(carol.addr()).await.unwrap();
        until(|| owner.peers().iter().any(|p| p.name == "carol")).await;

        let out: Arc<Mutex<HashMap<Key, tokio::task::AbortHandle>>> = Arc::default();
        let changes = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen = changes.clone();
        let changed: Arc<dyn Fn(&str) + Send + Sync> =
            Arc::new(move |name: &str| seen.lock().unwrap().push(name.to_owned()));
        ask(&f.store, &owner, &out, &changed);
        // Carol sees what she is asked, and accepts it.
        until(|| !carol.approvals().is_empty()).await;
        let asked = carol.approvals().remove(0);
        assert_eq!(asked.title, "Release agent fin-analyst 1.0.0?");
        assert!(asked.text.contains("Evaluation: ev-"), "{}", asked.text);
        assert!(
            asked
                .text
                .contains("More than nothing (its first release): MCP server pdf")
        );
        assert!(asked.changes, "she may ask for changes too");
        carol
            .answer_approval(asked.id, true, "Tools look right")
            .await
            .unwrap();
        let load = || release::load(&f.store, "fin-analyst", "1.0.0").unwrap();
        until(|| !load().approvals.is_empty()).await;
        assert_eq!(
            load().state,
            ReleaseState::PendingApproval,
            "the owner signs off last"
        );

        // Now the owner, on the capability increases, under Waiting for your approval.
        ask(&f.store, &owner, &out, &changed);
        until(|| !owner.approvals().is_empty()).await;
        let mine = owner.approvals().remove(0);
        assert_eq!(
            mine.title,
            "Sign off agent fin-analyst 1.0.0's new capabilities?"
        );
        owner.answer_approval(mine.id, true, "").await.unwrap();
        until(|| load().state == ReleaseState::Released).await;
        let released = load();
        let carols = &released.approvals[0];
        assert_eq!(
            (carols.approver.as_str(), carols.note.as_str()),
            ("carol", "Tools look right")
        );
        assert_eq!(released.approvals[1].covers, [Cover::Capabilities]);
        assert_eq!(
            f.store
                .state("fin-analyst")
                .unwrap()
                .unwrap()
                .active_version
                .as_deref(),
            Some("1.0.0")
        );
        assert!(changes.lock().unwrap().iter().any(|n| n == "fin-analyst"));
        ask(&f.store, &owner, &out, &changed);
        assert!(
            out.lock().unwrap().is_empty(),
            "nothing is asked of a released release"
        );
        owner.shutdown().await;
        carol.shutdown().await;
    }
}
