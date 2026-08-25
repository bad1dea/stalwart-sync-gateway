use axum::{
    body::Bytes,
    extract::{Query, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::hash::{Hash, Hasher};
use tokio::time::{sleep, Duration};

use crate::{
    http_server::AppState,
    jmap::client::{basic_credentials, AuthenticatedSession},
    model::{EmailBodyType, Note},
    state::{ItemState, SyncRecord},
    wbxml,
};

// Real bug, root-caused via the full z-push-stalwart-jmap source (PR #187,
// pinned in config/z-push/Dockerfile) -- verified against
// src/lib/syncobjects/*.php: nearly every WBXML class (SyncMail,
// SyncContact, SyncAppointment, SyncUserInformation, SyncBaseAttachment,
// ...) conditionally changes its field set -- sometimes its ENTIRE shape,
// e.g. SyncUserInformation's EmailAddresses moves from a direct child of
// Get to nested inside Accounts>Account>EmailAddresses at >=14.1 -- based
// on `Request::GetProtocolVersion()`, which iOS derives from the HIGHEST
// version this server advertises in MS-ASProtocolVersions. This gateway
// implements none of that version-conditional branching anywhere; it
// always sends the <=14.0 shape regardless of what it claims to support.
// Advertising up to 16.1 (as this did) let iOS negotiate a protocol
// version whose schema this gateway doesn't actually speak, which is the
// root cause behind the whole family of "No parse rule ..." / "We have
// an int in our WBXML" errors seen live (Attachment's ContentType field
// is itself gated to >=16.0 in the real source -- confirmed the same
// day, independently, via idevicesyslog, before this root cause was
// found). Matching z-push's own advertised ceiling exactly (confirmed
// against a live z-push OPTIONS response: "12.0,12.1,14.0") makes iOS
// negotiate the SAME protocol version z-push does, so every class
// collapses onto the single shape this gateway actually implements.
const SUPPORTED_PROTOCOLS: &str = "12.0,12.1,14.0";
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
            let text_body = wbxml::eas::find_text_after(document, set::REPLY_MESSAGE)
                .map(str::to_owned);
            let from_date = wbxml::eas::find_text_after(document, set::START_TIME)
                .map(str::to_owned);
            let to_date =
                wbxml::eas::find_text_after(document, set::END_TIME).map(str::to_owned);

            let update = crate::jmap::vacation::VacationResponseUpdate {
                is_enabled,
                subject,
                text_body,
                from_date,
                to_date,
            };
            match state.jmap.set_vacation_response(auth, update).await {
                Ok(()) => {
                    builder.start(set::OOF);
                    builder.leaf(set::STATUS, "1");
                    builder.end();
                }
                Err(error) => {
                    tracing::warn!(%error, "failed to persist Oof Set via VacationResponse/set");
                    builder.start(set::OOF);
                    builder.leaf(set::STATUS, "1");
                    builder.end();
                }
            }
        } else {
            let vacation = state.jmap.get_vacation_response(auth).await.ok().flatten();
            let is_enabled = vacation.as_ref().is_some_and(|v| v.is_enabled);

            // EXPERIMENTAL, not yet confirmed: the device's own Set uses
            // OofState=2 when it includes a StartTime/EndTime schedule,
            // and this gateway mirrored that back on Get (2 when a
            // schedule exists, matching the device's own convention: see
            // commit 56527b0). That mirrored response is now confirmed
            // byte-for-byte correct on every dimension checkable against
            // a primary source (schema, date format, device's own wire
            // encoding, z-push's actual encoder) -- yet the user reports
            // Automatic Replies still shows OFF specifically and only
            // when a schedule is set (the no-schedule/OofState=1 case
            // reportedly displays correctly). With every other avenue
            // exhausted, testing the simplest explanation for a
            // value-specific display bug: a client-side check written as
            // `oofState == 1` instead of `oofState != 0`, which would
            // fail silently for the scheduled case specifically. Reports
            // OofState=1 unconditionally whenever enabled (dropping the
            // 2-for-scheduled distinction) while still including the
            // real StartTime/EndTime so the schedule itself isn't lost.
            // If this doesn't fix the display, revert to mirroring 2 --
            // that version was schema-correct, this one is a guess.
            let oof_state_value = if is_enabled { "1" } else { "0" };
            builder.start(set::OOF);
            builder.leaf(set::STATUS, "1");
            builder.start(set::GET);
            builder.leaf(set::OOF_STATE, oof_state_value);
            if is_enabled {
                if let Some(vacation) = &vacation {
                    // Real bug, found live: pulled the zoidberg pcap again
                    // after the user reported Automatic Replies STILL
                    // stuck on "Loading..." even with the nested
                    // OofMessage fix deployed and structurally correct.
                    // The device's own Set request sends StartTime/EndTime
                    // in DASHED ISO form ("2026-08-25T16:14:32.000Z"), but
                    // this Get response was echoing the COMPACT form
                    // ("20260825T161432Z") -- the exact same "wrong date
                    // format on one specific field" failure class as the
                    // original DateReceived bug (commit 1bac5ae). Fixed by
                    // mirroring what the device itself sent, same
                    // reasoning as that fix: eas_datetime_dashes(), not
                    // eas_datetime().
                    if let Some(from) = &vacation.from_date {
                        builder.leaf(set::START_TIME, eas_datetime_dashes(from));
                    }
                    if let Some(to) = &vacation.to_date {
                        builder.leaf(set::END_TIME, eas_datetime_dashes(to));
                    }
                    let reply_message = vacation
                        .text_body
                        .clone()
                        .or_else(|| vacation.subject.clone())
                        .unwrap_or_default();
                    // Real bug, found live via idevicesyslog (not just the
                    // pcap): even though this exact nested shape is
                    // byte-for-byte well-formed WBXML (verified by a full
                    // manual structural walk, balanced start/end, no
                    // leftover bytes) and matches what the device's OWN
                    // Set request encodes, exchangesyncd logged a real
                    // parse error receiving it back: "We have an int in
                    // our WBXML, but Exchange never gives us this. Parse
                    // error." -- iOS's WBXML *decoder* doesn't accept
                    // OofMessage nested inside OofMessage, even though its
                    // own *encoder* produces exactly that shape. Lenient
                    // on write, strict on read -- an asymmetry, not a
                    // contradiction. Three SIBLING OofMessage blocks
                    // instead (closed before the next starts) -- this is
                    // the shape an earlier, since-reverted fix this
                    // session used (commit ee7f2b8), but that revert
                    // (folded into commit c6beadf) was specifically
                    // because it wrongly applied 3x blocks to the
                    // DISABLED case too, proven wrong via z-push
                    // comparison; nobody had tried siblings-only-when-
                    // enabled until now.
                    builder.start(set::OOF_MESSAGE);
                    builder.empty_tag(set::APPLIES_TO_INTERNAL);
                    builder.leaf(set::ENABLED, "1");
                    builder.leaf(set::REPLY_MESSAGE, reply_message.clone());
                    builder.leaf(set::BODY_TYPE, "Text");
                    builder.end();
                    builder.start(set::OOF_MESSAGE);
                    builder.empty_tag(set::APPLIES_TO_EXTERNAL_KNOWN);
                    builder.leaf(set::ENABLED, "1");
                    builder.leaf(set::REPLY_MESSAGE, reply_message.clone());
                    builder.leaf(set::BODY_TYPE, "Text");
                    builder.end();
                    builder.start(set::OOF_MESSAGE);
                    builder.empty_tag(set::APPLIES_TO_EXTERNAL_UNKNOWN);
                    builder.leaf(set::ENABLED, "1");
                    builder.leaf(set::REPLY_MESSAGE, reply_message);
                    builder.leaf(set::BODY_TYPE, "Text");
                    builder.end();
                }
            }
            builder.end();
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
        // Real gap, found via the full z-push-stalwart-jmap source (PR
        // #187): src/lib/request/getitemestimate.php's Folder response
        // writes FolderType before FolderId; this gateway never sent
        // FolderType at all. The token was already defined
        // (get_item_estimate::FOLDER_TYPE) but unused.
        builder.leaf(gie::FOLDER_TYPE, "Email");
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
            sync_contacts_collection(state, auth, &mut builder, user, device_id, &collection)
                .await;
            continue;
        }

        if collection.collection_id.starts_with("cal_") {
            sync_calendar_collection(state, auth, &mut builder, user, device_id, &collection)
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
            resolve_fetch_commands(state, auth, &collection.commands, collection.body_pref_type)
                .await
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
            let candidate_missing: Vec<String> = if collection.sync_key == "0" {
                Vec::new()
            } else {
                previous_seen
                    .difference(&fetched_ids)
                    .cloned()
                    .collect()
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
                || !to_remove.is_empty()
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
                    seen_ids: previous_seen
                        .union(&fetched_ids)
                        .filter(|id| !to_remove.contains(id.as_str()))
                        .cloned()
                        .collect(),
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

            if !emails_to_send.is_empty() || !to_remove.is_empty() {
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
    body_pref_type: Option<u8>,
) -> Vec<(String, anyhow::Result<Option<(crate::model::Email, Option<String>)>>)> {
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
                                match state
                                    .jmap
                                    .download_blob(auth, blob_id, "message.eml")
                                    .await
                                {
                                    Ok(bytes) => Some(String::from_utf8_lossy(&bytes).into_owned()),
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
fn write_fetch_responses(
    builder: &mut wbxml::eas::DocumentBuilder,
    results: Vec<(String, anyhow::Result<Option<(crate::model::Email, Option<String>)>>)>,
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
    mime_data: Option<String>,
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
    let mut native_body_type = None;
    if let Some(body) = email.body {
        // Real structural bug, found via the full z-push-stalwart-jmap
        // source (PR #187): AirSyncBase:Preview is a CHILD of Body (in
        // SyncBaseBody's own $mapping, appended after Data), not a
        // sibling of Body under ApplicationData/Email. This gateway had
        // it as a sibling, written BEFORE Body even opened. iOS never
        // hard-errored on it (unlike the Attachment/Settings bugs) --
        // it silently ignored a Preview in the wrong position instead,
        // which is why every message's list-row preview was still
        // showing raw HTML/markup even after the "strip tags" fix
        // landed: the correctly-stripped Preview text was being sent to
        // a schema position iOS's parser doesn't associate with the
        // list-view snippet at all.
        let preview_text = plain_text_preview(&body);
        native_body_type = Some(body.body_type);
        // Real bug, found via a direct live wire comparison against
        // z-push for a real message-open: the request's own
        // BodyPreference asked for Type=4 (MIME), not plain/HTML, and
        // this gateway had no MIME path at all -- see
        // resolve_fetch_commands's own docs for the full story. A MIME
        // reply bypasses apply_body_preference's HTML/plain conversion
        // entirely: the pre-fetched raw RFC822 source IS the Data,
        // Type is the literal "4", verbatim (not truncated -- z-push's
        // own MIME path doesn't truncate it either).
        if let Some(mime) = body_pref.as_ref().and_then(|p| p.mime_data.clone()) {
            builder.start(base::BODY);
            builder.leaf(base::TYPE, "4");
            builder.leaf(base::ESTIMATED_DATA_SIZE, mime.len().to_string());
            builder.leaf(base::TRUNCATED, "0");
            builder.leaf(base::DATA, mime);
            builder.leaf(base::PREVIEW, preview_text);
            builder.end();
        } else {
            let (out_type, out_value, full_len, truncated) =
                apply_body_preference(body, body_pref);
            builder.start(base::BODY);
            builder.leaf(base::TYPE, out_type.eas_value());
            // Full untruncated size, even when Data below is cut short --
            // this is how the client knows there's more to fetch via
            // ItemOperations.
            builder.leaf(base::ESTIMATED_DATA_SIZE, full_len.to_string());
            builder.leaf(base::TRUNCATED, if truncated { "1" } else { "0" });
            builder.leaf(base::DATA, out_value);
            builder.leaf(base::PREVIEW, preview_text);
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
    if let Some(digits) = entity.strip_prefix("#x").or_else(|| entity.strip_prefix("#X")) {
        return u32::from_str_radix(digits, 16).ok().and_then(char::from_u32);
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

/// Handles one Contacts collection's Sync round-trip end to end: applies
/// client Add/Change/Delete via `ContactCard/set` (real in-place update --
/// confirmed live, see `save_contact`'s own doc -- so unlike Notes, the
/// JMAP id itself is a stable ServerId with no keyword-based workaround
/// needed), then diffs current JMAP state against last-known per-item
/// hashes to find what to push back as `<Commands>`, mirroring
/// `sync_notes_collection`'s contract exactly (one complete
/// `<Collection>...</Collection>` block written directly into `builder`).
/// Upgraded from the old seen-id-only read path (which could never detect
/// a contact edited some other way, e.g. via webmail, since it only ever
/// tracked "have I sent this id before") to the same hash-based
/// add/change/remove diff Notes already uses.
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
        .contacts_in_address_book(auth, &address_book_id, collection.window_size)
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
        .filter_map(|(_, server_id, _)| server_id.clone())
        .chain(
            change_responses
                .iter()
                .map(|(server_id, _)| server_id.clone()),
        )
        .collect();

    let mut to_add = Vec::new();
    let mut to_change = Vec::new();
    for (id, (hash, _)) in &current_by_id {
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
        tracing::warn!(%error, user, device_id, collection = collection.collection_id, "failed to persist Contacts SyncRecord");
    }
    let item_states: Vec<ItemState> = current_by_id
        .iter()
        .map(|(id, (hash, _))| ItemState {
            item_id: id.clone(),
            hash: hash.clone(),
        })
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
        .calendar_events_in_calendar(auth, &calendar_id, collection.window_size)
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
        .filter_map(|(_, server_id, _)| server_id.clone())
        .chain(
            change_responses
                .iter()
                .map(|(server_id, _)| server_id.clone()),
        )
        .collect();

    let mut to_add = Vec::new();
    let mut to_change = Vec::new();
    for (id, (hash, _)) in &current_by_id {
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
        tracing::warn!(%error, user, device_id, collection = collection.collection_id, "failed to persist Calendar SyncRecord");
    }
    let item_states: Vec<ItemState> = current_by_id
        .iter()
        .map(|(id, (hash, _))| ItemState {
            item_id: id.clone(),
            hash: hash.clone(),
        })
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
    // gateway actually sets): DtStamp, StartTime, Subject, Location,
    // EndTime, AllDayEvent. This gateway had Subject first and DtStamp
    // last -- reversed -- same failure class as every other object
    // checked against this source.
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
    if let Some(location) = &event.location {
        builder.leaf(cal::LOCATION, location.clone());
    }
    if let Some(end) = &event.end_utc {
        builder.leaf(cal::END_TIME, end.clone());
    }
    builder.leaf(cal::ALL_DAY_EVENT, if event.all_day { "1" } else { "0" });
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

/// A `note_`/`ab_`/`cal_`-prefixed collection id is a synthetic id this
/// gateway itself invents for Notes/Contacts/Calendar (see
/// `jmap::client::collections()`) -- never a real JMAP `Mailbox` id.
/// FolderCreate/FolderUpdate/FolderDelete are mail-only (per the command
/// matrix: no create/rename/delete primitive exists for the
/// heuristically-derived address-book/calendar listings), so any of
/// these prefixes on a ParentId/ServerId is rejected before ever calling
/// JMAP, rather than relying on it to fail incidentally.
fn is_non_mail_collection_id(id: &str) -> bool {
    id.starts_with("note_") || id.starts_with("ab_") || id.starts_with("cal_")
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

    if is_non_mail_collection_id(server_id) || (parent_id != "0" && is_non_mail_collection_id(parent_id))
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
        assert_eq!(decode_entity_at(&"& rest".chars().collect::<Vec<_>>()), None);
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
            value: "<html>\n<head>\n    \n    \n</head>\n<body>\n\n\nHello world\n</body>\n</html>".to_owned(),
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
            (base::NATIVE_BODY_TYPE.code_page, base::NATIVE_BODY_TYPE.token),
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
        assert_eq!(id_a, id_a_again, "same thread id must hash identically every time");
        assert_ne!(id_a, id_b, "different thread ids must not collide");
    }

    #[test]
    fn write_email_fields_emits_conversation_id_after_native_body_type_only_when_thread_id_present() {
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
                        && (expected_order.contains(&t.token) || t.token == base::CONTENT_TYPE.token) =>
                {
                    Some(t.token)
                }
                _ => None,
            })
            .collect();
        assert_eq!(actual_order, expected_order, "ContentType must not appear at all");
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
