//! The knowledge base: what agents have learned about a system, kept as Markdown
//! topics in the profile (`knowledge/<topic>.md`, private to its owner). Plan steps
//! add to it (`save=topic`, only from verified results) and use it (`context=topics`
//! puts those topics in the step's prompt), so reverse-engineered architecture,
//! risks, design decisions and operating insights build up across runs.
//!
//! Troubleshooting guides (TSGs) are topics that start with front matter:
//!
//! ```text
//! ---
//! tsg: api-down
//! monitor: api-health
//! match: connection refused
//! fix: restart
//! verify: api-health
//! status: active
//! ---
//! # The API refuses connections
//! ...
//! ```
//!
//! When monitor `api-health` fails with output matching `match` (a
//! case-insensitive substring), the monitor's agent runs the folder owner's check
//! `fix`, then `verify` (default: the monitor's own check). See [`crate::ops`].
//!
//! Topics made from Microsoft 365 content (Work IQ) keep their origin in the same front
//! matter, and a topic can be both that and a guide:
//!
//! ```text
//! ---
//! origin: workiq
//! labels: General, Confidential
//! ---
//! ```
//!
//! See [`parse_mark`] and [`with_mark`]; plans use the mark to keep the content from peers
//! that may not have it (docs/work-iq.md, section 4).

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::profile::validate_name;

/// Most knowledge put into one prompt.
pub const MAX_CONTEXT_CHARS: usize = 24_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Topic {
    pub name: String,
    pub chars: usize,
    pub modified_unix: u64,
    /// For troubleshooting guides.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tsg: Option<Tsg>,
    /// For topics made from Microsoft 365 content: its sensitivity labels.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workiq: Option<Vec<String>>,
}

/// A troubleshooting guide's machine-readable part.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Tsg {
    pub topic: String,
    /// Monitor it applies to (None: any).
    pub monitor: Option<String>,
    /// Case-insensitive text the failure output must contain (None: any failure).
    pub matches: Option<String>,
    /// The folder owner's check that fixes it.
    pub fix: String,
    /// The check that confirms the fix (None: the monitor's check).
    pub verify: Option<String>,
    /// `active` guides run automatically; `draft` ones wait for a person to review.
    pub status: String,
}

impl Tsg {
    pub fn applies(&self, monitor: &str, output: &str) -> bool {
        self.status == "active"
            && self.monitor.as_deref().is_none_or(|m| m == monitor)
            && self
                .matches
                .as_deref()
                .is_none_or(|m| output.to_lowercase().contains(&m.to_lowercase()))
    }

    /// The guide as a topic: front matter, then `body`.
    pub fn render(&self, body: &str) -> String {
        let mut text = format!("---\ntsg: {}\n", self.topic);
        if let Some(m) = &self.monitor {
            text += &format!("monitor: {m}\n");
        }
        if let Some(m) = &self.matches {
            text += &format!("match: {}\n", m.replace('\n', " "));
        }
        text += &format!("fix: {}\n", self.fix);
        if let Some(v) = &self.verify {
            text += &format!("verify: {v}\n");
        }
        text += &format!("status: {}\n---\n{}", self.status, body.trim_start());
        text
    }
}

/// The front matter of `text` (the lines between the `---` lines) and what follows its
/// closing `---`.
fn front_matter(text: &str) -> Option<(&str, &str)> {
    let rest = text.strip_prefix("---")?.trim_start_matches(['\r', '\n']);
    rest.split_once("\n---")
}

/// `labels` and `more` together, without repeats (compared without case), in order.
pub fn union_labels(labels: &[String], more: &[String]) -> Vec<String> {
    let mut all: Vec<String> = Vec::new();
    for label in labels.iter().chain(more) {
        let label = label.trim();
        if !label.is_empty() && !all.iter().any(|l| l.eq_ignore_ascii_case(label)) {
            all.push(label.to_owned());
        }
    }
    all
}

/// The Microsoft 365 mark in a topic's front matter (`origin: workiq`): its labels
/// (`labels: a, b`, none if the line is missing), or `None` if the topic isn't marked.
pub fn parse_mark(text: &str) -> Option<Vec<String>> {
    let (head, _) = front_matter(text)?;
    let (mut marked, mut labels) = (false, Vec::new());
    for line in head.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        match key.trim() {
            "origin" => marked |= value.trim().eq_ignore_ascii_case("workiq"),
            "labels" => {
                let listed: Vec<String> = value.split(',').map(str::to_owned).collect();
                labels = union_labels(&[], &listed);
            }
            _ => {}
        }
    }
    marked.then_some(labels)
}

/// `text` marked as Microsoft 365 content with `labels`: the `origin` and `labels` lines go
/// into its front matter (replacing a mark it has, keeping its other lines, a guide's
/// included), or into new front matter.
pub fn with_mark(text: &str, labels: &[String]) -> String {
    let labels: Vec<String> = labels
        .iter()
        .map(|l| l.replace([',', '\r', '\n'], " "))
        .collect();
    let mark = format!(
        "origin: workiq\nlabels: {}",
        union_labels(&[], &labels).join(", ")
    );
    let Some((head, after)) = front_matter(text) else {
        return format!("---\n{mark}\n---\n{text}");
    };
    let is_mark = |line: &&str| {
        let key = line.split_once(':').map(|(k, _)| k.trim());
        matches!(key, Some("origin" | "labels"))
    };
    let mut lines: Vec<&str> = head.lines().filter(|l| !is_mark(l)).collect();
    lines.push(&mark);
    format!("---\n{}\n---{after}", lines.join("\n"))
}

/// Parses a topic's TSG front matter, if it has any.
pub fn parse_tsg(topic: &str, text: &str) -> Option<Tsg> {
    let (head, _) = front_matter(text)?;
    let mut tsg = Tsg {
        topic: topic.to_owned(),
        monitor: None,
        matches: None,
        fix: String::new(),
        verify: None,
        status: "active".into(),
    };
    let mut is_tsg = false;
    for line in head.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim().to_owned();
        let value = Some(value).filter(|v| !v.is_empty());
        match key.trim() {
            "tsg" => is_tsg = true,
            "monitor" => tsg.monitor = value,
            "match" => tsg.matches = value,
            "fix" => tsg.fix = value.unwrap_or_default(),
            "verify" => tsg.verify = value,
            "status" => tsg.status = value.unwrap_or_else(|| "active".into()),
            _ => {}
        }
    }
    (is_tsg && !tsg.fix.is_empty()).then_some(tsg)
}

#[derive(Debug, Clone)]
pub struct KnowledgeBase {
    dir: PathBuf,
}

impl KnowledgeBase {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path(&self, topic: &str) -> Result<PathBuf> {
        validate_name(topic).with_context(|| format!("invalid topic name {topic:?}"))?;
        Ok(self.dir.join(format!("{topic}.md")))
    }

    pub fn read(&self, topic: &str) -> Result<String> {
        let path = self.path(topic)?;
        std::fs::read_to_string(&path).with_context(|| format!("there is no topic '{topic}'"))
    }

    /// Writes `topic`, with a note of where the knowledge came from. Secrets are
    /// redacted: knowledge goes into prompts that other peers' models read.
    pub fn write(&self, topic: &str, text: &str, source: &str) -> Result<()> {
        let path = self.path(topic)?;
        std::fs::create_dir_all(&self.dir)?;
        let (text, _) = crate::redact::redact(text);
        let text = if text.starts_with("---") || source.is_empty() {
            text
        } else {
            format!(
                "<!-- source: {} -->\n{}",
                source.replace("--", "-"),
                text.trim()
            )
        };
        std::fs::write(&path, text + "\n").with_context(|| format!("writing {}", path.display()))
    }

    pub fn remove(&self, topic: &str) -> Result<bool> {
        let path = self.path(topic)?;
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// Topics, by name.
    pub fn list(&self) -> Vec<Topic> {
        let mut topics = Vec::new();
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return topics;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path
                .file_stem()
                .and_then(|s| s.to_str())
                .filter(|_| path.extension().is_some_and(|e| e == "md"))
            else {
                continue;
            };
            if validate_name(name).is_err() {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let modified_unix = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_secs());
            topics.push(Topic {
                name: name.to_owned(),
                chars: text.chars().count(),
                modified_unix,
                tsg: parse_tsg(name, &text),
                workiq: parse_mark(&text),
            });
        }
        topics.sort_by(|a, b| a.name.cmp(&b.name));
        topics
    }

    pub fn tsgs(&self) -> Vec<Tsg> {
        self.list().into_iter().filter_map(|t| t.tsg).collect()
    }

    /// The labels of the Microsoft 365 content in `topics` (`all`: every topic), or `None`
    /// if none of them is marked. Topics that don't exist are left out.
    pub fn marks(&self, topics: &[String]) -> Option<Vec<String>> {
        let all = topics.iter().any(|t| t == "all");
        let mut labels: Option<Vec<String>> = None;
        for topic in self.list() {
            if let Some(found) = topic.workiq
                && (all || topics.contains(&topic.name))
            {
                labels = Some(union_labels(labels.as_deref().unwrap_or_default(), &found));
            }
        }
        labels
    }

    /// Topics whose name or text contains every word of `query` (case-insensitive).
    pub fn search(&self, query: &str) -> Vec<String> {
        let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
        self.list()
            .into_iter()
            .filter(|t| {
                let text = self.read(&t.name).unwrap_or_default().to_lowercase() + &t.name;
                words.iter().all(|w| text.contains(w))
            })
            .map(|t| t.name)
            .collect()
    }

    /// The knowledge for a prompt: the named topics (`all`: every topic, TSGs last),
    /// up to [`MAX_CONTEXT_CHARS`]. Unknown topics are an error.
    pub fn context(&self, topics: &[String]) -> Result<String> {
        let names: Vec<String> = if topics.iter().any(|t| t == "all") {
            let mut all = self.list();
            all.sort_by_key(|t| t.tsg.is_some());
            all.into_iter().map(|t| t.name).collect()
        } else {
            topics.to_vec()
        };
        let mut text = String::new();
        for name in &names {
            let body = self.read(name)?;
            let section = format!(
                "\n### Knowledge: {name}\n{}\n",
                crate::boundary::wrap(&format!("knowledge:{name}"), body.trim())
            );
            if text.chars().count() + section.chars().count() > MAX_CONTEXT_CHARS {
                text += &format!("\n[... knowledge from '{name}' on left out: too long]\n");
                break;
            }
            text += &section;
        }
        Ok(text)
    }
}

/// Fails unless every topic in `topics` exists (or is `all`).
pub fn check_topics(kb: &KnowledgeBase, topics: &[String]) -> Result<()> {
    for topic in topics.iter().filter(|t| *t != "all") {
        if kb.read(topic).is_err() {
            bail!(
                "the knowledge base has no topic '{topic}' (add it with a step's save={topic}, or proofkin kb add)"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topics_are_written_listed_searched_and_given_as_context() {
        let dir = tempfile::tempdir().unwrap();
        let kb = KnowledgeBase::new(dir.path().join("knowledge"));
        assert!(kb.list().is_empty());
        kb.write(
            "architecture",
            "The API talks to Postgres.",
            "run 1 step arch by alice (verified)",
        )
        .unwrap();
        kb.write("glossary", "SKU: stock keeping unit", "").unwrap();
        assert!(
            kb.write("../escape", "x", "").is_err(),
            "topics are plain names"
        );
        let names: Vec<String> = kb.list().into_iter().map(|t| t.name).collect();
        assert_eq!(names, ["architecture", "glossary"]);
        assert!(
            kb.read("architecture")
                .unwrap()
                .starts_with("<!-- source: run 1 step arch")
        );
        assert_eq!(kb.search("postgres API"), ["architecture"]);
        assert!(kb.search("postgres sku").is_empty());

        let ctx = kb.context(&["glossary".into()]).unwrap();
        assert!(ctx.contains(
            "### Knowledge: glossary\n<external_content source=\"knowledge:glossary\">\nSKU"
        ));
        assert!(!ctx.contains("Postgres"));
        assert!(kb.context(&["all".into()]).unwrap().contains("Postgres"));
        assert!(kb.context(&["missing".into()]).is_err());
        assert!(check_topics(&kb, &["missing".into()]).is_err());
        check_topics(&kb, &["all".into(), "glossary".into()]).unwrap();

        kb.write("huge", &"x".repeat(MAX_CONTEXT_CHARS), "")
            .unwrap();
        let capped = kb.context(&["architecture".into(), "huge".into()]).unwrap();
        assert!(
            capped.contains("left out: too long") && capped.chars().count() < MAX_CONTEXT_CHARS
        );
        assert!(kb.remove("huge").unwrap() && !kb.remove("huge").unwrap());
    }

    #[test]
    fn troubleshooting_guides_parse_and_match() {
        let dir = tempfile::tempdir().unwrap();
        let kb = KnowledgeBase::new(dir.path());
        let tsg = Tsg {
            topic: "api-down".into(),
            monitor: Some("api-health".into()),
            matches: Some("Connection Refused".into()),
            fix: "restart".into(),
            verify: None,
            status: "active".into(),
        };
        kb.write("api-down", &tsg.render("# API down\nRestart it."), "")
            .unwrap();
        kb.write("notes", "--- not front matter", "").unwrap();
        let found = kb.tsgs();
        assert_eq!(found, std::slice::from_ref(&tsg));
        assert!(tsg.applies("api-health", "curl: (7) connection refused"));
        assert!(!tsg.applies("api-health", "HTTP 500"), "other failures");
        assert!(
            !tsg.applies("db-health", "connection refused"),
            "other monitors"
        );
        let draft = Tsg {
            status: "draft".into(),
            ..tsg
        };
        assert!(
            !draft.applies("api-health", "connection refused"),
            "drafts don't run"
        );
        assert_eq!(parse_tsg("x", "---\ntsg: x\nmatch: y\n---\nno fix"), None);
    }

    #[test]
    fn a_topics_microsoft_365_mark_round_trips_with_and_without_guide_front_matter() {
        let dir = tempfile::tempdir().unwrap();
        let kb = KnowledgeBase::new(dir.path());
        let labels = vec!["General".to_owned(), "Highly Confidential".to_owned()];

        kb.write("notes", &with_mark("# Notes\nBody", &labels), "")
            .unwrap();
        let text = kb.read("notes").unwrap();
        assert!(
            text.starts_with(
                "---\norigin: workiq\nlabels: General, Highly Confidential\n---\n# Notes\nBody"
            ),
            "{text}"
        );
        assert_eq!(parse_mark(&text), Some(labels.clone()));
        assert_eq!(parse_tsg("notes", &text), None);

        // A topic can be a guide and Microsoft 365 content at once.
        let tsg = Tsg {
            topic: "api-down".into(),
            monitor: Some("api-health".into()),
            matches: Some("refused".into()),
            fix: "restart".into(),
            verify: None,
            status: "active".into(),
        };
        let guide = with_mark(&tsg.render("# API down\nRestart it."), &labels);
        kb.write("api-down", &guide, "").unwrap();
        let text = kb.read("api-down").unwrap();
        assert_eq!(parse_mark(&text), Some(labels.clone()));
        assert_eq!(kb.tsgs(), std::slice::from_ref(&tsg));
        assert!(text.contains("---\n# API down\nRestart it."), "{text}");
        let again = with_mark(&text, &["Public".to_owned()]);
        assert_eq!(parse_mark(&again), Some(vec!["Public".to_owned()]));
        assert_eq!(parse_tsg("api-down", &again), Some(tsg.clone()));
        assert_eq!(again.matches("origin:").count(), 1, "{again}");

        // What isn't marked.
        assert_eq!(parse_mark(&tsg.render("x")), None);
        assert_eq!(parse_mark("origin: workiq\nlabels: A"), None);
        assert_eq!(parse_mark("---\norigin: other\nlabels: A\n---\nx"), None);
        assert_eq!(parse_mark("---\norigin: workiq\n---\nx"), Some(vec![]));

        let listed = kb.list();
        assert_eq!(listed[0].workiq, Some(labels.clone()));
        assert_eq!(kb.marks(&["notes".into(), "api-down".into()]), Some(labels));
        assert_eq!(kb.marks(&["missing".into()]), None);
    }
}
