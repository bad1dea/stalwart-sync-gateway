mod item_operations;

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
    model::{EmailBodyType, Note},
    state::{ItemState, SyncRecord},
    wbxml,
};

// Keep negotiation at the tested production version while newer response
// shapes and commands are implemented and checked with independent clients.
const SUPPORTED_PROTOCOLS: &str = "14.0";
// Advertise only commands with handlers.
const SUPPORTED_COMMANDS: &str = "Sync,SendMail,SmartForward,SmartReply,GetAttachment,FolderSync,FolderCreate,FolderDelete,FolderUpdate,MoveItems,GetItemEstimate,Search,Settings,Ping,ItemOperations,Provision";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProtocolVersion {
    V14_0,
    V14_1,
    V16_0,
    V16_1,
}

impl ProtocolVersion {
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

    // [MS-ASCMD] 2.2.1.21: an empty Sync re-executes the device's previous
    // (cached) Sync request; with nothing cached the answer is Status 13.
    // A full Sync is remembered for that purpose.
    let push_user = state.push.user(&auth.username().to_ascii_lowercase());
    let device_key = query.device_id.clone().unwrap_or_default();
    let replayed;
    let body: &[u8] = if command.eq_ignore_ascii_case("Sync") {
        if body.is_empty() {
            match push_user.last_sync_body(&device_key) {
                Some(cached) => {
                    replayed = cached;
                    &replayed
                }
                None => {
                    use wbxml::eas::airsync as air;
                    let mut builder = wbxml::eas::DocumentBuilder::new();
                    builder.start(air::SYNC);
                    builder.leaf(air::STATUS, "13");
                    builder.end();
                    state
                        .metrics
                        .eas_requests_total
                        .with_label_values(&[command, "200"])
                        .inc();
                    return (
                        StatusCode::OK,
                        [("Content-Type", "application/vnd.ms-sync.wbxml")],
                        wbxml::encode_document(&builder.finish()),
                    )
                        .into_response();
                }
            }
        } else {
            push_user.remember_sync_body(&device_key, &body);
            &body
        }
    } else {
        &body
    };

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
                    return sync_mail(&state, &auth, &query, &document, command).await;
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
                    return item_operations::handle(&state, &auth, &document, multipart, command)
                        .await;
                }
                if command.eq_ignore_ascii_case("Search") {
                    return search(&state, &auth, &document, command).await;
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
) -> Vec<String> {
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
                // Be conservative on a probe failure: let the client sync.
                tracing::warn!(%error, collection = %id, "push: change probe failed");
                changed.push(id.clone());
            }
        }
    }
    changed
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
            let mut found = changed_mail_folders(state, auth, &eas_user, &device, &mail_ids).await;
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
/// needs the conversion. A Mailbox search restricted to any
/// `airsync:Class` other than "Email" (Calendar/Contacts/Tasks/Notes/
/// SMS) returns a clean empty-but-successful result for that store
/// rather than erroring the whole command -- those classes have no
/// search backing today, but a device asking for them shouldn't see a
/// broken response, just zero matches.
async fn search(
    state: &AppState,
    auth: &crate::jmap::client::AuthenticatedSession,
    document: &wbxml::Document,
    command: &str,
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

    // (position, limit): default to the first 25 results (matching this
    // gateway's own Sync WindowSize default) when the device omits Range
    // entirely -- the schema marks it optional inside Options.
    let (position, limit) = match request.range {
        Some((start, end)) if end >= start => (start, (end - start + 1).clamp(1, 100) as usize),
        _ => (0, 25),
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
        body_type: Some(request.body_pref_type.unwrap_or(1)),
        truncation_size: Some(request.body_pref_truncation_size.unwrap_or(500)),
        mime_data: None,
    });

    let mut builder = wbxml::eas::DocumentBuilder::new();
    builder.start(srch::SEARCH);
    builder.leaf(srch::STATUS, "1");
    builder.start(srch::RESPONSE);
    builder.start(srch::STORE);

    if request.store_name.eq_ignore_ascii_case("Mailbox") {
        if request
            .class
            .as_deref()
            .is_some_and(|class| !class.eq_ignore_ascii_case("Email"))
        {
            builder.leaf(srch::STATUS, "1");
            builder.leaf(srch::TOTAL, "0");
        } else {
            let text = request.free_text.unwrap_or_default();
            match state.jmap.search_emails(auth, &text, position, limit).await {
                Ok((emails, total)) => {
                    builder.leaf(srch::STATUS, "1");
                    for email in emails {
                        let email_id = email.id.clone();
                        builder.start(srch::RESULT);
                        builder.leaf(air::CLASS, "Email");
                        builder.leaf(srch::LONG_ID, email_id.clone());
                        if let Some(mailbox_id) = email.mailbox_ids.first() {
                            builder.leaf(air::COLLECTION_ID, mailbox_id.clone());
                        }
                        builder.start(srch::PROPERTIES);
                        write_email_fields(&mut builder, &email_id, email, body_pref.clone());
                        builder.end();
                        builder.end();
                    }
                    if let Some((start, end)) = request.range {
                        builder.leaf(srch::RANGE, format!("{start}-{end}"));
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
        match state.jmap.search_principals(auth, &text, limit).await {
            Ok(principals) => {
                builder.leaf(srch::STATUS, "1");
                let total = principals.len();
                for principal in principals {
                    builder.start(srch::RESULT);
                    builder.start(srch::PROPERTIES);
                    builder.leaf(gal::DISPLAY_NAME, principal.display_name);
                    if let Some(email) = principal.email {
                        builder.leaf(gal::EMAIL_ADDRESS, email);
                    }
                    builder.end();
                    builder.end();
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
/// SmartForward/SmartReply both submit a complete, client-composed MIME
/// message exactly like SendMail -- the only EAS-level difference is the
/// `Source` element referencing the original message (used by some
/// servers to mark it Answered/Forwarded), which write_email_fields's
/// caller for Change commands (`apply_mail_client_commands`) already
/// handles via a plain flag update if the client separately sends one;
/// this handler does not attempt to set that flag itself, matching the
/// PHP reference's own scope (it links `replyflag`/`forwardflag` from
/// the same request, which isn't parsed by compose_mail_request here --
/// a reasonable gap given a subsequent Sync will independently mark the
/// original as Answered/Forwarded when clients set those flags).
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

    let (mime, save_in_sent_items) = match wbxml::decode_document(body) {
        Ok(document) => match wbxml::eas::compose_mail_request(&document) {
            Some(request) => (request.mime, request.save_in_sent_items),
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
        ),
    };

    match state.jmap.send_email(auth, mime, save_in_sent_items).await {
        Ok(()) => {
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
            Ok(_) => builder.leaf(set::STATUS, "3"),
            Err(error) => {
                tracing::warn!(%error, "failed to read user mail identities");
                builder.leaf(set::STATUS, "3");
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
    let policy_type = wbxml::eas::find_text_after(document, wbxml::eas::provision::POLICY_TYPE)
        .unwrap_or("MS-EAS-Provisioning-WBXML");
    let mut builder = wbxml::eas::DocumentBuilder::new();
    use wbxml::eas::provision as prov;

    builder.start(prov::PROVISION);
    builder.leaf(prov::STATUS, "1");
    builder.start(prov::POLICIES);
    builder.start(prov::POLICY);
    builder.leaf(prov::POLICY_TYPE, policy_type);
    // Real bug, found via the full z-push-stalwart-jmap source (PR #187):
    // per src/lib/core/zpushdefs.php, Policy-level Status 2 means
    // SYNC_PROVISION_POLICYSTATUS_NOPOLICY (a non-success state) -- 1 is
    // SYNC_PROVISION_POLICYSTATUS_SUCCESS, z-push's own default/working
    // value. This gateway was telling every device its policy request
    // failed while still handing out a PolicyKey, a self-contradictory
    // response. Never observed triggering a live failure (no real
    // device in this session has actually issued a Provision command),
    // but matches the exact same failure class as every other bug found
    // this way.
    builder.leaf(prov::STATUS, "1");
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

fn replay_collection_id(collection_id: &str) -> String {
    format!("__replay__:{collection_id}")
}

async fn store_replay(
    state: &AppState,
    user: &str,
    device: &str,
    collection_id: &str,
    request_key: &str,
    new_key: &str,
    nodes: Vec<wbxml::Node>,
) {
    use base64::Engine;
    let bytes = wbxml::encode_document(&wbxml::Document { nodes });
    let item = ItemState {
        item_id: format!("{request_key}>{new_key}"),
        hash: base64::engine::general_purpose::STANDARD.encode(bytes),
    };
    if let Err(error) = state
        .state
        .put_item_states(
            user,
            device,
            &replay_collection_id(collection_id),
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
) -> Option<(String, String, Vec<wbxml::Node>)> {
    use base64::Engine;
    let items = state
        .state
        .item_states(user, device, &replay_collection_id(collection_id))
        .await
        .ok()?;
    let item = items.into_iter().next()?;
    let (request_key, new_key) = item.item_id.split_once('>')?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(item.hash)
        .ok()?;
    let doc = wbxml::decode_document(&bytes).ok()?;
    Some((request_key.to_owned(), new_key.to_owned(), doc.nodes))
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

    if let Some(device) = query.device_id.as_deref() {
        let push = state.push.user(&auth.username().to_ascii_lowercase());
        for c in &collections {
            push.folder_synced(device, &c.collection_id);
        }
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

    for collection in collections {
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
            if let (Some(record), Some((request_key, new_key, nodes))) = (
                previous_record.as_ref(),
                load_replay(state, user, device_id, &collection.collection_id).await,
            ) {
                if request_key == collection.sync_key && new_key == record.sync_key {
                    tracing::info!(
                        device_id,
                        collection = %collection.collection_id,
                        resent_key = %collection.sync_key,
                        reply_key = %new_key,
                        "Sync: client resent its previous SyncKey, replaying the last reply"
                    );
                    builder.extend(nodes);
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
                sync_calendar_collection(state, auth, &mut builder, user, device_id, &collection)
                    .await;
            } else {
                sync_tasks_collection(state, auth, &mut builder, user, device_id, &collection)
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
                            &collection.sync_key,
                            key,
                            nodes,
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
        let (client_commands_applied, client_read_changes) =
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
        let fetch_responses = if is_mail_collection && !collection.commands.is_empty() {
            resolve_fetch_commands(state, auth, &collection.commands, collection.body_pref_type)
                .await
        } else {
            Vec::new()
        };

        if collection.get_changes && is_mail_collection {
            // 0 = page through the whole mailbox (no fixed cap).
            const SCAN_LIMIT: usize = 0;
            // Stored newest first (records written before that order was kept
            // are in id order; the next Sync rewrites them).
            let previous_order: Vec<String> = previous_record
                .as_ref()
                .map(|record| record.seen_ids.clone())
                .unwrap_or_default();
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
            let to_remove: BTreeSet<String> = if candidate_missing.is_empty() {
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
                .saturating_sub(page.len());
            let aged_sent: Vec<String> = aged_out.iter().take(aged_budget).cloned().collect();
            if aged_out.len() > aged_sent.len() {
                has_more = true;
            }
            let aged_set: BTreeSet<String> = aged_sent.iter().cloned().collect();

            // Read-state sync: tell this device about messages read/unread
            // elsewhere (another device, webmail). `item_states` holds what
            // this device was last told; a message with no stored state (a
            // device that predates this) gets one catch-up Change.
            let stored_read: BTreeMap<String, String> = match state
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
            let mut prev_read = if collection.sync_key == "0" {
                BTreeMap::new()
            } else {
                stored_read.clone()
            };
            // What this device itself just set is already known to it.
            for (id, read) in &client_read_changes {
                prev_read.insert(id.clone(), read_hash(*read).to_owned());
            }
            let mut read_changes: Vec<(String, bool)> = if collection.sync_key == "0" {
                Vec::new()
            } else {
                emails
                    .iter()
                    .filter(|email| previous_seen.contains(&email.id))
                    .filter(|email| {
                        prev_read.get(&email.id).map(String::as_str) != Some(read_hash(email.read))
                    })
                    .map(|email| (email.id.clone(), email.read))
                    .collect()
            };
            // Messages older than this device's Sync window never show up in
            // `emails`, so their read state would stay frozen at whatever the
            // device was last told. Refresh the stored ones directly.
            let mut outside_updates: Vec<(String, bool)> = Vec::new();
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
                    match state.jmap.email_read_states(auth, &outside).await {
                        Ok(current) => {
                            for (id, read) in current {
                                if prev_read.get(&id).map(String::as_str) != Some(read_hash(read)) {
                                    read_changes.push((id.clone(), read));
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
                    read_changes.iter().map(|(id, _)| id.clone()).collect();
                let drift: Vec<(String, bool)> = c
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
                        prev_read.get(id.as_str()).map(String::as_str) != Some(read_hash(*read))
                    })
                    .map(|(id, (read, _))| (id.clone(), *read))
                    .collect();
                if drift.len() > CHANGES_CAP {
                    has_more = true;
                    if let Some(since) = &since_state {
                        new_email_state = since.clone();
                    }
                }
                for (id, read) in drift.into_iter().take(CHANGES_CAP) {
                    read_changes.push((id.clone(), read));
                    outside_updates.push((id, read));
                }
            }
            // No Change for a message removed in this same reply.
            read_changes.retain(|(id, _)| !aged_set.contains(id));
            outside_updates.retain(|(id, _)| !aged_set.contains(id));
            if !read_changes.is_empty() {
                tracing::info!(
                    device_id,
                    collection = %collection.collection_id,
                    read_changes = read_changes.len(),
                    older_than_window = outside_updates.len(),
                    "mail Sync: sending read-state changes"
                );
            }
            let mut new_read = prev_read;
            for (id, read) in &outside_updates {
                new_read.insert(id.clone(), read_hash(*read).to_owned());
            }
            // Nothing is sent on a SyncKey 0 reply, so nothing is recorded.
            if collection.sync_key != "0" {
                for email in &emails {
                    new_read.insert(email.id.clone(), read_hash(email.read).to_owned());
                }
            }
            new_read.retain(|id, _| !to_remove.contains(id.as_str()) && !aged_set.contains(id));
            if new_read != stored_read {
                let items = new_read
                    .iter()
                    .map(|(id, hash)| crate::state::ItemState {
                        item_id: id.clone(),
                        hash: hash.clone(),
                    })
                    .collect();
                if let Err(error) = state
                    .state
                    .put_item_states(user, device_id, &collection.collection_id, items)
                    .await
                {
                    tracing::warn!(%error, "failed to persist mail read states");
                }
            }

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
            let new_sync_key = if collection.sync_key == "0"
                || !emails_to_send.is_empty()
                || !to_remove.is_empty()
                || !aged_sent.is_empty()
                || !read_changes.is_empty()
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
                    // Mail records keep the device's Sync window here so Ping
                    // can judge "new" the same way Sync does.
                    jmap_state: mail_record_state(collection.window_size, filter, &new_email_state),
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

            tracing::info!(
                device_id,
                collection = %collection.collection_id,
                client_key = %collection.sync_key,
                new_key = %new_sync_key,
                adds = emails_to_send.len(),
                deletes = to_remove.len(),
                aged_out = aged_sent.len(),
                read_changes = read_changes.len(),
                more_available = has_more,
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
            write_fetch_responses(&mut builder, fetch_responses);
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
                || !read_changes.is_empty()
            {
                let body_pref = BodyPreference {
                    body_type: collection.body_pref_type,
                    truncation_size: collection.body_pref_truncation_size,
                    // MIME (Type=4) is what a real device asks for when
                    // actually opening a message (see
                    // resolve_fetch_commands's own docs) -- the list-sync
                    // Add path never needs a pre-fetched MIME blob.
                    mime_data: None,
                };
                builder.start(air::COMMANDS);
                for email in emails_to_send {
                    write_email_add(&mut builder, email, Some(body_pref.clone()));
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
                for (id, read) in &read_changes {
                    write_email_read_change(&mut builder, id, *read);
                }
                builder.end();
            }
        } else {
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
            write_fetch_responses(&mut builder, fetch_responses);
        }

        builder.end();
        if let Some((mark, collection_id, request_key, new_key)) = replay_capture.take() {
            let nodes = builder.nodes_since(mark);
            store_replay(
                state,
                user,
                device_id,
                &collection_id,
                &request_key,
                &new_key,
                nodes,
            )
            .await;
        }
    }

    builder.end();
    builder.end();

    let reply = builder.finish();
    if trace_enabled(query.device_id.as_deref()) {
        tracing::info!(
            device_id = query.device_id.as_deref().unwrap_or(""),
            request = %wbxml_outline(document),
            reply = %wbxml_outline(&reply),
            "trace: Sync"
        );
    }
    let body = wbxml::encode_document(&reply);
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
) -> anyhow::Result<(bool, Vec<(String, bool)>)> {
    let mut applied = false;
    let mut read_changes = Vec::new();
    for command in commands {
        match command.kind {
            wbxml::eas::SyncClientCommandKind::Change => {
                if let Some(read) = command.read {
                    state
                        .jmap
                        .set_email_seen(auth, &command.server_id, read)
                        .await?;
                    applied = true;
                    read_changes.push((command.server_id.clone(), read));
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
                tracing::debug!(
                    collection = collection_id,
                    server_id = command.server_id,
                    kind = ?command.kind,
                    "ignoring unsupported mail Sync client command"
                );
            }
            // Handled separately by resolve_fetch_commands/write_fetch_responses
            // -- a Fetch is a request for item details, not a mutation, so it
            // doesn't belong in "did anything change" bookkeeping.
            wbxml::eas::SyncClientCommandKind::Fetch => {}
        }
    }
    Ok((applied, read_changes))
}

/// Opaque per-message read-state token kept in `item_states` for mail.
fn read_hash(read: bool) -> &'static str {
    if read {
        "1"
    } else {
        "0"
    }
}

/// `<Change>` carrying only the Read flag, how a server tells a device that
/// a message was read or unread elsewhere.
fn write_email_read_change(builder: &mut wbxml::eas::DocumentBuilder, id: &str, read: bool) {
    use wbxml::eas::{airsync as air, email as mail};
    builder.start(air::CHANGE);
    builder.leaf(air::SERVER_ID, id.to_owned());
    builder.start(air::APPLICATION_DATA);
    builder.leaf(mail::READ, read_hash(read));
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
    body_pref_type: Option<u8>,
) -> Vec<(
    String,
    anyhow::Result<Option<(crate::model::Email, Option<bytes::Bytes>)>>,
)> {
    let mut results = Vec::new();
    for command in commands {
        if command.kind == wbxml::eas::SyncClientCommandKind::Fetch {
            let result = state.jmap.get_email_by_id(auth, &command.server_id).await;
            let result = match result {
                Ok(Some(email)) => {
                    // Real bug, found via a direct live wire comparison
                    // against z-push for this exact scenario: opening a
                    // message sends its OWN BodyPreference (Type=4/MIME,
                    // MimeSupport=2 -- confirmed live from a real iPad),
                    // completely separate from the list-sync's Type=1
                    // preference. This gateway had no handling for Type=4
                    // at all -- it fell through and sent the original
                    // HTML untouched, no MIME envelope, which is very
                    // likely why the message body showed as literal
                    // "<!DOCTYPE html>..." source text rather than
                    // rendering: iOS expected a MIME-wrapped part and got
                    // raw markup with no MIME headers to make sense of it.
                    // z-push's own selectBody() downloads the email's
                    // raw RFC822 blob directly when MIME is requested
                    // (config/z-push/jmap.php) rather than building one
                    // by hand -- do the same here.
                    let mime = if body_pref_type == Some(4) {
                        match &email.blob_id {
                            Some(blob_id) => {
                                match state.jmap.download_blob(auth, blob_id, "message.eml").await {
                                    Ok(bytes) => Some(bytes::Bytes::from(bytes)),
                                    Err(error) => {
                                        tracing::warn!(
                                            %error,
                                            server_id = command.server_id,
                                            "Sync Fetch: MIME blob download failed, falling back to non-MIME body"
                                        );
                                        None
                                    }
                                }
                            }
                            None => None,
                        }
                    } else {
                        None
                    };
                    Ok(Some((email, mime)))
                }
                Ok(None) => Ok(None),
                Err(error) => Err(error),
            };
            results.push((command.server_id.clone(), result));
        }
    }
    results
}

/// Writes the `<Responses>` block answering any Sync-embedded Fetch
/// commands, reusing the same field writer as a normal Add so the two
/// can't drift.
type FetchResult = (
    String,
    anyhow::Result<Option<(crate::model::Email, Option<bytes::Bytes>)>>,
);

fn write_fetch_responses(builder: &mut wbxml::eas::DocumentBuilder, results: Vec<FetchResult>) {
    use wbxml::eas::airsync as air;

    if results.is_empty() {
        return;
    }
    builder.start(air::RESPONSES);
    for (server_id, result) in results {
        builder.start(air::FETCH);
        builder.leaf(air::SERVER_ID, server_id.clone());
        match result {
            Ok(Some((email, mime))) => {
                builder.leaf(air::STATUS, "1");
                builder.start(air::APPLICATION_DATA);
                let body_pref = mime.map(|mime_data| BodyPreference {
                    body_type: Some(4),
                    truncation_size: None,
                    mime_data: Some(mime_data),
                });
                write_email_fields(builder, &server_id, email, body_pref);
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
                builder.leaf(air::STATUS, "3");
            }
        }
        builder.end();
    }
    builder.end();
}

/// The client's requested body shape for a list-sync Add, from
/// AirSyncBase:BodyPreference on the Sync request's Collection > Options.
/// `None` at the write_email_fields call site (ItemOperations Fetch, and
/// Sync-embedded Fetch) means "always full body" -- correct there, since
/// those are the explicit "user opened this message" fetches, not the
/// list sync.
#[derive(Debug, Clone)]
struct BodyPreference {
    body_type: Option<u8>,
    truncation_size: Option<usize>,
    /// Pre-fetched raw RFC822 MIME source, when body_type == Some(4). Has
    /// to be fetched async (a JMAP blob download) before write_email_fields
    /// runs, since that function isn't async -- see resolve_fetch_commands.
    mime_data: Option<bytes::Bytes>,
}

fn write_email_add(
    builder: &mut wbxml::eas::DocumentBuilder,
    email: crate::model::Email,
    body_pref: Option<BodyPreference>,
) {
    use wbxml::eas::airsync as air;

    let email_id = email.id.clone();
    builder.start(air::ADD);
    builder.leaf(air::SERVER_ID, email.id.clone());
    builder.start(air::APPLICATION_DATA);
    write_email_fields(builder, &email_id, email, body_pref);
    builder.end();
    builder.end();
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
) {
    use wbxml::eas::{airsync_base as base, email as mail, email2};

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
    builder.leaf(mail::IMPORTANCE, "1");
    builder.leaf(mail::READ, if email.read { "1" } else { "0" });
    builder.leaf(mail::MESSAGE_CLASS, "IPM.Note");
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
    let preview_text = email
        .body
        .as_ref()
        .map(plain_text_preview)
        .unwrap_or_default();
    if let Some(mime) = body_pref.as_ref().and_then(|pref| pref.mime_data.clone()) {
        builder.start(base::BODY);
        builder.leaf(base::TYPE, "4");
        builder.leaf(base::ESTIMATED_DATA_SIZE, mime.len().to_string());
        builder.leaf(base::TRUNCATED, "0");
        builder.opaque_leaf(base::DATA, mime);
        builder.leaf(base::PREVIEW, preview_text);
        builder.end();
    } else if let Some(body) = email.body {
        let (out_type, out_value, full_len, truncated) = apply_body_preference(body, body_pref);
        builder.start(base::BODY);
        builder.leaf(base::TYPE, out_type.eas_value());
        builder.leaf(base::ESTIMATED_DATA_SIZE, full_len.to_string());
        builder.leaf(base::TRUNCATED, if truncated { "1" } else { "0" });
        builder.leaf(base::DATA, out_value);
        builder.leaf(base::PREVIEW, preview_text);
        builder.end();
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
            builder.leaf(base::METHOD, "1");
            builder.leaf(base::ESTIMATED_DATA_SIZE, attachment.size.to_string());
            builder.leaf(base::IS_INLINE, "0");
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

/// Shapes a body per the client's BodyPreference (list-sync Add only --
/// see `BodyPreference`'s docs for why Fetch responses always pass
/// `None`/full body instead of calling this).
///
/// Real bug, confirmed live via the zoidberg A/B test: a real iPad's Sync
/// request carried `BodyPreference { Type: 1 (plain), TruncationSize: 500
/// }`, but every build up to this fix ignored it completely and always
/// sent the full HTML body (11KB+, `Truncated: 0`) regardless -- the
/// client silently discarded every such response without ever advancing
/// its SyncKey off "0", so it kept re-fetching the exact same batch from
/// scratch forever (visible in Traefik's access log: the identical
/// response byte-count recurring every few seconds, never converging to
/// the empty steady-state response a settled sync produces).
///
/// Returns (type actually sent, body text actually sent, true full
/// length, whether truncation happened) -- EstimatedDataSize must report
/// the true full length even when Data is cut short, so the client knows
/// there's more to fetch via ItemOperations.
fn apply_body_preference(
    body: crate::model::EmailBody,
    pref: Option<BodyPreference>,
) -> (crate::model::EmailBodyType, String, usize, bool) {
    use crate::model::EmailBodyType;

    let Some(pref) = pref else {
        let full_len = body.value.len();
        return (body.body_type, body.value, full_len, false);
    };

    let (out_type, full_text) = match pref.body_type {
        Some(1) if body.body_type == EmailBodyType::Html => {
            // Collapse whitespace too, not just strip tags -- an HTML
            // source's leading indentation/blank lines (from a <head>,
            // template formatting, etc.) otherwise eats most of a small
            // TruncationSize budget before any real text appears.
            (
                EmailBodyType::Plain,
                collapse_whitespace(&strip_html_tags(&body.value)),
            )
        }
        _ => (body.body_type, body.value),
    };
    let full_len = full_text.len();

    match pref.truncation_size {
        Some(limit) if limit < full_len => {
            // Truncate on a char boundary, not a raw byte offset -- real
            // message bodies routinely carry multi-byte UTF-8.
            let mut end = limit;
            while end > 0 && !full_text.is_char_boundary(end) {
                end -= 1;
            }
            (out_type, full_text[..end].to_owned(), full_len, true)
        }
        _ => (out_type, full_text, full_len, false),
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
                tracing::debug!(
                    server_id = command.server_id,
                    "ignoring unsupported Contacts Fetch command"
                );
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
    contact.last_name.hash(&mut hasher);
    contact.file_as.hash(&mut hasher);
    contact.emails.hash(&mut hasher);
    contact.mobile_phone.hash(&mut hasher);
    contact.home_phone.hash(&mut hasher);
    contact.business_phone.hash(&mut hasher);
    contact.company_name.hash(&mut hasher);
    contact.job_title.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn write_contact_command(
    builder: &mut wbxml::eas::DocumentBuilder,
    tag: wbxml::token::Token,
    contact: &crate::model::Contact,
) {
    use wbxml::eas::{airsync as air, contacts as c};

    // Field order matches the real z-push-stalwart-jmap source (PR #187,
    // pinned in config/z-push/Dockerfile: src/lib/syncobjects/
    // synccontact.php's own $mapping, filtered down to the fields this
    // gateway actually sets): BusinessPhoneNumber, CompanyName,
    // Email1/2/3Address, FileAs, FirstName, HomePhoneNumber, JobTitle,
    // LastName, MobilePhoneNumber. This gateway had FirstName/LastName
    // first and Business/Company/JobTitle last -- completely reversed
    // from the real schema, the same failure class already confirmed
    // for Email/Attachment/Preview/Notes.
    builder.start(tag);
    builder.leaf(air::SERVER_ID, contact.id.clone());
    builder.start(air::APPLICATION_DATA);
    if let Some(business) = &contact.business_phone {
        builder.leaf(c::BUSINESS_PHONE_NUMBER, business.clone());
    }
    if let Some(company) = &contact.company_name {
        builder.leaf(c::COMPANY_NAME, company.clone());
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
    if let Some(home) = &contact.home_phone {
        builder.leaf(c::HOME_PHONE_NUMBER, home.clone());
    }
    if let Some(title) = &contact.job_title {
        builder.leaf(c::JOB_TITLE, title.clone());
    }
    if let Some(last) = &contact.last_name {
        builder.leaf(c::LAST_NAME, last.clone());
    }
    if let Some(mobile) = &contact.mobile_phone {
        builder.leaf(c::MOBILE_PHONE_NUMBER, mobile.clone());
    }
    builder.end();
    builder.end();
}

/// Handles one Calendar collection's Sync round-trip end to end, same
/// contract and hash-diff shape as `sync_contacts_collection` (see that
/// function's own doc for the full pattern description -- this is a
/// direct copy of it onto Calendar). No recurrence, attendee, or
/// reminder support -- single (non-recurring) events only, matching the
/// read path's existing field set. `CalendarEvent/set` supports
/// in-place `update` (confirmed live -- see `save_calendar_event`'s doc),
/// so like Contacts, the JMAP id is a stable ServerId across edits.
async fn sync_calendar_collection(
    state: &AppState,
    auth: &AuthenticatedSession,
    builder: &mut wbxml::eas::DocumentBuilder,
    user: &str,
    device_id: &str,
    collection: &wbxml::eas::SyncCollectionRequest,
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

    for command in &collection.commands {
        match command.kind {
            wbxml::eas::SyncClientCommandKind::Add => {
                match state
                    .jmap
                    .save_calendar_event(auth, &calendar_id, None, &command.calendar)
                    .await
                {
                    Ok(new_id) => {
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
                match state
                    .jmap
                    .save_calendar_event(
                        auth,
                        &calendar_id,
                        Some(&command.server_id),
                        &command.calendar,
                    )
                    .await
                {
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
                            "failed to delete calendar event"
                        );
                    }
                }
            }
            wbxml::eas::SyncClientCommandKind::Fetch => {
                tracing::debug!(
                    server_id = command.server_id,
                    "ignoring unsupported Calendar Fetch command"
                );
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
        tracing::warn!(%error, user, device_id, collection = collection.collection_id, "failed to persist Calendar SyncRecord");
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
        tracing::warn!(%error, user, device_id, collection = collection.collection_id, "failed to persist Calendar item state");
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

    if more_available {
        builder.empty_tag(air::MORE_AVAILABLE);
    }
    if collection.get_changes && has_server_changes {
        builder.start(air::COMMANDS);
        for id in &to_add {
            if let Some((_, event)) = current_by_id.get(id) {
                write_calendar_command(builder, air::ADD, event);
            }
        }
        for id in &to_change {
            if let Some((_, event)) = current_by_id.get(id) {
                write_calendar_command(builder, air::CHANGE, event);
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

/// Same purpose as `contact_hash`/`jmap::notes::note_hash`.
fn calendar_event_hash(event: &crate::model::CalendarEvent) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    event.title.hash(&mut hasher);
    event.location.hash(&mut hasher);
    event.start_utc.hash(&mut hasher);
    event.end_utc.hash(&mut hasher);
    event.all_day.hash(&mut hasher);
    event.organizer_email.hash(&mut hasher);
    event.organizer_name.hash(&mut hasher);
    event.is_organizer.hash(&mut hasher);
    event.attendees.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn write_calendar_command(
    builder: &mut wbxml::eas::DocumentBuilder,
    tag: wbxml::token::Token,
    event: &crate::model::CalendarEvent,
) {
    use wbxml::eas::{airsync as air, calendar as cal};

    // Field order matches the real z-push-stalwart-jmap source (PR #187,
    // pinned in config/z-push/Dockerfile: src/lib/syncobjects/
    // syncappointment.php's own $mapping, filtered to the fields this
    // gateway actually sets): DtStamp, StartTime, Subject, OrganizerName,
    // OrganizerEmail, Location, EndTime, AllDayEvent, MeetingStatus,
    // Attendees (confirmed against a real worked [MS-ASCAL] Sync example,
    // not just the PHP source, for the Organizer/MeetingStatus/Attendees
    // placement specifically -- the PR #187 mapping doesn't cover those
    // three since z-push-stalwart-jmap never implemented them).
    builder.start(tag);
    builder.leaf(air::SERVER_ID, event.id.clone());
    builder.start(air::APPLICATION_DATA);
    builder.leaf(
        cal::DTSTAMP,
        eas_datetime(&chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()),
    );
    if let Some(start) = &event.start_utc {
        builder.leaf(cal::START_TIME, start.clone());
    }
    builder.leaf(cal::SUBJECT, event.title.clone());
    if let Some(name) = &event.organizer_name {
        builder.leaf(cal::ORGANIZER_NAME, name.clone());
    }
    if let Some(email) = &event.organizer_email {
        builder.leaf(cal::ORGANIZER_EMAIL, email.clone());
    }
    if let Some(location) = &event.location {
        builder.leaf(cal::LOCATION, location.clone());
    }
    if let Some(end) = &event.end_utc {
        builder.leaf(cal::END_TIME, end.clone());
    }
    builder.leaf(cal::ALL_DAY_EVENT, if event.all_day { "1" } else { "0" });
    if !event.attendees.is_empty() {
        // [MS-ASCAL] MeetingStatus: 1 = meeting, organizer's own copy;
        // 3 = meeting, received as an attendee. The organizer is NEVER
        // listed as an Attendee (confirmed against the live spec's own
        // worked example) -- OrganizerName/OrganizerEmail above already
        // covers that, so `attendees` here only ever holds everyone else.
        builder.leaf(
            cal::MEETING_STATUS,
            if event.is_organizer { "1" } else { "3" },
        );
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
    }
    builder.end();
    builder.end();
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
) {
    use wbxml::eas::airsync as air;

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
                tracing::debug!(
                    server_id = command.server_id,
                    "ignoring unsupported Tasks Fetch command"
                );
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

    if more_available {
        builder.empty_tag(air::MORE_AVAILABLE);
    }
    if collection.get_changes && has_server_changes {
        builder.start(air::COMMANDS);
        for id in &to_add {
            if let Some((_, task)) = current_by_id.get(id) {
                write_task_command(builder, air::ADD, task);
            }
        }
        for id in &to_change {
            if let Some((_, task)) = current_by_id.get(id) {
                write_task_command(builder, air::CHANGE, task);
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
    task.completed.hash(&mut hasher);
    task.due.hash(&mut hasher);
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
) {
    use wbxml::eas::{airsync as air, tasks};

    builder.start(tag);
    builder.leaf(air::SERVER_ID, task.id.clone());
    builder.start(air::APPLICATION_DATA);
    builder.leaf(tasks::SUBJECT, task.title.clone());
    builder.leaf(tasks::COMPLETE, if task.completed { "1" } else { "0" });
    if let Some(due) = &task.due {
        builder.leaf(tasks::DUE_DATE, due.clone());
        builder.leaf(tasks::UTC_DUE_DATE, due.clone());
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

    let mut deleted = false;
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
                tracing::debug!(
                    server_id = command.server_id,
                    "ignoring unsupported Notes Fetch command"
                );
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
    for id in to_add.iter().chain(&to_change) {
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
    use wbxml::eas::{airsync as air, airsync_base as base, notes};

    // Field order matches the real z-push-stalwart-jmap source (PR #187,
    // pinned in config/z-push/Dockerfile: src/lib/syncobjects/
    // syncnote.php's own $mapping order): Body, Categories,
    // LastModifiedDate, MessageClass, Subject -- this gateway had
    // Subject first and Body second (reversed), and MessageClass before
    // Categories/LastModifiedDate instead of after.
    builder.start(tag);
    builder.leaf(air::SERVER_ID, note.id.clone());
    builder.start(air::APPLICATION_DATA);
    builder.start(base::BODY);
    builder.leaf(base::TYPE, note.body_type.eas_value());
    builder.leaf(base::ESTIMATED_DATA_SIZE, note.body.len().to_string());
    builder.leaf(base::TRUNCATED, "0");
    builder.leaf(base::DATA, note.body.clone());
    builder.end();
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
    builder.end();
    builder.end();
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

/// A `note_`/`ab_`/`cal_`/`task_`-prefixed collection id is a synthetic id
/// this gateway itself invents for Notes/Contacts/Calendar/Tasks (see
/// `jmap::client::collections()`) -- never a real JMAP `Mailbox` id.
/// FolderCreate/FolderUpdate/FolderDelete are mail-only (per the command
/// matrix: no create/rename/delete primitive exists for the
/// heuristically-derived address-book/calendar listings), so any of
/// these prefixes on a ParentId/ServerId is rejected before ever calling
/// JMAP, rather than relying on it to fail incidentally.
fn is_non_mail_collection_id(id: &str) -> bool {
    id.starts_with("note_")
        || id.starts_with("ab_")
        || id.starts_with("cal_")
        || id.starts_with("task_")
}

/// [MS-ASCMD] section 2.2.1.3. Request: SyncKey, ParentId, DisplayName,
/// Type (in that order, verified against section 6.9's XML schema).
/// Response: Status, SyncKey, ServerId -- SyncKey/ServerId are
/// `minOccurs="0"` in the schema; this always includes SyncKey (echoed
/// back unchanged -- this gateway doesn't maintain incremental
/// folder-hierarchy sync state beyond FolderSync's own always-full-relist
/// scheme, so there's nothing to advance here) and ServerId only on
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
        .unwrap_or(25);
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
            .is_some_and(|stored| stored != read_hash(*read))
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
        BodyPreference,
    };
    use crate::model::{Email, EmailAttachment, EmailBody, EmailBodyType};
    use crate::wbxml::eas::{airsync_base as base, DocumentBuilder};
    use crate::wbxml::Node;

    #[test]
    fn eas_datetime_strips_dashes_and_colons() {
        assert_eq!(eas_datetime("2026-08-24T02:05:00Z"), "20260824T020500Z");
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
    fn apply_body_preference_none_sends_full_body_untouched() {
        let body = EmailBody {
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
    fn apply_body_preference_real_bug_ipad_plain_500_truncates_html_to_plain_text() {
        // The exact request shape confirmed live: a real iPad's Sync
        // Options carried BodyPreference{Type: 1, TruncationSize: 500}.
        // Sending the full untruncated HTML back (what every build before
        // this fix did) is what caused the device to never advance its
        // SyncKey off "0".
        let long_text = "word ".repeat(200); // 1000 chars, well over 500
        let body = EmailBody {
            body_type: EmailBodyType::Html,
            value: format!("<p>{long_text}</p>"),
        };
        let pref = BodyPreference {
            body_type: Some(1),
            truncation_size: Some(500),
            mime_data: None,
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
            body_type: EmailBodyType::Html,
            value: "<html>\n<head>\n    \n    \n</head>\n<body>\n\n\nHello world\n</body>\n</html>"
                .to_owned(),
        };
        let pref = BodyPreference {
            body_type: Some(1),
            truncation_size: Some(500),
            mime_data: None,
        };
        let (_, out_value, _, truncated) = apply_body_preference(body, Some(pref));
        assert_eq!(out_value, "Hello world");
        assert!(!truncated);
    }

    #[test]
    fn apply_body_preference_short_body_under_truncation_size_is_not_truncated() {
        let body = EmailBody {
            body_type: EmailBodyType::Plain,
            value: "short".to_owned(),
        };
        let pref = BodyPreference {
            body_type: Some(1),
            truncation_size: Some(500),
            mime_data: None,
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
            id: "email-1".to_owned(),
            mailbox_ids: vec![],
            subject: "Subject".to_owned(),
            received_at: Some("2026-08-24T02:05:00Z".to_owned()),
            keywords: vec![],
            from: "sender@example.com".to_owned(),
            to: "recipient@example.com".to_owned(),
            cc: "cc@example.com".to_owned(),
            read: true,
            body: Some(EmailBody {
                body_type: EmailBodyType::Plain,
                value: "Hello".to_owned(),
            }),
            attachments: vec![EmailAttachment {
                blob_id: "blob-1".to_owned(),
                name: "file.txt".to_owned(),
                content_type: "text/plain".to_owned(),
                size: 10,
            }],
            blob_id: None,
            thread_id: None,
        };
        let mut builder = DocumentBuilder::new();
        write_email_fields(&mut builder, "email-1", email, None);
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
            id: "email-1".to_owned(),
            mailbox_ids: vec![],
            subject: "Subject".to_owned(),
            received_at: None,
            keywords: vec![],
            from: String::new(),
            to: String::new(),
            cc: String::new(),
            read: false,
            body: Some(EmailBody {
                body_type: EmailBodyType::Plain,
                value: "Hello".to_owned(),
            }),
            attachments: vec![],
            blob_id: None,
            thread_id: Some("thread-abc".to_owned()),
        };
        let mut builder = DocumentBuilder::new();
        write_email_fields(&mut builder, "email-1", email_with_thread, None);
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
            id: "email-2".to_owned(),
            mailbox_ids: vec![],
            subject: "Subject".to_owned(),
            received_at: None,
            keywords: vec![],
            from: String::new(),
            to: String::new(),
            cc: String::new(),
            read: false,
            body: None,
            attachments: vec![],
            blob_id: None,
            thread_id: None,
        };
        let mut builder2 = DocumentBuilder::new();
        write_email_fields(&mut builder2, "email-2", email_without_thread, None);
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
            id: "email-1".to_owned(),
            mailbox_ids: vec![],
            subject: "Subject".to_owned(),
            received_at: None,
            keywords: vec![],
            from: String::new(),
            to: String::new(),
            cc: String::new(),
            read: false,
            body: Some(EmailBody {
                body_type: EmailBodyType::Plain,
                value: "Hello world".to_owned(),
            }),
            attachments: vec![],
            blob_id: None,
            thread_id: None,
        };
        let mut builder = DocumentBuilder::new();
        write_email_fields(&mut builder, "email-1", email, None);
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
            id: "email-1".to_owned(),
            mailbox_ids: vec![],
            subject: "Has an attachment".to_owned(),
            received_at: None,
            keywords: vec![],
            from: String::new(),
            to: String::new(),
            cc: String::new(),
            read: false,
            body: None,
            attachments: vec![EmailAttachment {
                blob_id: "blob-1".to_owned(),
                name: "invoice.pdf".to_owned(),
                content_type: "application/pdf".to_owned(),
                size: 1234,
            }],
            blob_id: None,
            thread_id: None,
        };
        let mut builder = DocumentBuilder::new();
        write_email_fields(&mut builder, "email-1", email, None);
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
            body_type: EmailBodyType::Html,
            value: "<p>".to_owned() + &"x".repeat(1000) + "</p>",
        };
        let pref = BodyPreference {
            body_type: Some(2),
            truncation_size: Some(10),
            mime_data: None,
        };
        let (out_type, out_value, _, truncated) = apply_body_preference(body, Some(pref));
        assert_eq!(out_type, EmailBodyType::Html);
        assert_eq!(out_value.len(), 10);
        assert!(truncated);
    }
}
