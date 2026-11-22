//! The Work IQ CLI connection against a fake `workiq mcp` (tests/fixtures/workiq_cli_mcp.py),
//! started through the transport's command hook. Its own test binary because the tests set
//! credential variables in the environment and use the file secret store.

use std::path::Path;
use std::sync::{Arc, Once};

use proofkin::Profile;
use proofkin::profile::{INHERITED_CREDENTIAL_VARS, WorkIqAccount, WorkIqConnection};
use proofkin::workiq::login::CliLogin;
use proofkin::workiq::stdio::{NoToken, StdioTransport};
use proofkin::workiq::{
    Class, Client, Outgoing, TokenSource, ToolResult, Transport, WorkIqError, connect,
};
use serde_json::{Value, json};

const SCRIPT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/workiq_cli_mcp.py"
);

static SETUP: Once = Once::new();

/// Uses the file secret store, and puts a credential in every variable an agent's provider
/// processes lose, so a child that inherited them can be told.
fn setup() {
    SETUP.call_once(|| {
        // SAFETY: every test calls this first and `call_once` makes the others wait for it,
        // so nothing in this binary reads the environment while it is changed.
        unsafe {
            std::env::set_var("PROOFKIN_SECRET_STORE", "file");
            for var in INHERITED_CREDENTIAL_VARS {
                std::env::set_var(var, "parent-secret");
            }
        }
    });
}

fn python() -> &'static str {
    if cfg!(windows) { "python" } else { "python3" }
}

fn fake(state: &Path, mode: &str, tenant: Option<&str>) -> Arc<StdioTransport> {
    setup();
    let state = state.to_str().unwrap();
    Arc::new(StdioTransport::with_command(
        python(),
        &[SCRIPT, state, mode],
        tenant,
    ))
}

fn cli_login(tenant: Option<&str>, state: &Path, mode: &str) -> CliLogin {
    let state = state.to_str().unwrap();
    CliLogin::new(tenant).with_command(python(), &[SCRIPT, state, mode])
}

fn client(transport: Arc<StdioTransport>) -> Client {
    Client::with_transport(transport, Arc::new(NoToken))
}

fn log(state: &Path) -> Vec<String> {
    std::fs::read_to_string(state.join("log.txt"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

fn count(state: &Path, entry: &str) -> usize {
    log(state).iter().filter(|line| *line == entry).count()
}

fn text(result: &ToolResult) -> String {
    result.content[0]["text"].as_str().unwrap().to_string()
}

fn call(id: u64, tool: &str) -> Outgoing {
    Outgoing {
        message: json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": { "name": tool, "arguments": { "query": "budget" } },
        }),
        expect_id: Some(id),
        session_id: None,
        protocol_version: None,
        bearer: String::new(),
        client_request_id: "test".to_string(),
    }
}

fn cli_account(tenant: Option<&str>) -> WorkIqAccount {
    WorkIqAccount {
        connection: WorkIqConnection::Cli {
            tenant: tenant.map(str::to_string),
        },
        upn: "Work IQ CLI".into(),
        display_name: "Work IQ CLI".into(),
        tenant_id: "unknown".into(),
        added: 0,
    }
}

#[tokio::test]
async fn initialize_list_tools_and_call_tool_run_over_stdio_with_the_tenant() {
    let state = tempfile::tempdir().unwrap();
    let client = client(fake(state.path(), "ok", Some("contoso")));

    let tools = client.list_tools().await.unwrap();
    let inspected = client
        .call_tool("inspect", json!({ "names": [] }))
        .await
        .unwrap();

    let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
    assert_eq!(names, ["retrieve", "send_mail", "inspect"]);
    let seen: Value = serde_json::from_str(&text(&inspected)).unwrap();
    assert_eq!(seen["argv"], json!(["--tenant-id", "contoso"]));
    assert_eq!(
        log(state.path()).join(" "),
        "start initialize notifications/initialized tools/list tools/call"
    );
}

#[tokio::test]
async fn notifications_and_lines_that_are_not_json_before_a_response_are_skipped() {
    let state = tempfile::tempdir().unwrap();
    let client = client(fake(state.path(), "ok", None));

    let found = client.retrieve("budget", None, &[]).await.unwrap();

    assert_eq!(text(&found), "retrieve: budget");
}

#[tokio::test]
async fn a_cli_that_crashed_is_started_again_once_and_gets_the_handshake_again() {
    let state = tempfile::tempdir().unwrap();
    let client = client(fake(state.path(), "crash", None));

    let found = client.retrieve("budget", None, &[]).await.unwrap();

    assert_eq!(text(&found), "retrieve: budget");
    assert_eq!(count(state.path(), "start"), 2);
    assert_eq!(count(state.path(), "initialize"), 2);
    assert_eq!(count(state.path(), "notifications/initialized"), 2);
}

#[tokio::test]
async fn a_cli_that_crashes_again_is_not_started_a_third_time() {
    let state = tempfile::tempdir().unwrap();
    let transport = fake(state.path(), "crash-always", None);

    let failed = transport.send(call(1, "retrieve")).await.err().unwrap();

    assert!(failed.message.contains("stopped"), "{failed}");
    assert_eq!(count(state.path(), "start"), 2);
}

#[tokio::test]
async fn a_change_that_may_have_reached_a_crashed_cli_is_not_sent_again() {
    let state = tempfile::tempdir().unwrap();
    let client = client(fake(state.path(), "crash", None));

    let failed = client
        .call_tool("send_mail", json!({ "query": "budget" }))
        .await
        .unwrap_err();

    let failure = failed.downcast_ref::<WorkIqError>().unwrap();
    assert_eq!(failure.class, Class::UncertainChange);
    assert_eq!(count(state.path(), "start"), 1);
    assert_eq!(count(state.path(), "tools/call"), 1);
}

#[tokio::test]
async fn a_cli_that_wants_its_eula_accepted_says_to_run_accept_eula_yourself() {
    let state = tempfile::tempdir().unwrap();
    let client = client(fake(state.path(), "eula", None));

    let failed = client.list_tools().await.unwrap_err();

    let text = format!("{failed:#}");
    assert!(text.contains("EULA is not accepted"), "{text}");
    assert!(text.contains("run `workiq accept-eula` yourself"), "{text}");
    assert!(text.contains("never accepts it for you"), "{text}");
}

#[tokio::test]
async fn the_cli_does_not_inherit_the_credentials_the_providers_lose() {
    let state = tempfile::tempdir().unwrap();
    let client = client(fake(state.path(), "ok", None));
    let mut names: Vec<&str> = INHERITED_CREDENTIAL_VARS.to_vec();
    names.push("PATH");

    let inspected = client
        .call_tool("inspect", json!({ "names": names }))
        .await
        .unwrap();

    let seen: Value = serde_json::from_str(&text(&inspected)).unwrap();
    assert_eq!(
        seen["set"],
        json!(["PATH"]),
        "the CLI gets the agent's environment, minus the credentials"
    );
}

#[tokio::test]
async fn a_cli_account_has_no_token_and_connect_builds_its_client() {
    setup();
    let root = tempfile::tempdir().unwrap();
    let mut alice = Profile::open_in(root.path(), "alice").unwrap();
    alice.config.workiq = Some(cli_account(Some("contoso")));

    assert_eq!(NoToken.token().await.unwrap(), "");
    assert!(connect(&alice).is_ok());
}

#[tokio::test]
async fn the_cli_sign_in_saves_a_cli_account_without_a_token() {
    setup();
    let root = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let mut alice = Profile::open_in(root.path(), "alice").unwrap();

    let who = cli_login(Some("contoso"), state.path(), "ok")
        .run(&mut alice)
        .await
        .unwrap();

    assert_eq!(who, "Work IQ CLI");
    let saved = Profile::open_in(root.path(), "alice").unwrap();
    let account = saved.config.workiq.clone().unwrap();
    let WorkIqConnection::Cli { tenant } = account.connection else {
        panic!("not a CLI account");
    };
    assert_eq!(tenant.as_deref(), Some("contoso"));
    assert_eq!(account.upn, "Work IQ CLI");
    assert!(saved.workiq_refresh_token().is_err(), "no token is kept");
    assert_eq!(
        count(state.path(), "tools/list"),
        1,
        "the sign-in was checked"
    );

    let who = cli_login(Some("contoso"), state.path(), "user")
        .run(&mut alice)
        .await
        .unwrap();

    assert_eq!(who, "alice@contoso.example");
    let saved = Profile::open_in(root.path(), "alice").unwrap();
    assert_eq!(saved.config.workiq.unwrap().upn, "alice@contoso.example");
}

#[tokio::test]
async fn a_cli_sign_in_that_fails_is_signed_out_again() {
    setup();
    let root = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let mut alice = Profile::open_in(root.path(), "alice").unwrap();

    let failed = cli_login(None, state.path(), "eula")
        .run(&mut alice)
        .await
        .unwrap_err();

    let text = format!("{failed:#}");
    assert!(text.contains("signed out again"), "{text}");
    assert!(text.contains("workiq accept-eula"), "{text}");
    assert!(alice.config.workiq.is_none());
    let saved = Profile::open_in(root.path(), "alice").unwrap();
    assert!(saved.config.workiq.is_none());
}
