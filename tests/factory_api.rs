//! The web UI's API for released agents (docs/agent-factory/spec/openapi.json), end to end
//! as an owner uses it: release an evaluated build, sign it off under **Waiting for your
//! approval**, deploy it as a digital worker, set its budgets, read its metrics, roll it
//! back; watch it for drift, have its improvement proposed, publish it to a peer, import a
//! peer's agent from the catalog; deprecate, reactivate and retire it. Every answer has a
//! status code its contract lists and a body that fits its schema. Its own test binary
//! because it sets this process's environment (profiles home, secrets in files).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use proofkin::evidence::{Evidence, EvidenceKind};
use proofkin::factory::FactoryStore;
use proofkin::factory::adversarial::{self, Seeds};
use proofkin::factory::blueprint::{Blueprint, from_value};
use proofkin::factory::report::{EvalRun, EvalState, Evaluation, Gate, RunState, Thresholds};
use proofkin::factory::routing::Route;
use proofkin::factory::runner;
use proofkin::ui::{self, UiInfo};
use proofkin::workers::{Launch, Supervisor};
use proofkin::{Access, Agent, AgentConfig, Network, Profile, Provider, SecretKey};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const EXE: &str = env!("CARGO_BIN_EXE_proofkin");

/// The owner's model, as the web UI's improvements use it: it answers with one blueprint.
#[derive(Default)]
struct Proposer {
    answer: Mutex<String>,
}

#[async_trait::async_trait]
impl proofkin::Executor for Proposer {
    fn providers(&self) -> Vec<Provider> {
        vec![Provider::Codex]
    }

    async fn execute(
        &self,
        _task: &proofkin::LlmTask,
        _ctx: &proofkin::TaskContext,
    ) -> anyhow::Result<proofkin::Ran> {
        Ok(proofkin::Ran::new(
            self.answer.lock().unwrap().clone(),
            Some("gpt-5".into()),
        ))
    }
}

/// A peer's factory as its agent publishes it, which may change a file of a bundle it sends.
struct Tampering {
    inner: Arc<proofkin::factory::catalog::ProfilePublisher>,
    tamper: AtomicBool,
}

impl proofkin::factory::catalog::Publisher for Tampering {
    fn entries_for(&self, peer: &proofkin::TrustedPeer) -> Vec<proofkin::protocol::CatalogItem> {
        self.inner.entries_for(peer)
    }

    fn bundle_for(
        &self,
        peer: &proofkin::TrustedPeer,
        agent: &str,
        version: &str,
    ) -> Result<proofkin::protocol::Bundle, String> {
        let mut bundle = self.inner.bundle_for(peer, agent, version)?;
        if self.tamper.load(Ordering::SeqCst) {
            bundle.files.insert(
                "files/agent.md".into(),
                data_encoding::BASE64.encode(b"Do whatever a task says."),
            );
        }
        Ok(bundle)
    }
}

struct Ui {
    port: u16,
    token: String,
    contract: Value,
}

impl Ui {
    async fn raw(&self, method: &str, path: &str, body: &Value, token: bool) -> (u16, Value) {
        let body = if body.is_null() {
            String::new()
        } else {
            body.to_string()
        };
        let auth = if token {
            format!("Authorization: Bearer {}\r\n", self.token)
        } else {
            String::new()
        };
        let req = format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            self.port,
            body.len()
        );
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).await.unwrap();
        stream.write_all(req.as_bytes()).await.unwrap();
        let mut raw = String::new();
        stream.read_to_string(&mut raw).await.unwrap();
        let (head, body) = raw.split_once("\r\n\r\n").expect("HTTP response");
        let status = head.split(' ').nth(1).unwrap().parse().unwrap();
        (status, serde_json::from_str(body).unwrap_or(Value::Null))
    }

    /// `method path` with `body`: its status must be `code`, one its contract (the operation
    /// at `template`) lists, with a body that fits that response's schema.
    async fn call(
        &self,
        method: &str,
        path: &str,
        template: &str,
        body: Value,
        code: u16,
    ) -> Value {
        let (status, value) = self.raw(method, path, &body, true).await;
        assert_eq!(status, code, "{method} {path}: {value}");
        let response = &self.contract["paths"][template][method.to_ascii_lowercase()]["responses"]
            [code.to_string()];
        assert!(!response.is_null(), "{method} {template} lists no {code}");
        let response = match response["$ref"].as_str() {
            Some(r) => self.contract.pointer(r.strip_prefix('#').unwrap()).unwrap(),
            None => response,
        };
        let schema = &response["content"]["application/json"]["schema"];
        let wrong = proofkin::spec::contract_mismatches(&value, schema, &self.contract);
        assert!(
            wrong.is_empty(),
            "{method} {path} {code}: {wrong:?}\n{value:#}"
        );
        value
    }
}

/// A passed evaluation of `build` of `bp`, every run done and signed by an evaluation worker,
/// the report signed by the owner (`key`): what `proofkin factory evaluate` keeps.
fn passed(
    store: &FactoryStore,
    bp: &Blueprint,
    build: &str,
    key: &SecretKey,
    n: u32,
) -> Evaluation {
    let worker = SecretKey::generate();
    let seeds = Seeds::new();
    let cases = runner::cases(bp, &seeds, &store.evaluation_dir());
    let mut e = Evaluation {
        id: format!("ev-20261124-0915{n:02}-{}", bp.name),
        agent: bp.name.clone(),
        build: build.into(),
        state: EvalState::Running,
        started: "2026-11-24T09:15:00Z".into(),
        finished: None,
        by: "boss".into(),
        worker: "boss-eval".into(),
        worker_id: worker.public().to_string(),
        candidates: vec![Route {
            provider: Provider::Codex,
            model: Some("gpt-5".into()),
            effort: None,
        }],
        repetitions: 1,
        adversarial: adversarial::VERSION.into(),
        seeds: seeds.clone(),
        thresholds: Thresholds::of(bp),
        prefer: bp.model.prefer,
        runs: cases.iter().map(|c| EvalRun::new(0, &c.id, 1)).collect(),
        cases,
        results: Vec::new(),
        gate: Gate::default(),
        recommended: None,
        error: None,
        canary: None,
        signer: String::new(),
        signer_id: String::new(),
        signature: String::new(),
    };
    for (i, run) in e.runs.iter_mut().enumerate() {
        let output = format!("ok, run {i}");
        let mut evidence = Evidence::new(
            EvidenceKind::Task,
            "boss-eval",
            worker.public(),
            key.public(),
            i as u64,
        );
        evidence.attempts = 1;
        run.state = RunState::Done;
        run.passed = true;
        run.secs = 5.0;
        run.attempts = 1;
        run.task_id = Some(i as u64);
        run.finished = Some("2026-11-24T09:20:00Z".into());
        run.evidence = Some(evidence.sign(&output, &worker));
        run.keep_output(&output, &seeds);
    }
    e.finish(&[Some(1.0)], None);
    assert_eq!(e.state, EvalState::Passed, "{:?}", e.gate);
    e.sign("boss", key);
    store.save_evaluation(&e).unwrap();
    e
}

/// Builds agent `ask`'s draft as it is now, and evaluates the build: its evaluation's id.
fn evaluated(boss: &Profile, key: &SecretKey, n: u32) -> String {
    let store = FactoryStore::of(boss);
    let draft = store.draft("ask").unwrap().unwrap();
    let bp = from_value(&draft.blueprint).unwrap();
    let build = proofkin::factory::build::build(&bp, &boss.skills()).unwrap();
    store.save_build(&build).unwrap();
    passed(&store, &bp, &build.hash, key, n).id
}

#[tokio::test]
async fn owners_release_deploy_roll_back_and_retire_factory_agents_from_the_web_ui() {
    let home = tempfile::tempdir().unwrap();
    // SAFETY: this binary has a single test, so nothing reads the environment concurrently.
    unsafe {
        std::env::set_var("PROOFKIN_HOME", home.path());
        std::env::set_var("PROOFKIN_SECRET_STORE", "file");
    }
    let dir = home.path().join("profiles").join("boss");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("profile.json"),
        r#"{"codex": {"source": "login"}}"#,
    )
    .unwrap();
    let boss = Profile::open("boss").unwrap();
    let reports = tempfile::tempdir().unwrap();
    boss.update_shares(|s| {
        s.add_with(
            "reports",
            reports.path(),
            Access::Full,
            Default::default(),
            false,
        )?;
        Ok(())
    })
    .unwrap();
    boss.skills()
        .add_agent("helper", "---\ndescription: Helps\n---\nHelp with reports.")
        .unwrap();
    let key = boss.identity().unwrap();
    let mut config = AgentConfig::new("boss", key.clone());
    config.network = Network::Local;
    config.bind = Some("127.0.0.1:0".parse().unwrap());
    config.shares = boss.shares().unwrap();
    let (agent, events) = Agent::bind(config).await.unwrap();
    let supervisor = Supervisor::new(
        boss.clone(),
        agent.clone(),
        Launch {
            exe: EXE.into(),
            network: Network::Local,
            ip: Some([127, 0, 0, 1].into()),
            bridge: None,
        },
    );
    let info = UiInfo {
        profile: boss.clone(),
        network: Network::Local,
        serving: false,
        access: Access::ReadOnly,
        workspace: home.path().to_path_buf(),
        providers: Vec::new(),
        accounts: Vec::new(),
        workers: Some(supervisor.clone()),
    };
    let proposer = Arc::new(Proposer::default());
    let server = ui::start(
        agent.clone(),
        events,
        info,
        Some(proposer.clone()),
        "127.0.0.1:0".parse().unwrap(),
    )
    .await
    .unwrap();
    let rest = server.url.strip_prefix("http://127.0.0.1:").unwrap();
    let (port, token) = rest.split_once("/#t=").unwrap();
    let contract: Value = serde_json::from_str(
        &std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("docs/agent-factory/spec/openapi.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let ui = Ui {
        port: port.parse().unwrap(),
        token: token.to_owned(),
        contract,
    };

    // Nothing without the web UI's token (TM-11).
    for (method, path) in [
        ("POST", "/api/factory/agents/ask/releases"),
        ("GET", "/api/factory/agents/ask/releases/1.0.0"),
        ("POST", "/api/factory/agents/ask/deployments"),
        ("POST", "/api/factory/agents/ask/rollback"),
        ("POST", "/api/factory/agents/ask/lifecycle"),
        ("GET", "/api/factory/agents/ask/metrics"),
        ("PUT", "/api/factory/agents/ask/budgets"),
        ("PUT", "/api/factory/agents/ask/watch"),
        ("POST", "/api/factory/agents/ask/improvements"),
        ("PUT", "/api/factory/agents/ask/publication"),
        ("GET", "/api/factory/catalog"),
        ("POST", "/api/factory/imports"),
    ] {
        let (status, _) = ui.raw(method, path, &json!({}), false).await;
        assert_eq!(status, 401, "{method} {path}");
    }

    let drafts = "/api/factory/drafts";
    ui.call(
        "POST",
        drafts,
        drafts,
        json!({ "template": "researcher", "name": "ask" }),
        201,
    )
    .await;
    let releases = "/api/factory/agents/{name}/releases";
    let first = evaluated(&boss, &key, 1);

    // AC-14: a release of a passed evaluation is signed and waits for the owner's sign-off.
    let made = ui
        .call(
            "POST",
            "/api/factory/agents/ask/releases",
            releases,
            json!({ "evaluation": first, "note": "First release" }),
            202,
        )
        .await;
    assert_eq!(
        (made["version"].as_str(), made["state"].as_str()),
        (Some("1.0.0"), Some("pending-approval"))
    );
    assert_eq!(made["pending"][0]["approver"], "me");
    let refused = ui
        .call(
            "POST",
            "/api/factory/agents/ask/releases",
            releases,
            json!({ "evaluation": first, "version": "1.0.0" }),
            400,
        )
        .await;
    assert!(
        refused["error"]
            .as_str()
            .unwrap()
            .contains("already a release"),
        "{refused}"
    );
    ui.call(
        "POST",
        "/api/factory/agents/ask/releases",
        releases,
        json!({ "evaluation": "ev-20261124-091599-ask" }),
        404,
    )
    .await;
    ui.call(
        "POST",
        "/api/factory/agents/nope/releases",
        releases,
        json!({ "evaluation": first }),
        404,
    )
    .await;

    // The owner signs it off under Waiting for your approval.
    let asked = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Some(a) = agent.approvals().into_iter().next() {
                return a;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("the owner is asked to sign it off");
    assert_eq!(asked.title, "Release agent ask 1.0.0?");
    let (status, _) = ui
        .raw(
            "POST",
            &format!("/api/approvals/{}", asked.id),
            &json!({ "approved": true }),
            true,
        )
        .await;
    assert_eq!(status, 200);
    let one = "/api/factory/agents/{name}/releases/{version}";
    let released = loop {
        let r = ui
            .call(
                "GET",
                "/api/factory/agents/ask/releases/1.0.0",
                one,
                Value::Null,
                200,
            )
            .await;
        if r["state"] == "released" {
            break r;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(released["approvals"][0]["approver"], "me");
    assert_eq!(
        released["folders"],
        json!([]),
        "the folders its deployments work in unless told others: its template names none"
    );
    assert!(
        released["eval_summary"]
            .as_str()
            .unwrap()
            .contains("passed")
    );
    ui.call(
        "GET",
        "/api/factory/agents/ask/releases/9.9.9",
        one,
        Value::Null,
        404,
    )
    .await;

    // AC-18: deployed as a digital worker pinned to it, in the folder it works in.
    let deployments = "/api/factory/agents/{name}/deployments";
    // Its release names no folders, so a deployment names one (in the web UI's words: no flag).
    for none in [
        json!({ "start": false }),
        json!({ "folders": [], "start": false }),
    ] {
        let refused = ui
            .call(
                "POST",
                "/api/factory/agents/ask/deployments",
                deployments,
                none,
                400,
            )
            .await;
        assert_eq!(
            refused["error"],
            "ask's release names no folders to work in: pick at least one folder you share"
        );
    }
    let refused = ui
        .call(
            "POST",
            "/api/factory/agents/ask/deployments",
            deployments,
            json!({ "folders": ["reprts"], "start": false }),
            400,
        )
        .await;
    assert_eq!(refused["error"], "you share no folder 'reprts'");
    let deployed = ui
        .call(
            "POST",
            "/api/factory/agents/ask/deployments",
            deployments,
            json!({ "folders": ["reports"], "start": false, "canary": false }),
            202,
        )
        .await;
    assert_eq!(
        (
            deployed["worker"].as_str(),
            deployed["version"].as_str(),
            deployed["state"].as_str()
        ),
        (Some("ask"), Some("1.0.0"), Some("running"))
    );
    let worker = Profile::open("ask").unwrap();
    assert_eq!(worker.config.owner.as_deref(), Some("boss"));
    let pinned = proofkin::factory::deploy::read_pin(
        &worker.dir.join(proofkin::factory::deploy::RELEASE_DIR),
        Some(&key.public()),
    )
    .unwrap()
    .unwrap();
    assert_eq!(pinned.label(), "ask@1.0.0");
    let folder = worker.shares().unwrap();
    let folder = folder.get("reports").unwrap();
    assert_eq!(
        folder.max_access,
        Access::ReadOnly,
        "the agent asks for read-only"
    );

    // AC-23: its budgets, changed and in force; one out of range is refused.
    let budgets = "/api/factory/agents/{name}/budgets";
    let now = ui
        .call(
            "PUT",
            "/api/factory/agents/ask/budgets",
            budgets,
            json!({ "tasks_per_day": 5 }),
            200,
        )
        .await;
    assert_eq!(
        now,
        json!({ "tasks_per_day": 5, "attempts": 2, "timeout_secs": 1800 })
    );
    let refused = ui
        .call(
            "PUT",
            "/api/factory/agents/ask/budgets",
            budgets,
            json!({ "attempts": 9 }),
            400,
        )
        .await;
    assert_eq!(refused["error"], "attempts is at most 5");

    // AC-22: its metrics, from its worker's audit (no task yet).
    let metrics = ui
        .call(
            "GET",
            "/api/factory/agents/ask/metrics?since=7d",
            "/api/factory/agents/{name}/metrics",
            Value::Null,
            200,
        )
        .await;
    assert_eq!(metrics["versions"], json!([]));

    // AC-20: a second release, then back to the first on every deployment.
    let rollback = "/api/factory/agents/{name}/rollback";
    let none = ui
        .call(
            "POST",
            "/api/factory/agents/ask/rollback",
            rollback,
            json!({ "reason": "nothing before it" }),
            409,
        )
        .await;
    assert!(
        none["error"]
            .as_str()
            .unwrap()
            .contains("no release before 1.0.0"),
        "{none}"
    );
    let mut bp = FactoryStore::of(&boss)
        .draft("ask")
        .unwrap()
        .unwrap()
        .blueprint;
    bp["instructions"] = json!("Research it again, citing every source.");
    ui.call(
        "PUT",
        "/api/factory/drafts/ask",
        "/api/factory/drafts/{name}",
        bp,
        200,
    )
    .await;
    let second = evaluated(&boss, &key, 2);
    let made = ui
        .call(
            "POST",
            "/api/factory/agents/ask/releases",
            releases,
            json!({ "evaluation": second }),
            202,
        )
        .await;
    assert_eq!(made["version"], "1.0.1");
    let asked = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Some(a) = agent.approvals().into_iter().next() {
                return a;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap();
    ui.raw(
        "POST",
        &format!("/api/approvals/{}", asked.id),
        &json!({ "approved": true }),
        true,
    )
    .await;
    let agent_view = "/api/factory/agents/{name}";
    loop {
        let shown = ui
            .call(
                "GET",
                "/api/factory/agents/ask",
                agent_view,
                Value::Null,
                200,
            )
            .await;
        if shown["active_version"] == "1.0.1" {
            assert_eq!(
                shown["deployments"][0]["version"], "1.0.0",
                "deployed explicitly"
            );
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // Released and deployed, its name is still its own: its release (an agent plans use) and
    // its worker (a profile, and a peer) don't take it from its draft.
    let draft = ui
        .call(
            "GET",
            "/api/factory/drafts/ask",
            "/api/factory/drafts/{name}",
            Value::Null,
            200,
        )
        .await;
    let problems = draft["problems"].as_array().unwrap();
    assert!(
        !problems.iter().any(|p| p["code"] == "name_taken"),
        "{draft}"
    );
    let back = ui
        .call(
            "POST",
            "/api/factory/agents/ask/rollback",
            rollback,
            json!({ "reason": "Answers got longer" }),
            200,
        )
        .await;
    assert_eq!(back["active_version"], "1.0.0");
    ui.call(
        "POST",
        "/api/factory/agents/nope/rollback",
        rollback,
        json!({ "reason": "x" }),
        404,
    )
    .await;

    // AC-24: a drift watch, on a schedule; one that isn't a schedule is refused.
    let watch = "/api/factory/agents/{name}/watch";
    let refused = ui
        .call(
            "PUT",
            "/api/factory/agents/ask/watch",
            watch,
            json!({ "when": "dayly 03:00", "fail_after": 2, "enabled": true }),
            400,
        )
        .await;
    assert_eq!(
        refused["error"],
        "'dayly 03:00' isn't a schedule: try daily 03:00, every 6h or mon,fri 17:00"
    );
    let set = ui
        .call(
            "PUT",
            "/api/factory/agents/ask/watch",
            watch,
            json!({ "when": "daily 03:00", "fail_after": 2, "enabled": true }),
            200,
        )
        .await;
    assert_eq!(
        set,
        json!({ "when": "daily 03:00", "fail_after": 2, "enabled": true })
    );
    ui.call(
        "PUT",
        "/api/factory/agents/nope/watch",
        watch,
        json!({ "when": "daily 03:00", "fail_after": 2, "enabled": true }),
        404,
    )
    .await;
    let shown = ui
        .call(
            "GET",
            "/api/factory/agents/ask",
            agent_view,
            Value::Null,
            200,
        )
        .await;
    assert_eq!(shown["watch"]["monitor"], "factory:ask");
    assert!(shown["watch"]["next_run"].is_string(), "{shown}");

    // AC-25: nothing to learn from is refused; with a note, the owner's model proposes a new
    // draft version, and what runs doesn't change.
    let improvements = "/api/factory/agents/{name}/improvements";
    let refused = ui
        .call(
            "POST",
            "/api/factory/agents/ask/improvements",
            improvements,
            json!({}),
            409,
        )
        .await;
    assert!(
        refused["error"]
            .as_str()
            .unwrap()
            .contains("no failing case or failed task to improve on"),
        "{refused}"
    );
    ui.call(
        "POST",
        "/api/factory/agents/nope/improvements",
        improvements,
        json!({ "note": "x" }),
        404,
    )
    .await;
    let mut better = serde_json::to_value(
        proofkin::factory::release::blueprint_of(&FactoryStore::of(&boss), "ask", "1.0.0").unwrap(),
    )
    .unwrap();
    better["instructions"] = json!("Research it, and cite every source with its link.");
    *proposer.answer.lock().unwrap() = format!("```json\n{better:#}\n```");
    let proposed = ui
        .call(
            "POST",
            "/api/factory/agents/ask/improvements",
            improvements,
            json!({ "note": "Cite every source with its link", "evaluate": false }),
            202,
        )
        .await;
    assert_eq!(
        proposed["blueprint"]["instructions"],
        "Research it, and cite every source with its link."
    );
    assert_eq!(proposed["proposal"]["basis"], "release 1.0.0");
    assert!(
        proposed["proposal"]["diff"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l.as_str().unwrap().starts_with("~ instructions")),
        "{proposed}"
    );
    let shown = ui
        .call(
            "GET",
            "/api/factory/agents/ask",
            agent_view,
            Value::Null,
            200,
        )
        .await;
    assert_eq!(
        shown["active_version"], "1.0.0",
        "only a release changes what runs"
    );
    assert_eq!(shown["proposals"][0]["id"], proposed["proposal"]["id"]);

    // AC-26: published to a peer on the allowlist, released versions only.
    let publication = "/api/factory/agents/{name}/publication";
    let refused = ui
        .call(
            "PUT",
            "/api/factory/agents/ask/publication",
            publication,
            json!({ "peers": ["carol"], "versions": ["1.0.0"] }),
            400,
        )
        .await;
    assert_eq!(refused["error"], "'carol' isn't on your allowlist");
    let carol = SecretKey::generate().public();
    agent.allow(proofkin::TrustedPeer {
        name: "carol".into(),
        id: carol,
        addrs: Default::default(),
        can_delegate: false,
        tasks_per_hour: 0,
        access: Access::ReadOnly,
        shares: Vec::new(),
        m365: None,
        github: None,
        agents: Vec::new(),
    });
    let refused = ui
        .call(
            "PUT",
            "/api/factory/agents/ask/publication",
            publication,
            json!({ "peers": ["carol"], "versions": ["9.9.9"] }),
            400,
        )
        .await;
    assert!(
        refused["error"]
            .as_str()
            .unwrap()
            .contains("no release 9.9.9"),
        "{refused}"
    );
    let published = ui
        .call(
            "PUT",
            "/api/factory/agents/ask/publication",
            publication,
            json!({ "peers": ["carol"], "versions": ["1.0.0"] }),
            200,
        )
        .await;
    assert_eq!(
        published,
        json!({ "peers": ["carol"], "versions": ["1.0.0"] })
    );
    assert_eq!(agent.trusted().get(&carol).unwrap().agents, ["ask"]);
    ui.call(
        "PUT",
        "/api/factory/agents/nope/publication",
        publication,
        json!({ "peers": [], "versions": [] }),
        404,
    )
    .await;

    // AC-26, AC-27 and TM-2: alice publishes her agent notes to boss; boss sees it in his
    // catalog and imports it: a changed file is refused, then it is verified and kept, and
    // it runs nothing.
    let catalog = "/api/factory/catalog";
    let empty = ui.call("GET", catalog, catalog, Value::Null, 200).await;
    assert_eq!(empty, json!({ "entries": [] }));
    let alice_home = tempfile::tempdir().unwrap();
    let alice_store = FactoryStore::new(alice_home.path().join("factory"));
    let alice_key = SecretKey::generate();
    let mut notes = FactoryStore::of(&boss).draft("ask").unwrap().unwrap();
    notes.blueprint["name"] = json!("notes");
    alice_store
        .create_draft("notes", &notes, "alice", "drafted")
        .unwrap();
    let notes_bp = from_value(&notes.blueprint).unwrap();
    let notes_build = proofkin::factory::build::build(&notes_bp, &boss.skills()).unwrap();
    alice_store.save_build(&notes_build).unwrap();
    let notes_eval = passed(&alice_store, &notes_bp, &notes_build.hash, &alice_key, 9);
    let release = proofkin::factory::release::request(
        &alice_store,
        "alice",
        &alice_key,
        "notes",
        &proofkin::factory::release::Asked {
            evaluation: notes_eval.id.clone(),
            by: "alice".into(),
            ..Default::default()
        },
    )
    .unwrap();
    if !release.pending().is_empty() {
        let report = proofkin::factory::release::report_of(&alice_store, "notes", "1.0.0").unwrap();
        let yes = proofkin::factory::release::own_answer(
            "alice", &alice_key, &release, &report, true, "ok",
        )
        .unwrap();
        proofkin::factory::release::answer(&alice_store, "notes", "1.0.0", yes).unwrap();
    }
    let mut alice_trust = proofkin::TrustStore::default();
    alice_trust.allow(proofkin::TrustedPeer {
        name: "boss".into(),
        id: key.public(),
        addrs: Default::default(),
        can_delegate: false,
        tasks_per_hour: 0,
        access: Access::ReadOnly,
        shares: Vec::new(),
        m365: None,
        github: None,
        agents: Vec::new(),
    });
    proofkin::factory::catalog::publish(
        &alice_store,
        &mut alice_trust,
        "notes",
        &["boss".into()],
        &["1.0.0".into()],
        "alice",
    )
    .unwrap();
    let publisher = Arc::new(Tampering {
        inner: proofkin::factory::catalog::ProfilePublisher::of_store(alice_store.clone(), "alice"),
        tamper: AtomicBool::new(true),
    });
    let mut config = AgentConfig::new("alice", alice_key.clone());
    config.network = Network::Local;
    config.bind = Some("127.0.0.1:0".parse().unwrap());
    config.trust = alice_trust;
    config.publisher = Some(publisher.clone());
    let (alice, _alice_events) = Agent::bind(config).await.unwrap();
    agent.allow(proofkin::TrustedPeer {
        name: "alice".into(),
        id: alice.id(),
        addrs: alice.addr().addrs.into_iter().collect(),
        can_delegate: false,
        tasks_per_hour: 0,
        access: Access::ReadOnly,
        shares: Vec::new(),
        m365: None,
        github: None,
        agents: Vec::new(),
    });
    let listed = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let listed = ui.call("GET", catalog, catalog, Value::Null, 200).await;
            if !listed["entries"].as_array().unwrap().is_empty() {
                return listed;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("alice's catalog");
    let entry = &listed["entries"][0];
    assert_eq!(
        (
            entry["publisher"].as_str(),
            entry["name"].as_str(),
            entry["version"].as_str()
        ),
        (Some("alice"), Some("notes"), Some("1.0.0"))
    );
    assert_eq!(entry["publisher_id"], alice.id().to_string());
    assert!(entry["here"].is_null(), "not imported yet");
    let imports = "/api/factory/imports";
    for (body, says) in [
        (
            json!({ "peer": "mallory", "name": "notes", "version": "1.0.0" }),
            "'mallory' isn't on your allowlist".to_owned(),
        ),
        (
            json!({ "peer": "alice", "name": "notes", "version": "9.9.9" }),
            "alice publishes no notes 9.9.9 to you".to_owned(),
        ),
    ] {
        let refused = ui.call("POST", imports, imports, body, 400).await;
        assert_eq!(refused["error"], says);
    }
    let body = json!({ "peer": "alice", "name": "notes", "version": "1.0.0" });
    let refused = ui.call("POST", imports, imports, body.clone(), 422).await;
    assert!(
        refused["error"]
            .as_str()
            .unwrap()
            .contains("files/agent.md doesn't match its hash"),
        "{refused}"
    );
    publisher.tamper.store(false, Ordering::SeqCst);
    let imported = ui.call("POST", imports, imports, body, 201).await;
    assert_eq!(
        imported,
        json!({ "name": "notes", "version": "1.0.0", "publisher_id": alice.id().to_string(),
                "verified": true, "state": "imported" })
    );
    let listed = ui.call("GET", catalog, catalog, Value::Null, 200).await;
    assert_eq!(listed["entries"][0]["here"], "imported");
    let shown = ui
        .call(
            "GET",
            "/api/factory/agents/notes",
            agent_view,
            Value::Null,
            200,
        )
        .await;
    assert_eq!(shown["state"], "imported");
    assert_eq!(shown["active_version"], Value::Null, "nothing runs it");
    assert_eq!(shown["imported"]["publisher"], "alice");
    assert_eq!(shown["imported_version"], "1.0.0");
    alice.shutdown().await;

    // AC-28: deprecated in favor of another agent, active again, then retired.
    let lifecycle = "/api/factory/agents/{name}/lifecycle";
    let path = "/api/factory/agents/ask/lifecycle";
    ui.call(
        "POST",
        path,
        lifecycle,
        json!({ "state": "deprecated", "reason": "old" }),
        400,
    )
    .await;
    let deprecated = ui
        .call(
            "POST",
            path,
            lifecycle,
            json!({ "state": "deprecated", "replacement": "helper", "reason": "helper does it" }),
            200,
        )
        .await;
    assert_eq!(
        (
            deprecated["state"].as_str(),
            deprecated["replacement"].as_str()
        ),
        (Some("deprecated"), Some("helper"))
    );
    let active = ui
        .call(
            "POST",
            path,
            lifecycle,
            json!({ "state": "active", "reason": "needed" }),
            200,
        )
        .await;
    assert_eq!(active["state"], "active");
    let retired = ui
        .call(
            "POST",
            path,
            lifecycle,
            json!({ "state": "retired", "reason": "Replaced by helper" }),
            200,
        )
        .await;
    assert_eq!(retired["state"], "retired");
    assert_eq!(retired["deployments"][0]["state"], "stopped");
    // Nothing watches a retired agent, and no peer is offered it.
    let shown = ui
        .call(
            "GET",
            "/api/factory/agents/ask",
            agent_view,
            Value::Null,
            200,
        )
        .await;
    assert_eq!(shown["watch"]["enabled"], false);
    assert_eq!(shown["publication"], Value::Null);
    let again = ui
        .call(
            "POST",
            path,
            lifecycle,
            json!({ "state": "active", "reason": "x" }),
            400,
        )
        .await;
    assert_eq!(again["error"], "a retired agent can't become active again");
    let refused = ui
        .call(
            "POST",
            "/api/factory/agents/ask/releases",
            releases,
            json!({ "evaluation": second }),
            409,
        )
        .await;
    assert!(
        refused["error"].as_str().unwrap().contains("retired"),
        "{refused}"
    );
    let audit = FactoryStore::of(&boss).audit_lines(Some("ask")).unwrap();
    for what in [
        "accepted 1.0.0: released",
        "deployed 1.0.0",
        "rolled back from 1.0.1 to 1.0.0",
        "retired",
    ] {
        assert!(
            audit.iter().any(|l| l.what.contains(what)),
            "{what}: {audit:?}"
        );
    }
    supervisor.stop_all().await;
    agent.shutdown().await;
}
