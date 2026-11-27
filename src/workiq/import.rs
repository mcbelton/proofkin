//! `proofkin kb import workiq`: keeps what a cited Work IQ search found as a knowledge topic,
//! marked as Microsoft 365 content with the labels of the results kept, so `context=` can use
//! it and the plan check keeps it from peers that may not have it.

use anyhow::{Context, Result, bail};

use super::audit::{CallContext, Purpose};
use super::content::{Audience, DEFAULT_CAP, retrieval_section};
use super::context::{WorkContext, retrieve_with_fallback};
use crate::knowledge::{KnowledgeBase, with_mark};
use crate::plan::{WORKIQ_QUERY_CHARS, parse_workiq_sources, workiq_request_for};
use crate::profile::validate_name;

/// The note kept with an imported topic.
const SOURCE: &str = "Work IQ retrieve";

/// What an import wrote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Imported {
    pub topic: String,
    /// Results kept in the topic.
    pub kept: usize,
    /// Results left out: blocked, or without a protection status.
    pub withheld: usize,
    /// The labels of the results kept (`unknown` for one without label information).
    pub labels: Vec<String>,
    /// What to tell the owner when the import fell back to grounding.
    pub fallback_note: Option<&'static str>,
}

/// Searches Work IQ for `query` over `sources` (as `workiq=` takes them; none: `all`) through
/// `work`, and saves what it found as `topic` in `kb`. An existing topic needs `replace`.
/// Nothing is written when no result was kept.
pub async fn import_knowledge(
    work: &dyn WorkContext,
    kb: &KnowledgeBase,
    topic: &str,
    query: &str,
    sources: Option<&str>,
    replace: bool,
) -> Result<Imported> {
    validate_name(topic).map_err(|_| anyhow::anyhow!("invalid topic name {topic:?}"))?;
    let query = query.split_whitespace().collect::<Vec<_>>().join(" ");
    if query.is_empty() {
        bail!("a Work IQ import needs a query: --query \"what to look for\"");
    }
    let query: String = query.chars().take(WORKIQ_QUERY_CHARS).collect();
    let sources = parse_workiq_sources(sources.unwrap_or("all"))?;
    if !replace && kb.read(topic).is_ok() {
        bail!("the topic '{topic}' exists already; import with --replace to overwrite it");
    }
    let Some((strategy, capabilities)) = workiq_request_for(&sources) else {
        bail!("a Work IQ import needs sources");
    };

    let call = CallContext::local(Purpose::KnowledgeImport);
    let mut fallback_note = None;
    let result = retrieve_with_fallback(work, &call, &query, strategy, &capabilities, |note| {
        fallback_note = Some(note);
    })
    .await?;
    let saved = retrieval_section(&result, &Audience::Local, DEFAULT_CAP).and_then(|section| {
        if section.kept == 0 {
            let why = section
                .withheld
                .line()
                .unwrap_or_else(|| "Work IQ found nothing for the query.".to_owned());
            bail!("nothing was imported, and the topic '{topic}' was not written. {why}");
        }

        let retrieved = chrono::Local::now().format("%Y-%m-%d");
        let text = format!(
            "---\nquery: {query}\nretrieved: {retrieved}\n---\n{}",
            section.text
        );
        kb.write(topic, &with_mark(&text, &section.labels), SOURCE)?;
        Ok(Imported {
            topic: topic.to_owned(),
            kept: section.kept,
            withheld: section.withheld.total(),
            labels: section.labels,
            fallback_note,
        })
    });
    match fallback_note {
        Some(note) => saved.context(note),
        None => saved,
    }
}
