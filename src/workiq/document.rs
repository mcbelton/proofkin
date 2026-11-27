//! A spec in a Word, Markdown or text document on SharePoint or OneDrive, read through Work
//! IQ for `proofkin spec import --from` (docs/work-iq.md, section 5). The document's
//! metadata comes first (`fetch`: its name, type, size and sensitivity label); then, if it
//! is a type that is read, isn't too big and isn't encrypted by its label's protection, its
//! bytes (`fetch_blob`), as Markdown. Both calls go through the audited client as
//! `spec-import`, held to the profile's budgets. What the document holds is external
//! content: its text is redacted here, and it leaves only as a knowledge topic marked with
//! its label.

use anyhow::{Context, Result, anyhow, bail};
use data_encoding::{BASE64, BASE64_NOPAD, BASE64URL_NOPAD};
use serde_json::Value;

use super::audit::{CallContext, Purpose};
use super::client::{RequestIds, ToolResult, Who, WorkIqError, reported_text};
use super::context::WorkContext;

/// The largest document read: what `fetch_blob` gives at most.
pub const MAX_DOCUMENT: u64 = 4 * 1024 * 1024;

/// The fields of the document's driveItem that are read. Graph gives `sensitivityLabel`
/// only when it is selected.
const SELECT: &str = "id,name,size,file,webUrl,lastModifiedDateTime,sensitivityLabel";

const WORD: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";

/// SharePoint's hosts, by their suffix: `*.sharepoint.com`, and `*.sharepoint-df.com`,
/// Microsoft's own pre-production SharePoint (`retrieve` gives its documents' links too).
const SHAREPOINT_DOMAINS: [&str; 2] = [".sharepoint.com", ".sharepoint-df.com"];

/// OneDrive's hosts.
const ONEDRIVE_HOSTS: [&str; 2] = ["onedrive.live.com", "1drv.ms"];

const TYPES_READ: &str =
    "only Word (.docx), Markdown (.md, .markdown) and text (.txt) documents are read";

/// The types of document that are read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Type {
    Word,
    Markdown,
    Text,
}

impl Type {
    /// The type with MIME type `mime` (its parameters aside).
    fn of_mime(mime: &str) -> Option<Self> {
        let mime = mime.split(';').next().unwrap_or_default().trim();
        match mime.to_ascii_lowercase().as_str() {
            WORD => Some(Type::Word),
            "text/markdown" | "text/x-markdown" => Some(Type::Markdown),
            "text/plain" => Some(Type::Text),
            _ => None,
        }
    }

    /// The type of a file named `name`, by its extension.
    fn of_name(name: &str) -> Option<Self> {
        let (_, extension) = name.rsplit_once('.')?;
        match extension.to_ascii_lowercase().as_str() {
            "docx" => Some(Type::Word),
            "md" | "markdown" => Some(Type::Markdown),
            "txt" => Some(Type::Text),
            _ => None,
        }
    }

    /// The type of the file `name` whose MIME type is `mime`: by the MIME type, or by the
    /// name's extension when the MIME type says nothing (none, or any bytes).
    fn of_file(mime: &str, name: &str) -> Option<Self> {
        let mime = mime.trim();
        let generic = mime.is_empty() || mime.eq_ignore_ascii_case("application/octet-stream");
        Type::of_mime(mime).or_else(|| Type::of_name(name).filter(|_| generic))
    }
}

/// A document read for a spec import.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Document {
    /// Its file name (`Photo albums.docx`), on one line.
    pub name: String,
    /// The link it was read from.
    pub url: String,
    /// Its sensitivity label's name; `None` when Work IQ gave no label information.
    pub label: Option<String>,
    /// Its text as Markdown, with secrets redacted.
    pub markdown: String,
}

/// Whether the spec `spec` is a link (`https://...`) rather than a folder.
pub fn is_link(spec: &str) -> bool {
    let Some((scheme, _)) = spec.trim().split_once("://") else {
        return false;
    };
    let scheme_char = |c: char| c.is_ascii_alphanumeric() || "+-.".contains(c);
    // More than a letter: `C://specs` is a folder.
    scheme.len() > 1
        && scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        && scheme.chars().all(scheme_char)
}

/// The Work IQ path of the document at `url`, as Microsoft Graph addresses a shared item:
/// `/shares/u!` and the link in base64url without padding. Only an `https` link on
/// SharePoint (`*.sharepoint.com`, `*.sharepoint-df.com`) or OneDrive (`onedrive.live.com`,
/// `1drv.ms`) is taken.
pub fn share_path(url: &str) -> Result<String> {
    let url = url.trim();
    let refuse = || {
        anyhow!(
            "{url} is not a SharePoint or OneDrive link: a spec is read from the https:// link \
             of a document on *.sharepoint.com, *.sharepoint-df.com, onedrive.live.com or \
             1drv.ms"
        )
    };
    // URL parsers disagree on these, so the link could name another host elsewhere.
    if url.contains(|c: char| c == '\\' || c.is_whitespace() || c.is_control()) {
        return Err(refuse());
    }
    let parsed = reqwest::Url::parse(url).map_err(|_| refuse())?;
    let host = parsed.host_str().unwrap_or_default().to_ascii_lowercase();
    let sharepoint = SHAREPOINT_DOMAINS.iter().any(|d| host.ends_with(d));
    let known = sharepoint || ONEDRIVE_HOSTS.contains(&host.as_str());
    let credentials = !parsed.username().is_empty() || parsed.password().is_some();
    if parsed.scheme() != "https" || !known || credentials {
        return Err(refuse());
    }
    let id = BASE64URL_NOPAD.encode(url.as_bytes());
    Ok(format!("/shares/u!{id}"))
}

/// Reads the document at the SharePoint or OneDrive link `url` through `work`: its metadata
/// first; then, if it is a Word, Markdown or text document of at most [`MAX_DOCUMENT`] that
/// its sensitivity label doesn't protect, its content as Markdown. A link that isn't one is
/// refused before any call.
pub async fn read(work: &dyn WorkContext, url: &str) -> Result<Document> {
    let url = url.trim();
    let share = share_path(url)?;
    let context = CallContext::local(Purpose::SpecImport);
    let item = format!("{share}/driveItem?$select={SELECT}");
    let result = work
        .fetch(&context, &[item.as_str()])
        .await
        .map_err(|e| tool_failed("fetch", e))?;
    let item = fetched(&result)?;
    let name = file_name(item);
    let mime = item
        .pointer("/file/mimeType")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if Type::of_file(mime, &name).is_none() {
        let what = if mime.trim().is_empty() {
            String::new()
        } else {
            format!(" ({})", one_line(mime))
        };
        bail!(
            "{name} is not a Word, Markdown or text document{what}: {TYPES_READ}. Save it as \
             Word or Markdown, then import that"
        );
    }
    if let Some(size) = number(item.get("size"))
        && size > MAX_DOCUMENT
    {
        return Err(too_big(&name, Some(size)));
    }
    let label = item
        .pointer("/sensitivityLabel/displayName")
        .and_then(Value::as_str)
        .map(one_line)
        .filter(|label| !label.is_empty());
    // The label's protection encrypts the content, so there would be no text to read.
    if item.pointer("/sensitivityLabel/protectionEnabled") == Some(&Value::Bool(true)) {
        let label = label.map(|label| format!(" {label}")).unwrap_or_default();
        bail!(
            "{name} is protected by its sensitivity label{label}: its content is encrypted, so \
             it can't be read. Save a copy without protection (or as Markdown) and import that"
        );
    }

    let path = format!("{share}/driveItem/content");
    let blob = work
        .fetch_blob(&context, &path)
        .await
        .map_err(|e| tool_failed("fetch_blob", e))?;
    let (kind, bytes) = blob_of(&blob, &name)?;
    let markdown = if kind == Type::Word {
        crate::docx::to_markdown(&bytes).with_context(|| format!("reading {name}"))?
    } else {
        text_of(bytes, &name)?
    };
    let (markdown, _) = crate::redact::redact(&markdown);
    if markdown.trim().is_empty() {
        bail!("{name} has no text to import");
    }
    Ok(Document {
        name,
        url: url.to_owned(),
        label,
        markdown,
    })
}

/// The driveItem `fetch` gave (`structuredContent.results[0].data`), if its `statusCode`
/// is 200.
fn fetched(result: &ToolResult) -> Result<&Value> {
    if result.is_error {
        return Err(reported("fetch", &reported_text(&result.content)));
    }
    let entity = result
        .structured_content
        .as_ref()
        .and_then(|content| content.get("results"))
        .and_then(Value::as_array)
        .and_then(|results| results.first())
        .context("Work IQ's fetch tool gave no result for the document")?;
    check_status("fetch", entity, &result.ids)?;
    entity
        .get("data")
        .filter(|data| data.is_object())
        .context("Work IQ's fetch tool gave no data for the document")
}

/// The content `fetch_blob` gave, base64-decoded, and its type. It must have a status of
/// 200, a type that is read (by its `contentType`, as for the metadata), and as many bytes
/// as its `sizeBytes`.
fn blob_of(result: &ToolResult, name: &str) -> Result<(Type, Vec<u8>)> {
    if result.is_error {
        return Err(reported("fetch_blob", &reported_text(&result.content)));
    }
    let blob = result
        .structured_content
        .as_ref()
        .filter(|blob| blob.is_object())
        .context("Work IQ's fetch_blob tool gave no content for the document")?;
    check_status("fetch_blob", blob, &result.ids)?;
    let content_type = blob
        .get("contentType")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let Some(kind) = Type::of_file(content_type, name) else {
        bail!(
            "Work IQ gave {name} as {:?}, not as a Word, Markdown or text document: \
             {TYPES_READ}",
            one_line(content_type)
        );
    };
    let encoded: String = blob
        .get("base64Content")
        .and_then(Value::as_str)
        .context("Work IQ's fetch_blob tool gave no base64Content for the document")?
        .chars()
        .filter(|c| !c.is_ascii_whitespace())
        .collect();
    // Base64 takes 4 characters for 3 bytes.
    if encoded.len() as u64 > MAX_DOCUMENT.div_ceil(3) * 4 {
        return Err(too_big(name, None));
    }
    let bytes = BASE64
        .decode(encoded.as_bytes())
        .or_else(|_| BASE64_NOPAD.decode(encoded.as_bytes()))
        .context("Work IQ's fetch_blob tool gave content that is not base64")?;
    let size = number(blob.get("sizeBytes"))
        .context("Work IQ's fetch_blob tool gave no sizeBytes for the document")?;
    if bytes.len() as u64 != size {
        bail!(
            "Work IQ gave {} bytes of {name}, but its sizeBytes says {size}: the content is \
             incomplete",
            bytes.len()
        );
    }
    if size > MAX_DOCUMENT {
        return Err(too_big(name, Some(size)));
    }
    Ok((kind, bytes))
}

/// `error`, a failed call of Work IQ's `tool` for the document. A failure the tool reported
/// itself that only support can help with says what the tool said; any other (one you or an
/// admin can fix: signing in again, a policy, billing, waiting) keeps the client's error.
fn tool_failed(tool: &str, error: anyhow::Error) -> anyhow::Error {
    let said = error
        .downcast_ref::<WorkIqError>()
        .filter(|failure| failure.fix.who == Who::Support)
        .and_then(|failure| failure.tool_text.as_deref());
    match said {
        Some(said) => reported(tool, said),
        None => error,
    }
}

/// The error for a failure Work IQ's `tool` reported for the document, with what it said
/// (`text`, on one line and redacted) when it said anything.
fn reported(tool: &str, text: &str) -> anyhow::Error {
    let mut message = format!("Work IQ's {tool} tool reported an error for the document");
    if !text.is_empty() {
        message.push_str(&format!(": {text}"));
    }
    anyhow!(message)
}

/// Fails unless `value`'s `statusCode` is 200, with the class of Work IQ's errors for that
/// status.
fn check_status(tool: &str, value: &Value, ids: &RequestIds) -> Result<()> {
    match number(value.get("statusCode")) {
        Some(200) => Ok(()),
        Some(status) => {
            let status = u16::try_from(status).unwrap_or(u16::MAX);
            Err(WorkIqError::entity(tool, status, &failure(value), ids.clone()).into())
        }
        None => bail!("Work IQ's {tool} tool gave no statusCode for the document"),
    }
}

/// What a failed entity says about why: its error's code and message, redacted.
pub(crate) fn failure(value: &Value) -> String {
    let error = value
        .pointer("/data/error")
        .or_else(|| value.get("error"))
        .unwrap_or(&Value::Null);
    let parts: Vec<&str> = match error {
        Value::String(text) => vec![text.as_str()],
        _ => ["code", "message"]
            .iter()
            .filter_map(|key| error.get(key).and_then(Value::as_str))
            .collect(),
    };
    crate::redact::redact(&parts.join(": ")).0
}

/// A number given as a JSON number or a string.
pub(crate) fn number(value: Option<&Value>) -> Option<u64> {
    match value? {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// The text of a Markdown or text document, without a byte order mark and with `\n` line
/// ends.
fn text_of(bytes: Vec<u8>, name: &str) -> Result<String> {
    let text = String::from_utf8(bytes)
        .map_err(|_| anyhow!("{name} is not UTF-8 text: save it as UTF-8, or as Word"))?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    Ok(text.replace("\r\n", "\n"))
}

/// The driveItem's file name, on one line.
fn file_name(item: &Value) -> String {
    let name = item["name"].as_str().unwrap_or_default();
    let name: String = one_line(name).chars().take(120).collect();
    if name.is_empty() {
        "document".to_owned()
    } else {
        name
    }
}

fn too_big(name: &str, size: Option<u64>) -> anyhow::Error {
    let size = size
        .map(|bytes| format!(" ({:.1} MB)", bytes as f64 / (1024.0 * 1024.0)))
        .unwrap_or_default();
    anyhow!(
        "{name} is too big to read{size}: a document may have at most {} MB",
        MAX_DOCUMENT / (1024 * 1024)
    )
}

/// `text` on one line, without control characters.
fn one_line(text: &str) -> String {
    let text: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use axum::extract::State;
    use axum::http::StatusCode;
    use axum::routing::post;
    use serde_json::json;

    use super::*;
    use crate::docx::tests::{cell, docx, item, para};
    use crate::playbooks::{Rendered, SPEC_IMPORT};
    use crate::workiq::audit::{Audit, FILE};
    use crate::workiq::client::{Class, Client, FixedToken};
    use crate::workiq::context::ClientWork;

    const LINK: &str =
        "https://contoso.sharepoint.com/sites/Team/Shared%20Documents/Photo%20albums.docx";

    /// A fake Work IQ server that offers `fetch` and `fetch_blob`: `fetch` gives `item` (an
    /// entity's `{statusCode, data}`) and `fetch_blob` gives `blob`, except for the tool
    /// [`FakeWorkIq::fail`] names. It counts the requests it gets and keeps the tool calls.
    struct FakeWorkIq {
        item: Value,
        blob: Value,
        /// The tool that answers `isError`, and the `content` it answers with.
        failing: Mutex<Option<(&'static str, Value)>>,
        requests: AtomicUsize,
        calls: Mutex<Vec<(String, Value)>>,
    }

    impl FakeWorkIq {
        /// The tools called, in order.
        fn tools(&self) -> Vec<String> {
            let calls = self.calls.lock().unwrap();
            calls.iter().map(|(tool, _)| tool.clone()).collect()
        }

        /// Makes `tool`, from now on, report an error (`isError`) with `content`; the other
        /// tool answers as before.
        fn fail(&self, tool: &'static str, content: Value) {
            *self.failing.lock().unwrap() = Some((tool, content));
        }
    }

    async fn mcp(
        State(fake): State<Arc<FakeWorkIq>>,
        body: String,
    ) -> (StatusCode, [(&'static str, &'static str); 1], String) {
        fake.requests.fetch_add(1, Ordering::SeqCst);
        let json = [("content-type", "application/json")];
        let message: Value = serde_json::from_str(&body).unwrap();
        let result = match message["method"].as_str().unwrap() {
            "notifications/initialized" => return (StatusCode::ACCEPTED, json, String::new()),
            "initialize" => json!({
                "protocolVersion": crate::workiq::client::PROTOCOL_VERSION,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "fake-workiq", "version": "0" },
            }),
            "tools/list" => {
                let tools = ["fetch", "fetch_blob"]
                    .map(|name| json!({ "name": name, "inputSchema": { "type": "object" } }));
                json!({ "tools": tools })
            }
            "tools/call" => {
                let tool = message["params"]["name"].as_str().unwrap().to_owned();
                let failing = fake.failing.lock().unwrap().clone();
                let result = match failing.filter(|(name, _)| *name == tool) {
                    Some((_, content)) => json!({ "content": content, "isError": true }),
                    None => {
                        let structured = match tool.as_str() {
                            "fetch" => json!({ "results": [fake.item] }),
                            _ => fake.blob.clone(),
                        };
                        json!({ "content": [], "structuredContent": structured, "isError": false })
                    }
                };
                let arguments = message["params"]["arguments"].clone();
                fake.calls.lock().unwrap().push((tool, arguments));
                result
            }
            other => panic!("unexpected method {other}"),
        };
        let reply = json!({ "jsonrpc": "2.0", "id": message["id"], "result": result });
        (StatusCode::OK, json, reply.to_string())
    }

    /// Work IQ through the audited client, at a fake server whose `fetch` gives `item` and
    /// whose `fetch_blob` gives `blob`. The calls are audited in `dir`.
    async fn fake_work(dir: &Path, item: Value, blob: Value) -> (Arc<FakeWorkIq>, ClientWork) {
        let fake = Arc::new(FakeWorkIq {
            item,
            blob,
            failing: Mutex::default(),
            requests: AtomicUsize::new(0),
            calls: Mutex::default(),
        });
        let app = axum::Router::new()
            .route("/mcp", post(mcp))
            .with_state(fake.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let audit = Audit::new(dir.join(FILE), "boss", Default::default());
        let client = Client::new(&url, Arc::new(FixedToken("token".into())))
            .unwrap()
            .with_audit(Arc::new(audit));
        (fake, ClientWork::new(client))
    }

    /// What `fetch` gives for the document `name`: its driveItem, with the sensitivity label
    /// `label` if it has one.
    fn drive_item(name: &str, mime: &str, size: usize, label: Option<&str>) -> Value {
        let mut data = json!({
            "id": "01ABCDEF", "name": name, "size": size, "file": { "mimeType": mime },
            "webUrl": LINK, "lastModifiedDateTime": "2025-05-01T09:00:00Z",
        });
        if let Some(label) = label {
            data["sensitivityLabel"] = json!({
                "displayName": label, "id": "4f6a0c2e", "labelHash": "a1b2c3",
                "protectionEnabled": false,
            });
        }
        json!({ "statusCode": 200, "data": data })
    }

    /// What `fetch_blob` gives for the document `name`: `bytes` of type `mime`.
    fn file_content(name: &str, mime: &str, bytes: &[u8]) -> Value {
        json!({
            "statusCode": 200, "contentType": mime, "blobName": name,
            "sizeBytes": bytes.len(), "base64Content": BASE64.encode(bytes),
        })
    }

    /// The spec playbook's values for the spec `spec`, for peer alice.
    fn spec_vars(spec: &str) -> BTreeMap<String, String> {
        BTreeMap::from([
            ("spec".to_owned(), spec.to_owned()),
            ("peer".to_owned(), "alice".to_owned()),
        ])
    }

    /// The spec playbook filled in from the document at `link`, read through `work`.
    async fn import(work: &ClientWork, link: &str) -> Result<Rendered> {
        crate::playbooks::render_document(&spec_vars(link), work).await
    }

    /// The ids of the claims in plan `text`, in order.
    fn claim_ids(text: &str) -> Vec<&str> {
        let mut ids = Vec::new();
        for line in text.lines() {
            if let Some(claim) = line.strip_prefix("claim: ") {
                ids.extend(claim.split(' ').next());
            }
        }
        ids
    }

    /// The line of plan `text` that starts with `start`.
    fn line_of<'a>(text: &'a str, start: &str) -> &'a str {
        let found = text.lines().find(|line| line.starts_with(start));
        found.expect("the plan has the line")
    }

    #[tokio::test]
    async fn a_word_spec_on_sharepoint_becomes_claims_and_a_topic_marked_with_its_label() {
        let dir = tempfile::tempdir().unwrap();
        let table = format!(
            "<w:tbl><w:tr>{}{}</w:tr><w:tr>{}{}</w:tr></w:tbl>",
            cell(&["Field"]),
            cell(&["Rule"]),
            cell(&["Album name"]),
            cell(&["At most 80 characters"]),
        );
        let story = "User Story 1 - Create an album (Priority: P1)";
        let listed = "Given a library, When the user creates an album, Then it is listed";
        let shown = "Given an album, When the user adds a photo, Then the album shows it";
        let body = [
            para("Title", "Photo albums"),
            para("Heading1", "User Scenarios &amp; Testing"),
            para("Heading2", story),
            para("", "A user groups photos into an album."),
            item(0, listed),
            item(0, shown),
            para("Heading1", "Requirements"),
            para("", "FR-001: Users can create albums."),
            para("Heading1", "Success Criteria"),
            para("", "SC-001: A user creates an album in under a minute."),
            para("Heading2", "Key Entities"),
            table,
        ]
        .concat();
        let word = docx(&body);
        let name = "Photo albums.docx";
        let (fake, work) = fake_work(
            dir.path(),
            drive_item(name, WORD, word.len(), Some("Confidential")),
            file_content(name, WORD, &word),
        )
        .await;
        let rendered = import(&work, LINK).await.unwrap();

        // The metadata first, with the sensitivity label selected; then the content.
        let share = share_path(LINK).unwrap();
        let select = "id,name,size,file,webUrl,lastModifiedDateTime,sensitivityLabel";
        let entity = format!("{share}/driveItem?$select={select}");
        let fetch = json!({ "entityUrls": [entity] });
        let fetch_blob = json!({ "path": format!("{share}/driveItem/content") });
        let calls = fake.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert_eq!(calls[0], ("fetch".to_owned(), fetch));
        assert_eq!(calls[1], ("fetch_blob".to_owned(), fetch_blob));
        // Both through the audited client, as a spec import.
        let path = dir.path().join(FILE);
        let audited = crate::workiq::audit::read(&path).unwrap();
        let audited: Vec<(&str, &str)> = audited
            .iter()
            .map(|e| (e.tool.as_str(), e.purpose.as_str()))
            .collect();
        assert_eq!(
            audited,
            [("fetch", "spec-import"), ("fetch_blob", "spec-import")]
        );

        // The user story, the requirement and the success criterion are the claims.
        let text = &rendered.text;
        assert_eq!(claim_ids(text), ["US-1", "FR-001", "SC-001"]);
        let story = "claim: US-1 User story 1, Create an album (P1): each of its 2 acceptance \
                     scenarios passes\n";
        assert!(text.contains(story), "{text}");
        let requirement = "claim: FR-001 Users can create albums.\n";
        assert!(text.contains(requirement), "{text}");
        let implement = line_of(text, "impl ");
        assert!(implement.contains(" claims=US-1,FR-001 "), "{implement}");
        let context = " context=spec-photo-albums ";
        assert!(implement.contains(context), "{implement}");
        // Prompts name the document by its file name; its link is only in the comment.
        let named = "Implement Microsoft 365 document Photo albums.docx (Photo albums)";
        assert!(implement.contains(named), "{implement}");
        let signoff = line_of(text, "signoff ");
        assert!(signoff.contains(" claims=SC-001 "), "{signoff}");
        let imported = format!("// Imported from {LINK}. ");
        for line in text.lines() {
            let comment = line.starts_with(&imported);
            assert_eq!(line.contains(LINK), comment, "{line}");
        }

        // The document is the knowledge topic, marked with its origin and label.
        assert_eq!(rendered.knowledge.len(), 1);
        let (topic, knowledge, source) = &rendered.knowledge[0];
        assert_eq!(topic, "spec-photo-albums");
        let from = "Microsoft 365 document Photo albums.docx (Work IQ)";
        assert_eq!(source, from);
        let mark = "---\norigin: workiq\nlabels: Confidential\n---\n";
        assert!(knowledge.starts_with(mark), "{knowledge}");
        let requirements = "# Requirements\n\nFR-001: Users can create albums.";
        assert!(knowledge.contains(requirements), "{knowledge}");
        let row = "| Album name | At most 80 characters |";
        assert!(knowledge.contains(row), "{knowledge}");
        assert!(!knowledge.contains(LINK), "{knowledge}");
        // Like everything Work IQ returns, its text is external content.
        let envelope = "<external_content source=\"workiq:document:Photo albums.docx\">\n\
                        # Photo albums\n";
        assert!(knowledge.contains(envelope), "{knowledge}");
        let closed = knowledge.trim_end().ends_with("</external_content>");
        assert!(closed, "{knowledge}");
        let summary = &rendered.notes[0];
        let label = "Its sensitivity label is Confidential: peers that implement or review it \
                     need a Microsoft 365 grant that includes Confidential";
        assert!(summary.contains(label), "{summary}");

        // Saved, it makes the steps given it Microsoft 365 content with its label.
        let kb = crate::knowledge::KnowledgeBase::new(dir.path().join("knowledge"));
        rendered.save_knowledge(&kb).unwrap();
        let saved = kb.read(topic).unwrap();
        let confidential = Some(vec!["Confidential".to_owned()]);
        assert_eq!(crate::knowledge::parse_mark(&saved), confidential);
        let plan = crate::plan::Plan::parse(text).unwrap().marked(&kb);
        for step in &plan.steps {
            assert_eq!(step.m365, confidential, "{}", step.id);
        }
    }

    #[tokio::test]
    async fn markdown_and_text_specs_are_imported() {
        let dir = tempfile::tempdir().unwrap();
        let markdown = "# Albums\n\n### User Story 1 - Share an album (Priority: P1)\n\n\
                        1. **Given** an album, **When** its owner shares it, **Then** a guest \
                        sees it\n\n## Requirements\n\n- **FR-001**: Users can share an album.\n";
        let name = "Albums.md";
        let (_, work) = fake_work(
            dir.path(),
            drive_item(name, "text/markdown", markdown.len(), Some("General")),
            file_content(name, "text/markdown", markdown.as_bytes()),
        )
        .await;
        let rendered = import(&work, LINK).await.unwrap();
        assert_eq!(claim_ids(&rendered.text), ["US-1", "FR-001", "ACCEPT"]);
        let (topic, knowledge, _) = &rendered.knowledge[0];
        assert_eq!(topic, "spec-albums");
        let requirement = "- **FR-001**: Users can share an album.";
        assert!(knowledge.contains(requirement), "{knowledge}");

        // Text, with Windows line ends and a byte order mark, typed by its name only: its
        // title is its name.
        let text = "\u{feff}Album rules\r\n\r\nFR-001: Albums hold up to 500 photos.\r\n\
                    FR-002: Only an album's owner deletes it.\r\n";
        let name = "Album rules.txt";
        let (_, work) = fake_work(
            dir.path(),
            drive_item(name, "", text.len(), Some("General")),
            file_content(name, "application/octet-stream", text.as_bytes()),
        )
        .await;
        let rendered = import(&work, LINK).await.unwrap();
        assert_eq!(claim_ids(&rendered.text), ["FR-001", "FR-002", "ACCEPT"]);
        let first = "// Microsoft 365 document Album rules.txt: Album rules\n";
        assert!(rendered.text.starts_with(first), "{}", rendered.text);
        let requirement = "claim: FR-001 Albums hold up to 500 photos.\n";
        assert!(rendered.text.contains(requirement), "{}", rendered.text);
        let (topic, knowledge, _) = &rendered.knowledge[0];
        assert_eq!(topic, "spec-album-rules");
        let body = "Album rules\n\nFR-001";
        assert!(knowledge.contains(body), "{knowledge}");
    }

    #[tokio::test]
    async fn pdfs_and_documents_over_4_mb_are_refused_before_their_content_is_fetched() {
        let dir = tempfile::tempdir().unwrap();
        let content = file_content("Spec.md", "text/markdown", b"# Spec\n");

        let pdf = drive_item("Spec.pdf", "application/pdf", 1000, Some("General"));
        let (fake, work) = fake_work(dir.path(), pdf, content.clone()).await;
        let err = import(&work, LINK).await.unwrap_err().to_string();
        let refused = "Spec.pdf is not a Word, Markdown or text document (application/pdf)";
        assert!(err.contains(refused), "{err}");
        assert!(err.contains("Save it as Word or Markdown"), "{err}");
        assert_eq!(fake.tools(), ["fetch"]);

        let big = drive_item("Spec.docx", WORD, 5 * 1024 * 1024, Some("General"));
        let (fake, work) = fake_work(dir.path(), big, content).await;
        let err = import(&work, LINK).await.unwrap_err().to_string();
        let refused = "Spec.docx is too big to read (5.0 MB): a document may have at most 4 MB";
        assert!(err.contains(refused), "{err}");
        assert_eq!(fake.tools(), ["fetch"]);
    }

    #[tokio::test]
    async fn protected_documents_are_refused_with_their_label_before_their_content_is_fetched() {
        let dir = tempfile::tempdir().unwrap();
        let name = "Photo albums.docx";
        let word = docx(&para("", "FR-001: Users can create albums."));
        let mut item = drive_item(name, WORD, word.len(), Some("Highly Confidential"));
        item["data"]["sensitivityLabel"]["protectionEnabled"] = json!(true);
        let content = file_content(name, WORD, &word);
        let (fake, work) = fake_work(dir.path(), item, content).await;
        let err = import(&work, LINK).await.unwrap_err().to_string();
        let refused = "Photo albums.docx is protected by its sensitivity label Highly \
                       Confidential: its content is encrypted, so it can't be read. Save a copy \
                       without protection (or as Markdown) and import that";
        assert!(err.contains(refused), "{err}");
        assert_eq!(fake.tools(), ["fetch"]);
    }

    #[tokio::test]
    async fn a_document_work_iq_cannot_find_fails_with_its_error_class() {
        let dir = tempfile::tempdir().unwrap();
        let error = json!({ "code": "itemNotFound", "message": "The item was not found." });
        let missing = json!({ "statusCode": 404, "data": { "error": error } });
        let (fake, work) = fake_work(dir.path(), missing, Value::Null).await;
        let err = import(&work, LINK).await.unwrap_err();
        let err = err.downcast_ref::<WorkIqError>().expect("a Work IQ error");
        assert_eq!(err.class, Class::Tool);
        let detail = "the fetch tool answered 404: itemNotFound: The item was not found.";
        assert_eq!(err.message, detail);
        assert_eq!(fake.tools(), ["fetch"]);
    }

    #[tokio::test]
    async fn a_tool_error_on_the_document_says_what_work_iq_said_on_one_line_redacted() {
        let dir = tempfile::tempdir().unwrap();
        let (name, mime) = ("Albums.md", "text/markdown");
        let markdown = "# Albums\n\nFR-001: Users can share an album.\n";
        let item = drive_item(name, mime, markdown.len(), Some("General"));
        let content = file_content(name, mime, markdown.as_bytes());
        let (fake, work) = fake_work(dir.path(), item, content).await;
        let token = format!("ghp_{}", "a1B2".repeat(9));
        let said = format!(
            "The item could not be opened.\nToken {token} was refused. {}",
            "More details follow. ".repeat(20)
        );
        fake.fail("fetch", json!([{ "type": "text", "text": said }]));
        let err = import(&work, LINK).await.unwrap_err().to_string();
        let start = "Work IQ's fetch tool reported an error for the document: ";
        let why = err.strip_prefix(start).expect(&err);
        let redacted = "The item could not be opened. Token [REDACTED:github-token] was refused. \
                        More details follow. More";
        assert!(why.starts_with(redacted), "{err}");
        assert!(!err.contains(&token), "{err}");
        assert!(why.chars().count() <= 300, "{err}");
        assert_eq!(fake.tools(), ["fetch"]);

        // When it says nothing, the error ends there.
        fake.fail("fetch_blob", json!([]));
        let err = import(&work, LINK).await.unwrap_err().to_string();
        let ends = "Work IQ's fetch_blob tool reported an error for the document";
        assert_eq!(err, ends);
        assert_eq!(fake.tools(), ["fetch", "fetch", "fetch_blob"]);

        // A failure that you or an admin can fix keeps the client's class and fix.
        let billing = format!("Billing is not set up for this tenant ({token})");
        fake.fail("fetch", json!([{ "type": "text", "text": billing }]));
        let err = import(&work, LINK).await.unwrap_err();
        let failure = err.downcast_ref::<WorkIqError>().expect("a Work IQ error");
        assert_eq!(failure.class, Class::Billing);
        let shown = failure.to_string();
        let redacted = "Billing is not set up for this tenant ([REDACTED:github-token])";
        assert!(shown.contains(redacted), "{shown}");
    }

    #[tokio::test]
    async fn a_document_without_a_label_is_marked_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let markdown = "# Albums\n\nFR-001: Users can share an album.\n";
        let (_, work) = fake_work(
            dir.path(),
            drive_item("Albums.md", "text/markdown", markdown.len(), None),
            file_content("Albums.md", "text/markdown", markdown.as_bytes()),
        )
        .await;
        let rendered = import(&work, LINK).await.unwrap();
        let (_, knowledge, _) = &rendered.knowledge[0];
        let mark = "---\norigin: workiq\nlabels: unknown\n---\n";
        assert!(knowledge.starts_with(mark), "{knowledge}");
        let summary = &rendered.notes[0];
        let unknown = "so it is marked unknown: only your own agents and digital workers get it";
        assert!(summary.contains(unknown), "{summary}");
    }

    #[tokio::test]
    async fn a_document_without_requirements_is_one_sign_off_claim_with_a_warning() {
        let dir = tempfile::tempdir().unwrap();
        let word = docx(
            &[
                para("Title", "Album sharing"),
                para("", "Guests should see the albums shared with them."),
            ]
            .concat(),
        );
        let name = "Album sharing.docx";
        let (_, work) = fake_work(
            dir.path(),
            drive_item(name, WORD, word.len(), Some("General")),
            file_content(name, WORD, &word),
        )
        .await;
        let rendered = import(&work, LINK).await.unwrap();
        let text = &rendered.text;
        assert_eq!(claim_ids(text), ["DOC-1"]);
        let claim = "claim: DOC-1 Album sharing: what it asks for is done\n";
        assert!(text.contains(claim), "{text}");
        let signoff = line_of(text, "signoff ");
        assert!(signoff.contains(" claims=DOC-1 "), "{signoff}");
        let accept = "Accept Album sharing (Album sharing.docx): what it asks for is done.";
        assert!(signoff.contains(accept), "{signoff}");
        assert!(!line_of(text, "impl ").contains("claims="), "{text}");
        let warning = rendered
            .notes
            .iter()
            .find(|note| note.starts_with("Warning: "))
            .expect("a warning");
        let how = "write each requirement on its own line as FR-001: ... (and each success \
                   criterion as SC-001: ...) to get a claim for each";
        assert!(warning.contains(how), "{warning}");
    }

    #[tokio::test]
    async fn links_off_sharepoint_and_onedrive_are_refused_before_any_call() {
        let dir = tempfile::tempdir().unwrap();
        let (fake, work) = fake_work(dir.path(), Value::Null, Value::Null).await;
        let refused = "is not a SharePoint or OneDrive link";
        for link in [
            "http://contoso.sharepoint.com/sites/Team/Spec.docx",
            "https://contoso.example.com/Spec.docx",
        ] {
            let err = import(&work, link).await.unwrap_err().to_string();
            assert!(err.contains(refused), "{err}");
            // The playbook without Work IQ refuses it the same way.
            let err = crate::playbooks::render(SPEC_IMPORT, &spec_vars(link)).unwrap_err();
            let err = err.to_string();
            assert!(err.contains(refused), "{err}");
        }
        assert_eq!(fake.requests.load(Ordering::SeqCst), 0);
        assert!(!dir.path().join(FILE).exists());

        // A link it takes needs Work IQ: the playbook without it says how to import one.
        let err = crate::playbooks::render(SPEC_IMPORT, &spec_vars(LINK)).unwrap_err();
        let err = err.to_string();
        let how = "run proofkin spec import --from <link> --profile NAME --set peer=...";
        assert!(err.contains(how), "{err}");
    }

    #[test]
    fn a_document_spec_goes_only_to_peers_granted_its_label() {
        let dir = tempfile::tempdir().unwrap();
        let kb = crate::knowledge::KnowledgeBase::new(dir.path());
        let peer = |name: &str, labels: &[&str]| crate::trust::TrustedPeer {
            name: name.to_owned(),
            id: iroh::SecretKey::generate().public(),
            addrs: Default::default(),
            can_delegate: false,
            tasks_per_hour: 5,
            access: crate::llm::Access::ReadOnly,
            shares: vec![crate::shares::DATA_SHARE.into()],
            m365: Some(crate::trust::M365Grant {
                labels: labels.iter().map(|l| (*l).to_owned()).collect(),
                tools: false,
            }),
            github: None,
            agents: Vec::new(),
        };
        let general = peer("general", &["General"]);
        let wide = peer("wide", &["General", "Confidential"]);
        let worker = iroh::SecretKey::generate().public();
        let mut trust = crate::trust::TrustStore::default();
        trust.allow(general.clone());
        trust.allow(wide.clone());
        let recipients = crate::plan::Recipients::new(trust, vec![worker]);
        let roles = crate::spec_import::Roles {
            peer: "general",
            folder: "app",
            reviewers: "reviewers",
            test: "test",
            approver: "me",
        };
        // Without label information it is `unknown`: no peer's grant includes that.
        for (label, wide_gets_it) in [(Some("Confidential"), true), (None, false)] {
            let doc = Document {
                name: "Albums.md".into(),
                url: LINK.into(),
                label: label.map(str::to_owned),
                markdown: "# Albums\n\nFR-001: Users can share an album.\n".into(),
            };
            let import = crate::spec_import::from_document(&doc).unwrap();
            kb.write(&import.topic, &import.knowledge, "").unwrap();
            let text = crate::spec_import::plan_text(&import, &roles).unwrap();
            let plan = crate::plan::Plan::parse(&text).unwrap().marked(&kb);
            let step = &plan.steps[0];
            let refused = recipients.refusal(step, "general", &general.id);
            let refused = refused.expect("refused");
            let outside = format!("its grant doesn't include {}", label.unwrap_or("unknown"));
            assert!(refused.contains(&outside), "{refused}");
            let wide_refused = recipients.refusal(step, "wide", &wide.id);
            assert_eq!(wide_refused.is_none(), wide_gets_it, "{wide_refused:?}");
            assert_eq!(recipients.refusal(step, "worker", &worker), None);
        }
    }

    #[test]
    fn share_paths_are_made_only_for_https_sharepoint_and_onedrive_links() {
        // Microsoft Graph's own example of a share id.
        let graph =
            "https://onedrive.live.com/redir?resid=1231244193912!12&authKey=1201919!12921!1";
        let id = concat!(
            "u!aHR0cHM6Ly9vbmVkcml2ZS5saXZlLmNvbS9yZWRpcj9yZXNpZD0xMjMxMjQ0MTkzOTEy",
            "ITEyJmF1dGhLZXk9MTIwMTkxOSExMjkyMSEx",
        );
        assert_eq!(share_path(graph).unwrap(), format!("/shares/{id}"));
        // The webUrl `retrieve` gives for a Word document.
        let doc_aspx = concat!(
            "https://contoso.sharepoint.com/sites/Team/_layouts/15/Doc.aspx",
            "?sourcedoc=%7B0A1B2C3D-0000-4000-8000-000000000001%7D",
            "&file=Photo%20albums.docx&action=default&mobileredirect=true",
        );
        for link in [
            "https://contoso.sharepoint.com/sites/Team/Shared%20Documents/Spec.docx",
            doc_aspx,
            "https://contoso-my.sharepoint.com/:w:/g/personal/ada_contoso_com/EaBcD?e=x1Y2z3",
            // Microsoft's own pre-production SharePoint.
            "https://contoso.sharepoint-df.com/sites/Team/Shared%20Documents/Spec.docx",
            "https://onedrive.live.com/edit?id=ABC123!101&resid=ABC123!101",
            "https://1drv.ms/w/s!AbCdEfGh",
        ] {
            let path = share_path(&format!(" {link} ")).unwrap();
            let id = path.strip_prefix("/shares/u!").expect(&path);
            assert!(!id.contains(['=', '+', '/']), "{id}");
            let decoded = BASE64URL_NOPAD.decode(id.as_bytes()).unwrap();
            assert_eq!(String::from_utf8(decoded).unwrap(), link);
        }
        for refused in [
            "http://contoso.sharepoint.com/sites/Team/Spec.docx",
            "https://example.com/Spec.docx",
            "https://contoso.sharepoint.com.evil.example/Spec.docx",
            "https://contoso.sharepoint-df.com.example.com/Spec.docx",
            // Other clouds' SharePoint, until one is seen.
            "https://contoso.sharepoint.us/Spec.docx",
            "https://contoso.sharepoint-mil.us/Spec.docx",
            "https://contoso.sharepoint.cn/Spec.docx",
            "https://evil.example/?next=https://contoso.sharepoint.com/Spec.docx",
            "https://ada:secret@contoso.sharepoint.com/Spec.docx",
            "https://contoso.sharepoint.com\\@evil.example/Spec.docx",
            "https://contoso.sharepoint.com/Spec docx",
            "ftp://contoso.sharepoint.com/Spec.docx",
            "contoso.sharepoint.com/Spec.docx",
        ] {
            let err = share_path(refused).unwrap_err().to_string();
            assert!(err.contains("not a SharePoint or OneDrive link"), "{err}");
            let hosts = "the https:// link of a document on *.sharepoint.com, \
                         *.sharepoint-df.com, onedrive.live.com or 1drv.ms";
            assert!(err.ends_with(hosts), "{err}");
        }
        assert!(is_link("https://contoso.sharepoint.com/Spec.docx"));
        assert!(is_link(" http://example.com "));
        for folder in ["specs/001-albums", "C:\\specs", "C://specs", ""] {
            assert!(!is_link(folder), "{folder}");
        }
    }

    #[test]
    fn documents_are_read_by_mime_type_then_by_extension() {
        assert_eq!(Type::of_file(WORD, "Spec"), Some(Type::Word));
        assert_eq!(
            Type::of_file("text/markdown; charset=utf-8", "Spec.bin"),
            Some(Type::Markdown)
        );
        assert_eq!(Type::of_file("", "Spec.MD"), Some(Type::Markdown));
        assert_eq!(
            Type::of_file("application/octet-stream", "notes.txt"),
            Some(Type::Text)
        );
        // A type that is not read stays refused, whatever the name says.
        assert_eq!(Type::of_file("application/pdf", "Spec.docx"), None);
        assert_eq!(Type::of_file("", "Spec.pdf"), None);
        assert_eq!(Type::of_file("", "Spec"), None);
    }

    fn blob(content: Value) -> ToolResult {
        ToolResult {
            content: Vec::new(),
            structured_content: Some(content),
            is_error: false,
            ids: RequestIds::default(),
        }
    }

    #[test]
    fn document_content_must_be_complete_of_a_type_read_and_found() {
        let encoded = BASE64.encode(b"# Spec\n");
        let ok = blob(json!({
            "statusCode": 200, "contentType": "text/markdown", "blobName": "Spec.md",
            "sizeBytes": 7, "base64Content": encoded,
        }));
        let (kind, bytes) = blob_of(&ok, "Spec.md").unwrap();
        assert_eq!(kind, Type::Markdown);
        assert_eq!(bytes, b"# Spec\n");

        let short = blob(json!({
            "statusCode": 200, "contentType": "text/markdown", "sizeBytes": 70,
            "base64Content": encoded,
        }));
        let err = blob_of(&short, "Spec.md").unwrap_err().to_string();
        assert!(err.contains("incomplete"), "{err}");

        let pdf = blob(json!({
            "statusCode": 200, "contentType": "application/pdf", "sizeBytes": 7,
            "base64Content": encoded,
        }));
        let err = blob_of(&pdf, "Spec.md").unwrap_err().to_string();
        assert!(err.contains("application/pdf"), "{err}");

        // A status other than 200 fails with the class Work IQ's errors have for it.
        let denied = blob(json!({
            "statusCode": 403, "error": {"code": "accessDenied", "message": "No access"},
        }));
        let err = blob_of(&denied, "Spec.md").unwrap_err();
        let err = err.downcast_ref::<WorkIqError>().expect("a Work IQ error");
        assert_eq!(err.class, crate::workiq::client::Class::DeniedByPolicy);
        let detail = "the fetch_blob tool answered 403: accessDenied: No access";
        assert_eq!(err.message, detail);
    }
}
