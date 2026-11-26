//! Digital workers from their owner's web UI, end to end: a worker created there
//! runs as a real proofkin agent that the owner's agent starts, connects to its
//! owner, and is stopped, started and deleted from there. Its own test binary
//! because it sets this process's environment (profiles home, secrets in files),
//! which the workers it starts inherit.

use std::process::Stdio;
use std::time::Duration;

use proofkin::trust::{DEFAULT_TASKS_PER_HOUR, TrustedPeer};
use proofkin::ui::{self, UiInfo};
use proofkin::workers::{Launch, Supervisor};
use proofkin::{Access, Agent, AgentConfig, Network, Profile, SecretKey};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const EXE: &str = env!("CARGO_BIN_EXE_proofkin");

struct Ui {
    port: u16,
    token: String,
}

impl Ui {
    async fn call(&self, method: &str, path: &str, body: Value) -> (u16, Value) {
        let body = if body.is_null() {
            String::new()
        } else {
            body.to_string()
        };
        let req = format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            self.port,
            self.token,
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

    /// Worker `name` as `GET /api/workers` shows it, once `done` holds for it.
    async fn until(&self, name: &str, what: &str, done: impl Fn(&Value) -> bool) -> Value {
        let mut last = Value::Null;
        for _ in 0..300 {
            let (_, list) = self.call("GET", "/api/workers", Value::Null).await;
            if let Some(w) = list["workers"]
                .as_array()
                .and_then(|ws| ws.iter().find(|w| w["name"] == name))
            {
                if done(w) {
                    return w.clone();
                }
                last = w.clone();
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        panic!("{name} never got {what}: {last}");
    }
}

fn is_online(w: &Value) -> bool {
    w["online"] == true && w["process"]["state"] == "running"
}

#[tokio::test]
async fn owners_create_start_stop_and_delete_digital_workers_from_the_web_ui() {
    let home = tempfile::tempdir().unwrap();
    // SAFETY: this binary has a single test, so nothing reads the environment concurrently.
    unsafe {
        std::env::set_var("PROOFKIN_HOME", home.path());
        std::env::set_var("PROOFKIN_SECRET_STORE", "file");
    }
    let boss = Profile::open("boss").unwrap();
    boss.save().unwrap();
    let app = tempfile::tempdir().unwrap();
    boss.update_shares(|s| {
        let checks = [("test".to_owned(), "echo ok".to_owned())].into();
        s.add_with("app", app.path(), Access::WorkspaceWrite, checks, false)?;
        Ok(())
    })
    .unwrap();
    let mut config = AgentConfig::new("boss", boss.identity().unwrap());
    config.network = Network::Local;
    config.bind = Some("127.0.0.1:0".parse().unwrap());
    config.shares = boss.shares().unwrap();
    let (agent, events) = Agent::bind(config).await.unwrap();
    let launch = Launch {
        exe: EXE.into(),
        network: Network::Local,
        ip: Some([127, 0, 0, 1].into()),
        bridge: None,
    };
    let supervisor = Supervisor::new(boss.clone(), agent.clone(), launch);
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
    let server = ui::start(
        agent.clone(),
        events,
        info,
        None,
        "127.0.0.1:0".parse().unwrap(),
    )
    .await
    .unwrap();
    let rest = server.url.strip_prefix("http://127.0.0.1:").unwrap();
    let (port, token) = rest.split_once("/#t=").unwrap();
    let ui = Ui {
        port: port.parse().unwrap(),
        token: token.to_owned(),
    };

    // Workers run on their owner's sign-ins: none to run Copilot on yet, and an
    // account the GitHub CLI doesn't have signs nobody in.
    let (status, listed) = ui.call("GET", "/api/workers", Value::Null).await;
    assert_eq!(status, 200);
    assert_eq!(listed["can_own"], true);
    assert_eq!(listed["folders"][0]["name"], "app");
    let (status, _) = ui
        .call(
            "POST",
            "/api/sign-in/copilot",
            json!({ "user": "no-such-account-p2p", "host": "github.com" }),
        )
        .await;
    assert_eq!(status, 400);
    assert!(
        !Profile::open("boss")
            .unwrap()
            .is_configured(proofkin::Provider::Copilot)
    );
    let (status, refused) = ui
        .call(
            "POST",
            "/api/workers",
            json!({ "name": "helper", "providers": ["copilot"] }),
        )
        .await;
    assert_eq!(status, 400);
    assert!(
        refused["error"]
            .as_str()
            .unwrap()
            .contains("isn't signed in to copilot"),
        "{refused}"
    );

    // Created with boss's folder 'app': it shares it as boss does, lets boss give it
    // tasks there, starts now and connects to boss.
    let (status, created) = ui
        .call(
            "POST",
            "/api/workers",
            json!({ "name": "helper", "role": "  test   engineer ", "folders": ["app"], "start_with_owner": true }),
        )
        .await;
    assert_eq!(status, 200, "{created}");
    assert_eq!(created["started"], true, "{created}");
    let helper = Profile::open("helper").unwrap();
    assert_eq!(helper.config.owner.as_deref(), Some("boss"));
    let shares = helper.shares().unwrap();
    let app_share = shares.get("app").unwrap();
    assert_eq!(
        (app_share.max_access, app_share.checks["test"].as_str()),
        (Access::WorkspaceWrite, "echo ok")
    );
    let grant = helper
        .trust()
        .unwrap()
        .get_by_name("boss")
        .cloned()
        .unwrap();
    assert!(grant.can_delegate);
    assert_eq!(
        (grant.access, grant.shares.as_slice()),
        (
            Access::WorkspaceWrite,
            &["data".to_owned(), "app".to_owned()][..]
        )
    );
    assert!(
        !agent.trusted().get_by_name("helper").unwrap().can_delegate,
        "its owner lets it chat, not spend its accounts"
    );
    let online = ui.until("helper", "online", is_online).await;
    assert_eq!(
        (&online["role"], &online["folders"]),
        (&json!("test engineer"), &json!(["app"]))
    );
    let peer = agent
        .peers()
        .into_iter()
        .find(|p| p.name == "helper")
        .unwrap();
    assert_eq!(
        (peer.owner.as_deref(), peer.role.as_deref()),
        (Some("boss"), Some("test engineer"))
    );

    // One agent per profile: started again, here or by hand, it is refused.
    let (status, again) = ui
        .call("POST", "/api/workers/helper/start", Value::Null)
        .await;
    assert_eq!(status, 400);
    assert!(again["error"].as_str().unwrap().contains("already running"));
    let by_hand = std::process::Command::new(EXE)
        .args([
            "--name",
            "helper",
            "--network",
            "local",
            "--listen",
            "127.0.0.1:0",
        ])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!by_hand.status.success());
    assert!(
        String::from_utf8_lossy(&by_hand.stderr).contains("already running"),
        "{}",
        String::from_utf8_lossy(&by_hand.stderr)
    );

    // Stopped, it quits and leaves; started again, it's back.
    let (status, _) = ui
        .call("POST", "/api/workers/helper/stop", Value::Null)
        .await;
    assert_eq!(status, 200);
    ui.until("helper", "stopped", |w| {
        w["process"]["state"] == "stopped" && w["online"] == false
    })
    .await;
    let (status, _) = ui
        .call("POST", "/api/workers/helper/start", Value::Null)
        .await;
    assert_eq!(status, 200);
    ui.until("helper", "online again", is_online).await;

    // Deleted while it runs: it stops, leaves the allowlist and its squads (one left
    // empty goes), and its profile goes; its data folder stays.
    agent.allow(TrustedPeer {
        name: "alice".into(),
        id: SecretKey::generate().public(),
        addrs: Default::default(),
        can_delegate: false,
        tasks_per_hour: DEFAULT_TASKS_PER_HOUR,
        access: Access::ReadOnly,
        shares: Vec::new(),
        m365: None,
        github: None,
        agents: Vec::new(),
    });
    for (squad, members) in [
        ("mixed", json!(["helper", "alice"])),
        ("solo", json!(["helper"])),
    ] {
        let (status, saved) = ui
            .call(
                "POST",
                "/api/squads",
                json!({ "name": squad, "members": members }),
            )
            .await;
        assert_eq!(status, 200, "{saved}");
    }
    std::fs::create_dir_all(helper.data_folder()).unwrap();
    std::fs::write(helper.data_folder().join("notes.md"), "kept").unwrap();
    let (status, deleted) = ui.call("DELETE", "/api/workers/helper", Value::Null).await;
    assert_eq!(status, 200, "{deleted}");
    assert_eq!(deleted["squads"], json!(["mixed", "solo"]));
    assert!(!helper.dir.exists());
    assert!(helper.data_folder().join("notes.md").is_file());
    assert!(agent.trusted().get_by_name("helper").is_none());
    let (_, state) = ui.call("GET", "/api/state", Value::Null).await;
    assert_eq!(
        state["squads"],
        json!([{ "name": "mixed", "members": [{ "name": "alice", "online": false }] }])
    );

    // One that runs outside this agent isn't deleted from here until it stops.
    let (status, _) = ui
        .call("POST", "/api/workers", json!({ "name": "aide" }))
        .await;
    assert_eq!(status, 200);
    let mut outside = tokio::process::Command::new(EXE)
        .args([
            "--name",
            "aide",
            "--network",
            "local",
            "--listen",
            "127.0.0.1:0",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    ui.until("aide", "running outside", |w| {
        w["process"]["state"] == "elsewhere"
    })
    .await;
    let (status, kept) = ui.call("DELETE", "/api/workers/aide", Value::Null).await;
    assert_eq!(status, 409, "{kept}");
    assert!(
        kept["error"]
            .as_str()
            .unwrap()
            .contains("outside this agent")
    );
    let mut input = outside.stdin.take().unwrap();
    input.write_all(b"/quit\n").await.unwrap();
    drop(input);
    outside.wait().await.unwrap();
    let (status, _) = ui.call("DELETE", "/api/workers/aide", Value::Null).await;
    assert_eq!(status, 200);

    // One that can't run stops on its own: the page gets why, the feed says so.
    let (status, _) = ui
        .call("POST", "/api/workers", json!({ "name": "broken" }))
        .await;
    assert_eq!(status, 200);
    std::fs::write(Profile::open("broken").unwrap().trust_path(), "not json").unwrap();
    let (status, _) = ui
        .call("POST", "/api/workers/broken/start", Value::Null)
        .await;
    assert_eq!(status, 200);
    let exited = ui
        .until("broken", "exited", |w| w["process"]["state"] == "exited")
        .await;
    assert_eq!(exited["process"]["code"], 1);
    assert!(
        exited["process"]["log"]
            .as_str()
            .unwrap()
            .contains("peers.json"),
        "{exited}"
    );
    let (_, state) = ui.call("GET", "/api/state", Value::Null).await;
    assert!(
        state["feed"].as_array().unwrap().iter().any(|i| i["text"]
            .as_str()
            .is_some_and(|t| t.contains("broken stopped on its own (exit code 1)"))),
        "{}",
        state["feed"]
    );

    // Workers that start with their owner start when its agent starts (start_all),
    // and stop when it stops (stop_all).
    let (status, _) = ui
        .call("POST", "/api/workers", json!({ "name": "early" }))
        .await;
    assert_eq!(status, 200);
    let (status, _) = ui
        .call(
            "PATCH",
            "/api/workers/early",
            json!({ "start_with_owner": true }),
        )
        .await;
    assert_eq!(status, 200);
    let started: Vec<(String, bool)> = supervisor
        .start_all()
        .into_iter()
        .map(|(name, r)| (name, r.is_ok()))
        .collect();
    assert_eq!(started, [("early".to_owned(), true)]);
    ui.until("early", "online", is_online).await;
    supervisor.stop_all().await;
    ui.until("early", "stopped with its owner", |w| {
        w["process"]["state"] == "stopped"
    })
    .await;
    for name in ["early", "broken"] {
        let (status, _) = ui
            .call("DELETE", &format!("/api/workers/{name}"), Value::Null)
            .await;
        assert_eq!(status, 200, "{name}");
    }
    let (_, listed) = ui.call("GET", "/api/workers", Value::Null).await;
    assert_eq!(listed["workers"], json!([]));
    agent.shutdown().await;
}
