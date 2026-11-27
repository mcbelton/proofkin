//! `proofkin workiq doctor`: checks the connection to Work IQ step by step, and for the
//! first step that fails says what is wrong, what fixes it and who can do that (see
//! docs/work-iq.md, section 10).

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

use super::account::{HostedTokens, encode};
use super::audit::{Audit, CallContext, Purpose};
use super::client::{Class, Client, Fix, TokenSource, Who, WorkIqError, http_client};
use super::login::id_token_claims;
use super::{Endpoints, connect_at};
use crate::profile::{Profile, WorkIqAccount, WorkIqChanges, WorkIqConnection};

/// Where the Work IQ resource describes itself and its authorization servers.
pub const DISCOVERY_URL: &str =
    "https://workiq.svc.cloud.microsoft/.well-known/oauth-protected-resource/mcp";
/// The Work IQ resource's application id: the one an admin creates a service principal for.
pub const RESOURCE_APP_ID: &str = "fdcc1f02-fc51-4226-8753-f668596af7f7";
/// The `aud` values of a token meant for Work IQ.
pub const AUDIENCES: [&str; 2] = ["api://workiq.svc.cloud.microsoft", RESOURCE_APP_ID];
/// The delegated permission Work IQ needs, as it appears in a token's `scp`.
pub const REQUIRED_SCOPE: &str = "WorkIQAgent.Ask";
/// What the last check asks Work IQ for.
pub const PROBE_QUERY: &str = "Proofkin connection check";

const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(30);

/// How a check ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    Failed,
    /// It doesn't apply to this account.
    Skipped,
}

impl Outcome {
    fn key(self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::Failed => "failed",
            Outcome::Skipped => "skipped",
        }
    }
}

/// One check and what it found.
#[derive(Debug, Clone)]
pub struct Check {
    pub name: &'static str,
    pub outcome: Outcome,
    pub detail: String,
    /// What to do about a failure, and who can do it.
    pub fix: Option<Fix>,
}

impl Check {
    fn ok(name: &'static str, detail: impl Into<String>) -> Self {
        Check {
            name,
            outcome: Outcome::Ok,
            detail: detail.into(),
            fix: None,
        }
    }

    fn skipped(name: &'static str, detail: impl Into<String>) -> Self {
        Check {
            name,
            outcome: Outcome::Skipped,
            detail: detail.into(),
            fix: None,
        }
    }

    fn failed(name: &'static str, detail: impl Into<String>, who: Who, action: &str) -> Self {
        Check {
            name,
            outcome: Outcome::Failed,
            detail: detail.into(),
            fix: Some(Fix {
                who,
                action: action.to_string(),
            }),
        }
    }

    fn failed_with(name: &'static str, (detail, fix): (String, Fix)) -> Self {
        Check {
            name,
            outcome: Outcome::Failed,
            detail,
            fix: Some(fix),
        }
    }
}

/// The checks that ran, in order; the last one is the one that failed, if any did.
#[derive(Debug, Clone)]
pub struct Report {
    pub profile: String,
    pub checks: Vec<Check>,
    /// The profile's Microsoft 365 change settings, when it allows changes (`workiq changes
    /// on`).
    pub changes: Option<WorkIqChanges>,
}

impl Report {
    pub fn passed(&self) -> bool {
        self.checks.iter().all(|c| c.outcome != Outcome::Failed)
    }

    /// For the web UI: each check's name, result, detail and fix.
    pub fn to_json(&self) -> Value {
        let checks: Vec<Value> = self
            .checks
            .iter()
            .map(|check| {
                let fix = check
                    .fix
                    .as_ref()
                    .map(|fix| json!({ "who": who_key(fix.who), "action": fix.action }));
                json!({
                    "name": check.name,
                    "result": check.outcome.key(),
                    "detail": check.detail,
                    "fix": fix,
                })
            })
            .collect();
        json!({
            "profile": self.profile,
            "ok": self.passed(),
            "checks": checks,
            "changes": self.changes.is_some(),
            "change_wait_minutes": self.changes.as_ref().map(WorkIqChanges::wait_minutes),
        })
    }

    /// The report as `proofkin workiq doctor` prints it.
    pub fn describe(&self) -> String {
        let mut text = format!("Work IQ doctor for profile '{}'\n", self.profile);
        for check in &self.checks {
            let mark = match check.outcome {
                Outcome::Ok => "ok",
                Outcome::Failed => "FAILED",
                Outcome::Skipped => "skipped",
            };
            text.push_str(&format!(
                "  {mark:<8} {:<10} {}\n",
                check.name, check.detail
            ));
            if let Some(fix) = &check.fix {
                text.push_str(&format!(
                    "  {:<8} {:<10} fix ({}): {}\n",
                    "", "", fix.who, fix.action
                ));
            }
        }
        text.push_str(&match &self.changes {
            Some(changes) => format!(
                "Microsoft 365 changes: on (plans make the changes you sign off, and a digital worker's task for you, in a folder whose workiq server has --changes, asks you to sign off each change and waits up to {} minute{} for you; turn them off with: proofkin workiq changes off)\n",
                changes.wait_minutes(),
                if changes.wait_minutes() == 1 { "" } else { "s" }
            ),
            None => format!(
                "Microsoft 365 changes: off (turn them on with: proofkin workiq changes on --profile {})\n",
                self.profile
            ),
        });
        text.push_str(if self.passed() {
            "Work IQ is ready.\n"
        } else {
            "Work IQ is not ready: make the fix for the FAILED check, then run this again.\n"
        });
        text
    }
}

fn who_key(who: Who) -> &'static str {
    match who {
        Who::User => "user",
        Who::Admin => "admin",
        Who::Support => "support",
    }
}

/// The account a diagnosis is about.
struct SignedIn {
    holder: Profile,
    account: WorkIqAccount,
    tokens: Option<Arc<HostedTokens>>,
}

/// Checks the Work IQ connection of a profile.
pub struct Doctor<'a> {
    profile: &'a Profile,
    /// Where the service is, or why a test-only setting can't say (see
    /// [`Endpoints::configured`]).
    endpoints: Result<Endpoints, String>,
}

impl<'a> Doctor<'a> {
    pub fn new(profile: &'a Profile) -> Self {
        Doctor {
            profile,
            endpoints: Endpoints::configured().map_err(|e| format!("{e:#}")),
        }
    }

    /// Uses other addresses than the real service's, for tests.
    pub fn with_endpoints(mut self, endpoints: Endpoints) -> Self {
        self.endpoints = Ok(endpoints);
        self
    }

    /// Runs the checks in order and stops at the first that fails.
    pub async fn run(&self) -> Report {
        let mut checks = Vec::new();
        let endpoints = match &self.endpoints {
            Ok(endpoints) => endpoints,
            Err(e) => {
                let fix = "unset it: it points Proofkin at a fake Work IQ for tests";
                checks.push(Check::failed("discovery", e.clone(), Who::User, fix));
                return self.report(checks);
            }
        };
        let hosted = self.profile.workiq_account().is_ok_and(|(_, account)| {
            matches!(account.connection, WorkIqConnection::Hosted { .. })
        });
        let discovery = if hosted {
            self.discovery(endpoints).await
        } else {
            Check::skipped("discovery", "only hosted accounts use it")
        };
        if !self.finish(&mut checks, discovery) {
            return self.report(checks);
        }
        let (signed_in, client) = match self.sign_in(endpoints) {
            Ok((check, signed_in, client)) => {
                checks.push(check);
                (signed_in, client)
            }
            Err(check) => {
                checks.push(check);
                return self.report(checks);
            }
        };
        let token = self.token(&signed_in).await;
        if !self.finish(&mut checks, token) {
            return self.report(checks);
        }
        let audit = Arc::new(Audit::for_profile(self.profile));
        let context = CallContext::local(Purpose::Doctor);
        let client = client.with_audit(audit).with_context(context.clone());
        let tools = self.tools(&client, &signed_in).await;
        if !self.finish(&mut checks, tools) {
            return self.report(checks);
        }
        let retrieve = self.retrieve(&client, &context, &signed_in).await;
        checks.push(retrieve);
        self.report(checks)
    }

    /// Adds `check`; whether the checks may go on.
    fn finish(&self, checks: &mut Vec<Check>, check: Check) -> bool {
        let go_on = check.outcome != Outcome::Failed;
        checks.push(check);
        go_on
    }

    fn report(&self, checks: Vec<Check>) -> Report {
        Report {
            profile: self.profile.name.clone(),
            checks,
            changes: self.profile.config.workiq_changes.clone(),
        }
    }

    async fn discovery(&self, endpoints: &Endpoints) -> Check {
        let name = "discovery";
        let url = &endpoints.discovery_url;
        let retry = "check the network, proxy and firewall: Work IQ's address must be reachable \
                     over HTTPS; then run this again";
        let http = match http_client(DISCOVERY_TIMEOUT) {
            Ok(http) => http,
            Err(e) => {
                return Check::failed(name, format!("{e:#}"), Who::Support, "give this to support");
            }
        };
        let response = match http
            .get(url)
            .header("Accept", "application/json")
            .send()
            .await
        {
            Ok(response) => response,
            Err(e) => {
                let detail = format!("could not reach {url}: {}", e.without_url());
                return Check::failed(name, detail, Who::User, retry);
            }
        };
        let status = response.status();
        if !status.is_success() {
            let detail = format!("{url} answered {status}");
            return Check::failed(name, detail, Who::User, retry);
        }
        let body: Value = match response.text().await {
            Ok(text) => serde_json::from_str(&text).unwrap_or(Value::Null),
            Err(e) => {
                let detail = format!("reading the answer failed: {}", e.without_url());
                return Check::failed(name, detail, Who::User, retry);
            }
        };
        let resource = body
            .get("resource")
            .and_then(Value::as_str)
            .filter(|resource| !resource.is_empty());
        let server = body
            .get("authorization_servers")
            .and_then(Value::as_array)
            .and_then(|servers| servers.first())
            .and_then(Value::as_str);
        match (resource, server) {
            (Some(resource), Some(server)) => Check::ok(
                name,
                format!("the resource is {resource}, signing in at {server}"),
            ),
            _ => Check::failed(
                name,
                format!("{url} answers without `resource` and `authorization_servers`"),
                Who::Support,
                "this is not what Work IQ should answer: check for a proxy that rewrites it, and \
                 give the answer to support",
            ),
        }
    }

    fn sign_in(&self, endpoints: &Endpoints) -> Result<(Check, SignedIn, Client), Check> {
        let name = "sign-in";
        let hint = format!("run `{}`", self.profile.workiq_login_hint());
        let (holder, account) = match self.profile.workiq_account() {
            Ok(found) => found,
            Err(e) => {
                let action = if self.profile.workiq_profile().is_ok() {
                    hint.as_str()
                } else {
                    "have the worker's owner turn Work IQ on for it, as the message says"
                };
                return Err(Check::failed(name, format!("{e:#}"), Who::User, action));
            }
        };
        let hosted = matches!(account.connection, WorkIqConnection::Hosted { .. });
        if hosted && let Err(e) = holder.workiq_refresh_token() {
            let action = format!("run `{}`", holder.workiq_login_hint());
            return Err(Check::failed(name, format!("{e:#}"), Who::User, &action));
        }
        let connection = connect_at(self.profile, endpoints).map_err(|e| {
            Check::failed(
                name,
                format!("{e:#}"),
                Who::Support,
                "check the message; if it looks right, give it to support",
            )
        })?;
        let detail = super::account::account_line(&account);
        Ok((
            Check::ok(name, detail),
            SignedIn {
                holder,
                account,
                tokens: connection.tokens,
            },
            connection.client,
        ))
    }

    /// A fresh access token and its claims. The claims are decoded without checking the
    /// signature, only to say what is wrong; nothing else trusts them.
    async fn token(&self, signed_in: &SignedIn) -> Check {
        let name = "token";
        let Some(tokens) = &signed_in.tokens else {
            return Check::skipped(name, "the Work IQ CLI signs in by itself");
        };
        let token = match tokens.token().await {
            Ok(token) => token,
            Err(e) => return Check::failed_with(name, self.explain(&e, signed_in)),
        };
        let Some(claims) = id_token_claims(&token) else {
            return Check::failed(
                name,
                "the access token has no claims Proofkin can read",
                Who::User,
                &format!("run `{}`", signed_in.holder.workiq_login_hint()),
            );
        };
        let audience_ok = match claims.get("aud") {
            Some(Value::String(aud)) => AUDIENCES.contains(&aud.as_str()),
            Some(Value::Array(auds)) => auds
                .iter()
                .any(|aud| aud.as_str().is_some_and(|aud| AUDIENCES.contains(&aud))),
            _ => false,
        };
        if !audience_ok {
            let aud = claims
                .get("aud")
                .map_or("none".to_string(), Value::to_string);
            return Check::failed(
                name,
                format!(
                    "the token's audience is {aud}, not Work IQ ({})",
                    AUDIENCES[0]
                ),
                Who::Admin,
                &format!(
                    "make sure the client app has the delegated {REQUIRED_SCOPE} permission \
                     from Work IQ, and that the tenant has the Work IQ service principal: {}",
                    service_principal_command()
                ),
            );
        }
        let tenant = claims.get("tid").and_then(Value::as_str).unwrap_or("");
        if !tenant.eq_ignore_ascii_case(&signed_in.account.tenant_id) {
            return Check::failed(
                name,
                format!(
                    "the token is for tenant {} but the account is in tenant {}",
                    if tenant.is_empty() { "none" } else { tenant },
                    signed_in.account.tenant_id
                ),
                Who::User,
                &format!(
                    "sign out and sign in again with the account of tenant {}: `{}`",
                    signed_in.account.tenant_id,
                    signed_in.holder.workiq_login_hint()
                ),
            );
        }
        let has_scope = claims
            .get("scp")
            .and_then(Value::as_str)
            .is_some_and(|scp| scp.split_whitespace().any(|s| s == REQUIRED_SCOPE));
        if !has_scope {
            return Check::failed_with(
                name,
                (
                    format!("the token does not have the {REQUIRED_SCOPE} scope"),
                    Fix {
                        who: Who::Admin,
                        action: format!(
                            "add the delegated {REQUIRED_SCOPE} permission from Work IQ to the \
                             client app and grant consent; {}",
                            self.consent_action(signed_in)
                        ),
                    },
                ),
            );
        }
        Check::ok(
            name,
            format!(
                "a fresh token for Work IQ in tenant {} with the {REQUIRED_SCOPE} scope",
                signed_in.account.tenant_id
            ),
        )
    }

    async fn tools(&self, client: &Client, signed_in: &SignedIn) -> Check {
        let name = "tools";
        let tools = match client.list_tools().await {
            Ok(tools) => tools,
            Err(e) => return Check::failed_with(name, self.explain(&e, signed_in)),
        };
        if tools.iter().any(|tool| tool.name == "retrieve") {
            return Check::ok(
                name,
                format!("Work IQ offers {} tools, including retrieve", tools.len()),
            );
        }
        let offered: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
        Check::failed(
            name,
            format!(
                "Work IQ does not offer the retrieve tool (it offers: {})",
                offered.join(", ")
            ),
            Who::Admin,
            "turn the retrieve tool on in the Work IQ MCP policy (Microsoft 365 admin center, \
             Agents, Tools, Work IQ MCP)",
        )
    }

    async fn retrieve(
        &self,
        client: &Client,
        context: &CallContext,
        signed_in: &SignedIn,
    ) -> Check {
        let name = "retrieve";
        match client.retrieve_in(context, PROBE_QUERY, None, &[]).await {
            Ok(_) => Check::ok(
                name,
                format!(
                    "a small retrieve (\"{PROBE_QUERY}\") worked; it counts against the budget"
                ),
            ),
            Err(e) => Check::failed_with(name, self.explain(&e, signed_in)),
        }
    }

    /// What went wrong and the fix, from the client's error classes where they apply.
    fn explain(&self, error: &anyhow::Error, signed_in: &SignedIn) -> (String, Fix) {
        let Some(failure) = error.downcast_ref::<WorkIqError>() else {
            return (
                format!("{error:#}"),
                Fix {
                    who: Who::Support,
                    action: "check the message; if it looks right, give it to support".to_string(),
                },
            );
        };
        let mut detail = failure.message.clone();
        if failure.ids != Default::default() {
            detail.push_str(&format!(" ({})", failure.ids));
        }
        let text = failure.message.to_ascii_lowercase();
        let admin = |action: String| Fix {
            who: Who::Admin,
            action,
        };
        let fix = if text.contains("aadsts500011") || text.contains("aadsts650052") {
            admin(format!(
                "create the Work IQ service principal: a tenant admin runs `{}`",
                service_principal_command()
            ))
        } else if text.contains("aadsts50105") {
            admin(
                "assign the user to the Work IQ app in Microsoft Entra (Enterprise applications, \
                 Users and groups)"
                    .to_string(),
            )
        } else {
            match &failure.class {
                Class::SignInAgain => Fix {
                    who: Who::User,
                    action: format!("run `{}`", signed_in.holder.workiq_login_hint()),
                },
                Class::Consent => admin(self.consent_action(signed_in)),
                Class::Billing => admin(
                    "set up usage-based billing and a spending policy that covers this user and \
                     the Work IQ API, in the Microsoft 365 admin center (a billing admin)"
                        .to_string(),
                ),
                Class::DeniedByPolicy if !text.contains("aadsts53003") => admin(
                    "change the Work IQ MCP policy in the Microsoft 365 admin center (Agents, \
                     Tools, Work IQ MCP); it takes up to 24 hours to apply"
                        .to_string(),
                ),
                _ => failure.fix.clone(),
            }
        };
        (detail, fix)
    }

    /// The admin consent step for the account's app, with its URL when the app is known.
    fn consent_action(&self, signed_in: &SignedIn) -> String {
        let authority = self
            .endpoints
            .as_ref()
            .map_or(super::account::DEFAULT_AUTHORITY, |e| e.authority.as_str());
        match &signed_in.account.connection {
            WorkIqConnection::Hosted { tenant, client_id } => format!(
                "an admin grants consent by opening {}/{}/adminconsent?client_id={} and accepting, \
                 then you sign in again",
                authority,
                encode(tenant),
                encode(client_id)
            ),
            WorkIqConnection::Cli { .. } => {
                "an admin grants consent to the Work IQ CLI app, then you sign in again".to_string()
            }
        }
    }
}

fn service_principal_command() -> String {
    format!("az ad sp create --id {RESOURCE_APP_ID}")
}
