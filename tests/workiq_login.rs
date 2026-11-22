//! `proofkin auth login workiq` against fake Entra authorize, token and devicecode endpoints
//! and a fake Work IQ MCP endpoint. Its own test binary because the tests use the file secret
//! store, chosen through the environment.

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex, Once};
use std::time::Duration;

use anyhow::Result;
use axum::Router;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get as get_route, post};
use proofkin::Profile;
use proofkin::profile::WorkIqConnection;
use proofkin::workiq::login::{Browser, Flow, Login};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::task::JoinHandle;

static FILE_STORE: Once = Once::new();

fn use_file_secret_store() {
    FILE_STORE.call_once(|| {
        // SAFETY: every test calls this first and `call_once` makes the others wait for it,
        // so nothing in this binary reads the environment while it is changed.
        unsafe { std::env::set_var("PROOFKIN_SECRET_STORE", "file") };
    });
}

const TENANT_ID: &str = "11111111-2222-3333-4444-555555555555";

#[derive(Default)]
struct Fake {
    /// Replaces the state the authorize endpoint hands back.
    forged_state: Mutex<Option<String>>,
    /// Makes the authorize endpoint redirect with an error instead of a code.
    redirect_error: Mutex<Option<(String, String)>>,
    authorized: Mutex<Vec<HashMap<String, String>>>,
    token_answers: Mutex<VecDeque<(u16, Value)>>,
    token_requests: Mutex<Vec<HashMap<String, String>>>,
    device_answer: Mutex<Option<(u16, Value)>>,
    device_requests: Mutex<Vec<HashMap<String, String>>>,
    /// The status Work IQ answers with; 200 is a working sign-in.
    mcp_status: Mutex<u16>,
    mcp_bearers: Mutex<Vec<String>>,
}

fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                out.push(u8::from_str_radix(&text[i + 1..i + 3], 16).unwrap());
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8(out).unwrap()
}

fn fields(body: &str) -> HashMap<String, String> {
    body.split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(name, value)| (decode(name), decode(value)))
        .collect()
}

async fn authorize(
    State(fake): State<Arc<Fake>>,
    UrlPath(_tenant): UrlPath<String>,
    Query(query): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    fake.authorized.lock().unwrap().push(query.clone());
    let state = fake
        .forged_state
        .lock()
        .unwrap()
        .clone()
        .unwrap_or_else(|| query["state"].clone());
    let answer = match fake.redirect_error.lock().unwrap().clone() {
        Some((error, description)) => format!(
            "error={error}&error_description={}&state={state}",
            description.replace(' ', "+")
        ),
        None => format!("code=auth-code&state={state}"),
    };
    let location = format!("{}?{answer}", query["redirect_uri"]);
    (StatusCode::FOUND, [("location", location)])
}

async fn token(
    State(fake): State<Arc<Fake>>,
    UrlPath(_tenant): UrlPath<String>,
    body: String,
) -> impl IntoResponse {
    fake.token_requests.lock().unwrap().push(fields(&body));
    let (status, body) = fake
        .token_answers
        .lock()
        .unwrap()
        .pop_front()
        .unwrap_or_else(|| (500, json!({ "error": "no_answer_scripted" })));
    (
        StatusCode::from_u16(status).unwrap(),
        [("content-type", "application/json")],
        body.to_string(),
    )
}

async fn devicecode(
    State(fake): State<Arc<Fake>>,
    UrlPath(_tenant): UrlPath<String>,
    body: String,
) -> impl IntoResponse {
    fake.device_requests.lock().unwrap().push(fields(&body));
    let (status, body) = fake
        .device_answer
        .lock()
        .unwrap()
        .clone()
        .unwrap_or_else(|| (500, json!({ "error": "no_answer_scripted" })));
    (
        StatusCode::from_u16(status).unwrap(),
        [("content-type", "application/json")],
        body.to_string(),
    )
}

async fn mcp(State(fake): State<Arc<Fake>>, headers: HeaderMap, body: String) -> impl IntoResponse {
    let bearer = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    fake.mcp_bearers.lock().unwrap().push(bearer);
    let status = *fake.mcp_status.lock().unwrap();
    if status != 200 {
        return (
            StatusCode::from_u16(status).unwrap(),
            [("content-type", "application/json".to_string())],
            json!({ "error": "denied by policy" }).to_string(),
        );
    }
    let message: Value = serde_json::from_str(&body).unwrap();
    let id = message["id"].clone();
    let reply = match message["method"].as_str().unwrap() {
        "notifications/initialized" => {
            return (
                StatusCode::ACCEPTED,
                [("content-type", "application/json".to_string())],
                String::new(),
            );
        }
        "initialize" => json!({ "jsonrpc": "2.0", "id": id, "result": {
            "protocolVersion": "2025-06-18",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "fake-workiq", "version": "0" },
        }}),
        "tools/list" => json!({ "jsonrpc": "2.0", "id": id, "result": { "tools": [
            { "name": "retrieve", "inputSchema": { "type": "object" } },
        ]}}),
        other => panic!("unexpected method {other}"),
    };
    (
        StatusCode::OK,
        [("content-type", "application/json".to_string())],
        reply.to_string(),
    )
}

async fn start() -> (Arc<Fake>, String) {
    let fake = Arc::new(Fake {
        mcp_status: Mutex::new(200),
        ..Default::default()
    });
    let app = Router::new()
        .route("/{tenant}/oauth2/v2.0/authorize", get_route(authorize))
        .route("/{tenant}/oauth2/v2.0/token", post(token))
        .route("/{tenant}/oauth2/v2.0/devicecode", post(devicecode))
        .route("/mcp", post(mcp))
        .with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (fake, url)
}

/// A plain GET: the status, the `Location` header and the body.
async fn get(url: &str) -> (u16, Option<String>, String) {
    let rest = url.strip_prefix("http://").unwrap();
    let split = rest.find(['/', '?']).unwrap_or(rest.len());
    let host = rest[..split].replace("localhost", "127.0.0.1");
    let path = match &rest[split..] {
        "" => "/".to_string(),
        tail if tail.starts_with('?') => format!("/{tail}"),
        tail => tail.to_string(),
    };
    let mut stream = TcpStream::connect(&host).await.unwrap();
    let request = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut reply = String::new();
    stream.read_to_string(&mut reply).await.unwrap();
    let (head, body) = reply.split_once("\r\n\r\n").unwrap();
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    let location = head
        .lines()
        .find_map(|line| line.strip_prefix("location: "))
        .map(str::to_string);
    (status, location, body.to_string())
}

/// Stands in for the browser: follows the authorize redirect to the loopback address.
#[derive(Default)]
struct Hook {
    opened: Mutex<Vec<String>>,
    visit: Mutex<Option<JoinHandle<(u16, String)>>>,
}

impl Browser for Hook {
    fn open(&self, url: &str) -> Result<()> {
        self.opened.lock().unwrap().push(url.to_string());
        let url = url.to_string();
        *self.visit.lock().unwrap() = Some(tokio::spawn(async move {
            let (status, location, _) = get(&url).await;
            assert_eq!(status, 302, "the authorize endpoint redirects");
            let (status, _, page) = get(&location.expect("a Location header")).await;
            (status, page)
        }));
        Ok(())
    }
}

impl Hook {
    /// What the loopback listener answered the browser: the status and the page.
    async fn page(&self) -> (u16, String) {
        let visit = self
            .visit
            .lock()
            .unwrap()
            .take()
            .expect("the browser opened");
        visit.await.unwrap()
    }
}

fn id_token() -> String {
    let part = |value: Value| data_encoding::BASE64URL_NOPAD.encode(value.to_string().as_bytes());
    format!(
        "{}.{}.signature",
        part(json!({ "alg": "none" })),
        part(json!({
            "preferred_username": "alice@contoso.example",
            "name": "Alice Example",
            "tid": TENANT_ID,
        }))
    )
}

fn signed_in_tokens(fake: &Fake) {
    fake.token_answers.lock().unwrap().push_back((
        200,
        json!({
            "token_type": "Bearer",
            "access_token": "access-1",
            "refresh_token": "refresh-1",
            "id_token": id_token(),
            "expires_in": 3600,
        }),
    ));
}

fn profile(root: &Path) -> Profile {
    use_file_secret_store();
    Profile::open_in(root, "alice").unwrap()
}

type Said = Arc<Mutex<Vec<String>>>;

fn login(url: &str, hook: Arc<Hook>) -> (Login, Said) {
    let said = Said::default();
    let sink = said.clone();
    let login = Login::new("contoso", "app-id-1")
        .with_authority(url)
        .with_mcp_url(&format!("{url}/mcp"))
        .with_browser(hook)
        .with_output(Arc::new(move |text| {
            sink.lock().unwrap().push(text.to_string())
        }))
        .with_timing(Duration::ZERO, Duration::from_secs(20));
    (login, said)
}

fn device_code(fake: &Fake) {
    *fake.device_answer.lock().unwrap() = Some((
        200,
        json!({
            "device_code": "device-1",
            "user_code": "ABCD-EFGH",
            "verification_uri": "https://microsoft.com/devicelogin",
            "expires_in": 900,
            "interval": 0,
            "message": "To sign in, use a web browser to open https://microsoft.com/devicelogin and enter the code ABCD-EFGH",
        }),
    ));
}

#[tokio::test]
async fn the_browser_flow_signs_in_with_pkce_and_checks_work_iq() {
    let (fake, url) = start().await;
    let root = tempfile::tempdir().unwrap();
    let mut alice = profile(root.path());
    signed_in_tokens(&fake);
    let hook = Arc::new(Hook::default());
    let (login, said) = login(&url, hook.clone());

    let who = login.run(&mut alice, Flow::Browser).await.unwrap();

    assert_eq!(who, "Alice Example (alice@contoso.example)");
    let (status, page) = hook.page().await;
    assert_eq!(status, 200);
    assert!(page.contains("close this window"), "{page}");

    let authorized = fake.authorized.lock().unwrap();
    assert_eq!(authorized.len(), 1);
    let query = &authorized[0];
    assert_eq!(query["client_id"], "app-id-1");
    assert_eq!(query["response_type"], "code");
    assert_eq!(query["response_mode"], "query");
    assert_eq!(
        query["scope"],
        "api://workiq.svc.cloud.microsoft/WorkIQAgent.Ask offline_access openid profile"
    );
    assert_eq!(query["code_challenge_method"], "S256");
    assert!(!query.contains_key("resource"), "Entra rejects a resource");
    let redirect = &query["redirect_uri"];
    assert!(redirect.starts_with("http://localhost:"), "{redirect}");
    assert!(redirect[17..].parse::<u16>().is_ok(), "{redirect}");

    let requests = fake.token_requests.lock().unwrap();
    assert_eq!(requests.len(), 1, "the check reuses the access token");
    let exchange = &requests[0];
    assert_eq!(exchange["grant_type"], "authorization_code");
    assert_eq!(exchange["client_id"], "app-id-1");
    assert_eq!(exchange["code"], "auth-code");
    assert_eq!(exchange["redirect_uri"], *redirect);
    let verifier = &exchange["code_verifier"];
    assert!(
        verifier.len() >= 43,
        "a random verifier of at least 43 characters"
    );
    assert_eq!(
        data_encoding::BASE64URL_NOPAD.encode(&Sha256::digest(verifier.as_bytes())),
        query["code_challenge"]
    );
    assert!(query["state"].len() >= 16);

    assert_eq!(
        *fake.mcp_bearers.lock().unwrap().first().unwrap(),
        "Bearer access-1"
    );
    let opened = hook.opened.lock().unwrap();
    assert_eq!(opened.len(), 1);
    assert!(opened[0].starts_with(&format!("{url}/contoso/oauth2/v2.0/authorize?")));
    assert!(
        said.lock()
            .unwrap()
            .iter()
            .any(|text| text.contains(&opened[0])),
        "the URL is printed too"
    );

    let saved = Profile::open_in(root.path(), "alice").unwrap();
    let account = saved.config.workiq.clone().unwrap();
    assert_eq!(account.upn, "alice@contoso.example");
    assert_eq!(account.display_name, "Alice Example");
    assert_eq!(account.tenant_id, TENANT_ID);
    assert_eq!(
        account.connection,
        WorkIqConnection::Hosted {
            tenant: "contoso".into(),
            client_id: "app-id-1".into()
        }
    );
    assert_eq!(saved.workiq_refresh_token().unwrap(), "refresh-1");
    let file = std::fs::read_to_string(saved.dir.join("profile.json")).unwrap();
    assert!(!file.contains("refresh-1") && !file.contains("access-1"));
}

#[tokio::test]
async fn a_sign_in_answer_with_the_wrong_state_is_refused() {
    let (fake, url) = start().await;
    let root = tempfile::tempdir().unwrap();
    let mut alice = profile(root.path());
    signed_in_tokens(&fake);
    *fake.forged_state.lock().unwrap() = Some("forged".into());
    let hook = Arc::new(Hook::default());
    let (login, _) = login(&url, hook.clone());

    let error = login.run(&mut alice, Flow::Browser).await.unwrap_err();

    assert!(format!("{error:#}").contains("wrong state"), "{error:#}");
    let (status, page) = hook.page().await;
    assert_eq!(status, 400);
    assert!(!page.contains("signed in"), "{page}");
    assert!(
        fake.token_requests.lock().unwrap().is_empty(),
        "the code was not used"
    );
    assert!(
        Profile::open_in(root.path(), "alice")
            .unwrap()
            .config
            .workiq
            .is_none()
    );
}

#[tokio::test]
async fn the_device_code_flow_waits_through_pending_and_slow_down_then_signs_in() {
    let (fake, url) = start().await;
    let root = tempfile::tempdir().unwrap();
    let mut alice = profile(root.path());
    device_code(&fake);
    for error in ["authorization_pending", "slow_down"] {
        fake.token_answers
            .lock()
            .unwrap()
            .push_back((400, json!({ "error": error })));
    }
    signed_in_tokens(&fake);
    let hook = Arc::new(Hook::default());
    let (login, said) = login(&url, hook.clone());

    let who = login.run(&mut alice, Flow::DeviceCode).await.unwrap();

    assert_eq!(who, "Alice Example (alice@contoso.example)");
    assert!(
        hook.opened.lock().unwrap().is_empty(),
        "no browser for a device code"
    );
    let device = fake.device_requests.lock().unwrap();
    assert_eq!(device.len(), 1);
    assert_eq!(device[0]["client_id"], "app-id-1");
    assert!(device[0]["scope"].contains("WorkIQAgent.Ask"));
    let polls = fake.token_requests.lock().unwrap();
    assert_eq!(polls.len(), 3, "pending, slow down, then the tokens");
    for poll in polls.iter() {
        assert_eq!(
            poll["grant_type"],
            "urn:ietf:params:oauth:grant-type:device_code"
        );
        assert_eq!(poll["device_code"], "device-1");
    }
    assert!(
        said.lock()
            .unwrap()
            .iter()
            .any(|text| text.contains("ABCD-EFGH"))
    );
    let saved = Profile::open_in(root.path(), "alice").unwrap();
    assert_eq!(saved.workiq_refresh_token().unwrap(), "refresh-1");
    assert_eq!(
        *fake.mcp_bearers.lock().unwrap().first().unwrap(),
        "Bearer access-1"
    );
}

#[tokio::test]
async fn an_expired_or_declined_device_code_ends_the_sign_in() {
    for (error, message) in [
        ("expired_token", "expired"),
        ("authorization_declined", "declined"),
    ] {
        let (fake, url) = start().await;
        let root = tempfile::tempdir().unwrap();
        let mut alice = profile(root.path());
        device_code(&fake);
        fake.token_answers
            .lock()
            .unwrap()
            .push_back((400, json!({ "error": error })));
        let (login, _) = login(&url, Arc::new(Hook::default()));

        let failed = login.run(&mut alice, Flow::DeviceCode).await.unwrap_err();

        assert!(
            format!("{failed:#}").contains(message),
            "{error}: {failed:#}"
        );
        assert!(
            Profile::open_in(root.path(), "alice")
                .unwrap()
                .config
                .workiq
                .is_none()
        );
    }
}

#[tokio::test]
async fn entra_errors_become_clear_messages() {
    let cases = [
        (
            "invalid_grant",
            "AADSTS65001: The user or administrator has not consented to use the application.",
            vec![
                "admin must grant consent",
                "contoso/adminconsent?client_id=app-id-1",
            ],
        ),
        (
            "consent_required",
            "Needs consent.",
            vec![
                "admin must grant consent",
                "contoso/adminconsent?client_id=app-id-1",
            ],
        ),
        (
            "invalid_client",
            "AADSTS50105: Your administrator has configured the application to block users.",
            vec!["not assigned to the app"],
        ),
        (
            "unauthorized_client",
            "AADSTS700016: Application with identifier 'x' was not found in the directory.",
            vec!["client id or tenant", "app-id-1", "contoso"],
        ),
        (
            "invalid_request",
            "AADSTS90002: Tenant 'contoso' not found.",
            vec!["client id or tenant"],
        ),
        (
            "invalid_client",
            "AADSTS7000218: The request body must contain client_assertion or client_secret.",
            vec!["Allow public client flows"],
        ),
    ];
    for (code, description, wanted) in cases {
        let (fake, url) = start().await;
        let root = tempfile::tempdir().unwrap();
        let mut alice = profile(root.path());
        device_code(&fake);
        fake.token_answers.lock().unwrap().push_back((
            400,
            json!({ "error": code, "error_description": description }),
        ));
        let (login, _) = login(&url, Arc::new(Hook::default()));

        let failed = login.run(&mut alice, Flow::DeviceCode).await.unwrap_err();

        let text = format!("{failed:#}");
        for part in wanted {
            assert!(
                text.contains(part),
                "{description}: '{part}' not in: {text}"
            );
        }
        assert!(
            Profile::open_in(root.path(), "alice")
                .unwrap()
                .config
                .workiq
                .is_none()
        );
    }
}

#[tokio::test]
async fn an_error_in_the_browser_redirect_is_explained_too() {
    let (fake, url) = start().await;
    let root = tempfile::tempdir().unwrap();
    let mut alice = profile(root.path());
    *fake.redirect_error.lock().unwrap() = Some((
        "access_denied".into(),
        "AADSTS50105: The signed in user is not assigned to a role for the application.".into(),
    ));
    let hook = Arc::new(Hook::default());
    let (login, _) = login(&url, hook.clone());

    let failed = login.run(&mut alice, Flow::Browser).await.unwrap_err();

    assert!(
        format!("{failed:#}").contains("not assigned to the app"),
        "{failed:#}"
    );
    let (status, page) = hook.page().await;
    assert_eq!(status, 400);
    assert!(page.contains("not assigned to the app"), "{page}");
    assert!(fake.token_requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_sign_in_work_iq_refuses_is_signed_out_again() {
    let (fake, url) = start().await;
    let root = tempfile::tempdir().unwrap();
    let mut alice = profile(root.path());
    device_code(&fake);
    signed_in_tokens(&fake);
    *fake.mcp_status.lock().unwrap() = 403;
    let (login, _) = login(&url, Arc::new(Hook::default()));

    let failed = login.run(&mut alice, Flow::DeviceCode).await.unwrap_err();

    let text = format!("{failed:#}");
    assert!(text.contains("signed out again"), "{text}");
    assert!(
        !fake.mcp_bearers.lock().unwrap().is_empty(),
        "Work IQ was asked"
    );
    assert!(alice.config.workiq.is_none());
    let saved = Profile::open_in(root.path(), "alice").unwrap();
    assert!(
        saved.config.workiq.is_none(),
        "the account was removed from profile.json"
    );
    assert!(
        saved.workiq_refresh_token().is_err(),
        "and its token from the secret store"
    );
}
