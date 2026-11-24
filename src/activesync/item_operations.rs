use anyhow::Context;
use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use base64::{engine::general_purpose::STANDARD, Engine};

use super::{
    body_preferences_for_email, write_calendar_fields, write_contact_fields, write_email_fields,
    write_note_fields, write_task_fields, ProtocolVersion,
};
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
    multipart: bool,
    version: ProtocolVersion,
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
                fetch_item(state, auth, &mut builder, fetch, version).await;
            } else {
                builder.leaf(ops::STATUS, "2");
            }
            builder.end();
        }
        builder.end();
    }
    builder.end();
    let (content_type, body) = match encode_response(builder.finish(), multipart) {
        Ok(response) => response,
        Err(error) => {
            tracing::warn!(%error, "failed to encode ItemOperations response");
            state
                .metrics
                .eas_requests_total
                .with_label_values(&[command, "500"])
                .inc();
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    state
        .metrics
        .eas_requests_total
        .with_label_values(&[command, "200"])
        .inc();
    (StatusCode::OK, [("Content-Type", content_type)], body).into_response()
}

fn encode_response(
    mut document: wbxml::Document,
    multipart: bool,
) -> anyhow::Result<(&'static str, Vec<u8>)> {
    if !multipart {
        return Ok((
            "application/vnd.ms-sync.wbxml",
            wbxml::encode_document(&document),
        ));
    }
    // [MS-ASCMD] 2.2.1.10.1.1: part zero is WBXML. Data elements
    // become one-based references into the remaining binary parts.
    let mut parts = vec![Vec::new()];
    for index in 0..document.nodes.len().saturating_sub(2) {
        let wbxml::Node::Start(token) = document.nodes[index] else {
            continue;
        };
        let attachment = token.code_page == ops::PAGE && token.token == ops::DATA.token;
        let body = token.code_page == base::PAGE && token.token == base::DATA.token;
        if !attachment && !body {
            continue;
        }
        let data = match &document.nodes[index + 1] {
            wbxml::Node::Text(value) if attachment => STANDARD.decode(value)?,
            wbxml::Node::Text(value) => value.as_bytes().to_vec(),
            wbxml::Node::Opaque(value) => value.to_vec(),
            _ => continue,
        };
        document.nodes[index] = wbxml::Node::Start(wbxml::token::Token {
            has_content: true,
            ..ops::PART
        });
        document.nodes[index + 1] = wbxml::Node::Text(parts.len().to_string());
        parts.push(data);
    }
    parts[0] = wbxml::encode_document(&document);
    let count = u32::try_from(parts.len())?;
    let mut offset = count
        .checked_mul(8)
        .and_then(|n| n.checked_add(4))
        .context("multipart directory too large")?;
    let mut response = Vec::new();
    response.extend_from_slice(&count.to_le_bytes());
    for part in &parts {
        let size = u32::try_from(part.len())?;
        response.extend_from_slice(&offset.to_le_bytes());
        response.extend_from_slice(&size.to_le_bytes());
        offset = offset
            .checked_add(size)
            .context("multipart response too large")?;
    }
    for part in parts {
        response.extend_from_slice(&part);
    }
    Ok(("application/vnd.ms-sync.multipart", response))
}

async fn fetch_item(
    state: &AppState,
    auth: &AuthenticatedSession,
    builder: &mut DocumentBuilder,
    fetch: eas::ItemOperationsFetchRequest,
    version: ProtocolVersion,
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
        let download = if let Some(calendar_blob_id) = blob_id.strip_prefix("calendar:") {
            state
                .jmap
                .download_calendar_blob_with_type(auth, calendar_blob_id, name)
                .await
        } else {
            state
                .jmap
                .download_blob_with_type(auth, blob_id, name)
                .await
        };
        match download {
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
    let note_target = if let Some(long_id) = fetch
        .long_id
        .as_deref()
        .and_then(|id| id.strip_prefix("note:"))
    {
        long_id.split_once(':')
    } else if fetch.long_id.is_none() {
        fetch
            .collection_id
            .as_deref()
            .and_then(|id| id.strip_prefix("note_"))
            .map(|id| (id, target))
    } else {
        None
    };
    if fetch
        .long_id
        .as_deref()
        .is_some_and(|id| id.starts_with("note:"))
        && note_target.is_none()
    {
        builder.leaf(ops::STATUS, "6");
        return;
    }
    if let Some((mailbox, note_id)) = note_target {
        if mailbox.is_empty()
            || note_id.is_empty()
            || note_id.contains(':')
            || fetch
                .collection_id
                .as_deref()
                .is_some_and(|folder| folder.strip_prefix("note_") != Some(mailbox))
        {
            builder.leaf(ops::STATUS, "6");
            return;
        }
        let result = async {
            let folder = format!("note_{mailbox}");
            let collections = state.jmap.collections(auth).await?;
            if !collections
                .iter()
                .any(|item| item.id == folder && item.kind == crate::model::CollectionKind::Notes)
            {
                return Ok::<_, anyhow::Error>(None);
            }
            let summaries = state.jmap.list_notes(auth, mailbox, 0).await?;
            if !summaries.iter().any(|item| item.id == note_id) {
                return Ok(None);
            }
            state.jmap.get_note(auth, note_id).await
        }
        .await;
        match result {
            Ok(Some(note)) => {
                builder.leaf(ops::STATUS, "1");
                if let Some(id) = fetch.collection_id.as_deref() {
                    builder.leaf(air::COLLECTION_ID, id);
                }
                if let Some(id) = fetch.long_id.as_deref() {
                    builder.leaf(eas::search::LONG_ID, id);
                } else {
                    builder.leaf(air::SERVER_ID, note_id);
                }
                builder.leaf(air::CLASS, "Notes");
                builder.start(ops::PROPERTIES);
                write_note_fields(builder, &note, &fetch.body_preferences);
                builder.end();
            }
            Ok(None) => builder.leaf(ops::STATUS, "6"),
            Err(error) => {
                tracing::warn!(%error, "ItemOperations note fetch failed");
                builder.leaf(ops::STATUS, "3");
            }
        }
        return;
    }
    let task_target = if let Some(long_id) = fetch
        .long_id
        .as_deref()
        .and_then(|id| id.strip_prefix("task:"))
    {
        long_id.split_once(':')
    } else if fetch.long_id.is_none() {
        fetch
            .collection_id
            .as_deref()
            .and_then(|id| id.strip_prefix("task_"))
            .map(|id| (id, target))
    } else {
        None
    };
    if fetch
        .long_id
        .as_deref()
        .is_some_and(|id| id.starts_with("task:"))
        && task_target.is_none()
    {
        builder.leaf(ops::STATUS, "6");
        return;
    }
    if let Some((calendar, task_id)) = task_target {
        if calendar.is_empty()
            || task_id.is_empty()
            || task_id.contains(':')
            || fetch
                .collection_id
                .as_deref()
                .is_some_and(|folder| folder.strip_prefix("task_") != Some(calendar))
        {
            builder.leaf(ops::STATUS, "6");
            return;
        }
        match state.jmap.tasks_in_calendar(auth, calendar, 0).await {
            Ok(tasks) => match tasks.into_iter().find(|task| task.id == task_id) {
                Some(task) => {
                    builder.leaf(ops::STATUS, "1");
                    if let Some(id) = fetch.collection_id.as_deref() {
                        builder.leaf(air::COLLECTION_ID, id);
                    }
                    if let Some(id) = fetch.long_id.as_deref() {
                        builder.leaf(eas::search::LONG_ID, id);
                    } else {
                        builder.leaf(air::SERVER_ID, task_id);
                    }
                    builder.leaf(air::CLASS, "Tasks");
                    builder.start(ops::PROPERTIES);
                    write_task_fields(builder, &task, &fetch.body_preferences, version, false);
                    builder.end();
                }
                None => builder.leaf(ops::STATUS, "6"),
            },
            Err(error) => {
                tracing::warn!(%error, "ItemOperations task fetch failed");
                builder.leaf(ops::STATUS, "3");
            }
        }
        return;
    }
    let calendar_target = if let Some(long_id) = fetch
        .long_id
        .as_deref()
        .and_then(|id| id.strip_prefix("calendar:"))
    {
        long_id.split_once(":")
    } else if fetch.long_id.is_none() {
        fetch
            .collection_id
            .as_deref()
            .and_then(|id| id.strip_prefix("cal_"))
            .map(|id| (id, target))
    } else {
        None
    };
    if fetch
        .long_id
        .as_deref()
        .is_some_and(|id| id.starts_with("calendar:"))
        && calendar_target.is_none()
    {
        builder.leaf(ops::STATUS, "6");
        return;
    }
    if let Some((calendar, event_id)) = calendar_target {
        if calendar.is_empty()
            || event_id.is_empty()
            || event_id.contains(":")
            || fetch
                .collection_id
                .as_deref()
                .is_some_and(|folder| folder.strip_prefix("cal_") != Some(calendar))
        {
            builder.leaf(ops::STATUS, "6");
            return;
        }
        match state
            .jmap
            .calendar_events_in_calendar(auth, calendar, 0)
            .await
        {
            Ok(events) => match events.into_iter().find(|event| event.id == event_id) {
                Some(event) => {
                    builder.leaf(ops::STATUS, "1");
                    if let Some(id) = fetch.collection_id.as_deref() {
                        builder.leaf(air::COLLECTION_ID, id);
                    }
                    if let Some(id) = fetch.long_id.as_deref() {
                        builder.leaf(eas::search::LONG_ID, id);
                    } else {
                        builder.leaf(air::SERVER_ID, event_id);
                    }
                    builder.leaf(air::CLASS, "Calendar");
                    builder.start(ops::PROPERTIES);
                    write_calendar_fields(builder, &event, version, &fetch.body_preferences, false);
                    builder.end();
                }
                None => builder.leaf(ops::STATUS, "6"),
            },
            Err(error) => {
                tracing::warn!(%error, "ItemOperations calendar fetch failed");
                builder.leaf(ops::STATUS, "3");
            }
        }
        return;
    }
    let contact_id = fetch
        .long_id
        .as_deref()
        .and_then(|id| id.strip_prefix("contact:"))
        .or_else(|| {
            (fetch.long_id.is_none()
                && fetch
                    .collection_id
                    .as_deref()
                    .is_some_and(|id| id.starts_with("ab_")))
            .then_some(target)
        });
    if let Some(contact_id) = contact_id {
        match state.jmap.get_contact_by_id(auth, contact_id).await {
            Ok(Some(contact))
                if fetch.collection_id.as_deref().is_none_or(|folder| {
                    folder
                        .strip_prefix("ab_")
                        .is_some_and(|id| contact.address_book_ids.iter().any(|book| book == id))
                }) =>
            {
                builder.leaf(ops::STATUS, "1");
                if let Some(id) = fetch.collection_id.as_deref() {
                    builder.leaf(air::COLLECTION_ID, id);
                }
                if let Some(id) = fetch.long_id.as_deref() {
                    builder.leaf(eas::search::LONG_ID, id);
                } else {
                    builder.leaf(air::SERVER_ID, contact_id);
                }
                builder.leaf(air::CLASS, "Contacts");
                builder.start(ops::PROPERTIES);
                write_contact_fields(builder, &contact, false);
                builder.end();
            }
            Ok(_) => builder.leaf(ops::STATUS, "6"),
            Err(error) => {
                tracing::warn!(%error, "ItemOperations contact fetch failed");
                builder.leaf(ops::STATUS, "3");
            }
        }
        return;
    }
    match state.jmap.get_email_by_id(auth, target).await {
        Ok(Some(email)) => {
            let preference = match body_preferences_for_email(
                state,
                auth,
                &email,
                &fetch.body_preferences,
                fetch.mime_support,
            )
            .await
            {
                Ok(preference) => preference,
                Err(error) => {
                    tracing::warn!(%error, "ItemOperations body preparation failed");
                    builder.leaf(ops::STATUS, "14");
                    return;
                }
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
            write_email_fields(builder, target, email, Some(preference), version);
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
