use axum::{
    body::Bytes,
    extract::{Query, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use std::collections::BTreeSet;
use tokio::time::{sleep, Duration};

use crate::{http_server::AppState, jmap::client::basic_credentials, state::SyncRecord, wbxml};

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
        if !sync_key_valid {
            builder.start(air::COLLECTION);
            builder.leaf(air::SYNC_KEY, collection.sync_key);
            builder.leaf(air::COLLECTION_ID, collection.collection_id);
            builder.leaf(air::STATUS, "3");
            builder.end();
            continue;
        }

        let is_mail_collection = !collection.collection_id.starts_with("ab_")
            && !collection.collection_id.starts_with("cal_");
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
            let new_sync_key = if client_commands_applied {
                next_sync_key(&collection.sync_key)
            } else {
                collection.sync_key.clone()
            };
            if client_commands_applied {
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
