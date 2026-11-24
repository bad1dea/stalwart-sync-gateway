mod draft;
mod find;
mod item_operations;
mod meeting_response;
mod resolve_recipients;
mod search_multi;
mod smart_compose;
mod sync_options;
mod sync_request;
mod validate_cert;

use crate::wbxml::eas::RequestedBodyPreference;
use anyhow::Context;
use axum::{
    body::Bytes,
    extract::{Query, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::hash::{Hash, Hasher};
use tokio::time::Duration;

use crate::{
    http_server::AppState,
    jmap::client::{basic_credentials, AuthenticatedSession},
    model::{EmailBodyType, EmailState, Note},
    state::{ItemState, SyncRecord},
    wbxml,
};

// Keep negotiation at the tested production version while newer response
// shapes and commands are implemented and checked with independent clients.
const SUPPORTED_PROTOCOLS: &str = "14.0";
// Advertise only commands with handlers.
const SUPPORTED_COMMANDS: &str = "Sync,SendMail,SmartForward,SmartReply,GetAttachment,FolderSync,FolderCreate,FolderDelete,FolderUpdate,MoveItems,GetItemEstimate,Search,Settings,Ping,ItemOperations,Provision,ResolveRecipients,MeetingResponse,ValidateCert";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProtocolVersion {
    V14_0,
    V14_1,
    V16_0,
    V16_1,
}

impl ProtocolVersion {
    fn as_str(self) -> &'static str {
        match self {
            Self::V14_0 => "14.0",
            Self::V14_1 => "14.1",
            Self::V16_0 => "16.0",
            Self::V16_1 => "16.1",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "14.0" => Some(Self::V14_0),
            "14.1" => Some(Self::V14_1),
            "16.0" => Some(Self::V16_0),
            "16.1" => Some(Self::V16_1),
            _ => None,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct EasQuery {
    pub cmd: Option<String>,
    pub user: Option<String>,
    pub device_id: Option<String>,
    pub device_type: Option<String>,
    /// SendMail's "simplified" transport (MS-ASCMD 2.2.2.19, EAS 14.0+):
    /// the POST body is the raw RFC822 MIME message directly, not WBXML,
    /// and SaveInSentItems/ClientId travel as query params instead of
    /// WBXML elements. Presence (any value) means true, matching real
    /// client behavior -- absence means the client didn't ask either way.
    pub save_in_sent_items: Option<String>,
    /// GetAttachment (MS-ASCMD 2.2.2.8): a plain GET, no WBXML at all --
    /// the reference this gateway itself handed out earlier in an
    /// AirSyncBase:FileReference (see write_email_fields), "blobId||name".
    pub attachment_name: Option<String>,
}

/// Handles the one command that's a plain GET, not the POST+Cmd= scheme
/// every other command uses: GetAttachment (MS-ASCMD 2.2.2.8). Downloads
/// the JMAP blob and returns it with the right Content-Type/filename --
/// no WBXML involved at all, request or response.
pub async fn get_handler(
    State(state): State<AppState>,
    Query(query): Query<EasQuery>,
    headers: HeaderMap,
) -> Response {
    let command = query.cmd.as_deref().unwrap_or("unknown");
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

    if !command.eq_ignore_ascii_case("GetAttachment") {
        state
            .metrics
            .eas_requests_total
            .with_label_values(&[command, "501"])
            .inc();
        return (
            StatusCode::NOT_IMPLEMENTED,
            format!("ActiveSync command {command} is not implemented yet\n"),
        )
            .into_response();
    }

    let Some(reference) = query.attachment_name.as_deref() else {
        state
            .metrics
            .eas_requests_total
            .with_label_values(&[command, "400"])
            .inc();
        return (
            StatusCode::BAD_REQUEST,
            "GetAttachment requires AttachmentName\n",
        )
            .into_response();
    };
    let Some((blob_id, name)) = reference.split_once("||") else {
        state
            .metrics
            .eas_requests_total
            .with_label_values(&[command, "400"])
            .inc();
        return (StatusCode::BAD_REQUEST, "malformed AttachmentName\n").into_response();
    };

    match state.jmap.download_blob(&auth, blob_id, name).await {
        Ok(bytes) => {
            state
                .metrics
                .eas_requests_total
                .with_label_values(&[command, "200"])
                .inc();
            (
                StatusCode::OK,
                [("Content-Type", "application/octet-stream")],
                bytes,
            )
                .into_response()
        }
        Err(error) => {
            tracing::warn!(%error, blob_id, "GetAttachment download failed");
            state
                .metrics
                .eas_requests_total
                .with_label_values(&[command, "404"])
                .inc();
            (StatusCode::NOT_FOUND, "attachment not found\n").into_response()
        }
    }
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
    Query(mut query): Query<EasQuery>,
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

    // Persisted device state and replayed responses belong to the authenticated
    // login, never the untrusted User query parameter.
    query.user = Some(auth.username().to_owned());

    tracing::info!(
        user = query.user.as_deref().unwrap_or(""),
        device_id = query.device_id.as_deref().unwrap_or(""),
        device_type = query.device_type.as_deref().unwrap_or(""),
        eas_command = command,
        mail = auth.capabilities.mail,
        contacts = auth.capabilities.contacts,
        calendar = auth.capabilities.calendar,
        event_source = auth.capabilities.event_source,
        protocol_version = headers
            .get("MS-ASProtocolVersion")
            .and_then(|v| v.to_str().ok())
            .unwrap_or(""),
        "received ActiveSync command"
    );
    let version_header = headers
        .get("MS-ASProtocolVersion")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("14.0");
    let Some(version) = ProtocolVersion::parse(version_header) else {
        return (
            StatusCode::BAD_REQUEST,
            "unsupported MS-ASProtocolVersion\n",
        )
            .into_response();
    };
    if !SUPPORTED_PROTOCOLS.split(',').any(|v| v == version_header) {
        tracing::warn!(
            device_id = query.device_id.as_deref().unwrap_or(""),
            version = version_header,
            supported = SUPPORTED_PROTOCOLS,
            "client uses a protocol version still under development"
        );
    }

    if command.eq_ignore_ascii_case("SendMail")
        || command.eq_ignore_ascii_case("SmartForward")
        || command.eq_ignore_ascii_case("SmartReply")
    {
        return send_mail(&state, &auth, &query, &body, command).await;
    }

    if body.is_empty() && command.eq_ignore_ascii_case("Sync") {
        return sync_mail(
            &state,
            &auth,
            &query,
            &wbxml::Document { nodes: Vec::new() },
            version,
            command,
        )
        .await;
    }
    let body: &[u8] = &body;

    // ActiveSync lets a client send Ping with an empty body to reuse the
    // heartbeat and folder list from its previous Ping.
    if body.is_empty() && command.eq_ignore_ascii_case("Ping") {
        return ping(&state, &auth, &query, None, command).await;
    }

    if !body.is_empty() {
        match wbxml::decode_document(body) {
            Ok(document) => {
                tracing::debug!(
                    eas_command = command,
                    nodes = document.nodes.len(),
                    "decoded WBXML request"
                );
                if command.eq_ignore_ascii_case("FolderSync") {
                    return folder_sync(&state, &auth, &query, &document, command).await;
                }
                if command.eq_ignore_ascii_case("FolderCreate") {
                    return folder_create(&state, &auth, &document, command).await;
                }
                if command.eq_ignore_ascii_case("FolderDelete") {
                    return folder_delete(&state, &auth, &document, command).await;
                }
                if command.eq_ignore_ascii_case("FolderUpdate") {
                    return folder_update(&state, &auth, &document, command).await;
                }
                if command.eq_ignore_ascii_case("Sync") {
                    return sync_mail(&state, &auth, &query, &document, version, command).await;
                }
                if command.eq_ignore_ascii_case("MoveItems") {
                    return move_items(&state, &auth, &query, &document, command).await;
                }
                if command.eq_ignore_ascii_case("Provision") {
                    return provision(&state, &document, command).await;
                }
                if command.eq_ignore_ascii_case("GetItemEstimate") {
                    return get_item_estimate(&state, &auth, &query, &document, command).await;
                }
                if command.eq_ignore_ascii_case("Settings") {
                    return settings(&state, &auth, &document, version, command).await;
                }
                if command.eq_ignore_ascii_case("Ping") {
                    return ping(&state, &auth, &query, Some(&document), command).await;
                }
                if command.eq_ignore_ascii_case("ItemOperations") {
                    let multipart = headers
                        .get("MS-ASAcceptMultiPart")
                        .is_some_and(|value| value == "T");
                    return item_operations::handle(
                        &state, &auth, &document, multipart, version, command,
                    )
                    .await;
                }
                if command.eq_ignore_ascii_case("ValidateCert") {
                    return validate_cert::handle(&state.config, &document);
                }
                if command.eq_ignore_ascii_case("MeetingResponse") {
                    return meeting_response::handle(&state, &auth, &query, &document, version)
                        .await;
                }
                if command.eq_ignore_ascii_case("ResolveRecipients") {
                    return resolve_recipients::handle(&state, &auth, &document).await;
                }
                if command.eq_ignore_ascii_case("Find") {
                    return find::handle(&state, &auth, &query, &document, version).await;
                }
                if command.eq_ignore_ascii_case("Search") {
                    return search(&state, &auth, &document, version, command).await;
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

fn ping_response(
    state: &AppState,
    command: &str,
    status: u8,
    folders: &[String],
    limit: Option<u64>,
) -> Response {
    use wbxml::eas::ping;
    let mut builder = wbxml::eas::DocumentBuilder::new();
    builder.start(ping::PING);
    builder.leaf(ping::STATUS, status.to_string());
    if let Some(limit) = limit {
        builder.leaf(ping::LIFETIME, limit.to_string());
    }
    if !folders.is_empty() {
        builder.start(ping::FOLDERS);
        for id in folders {
            builder.leaf(ping::FOLDER, id.clone());
        }
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

/// Mailbox ids among the watched folders. Other item classes use prefixed ids.
fn watched_mail_ids(folders: &[(String, String)]) -> Vec<String> {
    folders
        .iter()
        .filter(|(id, class)| {
            !is_non_mail_collection_id(id)
                && crate::push::Class::from_eas(class) == Some(crate::push::Class::Mail)
        })
        .map(|(id, _)| id.clone())
        .collect()
}

/// Which of `mail_ids` would Sync actually send something for right now.
async fn changed_mail_folders(
    state: &AppState,
    auth: &crate::jmap::client::AuthenticatedSession,
    eas_user: &str,
    device: &str,
    mail_ids: &[String],
) -> anyhow::Result<Vec<String>> {
    let mut changed = Vec::new();
    for id in mail_ids {
        let record = match state.state.get(eas_user, device, id).await {
            Ok(Some(record)) => record,
            // Never synced (or state unreadable): nothing to compare against;
            // the client starts with a Sync anyway. Not reporting avoids a
            // Ping/Sync loop.
            Ok(None) => continue,
            Err(error) => {
                tracing::warn!(%error, "push: sync state unreadable");
                continue;
            }
        };
        let read_states: BTreeMap<String, String> = state
            .state
            .item_states(eas_user, device, id)
            .await
            .map(|items| items.into_iter().map(|i| (i.item_id, i.hash)).collect())
            .unwrap_or_default();
        // The device's last Sync window and FilterType for this folder (kept
        // in `jmap_state` for mail records).
        let options = mail_record_options(&record.jmap_state);
        let window = options.window;
        let after = mail_filter_cutoff(options.filter, chrono::Utc::now());
        let mut found = state
            .jmap
            .mailbox_has_sync_changes(
                auth,
                id,
                &record.seen_ids,
                &read_states,
                window,
                after.as_deref(),
            )
            .await;
        if matches!(found, Ok(None)) && !options.email_state.is_empty() {
            // Anything else since the device's last Sync, at any age: Sync
            // asks Email/changes from the same state, so it will deliver it.
            found = state
                .jmap
                .email_changes_since(auth, &options.email_state, id)
                .await
                .map(|changes| {
                    changes.and_then(|c| mail_changes_reason(&c, &record.seen_ids, &read_states))
                });
        }
        match found {
            Ok(Some(reason)) => {
                tracing::info!(device_id = device, collection = %id, reason, window, seen = record.seen_ids.len(), "push: probe found a change");
                changed.push(id.clone())
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(%error, collection = %id, "push: change probe failed");
                return Err(error);
            }
        }
    }
    Ok(changed)
}

async fn item_collection_hashes(
    state: &AppState,
    auth: &AuthenticatedSession,
    id: &str,
) -> anyhow::Result<BTreeMap<String, String>> {
    let current = if let Some(mailbox) = id.strip_prefix("note_") {
        state
            .jmap
            .list_notes(auth, mailbox, 0)
            .await?
            .into_iter()
            .map(|note| (note.id, note.hash))
            .collect()
    } else if let Some(book) = id.strip_prefix("ab_") {
        state
            .jmap
            .contacts_in_address_book(auth, book, 0)
            .await?
            .into_iter()
            .map(|contact| (contact.id.clone(), contact_hash(&contact)))
            .collect()
    } else if let Some(calendar) = id.strip_prefix("cal_") {
        state
            .jmap
            .calendar_events_in_calendar(auth, calendar, 0)
            .await?
            .into_iter()
            .map(|event| (event.id.clone(), calendar_event_hash(&event)))
            .collect()
    } else if let Some(calendar) = id.strip_prefix("task_") {
        state
            .jmap
            .tasks_in_calendar(auth, calendar, 0)
            .await?
            .into_iter()
            .map(|task| (task.id.clone(), task_hash(&task)))
            .collect()
    } else {
        anyhow::bail!("unknown item collection {id}");
    };
    Ok(current)
}

async fn changed_item_folders(
    state: &AppState,
    auth: &AuthenticatedSession,
    user: &str,
    device: &str,
    folders: &[(String, String)],
) -> anyhow::Result<Vec<String>> {
    let mut changed = Vec::new();
    for (id, _) in folders {
        if !is_non_mail_collection_id(id) || state.state.get(user, device, id).await?.is_none() {
            continue;
        }
        let previous: BTreeMap<String, String> = state
            .state
            .item_states(user, device, id)
            .await?
            .into_iter()
            .map(|item| (item.item_id, item.hash))
            .collect();
        let current = item_collection_hashes(state, auth, id).await?;
        if current != previous {
            changed.push(id.clone());
        }
    }
    Ok(changed)
}

/// A newer Ping from the same device replaced this one: answer it quietly
/// (the client has already moved on) so its resources free up now.
fn ping_superseded(state: &AppState, command: &str, device: &str) -> Response {
    tracing::info!(device_id = %device, "ping: superseded by a newer Ping from the same device");
    ping_response(state, command, 1, &[], None)
}

/// Logs a Ping the client hung up on before the gateway answered (the
/// handler future is dropped, so nothing else would record it).
struct PingHangupLog {
    device: String,
    started: std::time::Instant,
    answered: bool,
}

impl Drop for PingHangupLog {
    fn drop(&mut self) {
        if !self.answered {
            tracing::info!(
                device_id = %self.device,
                waited_ms = self.started.elapsed().as_millis() as u64,
                "ping: client closed the connection before the answer"
            );
        }
    }
}

async fn ping(
    state: &AppState,
    auth: &crate::jmap::client::AuthenticatedSession,
    query: &EasQuery,
    document: Option<&wbxml::Document>,
    command: &str,
) -> Response {
    let mut log = PingHangupLog {
        device: query
            .device_id
            .clone()
            .unwrap_or_else(|| "unknown".to_owned()),
        started: std::time::Instant::now(),
        answered: false,
    };
    let response = ping_inner(state, auth, query, document, command).await;
    log.answered = true;
    response
}

async fn ping_inner(
    state: &AppState,
    auth: &crate::jmap::client::AuthenticatedSession,
    query: &EasQuery,
    document: Option<&wbxml::Document>,
    command: &str,
) -> Response {
    use crate::push::Class;
    use std::time::Instant;

    let started = Instant::now();
    let user_key = auth.username().to_ascii_lowercase();
    let eas_user = query
        .user
        .clone()
        .unwrap_or_else(|| auth.username().to_owned());
    let device = query
        .device_id
        .clone()
        .unwrap_or_else(|| "unknown".to_owned());

    let Some(_slot) = state.push.try_ping_slot() else {
        tracing::warn!(device_id = %device, "ping: concurrent Ping limit reached");
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [("Retry-After", "60")],
            "too many pending Ping requests\n",
        )
            .into_response();
    };

    let push = state.push.ensure_subscriber(
        &user_key,
        auth.session.event_source_url.as_deref(),
        auth.username(),
        auth.authorization.password(),
    );

    // Heartbeat and folders: from the request, else the device's last Ping.
    let request = document.map(wbxml::eas::ping_request).unwrap_or_default();
    let mut cached = push.device(&device);
    if cached.folders.is_empty() || cached.heartbeat.is_none() {
        if let Some((heartbeat, folders)) = load_ping_params(state, &eas_user, &device).await {
            tracing::info!(device_id = %device, heartbeat, folders = folders.len(), "ping: parameters restored from state store");
            push.set_device_params(&device, heartbeat, folders.clone());
            cached.heartbeat = Some(heartbeat);
            cached.folders = folders;
        }
    }
    let folders_supplied = !request.folders.is_empty();
    let heartbeat = request.heartbeat.or(cached.heartbeat);
    let folders = if folders_supplied {
        request.folders.clone()
    } else {
        cached.folders.clone()
    };
    // An empty Ping reuses the device's last parameters (kept in the state
    // store, so they survive restarts). With none known the answer is Status
    // 3 ([MS-ASCMD] Ping Status 3, missing parameters): the client resends a
    // full Ping. Z-Push does the same (ping.php). Seen on iOS 2026-11-11: the
    // iPhone came back with a new Ping 17.5 min later, on its own timer.
    let (Some(heartbeat), false) = (heartbeat, folders.is_empty()) else {
        tracing::info!(
            device_id = %device,
            heartbeat_known = heartbeat.is_some(),
            folders_known = !folders.is_empty(),
            "ping: parameters missing and none stored, Status 3"
        );
        return ping_response(state, command, 3, &[], None);
    };

    let min = state.config.ping_min_seconds;
    let max = state.config.ping_max_seconds;
    if heartbeat < min || heartbeat > max {
        let limit = heartbeat.clamp(min, max);
        tracing::info!(device_id = %device, heartbeat, limit, "ping: heartbeat out of range, Status 5");
        return ping_response(state, command, 5, &[], Some(limit));
    }

    if folders_supplied {
        match state.jmap.collections(auth).await {
            Ok(known) => {
                let known: BTreeSet<&str> = known.iter().map(|c| c.id.as_str()).collect();
                if folders.iter().any(|(id, _)| !known.contains(id.as_str())) {
                    tracing::info!(device_id = %device, "ping: unknown folder id, Status 7");
                    return ping_response(state, command, 7, &[], None);
                }
            }
            Err(error) => {
                tracing::warn!(%error, "ping: folder validation skipped, folder list unavailable");
            }
        }
        push.set_device_params(&device, heartbeat, folders.clone());
    } else if request.heartbeat.is_some() {
        push.set_device_params(&device, heartbeat, folders.clone());
    }
    if (folders_supplied || request.heartbeat.is_some())
        && (cached.heartbeat != Some(heartbeat) || cached.folders != folders)
    {
        store_ping_params(state, &eas_user, &device, heartbeat, &folders).await;
    }

    let mail_ids = watched_mail_ids(&folders);
    tracing::info!(
        device_id = %device,
        heartbeat_requested = request.heartbeat,
        heartbeat_effective = heartbeat,
        folders = folders.len(),
        mail_folders = mail_ids.len(),
        params_cached = !folders_supplied,
        "ping: started"
    );

    let _waiter = push.waiter();
    let epoch = push.begin_ping(&device);
    let mut superseded_rx = push.subscribe_supersede();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(heartbeat);
    let mut rx = push.subscribe();
    let mut first = true;
    let guard_window = Duration::from_secs(state.config.ping_resend_guard_seconds);
    loop {
        let mut recheck: Option<tokio::time::Instant> = None;
        if push.is_superseded(&device, epoch) {
            return ping_superseded(state, command, &device);
        }
        // Snapshot BEFORE probing: an event arriving after this point is
        // seen by rx.changed() below, so nothing slips between the probe and
        // the wait.
        let seen_generation = *rx.borrow_and_update();

        {
            let mut found =
                match changed_mail_folders(state, auth, &eas_user, &device, &mail_ids).await {
                    Ok(found) => found,
                    Err(error) => {
                        tracing::warn!(%error, "ping: mail change probe failed");
                        return ping_response(state, command, 8, &[], None);
                    }
                };
            match changed_item_folders(state, auth, &eas_user, &device, &folders).await {
                Ok(items) => found.extend(items),
                Err(error) => {
                    tracing::warn!(%error, "ping: item change probe failed");
                    return ping_response(state, command, 8, &[], None);
                }
            }
            if push.is_superseded(&device, epoch) {
                return ping_superseded(state, command, &device);
            }
            // Never name a folder more than 3 times without the device syncing
            // it: iOS blacklists the folder from push after more than 5.
            let mut changed = Vec::new();
            for folder in found {
                if push.announce_allowed(&device, &folder, 3) {
                    changed.push(folder);
                } else {
                    tracing::info!(device_id = %device, collection = %folder, "ping: holding back folder already announced 3 times without a sync");
                }
            }
            let loop_guard = first && push.status2_pending_sync(&device, guard_window);
            if !changed.is_empty() && !loop_guard {
                let event_latency = push.note_status2(&device);
                push.note_announced(&device, &changed);
                tracing::info!(
                    device_id = %device,
                    status = 2,
                    changed_folders = changed.len(),
                    waited_ms = started.elapsed().as_millis() as u64,
                    event_to_ping_ms = event_latency.map(|d| d.as_millis() as u64),
                    "ping: completed"
                );
                return ping_response(state, command, 2, &changed, None);
            }
            if loop_guard {
                tracing::info!(device_id = %device, "ping: pending change not yet synced, waiting instead of re-answering");
                // Do not wait for a future event: look again when the window ends.
                recheck = push
                    .status2_guard_remaining(&device, guard_window)
                    .map(|left| tokio::time::Instant::now() + left);
            }
        }
        first = false;

        // Wait for an event for a watched class or the heartbeat.
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => {
                    tracing::info!(
                        device_id = %device,
                        status = 1,
                        waited_ms = started.elapsed().as_millis() as u64,
                        "ping: completed"
                    );
                    return ping_response(state, command, 1, &[], None);
                }
                _ = async {
                    match recheck {
                        Some(at) => tokio::time::sleep_until(at).await,
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    break;
                }
                _ = superseded_rx.changed() => {
                    if push.is_superseded(&device, epoch) {
                        return ping_superseded(state, command, &device);
                    }
                }
                woke = rx.changed() => {
                    if woke.is_err() {
                        // Sender gone (cannot happen while the hub lives).
                        tokio::time::sleep_until(deadline).await;
                        return ping_response(state, command, 1, &[], None);
                    }
                    // Coalesce bursts (a multi-message delivery fires several events).
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    if push.changed_since(seen_generation).iter().any(|class| {
                        folders.iter().any(|(_, name)| Class::from_eas(name) == Some(*class))
                    }) {
                        break;
                    }
                    // Unrelated type changed: keep waiting, but advance our
                    // snapshot so we do not re-test the same generation.
                    let _ = rx.borrow_and_update();
                }
            }
        }
    }
}

async fn search_conversation_emails(
    state: &AppState,
    auth: &crate::jmap::client::AuthenticatedSession,
    conditions: Vec<serde_json::Value>,
    position: i64,
    limit: usize,
    conversation: uuid::Uuid,
) -> anyhow::Result<(Vec<crate::model::Email>, u64)> {
    let filter = if conditions.is_empty() {
        serde_json::json!({})
    } else {
        serde_json::json!({"operator":"AND","conditions":conditions})
    };
    let mut offset = 0i64;
    let mut matched = 0u64;
    let mut selected = Vec::new();
    let mut state_token: Option<String> = None;
    let mut matched_thread: Option<String> = None;
    let expected = *conversation.as_bytes();
    let expected_le = conversation.to_bytes_le();
    loop {
        let page = state
            .jmap
            .query_emails(auth, filter.clone(), offset, 100, &["id", "threadId"])
            .await?;
        let query_state = page
            .query_state
            .context("conversation search state missing")?;
        if let Some(previous) = &state_token {
            anyhow::ensure!(
                previous == &query_state,
                "mail changed during conversation search"
            );
        } else {
            state_token = Some(query_state);
        }
        let count = page.emails.len();
        for email in page.emails {
            if let Some(thread) = email.thread_id.as_deref() {
                let id = eas_conversation_id(thread);
                if id == expected || id == expected_le {
                    if let Some(previous) = &matched_thread {
                        anyhow::ensure!(previous == thread, "ambiguous conversation ID");
                    } else {
                        matched_thread = Some(thread.to_owned());
                    }
                    if matched >= position as u64 && selected.len() < limit {
                        selected.push(email.id);
                    }
                    matched += 1;
                }
            }
        }
        offset += count as i64;
        if offset as u64 == page.total {
            break;
        }
        anyhow::ensure!(
            count > 0 && (offset as u64) < page.total,
            "incomplete conversation search"
        );
    }
    let maximum = auth
        .session
        .capabilities
        .get(crate::jmap::capabilities::CORE)
        .and_then(|cap| cap.get("maxObjectsInGet"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(100)
        .clamp(1, 1000) as usize;
    let mut emails = Vec::new();
    for chunk in selected.chunks(maximum) {
        emails.extend(state.jmap.emails_by_ids(auth, chunk).await?);
    }
    anyhow::ensure!(
        emails.len() == selected.len()
            && emails
                .iter()
                .all(|email| email.thread_id.as_ref() == matched_thread.as_ref()),
        "conversation changed during search"
    );
    let check = state
        .jmap
        .query_emails(auth, filter, 0, 1, &["id", "threadId"])
        .await?;
    anyhow::ensure!(
        check.query_state == state_token && check.total == offset as u64,
        "mail changed during conversation search"
    );
    Ok((emails, matched))
}

/// Handles `Search` (MS-ASCMD 2.2.1.16) -- was advertised in
/// `SUPPORTED_COMMANDS` with no handler at all, falling through to the
/// generic 501. Backs both `Store>Name == "Mailbox"` (full-text mail
/// search, via `search_emails`/`Email/query`'s `text` filter, confirmed
/// live to search body content too, not just subject) and
/// `Store>Name == "GAL"` (directory search, via
/// `search_principals`/`Principal/query`, confirmed live against the 7
/// real principals on this instance). `Range`'s "m-n" is zero-based
/// inclusive on both ends per the real schema; converted to JMAP's
/// `position`/`limit` here since that's the one place this command
/// needs the conversion. Mailbox searches for unsupported item classes
/// return SearchTooComplex instead of an incomplete result.
async fn search(
    state: &AppState,
    auth: &crate::jmap::client::AuthenticatedSession,
    document: &wbxml::Document,
    version: ProtocolVersion,
    command: &str,
) -> Response {
    search_inner(state, auth, document, version, command, false).await
}

async fn search_inner(
    state: &AppState,
    auth: &crate::jmap::client::AuthenticatedSession,
    document: &wbxml::Document,
    version: ProtocolVersion,
    command: &str,
    internal: bool,
) -> Response {
    use wbxml::eas::{airsync as air, gal, search as srch};

    let Some(request) = wbxml::eas::search_request(document) else {
        state
            .metrics
            .eas_requests_total
            .with_label_values(&[command, "400"])
            .inc();
        return (
            StatusCode::BAD_REQUEST,
            "Search request missing Store/Name\n",
        )
            .into_response();
    };

    if !internal
        && request.store_name.eq_ignore_ascii_case("Mailbox")
        && request.range_error.is_none()
        && !request.unsupported_class_query
        && !request.unsupported_query
        && !request.invalid_conversation
        && (request.classes.len() > 1
            || (request.classes.is_empty() && request.collection_id.is_none()))
    {
        let response =
            search_multi::handle(state, auth, document, &request, version, command).await;
        state
            .metrics
            .eas_requests_total
            .with_label_values(&[command, "200"])
            .inc();
        return response;
    }

    // (position, limit): default to the first 25 results when the device omits Range
    // entirely -- the schema marks it optional inside Options.
    let (position, limit) = match request.range {
        Some((start, end)) => (start, (end - start + 1) as usize),
        None => (0, 100),
    };

    // Search results need a lightweight preview, NOT the full body/
    // attachments an explicit ItemOperations Fetch returns (that's a
    // different, later request the device makes once the user actually
    // opens a result -- confirmed necessary live: the naive first pass
    // reused `None` here the same way ItemOperations Fetch does, and a
    // 4-result mailbox search came back as a 132KB response with every
    // match's full HTML body and attachment metadata inlined). Defaults
    // to plain text truncated to 500 chars (the same real-world preview
    // size this gateway's own `apply_body_preference` tests are built
    // around) when the device didn't send its own BodyPreference.
    let body_pref = Some(BodyPreference {
        requests: if request.body_preferences.is_empty() {
            vec![RequestedBodyPreference {
                body_type: 1,
                truncation_size: Some(500),
                ..Default::default()
            }]
        } else {
            request.body_preferences.clone()
        },
        mime_data: None,
        text_body: None,
    });

    let mut builder = wbxml::eas::DocumentBuilder::new();
    builder.start(srch::SEARCH);
    builder.leaf(srch::STATUS, "1");
    builder.start(srch::RESPONSE);
    builder.start(srch::STORE);

    let search_class = request.class.as_deref().or_else(|| {
        request.collection_id.as_deref().and_then(|folder| {
            if folder.starts_with("ab_") {
                Some("Contacts")
            } else if folder.starts_with("cal_") {
                Some("Calendar")
            } else if folder.starts_with("task_") {
                Some("Tasks")
            } else if folder.starts_with("note_") {
                Some("Notes")
            } else {
                None
            }
        })
    });

    if let Some(status) = request.range_error {
        builder.leaf(srch::STATUS, status);
    } else if request.store_name.eq_ignore_ascii_case("Mailbox") {
        if request.invalid_conversation || request.unsupported_class_query {
            builder.leaf(srch::STATUS, "8");
        } else if request.unsupported_query {
            builder.leaf(srch::STATUS, "2");
        } else if search_class.is_some_and(|class| class.eq_ignore_ascii_case("Notes")) {
            if request.received_after.is_some()
                || request.received_before.is_some()
                || request.conversation_id.is_some()
                || request.or_terms.as_ref().is_some_and(|terms| {
                    terms.received_after.is_some() || terms.received_before.is_some()
                })
            {
                builder.leaf(srch::STATUS, "8");
            } else {
                let result = async {
                    let before = state.jmap.email_state(auth).await?;
                    anyhow::ensure!(!before.is_empty(), "note search state unavailable");
                    let collections = state.jmap.collections(auth).await?;
                    let mut folders: Vec<_> = collections
                        .into_iter()
                        .filter(|item| item.kind == crate::model::CollectionKind::Notes)
                        .map(|item| item.id)
                        .collect();
                    folders.sort();
                    if let Some(folder) = &request.collection_id {
                        anyhow::ensure!(folders.contains(folder), "unknown note search folder");
                        folders.retain(|id| id == folder);
                    }
                    let terms: Vec<_> = request
                        .free_texts
                        .iter()
                        .filter(|term| !term.is_empty())
                        .map(|term| term.to_lowercase())
                        .collect();
                    let alternatives = request.or_terms.as_ref().map(|or| {
                        or.texts
                            .iter()
                            .filter(|term| !term.is_empty())
                            .map(|term| term.to_lowercase())
                            .collect::<Vec<_>>()
                    });
                    let mut matches = BTreeMap::new();
                    for folder in folders {
                        let mailbox = folder
                            .strip_prefix("note_")
                            .context("invalid notes folder")?;
                        let summaries = state.jmap.list_notes(auth, mailbox, 0).await?;
                        for summary in summaries {
                            let note = state
                                .jmap
                                .get_note(auth, &summary.id)
                                .await?
                                .context("note disappeared during search")?;
                            let has = |term: &str| {
                                note.title.to_lowercase().contains(term)
                                    || note.body.to_lowercase().contains(term)
                                    || note
                                        .categories
                                        .iter()
                                        .any(|category| category.to_lowercase().contains(term))
                            };
                            if terms.iter().all(|term| has(term))
                                && alternatives
                                    .as_ref()
                                    .is_none_or(|terms| terms.iter().any(|term| has(term)))
                            {
                                anyhow::ensure!(
                                    !mailbox.contains(':') && !note.id.contains(':'),
                                    "invalid note search identifier"
                                );
                                let long_id = format!("note:{mailbox}:{}", note.id);
                                anyhow::ensure!(
                                    long_id.len() <= 256,
                                    "note search identifier too long"
                                );
                                matches.entry(note.id.clone()).or_insert((
                                    folder.clone(),
                                    long_id,
                                    note,
                                ));
                            }
                        }
                    }
                    anyhow::ensure!(
                        before == state.jmap.email_state(auth).await?,
                        "notes changed during search"
                    );
                    let total = matches.len();
                    let notes: Vec<_> = matches
                        .into_values()
                        .skip(position as usize)
                        .take(limit)
                        .collect();
                    Ok::<_, anyhow::Error>((notes, total))
                }
                .await;
                match result {
                    Ok((notes, total)) => {
                        builder.leaf(srch::STATUS, "1");
                        let count = notes.len();
                        for (folder, long_id, note) in notes {
                            builder.start(srch::RESULT);
                            builder.leaf(air::CLASS, "Notes");
                            builder.leaf(srch::LONG_ID, long_id);
                            builder.leaf(air::COLLECTION_ID, folder);
                            builder.start(srch::PROPERTIES);
                            write_note_fields(
                                &mut builder,
                                &note,
                                body_pref
                                    .as_ref()
                                    .map(|pref| pref.requests.as_slice())
                                    .unwrap_or_default(),
                            );
                            builder.end();
                            builder.end();
                        }
                        if count > 0 {
                            builder.leaf(
                                srch::RANGE,
                                format!("{position}-{}", position + count as i64 - 1),
                            );
                        }
                        builder.leaf(srch::TOTAL, total.to_string());
                    }
                    Err(error) => {
                        tracing::warn!(%error, "Search notes query failed");
                        builder.leaf(srch::STATUS, "3");
                    }
                }
            }
        } else if search_class.is_some_and(|class| class.eq_ignore_ascii_case("Tasks")) {
            if request.received_after.is_some()
                || request.received_before.is_some()
                || request.conversation_id.is_some()
                || request.or_terms.as_ref().is_some_and(|terms| {
                    terms.received_after.is_some() || terms.received_before.is_some()
                })
            {
                builder.leaf(srch::STATUS, "8");
            } else {
                let result = async {
                    fn has_text(value: &serde_json::Value, needle: &str) -> bool {
                        match value {
                            serde_json::Value::String(text) => text.to_lowercase().contains(needle),
                            serde_json::Value::Array(items) => {
                                items.iter().any(|item| has_text(item, needle))
                            }
                            serde_json::Value::Object(items) => {
                                items.values().any(|item| has_text(item, needle))
                            }
                            _ => false,
                        }
                    }
                    let before = state.jmap.calendar_snapshot_state(auth).await?;
                    let collections = state.jmap.collections(auth).await?;
                    let mut calendars: Vec<_> = collections
                        .into_iter()
                        .filter(|item| item.kind == crate::model::CollectionKind::Tasks)
                        .map(|item| item.id)
                        .collect();
                    calendars.sort();
                    if let Some(folder) = &request.collection_id {
                        anyhow::ensure!(calendars.contains(folder), "unknown task search folder");
                        calendars.retain(|id| id == folder);
                    }
                    let terms: Vec<_> = request
                        .free_texts
                        .iter()
                        .filter(|term| !term.is_empty())
                        .map(|term| term.to_lowercase())
                        .collect();
                    let alternatives = request.or_terms.as_ref().map(|or| {
                        or.texts
                            .iter()
                            .filter(|term| !term.is_empty())
                            .map(|term| term.to_lowercase())
                            .collect::<Vec<_>>()
                    });
                    let fields = ["title", "description", "locations", "participants"];
                    let mut matches = BTreeMap::new();
                    for folder in calendars {
                        let calendar = folder
                            .strip_prefix("task_")
                            .context("invalid task folder")?;
                        for (task, raw) in state
                            .jmap
                            .searchable_tasks_in_calendar(auth, calendar)
                            .await?
                        {
                            let has = |term: &str| {
                                fields.iter().any(|field| has_text(&raw[*field], term))
                                    || raw["keywords"].as_object().is_some_and(|keywords| {
                                        keywords.iter().any(|(key, value)| {
                                            value == true && key.to_lowercase().contains(term)
                                        })
                                    })
                            };
                            if terms.iter().all(|term| has(term))
                                && alternatives
                                    .as_ref()
                                    .is_none_or(|terms| terms.iter().any(|term| has(term)))
                            {
                                anyhow::ensure!(
                                    !calendar.contains(':') && !task.id.contains(':'),
                                    "invalid task search identifier"
                                );
                                let long_id = format!("task:{calendar}:{}", task.id);
                                anyhow::ensure!(
                                    long_id.len() <= 256,
                                    "task search identifier too long"
                                );
                                matches.entry(task.id.clone()).or_insert((
                                    folder.clone(),
                                    long_id,
                                    task,
                                ));
                            }
                        }
                    }
                    anyhow::ensure!(
                        before == state.jmap.calendar_snapshot_state(auth).await?,
                        "tasks changed during search"
                    );
                    let total = matches.len();
                    let tasks: Vec<_> = matches
                        .into_values()
                        .skip(position as usize)
                        .take(limit)
                        .collect();
                    Ok::<_, anyhow::Error>((tasks, total))
                }
                .await;
                match result {
                    Ok((tasks, total)) => {
                        builder.leaf(srch::STATUS, "1");
                        let count = tasks.len();
                        for (folder, long_id, task) in tasks {
                            builder.start(srch::RESULT);
                            builder.leaf(air::CLASS, "Tasks");
                            builder.leaf(srch::LONG_ID, long_id);
                            builder.leaf(air::COLLECTION_ID, folder);
                            builder.start(srch::PROPERTIES);
                            write_task_fields(
                                &mut builder,
                                &task,
                                body_pref
                                    .as_ref()
                                    .map(|pref| pref.requests.as_slice())
                                    .unwrap_or_default(),
                                version,
                                false,
                            );
                            builder.end();
                            builder.end();
                        }
                        if count > 0 {
                            builder.leaf(
                                srch::RANGE,
                                format!("{position}-{}", position + count as i64 - 1),
                            );
                        }
                        builder.leaf(srch::TOTAL, total.to_string());
                    }
                    Err(error) => {
                        tracing::warn!(%error, "Search task query failed");
                        builder.leaf(srch::STATUS, "3");
                    }
                }
            }
        } else if search_class.is_some_and(|class| class.eq_ignore_ascii_case("Calendar")) {
            if request.received_after.is_some()
                || request.received_before.is_some()
                || request.conversation_id.is_some()
                || request.or_terms.as_ref().is_some_and(|terms| {
                    terms.received_after.is_some() || terms.received_before.is_some()
                })
            {
                builder.leaf(srch::STATUS, "8");
            } else {
                let result = async {
                    let before = state.jmap.calendar_snapshot_state(auth).await?;
                    let collections = state.jmap.collections(auth).await?;
                    let mut calendars: Vec<_> = collections
                        .into_iter()
                        .filter(|item| item.kind == crate::model::CollectionKind::Calendar)
                        .map(|item| item.id)
                        .collect();
                    calendars.sort();
                    if let Some(folder) = &request.collection_id {
                        anyhow::ensure!(
                            calendars.contains(folder),
                            "unknown calendar search folder"
                        );
                        calendars.retain(|id| id == folder);
                    }
                    let terms: Vec<_> = request
                        .free_texts
                        .iter()
                        .filter(|term| !term.is_empty())
                        .map(|term| term.to_lowercase())
                        .collect();
                    let alternatives = request.or_terms.as_ref().map(|or| {
                        or.texts
                            .iter()
                            .filter(|term| !term.is_empty())
                            .map(|term| term.to_lowercase())
                            .collect::<Vec<_>>()
                    });
                    let mut matches = BTreeMap::new();
                    for folder in calendars {
                        let calendar = folder
                            .strip_prefix("cal_")
                            .context("invalid calendar folder")?;
                        for event in state
                            .jmap
                            .calendar_events_in_calendar(auth, calendar, 0)
                            .await?
                        {
                            let has =
                                |term: &str| {
                                    event.title.to_lowercase().contains(term)
                                        || event.location.as_ref().is_some_and(|value| {
                                            value.to_lowercase().contains(term)
                                        })
                                        || event.description.as_ref().is_some_and(|value| {
                                            value.value.to_lowercase().contains(term)
                                        })
                                        || event.organizer_name.as_ref().is_some_and(|value| {
                                            value.to_lowercase().contains(term)
                                        })
                                        || event.organizer_email.as_ref().is_some_and(|value| {
                                            value.to_lowercase().contains(term)
                                        })
                                        || event
                                            .categories
                                            .iter()
                                            .any(|value| value.to_lowercase().contains(term))
                                        || event.attendees.iter().any(|attendee| {
                                            attendee.email.to_lowercase().contains(term)
                                                || attendee.name.as_ref().is_some_and(|value| {
                                                    value.to_lowercase().contains(term)
                                                })
                                        })
                                };
                            if terms.iter().all(|term| has(term))
                                && alternatives
                                    .as_ref()
                                    .is_none_or(|terms| terms.iter().any(|term| has(term)))
                            {
                                anyhow::ensure!(
                                    !calendar.contains(":") && !event.id.contains(":"),
                                    "invalid calendar search identifier"
                                );
                                let long_id = format!("calendar:{calendar}:{}", event.id);
                                anyhow::ensure!(
                                    long_id.len() <= 256,
                                    "calendar search identifier too long"
                                );
                                matches.entry(event.id.clone()).or_insert((
                                    folder.clone(),
                                    long_id,
                                    event,
                                ));
                            }
                        }
                    }
                    anyhow::ensure!(
                        before == state.jmap.calendar_snapshot_state(auth).await?,
                        "calendar changed during search"
                    );
                    let total = matches.len();
                    let events: Vec<_> = matches
                        .into_values()
                        .skip(position as usize)
                        .take(limit)
                        .collect();
                    Ok::<_, anyhow::Error>((events, total))
                }
                .await;
                match result {
                    Ok((events, total)) => {
                        builder.leaf(srch::STATUS, "1");
                        let count = events.len();
                        for (folder, long_id, event) in events {
                            builder.start(srch::RESULT);
                            builder.leaf(air::CLASS, "Calendar");
                            builder.leaf(srch::LONG_ID, long_id);
                            builder.leaf(air::COLLECTION_ID, folder);
                            builder.start(srch::PROPERTIES);
                            write_calendar_fields(
                                &mut builder,
                                &event,
                                version,
                                &request.body_preferences,
                                false,
                            );
                            builder.end();
                            builder.end();
                        }
                        if count > 0 {
                            builder.leaf(
                                srch::RANGE,
                                format!("{position}-{}", position + count as i64 - 1),
                            );
                        }
                        builder.leaf(srch::TOTAL, total.to_string());
                    }
                    Err(error) => {
                        tracing::warn!(%error, "Search calendar query failed");
                        builder.leaf(srch::STATUS, "3");
                    }
                }
            }
        } else if search_class.is_some_and(|class| class.eq_ignore_ascii_case("Contacts")) {
            if request.received_after.is_some()
                || request.received_before.is_some()
                || request.conversation_id.is_some()
                || request.or_terms.as_ref().is_some_and(|terms| {
                    terms.received_after.is_some() || terms.received_before.is_some()
                })
            {
                builder.leaf(srch::STATUS, "8");
            } else {
                let result = async {
                    let folder = if let Some(folder) = &request.collection_id {
                        let collections = state.jmap.collections(auth).await?;
                        let exists = collections.iter().any(|item| {
                            item.id == *folder
                                && item.kind == crate::model::CollectionKind::Contacts
                        });
                        anyhow::ensure!(exists, "unknown contact search folder");
                        Some(
                            folder
                                .strip_prefix("ab_")
                                .context("invalid contact folder")?
                                .to_owned(),
                        )
                    } else {
                        None
                    };
                    let (contacts, total) = state
                        .jmap
                        .search_contacts(
                            auth,
                            folder.as_deref(),
                            &request.free_texts,
                            request.or_terms.as_ref().map(|or| or.texts.as_slice()),
                            position as usize,
                            limit,
                        )
                        .await?;
                    let contacts = contacts
                        .into_iter()
                        .map(|contact| {
                            anyhow::ensure!(
                                contact.id.len() <= 248,
                                "contact ID is too long for search"
                            );
                            let address_book = contact
                                .address_book_ids
                                .iter()
                                .find(|id| folder.as_ref().is_none_or(|folder| folder == *id))
                                .context("contact has no matching address book")?
                                .clone();
                            Ok((contact, address_book))
                        })
                        .collect::<anyhow::Result<Vec<_>>>()?;
                    Ok::<_, anyhow::Error>((contacts, total))
                }
                .await;
                match result {
                    Ok((contacts, total)) => {
                        builder.leaf(srch::STATUS, "1");
                        let count = contacts.len();
                        for (contact, address_book) in contacts {
                            builder.start(srch::RESULT);
                            builder.leaf(air::CLASS, "Contacts");
                            builder.leaf(srch::LONG_ID, format!("contact:{}", contact.id));
                            builder.leaf(air::COLLECTION_ID, format!("ab_{address_book}"));
                            builder.start(srch::PROPERTIES);
                            write_contact_fields(&mut builder, &contact, false);
                            builder.end();
                            builder.end();
                        }
                        if count > 0 {
                            builder.leaf(
                                srch::RANGE,
                                format!("{position}-{}", position + count as i64 - 1),
                            );
                        }
                        builder.leaf(srch::TOTAL, total.to_string());
                    }
                    Err(error) => {
                        tracing::warn!(%error, "Search contacts query failed");
                        builder.leaf(srch::STATUS, "3");
                    }
                }
            }
        } else {
            let texts = &request.free_texts;
            let results = async {
                let collections = state.jmap.collections(auth).await?;
                let mailboxes: Vec<_> = collections
                    .iter()
                    .filter(|item| item.kind == crate::model::CollectionKind::Mail)
                    .collect();
                let folders = if let Some(folder) = &request.collection_id {
                    if !mailboxes.iter().any(|item| &item.id == folder) {
                        anyhow::bail!("unknown search folder");
                    }
                    let mut scope = BTreeSet::from([folder.clone()]);
                    if request.deep_traversal {
                        loop {
                            let before = scope.len();
                            for item in &mailboxes {
                                if item
                                    .parent_id
                                    .as_ref()
                                    .is_some_and(|parent| scope.contains(parent))
                                {
                                    scope.insert(item.id.clone());
                                }
                            }
                            if before == scope.len() {
                                break;
                            }
                        }
                    }
                    Some(scope.into_iter().collect::<Vec<_>>())
                } else {
                    Some(mailboxes.iter().map(|mailbox| mailbox.id.clone()).collect::<Vec<_>>())
                };
                if folders.as_ref().is_some_and(Vec::is_empty) {
                    return Ok((Vec::new(), 0, folders));
                }
                // Stalwart indexes receivedAt at whole-second precision.
                let boundary = |value: &str, upper: bool| -> anyhow::Result<String> {
                    let time = chrono::DateTime::parse_from_rfc3339(value)?;
                    let seconds =
                        time.timestamp() + i64::from(!upper || time.timestamp_subsec_nanos() != 0);
                    let time = chrono::DateTime::from_timestamp(seconds, 0)
                        .ok_or_else(|| anyhow::anyhow!("search date out of range"))?;
                    Ok(time.format("%Y-%m-%dT%H:%M:%SZ").to_string())
                };
                let after = request
                    .received_after
                    .as_deref()
                    .map(|value| boundary(value, false))
                    .transpose()?;
                let before = request
                    .received_before
                    .as_deref()
                    .map(|value| boundary(value, true))
                    .transpose()?;
                let mut conditions: Vec<_> = texts.iter().filter(|text| !text.is_empty())
                    .map(|text| serde_json::json!({"text":text})).collect();
                if let Some(folders) = &folders {
                    conditions.push(serde_json::json!({"operator":"OR","conditions":
                        folders.iter().map(|id| serde_json::json!({"inMailbox":id})).collect::<Vec<_>>() }));
                }
                if let Some(after) = after {
                    conditions.push(serde_json::json!({"operator":"NOT","conditions":[{"before":after}]}));
                }
                if let Some(before) = before {
                    conditions.push(serde_json::json!({"before":before}));
                }
                if let Some(or) = &request.or_terms {
                    let mut alternatives: Vec<_> = or.texts.iter().filter(|text| !text.is_empty())
                        .map(|text| serde_json::json!({"text":text})).collect();
                    if let Some(after) = or.received_after.as_deref() {
                        let after = boundary(after,false)?;
                        alternatives.push(serde_json::json!({"operator":"NOT","conditions":[{"before":after}]}));
                    }
                    if let Some(before) = or.received_before.as_deref() {
                        alternatives.push(serde_json::json!({"before":boundary(before,true)?}));
                    }
                    if alternatives.is_empty() {
                        anyhow::bail!("empty Search OR");
                    }
                    conditions.push(serde_json::json!({"operator":"OR","conditions":alternatives}));
                }
                let (emails, total) = if let Some(conversation) = request.conversation_id {
                    search_conversation_emails(state, auth, conditions, position, limit, conversation).await?
                } else {
                    state.jmap.search_emails(auth,conditions,position,limit).await?
                };
                let emails = prepare_mail_bodies(
                    state,
                    auth,
                    emails,
                    &body_pref.as_ref().unwrap().requests,
                    request.mime_support,
                )
                .await?;
                Ok::<_, anyhow::Error>((emails, total, folders))
            }
            .await;
            match results {
                Ok((emails, total, folders)) => {
                    builder.leaf(srch::STATUS, "1");
                    let count = emails.len();
                    for (email, preference) in emails {
                        let email_id = email.id.clone();
                        builder.start(srch::RESULT);
                        builder.leaf(air::CLASS, "Email");
                        builder.leaf(srch::LONG_ID, email_id.clone());
                        if let Some(mailbox_id) = email
                            .mailbox_ids
                            .iter()
                            .find(|id| folders.as_ref().is_none_or(|scope| scope.contains(id)))
                        {
                            builder.leaf(air::COLLECTION_ID, mailbox_id.clone());
                        }
                        builder.start(srch::PROPERTIES);
                        write_email_fields(
                            &mut builder,
                            &email_id,
                            email,
                            Some(preference),
                            version,
                        );
                        builder.end();
                        builder.end();
                    }
                    if count > 0 {
                        builder.leaf(
                            srch::RANGE,
                            format!("{position}-{}", position + count as i64 - 1),
                        );
                    }
                    builder.leaf(srch::TOTAL, total.to_string());
                }
                Err(error) => {
                    tracing::warn!(%error, "Search mailbox query failed");
                    builder.leaf(srch::STATUS, "3");
                }
            }
        }
    } else if request.store_name.eq_ignore_ascii_case("GAL") {
        let text = request.free_text.unwrap_or_default();
        match state.jmap.all_principals(auth).await {
            Ok(principals) => {
                let principals = find::matching_principals(principals, &text);
                let total = principals.len();
                let page: Vec<_> = principals
                    .into_iter()
                    .skip(position as usize)
                    .take(limit)
                    .collect();
                let count = page.len();
                builder.leaf(srch::STATUS, "1");
                for principal in page {
                    builder.start(srch::RESULT);
                    builder.start(srch::PROPERTIES);
                    builder.leaf(gal::DISPLAY_NAME, principal.display_name);
                    if let Some(email) = principal.email {
                        builder.leaf(gal::EMAIL_ADDRESS, email);
                    }
                    builder.end();
                    builder.end();
                }
                if count > 0 {
                    builder.leaf(
                        srch::RANGE,
                        format!("{position}-{}", position + count as i64 - 1),
                    );
                }
                builder.leaf(srch::TOTAL, total.to_string());
            }
            Err(error) => {
                tracing::warn!(%error, "Search GAL query failed");
                builder.leaf(srch::STATUS, "3");
            }
        }
    } else {
        // DocumentLibrary or anything else -- no backing today, clean
        // empty result rather than an error.
        builder.leaf(srch::STATUS, "1");
        builder.leaf(srch::TOTAL, "0");
    }

    builder.end();
    builder.end();
    builder.end();

    let body = wbxml::encode_document(&builder.finish());
    if !internal {
        state
            .metrics
            .eas_requests_total
            .with_label_values(&[command, "200"])
            .inc();
    }
    (
        StatusCode::OK,
        [("Content-Type", "application/vnd.ms-sync.wbxml")],
        body,
    )
        .into_response()
}

/// Handles SendMail/SmartForward/SmartReply. Was advertised in
/// SUPPORTED_COMMANDS but had no handler at all -- fell through to the
/// generic 501 (or, for the raw-MIME transport most real clients use,
/// would have hit the WBXML-decode error path first, since a raw RFC822
/// message isn't valid WBXML). Real EAS 14.0+ clients typically use the
/// "simplified" transport: the POST body IS the MIME message directly,
/// no WBXML wrapper, with SaveInSentItems/ClientId as query params
/// instead of WBXML elements (MS-ASCMD 2.2.2.19) -- detected here by
/// checking whether the body parses as WBXML at all, falling back to
/// treating it as raw MIME when it doesn't. The WBXML-wrapped form
/// (older/other clients) is also handled via `compose_mail_request`.
///
/// Partial SmartForward attaches the original; partial plain-text SmartReply
/// quotes the original text without including its attachments.
async fn send_mail(
    state: &AppState,
    auth: &crate::jmap::client::AuthenticatedSession,
    query: &EasQuery,
    body: &Bytes,
    command: &str,
) -> Response {
    if body.is_empty() {
        state
            .metrics
            .eas_requests_total
            .with_label_values(&[command, "400"])
            .inc();
        return (StatusCode::BAD_REQUEST, "SendMail requires a body\n").into_response();
    }

    let (mut mime, save_in_sent_items, source, partial_compose) = match wbxml::decode_document(body)
    {
        Ok(document) => match wbxml::eas::compose_mail_request(&document) {
            Some(request) => {
                let source = match (
                    request.source_item_id,
                    request.source_folder_id,
                    request.source_long_id,
                ) {
                    (Some(id), Some(folder), None) if !id.is_empty() && !folder.is_empty() => {
                        Some((id, Some(folder)))
                    }
                    (None, None, Some(id)) if !id.is_empty() => Some((id, None)),
                    _ => None,
                };
                (
                    request.mime,
                    request.save_in_sent_items,
                    source,
                    (command.eq_ignore_ascii_case("SmartForward")
                        || command.eq_ignore_ascii_case("SmartReply"))
                        && !request.replace_mime,
                )
            }
            None => {
                state
                    .metrics
                    .eas_requests_total
                    .with_label_values(&[command, "400"])
                    .inc();
                return (
                    StatusCode::BAD_REQUEST,
                    "SendMail WBXML request missing Mime\n",
                )
                    .into_response();
            }
        },
        // Not valid WBXML at all -- the simplified transport, where the
        // whole body IS the MIME message. SaveInSentItems defaults to
        // true when the client doesn't say either way (matches keeping a
        // Sent Items copy, the behavior every real mail user expects).
        Err(_) => (
            body.to_vec(),
            query.save_in_sent_items.is_none() || query.save_in_sent_items.as_deref() != Some("F"),
            None,
            false,
        ),
    };

    if partial_compose {
        let Some((id, folder)) = &source else {
            return compose_error(command, "150");
        };
        let original = match state.jmap.get_email_by_id(auth, id).await {
            Ok(Some(email))
                if folder
                    .as_ref()
                    .is_none_or(|folder| email.mailbox_ids.contains(folder)) =>
            {
                email
            }
            Ok(_) => return compose_error(command, "150"),
            Err(error) => {
                tracing::warn!(%error, "failed to find forwarded message");
                return compose_error(command, "110");
            }
        };
        if command.eq_ignore_ascii_case("SmartForward") {
            let Some(blob) = original.blob_id else {
                return compose_error(command, "110");
            };
            let original = match state
                .jmap
                .download_blob(auth, &blob, "forwarded_message.eml")
                .await
            {
                Ok(original) => original,
                Err(error) => {
                    tracing::warn!(%error, "failed to fetch forwarded message");
                    return compose_error(command, "110");
                }
            };
            mime = match smart_compose::attach_original(&mime, &original) {
                Ok(mime) => mime,
                Err(error) => {
                    tracing::warn!(%error, "failed to prepare forwarded message");
                    return compose_error(command, "110");
                }
            };
        } else {
            let original = match state.jmap.email_plain_body_by_id(auth, id).await {
                Ok(Some(body)) if !body.is_truncated => body.value,
                Ok(_) => return compose_error(command, "110"),
                Err(error) => {
                    tracing::warn!(%error, "failed to fetch original reply text");
                    return compose_error(command, "110");
                }
            };
            mime = match smart_compose::inline_plain_reply(&mime, &original) {
                Ok(mime) => mime,
                Err(error) => {
                    tracing::warn!(%error, "failed to prepare reply text");
                    return compose_error(command, "110");
                }
            };
        }
    }

    match state.jmap.send_email(auth, mime, save_in_sent_items).await {
        Ok(()) => {
            if let Some((id, folder)) = source.filter(|_| {
                command.eq_ignore_ascii_case("SmartReply")
                    || command.eq_ignore_ascii_case("SmartForward")
            }) {
                let jmap = state.jmap.clone();
                let auth = auth.clone();
                let forwarded = command.eq_ignore_ascii_case("SmartForward");
                tokio::spawn(async move {
                    if let Err(error) = jmap
                        .mark_reply_or_forward(&auth, &id, folder.as_deref(), forwarded)
                        .await
                    {
                        tracing::warn!(%error, "failed to update replied or forwarded state");
                    }
                });
            }
            state
                .metrics
                .eas_requests_total
                .with_label_values(&[command, "200"])
                .inc();
            (StatusCode::OK, ()).into_response()
        }
        Err(error) => {
            tracing::warn!(%error, eas_command = command, "SendMail failed");
            state
                .metrics
                .eas_requests_total
                .with_label_values(&[command, "500"])
                .inc();
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("SendMail failed: {error}\n"),
            )
                .into_response()
        }
    }
}

fn compose_error(command: &str, status: &str) -> Response {
    let mut builder = wbxml::eas::DocumentBuilder::new();
    let root = if command.eq_ignore_ascii_case("SmartForward") {
        wbxml::eas::compose_mail::SMART_FORWARD
    } else {
        wbxml::eas::compose_mail::SMART_REPLY
    };
    builder.start(root);
    builder.leaf(wbxml::eas::compose_mail::STATUS, status);
    builder.end();
    (
        StatusCode::OK,
        [("Content-Type", "application/vnd.ms-sync.wbxml")],
        wbxml::encode_document(&builder.finish()),
    )
        .into_response()
}

/// Writes the `Get` block's content (OofState, StartTime/EndTime,
/// OofMessage x3) shared by both the Get response path and the Set ack
/// (which now echoes the full state immediately -- see the call site's
/// own doc comment for why). Caller has already opened `Oof > Status`;
/// this only writes `Get > ...` and leaves `Oof`'s own end tag to the
/// caller.
///
/// The OofState=0 (disabled) shape is unchanged from the fix earlier
/// this session, confirmed via a direct live wire comparison against
/// z-push for the identical Get request: JUST Get/OofState, no
/// OofMessage blocks at all.
///
/// The OofState=1/2 (enabled) shape was root-caused by reading THREE
/// independent, currently-production, real-device-tested EAS server
/// implementations (SOGo's SOGoActiveSyncDispatcher.m, grommunio-sync's
/// grommunio.php, and z-push's own reference) rather than continuing to
/// guess: OofState=2 for a real schedule matches the device's own Set
/// convention and every schema check performed. The actual divergence,
/// confirmed against SOGo's real, working Oof Get response: this
/// gateway used to send Enabled=1 with full ReplyMessage/BodyType on
/// ALL THREE OofMessage blocks, because JMAP's VacationResponse only has
/// one unified message with no internal/external distinction. SOGo
/// sends full content ONLY for AppliesToInternal -- AppliesToExternalKnown
/// and AppliesToExternalUnknown get Enabled=0 (hardcoded), an EMPTY
/// self-closing ReplyMessage (no text content, not even an empty
/// string), and NO BodyType element at all.
fn write_oof_get_body(
    builder: &mut wbxml::eas::DocumentBuilder,
    vacation: &Option<crate::jmap::vacation::VacationResponse>,
) {
    use wbxml::eas::settings as set;

    let is_enabled = vacation.as_ref().is_some_and(|v| v.is_enabled);
    // PROVEN (not guessed) via a clean isolated A/B, everything else
    // held constant across both runs: OofState=1 -> toggle shows ON,
    // End Date shows None. OofState=2 -> toggle shows OFF, End Date
    // shows correctly. The value flips these two UI elements in
    // OPPOSITE directions -- back to 2-for-a-real-schedule since a
    // correct End Date/message is more valuable than a correct toggle
    // for the underlying feature (which works server-side regardless of
    // what the toggle displays). See BODY_TYPE below for the next real
    // variable being tested (device's own "TEXT" casing, never
    // actually tried -- every previous test used "Text").
    let oof_state_value = if !is_enabled {
        "0"
    } else if vacation.as_ref().is_some_and(|v| v.to_date.is_some()) {
        "2"
    } else {
        "1"
    };
    builder.start(set::GET);
    builder.leaf(set::OOF_STATE, oof_state_value);
    if is_enabled {
        if let Some(vacation) = vacation {
            // Dashed ISO with a literal ".000" suffix, matching the
            // device's own StartTime/EndTime encoding exactly (same
            // failure class as the original DateReceived bug, commit
            // 1bac5ae) -- see eas_datetime_dashes's own doc comment.
            // Real fix: the iOS Automatic Reply UI has no Start Date
            // concept at all -- only an End Date toggle+field. Every
            // previous attempt at the scheduled-Oof display bug changed
            // OofMessage content or the round-trip shape and left
            // StartTime in the response untouched; never actually tried
            // dropping it. StartTime is minOccurs=0 in the real
            // [MS-ASWBXML] SettingsResponse.xsd (independently optional
            // of EndTime), so omitting it while keeping EndTime is
            // schema-valid. If the device's own "is this schedule
            // currently active" check for the toggle compares against
            // StartTime and something about that comparison is what's
            // failing, this removes it from the equation entirely.
            if let Some(to) = &vacation.to_date {
                builder.leaf(set::END_TIME, eas_datetime_dashes(to));
            }
            let reply_message = vacation
                .text_body
                .clone()
                .or_else(|| vacation.subject.clone())
                .unwrap_or_default();
            // Three SIBLING OofMessage blocks (each fully closed before
            // the next starts), NOT nested -- confirmed live via
            // idevicesyslog that iOS's WBXML *decoder* rejects
            // OofMessage nested inside OofMessage even though its own
            // *encoder* produces exactly that shape when sending a Set.
            // BODY_TYPE experiment: every previous test used "Text"
            // (mixed case) regardless of which OofState value was being
            // tested, so it was never independently isolated. The device
            // itself always sends "TEXT" (all caps, confirmed in every
            // captured Set). Matching that exactly here in case the
            // client's toggle-state derivation is pickier than a bare
            // OofState comparison.
            builder.start(set::OOF_MESSAGE);
            builder.empty_tag(set::APPLIES_TO_INTERNAL);
            builder.leaf(set::ENABLED, "1");
            builder.leaf(set::REPLY_MESSAGE, reply_message);
            builder.leaf(set::BODY_TYPE, "TEXT");
            builder.end();
            builder.start(set::OOF_MESSAGE);
            builder.empty_tag(set::APPLIES_TO_EXTERNAL_KNOWN);
            builder.leaf(set::ENABLED, "0");
            builder.empty_tag(set::REPLY_MESSAGE);
            builder.end();
            builder.start(set::OOF_MESSAGE);
            builder.empty_tag(set::APPLIES_TO_EXTERNAL_UNKNOWN);
            builder.leaf(set::ENABLED, "0");
            builder.empty_tag(set::REPLY_MESSAGE);
            builder.end();
        }
    }
    builder.end();
}

async fn settings(
    state: &AppState,
    auth: &crate::jmap::client::AuthenticatedSession,
    document: &wbxml::Document,
    version: ProtocolVersion,
    command: &str,
) -> Response {
    let include_user = wbxml::eas::contains_token(document, wbxml::eas::settings::USER_INFORMATION);
    let has_oof = wbxml::eas::contains_token(document, wbxml::eas::settings::OOF);
    let is_oof_set = wbxml::eas::contains_token(document, wbxml::eas::settings::SET);
    let mut builder = wbxml::eas::DocumentBuilder::new();
    use wbxml::eas::settings as set;

    builder.start(set::SETTINGS);
    builder.leaf(set::STATUS, "1");

    if include_user {
        builder.start(set::USER_INFORMATION);
        match state.jmap.mail_identities(auth).await {
            Ok(identities) if !identities.is_empty() => {
                builder.leaf(set::STATUS, "1");
                builder.start(set::GET);
                let modern = version != ProtocolVersion::V14_0;
                if modern {
                    builder.start(set::ACCOUNTS);
                    builder.start(set::ACCOUNT);
                    // Identity/get's first identity is also used for outbound mail.
                    // The primary EAS account must not have an AccountId.
                    if !identities[0].name.is_empty() {
                        builder.leaf(set::USER_DISPLAY_NAME, &identities[0].name);
                    }
                }
                builder.start(set::EMAIL_ADDRESSES);
                let mut addresses = BTreeSet::new();
                for identity in &identities {
                    if addresses.insert(identity.email.to_ascii_lowercase()) {
                        builder.leaf(set::SMTP_ADDRESS, &identity.email);
                    }
                }
                // JMAP identities do not designate a primary SMTP address.
                // Only identify it when the login itself matches an identity.
                if modern {
                    if let Some(identity) = identities
                        .iter()
                        .find(|identity| identity.email.eq_ignore_ascii_case(auth.username()))
                    {
                        builder.leaf(set::PRIMARY_SMTP_ADDRESS, &identity.email);
                    }
                }
                builder.end();
                if modern {
                    builder.end();
                    builder.end();
                }
                builder.end();
            }
            Ok(_) => builder.leaf(set::STATUS, "110"),
            Err(error) => {
                tracing::warn!(%error, "failed to read user mail identities");
                builder.leaf(set::STATUS, "110");
            }
        }
        builder.end();
    }

    if has_oof {
        // Automatic Replies (Out-of-Office), now backed by Stalwart's real
        // VacationResponse object (RFC 8621 -- see jmap::vacation module
        // docs for the full story, including exactly what was and wasn't
        // live-verified and why). Get reads the real account state instead
        // of always reporting disabled; Set actually persists instead of
        // silently discarding.
        //
        // The OofState=0 (disabled) shape below is unchanged from the
        // fix earlier this session, confirmed via a direct live wire
        // comparison against z-push for the identical Get request: JUST
        // Status/Get/OofState, no OofMessage blocks.
        //
        // The OofState=1 (enabled) shape was a REAL bug, confirmed live
        // by pulling the zoidberg pcap right after the user actually
        // turned Automatic Replies on and got stuck on "Loading...":
        // the device's own Set request encodes THREE OofMessage blocks,
        // and -- this is the surprising, byte-confirmed part -- they are
        // genuinely NESTED inside each other (OofMessage>AppliesToInternal
        // +siblings, then a SECOND OofMessage as a further sibling inside
        // that same block wrapping AppliesToExternalKnown, then a THIRD
        // nested inside THAT wrapping AppliesToExternalUnknown), not three
        // flat sibling blocks the way an earlier, already-reverted fix
        // this session assumed (commit ee7f2b8, reverted because it
        // wrongly added this shape to the DISABLED case). This gateway's
        // Get response only ever echoed a single OofMessage back, so the
        // device never received the shape it was itself using, and kept
        // retrying Settings in a tight loop (confirmed in the same pcap:
        // repeated Set/Get pairs roughly every 250ms) instead of ever
        // showing the toggle as settled.
        //
        // JMAP's VacationResponse (RFC 8621) has no Internal/External-
        // Known/ExternalUnknown concept at all -- just one isEnabled/
        // subject/textBody. The device's own Set in this capture used
        // Enabled=1 for Internal but Enabled=0 for both External variants
        // (the "also apply to external senders" toggle the user didn't
        // enable) -- we don't persist that per-variant distinction
        // anywhere (nothing to persist it INTO), so this echoes the
        // single stored message back into all three variants, all marked
        // Enabled=1: simpler than trying to fake a distinction we don't
        // track, and "Enabled" reflects the real overall Oof state
        // correctly for all three, which is what matters for the device
        // to stop treating the response as incomplete.
        if is_oof_set {
            let oof_state = wbxml::eas::find_text_after(document, set::OOF_STATE);
            let is_enabled = oof_state.is_some_and(|state| state != "0");
            let subject = wbxml::eas::find_text_after(document, set::REPLY_MESSAGE)
                .map(|s| s.lines().next().unwrap_or(s).to_owned());
            let text_body =
                wbxml::eas::find_text_after(document, set::REPLY_MESSAGE).map(str::to_owned);
            let from_date =
                wbxml::eas::find_text_after(document, set::START_TIME).map(str::to_owned);
            let to_date = wbxml::eas::find_text_after(document, set::END_TIME).map(str::to_owned);

            let update = crate::jmap::vacation::VacationResponseUpdate {
                is_enabled,
                subject,
                text_body,
                from_date,
                to_date,
            };
            if let Err(error) = state.jmap.set_vacation_response(auth, update).await {
                tracing::warn!(%error, "failed to persist Oof Set via VacationResponse/set");
            }
            // Real experiment: a Set ack used to be a bare `Oof>Status=1`,
            // requiring the device to make a SEPARATE Get round-trip to
            // learn the new state -- two independent requests, two
            // independent chances for the client's own request-tracking
            // to race/drop one (the log evidence for that: every capture
            // this session shows `_oofSettingsRequestsFinished called
            // back with status -1` / "received results for an unknown
            // oof settings request" immediately before the real result,
            // every single time). The [MS-ASWBXML] SettingsResponse.xsd
            // schema doesn't tie `Get` content to the request having been
            // a Get -- Oof's response shape is just `Status, Get` (both
            // optional) regardless of whether the request was a Get or a
            // Set. Re-fetching and echoing the full state immediately in
            // the Set ack removes that race entirely: the device gets
            // the authoritative state in the SAME response it's already
            // waiting on, no second request needed.
            let vacation = state.jmap.get_vacation_response(auth).await.ok().flatten();
            builder.start(set::OOF);
            builder.leaf(set::STATUS, "1");
            write_oof_get_body(&mut builder, &vacation);
            builder.end();
        } else {
            let vacation = state.jmap.get_vacation_response(auth).await.ok().flatten();
            builder.start(set::OOF);
            builder.leaf(set::STATUS, "1");
            write_oof_get_body(&mut builder, &vacation);
            builder.end();
        }
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
    auth: &crate::jmap::client::AuthenticatedSession,
    query: &EasQuery,
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
    // 12.1+: each Collection carries an AirSync SyncKey, in request order.
    let sync_keys = wbxml::eas::find_all_text_after(document, wbxml::eas::airsync::SYNC_KEY);
    let request_filters =
        wbxml::eas::find_all_text_after(document, wbxml::eas::airsync::FILTER_TYPE);
    let user = query.user.as_deref().unwrap_or_else(|| auth.username());
    let device = query.device_id.as_deref().unwrap_or("unknown");

    let mut builder = wbxml::eas::DocumentBuilder::new();
    use wbxml::eas::get_item_estimate as gie;

    builder.start(gie::GET_ITEM_ESTIMATE);
    for (i, folder_id) in folder_ids.into_iter().enumerate() {
        let key = sync_keys.get(i).map(String::as_str);
        let record = state
            .state
            .get(user, device, &folder_id)
            .await
            .ok()
            .flatten();
        // [MS-ASCMD] GetItemEstimate Status: 3 = folder not primed (SyncKey 0),
        // 4 = SyncKey does not match. Estimate counts what Sync would send.
        let (status, estimate) = match (key, &record) {
            (Some("0"), _) => ("3", 0usize),
            (Some(k), Some(r)) if r.sync_key != k => ("4", 0),
            _ if is_non_mail_collection_id(&folder_id) => {
                match (
                    item_collection_hashes(state, auth, &folder_id).await,
                    state.state.item_states(user, device, &folder_id).await,
                ) {
                    (Ok(current), Ok(items)) => {
                        let previous: BTreeMap<String, String> = items
                            .into_iter()
                            .map(|item| (item.item_id, item.hash))
                            .collect();
                        let changes = current
                            .iter()
                            .filter(|(id, hash)| previous.get(*id) != Some(*hash))
                            .count()
                            + previous
                                .keys()
                                .filter(|id| !current.contains_key(*id))
                                .count();
                        ("1", changes)
                    }
                    _ => ("2", 0),
                }
            }
            _ => {
                let seen: BTreeSet<&str> = record
                    .as_ref()
                    .map(|r| r.seen_ids.iter().map(String::as_str).collect())
                    .unwrap_or_default();
                // FilterType from this request, else the one the folder's
                // last Sync used.
                let filter = request_filters
                    .get(i)
                    .and_then(|f| f.parse::<u8>().ok())
                    .or_else(|| {
                        record
                            .as_ref()
                            .map(|r| mail_record_options(&r.jmap_state).filter)
                    })
                    .unwrap_or(0);
                let after = mail_filter_cutoff(filter, chrono::Utc::now());
                let unseen = state
                    .jmap
                    .email_ids_in_mailbox_since(auth, &folder_id, after.as_deref(), 0)
                    .await
                    .map(|ids| ids.iter().filter(|id| !seen.contains(id.as_str())).count())
                    .unwrap_or(0);
                ("1", unseen)
            }
        };
        builder.start(gie::RESPONSE);
        builder.leaf(gie::STATUS, status);
        builder.start(gie::FOLDER);
        // Class (FolderType token) is only valid up to protocol 12.1; this
        // gateway speaks 14.0, so it is not sent.
        builder.leaf(gie::FOLDER_ID, folder_id);
        builder.leaf(gie::ESTIMATE, estimate.to_string());
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
    use wbxml::eas::{self, provision as prov, settings as set};
    let policy_type = eas::find_text_after(document, prov::POLICY_TYPE);
    let has_policies = eas::contains_token(document, prov::POLICIES);
    let malformed = has_policies && policy_type.is_none();
    let mut builder = eas::DocumentBuilder::new();
    builder.start(prov::PROVISION);
    builder.leaf(prov::STATUS, if malformed { "2" } else { "1" });
    if !malformed {
        if eas::contains_token(document, set::DEVICE_INFORMATION) {
            builder.start(set::DEVICE_INFORMATION);
            builder.leaf(set::STATUS, "1");
            builder.end();
        }
        if let Some(policy_type) = policy_type {
            builder.start(prov::POLICIES);
            builder.start(prov::POLICY);
            builder.leaf(prov::POLICY_TYPE, policy_type);
            // [MS-ASPROV] 3.2.5.2: no device policy is configured. A
            // success status and key would require actual policy Data.
            builder.leaf(
                prov::STATUS,
                if policy_type == "MS-EAS-Provisioning-WBXML" {
                    "2"
                } else {
                    "3"
                },
            );
            builder.end();
            builder.end();
        }
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

/// The device moved this message itself, so it already knows it left the
/// source folder: drop it from that device's record so Ping does not report
/// "a message left the folder" back to it.
async fn forget_moved_item(
    state: &AppState,
    auth: &crate::jmap::client::AuthenticatedSession,
    query: &EasQuery,
    folder: &str,
    item: &str,
) {
    let user = query.user.as_deref().unwrap_or_else(|| auth.username());
    let Some(device) = query.device_id.as_deref() else {
        return;
    };
    if let Ok(Some(mut record)) = state.state.get(user, device, folder).await {
        let before = record.seen_ids.len();
        record.seen_ids.retain(|id| id != item);
        if record.seen_ids.len() != before {
            if let Err(error) = state.state.put(record).await {
                tracing::warn!(%error, "failed to update Sync state after MoveItems");
            }
        }
    }
    if let Ok(items) = state.state.item_states(user, device, folder).await {
        let before = items.len();
        let kept: Vec<_> = items.into_iter().filter(|i| i.item_id != item).collect();
        if kept.len() != before {
            if let Err(error) = state
                .state
                .put_item_states(user, device, folder, kept)
                .await
            {
                tracing::warn!(%error, "failed to update read state after MoveItems");
            }
        }
    }
}

async fn move_items(
    state: &AppState,
    auth: &crate::jmap::client::AuthenticatedSession,
    query: &EasQuery,
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
                Ok(()) => {
                    forget_moved_item(
                        state,
                        auth,
                        query,
                        &move_request.src_fld_id,
                        &move_request.src_msg_id,
                    )
                    .await;
                    "3"
                }
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

#[cfg(test)]
pub(crate) fn wbxml_outline_for_tests(doc: &wbxml::Document) -> String {
    wbxml_outline(doc)
}

/// Device ids whose Sync traffic is traced (env `TRACE_DEVICE_IDS`, comma
/// separated). Diagnostic only.
fn trace_enabled(device: Option<&str>) -> bool {
    let Some(device) = device else { return false };
    std::env::var("TRACE_DEVICE_IDS")
        .map(|v| v.split(',').any(|d| d.trim() == device))
        .unwrap_or(false)
}

/// Structure of a WBXML document without content: every tag as
/// `page:token` (`/` marks an empty tag), text only for protocol fields
/// (SyncKey, ServerId, Status, CollectionId, ClientId, WindowSize, Read),
/// everything else reduced to its length.
fn wbxml_outline(doc: &wbxml::Document) -> String {
    use wbxml::Node;
    const SHOWN: &[(u8, u8)] = &[
        (0, 0x18),
        (0, 0x1b),
        (0, 0x22),
        (0, 0x23),
        (17, 0x06),
        (17, 0x07),
        (0, 0x0b),
        (0, 0x0c),
        (0, 0x0d),
        (0, 0x0e),
        (0, 0x12),
        (0, 0x15),
        (2, 0x15),
    ];
    let mut out = String::new();
    let mut stack: Vec<(u8, u8)> = Vec::new();
    for node in &doc.nodes {
        match node {
            Node::Start(t) => {
                out.push_str(&format!("<{}:{:02x}", t.code_page, t.token));
                if t.has_content {
                    out.push('>');
                    stack.push((t.code_page, t.token));
                } else {
                    out.push_str("/>");
                }
            }
            Node::Text(text) => match stack.last() {
                Some(top) if SHOWN.contains(top) => out.push_str(text),
                _ => out.push_str(&format!("[{}]", text.len())),
            },
            Node::Opaque(b) => out.push_str(&format!("[opaque {}]", b.len())),
            Node::End => {
                stack.pop();
                out.push_str("</>");
            }
        }
    }
    out
}

/// Last mail Sync reply per (user, device, folder), kept so a client that
/// lost the HTTP response and resends its previous SyncKey gets the same
/// reply again instead of a full resync. Stored as one ItemState under a
/// reserved collection id: item_id "<request key>><new key>", hash = base64
/// WBXML of the reply's Collection element.
/// Reserved item-state collection holding a device's last Ping parameters,
/// so an empty Ping after a gateway restart still watches every folder the
/// device asked for ([MS-ASCMD] 2.2.1.13: the server keeps them).
const PING_PARAMS_COLLECTION: &str = "__ping__";

async fn store_ping_params(
    state: &AppState,
    user: &str,
    device: &str,
    heartbeat: u64,
    folders: &[(String, String)],
) {
    let Ok(json) = serde_json::to_string(folders) else {
        return;
    };
    let item = ItemState {
        item_id: heartbeat.to_string(),
        hash: json,
    };
    if let Err(error) = state
        .state
        .put_item_states(user, device, PING_PARAMS_COLLECTION, vec![item])
        .await
    {
        tracing::warn!(%error, "failed to store Ping parameters");
    }
}

async fn load_ping_params(
    state: &AppState,
    user: &str,
    device: &str,
) -> Option<(u64, Vec<(String, String)>)> {
    let items = state
        .state
        .item_states(user, device, PING_PARAMS_COLLECTION)
        .await
        .ok()?;
    let item = items.into_iter().next()?;
    let heartbeat = item.item_id.parse().ok()?;
    let folders: Vec<(String, String)> = serde_json::from_str(&item.hash).ok()?;
    (!folders.is_empty()).then_some((heartbeat, folders))
}

fn replay_collection_id(collection_id: &str, version: ProtocolVersion) -> String {
    if version == ProtocolVersion::V14_0 {
        format!("__replay__:{collection_id}")
    } else {
        format!("__replay__:{}:{collection_id}", version.as_str())
    }
}

async fn store_replay(
    state: &AppState,
    user: &str,
    device: &str,
    collection_id: &str,
    keys: (&str, &str, &str, &str),
    nodes: Vec<wbxml::Node>,
    version: ProtocolVersion,
) {
    use base64::Engine;
    let bytes = wbxml::encode_document(&wbxml::Document { nodes });
    let item = ItemState {
        item_id: format!("{}>{}>{}>{}", keys.0, keys.1, keys.2, keys.3),
        hash: base64::engine::general_purpose::STANDARD.encode(bytes),
    };
    if let Err(error) = state
        .state
        .put_item_states(
            user,
            device,
            &replay_collection_id(collection_id, version),
            vec![item],
        )
        .await
    {
        tracing::warn!(%error, "failed to store Sync replay");
    }
}

async fn load_replay(
    state: &AppState,
    user: &str,
    device: &str,
    collection_id: &str,
    version: ProtocolVersion,
) -> Option<(String, String, String, String, Vec<wbxml::Node>)> {
    use base64::Engine;
    let items = state
        .state
        .item_states(user, device, &replay_collection_id(collection_id, version))
        .await
        .ok()?;
    let item = items.into_iter().next()?;
    let mut parts = item.item_id.splitn(4, '>');
    let request_key = parts.next()?;
    let new_key = parts.next()?;
    let fingerprint = parts.next()?;
    let without_uploads = parts.next()?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(item.hash)
        .ok()?;
    let doc = wbxml::decode_document(&bytes).ok()?;
    Some((
        request_key.to_owned(),
        new_key.to_owned(),
        fingerprint.to_owned(),
        without_uploads.to_owned(),
        doc.nodes,
    ))
}

async fn wait_for_sync_changes(
    state: &AppState,
    auth: &crate::jmap::client::AuthenticatedSession,
    query: &EasQuery,
    collections: &[wbxml::eas::SyncCollectionRequest],
    seconds: u64,
) -> bool {
    let user = query.user.as_deref().unwrap_or_else(|| auth.username());
    let device = query.device_id.as_deref().unwrap_or("unknown");
    for collection in collections {
        if collection.sync_key == "0" || !collection.commands.is_empty() || !collection.get_changes
        {
            return false;
        }
        if !matches!(state.state.get(user, device, &collection.collection_id).await,
            Ok(Some(record)) if record.sync_key == collection.sync_key)
        {
            return false;
        }
    }
    let Some(_slot) = state.push.try_ping_slot() else {
        return false;
    };
    let push = state.push.ensure_subscriber(
        &auth.username().to_ascii_lowercase(),
        auth.session.event_source_url.as_deref(),
        auth.username(),
        auth.authorization.password(),
    );
    let _waiter = push.waiter();
    let mut rx = push.subscribe();
    let folders: Vec<_> = collections
        .iter()
        .map(|c| (c.collection_id.clone(), "Email".to_owned()))
        .collect();
    let mail = watched_mail_ids(&folders);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);
    let mut probe_failures = 0;
    loop {
        rx.borrow_and_update();
        match changed_mail_folders(state, auth, user, device, &mail).await {
            Ok(changed) if changed.is_empty() => probe_failures = 0,
            Ok(_) => return false,
            Err(_) => {
                probe_failures += 1;
                if probe_failures > 2 || tokio::time::Instant::now() >= deadline {
                    return false;
                }
                tokio::time::sleep_until(
                    (tokio::time::Instant::now() + Duration::from_millis(250)).min(deadline),
                )
                .await;
                continue;
            }
        }
        match changed_item_folders(state, auth, user, device, &folders).await {
            Ok(items) if items.is_empty() => {}
            _ => return false,
        }
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => return true,
            _ = tokio::time::sleep(Duration::from_secs(30)) => {},
            result = rx.changed() => if result.is_err() { return false; },
        }
    }
}

async fn sync_mail(
    state: &AppState,
    auth: &crate::jmap::client::AuthenticatedSession,
    query: &EasQuery,
    document: &wbxml::Document,
    version: ProtocolVersion,
    command: &str,
) -> Response {
    let user = auth.username();
    let device = query.device_id.as_deref().unwrap_or("unknown");
    let original_request = document;
    let effective = match sync_request::resolve(state, user, device, version, document).await {
        Ok(document) => document,
        Err(status) => return sync_request::error(state, command, status),
    };
    let document = &effective;
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

    if let Err(status) = sync_request::global_window(document) {
        return sync_request::error(state, command, status);
    }

    let wait_expired = match sync_request::wait_seconds(document) {
        Ok(Some(seconds)) => wait_for_sync_changes(state, auth, query, &collections, seconds).await,
        Ok(None) => false,
        Err((status, limit)) => {
            return sync_request::error_with_limit(state, command, status, limit)
        }
    };

    let push = state.push.user(&auth.username().to_ascii_lowercase());
    let sync_lock = push.sync_lock(device);
    let _sync_guard = sync_lock.lock().await;
    let effective =
        match sync_request::resolve(state, user, device, version, original_request).await {
            Ok(document) => document,
            Err(status) => return sync_request::error(state, command, status),
        };
    let document = &effective;
    let collections = wbxml::eas::sync_collections(document);
    let mut remaining = match sync_request::global_window(document) {
        Ok(window) => window,
        Err(status) => return sync_request::error(state, command, status),
    };

    if let Some(device) = query.device_id.as_deref() {
        let push = state.push.user(&auth.username().to_ascii_lowercase());
        if let Some(latency) = push.mark_synced(device) {
            tracing::info!(
                device_id = device,
                ping_to_sync_ms = latency.as_millis() as u64,
                "push: follow-up Sync after Ping Status 2"
            );
        }
    }

    let mut builder = wbxml::eas::DocumentBuilder::new();
    use wbxml::eas::airsync as air;

    builder.start(air::SYNC);
    builder.start(air::COLLECTIONS);

    let mut previous_mark = builder.mark();
    'collections: for mut collection in collections {
        remaining = remaining.saturating_sub(sync_request::response_changes(
            builder.nodes_since(previous_mark),
        ));
        if remaining == 0 {
            break;
        }
        previous_mark = builder.mark();
        if let Some(device) = query.device_id.as_deref() {
            state
                .push
                .user(&auth.username().to_ascii_lowercase())
                .folder_synced(device, &collection.collection_id);
        }
        let requested_window = collection.window_size;
        collection.window_size = collection.window_size.min(remaining);
        let request_fingerprint = uuid::Uuid::new_v5(
            &uuid::Uuid::NAMESPACE_OID,
            format!("{collection:?}").as_bytes(),
        )
        .to_string();
        let mut without_uploads = collection.clone();
        without_uploads.commands.clear();
        let without_uploads_fingerprint = uuid::Uuid::new_v5(
            &uuid::Uuid::NAMESPACE_OID,
            format!("{without_uploads:?}").as_bytes(),
        )
        .to_string();
        let mut replay_capture: Option<(usize, String, String, String)> = None;
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
        if !sync_key_valid && collection.sync_key != "0" {
            // [MS-ASCMD] lost response: the client resends the SyncKey it
            // sent last time. Answer with the same reply instead of Status 3,
            // without applying its commands again.
            if let (
                Some(record),
                Some((request_key, new_key, fingerprint, without_uploads, nodes)),
            ) = (
                previous_record.as_ref(),
                load_replay(state, user, device_id, &collection.collection_id, version).await,
            ) {
                let exact = fingerprint == request_fingerprint;
                let omitted =
                    collection.commands.is_empty() && without_uploads == request_fingerprint;
                if request_key == collection.sync_key
                    && new_key == record.sync_key
                    && (exact || omitted)
                {
                    tracing::info!(
                        device_id,
                        collection = %collection.collection_id,
                        resent_key = %collection.sync_key,
                        reply_key = %new_key,
                        "Sync: client resent its previous SyncKey, replaying the last reply"
                    );
                    if exact {
                        builder.extend(nodes);
                    } else {
                        let reply = wbxml::Document { nodes };
                        builder.start(air::COLLECTION);
                        for child in wbxml::eas::child_elements(&reply) {
                            if !matches!(child.nodes.first(), Some(wbxml::Node::Start(token))
                                if token.code_page == air::RESPONSES.code_page && token.token == air::RESPONSES.token)
                            {
                                builder.extend(child.nodes);
                            }
                        }
                        builder.end();
                    }
                    continue;
                }
            }
        }
        if !sync_key_valid {
            builder.start(air::COLLECTION);
            builder.leaf(air::SYNC_KEY, collection.sync_key);
            builder.leaf(air::COLLECTION_ID, collection.collection_id);
            builder.leaf(air::STATUS, "3");
            builder.end();
            continue;
        }

        if collection.options_count > 1
            || collection.options_class.as_deref() == Some("SMS")
            || !collection.body_preferences_valid
        {
            builder.start(air::COLLECTION);
            builder.leaf(air::SYNC_KEY, &collection.sync_key);
            builder.leaf(air::COLLECTION_ID, &collection.collection_id);
            builder.leaf(air::STATUS, "4");
            builder.end();
            continue;
        }
        let legacy_filter = previous_record
            .as_ref()
            .map(|record| mail_record_options(&record.jmap_state).filter)
            .unwrap_or(0);
        if let Err(error) = sync_options::apply(
            state,
            user,
            device_id,
            version,
            &mut collection,
            legacy_filter,
        )
        .await
        {
            tracing::warn!(%error, "failed to persist Sync options");
            builder.start(air::COLLECTION);
            builder.leaf(air::SYNC_KEY, &collection.sync_key);
            builder.leaf(air::COLLECTION_ID, &collection.collection_id);
            builder.leaf(air::STATUS, "5");
            builder.end();
            continue;
        }

        if is_non_mail_collection_id(&collection.collection_id) {
            let mark = builder.mark();
            if collection.sync_key == "0" {
                let record = SyncRecord {
                    user: user.to_owned(),
                    device_id: device_id.to_owned(),
                    collection_id: collection.collection_id.clone(),
                    sync_key: "1".to_owned(),
                    jmap_state: String::new(),
                    seen_ids: Vec::new(),
                };
                let result = async {
                    state
                        .state
                        .put_item_states(user, device_id, &collection.collection_id, Vec::new())
                        .await?;
                    state.state.put(record).await
                }
                .await;
                builder.start(air::COLLECTION);
                builder.leaf(air::SYNC_KEY, if result.is_ok() { "1" } else { "0" });
                builder.leaf(air::COLLECTION_ID, collection.collection_id.clone());
                builder.leaf(air::STATUS, if result.is_ok() { "1" } else { "5" });
                builder.end();
                if let Err(error) = result {
                    tracing::warn!(%error, "failed to prime item collection");
                }
            } else if collection.collection_id.starts_with("note_") {
                sync_notes_collection(state, auth, &mut builder, user, device_id, &collection)
                    .await;
            } else if collection.collection_id.starts_with("ab_") {
                sync_contacts_collection(state, auth, &mut builder, user, device_id, &collection)
                    .await;
            } else if collection.collection_id.starts_with("cal_") {
                sync_calendar_collection(
                    state,
                    auth,
                    &mut builder,
                    user,
                    device_id,
                    &collection,
                    version,
                )
                .await;
            } else {
                sync_tasks_collection(
                    state,
                    auth,
                    &mut builder,
                    user,
                    device_id,
                    &collection,
                    version,
                )
                .await;
            }
            let nodes = builder.nodes_since(mark);
            let reply = wbxml::Document {
                nodes: nodes.clone(),
            };
            if wbxml::eas::find_text_after(&reply, air::STATUS) == Some("1") {
                if let Some(key) = wbxml::eas::find_text_after(&reply, air::SYNC_KEY) {
                    if key != collection.sync_key {
                        store_replay(
                            state,
                            user,
                            device_id,
                            &collection.collection_id,
                            (
                                &collection.sync_key,
                                key,
                                &request_fingerprint,
                                &without_uploads_fingerprint,
                            ),
                            nodes,
                            version,
                        )
                        .await;
                    }
                }
            }
            continue;
        }

        let is_mail_collection = !collection.collection_id.starts_with("ab_")
            && !collection.collection_id.starts_with("cal_")
            && !collection.collection_id.starts_with("note_")
            && !collection.collection_id.starts_with("task_");
        let mut failed_changes = Vec::new();
        let mut add_responses: Vec<(
            String,
            Option<crate::jmap::client::DraftCreated>,
            &'static str,
        )> = Vec::new();
        let mut added_ids = BTreeSet::new();
        let draft_journal = format!("__draft_add__:{}", collection.collection_id);
        let mut draft_completed = Vec::new();
        if is_mail_collection {
            let mut adds = Vec::new();
            collection.commands.retain(|command| {
                if matches!(command.kind, wbxml::eas::SyncClientCommandKind::Add) {
                    adds.push(command.clone());
                    false
                } else {
                    true
                }
            });
            let mut client_ids = BTreeMap::new();
            for command in &adds {
                *client_ids
                    .entry(command.client_id.clone())
                    .or_insert(0usize) += 1;
            }
            for command in adds {
                if !matches!(version, ProtocolVersion::V16_0 | ProtocolVersion::V16_1)
                    || collection.sync_key == "0"
                    || !command.flag_valid
                    || (command.flag_status.is_none() && !command.flag_fields.is_empty())
                    || command.client_id.is_empty()
                    || client_ids.get(&command.client_id) != Some(&1)
                {
                    add_responses.push((command.client_id, None, "6"));
                    continue;
                }
                let identity =
                    serde_json::to_vec(&(device_id, &collection.collection_id, &command.client_id))
                        .expect("serializing draft identity");
                let reservation_id = format!(
                    "eas-draft-add-{}",
                    uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, &identity).simple()
                );
                let attempt = serde_json::to_vec(&(
                    device_id,
                    &collection.collection_id,
                    &collection.sync_key,
                    &command.client_id,
                ))
                .expect("serializing draft attempt");
                let marker = format!(
                    "eas-draft-add-{}",
                    uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, &attempt).simple()
                );
                let content_marker = format!(
                    "eas-draft-data-{}",
                    uuid::Uuid::new_v5(
                        &uuid::Uuid::NAMESPACE_OID,
                        &wbxml::encode_document(&command.document)
                    )
                    .simple()
                );
                let flag = command.flag_status.map(|status| crate::model::MailFlag {
                    status,
                    fields: command.flag_fields.clone(),
                });
                let is_draft = match state.jmap.find_mailbox_by_role(auth, "drafts").await {
                    Ok(folder) => folder.as_deref() == Some(collection.collection_id.as_str()),
                    Err(error) => {
                        tracing::warn!(%error, "failed to identify draft mailbox for Sync Add");
                        add_responses.push((command.client_id, None, "5"));
                        continue;
                    }
                };
                let mut draft = match draft::create_properties(
                    &command.document,
                    &collection.collection_id,
                    auth.username(),
                    draft::CreateOptions {
                        marker: &marker,
                        content_marker: &content_marker,
                        is_draft,
                        read: command.read,
                        flag: flag.as_ref(),
                    },
                ) {
                    Ok(properties) => properties,
                    Err(_) => {
                        add_responses.push((command.client_id, None, "6"));
                        continue;
                    }
                };
                let baseline = match state.jmap.email_state(auth).await {
                    Ok(value) => value,
                    Err(error) => {
                        tracing::warn!(%error, "failed to read draft baseline");
                        add_responses.push((command.client_id, None, "5"));
                        continue;
                    }
                };
                let reservation = ItemState {
                    item_id: reservation_id.clone(),
                    hash: serde_json::to_string(&(&baseline, &content_marker, &marker))
                        .expect("serializing draft reservation"),
                };
                let reservation = match state
                    .state
                    .reserve_item(
                        user,
                        device_id,
                        &collection.collection_id,
                        &collection.sync_key,
                        &draft_journal,
                        reservation,
                    )
                    .await
                {
                    Ok(Some(value)) => value,
                    Ok(None) | Err(_) => {
                        add_responses.push((command.client_id, None, "5"));
                        continue;
                    }
                };
                let saved = serde_json::from_str::<(String, String, String)>(&reservation.hash)
                    .or_else(|_| {
                        serde_json::from_str::<(String, String)>(&reservation.hash)
                            .map(|(baseline, content)| (baseline, content, reservation_id.clone()))
                    });
                let (baseline, reserved_content, saved_marker) = match saved {
                    Ok(value) => value,
                    Err(_) => {
                        add_responses.push((command.client_id, None, "5"));
                        continue;
                    }
                };
                if reserved_content != content_marker {
                    add_responses.push((command.client_id, None, "6"));
                    continue;
                }
                if saved_marker != marker {
                    draft = match draft::create_properties(
                        &command.document,
                        &collection.collection_id,
                        auth.username(),
                        draft::CreateOptions {
                            marker: &saved_marker,
                            content_marker: &content_marker,
                            is_draft,
                            read: command.read,
                            flag: flag.as_ref(),
                        },
                    ) {
                        Ok(properties) => properties,
                        Err(_) => {
                            add_responses.push((command.client_id, None, "6"));
                            continue;
                        }
                    };
                }
                match state
                    .jmap
                    .create_email_sync_item(
                        auth,
                        crate::jmap::client::EmailSyncItem {
                            folder: collection.collection_id.clone(),
                            is_draft,
                            replay: (saved_marker, content_marker, baseline),
                            properties: draft.properties,
                            mime: draft.mime,
                            attachments: draft.attachments,
                        },
                    )
                    .await
                {
                    Ok(created) => {
                        draft_completed.push(reservation);
                        added_ids.insert(created.id.clone());
                        add_responses.push((command.client_id, Some(created), "1"));
                    }
                    Err(error) => {
                        tracing::warn!(%error, "failed to create draft");
                        add_responses.push((command.client_id, None, "5"));
                    }
                }
            }
            collection.commands.retain(|command| {
                if matches!(command.kind, wbxml::eas::SyncClientCommandKind::Change)
                    && (!command.flag_valid || command.unsupported_mail_change)
                {
                    failed_changes.push(command.server_id.clone());
                    false
                } else {
                    true
                }
            });
        }
        let (other_commands_applied, client_state_changes) =
            if is_mail_collection && !collection.commands.is_empty() {
                match apply_mail_client_commands(
                    state,
                    auth,
                    &collection.collection_id,
                    &collection.commands,
                    collection.deletes_as_moves,
                )
                .await
                {
                    Ok(result) => result,
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
                (false, Vec::new())
            };
        let client_commands_applied = other_commands_applied || !added_ids.is_empty();
        let added_states = if added_ids.is_empty() {
            BTreeMap::new()
        } else {
            let ids: Vec<_> = added_ids.iter().cloned().collect();
            match state.jmap.email_message_states(auth, &ids).await {
                Ok(states) if states.len() == ids.len() => states,
                _ => {
                    builder.start(air::COLLECTION);
                    builder.leaf(air::SYNC_KEY, &collection.sync_key);
                    builder.leaf(air::COLLECTION_ID, &collection.collection_id);
                    builder.leaf(air::STATUS, "5");
                    builder.end();
                    continue;
                }
            }
        };
        let fetch_responses = if is_mail_collection && !collection.commands.is_empty() {
            resolve_fetch_commands(
                state,
                auth,
                &collection.commands,
                &collection.body_preferences,
                collection.mime_support,
            )
            .await
        } else {
            Vec::new()
        };

        if collection.get_changes && is_mail_collection {
            // 0 = page through the whole mailbox (no fixed cap).
            const SCAN_LIMIT: usize = 0;
            // Stored newest first (records written before that order was kept
            // are in id order; the next Sync rewrites them).
            let mut previous_order: Vec<String> = previous_record
                .as_ref()
                .map(|record| record.seen_ids.clone())
                .unwrap_or_default();
            for id in &added_ids {
                if !previous_order.contains(id) {
                    previous_order.push(id.clone());
                }
            }
            let previous_seen: BTreeSet<String> = previous_order.iter().cloned().collect();
            let previous_options = previous_record
                .as_ref()
                .map(|record| mail_record_options(&record.jmap_state));
            // Email/changes since this device's last Sync, asked before anything
            // else is read: a change made meanwhile is reported again next time
            // rather than lost. Covers held messages of any age (read flags,
            // moves, destroys); the date-window checks below stay as a fallback
            // for when the server can't calculate changes.
            let since_state = previous_options
                .as_ref()
                .map(|o| o.email_state.clone())
                .filter(|s| !s.is_empty() && collection.sync_key != "0");
            let changes = match &since_state {
                Some(since) => state
                    .jmap
                    .email_changes_since(auth, since, &collection.collection_id)
                    .await
                    .unwrap_or_else(|error| {
                        tracing::warn!(%error, "mail Sync: Email/changes failed");
                        None
                    }),
                None => None,
            };
            let mut new_email_state = match &changes {
                Some(c) => c.new_state.clone(),
                None => state.jmap.email_state(auth).await.unwrap_or_else(|error| {
                    tracing::warn!(%error, "mail Sync: reading Email state failed");
                    String::new()
                }),
            };
            let scan = match state
                .jmap
                .email_ids_in_mailbox(auth, &collection.collection_id, SCAN_LIMIT)
                .await
            {
                Ok(ids) => ids,
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
            if scan.is_empty() && !previous_seen.is_empty() {
                let exists = match state.jmap.collections(auth).await {
                    Ok(all) => all.iter().any(|c| c.id == collection.collection_id),
                    Err(_) => true,
                };
                if !exists {
                    // [MS-ASCMD] Status (Sync) 12: hierarchy changed, the client
                    // runs FolderSync. Never answer a vanished folder with a
                    // Delete for every message in it.
                    tracing::info!(
                        device_id,
                        collection = %collection.collection_id,
                        "Sync: folder no longer exists, Status 12"
                    );
                    builder.start(air::COLLECTION);
                    builder.leaf(air::SYNC_KEY, collection.sync_key);
                    builder.leaf(air::COLLECTION_ID, collection.collection_id);
                    builder.leaf(air::STATUS, "12");
                    builder.end();
                    continue;
                }
            }
            let scan_set: BTreeSet<String> = scan.iter().cloned().collect();
            // What the device has, newest first by the folder listing.
            let seen_by_date: Vec<&String> = scan
                .iter()
                .filter(|id| previous_seen.contains(id.as_str()))
                .collect();
            // The slice the Ping probe checks (see PROBE_SEEN_CHECK). Sync must
            // cover it, or a probe hit becomes a Status 2 with nothing to send.
            let probe_slice = previous_order
                .iter()
                .take(crate::jmap::client::PROBE_SEEN_CHECK);
            // Messages the device does not have yet, newest first. A SyncKey 0
            // reply sends none ([MS-ASCMD] initial Sync); afterwards each reply
            // sends up to WindowSize of them, with MoreAvailable while any
            // remain ([MS-ASCMD] 2.2.3.116).
            // FilterType limits which messages are sent; deletion and read
            // checks still use the whole folder, so a message that ages out of
            // the range is never reported deleted. It is removed from the
            // device with a SoftDelete instead (see `aged_out` below).
            let filter = collection
                .filter_type
                .or_else(|| previous_options.as_ref().map(|o| o.filter))
                .unwrap_or(0);
            let in_range: Option<Vec<String>> = match mail_filter_cutoff(filter, chrono::Utc::now())
            {
                None => None,
                Some(after) => match state
                    .jmap
                    .email_ids_in_mailbox_since(
                        auth,
                        &collection.collection_id,
                        Some(&after),
                        SCAN_LIMIT,
                    )
                    .await
                {
                    Ok(ids) => Some(ids),
                    Err(error) => {
                        tracing::warn!(
                            %error,
                            collection = collection.collection_id,
                            "Sync mail discovery (FilterType range) failed"
                        );
                        builder.start(air::COLLECTION);
                        builder.leaf(air::SYNC_KEY, collection.sync_key);
                        builder.leaf(air::COLLECTION_ID, collection.collection_id);
                        builder.leaf(air::STATUS, "5");
                        builder.end();
                        continue;
                    }
                },
            };
            let unseen: Vec<String> = in_range
                .as_ref()
                .unwrap_or(&scan)
                .iter()
                .filter(|id| !previous_seen.contains(id.as_str()))
                .cloned()
                .collect();
            let page: Vec<String> = if collection.sync_key == "0" {
                Vec::new()
            } else {
                unseen
                    .iter()
                    .take(collection.window_size.clamp(1, 100))
                    .cloned()
                    .collect()
            };
            let mut has_more = collection.sync_key != "0" && unseen.len() > page.len();
            let emails = match state.jmap.emails_by_ids(auth, &page).await {
                Ok(emails) => emails,
                Err(error) => {
                    tracing::warn!(
                        %error,
                        collection = collection.collection_id,
                        "Sync mail fetch failed"
                    );
                    builder.start(air::COLLECTION);
                    builder.leaf(air::SYNC_KEY, collection.sync_key);
                    builder.leaf(air::COLLECTION_ID, collection.collection_id);
                    builder.leaf(air::STATUS, "5");
                    builder.end();
                    continue;
                }
            };
            let fetched_ids: BTreeSet<String> =
                emails.iter().map(|email| email.id.clone()).collect();

            // Real bug, found live: `emails` above is sorted newest-first
            // and capped at the window size, so a previously-seen id
            // that's simply absent from THIS fetch could mean either
            // "genuinely deleted" or "still on the server, just pushed
            // out of the window by newer mail" -- those look identical
            // under a plain set diff. Confirm each candidate directly
            // via emails_still_in_mailbox (its own doc comment has the
            // full story) rather than assuming absence-from-window means
            // deleted; see the else-branch below for the empty-window
            // shortcut that skips this call entirely.
            // Candidates: sent messages missing from the folder listing, plus
            // the newest 200 sent ones checked directly. The listing
            // (Email/query) can trail a move by a moment while the message
            // itself (Email/get) already shows its new folder; Ping checks the
            // message itself, so Sync must too, or a Ping-announced delete
            // arrives empty and waits for the next manual refresh.
            let candidate_missing: Vec<String> = if collection.sync_key == "0" {
                Vec::new()
            } else {
                let mut ids: BTreeSet<String> =
                    previous_seen.difference(&scan_set).cloned().collect();
                ids.extend(probe_slice.clone().cloned());
                ids.extend(seen_by_date.iter().take(200).map(|id| (*id).clone()));
                if let Some(c) = &changes {
                    ids.extend(
                        c.destroyed
                            .iter()
                            .filter(|id| previous_seen.contains(id.as_str()))
                            .cloned(),
                    );
                    ids.extend(
                        c.touched
                            .iter()
                            .filter(|(id, (_, here))| !here && previous_seen.contains(id.as_str()))
                            .map(|(id, _)| id.clone()),
                    );
                }
                ids.into_iter().collect()
            };
            let mut to_remove: BTreeSet<String> = if candidate_missing.is_empty() {
                BTreeSet::new()
            } else {
                match state
                    .jmap
                    .emails_still_in_mailbox(auth, &candidate_missing, &collection.collection_id)
                    .await
                {
                    Ok(still_present) => candidate_missing
                        .iter()
                        .filter(|id| !still_present.contains(id.as_str()))
                        .cloned()
                        .collect(),
                    Err(error) => {
                        // Best-effort: if we can't confirm which
                        // candidates are real deletions, don't guess --
                        // send none this round rather than risk a false
                        // positive that deletes real mail off the
                        // device. They'll be re-evaluated next Sync.
                        tracing::warn!(
                            %error,
                            collection = collection.collection_id,
                            "failed to confirm mail deletion candidates, skipping this round"
                        );
                        BTreeSet::new()
                    }
                }
            };

            let delete_budget = collection
                .window_size
                .clamp(1, 100)
                .saturating_sub(page.len());
            if to_remove.len() > delete_budget {
                has_more = true;
                to_remove = to_remove.into_iter().take(delete_budget).collect();
            }

            // Held messages that have aged out of the device's FilterType
            // window: still in the folder (so not deleted), received before the
            // cutoff. [MS-ASCMD] SoftDelete removes them from the device only;
            // Z-Push's generic backends send an ordinary Delete for the same
            // case (MAIL_AGED_OUT picks). Never touches Stalwart. Each is sent
            // once: it leaves `seen_ids`, and comes back as an Add if the
            // window widens again.
            let aged_mode = state.config.mail_aged_out;
            let aged_out: Vec<String> = match &in_range {
                Some(range)
                    if aged_mode != crate::config::AgedOut::Keep && collection.sync_key != "0" =>
                {
                    let range: BTreeSet<&str> = range.iter().map(String::as_str).collect();
                    seen_by_date
                        .iter()
                        .filter(|id| {
                            !range.contains(id.as_str()) && !to_remove.contains(id.as_str())
                        })
                        .map(|id| (*id).clone())
                        .collect()
                }
                _ => Vec::new(),
            };
            // Adds and cleanup share the window; the rest follows with
            // MoreAvailable.
            let aged_budget = collection
                .window_size
                .clamp(1, 100)
                .saturating_sub(page.len() + to_remove.len());
            let aged_sent: Vec<String> = aged_out.iter().take(aged_budget).cloned().collect();
            if aged_out.len() > aged_sent.len() {
                has_more = true;
            }
            let aged_set: BTreeSet<String> = aged_sent.iter().cloned().collect();

            // Read-state sync: tell this device about messages read/unread
            // elsewhere (another device, webmail). `item_states` holds what
            // this device was last told; a message with no stored state (a
            // device that predates this) gets one catch-up Change.
            let stored_states: BTreeMap<String, String> = match state
                .state
                .item_states(user, device_id, &collection.collection_id)
                .await
            {
                Ok(items) => items.into_iter().map(|i| (i.item_id, i.hash)).collect(),
                Err(error) => {
                    tracing::warn!(%error, "failed to load mail read states");
                    BTreeMap::new()
                }
            };
            // Fresh start (SyncKey 0): old read records describe a copy the
            // device no longer has.
            let mut previous_states = if collection.sync_key == "0" {
                BTreeMap::new()
            } else {
                stored_states.clone()
            };
            // What this device itself just set is already known to it.
            acknowledge_mail_changes(&mut previous_states, &client_state_changes);
            for (id, metadata) in &added_states {
                previous_states.insert(id.clone(), metadata.fingerprint());
            }
            let mut state_changes: Vec<(String, EmailState)> = if collection.sync_key == "0" {
                Vec::new()
            } else {
                emails
                    .iter()
                    .filter(|email| previous_seen.contains(&email.id))
                    .filter(|email| {
                        previous_states.get(&email.id).map(String::as_str)
                            != Some(email.message_state().fingerprint().as_str())
                    })
                    .map(|email| (email.id.clone(), email.message_state()))
                    .collect()
            };
            // Messages older than this device's Sync window never show up in
            // `emails`, so their read state would stay frozen at whatever the
            // device was last told. Refresh the stored ones directly.
            let mut outside_updates: Vec<(String, EmailState)> = Vec::new();
            if collection.sync_key != "0" {
                // The newest 300 messages the device has (by date, from the
                // listing) plus the slice the Ping probe checks, including ones
                // with no stored read state yet: those get a one-time catch-up
                // Change.
                let mut picked: BTreeSet<&String> = BTreeSet::new();
                let outside: Vec<String> = seen_by_date
                    .iter()
                    .copied()
                    .take(300)
                    .chain(probe_slice.clone())
                    .filter(|id| {
                        !fetched_ids.contains(id.as_str()) && !to_remove.contains(id.as_str())
                    })
                    .filter(|id| picked.insert(*id))
                    .cloned()
                    .collect();
                if !outside.is_empty() {
                    match state.jmap.email_message_states(auth, &outside).await {
                        Ok(current) => {
                            for (id, read) in current {
                                if previous_states.get(&id).map(String::as_str)
                                    != Some(read.fingerprint().as_str())
                                {
                                    state_changes.push((id.clone(), read.clone()));
                                    outside_updates.push((id, read));
                                }
                            }
                        }
                        Err(error) => {
                            tracing::warn!(%error, "failed to refresh older message read states");
                        }
                    }
                }
            }
            // Read flags changed since the last Sync on held messages of any
            // age (Email/changes). Capped per reply; past the cap the stored
            // Email state is not advanced, so the rest comes next round.
            if let Some(c) = &changes {
                const CHANGES_CAP: usize = 500;
                let already: BTreeSet<String> =
                    state_changes.iter().map(|(id, _)| id.clone()).collect();
                let drift: Vec<(String, EmailState)> = c
                    .touched
                    .iter()
                    .filter(|(id, (_, here))| {
                        *here
                            && previous_seen.contains(id.as_str())
                            && !fetched_ids.contains(id.as_str())
                            && !to_remove.contains(id.as_str())
                            && !already.contains(id.as_str())
                    })
                    .filter(|(id, (read, _))| {
                        previous_states.get(id.as_str()).map(String::as_str)
                            != Some(read.fingerprint().as_str())
                    })
                    .map(|(id, (read, _))| (id.clone(), read.clone()))
                    .collect();
                if drift.len() > CHANGES_CAP {
                    has_more = true;
                    if let Some(since) = &since_state {
                        new_email_state = since.clone();
                    }
                }
                for (id, read) in drift.into_iter().take(CHANGES_CAP) {
                    state_changes.push((id.clone(), read.clone()));
                    outside_updates.push((id, read));
                }
            }
            // No Change for a message removed in this same reply.
            state_changes.retain(|(id, _)| !aged_set.contains(id));
            outside_updates.retain(|(id, _)| !aged_set.contains(id));
            let change_budget = collection
                .window_size
                .clamp(1, 100)
                .saturating_sub(page.len() + to_remove.len() + aged_sent.len());
            if state_changes.len() > change_budget {
                state_changes.truncate(change_budget);
                has_more = true;
                if let Some(since) = &since_state {
                    new_email_state = since.clone();
                }
            }
            let sent_changes: BTreeSet<&str> =
                state_changes.iter().map(|(id, _)| id.as_str()).collect();
            outside_updates.retain(|(id, _)| sent_changes.contains(id.as_str()));
            if !state_changes.is_empty() {
                tracing::info!(
                    device_id,
                    collection = %collection.collection_id,
                    state_changes = state_changes.len(),
                    older_than_window = outside_updates.len(),
                    "mail Sync: sending message-state changes"
                );
            }
            let draft_ids: Vec<String> = state_changes
                .iter()
                .filter(|(_, metadata)| metadata.draft_revision.is_some())
                .map(|(id, _)| id.clone())
                .collect();
            let draft_emails = match state.jmap.emails_by_ids(auth, &draft_ids).await {
                Ok(emails)
                    if emails.len() == draft_ids.len()
                        && emails.iter().all(|email| {
                            state_changes.iter().any(|(id, metadata)| {
                                id == &email.id && *metadata == email.message_state()
                            })
                        }) =>
                {
                    emails
                }
                _ => {
                    builder.start(air::COLLECTION);
                    builder.leaf(air::SYNC_KEY, &collection.sync_key);
                    builder.leaf(air::COLLECTION_ID, &collection.collection_id);
                    builder.leaf(air::STATUS, "5");
                    builder.end();
                    continue 'collections;
                }
            };
            let mut draft_changes: BTreeMap<String, (crate::model::Email, BodyPreference)> =
                match prepare_mail_bodies(
                    state,
                    auth,
                    draft_emails,
                    &collection.body_preferences,
                    collection.mime_support,
                )
                .await
                {
                    Ok(items) => items
                        .into_iter()
                        .map(|(email, pref)| (email.id.clone(), (email, pref)))
                        .collect(),
                    Err(error) => {
                        tracing::warn!(%error, "draft change preparation failed");
                        builder.start(air::COLLECTION);
                        builder.leaf(air::SYNC_KEY, &collection.sync_key);
                        builder.leaf(air::COLLECTION_ID, &collection.collection_id);
                        builder.leaf(air::STATUS, "5");
                        builder.end();
                        continue 'collections;
                    }
                };
            let mut new_states = previous_states;
            for (id, read) in &outside_updates {
                new_states.insert(id.clone(), read.fingerprint().to_owned());
            }
            // Nothing is sent on a SyncKey 0 reply, so nothing is recorded.
            if collection.sync_key != "0" {
                for email in &emails {
                    new_states.insert(
                        email.id.clone(),
                        email.message_state().fingerprint().to_owned(),
                    );
                }
            }
            new_states.retain(|id, _| !to_remove.contains(id.as_str()) && !aged_set.contains(id));
            // [MS-ASCMD] initial Sync: the reply to SyncKey 0 carries only the
            // new SyncKey; items come on the next request. iOS 27 rejects a
            // <Commands> block here ("No parse rule ... codePage 0x0 token
            // 0x16") and fails the whole folder sync.
            let emails_to_send: Vec<_> = if collection.sync_key == "0" {
                Vec::new()
            } else {
                emails
                    .into_iter()
                    .filter(|email| !previous_seen.contains(&email.id))
                    .collect()
            };
            let emails_to_send = match prepare_mail_bodies(
                state,
                auth,
                emails_to_send,
                &collection.body_preferences,
                collection.mime_support,
            )
            .await
            {
                Ok(emails) => emails,
                Err(error) => {
                    tracing::warn!(%error, "Sync body preparation failed");
                    builder.start(air::COLLECTION);
                    builder.leaf(air::SYNC_KEY, &collection.sync_key);
                    builder.leaf(air::COLLECTION_ID, &collection.collection_id);
                    builder.leaf(air::STATUS, "5");
                    builder.end();
                    continue 'collections;
                }
            };
            let new_sync_key = if collection.sync_key == "0"
                || !emails_to_send.is_empty()
                || !to_remove.is_empty()
                || !aged_sent.is_empty()
                || !state_changes.is_empty()
                || client_commands_applied
            {
                next_sync_key(&collection.sync_key)
            } else {
                collection.sync_key.clone()
            };

            if let Err(error) = state
                .state
                .put_with_items_and_clear(
                    SyncRecord {
                        user: user.to_owned(),
                        device_id: device_id.to_owned(),
                        collection_id: collection.collection_id.clone(),
                        sync_key: new_sync_key.clone(),
                        // Mail records keep the device's Sync window here so Ping
                        // can judge "new" the same way Sync does.
                        jmap_state: mail_record_state(requested_window, filter, &new_email_state),
                        // SyncKey 0 means the device threw its copy of the folder
                        // away: what it has now is exactly what this response
                        // sends. Merging the old list kept messages it no longer
                        // has (moved/deleted), so Ping kept reporting them gone.
                        seen_ids: if collection.sync_key == "0" {
                            Vec::new()
                        } else {
                            // Newest first, in listing order; ids the listing does
                            // not show (yet) keep their place at the end.
                            let keep = |id: &String| {
                                (previous_seen.contains(id) || fetched_ids.contains(id))
                                    && !to_remove.contains(id)
                                    && !aged_set.contains(id)
                            };
                            let mut order: Vec<String> =
                                scan.iter().filter(|id| keep(id)).cloned().collect();
                            order.extend(
                                previous_order
                                    .iter()
                                    .chain(fetched_ids.iter())
                                    .filter(|id| !scan_set.contains(*id) && keep(id))
                                    .cloned(),
                            );
                            let mut dedup = BTreeSet::new();
                            order.retain(|id| dedup.insert(id.clone()));
                            order
                        },
                    },
                    (new_states != stored_states).then(|| {
                        new_states
                            .into_iter()
                            .map(|(item_id, hash)| ItemState { item_id, hash })
                            .collect()
                    }),
                    Some((draft_journal.clone(), draft_completed.clone())),
                )
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

            tracing::info!(
                device_id,
                collection = %collection.collection_id,
                client_key = %collection.sync_key,
                new_key = %new_sync_key,
                adds = emails_to_send.len(),
                deletes = to_remove.len(),
                aged_out = aged_sent.len(),
                state_changes = state_changes.len(),
                more_available = has_more,
                filter_type = filter,
                mailbox_items = scan.len(),
                eligible_items = in_range.as_ref().map_or(scan.len(), Vec::len),
                "mail Sync reply"
            );
            if new_sync_key != collection.sync_key {
                replay_capture = Some((
                    builder.mark(),
                    collection.collection_id.clone(),
                    collection.sync_key.clone(),
                    new_sync_key.clone(),
                ));
            }
            builder.start(air::COLLECTION);
            builder.leaf(air::SYNC_KEY, new_sync_key);
            builder.leaf(air::COLLECTION_ID, collection.collection_id);
            builder.leaf(air::STATUS, "1");
            write_mail_responses(
                &mut builder,
                fetch_responses,
                failed_changes,
                add_responses,
                version,
            );
            // Element order matters to a strict WBXML parser (iOS's
            // included) -- MoreAvailable must come AFTER Responses and
            // BEFORE Commands, confirmed against the working PHP z-push
            // reference's own encoder (config/z-push/sync.php: its Fetch-
            // responses loop runs, THEN MoreAvailable, THEN SYNC_PERFORM/
            // Commands). This was wrong (emitted before Responses) from
            // when the tag was first added.
            //
            // Only meaningful on the first-sync full-window fetch -- a
            // non-first Sync's emails_to_send is already the small
            // unseen-since-last-time diff, not itself windowed by
            // `limit`, so `has_more` (from the raw unfiltered query)
            // doesn't describe it.
            // Never on a SyncKey 0 reply (it carries no items).
            if has_more {
                // Presence-only element: a bare tag with no content and no END.
                // Written as start+end (content bit set) iOS 27 rejects the
                // whole reply, so the page never lands and the device retries.
                builder.empty_tag(air::MORE_AVAILABLE);
            }

            if !emails_to_send.is_empty()
                || !to_remove.is_empty()
                || !aged_sent.is_empty()
                || !state_changes.is_empty()
            {
                builder.start(air::COMMANDS);
                for (email, preference) in emails_to_send {
                    write_email_add(&mut builder, email, Some(preference), version);
                }
                for id in &to_remove {
                    builder.start(air::DELETE);
                    builder.leaf(air::SERVER_ID, id.clone());
                    builder.end();
                }
                let aged_tag = if aged_mode == crate::config::AgedOut::Delete {
                    air::DELETE
                } else {
                    air::SOFT_DELETE
                };
                for id in &aged_sent {
                    builder.start(aged_tag);
                    builder.leaf(air::SERVER_ID, id.clone());
                    builder.end();
                }
                for (id, read) in &state_changes {
                    if let Some((email, preference)) = draft_changes.remove(id) {
                        builder.start(air::CHANGE);
                        builder.leaf(air::SERVER_ID, id);
                        builder.start(air::APPLICATION_DATA);
                        write_email_fields(&mut builder, id, email, Some(preference), version);
                        builder.end();
                        builder.end();
                    } else {
                        write_email_state_change(&mut builder, id, read.clone());
                    }
                }
                builder.end();
            }
        } else {
            let uploaded_items = if !client_state_changes.is_empty() || !added_states.is_empty() {
                let items = match state
                    .state
                    .item_states(user, device_id, &collection.collection_id)
                    .await
                {
                    Ok(items) => items,
                    Err(error) => {
                        tracing::warn!(%error, "failed to load uploaded message state");
                        builder.start(air::COLLECTION);
                        builder.leaf(air::SYNC_KEY, &collection.sync_key);
                        builder.leaf(air::COLLECTION_ID, &collection.collection_id);
                        builder.leaf(air::STATUS, "5");
                        builder.end();
                        continue;
                    }
                };
                let mut items: BTreeMap<String, String> = items
                    .into_iter()
                    .map(|item| (item.item_id, item.hash))
                    .collect();
                acknowledge_mail_changes(&mut items, &client_state_changes);
                for (id, metadata) in &added_states {
                    items.insert(id.clone(), metadata.fingerprint());
                }
                Some(
                    items
                        .into_iter()
                        .map(|(item_id, hash)| ItemState { item_id, hash })
                        .collect(),
                )
            } else {
                None
            };
            let is_first_sync = collection.sync_key == "0";
            let advance = is_first_sync || client_commands_applied;
            let new_sync_key = if advance {
                next_sync_key(&collection.sync_key)
            } else {
                collection.sync_key.clone()
            };
            if advance || collection.options_count > 0 || !draft_completed.is_empty() {
                if let Err(error) = state
                    .state
                    .put_with_items_and_clear(
                        SyncRecord {
                            user: user.to_owned(),
                            device_id: device_id.to_owned(),
                            collection_id: collection.collection_id.clone(),
                            sync_key: new_sync_key.clone(),
                            jmap_state: if collection.options_count > 0 {
                                let previous = mail_record_options(
                                    previous_record
                                        .as_ref()
                                        .map(|record| record.jmap_state.as_str())
                                        .unwrap_or(""),
                                );
                                mail_record_state(
                                    previous.window,
                                    collection.filter_type.unwrap_or(0),
                                    &previous.email_state,
                                )
                            } else {
                                previous_record
                                    .as_ref()
                                    .map(|record| record.jmap_state.clone())
                                    .unwrap_or_default()
                            },
                            seen_ids: {
                                let mut ids = previous_record
                                    .as_ref()
                                    .map(|record| record.seen_ids.clone())
                                    .unwrap_or_default();
                                for id in &added_ids {
                                    if !ids.contains(id) {
                                        ids.push(id.clone());
                                    }
                                }
                                ids
                            },
                        },
                        uploaded_items,
                        Some((draft_journal.clone(), draft_completed.clone())),
                    )
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
            if new_sync_key != collection.sync_key {
                replay_capture = Some((
                    builder.mark(),
                    collection.collection_id.clone(),
                    collection.sync_key.clone(),
                    new_sync_key.clone(),
                ));
            }
            builder.start(air::COLLECTION);
            builder.leaf(air::SYNC_KEY, new_sync_key);
            builder.leaf(air::COLLECTION_ID, collection.collection_id);
            builder.leaf(air::STATUS, "1");
            write_mail_responses(
                &mut builder,
                fetch_responses,
                failed_changes,
                add_responses,
                version,
            );
        }

        builder.end();
        if let Some((mark, collection_id, request_key, new_key)) = replay_capture.take() {
            let nodes = builder.nodes_since(mark);
            store_replay(
                state,
                user,
                device_id,
                &collection_id,
                (
                    &request_key,
                    &new_key,
                    &request_fingerprint,
                    &without_uploads_fingerprint,
                ),
                nodes,
                version,
            )
            .await;
        }
    }

    builder.end();
    builder.end();

    let reply = builder.finish();
    if let Err(error) = sync_request::save(state, user, device, version, document, &reply).await {
        tracing::warn!(%error, "failed to cache Sync request");
        return sync_request::error(state, command, "5");
    }
    if trace_enabled(query.device_id.as_deref()) {
        tracing::info!(
            device_id = query.device_id.as_deref().unwrap_or(""),
            request = %wbxml_outline(document),
            reply = %wbxml_outline(&reply),
            "trace: Sync"
        );
    }
    let body = if wait_expired && sync_request::empty_timeout_response(document, &reply) {
        Vec::new()
    } else {
        wbxml::encode_document(&reply)
    };
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
    deletes_as_moves: bool,
) -> anyhow::Result<(bool, Vec<(String, EmailState)>)> {
    let mut applied = false;
    let mut metadata_changes = Vec::new();
    for command in commands {
        match command.kind {
            wbxml::eas::SyncClientCommandKind::Change => {
                let flag = command
                    .flag_status
                    .or_else(|| (!command.flag_fields.is_empty()).then_some(u8::MAX))
                    .map(|status| crate::model::MailFlag {
                        status,
                        fields: command.flag_fields.clone(),
                    });
                if command.read.is_some() || flag.is_some() {
                    let metadata = state
                        .jmap
                        .update_email_state(auth, &command.server_id, command.read, flag.as_ref())
                        .await?;
                    applied = true;
                    metadata_changes.push((command.server_id.clone(), metadata));
                }
            }
            wbxml::eas::SyncClientCommandKind::Delete => {
                state
                    .jmap
                    .delete_email(auth, &command.server_id, collection_id, deletes_as_moves)
                    .await?;
                applied = true;
            }
            wbxml::eas::SyncClientCommandKind::Add => {
                anyhow::bail!("mail draft creation is not implemented")
            }
            wbxml::eas::SyncClientCommandKind::Fetch => {}
        }
    }
    Ok((applied, metadata_changes))
}

fn acknowledge_mail_changes(
    previous: &mut BTreeMap<String, String>,
    changes: &[(String, EmailState)],
) {
    for (id, metadata) in changes {
        if let Some(hash) = previous.get_mut(id) {
            *hash = metadata.fingerprint();
        }
    }
}

fn read_hash(read: bool) -> &'static str {
    if read {
        "1"
    } else {
        "0"
    }
}

fn write_email_flag(builder: &mut wbxml::eas::DocumentBuilder, metadata: &EmailState) {
    use wbxml::eas::email as mail;
    if let Some(flag) = &metadata.flag {
        builder.start(mail::FLAG);
        builder.leaf(mail::FLAG_STATUS, flag.status.to_string());
        for (page, token, value) in &flag.fields {
            builder.leaf(
                wbxml::token::Token {
                    code_page: *page,
                    token: *token,
                    has_content: false,
                    has_attributes: false,
                },
                value,
            );
        }
        builder.end();
    } else if metadata.flagged {
        builder.start(mail::FLAG);
        builder.leaf(mail::FLAG_STATUS, "2");
        builder.leaf(mail::FLAG_TYPE, "Flag for follow up");
        builder.end();
    } else {
        builder.empty_tag(mail::FLAG);
    }
}

fn write_email_state_change(
    builder: &mut wbxml::eas::DocumentBuilder,
    id: &str,
    metadata: EmailState,
) {
    use wbxml::eas::{airsync as air, email as mail};
    builder.start(air::CHANGE);
    builder.leaf(air::SERVER_ID, id.to_owned());
    builder.start(air::APPLICATION_DATA);
    builder.leaf(mail::READ, read_hash(metadata.read));
    write_email_flag(builder, &metadata);
    builder.end();
    builder.end();
}

/// Resolves each Sync-embedded `<Fetch>` command (inside `<Commands>`,
/// distinct from the separate ItemOperations command) to its full email.
/// This is the mechanism iOS actually uses to fetch a message's full body
/// when a user opens it, at least on some protocol versions/iOS builds --
/// confirmed live: it never calls ItemOperations at all, sends this
/// instead, one per opened message. Previously entirely ignored (logged
/// and dropped), so opening any email hung forever waiting for a
/// `<Responses><Fetch>` that never came, even though ItemOperations
/// (implemented earlier the same session, for the mechanism iOS turned
/// out not to actually use here) worked fine in isolation.
async fn resolve_fetch_commands(
    state: &AppState,
    auth: &crate::jmap::client::AuthenticatedSession,
    commands: &[wbxml::eas::SyncClientCommand],
    body_preferences: &[RequestedBodyPreference],
    mime_support: u8,
) -> Vec<(
    String,
    anyhow::Result<Option<(crate::model::Email, BodyPreference)>>,
)> {
    let mut results = Vec::new();
    for command in commands {
        if command.kind == wbxml::eas::SyncClientCommandKind::Fetch {
            let result = state.jmap.get_email_by_id(auth, &command.server_id).await;
            let result = match result {
                Ok(Some(email)) => {
                    body_preferences_for_email(state, auth, &email, body_preferences, mime_support)
                        .await
                        .map(|preference| Some((email, preference)))
                }
                Ok(None) => Ok(None),
                Err(error) => Err(error),
            };
            results.push((command.server_id.clone(), result));
        }
    }
    results
}

type FetchResult = (
    String,
    anyhow::Result<Option<(crate::model::Email, BodyPreference)>>,
);

fn write_mail_responses(
    builder: &mut wbxml::eas::DocumentBuilder,
    results: Vec<FetchResult>,
    failed_changes: Vec<String>,
    add_responses: Vec<(
        String,
        Option<crate::jmap::client::DraftCreated>,
        &'static str,
    )>,
    version: ProtocolVersion,
) {
    use wbxml::eas::{airsync as air, airsync_base as base};

    if results.is_empty() && failed_changes.is_empty() && add_responses.is_empty() {
        return;
    }
    builder.start(air::RESPONSES);
    for (client_id, created, status) in add_responses {
        builder.start(air::ADD);
        builder.leaf(air::CLIENT_ID, client_id);
        if let Some(created) = &created {
            builder.leaf(air::SERVER_ID, &created.id);
        }
        builder.leaf(air::STATUS, status);
        if let Some(created) = created.filter(|item| !item.attachments.is_empty()) {
            builder.start(air::APPLICATION_DATA);
            builder.start(base::ATTACHMENTS);
            for (client_id, blob_id, name) in created.attachments {
                builder.start(base::ATTACHMENT);
                builder.leaf(base::CLIENT_ID, client_id);
                builder.leaf(base::FILE_REFERENCE, format!("{blob_id}||{name}"));
                builder.end();
            }
            builder.end();
            builder.end();
        }
        builder.end();
    }
    for server_id in failed_changes {
        builder.start(air::CHANGE);
        builder.leaf(air::SERVER_ID, server_id);
        builder.leaf(air::STATUS, "6");
        builder.end();
    }
    for (server_id, result) in results {
        builder.start(air::FETCH);
        builder.leaf(air::SERVER_ID, server_id.clone());
        match result {
            Ok(Some((email, preference))) => {
                builder.leaf(air::STATUS, "1");
                builder.start(air::APPLICATION_DATA);
                write_email_fields(builder, &server_id, email, Some(preference), version);
                builder.end();
            }
            Ok(None) => {
                tracing::warn!(
                    server_id = server_id.as_str(),
                    "Sync Fetch: email not found"
                );
                builder.leaf(air::STATUS, "8");
            }
            Err(error) => {
                tracing::warn!(%error, server_id = server_id.as_str(), "Sync Fetch failed");
                builder.leaf(air::STATUS, "5");
            }
        }
        builder.end();
    }
    builder.end();
}

#[derive(Debug, Clone)]
struct MimeBody {
    data: bytes::Bytes,
    full_size: usize,
}

impl MimeBody {
    fn from_download(data: Vec<u8>, preference: &RequestedBodyPreference) -> Self {
        let full_size = data.len();
        let limit = preference
            .truncation_size
            .unwrap_or(full_size)
            .min(full_size);
        let data = if preference.all_or_none && limit < full_size {
            bytes::Bytes::new()
        } else if limit < full_size {
            bytes::Bytes::copy_from_slice(&data[..limit])
        } else {
            bytes::Bytes::from(data)
        };
        Self { data, full_size }
    }
}

#[derive(Debug, Clone, Default)]
struct BodyPreference {
    requests: Vec<RequestedBodyPreference>,
    mime_data: Option<MimeBody>,
    text_body: Option<PreparedTextBody>,
}

#[derive(Debug, Clone)]
struct PreparedTextBody {
    body_type: EmailBodyType,
    data: Option<String>,
    full_size: usize,
    truncated: bool,
    preview: Option<String>,
}

fn prepare_text_body(
    body: crate::model::EmailBody,
    preference: Option<RequestedBodyPreference>,
    native_preview: Option<&str>,
) -> PreparedTextBody {
    let preview_limit = preference.and_then(|pref| pref.preview).unwrap_or(255);
    let preview = (preview_limit > 0).then(|| {
        native_preview
            .map(str::to_owned)
            .unwrap_or_else(|| plain_text_preview(&body))
            .chars()
            .take(preview_limit)
            .collect()
    });
    let (body_type, value, full_size, truncated) = apply_body_preference(body, preference);
    let data = if truncated && preference.is_some_and(|pref| pref.all_or_none) {
        None
    } else {
        Some(value)
    };
    PreparedTextBody {
        body_type,
        data,
        full_size,
        truncated,
        preview,
    }
}

async fn body_preferences_for_email(
    state: &AppState,
    auth: &AuthenticatedSession,
    email: &crate::model::Email,
    requests: &[RequestedBodyPreference],
    mime_support: u8,
) -> anyhow::Result<BodyPreference> {
    let allow_mime = mime_support == 2 || (mime_support == 1 && email.smime.is_some());
    let mut effective = requests.to_vec();
    if !allow_mime {
        effective.retain(|pref| pref.body_type != 4);
        if effective.is_empty() {
            if let Some(pref) = requests.iter().find(|pref| pref.body_type == 4) {
                effective.push(RequestedBodyPreference {
                    body_type: match email.body.as_ref().map(|body| body.body_type) {
                        Some(EmailBodyType::Html) => 2,
                        _ => 1,
                    },
                    ..*pref
                });
            }
        }
    }
    if allow_mime && email.smime.is_some() && effective.iter().any(|pref| pref.body_type == 4) {
        effective.retain(|pref| pref.body_type == 4);
    }
    let requests = effective.as_slice();
    let complete_body = if email.body.as_ref().is_some_and(|body| body.is_truncated)
        && (requests.is_empty() || requests.iter().any(|pref| matches!(pref.body_type, 1..=3)))
    {
        let body = state
            .jmap
            .email_body_by_id(auth, &email.id, 0)
            .await?
            .ok_or_else(|| anyhow::anyhow!("JMAP omitted the complete message body"))?;
        if body.is_truncated {
            anyhow::bail!("JMAP returned a truncated body for an unlimited request");
        }
        Some(body)
    } else {
        None
    };
    let body = complete_body.as_ref().or(email.body.as_ref());
    let native_fits = body.is_some_and(|body| {
        let native_type = match body.body_type {
            EmailBodyType::Plain => 1,
            EmailBodyType::Html => 2,
            EmailBodyType::Rtf => 3,
        };
        requests.iter().any(|pref| {
            pref.body_type == native_type
                && (!pref.all_or_none
                    || pref
                        .truncation_size
                        .is_none_or(|limit| body.value.len() <= limit))
        })
    });
    let mime_data = if let Some(preference) = requests
        .iter()
        .find(|pref| pref.body_type == 4 && !native_fits)
    {
        let id = email
            .blob_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("message has no MIME blob"))?;
        Some(MimeBody::from_download(
            state.jmap.download_blob(auth, id, "message.eml").await?,
            preference,
        ))
    } else {
        None
    };
    let selected = select_body_preference(body, mime_data.as_ref(), requests);
    let text_body = complete_body
        .filter(|_| selected.is_none_or(|pref| matches!(pref.body_type, 1..=3)))
        .map(|body| prepare_text_body(body, selected, email.preview.as_deref()));
    Ok(BodyPreference {
        requests: if text_body.is_some() || selected.is_some_and(|pref| pref.body_type == 4) {
            selected.into_iter().collect()
        } else {
            requests.to_vec()
        },
        mime_data,
        text_body,
    })
}

async fn prepare_mail_bodies(
    state: &AppState,
    auth: &AuthenticatedSession,
    emails: Vec<crate::model::Email>,
    requests: &[RequestedBodyPreference],
    mime_support: u8,
) -> anyhow::Result<Vec<(crate::model::Email, BodyPreference)>> {
    let mut prepared = Vec::with_capacity(emails.len());
    for email in emails {
        let preference =
            body_preferences_for_email(state, auth, &email, requests, mime_support).await?;
        prepared.push((email, preference));
    }
    Ok(prepared)
}

fn write_email_add(
    builder: &mut wbxml::eas::DocumentBuilder,
    email: crate::model::Email,
    body_pref: Option<BodyPreference>,
    version: ProtocolVersion,
) {
    use wbxml::eas::airsync as air;

    let email_id = email.id.clone();
    builder.start(air::ADD);
    builder.leaf(air::SERVER_ID, email.id.clone());
    builder.start(air::APPLICATION_DATA);
    write_email_fields(builder, &email_id, email, body_pref, version);
    builder.end();
    builder.end();
}

fn write_mime_data(builder: &mut wbxml::eas::DocumentBuilder, data: bytes::Bytes) {
    let token = wbxml::eas::airsync_base::DATA;
    match std::str::from_utf8(&data) {
        Ok(text) if !text.contains('\0') => builder.leaf(token, text.to_owned()),
        _ => builder.opaque_leaf(token, data),
    }
}

/// Writes the Email-class field set (Subject, DateReceived, From/To/Cc,
/// Importance, Read, Body/NativeBodyType) into whatever container the
/// caller already opened -- ApplicationData for a Sync Add, Properties for
/// an ItemOperations Fetch response. Shared so the two response shapes
/// can't drift out of sync with each other.
fn write_email_fields(
    builder: &mut wbxml::eas::DocumentBuilder,
    email_id: &str,
    email: crate::model::Email,
    body_pref: Option<BodyPreference>,
    version: ProtocolVersion,
) {
    use wbxml::eas::{airsync_base as base, email as mail, email2};
    let message_state = email.message_state();

    // Field order matches the real z-push-stalwart-jmap source (PR #187,
    // pinned in config/z-push/Dockerfile: src/lib/syncobjects/
    // syncmail.php's own $mapping, protocol version 14.0's actual final
    // shape after its >=2.5/>=12.0/>=14.0 gates and unset() calls are
    // applied): To, Cc, From, Subject, DateReceived, DisplayTo,
    // Importance, Read, MessageClass, [AirSyncBase:Body],
    // [AirSyncBase:Attachments], [AirSyncBase:NativeBodyType]. This
    // gateway had MessageClass and Subject first, DateReceived third,
    // From/To/DisplayTo/Cc all out of order, and Attachments after
    // NativeBodyType instead of before -- almost nothing was in the
    // right slot. This is very likely the actual explanation for the
    // original "every message shows the same time" symptom the whole
    // session started from: DateReceived itself was always proven
    // correct on the wire (repeated bracketing tests), but iOS's WBXML
    // parser has proven to be position-sensitive, not a flat tag
    // lookup, for every other object checked against this same source
    // (Attachment, Preview, Notes) -- a field in the wrong slot can be
    // silently misassigned rather than hard-rejected, which fits a
    // "wrong value" symptom instead of a parse error far better than
    // any hypothesis tried before this source was found.
    if !email.to.is_empty() {
        builder.leaf(mail::TO, email.to.clone());
    }
    if !email.cc.is_empty() {
        builder.leaf(mail::CC, email.cc);
    }
    if !email.from.is_empty() {
        builder.leaf(mail::FROM, email.from);
    }
    builder.leaf(mail::SUBJECT, email.subject);
    if let Some(received_at) = email.received_at {
        // Real bug, root-caused via a direct wire comparison against
        // z-push for the same real message: DateReceived is the one EAS
        // date field that wants the DASHES form (see
        // eas_datetime_dashes's own docs), not the compact form every
        // other date field uses. Sending the compact form here -- which
        // an earlier session concluded was the fix -- was itself the
        // bug, and very likely the actual explanation for the original
        // "every message shows the same time" symptom.
        let formatted = eas_datetime_dashes(&received_at);
        tracing::debug!(
            server_id = email_id,
            raw_received_at = received_at.as_str(),
            formatted_date_received = formatted.as_str(),
            "email date summary"
        );
        builder.leaf(mail::DATE_RECEIVED, formatted);
    } else {
        tracing::debug!(
            server_id = email_id,
            "email has no receivedAt -- DateReceived omitted entirely"
        );
    }
    if !email.to.is_empty() {
        builder.leaf(mail::DISPLAY_TO, email.to);
    }
    builder.leaf(mail::IMPORTANCE, email.importance.to_string());
    builder.leaf(mail::READ, if email.read { "1" } else { "0" });
    builder.leaf(
        mail::MESSAGE_CLASS,
        match email.smime {
            Some(crate::model::SmimeKind::Opaque) => "IPM.Note.SMIME",
            Some(crate::model::SmimeKind::MultipartSigned) => "IPM.Note.SMIME.MultipartSigned",
            None => "IPM.Note",
        },
    );
    if !email.reply_to.is_empty() {
        builder.leaf(mail::REPLY_TO, email.reply_to);
    }
    // Metadata only (no content -- see the project's logging constraint on
    // mail bodies): lets a body-not-rendering report be diagnosed without
    // needing to inspect an account's real mail.
    tracing::debug!(
        server_id = email_id,
        has_body = email.body.is_some(),
        body_type = email.body.as_ref().map(|b| b.body_type.eas_value()),
        body_len = email.body.as_ref().map(|b| b.value.len()),
        "email body summary"
    );
    // NativeBodyType is captured here but emitted AFTER Attachments below,
    // matching z-push's real appended order (Body, Attachments,
    // NativeBodyType) -- this gateway had it right after Body instead.
    let native_body_type = email.body.as_ref().map(|body| body.body_type);
    let preference = body_pref.unwrap_or_default();
    let selected = select_body_preference(
        email.body.as_ref(),
        preference.mime_data.as_ref(),
        &preference.requests,
    );
    if selected.is_some_and(|pref| pref.body_type == 4) {
        if let Some(mime) = preference.mime_data {
            let pref = selected.unwrap();
            let truncated = mime.data.len() < mime.full_size;
            builder.start(base::BODY);
            builder.leaf(base::TYPE, "4");
            builder.leaf(base::ESTIMATED_DATA_SIZE, mime.full_size.to_string());
            builder.leaf(base::TRUNCATED, if truncated { "1" } else { "0" });
            if !truncated || !pref.all_or_none {
                write_mime_data(builder, mime.data);
            }
            let preview_limit = pref.preview.unwrap_or(255);
            if preview_limit > 0 {
                let preview: String = email
                    .preview
                    .unwrap_or_else(|| {
                        email
                            .body
                            .as_ref()
                            .map(plain_text_preview)
                            .unwrap_or_default()
                    })
                    .chars()
                    .take(preview_limit)
                    .collect();
                builder.leaf(base::PREVIEW, preview);
            }
            builder.end();
        }
    } else if preference.requests.is_empty() || selected.is_some() {
        let prepared = preference.text_body.or_else(|| {
            email
                .body
                .map(|body| prepare_text_body(body, selected, email.preview.as_deref()))
        });
        if let Some(body) = prepared {
            builder.start(base::BODY);
            builder.leaf(base::TYPE, body.body_type.eas_value());
            builder.leaf(base::ESTIMATED_DATA_SIZE, body.full_size.to_string());
            builder.leaf(base::TRUNCATED, if body.truncated { "1" } else { "0" });
            if let Some(data) = body.data {
                builder.leaf(base::DATA, data);
            }
            if let Some(preview) = body.preview {
                builder.leaf(base::PREVIEW, preview);
            }
            builder.end();
        }
    }
    if !email.attachments.is_empty() {
        builder.start(base::ATTACHMENTS);
        for attachment in email.attachments {
            // Real bug, confirmed live via idevicesyslog on the zoidberg
            // A/B test, in two rounds. Round 1: iOS's WBXML parser is a
            // strict per-position grammar, not a flat tag lookup --
            // sending ContentType right after FileReference (skipping
            // Method entirely) desynced its parser state: `No parse rule
            // from object <private> for codePage 0x11 token 0x17 (CPT =
            // 69911)` -- 0x11=17 is AirSyncBase, 0x17 is ContentType --
            // and the WHOLE Sync task failed (`ASFolderItemsSyncTask ...
            // failed with status: 1`), discarding every field in the
            // response, not just the broken attachment. Fixing the field
            // ORDER (adding Method, moving ContentType last) did NOT
            // clear the error, though -- round 2, verified against the
            // working PHP z-push reference's own `SyncBaseAttachment`
            // construction (config/z-push/jmap.php): it sets
            // displayname/filereference/method/estimatedDataSize/
            // isinline (and optionally contentid) for a Sync response's
            // attachment list, but NEVER contenttype -- that only
            // appears on a completely different class
            // (SyncItemOperationsAttachment) used for the separate
            // GetAttachment-download response. ContentType simply isn't
            // valid in this position at all, regardless of order --
            // removed entirely, matching the reference exactly.
            builder.start(base::ATTACHMENT);
            builder.leaf(base::DISPLAY_NAME, attachment.name.clone());
            // GetAttachment addresses attachments by an opaque server-
            // chosen reference string (MS-ASCMD `AttachmentName` query
            // param) -- encoded here as "blobId||name" (mirrors the PHP
            // reference's own `explode('||', ...)` scheme) so the
            // GetAttachment handler can recover both the JMAP blob to
            // fetch and a filename to send back, without needing any
            // server-side state to remember what a reference pointed to.
            builder.leaf(
                base::FILE_REFERENCE,
                format!("{}||{}", attachment.blob_id, attachment.name),
            );
            // AttMethod 1 = "Normal" attachment (a plain file, not an
            // embedded message/OLE object) -- the only kind this gateway
            // ever produces.
            builder.leaf(
                base::METHOD,
                if attachment
                    .content_type
                    .eq_ignore_ascii_case("message/rfc822")
                {
                    "5"
                } else {
                    "1"
                },
            );
            builder.leaf(base::ESTIMATED_DATA_SIZE, attachment.size.to_string());
            if let Some(content_id) = &attachment.content_id {
                builder.leaf(base::CONTENT_ID, content_id);
            }
            if let Some(location) = &attachment.content_location {
                builder.leaf(base::CONTENT_LOCATION, location);
            }
            builder.leaf(base::IS_INLINE, if attachment.inline { "1" } else { "0" });
            builder.end();
        }
        builder.end();
    }
    if let Some(native_type) = native_body_type {
        builder.leaf(base::NATIVE_BODY_TYPE, native_type.eas_value());
    }
    // Email2:ConversationId, per z-push's own syncmail.php $mapping
    // (config/z-push, PR #187): its >=14.0 block appends UmCallerId,
    // UmUserNotes, ConversationId, ConversationIndex, ... strictly AFTER
    // the >=12.0 block (Body/Attachments/NativeBodyType, already this
    // function's ordering above) -- this gateway doesn't implement
    // UmCallerId/UmUserNotes, so ConversationId is the first 14.0-block
    // field actually sent, and belongs right here, after NativeBodyType.
    //
    // REDO of a previously-reverted attempt (commit d199d37): the
    // original sent the raw JMAP thread id string directly and caused a
    // real, device-visible sync error. Two likely bugs stacked: (1) it
    // used WBXML token 0x0a for CONVERSATION_ID, which is actually
    // CONVERSATION_INDEX (a different field) -- confirmed against the
    // primary MS-ASWBXML source this session, see email2::CONVERSATION_ID's
    // own doc comment; (2) real Exchange servers send a fixed 16-byte
    // GUID-shaped value here, not a short variable-length string (JMAP
    // thread ids observed as short as a single ASCII byte), and iOS may
    // validate the shape more strictly than the spec text alone implies.
    // This attempt fixes both: the correct token, and a deterministic
    // fixed-16-byte value (UUID v5, namespace + thread id) so the same
    // JMAP thread always produces the identical ConversationId (the
    // actual point of the field) while always being GUID-shaped.
    //
    // NOT YET CONFIRMED against a real device -- deployed to the
    // isolated zoidberg test instance only. Structural verification
    // (WBXML decode, doesn't corrupt anything downstream) is not the
    // same as device-level trust, per this exact feature's own history.
    if let Some(thread_id) = email.thread_id {
        if !thread_id.is_empty() {
            builder.opaque_leaf(
                email2::CONVERSATION_ID,
                eas_conversation_id(&thread_id).to_vec(),
            );
        }
    }
    write_email_flag(builder, &message_state);
    if matches!(version, ProtocolVersion::V16_0 | ProtocolVersion::V16_1) {
        if !email.bcc.is_empty() {
            builder.leaf(email2::BCC, &email.bcc);
        }
        builder.leaf(
            email2::IS_DRAFT,
            if email.keywords.iter().any(|k| k == "$draft") {
                "1"
            } else {
                "0"
            },
        );
    }
}

/// See the call site's doc comment in `write_email_fields` for the full
/// story of why this exists and what it replaces.
fn eas_conversation_id(thread_id: &str) -> [u8; 16] {
    // Fixed, arbitrary application-specific namespace (generated once --
    // must stay constant so the same JMAP thread always hashes to the
    // same ConversationId across restarts/deploys). Not a real DNS/URL/
    // OID namespace, just a stable 16-byte seed for UUID v5's algorithm.
    const NAMESPACE: uuid::Uuid = uuid::Uuid::from_bytes([
        0x8f, 0x1c, 0x2e, 0x77, 0x4a, 0x9b, 0x4d, 0x21, 0x9e, 0x6a, 0x3b, 0x5d, 0x1f, 0x0c, 0x77,
        0xe2,
    ]);
    *uuid::Uuid::new_v5(&NAMESPACE, thread_id.as_bytes()).as_bytes()
}

fn converted_body(body: crate::model::EmailBody, target: u8) -> crate::model::EmailBody {
    use crate::model::{EmailBody, EmailBodyType};
    if target == 3 {
        let text = match body.body_type {
            EmailBodyType::Html => collapse_whitespace(&strip_html_tags(&body.value)),
            EmailBodyType::Plain => body.value.clone(),
            EmailBodyType::Rtf => rtf_to_plain_text(&body.value),
        };
        return EmailBody {
            is_truncated: body.is_truncated,
            body_type: EmailBodyType::Rtf,
            value: plain_text_to_rtf(&text),
        };
    }
    match (body.body_type, target) {
        (EmailBodyType::Html, 1) => EmailBody {
            is_truncated: body.is_truncated,
            body_type: EmailBodyType::Plain,
            value: collapse_whitespace(&strip_html_tags(&body.value)),
        },
        (EmailBodyType::Plain, 2) => EmailBody {
            is_truncated: body.is_truncated,
            body_type: EmailBodyType::Html,
            value: format!(
                "<html><body><pre>{}</pre></body></html>",
                body.value
                    .replace('&', "&amp;")
                    .replace('<', "&lt;")
                    .replace('>', "&gt;")
            ),
        },
        _ => body,
    }
}

fn plain_text_to_rtf(text: &str) -> String {
    let mut result =
        String::from("{\\rtf1\\ansi\\deff0{\\fonttbl{\\f0\\fswiss Arial;}}\\f0\\fs20 ");
    for ch in text.chars() {
        match ch {
            '\\' => result.push_str("\\\\"),
            '{' => result.push_str("\\{"),
            '}' => result.push_str("\\}"),
            '\n' => result.push_str("\\line "),
            '\r' => {}
            c if c.is_ascii() => result.push(c),
            c => {
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units).iter() {
                    result.push_str(&format!("\\u{}?", *unit as i16));
                }
            }
        }
    }
    result.push('}');
    result
}

fn rtf_to_plain_text(rtf: &str) -> String {
    // This only handles the RTF subset emitted above. Incoming JMAP bodies
    // are plain text or HTML, so this is a defensive conversion path.
    let mut out = String::new();
    let mut chars = rtf.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => match chars.next() {
                Some('\'') => {
                    let hex: String = chars.by_ref().take(2).collect();
                    if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                        out.push(byte as char);
                    }
                }
                Some('\n') | Some('\r') => {}
                Some(control) if control.is_ascii_alphabetic() => {
                    let mut word = String::from(control);
                    while chars.peek().is_some_and(|c| c.is_ascii_alphabetic()) {
                        word.push(chars.next().unwrap());
                    }
                    let mut number = String::new();
                    if chars
                        .peek()
                        .is_some_and(|c| *c == '-' || c.is_ascii_digit())
                    {
                        number.push(chars.next().unwrap());
                        while chars.peek().is_some_and(|c| c.is_ascii_digit()) {
                            number.push(chars.next().unwrap());
                        }
                    }
                    if chars.peek() == Some(&' ') {
                        chars.next();
                    }
                    if word == "line" || word == "par" {
                        out.push('\n');
                    }
                    if word == "u" {
                        if let Ok(code) = number.parse::<i16>() {
                            out.push(char::from_u32(code as u16 as u32).unwrap_or('\u{fffd}'));
                        }
                        if chars.peek().is_some() {
                            chars.next();
                        }
                    }
                }
                Some(escaped @ ('\\' | '{' | '}')) => out.push(escaped),
                _ => {}
            },
            '{' | '}' => {}
            c => out.push(c),
        }
    }
    out
}

fn select_body_preference(
    body: Option<&crate::model::EmailBody>,
    mime: Option<&MimeBody>,
    requests: &[RequestedBodyPreference],
) -> Option<RequestedBodyPreference> {
    let size = |preference: &RequestedBodyPreference| match preference.body_type {
        1..=3 => body.map(|body| {
            converted_body(body.clone(), preference.body_type)
                .value
                .len()
        }),
        4 => mime.map(|mime| mime.full_size),
        _ => None,
    };
    let fits = |preference: &RequestedBodyPreference, size: usize| {
        !preference.all_or_none || preference.truncation_size.is_none_or(|limit| size <= limit)
    };
    let native = body.and_then(|body| {
        requests.iter().find(|preference| {
            preference.body_type
                == match body.body_type {
                    EmailBodyType::Plain => 1,
                    EmailBodyType::Html => 2,
                    EmailBodyType::Rtf => 3,
                }
        })
    });
    if let Some(preference) = native {
        if size(preference).is_some_and(|size| fits(preference, size)) {
            return Some(*preference);
        }
    }
    requests
        .iter()
        .filter_map(|preference| {
            size(preference)
                .filter(|size| fits(preference, *size))
                .map(|size| {
                    (
                        preference.truncation_size.unwrap_or(size).min(size),
                        *preference,
                    )
                })
        })
        .max_by_key(|(size, _)| *size)
        .map(|(_, preference)| preference)
        .or_else(|| native.copied())
        .or_else(|| {
            requests
                .iter()
                .find(|preference| size(preference).is_some())
                .copied()
        })
}

fn apply_body_preference(
    body: crate::model::EmailBody,
    preference: Option<RequestedBodyPreference>,
) -> (crate::model::EmailBodyType, String, usize, bool) {
    let body = match preference {
        Some(pref) => converted_body(body, pref.body_type),
        None => body,
    };
    let full_len = body.value.len();
    if let Some(limit) = preference
        .and_then(|pref| pref.truncation_size)
        .filter(|limit| *limit < full_len)
    {
        if preference.is_some_and(|pref| pref.all_or_none) {
            return (body.body_type, String::new(), full_len, true);
        }
        let mut end = limit;
        while end > 0 && !body.value.is_char_boundary(end) {
            end -= 1;
        }
        (body.body_type, body.value[..end].to_owned(), full_len, true)
    } else {
        (body.body_type, body.value, full_len, body.is_truncated)
    }
}

/// Plain-text snippet for AirSyncBase:Preview -- strips HTML tags (a real
/// parser isn't warranted here, this only has to look reasonable as a list
/// row, not round-trip) and collapses whitespace left behind by removed
/// tags/newlines, truncated to a conventional preview length.
fn plain_text_preview(body: &crate::model::EmailBody) -> String {
    const PREVIEW_CHARS: usize = 255;
    let text = match body.body_type {
        crate::model::EmailBodyType::Html => strip_html_tags(&body.value),
        crate::model::EmailBodyType::Plain => body.value.clone(),
        crate::model::EmailBodyType::Rtf => rtf_to_plain_text(&body.value),
    };
    collapse_whitespace(&text)
        .chars()
        .take(PREVIEW_CHARS)
        .collect()
}

/// Collapses any run of whitespace (including the blank lines an HTML
/// source's indentation/head section leaves behind once tags are
/// stripped) to a single space.
fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn strip_html_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    // <style>/<script> element CONTENT isn't tag markup, so the naive
    // char-by-char stripper below left it completely intact -- for an
    // HTML email whose <head> opens with a <style> block, that raw CSS
    // was the first "text" in the document and became the preview
    // (confirmed live: a message's list-row preview was literal
    // "@import url(...) :root { color-scheme: ... }"). Skip both
    // elements' content, not just their tags.
    // Char-based (not byte-sliced) lookahead/match so a multi-byte char
    // anywhere near a tag can never land a slice off a char boundary.
    fn starts_with_ci(chars: &[char], needle: &str) -> bool {
        chars.len() >= needle.chars().count()
            && chars
                .iter()
                .zip(needle.chars())
                .all(|(a, b)| a.eq_ignore_ascii_case(&b))
    }

    // Real bug: a '>' inside a quoted attribute value (common in inline
    // CSS/JS -- e.g. style="width:5px>2px?10px:0", or an onclick handler
    // doing a numeric comparison) prematurely ended in_tag with no quote
    // tracking, leaking the rest of that tag ('s remaining attributes,
    // the real closing '>') as literal output text -- visible as
    // fragments of raw markup in an otherwise-stripped preview/body.
    let mut in_attr_quote: Option<char> = None;

    let chars: Vec<char> = html.chars().collect();
    let mut skip_until: Option<&'static str> = None;
    let mut i = 0usize;
    while i < chars.len() {
        if let Some(closer) = skip_until {
            if starts_with_ci(&chars[i..], closer) {
                skip_until = None;
            }
            i += 1;
            continue;
        }
        let ch = chars[i];
        if in_tag {
            if let Some(q) = in_attr_quote {
                if ch == q {
                    in_attr_quote = None;
                }
            } else if ch == '"' || ch == '\'' {
                in_attr_quote = Some(ch);
            } else if ch == '>' {
                in_tag = false;
            }
            i += 1;
            continue;
        }
        if ch == '<' {
            if starts_with_ci(&chars[i + 1..], "style") {
                skip_until = Some("</style>");
            } else if starts_with_ci(&chars[i + 1..], "script") {
                skip_until = Some("</script>");
            }
            in_tag = true;
        } else if ch == '&' {
            // Real bug: entity references (`&nbsp;`, `&amp;`, ...) are
            // text content, not markup -- this loop only ever stripped
            // tags, so they passed straight through undecoded and showed
            // up as literal "&nbsp;" text in list-row previews (confirmed
            // live). Decode the common ones; anything unrecognized (or
            // any bare '&' in ordinary unencoded text, which is common)
            // is left exactly as-is rather than guessed at.
            if let Some((decoded, consumed)) = decode_entity_at(&chars[i..]) {
                out.push(decoded);
                i += consumed;
                continue;
            }
            out.push(ch);
        } else {
            out.push(ch);
        }
        i += 1;
    }
    out
}

/// `chars[0]` is the `&`. Returns the decoded character and how many
/// input chars it consumed (including the `&` and the terminating `;`),
/// or `None` if this isn't a recognized entity (leaves it untouched).
fn decode_entity_at(chars: &[char]) -> Option<(char, usize)> {
    // Real entities are short; a ';' further out than this is almost
    // certainly an unrelated bare '&' in ordinary text, not markup.
    let semi_index = chars.iter().take(32).position(|c| *c == ';')?;
    if semi_index == 0 {
        return None;
    }
    let entity: String = chars[1..semi_index].iter().collect();
    let decoded = decode_named_or_numeric_entity(&entity)?;
    Some((decoded, semi_index + 1))
}

fn decode_named_or_numeric_entity(entity: &str) -> Option<char> {
    if let Some(digits) = entity
        .strip_prefix("#x")
        .or_else(|| entity.strip_prefix("#X"))
    {
        return u32::from_str_radix(digits, 16)
            .ok()
            .and_then(char::from_u32);
    }
    if let Some(digits) = entity.strip_prefix('#') {
        return digits.parse::<u32>().ok().and_then(char::from_u32);
    }
    Some(match entity {
        // Collapses to a plain space rather than U+00A0 -- this feeds a
        // list-row preview / plain-text body, and a literal non-breaking
        // space has no Unicode White_Space property, so
        // collapse_whitespace()'s split_whitespace() wouldn't collapse
        // it, defeating the point of decoding it at all.
        "nbsp" => ' ',
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "mdash" => '\u{2014}',
        "ndash" => '\u{2013}',
        "hellip" => '\u{2026}',
        "copy" => '\u{00A9}',
        "reg" => '\u{00AE}',
        "trade" => '\u{2122}',
        "rsquo" => '\u{2019}',
        "lsquo" => '\u{2018}',
        "rdquo" => '\u{201D}',
        "ldquo" => '\u{201C}',
        "bull" => '\u{2022}',
        "middot" => '\u{00B7}',
        _ => return None,
    })
}

/// Select one EAS page and advance only the item hashes the client receives.
fn item_sync_page(
    previous: &mut BTreeMap<String, String>,
    current: &BTreeMap<String, String>,
    just_written: &BTreeSet<String>,
    get_changes: bool,
    window: usize,
) -> (Vec<String>, Vec<String>, Vec<String>, bool) {
    for id in just_written {
        if let Some(hash) = current.get(id) {
            previous.insert(id.clone(), hash.clone());
        }
    }
    let mut adds = Vec::new();
    let mut changes = Vec::new();
    let mut removes = Vec::new();
    let mut more = false;
    if get_changes {
        let pending: Vec<(String, u8)> = current
            .iter()
            .filter_map(|(id, hash)| match previous.get(id) {
                None => Some((id.clone(), 0)),
                Some(old) if old != hash => Some((id.clone(), 1)),
                _ => None,
            })
            .chain(
                previous
                    .keys()
                    .filter(|id| !current.contains_key(*id))
                    .map(|id| (id.clone(), 2)),
            )
            .collect();
        more = pending.len() > window;
        for (id, kind) in pending.into_iter().take(window) {
            if kind == 2 {
                previous.remove(&id);
                removes.push(id);
            } else {
                previous.insert(id.clone(), current[&id].clone());
                if kind == 0 {
                    adds.push(id);
                } else {
                    changes.push(id);
                }
            }
        }
    }
    (adds, changes, removes, more)
}

async fn sync_contacts_collection(
    state: &AppState,
    auth: &AuthenticatedSession,
    builder: &mut wbxml::eas::DocumentBuilder,
    user: &str,
    device_id: &str,
    collection: &wbxml::eas::SyncCollectionRequest,
) {
    use wbxml::eas::airsync as air;

    let address_book_id = collection
        .collection_id
        .strip_prefix("ab_")
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

    let mut deleted = false;
    let mut add_responses: Vec<(String, Option<String>, &'static str)> = Vec::new();
    let mut change_responses: Vec<(String, &'static str)> = Vec::new();
    let mut fetch_ids = Vec::new();

    for command in &collection.commands {
        match command.kind {
            wbxml::eas::SyncClientCommandKind::Add => {
                match state
                    .jmap
                    .save_contact(auth, &address_book_id, None, &command.contact)
                    .await
                {
                    Ok(new_id) => {
                        add_responses.push((command.client_id.clone(), Some(new_id), "1"))
                    }
                    Err(error) => {
                        tracing::warn!(
                            %error,
                            client_id = command.client_id,
                            "failed to create contact from client Add"
                        );
                        add_responses.push((command.client_id.clone(), None, "5"));
                    }
                }
            }
            wbxml::eas::SyncClientCommandKind::Change => {
                match state
                    .jmap
                    .save_contact(
                        auth,
                        &address_book_id,
                        Some(&command.server_id),
                        &command.contact,
                    )
                    .await
                {
                    Ok(id) => change_responses.push((id, "1")),
                    Err(error) => {
                        tracing::warn!(
                            %error,
                            server_id = command.server_id,
                            "failed to save contact change"
                        );
                        change_responses.push((command.server_id.clone(), "5"));
                    }
                }
            }
            wbxml::eas::SyncClientCommandKind::Delete => {
                match state.jmap.destroy_contact(auth, &command.server_id).await {
                    Ok(()) => {
                        previous_by_id.remove(&command.server_id);
                        deleted = true;
                    }
                    Err(error) => {
                        tracing::warn!(
                            %error,
                            server_id = command.server_id,
                            "failed to delete contact"
                        );
                    }
                }
            }
            wbxml::eas::SyncClientCommandKind::Fetch => {
                fetch_ids.push(command.server_id.clone());
            }
        }
    }

    let contacts = match state
        .jmap
        .contacts_in_address_book(auth, &address_book_id, 0)
        .await
    {
        Ok(contacts) => contacts,
        Err(error) => {
            tracing::warn!(
                %error,
                collection = collection.collection_id,
                "Sync contacts discovery failed"
            );
            builder.start(air::COLLECTION);
            builder.leaf(air::SYNC_KEY, collection.sync_key.clone());
            builder.leaf(air::COLLECTION_ID, collection.collection_id.clone());
            builder.leaf(air::STATUS, "5");
            builder.end();
            return;
        }
    };

    let current_by_id: BTreeMap<String, (String, crate::model::Contact)> = contacts
        .into_iter()
        .map(|contact| (contact.id.clone(), (contact_hash(&contact), contact)))
        .collect();

    // Same self-echo guard as sync_notes_collection: an id the client
    // itself just added/changed in THIS request is already confirmed via
    // the Responses entries above, so it must not also show up in
    // Commands as if the server discovered it independently.
    let just_written: BTreeSet<String> = add_responses
        .iter()
        .filter_map(|(_, server_id, status)| (*status == "1").then(|| server_id.clone()).flatten())
        .chain(
            change_responses
                .iter()
                .filter(|(_, status)| *status == "1")
                .map(|(server_id, _)| server_id.clone()),
        )
        .collect();

    let current_hashes: BTreeMap<String, String> = current_by_id
        .iter()
        .map(|(id, (hash, _))| (id.clone(), hash.clone()))
        .collect();
    let (to_add, to_change, to_remove, more_available) = item_sync_page(
        &mut previous_by_id,
        &current_hashes,
        &just_written,
        collection.get_changes,
        collection.window_size,
    );

    let has_server_changes = !to_add.is_empty() || !to_change.is_empty() || !to_remove.is_empty();
    let client_commands_applied =
        deleted || !add_responses.is_empty() || !change_responses.is_empty();
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
        tracing::warn!(%error, user, device_id, collection = collection.collection_id, "failed to persist Contacts SyncRecord");
    }
    let item_states: Vec<ItemState> = previous_by_id
        .into_iter()
        .map(|(item_id, hash)| ItemState { item_id, hash })
        .collect();
    if let Err(error) = state
        .state
        .put_item_states(user, device_id, &collection.collection_id, item_states)
        .await
    {
        tracing::warn!(%error, user, device_id, collection = collection.collection_id, "failed to persist Contacts item state");
    }

    builder.start(air::COLLECTION);
    builder.leaf(air::SYNC_KEY, new_sync_key);
    builder.leaf(air::COLLECTION_ID, collection.collection_id.clone());
    builder.leaf(air::STATUS, "1");

    if !add_responses.is_empty() || !change_responses.is_empty() || !fetch_ids.is_empty() {
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
        for server_id in fetch_ids {
            builder.start(air::FETCH);
            builder.leaf(air::SERVER_ID, server_id.clone());
            if let Some((_, contact)) = current_by_id.get(&server_id) {
                builder.leaf(air::STATUS, "1");
                builder.start(air::APPLICATION_DATA);
                write_contact_fields(builder, contact, false);
                builder.end();
            } else {
                builder.leaf(air::STATUS, "8");
            }
            builder.end();
        }
        builder.end();
    }

    if more_available {
        builder.empty_tag(air::MORE_AVAILABLE);
    }
    if collection.get_changes && has_server_changes {
        builder.start(air::COMMANDS);
        for id in &to_add {
            if let Some((_, contact)) = current_by_id.get(id) {
                write_contact_command(builder, air::ADD, contact);
            }
        }
        for id in &to_change {
            if let Some((_, contact)) = current_by_id.get(id) {
                write_contact_command(builder, air::CHANGE, contact);
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

/// Hashes exactly the fields `write_contact_command` sends -- same
/// purpose as `jmap::notes::note_hash`, to detect a server-side edit
/// between syncs without needing JMAP's own per-object state token
/// (`Contact` doesn't carry one; recomputing from content is simpler
/// than threading a JMAP state string through the whole diff).
fn contact_hash(contact: &crate::model::Contact) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    contact.first_name.hash(&mut hasher);
    contact.middle_name.hash(&mut hasher);
    contact.last_name.hash(&mut hasher);
    contact.suffix.hash(&mut hasher);
    contact.name_prefix.hash(&mut hasher);
    contact.yomi_first_name.hash(&mut hasher);
    contact.yomi_last_name.hash(&mut hasher);
    contact.yomi_company_name.hash(&mut hasher);
    contact.office_location.hash(&mut hasher);
    contact.nickname.hash(&mut hasher);
    contact.company_main_phone.hash(&mut hasher);
    contact.mms.hash(&mut hasher);
    contact.account_name.hash(&mut hasher);
    contact.picture.hash(&mut hasher);
    contact.im_address1.hash(&mut hasher);
    contact.im_address2.hash(&mut hasher);
    contact.im_address3.hash(&mut hasher);
    contact.birthday.hash(&mut hasher);
    contact.anniversary.hash(&mut hasher);
    contact.file_as.hash(&mut hasher);
    contact.emails.hash(&mut hasher);
    contact.mobile_phone.hash(&mut hasher);
    contact.home_phone.hash(&mut hasher);
    contact.home_phone2.hash(&mut hasher);
    contact.business_phone.hash(&mut hasher);
    contact.business_phone2.hash(&mut hasher);
    contact.home_fax.hash(&mut hasher);
    contact.business_fax.hash(&mut hasher);
    contact.pager.hash(&mut hasher);
    contact.car_phone.hash(&mut hasher);
    contact.radio_phone.hash(&mut hasher);
    contact.web_page.hash(&mut hasher);
    contact.assistant_phone.hash(&mut hasher);
    contact.company_name.hash(&mut hasher);
    contact.department.hash(&mut hasher);
    contact.job_title.hash(&mut hasher);
    contact.business_address.hash(&mut hasher);
    contact.home_address.hash(&mut hasher);
    contact.other_address.hash(&mut hasher);
    contact.categories.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn write_contact_command(
    builder: &mut wbxml::eas::DocumentBuilder,
    tag: wbxml::token::Token,
    contact: &crate::model::Contact,
) {
    use wbxml::eas::airsync as air;
    builder.start(tag);
    builder.leaf(air::SERVER_ID, contact.id.clone());
    builder.start(air::APPLICATION_DATA);
    write_contact_fields(
        builder,
        contact,
        tag.code_page == air::PAGE && tag.token == air::CHANGE.token,
    );
    builder.end();
    builder.end();
}

fn write_contact_fields(
    builder: &mut wbxml::eas::DocumentBuilder,
    contact: &crate::model::Contact,
    is_change: bool,
) {
    use base64::Engine;
    use wbxml::eas::contacts as c;
    use wbxml::eas::contacts2 as c2;

    // Field order follows the real z-push-stalwart-jmap source (PR #187,
    // pinned in config/z-push/Dockerfile: synccontact.php's $mapping),
    // extending that sequence with Contacts-codepage address and Categories
    // fields. The prior FirstName/LastName-first sequence was reversed from
    // the reference order, the same failure class confirmed for Email,
    // Attachment, Preview, and Notes.
    write_contact_address_fields(
        builder,
        contact.business_address.as_ref(),
        "business",
        is_change,
    );
    if let Some(business) = &contact.business_phone {
        builder.leaf(c::BUSINESS_PHONE_NUMBER, business.clone());
    }
    if let Some(business2) = &contact.business_phone2 {
        builder.leaf(c::BUSINESS_2_PHONE_NUMBER, business2.clone());
    }
    if !contact.categories.is_empty() {
        builder.start(c::CATEGORIES);
        for category in &contact.categories {
            builder.leaf(c::CATEGORY, category.clone());
        }
        builder.end();
    } else if is_change {
        builder.empty_tag(c::CATEGORIES);
    }
    if let Some(company) = &contact.company_name {
        builder.leaf(c::COMPANY_NAME, company.clone());
    }
    if let Some(department) = &contact.department {
        builder.leaf(c::DEPARTMENT, department.clone());
    }
    let mut emails = contact.emails.iter();
    if let Some(email1) = emails.next() {
        builder.leaf(c::EMAIL1_ADDRESS, email1.clone());
    }
    if let Some(email2) = emails.next() {
        builder.leaf(c::EMAIL2_ADDRESS, email2.clone());
    }
    if let Some(email3) = emails.next() {
        builder.leaf(c::EMAIL3_ADDRESS, email3.clone());
    }
    if let Some(file_as) = &contact.file_as {
        builder.leaf(c::FILE_AS, file_as.clone());
    }
    if let Some(first) = &contact.first_name {
        builder.leaf(c::FIRST_NAME, first.clone());
    }
    if let Some(middle) = &contact.middle_name {
        builder.leaf(c::MIDDLE_NAME, middle.clone());
    }
    write_contact_address_fields(builder, contact.home_address.as_ref(), "home", is_change);
    if let Some(home) = &contact.home_phone {
        builder.leaf(c::HOME_PHONE_NUMBER, home.clone());
    }
    if let Some(home2) = &contact.home_phone2 {
        builder.leaf(c::HOME_2_PHONE_NUMBER, home2.clone());
    }
    if let Some(home_fax) = &contact.home_fax {
        builder.leaf(c::HOME_FAX_NUMBER, home_fax.clone());
    }
    if let Some(business_fax) = &contact.business_fax {
        builder.leaf(c::BUSINESS_FAX_NUMBER, business_fax.clone());
    }
    if let Some(car) = &contact.car_phone {
        builder.leaf(c::CAR_PHONE_NUMBER, car.clone());
    }
    if let Some(title) = &contact.job_title {
        builder.leaf(c::JOB_TITLE, title.clone());
    }
    if let Some(last) = &contact.last_name {
        builder.leaf(c::LAST_NAME, last.clone());
    }
    if let Some(suffix) = &contact.suffix {
        builder.leaf(c::SUFFIX, suffix.clone());
    }
    if let Some(prefix) = &contact.name_prefix {
        builder.leaf(c::TITLE, prefix.clone());
    }
    if let Some(yomi_first_name) = &contact.yomi_first_name {
        builder.leaf(c::YOMI_FIRST_NAME, yomi_first_name.clone());
    }
    if let Some(yomi_last_name) = &contact.yomi_last_name {
        builder.leaf(c::YOMI_LAST_NAME, yomi_last_name.clone());
    }
    if let Some(yomi_company_name) = &contact.yomi_company_name {
        builder.leaf(c::YOMI_COMPANY_NAME, yomi_company_name.clone());
    }
    if let Some(office_location) = &contact.office_location {
        builder.leaf(c::OFFICE_LOCATION, office_location.clone());
    }
    if let Some(nickname) = &contact.nickname {
        builder.leaf(c2::NICK_NAME, nickname.clone());
    }
    if let Some(phone) = &contact.company_main_phone {
        builder.leaf(c2::COMPANY_MAIN_PHONE, phone.clone());
    }
    if let Some(mms) = &contact.mms {
        builder.leaf(c2::MMS, mms.clone());
    }
    if let Some(account) = &contact.account_name {
        builder.leaf(c2::ACCOUNT_NAME, account.clone());
    }
    if let Some(picture) = &contact.picture {
        builder.leaf(
            c::PICTURE,
            base64::engine::general_purpose::STANDARD.encode(picture),
        );
    }
    if let Some(im) = &contact.im_address1 {
        builder.leaf(c2::IM_ADDRESS, im.clone());
    }
    if let Some(im) = &contact.im_address2 {
        builder.leaf(c2::IM_ADDRESS_2, im.clone());
    }
    if let Some(im) = &contact.im_address3 {
        builder.leaf(c2::IM_ADDRESS_3, im.clone());
    }
    if let Some(birthday) = &contact.birthday {
        builder.leaf(c::BIRTHDAY, birthday.clone());
    }
    if let Some(anniversary) = &contact.anniversary {
        builder.leaf(c::ANNIVERSARY, anniversary.clone());
    }
    if let Some(birthday) = &contact.birthday {
        builder.leaf(c::BIRTHDAY, birthday.clone());
    }
    if let Some(mobile) = &contact.mobile_phone {
        builder.leaf(c::MOBILE_PHONE_NUMBER, mobile.clone());
    }
    if let Some(pager) = &contact.pager {
        builder.leaf(c::PAGER_NUMBER, pager.clone());
    }
    if let Some(radio) = &contact.radio_phone {
        builder.leaf(c::RADIO_PHONE_NUMBER, radio.clone());
    }
    if let Some(web_page) = &contact.web_page {
        builder.leaf(c::WEB_PAGE, web_page.clone());
    }
    if let Some(assistant_phone) = &contact.assistant_phone {
        builder.leaf(c::ASSISTANT_PHONE_NUMBER, assistant_phone.clone());
    }
    write_contact_address_fields(builder, contact.other_address.as_ref(), "other", is_change);
}

fn write_contact_address_fields(
    builder: &mut wbxml::eas::DocumentBuilder,
    address: Option<&crate::model::ContactAddress>,
    kind: &str,
    is_change: bool,
) {
    use wbxml::eas::contacts as c;
    let tokens = match kind {
        "business" => [
            c::BUSINESS_ADDRESS_CITY,
            c::BUSINESS_ADDRESS_COUNTRY,
            c::BUSINESS_ADDRESS_POSTAL_CODE,
            c::BUSINESS_ADDRESS_STATE,
            c::BUSINESS_ADDRESS_STREET,
        ],
        "home" => [
            c::HOME_ADDRESS_CITY,
            c::HOME_ADDRESS_COUNTRY,
            c::HOME_ADDRESS_POSTAL_CODE,
            c::HOME_ADDRESS_STATE,
            c::HOME_ADDRESS_STREET,
        ],
        _ => [
            c::OTHER_ADDRESS_CITY,
            c::OTHER_ADDRESS_COUNTRY,
            c::OTHER_ADDRESS_POSTAL_CODE,
            c::OTHER_ADDRESS_STATE,
            c::OTHER_ADDRESS_STREET,
        ],
    };
    let values = address
        .map(|address| {
            [
                address.city.as_ref(),
                address.country.as_ref(),
                address.postal_code.as_ref(),
                address.state.as_ref(),
                address.street.as_ref(),
            ]
        })
        .unwrap_or([None, None, None, None, None]);
    for (token, value) in tokens.into_iter().zip(values) {
        match value {
            Some(value) if value.is_empty() => {
                builder.empty_tag(token);
            }
            Some(value) => {
                builder.leaf(token, value.clone());
            }
            None if is_change => builder.empty_tag(token),
            None => {}
        }
    }
}

fn calendar_instance_change_supported(fields: &wbxml::eas::CalendarFields) -> bool {
    (fields.subject.is_some()
        || fields.body.is_some()
        || fields.location.is_some()
        || fields.location_coordinates.is_some()
        || fields.start_time.is_some()
        || fields.end_time.is_some()
        || fields.all_day_event.is_some()
        || fields.busy_status.is_some()
        || fields.categories.is_some()
        || fields.reminder.is_some()
        || fields.attendees_present)
        && !fields.unsupported_location
        && !fields.unsupported_busy_status
        && !fields.unsupported_sensitivity
        && fields.sensitivity.is_none()
        && !fields.unsupported_reminder
        && !fields.unsupported_categories
        && !fields.recurrence_present
        && fields.recurrence.is_none()
        && !fields.unsupported_recurrence
        && fields.client_uid.is_none()
        && !fields.unsupported_client_uid
        && !fields.uid_present
        && !fields.unsupported_response_type
        && !fields.unsupported_attendees
        && !fields.unsupported_body
        && fields.body_type.is_none_or(|body_type| body_type == 1)
}

fn calendar_series_change_clears_exceptions(
    version: ProtocolVersion,
    fields: &wbxml::eas::CalendarFields,
) -> bool {
    matches!(version, ProtocolVersion::V16_0 | ProtocolVersion::V16_1)
        && (fields.recurrence_present || fields.start_time.is_some() || fields.end_time.is_some())
}

/// Handles one Calendar collection's Sync round-trip end to end, same
/// contract and hash-diff shape as `sync_contacts_collection` (see that
/// function's own doc for the full pattern description -- this is a
/// direct copy of it onto Calendar). Recurrence, attendee, and reminder
/// fields are handled by this collection's supported paths.
/// `CalendarEvent/set` supports in-place `update` (confirmed live -- see
/// `save_calendar_event`'s doc), so the JMAP id is a stable ServerId.
async fn sync_calendar_collection(
    state: &AppState,
    auth: &AuthenticatedSession,
    builder: &mut wbxml::eas::DocumentBuilder,
    user: &str,
    device_id: &str,
    collection: &wbxml::eas::SyncCollectionRequest,
    version: ProtocolVersion,
) {
    use wbxml::eas::airsync as air;

    let calendar_id = collection
        .collection_id
        .strip_prefix("cal_")
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

    let mut deleted = false;
    let mut add_responses: Vec<(String, Option<String>, &'static str)> = Vec::new();
    let mut change_responses: Vec<(String, &'static str)> = Vec::new();
    let mut fetch_ids = Vec::new();
    let journal = format!("__calendar_add__:{}", collection.collection_id);
    let mut completed = Vec::new();
    let mut client_ids = BTreeMap::new();
    for command in &collection.commands {
        if matches!(command.kind, wbxml::eas::SyncClientCommandKind::Add) {
            *client_ids.entry(&command.client_id).or_insert(0usize) += 1;
        }
    }

    for command in &collection.commands {
        if command.instance_id.is_some()
            && (!matches!(version, ProtocolVersion::V16_0 | ProtocolVersion::V16_1)
                || !(command.kind == wbxml::eas::SyncClientCommandKind::Delete
                    || (command.kind == wbxml::eas::SyncClientCommandKind::Change
                        && calendar_instance_change_supported(&command.calendar))))
        {
            if command.kind == wbxml::eas::SyncClientCommandKind::Change {
                change_responses.push((command.server_id.clone(), "6"));
            }
            continue;
        }
        if command.calendar.unsupported_reminder
            || (command.calendar.reminder == Some(None)
                && !matches!(version, ProtocolVersion::V16_0 | ProtocolVersion::V16_1))
            || command.calendar.unsupported_busy_status
            || command.calendar.unsupported_location
            || command.calendar.unsupported_body
            || command.calendar.unsupported_attendees
            || command.calendar.unsupported_categories
            || command.calendar.unsupported_recurrence
            || (command.calendar.recurrence_present
                && command.calendar.recurrence.is_none()
                && !matches!(version, ProtocolVersion::V16_0 | ProtocolVersion::V16_1))
            || (command
                .calendar
                .recurrence
                .as_ref()
                .and_then(|recurrence| recurrence.first_day_of_week)
                .is_some()
                && !matches!(
                    version,
                    ProtocolVersion::V14_1 | ProtocolVersion::V16_0 | ProtocolVersion::V16_1
                ))
            || command.calendar.unsupported_response_type
            || command.calendar.unsupported_sensitivity
            || command.calendar.unsupported_client_uid
            || (command.calendar.client_uid.is_some()
                && !matches!(version, ProtocolVersion::V16_0 | ProtocolVersion::V16_1))
            || (command.calendar.client_uid.is_some()
                && command.kind != wbxml::eas::SyncClientCommandKind::Add)
            || (command.calendar.uid_present
                && matches!(version, ProtocolVersion::V16_0 | ProtocolVersion::V16_1))
        {
            match command.kind {
                wbxml::eas::SyncClientCommandKind::Add => {
                    add_responses.push((command.client_id.clone(), None, "6"));
                    continue;
                }
                wbxml::eas::SyncClientCommandKind::Change => {
                    change_responses.push((command.server_id.clone(), "6"));
                    continue;
                }
                _ => {}
            }
        }
        match command.kind {
            wbxml::eas::SyncClientCommandKind::Add => {
                let reservation = if command.calendar.reminder.is_some()
                    || command.calendar.categories.is_some()
                    || command.calendar.sensitivity.is_some()
                    || command.calendar.recurrence_present
                    || command.calendar.client_uid.is_some()
                {
                    if command.client_id.is_empty()
                        || client_ids.get(&command.client_id) != Some(&1)
                    {
                        add_responses.push((command.client_id.clone(), None, "6"));
                        continue;
                    }
                    let identity = calendar_add_replay_identity(
                        device_id,
                        &collection.collection_id,
                        &command.client_id,
                        command.calendar.client_uid.as_deref(),
                    );
                    let item_id =
                        uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, &identity).to_string();
                    let content = uuid::Uuid::new_v5(
                        &uuid::Uuid::NAMESPACE_OID,
                        &wbxml::encode_document(&command.document),
                    )
                    .to_string();
                    let uid = format!(
                        "eas-{}@stalwart-sync-gateway",
                        uuid::Uuid::new_v4().simple()
                    );
                    let item = ItemState {
                        item_id,
                        hash: serde_json::to_string(&(&uid, &content))
                            .expect("serializing calendar reservation"),
                    };
                    let reserved = state
                        .state
                        .reserve_item(
                            user,
                            device_id,
                            &collection.collection_id,
                            &collection.sync_key,
                            &journal,
                            item,
                        )
                        .await;
                    match reserved {
                        Ok(Some(item)) => {
                            match serde_json::from_str::<(String, String)>(&item.hash) {
                                Ok((uid, saved)) if saved == content => Some((uid, item)),
                                _ => {
                                    add_responses.push((command.client_id.clone(), None, "6"));
                                    continue;
                                }
                            }
                        }
                        _ => {
                            add_responses.push((command.client_id.clone(), None, "5"));
                            continue;
                        }
                    }
                } else {
                    None
                };
                let uid = reservation.as_ref().map(|(uid, _)| uid);
                let result = if let Some(uid) = uid {
                    match state
                        .jmap
                        .calendar_event_by_uid(auth, &calendar_id, uid)
                        .await
                    {
                        Ok(Some(id)) => {
                            let mut verified = Ok(());
                            if let Some(reminder) = command.calendar.reminder {
                                verified = state
                                    .jmap
                                    .verify_calendar_reminder(auth, &id, reminder)
                                    .await;
                            }
                            if verified.is_ok() {
                                if let Some(sensitivity) = command.calendar.sensitivity {
                                    verified = state
                                        .jmap
                                        .verify_calendar_privacy(auth, &id, sensitivity)
                                        .await;
                                }
                            }
                            if verified.is_ok() {
                                if let Some(categories) = &command.calendar.categories {
                                    verified = state
                                        .jmap
                                        .verify_calendar_categories(auth, &id, categories)
                                        .await;
                                }
                            }
                            if verified.is_ok() && command.calendar.recurrence_present {
                                verified = state
                                    .jmap
                                    .verify_calendar_recurrence(
                                        auth,
                                        &id,
                                        command.calendar.recurrence.as_ref(),
                                    )
                                    .await;
                            }
                            verified.map(|_| id)
                        }
                        Ok(None) => {
                            state
                                .jmap
                                .save_calendar_event(
                                    auth,
                                    &calendar_id,
                                    None,
                                    Some(uid),
                                    &command.calendar,
                                    false,
                                )
                                .await
                        }
                        Err(error) => Err(error),
                    }
                } else {
                    state
                        .jmap
                        .save_calendar_event(
                            auth,
                            &calendar_id,
                            None,
                            None,
                            &command.calendar,
                            false,
                        )
                        .await
                };
                match result {
                    Ok(new_id) => {
                        if let Some((_, item)) = reservation {
                            completed.push(item);
                        }
                        add_responses.push((command.client_id.clone(), Some(new_id), "1"))
                    }
                    Err(error) => {
                        tracing::warn!(
                            %error,
                            client_id = command.client_id,
                            "failed to create calendar event from client Add"
                        );
                        add_responses.push((command.client_id.clone(), None, "5"));
                    }
                }
            }
            wbxml::eas::SyncClientCommandKind::Change => {
                let result = if let Some(instance_id) = command.instance_id.as_deref() {
                    state
                        .jmap
                        .save_calendar_event_instance(
                            auth,
                            &command.server_id,
                            instance_id,
                            &command.calendar,
                        )
                        .await
                        .map(|()| command.server_id.clone())
                } else {
                    state
                        .jmap
                        .save_calendar_event(
                            auth,
                            &calendar_id,
                            Some(&command.server_id),
                            None,
                            &command.calendar,
                            calendar_series_change_clears_exceptions(version, &command.calendar),
                        )
                        .await
                };
                match result {
                    Ok(id) => change_responses.push((id, "1")),
                    Err(error) => {
                        tracing::warn!(
                            %error,
                            server_id = command.server_id,
                            "failed to save calendar event change"
                        );
                        change_responses.push((command.server_id.clone(), "5"));
                    }
                }
            }
            wbxml::eas::SyncClientCommandKind::Delete => {
                let result = if let Some(instance_id) = command.instance_id.as_deref() {
                    state
                        .jmap
                        .exclude_calendar_event_instance(auth, &command.server_id, instance_id)
                        .await
                } else {
                    state
                        .jmap
                        .destroy_calendar_event(auth, &command.server_id)
                        .await
                };
                match result {
                    Ok(()) => {
                        if command.instance_id.is_none() {
                            previous_by_id.remove(&command.server_id);
                        }
                        deleted = true;
                    }
                    Err(error) => {
                        tracing::warn!(
                            %error,
                            server_id = command.server_id,
                            instance_id = command.instance_id,
                            "failed to delete calendar event or recurrence occurrence"
                        );
                    }
                }
            }
            wbxml::eas::SyncClientCommandKind::Fetch => {
                fetch_ids.push(command.server_id.clone());
            }
        }
    }

    let events = match state
        .jmap
        .calendar_events_in_calendar(auth, &calendar_id, 0)
        .await
    {
        Ok(events) => events,
        Err(error) => {
            tracing::warn!(
                %error,
                collection = collection.collection_id,
                "Sync calendar discovery failed"
            );
            builder.start(air::COLLECTION);
            builder.leaf(air::SYNC_KEY, collection.sync_key.clone());
            builder.leaf(air::COLLECTION_ID, collection.collection_id.clone());
            builder.leaf(air::STATUS, "5");
            builder.end();
            return;
        }
    };

    if !matches!(
        version,
        ProtocolVersion::V14_1 | ProtocolVersion::V16_0 | ProtocolVersion::V16_1
    ) && events.iter().any(|event| {
        event
            .recurrence
            .as_ref()
            .and_then(|recurrence| recurrence.first_day_of_week)
            .is_some_and(|first_day| first_day != 1)
    }) {
        builder.start(air::COLLECTION);
        builder.leaf(air::SYNC_KEY, collection.sync_key.clone());
        builder.leaf(air::COLLECTION_ID, collection.collection_id.clone());
        builder.leaf(air::STATUS, "5");
        builder.end();
        return;
    }

    let current_by_id: BTreeMap<String, (String, crate::model::CalendarEvent)> = events
        .into_iter()
        .map(|event| (event.id.clone(), (calendar_event_hash(&event), event)))
        .collect();

    let just_written: BTreeSet<String> = add_responses
        .iter()
        .filter_map(|(_, server_id, status)| (*status == "1").then(|| server_id.clone()).flatten())
        .chain(
            change_responses
                .iter()
                .filter(|(_, status)| *status == "1")
                .map(|(server_id, _)| server_id.clone()),
        )
        .collect();

    let current_hashes: BTreeMap<String, String> = current_by_id
        .iter()
        .map(|(id, (hash, _))| (id.clone(), hash.clone()))
        .collect();
    let (to_add, to_change, to_remove, more_available) = item_sync_page(
        &mut previous_by_id,
        &current_hashes,
        &just_written,
        collection.get_changes,
        collection.window_size,
    );

    let has_server_changes = !to_add.is_empty() || !to_change.is_empty() || !to_remove.is_empty();
    let client_commands_applied =
        deleted || !add_responses.is_empty() || !change_responses.is_empty();
    let new_sync_key =
        if collection.sync_key == "0" || has_server_changes || client_commands_applied {
            next_sync_key(&collection.sync_key)
        } else {
            collection.sync_key.clone()
        };

    let item_states: Vec<ItemState> = previous_by_id
        .into_iter()
        .map(|(item_id, hash)| ItemState { item_id, hash })
        .collect();
    if let Err(error) = state
        .state
        .put_with_items_and_clear(
            SyncRecord {
                user: user.to_owned(),
                device_id: device_id.to_owned(),
                collection_id: collection.collection_id.clone(),
                sync_key: new_sync_key.clone(),
                jmap_state: String::new(),
                seen_ids: Vec::new(),
            },
            Some(item_states),
            Some((journal, completed)),
        )
        .await
    {
        tracing::warn!(%error, user, device_id, collection = collection.collection_id, "failed to persist Calendar Sync state");
        builder.start(air::COLLECTION);
        builder.leaf(air::SYNC_KEY, collection.sync_key.clone());
        builder.leaf(air::COLLECTION_ID, collection.collection_id.clone());
        builder.leaf(air::STATUS, "5");
        builder.end();
        return;
    }

    builder.start(air::COLLECTION);
    builder.leaf(air::SYNC_KEY, new_sync_key);
    builder.leaf(air::COLLECTION_ID, collection.collection_id.clone());
    builder.leaf(air::STATUS, "1");

    if !add_responses.is_empty() || !change_responses.is_empty() || !fetch_ids.is_empty() {
        builder.start(air::RESPONSES);
        for (client_id, server_id, status) in &add_responses {
            builder.start(air::ADD);
            builder.leaf(air::CLIENT_ID, client_id.clone());
            if let Some(server_id) = server_id {
                builder.leaf(air::SERVER_ID, server_id.clone());
            }
            builder.leaf(air::STATUS, *status);
            if matches!(version, ProtocolVersion::V16_0 | ProtocolVersion::V16_1) {
                if let Some(uid) = server_id
                    .as_ref()
                    .and_then(|id| current_by_id.get(id))
                    .and_then(|(_, event)| event.uid.as_ref())
                {
                    builder.start(air::APPLICATION_DATA);
                    builder.leaf(wbxml::eas::calendar::UID, uid.clone());
                    builder.end();
                }
            }
            builder.end();
        }
        for (server_id, status) in &change_responses {
            builder.start(air::CHANGE);
            builder.leaf(air::SERVER_ID, server_id.clone());
            builder.leaf(air::STATUS, *status);
            builder.end();
        }
        for server_id in fetch_ids {
            builder.start(air::FETCH);
            builder.leaf(air::SERVER_ID, server_id.clone());
            if let Some((_, event)) = current_by_id.get(&server_id) {
                builder.leaf(air::STATUS, "1");
                builder.start(air::APPLICATION_DATA);
                write_calendar_fields(builder, event, version, &collection.body_preferences, false);
                builder.end();
            } else {
                builder.leaf(air::STATUS, "8");
            }
            builder.end();
        }
        builder.end();
    }

    if more_available {
        builder.empty_tag(air::MORE_AVAILABLE);
    }
    if collection.get_changes && has_server_changes {
        builder.start(air::COMMANDS);
        for id in &to_add {
            if let Some((_, event)) = current_by_id.get(id) {
                write_calendar_command(
                    builder,
                    air::ADD,
                    event,
                    version,
                    &collection.body_preferences,
                );
            }
        }
        for id in &to_change {
            if let Some((_, event)) = current_by_id.get(id) {
                write_calendar_command(
                    builder,
                    air::CHANGE,
                    event,
                    version,
                    &collection.body_preferences,
                );
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

fn calendar_add_replay_identity(
    device_id: &str,
    collection_id: &str,
    client_id: &str,
    client_uid: Option<&str>,
) -> Vec<u8> {
    let key = client_uid
        .map(|uid| ("clientUid", uid))
        .unwrap_or(("clientId", client_id));
    serde_json::to_vec(&(device_id, collection_id, key)).expect("serializing calendar add identity")
}

/// Same purpose as `contact_hash`/`jmap::notes::note_hash`.
fn calendar_event_hash(event: &crate::model::CalendarEvent) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    event.uid.hash(&mut hasher);
    event.title.hash(&mut hasher);
    event.updated_utc.hash(&mut hasher);
    if let Some(body) = &event.description {
        body.body_type.eas_value().hash(&mut hasher);
        body.value.hash(&mut hasher);
    }
    event.location.hash(&mut hasher);
    event.location_coordinates.hash(&mut hasher);
    event.start_utc.hash(&mut hasher);
    event.end_utc.hash(&mut hasher);
    event.all_day.hash(&mut hasher);
    event.busy_status.hash(&mut hasher);
    event.sensitivity.hash(&mut hasher);
    event.reminder_minutes.hash(&mut hasher);
    event.categories.hash(&mut hasher);
    event.organizer_email.hash(&mut hasher);
    event.organizer_name.hash(&mut hasher);
    event.is_organizer.hash(&mut hasher);
    event.cancelled.hash(&mut hasher);
    event.response_type.hash(&mut hasher);
    event.attendees.hash(&mut hasher);
    for exception in &event.recurrence_exceptions {
        exception.instance_id.hash(&mut hasher);
        exception.deleted.hash(&mut hasher);
        exception.title.hash(&mut hasher);
        exception.location.hash(&mut hasher);
        exception.location_coordinates.hash(&mut hasher);
        exception.start_utc.hash(&mut hasher);
        exception.end_utc.hash(&mut hasher);
        exception.all_day.hash(&mut hasher);
        exception.busy_status.hash(&mut hasher);
        exception.categories.hash(&mut hasher);
        exception.attendees.hash(&mut hasher);
        exception.reminder_minutes.hash(&mut hasher);
        if let Some(description) = &exception.description {
            description.body_type.eas_value().hash(&mut hasher);
            description.value.hash(&mut hasher);
        }
    }
    format!("{:016x}", hasher.finish())
}

fn write_calendar_command(
    builder: &mut wbxml::eas::DocumentBuilder,
    tag: wbxml::token::Token,
    event: &crate::model::CalendarEvent,
    version: ProtocolVersion,
    requests: &[RequestedBodyPreference],
) {
    use wbxml::eas::airsync as air;

    builder.start(tag);
    builder.leaf(air::SERVER_ID, event.id.clone());
    builder.start(air::APPLICATION_DATA);
    write_calendar_fields(
        builder,
        event,
        version,
        requests,
        tag.code_page == air::PAGE && tag.token == air::CHANGE.token,
    );
    builder.end();
    builder.end();
}

fn write_calendar_fields(
    builder: &mut wbxml::eas::DocumentBuilder,
    event: &crate::model::CalendarEvent,
    version: ProtocolVersion,
    requests: &[RequestedBodyPreference],
    is_change: bool,
) {
    use wbxml::eas::calendar as cal;

    // Field order matches the real z-push-stalwart-jmap source (PR #187,
    // pinned in config/z-push/Dockerfile: src/lib/syncobjects/
    // syncappointment.php's own $mapping, filtered to the fields this
    // gateway actually sets): DtStamp, StartTime, Subject, OrganizerName,
    // OrganizerEmail, Location, EndTime, AllDayEvent, MeetingStatus,
    // Attendees (confirmed against a real worked [MS-ASCAL] Sync example,
    // not just the PHP source, for the Organizer/MeetingStatus/Attendees
    // placement specifically -- the PR #187 mapping doesn't cover those
    // three since z-push-stalwart-jmap never implemented them).
    if let Some(updated) = &event.updated_utc {
        builder.leaf(cal::DTSTAMP, updated);
    }
    if let Some(start) = &event.start_utc {
        builder.leaf(cal::START_TIME, start.clone());
    }
    builder.leaf(cal::SUBJECT, event.title.clone());
    if let Some(uid) = &event.uid {
        builder.leaf(cal::UID, uid.clone());
    }
    let empty_body = crate::model::EmailBody {
        body_type: EmailBodyType::Plain,
        value: String::new(),
        is_truncated: false,
    };
    let description = event
        .description
        .as_ref()
        .or_else(|| (is_change).then_some(&empty_body));
    if let Some(body) = description {
        use wbxml::eas::airsync_base as base;
        let preference = select_body_preference(Some(body), None, requests);
        if requests.is_empty() || preference.is_some() {
            let (kind, data, size, truncated) = apply_body_preference(body.clone(), preference);
            builder.start(base::BODY);
            builder.leaf(base::TYPE, kind.eas_value());
            builder.leaf(base::ESTIMATED_DATA_SIZE, size.to_string());
            builder.leaf(base::TRUNCATED, if truncated { "1" } else { "0" });
            if !truncated || !preference.is_some_and(|pref| pref.all_or_none) {
                builder.leaf(base::DATA, data);
            }
            builder.end();
        }
    }
    if let Some(name) = &event.organizer_name {
        builder.leaf(cal::ORGANIZER_NAME, name.clone());
    }
    if let Some(email) = &event.organizer_email {
        builder.leaf(cal::ORGANIZER_EMAIL, email.clone());
    }
    if matches!(version, ProtocolVersion::V16_0 | ProtocolVersion::V16_1) {
        use wbxml::eas::airsync_base as base;
        if event.location.is_some() || event.location_coordinates.is_some() {
            builder.start(base::LOCATION);
            if let Some(location) = &event.location {
                builder.leaf(base::DISPLAY_NAME, location.clone());
            }
            if let Some((latitude, longitude)) = &event.location_coordinates {
                builder.leaf(base::LATITUDE, latitude);
                builder.leaf(base::LONGITUDE, longitude);
            }
            builder.end();
        } else {
            builder.empty_tag(base::LOCATION);
        }
    } else if let Some(location) = &event.location {
        builder.leaf(cal::LOCATION, location.clone());
    }
    if let Some(end) = &event.end_utc {
        builder.leaf(cal::END_TIME, end.clone());
    }
    builder.leaf(cal::ALL_DAY_EVENT, if event.all_day { "1" } else { "0" });
    let busy = event.busy_status.unwrap_or(2);
    let busy = if busy == 4 && !matches!(version, ProtocolVersion::V16_0 | ProtocolVersion::V16_1) {
        2
    } else {
        busy
    };
    builder.leaf(cal::BUSY_STATUS, busy.to_string());
    builder.leaf(cal::SENSITIVITY, event.sensitivity.to_string());
    if !event.categories.is_empty() {
        builder.start(cal::CATEGORIES);
        for category in &event.categories {
            builder.leaf(cal::CATEGORY, category);
        }
        builder.end();
    } else if is_change {
        builder.empty_tag(cal::CATEGORIES);
    }
    if let Some(recurrence) = &event.recurrence {
        builder.start(cal::RECURRENCE);
        builder.leaf(cal::RECURRENCE_TYPE, recurrence.kind.to_string());
        if let Some(until) = &recurrence.until {
            builder.leaf(cal::RECURRENCE_UNTIL, until.clone());
        }
        if let Some(count) = recurrence.occurrences {
            builder.leaf(cal::RECURRENCE_OCCURRENCES, count.to_string());
        }
        if let Some(interval) = recurrence.interval {
            builder.leaf(cal::RECURRENCE_INTERVAL, interval.to_string());
        }
        if let Some(days) = recurrence.day_of_week {
            builder.leaf(cal::RECURRENCE_DAY_OF_WEEK, days.to_string());
        }
        if let Some(day) = recurrence.day_of_month {
            builder.leaf(cal::RECURRENCE_DAY_OF_MONTH, day.to_string());
        }
        if let Some(week) = recurrence.week_of_month {
            builder.leaf(cal::RECURRENCE_WEEK_OF_MONTH, week.to_string());
        }
        if let Some(month) = recurrence.month_of_year {
            builder.leaf(cal::RECURRENCE_MONTH_OF_YEAR, month.to_string());
        }
        if let Some(first_day) = recurrence.first_day_of_week.filter(|_| {
            matches!(
                version,
                ProtocolVersion::V14_1 | ProtocolVersion::V16_0 | ProtocolVersion::V16_1
            )
        }) {
            builder.leaf(cal::FIRST_DAY_OF_WEEK, first_day.to_string());
        }
        builder.end();
    } else if is_change && matches!(version, ProtocolVersion::V16_0 | ProtocolVersion::V16_1) {
        builder.empty_tag(cal::RECURRENCE);
    }
    if !event.recurrence_exceptions.is_empty() {
        builder.start(cal::EXCEPTIONS);
        for exception in &event.recurrence_exceptions {
            builder.start(cal::EXCEPTION);
            if let Some(uid) = &event.uid {
                builder.leaf(cal::UID, uid.clone());
            }
            if matches!(version, ProtocolVersion::V16_0 | ProtocolVersion::V16_1) {
                use wbxml::eas::airsync_base as base;
                let instance_id =
                    chrono::NaiveDateTime::parse_from_str(&exception.instance_id, "%Y%m%dT%H%M%SZ")
                        .map(|date| date.format("%Y-%m-%dT%H:%M:%S.000Z").to_string())
                        .unwrap_or_else(|_| exception.instance_id.clone());
                builder.leaf(base::INSTANCE_ID, instance_id);
            } else {
                builder.leaf(cal::EXCEPTION_START_TIME, exception.instance_id.clone());
            }
            if exception.deleted {
                builder.leaf(cal::DELETED, "1");
            } else {
                if let Some(start) = &exception.start_utc {
                    builder.leaf(cal::START_TIME, start.clone());
                }
                if let Some(end) = &exception.end_utc {
                    builder.leaf(cal::END_TIME, end.clone());
                }
                if let Some(all_day) = exception.all_day {
                    builder.leaf(cal::ALL_DAY_EVENT, if all_day { "1" } else { "0" });
                }
                if let Some(busy_status) = exception.busy_status {
                    builder.leaf(cal::BUSY_STATUS, busy_status.to_string());
                }
                if let Some(categories) = &exception.categories {
                    builder.start(cal::CATEGORIES);
                    for category in categories {
                        builder.leaf(cal::CATEGORY, category.clone());
                    }
                    builder.end();
                }
                if let Some(attendees) = &exception.attendees {
                    builder.start(cal::ATTENDEES);
                    for attendee in attendees {
                        builder.start(cal::ATTENDEE);
                        builder.leaf(cal::ATTENDEE_EMAIL, attendee.email.clone());
                        if let Some(name) = &attendee.name {
                            builder.leaf(cal::ATTENDEE_NAME, name.clone());
                        }
                        builder.leaf(
                            cal::ATTENDEE_STATUS,
                            match attendee.participation_status.as_deref() {
                                Some("accepted") => "3",
                                Some("declined") => "4",
                                Some("tentative") => "2",
                                _ => "5",
                            },
                        );
                        builder.leaf(
                            cal::ATTENDEE_TYPE,
                            if attendee.optional { "2" } else { "1" },
                        );
                        builder.end();
                    }
                    builder.end();
                }
                if let Some(reminder) = exception.reminder_minutes {
                    if let Some(minutes) = reminder {
                        builder.leaf(cal::REMINDER, minutes.to_string());
                    } else if matches!(version, ProtocolVersion::V16_0 | ProtocolVersion::V16_1) {
                        builder.empty_tag(cal::REMINDER);
                    }
                }
                if let Some(title) = &exception.title {
                    builder.leaf(cal::SUBJECT, title.clone());
                }
                if let Some(body) = &exception.description {
                    use wbxml::eas::airsync_base as base;
                    let preference = select_body_preference(Some(body), None, requests);
                    if requests.is_empty() || preference.is_some() {
                        let (kind, data, size, truncated) =
                            apply_body_preference(body.clone(), preference);
                        builder.start(base::BODY);
                        builder.leaf(base::TYPE, kind.eas_value());
                        builder.leaf(base::ESTIMATED_DATA_SIZE, size.to_string());
                        builder.leaf(base::TRUNCATED, if truncated { "1" } else { "0" });
                        if !truncated || !preference.is_some_and(|pref| pref.all_or_none) {
                            builder.leaf(base::DATA, data);
                        }
                        builder.end();
                    }
                }
                if matches!(version, ProtocolVersion::V16_0 | ProtocolVersion::V16_1) {
                    use wbxml::eas::airsync_base as base;
                    if exception.location.is_some() || exception.location_coordinates.is_some() {
                        builder.start(base::LOCATION);
                        if let Some(location) = &exception.location {
                            builder.leaf(base::DISPLAY_NAME, location.clone());
                        }
                        if let Some((latitude, longitude)) = &exception.location_coordinates {
                            builder.leaf(base::LATITUDE, latitude);
                            builder.leaf(base::LONGITUDE, longitude);
                        }
                        builder.end();
                    }
                } else if let Some(location) = &exception.location {
                    builder.leaf(cal::LOCATION, location.clone());
                }
            }
            builder.end();
        }
        builder.end();
    }
    if let Some(minutes) = event.reminder_minutes {
        builder.leaf(cal::REMINDER, minutes.to_string());
    } else if is_change && matches!(version, ProtocolVersion::V16_0 | ProtocolVersion::V16_1) {
        builder.empty_tag(cal::REMINDER);
    }
    if !event.attendees.is_empty() {
        // [MS-ASCAL] MeetingStatus: 1 = meeting, organizer's own copy;
        // 3 = meeting, received as an attendee. The organizer is NEVER
        // listed as an Attendee (confirmed against the live spec's own
        // worked example) -- OrganizerName/OrganizerEmail above already
        // covers that, so `attendees` here only ever holds everyone else.
        if event.organizer_email.is_some() {
            builder.leaf(
                cal::MEETING_STATUS,
                match (event.is_organizer, event.cancelled) {
                    (true, false) => "1",
                    (false, false) => "3",
                    (true, true) => "5",
                    (false, true) => "7",
                },
            );
        }
        builder.start(cal::ATTENDEES);
        for attendee in &event.attendees {
            builder.start(cal::ATTENDEE);
            builder.leaf(cal::ATTENDEE_EMAIL, attendee.email.clone());
            if let Some(name) = &attendee.name {
                builder.leaf(cal::ATTENDEE_NAME, name.clone());
            }
            // AttendeeStatus values confirmed against the live [MS-ASCAL]
            // spec page (2.2.2.5), not guessed: 0 = response unknown,
            // 2 = tentative, 3 = accepted, 4 = declined, 5 = not
            // responded. JSCalendar's `participationStatus` has no
            // "response unknown" distinct from "needs-action" (not yet
            // responded), so that maps to 5, not 0 -- 0 is reserved for a
            // state JSCalendar doesn't represent here at all.
            let status = match attendee.participation_status.as_deref() {
                Some("accepted") => "3",
                Some("declined") => "4",
                Some("tentative") => "2",
                _ => "5",
            };
            builder.leaf(cal::ATTENDEE_STATUS, status);
            builder.leaf(
                cal::ATTENDEE_TYPE,
                if attendee.optional { "2" } else { "1" },
            );
            builder.end();
        }
        builder.end();
    } else if is_change {
        builder.leaf(cal::MEETING_STATUS, "0");
        builder.empty_tag(cal::ATTENDEES);
    }
    if let Some(response) = event.response_type {
        builder.leaf(cal::RESPONSE_TYPE, response.to_string());
    }
}

/// Same Add/Change/Delete + hash-diff shape as `sync_calendar_collection`
/// (see that function's own comments for the general pattern) -- Tasks
/// rides on the SAME underlying `CalendarEvent` storage as Calendar,
/// distinguished only by JMAP `@type` (confirmed live 2026-08-25: Stalwart
/// accepts and round-trips `@type: "Task"` objects; see
/// `jmap::client::tasks_in_calendar`'s module doc). `save_task` mirrors
/// `save_calendar_event`'s confirmed in-place-`update` behavior, so the
/// JMAP id is a stable ServerId across edits here too, same as
/// Contacts/Calendar and unlike Notes.
async fn sync_tasks_collection(
    state: &AppState,
    auth: &AuthenticatedSession,
    builder: &mut wbxml::eas::DocumentBuilder,
    user: &str,
    device_id: &str,
    collection: &wbxml::eas::SyncCollectionRequest,
    version: ProtocolVersion,
) {
    use wbxml::eas::airsync as air;

    if collection.commands.iter().any(|command| {
        command.task.unsupported_recurrence
            || (command.task.recurrence_present
                && command.task.recurrence.is_none()
                && !matches!(version, ProtocolVersion::V16_0 | ProtocolVersion::V16_1))
            || (command
                .task
                .recurrence
                .as_ref()
                .and_then(|recurrence| recurrence.first_day_of_week)
                .is_some()
                && !matches!(
                    version,
                    ProtocolVersion::V14_1 | ProtocolVersion::V16_0 | ProtocolVersion::V16_1
                ))
    }) {
        builder.start(air::COLLECTION);
        builder.leaf(air::SYNC_KEY, collection.sync_key.clone());
        builder.leaf(air::COLLECTION_ID, collection.collection_id.clone());
        builder.leaf(air::STATUS, "5");
        builder.end();
        return;
    }

    let calendar_id = collection
        .collection_id
        .strip_prefix("task_")
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

    let mut deleted = false;
    let mut add_responses: Vec<(String, Option<String>, &'static str)> = Vec::new();
    let mut change_responses: Vec<(String, &'static str)> = Vec::new();
    let mut fetch_ids = Vec::new();

    for command in &collection.commands {
        match command.kind {
            wbxml::eas::SyncClientCommandKind::Add => {
                match state
                    .jmap
                    .save_task(auth, &calendar_id, None, &command.task)
                    .await
                {
                    Ok(new_id) => {
                        add_responses.push((command.client_id.clone(), Some(new_id), "1"))
                    }
                    Err(error) => {
                        tracing::warn!(
                            %error,
                            client_id = command.client_id,
                            "failed to create task from client Add"
                        );
                        add_responses.push((command.client_id.clone(), None, "5"));
                    }
                }
            }
            wbxml::eas::SyncClientCommandKind::Change => {
                match state
                    .jmap
                    .save_task(auth, &calendar_id, Some(&command.server_id), &command.task)
                    .await
                {
                    Ok(id) => change_responses.push((id, "1")),
                    Err(error) => {
                        tracing::warn!(
                            %error,
                            server_id = command.server_id,
                            "failed to save task change"
                        );
                        change_responses.push((command.server_id.clone(), "5"));
                    }
                }
            }
            wbxml::eas::SyncClientCommandKind::Delete => {
                match state
                    .jmap
                    .destroy_calendar_event(auth, &command.server_id)
                    .await
                {
                    Ok(()) => {
                        previous_by_id.remove(&command.server_id);
                        deleted = true;
                    }
                    Err(error) => {
                        tracing::warn!(
                            %error,
                            server_id = command.server_id,
                            "failed to delete task"
                        );
                    }
                }
            }
            wbxml::eas::SyncClientCommandKind::Fetch => {
                fetch_ids.push(command.server_id.clone());
            }
        }
    }

    let tasks = match state.jmap.tasks_in_calendar(auth, &calendar_id, 0).await {
        Ok(tasks) => tasks,
        Err(error) => {
            tracing::warn!(
                %error,
                collection = collection.collection_id,
                "Sync tasks discovery failed"
            );
            builder.start(air::COLLECTION);
            builder.leaf(air::SYNC_KEY, collection.sync_key.clone());
            builder.leaf(air::COLLECTION_ID, collection.collection_id.clone());
            builder.leaf(air::STATUS, "5");
            builder.end();
            return;
        }
    };

    if !matches!(
        version,
        ProtocolVersion::V14_1 | ProtocolVersion::V16_0 | ProtocolVersion::V16_1
    ) && tasks.iter().any(|task| {
        task.recurrence
            .as_ref()
            .and_then(|recurrence| recurrence.first_day_of_week)
            .is_some_and(|first_day| first_day != 1)
    }) {
        builder.start(air::COLLECTION);
        builder.leaf(air::SYNC_KEY, collection.sync_key.clone());
        builder.leaf(air::COLLECTION_ID, collection.collection_id.clone());
        builder.leaf(air::STATUS, "5");
        builder.end();
        return;
    }

    let current_by_id: BTreeMap<String, (String, crate::model::Task)> = tasks
        .into_iter()
        .map(|task| (task.id.clone(), (task_hash(&task), task)))
        .collect();

    let just_written: BTreeSet<String> = add_responses
        .iter()
        .filter_map(|(_, server_id, status)| (*status == "1").then(|| server_id.clone()).flatten())
        .chain(
            change_responses
                .iter()
                .filter(|(_, status)| *status == "1")
                .map(|(server_id, _)| server_id.clone()),
        )
        .collect();

    let current_hashes: BTreeMap<String, String> = current_by_id
        .iter()
        .map(|(id, (hash, _))| (id.clone(), hash.clone()))
        .collect();
    let (to_add, to_change, to_remove, more_available) = item_sync_page(
        &mut previous_by_id,
        &current_hashes,
        &just_written,
        collection.get_changes,
        collection.window_size,
    );

    let has_server_changes = !to_add.is_empty() || !to_change.is_empty() || !to_remove.is_empty();
    let client_commands_applied =
        deleted || !add_responses.is_empty() || !change_responses.is_empty();
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
        tracing::warn!(%error, user, device_id, collection = collection.collection_id, "failed to persist Tasks SyncRecord");
    }
    let item_states: Vec<ItemState> = previous_by_id
        .into_iter()
        .map(|(item_id, hash)| ItemState { item_id, hash })
        .collect();
    if let Err(error) = state
        .state
        .put_item_states(user, device_id, &collection.collection_id, item_states)
        .await
    {
        tracing::warn!(%error, user, device_id, collection = collection.collection_id, "failed to persist Tasks item state");
    }

    builder.start(air::COLLECTION);
    builder.leaf(air::SYNC_KEY, new_sync_key);
    builder.leaf(air::COLLECTION_ID, collection.collection_id.clone());
    builder.leaf(air::STATUS, "1");

    if !add_responses.is_empty() || !change_responses.is_empty() || !fetch_ids.is_empty() {
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
        for server_id in fetch_ids {
            builder.start(air::FETCH);
            builder.leaf(air::SERVER_ID, server_id.clone());
            if let Some((_, task)) = current_by_id.get(&server_id) {
                builder.leaf(air::STATUS, "1");
                builder.start(air::APPLICATION_DATA);
                write_task_fields(builder, task, &collection.body_preferences, version, false);
                builder.end();
            } else {
                builder.leaf(air::STATUS, "8");
            }
            builder.end();
        }
        builder.end();
    }

    if more_available {
        builder.empty_tag(air::MORE_AVAILABLE);
    }
    if collection.get_changes && has_server_changes {
        builder.start(air::COMMANDS);
        for id in &to_add {
            if let Some((_, task)) = current_by_id.get(id) {
                write_task_command(
                    builder,
                    air::ADD,
                    task,
                    &collection.body_preferences,
                    version,
                );
            }
        }
        for id in &to_change {
            if let Some((_, task)) = current_by_id.get(id) {
                write_task_command(
                    builder,
                    air::CHANGE,
                    task,
                    &collection.body_preferences,
                    version,
                );
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

/// Same purpose as `calendar_event_hash`.
fn task_hash(task: &crate::model::Task) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    task.title.hash(&mut hasher);
    if let Some(body) = &task.description {
        body.value.hash(&mut hasher);
        body.body_type.eas_value().hash(&mut hasher);
    }
    task.completed.hash(&mut hasher);
    task.date_completed.hash(&mut hasher);
    task.importance.hash(&mut hasher);
    task.sensitivity.hash(&mut hasher);
    task.categories.hash(&mut hasher);
    task.reminder_time.hash(&mut hasher);
    task.start.hash(&mut hasher);
    task.start_local.hash(&mut hasher);
    task.due.hash(&mut hasher);
    task.due_local.hash(&mut hasher);
    task.recurrence.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Field order (Subject, Complete, DueDate, UtcDueDate) is a reasonable
/// choice, NOT a verified one -- unlike Calendar/Contacts/Email, there is
/// no real reference implementation to check it against: this codebase's
/// own z-push-derived porting notes (`docs/PR187_ANALYSIS.md`) explicitly
/// say Tasks was never attempted there, and MS-ASCMD's own `ItemProperties`
/// group (which `tasks:*` elements belong to) is an `xs:choice`, not an
/// `xs:sequence` -- checked directly against the primary schema on
/// learn.microsoft.com, so the wire spec itself doesn't mandate an order
/// the way it would inside a `sequence`. Needs a real device Tasks sync to
/// confirm iOS accepts this shape, same as the ConversationId redo -- do
/// not treat as trusted until that happens.
fn write_task_command(
    builder: &mut wbxml::eas::DocumentBuilder,
    tag: wbxml::token::Token,
    task: &crate::model::Task,
    requests: &[RequestedBodyPreference],
    version: ProtocolVersion,
) {
    use wbxml::eas::airsync as air;

    builder.start(tag);
    builder.leaf(air::SERVER_ID, task.id.clone());
    builder.start(air::APPLICATION_DATA);
    write_task_fields(
        builder,
        task,
        requests,
        version,
        tag.code_page == air::PAGE && tag.token == air::CHANGE.token,
    );
    builder.end();
    builder.end();
}

fn write_task_fields(
    builder: &mut wbxml::eas::DocumentBuilder,
    task: &crate::model::Task,
    requests: &[RequestedBodyPreference],
    version: ProtocolVersion,
    is_change: bool,
) {
    use wbxml::eas::{airsync_base as base, tasks};
    builder.leaf(tasks::SUBJECT, task.title.clone());
    let empty_body = crate::model::EmailBody {
        body_type: EmailBodyType::Plain,
        value: String::new(),
        is_truncated: false,
    };
    if let Some(body) = task
        .description
        .as_ref()
        .or_else(|| is_change.then_some(&empty_body))
    {
        let preference = select_body_preference(Some(body), None, requests);
        if requests.is_empty() || preference.is_some() {
            let (kind, data, size, truncated) = apply_body_preference(body.clone(), preference);
            builder.start(base::BODY);
            builder.leaf(base::TYPE, kind.eas_value());
            builder.leaf(base::ESTIMATED_DATA_SIZE, size.to_string());
            builder.leaf(base::TRUNCATED, if truncated { "1" } else { "0" });
            if !truncated || !preference.is_some_and(|pref| pref.all_or_none) {
                builder.leaf(base::DATA, data);
            }
            builder.end();
        }
    }
    builder.leaf(tasks::COMPLETE, if task.completed { "1" } else { "0" });
    builder.leaf(tasks::SENSITIVITY, task.sensitivity.to_string());
    builder.leaf(
        tasks::REMINDER_SET,
        if task.reminder_time.is_some() {
            "1"
        } else {
            "0"
        },
    );
    if let Some(reminder_time) = task.reminder_time.as_deref() {
        builder.leaf(tasks::REMINDER_TIME, reminder_time);
    }
    if let Some(importance) = task.importance {
        builder.leaf(
            tasks::IMPORTANCE,
            match importance {
                0 => "0",
                1 => "1",
                _ => "2",
            },
        );
    }
    if !task.categories.is_empty() {
        builder.start(tasks::CATEGORIES);
        for category in &task.categories {
            builder.leaf(tasks::CATEGORY, category.clone());
        }
        builder.end();
    }
    if task.completed {
        if let Some(date_completed) = task.date_completed.as_deref() {
            builder.leaf(tasks::DATE_COMPLETED, date_completed);
        }
    }
    let format_due = |due: &str| {
        chrono::NaiveDateTime::parse_from_str(due, "%Y%m%dT%H%M%SZ")
            .ok()
            .map(|date| date.format("%Y-%m-%dT%H:%M:%S.000Z").to_string())
    };
    if let (Some(local), Some(utc)) = (
        task.start_local.as_deref().and_then(format_due),
        task.start.as_deref().and_then(format_due),
    ) {
        builder.leaf(tasks::START_DATE, local);
        builder.leaf(tasks::UTC_START_DATE, utc);
    }
    if let (Some(local), Some(utc)) = (
        task.due_local.as_deref().and_then(format_due),
        task.due.as_deref().and_then(format_due),
    ) {
        builder.leaf(tasks::DUE_DATE, local);
        builder.leaf(tasks::UTC_DUE_DATE, utc);
    }
    if let Some(recurrence) = &task.recurrence {
        builder.start(tasks::RECURRENCE);
        if let Some(series_start) = task.start.as_deref().and_then(format_due) {
            builder.leaf(tasks::START, series_start);
        }
        builder.leaf(tasks::TYPE, recurrence.kind.to_string());
        if let Some(until) = &recurrence.until {
            builder.leaf(tasks::UNTIL, until.clone());
        }
        if let Some(count) = recurrence.occurrences {
            builder.leaf(tasks::OCCURRENCES, count.to_string());
        }
        if let Some(interval) = recurrence.interval {
            builder.leaf(tasks::INTERVAL, interval.to_string());
        }
        if let Some(days) = recurrence.day_of_week {
            builder.leaf(tasks::DAY_OF_WEEK, days.to_string());
        }
        if let Some(day) = recurrence.day_of_month {
            builder.leaf(tasks::DAY_OF_MONTH, day.to_string());
        }
        if let Some(week) = recurrence.week_of_month {
            builder.leaf(tasks::WEEK_OF_MONTH, week.to_string());
        }
        if let Some(month) = recurrence.month_of_year {
            builder.leaf(tasks::MONTH_OF_YEAR, month.to_string());
        }
        if let Some(first_day) = recurrence.first_day_of_week.filter(|_| {
            matches!(
                version,
                ProtocolVersion::V14_1 | ProtocolVersion::V16_0 | ProtocolVersion::V16_1
            )
        }) {
            builder.leaf(tasks::FIRST_DAY_OF_WEEK, first_day.to_string());
        }
        builder.end();
    } else if is_change && matches!(version, ProtocolVersion::V16_0 | ProtocolVersion::V16_1) {
        builder.empty_tag(tasks::RECURRENCE);
    }
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

    let mut deleted = false;
    let mut add_responses: Vec<(String, Option<String>, &'static str)> = Vec::new();
    let mut change_responses: Vec<(String, &'static str)> = Vec::new();
    let mut fetch_ids = Vec::new();

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
                        deleted = true;
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
                fetch_ids.push(command.server_id.clone());
            }
        }
    }

    let current_summaries = match state.jmap.list_notes(auth, &mailbox_id, 0).await {
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
        .filter_map(|(_, server_id, status)| (*status == "1").then(|| server_id.clone()).flatten())
        .chain(
            change_responses
                .iter()
                .filter(|(_, status)| *status == "1")
                .map(|(server_id, _)| server_id.clone()),
        )
        .collect();

    let current_hashes: BTreeMap<String, String> = current_by_id.clone();
    let (to_add, to_change, to_remove, more_available) = item_sync_page(
        &mut previous_by_id,
        &current_hashes,
        &just_written,
        collection.get_changes,
        collection.window_size,
    );

    let mut notes = BTreeMap::new();
    let note_ids: BTreeSet<String> = to_add
        .iter()
        .chain(&to_change)
        .chain(
            fetch_ids
                .iter()
                .filter(|id| current_by_id.contains_key(*id)),
        )
        .cloned()
        .collect();
    for id in &note_ids {
        match state.jmap.get_note(auth, id).await {
            Ok(Some(note)) => {
                notes.insert(id.clone(), note);
            }
            result => {
                tracing::warn!(error = ?result.err(), "Notes body unavailable during Sync");
                builder.start(air::COLLECTION);
                builder.leaf(air::SYNC_KEY, collection.sync_key.clone());
                builder.leaf(air::COLLECTION_ID, collection.collection_id.clone());
                builder.leaf(air::STATUS, "5");
                builder.end();
                return;
            }
        }
    }

    let has_server_changes = !to_add.is_empty() || !to_change.is_empty() || !to_remove.is_empty();
    let client_commands_applied =
        deleted || !add_responses.is_empty() || !change_responses.is_empty();
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
    let item_states: Vec<ItemState> = previous_by_id
        .into_iter()
        .map(|(item_id, hash)| ItemState { item_id, hash })
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

    if !add_responses.is_empty() || !change_responses.is_empty() || !fetch_ids.is_empty() {
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
        for server_id in fetch_ids {
            builder.start(air::FETCH);
            builder.leaf(air::SERVER_ID, server_id.clone());
            if let Some(note) = notes.get(&server_id) {
                builder.leaf(air::STATUS, "1");
                builder.start(air::APPLICATION_DATA);
                write_note_fields(builder, note, &collection.body_preferences);
                builder.end();
            } else {
                builder.leaf(air::STATUS, "8");
            }
            builder.end();
        }
        builder.end();
    }

    if more_available {
        builder.empty_tag(air::MORE_AVAILABLE);
    }
    if collection.get_changes && has_server_changes {
        builder.start(air::COMMANDS);
        for id in &to_add {
            if let Some(note) = notes.get(id) {
                write_note_command(builder, air::ADD, note);
            }
        }
        for id in &to_change {
            if let Some(note) = notes.get(id) {
                write_note_command(builder, air::CHANGE, note);
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
    use wbxml::eas::airsync as air;

    // Field order matches the real z-push-stalwart-jmap source (PR #187,
    // pinned in config/z-push/Dockerfile: src/lib/syncobjects/
    // syncnote.php's own $mapping order): Body, Categories,
    // LastModifiedDate, MessageClass, Subject -- this gateway had
    // Subject first and Body second (reversed), and MessageClass before
    // Categories/LastModifiedDate instead of after.
    builder.start(tag);
    builder.leaf(air::SERVER_ID, note.id.clone());
    builder.start(air::APPLICATION_DATA);
    write_note_fields(builder, note, &[]);
    builder.end();
    builder.end();
}

fn write_note_fields(
    builder: &mut wbxml::eas::DocumentBuilder,
    note: &Note,
    requests: &[RequestedBodyPreference],
) {
    use wbxml::eas::{airsync_base as base, notes};
    let body = crate::model::EmailBody {
        body_type: note.body_type,
        value: note.body.clone(),
        is_truncated: false,
    };
    let preference = select_body_preference(Some(&body), None, requests);
    if requests.is_empty() || preference.is_some() {
        let (kind, data, size, truncated) = apply_body_preference(body, preference);
        builder.start(base::BODY);
        builder.leaf(base::TYPE, kind.eas_value());
        builder.leaf(base::ESTIMATED_DATA_SIZE, size.to_string());
        builder.leaf(base::TRUNCATED, if truncated { "1" } else { "0" });
        if !truncated || !preference.is_some_and(|pref| pref.all_or_none) {
            builder.leaf(base::DATA, data);
        }
        builder.end();
    }
    if !note.categories.is_empty() {
        builder.start(notes::CATEGORIES);
        for category in &note.categories {
            builder.leaf(notes::CATEGORY, category.clone());
        }
        builder.end();
    }
    if let Some(modified) = &note.modified {
        builder.leaf(notes::LAST_MODIFIED_DATE, eas_datetime(modified));
    }
    builder.leaf(notes::MESSAGE_CLASS, "IPM.StickyNote");
    builder.leaf(notes::SUBJECT, note.title.clone());
}

/// `Notes:LastModifiedDate`, `Calendar:DtStamp/StartTime/EndTime` want the
/// compact EAS datetime format (`YYYYMMDDTHHMMSSZ`, MS-ASWBXML
/// `STREAMER_TYPE_DATE`), not JMAP's ISO 8601 (`YYYY-MM-DDTHH:MM:SSZ`) --
/// confirmed against a live device trace captured against this fork's PHP
/// predecessor, not assumed. `Email:DateReceived` is the one field that
/// does NOT use this format -- see `eas_datetime_dashes` below.
fn eas_datetime(jmap_datetime: &str) -> String {
    // JMAP UTCDate allows fractional seconds ("...T02:05:00.123Z"); EAS
    // DateTime has no room for them, so drop any ".NNN" before stripping
    // the separators, keeping the trailing Z.
    let trimmed = match jmap_datetime.split_once('.') {
        Some((prefix, suffix)) if suffix.ends_with(['Z', 'z']) => {
            format!("{prefix}Z")
        }
        Some((prefix, _)) => prefix.to_owned(),
        None => jmap_datetime.to_owned(),
    };
    trimmed
        .chars()
        .filter(|ch| *ch != '-' && *ch != ':')
        .collect()
}

/// Real bug, root-caused via a direct side-by-side wire comparison against
/// the working PHP z-push reference for the SAME real message (both
/// gateways queried live, same account, same email): z-push sent
/// `DateReceived` as `2026-08-24T19:37:00.000Z` -- full ISO shape, dashes
/// and colons intact, plus a literal ".000" -- while this gateway sent the
/// compact `20260824T193730Z` form. Confirmed against
/// src/lib/core/streamer.php's own formatDate() (z-push-stalwart-jmap PR
/// #187): `Email:DateReceived` is mapped with `STREAMER_TYPE_DATE_DASHES`,
/// which formats as `yyyy-MM-dd'T'HH:mm:SS'.000Z'` -- a DIFFERENT format
/// than every other EAS date field (which use plain `STREAMER_TYPE_DATE`,
/// the compact form `eas_datetime` above produces). This was backwards
/// from the start: an earlier session concluded DateReceived needed the
/// compact form and "fixed" it that way, which was itself the bug -- the
/// compact form is wrong specifically for this one field. Very likely
/// THE actual explanation for the "every message shows the same time in
/// the list view" symptom that kicked off this entire investigation: iOS
/// couldn't parse an unexpected date shape for DateReceived and fell back
/// to displaying something else (observed as "now", or a fixed collapsed
/// time) instead of erroring outright.
fn eas_datetime_dashes(jmap_datetime: &str) -> String {
    // Same fractional-seconds handling as eas_datetime, but keep dashes
    // and colons, and always append a literal ".000" before the Z --
    // z-push's own format string is a fixed ".000Z" suffix regardless of
    // the source's actual sub-second precision, not real milliseconds.
    let trimmed = match jmap_datetime.split_once('.') {
        Some((prefix, suffix)) if suffix.ends_with(['Z', 'z']) => {
            format!("{prefix}Z")
        }
        Some((prefix, _)) => prefix.to_owned(),
        None => jmap_datetime.to_owned(),
    };
    match trimmed.strip_suffix(['Z', 'z']) {
        Some(prefix) => format!("{prefix}.000Z"),
        None => format!("{trimmed}.000Z"),
    }
}

/// Per-device folder hierarchy state for FolderSync, kept as a SyncRecord
/// under this reserved collection id. `seen_ids` holds one fingerprint per
/// folder: id, parent, name and type joined by U+001F.
const FOLDER_STATE_ID: &str = "__folders__";

fn folder_fingerprint(c: &crate::model::Collection) -> String {
    format!(
        "{}\u{1f}{}\u{1f}{}\u{1f}{}",
        c.id,
        c.parent_id.clone().unwrap_or_else(|| "0".to_owned()),
        c.name,
        c.folder_type
    )
}

async fn folder_sync(
    state: &AppState,
    auth: &crate::jmap::client::AuthenticatedSession,
    query: &EasQuery,
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

    // [MS-ASCMD] FolderSync: SyncKey 0 returns the whole hierarchy; any
    // other key returns only what changed since that key (Count 0 when
    // nothing did); an unknown key is Status 9 and the client starts over.
    let user = query.user.as_deref().unwrap_or_else(|| auth.username());
    let device = query.device_id.as_deref().unwrap_or("unknown");
    let stored = state
        .state
        .get(user, device, FOLDER_STATE_ID)
        .await
        .ok()
        .flatten();
    let current: BTreeMap<String, (String, &crate::model::Collection)> = collections
        .iter()
        .map(|c| (c.id.clone(), (folder_fingerprint(c), c)))
        .collect();

    use wbxml::eas::folder_hierarchy as fh;
    let mut builder = wbxml::eas::DocumentBuilder::new();
    builder.start(fh::FOLDER_SYNC);

    let previous: Option<BTreeMap<String, String>> = if sync_key == "0" {
        Some(BTreeMap::new())
    } else {
        match &stored {
            Some(record) if record.sync_key == sync_key => Some(
                record
                    .seen_ids
                    .iter()
                    .map(|fp| {
                        (
                            fp.split('\u{1f}').next().unwrap_or("").to_owned(),
                            fp.clone(),
                        )
                    })
                    .collect(),
            ),
            // No stored hierarchy yet (device partnered before this was
            // tracked): accept its key as the baseline instead of forcing a
            // full reset of every folder on the device.
            None => Some(
                current
                    .iter()
                    .map(|(id, (fp, _))| (id.clone(), fp.clone()))
                    .collect(),
            ),
            Some(_) => None,
        }
    };

    let Some(previous) = previous else {
        builder.leaf(fh::STATUS, "9");
        builder.end();
        let body = wbxml::encode_document(&builder.finish());
        state
            .metrics
            .eas_requests_total
            .with_label_values(&[command, "200"])
            .inc();
        return (
            StatusCode::OK,
            [("Content-Type", "application/vnd.ms-sync.wbxml")],
            body,
        )
            .into_response();
    };

    let mut adds = Vec::new();
    let mut updates = Vec::new();
    for (id, (fp, c)) in &current {
        match previous.get(id) {
            None => adds.push(*c),
            Some(old) if old != fp => updates.push(*c),
            _ => {}
        }
    }
    let deletes: Vec<&String> = previous
        .keys()
        .filter(|id| !current.contains_key(*id))
        .collect();
    let changed = !(adds.is_empty() && updates.is_empty() && deletes.is_empty());
    let new_sync_key = if sync_key == "0" || changed {
        next_sync_key(sync_key)
    } else {
        sync_key.to_owned()
    };

    builder.leaf(fh::STATUS, "1");
    builder.leaf(fh::SYNC_KEY, new_sync_key.clone());
    builder.start(fh::CHANGES);
    builder.leaf(
        fh::COUNT,
        (adds.len() + updates.len() + deletes.len()).to_string(),
    );
    for c in &updates {
        builder.start(fh::UPDATE);
        builder.leaf(fh::SERVER_ID, c.id.clone());
        builder.leaf(
            fh::PARENT_ID,
            c.parent_id.clone().unwrap_or_else(|| "0".to_owned()),
        );
        builder.leaf(fh::DISPLAY_NAME, c.name.clone());
        builder.leaf(fh::TYPE, c.folder_type.to_string());
        builder.end();
    }
    for id in &deletes {
        builder.start(fh::DELETE);
        builder.leaf(fh::SERVER_ID, (*id).clone());
        builder.end();
    }
    for c in &adds {
        builder.start(fh::ADD);
        builder.leaf(fh::SERVER_ID, c.id.clone());
        builder.leaf(
            fh::PARENT_ID,
            c.parent_id.clone().unwrap_or_else(|| "0".to_owned()),
        );
        builder.leaf(fh::DISPLAY_NAME, c.name.clone());
        builder.leaf(fh::TYPE, c.folder_type.to_string());
        builder.end();
    }
    builder.end();
    builder.end();

    if let Err(error) = state
        .state
        .put(SyncRecord {
            user: user.to_owned(),
            device_id: device.to_owned(),
            collection_id: FOLDER_STATE_ID.to_owned(),
            sync_key: new_sync_key,
            jmap_state: String::new(),
            seen_ids: current.values().map(|(fp, _)| fp.clone()).collect(),
        })
        .await
    {
        tracing::warn!(%error, "failed to persist FolderSync state");
    }

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

/// Contact/calendar folders use synthetic ids around their JMAP ids;
/// Notes and Tasks are derived collections with no independent lifecycle.
/// Only the former can be passed through to AddressBook/set or Calendar/set.
fn is_non_mail_collection_id(id: &str) -> bool {
    id.starts_with("note_")
        || id.starts_with("ab_")
        || id.starts_with("cal_")
        || id.starts_with("task_")
}

fn groupware_collection_from_server_id(
    id: &str,
) -> Option<(crate::jmap::client::GroupwareCollectionKind, &str)> {
    if let Some(id) = id.strip_prefix("ab_") {
        Some((
            crate::jmap::client::GroupwareCollectionKind::AddressBook,
            id,
        ))
    } else {
        id.strip_prefix("cal_")
            .map(|id| (crate::jmap::client::GroupwareCollectionKind::Calendar, id))
    }
}

/// [MS-ASCMD] section 2.2.1.3. Request: SyncKey, ParentId, DisplayName,
/// Type (in that order, verified against section 6.9's XML schema).
/// Response: Status, SyncKey, ServerId -- SyncKey/ServerId are
/// `minOccurs="0"` in the schema; this always includes SyncKey (echoed
/// back unchanged -- this gateway doesn't maintain incremental
/// folder-hierarchy sync state for FolderSync's incremental set-diff
/// scheme, so there is nothing to advance here) and ServerId only on
/// success. Status codes verified against section 2.2.3.177.3.
async fn folder_create(
    state: &AppState,
    auth: &crate::jmap::client::AuthenticatedSession,
    document: &wbxml::Document,
    command: &str,
) -> Response {
    use wbxml::eas::folder_hierarchy as fh;

    let sync_key = wbxml::eas::find_text_after(document, fh::SYNC_KEY).unwrap_or("0");
    let Some(parent_id) = wbxml::eas::find_text_after(document, fh::PARENT_ID) else {
        return folder_create_status_response(state, command, "10", sync_key, None);
    };
    let Some(display_name) = wbxml::eas::find_text_after(document, fh::DISPLAY_NAME) else {
        return folder_create_status_response(state, command, "10", sync_key, None);
    };
    let folder_type = match wbxml::eas::find_text_after(document, fh::TYPE) {
        None => crate::model::eas_folder_type::USER_MAIL,
        Some(value) => match value.parse::<u8>() {
            Ok(value) => value,
            Err(_) => return folder_create_status_response(state, command, "8", sync_key, None),
        },
    };

    let groupware_kind = match folder_type {
        crate::model::eas_folder_type::USER_CONTACT => {
            Some(crate::jmap::client::GroupwareCollectionKind::AddressBook)
        }
        crate::model::eas_folder_type::USER_APPOINTMENT => {
            Some(crate::jmap::client::GroupwareCollectionKind::Calendar)
        }
        crate::model::eas_folder_type::USER_MAIL => None,
        _ => return folder_create_status_response(state, command, "8", sync_key, None),
    };

    if let Some(kind) = groupware_kind {
        if parent_id != "0" {
            return folder_create_status_response(state, command, "5", sync_key, None);
        }
        return match state
            .jmap
            .create_groupware_collection(auth, kind, display_name)
            .await
        {
            Ok(crate::jmap::client::CreateGroupwareCollectionOutcome::Created(id)) => {
                let server_id = match kind {
                    crate::jmap::client::GroupwareCollectionKind::AddressBook => {
                        format!("ab_{id}")
                    }
                    crate::jmap::client::GroupwareCollectionKind::Calendar => {
                        format!("cal_{id}")
                    }
                };
                folder_create_status_response(state, command, "1", sync_key, Some(server_id))
            }
            Ok(crate::jmap::client::CreateGroupwareCollectionOutcome::NameExists) => {
                folder_create_status_response(state, command, "2", sync_key, None)
            }
            Ok(crate::jmap::client::CreateGroupwareCollectionOutcome::Unsupported) => {
                folder_create_status_response(state, command, "6", sync_key, None)
            }
            Err(error) => {
                tracing::warn!(%error, "FolderCreate groupware collection failed");
                folder_create_status_response(state, command, "6", sync_key, None)
            }
        };
    }

    if parent_id != "0" && is_non_mail_collection_id(parent_id) {
        return folder_create_status_response(state, command, "5", sync_key, None);
    }
    let jmap_parent_id = if parent_id == "0" {
        None
    } else {
        Some(parent_id)
    };

    match state
        .jmap
        .create_mailbox(auth, jmap_parent_id, display_name)
        .await
    {
        Ok(crate::jmap::client::CreateMailboxOutcome::Created(id)) => {
            folder_create_status_response(state, command, "1", sync_key, Some(id))
        }
        Ok(crate::jmap::client::CreateMailboxOutcome::NameExists) => {
            folder_create_status_response(state, command, "2", sync_key, None)
        }
        Ok(crate::jmap::client::CreateMailboxOutcome::ParentNotFound) => {
            folder_create_status_response(state, command, "5", sync_key, None)
        }
        Err(error) => {
            tracing::warn!(%error, "FolderCreate failed");
            folder_create_status_response(state, command, "6", sync_key, None)
        }
    }
}

fn folder_create_status_response(
    state: &AppState,
    command: &str,
    status: &str,
    sync_key: &str,
    server_id: Option<String>,
) -> Response {
    use wbxml::eas::folder_hierarchy as fh;
    let mut builder = wbxml::eas::DocumentBuilder::new();
    builder.start(fh::FOLDER_CREATE);
    builder.leaf(fh::STATUS, status);
    builder.leaf(fh::SYNC_KEY, sync_key.to_owned());
    if let Some(server_id) = server_id {
        builder.leaf(fh::SERVER_ID, server_id);
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

/// [MS-ASCMD] section 2.2.1.4. Request: SyncKey, ServerId. Response:
/// Status, SyncKey. Status codes verified against section 2.2.3.177.4.
async fn folder_delete(
    state: &AppState,
    auth: &crate::jmap::client::AuthenticatedSession,
    document: &wbxml::Document,
    command: &str,
) -> Response {
    use wbxml::eas::folder_hierarchy as fh;

    let sync_key = wbxml::eas::find_text_after(document, fh::SYNC_KEY).unwrap_or("0");
    let Some(server_id) = wbxml::eas::find_text_after(document, fh::SERVER_ID) else {
        return folder_status_response(state, command, fh::FOLDER_DELETE, "10", sync_key);
    };

    if let Some((kind, id)) = groupware_collection_from_server_id(server_id) {
        return match state
            .jmap
            .destroy_groupware_collection(auth, kind, id)
            .await
        {
            Ok(crate::jmap::client::DestroyGroupwareCollectionOutcome::Destroyed) => {
                folder_status_response(state, command, fh::FOLDER_DELETE, "1", sync_key)
            }
            Ok(crate::jmap::client::DestroyGroupwareCollectionOutcome::NotFound) => {
                folder_status_response(state, command, fh::FOLDER_DELETE, "4", sync_key)
            }
            Ok(crate::jmap::client::DestroyGroupwareCollectionOutcome::Forbidden) => {
                folder_status_response(state, command, fh::FOLDER_DELETE, "3", sync_key)
            }
            Ok(crate::jmap::client::DestroyGroupwareCollectionOutcome::Unsupported) => {
                folder_status_response(state, command, fh::FOLDER_DELETE, "6", sync_key)
            }
            Err(error) => {
                tracing::warn!(%error, "FolderDelete groupware collection failed");
                folder_status_response(state, command, fh::FOLDER_DELETE, "6", sync_key)
            }
        };
    }
    if is_non_mail_collection_id(server_id) {
        return folder_status_response(state, command, fh::FOLDER_DELETE, "4", sync_key);
    }

    match state.jmap.destroy_mailbox(auth, server_id).await {
        Ok(crate::jmap::client::DestroyMailboxOutcome::Destroyed) => {
            folder_status_response(state, command, fh::FOLDER_DELETE, "1", sync_key)
        }
        Ok(crate::jmap::client::DestroyMailboxOutcome::NotFound) => {
            folder_status_response(state, command, fh::FOLDER_DELETE, "4", sync_key)
        }
        Ok(crate::jmap::client::DestroyMailboxOutcome::Forbidden) => {
            folder_status_response(state, command, fh::FOLDER_DELETE, "3", sync_key)
        }
        Err(error) => {
            tracing::warn!(%error, "FolderDelete failed");
            folder_status_response(state, command, fh::FOLDER_DELETE, "6", sync_key)
        }
    }
}

/// [MS-ASCMD] section 2.2.1.6. Request: SyncKey, ServerId, ParentId,
/// DisplayName (in that order, verified against section 6.16's XML
/// schema). Response: Status, SyncKey. Status codes verified against
/// section 2.2.3.177.6.
async fn folder_update(
    state: &AppState,
    auth: &crate::jmap::client::AuthenticatedSession,
    document: &wbxml::Document,
    command: &str,
) -> Response {
    use wbxml::eas::folder_hierarchy as fh;

    let sync_key = wbxml::eas::find_text_after(document, fh::SYNC_KEY).unwrap_or("0");
    let Some(server_id) = wbxml::eas::find_text_after(document, fh::SERVER_ID) else {
        return folder_status_response(state, command, fh::FOLDER_UPDATE, "10", sync_key);
    };
    let Some(parent_id) = wbxml::eas::find_text_after(document, fh::PARENT_ID) else {
        return folder_status_response(state, command, fh::FOLDER_UPDATE, "10", sync_key);
    };
    let Some(display_name) = wbxml::eas::find_text_after(document, fh::DISPLAY_NAME) else {
        return folder_status_response(state, command, fh::FOLDER_UPDATE, "10", sync_key);
    };

    if let Some((kind, id)) = groupware_collection_from_server_id(server_id) {
        if parent_id != "0" {
            return folder_status_response(state, command, fh::FOLDER_UPDATE, "4", sync_key);
        }
        return match state
            .jmap
            .update_groupware_collection(auth, kind, id, display_name)
            .await
        {
            Ok(crate::jmap::client::UpdateGroupwareCollectionOutcome::Updated) => {
                folder_status_response(state, command, fh::FOLDER_UPDATE, "1", sync_key)
            }
            Ok(crate::jmap::client::UpdateGroupwareCollectionOutcome::NotFound) => {
                folder_status_response(state, command, fh::FOLDER_UPDATE, "4", sync_key)
            }
            Ok(crate::jmap::client::UpdateGroupwareCollectionOutcome::Forbidden)
            | Ok(crate::jmap::client::UpdateGroupwareCollectionOutcome::NameExists) => {
                folder_status_response(state, command, fh::FOLDER_UPDATE, "2", sync_key)
            }
            Ok(crate::jmap::client::UpdateGroupwareCollectionOutcome::Unsupported) => {
                folder_status_response(state, command, fh::FOLDER_UPDATE, "6", sync_key)
            }
            Err(error) => {
                tracing::warn!(%error, "FolderUpdate groupware collection failed");
                folder_status_response(state, command, fh::FOLDER_UPDATE, "6", sync_key)
            }
        };
    }
    if is_non_mail_collection_id(server_id)
        || (parent_id != "0" && is_non_mail_collection_id(parent_id))
    {
        return folder_status_response(state, command, fh::FOLDER_UPDATE, "4", sync_key);
    }
    let jmap_parent_id = if parent_id == "0" {
        None
    } else {
        Some(parent_id)
    };

    match state
        .jmap
        .update_mailbox(auth, server_id, jmap_parent_id, display_name)
        .await
    {
        Ok(crate::jmap::client::UpdateMailboxOutcome::Updated) => {
            folder_status_response(state, command, fh::FOLDER_UPDATE, "1", sync_key)
        }
        Ok(crate::jmap::client::UpdateMailboxOutcome::NotFound) => {
            folder_status_response(state, command, fh::FOLDER_UPDATE, "4", sync_key)
        }
        Ok(crate::jmap::client::UpdateMailboxOutcome::Forbidden) => {
            folder_status_response(state, command, fh::FOLDER_UPDATE, "2", sync_key)
        }
        Ok(crate::jmap::client::UpdateMailboxOutcome::NameExists) => {
            folder_status_response(state, command, fh::FOLDER_UPDATE, "2", sync_key)
        }
        Err(error) => {
            tracing::warn!(%error, "FolderUpdate failed");
            folder_status_response(state, command, fh::FOLDER_UPDATE, "6", sync_key)
        }
    }
}

/// Shared by FolderDelete/FolderUpdate -- both response shapes are
/// identical (Status, SyncKey), differing only in the outer element name.
fn folder_status_response(
    state: &AppState,
    command: &str,
    outer: wbxml::token::Token,
    status: &str,
    sync_key: &str,
) -> Response {
    let mut builder = wbxml::eas::DocumentBuilder::new();
    builder.start(outer);
    builder.leaf(wbxml::eas::folder_hierarchy::STATUS, status);
    builder.leaf(wbxml::eas::folder_hierarchy::SYNC_KEY, sync_key.to_owned());
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

/// What a mail SyncRecord keeps in `jmap_state`: the device's Sync window
/// and FilterType (so Ping judges "new" exactly as the next Sync will) and
/// the account's JMAP Email state at that Sync (so both can ask Email/changes
/// what happened since). Stored as `"<window>"`, `"<window>;<filter>"` or
/// `"<window>;<filter>;<state>"`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MailRecordOptions {
    window: usize,
    filter: u8,
    email_state: String,
}

fn mail_record_options(jmap_state: &str) -> MailRecordOptions {
    let mut parts = jmap_state.splitn(3, ';');
    let window = parts
        .next()
        .and_then(|w| w.parse::<usize>().ok())
        .filter(|w| *w > 0)
        .unwrap_or(100);
    let filter = parts.next().and_then(|f| f.parse().ok()).unwrap_or(0);
    let email_state = parts.next().unwrap_or_default().to_owned();
    MailRecordOptions {
        window,
        filter,
        email_state,
    }
}

fn mail_record_state(window: usize, filter: u8, email_state: &str) -> String {
    match (filter, email_state.is_empty()) {
        (0, true) => window.to_string(),
        (_, true) => format!("{window};{filter}"),
        _ => format!("{window};{filter};{email_state}"),
    }
}

/// Whether Email/changes since a device's last Sync holds something its next
/// Sync will send: a held message that left the folder or was destroyed, or
/// a read flag that differs from what the device was told.
fn mail_changes_reason(
    changes: &crate::jmap::client::EmailChangesSince,
    seen_ids: &[String],
    read_states: &BTreeMap<String, String>,
) -> Option<&'static str> {
    let seen: BTreeSet<&str> = seen_ids.iter().map(String::as_str).collect();
    if changes
        .destroyed
        .iter()
        .any(|id| seen.contains(id.as_str()))
    {
        return Some("sent message destroyed (changes)");
    }
    for (id, (read, here)) in &changes.touched {
        if !seen.contains(id.as_str()) {
            continue;
        }
        if !here {
            return Some("sent message left the folder (changes)");
        }
        if read_states
            .get(id)
            .is_some_and(|stored| stored.as_str() != read.fingerprint())
        {
            return Some("read state changed (changes)");
        }
    }
    None
}

/// Oldest receivedAt (UTC, RFC 3339) a mail FilterType admits, or None for
/// "all" (0) and values not defined for mail ([MS-ASCMD] 2.2.3.68.2).
fn mail_filter_cutoff(filter: u8, now: chrono::DateTime<chrono::Utc>) -> Option<String> {
    let since = match filter {
        1 => now - chrono::Duration::days(1),
        2 => now - chrono::Duration::days(3),
        3 => now - chrono::Duration::weeks(1),
        4 => now - chrono::Duration::weeks(2),
        5 => now.checked_sub_months(chrono::Months::new(1))?,
        _ => return None,
    };
    Some(since.format("%Y-%m-%dT%H:%M:%SZ").to_string())
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
    // z-push (the working reference) sends this on every OPTIONS response
    // too -- this gateway never did. Matches the protocol ceiling above.
    headers.insert("MS-Server-ActiveSync", HeaderValue::from_static("14.0"));
    headers.insert("X-AspNet-Version", HeaderValue::from_static("4.0.30319"));
    // [MS-ASHTTP] OPTIONS response: the HTTP methods the endpoint accepts.
    headers.insert("Allow", HeaderValue::from_static("OPTIONS,POST"));
    headers.insert("Public", HeaderValue::from_static("OPTIONS,POST"));
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

#[cfg(test)]
mod tests {
    use super::{
        apply_body_preference, decode_entity_at, eas_conversation_id, eas_datetime,
        eas_datetime_dashes, is_non_mail_collection_id, plain_text_preview, write_email_fields,
        ProtocolVersion,
    };
    use crate::model::{Email, EmailAttachment, EmailBody, EmailBodyType};
    use crate::wbxml::eas::RequestedBodyPreference as BodyPreference;
    use crate::wbxml::eas::{airsync_base as base, DocumentBuilder};
    use crate::wbxml::Node;

    #[test]
    fn mime_body_uses_inline_wbxml_string_when_possible() {
        use crate::wbxml::{decode_document, eas, encode_document};
        let raw = b"From: sender@example.test\r\nContent-Type: text/plain\r\n\r\nHello";
        let mut builder = DocumentBuilder::new();
        super::write_mime_data(&mut builder, bytes::Bytes::copy_from_slice(raw));
        let doc = decode_document(&encode_document(&builder.finish())).unwrap();
        assert_eq!(
            eas::find_text_after(&doc, base::DATA).map(str::as_bytes),
            Some(raw.as_slice())
        );
        assert!(eas::find_opaque_after(&doc, base::DATA).is_none());

        let binary = b"binary\0\xff";
        let mut builder = DocumentBuilder::new();
        super::write_mime_data(&mut builder, bytes::Bytes::copy_from_slice(binary));
        let doc = decode_document(&encode_document(&builder.finish())).unwrap();
        assert_eq!(
            eas::find_opaque_after(&doc, base::DATA),
            Some(binary.as_slice())
        );
    }

    #[test]
    fn calendar_reminder_clear_uses_the_modern_empty_tag() {
        use crate::wbxml::eas::{self, airsync as air, calendar as cal};
        let mut event = crate::model::CalendarEvent::default();
        let before = super::calendar_event_hash(&event);
        event.reminder_minutes = Some(0);
        assert_ne!(super::calendar_event_hash(&event), before);
        let mut b = eas::DocumentBuilder::new();
        super::write_calendar_command(&mut b, air::CHANGE, &event, ProtocolVersion::V16_1, &[]);
        assert_eq!(eas::find_text_after(&b.finish(), cal::REMINDER), Some("0"));
        event.reminder_minutes = None;
        for (version, present) in [
            (ProtocolVersion::V14_0, false),
            (ProtocolVersion::V16_1, true),
        ] {
            let mut b = eas::DocumentBuilder::new();
            super::write_calendar_command(&mut b, air::CHANGE, &event, version, &[]);
            let doc = b.finish();
            assert_eq!(eas::contains_token(&doc, cal::REMINDER), present);
            if present {
                assert!(doc.nodes.iter().any(|node| matches!(node,crate::wbxml::Node::Start(token)
                    if token.code_page==cal::PAGE && token.token==cal::REMINDER.token && !token.has_content)));
            }
        }
    }

    #[test]
    fn calendar_recurrence_is_written_with_eas_fields() {
        use crate::wbxml::eas::{self, airsync as air, calendar as cal};
        let event = crate::model::CalendarEvent {
            recurrence: Some(crate::wbxml::eas::CalendarRecurrence {
                kind: 1,
                interval: Some(2),
                day_of_week: Some(10),
                occurrences: Some(12),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut builder = eas::DocumentBuilder::new();
        super::write_calendar_command(&mut builder, air::ADD, &event, ProtocolVersion::V16_1, &[]);
        let document = builder.finish();
        assert_eq!(
            eas::find_text_after(&document, cal::RECURRENCE_TYPE),
            Some("1")
        );
        assert_eq!(
            eas::find_text_after(&document, cal::RECURRENCE_INTERVAL),
            Some("2")
        );
        assert_eq!(
            eas::find_text_after(&document, cal::RECURRENCE_DAY_OF_WEEK),
            Some("10")
        );
        assert_eq!(
            eas::find_text_after(&document, cal::RECURRENCE_OCCURRENCES),
            Some("12")
        );
    }

    #[test]
    fn excluded_occurrences_use_versioned_exception_identifiers() {
        use crate::wbxml::eas::{self, airsync as air, airsync_base as base, calendar as cal};
        let event = crate::model::CalendarEvent {
            uid: Some("series-uid".to_owned()),
            recurrence_exceptions: vec![crate::model::CalendarEventException {
                instance_id: "20261125T170000Z".to_owned(),
                deleted: true,
                ..Default::default()
            }],
            ..Default::default()
        };
        for (version, expected_token, expected_value) in [
            (
                ProtocolVersion::V14_0,
                cal::EXCEPTION_START_TIME,
                "20261125T170000Z",
            ),
            (
                ProtocolVersion::V16_1,
                base::INSTANCE_ID,
                "2026-11-25T17:00:00.000Z",
            ),
        ] {
            let mut builder = eas::DocumentBuilder::new();
            super::write_calendar_command(&mut builder, air::CHANGE, &event, version, &[]);
            let document = builder.finish();
            assert_eq!(
                eas::find_text_after(&document, expected_token),
                Some(expected_value)
            );
            assert_eq!(eas::find_text_after(&document, cal::DELETED), Some("1"));
            assert_eq!(
                eas::find_text_after(&document, cal::UID),
                Some("series-uid")
            );
            assert!(eas::contains_token(&document, cal::EXCEPTIONS));
        }

        let event = crate::model::CalendarEvent {
            uid: Some("series-uid".to_owned()),
            recurrence_exceptions: vec![crate::model::CalendarEventException {
                instance_id: "20261125T170000Z".to_owned(),
                title: Some("Changed occurrence".to_owned()),
                location: Some("Changed room".to_owned()),
                location_coordinates: Some(("43.65".to_owned(), "-79.38".to_owned())),
                start_utc: Some("20261125T180000Z".to_owned()),
                end_utc: Some("20261125T190000Z".to_owned()),
                all_day: Some(true),
                busy_status: Some(1),
                reminder_minutes: Some(Some(15)),
                categories: Some(vec!["Work".to_owned()]),
                attendees: Some(vec![crate::model::Attendee {
                    email: "guest@example.test".to_owned(),
                    name: Some("Guest".to_owned()),
                    participation_status: Some("tentative".to_owned()),
                    optional: true,
                }]),
                description: Some(crate::model::EmailBody {
                    body_type: crate::model::EmailBodyType::Plain,
                    value: "Changed body".to_owned(),
                    is_truncated: false,
                }),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut builder = eas::DocumentBuilder::new();
        super::write_calendar_command(
            &mut builder,
            air::CHANGE,
            &event,
            ProtocolVersion::V16_1,
            &[],
        );
        let document = builder.finish();
        assert!(eas::find_all_text_after(&document, cal::SUBJECT)
            .iter()
            .any(|subject| subject == "Changed occurrence"));
        assert!(eas::find_all_text_after(&document, base::DATA)
            .iter()
            .any(|body| body == "Changed body"));
        assert!(eas::find_all_text_after(&document, base::DISPLAY_NAME)
            .iter()
            .any(|location| location == "Changed room"));
        assert_eq!(
            eas::find_text_after(&document, base::LATITUDE),
            Some("43.65")
        );
        assert_eq!(
            eas::find_text_after(&document, base::LONGITUDE),
            Some("-79.38")
        );
        assert_eq!(
            eas::find_text_after(&document, cal::START_TIME),
            Some("20261125T180000Z")
        );
        assert_eq!(
            eas::find_text_after(&document, cal::END_TIME),
            Some("20261125T190000Z")
        );
        assert!(eas::find_all_text_after(&document, cal::ALL_DAY_EVENT)
            .iter()
            .any(|value| value == "1"));
        assert!(eas::find_all_text_after(&document, cal::BUSY_STATUS)
            .iter()
            .any(|value| value == "1"));
        assert!(eas::find_all_text_after(&document, cal::REMINDER)
            .iter()
            .any(|value| value == "15"));
        assert_eq!(
            eas::find_all_text_after(&document, cal::CATEGORY),
            vec!["Work"]
        );
        assert_eq!(
            eas::find_text_after(&document, cal::ATTENDEE_EMAIL),
            Some("guest@example.test")
        );
        assert_eq!(
            eas::find_text_after(&document, cal::ATTENDEE_STATUS),
            Some("2")
        );
        assert!(!eas::contains_token(&document, cal::DELETED));
    }

    #[test]
    fn occurrence_reminder_clear_uses_empty_tag_only_for_16_x() {
        use crate::wbxml::eas::{self, airsync as air, calendar as cal};
        let event = crate::model::CalendarEvent {
            recurrence_exceptions: vec![crate::model::CalendarEventException {
                instance_id: "20261125T170000Z".to_owned(),
                reminder_minutes: Some(None),
                ..Default::default()
            }],
            ..Default::default()
        };
        for (version, expected) in [
            (ProtocolVersion::V14_0, false),
            (ProtocolVersion::V16_1, true),
        ] {
            let mut builder = eas::DocumentBuilder::new();
            super::write_calendar_command(&mut builder, air::CHANGE, &event, version, &[]);
            let document = builder.finish();
            assert_eq!(eas::contains_token(&document, cal::REMINDER), expected);
        }
    }

    #[test]
    fn occurrence_change_gate_accepts_supported_fields_and_rejects_unimplemented_shapes() {
        let mut fields = crate::wbxml::eas::CalendarFields {
            subject: Some("Occurrence".to_owned()),
            ..Default::default()
        };
        assert!(super::calendar_instance_change_supported(&fields));
        fields.sensitivity = Some(2);
        assert!(!super::calendar_instance_change_supported(&fields));
        fields.sensitivity = None;
        fields.reminder = Some(Some(10));
        assert!(super::calendar_instance_change_supported(&fields));
        fields.reminder = None;
        fields.start_time = Some("20261125T170000Z".to_owned());
        assert!(super::calendar_instance_change_supported(&fields));
        fields.start_time = None;
        fields.body = Some("body".to_owned());
        fields.body_type = Some(3);
        assert!(!super::calendar_instance_change_supported(&fields));
        fields.body_type = None;
        fields.location = Some("Room".to_owned());
        assert!(super::calendar_instance_change_supported(&fields));
        fields.unsupported_location = true;
        assert!(!super::calendar_instance_change_supported(&fields));
    }

    #[test]
    fn series_rule_and_time_changes_clear_exceptions_only_in_eas_16_x() {
        let fields = crate::wbxml::eas::CalendarFields {
            recurrence_present: true,
            ..Default::default()
        };
        assert!(!super::calendar_series_change_clears_exceptions(
            ProtocolVersion::V14_0,
            &fields
        ));
        assert!(super::calendar_series_change_clears_exceptions(
            ProtocolVersion::V16_0,
            &fields
        ));
        assert!(super::calendar_series_change_clears_exceptions(
            ProtocolVersion::V16_1,
            &fields
        ));

        let fields = crate::wbxml::eas::CalendarFields {
            end_time: Some("20261125T180000Z".to_owned()),
            ..Default::default()
        };
        assert!(super::calendar_series_change_clears_exceptions(
            ProtocolVersion::V16_1,
            &fields
        ));
        let fields = crate::wbxml::eas::CalendarFields {
            subject: Some("Title only".to_owned()),
            ..Default::default()
        };
        assert!(!super::calendar_series_change_clears_exceptions(
            ProtocolVersion::V16_1,
            &fields
        ));
    }

    #[test]
    fn calendar_client_uid_stabilizes_add_replay_identity() {
        let first = super::calendar_add_replay_identity(
            "device-a",
            "calendar-a",
            "client-id-one",
            Some("client-uid-stable"),
        );
        let retry_with_new_client_id = super::calendar_add_replay_identity(
            "device-a",
            "calendar-a",
            "client-id-two",
            Some("client-uid-stable"),
        );
        assert_eq!(first, retry_with_new_client_id);
        assert_ne!(
            first,
            super::calendar_add_replay_identity("device-a", "calendar-a", "client-id-one", None,)
        );
        assert_ne!(
            first,
            super::calendar_add_replay_identity(
                "device-b",
                "calendar-a",
                "client-id-one",
                Some("client-uid-stable"),
            )
        );
    }

    #[test]
    fn task_recurrence_is_written_with_tasks_codepage_fields() {
        use crate::wbxml::eas::{self, airsync as air, tasks};
        let task = crate::model::Task {
            id: "task-1".to_owned(),
            title: "Weekly task".to_owned(),
            description: None,
            completed: false,
            date_completed: None,
            importance: None,
            sensitivity: 0,
            categories: Vec::new(),
            reminder_time: None,
            start: Some("20261124T090000Z".to_owned()),
            start_local: Some("20261124T090000Z".to_owned()),
            due: None,
            due_local: None,
            recurrence: Some(crate::wbxml::eas::CalendarRecurrence {
                kind: 1,
                interval: Some(2),
                day_of_week: Some(10),
                occurrences: Some(8),
                ..Default::default()
            }),
        };
        let mut builder = eas::DocumentBuilder::new();
        super::write_task_command(&mut builder, air::ADD, &task, &[], ProtocolVersion::V16_1);
        let document = builder.finish();
        assert_eq!(eas::find_text_after(&document, tasks::TYPE), Some("1"));
        assert_eq!(eas::find_text_after(&document, tasks::INTERVAL), Some("2"));
        assert_eq!(
            eas::find_text_after(&document, tasks::DAY_OF_WEEK),
            Some("10")
        );
        assert_eq!(
            eas::find_text_after(&document, tasks::OCCURRENCES),
            Some("8")
        );
    }

    #[test]
    fn contact_address_fields_are_emitted_and_included_in_change_hashes() {
        use crate::wbxml::eas::{self, contacts as c, contacts2 as c2};
        let mut contact = crate::model::Contact {
            middle_name: Some("Augusta".into()),
            suffix: Some("Jr.".into()),
            name_prefix: Some("Countess".into()),
            yomi_first_name: Some("エイダ".into()),
            yomi_last_name: Some("ラブレス".into()),
            yomi_company_name: Some("アナリティカル エンジンズ".into()),
            office_location: Some("Building 4, Floor 2".into()),
            nickname: Some("Enchantress of Numbers".into()),
            company_main_phone: Some("555-0199".into()),
            mms: Some("ada@mms.example".into()),
            account_name: Some("ada-eas-account".into()),
            picture: Some(vec![1, 2, 3, 4]),
            im_address1: Some("ada@example.im".into()),
            im_address2: Some("ada@chat.example".into()),
            im_address3: Some("ada@xmpp.example".into()),
            department: Some("Research".into()),
            home_phone2: Some("555-0102".into()),
            business_phone2: Some("555-0104".into()),
            home_fax: Some("555-0105".into()),
            business_fax: Some("555-0109".into()),
            pager: Some("555-0106".into()),
            car_phone: Some("555-0107".into()),
            radio_phone: Some("555-0108".into()),
            assistant_phone: Some("555-0110".into()),
            web_page: Some("https://example.com/ada".into()),
            business_address: Some(crate::model::ContactAddress {
                city: Some("Toronto".into()),
                country: Some("Canada".into()),
                postal_code: Some("M5V 2T6".into()),
                state: Some("Ontario".into()),
                street: Some("100 King St W".into()),
            }),
            home_address: Some(crate::model::ContactAddress {
                city: Some("Ottawa".into()),
                ..Default::default()
            }),
            categories: vec!["Work".into(), "Friends".into()],
            ..Default::default()
        };
        let before = super::contact_hash(&contact);
        let mut builder = DocumentBuilder::new();
        super::write_contact_fields(&mut builder, &contact, true);
        let doc = builder.finish();
        assert_eq!(
            eas::find_text_after(&doc, c::BUSINESS_ADDRESS_CITY),
            Some("Toronto")
        );
        assert_eq!(
            eas::find_text_after(&doc, c::BUSINESS_ADDRESS_COUNTRY),
            Some("Canada")
        );
        assert_eq!(
            eas::find_text_after(&doc, c::BUSINESS_ADDRESS_POSTAL_CODE),
            Some("M5V 2T6")
        );
        assert_eq!(
            eas::find_text_after(&doc, c::BUSINESS_ADDRESS_STATE),
            Some("Ontario")
        );
        assert_eq!(
            eas::find_text_after(&doc, c::BUSINESS_ADDRESS_STREET),
            Some("100 King St W")
        );
        assert_eq!(
            eas::find_text_after(&doc, c::HOME_ADDRESS_CITY),
            Some("Ottawa")
        );
        assert!(eas::contains_token(&doc, c::OTHER_ADDRESS_STREET));
        assert_eq!(
            eas::find_all_text_after(&doc, c::CATEGORY),
            vec!["Work", "Friends"]
        );
        assert_eq!(eas::find_text_after(&doc, c::MIDDLE_NAME), Some("Augusta"));
        assert_eq!(eas::find_text_after(&doc, c::SUFFIX), Some("Jr."));
        assert_eq!(eas::find_text_after(&doc, c::TITLE), Some("Countess"));
        assert_eq!(
            eas::find_text_after(&doc, c::YOMI_FIRST_NAME),
            Some("エイダ")
        );
        assert_eq!(
            eas::find_text_after(&doc, c::YOMI_LAST_NAME),
            Some("ラブレス")
        );
        assert_eq!(eas::find_text_after(&doc, c::DEPARTMENT), Some("Research"));
        assert_eq!(
            eas::find_text_after(&doc, c::HOME_2_PHONE_NUMBER),
            Some("555-0102")
        );
        assert_eq!(
            eas::find_text_after(&doc, c::BUSINESS_2_PHONE_NUMBER),
            Some("555-0104")
        );
        assert_eq!(
            eas::find_text_after(&doc, c::HOME_FAX_NUMBER),
            Some("555-0105")
        );
        assert_eq!(
            eas::find_text_after(&doc, c::BUSINESS_FAX_NUMBER),
            Some("555-0109")
        );
        assert_eq!(
            eas::find_text_after(&doc, c::PAGER_NUMBER),
            Some("555-0106")
        );
        assert_eq!(
            eas::find_text_after(&doc, c::CAR_PHONE_NUMBER),
            Some("555-0107")
        );
        assert_eq!(
            eas::find_text_after(&doc, c::RADIO_PHONE_NUMBER),
            Some("555-0108")
        );
        assert_eq!(
            eas::find_text_after(&doc, c::ASSISTANT_PHONE_NUMBER),
            Some("555-0110")
        );
        assert_eq!(
            eas::find_text_after(&doc, c::YOMI_FIRST_NAME),
            Some("エイダ")
        );
        assert_eq!(
            eas::find_text_after(&doc, c::YOMI_LAST_NAME),
            Some("ラブレス")
        );
        assert_eq!(
            eas::find_text_after(&doc, c::YOMI_COMPANY_NAME),
            Some("アナリティカル エンジンズ")
        );
        assert_eq!(
            eas::find_text_after(&doc, c::OFFICE_LOCATION),
            Some("Building 4, Floor 2")
        );
        assert_eq!(
            eas::find_text_after(&doc, c2::NICK_NAME),
            Some("Enchantress of Numbers")
        );
        assert_eq!(
            eas::find_text_after(&doc, c::WEB_PAGE),
            Some("https://example.com/ada")
        );
        contact.assistant_phone = Some("555-0111".into());
        assert_ne!(super::contact_hash(&contact), before);
        contact.yomi_first_name = Some("エイダ改".into());
        assert_ne!(super::contact_hash(&contact), before);
        contact.yomi_company_name = Some("アナリティカル エンジンズ改".into());
        assert_ne!(super::contact_hash(&contact), before);
        contact.office_location = Some("Building 5".into());
        assert_ne!(super::contact_hash(&contact), before);
        contact.nickname = Some("Lady Ada".into());
        assert_ne!(super::contact_hash(&contact), before);
        contact.company_main_phone = Some("555-0188".into());
        assert_ne!(super::contact_hash(&contact), before);
        contact.mms = Some("new@mms.example".into());
        assert_ne!(super::contact_hash(&contact), before);
        contact.account_name = Some("new-account".into());
        assert_ne!(super::contact_hash(&contact), before);
        contact.picture = Some(vec![4, 3, 2, 1]);
        assert_ne!(super::contact_hash(&contact), before);
        contact.im_address1 = Some("ada2@example.im".into());
        assert_ne!(super::contact_hash(&contact), before);
        contact.business_address.as_mut().unwrap().city = Some("Hamilton".into());
        assert_ne!(super::contact_hash(&contact), before);
    }

    #[test]
    fn calendar_working_elsewhere_is_only_sent_to_newer_protocols() {
        use crate::wbxml::eas::{self, airsync as air, calendar as cal};
        let event = crate::model::CalendarEvent {
            busy_status: Some(4),
            ..Default::default()
        };
        for (version, expected) in [(ProtocolVersion::V14_0, "2"), (ProtocolVersion::V16_1, "4")] {
            let mut b = eas::DocumentBuilder::new();
            super::write_calendar_command(&mut b, air::CHANGE, &event, version, &[]);
            assert_eq!(
                eas::find_text_after(&b.finish(), cal::BUSY_STATUS),
                Some(expected)
            );
        }
    }

    #[test]
    fn calendar_timestamp_is_stable_and_tracks_modification() {
        use crate::wbxml::eas::{self, airsync as air, calendar as cal};
        let mut event = crate::model::CalendarEvent::default();
        let before = super::calendar_event_hash(&event);
        let mut b = eas::DocumentBuilder::new();
        super::write_calendar_command(&mut b, air::ADD, &event, ProtocolVersion::V16_1, &[]);
        assert_eq!(eas::find_text_after(&b.finish(), cal::DTSTAMP), None);
        event.updated_utc = Some("20240229T235958Z".into());
        assert_ne!(super::calendar_event_hash(&event), before);
        let mut b = eas::DocumentBuilder::new();
        super::write_calendar_command(&mut b, air::ADD, &event, ProtocolVersion::V16_1, &[]);
        assert_eq!(
            eas::find_text_after(&b.finish(), cal::DTSTAMP),
            Some("20240229T235958Z")
        );
    }

    #[test]
    fn calendar_coordinates_are_versioned_and_trigger_changes() {
        use crate::wbxml::eas::{self, airsync as air, airsync_base as base};
        let mut event = crate::model::CalendarEvent::default();
        let original = super::calendar_event_hash(&event);
        event.location_coordinates = Some(("43.65".into(), "-79.38".into()));
        assert_ne!(super::calendar_event_hash(&event), original);
        for (version, expected) in [
            (ProtocolVersion::V14_0, None),
            (ProtocolVersion::V16_1, Some("43.65")),
        ] {
            let mut b = eas::DocumentBuilder::new();
            super::write_calendar_command(&mut b, air::CHANGE, &event, version, &[]);
            assert_eq!(eas::find_text_after(&b.finish(), base::LATITUDE), expected);
        }
    }

    #[test]
    fn calendar_location_wire_format_and_clear_follow_the_protocol_version() {
        use crate::wbxml::{
            self,
            eas::{self, airsync as air, airsync_base as base, calendar as cal},
        };
        for version in [
            super::ProtocolVersion::V14_0,
            super::ProtocolVersion::V14_1,
            super::ProtocolVersion::V16_0,
            super::ProtocolVersion::V16_1,
        ] {
            let modern = matches!(
                version,
                super::ProtocolVersion::V16_0 | super::ProtocolVersion::V16_1
            );
            for location in [Some("Room A".to_owned()), None] {
                let event = crate::model::CalendarEvent {
                    id: "event".to_owned(),
                    location: location.clone(),
                    ..Default::default()
                };
                let mut b = eas::DocumentBuilder::new();
                super::write_calendar_command(&mut b, air::CHANGE, &event, version, &[]);
                let doc = wbxml::decode_document(&wbxml::encode_document(&b.finish())).unwrap();
                assert_eq!(
                    eas::contains_token(&doc, cal::LOCATION),
                    !modern && location.is_some()
                );
                assert_eq!(eas::contains_token(&doc, base::LOCATION), modern);
                if modern {
                    assert_eq!(
                        eas::find_text_after(&doc, base::DISPLAY_NAME),
                        location.as_deref()
                    );
                    if location.is_none() {
                        assert!(doc.nodes.iter().any(|node| matches!(node, wbxml::Node::Start(token)
                            if token.code_page == base::PAGE && token.token == base::LOCATION.token && !token.has_content)));
                    }
                } else {
                    assert_eq!(
                        eas::find_text_after(&doc, cal::LOCATION),
                        location.as_deref()
                    );
                }
            }
        }
    }

    #[test]
    fn eas_datetime_strips_dashes_and_colons() {
        assert_eq!(eas_datetime("2026-08-24T02:05:00Z"), "20260824T020500Z");
    }

    #[test]
    fn calendar_meeting_status_reflects_cancellation() {
        use crate::wbxml::eas::{airsync as air, calendar as cal, DocumentBuilder};
        let mut event = crate::model::CalendarEvent {
            organizer_email: Some("owner@example.test".into()),
            attendees: vec![crate::model::Attendee {
                email: "attendee@example.test".into(),
                name: None,
                participation_status: None,
                optional: false,
            }],
            ..Default::default()
        };
        for (organizer, cancelled, expected) in [
            (true, false, "1"),
            (false, false, "3"),
            (true, true, "5"),
            (false, true, "7"),
        ] {
            event.is_organizer = organizer;
            event.cancelled = cancelled;
            let mut builder = DocumentBuilder::new();
            super::write_calendar_command(
                &mut builder,
                air::CHANGE,
                &event,
                super::ProtocolVersion::V16_1,
                &[],
            );
            assert_eq!(
                crate::wbxml::eas::find_text_after(&builder.finish(), cal::MEETING_STATUS),
                Some(expected)
            );
        }
    }

    #[test]
    fn calendar_response_type_is_omitted_when_not_known() {
        use crate::wbxml::eas::{airsync as air, calendar as cal, DocumentBuilder};
        let mut event = crate::model::CalendarEvent::default();
        let mut builder = DocumentBuilder::new();
        super::write_calendar_command(
            &mut builder,
            air::CHANGE,
            &event,
            super::ProtocolVersion::V16_1,
            &[],
        );
        assert!(!crate::wbxml::eas::contains_token(
            &builder.finish(),
            cal::RESPONSE_TYPE
        ));
        event.response_type = Some(3);
        let mut builder = DocumentBuilder::new();
        super::write_calendar_command(
            &mut builder,
            air::CHANGE,
            &event,
            super::ProtocolVersion::V16_1,
            &[],
        );
        assert_eq!(
            crate::wbxml::eas::find_text_after(&builder.finish(), cal::RESPONSE_TYPE),
            Some("3")
        );
    }

    #[test]
    fn is_non_mail_collection_id_rejects_the_synthetic_prefixes() {
        // FolderCreate/Update/Delete are mail-only -- these prefixes are
        // this gateway's own synthetic ids for Notes/Contacts/Calendar
        // (see jmap::client::collections()), never a real Mailbox id.
        assert!(is_non_mail_collection_id("note_q"));
        assert!(is_non_mail_collection_id("ab_b"));
        assert!(is_non_mail_collection_id("cal_b"));
        assert!(is_non_mail_collection_id("task_b"));
        assert!(!is_non_mail_collection_id("a"));
        assert!(!is_non_mail_collection_id("0"));
    }

    #[test]
    fn eas_datetime_drops_fractional_seconds() {
        assert_eq!(eas_datetime("2026-08-24T02:05:00.123Z"), "20260824T020500Z");
    }

    #[test]
    fn eas_datetime_dashes_matches_real_zpush_wire_format() {
        // Real bug, found via a direct side-by-side wire comparison
        // against z-push for the same real message: z-push sent
        // DateReceived as "2026-08-24T19:37:00.000Z" for a message this
        // gateway sent as "20260824T193730Z" for -- dashes/colons kept,
        // literal ".000" appended, confirmed against
        // src/lib/core/streamer.php's formatDate() for
        // STREAMER_TYPE_DATE_DASHES: yyyy-MM-dd'T'HH:mm:SS'.000Z'.
        assert_eq!(
            eas_datetime_dashes("2026-08-24T19:37:00Z"),
            "2026-08-24T19:37:00.000Z"
        );
    }

    #[test]
    fn eas_datetime_dashes_replaces_real_fractional_seconds_with_literal_000() {
        // z-push's format string is a fixed ".000Z" suffix, not real
        // milliseconds -- even if the source has real sub-second
        // precision, the wire value is always ".000".
        assert_eq!(
            eas_datetime_dashes("2026-08-24T19:37:00.123Z"),
            "2026-08-24T19:37:00.000Z"
        );
    }

    #[test]
    fn plain_text_preview_strips_html_tags_and_collapses_whitespace() {
        let body = EmailBody {
            is_truncated: false,
            body_type: EmailBodyType::Html,
            value: "<html>\n<body>\n<p>Hi   Raj,</p>\n<div><br>\n</div>\n<div>Please put below in the system.</div>\n</body>\n</html>".to_owned(),
        };
        assert_eq!(
            plain_text_preview(&body),
            "Hi Raj, Please put below in the system."
        );
    }

    #[test]
    fn plain_text_preview_passes_plain_text_through() {
        let body = EmailBody {
            is_truncated: false,
            body_type: EmailBodyType::Plain,
            value: "Hello  world".to_owned(),
        };
        assert_eq!(plain_text_preview(&body), "Hello world");
    }

    #[test]
    fn plain_text_preview_skips_style_and_script_content() {
        // Real bug, confirmed live: a <head><style> block's CSS is not
        // tag markup, so a naive "strip anything between < and >"
        // stripper left it completely intact -- and since it's the
        // first text in the document, it became the entire preview
        // (e.g. "@import url(...) :root { color-scheme: ... }").
        let body = EmailBody {
            is_truncated: false,
            body_type: EmailBodyType::Html,
            value: "<html><head><style>@import url(\"https://example.com/x.css\"); :root { color-scheme: light dark; }</style><script>track();</script></head><body><p>Invoice attached, thanks!</p></body></html>".to_owned(),
        };
        assert_eq!(plain_text_preview(&body), "Invoice attached, thanks!");
    }

    #[test]
    fn plain_text_preview_handles_gt_inside_quoted_attribute_values() {
        // Real bug: a '>' inside a quoted attribute value (common in
        // inline CSS/JS -- a numeric comparison, or a CSS calc()/media
        // query) prematurely ended the "inside a tag" state with no
        // quote tracking, leaking the rest of that tag's text (its
        // remaining attributes, the real closing '>') as literal output
        // -- visible as fragments of raw markup in an otherwise-stripped
        // preview.
        let body = EmailBody {
            is_truncated: false,
            body_type: EmailBodyType::Html,
            value: r#"<div style="width:5px>2px?10px:0" data-x="a>b" onclick="if(x>1)y()">Real text</div>"#.to_owned(),
        };
        assert_eq!(plain_text_preview(&body), "Real text");
    }

    #[test]
    fn plain_text_preview_decodes_html_entities() {
        // Real bug, reported live against a real VoIP.ms email: the list
        // preview showed literal "&nbsp;" text instead of a space --
        // strip_html_tags only ever removed markup, entity references
        // (text content, not tags) passed straight through undecoded.
        let body = EmailBody {
            is_truncated: false,
            body_type: EmailBodyType::Html,
            value: "Raj Singh&nbsp; Director of Sales &amp; Support".to_owned(),
        };
        assert_eq!(
            plain_text_preview(&body),
            "Raj Singh Director of Sales & Support"
        );
    }

    #[test]
    fn decode_entity_at_handles_named_decimal_and_hex_forms() {
        assert_eq!(
            decode_entity_at(&"&nbsp; rest".chars().collect::<Vec<_>>()),
            Some((' ', 6))
        );
        assert_eq!(
            decode_entity_at(&"&#39;s".chars().collect::<Vec<_>>()),
            Some(('\'', 5))
        );
        assert_eq!(
            decode_entity_at(&"&#x27;s".chars().collect::<Vec<_>>()),
            Some(('\'', 6))
        );
        // An unrecognized/malformed entity, or a bare '&' in ordinary
        // unencoded text, is left untouched rather than guessed at.
        assert_eq!(
            decode_entity_at(&"&whatever;".chars().collect::<Vec<_>>()),
            None
        );
        assert_eq!(
            decode_entity_at(&"& rest".chars().collect::<Vec<_>>()),
            None
        );
    }

    #[test]
    fn body_preferences_prefer_native_then_fallback_and_preserve_utf8_boundaries() {
        let body = EmailBody {
            is_truncated: false,
            body_type: EmailBodyType::Html,
            value: "<p>éclair hello</p>".to_owned(),
        };
        let requests = [
            BodyPreference {
                body_type: 2,
                truncation_size: Some(4),
                all_or_none: true,
                preview: Some(3),
            },
            BodyPreference {
                body_type: 1,
                truncation_size: Some(3),
                ..Default::default()
            },
        ];
        let selected = super::select_body_preference(Some(&body), None, &requests).unwrap();
        assert_eq!(selected.body_type, 1);
        let (kind, text, full_size, truncated) =
            apply_body_preference(body.clone(), Some(selected));
        assert_eq!(kind, EmailBodyType::Plain);
        assert_eq!(text, "éc");
        assert_eq!(full_size, "éclair hello".len());
        assert!(truncated);
        let requests = [
            BodyPreference {
                body_type: 1,
                ..Default::default()
            },
            BodyPreference {
                body_type: 2,
                ..Default::default()
            },
        ];
        assert_eq!(
            super::select_body_preference(Some(&body), None, &requests)
                .unwrap()
                .body_type,
            2
        );
        let plain = EmailBody {
            is_truncated: false,
            body_type: EmailBodyType::Plain,
            value: "a<&".to_owned(),
        };
        let request = BodyPreference {
            body_type: 2,
            ..Default::default()
        };
        let (kind, text, _, _) = apply_body_preference(plain, Some(request));
        assert_eq!(kind, EmailBodyType::Html);
        assert!(text.contains("a&lt;&amp;"));
    }

    #[test]
    fn prepared_text_retains_only_the_requested_prefix_and_full_size() {
        let value = "x".repeat(100_000);
        let body = EmailBody {
            body_type: EmailBodyType::Plain,
            value,
            is_truncated: false,
        };
        let prepared = super::prepare_text_body(
            body,
            Some(BodyPreference {
                body_type: 1,
                truncation_size: Some(17),
                preview: Some(0),
                ..Default::default()
            }),
            None,
        );
        assert_eq!(prepared.full_size, 100_000);
        let data = prepared.data.unwrap();
        assert_eq!(data.len(), 17);
        assert!(data.capacity() < prepared.full_size);
        assert!(prepared.truncated);
        assert!(prepared.preview.is_none());
    }

    #[test]
    fn mime_prefix_does_not_retain_the_full_download_allocation() {
        let data = vec![b'x'; 1024 * 1024];
        let original = data.as_ptr();
        let request = BodyPreference {
            body_type: 4,
            truncation_size: Some(17),
            ..Default::default()
        };
        let mime = super::MimeBody::from_download(data, &request);
        assert_eq!(mime.full_size, 1024 * 1024);
        assert_eq!(mime.data.as_ref(), &[b'x'; 17]);
        assert_ne!(mime.data.as_ptr(), original);

        let data = vec![b'y'; 1024];
        let original = data.as_ptr();
        let request = BodyPreference {
            truncation_size: None,
            ..request
        };
        let mime = super::MimeBody::from_download(data, &request);
        assert_eq!(mime.data.as_ptr(), original);
        assert_eq!(mime.data.len(), mime.full_size);

        let request = BodyPreference {
            truncation_size: Some(1),
            all_or_none: true,
            ..request
        };
        let mime = super::MimeBody::from_download(vec![b'z'; 1024], &request);
        assert!(mime.data.is_empty());
        assert_eq!(mime.full_size, 1024);
    }

    #[test]
    fn all_or_none_omits_an_oversized_body_and_is_ignored_without_a_limit() {
        let body = EmailBody {
            is_truncated: false,
            body_type: EmailBodyType::Plain,
            value: "éclair".to_owned(),
        };
        let request = BodyPreference {
            body_type: 1,
            truncation_size: Some(1),
            all_or_none: true,
            preview: None,
        };
        let (_, text, size, truncated) = apply_body_preference(body.clone(), Some(request));
        assert!(text.is_empty());
        assert_eq!(size, "éclair".len());
        assert!(truncated);
        let request = BodyPreference {
            truncation_size: None,
            ..request
        };
        let (_, text, size, truncated) = apply_body_preference(body, Some(request));
        assert_eq!(text, "éclair");
        assert_eq!(size, text.len());
        assert!(!truncated);
    }

    #[test]
    fn apply_body_preference_none_sends_full_body_untouched() {
        let body = EmailBody {
            is_truncated: false,
            body_type: EmailBodyType::Html,
            value: "<p>Hello there</p>".to_owned(),
        };
        let (out_type, out_value, full_len, truncated) = apply_body_preference(body, None);
        assert_eq!(out_type, EmailBodyType::Html);
        assert_eq!(out_value, "<p>Hello there</p>");
        assert_eq!(full_len, "<p>Hello there</p>".len());
        assert!(!truncated);
    }

    #[test]
    fn rtf_body_preference_converts_text_and_escapes_rtf_metacharacters() {
        let body = EmailBody {
            is_truncated: false,
            body_type: EmailBodyType::Plain,
            value: "Hi {you}\\\n雪".to_owned(),
        };
        let (kind, value, size, truncated) = apply_body_preference(
            body,
            Some(BodyPreference {
                body_type: 3,
                ..Default::default()
            }),
        );
        assert_eq!(kind, EmailBodyType::Rtf);
        assert!(value.starts_with(r"{\rtf1"));
        assert!(value.contains(r"\{you\}"));
        assert!(value.contains(r"\line "));
        assert!(value.contains(r"\u-"));
        assert_eq!(size, value.len());
        assert!(!truncated);
    }

    #[test]
    fn rtf_output_obeys_byte_truncation_and_all_or_none() {
        let body = EmailBody {
            is_truncated: false,
            body_type: EmailBodyType::Plain,
            value: "long text".to_owned(),
        };
        let request = BodyPreference {
            body_type: 3,
            truncation_size: Some(12),
            ..Default::default()
        };
        let (kind, value, full_size, truncated) =
            apply_body_preference(body.clone(), Some(request));
        assert_eq!(kind, EmailBodyType::Rtf);
        assert_eq!(value.len(), 12);
        assert!(truncated);
        let (_, omitted, size, truncated) = apply_body_preference(
            body,
            Some(BodyPreference {
                all_or_none: true,
                ..request
            }),
        );
        assert!(omitted.is_empty());
        assert_eq!(size, full_size);
        assert!(truncated);
    }

    #[test]
    fn apply_body_preference_real_bug_ipad_plain_500_truncates_html_to_plain_text() {
        // The exact request shape confirmed live: a real iPad's Sync
        // Options carried BodyPreference{Type: 1, TruncationSize: 500}.
        // Sending the full untruncated HTML back (what every build before
        // this fix did) is what caused the device to never advance its
        // SyncKey off "0".
        let long_text = "word ".repeat(200); // 1000 chars, well over 500
        let body = EmailBody {
            is_truncated: false,
            body_type: EmailBodyType::Html,
            value: format!("<p>{long_text}</p>"),
        };
        let pref = BodyPreference {
            body_type: 1,
            truncation_size: Some(500),
            ..Default::default()
        };
        let (out_type, out_value, full_len, truncated) = apply_body_preference(body, Some(pref));
        assert_eq!(out_type, EmailBodyType::Plain);
        assert_eq!(out_value.len(), 500);
        assert!(!out_value.contains('<'));
        // full_len reflects the converted (HTML-stripped, whitespace-
        // collapsed) plain text's true length, not the original HTML
        // source's -- 200 "word " tokens collapsed to single spaces is
        // one byte shorter than the source (no trailing space survives
        // the join). EstimatedDataSize describes what Type/Data now
        // claim to be (plain text).
        assert_eq!(full_len, long_text.trim_end().len());
        assert!(truncated);
    }

    #[test]
    fn apply_body_preference_plain_conversion_collapses_leading_blank_lines() {
        // Real bug, confirmed live: an HTML source's <head>/indentation
        // whitespace survived tag-stripping untouched, so a small
        // TruncationSize budget was mostly blank lines before any real
        // text -- e.g. a real synced message's first 80 characters were
        // literally "\n\n\n\n    \n    \n    \n    \n    \n\n    \n\n\n\n\n    \n ...".
        let body = EmailBody {
            is_truncated: false,
            body_type: EmailBodyType::Html,
            value: "<html>\n<head>\n    \n    \n</head>\n<body>\n\n\nHello world\n</body>\n</html>"
                .to_owned(),
        };
        let pref = BodyPreference {
            body_type: 1,
            truncation_size: Some(500),
            ..Default::default()
        };
        let (_, out_value, _, truncated) = apply_body_preference(body, Some(pref));
        assert_eq!(out_value, "Hello world");
        assert!(!truncated);
    }

    #[test]
    fn apply_body_preference_short_body_under_truncation_size_is_not_truncated() {
        let body = EmailBody {
            is_truncated: false,
            body_type: EmailBodyType::Plain,
            value: "short".to_owned(),
        };
        let pref = BodyPreference {
            body_type: 1,
            truncation_size: Some(500),
            ..Default::default()
        };
        let (_, out_value, full_len, truncated) = apply_body_preference(body, Some(pref));
        assert_eq!(out_value, "short");
        assert_eq!(full_len, 5);
        assert!(!truncated);
    }

    #[test]
    fn write_email_fields_outer_order_matches_syncmail_at_protocol_14() {
        // Real bug, found via the full z-push-stalwart-jmap source (PR
        // #187): src/lib/syncobjects/syncmail.php's own $mapping, once
        // its >=2.5/>=12.0/>=14.0 gates and unset() calls are applied at
        // protocol 14.0 (this gateway's now-clamped ceiling), has a
        // final field order of To, Cc, From, Subject, DateReceived,
        // DisplayTo, Importance, Read, MessageClass, [Body],
        // [Attachments], [NativeBodyType]. This gateway had MessageClass
        // and Subject first, DateReceived third, From/To/DisplayTo/Cc
        // all out of order, and Attachments after NativeBodyType instead
        // of before -- almost nothing was in the right slot. Likely
        // explains the session's original "every message shows the same
        // time" symptom: DateReceived was always proven correct on the
        // wire, but a field in the wrong slot can be silently
        // misassigned by iOS's position-sensitive parser rather than
        // hard-rejected (the same failure class already confirmed for
        // Attachment/Preview/Notes).
        let email = Email {
            draft_revision: None,
            id: "email-1".to_owned(),
            mailbox_ids: vec![],
            subject: "Subject".to_owned(),
            received_at: Some("2026-08-24T02:05:00Z".to_owned()),
            keywords: vec![],
            from: "sender@example.com".to_owned(),
            to: "recipient@example.com".to_owned(),
            cc: "cc@example.com".to_owned(),
            bcc: String::new(),
            reply_to: String::new(),
            importance: 1,
            preview: None,
            read: true,
            body: Some(EmailBody {
                is_truncated: false,
                body_type: EmailBodyType::Plain,
                value: "Hello".to_owned(),
            }),
            smime: None,
            attachments: vec![EmailAttachment {
                blob_id: "blob-1".to_owned(),
                name: "file.txt".to_owned(),
                content_type: "text/plain".to_owned(),
                size: 10,
                content_id: None,
                content_location: None,
                inline: false,
            }],
            blob_id: None,
            thread_id: None,
        };
        let mut builder = DocumentBuilder::new();
        write_email_fields(&mut builder, "email-1", email, None, ProtocolVersion::V14_0);
        let doc = builder.finish();

        use crate::wbxml::eas::email as mail;
        let expected_order = [
            (mail::TO.code_page, mail::TO.token),
            (mail::CC.code_page, mail::CC.token),
            (mail::FROM.code_page, mail::FROM.token),
            (mail::SUBJECT.code_page, mail::SUBJECT.token),
            (mail::DATE_RECEIVED.code_page, mail::DATE_RECEIVED.token),
            (mail::DISPLAY_TO.code_page, mail::DISPLAY_TO.token),
            (mail::IMPORTANCE.code_page, mail::IMPORTANCE.token),
            (mail::READ.code_page, mail::READ.token),
            (mail::MESSAGE_CLASS.code_page, mail::MESSAGE_CLASS.token),
            (base::BODY.code_page, base::BODY.token),
            (base::ATTACHMENTS.code_page, base::ATTACHMENTS.token),
            (
                base::NATIVE_BODY_TYPE.code_page,
                base::NATIVE_BODY_TYPE.token,
            ),
        ];
        let actual_order: Vec<_> = doc
            .nodes
            .iter()
            .filter_map(|n| match n {
                Node::Start(t) if expected_order.contains(&(t.code_page, t.token)) => {
                    Some((t.code_page, t.token))
                }
                _ => None,
            })
            .collect();
        assert_eq!(actual_order, expected_order);
    }

    #[test]
    fn eas_conversation_id_is_a_deterministic_fixed_16_bytes() {
        // Redo of a previously-reverted attempt (commit d199d37): the
        // original sent the raw JMAP thread id directly (observed as
        // short as a single ASCII byte) and caused a real device sync
        // error, most likely because real Exchange servers -- and
        // apparently iOS's own validation -- expect a fixed 16-byte
        // GUID-shaped value here, not an arbitrary-length string.
        let id_a = eas_conversation_id("p");
        let id_a_again = eas_conversation_id("p");
        let id_b = eas_conversation_id("q");
        assert_eq!(id_a.len(), 16);
        assert_eq!(
            id_a, id_a_again,
            "same thread id must hash identically every time"
        );
        assert_ne!(id_a, id_b, "different thread ids must not collide");
    }

    #[test]
    fn write_email_fields_emits_conversation_id_after_native_body_type_only_when_thread_id_present()
    {
        use crate::wbxml::eas::email2;

        let email_with_thread = Email {
            draft_revision: None,
            id: "email-1".to_owned(),
            mailbox_ids: vec![],
            subject: "Subject".to_owned(),
            received_at: None,
            keywords: vec![],
            from: String::new(),
            to: String::new(),
            cc: String::new(),
            bcc: String::new(),
            reply_to: String::new(),
            importance: 1,
            preview: None,
            read: false,
            body: Some(EmailBody {
                is_truncated: false,
                body_type: EmailBodyType::Plain,
                value: "Hello".to_owned(),
            }),
            smime: None,
            attachments: vec![],
            blob_id: None,
            thread_id: Some("thread-abc".to_owned()),
        };
        let mut builder = DocumentBuilder::new();
        write_email_fields(
            &mut builder,
            "email-1",
            email_with_thread,
            None,
            ProtocolVersion::V14_0,
        );
        let doc = builder.finish();

        let native_body_type_pos = doc.nodes.iter().position(|n| matches!(
            n,
            Node::Start(t) if t.code_page == base::PAGE && t.token == base::NATIVE_BODY_TYPE.token
        )).expect("NativeBodyType should be present");
        let conversation_id_pos = doc.nodes.iter().position(|n| matches!(
            n,
            Node::Start(t) if t.code_page == email2::PAGE && t.token == email2::CONVERSATION_ID.token
        )).expect("ConversationId should be present when thread_id is set");
        assert!(
            conversation_id_pos > native_body_type_pos,
            "ConversationId (>=14.0 block) must come after NativeBodyType (>=12.0 block), per z-push's own field-order oracle"
        );
        // Immediately followed by the opaque 16-byte payload, not text --
        // ConversationId is opaque binary per spec, not a string.
        match &doc.nodes[conversation_id_pos + 1] {
            Node::Opaque(bytes) => assert_eq!(bytes.len(), 16),
            other => panic!("expected opaque ConversationId payload, got {other:?}"),
        }

        let email_without_thread = Email {
            draft_revision: None,
            id: "email-2".to_owned(),
            mailbox_ids: vec![],
            subject: "Subject".to_owned(),
            received_at: None,
            keywords: vec![],
            from: String::new(),
            to: String::new(),
            cc: String::new(),
            bcc: String::new(),
            reply_to: String::new(),
            importance: 1,
            preview: None,
            read: false,
            body: None,
            smime: None,
            attachments: vec![],
            blob_id: None,
            thread_id: None,
        };
        let mut builder2 = DocumentBuilder::new();
        write_email_fields(
            &mut builder2,
            "email-2",
            email_without_thread,
            None,
            ProtocolVersion::V14_0,
        );
        let doc2 = builder2.finish();
        assert!(
            !doc2.nodes.iter().any(|n| matches!(
                n,
                Node::Start(t) if t.code_page == email2::PAGE && t.token == email2::CONVERSATION_ID.token
            )),
            "ConversationId must be omitted entirely when there's no JMAP threadId, not sent empty"
        );
    }

    #[test]
    fn write_email_fields_preview_is_nested_inside_body_after_data() {
        // Real structural bug, found via the full z-push-stalwart-jmap
        // source (PR #187): AirSyncBase:Preview is a child of Body (in
        // SyncBaseBody's own $mapping, appended after Data), not a
        // sibling of Body under ApplicationData. iOS never hard-errored
        // on the old (wrong) position -- it silently ignored it, which
        // is why previews kept showing raw markup even after the
        // separate "strip HTML tags" fix landed.
        let email = Email {
            draft_revision: None,
            id: "email-1".to_owned(),
            mailbox_ids: vec![],
            subject: "Subject".to_owned(),
            received_at: None,
            keywords: vec![],
            from: String::new(),
            to: String::new(),
            cc: String::new(),
            bcc: String::new(),
            reply_to: String::new(),
            importance: 1,
            preview: None,
            read: false,
            body: Some(EmailBody {
                is_truncated: false,
                body_type: EmailBodyType::Plain,
                value: "Hello world".to_owned(),
            }),
            smime: None,
            attachments: vec![],
            blob_id: None,
            thread_id: None,
        };
        let mut builder = DocumentBuilder::new();
        write_email_fields(&mut builder, "email-1", email, None, ProtocolVersion::V14_0);
        let doc = builder.finish();

        let mut depth = 0i32;
        let mut body_depth = None;
        let mut preview_depth = None;
        let mut data_seen_before_preview = false;
        let mut data_seen = false;
        for node in &doc.nodes {
            match node {
                Node::Start(t) if t.code_page == base::PAGE && t.token == base::BODY.token => {
                    body_depth = Some(depth);
                    depth += 1;
                }
                Node::Start(t) if t.code_page == base::PAGE && t.token == base::DATA.token => {
                    data_seen = true;
                    depth += 1;
                }
                Node::Start(t) if t.code_page == base::PAGE && t.token == base::PREVIEW.token => {
                    preview_depth = Some(depth);
                    data_seen_before_preview = data_seen;
                    depth += 1;
                }
                Node::Start(_) => depth += 1,
                Node::End => depth -= 1,
                _ => {}
            }
        }
        assert_eq!(
            preview_depth,
            body_depth.map(|d| d + 1),
            "Preview must be one level deeper than Body -- i.e. nested inside it"
        );
        assert!(data_seen_before_preview, "Data must come before Preview");
    }

    #[test]
    fn write_email_fields_attachment_field_order_matches_ms_asairs() {
        // Real bug, confirmed live via idevicesyslog on the zoidberg A/B
        // test, in two rounds. Round 1: iOS's WBXML parser is a strict
        // per-position grammar, not a flat tag lookup -- the old field
        // order (DisplayName, FileReference, ContentType,
        // EstimatedDataSize, IsInline -- ContentType right after
        // FileReference, Method never sent at all) desynced iOS's parser
        // mid-Attachment: "No parse rule from object <private> for
        // codePage 0x11 token 0x17 (CPT = 69911)" (0x11=17 is
        // AirSyncBase, 0x17 is ContentType) -- and the WHOLE Sync task
        // failed, discarding every field in the response, not just the
        // attachment. Fixing the ORDER (adding Method, moving ContentType
        // last) did not clear the error, though -- round 2, verified
        // against the working PHP z-push reference's own
        // SyncBaseAttachment construction (config/z-push/jmap.php): it
        // never sets contenttype for a Sync response's attachment list
        // at all (only for the unrelated GetAttachment-download
        // response's own class) -- ContentType simply isn't valid here
        // regardless of position, so this gateway's real Attachment
        // sequence is just DisplayName, FileReference, Method,
        // EstimatedDataSize, IsInline.
        let email = Email {
            draft_revision: None,
            id: "email-1".to_owned(),
            mailbox_ids: vec![],
            subject: "Has an attachment".to_owned(),
            received_at: None,
            keywords: vec![],
            from: String::new(),
            to: String::new(),
            cc: String::new(),
            bcc: String::new(),
            reply_to: String::new(),
            importance: 1,
            preview: None,
            read: false,
            body: None,
            smime: None,
            attachments: vec![EmailAttachment {
                blob_id: "blob-1".to_owned(),
                name: "invoice.pdf".to_owned(),
                content_type: "application/pdf".to_owned(),
                size: 1234,
                content_id: None,
                content_location: None,
                inline: false,
            }],
            blob_id: None,
            thread_id: None,
        };
        let mut builder = DocumentBuilder::new();
        write_email_fields(&mut builder, "email-1", email, None, ProtocolVersion::V14_0);
        let doc = builder.finish();

        // Compare by (code_page, token) only -- Token's derived equality
        // also includes has_content, which DocumentBuilder::start()/leaf()
        // always force to true regardless of how the constant itself was
        // declared, so a direct Token == Token comparison against the
        // eas.rs constants would never match.
        let expected_order = [
            base::DISPLAY_NAME.token,
            base::FILE_REFERENCE.token,
            base::METHOD.token,
            base::ESTIMATED_DATA_SIZE.token,
            base::IS_INLINE.token,
        ];
        let actual_order: Vec<_> = doc
            .nodes
            .iter()
            .filter_map(|n| match n {
                Node::Start(t)
                    if t.code_page == base::PAGE
                        && (expected_order.contains(&t.token)
                            || t.token == base::CONTENT_TYPE.token) =>
                {
                    Some(t.token)
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            actual_order, expected_order,
            "ContentType must not appear at all"
        );
    }

    #[test]
    fn apply_body_preference_html_requested_keeps_html_but_still_truncates() {
        let body = EmailBody {
            is_truncated: false,
            body_type: EmailBodyType::Html,
            value: "<p>".to_owned() + &"x".repeat(1000) + "</p>",
        };
        let pref = BodyPreference {
            body_type: 2,
            truncation_size: Some(10),
            ..Default::default()
        };
        let (out_type, out_value, _, truncated) = apply_body_preference(body, Some(pref));
        assert_eq!(out_type, EmailBodyType::Html);
        assert_eq!(out_value.len(), 10);
        assert!(truncated);
    }
}
