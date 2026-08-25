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

    if command.eq_ignore_ascii_case("SendMail")
        || command.eq_ignore_ascii_case("SmartForward")
        || command.eq_ignore_ascii_case("SmartReply")
    {
        return send_mail(&state, &auth, &query, &body, command).await;
    }

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
                if command.eq_ignore_ascii_case("ItemOperations") {
                    return item_operations(&state, &auth, &document, command).await;
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

/// Handles `ItemOperations > Fetch` addressed the "Mailbox Store" way
/// (Store/CollectionId/ServerId) -- what iOS sends when a user opens a
/// message from a prior Sync result. This command was entirely
/// unimplemented (fell through to the generic 501 in `post_handler`)
/// until now, which iOS reads as "keep waiting" rather than an error --
/// confirmed live as an infinite spinner with no body shown when opening
/// any message, even though the full body was already delivered inline
/// in the original Sync response.
async fn item_operations(
    state: &AppState,
    auth: &crate::jmap::client::AuthenticatedSession,
    document: &wbxml::Document,
    command: &str,
) -> Response {
    use wbxml::eas::{airsync as air, item_operations as ops};

    let Some(fetch) = wbxml::eas::item_operations_fetch(document) else {
        state
            .metrics
            .eas_requests_total
            .with_label_values(&[command, "400"])
            .inc();
        return (
            StatusCode::BAD_REQUEST,
            "ItemOperations Fetch missing Store/CollectionId/ServerId\n",
        )
            .into_response();
    };

    let mut builder = wbxml::eas::DocumentBuilder::new();
    builder.start(ops::ITEM_OPERATIONS);
    builder.leaf(ops::STATUS, "1");
    builder.start(ops::RESPONSE);
    builder.start(ops::FETCH);

    if !fetch.store.eq_ignore_ascii_case("Mailbox") {
        // Only mail items are fetchable this way today (no Notes/Contacts/
        // Calendar item fetch, no DocumentLibrary store). Fail this one
        // Fetch cleanly rather than hang or 400 the whole request.
        builder.leaf(ops::STATUS, "2");
        builder.end();
        builder.end();
        builder.end();
    } else {
        match state.jmap.get_email_by_id(auth, &fetch.server_id).await {
            Ok(Some(email)) => {
                builder.leaf(ops::STATUS, "1");
                builder.leaf(air::COLLECTION_ID, fetch.collection_id);
                builder.leaf(air::SERVER_ID, fetch.server_id.clone());
                builder.start(ops::PROPERTIES);
                write_email_fields(&mut builder, &fetch.server_id, email, None);
                builder.end();
                builder.end();
                builder.end();
                builder.end();
            }
            Ok(None) => {
                tracing::warn!(
                    server_id = fetch.server_id.as_str(),
                    "ItemOperations Fetch: email not found"
                );
                builder.leaf(ops::STATUS, "8");
                builder.end();
                builder.end();
                builder.end();
            }
            Err(error) => {
                tracing::warn!(%error, server_id = fetch.server_id.as_str(), "ItemOperations Fetch failed");
                builder.leaf(ops::STATUS, "3");
                builder.end();
                builder.end();
                builder.end();
            }
        }
    }

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

async fn settings(
    state: &AppState,
    auth: &crate::jmap::client::AuthenticatedSession,
    document: &wbxml::Document,
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
        // Real bug, confirmed live via idevicesyslog on the zoidberg A/B
        // test: "No parse rule from object <private> for codePage 0x12
        // token 0x23" -- 0x12=18 is Settings, 0x23 is
        // set::PRIMARY_SMTP_ADDRESS. iOS's parser desynced on this
        // unrecognized field and every subsequent field in the SAME
        // response misparsed too (the immediately-following "We have an
        // int in our WBXML, but Exchange never gives us this" is the same
        // cascade, not a separate bug -- the same failure class as the
        // Attachment field-order bug: one bad field corrupts the whole
        // response). The working PHP z-push reference (config/z-push/
        // *.php in the homelab repo) doesn't implement EmailAddresses/
        // PrimarySmtpAddress at all, so rather than guess at where this
        // field actually belongs in MS-ASSETTINGS's real schema, just
        // stop sending it -- EmailAddresses > SMTPAddress below already
        // conveys the same information in a schema-valid way.
        builder.start(set::USER_INFORMATION);
        builder.leaf(set::STATUS, "1");
        builder.start(set::GET);
        builder.start(set::EMAIL_ADDRESSES);
        builder.leaf(set::SMTP_ADDRESS, auth.username());
        builder.end();
        builder.end();
        builder.end();
    }

    if has_oof {
        // Automatic Replies (Out-of-Office). No Oof/Set handling here was
        // ever implemented -- a Get with no <Oof> section in the response
        // left iOS's Automatic Replies screen waiting on a shape it never
        // got, spinning forever (confirmed live). Get now answers honestly
        // (state always reports disabled, since nothing is wired to a real
        // backend). Set is accepted (Status 1, no hang/error) rather than
        // silently doing nothing AND erroring -- but it does not persist:
        // toggling it on will read back as off on the next Get. Wiring
        // this to Stalwart's ManageSieve vacation extension is the real
        // fix; this stub only stops the client-side hang.
        builder.start(set::OOF);
        builder.leaf(set::STATUS, "1");
        if !is_oof_set {
            builder.start(set::GET);
            builder.leaf(set::OOF_STATE, "0");
            builder.start(set::OOF_MESSAGE);
            builder.start(set::APPLIES_TO_INTERNAL);
            builder.end();
            builder.leaf(set::ENABLED, "0");
            builder.leaf(set::REPLY_MESSAGE, "");
            builder.leaf(set::BODY_TYPE, "Text");
            builder.end();
            builder.end();
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

        if collection.collection_id.starts_with("ab_") {
            sync_contacts_collection(
                state,
                auth,
                &mut builder,
                user,
                device_id,
                &collection,
                previous_record.as_ref(),
            )
            .await;
            continue;
        }

        if collection.collection_id.starts_with("cal_") {
            sync_calendar_collection(
                state,
                auth,
                &mut builder,
                user,
                device_id,
                &collection,
                previous_record.as_ref(),
            )
            .await;
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
        let fetch_responses = if is_mail_collection && !collection.commands.is_empty() {
            resolve_fetch_commands(state, auth, &collection.commands).await
        } else {
            Vec::new()
        };

        if collection.get_changes && is_mail_collection {
            let (emails, has_more) = match state
                .jmap
                .emails_in_mailbox(auth, &collection.collection_id, collection.window_size)
                .await
            {
                Ok(result) => result,
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
            if collection.sync_key == "0" && has_more {
                builder.start(air::MORE_AVAILABLE);
                builder.end();
            }

            if !emails_to_send.is_empty() {
                let body_pref = BodyPreference {
                    body_type: collection.body_pref_type,
                    truncation_size: collection.body_pref_truncation_size,
                };
                builder.start(air::COMMANDS);
                for email in emails_to_send {
                    write_email_add(&mut builder, email, Some(body_pref));
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
            write_fetch_responses(&mut builder, fetch_responses);
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
    Ok(applied)
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
) -> Vec<(String, anyhow::Result<Option<crate::model::Email>>)> {
    let mut results = Vec::new();
    for command in commands {
        if command.kind == wbxml::eas::SyncClientCommandKind::Fetch {
            let result = state.jmap.get_email_by_id(auth, &command.server_id).await;
            results.push((command.server_id.clone(), result));
        }
    }
    results
}

/// Writes the `<Responses>` block answering any Sync-embedded Fetch
/// commands, reusing the same field writer as a normal Add so the two
/// can't drift.
fn write_fetch_responses(
    builder: &mut wbxml::eas::DocumentBuilder,
    results: Vec<(String, anyhow::Result<Option<crate::model::Email>>)>,
) {
    use wbxml::eas::airsync as air;

    if results.is_empty() {
        return;
    }
    builder.start(air::RESPONSES);
    for (server_id, result) in results {
        builder.start(air::FETCH);
        builder.leaf(air::SERVER_ID, server_id.clone());
        match result {
            Ok(Some(email)) => {
                builder.leaf(air::STATUS, "1");
                builder.start(air::APPLICATION_DATA);
                write_email_fields(builder, &server_id, email, None);
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
#[derive(Debug, Clone, Copy)]
struct BodyPreference {
    body_type: Option<u8>,
    truncation_size: Option<usize>,
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
    use wbxml::eas::{airsync_base as base, email as mail};

    builder.leaf(mail::MESSAGE_CLASS, "IPM.Note");
    builder.leaf(mail::SUBJECT, email.subject);
    if let Some(received_at) = email.received_at {
        // JMAP receivedAt is ISO 8601 ("2026-08-24T02:05:00Z"); MS-ASEMAIL
        // DateReceived requires the compact EAS form (no dashes/colons).
        // This was sent raw for a long time -- iOS silently mis-renders (or
        // ignores) an ISO-formatted DateReceived, which showed up live as
        // "timestamps are off" on every synced message.
        let formatted = eas_datetime(&received_at);
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
    if let Some(body) = email.body {
        // The list view's snippet line -- a real, dedicated field for this
        // (AirSyncBase:Preview), separate from Body. Never sending one
        // meant iOS had nothing to summarize with and fell back to
        // showing raw markup from Body's Data verbatim as the preview
        // line -- confirmed live (every message's preview was literal
        // "<html xmlns:v=..." source, not rendered/extracted text).
        builder.leaf(base::PREVIEW, plain_text_preview(&body));
        let native_type = body.body_type;
        let (out_type, out_value, full_len, truncated) = apply_body_preference(body, body_pref);
        builder.start(base::BODY);
        builder.leaf(base::TYPE, out_type.eas_value());
        // Full untruncated size, even when Data below is cut short -- this
        // is how the client knows there's more to fetch via ItemOperations.
        builder.leaf(base::ESTIMATED_DATA_SIZE, full_len.to_string());
        builder.leaf(base::TRUNCATED, if truncated { "1" } else { "0" });
        builder.leaf(base::DATA, out_value);
        builder.end();
        builder.leaf(base::NATIVE_BODY_TYPE, native_type.eas_value());
    }
    if !email.attachments.is_empty() {
        builder.start(base::ATTACHMENTS);
        for attachment in email.attachments {
            // Real bug, confirmed live via idevicesyslog on the zoidberg
            // A/B test: iOS's WBXML parser is a strict per-position
            // grammar, not a flat tag lookup -- sending ContentType
            // right after FileReference (skipping the required Method
            // field, per MS-ASAIRS's Attachment element sequence:
            // DisplayName, FileReference, Method, EstimatedDataSize,
            // IsInline, [NativeBodyType], [ContentType]) desynced its
            // parser state entirely: `No parse rule from object <private>
            // for codePage 0x11 token 0x17 (CPT = 69911)` -- 0x11=17 is
            // AirSyncBase, 0x17 is ContentType -- and the WHOLE Sync task
            // failed (`ASFolderItemsSyncTask ... failed with status: 1`),
            // discarding every field in the response, mail included, not
            // just the broken attachment. This is why SyncKey never
            // durably advanced even after the BodyPreference/
            // MoreAvailable fixes landed correctly in isolation: any
            // batch containing an attachment (this mailbox has several)
            // tripped this on every attempt.
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
            builder.leaf(base::CONTENT_TYPE, attachment.content_type);
            builder.end();
        }
        builder.end();
    }
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
            (EmailBodyType::Plain, collapse_whitespace(&strip_html_tags(&body.value)))
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
    collapse_whitespace(&text).chars().take(PREVIEW_CHARS).collect()
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
        if ch == '<' {
            if starts_with_ci(&chars[i + 1..], "style") {
                skip_until = Some("</style>");
            } else if starts_with_ci(&chars[i + 1..], "script") {
                skip_until = Some("</script>");
            }
            in_tag = true;
        } else if ch == '>' {
            in_tag = false;
        } else if !in_tag {
            out.push(ch);
        }
        i += 1;
    }
    out
}

/// Handles one Contacts collection's Sync round-trip: read-only list-and-
/// diff against previously-seen JMAP ids (the same shape mail's own sync
/// uses, `SyncRecord.seen_ids`), against a real JSContact address book.
/// No client-side Add/Change/Delete support yet -- contacts have no real
/// two-way sync (see CLAUDE.md's gap list) -- but this replaces the
/// previous empty stub (which only ever handled the sync_key handshake
/// and never surfaced any contact) with real content. Writes one
/// complete `<Collection>...</Collection>` block into `builder` itself,
/// matching sync_notes_collection's contract.
async fn sync_contacts_collection(
    state: &AppState,
    auth: &AuthenticatedSession,
    builder: &mut wbxml::eas::DocumentBuilder,
    user: &str,
    device_id: &str,
    collection: &wbxml::eas::SyncCollectionRequest,
    previous_record: Option<&SyncRecord>,
) {
    use wbxml::eas::airsync as air;

    let address_book_id = collection
        .collection_id
        .strip_prefix("ab_")
        .unwrap_or(&collection.collection_id);

    if !collection.get_changes {
        // First-sync handshake (or a poll with nothing to check) still
        // has to advance sync_key 0->1 and persist, or the client retries
        // in a tight loop forever -- the same class of bug the contacts/
        // calendar stub had before it advanced on sync_key "0" alone.
        let is_first_sync = collection.sync_key == "0";
        let new_sync_key = if is_first_sync {
            next_sync_key(&collection.sync_key)
        } else {
            collection.sync_key.clone()
        };
        if is_first_sync {
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
                tracing::warn!(
                    %error,
                    user,
                    device_id,
                    collection = collection.collection_id,
                    "failed to persist Sync state"
                );
                builder.start(air::COLLECTION);
                builder.leaf(air::SYNC_KEY, collection.sync_key.clone());
                builder.leaf(air::COLLECTION_ID, collection.collection_id.clone());
                builder.leaf(air::STATUS, "5");
                builder.end();
                return;
            }
        }
        builder.start(air::COLLECTION);
        builder.leaf(air::SYNC_KEY, new_sync_key);
        builder.leaf(air::COLLECTION_ID, collection.collection_id.clone());
        builder.leaf(air::STATUS, "1");
        builder.end();
        return;
    }

    let contacts = match state
        .jmap
        .contacts_in_address_book(auth, address_book_id, collection.window_size)
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

    let previous_seen: BTreeSet<String> = previous_record
        .map(|record| record.seen_ids.iter().cloned().collect())
        .unwrap_or_default();
    let fetched_ids: BTreeSet<String> = contacts.iter().map(|contact| contact.id.clone()).collect();
    let contacts_to_send: Vec<_> = if collection.sync_key == "0" {
        contacts
    } else {
        contacts
            .into_iter()
            .filter(|contact| !previous_seen.contains(&contact.id))
            .collect()
    };
    let new_sync_key = if collection.sync_key == "0" || !contacts_to_send.is_empty() {
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
    if !contacts_to_send.is_empty() {
        builder.start(air::COMMANDS);
        for contact in contacts_to_send {
            write_contact_add(builder, contact);
        }
        builder.end();
    }
    builder.end();
}

fn write_contact_add(builder: &mut wbxml::eas::DocumentBuilder, contact: crate::model::Contact) {
    use wbxml::eas::{airsync as air, contacts as c};

    builder.start(air::ADD);
    builder.leaf(air::SERVER_ID, contact.id);
    builder.start(air::APPLICATION_DATA);
    if let Some(first) = contact.first_name {
        builder.leaf(c::FIRST_NAME, first);
    }
    if let Some(last) = contact.last_name {
        builder.leaf(c::LAST_NAME, last);
    }
    if let Some(file_as) = contact.file_as {
        builder.leaf(c::FILE_AS, file_as);
    }
    let mut emails = contact.emails.into_iter();
    if let Some(email1) = emails.next() {
        builder.leaf(c::EMAIL1_ADDRESS, email1);
    }
    if let Some(email2) = emails.next() {
        builder.leaf(c::EMAIL2_ADDRESS, email2);
    }
    if let Some(email3) = emails.next() {
        builder.leaf(c::EMAIL3_ADDRESS, email3);
    }
    if let Some(mobile) = contact.mobile_phone {
        builder.leaf(c::MOBILE_PHONE_NUMBER, mobile);
    }
    if let Some(home) = contact.home_phone {
        builder.leaf(c::HOME_PHONE_NUMBER, home);
    }
    if let Some(business) = contact.business_phone {
        builder.leaf(c::BUSINESS_PHONE_NUMBER, business);
    }
    if let Some(company) = contact.company_name {
        builder.leaf(c::COMPANY_NAME, company);
    }
    if let Some(title) = contact.job_title {
        builder.leaf(c::JOB_TITLE, title);
    }
    builder.end();
    builder.end();
}

/// Handles one Calendar collection's Sync round-trip: read-only list-and-
/// diff against previously-seen JMAP ids, same shape as
/// sync_contacts_collection. No recurrence, attendee, or reminder
/// support -- single (non-recurring) events only, matching the fields
/// the PHP z-push reference itself fetched (`id, title, start, duration,
/// updated`). Start/end times are converted from JSCalendar local-time-
/// plus-IANA-timezone to real UTC instants in the JMAP client
/// (`local_to_utc_eas`) -- see that function's docs for why this needed
/// a new dependency rather than a naive/wrong conversion.
async fn sync_calendar_collection(
    state: &AppState,
    auth: &AuthenticatedSession,
    builder: &mut wbxml::eas::DocumentBuilder,
    user: &str,
    device_id: &str,
    collection: &wbxml::eas::SyncCollectionRequest,
    previous_record: Option<&SyncRecord>,
) {
    use wbxml::eas::airsync as air;

    let calendar_id = collection
        .collection_id
        .strip_prefix("cal_")
        .unwrap_or(&collection.collection_id);

    if !collection.get_changes {
        let is_first_sync = collection.sync_key == "0";
        let new_sync_key = if is_first_sync {
            next_sync_key(&collection.sync_key)
        } else {
            collection.sync_key.clone()
        };
        if is_first_sync {
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
                tracing::warn!(
                    %error,
                    user,
                    device_id,
                    collection = collection.collection_id,
                    "failed to persist Sync state"
                );
                builder.start(air::COLLECTION);
                builder.leaf(air::SYNC_KEY, collection.sync_key.clone());
                builder.leaf(air::COLLECTION_ID, collection.collection_id.clone());
                builder.leaf(air::STATUS, "5");
                builder.end();
                return;
            }
        }
        builder.start(air::COLLECTION);
        builder.leaf(air::SYNC_KEY, new_sync_key);
        builder.leaf(air::COLLECTION_ID, collection.collection_id.clone());
        builder.leaf(air::STATUS, "1");
        builder.end();
        return;
    }

    let events = match state
        .jmap
        .calendar_events_in_calendar(auth, calendar_id, collection.window_size)
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

    let previous_seen: BTreeSet<String> = previous_record
        .map(|record| record.seen_ids.iter().cloned().collect())
        .unwrap_or_default();
    let fetched_ids: BTreeSet<String> = events.iter().map(|event| event.id.clone()).collect();
    let events_to_send: Vec<_> = if collection.sync_key == "0" {
        events
    } else {
        events
            .into_iter()
            .filter(|event| !previous_seen.contains(&event.id))
            .collect()
    };
    let new_sync_key = if collection.sync_key == "0" || !events_to_send.is_empty() {
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
    if !events_to_send.is_empty() {
        builder.start(air::COMMANDS);
        for event in events_to_send {
            write_calendar_add(builder, event);
        }
        builder.end();
    }
    builder.end();
}

fn write_calendar_add(
    builder: &mut wbxml::eas::DocumentBuilder,
    event: crate::model::CalendarEvent,
) {
    use wbxml::eas::{airsync as air, calendar as cal};

    builder.start(air::ADD);
    builder.leaf(air::SERVER_ID, event.id);
    builder.start(air::APPLICATION_DATA);
    builder.leaf(cal::SUBJECT, event.title);
    if let Some(start) = event.start_utc {
        builder.leaf(cal::START_TIME, start);
    }
    if let Some(end) = event.end_utc {
        builder.leaf(cal::END_TIME, end);
    }
    if let Some(location) = event.location {
        builder.leaf(cal::LOCATION, location);
    }
    builder.leaf(cal::ALL_DAY_EVENT, if event.all_day { "1" } else { "0" });
    builder.leaf(
        cal::DTSTAMP,
        eas_datetime(&chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()),
    );
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

#[cfg(test)]
mod tests {
    use super::{apply_body_preference, eas_datetime, plain_text_preview, write_email_fields, BodyPreference};
    use crate::model::{Email, EmailAttachment, EmailBody, EmailBodyType};
    use crate::wbxml::eas::{airsync_base as base, DocumentBuilder};
    use crate::wbxml::Node;

    #[test]
    fn eas_datetime_strips_dashes_and_colons() {
        assert_eq!(eas_datetime("2026-08-24T02:05:00Z"), "20260824T020500Z");
    }

    #[test]
    fn eas_datetime_drops_fractional_seconds() {
        assert_eq!(eas_datetime("2026-08-24T02:05:00.123Z"), "20260824T020500Z");
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
            value: "<html>\n<head>\n    \n    \n</head>\n<body>\n\n\nHello world\n</body>\n</html>".to_owned(),
        };
        let pref = BodyPreference {
            body_type: Some(1),
            truncation_size: Some(500),
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
        };
        let (_, out_value, full_len, truncated) = apply_body_preference(body, Some(pref));
        assert_eq!(out_value, "short");
        assert_eq!(full_len, 5);
        assert!(!truncated);
    }

    #[test]
    fn write_email_fields_attachment_field_order_matches_ms_asairs() {
        // Real bug, confirmed live via idevicesyslog on the zoidberg A/B
        // test: iOS's WBXML parser is a strict per-position grammar, not
        // a flat tag lookup. The old field order (DisplayName,
        // FileReference, ContentType, EstimatedDataSize, IsInline --
        // ContentType right after FileReference, Method never sent at
        // all) desynced iOS's parser mid-Attachment: "No parse rule from
        // object <private> for codePage 0x11 token 0x17 (CPT = 69911)"
        // (0x11=17 is AirSyncBase, 0x17 is ContentType) -- and the WHOLE
        // Sync task failed, discarding every field in the response, not
        // just the attachment. MS-ASAIRS's real Attachment sequence is
        // DisplayName, FileReference, Method, EstimatedDataSize,
        // IsInline, ContentType.
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
            base::CONTENT_TYPE.token,
        ];
        let actual_order: Vec<_> = doc
            .nodes
            .iter()
            .filter_map(|n| match n {
                Node::Start(t) if t.code_page == base::PAGE && expected_order.contains(&t.token) => {
                    Some(t.token)
                }
                _ => None,
            })
            .collect();
        assert_eq!(actual_order, expected_order);
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
        };
        let (out_type, out_value, _, truncated) = apply_body_preference(body, Some(pref));
        assert_eq!(out_type, EmailBodyType::Html);
        assert_eq!(out_value.len(), 10);
        assert!(truncated);
    }
}
