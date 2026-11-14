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
    read: Mutex<std::collections::BTreeSet<String>>,
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
        self.log_change(id);
        let mut set = self.read.lock().unwrap();
        if read {
            set.insert(id.to_owned());
        } else {
            set.remove(id);
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

fn email_json(id: &str, mailbox: &str, read: bool, received_at: &str) -> serde_json::Value {
    serde_json::json!({
        "id": id, "blobId": format!("b-{id}"), "threadId": format!("t-{id}"),
        "mailboxIds": { mailbox: true },
        "keywords": if read { serde_json::json!({"$seen": true}) } else { serde_json::json!({}) },
        "receivedAt": received_at, "subject": format!("subject {id}"),
        "from": [{"name": "A", "email": "a@x.test"}], "to": null, "cc": null,
        "textBody": [{"partId": "1"}], "htmlBody": [],
        "bodyValues": { "1": { "value": "body" } }, "attachments": []
    })
}

async fn mock_api(
    State(m): State<Arc<Mock>>,
    Json(req): Json<serde_json::Value>,
) -> impl IntoResponse {
    let boxes = m.boxes.lock().unwrap().clone();
    let read_set = m.read.lock().unwrap().clone();
    let mut responses = Vec::new();
    let mut last_query: Vec<String> = Vec::new();
    let mut last_changes: Vec<String> = Vec::new();
    for call in req["methodCalls"].as_array().unwrap() {
        let name = call[0].as_str().unwrap();
        let args = &call[1];
        let cid = call[2].clone();
        let body = match name {
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
                serde_json::json!({ "ids": ids, "total": total })
            }
            "AddressBook/get" => {
                serde_json::json!({"list": [{"id": "book", "name": "Contacts", "isDefault": true}]})
            }
            "Calendar/get" => {
                serde_json::json!({"list": [{"id": "calendar", "name": "Calendar", "isDefault": true}]})
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
                let list: Vec<_> = items
                    .get(kind)
                    .into_iter()
                    .flatten()
                    .filter(|v| last_query.iter().any(|id| v["id"] == id.as_str()))
                    .cloned()
                    .collect();
                serde_json::json!({"list": list})
            }
            "Email/set" => {
                let mut updated = serde_json::Map::new();
                if let Some(updates) = args["update"].as_object() {
                    for (id, patch) in updates {
                        m.log_change(id);
                        let mut boxes = m.boxes.lock().unwrap();
                        for (key, value) in patch.as_object().unwrap() {
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
                            let mut email =
                                email_json(&id, mb, read_set.contains(&id), &m.date_of(&id));
                            if mb == "notes" {
                                email["keywords"] = serde_json::json!({format!("noteid-{id}"): true, "$note": true});
                            }
                            list.push(email);
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
        read: Mutex::new(Default::default()),
        extra_folders: Mutex::new(Vec::new()),
        stale_listing: Mutex::new(Vec::new()),
        hidden_folders: Mutex::new(Vec::new()),
        dates: Mutex::new(Default::default()),
        change_log: Mutex::new(Vec::new()),
        groupware: Mutex::new(Default::default()),
        snapshot_changes: AtomicBool::new(false),
    });
    let app = Router::new()
        .route("/.well-known/jmap", get(mock_session))
        .route("/api", post(mock_api))
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
        jmap: crate::jmap::client::JmapClient::new(config.clone()).unwrap(),
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
    let dir = std::env::temp_dir().join(format!("ssg-push-{}", std::process::id()));
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
    for unhandled in [
        "MeetingResponse",
        "ResolveRecipients",
        "ValidateCert",
        "Find",
    ] {
        assert!(
            !commands.split(',').any(|c| c == unhandled),
            "advertises {unhandled}"
        );
    }
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
            let body = serde_json::json!({
                "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
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
            let user = self.user.split('@').next().unwrap().to_owned();
            self.http
                .post(format!(
                    "{}?Cmd={cmd}&User={user}&DeviceId={device}&DeviceType=iPhone",
                    self.gw
                ))
                .basic_auth(&self.user, Some(&self.pass))
                .header("MS-ASProtocolVersion", "14.0")
        }

        async fn wbxml(&self, cmd: &str, device: &str, body: Vec<u8>) -> wbxml::Document {
            let resp = self.req(cmd, device).body(body).send().await.unwrap();
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
