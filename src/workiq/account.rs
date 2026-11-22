//! A profile's Work IQ account: the token source for hosted accounts, and the account's
//! description and removal (see docs/work-iq.md, section 1).
//!
//! The refresh token lives in the profile's secret store and is replaced whenever Entra
//! returns a new one. The access token lives in memory only.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::Mutex;

use super::client::{
    Class, Fix, RequestIds, TokenSource, Who, WorkIqError, WorkIqErrorKind, http_client,
};
use crate::profile::{Profile, WorkIqConnection};

/// The Entra authority; the tenant and `/oauth2/v2.0/token` follow.
pub const DEFAULT_AUTHORITY: &str = "https://login.microsoftonline.com";
/// What a Work IQ token is for. There is no `resource` parameter: Entra rejects the MCP URL.
pub const SCOPE: &str =
    "api://workiq.svc.cloud.microsoft/WorkIQAgent.Ask offline_access openid profile";
/// A token is replaced this long before it expires.
pub const REFRESH_MARGIN: Duration = Duration::from_secs(5 * 60);

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Used when Entra doesn't say how long a token lives.
const DEFAULT_LIFETIME: Duration = Duration::from_secs(3600);
const ERROR_CHARS: usize = 300;

struct Cached {
    token: String,
    expires_at: Instant,
}

impl Cached {
    fn is_fresh(&self) -> bool {
        Instant::now() + REFRESH_MARGIN < self.expires_at
    }
}

/// Tokens for a hosted Work IQ account: signed in to Entra by Proofkin itself.
pub struct HostedTokens {
    holder: Profile,
    tenant: String,
    client_id: String,
    authority: String,
    http: reqwest::Client,
    cached: Mutex<Option<Cached>>,
    /// Counts the tokens fetched, so a caller that waited for another's refresh can tell.
    generation: AtomicU64,
}

impl HostedTokens {
    /// The tokens of the account `profile` uses: its own, or for a digital worker its
    /// owner's when the owner turned that on for it.
    pub fn new(profile: &Profile) -> Result<Self> {
        let (holder, account) = profile.workiq_account()?;
        let WorkIqConnection::Hosted { tenant, client_id } = account.connection else {
            bail!(
                "the Work IQ account of '{}' uses the Work IQ CLI, which signs in by itself",
                holder.name
            );
        };
        Ok(HostedTokens {
            holder,
            tenant,
            client_id,
            authority: DEFAULT_AUTHORITY.to_string(),
            http: http_client(REQUEST_TIMEOUT)?,
            cached: Mutex::new(None),
            generation: AtomicU64::new(0),
        })
    }

    /// Uses another authority than Entra's, for tests.
    pub fn with_authority(mut self, base_url: &str) -> Self {
        self.authority = base_url.trim_end_matches('/').to_string();
        self
    }

    /// Starts with the access token a sign-in just got, so checking it needs no refresh.
    pub fn with_token(mut self, token: &str, lifetime: Duration) -> Self {
        *self.cached.get_mut() = Some(Cached {
            token: token.to_string(),
            expires_at: Instant::now() + lifetime,
        });
        self
    }

    /// Gets a new access token. The caller holds the lock on the cache, so only one refresh
    /// runs at a time.
    async fn renew(&self, cached: &mut Option<Cached>) -> Result<String> {
        match self.fetch().await {
            Ok(new) => {
                let token = new.token.clone();
                *cached = Some(new);
                self.generation.fetch_add(1, Ordering::SeqCst);
                Ok(token)
            }
            Err(error) => {
                if matches!(&error.class, Class::SignInAgain) {
                    *cached = None;
                }
                Err((*error).into())
            }
        }
    }

    async fn fetch(&self) -> Result<Cached, Box<WorkIqError>> {
        let no_ids = RequestIds::default;
        let refresh_token = self
            .holder
            .workiq_refresh_token()
            .map_err(|e| self.sign_in_again(WorkIqErrorKind::Protocol, format!("{e:#}")))?;
        let url = format!(
            "{}/{}/oauth2/v2.0/token",
            self.authority,
            encode(&self.tenant)
        );
        let form = [
            ("grant_type", "refresh_token"),
            ("client_id", self.client_id.as_str()),
            ("refresh_token", refresh_token.as_str()),
            ("scope", SCOPE),
        ]
        .map(|(name, value)| format!("{name}={}", encode(value)))
        .join("&");
        let response = self
            .http
            .post(url)
            .header("Accept", "application/json")
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(form)
            .send()
            .await
            .map_err(|e| {
                WorkIqError::new(
                    WorkIqErrorKind::Transport,
                    Class::Transient,
                    format!("the request to Entra failed: {}", e.without_url()),
                    no_ids(),
                )
            })?;
        let status = response.status();
        let text = response.text().await.map_err(|e| {
            WorkIqError::new(
                WorkIqErrorKind::Transport,
                Class::Transient,
                format!("reading Entra's answer failed: {}", e.without_url()),
                no_ids(),
            )
        })?;
        let body: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let kind = WorkIqErrorKind::Http(status.as_u16());
        if !status.is_success() {
            return Err(Box::new(self.entra_error(status.as_u16(), &body, &text)));
        }
        let Some(token) = body
            .get("access_token")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
        else {
            return Err(Box::new(WorkIqError::new(
                kind,
                Class::Tool,
                "Entra's answer has no access token",
                no_ids(),
            )));
        };
        // Entra rotates refresh tokens: the next refresh must use the new one.
        if let Some(rotated) = body
            .get("refresh_token")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
        {
            self.holder.set_workiq_refresh_token(rotated).map_err(|e| {
                WorkIqError::new(
                    WorkIqErrorKind::Protocol,
                    Class::Tool,
                    format!("saving the new refresh token failed: {e:#}"),
                    no_ids(),
                )
            })?;
        }
        let lifetime = body
            .get("expires_in")
            .and_then(|v| v.as_u64().or_else(|| v.as_str()?.parse().ok()))
            .map(Duration::from_secs)
            .unwrap_or(DEFAULT_LIFETIME);
        Ok(Cached {
            token: token.to_string(),
            expires_at: Instant::now() + lifetime,
        })
    }

    /// A refused refresh, classified by Entra's error code.
    fn entra_error(&self, status: u16, body: &Value, text: &str) -> WorkIqError {
        let code = body.get("error").and_then(Value::as_str).unwrap_or("");
        let description = body
            .get("error_description")
            .and_then(Value::as_str)
            .unwrap_or(text);
        let description: String = description
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(ERROR_CHARS)
            .collect();
        let message = format!(
            "Entra refused to refresh the token ({}): {description}",
            if code.is_empty() {
                status.to_string()
            } else {
                code.to_string()
            }
        );
        let kind = WorkIqErrorKind::Http(status);
        if matches!(code, "invalid_grant" | "interaction_required") {
            return self.sign_in_again(kind, message);
        }
        let class = if code == "consent_required" || description.contains("AADSTS65001") {
            Class::Consent
        } else if status >= 500 {
            Class::Transient
        } else {
            Class::Tool
        };
        WorkIqError::new(kind, class, message, RequestIds::default())
    }

    fn sign_in_again(&self, kind: WorkIqErrorKind, message: String) -> WorkIqError {
        let mut error = WorkIqError::new(kind, Class::SignInAgain, message, RequestIds::default());
        error.fix = Fix {
            who: Who::User,
            action: format!("run `{}`", self.holder.workiq_login_hint()),
        };
        error
    }
}

#[async_trait]
impl TokenSource for HostedTokens {
    async fn token(&self) -> Result<String> {
        let mut cached = self.cached.lock().await;
        if let Some(current) = cached.as_ref().filter(|c| c.is_fresh()) {
            return Ok(current.token.clone());
        }
        self.renew(&mut cached).await
    }

    async fn refresh(&self) -> Result<()> {
        let seen = self.generation.load(Ordering::SeqCst);
        let mut cached = self.cached.lock().await;
        // Another caller got a new token while this one waited.
        if self.generation.load(Ordering::SeqCst) != seen {
            return Ok(());
        }
        self.renew(&mut cached).await.map(|_| ())
    }
}

/// Percent-encodes `value` for a form body or a URL path segment.
pub(crate) fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(char::from(byte))
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// One line for `proofkin auth status`: who the Work IQ account is, or why there is none.
pub fn describe(profile: &Profile) -> String {
    let Ok(holder) = profile.workiq_profile() else {
        return "not used: its owner turns it on with `proofkin worker create --workiq`".into();
    };
    let Some(account) = &holder.config.workiq else {
        return format!("not signed in  ({})", holder.workiq_login_hint());
    };
    let connection = match &account.connection {
        WorkIqConnection::Hosted { .. } => "hosted",
        WorkIqConnection::Cli { .. } => "Work IQ CLI",
    };
    let mut text = format!(
        "signed in: {} ({}), tenant {}, {connection}",
        account.display_name, account.upn, account.tenant_id
    );
    if holder.name != profile.name {
        text.push_str(&format!(", the account of '{}'", holder.name));
    }
    let hosted = matches!(account.connection, WorkIqConnection::Hosted { .. });
    if hosted && holder.workiq_refresh_token().is_err() {
        text.push_str(&format!(
            "; ERROR: its token is missing, run: {}",
            holder.workiq_login_hint()
        ));
    }
    text
}

/// Removes `profile`'s Work IQ account and its token.
pub fn logout(profile: &mut Profile) -> Result<()> {
    profile.config.workiq = None;
    profile.delete_workiq_token()?;
    profile.save()
}
