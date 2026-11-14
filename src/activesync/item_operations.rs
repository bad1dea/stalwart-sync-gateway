use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use base64::{engine::general_purpose::STANDARD, Engine};

use super::{write_email_fields, BodyPreference};
use crate::{
    http_server::AppState,
    jmap::client::AuthenticatedSession,
    wbxml::{
        self,
        eas::{
            self, airsync as air, airsync_base as base, item_operations as ops, DocumentBuilder,
        },
    },
};

pub(super) async fn handle(
    state: &AppState,
    auth: &AuthenticatedSession,
    document: &wbxml::Document,
    command: &str,
) -> Response {
    let operations = eas::child_elements(document);
    let mut builder = DocumentBuilder::new();
    builder.start(ops::ITEM_OPERATIONS);
    builder.leaf(ops::STATUS, if operations.is_empty() { "2" } else { "1" });
    if !operations.is_empty() {
        builder.start(ops::RESPONSE);
        for operation in operations {
            let Some(wbxml::Node::Start(kind)) = operation.nodes.first() else {
                continue;
            };
            builder.start(*kind);
            if kind.code_page != ops::PAGE || kind.token != ops::FETCH.token {
                builder.leaf(ops::STATUS, "2");
            } else if let Some(fetch) = eas::item_operations_fetch(&operation) {
                fetch_item(state, auth, &mut builder, fetch).await;
            } else {
                builder.leaf(ops::STATUS, "2");
            }
            builder.end();
        }
        builder.end();
    }
    builder.end();
    state
        .metrics
        .eas_requests_total
        .with_label_values(&[command, "200"])
        .inc();
    (
        StatusCode::OK,
        [("Content-Type", "application/vnd.ms-sync.wbxml")],
        wbxml::encode_document(&builder.finish()),
    )
        .into_response()
}

async fn fetch_item(
    state: &AppState,
    auth: &AuthenticatedSession,
    builder: &mut DocumentBuilder,
    fetch: eas::ItemOperationsFetchRequest,
) {
    if !fetch.store.eq_ignore_ascii_case("Mailbox") {
        builder.leaf(ops::STATUS, "9");
        return;
    }
    if let Some(reference) = fetch.file_reference {
        let Some((blob_id, name)) = reference.split_once("||") else {
            builder.leaf(ops::STATUS, "15");
            return;
        };
        match state
            .jmap
            .download_blob_with_type(auth, blob_id, name)
            .await
        {
            Ok((data, content_type)) => {
                let slice = match fetch.range.as_deref() {
                    Some(range) => match byte_range(range, data.len()) {
                        Some(range) => range,
                        None => {
                            builder.leaf(ops::STATUS, "8");
                            return;
                        }
                    },
                    None => 0..data.len(),
                };
                builder.leaf(ops::STATUS, "1");
                builder.leaf(base::FILE_REFERENCE, reference);
                builder.start(ops::PROPERTIES);
                builder.leaf(base::CONTENT_TYPE, content_type);
                if fetch.range.is_some() {
                    builder.leaf(ops::RANGE, format!("{}-{}", slice.start, slice.end - 1));
                    builder.leaf(ops::TOTAL, data.len().to_string());
                }
                builder.leaf(ops::DATA, STANDARD.encode(&data[slice]));
                builder.end();
            }
            Err(error) => {
                tracing::warn!(%error, "ItemOperations attachment fetch failed");
                builder.leaf(ops::STATUS, "15");
            }
        }
        return;
    }
    let target = fetch
        .long_id
        .as_deref()
        .or(fetch.server_id.as_deref())
        .unwrap();
    match state.jmap.get_email_by_id(auth, target).await {
        Ok(Some(email)) => {
            let mime = if fetch.body_pref_type == Some(4) {
                let Some(blob_id) = email.blob_id.as_deref() else {
                    builder.leaf(ops::STATUS, "14");
                    return;
                };
                match state.jmap.download_blob(auth, blob_id, "message.eml").await {
                    Ok(data) => Some(bytes::Bytes::from(data)),
                    Err(error) => {
                        tracing::warn!(%error, "ItemOperations MIME fetch failed");
                        builder.leaf(ops::STATUS, "14");
                        return;
                    }
                }
            } else {
                None
            };
            builder.leaf(ops::STATUS, "1");
            if let Some(id) = fetch.collection_id {
                builder.leaf(air::COLLECTION_ID, id);
            }
            if let Some(id) = fetch.long_id.as_deref() {
                builder.leaf(eas::search::LONG_ID, id);
            } else {
                builder.leaf(air::SERVER_ID, target);
            }
            builder.start(ops::PROPERTIES);
            let preference = fetch.body_pref_type.map(|body_type| BodyPreference {
                body_type: Some(body_type),
                truncation_size: fetch.body_pref_truncation_size,
                mime_data: mime,
            });
            write_email_fields(builder, target, email, preference);
            builder.end();
        }
        Ok(None) => {
            builder.leaf(ops::STATUS, "6");
        }
        Err(error) => {
            tracing::warn!(%error, "ItemOperations message fetch failed");
            builder.leaf(ops::STATUS, "3");
        }
    }
}

fn byte_range(range: &str, length: usize) -> Option<std::ops::Range<usize>> {
    let (start, end) = range.split_once('-')?;
    let start: usize = start.parse().ok()?;
    let end: usize = end.parse().ok()?;
    if start > end || start >= length {
        return None;
    }
    Some(start..end.saturating_add(1).min(length))
}
