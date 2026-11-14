use super::ProtocolVersion;
use crate::{
    http_server::AppState,
    state::ItemState,
    wbxml::{
        self,
        eas::{self, airsync as air, DocumentBuilder},
        token::Token,
        Document, Node,
    },
};
use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use base64::{engine::general_purpose::STANDARD, Engine};
use std::collections::BTreeSet;

fn is(document: &Document, token: Token) -> bool {
    matches!(document.nodes.first(), Some(Node::Start(start))
        if start.code_page == token.code_page && start.token == token.token)
}

fn cache_key(version: ProtocolVersion) -> String {
    format!("__sync_request__:{}", version.as_str())
}

async fn load(
    state: &AppState,
    user: &str,
    device: &str,
    version: ProtocolVersion,
) -> anyhow::Result<Option<Document>> {
    let items = state
        .state
        .item_states(user, device, &cache_key(version))
        .await?;
    items
        .iter()
        .find(|item| item.item_id == "request")
        .map(|item| -> anyhow::Result<Document> {
            Ok(wbxml::decode_document(&STANDARD.decode(&item.hash)?)?)
        })
        .transpose()
}

fn collections(document: &Document) -> Vec<Document> {
    eas::child_elements(document)
        .iter()
        .find(|child| is(child, air::COLLECTIONS))
        .map(eas::child_elements)
        .unwrap_or_default()
}

fn assemble(root: Vec<Document>, folders: Vec<Document>, strip_commands: bool) -> Document {
    let mut b = DocumentBuilder::new();
    b.start(air::SYNC);
    for child in root {
        b.extend(child.nodes);
    }
    b.start(air::COLLECTIONS);
    for folder in folders {
        if strip_commands {
            b.start(air::COLLECTION);
            for child in eas::child_elements(&folder) {
                if !is(&child, air::COMMANDS) {
                    b.extend(child.nodes);
                }
            }
            b.end();
        } else {
            b.extend(folder.nodes);
        }
    }
    b.end();
    b.end();
    b.finish()
}

fn root_settings(document: &Document) -> Vec<Document> {
    eas::child_elements(document)
        .into_iter()
        .filter(|child| {
            [air::WINDOW_SIZE, air::WAIT, air::HEARTBEAT_INTERVAL]
                .iter()
                .any(|token| is(child, *token))
        })
        .collect()
}

pub(super) async fn resolve(
    state: &AppState,
    user: &str,
    device: &str,
    version: ProtocolVersion,
    request: &Document,
) -> Result<Document, &'static str> {
    let empty = request.nodes.is_empty();
    if !empty && !is(request, air::SYNC) {
        return Err("4");
    }
    let children = eas::child_elements(request);
    let partial_count = children
        .iter()
        .filter(|child| is(child, air::PARTIAL))
        .count();
    let partial = partial_count > 0;
    if partial_count > 1
        || children
            .iter()
            .filter(|child| is(child, air::COLLECTIONS))
            .count()
            > 1
    {
        return Err("4");
    }
    if partial && children.len() == 1 {
        return Err("4");
    }
    let mut folders = collections(request);
    let mut root = root_settings(request);
    if empty || partial {
        let cached = load(state, user, device, version)
            .await
            .map_err(|error| {
                tracing::warn!(%error, "failed to load cached Sync request");
                "5"
            })?
            .ok_or("13")?;
        if empty {
            return Ok(cached);
        }
        let ids: BTreeSet<_> = folders
            .iter()
            .filter_map(|folder| {
                eas::find_text_after(folder, air::COLLECTION_ID).map(str::to_owned)
            })
            .collect();
        for folder in collections(&cached) {
            if !eas::find_text_after(&folder, air::COLLECTION_ID).is_some_and(|id| ids.contains(id))
            {
                folders.push(folder);
            }
        }
        for child in root_settings(&cached) {
            if !root.iter().any(|current| matches!((current.nodes.first(), child.nodes.first()),
                (Some(Node::Start(a)), Some(Node::Start(b))) if a.token == b.token && a.code_page == b.code_page))
            { root.push(child); }
        }
    }
    let mut ids = BTreeSet::new();
    for folder in &folders {
        if !is(folder, air::COLLECTION) {
            return Err("4");
        }
        let id = eas::find_text_after(folder, air::COLLECTION_ID)
            .filter(|id| !id.is_empty())
            .ok_or("4")?;
        if !ids.insert(id) {
            return Err("4");
        }
    }
    if folders.is_empty() {
        return Err("4");
    }
    let resolved = assemble(root, folders, false);
    if wbxml::encode_document(&resolved).len() > state.config.max_wbxml_bytes {
        return Err("15");
    }
    Ok(resolved)
}

pub(super) async fn save(
    state: &AppState,
    user: &str,
    device: &str,
    version: ProtocolVersion,
    request: &Document,
    reply: &Document,
) -> anyhow::Result<()> {
    let replies = collections(reply);
    if replies.is_empty()
        || replies
            .iter()
            .any(|folder| eas::find_text_after(folder, air::STATUS) != Some("1"))
    {
        return Ok(());
    }
    let cached = assemble(root_settings(request), collections(request), true);
    state
        .state
        .put_item_states(
            user,
            device,
            &cache_key(version),
            vec![ItemState {
                item_id: "request".to_owned(),
                hash: STANDARD.encode(wbxml::encode_document(&cached)),
            }],
        )
        .await
}

pub(super) fn error(state: &AppState, command: &str, status: &str) -> Response {
    let mut b = DocumentBuilder::new();
    b.start(air::SYNC);
    b.leaf(air::STATUS, status);
    b.end();
    state
        .metrics
        .eas_requests_total
        .with_label_values(&[command, "200"])
        .inc();
    (
        StatusCode::OK,
        [("Content-Type", "application/vnd.ms-sync.wbxml")],
        wbxml::encode_document(&b.finish()),
    )
        .into_response()
}
