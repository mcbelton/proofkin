//! Work IQ: Microsoft 365 work context for plans and tasks (see docs/work-iq.md).
//!
//! [`client`] is the MCP client. It talks to a [`Transport`] (streamable HTTP, or [`stdio`]
//! for Microsoft's CLI) with a bearer from a [`TokenSource`]. [`connect`] builds the client
//! for a profile's account. [`content`] is the content policy for what the client returns.

use std::sync::Arc;

use anyhow::Result;

use crate::profile::{Profile, WorkIqConnection};

pub mod account;
pub mod audit;
pub mod client;
pub mod content;
pub mod context;
pub mod doctor;
pub mod import;
pub mod login;
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

/// A client for an account, with the hosted account's token source (the client's own), which
/// a diagnosis reads claims from.
pub struct Connection {
    pub client: Client,
    pub tokens: Option<Arc<account::HostedTokens>>,
}

/// The Work IQ client for the account `profile` uses: the hosted service with Proofkin's own
/// sign-in, or Microsoft's CLI over stdio, which signs in by itself.
pub fn connect(profile: &Profile) -> Result<Client> {
    Ok(connect_at(profile, &Endpoints::default())?.client)
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
        WorkIqConnection::Cli { tenant } => Ok(Connection {
            client: Client::with_transport(
                Arc::new(stdio::StdioTransport::new(tenant.as_deref())),
                Arc::new(stdio::NoToken),
            ),
            tokens: None,
        }),
    }
}
