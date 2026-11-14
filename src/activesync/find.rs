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
            self, airsync as air, email as mail, email2, find as f, item_operations as ops,
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
    let criteria =
        child(&execute, f::MAILBOX_SEARCH_CRITERION).ok_or("unsupported search criterion")?;
    let query = child(&criteria, f::QUERY).ok_or("missing Query")?;
    if eas::find_text_after(&query, air::CLASS) != Some("Email") {
        return Err("unsupported search class");
    }
    let text = eas::find_text_after(&query, f::FREE_TEXT)
        .filter(|text| !text.trim().is_empty())
        .ok_or("missing FreeText")?;
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
    let filter = match query::to_filter(&request.text) {
        Ok(filter) => filter,
        Err(_) => return error_response(state, "2"),
    };
    let reply = execute(state, auth, device, request, filter).await;
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
    let page = state
        .jmap
        .query_emails(
            auth,
            filter.clone(),
            request.start as i64,
            request.end - request.start + 1,
            &[
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
            ],
        )
        .await?;
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
