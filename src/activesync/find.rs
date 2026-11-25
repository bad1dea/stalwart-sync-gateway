mod query;

use super::{eas_datetime_dashes, EasQuery, ProtocolVersion};
use crate::{
    http_server::AppState,
    jmap::client::AuthenticatedSession,
    model::CollectionKind,
    state::ItemState,
    wbxml::{
        self,
        eas::{
            self, airsync as air, email as mail, email2, find as f, gal, item_operations as ops,
            DocumentBuilder,
        },
    },
};
use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::io::Read;

struct Request {
    search_id: String,
    gal: bool,
    text: String,
    folder: Option<String>,
    deep: bool,
    start: usize,
    end: usize,
}

fn child(document: &wbxml::Document, token: wbxml::token::Token) -> Option<wbxml::Document> {
    eas::child_elements(document).into_iter().find(|doc| matches!(doc.nodes.first(), Some(wbxml::Node::Start(t)) if t.code_page == token.code_page && t.token == token.token))
}

fn parse(document: &wbxml::Document) -> Result<Request, &'static str> {
    let search_id = eas::find_text_after(document, f::SEARCH_ID).ok_or("missing SearchId")?;
    if search_id.len() != 36 || uuid::Uuid::parse_str(search_id).is_err() {
        return Err("invalid SearchId");
    }
    let execute = child(document, f::EXECUTE_SEARCH).ok_or("missing ExecuteSearch")?;
    let criteria = eas::child_elements(&execute);
    if criteria.len() != 1 {
        return Err("expected one search criterion");
    }
    let (criteria, gal) = if let Some(criteria) = child(&execute, f::MAILBOX_SEARCH_CRITERION) {
        (criteria, false)
    } else if let Some(criteria) = child(&execute, f::GAL_SEARCH_CRITERION) {
        (criteria, true)
    } else {
        return Err("unsupported search criterion");
    };
    let query = child(&criteria, f::QUERY).ok_or("missing Query")?;
    let text = if gal {
        let text = eas::find_text_after(&query, f::QUERY)
            .ok_or("missing Query")?
            .trim();
        if !(4..=256).contains(&text.chars().count()) {
            return Err("invalid GAL Query");
        }
        text
    } else {
        if eas::find_text_after(&query, air::CLASS) != Some("Email") {
            return Err("unsupported search class");
        }
        eas::find_text_after(&query, f::FREE_TEXT)
            .filter(|text| !text.trim().is_empty())
            .ok_or("missing FreeText")?
    };
    let options = child(&criteria, f::OPTIONS);
    let (start, end) = if let Some(options) = &options {
        let range = eas::find_text_after(options, f::RANGE).ok_or("missing Range")?;
        let (start, end) = range.split_once('-').ok_or("invalid Range")?;
        let start = start.parse::<usize>().map_err(|_| "invalid Range")?;
        let end = end.parse::<usize>().map_err(|_| "invalid Range")?;
        if end < start || end >= 1000 {
            return Err("invalid Range");
        }
        (start, end)
    } else {
        (0, 99)
    };
    Ok(Request {
        search_id: search_id.to_ascii_lowercase(),
        gal,
        text: text.to_owned(),
        folder: eas::find_text_after(&query, air::COLLECTION_ID).map(str::to_owned),
        deep: options
            .as_ref()
            .is_some_and(|doc| eas::contains_token(doc, f::DEEP_TRAVERSAL)),
        start,
        end,
    })
}

#[derive(Serialize, Deserialize)]
struct SearchState {
    filter: Value,
    search_text: String,
    query_state: String,
    created: i64,
}

fn same_search(session: &SearchState, filter: &Value, search_text: &str) -> bool {
    session.filter == *filter && session.search_text == search_text
}
const SEARCHES: &str = "__find__";
const MAX_FIND_ATTACHMENT_TEXT_BYTES: u64 = 8 * 1024 * 1024;
const MAX_FIND_ATTACHMENT_DOWNLOAD_BYTES: u64 = 16 * 1024 * 1024;
const MAX_FIND_PDF_OCR_PAGES: u32 = 3;
const MAX_FIND_PDF_OCR_EDGE: u32 = 1600;
const MAX_FIND_ARCHIVE_ENTRIES: usize = 4096;
const MAX_FIND_ARCHIVE_EXPANDED_BYTES: usize = 8 * 1024 * 1024;
static FIND_PDF_OCR_SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

pub(super) async fn handle(
    state: &AppState,
    auth: &AuthenticatedSession,
    device: &EasQuery,
    document: &wbxml::Document,
    version: ProtocolVersion,
) -> Response {
    if version != ProtocolVersion::V16_1 {
        return (StatusCode::BAD_REQUEST, "Find requires EAS 16.1\n").into_response();
    }
    let request = match parse(document) {
        Ok(request) => request,
        Err(_) => return error_response(state, "2"),
    };
    let reply = if request.gal {
        execute_gal(state, auth, device, request).await
    } else {
        let phrase_filter = query::phrases_for_postfilter(&request.text);
        let filter = match query::to_filter(&request.text) {
            Ok(filter) => filter,
            Err(_) => return error_response(state, "2"),
        };
        execute(
            state,
            auth,
            device,
            request,
            filter,
            phrase_filter.unwrap_or((query::PhraseMatch::All, Vec::new())),
        )
        .await
    };
    match reply {
        Ok(reply) => respond(state, reply),
        Err(error) => {
            tracing::warn!(%error, "JMAP-backed Find failed");
            error_response(state, "110")
        }
    }
}

async fn execute(
    state: &AppState,
    auth: &AuthenticatedSession,
    device: &EasQuery,
    request: Request,
    query: Value,
    phrase_filter: (query::PhraseMatch, Vec<(String, String)>),
) -> anyhow::Result<wbxml::Document> {
    let collections = state.jmap.collections(auth).await?;
    let mailboxes: Vec<_> = collections
        .iter()
        .filter(|folder| folder.kind == CollectionKind::Mail)
        .collect();
    let mut scope: BTreeSet<String> = if let Some(folder) = &request.folder {
        if !mailboxes.iter().any(|item| item.id == *folder) {
            return Ok(error_document("3"));
        }
        [folder.clone()].into_iter().collect()
    } else {
        mailboxes.iter().map(|item| item.id.clone()).collect()
    };
    if request.deep {
        loop {
            let old = scope.len();
            for folder in &mailboxes {
                if folder
                    .parent_id
                    .as_ref()
                    .is_some_and(|parent| scope.contains(parent))
                {
                    scope.insert(folder.id.clone());
                }
            }
            if old == scope.len() {
                break;
            }
        }
    }
    let folder_filter = if scope.is_empty() {
        json!({"operator":"NOT", "conditions":[{}]})
    } else {
        json!({"operator":"OR", "conditions":scope.iter().map(|id| json!({"inMailbox":id})).collect::<Vec<_>>()})
    };
    let (phrase_match, phrases) = phrase_filter;
    let search_attachment_text = matches!(
        phrase_match,
        query::PhraseMatch::Near { .. } | query::PhraseMatch::Onear { .. }
    ) || phrases.iter().any(|(property, _)| {
        matches!(property.as_str(), "text" | "$not$text")
            || property.starts_with("$proximity-")
            || property.starts_with("$op$near:")
            || property.starts_with("$op$onear:")
    });
    // A negated phrase cannot use JMAP's term filter as its candidate set:
    // the backend does not preserve phrase semantics, and NOT would exclude
    // messages that do not contain all indexed phrase terms. Boolean terms,
    // including negated ones, remain exact JMAP predicates, so retaining the
    // query preserves a smaller safe candidate set for positive phrase forms.
    let query = if phrase_match == query::PhraseMatch::None
        || search_attachment_text
        || matches!(
            phrase_match,
            query::PhraseMatch::Near { .. } | query::PhraseMatch::Onear { .. }
        )
        || phrases.iter().any(|(property, _)| {
            property.starts_with("$op$near:") || property.starts_with("$op$onear:")
        })
        || phrases.iter().any(|(property, _)| {
            matches!(
                property.as_str(),
                "$not$subject" | "$not$body" | "$not$text"
            )
        }) {
        json!({})
    } else {
        query
    };
    let filter = json!({"operator":"AND", "conditions":[query, folder_filter]});
    let user = auth.username();
    let device = device.device_id.as_deref().unwrap_or("unknown");
    let now = chrono::Utc::now().timestamp();
    let mut sessions = state.state.item_states(user, device, SEARCHES).await?;
    sessions.retain(|entry| {
        serde_json::from_str::<SearchState>(&entry.hash)
            .is_ok_and(|session| now - session.created < 900)
    });
    let previous = sessions
        .iter()
        .find(|item| item.item_id == request.search_id)
        .and_then(|item| serde_json::from_str::<SearchState>(&item.hash).ok());
    if request.start != 0
        && previous
            .as_ref()
            .is_none_or(|session| !same_search(session, &filter, &request.text))
    {
        return Ok(restart_document());
    }
    let mut properties = vec![
        "id",
        "mailboxIds",
        "keywords",
        "subject",
        "receivedAt",
        "sentAt",
        "size",
        "from",
        "to",
        "cc",
        "bcc",
        "preview",
        "attachments",
        "header:Importance:asText",
    ];
    if phrases.iter().any(|(property, _)| {
        matches!(property.as_str(), "body" | "text") || property.starts_with("$proximity-")
    }) {
        properties.extend(["textBody", "htmlBody", "bodyValues"]);
    }
    let page = if !phrases.is_empty() {
        let mut position = 0usize;
        let mut matched = 0usize;
        let mut selected = Vec::new();
        let mut query_state = None;
        let mut candidate_total = None;
        let matches_exactly = |email: &crate::model::Email| {
            email_matches_phrases_with_addresses_and_attachments(
                &email.subject,
                email.body.as_ref(),
                &email.from,
                &email.to,
                &email.cc,
                &email.bcc,
                email.received_at.as_deref(),
                email.sent_at.as_deref(),
                email.size,
                email.importance,
                Some(email.read),
                Some(email.keywords.iter().any(|keyword| keyword == "$flagged")),
                Some(!email.attachments.is_empty()),
                &email.attachments,
                phrase_match,
                &phrases,
            )
        };
        loop {
            let candidate = state
                .jmap
                .query_emails(auth, filter.clone(), position as i64, 100, &properties)
                .await?;
            if let Some(total) = candidate_total {
                anyhow::ensure!(
                    candidate.total == total,
                    "Find candidate total changed while filtering exact phrase"
                );
            } else {
                candidate_total = Some(candidate.total);
            }
            if let Some(state) = &query_state {
                anyhow::ensure!(
                    candidate.query_state.as_ref() == Some(state),
                    "Find query changed while filtering exact phrase"
                );
            } else {
                query_state = candidate.query_state.clone();
            }
            let count = candidate.emails.len();
            for mut email in candidate.emails {
                let mut is_match = matches_exactly(&email)?;
                if !is_match && search_attachment_text {
                    for attachment in &mut email.attachments {
                        if attachment.extracted_text.is_some()
                            || attachment.size > MAX_FIND_ATTACHMENT_DOWNLOAD_BYTES
                            || !is_extractable_attachment_type(&attachment.content_type)
                        {
                            continue;
                        }
                        let Some((bytes, downloaded_type)) = state
                            .jmap
                            .download_blob_with_type_limited(
                                auth,
                                &attachment.blob_id,
                                &attachment.name,
                                MAX_FIND_ATTACHMENT_DOWNLOAD_BYTES as usize,
                            )
                            .await?
                        else {
                            continue;
                        };
                        let content_type = if downloaded_type == "application/octet-stream" {
                            &attachment.content_type
                        } else {
                            &downloaded_type
                        };
                        if content_type.split(';').next().is_some_and(|media_type| {
                            media_type.trim().eq_ignore_ascii_case("application/pdf")
                        }) {
                            attachment.extracted_text =
                                extract_find_pdf_text_async(content_type.to_owned(), bytes).await;
                        } else {
                            attachment.extracted_text =
                                extract_find_attachment_text(content_type, &bytes);
                        }
                    }
                    is_match = matches_exactly(&email)?;
                }
                if is_match {
                    if (request.start..=request.end).contains(&matched) {
                        selected.push(email);
                    }
                    matched += 1;
                }
            }
            position += count;
            if count == 0 || position as u64 >= candidate_total.unwrap_or_default() {
                break;
            }
        }
        crate::jmap::client::EmailSearchPage {
            emails: selected,
            total: matched as u64,
            query_state,
        }
    } else {
        state
            .jmap
            .query_emails(
                auth,
                filter.clone(),
                request.start as i64,
                request.end - request.start + 1,
                &properties,
            )
            .await?
    };
    let query_state = page
        .query_state
        .ok_or_else(|| anyhow::anyhow!("Email/query omitted queryState"))?;
    if request.start != 0
        && previous
            .as_ref()
            .is_some_and(|session| session.query_state != query_state)
    {
        return Ok(restart_document());
    }
    if request.start == 0 {
        sessions.retain(|item| item.item_id != request.search_id);
        if sessions.len() >= 16 {
            sessions.remove(0);
        }
        sessions.push(ItemState {
            item_id: request.search_id,
            hash: serde_json::to_string(&SearchState {
                filter,
                search_text: request.text.clone(),
                query_state,
                created: now,
            })?,
        });
        state
            .state
            .put_item_states(user, device, SEARCHES, sessions)
            .await?;
    }
    let mut b = DocumentBuilder::new();
    b.start(f::FIND);
    b.leaf(f::STATUS, "1");
    b.start(f::RESPONSE);
    b.leaf(ops::STORE, "Mailbox");
    b.leaf(f::STATUS, "1");
    let count = page.emails.len();
    for email in page.emails {
        let folder = email
            .mailbox_ids
            .iter()
            .find(|id| scope.contains(*id))
            .ok_or_else(|| anyhow::anyhow!("Find result outside requested mailbox scope"))?;
        b.start(f::RESULT);
        b.leaf(air::CLASS, "Email");
        b.leaf(air::SERVER_ID, &email.id);
        b.leaf(air::COLLECTION_ID, folder);
        b.start(f::PROPERTIES);
        b.leaf(mail::SUBJECT, email.subject);
        if let Some(date) = email.received_at {
            b.leaf(mail::DATE_RECEIVED, eas_datetime_dashes(&date));
        }
        if !email.to.is_empty() {
            b.leaf(mail::DISPLAY_TO, email.to);
        }
        if !email.cc.is_empty() {
            b.leaf(f::DISPLAY_CC, email.cc);
        }
        if !email.bcc.is_empty() {
            b.leaf(f::DISPLAY_BCC, email.bcc);
        }
        if !email.from.is_empty() {
            b.leaf(mail::FROM, email.from);
        }
        b.leaf(mail::READ, if email.read { "1" } else { "0" });
        b.leaf(mail::IMPORTANCE, email.importance.to_string());
        b.leaf(
            email2::IS_DRAFT,
            if email.keywords.iter().any(|keyword| keyword == "$draft") {
                "1"
            } else {
                "0"
            },
        );
        b.leaf(
            f::HAS_ATTACHMENTS,
            if email.attachments.is_empty() {
                "0"
            } else {
                "1"
            },
        );
        if let Some(preview) = email.preview {
            b.leaf(f::PREVIEW, preview);
        }
        b.end();
        b.end();
    }
    if count != 0 {
        b.leaf(
            f::RANGE,
            format!("{}-{}", request.start, request.start + count - 1),
        );
    }
    b.leaf(f::TOTAL, page.total.to_string());
    b.end();
    b.end();
    Ok(b.finish())
}

async fn extract_find_pdf_text_async(content_type: String, bytes: Vec<u8>) -> Option<String> {
    let permit = FIND_PDF_OCR_SLOTS.acquire().await.ok()?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        extract_find_attachment_text_inner(&content_type, &bytes, true)
    })
    .await
    .ok()?
}

fn is_extractable_attachment_type(content_type: &str) -> bool {
    let media_type = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    media_type.starts_with("text/")
        || media_type.ends_with("+xml")
        || media_type.ends_with("+json")
        || is_textual_application_type(&media_type)
        || matches!(
            media_type.as_str(),
            "application/rtf"
                | "application/json"
                | "application/xml"
                | "application/pdf"
                | "application/vnd.ms-outlook"
                | "application/vnd.ms-excel"
                | "application/vnd.ms-powerpoint"
                | "application/msword"
                | "message/rfc822"
                | "message/global"
        )
        || is_extractable_archive_type(&media_type)
}

fn is_textual_application_type(media_type: &str) -> bool {
    matches!(
        media_type,
        "application/csv"
            | "application/ndjson"
            | "application/x-ndjson"
            | "application/yaml"
            | "application/x-yaml"
            | "application/toml"
    )
}

fn is_extractable_archive_type(media_type: &str) -> bool {
    matches!(
        media_type,
        "application/epub+zip"
            | "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
            | "application/vnd.openxmlformats-officedocument.wordprocessingml.template"
            | "application/vnd.ms-word.document.macroenabled.12"
            | "application/vnd.ms-word.template.macroenabled.12"
            | "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
            | "application/vnd.openxmlformats-officedocument.spreadsheetml.template"
            | "application/vnd.ms-excel.sheet.macroenabled.12"
            | "application/vnd.ms-excel.template.macroenabled.12"
            | "application/vnd.openxmlformats-officedocument.presentationml.presentation"
            | "application/vnd.openxmlformats-officedocument.presentationml.slideshow"
            | "application/vnd.openxmlformats-officedocument.presentationml.template"
            | "application/vnd.ms-powerpoint.presentation.macroenabled.12"
            | "application/vnd.ms-powerpoint.slideshow.macroenabled.12"
            | "application/vnd.ms-powerpoint.template.macroenabled.12"
            | "application/zip"
            | "application/x-zip-compressed"
            | "application/vnd.oasis.opendocument.text"
            | "application/vnd.oasis.opendocument.spreadsheet"
            | "application/vnd.oasis.opendocument.presentation"
    )
}

fn extract_find_attachment_text(content_type: &str, bytes: &[u8]) -> Option<String> {
    extract_find_attachment_text_inner(content_type, bytes, true)
}

fn extract_find_attachment_text_inner(
    content_type: &str,
    bytes: &[u8],
    allow_pdf_ocr: bool,
) -> Option<String> {
    let media_type = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let text = match media_type.as_str() {
        "text/plain" => decode_find_attachment_text(content_type, bytes)?,
        "text/html" => super::strip_html_tags(
            &decode_find_attachment_text(content_type, bytes)?.replace('>', "> "),
        ),
        "text/rtf" | "application/rtf" => {
            super::rtf_to_plain_text(&decode_find_attachment_text(content_type, bytes)?)
        }
        "application/pdf" => {
            // Attachments are untrusted input. pdf-extract can panic on malformed
            // object graphs, so contain parser panics and treat malformed PDFs as
            // non-extractable rather than failing the whole Find request.
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                extract_bounded_pdf_text_with_ocr(bytes, allow_pdf_ocr)
            }))
            .ok()??
        }
        "application/vnd.ms-excel" => {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                extract_legacy_xls_text(bytes)
            }))
            .ok()??
        }
        "application/vnd.ms-powerpoint" => {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                extract_legacy_ppt_text(bytes)
            }))
            .ok()??
        }
        "application/msword" => std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            extract_legacy_doc_text(bytes)
        }))
        .ok()??,
        "message/rfc822" | "message/global" => {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| extract_rfc822_text(bytes)))
                .ok()??
        }
        "application/vnd.ms-outlook" => {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                extract_outlook_msg_text(bytes)
            }))
            .ok()??
        }
        _ if is_extractable_archive_type(&media_type) => {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                extract_archive_text(bytes, &media_type)
            }))
            .ok()??
        }
        "application/xml" | "text/xml" => extract_xml_text(bytes)?,
        _ if media_type.ends_with("+xml") => extract_xml_text(bytes)?,
        _ if media_type.starts_with("text/")
            || media_type == "application/json"
            || media_type.ends_with("+json")
            || is_textual_application_type(&media_type) =>
        {
            decode_find_attachment_text(content_type, bytes)?
        }
        _ => return None,
    };
    normalize_find_attachment_text(&text)
}

fn normalize_find_attachment_text(text: &str) -> Option<String> {
    let mut normalized = String::new();
    for word in text.split_whitespace() {
        let separator = usize::from(!normalized.is_empty());
        if normalized
            .len()
            .saturating_add(separator)
            .saturating_add(word.len())
            > MAX_FIND_ATTACHMENT_TEXT_BYTES as usize
        {
            return None;
        }
        if separator != 0 {
            normalized.push(' ');
        }
        normalized.push_str(word);
    }
    Some(normalized)
}

fn decode_find_attachment_text(content_type: &str, bytes: &[u8]) -> Option<String> {
    let charset = content_type.split(';').skip(1).find_map(|parameter| {
        let (name, value) = parameter.split_once('=')?;
        name.trim()
            .eq_ignore_ascii_case("charset")
            .then(|| value.trim().trim_matches(['"', '\'']))
    });
    let (encoding, payload) = charset
        .and_then(|label| encoding_rs::Encoding::for_label(label.as_bytes()))
        .map(|encoding| (encoding, bytes))
        .or_else(|| {
            encoding_rs::Encoding::for_bom(bytes)
                .map(|(encoding, bom_len)| (encoding, &bytes[bom_len..]))
        })
        .unwrap_or((encoding_rs::UTF_8, bytes));
    let (decoded, _) = encoding.decode_without_bom_handling(payload);
    (decoded.len() <= MAX_FIND_ATTACHMENT_TEXT_BYTES as usize).then(|| decoded.into_owned())
}

fn extract_rfc822_text(bytes: &[u8]) -> Option<String> {
    let message = mail_parser::MessageParser::default().parse(bytes)?;
    let mut output = String::new();
    for part in message.messages() {
        if let Some(subject) = part.subject() {
            append_find_text(&mut output, subject)?;
        }
        for body in part.text_bodies() {
            append_find_text(&mut output, &body)?;
        }
        for body in part.html_body() {
            if let Some(html) = body.text() {
                append_find_text(&mut output, &mail_parser::html_to_text(&html))?;
            }
        }
    }
    Some(output)
}

fn extract_outlook_msg_text(bytes: &[u8]) -> Option<String> {
    let message = msg_parser::Outlook::from_slice(bytes).ok()?;
    let mut output = String::new();
    append_find_text(&mut output, &message.subject)?;
    append_find_text(&mut output, &message.body)?;
    if !message.html.is_empty() {
        append_find_text(&mut output, &mail_parser::html_to_text(&message.html))?;
    }
    Some(output)
}

fn append_find_text(output: &mut String, text: &str) -> Option<()> {
    let separator = usize::from(!output.is_empty());
    if output
        .len()
        .saturating_add(separator)
        .saturating_add(text.len())
        > MAX_FIND_ATTACHMENT_TEXT_BYTES as usize
    {
        return None;
    }
    if separator != 0 {
        output.push(' ');
    }
    output.push_str(text);
    Some(())
}

fn extract_archive_text(bytes: &[u8], media_type: &str) -> Option<String> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).ok()?;
    if archive.len() > MAX_FIND_ARCHIVE_ENTRIES {
        return None;
    }
    let wanted = |name: &str| match media_type {
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
        | "application/vnd.openxmlformats-officedocument.wordprocessingml.template"
        | "application/vnd.ms-word.document.macroenabled.12"
        | "application/vnd.ms-word.template.macroenabled.12" => {
            name == "word/document.xml"
                || name == "word/footnotes.xml"
                || name == "word/endnotes.xml"
                || name == "word/comments.xml"
                || name == "word/glossary/document.xml"
                || (name.starts_with("word/header") && name.ends_with(".xml"))
                || (name.starts_with("word/footer") && name.ends_with(".xml"))
        }
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
        | "application/vnd.openxmlformats-officedocument.spreadsheetml.template"
        | "application/vnd.ms-excel.sheet.macroenabled.12"
        | "application/vnd.ms-excel.template.macroenabled.12" => {
            name == "xl/sharedStrings.xml"
                || (name.starts_with("xl/worksheets/") && name.ends_with(".xml"))
                || (name.starts_with("xl/comments") && name.ends_with(".xml"))
                || (name.starts_with("xl/threadedComments/") && name.ends_with(".xml"))
        }
        "application/vnd.openxmlformats-officedocument.presentationml.presentation"
        | "application/vnd.openxmlformats-officedocument.presentationml.slideshow"
        | "application/vnd.openxmlformats-officedocument.presentationml.template"
        | "application/vnd.ms-powerpoint.presentation.macroenabled.12"
        | "application/vnd.ms-powerpoint.slideshow.macroenabled.12"
        | "application/vnd.ms-powerpoint.template.macroenabled.12" => {
            (name.starts_with("ppt/slides/slide")
                || name.starts_with("ppt/notesSlides/notesSlide")
                || name.starts_with("ppt/comments/comment"))
                && name.ends_with(".xml")
        }
        "application/vnd.oasis.opendocument.text"
        | "application/vnd.oasis.opendocument.spreadsheet"
        | "application/vnd.oasis.opendocument.presentation" => name == "content.xml",
        "application/epub+zip" => name.to_ascii_lowercase().ends_with(".xhtml"),
        "application/zip" | "application/x-zip-compressed" => {
            let extension = name.rsplit_once('.').map(|(_, extension)| extension);
            extension.is_some_and(|extension| {
                matches!(
                    extension.to_ascii_lowercase().as_str(),
                    "txt"
                        | "md"
                        | "csv"
                        | "json"
                        | "xml"
                        | "html"
                        | "htm"
                        | "yaml"
                        | "yml"
                        | "toml"
                        | "sql"
                        | "log"
                        | "ini"
                        | "cfg"
                        | "conf"
                        | "properties"
                        | "rtf"
                )
            })
        }
        _ => false,
    };
    let mut remaining = MAX_FIND_ARCHIVE_EXPANDED_BYTES;
    let mut output = String::new();
    let mut found = false;
    for index in 0..archive.len() {
        let mut file = archive.by_index(index).ok()?;
        if !wanted(file.name()) {
            continue;
        }
        let name = file.name().to_ascii_lowercase();
        found = true;
        if file.size() > remaining as u64 {
            return None;
        }
        let mut xml = Vec::with_capacity(file.size() as usize);
        file.by_ref()
            .take(remaining as u64 + 1)
            .read_to_end(&mut xml)
            .ok()?;
        if xml.len() > remaining {
            return None;
        }
        remaining -= xml.len();
        let text = if matches!(
            media_type,
            "application/zip" | "application/x-zip-compressed"
        ) {
            if name.ends_with(".xml") {
                extract_xml_text(&xml)?
            } else if name.ends_with(".html") || name.ends_with(".htm") {
                super::strip_html_tags(&String::from_utf8_lossy(&xml))
            } else if name.ends_with(".rtf") {
                super::rtf_to_plain_text(&String::from_utf8_lossy(&xml))
            } else {
                decode_find_attachment_text("text/plain; charset=utf-8", &xml)?
            }
        } else {
            extract_xml_text(&xml)?
        };
        let separator_bytes = usize::from(!output.is_empty());
        if output
            .len()
            .saturating_add(separator_bytes)
            .saturating_add(text.len())
            > MAX_FIND_ATTACHMENT_TEXT_BYTES as usize
        {
            return None;
        }
        if separator_bytes != 0 {
            output.push(' ');
        }
        output.push_str(&text);
    }
    found.then_some(output)
}

fn extract_xml_text(bytes: &[u8]) -> Option<String> {
    use quick_xml::events::Event;

    let mut reader = quick_xml::Reader::from_reader(bytes);
    reader.config_mut().trim_text(false);
    let mut buffer = Vec::new();
    let mut text = String::new();
    let mut event_count = 0usize;
    loop {
        let event = reader.read_event_into(&mut buffer).ok()?;
        event_count += 1;
        if event_count > 250_000 {
            return None;
        }
        match event {
            Event::Start(_) | Event::End(_) | Event::Empty(_) => {
                append_find_extracted_text(&mut text, " ")?
            }
            Event::Text(value) => {
                let decoded = value.unescape().ok()?;
                append_find_extracted_text(&mut text, &decoded)?;
            }
            Event::CData(value) => {
                let decoded = reader.decoder().decode(value.as_ref()).ok()?;
                append_find_extracted_text(&mut text, &decoded)?;
            }
            Event::DocType(_) => return None,
            Event::Eof => break,
            _ => {}
        }
        buffer.clear();
    }
    (text.len() <= MAX_FIND_ATTACHMENT_TEXT_BYTES as usize).then_some(text)
}

fn append_find_extracted_text(output: &mut String, text: &str) -> Option<()> {
    if output.len().saturating_add(text.len()) > MAX_FIND_ATTACHMENT_TEXT_BYTES as usize {
        return None;
    }
    output.push_str(text);
    Some(())
}

fn extract_bounded_pdf_text_with_ocr(bytes: &[u8], allow_ocr: bool) -> Option<String> {
    extract_bounded_pdf_text_with_tools(
        bytes,
        allow_ocr,
        std::path::Path::new("pdftoppm"),
        std::path::Path::new("tesseract"),
    )
}

struct BoundedPdfOutput {
    text: String,
}

impl pdf_extract::OutputDev for BoundedPdfOutput {
    fn begin_page(
        &mut self,
        _page_num: u32,
        _media_box: &pdf_extract::MediaBox,
        _art_box: Option<(f64, f64, f64, f64)>,
    ) -> Result<(), pdf_extract::OutputError> {
        Ok(())
    }

    fn end_page(&mut self) -> Result<(), pdf_extract::OutputError> {
        self.push(" ")
    }

    fn output_character(
        &mut self,
        _trm: &pdf_extract::Transform,
        _width: f64,
        _spacing: f64,
        _font_size: f64,
        character: &str,
    ) -> Result<(), pdf_extract::OutputError> {
        self.push(character)
    }

    fn begin_word(&mut self) -> Result<(), pdf_extract::OutputError> {
        if !self.text.is_empty() && !self.text.chars().last().is_some_and(char::is_whitespace) {
            self.push(" ")?;
        }
        Ok(())
    }

    fn end_word(&mut self) -> Result<(), pdf_extract::OutputError> {
        Ok(())
    }

    fn end_line(&mut self) -> Result<(), pdf_extract::OutputError> {
        self.push(" ")
    }
}

impl BoundedPdfOutput {
    fn push(&mut self, value: &str) -> Result<(), pdf_extract::OutputError> {
        if self.text.len().saturating_add(value.len()) > MAX_FIND_ATTACHMENT_TEXT_BYTES as usize {
            return Err(pdf_extract::OutputError::FormatError(std::fmt::Error));
        }
        self.text.push_str(value);
        Ok(())
    }
}

fn extract_bounded_pdf_text_with_tools(
    bytes: &[u8],
    allow_ocr: bool,
    pdftoppm: &std::path::Path,
    tesseract: &std::path::Path,
) -> Option<String> {
    let document = pdf_extract::Document::load_mem(bytes).ok()?;
    if document.is_encrypted() {
        return None;
    }
    let pages = document.get_pages();
    if pages.is_empty() {
        return None;
    }
    let mut page_text = Vec::with_capacity(pages.len());
    let mut extracted_bytes = 0usize;
    for page_num in pages.keys().copied() {
        let mut output = BoundedPdfOutput {
            text: String::new(),
        };
        pdf_extract::output_doc_page(&document, &mut output, page_num).ok()?;
        extracted_bytes = extracted_bytes.saturating_add(output.text.len());
        if extracted_bytes > MAX_FIND_ATTACHMENT_TEXT_BYTES as usize {
            return None;
        }
        page_text.push((page_num, output.text));
    }

    let missing_text_pages: Vec<_> = page_text
        .iter()
        .filter_map(|(page, text)| text.trim().is_empty().then_some(*page))
        .collect();
    if !missing_text_pages.is_empty() && allow_ocr {
        if missing_text_pages.len() > MAX_FIND_PDF_OCR_PAGES as usize {
            // A textless page may contain an image. Refuse to return partial
            // text when more than three pages need OCR.
            return None;
        }
        let recognized =
            extract_pdf_pages_ocr_with_tools(bytes, &missing_text_pages, pdftoppm, tesseract)?;
        let recognized: std::collections::HashMap<_, _> = recognized.into_iter().collect();
        for (page, text) in &mut page_text {
            if text.trim().is_empty() {
                *text = recognized.get(page)?.clone();
            }
        }
    }

    let mut output = String::new();
    for (_, page) in page_text {
        append_find_extracted_text(&mut output, &page)?;
        append_find_extracted_text(&mut output, " ")?;
    }
    Some(output)
}

struct FindOcrTempDir(std::path::PathBuf);

impl FindOcrTempDir {
    fn create() -> Option<Self> {
        use std::os::unix::fs::DirBuilderExt;

        let path = std::env::temp_dir().join(format!("stalwart-find-ocr-{}", uuid::Uuid::new_v4()));
        let mut builder = std::fs::DirBuilder::new();
        builder.mode(0o700).create(&path).ok()?;
        Some(Self(path))
    }
}

impl Drop for FindOcrTempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn run_find_ocr_command(command: &mut std::process::Command, timeout: std::time::Duration) -> bool {
    use std::process::Stdio;
    use std::time::Instant;

    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let Ok(mut child) = command.spawn() else {
        return false;
    };
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if started.elapsed() < timeout => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Ok(None) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

fn extract_pdf_pages_ocr_with_tools(
    bytes: &[u8],
    pages: &[u32],
    pdftoppm: &std::path::Path,
    tesseract: &std::path::Path,
) -> Option<Vec<(u32, String)>> {
    use std::time::Duration;

    if bytes.len() as u64 > MAX_FIND_ATTACHMENT_DOWNLOAD_BYTES
        || pages.is_empty()
        || pages.len() > MAX_FIND_PDF_OCR_PAGES as usize
        || pages.contains(&0)
    {
        return None;
    }
    let temp = FindOcrTempDir::create()?;
    let input = temp.0.join("input.pdf");
    std::fs::write(&input, bytes).ok()?;

    let mut output = Vec::with_capacity(pages.len());
    let mut extracted_bytes = 0usize;
    for page in pages {
        let prefix = temp.0.join("page");
        if !run_find_ocr_command(
            std::process::Command::new(pdftoppm)
                .arg("-f")
                .arg(page.to_string())
                .arg("-l")
                .arg(page.to_string())
                .arg("-scale-to")
                .arg(MAX_FIND_PDF_OCR_EDGE.to_string())
                .arg("-gray")
                .arg("-png")
                .arg("-q")
                .arg(&input)
                .arg(&prefix),
            Duration::from_secs(2),
        ) {
            return None;
        }
        let image = temp.0.join(format!("page-{page}.png"));
        if !image.is_file() {
            return None;
        }
        let output_base = temp.0.join(format!("recognized-{page}"));
        if !run_find_ocr_command(
            std::process::Command::new(tesseract)
                .arg(&image)
                .arg(&output_base)
                .arg("-l")
                .arg("eng")
                .arg("--psm")
                .arg("3"),
            Duration::from_secs(4),
        ) {
            return None;
        }
        let text_path = output_base.with_extension("txt");
        let size = std::fs::metadata(&text_path).ok()?.len();
        extracted_bytes = extracted_bytes.saturating_add(size as usize);
        if extracted_bytes > MAX_FIND_ATTACHMENT_TEXT_BYTES as usize {
            return None;
        }
        let text = std::fs::read_to_string(text_path).ok()?;
        output.push((*page, text));
    }
    Some(output)
}

fn extract_legacy_xls_text(bytes: &[u8]) -> Option<String> {
    use calamine::{Data, Reader, Xls};

    let mut workbook = Xls::new(std::io::Cursor::new(bytes)).ok()?;
    let sheet_names = workbook.sheet_names();
    if sheet_names.len() > 256 {
        return None;
    }
    let mut output = String::new();
    for sheet in sheet_names {
        let range = workbook.worksheet_range(&sheet).ok()?;
        for (_, _, cell) in range.used_cells() {
            let value = match cell {
                Data::String(value) | Data::DateTimeIso(value) | Data::DurationIso(value) => {
                    value.clone()
                }
                Data::Int(value) => value.to_string(),
                Data::Float(value) => value.to_string(),
                Data::Bool(value) => value.to_string(),
                Data::DateTime(value) => value.to_string(),
                Data::Error(_) | Data::Empty => continue,
            };
            append_find_text(&mut output, &value)?;
        }
    }
    Some(output)
}

fn extract_legacy_ppt_text(bytes: &[u8]) -> Option<String> {
    // PPT is a compound-file container. Keep each parser limit at or below the
    // already enforced attachment cap so a small hostile file cannot cause a
    // large stream, persist table, or parsed text allocation.
    let max_bytes = MAX_FIND_ATTACHMENT_DOWNLOAD_BYTES as usize;
    let limits = duke_slides::ParseLimits {
        max_input_bytes: max_bytes,
        max_entries: 2_048,
        max_entry_bytes: max_bytes,
        max_total_uncompressed_bytes: max_bytes,
        max_xml_depth: 128,
        max_xml_nodes: 250_000,
        max_xml_attributes: 250_000,
        max_xml_text_bytes: MAX_FIND_ATTACHMENT_TEXT_BYTES as usize,
        max_xml_attribute_bytes: MAX_FIND_ATTACHMENT_TEXT_BYTES as usize,
    };
    let presentation = duke_slides::parse_presentation(bytes, &limits).ok()?;
    let mut text = presentation.body_text();
    if text.len() > max_bytes {
        return None;
    }
    for slide in &presentation.slides {
        for note in &slide.notes {
            append_find_text(&mut text, &note.text)?;
        }
        for comment in &slide.comments {
            append_find_text(&mut text, &comment.text)?;
        }
    }
    Some(text)
}

fn extract_legacy_doc_text(bytes: &[u8]) -> Option<String> {
    use std::io::Read;

    let mut compound = cfb::CompoundFile::open(std::io::Cursor::new(bytes)).ok()?;
    let mut read_bounded_stream = |name: &str| {
        let mut stream = compound.open_stream(name).ok()?;
        let size = usize::try_from(stream.len()).ok()?;
        if size > MAX_FIND_ATTACHMENT_DOWNLOAD_BYTES as usize {
            return None;
        }
        let mut data = Vec::with_capacity(size);
        stream
            .by_ref()
            .take(MAX_FIND_ATTACHMENT_DOWNLOAD_BYTES + 1)
            .read_to_end(&mut data)
            .ok()?;
        (data.len() <= MAX_FIND_ATTACHMENT_DOWNLOAD_BYTES as usize).then_some(data)
    };

    let word = read_bounded_stream("/WordDocument")?;
    let table_name = if word.get(10).is_some_and(|flags| flags & 0x02 != 0) {
        "/1Table"
    } else {
        "/0Table"
    };
    let table = read_bounded_stream(table_name)?;
    let pieces = legacy_doc_piece_table(&word, &table)?;
    let mut output = String::new();
    for piece in pieces {
        let end = piece.offset.checked_add(piece.length)?;
        let raw = word.get(piece.offset..end)?;
        if piece.compressed {
            let (decoded, _) = encoding_rs::WINDOWS_1252.decode_without_bom_handling(raw);
            append_find_text(&mut output, &decoded)?;
        } else {
            if raw.len() % 2 != 0 {
                return None;
            }
            let units = raw
                .as_chunks::<2>()
                .0
                .iter()
                .map(|chunk| u16::from_le_bytes(*chunk))
                .collect::<Vec<_>>();
            append_find_text(&mut output, &String::from_utf16_lossy(&units))?;
        }
    }
    Some(output)
}

#[derive(Debug)]
struct LegacyDocPiece {
    compressed: bool,
    offset: usize,
    length: usize,
}

fn legacy_doc_piece_table(word: &[u8], table: &[u8]) -> Option<Vec<LegacyDocPiece>> {
    let fc_clx = usize::try_from(u32::from_le_bytes(
        word.get(0x01a2..0x01a6)?.try_into().ok()?,
    ))
    .ok()?;
    let lcb_clx = usize::try_from(u32::from_le_bytes(
        word.get(0x01a6..0x01aa)?.try_into().ok()?,
    ))
    .ok()?;
    if lcb_clx == 0 {
        let fc_min =
            usize::try_from(u32::from_le_bytes(word.get(24..28)?.try_into().ok()?)).ok()?;
        let fc_mac =
            usize::try_from(u32::from_le_bytes(word.get(28..32)?.try_into().ok()?)).ok()?;
        return (fc_mac > fc_min).then_some(vec![LegacyDocPiece {
            compressed: false,
            offset: fc_min,
            length: fc_mac - fc_min,
        }]);
    }
    if lcb_clx > MAX_FIND_ATTACHMENT_TEXT_BYTES as usize {
        return None;
    }
    let clx_end = fc_clx.checked_add(lcb_clx)?;
    let clx = table.get(fc_clx..clx_end)?;
    let mut position = 0usize;
    loop {
        match *clx.get(position)? {
            0x01 => {
                let cb = usize::from(u16::from_le_bytes(
                    clx.get(position.checked_add(1)?..position.checked_add(3)?)?
                        .try_into()
                        .ok()?,
                ));
                position = position.checked_add(3)?.checked_add(cb)?;
            }
            0x02 => break,
            _ => return None,
        }
    }
    let plc_header_end = position.checked_add(5)?;
    let plc_len = usize::try_from(u32::from_le_bytes(
        clx.get(position.checked_add(1)?..plc_header_end)?
            .try_into()
            .ok()?,
    ))
    .ok()?;
    let plc_end = plc_header_end.checked_add(plc_len)?;
    let plc = clx.get(plc_header_end..plc_end)?;
    if plc.len() < 8 || !(plc.len() - 4).is_multiple_of(12) {
        return None;
    }
    let piece_count = (plc.len() - 4) / 12;
    let pcds_start = piece_count.checked_add(1)?.checked_mul(4)?;
    let mut pieces = Vec::with_capacity(piece_count);
    for index in 0..piece_count {
        let cp_start = i32::from_le_bytes(
            plc.get(index.checked_mul(4)?..index.checked_mul(4)?.checked_add(4)?)?
                .try_into()
                .ok()?,
        );
        let next_cp = index.checked_add(1)?.checked_mul(4)?;
        let cp_end =
            i32::from_le_bytes(plc.get(next_cp..next_cp.checked_add(4)?)?.try_into().ok()?);
        if cp_start < 0 || cp_end <= cp_start {
            return None;
        }
        let char_count = usize::try_from(cp_end.checked_sub(cp_start)?).ok()?;
        let pcd_start = pcds_start.checked_add(index.checked_mul(8)?)?;
        let raw_fc = u32::from_le_bytes(
            plc.get(pcd_start.checked_add(2)?..pcd_start.checked_add(6)?)?
                .try_into()
                .ok()?,
        );
        let compressed = raw_fc & 0x4000_0000 != 0;
        let file_offset = raw_fc & !0xc000_0000;
        let offset = if compressed {
            usize::try_from(file_offset >> 1).ok()?
        } else {
            usize::try_from(file_offset).ok()?
        };
        let length = if compressed {
            char_count
        } else {
            char_count.checked_mul(2)?
        };
        pieces.push(LegacyDocPiece {
            compressed,
            offset,
            length,
        });
    }
    Some(pieces)
}

#[cfg(test)]
pub(crate) fn test_legacy_doc_attachment(text: &str) -> Vec<u8> {
    use std::io::Write;

    let raw_text = text.as_bytes();
    let raw_fc = ((512u32) << 1) | 0x4000_0000;
    let mut plc = Vec::with_capacity(16);
    plc.extend_from_slice(&0i32.to_le_bytes());
    plc.extend_from_slice(&(raw_text.len() as i32).to_le_bytes());
    plc.extend_from_slice(&0u16.to_le_bytes());
    plc.extend_from_slice(&raw_fc.to_le_bytes());
    plc.extend_from_slice(&0u16.to_le_bytes());
    let mut clx = vec![0x02];
    clx.extend_from_slice(&(plc.len() as u32).to_le_bytes());
    clx.extend_from_slice(&plc);

    let mut word = vec![0; 512];
    word[0x00..0x02].copy_from_slice(&0x00c1u16.to_le_bytes());
    word[0x02..0x04].copy_from_slice(&0x00bfu16.to_le_bytes());
    word[10] = 0x02; // fWhichTblStm selects 1Table.
    word[24..28].copy_from_slice(&0u32.to_le_bytes());
    word[28..32].copy_from_slice(&(raw_text.len() as u32).to_le_bytes());
    word[0x01a2..0x01a6].copy_from_slice(&0u32.to_le_bytes());
    word[0x01a6..0x01aa].copy_from_slice(&(clx.len() as u32).to_le_bytes());
    word.extend_from_slice(raw_text);

    let mut compound = cfb::CompoundFile::create(std::io::Cursor::new(Vec::new())).unwrap();
    compound
        .create_stream("/WordDocument")
        .unwrap()
        .write_all(&word)
        .unwrap();
    compound
        .create_stream("/1Table")
        .unwrap()
        .write_all(&clx)
        .unwrap();
    compound.into_inner().into_inner()
}

pub(super) fn matching_principals(
    mut principals: Vec<crate::jmap::client::PrincipalResult>,
    term: &str,
) -> Vec<crate::jmap::client::PrincipalResult> {
    let term = principal_search_tokens(term);
    if term.is_empty() {
        return Vec::new();
    }
    principals.retain(|principal| {
        principal_field_matches_prefix(&principal.display_name, &term)
            || principal_field_matches_prefix(&principal.alias, &term)
            || principal
                .email
                .as_ref()
                .is_some_and(|email| principal_field_matches_prefix(email, &term))
    });
    principals.sort_by_key(|principal| {
        (
            principal.display_name.to_lowercase(),
            principal.alias.to_lowercase(),
            principal.id.clone(),
        )
    });
    principals
}

fn principal_search_tokens(value: &str) -> Vec<String> {
    value
        .split(|character: char| !character.is_alphanumeric())
        .filter(|part| !part.is_empty())
        .map(str::to_lowercase)
        .collect()
}

fn principal_field_matches_prefix(field: &str, query: &[String]) -> bool {
    let words = principal_search_tokens(field);
    !query.is_empty()
        && query.len() <= words.len()
        && words.windows(query.len()).any(|candidate| {
            candidate
                .iter()
                .zip(query)
                .all(|(candidate, query)| candidate.starts_with(query))
        })
}

async fn execute_gal(
    state: &AppState,
    auth: &AuthenticatedSession,
    device: &EasQuery,
    request: Request,
) -> anyhow::Result<wbxml::Document> {
    let term = request.text.to_lowercase();
    let principals = matching_principals(state.jmap.all_principals(auth).await?, &term);
    let query_state = uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_OID,
        &serde_json::to_vec(&principals)?,
    )
    .to_string();
    let filter = json!({"gal": term});
    let user = auth.username();
    let device = device.device_id.as_deref().unwrap_or("unknown");
    let now = chrono::Utc::now().timestamp();
    let mut sessions = state.state.item_states(user, device, SEARCHES).await?;
    sessions.retain(|entry| {
        serde_json::from_str::<SearchState>(&entry.hash)
            .is_ok_and(|session| now - session.created < 900)
    });
    let previous = sessions
        .iter()
        .find(|item| item.item_id == request.search_id)
        .and_then(|item| serde_json::from_str::<SearchState>(&item.hash).ok());
    if request.start != 0
        && previous.as_ref().is_none_or(|session| {
            !same_search(session, &filter, &term) || session.query_state != query_state
        })
    {
        return Ok(restart_document());
    }
    if request.start == 0 {
        sessions.retain(|item| item.item_id != request.search_id);
        if sessions.len() >= 16 {
            sessions.remove(0);
        }
        sessions.push(ItemState {
            item_id: request.search_id,
            hash: serde_json::to_string(&SearchState {
                filter,
                search_text: term,
                query_state,
                created: now,
            })?,
        });
        state
            .state
            .put_item_states(user, device, SEARCHES, sessions)
            .await?;
    }
    let mut b = DocumentBuilder::new();
    b.start(f::FIND);
    b.leaf(f::STATUS, "1");
    b.start(f::RESPONSE);
    b.leaf(ops::STORE, "Mailbox");
    b.leaf(f::STATUS, "1");
    let page = principals
        .iter()
        .skip(request.start)
        .take(request.end - request.start + 1);
    let mut count = 0;
    for principal in page {
        b.start(f::RESULT);
        b.start(f::PROPERTIES);
        b.leaf(gal::DISPLAY_NAME, &principal.display_name);
        b.leaf(gal::ALIAS, &principal.alias);
        if let Some(email) = &principal.email {
            b.leaf(gal::EMAIL_ADDRESS, email);
        }
        b.end();
        b.end();
        count += 1;
    }
    if count != 0 {
        b.leaf(
            f::RANGE,
            format!("{}-{}", request.start, request.start + count - 1),
        );
    }
    b.leaf(f::TOTAL, principals.len().to_string());
    b.end();
    b.end();
    Ok(b.finish())
}

fn restart_document() -> wbxml::Document {
    let mut b = DocumentBuilder::new();
    b.start(f::FIND);
    b.leaf(f::STATUS, "1");
    b.start(f::RESPONSE);
    b.leaf(ops::STORE, "Mailbox");
    b.leaf(f::STATUS, "4");
    b.leaf(f::TOTAL, "0");
    b.end();
    b.end();
    b.finish()
}

fn email_matches_phrases(
    subject: &str,
    body: Option<&crate::model::EmailBody>,
    is_read: Option<bool>,
    is_flagged: Option<bool>,
    has_attachment: Option<bool>,
    phrase_match: query::PhraseMatch,
    phrases: &[(String, String)],
) -> anyhow::Result<bool> {
    email_matches_phrases_with_addresses(
        subject,
        body,
        "",
        "",
        "",
        "",
        None,
        None,
        0,
        1,
        is_read,
        is_flagged,
        has_attachment,
        phrase_match,
        phrases,
    )
}

#[allow(clippy::too_many_arguments)]
fn email_matches_phrases_with_addresses(
    subject: &str,
    body: Option<&crate::model::EmailBody>,
    from: &str,
    to: &str,
    cc: &str,
    bcc: &str,
    received_at: Option<&str>,
    sent_at: Option<&str>,
    size: u64,
    importance: u8,
    is_read: Option<bool>,
    is_flagged: Option<bool>,
    has_attachment: Option<bool>,
    phrase_match: query::PhraseMatch,
    phrases: &[(String, String)],
) -> anyhow::Result<bool> {
    email_matches_phrases_with_addresses_and_attachments(
        subject,
        body,
        from,
        to,
        cc,
        bcc,
        received_at,
        sent_at,
        size,
        importance,
        is_read,
        is_flagged,
        has_attachment,
        &[],
        phrase_match,
        phrases,
    )
}

#[allow(clippy::too_many_arguments)]
fn email_matches_phrases_with_addresses_and_attachments(
    subject: &str,
    body: Option<&crate::model::EmailBody>,
    from: &str,
    to: &str,
    cc: &str,
    bcc: &str,
    received_at: Option<&str>,
    sent_at: Option<&str>,
    size: u64,
    importance: u8,
    is_read: Option<bool>,
    is_flagged: Option<bool>,
    has_attachment: Option<bool>,
    attachments: &[crate::model::EmailAttachment],
    phrase_match: query::PhraseMatch,
    phrases: &[(String, String)],
) -> anyhow::Result<bool> {
    match phrase_match {
        query::PhraseMatch::All => {
            for (property, phrase) in phrases {
                let negated = property.starts_with("$not$");
                let property = property.strip_prefix("$not$").unwrap_or(property);
                let boolean_property = match property {
                    "isread" | "$isread" => Some("$isread"),
                    "isflagged" | "$isflagged" => Some("$isflagged"),
                    "hasattachment" | "$hasattachment" => Some("$hasattachment"),
                    "importance" | "$importance" => Some("$importance"),
                    _ => None,
                };
                let boolean_match = boolean_property.and_then(|property| match property {
                    "$isread" => is_read,
                    "$isflagged" => is_flagged,
                    "$hasattachment" => has_attachment,
                    "$importance" => Some(importance_matches(importance, phrase)),
                    _ => None,
                });
                let matched = if matches!(property, "importance" | "$importance") {
                    importance_matches(importance, phrase)
                } else if matches!(property, "$importance_cmp" | "importance_cmp") {
                    importance_comparison_matches(importance, phrase)
                } else if property == "$received" {
                    received_filter_matches(received_at, phrase)?
                } else if property == "$sent" {
                    received_filter_matches(sent_at, phrase)?
                } else if matches!(property, "$size" | "size") {
                    size_filter_matches(size, phrase)?
                } else if boolean_property.is_some() {
                    boolean_match.is_some_and(|value| value == (phrase == "true"))
                } else {
                    email_matches_phrase_with_attachments(
                        subject,
                        body,
                        from,
                        to,
                        cc,
                        bcc,
                        attachments,
                        property,
                        phrase,
                    )?
                };
                if matched == negated {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        query::PhraseMatch::Any => {
            for (property, phrase) in phrases {
                if property == "$importance" {
                    if importance_matches(importance, phrase) {
                        return Ok(true);
                    }
                    continue;
                }
                if property == "$not$importance" {
                    if !importance_matches(importance, phrase) {
                        return Ok(true);
                    }
                    continue;
                }
                if matches!(
                    property.as_str(),
                    "$importance_cmp" | "$not$importance_cmp" | "$not$$importance_cmp"
                ) {
                    let matched = importance_comparison_matches(importance, phrase);
                    if matched != property.starts_with("$not$") {
                        return Ok(true);
                    }
                    continue;
                }
                if property == "$received" {
                    if received_filter_matches(received_at, phrase)? {
                        return Ok(true);
                    }
                    continue;
                }
                if property == "$not$$received" {
                    if !received_filter_matches(received_at, phrase)? {
                        return Ok(true);
                    }
                    continue;
                }
                if property == "$sent" {
                    if received_filter_matches(sent_at, phrase)? {
                        return Ok(true);
                    }
                    continue;
                }
                if property == "$not$$sent" {
                    if !received_filter_matches(sent_at, phrase)? {
                        return Ok(true);
                    }
                    continue;
                }
                if property == "$size" {
                    if size_filter_matches(size, phrase)? {
                        return Ok(true);
                    }
                    continue;
                }
                if property == "$not$size" {
                    if !size_filter_matches(size, phrase)? {
                        return Ok(true);
                    }
                    continue;
                }
                let boolean_match = match property.as_str() {
                    "$isread" => is_read,
                    "$isflagged" => is_flagged,
                    "$hasattachment" => has_attachment,
                    "$not$isread" => is_read.map(|value| !value),
                    "$not$isflagged" => is_flagged.map(|value| !value),
                    "$not$hasattachment" => has_attachment.map(|value| !value),
                    "$importance" => Some(importance_matches(importance, phrase)),
                    _ => None,
                };
                let matched = if property.starts_with('$') {
                    boolean_match.is_some_and(|value| value == (phrase == "true"))
                } else {
                    email_matches_phrase_with_attachments(
                        subject,
                        body,
                        from,
                        to,
                        cc,
                        bcc,
                        attachments,
                        property,
                        phrase,
                    )?
                };
                if matched {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        query::PhraseMatch::None => {
            for (property, phrase) in phrases {
                if email_matches_phrase_with_attachments(
                    subject,
                    body,
                    from,
                    to,
                    cc,
                    bcc,
                    attachments,
                    property,
                    phrase,
                )? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        query::PhraseMatch::Expression => {
            let mut values = Vec::new();
            for (property, phrase) in phrases {
                match property.as_str() {
                    "$op$not" => {
                        let value = match values
                            .pop()
                            .ok_or_else(|| anyhow::anyhow!("invalid phrase expression"))?
                        {
                            ExpressionValue::Boolean(value) => value,
                            ExpressionValue::ProximityChain(chain) => {
                                proximity_chain_matches_email(
                                    subject,
                                    body,
                                    from,
                                    to,
                                    cc,
                                    bcc,
                                    attachments,
                                    &chain,
                                )?
                            }
                            ExpressionValue::ProximityGroup(operand) => {
                                proximity_operand_matches_email(
                                    subject,
                                    body,
                                    from,
                                    to,
                                    cc,
                                    bcc,
                                    attachments,
                                    &operand,
                                )?
                            }
                            _ => anyhow::bail!("invalid phrase expression"),
                        };
                        values.push(ExpressionValue::Boolean(!value));
                    }
                    "$op$proximity-or" => {
                        let right = proximity_operand_from_value(
                            values
                                .pop()
                                .ok_or_else(|| anyhow::anyhow!("invalid proximity expression"))?,
                        )?;
                        let left = proximity_operand_from_value(
                            values
                                .pop()
                                .ok_or_else(|| anyhow::anyhow!("invalid proximity expression"))?,
                        )?;
                        let mut alternatives = Vec::new();
                        match left {
                            ProximityOperand::Any(items) => alternatives.extend(items),
                            item => alternatives.push(item),
                        }
                        match right {
                            ProximityOperand::Any(items) => alternatives.extend(items),
                            item => alternatives.push(item),
                        }
                        values.push(ExpressionValue::ProximityGroup(ProximityOperand::Any(
                            alternatives,
                        )));
                    }
                    "$op$and" | "$op$or" => {
                        let right = match values
                            .pop()
                            .ok_or_else(|| anyhow::anyhow!("invalid phrase expression"))?
                        {
                            ExpressionValue::Boolean(value) => value,
                            ExpressionValue::ProximityChain(chain) => {
                                proximity_chain_matches_email(
                                    subject,
                                    body,
                                    from,
                                    to,
                                    cc,
                                    bcc,
                                    attachments,
                                    &chain,
                                )?
                            }
                            ExpressionValue::ProximityGroup(operand) => {
                                proximity_operand_matches_email(
                                    subject,
                                    body,
                                    from,
                                    to,
                                    cc,
                                    bcc,
                                    attachments,
                                    &operand,
                                )?
                            }
                            _ => anyhow::bail!("invalid phrase expression"),
                        };
                        let left = match values
                            .pop()
                            .ok_or_else(|| anyhow::anyhow!("invalid phrase expression"))?
                        {
                            ExpressionValue::Boolean(value) => value,
                            ExpressionValue::ProximityChain(chain) => {
                                proximity_chain_matches_email(
                                    subject,
                                    body,
                                    from,
                                    to,
                                    cc,
                                    bcc,
                                    attachments,
                                    &chain,
                                )?
                            }
                            ExpressionValue::ProximityGroup(operand) => {
                                proximity_operand_matches_email(
                                    subject,
                                    body,
                                    from,
                                    to,
                                    cc,
                                    bcc,
                                    attachments,
                                    &operand,
                                )?
                            }
                            _ => anyhow::bail!("invalid phrase expression"),
                        };
                        values.push(ExpressionValue::Boolean(if property == "$op$and" {
                            left && right
                        } else {
                            left || right
                        }));
                    }
                    "$proximity-term" => {
                        values.push(ExpressionValue::ProximityTerm(phrase.clone()))
                    }
                    _ if property.starts_with("$op$near:")
                        || property.starts_with("$op$onear:") => {}
                    _ => {
                        let negated = property.starts_with("$not$");
                        let property = property.strip_prefix("$not$").unwrap_or(property);
                        let boolean_property = match property {
                            "isread" | "$isread" => Some("$isread"),
                            "isflagged" | "$isflagged" => Some("$isflagged"),
                            "hasattachment" | "$hasattachment" => Some("$hasattachment"),
                            "importance" | "$importance" => Some("$importance"),
                            _ => None,
                        };
                        let matched = if matches!(property, "importance" | "$importance") {
                            importance_matches(importance, phrase)
                        } else if matches!(property, "$importance_cmp" | "importance_cmp") {
                            importance_comparison_matches(importance, phrase)
                        } else if property == "$received" {
                            received_filter_matches(received_at, phrase)?
                        } else if property == "$sent" {
                            received_filter_matches(sent_at, phrase)?
                        } else if matches!(property, "$size" | "size") {
                            size_filter_matches(size, phrase)?
                        } else if let Some(boolean_property) = boolean_property {
                            let value = match boolean_property {
                                "$isread" => is_read,
                                "$isflagged" => is_flagged,
                                "$hasattachment" => has_attachment,
                                "$importance" => Some(importance_matches(importance, phrase)),
                                _ => None,
                            };
                            value.is_some_and(|value| value == (phrase == "true"))
                        } else {
                            email_matches_phrase_with_attachments(
                                subject,
                                body,
                                from,
                                to,
                                cc,
                                bcc,
                                attachments,
                                property,
                                phrase,
                            )?
                        };
                        values.push(ExpressionValue::Boolean(if negated {
                            !matched
                        } else {
                            matched
                        }));
                    }
                }
                if let Some(operator) = property.strip_prefix("$op$near:") {
                    let max_words = operator.parse::<usize>()?;
                    let right = values
                        .pop()
                        .ok_or_else(|| anyhow::anyhow!("invalid proximity expression"))?;
                    let left = values
                        .pop()
                        .ok_or_else(|| anyhow::anyhow!("invalid proximity expression"))?;
                    let chain = proximity_chain_from_values(left, right, false, max_words)?;
                    values.push(ExpressionValue::ProximityChain(chain));
                } else if let Some(operator) = property.strip_prefix("$op$onear:") {
                    let max_words = operator.parse::<usize>()?;
                    let right = values
                        .pop()
                        .ok_or_else(|| anyhow::anyhow!("invalid proximity expression"))?;
                    let left = values
                        .pop()
                        .ok_or_else(|| anyhow::anyhow!("invalid proximity expression"))?;
                    let chain = proximity_chain_from_values(left, right, true, max_words)?;
                    values.push(ExpressionValue::ProximityChain(chain));
                }
            }
            if values.len() != 1 {
                anyhow::bail!("invalid phrase expression");
            }
            match values.pop() {
                Some(ExpressionValue::Boolean(value)) => Ok(value),
                Some(ExpressionValue::ProximityChain(chain)) => proximity_chain_matches_email(
                    subject,
                    body,
                    from,
                    to,
                    cc,
                    bcc,
                    attachments,
                    &chain,
                ),
                Some(ExpressionValue::ProximityGroup(operand)) => proximity_operand_matches_email(
                    subject,
                    body,
                    from,
                    to,
                    cc,
                    bcc,
                    attachments,
                    &operand,
                ),
                _ => anyhow::bail!("invalid phrase expression"),
            }
        }
        query::PhraseMatch::Near { max_words } => proximity_matches_email(
            subject,
            body,
            from,
            to,
            cc,
            bcc,
            attachments,
            false,
            max_words,
            phrases,
        ),
        query::PhraseMatch::Onear { max_words } => proximity_matches_email(
            subject,
            body,
            from,
            to,
            cc,
            bcc,
            attachments,
            true,
            max_words,
            phrases,
        ),
    }
}

enum ExpressionValue {
    Boolean(bool),
    ProximityTerm(String),
    ProximityGroup(ProximityOperand),
    ProximityChain(ProximityChain),
}

enum ProximityOperand {
    Term(String),
    Any(Vec<ProximityOperand>),
    Chain(Box<ProximityChain>),
}

fn proximity_operand_from_value(value: ExpressionValue) -> anyhow::Result<ProximityOperand> {
    match value {
        ExpressionValue::ProximityTerm(term) => Ok(ProximityOperand::Term(term)),
        ExpressionValue::ProximityGroup(operand) => Ok(operand),
        ExpressionValue::ProximityChain(chain) => Ok(ProximityOperand::Chain(Box::new(chain))),
        _ => anyhow::bail!("invalid proximity expression"),
    }
}

struct ProximityChain {
    left: ProximityOperand,
    right: ProximityOperand,
    ordered: bool,
    max_words: usize,
}

fn proximity_chain_from_values(
    left: ExpressionValue,
    right: ExpressionValue,
    ordered: bool,
    max_words: usize,
) -> anyhow::Result<ProximityChain> {
    Ok(ProximityChain {
        left: proximity_operand_from_value(left)?,
        right: proximity_operand_from_value(right)?,
        ordered,
        max_words,
    })
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct ProximitySpan {
    start: usize,
    end: usize,
}

#[allow(clippy::too_many_arguments)]
fn proximity_matches_email(
    subject: &str,
    body: Option<&crate::model::EmailBody>,
    from: &str,
    to: &str,
    cc: &str,
    bcc: &str,
    attachments: &[crate::model::EmailAttachment],
    ordered: bool,
    max_words: usize,
    operands: &[(String, String)],
) -> anyhow::Result<bool> {
    anyhow::ensure!(operands.len() == 2, "invalid proximity query");
    proximity_chain_matches_email(
        subject,
        body,
        from,
        to,
        cc,
        bcc,
        attachments,
        &ProximityChain {
            left: ProximityOperand::Term(operands[0].1.clone()),
            right: ProximityOperand::Term(operands[1].1.clone()),
            ordered,
            max_words,
        },
    )
}

#[allow(clippy::too_many_arguments)]
fn proximity_chain_matches_email(
    subject: &str,
    body: Option<&crate::model::EmailBody>,
    from: &str,
    to: &str,
    cc: &str,
    bcc: &str,
    attachments: &[crate::model::EmailAttachment],
    chain: &ProximityChain,
) -> anyhow::Result<bool> {
    let mut fields = vec![
        subject.to_owned(),
        from.to_owned(),
        to.to_owned(),
        cc.to_owned(),
        bcc.to_owned(),
    ];
    if let Some(body) = body {
        anyhow::ensure!(
            !body.is_truncated,
            "Find cannot exact-match proximity in a truncated message body"
        );
        fields.push(match body.body_type {
            crate::model::EmailBodyType::Plain => body.value.clone(),
            crate::model::EmailBodyType::Html => {
                super::strip_html_tags(&body.value.replace('>', "> "))
            }
            crate::model::EmailBodyType::Rtf => super::rtf_to_plain_text(&body.value),
        });
    }
    fields.extend(attachments.iter().map(|attachment| attachment.name.clone()));
    fields.extend(
        attachments
            .iter()
            .filter_map(|attachment| attachment.extracted_text.clone()),
    );
    for field in fields {
        let words = proximity_words(&field);
        if !evaluate_proximity_chain(&words, chain)?.is_empty() {
            return Ok(true);
        }
    }
    Ok(false)
}

#[allow(clippy::too_many_arguments)]
fn proximity_operand_matches_email(
    subject: &str,
    body: Option<&crate::model::EmailBody>,
    from: &str,
    to: &str,
    cc: &str,
    bcc: &str,
    attachments: &[crate::model::EmailAttachment],
    operand: &ProximityOperand,
) -> anyhow::Result<bool> {
    let mut fields = vec![
        subject.to_owned(),
        from.to_owned(),
        to.to_owned(),
        cc.to_owned(),
        bcc.to_owned(),
    ];
    if let Some(body) = body {
        anyhow::ensure!(
            !body.is_truncated,
            "Find cannot exact-match proximity in a truncated message body"
        );
        fields.push(match body.body_type {
            crate::model::EmailBodyType::Plain => body.value.clone(),
            crate::model::EmailBodyType::Html => {
                super::strip_html_tags(&body.value.replace('>', "> "))
            }
            crate::model::EmailBodyType::Rtf => super::rtf_to_plain_text(&body.value),
        });
    }
    fields.extend(attachments.iter().map(|attachment| attachment.name.clone()));
    fields.extend(
        attachments
            .iter()
            .filter_map(|attachment| attachment.extracted_text.clone()),
    );
    for field in fields {
        if !evaluate_proximity_operand(&proximity_words(&field), operand)?.is_empty() {
            return Ok(true);
        }
    }
    Ok(false)
}

fn evaluate_proximity_operand(
    words: &[String],
    operand: &ProximityOperand,
) -> anyhow::Result<std::collections::BTreeSet<ProximitySpan>> {
    match operand {
        ProximityOperand::Term(term) => {
            let term = proximity_pattern_words(term);
            if term.is_empty() {
                return Ok(std::collections::BTreeSet::new());
            }
            Ok(find_term_positions(words, &term)
                .into_iter()
                .map(|start| ProximitySpan {
                    start,
                    end: start + term.len(),
                })
                .collect())
        }
        ProximityOperand::Chain(chain) => evaluate_proximity_chain(words, chain),
        ProximityOperand::Any(alternatives) => {
            let mut spans = std::collections::BTreeSet::new();
            for alternative in alternatives {
                spans.extend(evaluate_proximity_operand(words, alternative)?);
                anyhow::ensure!(
                    spans.len() <= 100_000,
                    "proximity expression produces too many matching spans"
                );
            }
            Ok(spans)
        }
    }
}

fn evaluate_proximity_chain(
    words: &[String],
    chain: &ProximityChain,
) -> anyhow::Result<std::collections::BTreeSet<ProximitySpan>> {
    let left_spans = evaluate_proximity_operand(words, &chain.left)?;
    let right_spans = evaluate_proximity_operand(words, &chain.right)?;
    let mut combined = std::collections::BTreeSet::new();
    let mut comparisons = 0usize;
    for left in &left_spans {
        for right in &right_spans {
            comparisons += 1;
            anyhow::ensure!(
                comparisons <= 1_000_000,
                "proximity expression requires too many span comparisons"
            );
            let gap = left
                .start
                .saturating_sub(right.end)
                .max(right.start.saturating_sub(left.end));
            if gap <= chain.max_words
                && (!chain.ordered || (left.end <= right.start && gap <= chain.max_words))
            {
                insert_proximity_span(&mut combined, *left, *right)?;
            }
        }
    }
    Ok(combined)
}

fn insert_proximity_span(
    spans: &mut std::collections::BTreeSet<ProximitySpan>,
    left: ProximitySpan,
    right: ProximitySpan,
) -> anyhow::Result<()> {
    spans.insert(ProximitySpan {
        start: left.start.min(right.start),
        end: left.end.max(right.end),
    });
    anyhow::ensure!(
        spans.len() <= 100_000,
        "proximity expression produces too many matching spans"
    );
    Ok(())
}

fn proximity_words(text: &str) -> Vec<String> {
    text.split(|ch: char| !ch.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect()
}

fn find_term_positions(words: &[String], term: &[String]) -> Vec<usize> {
    if term.is_empty() || term.len() > words.len() {
        return Vec::new();
    }
    words
        .windows(term.len())
        .enumerate()
        .filter_map(|(index, window)| {
            window
                .iter()
                .zip(term)
                .all(|(word, pattern)| {
                    pattern.strip_suffix('*').map_or_else(
                        || word == pattern,
                        |prefix| !prefix.is_empty() && word.starts_with(prefix),
                    )
                })
                .then_some(index)
        })
        .collect()
}

fn proximity_pattern_words(term: &str) -> Vec<String> {
    let prefix_last = term.trim_end().ends_with('*');
    let mut words = proximity_words(term.trim_end_matches('*').trim_end());
    if prefix_last {
        if let Some(last) = words.last_mut() {
            last.push('*');
        }
    }
    words
}

fn email_matches_phrase(
    subject: &str,
    body: Option<&crate::model::EmailBody>,
    property: &str,
    phrase: &str,
) -> anyhow::Result<bool> {
    email_matches_phrase_with_addresses(subject, body, "", "", "", "", property, phrase)
}

#[allow(clippy::too_many_arguments)]
fn email_matches_phrase_with_addresses(
    subject: &str,
    body: Option<&crate::model::EmailBody>,
    from: &str,
    to: &str,
    cc: &str,
    bcc: &str,
    property: &str,
    phrase: &str,
) -> anyhow::Result<bool> {
    email_matches_phrase_with_attachments(subject, body, from, to, cc, bcc, &[], property, phrase)
}

#[allow(clippy::too_many_arguments)]
fn email_matches_phrase_with_attachments(
    subject: &str,
    body: Option<&crate::model::EmailBody>,
    from: &str,
    to: &str,
    cc: &str,
    bcc: &str,
    attachments: &[crate::model::EmailAttachment],
    property: &str,
    phrase: &str,
) -> anyhow::Result<bool> {
    if phrase == "*" {
        if matches!(property, "body" | "text")
            && body.is_some_and(|body| body.is_truncated && body.value.trim().is_empty())
        {
            anyhow::bail!("Find cannot determine whether a truncated body is non-empty");
        }
        return Ok(match property {
            "subject" => !subject.trim().is_empty(),
            "body" => body.is_some_and(|body| !body.value.trim().is_empty()),
            "from" => !from.trim().is_empty(),
            "to" => !to.trim().is_empty(),
            "cc" => !cc.trim().is_empty(),
            "bcc" => !bcc.trim().is_empty(),
            "participants" => [from, to, cc, bcc]
                .into_iter()
                .any(|value| !value.trim().is_empty()),
            "attachment" => !attachments.is_empty(),
            "text" => {
                !subject.trim().is_empty()
                    || body.is_some_and(|body| !body.value.trim().is_empty())
                    || [from, to, cc, bcc]
                        .into_iter()
                        .any(|value| !value.trim().is_empty())
                    || attachments.iter().any(|attachment| {
                        !attachment.name.trim().is_empty()
                            || attachment
                                .extracted_text
                                .as_deref()
                                .is_some_and(|value| !value.trim().is_empty())
                    })
            }
            _ => false,
        });
    }
    if matches!(property, "from" | "to" | "cc" | "bcc") {
        let value = match property {
            "from" => from,
            "to" => to,
            "cc" => cc,
            _ => bcc,
        };
        return Ok(query::subject_has_phrase(value, phrase));
    }
    if property == "participants" {
        return Ok([from, to, cc, bcc]
            .into_iter()
            .any(|value| query::subject_has_phrase(value, phrase)));
    }
    if property == "text"
        && [from, to, cc, bcc]
            .into_iter()
            .any(|value| query::subject_has_phrase(value, phrase))
    {
        return Ok(true);
    }
    if property == "text"
        && attachments
            .iter()
            .filter_map(|attachment| attachment.extracted_text.as_deref())
            .any(|value| query::subject_has_phrase(value, phrase))
    {
        return Ok(true);
    }
    if property == "attachment" {
        if phrase.trim() == "*" {
            return Ok(!attachments.is_empty());
        }
        return Ok(attachments
            .iter()
            .any(|attachment| query::subject_has_phrase(&attachment.name, phrase)));
    }
    if matches!(property, "subject" | "text") && query::subject_has_phrase(subject, phrase) {
        return Ok(true);
    }
    if property == "subject" {
        return Ok(false);
    }
    let Some(body) = body else {
        return Ok(false);
    };
    anyhow::ensure!(
        !body.is_truncated,
        "Find cannot exact-match a truncated message body"
    );
    let plain = match body.body_type {
        crate::model::EmailBodyType::Plain => body.value.clone(),
        crate::model::EmailBodyType::Html => super::strip_html_tags(&body.value.replace('>', "> ")),
        crate::model::EmailBodyType::Rtf => super::rtf_to_plain_text(&body.value),
    };
    Ok(query::subject_has_phrase(&plain, phrase))
}

fn importance_matches(importance: u8, expected: &str) -> bool {
    match expected {
        "low" => importance == 0,
        "normal" => importance == 1,
        "high" => importance == 2,
        _ => false,
    }
}

fn importance_comparison_matches(importance: u8, comparison: &str) -> bool {
    if importance > 2 {
        return false;
    }
    let Some((operator, expected)) = comparison.split_once(':') else {
        return false;
    };
    let expected = match expected {
        "low" => 0,
        "normal" => 1,
        "high" => 2,
        _ => return false,
    };
    match operator {
        "<>" => importance != expected,
        "<" => importance < expected,
        "<=" => importance <= expected,
        ">" => importance > expected,
        ">=" => importance >= expected,
        _ => false,
    }
}

fn received_filter_matches(received_at: Option<&str>, filter: &str) -> anyhow::Result<bool> {
    fn matches(
        received_at: Option<chrono::DateTime<chrono::FixedOffset>>,
        filter: &serde_json::Value,
    ) -> anyhow::Result<bool> {
        if let Some(operator) = filter.get("operator").and_then(serde_json::Value::as_str) {
            let conditions = filter
                .get("conditions")
                .and_then(serde_json::Value::as_array)
                .ok_or_else(|| anyhow::anyhow!("invalid received date filter"))?;
            let values = conditions
                .iter()
                .map(|condition| matches(received_at, condition))
                .collect::<anyhow::Result<Vec<_>>>()?;
            return Ok(match operator {
                "AND" => values.into_iter().all(|value| value),
                "OR" => values.into_iter().any(|value| value),
                "NOT" if values.len() == 1 => !values[0],
                _ => anyhow::bail!("unsupported received date filter operator"),
            });
        }
        let before = filter
            .get("before")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("invalid received date filter leaf"))?;
        let before = chrono::DateTime::parse_from_rfc3339(before)?;
        Ok(received_at.is_some_and(|received_at| received_at < before))
    }

    let filter: serde_json::Value = serde_json::from_str(filter)?;
    let received_at = received_at
        .map(chrono::DateTime::parse_from_rfc3339)
        .transpose()?;
    matches(received_at, &filter)
}

fn size_filter_matches(size: u64, filter: &str) -> anyhow::Result<bool> {
    fn matches(size: u64, filter: &serde_json::Value) -> anyhow::Result<bool> {
        if let Some(operator) = filter.get("operator").and_then(serde_json::Value::as_str) {
            let conditions = filter
                .get("conditions")
                .and_then(serde_json::Value::as_array)
                .ok_or_else(|| anyhow::anyhow!("invalid size filter"))?;
            let values = conditions
                .iter()
                .map(|condition| matches(size, condition))
                .collect::<anyhow::Result<Vec<_>>>()?;
            return Ok(match operator {
                "AND" => values.into_iter().all(|value| value),
                "OR" => values.into_iter().any(|value| value),
                "NOT" if values.len() == 1 => !values[0],
                _ => anyhow::bail!("unsupported size filter operator"),
            });
        }
        let min = filter.get("minSize").and_then(serde_json::Value::as_u64);
        let max = filter.get("maxSize").and_then(serde_json::Value::as_u64);
        anyhow::ensure!(min.is_some() || max.is_some(), "invalid size filter leaf");
        Ok(min.is_none_or(|minimum| size >= minimum) && max.is_none_or(|maximum| size < maximum))
    }
    let filter: serde_json::Value = serde_json::from_str(filter)?;
    matches(size, &filter)
}

fn error_document(status: &str) -> wbxml::Document {
    let mut b = DocumentBuilder::new();
    b.start(f::FIND);
    b.leaf(f::STATUS, status);
    b.end();
    b.finish()
}

fn error_response(state: &AppState, status: &str) -> Response {
    respond(state, error_document(status))
}
fn respond(state: &AppState, document: wbxml::Document) -> Response {
    state
        .metrics
        .eas_requests_total
        .with_label_values(&["Find", "200"])
        .inc();
    (
        StatusCode::OK,
        [("Content-Type", "application/vnd.ms-sync.wbxml")],
        wbxml::encode_document(&document),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{EmailBody, EmailBodyType};

    #[test]
    fn gal_anr_matches_prefixes_across_name_and_email_tokens() {
        let principal = crate::jmap::client::PrincipalResult {
            id: "person".to_owned(),
            alias: "michael.alexander".to_owned(),
            display_name: "Michael Alexander".to_owned(),
            email: Some("michael.alexander@example.org".to_owned()),
        };
        for query in ["Michael A.", "Alex", "alexander@example"] {
            assert_eq!(
                matching_principals(vec![principal.clone()], query).len(),
                1,
                "{query}"
            );
        }
        assert!(matching_principals(vec![principal], "unrelated").is_empty());
    }

    #[test]
    fn find_paging_identity_includes_the_original_kql_query() {
        let session = SearchState {
            filter: json!({"operator":"AND", "conditions":[json!({})]}),
            search_text: "alpha NEAR beta".to_owned(),
            query_state: "state".to_owned(),
            created: 0,
        };
        assert!(same_search(&session, &session.filter, "alpha NEAR beta"));
        assert!(!same_search(&session, &session.filter, "gamma NEAR delta"));
    }

    #[test]
    fn importance_comparisons_follow_low_normal_high_order() {
        assert!(importance_comparison_matches(0, "<:normal"));
        assert!(importance_comparison_matches(1, "<=:normal"));
        assert!(importance_comparison_matches(2, ">:normal"));
        assert!(importance_comparison_matches(2, ">=:high"));
        assert!(importance_comparison_matches(0, "<>:high"));
        assert!(!importance_comparison_matches(1, ">:normal"));
        assert!(!importance_comparison_matches(2, "<:normal"));
    }

    #[test]
    fn size_phrase_filters_match_inclusive_and_exclusive_bounds() {
        let at_threshold = serde_json::json!({"minSize": 100});
        let before_threshold = serde_json::json!({"maxSize": 100});
        assert!(size_filter_matches(100, &at_threshold.to_string()).unwrap());
        assert!(!size_filter_matches(99, &at_threshold.to_string()).unwrap());
        assert!(size_filter_matches(99, &before_threshold.to_string()).unwrap());
        assert!(!size_filter_matches(100, &before_threshold.to_string()).unwrap());
        let phrase = ("$size".to_owned(), at_threshold.to_string());
        assert!(email_matches_phrases_with_addresses(
            "",
            None,
            "",
            "",
            "",
            "",
            None,
            None,
            100,
            1,
            None,
            None,
            None,
            query::PhraseMatch::All,
            std::slice::from_ref(&phrase)
        )
        .unwrap());
        assert!(!email_matches_phrases_with_addresses(
            "",
            None,
            "",
            "",
            "",
            "",
            None,
            None,
            99,
            1,
            None,
            None,
            None,
            query::PhraseMatch::All,
            &[phrase]
        )
        .unwrap());
        let negated_size = vec![
            ("subject".to_owned(), "never matches".to_owned()),
            ("$not$size".to_owned(), at_threshold.to_string()),
            ("$op$or".to_owned(), String::new()),
        ];
        assert!(email_matches_phrases_with_addresses(
            "other",
            None,
            "",
            "",
            "",
            "",
            None,
            None,
            99,
            1,
            None,
            None,
            None,
            query::PhraseMatch::Expression,
            &negated_size,
        )
        .unwrap());
        assert!(!email_matches_phrases_with_addresses(
            "other",
            None,
            "",
            "",
            "",
            "",
            None,
            None,
            100,
            1,
            None,
            None,
            None,
            query::PhraseMatch::Expression,
            &negated_size,
        )
        .unwrap());
    }

    #[test]
    fn sent_date_phrase_matching_uses_sent_at_in_utc() {
        let phrase = (
            "$sent".to_owned(),
            r#"{"operator":"AND","conditions":[{"operator":"NOT","conditions":[{"before":"2026-02-28T00:00:00Z"}]},{"before":"2026-03-01T00:00:00Z"}]}"#.to_owned(),
        );
        let matches = |sent_at| {
            email_matches_phrases_with_addresses(
                "",
                None,
                "",
                "",
                "",
                "",
                None,
                sent_at,
                0,
                1,
                None,
                None,
                None,
                query::PhraseMatch::All,
                std::slice::from_ref(&phrase),
            )
            .unwrap()
        };
        assert!(matches(Some("2026-02-28T12:00:00Z")));
        assert!(!matches(Some("2026-02-27T23:59:59Z")));
        assert!(!matches(Some("2026-03-01T00:00:00Z")));
        assert!(!matches(None));
    }

    #[test]
    fn body_and_text_phrase_match_plain_and_html_content() {
        let plain = EmailBody {
            body_type: EmailBodyType::Plain,
            value: "Hello Alpha beta, see you".to_owned(),
            is_truncated: false,
        };
        assert!(email_matches_phrase("Other", Some(&plain), "body", "alpha beta").unwrap());
        assert!(!email_matches_phrase("Other", Some(&plain), "body", "beta alpha").unwrap());

        let html = EmailBody {
            body_type: EmailBodyType::Html,
            value: "<p>Alpha</p><p>beta</p><script>hidden phrase</script>".to_owned(),
            is_truncated: false,
        };
        assert!(email_matches_phrase("Other", Some(&html), "body", "alpha beta").unwrap());
        assert!(!email_matches_phrase("Other", Some(&html), "body", "hidden phrase").unwrap());
        let rtf = EmailBody {
            body_type: EmailBodyType::Rtf,
            value: r#"{\rtf1 Alpha beta}"#.to_owned(),
            is_truncated: false,
        };
        assert!(email_matches_phrase("Other", Some(&rtf), "body", "alpha beta").unwrap());
        assert!(email_matches_phrase("Alpha beta", None, "text", "alpha beta").unwrap());
        assert!(email_matches_phrase("Other", Some(&plain), "text", "alpha beta").unwrap());
        assert!(email_matches_phrase_with_addresses(
            "Unrelated subject",
            None,
            "Alice Example <alice@example.test>",
            "Bob Reader",
            "",
            "",
            "from",
            "alice example",
        )
        .unwrap());
        assert!(!email_matches_phrase_with_addresses(
            "Unrelated subject",
            None,
            "Alice Example <alice@example.test>",
            "Bob Reader",
            "",
            "",
            "to",
            "alice example",
        )
        .unwrap());
        assert!(email_matches_phrase_with_addresses(
            "Unrelated subject",
            None,
            "Alice Example <alice@example.test>",
            "Bob Reader",
            "",
            "",
            "participants",
            "bob reader",
        )
        .unwrap());
        assert!(email_matches_phrases(
            "Alpha beta subject",
            Some(&plain),
            None,
            None,
            None,
            query::PhraseMatch::All,
            &[
                ("subject".to_owned(), "alpha beta".to_owned()),
                ("body".to_owned(), "alpha beta".to_owned())
            ]
        )
        .unwrap());
        assert!(!email_matches_phrases(
            "Alpha beta subject",
            Some(&plain),
            None,
            None,
            None,
            query::PhraseMatch::All,
            &[
                ("subject".to_owned(), "alpha beta".to_owned()),
                ("body".to_owned(), "beta alpha".to_owned())
            ]
        )
        .unwrap());
        assert!(email_matches_phrases(
            "Alpha beta and gamma delta",
            None,
            None,
            None,
            None,
            query::PhraseMatch::All,
            &[
                ("subject".to_owned(), "alpha beta".to_owned()),
                ("subject".to_owned(), "gamma delta".to_owned())
            ]
        )
        .unwrap());
        assert!(!email_matches_phrases(
            "Alpha beta only",
            None,
            None,
            None,
            None,
            query::PhraseMatch::All,
            &[
                ("subject".to_owned(), "alpha beta".to_owned()),
                ("subject".to_owned(), "gamma delta".to_owned())
            ]
        )
        .unwrap());
        assert!(email_matches_phrases(
            "Unrelated subject",
            None,
            None,
            None,
            None,
            query::PhraseMatch::None,
            &[("subject".to_owned(), "alpha beta".to_owned())]
        )
        .unwrap());
        assert!(!email_matches_phrases(
            "Alpha beta subject",
            None,
            None,
            None,
            None,
            query::PhraseMatch::None,
            &[("subject".to_owned(), "alpha beta".to_owned())]
        )
        .unwrap());
        assert!(email_matches_phrases(
            "Other subject",
            Some(&plain),
            None,
            None,
            None,
            query::PhraseMatch::Any,
            &[
                ("subject".to_owned(), "alpha beta".to_owned()),
                ("body".to_owned(), "alpha beta".to_owned())
            ]
        )
        .unwrap());
        assert!(email_matches_phrases_with_addresses(
            "Unrelated subject",
            None,
            "",
            "",
            "",
            "",
            None,
            None,
            0,
            2,
            None,
            None,
            None,
            query::PhraseMatch::Expression,
            &[
                ("$not$subject".to_owned(), "alpha beta".to_owned()),
                ("$importance".to_owned(), "high".to_owned()),
                ("$op$or".to_owned(), String::new())
            ]
        )
        .unwrap());
        assert!(!email_matches_phrases_with_addresses(
            "Alpha beta subject",
            None,
            "",
            "",
            "",
            "",
            None,
            None,
            0,
            1,
            None,
            None,
            None,
            query::PhraseMatch::Expression,
            &[
                ("$not$subject".to_owned(), "alpha beta".to_owned()),
                ("$importance".to_owned(), "high".to_owned()),
                ("$op$or".to_owned(), String::new())
            ]
        )
        .unwrap());
        assert!(email_matches_phrases_with_addresses(
            "Unrelated subject",
            None,
            "",
            "",
            "",
            "",
            None,
            None,
            0,
            1,
            None,
            None,
            None,
            query::PhraseMatch::Any,
            &[
                ("$not$importance".to_owned(), "high".to_owned()),
                ("subject".to_owned(), "alpha beta".to_owned())
            ]
        )
        .unwrap());
        assert!(!email_matches_phrases_with_addresses(
            "Unrelated subject",
            None,
            "",
            "",
            "",
            "",
            None,
            None,
            0,
            2,
            None,
            None,
            None,
            query::PhraseMatch::Any,
            &[
                ("$not$importance".to_owned(), "high".to_owned()),
                ("subject".to_owned(), "alpha beta".to_owned())
            ]
        )
        .unwrap());
        assert!(!email_matches_phrases(
            "Other subject",
            None,
            None,
            None,
            None,
            query::PhraseMatch::Any,
            &[
                ("subject".to_owned(), "alpha beta".to_owned()),
                ("text".to_owned(), "alpha beta".to_owned())
            ]
        )
        .unwrap());
        assert!(email_matches_phrases(
            "Other subject",
            None,
            Some(false),
            Some(true),
            Some(false),
            query::PhraseMatch::Any,
            &[("$isflagged".to_owned(), "true".to_owned())]
        )
        .unwrap());
        assert!(email_matches_phrases(
            "Other subject",
            None,
            Some(false),
            Some(false),
            Some(false),
            query::PhraseMatch::Any,
            &[("$not$isread".to_owned(), "true".to_owned())]
        )
        .unwrap());
        assert!(email_matches_phrases(
            "Other subject",
            None,
            Some(false),
            Some(false),
            Some(false),
            query::PhraseMatch::Any,
            &[("$hasattachment".to_owned(), "false".to_owned())]
        )
        .unwrap());
    }

    #[test]
    fn words_operator_matches_quoted_synonym_as_an_exact_phrase() {
        let (kind, terms) = query::phrases_for_postfilter(r#"WORDS("alpha beta", gamma)"#).unwrap();
        assert_eq!(kind, query::PhraseMatch::Any);
        assert!(
            email_matches_phrases("Alpha beta subject", None, None, None, None, kind, &terms,)
                .unwrap()
        );
        assert!(
            email_matches_phrases("Gamma subject", None, None, None, None, kind, &terms,).unwrap()
        );
        assert!(
            !email_matches_phrases("Alpha between beta", None, None, None, None, kind, &terms,)
                .unwrap()
        );
        assert!(
            !email_matches_phrases("Beta alpha subject", None, None, None, None, kind, &terms,)
                .unwrap()
        );

        let (kind, terms) = query::phrases_for_postfilter(r#"WORDS(serv*, "alpha beta")"#).unwrap();
        assert!(
            email_matches_phrases("Service request", None, None, None, None, kind, &terms,)
                .unwrap()
        );
        assert!(
            email_matches_phrases("Alpha beta request", None, None, None, None, kind, &terms,)
                .unwrap()
        );
        assert!(
            !email_matches_phrases("Observe request", None, None, None, None, kind, &terms,)
                .unwrap()
        );
    }

    #[test]
    fn property_wildcard_matches_nonempty_values_and_fails_closed_when_unknown() {
        let body = EmailBody {
            body_type: EmailBodyType::Plain,
            value: "Body text".to_owned(),
            is_truncated: false,
        };
        for (property, subject, body, from, to, attachments, expected) in [
            ("subject", "Subject", None, "", "", &[][..], true),
            ("subject", "", None, "", "", &[][..], false),
            ("body", "", Some(&body), "", "", &[][..], true),
            ("from", "", None, "Sender", "", &[][..], true),
            ("to", "", None, "", "Recipient", &[][..], true),
            ("text", "Subject", None, "", "", &[][..], true),
            ("attachment", "", None, "", "", &[][..], false),
        ] {
            assert_eq!(
                email_matches_phrase_with_attachments(
                    subject,
                    body,
                    from,
                    to,
                    "",
                    "",
                    attachments,
                    property,
                    "*",
                )
                .unwrap(),
                expected,
                "{property}"
            );
        }
        let truncated = EmailBody {
            is_truncated: true,
            value: String::new(),
            ..body
        };
        assert!(email_matches_phrase_with_attachments(
            "",
            Some(&truncated),
            "",
            "",
            "",
            "",
            &[],
            "body",
            "*"
        )
        .is_err());
    }

    #[test]
    fn find_extracts_text_attachments_for_exact_search_matching() {
        use std::io::Write;

        assert!(is_extractable_attachment_type("application/xhtml+xml"));
        assert!(is_extractable_attachment_type("application/atom+xml"));
        assert!(is_extractable_attachment_type("application/pdf"));
        assert!(is_extractable_attachment_type("application/zip"));
        assert!(is_extractable_attachment_type(
            "application/x-zip-compressed"
        ));
        assert!(is_extractable_attachment_type("application/vnd.ms-outlook"));
        assert!(is_extractable_attachment_type("application/problem+json"));
        for media_type in [
            "application/csv",
            "application/ndjson",
            "application/x-ndjson",
            "application/yaml",
            "application/x-yaml",
            "application/toml",
        ] {
            assert!(is_extractable_attachment_type(media_type), "{media_type}");
        }
        assert!(is_extractable_attachment_type("message/rfc822"));
        assert!(is_extractable_attachment_type("message/global"));
        assert!(is_extractable_attachment_type("application/epub+zip"));
        assert!(is_extractable_attachment_type(
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
        ));
        assert!(is_extractable_attachment_type(
            "application/vnd.ms-word.document.macroEnabled.12"
        ));
        assert!(is_extractable_attachment_type(
            "application/vnd.ms-excel.sheet.macroEnabled.12"
        ));
        assert!(is_extractable_attachment_type(
            "application/vnd.ms-powerpoint.presentation.macroEnabled.12"
        ));
        assert!(is_extractable_attachment_type(
            "application/vnd.oasis.opendocument.text"
        ));
        assert_eq!(
            extract_find_attachment_text("text/plain; charset=utf-8", b"alpha beta"),
            Some("alpha beta".to_owned())
        );
        assert_eq!(
            extract_find_attachment_text("text/plain; charset=windows-1252", b"price \x80"),
            Some("price €".to_owned())
        );
        assert_eq!(
            extract_find_attachment_text("text/plain; charset=\"UTF-16LE\"", b"a\0b\0"),
            Some("ab".to_owned())
        );
        assert_eq!(
            extract_find_attachment_text("text/plain", b"\xff\xfea\0b\0"),
            Some("ab".to_owned())
        );
        assert_eq!(
            extract_find_attachment_text("text/html", b"<p>alpha</p><p>beta</p>"),
            Some("alpha beta".to_owned())
        );
        assert_eq!(
            extract_find_attachment_text("text/csv; charset=utf-8", b"alpha,beta"),
            Some("alpha,beta".to_owned())
        );
        assert_eq!(
            extract_find_attachment_text("application/json", b"alpha beta"),
            Some("alpha beta".to_owned())
        );
        assert_eq!(
            extract_find_attachment_text("application/problem+json", b"alpha beta"),
            Some("alpha beta".to_owned())
        );
        assert_eq!(
            extract_find_attachment_text(
                "application/yaml; charset=utf-8",
                b"title: alpha\nbody: beta",
            ),
            Some("title: alpha body: beta".to_owned())
        );
        assert_eq!(
            extract_find_attachment_text(
                "application/x-ndjson",
                b"{\"event\":\"alpha\"}\n{\"event\":\"beta\"}",
            ),
            Some("{\"event\":\"alpha\"} {\"event\":\"beta\"}".to_owned())
        );
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        zip.start_file("docs/README.md", zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"alpha beta from a ZIP readme").unwrap();
        zip.start_file(
            "settings/config.yaml",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        zip.write_all(b"gamma: delta").unwrap();
        zip.start_file("images/logo.png", zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"binary image data is skipped").unwrap();
        let zip = zip.finish().unwrap().into_inner();
        assert_eq!(
            extract_find_attachment_text("application/zip", &zip),
            Some("alpha beta from a ZIP readme gamma: delta".to_owned())
        );
        let eml = b"From: sender@example.test\r\nSubject: alpha subject\r\nContent-Type: multipart/alternative; boundary=body\r\n\r\n--body\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\nplain=20beta\r\n--body\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<p>html gamma</p>\r\n--body--\r\n";
        let extracted = extract_find_attachment_text("message/rfc822", eml).unwrap();
        assert!(extracted.contains("alpha subject"));
        assert!(extracted.contains("plain beta"));
        assert!(extracted.contains("html gamma"));
        // Upstream msg_parser 0.3.6 MIT test fixture, used to exercise an
        // actual OLE/Outlook message rather than only malformed input.
        let outlook = include_bytes!("../../tests/fixtures/outlook-ascii.msg");
        let extracted = extract_find_attachment_text("application/vnd.ms-outlook", outlook)
            .expect("valid Outlook message should expose its subject");
        assert!(extracted.contains("creating an outlook message file"));
        let extracted = extract_find_attachment_text(
            "message/global",
            b"From: sender@example.test\r\nSubject: delta\r\n\r\nbody epsilon",
        )
        .unwrap();
        assert!(extracted.contains("delta"));
        assert!(extracted.contains("body epsilon"));
        assert_eq!(
            extract_find_attachment_text(
                "application/xml",
                b"<root><first>alpha</first><second>beta &amp; gamma</second></root>",
            ),
            Some("alpha beta & gamma".to_owned())
        );
        assert_eq!(
            extract_find_attachment_text(
                "application/xml",
                b"<?xml version=\"1.0\" encoding=\"windows-1252\"?><root>caf\xe9</root>",
            ),
            Some("café".to_owned())
        );
        assert_eq!(
            extract_find_attachment_text("text/xml", b"<root><![CDATA[alpha beta]]></root>",),
            Some("alpha beta".to_owned())
        );
        assert_eq!(
            extract_find_attachment_text(
                "application/xhtml+xml",
                b"<html><body><p>alpha</p><p>beta</p></body></html>",
            ),
            Some("alpha beta".to_owned())
        );
        let pdf = test_pdf_with_text("alpha beta");
        assert_eq!(
            extract_find_attachment_text("application/pdf", &pdf),
            Some("alpha beta".to_owned())
        );
        assert_eq!(
            extract_find_attachment_text("application/pdf", b"not a PDF"),
            None
        );
        let docx = test_office_archive(&[(
            "word/document.xml",
            b"<w:document><w:t>alpha</w:t><w:t>beta</w:t></w:document>",
        )]);
        assert_eq!(
            extract_find_attachment_text(
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
                &docx
            ),
            Some("alpha beta".to_owned())
        );
        let docx_with_comments = test_office_archive(&[
            (
                "word/document.xml",
                b"<w:document><w:t>body text</w:t></w:document>",
            ),
            (
                "word/comments.xml",
                b"<w:comments><w:t>comment text</w:t></w:comments>",
            ),
        ]);
        assert_eq!(
            extract_find_attachment_text(
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
                &docx_with_comments
            ),
            Some("body text comment text".to_owned())
        );
        let xlsx_with_comments = test_office_archive(&[(
            "xl/comments1.xml",
            b"<comments><comment><text>cell comment</text></comment></comments>",
        )]);
        assert_eq!(
            extract_find_attachment_text(
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
                &xlsx_with_comments
            ),
            Some("cell comment".to_owned())
        );
        let pptx_with_comments = test_office_archive(&[(
            "ppt/comments/comment1.xml",
            b"<comments><cm><text>slide comment</text></cm></comments>",
        )]);
        assert_eq!(
            extract_find_attachment_text(
                "application/vnd.openxmlformats-officedocument.presentationml.presentation",
                &pptx_with_comments
            ),
            Some("slide comment".to_owned())
        );
        let odt = test_office_archive(&[(
            "content.xml",
            b"<document><p>gamma</p><p>delta</p></document>",
        )]);
        assert_eq!(
            extract_find_attachment_text("application/vnd.oasis.opendocument.text", &odt),
            Some("gamma delta".to_owned())
        );
        let epub = test_office_archive(&[(
            "EPUB/chapter.xhtml",
            b"<html><body><p>hotel</p><p>india</p></body></html>",
        )]);
        assert_eq!(
            extract_find_attachment_text("application/epub+zip", &epub),
            Some("hotel india".to_owned())
        );

        let attachment = crate::model::EmailAttachment {
            blob_id: "text-blob".to_owned(),
            part_id: None,
            name: "notes.txt".to_owned(),
            content_type: "text/plain".to_owned(),
            size: 10,
            content_id: None,
            content_location: None,
            inline: false,
            extracted_text: Some("Alpha beta in attachment".to_owned()),
        };
        assert!(email_matches_phrase_with_attachments(
            "Unrelated subject",
            None,
            "",
            "",
            "",
            "",
            std::slice::from_ref(&attachment),
            "text",
            "alpha beta",
        )
        .unwrap());
        assert!(proximity_matches_email(
            "Unrelated subject",
            None,
            "",
            "",
            "",
            "",
            &[attachment],
            false,
            0,
            &[
                ("left".to_owned(), "alpha".to_owned()),
                ("right".to_owned(), "beta".to_owned())
            ],
        )
        .unwrap());
    }

    #[test]
    fn generic_zip_expansion_limit_matches_text_output_limit() {
        use std::io::Write;

        let mut exact_zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        exact_zip
            .start_file("exact.txt", zip::write::SimpleFileOptions::default())
            .unwrap();
        exact_zip
            .write_all(&vec![b'x'; MAX_FIND_ARCHIVE_EXPANDED_BYTES])
            .unwrap();
        let exact_zip = exact_zip.finish().unwrap().into_inner();
        assert_eq!(
            extract_archive_text(&exact_zip, "application/zip")
                .unwrap()
                .len(),
            MAX_FIND_ARCHIVE_EXPANDED_BYTES
        );

        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        zip.start_file("large.txt", zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(&vec![b'x'; MAX_FIND_ARCHIVE_EXPANDED_BYTES + 1])
            .unwrap();
        let zip = zip.finish().unwrap().into_inner();

        assert!(extract_archive_text(&zip, "application/zip").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn scanned_pdf_ocr_limits_pages_and_bounds_tool_execution() {
        use super::{extract_pdf_pages_ocr_with_tools, FindOcrTempDir};
        use std::{fs, os::unix::fs::PermissionsExt};

        let tools = FindOcrTempDir::create().unwrap();
        let renderer = tools.0.join("pdftoppm");
        let ocr = tools.0.join("tesseract");
        let render_args = tools.0.join("render-args");
        fs::write(
            &renderer,
            format!(
                "#!/bin/sh\nset -eu\nprintf '%s\\n' \"$@\" >> '{}'\nprefix=\npage=\nprevious=\nfor arg in \"$@\"; do [ \"$previous\" != -f ] || page=\"$arg\"; prefix=\"$arg\"; previous=\"$arg\"; done\nprintf image > \"$prefix-$page.png\"\n",
                render_args.display()
            ),
        )
        .unwrap();
        fs::write(
            &ocr,
            "#!/bin/sh\nset -eu\ncase \"$1\" in *page-1.png) text='scan alpha';; *page-2.png) text='beta';; *) text='ignored page';; esac\nprintf '%s\\n' \"$text\" > \"$2.txt\"\n",
        )
        .unwrap();
        for tool in [&renderer, &ocr] {
            let mut permissions = fs::metadata(tool).unwrap().permissions();
            permissions.set_mode(0o700);
            fs::set_permissions(tool, permissions).unwrap();
        }

        let pages =
            extract_pdf_pages_ocr_with_tools(b"bounded test PDF", &[1, 2, 3], &renderer, &ocr)
                .expect("bounded OCR pipeline should extract the fake page text");
        let text = pages
            .into_iter()
            .map(|(_, text)| text)
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(
            super::normalize_find_attachment_text(&text),
            Some("scan alpha beta ignored page".to_owned())
        );
        let args = fs::read_to_string(render_args).unwrap();
        let args: Vec<_> = args.lines().collect();
        let page_limit = args.iter().position(|arg| *arg == "-l").unwrap();
        assert_eq!(args[page_limit + 1], "1");
        let scale_limit = args.iter().position(|arg| *arg == "-scale-to").unwrap();
        assert_eq!(args[scale_limit + 1], "1600");

        assert!(
            extract_pdf_pages_ocr_with_tools(b"four-page PDF", &[1, 2, 3, 4], &renderer, &ocr,)
                .is_none()
        );

        let slow = tools.0.join("slow-tool");
        fs::write(&slow, "#!/bin/sh\nwhile :; do :; done\n").unwrap();
        let mut permissions = fs::metadata(&slow).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&slow, permissions).unwrap();
        assert!(!super::run_find_ocr_command(
            &mut std::process::Command::new(slow),
            std::time::Duration::from_millis(50),
        ));
    }

    #[cfg(unix)]
    #[test]
    fn mixed_pdf_ocr_adds_image_page_text_without_losing_native_text() {
        use super::{extract_bounded_pdf_text_with_tools, FindOcrTempDir};
        use std::{fs, os::unix::fs::PermissionsExt};

        let tools = FindOcrTempDir::create().unwrap();
        let renderer = tools.0.join("pdftoppm");
        let ocr = tools.0.join("tesseract");
        fs::write(
            &renderer,
            "#!/bin/sh\nset -eu\nprefix=\nfor arg in \"$@\"; do prefix=\"$arg\"; done\nprintf image > \"$prefix-1.png\"\nprintf image > \"$prefix-2.png\"\n",
        )
        .unwrap();
        fs::write(
            &ocr,
            "#!/bin/sh\nset -eu\ncase \"$1\" in *page-2.png) text='scanned alpha';; *) text='wrong page';; esac\nprintf '%s\\n' \"$text\" > \"$2.txt\"\n",
        )
        .unwrap();
        for tool in [&renderer, &ocr] {
            let mut permissions = fs::metadata(tool).unwrap().permissions();
            permissions.set_mode(0o700);
            fs::set_permissions(tool, permissions).unwrap();
        }

        let pdf = test_pdf_with_mixed_text_and_image_page("native bravo");
        let text = extract_bounded_pdf_text_with_tools(&pdf, true, &renderer, &ocr)
            .expect("native and scanned page text should both be available");
        assert!(text.contains("native bravo"), "{text}");
        assert!(text.contains("scanned alpha"), "{text}");
        assert!(!text.contains("wrong page"), "{text}");
    }

    #[test]
    #[ignore = "requires Poppler and Tesseract OCR runtime packages"]
    fn scanned_pdf_attachment_ocr_extracts_page_text() {
        let pdf = include_bytes!("../../tests/fixtures/scanned-text.pdf");
        let text = extract_find_attachment_text("application/pdf", pdf)
            .expect("scanned PDF should produce OCR text");
        assert!(text.contains("scanned"), "{text}");
        assert!(text.contains("alpha"), "{text}");
        assert!(text.contains("beta"), "{text}");

        let mixed = include_bytes!("../../tests/fixtures/mixed-native-scanned.pdf");
        let text = extract_find_attachment_text("application/pdf", mixed)
            .expect("mixed native/scanned PDF should extract both pages");
        assert!(text.contains("native bravo"), "{text}");
        assert!(text.contains("scanned"), "{text}");
        assert!(text.contains("alpha"), "{text}");
        assert!(text.contains("beta"), "{text}");
    }

    #[test]
    fn find_extracted_and_normalized_text_never_exceeds_limit() {
        let limit = MAX_FIND_ATTACHMENT_TEXT_BYTES as usize;
        let exact = "x".repeat(limit);
        assert_eq!(normalize_find_attachment_text(&exact).unwrap().len(), limit);
        assert!(normalize_find_attachment_text(&format!("{exact} x")).is_none());

        let xml = format!("<root>{}</root>", "x".repeat(limit - 2));
        assert_eq!(extract_xml_text(xml.as_bytes()).unwrap().len(), limit);
        let oversized_xml = format!("<root>{}</root>", "x".repeat(limit - 1));
        assert!(extract_xml_text(oversized_xml.as_bytes()).is_none());

        let pdf = test_pdf_with_text(&"x".repeat(limit + 1));
        assert!(extract_bounded_pdf_text_with_ocr(&pdf, true).is_none());
    }

    #[test]
    fn legacy_xls_find_is_bounded_against_large_declared_ranges() {
        assert!(is_extractable_attachment_type("application/vnd.ms-excel"));
        let fixture = include_bytes!("../../tests/fixtures/legacy-xls.xls");
        assert_eq!(
            extract_find_attachment_text("application/vnd.ms-excel", fixture),
            Some("aa bb aa bb 88 bb".to_owned())
        );

        // XLS Dimensions records can claim the entire 65,536 by 256 cell
        // grid while containing only a few values. Calamine would otherwise
        // reserve a dense range from that declaration before reading cells.
        let mut expanded = fixture.to_vec();
        let mut dimensions_records = 0;
        for index in 0..expanded.len().saturating_sub(18) {
            if expanded[index..index + 4] == [0x00, 0x02, 0x0e, 0x00] {
                expanded[index + 8..index + 12].copy_from_slice(&65_536u32.to_le_bytes());
                expanded[index + 14..index + 16].copy_from_slice(&256u16.to_le_bytes());
                dimensions_records += 1;
            }
        }
        assert!(
            dimensions_records > 0,
            "fixture has worksheet Dimensions records"
        );
        assert_eq!(
            extract_find_attachment_text("application/vnd.ms-excel", &expanded),
            Some("aa bb aa bb 88 bb".to_owned())
        );
    }

    #[test]
    fn legacy_ppt_find_extracts_slide_text_with_bounded_limits() {
        use std::io::{Cursor, Write};

        fn record(version: u16, record_type: u16, payload: &[u8]) -> Vec<u8> {
            let mut bytes = Vec::with_capacity(payload.len() + 8);
            bytes.extend_from_slice(&version.to_le_bytes());
            bytes.extend_from_slice(&record_type.to_le_bytes());
            bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            bytes.extend_from_slice(payload);
            bytes
        }

        let text: Vec<u8> = "Legacy powerpoint phrase"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        let slide_text = record(0, 4000, &text);
        let slide = record(0x000f, 1006, &slide_text);
        let mut compound = cfb::CompoundFile::create(Cursor::new(Vec::new())).unwrap();
        compound
            .create_stream("/PowerPoint Document")
            .unwrap()
            .write_all(&slide)
            .unwrap();
        let bytes = compound.into_inner().into_inner();

        assert!(is_extractable_attachment_type(
            "application/vnd.ms-powerpoint"
        ));
        assert_eq!(
            extract_find_attachment_text("application/vnd.ms-powerpoint", &bytes),
            Some("Legacy powerpoint phrase".to_owned())
        );
        assert!(
            extract_legacy_ppt_text(&vec![0; MAX_FIND_ATTACHMENT_DOWNLOAD_BYTES as usize + 1])
                .is_none()
        );
    }

    #[test]
    fn legacy_doc_find_extracts_piece_table_text_with_stream_limits() {
        assert!(is_extractable_attachment_type("application/msword"));
        let fixture = test_legacy_doc_attachment("lima mike in old Word");
        assert_eq!(
            extract_find_attachment_text("application/msword", &fixture),
            Some("lima mike in old Word".to_owned())
        );
        assert!(
            extract_legacy_doc_text(&vec![0; MAX_FIND_ATTACHMENT_DOWNLOAD_BYTES as usize + 1])
                .is_none()
        );
        assert!(extract_legacy_doc_text(b"not an OLE document").is_none());

        // The CFB directory can claim a stream larger than the received file.
        // Reject its declared length before allocating or reading that stream.
        let mut oversized_stream = fixture.clone();
        let word_name: Vec<u8> = "WordDocument\0"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        let entry = oversized_stream
            .windows(word_name.len())
            .position(|bytes| bytes == word_name)
            .expect("WordDocument directory entry");
        oversized_stream[entry + 120..entry + 124].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(extract_legacy_doc_text(&oversized_stream).is_none());
    }

    fn test_pdf_with_text(text: &str) -> Vec<u8> {
        let stream = format!("BT /F1 12 Tf 72 720 Td ({text}) Tj ET");
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_owned(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>".to_owned(),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_owned(),
            format!("<< /Length {} >>\nstream\n{}\nendstream", stream.len(), stream),
        ];
        let mut pdf = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (index, object) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.extend_from_slice(format!("{} 0 obj\n{}\nendobj\n", index + 1, object).as_bytes());
        }
        let xref = pdf.len();
        pdf.extend_from_slice(
            format!("xref\n0 {}\n0000000000 65535 f \n", offsets.len() + 1).as_bytes(),
        );
        for offset in offsets {
            pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objects.len() + 1
            )
            .as_bytes(),
        );
        pdf
    }

    fn test_pdf_with_mixed_text_and_image_page(text: &str) -> Vec<u8> {
        let stream = format!("BT /F1 12 Tf 72 720 Td ({text}) Tj ET");
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
            "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>".to_owned(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 6 0 R >> >> /Contents 5 0 R >>".to_owned(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>".to_owned(),
            format!("<< /Length {} >>\nstream\n{}\nendstream", stream.len(), stream),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_owned(),
        ];
        let mut pdf = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (index, object) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.extend_from_slice(format!("{} 0 obj\n{}\nendobj\n", index + 1, object).as_bytes());
        }
        let xref = pdf.len();
        pdf.extend_from_slice(
            format!("xref\n0 {}\n0000000000 65535 f \n", offsets.len() + 1).as_bytes(),
        );
        for offset in offsets {
            pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objects.len() + 1
            )
            .as_bytes(),
        );
        pdf
    }

    fn test_office_archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
        use std::io::Write;
        let mut archive = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (name, data) in entries {
            archive
                .start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            archive.write_all(data).unwrap();
        }
        archive.finish().unwrap().into_inner()
    }

    #[test]
    fn kql_prefix_queries_match_only_word_prefixes() {
        let (kind, terms) = query::phrases_for_postfilter("subject:serv*").unwrap();
        assert!(
            email_matches_phrases("Server outage", None, None, None, None, kind, &terms,).unwrap()
        );
        assert!(
            !email_matches_phrases("observe outage", None, None, None, None, kind, &terms,)
                .unwrap()
        );
        assert!(
            email_matches_phrases("Service outage", None, None, None, None, kind, &terms,).unwrap()
        );
    }

    #[test]
    fn kql_negated_prefix_excludes_only_matching_word_prefixes() {
        for input in ["-subject:serv*", "NOT subject:serv*"] {
            let (kind, terms) = query::phrases_for_postfilter(input).unwrap();
            assert!(
                !email_matches_phrases("Server outage", None, None, None, None, kind, &terms,)
                    .unwrap()
            );
            assert!(
                email_matches_phrases("Observe outage", None, None, None, None, kind, &terms,)
                    .unwrap()
            );
        }
    }

    #[test]
    fn kql_negated_prefix_preserves_boolean_or_semantics() {
        let (kind, terms) = query::phrases_for_postfilter("-subject:serv* OR isread:true").unwrap();
        assert!(
            email_matches_phrases("Server outage", None, Some(true), None, None, kind, &terms,)
                .unwrap()
        );
        assert!(!email_matches_phrases(
            "Server outage",
            None,
            Some(false),
            None,
            None,
            kind,
            &terms,
        )
        .unwrap());
        assert!(email_matches_phrases(
            "Observe outage",
            None,
            Some(false),
            None,
            None,
            kind,
            &terms,
        )
        .unwrap());
    }

    #[test]
    fn boolean_text_operators_filter_phrase_expressions_exactly() {
        for (input, expected) in [
            (r#"subject:"alpha beta" AND ALL(alpha beta)"#, true),
            (r#"subject:"alpha beta" AND ANY(alpha gamma)"#, true),
            (r#"subject:"alpha beta" AND NONE(gamma delta)"#, true),
            (r#"subject:"beta alpha" AND ALL(alpha beta)"#, false),
            (r#"subject:"alpha beta" AND NONE(beta)"#, false),
        ] {
            let (kind, terms) = query::phrases_for_postfilter(input).unwrap();
            assert_eq!(
                email_matches_phrases("alpha beta", None, None, None, None, kind, &terms).unwrap(),
                expected,
                "{input}"
            );
        }
    }

    #[test]
    fn words_synonyms_keep_quoted_phrase_semantics_inside_exact_expressions() {
        let input = r#"subject:"alpha beta" AND WORDS("gamma delta", epsilon)"#;
        let (kind, terms) = query::phrases_for_postfilter(input).unwrap();
        assert!(email_matches_phrases(
            "alpha beta gamma delta",
            None,
            None,
            None,
            None,
            kind,
            &terms,
        )
        .unwrap());
        assert!(
            email_matches_phrases("alpha beta epsilon", None, None, None, None, kind, &terms,)
                .unwrap()
        );
        assert!(!email_matches_phrases(
            "alpha beta delta gamma",
            None,
            None,
            None,
            None,
            kind,
            &terms,
        )
        .unwrap());
    }

    #[test]
    fn kql_prefix_queries_keep_boolean_and_or_meaning() {
        let (and_kind, and_terms) =
            query::phrases_for_postfilter("subject:serv* AND isread:true").unwrap();
        assert!(email_matches_phrases(
            "Server outage",
            None,
            Some(true),
            None,
            None,
            and_kind,
            &and_terms,
        )
        .unwrap());
        assert!(!email_matches_phrases(
            "Server outage",
            None,
            Some(false),
            None,
            None,
            and_kind,
            &and_terms,
        )
        .unwrap());
        assert!(!email_matches_phrases(
            "Unrelated outage",
            None,
            Some(true),
            None,
            None,
            and_kind,
            &and_terms,
        )
        .unwrap());

        let (or_kind, or_terms) =
            query::phrases_for_postfilter("subject:serv* OR isread:true").unwrap();
        assert!(email_matches_phrases(
            "Server outage",
            None,
            Some(false),
            None,
            None,
            or_kind,
            &or_terms,
        )
        .unwrap());
        assert!(email_matches_phrases(
            "Unrelated outage",
            None,
            Some(true),
            None,
            None,
            or_kind,
            &or_terms,
        )
        .unwrap());
        assert!(!email_matches_phrases(
            "Unrelated outage",
            None,
            Some(false),
            None,
            None,
            or_kind,
            &or_terms,
        )
        .unwrap());
    }

    #[test]
    fn body_phrase_fails_closed_on_truncated_body_and_handles_missing_body() {
        let truncated = EmailBody {
            body_type: EmailBodyType::Plain,
            value: "Alpha".to_owned(),
            is_truncated: true,
        };
        assert!(email_matches_phrase("Other", Some(&truncated), "body", "alpha beta").is_err());
        assert!(!email_matches_phrase("Other", None, "body", "alpha beta").unwrap());
    }

    #[test]
    fn proximity_filters_enforce_distance_and_order() {
        let terms = vec![
            ("$proximity-left".to_owned(), "alpha beta".to_owned()),
            ("$proximity-right".to_owned(), "gamma".to_owned()),
        ];
        let matches = |subject: &str, filter| {
            email_matches_phrases_with_addresses(
                subject,
                None,
                "",
                "",
                "",
                "",
                None,
                None,
                0,
                1,
                Some(false),
                Some(false),
                Some(false),
                filter,
                &terms,
            )
            .unwrap()
        };
        assert!(matches(
            "alpha beta x gamma",
            query::PhraseMatch::Near { max_words: 1 }
        ));
        assert!(matches(
            "gamma alpha beta",
            query::PhraseMatch::Near { max_words: 8 }
        ));
        assert!(!matches(
            "alpha beta x x gamma",
            query::PhraseMatch::Near { max_words: 1 }
        ));
        assert!(!matches(
            "gamma alpha beta",
            query::PhraseMatch::Onear { max_words: 8 }
        ));
        assert!(matches(
            "alpha beta gamma",
            query::PhraseMatch::Onear { max_words: 0 }
        ));
    }

    #[test]
    fn rfc822_attachment_extraction_stays_within_the_configured_text_limit() {
        let mut output = "x".repeat(MAX_FIND_ATTACHMENT_TEXT_BYTES as usize);
        assert!(append_find_text(&mut output, "extra").is_none());
        assert_eq!(output.len(), MAX_FIND_ATTACHMENT_TEXT_BYTES as usize);
    }

    #[test]
    fn proximity_prefix_terms_match_word_starts_only() {
        let words = proximity_words("alphabet beta alphabeta bet");
        assert_eq!(
            find_term_positions(&words, &proximity_pattern_words("alpha*")),
            vec![0, 2]
        );
        assert_eq!(
            find_term_positions(&words, &proximity_pattern_words("beta*")),
            vec![1]
        );
        assert_eq!(
            find_term_positions(&words, &proximity_pattern_words("alpha")),
            Vec::<usize>::new()
        );
    }

    #[test]
    fn unordered_proximity_allows_both_operands_to_match_the_same_token() {
        assert!(email_matches_phrases(
            "cat",
            None,
            None,
            None,
            None,
            query::PhraseMatch::Near { max_words: 0 },
            &[
                ("$proximity-left".to_owned(), "cat".to_owned()),
                ("$proximity-right".to_owned(), "cat".to_owned()),
            ],
        )
        .unwrap());
    }

    #[test]
    fn proximity_expression_can_be_combined_with_a_boolean_or() {
        let (kind, terms) =
            query::phrases_for_postfilter("\"alpha beta\" ONEAR(1) gamma OR isread:true").unwrap();
        assert_eq!(kind, query::PhraseMatch::Expression);
        assert!(email_matches_phrases(
            "unrelated",
            None,
            Some(true),
            Some(false),
            Some(false),
            kind,
            &terms,
        )
        .unwrap());
    }

    #[test]
    fn not_can_negate_a_parenthesized_proximity_expression() {
        let (kind, terms) = query::phrases_for_postfilter("NOT (alpha ONEAR(0) beta)").unwrap();
        assert!(
            !email_matches_phrases("alpha beta", None, None, None, None, kind, &terms,).unwrap()
        );
        assert!(
            email_matches_phrases("beta alpha", None, None, None, None, kind, &terms,).unwrap()
        );
    }

    #[test]
    fn chained_proximity_operators_follow_left_to_right_associativity() {
        let (kind, terms) =
            query::phrases_for_postfilter("alpha NEAR(0) beta ONEAR(1) gamma").unwrap();
        let body = EmailBody {
            body_type: EmailBodyType::Plain,
            value: "alpha beta x gamma".to_owned(),
            is_truncated: false,
        };
        assert!(
            email_matches_phrases("unrelated", Some(&body), None, None, None, kind, &terms,)
                .unwrap()
        );
        let too_far = EmailBody {
            value: "alpha beta x x gamma".to_owned(),
            ..body
        };
        assert!(!email_matches_phrases(
            "unrelated",
            Some(&too_far),
            None,
            None,
            None,
            kind,
            &terms,
        )
        .unwrap());
    }

    #[test]
    fn negated_phrase_can_be_combined_with_a_text_property_predicate() {
        let (kind, terms) =
            query::phrases_for_postfilter(r#"NOT subject:"alpha beta" AND from:alice"#).unwrap();
        assert_eq!(kind, query::PhraseMatch::All);
        assert_eq!(
            terms,
            vec![
                ("$not$subject".to_owned(), "alpha beta".to_owned()),
                ("from".to_owned(), "alice".to_owned()),
            ]
        );
        let matches = |subject: &str, from: &str| {
            email_matches_phrases_with_addresses(
                subject, None, from, "", "", "", None, None, 0, 1, None, None, None, kind, &terms,
            )
            .unwrap()
        };
        assert!(!query::subject_has_phrase(
            "Bob Example <bob@example.test>",
            "alice"
        ));
        assert!(!email_matches_phrase_with_attachments(
            "unrelated subject",
            None,
            "Bob Example <bob@example.test>",
            "",
            "",
            "",
            &[],
            "from",
            "alice",
        )
        .unwrap());
        assert!(email_matches_phrase_with_attachments(
            "unrelated subject",
            None,
            "Alice Example <alice@example.test>",
            "",
            "",
            "",
            &[],
            "text",
            "alice",
        )
        .unwrap());
        assert!(matches(
            "unrelated subject",
            "Alice Example <alice@example.test>"
        ));
        assert!(!matches("alpha beta", "Alice Example <alice@example.test>"));
        assert!(!matches(
            "unrelated subject",
            "Bob Example <bob@example.test>"
        ));
    }

    #[test]
    fn right_nested_proximity_groups_match_as_an_operand() {
        let (kind, terms) =
            query::phrases_for_postfilter("alpha ONEAR(0) (beta ONEAR(0) gamma)").unwrap();
        assert!(
            email_matches_phrases("alpha beta gamma", None, None, None, None, kind, &terms,)
                .unwrap()
        );
        assert!(
            !email_matches_phrases("beta gamma alpha", None, None, None, None, kind, &terms,)
                .unwrap()
        );
    }

    #[test]
    fn attachment_name_phrase_matches_only_attachment_filenames() {
        let (kind, terms) =
            query::phrases_for_postfilter("attachment:\"annual report.pdf\"").unwrap();
        let attachments = [crate::model::EmailAttachment {
            blob_id: "blob-1".to_owned(),
            part_id: None,
            name: "Annual Report.pdf".to_owned(),
            content_type: "application/pdf".to_owned(),
            size: 123,
            content_id: None,
            content_location: None,
            inline: false,
            extracted_text: None,
        }];
        assert!(email_matches_phrases_with_addresses_and_attachments(
            "Unrelated subject",
            None,
            "",
            "",
            "",
            "",
            None,
            None,
            123,
            1,
            Some(false),
            Some(false),
            Some(true),
            &attachments,
            kind,
            &terms,
        )
        .unwrap());
        let mut wrong_filename = attachments.clone();
        wrong_filename[0].name = "report annual.pdf".to_owned();
        assert!(!email_matches_phrases_with_addresses_and_attachments(
            "Unrelated subject",
            None,
            "",
            "",
            "",
            "",
            None,
            None,
            123,
            1,
            Some(false),
            Some(false),
            Some(true),
            &wrong_filename,
            kind,
            &terms,
        )
        .unwrap());
        let (kind, terms) = query::phrases_for_postfilter("attachment:*").unwrap();
        assert!(email_matches_phrases_with_addresses_and_attachments(
            "Unrelated subject",
            None,
            "",
            "",
            "",
            "",
            None,
            None,
            123,
            1,
            Some(false),
            Some(false),
            Some(true),
            &attachments,
            kind,
            &terms,
        )
        .unwrap());
        assert!(!email_matches_phrases_with_addresses_and_attachments(
            "Unrelated subject",
            None,
            "",
            "",
            "",
            "",
            None,
            None,
            0,
            1,
            Some(false),
            Some(false),
            Some(false),
            &[],
            kind,
            &terms,
        )
        .unwrap());
    }

    #[test]
    fn negated_phrase_can_be_combined_with_an_attachment_filename_term() {
        let (kind, terms) =
            query::phrases_for_postfilter(r#"NOT subject:"alpha beta" AND attachment:report"#)
                .unwrap();
        let attachments = [crate::model::EmailAttachment {
            blob_id: "blob-report".to_owned(),
            part_id: None,
            name: "Annual Report.pdf".to_owned(),
            content_type: "application/pdf".to_owned(),
            size: 64,
            content_id: None,
            content_location: None,
            inline: false,
            extracted_text: None,
        }];
        let matches = |subject: &str, attachments: &[crate::model::EmailAttachment]| {
            email_matches_phrases_with_addresses_and_attachments(
                subject,
                None,
                "",
                "",
                "",
                "",
                None,
                None,
                0,
                1,
                None,
                None,
                Some(!attachments.is_empty()),
                attachments,
                kind,
                &terms,
            )
            .unwrap()
        };
        assert!(matches("unrelated subject", &attachments));
        assert!(!matches("alpha beta", &attachments));
        assert!(!matches("unrelated subject", &[]));
    }

    #[test]
    fn proximity_accepts_parenthesized_or_alternatives_as_operands() {
        let (kind, terms) = query::phrases_for_postfilter("cat NEAR(0) (cat OR dog)").unwrap();
        assert!(email_matches_phrases("cat", None, None, None, None, kind, &terms).unwrap());

        let (kind, terms) = query::phrases_for_postfilter("alpha ONEAR(0) (dog OR beta)").unwrap();
        assert!(
            email_matches_phrases("alpha beta", None, None, None, None, kind, &terms,).unwrap()
        );
        assert!(
            !email_matches_phrases("beta alpha", None, None, None, None, kind, &terms,).unwrap()
        );

        let (kind, terms) = query::phrases_for_postfilter("(cat OR dog) ONEAR(0) mouse").unwrap();
        assert!(email_matches_phrases("dog mouse", None, None, None, None, kind, &terms,).unwrap());
        assert!(
            !email_matches_phrases("mouse dog", None, None, None, None, kind, &terms,).unwrap()
        );

        let (kind, terms) =
            query::phrases_for_postfilter("alpha NEAR(0) (beta OR gamma) OR isread:true").unwrap();
        assert!(
            email_matches_phrases("unrelated", None, Some(true), None, None, kind, &terms,)
                .unwrap()
        );
    }

    #[test]
    fn proximity_terms_without_indexable_words_do_not_match_or_panic() {
        let (kind, terms) = query::phrases_for_postfilter("\"!!!\" NEAR(2) alpha").unwrap();
        assert!(
            !email_matches_phrases("!!! alpha", None, None, None, None, kind, &terms,).unwrap()
        );
    }
}
