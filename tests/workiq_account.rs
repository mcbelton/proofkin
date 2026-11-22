//! The Work IQ account and its token source, against a fake Entra token endpoint. Its own test
//! binary because the tests use the file secret store, chosen through the environment.

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::time::Duration;

use axum::Router;
use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::post;
use proofkin::Profile;
use proofkin::profile::{WorkIqAccount, WorkIqConnection};
use proofkin::workers::{WorkerSpec, create};
use proofkin::workiq::account::{HostedTokens, SCOPE, describe, logout};
use proofkin::workiq::{Class, TokenSource, Who, WorkIqError};
use serde_json::{Value, json};

static FILE_STORE: Once = Once::new();

fn use_file_secret_store() {
    FILE_STORE.call_once(|| {
        // SAFETY: every test calls this first and `call_once` makes the others wait for it,
        // so nothing in this binary reads the environment while it is changed.
        unsafe { std::env::set_var("PROOFKIN_SECRET_STORE", "file") };
    });
}

#[derive(Default)]
struct Entra {
    /// What it answers, in order: a status and a JSON body.
    answers: Mutex<VecDeque<(u16, Value)>>,
    /// The tenant in the path and the form fields of each request.
    seen: Mutex<Vec<(String, HashMap<String, String>)>>,
    delay: Mutex<Duration>,
    count: AtomicUsize,
}

impl Entra {
    fn answer(&self, status: u16, body: Value) {
        self.answers.lock().unwrap().push_back((status, body));
    }

    fn token(&self, access: &str, expires_in: u64, refresh: Option<&str>) {
        let mut body = json!({
            "token_type": "Bearer",
            "access_token": access,
            "expires_in": expires_in,
        });
        if let Some(refresh) = refresh {
            body["refresh_token"] = json!(refresh);
        }
        self.answer(200, body);
    }

    fn requests(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }

    fn field(&self, request: usize, name: &str) -> String {
        self.seen.lock().unwrap()[request].1[name].clone()
    }
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

async fn handle(
    State(entra): State<Arc<Entra>>,
    UrlPath(tenant): UrlPath<String>,
    body: String,
) -> impl IntoResponse {
    let fields = body
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(name, value)| (decode(name), decode(value)))
        .collect();
    entra.seen.lock().unwrap().push((tenant, fields));
    entra.count.fetch_add(1, Ordering::SeqCst);
    let delay = *entra.delay.lock().unwrap();
    tokio::time::sleep(delay).await;
    let (status, body) = entra
        .answers
        .lock()
        .unwrap()
        .pop_front()
        .unwrap_or_else(|| (500, json!({ "error": "no_answer_scripted" })));
    (StatusCode::from_u16(status).unwrap(), body.to_string())
}

async fn start() -> (Arc<Entra>, String) {
    let entra = Arc::new(Entra::default());
    let app = Router::new()
        .route("/{tenant}/oauth2/v2.0/token", post(handle))
        .with_state(entra.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (entra, url)
}

fn account(connection: WorkIqConnection) -> WorkIqAccount {
    WorkIqAccount {
        connection,
        upn: "alice@contoso.example".into(),
        display_name: "Alice Example".into(),
        tenant_id: "11111111-2222-3333-4444-555555555555".into(),
        added: 1_700_000_000,
    }
}

fn hosted() -> WorkIqConnection {
    WorkIqConnection::Hosted {
        tenant: "contoso".into(),
        client_id: "app-id-1".into(),
    }
}

/// A profile signed in to Work IQ with the refresh token `refresh_token`.
fn signed_in(root: &Path, name: &str, refresh_token: &str) -> Profile {
    use_file_secret_store();
    let mut profile = Profile::open_in(root, name).unwrap();
    profile.config.workiq = Some(account(hosted()));
    profile.set_workiq_refresh_token(refresh_token).unwrap();
    profile.save().unwrap();
    profile
}

fn tokens(profile: &Profile, authority: &str) -> HostedTokens {
    HostedTokens::new(profile)
        .unwrap()
        .with_authority(authority)
}

fn failure(error: anyhow::Error) -> WorkIqError {
    error.downcast::<WorkIqError>().expect("a Work IQ error")
}

#[tokio::test]
async fn a_token_is_refreshed_five_minutes_before_it_expires() {
    let (entra, url) = start().await;
    let root = tempfile::tempdir().unwrap();
    let alice = signed_in(root.path(), "alice", "refresh-0");
    // Four minutes of life left is inside the five-minute margin; an hour is not.
    entra.token("access-1", 240, None);
    entra.token("access-2", 3600, None);
    let tokens = tokens(&alice, &url);

    assert_eq!(tokens.token().await.unwrap(), "access-1");
    assert_eq!(entra.requests(), 1);
    assert_eq!(tokens.token().await.unwrap(), "access-2");
    assert_eq!(entra.requests(), 2, "the first token was about to expire");
    assert_eq!(tokens.token().await.unwrap(), "access-2");
    assert_eq!(entra.requests(), 2, "the second one has an hour left");

    let seen = entra.seen.lock().unwrap();
    assert_eq!(seen[0].0, "contoso");
    drop(seen);
    assert_eq!(entra.field(0, "grant_type"), "refresh_token");
    assert_eq!(entra.field(0, "client_id"), "app-id-1");
    assert_eq!(entra.field(0, "refresh_token"), "refresh-0");
    assert_eq!(entra.field(0, "scope"), SCOPE);
    assert_eq!(
        SCOPE,
        "api://workiq.svc.cloud.microsoft/WorkIQAgent.Ask offline_access openid profile"
    );
}

#[tokio::test]
async fn a_rotated_refresh_token_is_saved_and_used_next() {
    let (entra, url) = start().await;
    let root = tempfile::tempdir().unwrap();
    let alice = signed_in(root.path(), "alice", "refresh-0");
    entra.token("access-1", 240, Some("refresh-1"));
    entra.token("access-2", 240, None);
    entra.token("access-3", 3600, Some("refresh-3"));
    let tokens = tokens(&alice, &url);

    tokens.token().await.unwrap();
    assert_eq!(alice.workiq_refresh_token().unwrap(), "refresh-1");
    tokens.token().await.unwrap();
    assert_eq!(entra.field(1, "refresh_token"), "refresh-1");
    assert_eq!(
        alice.workiq_refresh_token().unwrap(),
        "refresh-1",
        "kept when Entra returns none"
    );
    tokens.token().await.unwrap();
    assert_eq!(entra.field(2, "refresh_token"), "refresh-1");
    assert_eq!(alice.workiq_refresh_token().unwrap(), "refresh-3");
}

#[tokio::test]
async fn two_callers_at_once_cause_one_refresh() {
    let (entra, url) = start().await;
    let root = tempfile::tempdir().unwrap();
    let alice = signed_in(root.path(), "alice", "refresh-0");
    *entra.delay.lock().unwrap() = Duration::from_millis(300);
    entra.token("access-1", 3600, Some("refresh-1"));
    let tokens = Arc::new(tokens(&alice, &url));

    let callers: Vec<_> = (0..2)
        .map(|_| {
            let tokens = tokens.clone();
            tokio::spawn(async move { tokens.token().await.unwrap() })
        })
        .collect();
    for caller in callers {
        assert_eq!(caller.await.unwrap(), "access-1");
    }
    assert_eq!(entra.requests(), 1);

    // The client's 401 retry from several calls at once also refreshes once.
    *entra.delay.lock().unwrap() = Duration::from_millis(300);
    entra.token("access-2", 3600, None);
    let callers: Vec<_> = (0..2)
        .map(|_| {
            let tokens = tokens.clone();
            tokio::spawn(async move { tokens.refresh().await.unwrap() })
        })
        .collect();
    for caller in callers {
        caller.await.unwrap();
    }
    assert_eq!(entra.requests(), 2);
    assert_eq!(tokens.token().await.unwrap(), "access-2");
}

#[tokio::test]
async fn refresh_replaces_a_token_that_work_iq_rejected() {
    let (entra, url) = start().await;
    let root = tempfile::tempdir().unwrap();
    let alice = signed_in(root.path(), "alice", "refresh-0");
    entra.token("access-1", 3600, None);
    entra.token("access-2", 3600, None);
    let tokens = tokens(&alice, &url);

    assert_eq!(tokens.token().await.unwrap(), "access-1");
    tokens.refresh().await.unwrap();
    assert_eq!(tokens.token().await.unwrap(), "access-2");
    assert_eq!(entra.requests(), 2);
}

#[tokio::test]
async fn invalid_grant_and_interaction_required_are_sign_in_again() {
    let (entra, url) = start().await;
    let root = tempfile::tempdir().unwrap();
    let alice = signed_in(root.path(), "alice", "refresh-0");
    entra.answer(
        400,
        json!({
            "error": "invalid_grant",
            "error_description": "AADSTS70008: The refresh token has expired.\r\nTrace ID: 1",
        }),
    );
    entra.answer(
        400,
        json!({ "error": "interaction_required", "error_description": "AADSTS50076: MFA" }),
    );
    entra.answer(
        400,
        json!({ "error": "invalid_client", "error_description": "AADSTS7000218" }),
    );
    let tokens = tokens(&alice, &url);

    for expected in ["invalid_grant", "interaction_required"] {
        let error = failure(tokens.token().await.unwrap_err());
        assert_eq!(error.class, Class::SignInAgain);
        assert_eq!(error.fix.who, Who::User);
        assert!(
            error
                .fix
                .action
                .contains("proofkin auth login workiq --profile alice"),
            "{}",
            error.fix.action
        );
        assert!(error.message.contains(expected), "{}", error.message);
        assert!(!error.message.contains('\n'), "one line");
    }
    let other = failure(tokens.token().await.unwrap_err());
    assert_ne!(other.class, Class::SignInAgain);
    assert_eq!(
        alice.workiq_refresh_token().unwrap(),
        "refresh-0",
        "a failed refresh keeps the token"
    );
}

#[tokio::test]
async fn a_worker_without_the_grant_is_refused_and_one_with_it_gets_its_owners_token() {
    let (entra, url) = start().await;
    let root = tempfile::tempdir().unwrap();
    let alice = signed_in(root.path(), "alice", "owner-refresh");
    entra.token("owner-access", 3600, None);

    let plain = alice.create_digital_worker("plain", None).unwrap();
    let refused = HostedTokens::new(&plain)
        .err()
        .expect("refused")
        .to_string();
    assert!(
        refused.contains("proofkin worker create plain --owner alice --workiq"),
        "{refused}"
    );
    assert!(describe(&plain).contains("not used"));
    assert_eq!(entra.requests(), 0);

    // `worker create --workiq` turns it on, once the owner is signed in.
    let addr = proofkin::EndpointAddr::new(proofkin::SecretKey::generate().public());
    let spec = |name: &str, workiq| WorkerSpec {
        name: name.into(),
        workiq,
        ..WorkerSpec::default()
    };
    let trust = proofkin::TrustStore::default();
    let created = create(&alice, addr.clone(), &trust, &spec("granted", true)).unwrap();
    assert!(created.worker.config.workiq_from_owner);
    let created_plain = create(&alice, addr.clone(), &trust, &spec("also-plain", false)).unwrap();
    assert!(!created_plain.worker.config.workiq_from_owner);
    let mut nobody = signed_in(root.path(), "nobody", "x");
    logout(&mut nobody).unwrap();
    let error = create(&nobody, addr, &trust, &spec("w", true))
        .expect_err("owner not signed in")
        .to_string();
    assert!(error.contains("proofkin auth login workiq --profile nobody"));

    let granted = Profile::open_in(root.path(), "granted").unwrap();
    let tokens = tokens(&granted, &url);
    assert_eq!(tokens.token().await.unwrap(), "owner-access");
    assert_eq!(entra.field(0, "refresh_token"), "owner-refresh");
    assert!(
        granted.workiq_refresh_token().is_err(),
        "the worker keeps no token of its own"
    );
    assert!(describe(&granted).contains("Alice Example"));
    assert!(describe(&granted).contains("'alice'"));
}

#[tokio::test]
async fn profile_json_holds_no_token() {
    let (entra, url) = start().await;
    let root = tempfile::tempdir().unwrap();
    let alice = signed_in(root.path(), "alice", "refresh-secret-0");
    entra.token("access-secret-1", 3600, Some("refresh-secret-1"));
    let tokens = tokens(&alice, &url);
    assert_eq!(tokens.token().await.unwrap(), "access-secret-1");

    let text = std::fs::read_to_string(alice.dir.join("profile.json")).unwrap();
    for secret in ["refresh-secret", "access-secret"] {
        assert!(!text.contains(secret), "profile.json has {secret}");
    }
    let json: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(json["workiq"]["connection"]["kind"], "hosted");
    assert_eq!(json["workiq"]["upn"], "alice@contoso.example");
    assert_eq!(json["workiq"]["added"], 1_700_000_000u64);
    assert_eq!(
        std::fs::read_to_string(alice.dir.join("secrets/workiq")).unwrap(),
        "refresh-secret-1"
    );
    assert_eq!(
        Profile::open_in(root.path(), "alice").unwrap().config,
        alice.config
    );

    // A profile without the account has no `workiq` in its file.
    let bob = Profile::open_in(root.path(), "bob").unwrap();
    bob.save().unwrap();
    let text = std::fs::read_to_string(bob.dir.join("profile.json")).unwrap();
    assert!(!text.contains("workiq"), "{text}");
}

#[tokio::test]
async fn logout_removes_the_account_and_its_token() {
    let root = tempfile::tempdir().unwrap();
    let mut alice = signed_in(root.path(), "alice", "refresh-0");
    assert!(describe(&alice).contains("signed in: Alice Example (alice@contoso.example)"));

    logout(&mut alice).unwrap();
    let again = Profile::open_in(root.path(), "alice").unwrap();
    assert!(again.config.workiq.is_none());
    assert!(again.workiq_refresh_token().is_err());
    assert!(!alice.dir.join("secrets/workiq").exists());
    assert!(describe(&again).contains("proofkin auth login workiq --profile alice"));
    assert!(
        !std::fs::read_to_string(again.dir.join("profile.json"))
            .unwrap()
            .contains("workiq")
    );
}
