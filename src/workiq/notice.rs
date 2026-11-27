//! Sign-off notices in Microsoft 365 (docs/work-iq.md, section 7). When a sign-off waits for
//! a profile's owner (a plan's `approve @me` step, or a change a digital worker's task holds
//! for its owner), Proofkin can also tell them by mail or in a Teams chat, with the Work IQ
//! account the profile uses, so they needn't watch the web UI; they still answer it in
//! Proofkin. Notices are off by default; the owner opts in once per profile
//! (`proofkin workiq notices mail|teams`).
//!
//! A notice holds the sign-off's title, its run and step (or its task), the agent's name and
//! how to answer, never what is signed off. It is a change Proofkin makes with the account,
//! which the owner opted in to, so it needs no sign-off of its own: it goes through the
//! audited client with purpose `notice` and counts against the budgets, it is sent once, in
//! the background, and never again, and a failure is told in the activity while the sign-off
//! still waits.

use std::fmt;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use super::audit::{Audit, CallContext, Purpose};
use super::changes::{self, Change};
use super::client::{Client, ToolResult, WorkIqError};
use super::document::{failure, number};
use super::{Endpoints, connect_at};
use crate::profile::{Profile, WorkIqNotices};
use crate::redact::redact;
use crate::runs::one_line;

/// The most characters of a sign-off's title (and of the other names) a notice holds.
pub const MAX_TITLE_CHARS: usize = 200;
/// Teams' "chat with yourself". It isn't in the Microsoft Graph reference, so it isn't
/// offered.
pub const SELF_CHAT: &str = "48:notes";
/// How many of the account's recent chats `proofkin workiq notices teams --list` shows.
pub const LISTED_CHATS: usize = 20;
/// The longest Teams chat id taken.
const MAX_CHAT_ID_CHARS: usize = 256;
const HOW_TO_ANSWER: &str = "Answer it in Proofkin's web UI or with /approve in the agent's \
                             terminal. This message holds nothing of what you are asked to \
                             sign off.";
const LIST_HINT: &str = "choose one of your chats from: proofkin workiq notices teams --list";

/// A sign-off that waits for its owner, as its notice tells it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SignOff {
    /// The agent that waits for it.
    pub agent: String,
    /// The sign-off's title.
    pub title: String,
    /// The plan run it is part of.
    pub run: Option<String>,
    /// The plan step it is.
    pub step: Option<String>,
    /// The owner's number for the task whose change it is.
    pub task: Option<u64>,
}

impl SignOff {
    /// The subject of its notice by mail.
    pub fn subject(&self) -> String {
        format!("Proofkin: sign-off waiting: {}", clean(&self.title))
    }

    /// The text of its notice: who waits for which sign-off, and how to answer it.
    pub fn text(&self) -> String {
        let mut text = format!(
            "{} waits for your sign-off of \"{}\"",
            clean(&self.agent),
            clean(&self.title)
        );
        let mut parts = Vec::new();
        if let Some(run) = &self.run {
            parts.push(format!("run {}", clean(run)));
        }
        if let Some(step) = &self.step {
            parts.push(format!("step {}", clean(step)));
        }
        if let Some(task) = self.task {
            parts.push(format!("task #{task}"));
        }
        if !parts.is_empty() {
            text += &format!(" ({})", parts.join(", "));
        }
        text + ". " + HOW_TO_ANSWER
    }

    fn purpose(&self) -> Purpose {
        Purpose::Notice {
            run: self.run.clone(),
            step: self.step.clone(),
        }
    }
}

/// `text` as a notice holds it: plain text on one line, without control characters or
/// characters that change how the text around them shows, its secrets redacted, cut to
/// [`MAX_TITLE_CHARS`] characters.
pub fn clean(text: &str) -> String {
    let plain: String = text
        .chars()
        .map(|c| {
            if c.is_control() || invisible(c) {
                ' '
            } else {
                c
            }
        })
        .collect();
    // Redacted before it is cut, so a cut never leaves part of a secret unrecognized.
    let (redacted, _) = redact(&one_line(&plain));
    if redacted.chars().count() <= MAX_TITLE_CHARS {
        return redacted;
    }
    let mut cut: String = redacted.chars().take(MAX_TITLE_CHARS - 1).collect();
    cut.push('…');
    cut
}

/// Zero-width and bidirectional formatting characters.
fn invisible(c: char) -> bool {
    matches!(
        c,
        '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2069}' | '\u{FEFF}'
    )
}

/// The change that sends `sign_off`'s notice where `setting` says: a mail to `address`, the
/// account's own (`do_action` on `/me/sendMail`, not kept in Sent Items), or a message in a
/// Teams chat (`create_entity` on `/chats/{id}/messages`). Plain text either way.
pub fn change(setting: &WorkIqNotices, address: &str, sign_off: &SignOff) -> Change {
    let content = sign_off.text();
    match setting {
        WorkIqNotices::Mail => Change {
            tool: "do_action".to_owned(),
            path: "/me/sendMail".to_owned(),
            body: Some(json!({
                "message": {
                    "subject": sign_off.subject(),
                    "body": { "contentType": "Text", "content": content },
                    "toRecipients": [{ "emailAddress": { "address": address } }],
                },
                "saveToSentItems": false,
            })),
            if_match: None,
        },
        WorkIqNotices::Teams { chat } => Change {
            tool: "create_entity".to_owned(),
            path: format!("/chats/{chat}/messages"),
            body: Some(json!({ "body": { "contentType": "text", "content": content } })),
            if_match: None,
        },
    }
}

/// Why `id` can't be the Teams chat notices go to, if it can't: it is a chat id as Microsoft
/// Graph gives it (like `19:...@thread.v2`), never a path, and not Teams' chat with yourself
/// ([`SELF_CHAT`]).
pub fn check_chat_id(id: &str) -> std::result::Result<(), String> {
    if id.eq_ignore_ascii_case(SELF_CHAT) {
        return Err(format!(
            "{SELF_CHAT} (Teams' chat with yourself) isn't in the Microsoft Graph reference, so \
             Proofkin doesn't offer it; {LIST_HINT}"
        ));
    }
    let allowed = |c: char| c.is_ascii_alphanumeric() || ":@._-=".contains(c);
    if id.is_empty() || id.chars().count() > MAX_CHAT_ID_CHARS || !id.chars().all(allowed) {
        let shown: String = id.chars().take(60).collect();
        return Err(format!(
            "{shown:?} isn't a Teams chat id (like 19:...@thread.v2); {LIST_HINT}"
        ));
    }
    Ok(())
}

/// A Teams chat of the account: its id, and its topic and type as Microsoft Graph gives them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chat {
    pub id: String,
    pub topic: Option<String>,
    pub chat_type: Option<String>,
}

impl Chat {
    fn from_data(data: &Value) -> Option<Chat> {
        let text = |key: &str| {
            data.get(key)
                .and_then(Value::as_str)
                .map(clean)
                .filter(|text| !text.is_empty())
        };
        Some(Chat {
            id: data.get("id")?.as_str()?.to_owned(),
            topic: text("topic"),
            chat_type: text("chatType"),
        })
    }

    /// One line: its id, then its type and topic when Microsoft Graph gave them.
    pub fn describe(&self) -> String {
        let mut line = self.id.clone();
        if let Some(kind) = &self.chat_type {
            line += &format!("  {kind}");
        }
        if let Some(topic) = &self.topic {
            line += &format!("  \"{topic}\"");
        }
        line
    }
}

/// The Teams chat `id`, once Microsoft Graph says the account is a member of it (a `fetch` of
/// `/me/chats/{id}`, audited as a command). A chat it isn't a member of is refused.
pub async fn check_chat(client: &Client, id: &str) -> Result<Chat> {
    check_chat_id(id).map_err(|e| anyhow!(e))?;
    let path = format!("/me/chats/{id}?$select=id,topic,chatType");
    let result = client
        .fetch_in(&CallContext::command(), &[&path])
        .await
        .context("checking that you are a member of the Teams chat")?;
    let entity = first_entity(&result)?;
    match number(entity.get("statusCode")) {
        Some(200) => {}
        Some(status @ (403 | 404)) => bail!(
            "you aren't a member of Teams chat {id} (Microsoft Graph answered {status}), so \
             notices can't go there; {LIST_HINT}"
        ),
        Some(status) => return Err(failed_entity(status, entity, &result)),
        None => bail!("Work IQ's fetch tool gave no statusCode for the chat"),
    }
    let data = entity.get("data").unwrap_or(&Value::Null);
    let chat = Chat::from_data(data);
    Ok(Chat {
        id: id.to_owned(),
        topic: chat.as_ref().and_then(|c| c.topic.clone()),
        chat_type: chat.and_then(|c| c.chat_type),
    })
}

/// The account's recent Teams chats, at most [`LISTED_CHATS`] (a `fetch` of `/me/chats`,
/// audited as a command), without the ones notices can't go to.
pub async fn list_chats(client: &Client) -> Result<Vec<Chat>> {
    let path = format!("/me/chats?$select=id,topic,chatType&$top={LISTED_CHATS}");
    let result = client
        .fetch_in(&CallContext::command(), &[&path])
        .await
        .context("listing your Teams chats")?;
    let entity = first_entity(&result)?;
    match number(entity.get("statusCode")) {
        Some(200) => {}
        Some(status) => return Err(failed_entity(status, entity, &result)),
        None => bail!("Work IQ's fetch tool gave no statusCode for your chats"),
    }
    let data = entity.get("data").unwrap_or(&Value::Null);
    let listed = data
        .get("value")
        .unwrap_or(data)
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    Ok(listed
        .iter()
        .filter_map(Chat::from_data)
        .filter(|chat| check_chat_id(&chat.id).is_ok())
        .take(LISTED_CHATS)
        .collect())
}

/// The first entity `fetch` gave (`structuredContent.results[0]`).
fn first_entity(result: &ToolResult) -> Result<&Value> {
    result
        .structured_content
        .as_ref()
        .and_then(|content| content.get("results"))
        .and_then(Value::as_array)
        .and_then(|results| results.first())
        .context("Work IQ's fetch tool gave no result")
}

fn failed_entity(status: u64, entity: &Value, result: &ToolResult) -> anyhow::Error {
    let status = u16::try_from(status).unwrap_or(u16::MAX);
    WorkIqError::entity("fetch", status, &failure(entity), result.ids.clone()).into()
}

/// Fails if Work IQ answered a notice's `tool` call with a failed status (a call it reported
/// failed is already an error).
fn check_sent(tool: &str, result: &ToolResult) -> Result<()> {
    let (answer, status) = result.answer_with_status();
    match status {
        Some(status @ 400..) => {
            Err(WorkIqError::entity(tool, status, &failure(&answer), result.ids.clone()).into())
        }
        _ => Ok(()),
    }
}

/// Sends the notices of one profile's sign-offs (see [`Notifier::of`]).
pub struct Notifier {
    setting: WorkIqNotices,
    /// The audited client of the account notices go from, and the account's own address; or
    /// why the account can't be used.
    account: std::result::Result<(Arc<Client>, String), String>,
}

impl fmt::Debug for Notifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Notifier")
            .field("setting", &self.setting)
            .finish_non_exhaustive()
    }
}

impl Notifier {
    /// Notices where `setting` says, through `client`, whose account's own address is
    /// `address`.
    pub fn new(client: Arc<Client>, setting: WorkIqNotices, address: impl Into<String>) -> Self {
        Notifier {
            setting,
            account: Ok((client, address.into())),
        }
    }

    /// The notices of `profile`'s sign-offs, as its profile file says now: none unless the
    /// profile whose Work IQ account it uses (a digital worker may use its owner's) turned
    /// them on. They go with that account, through a client audited for `profile`. An account
    /// that can't be used fails each notice, which the sign-off's agent tells.
    pub fn of(profile: &Profile) -> Option<Arc<Notifier>> {
        match Endpoints::configured() {
            Ok(endpoints) => Self::of_at(profile, &endpoints),
            Err(e) => {
                let profile = profile.reload().unwrap_or_else(|_| profile.clone());
                Some(Arc::new(Notifier {
                    setting: setting_of(&profile)?,
                    account: Err(format!("{e:#}")),
                }))
            }
        }
    }

    /// [`Notifier::of`] for a Work IQ at `endpoints`.
    pub fn of_at(profile: &Profile, endpoints: &Endpoints) -> Option<Arc<Notifier>> {
        let profile = profile.reload().unwrap_or_else(|_| profile.clone());
        let setting = setting_of(&profile)?;
        let account = address_of(&profile).and_then(|address| {
            let client = connect_at(&profile, endpoints)
                .map_err(|e| format!("{e:#}"))?
                .client
                .with_audit(Arc::new(Audit::for_profile(&profile)));
            Ok((Arc::new(client), address))
        });
        Some(Arc::new(Notifier { setting, account }))
    }

    /// The notices of `profile`'s sign-offs (see [`Notifier::of`]) through `client`, the
    /// audited client of the account `profile` uses.
    pub fn with_client(profile: &Profile, client: Arc<Client>) -> Option<Arc<Notifier>> {
        let setting = setting_of(profile)?;
        let account = address_of(profile).map(|address| (client, address));
        Some(Arc::new(Notifier { setting, account }))
    }

    /// Sends `sign_off`'s notice: one call of a Work IQ change tool, audited with purpose
    /// `notice`, which is never sent again.
    pub async fn send(&self, sign_off: &SignOff) -> Result<()> {
        let (client, address) = self.account.as_ref().map_err(|e| anyhow!("{e}"))?;
        match &self.setting {
            WorkIqNotices::Mail if !address.contains('@') => bail!(
                "notices by mail go to the Work IQ account's own address, and {address:?} isn't \
                 one; sign in with your account (for Microsoft's CLI: proofkin auth login \
                 workiq --cli --account you@example.com), or send them to Teams: proofkin \
                 workiq notices teams --chat <id>"
            ),
            WorkIqNotices::Mail => {}
            WorkIqNotices::Teams { chat } => check_chat_id(chat).map_err(|e| anyhow!(e))?,
        }
        let change = change(&self.setting, address, sign_off);
        changes::check(&change).map_err(|rule| anyhow!("the notice breaks a rule: {rule}"))?;
        let context = CallContext::local(sign_off.purpose());
        let result = client
            .call_tool_in(&context, &change.tool, change.arguments())
            .await?;
        check_sent(&change.tool, &result)
    }

    /// Sends `sign_off`'s notice in the background, so it never holds up the sign-off; if it
    /// isn't sent, `failed` is told why (on one line, redacted).
    pub fn spawn(
        self: &Arc<Self>,
        sign_off: SignOff,
        failed: impl FnOnce(String) + Send + 'static,
    ) {
        let notifier = self.clone();
        tokio::spawn(async move {
            if let Err(e) = notifier.send(&sign_off).await {
                failed(redact(&one_line(&format!("{e:#}"))).0);
            }
        });
    }
}

/// Where the sign-off notices of `profile` go: the setting of the profile whose Work IQ
/// account it uses.
fn setting_of(profile: &Profile) -> Option<WorkIqNotices> {
    profile.workiq_profile().ok()?.config.workiq_notices
}

/// Where a profile's sign-off notices go, as `workiq doctor` and the web UI show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Setting {
    /// The profile whose setting it is: the profile itself, or the owner whose Work IQ a
    /// digital worker uses.
    pub profile: String,
    /// Where they go; none when they are off.
    pub to: Option<WorkIqNotices>,
    /// The Work IQ account's own address, where notices by mail go.
    pub address: Option<String>,
}

impl Setting {
    /// The setting `profile`'s notices follow: none when it can't use a Work IQ account.
    pub fn of(profile: &Profile) -> Setting {
        match profile.workiq_profile() {
            Ok(holder) => Setting {
                address: holder.config.workiq.as_ref().map(|a| a.upn.clone()),
                to: holder.config.workiq_notices.clone(),
                profile: holder.name,
            },
            Err(_) => Setting {
                profile: profile.name.clone(),
                to: None,
                address: None,
            },
        }
    }

    /// Where notices go, in words: `by mail to <address>`, `in Teams chat <id>`, or `off`.
    pub fn describe(&self) -> String {
        match &self.to {
            Some(to) => to.describe(self.address.as_deref().unwrap_or("the account's address")),
            None => "off".to_owned(),
        }
    }

    /// `{"to": "mail", "address": ...}`, `{"to": "teams", "chat": ...}`, or null when off.
    pub fn to_json(&self) -> Value {
        let mut value = serde_json::to_value(&self.to).unwrap_or(Value::Null);
        if let (Some(WorkIqNotices::Mail), Some(fields)) = (&self.to, value.as_object_mut()) {
            fields.insert("address".to_owned(), json!(self.address));
        }
        value
    }
}

/// The own address of the Work IQ account `profile` uses.
fn address_of(profile: &Profile) -> std::result::Result<String, String> {
    profile
        .workiq_account()
        .map(|(_, account)| account.upn)
        .map_err(|e| format!("{e:#}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan_step() -> SignOff {
        SignOff {
            agent: "boss".into(),
            title: "Plan step ok".into(),
            run: Some("r1".into()),
            step: Some("ok".into()),
            task: None,
        }
    }

    #[test]
    fn a_notice_says_who_waits_for_which_sign_off_and_how_to_answer_it() {
        let step = plan_step();
        assert_eq!(step.subject(), "Proofkin: sign-off waiting: Plan step ok");
        assert_eq!(
            step.text(),
            "boss waits for your sign-off of \"Plan step ok\" (run r1, step ok). Answer it in \
             Proofkin's web UI or with /approve in the agent's terminal. This message holds \
             nothing of what you are asked to sign off."
        );
        let held = SignOff {
            agent: "helper".into(),
            title: "Microsoft 365 change from helper".into(),
            task: Some(12),
            ..SignOff::default()
        };
        assert!(
            held.text().starts_with(
                "helper waits for your sign-off of \"Microsoft 365 change from helper\" \
                 (task #12). Answer it in Proofkin's web UI"
            ),
            "{}",
            held.text()
        );
    }

    #[test]
    fn a_title_is_cut_to_200_characters_redacted_and_stripped_of_control_characters() {
        let secret = format!("ghp_{}", "a1".repeat(20));
        let title = format!(
            "Ship\u{7} it\r\nnow\u{202E}\u{200B} {secret} {}",
            "x".repeat(400)
        );
        let cleaned = clean(&title);
        assert!(
            cleaned.starts_with("Ship it now [REDACTED:github-token] xxx"),
            "{cleaned}"
        );
        assert!(!cleaned.contains("ghp_"));
        assert!(!cleaned.chars().any(|c| c.is_control() || invisible(c)));
        assert_eq!(cleaned.chars().count(), MAX_TITLE_CHARS);
        assert!(cleaned.ends_with('…'));
        let sign_off = SignOff {
            title,
            ..plan_step()
        };
        assert!(sign_off.subject().ends_with('…'));
        assert!(!sign_off.text().contains("ghp_"));
        assert!(!sign_off.text().chars().any(char::is_control));
    }

    #[test]
    fn a_mail_notice_goes_to_the_accounts_own_address_and_a_teams_notice_to_its_chat() {
        let step = plan_step();
        let mail = change(&WorkIqNotices::Mail, "alice@contoso.example", &step);
        assert_eq!(
            (mail.tool.as_str(), mail.path.as_str()),
            ("do_action", "/me/sendMail")
        );
        assert_eq!(
            mail.arguments(),
            json!({
                "actionUrl": "/me/sendMail",
                "jsonBody": {
                    "message": {
                        "subject": "Proofkin: sign-off waiting: Plan step ok",
                        "body": { "contentType": "Text", "content": step.text() },
                        "toRecipients": [
                            { "emailAddress": { "address": "alice@contoso.example" } }
                        ],
                    },
                    "saveToSentItems": false,
                },
            })
        );
        let chat = "19:crew_1@thread.v2";
        let teams = WorkIqNotices::Teams { chat: chat.into() };
        assert_eq!(
            change(&teams, "alice@contoso.example", &step).arguments(),
            json!({
                "parentUrl": "/chats/19:crew_1@thread.v2/messages",
                "jsonBody": { "body": { "contentType": "text", "content": step.text() } },
            })
        );
        assert!(changes::check(&mail).is_ok());
        // As profile.json keeps it.
        assert_eq!(
            serde_json::to_value(&WorkIqNotices::Mail).unwrap(),
            json!({ "to": "mail" })
        );
        assert_eq!(
            serde_json::to_value(&teams).unwrap(),
            json!({ "to": "teams", "chat": chat })
        );
    }

    #[test]
    fn only_a_teams_chat_id_is_taken_and_never_the_chat_with_yourself() {
        assert!(check_chat_id("19:crew_1@thread.v2").is_ok());
        assert!(check_chat_id("19:a1b2-c3_d4@unq.gbl.spaces").is_ok());
        let notes = check_chat_id("48:notes").unwrap_err();
        assert!(
            notes.contains("isn't in the Microsoft Graph reference"),
            "{notes}"
        );
        assert!(check_chat_id("48:NOTES").is_err());
        let long = "9".repeat(MAX_CHAT_ID_CHARS + 1);
        for bad in [
            "",
            "19:a/../../me/sendMail",
            "19:a?x=1",
            "19:a#b",
            "19:a b",
            "19:%2e%2e",
            long.as_str(),
        ] {
            assert!(check_chat_id(bad).is_err(), "{bad:?}");
        }
    }

    #[tokio::test]
    async fn without_the_setting_there_is_no_notice_and_without_an_account_it_fails() {
        let root = tempfile::tempdir().unwrap();
        let endpoints = Endpoints {
            mcp_url: "http://127.0.0.1:9/mcp".into(),
            authority: "http://127.0.0.1:9".into(),
            discovery_url: "http://127.0.0.1:9/discovery".into(),
        };
        let mut profile = Profile::open_in(root.path(), "alice").unwrap();
        profile.save().unwrap();
        assert!(Notifier::of_at(&profile, &endpoints).is_none());

        profile.config.workiq_notices = Some(WorkIqNotices::Mail);
        profile.save().unwrap();
        let notifier = Notifier::of_at(&profile, &endpoints).expect("notices are on");
        let error = notifier.send(&plan_step()).await.unwrap_err();
        assert!(format!("{error:#}").contains("not signed in"), "{error:#}");
    }
}
