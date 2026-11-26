//! What a plan's agent asks Work IQ for while it runs a step with `workiq=`, behind a trait
//! so tests can use a fake. The real one is an audited [`Client`] for the plan's profile.

use std::fmt;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use super::audit::{Audit, CallContext};
use super::client::{Client, ToolResult};
use super::connect;
use super::content::read_agents;
use crate::profile::Profile;

/// The Work IQ calls a run makes for its steps.
#[async_trait]
pub trait WorkContext: Send + Sync + fmt::Debug {
    /// A cited search for `query` on behalf of `context`; `strategy` is `grounding` or
    /// `copilot`, `capabilities` Work IQ's capability names (none for all).
    async fn retrieve(
        &self,
        context: &CallContext,
        query: &str,
        strategy: &str,
        capabilities: &[&str],
    ) -> Result<ToolResult>;

    /// An answer from Microsoft 365 Copilot, or from the declarative agent `agent_id`, to
    /// `question` on behalf of `context`.
    async fn ask(
        &self,
        _context: &CallContext,
        _question: &str,
        _agent_id: Option<&str>,
    ) -> Result<ToolResult> {
        anyhow::bail!("this Work IQ context can't ask")
    }

    /// Entities by their Work IQ paths (`fetch`) on behalf of `context`.
    async fn fetch(&self, _context: &CallContext, _entity_urls: &[&str]) -> Result<ToolResult> {
        anyhow::bail!("this Work IQ context can't fetch documents")
    }

    /// A file's bytes by its Work IQ path (`fetch_blob`) on behalf of `context`.
    async fn fetch_blob(&self, _context: &CallContext, _path: &str) -> Result<ToolResult> {
        anyhow::bail!("this Work IQ context can't fetch documents")
    }
}

/// The declarative agents Work IQ offers `client`'s account, one per line (name, then the id
/// `agent=` takes on a `@workiq` step), as `proofkin workiq agents` prints them.
pub async fn agents_listing(client: &Client) -> Result<String> {
    let result = client.list_agents().await?;
    let agents = read_agents(&result)?;
    if agents.is_empty() {
        return Ok("Work IQ lists no agents for this account.\n".to_owned());
    }
    let width = agents
        .iter()
        .map(|a| a.name.chars().count())
        .max()
        .unwrap_or(0);
    Ok(agents
        .iter()
        .map(|a| format!("{:<width$}  {}\n", a.name, a.id))
        .collect())
}

/// [`WorkContext`] through the Work IQ client, which audits the calls and holds them to the
/// profile's budgets.
pub struct ClientWork {
    client: Client,
}

impl ClientWork {
    pub fn new(client: Client) -> Self {
        ClientWork { client }
    }

    /// Work IQ through the account `profile` uses, audited for `profile`.
    pub fn for_profile(profile: &Profile) -> Result<Self> {
        Ok(Self::new(client_for_profile(profile)?))
    }
}

/// A client for Work IQ through the account `profile` uses (hosted or the CLI), audited for
/// `profile`.
pub fn client_for_profile(profile: &Profile) -> Result<Client> {
    let audit = Arc::new(Audit::for_profile(profile));
    Ok(connect(profile)?.with_audit(audit))
}

impl fmt::Debug for ClientWork {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ClientWork")
    }
}

#[async_trait]
impl WorkContext for ClientWork {
    async fn retrieve(
        &self,
        context: &CallContext,
        query: &str,
        strategy: &str,
        capabilities: &[&str],
    ) -> Result<ToolResult> {
        self.client
            .retrieve_in(context, query, Some(strategy), capabilities)
            .await
    }

    async fn ask(
        &self,
        context: &CallContext,
        question: &str,
        agent_id: Option<&str>,
    ) -> Result<ToolResult> {
        self.client.ask_in(context, question, agent_id, None).await
    }

    async fn fetch(&self, context: &CallContext, entity_urls: &[&str]) -> Result<ToolResult> {
        self.client.fetch_in(context, entity_urls).await
    }

    async fn fetch_blob(&self, context: &CallContext, path: &str) -> Result<ToolResult> {
        self.client.fetch_blob_in(context, path).await
    }
}
