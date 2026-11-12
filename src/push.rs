//! Push wakeups: one JMAP EventSource connection per account, shared by all
//! of that account's devices, waking pending ActiveSync Ping requests.
//!
//! Flow: Ping registers interest -> hub makes sure a subscriber task is
//! running for the account -> Stalwart sends `StateChange` events -> the
//! subscriber bumps a per-class generation counter -> waiting Pings wake and
//! answer Status 2 with the watched folders of the changed class.
//!
//! Missed-event safety: every device remembers the generation it last
//! acknowledged (a Sync start, or a Ping answer). A Ping that starts with the
//! account generation ahead of that returns immediately. Every (re)connect of
//! the subscriber also bumps the generation, since events may have been lost
//! while it was down.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use anyhow::Context;
use tokio::sync::watch;

use crate::config::Config;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Class {
    Mail,
    Contacts,
    Calendar,
}

impl Class {
    /// ActiveSync Ping `Class` text -> class. Notes live in a JMAP mailbox,
    /// so they follow mail. Unknown/empty defaults to mail.
    pub fn from_eas(class: &str) -> Option<Class> {
        match class.to_ascii_lowercase().as_str() {
            "" | "email" | "notes" => Some(Class::Mail),
            "contacts" => Some(Class::Contacts),
            "calendar" => Some(Class::Calendar),
            _ => None,
        }
    }

    fn from_jmap_type(name: &str) -> Option<Class> {
        match name {
            "Email" | "Mailbox" | "EmailDelivery" => Some(Class::Mail),
            "ContactCard" | "AddressBook" => Some(Class::Contacts),
            "CalendarEvent" | "Calendar" => Some(Class::Calendar),
            _ => None,
        }
    }
}

/// Per-device Ping bookkeeping (in memory; ActiveSync Ping parameters may be
/// omitted by the client on later requests and must then be reused).
#[derive(Default, Clone)]
pub struct DeviceState {
    pub heartbeat: Option<u64>,
    pub folders: Vec<(String, String)>,
    last_status2: Option<Instant>,
    synced_since_status2: bool,
    /// Newest Ping request number for this device; older ones are stale.
    ping_epoch: u64,
    /// Folder id -> Status 2 answers naming it since the device last synced
    /// that folder. iOS blacklists a folder from push after more than 5.
    announced: HashMap<String, u32>,
}

#[derive(Default)]
struct Inner {
    generation: u64,
    class_generation: HashMap<Class, u64>,
    devices: HashMap<String, DeviceState>,
    last_event_at: Option<Instant>,
    /// (account, type) -> last state string, to drop repeated states.
    last_state: HashMap<(String, String), String>,
    event_source_url: Option<String>,
    authorization: Option<(String, String)>,
    running: bool,
    last_activity: Option<Instant>,
}

pub struct UserPush {
    inner: Mutex<Inner>,
    tx: watch::Sender<u64>,
    supersede_tx: watch::Sender<u64>,
    waiters: AtomicUsize,
}

#[derive(Clone)]
pub struct PushHub {
    users: Arc<Mutex<HashMap<String, Arc<UserPush>>>>,
    http: reqwest::Client,
    idle: Duration,
    active_pings: Arc<AtomicUsize>,
    max_pings: usize,
}

pub struct PingSlot(Arc<AtomicUsize>);

impl Drop for PingSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

pub struct WaiterGuard(Arc<UserPush>);

impl Drop for WaiterGuard {
    fn drop(&mut self) {
        self.0.waiters.fetch_sub(1, Ordering::SeqCst);
        self.0.touch();
    }
}

impl UserPush {
    fn touch(&self) {
        self.inner.lock().unwrap().last_activity = Some(Instant::now());
    }

    fn bump(&self, classes: &[Class]) {
        let mut inner = self.inner.lock().unwrap();
        inner.generation += 1;
        let generation = inner.generation;
        inner.last_event_at = Some(Instant::now());
        for class in classes {
            inner.class_generation.insert(*class, generation);
        }
        drop(inner);
        self.tx.send_replace(generation);
    }

    #[cfg(test)]
    pub fn waiter_count(&self) -> usize {
        self.waiters.load(Ordering::SeqCst)
    }

    pub fn generation(&self) -> u64 {
        self.inner.lock().unwrap().generation
    }

    /// A Sync from `device` started: it will see everything delivered so far.
    /// Returns the time since the last Status 2 sent to it, if that Status 2
    /// has not been followed by a Sync yet (for ping->sync latency logs).
    pub fn mark_synced(&self, device: &str) -> Option<Duration> {
        let mut inner = self.inner.lock().unwrap();
        inner.last_activity = Some(Instant::now());
        let d = inner.devices.entry(device.to_owned()).or_default();
        let latency = match (d.last_status2, d.synced_since_status2) {
            (Some(at), false) => Some(at.elapsed()),
            _ => None,
        };
        d.synced_since_status2 = true;
        latency
    }

    pub fn device(&self, device: &str) -> DeviceState {
        self.inner
            .lock()
            .unwrap()
            .devices
            .get(device)
            .cloned()
            .unwrap_or_default()
    }

    pub fn set_device_params(&self, device: &str, heartbeat: u64, folders: Vec<(String, String)>) {
        let mut inner = self.inner.lock().unwrap();
        let d = inner.devices.entry(device.to_owned()).or_default();
        d.heartbeat = Some(heartbeat);
        d.folders = folders;
    }

    /// Record a Status 2 answer. Returns event->answer latency if known.
    pub fn note_status2(&self, device: &str) -> Option<Duration> {
        let mut inner = self.inner.lock().unwrap();
        let latency = inner.last_event_at.map(|t| t.elapsed());
        let d = inner.devices.entry(device.to_owned()).or_default();
        d.last_status2 = Some(Instant::now());
        d.synced_since_status2 = false;
        latency
    }

    /// Count a Status 2 answer for each of `folders`.
    pub fn note_announced(&self, device: &str, folders: &[String]) {
        let mut inner = self.inner.lock().unwrap();
        let d = inner.devices.entry(device.to_owned()).or_default();
        for f in folders {
            *d.announced.entry(f.clone()).or_default() += 1;
        }
    }

    /// The device synced `folder`: it may be told about it again.
    pub fn folder_synced(&self, device: &str, folder: &str) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(d) = inner.devices.get_mut(device) {
            d.announced.remove(folder);
        }
    }

    /// False once `folder` was announced `cap` times without being synced.
    pub fn announce_allowed(&self, device: &str, folder: &str, cap: u32) -> bool {
        let inner = self.inner.lock().unwrap();
        inner
            .devices
            .get(device)
            .and_then(|d| d.announced.get(folder))
            .is_none_or(|n| *n < cap)
    }

    /// Time left in the loop-protection window, if a Status 2 went to this
    /// device within `window` and no Sync has followed.
    pub fn status2_guard_remaining(&self, device: &str, window: Duration) -> Option<Duration> {
        let inner = self.inner.lock().unwrap();
        let d = inner.devices.get(device)?;
        if d.synced_since_status2 {
            return None;
        }
        let elapsed = d.last_status2?.elapsed();
        window.checked_sub(elapsed).filter(|left| !left.is_zero())
    }

    /// True when a Status 2 went to this device in the last `window` and no
    /// Sync followed: an immediate re-answer would just loop.
    pub fn status2_pending_sync(&self, device: &str, window: Duration) -> bool {
        let inner = self.inner.lock().unwrap();
        inner.devices.get(device).is_some_and(|d| {
            !d.synced_since_status2 && d.last_status2.is_some_and(|t| t.elapsed() < window)
        })
    }

    /// Classes whose generation is newer than `baseline`.
    pub fn changed_since(&self, baseline: u64) -> Vec<Class> {
        let inner = self.inner.lock().unwrap();
        inner
            .class_generation
            .iter()
            .filter(|(_, g)| **g > baseline)
            .map(|(c, _)| *c)
            .collect()
    }

    /// Register a new Ping for `device`; any older pending Ping of the same
    /// device is stale (ActiveSync allows one outstanding Ping per device,
    /// and a client that opens a new one has abandoned the old, though a
    /// reverse proxy may not have told us).
    pub fn begin_ping(&self, device: &str) -> u64 {
        let epoch = {
            let mut inner = self.inner.lock().unwrap();
            let d = inner.devices.entry(device.to_owned()).or_default();
            d.ping_epoch += 1;
            d.ping_epoch
        };
        self.supersede_tx.send_modify(|n| *n += 1);
        epoch
    }

    pub fn is_superseded(&self, device: &str, epoch: u64) -> bool {
        self.inner
            .lock()
            .unwrap()
            .devices
            .get(device)
            .is_some_and(|d| d.ping_epoch != epoch)
    }

    pub fn subscribe_supersede(&self) -> watch::Receiver<u64> {
        self.supersede_tx.subscribe()
    }

    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.tx.subscribe()
    }

    pub fn waiter(self: &Arc<Self>) -> WaiterGuard {
        self.waiters.fetch_add(1, Ordering::SeqCst);
        self.touch();
        WaiterGuard(self.clone())
    }
}

impl PushHub {
    pub fn new(config: &Config) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent("stalwart-sync-gateway/0.1 push")
            .danger_accept_invalid_certs(!config.stalwart_tls_verify)
            .connect_timeout(Duration::from_secs(10))
            // No total timeout: the EventSource response is open-ended.
            .build()
            .context("failed to build push HTTP client")?;
        Ok(Self {
            users: Arc::new(Mutex::new(HashMap::new())),
            http,
            idle: Duration::from_secs(config.push_idle_seconds),
            active_pings: Arc::new(AtomicUsize::new(0)),
            max_pings: config.ping_max_concurrent,
        })
    }

    /// Reserve one of the global concurrent-Ping slots; None when full.
    pub fn try_ping_slot(&self) -> Option<PingSlot> {
        let n = self.active_pings.fetch_add(1, Ordering::SeqCst);
        if n >= self.max_pings {
            self.active_pings.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        Some(PingSlot(self.active_pings.clone()))
    }

    pub fn user(&self, key: &str) -> Arc<UserPush> {
        let mut users = self.users.lock().unwrap();
        users
            .entry(key.to_owned())
            .or_insert_with(|| {
                let (tx, _) = watch::channel(0);
                let (supersede_tx, _) = watch::channel(0);
                Arc::new(UserPush {
                    inner: Mutex::new(Inner::default()),
                    tx,
                    supersede_tx,
                    waiters: AtomicUsize::new(0),
                })
            })
            .clone()
    }

    /// Make sure a subscriber runs for this user, using the latest
    /// credentials. `event_source_url` is the session's (rebased) template.
    pub fn ensure_subscriber(
        &self,
        key: &str,
        event_source_url: Option<&str>,
        username: &str,
        password: &str,
    ) -> Arc<UserPush> {
        let user = self.user(key);
        let Some(url) = event_source_url else {
            return user;
        };
        let start = {
            let mut inner = user.inner.lock().unwrap();
            inner.event_source_url = Some(url.to_owned());
            inner.authorization = Some((username.to_owned(), password.to_owned()));
            inner.last_activity = Some(Instant::now());
            let start = !inner.running;
            inner.running = true;
            start
        };
        if start {
            let hub = self.clone();
            let user = user.clone();
            let key = key.to_owned();
            tokio::spawn(async move { hub.run_subscriber(key, user).await });
        }
        user
    }

    async fn run_subscriber(self, key: String, user: Arc<UserPush>) {
        tracing::info!(account = %key, "push: EventSource subscriber started");
        let mut backoff = Duration::from_secs(2);
        loop {
            if self.should_stop(&user) {
                break;
            }
            match self.stream_once(&key, &user).await {
                StreamEnd::Unauthorized => {
                    tracing::warn!(account = %key, "push: EventSource rejected credentials, stopping");
                    break;
                }
                StreamEnd::Idle => break,
                StreamEnd::Ended(connected) => {
                    if connected {
                        backoff = Duration::from_secs(2);
                    }
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(60));
                }
            }
        }
        user.inner.lock().unwrap().running = false;
        tracing::info!(account = %key, "push: EventSource subscriber stopped");
    }

    fn should_stop(&self, user: &UserPush) -> bool {
        if user.waiters.load(Ordering::SeqCst) > 0 {
            return false;
        }
        let inner = user.inner.lock().unwrap();
        match inner.last_activity {
            Some(at) => at.elapsed() > self.idle,
            None => true,
        }
    }

    async fn stream_once(&self, key: &str, user: &Arc<UserPush>) -> StreamEnd {
        let (template, auth) = {
            let inner = user.inner.lock().unwrap();
            (inner.event_source_url.clone(), inner.authorization.clone())
        };
        let (Some(template), Some((username, password))) = (template, auth) else {
            return StreamEnd::Idle;
        };
        let url = template
            .replace("{types}", "*")
            .replace("{closeafter}", "no")
            .replace("{ping}", "30");
        let response = match self
            .http
            .get(&url)
            .basic_auth(&username, Some(&password))
            .header("Accept", "text/event-stream")
            .send()
            .await
        {
            Ok(r) => r,
            Err(error) => {
                tracing::warn!(account = %key, %error, "push: EventSource connect failed");
                return StreamEnd::Ended(false);
            }
        };
        if matches!(
            response.status(),
            reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
        ) {
            return StreamEnd::Unauthorized;
        }
        if !response.status().is_success() {
            tracing::warn!(account = %key, status = %response.status(), "push: EventSource bad status");
            return StreamEnd::Ended(false);
        }

        tracing::info!(account = %key, "push: EventSource connected");
        // Events may have been lost while disconnected: make waiting/next
        // Pings re-check.
        user.bump(&[Class::Mail, Class::Contacts, Class::Calendar]);

        let mut response = response;
        let mut buffer = String::new();
        loop {
            if self.should_stop(user) {
                return StreamEnd::Idle;
            }
            // Server pings every 30s; 3 missed pings = dead connection.
            match tokio::time::timeout(Duration::from_secs(100), response.chunk()).await {
                Ok(Ok(Some(chunk))) => {
                    buffer.push_str(&String::from_utf8_lossy(&chunk).replace("\r\n", "\n"));
                    while let Some(pos) = buffer.find("\n\n") {
                        let block: String = buffer.drain(..pos + 2).collect();
                        handle_event_block(&block, user);
                    }
                    if buffer.len() > 1 << 20 {
                        buffer.clear();
                    }
                }
                Ok(Ok(None)) => {
                    tracing::info!(account = %key, "push: EventSource closed by server");
                    return StreamEnd::Ended(true);
                }
                Ok(Err(error)) => {
                    tracing::warn!(account = %key, %error, "push: EventSource read error");
                    return StreamEnd::Ended(true);
                }
                Err(_) => {
                    tracing::warn!(account = %key, "push: EventSource silent for 100s, reconnecting");
                    return StreamEnd::Ended(true);
                }
            }
        }
    }
}

enum StreamEnd {
    Unauthorized,
    Idle,
    /// bool: whether a connection was established first.
    Ended(bool),
}

fn handle_event_block(block: &str, user: &UserPush) {
    let mut event = "";
    let mut data = String::new();
    for line in block.lines() {
        if let Some(v) = line.strip_prefix("event:") {
            event = v.trim();
        } else if let Some(v) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(v.trim_start());
        }
    }
    if event != "state" || data.is_empty() {
        return;
    }
    let classes = classes_from_state_change(&data, user);
    if !classes.is_empty() {
        tracing::debug!(?classes, "push: state change");
        user.bump(&classes);
    }
}

/// Parse a JMAP StateChange payload; only types whose state string differs
/// from the last one seen count as changed.
fn classes_from_state_change(data: &str, user: &UserPush) -> Vec<Class> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(data) else {
        return Vec::new();
    };
    let Some(changed) = value.get("changed").and_then(|c| c.as_object()) else {
        return Vec::new();
    };
    let mut out: Vec<Class> = Vec::new();
    let mut inner = user.inner.lock().unwrap();
    for (account, types) in changed {
        let Some(types) = types.as_object() else {
            continue;
        };
        for (name, state) in types {
            let Some(class) = Class::from_jmap_type(name) else {
                continue;
            };
            let state = state.as_str().unwrap_or("").to_owned();
            let key = (account.clone(), name.clone());
            if inner.last_state.get(&key) == Some(&state) {
                continue;
            }
            inner.last_state.insert(key, state);
            if !out.contains(&class) {
                out.push(class);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user() -> Arc<UserPush> {
        let (tx, _) = watch::channel(0);
        let (supersede_tx, _) = watch::channel(0);
        Arc::new(UserPush {
            inner: Mutex::new(Inner::default()),
            tx,
            supersede_tx,
            waiters: AtomicUsize::new(0),
        })
    }

    #[test]
    fn state_change_maps_classes_and_dedups() {
        let u = user();
        let data = r#"{"@type":"StateChange","changed":{"a1":{"Email":"s1","Mailbox":"m1","Thread":"t1"}}}"#;
        handle_event_block(&format!("event: state\ndata: {data}\n\n"), &u);
        assert_eq!(u.generation(), 1);
        assert_eq!(u.changed_since(0), vec![Class::Mail]);
        // identical states again: nothing new
        handle_event_block(&format!("event: state\ndata: {data}\n\n"), &u);
        assert_eq!(u.generation(), 1);
        let data2 = r#"{"changed":{"a1":{"Email":"s2","ContactCard":"c1"}}}"#;
        handle_event_block(&format!("event: state\ndata: {data2}\n\n"), &u);
        assert_eq!(u.generation(), 2);
        let mut c = u.changed_since(1);
        c.sort_by_key(|c| format!("{c:?}"));
        assert_eq!(c, vec![Class::Contacts, Class::Mail]);
    }

    #[test]
    fn ping_events_ignored() {
        let u = user();
        handle_event_block("event: ping\ndata: {\"interval\":30}\n\n", &u);
        assert_eq!(u.generation(), 0);
    }

    #[test]
    fn status2_guard_clears_on_sync() {
        let u = user();
        assert!(!u.status2_pending_sync("d", Duration::from_secs(15)));
        u.note_status2("d");
        assert!(u.status2_pending_sync("d", Duration::from_secs(15)));
        assert!(u.mark_synced("d").is_some());
        assert!(!u.status2_pending_sync("d", Duration::from_secs(15)));
        assert!(u.mark_synced("d").is_none());
    }

    #[test]
    fn device_params_are_cached() {
        let u = user();
        u.set_device_params("d", 600, vec![("a".into(), "Email".into())]);
        let d = u.device("d");
        assert_eq!(d.heartbeat, Some(600));
        assert_eq!(d.folders.len(), 1);
        assert!(u.device("other").folders.is_empty());
    }
}
