//! End-to-end push tests: the real gateway router over real HTTP against a
//! mock Stalwart (JMAP session + API + EventSource). No production access.

use std::{
    convert::Infallible,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use axum::{
    body::{Body, Bytes},
    extract::State,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use tokio::sync::broadcast;
use tokio_stream::{wrappers::BroadcastStream, StreamExt};

use crate::{
    config::{Config, StateBackend},
    http_server::{build_router, AppState},
    metrics::Metrics,
    push::PushHub,
    state::{self, SyncRecord},
    wbxml::{self, eas},
};

const MAIL_CAP: &str = "urn:ietf:params:jmap:mail";

/// RFC 3339 UTC time `days` days ago.
fn days_ago(days: i64) -> String {
    (chrono::Utc::now() - chrono::Duration::days(days))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

// ---------------------------------------------------------------- mock

struct Mock {
    port: u16,
    boxes: Mutex<std::collections::BTreeMap<String, Vec<String>>>,
    es_tx: broadcast::Sender<String>,
    es_conns: AtomicUsize,
    counter: AtomicUsize,
    keywords: Mutex<std::collections::BTreeMap<String, std::collections::BTreeSet<String>>>,
    extra_folders: Mutex<Vec<String>>,
    /// Ids still returned by Email/query after they left the mailbox
    /// (simulates the listing trailing a move).
    stale_listing: Mutex<Vec<(String, String)>>,
    hidden_folders: Mutex<Vec<String>>,
    /// receivedAt per id (RFC 3339); unset ids use a fixed recent date.
    dates: Mutex<std::collections::BTreeMap<String, String>>,
    /// Every mutation's message id, in order; the JMAP Email state is its
    /// length (for Email/changes).
    change_log: Mutex<Vec<String>>,
    groupware: Mutex<std::collections::BTreeMap<String, Vec<serde_json::Value>>>,
    snapshot_changes: AtomicBool,
    delay_mail_set: AtomicBool,
    delayed_mail_sets: AtomicUsize,
    snapshot_missing_item: AtomicBool,
    identity_error: AtomicBool,
    item_set_error: AtomicBool,
    drop_flag_keywords: AtomicBool,
    download_error: AtomicBool,
    mail_structures: Mutex<std::collections::BTreeMap<String, serde_json::Value>>,
    mail_bodies: Mutex<std::collections::BTreeMap<String, (String, bool)>>,
    body_get_limits: Mutex<Vec<(String, usize)>>,
    body_get_error: AtomicBool,
    body_get_incomplete: AtomicBool,
    identities: Mutex<Vec<serde_json::Value>>,
}

impl Mock {
    fn log_change(&self, id: &str) {
        self.change_log.lock().unwrap().push(id.to_owned());
    }

    fn set_date(&self, id: &str, received_at: &str) {
        self.dates
            .lock()
            .unwrap()
            .insert(id.to_owned(), received_at.to_owned());
    }

    fn date_of(&self, id: &str) -> String {
        self.dates
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            // Default: recent enough for every FilterType.
            .unwrap_or_else(|| days_ago(0))
    }

    fn add_mail(&self, mailbox: &str, id: &str) {
        self.dates
            .lock()
            .unwrap()
            .entry(id.to_owned())
            .or_insert_with(|| days_ago(0));
        self.log_change(id);
        self.boxes
            .lock()
            .unwrap()
            .entry(mailbox.to_owned())
            .or_default()
            .push(id.to_owned());
    }

    fn set_read(&self, id: &str, read: bool) {
        self.set_keyword(id, "$seen", read);
    }

    fn set_keyword(&self, id: &str, keyword: &str, enabled: bool) {
        self.log_change(id);
        let mut all = self.keywords.lock().unwrap();
        let keywords = all.entry(id.to_owned()).or_default();
        if enabled {
            keywords.insert(keyword.to_owned());
        } else {
            keywords.remove(keyword);
        }
    }

    fn remove_mail(&self, mailbox: &str, id: &str) {
        self.log_change(id);
        if let Some(v) = self.boxes.lock().unwrap().get_mut(mailbox) {
            v.retain(|x| x != id);
        }
    }

    fn emit(&self, data: String) {
        let _ = self.es_tx.send(format!("event: state\ndata: {data}\n\n"));
    }

    /// A fresh Email+Mailbox state change (new state strings each call).
    fn emit_email_change(&self) {
        let n = self.counter.fetch_add(1, Ordering::SeqCst);
        self.emit(format!(
            r#"{{"@type":"StateChange","changed":{{"a1":{{"Email":"e{n}","Mailbox":"m{n}","Thread":"t{n}"}}}}}}"#
        ));
    }

    fn close_streams(&self) {
        let _ = self.es_tx.send("CLOSE".to_owned());
    }

    fn connections(&self) -> usize {
        self.es_conns.load(Ordering::SeqCst)
    }
}

async fn mock_session(State(m): State<Arc<Mock>>) -> impl IntoResponse {
    let base = format!("http://127.0.0.1:{}", m.port);
    let mut session = serde_json::json!({
        "capabilities": { "urn:ietf:params:jmap:core": {}, MAIL_CAP: {} },
        "accounts": { "a1": { "name": "u@test", "isPersonal": true, "isReadOnly": false,
                              "accountCapabilities": { MAIL_CAP: {} } } },
        "primaryAccounts": { MAIL_CAP: "a1" },
        "username": "u@test",
        "apiUrl": format!("{base}/api"),
        "downloadUrl": format!("{base}/dl/{{accountId}}/{{blobId}}/{{name}}"),
        "uploadUrl": format!("{base}/ul/{{accountId}}/"),
        "eventSourceUrl": format!("{base}/es/?types={{types}}&closeafter={{closeafter}}&ping={{ping}}"),
        "state": "s"
    });
    if !m.groupware.lock().unwrap().is_empty() {
        for cap in [
            crate::jmap::capabilities::CONTACTS,
            crate::jmap::capabilities::CALENDARS,
        ] {
            session["capabilities"][cap] = serde_json::json!({});
            session["accounts"]["a1"]["accountCapabilities"][cap] = serde_json::json!({});
            session["primaryAccounts"][cap] = serde_json::json!("a1");
        }
    }
    Json(session)
}

fn email_json(
    id: &str,
    mailbox: &str,
    keywords: &std::collections::BTreeSet<String>,
    received_at: &str,
) -> serde_json::Value {
    serde_json::json!({
        "id": id, "blobId": format!("b-{id}"), "threadId": format!("t-{id}"),
        "mailboxIds": { mailbox: true },
        "keywords": keywords.iter().map(|keyword| (keyword.clone(), true)).collect::<std::collections::BTreeMap<_, _>>(),
        "receivedAt": received_at, "subject": format!("subject {id}"),
        "from": [{"name": "A", "email": "a@x.test"}], "to": null, "cc": null,
        "textBody": [{"partId": "1"}], "htmlBody": [],
        "bodyValues": { "1": { "value": "body" } }, "attachments": []
    })
}

fn matches_mail_filter(m: &Mock, id: &str, filter: &serde_json::Value) -> bool {
    if let Some(operator) = filter["operator"].as_str() {
        let conditions = filter["conditions"].as_array().unwrap();
        return match operator {
            "AND" => conditions
                .iter()
                .all(|value| matches_mail_filter(m, id, value)),
            "OR" => conditions
                .iter()
                .any(|value| matches_mail_filter(m, id, value)),
            "NOT" => !conditions
                .iter()
                .any(|value| matches_mail_filter(m, id, value)),
            _ => panic!("unknown JMAP operator"),
        };
    }
    filter
        .as_object()
        .unwrap()
        .iter()
        .all(|(key, value)| match key.as_str() {
            "inMailbox" => m
                .boxes
                .lock()
                .unwrap()
                .get(value.as_str().unwrap())
                .is_some_and(|ids| ids.iter().any(|item| item == id)),
            "text" | "subject" => format!("subject {id}").contains(value.as_str().unwrap()),
            "body" => "body".contains(value.as_str().unwrap()),
            "from" => "a@x.test".contains(value.as_str().unwrap()),
            "to" | "cc" | "bcc" => false,
            "hasAttachment" => value == false,
            "hasKeyword" => m
                .keywords
                .lock()
                .unwrap()
                .get(id)
                .is_some_and(|keywords| keywords.contains(value.as_str().unwrap())),
            "notKeyword" => !m
                .keywords
                .lock()
                .unwrap()
                .get(id)
                .is_some_and(|keywords| keywords.contains(value.as_str().unwrap())),
            "after" => m.date_of(id).as_str() >= value.as_str().unwrap(),
            "before" => m.date_of(id).as_str() < value.as_str().unwrap(),
            _ => panic!("unexpected native JMAP filter field: {key}"),
        })
}

async fn mock_api(
    State(m): State<Arc<Mock>>,
    Json(req): Json<serde_json::Value>,
) -> impl IntoResponse {
    let boxes = m.boxes.lock().unwrap().clone();
    let keyword_map = m.keywords.lock().unwrap().clone();
    let mut responses = Vec::new();
    let mut last_query: Vec<String> = Vec::new();
    let mut last_changes: Vec<String> = Vec::new();
    for call in req["methodCalls"].as_array().unwrap() {
        let name = call[0].as_str().unwrap();
        let args = &call[1];
        let cid = call[2].clone();
        let body = match name {
            "Identity/get" => {
                if m.identity_error.load(Ordering::SeqCst) {
                    responses.push(serde_json::json!(["error", {"type": "serverFail"}, cid]));
                    continue;
                }
                serde_json::json!({"accountId": "a1", "state": "i1", "list": *m.identities.lock().unwrap()})
            }
            "Mailbox/get" => {
                let hidden = m.hidden_folders.lock().unwrap().clone();
                let mut list: Vec<serde_json::Value> = vec![
                    serde_json::json!({"id":"inbox","name":"Inbox","parentId":null,"role":"inbox"}),
                    serde_json::json!({"id":"other","name":"Archive","parentId":null,"role":null}),
                ]
                .into_iter()
                .filter(|f| !hidden.iter().any(|h| f["id"] == h.as_str()))
                .collect();
                for f in m.extra_folders.lock().unwrap().iter() {
                    list.push(serde_json::json!({"id":f,"name": if f == "notes" {"Notes"} else {f},"parentId":null,"role": if f == "trash" {Some("trash")} else {None} }));
                }
                serde_json::json!({ "list": list })
            }
            "Email/query" => {
                let mb = args["filter"]["inMailbox"].as_str().unwrap_or("inbox");
                let mut ids = boxes.get(mb).cloned().unwrap_or_default();
                for (smb, sid) in m.stale_listing.lock().unwrap().iter() {
                    if smb == mb && !ids.contains(sid) {
                        ids.push(sid.clone());
                    }
                }
                if let Some(keyword) = args["filter"]["hasKeyword"].as_str() {
                    ids = boxes
                        .get("notes")
                        .into_iter()
                        .flatten()
                        .filter(|id| format!("noteid-{id}") == keyword)
                        .cloned()
                        .collect();
                }
                if args["filter"]["operator"].is_string() {
                    ids = boxes
                        .values()
                        .flatten()
                        .cloned()
                        .collect::<std::collections::BTreeSet<_>>()
                        .into_iter()
                        .collect();
                    ids.retain(|id| matches_mail_filter(&m, id, &args["filter"]));
                }
                ids.reverse();
                if let Some(after) = args["filter"]["after"].as_str() {
                    // RFC 3339 UTC strings of one format compare as dates.
                    ids.retain(|id| m.date_of(id).as_str() >= after);
                }
                let limit = args["limit"].as_u64().unwrap_or(100) as usize;
                let total = ids.len();
                let position = args["position"].as_u64().unwrap_or(0) as usize;
                ids = ids.into_iter().skip(position).take(limit).collect();
                last_query = ids.clone();
                serde_json::json!({ "ids": ids, "total": total, "queryState": m.change_log.lock().unwrap().len().to_string() })
            }
            "AddressBook/get" => {
                serde_json::json!({"list": [{"id": "book", "name": "Contacts", "isDefault": true}]})
            }
            "Calendar/get" => {
                serde_json::json!({"state":"stable", "list": [{"id": "calendar", "name": "Calendar", "isDefault": true}]})
            }
            "ContactCard/query" | "CalendarEvent/query" => {
                let kind = if name == "ContactCard/query" {
                    "contacts"
                } else {
                    "calendar"
                };
                let items = m.groupware.lock().unwrap();
                let items = items.get(kind).cloned().unwrap_or_default();
                let position = args["position"].as_u64().unwrap_or(0) as usize;
                // Deliberately return short pages, as a server may do.
                let limit = (args["limit"].as_u64().unwrap_or(200) as usize).min(37);
                last_query = items
                    .iter()
                    .skip(position)
                    .take(limit)
                    .map(|v| v["id"].as_str().unwrap().to_owned())
                    .collect();
                serde_json::json!({"ids": last_query, "total": items.len(),
                    "queryState": if m.snapshot_changes.load(Ordering::SeqCst) && position > 0 {"changed"} else {"stable"}})
            }
            "ContactCard/get" | "CalendarEvent/get" => {
                let kind = if name == "ContactCard/get" {
                    "contacts"
                } else {
                    "calendar"
                };
                let items = m.groupware.lock().unwrap();
                let mut list: Vec<_> = items
                    .get(kind)
                    .into_iter()
                    .flatten()
                    .filter(|v| last_query.iter().any(|id| v["id"] == id.as_str()))
                    .cloned()
                    .collect();
                if m.snapshot_missing_item.load(Ordering::SeqCst) {
                    list.pop();
                }
                serde_json::json!({"state":"stable", "list": list})
            }
            "Email/set" => {
                if m.delay_mail_set.load(Ordering::SeqCst) {
                    m.delayed_mail_sets.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
                if m.item_set_error.load(Ordering::SeqCst) {
                    let not_updated = args["update"]
                        .as_object()
                        .into_iter()
                        .flat_map(|items| items.keys())
                        .map(|id| (id.clone(), serde_json::json!({"type":"forbidden"})))
                        .collect::<serde_json::Map<_, _>>();
                    let not_destroyed = args["destroy"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|id| id.as_str())
                        .map(|id| (id.to_owned(), serde_json::json!({"type":"forbidden"})))
                        .collect::<serde_json::Map<_, _>>();
                    responses.push(
                        serde_json::json!(["Email/set", {"updated":{}, "destroyed":[],
                        "notUpdated":not_updated,"notDestroyed":not_destroyed}, cid]),
                    );
                    continue;
                }
                let mut updated = serde_json::Map::new();
                if let Some(updates) = args["update"].as_object() {
                    for (id, patch) in updates {
                        m.log_change(id);
                        let mut boxes = m.boxes.lock().unwrap();
                        for (key, value) in patch.as_object().unwrap() {
                            if let Some(keyword) = key.strip_prefix("keywords/") {
                                let keyword = keyword.replace("~1", "/").replace("~0", "~");
                                let mut all = m.keywords.lock().unwrap();
                                let keywords = all.entry(id.to_owned()).or_default();
                                if value == true {
                                    if !m.drop_flag_keywords.load(Ordering::SeqCst)
                                        || !keyword.starts_with(crate::model::FLAG_KEYWORD_PREFIX)
                                    {
                                        keywords.insert(keyword);
                                    }
                                } else {
                                    keywords.remove(&keyword);
                                }
                            }
                            if let Some(folder) = key.strip_prefix("mailboxIds/") {
                                let items = boxes.entry(folder.to_owned()).or_default();
                                items.retain(|item| item != id);
                                if value == true {
                                    items.push(id.clone());
                                }
                            }
                        }
                        updated.insert(id.clone(), serde_json::Value::Null);
                    }
                }
                let destroyed = args["destroy"].as_array().cloned().unwrap_or_default();
                for id in &destroyed {
                    let id = id.as_str().unwrap();
                    m.log_change(id);
                    for items in m.boxes.lock().unwrap().values_mut() {
                        items.retain(|item| item != id);
                    }
                }
                m.emit_email_change();
                serde_json::json!({"updated": updated, "destroyed": destroyed})
            }
            "Email/changes" => {
                let log = m.change_log.lock().unwrap().clone();
                let since: usize = args["sinceState"]
                    .as_str()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(usize::MAX);
                if since > log.len() {
                    responses.push(
                        serde_json::json!(["error", {"type": "cannotCalculateChanges"}, cid]),
                    );
                    continue;
                }
                let mut updated = Vec::new();
                let mut destroyed = Vec::new();
                for id in &log[since..] {
                    let exists = boxes.values().any(|v| v.contains(id));
                    let list = if exists { &mut updated } else { &mut destroyed };
                    if !list.contains(id) {
                        list.push(id.clone());
                    }
                }
                last_changes = updated.clone();
                serde_json::json!({
                    "oldState": since.to_string(), "newState": log.len().to_string(),
                    "hasMoreChanges": false, "created": [], "updated": updated, "destroyed": destroyed
                })
            }
            "Email/get" => {
                if args["maxBodyValueBytes"] == 0 && m.body_get_error.load(Ordering::SeqCst) {
                    responses.push(serde_json::json!(["error", {"type":"serverFail"}, cid]));
                    continue;
                }
                let ids: Vec<String> = if let Some(arr) = args["ids"].as_array() {
                    arr.iter().map(|v| v.as_str().unwrap().to_owned()).collect()
                } else if args["#ids"]["resultOf"] == "c" {
                    if args["#ids"]["path"] == "/updated" {
                        last_changes.clone()
                    } else {
                        Vec::new()
                    }
                } else {
                    last_query.clone()
                };
                let mut list = Vec::new();
                for id in ids {
                    for (mb, items) in &boxes {
                        if items.contains(&id) {
                            let mut email = email_json(
                                &id,
                                mb,
                                keyword_map.get(&id).unwrap_or(&Default::default()),
                                &m.date_of(&id),
                            );
                            email["mailboxIds"] = serde_json::json!(boxes
                                .iter()
                                .filter(|(_, items)| items.contains(&id))
                                .map(|(mailbox, _)| (mailbox.clone(), true))
                                .collect::<std::collections::BTreeMap<_, _>>());
                            if let Some(structure) = m.mail_structures.lock().unwrap().get(&id) {
                                email["bodyStructure"] = structure.clone();
                            }

                            if let Some((value, html)) = m.mail_bodies.lock().unwrap().get(&id) {
                                let requested =
                                    args["maxBodyValueBytes"].as_u64().unwrap_or(0) as usize;
                                if args["fetchAllBodyValues"] == true {
                                    m.body_get_limits
                                        .lock()
                                        .unwrap()
                                        .push((id.clone(), requested));
                                }
                                let limit = if requested == 0
                                    && m.body_get_incomplete.load(Ordering::SeqCst)
                                {
                                    65536
                                } else {
                                    requested
                                };
                                let mut end = if limit == 0 {
                                    value.len()
                                } else {
                                    limit.min(value.len())
                                };
                                while !value.is_char_boundary(end) {
                                    end -= 1;
                                }
                                email["bodyValues"] = serde_json::json!({"1":{
                                    "value":&value[..end], "isTruncated":end < value.len()
                                }});
                                email["textBody"] = if *html {
                                    serde_json::json!([])
                                } else {
                                    serde_json::json!([{"partId":"1"}])
                                };
                                email["htmlBody"] = if *html {
                                    serde_json::json!([{"partId":"1"}])
                                } else {
                                    serde_json::json!([])
                                };
                            }
                            if boxes.get("notes").is_some_and(|items| items.contains(&id)) {
                                email["keywords"] = serde_json::json!({format!("noteid-{id}"): true, "$note": true});
                            }
                            list.push(email);
                            break;
                        }
                    }
                }
                let state = m.change_log.lock().unwrap().len().to_string();
                serde_json::json!({ "list": list, "state": state })
            }
            other => serde_json::json!({ "type": "unknownMethod", "description": other }),
        };
        responses.push(serde_json::json!([name, body, cid]));
    }
    Json(serde_json::json!({ "methodResponses": responses, "sessionState": "s" }))
}

async fn mock_es(State(m): State<Arc<Mock>>) -> impl IntoResponse {
    m.es_conns.fetch_add(1, Ordering::SeqCst);
    let rx = m.es_tx.subscribe();
    let first = tokio_stream::once(Ok::<
        String,
        tokio_stream::wrappers::errors::BroadcastStreamRecvError,
    >(": connected\n\n".to_owned()));
    let events = first
        .chain(BroadcastStream::new(rx))
        .take_while(|item| !matches!(item, Ok(s) if s == "CLOSE"))
        .filter_map(|item| item.ok())
        .map(|s| Ok::<Bytes, Infallible>(Bytes::from(s)));
    (
        [("Content-Type", "text/event-stream")],
        Body::from_stream(events),
    )
}

fn body_data_bytes(document: &wbxml::Document) -> Option<&[u8]> {
    eas::find_opaque_after(document, eas::airsync_base::DATA)
        .or_else(|| eas::find_text_after(document, eas::airsync_base::DATA).map(str::as_bytes))
}

const BLOB_BYTES: &[u8] = b"\x00\xff\x01\x02\x80payload";
const MIME_BYTES: &[u8] =
    b"From: a@example.test\r\nContent-Type: application/octet-stream\r\n\r\n\xff\x00\x80";

async fn mock_download(
    State(mock): State<Arc<Mock>>,
    axum::extract::Path((_account, blob, _name)): axum::extract::Path<(String, String, String)>,
) -> axum::response::Response {
    if mock.download_error.load(Ordering::SeqCst) || blob == "missing" {
        return (axum::http::StatusCode::NOT_FOUND, "missing").into_response();
    }
    let data = if blob == "binary" {
        BLOB_BYTES
    } else {
        MIME_BYTES
    };
    ([("Content-Type", "application/octet-stream")], data).into_response()
}

async fn start_mock() -> Arc<Mock> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (es_tx, _) = broadcast::channel(64);
    let mock = Arc::new(Mock {
        port,
        boxes: Mutex::new(Default::default()),
        es_tx,
        es_conns: AtomicUsize::new(0),
        counter: AtomicUsize::new(1),
        keywords: Mutex::new(Default::default()),
        extra_folders: Mutex::new(Vec::new()),
        stale_listing: Mutex::new(Vec::new()),
        hidden_folders: Mutex::new(Vec::new()),
        dates: Mutex::new(Default::default()),
        change_log: Mutex::new(Vec::new()),
        groupware: Mutex::new(Default::default()),
        snapshot_changes: AtomicBool::new(false),
        delay_mail_set: AtomicBool::new(false),
        delayed_mail_sets: AtomicUsize::new(0),
        snapshot_missing_item: AtomicBool::new(false),
        identity_error: AtomicBool::new(false),
        item_set_error: AtomicBool::new(false),
        drop_flag_keywords: AtomicBool::new(false),
        download_error: AtomicBool::new(false),
        mail_structures: Mutex::new(Default::default()),
        mail_bodies: Mutex::new(Default::default()),
        body_get_limits: Mutex::new(Vec::new()),
        body_get_error: AtomicBool::new(false),
        body_get_incomplete: AtomicBool::new(false),
        identities: Mutex::new(vec![
            serde_json::json!({"id": "i1", "name": "Owner", "email": "owner@example.test"}),
            serde_json::json!({"id": "i2", "name": "Alias", "email": "alias@example.test"}),
        ]),
    });
    let app = Router::new()
        .route("/.well-known/jmap", get(mock_session))
        .route("/api", post(mock_api))
        .route("/dl/:account/:blob/:name", get(mock_download))
        .route("/es/", get(mock_es))
        .with_state(mock.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    mock
}

// ------------------------------------------------------------- gateway

struct Env {
    mock: Arc<Mock>,
    gw: String,
    http: reqwest::Client,
    push: PushHub,
    state: state::SharedStateStore,
    keys: Mutex<std::collections::HashMap<String, String>>,
}

#[derive(Debug, PartialEq)]
struct PingReply {
    status: String,
    folders: Vec<String>,
    limit: Option<String>,
}

struct Opts {
    max_concurrent: usize,
    sqlite: Option<String>,
    aged_out: crate::config::AgedOut,
}

impl Default for Opts {
    fn default() -> Self {
        Self {
            max_concurrent: 200,
            sqlite: None,
            aged_out: crate::config::AgedOut::SoftDelete,
        }
    }
}

async fn start_env(opts: Opts) -> Env {
    let mock = start_mock().await;
    mock.add_mail("inbox", "m1");
    mock.add_mail("inbox", "m2");
    let (gw, push, state) = spawn_gateway(&mock, &opts, None).await;
    Env {
        mock,
        gw,
        http: reqwest::Client::new(),
        push,
        state,
        keys: Mutex::new(Default::default()),
    }
}

impl Env {
    /// A fresh gateway process on the same Stalwart and state store: what a
    /// container restart looks like (in-memory push state is gone).
    async fn restarted(&self) -> Env {
        let (gw, push, state) =
            spawn_gateway(&self.mock, &Opts::default(), Some(self.state.clone())).await;
        Env {
            mock: self.mock.clone(),
            gw,
            http: reqwest::Client::new(),
            push,
            state,
            keys: Mutex::new(self.keys.lock().unwrap().clone()),
        }
    }
}

async fn spawn_gateway(
    mock: &Arc<Mock>,
    opts: &Opts,
    store: Option<state::SharedStateStore>,
) -> (String, PushHub, state::SharedStateStore) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gw_addr = listener.local_addr().unwrap();
    let config = Config {
        listen_addr: gw_addr,
        stalwart_jmap_url: format!("http://127.0.0.1:{}/.well-known/jmap", mock.port)
            .parse()
            .unwrap(),
        stalwart_tls_verify: true,
        eas_public_url: "http://localhost/Microsoft-Server-ActiveSync"
            .parse()
            .unwrap(),
        state_backend: if opts.sqlite.is_some() {
            StateBackend::Sqlite
        } else {
            StateBackend::Memory
        },
        state_sqlite_path: opts.sqlite.clone().unwrap_or_default(),
        log_level: "info".into(),
        max_wbxml_bytes: 1 << 20,
        validate_cert_ca_file: "/etc/ssl/certs/ca-certificates.crt".into(),
        validate_cert_crl_file: None,
        ping_max_seconds: 30,
        ping_min_seconds: 1,
        ping_max_concurrent: opts.max_concurrent,
        ping_resend_guard_seconds: 1,
        push_idle_seconds: 60,
        mail_aged_out: opts.aged_out,
    };
    let state = match store {
        Some(store) => store,
        None => state::new_store(&config).await.unwrap(),
    };
    let push = PushHub::new(&config).unwrap();
    let app_state = AppState {
        config: config.clone(),
        jmap: crate::jmap::client::JmapClient::new(config.clone(), state.clone()).unwrap(),
        metrics: Metrics::new().unwrap(),
        state: state.clone(),
        push: push.clone(),
    };
    let app = build_router(app_state);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (
        format!("http://{gw_addr}/Microsoft-Server-ActiveSync"),
        push,
        state,
    )
}

fn ping_body(heartbeat: u64, folders: &[(&str, &str)]) -> Vec<u8> {
    let mut b = eas::DocumentBuilder::new();
    b.start(eas::ping::PING);
    b.leaf(eas::ping::LIFETIME, heartbeat.to_string());
    b.start(eas::ping::FOLDERS);
    for (id, class) in folders {
        b.start(eas::ping::FOLDER);
        b.leaf(eas::ping::SERVER_ENTRY_ID, *id);
        b.leaf(eas::ping::FOLDER_TYPE, *class);
        b.end();
    }
    b.end();
    b.end();
    wbxml::encode_document(&b.finish())
}

fn sync_body(key: &str, collection: &str) -> Vec<u8> {
    sync_body_window(key, collection, "100")
}

fn sync_body_window(key: &str, collection: &str, window: &str) -> Vec<u8> {
    sync_body_options(key, collection, window, None)
}

fn sync_body_options(key: &str, collection: &str, window: &str, filter: Option<&str>) -> Vec<u8> {
    use eas::airsync as air;
    let mut b = eas::DocumentBuilder::new();
    b.start(air::SYNC);
    b.start(air::COLLECTIONS);
    b.start(air::COLLECTION);
    b.leaf(air::SYNC_KEY, key);
    b.leaf(air::COLLECTION_ID, collection);
    b.empty_tag(air::GET_CHANGES);
    b.leaf(air::WINDOW_SIZE, window);
    if let Some(filter) = filter {
        b.start(air::OPTIONS);
        b.leaf(air::FILTER_TYPE, filter);
        b.end();
    }
    b.end();
    b.end();
    b.end();
    wbxml::encode_document(&b.finish())
}

fn mail_preferences(filter: Option<&str>, body: Option<(&str, &str)>) -> wbxml::Document {
    use eas::{airsync as air, airsync_base as base};
    let mut b = eas::DocumentBuilder::new();
    b.start(air::OPTIONS);
    if let Some(filter) = filter {
        b.leaf(air::FILTER_TYPE, filter);
    }
    if let Some((body_type, limit)) = body {
        b.start(base::BODY_PREFERENCE);
        b.leaf(base::TYPE, body_type);
        b.leaf(base::TRUNCATION_SIZE, limit);
        b.end();
    }
    b.end();
    b.finish()
}

fn requested_body_options(preferences: &[eas::RequestedBodyPreference]) -> wbxml::Document {
    use eas::{airsync as air, airsync_base as base};
    let mut b = eas::DocumentBuilder::new();
    b.start(air::OPTIONS);
    for preference in preferences {
        b.start(base::BODY_PREFERENCE);
        b.leaf(base::TYPE, preference.body_type.to_string());
        if let Some(limit) = preference.truncation_size {
            b.leaf(base::TRUNCATION_SIZE, limit.to_string());
        }
        b.leaf(
            base::ALL_OR_NONE,
            if preference.all_or_none { "1" } else { "0" },
        );
        if let Some(preview) = preference.preview {
            b.leaf(base::PREVIEW, preview.to_string());
        }
        b.end();
    }
    if preferences
        .iter()
        .any(|preference| preference.body_type == 4)
    {
        b.leaf(air::MIME_SUPPORT, "2");
    }
    b.end();
    b.finish()
}

fn sync_body_preferences(
    key: &str,
    folder: &str,
    options: Option<&wbxml::Document>,
    downloads: bool,
) -> Vec<u8> {
    use eas::airsync as air;
    let mut b = eas::DocumentBuilder::new();
    b.start(air::SYNC);
    b.start(air::COLLECTIONS);
    b.start(air::COLLECTION);
    b.leaf(air::SYNC_KEY, key);
    b.leaf(air::COLLECTION_ID, folder);
    b.leaf(air::GET_CHANGES, if downloads { "1" } else { "0" });
    b.leaf(air::WINDOW_SIZE, "100");
    if let Some(options) = options {
        b.extend(options.nodes.clone());
    }
    b.end();
    b.end();
    b.end();
    wbxml::encode_document(&b.finish())
}

fn partial_sync(folders: &[(&str, &str)], partial: bool) -> Vec<u8> {
    use eas::airsync as air;
    let mut b = eas::DocumentBuilder::new();
    b.start(air::SYNC);
    if partial {
        b.empty_tag(air::PARTIAL);
    }
    b.start(air::COLLECTIONS);
    for &(folder, key) in folders {
        b.start(air::COLLECTION);
        b.leaf(air::SYNC_KEY, key);
        b.leaf(air::COLLECTION_ID, folder);
        b.leaf(air::WINDOW_SIZE, "100");
        b.end();
    }
    b.end();
    b.end();
    wbxml::encode_document(&b.finish())
}

impl Env {
    fn req(&self, cmd: &str, device: &str) -> reqwest::RequestBuilder {
        self.http
            .post(format!(
                "{}?Cmd={cmd}&User=u%40test&DeviceId={device}&DeviceType=iPhone",
                self.gw
            ))
            .basic_auth("u@test", Some("pw"))
    }

    async fn ping(&self, device: &str, body: Option<Vec<u8>>) -> PingReply {
        let resp = self
            .req("Ping", device)
            .body(body.unwrap_or_default())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "Ping HTTP status");
        let bytes = resp.bytes().await.unwrap();
        let doc = wbxml::decode_document(&bytes).unwrap();
        PingReply {
            status: eas::find_text_after(&doc, eas::ping::STATUS)
                .unwrap_or("<none>")
                .to_owned(),
            folders: eas::find_all_text_after(&doc, eas::ping::FOLDER),
            limit: eas::find_text_after(&doc, eas::ping::LIFETIME).map(str::to_owned),
        }
    }

    /// Run Sync until the device is caught up; returns every id delivered.
    async fn sync_all(&self, device: &str) -> Vec<String> {
        use eas::airsync as air;
        let mut key = self
            .keys
            .lock()
            .unwrap()
            .get(device)
            .cloned()
            .unwrap_or_else(|| "0".to_owned());
        let mut delivered = Vec::new();
        for _ in 0..20 {
            let resp = self
                .req("Sync", device)
                .body(sync_body(&key, "inbox"))
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 200);
            let doc = wbxml::decode_document(&resp.bytes().await.unwrap()).unwrap();
            let new_key = eas::find_all_text_after(&doc, air::SYNC_KEY)
                .pop()
                .expect("response SyncKey");
            let adds = eas::find_all_text_after(&doc, air::SERVER_ID);
            let more = eas::contains_token(&doc, air::MORE_AVAILABLE);
            delivered.extend(adds);
            let progressed = new_key != key;
            key = new_key;
            if !more && !progressed {
                break;
            }
        }
        self.keys.lock().unwrap().insert(device.to_owned(), key);
        delivered
    }

    /// Brings another folder of the device in step (its key is not kept).
    async fn sync_folder(&self, device: &str, collection: &str) {
        use eas::airsync as air;
        let mut key = "0".to_owned();
        for _ in 0..20 {
            let resp = self
                .req("Sync", device)
                .body(sync_body(&key, collection))
                .send()
                .await
                .unwrap();
            let doc = wbxml::decode_document(&resp.bytes().await.unwrap()).unwrap();
            let new_key = eas::find_all_text_after(&doc, air::SYNC_KEY)
                .pop()
                .expect("response SyncKey");
            let more = eas::contains_token(&doc, air::MORE_AVAILABLE);
            let progressed = new_key != key;
            key = new_key;
            if !more && !progressed {
                break;
            }
        }
    }

    /// One Sync round with an explicit key: (new key, adds, changes).
    async fn sync_detail(
        &self,
        device: &str,
        key: &str,
    ) -> (String, Vec<String>, Vec<(String, String)>) {
        self.sync_detail_window(device, key, "100").await
    }

    async fn sync_detail_window(
        &self,
        device: &str,
        key: &str,
        window: &str,
    ) -> (String, Vec<String>, Vec<(String, String)>) {
        use eas::airsync as air;
        let resp = self
            .req("Sync", device)
            .body(sync_body_window(key, "inbox", window))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let doc = wbxml::decode_document(&resp.bytes().await.unwrap()).unwrap();
        let new_key = eas::find_all_text_after(&doc, air::SYNC_KEY).pop().unwrap();
        let mut adds = Vec::new();
        let mut changes = Vec::new();
        let mut mode = 0u8; // 1 add, 2 change
        let mut pending: Option<String> = None;
        let mut iter = doc.nodes.iter().peekable();
        while let Some(node) = iter.next() {
            if let wbxml::Node::Start(t) = node {
                if t.code_page == air::PAGE && t.token == air::ADD.token {
                    mode = 1;
                } else if t.code_page == air::PAGE && t.token == air::CHANGE.token {
                    mode = 2;
                } else if t.code_page == air::PAGE && t.token == air::SERVER_ID.token {
                    if let Some(wbxml::Node::Text(id)) = iter.peek() {
                        match mode {
                            1 => adds.push(id.clone()),
                            2 => pending = Some(id.clone()),
                            _ => {}
                        }
                    }
                } else if t.code_page == eas::email::PAGE && t.token == eas::email::READ.token {
                    if let (Some(wbxml::Node::Text(v)), Some(id)) = (iter.peek(), pending.take()) {
                        changes.push((id, v.clone()));
                    }
                }
            }
        }
        (new_key, adds, changes)
    }

    async fn wait_connected(&self, n: usize) {
        let start = Instant::now();
        while self.mock.connections() < n {
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "EventSource never connected"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        // let the subscriber finish its post-connect bump before the test acts
        tokio::time::sleep(Duration::from_millis(150)).await;
    }

    fn spawn_ping(
        self: &Arc<Self>,
        device: &'static str,
        hb: u64,
        folders: &'static [(&'static str, &'static str)],
    ) -> tokio::task::JoinHandle<(PingReply, Duration)> {
        let env = self.clone();
        tokio::spawn(async move {
            let t = Instant::now();
            let reply = env.ping(device, Some(ping_body(hb, folders))).await;
            (reply, t.elapsed())
        })
    }
}

const INBOX: &[(&str, &str)] = &[("inbox", "Email")];

// --------------------------------------------------------------- tests

#[tokio::test]
async fn new_email_during_active_ping_then_sync_then_idle() {
    let env = Arc::new(start_env(Opts::default()).await);
    assert_eq!(env.sync_all("devA").await, vec!["m2", "m1"]);

    let ping = env.spawn_ping("devA", 10, INBOX);
    env.wait_connected(1).await;
    env.mock.add_mail("inbox", "m3");
    let sent = Instant::now();
    env.mock.emit_email_change();

    let (reply, elapsed) = ping.await.unwrap();
    assert_eq!(reply.status, "2");
    assert_eq!(reply.folders, vec!["inbox"]);
    assert!(
        sent.elapsed() < Duration::from_secs(2),
        "latency {elapsed:?}"
    );

    // Follow-up Sync delivers exactly the new message.
    assert_eq!(env.sync_all("devA").await, vec!["m3"]);

    // Ready for the next Ping: no change -> no loop, Status 1 at heartbeat.
    let t = Instant::now();
    let reply = env.ping("devA", Some(ping_body(2, INBOX))).await;
    assert_eq!(reply.status, "1");
    assert!(t.elapsed() >= Duration::from_millis(1900));
}

#[tokio::test]
async fn no_changes_until_heartbeat_expiry() {
    let env = Arc::new(start_env(Opts::default()).await);
    env.sync_all("devA").await;
    let t = Instant::now();
    let reply = env.ping("devA", Some(ping_body(2, INBOX))).await;
    assert_eq!(
        reply,
        PingReply {
            status: "1".into(),
            folders: vec![],
            limit: None
        }
    );
    assert!(t.elapsed() >= Duration::from_millis(1900));
}

#[tokio::test]
async fn change_right_before_waiter_registration_is_not_lost() {
    let env = Arc::new(start_env(Opts::default()).await);
    env.sync_all("devA").await;
    // Mail lands while no Ping and no EventSource exist: the event is lost.
    env.mock.add_mail("inbox", "m3");
    let t = Instant::now();
    let reply = env.ping("devA", Some(ping_body(10, INBOX))).await;
    assert_eq!(reply.status, "2");
    assert_eq!(reply.folders, vec!["inbox"]);
    assert!(t.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn multiple_devices_share_one_subscription_and_all_wake() {
    let env = Arc::new(start_env(Opts::default()).await);
    env.sync_all("devA").await;
    env.sync_all("devB").await;
    let a = env.spawn_ping("devA", 10, INBOX);
    let b = env.spawn_ping("devB", 10, INBOX);
    env.wait_connected(1).await;
    env.mock.add_mail("inbox", "m3");
    env.mock.emit_email_change();
    let (ra, _) = a.await.unwrap();
    let (rb, _) = b.await.unwrap();
    assert_eq!((ra.status.as_str(), rb.status.as_str()), ("2", "2"));
    assert_eq!(
        env.mock.connections(),
        1,
        "one EventSource per account, not per Ping/device"
    );
    // Each device still gets the message through its own Sync state.
    assert_eq!(env.sync_all("devA").await, vec!["m3"]);
    assert_eq!(env.sync_all("devB").await, vec!["m3"]);
}

#[tokio::test]
async fn unmonitored_folder_change_does_not_wake() {
    let env = Arc::new(start_env(Opts::default()).await);
    env.sync_all("devA").await;
    let ping = env.spawn_ping("devA", 2, INBOX);
    env.wait_connected(1).await;
    env.mock.add_mail("other", "x1");
    env.mock.emit_email_change();
    let (reply, elapsed) = ping.await.unwrap();
    assert_eq!(
        reply.status, "1",
        "Email event for another folder is not a change"
    );
    assert!(elapsed >= Duration::from_millis(1800));
}

#[tokio::test]
async fn burst_is_coalesced_into_one_wakeup_and_one_sync() {
    let env = Arc::new(start_env(Opts::default()).await);
    env.sync_all("devA").await;
    let ping = env.spawn_ping("devA", 10, INBOX);
    env.wait_connected(1).await;
    for id in ["b1", "b2", "b3"] {
        env.mock.add_mail("inbox", id);
        env.mock.emit_email_change();
    }
    let (reply, _) = ping.await.unwrap();
    assert_eq!(reply.status, "2");
    let mut got = env.sync_all("devA").await;
    got.sort();
    assert_eq!(got, vec!["b1", "b2", "b3"]);
    let reply = env.ping("devA", Some(ping_body(2, INBOX))).await;
    assert_eq!(
        reply.status, "1",
        "no repeat notification after the burst was synced"
    );
}

#[tokio::test]
async fn eventsource_reconnect_reconciles_missed_changes() {
    let env = Arc::new(start_env(Opts::default()).await);
    env.sync_all("devA").await;
    let ping = env.spawn_ping("devA", 15, INBOX);
    env.wait_connected(1).await;
    env.mock.close_streams();
    // Delivered while the stream is down: no event will ever announce it.
    env.mock.add_mail("inbox", "m3");
    let (reply, elapsed) = ping.await.unwrap();
    assert_eq!(reply.status, "2");
    assert!(env.mock.connections() >= 2, "subscriber reconnected");
    assert!(
        elapsed < Duration::from_secs(8),
        "recovered via reconnect, took {elapsed:?}"
    );
}

#[tokio::test]
async fn duplicate_state_events_do_not_wake_or_loop() {
    let env = Arc::new(start_env(Opts::default()).await);
    env.sync_all("devA").await;
    let ping = env.spawn_ping("devA", 2, INBOX);
    env.wait_connected(1).await;
    let same = r#"{"changed":{"a1":{"Email":"same","Mailbox":"same"}}}"#.to_owned();
    for _ in 0..5 {
        env.mock.emit(same.clone());
    }
    let (reply, _) = ping.await.unwrap();
    assert_eq!(reply.status, "1");
}

#[tokio::test]
async fn ping_status_codes_and_cached_parameters() {
    let env = Arc::new(start_env(Opts::default()).await);
    env.sync_all("devA").await;

    // Heartbeat outside [1, 30]: Status 5 with the limit, not silent truncation.
    let r = env.ping("devA", Some(ping_body(100, INBOX))).await;
    assert_eq!((r.status.as_str(), r.limit.as_deref()), ("5", Some("30")));
    let r = env.ping("devA", Some(ping_body(0, INBOX))).await;
    assert_eq!((r.status.as_str(), r.limit.as_deref()), ("5", Some("1")));
    // Unknown folder: folder hierarchy out of date.
    let r = env
        .ping("devA", Some(ping_body(2, &[("nope", "Email")])))
        .await;
    assert_eq!(r.status, "7");

    // Valid Ping caches heartbeat+folders; a later empty-body Ping reuses them.
    let r = env.ping("devA", Some(ping_body(2, INBOX))).await;
    assert_eq!(r.status, "1");
    env.mock.add_mail("inbox", "m3");
    let r = env.ping("devA", None).await;
    assert_eq!(
        (r.status.as_str(), r.folders),
        ("2", vec!["inbox".to_owned()])
    );
}

#[tokio::test]
async fn unsynced_status2_is_not_re_answered_in_a_tight_loop() {
    let env = Arc::new(start_env(Opts::default()).await);
    env.sync_all("devA").await;
    env.mock.add_mail("inbox", "m3");
    assert_eq!(
        env.ping("devA", Some(ping_body(10, INBOX))).await.status,
        "2"
    );
    // Client re-pings without syncing: the answer is held back for the
    // guard window (1 s in tests, 15 s by default), not repeated at once,
    // then given again so the change is not lost.
    let t = Instant::now();
    let r = env.ping("devA", Some(ping_body(20, INBOX))).await;
    assert_eq!(r.status, "2");
    assert!(
        t.elapsed() >= Duration::from_millis(800),
        "answered too fast: {:?}",
        t.elapsed()
    );
    assert!(
        t.elapsed() < Duration::from_secs(4),
        "took {:?}",
        t.elapsed()
    );
    // Once it has synced, it is quiet again.
    env.sync_all("devA").await;
    assert_eq!(
        env.ping("devA", Some(ping_body(2, INBOX))).await.status,
        "1"
    );
}

#[tokio::test]
async fn client_cancellation_releases_waiter_and_slot() {
    let env = Arc::new(
        start_env(Opts {
            max_concurrent: 1,
            sqlite: None,
            ..Opts::default()
        })
        .await,
    );
    env.sync_all("devA").await;

    // Client gives up after 600 ms (connection drop while Ping pending).
    let res = env
        .req("Ping", "devA")
        .body(ping_body(20, INBOX))
        .timeout(Duration::from_millis(600))
        .send()
        .await;
    assert!(res.is_err());
    let user = env.push.user("u@test");
    let start = Instant::now();
    while user.waiter_count() != 0 {
        assert!(start.elapsed() < Duration::from_secs(3), "waiter leaked");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Slot is free again (limit is 1): this must not be a 503.
    let r = env.ping("devA", Some(ping_body(1, INBOX))).await;
    assert_eq!(r.status, "1");
}

#[tokio::test]
async fn concurrent_ping_limit_returns_503() {
    let env = Arc::new(
        start_env(Opts {
            max_concurrent: 1,
            sqlite: None,
            ..Opts::default()
        })
        .await,
    );
    env.sync_all("devA").await;
    let first = env.spawn_ping("devA", 3, INBOX);
    env.wait_connected(1).await;
    let resp = env
        .req("Ping", "devB")
        .body(ping_body(1, INBOX))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 503);
    assert_eq!(resp.headers().get("Retry-After").unwrap(), "60");
    first.abort();
}

#[tokio::test]
async fn existing_sqlite_device_state_is_honoured() {
    let dir = std::env::temp_dir().join(format!("ssg-push-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("state.db").to_string_lossy().into_owned();
    {
        // A device partnership written the way the deployed gateway wrote it.
        let cfg = Config {
            listen_addr: "127.0.0.1:0".parse().unwrap(),
            stalwart_jmap_url: "http://127.0.0.1:1/".parse().unwrap(),
            stalwart_tls_verify: true,
            eas_public_url: "http://x/".parse().unwrap(),
            state_backend: StateBackend::Sqlite,
            state_sqlite_path: path.clone(),
            log_level: "info".into(),
            max_wbxml_bytes: 1,
            validate_cert_ca_file: "/etc/ssl/certs/ca-certificates.crt".into(),
            validate_cert_crl_file: None,
            ping_max_seconds: 30,
            ping_min_seconds: 1,
            ping_max_concurrent: 1,
            ping_resend_guard_seconds: 1,
            push_idle_seconds: 1,
            mail_aged_out: crate::config::AgedOut::SoftDelete,
        };
        let store = state::new_store(&cfg).await.unwrap();
        store
            .put(SyncRecord {
                user: "u@test".into(),
                device_id: "oldphone".into(),
                collection_id: "inbox".into(),
                sync_key: "7".into(),
                jmap_state: String::new(),
                seen_ids: vec!["m1".into(), "m2".into()],
            })
            .await
            .unwrap();
    }
    let env = Arc::new(
        start_env(Opts {
            max_concurrent: 10,
            sqlite: Some(path),
            ..Opts::default()
        })
        .await,
    );
    // Existing partnership with no stored read states: no wake for that alone
    // (a Ping answer the next Sync could not act on gets the folder
    // blacklisted on iOS), but its next Sync, on the old SyncKey 7, carries
    // a catch-up Change for every message it already has.
    let r = env.ping("oldphone", Some(ping_body(1, INBOX))).await;
    assert_eq!(r.status, "1");
    let (key, adds, changes) = env.sync_detail("oldphone", "7").await;
    assert_eq!(key, "8");
    assert!(adds.is_empty());
    assert_eq!(
        changes.len(),
        2,
        "catch-up Change for the two known messages"
    );
    // Caught up: quiet again.
    let r = env.ping("oldphone", Some(ping_body(1, INBOX))).await;
    assert_eq!(r.status, "1");
    // New mail: the old seen-ids still drive a Status 2.
    env.mock.add_mail("inbox", "m3");
    let r = env.ping("oldphone", Some(ping_body(5, INBOX))).await;
    assert_eq!(r.status, "2");
    let (_, adds, _) = env.sync_detail("oldphone", &key).await;
    assert_eq!(adds, vec!["m3"]);
    assert!(env
        .state
        .get("u@test", "oldphone", "inbox")
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn server_side_delete_wakes_ping() {
    let env = Arc::new(start_env(Opts::default()).await);
    env.sync_all("devA").await;
    let ping = env.spawn_ping("devA", 10, INBOX);
    env.wait_connected(1).await;
    env.mock.remove_mail("inbox", "m1");
    env.mock.emit_email_change();
    let (reply, _) = ping.await.unwrap();
    assert_eq!(reply.status, "2");
}

#[tokio::test]
async fn newer_ping_from_same_device_supersedes_the_older_one() {
    let env = Arc::new(start_env(Opts::default()).await);
    env.sync_all("devA").await;
    let old = env.spawn_ping("devA", 20, INBOX);
    env.wait_connected(1).await;
    let user = env.push.user("u@test");
    assert_eq!(user.waiter_count(), 1);
    // The device opens a new Ping without the old connection ever closing.
    let newer = env.spawn_ping("devA", 20, INBOX);
    let (reply, elapsed) = old.await.unwrap();
    assert_eq!(reply.status, "1");
    assert!(
        elapsed < Duration::from_secs(3),
        "old Ping freed late: {elapsed:?}"
    );
    assert_eq!(user.waiter_count(), 1, "only the newest Ping still waits");
    // The newest one still gets the real wakeup.
    env.mock.add_mail("inbox", "m3");
    env.mock.emit_email_change();
    let (reply, _) = newer.await.unwrap();
    assert_eq!(reply.status, "2");
}

#[tokio::test]
async fn read_on_one_device_reaches_the_other_without_looping() {
    let env = Arc::new(start_env(Opts::default()).await);
    env.sync_all("phone").await;
    env.sync_all("ipad").await;
    let key = env.keys.lock().unwrap().get("ipad").cloned().unwrap();

    // Phone reads m1 (another client sets $seen on the server).
    let ping = env.spawn_ping("ipad", 10, INBOX);
    env.wait_connected(1).await;
    env.mock.set_read("m1", true);
    env.mock.emit_email_change();
    let (reply, elapsed) = ping.await.unwrap();
    assert_eq!(
        reply.status, "2",
        "read-state change wakes the other device"
    );
    assert!(elapsed < Duration::from_secs(2));

    let (key2, adds, changes) = env.sync_detail("ipad", &key).await;
    assert!(adds.is_empty());
    assert_eq!(changes, vec![("m1".to_owned(), "1".to_owned())]);
    assert_ne!(key2, key);

    // Delivered once: next Sync is empty and Ping goes back to waiting.
    let (key3, adds, changes) = env.sync_detail("ipad", &key2).await;
    assert!(adds.is_empty() && changes.is_empty());
    assert_eq!(key3, key2);
    let r = env.ping("ipad", Some(ping_body(2, INBOX))).await;
    assert_eq!(r.status, "1");

    // Unread again.
    env.mock.set_read("m1", false);
    let r = env.ping("ipad", Some(ping_body(5, INBOX))).await;
    assert_eq!(r.status, "2");
    let (_, _, changes) = env.sync_detail("ipad", &key3).await;
    assert_eq!(changes, vec![("m1".to_owned(), "0".to_owned())]);
}

#[tokio::test]
async fn empty_ping_with_nothing_stored_gets_status_3_then_a_full_ping_works() {
    // [MS-ASCMD] Ping Status 3: required parameters missing and none stored.
    // The device resends a full Ping, which is stored for later empty ones.
    let env = Arc::new(start_env(Opts::default()).await);
    env.sync_all("ipad").await;
    env.mock.add_mail("inbox", "m3");
    let r = env.ping("ipad", None).await;
    assert_eq!((r.status.as_str(), r.folders.len()), ("3", 0));
    // Status 3 does not count as an announcement or eat the change.
    let r = env.ping("ipad", Some(ping_body(3, INBOX))).await;
    assert_eq!(
        (r.status.as_str(), r.folders),
        ("2", vec!["inbox".to_owned()])
    );
    env.sync_all("ipad").await;
    env.mock.add_mail("inbox", "m4");
    let r = env.ping("ipad", None).await;
    assert_eq!(
        (r.status.as_str(), r.folders),
        ("2", vec!["inbox".to_owned()]),
        "parameters from the full Ping are reused"
    );
}

#[tokio::test]
async fn heartbeat_alone_without_folders_and_nothing_stored_gets_status_3() {
    let env = Arc::new(start_env(Opts::default()).await);
    env.sync_all("ipad").await;
    let r = env.ping("ipad", Some(ping_body(3, &[]))).await;
    assert_eq!(r.status, "3");
}

#[tokio::test]
async fn status_3_is_not_sent_after_a_restart_when_parameters_are_stored() {
    let env = Arc::new(start_env(Opts::default()).await);
    env.sync_all("iphone").await;
    let r = env.ping("iphone", Some(ping_body(2, INBOX))).await;
    assert_eq!(r.status, "1");
    let env2 = Arc::new(env.restarted().await);
    let r = env2.ping("iphone", None).await;
    assert_eq!(r.status, "1", "stored parameters used, no Status 3");
}

#[tokio::test]
async fn ping_parameters_survive_a_gateway_restart() {
    // Prod 2026-11-12: after a restart the iPad kept sending empty Pings and
    // the gateway, with its parameters lost, watched only the Inbox all
    // night. They are now kept in the state store.
    let env = Arc::new(start_env(Opts::default()).await);
    env.sync_all("ipad").await;
    env.sync_folder("ipad", "other").await;
    let folders = &[("inbox", "Email"), ("other", "Email")];
    let r = env.ping("ipad", Some(ping_body(3, folders))).await;
    assert_eq!(r.status, "1");

    let env2 = Arc::new(env.restarted().await);
    env2.mock.add_mail("other", "o1");
    let r = env2.ping("ipad", None).await;
    assert_eq!(
        (r.status.as_str(), r.folders),
        ("2", vec!["other".to_owned()]),
        "empty Ping after restart still watches the Archive"
    );
}

#[tokio::test]
async fn read_change_on_message_older_than_the_window_still_reaches_the_device() {
    let env = Arc::new(start_env(Opts::default()).await);
    for n in 0..30 {
        env.mock.add_mail("inbox", &format!("x{n:02}"));
    }
    env.sync_all("ipad").await;
    let key = env.keys.lock().unwrap().get("ipad").cloned().unwrap();

    // m1 is the oldest message: far outside a 5-message window and outside
    // the newest-25 the Ping probe compares.
    env.mock.set_read("m1", true);
    let r = env.ping("ipad", Some(ping_body(10, INBOX))).await;
    assert_eq!(r.status, "2", "older read change wakes the device");

    let (key2, adds, changes) = env.sync_detail_window("ipad", &key, "5").await;
    assert!(adds.is_empty());
    assert_eq!(changes, vec![("m1".to_owned(), "1".to_owned())]);
    assert_ne!(key2, key);

    // Delivered once; quiet afterwards.
    let (_, adds, changes) = env.sync_detail_window("ipad", &key2, "5").await;
    assert!(adds.is_empty() && changes.is_empty());
    let r = env.ping("ipad", Some(ping_body(2, INBOX))).await;
    assert_eq!(r.status, "1");
}

#[tokio::test]
async fn read_change_on_newest_message_is_delivered_in_a_large_mailbox() {
    // Regression (found by the 5000-message e2e run): Sync picked "the newest
    // 300 seen messages" from an id-sorted set, so in a big mailbox whose ids
    // don't sort by date a read change on the newest message woke Ping
    // (Status 2) but the Sync sent nothing -- the undeliverable Status 2 iOS
    // counts toward its push blacklist. Here the newest ids sort lowest.
    let env = Arc::new(start_env(Opts::default()).await);
    for n in 0..400 {
        env.mock.add_mail("inbox", &format!("m{:03}", 999 - n));
    }
    env.sync_all("ipad").await;
    let key = env.keys.lock().unwrap().get("ipad").cloned().unwrap();

    env.mock.set_read("m600", true); // the newest message
    let r = env.ping("ipad", Some(ping_body(10, INBOX))).await;
    assert_eq!(r.status, "2");
    let (key2, adds, changes) = env.sync_detail("ipad", &key).await;
    assert!(adds.is_empty());
    assert_eq!(changes, vec![("m600".to_owned(), "1".to_owned())]);
    assert_ne!(key2, key);
    let r = env.ping("ipad", Some(ping_body(2, INBOX))).await;
    assert_eq!(r.status, "1", "quiet once delivered");

    // The same holds for a move away of the newest message: Delete arrives.
    env.mock.remove_mail("inbox", "m601");
    env.mock.add_mail("other", "m601");
    let r = env.ping("ipad", Some(ping_body(10, INBOX))).await;
    assert_eq!(r.status, "2");
    let (key3, _, _) = env.sync_detail("ipad", &key2).await;
    assert_ne!(key3, key2, "the move was delivered");
    let r = env.ping("ipad", Some(ping_body(2, INBOX))).await;
    assert_eq!(r.status, "1");
}

#[tokio::test]
async fn changes_on_a_message_older_than_every_window_still_reach_the_device() {
    // m1 is the oldest of 402 messages: outside the newest 300 Sync refreshes,
    // the 200-id slice Ping checks and the newest 25. Only Email/changes since
    // the device's last Sync sees a read flip or move on it.
    let env = Arc::new(start_env(Opts::default()).await);
    for n in 0..400 {
        env.mock.add_mail("inbox", &format!("x{n:03}"));
    }
    env.sync_all("ipad").await;
    let key = env.keys.lock().unwrap().get("ipad").cloned().unwrap();

    env.mock.set_read("m1", true);
    let r = env.ping("ipad", Some(ping_body(10, INBOX))).await;
    assert_eq!(r.status, "2", "old read change announced");
    let (key2, adds, changes) = env.sync_detail("ipad", &key).await;
    assert!(adds.is_empty());
    assert_eq!(changes, vec![("m1".to_owned(), "1".to_owned())]);
    let r = env.ping("ipad", Some(ping_body(2, INBOX))).await;
    assert_eq!(r.status, "1", "delivered once");

    // m2 (second oldest) moves out: Delete.
    env.mock.remove_mail("inbox", "m2");
    env.mock.add_mail("other", "m2");
    let r = env.ping("ipad", Some(ping_body(10, INBOX))).await;
    assert_eq!(r.status, "2", "old move announced");
    let (key3, _, _) = env.sync_detail("ipad", &key2).await;
    assert_ne!(key3, key2, "Delete delivered");
    let r = env.ping("ipad", Some(ping_body(2, INBOX))).await;
    assert_eq!(r.status, "1");
}

#[tokio::test]
async fn filter_type_limits_what_is_sent_and_what_ping_announces() {
    // [MS-ASCMD] Options > FilterType 3 = one week. Messages older than that
    // are not sent; Ping must not announce them either (an announcement Sync
    // won't deliver counts toward the iOS push blacklist), and Options
    // persist when a later Sync omits them.
    use eas::airsync as air;
    let env = Arc::new(start_env(Opts::default()).await);
    env.mock.add_mail("inbox", "old1");
    env.mock.set_date("old1", &days_ago(30));
    env.mock.add_mail("inbox", "new1");

    let sync = |key: String, filter: Option<&'static str>| {
        let env = env.clone();
        async move {
            let resp = env
                .req("Sync", "ipad")
                .body(sync_body_options(&key, "inbox", "100", filter))
                .send()
                .await
                .unwrap();
            let doc = wbxml::decode_document(&resp.bytes().await.unwrap()).unwrap();
            (
                eas::find_all_text_after(&doc, air::SYNC_KEY).pop().unwrap(),
                eas::find_all_text_after(&doc, air::SERVER_ID),
            )
        }
    };
    let (k1, _) = sync("0".into(), Some("3")).await;
    let (k2, ids) = sync(k1, Some("3")).await;
    let mut ids = ids;
    ids.sort();
    assert_eq!(ids, vec!["m1", "m2", "new1"], "old1 is outside one week");
    // A later Sync without Options keeps FilterType 3.
    let (k3, ids) = sync(k2.clone(), None).await;
    assert!(ids.is_empty(), "nothing more: {ids:?}");
    assert_eq!(k3, k2);

    // An old message moved into the Inbox: outside the range, so Ping stays
    // quiet and Sync sends nothing.
    env.mock.add_mail("inbox", "old2");
    env.mock.set_date("old2", &days_ago(20));
    let r = env.ping("ipad", Some(ping_body(2, INBOX))).await;
    assert_eq!(r.status, "1", "out-of-range mail is not announced");

    // A recent one is announced and delivered.
    env.mock.add_mail("inbox", "new2");
    let r = env.ping("ipad", Some(ping_body(5, INBOX))).await;
    assert_eq!(r.status, "2");
    let (_, ids) = sync(k3, None).await;
    assert_eq!(ids, vec!["new2"]);

    // FilterType 0 (all) brings the older messages in.
    let (k4, _) = sync("0".into(), Some("0")).await;
    let (_, ids) = sync(k4, Some("0")).await;
    assert!(ids.contains(&"old1".to_owned()) && ids.contains(&"old2".to_owned()));
}

#[tokio::test]
async fn ignored_status2_is_answered_again_when_the_guard_window_ends() {
    let env = Arc::new(start_env(Opts::default()).await);
    env.sync_all("ipad").await;
    env.mock.add_mail("inbox", "m3");
    assert_eq!(
        env.ping("ipad", Some(ping_body(10, INBOX))).await.status,
        "2"
    );
    // The device ignored that answer (its own Sync raced it) and pings
    // again without syncing: the pending change must come back within the
    // guard window (1 s here), not wait for the next event or the heartbeat.
    let t = Instant::now();
    let r = env.ping("ipad", Some(ping_body(20, INBOX))).await;
    assert_eq!(
        (r.status.as_str(), r.folders),
        ("2", vec!["inbox".to_owned()])
    );
    assert!(
        t.elapsed() < Duration::from_secs(4),
        "took {:?}",
        t.elapsed()
    );
}

#[tokio::test]
async fn ping_ignores_messages_outside_the_devices_sync_window() {
    // A device syncing with a small window is only told about changes its
    // next Sync can deliver: iOS blacklists a folder from push after more
    // than 5 Status 2 answers with no real change.
    use eas::airsync as air;
    let env = Arc::new(start_env(Opts::default()).await);
    for n in 0..30 {
        env.mock.add_mail("inbox", &format!("x{n:02}"));
    }
    // Full initial sync with WindowSize 5, following MoreAvailable.
    let mut key = "0".to_owned();
    loop {
        let resp = env
            .req("Sync", "phone")
            .body(sync_body_window(&key, "inbox", "5"))
            .send()
            .await
            .unwrap();
        let doc = wbxml::decode_document(&resp.bytes().await.unwrap()).unwrap();
        key = eas::find_all_text_after(&doc, air::SYNC_KEY).pop().unwrap();
        if key != "1" && !eas::contains_token(&doc, air::MORE_AVAILABLE) {
            break;
        }
    }
    for _ in 0..3 {
        let r = env.ping("phone", Some(ping_body(1, INBOX))).await;
        assert_eq!(r.status, "1", "fully synced: nothing to announce");
    }
    // A genuinely new message is announced and delivered on its own.
    env.mock.add_mail("inbox", "fresh");
    let r = env.ping("phone", Some(ping_body(5, INBOX))).await;
    assert_eq!(r.status, "2");
    let (_, adds, _) = env.sync_detail_window("phone", &key, "5").await;
    assert_eq!(adds, vec!["fresh"]);
}

#[tokio::test]
async fn a_folder_is_not_announced_more_than_three_times_without_a_sync() {
    let env = Arc::new(start_env(Opts::default()).await);
    env.sync_all("phone").await;
    // A message the phone has leaves the Inbox; the phone never syncs it.
    env.mock.remove_mail("inbox", "m1");
    for round in 1..=3 {
        let r = env.ping("phone", Some(ping_body(10, INBOX))).await;
        assert_eq!(r.status, "2", "announcement {round}");
    }
    let t = Instant::now();
    let r = env.ping("phone", Some(ping_body(2, INBOX))).await;
    assert_eq!(r.status, "1", "fourth announcement is held back");
    assert!(t.elapsed() >= Duration::from_millis(1800));
    // Once the phone syncs the folder, the change is delivered and it is quiet.
    env.sync_all("phone").await;
    assert_eq!(
        env.ping("phone", Some(ping_body(1, INBOX))).await.status,
        "1"
    );
}

#[tokio::test]
async fn a_fresh_sync_forgets_messages_the_device_no_longer_has() {
    let env = Arc::new(start_env(Opts::default()).await);
    env.sync_all("phone").await;
    env.mock.remove_mail("inbox", "m1");
    // The phone restarts the folder from scratch (SyncKey 0).
    let (key0, adds, _) = env.sync_detail("phone", "0").await;
    assert!(adds.is_empty());
    let (_, adds, _) = env.sync_detail("phone", &key0).await;
    assert_eq!(adds, vec!["m2"]);
    // m1 is gone from the server and from the phone: nothing to report.
    let r = env.ping("phone", Some(ping_body(1, INBOX))).await;
    assert_eq!(r.status, "1");
}

#[tokio::test]
async fn initial_sync_reply_has_no_commands_block() {
    // iOS 27 fails the whole folder sync if the SyncKey 0 reply has <Commands>.
    use eas::airsync as air;
    let env = Arc::new(start_env(Opts::default()).await);
    let resp = env
        .req("Sync", "fresh")
        .body(sync_body("0", "inbox"))
        .send()
        .await
        .unwrap();
    let doc = wbxml::decode_document(&resp.bytes().await.unwrap()).unwrap();
    assert!(
        !eas::contains_token(&doc, air::COMMANDS),
        "no <Commands> on SyncKey 0"
    );
    assert_eq!(
        eas::find_all_text_after(&doc, air::SYNC_KEY)
            .pop()
            .as_deref(),
        Some("1")
    );
    // The next request gets the messages.
    let (_, adds, _) = env.sync_detail("fresh", "1").await;
    assert_eq!(adds, vec!["m2", "m1"]);
}

#[tokio::test]
async fn sync_pages_through_the_whole_folder_with_more_available() {
    use eas::airsync as air;
    let env = Arc::new(start_env(Opts::default()).await);
    for n in 0..30 {
        env.mock.add_mail("inbox", &format!("x{n:02}"));
    }
    // Prime (SyncKey 0): no items, no MoreAvailable.
    let resp = env
        .req("Sync", "pager")
        .body(sync_body_window("0", "inbox", "10"))
        .send()
        .await
        .unwrap();
    let doc = wbxml::decode_document(&resp.bytes().await.unwrap()).unwrap();
    assert!(!eas::contains_token(&doc, air::MORE_AVAILABLE));
    let mut key = eas::find_all_text_after(&doc, air::SYNC_KEY).pop().unwrap();
    let mut all = Vec::new();
    let mut rounds = 0;
    loop {
        rounds += 1;
        let resp = env
            .req("Sync", "pager")
            .body(sync_body_window(&key, "inbox", "10"))
            .send()
            .await
            .unwrap();
        let doc = wbxml::decode_document(&resp.bytes().await.unwrap()).unwrap();
        key = eas::find_all_text_after(&doc, air::SYNC_KEY).pop().unwrap();
        let adds = eas::find_all_text_after(&doc, air::SERVER_ID);
        assert!(adds.len() <= 10, "never more than WindowSize per reply");
        all.extend(adds);
        if !eas::contains_token(&doc, air::MORE_AVAILABLE) {
            break;
        }
        assert!(rounds < 10, "paging must terminate");
    }
    assert_eq!(rounds, 4, "32 messages at 10 per reply");
    all.sort();
    all.dedup();
    assert_eq!(all.len(), 32, "every message delivered exactly once");
    // Newest first: the first page started with the newest message.
    let r = env.ping("pager", Some(ping_body(1, INBOX))).await;
    assert_eq!(r.status, "1", "fully synced folder is quiet");
}

fn folder_sync_body(key: &str) -> Vec<u8> {
    use eas::folder_hierarchy as fh;
    let mut b = eas::DocumentBuilder::new();
    b.start(fh::FOLDER_SYNC);
    b.leaf(fh::SYNC_KEY, key);
    b.end();
    wbxml::encode_document(&b.finish())
}

/// (status, new key, count, added ids, deleted ids)
async fn folder_sync(
    env: &Env,
    device: &str,
    key: &str,
) -> (String, Option<String>, Option<String>, Vec<String>) {
    use eas::folder_hierarchy as fh;
    let resp = env
        .req("FolderSync", device)
        .body(folder_sync_body(key))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let doc = wbxml::decode_document(&resp.bytes().await.unwrap()).unwrap();
    (
        eas::find_text_after(&doc, fh::STATUS).unwrap().to_owned(),
        eas::find_text_after(&doc, fh::SYNC_KEY).map(str::to_owned),
        eas::find_text_after(&doc, fh::COUNT).map(str::to_owned),
        eas::find_all_text_after(&doc, fh::SERVER_ID),
    )
}

#[tokio::test]
async fn replacement_draft_keeps_identity_and_sends_content_changes() {
    use eas::{airsync as air, email};
    let env = start_env(Opts::default()).await;
    env.mock.set_keyword("m1", "$draft", true);
    for device in ["draft-a", "draft-b"] {
        let ids = env.sync_all(device).await;
        assert!(ids.contains(&"m1".to_owned()));
    }
    env.mock.add_mail("inbox", "replacement");
    env.mock
        .mail_bodies
        .lock()
        .unwrap()
        .insert("replacement".to_owned(), ("x".repeat(70_000), false));
    env.mock.set_keyword("replacement", "$draft", true);
    env.mock.remove_mail("inbox", "m1");
    assert!(env
        .state
        .swap_draft_mapping("a1", "m1", "m1", "replacement")
        .await
        .unwrap());
    for device in ["draft-a", "draft-b"] {
        let key = env.keys.lock().unwrap().get(device).unwrap().clone();
        let response = env
            .req("Sync", device)
            .body(sync_body(&key, "inbox"))
            .send()
            .await
            .unwrap();
        let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
        let commands = eas::child_elements(&doc)
            .into_iter()
            .flat_map(|child| eas::child_elements(&child))
            .flat_map(|child| eas::child_elements(&child))
            .find(|child| {
                matches!(child.nodes.first(), Some(wbxml::Node::Start(token))
                if token.code_page==air::PAGE && token.token==air::COMMANDS.token)
            })
            .unwrap();
        let changes = eas::child_elements(&commands);
        assert!(changes.iter().any(
            |change| matches!(change.nodes.first(), Some(wbxml::Node::Start(token))
            if token.code_page==air::PAGE && token.token==air::CHANGE.token)
                && eas::find_text_after(change, air::SERVER_ID) == Some("m1")
                && eas::find_text_after(change, email::SUBJECT) == Some("subject replacement")
        ));
        assert!(!changes.iter().any(|change| matches!(change.nodes.first(), Some(wbxml::Node::Start(token))
            if token.code_page==air::PAGE && (token.token==air::ADD.token || token.token==air::DELETE.token))));
    }
    assert!(env
        .mock
        .body_get_limits
        .lock()
        .unwrap()
        .iter()
        .any(|(id, limit)| id == "replacement" && *limit == 0));
}

#[tokio::test]
async fn folder_sync_is_incremental_and_checks_its_key() {
    let env = Arc::new(start_env(Opts::default()).await);
    // SyncKey 0: whole hierarchy.
    let (st, key, count, ids) = folder_sync(&env, "fs", "0").await;
    assert_eq!(
        (st.as_str(), key.as_deref(), count.as_deref()),
        ("1", Some("1"), Some("2"))
    );
    assert_eq!(ids, vec!["inbox", "other"]);
    // Same key, nothing changed: Count 0, key unchanged, no re-sent folders.
    let (st, key, count, ids) = folder_sync(&env, "fs", "1").await;
    assert_eq!(
        (st.as_str(), key.as_deref(), count.as_deref()),
        ("1", Some("1"), Some("0"))
    );
    assert!(ids.is_empty());
    // A folder appears on the server: only it is sent, as an Add.
    env.mock
        .extra_folders
        .lock()
        .unwrap()
        .push("Receipts".into());
    let (st, key, count, ids) = folder_sync(&env, "fs", "1").await;
    assert_eq!(
        (st.as_str(), key.as_deref(), count.as_deref()),
        ("1", Some("2"), Some("1"))
    );
    assert_eq!(ids, vec!["Receipts"]);
    // It disappears: sent as a Delete.
    env.mock.extra_folders.lock().unwrap().clear();
    let (st, key, count, ids) = folder_sync(&env, "fs", "2").await;
    assert_eq!(
        (st.as_str(), key.as_deref(), count.as_deref()),
        ("1", Some("3"), Some("1"))
    );
    assert_eq!(ids, vec!["Receipts"]);
    // A key the server never issued: Status 9, client must start over.
    let (st, key, _, _) = folder_sync(&env, "fs", "17").await;
    assert_eq!((st.as_str(), key), ("9", None));
}

#[tokio::test]
async fn folder_sync_for_a_device_partnered_earlier_does_not_force_a_reset() {
    let env = Arc::new(start_env(Opts::default()).await);
    // No stored hierarchy (device set up before this was tracked) and key 5.
    let (st, key, count, ids) = folder_sync(&env, "old", "5").await;
    assert_eq!(
        (st.as_str(), key.as_deref(), count.as_deref()),
        ("1", Some("5"), Some("0"))
    );
    assert!(ids.is_empty());
}

#[tokio::test]
async fn get_item_estimate_reports_real_counts_and_statuses() {
    use eas::{airsync as air, get_item_estimate as gie};
    let env = Arc::new(start_env(Opts::default()).await);
    let gie_body = |key: &str| {
        let mut b = eas::DocumentBuilder::new();
        b.start(gie::GET_ITEM_ESTIMATE);
        b.start(gie::FOLDERS);
        b.start(gie::FOLDER);
        b.leaf(air::SYNC_KEY, key);
        b.leaf(gie::FOLDER_ID, "inbox");
        b.end();
        b.end();
        b.end();
        wbxml::encode_document(&b.finish())
    };
    let estimate = |key: &'static str| {
        let env = env.clone();
        let body = gie_body(key);
        async move {
            let resp = env
                .req("GetItemEstimate", "est")
                .body(body)
                .send()
                .await
                .unwrap();
            let doc = wbxml::decode_document(&resp.bytes().await.unwrap()).unwrap();
            assert!(
                !eas::contains_token(&doc, gie::FOLDER_TYPE),
                "no 12.1-only Class element"
            );
            (
                eas::find_text_after(&doc, gie::STATUS).unwrap().to_owned(),
                eas::find_text_after(&doc, gie::ESTIMATE)
                    .unwrap()
                    .to_owned(),
            )
        }
    };
    assert_eq!(estimate("0").await.0, "3", "not primed");
    // Prime the folder: both messages are still pending.
    let (key1, _, _) = env.sync_detail("est", "0").await;
    assert_eq!(key1, "1");
    assert_eq!(estimate("1").await, ("1".to_owned(), "2".to_owned()));
    let (key2, adds, _) = env.sync_detail("est", "1").await;
    assert_eq!(adds.len(), 2);
    assert_eq!(key2, "2");
    assert_eq!(estimate("2").await, ("1".to_owned(), "0".to_owned()));
    assert_eq!(estimate("9").await.0, "4", "mismatched key");
}

#[tokio::test]
async fn empty_sync_replays_the_cached_request_or_returns_status_13() {
    use eas::airsync as air;
    let env = Arc::new(start_env(Opts::default()).await);
    // Nothing cached yet: Status 13, resend a full request.
    let resp = env.req("Sync", "e1").send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let doc = wbxml::decode_document(&resp.bytes().await.unwrap()).unwrap();
    assert_eq!(eas::find_text_after(&doc, air::STATUS), Some("13"));
    // Full Sync (prime), then an empty Sync re-executes it: since the cached
    // request carries SyncKey 0 the reply is a fresh prime again.
    let (key, _, _) = env.sync_detail("e1", "0").await;
    assert_eq!(key, "1");
    let resp = env.req("Sync", "e1").send().await.unwrap();
    let doc = wbxml::decode_document(&resp.bytes().await.unwrap()).unwrap();
    assert_eq!(
        eas::find_text_after(&doc, air::COLLECTION_ID),
        Some("inbox")
    );
    assert_eq!(
        eas::find_all_text_after(&doc, air::STATUS)
            .first()
            .map(String::as_str),
        Some("1")
    );
}

#[tokio::test]
async fn options_advertises_methods_versions_and_commands() {
    let env = Arc::new(start_env(Opts::default()).await);
    let resp = env
        .http
        .request(reqwest::Method::OPTIONS, &env.gw)
        .basic_auth("u@test", Some("pw"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let h = resp.headers();
    for name in [
        "Allow",
        "Public",
        "MS-ASProtocolVersions",
        "MS-ASProtocolCommands",
        "MS-Server-ActiveSync",
    ] {
        assert!(h.get(name).is_some(), "missing {name}");
    }
    assert_eq!(h.get("Allow").unwrap(), "OPTIONS,POST");
    // Only the version every response is actually shaped for.
    assert_eq!(h.get("MS-ASProtocolVersions").unwrap(), "14.0");
    let commands = h.get("MS-ASProtocolCommands").unwrap().to_str().unwrap();
    assert!(commands
        .split(',')
        .any(|command| command == "ResolveRecipients"));
    assert!(commands
        .split(',')
        .any(|command| command == "MeetingResponse"));
    assert!(commands.split(',').any(|command| command == "ValidateCert"));
    assert!(!commands.split(',').any(|command| command == "Find"));
}

#[tokio::test]
async fn more_available_is_a_bare_tag_without_content() {
    // [MS-ASWBXML]: an empty element is encoded without the content bit.
    use eas::airsync as air;
    let env = Arc::new(start_env(Opts::default()).await);
    for n in 0..12 {
        env.mock.add_mail("inbox", &format!("x{n:02}"));
    }
    let (key, _, _) = env.sync_detail_window("ma", "0", "5").await;
    let resp = env
        .req("Sync", "ma")
        .body(sync_body_window(&key, "inbox", "5"))
        .send()
        .await
        .unwrap();
    let bytes = resp.bytes().await.unwrap();
    let doc = wbxml::decode_document(&bytes).unwrap();
    let mut found = false;
    for (i, node) in doc.nodes.iter().enumerate() {
        if let wbxml::Node::Start(t) = node {
            if t.code_page == air::PAGE && t.token == air::MORE_AVAILABLE.token {
                found = true;
                assert!(
                    !t.has_content,
                    "MoreAvailable must not carry the content bit"
                );
                assert!(!matches!(doc.nodes.get(i + 1), Some(wbxml::Node::End) if false));
            }
        }
    }
    assert!(found, "MoreAvailable present when more messages remain");
}

#[tokio::test]
async fn delete_is_delivered_even_while_the_folder_listing_trails() {
    let env = Arc::new(start_env(Opts::default()).await);
    env.sync_all("phone").await;
    let key = env.keys.lock().unwrap().get("phone").cloned().unwrap();
    // m1 moved out of the Inbox; the listing still shows it for a moment.
    env.mock.remove_mail("inbox", "m1");
    env.mock.add_mail("other", "m1");
    env.mock
        .stale_listing
        .lock()
        .unwrap()
        .push(("inbox".into(), "m1".into()));
    let r = env.ping("phone", Some(ping_body(5, INBOX))).await;
    assert_eq!(r.status, "2");
    use eas::airsync as air;
    let resp = env
        .req("Sync", "phone")
        .body(sync_body(&key, "inbox"))
        .send()
        .await
        .unwrap();
    let doc = wbxml::decode_document(&resp.bytes().await.unwrap()).unwrap();
    assert!(
        eas::contains_token(&doc, air::DELETE),
        "the announced delete is in this Sync"
    );
    assert_eq!(eas::find_all_text_after(&doc, air::SERVER_ID), vec!["m1"]);
}

/// Sends one Sync and returns (reply outline, new SyncKey).
async fn sync_outline(env: &Env, device: &str, key: &str) -> (String, String) {
    use eas::airsync as air;
    let resp = env
        .req("Sync", device)
        .body(sync_body_options(key, "inbox", "25", Some("5")))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let doc = wbxml::decode_document(&resp.bytes().await.unwrap()).unwrap();
    let new_key = eas::find_all_text_after(&doc, air::SYNC_KEY).pop().unwrap();
    (crate::activesync::wbxml_outline_for_tests(&doc), new_key)
}

#[tokio::test]
async fn delete_from_another_device_has_the_exact_spec_shape_and_is_sent_once() {
    // Pins the Delete the receiving iPhone got in production (trace
    // 2026-11-11 23:44:41): Collection children in MS-ASCMD order (SyncKey,
    // CollectionId, Status, Commands), Delete holding only the ServerId it
    // was given as an Add, the key advanced, and nothing after it.
    let env = Arc::new(start_env(Opts::default()).await);
    env.sync_all("iphone").await;
    let key = env.keys.lock().unwrap().get("iphone").cloned().unwrap();
    env.mock.remove_mail("inbox", "m1");
    let (outline, key2) = sync_outline(&env, "iphone", &key).await;
    assert_ne!(key2, key);
    assert_eq!(
        outline,
        format!(
            "<0:05><0:1c><0:0f><0:0b>{key2}</><0:12>inbox</><0:0e>1</>\
             <0:16><0:09><0:0d>m1</></></></></></>"
        )
    );
    let (outline, key3) = sync_outline(&env, "iphone", &key2).await;
    assert_eq!(key3, key2, "nothing more to send");
    assert!(!outline.contains("<0:16>"), "{outline}");
}

#[tokio::test]
async fn adds_carry_the_server_read_flag() {
    // The badge counts what the device stores; every Add must state Read.
    let env = Arc::new(start_env(Opts::default()).await);
    env.mock.set_read("m1", true);
    let (outline, key) = sync_outline(&env, "ipad", "0").await;
    assert!(
        !outline.contains("<0:16>"),
        "SyncKey 0 reply has no Commands: {outline}"
    );
    let (outline, _) = sync_outline(&env, "ipad", &key).await;
    let read_of = |id: &str| {
        let at = outline.find(&format!("<0:0d>{id}</>")).expect(id);
        let rest = &outline[at..];
        let end = rest[1..]
            .find("<0:07>")
            .map(|e| e + 1)
            .unwrap_or(rest.len());
        let add = &rest[..end];
        assert_eq!(add.matches("<2:15>").count(), 1, "one Read per Add: {add}");
        add[add.find("<2:15>").unwrap() + 6..]
            .chars()
            .next()
            .unwrap()
    };
    assert_eq!(read_of("m1"), '1');
    assert_eq!(read_of("m2"), '0');
}

#[test]
fn wbxml_outline_shows_protocol_fields_but_not_content() {
    use eas::airsync as air;
    let mut b = eas::DocumentBuilder::new();
    b.start(air::SYNC);
    b.leaf(air::SYNC_KEY, "7");
    b.start(air::COMMANDS);
    b.start(air::DELETE);
    b.leaf(air::SERVER_ID, "m1");
    b.end();
    b.end();
    b.leaf(eas::email::SUBJECT, "secret subject");
    b.empty_tag(air::MORE_AVAILABLE);
    b.end();
    let outline = crate::activesync::wbxml_outline_for_tests(&b.finish());
    assert!(outline.contains("<0:0b>7</>"), "{outline}");
    assert!(outline.contains("<0:09><0:0d>m1</></>"), "{outline}");
    assert!(!outline.contains("secret"), "{outline}");
    assert!(outline.contains("<0:14/>"), "{outline}");
}

#[tokio::test]
async fn a_resent_previous_synckey_gets_the_same_reply_not_a_reset() {
    use eas::airsync as air;
    let env = Arc::new(start_env(Opts::default()).await);
    env.sync_all("phone").await;
    let key = env.keys.lock().unwrap().get("phone").cloned().unwrap();
    env.mock.add_mail("inbox", "m3");
    let first = env
        .req("Sync", "phone")
        .body(sync_body(&key, "inbox"))
        .send()
        .await
        .unwrap();
    let first = wbxml::decode_document(&first.bytes().await.unwrap()).unwrap();
    let new_key = eas::find_all_text_after(&first, air::SYNC_KEY)
        .pop()
        .unwrap();
    assert_ne!(new_key, key);
    // The HTTP response was lost: the phone sends the same request again.
    let again = env
        .req("Sync", "phone")
        .body(sync_body(&key, "inbox"))
        .send()
        .await
        .unwrap();
    let again = wbxml::decode_document(&again.bytes().await.unwrap()).unwrap();
    assert_eq!(again, first, "identical reply, not Status 3");
    let changed = env
        .req("Sync", "phone")
        .body(flag_change_body(
            &key,
            "inbox",
            "m1",
            None,
            Some(true),
            false,
        ))
        .send()
        .await
        .unwrap();
    let changed = wbxml::decode_document(&changed.bytes().await.unwrap()).unwrap();
    assert_eq!(eas::find_text_after(&changed, air::STATUS), Some("3"));
    assert!(env
        .mock
        .keywords
        .lock()
        .unwrap()
        .get("m1")
        .is_none_or(|keywords| !keywords.contains("$seen")));
    // And it continues normally from the new key.
    let (key3, adds, _) = env.sync_detail("phone", &new_key).await;
    assert!(adds.is_empty());
    assert_eq!(key3, new_key);
}

#[tokio::test]
async fn a_folder_that_no_longer_exists_gets_status_12_not_mass_deletes() {
    use eas::airsync as air;
    let env = Arc::new(start_env(Opts::default()).await);
    env.sync_all("phone").await;
    let key = env.keys.lock().unwrap().get("phone").cloned().unwrap();
    env.mock.remove_mail("inbox", "m1");
    env.mock.remove_mail("inbox", "m2");
    env.mock.hidden_folders.lock().unwrap().push("inbox".into());
    let resp = env
        .req("Sync", "phone")
        .body(sync_body(&key, "inbox"))
        .send()
        .await
        .unwrap();
    let doc = wbxml::decode_document(&resp.bytes().await.unwrap()).unwrap();
    assert_eq!(eas::find_all_text_after(&doc, air::STATUS), vec!["12"]);
    assert!(
        !eas::contains_token(&doc, air::DELETE),
        "no Delete for every message"
    );
}

/// End-to-end tests against a REAL Stalwart through a REAL running gateway.
/// Ignored by default; run with the E2E_* environment set, e.g.
///   E2E_GATEWAY=http://127.0.0.1:18099/Microsoft-Server-ActiveSync
///   E2E_JMAP=http://127.0.0.1:18080 E2E_USER=alice@e2e.test E2E_PASS=...
///   cargo test e2e -- --ignored --test-threads=1
mod e2e {
    use super::*;
    use eas::airsync as air;

    struct Live {
        gw: String,
        jmap: String,
        user: String,
        pass: String,
        http: reqwest::Client,
        account: String,
        inbox: String,
    }

    fn env(name: &str) -> String {
        std::env::var(name).unwrap_or_else(|_| panic!("set {name}"))
    }

    impl Live {
        async fn new(user_var: &str) -> Live {
            let jmap = env("E2E_JMAP");
            let user = env(user_var);
            let pass = env("E2E_PASS");
            let http = reqwest::Client::new();
            let session: serde_json::Value = http
                .get(format!("{jmap}/jmap/session"))
                .basic_auth(&user, Some(&pass))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            let account = session["primaryAccounts"]["urn:ietf:params:jmap:mail"]
                .as_str()
                .unwrap()
                .to_owned();
            let mut live = Live {
                gw: env("E2E_GATEWAY"),
                jmap,
                user,
                pass,
                http,
                account,
                inbox: String::new(),
            };
            let boxes = live
                .call(serde_json::json!([["Mailbox/get", {"accountId": live.account}, "0"]]))
                .await;
            live.inbox = boxes[0][1]["list"]
                .as_array()
                .unwrap()
                .iter()
                .find(|m| m["role"] == "inbox")
                .unwrap()["id"]
                .as_str()
                .unwrap()
                .to_owned();
            live
        }

        async fn call(&self, calls: serde_json::Value) -> serde_json::Value {
            self.call_using(
                calls,
                &["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
            )
            .await
        }

        async fn call_using(&self, calls: serde_json::Value, using: &[&str]) -> serde_json::Value {
            let body = serde_json::json!({
                "using": using,
                "methodCalls": calls
            });
            let r: serde_json::Value = self
                .http
                .post(format!("{}/jmap/", self.jmap))
                .basic_auth(&self.user, Some(&self.pass))
                .json(&body)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            r["methodResponses"].clone()
        }

        /// Create `count` messages in `mailbox`; returns their JMAP ids. Plain
        /// messages go through `Email/set` with inline bodies (no blob uploads:
        /// Stalwart caps temporary uploads at 1000 files per account); messages
        /// with an attachment share one uploaded blob.
        async fn seed(
            &self,
            mailbox: &str,
            count: usize,
            tag: &str,
            attachment: bool,
        ) -> Vec<String> {
            let blob = if attachment {
                let up: serde_json::Value = self
                    .http
                    .post(format!("{}/jmap/upload/{}/", self.jmap, self.account))
                    .basic_auth(&self.user, Some(&self.pass))
                    .header("Content-Type", "application/octet-stream")
                    .body(vec![0u8, 1, 2, 3, 4, 5, 6, 7, 8, 9])
                    .send()
                    .await
                    .unwrap()
                    .json()
                    .await
                    .unwrap();
                up["blobId"].as_str().expect("upload blobId").to_owned()
            } else {
                String::new()
            };
            let mut ids = Vec::new();
            let mut batch = serde_json::Map::new();
            for n in 0..count {
                let mut email = serde_json::json!({
                    "mailboxIds": {mailbox: true},
                    "keywords": {},
                    "from": [{"email": "e2e@e2e.test"}],
                    "to": [{"email": self.user}],
                    "subject": format!("{tag} {n}"),
                    "messageId": [format!("{tag}-{n}@e2e.test")],
                    "bodyValues": {"b": {"value": format!("body {n}")}},
                    "textBody": [{"partId": "b", "type": "text/plain"}]
                });
                if attachment {
                    email["attachments"] = serde_json::json!([{
                        "blobId": blob, "type": "application/octet-stream",
                        "name": "blob.bin", "disposition": "attachment"
                    }]);
                }
                batch.insert(format!("k{n}"), email);
                if batch.len() == 200 || n + 1 == count {
                    let r = self
                        .call(serde_json::json!([["Email/set", {"accountId": self.account, "create": batch}, "0"]]))
                        .await;
                    let created = r[0][1]["created"]
                        .as_object()
                        .unwrap_or_else(|| panic!("Email/set failed: {}", r[0]));
                    if let Some(bad) = r[0][1]["notCreated"].as_object().filter(|m| !m.is_empty()) {
                        panic!("Email/set notCreated: {bad:?}");
                    }
                    for (_, v) in created {
                        ids.push(v["id"].as_str().unwrap().to_owned());
                    }
                    batch = serde_json::Map::new();
                }
            }
            ids
        }

        fn req(&self, cmd: &str, device: &str) -> reqwest::RequestBuilder {
            self.req_version(cmd, device, "14.0")
        }

        fn req_version(&self, cmd: &str, device: &str, version: &str) -> reqwest::RequestBuilder {
            let user = self.user.split('@').next().unwrap().to_owned();
            self.http
                .post(format!(
                    "{}?Cmd={cmd}&User={user}&DeviceId={device}&DeviceType=iPhone",
                    self.gw
                ))
                .basic_auth(&self.user, Some(&self.pass))
                .header("MS-ASProtocolVersion", version)
        }

        async fn wbxml(&self, cmd: &str, device: &str, body: Vec<u8>) -> wbxml::Document {
            self.wbxml_version(cmd, device, body, "14.0").await
        }

        async fn wbxml_version(
            &self,
            cmd: &str,
            device: &str,
            body: Vec<u8>,
            version: &str,
        ) -> wbxml::Document {
            let resp = self
                .req_version(cmd, device, version)
                .body(body)
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 200, "{cmd} HTTP status");
            wbxml::decode_document(&resp.bytes().await.unwrap()).unwrap()
        }

        /// Full initial sync of the Inbox with `window`, following MoreAvailable.
        /// Returns (final key, every ServerId added, rounds).
        async fn initial_sync(&self, device: &str, window: &str) -> (String, Vec<String>, usize) {
            let doc = self
                .wbxml("Sync", device, sync_body_window("0", &self.inbox, window))
                .await;
            assert!(
                !eas::contains_token(&doc, air::COMMANDS),
                "no items on SyncKey 0"
            );
            let mut key = eas::find_all_text_after(&doc, air::SYNC_KEY).pop().unwrap();
            let mut all = Vec::new();
            let mut rounds = 0;
            loop {
                rounds += 1;
                let doc = self
                    .wbxml("Sync", device, sync_body_window(&key, &self.inbox, window))
                    .await;
                assert_eq!(
                    eas::find_all_text_after(&doc, air::STATUS)
                        .first()
                        .map(String::as_str),
                    Some("1")
                );
                key = eas::find_all_text_after(&doc, air::SYNC_KEY).pop().unwrap();
                all.extend(eas::find_all_text_after(&doc, air::SERVER_ID));
                if !eas::contains_token(&doc, air::MORE_AVAILABLE) {
                    break;
                }
                assert!(rounds < 10_000);
            }
            (key, all, rounds)
        }

        async fn ping(&self, device: &str, hb: u64) -> (String, Vec<String>) {
            let doc = self
                .wbxml(
                    "Ping",
                    device,
                    ping_body(hb, &[(self.inbox.as_str(), "Email")]),
                )
                .await;
            (
                eas::find_text_after(&doc, eas::ping::STATUS)
                    .unwrap_or("")
                    .to_owned(),
                eas::find_all_text_after(&doc, eas::ping::FOLDER),
            )
        }

        async fn is_read(&self, id: &str) -> bool {
            let r = self
                .call(serde_json::json!([["Email/get", {"accountId": self.account, "ids": [id], "properties": ["keywords"]}, "0"]]))
                .await;
            r[0][1]["list"][0]["keywords"]["$seen"]
                .as_bool()
                .unwrap_or(false)
        }

        async fn inbox_count(&self) -> usize {
            let r = self
                .call(serde_json::json!([["Email/query", {"accountId": self.account, "filter": {"inMailbox": self.inbox}, "calculateTotal": true, "limit": 1}, "0"]]))
                .await;
            r[0][1]["total"].as_u64().unwrap() as usize
        }
    }

    fn dev(name: &str) -> String {
        format!("{name}{}", std::process::id())
    }

    impl Live {
        /// Create messages with given receivedAt dates; returns their ids.
        async fn seed_dated(&self, items: &[(&str, String)]) -> Vec<String> {
            let mut batch = serde_json::Map::new();
            for (n, (tag, at)) in items.iter().enumerate() {
                batch.insert(
                    format!("k{n}"),
                    serde_json::json!({
                        "mailboxIds": {self.inbox.clone(): true},
                        "keywords": {},
                        "receivedAt": at,
                        "from": [{"email": "e2e@e2e.test"}],
                        "to": [{"email": self.user}],
                        "subject": tag,
                        "bodyValues": {"b": {"value": "body"}},
                        "textBody": [{"partId": "b", "type": "text/plain"}]
                    }),
                );
            }
            let r = self
                .call(serde_json::json!([["Email/set", {"accountId": self.account, "create": batch}, "0"]]))
                .await;
            (0..items.len())
                .map(|n| {
                    r[0][1]["created"][format!("k{n}")]["id"]
                        .as_str()
                        .expect("created")
                        .to_owned()
                })
                .collect()
        }

        async fn in_inbox(&self, id: &str) -> bool {
            let r = self
                .call(serde_json::json!([["Email/get", {"accountId": self.account, "ids": [id], "properties": ["mailboxIds"]}, "0"]]))
                .await;
            r[0][1]["list"][0]["mailboxIds"][self.inbox.as_str()].as_bool() == Some(true)
        }

        /// One Sync with a FilterType: (key, SoftDelete ids, Delete ids, Add ids).
        async fn sync_filter(
            &self,
            device: &str,
            key: &str,
            filter: &str,
        ) -> (String, Vec<String>, Vec<String>, Vec<String>) {
            let doc = self
                .wbxml(
                    "Sync",
                    device,
                    sync_body_options(key, &self.inbox, "100", Some(filter)),
                )
                .await;
            let outline = crate::activesync::wbxml_outline_for_tests(&doc);
            (
                eas::find_all_text_after(&doc, air::SYNC_KEY).pop().unwrap(),
                super::ids_after(&outline, "<0:21>"),
                super::ids_after(&outline, "<0:09>"),
                super::ids_after(&outline, "<0:07>"),
            )
        }
    }

    #[tokio::test]
    #[ignore]
    async fn e2e_mail_older_than_the_filter_window_leaves_the_device_not_the_server() {
        let live = Live::new("E2E_USER2").await;
        let ids = live
            .seed_dated(&[
                ("aged 45d", super::days_ago(45)),
                ("aged 400d", super::days_ago(400)),
                ("recent 2d", super::days_ago(2)),
            ])
            .await;
        let (old45, old400, recent) = (&ids[0], &ids[1], &ids[2]);
        let d = dev("aged");
        // Held from an all-mail Sync, as the iPad's pre-12m copy is.
        let (k0, _, _, _) = live.sync_filter(&d, "0", "0").await;
        let mut key = k0;
        loop {
            let (k, _, _, _) = live.sync_filter(&d, &key, "0").await;
            if k == key {
                break;
            }
            key = k;
        }
        let (k1, soft, del, adds) = live.sync_filter(&d, &key, "5").await;
        assert!(soft.contains(old45) && soft.contains(old400), "{soft:?}");
        assert!(
            !soft.contains(recent) && del.is_empty() && adds.is_empty(),
            "{soft:?} {del:?} {adds:?}"
        );
        assert!(
            live.in_inbox(old45).await && live.in_inbox(old400).await,
            "still on the server"
        );
        let (k2, soft2, del2, _) = live.sync_filter(&d, &k1, "5").await;
        assert!(
            soft2.is_empty() && del2.is_empty() && k2 == k1,
            "not repeated"
        );
        if let Ok(cmd) = std::env::var("E2E_RESTART_CMD") {
            assert!(std::process::Command::new(cmd).status().unwrap().success());
            let live = Live::new("E2E_USER2").await;
            let (k3, soft3, del3, _) = live.sync_filter(&d, &k2, "5").await;
            assert!(
                soft3.is_empty() && del3.is_empty() && k3 == k2,
                "not repeated after restart"
            );
        }
        // Widening the window brings them back.
        let (_, _, _, adds) = live.sync_filter(&d, &k2, "0").await;
        assert!(adds.contains(old45) && adds.contains(old400), "{adds:?}");
    }

    #[tokio::test]
    #[ignore]
    async fn e2e_empty_mailbox() {
        let live = Live::new("E2E_USER2").await;
        let (_, adds, _) = live.initial_sync(&dev("empty"), "100").await;
        assert_eq!(adds.len(), live.inbox_count().await);
    }

    #[tokio::test]
    #[ignore]
    async fn e2e_initial_sync_delivers_every_message_exactly_once() {
        let live = Live::new("E2E_USER").await;
        let want: usize = std::env::var("E2E_N")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(100);
        let have = live.inbox_count().await;
        if have < want {
            live.seed(&live.inbox, want - have, "bulk", false).await;
        }
        let total = live.inbox_count().await;
        let t = Instant::now();
        let (_, mut adds, rounds) = live.initial_sync(&dev("bulk"), "100").await;
        let n = adds.len();
        adds.sort();
        adds.dedup();
        assert_eq!(adds.len(), n, "no duplicates");
        assert_eq!(n, total, "every message in the Inbox delivered");
        eprintln!(
            "initial sync: {total} messages in {rounds} rounds, {:?}",
            t.elapsed()
        );
    }

    #[tokio::test]
    #[ignore]
    async fn e2e_new_mail_during_ping_then_read_move_and_two_devices() {
        let live = Live::new("E2E_USER").await;
        let (a, b) = (dev("pa"), dev("pb"));
        let (ka, _, _) = live.initial_sync(&a, "100").await;
        let (kb, _, _) = live.initial_sync(&b, "100").await;
        assert_eq!(live.ping(&a, 1).await.0, "1", "synced device is quiet");

        // New mail during a long Ping, two devices at once.
        let la = Arc::new(Live::new("E2E_USER").await);
        let lb = la.clone();
        let (da, db) = (a.clone(), b.clone());
        let pa = tokio::spawn(async move { la.ping(&da, 60).await });
        let pb = tokio::spawn(async move { lb.ping(&db, 60).await });
        tokio::time::sleep(Duration::from_secs(2)).await;
        let t = Instant::now();
        let new_id = live.seed(&live.inbox, 1, "push", false).await.remove(0);
        let (sa, fa) = pa.await.unwrap();
        let (sb, _) = pb.await.unwrap();
        assert_eq!((sa.as_str(), sb.as_str()), ("2", "2"), "both devices woken");
        assert_eq!(fa, vec![live.inbox.clone()]);
        eprintln!("new mail -> both Pings answered in {:?}", t.elapsed());
        let doc = live.wbxml("Sync", &a, sync_body(&ka, &live.inbox)).await;
        let ka = eas::find_all_text_after(&doc, air::SYNC_KEY).pop().unwrap();
        assert_eq!(
            eas::find_all_text_after(&doc, air::SERVER_ID),
            vec![new_id.clone()]
        );
        let doc = live.wbxml("Sync", &b, sync_body(&kb, &live.inbox)).await;
        let kb = eas::find_all_text_after(&doc, air::SYNC_KEY).pop().unwrap();
        assert_eq!(
            eas::find_all_text_after(&doc, air::SERVER_ID),
            vec![new_id.clone()]
        );

        // Read elsewhere -> Change Read=1 to the device.
        live.call(serde_json::json!([["Email/set", {"accountId": live.account, "update": {&new_id: {"keywords/$seen": true}}}, "0"]])).await;
        assert_eq!(
            live.ping(&a, 20).await.0,
            "2",
            "read change wakes the device"
        );
        let doc = live.wbxml("Sync", &a, sync_body(&ka, &live.inbox)).await;
        let ka = eas::find_all_text_after(&doc, air::SYNC_KEY).pop().unwrap();
        assert!(eas::contains_token(&doc, air::CHANGE), "Change sent");
        assert_eq!(eas::find_text_after(&doc, eas::email::READ), Some("1"));

        // Moved out of the Inbox elsewhere -> Delete with the same ServerId.
        let other = live.call(serde_json::json!([["Mailbox/set", {"accountId": live.account, "create": {"m": {"name": format!("E2E move {}", std::process::id())}}}, "0"]])).await;
        let other_id = other[0][1]["created"]["m"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        live.call(serde_json::json!([["Email/set", {"accountId": live.account, "update": {&new_id: {"mailboxIds": {&other_id: true}}}}, "0"]])).await;
        assert_eq!(live.ping(&a, 20).await.0, "2", "move wakes the device");
        let doc = live.wbxml("Sync", &a, sync_body(&ka, &live.inbox)).await;
        assert!(
            eas::contains_token(&doc, air::DELETE),
            "Delete sent for the moved message"
        );
        assert_eq!(
            eas::find_all_text_after(&doc, air::SERVER_ID),
            vec![new_id.clone()]
        );
        // Second device gets the same Delete.
        let _ = kb;
    }

    #[tokio::test]
    #[ignore]
    async fn e2e_sync_heartbeat_delivers_new_stalwart_mail() {
        let live = Live::new("E2E_USER2").await;
        let result = live.call(serde_json::json!([["Mailbox/set", {
            "accountId": live.account, "create": {"m":{"name":format!("Sync wait {}",std::process::id())}}
        }, "0"]])).await;
        let folder = result[0][1]["created"]["m"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let device = dev("sync-wait-native");
        let prime = live
            .wbxml_version("Sync", &device, sync_body("0", &folder), "16.1")
            .await;
        let synced = live
            .wbxml_version(
                "Sync",
                &device,
                sync_body(&sync_key(&prime), &folder),
                "16.1",
            )
            .await;
        let mut b = eas::DocumentBuilder::new();
        b.start(air::SYNC);
        b.leaf(air::HEARTBEAT_INTERVAL, "60");
        b.start(air::COLLECTIONS);
        b.start(air::COLLECTION);
        b.leaf(air::SYNC_KEY, sync_key(&synced));
        b.leaf(air::COLLECTION_ID, &folder);
        b.empty_tag(air::GET_CHANGES);
        b.end();
        b.end();
        b.end();
        let request = live
            .req_version("Sync", &device, "16.1")
            .body(wbxml::encode_document(&b.finish()))
            .send();
        tokio::pin!(request);
        tokio::select! {
            response = &mut request => panic!("Sync returned before mail arrived: {:?}",response.unwrap().status()),
            _ = tokio::time::sleep(Duration::from_millis(700)) => {}
        }
        let ids = live.seed(&folder, 1, "sync-wait", false).await;
        let response = tokio::time::timeout(Duration::from_secs(10), &mut request)
            .await
            .unwrap()
            .unwrap();
        let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
        assert_eq!(eas::find_all_text_after(&doc, air::SERVER_ID), ids);
        let mut b = eas::DocumentBuilder::new();
        b.start(air::SYNC);
        b.leaf(air::HEARTBEAT_INTERVAL, "60");
        b.start(air::COLLECTIONS);
        b.start(air::COLLECTION);
        b.leaf(air::SYNC_KEY, sync_key(&doc));
        b.leaf(air::COLLECTION_ID, &folder);
        b.empty_tag(air::GET_CHANGES);
        b.end();
        b.end();
        b.end();
        let started = std::time::Instant::now();
        let response = tokio::time::timeout(
            Duration::from_secs(70),
            live.req_version("Sync", &device, "16.1")
                .body(wbxml::encode_document(&b.finish()))
                .send(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(started.elapsed() >= Duration::from_secs(59));
        assert!(response.bytes().await.unwrap().is_empty());
        let result = live
            .call(serde_json::json!([
                ["Email/set",{"accountId":live.account,"destroy":ids},"0"],
                ["Mailbox/set",{"accountId":live.account,"destroy":[folder]},"1"]
            ]))
            .await;
        assert!(result[0][1]["notDestroyed"].is_null());
        assert!(result[1][1]["notDestroyed"].is_null());
    }

    #[tokio::test]
    #[ignore]
    async fn e2e_jmap_item_error_does_not_acknowledge_sync_change() {
        use eas::email as mail;
        let live = Live::new("E2E_USER2").await;
        let result = live.call(serde_json::json!([["Mailbox/set",{
            "accountId":live.account,"create":{"m":{"name":format!("Mutation failure {}",std::process::id())}}
        },"0"]])).await;
        let folder = result[0][1]["created"]["m"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let ids = live.seed(&folder, 1, "mutation-failure", false).await;
        let device = dev("mutation-failure");
        let prime = live
            .wbxml_version("Sync", &device, sync_body("0", &folder), "16.1")
            .await;
        let synced = live
            .wbxml_version(
                "Sync",
                &device,
                sync_body(&sync_key(&prime), &folder),
                "16.1",
            )
            .await;
        let key = sync_key(&synced);
        let destroyed = live
            .call(serde_json::json!([["Email/set",{
            "accountId":live.account,"destroy":ids
        },"0"]]))
            .await;
        assert!(destroyed[0][1]["notDestroyed"].is_null());
        let rejected = live
            .call(serde_json::json!([["Email/set",{
            "accountId":live.account,"update":{&ids[0]:{"keywords/$seen":true}}
        },"0"]]))
            .await;
        assert_eq!(rejected[0][1]["notUpdated"][&ids[0]]["type"], "notFound");
        let mut b = eas::DocumentBuilder::new();
        b.start(air::SYNC);
        b.start(air::COLLECTIONS);
        b.start(air::COLLECTION);
        b.leaf(air::SYNC_KEY, &key);
        b.leaf(air::COLLECTION_ID, &folder);
        b.leaf(air::GET_CHANGES, "0");
        b.start(air::COMMANDS);
        b.start(air::CHANGE);
        b.leaf(air::SERVER_ID, &ids[0]);
        b.start(air::APPLICATION_DATA);
        b.leaf(mail::READ, "1");
        b.end();
        for _ in 0..5 {
            b.end();
        }
        let doc = live
            .wbxml_version("Sync", &device, wbxml::encode_document(&b.finish()), "16.1")
            .await;
        assert_eq!(eas::find_text_after(&doc, air::STATUS), Some("5"));
        assert_eq!(sync_key(&doc), key);
        let result = live
            .call(serde_json::json!([["Mailbox/set",{
            "accountId":live.account,"destroy":[folder]
        },"0"]]))
            .await;
        assert!(result[0][1]["notDestroyed"].is_null());
    }

    #[tokio::test]
    #[ignore]
    async fn e2e_find_received_date_boundaries_match_stalwart() {
        use eas::find as f;
        let live = Live::new("E2E_USER2").await;
        let device = dev("find-received");
        let tag = format!("datefixture{}", std::process::id());
        let result = live
            .call(serde_json::json!([["Mailbox/set",{
            "accountId":live.account,"create":{"m":{"name":&tag}}
        },"0"]]))
            .await;
        let folder = result[0][1]["created"]["m"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let mut create = serde_json::Map::new();
        for (i, received) in [
            "2024-02-28T23:59:59Z",
            "2024-02-29T00:00:00Z",
            "2024-02-29T23:59:59Z",
            "2024-03-01T00:00:00Z",
        ]
        .iter()
        .enumerate()
        {
            create.insert(
                format!("m{i}"),
                serde_json::json!({
                    "mailboxIds":{&folder:true},"keywords":{},"receivedAt":received,
                    "subject":&tag,"bodyValues":{"b":{"value":"test"}},
                    "textBody":[{"partId":"b","type":"text/plain"}]
                }),
            );
        }
        let result = live
            .call(serde_json::json!([["Email/set",{
            "accountId":live.account,"create":create
        },"0"]]))
            .await;
        let ids: Vec<_> = result[0][1]["created"]
            .as_object()
            .unwrap()
            .values()
            .map(|item| item["id"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(ids.len(), 4);
        let fetched = live
            .call(serde_json::json!([["Email/get",{
            "accountId":live.account,"ids":ids,"properties":["id","receivedAt","size"]
        },"0"]]))
            .await;
        let dates: Vec<_> = fetched[0][1]["list"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["receivedAt"].as_str().unwrap()[..10].to_owned())
            .collect();
        assert_eq!(
            dates
                .iter()
                .filter(|date| date.as_str() == "2024-02-29")
                .count(),
            2
        );
        for (operator, expected) in [
            ("=", 2),
            (">", 1),
            (">=", 3),
            ("<", 1),
            ("<=", 3),
            ("<>", 2),
        ] {
            let search_id = uuid::Uuid::new_v4().to_string();
            let response = live
                .req_version("Find", &device, "16.1")
                .body(find_body(
                    &search_id,
                    &format!("subject:{tag} received{operator}2024-02-29"),
                    Some(&folder),
                    "0-9",
                ))
                .send()
                .await
                .unwrap();
            let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
            assert_eq!(eas::find_all_text_after(&doc, f::STATUS), vec!["1", "1"]);
            assert_eq!(
                eas::find_text_after(&doc, f::TOTAL)
                    .unwrap()
                    .parse::<usize>()
                    .unwrap(),
                expected,
                "{operator}"
            );
        }
        let response = live
            .req_version("Find", &device, "16.1")
            .body(find_body(
                &uuid::Uuid::new_v4().to_string(),
                &format!("subject:{tag} received:2024-02-29..2024-03-01"),
                Some(&folder),
                "0-9",
            ))
            .send()
            .await
            .unwrap();
        let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
        assert_eq!(eas::find_all_text_after(&doc, f::STATUS), vec!["1", "1"]);
        assert_eq!(eas::find_text_after(&doc, f::TOTAL), Some("3"));
        let response = live
            .req_version("Find", &device, "16.1")
            .body(find_body(
                &uuid::Uuid::new_v4().to_string(),
                &format!("subject:{tag} received:2024-02-29T12:34:56"),
                Some(&folder),
                "0-9",
            ))
            .send()
            .await
            .unwrap();
        let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
        assert_eq!(eas::find_all_text_after(&doc, f::STATUS), vec!["1", "1"]);
        assert_eq!(eas::find_text_after(&doc, f::TOTAL), Some("2"));
        let sizes: Vec<_> = fetched[0][1]["list"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["size"].as_u64().unwrap())
            .collect();
        let smallest = *sizes.iter().min().unwrap();
        let expected = sizes.iter().filter(|size| **size == smallest).count();
        let response = live
            .req_version("Find", &device, "16.1")
            .body(find_body(
                &uuid::Uuid::new_v4().to_string(),
                &format!("subject:{tag} size:{smallest}..{smallest}"),
                Some(&folder),
                "0-9",
            ))
            .send()
            .await
            .unwrap();
        let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
        assert_eq!(eas::find_all_text_after(&doc, f::STATUS), vec!["1", "1"]);
        assert_eq!(
            eas::find_text_after(&doc, f::TOTAL)
                .unwrap()
                .parse::<usize>()
                .unwrap(),
            expected
        );
        let result = live
            .call(serde_json::json!([
                ["Email/set",{"accountId":live.account,"destroy":ids},"0"],
                ["Mailbox/set",{"accountId":live.account,"destroy":[folder]},"1"]
            ]))
            .await;
        assert!(result[0][1]["notDestroyed"].is_null());
        assert!(result[1][1]["notDestroyed"].is_null());
    }

    #[tokio::test]
    #[ignore]
    async fn e2e_find_gal_matches_principal_prefixes() {
        use eas::{find as f, gal};
        let live = Live::new("E2E_USER2").await;
        let device = dev("findgal");
        let user = std::env::var("E2E_USER").unwrap();
        let prefix: String = user.chars().take(4).collect();
        assert_eq!(prefix.chars().count(), 4);
        let sid = uuid::Uuid::new_v4().to_string();
        let search = |term: &str, range: &str| {
            live.req_version("Find", &device, "16.1")
                .body(gal_find_body(&sid, term, range))
                .send()
        };
        let response = search(&prefix.to_uppercase(), "0-0").await.unwrap();
        let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
        assert_eq!(eas::find_all_text_after(&doc, f::STATUS), vec!["1", "1"]);
        assert!(eas::find_all_text_after(&doc, gal::EMAIL_ADDRESS).contains(&user));
        assert_eq!(eas::find_text_after(&doc, f::RANGE), Some("0-0"));
        let response = search("nonexistentdirectoryprefix", "1-2").await.unwrap();
        let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
        assert_eq!(eas::find_all_text_after(&doc, f::STATUS), vec!["1", "4"]);
        let sid2 = uuid::Uuid::new_v4().to_string();
        let response = live
            .req_version("Find", &device, "16.1")
            .body(gal_find_body(&sid2, "nonexistentdirectoryprefix", "0-9"))
            .send()
            .await
            .unwrap();
        let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
        assert_eq!(eas::find_text_after(&doc, f::TOTAL), Some("0"));
    }

    #[tokio::test]
    #[ignore]
    async fn e2e_find_16_1_reads_native_jmap_metadata_and_keywords() {
        use eas::{airsync_base as base, email2, find as f};
        let live = Live::new("E2E_USER2").await;
        let device = dev("find16");
        let tag = format!("findinterop{}", std::process::id());
        let r = live
            .call(serde_json::json!([["Mailbox/set", {
            "accountId": live.account, "create": {"m": {"name": &tag}}
        }, "0"]]))
            .await;
        let folder = r[0][1]["created"]["m"]["id"].as_str().unwrap().to_owned();
        let ids = live.seed(&folder, 3, &tag, true).await;
        let r = live.call(serde_json::json!([["Email/set", {
            "accountId": live.account,
            "update": {&ids[0]: {"keywords/$flagged": true, "keywords/$seen": true, "keywords/$draft": true}}
        }, "0"]])).await;
        assert!(r[0][1]["updated"].get(&ids[0]).is_some());
        let search_id = uuid::Uuid::new_v4().to_string();
        let send = |text: String, range: &str| {
            live.req_version("Find", &device, "16.1")
                .body(find_body(&search_id, &text, Some(&folder), range))
                .send()
        };
        let response = send(format!("subject:{tag}"), "0-0").await.unwrap();
        assert_eq!(response.status(), 200);
        let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
        assert_eq!(eas::find_all_text_after(&doc, f::STATUS), vec!["1", "1"]);
        assert_eq!(eas::find_text_after(&doc, f::TOTAL), Some("3"));
        assert_eq!(eas::find_text_after(&doc, f::RANGE), Some("0-0"));
        let mut found = eas::find_all_text_after(&doc, air::SERVER_ID);
        let response = send(format!("subject:{tag}"), "1-2").await.unwrap();
        let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
        assert_eq!(eas::find_all_text_after(&doc, f::STATUS), vec!["1", "1"]);
        found.extend(eas::find_all_text_after(&doc, air::SERVER_ID));
        found.sort();
        let mut expected = ids.clone();
        expected.sort();
        assert_eq!(found, expected);
        assert!(!eas::contains_token(&doc, base::BODY));

        let response = send(format!("subject:{tag} isflagged:true isread:true"), "0-9")
            .await
            .unwrap();
        let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
        assert_eq!(eas::find_all_text_after(&doc, f::STATUS), vec!["1", "1"]);
        assert_eq!(eas::find_text_after(&doc, f::TOTAL), Some("1"));
        assert_eq!(
            eas::find_all_text_after(&doc, air::SERVER_ID),
            vec![ids[0].clone()]
        );
        assert_eq!(eas::find_text_after(&doc, email2::IS_DRAFT), Some("1"));
        assert_eq!(eas::find_text_after(&doc, f::HAS_ATTACHMENTS), Some("1"));
        assert!(!eas::contains_token(&doc, base::BODY));

        let response = send(format!("subject:\"{tag} 0\""), "0-9").await.unwrap();
        let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
        assert_eq!(eas::find_all_text_after(&doc, f::STATUS), vec!["2"]);

        let sizes = live
            .call(serde_json::json!([["Email/get", {
            "accountId": live.account, "ids": ids, "properties": ["id", "size", "receivedAt"]
        }, "0"]]))
            .await;
        let entries = sizes[0][1]["list"].as_array().unwrap();
        let size = entries[0]["size"].as_u64().unwrap();
        for (operator, expected) in [
            (
                "=",
                entries
                    .iter()
                    .filter(|e| e["size"].as_u64().unwrap() == size)
                    .count(),
            ),
            (
                ">",
                entries
                    .iter()
                    .filter(|e| e["size"].as_u64().unwrap() > size)
                    .count(),
            ),
            (
                ">=",
                entries
                    .iter()
                    .filter(|e| e["size"].as_u64().unwrap() >= size)
                    .count(),
            ),
            (
                "<",
                entries
                    .iter()
                    .filter(|e| e["size"].as_u64().unwrap() < size)
                    .count(),
            ),
            (
                "<=",
                entries
                    .iter()
                    .filter(|e| e["size"].as_u64().unwrap() <= size)
                    .count(),
            ),
            (
                "<>",
                entries
                    .iter()
                    .filter(|e| e["size"].as_u64().unwrap() != size)
                    .count(),
            ),
        ] {
            let response = send(format!("subject:{tag} size{operator}{size}"), "0-9")
                .await
                .unwrap();
            let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
            assert_eq!(eas::find_all_text_after(&doc, f::STATUS), vec!["1", "1"]);
            assert_eq!(
                eas::find_text_after(&doc, f::TOTAL)
                    .unwrap()
                    .parse::<usize>()
                    .unwrap(),
                expected,
                "{operator}"
            );
        }

        let date = &entries[0]["receivedAt"].as_str().unwrap()[..10];
        for operator in ["=", ">", ">=", "<", "<=", "<>"] {
            let expected = entries
                .iter()
                .filter(|entry| {
                    let day = &entry["receivedAt"].as_str().unwrap()[..10];
                    match operator {
                        "=" => day == date,
                        ">" => day > date,
                        ">=" => day >= date,
                        "<" => day < date,
                        "<=" => day <= date,
                        "<>" => day != date,
                        _ => unreachable!(),
                    }
                })
                .count();
            let response = send(format!("subject:{tag} received{operator}{date}"), "0-9")
                .await
                .unwrap();
            let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
            assert_eq!(eas::find_all_text_after(&doc, f::STATUS), vec!["1", "1"]);
            assert_eq!(
                eas::find_text_after(&doc, f::TOTAL)
                    .unwrap()
                    .parse::<usize>()
                    .unwrap(),
                expected,
                "{operator}"
            );
        }
        let r = live
            .call(serde_json::json!([["Email/set", {
            "accountId": live.account, "destroy": ids
        }, "0"], ["Mailbox/set", {
            "accountId": live.account, "destroy": [&folder]
        }, "1"]]))
            .await;
        assert!(r[0][1]["notDestroyed"].is_null());
        assert!(r[1][1]["notDestroyed"].is_null());
    }

    #[tokio::test]
    #[ignore]
    async fn e2e_calendar_attendees_can_be_cleared_without_losing_organizer() {
        use eas::calendar as cal;
        let live = Live::new("E2E_USER2").await;
        let using = [
            "urn:ietf:params:jmap:core",
            "urn:ietf:params:jmap:calendars",
        ];
        let result = live
            .call_using(
                serde_json::json!([["Calendar/get",{"accountId":live.account},"0"]]),
                &using,
            )
            .await;
        let calendar = result[0][1]["list"][0]["id"].as_str().unwrap().to_owned();
        let folder = format!("cal_{calendar}");
        let owner = live.user.clone();
        let result = live
            .call_using(
                serde_json::json!([["CalendarEvent/set",{
            "accountId":live.account,"create":{"e":{
                "@type":"Event","calendarIds":{&calendar:true},"title":"Attendee fixture",
                "start":"2026-11-15T10:00:00","duration":"PT1H",
                "replyTo":{"imip":format!("mailto:{owner}")},
                "participants":{
                    "owner":{"@type":"Participant","name":"Organizer name","email":owner,
                        "calendarAddress":format!("mailto:{owner}"),"roles":{"owner":true},
                        "sendTo":{"imip":format!("mailto:{owner}")}},
                    "guest/~":{"@type":"Participant","name":"Guest","email":"guest@example.test",
                        "calendarAddress":"mailto:guest@example.test","roles":{"attendee":true},
                        "participationStatus":"accepted",
                        "sendTo":{"imip":"mailto:guest@example.test"}}
                }
            }}
        },"0"]]),
                &using,
            )
            .await;
        let id = result[0][1]["created"]["e"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let mut devices = Vec::new();
        for prefix in ["attendees-a", "attendees-b"] {
            let device = dev(prefix);
            let prime = live
                .wbxml_version("Sync", &device, sync_body("0", &folder), "16.1")
                .await;
            let doc = live
                .wbxml_version(
                    "Sync",
                    &device,
                    sync_body(&sync_key(&prime), &folder),
                    "16.1",
                )
                .await;
            devices.push((device, sync_key(&doc)));
        }
        for mode in ["omit", "invalid", "duplicate", "edit", "clear"] {
            let mut b = eas::DocumentBuilder::new();
            b.start(air::SYNC);
            b.start(air::COLLECTIONS);
            b.start(air::COLLECTION);
            b.leaf(air::SYNC_KEY, &devices[0].1);
            b.leaf(air::COLLECTION_ID, &folder);
            b.start(air::COMMANDS);
            b.start(air::CHANGE);
            b.leaf(air::SERVER_ID, &id);
            b.start(air::APPLICATION_DATA);
            match mode {
                "omit" => b.leaf(cal::SUBJECT, "Changed subject"),
                "invalid" => {
                    b.leaf(cal::SUBJECT, "Must not apply");
                    b.start(cal::ATTENDEES);
                    b.start(cal::ATTENDEE);
                    b.leaf(cal::ATTENDEE_NAME, "Missing email");
                    b.end();
                    b.end();
                }
                "duplicate" => {
                    b.leaf(cal::SUBJECT, "Must not apply");
                    b.start(cal::ATTENDEES);
                    for email in ["guest@example.test", "GUEST@example.test"] {
                        b.start(cal::ATTENDEE);
                        b.leaf(cal::ATTENDEE_EMAIL, email);
                        b.end();
                    }
                    b.end();
                }
                "edit" => {
                    b.start(cal::ATTENDEES);
                    b.start(cal::ATTENDEE);
                    b.leaf(cal::ATTENDEE_EMAIL, "guest@example.test");
                    b.leaf(cal::ATTENDEE_NAME, "Renamed guest");
                    b.leaf(cal::ATTENDEE_TYPE, "2");
                    b.end();
                    b.start(cal::ATTENDEE);
                    b.leaf(cal::ATTENDEE_EMAIL, "new@example.test");
                    b.leaf(cal::ATTENDEE_NAME, "New guest");
                    b.end();
                    b.end();
                }
                _ => b.empty_tag(cal::ATTENDEES),
            }
            for _ in 0..6 {
                b.end();
            }
            let doc = live
                .wbxml_version(
                    "Sync",
                    &devices[0].0,
                    wbxml::encode_document(&b.finish()),
                    "16.1",
                )
                .await;
            let statuses = eas::find_all_text_after(&doc, air::STATUS);
            if mode == "invalid" || mode == "duplicate" {
                assert!(statuses.contains(&"6".to_owned()));
            } else {
                assert!(statuses.iter().all(|status| status == "1"));
            }
            devices[0].1 = sync_key(&doc);
            let native=live.call_using(serde_json::json!([["CalendarEvent/get",{
                "accountId":live.account,"ids":[&id],"properties":["id","title","participants","replyTo"]
            },"0"]]),&using).await;
            let event = &native[0][1]["list"][0];
            assert_eq!(event["title"], "Changed subject");
            assert_eq!(event["participants"]["owner"]["name"], "Organizer name");
            assert_eq!(event["participants"]["guest/~"].is_null(), mode == "clear");
            if mode == "edit" {
                assert_eq!(event["participants"]["guest/~"]["name"], "Renamed guest");
                assert_eq!(
                    event["participants"]["guest/~"]["participationStatus"],
                    "accepted"
                );
                assert_eq!(event["participants"]["guest/~"]["roles"]["optional"], true);
                assert!(event["participants"]
                    .as_object()
                    .unwrap()
                    .values()
                    .any(|p| p["email"] == "new@example.test"));
                let peer = live
                    .wbxml_version(
                        "Sync",
                        &devices[1].0,
                        sync_body(&devices[1].1, &folder),
                        "16.1",
                    )
                    .await;
                assert_eq!(
                    eas::find_text_after(&peer, air::SERVER_ID),
                    Some(id.as_str())
                );
                let names = eas::find_all_text_after(&peer, cal::ATTENDEE_NAME);
                assert!(names.contains(&"Renamed guest".to_owned()));
                assert!(names.contains(&"New guest".to_owned()));
                devices[1].1 = sync_key(&peer);
            }
            if mode == "clear" {
                assert_eq!(event["participants"].as_object().unwrap().len(), 1);
            }
            assert!(event["replyTo"]["imip"].as_str().is_some());
        }
        let doc = live
            .wbxml_version(
                "Sync",
                &devices[1].0,
                sync_body(&devices[1].1, &folder),
                "16.1",
            )
            .await;
        assert_eq!(
            eas::find_text_after(&doc, air::SERVER_ID),
            Some(id.as_str())
        );
        assert_eq!(eas::find_text_after(&doc, cal::MEETING_STATUS), Some("0"));
        assert!(doc
            .nodes
            .iter()
            .any(|node| matches!(node,wbxml::Node::Start(t)
            if t.code_page==cal::PAGE && t.token==cal::ATTENDEES.token && !t.has_content)));
        let result = live
            .call_using(
                serde_json::json!([["CalendarEvent/set",{
            "accountId":live.account,"destroy":[id]
        },"0"]]),
                &using,
            )
            .await;
        assert!(result[0][1]["notDestroyed"].is_null());
    }

    #[tokio::test]
    #[ignore]
    async fn e2e_calendar_location_coordinates_sync() {
        use eas::airsync_base as base;
        let live = Live::new("E2E_USER2").await;
        let using = [
            "urn:ietf:params:jmap:core",
            "urn:ietf:params:jmap:calendars",
        ];
        let created = live.call_using(serde_json::json!([["Calendar/set", {"accountId":live.account,"create":{"c":{"name":format!("Geo fixture {}",std::process::id())}}},"0"]]), &using).await;
        let calendar = created[0][1]["created"]["c"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let created = live.call_using(serde_json::json!([["CalendarEvent/set", {"accountId":live.account,"create":{"e":{"@type":"Event","calendarIds":{&calendar:true},"title":"Coordinates fixture","start":"2042-05-05T10:00:00","duration":"PT1H","locations":{"a":{"@type":"Location","name":"Room","coordinates":"geo:43.65,-79.38"}}}}},"0"]]), &using).await;
        let id = created[0][1]["created"]["e"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let folder = format!("cal_{calendar}");
        let device = dev("geo");
        let prime = live
            .wbxml_version("Sync", &device, sync_body("0", &folder), "16.1")
            .await;
        let response = live
            .wbxml_version(
                "Sync",
                &device,
                sync_body(&sync_key(&prime), &folder),
                "16.1",
            )
            .await;
        assert_eq!(
            eas::find_text_after(&response, base::LATITUDE),
            Some("43.65")
        );
        assert_eq!(
            eas::find_text_after(&response, base::LONGITUDE),
            Some("-79.38")
        );
        let changed = live.call_using(serde_json::json!([["CalendarEvent/set", {"accountId":live.account,"update":{&id:{"locations/a/coordinates":"geo:44,-80"}}},"0"]]), &using).await;
        assert!(changed[0][1]["notUpdated"].is_null());
        let response = live
            .wbxml_version(
                "Sync",
                &device,
                sync_body(&sync_key(&response), &folder),
                "16.1",
            )
            .await;
        assert_eq!(eas::find_text_after(&response, base::LATITUDE), Some("44"));
        assert_eq!(
            eas::find_text_after(&response, base::LONGITUDE),
            Some("-80")
        );
        let mut b = eas::DocumentBuilder::new();
        b.start(air::SYNC);
        b.start(air::COLLECTIONS);
        b.start(air::COLLECTION);
        b.leaf(air::SYNC_KEY, sync_key(&response));
        b.leaf(air::COLLECTION_ID, &folder);
        b.start(air::COMMANDS);
        b.start(air::CHANGE);
        b.leaf(air::SERVER_ID, &id);
        b.start(air::APPLICATION_DATA);
        b.start(base::LOCATION);
        b.leaf(base::DISPLAY_NAME, "Room");
        b.leaf(base::LATITUDE, "45.2");
        b.leaf(base::LONGITUDE, "-81.4");
        for _ in 0..7 {
            b.end();
        }
        let changed = live
            .wbxml_version("Sync", &device, wbxml::encode_document(&b.finish()), "16.1")
            .await;
        assert!(!eas::find_all_text_after(&changed, air::STATUS).contains(&"6".to_owned()));
        let got = live.call_using(serde_json::json!([["CalendarEvent/get", {"accountId":live.account,"ids":[&id],"properties":["locations"]},"0"]]), &using).await;
        assert_eq!(
            got[0][1]["list"][0]["locations"]["a"]["coordinates"],
            "geo:45.2,-81.4"
        );
        let second = dev("geo-second");
        let prime = live
            .wbxml_version("Sync", &second, sync_body("0", &folder), "16.1")
            .await;
        let response = live
            .wbxml_version(
                "Sync",
                &second,
                sync_body(&sync_key(&prime), &folder),
                "16.1",
            )
            .await;
        assert_eq!(
            eas::find_text_after(&response, base::LATITUDE),
            Some("45.2")
        );
        assert_eq!(
            eas::find_text_after(&response, base::LONGITUDE),
            Some("-81.4")
        );
        let mut b = eas::DocumentBuilder::new();
        b.start(air::SYNC);
        b.start(air::COLLECTIONS);
        b.start(air::COLLECTION);
        b.leaf(air::SYNC_KEY, sync_key(&changed));
        b.leaf(air::COLLECTION_ID, &folder);
        b.leaf(air::GET_CHANGES, "0");
        b.start(air::COMMANDS);
        b.start(air::ADD);
        b.leaf(air::CLIENT_ID, "geo-created");
        b.start(air::APPLICATION_DATA);
        b.leaf(eas::calendar::SUBJECT, "Created coordinates fixture");
        b.leaf(eas::calendar::START_TIME, "20420505T100000Z");
        b.leaf(eas::calendar::END_TIME, "20420505T110000Z");
        b.start(base::LOCATION);
        b.leaf(base::LATITUDE, "12.5");
        b.leaf(base::LONGITUDE, "-3.25");
        for _ in 0..7 {
            b.end();
        }
        let created = live
            .wbxml_version("Sync", &device, wbxml::encode_document(&b.finish()), "16.1")
            .await;
        let new_id = eas::find_text_after(&created, air::SERVER_ID)
            .expect("created calendar ID")
            .to_owned();
        let got = live.call_using(serde_json::json!([["CalendarEvent/get", {"accountId":live.account,"ids":[&new_id],"properties":["locations"]},"0"]]), &using).await;
        assert_eq!(
            got[0][1]["list"][0]["locations"]["loc1"]["coordinates"],
            "geo:12.5,-3.25"
        );
        let removed = live.call_using(serde_json::json!([["CalendarEvent/set", {"accountId":live.account,"destroy":[id,new_id]},"0"],["Calendar/set", {"accountId":live.account,"destroy":[calendar]},"1"]]), &using).await;
        assert!(removed[0][1]["notDestroyed"].is_null());
        assert!(removed[1][1]["notDestroyed"].is_null());
    }

    #[tokio::test]
    #[ignore]
    async fn e2e_calendar_privacy_round_trips_and_unsupported_values_are_rejected() {
        use eas::calendar as cal;
        let live = Live::new("E2E_USER2").await;
        let using = [
            "urn:ietf:params:jmap:core",
            "urn:ietf:params:jmap:calendars",
        ];
        let calendars = live
            .call_using(
                serde_json::json!([
            ["Calendar/get",{"accountId":live.account},"0"]]),
                &using,
            )
            .await;
        let calendar = calendars[0][1]["list"][0]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let folder = format!("cal_{calendar}");
        let created = live
            .call_using(
                serde_json::json!([["CalendarEvent/set",{
            "accountId":live.account,"create":{"e":{"@type":"Event",
            "calendarIds":{&calendar:true},"title":"Privacy fixture",
            "start":"2042-05-05T10:00:00","duration":"PT1H","privacy":"private"}}},"0"]]),
                &using,
            )
            .await;
        let id = created[0][1]["created"]["e"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let mut devices = Vec::new();
        for prefix in ["calendar-privacy-a", "calendar-privacy-b"] {
            let device = dev(prefix);
            let prime = live
                .wbxml_version("Sync", &device, sync_body("0", &folder), "16.1")
                .await;
            let synced = live
                .wbxml_version(
                    "Sync",
                    &device,
                    sync_body(&sync_key(&prime), &folder),
                    "16.1",
                )
                .await;
            assert!(eas::find_all_text_after(&synced, cal::SENSITIVITY).contains(&"2".to_owned()));
            devices.push((device, sync_key(&synced)));
        }
        for (value, privacy) in [
            ("0", "public"),
            ("2", "private"),
            ("1", "private"),
            ("3", "private"),
        ] {
            let mut b = eas::DocumentBuilder::new();
            b.start(air::SYNC);
            b.start(air::COLLECTIONS);
            b.start(air::COLLECTION);
            b.leaf(air::SYNC_KEY, &devices[0].1);
            b.leaf(air::COLLECTION_ID, &folder);
            b.start(air::COMMANDS);
            b.start(air::CHANGE);
            b.leaf(air::SERVER_ID, &id);
            b.start(air::APPLICATION_DATA);
            b.leaf(cal::SENSITIVITY, value);
            if value == "1" || value == "3" {
                b.leaf(cal::SUBJECT, "must not change");
            }
            for _ in 0..6 {
                b.end();
            }
            let response = live
                .wbxml_version(
                    "Sync",
                    &devices[0].0,
                    wbxml::encode_document(&b.finish()),
                    "16.1",
                )
                .await;
            if value == "1" || value == "3" {
                assert!(eas::find_all_text_after(&response, air::STATUS).contains(&"6".to_owned()));
            } else {
                assert!(eas::find_all_text_after(&response, air::STATUS)
                    .iter()
                    .all(|status| status == "1"));
            }
            devices[0].1 = sync_key(&response);
            let native = live
                .call_using(
                    serde_json::json!([["CalendarEvent/get",{
                "accountId":live.account,"ids":[&id],"properties":["title","privacy"]},"0"]]),
                    &using,
                )
                .await;
            assert_eq!(native[0][1]["list"][0]["title"], "Privacy fixture");
            assert_eq!(native[0][1]["list"][0]["privacy"], privacy);
            if value == "0" || value == "2" {
                let other = live
                    .wbxml_version(
                        "Sync",
                        &devices[1].0,
                        sync_body(&devices[1].1, &folder),
                        "16.1",
                    )
                    .await;
                assert_eq!(
                    eas::find_text_after(&other, air::SERVER_ID),
                    Some(id.as_str())
                );
                assert_eq!(eas::find_text_after(&other, cal::SENSITIVITY), Some(value));
                devices[1].1 = sync_key(&other);
            }
        }
        let mut add = eas::DocumentBuilder::new();
        add.start(air::SYNC);
        add.start(air::COLLECTIONS);
        add.start(air::COLLECTION);
        add.leaf(air::SYNC_KEY, &devices[0].1);
        add.leaf(air::COLLECTION_ID, &folder);
        add.start(air::COMMANDS);
        add.start(air::ADD);
        add.leaf(air::CLIENT_ID, "privacy-add");
        add.start(air::APPLICATION_DATA);
        add.leaf(cal::SUBJECT, "Private created fixture");
        add.leaf(cal::START_TIME, "20420506T100000Z");
        add.leaf(cal::END_TIME, "20420506T110000Z");
        add.leaf(cal::SENSITIVITY, "2");
        for _ in 0..6 {
            add.end();
        }
        let request = wbxml::encode_document(&add.finish());
        let added = live
            .wbxml_version("Sync", &devices[0].0, request.clone(), "16.1")
            .await;
        assert!(eas::find_all_text_after(&added, air::STATUS)
            .iter()
            .all(|status| status == "1"));
        let added_id = eas::find_text_after(&added, air::SERVER_ID)
            .unwrap()
            .to_owned();
        let replay = live
            .wbxml_version("Sync", &devices[0].0, request, "16.1")
            .await;
        assert_eq!(
            eas::find_text_after(&replay, air::SERVER_ID),
            Some(added_id.as_str())
        );
        let native = live
            .call_using(
                serde_json::json!([["CalendarEvent/get",{
            "accountId":live.account,"ids":[&added_id],"properties":["privacy"]},"0"]]),
                &using,
            )
            .await;
        assert_eq!(native[0][1]["list"][0]["privacy"], "private");
        let cleanup = live
            .call_using(
                serde_json::json!([["CalendarEvent/set",{
            "accountId":live.account,"destroy":[id,added_id]},"0"]]),
                &using,
            )
            .await;
        assert!(cleanup[0][1]["notDestroyed"].is_null());
    }

    #[tokio::test]
    #[ignore]
    async fn e2e_unsupported_recurrence_does_not_save_other_fields() {
        use eas::calendar as cal;
        let live = Live::new("E2E_USER2").await;
        let using = [
            "urn:ietf:params:jmap:core",
            "urn:ietf:params:jmap:calendars",
        ];
        let calendars = live
            .call_using(
                serde_json::json!([
            ["Calendar/get",{"accountId":live.account},"0"]]),
                &using,
            )
            .await;
        let calendar = calendars[0][1]["list"][0]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let folder = format!("cal_{calendar}");
        let created = live
            .call_using(
                serde_json::json!([["CalendarEvent/set",{
            "accountId":live.account,"create":{"e":{"@type":"Event",
            "calendarIds":{&calendar:true},"title":"Recurrence safety fixture",
            "start":"2042-05-05T10:00:00","duration":"PT1H"}}},"0"]]),
                &using,
            )
            .await;
        let id = created[0][1]["created"]["e"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let device = dev("unsupported-recurrence");
        let prime = live
            .wbxml_version("Sync", &device, sync_body("0", &folder), "16.1")
            .await;
        let synced = live
            .wbxml_version(
                "Sync",
                &device,
                sync_body(&sync_key(&prime), &folder),
                "16.1",
            )
            .await;
        let mut b = eas::DocumentBuilder::new();
        b.start(air::SYNC);
        b.start(air::COLLECTIONS);
        b.start(air::COLLECTION);
        b.leaf(air::SYNC_KEY, sync_key(&synced));
        b.leaf(air::COLLECTION_ID, &folder);
        b.start(air::COMMANDS);
        b.start(air::CHANGE);
        b.leaf(air::SERVER_ID, &id);
        b.start(air::APPLICATION_DATA);
        b.leaf(cal::SUBJECT, "must not change");
        b.start(cal::RECURRENCE);
        b.leaf(cal::RECURRENCE_TYPE, "0");
        b.end();
        for _ in 0..6 {
            b.end();
        }
        let response = live
            .wbxml_version("Sync", &device, wbxml::encode_document(&b.finish()), "16.1")
            .await;
        assert!(eas::find_all_text_after(&response, air::STATUS).contains(&"6".to_owned()));
        let native = live
            .call_using(
                serde_json::json!([["CalendarEvent/get",{
            "accountId":live.account,"ids":[&id],"properties":["title","recurrenceRules"]},"0"]]),
                &using,
            )
            .await;
        assert_eq!(
            native[0][1]["list"][0]["title"],
            "Recurrence safety fixture"
        );
        assert!(native[0][1]["list"][0]["recurrenceRules"].is_null());
        let cleanup = live
            .call_using(
                serde_json::json!([["CalendarEvent/set",{
            "accountId":live.account,"destroy":[id]},"0"]]),
                &using,
            )
            .await;
        assert!(cleanup[0][1]["notDestroyed"].is_null());
    }

    #[tokio::test]
    #[ignore]
    async fn e2e_calendar_categories_round_trip_between_devices() {
        use eas::calendar as cal;
        let live = Live::new("E2E_USER2").await;
        let using = [
            "urn:ietf:params:jmap:core",
            "urn:ietf:params:jmap:calendars",
        ];
        let calendars = live
            .call_using(
                serde_json::json!([["Calendar/get",{"accountId":live.account},"0"]]),
                &using,
            )
            .await;
        let calendar = calendars[0][1]["list"][0]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let folder = format!("cal_{calendar}");
        let created = live
            .call_using(
                serde_json::json!([["CalendarEvent/set",{
            "accountId":live.account,"create":{"e":{"@type":"Event",
            "calendarIds":{&calendar:true},"title":"Categories fixture",
            "start":"2042-05-05T10:00:00","duration":"PT1H",
            "keywords":{"Initial":true}}}},"0"]]),
                &using,
            )
            .await;
        let id = created[0][1]["created"]["e"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let mut devices = Vec::new();
        for prefix in ["calendar-categories-a", "calendar-categories-b"] {
            let device = dev(prefix);
            let prime = live
                .wbxml_version("Sync", &device, sync_body("0", &folder), "16.1")
                .await;
            let sync = live
                .wbxml_version(
                    "Sync",
                    &device,
                    sync_body(&sync_key(&prime), &folder),
                    "16.1",
                )
                .await;
            assert!(eas::find_all_text_after(&sync, cal::CATEGORY).contains(&"Initial".to_owned()));
            devices.push((device, sync_key(&sync)));
        }
        for values in [&["Personal space", "Work"][..], &[][..]] {
            let mut b = eas::DocumentBuilder::new();
            b.start(air::SYNC);
            b.start(air::COLLECTIONS);
            b.start(air::COLLECTION);
            b.leaf(air::SYNC_KEY, &devices[0].1);
            b.leaf(air::COLLECTION_ID, &folder);
            b.start(air::COMMANDS);
            b.start(air::CHANGE);
            b.leaf(air::SERVER_ID, &id);
            b.start(air::APPLICATION_DATA);
            if values.is_empty() {
                b.empty_tag(cal::CATEGORIES);
            } else {
                b.start(cal::CATEGORIES);
                for value in values {
                    b.leaf(cal::CATEGORY, *value);
                }
                b.end();
            }
            for _ in 0..6 {
                b.end();
            }
            let response = live
                .wbxml_version(
                    "Sync",
                    &devices[0].0,
                    wbxml::encode_document(&b.finish()),
                    "16.1",
                )
                .await;
            assert!(eas::find_all_text_after(&response, air::STATUS)
                .iter()
                .all(|v| v == "1"));
            devices[0].1 = sync_key(&response);
            let native = live
                .call_using(
                    serde_json::json!([["CalendarEvent/get",{
                "accountId":live.account,"ids":[&id],"properties":["keywords"]},"0"]]),
                    &using,
                )
                .await;
            let keywords = &native[0][1]["list"][0]["keywords"];
            if values.is_empty() {
                assert!(keywords.is_null() || keywords == &serde_json::json!({}));
            } else {
                assert_eq!(
                    keywords,
                    &serde_json::json!({"Personal space":true,"Work":true})
                );
            }
            let other = live
                .wbxml_version(
                    "Sync",
                    &devices[1].0,
                    sync_body(&devices[1].1, &folder),
                    "16.1",
                )
                .await;
            assert_eq!(
                eas::find_text_after(&other, air::SERVER_ID),
                Some(id.as_str())
            );
            assert_eq!(
                eas::find_all_text_after(&other, cal::CATEGORY),
                values.iter().map(|v| (*v).to_owned()).collect::<Vec<_>>()
            );
            if values.is_empty() {
                assert!(other.nodes.iter().any(|node| matches!(node, wbxml::Node::Start(t) if t.code_page == cal::CATEGORIES.code_page && t.token == cal::CATEGORIES.token)));
            }
            devices[1].1 = sync_key(&other);
        }
        let mut add = eas::DocumentBuilder::new();
        add.start(air::SYNC);
        add.start(air::COLLECTIONS);
        add.start(air::COLLECTION);
        add.leaf(air::SYNC_KEY, &devices[0].1);
        add.leaf(air::COLLECTION_ID, &folder);
        add.start(air::COMMANDS);
        add.start(air::ADD);
        add.leaf(air::CLIENT_ID, "categories-add");
        add.start(air::APPLICATION_DATA);
        add.leaf(cal::SUBJECT, "Categories created fixture");
        add.leaf(cal::START_TIME, "20420506T100000Z");
        add.leaf(cal::END_TIME, "20420506T110000Z");
        add.start(cal::CATEGORIES);
        add.leaf(cal::CATEGORY, "Created category");
        add.end();
        for _ in 0..6 {
            add.end();
        }
        let request = wbxml::encode_document(&add.finish());
        let result = live
            .wbxml_version("Sync", &devices[0].0, request.clone(), "16.1")
            .await;
        assert!(eas::find_all_text_after(&result, air::STATUS)
            .iter()
            .all(|status| status == "1"));
        let added_id = eas::find_text_after(&result, air::SERVER_ID)
            .unwrap()
            .to_owned();
        let replay = live
            .wbxml_version("Sync", &devices[0].0, request, "16.1")
            .await;
        assert_eq!(
            eas::find_text_after(&replay, air::SERVER_ID),
            Some(added_id.as_str())
        );
        let native = live
            .call_using(
                serde_json::json!([["CalendarEvent/get",{
            "accountId":live.account, "ids":[&added_id], "properties":["keywords"]},"0"]]),
                &using,
            )
            .await;
        assert_eq!(
            native[0][1]["list"][0]["keywords"],
            serde_json::json!({"Created category":true})
        );
        let cleanup = live
            .call_using(
                serde_json::json!([["CalendarEvent/set",{
            "accountId":live.account,"destroy":[id,added_id]},"0"]]),
                &using,
            )
            .await;
        assert!(cleanup[0][1]["notDestroyed"].is_null());
    }

    #[tokio::test]
    #[ignore]
    async fn e2e_calendar_busy_status_round_trips_between_devices() {
        use eas::calendar as cal;
        let live = Live::new("E2E_USER2").await;
        let using = [
            "urn:ietf:params:jmap:core",
            "urn:ietf:params:jmap:calendars",
        ];
        let result = live
            .call_using(
                serde_json::json!([["Calendar/get",{"accountId":live.account},"0"]]),
                &using,
            )
            .await;
        let calendar = result[0][1]["list"][0]["id"].as_str().unwrap().to_owned();
        let folder = format!("cal_{calendar}");
        let result = live
            .call_using(
                serde_json::json!([["CalendarEvent/set",{"accountId":live.account,
            "create":{"e":{"@type":"Event","calendarIds":{&calendar:true},"title":"Busy fixture",
                "start":"2042-05-05T10:00:00","timeZone":"Etc/UTC","duration":"PT1H",
                "freeBusyStatus":"busy"}}},"0"]]),
                &using,
            )
            .await;
        let id = result[0][1]["created"]["e"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let mut devices = Vec::new();
        for prefix in ["calendar-busy-a", "calendar-busy-b"] {
            let device = dev(prefix);
            let prime = live
                .wbxml_version("Sync", &device, sync_body("0", &folder), "16.1")
                .await;
            let sync = live
                .wbxml_version(
                    "Sync",
                    &device,
                    sync_body(&sync_key(&prime), &folder),
                    "16.1",
                )
                .await;
            assert!(eas::find_all_text_after(&sync, air::SERVER_ID).contains(&id));
            devices.push((device, sync_key(&sync)));
        }
        for (status, native_value) in [
            ("0", "free"),
            ("1", "x-eas-tentative"),
            ("2", "busy"),
            ("3", "x-eas-out-of-office"),
        ] {
            let mut b = eas::DocumentBuilder::new();
            b.start(air::SYNC);
            b.start(air::COLLECTIONS);
            b.start(air::COLLECTION);
            b.leaf(air::SYNC_KEY, &devices[0].1);
            b.leaf(air::COLLECTION_ID, &folder);
            b.start(air::COMMANDS);
            b.start(air::CHANGE);
            b.leaf(air::SERVER_ID, &id);
            b.start(air::APPLICATION_DATA);
            b.leaf(cal::BUSY_STATUS, status);
            for _ in 0..6 {
                b.end();
            }
            let response = live
                .wbxml_version(
                    "Sync",
                    &devices[0].0,
                    wbxml::encode_document(&b.finish()),
                    "16.1",
                )
                .await;
            assert!(eas::find_all_text_after(&response, air::STATUS)
                .iter()
                .all(|v| v == "1"));
            devices[0].1 = sync_key(&response);
            let native = live
                .call_using(
                    serde_json::json!([["CalendarEvent/get",{"accountId":live.account,
                "ids":[&id],"properties":["title","freeBusyStatus"]},"0"]]),
                    &using,
                )
                .await;
            assert_eq!(native[0][1]["list"][0]["title"], "Busy fixture");
            assert_eq!(native[0][1]["list"][0]["freeBusyStatus"], native_value);
            let availability = live.call_using(serde_json::json!([["Principal/getAvailability",{
                "accountId":live.account,"id":live.account,"utcStart":"2042-05-05T10:00:00Z","utcEnd":"2042-05-05T11:00:00Z"
            },"0"]]), &["urn:ietf:params:jmap:core","urn:ietf:params:jmap:calendars","urn:ietf:params:jmap:principals"]).await;
            assert_eq!(
                availability[0][1]["list"].as_array().unwrap().is_empty(),
                status == "0"
            );
            let other = live
                .wbxml_version(
                    "Sync",
                    &devices[1].0,
                    sync_body(&devices[1].1, &folder),
                    "16.1",
                )
                .await;
            assert_eq!(
                eas::find_text_after(&other, air::SERVER_ID),
                Some(id.as_str())
            );
            assert_eq!(eas::find_text_after(&other, cal::BUSY_STATUS), Some(status));
            devices[1].1 = sync_key(&other);
        }
        let mut b = eas::DocumentBuilder::new();
        b.start(air::SYNC);
        b.start(air::COLLECTIONS);
        b.start(air::COLLECTION);
        b.leaf(air::SYNC_KEY, &devices[0].1);
        b.leaf(air::COLLECTION_ID, &folder);
        b.start(air::COMMANDS);
        b.start(air::CHANGE);
        b.leaf(air::SERVER_ID, &id);
        b.start(air::APPLICATION_DATA);
        b.leaf(cal::BUSY_STATUS, "4");
        b.leaf(cal::SUBJECT, "must not apply");
        for _ in 0..6 {
            b.end();
        }
        let response = live
            .wbxml_version(
                "Sync",
                &devices[0].0,
                wbxml::encode_document(&b.finish()),
                "16.1",
            )
            .await;
        assert!(eas::find_all_text_after(&response, air::STATUS).contains(&"6".to_owned()));
        let native = live
            .call_using(
                serde_json::json!([["CalendarEvent/get",{"accountId":live.account,
            "ids":[&id],"properties":["title","freeBusyStatus"]},"0"]]),
                &using,
            )
            .await;
        assert_eq!(native[0][1]["list"][0]["title"], "Busy fixture");
        assert_eq!(
            native[0][1]["list"][0]["freeBusyStatus"],
            "x-eas-out-of-office"
        );
        devices[0].1 = sync_key(&response);
        let mut b = eas::DocumentBuilder::new();
        b.start(air::SYNC);
        b.start(air::COLLECTIONS);
        b.start(air::COLLECTION);
        b.leaf(air::SYNC_KEY, &devices[0].1);
        b.leaf(air::COLLECTION_ID, &folder);
        b.start(air::COMMANDS);
        b.start(air::ADD);
        b.leaf(air::CLIENT_ID, "busy-add");
        b.start(air::APPLICATION_DATA);
        b.leaf(cal::SUBJECT, "Free fixture");
        b.leaf(cal::START_TIME, "20420506T100000Z");
        b.leaf(cal::END_TIME, "20420506T110000Z");
        b.leaf(cal::BUSY_STATUS, "0");
        for _ in 0..6 {
            b.end();
        }
        let added = live
            .wbxml_version(
                "Sync",
                &devices[0].0,
                wbxml::encode_document(&b.finish()),
                "16.1",
            )
            .await;
        assert!(eas::find_all_text_after(&added, air::STATUS)
            .iter()
            .all(|status| status == "1"));
        let added_id = eas::find_text_after(&added, air::SERVER_ID)
            .unwrap()
            .to_owned();
        let native = live
            .call_using(
                serde_json::json!([["CalendarEvent/get",{"accountId":live.account,
            "ids":[&added_id],"properties":["freeBusyStatus"]},"0"]]),
                &using,
            )
            .await;
        assert_eq!(native[0][1]["list"][0]["freeBusyStatus"], "free");
        let cleanup = live
            .call_using(
                serde_json::json!([["CalendarEvent/set",{"accountId":live.account,
            "destroy":[id,added_id]},"0"]]),
                &using,
            )
            .await;
        assert!(cleanup[0][1]["notDestroyed"].is_null());
    }

    #[tokio::test]
    #[ignore]
    async fn e2e_calendar_reminder_rejects_unsupported_default_flag() {
        use eas::calendar as cal;
        let live = Live::new("E2E_USER2").await;
        let using = [
            "urn:ietf:params:jmap:core",
            "urn:ietf:params:jmap:calendars",
        ];
        let calendars = live
            .call_using(
                serde_json::json!([["Calendar/get",{"accountId":live.account},"0"]]),
                &using,
            )
            .await;
        let calendar = calendars[0][1]["list"][0]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let folder = format!("cal_{calendar}");
        let created=live.call_using(serde_json::json!([["CalendarEvent/set",{"accountId":live.account,
            "create":{"e":{"@type":"Event","calendarIds":{&calendar:true},"title":"Unsupported default fixture",
                "start":"2042-08-05T10:00:00","duration":"PT1H","useDefaultAlerts":false,
                "alerts":{"a":{"@type":"Alert","action":"display","trigger":{"@type":"OffsetTrigger","offset":"-PT15M"}}}}}},"0"]]),&using).await;
        let id = created[0][1]["created"]["e"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let initial = live
            .call_using(
                serde_json::json!([["CalendarEvent/get",{"accountId":live.account,
            "ids":[&id],"properties":["useDefaultAlerts"]},"0"]]),
                &using,
            )
            .await;
        assert_eq!(initial[0][1]["list"][0]["useDefaultAlerts"], true);
        let device = dev("old-reminder");
        let prime = live
            .wbxml_version("Sync", &device, sync_body("0", &folder), "16.1")
            .await;
        let sync = live
            .wbxml_version(
                "Sync",
                &device,
                sync_body(&sync_key(&prime), &folder),
                "16.1",
            )
            .await;
        let mut b = eas::DocumentBuilder::new();
        b.start(air::SYNC);
        b.start(air::COLLECTIONS);
        b.start(air::COLLECTION);
        b.leaf(air::SYNC_KEY, sync_key(&sync));
        b.leaf(air::COLLECTION_ID, &folder);
        b.start(air::COMMANDS);
        b.start(air::CHANGE);
        b.leaf(air::SERVER_ID, &id);
        b.start(air::APPLICATION_DATA);
        b.leaf(cal::REMINDER, "30");
        for _ in 0..6 {
            b.end();
        }
        let changed = live
            .wbxml_version("Sync", &device, wbxml::encode_document(&b.finish()), "16.1")
            .await;
        assert!(eas::find_all_text_after(&changed, air::STATUS).contains(&"5".to_owned()));
        let cleanup = live
            .call_using(
                serde_json::json!([["CalendarEvent/set",{"accountId":live.account,
            "destroy":[id]},"0"]]),
                &using,
            )
            .await;
        assert!(cleanup[0][1]["notDestroyed"].is_null());
    }

    #[tokio::test]
    #[ignore]
    async fn e2e_validate_certificates() {
        use base64::{engine::general_purpose::STANDARD, Engine};
        use eas::validate_cert as vc;
        let live = Live::new("E2E_USER").await;
        let mut b = eas::DocumentBuilder::new();
        b.start(vc::VALIDATE_CERT);
        b.start(vc::CERTIFICATES);
        for cert in [
            include_bytes!("../tests/fixtures/validate-cert/good.der").as_slice(),
            include_bytes!("../tests/fixtures/validate-cert/revoked.der").as_slice(),
        ] {
            b.leaf(vc::CERTIFICATE, STANDARD.encode(cert));
        }
        b.end();
        b.leaf(vc::CHECK_CRL, "1");
        b.end();
        let result = live
            .wbxml_version(
                "ValidateCert",
                &dev("cert"),
                wbxml::encode_document(&b.finish()),
                "14.0",
            )
            .await;
        assert_eq!(
            eas::find_all_text_after(&result, vc::STATUS),
            vec!["1", "1", "13"]
        );
    }

    #[tokio::test]
    #[ignore]
    async fn e2e_resolve_recipients_contacts_and_ambiguity() {
        use eas::resolve_recipients as rr;
        let live = Live::new("E2E_USER2").await;
        let using = ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:contacts"];
        let books = live
            .call_using(
                serde_json::json!([["AddressBook/get", {"accountId":live.account}, "0"]]),
                &using,
            )
            .await;
        let book = books[0][1]["list"][0]["id"].as_str().unwrap();
        let marker = format!("resolverfixture{}", std::process::id());
        let first = format!("{marker}1@example.test");
        let second = format!("{marker}2@example.test");
        let created = live.call_using(serde_json::json!([["ContactCard/set", {"accountId":live.account,
            "create":{"a":{"@type":"Card","version":"1.0","addressBookIds":{book:true},"name":{"full":format!("{marker} one")},"emails":{"e":{"address":&first}}},
                      "b":{"@type":"Card","version":"1.0","addressBookIds":{book:true},"name":{"full":format!("{marker} two")},"emails":{"e":{"address":&second}}}}}, "0"]]), &using).await;
        let ids = [
            created[0][1]["created"]["a"]["id"]
                .as_str()
                .unwrap()
                .to_owned(),
            created[0][1]["created"]["b"]["id"]
                .as_str()
                .unwrap()
                .to_owned(),
        ];
        let mut b = eas::DocumentBuilder::new();
        b.start(rr::RESOLVE_RECIPIENTS);
        b.leaf(rr::TO, &first);
        b.leaf(rr::TO, &marker);
        b.leaf(rr::TO, "nobody-resolver@example.invalid");
        b.leaf(rr::TO, &live.user);
        b.start(rr::OPTIONS);
        b.leaf(rr::MAX_AMBIGUOUS_RECIPIENTS, "1");
        b.end();
        b.end();
        let result = live
            .wbxml_version(
                "ResolveRecipients",
                &dev("resolve"),
                wbxml::encode_document(&b.finish()),
                "14.0",
            )
            .await;
        assert_eq!(
            eas::find_all_text_after(&result, rr::STATUS),
            vec!["1", "1", "3", "4", "1"]
        );
        assert_eq!(
            eas::find_all_text_after(&result, rr::RECIPIENT_COUNT),
            vec!["1", "2", "0", "1"]
        );
        assert_eq!(
            eas::find_all_text_after(&result, rr::TYPE),
            vec!["2", "2", "1"]
        );
        assert_eq!(
            eas::find_all_text_after(&result, rr::EMAIL_ADDRESS)[0],
            first
        );
        let cleanup = live.call_using(serde_json::json!([["ContactCard/set", {"accountId":live.account,"destroy":ids}, "0"]]), &using).await;
        assert!(cleanup[0][1]["notDestroyed"].is_null());
    }

    #[tokio::test]
    #[ignore]
    async fn e2e_search_contacts_paginates_and_fetches_results() {
        use eas::{contacts, item_operations as ops, search as srch};
        let live = Live::new("E2E_USER2").await;
        let using = ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:contacts"];
        let books = live
            .call_using(
                serde_json::json!([["AddressBook/get",{
            "accountId":live.account
        },"0"]]),
                &using,
            )
            .await;
        let book = books[0][1]["list"][0]["id"].as_str().unwrap().to_owned();
        let marker = format!("contactfixture{}", std::process::id());
        let created = live
            .call_using(
                serde_json::json!([["ContactCard/set",{
            "accountId":live.account,
            "create":{
                "a":{"@type":"Card","version":"1.0","addressBookIds":{&book:true},
                    "name":{"full":format!("{marker} Alpha")},
                    "emails":{"e":{"address":"alpha@example.test"}}},
                "b":{"@type":"Card","version":"1.0","addressBookIds":{&book:true},
                    "name":{"full":format!("{marker} Beta")}},
                "c":{"@type":"Card","version":"1.0","addressBookIds":{&book:true},
                    "name":{"full":"unrelated fixture"}}
            }
        },"0"]]),
                &using,
            )
            .await;
        let cards = &created[0][1]["created"];
        let ids: Vec<String> = ["a", "b", "c"]
            .into_iter()
            .map(|key| {
                cards[key]["id"]
                    .as_str()
                    .unwrap_or_else(|| panic!("contact create failed: {created}"))
                    .to_owned()
            })
            .collect();
        let search = |range: &str| {
            let mut b = eas::DocumentBuilder::new();
            b.start(srch::SEARCH);
            b.start(srch::STORE);
            b.leaf(srch::NAME, "Mailbox");
            b.start(srch::QUERY);
            b.start(srch::AND);
            b.leaf(air::COLLECTION_ID, format!("ab_{book}"));
            b.leaf(srch::FREE_TEXT, &marker);
            b.end();
            b.end();
            b.start(srch::OPTIONS);
            b.leaf(srch::RANGE, range);
            b.end();
            b.end();
            b.end();
            wbxml::encode_document(&b.finish())
        };
        let first = live
            .wbxml_version("Search", &dev("contact-search"), search("0-0"), "16.1")
            .await;
        assert_eq!(
            eas::find_all_text_after(&first, srch::STATUS),
            vec!["1", "1"]
        );
        assert_eq!(eas::find_text_after(&first, srch::TOTAL), Some("2"));
        assert_eq!(eas::find_text_after(&first, srch::RANGE), Some("0-0"));
        assert_eq!(eas::find_text_after(&first, air::CLASS), Some("Contacts"));
        assert_eq!(
            eas::find_text_after(&first, air::COLLECTION_ID),
            Some(format!("ab_{book}").as_str())
        );
        let long_id = eas::find_text_after(&first, srch::LONG_ID)
            .unwrap()
            .to_owned();
        let name = eas::find_text_after(&first, contacts::FILE_AS)
            .unwrap()
            .to_owned();
        assert!(name.starts_with(&marker));
        let second = live
            .wbxml_version("Search", &dev("contact-search"), search("1-1"), "16.1")
            .await;
        assert_eq!(eas::find_text_after(&second, srch::TOTAL), Some("2"));
        assert_eq!(eas::find_text_after(&second, srch::RANGE), Some("1-1"));
        assert_ne!(
            eas::find_text_after(&second, srch::LONG_ID),
            Some(long_id.as_str())
        );

        let mut b = eas::DocumentBuilder::new();
        b.start(ops::ITEM_OPERATIONS);
        b.start(ops::FETCH);
        b.leaf(ops::STORE, "Mailbox");
        b.leaf(srch::LONG_ID, &long_id);
        b.end();
        b.end();
        let fetch = live
            .wbxml_version(
                "ItemOperations",
                &dev("contact-search"),
                wbxml::encode_document(&b.finish()),
                "16.1",
            )
            .await;
        assert_eq!(
            eas::find_all_text_after(&fetch, ops::STATUS),
            vec!["1", "1"]
        );
        assert_eq!(eas::find_text_after(&fetch, air::CLASS), Some("Contacts"));
        assert_eq!(
            eas::find_text_after(&fetch, contacts::FILE_AS),
            Some(name.as_str())
        );
        for (folder, expected) in [
            (format!("ab_{book}"), "1"),
            ("ab_nonexistent".to_owned(), "6"),
        ] {
            let mut b = eas::DocumentBuilder::new();
            b.start(ops::ITEM_OPERATIONS);
            b.start(ops::FETCH);
            b.leaf(ops::STORE, "Mailbox");
            b.leaf(air::COLLECTION_ID, folder);
            b.leaf(air::SERVER_ID, long_id.strip_prefix("contact:").unwrap());
            b.end();
            b.end();
            let fetched = live
                .wbxml_version(
                    "ItemOperations",
                    &dev("contact-search"),
                    wbxml::encode_document(&b.finish()),
                    "16.1",
                )
                .await;
            assert_eq!(
                eas::find_all_text_after(&fetched, ops::STATUS),
                vec!["1", expected]
            );
            if expected == "1" {
                assert_eq!(
                    eas::find_text_after(&fetched, contacts::FILE_AS),
                    Some(name.as_str())
                );
            } else {
                assert!(!eas::contains_token(&fetched, contacts::FILE_AS));
            }
        }
        let cleanup = live
            .call_using(
                serde_json::json!([["ContactCard/set",{
            "accountId":live.account,"destroy":ids
        },"0"]]),
                &using,
            )
            .await;
        assert!(cleanup[0][1]["notDestroyed"].is_null());
    }

    #[tokio::test]
    #[ignore]
    async fn e2e_search_finds_conversation_through_multiple_pages() {
        use eas::{email2, search as srch};
        let live = Live::new("E2E_USER2").await;
        let created = live.call(serde_json::json!([["Mailbox/set",{
            "accountId":live.account,"create":{"m":{"name":format!("Conversation search {}",std::process::id())}}
        },"0"]])).await;
        let folder = created[0][1]["created"]["m"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let target = live
            .seed(&folder, 1, "conversation-target", false)
            .await
            .remove(0);
        let device = dev("conversation-search");
        let prime = live
            .wbxml_version("Sync", &device, sync_body("0", &folder), "14.0")
            .await;
        let sync = live
            .wbxml_version(
                "Sync",
                &device,
                sync_body(&sync_key(&prime), &folder),
                "14.0",
            )
            .await;
        assert_eq!(
            eas::find_text_after(&sync, air::SERVER_ID),
            Some(target.as_str())
        );
        let bytes: [u8; 16] = eas::find_opaque_after(&sync, email2::CONVERSATION_ID)
            .unwrap()
            .try_into()
            .unwrap();
        let conversation = uuid::Uuid::from_bytes(bytes).to_string();
        let first_mail = live
            .call(serde_json::json!([["Email/get",{
            "accountId":live.account,"ids":[&target],"properties":["messageId","threadId"]},"0"]]))
            .await;
        let message_id = first_mail[0][1]["list"][0]["messageId"].clone();
        let reply = serde_json::json!({"mailboxIds":{&folder:true},
            "from":[{"email":live.user}],"subject":"Re: conversation-target 0",
            "messageId":[format!("{}@e2e.test", uuid::Uuid::new_v4())],
            "inReplyTo":message_id,"references":message_id});
        let reply_result = live
            .call(serde_json::json!([["Email/set",{
            "accountId":live.account,"create":{"reply":reply}},"0"]]))
            .await;
        let reply_id = reply_result[0][1]["created"]["reply"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let reply_item = live
            .call(serde_json::json!([["Email/get",{
            "accountId":live.account,"ids":[&reply_id],"properties":["threadId"]},"0"]]))
            .await;
        assert_eq!(
            reply_item[0][1]["list"][0]["threadId"],
            first_mail[0][1]["list"][0]["threadId"]
        );
        let mut ids = live.seed(&folder, 105, "conversation-filler", false).await;
        ids.extend([target.clone(), reply_id.clone()]);
        let request = |guid: &str, range: &str, parent: wbxml::token::Token, text: Option<&str>| {
            let mut b = eas::DocumentBuilder::new();
            b.start(srch::SEARCH);
            b.start(srch::STORE);
            b.leaf(srch::NAME, "Mailbox");
            b.start(srch::QUERY);
            b.start(parent);
            b.leaf(air::CLASS, "Email");
            b.leaf(air::COLLECTION_ID, &folder);
            b.leaf(srch::CONVERSATION_ID, guid);
            if let Some(text) = text {
                b.leaf(srch::FREE_TEXT, text);
            }
            b.end();
            b.end();
            b.start(srch::OPTIONS);
            b.leaf(srch::RANGE, range);
            b.end();
            b.end();
            b.end();
            wbxml::encode_document(&b.finish())
        };
        let first = live
            .wbxml_version(
                "Search",
                &device,
                request(&conversation, "0-0", srch::AND, None),
                "14.0",
            )
            .await;
        assert_eq!(
            eas::find_all_text_after(&first, srch::STATUS),
            vec!["1", "1"]
        );
        assert_eq!(eas::find_text_after(&first, srch::TOTAL), Some("2"));
        let first_id = eas::find_text_after(&first, srch::LONG_ID)
            .unwrap()
            .to_owned();
        let next = live
            .wbxml_version(
                "Search",
                &device,
                request(&conversation, "1-1", srch::AND, None),
                "14.0",
            )
            .await;
        assert_eq!(eas::find_text_after(&next, srch::TOTAL), Some("2"));
        let next_id = eas::find_text_after(&next, srch::LONG_ID)
            .unwrap()
            .to_owned();
        let mut actual = vec![first_id, next_id];
        actual.sort();
        let mut expected = vec![target, reply_id];
        expected.sort();
        assert_eq!(actual, expected);
        let beyond = live
            .wbxml_version(
                "Search",
                &device,
                request(&conversation, "2-2", srch::AND, None),
                "14.0",
            )
            .await;
        assert_eq!(eas::find_text_after(&beyond, srch::TOTAL), Some("2"));
        assert!(eas::find_all_text_after(&beyond, srch::LONG_ID).is_empty());
        let filtered = live
            .wbxml_version(
                "Search",
                &device,
                request(&conversation, "0-9", srch::AND, Some("conversation-filler")),
                "14.0",
            )
            .await;
        assert_eq!(eas::find_text_after(&filtered, srch::TOTAL), Some("0"));
        let absent = live
            .wbxml_version(
                "Search",
                &device,
                request(&uuid::Uuid::new_v4().to_string(), "0-0", srch::AND, None),
                "14.0",
            )
            .await;
        assert_eq!(eas::find_text_after(&absent, srch::TOTAL), Some("0"));
        let invalid = live
            .wbxml_version(
                "Search",
                &device,
                request(&conversation, "0-0", srch::OR, None),
                "14.0",
            )
            .await;
        assert_eq!(
            eas::find_all_text_after(&invalid, srch::STATUS),
            vec!["1", "8"]
        );
        let cleanup = live
            .call(serde_json::json!([
                ["Email/set",{"accountId":live.account,"destroy":ids},"0"],
                ["Mailbox/set",{"accountId":live.account,"destroy":[folder]},"1"]
            ]))
            .await;
        assert!(cleanup[0][1]["notDestroyed"].is_null());
        assert!(cleanup[1][1]["notDestroyed"].is_null());
    }

    #[tokio::test]
    #[ignore]
    async fn e2e_mailbox_search_combines_or_text_and_date() {
        use eas::{email as mail, search as srch};
        let live = Live::new("E2E_USER2").await;
        let word = format!("orgroup{}", std::process::id());
        let alternate = format!("alternate{}", std::process::id());
        let old = format!("{word} old");
        let middle = format!("{word} {alternate}");
        let recent = format!("{word} recent");
        let ids = live
            .seed_dated(&[
                (&old, "2020-01-01T00:00:00Z".to_owned()),
                (&middle, "2020-01-02T00:00:00Z".to_owned()),
                (&recent, "2020-01-03T00:00:00Z".to_owned()),
            ])
            .await;
        let mut b = eas::DocumentBuilder::new();
        b.start(srch::SEARCH);
        b.start(srch::STORE);
        b.leaf(srch::NAME, "Mailbox");
        b.start(srch::QUERY);
        b.start(srch::AND);
        b.leaf(air::CLASS, "Email");
        b.leaf(srch::FREE_TEXT, &word);
        b.start(srch::OR);
        b.leaf(srch::FREE_TEXT, &alternate);
        b.start(srch::GREATER_THAN);
        b.empty_tag(mail::DATE_RECEIVED);
        b.leaf(srch::VALUE, "2020-01-02T00:00:00Z");
        b.end();
        b.end();
        b.end();
        b.end();
        b.start(srch::OPTIONS);
        b.leaf(srch::RANGE, "0-9");
        b.end();
        b.end();
        b.end();
        let result = live
            .wbxml_version(
                "Search",
                &dev("search-or"),
                wbxml::encode_document(&b.finish()),
                "14.0",
            )
            .await;
        let mut actual = eas::find_all_text_after(&result, srch::LONG_ID);
        actual.sort();
        let mut expected = vec![ids[1].clone(), ids[2].clone()];
        expected.sort();
        assert_eq!(actual, expected);
        let cleanup = live
            .call(serde_json::json!([["Email/set",{"accountId":live.account,"destroy":ids},"0"]]))
            .await;
        assert!(cleanup[0][1]["notDestroyed"].is_null());
    }

    #[tokio::test]
    #[ignore]
    async fn e2e_gal_search_paginates_plain_query_and_reports_total() {
        use eas::{gal, search as srch};
        let live = Live::new("E2E_USER2").await;
        let search = |range: &str| {
            let mut b = eas::DocumentBuilder::new();
            b.start(srch::SEARCH);
            b.start(srch::STORE);
            b.leaf(srch::NAME, "GAL");
            b.leaf(srch::QUERY, "a");
            b.start(srch::OPTIONS);
            b.leaf(srch::RANGE, range);
            b.end();
            b.end();
            b.end();
            wbxml::encode_document(&b.finish())
        };
        let first = live
            .wbxml_version("Search", &dev("gal-first"), search("0-0"), "14.0")
            .await;
        let second = live
            .wbxml_version("Search", &dev("gal-second"), search("1-1"), "14.0")
            .await;
        assert_eq!(
            eas::find_all_text_after(&first, srch::STATUS),
            vec!["1", "1"]
        );
        assert_eq!(eas::find_text_after(&first, srch::TOTAL), Some("2"));
        assert_eq!(eas::find_text_after(&second, srch::TOTAL), Some("2"));
        assert_eq!(eas::find_text_after(&first, srch::RANGE), Some("0-0"));
        assert_eq!(eas::find_text_after(&second, srch::RANGE), Some("1-1"));
        assert_ne!(
            eas::find_text_after(&first, gal::DISPLAY_NAME),
            eas::find_text_after(&second, gal::DISPLAY_NAME)
        );
        let invalid = live
            .wbxml_version("Search", &dev("gal-invalid"), search("0-100"), "14.0")
            .await;
        assert_eq!(
            eas::find_all_text_after(&invalid, srch::STATUS),
            vec!["1", "12"]
        );
    }

    #[tokio::test]
    #[ignore]
    async fn e2e_mailbox_search_requires_each_free_text_term() {
        use eas::search as srch;
        let live = Live::new("E2E_USER2").await;
        let created = live
            .call(serde_json::json!([["Mailbox/set",{"accountId":live.account,
            "create":{"m":{"name":format!("Search terms {}",std::process::id())}}},"0"]]))
            .await;
        let folder = created[0][1]["created"]["m"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let first = format!("alphaterm{}", std::process::id());
        let second = format!("betaterm{}", std::process::id());
        let both = live
            .seed(&folder, 1, &format!("{first} {second}"), false)
            .await;
        let one = live.seed(&folder, 1, &first, false).await;
        let mut b = eas::DocumentBuilder::new();
        b.start(srch::SEARCH);
        b.start(srch::STORE);
        b.leaf(srch::NAME, "Mailbox");
        b.start(srch::QUERY);
        b.start(srch::AND);
        b.leaf(air::CLASS, "Email");
        b.leaf(air::COLLECTION_ID, &folder);
        b.leaf(srch::FREE_TEXT, &first);
        b.leaf(srch::FREE_TEXT, &second);
        b.end();
        b.end();
        b.start(srch::OPTIONS);
        b.leaf(srch::RANGE, "0-9");
        b.end();
        b.end();
        b.end();
        let result = live
            .wbxml_version(
                "Search",
                &dev("search-terms"),
                wbxml::encode_document(&b.finish()),
                "14.0",
            )
            .await;
        assert_eq!(eas::find_all_text_after(&result, srch::LONG_ID), both);
        assert_eq!(eas::find_text_after(&result, srch::RANGE), Some("0-0"));
        let cleanup = live
            .call(serde_json::json!([
            ["Email/set",{"accountId":live.account,"destroy":[&both[0],&one[0]]},"0"],
            ["Mailbox/set",{"accountId":live.account,"destroy":[folder]},"1"]]))
            .await;
        assert!(cleanup[0][1]["notDestroyed"].is_null());
        assert!(cleanup[1][1]["notDestroyed"].is_null());
    }

    #[tokio::test]
    #[ignore]
    async fn e2e_mailbox_search_date_bounds_are_exclusive() {
        use eas::{email as mail, search as srch};
        let live = Live::new("E2E_USER2").await;
        let word = format!("datefixture{}", std::process::id());
        let ids = live
            .seed_dated(&[
                (&word, "2020-01-01T00:00:00Z".to_owned()),
                (&word, "2020-01-01T00:00:01Z".to_owned()),
                (&word, "2020-01-01T00:00:02Z".to_owned()),
            ])
            .await;
        let search = |after: &str, before: &str, text: bool| {
            let mut b = eas::DocumentBuilder::new();
            b.start(srch::SEARCH);
            b.start(srch::STORE);
            b.leaf(srch::NAME, "Mailbox");
            b.start(srch::QUERY);
            b.start(srch::AND);
            b.leaf(air::CLASS, "Email");
            if text {
                b.leaf(srch::FREE_TEXT, &word);
            }
            b.start(srch::GREATER_THAN);
            b.empty_tag(mail::DATE_RECEIVED);
            b.leaf(srch::VALUE, after);
            b.end();
            b.start(srch::LESS_THAN);
            b.empty_tag(mail::DATE_RECEIVED);
            b.leaf(srch::VALUE, before);
            b.end();
            b.end();
            b.end();
            b.start(srch::OPTIONS);
            b.leaf(srch::RANGE, "0-9");
            b.end();
            b.end();
            b.end();
            wbxml::encode_document(&b.finish())
        };
        let result = live
            .wbxml_version(
                "Search",
                &dev("date-search"),
                search("2020-01-01T00:00:00Z", "2020-01-01T00:00:02Z", true),
                "14.0",
            )
            .await;
        assert_eq!(
            eas::find_all_text_after(&result, srch::LONG_ID),
            vec![ids[1].clone()]
        );
        let fraction = live
            .wbxml_version(
                "Search",
                &dev("date-search-fraction"),
                search("2020-01-01T00:00:00.500Z", "2020-01-01T00:00:01.500Z", true),
                "14.0",
            )
            .await;
        assert_eq!(
            eas::find_all_text_after(&fraction, srch::LONG_ID),
            vec![ids[1].clone()]
        );
        let only_dates = live
            .wbxml_version(
                "Search",
                &dev("date-only"),
                search("2020-01-01T00:00:00Z", "2020-01-01T00:00:02Z", false),
                "14.0",
            )
            .await;
        let returned = eas::find_all_text_after(&only_dates, srch::LONG_ID);
        assert!(returned.contains(&ids[1]));
        assert!(!returned.contains(&ids[0]) && !returned.contains(&ids[2]));
        let cleanup = live
            .call(serde_json::json!([["Email/set",{"accountId":live.account,"destroy":ids},"0"]]))
            .await;
        assert!(cleanup[0][1]["notDestroyed"].is_null());
    }

    #[tokio::test]
    #[ignore]
    async fn e2e_mailbox_search_respects_folder_and_deep_traversal() {
        use eas::search as srch;
        let live = Live::new("E2E_USER2").await;
        let created = live.call(serde_json::json!([["Mailbox/set",{"accountId":live.account,
            "create":{"parent":{"name":format!("Search root {}",std::process::id())},
                      "child":{"name":format!("Search child {}",std::process::id()),"parentId":"#parent"},
                      "outside":{"name":format!("Search outside {}",std::process::id())}}},"0"]])).await;
        let created = &created[0][1]["created"];
        let parent = created["parent"]["id"].as_str().unwrap().to_owned();
        let child = created["child"]["id"].as_str().unwrap().to_owned();
        let outside = created["outside"]["id"].as_str().unwrap().to_owned();
        let word = format!("scopefixture{}", std::process::id());
        let root_ids = live.seed(&parent, 1, &word, false).await;
        let child_ids = live.seed(&child, 1, &word, false).await;
        let outside_ids = live.seed(&outside, 1, &word, false).await;
        let update = live
            .call(serde_json::json!([["Email/set",{"accountId":live.account,
            "update":{&root_ids[0]:{format!("mailboxIds/{outside}"):true}}},"0"]]))
            .await;
        assert!(update[0][1]["updated"].get(&root_ids[0]).is_some());
        let search = |deep, unsupported| {
            let mut b = eas::DocumentBuilder::new();
            b.start(srch::SEARCH);
            b.start(srch::STORE);
            b.leaf(srch::NAME, "Mailbox");
            b.start(srch::QUERY);
            b.start(if unsupported { srch::OR } else { srch::AND });
            b.leaf(air::CLASS, "Email");
            b.leaf(air::COLLECTION_ID, &parent);
            b.leaf(srch::FREE_TEXT, &word);
            b.end();
            b.end();
            b.start(srch::OPTIONS);
            b.leaf(srch::RANGE, "0-9");
            if deep {
                b.empty_tag(srch::DEEP_TRAVERSAL);
            }
            b.end();
            b.end();
            b.end();
            wbxml::encode_document(&b.finish())
        };
        let root = live
            .wbxml_version("Search", &dev("search-scope"), search(false, false), "14.0")
            .await;
        assert_eq!(eas::find_all_text_after(&root, srch::LONG_ID), root_ids);
        assert_eq!(
            eas::find_text_after(&root, air::COLLECTION_ID),
            Some(parent.as_str())
        );
        let deep = live
            .wbxml_version("Search", &dev("search-deep"), search(true, false), "14.0")
            .await;
        let mut actual = eas::find_all_text_after(&deep, srch::LONG_ID);
        actual.sort();
        let mut expected = [root_ids.clone(), child_ids.clone()].concat();
        expected.sort();
        assert_eq!(actual, expected);
        let invalid = live
            .wbxml_version(
                "Search",
                &dev("search-invalid"),
                search(false, true),
                "14.0",
            )
            .await;
        assert_eq!(
            eas::find_all_text_after(&invalid, srch::STATUS),
            vec!["1", "8"]
        );
        let ids = [root_ids, child_ids, outside_ids].concat();
        let cleanup = live
            .call(
                serde_json::json!([["Email/set",{"accountId":live.account,"destroy":ids},"0"],
            ["Mailbox/set",{"accountId":live.account,"destroy":[child,parent,outside]},"1"]]),
            )
            .await;
        assert!(cleanup[0][1]["notDestroyed"].is_null());
        assert!(cleanup[1][1]["notDestroyed"].is_null());
    }

    #[tokio::test]
    #[ignore]
    async fn e2e_large_note_body_is_not_truncated() {
        use eas::{airsync_base as base, item_operations as ops, notes, search as srch};
        let live = Live::new("E2E_USER2").await;
        let mailboxes = live
            .call(serde_json::json!([["Mailbox/get", {"accountId":live.account}, "0"]]))
            .await;
        let existing = mailboxes[0][1]["list"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["name"] == "Notes" && item["parentId"].is_null())
            .and_then(|item| item["id"].as_str())
            .map(str::to_owned);
        let (mailbox, created) = if let Some(id) = existing {
            (id, false)
        } else {
            let result = live
                .call(
                    serde_json::json!([["Mailbox/set", {"accountId":live.account,
                "create":{"n":{"name":"Notes"}}}, "0"]]),
                )
                .await;
            (
                result[0][1]["created"]["n"]["id"]
                    .as_str()
                    .unwrap()
                    .to_owned(),
                true,
            )
        };
        let folder = format!("note_{mailbox}");
        let device = dev("large-note");
        let peer = dev("large-note-peer");
        let prime = live
            .wbxml_version("Sync", &device, sync_body("0", &folder), "16.1")
            .await;
        let initial = live
            .wbxml_version(
                "Sync",
                &device,
                sync_body(&sync_key(&prime), &folder),
                "16.1",
            )
            .await;
        let marker = format!("note_end_marker_{}", std::process::id());
        let content = format!("{}{marker}", "x".repeat(270_000));
        let subject = format!("Noteonlyfixture{}", std::process::id());
        let mut b = eas::DocumentBuilder::new();
        b.start(air::SYNC);
        b.start(air::COLLECTIONS);
        b.start(air::COLLECTION);
        b.leaf(air::SYNC_KEY, sync_key(&initial));
        b.leaf(air::COLLECTION_ID, &folder);
        b.start(air::COMMANDS);
        b.start(air::ADD);
        b.leaf(air::CLIENT_ID, "large-note-fixture");
        b.start(air::APPLICATION_DATA);
        b.leaf(notes::SUBJECT, &subject);
        b.start(base::BODY);
        b.leaf(base::TYPE, "1");
        b.leaf(base::DATA, &content);
        for _ in 0..7 {
            b.end();
        }
        let added = live
            .wbxml_version("Sync", &device, wbxml::encode_document(&b.finish()), "16.1")
            .await;
        let id = eas::find_text_after(&added, air::SERVER_ID)
            .unwrap()
            .to_owned();
        let peer_prime = live
            .wbxml_version("Sync", &peer, sync_body("0", &folder), "16.1")
            .await;
        let received = live
            .wbxml_version(
                "Sync",
                &peer,
                sync_body(&sync_key(&peer_prime), &folder),
                "16.1",
            )
            .await;
        assert!(eas::find_all_text_after(&received, base::DATA)
            .iter()
            .any(|body| body == &content));
        for explicit in [true, false] {
            let mut b = eas::DocumentBuilder::new();
            b.start(srch::SEARCH);
            b.start(srch::STORE);
            b.leaf(srch::NAME, "Mailbox");
            b.start(srch::QUERY);
            b.start(srch::AND);
            if explicit {
                b.leaf(air::CLASS, "Notes");
            }
            b.leaf(air::COLLECTION_ID, &folder);
            b.leaf(srch::FREE_TEXT, &marker);
            b.end();
            b.end();
            b.end();
            b.end();
            let result = live
                .wbxml_version("Search", &peer, wbxml::encode_document(&b.finish()), "16.1")
                .await;
            assert_eq!(
                eas::find_all_text_after(&result, srch::STATUS),
                vec!["1", "1"]
            );
            assert_eq!(eas::find_text_after(&result, srch::TOTAL), Some("1"));
            assert_eq!(eas::find_text_after(&result, air::CLASS), Some("Notes"));
            let long_id = eas::find_text_after(&result, srch::LONG_ID).unwrap();
            let mut b = eas::DocumentBuilder::new();
            b.start(ops::ITEM_OPERATIONS);
            b.start(ops::FETCH);
            b.leaf(ops::STORE, "Mailbox");
            b.leaf(srch::LONG_ID, long_id);
            b.end();
            b.end();
            let fetched = live
                .wbxml_version(
                    "ItemOperations",
                    &peer,
                    wbxml::encode_document(&b.finish()),
                    "16.1",
                )
                .await;
            assert_eq!(
                eas::find_all_text_after(&fetched, ops::STATUS),
                vec!["1", "1"]
            );
            assert_eq!(
                eas::find_text_after(&fetched, base::DATA),
                Some(content.as_str())
            );
            assert_eq!(eas::find_text_after(&result, base::TRUNCATED), Some("1"));
            let mut b = eas::DocumentBuilder::new();
            b.start(ops::ITEM_OPERATIONS);
            b.start(ops::FETCH);
            b.leaf(ops::STORE, "Mailbox");
            b.leaf(srch::LONG_ID, long_id);
            b.leaf(air::COLLECTION_ID, "note_wrongfolder");
            b.end();
            b.end();
            let denied = live
                .wbxml_version(
                    "ItemOperations",
                    &peer,
                    wbxml::encode_document(&b.finish()),
                    "16.1",
                )
                .await;
            assert_eq!(
                eas::find_all_text_after(&denied, ops::STATUS),
                vec!["1", "6"]
            );
        }
        let mut b = eas::DocumentBuilder::new();
        b.start(srch::SEARCH);
        b.start(srch::STORE);
        b.leaf(srch::NAME, "Mailbox");
        b.start(srch::QUERY);
        b.start(srch::AND);
        b.leaf(srch::FREE_TEXT, &marker);
        b.end();
        b.end();
        b.end();
        b.end();
        let mixed = live
            .wbxml_version("Search", &peer, wbxml::encode_document(&b.finish()), "16.1")
            .await;
        assert_eq!(
            eas::find_all_text_after(&mixed, srch::STATUS),
            vec!["1", "1"]
        );
        assert_eq!(eas::find_text_after(&mixed, srch::TOTAL), Some("1"));
        assert_eq!(eas::find_text_after(&mixed, air::CLASS), Some("Notes"));
        let mut b = eas::DocumentBuilder::new();
        b.start(srch::SEARCH);
        b.start(srch::STORE);
        b.leaf(srch::NAME, "Mailbox");
        b.start(srch::QUERY);
        b.start(srch::AND);
        b.leaf(air::CLASS, "Email");
        b.leaf(srch::FREE_TEXT, &subject);
        b.end();
        b.end();
        b.end();
        b.end();
        let email_result = live
            .wbxml_version("Search", &peer, wbxml::encode_document(&b.finish()), "16.1")
            .await;
        assert_eq!(
            eas::find_all_text_after(&email_result, srch::STATUS),
            vec!["1", "1"]
        );
        assert_eq!(eas::find_text_after(&email_result, srch::TOTAL), Some("0"));
        let mut b = eas::DocumentBuilder::new();
        b.start(air::SYNC);
        b.start(air::COLLECTIONS);
        b.start(air::COLLECTION);
        b.leaf(air::SYNC_KEY, sync_key(&added));
        b.leaf(air::COLLECTION_ID, &folder);
        b.start(air::COMMANDS);
        b.start(air::DELETE);
        b.leaf(air::SERVER_ID, &id);
        for _ in 0..5 {
            b.end();
        }
        live.wbxml_version("Sync", &device, wbxml::encode_document(&b.finish()), "16.1")
            .await;
        if created {
            let result = live.call(serde_json::json!([["Mailbox/set", {"accountId":live.account,"destroy":[mailbox]}, "0"]])).await;
            assert!(result[0][1]["notDestroyed"].is_null());
        }
    }

    #[tokio::test]
    #[ignore]
    async fn e2e_smart_compose_marks_source() {
        use eas::compose_mail as cm;
        let live = Live::new("E2E_USER2").await;
        let caps = [
            "urn:ietf:params:jmap:core",
            "urn:ietf:params:jmap:mail",
            "urn:ietf:params:jmap:submission",
        ];
        let before = live
            .call_using(
                serde_json::json!([["EmailSubmission/query", {"accountId":live.account}, "0"]]),
                &caps,
            )
            .await;
        let before: std::collections::BTreeSet<_> = before[0][1]["ids"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|id| id.as_str().map(str::to_owned))
            .collect();
        let marker = format!("smartfixture{}", std::process::id());
        let source = live
            .seed(&live.inbox, 2, &format!("{marker}source"), false)
            .await;
        for (command, token, keyword, id) in [
            ("SmartReply", cm::SMART_REPLY, "$answered", &source[0]),
            ("SmartForward", cm::SMART_FORWARD, "$forwarded", &source[1]),
        ] {
            let mime = format!("From: {}\r\nTo: {}\r\nSubject: {}\r\nMessage-ID: <{}-{}@e2e.test>\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nComplete message", live.user, live.user, marker, marker, command);
            let mut b = eas::DocumentBuilder::new();
            b.start(token);
            b.start(cm::SOURCE);
            if command == "SmartReply" {
                b.leaf(cm::FOLDER_ID, &live.inbox);
                b.leaf(cm::ITEM_ID, id);
            } else {
                b.leaf(cm::LONG_ID, id);
            }
            b.end();
            b.empty_tag(cm::REPLACE_MIME);
            b.opaque_leaf(cm::MIME, mime.into_bytes());
            b.end();
            let sent = live
                .req(command, &dev("smart-mark"))
                .body(wbxml::encode_document(&b.finish()))
                .send()
                .await
                .unwrap();
            assert_eq!(sent.status(), 200);
            let mut marked = false;
            for _ in 0..50 {
                let result = live.call(serde_json::json!([["Email/get", {"accountId":live.account,"ids":[id],"properties":["keywords"]}, "0"]])).await;
                if result[0][1]["list"][0]["keywords"][keyword] == true {
                    marked = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            assert!(marked, "source message was not marked {keyword}");
        }
        let mut delivered = Vec::new();
        for _ in 0..50 {
            let result = live.call(serde_json::json!([["Email/query", {"accountId":live.account,"filter":{"subject":&marker}}, "0"]])).await;
            delivered = result[0][1]["ids"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|id| id.as_str().map(str::to_owned))
                .collect();
            if delivered.len() >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(delivered.len(), 2);
        delivered.extend(source);
        let after = live
            .call_using(
                serde_json::json!([["EmailSubmission/query", {"accountId":live.account}, "0"]]),
                &caps,
            )
            .await;
        let created: Vec<_> = after[0][1]["ids"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|id| id.as_str())
            .filter(|id| !before.contains(*id))
            .collect();
        assert_eq!(created.len(), 2);
        let removed = live.call_using(serde_json::json!([["EmailSubmission/set", {"accountId":live.account,"destroy":created}, "0"]]), &caps).await;
        assert!(removed[0][1]["notDestroyed"].is_null());
        let removed = live.call(serde_json::json!([["Email/set", {"accountId":live.account,"destroy":delivered}, "0"]])).await;
        assert!(removed[0][1]["notDestroyed"].is_null());
    }

    #[tokio::test]
    #[ignore]
    async fn e2e_sendmail_without_sent_copy() {
        let live = Live::new("E2E_USER2").await;
        let caps = [
            "urn:ietf:params:jmap:core",
            "urn:ietf:params:jmap:mail",
            "urn:ietf:params:jmap:submission",
        ];
        let before = live
            .call_using(
                serde_json::json!([["EmailSubmission/query", {"accountId":live.account}, "0"]]),
                &caps,
            )
            .await;
        let before: std::collections::BTreeSet<_> = before[0][1]["ids"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|id| id.as_str().map(str::to_owned))
            .collect();
        let marker = format!("unsavedfixture{}", std::process::id());
        let mime = format!("From: {}\r\nTo: {}\r\nSubject: {}\r\nMessage-ID: <{}@e2e.test>\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nfixture", live.user, live.user, marker, marker);
        let response = live
            .req("SendMail", &dev("send-unsaved"))
            .query(&[("SaveInSentItems", "F")])
            .header("Content-Type", "message/rfc822")
            .body(mime)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let mut ids = Vec::new();
        for _ in 0..50 {
            let found = live
                .call(
                    serde_json::json!([["Email/query", {"accountId":live.account,
                "filter":{"subject":&marker}}, "0"]]),
                )
                .await;
            ids = found[0][1]["ids"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|id| id.as_str().map(str::to_owned))
                .collect();
            if !ids.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(ids.len(), 1, "expected only the delivered inbox copy");
        let got = live.call(serde_json::json!([["Email/get", {"accountId":live.account,"ids":ids,"properties":["id","mailboxIds"]}, "0"]])).await;
        assert_eq!(got[0][1]["list"][0]["mailboxIds"][&live.inbox], true);
        let after = live
            .call_using(
                serde_json::json!([["EmailSubmission/query", {"accountId":live.account}, "0"]]),
                &caps,
            )
            .await;
        let created: Vec<_> = after[0][1]["ids"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|id| id.as_str())
            .filter(|id| !before.contains(*id))
            .collect();
        assert_eq!(created.len(), 1);
        let removed = live.call_using(serde_json::json!([["EmailSubmission/set", {"accountId":live.account,"destroy":created}, "0"]]), &caps).await;
        assert!(removed[0][1]["notDestroyed"].is_null());
        let cleanup = live
            .call(serde_json::json!([["Email/set", {"accountId":live.account,"destroy":ids}, "0"]]))
            .await;
        assert!(cleanup[0][1]["notDestroyed"].is_null());
    }

    #[tokio::test]
    #[ignore]
    async fn e2e_sendmail_confirms_real_submission() {
        let live = Live::new("E2E_USER2").await;
        let marker = format!("sendfixture{}", std::process::id());
        let mime = format!("From: {}\r\nTo: {}\r\nSubject: {}\r\nMessage-ID: <{}@e2e.test>\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nfixture", live.user, live.user, marker, marker);
        let response = live
            .req("SendMail", &dev("send-confirm"))
            .header("Content-Type", "message/rfc822")
            .body(mime)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let mut ids = Vec::new();
        for _ in 0..50 {
            let found = live
                .call(
                    serde_json::json!([["Email/query", {"accountId":live.account,
                "filter":{"subject":&marker}, "calculateTotal":true}, "0"]]),
                )
                .await;
            ids = found[0][1]["ids"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|id| id.as_str().map(str::to_owned))
                .collect();
            if ids.len() >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(ids.len() >= 2, "sandbox message was not delivered locally");
        let caps = [
            "urn:ietf:params:jmap:core",
            "urn:ietf:params:jmap:mail",
            "urn:ietf:params:jmap:submission",
        ];
        let submissions = live.call_using(serde_json::json!([["EmailSubmission/query", {"accountId":live.account, "filter":{"emailIds":&ids}}, "0"]]), &caps).await;
        let submission_ids = submissions[0][1]["ids"].as_array().unwrap().clone();
        assert_eq!(submission_ids.len(), 1);
        let submission_cleanup = live.call_using(serde_json::json!([["EmailSubmission/set", {"accountId":live.account,"destroy":submission_ids}, "0"]]), &caps).await;
        assert!(submission_cleanup[0][1]["notDestroyed"].is_null());
        let cleanup = live
            .call(
                serde_json::json!([["Email/set", {"accountId":live.account, "destroy":ids}, "0"]]),
            )
            .await;
        assert!(cleanup[0][1]["notDestroyed"].is_null());
    }

    #[tokio::test]
    #[ignore]
    async fn e2e_overlapping_sync_commands_apply_once() {
        use eas::email;
        let live = Live::new("E2E_USER2").await;
        let created = live
            .call(
                serde_json::json!([["Mailbox/set", {"accountId":live.account,
            "create":{"m":{"name":format!("Overlap fixture {}", std::process::id())}}}, "0"]]),
            )
            .await;
        let folder = created[0][1]["created"]["m"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let ids = live.seed(&folder, 1, "overlap-fixture", false).await;
        let device = dev("overlap");
        let prime = live
            .wbxml_version("Sync", &device, sync_body("0", &folder), "16.1")
            .await;
        let synced = live
            .wbxml_version(
                "Sync",
                &device,
                sync_body(&sync_key(&prime), &folder),
                "16.1",
            )
            .await;
        let key = sync_key(&synced);
        let change = |read: &str| {
            let mut b = eas::DocumentBuilder::new();
            b.start(air::SYNC);
            b.start(air::COLLECTIONS);
            b.start(air::COLLECTION);
            b.leaf(air::SYNC_KEY, &key);
            b.leaf(air::COLLECTION_ID, &folder);
            b.start(air::COMMANDS);
            b.start(air::CHANGE);
            b.leaf(air::SERVER_ID, &ids[0]);
            b.start(air::APPLICATION_DATA);
            b.leaf(email::READ, read);
            for _ in 0..6 {
                b.end();
            }
            wbxml::encode_document(&b.finish())
        };
        let first = live
            .req_version("Sync", &device, "16.1")
            .body(change("1"))
            .send();
        let second = live
            .req_version("Sync", &device, "16.1")
            .body(change("0"))
            .send();
        let (first, second) = tokio::join!(first, second);
        let first = wbxml::decode_document(&first.unwrap().bytes().await.unwrap()).unwrap();
        let second = wbxml::decode_document(&second.unwrap().bytes().await.unwrap()).unwrap();
        let first_status = eas::find_text_after(&first, air::STATUS).unwrap();
        let second_status = eas::find_text_after(&second, air::STATUS).unwrap();
        assert!(matches!(
            (first_status, second_status),
            ("1", "3") | ("3", "1")
        ));
        let native = live
            .call(serde_json::json!([["Email/get", {"accountId":live.account,
            "ids":&ids,"properties":["keywords"]}, "0"]]))
            .await;
        let seen = native[0][1]["list"][0]["keywords"]["$seen"] == true;
        assert_eq!(seen, first_status == "1");
        let cleanup = live
            .call(serde_json::json!([
            ["Email/set", {"accountId":live.account,"destroy":ids}, "0"],
            ["Mailbox/set", {"accountId":live.account,"destroy":[folder]}, "1"]]))
            .await;
        assert!(cleanup[0][1]["notDestroyed"].is_null());
        assert!(cleanup[1][1]["notDestroyed"].is_null());
    }

    #[tokio::test]
    #[ignore]
    async fn e2e_sync_heartbeat_delivers_new_stalwart_calendar_event() {
        use eas::calendar as cal;
        let live = Live::new("E2E_USER2").await;
        let using = [
            "urn:ietf:params:jmap:core",
            "urn:ietf:params:jmap:calendars",
        ];
        let created = live
            .call_using(
                serde_json::json!([["Calendar/set", {"accountId":live.account,
            "create":{"c":{"name":format!("Wait fixture {}", std::process::id())}}}, "0"]]),
                &using,
            )
            .await;
        let calendar = created[0][1]["created"]["c"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let folder = format!("cal_{calendar}");
        let device = dev("calendar-wait");
        let prime = live
            .wbxml_version("Sync", &device, sync_body("0", &folder), "16.1")
            .await;
        let synced = live
            .wbxml_version(
                "Sync",
                &device,
                sync_body(&sync_key(&prime), &folder),
                "16.1",
            )
            .await;
        let mut b = eas::DocumentBuilder::new();
        b.start(air::SYNC);
        b.leaf(air::HEARTBEAT_INTERVAL, "60");
        b.start(air::COLLECTIONS);
        b.start(air::COLLECTION);
        b.leaf(air::SYNC_KEY, sync_key(&synced));
        b.leaf(air::COLLECTION_ID, &folder);
        b.empty_tag(air::GET_CHANGES);
        b.end();
        b.end();
        b.end();
        let request = live
            .req_version("Sync", &device, "16.1")
            .body(wbxml::encode_document(&b.finish()))
            .send();
        tokio::pin!(request);
        tokio::select! {
            response = &mut request => panic!("Sync returned before calendar change: {:?}", response.unwrap().status()),
            _ = tokio::time::sleep(Duration::from_millis(700)) => {}
        }
        let event = live.call_using(serde_json::json!([["CalendarEvent/set", {"accountId":live.account,
            "create":{"e":{"@type":"Event","calendarIds":{&calendar:true},"title":"Calendar wait fixture",
            "start":"2042-07-06T10:00:00","duration":"PT1H"}}}, "0"]]), &using).await;
        let id = event[0][1]["created"]["e"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let response = tokio::time::timeout(Duration::from_secs(10), &mut request)
            .await
            .unwrap()
            .unwrap();
        let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
        assert_eq!(
            eas::find_text_after(&doc, cal::SUBJECT),
            Some("Calendar wait fixture")
        );
        let cleanup = live
            .call_using(
                serde_json::json!([
            ["CalendarEvent/set", {"accountId":live.account,"destroy":[id]}, "0"],
            ["Calendar/set", {"accountId":live.account,"destroy":[calendar]}, "1"]]),
                &using,
            )
            .await;
        assert!(cleanup[0][1]["notDestroyed"].is_null());
        assert!(cleanup[1][1]["notDestroyed"].is_null());
    }

    #[tokio::test]
    #[ignore]
    async fn e2e_task_search_and_fetch() {
        use eas::{airsync_base as base, item_operations as ops, search as srch, tasks};
        let live = Live::new("E2E_USER2").await;
        let using = [
            "urn:ietf:params:jmap:core",
            "urn:ietf:params:jmap:calendars",
        ];
        let created = live
            .call_using(
                serde_json::json!([["Calendar/set", {"accountId":live.account,
            "create":{"c":{"name":format!("Search fixture {}", std::process::id())}}}, "0"]]),
                &using,
            )
            .await;
        let calendar = created[0][1]["created"]["c"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let marker = format!("tasksearchfixture{}", std::process::id());
        let events = live
            .call_using(
                serde_json::json!([["CalendarEvent/set", {"accountId":live.account,
            "create":{"e":{"@type":"Task","calendarIds":{&calendar:true}, "title":"Task search fixture",
            "description":&marker, "due":"2042-07-06T10:00:00", "timeZone":"America/Toronto", "progress":"needs-action"},
            "c":{"@type":"Event","calendarIds":{&calendar:true},"title":&marker,
            "start":"2042-07-06T10:00:00","duration":"PT1H"}}}, "0"]]),
                &using,
            )
            .await;
        let id = events[0][1]["created"]["e"]["id"]
            .as_str()
            .unwrap_or_else(|| panic!("event create failed: {events}"))
            .to_owned();
        let calendar_event_id = events[0][1]["created"]["c"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let contact_caps = ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:contacts"];
        let books = live
            .call_using(
                serde_json::json!([["AddressBook/get", {"accountId":live.account}, "0"]]),
                &contact_caps,
            )
            .await;
        let book = books[0][1]["list"][0]["id"].as_str().unwrap();
        let created_contact = live.call_using(serde_json::json!([["ContactCard/set", {"accountId":live.account,
            "create":{"c":{"@type":"Card", "version":"1.0", "addressBookIds":{book:true}, "name":{"full":&marker}}}}, "0"]]), &contact_caps).await;
        let contact_id = created_contact[0][1]["created"]["c"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let email_ids = live.seed(&live.inbox, 1, &marker, false).await;
        let mut b = eas::DocumentBuilder::new();
        b.start(srch::SEARCH);
        b.start(srch::STORE);
        b.leaf(srch::NAME, "Mailbox");
        b.start(srch::QUERY);
        b.start(srch::AND);
        b.leaf(srch::FREE_TEXT, &marker);
        b.end();
        b.end();
        b.end();
        b.end();
        let mixed = live
            .wbxml_version(
                "Search",
                &dev("mixed-search"),
                wbxml::encode_document(&b.finish()),
                "16.1",
            )
            .await;
        assert_eq!(
            eas::find_all_text_after(&mixed, srch::STATUS),
            vec!["1", "1"]
        );
        assert_eq!(eas::find_text_after(&mixed, srch::TOTAL), Some("4"));
        assert_eq!(
            eas::find_all_text_after(&mixed, air::CLASS),
            vec!["Email", "Contacts", "Calendar", "Tasks"]
        );
        let mut b = eas::DocumentBuilder::new();
        b.start(srch::SEARCH);
        b.start(srch::STORE);
        b.leaf(srch::NAME, "Mailbox");
        b.start(srch::QUERY);
        b.start(srch::AND);
        for class in ["Email", "Contacts", "Calendar", "Tasks"] {
            b.leaf(air::CLASS, class);
        }
        b.leaf(srch::FREE_TEXT, &marker);
        b.end();
        b.end();
        b.start(srch::OPTIONS);
        b.leaf(srch::RANGE, "1-2");
        b.end();
        b.end();
        b.end();
        let page = live
            .wbxml_version(
                "Search",
                &dev("mixed-search"),
                wbxml::encode_document(&b.finish()),
                "16.1",
            )
            .await;
        assert_eq!(
            eas::find_all_text_after(&page, srch::STATUS),
            vec!["1", "1"]
        );
        assert_eq!(eas::find_text_after(&page, srch::TOTAL), Some("4"));
        assert_eq!(eas::find_text_after(&page, srch::RANGE), Some("1-2"));
        assert_eq!(
            eas::find_all_text_after(&page, air::CLASS),
            vec!["Contacts", "Calendar"]
        );
        let folder = format!("task_{calendar}");
        let mut b = eas::DocumentBuilder::new();
        b.start(srch::SEARCH);
        b.start(srch::STORE);
        b.leaf(srch::NAME, "Mailbox");
        b.start(srch::QUERY);
        b.start(srch::AND);
        b.leaf(air::COLLECTION_ID, &folder);
        b.leaf(srch::FREE_TEXT, &marker);
        b.end();
        b.end();
        b.end();
        b.end();
        let result = live
            .wbxml_version(
                "Search",
                &dev("task-search"),
                wbxml::encode_document(&b.finish()),
                "16.1",
            )
            .await;
        assert_eq!(
            eas::find_all_text_after(&result, srch::STATUS),
            vec!["1", "1"]
        );
        assert_eq!(eas::find_text_after(&result, srch::TOTAL), Some("1"));
        assert_eq!(
            eas::find_text_after(&result, tasks::SUBJECT),
            Some("Task search fixture")
        );
        let long_id = eas::find_text_after(&result, srch::LONG_ID).unwrap();
        let mut b = eas::DocumentBuilder::new();
        b.start(ops::ITEM_OPERATIONS);
        b.start(ops::FETCH);
        b.leaf(ops::STORE, "Mailbox");
        b.leaf(srch::LONG_ID, long_id);
        b.end();
        b.end();
        let fetched = live
            .wbxml_version(
                "ItemOperations",
                &dev("task-search"),
                wbxml::encode_document(&b.finish()),
                "16.1",
            )
            .await;
        assert_eq!(
            eas::find_all_text_after(&fetched, ops::STATUS),
            vec!["1", "1"]
        );
        assert_eq!(
            eas::find_text_after(&fetched, tasks::SUBJECT),
            Some("Task search fixture")
        );
        assert_eq!(
            eas::find_text_after(&fetched, tasks::DUE_DATE),
            Some("2042-07-06T10:00:00.000Z")
        );
        assert_eq!(
            eas::find_text_after(&fetched, tasks::UTC_DUE_DATE),
            Some("2042-07-06T14:00:00.000Z")
        );
        assert_eq!(
            eas::find_text_after(&fetched, base::DATA),
            Some(marker.as_str())
        );
        let device = dev("task-body-edit");
        let prime = live
            .wbxml_version("Sync", &device, sync_body("0", &folder), "16.1")
            .await;
        let initial = live
            .wbxml_version(
                "Sync",
                &device,
                sync_body(&sync_key(&prime), &folder),
                "16.1",
            )
            .await;
        let mut b = eas::DocumentBuilder::new();
        b.start(air::SYNC);
        b.start(air::COLLECTIONS);
        b.start(air::COLLECTION);
        b.leaf(air::SYNC_KEY, sync_key(&initial));
        b.leaf(air::COLLECTION_ID, &folder);
        b.start(air::COMMANDS);
        b.start(air::CHANGE);
        b.leaf(air::SERVER_ID, &id);
        b.start(air::APPLICATION_DATA);
        b.start(base::BODY);
        b.leaf(base::TYPE, "1");
        b.leaf(base::TRUNCATED, "1");
        b.leaf(base::DATA, "incomplete");
        for _ in 0..7 {
            b.end();
        }
        let rejected = live
            .wbxml_version("Sync", &device, wbxml::encode_document(&b.finish()), "16.1")
            .await;
        assert!(eas::find_all_text_after(&rejected, air::STATUS).contains(&"5".to_owned()));
        let native = live
            .call_using(
                serde_json::json!([["CalendarEvent/get", {"accountId":live.account,
            "ids":[&id],"properties":["description"]}, "0"]]),
                &using,
            )
            .await;
        assert_eq!(native[0][1]["list"][0]["description"], marker);
        let mut b = eas::DocumentBuilder::new();
        b.start(air::SYNC);
        b.start(air::COLLECTIONS);
        b.start(air::COLLECTION);
        b.leaf(air::SYNC_KEY, sync_key(&rejected));
        b.leaf(air::COLLECTION_ID, &folder);
        b.start(air::COMMANDS);
        b.start(air::CHANGE);
        b.leaf(air::SERVER_ID, &id);
        b.start(air::APPLICATION_DATA);
        b.leaf(tasks::DUE_DATE, "2042-07-07T12:30:00.000Z");
        b.leaf(tasks::UTC_DUE_DATE, "2042-07-07T16:30:00.000Z");
        b.start(base::BODY);
        b.leaf(base::TYPE, "1");
        b.leaf(base::DATA, "updated task description");
        for _ in 0..7 {
            b.end();
        }
        let updated = live
            .wbxml_version("Sync", &device, wbxml::encode_document(&b.finish()), "16.1")
            .await;
        assert_eq!(
            eas::find_all_text_after(&updated, air::STATUS),
            vec!["1", "1"]
        );
        let native = live
            .call_using(
                serde_json::json!([["CalendarEvent/get", {"accountId":live.account,
            "ids":[&id],"properties":["description","due"]}, "0"]]),
                &using,
            )
            .await;
        assert_eq!(
            native[0][1]["list"][0]["description"],
            "updated task description"
        );
        assert_eq!(native[0][1]["list"][0]["due"], "2042-07-07T12:30:00");
        let mut b = eas::DocumentBuilder::new();
        b.start(air::SYNC);
        b.start(air::COLLECTIONS);
        b.start(air::COLLECTION);
        b.leaf(air::SYNC_KEY, sync_key(&updated));
        b.leaf(air::COLLECTION_ID, &folder);
        b.start(air::COMMANDS);
        b.start(air::CHANGE);
        b.leaf(air::SERVER_ID, &id);
        b.start(air::APPLICATION_DATA);
        b.leaf(tasks::UTC_DUE_DATE, "2042-07-07T17:30:00.000Z");
        for _ in 0..6 {
            b.end();
        }
        let utc_updated = live
            .wbxml_version("Sync", &device, wbxml::encode_document(&b.finish()), "16.1")
            .await;
        assert_eq!(
            eas::find_all_text_after(&utc_updated, air::STATUS),
            vec!["1", "1"]
        );
        let native = live
            .call_using(
                serde_json::json!([["CalendarEvent/get", {"accountId":live.account,
            "ids":[&id],"properties":["due"]}, "0"]]),
                &using,
            )
            .await;
        assert_eq!(native[0][1]["list"][0]["due"], "2042-07-07T13:30:00");

        let mut b = eas::DocumentBuilder::new();
        b.start(air::SYNC);
        b.start(air::COLLECTIONS);
        b.start(air::COLLECTION);
        b.leaf(air::SYNC_KEY, sync_key(&utc_updated));
        b.leaf(air::COLLECTION_ID, &folder);
        b.start(air::COMMANDS);
        b.start(air::CHANGE);
        b.leaf(air::SERVER_ID, &id);
        b.start(air::APPLICATION_DATA);
        b.leaf(tasks::DUE_DATE, "invalid-date");
        for _ in 0..6 {
            b.end();
        }
        let rejected = live
            .wbxml_version("Sync", &device, wbxml::encode_document(&b.finish()), "16.1")
            .await;
        assert!(eas::find_all_text_after(&rejected, air::STATUS).contains(&"5".to_owned()));
        let native = live
            .call_using(
                serde_json::json!([["CalendarEvent/get", {"accountId":live.account,
            "ids":[&id],"properties":["due"]}, "0"]]),
                &using,
            )
            .await;
        assert_eq!(native[0][1]["list"][0]["due"], "2042-07-07T13:30:00");

        let cleanup = live
            .call_using(
                serde_json::json!([
            ["CalendarEvent/set", {"accountId":live.account,"destroy":[id,calendar_event_id]}, "0"],
            ["Calendar/set", {"accountId":live.account,"destroy":[calendar]}, "1"]]),
                &using,
            )
            .await;
        assert!(cleanup[0][1]["notDestroyed"].is_null());
        assert!(cleanup[1][1]["notDestroyed"].is_null());
        let cleanup_contact = live.call_using(serde_json::json!([["ContactCard/set", {"accountId":live.account,"destroy":[contact_id]}, "0"]]), &contact_caps).await;
        assert!(cleanup_contact[0][1]["notDestroyed"].is_null());
        let cleanup_mail = live.call(serde_json::json!([["Email/set", {"accountId":live.account,"destroy":email_ids}, "0"]])).await;
        assert!(cleanup_mail[0][1]["notDestroyed"].is_null());
    }

    #[tokio::test]
    #[ignore]
    async fn e2e_calendar_search_and_fetch() {
        use eas::{calendar as cal, item_operations as ops, search as srch};
        let live = Live::new("E2E_USER2").await;
        let using = [
            "urn:ietf:params:jmap:core",
            "urn:ietf:params:jmap:calendars",
        ];
        let created = live
            .call_using(
                serde_json::json!([["Calendar/set", {"accountId":live.account,
            "create":{"c":{"name":format!("Search fixture {}", std::process::id())}}}, "0"]]),
                &using,
            )
            .await;
        let calendar = created[0][1]["created"]["c"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let marker = format!("calendarsearchfixture{}", std::process::id());
        let events = live
            .call_using(
                serde_json::json!([["CalendarEvent/set", {"accountId":live.account,
            "create":{"e":{"@type":"Event","calendarIds":{&calendar:true}, "title":&marker,
            "start":"2042-07-06T10:00:00", "duration":"PT1H"}}}, "0"]]),
                &using,
            )
            .await;
        let id = events[0][1]["created"]["e"]["id"]
            .as_str()
            .unwrap_or_else(|| panic!("event create failed: {events}"))
            .to_owned();
        let folder = format!("cal_{calendar}");
        let mut b = eas::DocumentBuilder::new();
        b.start(srch::SEARCH);
        b.start(srch::STORE);
        b.leaf(srch::NAME, "Mailbox");
        b.start(srch::QUERY);
        b.start(srch::AND);
        b.leaf(air::COLLECTION_ID, &folder);
        b.leaf(srch::FREE_TEXT, &marker);
        b.end();
        b.end();
        b.end();
        b.end();
        let result = live
            .wbxml_version(
                "Search",
                &dev("calendar-search"),
                wbxml::encode_document(&b.finish()),
                "16.1",
            )
            .await;
        assert_eq!(
            eas::find_all_text_after(&result, srch::STATUS),
            vec!["1", "1"]
        );
        assert_eq!(eas::find_text_after(&result, srch::TOTAL), Some("1"));
        assert_eq!(
            eas::find_text_after(&result, cal::SUBJECT),
            Some(marker.as_str())
        );
        let long_id = eas::find_text_after(&result, srch::LONG_ID).unwrap();
        let mut b = eas::DocumentBuilder::new();
        b.start(ops::ITEM_OPERATIONS);
        b.start(ops::FETCH);
        b.leaf(ops::STORE, "Mailbox");
        b.leaf(srch::LONG_ID, long_id);
        b.end();
        b.end();
        let fetched = live
            .wbxml_version(
                "ItemOperations",
                &dev("calendar-search"),
                wbxml::encode_document(&b.finish()),
                "16.1",
            )
            .await;
        assert_eq!(
            eas::find_all_text_after(&fetched, ops::STATUS),
            vec!["1", "1"]
        );
        assert_eq!(
            eas::find_text_after(&fetched, cal::SUBJECT),
            Some(marker.as_str())
        );
        let cleanup = live
            .call_using(
                serde_json::json!([
            ["CalendarEvent/set", {"accountId":live.account,"destroy":[id]}, "0"],
            ["Calendar/set", {"accountId":live.account,"destroy":[calendar]}, "1"]]),
                &using,
            )
            .await;
        assert!(cleanup[0][1]["notDestroyed"].is_null());
        assert!(cleanup[1][1]["notDestroyed"].is_null());
    }

    #[tokio::test]
    #[ignore]
    async fn e2e_meeting_response_updates_only_the_attendee() {
        use eas::meeting_response as mr;
        let live = Live::new("E2E_USER2").await;
        let using = [
            "urn:ietf:params:jmap:core",
            "urn:ietf:params:jmap:calendars",
        ];
        let created = live.call_using(serde_json::json!([["Calendar/set", {"accountId":live.account,
            "create":{"c":{"name":format!("Meeting response fixture {}", std::process::id())}}}, "0"]]), &using).await;
        let calendar = created[0][1]["created"]["c"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let created = live.call_using(serde_json::json!([["CalendarEvent/set", {"accountId":live.account,
            "create":{"e":{"@type":"Event","calendarIds":{&calendar:true},"title":"Meeting response fixture",
                "start":"2042-07-06T10:00:00","duration":"PT1H","replyTo":{"imip":"mailto:owner@example.test"},
                "participants":{
                  "owner":{"@type":"Participant","email":"owner@example.test","calendarAddress":"mailto:owner@example.test","roles":{"owner":true},"sendTo":{"imip":"mailto:owner@example.test"},"scheduleAgent":"client"},
                  "self":{"@type":"Participant","email":live.user,"calendarAddress":format!("mailto:{}",live.user),"roles":{"attendee":true},"participationStatus":"needs-action","sendTo":{"imip":format!("mailto:{}",live.user)},"scheduleAgent":"client"}
                }}}}, "0"]]), &using).await;
        let id = created[0][1]["created"]["e"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let folder = format!("cal_{calendar}");
        let device = dev("meeting-response");
        let prime = live
            .wbxml_version("Sync", &device, sync_body("0", &folder), "16.1")
            .await;
        let _initial = live
            .wbxml_version(
                "Sync",
                &device,
                sync_body(&sync_key(&prime), &folder),
                "16.1",
            )
            .await;
        let before = live.call_using(serde_json::json!([["CalendarEvent/get", {"accountId":live.account,"ids":[&id],"properties":["participants"]}, "0"]]), &using).await;
        let mut b = eas::DocumentBuilder::new();
        b.start(mr::MEETING_RESPONSE);
        b.start(mr::REQUEST);
        b.leaf(mr::USER_RESPONSE, "1");
        b.leaf(mr::COLLECTION_ID, &folder);
        b.leaf(mr::REQUEST_ID, &id);
        b.end();
        b.end();
        let response = live
            .wbxml_version(
                "MeetingResponse",
                &device,
                wbxml::encode_document(&b.finish()),
                "16.1",
            )
            .await;
        assert_eq!(eas::find_text_after(&response, mr::STATUS), Some("1"));
        assert_eq!(
            eas::find_text_after(&response, mr::CALENDAR_ID),
            Some(id.as_str())
        );
        let after = live.call_using(serde_json::json!([["CalendarEvent/get", {"accountId":live.account,"ids":[&id],"properties":["participants"]}, "0"]]), &using).await;
        let before = &before[0][1]["list"][0]["participants"];
        let after = &after[0][1]["list"][0]["participants"];
        assert_eq!(after["self"]["participationStatus"], "accepted");
        assert_eq!(after["owner"], before["owner"]);
        assert_eq!(after["self"]["roles"], before["self"]["roles"]);
        let mut b = eas::DocumentBuilder::new();
        b.start(mr::MEETING_RESPONSE);
        b.start(mr::REQUEST);
        b.leaf(mr::USER_RESPONSE, "2");
        b.leaf(mr::COLLECTION_ID, &folder);
        b.leaf(mr::REQUEST_ID, &id);
        b.end();
        b.end();
        let tentative = live
            .wbxml_version(
                "MeetingResponse",
                &device,
                wbxml::encode_document(&b.finish()),
                "16.1",
            )
            .await;
        assert_eq!(eas::find_text_after(&tentative, mr::STATUS), Some("1"));
        let native = live.call_using(serde_json::json!([["CalendarEvent/get", {"accountId":live.account,"ids":[&id],"properties":["participants"]}, "0"]]), &using).await;
        assert_eq!(
            native[0][1]["list"][0]["participants"]["self"]["participationStatus"],
            "tentative"
        );

        for (answer, version, send, expected) in [
            ("1", "14.0", false, "2"),
            ("1", "16.1", true, "4"),
            ("9", "16.1", false, "2"),
        ] {
            let mut b = eas::DocumentBuilder::new();
            b.start(mr::MEETING_RESPONSE);
            b.start(mr::REQUEST);
            b.leaf(mr::USER_RESPONSE, answer);
            b.leaf(mr::COLLECTION_ID, &folder);
            b.leaf(mr::REQUEST_ID, &id);
            if send {
                b.empty_tag(mr::SEND_RESPONSE);
            }
            b.end();
            b.end();
            let doc = live
                .wbxml_version(
                    "MeetingResponse",
                    &device,
                    wbxml::encode_document(&b.finish()),
                    version,
                )
                .await;
            assert_eq!(eas::find_text_after(&doc, mr::STATUS), Some(expected));
            assert!(eas::find_text_after(&doc, mr::CALENDAR_ID).is_none());
        }

        let event = live.call_using(serde_json::json!([["CalendarEvent/get", {"accountId":live.account,"ids":[&id],"properties":["uid"]}, "0"]]), &using).await;
        let uid = event[0][1]["list"][0]["uid"].as_str().unwrap();
        let calendar_text = format!("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//EAS Test//EN\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:{uid}\r\nDTSTAMP:20420701T100000Z\r\nDTSTART:20420706T100000Z\r\nDTEND:20420706T110000Z\r\nSUMMARY:Meeting response fixture\r\nORGANIZER:mailto:owner@example.test\r\nATTENDEE;PARTSTAT=NEEDS-ACTION:mailto:{}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n", live.user);
        let message = format!("From: owner@example.test\r\nTo: {}\r\nSubject: Meeting response fixture\r\nMIME-Version: 1.0\r\nContent-Type: text/calendar; charset=utf-8; method=REQUEST\r\n\r\n{calendar_text}", live.user);
        let upload: serde_json::Value = live
            .http
            .post(format!("{}/jmap/upload/{}/", live.jmap, live.account))
            .basic_auth(&live.user, Some(&live.pass))
            .header("Content-Type", "message/rfc822")
            .body(message)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let imported = live.call(serde_json::json!([["Email/import", {"accountId":live.account,
            "emails":{"m":{"blobId":upload["blobId"],"mailboxIds":{&live.inbox:true},"keywords":{}}}}, "0"]])).await;
        let mail_id = imported[0][1]["created"]["m"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let inbox_device = dev("meeting-inbox");
        let prime = live
            .wbxml_version("Sync", &inbox_device, sync_body("0", &live.inbox), "14.0")
            .await;
        let _initial = live
            .wbxml_version(
                "Sync",
                &inbox_device,
                sync_body(&sync_key(&prime), &live.inbox),
                "14.0",
            )
            .await;
        let mut b = eas::DocumentBuilder::new();
        b.start(mr::MEETING_RESPONSE);
        b.start(mr::REQUEST);
        b.leaf(mr::USER_RESPONSE, "1");
        b.leaf(mr::COLLECTION_ID, &live.inbox);
        b.leaf(mr::REQUEST_ID, &mail_id);
        b.end();
        b.end();
        let result = live
            .wbxml_version(
                "MeetingResponse",
                &inbox_device,
                wbxml::encode_document(&b.finish()),
                "14.0",
            )
            .await;
        assert_eq!(eas::find_text_after(&result, mr::STATUS), Some("1"));
        assert_eq!(
            eas::find_text_after(&result, mr::CALENDAR_ID),
            Some(id.as_str())
        );
        let mut b = eas::DocumentBuilder::new();
        b.start(mr::MEETING_RESPONSE);
        b.start(mr::REQUEST);
        b.leaf(mr::USER_RESPONSE, "1");
        b.leaf(eas::search::LONG_ID, &mail_id);
        b.end();
        b.end();
        let result = live
            .wbxml_version(
                "MeetingResponse",
                &inbox_device,
                wbxml::encode_document(&b.finish()),
                "14.0",
            )
            .await;
        assert_eq!(eas::find_text_after(&result, mr::STATUS), Some("1"));
        assert_eq!(
            eas::find_text_after(&result, mr::CALENDAR_ID),
            Some(id.as_str())
        );
        assert!(eas::find_text_after(&result, mr::REQUEST_ID).is_none());
        let mut b = eas::DocumentBuilder::new();
        b.start(mr::MEETING_RESPONSE);
        b.start(mr::REQUEST);
        b.leaf(mr::USER_RESPONSE, "2");
        b.leaf(eas::search::LONG_ID, &mail_id);
        b.leaf(mr::COLLECTION_ID, &live.inbox);
        b.leaf(mr::REQUEST_ID, &mail_id);
        b.end();
        b.end();
        let result = live
            .wbxml_version(
                "MeetingResponse",
                &inbox_device,
                wbxml::encode_document(&b.finish()),
                "14.0",
            )
            .await;
        assert_eq!(eas::find_text_after(&result, mr::STATUS), Some("2"));
        let forged = calendar_text.replace(
            "ORGANIZER:mailto:owner@example.test",
            "ORGANIZER:mailto:other@example.test",
        );
        let message = format!("From: other@example.test\r\nTo: {}\r\nSubject: Meeting response fixture\r\nMIME-Version: 1.0\r\nContent-Type: text/calendar; charset=utf-8; method=REQUEST\r\n\r\n{forged}", live.user);
        let upload: serde_json::Value = live
            .http
            .post(format!("{}/jmap/upload/{}/", live.jmap, live.account))
            .basic_auth(&live.user, Some(&live.pass))
            .header("Content-Type", "message/rfc822")
            .body(message)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let imported = live.call(serde_json::json!([["Email/import", {"accountId":live.account,
            "emails":{"m":{"blobId":upload["blobId"],"mailboxIds":{&live.inbox:true},"keywords":{}}}}, "0"]])).await;
        let forged_id = imported[0][1]["created"]["m"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let mut b = eas::DocumentBuilder::new();
        b.start(mr::MEETING_RESPONSE);
        b.start(mr::REQUEST);
        b.leaf(mr::USER_RESPONSE, "2");
        b.leaf(mr::COLLECTION_ID, &live.inbox);
        b.leaf(mr::REQUEST_ID, &forged_id);
        b.end();
        b.end();
        let rejected = live
            .wbxml_version(
                "MeetingResponse",
                &inbox_device,
                wbxml::encode_document(&b.finish()),
                "14.0",
            )
            .await;
        assert_eq!(eas::find_text_after(&rejected, mr::STATUS), Some("2"));
        let native = live.call_using(serde_json::json!([["CalendarEvent/get", {"accountId":live.account,"ids":[&id],"properties":["participants"]}, "0"]]), &using).await;
        assert_eq!(
            native[0][1]["list"][0]["participants"]["self"]["participationStatus"],
            "accepted"
        );
        let new_uid = format!("new-invite-{}@example.test", std::process::id());
        let new_calendar_text = calendar_text.replace(uid, &new_uid);
        let message = format!("From: owner@example.test\r\nTo: {}\r\nSubject: Meeting response fixture\r\nMIME-Version: 1.0\r\nContent-Type: text/calendar; charset=utf-8; method=REQUEST\r\n\r\n{new_calendar_text}", live.user);
        let upload: serde_json::Value = live
            .http
            .post(format!("{}/jmap/upload/{}/", live.jmap, live.account))
            .basic_auth(&live.user, Some(&live.pass))
            .header("Content-Type", "message/rfc822")
            .body(message)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let imported = live.call(serde_json::json!([["Email/import", {"accountId":live.account,
            "emails":{"m":{"blobId":upload["blobId"],"mailboxIds":{&live.inbox:true},"keywords":{}}}}, "0"]])).await;
        let new_mail = imported[0][1]["created"]["m"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let mut b = eas::DocumentBuilder::new();
        b.start(mr::MEETING_RESPONSE);
        b.start(mr::REQUEST);
        b.leaf(mr::USER_RESPONSE, "1");
        b.leaf(mr::COLLECTION_ID, &live.inbox);
        b.leaf(mr::REQUEST_ID, &new_mail);
        b.end();
        b.end();
        let request = wbxml::encode_document(&b.finish());
        let (first_response, second_response) = tokio::join!(
            live.wbxml_version("MeetingResponse", &inbox_device, request.clone(), "14.0"),
            live.wbxml_version("MeetingResponse", &inbox_device, request, "14.0")
        );
        assert_eq!(eas::find_text_after(&first_response, mr::STATUS), Some("1"));
        assert_eq!(
            eas::find_text_after(&second_response, mr::STATUS),
            Some("1")
        );
        let new_id = eas::find_text_after(&first_response, mr::CALENDAR_ID)
            .unwrap()
            .to_owned();
        assert_eq!(
            eas::find_text_after(&second_response, mr::CALENDAR_ID),
            Some(new_id.as_str())
        );
        let got = live.call_using(serde_json::json!([["CalendarEvent/get", {"accountId":live.account,"ids":[&new_id],"properties":["uid","participants"]}, "0"]]), &using).await;
        assert_eq!(got[0][1]["list"][0]["uid"], new_uid);
        let participants = got[0][1]["list"][0]["participants"].as_object().unwrap();
        assert!(participants
            .values()
            .any(|p| p["email"] == live.user && p["participationStatus"] == "accepted"));
        let query = live.call_using(serde_json::json!([["CalendarEvent/query", {"accountId":live.account,"filter":{"uid":&new_uid}}, "0"]]), &using).await;
        assert_eq!(query[0][1]["ids"].as_array().unwrap().len(), 1);
        let removed = live.call_using(serde_json::json!([["CalendarEvent/set", {"accountId":live.account,"destroy":[new_id]}, "0"]]), &using).await;
        assert!(removed[0][1]["notDestroyed"].is_null());
        let mut b = eas::DocumentBuilder::new();
        b.start(mr::MEETING_RESPONSE);
        b.start(mr::REQUEST);
        b.leaf(mr::USER_RESPONSE, "3");
        b.leaf(mr::COLLECTION_ID, &folder);
        b.leaf(mr::REQUEST_ID, &id);
        b.end();
        b.end();
        let declined = live
            .wbxml_version(
                "MeetingResponse",
                &device,
                wbxml::encode_document(&b.finish()),
                "16.1",
            )
            .await;
        assert_eq!(eas::find_text_after(&declined, mr::STATUS), Some("1"));
        assert!(eas::find_text_after(&declined, mr::CALENDAR_ID).is_none());
        let gone = live.call_using(serde_json::json!([["CalendarEvent/get", {"accountId":live.account,"ids":[&id]}, "0"]]), &using).await;
        assert!(gone[0][1]["list"].as_array().unwrap().is_empty());
        let mut b = eas::DocumentBuilder::new();
        b.start(mr::MEETING_RESPONSE);
        b.start(mr::REQUEST);
        b.leaf(mr::USER_RESPONSE, "3");
        b.leaf(mr::COLLECTION_ID, &live.inbox);
        b.leaf(mr::REQUEST_ID, &mail_id);
        b.end();
        b.end();
        let declined = live
            .wbxml_version(
                "MeetingResponse",
                &inbox_device,
                wbxml::encode_document(&b.finish()),
                "14.0",
            )
            .await;
        assert_eq!(eas::find_text_after(&declined, mr::STATUS), Some("1"));
        assert!(eas::find_text_after(&declined, mr::CALENDAR_ID).is_none());
        let query = live.call_using(serde_json::json!([["CalendarEvent/query", {"accountId":live.account,"filter":{"uid":uid}}, "0"]]), &using).await;
        assert!(query[0][1]["ids"].as_array().unwrap().is_empty());
        let second = live.call_using(serde_json::json!([["Calendar/set", {"accountId":live.account,"create":{"c":{"name":format!("Meeting shared fixture {}",std::process::id())}}}, "0"]]), &using).await;
        let second_calendar = second[0][1]["created"]["c"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let shared = live.call_using(serde_json::json!([["CalendarEvent/set", {"accountId":live.account,"create":{"e":{
            "@type":"Event","calendarIds":{&calendar:true,&second_calendar:true},"title":"Shared meeting fixture",
            "start":"2042-07-06T10:00:00","duration":"PT1H","replyTo":{"imip":"mailto:owner@example.test"},
            "participants":{
                "owner":{"@type":"Participant","email":"owner@example.test","calendarAddress":"mailto:owner@example.test","roles":{"owner":true},"sendTo":{"imip":"mailto:owner@example.test"},"scheduleAgent":"client"},
                "self":{"@type":"Participant","email":live.user,"calendarAddress":format!("mailto:{}",live.user),"roles":{"attendee":true},"participationStatus":"needs-action","sendTo":{"imip":format!("mailto:{}",live.user)},"scheduleAgent":"client"}
            }} }}, "0"]]), &using).await;
        let shared_id = shared[0][1]["created"]["e"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let mut b = eas::DocumentBuilder::new();
        b.start(mr::MEETING_RESPONSE);
        b.start(mr::REQUEST);
        b.leaf(mr::USER_RESPONSE, "3");
        b.leaf(mr::COLLECTION_ID, &folder);
        b.leaf(mr::REQUEST_ID, &shared_id);
        b.end();
        b.end();
        let declined = live
            .wbxml_version(
                "MeetingResponse",
                &device,
                wbxml::encode_document(&b.finish()),
                "16.1",
            )
            .await;
        assert_eq!(eas::find_text_after(&declined, mr::STATUS), Some("1"));
        let remaining = live.call_using(serde_json::json!([["CalendarEvent/get", {"accountId":live.account,"ids":[&shared_id],"properties":["calendarIds"]}, "0"]]), &using).await;
        assert!(remaining[0][1]["list"][0]["calendarIds"][&calendar] != true);
        assert_eq!(
            remaining[0][1]["list"][0]["calendarIds"][&second_calendar],
            true
        );
        let cleanup_shared = live.call_using(serde_json::json!([["CalendarEvent/set", {"accountId":live.account,"destroy":[shared_id]}, "0"], ["Calendar/set", {"accountId":live.account,"destroy":[second_calendar]}, "1"]]), &using).await;
        assert!(cleanup_shared[0][1]["notDestroyed"].is_null());
        assert!(cleanup_shared[1][1]["notDestroyed"].is_null());
        let own = live.call_using(serde_json::json!([["CalendarEvent/set", {"accountId":live.account,"create":{"e":{
            "@type":"Event","calendarIds":{&calendar:true},"title":"Organizer fixture",
            "start":"2042-07-06T10:00:00","duration":"PT1H","replyTo":{"imip":format!("mailto:{}",live.user)},
            "participants":{
                "owner":{"@type":"Participant","email":live.user,"calendarAddress":format!("mailto:{}",live.user),"roles":{"owner":true},"sendTo":{"imip":format!("mailto:{}",live.user)},"scheduleAgent":"client"},
                "other":{"@type":"Participant","email":"other@example.test","calendarAddress":"mailto:other@example.test","roles":{"attendee":true},"sendTo":{"imip":"mailto:other@example.test"},"scheduleAgent":"client"}
            }} }}, "0"]]), &using).await;
        let own_id = own[0][1]["created"]["e"]["id"].as_str().unwrap().to_owned();
        let mut b = eas::DocumentBuilder::new();
        b.start(mr::MEETING_RESPONSE);
        b.start(mr::REQUEST);
        b.leaf(mr::USER_RESPONSE, "3");
        b.leaf(mr::COLLECTION_ID, &folder);
        b.leaf(mr::REQUEST_ID, &own_id);
        b.end();
        b.end();
        let rejected = live
            .wbxml_version(
                "MeetingResponse",
                &device,
                wbxml::encode_document(&b.finish()),
                "16.1",
            )
            .await;
        assert_eq!(eas::find_text_after(&rejected, mr::STATUS), Some("2"));
        let still_there = live.call_using(serde_json::json!([["CalendarEvent/get", {"accountId":live.account,"ids":[&own_id]}, "0"]]), &using).await;
        assert_eq!(still_there[0][1]["list"].as_array().unwrap().len(), 1);
        let removed = live.call_using(serde_json::json!([["CalendarEvent/set", {"accountId":live.account,"destroy":[own_id]}, "0"]]), &using).await;
        assert!(removed[0][1]["notDestroyed"].is_null());
        let cleanup_mail = live.call(serde_json::json!([["Email/set", {"accountId":live.account,"destroy":[mail_id,forged_id,new_mail]}, "0"]])).await;
        assert!(cleanup_mail[0][1]["notDestroyed"].is_null());
        let cleanup = live.call_using(serde_json::json!([["Calendar/set", {"accountId":live.account,"destroy":[calendar]}, "0"]]), &using).await;
        assert!(cleanup[0][1]["notDestroyed"].is_null());
    }

    #[tokio::test]
    #[ignore]
    async fn e2e_calendar_response_type_tracks_the_current_user() {
        use eas::calendar as cal;
        let live = Live::new("E2E_USER2").await;
        let using = [
            "urn:ietf:params:jmap:core",
            "urn:ietf:params:jmap:calendars",
        ];
        let created = live
            .call_using(
                serde_json::json!([["Calendar/set", {"accountId":live.account,
            "create":{"c":{"name":format!("Response fixture {}", std::process::id())}}}, "0"]]),
                &using,
            )
            .await;
        let calendar = created[0][1]["created"]["c"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let event = live.call_using(serde_json::json!([["CalendarEvent/set", {"accountId":live.account,
            "create":{"e":{"@type":"Event", "calendarIds":{&calendar:true}, "title":"Response fixture",
                "start":"2042-07-06T10:00:00", "duration":"PT1H", "replyTo":{"imip":"mailto:owner@example.test"},
                "participants":{
                    "owner":{"@type":"Participant", "email":"owner@example.test", "calendarAddress":"mailto:owner@example.test", "roles":{"owner":true}, "sendTo":{"imip":"mailto:owner@example.test"}, "scheduleAgent":"client"},
                    "self":{"@type":"Participant", "email":live.user, "calendarAddress":format!("mailto:{}",live.user), "roles":{"attendee":true}, "participationStatus":"tentative", "sendTo":{"imip":format!("mailto:{}",live.user)}, "scheduleAgent":"client"}
                }}}}, "0"]]), &using).await;
        let id = event[0][1]["created"]["e"]["id"]
            .as_str()
            .unwrap_or_else(|| panic!("event creation failed: {event}"))
            .to_owned();
        let folder = format!("cal_{calendar}");
        let device = dev("response-type");
        let prime = live
            .wbxml_version("Sync", &device, sync_body("0", &folder), "16.1")
            .await;
        let initial = live
            .wbxml_version(
                "Sync",
                &device,
                sync_body(&sync_key(&prime), &folder),
                "16.1",
            )
            .await;
        assert_eq!(
            eas::find_text_after(&initial, cal::RESPONSE_TYPE),
            Some("2")
        );
        let updated = live
            .call_using(
                serde_json::json!([["CalendarEvent/set", {"accountId":live.account,
            "update":{&id:{"participants/self/participationStatus":"accepted"}}}, "0"]]),
                &using,
            )
            .await;
        assert!(updated[0][1]["notUpdated"].is_null());
        let changed = live
            .wbxml_version(
                "Sync",
                &device,
                sync_body(&sync_key(&initial), &folder),
                "16.1",
            )
            .await;
        assert_eq!(
            eas::find_text_after(&changed, cal::RESPONSE_TYPE),
            Some("3")
        );
        let cancelled = live
            .call_using(
                serde_json::json!([["CalendarEvent/set", {"accountId":live.account,
            "update":{&id:{"status":"cancelled"}}}, "0"]]),
                &using,
            )
            .await;
        assert!(cancelled[0][1]["notUpdated"].is_null());
        let cancelled_sync = live
            .wbxml_version(
                "Sync",
                &device,
                sync_body(&sync_key(&changed), &folder),
                "16.1",
            )
            .await;
        assert_eq!(
            eas::find_text_after(&cancelled_sync, cal::MEETING_STATUS),
            Some("7")
        );
        let native = live
            .call_using(
                serde_json::json!([["CalendarEvent/get", {"accountId":live.account,
            "ids":[&id],"properties":["participants"]}, "0"]]),
                &using,
            )
            .await;
        let participants = native[0][1]["list"][0]["participants"].clone();
        let owners = participants
            .as_object()
            .unwrap()
            .values()
            .filter(|item| item["roles"]["owner"] == true)
            .count();
        let mut key = sync_key(&cancelled_sync);
        if owners > 1 {
            let mut b = eas::DocumentBuilder::new();
            b.start(air::SYNC);
            b.start(air::COLLECTIONS);
            b.start(air::COLLECTION);
            b.leaf(air::SYNC_KEY, &key);
            b.leaf(air::COLLECTION_ID, &folder);
            b.start(air::COMMANDS);
            b.start(air::CHANGE);
            b.leaf(air::SERVER_ID, &id);
            b.start(air::APPLICATION_DATA);
            b.start(cal::ATTENDEES);
            b.start(cal::ATTENDEE);
            b.leaf(cal::ATTENDEE_EMAIL, &live.user);
            for _ in 0..8 {
                b.end();
            }
            let rejected = live
                .wbxml_version("Sync", &device, wbxml::encode_document(&b.finish()), "16.1")
                .await;
            assert!(eas::find_all_text_after(&rejected, air::STATUS).contains(&"5".to_owned()));
            key = sync_key(&rejected);
            let after = live
                .call_using(
                    serde_json::json!([["CalendarEvent/get", {"accountId":live.account,
                "ids":[&id],"properties":["participants"]}, "0"]]),
                    &using,
                )
                .await;
            assert_eq!(after[0][1]["list"][0]["participants"], participants);
        }
        let mut b = eas::DocumentBuilder::new();
        b.start(air::SYNC);
        b.start(air::COLLECTIONS);
        b.start(air::COLLECTION);
        b.leaf(air::SYNC_KEY, &key);
        b.leaf(air::COLLECTION_ID, &folder);
        b.start(air::COMMANDS);
        b.start(air::CHANGE);
        b.leaf(air::SERVER_ID, &id);
        b.start(air::APPLICATION_DATA);
        b.leaf(cal::SUBJECT, "must not apply");
        b.leaf(cal::RESPONSE_TYPE, "4");
        for _ in 0..6 {
            b.end();
        }
        let rejected = live
            .wbxml_version("Sync", &device, wbxml::encode_document(&b.finish()), "16.1")
            .await;
        assert!(eas::find_all_text_after(&rejected, air::STATUS).contains(&"6".to_owned()));
        let native = live
            .call_using(
                serde_json::json!([["CalendarEvent/get", {"accountId":live.account,
            "ids":[&id],"properties":["title"]}, "0"]]),
                &using,
            )
            .await;
        assert_eq!(native[0][1]["list"][0]["title"], "Response fixture");
        let cleanup = live
            .call_using(
                serde_json::json!([
            ["CalendarEvent/set", {"accountId":live.account,"destroy":[id]}, "0"],
            ["Calendar/set", {"accountId":live.account,"destroy":[calendar]}, "1"]]),
                &using,
            )
            .await;
        assert!(cleanup[0][1]["notDestroyed"].is_null());
        assert!(cleanup[1][1]["notDestroyed"].is_null());
    }

    #[tokio::test]
    #[ignore]
    async fn e2e_calendar_uid_matches_stalwart_and_other_device() {
        use eas::calendar as cal;
        let live = Live::new("E2E_USER2").await;
        let using = [
            "urn:ietf:params:jmap:core",
            "urn:ietf:params:jmap:calendars",
        ];
        let created = live
            .call_using(
                serde_json::json!([["Calendar/set",{"accountId":live.account,
            "create":{"c":{"name":format!("UID fixture {}",std::process::id())}}},"0"]]),
                &using,
            )
            .await;
        let calendar = created[0][1]["created"]["c"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let folder = format!("cal_{calendar}");
        let device = dev("calendar-uid");
        let peer = dev("calendar-uid-peer");
        let prime = live
            .wbxml_version("Sync", &device, sync_body("0", &folder), "16.1")
            .await;
        let initial = live
            .wbxml_version(
                "Sync",
                &device,
                sync_body(&sync_key(&prime), &folder),
                "16.1",
            )
            .await;
        let peer_prime = live
            .wbxml_version("Sync", &peer, sync_body("0", &folder), "16.1")
            .await;
        let peer_initial = live
            .wbxml_version(
                "Sync",
                &peer,
                sync_body(&sync_key(&peer_prime), &folder),
                "16.1",
            )
            .await;
        let mut b = eas::DocumentBuilder::new();
        b.start(air::SYNC);
        b.start(air::COLLECTIONS);
        b.start(air::COLLECTION);
        b.leaf(air::SYNC_KEY, sync_key(&initial));
        b.leaf(air::COLLECTION_ID, &folder);
        b.start(air::COMMANDS);
        b.start(air::ADD);
        b.leaf(air::CLIENT_ID, "uid-client");
        b.start(air::APPLICATION_DATA);
        b.leaf(cal::SUBJECT, "UID fixture");
        b.leaf(cal::START_TIME, "20420706T100000Z");
        b.leaf(cal::END_TIME, "20420706T110000Z");
        for _ in 0..6 {
            b.end();
        }
        let added = live
            .wbxml_version("Sync", &device, wbxml::encode_document(&b.finish()), "16.1")
            .await;
        assert_eq!(
            eas::find_all_text_after(&added, air::STATUS),
            vec!["1", "1"]
        );
        let id = eas::find_text_after(&added, air::SERVER_ID)
            .unwrap()
            .to_owned();
        let uid = eas::find_text_after(&added, cal::UID).unwrap().to_owned();
        let native = live
            .call_using(
                serde_json::json!([["CalendarEvent/get",{"accountId":live.account,
            "ids":[&id],"properties":["uid"]},"0"]]),
                &using,
            )
            .await;
        assert_eq!(native[0][1]["list"][0]["uid"], uid);
        let peer_sync = live
            .wbxml_version(
                "Sync",
                &peer,
                sync_body(&sync_key(&peer_initial), &folder),
                "16.1",
            )
            .await;
        assert_eq!(
            eas::find_text_after(&peer_sync, cal::UID),
            Some(uid.as_str())
        );
        let cleanup = live
            .call_using(
                serde_json::json!([
            ["CalendarEvent/set",{"accountId":live.account,"destroy":[id]},"0"],
            ["Calendar/set",{"accountId":live.account,"destroy":[calendar]},"1"]]),
                &using,
            )
            .await;
        assert!(cleanup[0][1]["notDestroyed"].is_null());
        assert!(cleanup[1][1]["notDestroyed"].is_null());
    }

    #[tokio::test]
    #[ignore]
    async fn e2e_calendar_add_retries_without_duplicate_on_unsupported_server() {
        use eas::calendar as cal;
        let live = Live::new("E2E_USER2").await;
        let using = [
            "urn:ietf:params:jmap:core",
            "urn:ietf:params:jmap:calendars",
        ];
        let created = live
            .call_using(
                serde_json::json!([["Calendar/set",{"accountId":live.account,
            "create":{"c":{"name":format!("Reminder retry {}",std::process::id())}}},"0"]]),
                &using,
            )
            .await;
        let calendar = created[0][1]["created"]["c"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let folder = format!("cal_{calendar}");
        let device = dev("reminder-add-retry");
        let prime = live
            .wbxml_version("Sync", &device, sync_body("0", &folder), "16.1")
            .await;
        let sync = live
            .wbxml_version(
                "Sync",
                &device,
                sync_body(&sync_key(&prime), &folder),
                "16.1",
            )
            .await;
        let request = |key: &str| {
            let mut b = eas::DocumentBuilder::new();
            b.start(air::SYNC);
            b.start(air::COLLECTIONS);
            b.start(air::COLLECTION);
            b.leaf(air::SYNC_KEY, key);
            b.leaf(air::COLLECTION_ID, &folder);
            b.start(air::COMMANDS);
            b.start(air::ADD);
            b.leaf(air::CLIENT_ID, "retry");
            b.start(air::APPLICATION_DATA);
            b.leaf(cal::SUBJECT, "Reminder retry");
            b.leaf(cal::START_TIME, "20420706T100000Z");
            b.leaf(cal::END_TIME, "20420706T110000Z");
            b.leaf(cal::REMINDER, "15");
            for _ in 0..6 {
                b.end();
            }
            wbxml::encode_document(&b.finish())
        };
        let first = live
            .wbxml_version("Sync", &device, request(&sync_key(&sync)), "16.1")
            .await;
        assert!(eas::find_all_text_after(&first, air::STATUS).contains(&"5".to_owned()));
        let query = || {
            serde_json::json!([["CalendarEvent/query",{"accountId":live.account,
            "filter":{"inCalendar":&calendar}},"0"]])
        };
        let before = live.call_using(query(), &using).await;
        let ids = before[0][1]["ids"].as_array().unwrap().clone();
        assert_eq!(ids.len(), 1);
        let retry = live
            .wbxml_version("Sync", &device, request(&sync_key(&first)), "16.1")
            .await;
        assert!(eas::find_all_text_after(&retry, air::STATUS).contains(&"5".to_owned()));
        let after = live.call_using(query(), &using).await;
        assert_eq!(after[0][1]["ids"].as_array().unwrap(), &ids);
        let cleanup = live
            .call_using(
                serde_json::json!([
            ["CalendarEvent/set",{"accountId":live.account,"destroy":ids},"0"],
            ["Calendar/set",{"accountId":live.account,"destroy":[calendar]},"1"]]),
                &using,
            )
            .await;
        assert!(cleanup[0][1]["notDestroyed"].is_null());
        assert!(cleanup[1][1]["notDestroyed"].is_null());
    }

    #[tokio::test]
    #[ignore]
    async fn e2e_calendar_default_reminders_are_not_overwritten() {
        use eas::calendar as cal;
        let live = Live::new("E2E_USER2").await;
        let using = [
            "urn:ietf:params:jmap:core",
            "urn:ietf:params:jmap:calendars",
        ];
        let created=live.call_using(serde_json::json!([["Calendar/set",{"accountId":live.account,
            "create":{"c":{"name":format!("Default reminder {}",std::process::id()),
                "defaultAlertsWithTime":{"a":{"@type":"Alert","action":"display",
                    "trigger":{"@type":"OffsetTrigger","offset":"-PT20M","relativeTo":"start"}}}}}},"0"]]),&using).await;
        let calendar = created[0][1]["created"]["c"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let folder = format!("cal_{calendar}");
        let created=live.call_using(serde_json::json!([["CalendarEvent/set",{"accountId":live.account,
            "create":{"e":{"@type":"Event","calendarIds":{&calendar:true},"title":"Default alert fixture",
                "start":"2042-07-05T10:00:00","duration":"PT1H","useDefaultAlerts":true,
                "alerts":{"email":{"@type":"Alert","action":"email","trigger":{"@type":"OffsetTrigger","offset":"-PT1H"}}}}}},"0"]]),&using).await;
        let id = created[0][1]["created"]["e"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let device = dev("default-reminder");
        let prime = live
            .wbxml_version("Sync", &device, sync_body("0", &folder), "16.1")
            .await;
        let sync = live
            .wbxml_version(
                "Sync",
                &device,
                sync_body(&sync_key(&prime), &folder),
                "16.1",
            )
            .await;
        assert_eq!(
            eas::find_text_after(&sync, air::SERVER_ID),
            Some(id.as_str())
        );
        assert_eq!(eas::find_text_after(&sync, cal::REMINDER), Some("20"));
        let mut unchanged = eas::DocumentBuilder::new();
        unchanged.start(air::SYNC);
        unchanged.start(air::COLLECTIONS);
        unchanged.start(air::COLLECTION);
        unchanged.leaf(air::SYNC_KEY, sync_key(&sync));
        unchanged.leaf(air::COLLECTION_ID, &folder);
        unchanged.start(air::COMMANDS);
        unchanged.start(air::CHANGE);
        unchanged.leaf(air::SERVER_ID, &id);
        unchanged.start(air::APPLICATION_DATA);
        unchanged.leaf(cal::REMINDER, "20");
        unchanged.leaf(cal::SUBJECT, "Preserved default");
        for _ in 0..6 {
            unchanged.end();
        }
        let same = live
            .wbxml_version(
                "Sync",
                &device,
                wbxml::encode_document(&unchanged.finish()),
                "16.1",
            )
            .await;
        assert!(eas::find_all_text_after(&same, air::STATUS)
            .iter()
            .all(|value| value == "1"));
        let native = live
            .call_using(
                serde_json::json!([["CalendarEvent/get",{"accountId":live.account,
            "ids":[&id],"properties":["title","useDefaultAlerts"]},"0"]]),
                &using,
            )
            .await;
        assert_eq!(native[0][1]["list"][0]["title"], "Preserved default");
        assert_eq!(native[0][1]["list"][0]["useDefaultAlerts"], true);
        let mut b = eas::DocumentBuilder::new();
        b.start(air::SYNC);
        b.start(air::COLLECTIONS);
        b.start(air::COLLECTION);
        b.leaf(air::SYNC_KEY, sync_key(&same));
        b.leaf(air::COLLECTION_ID, &folder);
        b.start(air::COMMANDS);
        b.start(air::CHANGE);
        b.leaf(air::SERVER_ID, &id);
        b.start(air::APPLICATION_DATA);
        b.leaf(cal::REMINDER, "30");
        b.leaf(cal::SUBJECT, "must not apply");
        for _ in 0..6 {
            b.end();
        }
        let changed = live
            .wbxml_version("Sync", &device, wbxml::encode_document(&b.finish()), "16.1")
            .await;
        assert!(eas::find_all_text_after(&changed, air::STATUS).contains(&"5".to_owned()));
        let native = live
            .call_using(
                serde_json::json!([["CalendarEvent/get",{"accountId":live.account,
            "ids":[&id],"properties":["title","useDefaultAlerts"]},"0"]]),
                &using,
            )
            .await;
        assert_eq!(native[0][1]["list"][0]["title"], "Preserved default");
        assert_eq!(native[0][1]["list"][0]["useDefaultAlerts"], true);
        let mut b = eas::DocumentBuilder::new();
        b.start(air::SYNC);
        b.start(air::COLLECTIONS);
        b.start(air::COLLECTION);
        b.leaf(air::SYNC_KEY, sync_key(&changed));
        b.leaf(air::COLLECTION_ID, &folder);
        b.start(air::COMMANDS);
        b.start(air::ADD);
        b.leaf(air::CLIENT_ID, "reminder-add");
        b.start(air::APPLICATION_DATA);
        b.leaf(cal::SUBJECT, "Created with reminder");
        b.leaf(cal::START_TIME, "20420706T100000Z");
        b.leaf(cal::END_TIME, "20420706T110000Z");
        b.leaf(cal::REMINDER, "15");
        for _ in 0..6 {
            b.end();
        }
        let request = wbxml::encode_document(&b.finish());
        let added = live
            .wbxml_version("Sync", &device, request.clone(), "16.1")
            .await;
        assert_eq!(
            eas::find_all_text_after(&added, air::STATUS),
            vec!["1", "1"]
        );
        let added_id = eas::find_text_after(&added, air::SERVER_ID)
            .unwrap()
            .to_owned();
        assert_eq!(
            live.wbxml_version("Sync", &device, request.clone(), "16.1")
                .await,
            added
        );
        let mut retry = wbxml::decode_document(&request).unwrap();
        for index in 0..retry.nodes.len().saturating_sub(1) {
            if matches!(&retry.nodes[index], wbxml::Node::Start(token) if token.code_page == air::PAGE && token.token == air::SYNC_KEY.token)
            {
                retry.nodes[index + 1] = wbxml::Node::Text(sync_key(&added));
                break;
            }
        }
        let retried = live
            .wbxml_version("Sync", &device, wbxml::encode_document(&retry), "16.1")
            .await;
        assert_eq!(
            eas::find_all_text_after(&retried, air::STATUS),
            vec!["1", "1"]
        );
        let reused_id = eas::find_text_after(&retried, air::SERVER_ID)
            .unwrap()
            .to_owned();
        assert_ne!(reused_id, added_id);
        let created = live
            .call_using(
                serde_json::json!([["CalendarEvent/get",{"accountId":live.account,
            "ids":[&added_id],"properties":["alerts","useDefaultAlerts"]},"0"]]),
                &using,
            )
            .await;
        assert_eq!(created[0][1]["list"][0]["useDefaultAlerts"], false);
        assert_eq!(
            crate::jmap::calendar_reminder::read_reminder(&created[0][1]["list"][0]["alerts"]),
            Some(15)
        );
        let query = live
            .call_using(
                serde_json::json!([["CalendarEvent/query",{"accountId":live.account,
            "filter":{"inCalendar":calendar}},"0"]]),
                &using,
            )
            .await;
        assert_eq!(query[0][1]["ids"].as_array().unwrap().len(), 3);
        let second=live.call_using(serde_json::json!([["CalendarEvent/set",{"accountId":live.account,
            "create":{"e":{"@type":"Event","calendarIds":{&calendar:true},"title":"Default only fixture",
                "start":"2042-07-06T10:00:00","duration":"PT1H","useDefaultAlerts":true}}},"0"]]),&using).await;
        let second_id = second[0][1]["created"]["e"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let sync_second = live
            .wbxml_version(
                "Sync",
                &device,
                sync_body(&sync_key(&retried), &folder),
                "16.1",
            )
            .await;
        assert!(eas::find_all_text_after(&sync_second, air::SERVER_ID).contains(&second_id));
        let mut b = eas::DocumentBuilder::new();
        b.start(air::SYNC);
        b.start(air::COLLECTIONS);
        b.start(air::COLLECTION);
        b.leaf(air::SYNC_KEY, sync_key(&sync_second));
        b.leaf(air::COLLECTION_ID, &folder);
        b.start(air::COMMANDS);
        b.start(air::CHANGE);
        b.leaf(air::SERVER_ID, &second_id);
        b.start(air::APPLICATION_DATA);
        b.leaf(cal::REMINDER, "30");
        for _ in 0..6 {
            b.end();
        }
        let changed = live
            .wbxml_version("Sync", &device, wbxml::encode_document(&b.finish()), "16.1")
            .await;
        assert!(eas::find_all_text_after(&changed, air::STATUS)
            .iter()
            .all(|value| value == "1"));
        let native = live
            .call_using(
                serde_json::json!([["CalendarEvent/get",{"accountId":live.account,
            "ids":[&second_id],"properties":["alerts","useDefaultAlerts"]},"0"]]),
                &using,
            )
            .await;
        assert_eq!(native[0][1]["list"][0]["useDefaultAlerts"], false);
        assert_eq!(
            crate::jmap::calendar_reminder::read_reminder(&native[0][1]["list"][0]["alerts"]),
            Some(30)
        );
        let cleanup = live
            .call_using(
                serde_json::json!([
            ["CalendarEvent/set",{"accountId":live.account,"destroy":[id,second_id,added_id,reused_id]},"0"],
            ["Calendar/set",{"accountId":live.account,"destroy":[calendar]},"1"]]),
                &using,
            )
            .await;
        assert!(cleanup[0][1]["notDestroyed"].is_null());
        assert!(cleanup[1][1]["notDestroyed"].is_null());
    }

    #[tokio::test]
    #[ignore]
    async fn e2e_calendar_reminders_preserve_other_alerts() {
        use eas::calendar as cal;
        let live = Live::new("E2E_USER2").await;
        let using = [
            "urn:ietf:params:jmap:core",
            "urn:ietf:params:jmap:calendars",
        ];
        let calendars = live
            .call_using(
                serde_json::json!([["Calendar/get",{"accountId":live.account},"0"]]),
                &using,
            )
            .await;
        let calendar = calendars[0][1]["list"][0]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let folder = format!("cal_{calendar}");
        let created=live.call_using(serde_json::json!([["CalendarEvent/set",{"accountId":live.account,
            "create":{"e":{"@type":"Event","calendarIds":{&calendar:true},"title":"Reminder fixture",
                "start":"2042-06-05T10:00:00","timeZone":"Etc/UTC","duration":"PT1H","useDefaultAlerts":false,
                "alerts":{"display":{"@type":"Alert","action":"display","trigger":{"@type":"OffsetTrigger","offset":"-PT15M"}},
                    "email":{"@type":"Alert","action":"email","trigger":{"@type":"OffsetTrigger","offset":"-PT1H"}}}}}},"0"]]),&using).await;
        let id = created[0][1]["created"]["e"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let mut devices = Vec::new();
        for prefix in ["calendar-reminder-a", "calendar-reminder-b"] {
            let device = dev(prefix);
            let prime = live
                .wbxml_version("Sync", &device, sync_body("0", &folder), "16.1")
                .await;
            let sync = live
                .wbxml_version(
                    "Sync",
                    &device,
                    sync_body(&sync_key(&prime), &folder),
                    "16.1",
                )
                .await;
            let commands=eas::child_elements(&sync).into_iter().flat_map(|node|eas::child_elements(&node))
                .flat_map(|node|eas::child_elements(&node))
                .find(|node|matches!(node.nodes.first(),Some(wbxml::Node::Start(t)) if t.code_page==air::PAGE && t.token==air::COMMANDS.token)).unwrap();
            let item = eas::child_elements(&commands)
                .into_iter()
                .find(|item| eas::find_text_after(item, air::SERVER_ID) == Some(id.as_str()))
                .unwrap();
            assert_eq!(eas::find_text_after(&item, cal::REMINDER), Some("15"));
            devices.push((device, sync_key(&sync)));
        }
        for value in [Some("30"), None, Some("0")] {
            let mut b = eas::DocumentBuilder::new();
            b.start(air::SYNC);
            b.start(air::COLLECTIONS);
            b.start(air::COLLECTION);
            b.leaf(air::SYNC_KEY, &devices[0].1);
            b.leaf(air::COLLECTION_ID, &folder);
            b.start(air::COMMANDS);
            b.start(air::CHANGE);
            b.leaf(air::SERVER_ID, &id);
            b.start(air::APPLICATION_DATA);
            if let Some(value) = value {
                b.leaf(cal::REMINDER, value);
            } else {
                b.empty_tag(cal::REMINDER);
            }
            for _ in 0..6 {
                b.end();
            }
            let response = live
                .wbxml_version(
                    "Sync",
                    &devices[0].0,
                    wbxml::encode_document(&b.finish()),
                    "16.1",
                )
                .await;
            assert!(eas::find_all_text_after(&response, air::STATUS)
                .iter()
                .all(|v| v == "1"));
            devices[0].1 = sync_key(&response);
            let native = live
                .call_using(
                    serde_json::json!([["CalendarEvent/get",{"accountId":live.account,
                "ids":[&id],"properties":["title","alerts","useDefaultAlerts"]},"0"]]),
                    &using,
                )
                .await;
            let event = &native[0][1]["list"][0];
            assert_eq!(event["title"], "Reminder fixture");
            assert_eq!(event["useDefaultAlerts"], false);
            assert_eq!(event["alerts"]["email"]["action"], "email");
            assert_eq!(event["alerts"]["email"]["trigger"]["offset"], "-PT1H");
            assert_eq!(
                crate::jmap::calendar_reminder::read_reminder(&event["alerts"]),
                value.map(|v| v.parse().unwrap())
            );
            let peer = live
                .wbxml_version(
                    "Sync",
                    &devices[1].0,
                    sync_body(&devices[1].1, &folder),
                    "16.1",
                )
                .await;
            assert_eq!(
                eas::find_text_after(&peer, air::SERVER_ID),
                Some(id.as_str())
            );
            assert_eq!(eas::find_text_after(&peer, cal::REMINDER), value);
            assert!(eas::contains_token(&peer, cal::REMINDER));
            devices[1].1 = sync_key(&peer);
        }
        let extra = live.call_using(serde_json::json!([["CalendarEvent/set",{
            "accountId":live.account,"update":{&id:{"alerts/other":{"@type":"Alert","action":"display",
                "trigger":{"@type":"OffsetTrigger","offset":"-PT5M"}}}}},"0"]]),&using).await;
        assert!(extra[0][1]["updated"].get(&id).is_some());
        let mut b = eas::DocumentBuilder::new();
        b.start(air::SYNC);
        b.start(air::COLLECTIONS);
        b.start(air::COLLECTION);
        b.leaf(air::SYNC_KEY, &devices[0].1);
        b.leaf(air::COLLECTION_ID, &folder);
        b.start(air::COMMANDS);
        b.start(air::CHANGE);
        b.leaf(air::SERVER_ID, &id);
        b.start(air::APPLICATION_DATA);
        b.leaf(cal::REMINDER, "20");
        b.leaf(cal::SUBJECT, "must not apply");
        for _ in 0..6 {
            b.end();
        }
        let rejected = live
            .wbxml_version(
                "Sync",
                &devices[0].0,
                wbxml::encode_document(&b.finish()),
                "16.1",
            )
            .await;
        assert!(eas::find_all_text_after(&rejected, air::STATUS).contains(&"5".to_owned()));
        let native = live
            .call_using(
                serde_json::json!([["CalendarEvent/get",{"accountId":live.account,
            "ids":[&id],"properties":["title","alerts"]},"0"]]),
                &using,
            )
            .await;
        assert_eq!(native[0][1]["list"][0]["title"], "Reminder fixture");
        assert!(native[0][1]["list"][0]["alerts"].get("other").is_some());
        let cleanup=live.call_using(serde_json::json!([["CalendarEvent/set",{"accountId":live.account,"destroy":[id]},"0"]]),&using).await;
        assert!(cleanup[0][1]["notDestroyed"].is_null());
    }

    #[tokio::test]
    #[ignore]
    async fn e2e_calendar_descriptions_round_trip_between_devices() {
        use eas::{airsync_base as base, calendar as cal, RequestedBodyPreference as Pref};
        let live = Live::new("E2E_USER2").await;
        let using = [
            "urn:ietf:params:jmap:core",
            "urn:ietf:params:jmap:calendars",
        ];
        let result = live
            .call_using(
                serde_json::json!([["Calendar/get",{"accountId":live.account},"0"]]),
                &using,
            )
            .await;
        let calendar = result[0][1]["list"][0]["id"].as_str().unwrap().to_owned();
        let folder = format!("cal_{calendar}");
        let result = live
            .call_using(
                serde_json::json!([["CalendarEvent/set",{
            "accountId":live.account, "create":{"e":{
                "@type":"Event","calendarIds":{&calendar:true},"title":"Description fixture",
                "start":"2026-11-15T10:00:00","duration":"PT1H",
                "description":"<p>Initial &amp; details</p>","descriptionContentType":"text/html"
            }}
        },"0"]]),
                &using,
            )
            .await;
        let id = result[0][1]["created"]["e"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let options = requested_body_options(&[Pref {
            body_type: 2,
            ..Default::default()
        }]);
        let mut devices = Vec::new();
        for prefix in ["calendar-description-a", "calendar-description-b"] {
            let device = dev(prefix);
            let prime = live
                .wbxml_version(
                    "Sync",
                    &device,
                    sync_body_preferences("0", &folder, Some(&options), true),
                    "16.1",
                )
                .await;
            let doc = live
                .wbxml_version(
                    "Sync",
                    &device,
                    sync_body_preferences(&sync_key(&prime), &folder, None, true),
                    "16.1",
                )
                .await;
            let collection = eas::child_elements(&doc).into_iter().flat_map(|n|eas::child_elements(&n))
                .find(|n|matches!(n.nodes.first(),Some(wbxml::Node::Start(t)) if t.code_page == air::PAGE && t.token == air::COLLECTION.token)).unwrap();
            let commands = eas::child_elements(&collection).into_iter()
                .find(|n|matches!(n.nodes.first(),Some(wbxml::Node::Start(t)) if t.code_page == air::PAGE && t.token == air::COMMANDS.token)).unwrap();
            let item = eas::child_elements(&commands)
                .into_iter()
                .find(|n| eas::find_text_after(n, air::SERVER_ID) == Some(id.as_str()))
                .unwrap();
            assert_eq!(eas::find_text_after(&item, base::TYPE), Some("2"));
            assert_eq!(
                eas::find_text_after(&item, base::DATA),
                Some("<p>Initial &amp; details</p>")
            );
            devices.push((device, sync_key(&doc)));
        }
        for (kind, data, expected_type, expected_data) in [
            (
                "2",
                "<p>Updated details</p>",
                "text/html",
                "<p>Updated details</p>",
            ),
            ("1", "", "text/plain", ""),
        ] {
            let mut b = eas::DocumentBuilder::new();
            b.start(air::SYNC);
            b.start(air::COLLECTIONS);
            b.start(air::COLLECTION);
            b.leaf(air::SYNC_KEY, &devices[0].1);
            b.leaf(air::COLLECTION_ID, &folder);
            b.start(air::COMMANDS);
            b.start(air::CHANGE);
            b.leaf(air::SERVER_ID, &id);
            b.start(air::APPLICATION_DATA);
            b.start(base::BODY);
            b.leaf(base::TYPE, kind);
            b.leaf(base::DATA, data);
            b.end();
            for _ in 0..6 {
                b.end();
            }
            let doc = live
                .wbxml_version(
                    "Sync",
                    &devices[0].0,
                    wbxml::encode_document(&b.finish()),
                    "16.1",
                )
                .await;
            assert!(eas::find_all_text_after(&doc, air::STATUS)
                .iter()
                .all(|s| s == "1"));
            devices[0].1 = sync_key(&doc);
            let native=live.call_using(serde_json::json!([["CalendarEvent/get",{
                "accountId":live.account,"ids":[&id],"properties":["id","title","description","descriptionContentType"]
            },"0"]]),&using).await;
            assert_eq!(native[0][1]["list"][0]["title"], "Description fixture");
            assert_eq!(native[0][1]["list"][0]["description"], expected_data);
            assert_eq!(
                native[0][1]["list"][0]["descriptionContentType"],
                expected_type
            );
            let doc = live
                .wbxml_version(
                    "Sync",
                    &devices[1].0,
                    sync_body_preferences(&devices[1].1, &folder, None, true),
                    "16.1",
                )
                .await;
            assert_eq!(
                eas::find_text_after(&doc, air::SERVER_ID),
                Some(id.as_str())
            );
            assert!(eas::contains_token(&doc, base::BODY));
            if !data.is_empty() {
                assert_eq!(eas::find_text_after(&doc, base::DATA), Some(data));
            }
            assert_eq!(
                eas::find_text_after(&doc, cal::SUBJECT),
                Some("Description fixture")
            );
            devices[1].1 = sync_key(&doc);
        }

        let mut invalid = eas::DocumentBuilder::new();
        invalid.start(air::SYNC);
        invalid.start(air::COLLECTIONS);
        invalid.start(air::COLLECTION);
        invalid.leaf(air::SYNC_KEY, &devices[0].1);
        invalid.leaf(air::COLLECTION_ID, &folder);
        invalid.start(air::COMMANDS);
        invalid.start(air::CHANGE);
        invalid.leaf(air::SERVER_ID, &id);
        invalid.start(air::APPLICATION_DATA);
        invalid.leaf(cal::SUBJECT, "Must not apply");
        invalid.start(base::BODY);
        invalid.leaf(base::TYPE, "3");
        invalid.leaf(base::DATA, "unsupported");
        invalid.end();
        for _ in 0..6 {
            invalid.end();
        }
        let doc = live
            .wbxml_version(
                "Sync",
                &devices[0].0,
                wbxml::encode_document(&invalid.finish()),
                "16.1",
            )
            .await;
        assert!(eas::find_all_text_after(&doc, air::STATUS).contains(&"6".to_owned()));
        devices[0].1 = sync_key(&doc);
        let native = live
            .call_using(
                serde_json::json!([["CalendarEvent/get",{
            "accountId":live.account,"ids":[&id],"properties":["id","title","description"]
        },"0"]]),
                &using,
            )
            .await;
        assert_eq!(native[0][1]["list"][0]["title"], "Description fixture");
        assert_eq!(native[0][1]["list"][0]["description"], "");
        for value in [serde_json::json!("Native change"), serde_json::Value::Null] {
            let result = live
                .call_using(
                    serde_json::json!([["CalendarEvent/set",{
                "accountId":live.account,"update":{&id:{"description":value}}
            },"0"]]),
                    &using,
                )
                .await;
            assert!(result[0][1]["updated"].get(&id).is_some());
            let doc = live
                .wbxml_version(
                    "Sync",
                    &devices[1].0,
                    sync_body_preferences(&devices[1].1, &folder, None, true),
                    "16.1",
                )
                .await;
            assert_eq!(
                eas::find_text_after(&doc, air::SERVER_ID),
                Some(id.as_str())
            );
            assert!(eas::contains_token(&doc, base::BODY));
            let body = eas::find_text_after(&doc, base::DATA).unwrap();
            if value.is_null() {
                assert!(!body.contains("Native change"));
            } else {
                assert!(body.contains("Native change"));
            }
            devices[1].1 = sync_key(&doc);
        }
        let result = live
            .call_using(
                serde_json::json!([["CalendarEvent/set",{
            "accountId":live.account,"destroy":[id]
        },"0"]]),
                &using,
            )
            .await;
        assert!(result[0][1]["notDestroyed"].is_null());
    }

    #[tokio::test]
    #[ignore]
    async fn e2e_calendar_partial_times_preserve_timezone_and_other_endpoint() {
        use eas::calendar as cal;
        let live = Live::new("E2E_USER2").await;
        let using = [
            "urn:ietf:params:jmap:core",
            "urn:ietf:params:jmap:calendars",
        ];
        let result = live
            .call_using(
                serde_json::json!([["Calendar/get", {
            "accountId": live.account
        }, "0"]]),
                &using,
            )
            .await;
        let calendar = result[0][1]["list"][0]["id"].as_str().unwrap().to_owned();
        let folder = format!("cal_{calendar}");
        let result = live
            .call_using(
                serde_json::json!([["CalendarEvent/set", {
            "accountId":live.account, "create":{"e":{
                "@type":"Event","calendarIds":{&calendar:true},"title":"Partial times",
                "start":"2026-03-08T01:30:00","timeZone":"America/Toronto","duration":"PT2H"
            }}
        },"0"]]),
                &using,
            )
            .await;
        let id = result[0][1]["created"]["e"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let device = dev("calendar-times");
        let prime = live
            .wbxml_version("Sync", &device, sync_body("0", &folder), "16.1")
            .await;
        let synced = live
            .wbxml_version(
                "Sync",
                &device,
                sync_body(&sync_key(&prime), &folder),
                "16.1",
            )
            .await;
        let mut key = sync_key(&synced);
        for (field, value, expected_start, expected_end) in [
            (
                cal::END_TIME,
                "20260308T093000Z",
                "20260308T063000Z",
                "20260308T093000Z",
            ),
            (
                cal::START_TIME,
                "20260308T060000Z",
                "20260308T060000Z",
                "20260308T093000Z",
            ),
        ] {
            let mut b = eas::DocumentBuilder::new();
            b.start(air::SYNC);
            b.start(air::COLLECTIONS);
            b.start(air::COLLECTION);
            b.leaf(air::SYNC_KEY, &key);
            b.leaf(air::COLLECTION_ID, &folder);
            b.start(air::COMMANDS);
            b.start(air::CHANGE);
            b.leaf(air::SERVER_ID, &id);
            b.start(air::APPLICATION_DATA);
            b.leaf(field, value);
            for _ in 0..6 {
                b.end();
            }
            let doc = live
                .wbxml_version("Sync", &device, wbxml::encode_document(&b.finish()), "16.1")
                .await;
            assert!(eas::find_all_text_after(&doc, air::STATUS)
                .iter()
                .all(|status| status == "1"));
            key = sync_key(&doc);
            let inspect = live
                .wbxml_version(
                    "Sync",
                    &dev("calendar-inspect"),
                    sync_body("0", &folder),
                    "16.1",
                )
                .await;
            let inspect = live
                .wbxml_version(
                    "Sync",
                    &dev("calendar-inspect"),
                    sync_body(&sync_key(&inspect), &folder),
                    "16.1",
                )
                .await;
            let collection = eas::child_elements(&inspect)
                .into_iter()
                .flat_map(|node| eas::child_elements(&node))
                .find(|node| {
                    matches!(node.nodes.first(), Some(wbxml::Node::Start(t))
                    if t.code_page == air::PAGE && t.token == air::COLLECTION.token)
                })
                .unwrap();
            let commands = eas::child_elements(&collection)
                .into_iter()
                .find(|node| {
                    matches!(node.nodes.first(), Some(wbxml::Node::Start(t))
                    if t.code_page == air::PAGE && t.token == air::COMMANDS.token)
                })
                .unwrap();
            let inspect = eas::child_elements(&commands)
                .into_iter()
                .find(|node| eas::find_text_after(node, air::SERVER_ID) == Some(id.as_str()))
                .unwrap();
            assert_eq!(
                eas::find_text_after(&inspect, cal::START_TIME),
                Some(expected_start)
            );
            assert_eq!(
                eas::find_text_after(&inspect, cal::END_TIME),
                Some(expected_end)
            );
            let result = live
                .call_using(
                    serde_json::json!([["CalendarEvent/get",{
                "accountId":live.account,"ids":[&id],"properties":["id","timeZone"]
            },"0"]]),
                    &using,
                )
                .await;
            assert_eq!(result[0][1]["list"][0]["timeZone"], "America/Toronto");
        }
        let result = live
            .call_using(
                serde_json::json!([["CalendarEvent/set",{
            "accountId":live.account,"destroy":[id]
        },"0"]]),
                &using,
            )
            .await;
        assert!(result[0][1]["notDestroyed"].is_null());
    }

    #[tokio::test]
    #[ignore]
    async fn e2e_calendar_16_1_location_edits_preserve_jmap_data_and_clear() {
        use eas::{airsync_base as base, calendar as cal};
        let live = Live::new("E2E_USER2").await;
        let using = [
            "urn:ietf:params:jmap:core",
            "urn:ietf:params:jmap:calendars",
        ];
        let result = live
            .call_using(
                serde_json::json!([["Calendar/get", {"accountId": live.account}, "0"]]),
                &using,
            )
            .await;
        let calendar = result[0][1]["list"][0]["id"].as_str().unwrap().to_owned();
        let folder = format!("cal_{calendar}");
        let result = live
            .call_using(
                serde_json::json!([["CalendarEvent/set", {
            "accountId": live.account, "create": {"e": {
                "@type": "Event", "calendarIds": {&calendar: true}, "title": "Location test",
                "start": "2026-11-15T10:00:00", "duration": "PT1H",
                "locations": {"room/~1": {"@type": "Location", "name": "Room A",
                    "description": "Keep this door instruction", "coordinates": "geo:43.65,-79.38"}}
            }}
        }, "0"]]),
                &using,
            )
            .await;
        let event = result[0][1]["created"]["e"]["id"]
            .as_str()
            .unwrap_or_else(|| panic!("CalendarEvent create failed: {result}"))
            .to_owned();
        let mut devices = Vec::new();
        for prefix in ["cal16a", "cal16b"] {
            let device = dev(prefix);
            let doc = live
                .wbxml_version("Sync", &device, sync_body("0", &folder), "16.1")
                .await;
            let key = eas::find_text_after(&doc, air::SYNC_KEY).unwrap();
            let doc = live
                .wbxml_version("Sync", &device, sync_body(key, &folder), "16.1")
                .await;
            assert_eq!(
                eas::find_text_after(&doc, base::DISPLAY_NAME),
                Some("Room A")
            );
            assert!(!eas::contains_token(&doc, cal::LOCATION));
            devices.push((
                device,
                eas::find_text_after(&doc, air::SYNC_KEY)
                    .unwrap()
                    .to_owned(),
            ));
        }
        let change = |key: &str, name: Option<&str>, subject: Option<&str>, unsupported: bool| {
            let mut b = eas::DocumentBuilder::new();
            b.start(air::SYNC);
            b.start(air::COLLECTIONS);
            b.start(air::COLLECTION);
            b.leaf(air::SYNC_KEY, key);
            b.leaf(air::COLLECTION_ID, &folder);
            b.start(air::COMMANDS);
            b.start(air::CHANGE);
            b.leaf(air::SERVER_ID, &event);
            b.start(air::APPLICATION_DATA);
            if let Some(name) = name {
                if name.is_empty() {
                    b.empty_tag(base::LOCATION);
                } else {
                    b.start(base::LOCATION);
                    b.leaf(base::DISPLAY_NAME, name);
                    if unsupported {
                        b.leaf(base::tag(0x22, false), "Example Street");
                    }
                    b.end();
                }
            }
            if let Some(subject) = subject {
                if subject.is_empty() {
                    b.empty_tag(cal::SUBJECT);
                } else {
                    b.leaf(cal::SUBJECT, subject);
                }
            }
            for _ in 0..6 {
                b.end();
            }
            wbxml::encode_document(&b.finish())
        };
        let a = &mut devices[0];
        let body = change(&a.1, Some("Room B"), None, false);
        let doc = live.wbxml_version("Sync", &a.0, body.clone(), "16.1").await;
        assert!(eas::find_all_text_after(&doc, air::STATUS)
            .iter()
            .all(|status| status == "1"));
        assert_eq!(live.wbxml_version("Sync", &a.0, body, "16.1").await, doc);
        a.1 = eas::find_text_after(&doc, air::SYNC_KEY)
            .unwrap()
            .to_owned();
        let result = live
            .call_using(
                serde_json::json!([["CalendarEvent/get", {
            "accountId": live.account, "ids": [&event]
        }, "0"]]),
                &using,
            )
            .await;
        let location = &result[0][1]["list"][0]["locations"]["room/~1"];
        assert_eq!(location["name"], "Room B");
        assert_eq!(location["description"], "Keep this door instruction");
        assert_eq!(location["coordinates"], "geo:43.65,-79.38");

        let b = &mut devices[1];
        let doc = live
            .wbxml_version("Sync", &b.0, sync_body(&b.1, &folder), "16.1")
            .await;
        assert_eq!(
            eas::find_text_after(&doc, base::DISPLAY_NAME),
            Some("Room B")
        );
        b.1 = eas::find_text_after(&doc, air::SYNC_KEY)
            .unwrap()
            .to_owned();

        let a = &mut devices[0];
        let doc = live
            .wbxml_version(
                "Sync",
                &a.0,
                change(&a.1, None, Some("Changed title"), false),
                "16.1",
            )
            .await;
        a.1 = eas::find_text_after(&doc, air::SYNC_KEY)
            .unwrap()
            .to_owned();
        let result = live
            .call_using(
                serde_json::json!([["CalendarEvent/get", {
            "accountId": live.account, "ids": [&event]
        }, "0"]]),
                &using,
            )
            .await;
        assert_eq!(
            result[0][1]["list"][0]["locations"]["room/~1"]["name"],
            "Room B"
        );

        let doc = live
            .wbxml_version(
                "Sync",
                &a.0,
                change(&a.1, Some("Bad replacement"), Some("Must not apply"), true),
                "16.1",
            )
            .await;
        assert!(eas::find_all_text_after(&doc, air::STATUS).contains(&"6".to_owned()));
        a.1 = eas::find_text_after(&doc, air::SYNC_KEY)
            .unwrap()
            .to_owned();
        let result = live
            .call_using(
                serde_json::json!([["CalendarEvent/get", {
            "accountId": live.account, "ids": [&event]
        }, "0"]]),
                &using,
            )
            .await;
        assert_eq!(result[0][1]["list"][0]["title"], "Changed title");
        assert_eq!(
            result[0][1]["list"][0]["locations"]["room/~1"]["name"],
            "Room B"
        );

        let doc = live
            .wbxml_version(
                "Sync",
                &a.0,
                change(&a.1, Some(""), Some("Cleared location"), false),
                "16.1",
            )
            .await;
        assert!(eas::find_all_text_after(&doc, air::STATUS)
            .iter()
            .all(|status| status == "1"));
        let result = live
            .call_using(
                serde_json::json!([["CalendarEvent/get", {
            "accountId": live.account, "ids": [&event]
        }, "0"]]),
                &using,
            )
            .await;
        assert!(result[0][1]["list"][0]["locations"]
            .as_object()
            .is_none_or(|locations| locations.is_empty()));
        assert_eq!(result[0][1]["list"][0]["title"], "Cleared location");

        a.1 = eas::find_text_after(&doc, air::SYNC_KEY)
            .unwrap()
            .to_owned();
        let doc = live
            .wbxml_version("Sync", &a.0, change(&a.1, None, Some(""), false), "16.1")
            .await;
        assert!(eas::find_all_text_after(&doc, air::STATUS)
            .iter()
            .all(|status| status == "1"));
        let result = live
            .call_using(
                serde_json::json!([["CalendarEvent/get", {
            "accountId": live.account, "ids": [&event]
        }, "0"]]),
                &using,
            )
            .await;
        assert_eq!(result[0][1]["list"][0]["title"], "");

        let b = &devices[1];
        let doc = live
            .wbxml_version("Sync", &b.0, sync_body(&b.1, &folder), "16.1")
            .await;
        assert!(doc.nodes.iter().any(|node| matches!(node, wbxml::Node::Start(token)
            if token.code_page == base::PAGE && token.token == base::LOCATION.token && !token.has_content)));
        let result = live
            .call_using(
                serde_json::json!([["CalendarEvent/set", {
            "accountId": live.account, "destroy": [&event]
        }, "0"]]),
                &using,
            )
            .await;
        assert!(result[0][1]["notDestroyed"].is_null());
    }

    #[tokio::test]
    #[ignore]
    async fn e2e_sync_options_preserve_body_format_across_restart_and_replace() {
        use eas::airsync_base as base;
        let live = Live::new("E2E_USER2").await;
        let result = live.call(serde_json::json!([["Mailbox/set", {
            "accountId": live.account, "create": {"m": {"name": format!("Options {}", std::process::id())}}
        }, "0"]])).await;
        let folder = result[0][1]["created"]["m"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let html = "<p>Hello from the native backend</p>";
        let create = |client: &str| {
            serde_json::json!([["Email/set", {
            "accountId": live.account, "create": {client: {
                "mailboxIds": {&folder: true}, "keywords": {},
                "from": [{"email":"e2e@example.test"}], "to":[{"email":live.user}],
                "subject": client, "bodyValues":{"b":{"value":html}},
                "htmlBody":[{"partId":"b","type":"text/html"}]
            }}
        }, "0"]])
        };
        let result = live.call(create("first")).await;
        let mut ids = vec![result[0][1]["created"]["first"]["id"]
            .as_str()
            .unwrap()
            .to_owned()];
        let device = dev("opts");
        let options = mail_preferences(Some("5"), Some(("1", "5")));
        let doc = live
            .wbxml_version(
                "Sync",
                &device,
                sync_body_preferences("0", &folder, Some(&options), true),
                "16.1",
            )
            .await;
        let doc = live
            .wbxml_version(
                "Sync",
                &device,
                sync_body_preferences(&sync_key(&doc), &folder, None, true),
                "16.1",
            )
            .await;
        assert_eq!(eas::find_text_after(&doc, base::DATA), Some("Hello"));
        assert_eq!(eas::find_text_after(&doc, base::TYPE), Some("1"));
        let key = sync_key(&doc);

        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(env("E2E_RESTART_CMD"))
            .output()
            .unwrap();
        assert!(out.status.success(), "gateway restart failed");
        let live = Live::new("E2E_USER2").await;
        let result = live.call(create("second")).await;
        ids.push(
            result[0][1]["created"]["second"]["id"]
                .as_str()
                .unwrap()
                .to_owned(),
        );
        let doc = live
            .wbxml_version(
                "Sync",
                &device,
                sync_body_preferences(&key, &folder, None, true),
                "16.1",
            )
            .await;
        assert_eq!(eas::find_text_after(&doc, base::DATA), Some("Hello"));
        let key = sync_key(&doc);
        let replacement = mail_preferences(Some("0"), None);
        let doc = live
            .wbxml_version(
                "Sync",
                &device,
                sync_body_preferences(&key, &folder, Some(&replacement), false),
                "16.1",
            )
            .await;
        let result = live.call(create("third")).await;
        ids.push(
            result[0][1]["created"]["third"]["id"]
                .as_str()
                .unwrap()
                .to_owned(),
        );
        let doc = live
            .wbxml_version(
                "Sync",
                &device,
                sync_body_preferences(&sync_key(&doc), &folder, None, true),
                "16.1",
            )
            .await;
        assert_eq!(eas::find_text_after(&doc, base::TYPE), Some("2"));
        assert_eq!(eas::find_text_after(&doc, base::DATA), Some(html));
        let result = live
            .call(serde_json::json!([["Email/set", {
            "accountId": live.account, "destroy": ids
        }, "0"], ["Mailbox/set", {"accountId":live.account,"destroy":[&folder]}, "1"]]))
            .await;
        assert!(result[0][1]["notDestroyed"].is_null());
        assert!(result[1][1]["notDestroyed"].is_null());
    }

    #[tokio::test]
    #[ignore]
    async fn e2e_partial_and_empty_sync_keep_collection_cache_after_restart() {
        let live = Live::new("E2E_USER2").await;
        let mut folders = Vec::new();
        for name in ["partial-a", "partial-b"] {
            let result = live.call(serde_json::json!([["Mailbox/set", {
                "accountId": live.account, "create": {"m":{"name":format!("{name} {}", std::process::id())}}
            }, "0"]])).await;
            folders.push(
                result[0][1]["created"]["m"]["id"]
                    .as_str()
                    .unwrap()
                    .to_owned(),
            );
        }
        let device = dev("partial16");
        let doc = live
            .wbxml_version(
                "Sync",
                &device,
                partial_sync(&[(&folders[0], "0"), (&folders[1], "0")], false),
                "16.1",
            )
            .await;
        let keys = eas::find_all_text_after(&doc, air::SYNC_KEY);
        let doc = live
            .wbxml_version(
                "Sync",
                &device,
                partial_sync(&[(&folders[0], &keys[0]), (&folders[1], &keys[1])], false),
                "16.1",
            )
            .await;
        let keys = eas::find_all_text_after(&doc, air::SYNC_KEY);
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(env("E2E_RESTART_CMD"))
            .output()
            .unwrap();
        assert!(out.status.success(), "gateway restart failed");
        let live = Live::new("E2E_USER2").await;
        let mut ids = live.seed(&folders[1], 1, "partial", false).await;
        let doc = live
            .wbxml_version(
                "Sync",
                &device,
                partial_sync(&[(&folders[0], &keys[0])], true),
                "16.1",
            )
            .await;
        assert_eq!(eas::find_all_text_after(&doc, air::COLLECTION_ID), folders);
        assert_eq!(eas::find_all_text_after(&doc, air::SERVER_ID), ids);
        let keys = eas::find_all_text_after(&doc, air::SYNC_KEY);
        live.wbxml_version(
            "Sync",
            &device,
            partial_sync(&[(&folders[0], &keys[0]), (&folders[1], &keys[1])], false),
            "16.1",
        )
        .await;
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(env("E2E_RESTART_CMD"))
            .output()
            .unwrap();
        assert!(out.status.success(), "gateway restart failed");
        let live = Live::new("E2E_USER2").await;
        let new_ids = live.seed(&folders[0], 1, "empty", false).await;
        let doc = live
            .wbxml_version("Sync", &device, Vec::new(), "16.1")
            .await;
        assert_eq!(eas::find_all_text_after(&doc, air::SERVER_ID), new_ids);
        ids.extend(new_ids);
        let result = live
            .call(serde_json::json!([["Email/set",{
            "accountId":live.account,"destroy":ids
        }, "0"],["Mailbox/set",{"accountId":live.account,"destroy":folders},"1"]]))
            .await;
        assert!(result[0][1]["notDestroyed"].is_null());
        assert!(result[1][1]["notDestroyed"].is_null());
    }

    #[tokio::test]
    #[ignore]
    async fn e2e_draft_create_replays_and_syncs_to_another_device() {
        use eas::{airsync_base as base, email, email2};
        let mut live = Live::new("E2E_USER2").await;
        let boxes = live
            .call(serde_json::json!([["Mailbox/get",{"accountId":live.account},"0"]]))
            .await;
        let folder = boxes[0][1]["list"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["role"] == "drafts")
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let device = dev("draft-create");
        let peer = dev("draft-create-peer");
        let prime = live
            .wbxml_version("Sync", &device, sync_body("0", &folder), "16.1")
            .await;
        let sync = live
            .wbxml_version(
                "Sync",
                &device,
                sync_body(&sync_key(&prime), &folder),
                "16.1",
            )
            .await;
        let peer_prime = live
            .wbxml_version("Sync", &peer, sync_body("0", &folder), "16.1")
            .await;
        let peer_sync = live
            .wbxml_version(
                "Sync",
                &peer,
                sync_body(&sync_key(&peer_prime), &folder),
                "16.1",
            )
            .await;
        let request = draft_add_body(&sync_key(&sync), &folder, "draft-client-id", false);
        let response = live
            .wbxml_version("Sync", &device, request.clone(), "16.1")
            .await;
        assert_eq!(
            eas::find_all_text_after(&response, air::STATUS),
            vec!["1", "1"]
        );
        assert_eq!(
            eas::find_text_after(&response, air::CLIENT_ID),
            Some("draft-client-id")
        );
        let id = eas::find_text_after(&response, air::SERVER_ID)
            .unwrap()
            .to_owned();
        let mut altered = wbxml::decode_document(&request).unwrap();
        for node in &mut altered.nodes {
            if let wbxml::Node::Text(value) = node {
                if value == "Draft creation fixture" {
                    *value = "Different content".to_owned();
                }
            }
        }
        let changed = live
            .wbxml_version("Sync", &device, wbxml::encode_document(&altered), "16.1")
            .await;
        assert_eq!(eas::find_text_after(&changed, air::STATUS), Some("3"));
        assert_eq!(
            live.wbxml_version("Sync", &device, request.clone(), "16.1")
                .await,
            response
        );
        let restart = std::process::Command::new("sh")
            .arg("-c")
            .arg(env("E2E_RESTART_CMD"))
            .output()
            .unwrap();
        assert!(restart.status.success());
        live = Live::new("E2E_USER2").await;
        assert_eq!(
            live.wbxml_version("Sync", &device, request, "16.1").await,
            response
        );
        let native = live.call(serde_json::json!([["Email/get",{
            "accountId":live.account,"ids":[&id],"properties":["id","subject","keywords","to","bcc","replyTo","header:Importance:asText","bodyValues","textBody"],
            "fetchAllBodyValues":true
        },"0"]])).await;
        let item = &native[0][1]["list"][0];
        assert_eq!(item["subject"], "Draft creation fixture");
        assert_eq!(item["keywords"]["$draft"], true);
        assert_eq!(item["keywords"]["$seen"], true);
        assert_eq!(item["to"][0]["email"], "recipient@example.test");
        assert_eq!(item["bcc"][0]["email"], "hidden@example.test");
        assert_eq!(item["replyTo"][0]["email"], "reply@example.test");
        assert_eq!(item["header:Importance:asText"], "high");
        let peer_update = live
            .wbxml_version(
                "Sync",
                &peer,
                sync_body(&sync_key(&peer_sync), &folder),
                "16.1",
            )
            .await;
        assert_eq!(
            eas::find_text_after(&peer_update, air::SERVER_ID),
            Some(id.as_str())
        );
        assert_eq!(
            eas::find_text_after(&peer_update, email::SUBJECT),
            Some("Draft creation fixture")
        );
        assert!(eas::find_text_after(&peer_update, email2::BCC)
            .is_some_and(|s| s.contains("hidden@example.test")));
        assert_eq!(
            eas::find_text_after(&peer_update, base::DATA),
            Some("Draft contents")
        );
        assert_eq!(
            eas::find_text_after(&peer_update, email::IMPORTANCE),
            Some("2")
        );
        assert!(eas::find_text_after(&peer_update, email::REPLY_TO)
            .is_some_and(|value| value.contains("reply@example.test")));
        let mut second = wbxml::decode_document(&draft_add_body(
            &sync_key(&response),
            &folder,
            "draft-second",
            true,
        ))
        .unwrap();
        for node in &mut second.nodes {
            if let wbxml::Node::Text(value) = node {
                if value.contains("recipient@example.test") {
                    *value = "partially-entered-recipient".to_owned();
                }
            }
        }
        let normal = live
            .wbxml_version("Sync", &device, wbxml::encode_document(&second), "16.1")
            .await;
        assert_eq!(
            eas::find_all_text_after(&normal, air::STATUS),
            vec!["1", "1"]
        );
        let new_id = eas::find_text_after(&normal, air::SERVER_ID)
            .unwrap()
            .to_owned();
        assert_ne!(new_id, id);
        assert_eq!(
            eas::find_all_text_after(&normal, air::SERVER_ID)
                .iter()
                .filter(|server_id| *server_id == &new_id)
                .count(),
            1
        );
        let peer_second = live
            .wbxml_version(
                "Sync",
                &peer,
                sync_body(&sync_key(&peer_update), &folder),
                "16.1",
            )
            .await;
        assert_eq!(
            eas::find_text_after(&peer_second, air::SERVER_ID),
            Some(new_id.as_str())
        );
        assert_eq!(
            eas::find_text_after(&peer_second, email::TO),
            Some("partially-entered-recipient")
        );
        let cleanup = live
            .call(serde_json::json!([["Email/set",{"accountId":live.account,"destroy":[id,new_id]},"0"]]))
            .await;
        assert!(cleanup[0][1]["notDestroyed"].is_null());
    }

    #[tokio::test]
    #[ignore]
    async fn e2e_mail_mixed_changes_respect_window() {
        let live = Live::new("E2E_USER2").await;
        let created = live.call(serde_json::json!([["Mailbox/set",{
            "accountId":live.account, "create":{"m":{"name":format!("Window test {}",std::process::id())}}
        },"0"]])).await;
        let folder = created[0][1]["created"]["m"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let ids = live.seed(&folder, 4, "window-fixture", false).await;
        let device = dev("mixed-window");
        let prime = live
            .wbxml_version("Sync", &device, sync_body_window("0", &folder, "2"), "16.1")
            .await;
        let mut key = sync_key(&prime);
        let mut received = Vec::new();
        loop {
            let response = live
                .wbxml_version(
                    "Sync",
                    &device,
                    sync_body_window(&key, &folder, "2"),
                    "16.1",
                )
                .await;
            received.extend(eas::find_all_text_after(&response, air::SERVER_ID));
            key = sync_key(&response);
            if !eas::contains_token(&response, air::MORE_AVAILABLE) {
                break;
            }
        }
        assert_eq!(received.len(), 4);
        let changed = live
            .call(serde_json::json!([["Email/set",{
            "accountId":live.account,
            "destroy":[&ids[0], &ids[1]],
            "update":{&ids[2]:{"keywords/$seen":true}, &ids[3]:{"keywords/$seen":true}}
        },"0"]]))
            .await;
        assert!(changed[0][1]["notDestroyed"].is_null());
        assert!(changed[0][1]["notUpdated"].is_null());
        let added = live.seed(&folder, 1, "window-new", false).await.remove(0);
        let mut commands = Vec::new();
        for _ in 0..10 {
            let response = live
                .wbxml_version(
                    "Sync",
                    &device,
                    sync_body_window(&key, &folder, "2"),
                    "16.1",
                )
                .await;
            let values = eas::find_all_text_after(&response, air::SERVER_ID);
            assert!(values.len() <= 2, "more commands than the requested window");
            commands.extend(values);
            key = sync_key(&response);
            if !eas::contains_token(&response, air::MORE_AVAILABLE) {
                break;
            }
        }
        commands.sort();
        let mut expected = ids.clone();
        expected.push(added.clone());
        expected.sort();
        assert_eq!(commands, expected);
        let cleanup = live
            .call(serde_json::json!([
                ["Email/set",{"accountId":live.account,"destroy":[&ids[2], &ids[3], added]},"0"],
                ["Mailbox/set",{"accountId":live.account,"destroy":[folder]},"1"]
            ]))
            .await;
        assert!(cleanup[0][1]["notDestroyed"].is_null());
        assert!(cleanup[1][1]["notDestroyed"].is_null());
    }

    #[tokio::test]
    #[ignore]
    async fn e2e_global_window_across_mailboxes() {
        let live = Live::new("E2E_USER2").await;
        let result = live
            .call(serde_json::json!([["Mailbox/set", {
            "accountId": live.account, "create": {
                "a": {"name": format!("Global window A {}", std::process::id())},
                "b": {"name": format!("Global window B {}", std::process::id())}
            }
        }, "0"]]))
            .await;
        let a = result[0][1]["created"]["a"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let b = result[0][1]["created"]["b"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let mut ids = live.seed(&a, 2, "global-a", false).await;
        ids.extend(live.seed(&b, 2, "global-b", false).await);
        let device = dev("global-window");
        let prime_a = live
            .wbxml_version("Sync", &device, sync_body("0", &a), "16.1")
            .await;
        let prime_b = live
            .wbxml_version("Sync", &device, sync_body("0", &b), "16.1")
            .await;
        let mut request = eas::DocumentBuilder::new();
        request.start(air::SYNC);
        request.leaf(air::WINDOW_SIZE, "3");
        request.start(air::COLLECTIONS);
        for (folder, key) in [(&a, sync_key(&prime_a)), (&b, sync_key(&prime_b))] {
            request.start(air::COLLECTION);
            request.leaf(air::SYNC_KEY, key);
            request.leaf(air::COLLECTION_ID, folder);
            request.leaf(air::WINDOW_SIZE, "10");
            request.end();
        }
        request.end();
        request.end();
        let response = live
            .wbxml_version(
                "Sync",
                &device,
                wbxml::encode_document(&request.finish()),
                "16.1",
            )
            .await;
        let folders = eas::child_elements(&response).into_iter().find(|child|
            matches!(child.nodes.first(), Some(wbxml::Node::Start(t)) if t.code_page == air::PAGE && t.token == air::COLLECTIONS.token)
        ).unwrap();
        let replies = eas::child_elements(&folders);
        assert_eq!(replies.len(), 2);
        assert_eq!(
            eas::find_all_text_after(&replies[0], air::SERVER_ID).len(),
            2
        );
        assert_eq!(
            eas::find_all_text_after(&replies[1], air::SERVER_ID).len(),
            1
        );
        assert!(eas::contains_token(&replies[1], air::MORE_AVAILABLE));
        let next = live
            .wbxml_version(
                "Sync",
                &device,
                sync_body(&sync_key(&replies[1]), &b),
                "16.1",
            )
            .await;
        assert_eq!(eas::find_all_text_after(&next, air::SERVER_ID).len(), 1);
        let cleanup = live
            .call(serde_json::json!([
                ["Email/set", {"accountId":live.account, "destroy":ids}, "0"],
                ["Mailbox/set", {"accountId":live.account, "destroy":[a,b]}, "1"]
            ]))
            .await;
        assert!(cleanup[0][1]["notDestroyed"].is_null());
        assert!(cleanup[1][1]["notDestroyed"].is_null());
    }

    #[tokio::test]
    #[ignore]
    async fn e2e_hidden_keyword_metadata_is_not_acknowledged() {
        use eas::{email, tasks};
        let live = Live::new("E2E_USER2").await;
        let created = live
            .call(serde_json::json!([["Mailbox/set",{"accountId":live.account,
            "create":{"m":{"name":format!("Flag capacity {}",std::process::id())}}},"0"]]))
            .await;
        let folder = created[0][1]["created"]["m"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let ids = live.seed(&folder, 2, "flag-capacity", false).await;
        let mut patch = serde_json::Map::new();
        for n in 0..110 {
            patch.insert(
                format!("keywords/eas-pressure-{}-{n}", std::process::id()),
                serde_json::json!(true),
            );
        }
        let updated = live
            .call(serde_json::json!([["Email/set",{"accountId":live.account,
            "update":{&ids[0]:patch}},"0"]]))
            .await;
        assert!(updated[0][1]["updated"].get(&ids[0]).is_some());
        let device = dev("flag-capacity");
        let prime = live
            .wbxml_version("Sync", &device, sync_body("0", &folder), "16.1")
            .await;
        let sync = live
            .wbxml_version(
                "Sync",
                &device,
                sync_body(&sync_key(&prime), &folder),
                "16.1",
            )
            .await;
        let key = sync_key(&sync);
        let mut b = eas::DocumentBuilder::new();
        b.start(air::SYNC);
        b.start(air::COLLECTIONS);
        b.start(air::COLLECTION);
        b.leaf(air::SYNC_KEY, &key);
        b.leaf(air::COLLECTION_ID, &folder);
        b.leaf(air::GET_CHANGES, "0");
        b.start(air::COMMANDS);
        b.start(air::CHANGE);
        b.leaf(air::SERVER_ID, &ids[1]);
        b.start(air::APPLICATION_DATA);
        b.start(email::FLAG);
        b.leaf(email::FLAG_STATUS, "2");
        b.leaf(email::FLAG_TYPE, "Flag for follow up");
        b.leaf(
            tasks::SUBJECT,
            format!("unique fixture {}", std::process::id()),
        );
        for token in [
            tasks::START_DATE,
            tasks::UTC_START_DATE,
            tasks::DUE_DATE,
            tasks::UTC_DUE_DATE,
        ] {
            b.leaf(token, "2026-11-14T12:00:00.000Z");
        }
        b.end();
        b.end();
        b.end();
        b.end();
        b.end();
        b.end();
        b.end();
        let result = live
            .wbxml_version("Sync", &device, wbxml::encode_document(&b.finish()), "16.1")
            .await;
        assert_eq!(eas::find_text_after(&result, air::STATUS), Some("5"));
        assert_eq!(
            eas::find_text_after(&result, air::SYNC_KEY),
            Some(key.as_str())
        );
        let cleanup = live
            .call(serde_json::json!([
                ["Email/set",{"accountId":live.account,"destroy":ids},"0"],
                ["Mailbox/set",{"accountId":live.account,"destroy":[folder]},"1"]
            ]))
            .await;
        assert!(cleanup[0][1]["notDestroyed"].is_null());
        assert!(cleanup[1][1]["notDestroyed"].is_null());
    }

    #[tokio::test]
    #[ignore]
    async fn e2e_draft_retry_does_not_duplicate_when_keywords_are_hidden() {
        let live = Live::new("E2E_USER2").await;
        let boxes = live
            .call(serde_json::json!([["Mailbox/get",{"accountId":live.account},"0"]]))
            .await;
        let folder = boxes[0][1]["list"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["role"] == "drafts")
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let pressure = live.seed(&folder, 1, "draft-keyword-pressure", false).await;
        let mut patch = serde_json::Map::new();
        for n in 0..110 {
            patch.insert(
                format!("keywords/draft-pressure-{}-{n}", std::process::id()),
                serde_json::json!(true),
            );
        }
        let updated = live
            .call(serde_json::json!([["Email/set",{"accountId":live.account,
            "update":{&pressure[0]:patch}},"0"]]))
            .await;
        assert!(updated[0][1]["updated"].get(&pressure[0]).is_some());
        let query = || {
            serde_json::json!([["Email/query",{"accountId":live.account,
            "filter":{"inMailbox":&folder},"limit":1000},"0"]])
        };
        let before = live.call(query()).await;
        let before_ids = before[0][1]["ids"].as_array().unwrap().clone();
        let device = dev("draft-pressure");
        let prime = live
            .wbxml_version("Sync", &device, sync_body("0", &folder), "16.1")
            .await;
        let sync = live
            .wbxml_version(
                "Sync",
                &device,
                sync_body(&sync_key(&prime), &folder),
                "16.1",
            )
            .await;
        let first = live
            .wbxml_version(
                "Sync",
                &device,
                draft_add_body(&sync_key(&sync), &folder, "capacity-client-id", false),
                "16.1",
            )
            .await;
        assert!(eas::find_all_text_after(&first, air::STATUS)
            .iter()
            .any(|status| status == "5"));
        let after = live.call(query()).await;
        let added: Vec<_> = after[0][1]["ids"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|id| !before_ids.contains(id))
            .cloned()
            .collect();
        assert_eq!(added.len(), 1);
        let retry = live
            .wbxml_version(
                "Sync",
                &device,
                draft_add_body(&sync_key(&first), &folder, "capacity-client-id", false),
                "16.1",
            )
            .await;
        assert!(eas::find_all_text_after(&retry, air::STATUS)
            .iter()
            .any(|status| status == "5"));
        let final_state = live.call(query()).await;
        let final_added: Vec<_> = final_state[0][1]["ids"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|id| !before_ids.contains(id))
            .cloned()
            .collect();
        assert_eq!(final_added, added);
        let mut cleanup = pressure;
        cleanup.extend(added.iter().map(|id| id.as_str().unwrap().to_owned()));
        let result = live
            .call(
                serde_json::json!([["Email/set",{"accountId":live.account,"destroy":cleanup},"0"]]),
            )
            .await;
        assert!(result[0][1]["notDestroyed"].is_null());
    }

    #[tokio::test]
    #[ignore]
    async fn e2e_replaced_draft_keeps_eas_identity_and_syncs_content() {
        use eas::{email, item_operations as ops};
        let live = Live::new("E2E_USER2").await;
        let created = live.call(serde_json::json!([["Mailbox/set",{
            "accountId":live.account, "create":{"m":{"name":format!("Draft identity {}",std::process::id())}}
        },"0"]])).await;
        let folder = created[0][1]["created"]["m"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let old = live
            .seed(&folder, 1, "draft-original", false)
            .await
            .remove(0);
        let r = live
            .call(serde_json::json!([["Email/set",{
            "accountId":live.account,"update":{&old:{"keywords/$draft":true}}
        },"0"]]))
            .await;
        assert!(r[0][1]["updated"].get(&old).is_some());
        let device = dev("draft-identity");
        let prime = live
            .wbxml_version("Sync", &device, sync_body("0", &folder), "16.1")
            .await;
        let sync = live
            .wbxml_version(
                "Sync",
                &device,
                sync_body(&sync_key(&prime), &folder),
                "16.1",
            )
            .await;
        assert_eq!(
            eas::find_text_after(&sync, air::SERVER_ID),
            Some(old.as_str())
        );
        let other = dev("draft-identity-other");
        let other_prime = live
            .wbxml_version("Sync", &other, sync_body("0", &folder), "16.1")
            .await;
        let other_sync = live
            .wbxml_version(
                "Sync",
                &other,
                sync_body(&sync_key(&other_prime), &folder),
                "16.1",
            )
            .await;
        assert_eq!(
            eas::find_text_after(&other_sync, air::SERVER_ID),
            Some(old.as_str())
        );
        let keywords = serde_json::json!({"$draft":true});
        let replacement = live
            .call(serde_json::json!([["Email/set",{
            "accountId":live.account,"create":{"e":{"mailboxIds":{&folder:true},"keywords":keywords,
                "subject":"Changed draft subject","bodyValues":{"b":{"value":"Changed draft body"}},
                "textBody":[{"partId":"b","type":"text/plain"}]}}
        },"0"]]))
            .await;
        let new = replacement[0][1]["created"]["e"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_ne!(new, old);
        let result = live
            .call(serde_json::json!([["Email/set",{
            "accountId":live.account,"destroy":[&old]
        },"0"]]))
            .await;
        assert!(result[0][1]["notDestroyed"].is_null());
        let state = crate::state::SqliteStateStore::open(&env("E2E_STATE_SQLITE_PATH")).unwrap();
        assert!(crate::state::StateStore::swap_draft_mapping(
            &state,
            &live.account,
            &old,
            &old,
            &new
        )
        .await
        .unwrap());
        if let Ok(restart) = std::env::var("E2E_RESTART_CMD") {
            let status = std::process::Command::new("sh")
                .arg("-c")
                .arg(restart)
                .status()
                .unwrap();
            assert!(status.success());
        }
        let live = Live::new("E2E_USER2").await;
        let changed = live
            .wbxml_version(
                "Sync",
                &device,
                sync_body(&sync_key(&sync), &folder),
                "16.1",
            )
            .await;
        assert_eq!(
            eas::find_all_text_after(&changed, air::SERVER_ID),
            vec![old.clone()]
        );
        assert_eq!(
            eas::find_text_after(&changed, email::SUBJECT),
            Some("Changed draft subject")
        );
        assert!(eas::contains_token(&changed, air::CHANGE));
        assert!(!eas::contains_token(&changed, air::DELETE));
        let other_changed = live
            .wbxml_version(
                "Sync",
                &other,
                sync_body(&sync_key(&other_sync), &folder),
                "16.1",
            )
            .await;
        assert_eq!(
            eas::find_all_text_after(&other_changed, air::SERVER_ID),
            vec![old.clone()]
        );
        assert!(eas::contains_token(&other_changed, air::CHANGE));
        assert_eq!(
            eas::find_text_after(&other_changed, email::SUBJECT),
            Some("Changed draft subject")
        );
        let mut b = eas::DocumentBuilder::new();
        b.start(ops::ITEM_OPERATIONS);
        b.start(ops::FETCH);
        b.leaf(ops::STORE, "Mailbox");
        b.leaf(air::COLLECTION_ID, &folder);
        b.leaf(air::SERVER_ID, &old);
        b.end();
        b.end();
        let fetched = live
            .wbxml_version(
                "ItemOperations",
                &device,
                wbxml::encode_document(&b.finish()),
                "16.1",
            )
            .await;
        assert_eq!(
            eas::find_text_after(&fetched, email::SUBJECT),
            Some("Changed draft subject")
        );
        let update = live
            .wbxml_version(
                "Sync",
                &device,
                flag_change_body(
                    &sync_key(&changed),
                    &folder,
                    &old,
                    Some("2"),
                    Some(true),
                    false,
                ),
                "16.1",
            )
            .await;
        assert_eq!(eas::find_text_after(&update, air::STATUS), Some("1"));
        let get = live
            .call(serde_json::json!([["Email/get",{
            "accountId":live.account, "ids":[&new], "properties":["id","keywords"]
        },"0"]]))
            .await;
        assert_eq!(get[0][1]["list"][0]["keywords"]["$seen"], true);
        assert_eq!(get[0][1]["list"][0]["keywords"]["$flagged"], true);
        let other_flag = live
            .wbxml_version(
                "Sync",
                &other,
                sync_body(&sync_key(&other_changed), &folder),
                "16.1",
            )
            .await;
        assert!(eas::contains_token(&other_flag, air::CHANGE));
        assert_eq!(
            eas::find_text_after(&other_flag, air::SERVER_ID),
            Some(old.as_str())
        );
        let mut b = eas::DocumentBuilder::new();
        b.start(air::SYNC);
        b.start(air::COLLECTIONS);
        b.start(air::COLLECTION);
        b.leaf(air::SYNC_KEY, sync_key(&update));
        b.leaf(air::COLLECTION_ID, &folder);
        b.leaf(air::GET_CHANGES, "0");
        b.leaf(air::DELETES_AS_MOVES, "0");
        b.start(air::COMMANDS);
        b.start(air::DELETE);
        b.leaf(air::SERVER_ID, &old);
        b.end();
        b.end();
        b.end();
        b.end();
        b.end();
        let deleted = live
            .wbxml_version("Sync", &device, wbxml::encode_document(&b.finish()), "16.1")
            .await;
        assert_eq!(eas::find_text_after(&deleted, air::STATUS), Some("1"));
        let other_delete = live
            .wbxml_version(
                "Sync",
                &other,
                sync_body(&sync_key(&other_flag), &folder),
                "16.1",
            )
            .await;
        assert!(eas::contains_token(&other_delete, air::DELETE));
        assert_eq!(
            eas::find_text_after(&other_delete, air::SERVER_ID),
            Some(old.as_str())
        );
        assert!(crate::state::StateStore::swap_draft_mapping(
            &state,
            &live.account,
            &old,
            &new,
            &old
        )
        .await
        .unwrap());
        let cleanup = live
            .call(serde_json::json!([
                ["Mailbox/set",{"accountId":live.account,"destroy":[folder]},"0"]
            ]))
            .await;
        assert!(cleanup[0][1]["notDestroyed"].is_null());
    }

    #[tokio::test]
    #[ignore]
    async fn e2e_draft_client_id_can_be_reused_after_success() {
        let live = Live::new("E2E_USER2").await;
        let boxes = live
            .call(serde_json::json!([["Mailbox/get",{"accountId":live.account},"0"]]))
            .await;
        let folder = boxes[0][1]["list"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["role"] == "drafts")
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let device = dev("draft-reuse");
        let prime = live
            .wbxml_version("Sync", &device, sync_body("0", &folder), "16.1")
            .await;
        let sync = live
            .wbxml_version(
                "Sync",
                &device,
                sync_body(&sync_key(&prime), &folder),
                "16.1",
            )
            .await;
        let first = live
            .wbxml_version(
                "Sync",
                &device,
                draft_add_body(&sync_key(&sync), &folder, "reused-id", false),
                "16.1",
            )
            .await;
        assert_eq!(
            eas::find_all_text_after(&first, air::STATUS),
            vec!["1", "1"]
        );
        let first_id = eas::find_text_after(&first, air::SERVER_ID)
            .unwrap()
            .to_owned();
        let second = live
            .wbxml_version(
                "Sync",
                &device,
                draft_add_body(&sync_key(&first), &folder, "reused-id", false),
                "16.1",
            )
            .await;
        assert_eq!(
            eas::find_all_text_after(&second, air::STATUS),
            vec!["1", "1"]
        );
        let second_id = eas::find_text_after(&second, air::SERVER_ID)
            .unwrap()
            .to_owned();
        assert_ne!(first_id, second_id);
        let cleanup = live
            .call(serde_json::json!([["Email/set",{"accountId":live.account,
            "destroy":[first_id,second_id]},"0"]]))
            .await;
        assert!(cleanup[0][1]["notDestroyed"].is_null());
    }

    #[tokio::test]
    #[ignore]
    async fn e2e_draft_add_recovers_after_server_write_before_sync_checkpoint() {
        let live = Live::new("E2E_USER2").await;
        let boxes = live
            .call(serde_json::json!([["Mailbox/get",{"accountId":live.account},"0"]]))
            .await;
        let folder = boxes[0][1]["list"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["role"] == "drafts")
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let device = dev("draft-recovery");
        let prime = live
            .wbxml_version("Sync", &device, sync_body("0", &folder), "16.1")
            .await;
        let sync = live
            .wbxml_version(
                "Sync",
                &device,
                sync_body(&sync_key(&prime), &folder),
                "16.1",
            )
            .await;
        let key = sync_key(&sync);
        let request = draft_add_body(&key, &folder, "recovery-id", false);
        let doc = wbxml::decode_document(&request).unwrap();
        let collection = eas::sync_collections(&doc).remove(0);
        let identity = serde_json::to_vec(&(
            device.as_str(),
            &folder,
            &key,
            &collection.commands[0].client_id,
        ))
        .unwrap();
        let marker = format!(
            "eas-draft-add-{}",
            uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, &identity).simple()
        );
        let content = format!(
            "eas-draft-data-{}",
            uuid::Uuid::new_v5(
                &uuid::Uuid::NAMESPACE_OID,
                &wbxml::encode_document(&collection.commands[0].document)
            )
            .simple()
        );
        let created = live
            .call(serde_json::json!([["Email/set",{
            "accountId":live.account,"create":{"e":{"mailboxIds":{&folder:true},
            "keywords":{"$draft":true,&marker:true,&content:true},"subject":"recovery fixture"}}
        },"0"]]))
            .await;
        let id = created[0][1]["created"]["e"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let response = live.wbxml_version("Sync", &device, request, "16.1").await;
        assert_eq!(
            eas::find_all_text_after(&response, air::STATUS),
            vec!["1", "1"]
        );
        assert_eq!(
            eas::find_text_after(&response, air::SERVER_ID),
            Some(id.as_str())
        );
        let query = live
            .call(serde_json::json!([["Email/query",{
            "accountId":live.account,"filter":{"hasKeyword":&marker}
        },"0"]]))
            .await;
        assert_eq!(query[0][1]["ids"].as_array().unwrap().len(), 1);
        let cleanup = live
            .call(serde_json::json!([["Email/set",{"accountId":live.account,"destroy":[id]},"0"]]))
            .await;
        assert!(cleanup[0][1]["notDestroyed"].is_null());
    }

    #[tokio::test]
    #[ignore]
    async fn e2e_draft_attachments_round_trip_and_return_client_references() {
        use base64::Engine;
        use eas::{airsync_base as base, item_operations as ops};
        let live = Live::new("E2E_USER2").await;
        let boxes = live
            .call(serde_json::json!([["Mailbox/get",{"accountId":live.account},"0"]]))
            .await;
        let folder = boxes[0][1]["list"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["role"] == "drafts")
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let device = dev("draft-attachments");
        let prime = live
            .wbxml_version("Sync", &device, sync_body("0", &folder), "16.1")
            .await;
        let sync = live
            .wbxml_version(
                "Sync",
                &device,
                sync_body(&sync_key(&prime), &folder),
                "16.1",
            )
            .await;
        let reply = live
            .wbxml_version(
                "Sync",
                &device,
                attachment_draft_add_body(&sync_key(&sync), &folder),
                "16.1",
            )
            .await;
        assert_eq!(
            eas::find_all_text_after(&reply, air::STATUS),
            vec!["1", "1"]
        );
        let id = eas::find_text_after(&reply, air::SERVER_ID)
            .unwrap()
            .to_owned();
        let clients = eas::find_all_text_after(&reply, base::CLIENT_ID);
        let refs = eas::find_all_text_after(&reply, base::FILE_REFERENCE);
        assert_eq!(
            clients,
            vec![
                "file-client",
                "message-client",
                "duplicate-a",
                "duplicate-b"
            ]
        );
        assert_eq!(refs.len(), 4);
        for (reference, expected) in refs.iter().zip([
            DRAFT_ATTACHMENT_BYTES,
            DRAFT_EMBEDDED_BYTES,
            b"same1".as_slice(),
            b"same2".as_slice(),
        ]) {
            let mut b = eas::DocumentBuilder::new();
            b.start(ops::ITEM_OPERATIONS);
            b.start(ops::FETCH);
            b.leaf(ops::STORE, "Mailbox");
            b.leaf(base::FILE_REFERENCE, reference);
            b.end();
            b.end();
            let result = live
                .wbxml_version(
                    "ItemOperations",
                    &device,
                    wbxml::encode_document(&b.finish()),
                    "16.1",
                )
                .await;
            let data = eas::find_text_after(&result, ops::DATA).unwrap();
            assert_eq!(
                base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .unwrap(),
                expected
            );
        }
        let peer = dev("draft-attachment-peer");
        let prime = live
            .wbxml_version("Sync", &peer, sync_body("0", &folder), "16.1")
            .await;
        let sync = live
            .wbxml_version("Sync", &peer, sync_body(&sync_key(&prime), &folder), "16.1")
            .await;
        let collection = eas::child_elements(&sync).into_iter().flat_map(|d| eas::child_elements(&d))
            .find(|d| matches!(d.nodes.first(), Some(wbxml::Node::Start(t)) if t.code_page == air::PAGE && t.token == air::COLLECTION.token)).unwrap();
        let commands = eas::child_elements(&collection).into_iter()
            .find(|d| matches!(d.nodes.first(), Some(wbxml::Node::Start(t)) if t.code_page == air::PAGE && t.token == air::COMMANDS.token)).unwrap();
        let item = eas::child_elements(&commands)
            .into_iter()
            .find(|item| eas::find_text_after(item, air::SERVER_ID) == Some(id.as_str()))
            .unwrap();
        assert!(eas::find_all_text_after(&item, base::METHOD).contains(&"5".to_owned()));
        assert_eq!(
            eas::find_text_after(&item, base::CONTENT_ID),
            Some("inline@example.test")
        );
        assert_eq!(
            eas::find_text_after(&item, base::CONTENT_LOCATION),
            Some("https://example.test/image")
        );
        assert!(eas::find_all_text_after(&item, base::IS_INLINE).contains(&"1".to_owned()));
        let cleanup = live
            .call(serde_json::json!([["Email/set",{"accountId":live.account,"destroy":[id]},"0"]]))
            .await;
        assert!(cleanup[0][1]["notDestroyed"].is_null());
    }

    #[tokio::test]
    #[ignore]
    async fn e2e_ios_mime_fetch_is_a_wbxml_string() {
        use eas::{airsync_base as base, email as mail};
        let live = Live::new("E2E_USER2").await;
        let created = live.call(serde_json::json!([["Mailbox/set",{
            "accountId":live.account,"create":{"m":{"name":format!("MIME fetch fixture {}",std::process::id())}}
        },"0"]])).await;
        let folder = created[0][1]["created"]["m"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let ids = live.seed(&folder, 1, "mime-fetch", false).await;
        let device = dev("mime-fetch");
        let prime = live
            .wbxml_version("Sync", &device, sync_body("0", &folder), "14.0")
            .await;
        let synced = live
            .wbxml_version(
                "Sync",
                &device,
                sync_body(&sync_key(&prime), &folder),
                "14.0",
            )
            .await;
        let mut b = eas::DocumentBuilder::new();
        b.start(air::SYNC);
        b.start(air::COLLECTIONS);
        b.start(air::COLLECTION);
        b.leaf(air::SYNC_KEY, sync_key(&synced));
        b.leaf(air::COLLECTION_ID, &folder);
        b.leaf(air::GET_CHANGES, "0");
        b.start(air::OPTIONS);
        b.leaf(air::MIME_SUPPORT, "2");
        b.start(base::BODY_PREFERENCE);
        b.leaf(base::TYPE, "4");
        b.end();
        b.end();
        b.start(air::COMMANDS);
        b.start(air::FETCH);
        b.leaf(air::SERVER_ID, &ids[0]);
        for _ in 0..5 {
            b.end();
        }
        let response = live
            .wbxml_version("Sync", &device, wbxml::encode_document(&b.finish()), "14.0")
            .await;
        assert_eq!(
            eas::find_all_text_after(&response, air::STATUS),
            vec!["1", "1"]
        );
        assert_eq!(eas::find_text_after(&response, base::TYPE), Some("4"));
        assert!(eas::find_text_after(&response, mail::SUBJECT).is_some());
        let body =
            eas::find_text_after(&response, base::DATA).expect("MIME must be a WBXML string");
        assert!(body.contains("body 0"));
        assert!(eas::find_opaque_after(&response, base::DATA).is_none());
        let cleanup = live
            .call(serde_json::json!([
                ["Email/set",{"accountId":live.account,"destroy":ids},"0"],
                ["Mailbox/set",{"accountId":live.account,"destroy":[folder]},"1"]
            ]))
            .await;
        assert!(cleanup[0][1]["notDestroyed"].is_null());
        assert!(cleanup[1][1]["notDestroyed"].is_null());
    }

    #[tokio::test]
    #[ignore]
    async fn e2e_mime_draft_import_preserves_original_bytes() {
        use eas::airsync_base as base;
        let live = Live::new("E2E_USER2").await;
        let boxes = live
            .call(serde_json::json!([["Mailbox/get",{"accountId":live.account},"0"]]))
            .await;
        let folder = boxes[0][1]["list"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["role"] == "drafts")
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let device = dev("draft-mime");
        let prime = live
            .wbxml_version("Sync", &device, sync_body("0", &folder), "16.1")
            .await;
        let sync = live
            .wbxml_version(
                "Sync",
                &device,
                sync_body(&sync_key(&prime), &folder),
                "16.1",
            )
            .await;
        let raw = b"From: sender@example.test\r\nTo: recipient@example.test\r\nSubject: MIME draft fixture\r\nMIME-Version: 1.0\r\nContent-Type: text/plain\r\n\r\nOriginal MIME body.\r\n";
        let reply = live
            .wbxml_version(
                "Sync",
                &device,
                mime_draft_add_body(&sync_key(&sync), &folder, raw),
                "16.1",
            )
            .await;
        assert_eq!(
            eas::find_all_text_after(&reply, air::STATUS),
            vec!["1", "1"]
        );
        let id = eas::find_text_after(&reply, air::SERVER_ID)
            .unwrap()
            .to_owned();
        let peer = dev("draft-mime-peer");
        let options = mime_options(Some("2"), u32::MAX as usize);
        let prime = live
            .wbxml_version(
                "Sync",
                &peer,
                sync_body_preferences("0", &folder, Some(&options), true),
                "16.1",
            )
            .await;
        let sync = live
            .wbxml_version(
                "Sync",
                &peer,
                sync_body_preferences(&sync_key(&prime), &folder, None, true),
                "16.1",
            )
            .await;
        let collection = eas::child_elements(&sync).into_iter().flat_map(|d| eas::child_elements(&d))
            .find(|d| matches!(d.nodes.first(), Some(wbxml::Node::Start(t)) if t.code_page == air::PAGE && t.token == air::COLLECTION.token)).unwrap();
        let commands = eas::child_elements(&collection).into_iter()
            .find(|d| matches!(d.nodes.first(), Some(wbxml::Node::Start(t)) if t.code_page == air::PAGE && t.token == air::COMMANDS.token)).unwrap();
        let item = eas::child_elements(&commands)
            .into_iter()
            .find(|item| eas::find_text_after(item, air::SERVER_ID) == Some(id.as_str()))
            .unwrap();
        assert_eq!(eas::find_text_after(&item, base::TYPE), Some("4"));
        assert_eq!(
            eas::find_text_after(&item, base::DATA).map(str::as_bytes),
            Some(raw.as_slice())
        );
        assert!(eas::find_opaque_after(&item, base::DATA).is_none());
        let cleanup = live
            .call(serde_json::json!([["Email/set",{"accountId":live.account,"destroy":[id]},"0"]]))
            .await;
        assert!(cleanup[0][1]["notDestroyed"].is_null());
    }

    #[tokio::test]
    #[ignore]
    async fn e2e_draft_bcc_is_returned_on_sync_and_fetch() {
        use eas::{email2, item_operations as ops};
        let live = Live::new("E2E_USER2").await;
        let boxes = live
            .call(serde_json::json!([["Mailbox/get",{"accountId":live.account},"0"]]))
            .await;
        let folder = boxes[0][1]["list"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["role"] == "drafts")
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let created = live.call(serde_json::json!([["Email/set",{
            "accountId":live.account,"create":{"e":{"mailboxIds":{&folder:true},"keywords":{"$draft":true},
                "subject":"draft-bcc-fixture","bcc":[{"name":"Hidden","email":"hidden@example.test"}],
                "bodyValues":{"b":{"value":"test"}},"textBody":[{"partId":"b","type":"text/plain"}]}}
        },"0"]])).await;
        let id = created[0][1]["created"]["e"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let device = dev("draft-bcc");
        let prime = live
            .wbxml_version("Sync", &device, sync_body("0", &folder), "16.1")
            .await;
        let sync = live
            .wbxml_version(
                "Sync",
                &device,
                sync_body(&sync_key(&prime), &folder),
                "16.1",
            )
            .await;
        assert!(eas::find_all_text_after(&sync, email2::BCC)
            .iter()
            .any(|value| value.contains("hidden@example.test")));
        let mut b = eas::DocumentBuilder::new();
        b.start(ops::ITEM_OPERATIONS);
        b.start(ops::FETCH);
        b.leaf(ops::STORE, "Mailbox");
        b.leaf(air::COLLECTION_ID, &folder);
        b.leaf(air::SERVER_ID, &id);
        b.end();
        b.end();
        let fetched = live
            .wbxml_version(
                "ItemOperations",
                &device,
                wbxml::encode_document(&b.finish()),
                "16.1",
            )
            .await;
        assert!(eas::find_text_after(&fetched, email2::BCC)
            .is_some_and(|value| value.contains("hidden@example.test")));
        let result = live
            .call(serde_json::json!([["Email/set",{"accountId":live.account,"destroy":[id]},"0"]]))
            .await;
        assert!(result[0][1]["notDestroyed"].is_null());
    }

    #[tokio::test]
    #[ignore]
    async fn e2e_unsupported_draft_change_returns_item_error() {
        let live = Live::new("E2E_USER2").await;
        let r = live.call(serde_json::json!([["Mailbox/set", {
            "accountId":live.account, "create":{"m":{"name":format!("Draft guard {}",std::process::id())}}
        },"0"]])).await;
        let folder = r[0][1]["created"]["m"]["id"].as_str().unwrap().to_owned();
        let ids = live.seed(&folder, 1, "draft-guard", false).await;
        let device = dev("draft-guard");
        let prime = live
            .wbxml_version("Sync", &device, sync_body("0", &folder), "16.1")
            .await;
        let sync = live
            .wbxml_version(
                "Sync",
                &device,
                sync_body(&sync_key(&prime), &folder),
                "16.1",
            )
            .await;
        let result = live
            .wbxml_version(
                "Sync",
                &device,
                unsupported_mail_change_body(&sync_key(&sync), &folder, &ids[0]),
                "16.1",
            )
            .await;
        assert!(eas::find_all_text_after(&result, air::STATUS).contains(&"6".to_owned()));
        let native = live
            .call(serde_json::json!([["Email/get",{
            "accountId":live.account,"ids":ids,"properties":["id","subject","keywords"]
        },"0"]]))
            .await;
        let item = &native[0][1]["list"][0];
        assert_ne!(item["subject"], "Edited draft");
        assert!(item["keywords"]["$seen"] != true);
        let cleanup = live
            .call(serde_json::json!([
                ["Email/set",{"accountId":live.account,"destroy":ids},"0"],
                ["Mailbox/set",{"accountId":live.account,"destroy":[folder]},"1"]
            ]))
            .await;
        assert!(cleanup[0][1]["notDestroyed"].is_null());
        assert!(cleanup[1][1]["notDestroyed"].is_null());
    }

    #[tokio::test]
    #[ignore]
    async fn e2e_flag_validation_keeps_failed_items_unchanged_and_replays_mixed_results() {
        let live = Live::new("E2E_USER2").await;
        let r = live.call(serde_json::json!([["Mailbox/set", {
            "accountId": live.account, "create": {"m":{"name":format!("Flag validation {}", std::process::id())}}
        }, "0"]])).await;
        let folder = r[0][1]["created"]["m"]["id"].as_str().unwrap().to_owned();
        let ids = live.seed(&folder, 2, "flag-validation", false).await;
        let device = dev("flag-validation");
        let doc = live
            .wbxml_version("Sync", &device, sync_body("0", &folder), "16.1")
            .await;
        let doc = live
            .wbxml_version("Sync", &device, sync_body(&sync_key(&doc), &folder), "16.1")
            .await;
        let body = mixed_flag_request(&sync_key(&doc), &folder, &ids[0], &ids[1]);
        let doc = live
            .wbxml_version("Sync", &device, body.clone(), "16.1")
            .await;
        assert_eq!(
            eas::find_all_text_after(&doc, air::STATUS),
            vec!["1", "6", "1"]
        );
        assert_eq!(live.wbxml_version("Sync", &device, body, "16.1").await, doc);
        let r = live
            .call(serde_json::json!([["Email/get",{
            "accountId":live.account,"ids":ids,"properties":["id","keywords"]
        },"0"]]))
            .await;
        let list = r[0][1]["list"].as_array().unwrap();
        let failed = list.iter().find(|email| email["id"] == ids[0]).unwrap();
        let good = list.iter().find(|email| email["id"] == ids[1]).unwrap();
        assert!(failed["keywords"]["$seen"] != true && failed["keywords"]["$flagged"] != true);
        assert_eq!(good["keywords"]["$seen"], true);

        let body = flag_change_body(&sync_key(&doc), &folder, &ids[0], Some("2"), None, false);
        let doc = live.wbxml_version("Sync", &device, body, "16.1").await;
        assert_eq!(eas::find_all_text_after(&doc, air::STATUS), vec!["1"]);
        let r = live
            .call(serde_json::json!([["Email/get",{
            "accountId":live.account,"ids":[&ids[0]],"properties":["id","keywords"]
        },"0"]]))
            .await;
        assert_eq!(r[0][1]["list"][0]["keywords"]["$flagged"], true);
        let peer = dev("flag-peer");
        let peer0 = live
            .wbxml_version("Sync", &peer, sync_body("0", &folder), "16.1")
            .await;
        let peer1 = live
            .wbxml_version("Sync", &peer, sync_body(&sync_key(&peer0), &folder), "16.1")
            .await;
        assert_eq!(
            eas::find_text_after(&peer1, eas::tasks::DUE_DATE),
            Some("2026-11-14T12:00:00.000Z")
        );
        let updated = live
            .wbxml_version(
                "Sync",
                &device,
                flag_metadata_body(&sync_key(&doc), &folder, &ids[0]),
                "16.1",
            )
            .await;
        assert_eq!(eas::find_text_after(&updated, air::STATUS), Some("1"));
        let peer_update = live
            .wbxml_version("Sync", &peer, sync_body(&sync_key(&peer1), &folder), "16.1")
            .await;
        assert_eq!(
            eas::find_text_after(&peer_update, eas::tasks::REMINDER_TIME),
            Some("2026-11-15T10:00:00.000Z")
        );
        assert_eq!(
            eas::find_text_after(&peer_update, eas::tasks::DUE_DATE),
            Some("2026-11-14T12:00:00.000Z")
        );
        let finished = live
            .wbxml_version(
                "Sync",
                &device,
                complete_flag_body(&sync_key(&updated), &folder, &ids[0]),
                "16.1",
            )
            .await;
        assert_eq!(eas::find_text_after(&finished, air::STATUS), Some("1"));
        let peer2 = live
            .wbxml_version(
                "Sync",
                &peer,
                sync_body(&sync_key(&peer_update), &folder),
                "16.1",
            )
            .await;
        assert_eq!(
            eas::find_text_after(&peer2, eas::email::FLAG_STATUS),
            Some("1")
        );
        assert_eq!(
            eas::find_text_after(&peer2, eas::tasks::DATE_COMPLETED),
            Some("2026-11-15T11:31:00.000Z")
        );
        assert_eq!(
            eas::find_text_after(&peer2, eas::tasks::DUE_DATE),
            Some("2026-11-14T12:00:00.000Z")
        );
        assert_eq!(
            eas::find_text_after(&peer2, eas::tasks::REMINDER_TIME),
            Some("2026-11-15T10:00:00.000Z")
        );
        let r = live
            .call(serde_json::json!([["Email/set", {
            "accountId": live.account, "destroy": ids
        }, "0"],["Mailbox/set", {"accountId":live.account,"destroy":[&folder]}, "1"]]))
            .await;
        assert!(r[0][1]["notDestroyed"].is_null());
        assert!(r[1][1]["notDestroyed"].is_null());
    }

    #[tokio::test]
    #[ignore]
    async fn e2e_mime_policy_uses_native_smime_structure_and_preserves_original_bytes() {
        use eas::{airsync_base as base, email as mail};
        let mut live = Live::new("E2E_USER2").await;
        let r = live.call(serde_json::json!([["Mailbox/set", {
            "accountId":live.account, "create":{"m":{"name":format!("MIME policy {}",std::process::id())}}
        },"0"]])).await;
        let folder = r[0][1]["created"]["m"]["id"].as_str().unwrap().to_owned();
        let mut ids = Vec::new();
        for (kind, body, class) in [
            ("text/plain", "ordinary mail", "IPM.Note"),
            ("application/pkcs7-mime; smime-type=enveloped-data", "AAECAwQF", "IPM.Note.SMIME"),
            ("multipart/signed; protocol=\"application/pkcs7-signature\"; boundary=\"signed\"",
                "--signed\r\nContent-Type: text/plain\r\n\r\nsigned text\r\n--signed\r\nContent-Type: application/pkcs7-signature\r\nContent-Transfer-Encoding: base64\r\n\r\nAAECAwQF\r\n--signed--\r\n",
                "IPM.Note.SMIME.MultipartSigned"),
        ] {
            // Synthetic envelopes exercise MIME transport, not cryptographic validation.
            let raw = format!("From: test@example.test\r\nTo: {}\r\nSubject: MIME policy\r\nMIME-Version: 1.0\r\nContent-Type: {kind}\r\n\r\n{body}", live.user);
            let upload: serde_json::Value = live.http
                .post(format!("{}/jmap/upload/{}/",live.jmap,live.account))
                .basic_auth(&live.user, Some(&live.pass)).header("Content-Type","message/rfc822")
                .body(raw.clone()).send().await.unwrap().json().await.unwrap();
            let r = live.call(serde_json::json!([["Email/import",{
                "accountId":live.account,
                "emails":{"m":{"blobId":upload["blobId"],"mailboxIds":{&folder:true},"keywords":{}}}
            },"0"]])).await;
            let id = r[0][1]["created"]["m"]["id"].as_str().unwrap().to_owned();
            ids.push(id.clone());
            for mode in [None, Some("0"), Some("1"), Some("2")] {
                let mime = mode == Some("2") || (mode == Some("1") && class != "IPM.Note");
                let device = dev(&format!("mime-{class}-{mode:?}"));
                let options = mime_options(mode, u32::MAX as usize);
                let prime = live.wbxml_version("Sync", &device,
                    sync_body_preferences("0",&folder,Some(&options),true),"16.1").await;
                // Persisted MIME policy must survive a process restart.
                if mode == Some("1") {
                    let out = std::process::Command::new("sh").arg("-c")
                        .arg(env("E2E_RESTART_CMD")).output().unwrap();
                    assert!(out.status.success());
                    live = Live::new("E2E_USER2").await;
                }
                let doc = live.wbxml_version("Sync", &device,
                    sync_body_preferences(&sync_key(&prime),&folder,None,true),"16.1").await;
                use eas::airsync as air;
                let collection = eas::child_elements(&doc).into_iter().flat_map(|d|eas::child_elements(&d))
                    .find(|d|matches!(d.nodes.first(),Some(wbxml::Node::Start(t)) if t.token == air::COLLECTION.token && t.code_page == air::PAGE)).unwrap();
                let commands = eas::child_elements(&collection).into_iter().find(|d|
                    matches!(d.nodes.first(),Some(wbxml::Node::Start(t)) if t.token == air::COMMANDS.token && t.code_page == air::PAGE)).unwrap();
                let item = eas::child_elements(&commands).into_iter()
                    .find(|d|eas::find_text_after(d,air::SERVER_ID)==Some(id.as_str())).unwrap();
                assert_eq!(eas::find_text_after(&item,mail::MESSAGE_CLASS),Some(class));
                if mime {
                    assert_eq!(eas::find_text_after(&item,base::TYPE),Some("4"));
                    assert_eq!(body_data_bytes(&item),Some(raw.as_bytes()));
                } else {
                    assert_ne!(eas::find_text_after(&item,base::TYPE),Some("4"));
                    assert!(eas::find_opaque_after(&item, base::DATA).is_none());
                }
            }
        }
        let r = live
            .call(serde_json::json!([
                ["Email/set",{"accountId":live.account,"destroy":ids},"0"],
                ["Mailbox/set",{"accountId":live.account,"destroy":[folder]},"1"]
            ]))
            .await;
        assert!(r[0][1]["notDestroyed"].is_null());
        assert!(r[1][1]["notDestroyed"].is_null());
    }

    #[tokio::test]
    #[ignore]
    async fn e2e_body_preferences_choose_native_fallback_and_persist_after_restart() {
        use eas::{airsync_base as base, RequestedBodyPreference as Pref};
        let live = Live::new("E2E_USER2").await;
        let r = live.call(serde_json::json!([["Mailbox/set",{
            "accountId":live.account,"create":{"m":{"name":format!("Body preferences {}",std::process::id())}}
        },"0"]])).await;
        let folder = r[0][1]["created"]["m"]["id"].as_str().unwrap().to_owned();
        let html = "<p>éclair hello from JMAP</p>";
        let r = live
            .call(serde_json::json!([["Email/set",{
            "accountId":live.account,"create":{"m":{"mailboxIds":{&folder:true},"keywords":{},
                "from":[{"email":"test@example.test"}],"to":[{"email":live.user}],
                "subject":"Body preferences","bodyValues":{"b":{"value":html}},
                "htmlBody":[{"partId":"b","type":"text/html"}]}}
        },"0"]]))
            .await;
        let id = r[0][1]["created"]["m"]["id"].as_str().unwrap().to_owned();
        let device = dev("body-preferences");
        let options = requested_body_options(&[
            Pref {
                body_type: 2,
                truncation_size: Some(4),
                all_or_none: true,
                preview: Some(0),
            },
            Pref {
                body_type: 1,
                truncation_size: Some(3),
                preview: Some(2),
                ..Default::default()
            },
        ]);
        let prime = live
            .wbxml_version(
                "Sync",
                &device,
                sync_body_preferences("0", &folder, Some(&options), true),
                "16.1",
            )
            .await;
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(env("E2E_RESTART_CMD"))
            .output()
            .unwrap();
        assert!(out.status.success());
        let live = Live::new("E2E_USER2").await;
        let doc = live
            .wbxml_version(
                "Sync",
                &device,
                sync_body_preferences(&sync_key(&prime), &folder, None, true),
                "16.1",
            )
            .await;
        assert_eq!(eas::find_text_after(&doc, base::TYPE), Some("1"));
        assert_eq!(eas::find_text_after(&doc, base::DATA), Some("éc"));
        assert_eq!(eas::find_text_after(&doc, base::PREVIEW), Some("éc"));
        assert_eq!(
            eas::find_text_after(&doc, base::ESTIMATED_DATA_SIZE),
            Some("éclair hello from JMAP".len().to_string()).as_deref()
        );
        let second = dev("native-body");
        let options = requested_body_options(&[
            Pref {
                body_type: 2,
                preview: Some(0),
                ..Default::default()
            },
            Pref {
                body_type: 1,
                ..Default::default()
            },
        ]);
        let prime = live
            .wbxml_version(
                "Sync",
                &second,
                sync_body_preferences("0", &folder, Some(&options), true),
                "16.1",
            )
            .await;
        let doc = live
            .wbxml_version(
                "Sync",
                &second,
                sync_body_preferences(&sync_key(&prime), &folder, None, true),
                "16.1",
            )
            .await;
        assert_eq!(eas::find_text_after(&doc, base::TYPE), Some("2"));
        assert_eq!(eas::find_text_after(&doc, base::DATA), Some(html));
        assert!(!eas::contains_token(&doc, base::PREVIEW));
        let mime_device = dev("mime-body");
        let options = requested_body_options(&[Pref {
            body_type: 4,
            truncation_size: Some(64),
            preview: Some(0),
            ..Default::default()
        }]);
        let prime = live
            .wbxml_version(
                "Sync",
                &mime_device,
                sync_body_preferences("0", &folder, Some(&options), true),
                "16.1",
            )
            .await;
        let doc = live
            .wbxml_version(
                "Sync",
                &mime_device,
                sync_body_preferences(&sync_key(&prime), &folder, None, true),
                "16.1",
            )
            .await;
        assert_eq!(eas::find_text_after(&doc, base::TYPE), Some("4"));
        assert_eq!(body_data_bytes(&doc).unwrap().len(), 64);
        assert!(
            eas::find_text_after(&doc, base::ESTIMATED_DATA_SIZE)
                .unwrap()
                .parse::<usize>()
                .unwrap()
                > 64
        );
        assert_eq!(eas::find_text_after(&doc, base::TRUNCATED), Some("1"));
        assert!(!eas::contains_token(&doc, base::PREVIEW));
        let r = live
            .call(
                serde_json::json!([["Email/set",{"accountId":live.account,"destroy":[id]},"0"],
            ["Mailbox/set",{"accountId":live.account,"destroy":[folder]},"1"]]),
            )
            .await;
        assert!(r[0][1]["notDestroyed"].is_null());
        assert!(r[1][1]["notDestroyed"].is_null());
    }

    #[tokio::test]
    #[ignore]
    async fn e2e_large_jmap_text_is_not_reported_as_complete_at_64k() {
        use eas::{airsync_base as base, item_operations as ops, RequestedBodyPreference as Pref};
        let live = Live::new("E2E_USER2").await;
        let r=live.call(serde_json::json!([["Mailbox/set",{
            "accountId":live.account,"create":{"m":{"name":format!("Large body {}",std::process::id())}}
        },"0"]])).await;
        let folder = r[0][1]["created"]["m"]["id"].as_str().unwrap().to_owned();
        let plain = format!("{}END_AFTER_64K", "é ".repeat(40_000));
        let html = format!("<p>{plain}</p>");
        let r = live
            .call(serde_json::json!([["Email/set",{
            "accountId":live.account,"create":{"m":{"mailboxIds":{&folder:true},"keywords":{},
            "from":[{"email":"test@example.test"}],"to":[{"email":live.user}],
            "subject":"Large body","bodyValues":{"b":{"value":html}},
            "htmlBody":[{"partId":"b","type":"text/html"}]}}
        },"0"]]))
            .await;
        let id = r[0][1]["created"]["m"]["id"].as_str().unwrap().to_owned();
        let r = live
            .call(serde_json::json!([["Email/get",{
            "accountId":live.account,"ids":[&id],"properties":["id","htmlBody","bodyValues"],
            "fetchAllBodyValues":true,"maxBodyValueBytes":65536
        },"0"]]))
            .await;
        assert!(r[0][1]["list"][0]["bodyValues"]
            .as_object()
            .unwrap()
            .values()
            .any(|body| body["isTruncated"] == true));
        let options = requested_body_options(&[
            Pref {
                body_type: 2,
                truncation_size: Some(65_540),
                all_or_none: true,
                preview: Some(0),
            },
            Pref {
                body_type: 1,
                truncation_size: Some(4),
                preview: Some(3),
                ..Default::default()
            },
        ]);
        let device = dev("large-text");
        let prime = live
            .wbxml_version(
                "Sync",
                &device,
                sync_body_preferences("0", &folder, Some(&options), true),
                "16.1",
            )
            .await;
        let doc = live
            .wbxml_version(
                "Sync",
                &device,
                sync_body_preferences(&sync_key(&prime), &folder, None, true),
                "16.1",
            )
            .await;
        assert_eq!(eas::find_text_after(&doc, base::TYPE), Some("1"));
        assert_eq!(eas::find_text_after(&doc, base::DATA), Some("é "));
        assert_eq!(
            eas::find_text_after(&doc, base::ESTIMATED_DATA_SIZE),
            Some(plain.len().to_string()).as_deref()
        );
        let mut b = eas::DocumentBuilder::new();
        b.start(ops::ITEM_OPERATIONS);
        b.start(ops::FETCH);
        b.leaf(ops::STORE, "Mailbox");
        b.leaf(air::COLLECTION_ID, &folder);
        b.leaf(air::SERVER_ID, &id);
        b.end();
        b.end();
        let doc = live
            .wbxml_version(
                "ItemOperations",
                &device,
                wbxml::encode_document(&b.finish()),
                "16.1",
            )
            .await;
        assert_eq!(eas::find_text_after(&doc, base::DATA), Some(html.as_str()));
        assert_eq!(
            eas::find_text_after(&doc, base::ESTIMATED_DATA_SIZE),
            Some(html.len().to_string()).as_deref()
        );
        assert_eq!(eas::find_text_after(&doc, base::TRUNCATED), Some("0"));
        let r = live
            .call(
                serde_json::json!([["Email/set",{"accountId":live.account,"destroy":[id]},"0"],
            ["Mailbox/set",{"accountId":live.account,"destroy":[folder]},"1"]]),
            )
            .await;
        assert!(r[0][1]["notDestroyed"].is_null());
        assert!(r[1][1]["notDestroyed"].is_null());
    }

    #[tokio::test]
    #[ignore]
    async fn e2e_lost_response_is_replayed() {
        let live = Live::new("E2E_USER").await;
        let d = dev("replay");
        let (key, _, _) = live.initial_sync(&d, "100").await;
        live.seed(&live.inbox, 1, "replay", false).await;
        let first = live.wbxml("Sync", &d, sync_body(&key, &live.inbox)).await;
        let again = live.wbxml("Sync", &d, sync_body(&key, &live.inbox)).await;
        assert_eq!(again, first, "resent SyncKey gets the identical reply");
    }

    #[tokio::test]
    #[ignore]
    async fn e2e_folder_create_rename_delete() {
        use eas::folder_hierarchy as fh;
        let live = Live::new("E2E_USER").await;
        let d = dev("folders");
        let fs = |key: &str| {
            let mut b = eas::DocumentBuilder::new();
            b.start(fh::FOLDER_SYNC);
            b.leaf(fh::SYNC_KEY, key);
            b.end();
            wbxml::encode_document(&b.finish())
        };
        let doc = live.wbxml("FolderSync", &d, fs("0")).await;
        let key = eas::find_text_after(&doc, fh::SYNC_KEY).unwrap().to_owned();
        let name = format!("E2E folder {}", std::process::id());
        let r = live.call(serde_json::json!([["Mailbox/set", {"accountId": live.account, "create": {"f": {"name": &name}}}, "0"]])).await;
        let id = r[0][1]["created"]["f"]["id"].as_str().unwrap().to_owned();
        let doc = live.wbxml("FolderSync", &d, fs(&key)).await;
        let key = eas::find_text_after(&doc, fh::SYNC_KEY).unwrap().to_owned();
        assert!(eas::contains_token(&doc, fh::ADD));
        assert_eq!(
            eas::find_all_text_after(&doc, fh::SERVER_ID),
            vec![id.clone()]
        );
        live.call(serde_json::json!([["Mailbox/set", {"accountId": live.account, "update": {&id: {"name": format!("{name} renamed")}}}, "0"]])).await;
        let doc = live.wbxml("FolderSync", &d, fs(&key)).await;
        let key = eas::find_text_after(&doc, fh::SYNC_KEY).unwrap().to_owned();
        assert!(eas::contains_token(&doc, fh::UPDATE));
        live.call(serde_json::json!([["Mailbox/set", {"accountId": live.account, "destroy": [&id]}, "0"]])).await;
        let doc = live.wbxml("FolderSync", &d, fs(&key)).await;
        assert!(eas::contains_token(&doc, fh::DELETE));
        assert_eq!(eas::find_all_text_after(&doc, fh::SERVER_ID), vec![id]);
    }

    #[tokio::test]
    #[ignore]
    async fn e2e_attachment_download() {
        let live = Live::new("E2E_USER2").await;
        let d = dev("attach");
        let (key, _, _) = live.initial_sync(&d, "100").await;
        live.seed(&live.inbox, 1, "attach", true).await;
        let doc = live.wbxml("Sync", &d, sync_body(&key, &live.inbox)).await;
        let reference = eas::find_text_after(&doc, eas::airsync_base::FILE_REFERENCE)
            .expect("attachment advertised")
            .to_owned();
        let user = live.user.split('@').next().unwrap();
        let resp = live
            .http
            .get(format!(
                "{}?Cmd=GetAttachment&User={user}&DeviceId={d}&DeviceType=iPhone&AttachmentName={}",
                live.gw,
                urlencode(&reference)
            ))
            .basic_auth(&live.user, Some(&live.pass))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(
            resp.bytes().await.unwrap().as_ref(),
            &[0u8, 1, 2, 3, 4, 5, 6, 7, 8, 9]
        );
    }

    /// Restarts the real gateway via `E2E_RESTART_CMD` (a shell command that
    /// returns once the new process answers /healthz). Device state must
    /// survive: no re-download, no false Status 2, replay still works.
    #[tokio::test]
    #[ignore]
    async fn e2e_gateway_restart_keeps_device_state() {
        let live = Live::new("E2E_USER").await;
        let d = dev("restart");
        let (key, _, _) = live.initial_sync(&d, "100").await;
        assert_eq!(live.ping(&d, 1).await.0, "1");

        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(env("E2E_RESTART_CMD"))
            .output()
            .unwrap();
        assert!(out.status.success(), "restart command failed: {out:?}");
        // Fresh client: pooled connections died with the old process (a real
        // device sees the same and reconnects).
        let live = Live::new("E2E_USER").await;

        // First Ping after the restart: the gateway lost its cached Ping
        // parameters and the EventSource; nothing changed, so no Status 2.
        assert_eq!(
            live.ping(&d, 2).await.0,
            "1",
            "no false change after restart"
        );
        let doc = live.wbxml("Sync", &d, sync_body(&key, &live.inbox)).await;
        assert_eq!(
            eas::find_all_text_after(&doc, air::STATUS)
                .first()
                .map(String::as_str),
            Some("1")
        );
        assert!(
            eas::find_all_text_after(&doc, air::SERVER_ID).is_empty(),
            "nothing re-sent"
        );

        // New mail after the restart: woken and delivered once.
        let la = Arc::new(Live::new("E2E_USER").await);
        let dd = d.clone();
        let p = tokio::spawn(async move { la.ping(&dd, 30).await });
        tokio::time::sleep(Duration::from_secs(1)).await;
        let new_id = live
            .seed(&live.inbox, 1, "after-restart", false)
            .await
            .remove(0);
        assert_eq!(p.await.unwrap().0, "2");
        let doc = live.wbxml("Sync", &d, sync_body(&key, &live.inbox)).await;
        assert_eq!(eas::find_all_text_after(&doc, air::SERVER_ID), vec![new_id]);
    }

    /// A quiet Ping is held for its whole heartbeat and answered Status 1,
    /// not earlier. `E2E_LONG_PING` seconds (default 120).
    #[tokio::test]
    #[ignore]
    async fn e2e_long_ping_is_held_for_the_heartbeat() {
        let live = Live::new("E2E_USER").await;
        let d = dev("long");
        live.initial_sync(&d, "100").await;
        let hb: u64 = std::env::var("E2E_LONG_PING")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(120);
        let t = Instant::now();
        let (status, _) = live.ping(&d, hb).await;
        let held = t.elapsed();
        assert_eq!(status, "1");
        assert!(
            held >= Duration::from_secs(hb.saturating_sub(2))
                && held <= Duration::from_secs(hb + 10),
            "held {held:?} for a {hb}s heartbeat"
        );
        eprintln!("long Ping: {hb}s heartbeat held {held:?}");
    }

    /// Sync with one client command for `id`: a read flag change, or a Delete.
    fn sync_command_body(key: &str, collection: &str, id: &str, read: Option<bool>) -> Vec<u8> {
        let mut b = eas::DocumentBuilder::new();
        b.start(air::SYNC);
        b.start(air::COLLECTIONS);
        b.start(air::COLLECTION);
        b.leaf(air::SYNC_KEY, key);
        b.leaf(air::COLLECTION_ID, collection);
        b.empty_tag(air::GET_CHANGES);
        b.leaf(air::WINDOW_SIZE, "100");
        b.start(air::COMMANDS);
        match read {
            Some(read) => {
                b.start(air::CHANGE);
                b.leaf(air::SERVER_ID, id);
                b.start(air::APPLICATION_DATA);
                b.leaf(eas::email::READ, if read { "1" } else { "0" });
                b.end();
                b.end();
            }
            None => {
                b.start(air::DELETE);
                b.leaf(air::SERVER_ID, id);
                b.end();
            }
        }
        b.end();
        b.end();
        b.end();
        b.end();
        wbxml::encode_document(&b.finish())
    }

    fn move_items_body(id: &str, src: &str, dst: &str) -> Vec<u8> {
        use eas::move_items as mv;
        let mut b = eas::DocumentBuilder::new();
        b.start(mv::MOVES);
        b.start(mv::MOVE);
        b.leaf(mv::SRC_MSG_ID, id);
        b.leaf(mv::SRC_FLD_ID, src);
        b.leaf(mv::DST_FLD_ID, dst);
        b.end();
        b.end();
        wbxml::encode_document(&b.finish())
    }

    /// One simulated device: its SyncKey and the ServerIds it holds.
    struct Device {
        id: String,
        key: String,
        held: Vec<String>,
    }

    impl Device {
        /// Sync until quiet; returns how many rounds changed the key.
        async fn drain(&mut self, live: &Live) -> usize {
            let mut changed = 0;
            for _ in 0..20 {
                let doc = live
                    .wbxml("Sync", &self.id, sync_body(&self.key, &live.inbox))
                    .await;
                assert_eq!(
                    eas::find_all_text_after(&doc, air::STATUS)
                        .first()
                        .map(String::as_str),
                    Some("1"),
                    "{} Sync status",
                    self.id
                );
                let new_key = eas::find_all_text_after(&doc, air::SYNC_KEY).pop().unwrap();
                apply_reply(&doc, &mut self.held);
                let more = eas::contains_token(&doc, air::MORE_AVAILABLE);
                let progressed = new_key != self.key;
                self.key = new_key;
                if progressed {
                    changed += 1;
                }
                if !progressed && !more {
                    break;
                }
            }
            changed
        }
    }

    /// Applies a Sync reply's Adds and Deletes to a device's held ids.
    fn apply_reply(doc: &wbxml::Document, held: &mut Vec<String>) {
        let mut mode = 0u8;
        let mut it = doc.nodes.iter().peekable();
        while let Some(node) = it.next() {
            if let wbxml::Node::Start(t) = node {
                if t.code_page != air::PAGE {
                    continue;
                }
                if t.token == air::ADD.token {
                    mode = 1;
                } else if t.token == air::DELETE.token {
                    mode = 2;
                } else if t.token == air::CHANGE.token {
                    mode = 3;
                } else if t.token == air::SERVER_ID.token {
                    if let Some(wbxml::Node::Text(id)) = it.peek() {
                        match mode {
                            1 => held.insert(0, id.clone()),
                            2 => held.retain(|h| h != id),
                            _ => {}
                        }
                    }
                }
            }
        }
    }

    /// Randomized check of the push contract on two devices against a real
    /// Stalwart: after every change (made on the server or by one device),
    /// each device that should hear of it gets Ping Status 2 and the next
    /// Sync delivers something; a device whose own change it was, and every
    /// device once caught up, gets Status 1. `E2E_STEPS` (default 30) and
    /// `E2E_SEED` select the sequence. Targets are messages of any age the
    /// devices hold (old ones are reached through Email/changes).
    #[tokio::test]
    #[ignore]
    async fn e2e_random_changes_every_status2_is_delivered_and_none_missed() {
        let live = Live::new("E2E_USER").await;
        let steps: usize = std::env::var("E2E_STEPS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(30);
        let mut seed: u64 = std::env::var("E2E_SEED")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| std::process::id() as u64);
        eprintln!("E2E_SEED={seed}");
        let mut rnd = move |n: usize| {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((seed >> 33) as usize) % n
        };
        let other = live
            .call(serde_json::json!([["Mailbox/set", {"accountId": live.account, "create": {"m": {"name": format!("E2E random {}", std::process::id())}}}, "0"]]))
            .await;
        let other_id = other[0][1]["created"]["m"]["id"]
            .as_str()
            .unwrap()
            .to_owned();

        let mut devs = Vec::new();
        for name in ["ra", "rb"] {
            let id = dev(name);
            // initial_sync returns ids newest first (each round is newest
            // first and rounds go back in time), like apply_reply's inserts.
            let (key, held, _) = live.initial_sync(&id, "100").await;
            devs.push(Device { id, key, held });
        }

        for step in 0..steps {
            // Pick a target among the newest messages both devices hold.
            let held_b: std::collections::HashSet<&String> = devs[1].held.iter().collect();
            let common: Vec<String> = devs[0]
                .held
                .iter()
                .filter(|id| held_b.contains(id))
                .cloned()
                .collect();
            // Half the time one of the newest 25, otherwise any age.
            let target = if rnd(2) == 0 {
                common[rnd(common.len().min(25))].clone()
            } else {
                common[rnd(common.len())].clone()
            };
            let actor = rnd(2); // for device-originated ops
            let op = rnd(7);
            // Which devices must hear about it (false = the actor itself).
            let mut expect = [true, true];
            let label = match op {
                0 => {
                    live.seed(&live.inbox, 1, &format!("rnd{step}"), false)
                        .await;
                    "server: new mail"
                }
                1 => {
                    let r = !live.is_read(&target).await;
                    live.call(serde_json::json!([["Email/set", {"accountId": live.account, "update": {&target: {"keywords/$seen": r}}}, "0"]])).await;
                    "server: read flip"
                }
                2 => {
                    live.call(serde_json::json!([["Email/set", {"accountId": live.account, "update": {&target: {"mailboxIds": {&other_id: true}}}}, "0"]])).await;
                    "server: move out"
                }
                3 => {
                    live.call(serde_json::json!([["Email/set", {"accountId": live.account, "destroy": [&target]}, "0"]])).await;
                    "server: destroy"
                }
                4 => {
                    let r = !live.is_read(&target).await;
                    let d = &mut devs[actor];
                    let doc = live
                        .wbxml(
                            "Sync",
                            &d.id,
                            sync_command_body(&d.key, &live.inbox, &target, Some(r)),
                        )
                        .await;
                    d.key = eas::find_all_text_after(&doc, air::SYNC_KEY).pop().unwrap();
                    apply_reply(&doc, &mut d.held);
                    expect[actor] = false;
                    "device: read flip"
                }
                5 => {
                    let d = &mut devs[actor];
                    let doc = live
                        .wbxml(
                            "Sync",
                            &d.id,
                            sync_command_body(&d.key, &live.inbox, &target, None),
                        )
                        .await;
                    d.key = eas::find_all_text_after(&doc, air::SYNC_KEY).pop().unwrap();
                    apply_reply(&doc, &mut d.held);
                    d.held.retain(|h| h != &target);
                    expect[actor] = false;
                    "device: delete"
                }
                _ => {
                    let d = &mut devs[actor];
                    let resp = live
                        .req("MoveItems", &d.id)
                        .body(move_items_body(&target, &live.inbox, &other_id))
                        .send()
                        .await
                        .unwrap();
                    assert_eq!(resp.status(), 200);
                    d.held.retain(|h| h != &target);
                    expect[actor] = false;
                    "device: MoveItems"
                }
            };
            let ctx = format!("step {step} {label} target {target} actor {actor}");
            for (i, d) in devs.iter_mut().enumerate() {
                if expect[i] {
                    let (status, _) = live.ping(&d.id, 10).await;
                    assert_eq!(status, "2", "{ctx}: device {i} must be told");
                    assert!(
                        d.drain(&live).await > 0,
                        "{ctx}: device {i} Status 2 was not deliverable"
                    );
                } else {
                    // The actor caught up on anything its own reply did not carry.
                    d.drain(&live).await;
                }
            }
            for (i, d) in devs.iter().enumerate() {
                let (status, _) = live.ping(&d.id, 1).await;
                assert_eq!(
                    status, "1",
                    "{ctx}: device {i} must be quiet once caught up"
                );
            }
            // Both devices hold the same messages.
            let (mut a, mut b) = (devs[0].held.clone(), devs[1].held.clone());
            a.sort();
            b.sort();
            assert_eq!(a, b, "{ctx}: devices diverged");
        }
        eprintln!("{steps} random steps passed");
    }

    fn urlencode(s: &str) -> String {
        s.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                    (b as char).to_string()
                }
                _ => format!("%{b:02X}"),
            })
            .collect()
    }
}

// ------------------------------------------------- FilterType aged-out cleanup

#[derive(Debug, Default)]
struct Cmds {
    key: String,
    adds: Vec<String>,
    deletes: Vec<String>,
    soft: Vec<String>,
    changes: Vec<String>,
    more: bool,
}

fn ids_after(outline: &str, open: &str) -> Vec<String> {
    let pat = format!("{open}<0:0d>");
    let mut out = Vec::new();
    let mut rest = outline;
    while let Some(at) = rest.find(&pat) {
        rest = &rest[at + pat.len()..];
        out.push(rest[..rest.find("</>").unwrap()].to_owned());
    }
    out
}

/// One mail Sync of the Inbox with an optional FilterType.
async fn sync_cmds(env: &Env, device: &str, key: &str, filter: Option<&str>, window: &str) -> Cmds {
    use eas::airsync as air;
    let resp = env
        .req("Sync", device)
        .body(sync_body_options(key, "inbox", window, filter))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let doc = wbxml::decode_document(&resp.bytes().await.unwrap()).unwrap();
    let outline = crate::activesync::wbxml_outline_for_tests(&doc);
    Cmds {
        key: eas::find_all_text_after(&doc, air::SYNC_KEY).pop().unwrap(),
        adds: ids_after(&outline, "<0:07>"),
        deletes: ids_after(&outline, "<0:09>"),
        soft: ids_after(&outline, "<0:21>"),
        changes: ids_after(&outline, "<0:08>"),
        more: outline.contains("<0:14/>"),
    }
}

/// SyncKey 0 then pages until done; returns the final key and every Add.
async fn initial_sync(env: &Env, device: &str, filter: Option<&str>) -> (String, Vec<String>) {
    let mut key = sync_cmds(env, device, "0", filter, "25").await.key;
    let mut adds = Vec::new();
    loop {
        let r = sync_cmds(env, device, &key, filter, "25").await;
        adds.extend(r.adds);
        let done = !r.more && r.key == key;
        key = r.key;
        if done {
            return (key, adds);
        }
    }
}

fn still_on_server(env: &Env, id: &str) -> bool {
    env.mock
        .boxes
        .lock()
        .unwrap()
        .get("inbox")
        .is_some_and(|b| b.iter().any(|i| i == id))
}

#[tokio::test]
async fn no_filter_never_removes_old_mail() {
    let env = Arc::new(start_env(Opts::default()).await);
    env.mock.add_mail("inbox", "old");
    env.mock.set_date("old", &days_ago(400));
    let (key, adds) = initial_sync(&env, "iphone", Some("0")).await;
    assert!(adds.contains(&"old".to_owned()));
    for filter in [Some("0"), None] {
        let r = sync_cmds(&env, "iphone", &key, filter, "25").await;
        assert!(r.soft.is_empty() && r.deletes.is_empty(), "{r:?}");
        assert_eq!(r.key, key);
    }
}

#[tokio::test]
async fn previously_synced_mail_older_than_one_month_is_soft_deleted_once_and_kept_on_the_server() {
    // The iPad case: messages it got before FilterType was honoured (12m)
    // are older than its one-month window.
    let env = Arc::new(start_env(Opts::default()).await);
    for (id, days) in [("old40", 40), ("old90", 90)] {
        env.mock.add_mail("inbox", id);
        env.mock.set_date(id, &days_ago(days));
    }
    let (key, adds) = initial_sync(&env, "ipad", Some("0")).await;
    assert_eq!(adds.len(), 4);
    let r = sync_cmds(&env, "ipad", &key, Some("5"), "25").await;
    let mut soft = r.soft.clone();
    soft.sort();
    assert_eq!(soft, vec!["old40", "old90"]);
    assert!(
        r.deletes.is_empty() && r.adds.is_empty() && r.changes.is_empty(),
        "{r:?}"
    );
    assert_ne!(r.key, key, "SyncKey advances");
    assert!(still_on_server(&env, "old40") && still_on_server(&env, "old90"));
    // Repeated Syncs (with and without Options): nothing again.
    for filter in [Some("5"), None, Some("5")] {
        let again = sync_cmds(&env, "ipad", &r.key, filter, "25").await;
        assert!(
            again.soft.is_empty() && again.deletes.is_empty(),
            "{again:?}"
        );
        assert_eq!(again.key, r.key);
    }
    // Ping has nothing to announce about them.
    let p = env.ping("ipad", Some(ping_body(1, INBOX))).await;
    assert_eq!(p.status, "1");
}

#[tokio::test]
async fn mail_that_ages_past_the_cutoff_while_held_is_soft_deleted_without_waking_ping() {
    let env = Arc::new(start_env(Opts::default()).await);
    let (key, adds) = initial_sync(&env, "iphone", Some("5")).await;
    assert_eq!(adds.len(), 2);
    // Time passes: m1 is now older than one month.
    env.mock.set_date("m1", &days_ago(32));
    let p = env.ping("iphone", Some(ping_body(1, INBOX))).await;
    assert_eq!(p.status, "1", "ageing alone does not wake the device");
    let r = sync_cmds(&env, "iphone", &key, Some("5"), "25").await;
    assert_eq!(r.soft, vec!["m1"]);
    assert!(still_on_server(&env, "m1"));
}

#[tokio::test]
async fn one_month_boundary_keeps_mail_just_inside_and_removes_mail_just_outside() {
    let env = Arc::new(start_env(Opts::default()).await);
    let cutoff = chrono::Utc::now()
        .checked_sub_months(chrono::Months::new(1))
        .unwrap();
    let fmt = |t: chrono::DateTime<chrono::Utc>| t.format("%Y-%m-%dT%H:%M:%SZ").to_string();
    env.mock.add_mail("inbox", "inside");
    env.mock
        .set_date("inside", &fmt(cutoff + chrono::Duration::minutes(10)));
    env.mock.add_mail("inbox", "outside");
    env.mock
        .set_date("outside", &fmt(cutoff - chrono::Duration::minutes(10)));
    let (key, _) = initial_sync(&env, "ipad", Some("0")).await;
    let r = sync_cmds(&env, "ipad", &key, Some("5"), "25").await;
    assert_eq!(r.soft, vec!["outside"]);
    // And an initial one-month Sync sends only the inside one.
    let (_, adds) = initial_sync(&env, "iphone", Some("5")).await;
    assert!(adds.contains(&"inside".to_owned()) && !adds.contains(&"outside".to_owned()));
}

#[tokio::test]
async fn widening_the_window_brings_soft_deleted_mail_back_as_adds() {
    let env = Arc::new(start_env(Opts::default()).await);
    env.mock.add_mail("inbox", "old");
    env.mock.set_date("old", &days_ago(45));
    let (key, _) = initial_sync(&env, "ipad", Some("0")).await;
    let r = sync_cmds(&env, "ipad", &key, Some("5"), "25").await;
    assert_eq!(r.soft, vec!["old"]);
    // Narrow to one week: m1/m2 are recent, nothing more.
    let r2 = sync_cmds(&env, "ipad", &r.key, Some("3"), "25").await;
    assert!(r2.soft.is_empty(), "{r2:?}");
    // Back to all mail: it returns, once.
    let r3 = sync_cmds(&env, "ipad", &r2.key, Some("0"), "25").await;
    assert_eq!(r3.adds, vec!["old"]);
    let r4 = sync_cmds(&env, "ipad", &r3.key, Some("0"), "25").await;
    assert!(r4.adds.is_empty() && r4.soft.is_empty(), "{r4:?}");
}

#[tokio::test]
async fn iphone_and_ipad_windows_are_reconciled_independently() {
    let env = Arc::new(start_env(Opts::default()).await);
    env.mock.add_mail("inbox", "d10");
    env.mock.set_date("d10", &days_ago(10));
    env.mock.add_mail("inbox", "d40");
    env.mock.set_date("d40", &days_ago(40));
    let (kp, _) = initial_sync(&env, "iphone", Some("0")).await;
    let (ka, _) = initial_sync(&env, "ipad", Some("0")).await;
    // iPhone: one month. iPad: one week.
    let p = sync_cmds(&env, "iphone", &kp, Some("5"), "25").await;
    let a = sync_cmds(&env, "ipad", &ka, Some("3"), "25").await;
    assert_eq!(p.soft, vec!["d40"]);
    let mut a_soft = a.soft.clone();
    a_soft.sort();
    assert_eq!(a_soft, vec!["d10", "d40"]);
    // Neither affects the other's record.
    let p2 = sync_cmds(&env, "iphone", &p.key, Some("5"), "25").await;
    assert!(p2.soft.is_empty() && p2.adds.is_empty(), "{p2:?}");
    let a2 = sync_cmds(&env, "ipad", &a.key, Some("3"), "25").await;
    assert!(a2.soft.is_empty() && a2.adds.is_empty(), "{a2:?}");
}

#[tokio::test]
async fn sync_global_window_is_shared_between_folders() {
    use eas::airsync as air;
    let env = Arc::new(start_env(Opts::default()).await);
    env.mock
        .boxes
        .lock()
        .unwrap()
        .insert("inbox".to_owned(), Vec::new());
    for id in ["inbox-a", "inbox-b"] {
        env.mock.add_mail("inbox", id);
    }
    for id in ["sent-a", "sent-b"] {
        env.mock.add_mail("sent", id);
    }
    let device = "global-window";
    let inbox_key = sync_cmds(&env, device, "0", None, "10").await.key;
    let sent_response = env
        .req("Sync", device)
        .body(sync_body_window("0", "sent", "10"))
        .send()
        .await
        .unwrap();
    let sent_doc = wbxml::decode_document(&sent_response.bytes().await.unwrap()).unwrap();
    let sent_key = eas::find_text_after(&sent_doc, air::SYNC_KEY).unwrap();
    let mut b = eas::DocumentBuilder::new();
    b.start(air::SYNC);
    b.leaf(air::WINDOW_SIZE, "3");
    b.start(air::COLLECTIONS);
    for (id, key) in [("inbox", inbox_key.as_str()), ("sent", sent_key)] {
        b.start(air::COLLECTION);
        b.leaf(air::SYNC_KEY, key);
        b.leaf(air::COLLECTION_ID, id);
        b.leaf(air::WINDOW_SIZE, "10");
        b.end();
    }
    b.end();
    b.end();
    let response = env
        .req("Sync", device)
        .body(wbxml::encode_document(&b.finish()))
        .send()
        .await
        .unwrap();
    let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
    let folders = eas::child_elements(&doc).into_iter()
        .find(|child| matches!(child.nodes.first(), Some(wbxml::Node::Start(t)) if t.code_page == air::PAGE && t.token == air::COLLECTIONS.token))
        .unwrap();
    let replies = eas::child_elements(&folders);
    assert_eq!(
        replies.len(),
        2,
        "{}",
        crate::activesync::wbxml_outline_for_tests(&doc)
    );
    assert_eq!(
        eas::find_all_text_after(&replies[0], air::SERVER_ID).len(),
        2
    );
    assert_eq!(
        eas::find_all_text_after(&replies[1], air::SERVER_ID).len(),
        1
    );
    assert!(eas::contains_token(&replies[1], air::MORE_AVAILABLE));
    let key = eas::find_text_after(&replies[1], air::SYNC_KEY).unwrap();
    let next = env
        .req("Sync", device)
        .body(sync_body_window(key, "sent", "10"))
        .send()
        .await
        .unwrap();
    let next = wbxml::decode_document(&next.bytes().await.unwrap()).unwrap();
    assert_eq!(eas::find_all_text_after(&next, air::SERVER_ID).len(), 1);
}

#[tokio::test]
async fn global_window_leaves_unprocessed_folders_for_the_next_sync() {
    use eas::airsync as air;
    let env = Arc::new(start_env(Opts::default()).await);
    env.mock
        .boxes
        .lock()
        .unwrap()
        .insert("inbox".to_owned(), Vec::new());
    for id in ["first-a", "first-b"] {
        env.mock.add_mail("inbox", id);
    }
    env.mock.add_mail("sent", "later");
    let device = "global-exhausted";
    let inbox_key = sync_cmds(&env, device, "0", None, "10").await.key;
    let sent_response = env
        .req("Sync", device)
        .body(sync_body("0", "sent"))
        .send()
        .await
        .unwrap();
    let sent_doc = wbxml::decode_document(&sent_response.bytes().await.unwrap()).unwrap();
    let sent_key = eas::find_text_after(&sent_doc, air::SYNC_KEY).unwrap();
    let mut b = eas::DocumentBuilder::new();
    b.start(air::SYNC);
    b.leaf(air::WINDOW_SIZE, "2");
    b.start(air::COLLECTIONS);
    for (folder, key) in [("inbox", inbox_key.as_str()), ("sent", sent_key)] {
        b.start(air::COLLECTION);
        b.leaf(air::SYNC_KEY, key);
        b.leaf(air::COLLECTION_ID, folder);
        b.end();
    }
    b.end();
    b.end();
    let response = env
        .req("Sync", device)
        .body(wbxml::encode_document(&b.finish()))
        .send()
        .await
        .unwrap();
    let response = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
    assert_eq!(
        eas::find_all_text_after(&response, air::COLLECTION_ID),
        vec!["inbox"]
    );
    assert_eq!(eas::find_all_text_after(&response, air::SERVER_ID).len(), 2);
    let next = env
        .req("Sync", device)
        .body(sync_body(sent_key, "sent"))
        .send()
        .await
        .unwrap();
    let next = wbxml::decode_document(&next.bytes().await.unwrap()).unwrap();
    assert_eq!(
        eas::find_all_text_after(&next, air::SERVER_ID),
        vec!["later"]
    );
}

#[tokio::test]
async fn mail_changes_share_the_window_and_are_not_lost() {
    let env = Arc::new(start_env(Opts::default()).await);
    for id in ["delete-a", "delete-b", "read-a", "read-b"] {
        env.mock.add_mail("inbox", id);
    }
    let (mut key, _) = initial_sync(&env, "ipad", None).await;
    for id in ["delete-a", "delete-b"] {
        env.mock.remove_mail("inbox", id);
    }
    for id in ["read-a", "read-b"] {
        env.mock.set_read(id, true);
    }
    env.mock.add_mail("inbox", "new-a");
    let mut adds = Vec::new();
    let mut deletes = Vec::new();
    let mut changes = Vec::new();
    for _ in 0..10 {
        let r = sync_cmds(&env, "ipad", &key, None, "2").await;
        assert!(
            r.adds.len() + r.deletes.len() + r.soft.len() + r.changes.len() <= 2,
            "{r:?}"
        );
        adds.extend(r.adds);
        deletes.extend(r.deletes);
        changes.extend(r.changes);
        key = r.key;
        if !r.more {
            break;
        }
    }
    adds.sort();
    deletes.sort();
    changes.sort();
    assert_eq!(adds, ["new-a"]);
    assert_eq!(deletes, ["delete-a", "delete-b"]);
    assert_eq!(changes, ["read-a", "read-b"]);
    let quiet = sync_cmds(&env, "ipad", &key, None, "2").await;
    assert!(
        quiet.adds.is_empty() && quiet.deletes.is_empty() && quiet.changes.is_empty(),
        "{quiet:?}"
    );
}

#[tokio::test]
async fn cleanup_shares_the_window_with_more_available() {
    let env = Arc::new(start_env(Opts::default()).await);
    for n in 0..30 {
        let id = format!("o{n:02}");
        env.mock.add_mail("inbox", &id);
        env.mock.set_date(&id, &days_ago(60));
    }
    let (key, _) = initial_sync(&env, "ipad", Some("0")).await;
    let r = sync_cmds(&env, "ipad", &key, Some("5"), "25").await;
    assert_eq!((r.soft.len(), r.more), (25, true));
    let r2 = sync_cmds(&env, "ipad", &r.key, Some("5"), "25").await;
    assert_eq!((r2.soft.len(), r2.more), (5, false));
    let mut all: Vec<String> = r.soft.iter().chain(&r2.soft).cloned().collect();
    all.sort();
    all.dedup();
    assert_eq!(all.len(), 30, "each exactly once");
}

#[tokio::test]
async fn an_unread_aged_message_is_removed_without_a_read_change_and_its_state_dropped() {
    let env = Arc::new(start_env(Opts::default()).await);
    env.mock.add_mail("inbox", "oldunread");
    env.mock.set_date("oldunread", &days_ago(50));
    let (key, _) = initial_sync(&env, "ipad", Some("0")).await;
    // Read elsewhere at the same time it is outside the window.
    env.mock.set_read("oldunread", true);
    let p = env.ping("ipad", Some(ping_body(2, INBOX))).await;
    let r = sync_cmds(&env, "ipad", &key, Some("5"), "25").await;
    assert_eq!(r.soft, vec!["oldunread"]);
    assert!(
        r.changes.is_empty(),
        "no Change for a removed message: {r:?}"
    );
    // Ping announced the read flip on a held message (Email/changes); the
    // Sync it triggers carries the cleanup, so that Status 2 was deliverable.
    assert_eq!(p.status, "2");
    let states = env
        .state
        .item_states("u@test", "ipad", "inbox")
        .await
        .unwrap();
    assert!(!states.iter().any(|s| s.item_id == "oldunread"));
    let rec = env
        .state
        .get("u@test", "ipad", "inbox")
        .await
        .unwrap()
        .unwrap();
    assert!(!rec.seen_ids.contains(&"oldunread".to_owned()));
}

#[tokio::test]
async fn cleanup_is_not_repeated_after_a_gateway_restart() {
    let env = Arc::new(start_env(Opts::default()).await);
    env.mock.add_mail("inbox", "old");
    env.mock.set_date("old", &days_ago(45));
    let (key, _) = initial_sync(&env, "ipad", Some("0")).await;
    let r = sync_cmds(&env, "ipad", &key, Some("5"), "25").await;
    assert_eq!(r.soft, vec!["old"]);
    let env2 = Arc::new(env.restarted().await);
    let r2 = sync_cmds(&env2, "ipad", &r.key, Some("5"), "25").await;
    assert!(r2.soft.is_empty() && r2.deletes.is_empty(), "{r2:?}");
    assert_eq!(r2.key, r.key);
}

#[tokio::test]
async fn delete_mode_sends_an_ordinary_delete_and_keep_mode_sends_nothing() {
    for (mode, want_soft, want_del) in [
        (crate::config::AgedOut::Delete, false, true),
        (crate::config::AgedOut::Keep, false, false),
        (crate::config::AgedOut::SoftDelete, true, false),
    ] {
        let env = Arc::new(
            start_env(Opts {
                aged_out: mode,
                ..Opts::default()
            })
            .await,
        );
        env.mock.add_mail("inbox", "old");
        env.mock.set_date("old", &days_ago(45));
        let (key, _) = initial_sync(&env, "ipad", Some("0")).await;
        let r = sync_cmds(&env, "ipad", &key, Some("5"), "25").await;
        assert_eq!(r.soft == vec!["old"], want_soft, "{mode:?} {r:?}");
        assert_eq!(r.deletes == vec!["old"], want_del, "{mode:?} {r:?}");
        assert!(still_on_server(&env, "old"));
        let again = sync_cmds(&env, "ipad", &r.key, Some("5"), "25").await;
        assert!(
            again.soft.is_empty() && again.deletes.is_empty(),
            "{mode:?} {again:?}"
        );
    }
}

#[tokio::test]
async fn soft_delete_reply_has_the_spec_shape() {
    let env = Arc::new(start_env(Opts::default()).await);
    env.mock.add_mail("inbox", "old");
    env.mock.set_date("old", &days_ago(45));
    let (key, _) = initial_sync(&env, "ipad", Some("0")).await;
    let (outline, key2) = sync_outline(&env, "ipad", &key).await;
    assert_eq!(
        outline,
        format!(
            "<0:05><0:1c><0:0f><0:0b>{key2}</><0:12>inbox</><0:0e>1</>\
             <0:16><0:21><0:0d>old</></></></></></>"
        )
    );
}

#[tokio::test]
async fn cross_device_delete_is_still_an_ordinary_delete_inside_the_window() {
    let env = Arc::new(start_env(Opts::default()).await);
    let (key, _) = initial_sync(&env, "iphone", Some("5")).await;
    env.mock.remove_mail("inbox", "m1");
    let r = sync_cmds(&env, "iphone", &key, Some("5"), "25").await;
    assert_eq!(
        (r.deletes.clone(), r.soft.clone()),
        (vec!["m1".to_owned()], vec![])
    );
}

async fn item_sync(
    env: &Env,
    device: &str,
    folder: &str,
    key: &str,
    window: &str,
) -> wbxml::Document {
    let response = env
        .req("Sync", device)
        .body(sync_body_window(key, folder, window))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    wbxml::decode_document(&response.bytes().await.unwrap()).unwrap()
}

fn sync_key(doc: &wbxml::Document) -> String {
    eas::find_text_after(doc, eas::airsync::SYNC_KEY)
        .unwrap()
        .to_owned()
}

fn enable_groupware(env: &Env, count: usize) {
    let mut items = env.mock.groupware.lock().unwrap();
    items.insert(
        "contacts".into(),
        (0..count)
            .map(|i| {
                serde_json::json!({
                    "id": format!("c{i:04}"), "addressBookIds": {"book": true},
                    "name": {"full": format!("Contact {i}")}
                })
            })
            .collect(),
    );
    let mut calendar: Vec<_> = (0..count)
        .map(|i| {
            serde_json::json!({
                "id": format!("e{i:04}"), "@type": "Event", "calendarIds": {"calendar": true},
                "title": format!("Event {i}"), "start": "2026-11-13T12:00:00", "duration": "PT1H"
            })
        })
        .collect();
    calendar.extend((0..count).map(|i| {
        serde_json::json!({
            "id": format!("t{i:04}"), "@type": "Task", "calendarIds": {"calendar": true},
            "title": format!("Task {i}"), "due": "2026-11-13T12:00:00", "progress": "needs-action"
        })
    }));
    items.insert("calendar".into(), calendar);
    drop(items);
    env.mock.extra_folders.lock().unwrap().push("notes".into());
    for i in 0..count {
        env.mock.add_mail("notes", &format!("n{i:04}"));
    }
}

#[tokio::test]
async fn item_collections_prime_page_replay_and_resync_without_losing_items() {
    use eas::airsync as air;
    let env = start_env(Opts::default()).await;
    enable_groupware(&env, 215);
    for folder in ["ab_book", "cal_calendar", "task_calendar", "note_notes"] {
        let prime = item_sync(&env, "ipad", folder, "0", "19").await;
        assert_eq!(eas::find_text_after(&prime, air::STATUS), Some("1"));
        assert!(
            !eas::contains_token(&prime, air::COMMANDS),
            "priming must have no Commands"
        );
        let mut key = sync_key(&prime);
        let mut delivered = std::collections::BTreeSet::new();
        for _ in 0..20 {
            let page = item_sync(&env, "ipad", folder, &key, "19").await;
            let ids = eas::find_all_text_after(&page, air::SERVER_ID);
            assert!(ids.len() <= 19);
            for id in ids {
                assert!(
                    delivered.insert(id.clone()),
                    "duplicate item {id} in {folder}"
                );
            }
            let next = sync_key(&page);
            if next != key {
                let replay = item_sync(&env, "ipad", folder, &key, "19").await;
                assert_eq!(
                    wbxml::encode_document(&page),
                    wbxml::encode_document(&replay)
                );
            }
            key = next;
            if !eas::contains_token(&page, air::MORE_AVAILABLE) {
                break;
            }
        }
        assert_eq!(
            delivered.len(),
            215,
            "{folder} must include backend pages beyond 200"
        );
        let quiet = item_sync(&env, "ipad", folder, &key, "19").await;
        assert!(!eas::contains_token(&quiet, air::COMMANDS));
        let reset = item_sync(&env, "ipad", folder, "0", "19").await;
        assert!(!eas::contains_token(&reset, air::COMMANDS));
        let again = item_sync(&env, "ipad", folder, &sync_key(&reset), "19").await;
        assert_eq!(eas::find_all_text_after(&again, air::SERVER_ID).len(), 19);
    }
}

#[tokio::test]
async fn item_changes_wake_both_devices_and_sync_delivers_them() {
    use eas::airsync as air;
    let env = Arc::new(start_env(Opts::default()).await);
    enable_groupware(&env, 1);
    for (folder, class, kind) in [
        ("ab_book", "Contacts", "ContactCard"),
        ("cal_calendar", "Calendar", "CalendarEvent"),
        ("task_calendar", "Tasks", "CalendarEvent"),
        ("note_notes", "Notes", "Email"),
    ] {
        let mut keys = Vec::new();
        for device in ["iphone", "ipad"] {
            let prime = item_sync(&env, device, folder, "0", "25").await;
            let page = item_sync(&env, device, folder, &sync_key(&prime), "25").await;
            keys.push(sync_key(&page));
        }
        let mut pings = Vec::new();
        for device in ["iphone", "ipad"] {
            let env = env.clone();
            pings.push(tokio::spawn(async move {
                env.ping(device, Some(ping_body(4, &[(folder, class)])))
                    .await
            }));
        }
        env.wait_connected(1).await;
        tokio::time::sleep(Duration::from_millis(150)).await;
        if folder == "note_notes" {
            env.mock.add_mail("notes", "new-note");
        } else {
            let mut all = env.mock.groupware.lock().unwrap();
            if folder == "ab_book" {
                all.get_mut("contacts").unwrap()[0]["name"]["full"] = "Changed contact".into();
            } else {
                let index = if folder == "cal_calendar" { 0 } else { 1 };
                all.get_mut("calendar").unwrap()[index]["title"] = "Changed title".into();
            }
        }
        let n = env.mock.counter.fetch_add(1, Ordering::SeqCst);
        env.mock.emit(
            serde_json::json!({"@type": "StateChange", "changed": {"a1": {kind: n.to_string()}}})
                .to_string(),
        );
        for (index, ping) in pings.into_iter().enumerate() {
            let answer = ping.await.unwrap();
            assert_eq!(answer.status, "2", "{folder} must wake");
            assert_eq!(answer.folders, vec![folder]);
            let device = ["iphone", "ipad"][index];
            let page = item_sync(&env, device, folder, &keys[index], "25").await;
            assert_eq!(
                eas::find_all_text_after(&page, air::SERVER_ID).len(),
                1,
                "{folder}: {}",
                crate::activesync::wbxml_outline_for_tests(&page)
            );
            let quiet = item_sync(&env, device, folder, &sync_key(&page), "25").await;
            assert!(!eas::contains_token(&quiet, air::COMMANDS));
        }
    }
}

#[tokio::test]
async fn upload_only_item_sync_does_not_acknowledge_unreceived_changes() {
    use eas::airsync as air;
    let env = start_env(Opts::default()).await;
    enable_groupware(&env, 1);
    for folder in ["ab_book", "cal_calendar", "task_calendar", "note_notes"] {
        let prime = item_sync(&env, "ipad", folder, "0", "25").await;
        let key = sync_key(&prime);
        let mut b = eas::DocumentBuilder::new();
        b.start(air::SYNC);
        b.start(air::COLLECTIONS);
        b.start(air::COLLECTION);
        b.leaf(air::SYNC_KEY, key.clone());
        b.leaf(air::COLLECTION_ID, folder);
        b.leaf(air::GET_CHANGES, "0");
        b.end();
        b.end();
        b.end();
        let request = b.finish();
        let response = env
            .req("Sync", "ipad")
            .body(wbxml::encode_document(&request))
            .send()
            .await
            .unwrap();
        let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
        assert!(!eas::contains_token(&doc, air::COMMANDS));
        let received = item_sync(&env, "ipad", folder, &sync_key(&doc), "25").await;
        assert_eq!(eas::find_all_text_after(&received, air::SERVER_ID).len(), 1);
    }
}

#[tokio::test]
async fn mail_delete_defaults_to_trash_and_explicit_zero_permanently_deletes() {
    use eas::airsync as air;
    let env = start_env(Opts::default()).await;
    env.mock.extra_folders.lock().unwrap().push("trash".into());
    env.sync_all("iphone").await;
    env.sync_all("ipad").await;
    for (id, mode) in [("m1", None), ("m2", Some("0"))] {
        let key = env
            .state
            .get("u@test", "iphone", "inbox")
            .await
            .unwrap()
            .unwrap()
            .sync_key;
        let mut b = eas::DocumentBuilder::new();
        b.start(air::SYNC);
        b.start(air::COLLECTIONS);
        b.start(air::COLLECTION);
        b.leaf(air::SYNC_KEY, key);
        b.leaf(air::COLLECTION_ID, "inbox");
        if let Some(mode) = mode {
            b.leaf(air::DELETES_AS_MOVES, mode);
        }
        b.leaf(air::GET_CHANGES, "0");
        b.start(air::COMMANDS);
        b.start(air::DELETE);
        b.leaf(air::SERVER_ID, id);
        b.end();
        b.end();
        b.end();
        b.end();
        b.end();
        let request = wbxml::encode_document(&b.finish());
        let response = env
            .req("Sync", "iphone")
            .body(request.clone())
            .send()
            .await
            .unwrap();
        let bytes = response.bytes().await.unwrap();
        let replay = env
            .req("Sync", "iphone")
            .body(request)
            .send()
            .await
            .unwrap();
        assert_eq!(replay.bytes().await.unwrap(), bytes);
        let doc = wbxml::decode_document(&bytes).unwrap();
        assert_eq!(eas::find_text_after(&doc, air::STATUS), Some("1"));
    }
    let boxes = env.mock.boxes.lock().unwrap().clone();
    assert!(!boxes["inbox"].contains(&"m1".into()));
    assert_eq!(boxes["trash"], vec!["m1"]);
    assert!(!boxes.values().any(|items| items.contains(&"m2".into())));
    let key = env
        .state
        .get("u@test", "ipad", "inbox")
        .await
        .unwrap()
        .unwrap()
        .sync_key;
    let page = item_sync(&env, "ipad", "inbox", &key, "25").await;
    assert!(eas::contains_token(&page, air::DELETE));
    assert_eq!(eas::find_all_text_after(&page, air::SERVER_ID).len(), 2);
}

#[tokio::test]
async fn user_query_cannot_select_another_accounts_sync_state() {
    let env = start_env(Opts::default()).await;
    let response = env
        .http
        .post(format!(
            "{}?Cmd=Sync&User=victim&DeviceId=iphone&DeviceType=iPhone",
            env.gw
        ))
        .basic_auth("u@test", Some("pw"))
        .body(sync_body("0", "inbox"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert!(env
        .state
        .get("victim", "iphone", "inbox")
        .await
        .unwrap()
        .is_none());
    assert!(env
        .state
        .get("u@test", "iphone", "inbox")
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn a_changing_backend_snapshot_does_not_delete_held_contacts() {
    use eas::airsync as air;
    let env = start_env(Opts::default()).await;
    enable_groupware(&env, 80);
    let prime = item_sync(&env, "ipad", "ab_book", "0", "100").await;
    let page = item_sync(&env, "ipad", "ab_book", &sync_key(&prime), "100").await;
    let key = sync_key(&page);
    let previous = env
        .state
        .item_states("u@test", "ipad", "ab_book")
        .await
        .unwrap();
    assert_eq!(previous.len(), 80);
    env.mock.snapshot_changes.store(true, Ordering::SeqCst);
    let failed = item_sync(&env, "ipad", "ab_book", &key, "100").await;
    assert_eq!(eas::find_text_after(&failed, air::STATUS), Some("5"));
    assert_eq!(sync_key(&failed), key);
    assert!(!eas::contains_token(&failed, air::DELETE));
    assert_eq!(
        env.state
            .item_states("u@test", "ipad", "ab_book")
            .await
            .unwrap(),
        previous
    );
}

#[tokio::test]
async fn missing_fetched_contact_does_not_delete_held_contacts() {
    use eas::airsync as air;
    let env = start_env(Opts::default()).await;
    enable_groupware(&env, 3);
    let prime = item_sync(&env, "ipad", "ab_book", "0", "100").await;
    let page = item_sync(&env, "ipad", "ab_book", &sync_key(&prime), "100").await;
    let key = sync_key(&page);
    let previous = env
        .state
        .item_states("u@test", "ipad", "ab_book")
        .await
        .unwrap();
    assert_eq!(previous.len(), 3);
    env.mock.snapshot_missing_item.store(true, Ordering::SeqCst);
    let failed = item_sync(&env, "ipad", "ab_book", &key, "100").await;
    assert_eq!(eas::find_text_after(&failed, air::STATUS), Some("5"));
    assert_eq!(sync_key(&failed), key);
    assert!(!eas::contains_token(&failed, air::DELETE));
    assert_eq!(
        env.state
            .item_states("u@test", "ipad", "ab_book")
            .await
            .unwrap(),
        previous
    );
}

#[tokio::test]
async fn item_estimates_count_pending_downloads_instead_of_always_zero() {
    use eas::{airsync as air, get_item_estimate as gie};
    let env = start_env(Opts::default()).await;
    enable_groupware(&env, 3);
    for folder in ["ab_book", "cal_calendar", "task_calendar", "note_notes"] {
        let prime = item_sync(&env, "ipad", folder, "0", "1").await;
        let key = sync_key(&prime);
        let mut b = eas::DocumentBuilder::new();
        b.start(gie::GET_ITEM_ESTIMATE);
        b.start(gie::FOLDERS);
        b.start(gie::FOLDER);
        b.leaf(air::SYNC_KEY, &key);
        b.leaf(gie::FOLDER_ID, folder);
        b.end();
        b.end();
        b.end();
        let response = env
            .req("GetItemEstimate", "ipad")
            .body(wbxml::encode_document(&b.finish()))
            .send()
            .await
            .unwrap();
        let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
        assert_eq!(eas::find_text_after(&doc, gie::ESTIMATE), Some("3"));
    }
}

#[tokio::test]
async fn item_operations_fetches_all_entries_in_order_and_isolates_failures() {
    use base64::Engine;
    use eas::{airsync as air, airsync_base as base, item_operations as ops};
    let env = start_env(Opts::default()).await;
    let mut b = eas::DocumentBuilder::new();
    b.start(ops::ITEM_OPERATIONS);
    b.start(ops::FETCH);
    b.leaf(ops::STORE, "Mailbox");
    b.leaf(base::FILE_REFERENCE, "binary||test.dat");
    b.start(ops::OPTIONS);
    b.leaf(ops::RANGE, "1-3");
    b.end();
    b.end();
    b.start(ops::FETCH);
    b.leaf(ops::STORE, "Mailbox");
    b.leaf(air::COLLECTION_ID, "inbox");
    b.leaf(air::SERVER_ID, "missing");
    b.end();
    b.start(ops::FETCH);
    b.leaf(ops::STORE, "Mailbox");
    b.leaf(air::COLLECTION_ID, "inbox");
    b.leaf(air::SERVER_ID, "m1");
    b.start(ops::OPTIONS);
    b.start(base::BODY_PREFERENCE);
    b.leaf(base::TYPE, "1");
    b.leaf(base::TRUNCATION_SIZE, "2");
    b.end();
    b.end();
    b.end();
    b.start(ops::FETCH);
    b.leaf(ops::STORE, "Mailbox");
    b.leaf(base::FILE_REFERENCE, "binary||test.dat");
    b.start(ops::OPTIONS);
    b.leaf(ops::RANGE, "9-1");
    b.end();
    b.end();
    b.start(ops::FETCH);
    b.leaf(ops::STORE, "Other");
    b.leaf(base::FILE_REFERENCE, "binary||test.dat");
    b.end();
    b.start(ops::FETCH);
    b.leaf(ops::STORE, "Mailbox");
    b.leaf(base::FILE_REFERENCE, "missing||test.dat");
    b.end();
    b.end();
    let request = wbxml::encode_document(&b.finish());
    for version in ["14.0", "16.1"] {
        let response = env
            .req("ItemOperations", "client")
            .header("MS-ASProtocolVersion", version)
            .body(request.clone())
            .send()
            .await
            .unwrap();
        let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
        assert_eq!(
            eas::find_all_text_after(&doc, ops::STATUS),
            vec!["1", "1", "6", "1", "8", "9", "15"]
        );
        let root = eas::child_elements(&doc);
        let fetches = eas::child_elements(&root[1]);
        assert_eq!(fetches.len(), 6);
        let data = eas::find_text_after(&fetches[0], ops::DATA).unwrap();
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(data)
                .unwrap(),
            BLOB_BYTES[1..4]
        );
        assert_eq!(eas::find_text_after(&fetches[0], ops::RANGE), Some("1-3"));
        assert_eq!(
            eas::find_text_after(&fetches[0], ops::TOTAL),
            Some(BLOB_BYTES.len().to_string()).as_deref()
        );
        assert_eq!(eas::find_text_after(&fetches[2], base::DATA), Some("bo"));
        assert_eq!(
            eas::find_text_after(&fetches[2], base::TRUNCATED),
            Some("1")
        );
    }
}

#[tokio::test]
async fn item_operations_mime_preserves_non_utf8_bytes() {
    use eas::{airsync as air, airsync_base as base, item_operations as ops};
    let env = start_env(Opts::default()).await;
    let mut b = eas::DocumentBuilder::new();
    b.start(ops::ITEM_OPERATIONS);
    b.start(ops::FETCH);
    b.leaf(ops::STORE, "Mailbox");
    b.leaf(air::COLLECTION_ID, "inbox");
    b.leaf(air::SERVER_ID, "m1");
    b.start(ops::OPTIONS);
    b.leaf(air::MIME_SUPPORT, "2");
    b.start(base::BODY_PREFERENCE);
    b.leaf(base::TYPE, "4");
    b.end();
    b.end();
    b.end();
    b.end();
    let response = env
        .req("ItemOperations", "client")
        .header("MS-ASProtocolVersion", "16.1")
        .body(wbxml::encode_document(&b.finish()))
        .send()
        .await
        .unwrap();
    let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
    assert_eq!(eas::find_all_text_after(&doc, ops::STATUS), vec!["1", "1"]);
    assert_eq!(eas::find_text_after(&doc, base::TYPE), Some("4"));
    assert_eq!(body_data_bytes(&doc), Some(MIME_BYTES));
}

/// Local mock-backed endpoint for independent EAS clients. EAS_INTEROP_FILE
/// receives its URL; this does not connect to a production mailbox.
#[tokio::test]
#[ignore]
async fn client_interop_fixture() {
    let env = start_env(Opts::default()).await;
    env.mock
        .extra_folders
        .lock()
        .unwrap()
        .push("trash".to_owned());
    let path = std::env::var("EAS_INTEROP_FILE").expect("set EAS_INTEROP_FILE");
    std::fs::write(path, serde_json::json!({"gateway": env.gw}).to_string()).unwrap();
    tokio::time::sleep(Duration::from_secs(120)).await;
}

#[tokio::test]
async fn user_information_uses_versioned_accounts_and_backend_addresses() {
    use eas::settings as set;
    let env = start_env(Opts::default()).await;
    let mut b = eas::DocumentBuilder::new();
    b.start(set::SETTINGS);
    b.start(set::USER_INFORMATION);
    b.empty_tag(set::GET);
    b.end();
    b.end();
    let body = wbxml::encode_document(&b.finish());
    for version in ["14.0", "14.1", "16.0", "16.1"] {
        for login in ["owner", "owner@example.test"] {
            let response = env
                .http
                .post(format!("{}?Cmd=Settings&DeviceId=client", env.gw))
                .basic_auth(login, Some("pw"))
                .header("MS-ASProtocolVersion", version)
                .body(body.clone())
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 200);
            let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
            assert_eq!(eas::find_all_text_after(&doc, set::STATUS), vec!["1", "1"]);
            assert_eq!(
                eas::find_all_text_after(&doc, set::SMTP_ADDRESS),
                vec!["owner@example.test", "alias@example.test"]
            );
            let user = &eas::child_elements(&doc)[1];
            let get = &eas::child_elements(user)[1];
            let children = eas::child_elements(get);
            let modern = version != "14.0";
            assert_eq!(eas::contains_token(&doc, set::ACCOUNTS), modern);
            assert!(!eas::contains_token(&doc, set::ACCOUNT_ID));
            if modern {
                assert_eq!(
                    children[0].nodes[0],
                    wbxml::Node::Start(wbxml::token::Token {
                        has_content: true,
                        ..set::ACCOUNTS
                    })
                );
                let accounts = eas::child_elements(&children[0]);
                assert_eq!(accounts.len(), 1);
                assert!(eas::contains_token(&accounts[0], set::EMAIL_ADDRESSES));
                assert_eq!(
                    eas::find_text_after(&doc, set::PRIMARY_SMTP_ADDRESS),
                    if login.contains('@') {
                        Some("owner@example.test")
                    } else {
                        None
                    }
                );
            } else {
                assert!(eas::contains_token(&children[0], set::SMTP_ADDRESS));
                assert!(!eas::contains_token(&doc, set::PRIMARY_SMTP_ADDRESS));
            }
        }
    }
    env.mock.identity_error.store(true, Ordering::SeqCst);
    let response = env
        .req("Settings", "client")
        .header("MS-ASProtocolVersion", "16.1")
        .body(body.clone())
        .send()
        .await
        .unwrap();
    let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
    assert_eq!(
        eas::find_all_text_after(&doc, set::STATUS),
        vec!["1", "110"]
    );
    assert!(!eas::contains_token(&doc, set::SMTP_ADDRESS));
    env.mock.identity_error.store(false, Ordering::SeqCst);
    env.mock.identities.lock().unwrap().clear();
    let response = env
        .req("Settings", "client")
        .body(body)
        .send()
        .await
        .unwrap();
    let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
    assert_eq!(
        eas::find_all_text_after(&doc, set::STATUS),
        vec!["1", "110"]
    );
    assert!(!eas::contains_token(&doc, set::SMTP_ADDRESS));
}

#[tokio::test]
async fn flag_update_is_not_acknowledged_when_the_server_hides_metadata() {
    use eas::airsync as air;
    let env = Arc::new(start_env(Opts::default()).await);
    let (key, _) = initial_sync(&env, "flag-verification", None).await;
    env.mock.drop_flag_keywords.store(true, Ordering::SeqCst);
    let body = flag_change_body(&key, "inbox", "m1", Some("2"), None, false);
    let response = env
        .req("Sync", "flag-verification")
        .body(body.clone())
        .send()
        .await
        .unwrap();
    let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
    assert_eq!(eas::find_text_after(&doc, air::STATUS), Some("5"));
    assert_eq!(
        eas::find_text_after(&doc, air::SYNC_KEY),
        Some(key.as_str())
    );
    env.mock.drop_flag_keywords.store(false, Ordering::SeqCst);
    let response = env
        .req("Sync", "flag-verification")
        .body(body)
        .send()
        .await
        .unwrap();
    let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
    assert_eq!(eas::find_text_after(&doc, air::STATUS), Some("1"));
    assert_ne!(
        eas::find_text_after(&doc, air::SYNC_KEY),
        Some(key.as_str())
    );
}

#[tokio::test]
async fn item_operations_multipart_directory_and_binary_parts() {
    use eas::{airsync as air, airsync_base as base, item_operations as ops};
    let env = start_env(Opts::default()).await;
    let mut b = eas::DocumentBuilder::new();
    b.start(ops::ITEM_OPERATIONS);
    b.start(ops::FETCH);
    b.leaf(ops::STORE, "Mailbox");
    b.leaf(base::FILE_REFERENCE, "binary||test.dat");
    b.start(ops::OPTIONS);
    b.leaf(ops::RANGE, "1-3");
    b.end();
    b.end();
    b.start(ops::FETCH);
    b.leaf(ops::STORE, "Mailbox");
    b.leaf(base::FILE_REFERENCE, "missing||test.dat");
    b.end();
    b.start(ops::FETCH);
    b.leaf(ops::STORE, "Mailbox");
    b.leaf(air::COLLECTION_ID, "inbox");
    b.leaf(air::SERVER_ID, "m1");
    b.start(ops::OPTIONS);
    b.leaf(air::MIME_SUPPORT, "2");
    b.start(base::BODY_PREFERENCE);
    b.leaf(base::TYPE, "4");
    b.end();
    b.end();
    b.end();
    b.end();
    let request = wbxml::encode_document(&b.finish());
    for version in ["14.0", "16.1"] {
        let response = env
            .req("ItemOperations", "client")
            .header("MS-ASProtocolVersion", version)
            .header("MS-ASAcceptMultiPart", "T")
            .body(request.clone())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(
            response.headers()["Content-Type"],
            "application/vnd.ms-sync.multipart"
        );
        let bytes = response.bytes().await.unwrap();
        let read_u32 =
            |start: usize| u32::from_le_bytes(bytes[start..start + 4].try_into().unwrap()) as usize;
        let count = read_u32(0);
        assert_eq!(count, 3);
        let mut offset = 4 + count * 8;
        let mut parts = Vec::new();
        for part in 0..count {
            let start = read_u32(4 + part * 8);
            let len = read_u32(8 + part * 8);
            assert_eq!(start, offset);
            parts.push(&bytes[start..start + len]);
            offset += len;
        }
        assert_eq!(offset, bytes.len());
        let doc = wbxml::decode_document(parts[0]).unwrap();
        assert_eq!(
            eas::find_all_text_after(&doc, ops::STATUS),
            vec!["1", "1", "15", "1"]
        );
        assert_eq!(eas::find_all_text_after(&doc, ops::PART), vec!["1", "2"]);
        assert!(!eas::contains_token(&doc, ops::DATA));
        assert!(!eas::contains_token(&doc, base::DATA));
        assert_eq!(parts[1], &BLOB_BYTES[1..4]);
        assert_eq!(parts[2], MIME_BYTES);
        let fetches = eas::child_elements(&eas::child_elements(&doc)[1]);
        assert!(eas::contains_token(&fetches[0], ops::PART));
        let properties = eas::child_elements(&fetches[2]);
        let properties = properties.last().unwrap();
        let body = eas::child_elements(properties)
            .into_iter()
            .find(|node| {
                matches!(node.nodes.first(), Some(wbxml::Node::Start(t))
                if t.code_page == base::PAGE && t.token == base::BODY.token)
            })
            .unwrap();
        assert_eq!(eas::find_text_after(&body, ops::PART), Some("2"));
    }
}

#[tokio::test]
async fn provision_reports_no_policy_without_inventing_a_policy_key() {
    use eas::{provision as prov, settings as set};
    let env = start_env(Opts::default()).await;
    for version in ["14.0", "16.1"] {
        for (kind, expected) in [
            (Some("MS-EAS-Provisioning-WBXML"), "2"),
            (Some("unknown"), "3"),
            (None, "2"),
        ] {
            let mut b = eas::DocumentBuilder::new();
            b.start(prov::PROVISION);
            b.start(set::DEVICE_INFORMATION);
            b.empty_tag(set::SET);
            b.end();
            b.start(prov::POLICIES);
            b.start(prov::POLICY);
            if let Some(kind) = kind {
                b.leaf(prov::POLICY_TYPE, kind);
            }
            b.end();
            b.end();
            b.end();
            let response = env
                .req("Provision", "client")
                .header("MS-ASProtocolVersion", version)
                .body(wbxml::encode_document(&b.finish()))
                .send()
                .await
                .unwrap();
            let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
            assert_eq!(
                eas::find_all_text_after(&doc, prov::STATUS),
                if kind.is_some() {
                    vec!["1", expected]
                } else {
                    vec!["2"]
                }
            );
            assert!(!eas::contains_token(&doc, prov::POLICY_KEY));
            assert_eq!(
                eas::contains_token(&doc, set::DEVICE_INFORMATION),
                kind.is_some()
            );
        }
        let mut b = eas::DocumentBuilder::new();
        b.empty_tag(prov::PROVISION);
        let response = env
            .req("Provision", "client")
            .header("MS-ASProtocolVersion", version)
            .body(wbxml::encode_document(&b.finish()))
            .send()
            .await
            .unwrap();
        let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
        assert_eq!(eas::find_all_text_after(&doc, prov::STATUS), vec!["1"]);
        assert!(!eas::contains_token(&doc, prov::POLICIES));
    }
}

#[tokio::test]
async fn autodiscover_accepts_standard_lowercase_and_existing_paths() {
    let env = start_env(Opts::default()).await;
    let base = env.gw.strip_suffix("/Microsoft-Server-ActiveSync").unwrap();
    let request = r#"<?xml version="1.0"?>
<Autodiscover xmlns="http://schemas.microsoft.com/exchange/autodiscover/mobilesync/requestschema/2006">
<Request><EMailAddress>u@test</EMailAddress><AcceptableResponseSchema>http://schemas.microsoft.com/exchange/autodiscover/mobilesync/responseschema/2006</AcceptableResponseSchema></Request>
</Autodiscover>"#;
    for path in [
        "/autodiscover/autodiscover.xml",
        "/Autodiscover/Autodiscover.xml",
    ] {
        let response = env
            .http
            .post(format!("{base}{path}"))
            .header("Content-Type", "text/xml")
            .body(request)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "Autodiscover POST {path}");
        assert_eq!(
            response.headers()["Content-Type"],
            "text/xml; charset=utf-8"
        );
        let body = response.text().await.unwrap();
        assert!(body.contains("<Type>MobileSync</Type>"));
        assert!(body.contains("<Url>http://localhost/Microsoft-Server-ActiveSync</Url>"));
    }
}

#[tokio::test]
async fn mail_draft_marker_follows_requested_protocol_in_sync_and_fetch() {
    use eas::{airsync as air, email2, item_operations as ops};
    let env = start_env(Opts::default()).await;
    env.mock.set_keyword("m1", "$draft", true);
    for version in ["14.0", "14.1", "16.0", "16.1"] {
        let device = format!("version-{version}");
        let prime = env
            .req("Sync", &device)
            .header("MS-ASProtocolVersion", version)
            .body(sync_body("0", "inbox"))
            .send()
            .await
            .unwrap();
        let prime = wbxml::decode_document(&prime.bytes().await.unwrap()).unwrap();
        let response = env
            .req("Sync", &device)
            .header("MS-ASProtocolVersion", version)
            .body(sync_body(&sync_key(&prime), "inbox"))
            .send()
            .await
            .unwrap();
        let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
        assert_eq!(eas::find_all_text_after(&doc, air::SERVER_ID).len(), 2);
        let markers = eas::find_all_text_after(&doc, email2::IS_DRAFT);
        if version.starts_with("16") {
            assert_eq!(markers.len(), 2);
            assert!(markers.contains(&"1".to_owned()));
            assert!(markers.contains(&"0".to_owned()));
        } else {
            assert!(markers.is_empty());
        }
        for (id, draft) in [("m1", "1"), ("m2", "0")] {
            let mut b = eas::DocumentBuilder::new();
            b.start(ops::ITEM_OPERATIONS);
            b.start(ops::FETCH);
            b.leaf(ops::STORE, "Mailbox");
            b.leaf(air::COLLECTION_ID, "inbox");
            b.leaf(air::SERVER_ID, id);
            b.end();
            b.end();
            let response = env
                .req("ItemOperations", &device)
                .header("MS-ASProtocolVersion", version)
                .body(wbxml::encode_document(&b.finish()))
                .send()
                .await
                .unwrap();
            let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
            assert_eq!(
                eas::find_text_after(&doc, email2::IS_DRAFT),
                if version.starts_with("16") {
                    Some(draft)
                } else {
                    None
                }
            );
        }
    }
}

fn mixed_flag_request(key: &str, folder: &str, bad: &str, good: &str) -> Vec<u8> {
    use eas::{airsync as air, email as mail};
    let mut b = eas::DocumentBuilder::new();
    b.start(air::SYNC);
    b.start(air::COLLECTIONS);
    b.start(air::COLLECTION);
    b.leaf(air::SYNC_KEY, key);
    b.leaf(air::COLLECTION_ID, folder);
    b.leaf(air::GET_CHANGES, "0");
    b.start(air::COMMANDS);
    b.start(air::CHANGE);
    b.leaf(air::SERVER_ID, bad);
    b.start(air::APPLICATION_DATA);
    b.leaf(mail::READ, "1");
    b.start(mail::FLAG);
    b.leaf(mail::FLAG_STATUS, "2");
    b.leaf(mail::FLAG_TYPE, "Flag for follow up");
    b.end();
    b.end();
    b.end();
    b.start(air::CHANGE);
    b.leaf(air::SERVER_ID, good);
    b.start(air::APPLICATION_DATA);
    b.leaf(mail::READ, "1");
    b.end();
    b.end();
    b.start(air::FETCH);
    b.leaf(air::SERVER_ID, bad);
    b.end();
    for _ in 0..4 {
        b.end();
    }
    wbxml::encode_document(&b.finish())
}

fn flag_metadata_body(key: &str, folder: &str, id: &str) -> Vec<u8> {
    use eas::{airsync as air, email as mail, tasks};
    let mut b = eas::DocumentBuilder::new();
    b.start(air::SYNC);
    b.start(air::COLLECTIONS);
    b.start(air::COLLECTION);
    b.leaf(air::SYNC_KEY, key);
    b.leaf(air::COLLECTION_ID, folder);
    b.leaf(air::GET_CHANGES, "0");
    b.start(air::COMMANDS);
    b.start(air::CHANGE);
    b.leaf(air::SERVER_ID, id);
    b.start(air::APPLICATION_DATA);
    b.start(mail::FLAG);
    b.leaf(tasks::REMINDER_SET, "1");
    b.leaf(tasks::REMINDER_TIME, "2026-11-15T10:00:00.000Z");
    for _ in 0..7 {
        b.end();
    }
    wbxml::encode_document(&b.finish())
}

fn complete_flag_body(key: &str, folder: &str, id: &str) -> Vec<u8> {
    use eas::{airsync as air, email as mail, tasks};
    let mut b = eas::DocumentBuilder::new();
    b.start(air::SYNC);
    b.start(air::COLLECTIONS);
    b.start(air::COLLECTION);
    b.leaf(air::SYNC_KEY, key);
    b.leaf(air::COLLECTION_ID, folder);
    b.leaf(air::GET_CHANGES, "0");
    b.start(air::COMMANDS);
    b.start(air::CHANGE);
    b.leaf(air::SERVER_ID, id);
    b.start(air::APPLICATION_DATA);
    b.start(mail::FLAG);
    b.leaf(mail::FLAG_STATUS, "1");
    b.leaf(mail::COMPLETE_TIME, "2026-11-15T11:30:00.000Z");
    b.leaf(tasks::DATE_COMPLETED, "2026-11-15T11:31:00.000Z");
    for _ in 0..7 {
        b.end();
    }
    wbxml::encode_document(&b.finish())
}

#[tokio::test]
async fn partial_flag_metadata_preserves_other_fields() {
    use eas::{airsync as air, email as mail, tasks};
    let env = start_env(Opts::default()).await;
    let (a0, _, _) = env.sync_detail("iphone", "0").await;
    let (a, _, _) = env.sync_detail("iphone", &a0).await;
    let (b0, _, _) = env.sync_detail("ipad", "0").await;
    let (b, _, _) = env.sync_detail("ipad", &b0).await;
    let response = env
        .req("Sync", "iphone")
        .body(flag_change_body(&a, "inbox", "m1", Some("2"), None, false))
        .send()
        .await
        .unwrap();
    let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
    let response = env
        .req("Sync", "iphone")
        .body(flag_metadata_body(&sync_key(&doc), "inbox", "m1"))
        .send()
        .await
        .unwrap();
    let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
    assert_eq!(eas::find_text_after(&doc, air::STATUS), Some("1"));
    let response = env
        .req("Sync", "ipad")
        .body(sync_body(&b, "inbox"))
        .send()
        .await
        .unwrap();
    let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
    assert_eq!(eas::find_text_after(&doc, mail::FLAG_STATUS), Some("2"));
    assert_eq!(
        eas::find_text_after(&doc, tasks::DUE_DATE),
        Some("2026-11-14T12:00:00.000Z")
    );
    assert_eq!(
        eas::find_text_after(&doc, tasks::REMINDER_TIME),
        Some("2026-11-15T10:00:00.000Z")
    );
}

const DRAFT_ATTACHMENT_BYTES: &[u8] = b"file bytes\0\xff";
const DRAFT_EMBEDDED_BYTES: &[u8] =
    b"From: sender@example.test\r\nSubject: Embedded\r\n\r\nMessage body\r\n";

fn attachment_draft_add_body(key: &str, folder: &str) -> Vec<u8> {
    use eas::{airsync as air, airsync_base as base, email};
    let mut b = eas::DocumentBuilder::new();
    b.start(air::SYNC);
    b.start(air::COLLECTIONS);
    b.start(air::COLLECTION);
    b.leaf(air::SYNC_KEY, key);
    b.leaf(air::COLLECTION_ID, folder);
    b.leaf(air::GET_CHANGES, "0");
    b.start(air::COMMANDS);
    b.start(air::ADD);
    b.leaf(air::CLIENT_ID, "draft-attachments");
    b.start(air::APPLICATION_DATA);
    b.leaf(email::SUBJECT, "Draft attachment fixture");
    b.start(base::BODY);
    b.leaf(base::TYPE, "2");
    b.leaf(base::DATA, "<p>Draft</p>");
    b.end();
    b.start(base::ATTACHMENTS);
    b.start(base::ADD);
    b.leaf(base::CLIENT_ID, "file-client");
    b.leaf(base::METHOD, "1");
    b.leaf(base::CONTENT_TYPE, "application/octet-stream");
    b.opaque_leaf(base::CONTENT, DRAFT_ATTACHMENT_BYTES.to_vec());
    b.leaf(base::DISPLAY_NAME, "image.bin");
    b.leaf(base::CONTENT_ID, "inline@example.test");
    b.leaf(base::CONTENT_LOCATION, "https://example.test/image");
    b.empty_tag(base::IS_INLINE);
    b.end();
    b.start(base::ADD);
    b.leaf(base::CLIENT_ID, "message-client");
    b.leaf(base::METHOD, "5");
    b.leaf(base::CONTENT_TYPE, "message/rfc822");
    b.opaque_leaf(base::CONTENT, DRAFT_EMBEDDED_BYTES.to_vec());
    b.leaf(base::DISPLAY_NAME, "message.eml");
    b.end();
    for (client_id, data) in [
        ("duplicate-a", b"same1".as_slice()),
        ("duplicate-b", b"same2".as_slice()),
    ] {
        b.start(base::ADD);
        b.leaf(base::CLIENT_ID, client_id);
        b.leaf(base::METHOD, "1");
        b.leaf(base::CONTENT_TYPE, "application/octet-stream");
        b.opaque_leaf(base::CONTENT, data.to_vec());
        b.leaf(base::DISPLAY_NAME, "same.bin");
        b.end();
    }
    b.end();
    b.end();
    for _ in 0..5 {
        b.end();
    }
    wbxml::encode_document(&b.finish())
}

fn mime_draft_add_body(key: &str, folder: &str, content: &[u8]) -> Vec<u8> {
    use eas::{airsync as air, airsync_base as base};
    let mut b = eas::DocumentBuilder::new();
    b.start(air::SYNC);
    b.start(air::COLLECTIONS);
    b.start(air::COLLECTION);
    b.leaf(air::SYNC_KEY, key);
    b.leaf(air::COLLECTION_ID, folder);
    b.leaf(air::GET_CHANGES, "0");
    b.start(air::COMMANDS);
    b.start(air::ADD);
    b.leaf(air::CLIENT_ID, "mime-draft");
    b.start(air::APPLICATION_DATA);
    b.start(base::BODY);
    b.leaf(base::TYPE, "4");
    b.opaque_leaf(base::DATA, content.to_vec());
    b.end();
    b.end();
    for _ in 0..5 {
        b.end();
    }
    wbxml::encode_document(&b.finish())
}

fn draft_add_body(key: &str, folder: &str, client_id: &str, get_changes: bool) -> Vec<u8> {
    use eas::{airsync as air, airsync_base as base, email, email2};
    let mut b = eas::DocumentBuilder::new();
    b.start(air::SYNC);
    b.start(air::COLLECTIONS);
    b.start(air::COLLECTION);
    b.leaf(air::SYNC_KEY, key);
    b.leaf(air::COLLECTION_ID, folder);
    b.leaf(air::GET_CHANGES, if get_changes { "1" } else { "0" });
    b.start(air::COMMANDS);
    b.start(air::ADD);
    b.leaf(air::CLIENT_ID, client_id);
    b.start(air::APPLICATION_DATA);
    b.leaf(email::TO, r#""Recipient" <recipient@example.test>"#);
    b.leaf(email2::BCC, r#""Hidden" <hidden@example.test>"#);
    b.leaf(email::REPLY_TO, r#""Reply" <reply@example.test>"#);
    b.leaf(email::IMPORTANCE, "2");
    b.leaf(email::SUBJECT, "Draft creation fixture");
    b.start(base::BODY);
    b.leaf(base::TYPE, "1");
    b.leaf(base::DATA, "Draft contents");
    b.end();
    b.leaf(email::READ, "1");
    b.end();
    for _ in 0..5 {
        b.end();
    }
    wbxml::encode_document(&b.finish())
}

fn unsupported_mail_change_body(key: &str, folder: &str, id: &str) -> Vec<u8> {
    use eas::{airsync as air, email as mail};
    let mut b = eas::DocumentBuilder::new();
    b.start(air::SYNC);
    b.start(air::COLLECTIONS);
    b.start(air::COLLECTION);
    b.leaf(air::SYNC_KEY, key);
    b.leaf(air::COLLECTION_ID, folder);
    b.leaf(air::GET_CHANGES, "0");
    b.start(air::COMMANDS);
    b.start(air::CHANGE);
    b.leaf(air::SERVER_ID, id);
    b.start(air::APPLICATION_DATA);
    b.leaf(mail::SUBJECT, "Edited draft");
    b.leaf(mail::READ, "1");
    for _ in 0..6 {
        b.end();
    }
    wbxml::encode_document(&b.finish())
}

#[tokio::test]
async fn unsupported_mail_content_change_does_not_acknowledge_or_apply_read_state() {
    use eas::airsync as air;
    let env = start_env(Opts::default()).await;
    let (prime, _, _) = env.sync_detail("client", "0").await;
    let (key, _, _) = env.sync_detail("client", &prime).await;
    let response = env
        .req("Sync", "client")
        .body(unsupported_mail_change_body(&key, "inbox", "m1"))
        .send()
        .await
        .unwrap();
    let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
    assert!(eas::find_all_text_after(&doc, air::STATUS).contains(&"6".to_owned()));
    assert!(env
        .mock
        .keywords
        .lock()
        .unwrap()
        .get("m1")
        .is_none_or(|keywords| !keywords.contains("$seen")));
}

#[tokio::test]
async fn unsupported_sync_send_is_rejected() {
    use eas::{airsync as air, email2};
    let env = start_env(Opts::default()).await;
    let (prime, _, _) = env.sync_detail("client", "0").await;
    let (key, _, _) = env.sync_detail("client", &prime).await;
    let mut b = eas::DocumentBuilder::new();
    b.start(air::SYNC);
    b.start(air::COLLECTIONS);
    b.start(air::COLLECTION);
    b.leaf(air::SYNC_KEY, &key);
    b.leaf(air::COLLECTION_ID, "inbox");
    b.leaf(air::GET_CHANGES, "0");
    b.start(air::COMMANDS);
    b.start(air::CHANGE);
    b.leaf(air::SERVER_ID, "m1");
    b.empty_tag(email2::SEND);
    for _ in 0..5 {
        b.end();
    }
    let response = env
        .req("Sync", "client")
        .body(wbxml::encode_document(&b.finish()))
        .send()
        .await
        .unwrap();
    let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
    assert!(eas::find_all_text_after(&doc, air::STATUS).contains(&"6".to_owned()));
}

#[tokio::test]
async fn complete_flag_syncs_between_devices() {
    use eas::{airsync as air, email as mail, tasks};
    let env = start_env(Opts::default()).await;
    let (a0, _, _) = env.sync_detail("iphone", "0").await;
    let (a, _, _) = env.sync_detail("iphone", &a0).await;
    let (b0, _, _) = env.sync_detail("ipad", "0").await;
    let (b, _, _) = env.sync_detail("ipad", &b0).await;
    let response = env
        .req("Sync", "iphone")
        .body(complete_flag_body(&a, "inbox", "m1"))
        .send()
        .await
        .unwrap();
    let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
    assert_eq!(eas::find_text_after(&doc, air::STATUS), Some("1"));
    let response = env
        .req("Sync", "ipad")
        .body(sync_body(&b, "inbox"))
        .send()
        .await
        .unwrap();
    let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
    assert_eq!(eas::find_text_after(&doc, mail::FLAG_STATUS), Some("1"));
    assert_eq!(
        eas::find_text_after(&doc, mail::COMPLETE_TIME),
        Some("2026-11-15T11:30:00.000Z")
    );
    assert_eq!(
        eas::find_text_after(&doc, tasks::DATE_COMPLETED),
        Some("2026-11-15T11:31:00.000Z")
    );
}

fn flag_change_body(
    key: &str,
    folder: &str,
    id: &str,
    status: Option<&str>,
    read: Option<bool>,
    downloads: bool,
) -> Vec<u8> {
    use eas::{airsync as air, email as mail};
    let mut b = eas::DocumentBuilder::new();
    b.start(air::SYNC);
    b.start(air::COLLECTIONS);
    b.start(air::COLLECTION);
    b.leaf(air::SYNC_KEY, key);
    b.leaf(air::COLLECTION_ID, folder);
    b.leaf(air::GET_CHANGES, if downloads { "1" } else { "0" });
    b.start(air::COMMANDS);
    b.start(air::CHANGE);
    b.leaf(air::SERVER_ID, id);
    b.start(air::APPLICATION_DATA);
    if let Some(read) = read {
        b.leaf(mail::READ, if read { "1" } else { "0" });
    }
    if let Some(status) = status {
        b.start(mail::FLAG);
        b.leaf(mail::FLAG_STATUS, status);
        b.leaf(mail::FLAG_TYPE, "Flag for follow up");
        if status == "2" {
            for token in [
                eas::tasks::START_DATE,
                eas::tasks::UTC_START_DATE,
                eas::tasks::DUE_DATE,
                eas::tasks::UTC_DUE_DATE,
            ] {
                b.leaf(token, "2026-11-14T12:00:00.000Z");
            }
        }
        b.end();
    } else {
        b.empty_tag(mail::FLAG);
    }
    b.end();
    b.end();
    b.end();
    b.end();
    b.end();
    b.end();
    wbxml::encode_document(&b.finish())
}

#[tokio::test]
async fn native_jmap_flags_push_between_devices_and_survive_replay_and_restart() {
    use eas::{airsync as air, email as mail};
    for version in ["14.0", "16.1"] {
        let env = start_env(Opts::default()).await;
        let (a0, _, _) = env.sync_detail("iphone", "0").await;
        let (a, _, _) = env.sync_detail("iphone", &a0).await;
        let (b0, _, _) = env.sync_detail("ipad", "0").await;
        let (bkey, _, _) = env.sync_detail("ipad", &b0).await;
        let ping = env.ping("ipad", Some(ping_body(4, &[("inbox", "Email")])));
        let upload = async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let request = flag_change_body(&a, "inbox", "m1", Some("2"), Some(true), false);
            let response = env
                .req("Sync", "iphone")
                .header("MS-ASProtocolVersion", version)
                .body(request.clone())
                .send()
                .await
                .unwrap();
            let bytes = response.bytes().await.unwrap();
            let doc = wbxml::decode_document(&bytes).unwrap();
            assert_eq!(eas::find_text_after(&doc, air::STATUS), Some("1"));
            assert!(!eas::contains_token(&doc, air::COMMANDS));
            let replay = env
                .req("Sync", "iphone")
                .header("MS-ASProtocolVersion", version)
                .body(request)
                .send()
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap();
            assert_eq!(bytes, replay);
            let keywords = env.mock.keywords.lock().unwrap()["m1"].clone();
            assert!(keywords.contains("$seen"));
            assert!(keywords.contains("$flagged"));
            assert!(keywords.iter().any(|key| key.starts_with("easflag-")));
            sync_key(&doc)
        };
        let (ping, akey) = tokio::join!(ping, upload);
        assert_eq!(ping.status, "2");
        let response = env
            .req("Sync", "ipad")
            .header("MS-ASProtocolVersion", version)
            .body(sync_body(&bkey, "inbox"))
            .send()
            .await
            .unwrap();
        let bytes = response.bytes().await.unwrap();
        let doc = wbxml::decode_document(&bytes).unwrap();
        assert_eq!(eas::find_all_text_after(&doc, mail::FLAG_STATUS), vec!["2"]);
        assert_eq!(eas::find_text_after(&doc, mail::READ), Some("1"));
        assert_eq!(
            eas::find_text_after(&doc, eas::tasks::DUE_DATE),
            Some("2026-11-14T12:00:00.000Z")
        );
        let replay = env
            .req("Sync", "ipad")
            .header("MS-ASProtocolVersion", version)
            .body(sync_body(&bkey, "inbox"))
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        assert_eq!(bytes, replay);
        let bkey = sync_key(&doc);
        let env = env.restarted().await;
        let response = env
            .req("Sync", "iphone")
            .header("MS-ASProtocolVersion", version)
            .body(flag_change_body(&akey, "inbox", "m1", None, None, true))
            .send()
            .await
            .unwrap();
        let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
        assert_eq!(eas::find_text_after(&doc, air::STATUS), Some("1"));
        let keywords = env.mock.keywords.lock().unwrap()["m1"].clone();
        assert!(keywords.contains("$seen"));
        assert!(!keywords.contains("$flagged"));
        assert!(!keywords.iter().any(|key| key.starts_with("easflag-")));
        let response = env
            .req("Sync", "ipad")
            .header("MS-ASProtocolVersion", version)
            .body(sync_body(&bkey, "inbox"))
            .send()
            .await
            .unwrap();
        let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
        assert!(eas::contains_token(&doc, air::CHANGE));
        assert!(eas::contains_token(&doc, mail::FLAG));
        assert!(!eas::contains_token(&doc, mail::FLAG_STATUS));
    }
}

#[tokio::test]
async fn older_native_jmap_flag_change_wakes_ping_without_an_email_event() {
    use eas::{airsync as air, email as mail};
    let env = start_env(Opts::default()).await;
    for i in 0..350 {
        env.mock.add_mail("inbox", &format!("later{i}"));
    }
    env.sync_folder("ipad", "inbox").await;
    let record = env
        .state
        .get("u@test", "ipad", "inbox")
        .await
        .unwrap()
        .unwrap();
    // Native JMAP or webmail mutation, outside the newest-25 listing window.
    env.mock.set_keyword("m1", "$flagged", true);
    let ping = env
        .ping("ipad", Some(ping_body(4, &[("inbox", "Email")])))
        .await;
    assert_eq!(ping.status, "2");
    let response = env
        .req("Sync", "ipad")
        .body(sync_body(&record.sync_key, "inbox"))
        .send()
        .await
        .unwrap();
    let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
    assert_eq!(eas::find_text_after(&doc, air::SERVER_ID), Some("m1"));
    assert_eq!(eas::find_text_after(&doc, mail::FLAG_STATUS), Some("2"));
}

#[tokio::test]
async fn unsupported_flag_completion_does_not_silently_clear_the_native_flag() {
    use eas::airsync as air;
    let env = start_env(Opts::default()).await;
    env.mock.set_keyword("m1", "$flagged", true);
    let (prime, _, _) = env.sync_detail("client", "0").await;
    let (key, _, _) = env.sync_detail("client", &prime).await;
    let response = env
        .req("Sync", "client")
        .body(flag_change_body(
            &key,
            "inbox",
            "m1",
            Some("1"),
            Some(true),
            true,
        ))
        .send()
        .await
        .unwrap();
    let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
    assert_eq!(eas::find_all_text_after(&doc, air::STATUS), vec!["1", "6"]);
    assert_eq!(sync_key(&doc), key);
    let keywords = env.mock.keywords.lock().unwrap()["m1"].clone();
    assert!(keywords.contains("$flagged"));
    assert!(!keywords.contains("$seen"));
}

#[tokio::test]
async fn replay_never_serves_a_response_encoded_for_another_protocol_version() {
    use eas::{airsync as air, email2};
    let env = start_env(Opts::default()).await;
    let prime = env
        .req("Sync", "client")
        .header("MS-ASProtocolVersion", "16.1")
        .body(sync_body("0", "inbox"))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    let key = sync_key(&wbxml::decode_document(&prime).unwrap());
    let body = sync_body(&key, "inbox");
    let response = env
        .req("Sync", "client")
        .header("MS-ASProtocolVersion", "16.1")
        .body(body.clone())
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    let doc = wbxml::decode_document(&response).unwrap();
    assert!(eas::contains_token(&doc, email2::IS_DRAFT));
    let wrong_version = env
        .req("Sync", "client")
        .header("MS-ASProtocolVersion", "14.0")
        .body(body.clone())
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    let wrong_version = wbxml::decode_document(&wrong_version).unwrap();
    assert_eq!(eas::find_text_after(&wrong_version, air::STATUS), Some("3"));
    assert!(!eas::contains_token(&wrong_version, email2::IS_DRAFT));
    let env = env.restarted().await;
    let replay = env
        .req("Sync", "client")
        .header("MS-ASProtocolVersion", "16.1")
        .body(body)
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(replay, response);
}

fn gal_find_body(id: &str, text: &str, range: &str) -> Vec<u8> {
    use eas::find as f;
    let mut b = eas::DocumentBuilder::new();
    b.start(f::FIND);
    b.leaf(f::SEARCH_ID, id);
    b.start(f::EXECUTE_SEARCH);
    b.start(f::GAL_SEARCH_CRITERION);
    b.leaf(f::QUERY, text);
    b.start(f::OPTIONS);
    b.leaf(f::RANGE, range);
    b.end();
    b.end();
    b.end();
    b.end();
    wbxml::encode_document(&b.finish())
}

#[tokio::test]
async fn find_rejects_short_gal_queries() {
    use eas::find as f;
    let env = start_env(Opts::default()).await;
    let response = env
        .req("Find", "client")
        .header("MS-ASProtocolVersion", "16.1")
        .body(gal_find_body(
            "7dc6ffa0-2aa5-43f6-b441-bdda13785428",
            "abc",
            "0-9",
        ))
        .send()
        .await
        .unwrap();
    let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
    assert_eq!(eas::find_text_after(&doc, f::STATUS), Some("2"));
}

fn find_body(id: &str, text: &str, folder: Option<&str>, range: &str) -> Vec<u8> {
    use eas::{airsync as air, find as f};
    let mut b = eas::DocumentBuilder::new();
    b.start(f::FIND);
    b.leaf(f::SEARCH_ID, id);
    b.start(f::EXECUTE_SEARCH);
    b.start(f::MAILBOX_SEARCH_CRITERION);
    b.start(f::QUERY);
    b.leaf(air::CLASS, "Email");
    b.leaf(f::FREE_TEXT, text);
    if let Some(folder) = folder {
        b.leaf(air::COLLECTION_ID, folder);
    }
    b.end();
    b.start(f::OPTIONS);
    b.leaf(f::RANGE, range);
    b.end();
    b.end();
    b.end();
    b.end();
    wbxml::encode_document(&b.finish())
}

#[tokio::test]
async fn find_maps_kql_to_jmap_pages_metadata_and_restarts_changed_searches() {
    use eas::{airsync as air, airsync_base as base, find as f};
    let env = start_env(Opts::default()).await;
    env.mock.add_mail("other", "archived-old");
    env.mock.set_date("archived-old", "2020-01-01T00:00:00Z");
    let sid = "7dc6ffa0-2aa5-43f6-b441-bdda13785428";
    let request = find_body(sid, "subject: subject", None, "0-0");
    let send = |body| {
        env.req("Find", "client")
            .header("MS-ASProtocolVersion", "16.1")
            .body(body)
            .send()
    };
    let response = send(request).await.unwrap();
    assert_eq!(response.status(), 200);
    let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
    assert_eq!(eas::find_all_text_after(&doc, f::STATUS), vec!["1", "1"]);
    assert_eq!(eas::find_text_after(&doc, f::TOTAL), Some("3"));
    assert_eq!(eas::find_text_after(&doc, f::RANGE), Some("0-0"));
    assert_eq!(eas::find_all_text_after(&doc, air::SERVER_ID).len(), 1);
    assert!(!eas::contains_token(&doc, base::BODY));
    let response = send(find_body(sid, "subject: subject", None, "1-2"))
        .await
        .unwrap();
    let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
    let ids = eas::find_all_text_after(&doc, air::SERVER_ID);
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&"archived-old".to_owned()));
    let env = env.restarted().await;
    let response = env
        .req("Find", "client")
        .header("MS-ASProtocolVersion", "16.1")
        .body(find_body(sid, "subject: subject", None, "1-2"))
        .send()
        .await
        .unwrap();
    let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
    assert_eq!(eas::find_all_text_after(&doc, f::STATUS), vec!["1", "1"]);
    env.mock.add_mail("inbox", "new");
    let response = env
        .req("Find", "client")
        .header("MS-ASProtocolVersion", "16.1")
        .body(find_body(sid, "subject: subject", None, "1-2"))
        .send()
        .await
        .unwrap();
    let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
    assert_eq!(eas::find_all_text_after(&doc, f::STATUS), vec!["1", "4"]);
}

#[tokio::test]
async fn find_scopes_queries_and_rejects_unsupported_inputs() {
    use eas::{airsync as air, find as f};
    let env = start_env(Opts::default()).await;
    env.mock.add_mail("other", "archived");
    env.mock.set_keyword("m1", "$flagged", true);
    let sid = "7dc6ffa0-2aa5-43f6-b441-bdda13785428";
    for (query, folder, range, status, expected) in [
        (
            "subject:subject AND isflagged:true",
            Some("inbox"),
            "0-9",
            vec!["1", "1"],
            vec!["m1"],
        ),
        (
            "subject:archived",
            Some("other"),
            "0-9",
            vec!["1", "1"],
            vec!["archived"],
        ),
        (
            "subject:archived",
            Some("inbox"),
            "0-9",
            vec!["1", "1"],
            vec![],
        ),
        ("sent:2020-01-01", None, "0-9", vec!["2"], vec![]),
        ("subject:subject", Some("missing"), "0-9", vec!["3"], vec![]),
        ("subject:subject", None, "8-2", vec!["2"], vec![]),
        ("subject:subject", None, "1-2", vec!["1", "4"], vec![]),
    ] {
        let response = env
            .req("Find", "client")
            .header("MS-ASProtocolVersion", "16.1")
            .body(find_body(sid, query, folder, range))
            .send()
            .await
            .unwrap();
        let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
        assert_eq!(eas::find_all_text_after(&doc, f::STATUS), status, "{query}");
        assert_eq!(
            eas::find_all_text_after(&doc, air::SERVER_ID),
            expected,
            "{query}"
        );
    }
    let response = env
        .req("Find", "client")
        .header("MS-ASProtocolVersion", "14.0")
        .body(find_body(sid, "subject:subject", None, "0-9"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
}

#[tokio::test]
async fn sync_options_survive_restart_replace_as_a_block_and_stay_scoped() {
    use eas::{airsync as air, airsync_base as base};
    for version in ["14.0", "16.1"] {
        let env = start_env(Opts::default()).await;
        env.mock.set_date("m1", &days_ago(90));
        let options = mail_preferences(Some("5"), Some(("1", "2")));
        let send = |body, device: &str| {
            env.req("Sync", device)
                .header("MS-ASProtocolVersion", version)
                .body(body)
                .send()
        };
        let response = send(
            sync_body_preferences("0", "inbox", Some(&options), true),
            "iphone",
        )
        .await
        .unwrap();
        let prime = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
        let body = sync_body_preferences(&sync_key(&prime), "inbox", None, true);
        let response = send(body.clone(), "iphone").await.unwrap();
        let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
        assert_eq!(eas::find_all_text_after(&doc, air::SERVER_ID), vec!["m2"]);
        assert_eq!(eas::find_text_after(&doc, base::DATA), Some("bo"));
        let response = send(body, "iphone").await.unwrap();
        assert_eq!(
            wbxml::decode_document(&response.bytes().await.unwrap()).unwrap(),
            doc
        );
        let key = sync_key(&doc);

        let env = env.restarted().await;
        let send = |body, device: &str| {
            env.req("Sync", device)
                .header("MS-ASProtocolVersion", version)
                .body(body)
                .send()
        };
        let empty = mail_preferences(None, None);
        let response = send(
            sync_body_preferences("wrong-key", "inbox", Some(&empty), true),
            "iphone",
        )
        .await
        .unwrap();
        let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
        assert_eq!(eas::find_text_after(&doc, air::STATUS), Some("3"));
        env.mock.add_mail("inbox", "new");
        let response = send(sync_body_preferences(&key, "inbox", None, true), "iphone")
            .await
            .unwrap();
        let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
        assert_eq!(eas::find_text_after(&doc, base::DATA), Some("bo"));
        let key = sync_key(&doc);

        let body_only = mail_preferences(None, Some(("1", "1")));
        let response = send(
            sync_body_preferences(&key, "inbox", Some(&body_only), true),
            "iphone",
        )
        .await
        .unwrap();
        let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
        assert_eq!(eas::find_all_text_after(&doc, air::SERVER_ID), vec!["m1"]);
        assert_eq!(eas::find_text_after(&doc, base::DATA), Some("b"));
        let key = sync_key(&doc);

        let response = send(sync_body_preferences("0", "inbox", None, true), "ipad")
            .await
            .unwrap();
        let prime = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
        let response = send(
            sync_body_preferences(&sync_key(&prime), "inbox", None, true),
            "ipad",
        )
        .await
        .unwrap();
        let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
        assert!(eas::find_all_text_after(&doc, base::DATA)
            .iter()
            .all(|body| body == "body"));

        env.mock.add_mail("other", "other-mail");
        let response = send(sync_body_preferences("0", "other", None, true), "iphone")
            .await
            .unwrap();
        let prime = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
        let response = send(
            sync_body_preferences(&sync_key(&prime), "other", None, true),
            "iphone",
        )
        .await
        .unwrap();
        let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
        assert_eq!(eas::find_text_after(&doc, base::DATA), Some("body"));

        let response = send(
            sync_body_preferences(&key, "inbox", Some(&empty), false),
            "iphone",
        )
        .await
        .unwrap();
        let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
        assert_eq!(eas::find_text_after(&doc, air::STATUS), Some("1"));
        env.mock.add_mail("inbox", "after-clear");
        let response = send(
            sync_body_preferences(&sync_key(&doc), "inbox", None, true),
            "iphone",
        )
        .await
        .unwrap();
        let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
        assert_eq!(eas::find_text_after(&doc, base::DATA), Some("body"));
    }
}

#[tokio::test]
async fn sync_options_do_not_cross_protocol_versions_or_accept_mixed_sms_options() {
    use eas::{airsync as air, airsync_base as base};
    let env = start_env(Opts::default()).await;
    let options = mail_preferences(None, Some(("1", "1")));
    let response = env
        .req("Sync", "client")
        .header("MS-ASProtocolVersion", "16.1")
        .body(sync_body_preferences("0", "inbox", Some(&options), true))
        .send()
        .await
        .unwrap();
    let prime = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
    let key = sync_key(&prime);

    let mut mixed = mail_preferences(Some("5"), None);
    mixed.nodes.extend(mail_preferences(None, None).nodes);
    let response = env
        .req("Sync", "client")
        .header("MS-ASProtocolVersion", "16.1")
        .body(sync_body_preferences(&key, "inbox", Some(&mixed), true))
        .send()
        .await
        .unwrap();
    let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
    assert_eq!(eas::find_text_after(&doc, air::STATUS), Some("4"));
    let mut sms = eas::DocumentBuilder::new();
    sms.start(air::OPTIONS);
    sms.leaf(air::CLASS, "SMS");
    sms.end();
    let response = env
        .req("Sync", "client")
        .header("MS-ASProtocolVersion", "16.1")
        .body(sync_body_preferences(
            &key,
            "inbox",
            Some(&sms.finish()),
            true,
        ))
        .send()
        .await
        .unwrap();
    let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
    assert_eq!(eas::find_text_after(&doc, air::STATUS), Some("4"));
    let response = env
        .req("Sync", "client")
        .header("MS-ASProtocolVersion", "14.0")
        .body(sync_body_preferences(&key, "inbox", None, true))
        .send()
        .await
        .unwrap();
    let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
    assert!(eas::find_all_text_after(&doc, base::DATA)
        .iter()
        .all(|body| body == "body"));
}

#[tokio::test]
async fn partial_and_empty_sync_preserve_cached_folders_across_restart() {
    use eas::airsync as air;
    for version in ["14.0", "16.1"] {
        let env = start_env(Opts::default()).await;
        env.mock.add_mail("other", "other-mail");
        let send = |body| {
            env.req("Sync", "client")
                .header("MS-ASProtocolVersion", version)
                .body(body)
                .send()
        };
        let response = send(partial_sync(&[("inbox", "0")], true)).await.unwrap();
        let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
        assert_eq!(eas::find_text_after(&doc, air::STATUS), Some("13"));
        let response = send(partial_sync(&[("inbox", "0"), ("other", "0")], false))
            .await
            .unwrap();
        let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
        assert_eq!(
            eas::find_all_text_after(&doc, air::SYNC_KEY),
            vec!["1", "1"]
        );
        let response = send(partial_sync(&[("inbox", "1"), ("other", "1")], false))
            .await
            .unwrap();
        let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
        let keys = eas::find_all_text_after(&doc, air::SYNC_KEY);
        let response = send(partial_sync(
            &[("inbox", &keys[0]), ("other", &keys[1])],
            false,
        ))
        .await
        .unwrap();
        let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
        assert!(!eas::contains_token(&doc, air::COMMANDS));

        let env = env.restarted().await;
        let send = |body| {
            env.req("Sync", "client")
                .header("MS-ASProtocolVersion", version)
                .body(body)
                .send()
        };
        env.mock.add_mail("other", "after-restart");
        let response = send(partial_sync(&[("inbox", &keys[0])], true))
            .await
            .unwrap();
        let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
        assert_eq!(
            eas::find_all_text_after(&doc, air::COLLECTION_ID),
            vec!["inbox", "other"]
        );
        assert_eq!(
            eas::find_all_text_after(&doc, air::SERVER_ID),
            vec!["after-restart"]
        );
        let new_keys = eas::find_all_text_after(&doc, air::SYNC_KEY);
        let response = send(partial_sync(&[("other", &new_keys[1])], true))
            .await
            .unwrap();
        let quiet = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
        assert!(!eas::contains_token(&quiet, air::COMMANDS));

        let response = send(partial_sync(&[("inbox", "invalid")], false))
            .await
            .unwrap();
        let invalid = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
        assert_eq!(eas::find_text_after(&invalid, air::STATUS), Some("3"));
        let env = env.restarted().await;
        env.mock.add_mail("inbox", "empty-after-restart");
        let response = env
            .req("Sync", "client")
            .header("MS-ASProtocolVersion", version)
            .send()
            .await
            .unwrap();
        let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
        assert_eq!(
            eas::find_all_text_after(&doc, air::COLLECTION_ID),
            vec!["other", "inbox"]
        );
        assert_eq!(
            eas::find_all_text_after(&doc, air::SERVER_ID),
            vec!["empty-after-restart"]
        );
        let response = env
            .req("Sync", "other-device")
            .header("MS-ASProtocolVersion", version)
            .send()
            .await
            .unwrap();
        let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
        assert_eq!(eas::find_text_after(&doc, air::STATUS), Some("13"));
        let response = env
            .http
            .post(format!("{}?Cmd=Sync&User=u%40test&DeviceId=client", env.gw))
            .basic_auth("different@example.test", Some("pw"))
            .header("MS-ASProtocolVersion", version)
            .send()
            .await
            .unwrap();
        let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
        assert_eq!(eas::find_text_after(&doc, air::STATUS), Some("13"));
        let other_version = if version == "14.0" { "16.1" } else { "14.0" };
        let response = env
            .req("Sync", "client")
            .header("MS-ASProtocolVersion", other_version)
            .send()
            .await
            .unwrap();
        let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
        assert_eq!(eas::find_text_after(&doc, air::STATUS), Some("13"));
    }
}

#[tokio::test]
async fn cached_sync_omits_uploads_and_rejects_duplicate_collections() {
    use base64::Engine;
    use eas::airsync as air;
    let env = start_env(Opts::default()).await;
    let (key, _) = initial_sync(&env, "client", None).await;
    let response = env
        .req("Sync", "client")
        .body(flag_change_body(
            &key,
            "inbox",
            "m1",
            Some("2"),
            Some(true),
            false,
        ))
        .send()
        .await
        .unwrap();
    let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
    assert_eq!(eas::find_text_after(&doc, air::STATUS), Some("1"));
    let saved = env
        .state
        .item_states("u@test", "client", "__sync_request__:14.0")
        .await
        .unwrap();
    let cached = base64::engine::general_purpose::STANDARD
        .decode(&saved[0].hash)
        .unwrap();
    let cached = wbxml::decode_document(&cached).unwrap();
    assert!(!eas::contains_token(&cached, air::COMMANDS));
    let log_count = env.mock.change_log.lock().unwrap().len();
    let response = env.req("Sync", "client").send().await.unwrap();
    let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
    assert_eq!(eas::find_text_after(&doc, air::STATUS), Some("1"));
    assert_eq!(env.mock.change_log.lock().unwrap().len(), log_count);
    let response = env
        .req("Sync", "client")
        .body(partial_sync(&[("inbox", "0"), ("inbox", "0")], false))
        .send()
        .await
        .unwrap();
    let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
    assert_eq!(eas::find_text_after(&doc, air::STATUS), Some("4"));
    let mut bare = eas::DocumentBuilder::new();
    bare.start(air::SYNC);
    bare.empty_tag(air::PARTIAL);
    bare.end();
    let response = env
        .req("Sync", "client")
        .body(wbxml::encode_document(&bare.finish()))
        .send()
        .await
        .unwrap();
    let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
    assert_eq!(eas::find_text_after(&doc, air::STATUS), Some("4"));
}

#[tokio::test]
async fn invalid_flag_changes_are_item_errors_and_do_not_block_valid_changes_or_fetches() {
    use eas::{airsync as air, email as mail};
    for version in ["14.0", "16.1"] {
        let env = start_env(Opts::default()).await;
        let (key, _) = initial_sync(&env, "client", None).await;
        let body = mixed_flag_request(&key, "inbox", "m1", "m2");
        let send = || {
            env.req("Sync", "client")
                .header("MS-ASProtocolVersion", version)
                .body(body.clone())
                .send()
        };
        let response = send().await.unwrap();
        let first = response.bytes().await.unwrap();
        let doc = wbxml::decode_document(&first).unwrap();
        assert_eq!(
            eas::find_all_text_after(&doc, air::STATUS),
            vec!["1", "6", "1"]
        );
        assert_eq!(
            eas::find_all_text_after(&doc, air::SERVER_ID),
            vec!["m1", "m1"]
        );
        assert_eq!(eas::find_text_after(&doc, mail::READ), Some("0"));
        let keywords = env.mock.keywords.lock().unwrap().clone();
        assert!(!keywords
            .get("m1")
            .is_some_and(|values| values.contains("$seen") || values.contains("$flagged")));
        assert!(keywords
            .get("m2")
            .is_some_and(|values| values.contains("$seen")));
        let count = env.mock.change_log.lock().unwrap().len();
        let response = send().await.unwrap();
        assert_eq!(response.bytes().await.unwrap(), first);
        assert_eq!(env.mock.change_log.lock().unwrap().len(), count);
    }
}

#[tokio::test]
async fn multiple_body_preferences_fallback_and_persist_without_leaking_partial_data() {
    use eas::{airsync as air, airsync_base as base, RequestedBodyPreference as Pref};
    for version in ["14.0", "16.1"] {
        let env = start_env(Opts::default()).await;
        let prefs = [
            Pref {
                body_type: 2,
                truncation_size: Some(1),
                all_or_none: true,
                preview: Some(0),
            },
            Pref {
                body_type: 1,
                truncation_size: Some(2),
                preview: Some(1),
                ..Default::default()
            },
        ];
        let options = requested_body_options(&prefs);
        let response = env
            .req("Sync", "client")
            .header("MS-ASProtocolVersion", version)
            .body(sync_body_preferences("0", "inbox", Some(&options), true))
            .send()
            .await
            .unwrap();
        let prime = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
        let env = env.restarted().await;
        let response = env
            .req("Sync", "client")
            .header("MS-ASProtocolVersion", version)
            .body(sync_body_preferences(
                &sync_key(&prime),
                "inbox",
                None,
                true,
            ))
            .send()
            .await
            .unwrap();
        let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
        assert_eq!(eas::find_all_text_after(&doc, base::DATA), vec!["bo", "bo"]);
        assert_eq!(
            eas::find_all_text_after(&doc, base::PREVIEW),
            vec!["b", "b"]
        );
        let key = sync_key(&doc);
        let duplicate = requested_body_options(&[prefs[1], prefs[1]]);
        let response = env
            .req("Sync", "client")
            .header("MS-ASProtocolVersion", version)
            .body(sync_body_preferences(&key, "inbox", Some(&duplicate), true))
            .send()
            .await
            .unwrap();
        let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
        assert_eq!(eas::find_text_after(&doc, air::STATUS), Some("4"));

        env.mock.add_mail("inbox", "all-or-none");
        let options = requested_body_options(&[Pref {
            body_type: 1,
            truncation_size: Some(1),
            all_or_none: true,
            preview: Some(0),
        }]);
        let response = env
            .req("Sync", "client")
            .header("MS-ASProtocolVersion", version)
            .body(sync_body_preferences(&key, "inbox", Some(&options), true))
            .send()
            .await
            .unwrap();
        let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
        assert!(!eas::contains_token(&doc, base::DATA));
        assert!(!eas::contains_token(&doc, base::PREVIEW));
        assert_eq!(
            eas::find_text_after(&doc, base::ESTIMATED_DATA_SIZE),
            Some("4")
        );
        assert_eq!(eas::find_text_after(&doc, base::TRUNCATED), Some("1"));

        let legacy = env
            .state
            .item_states("u@test", "legacy", &format!("__options__:{version}:inbox"))
            .await
            .unwrap();
        assert!(legacy.is_empty());
        env.state
            .put_item_states(
                "u@test",
                "legacy",
                &format!("__options__:{version}:inbox"),
                vec![state::ItemState {
                    item_id: "options".to_owned(),
                    hash: serde_json::json!({"filter_type":0,"body_type":1,"truncation_size":1})
                        .to_string(),
                }],
            )
            .await
            .unwrap();
        let response = env
            .req("Sync", "legacy")
            .header("MS-ASProtocolVersion", version)
            .body(sync_body_preferences("0", "inbox", None, true))
            .send()
            .await
            .unwrap();
        let prime = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
        let response = env
            .req("Sync", "legacy")
            .header("MS-ASProtocolVersion", version)
            .body(sync_body_preferences(
                &sync_key(&prime),
                "inbox",
                None,
                true,
            ))
            .send()
            .await
            .unwrap();
        let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
        assert!(eas::find_all_text_after(&doc, base::DATA)
            .iter()
            .all(|body| body == "b"));
    }
}

#[tokio::test]
async fn sync_and_item_operations_honor_mime_limits_and_all_or_none() {
    use eas::{
        airsync as air, airsync_base as base, item_operations as ops,
        RequestedBodyPreference as Pref,
    };
    let env = start_env(Opts::default()).await;
    let options = requested_body_options(&[Pref {
        body_type: 4,
        truncation_size: Some(5),
        preview: Some(2),
        ..Default::default()
    }]);
    let response = env
        .req("Sync", "mime-list")
        .body(sync_body_preferences("0", "inbox", Some(&options), true))
        .send()
        .await
        .unwrap();
    let prime = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
    let response = env
        .req("Sync", "mime-list")
        .body(sync_body_preferences(
            &sync_key(&prime),
            "inbox",
            None,
            true,
        ))
        .send()
        .await
        .unwrap();
    let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
    assert_eq!(eas::find_all_text_after(&doc, base::TYPE), vec!["4", "4"]);
    assert_eq!(body_data_bytes(&doc), Some(&MIME_BYTES[..5]));

    let mut b = eas::DocumentBuilder::new();
    b.start(ops::ITEM_OPERATIONS);
    b.start(ops::FETCH);
    b.leaf(ops::STORE, "Mailbox");
    b.leaf(air::COLLECTION_ID, "inbox");
    b.leaf(air::SERVER_ID, "m1");
    b.start(ops::OPTIONS);
    let options = requested_body_options(&[Pref {
        body_type: 1,
        truncation_size: Some(1),
        all_or_none: true,
        preview: Some(2),
    }]);
    for child in eas::child_elements(&options) {
        b.extend(child.nodes);
    }
    b.end();
    b.end();
    b.end();
    let response = env
        .req("ItemOperations", "client")
        .body(wbxml::encode_document(&b.finish()))
        .send()
        .await
        .unwrap();
    let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
    assert_eq!(eas::find_all_text_after(&doc, ops::STATUS), vec!["1", "1"]);
    assert!(!eas::contains_token(&doc, base::DATA));
    assert_eq!(eas::find_text_after(&doc, base::PREVIEW), Some("bo"));
    assert_eq!(
        eas::find_text_after(&doc, base::ESTIMATED_DATA_SIZE),
        Some("4")
    );
}

#[tokio::test]
async fn failed_sync_mime_preparation_keeps_the_previous_key_and_pending_messages() {
    use eas::{airsync as air, RequestedBodyPreference as Pref};
    let env = start_env(Opts::default()).await;
    let options = requested_body_options(&[Pref {
        body_type: 4,
        ..Default::default()
    }]);
    let response = env
        .req("Sync", "client")
        .body(sync_body_preferences("0", "inbox", Some(&options), true))
        .send()
        .await
        .unwrap();
    let prime = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
    let key = sync_key(&prime);
    env.mock.download_error.store(true, Ordering::SeqCst);
    let body = sync_body_preferences(&key, "inbox", None, true);
    let response = env
        .req("Sync", "client")
        .body(body.clone())
        .send()
        .await
        .unwrap();
    let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
    assert_eq!(eas::find_text_after(&doc, air::STATUS), Some("5"));
    assert_eq!(sync_key(&doc), key);
    assert!(env
        .state
        .get("u@test", "client", "inbox")
        .await
        .unwrap()
        .unwrap()
        .seen_ids
        .is_empty());
    env.mock.download_error.store(false, Ordering::SeqCst);
    let response = env
        .req("Sync", "client")
        .body(body.clone())
        .send()
        .await
        .unwrap();
    let first = response.bytes().await.unwrap();
    let doc = wbxml::decode_document(&first).unwrap();
    assert_eq!(eas::find_text_after(&doc, air::STATUS), Some("1"));
    assert_eq!(eas::find_all_text_after(&doc, air::SERVER_ID).len(), 2);
    assert_eq!(body_data_bytes(&doc), Some(MIME_BYTES));
    let response = env.req("Sync", "client").body(body).send().await.unwrap();
    assert_eq!(response.bytes().await.unwrap(), first);
}

#[tokio::test]
async fn search_rejects_unsupported_or_misplaced_classes() {
    use eas::{airsync as air, search as srch};
    let env = start_env(Opts::default()).await;
    for (classes, nested, expected) in [
        (vec!["Email"], true, "1"),
        (vec!["SMS"], true, "8"),
        (vec!["Email", "SMS"], true, "8"),
        (vec!["Email"], false, "8"),
    ] {
        let mut b = eas::DocumentBuilder::new();
        b.start(srch::SEARCH);
        b.start(srch::STORE);
        b.leaf(srch::NAME, "Mailbox");
        b.start(srch::QUERY);
        if nested {
            b.start(srch::AND);
        }
        for class in classes {
            b.leaf(air::CLASS, class);
        }
        b.leaf(srch::FREE_TEXT, "subject");
        if nested {
            b.end();
        }
        b.end();
        b.end();
        b.end();
        let response = env
            .req("Search", "client")
            .body(wbxml::encode_document(&b.finish()))
            .send()
            .await
            .unwrap();
        let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
        assert_eq!(
            eas::find_all_text_after(&doc, srch::STATUS),
            vec!["1", expected]
        );
        if expected == "8" {
            assert!(!eas::contains_token(&doc, srch::TOTAL));
        }
    }
}

#[tokio::test]
async fn email_search_excludes_hidden_notes_but_keeps_shared_mail() {
    use eas::{airsync as air, search as srch};
    let env = start_env(Opts::default()).await;
    enable_groupware(&env, 1);
    let mut b = eas::DocumentBuilder::new();
    b.start(srch::SEARCH);
    b.start(srch::STORE);
    b.leaf(srch::NAME, "Mailbox");
    b.start(srch::QUERY);
    b.start(srch::AND);
    b.leaf(air::CLASS, "Email");
    b.leaf(srch::FREE_TEXT, "n0000");
    b.end();
    b.end();
    b.end();
    b.end();
    let request = wbxml::encode_document(&b.finish());
    let response = env
        .req("Search", "client")
        .body(request.clone())
        .send()
        .await
        .unwrap();
    let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
    assert_eq!(eas::find_text_after(&doc, srch::TOTAL), Some("0"));
    env.mock.add_mail("inbox", "n0000");
    let response = env
        .req("Search", "client")
        .body(request)
        .send()
        .await
        .unwrap();
    let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
    assert_eq!(eas::find_text_after(&doc, srch::TOTAL), Some("1"));
    assert_eq!(
        eas::find_text_after(&doc, air::COLLECTION_ID),
        Some("inbox")
    );
}

#[tokio::test]
async fn unscoped_and_mixed_searches_page_across_classes() {
    use eas::{airsync as air, search as srch};
    let env = start_env(Opts::default()).await;
    enable_groupware(&env, 2);
    {
        let mut all = env.mock.groupware.lock().unwrap();
        for contact in all.get_mut("contacts").unwrap() {
            contact["name"]["full"] = serde_json::json!("subject contact");
        }
        for item in all.get_mut("calendar").unwrap() {
            item["title"] = serde_json::json!("subject event");
        }
    }
    for (classes, expected_total, expected_classes) in [
        (
            vec![],
            "10",
            vec!["Email", "Contacts", "Contacts", "Calendar"],
        ),
        (
            vec!["Contacts", "Calendar"],
            "4",
            vec!["Contacts", "Calendar", "Calendar"],
        ),
    ] {
        let mut b = eas::DocumentBuilder::new();
        b.start(srch::SEARCH);
        b.start(srch::STORE);
        b.leaf(srch::NAME, "Mailbox");
        b.start(srch::QUERY);
        b.start(srch::AND);
        for class in classes {
            b.leaf(air::CLASS, class);
        }
        b.leaf(srch::FREE_TEXT, "subject");
        b.end();
        b.end();
        b.start(srch::OPTIONS);
        b.leaf(srch::RANGE, "1-4");
        b.end();
        b.end();
        b.end();
        let response = env
            .req("Search", "multi")
            .body(wbxml::encode_document(&b.finish()))
            .send()
            .await
            .unwrap();
        let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
        assert_eq!(eas::find_all_text_after(&doc, srch::STATUS), vec!["1", "1"]);
        assert_eq!(
            eas::find_text_after(&doc, srch::TOTAL),
            Some(expected_total)
        );
        assert_eq!(eas::find_all_text_after(&doc, air::CLASS), expected_classes);
    }
}

#[tokio::test]
async fn mixed_search_does_not_return_partial_results_when_a_class_changes() {
    use eas::{airsync as air, search as srch};
    let env = start_env(Opts::default()).await;
    enable_groupware(&env, 80);
    env.mock.snapshot_changes.store(true, Ordering::SeqCst);
    let mut b = eas::DocumentBuilder::new();
    b.start(srch::SEARCH);
    b.start(srch::STORE);
    b.leaf(srch::NAME, "Mailbox");
    b.start(srch::QUERY);
    b.start(srch::AND);
    b.leaf(air::CLASS, "Email");
    b.leaf(air::CLASS, "Contacts");
    b.leaf(srch::FREE_TEXT, "contact");
    b.end();
    b.end();
    b.end();
    b.end();
    let response = env
        .req("Search", "multi")
        .body(wbxml::encode_document(&b.finish()))
        .send()
        .await
        .unwrap();
    let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
    assert_eq!(eas::find_all_text_after(&doc, srch::STATUS), vec!["1", "3"]);
    assert!(!eas::contains_token(&doc, srch::RESULT));
    assert!(!eas::contains_token(&doc, srch::TOTAL));
}

#[tokio::test]
async fn classless_search_uses_the_scoped_groupware_folder() {
    use eas::{airsync as air, calendar as cal, contacts, search as srch};
    let env = start_env(Opts::default()).await;
    enable_groupware(&env, 3);
    for (folder, text, class, subject, expected) in [
        (
            "ab_book",
            "Contact 1",
            "Contacts",
            contacts::FILE_AS,
            "Contact 1",
        ),
        (
            "cal_calendar",
            "Event 1",
            "Calendar",
            cal::SUBJECT,
            "Event 1",
        ),
    ] {
        let mut b = eas::DocumentBuilder::new();
        b.start(srch::SEARCH);
        b.start(srch::STORE);
        b.leaf(srch::NAME, "Mailbox");
        b.start(srch::QUERY);
        b.start(srch::AND);
        b.leaf(air::COLLECTION_ID, folder);
        b.leaf(srch::FREE_TEXT, text);
        b.end();
        b.end();
        b.end();
        b.end();
        let response = env
            .req("Search", "client")
            .body(wbxml::encode_document(&b.finish()))
            .send()
            .await
            .unwrap();
        let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
        assert_eq!(eas::find_all_text_after(&doc, srch::STATUS), vec!["1", "1"]);
        assert_eq!(eas::find_text_after(&doc, srch::TOTAL), Some("1"));
        assert_eq!(eas::find_text_after(&doc, air::CLASS), Some(class));
        assert_eq!(eas::find_text_after(&doc, subject), Some(expected));
    }
}

#[tokio::test]
async fn task_search_returns_results_and_fetches_them() {
    use eas::{
        airsync as air, airsync_base as base, item_operations as ops, search as srch, tasks,
    };
    let env = start_env(Opts::default()).await;
    enable_groupware(&env, 4);
    {
        let mut items = env.mock.groupware.lock().unwrap();
        let task = items
            .get_mut("calendar")
            .unwrap()
            .iter_mut()
            .find(|item| item["id"] == "t0002")
            .unwrap();
        task["description"] = serde_json::json!("a unique description token");
    }
    for explicit in [true, false] {
        let mut b = eas::DocumentBuilder::new();
        b.start(srch::SEARCH);
        b.start(srch::STORE);
        b.leaf(srch::NAME, "Mailbox");
        b.start(srch::QUERY);
        b.start(srch::AND);
        if explicit {
            b.leaf(air::CLASS, "Tasks");
        }
        b.leaf(air::COLLECTION_ID, "task_calendar");
        b.leaf(srch::FREE_TEXT, "DESCRIPTION TOKEN");
        b.end();
        b.end();
        b.end();
        b.end();
        let response = env
            .req("Search", "client")
            .body(wbxml::encode_document(&b.finish()))
            .send()
            .await
            .unwrap();
        let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
        assert_eq!(eas::find_all_text_after(&doc, srch::STATUS), vec!["1", "1"]);
        assert_eq!(eas::find_text_after(&doc, srch::TOTAL), Some("1"));
        assert_eq!(eas::find_text_after(&doc, tasks::SUBJECT), Some("Task 2"));
        let id = eas::find_text_after(&doc, srch::LONG_ID).unwrap();
        let mut b = eas::DocumentBuilder::new();
        b.start(ops::ITEM_OPERATIONS);
        b.start(ops::FETCH);
        b.leaf(ops::STORE, "Mailbox");
        b.leaf(srch::LONG_ID, id);
        b.end();
        b.end();
        let response = env
            .req("ItemOperations", "client")
            .body(wbxml::encode_document(&b.finish()))
            .send()
            .await
            .unwrap();
        let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
        assert_eq!(eas::find_all_text_after(&doc, ops::STATUS), vec!["1", "1"]);
        assert_eq!(eas::find_text_after(&doc, tasks::SUBJECT), Some("Task 2"));
        assert_eq!(
            eas::find_text_after(&doc, base::DATA),
            Some("a unique description token")
        );
    }
}

#[tokio::test]
async fn task_description_changes_are_synced() {
    use eas::{airsync as air, airsync_base as base};
    let env = start_env(Opts::default()).await;
    enable_groupware(&env, 1);
    let prime = item_sync(&env, "ipad", "task_calendar", "0", "100").await;
    let first = item_sync(&env, "ipad", "task_calendar", &sync_key(&prime), "100").await;
    {
        let mut items = env.mock.groupware.lock().unwrap();
        let task = items
            .get_mut("calendar")
            .unwrap()
            .iter_mut()
            .find(|item| item["id"] == "t0000")
            .unwrap();
        task["description"] = serde_json::json!("edited description");
    }
    let update = item_sync(&env, "ipad", "task_calendar", &sync_key(&first), "100").await;
    assert!(eas::contains_token(&update, air::CHANGE));
    assert_eq!(
        eas::find_text_after(&update, base::DATA),
        Some("edited description")
    );
}

#[tokio::test]
async fn calendar_search_pages_and_fetches_results() {
    use eas::{airsync as air, calendar as cal, item_operations as ops, search as srch};
    let env = start_env(Opts::default()).await;
    enable_groupware(&env, 12);
    let mut b = eas::DocumentBuilder::new();
    b.start(srch::SEARCH);
    b.start(srch::STORE);
    b.leaf(srch::NAME, "Mailbox");
    b.start(srch::QUERY);
    b.start(srch::AND);
    b.leaf(air::CLASS, "Calendar");
    b.leaf(srch::FREE_TEXT, "EVENT");
    b.end();
    b.end();
    b.start(srch::OPTIONS);
    b.leaf(srch::RANGE, "2-3");
    b.end();
    b.end();
    b.end();
    let response = env
        .req("Search", "client")
        .body(wbxml::encode_document(&b.finish()))
        .send()
        .await
        .unwrap();
    let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
    assert_eq!(eas::find_all_text_after(&doc, srch::STATUS), vec!["1", "1"]);
    assert_eq!(eas::find_text_after(&doc, srch::TOTAL), Some("12"));
    assert_eq!(eas::find_text_after(&doc, srch::RANGE), Some("2-3"));
    assert_eq!(
        eas::find_all_text_after(&doc, cal::SUBJECT),
        vec!["Event 2", "Event 3"]
    );
    let id = eas::find_text_after(&doc, srch::LONG_ID).unwrap();
    let mut b = eas::DocumentBuilder::new();
    b.start(ops::ITEM_OPERATIONS);
    b.start(ops::FETCH);
    b.leaf(ops::STORE, "Mailbox");
    b.leaf(srch::LONG_ID, id);
    b.end();
    b.end();
    let response = env
        .req("ItemOperations", "client")
        .body(wbxml::encode_document(&b.finish()))
        .send()
        .await
        .unwrap();
    let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
    assert_eq!(eas::find_all_text_after(&doc, ops::STATUS), vec!["1", "1"]);
    assert_eq!(eas::find_text_after(&doc, cal::SUBJECT), Some("Event 2"));

    for (long_id, folder) in [
        ("calendar:calendar:e0002", Some("cal_other")),
        ("calendar:bad", None),
    ] {
        let mut b = eas::DocumentBuilder::new();
        b.start(ops::ITEM_OPERATIONS);
        b.start(ops::FETCH);
        b.leaf(ops::STORE, "Mailbox");
        b.leaf(srch::LONG_ID, long_id);
        if let Some(folder) = folder {
            b.leaf(air::COLLECTION_ID, folder);
        }
        b.end();
        b.end();
        let response = env
            .req("ItemOperations", "client")
            .body(wbxml::encode_document(&b.finish()))
            .send()
            .await
            .unwrap();
        let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
        assert_eq!(eas::find_all_text_after(&doc, ops::STATUS), vec!["1", "6"]);
    }
}

#[tokio::test]
async fn calendar_search_rejects_a_changing_snapshot() {
    use eas::{airsync as air, search as srch};
    let env = start_env(Opts::default()).await;
    enable_groupware(&env, 80);
    env.mock.snapshot_changes.store(true, Ordering::SeqCst);
    let mut b = eas::DocumentBuilder::new();
    b.start(srch::SEARCH);
    b.start(srch::STORE);
    b.leaf(srch::NAME, "Mailbox");
    b.start(srch::QUERY);
    b.start(srch::AND);
    b.leaf(air::CLASS, "Calendar");
    b.leaf(srch::FREE_TEXT, "Event");
    b.end();
    b.end();
    b.end();
    b.end();
    let response = env
        .req("Search", "client")
        .body(wbxml::encode_document(&b.finish()))
        .send()
        .await
        .unwrap();
    let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
    assert_eq!(eas::find_all_text_after(&doc, srch::STATUS), vec!["1", "3"]);
    assert!(!eas::contains_token(&doc, srch::TOTAL));
}

#[tokio::test]
async fn contact_search_pages_and_rejects_changed_snapshots() {
    use eas::{airsync as air, contacts, search as srch};
    let env = start_env(Opts::default()).await;
    enable_groupware(&env, 215);
    let mut b = eas::DocumentBuilder::new();
    b.start(srch::SEARCH);
    b.start(srch::STORE);
    b.leaf(srch::NAME, "Mailbox");
    b.start(srch::QUERY);
    b.start(srch::AND);
    b.leaf(air::CLASS, "Contacts");
    b.leaf(srch::FREE_TEXT, "CONTACT");
    b.end();
    b.end();
    b.start(srch::OPTIONS);
    b.leaf(srch::RANGE, "5-7");
    b.end();
    b.end();
    b.end();
    let request = wbxml::encode_document(&b.finish());
    let search = || env.req("Search", "client").body(request.clone()).send();
    let response = search().await.unwrap();
    let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
    assert_eq!(eas::find_all_text_after(&doc, srch::STATUS), vec!["1", "1"]);
    assert_eq!(eas::find_text_after(&doc, srch::TOTAL), Some("215"));
    assert_eq!(eas::find_text_after(&doc, srch::RANGE), Some("5-7"));
    assert_eq!(
        eas::find_all_text_after(&doc, contacts::FILE_AS),
        vec!["Contact 5", "Contact 6", "Contact 7"]
    );
    env.mock.snapshot_changes.store(true, Ordering::SeqCst);
    let response = search().await.unwrap();
    let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
    assert_eq!(eas::find_all_text_after(&doc, srch::STATUS), vec!["1", "3"]);
    assert!(!eas::contains_token(&doc, srch::TOTAL));
}

#[tokio::test]
async fn contact_search_combines_and_or_terms() {
    use eas::{airsync as air, contacts, search as srch};
    let env = start_env(Opts::default()).await;
    enable_groupware(&env, 3);
    {
        let mut all = env.mock.groupware.lock().unwrap();
        let items = all.get_mut("contacts").unwrap();
        items[0]["name"]["full"] = serde_json::json!("Ada Blue");
        items[1]["name"]["full"] = serde_json::json!("Ada Green");
        items[2]["name"]["full"] = serde_json::json!("Bob Blue");
    }
    let mut b = eas::DocumentBuilder::new();
    b.start(srch::SEARCH);
    b.start(srch::STORE);
    b.leaf(srch::NAME, "Mailbox");
    b.start(srch::QUERY);
    b.start(srch::AND);
    b.leaf(air::CLASS, "Contacts");
    b.leaf(srch::FREE_TEXT, "ada");
    b.start(srch::OR);
    b.leaf(srch::FREE_TEXT, "BLUE");
    b.leaf(srch::FREE_TEXT, "green");
    b.end();
    b.end();
    b.end();
    b.end();
    b.end();
    let response = env
        .req("Search", "client")
        .body(wbxml::encode_document(&b.finish()))
        .send()
        .await
        .unwrap();
    let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
    assert_eq!(eas::find_all_text_after(&doc, srch::STATUS), vec!["1", "1"]);
    assert_eq!(eas::find_text_after(&doc, srch::TOTAL), Some("2"));
    assert_eq!(
        eas::find_all_text_after(&doc, contacts::FILE_AS),
        vec!["Ada Blue", "Ada Green"]
    );
}

#[tokio::test]
async fn search_and_sync_fetch_use_the_requested_body_preferences() {
    use eas::{
        airsync as air, airsync_base as base, search as srch, RequestedBodyPreference as Pref,
    };
    let env = start_env(Opts::default()).await;
    let options = requested_body_options(&[
        Pref {
            body_type: 2,
            truncation_size: Some(1),
            all_or_none: true,
            preview: Some(0),
        },
        Pref {
            body_type: 1,
            truncation_size: Some(2),
            preview: Some(1),
            ..Default::default()
        },
    ]);
    let mut b = eas::DocumentBuilder::new();
    b.start(srch::SEARCH);
    b.start(srch::STORE);
    b.leaf(srch::NAME, "Mailbox");
    b.start(srch::QUERY);
    b.start(srch::AND);
    b.leaf(air::CLASS, "Email");
    b.leaf(srch::FREE_TEXT, "subject");
    b.end();
    b.end();
    b.start(srch::OPTIONS);
    for child in eas::child_elements(&options) {
        b.extend(child.nodes);
    }
    b.end();
    b.end();
    b.end();
    let response = env
        .req("Search", "client")
        .body(wbxml::encode_document(&b.finish()))
        .send()
        .await
        .unwrap();
    let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
    assert_eq!(eas::find_all_text_after(&doc, srch::STATUS), vec!["1", "1"]);
    let data = eas::find_all_text_after(&doc, base::DATA);
    assert!(!data.is_empty());
    assert!(data.iter().all(|value| value == "bo"));
    assert!(eas::find_all_text_after(&doc, base::PREVIEW)
        .iter()
        .all(|value| value == "b"));

    let response = env
        .req("Sync", "client")
        .body(sync_body("0", "inbox"))
        .send()
        .await
        .unwrap();
    let prime = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
    let options = requested_body_options(&[Pref {
        body_type: 4,
        truncation_size: Some(5),
        ..Default::default()
    }]);
    let mut b = eas::DocumentBuilder::new();
    b.start(air::SYNC);
    b.start(air::COLLECTIONS);
    b.start(air::COLLECTION);
    b.leaf(air::SYNC_KEY, sync_key(&prime));
    b.leaf(air::COLLECTION_ID, "inbox");
    b.leaf(air::GET_CHANGES, "0");
    b.extend(options.nodes);
    b.start(air::COMMANDS);
    b.start(air::FETCH);
    b.leaf(air::SERVER_ID, "m1");
    for _ in 0..5 {
        b.end();
    }
    let response = env
        .req("Sync", "client")
        .body(wbxml::encode_document(&b.finish()))
        .send()
        .await
        .unwrap();
    let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
    assert_eq!(eas::find_all_text_after(&doc, air::STATUS), vec!["1", "1"]);
    assert_eq!(eas::find_text_after(&doc, base::TYPE), Some("4"));
    assert_eq!(body_data_bytes(&doc), Some(&MIME_BYTES[..5]));
    assert_eq!(eas::find_text_after(&doc, base::TRUNCATED), Some("1"));
}

#[tokio::test]
async fn large_text_is_completed_before_format_selection_and_all_fetch_paths() {
    use eas::{
        airsync as air, airsync_base as base, item_operations as ops, search as srch,
        RequestedBodyPreference as Pref,
    };
    let env = start_env(Opts::default()).await;
    env.mock
        .boxes
        .lock()
        .unwrap()
        .insert("inbox".to_owned(), vec!["large".to_owned()]);
    let plain = format!("{}END_AFTER_64K", "é ".repeat(40_000));
    let html = format!("<p>{plain}</p>");
    env.mock
        .mail_bodies
        .lock()
        .unwrap()
        .insert("large".to_owned(), (html.clone(), true));
    let options = requested_body_options(&[
        Pref {
            body_type: 2,
            truncation_size: Some(65_540),
            all_or_none: true,
            preview: Some(0),
        },
        Pref {
            body_type: 1,
            truncation_size: Some(4),
            preview: Some(3),
            ..Default::default()
        },
    ]);
    let response = env
        .req("Sync", "client")
        .body(sync_body_preferences("0", "inbox", Some(&options), true))
        .send()
        .await
        .unwrap();
    let prime = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
    let response = env
        .req("Sync", "client")
        .body(sync_body_preferences(
            &sync_key(&prime),
            "inbox",
            None,
            true,
        ))
        .send()
        .await
        .unwrap();
    let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
    assert_eq!(eas::find_text_after(&doc, base::TYPE), Some("1"));
    assert_eq!(eas::find_text_after(&doc, base::DATA), Some("é "));
    assert_eq!(eas::find_text_after(&doc, base::PREVIEW), Some("é é"));
    assert_eq!(
        eas::find_text_after(&doc, base::ESTIMATED_DATA_SIZE),
        Some(plain.len().to_string()).as_deref()
    );
    assert!(env
        .mock
        .body_get_limits
        .lock()
        .unwrap()
        .iter()
        .any(|(id, limit)| id == "large" && *limit == 0));
    let key = sync_key(&doc);

    let fetch = |options: Option<wbxml::Document>| {
        let mut b = eas::DocumentBuilder::new();
        b.start(ops::ITEM_OPERATIONS);
        b.start(ops::FETCH);
        b.leaf(ops::STORE, "Mailbox");
        b.leaf(air::COLLECTION_ID, "inbox");
        b.leaf(air::SERVER_ID, "large");
        if let Some(options) = options {
            b.start(ops::OPTIONS);
            for child in eas::child_elements(&options) {
                b.extend(child.nodes);
            }
            b.end();
        }
        b.end();
        b.end();
        wbxml::encode_document(&b.finish())
    };
    let response = env
        .req("ItemOperations", "client")
        .body(fetch(None))
        .send()
        .await
        .unwrap();
    let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
    assert_eq!(eas::find_text_after(&doc, base::DATA), Some(html.as_str()));
    assert_eq!(eas::find_text_after(&doc, base::TRUNCATED), Some("0"));
    let options = requested_body_options(&[Pref {
        body_type: 2,
        truncation_size: Some(65_540),
        all_or_none: true,
        preview: Some(0),
    }]);
    let response = env
        .req("ItemOperations", "client")
        .body(fetch(Some(options)))
        .send()
        .await
        .unwrap();
    let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
    assert!(!eas::contains_token(&doc, base::DATA));
    assert_eq!(
        eas::find_text_after(&doc, base::ESTIMATED_DATA_SIZE),
        Some(html.len().to_string()).as_deref()
    );
    assert_eq!(eas::find_text_after(&doc, base::TRUNCATED), Some("1"));

    let options = requested_body_options(&[Pref {
        body_type: 1,
        preview: Some(0),
        ..Default::default()
    }]);
    let mut b = eas::DocumentBuilder::new();
    b.start(air::SYNC);
    b.start(air::COLLECTIONS);
    b.start(air::COLLECTION);
    b.leaf(air::SYNC_KEY, key);
    b.leaf(air::COLLECTION_ID, "inbox");
    b.leaf(air::GET_CHANGES, "0");
    b.extend(options.nodes);
    b.start(air::COMMANDS);
    b.start(air::FETCH);
    b.leaf(air::SERVER_ID, "large");
    for _ in 0..5 {
        b.end();
    }
    let response = env
        .req("Sync", "client")
        .body(wbxml::encode_document(&b.finish()))
        .send()
        .await
        .unwrap();
    let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
    assert_eq!(eas::find_text_after(&doc, base::DATA), Some(plain.as_str()));
    assert_eq!(eas::find_text_after(&doc, base::TRUNCATED), Some("0"));

    let mut b = eas::DocumentBuilder::new();
    b.start(srch::SEARCH);
    b.start(srch::STORE);
    b.leaf(srch::NAME, "Mailbox");
    b.start(srch::QUERY);
    b.leaf(srch::FREE_TEXT, "large");
    b.end();
    b.start(srch::OPTIONS);
    let options = requested_body_options(&[Pref {
        body_type: 2,
        preview: Some(0),
        ..Default::default()
    }]);
    for child in eas::child_elements(&options) {
        b.extend(child.nodes);
    }
    b.end();
    b.end();
    b.end();
    let response = env
        .req("Search", "client")
        .body(wbxml::encode_document(&b.finish()))
        .send()
        .await
        .unwrap();
    let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
    assert_eq!(eas::find_text_after(&doc, base::DATA), Some(html.as_str()));
    assert_eq!(eas::find_text_after(&doc, base::TRUNCATED), Some("0"));
}

#[tokio::test]
async fn failed_or_incomplete_full_text_fetch_does_not_acknowledge_the_message() {
    use eas::{airsync as air, airsync_base as base, RequestedBodyPreference as Pref};
    let env = start_env(Opts::default()).await;
    env.mock
        .boxes
        .lock()
        .unwrap()
        .insert("inbox".to_owned(), vec!["large".to_owned()]);
    env.mock
        .mail_bodies
        .lock()
        .unwrap()
        .insert("large".to_owned(), ("x".repeat(100_000), false));
    let options = requested_body_options(&[Pref {
        body_type: 1,
        truncation_size: Some(2),
        ..Default::default()
    }]);
    let response = env
        .req("Sync", "client")
        .body(sync_body_preferences("0", "inbox", Some(&options), true))
        .send()
        .await
        .unwrap();
    let prime = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
    let key = sync_key(&prime);
    let body = sync_body_preferences(&key, "inbox", None, true);
    env.mock.body_get_error.store(true, Ordering::SeqCst);
    for incomplete in [false, true] {
        if incomplete {
            env.mock.body_get_error.store(false, Ordering::SeqCst);
            env.mock.body_get_incomplete.store(true, Ordering::SeqCst);
        }
        let response = env
            .req("Sync", "client")
            .body(body.clone())
            .send()
            .await
            .unwrap();
        let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
        assert_eq!(eas::find_text_after(&doc, air::STATUS), Some("5"));
        assert_eq!(sync_key(&doc), key);
        assert!(env
            .state
            .get("u@test", "client", "inbox")
            .await
            .unwrap()
            .unwrap()
            .seen_ids
            .is_empty());
    }
    env.mock.body_get_incomplete.store(false, Ordering::SeqCst);
    let response = env
        .req("Sync", "client")
        .body(body.clone())
        .send()
        .await
        .unwrap();
    let first = response.bytes().await.unwrap();
    let doc = wbxml::decode_document(&first).unwrap();
    assert_eq!(eas::find_text_after(&doc, base::DATA), Some("xx"));
    assert_eq!(
        eas::find_text_after(&doc, base::ESTIMATED_DATA_SIZE),
        Some("100000")
    );
    let calls = env.mock.body_get_limits.lock().unwrap().len();
    let response = env.req("Sync", "client").body(body).send().await.unwrap();
    assert_eq!(response.bytes().await.unwrap(), first);
    assert_eq!(env.mock.body_get_limits.lock().unwrap().len(), calls);
}

fn mime_options(mode: Option<&str>, limit: usize) -> wbxml::Document {
    use eas::{airsync as air, airsync_base as base};
    let mut b = eas::DocumentBuilder::new();
    b.start(air::OPTIONS);
    if let Some(mode) = mode {
        b.leaf(air::MIME_SUPPORT, mode);
    }
    b.start(base::BODY_PREFERENCE);
    b.leaf(base::TYPE, "4");
    b.leaf(base::TRUNCATION_SIZE, limit.to_string());
    b.end();
    b.end();
    b.finish()
}

#[tokio::test]
async fn mime_policy_controls_sync_fetch_and_search_without_cross_request_leaks() {
    use eas::{
        airsync as air, airsync_base as base, email as mail, item_operations as ops, search as srch,
    };
    let env = start_env(Opts::default()).await;
    env.mock
        .boxes
        .lock()
        .unwrap()
        .insert("inbox".to_owned(), vec!["m1".to_owned()]);
    for structure in [
        serde_json::json!({"type":"text/plain","subParts":null}),
        serde_json::json!({"type":"application/pkcs7-mime"}),
        serde_json::json!({"type":"multipart/signed","subParts":[
            {"type":"text/plain"}, {"type":"application/pkcs7-signature"}]}),
    ] {
        let smime = structure["type"] != "text/plain";
        let class = match structure["type"].as_str().unwrap() {
            "text/plain" => "IPM.Note",
            "multipart/signed" => "IPM.Note.SMIME.MultipartSigned",
            _ => "IPM.Note.SMIME",
        };
        env.mock
            .mail_structures
            .lock()
            .unwrap()
            .insert("m1".to_owned(), structure);
        for version in ["14.0", "16.1"] {
            for mode in [None, Some("0"), Some("1"), Some("2")] {
                let mime = mode == Some("2") || (mode == Some("1") && smime);
                let options = mime_options(mode, 2);
                let device = format!("mime-{class}-{mode:?}-{version}");
                let response = env
                    .req("Sync", &device)
                    .header("MS-ASProtocolVersion", version)
                    .body(sync_body_preferences("0", "inbox", Some(&options), true))
                    .send()
                    .await
                    .unwrap();
                let prime = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
                let response = env
                    .req("Sync", &device)
                    .header("MS-ASProtocolVersion", version)
                    .body(sync_body_preferences(
                        &sync_key(&prime),
                        "inbox",
                        None,
                        true,
                    ))
                    .send()
                    .await
                    .unwrap();
                let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
                assert_eq!(
                    eas::find_text_after(&doc, base::TYPE),
                    Some(if mime { "4" } else { "1" })
                );
                assert_eq!(eas::find_text_after(&doc, mail::MESSAGE_CLASS), Some(class));
                if mime {
                    assert_eq!(body_data_bytes(&doc), Some(&MIME_BYTES[..2]));
                } else {
                    assert_eq!(eas::find_text_after(&doc, base::DATA), Some("bo"));
                }
                for command in ["ItemOperations", "Search"] {
                    let mut b = eas::DocumentBuilder::new();
                    if command == "ItemOperations" {
                        b.start(ops::ITEM_OPERATIONS);
                        b.start(ops::FETCH);
                        b.leaf(ops::STORE, "Mailbox");
                        b.leaf(air::COLLECTION_ID, "inbox");
                        b.leaf(air::SERVER_ID, "m1");
                        b.start(ops::OPTIONS);
                    } else {
                        b.start(srch::SEARCH);
                        b.start(srch::STORE);
                        b.leaf(srch::NAME, "Mailbox");
                        b.start(srch::QUERY);
                        b.leaf(srch::FREE_TEXT, "subject");
                        // MIMESupport outside Options must not affect policy.
                        b.leaf(air::MIME_SUPPORT, "2");
                        b.end();
                        b.start(srch::OPTIONS);
                    }
                    for child in eas::child_elements(&options) {
                        b.extend(child.nodes);
                    }
                    b.end();
                    b.end();
                    b.end();
                    let response = env
                        .req(command, "mime-policy")
                        .header("MS-ASProtocolVersion", version)
                        .body(wbxml::encode_document(&b.finish()))
                        .send()
                        .await
                        .unwrap();
                    let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
                    assert_eq!(
                        eas::find_text_after(&doc, base::TYPE),
                        Some(if mime { "4" } else { "1" })
                    );
                    assert_eq!(eas::find_text_after(&doc, mail::MESSAGE_CLASS), Some(class));
                }
            }
        }
    }
}

#[test]
fn mime_policy_rejects_empty_duplicate_and_out_of_range_options() {
    use eas::airsync as air;
    for values in [vec![""], vec!["3"], vec!["-1"], vec!["2", "1"]] {
        let mut b = eas::DocumentBuilder::new();
        b.start(air::OPTIONS);
        for value in values {
            b.leaf(air::MIME_SUPPORT, value);
        }
        b.end();
        assert!(eas::mime_support(&b.finish()).is_err());
    }
}

#[tokio::test]
async fn invalid_mime_options_preserve_saved_policy_and_replacement_resets_it() {
    use eas::{airsync as air, airsync_base as base};
    let env = start_env(Opts::default()).await;
    let options = mime_options(Some("2"), 5);
    let response = env
        .req("Sync", "policy-replacement")
        .body(sync_body_preferences("0", "inbox", Some(&options), true))
        .send()
        .await
        .unwrap();
    let prime = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
    let mut key = sync_key(&prime);
    let env = env.restarted().await;
    let fetch = |key: &str, options: Option<wbxml::Document>| {
        let mut b = eas::DocumentBuilder::new();
        b.start(air::SYNC);
        b.start(air::COLLECTIONS);
        b.start(air::COLLECTION);
        b.leaf(air::SYNC_KEY, key);
        b.leaf(air::COLLECTION_ID, "inbox");
        b.leaf(air::GET_CHANGES, "0");
        if let Some(options) = options {
            b.extend(options.nodes);
        }
        b.start(air::COMMANDS);
        b.start(air::FETCH);
        b.leaf(air::SERVER_ID, "m1");
        for _ in 0..5 {
            b.end();
        }
        wbxml::encode_document(&b.finish())
    };
    for (options, status, mime) in [
        (Some(mime_options(Some("3"), 5)), "4", false),
        (None, "1", true),
        (Some(mime_options(None, 2)), "1", false),
        (None, "1", false),
    ] {
        let response = env
            .req("Sync", "policy-replacement")
            .body(fetch(&key, options))
            .send()
            .await
            .unwrap();
        let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
        assert_eq!(eas::find_text_after(&doc, air::STATUS), Some(status));
        key = sync_key(&doc);
        if status == "1" {
            assert_eq!(
                eas::find_text_after(&doc, base::TYPE),
                Some(if mime { "4" } else { "1" })
            );
            if mime {
                assert_eq!(body_data_bytes(&doc), Some(&MIME_BYTES[..5]));
            } else {
                assert_eq!(eas::find_text_after(&doc, base::DATA), Some("bo"));
            }
        }
    }
}

#[tokio::test]
async fn omitted_sync_window_delivers_up_to_one_hundred_messages() {
    use eas::airsync as air;
    let env = start_env(Opts::default()).await;
    for i in 0..38 {
        env.mock.add_mail("inbox", &format!("default-window-{i}"));
    }
    let body = |key: &str| {
        let mut b = eas::DocumentBuilder::new();
        b.start(air::SYNC);
        b.start(air::COLLECTIONS);
        b.start(air::COLLECTION);
        b.leaf(air::SYNC_KEY, key);
        b.leaf(air::COLLECTION_ID, "inbox");
        b.empty_tag(air::GET_CHANGES);
        b.end();
        b.end();
        b.end();
        wbxml::encode_document(&b.finish())
    };
    let response = env
        .req("Sync", "default-window")
        .body(body("0"))
        .send()
        .await
        .unwrap();
    let prime = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
    let response = env
        .req("Sync", "default-window")
        .body(body(&sync_key(&prime)))
        .send()
        .await
        .unwrap();
    let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
    assert_eq!(eas::find_all_text_after(&doc, air::SERVER_ID).len(), 40);
    assert!(!eas::contains_token(&doc, air::MORE_AVAILABLE));
}

#[tokio::test]
#[ignore]
async fn sync_heartbeat_timeout_is_empty_and_cached_request_receives_later_change() {
    use eas::airsync as air;
    let env = start_env(Opts::default()).await;
    let response = env
        .req("Sync", "heartbeat-timeout")
        .body(sync_body("0", "inbox"))
        .send()
        .await
        .unwrap();
    let prime = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
    let response = env
        .req("Sync", "heartbeat-timeout")
        .body(sync_body(&sync_key(&prime), "inbox"))
        .send()
        .await
        .unwrap();
    let synced = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
    let mut b = eas::DocumentBuilder::new();
    b.start(air::SYNC);
    b.leaf(air::HEARTBEAT_INTERVAL, "60");
    b.start(air::COLLECTIONS);
    b.start(air::COLLECTION);
    b.leaf(air::SYNC_KEY, sync_key(&synced));
    b.leaf(air::COLLECTION_ID, "inbox");
    b.empty_tag(air::GET_CHANGES);
    b.end();
    b.end();
    b.end();
    let start = tokio::time::Instant::now();
    let response = tokio::time::timeout(
        Duration::from_secs(75),
        env.req("Sync", "heartbeat-timeout")
            .body(wbxml::encode_document(&b.finish()))
            .send(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(response.status(), 200);
    assert!(start.elapsed() >= Duration::from_secs(59));
    assert!(response.bytes().await.unwrap().is_empty());
    env.mock.add_mail("inbox", "after-heartbeat-timeout");
    let response = tokio::time::timeout(
        Duration::from_secs(5),
        env.req("Sync", "heartbeat-timeout").send(),
    )
    .await
    .unwrap()
    .unwrap();
    let reply = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
    assert!(eas::find_all_text_after(&reply, air::SERVER_ID)
        .contains(&"after-heartbeat-timeout".to_owned()));
}

#[tokio::test]
async fn overlapping_sync_commands_do_not_reuse_a_sync_key() {
    use eas::{airsync as air, email};
    let env = start_env(Opts::default()).await;
    let prime = env
        .req("Sync", "overlap")
        .body(sync_body("0", "inbox"))
        .send()
        .await
        .unwrap();
    let prime = wbxml::decode_document(&prime.bytes().await.unwrap()).unwrap();
    let synced = env
        .req("Sync", "overlap")
        .body(sync_body(&sync_key(&prime), "inbox"))
        .send()
        .await
        .unwrap();
    let synced = wbxml::decode_document(&synced.bytes().await.unwrap()).unwrap();
    let key = sync_key(&synced);
    let change = |read: &str| {
        let mut b = eas::DocumentBuilder::new();
        b.start(air::SYNC);
        b.start(air::COLLECTIONS);
        b.start(air::COLLECTION);
        b.leaf(air::SYNC_KEY, &key);
        b.leaf(air::COLLECTION_ID, "inbox");
        b.start(air::COMMANDS);
        b.start(air::CHANGE);
        b.leaf(air::SERVER_ID, "m1");
        b.start(air::APPLICATION_DATA);
        b.leaf(email::READ, read);
        for _ in 0..6 {
            b.end();
        }
        wbxml::encode_document(&b.finish())
    };
    env.mock.delay_mail_set.store(true, Ordering::SeqCst);
    let first = tokio::spawn(env.req("Sync", "overlap").body(change("1")).send());
    tokio::time::timeout(Duration::from_secs(2), async {
        while env.mock.delayed_mail_sets.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let second = tokio::spawn(env.req("Sync", "overlap").body(change("0")).send());
    let first =
        wbxml::decode_document(&first.await.unwrap().unwrap().bytes().await.unwrap()).unwrap();
    let second =
        wbxml::decode_document(&second.await.unwrap().unwrap().bytes().await.unwrap()).unwrap();
    assert_eq!(eas::find_text_after(&first, air::STATUS), Some("1"));
    assert_eq!(eas::find_text_after(&second, air::STATUS), Some("3"));
    assert!(env
        .mock
        .keywords
        .lock()
        .unwrap()
        .get("m1")
        .unwrap()
        .contains("$seen"));
    assert_eq!(env.mock.delayed_mail_sets.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn waiting_sync_does_not_block_a_newer_sync() {
    use eas::airsync as air;
    let env = start_env(Opts::default()).await;
    let prime = env
        .req("Sync", "waiting")
        .body(sync_body("0", "inbox"))
        .send()
        .await
        .unwrap();
    let prime = wbxml::decode_document(&prime.bytes().await.unwrap()).unwrap();
    let synced = env
        .req("Sync", "waiting")
        .body(sync_body(&sync_key(&prime), "inbox"))
        .send()
        .await
        .unwrap();
    let synced = wbxml::decode_document(&synced.bytes().await.unwrap()).unwrap();
    let key = sync_key(&synced);
    let mut b = eas::DocumentBuilder::new();
    b.start(air::SYNC);
    b.leaf(air::HEARTBEAT_INTERVAL, "60");
    b.start(air::COLLECTIONS);
    b.start(air::COLLECTION);
    b.leaf(air::SYNC_KEY, &key);
    b.leaf(air::COLLECTION_ID, "inbox");
    b.empty_tag(air::GET_CHANGES);
    b.end();
    b.end();
    b.end();
    let pending = tokio::spawn(
        env.req("Sync", "waiting")
            .body(wbxml::encode_document(&b.finish()))
            .send(),
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!pending.is_finished());
    let response = tokio::time::timeout(
        Duration::from_secs(2),
        env.req("Sync", "waiting")
            .body(sync_body(&key, "inbox"))
            .send(),
    )
    .await
    .unwrap()
    .unwrap();
    let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
    assert_eq!(eas::find_text_after(&doc, air::STATUS), Some("1"));
    env.mock.add_mail("inbox", "after-overlap");
    env.mock.emit_email_change();
    let response = tokio::time::timeout(Duration::from_secs(5), pending)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
    assert!(eas::find_all_text_after(&doc, air::SERVER_ID).contains(&"after-overlap".to_owned()));
}

#[tokio::test]
async fn sync_heartbeat_waits_for_new_mail_and_delivers_it() {
    use eas::airsync as air;
    let env = start_env(Opts::default()).await;
    let response = env
        .req("Sync", "sync-wait")
        .body(sync_body("0", "inbox"))
        .send()
        .await
        .unwrap();
    let prime = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
    let response = env
        .req("Sync", "sync-wait")
        .body(sync_body(&sync_key(&prime), "inbox"))
        .send()
        .await
        .unwrap();
    let synced = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
    let mut b = eas::DocumentBuilder::new();
    b.start(air::SYNC);
    b.leaf(air::HEARTBEAT_INTERVAL, "60");
    b.start(air::COLLECTIONS);
    b.start(air::COLLECTION);
    b.leaf(air::SYNC_KEY, sync_key(&synced));
    b.leaf(air::COLLECTION_ID, "inbox");
    b.empty_tag(air::GET_CHANGES);
    b.end();
    b.end();
    b.end();
    let request = env
        .req("Sync", "sync-wait")
        .body(wbxml::encode_document(&b.finish()))
        .send();
    tokio::pin!(request);
    tokio::select! {
        response = &mut request => panic!("Sync returned before a change: {:?}", response.unwrap().status()),
        _ = tokio::time::sleep(Duration::from_millis(500)) => {}
    }
    env.mock.add_mail("other", "unrelated-new");
    env.mock.emit_email_change();
    tokio::select! {
        response = &mut request => panic!("Unrelated mail ended Sync: {:?}", response.unwrap().status()),
        _ = tokio::time::sleep(Duration::from_millis(300)) => {}
    }
    env.mock.add_mail("inbox", "heartbeat-new");
    env.mock.emit_email_change();
    let response = tokio::time::timeout(Duration::from_secs(5), &mut request)
        .await
        .unwrap()
        .unwrap();
    let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
    assert!(eas::find_all_text_after(&doc, air::SERVER_ID).contains(&"heartbeat-new".to_owned()));
}

#[tokio::test]
async fn partial_sync_replaces_the_previous_wait_setting() {
    use eas::airsync as air;
    let env = start_env(Opts::default()).await;
    let request = |partial: bool| {
        let mut b = eas::DocumentBuilder::new();
        b.start(air::SYNC);
        if partial {
            b.empty_tag(air::PARTIAL);
            b.leaf(air::HEARTBEAT_INTERVAL, "60");
        } else {
            b.leaf(air::WAIT, "1");
        }
        b.start(air::COLLECTIONS);
        b.start(air::COLLECTION);
        b.leaf(air::SYNC_KEY, "0");
        b.leaf(air::COLLECTION_ID, "inbox");
        b.end();
        b.end();
        b.end();
        wbxml::encode_document(&b.finish())
    };
    for partial in [false, true] {
        let response = env
            .req("Sync", "replace-wait")
            .body(request(partial))
            .send()
            .await
            .unwrap();
        let doc = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
        assert_eq!(eas::find_text_after(&doc, air::STATUS), Some("1"));
    }
}

#[tokio::test]
async fn jmap_item_errors_do_not_acknowledge_mail_mutations() {
    use eas::{airsync as air, email as mail};
    let env = start_env(Opts::default()).await;
    let prime = env
        .req("Sync", "item-error")
        .body(sync_body("0", "inbox"))
        .send()
        .await
        .unwrap();
    let prime = wbxml::decode_document(&prime.bytes().await.unwrap()).unwrap();
    let synced = env
        .req("Sync", "item-error")
        .body(sync_body(&sync_key(&prime), "inbox"))
        .send()
        .await
        .unwrap();
    let synced = wbxml::decode_document(&synced.bytes().await.unwrap()).unwrap();
    let mut key = sync_key(&synced);
    for change in [true, false] {
        let mut b = eas::DocumentBuilder::new();
        b.start(air::SYNC);
        b.start(air::COLLECTIONS);
        b.start(air::COLLECTION);
        b.leaf(air::SYNC_KEY, &key);
        b.leaf(air::COLLECTION_ID, "inbox");
        b.leaf(air::GET_CHANGES, "0");
        b.leaf(air::DELETES_AS_MOVES, "0");
        b.start(air::COMMANDS);
        b.start(if change { air::CHANGE } else { air::DELETE });
        b.leaf(air::SERVER_ID, "m1");
        if change {
            b.start(air::APPLICATION_DATA);
            b.leaf(mail::READ, "1");
            b.end();
        }
        for _ in 0..5 {
            b.end();
        }
        let body = wbxml::encode_document(&b.finish());
        env.mock.item_set_error.store(true, Ordering::SeqCst);
        let response = env
            .req("Sync", "item-error")
            .body(body.clone())
            .send()
            .await
            .unwrap();
        let failed = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
        assert_eq!(eas::find_text_after(&failed, air::STATUS), Some("5"));
        assert_eq!(sync_key(&failed), key);
        assert!(env.mock.boxes.lock().unwrap()["inbox"].contains(&"m1".to_owned()));
        if change {
            assert!(!env
                .mock
                .keywords
                .lock()
                .unwrap()
                .get("m1")
                .is_some_and(|k| k.contains("$seen")));
        }
        env.mock.item_set_error.store(false, Ordering::SeqCst);
        let response = env
            .req("Sync", "item-error")
            .body(body)
            .send()
            .await
            .unwrap();
        let ok = wbxml::decode_document(&response.bytes().await.unwrap()).unwrap();
        assert_eq!(eas::find_text_after(&ok, air::STATUS), Some("1"));
        key = sync_key(&ok);
    }
    assert!(!env.mock.boxes.lock().unwrap()["inbox"].contains(&"m1".to_owned()));
}
