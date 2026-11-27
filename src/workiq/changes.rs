//! Microsoft 365 changes (docs/work-iq.md, section 7). A plan step's model never changes
//! Microsoft 365 itself: it proposes changes in one fenced `m365-changes` block of its result
//! ([`proposed`]), its owner signs off exactly that list (shown by [`preview`], with the
//! list's [`digest`]), and a `@workiq apply=STEP` step makes them once, in order, reporting
//! how each ended ([`report`]). In a digital worker's task for its owner, the Work IQ gateway
//! reads each change call ([`Change::from_arguments`]), holds it to the same rules
//! ([`check`]) and has the owner sign off exactly that change ([`preview_one`]) before it
//! makes it once (see [`crate::workiq::gateway`]).

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::workiq::{Class, ToolResult, WorkIqError};

/// The language of the fenced block a step proposes its changes in.
pub const FENCE: &str = "m365-changes";
/// The Work IQ tools a change calls.
pub const TOOLS: [&str; 4] = [
    "create_entity",
    "update_entity",
    "delete_entity",
    "do_action",
];
/// The most changes a step proposes.
pub const MAX_CHANGES: usize = 20;
/// The largest block, in bytes.
pub const MAX_BLOCK_BYTES: usize = 256 * 1024;
/// The longest path, in bytes.
pub const MAX_PATH_BYTES: usize = 2048;
/// The longest ETag, in bytes.
const MAX_ETAG_BYTES: usize = 1024;
/// What a sign-off shows of a mail's text, and of another change's body.
const MAIL_LINES: usize = 10;
const MAIL_CHARS: usize = 800;
const BODY_CHARS: usize = 500;

/// One proposed change: a Work IQ change tool, the Microsoft Graph path it acts on, its JSON
/// body (none for a delete) and, for an update or a delete, the entity's ETag.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Change {
    pub tool: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<Value>,
    #[serde(default, rename = "ifMatch", skip_serializing_if = "Option::is_none")]
    pub if_match: Option<String>,
}

impl Change {
    /// The arguments of its Work IQ tool.
    pub fn arguments(&self) -> Value {
        let body = self.body.clone().unwrap_or_else(|| json!({}));
        let mut arguments = match self.tool.as_str() {
            "create_entity" => json!({ "parentUrl": self.path, "jsonBody": body }),
            "update_entity" => json!({ "entityUrl": self.path, "jsonBody": body }),
            "delete_entity" => json!({ "entityUrl": self.path }),
            _ => json!({ "actionUrl": self.path, "jsonBody": body }),
        };
        if let Some(tag) = &self.if_match {
            arguments["headers"] = json!({ "If-Match": tag });
        }
        arguments
    }

    /// The change a call of Work IQ's change tool `tool` with `arguments` makes: the inverse
    /// of [`Change::arguments`]. Its path is `parentUrl`, `entityUrl` or `actionUrl`;
    /// `jsonBody` is a JSON object or one encoded as a string; `headers` has only
    /// `If-Match`. An error says what is wrong; [`check`] then holds it to the rules.
    pub fn from_arguments(tool: &str, arguments: &Value) -> std::result::Result<Change, String> {
        let url = match tool {
            "create_entity" => "parentUrl",
            "update_entity" | "delete_entity" => "entityUrl",
            "do_action" => "actionUrl",
            _ => {
                return Err(format!(
                    "tool is create_entity, update_entity, delete_entity or do_action, not {:?}",
                    cut(tool, 40)
                ));
            }
        };
        let Some(given) = arguments.as_object() else {
            return Err(format!("{tool}'s arguments are a JSON object"));
        };
        let unknown: Vec<&str> = given
            .keys()
            .map(String::as_str)
            .filter(|key| ![url, "jsonBody", "headers"].contains(key))
            .collect();
        if !unknown.is_empty() {
            return Err(format!(
                "{tool} takes {url}, jsonBody and headers, not {}",
                cut(&unknown.join(", "), 200)
            ));
        }
        let Some(Value::String(path)) = given.get(url) else {
            return Err(format!(
                "{url} is the Microsoft Graph path, as a string (like /me/sendMail)"
            ));
        };
        let body = match given.get("jsonBody") {
            None | Some(Value::Null) => None,
            Some(Value::String(text)) => Some(serde_json::from_str(text).map_err(|e| {
                format!("jsonBody is a JSON object, or one encoded as a string ({e})")
            })?),
            Some(body) => Some(body.clone()),
        };
        let if_match = match given.get("headers") {
            None | Some(Value::Null) => None,
            Some(Value::Object(headers)) => {
                let mut tag = None;
                for (name, value) in headers {
                    if !name.eq_ignore_ascii_case("If-Match") || tag.is_some() {
                        return Err(format!(
                            "headers has only one If-Match, not {:?}",
                            cut(name, 40)
                        ));
                    }
                    let Value::String(value) = value else {
                        return Err("If-Match is the entity's ETag, as a string".into());
                    };
                    tag = Some(value.clone());
                }
                tag
            }
            Some(_) => return Err("headers is a JSON object with If-Match".into()),
        };
        Ok(Change {
            tool: tool.to_owned(),
            path: path.clone(),
            body,
            if_match,
        })
    }

    fn verb(&self) -> &'static str {
        match self.tool.as_str() {
            "create_entity" => "Create",
            "update_entity" => "Update",
            "delete_entity" => "Delete",
            _ => "Action",
        }
    }

    /// The mail it sends, if it is a `sendMail` action.
    fn mail(&self) -> Option<&Value> {
        let segments: Vec<&str> = self
            .path
            .split('?')
            .next()
            .unwrap_or_default()
            .trim_matches('/')
            .split('/')
            .collect();
        let sends = match segments.as_slice() {
            [me, action] => {
                me.eq_ignore_ascii_case("me") && action.eq_ignore_ascii_case("sendMail")
            }
            [users, _, action] => {
                users.eq_ignore_ascii_case("users") && action.eq_ignore_ascii_case("sendMail")
            }
            _ => false,
        };
        (self.tool == "do_action" && sends)
            .then(|| self.body.as_ref()?.get("message"))
            .flatten()
    }

    /// How a result names it: its verb and path.
    pub fn label(&self) -> String {
        format!("{} {}", self.verb(), cut(&self.path, 120))
    }
}

/// The instruction in front of the task of a step that a later step applies.
pub fn instruction() -> String {
    format!(
        "## Microsoft 365 changes\n\n\
         Don't change Microsoft 365 yourself. Propose the changes instead: your owner signs \
         them off, then they are made once, in order, with your owner's account. Write them in \
         one fenced block in your result, opened with ```{FENCE} and closed with ```, one JSON \
         object a line:\n\n\
         ```{FENCE}\n\
         {{\"tool\":\"do_action\",\"path\":\"/me/sendMail\",\"body\":{{\"message\":{{\"subject\":\"...\",\"body\":{{\"contentType\":\"Text\",\"content\":\"...\"}},\"toRecipients\":[{{\"emailAddress\":{{\"address\":\"...\"}}}}]}}}}}}\n\
         {{\"tool\":\"update_entity\",\"path\":\"/me/events/...\",\"body\":{{\"subject\":\"...\"}},\"ifMatch\":\"W/\\\"...\\\"\"}}\n\
         ```\n\n\
         `tool` is create_entity, update_entity, delete_entity or do_action; `path` is the \
         Microsoft Graph path (it starts with /, with no host); `body` is the JSON body (none \
         for delete_entity); `ifMatch` is the entity's ETag, for an update or a delete. At \
         most {MAX_CHANGES} changes. With nothing to change, write no block.\n"
    )
}

/// The changes `output` proposes: its `m365-changes` block, checked (none if it has no
/// block). A block that doesn't parse or breaks a rule is an error naming the line and the
/// rule.
pub fn proposed(output: &str) -> Result<Option<Vec<Change>>> {
    let mut lines = output.lines();
    let mut block: Option<Vec<&str>> = None;
    while let Some(line) = lines.next() {
        let Some(info) = line.trim().strip_prefix("```") else {
            continue;
        };
        if info.trim() != FENCE {
            continue;
        }
        if block.is_some() {
            return Err(anyhow!(
                "the result has more than one {FENCE} block: put all the changes in one"
            ));
        }
        let mut body = Vec::new();
        let mut closed = false;
        for line in lines.by_ref() {
            if line.trim().starts_with("```") {
                closed = true;
                break;
            }
            body.push(line);
        }
        if !closed {
            return Err(anyhow!("the {FENCE} block isn't closed with ```"));
        }
        block = Some(body);
    }
    let Some(block) = block else {
        return Ok(None);
    };
    let bytes: usize = block.iter().map(|l| l.len() + 1).sum();
    if bytes > MAX_BLOCK_BYTES {
        return Err(anyhow!(
            "the {FENCE} block is {} KB: at most {} KB",
            bytes.div_ceil(1024),
            MAX_BLOCK_BYTES / 1024
        ));
    }
    let mut changes = Vec::new();
    for (n, line) in block.iter().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let at = |rule: String| anyhow!("line {} of the {FENCE} block: {rule}", n + 1);
        let change: Change = serde_json::from_str(line.trim())
            .map_err(|e| at(format!("not a change as one JSON object ({e})")))?;
        check(&change).map_err(at)?;
        changes.push(change);
        if changes.len() > MAX_CHANGES {
            return Err(at(format!("at most {MAX_CHANGES} changes")));
        }
    }
    Ok(Some(changes))
}

/// Why `change` breaks a rule of Microsoft 365 changes, if it does: the rule, in words.
pub fn check(change: &Change) -> std::result::Result<(), String> {
    if !TOOLS.contains(&change.tool.as_str()) {
        return Err(format!(
            "tool is create_entity, update_entity, delete_entity or do_action, not {:?}",
            cut(&change.tool, 40)
        ));
    }
    check_path(&change.path)?;
    match (&change.body, change.tool == "delete_entity") {
        (Some(_), true) => return Err("delete_entity has no body".into()),
        (None, false) => return Err(format!("{} needs a body: a JSON object", change.tool)),
        (Some(body), false) if !body.is_object() => {
            return Err("body is a JSON object".into());
        }
        _ => {}
    }
    if let Some(tag) = &change.if_match {
        if !matches!(change.tool.as_str(), "update_entity" | "delete_entity") {
            return Err("ifMatch is for update_entity and delete_entity".into());
        }
        if tag.is_empty() || tag.len() > MAX_ETAG_BYTES || tag.chars().any(char::is_control) {
            return Err("ifMatch is the entity's ETag, as Microsoft Graph gave it".into());
        }
    }
    Ok(())
}

/// Why `path` isn't a Microsoft Graph path a change may use, if it isn't.
fn check_path(path: &str) -> std::result::Result<(), String> {
    if path.len() > MAX_PATH_BYTES {
        return Err(format!("path is at most {} KB", MAX_PATH_BYTES / 1024));
    }
    if !path.starts_with('/') || path.starts_with("//") || path.contains("://") {
        return Err(
            "path is a Microsoft Graph path that starts with /, with no host (like /me/sendMail)"
                .into(),
        );
    }
    if path.chars().any(char::is_control) {
        return Err("path has control characters".into());
    }
    let route = path.split(['?', '#']).next().unwrap_or_default();
    if route
        .split('/')
        .any(|s| s.to_ascii_lowercase().replace("%2e", ".") == "..")
    {
        return Err("path can't have ..".into());
    }
    Ok(())
}

/// The SHA-256 of `changes`, in hex, over their canonical JSON (keys sorted, no spaces):
/// what a sign-off shows and the apply step checks before it makes them.
pub fn digest(changes: &[Change]) -> String {
    let value = serde_json::to_value(changes).unwrap_or(Value::Null);
    let mut text = String::new();
    canonical(&value, &mut text);
    data_encoding::HEXLOWER.encode(&Sha256::digest(text.as_bytes()))
}

fn canonical(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, key) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(key.clone()).to_string());
                out.push(':');
                canonical(&map[key], out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                canonical(item, out);
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

/// What a sign-off shows of the changes step `step` proposed and step `by` makes: each one
/// as a person reads it, and the list's SHA-256. Redacted; the web UI shows it as text.
pub fn preview(step: &str, by: &str, changes: &[Change]) -> String {
    let mut text = format!("### Microsoft 365 changes step {step} proposes\n\n");
    if changes.is_empty() {
        text += &format!("Step {step} proposes no changes: step {by} makes none.\n");
        return crate::redact::redact(&text).0;
    }
    text += &format!(
        "If you approve, step {by} makes {} with your account, once and in order. \
         SHA-256 of the list: `{}`\n",
        if changes.len() == 1 {
            "this change".to_owned()
        } else {
            format!("these {} changes", changes.len())
        },
        digest(changes)
    );
    for (i, change) in changes.iter().enumerate() {
        let (heading, details) = shown_as(change);
        text += &format!("\n**{}. {heading}**\n\n{}", i + 1, fenced(&details));
    }
    crate::redact::redact(&text).0
}

/// What a sign-off shows of one change a digital worker's task makes: the change as a person
/// reads it, as [`preview`] shows each. Redacted; the web UI shows it as text.
pub fn preview_one(change: &Change) -> String {
    let (heading, details) = shown_as(change);
    crate::redact::redact(&format!("**{heading}**\n\n{}", fenced(&details))).0
}

/// How a sign-off shows `change`: its heading (Mail, or its verb) and its details.
fn shown_as(change: &Change) -> (&'static str, String) {
    match change.mail() {
        Some(message) => ("Mail", mail_lines(&change.path, message)),
        None => {
            let mut lines = format!("{} {}", change.tool, change.path);
            if let Some(body) = &change.body {
                lines += &format!("\n{}", cut(&compact(body), BODY_CHARS));
            }
            (change.verb(), lines)
        }
    }
}

/// `text` in a fenced block that its own backticks can't close.
pub(crate) fn fenced(text: &str) -> String {
    let fence = "`".repeat(longest_backticks(text).max(2) + 1);
    format!("{fence}text\n{text}\n{fence}\n")
}

/// The section a task's result ends with when it made Microsoft 365 changes: each change's
/// verb and path, and how it ended (never its body). Empty when it made none.
pub fn made_section(made: &[String]) -> String {
    if made.is_empty() {
        return String::new();
    }
    let mut text = "**Microsoft 365 changes this task made:**\n".to_owned();
    for line in made {
        text += &format!("- {}\n", crate::runs::one_line(line));
    }
    crate::redact::redact(&text).0
}

/// A mail's recipients, subject and first lines, as plain text.
fn mail_lines(path: &str, message: &Value) -> String {
    let field = |key: &str| message.get(key).and_then(Value::as_str).unwrap_or_default();
    let recipients = |key: &str| -> String {
        message
            .get(key)
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(|r| {
                        let address = r.pointer("/emailAddress/address")?.as_str()?;
                        Some(
                            match r.pointer("/emailAddress/name").and_then(Value::as_str) {
                                Some(name) if !name.is_empty() => format!("{name} <{address}>"),
                                _ => address.to_owned(),
                            },
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default()
    };
    let mut lines = format!("do_action {path}\nTo: {}", recipients("toRecipients"));
    for (key, name) in [("ccRecipients", "Cc"), ("bccRecipients", "Bcc")] {
        let list = recipients(key);
        if !list.is_empty() {
            lines += &format!("\n{name}: {list}");
        }
    }
    lines += &format!("\nSubject: {}", field("subject"));
    let content = message
        .pointer("/body/content")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let html = message
        .pointer("/body/contentType")
        .and_then(Value::as_str)
        .is_some_and(|t| t.eq_ignore_ascii_case("html"));
    let plain = if html {
        html_text(content)
    } else {
        content.to_owned()
    };
    let first: Vec<&str> = plain
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty())
        .take(MAIL_LINES)
        .collect();
    lines += &format!("\n\n{}", cut(&first.join("\n"), MAIL_CHARS));
    lines
}

/// `html` as plain text: tags removed (a line break for a break or the end of a block), and
/// the common entities written out.
fn html_text(html: &str) -> String {
    let mut text = String::new();
    let mut rest = html;
    while let Some(start) = rest.find('<') {
        text.push_str(&rest[..start]);
        let Some(end) = rest[start..].find('>') else {
            rest = "";
            break;
        };
        let tag = rest[start + 1..start + end]
            .trim_start_matches('/')
            .split(|c: char| c.is_whitespace() || c == '/')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        if matches!(
            tag.as_str(),
            "br" | "p" | "div" | "li" | "tr" | "h1" | "h2" | "h3"
        ) {
            text.push('\n');
        }
        rest = &rest[start + end + 1..];
    }
    text.push_str(rest);
    text.replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

fn longest_backticks(text: &str) -> usize {
    text.split(|c| c != '`').map(str::len).max().unwrap_or(0)
}

fn compact(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

/// `text` cut to `chars` characters, with … when it was longer.
fn cut(text: &str, chars: usize) -> String {
    if text.chars().count() <= chars {
        return text.to_owned();
    }
    let mut cut: String = text.chars().take(chars).collect();
    cut.push('…');
    cut
}

/// Whether `text`, a sign-off's request, shows the changes of step `step` with `digest`.
pub fn shown_in(text: &str, step: &str, digest: &str) -> bool {
    text.contains(&format!("### Microsoft 365 changes step {step} proposes"))
        && text.contains(&format!("SHA-256 of the list: `{digest}`"))
}

/// How a change ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Made (a 2xx), with a created entity's id and web link when Work IQ gave them.
    Done {
        status: Option<u16>,
        id: Option<String>,
        link: Option<String>,
    },
    /// Accepted (202): Microsoft 365 does it later.
    Accepted,
    /// It failed: what Work IQ said, and the fix.
    Failed(String),
    /// It isn't known whether it was made (it timed out, or failed after it was sent).
    Uncertain(String),
}

impl Outcome {
    /// Whether it was made (or accepted).
    pub fn made(&self) -> bool {
        matches!(self, Outcome::Done { .. } | Outcome::Accepted)
    }

    /// The outcome of a change call that Work IQ answered without an error: its status, and a
    /// created entity's id and web link (never its content).
    pub fn of(result: &ToolResult) -> Outcome {
        let found = result.structured_content.clone().or_else(|| {
            result
                .content
                .iter()
                .filter_map(|c| c.get("text")?.as_str())
                .find_map(|t| serde_json::from_str::<Value>(t).ok())
        });
        let found = found.unwrap_or(Value::Null);
        let status = ["status", "statusCode", "httpStatusCode"]
            .iter()
            .find_map(|k| found.get(*k).and_then(Value::as_u64))
            .and_then(|s| u16::try_from(s).ok());
        if status == Some(202) {
            return Outcome::Accepted;
        }
        let text = |key: &str| {
            found
                .get(key)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty() && !s.chars().any(char::is_control))
                .map(|s| cut(s, 300))
        };
        Outcome::Done {
            status,
            id: text("id"),
            link: text("webLink").or_else(|| text("webUrl")),
        }
    }

    /// The outcome of a change call that failed: uncertain if it may have been made.
    pub fn of_error(error: &anyhow::Error) -> Outcome {
        let text = crate::runs::one_line(&format!("{error:#}"));
        match error.downcast_ref::<WorkIqError>() {
            Some(e) if e.class == Class::UncertainChange => Outcome::Uncertain(text),
            _ => Outcome::Failed(text),
        }
    }

    /// How it ended, in words: done (with a created entity's id and link), accepted, failed
    /// or outcome unknown.
    pub fn describe(&self) -> String {
        match self {
            Outcome::Done { status, id, link } => {
                let mut text = match status {
                    Some(status) => format!("done ({status})"),
                    None => "done".to_owned(),
                };
                if let Some(id) = id {
                    text += &format!("; id {id}");
                }
                if let Some(link) = link {
                    text += &format!("; {link}");
                }
                text
            }
            Outcome::Accepted => "accepted (202): Microsoft 365 finishes it later".to_owned(),
            Outcome::Failed(why) => format!("failed: {why}"),
            Outcome::Uncertain(why) => format!(
                "outcome unknown: {why}. Check in Microsoft 365 whether it was made; it isn't sent again"
            ),
        }
    }
}

/// The apply step's result: how each change step `step` proposed (signed off as `digest`)
/// ended, in order, then those not made after the first that failed or is uncertain.
pub fn report(step: &str, digest: &str, changes: &[Change], outcomes: &[Outcome]) -> String {
    if changes.is_empty() {
        return format!("Step {step} proposed no Microsoft 365 changes: none was made.");
    }
    let mut text =
        format!("Microsoft 365 changes step {step} proposed, signed off (SHA-256 {digest}):\n");
    for (i, (change, outcome)) in changes.iter().zip(outcomes).enumerate() {
        text += &format!("{}. {}: {}\n", i + 1, change.label(), outcome.describe());
    }
    if outcomes.len() < changes.len() {
        let rest: Vec<String> = (outcomes.len() + 1..=changes.len())
            .map(|n| n.to_string())
            .collect();
        text += &format!(
            "Not made, after change {}: {}.\n",
            outcomes.len(),
            rest.join(", ")
        );
    }
    crate::redact::redact(&text).0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(lines: &[&str]) -> String {
        format!(
            "Here are the replies.\n\n```{FENCE}\n{}\n```\nDone.",
            lines.join("\n")
        )
    }

    const MAIL: &str = r#"{"tool":"do_action","path":"/me/sendMail","body":{"message":{"subject":"Re: the outage","body":{"contentType":"HTML","content":"<p>Hi Ana,</p><p>It is fixed &amp; deployed.</p><p>Line 3</p><p>4</p><p>5</p><p>6</p><p>7</p><p>8</p><p>9</p><p>10</p><p>11 is cut</p>"},"toRecipients":[{"emailAddress":{"name":"Ana","address":"ana@contoso.com"}}],"ccRecipients":[{"emailAddress":{"address":"ops@contoso.com"}}]}}}"#;
    const UPDATE: &str = r#"{"tool":"update_entity","path":"/me/events/AAMk1","body":{"subject":"Moved"},"ifMatch":"W/\"abc\""}"#;

    #[test]
    fn a_block_of_changes_is_read_with_each_tools_arguments() {
        assert_eq!(proposed("No changes needed.").unwrap(), None);
        let changes = proposed(&block(&[MAIL, "", UPDATE])).unwrap().unwrap();
        assert_eq!(changes.len(), 2);
        assert_eq!(
            changes[0].arguments(),
            json!({ "actionUrl": "/me/sendMail", "jsonBody": changes[0].body.clone().unwrap() })
        );
        assert_eq!(
            changes[1].arguments(),
            json!({ "entityUrl": "/me/events/AAMk1", "jsonBody": { "subject": "Moved" },
                    "headers": { "If-Match": "W/\"abc\"" } })
        );
        let create: Change = serde_json::from_str(
            r#"{"tool":"create_entity","path":"/me/events","body":{"subject":"Sync"}}"#,
        )
        .unwrap();
        assert_eq!(
            create.arguments(),
            json!({ "parentUrl": "/me/events", "jsonBody": { "subject": "Sync" } })
        );
        let delete: Change =
            serde_json::from_str(r#"{"tool":"delete_entity","path":"/me/messages/AAMk2"}"#)
                .unwrap();
        assert_eq!(
            delete.arguments(),
            json!({ "entityUrl": "/me/messages/AAMk2" })
        );
        assert_eq!(proposed(&block(&[])).unwrap(), Some(vec![]));
    }

    #[test]
    fn a_block_that_breaks_a_rule_names_the_line_and_the_rule() {
        let fails = |lines: &[&str], rule: &str| {
            let err = format!("{:#}", proposed(&block(lines)).unwrap_err());
            assert!(err.contains(rule), "{lines:?}: {err}");
        };
        fails(
            &["not json"],
            "line 1 of the m365-changes block: not a change",
        );
        fails(
            &[
                MAIL,
                r#"{"tool":"send_mail","path":"/me/sendMail","body":{}}"#,
            ],
            "line 2 of the m365-changes block: tool is create_entity",
        );
        for path in [
            "https://graph.microsoft.com/v1.0/me/sendMail",
            "//graph.microsoft.com/me",
            "me/sendMail",
            "/me/../users/x/sendMail",
            "/me/%2E%2e/users",
            "/me/send\u{7}Mail",
        ] {
            let line = json!({ "tool": "do_action", "path": path, "body": {} }).to_string();
            fails(&[&line], "path");
        }
        let long = format!("/me/{}", "a".repeat(MAX_PATH_BYTES));
        fails(
            &[&json!({ "tool": "do_action", "path": long, "body": {} }).to_string()],
            "path is at most 2 KB",
        );
        fails(
            &[r#"{"tool":"delete_entity","path":"/me/messages/1","body":{}}"#],
            "delete_entity has no body",
        );
        fails(
            &[r#"{"tool":"create_entity","path":"/me/events"}"#],
            "create_entity needs a body",
        );
        fails(
            &[r#"{"tool":"do_action","path":"/me/sendMail","body":[1]}"#],
            "body is a JSON object",
        );
        fails(
            &[r#"{"tool":"create_entity","path":"/me/events","body":{},"ifMatch":"x"}"#],
            "ifMatch is for update_entity and delete_entity",
        );
        fails(
            &[r#"{"tool":"do_action","path":"/me/sendMail","body":{},"to":"x"}"#],
            "unknown field",
        );
        let many: Vec<String> = (0..=MAX_CHANGES)
            .map(|i| format!(r#"{{"tool":"delete_entity","path":"/me/messages/{i}"}}"#))
            .collect();
        let many: Vec<&str> = many.iter().map(String::as_str).collect();
        fails(
            &many,
            "line 21 of the m365-changes block: at most 20 changes",
        );
        let big = format!(
            r#"{{"tool":"create_entity","path":"/me/events","body":{{"x":"{}"}}}}"#,
            "a".repeat(MAX_BLOCK_BYTES)
        );
        fails(&[&big], "at most 256 KB");
        let two = format!("{}\n{}", block(&[MAIL]), block(&[UPDATE]));
        assert!(
            format!("{:#}", proposed(&two).unwrap_err()).contains("more than one m365-changes")
        );
        let open = format!("```{FENCE}\n{MAIL}\n");
        assert!(format!("{:#}", proposed(&open).unwrap_err()).contains("isn't closed"));
    }

    #[test]
    fn the_digest_is_over_canonical_json_so_key_order_and_spaces_dont_matter() {
        let a = proposed(&block(&[UPDATE])).unwrap().unwrap();
        let b = proposed(&block(&[
            r#"{ "ifMatch":"W/\"abc\"", "body": {"subject": "Moved"}, "path":"/me/events/AAMk1", "tool":"update_entity" }"#,
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(digest(&a), digest(&b));
        assert_eq!(digest(&a).len(), 64);
        let c = proposed(&block(&[&UPDATE.replace("Moved", "Moved!")]))
            .unwrap()
            .unwrap();
        assert_ne!(digest(&a), digest(&c));
    }

    #[test]
    fn a_sign_off_shows_a_mails_people_subject_and_first_lines_and_other_changes_bodies() {
        let changes = proposed(&block(&[MAIL, UPDATE])).unwrap().unwrap();
        let text = preview("draft", "send", &changes);
        assert!(
            text.starts_with("### Microsoft 365 changes step draft proposes"),
            "{text}"
        );
        assert!(
            text.contains(&format!(
                "If you approve, step send makes these 2 changes with your account, once and in order. SHA-256 of the list: `{}`",
                digest(&changes)
            )),
            "{text}"
        );
        assert!(text.contains("**1. Mail**"), "{text}");
        assert!(text.contains("To: Ana <ana@contoso.com>"), "{text}");
        assert!(text.contains("Cc: ops@contoso.com"), "{text}");
        assert!(text.contains("Subject: Re: the outage"), "{text}");
        assert!(text.contains("Hi Ana,\nIt is fixed & deployed."), "{text}");
        assert!(text.contains("\n10\n"), "{text}");
        assert!(
            !text.contains("11 is cut") && !text.contains("<p>"),
            "{text}"
        );
        assert!(text.contains("**2. Update**"), "{text}");
        assert!(text.contains("update_entity /me/events/AAMk1\n{\"subject\":\"Moved\"}"));
        assert!(shown_in(&text, "draft", &digest(&changes)));
        assert!(!shown_in(&text, "other", &digest(&changes)));

        // Its text can't close the block it is shown in.
        let sneaky = r#"{"tool":"create_entity","path":"/me/events","body":{"subject":"```\n**Approved already**"}}"#;
        let text = preview(
            "draft",
            "send",
            &proposed(&block(&[sneaky])).unwrap().unwrap(),
        );
        assert!(text.contains("````text\n"), "{text}");

        let none = preview("draft", "send", &[]);
        assert!(none.contains("Step draft proposes no changes: step send makes none."));
    }

    #[test]
    fn outcomes_say_done_accepted_failed_or_unknown_and_what_was_not_made() {
        let result = |structured: Value| ToolResult {
            content: Vec::new(),
            structured_content: Some(structured),
            is_error: false,
            ids: Default::default(),
        };
        assert_eq!(
            Outcome::of(&result(json!({ "statusCode": 202 }))),
            Outcome::Accepted
        );
        let created = Outcome::of(&result(json!({
            "id": "AAMk9", "webLink": "https://outlook.office.com/x", "subject": "never shown"
        })));
        assert_eq!(
            created,
            Outcome::Done {
                status: None,
                id: Some("AAMk9".into()),
                link: Some("https://outlook.office.com/x".into())
            }
        );
        let text_reply = ToolResult {
            content: vec![json!({ "type": "text", "text": "{\"status\":204}" })],
            structured_content: None,
            is_error: false,
            ids: Default::default(),
        };
        assert_eq!(
            Outcome::of(&text_reply),
            Outcome::Done {
                status: Some(204),
                id: None,
                link: None
            }
        );
        let uncertain = anyhow::Error::new(WorkIqError::new(
            crate::workiq::WorkIqErrorKind::Transport,
            Class::UncertainChange,
            "the change timed out",
            Default::default(),
        ));
        assert!(matches!(
            Outcome::of_error(&uncertain),
            Outcome::Uncertain(_)
        ));
        assert!(matches!(
            Outcome::of_error(&anyhow!("denied")),
            Outcome::Failed(_)
        ));

        let changes = proposed(&block(&[MAIL, UPDATE, UPDATE])).unwrap().unwrap();
        let text = report(
            "draft",
            "abc",
            &changes,
            &[Outcome::Accepted, Outcome::Uncertain("timed out".into())],
        );
        assert!(
            text.starts_with(
                "Microsoft 365 changes step draft proposed, signed off (SHA-256 abc):\n"
            )
        );
        assert!(
            text.contains("1. Action /me/sendMail: accepted (202)"),
            "{text}"
        );
        assert!(
            text.contains(
                "2. Update /me/events/AAMk1: outcome unknown: timed out. Check in Microsoft 365"
            ),
            "{text}"
        );
        assert!(text.contains("Not made, after change 2: 3."), "{text}");
        assert!(!text.contains("never shown"));
        assert_eq!(
            report("draft", "abc", &[], &[]),
            "Step draft proposed no Microsoft 365 changes: none was made."
        );
    }

    #[test]
    fn the_instruction_shows_a_block_that_parses() {
        let text = instruction();
        let example = text
            .split("```m365-changes\n")
            .nth(1)
            .and_then(|rest| rest.split("\n```").next())
            .unwrap();
        let changes = proposed(&format!("```{FENCE}\n{example}\n```"))
            .unwrap()
            .unwrap();
        assert_eq!(changes.len(), 2);
    }

    #[test]
    fn a_change_call_reads_back_as_the_change_whose_arguments_it_has() {
        let create: Change = serde_json::from_str(
            r#"{"tool":"create_entity","path":"/me/events","body":{"subject":"Sync"}}"#,
        )
        .unwrap();
        let delete: Change = serde_json::from_str(
            r#"{"tool":"delete_entity","path":"/me/messages/AAMk2","ifMatch":"W/\"x\""}"#,
        )
        .unwrap();
        let mut changes = proposed(&block(&[MAIL, UPDATE])).unwrap().unwrap();
        changes.extend([create, delete]);
        for change in changes {
            let read = Change::from_arguments(&change.tool, &change.arguments()).unwrap();
            assert_eq!(read, change);
            assert_eq!(check(&read), Ok(()));
        }

        // As Work IQ's schemas also allow: the body encoded as a string, any case of If-Match.
        let read = Change::from_arguments(
            "update_entity",
            &json!({ "entityUrl": "/me/events/AAMk1", "jsonBody": "{\"subject\":\"Moved\"}",
                     "headers": { "if-match": "W/\"abc\"" } }),
        )
        .unwrap();
        assert_eq!(read, serde_json::from_str::<Change>(UPDATE).unwrap());

        let fails = |tool: &str, arguments: Value, says: &str| {
            let err = Change::from_arguments(tool, &arguments).unwrap_err();
            assert!(err.contains(says), "{arguments}: {err}");
        };
        fails("send_mail", json!({}), "tool is create_entity");
        fails("do_action", json!([1]), "arguments are a JSON object");
        fails(
            "do_action",
            json!({ "actionUrl": "/me/sendMail", "jsonBody": {}, "to": "x" }),
            "do_action takes actionUrl, jsonBody and headers, not to",
        );
        fails(
            "create_entity",
            json!({ "entityUrl": "/me/events", "jsonBody": {} }),
            "create_entity takes parentUrl",
        );
        fails("do_action", json!({ "actionUrl": 7 }), "actionUrl is the");
        fails(
            "create_entity",
            json!({ "parentUrl": "/me/events", "jsonBody": "{not json" }),
            "jsonBody is a JSON object",
        );
        fails(
            "update_entity",
            json!({ "entityUrl": "/me/events/1", "jsonBody": {}, "headers": { "Prefer": "x" } }),
            "headers has only one If-Match",
        );
        fails(
            "update_entity",
            json!({ "entityUrl": "/me/events/1", "jsonBody": {}, "headers": ["If-Match"] }),
            "headers is a JSON object",
        );

        // What reads well is still held to the rules.
        let graph = "https://graph.microsoft.com/v1.0/me/sendMail";
        let hosted =
            Change::from_arguments("do_action", &json!({ "actionUrl": graph, "jsonBody": {} }))
                .unwrap();
        assert!(check(&hosted).unwrap_err().contains("with no host"));
        let deleted = Change::from_arguments(
            "delete_entity",
            &json!({ "entityUrl": "/me/messages/1", "jsonBody": {} }),
        )
        .unwrap();
        assert_eq!(check(&deleted), Err("delete_entity has no body".into()));
    }

    #[test]
    fn a_sign_off_of_one_change_shows_it_as_a_plans_sign_off_shows_each() {
        let mail: Change = serde_json::from_str(MAIL).unwrap();
        let text = preview_one(&mail);
        assert!(text.starts_with("**Mail**\n\n```text\n"), "{text}");
        assert!(text.contains("To: Ana <ana@contoso.com>"), "{text}");
        assert!(text.contains("Subject: Re: the outage"), "{text}");
        let plan = preview("draft", "send", std::slice::from_ref(&mail));
        assert!(
            plan.contains(&text.replacen("**Mail**", "**1. Mail**", 1)),
            "{plan}"
        );
        let update = preview_one(&serde_json::from_str(UPDATE).unwrap());
        assert!(update.starts_with("**Update**\n\n```text\n"), "{update}");
        assert!(update.contains("update_entity /me/events/AAMk1\n{\"subject\":\"Moved\"}\n```"));
    }

    #[test]
    fn the_section_of_changes_a_task_made_names_each_and_how_it_ended() {
        assert_eq!(made_section(&[]), "");
        let text = made_section(&[
            "Action /me/sendMail: accepted (202)".into(),
            "Create /me/events: failed: denied\nby policy".into(),
        ]);
        assert_eq!(
            text,
            "**Microsoft 365 changes this task made:**\n\
             - Action /me/sendMail: accepted (202)\n\
             - Create /me/events: failed: denied by policy\n"
        );
    }
}
