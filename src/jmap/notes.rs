//! Email-backed ActiveSync Notes.
//!
//! Stalwart has no native JMAP Notes capability (confirmed against a live
//! session response: capabilities are core/mail/calendars/contacts/
//! submission/vacationresponse/sieve/blob/quota/filenode/principals/
//! emailpush/webpush/websocket -- nothing notes-shaped, and JMAP itself has
//! no ratified Notes extension). This maps ActiveSync Notes onto JMAP Email
//! instead: each note is a synthetic RFC822 message imported into a
//! dedicated Mailbox named literally `"Notes"` -- the SAME mailbox a real
//! IMAP/Notes-capable mail client would already be using on this account,
//! not a separate gateway-only folder. (An earlier PHP reference
//! implementation used a distinct `"EAS Notes"` mailbox to avoid colliding
//! with a pre-existing user folder of that name; here the two are
//! deliberately unified into one, since the account this was built against
//! already treats `"Notes"` as its one true notes folder either way.)
//!
//! Two structural JMAP facts drive this whole module, both confirmed live,
//! not assumed from the spec:
//!
//! 1. `Email/set` REJECTS updating `subject` or body content on an existing
//!    Email (`invalidProperties`). Only `keywords`/`mailboxIds` are mutable
//!    post-import. Editing a note therefore always means import a
//!    replacement email + destroy the old one -- there is no in-place edit.
//!
//! 2. MS-ASCMD requires a Sync ServerId to stay constant across edits ("a
//!    given item MUST have the same ServerId value after a
//!    resynchronization"). Because of (1), the underlying JMAP Email id
//!    changes on every edit. Exposing that raw id as the ActiveSync
//!    ServerId therefore violates the protocol and silently corrupts sync
//!    state (confirmed live against a WBXML trace with the PHP reference
//!    implementation: an edit that changed the underlying id produced a
//!    server reply that only said "remove the old id", with no way to tell
//!    the client about the replacement -- the note simply vanished from the
//!    device even though the edit had saved correctly). Fix: every note
//!    carries a permanent random id (`noteid-<hex>`) as a JMAP keyword,
//!    generated once at creation and copied forward into the replacement
//!    email on every edit. That keyword-derived id -- never the JMAP
//!    Email's own `id` -- is the only thing ever exposed as a ServerId.
//!    Resolution from stable id -> "whichever JMAP email currently backs
//!    it" goes through an `Email/query` `hasKeyword` filter (verified live
//!    that Stalwart supports this as a query filter).
//!
//! Categories round-trip through a slugified keyword
//! (`notecat-<slug>`) since JMAP/IMAP keywords can't hold spaces or
//! arbitrary characters -- lossless for ordinary names ("Work"), lossy for
//! exotic ones. There is nowhere else to put them; this is the same
//! constraint an earlier FileNode-backed design hit even harder (FileNode
//! has no field for categories at all, which is the main reason Email won
//! out as the storage choice here).

use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};

use anyhow::Context;
use serde::Deserialize;

use crate::{
    jmap::{
        capabilities,
        client::{AuthenticatedSession, GetResponse, JmapClient, JmapResponse, MethodCall},
    },
    model::{EmailBodyType, Note},
};

/// Deliberately the same name a real IMAP/Notes-capable client would use on
/// this account -- see module docs.
pub const NOTES_MAILBOX_NAME: &str = "Notes";

const STABLE_ID_PREFIX: &str = "noteid-";
const CATEGORY_PREFIX: &str = "notecat-";
const NOTE_MARKER_KEYWORD: &str = "$note";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteSummary {
    pub id: String,
    pub hash: String,
}

pub struct NoteContent<'a> {
    pub subject: &'a str,
    pub body: &'a str,
    pub body_type: EmailBodyType,
    pub categories: &'a [String],
}

impl JmapClient {
    /// Finds the account's `"Notes"` mailbox without creating one. Used for
    /// FolderSync discovery -- an account with no notes yet simply doesn't
    /// advertise a Notes collection, rather than one being force-created
    /// just to be listed.
    pub async fn find_notes_mailbox_id(
        &self,
        auth: &AuthenticatedSession,
    ) -> anyhow::Result<Option<String>> {
        let Some(account_id) = auth.session.primary_account_for(capabilities::MAIL) else {
            return Ok(None);
        };
        let response: JmapResponse<serde_json::Value> = self
            .api_call(
                auth,
                &[capabilities::CORE.to_owned(), capabilities::MAIL.to_owned()],
                vec![MethodCall::new(
                    "Mailbox/get",
                    serde_json::json!({ "accountId": account_id }),
                    "mailboxes",
                )],
            )
            .await?;

        for method in response.method_responses {
            if method.0 == "Mailbox/get" {
                let get: GetResponse<MailboxIdentity> = serde_json::from_value(method.1)
                    .context("invalid Mailbox/get response while locating Notes mailbox")?;
                return Ok(get
                    .list
                    .into_iter()
                    .find(|mailbox| {
                        mailbox.parent_id.is_none() && mailbox.name == NOTES_MAILBOX_NAME
                    })
                    .map(|mailbox| mailbox.id));
            }
        }
        Ok(None)
    }

    /// Find-or-create variant, used by actual note-saving flows (unlike
    /// discovery, saving a note needs somewhere to put it even on an
    /// account that has never had one before).
    pub async fn ensure_notes_mailbox_id(
        &self,
        auth: &AuthenticatedSession,
    ) -> anyhow::Result<String> {
        if let Some(id) = self.find_notes_mailbox_id(auth).await? {
            return Ok(id);
        }

        let Some(account_id) = auth.session.primary_account_for(capabilities::MAIL) else {
            anyhow::bail!("JMAP Mail capability is not available");
        };
        let response: JmapResponse<serde_json::Value> = self
            .api_call(
                auth,
                &[capabilities::CORE.to_owned(), capabilities::MAIL.to_owned()],
                vec![MethodCall::new(
                    "Mailbox/set",
                    serde_json::json!({
                        "accountId": account_id,
                        "create": { "nf1": { "name": NOTES_MAILBOX_NAME } }
                    }),
                    "0",
                )],
            )
            .await?;

        for method in response.method_responses {
            if method.0 == "Mailbox/set" {
                if let Some(id) = method
                    .1
                    .get("created")
                    .and_then(|created| created.get("nf1"))
                    .and_then(|note| note.get("id"))
                    .and_then(|id| id.as_str())
                {
                    return Ok(id.to_owned());
                }
                anyhow::bail!(
                    "Mailbox/set did not create the Notes mailbox: {:?}",
                    method.1
                );
            }
        }
        anyhow::bail!("Mailbox/set response for Notes mailbox creation was missing")
    }

    /// Resolves a note's permanent stable id to whichever JMAP Email
    /// currently backs it. Not scoped to a mailbox -- the stable id (16
    /// random hex chars) is unique enough on its own.
    async fn resolve_note_email_id(
        &self,
        auth: &AuthenticatedSession,
        stable_id: &str,
    ) -> anyhow::Result<Option<String>> {
        let Some(account_id) = auth.session.primary_account_for(capabilities::MAIL) else {
            anyhow::bail!("JMAP Mail capability is not available");
        };
        let response: JmapResponse<serde_json::Value> = self
            .api_call(
                auth,
                &[capabilities::CORE.to_owned(), capabilities::MAIL.to_owned()],
                vec![MethodCall::new(
                    "Email/query",
                    serde_json::json!({
                        "accountId": account_id,
                        "filter": { "hasKeyword": stable_id },
                        "limit": 1
                    }),
                    "0",
                )],
            )
            .await?;

        for method in response.method_responses {
            if method.0 == "Email/query" {
                let ids = method
                    .1
                    .get("ids")
                    .and_then(|value| value.as_array())
                    .cloned()
                    .unwrap_or_default();
                return Ok(ids
                    .first()
                    .and_then(|value| value.as_str())
                    .map(str::to_owned));
            }
        }
        Ok(None)
    }

    pub async fn list_notes(
        &self,
        auth: &AuthenticatedSession,
        mailbox_id: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<NoteSummary>> {
        let Some(account_id) = auth.session.primary_account_for(capabilities::MAIL) else {
            return Ok(Vec::new());
        };

        let calls = vec![
            MethodCall::new(
                "Email/query",
                serde_json::json!({
                    "accountId": account_id,
                    "filter": { "inMailbox": mailbox_id },
                    "limit": limit.clamp(1, 200)
                }),
                "q",
            ),
            MethodCall::new(
                "Email/get",
                serde_json::json!({
                    "accountId": account_id,
                    "#ids": { "resultOf": "q", "name": "Email/query", "path": "/ids" },
                    "properties": ["id", "subject", "keywords", "receivedAt"]
                }),
                "g",
            ),
        ];

        let response: JmapResponse<serde_json::Value> = self
            .api_call(
                auth,
                &[capabilities::CORE.to_owned(), capabilities::MAIL.to_owned()],
                calls,
            )
            .await?;

        for method in response.method_responses {
            if method.0 == "Email/get" {
                let get: GetResponse<NoteEmailObject> = serde_json::from_value(method.1)
                    .context("invalid Email/get response while listing notes")?;
                return Ok(get
                    .list
                    .into_iter()
                    .filter_map(|email| {
                        let stable_id = extract_stable_id(&email.keywords)?;
                        let hash = note_hash(&email);
                        Some(NoteSummary {
                            id: stable_id,
                            hash,
                        })
                    })
                    .collect());
            }
        }
        Ok(Vec::new())
    }

    pub async fn get_note(
        &self,
        auth: &AuthenticatedSession,
        stable_id: &str,
    ) -> anyhow::Result<Option<Note>> {
        let Some(email_id) = self.resolve_note_email_id(auth, stable_id).await? else {
            return Ok(None);
        };
        let Some(account_id) = auth.session.primary_account_for(capabilities::MAIL) else {
            anyhow::bail!("JMAP Mail capability is not available");
        };

        let response: JmapResponse<serde_json::Value> = self
            .api_call(
                auth,
                &[capabilities::CORE.to_owned(), capabilities::MAIL.to_owned()],
                vec![MethodCall::new(
                    "Email/get",
                    serde_json::json!({
                        "accountId": account_id,
                        "ids": [email_id],
                        "properties": [
                            "id", "subject", "receivedAt", "keywords",
                            "textBody", "htmlBody", "bodyValues"
                        ],
                        "fetchAllBodyValues": true,
                        "maxBodyValueBytes": 262144
                    }),
                    "0",
                )],
            )
            .await?;

        for method in response.method_responses {
            if method.0 == "Email/get" {
                let get: GetResponse<NoteEmailObject> = serde_json::from_value(method.1)
                    .context("invalid Email/get response while fetching note")?;
                let Some(email) = get.list.into_iter().next() else {
                    return Ok(None);
                };
                let (body_type, body) = select_note_body(&email);
                return Ok(Some(Note {
                    id: stable_id.to_owned(),
                    title: email.subject.clone(),
                    body,
                    body_type,
                    modified: email.received_at.clone(),
                    categories: extract_categories(&email.keywords),
                }));
            }
        }
        Ok(None)
    }

    /// Creates a new note (`stable_id: None`) or replaces the email backing
    /// an existing one (`stable_id: Some`). Returns the note's stable id --
    /// unchanged from the input on an edit, freshly generated on a create.
    /// Never returns the underlying JMAP Email id; see module docs.
    pub async fn save_note(
        &self,
        auth: &AuthenticatedSession,
        mailbox_id: &str,
        stable_id: Option<&str>,
        content: NoteContent<'_>,
    ) -> anyhow::Result<String> {
        let NoteContent {
            subject,
            body,
            body_type,
            categories,
        } = content;
        let Some(account_id) = auth.session.primary_account_for(capabilities::MAIL) else {
            anyhow::bail!("JMAP Mail capability is not available");
        };

        let note_id = match stable_id {
            Some(id) => id.to_owned(),
            None => generate_stable_id(),
        };

        // Resolve the OLD email BEFORE creating the new one -- once both
        // exist they'd momentarily share the same stable-id keyword, and a
        // limit:1 lookup couldn't reliably tell them apart (hit this exact
        // ordering bug in the PHP reference implementation).
        let old_email_id = match stable_id {
            Some(id) => self.resolve_note_email_id(auth, id).await?,
            None => None,
        };

        // Real bug, confirmed live via a raw MIME diff against a note
        // actually created by Apple's own IMAP Notes sync on this same
        // account: a real EAS Notes edit produced a body with the outer
        // <html> tag doubled (`<html><html>...</html></html>`) while
        // <head>/<body> stayed singular -- the client's own wrap-on-save
        // step doesn't check whether the content it's wrapping is already
        // wrapped. Can't fix client behavior from here; collapse it
        // before persisting so it can't compound across further edits.
        let normalized_body = if body_type == EmailBodyType::Html {
            collapse_duplicate_html_wrapper(body)
        } else {
            body.to_owned()
        };
        let mime = build_note_mime(subject, body_type, &normalized_body, auth.username());
        let blob_id = self
            .upload_blob(auth, mime.into_bytes(), "message/rfc822")
            .await?;

        let mut keywords = serde_json::Map::new();
        keywords.insert(
            NOTE_MARKER_KEYWORD.to_owned(),
            serde_json::Value::Bool(true),
        );
        // Matches what a real IMAP-created note gets by default on this
        // server (observed live) -- notes aren't unread mail, no reason to
        // show them as such.
        keywords.insert("$seen".to_owned(), serde_json::Value::Bool(true));
        keywords.insert(note_id.clone(), serde_json::Value::Bool(true));
        for category in categories {
            let slug = slugify_category(category);
            if !slug.is_empty() {
                keywords.insert(
                    format!("{CATEGORY_PREFIX}{slug}"),
                    serde_json::Value::Bool(true),
                );
            }
        }

        let response: JmapResponse<serde_json::Value> = self
            .api_call(
                auth,
                &[capabilities::CORE.to_owned(), capabilities::MAIL.to_owned()],
                vec![MethodCall::new(
                    "Email/import",
                    serde_json::json!({
                        "accountId": account_id,
                        "emails": {
                            "n1": {
                                "blobId": blob_id,
                                "mailboxIds": { mailbox_id: true },
                                "keywords": keywords,
                                "receivedAt": chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
                            }
                        }
                    }),
                    "0",
                )],
            )
            .await?;

        let mut created = false;
        for method in response.method_responses {
            if method.0 == "Email/import" {
                created = method
                    .1
                    .get("created")
                    .and_then(|c| c.get("n1"))
                    .and_then(|n| n.get("id"))
                    .is_some();
            } else if method.0 == "error" {
                anyhow::bail!("JMAP method error in Email/import for note");
            }
        }
        if !created {
            anyhow::bail!("Email/import for note did not return a created id");
        }

        if let Some(old_email_id) = old_email_id {
            // Best-effort: the new revision already exists and is what
            // matters. Don't fail the whole save over cleanup of the
            // superseded one.
            if let Err(error) = self.destroy_email_by_id(auth, &old_email_id).await {
                tracing::warn!(
                    %error,
                    note_id,
                    old_email_id,
                    "failed to destroy superseded note revision"
                );
            }
        }

        Ok(note_id)
    }

    /// Idempotent: an id that's already gone (raced with something else,
    /// or was already deleted out-of-band) reads as success, not failure --
    /// Stalwart reports that case as `notDestroyed: {"type": "notFound"}`
    /// (confirmed live), which is the outcome the caller actually wanted.
    pub async fn destroy_note(
        &self,
        auth: &AuthenticatedSession,
        stable_id: &str,
    ) -> anyhow::Result<()> {
        let Some(email_id) = self.resolve_note_email_id(auth, stable_id).await? else {
            return Ok(());
        };
        self.destroy_email_by_id(auth, &email_id).await
    }

    async fn destroy_email_by_id(
        &self,
        auth: &AuthenticatedSession,
        email_id: &str,
    ) -> anyhow::Result<()> {
        let Some(account_id) = auth.session.primary_account_for(capabilities::MAIL) else {
            anyhow::bail!("JMAP Mail capability is not available");
        };
        let response: JmapResponse<serde_json::Value> = self
            .api_call(
                auth,
                &[capabilities::CORE.to_owned(), capabilities::MAIL.to_owned()],
                vec![MethodCall::new(
                    "Email/set",
                    serde_json::json!({ "accountId": account_id, "destroy": [email_id] }),
                    "0",
                )],
            )
            .await?;

        for method in response.method_responses {
            if method.0 == "Email/set" {
                let destroyed = method
                    .1
                    .get("destroyed")
                    .and_then(|value| value.as_array())
                    .is_some_and(|list| list.iter().any(|id| id.as_str() == Some(email_id)));
                if destroyed {
                    return Ok(());
                }
                let not_found = method
                    .1
                    .get("notDestroyed")
                    .and_then(|value| value.get(email_id))
                    .and_then(|entry| entry.get("type"))
                    .and_then(|t| t.as_str())
                    == Some("notFound");
                if not_found {
                    return Ok(());
                }
                anyhow::bail!("Email/set destroy did not report success for {email_id}");
            }
        }
        anyhow::bail!("Email/set response was missing")
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MailboxIdentity {
    id: String,
    name: String,
    parent_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NoteEmailObject {
    #[serde(default)]
    subject: String,
    #[serde(default)]
    keywords: BTreeMap<String, bool>,
    #[serde(default)]
    received_at: Option<String>,
    #[serde(default)]
    text_body: Vec<NoteBodyPart>,
    #[serde(default)]
    html_body: Vec<NoteBodyPart>,
    #[serde(default)]
    body_values: BTreeMap<String, NoteBodyValue>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NoteBodyPart {
    part_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct NoteBodyValue {
    #[serde(default)]
    value: String,
}

/// Stalwart reports a single-part HTML-only message's part under BOTH
/// htmlBody and textBody (verified live) -- prefer html explicitly rather
/// than relying on array presence alone, matching the PHP reference fix.
fn select_note_body(email: &NoteEmailObject) -> (EmailBodyType, String) {
    let html = body_value_for(&email.html_body, &email.body_values);
    if let Some(html) = html.clone() {
        if !html.is_empty() {
            return (EmailBodyType::Html, html);
        }
    }
    let text = body_value_for(&email.text_body, &email.body_values).unwrap_or_default();
    if let Some(html) = html {
        if text.is_empty() && !html.is_empty() {
            return (EmailBodyType::Html, html);
        }
    }
    (EmailBodyType::Plain, text)
}

fn body_value_for(
    parts: &[NoteBodyPart],
    values: &BTreeMap<String, NoteBodyValue>,
) -> Option<String> {
    parts
        .iter()
        .find_map(|part| part.part_id.as_ref().and_then(|id| values.get(id)))
        .map(|value| value.value.clone())
}

fn extract_stable_id(keywords: &BTreeMap<String, bool>) -> Option<String> {
    keywords
        .iter()
        .find(|(keyword, present)| **present && keyword.starts_with(STABLE_ID_PREFIX))
        .map(|(keyword, _)| keyword.clone())
}

fn extract_categories(keywords: &BTreeMap<String, bool>) -> Vec<String> {
    keywords
        .iter()
        .filter(|(keyword, present)| **present && keyword.starts_with(CATEGORY_PREFIX))
        .map(|(keyword, _)| unslugify_category(&keyword[CATEGORY_PREFIX.len()..]))
        .collect()
}

fn generate_stable_id() -> String {
    format!("{STABLE_ID_PREFIX}{}", uuid::Uuid::new_v4().simple())
}

fn slugify_category(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut last_was_dash = true; // suppress a leading dash
    for ch in input.trim().to_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            last_was_dash = false;
        } else if !last_was_dash {
            out.push('-');
            last_was_dash = true;
        }
    }
    out.trim_end_matches('-').to_owned()
}

fn unslugify_category(slug: &str) -> String {
    slug.split('-')
        .filter(|word| !word.is_empty())
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// `receivedAt` is included specifically because it's the one field
/// guaranteed to change on every real edit (a fresh Email/import always
/// gets a new one) -- subject/keywords alone would miss a body-only edit,
/// since the body isn't fetched by list_notes().
fn note_hash(email: &NoteEmailObject) -> String {
    let mut keys: Vec<&str> = email
        .keywords
        .iter()
        .filter(|(_, present)| **present)
        .map(|(keyword, _)| keyword.as_str())
        .collect();
    keys.sort_unstable();

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    email.subject.hash(&mut hasher);
    email.received_at.as_deref().unwrap_or("").hash(&mut hasher);
    keys.join(",").hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Real bug, confirmed live: strips a doubled outer `<html>...</html>`
/// wrapper (`<head>`/`<body>` stay singular -- only the outermost tag
/// repeats) before persisting. See the call site in `save_note` for the
/// full story.
fn collapse_duplicate_html_wrapper(body: &str) -> String {
    let mut inner = body;
    let mut stripped_any = false;
    while let Some(rest) = inner.strip_prefix("<html>") {
        inner = rest;
        stripped_any = true;
    }
    while let Some(rest) = inner.strip_suffix("</html>") {
        inner = rest;
    }
    if stripped_any {
        format!("<html>{inner}</html>")
    } else {
        body.to_owned()
    }
}

/// Builds a minimal, parseable RFC822 message representing one note, for
/// Email/import. Header shape (From display name, no To, the
/// X-Uniform-Type-Identifier marker, a real Date and Message-Id) matches
/// what Apple's own IMAP Notes sync (`dataaccessd`) writes for a note on
/// this same account -- confirmed via a raw MIME diff against a real one
/// (see docs/eas-jmap-gap-analysis.md), not guessed from the MS-ASNOTE
/// spec text alone. Before this fix the gateway sent a bare `From:
/// <address>` with no display name (clients showed the note's sender as
/// "Unknown") and a `To:` header real notes never carry.
fn build_note_mime(
    subject: &str,
    body_type: EmailBodyType,
    body: &str,
    account_address: &str,
) -> String {
    let content_type = match body_type {
        EmailBodyType::Html => "text/html; charset=utf-8",
        EmailBodyType::Plain => "text/plain; charset=utf-8",
    };
    let display_name = account_address.split('@').next().unwrap_or(account_address);
    let domain = account_address.split('@').nth(1).unwrap_or("localhost");
    let message_id = format!(
        "<{}@{domain}>",
        uuid::Uuid::new_v4().simple().to_string().to_uppercase()
    );
    let date = chrono::Utc::now().to_rfc2822();
    format!(
        "From: {display_name} <{account_address}>\r\n\
         X-Uniform-Type-Identifier: com.apple.mail-note\r\n\
         MIME-Version: 1.0\r\n\
         Date: {date}\r\n\
         Subject: {}\r\n\
         Message-Id: {message_id}\r\n\
         Content-Type: {content_type}\r\n\r\n{body}",
        encode_mime_header(subject)
    )
}

fn encode_mime_header(value: &str) -> String {
    if value.is_ascii() {
        value.to_owned()
    } else {
        use base64::{engine::general_purpose::STANDARD, Engine};
        format!("=?UTF-8?B?{}?=", STANDARD.encode(value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_round_trips_simple_names() {
        assert_eq!(slugify_category("Work"), "work");
        assert_eq!(unslugify_category("work"), "Work");
        assert_eq!(slugify_category("Follow Up"), "follow-up");
        assert_eq!(unslugify_category("follow-up"), "Follow Up");
    }

    #[test]
    fn stable_ids_are_extracted_by_prefix_only() {
        let mut keywords = BTreeMap::new();
        keywords.insert("$note".to_owned(), true);
        keywords.insert("$seen".to_owned(), true);
        keywords.insert("noteid-abc123".to_owned(), true);
        keywords.insert("notecat-work".to_owned(), true);
        assert_eq!(
            extract_stable_id(&keywords),
            Some("noteid-abc123".to_owned())
        );
        assert_eq!(extract_categories(&keywords), vec!["Work".to_owned()]);
    }

    #[test]
    fn mime_builder_selects_content_type_from_body_type() {
        let mime = build_note_mime(
            "hello",
            EmailBodyType::Html,
            "<b>hi</b>",
            "user@example.com",
        );
        assert!(mime.contains("Content-Type: text/html"));
        assert!(mime.contains("Subject: hello"));
        assert!(mime.ends_with("<b>hi</b>"));
    }

    #[test]
    fn mime_builder_matches_real_apple_notes_header_shape() {
        // Confirmed live against a raw MIME diff of a real Apple IMAP
        // Notes-created message on the same account: display-name From,
        // no To, the UTI marker, and a real Message-Id.
        let mime = build_note_mime("hello", EmailBodyType::Html, "<b>hi</b>", "khuong@khuo.ng");
        assert!(mime.contains("From: khuong <khuong@khuo.ng>"));
        assert!(!mime.contains("To:"));
        assert!(mime.contains("X-Uniform-Type-Identifier: com.apple.mail-note"));
        assert!(mime.contains("Message-Id: <"));
        assert!(mime.contains("@khuo.ng>"));
        assert!(mime.contains("Date: "));
    }

    #[test]
    fn collapse_duplicate_html_wrapper_removes_only_the_repeated_outer_tag() {
        let doubled = r#"<html><html><head></head><body>Notes<div>Test edit one</div></body></html></html>"#;
        assert_eq!(
            collapse_duplicate_html_wrapper(doubled),
            "<html><head></head><body>Notes<div>Test edit one</div></body></html>"
        );
    }

    #[test]
    fn collapse_duplicate_html_wrapper_leaves_a_single_wrap_untouched() {
        let single = "<html><head></head><body>Notes</body></html>";
        assert_eq!(collapse_duplicate_html_wrapper(single), single);
    }

    #[test]
    fn collapse_duplicate_html_wrapper_leaves_an_unwrapped_fragment_untouched() {
        assert_eq!(collapse_duplicate_html_wrapper("just text"), "just text");
    }
}
