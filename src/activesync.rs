use axum::{
    body::Bytes,
    extract::{Query, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use tokio::time::{sleep, Duration};

use crate::{
    http_server::AppState,
    jmap::client::{basic_credentials, AuthenticatedSession},
    model::{EmailBodyType, Note},
    state::{ItemState, SyncRecord},
    wbxml,
};

const SUPPORTED_PROTOCOLS: &str = "12.1,14.0,14.1,16.0,16.1";
const SUPPORTED_COMMANDS: &str = "Sync,SendMail,SmartForward,SmartReply,GetAttachment,FolderSync,FolderCreate,FolderDelete,FolderUpdate,MoveItems,GetItemEstimate,MeetingResponse,Search,Settings,Ping,ItemOperations,Provision,ResolveRecipients,ValidateCert,Find";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct EasQuery {
    pub cmd: Option<String>,
    pub user: Option<String>,
    pub device_id: Option<String>,
    pub device_type: Option<String>,
}

pub async fn options_handler(State(state): State<AppState>, headers: HeaderMap) -> Response {
    match authenticate(&state, &headers).await {
        Ok(_) => eas_options_response(StatusCode::OK),
        Err(error) => {
            tracing::warn!(%error, "ActiveSync OPTIONS authentication failed");
            unauthorized()
        }
    }
}

pub async fn post_handler(
    State(state): State<AppState>,
    Query(query): Query<EasQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let command = query.cmd.as_deref().unwrap_or("unknown");
    if body.len() > state.config.max_wbxml_bytes {
        state
            .metrics
            .eas_requests_total
            .with_label_values(&[command, "413"])
            .inc();
        return (StatusCode::PAYLOAD_TOO_LARGE, "WBXML request too large\n").into_response();
    }

    let auth = match authenticate(&state, &headers).await {
        Ok(auth) => auth,
        Err(error) => {
            tracing::warn!(%error, eas_command = command, "ActiveSync authentication failed");
            state
                .metrics
                .eas_requests_total
                .with_label_values(&[command, "401"])
                .inc();
            return unauthorized();
        }
    };

    tracing::info!(
        user = query.user.as_deref().unwrap_or(""),
        device_id = query.device_id.as_deref().unwrap_or(""),
        device_type = query.device_type.as_deref().unwrap_or(""),
        eas_command = command,
        mail = auth.capabilities.mail,
        contacts = auth.capabilities.contacts,
        calendar = auth.capabilities.calendar,
        event_source = auth.capabilities.event_source,
        "received ActiveSync command"
    );

    if !body.is_empty() {
        match wbxml::decode_document(&body) {
            Ok(document) => {
                tracing::debug!(
                    eas_command = command,
                    nodes = document.nodes.len(),
                    "decoded WBXML request"
                );
                if command.eq_ignore_ascii_case("FolderSync") {
                    return folder_sync(&state, &auth, &document, command).await;
                }
                if command.eq_ignore_ascii_case("Sync") {
                    return sync_mail(&state, &auth, &query, &document, command).await;
                }
                if command.eq_ignore_ascii_case("MoveItems") {
                    return move_items(&state, &auth, &document, command).await;
                }
                if command.eq_ignore_ascii_case("Provision") {
                    return provision(&state, &document, command).await;
                }
                if command.eq_ignore_ascii_case("GetItemEstimate") {
                    return get_item_estimate(&state, &document, command).await;
                }
                if command.eq_ignore_ascii_case("Settings") {
                    return settings(&state, &auth, &document, command).await;
                }
                if command.eq_ignore_ascii_case("Ping") {
                    return ping(&state, &document, command).await;
                }
            }
            Err(error) => {
                state
                    .metrics
                    .eas_requests_total
                    .with_label_values(&[command, "400"])
                    .inc();
                return (StatusCode::BAD_REQUEST, format!("invalid WBXML: {error}\n"))
                    .into_response();
            }
        }
    } else if command.eq_ignore_ascii_case("FolderSync") {
        return (
            StatusCode::BAD_REQUEST,
            "FolderSync requires a WBXML request body\n",
        )
            .into_response();
    } else if command.eq_ignore_ascii_case("Sync") {
        return (
            StatusCode::BAD_REQUEST,
            "Sync requires a WBXML request body\n",
        )
            .into_response();
    }

    state
        .metrics
        .eas_requests_total
        .with_label_values(&[command, "501"])
        .inc();
    (
        StatusCode::NOT_IMPLEMENTED,
        [("Content-Type", "text/plain; charset=utf-8")],
        format!("ActiveSync command {command} is not implemented yet\n"),
    )
        .into_response()
}

async fn ping(state: &AppState, document: &wbxml::Document, command: &str) -> Response {
    let lifetime = wbxml::eas::find_text_after(document, wbxml::eas::ping::LIFETIME)
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(30)
        .clamp(1, 60);

    sleep(Duration::from_secs(lifetime)).await;

    let mut builder = wbxml::eas::DocumentBuilder::new();
    use wbxml::eas::ping;

    builder.start(ping::PING);
    builder.leaf(ping::STATUS, "1");
    builder.end();

    let body = wbxml::encode_document(&builder.finish());
    state
        .metrics
        .eas_requests_total
        .with_label_values(&[command, "200"])
        .inc();
    (
        StatusCode::OK,
        [("Content-Type", "application/vnd.ms-sync.wbxml")],
        body,
    )
        .into_response()
}

async fn settings(
    state: &AppState,
    auth: &crate::jmap::client::AuthenticatedSession,
    document: &wbxml::Document,
    command: &str,
) -> Response {
    let include_user = wbxml::eas::contains_token(document, wbxml::eas::settings::USER_INFORMATION);
    let mut builder = wbxml::eas::DocumentBuilder::new();
    use wbxml::eas::settings as set;

    builder.start(set::SETTINGS);
    builder.leaf(set::STATUS, "1");

    if include_user {
        builder.start(set::USER_INFORMATION);
        builder.leaf(set::STATUS, "1");
        builder.start(set::GET);
        builder.start(set::EMAIL_ADDRESSES);
        builder.leaf(set::SMTP_ADDRESS, auth.username());
        builder.end();
        builder.leaf(set::PRIMARY_SMTP_ADDRESS, auth.username());
        builder.end();
        builder.end();
    }

    builder.end();

    let body = wbxml::encode_document(&builder.finish());
    state
        .metrics
        .eas_requests_total
        .with_label_values(&[command, "200"])
        .inc();
    (
        StatusCode::OK,
        [("Content-Type", "application/vnd.ms-sync.wbxml")],
        body,
    )
        .into_response()
}

async fn get_item_estimate(
    state: &AppState,
    document: &wbxml::Document,
    command: &str,
) -> Response {
    let folder_ids =
        wbxml::eas::find_all_text_after(document, wbxml::eas::get_item_estimate::FOLDER_ID);
    if folder_ids.is_empty() {
        state
            .metrics
            .eas_requests_total
            .with_label_values(&[command, "400"])
            .inc();
        return (
            StatusCode::BAD_REQUEST,
            "GetItemEstimate missing FolderId\n",
        )
            .into_response();
    }

    let mut builder = wbxml::eas::DocumentBuilder::new();
    use wbxml::eas::get_item_estimate as gie;

    builder.start(gie::GET_ITEM_ESTIMATE);
    for folder_id in folder_ids {
        builder.start(gie::RESPONSE);
        builder.leaf(gie::STATUS, "1");
        builder.start(gie::FOLDER);
        builder.leaf(gie::FOLDER_ID, folder_id);
        builder.leaf(gie::ESTIMATE, "0");
        builder.end();
        builder.end();
    }
    builder.end();

    let body = wbxml::encode_document(&builder.finish());
    state
        .metrics
        .eas_requests_total
        .with_label_values(&[command, "200"])
        .inc();
    (
        StatusCode::OK,
        [("Content-Type", "application/vnd.ms-sync.wbxml")],
        body,
    )
        .into_response()
}

async fn provision(state: &AppState, document: &wbxml::Document, command: &str) -> Response {
    let policy_type = wbxml::eas::find_text_after(document, wbxml::eas::provision::POLICY_TYPE)
        .unwrap_or("MS-EAS-Provisioning-WBXML");
    let mut builder = wbxml::eas::DocumentBuilder::new();
    use wbxml::eas::provision as prov;

    builder.start(prov::PROVISION);
    builder.leaf(prov::STATUS, "1");
    builder.start(prov::POLICIES);
    builder.start(prov::POLICY);
    builder.leaf(prov::POLICY_TYPE, policy_type);
    builder.leaf(prov::STATUS, "2");
    builder.leaf(prov::POLICY_KEY, "1");
    builder.end();
    builder.end();
    builder.end();

    let body = wbxml::encode_document(&builder.finish());
    state
        .metrics
        .eas_requests_total
        .with_label_values(&[command, "200"])
        .inc();
    (
        StatusCode::OK,
        [("Content-Type", "application/vnd.ms-sync.wbxml")],
        body,
    )
        .into_response()
}

async fn move_items(
    state: &AppState,
    auth: &crate::jmap::client::AuthenticatedSession,
    document: &wbxml::Document,
    command: &str,
) -> Response {
    let moves = wbxml::eas::move_item_requests(document);
    if moves.is_empty() {
        state
            .metrics
            .eas_requests_total
            .with_label_values(&[command, "400"])
            .inc();
        return (StatusCode::BAD_REQUEST, "MoveItems missing Move entries\n").into_response();
    }

    let mut builder = wbxml::eas::DocumentBuilder::new();
    use wbxml::eas::move_items as mv;

    builder.start(mv::MOVES);
    for move_request in moves {
        builder.start(mv::RESPONSE);
        builder.leaf(mv::SRC_MSG_ID, move_request.src_msg_id.clone());

        let status = if move_request.src_fld_id == move_request.dst_fld_id {
            "4"
        } else {
            match state
                .jmap
                .move_email(
                    auth,
                    &move_request.src_msg_id,
                    &move_request.src_fld_id,
                    &move_request.dst_fld_id,
                )
                .await
            {
                Ok(()) => "3",
                Err(error) => {
                    tracing::warn!(
                        %error,
                        src_msg_id = move_request.src_msg_id,
                        src_fld_id = move_request.src_fld_id,
                        dst_fld_id = move_request.dst_fld_id,
                        "MoveItems JMAP move failed"
                    );
                    "5"
                }
            }
        };

        builder.leaf(mv::STATUS, status);
        builder.leaf(mv::DST_MSG_ID, move_request.src_msg_id);
        builder.end();
    }
    builder.end();

    let body = wbxml::encode_document(&builder.finish());
    state
        .metrics
        .eas_requests_total
        .with_label_values(&[command, "200"])
        .inc();
    (
        StatusCode::OK,
        [("Content-Type", "application/vnd.ms-sync.wbxml")],
        body,
    )
        .into_response()
}

async fn sync_mail(
    state: &AppState,
    auth: &crate::jmap::client::AuthenticatedSession,
    query: &EasQuery,
    document: &wbxml::Document,
    command: &str,
) -> Response {
    let collections = wbxml::eas::sync_collections(document);
    for c in &collections {
        tracing::debug!(
            collection_id = c.collection_id,
            sync_key = c.sync_key,
            commands = c.commands.len(),
            window_size = c.window_size,
            get_changes = c.get_changes,
            "parsed Sync collection"
        );
    }
    if collections.is_empty() {
        state
            .metrics
            .eas_requests_total
            .with_label_values(&[command, "400"])
            .inc();
        return (StatusCode::BAD_REQUEST, "Sync missing Collections\n").into_response();
    }

    let mut builder = wbxml::eas::DocumentBuilder::new();
    use wbxml::eas::airsync as air;

    builder.start(air::SYNC);
    builder.start(air::COLLECTIONS);

    for collection in collections {
        let user = query.user.as_deref().unwrap_or_else(|| auth.username());
        let device_id = query.device_id.as_deref().unwrap_or("unknown");
        let previous_record = match state
            .state
            .get(user, device_id, &collection.collection_id)
            .await
        {
            Ok(record) => record,
            Err(error) => {
                tracing::warn!(
                    %error,
                    user,
                    device_id,
                    collection = collection.collection_id,
                    "failed to load Sync state"
                );
                None
            }
        };
        let sync_key_valid = collection.sync_key == "0"
            || previous_record
                .as_ref()
                .is_some_and(|record| record.sync_key == collection.sync_key);
        tracing::debug!(
            collection_id = collection.collection_id,
            client_sync_key = collection.sync_key,
            stored_sync_key = previous_record
                .as_ref()
                .map(|r| r.sync_key.as_str())
                .unwrap_or("<none>"),
            sync_key_valid,
            "Sync key check"
        );
        if !sync_key_valid {
            builder.start(air::COLLECTION);
            builder.leaf(air::SYNC_KEY, collection.sync_key);
            builder.leaf(air::COLLECTION_ID, collection.collection_id);
            builder.leaf(air::STATUS, "3");
            builder.end();
            continue;
        }

        if collection.collection_id.starts_with("note_") {
            sync_notes_collection(state, auth, &mut builder, user, device_id, &collection).await;
            continue;
        }

        let is_mail_collection = !collection.collection_id.starts_with("ab_")
            && !collection.collection_id.starts_with("cal_")
            && !collection.collection_id.starts_with("note_");
        let client_commands_applied = if is_mail_collection && !collection.commands.is_empty() {
            match apply_mail_client_commands(
                state,
                auth,
                &collection.collection_id,
                &collection.commands,
            )
            .await
            {
                Ok(applied) => applied,
                Err(error) => {
                    tracing::warn!(
                        %error,
                        collection = collection.collection_id,
                        "failed to apply client Sync commands"
                    );
                    builder.start(air::COLLECTION);
                    builder.leaf(air::SYNC_KEY, collection.sync_key);
                    builder.leaf(air::COLLECTION_ID, collection.collection_id);
                    builder.leaf(air::STATUS, "5");
                    builder.end();
                    continue;
                }
            }
        } else {
            false
        };

        if collection.get_changes && is_mail_collection {
            let emails = match state
                .jmap
                .emails_in_mailbox(auth, &collection.collection_id, collection.window_size)
                .await
            {
                Ok(emails) => emails,
                Err(error) => {
                    tracing::warn!(
                        %error,
                        collection = collection.collection_id,
                        "Sync mail discovery failed"
                    );
                    builder.start(air::COLLECTION);
                    builder.leaf(air::SYNC_KEY, collection.sync_key);
                    builder.leaf(air::COLLECTION_ID, collection.collection_id);
                    builder.leaf(air::STATUS, "5");
                    builder.end();
                    continue;
                }
            };

            let previous_seen: BTreeSet<String> = previous_record
                .as_ref()
                .map(|record| record.seen_ids.iter().cloned().collect())
                .unwrap_or_default();
            let fetched_ids: BTreeSet<String> =
                emails.iter().map(|email| email.id.clone()).collect();
            let emails_to_send: Vec<_> = if collection.sync_key == "0" {
                emails
            } else {
                emails
                    .into_iter()
                    .filter(|email| !previous_seen.contains(&email.id))
                    .collect()
            };
            let new_sync_key = if collection.sync_key == "0"
                || !emails_to_send.is_empty()
                || client_commands_applied
            {
                next_sync_key(&collection.sync_key)
            } else {
                collection.sync_key.clone()
            };

            if let Err(error) = state
                .state
                .put(SyncRecord {
                    user: user.to_owned(),
                    device_id: device_id.to_owned(),
                    collection_id: collection.collection_id.clone(),
                    sync_key: new_sync_key.clone(),
                    jmap_state: String::new(),
                    seen_ids: previous_seen.union(&fetched_ids).cloned().collect(),
                })
                .await
            {
                tracing::warn!(
                    %error,
                    user,
                    device_id,
                    collection = collection.collection_id,
                    "failed to persist Sync state"
                );
                builder.start(air::COLLECTION);
                builder.leaf(air::SYNC_KEY, collection.sync_key);
                builder.leaf(air::COLLECTION_ID, collection.collection_id);
                builder.leaf(air::STATUS, "5");
                builder.end();
                continue;
            }

            builder.start(air::COLLECTION);
            builder.leaf(air::SYNC_KEY, new_sync_key);
            builder.leaf(air::COLLECTION_ID, collection.collection_id);
            builder.leaf(air::STATUS, "1");

            if !emails_to_send.is_empty() {
                builder.start(air::COMMANDS);
                for email in emails_to_send {
                    write_email_add(&mut builder, email);
                }
                builder.end();
            }
        } else {
            // Contacts/calendar have no real two-way sync yet (see
            // CLAUDE.md's Not-Yet-Implemented list) -- this branch is a
            // stub. But the initial sync_key 0->1 handshake still has to
            // happen and persist even with zero commands, or the server
            // echoes sync_key=0 back forever: the client reads that as
            // "never initialized" and retries immediately in a tight loop,
            // never settling into its normal Ping cadence. Confirmed live:
            // a real account's ab_*/cal_* collections hammered Sync every
            // ~300ms indefinitely with stored_sync_key always "<none>".
            let is_first_sync = collection.sync_key == "0";
            let advance = is_first_sync || client_commands_applied;
            let new_sync_key = if advance {
                next_sync_key(&collection.sync_key)
            } else {
                collection.sync_key.clone()
            };
            if advance {
                if let Err(error) = state
                    .state
                    .put(SyncRecord {
                        user: user.to_owned(),
                        device_id: device_id.to_owned(),
                        collection_id: collection.collection_id.clone(),
                        sync_key: new_sync_key.clone(),
                        jmap_state: previous_record
                            .as_ref()
                            .map(|record| record.jmap_state.clone())
                            .unwrap_or_default(),
                        seen_ids: previous_record
                            .as_ref()
                            .map(|record| record.seen_ids.clone())
                            .unwrap_or_default(),
                    })
                    .await
                {
                    tracing::warn!(
                        %error,
                        user,
                        device_id,
                        collection = collection.collection_id,
                        "failed to persist Sync state after client commands"
                    );
                    builder.start(air::COLLECTION);
                    builder.leaf(air::SYNC_KEY, collection.sync_key);
                    builder.leaf(air::COLLECTION_ID, collection.collection_id);
                    builder.leaf(air::STATUS, "5");
                    builder.end();
                    continue;
                }
            }
            builder.start(air::COLLECTION);
            builder.leaf(air::SYNC_KEY, new_sync_key);
            builder.leaf(air::COLLECTION_ID, collection.collection_id);
            builder.leaf(air::STATUS, "1");
        }

        builder.end();
    }

    builder.end();
    builder.end();

    let body = wbxml::encode_document(&builder.finish());
    state
        .metrics
        .eas_requests_total
        .with_label_values(&[command, "200"])
        .inc();
    (
        StatusCode::OK,
        [("Content-Type", "application/vnd.ms-sync.wbxml")],
        body,
    )
        .into_response()
}

async fn apply_mail_client_commands(
    state: &AppState,
    auth: &crate::jmap::client::AuthenticatedSession,
    collection_id: &str,
    commands: &[wbxml::eas::SyncClientCommand],
) -> anyhow::Result<bool> {
    let mut applied = false;
    for command in commands {
        match command.kind {
            wbxml::eas::SyncClientCommandKind::Change => {
                if let Some(read) = command.read {
                    state
                        .jmap
                        .set_email_seen(auth, &command.server_id, read)
                        .await?;
                    applied = true;
                }
            }
            wbxml::eas::SyncClientCommandKind::Delete => {
                state.jmap.destroy_email(auth, &command.server_id).await?;
                applied = true;
            }
            wbxml::eas::SyncClientCommandKind::Add | wbxml::eas::SyncClientCommandKind::Fetch => {
                tracing::debug!(
                    collection = collection_id,
                    server_id = command.server_id,
                    kind = ?command.kind,
                    "ignoring unsupported mail Sync client command"
                );
            }
        }
    }
    Ok(applied)
}

fn write_email_add(builder: &mut wbxml::eas::DocumentBuilder, email: crate::model::Email) {
    use wbxml::eas::{airsync as air, airsync_base as base, email as mail};

    builder.start(air::ADD);
    builder.leaf(air::SERVER_ID, email.id);
    builder.start(air::APPLICATION_DATA);
    builder.leaf(mail::MESSAGE_CLASS, "IPM.Note");
    builder.leaf(mail::SUBJECT, email.subject);
    if let Some(received_at) = email.received_at {
        builder.leaf(mail::DATE_RECEIVED, received_at);
    }
    if !email.from.is_empty() {
        builder.leaf(mail::FROM, email.from);
    }
    if !email.to.is_empty() {
        builder.leaf(mail::TO, email.to.clone());
        builder.leaf(mail::DISPLAY_TO, email.to);
    }
    if !email.cc.is_empty() {
        builder.leaf(mail::CC, email.cc);
    }
    builder.leaf(mail::IMPORTANCE, "1");
    builder.leaf(mail::READ, if email.read { "1" } else { "0" });
    if let Some(body) = email.body {
        builder.start(base::BODY);
        builder.leaf(base::TYPE, body.body_type.eas_value());
        builder.leaf(base::ESTIMATED_DATA_SIZE, body.value.len().to_string());
        builder.leaf(base::TRUNCATED, "0");
        builder.leaf(base::DATA, body.value);
        builder.end();
        builder.leaf(base::NATIVE_BODY_TYPE, body.body_type.eas_value());
    }
    builder.end();
    builder.end();
}

/// Handles one Notes collection's Sync round-trip end to end: applies any
/// client Add/Change/Delete commands (building `<Responses>` entries --
/// see the module-level notes in jmap::notes for why Add specifically
/// needs a real `<ServerId>` in that reply, not just Status), diffs the
/// current JMAP state against last-known per-item state to find what to
/// push back as `<Commands>`, and persists the new baseline. Writes one
/// complete `<Collection>...</Collection>` block into `builder` itself
/// (the caller has already validated the SyncKey and does not wrap this
/// call in its own Collection tags).
async fn sync_notes_collection(
    state: &AppState,
    auth: &AuthenticatedSession,
    builder: &mut wbxml::eas::DocumentBuilder,
    user: &str,
    device_id: &str,
    collection: &wbxml::eas::SyncCollectionRequest,
) {
    use wbxml::eas::airsync as air;

    let mailbox_id = collection
        .collection_id
        .strip_prefix("note_")
        .unwrap_or(&collection.collection_id)
        .to_owned();

    let previous_items = state
        .state
        .item_states(user, device_id, &collection.collection_id)
        .await
        .unwrap_or_default();
    let mut previous_by_id: BTreeMap<String, String> = previous_items
        .into_iter()
        .map(|item| (item.item_id, item.hash))
        .collect();

    let mut add_responses: Vec<(String, Option<String>, &'static str)> = Vec::new();
    let mut change_responses: Vec<(String, &'static str)> = Vec::new();

    for command in &collection.commands {
        match command.kind {
            wbxml::eas::SyncClientCommandKind::Add => {
                let content = crate::jmap::notes::NoteContent {
                    subject: command.note.subject.as_deref().unwrap_or(""),
                    body: command.note.body.as_deref().unwrap_or(""),
                    body_type: note_body_type(command.note.body_type),
                    categories: &command.note.categories,
                };
                match state.jmap.save_note(auth, &mailbox_id, None, content).await {
                    Ok(new_id) => {
                        add_responses.push((command.client_id.clone(), Some(new_id), "1"))
                    }
                    Err(error) => {
                        tracing::warn!(
                            %error,
                            client_id = command.client_id,
                            "failed to create note from client Add"
                        );
                        add_responses.push((command.client_id.clone(), None, "5"));
                    }
                }
            }
            wbxml::eas::SyncClientCommandKind::Change => {
                let content = crate::jmap::notes::NoteContent {
                    subject: command.note.subject.as_deref().unwrap_or(""),
                    body: command.note.body.as_deref().unwrap_or(""),
                    body_type: note_body_type(command.note.body_type),
                    categories: &command.note.categories,
                };
                match state
                    .jmap
                    .save_note(auth, &mailbox_id, Some(&command.server_id), content)
                    .await
                {
                    Ok(id) => change_responses.push((id, "1")),
                    Err(error) => {
                        tracing::warn!(
                            %error,
                            server_id = command.server_id,
                            "failed to save note change"
                        );
                        change_responses.push((command.server_id.clone(), "5"));
                    }
                }
            }
            wbxml::eas::SyncClientCommandKind::Delete => {
                match state.jmap.destroy_note(auth, &command.server_id).await {
                    Ok(()) => {
                        previous_by_id.remove(&command.server_id);
                    }
                    Err(error) => {
                        tracing::warn!(
                            %error,
                            server_id = command.server_id,
                            "failed to delete note"
                        );
                    }
                }
                // A successful Delete gets no Responses entry -- the client
                // already knows (it's the one that asked). Only Add/Change
                // need to report back, since those are where the client
                // needs information (a new ServerId, or confirmation) it
                // doesn't already have.
            }
            wbxml::eas::SyncClientCommandKind::Fetch => {
                tracing::debug!(
                    server_id = command.server_id,
                    "ignoring unsupported Notes Fetch command"
                );
            }
        }
    }

    let current_summaries = match state.jmap.list_notes(auth, &mailbox_id, 500).await {
        Ok(summaries) => summaries,
        Err(error) => {
            tracing::warn!(
                %error,
                collection = collection.collection_id,
                "Notes sync discovery failed"
            );
            builder.start(air::COLLECTION);
            builder.leaf(air::SYNC_KEY, collection.sync_key.clone());
            builder.leaf(air::COLLECTION_ID, collection.collection_id.clone());
            builder.leaf(air::STATUS, "5");
            builder.end();
            return;
        }
    };

    let current_by_id: BTreeMap<String, String> = current_summaries
        .into_iter()
        .map(|summary| (summary.id, summary.hash))
        .collect();

    // Ids the client itself just added/changed in THIS request are already
    // confirmed via the Responses entries above -- do NOT also echo them
    // back in Commands as if they were new-from-server. Naively diffing
    // current-vs-previous would otherwise flag every fresh Add as "new"
    // (it wasn't in the pre-request state either), telling the client
    // about its own submission a second time in the same response. This
    // exact class of self-echo confusion is what caused every duplicate
    // seen while building the PHP reference implementation.
    let just_written: BTreeSet<String> = add_responses
        .iter()
        .filter_map(|(_, server_id, _)| server_id.clone())
        .chain(
            change_responses
                .iter()
                .map(|(server_id, _)| server_id.clone()),
        )
        .collect();

    let mut to_add = Vec::new();
    let mut to_change = Vec::new();
    for (id, hash) in &current_by_id {
        if just_written.contains(id) {
            continue;
        }
        match previous_by_id.get(id) {
            None => to_add.push(id.clone()),
            Some(previous_hash) if previous_hash != hash => to_change.push(id.clone()),
            _ => {}
        }
    }
    let to_remove: Vec<String> = previous_by_id
        .keys()
        .filter(|id| !current_by_id.contains_key(id.as_str()))
        .cloned()
        .collect();

    let has_server_changes = !to_add.is_empty() || !to_change.is_empty() || !to_remove.is_empty();
    let client_commands_applied = !add_responses.is_empty() || !change_responses.is_empty();
    let new_sync_key =
        if collection.sync_key == "0" || has_server_changes || client_commands_applied {
            next_sync_key(&collection.sync_key)
        } else {
            collection.sync_key.clone()
        };

    if let Err(error) = state
        .state
        .put(SyncRecord {
            user: user.to_owned(),
            device_id: device_id.to_owned(),
            collection_id: collection.collection_id.clone(),
            sync_key: new_sync_key.clone(),
            jmap_state: String::new(),
            seen_ids: Vec::new(),
        })
        .await
    {
        tracing::warn!(%error, user, device_id, collection = collection.collection_id, "failed to persist Notes SyncRecord");
    }
    let item_states: Vec<ItemState> = current_by_id
        .iter()
        .map(|(id, hash)| ItemState {
            item_id: id.clone(),
            hash: hash.clone(),
        })
        .collect();
    if let Err(error) = state
        .state
        .put_item_states(user, device_id, &collection.collection_id, item_states)
        .await
    {
        tracing::warn!(%error, user, device_id, collection = collection.collection_id, "failed to persist Notes item state");
    }

    builder.start(air::COLLECTION);
    builder.leaf(air::SYNC_KEY, new_sync_key);
    builder.leaf(air::COLLECTION_ID, collection.collection_id.clone());
    builder.leaf(air::STATUS, "1");

    if !add_responses.is_empty() || !change_responses.is_empty() {
        builder.start(air::RESPONSES);
        for (client_id, server_id, status) in &add_responses {
            builder.start(air::ADD);
            builder.leaf(air::CLIENT_ID, client_id.clone());
            if let Some(server_id) = server_id {
                builder.leaf(air::SERVER_ID, server_id.clone());
            }
            builder.leaf(air::STATUS, *status);
            builder.end();
        }
        for (server_id, status) in &change_responses {
            builder.start(air::CHANGE);
            builder.leaf(air::SERVER_ID, server_id.clone());
            builder.leaf(air::STATUS, *status);
            builder.end();
        }
        builder.end();
    }

    if collection.get_changes && has_server_changes {
        builder.start(air::COMMANDS);
        for id in &to_add {
            if let Ok(Some(note)) = state.jmap.get_note(auth, id).await {
                write_note_command(builder, air::ADD, &note);
            }
        }
        for id in &to_change {
            if let Ok(Some(note)) = state.jmap.get_note(auth, id).await {
                write_note_command(builder, air::CHANGE, &note);
            }
        }
        for id in &to_remove {
            builder.start(air::DELETE);
            builder.leaf(air::SERVER_ID, id.clone());
            builder.end();
        }
        builder.end();
    }

    builder.end();
}

fn note_body_type(raw: Option<u8>) -> EmailBodyType {
    match raw {
        Some(2) => EmailBodyType::Html,
        _ => EmailBodyType::Plain,
    }
}

fn write_note_command(
    builder: &mut wbxml::eas::DocumentBuilder,
    tag: wbxml::token::Token,
    note: &Note,
) {
    use wbxml::eas::{airsync as air, airsync_base as base, notes};

    builder.start(tag);
    builder.leaf(air::SERVER_ID, note.id.clone());
    builder.start(air::APPLICATION_DATA);
    builder.leaf(notes::SUBJECT, note.title.clone());
    builder.start(base::BODY);
    builder.leaf(base::TYPE, note.body_type.eas_value());
    builder.leaf(base::ESTIMATED_DATA_SIZE, note.body.len().to_string());
    builder.leaf(base::TRUNCATED, "0");
    builder.leaf(base::DATA, note.body.clone());
    builder.end();
    builder.leaf(notes::MESSAGE_CLASS, "IPM.StickyNote");
    if let Some(modified) = &note.modified {
        builder.leaf(notes::LAST_MODIFIED_DATE, eas_datetime(modified));
    }
    if !note.categories.is_empty() {
        builder.start(notes::CATEGORIES);
        for category in &note.categories {
            builder.leaf(notes::CATEGORY, category.clone());
        }
        builder.end();
    }
    builder.end();
    builder.end();
}

/// `Notes:LastModifiedDate` wants the compact EAS datetime format
/// (`YYYYMMDDTHHMMSSZ`), not JMAP's ISO 8601 (`YYYY-MM-DDTHH:MM:SSZ`) --
/// confirmed against a live device trace captured against this fork's PHP
/// predecessor, not assumed.
fn eas_datetime(jmap_datetime: &str) -> String {
    jmap_datetime
        .chars()
        .filter(|ch| *ch != '-' && *ch != ':')
        .collect()
}

async fn folder_sync(
    state: &AppState,
    auth: &crate::jmap::client::AuthenticatedSession,
    document: &wbxml::Document,
    command: &str,
) -> Response {
    let Some(sync_key) =
        wbxml::eas::find_text_after(document, wbxml::eas::folder_hierarchy::SYNC_KEY)
    else {
        state
            .metrics
            .eas_requests_total
            .with_label_values(&[command, "400"])
            .inc();
        return (StatusCode::BAD_REQUEST, "FolderSync missing SyncKey\n").into_response();
    };

    let collections = match state.jmap.collections(auth).await {
        Ok(collections) => collections,
        Err(error) => {
            tracing::warn!(%error, "FolderSync collection discovery failed");
            state
                .metrics
                .eas_requests_total
                .with_label_values(&[command, "502"])
                .inc();
            return (
                StatusCode::BAD_GATEWAY,
                format!("JMAP collection discovery failed: {error}\n"),
            )
                .into_response();
        }
    };

    let new_sync_key = next_sync_key(sync_key);
    let mut builder = wbxml::eas::DocumentBuilder::new();
    use wbxml::eas::folder_hierarchy as fh;

    builder.start(fh::FOLDER_SYNC);
    builder.leaf(fh::STATUS, "1");
    builder.leaf(fh::SYNC_KEY, new_sync_key);
    builder.start(fh::CHANGES);
    builder.leaf(fh::COUNT, collections.len().to_string());
    for collection in collections {
        builder.start(fh::ADD);
        builder.leaf(fh::SERVER_ID, collection.id);
        builder.leaf(
            fh::PARENT_ID,
            collection.parent_id.unwrap_or_else(|| "0".to_owned()),
        );
        builder.leaf(fh::DISPLAY_NAME, collection.name);
        builder.leaf(fh::TYPE, collection.folder_type.to_string());
        builder.end();
    }
    builder.end();
    builder.end();

    let body = wbxml::encode_document(&builder.finish());
    state
        .metrics
        .eas_requests_total
        .with_label_values(&[command, "200"])
        .inc();
    (
        StatusCode::OK,
        [
            ("Content-Type", "application/vnd.ms-sync.wbxml"),
            ("MS-ASProtocolVersions", SUPPORTED_PROTOCOLS),
            ("MS-ASProtocolCommands", SUPPORTED_COMMANDS),
        ],
        body,
    )
        .into_response()
}

fn next_sync_key(sync_key: &str) -> String {
    if sync_key == "0" {
        return "1".to_owned();
    }
    sync_key
        .parse::<u64>()
        .map(|value| value.saturating_add(1).to_string())
        .unwrap_or_else(|_| "1".to_owned())
}

async fn authenticate(
    state: &AppState,
    headers: &HeaderMap,
) -> anyhow::Result<crate::jmap::client::AuthenticatedSession> {
    let (username, password) = basic_credentials(headers)?;
    state.jmap.session_with_basic(&username, &password).await
}

fn eas_options_response(status: StatusCode) -> Response {
    let mut response = status.into_response();
    let headers = response.headers_mut();
    headers.insert(
        "MS-ASProtocolVersions",
        HeaderValue::from_static(SUPPORTED_PROTOCOLS),
    );
    headers.insert(
        "MS-ASProtocolCommands",
        HeaderValue::from_static(SUPPORTED_COMMANDS),
    );
    headers.insert("X-AspNet-Version", HeaderValue::from_static("4.0.30319"));
    headers.insert("Content-Length", HeaderValue::from_static("0"));
    response
}

fn unauthorized() -> Response {
    let mut response = StatusCode::UNAUTHORIZED.into_response();
    response.headers_mut().insert(
        http::header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Basic realm=\"Stalwart Sync Gateway\""),
    );
    response
}
