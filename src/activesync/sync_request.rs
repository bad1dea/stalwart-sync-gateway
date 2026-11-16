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

pub(super) fn global_window(request: &Document) -> Result<usize, &'static str> {
    let values: Vec<_> = eas::child_elements(request)
        .into_iter()
        .filter(|child| is(child, air::WINDOW_SIZE))
        .collect();
    if values.len() > 1 {
        return Err("4");
    }
    match values.first() {
        None => Ok(100),
        Some(value) => {
            let size = eas::find_text_after(value, air::WINDOW_SIZE)
                .and_then(|text| text.parse::<usize>().ok())
                .ok_or("4")?;
            Ok(if size == 0 { 512 } else { size.min(512) })
        }
    }
}

pub(super) fn response_changes(nodes: Vec<Node>) -> usize {
    let response = Document { nodes };
    eas::child_elements(&response)
        .into_iter()
        .filter(|child| is(child, air::COMMANDS))
        .map(|commands| eas::child_elements(&commands).len())
        .sum()
}

pub(super) fn empty_timeout_response(request: &Document, response: &Document) -> bool {
    let requested = eas::sync_collections(request);
    let replies = collections(response);
    if requested.is_empty() || requested.len() != replies.len() {
        return false;
    }
    requested.iter().all(|collection| {
        if !collection.commands.is_empty() || collection.sync_key == "0" {
            return false;
        }
        let Some(reply) = replies.iter().find(|reply| {
            eas::find_text_after(reply, air::COLLECTION_ID)
                == Some(collection.collection_id.as_str())
        }) else {
            return false;
        };
        let children = eas::child_elements(reply);
        children.len() == 3
            && eas::find_text_after(reply, air::SYNC_KEY) == Some(collection.sync_key.as_str())
            && eas::find_text_after(reply, air::STATUS) == Some("1")
            && children.iter().all(|child| {
                [air::COLLECTION_ID, air::SYNC_KEY, air::STATUS]
                    .iter()
                    .any(|token| is(child, *token))
            })
    })
}

pub(super) fn wait_seconds(request: &Document) -> Result<Option<u64>, (&'static str, Option<u64>)> {
    let values: Vec<_> = eas::child_elements(request)
        .into_iter()
        .filter(|child| is(child, air::WAIT) || is(child, air::HEARTBEAT_INTERVAL))
        .collect();
    if values.len() > 1 {
        return Err(("4", None));
    }
    let Some(value) = values.first() else {
        return Ok(None);
    };
    let minutes = is(value, air::WAIT);
    let token = if minutes {
        air::WAIT
    } else {
        air::HEARTBEAT_INTERVAL
    };
    let number = eas::find_text_after(value, token)
        .and_then(|text| text.parse::<i64>().ok())
        .ok_or(("4", None))?;
    let (min, max) = if minutes { (1, 59) } else { (60, 3540) };
    if number < min || number > max {
        return Err(("14", Some(number.clamp(min, max) as u64)));
    }
    Ok(Some(if minutes {
        number as u64 * 60
    } else {
        number as u64
    }))
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
            if (is(&child, air::WAIT) || is(&child, air::HEARTBEAT_INTERVAL))
                && root
                    .iter()
                    .any(|current| is(current, air::WAIT) || is(current, air::HEARTBEAT_INTERVAL))
            {
                continue;
            }
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
    error_with_limit(state, command, status, None)
}

pub(super) fn error_with_limit(
    state: &AppState,
    command: &str,
    status: &str,
    limit: Option<u64>,
) -> Response {
    let mut b = DocumentBuilder::new();
    b.start(air::SYNC);
    b.leaf(air::STATUS, status);
    if let Some(limit) = limit {
        b.leaf(air::LIMIT, limit.to_string());
    }
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

#[cfg(test)]
mod wait_tests {
    use super::*;
    fn request(values: &[(Token, &str)]) -> Document {
        let mut b = DocumentBuilder::new();
        b.start(air::SYNC);
        for (token, text) in values {
            b.leaf(*token, *text);
        }
        b.end();
        b.finish()
    }
    #[test]
    fn validates_global_window() {
        assert_eq!(global_window(&request(&[])), Ok(100));
        assert_eq!(global_window(&request(&[(air::WINDOW_SIZE, "0")])), Ok(512));
        assert_eq!(global_window(&request(&[(air::WINDOW_SIZE, "3")])), Ok(3));
        assert_eq!(
            global_window(&request(&[(air::WINDOW_SIZE, "900")])),
            Ok(512)
        );
        assert_eq!(
            global_window(&request(&[(air::WINDOW_SIZE, "bad")])),
            Err("4")
        );
        assert_eq!(
            global_window(&request(&[
                (air::WINDOW_SIZE, "1"),
                (air::WINDOW_SIZE, "2")
            ])),
            Err("4")
        );
    }

    #[test]
    fn only_unchanged_successful_collections_can_have_an_empty_timeout_response() {
        fn collection(key: &str, response: bool, changed: bool) -> Document {
            let mut b = DocumentBuilder::new();
            b.start(air::SYNC);
            b.start(air::COLLECTIONS);
            b.start(air::COLLECTION);
            b.leaf(air::SYNC_KEY, key);
            b.leaf(air::COLLECTION_ID, "inbox");
            if response {
                b.leaf(air::STATUS, "1");
            }
            if changed {
                b.start(air::COMMANDS);
                b.start(air::ADD);
                b.leaf(air::SERVER_ID, "mail");
                b.end();
                b.end();
            }
            b.end();
            b.end();
            b.end();
            b.finish()
        }
        let request = collection("2", false, false);
        assert!(empty_timeout_response(
            &request,
            &collection("2", true, false)
        ));
        assert!(!empty_timeout_response(
            &request,
            &collection("3", true, false)
        ));
        assert!(!empty_timeout_response(
            &request,
            &collection("2", true, true)
        ));
        assert!(!empty_timeout_response(
            &collection("2", false, true),
            &collection("2", true, false)
        ));
        assert!(!empty_timeout_response(
            &collection("0", false, false),
            &collection("0", true, false)
        ));
    }

    #[test]
    fn validates_sync_wait_interval_and_limit() {
        assert_eq!(wait_seconds(&request(&[])), Ok(None));
        assert_eq!(wait_seconds(&request(&[(air::WAIT, "1")])), Ok(Some(60)));
        assert_eq!(
            wait_seconds(&request(&[(air::HEARTBEAT_INTERVAL, "3540")])),
            Ok(Some(3540))
        );
        assert_eq!(
            wait_seconds(&request(&[(air::WAIT, "0")])),
            Err(("14", Some(1)))
        );
        assert_eq!(
            wait_seconds(&request(&[(air::WAIT, "-1")])),
            Err(("14", Some(1)))
        );
        assert_eq!(
            wait_seconds(&request(&[(air::WAIT, "60")])),
            Err(("14", Some(59)))
        );
        assert_eq!(
            wait_seconds(&request(&[(air::HEARTBEAT_INTERVAL, "59")])),
            Err(("14", Some(60)))
        );
        assert_eq!(
            wait_seconds(&request(&[(air::HEARTBEAT_INTERVAL, "3541")])),
            Err(("14", Some(3540)))
        );
        assert_eq!(
            wait_seconds(&request(&[
                (air::WAIT, "1"),
                (air::HEARTBEAT_INTERVAL, "60")
            ])),
            Err(("4", None))
        );
        assert_eq!(
            wait_seconds(&request(&[(air::WAIT, "bad")])),
            Err(("4", None))
        );
    }
}
