//! Work IQ: Microsoft 365 work context for plans and tasks (see docs/work-iq.md).
//!
//! [`client`] is the MCP client. It talks to a [`Transport`] (streamable HTTP, or [`stdio`]
//! for Microsoft's CLI) with a bearer from a [`TokenSource`]. [`connect`] builds the client
//! for a profile's account. [`content`] is the content policy for what the client returns.
//! [`document`] reads a spec in a SharePoint or OneDrive document for `spec import --from`.

use std::sync::Arc;

use anyhow::Result;

use crate::profile::{Profile, WorkIqConnection};

pub mod account;
pub mod audit;
pub mod changes;
pub mod client;
pub mod content;
pub mod context;
pub mod doctor;
pub mod document;
pub mod gateway;
pub mod import;
pub mod login;
pub mod notice;
pub mod stdio;

pub use client::*;

/// Where the hosted service and its sign-in are; tests point them at fakes.
#[derive(Debug, Clone)]
pub struct Endpoints {
    pub mcp_url: String,
    pub authority: String,
    /// The resource's OAuth protected-resource metadata, which `workiq doctor` reads.
    pub discovery_url: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Endpoints {
            mcp_url: DEFAULT_URL.to_string(),
            authority: account::DEFAULT_AUTHORITY.to_string(),
            discovery_url: doctor::DISCOVERY_URL.to_string(),
        }
    }
}

/// The test-only settings that point the program at a fake Work IQ (it serves `/mcp` and the
/// discovery document) and a fake Entra, so tests can run the program itself against them.
pub const TEST_WORKIQ_URL: &str = "PROOFKIN_TEST_WORKIQ_URL";
pub const TEST_ENTRA_URL: &str = "PROOFKIN_TEST_ENTRA_URL";

impl Endpoints {
    /// The real service's addresses, unless [`TEST_WORKIQ_URL`] or [`TEST_ENTRA_URL`] point
    /// them at fakes. Those take only an address on this machine, so they can't send a token
    /// anywhere else; any other value is an error, never a fallback to the real service.
    pub fn configured() -> Result<Endpoints> {
        Self::with_test_settings(
            setting(TEST_WORKIQ_URL).as_deref(),
            setting(TEST_ENTRA_URL).as_deref(),
        )
    }

    fn with_test_settings(workiq: Option<&str>, entra: Option<&str>) -> Result<Endpoints> {
        let mut endpoints = Endpoints::default();
        if let Some(base) = workiq {
            let base = loopback_base(TEST_WORKIQ_URL, base)?;
            endpoints.mcp_url = format!("{base}/mcp");
            endpoints.discovery_url = format!("{base}/.well-known/oauth-protected-resource/mcp");
        }
        if let Some(base) = entra {
            endpoints.authority = loopback_base(TEST_ENTRA_URL, base)?;
        }
        Ok(endpoints)
    }
}

/// A test-only setting, by its full name (`P2PAGENTS_` works too, as for every setting).
fn setting(var: &str) -> Option<String> {
    crate::profile::env_setting(var.strip_prefix("PROOFKIN_").unwrap_or(var))
}

/// `value` without a trailing `/` if it is the root of an `http(s)://` address on the loopback
/// interface (`localhost` or a loopback IP), with no path, query or credentials.
fn loopback_base(var: &str, value: &str) -> Result<String> {
    let refuse = || {
        anyhow::anyhow!(
            "{var} is only for tests and takes an address on this machine, such as \
             http://127.0.0.1:8080, not {value:?}"
        )
    };
    let url = reqwest::Url::parse(value).map_err(|_| refuse())?;
    let loopback = match url.host_str() {
        Some(host) if host.eq_ignore_ascii_case("localhost") => true,
        Some(host) => host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback()),
        None => false,
    };
    let bare = url.path() == "/"
        && url.query().is_none()
        && url.fragment().is_none()
        && url.username().is_empty()
        && url.password().is_none();
    if !matches!(url.scheme(), "http" | "https") || !loopback || !bare {
        return Err(refuse());
    }
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

/// A client for an account, with the hosted account's token source (the client's own), which
/// a diagnosis reads claims from.
pub struct Connection {
    pub client: Client,
    pub tokens: Option<Arc<account::HostedTokens>>,
}

/// The Work IQ client for the account `profile` uses: the hosted service with Proofkin's own
/// sign-in, or Microsoft's CLI over stdio, which signs in by itself.
pub fn connect(profile: &Profile) -> Result<Client> {
    Ok(connect_at(profile, &Endpoints::configured()?)?.client)
}

/// [`connect`] to the hosted service and sign-in at `endpoints`.
pub fn connect_at(profile: &Profile, endpoints: &Endpoints) -> Result<Connection> {
    let (_, account) = profile.workiq_account()?;
    match account.connection {
        WorkIqConnection::Hosted { .. } => {
            let tokens = account::HostedTokens::new(profile)?.with_authority(&endpoints.authority);
            let tokens = Arc::new(tokens);
            Ok(Connection {
                client: Client::new(&endpoints.mcp_url, tokens.clone())?,
                tokens: Some(tokens),
            })
        }
        WorkIqConnection::Cli { account, program } => Ok(Connection {
            client: Client::with_transport(
                Arc::new(stdio::StdioTransport::new(
                    program.as_deref(),
                    account.as_deref(),
                )),
                Arc::new(stdio::NoToken),
            ),
            tokens: None,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_test_settings_take_only_an_address_on_this_machine() {
        let real = Endpoints::with_test_settings(None, None).unwrap();
        assert_eq!(real.mcp_url, DEFAULT_URL);
        assert_eq!(real.authority, account::DEFAULT_AUTHORITY);

        let fake = Endpoints::with_test_settings(
            Some("http://127.0.0.1:8080/"),
            Some("http://localhost:9090"),
        )
        .unwrap();
        assert_eq!(fake.mcp_url, "http://127.0.0.1:8080/mcp");
        assert_eq!(
            fake.discovery_url,
            "http://127.0.0.1:8080/.well-known/oauth-protected-resource/mcp"
        );
        assert_eq!(fake.authority, "http://localhost:9090");
        assert!(Endpoints::with_test_settings(Some("http://[::1]:8080"), None).is_ok());

        for bad in [
            "https://workiq.svc.cloud.microsoft",
            "http://10.0.0.5:8080",
            "http://127.0.0.1.example.com",
            "http://user:secret@127.0.0.1:8080",
            "http://127.0.0.1:8080/mcp",
            "http://127.0.0.1:8080/?next=1",
            "ftp://127.0.0.1",
            "127.0.0.1:8080",
        ] {
            let err = Endpoints::with_test_settings(Some(bad), None).unwrap_err();
            assert!(
                err.to_string().contains(TEST_WORKIQ_URL),
                "{bad} was taken: {err}"
            );
            let err = Endpoints::with_test_settings(None, Some(bad)).unwrap_err();
            assert!(err.to_string().contains(TEST_ENTRA_URL), "{bad}: {err}");
        }
    }
}
