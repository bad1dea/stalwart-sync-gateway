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
    let filter = json!({"operator":"AND", "conditions":[query, folder_filter]});
    let (phrase_match, phrases) = phrase_filter;
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
        "from",
        "to",
        "cc",
        "bcc",
        "preview",
        "attachments",
    ];
    if phrases.iter().any(|(property, _)| property != "subject") {
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
                if email_matches_phrases(
                    &email.subject,
                    email.body.as_ref(),
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
    phrase_match: query::PhraseMatch,
    phrases: &[(String, String)],
) -> anyhow::Result<bool> {
    match phrase_match {
        query::PhraseMatch::All => {
            for (property, phrase) in phrases {
                if !email_matches_phrase(subject, body, property, phrase)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        query::PhraseMatch::Any => {
            for (property, phrase) in phrases {
                if email_matches_phrase(subject, body, property, phrase)? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
    }
}

fn email_matches_phrase(
    subject: &str,
    body: Option<&crate::model::EmailBody>,
    property: &str,
    phrase: &str,
) -> anyhow::Result<bool> {
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
        assert!(email_matches_phrases(
            "Alpha beta subject",
            Some(&plain),
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
            query::PhraseMatch::All,
            &[
                ("subject".to_owned(), "alpha beta".to_owned()),
                ("subject".to_owned(), "gamma delta".to_owned())
            ]
        )
        .unwrap());
        assert!(email_matches_phrases(
            "Other subject",
            Some(&plain),
            query::PhraseMatch::Any,
            &[
                ("subject".to_owned(), "alpha beta".to_owned()),
                ("body".to_owned(), "alpha beta".to_owned())
            ]
        )
        .unwrap());
        assert!(!email_matches_phrases(
            "Other subject",
            None,
            query::PhraseMatch::Any,
            &[
                ("subject".to_owned(), "alpha beta".to_owned()),
                ("text".to_owned(), "alpha beta".to_owned())
            ]
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
}
