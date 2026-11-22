//! `proofkin workiq doctor` against a fake Work IQ (discovery document and MCP endpoint) and a
//! fake Entra token endpoint. Its own test binary because the tests use the file secret store,
//! chosen through the environment.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Once};

use axum::Router;
use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use proofkin::Profile;
use proofkin::profile::{WorkIqAccount, WorkIqConnection};
use proofkin::workiq::audit::{Audit, read};
use proofkin::workiq::doctor::{Check, Doctor, Outcome, Report};
use proofkin::workiq::{Endpoints, Who};
use serde_json::{Value, json};

const TENANT_ID: &str = "11111111-2222-3333-4444-555555555555";

static FILE_STORE: Once = Once::new();

fn use_file_secret_store() {
    FILE_STORE.call_once(|| {
        // SAFETY: every test calls this first and `call_once` makes the others wait for it,
        // so nothing in this binary reads the environment while it is changed.
        unsafe { std::env::set_var("PROOFKIN_SECRET_STORE", "file") };
    });
}

struct Fake {
    discovery: Mutex<(u16, Value)>,
    /// The claims of the access token Entra hands out.
    claims: Mutex<Value>,
    /// What Entra answers instead of a token.
    token_failure: Mutex<Option<(u16, Value)>>,
    tools: Mutex<Vec<String>>,
    /// What the MCP endpoint answers to `tools/list` and `tools/call` instead of working.
    list_failure: Mutex<Option<(u16, String)>>,
    call_failure: Mutex<Option<(u16, String)>>,
    calls: Mutex<Vec<Value>>,
    discovery_requests: AtomicUsize,
    token_requests: AtomicUsize,
    mcp_requests: AtomicUsize,
}

fn good_claims() -> Value {
    json!({
        "aud": "api://workiq.svc.cloud.microsoft",
        "tid": TENANT_ID,
        "scp": "WorkIQAgent.Ask",
    })
}

fn jwt(claims: &Value) -> String {
    let part = |value: &Value| data_encoding::BASE64URL_NOPAD.encode(value.to_string().as_bytes());
    format!(
        "{}.{}.signature",
        part(&json!({ "alg": "none" })),
        part(claims)
    )
}

async fn discovery(State(fake): State<Arc<Fake>>) -> impl IntoResponse {
    fake.discovery_requests.fetch_add(1, Ordering::SeqCst);
    let (status, body) = fake.discovery.lock().unwrap().clone();
    (StatusCode::from_u16(status).unwrap(), body.to_string())
}

async fn token(State(fake): State<Arc<Fake>>, UrlPath(_): UrlPath<String>) -> impl IntoResponse {
    fake.token_requests.fetch_add(1, Ordering::SeqCst);
    if let Some((status, body)) = fake.token_failure.lock().unwrap().clone() {
        return (StatusCode::from_u16(status).unwrap(), body.to_string());
    }
    let claims = fake.claims.lock().unwrap().clone();
    let body = json!({
        "token_type": "Bearer",
        "access_token": jwt(&claims),
        "expires_in": 3600,
    });
    (StatusCode::OK, body.to_string())
}

async fn mcp(State(fake): State<Arc<Fake>>, body: String) -> impl IntoResponse {
    fake.mcp_requests.fetch_add(1, Ordering::SeqCst);
    let json = [("content-type", "application/json".to_string())];
    let message: Value = serde_json::from_str(&body).unwrap();
    let id = message["id"].clone();
    let method = message["method"].as_str().unwrap();
    let failure = match method {
        "tools/list" => fake.list_failure.lock().unwrap().clone(),
        "tools/call" => fake.call_failure.lock().unwrap().clone(),
        _ => None,
    };
    if let Some((status, text)) = failure {
        return (StatusCode::from_u16(status).unwrap(), json, text);
    }
    let result = match method {
        "notifications/initialized" => return (StatusCode::ACCEPTED, json, String::new()),
        "initialize" => json!({
            "protocolVersion": "2025-06-18",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "fake-workiq", "version": "0" },
        }),
        "tools/list" => {
            let tools: Vec<Value> = fake
                .tools
                .lock()
                .unwrap()
                .iter()
                .map(|name| json!({ "name": name, "inputSchema": { "type": "object" } }))
                .collect();
            json!({ "tools": tools })
        }
        "tools/call" => {
            fake.calls.lock().unwrap().push(message["params"].clone());
            json!({
                "content": [{ "type": "text", "text": "ok" }],
                "structuredContent": {},
                "isError": false,
            })
        }
        other => panic!("unexpected method {other}"),
    };
    let reply = json!({ "jsonrpc": "2.0", "id": id, "result": result });
    (StatusCode::OK, json, reply.to_string())
}

async fn start() -> (Arc<Fake>, Endpoints) {
    let fake = Arc::new(Fake {
        discovery: Mutex::new((
            200,
            json!({
                "resource": "https://workiq.example/mcp",
                "authorization_servers": ["https://login.example/contoso"],
            }),
        )),
        claims: Mutex::new(good_claims()),
        token_failure: Mutex::new(None),
        tools: Mutex::new(vec!["retrieve".into(), "ask".into()]),
        list_failure: Mutex::new(None),
        call_failure: Mutex::new(None),
        calls: Mutex::new(Vec::new()),
        discovery_requests: AtomicUsize::new(0),
        token_requests: AtomicUsize::new(0),
        mcp_requests: AtomicUsize::new(0),
    });
    let app = Router::new()
        .route("/discovery", get(discovery))
        .route("/{tenant}/oauth2/v2.0/token", post(token))
        .route("/mcp", post(mcp))
        .with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let endpoints = Endpoints {
        mcp_url: format!("{base}/mcp"),
        authority: base.clone(),
        discovery_url: format!("{base}/discovery"),
    };
    (fake, endpoints)
}

fn account() -> WorkIqAccount {
    WorkIqAccount {
        connection: WorkIqConnection::Hosted {
            tenant: "contoso".into(),
            client_id: "app-id-1".into(),
        },
        upn: "alice@contoso.example".into(),
        display_name: "Alice Example".into(),
        tenant_id: TENANT_ID.into(),
        added: 1_700_000_000,
    }
}

/// A profile signed in to Work IQ (hosted) with a refresh token.
fn signed_in(root: &Path) -> Profile {
    use_file_secret_store();
    let mut profile = Profile::open_in(root, "alice").unwrap();
    profile.config.workiq = Some(account());
    profile.set_workiq_refresh_token("refresh-0").unwrap();
    profile.save().unwrap();
    profile
}

async fn diagnose(profile: &Profile, endpoints: &Endpoints) -> Report {
    Doctor::new(profile)
        .with_endpoints(endpoints.clone())
        .run()
        .await
}

fn names(report: &Report) -> Vec<&'static str> {
    report.checks.iter().map(|check| check.name).collect()
}

/// The check that failed, which must be the last one.
fn failed(report: &Report) -> &Check {
    let last = report.checks.last().unwrap();
    assert_eq!(last.outcome, Outcome::Failed, "{}", report.describe());
    assert!(!report.passed());
    assert_eq!(
        report
            .checks
            .iter()
            .filter(|c| c.outcome == Outcome::Failed)
            .count(),
        1,
        "the checks stop at the first failure"
    );
    last
}

fn fix_of(check: &Check) -> (Who, &str) {
    let fix = check.fix.as_ref().expect("a failed check has a fix");
    (fix.who, fix.action.as_str())
}

#[tokio::test]
async fn all_five_checks_pass_and_the_retrieve_is_audited_as_a_doctor_call() {
    let (fake, endpoints) = start().await;
    let root = tempfile::tempdir().unwrap();
    let alice = signed_in(root.path());

    let report = diagnose(&alice, &endpoints).await;

    assert!(report.passed(), "{}", report.describe());
    assert_eq!(
        names(&report),
        ["discovery", "sign-in", "token", "tools", "retrieve"]
    );
    assert!(report.checks.iter().all(|c| c.outcome == Outcome::Ok));
    assert!(report.checks.iter().all(|c| c.fix.is_none()));
    assert!(report.describe().contains("Work IQ is ready."));
    let calls = fake.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 1, "one retrieve and nothing else");
    assert_eq!(calls[0]["name"], "retrieve");
    assert_eq!(calls[0]["arguments"]["query"], "Proofkin connection check");
    let audit = Audit::for_profile(&alice);
    let entries = read(audit.path()).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].tool, "retrieve");
    assert_eq!(entries[0].purpose, "doctor");
    assert_eq!(entries[0].outcome, "ok");
    assert_eq!(audit.usage(None).today, 1, "it counts against the budget");
    assert_eq!(fake.discovery_requests.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn an_unreachable_discovery_document_fails_first_and_nothing_else_runs() {
    let (fake, endpoints) = start().await;
    *fake.discovery.lock().unwrap() = (503, json!({ "error": "down" }));
    let root = tempfile::tempdir().unwrap();
    let alice = signed_in(root.path());

    let report = diagnose(&alice, &endpoints).await;

    assert_eq!(names(&report), ["discovery"]);
    let check = failed(&report);
    assert!(check.detail.contains("503"), "{}", check.detail);
    let (who, action) = fix_of(check);
    assert_eq!(who, Who::User);
    assert!(action.contains("network"), "{action}");
    assert_eq!(fake.token_requests.load(Ordering::SeqCst), 0);
    assert_eq!(fake.mcp_requests.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_discovery_document_without_the_resource_and_its_servers_fails() {
    let (fake, endpoints) = start().await;
    *fake.discovery.lock().unwrap() = (200, json!({ "resource": "https://workiq.example/mcp" }));
    let root = tempfile::tempdir().unwrap();
    let alice = signed_in(root.path());

    let report = diagnose(&alice, &endpoints).await;

    assert_eq!(names(&report), ["discovery"]);
    let check = failed(&report);
    assert!(check.detail.contains("authorization_servers"));
    assert_eq!(fix_of(check).0, Who::Support);
}

#[tokio::test]
async fn a_profile_that_is_not_signed_in_is_told_to_sign_in() {
    let (fake, endpoints) = start().await;
    use_file_secret_store();
    let root = tempfile::tempdir().unwrap();
    let alice = Profile::open_in(root.path(), "alice").unwrap();

    let report = diagnose(&alice, &endpoints).await;

    assert_eq!(names(&report), ["discovery", "sign-in"]);
    assert_eq!(report.checks[0].outcome, Outcome::Skipped);
    let check = failed(&report);
    let (who, action) = fix_of(check);
    assert_eq!(who, Who::User);
    assert!(
        action.contains("proofkin auth login workiq --profile alice"),
        "{action}"
    );
    assert_eq!(fake.discovery_requests.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_hosted_account_without_its_refresh_token_is_told_to_sign_in_again() {
    let (fake, endpoints) = start().await;
    let root = tempfile::tempdir().unwrap();
    let alice = signed_in(root.path());
    alice.delete_workiq_token().unwrap();

    let report = diagnose(&alice, &endpoints).await;

    assert_eq!(names(&report), ["discovery", "sign-in"]);
    let check = failed(&report);
    assert!(check.detail.contains("token"), "{}", check.detail);
    let (who, action) = fix_of(check);
    assert_eq!(who, Who::User);
    assert!(action.contains("proofkin auth login workiq"), "{action}");
    assert_eq!(fake.token_requests.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_token_for_another_audience_points_at_the_service_principal() {
    let (fake, endpoints) = start().await;
    fake.claims.lock().unwrap()["aud"] = json!("https://graph.microsoft.com");
    let root = tempfile::tempdir().unwrap();
    let alice = signed_in(root.path());

    let report = diagnose(&alice, &endpoints).await;

    assert_eq!(names(&report), ["discovery", "sign-in", "token"]);
    let check = failed(&report);
    assert!(check.detail.contains("graph.microsoft.com"));
    let (who, action) = fix_of(check);
    assert_eq!(who, Who::Admin);
    assert!(
        action.contains("az ad sp create --id fdcc1f02-fc51-4226-8753-f668596af7f7"),
        "{action}"
    );
    assert_eq!(fake.mcp_requests.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn the_resource_app_id_is_also_an_accepted_audience() {
    let (fake, endpoints) = start().await;
    fake.claims.lock().unwrap()["aud"] = json!("fdcc1f02-fc51-4226-8753-f668596af7f7");
    let root = tempfile::tempdir().unwrap();
    let alice = signed_in(root.path());

    let report = diagnose(&alice, &endpoints).await;

    assert!(report.passed(), "{}", report.describe());
}

#[tokio::test]
async fn a_token_for_another_tenant_is_told_to_sign_in_with_the_right_account() {
    let (fake, endpoints) = start().await;
    fake.claims.lock().unwrap()["tid"] = json!("99999999-0000-0000-0000-000000000000");
    let root = tempfile::tempdir().unwrap();
    let alice = signed_in(root.path());

    let report = diagnose(&alice, &endpoints).await;

    let check = failed(&report);
    assert_eq!(check.name, "token");
    assert!(check.detail.contains("99999999-0000"), "{}", check.detail);
    let (who, action) = fix_of(check);
    assert_eq!(who, Who::User);
    assert!(action.contains(TENANT_ID), "{action}");
}

#[tokio::test]
async fn a_token_without_the_ask_scope_needs_an_admin_to_grant_consent() {
    let (fake, endpoints) = start().await;
    fake.claims.lock().unwrap()["scp"] = json!("User.Read");
    let root = tempfile::tempdir().unwrap();
    let alice = signed_in(root.path());

    let report = diagnose(&alice, &endpoints).await;

    let check = failed(&report);
    assert_eq!(check.name, "token");
    assert!(check.detail.contains("WorkIQAgent.Ask"));
    let (who, action) = fix_of(check);
    assert_eq!(who, Who::Admin);
    let consent_url = format!(
        "{}/contoso/adminconsent?client_id=app-id-1",
        endpoints.authority
    );
    assert!(action.contains(&consent_url), "{action}");
}

#[tokio::test]
async fn entra_refusals_get_the_fix_that_matches_them() {
    let (fake, endpoints) = start().await;
    let root = tempfile::tempdir().unwrap();
    let alice = signed_in(root.path());
    let consent_url = format!(
        "{}/contoso/adminconsent?client_id=app-id-1",
        endpoints.authority
    );
    let cases = [
        (
            json!({ "error": "invalid_grant", "error_description": "AADSTS70008: expired" }),
            Who::User,
            "proofkin auth login workiq --profile alice".to_string(),
        ),
        (
            json!({ "error": "consent_required", "error_description": "AADSTS65001: no consent" }),
            Who::Admin,
            consent_url,
        ),
        (
            json!({ "error": "invalid_resource", "error_description": "AADSTS500011: no resource" }),
            Who::Admin,
            "az ad sp create --id fdcc1f02-fc51-4226-8753-f668596af7f7".to_string(),
        ),
        (
            json!({ "error": "access_denied", "error_description": "AADSTS50105: not assigned" }),
            Who::Admin,
            "Enterprise applications, Users and groups".to_string(),
        ),
    ];
    for (body, who, expected) in cases {
        *fake.token_failure.lock().unwrap() = Some((400, body.clone()));

        let report = diagnose(&alice, &endpoints).await;

        let check = failed(&report);
        assert_eq!(check.name, "token", "{body}");
        let (actual_who, action) = fix_of(check);
        assert_eq!(actual_who, who, "{body}: {action}");
        assert!(action.contains(&expected), "{body}: {action}");
    }
}

#[tokio::test]
async fn a_work_iq_that_refuses_the_token_is_told_to_sign_in_again() {
    let (fake, endpoints) = start().await;
    *fake.list_failure.lock().unwrap() = Some((401, "{}".into()));
    let root = tempfile::tempdir().unwrap();
    let alice = signed_in(root.path());

    let report = diagnose(&alice, &endpoints).await;

    assert_eq!(names(&report), ["discovery", "sign-in", "token", "tools"]);
    let (who, action) = fix_of(failed(&report));
    assert_eq!(who, Who::User);
    assert!(
        action.contains("proofkin auth login workiq --profile alice"),
        "{action}"
    );
}

#[tokio::test]
async fn a_tool_list_without_retrieve_needs_the_mcp_policy_changed() {
    let (fake, endpoints) = start().await;
    *fake.tools.lock().unwrap() = vec!["ask".into()];
    let root = tempfile::tempdir().unwrap();
    let alice = signed_in(root.path());

    let report = diagnose(&alice, &endpoints).await;

    let check = failed(&report);
    assert_eq!(check.name, "tools");
    assert!(check.detail.contains("retrieve"));
    let (who, action) = fix_of(check);
    assert_eq!(who, Who::Admin);
    assert!(action.contains("Agents, Tools, Work IQ MCP"), "{action}");
    assert!(fake.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_retrieve_that_billing_stops_needs_a_billing_admin() {
    let (fake, endpoints) = start().await;
    *fake.call_failure.lock().unwrap() = Some((402, r#"{"error":"payment required"}"#.into()));
    let root = tempfile::tempdir().unwrap();
    let alice = signed_in(root.path());

    let report = diagnose(&alice, &endpoints).await;

    let check = failed(&report);
    assert_eq!(check.name, "retrieve");
    let (who, action) = fix_of(check);
    assert_eq!(who, Who::Admin);
    assert!(action.contains("billing admin"), "{action}");
    assert!(action.contains("spending policy"), "{action}");
    assert!(action.contains("Microsoft 365 admin center"), "{action}");
    let entries = read(Audit::for_profile(&alice).path()).unwrap();
    assert_eq!(entries[0].purpose, "doctor");
    assert_eq!(entries[0].outcome, "billing");
}

#[tokio::test]
async fn a_retrieve_the_mcp_policy_denies_needs_an_admin_to_change_the_policy() {
    let (fake, endpoints) = start().await;
    *fake.call_failure.lock().unwrap() = Some((403, r#"{"error":"forbidden"}"#.into()));
    let root = tempfile::tempdir().unwrap();
    let alice = signed_in(root.path());

    let report = diagnose(&alice, &endpoints).await;

    let check = failed(&report);
    assert_eq!(check.name, "retrieve");
    let (who, action) = fix_of(check);
    assert_eq!(who, Who::Admin);
    assert!(action.contains("Agents, Tools, Work IQ MCP"), "{action}");
}

#[tokio::test]
async fn a_user_who_is_not_assigned_to_the_app_is_sent_to_the_admin() {
    let (fake, endpoints) = start().await;
    let body = r#"{"error":"AADSTS50105: The user is not assigned to a role"}"#;
    *fake.call_failure.lock().unwrap() = Some((403, body.into()));
    let root = tempfile::tempdir().unwrap();
    let alice = signed_in(root.path());

    let report = diagnose(&alice, &endpoints).await;

    let check = failed(&report);
    assert_eq!(check.name, "retrieve");
    let (who, action) = fix_of(check);
    assert_eq!(who, Who::Admin);
    assert!(
        action.contains("Enterprise applications, Users and groups"),
        "{action}"
    );
}

#[tokio::test]
async fn the_json_report_has_each_checks_name_result_and_fix() {
    let (fake, endpoints) = start().await;
    *fake.call_failure.lock().unwrap() = Some((402, "{}".into()));
    let root = tempfile::tempdir().unwrap();
    let alice = signed_in(root.path());

    let report = diagnose(&alice, &endpoints).await;
    let json = report.to_json();

    assert_eq!(json["profile"], "alice");
    assert_eq!(json["ok"], false);
    let checks = json["checks"].as_array().unwrap();
    let listed: Vec<(&str, &str)> = checks
        .iter()
        .map(|c| (c["name"].as_str().unwrap(), c["result"].as_str().unwrap()))
        .collect();
    assert_eq!(
        listed,
        [
            ("discovery", "ok"),
            ("sign-in", "ok"),
            ("token", "ok"),
            ("tools", "ok"),
            ("retrieve", "failed"),
        ]
    );
    assert!(checks[0]["fix"].is_null());
    assert!(checks[0]["detail"].as_str().is_some());
    assert_eq!(checks[4]["fix"]["who"], "admin");
    assert!(
        checks[4]["fix"]["action"]
            .as_str()
            .unwrap()
            .contains("billing")
    );

    *fake.call_failure.lock().unwrap() = None;
    let json = diagnose(&alice, &endpoints).await.to_json();
    assert_eq!(json["ok"], true);
    assert_eq!(json["checks"].as_array().unwrap().len(), 5);
}
