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
    query_state: String,
    created: i64,
}
const SEARCHES: &str = "__find__";

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
    // A negated phrase cannot use JMAP's term filter as its candidate set:
    // the backend does not preserve phrase semantics, and NOT would exclude
    // messages that do not contain all indexed phrase terms. Boolean terms,
    // including negated ones, remain exact JMAP predicates, so retaining the
    // query preserves a smaller safe candidate set for positive phrase forms.
    let query = if phrase_match == query::PhraseMatch::None
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
            .is_none_or(|session| session.filter != filter)
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
            for email in candidate.emails {
                if email_matches_phrases_with_addresses_and_attachments(
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
                )? {
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

pub(super) fn matching_principals(
    mut principals: Vec<crate::jmap::client::PrincipalResult>,
    term: &str,
) -> Vec<crate::jmap::client::PrincipalResult> {
    let term = term.to_lowercase();
    principals.retain(|principal| {
        let display = principal.display_name.to_lowercase();
        display.starts_with(&term)
            || display
                .split_whitespace()
                .any(|part| part.starts_with(&term))
            || principal.alias.to_lowercase().starts_with(&term)
            || principal
                .email
                .as_ref()
                .is_some_and(|email| email.to_lowercase().starts_with(&term))
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
        && previous
            .as_ref()
            .is_none_or(|session| session.filter != filter || session.query_state != query_state)
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
            enum ExpressionValue {
                Boolean(bool),
                ProximityTerm(String),
            }
            let mut values = Vec::new();
            for (property, phrase) in phrases {
                match property.as_str() {
                    "$op$and" | "$op$or" => {
                        let ExpressionValue::Boolean(right) = values
                            .pop()
                            .ok_or_else(|| anyhow::anyhow!("invalid phrase expression"))?
                        else {
                            anyhow::bail!("invalid phrase expression");
                        };
                        let ExpressionValue::Boolean(left) = values
                            .pop()
                            .ok_or_else(|| anyhow::anyhow!("invalid phrase expression"))?
                        else {
                            anyhow::bail!("invalid phrase expression");
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
                    let ExpressionValue::ProximityTerm(right) = values
                        .pop()
                        .ok_or_else(|| anyhow::anyhow!("invalid proximity expression"))?
                    else {
                        anyhow::bail!("invalid proximity expression");
                    };
                    let ExpressionValue::ProximityTerm(left) = values
                        .pop()
                        .ok_or_else(|| anyhow::anyhow!("invalid proximity expression"))?
                    else {
                        anyhow::bail!("invalid proximity expression");
                    };
                    let matched = proximity_matches_email(
                        subject,
                        body,
                        from,
                        to,
                        cc,
                        bcc,
                        attachments,
                        false,
                        max_words,
                        &[
                            ("$proximity-left".to_owned(), left),
                            ("$proximity-right".to_owned(), right),
                        ],
                    )?;
                    values.push(ExpressionValue::Boolean(matched));
                } else if let Some(operator) = property.strip_prefix("$op$onear:") {
                    let max_words = operator.parse::<usize>()?;
                    let ExpressionValue::ProximityTerm(right) = values
                        .pop()
                        .ok_or_else(|| anyhow::anyhow!("invalid proximity expression"))?
                    else {
                        anyhow::bail!("invalid proximity expression");
                    };
                    let ExpressionValue::ProximityTerm(left) = values
                        .pop()
                        .ok_or_else(|| anyhow::anyhow!("invalid proximity expression"))?
                    else {
                        anyhow::bail!("invalid proximity expression");
                    };
                    let matched = proximity_matches_email(
                        subject,
                        body,
                        from,
                        to,
                        cc,
                        bcc,
                        attachments,
                        true,
                        max_words,
                        &[
                            ("$proximity-left".to_owned(), left),
                            ("$proximity-right".to_owned(), right),
                        ],
                    )?;
                    values.push(ExpressionValue::Boolean(matched));
                }
            }
            if values.len() != 1 {
                anyhow::bail!("invalid phrase expression");
            }
            match values.pop() {
                Some(ExpressionValue::Boolean(value)) => Ok(value),
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
    let left = proximity_words(&operands[0].1);
    let right = proximity_words(&operands[1].1);
    if left.is_empty() || right.is_empty() {
        return Ok(false);
    }
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
    for field in fields {
        let words = proximity_words(&field);
        let left_positions = find_term_positions(&words, &left);
        let right_positions = find_term_positions(&words, &right);
        if !ordered && left == right && !left_positions.is_empty() {
            return Ok(true);
        }
        for left_start in left_positions {
            let left_end = left_start + left.len();
            let next = right_positions.partition_point(|position| *position < left_end);
            if right_positions
                .get(next)
                .is_some_and(|right_start| right_start.saturating_sub(left_end) <= max_words)
            {
                return Ok(true);
            }
            if !ordered {
                if let Some(right_start) = next.checked_sub(1).map(|index| right_positions[index]) {
                    if right_start != left_start {
                        let right_end = right_start.saturating_add(right.len());
                        let gap = left_start.saturating_sub(right_end);
                        if right_end >= left_start || gap <= max_words {
                            return Ok(true);
                        }
                    }
                }
            }
        }
    }
    Ok(false)
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
        .filter_map(|(index, window)| (window == term).then_some(index))
        .collect()
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
}
