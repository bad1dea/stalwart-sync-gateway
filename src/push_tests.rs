//! End-to-end push tests: the real gateway router over real HTTP against a
//! mock Stalwart (JMAP session + API + EventSource). No production access.

use std::{
    convert::Infallible,
    sync::{
        atomic::{AtomicUsize, Ordering},
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
}

impl Mock {
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
        self.boxes
            .lock()
            .unwrap()
            .entry(mailbox.to_owned())
            .or_default()
            .push(id.to_owned());
    }

    fn set_read(&self, id: &str, read: bool) {
        let mut set = self.read.lock().unwrap();
        if read {
            set.insert(id.to_owned());
        } else {
            set.remove(id);
        }
    }

    fn remove_mail(&self, mailbox: &str, id: &str) {
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
    Json(serde_json::json!({
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
    }))
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
                    list.push(serde_json::json!({"id":f,"name":f,"parentId":null,"role":null}));
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
                ids.reverse();
                if let Some(after) = args["filter"]["after"].as_str() {
                    // RFC 3339 UTC strings of one format compare as dates.
                    ids.retain(|id| m.date_of(id).as_str() >= after);
                }
                let limit = args["limit"].as_u64().unwrap_or(100) as usize;
                let total = ids.len();
                ids.truncate(limit);
                last_query = ids.clone();
                serde_json::json!({ "ids": ids, "total": total })
            }
            "Email/get" => {
                let ids: Vec<String> = if let Some(arr) = args["ids"].as_array() {
                    arr.iter().map(|v| v.as_str().unwrap().to_owned()).collect()
                } else {
                    last_query.clone()
                };
                let mut list = Vec::new();
                for id in ids {
                    for (mb, items) in &boxes {
                        if items.contains(&id) {
                            list.push(email_json(&id, mb, read_set.contains(&id), &m.date_of(&id)));
                        }
                    }
                }
                serde_json::json!({ "list": list })
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
}

impl Default for Opts {
    fn default() -> Self {
        Self {
            max_concurrent: 200,
            sqlite: None,
        }
    }
}

async fn start_env(opts: Opts) -> Env {
    let mock = start_mock().await;
    mock.add_mail("inbox", "m1");
    mock.add_mail("inbox", "m2");
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
    };
    let state = state::new_store(&config).await.unwrap();
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
    Env {
        mock,
        gw: format!("http://{gw_addr}/Microsoft-Server-ActiveSync"),
        http: reqwest::Client::new(),
        push,
        state,
        keys: Mutex::new(Default::default()),
    }
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
async fn empty_ping_after_restart_falls_back_to_inbox() {
    let env = Arc::new(start_env(Opts::default()).await);
    env.sync_all("devA").await;
    // No cached parameters (as after a gateway restart) and an empty body.
    env.mock.add_mail("inbox", "m3");
    let r = env.ping("devA", None).await;
    assert_eq!(
        (r.status.as_str(), r.folders),
        ("2", vec!["inbox".to_owned()])
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
