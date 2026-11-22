//! `proofkin auth login workiq`: signs a profile in to Entra ID for Work IQ, with the
//! authorization code flow (PKCE, loopback redirect) or the device code flow (see
//! docs/work-iq.md, section 1).

use std::ffi::OsString;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

use super::account::{DEFAULT_AUTHORITY, HostedTokens, SCOPE, encode, logout};
use super::client::{Client, DEFAULT_URL, http_client};
use super::stdio::{NoToken, StdioTransport};
use crate::profile::{Profile, WorkIqAccount, WorkIqConnection};

/// Shown as the user of a CLI account when the CLI doesn't say who it is.
const CLI_USER: &str = "Work IQ CLI";

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the browser has to come back to the loopback address.
const BROWSER_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const DEFAULT_LIFETIME: Duration = Duration::from_secs(3600);
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(5);
/// What `slow_down` adds to the polling interval.
const SLOW_DOWN_STEP: Duration = Duration::from_secs(5);
const DEFAULT_DEVICE_LIFETIME: Duration = Duration::from_secs(15 * 60);
const DEVICE_CODE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
const MAX_REQUEST_BYTES: usize = 16 * 1024;

/// Which sign-in to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    /// Authorization code with PKCE; the browser opens.
    Browser,
    /// Device code, for agents without a browser.
    DeviceCode,
}

/// Opens a web page for the user. A hook so tests can drive the sign-in.
pub trait Browser: Send + Sync {
    fn open(&self, url: &str) -> Result<()>;
}

/// The system's default browser.
pub struct SystemBrowser;

impl Browser for SystemBrowser {
    fn open(&self, url: &str) -> Result<()> {
        let mut command = browser_command(url);
        let status = command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .context("starting the browser")?;
        if !status.success() {
            bail!("the browser command failed ({status})");
        }
        Ok(())
    }
}

#[cfg(windows)]
fn browser_command(url: &str) -> std::process::Command {
    use std::os::windows::process::CommandExt;
    let mut command = std::process::Command::new("cmd");
    // The URL has `&`, which cmd would take as the end of the command.
    command.raw_arg(format!("/c start \"\" \"{url}\""));
    command
}

#[cfg(not(windows))]
fn browser_command(url: &str) -> std::process::Command {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let mut command = std::process::Command::new(opener);
    command.arg(url);
    command
}

/// A Work IQ sign-in through Microsoft's Work IQ CLI: no token is kept, because the CLI signs
/// in by itself with its own app, which the tenant's admin has approved.
pub struct CliLogin {
    tenant: Option<String>,
    command: Option<(OsString, Vec<String>)>,
}

impl CliLogin {
    pub fn new(tenant: Option<&str>) -> Self {
        CliLogin {
            tenant: tenant.map(str::to_string),
            command: None,
        }
    }

    /// Starts `program` with `args` instead of the CLI, for tests.
    pub fn with_command(mut self, program: impl Into<OsString>, args: &[&str]) -> Self {
        let args = args.iter().map(|arg| arg.to_string()).collect();
        self.command = Some((program.into(), args));
        self
    }

    /// Saves the account, and checks it with `tools/list` through the CLI; if that fails, the
    /// profile is signed out again. Returns who signed in: the user the CLI reports, else
    /// `Work IQ CLI`.
    pub async fn run(&self, profile: &mut Profile) -> Result<String> {
        let transport = Arc::new(match &self.command {
            Some((program, args)) => {
                let args: Vec<&str> = args.iter().map(String::as_str).collect();
                StdioTransport::with_command(program, &args, self.tenant.as_deref())
            }
            None => StdioTransport::new(self.tenant.as_deref()),
        });
        let mut account = WorkIqAccount {
            connection: WorkIqConnection::Cli {
                tenant: self.tenant.clone(),
            },
            display_name: CLI_USER.to_string(),
            upn: CLI_USER.to_string(),
            tenant_id: self.tenant.clone().unwrap_or_else(|| "unknown".into()),
            added: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        };
        profile.config.workiq = Some(account.clone());
        profile.delete_workiq_token()?;
        profile.save()?;

        let client = Client::with_transport(transport.clone(), Arc::new(NoToken));
        if let Err(error) = client.list_tools().await {
            logout(profile)?;
            bail!(
                "Work IQ refused the sign-in through the Work IQ CLI, so it was signed out \
                 again: {error:#}"
            );
        }
        let who = transport
            .reported_user()
            .await
            .unwrap_or_else(|| CLI_USER.to_string());
        if who != CLI_USER {
            account.display_name = who.clone();
            account.upn = who.clone();
            profile.config.workiq = Some(account);
            profile.save()?;
        }
        Ok(who)
    }
}

/// A Work IQ sign-in for one tenant and client app.
pub struct Login {
    tenant: String,
    client_id: String,
    authority: String,
    mcp_url: String,
    browser: Arc<dyn Browser>,
    say: Arc<dyn Fn(&str) + Send + Sync>,
    slow_down_step: Duration,
    browser_timeout: Duration,
}

/// What Entra's token endpoint gave.
struct Grant {
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
    lifetime: Duration,
}

impl Login {
    pub fn new(tenant: &str, client_id: &str) -> Self {
        Login {
            tenant: tenant.to_string(),
            client_id: client_id.to_string(),
            authority: DEFAULT_AUTHORITY.to_string(),
            mcp_url: DEFAULT_URL.to_string(),
            browser: Arc::new(SystemBrowser),
            say: Arc::new(|text| println!("{text}")),
            slow_down_step: SLOW_DOWN_STEP,
            browser_timeout: BROWSER_TIMEOUT,
        }
    }

    /// Uses another authority than Entra's, for tests.
    pub fn with_authority(mut self, base_url: &str) -> Self {
        self.authority = base_url.trim_end_matches('/').to_string();
        self
    }

    /// Checks the sign-in against another MCP endpoint than Work IQ's, for tests.
    pub fn with_mcp_url(mut self, url: &str) -> Self {
        self.mcp_url = url.to_string();
        self
    }

    pub fn with_browser(mut self, browser: Arc<dyn Browser>) -> Self {
        self.browser = browser;
        self
    }

    /// Where the instructions for the user go (standard output by default).
    pub fn with_output(mut self, say: Arc<dyn Fn(&str) + Send + Sync>) -> Self {
        self.say = say;
        self
    }

    /// Changes what `slow_down` adds to the polling interval, and how long the browser has to
    /// come back; for tests.
    pub fn with_timing(mut self, slow_down_step: Duration, browser_timeout: Duration) -> Self {
        self.slow_down_step = slow_down_step;
        self.browser_timeout = browser_timeout;
        self
    }

    /// Signs `profile` in, saves the account and checks it with `tools/list`; if Work IQ
    /// refuses, the profile is signed out again. Returns who signed in.
    pub async fn run(&self, profile: &mut Profile, flow: Flow) -> Result<String> {
        let grant = match flow {
            Flow::Browser => self.browser_flow().await?,
            Flow::DeviceCode => self.device_code_flow().await?,
        };
        let refresh_token = grant
            .refresh_token
            .clone()
            .filter(|t| !t.is_empty())
            .ok_or_else(|| {
                anyhow!("Entra returned no refresh token; the app must allow offline_access")
            })?;
        let claims = grant.id_token.as_deref().and_then(id_token_claims);
        let claim = |name: &str| {
            claims
                .as_ref()
                .and_then(|c| c.get(name))
                .and_then(Value::as_str)
                .filter(|v| !v.is_empty())
                .map(str::to_string)
        };
        let upn = claim("preferred_username").unwrap_or_else(|| "unknown user".into());
        let account = WorkIqAccount {
            connection: WorkIqConnection::Hosted {
                tenant: self.tenant.clone(),
                client_id: self.client_id.clone(),
            },
            display_name: claim("name").unwrap_or_else(|| upn.clone()),
            upn,
            tenant_id: claim("tid").unwrap_or_else(|| self.tenant.clone()),
            added: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        };
        let who = format!("{} ({})", account.display_name, account.upn);
        profile.config.workiq = Some(account);
        profile.set_workiq_refresh_token(&refresh_token)?;
        profile.save()?;

        let tokens = HostedTokens::new(profile)?
            .with_authority(&self.authority)
            .with_token(&grant.access_token, grant.lifetime);
        let check = match Client::new(&self.mcp_url, Arc::new(tokens)) {
            Ok(client) => client.list_tools().await.map(|_| ()),
            Err(e) => Err(e),
        };
        if let Err(error) = check {
            logout(profile)?;
            bail!("Work IQ refused the sign-in of {who}, so it was signed out again: {error:#}");
        }
        Ok(who)
    }

    fn token_url(&self) -> String {
        format!(
            "{}/{}/oauth2/v2.0/token",
            self.authority,
            encode(&self.tenant)
        )
    }

    async fn browser_flow(&self) -> Result<Grant> {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .context("opening a loopback port for the sign-in")?;
        let redirect_uri = format!("http://localhost:{}", listener.local_addr()?.port());
        let verifier = random_token(32);
        let challenge = data_encoding::BASE64URL_NOPAD.encode(&Sha256::digest(verifier.as_bytes()));
        let state = random_token(16);
        let query = form(&[
            ("client_id", self.client_id.as_str()),
            ("response_type", "code"),
            ("redirect_uri", redirect_uri.as_str()),
            ("response_mode", "query"),
            ("scope", SCOPE),
            ("state", state.as_str()),
            ("code_challenge", challenge.as_str()),
            ("code_challenge_method", "S256"),
        ]);
        let url = format!(
            "{}/{}/oauth2/v2.0/authorize?{query}",
            self.authority,
            encode(&self.tenant)
        );
        (self.say)(&format!(
            "Sign in to Work IQ in your browser. If it doesn't open, go to:\n{url}"
        ));
        if let Err(error) = self.browser.open(&url) {
            (self.say)(&format!(
                "The browser did not open ({error:#}); open the address above yourself."
            ));
        }
        let (found, mut results) = mpsc::channel(4);
        let expected = state.clone();
        let (tenant, client_id) = (self.tenant.clone(), self.client_id.clone());
        let accepting = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let found = found.clone();
                let (expected, tenant, client_id) =
                    (expected.clone(), tenant.clone(), client_id.clone());
                tokio::spawn(async move {
                    if let Some(result) =
                        answer_redirect(stream, &expected, &tenant, &client_id).await
                    {
                        let _ = found.send(result).await;
                    }
                });
            }
        });
        let code = tokio::time::timeout(self.browser_timeout, results.recv()).await;
        accepting.abort();
        let code = match code {
            Err(_) => bail!(
                "the browser sign-in did not finish in {} minutes; run the command again",
                self.browser_timeout.as_secs().div_ceil(60)
            ),
            Ok(None) => bail!("the sign-in listener stopped"),
            Ok(Some(result)) => result?,
        };
        self.token_request(&[
            ("grant_type", "authorization_code"),
            ("client_id", self.client_id.as_str()),
            ("code", code.as_str()),
            ("redirect_uri", redirect_uri.as_str()),
            ("code_verifier", verifier.as_str()),
            ("scope", SCOPE),
        ])
        .await
        .map_err(|e| anyhow!(self.explain(&e)))
    }

    async fn device_code_flow(&self) -> Result<Grant> {
        let http = http_client(REQUEST_TIMEOUT)?;
        let url = format!(
            "{}/{}/oauth2/v2.0/devicecode",
            self.authority,
            encode(&self.tenant)
        );
        let (status, body) = post_form(
            &http,
            &url,
            &[("client_id", self.client_id.as_str()), ("scope", SCOPE)],
        )
        .await?;
        if !status.is_success() {
            bail!("{}", self.explain(&EntraError::of(&body)));
        }
        let text = |name: &str| body.get(name).and_then(Value::as_str).map(str::to_string);
        let (Some(device_code), Some(user_code), Some(uri)) = (
            text("device_code"),
            text("user_code"),
            text("verification_uri"),
        ) else {
            bail!("Entra's device code answer has no device code, user code or address");
        };
        (self.say)(&text("message").unwrap_or_else(|| {
            format!("To sign in, open {uri} in a browser and enter the code {user_code}")
        }));
        let number = |name: &str| {
            body.get(name)
                .and_then(|v| v.as_u64().or_else(|| v.as_str()?.parse().ok()))
                .map(Duration::from_secs)
        };
        let mut interval = number("interval").unwrap_or(DEFAULT_POLL_INTERVAL);
        let deadline =
            tokio::time::Instant::now() + number("expires_in").unwrap_or(DEFAULT_DEVICE_LIFETIME);
        loop {
            tokio::time::sleep(interval).await;
            if tokio::time::Instant::now() >= deadline {
                bail!("the sign-in code expired before it was used; run the command again");
            }
            let error = match self
                .token_request(&[
                    ("grant_type", DEVICE_CODE_GRANT),
                    ("client_id", self.client_id.as_str()),
                    ("device_code", device_code.as_str()),
                ])
                .await
            {
                Ok(grant) => return Ok(grant),
                Err(error) => error,
            };
            match error.code.as_str() {
                "authorization_pending" => {}
                "slow_down" => interval += self.slow_down_step,
                "expired_token" => {
                    bail!("the sign-in code expired before it was used; run the command again")
                }
                "authorization_declined" => bail!("the sign-in was declined on the other device"),
                _ => bail!("{}", self.explain(&error)),
            }
        }
    }

    /// Asks the token endpoint for tokens.
    async fn token_request(&self, fields: &[(&str, &str)]) -> Result<Grant, EntraError> {
        let http = http_client(REQUEST_TIMEOUT).map_err(EntraError::local)?;
        let (status, body) = post_form(&http, &self.token_url(), fields)
            .await
            .map_err(EntraError::local)?;
        if !status.is_success() {
            return Err(EntraError::of(&body));
        }
        let text = |name: &str| {
            body.get(name)
                .and_then(Value::as_str)
                .filter(|t| !t.is_empty())
                .map(str::to_string)
        };
        let Some(access_token) = text("access_token") else {
            return Err(EntraError::local("Entra's answer has no access token"));
        };
        Ok(Grant {
            access_token,
            refresh_token: text("refresh_token"),
            id_token: text("id_token"),
            lifetime: body
                .get("expires_in")
                .and_then(|v| v.as_u64().or_else(|| v.as_str()?.parse().ok()))
                .map(Duration::from_secs)
                .unwrap_or(DEFAULT_LIFETIME),
        })
    }

    fn explain(&self, error: &EntraError) -> String {
        explain(error, &self.tenant, self.client_id.as_str())
    }
}

/// An error answer from Entra.
struct EntraError {
    code: String,
    description: String,
    error_codes: Vec<u64>,
}

impl EntraError {
    fn of(body: &Value) -> Self {
        let text = |name: &str| {
            body.get(name)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        EntraError {
            code: text("error"),
            description: text("error_description"),
            error_codes: body
                .get("error_codes")
                .and_then(Value::as_array)
                .map(|codes| codes.iter().filter_map(Value::as_u64).collect())
                .unwrap_or_default(),
        }
    }

    /// A failure that did not come from Entra's answer.
    fn local(error: impl std::fmt::Display) -> Self {
        EntraError {
            code: "proofkin".into(),
            description: format!("{error:#}"),
            error_codes: Vec::new(),
        }
    }

    fn mentions(&self, aadsts: u64) -> bool {
        self.error_codes.contains(&aadsts) || self.description.contains(&format!("AADSTS{aadsts}"))
    }
}

/// A clear message for the Entra errors people meet while signing in.
fn explain(error: &EntraError, tenant: &str, client_id: &str) -> String {
    if error.mentions(65001) || error.code == "consent_required" {
        return format!(
            "an admin must grant consent for this app before anyone can sign in to Work IQ. \
             An admin can do that at {}/{}/adminconsent?client_id={}",
            DEFAULT_AUTHORITY,
            encode(tenant),
            encode(client_id)
        );
    }
    if error.mentions(50105) {
        return "you are not assigned to the app. An admin must assign you (or a group you are \
                in) to the app under Enterprise applications > Users and groups in Entra ID"
            .into();
    }
    if error.mentions(700016) || error.mentions(90002) {
        return format!(
            "Entra doesn't know this client id or tenant. Check --client-id ({client_id}) and \
             --tenant ({tenant}) against the app registration"
        );
    }
    if error.mentions(7000218) {
        return "the app doesn't allow public client flows. Turn on Allow public client flows \
                (Authentication > Advanced settings) in the app registration to sign in with a \
                device code"
            .into();
    }
    let what = if error.code.is_empty() {
        "an error"
    } else {
        &error.code
    };
    let description: String = error
        .description
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(300)
        .collect();
    format!("Entra refused the sign-in ({what}): {description}")
}

async fn post_form(
    http: &reqwest::Client,
    url: &str,
    fields: &[(&str, &str)],
) -> Result<(reqwest::StatusCode, Value)> {
    let response = http
        .post(url)
        .header("Accept", "application/json")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(form(fields))
        .send()
        .await
        .map_err(|e| anyhow!("the request to Entra failed: {}", e.without_url()))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|e| anyhow!("reading Entra's answer failed: {}", e.without_url()))?;
    let body = serde_json::from_str(&text).unwrap_or(Value::Null);
    Ok((status, body))
}

fn form(fields: &[(&str, &str)]) -> String {
    fields
        .iter()
        .map(|(name, value)| format!("{name}={}", encode(value)))
        .collect::<Vec<_>>()
        .join("&")
}

/// `bytes` random bytes as URL-safe text.
fn random_token(bytes: usize) -> String {
    let mut random = Vec::new();
    while random.len() < bytes {
        random.extend_from_slice(&iroh::SecretKey::generate().to_bytes());
    }
    data_encoding::BASE64URL_NOPAD.encode(&random[..bytes])
}

/// The claims of a JWT, read without checking its signature: only for display and for
/// diagnosing a connection.
pub(crate) fn id_token_claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = data_encoding::BASE64URL_NOPAD
        .decode(payload.trim_end_matches('=').as_bytes())
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Answers one request to the loopback address. Returns the authorization code, or why the
/// sign-in failed, when the request is Entra's redirect; `None` for any other request (a
/// favicon, a probe), which doesn't end the sign-in.
async fn answer_redirect(
    mut stream: TcpStream,
    expected_state: &str,
    tenant: &str,
    client_id: &str,
) -> Option<Result<String>> {
    let mut request = Vec::new();
    let mut chunk = [0u8; 2048];
    let read = tokio::time::timeout(Duration::from_secs(10), async {
        while !request.windows(4).any(|w| w == b"\r\n\r\n") && request.len() < MAX_REQUEST_BYTES {
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => request.extend_from_slice(&chunk[..n]),
            }
        }
    })
    .await;
    if read.is_err() {
        return None;
    }
    let text = String::from_utf8_lossy(&request);
    let target = text
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("GET "))
        .and_then(|rest| rest.split(' ').next())
        .unwrap_or_default();
    let query = target.split_once('?').map(|(_, q)| q).unwrap_or_default();
    let params: Vec<(String, String)> = query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(name, value)| (decode(name), decode(value)))
        .collect();
    let param = |name: &str| {
        params
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    };
    if param("code").is_none() && param("error").is_none() {
        respond(&mut stream, "404 Not Found", "Nothing here.").await;
        return None;
    }
    if param("state") != Some(expected_state) {
        respond(
            &mut stream,
            "400 Bad Request",
            "This sign-in answer is not for the sign-in Proofkin started, so it was refused.",
        )
        .await;
        return Some(Err(anyhow!(
            "the sign-in answer had the wrong state, so it was refused; run the command again"
        )));
    }
    if let Some(code) = param("error") {
        let error = EntraError {
            code: code.to_string(),
            description: param("error_description").unwrap_or_default().to_string(),
            error_codes: Vec::new(),
        };
        let message = explain(&error, tenant, client_id);
        respond(
            &mut stream,
            "400 Bad Request",
            &format!("Sign-in failed: {message}"),
        )
        .await;
        return Some(Err(anyhow!(message)));
    }
    respond(
        &mut stream,
        "200 OK",
        "You are signed in to Work IQ. You can close this window.",
    )
    .await;
    param("code").map(|code| Ok(code.to_string()))
}

async fn respond(stream: &mut TcpStream, status: &str, message: &str) {
    let message = message
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    let body = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>Proofkin</title></head>\
         <body><p>{message}</p></body></html>"
    );
    let reply = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(reply.as_bytes()).await;
    let _ = stream.shutdown().await;
}

/// Decodes a percent-encoded query component.
fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() && hex_pair(&bytes[i + 1..i + 3]).is_some() => {
                out.push(hex_pair(&bytes[i + 1..i + 3]).unwrap_or_default());
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
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_pair(pair: &[u8]) -> Option<u8> {
    let text = std::str::from_utf8(pair).ok()?;
    u8::from_str_radix(text, 16).ok()
}
