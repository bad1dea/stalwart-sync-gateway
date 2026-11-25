use anyhow::Context;
use axum::http::HeaderMap;
use base64::{engine::general_purpose::STANDARD, Engine};
use reqwest::header::{ACCEPT, AUTHORIZATION};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use url::Url;

use crate::{
    config::Config,
    jmap::{
        capabilities::{self, GatewayCapabilities},
        session::JmapSession,
    },
    model::{
        eas_folder_type, CalendarEvent, Collection, CollectionKind, Contact, Email, EmailBody,
        EmailBodyType, EmailState, Task,
    },
};

#[derive(Debug, thiserror::Error)]
#[error("JMAP email state changed")]
struct EmailStateMismatch;

/// Stalwart's JMAP session response returns ABSOLUTE apiUrl/uploadUrl/
/// downloadUrl/eventSourceUrl fields built from its own configured
/// `server.hostname` -- which is not necessarily reachable from wherever
/// this gateway is actually calling in from (confirmed live against a real
/// deployment: `server.hostname` is deliberately `hermes.zt.khuo.ng` for an
/// unrelated Postfix-relay-identity reason documented in that deployment's
/// own infra docs, and there is no route for that bare host; every real
/// JMAP call after session discovery 502'd until this was added). Rewrite
/// scheme+host+port on every session URL to match whatever
/// `stalwart_jmap_url` (the URL actually used to reach Stalwart) resolves
/// to, preserving path+query. Same fix as the reference PHP implementation
/// applies (`jmap_client.php`'s `rebaseSessionUrls()`), done as part of the
/// JMAP client here instead of a bolted-on patch.
fn rebase_session_urls(session: &mut JmapSession, stalwart_jmap_url: &Url) {
    let authority = match (
        stalwart_jmap_url.host_str(),
        stalwart_jmap_url.port_or_known_default(),
    ) {
        (Some(host), Some(port)) => format!("{}://{host}:{port}", stalwart_jmap_url.scheme()),
        (Some(host), None) => format!("{}://{host}", stalwart_jmap_url.scheme()),
        _ => return,
    };

    session.api_url = rebase_one(&session.api_url, &authority);
    session.download_url = rebase_one(&session.download_url, &authority);
    session.upload_url = rebase_one(&session.upload_url, &authority);
    if let Some(event_source_url) = session.event_source_url.as_deref() {
        session.event_source_url = Some(rebase_one(event_source_url, &authority));
    }
}

/// Minimal percent-encoding for a single URL path segment substituted into
/// a JMAP session URL template (`{accountId}`). JMAP account ids are
/// typically short opaque tokens, but escape defensively rather than
/// assume that always holds.
pub(crate) fn percent_encode_path_segment(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Rebases the authority of `url` onto `authority`, preserving the path
/// and query EXACTLY as written. Deliberately does NOT go through
/// `url::Url`'s `.path()`/`.query()` accessors -- those percent-encode
/// per RFC 3986, which silently mangles `{` and `}` into `%7B`/`%7D`.
/// JMAP session URLs use literal `{accountId}`/`{blobId}`/etc. RFC 6570
/// URI Template placeholders that later code matches via plain string
/// substitution; going through `Url` first breaks that substitution with
/// no error, just a 404 (confirmed live: blob upload silently hit
/// `/jmap/upload/%7BaccountId%7D/` instead of a real account id until
/// this was fixed to use raw string slicing).
fn rebase_one(url: &str, authority: &str) -> String {
    match path_and_query_raw(url) {
        Some(path_and_query) => format!("{authority}{path_and_query}"),
        None => url.to_owned(),
    }
}

fn path_and_query_raw(url: &str) -> Option<&str> {
    let scheme_end = url.find("://")? + 3;
    let path_start = url[scheme_end..].find('/')?;
    Some(&url[scheme_end + path_start..])
}

#[derive(Clone)]
pub struct JmapClient {
    http: reqwest::Client,
    config: Config,
    state: crate::state::SharedStateStore,
}

#[derive(Default)]
struct DraftIdentityMap {
    by_backend: BTreeMap<String, String>,
    by_stable: BTreeMap<String, String>,
}
impl DraftIdentityMap {
    fn backend<'a>(&'a self, id: &'a str) -> &'a str {
        self.by_stable.get(id).map(String::as_str).unwrap_or(id)
    }
    fn stable<'a>(&'a self, id: &'a str) -> &'a str {
        self.by_backend.get(id).map(String::as_str).unwrap_or(id)
    }
    fn email(&self, mut email: Email) -> Email {
        if let Some(stable) = self.by_backend.get(&email.id) {
            email.draft_revision = Some(email.id.clone());
            email.id = stable.clone();
        }
        email
    }
    fn state(&self, backend: &str, mut state: EmailState) -> EmailState {
        if self.by_backend.contains_key(backend) {
            state.draft_revision = Some(backend.to_owned());
        }
        state
    }
}

#[derive(Debug, Clone)]
pub struct AuthenticatedSession {
    pub session: JmapSession,
    pub capabilities: GatewayCapabilities,
    pub authorization: BasicAuthorization,
}

#[derive(Debug, Clone)]
pub struct BasicAuthorization {
    username: String,
    password: String,
}

pub struct DraftCreated {
    pub id: String,
    pub attachments: Vec<(String, String, String)>,
}

pub struct EmailSyncItem {
    pub folder: String,
    pub is_draft: bool,
    pub replay: (String, String, String),
    pub properties: serde_json::Value,
    pub mime: Option<Vec<u8>>,
    pub attachments: Vec<crate::model::DraftAttachment>,
}

pub struct EmailSearchPage {
    pub emails: Vec<Email>,
    pub total: u64,
    pub query_state: Option<String>,
}

/// Mail changes since a stored JMAP Email state.
#[derive(Debug, Default)]
pub struct EmailChangesSince {
    pub new_state: String,
    /// Created or updated message id -> (message state, in the requested mailbox).
    pub touched: BTreeMap<String, (EmailState, bool)>,
    pub destroyed: Vec<String>,
}

/// How many of a device's stored mail ids (newest first) the Ping probe checks
/// for moves and read-state drift. Sync always re-checks at least this slice,
/// so a probe hit is always deliverable.
pub const PROBE_SEEN_CHECK: usize = 200;

impl BasicAuthorization {
    pub fn username(&self) -> &str {
        &self.username
    }

    pub fn password(&self) -> &str {
        &self.password
    }
}

/// Outcome of a `Mailbox/set` create, mapped from Stalwart's real
/// error `type` values -- confirmed live, not assumed from the JMAP
/// spec's generic error vocabulary: `alreadyExists` on a name
/// collision (with an `existingId`), `invalidProperties` (with a
/// "Parent ID does not exist" description) on a bad `parentId`.
pub enum CreateMailboxOutcome {
    Created(String),
    NameExists,
    ParentNotFound,
}

/// Outcome of a `Mailbox/set` update, same live-confirmed error-type
/// mapping approach as `CreateMailboxOutcome`. `forbidden` is
/// Stalwart's real rejection for a protected folder (confirmed live
/// against Inbox: "You are not allowed to delete Inbox, Junk or
/// Trash folders." -- same wording/type for update).
pub enum UpdateMailboxOutcome {
    Updated,
    NotFound,
    Forbidden,
    NameExists,
}

/// Outcome of a `Mailbox/set` destroy, backing `FolderDelete`. Same
/// live-confirmed `notFound`/`forbidden` error types as update.
pub enum DestroyMailboxOutcome {
    Destroyed,
    NotFound,
    Forbidden,
}

#[derive(Clone, Copy)]
pub enum GroupwareCollectionKind {
    AddressBook,
    Calendar,
}

impl GroupwareCollectionKind {
    fn object(self) -> &'static str {
        match self {
            Self::AddressBook => "AddressBook",
            Self::Calendar => "Calendar",
        }
    }

    fn capability(self) -> &'static str {
        match self {
            Self::AddressBook => capabilities::CONTACTS,
            Self::Calendar => capabilities::CALENDARS,
        }
    }
}

pub enum CreateGroupwareCollectionOutcome {
    Created(String),
    NameExists,
    Unsupported,
}

pub enum UpdateGroupwareCollectionOutcome {
    Updated,
    NotFound,
    Forbidden,
    NameExists,
    Unsupported,
}

pub enum DestroyGroupwareCollectionOutcome {
    Destroyed,
    NotFound,
    Forbidden,
    Unsupported,
}

impl JmapClient {
    pub fn new(config: Config, state: crate::state::SharedStateStore) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent("stalwart-sync-gateway/0.1")
            .danger_accept_invalid_certs(!config.stalwart_tls_verify)
            // Stalwart can close idle connections before the next 30-second probe.
            .pool_idle_timeout(std::time::Duration::from_secs(20))
            .http2_adaptive_window(true)
            .build()
            .context("failed to build JMAP HTTP client")?;
        Ok(Self {
            http,
            config,
            state,
        })
    }

    async fn draft_identity_map(
        &self,
        auth: &AuthenticatedSession,
    ) -> anyhow::Result<DraftIdentityMap> {
        let account = auth
            .session
            .primary_account_for(capabilities::MAIL)
            .context("JMAP Mail capability is unavailable")?;
        let mut result = DraftIdentityMap::default();
        for item in self.state.draft_mappings(account).await? {
            if item.item_id == item.hash
                || result
                    .by_backend
                    .insert(item.hash.clone(), item.item_id.clone())
                    .is_some()
                || result.by_stable.insert(item.item_id, item.hash).is_some()
            {
                anyhow::bail!("invalid draft identity mapping");
            }
        }
        Ok(result)
    }

    pub async fn unauthenticated_probe(&self) -> anyhow::Result<()> {
        let response = self
            .http
            .get(self.config.stalwart_jmap_url.clone())
            .send()
            .await
            .context("failed to reach JMAP session URL")?;
        if response.status().is_server_error() {
            anyhow::bail!("JMAP session URL returned {}", response.status());
        }
        Ok(())
    }

    pub async fn session_with_basic(
        &self,
        username: &str,
        password: &str,
    ) -> anyhow::Result<AuthenticatedSession> {
        let response = self
            .http
            .get(self.config.stalwart_jmap_url.clone())
            .basic_auth(username, Some(password))
            .header(ACCEPT, "application/json")
            .send()
            .await
            .context("failed to fetch JMAP session")?;

        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            anyhow::bail!("JMAP authentication failed");
        }
        let response = response.error_for_status().context("JMAP session failed")?;
        let mut session: JmapSession =
            response.json().await.context("invalid JMAP session JSON")?;
        rebase_session_urls(&mut session, &self.config.stalwart_jmap_url);
        let capabilities = GatewayCapabilities::from_session(&session);
        Ok(AuthenticatedSession {
            session,
            capabilities,
            authorization: BasicAuthorization {
                username: username.to_owned(),
                password: password.to_owned(),
            },
        })
    }

    pub async fn collections(
        &self,
        auth: &AuthenticatedSession,
    ) -> anyhow::Result<Vec<Collection>> {
        let mut calls = Vec::new();
        let mut using = vec![capabilities::CORE.to_owned()];

        if let Some(account_id) = auth.session.primary_account_for(capabilities::MAIL) {
            using.push(capabilities::MAIL.to_owned());
            calls.push(MethodCall::new(
                "Mailbox/get",
                serde_json::json!({ "accountId": account_id }),
                "mailboxes",
            ));
        }
        if auth.capabilities.contacts {
            if let Some(account_id) = auth.session.primary_account_for(capabilities::CONTACTS) {
                using.push(capabilities::CONTACTS.to_owned());
                calls.push(MethodCall::new(
                    "AddressBook/get",
                    serde_json::json!({ "accountId": account_id }),
                    "addressBooks",
                ));
            }
        }
        if auth.capabilities.calendar {
            if let Some(account_id) = auth.session.primary_account_for(capabilities::CALENDARS) {
                using.push(capabilities::CALENDARS.to_owned());
                calls.push(MethodCall::new(
                    "Calendar/get",
                    serde_json::json!({ "accountId": account_id }),
                    "calendars",
                ));
            }
        }

        if calls.is_empty() {
            return Ok(Vec::new());
        }

        let response: JmapResponse<serde_json::Value> = self.api_call(auth, &using, calls).await?;
        let mut collections = Vec::new();
        let mut found_notes_mailbox = false;

        for method in response.method_responses {
            match method.0.as_str() {
                "Mailbox/get" => {
                    let get: GetResponse<MailboxObject> =
                        serde_json::from_value(method.1).context("invalid Mailbox/get response")?;
                    for mailbox in get.list {
                        // Metadata only -- folder names/ids, same as what
                        // FolderSync itself hands to any authenticated
                        // client. Diagnosing a live report that a real
                        // pre-existing "Notes" folder isn't being found by
                        // the exact-name match below (case/whitespace
                        // mismatch is the leading suspect).
                        tracing::debug!(
                            mailbox_id = mailbox.id.as_str(),
                            mailbox_name = mailbox.name.as_str(),
                            is_top_level = mailbox.parent_id.is_none(),
                            "mailbox discovered"
                        );
                        // The account's "Notes" mailbox (see jmap::notes
                        // module docs for why it's this exact mailbox, not
                        // a separate gateway-only folder) is advertised as
                        // a Notes collection, not a generic mail folder --
                        // never both.
                        if mailbox.parent_id.is_none()
                            && mailbox.name == crate::jmap::notes::NOTES_MAILBOX_NAME
                        {
                            found_notes_mailbox = true;
                            collections.push(Collection {
                                id: format!("note_{}", mailbox.id),
                                parent_id: None,
                                name: mailbox.name,
                                kind: CollectionKind::Notes,
                                role: None,
                                folder_type: eas_folder_type::NOTE,
                            });
                        } else {
                            collections.push(Collection::from(mailbox));
                        }
                    }
                }
                "AddressBook/get" => {
                    let get: GetResponse<AddressBookObject> = serde_json::from_value(method.1)
                        .context("invalid AddressBook/get response")?;
                    collections.extend(get.list.into_iter().map(Collection::from));
                }
                "Calendar/get" => {
                    let get: GetResponse<CalendarObject> = serde_json::from_value(method.1)
                        .context("invalid Calendar/get response")?;
                    for calendar in get.list {
                        // Stalwart has no separate Tasks-list object --
                        // confirmed live (2026-08-25) that `CalendarEvent`
                        // itself accepts `@type: "Task"` (title/due/start/
                        // progress/percentComplete/priority/description all
                        // round-trip correctly), so Tasks rides on the SAME
                        // underlying Calendar storage as Events, distinguished
                        // only by `@type` -- there's no per-calendar Tasks
                        // capability to check. One synthetic Tasks collection
                        // per real calendar (same multiplicity as Calendar
                        // itself), `task_`-prefixed like `cal_`/`note_`/`ab_`.
                        collections.push(Collection {
                            id: format!("task_{}", calendar.id),
                            parent_id: None,
                            name: "Tasks".to_owned(),
                            kind: CollectionKind::Tasks,
                            role: None,
                            folder_type: if calendar.is_default {
                                eas_folder_type::TASK
                            } else {
                                eas_folder_type::USER_TASK
                            },
                        });
                        collections.push(Collection::from(calendar));
                    }
                }
                "error" => anyhow::bail!("JMAP method error in collection discovery"),
                other => {
                    tracing::debug!(method = other, "ignoring unexpected JMAP method response")
                }
            }
        }

        // An account with no "Notes" mailbox yet would otherwise never
        // advertise a Notes collection at all -- and with no collection id
        // to target, the client would have no way to ever create its
        // first note (there's no FolderCreate support yet either). Create
        // it lazily here so Notes always shows up, same as a real Exchange
        // server's Notes folder always being there.
        if !found_notes_mailbox {
            match self.ensure_notes_mailbox_id(auth).await {
                Ok(mailbox_id) => collections.push(Collection {
                    id: format!("note_{mailbox_id}"),
                    parent_id: None,
                    name: crate::jmap::notes::NOTES_MAILBOX_NAME.to_owned(),
                    kind: CollectionKind::Notes,
                    role: None,
                    folder_type: eas_folder_type::NOTE,
                }),
                Err(error) => {
                    tracing::warn!(%error, "failed to ensure Notes mailbox during FolderSync")
                }
            }
        }

        Ok(collections)
    }

    /// Returns the mailbox's newest messages (up to `limit`) plus whether
    /// more exist beyond that window -- real bug, confirmed live via the
    /// zoidberg A/B test: a real device's first Sync omitted WindowSize
    /// (default 25), the mailbox had well over 25 messages, and the
    /// gateway silently truncated to 25 with no `MoreAvailable` flag --
    /// a real EAS protocol violation (the spec requires it whenever a
    /// windowed response doesn't cover everything) that plausibly
    /// contributed to the client discarding the response outright.
    pub async fn emails_in_mailbox(
        &self,
        auth: &AuthenticatedSession,
        mailbox_id: &str,
        limit: usize,
    ) -> anyhow::Result<(Vec<Email>, bool)> {
        let Some(account_id) = auth.session.primary_account_for(capabilities::MAIL) else {
            return Ok((Vec::new(), false));
        };
        let identities = self.draft_identity_map(auth).await?;
        let limit = limit.clamp(1, 100);

        let calls = vec![
            MethodCall::new(
                "Email/query",
                serde_json::json!({
                    "accountId": account_id,
                    "filter": { "inMailbox": mailbox_id },
                    "sort": [{ "property": "receivedAt", "isAscending": false }],
                    "limit": limit,
                    "calculateTotal": true
                }),
                "q",
            ),
            MethodCall::new(
                "Email/get",
                serde_json::json!({
                    "accountId": account_id,
                    "#ids": {
                        "resultOf": "q",
                        "name": "Email/query",
                        "path": "/ids"
                    },
                    "properties": [
                        "id", "blobId", "mailboxIds", "keywords", "receivedAt", "subject",
                        "from", "to", "cc", "bcc", "replyTo", "header:To:asRaw", "header:Cc:asRaw",
                            "header:Bcc:asRaw", "header:Reply-To:asRaw", "header:Importance:asText", "textBody", "htmlBody", "bodyValues", "bodyStructure", "attachments", "threadId"
                    ],
                    "fetchAllBodyValues": true,
                    "maxBodyValueBytes": 65536
                }),
                "g",
            ),
        ];

        let response: JmapResponse<serde_json::Value> = self
            .api_call(
                auth,
                &[capabilities::CORE.to_owned(), capabilities::MAIL.to_owned()],
                calls,
            )
            .await?;

        let mut total: Option<u64> = None;
        for method in response.method_responses {
            match method.0.as_str() {
                "Email/query" => {
                    let query: QueryResponse =
                        serde_json::from_value(method.1).context("invalid Email/query response")?;
                    total = query.total;
                }
                "Email/get" => {
                    let get: GetResponse<EmailObject> =
                        serde_json::from_value(method.1).context("invalid Email/get response")?;
                    let emails: Vec<Email> = get
                        .list
                        .into_iter()
                        .map(Email::from)
                        .map(|email| identities.email(email))
                        .collect();
                    let has_more = total.is_some_and(|total| total as usize > limit);
                    return Ok((emails, has_more));
                }
                "error" => anyhow::bail!("JMAP method error in email sync"),
                _ => {}
            }
        }

        Ok((Vec::new(), false))
    }

    /// Ids of the newest `limit` messages in a mailbox, newest first. Sync
    /// pages through these: each response sends the next `WindowSize` ids
    /// the device has not been sent, with MoreAvailable while any remain.
    pub async fn email_ids_in_mailbox(
        &self,
        auth: &AuthenticatedSession,
        mailbox_id: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<String>> {
        self.email_ids_in_mailbox_since(auth, mailbox_id, None, limit)
            .await
    }

    /// Like `email_ids_in_mailbox`, limited to messages received at or after
    /// `after` (UTC, RFC 3339) when given: the EAS FilterType date range.
    pub async fn email_ids_in_mailbox_since(
        &self,
        auth: &AuthenticatedSession,
        mailbox_id: &str,
        after: Option<&str>,
        limit: usize,
    ) -> anyhow::Result<Vec<String>> {
        // `limit` 0 = the whole mailbox. Paged through Email/query positions
        // so a large mailbox is never silently cut off.
        let filter = match after {
            Some(after) => serde_json::json!({ "inMailbox": mailbox_id, "after": after }),
            None => serde_json::json!({ "inMailbox": mailbox_id }),
        };
        const PAGE: usize = 5000;
        let Some(account_id) = auth.session.primary_account_for(capabilities::MAIL) else {
            return Ok(Vec::new());
        };
        let mut ids: Vec<String> = Vec::new();
        loop {
            let want = if limit == 0 {
                PAGE
            } else {
                PAGE.min(limit - ids.len())
            };
            if want == 0 {
                break;
            }
            let response: JmapResponse<serde_json::Value> = self
                .api_call(
                    auth,
                    &[capabilities::CORE.to_owned(), capabilities::MAIL.to_owned()],
                    vec![MethodCall::new(
                        "Email/query",
                        serde_json::json!({
                            "accountId": account_id,
                            "filter": filter,
                            "sort": [{ "property": "receivedAt", "isAscending": false }],
                            "position": ids.len(),
                            "limit": want
                        }),
                        "q",
                    )],
                )
                .await?;
            let mut page: Vec<String> = Vec::new();
            for method in response.method_responses {
                match method.0.as_str() {
                    "Email/query" => {
                        let q: QueryResponse = serde_json::from_value(method.1)
                            .context("invalid Email/query response")?;
                        page = q.ids;
                    }
                    "error" => anyhow::bail!("JMAP method error listing mailbox"),
                    _ => {}
                }
            }
            let got = page.len();
            ids.extend(page);
            if got < want {
                break;
            }
        }
        let identities = self.draft_identity_map(auth).await?;
        for id in &mut ids {
            *id = identities.stable(id).to_owned();
        }
        let mut unique = BTreeSet::new();
        anyhow::ensure!(
            ids.iter().all(|id| unique.insert(id.clone())),
            "duplicate draft identity in mailbox"
        );
        Ok(ids)
    }

    /// Full message objects for `ids`, in the order given (missing ids are
    /// skipped).
    pub async fn emails_by_ids(
        &self,
        auth: &AuthenticatedSession,
        ids: &[String],
    ) -> anyhow::Result<Vec<Email>> {
        let Some(account_id) = auth.session.primary_account_for(capabilities::MAIL) else {
            return Ok(Vec::new());
        };
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let identities = self.draft_identity_map(auth).await?;
        let backend_ids: Vec<_> = ids
            .iter()
            .map(|id| identities.backend(id).to_owned())
            .collect();
        let response: JmapResponse<serde_json::Value> = self
            .api_call(
                auth,
                &[capabilities::CORE.to_owned(), capabilities::MAIL.to_owned()],
                vec![MethodCall::new(
                    "Email/get",
                    serde_json::json!({
                        "accountId": account_id,
                        "ids": backend_ids,
                        "properties": [
                            "id", "blobId", "mailboxIds", "keywords", "receivedAt", "subject",
                            "from", "to", "cc", "bcc", "replyTo", "header:To:asRaw", "header:Cc:asRaw",
                            "header:Bcc:asRaw", "header:Reply-To:asRaw", "header:Importance:asText", "textBody", "htmlBody", "bodyValues", "bodyStructure", "attachments", "threadId"
                        ],
                        "fetchAllBodyValues": true,
                        "maxBodyValueBytes": 65536
                    }),
                    "g",
                )],
            )
            .await?;
        for method in response.method_responses {
            match method.0.as_str() {
                "Email/get" => {
                    let get: GetResponse<EmailObject> =
                        serde_json::from_value(method.1).context("invalid Email/get response")?;
                    let mut by_id: BTreeMap<String, Email> = get
                        .list
                        .into_iter()
                        .map(Email::from)
                        .map(|email| identities.email(email))
                        .map(|e| (e.id.clone(), e))
                        .collect();
                    return Ok(ids.iter().filter_map(|id| by_id.remove(id)).collect());
                }
                "error" => anyhow::bail!("JMAP method error fetching messages"),
                _ => {}
            }
        }
        Ok(Vec::new())
    }

    /// Backs EAS `Search` (MS-ASCMD 2.2.1.16) for `Store>Name == "Mailbox"`
    /// with a `FreeText` query -- confirmed live 2026-08-26 that
    /// Stalwart's `Email/query` `filter: {"text": ...}` does real
    /// full-text matching (subject AND body, not just subject: a probe
    /// search for a common word matched messages where it only appeared
    /// in the body, not the subject line). `position`/`limit` implement
    /// the EAS `Range` element's zero-based inclusive "m-n" semantics
    /// directly via JMAP's own `position`/`limit` query params, so this
    /// doesn't need to over-fetch and slice client-side. Returns
    /// `(matches, total)` -- `total` is the full match count regardless
    /// of the requested range, same shape as `emails_in_mailbox`.
    pub async fn search_emails(
        &self,
        auth: &AuthenticatedSession,
        conditions: Vec<serde_json::Value>,
        position: i64,
        limit: usize,
    ) -> anyhow::Result<(Vec<Email>, u64)> {
        let filter = if conditions.is_empty() {
            serde_json::json!({})
        } else {
            serde_json::json!({"operator":"AND","conditions":conditions})
        };
        let page = self
            .query_emails(
                auth,
                filter,
                position,
                limit.min(100),
                &[
                    "id",
                    "blobId",
                    "mailboxIds",
                    "keywords",
                    "receivedAt",
                    "subject",
                    "from",
                    "to",
                    "cc",
                    "bcc",
                    "preview",
                    "textBody",
                    "htmlBody",
                    "bodyValues",
                    "bodyStructure",
                    "attachments",
                    "threadId",
                ],
            )
            .await?;
        Ok((page.emails, page.total))
    }

    pub async fn query_emails(
        &self,
        auth: &AuthenticatedSession,
        filter: serde_json::Value,
        position: i64,
        limit: usize,
        properties: &[&str],
    ) -> anyhow::Result<EmailSearchPage> {
        let Some(account_id) = auth.session.primary_account_for(capabilities::MAIL) else {
            anyhow::bail!("JMAP Mail capability is unavailable");
        };
        let maximum = auth
            .session
            .capabilities
            .get(capabilities::CORE)
            .and_then(|cap| cap.get("maxObjectsInGet"))
            .and_then(|value| value.as_u64())
            .unwrap_or(100)
            .clamp(1, 1000) as usize;
        let limit = limit.clamp(1, maximum);

        let identities = self.draft_identity_map(auth).await?;
        let calls = vec![
            MethodCall::new(
                "Email/query",
                serde_json::json!({
                    "accountId": account_id,
                    "filter": filter,
                    "sort": [{ "property": "receivedAt", "isAscending": false }],
                    "position": position,
                    "limit": limit,
                    "calculateTotal": true
                }),
                "q",
            ),
            MethodCall::new(
                "Email/get",
                serde_json::json!({
                    "accountId": account_id,
                    "#ids": {
                        "resultOf": "q",
                        "name": "Email/query",
                        "path": "/ids"
                    },
                    "properties": properties,
                    "fetchAllBodyValues": properties.contains(&"bodyValues"),
                    "maxBodyValueBytes": 65536
                }),
                "g",
            ),
        ];

        let response: JmapResponse<serde_json::Value> = self
            .api_call(
                auth,
                &[capabilities::CORE.to_owned(), capabilities::MAIL.to_owned()],
                calls,
            )
            .await?;

        let mut total: u64 = 0;
        let mut query_state = None;
        let mut query_ids = Vec::new();
        for method in response.method_responses {
            match method.0.as_str() {
                "Email/query" => {
                    let query: QueryResponse =
                        serde_json::from_value(method.1).context("invalid Email/query response")?;
                    total = query.total.context("Email/query omitted total")?;
                    query_state = query.query_state;
                    query_ids = query.ids;
                }
                "Email/get" => {
                    let get: GetResponse<EmailObject> =
                        serde_json::from_value(method.1).context("invalid Email/get response")?;
                    let mut by_id: BTreeMap<_, _> = get
                        .list
                        .into_iter()
                        .map(Email::from)
                        .map(|email| identities.email(email))
                        .map(|email| (email.id.clone(), email))
                        .collect();
                    let emails: Vec<Email> = query_ids
                        .iter()
                        .filter_map(|id| by_id.remove(identities.stable(id)))
                        .collect();
                    anyhow::ensure!(
                        emails.len() == query_ids.len() && by_id.is_empty(),
                        "Email search changed during fetch"
                    );
                    return Ok(EmailSearchPage {
                        emails,
                        total,
                        query_state,
                    });
                }
                "error" => anyhow::bail!("JMAP method error in email search"),
                _ => {}
            }
        }

        anyhow::bail!("missing Email/get response for search")
    }

    /// Backs EAS `Search` for `Store>Name == "GAL"` -- confirmed live
    /// 2026-08-26 against Stalwart's `Principal/query`: `filter:
    /// {"text": ...}` does real (if strict -- whole-token, not
    /// substring/prefix like real Exchange GAL's ANR) matching against
    /// name/email, checked by searching a real principal's first name
    /// and getting exactly that principal back. There are 7 real
    /// principals on this instance (not a placeholder/empty directory),
    /// so this has something real to search. Stalwart's `Principal`
    /// object has no separate first/last name fields -- `description` is
    /// the closest thing to a human display name (e.g. "khuong", "System
    /// administrator") and is used for `gal:DisplayName` when present,
    /// falling back to the account name (its email) otherwise.
    pub async fn all_principals(
        &self,
        auth: &AuthenticatedSession,
    ) -> anyhow::Result<Vec<PrincipalResult>> {
        let Some(account_id) = auth.session.primary_account_for(capabilities::PRINCIPALS) else {
            anyhow::bail!("JMAP principal capability is unavailable");
        };
        let maximum = auth
            .session
            .capabilities
            .get(capabilities::CORE)
            .and_then(|cap| cap.get("maxObjectsInGet"))
            .and_then(|value| value.as_u64())
            .unwrap_or(100)
            .clamp(1, 100) as usize;
        let mut position = 0usize;
        let mut state: Option<String> = None;
        let mut seen = std::collections::HashSet::new();
        let mut principals = Vec::new();
        loop {
            let calls = vec![
                MethodCall::new(
                    "Principal/query",
                    serde_json::json!({
                        "accountId":account_id, "position":position, "limit":maximum, "calculateTotal":true
                    }),
                    "q",
                ),
                MethodCall::new(
                    "Principal/get",
                    serde_json::json!({
                        "accountId":account_id,
                        "#ids":{"resultOf":"q","name":"Principal/query","path":"/ids"},
                        "properties":["id","name","email","description"]
                    }),
                    "g",
                ),
            ];
            let response: JmapResponse<serde_json::Value> = self
                .api_call(
                    auth,
                    &[
                        capabilities::CORE.to_owned(),
                        capabilities::PRINCIPALS.to_owned(),
                    ],
                    calls,
                )
                .await?;
            let mut query = None;
            let mut list = None;
            for method in response.method_responses {
                match method.0.as_str() {
                    "Principal/query" => {
                        query = Some(serde_json::from_value::<QueryResponse>(method.1)?)
                    }
                    "Principal/get" => {
                        list = Some(
                            serde_json::from_value::<GetResponse<PrincipalObject>>(method.1)?.list,
                        )
                    }
                    "error" => anyhow::bail!("JMAP principal query failed"),
                    _ => {}
                }
            }
            let query = query.ok_or_else(|| anyhow::anyhow!("Principal/query response missing"))?;
            let query_state = query
                .query_state
                .ok_or_else(|| anyhow::anyhow!("Principal/query state missing"))?;
            if state.as_ref().is_some_and(|old| old != &query_state) {
                anyhow::bail!("principal directory changed during search");
            }
            state = Some(query_state);
            let list = list.ok_or_else(|| anyhow::anyhow!("Principal/get response missing"))?;
            if list.len() != query.ids.len() {
                anyhow::bail!("principal directory changed during search");
            }
            let count = query.ids.len();
            if count > maximum
                || query.ids.iter().any(|id| !seen.insert(id.clone()))
                || list
                    .iter()
                    .any(|principal| !query.ids.contains(&principal.id))
            {
                anyhow::bail!("invalid Principal/query page");
            }
            principals.extend(list.into_iter().map(PrincipalResult::from));
            position += count;
            if count == 0 || query.total.is_some_and(|total| position as u64 >= total) {
                break;
            }
        }
        Ok(principals)
    }

    /// Returns free/busy periods for multiple Principals in one JMAP request.
    /// The nested error status is an EAS per-recipient availability status;
    /// transport and malformed response errors remain outer errors and are
    /// reported as transient.
    pub async fn principal_availabilities(
        &self,
        auth: &AuthenticatedSession,
        principal_ids: &[String],
        utc_start: &str,
        utc_end: &str,
    ) -> anyhow::Result<
        std::collections::HashMap<String, Result<Vec<serde_json::Value>, &'static str>>,
    > {
        if principal_ids.is_empty() {
            return Ok(Default::default());
        }
        let Some(account_id) = auth.session.primary_account_for(capabilities::PRINCIPALS) else {
            return Ok(principal_ids
                .iter()
                .cloned()
                .map(|id| (id, Err("163")))
                .collect());
        };
        let mut results = std::collections::HashMap::new();
        let max_calls = auth
            .session
            .capabilities
            .get(capabilities::CORE)
            .and_then(|core| core["maxCallsInRequest"].as_u64())
            .unwrap_or(16)
            .clamp(1, 100) as usize;
        for chunk in principal_ids.chunks(max_calls) {
            let calls: Vec<_> = chunk
                .iter()
                .enumerate()
                .map(|(index, principal_id)| {
                    MethodCall::new(
                        "Principal/getAvailability",
                        serde_json::json!({
                            "accountId": account_id,
                            "id": principal_id,
                            "utcStart": utc_start,
                            "utcEnd": utc_end,
                            "showDetails": true,
                            "eventProperties": ["freeBusyStatus"]
                        }),
                        &format!("availability-{index}"),
                    )
                })
                .collect();
            let response: JmapResponse<serde_json::Value> = self
                .api_call(
                    auth,
                    &[
                        capabilities::CORE.to_owned(),
                        capabilities::PRINCIPALS.to_owned(),
                        capabilities::PRINCIPALS_AVAILABILITY.to_owned(),
                    ],
                    calls,
                )
                .await?;
            for method in response.method_responses {
                if let Some(index) = method.2.strip_prefix("availability-") {
                    let Ok(index) = index.parse::<usize>() else {
                        continue;
                    };
                    let Some(principal_id) = chunk.get(index) else {
                        continue;
                    };
                    match method.0.as_str() {
                        "Principal/getAvailability" => {
                            let list = method.1["list"]
                                .as_array()
                                .context("Principal/getAvailability omitted list")?
                                .clone();
                            results.insert(principal_id.clone(), Ok(list));
                        }
                        "error" => {
                            let status = match method.1["type"].as_str() {
                                Some("forbidden" | "notFound" | "tooLarge") => "163",
                                _ => "162",
                            };
                            results.insert(principal_id.clone(), Err(status));
                        }
                        _ => {}
                    }
                }
            }
        }
        for principal_id in principal_ids {
            results.entry(principal_id.clone()).or_insert(Err("162"));
        }
        Ok(results)
    }

    /// Real bug, found live while implementing mail deletion detection:
    /// `emails_in_mailbox` is sorted newest-first and capped at `limit`,
    /// so an OLD message that's still on the server but has simply been
    /// pushed out of the window by newer mail arriving looks IDENTICAL
    /// to a genuinely deleted one under a naive "not in this fetch"
    /// diff -- the same diff-against-last-seen approach that's safe for
    /// Contacts/Calendar/Notes (their queries aren't meaningfully
    /// windowed at real-world item counts) would be actively wrong here
    /// and delete mail from the device that's still sitting on the
    /// server, just further back than the window. This checks each
    /// candidate id directly instead: one that comes back in Email/get's
    /// `notFound`, or whose `mailboxIds` no longer includes this
    /// collection's mailbox (moved elsewhere -- e.g. to Trash via
    /// another client), is a real removal from THIS collection. Returns
    /// the subset of `candidate_ids` that are CONFIRMED STILL present
    /// (i.e. NOT a real removal) -- the caller computes real removals as
    /// `candidate_ids - this result`.
    pub async fn emails_still_in_mailbox(
        &self,
        auth: &AuthenticatedSession,
        candidate_ids: &[String],
        mailbox_id: &str,
    ) -> anyhow::Result<BTreeSet<String>> {
        if candidate_ids.is_empty() {
            return Ok(BTreeSet::new());
        }
        if candidate_ids.len() > 400 {
            let mut present = BTreeSet::new();
            for chunk in candidate_ids.chunks(400) {
                present
                    .extend(Box::pin(self.emails_still_in_mailbox(auth, chunk, mailbox_id)).await?);
            }
            return Ok(present);
        }
        let Some(account_id) = auth.session.primary_account_for(capabilities::MAIL) else {
            return Ok(BTreeSet::new());
        };

        let identities = self.draft_identity_map(auth).await?;
        let backend_ids: Vec<_> = candidate_ids
            .iter()
            .map(|id| identities.backend(id).to_owned())
            .collect();
        let response: JmapResponse<serde_json::Value> = self
            .api_call(
                auth,
                &[capabilities::CORE.to_owned(), capabilities::MAIL.to_owned()],
                vec![MethodCall::new(
                    "Email/get",
                    serde_json::json!({
                        "accountId": account_id,
                        "ids": backend_ids,
                        "properties": ["id", "mailboxIds"]
                    }),
                    "0",
                )],
            )
            .await?;

        for method in response.method_responses {
            if method.0 == "Email/get" {
                let get: GetResponse<EmailMembership> = serde_json::from_value(method.1).context(
                    "invalid Email/get response while checking mail deletion candidates",
                )?;
                return Ok(get
                    .list
                    .into_iter()
                    .filter(|email| email.mailbox_ids.get(mailbox_id).copied().unwrap_or(false))
                    .map(|email| identities.stable(&email.id).to_owned())
                    .collect());
            } else if method.0 == "error" {
                anyhow::bail!("JMAP method error while checking mail deletion candidates");
            }
        }
        anyhow::bail!("JMAP Email/get response was missing while checking mail deletion candidates")
    }

    /// Cheap "would Sync send anything for this mailbox?" probe used by Ping
    /// after a push wakeup. Mirrors what mail Sync actually delivers with its
    /// seen-id diff: a message among the newest `NEWEST` that the device has
    /// not been sent yet (arrival or move-in), or a message the device was
    /// sent that is no longer in the mailbox (delete or move-out). Server-side
    /// read-flag changes are NOT reported, because Sync cannot deliver them.
    pub async fn mailbox_has_sync_changes(
        &self,
        auth: &AuthenticatedSession,
        mailbox_id: &str,
        seen_ids: &[String],
        message_states: &BTreeMap<String, String>,
        window: usize,
        after: Option<&str>,
    ) -> anyhow::Result<Option<&'static str>> {
        const NEWEST: usize = 100;
        const SEEN_CHECK: usize = PROBE_SEEN_CHECK;
        // Read-flag changes are only compared for the newest few messages,
        // the part of the mailbox every device's Sync window covers.
        const STATE_WINDOW: usize = 25;
        let Some(account_id) = auth.session.primary_account_for(capabilities::MAIL) else {
            return Ok(None);
        };
        let identities = self.draft_identity_map(auth).await?;
        let mut calls = vec![
            MethodCall::new(
                "Email/query",
                serde_json::json!({
                    "accountId": account_id,
                    // Same FilterType range as the device's Sync: a message
                    // Sync would not send is not news.
                    "filter": match after {
                        Some(after) => serde_json::json!({ "inMailbox": mailbox_id, "after": after }),
                        None => serde_json::json!({ "inMailbox": mailbox_id }),
                    },
                    "sort": [{ "property": "receivedAt", "isAscending": false }],
                    "limit": NEWEST
                }),
                "q",
            ),
            MethodCall::new(
                "Email/get",
                serde_json::json!({
                    "accountId": account_id,
                    "#ids": { "resultOf": "q", "name": "Email/query", "path": "/ids" },
                    "properties": ["id", "keywords"]
                }),
                "n",
            ),
        ];
        // `seen_ids` is stored newest first (see sync_mail); Sync confirms the
        // same leading slice, so anything reported here it can deliver.
        let check: Vec<String> = seen_ids
            .iter()
            .take(SEEN_CHECK)
            .map(|id| identities.backend(id).to_owned())
            .collect();
        if !check.is_empty() {
            calls.push(MethodCall::new(
                "Email/get",
                serde_json::json!({
                    "accountId": account_id,
                    "ids": check,
                    "properties": ["id", "mailboxIds", "keywords"]
                }),
                "g",
            ));
        }
        let response: JmapResponse<serde_json::Value> = self
            .api_call(
                auth,
                &[capabilities::CORE.to_owned(), capabilities::MAIL.to_owned()],
                calls,
            )
            .await?;

        let seen: BTreeSet<&str> = seen_ids.iter().map(String::as_str).collect();
        let mut query_ids: Vec<String> = Vec::new();
        let mut newest_state: BTreeMap<String, EmailState> = BTreeMap::new();
        let mut still_here: Option<BTreeSet<String>> = None;
        let mut seen_state: BTreeMap<String, EmailState> = BTreeMap::new();
        for method in response.method_responses {
            let call_id = method.2.clone();
            match method.0.as_str() {
                "Email/query" => {
                    let q: QueryResponse =
                        serde_json::from_value(method.1).context("invalid Email/query response")?;
                    query_ids = q
                        .ids
                        .into_iter()
                        .map(|id| identities.stable(&id).to_owned())
                        .collect();
                }
                "Email/get" if call_id == "n" => {
                    let g: GetResponse<EmailMembership> =
                        serde_json::from_value(method.1).context("invalid Email/get response")?;
                    for e in g.list {
                        let read = identities.state(&e.id, EmailState::from_keywords(&e.keywords));
                        newest_state.insert(identities.stable(&e.id).to_owned(), read);
                    }
                }
                "Email/get" => {
                    let g: GetResponse<EmailMembership> =
                        serde_json::from_value(method.1).context("invalid Email/get response")?;
                    for e in &g.list {
                        let read = identities.state(&e.id, EmailState::from_keywords(&e.keywords));
                        seen_state.insert(identities.stable(&e.id).to_owned(), read);
                    }
                    still_here = Some(
                        g.list
                            .into_iter()
                            .filter(|e| e.mailbox_ids.get(mailbox_id).copied().unwrap_or(false))
                            .map(|e| identities.stable(&e.id).to_owned())
                            .collect(),
                    );
                }
                "error" => anyhow::bail!("JMAP method error while probing mailbox changes"),
                _ => {}
            }
        }
        // Only the newest `window` messages count as "new": that is all a
        // device with that Sync window will ever be sent. Looking further
        // makes Ping announce changes the following Sync cannot deliver, and
        // iOS then blacklists the folder from push.
        if query_ids
            .iter()
            .take(window.clamp(1, NEWEST))
            .any(|id| !seen.contains(id.as_str()))
        {
            return Ok(Some("new message in window"));
        }
        if let Some(still_here) = still_here {
            let gone = seen_ids
                .iter()
                .take(SEEN_CHECK)
                .any(|id| !still_here.contains(id));
            if gone {
                return Ok(Some("sent message left the folder"));
            }
        }
        // Read-state drift on older messages the device already has (outside
        // the newest-25 check below): only ids with a stored state count.
        let older_drift = seen_state.iter().any(|(id, read)| {
            message_states
                .get(id)
                .is_some_and(|stored| stored.as_str() != read.fingerprint())
        });
        if older_drift {
            return Ok(Some("read state changed (older message)"));
        }
        // Read-state drift on a message the device already has.
        let drift = query_ids.iter().take(STATE_WINDOW).any(|id| {
            let Some(read) = newest_state.get(id) else {
                return false;
            };
            // Only messages the device was actually sent (stored state) count;
            // a message outside its window is not a change it can receive.
            message_states
                .get(id)
                .is_some_and(|stored| stored.as_str() != read.fingerprint())
        });
        Ok(drift.then_some("read state changed (newest messages)"))
    }

    /// The account's current JMAP Email state (`Email/get` with no ids).
    pub async fn email_state(&self, auth: &AuthenticatedSession) -> anyhow::Result<String> {
        let Some(account_id) = auth.session.primary_account_for(capabilities::MAIL) else {
            return Ok(String::new());
        };
        let response: JmapResponse<serde_json::Value> = self
            .api_call(
                auth,
                &[capabilities::CORE.to_owned(), capabilities::MAIL.to_owned()],
                vec![MethodCall::new(
                    "Email/get",
                    serde_json::json!({ "accountId": account_id, "ids": [], "properties": ["id"] }),
                    "0",
                )],
            )
            .await?;
        for method in response.method_responses {
            match method.0.as_str() {
                "Email/get" => {
                    return Ok(method.1["state"].as_str().unwrap_or_default().to_owned());
                }
                "error" => anyhow::bail!("JMAP method error reading Email state"),
                _ => {}
            }
        }
        anyhow::bail!("no Email/get response reading Email state")
    }

    /// Every message created or updated since `since_state` (JMAP
    /// Email/changes), with its read flag and whether it is in `mailbox_id`,
    /// plus destroyed ids. `None` when the server cannot calculate changes
    /// from that state (too old) or there are too many: the caller falls back
    /// to its own checks.
    pub async fn email_changes_since(
        &self,
        auth: &AuthenticatedSession,
        since_state: &str,
        mailbox_id: &str,
    ) -> anyhow::Result<Option<EmailChangesSince>> {
        // Each page's created/updated lists feed one Email/get each, which
        // Stalwart caps at 500 ids.
        const PAGE: usize = 400;
        const MAX_PAGES: usize = 25;
        let Some(account_id) = auth.session.primary_account_for(capabilities::MAIL) else {
            return Ok(None);
        };
        let identities = self.draft_identity_map(auth).await?;
        let mut out = EmailChangesSince {
            new_state: since_state.to_owned(),
            touched: BTreeMap::new(),
            destroyed: Vec::new(),
        };
        for _ in 0..MAX_PAGES {
            let get = |path: &str, id: &str| {
                MethodCall::new(
                    "Email/get",
                    serde_json::json!({
                        "accountId": account_id,
                        "#ids": { "resultOf": "c", "name": "Email/changes", "path": path },
                        "properties": ["id", "mailboxIds", "keywords"]
                    }),
                    id,
                )
            };
            let response: JmapResponse<serde_json::Value> = self
                .api_call(
                    auth,
                    &[capabilities::CORE.to_owned(), capabilities::MAIL.to_owned()],
                    vec![
                        MethodCall::new(
                            "Email/changes",
                            serde_json::json!({
                                "accountId": account_id,
                                "sinceState": out.new_state,
                                "maxChanges": PAGE
                            }),
                            "c",
                        ),
                        get("/created", "gc"),
                        get("/updated", "gu"),
                    ],
                )
                .await?;
            let mut more = false;
            for method in response.method_responses {
                match (method.0.as_str(), method.2.as_str()) {
                    ("Email/changes", _) => {
                        out.new_state = method.1["newState"]
                            .as_str()
                            .context("Email/changes without newState")?
                            .to_owned();
                        more = method.1["hasMoreChanges"].as_bool().unwrap_or(false);
                        if let Some(ids) = method.1["destroyed"].as_array() {
                            out.destroyed.extend(
                                ids.iter()
                                    .filter_map(|v| v.as_str())
                                    .filter(|id| !identities.by_stable.contains_key(*id))
                                    .map(|id| identities.stable(id).to_owned()),
                            );
                        }
                    }
                    ("Email/get", _) => {
                        let get: GetResponse<EmailMembership> = serde_json::from_value(method.1)
                            .context("invalid Email/get response for changes")?;
                        for e in get.list {
                            let read =
                                identities.state(&e.id, EmailState::from_keywords(&e.keywords));
                            let here = e.mailbox_ids.get(mailbox_id).copied().unwrap_or(false);
                            out.touched
                                .insert(identities.stable(&e.id).to_owned(), (read, here));
                        }
                    }
                    ("error", "c") => {
                        // cannotCalculateChanges and friends: fall back.
                        return Ok(None);
                    }
                    ("error", _) => anyhow::bail!("JMAP method error reading Email changes"),
                    _ => {}
                }
            }
            if !more {
                // A destroyed id may also have been listed as touched on an
                // earlier page.
                for id in &out.destroyed {
                    out.touched.remove(id);
                }
                return Ok(Some(out));
            }
        }
        Ok(None)
    }

    /// Native read and flagged keywords of messages that still exist.
    /// Includes held messages outside the newest listing window.
    pub async fn email_message_states(
        &self,
        auth: &AuthenticatedSession,
        ids: &[String],
    ) -> anyhow::Result<BTreeMap<String, EmailState>> {
        let mut out = BTreeMap::new();
        if ids.is_empty() {
            return Ok(out);
        }
        let Some(account_id) = auth.session.primary_account_for(capabilities::MAIL) else {
            return Ok(out);
        };
        let maximum = auth
            .session
            .capabilities
            .get(capabilities::CORE)
            .and_then(|cap| cap.get("maxObjectsInGet"))
            .and_then(|value| value.as_u64())
            .unwrap_or(100)
            .clamp(1, 1000) as usize;
        if ids.len() > maximum {
            for chunk in ids.chunks(maximum) {
                out.extend(Box::pin(self.email_message_states(auth, chunk)).await?);
            }
            return Ok(out);
        }
        let identities = self.draft_identity_map(auth).await?;
        let backend_ids: Vec<_> = ids
            .iter()
            .map(|id| identities.backend(id).to_owned())
            .collect();
        let response: JmapResponse<serde_json::Value> = self
            .api_call(
                auth,
                &[capabilities::CORE.to_owned(), capabilities::MAIL.to_owned()],
                vec![MethodCall::new(
                    "Email/get",
                    serde_json::json!({
                        "accountId": account_id,
                        "ids": backend_ids,
                        "properties": ["id", "keywords"]
                    }),
                    "0",
                )],
            )
            .await?;
        for method in response.method_responses {
            match method.0.as_str() {
                "Email/get" => {
                    let get: GetResponse<EmailMembership> = serde_json::from_value(method.1)
                        .context("invalid Email/get response for read states")?;
                    for email in get.list {
                        let read =
                            identities.state(&email.id, EmailState::from_keywords(&email.keywords));
                        out.insert(identities.stable(&email.id).to_owned(), read);
                    }
                }
                "error" => anyhow::bail!("JMAP method error reading message read states"),
                _ => {}
            }
        }
        Ok(out)
    }

    /// Lists contacts in a JSContact address book, newest-touched first.
    /// Contacts have no real two-way sync yet (this is read-only, like
    /// mail's own list-and-diff via seen_ids) -- see the
    /// `sync_contacts_collection` caller for that state-diffing.
    pub(crate) async fn query_objects<T: serde::de::DeserializeOwned + Default>(
        &self,
        auth: &AuthenticatedSession,
        capability: &str,
        object_type: &str,
        filter: serde_json::Value,
        properties: Option<&[&str]>,
        limit: usize,
    ) -> anyhow::Result<Vec<T>> {
        let Some(account_id) = auth.session.primary_account_for(capability) else {
            return Ok(Vec::new());
        };
        let query = format!("{object_type}/query");
        let get = format!("{object_type}/get");
        let mut objects = Vec::new();
        let mut position = 0;
        let mut query_state = None;
        loop {
            let page_size = if limit == 0 {
                200
            } else {
                (limit - position).min(200)
            };
            let mut get_args = serde_json::json!({
                "accountId": account_id,
                "#ids": { "resultOf": "q", "name": query, "path": "/ids" }
            });
            if let Some(properties) = properties {
                get_args["properties"] = serde_json::json!(properties);
            }
            let response: JmapResponse<serde_json::Value> = self
                .api_call(
                    auth,
                    &[capabilities::CORE.to_owned(), capability.to_owned()],
                    vec![
                        MethodCall::new(
                            &query,
                            serde_json::json!({
                                "accountId": account_id, "filter": filter,
                                "position": position, "limit": page_size
                            }),
                            "q",
                        ),
                        MethodCall::new(&get, get_args, "g"),
                    ],
                )
                .await?;
            let mut count = None;
            let mut page = None;
            for method in response.method_responses {
                if method.0 == "error" {
                    anyhow::bail!("JMAP method error in {object_type} snapshot");
                } else if method.0 == query {
                    let result: QueryResponse = serde_json::from_value(method.1)?;
                    if position != 0 && query_state != result.query_state {
                        anyhow::bail!("JMAP {object_type} changed during snapshot; retry Sync");
                    }
                    query_state = result.query_state;
                    count = Some(result.ids.len());
                } else if method.0 == get {
                    let result: GetResponse<T> = serde_json::from_value(method.1)?;
                    page = Some(result.list);
                }
            }
            let count = count.context("missing JMAP query response")?;
            let page = page.context("missing JMAP get response")?;
            anyhow::ensure!(
                page.len() == count,
                "JMAP {object_type} changed during snapshot; retry Sync"
            );
            objects.extend(page);
            position += count;
            if count == 0 || (limit != 0 && position >= limit) {
                return Ok(objects);
            }
        }
    }

    pub async fn contact_snapshot_state(
        &self,
        auth: &AuthenticatedSession,
    ) -> anyhow::Result<String> {
        let account = auth
            .session
            .primary_account_for(capabilities::CONTACTS)
            .context("JMAP Contacts capability is unavailable")?;
        let response: JmapResponse<serde_json::Value> = self
            .api_call(
                auth,
                &[
                    capabilities::CORE.to_owned(),
                    capabilities::CONTACTS.to_owned(),
                ],
                vec![MethodCall::new(
                    "ContactCard/get",
                    serde_json::json!({"accountId":account,"ids":[]}),
                    "snapshot",
                )],
            )
            .await?;
        response
            .method_responses
            .iter()
            .find(|method| method.0 == "ContactCard/get")
            .and_then(|method| method.1["state"].as_str())
            .map(str::to_owned)
            .context("contact snapshot state unavailable")
    }

    pub async fn search_contacts(
        &self,
        auth: &AuthenticatedSession,
        address_book: Option<&str>,
        terms: &[String],
        alternatives: Option<&[String]>,
        position: usize,
        limit: usize,
    ) -> anyhow::Result<(Vec<Contact>, u64)> {
        self.search_contacts_inner(
            auth,
            address_book,
            terms,
            alternatives,
            position,
            limit,
            false,
        )
        .await
    }

    pub async fn search_contacts_for_recipients(
        &self,
        auth: &AuthenticatedSession,
        queries: &[String],
    ) -> anyhow::Result<(Vec<Contact>, u64)> {
        self.search_contacts_inner(auth, None, &[], Some(queries), 0, 10000, true)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn search_contacts_inner(
        &self,
        auth: &AuthenticatedSession,
        address_book: Option<&str>,
        terms: &[String],
        alternatives: Option<&[String]>,
        position: usize,
        limit: usize,
        fetch_picture_blobs: bool,
    ) -> anyhow::Result<(Vec<Contact>, u64)> {
        let account = auth
            .session
            .primary_account_for(capabilities::CONTACTS)
            .context("JMAP Contacts capability is unavailable")?;
        let filter = match address_book {
            Some(book) => serde_json::json!({"inAddressBook":book}),
            None => serde_json::json!({}),
        };
        let maximum = auth
            .session
            .capabilities
            .get(capabilities::CORE)
            .and_then(|cap| cap.get("maxObjectsInGet"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(100)
            .clamp(1, 200);
        let using = [
            capabilities::CORE.to_owned(),
            capabilities::CONTACTS.to_owned(),
        ];
        let terms: Vec<_> = terms
            .iter()
            .filter(|text| !text.is_empty())
            .map(|text| text.to_lowercase())
            .collect();
        let alternatives = alternatives.map(|values| {
            values
                .iter()
                .filter(|text| !text.is_empty())
                .map(|text| text.to_lowercase())
                .collect::<Vec<_>>()
        });

        fn matches(value: &serde_json::Value, term: &str) -> bool {
            match value {
                serde_json::Value::String(value) => value.to_lowercase().contains(term),
                serde_json::Value::Array(values) => values.iter().any(|value| matches(value, term)),
                serde_json::Value::Object(values) => values
                    .iter()
                    .filter(|(key, _)| {
                        !matches!(
                            key.as_str(),
                            "@type" | "kind" | "contexts" | "language" | "pref"
                        )
                    })
                    .any(|(_, value)| matches(value, term)),
                _ => false,
            }
        }

        let fields = [
            "name",
            "emails",
            "phones",
            "organizations",
            "titles",
            "nicknames",
            "addresses",
            "anniversaries",
            "notes",
            "onlineServices",
            "media",
            "links",
            "keywords",
        ];
        let mut offset = 0u64;
        let mut query_state: Option<String> = None;
        let mut all_count = None;
        let mut total = 0u64;
        let mut contacts = Vec::new();
        let mut seen = BTreeSet::new();
        loop {
            let response: JmapResponse<serde_json::Value> = self.api_call(auth, &using, vec![
                MethodCall::new("ContactCard/query", serde_json::json!({
                    "accountId":account, "filter":filter, "position":offset,
                    "limit":maximum, "calculateTotal":true
                }), "q"),
                MethodCall::new("ContactCard/get", serde_json::json!({
                    "accountId":account, "#ids":{"resultOf":"q","name":"ContactCard/query","path":"/ids"},
                    "properties":["id","addressBookIds","name","emails","phones","organizations",
                        "titles","nicknames","addresses","anniversaries","notes","onlineServices","media","links","keywords"]
                }), "g"),
            ]).await?;
            let query = response
                .method_responses
                .iter()
                .find(|method| method.0 == "ContactCard/query")
                .context("contact search query failed")?;
            let query: QueryResponse = serde_json::from_value(query.1.clone())?;
            let state = query.query_state.context("contact search state missing")?;
            if let Some(previous) = &query_state {
                anyhow::ensure!(previous == &state, "contacts changed during search");
            } else {
                query_state = Some(state);
            }
            let count = query.total.context("contact search total missing")?;
            if let Some(previous) = all_count {
                anyhow::ensure!(previous == count, "contact search total changed");
            } else {
                all_count = Some(count);
            }
            let get = response
                .method_responses
                .iter()
                .find(|method| method.0 == "ContactCard/get")
                .context("contact search lookup failed")?;
            let get: GetResponse<serde_json::Value> = serde_json::from_value(get.1.clone())?;
            let mut by_id = BTreeMap::new();
            for item in get.list {
                let id = item["id"]
                    .as_str()
                    .context("contact ID missing")?
                    .to_owned();
                anyhow::ensure!(by_id.insert(id, item).is_none(), "duplicate contact lookup");
            }
            anyhow::ensure!(
                by_id.len() == query.ids.len(),
                "contact search changed during lookup"
            );
            let page_len = query.ids.len() as u64;
            for id in query.ids {
                anyhow::ensure!(seen.insert(id.clone()), "duplicate contact search result");
                let item = by_id
                    .remove(&id)
                    .context("contact search changed during lookup")?;
                let has = |term: &str| fields.iter().any(|field| matches(&item[*field], term));
                if terms.iter().all(|term| has(term))
                    && alternatives
                        .as_ref()
                        .is_none_or(|values| values.iter().any(|term| has(term)))
                {
                    if total >= position as u64 && contacts.len() < limit {
                        let card: ContactCardObject = serde_json::from_value(item)?;
                        let picture_blob = card
                            .media
                            .get("eas_picture")
                            .or_else(|| {
                                card.media
                                    .values()
                                    .find(|media| media.kind.as_deref() == Some("photo"))
                            })
                            .and_then(|media| media.blob_id.clone());
                        let mut contact = Contact::from(card);
                        if fetch_picture_blobs && contact.picture.is_none() {
                            if let Some(blob_id) = picture_blob {
                                if let Some((bytes, _)) = self
                                    .download_contact_blob_limited(
                                        auth,
                                        &blob_id,
                                        "contact-picture",
                                        102401,
                                    )
                                    .await?
                                {
                                    contact.picture = Some(bytes);
                                } else {
                                    // Preserve the over-limit condition so EAS can return
                                    // NoPictureTooBig without downloading an unbounded blob.
                                    contact.picture = Some(vec![0; 102401]);
                                }
                            }
                        }
                        contacts.push(contact);
                    }
                    total += 1;
                }
            }
            offset += page_len;
            anyhow::ensure!(offset <= count, "contact search exceeded total");
            if offset == count {
                break;
            }
            anyhow::ensure!(page_len > 0, "incomplete contact search");
        }
        let check: JmapResponse<serde_json::Value> = self.api_call(auth, &using, vec![
            MethodCall::new("ContactCard/query", serde_json::json!({
                "accountId":account, "filter":filter, "position":0, "limit":1, "calculateTotal":true
            }), "check")
        ]).await?;
        let check = check
            .method_responses
            .iter()
            .find(|method| method.0 == "ContactCard/query")
            .context("contact search verification failed")?;
        let check: QueryResponse = serde_json::from_value(check.1.clone())?;
        anyhow::ensure!(
            check.query_state == query_state && check.total == all_count,
            "contacts changed during search"
        );
        Ok((contacts, total))
    }

    pub async fn get_contact_by_id(
        &self,
        auth: &AuthenticatedSession,
        id: &str,
    ) -> anyhow::Result<Option<Contact>> {
        let account = auth
            .session
            .primary_account_for(capabilities::CONTACTS)
            .context("JMAP Contacts capability is unavailable")?;
        let response: JmapResponse<serde_json::Value> = self
            .api_call(
                auth,
                &[
                    capabilities::CORE.to_owned(),
                    capabilities::CONTACTS.to_owned(),
                ],
                vec![MethodCall::new(
                    "ContactCard/get",
                    serde_json::json!({
                        "accountId":account, "ids":[id],
                        "properties":["id","addressBookIds","name","emails","phones","organizations","titles","keywords","anniversaries","links","notes"]
                    }),
                    "g",
                )],
            )
            .await?;
        let result = response
            .method_responses
            .iter()
            .find(|method| method.0 == "ContactCard/get")
            .context("contact lookup failed")?;
        let get: GetResponse<ContactCardObject> = serde_json::from_value(result.1.clone())?;
        anyhow::ensure!(
            get.list.len() <= 1 && get.list.iter().all(|item| item.id == id),
            "contact lookup returned unexpected items"
        );
        Ok(get.list.into_iter().next().map(Contact::from))
    }

    pub async fn contacts_in_address_book(
        &self,
        auth: &AuthenticatedSession,
        address_book_id: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<Contact>> {
        let items: Vec<ContactCardObject> = self
            .query_objects(
                auth,
                capabilities::CONTACTS,
                "ContactCard",
                serde_json::json!({ "inAddressBook": address_book_id }),
                None,
                limit,
            )
            .await?;
        Ok(items.into_iter().map(Contact::from).collect())
    }

    /// Creates (`id: None`) or updates (`id: Some`) a contact from
    /// ActiveSync `ContactFields`, mapped to the same JSContact shape
    /// `ContactCardObject`/`Contact::from` reads back (name components,
    /// up to 3 emails, phones by `contexts`, one organization, one
    /// title). Returns the JMAP ContactCard id -- unlike Notes/Email,
    /// `ContactCard/set` genuinely supports in-place `update` (confirmed
    /// live: created a throwaway test card, updated `name/full` on the
    /// SAME id, read it back changed, destroyed it) -- so the id IS a
    /// stable ActiveSync ServerId across edits with no workaround needed,
    /// unlike the permanent-keyword-id trick `jmap::notes` needs because
    /// `Email/set` can't update subject/body at all.
    pub async fn save_contact(
        &self,
        auth: &AuthenticatedSession,
        address_book_id: &str,
        id: Option<&str>,
        fields: &crate::wbxml::eas::ContactFields,
    ) -> anyhow::Result<String> {
        anyhow::ensure!(!fields.unsupported_body, "unsupported contact notes body");
        let Some(account_id) = auth.session.primary_account_for(capabilities::CONTACTS) else {
            anyhow::bail!("JMAP Contacts capability is not available");
        };

        let mut card = serde_json::Map::new();

        let mut components = Vec::new();
        let yomi_first = fields
            .yomi_first_name
            .as_deref()
            .filter(|value| !value.is_empty());
        let yomi_last = fields
            .yomi_last_name
            .as_deref()
            .filter(|value| !value.is_empty());
        if let Some(first) = &fields.first_name {
            let mut component = serde_json::json!({"kind": "given", "value": first});
            if let Some(phonetic) = yomi_first {
                component["phonetic"] = serde_json::json!(phonetic);
            }
            components.push(component);
        } else if let Some(phonetic) = yomi_first {
            components.push(serde_json::json!({"kind":"given","value":"","phonetic":phonetic}));
        }
        if let Some(middle) = &fields.middle_name {
            components.push(serde_json::json!({"kind": "given2", "value": middle}));
        }
        if let Some(last) = &fields.last_name {
            let mut component = serde_json::json!({"kind": "surname", "value": last});
            if let Some(phonetic) = yomi_last {
                component["phonetic"] = serde_json::json!(phonetic);
            }
            components.push(component);
        } else if let Some(phonetic) = yomi_last {
            components.push(serde_json::json!({"kind":"surname","value":"","phonetic":phonetic}));
        }
        if let Some(suffix) = &fields.suffix {
            components.push(serde_json::json!({"kind": "generation", "value": suffix}));
        }
        if let Some(prefix) = &fields.name_prefix {
            components.push(serde_json::json!({"kind": "prefix", "value": prefix}));
        }
        let mut name = serde_json::Map::new();
        if let Some(file_as) = &fields.file_as {
            name.insert(
                "full".to_owned(),
                serde_json::Value::String(file_as.clone()),
            );
        }
        if yomi_first.is_some() || yomi_last.is_some() {
            name.insert("phoneticScript".to_owned(), serde_json::json!("Jpan"));
        }
        if !components.is_empty() {
            name.insert(
                "components".to_owned(),
                serde_json::Value::Array(components),
            );
        }
        let has_name_update =
            !name.is_empty() || fields.yomi_first_name.is_some() || fields.yomi_last_name.is_some();
        if !name.is_empty() {
            card.insert("name".to_owned(), serde_json::Value::Object(name));
        }
        let has_nickname_update = fields.nickname.is_some();
        let has_picture_update = fields.picture.is_some();
        let has_notes_update = fields.notes.is_some();
        if id.is_none() {
            if let Some(picture) = fields.picture.as_deref().filter(|value| !value.is_empty()) {
                card.insert("media".to_owned(), serde_json::json!({"eas_picture":{"@type":"Media","kind":"photo","uri":eas_contact_picture_to_data_uri(picture)?}}));
            }
            if let Some(notes) = &fields.notes {
                card.insert(
                    "notes".to_owned(),
                    serde_json::json!({"eas_notes":{"@type":"Note","note":notes}}),
                );
            }
        }
        if let Some(nickname) = fields.nickname.as_ref().filter(|value| !value.is_empty()) {
            if id.is_none() {
                card.insert(
                    "nicknames".to_owned(),
                    serde_json::json!({"eas_nickname":{"name":nickname}}),
                );
            }
        }
        let has_im_update = fields.im_address.is_some()
            || fields.im_address_2.is_some()
            || fields.im_address_3.is_some()
            || fields.mms.is_some()
            || fields.account_name.is_some();
        let mut online_services = serde_json::Map::new();
        for (key, label, value) in [
            ("eas_im1", "EAS IM Address 1", &fields.im_address),
            ("eas_im2", "EAS IM Address 2", &fields.im_address_2),
            ("eas_im3", "EAS IM Address 3", &fields.im_address_3),
        ] {
            if let Some(user) = value.as_ref().filter(|value| !value.is_empty()) {
                online_services.insert(
                    key.to_owned(),
                    serde_json::json!({"@type":"OnlineService","user":user,"label":label}),
                );
            }
        }
        if let Some(mms) = fields.mms.as_ref().filter(|value| !value.is_empty()) {
            online_services.insert(
                "eas_mms".to_owned(),
                serde_json::json!({"@type":"OnlineService","user":mms,"label":"EAS MMS"}),
            );
        }
        if let Some(account) = fields
            .account_name
            .as_ref()
            .filter(|value| !value.is_empty())
        {
            online_services.insert("eas_account_name".to_owned(), serde_json::json!({"@type":"OnlineService","user":account,"label":"EAS account name"}));
        }
        if id.is_none() && !online_services.is_empty() {
            card.insert(
                "onlineServices".to_owned(),
                serde_json::Value::Object(online_services.clone()),
            );
        }
        let mut anniversaries = serde_json::Map::new();
        for (key, kind, date) in [
            ("eas_birthday", "birth", &fields.birthday),
            ("eas_anniversary", "wedding", &fields.anniversary),
        ] {
            if let Some(date) = date.as_ref().filter(|date| !date.is_empty()) {
                anniversaries.insert(
                    key.to_owned(),
                    serde_json::json!({"@type":"Anniversary","kind":kind,"date":eas_contact_date_to_partial(date)?}),
                );
            }
        }
        let has_anniversary_update = fields.birthday.is_some() || fields.anniversary.is_some();
        if has_anniversary_update {
            card.insert(
                "anniversaries".to_owned(),
                serde_json::Value::Object(anniversaries.clone()),
            );
        }

        let mut emails = serde_json::Map::new();
        for (key, email) in [
            ("e1", &fields.email1_address),
            ("e2", &fields.email2_address),
            ("e3", &fields.email3_address),
        ] {
            if let Some(address) = email {
                emails.insert(key.to_owned(), serde_json::json!({ "address": address }));
            }
        }
        if !emails.is_empty() {
            card.insert("emails".to_owned(), serde_json::Value::Object(emails));
        }

        let mut phones = serde_json::Map::new();
        if let Some(mobile) = &fields.mobile_phone_number {
            phones.insert(
                "p_mobile".to_owned(),
                serde_json::json!({ "number": mobile, "contexts": { "mobile": true } }),
            );
        }
        if let Some(home) = &fields.home_phone_number {
            phones.insert(
                "p_home".to_owned(),
                serde_json::json!({ "number": home, "contexts": { "home": true } }),
            );
        }
        if let Some(home2) = &fields.home_2_phone_number {
            phones.insert(
                "p_home2".to_owned(),
                serde_json::json!({ "number": home2, "contexts": { "home": true } }),
            );
        }
        if let Some(business) = &fields.business_phone_number {
            phones.insert(
                "p_work".to_owned(),
                serde_json::json!({ "number": business, "contexts": { "work": true } }),
            );
        }
        if let Some(business2) = &fields.business_2_phone_number {
            phones.insert(
                "p_work2".to_owned(),
                serde_json::json!({ "number": business2, "contexts": { "work": true } }),
            );
        }
        if let Some(home_fax) = &fields.home_fax_number {
            phones.insert(
                "p_home_fax".to_owned(),
                serde_json::json!({ "number": home_fax, "contexts": { "private": true }, "features": { "fax": true } }),
            );
        }
        if let Some(business_fax) = &fields.business_fax_number {
            phones.insert(
                "p_work_fax".to_owned(),
                serde_json::json!({ "number": business_fax, "contexts": { "work": true }, "features": { "fax": true } }),
            );
        }
        if let Some(pager) = &fields.pager_number {
            phones.insert(
                "p_pager".to_owned(),
                serde_json::json!({ "number": pager, "features": { "pager": true } }),
            );
        }
        if let Some(car) = &fields.car_phone_number {
            phones.insert(
                "p_car".to_owned(),
                serde_json::json!({ "number": car, "label": "car" }),
            );
        }
        if let Some(radio) = &fields.radio_phone_number {
            phones.insert(
                "p_radio".to_owned(),
                serde_json::json!({ "number": radio, "label": "radio" }),
            );
        }
        if let Some(assistant_phone) = &fields.assistant_phone_number {
            phones.insert(
                "p_assistant".to_owned(),
                serde_json::json!({ "number": assistant_phone, "label": "assistant" }),
            );
        }
        if let Some(company_main) = &fields.company_main_phone {
            phones.insert(
                "eas_company_main".to_owned(),
                serde_json::json!({"number":company_main,"features":{"main-number":true}}),
            );
        }
        let has_phones_update = fields.company_main_phone.is_some()
            || fields.mobile_phone_number.is_some()
            || fields.home_phone_number.is_some()
            || fields.home_2_phone_number.is_some()
            || fields.business_phone_number.is_some()
            || fields.business_2_phone_number.is_some()
            || fields.home_fax_number.is_some()
            || fields.business_fax_number.is_some()
            || fields.pager_number.is_some()
            || fields.car_phone_number.is_some()
            || fields.radio_phone_number.is_some()
            || fields.assistant_phone_number.is_some();
        if id.is_none() && !phones.is_empty() {
            card.insert("phones".to_owned(), serde_json::Value::Object(phones));
        }
        let has_links_update = fields.web_page.is_some();
        if let Some(web_page) = fields.web_page.as_ref().filter(|value| !value.is_empty()) {
            if id.is_none() {
                card.insert(
                    "links".to_owned(),
                    serde_json::json!({"eas_web":{"uri":web_page}}),
                );
            }
        }

        let has_organizations_update = fields.company_name.is_some()
            || fields.department.is_some()
            || fields.yomi_company_name.is_some();
        let mut eas_organization = serde_json::Map::new();
        if let Some(company) = &fields.company_name {
            if !company.is_empty() {
                eas_organization.insert("name".to_owned(), serde_json::json!(company));
            }
        }
        if let Some(yomi_company) = fields
            .yomi_company_name
            .as_ref()
            .filter(|value| !value.is_empty())
        {
            eas_organization.insert("sortAs".to_owned(), serde_json::json!(yomi_company));
            eas_organization
                .entry("name".to_owned())
                .or_insert_with(|| serde_json::json!(""));
        }
        if let Some(department) = &fields.department {
            if !department.is_empty() {
                eas_organization.insert(
                    "units".to_owned(),
                    serde_json::json!([{"@type":"OrgUnit","name":department}]),
                );
            }
        }
        if id.is_none() && !eas_organization.is_empty() {
            card.insert(
                "organizations".to_owned(),
                serde_json::json!({ "o1": eas_organization }),
            );
        }
        if let Some(title) = &fields.job_title {
            card.insert(
                "titles".to_owned(),
                serde_json::json!({ "t1": { "name": title, "kind": "title" } }),
            );
        }

        let mut addresses = serde_json::Map::new();
        for (address_key, context, components) in [
            (
                "work",
                "work",
                [
                    ("locality", &fields.business_address_city),
                    ("country", &fields.business_address_country),
                    ("postcode", &fields.business_address_postal_code),
                    ("region", &fields.business_address_state),
                    ("name", &fields.business_address_street),
                ],
            ),
            (
                "home",
                "private",
                [
                    ("locality", &fields.home_address_city),
                    ("country", &fields.home_address_country),
                    ("postcode", &fields.home_address_postal_code),
                    ("region", &fields.home_address_state),
                    ("name", &fields.home_address_street),
                ],
            ),
            (
                "other",
                "other",
                [
                    ("locality", &fields.other_address_city),
                    ("country", &fields.other_address_country),
                    ("postcode", &fields.other_address_postal_code),
                    ("region", &fields.other_address_state),
                    ("name", &fields.other_address_street),
                ],
            ),
        ] {
            let values: Vec<_> = components
                .into_iter()
                .filter_map(|(kind, value)| {
                    value
                        .as_ref()
                        .filter(|value| id.is_some() || !value.is_empty())
                        .map(|value| serde_json::json!({"kind":kind,"value":value}))
                })
                .collect();
            if !values.is_empty() {
                addresses.insert(
                    address_key.to_owned(),
                    serde_json::json!({
                        "@type":"Address", "contexts":{context:true},
                        "components":values, "isOrdered":false
                    }),
                );
            }
        }
        let has_office_location_update = fields.office_location.is_some();
        if let Some(office_location) = fields
            .office_location
            .as_ref()
            .filter(|value| id.is_some() || !value.is_empty())
        {
            if id.is_none() {
                addresses.insert(
                    "eas_office_location".to_owned(),
                    serde_json::json!({"@type":"Address","label":"office location","full":office_location}),
                );
            }
        }
        let mut contact_state = None;
        if let Some(existing_id) = id.filter(|_| {
            has_name_update
                || has_nickname_update
                || has_im_update
                || has_picture_update
                || has_notes_update
                || has_anniversary_update
                || has_organizations_update
                || has_phones_update
                || has_links_update
                || !addresses.is_empty()
                || has_office_location_update
        }) {
            let mut properties = vec!["id"];
            if has_name_update {
                properties.push("name");
            }
            if has_nickname_update {
                properties.push("nicknames");
            }
            if has_im_update {
                properties.push("onlineServices");
            }
            if has_picture_update {
                properties.push("media");
            }
            if has_notes_update {
                properties.push("notes");
            }
            if !addresses.is_empty() || has_office_location_update {
                properties.push("addresses");
            }
            if has_anniversary_update {
                properties.push("anniversaries");
            }
            if has_organizations_update {
                properties.push("organizations");
            }
            if has_phones_update {
                properties.push("phones");
            }
            if has_links_update {
                properties.push("links");
            }
            let response: JmapResponse<serde_json::Value> = self
                .api_call(
                    auth,
                    &[capabilities::CORE.to_owned(), capabilities::CONTACTS.to_owned()],
                    vec![MethodCall::new(
                        "ContactCard/get",
                        serde_json::json!({"accountId":account_id,"ids":[existing_id],"properties":properties}),
                        "address-state",
                    )],
                )
                .await?;
            let result = response
                .method_responses
                .iter()
                .find(|method| method.0 == "ContactCard/get")
                .context("contact address lookup failed")?;
            contact_state = Some(
                result.1["state"]
                    .as_str()
                    .context("contact address state missing")?
                    .to_owned(),
            );
            let existing = result.1["list"]
                .as_array()
                .and_then(|list| list.iter().find(|item| item["id"] == existing_id))
                .context("contact not found while updating address")?;
            if has_name_update {
                let mut merged_name = existing["name"].as_object().cloned().unwrap_or_default();
                let mut components = merged_name
                    .get("components")
                    .and_then(serde_json::Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                for (kind, requested) in [
                    ("given", &fields.first_name),
                    ("given2", &fields.middle_name),
                    ("surname", &fields.last_name),
                    ("generation", &fields.suffix),
                    ("prefix", &fields.name_prefix),
                ] {
                    let Some(value) = requested else { continue };
                    let yomi_requested = match kind {
                        "given" => fields.yomi_first_name.is_some(),
                        "surname" => fields.yomi_last_name.is_some(),
                        _ => true,
                    };
                    let preserved_phonetic = (!yomi_requested)
                        .then(|| {
                            components
                                .iter()
                                .find(|component| component["kind"] == kind)
                                .and_then(|component| component["phonetic"].as_str())
                                .map(str::to_owned)
                        })
                        .flatten();
                    components.retain(|component| component["kind"] != kind);
                    if !value.is_empty() {
                        let mut component = serde_json::json!({"kind":kind,"value":value});
                        if let Some(phonetic) = preserved_phonetic {
                            component["phonetic"] = serde_json::json!(phonetic);
                        }
                        components.push(component);
                    }
                }
                for (kind, requested) in [
                    ("given", &fields.yomi_first_name),
                    ("surname", &fields.yomi_last_name),
                ] {
                    let Some(phonetic) = requested else { continue };
                    let selected = components
                        .iter_mut()
                        .find(|component| component["kind"] == kind);
                    if let Some(component) = selected {
                        if phonetic.is_empty() {
                            if component["value"].as_str().unwrap_or_default().is_empty() {
                                components.retain(|candidate| candidate["kind"] != kind);
                            } else if let Some(object) = component.as_object_mut() {
                                object.remove("phonetic");
                            }
                        } else if let Some(object) = component.as_object_mut() {
                            object.insert("phonetic".to_owned(), serde_json::json!(phonetic));
                        }
                    } else if !phonetic.is_empty() {
                        components
                            .push(serde_json::json!({"kind":kind,"value":"","phonetic":phonetic}));
                    }
                }
                if (fields.yomi_first_name.is_some() || fields.yomi_last_name.is_some())
                    && components
                        .iter()
                        .any(|component| component.get("phonetic").is_some())
                    && !merged_name.contains_key("phoneticScript")
                    && !merged_name.contains_key("phoneticSystem")
                {
                    merged_name.insert("phoneticScript".to_owned(), serde_json::json!("Jpan"));
                }
                if components.is_empty() {
                    merged_name.remove("components");
                } else {
                    merged_name.insert(
                        "components".to_owned(),
                        serde_json::Value::Array(components),
                    );
                }
                if let Some(file_as) = &fields.file_as {
                    if file_as.is_empty() {
                        merged_name.remove("full");
                    } else {
                        merged_name.insert("full".to_owned(), serde_json::json!(file_as));
                    }
                }
                card.insert(
                    "name".to_owned(),
                    if merged_name.is_empty() {
                        serde_json::Value::Null
                    } else {
                        serde_json::Value::Object(merged_name)
                    },
                );
            }
            if has_nickname_update {
                let mut nicknames = existing["nicknames"]
                    .as_object()
                    .cloned()
                    .unwrap_or_default();
                if let Some(nickname) = &fields.nickname {
                    if nickname.is_empty() {
                        nicknames.remove("eas_nickname");
                    } else {
                        nicknames.insert(
                            "eas_nickname".to_owned(),
                            serde_json::json!({"name":nickname}),
                        );
                    }
                }
                card.insert(
                    "nicknames".to_owned(),
                    if nicknames.is_empty() {
                        serde_json::Value::Null
                    } else {
                        serde_json::Value::Object(nicknames)
                    },
                );
            }
            if has_im_update {
                let mut services = existing["onlineServices"]
                    .as_object()
                    .cloned()
                    .unwrap_or_default();
                for (key, label, requested) in [
                    ("eas_im1", "EAS IM Address 1", &fields.im_address),
                    ("eas_im2", "EAS IM Address 2", &fields.im_address_2),
                    ("eas_im3", "EAS IM Address 3", &fields.im_address_3),
                    ("eas_mms", "EAS MMS", &fields.mms),
                    ("eas_account_name", "EAS account name", &fields.account_name),
                ] {
                    if let Some(user) = requested {
                        if user.is_empty() {
                            services.remove(key);
                        } else {
                            let mut service = services
                                .get(key)
                                .and_then(serde_json::Value::as_object)
                                .cloned()
                                .unwrap_or_default();
                            service.insert("@type".to_owned(), serde_json::json!("OnlineService"));
                            service.insert("user".to_owned(), serde_json::json!(user));
                            service.insert("label".to_owned(), serde_json::json!(label));
                            services.insert(key.to_owned(), serde_json::Value::Object(service));
                        }
                    }
                }
                card.insert(
                    "onlineServices".to_owned(),
                    if services.is_empty() {
                        serde_json::Value::Null
                    } else {
                        serde_json::Value::Object(services)
                    },
                );
            }
            if has_picture_update {
                let mut media = existing["media"].as_object().cloned().unwrap_or_default();
                if let Some(picture) = &fields.picture {
                    if picture.is_empty() {
                        media.remove("eas_picture");
                    } else {
                        media.insert("eas_picture".to_owned(), serde_json::json!({"@type":"Media","kind":"photo","uri":eas_contact_picture_to_data_uri(picture)?}));
                    }
                }
                card.insert(
                    "media".to_owned(),
                    if media.is_empty() {
                        serde_json::Value::Null
                    } else {
                        serde_json::Value::Object(media)
                    },
                );
            }
            if has_notes_update {
                let mut notes = existing["notes"].as_object().cloned().unwrap_or_default();
                let key = notes
                    .contains_key("eas_notes")
                    .then(|| "eas_notes".to_owned())
                    .or_else(|| notes.keys().next().cloned())
                    .unwrap_or_else(|| "eas_notes".to_owned());
                notes.insert(
                    key,
                    serde_json::json!({"@type":"Note","note":fields.notes.as_deref().unwrap_or_default()}),
                );
                card.insert("notes".to_owned(), serde_json::Value::Object(notes));
            }
            if has_anniversary_update {
                let mut merged_anniversaries = existing["anniversaries"]
                    .as_object()
                    .cloned()
                    .unwrap_or_default();
                for (key, kind, date) in [
                    ("eas_birthday", "birth", &fields.birthday),
                    ("eas_anniversary", "wedding", &fields.anniversary),
                ] {
                    if let Some(date) = date {
                        let existing_key = merged_anniversaries
                            .iter()
                            .find(|(_, value)| value["kind"] == kind)
                            .map(|(id, _)| id.clone());
                        if date.is_empty() {
                            if let Some(existing_key) = existing_key {
                                merged_anniversaries.remove(&existing_key);
                            } else {
                                merged_anniversaries.remove(key);
                            }
                        } else if let Some(updated) = anniversaries.get(key) {
                            merged_anniversaries.insert(
                                existing_key.unwrap_or_else(|| key.to_owned()),
                                updated.clone(),
                            );
                        }
                    }
                }
                card.insert(
                    "anniversaries".to_owned(),
                    if merged_anniversaries.is_empty() {
                        serde_json::Value::Null
                    } else {
                        serde_json::Value::Object(merged_anniversaries)
                    },
                );
            }
            if has_organizations_update {
                let mut merged_organizations = existing["organizations"]
                    .as_object()
                    .cloned()
                    .unwrap_or_default();
                let organization_id = merged_organizations
                    .iter()
                    .find(|(_, value)| value.get("name").is_some())
                    .map(|(id, _)| id.clone())
                    .or_else(|| merged_organizations.keys().next().cloned())
                    .unwrap_or_else(|| "o1".to_owned());
                let mut organization = merged_organizations
                    .get(&organization_id)
                    .and_then(serde_json::Value::as_object)
                    .cloned()
                    .unwrap_or_default();
                if let Some(company) = &fields.company_name {
                    if company.is_empty() {
                        organization.remove("name");
                        if fields.yomi_company_name.is_none()
                            || fields.yomi_company_name.as_deref() == Some("")
                        {
                            organization.remove("sortAs");
                        }
                    } else {
                        organization.insert("name".to_owned(), serde_json::json!(company));
                    }
                }
                if let Some(yomi_company) = &fields.yomi_company_name {
                    if yomi_company.is_empty() {
                        organization.remove("sortAs");
                    } else {
                        organization.insert("sortAs".to_owned(), serde_json::json!(yomi_company));
                        organization
                            .entry("name".to_owned())
                            .or_insert_with(|| serde_json::json!(""));
                    }
                }
                if let Some(department) = &fields.department {
                    if department.is_empty() {
                        organization.remove("units");
                    } else {
                        organization.insert(
                            "units".to_owned(),
                            serde_json::json!([{"@type":"OrgUnit","name":department}]),
                        );
                    }
                }
                if organization.is_empty() {
                    merged_organizations.remove(&organization_id);
                } else {
                    merged_organizations
                        .insert(organization_id, serde_json::Value::Object(organization));
                }
                card.insert(
                    "organizations".to_owned(),
                    if merged_organizations.is_empty() {
                        serde_json::Value::Null
                    } else {
                        serde_json::Value::Object(merged_organizations)
                    },
                );
            }
            if has_phones_update {
                let mut merged_phones = existing["phones"].as_object().cloned().unwrap_or_default();
                for (key, context, feature, label, rank, requested) in [
                    (
                        "p_mobile",
                        "mobile",
                        "",
                        "",
                        0usize,
                        &fields.mobile_phone_number,
                    ),
                    ("p_home", "home", "", "", 0usize, &fields.home_phone_number),
                    (
                        "p_home2",
                        "home",
                        "",
                        "",
                        1usize,
                        &fields.home_2_phone_number,
                    ),
                    (
                        "p_work",
                        "work",
                        "",
                        "",
                        0usize,
                        &fields.business_phone_number,
                    ),
                    (
                        "p_work2",
                        "work",
                        "",
                        "",
                        1usize,
                        &fields.business_2_phone_number,
                    ),
                    (
                        "p_home_fax",
                        "private",
                        "fax",
                        "",
                        0usize,
                        &fields.home_fax_number,
                    ),
                    (
                        "p_work_fax",
                        "work",
                        "fax",
                        "",
                        0usize,
                        &fields.business_fax_number,
                    ),
                    ("p_pager", "", "pager", "", 0usize, &fields.pager_number),
                    ("p_car", "", "", "car", 0usize, &fields.car_phone_number),
                    (
                        "p_radio",
                        "",
                        "",
                        "radio",
                        0usize,
                        &fields.radio_phone_number,
                    ),
                    (
                        "p_assistant",
                        "",
                        "",
                        "assistant",
                        0usize,
                        &fields.assistant_phone_number,
                    ),
                    (
                        "eas_company_main",
                        "",
                        "main-number",
                        "",
                        0usize,
                        &fields.company_main_phone,
                    ),
                ] {
                    let Some(number) = requested else { continue };
                    let matches: Vec<_> = merged_phones
                        .iter()
                        .filter(|(_, phone)| {
                            (context.is_empty() || phone["contexts"][context] == true)
                                && (feature.is_empty() || phone["features"][feature] == true)
                                && (label.is_empty() || phone["label"] == label)
                        })
                        .map(|(id, _)| id.clone())
                        .collect();
                    let selected = matches.get(rank).cloned();
                    if number.is_empty() {
                        if let Some(selected) = selected {
                            merged_phones.remove(&selected);
                        } else {
                            merged_phones.remove(key);
                        }
                        continue;
                    }
                    let selected = selected.unwrap_or_else(|| key.to_owned());
                    let mut phone = merged_phones
                        .get(&selected)
                        .and_then(serde_json::Value::as_object)
                        .cloned()
                        .unwrap_or_default();
                    phone.insert("number".to_owned(), serde_json::json!(number));
                    if !context.is_empty() {
                        let mut contexts = phone
                            .get("contexts")
                            .and_then(serde_json::Value::as_object)
                            .cloned()
                            .unwrap_or_default();
                        contexts.insert(context.to_owned(), serde_json::Value::Bool(true));
                        phone.insert("contexts".to_owned(), serde_json::Value::Object(contexts));
                    }
                    if !feature.is_empty() {
                        let mut features = phone
                            .get("features")
                            .and_then(serde_json::Value::as_object)
                            .cloned()
                            .unwrap_or_default();
                        features.insert(feature.to_owned(), serde_json::Value::Bool(true));
                        phone.insert("features".to_owned(), serde_json::Value::Object(features));
                    }
                    if !label.is_empty() {
                        phone.insert("label".to_owned(), serde_json::json!(label));
                    }
                    merged_phones.insert(selected, serde_json::Value::Object(phone));
                }
                card.insert(
                    "phones".to_owned(),
                    if merged_phones.is_empty() {
                        serde_json::Value::Null
                    } else {
                        serde_json::Value::Object(merged_phones)
                    },
                );
            }
            if has_links_update {
                let mut merged_links = existing["links"].as_object().cloned().unwrap_or_default();
                let website_key = merged_links
                    .iter()
                    .find(|(_, link)| {
                        link["uri"].as_str().is_some_and(|uri| {
                            uri.starts_with("http://") || uri.starts_with("https://")
                        })
                    })
                    .map(|(id, _)| id.clone())
                    .unwrap_or_else(|| "eas_web".to_owned());
                if let Some(web_page) = &fields.web_page {
                    if web_page.is_empty() {
                        merged_links.remove(&website_key);
                    } else {
                        let mut link = merged_links
                            .get(&website_key)
                            .and_then(serde_json::Value::as_object)
                            .cloned()
                            .unwrap_or_default();
                        link.insert("uri".to_owned(), serde_json::json!(web_page));
                        merged_links.insert(website_key, serde_json::Value::Object(link));
                    }
                }
                card.insert(
                    "links".to_owned(),
                    if merged_links.is_empty() {
                        serde_json::Value::Null
                    } else {
                        serde_json::Value::Object(merged_links)
                    },
                );
            }
            let mut merged = existing["addresses"]
                .as_object()
                .cloned()
                .unwrap_or_default();
            for (_, requested) in &addresses {
                if requested.get("label").is_some() {
                    continue;
                }
                let context = requested["contexts"]
                    .as_object()
                    .and_then(|contexts| contexts.keys().next())
                    .context("contact address context missing")?;
                let address_id = merged
                    .iter()
                    .find(|(_, address)| address["contexts"][context] == true)
                    .map(|(id, _)| id.clone())
                    .unwrap_or_else(|| format!("eas_{context}"));
                let mut address = merged
                    .get(&address_id)
                    .and_then(serde_json::Value::as_object)
                    .cloned()
                    .unwrap_or_default();
                let mut components: BTreeMap<String, serde_json::Value> = address
                    .get("components")
                    .and_then(serde_json::Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|component| {
                        Some((
                            component.get("kind")?.as_str()?.to_owned(),
                            component.clone(),
                        ))
                    })
                    .filter(|(kind, _)| kind != "separator")
                    .collect();
                for component in requested["components"]
                    .as_array()
                    .context("contact address components missing")?
                {
                    let kind = component["kind"]
                        .as_str()
                        .context("contact address component kind missing")?;
                    if component["value"].as_str() == Some("") {
                        components.remove(kind);
                    } else {
                        components.insert(kind.to_owned(), component.clone());
                    }
                }
                address.insert("@type".to_owned(), serde_json::json!("Address"));
                let mut contexts = address
                    .get("contexts")
                    .and_then(serde_json::Value::as_object)
                    .cloned()
                    .unwrap_or_default();
                contexts.insert(context.to_owned(), serde_json::Value::Bool(true));
                address.insert("contexts".to_owned(), serde_json::Value::Object(contexts));
                if components.is_empty() {
                    merged.remove(&address_id);
                } else {
                    address.insert(
                        "components".to_owned(),
                        serde_json::Value::Array(components.into_values().collect()),
                    );
                    address.insert("isOrdered".to_owned(), serde_json::Value::Bool(false));
                    address.remove("defaultSeparator");
                    address.remove("full");
                    merged.insert(address_id, serde_json::Value::Object(address));
                }
            }
            if let Some(office_location) = &fields.office_location {
                let office_id = merged
                    .iter()
                    .find(|(_, address)| {
                        address["label"]
                            .as_str()
                            .is_some_and(|label| label.eq_ignore_ascii_case("office location"))
                    })
                    .map(|(id, _)| id.clone())
                    .unwrap_or_else(|| "eas_office_location".to_owned());
                if office_location.is_empty() {
                    merged.remove(&office_id);
                } else {
                    merged.insert(
                        office_id,
                        serde_json::json!({"@type":"Address","label":"office location","full":office_location}),
                    );
                }
            }
            addresses = merged;
        }
        if !addresses.is_empty() {
            card.insert("addresses".to_owned(), serde_json::Value::Object(addresses));
        } else if contact_state.is_some() {
            card.insert("addresses".to_owned(), serde_json::Value::Null);
        }
        if let Some(categories) = &fields.categories {
            let keywords: BTreeMap<_, _> =
                categories.iter().map(|category| (category, true)).collect();
            card.insert(
                "keywords".to_owned(),
                if keywords.is_empty() {
                    serde_json::Value::Null
                } else {
                    serde_json::json!(keywords)
                },
            );
        }

        let call = match id {
            Some(existing_id) => {
                card.insert(
                    "@type".to_owned(),
                    serde_json::Value::String("Card".to_owned()),
                );
                card.insert(
                    "version".to_owned(),
                    serde_json::Value::String("1.0".to_owned()),
                );
                let mut arguments = serde_json::json!({
                    "accountId": account_id,
                    "update": { existing_id: card }
                });
                if let Some(state) = contact_state {
                    arguments["ifInState"] = serde_json::json!(state);
                }
                MethodCall::new("ContactCard/set", arguments, "0")
            }
            None => {
                card.insert(
                    "@type".to_owned(),
                    serde_json::Value::String("Card".to_owned()),
                );
                card.insert(
                    "version".to_owned(),
                    serde_json::Value::String("1.0".to_owned()),
                );
                card.insert(
                    "addressBookIds".to_owned(),
                    serde_json::json!({ address_book_id: true }),
                );
                MethodCall::new(
                    "ContactCard/set",
                    serde_json::json!({
                        "accountId": account_id,
                        "create": { "c1": card }
                    }),
                    "0",
                )
            }
        };

        let response: JmapResponse<serde_json::Value> = self
            .api_call(
                auth,
                &[
                    capabilities::CORE.to_owned(),
                    capabilities::CONTACTS.to_owned(),
                ],
                vec![call],
            )
            .await?;

        for method in response.method_responses {
            if method.0 == "error" {
                anyhow::bail!("JMAP method error in ContactCard/set");
            }
            if method.0 == "ContactCard/set" {
                if let Some(existing_id) = id {
                    let updated = method
                        .1
                        .get("updated")
                        .and_then(|value| value.get(existing_id));
                    if updated.is_some() {
                        return Ok(existing_id.to_owned());
                    }
                    anyhow::bail!(
                        "ContactCard/set did not confirm update for {existing_id}: {:?}",
                        method.1.get("notUpdated")
                    );
                }
                if let Some(new_id) = method
                    .1
                    .get("created")
                    .and_then(|c| c.get("c1"))
                    .and_then(|card| card.get("id"))
                    .and_then(|value| value.as_str())
                {
                    return Ok(new_id.to_owned());
                }
                anyhow::bail!(
                    "ContactCard/set did not return a created id: {:?}",
                    method.1.get("notCreated")
                );
            }
        }
        anyhow::bail!("ContactCard/set response was missing")
    }

    /// Idempotent, matching `destroy_note`/`destroy_email_by_id`'s own
    /// convention: an id that's already gone reads as success.
    pub async fn destroy_contact(
        &self,
        auth: &AuthenticatedSession,
        id: &str,
    ) -> anyhow::Result<()> {
        let Some(account_id) = auth.session.primary_account_for(capabilities::CONTACTS) else {
            anyhow::bail!("JMAP Contacts capability is not available");
        };
        let response: JmapResponse<serde_json::Value> = self
            .api_call(
                auth,
                &[
                    capabilities::CORE.to_owned(),
                    capabilities::CONTACTS.to_owned(),
                ],
                vec![MethodCall::new(
                    "ContactCard/set",
                    serde_json::json!({ "accountId": account_id, "destroy": [id] }),
                    "0",
                )],
            )
            .await?;

        for method in response.method_responses {
            if method.0 == "ContactCard/set" {
                let destroyed = method
                    .1
                    .get("destroyed")
                    .and_then(|value| value.as_array())
                    .is_some_and(|list| list.iter().any(|v| v.as_str() == Some(id)));
                if destroyed {
                    return Ok(());
                }
                let not_found = method
                    .1
                    .get("notDestroyed")
                    .and_then(|value| value.get(id))
                    .and_then(|entry| entry.get("type"))
                    .and_then(|t| t.as_str())
                    == Some("notFound");
                if not_found {
                    return Ok(());
                }
                anyhow::bail!("ContactCard/set destroy did not report success for {id}");
            }
        }
        anyhow::bail!("ContactCard/set response was missing")
    }

    /// Lists events in a JSCalendar (RFC 8984) calendar, mirroring the
    /// list-and-diff read pattern mail/contacts use. `start`/`timeZone`/
    /// `duration` are converted to UTC EAS DateTimes here (not left for
    /// the WBXML writer) so the conversion has one call site to get
    /// right -- see `local_to_utc_eas`/`calendar_end_utc`.
    /// Filters out `@type: "Task"` items -- `CalendarEvent/query` has no
    /// server-side type filter (same as `tasks_in_calendar` already notes)
    /// so this and the Tasks read path both list everything in the
    /// calendar and split client-side. Without this, a Task with no
    /// `start` (the common case today, since `save_task` never sets one)
    /// happened to not visibly break anything -- iOS's Calendar app just
    /// doesn't render a start-less event -- but that was luck, not a
    /// guarantee, so filter explicitly rather than rely on it.
    pub async fn calendar_snapshot_state(
        &self,
        auth: &AuthenticatedSession,
    ) -> anyhow::Result<(String, String)> {
        let account = auth
            .session
            .primary_account_for(capabilities::CALENDARS)
            .context("JMAP Calendars capability is unavailable")?;
        let response: JmapResponse<serde_json::Value> = self
            .api_call(
                auth,
                &[
                    capabilities::CORE.to_owned(),
                    capabilities::CALENDARS.to_owned(),
                ],
                vec![
                    MethodCall::new(
                        "Calendar/get",
                        serde_json::json!({"accountId": account, "ids": []}),
                        "c",
                    ),
                    MethodCall::new(
                        "CalendarEvent/get",
                        serde_json::json!({"accountId": account, "ids": []}),
                        "e",
                    ),
                ],
            )
            .await?;
        let state = |name: &str| -> anyhow::Result<String> {
            Ok(response
                .method_responses
                .iter()
                .find(|method| method.0 == name)
                .and_then(|method| method.1["state"].as_str())
                .context("calendar snapshot state unavailable")?
                .to_owned())
        };
        Ok((state("Calendar/get")?, state("CalendarEvent/get")?))
    }

    pub async fn calendar_events_in_calendar(
        &self,
        auth: &AuthenticatedSession,
        calendar_id: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<CalendarEvent>> {
        let items: Vec<CalendarEventObject> = self
            .query_objects(
                auth,
                capabilities::CALENDARS,
                "CalendarEvent",
                serde_json::json!({ "inCalendar": calendar_id }),
                Some(&[
                    "id",
                    "uid",
                    "@type",
                    "title",
                    "updated",
                    "description",
                    "descriptionContentType",
                    "start",
                    "timeZone",
                    "duration",
                    "status",
                    "locations",
                    "showWithoutTime",
                    "freeBusyStatus",
                    "privacy",
                    "keywords",
                    "alerts",
                    "useDefaultAlerts",
                    "recurrenceRule",
                    "recurrenceOverrides",
                    "participants",
                    "replyTo",
                    "links",
                    "virtualLocations",
                    "iCalendar",
                ]),
                limit,
            )
            .await?;
        let defaults =
            if items.iter().any(|item| item.use_default_alerts) {
                let account_id = auth
                    .session
                    .primary_account_for(capabilities::CALENDARS)
                    .context("JMAP Calendars capability is not available")?;
                let response: JmapResponse<serde_json::Value> = self.api_call(auth,
                &[capabilities::CORE.to_owned(), capabilities::CALENDARS.to_owned()],
                vec![MethodCall::new("Calendar/get", serde_json::json!({
                    "accountId":account_id, "ids":[calendar_id],
                    "properties":["id","defaultAlertsWithTime","defaultAlertsWithoutTime"]
                }), "defaults")]).await?;
                let result = response
                    .method_responses
                    .iter()
                    .find(|method| method.0 == "Calendar/get")
                    .context("calendar default alerts lookup failed")?;
                let item = result.1["list"]
                    .as_array()
                    .and_then(|items| items.iter().find(|item| item["id"] == calendar_id))
                    .context("calendar default alerts unavailable")?;
                for property in ["defaultAlertsWithTime", "defaultAlertsWithoutTime"] {
                    let value = item
                        .get(property)
                        .context("calendar default alerts missing")?;
                    if !value.is_null() && !value.is_object() {
                        anyhow::bail!("invalid calendar default alerts");
                    }
                }
                Some((
                    item["defaultAlertsWithTime"].clone(),
                    item["defaultAlertsWithoutTime"].clone(),
                ))
            } else {
                None
            };
        items
            .into_iter()
            .filter(|item| item.kind != "Task")
            .map(|mut item| {
                if item.use_default_alerts {
                    if let Some((with_time, without_time)) = &defaults {
                        let selected = if item.show_without_time {
                            without_time
                        } else {
                            with_time
                        };
                        if selected
                            .as_object()
                            .is_some_and(|alerts| !alerts.is_empty())
                        {
                            item.alerts = Some(selected.clone());
                        }
                    }
                }
                calendar_event_from(item, Some(auth.username()))
            })
            .map(|mut event| {
                anyhow::ensure!(
                    !event.unsupported_recurrence,
                    "calendar event recurrence cannot be represented by EAS"
                );
                event.is_organizer = event
                    .organizer_email
                    .as_deref()
                    .map(|email| email.eq_ignore_ascii_case(auth.username()))
                    .unwrap_or(true);
                event.response_type = calendar_response_type(&event, auth.username());
                Ok(event)
            })
            .collect::<anyhow::Result<Vec<_>>>()
    }

    pub async fn respond_calendar_event(
        &self,
        auth: &AuthenticatedSession,
        calendar_id: &str,
        event_id: &str,
        participation: &str,
        options: MeetingResponseOptions<'_>,
    ) -> anyhow::Result<&'static str> {
        let MeetingResponseOptions {
            expected_organizer,
            send_response,
            instance_id,
            custom_body,
            proposed_times,
        } = options;
        let account = auth
            .session
            .primary_account_for(capabilities::CALENDARS)
            .context("JMAP Calendars capability is unavailable")?;
        let using = [
            capabilities::CORE.to_owned(),
            capabilities::CALENDARS.to_owned(),
        ];
        let response: JmapResponse<serde_json::Value> = self.api_call(auth, &using,
            vec![MethodCall::new("CalendarEvent/get", serde_json::json!({"accountId":account,
                "ids":[event_id],"properties":["id","uid","title","description","locations","sequence","@type","calendarIds","participants","replyTo","timeZone","recurrenceRule","recurrenceOverrides"]}), "get")]).await?;
        let result = response
            .method_responses
            .iter()
            .find(|method| method.0 == "CalendarEvent/get")
            .context("calendar lookup failed")?;
        let Some(event) = result.1["list"]
            .as_array()
            .and_then(|items| items.iter().find(|event| event["id"] == event_id))
        else {
            return Ok("2");
        };
        if event["@type"] != "Event" || event["calendarIds"][calendar_id] != true {
            return Ok("2");
        }
        if proposed_times.is_some()
            && instance_id.is_none()
            && event["recurrenceRule"].is_object()
            && !series_counter_supported(event)
        {
            return Ok("4");
        }
        let instance_key = if let Some(instance_id) = instance_id {
            if event["recurrenceRule"].is_null() {
                return Ok("146");
            }
            let key = recurrence_override_key(instance_id, event["timeZone"].as_str())?;
            anyhow::ensure!(
                event["recurrenceOverrides"][&key]["excluded"] != true,
                "cannot respond to a deleted meeting occurrence"
            );
            Some(key)
        } else {
            None
        };
        let effective_participants = instance_key
            .as_deref()
            .and_then(|key| event["recurrenceOverrides"][key].get("participants"))
            .unwrap_or(&event["participants"]);
        let Some(participants) = effective_participants.as_object() else {
            return Ok("2");
        };
        let owners: Vec<_> = participants
            .values()
            .filter(|p| p["roles"]["owner"] == true)
            .collect();
        if owners.len() != 1
            || owners[0]["email"]
                .as_str()
                .is_none_or(|email| email.eq_ignore_ascii_case(auth.username()))
        {
            return Ok("2");
        }
        if expected_organizer.is_some_and(|expected| {
            owners[0]["email"]
                .as_str()
                .is_none_or(|email| !email.eq_ignore_ascii_case(expected))
        }) {
            return Ok("2");
        }
        let matches: Vec<_> = participants
            .iter()
            .filter(|(_, p)| {
                p["email"]
                    .as_str()
                    .is_some_and(|email| email.eq_ignore_ascii_case(auth.username()))
            })
            .collect();
        let [(key, participant)] = matches.as_slice() else {
            return Ok("2");
        };
        if participant["roles"]["owner"] == true {
            return Ok("2");
        }
        let state = result.1["state"]
            .as_str()
            .context("calendar state missing")?;
        let custom_mime = if custom_body.is_some() || proposed_times.is_some() {
            if !send_response
                || !auth.capabilities.submission
                || auth
                    .session
                    .primary_account_for(capabilities::MAIL)
                    .is_none()
            {
                return Ok("4");
            }
            let (identity_id, _) = self.fetch_primary_identity(auth).await?;
            if identity_id.is_empty() || self.find_mailbox_by_role(auth, "sent").await?.is_none() {
                return Ok("4");
            }
            let Some(uid) = event["uid"].as_str() else {
                return Ok("4");
            };
            let Some(organizer) = owners[0]["email"].as_str() else {
                return Ok("4");
            };
            let Some(attendee) = participant["email"].as_str() else {
                return Ok("4");
            };
            let location = counter_event_location(&event["locations"]);
            Some(meeting_response_mime(
                custom_body.as_ref(),
                proposed_times.as_ref(),
                MeetingResponseMimeContext {
                    uid,
                    organizer,
                    attendee,
                    participation,
                    recurrence_key: instance_key.as_deref(),
                    instance_id,
                    time_zone: event["timeZone"].as_str(),
                    title: event["title"].as_str().unwrap_or(""),
                    description: event["description"].as_str(),
                    location: location.as_deref(),
                    sequence: event["sequence"].as_u64().unwrap_or(0),
                    recurrence_rule: event["recurrenceRule"]
                        .as_object()
                        .map(|_| &event["recurrenceRule"]),
                },
            )?)
        } else {
            None
        };
        let send_scheduling_messages = send_response && custom_mime.is_none();
        if participation == "declined" && instance_key.is_none() {
            if auth.capabilities.submission {
                let identities = self.mail_identities(auth).await?;
                if identities.iter().any(|identity| {
                    owners[0]["email"]
                        .as_str()
                        .is_some_and(|email| email.eq_ignore_ascii_case(&identity.email))
                }) {
                    return Ok("2");
                }
            }
            let calendars = event["calendarIds"]
                .as_object()
                .context("meeting calendar membership missing")?;
            let count = calendars.values().filter(|value| **value == true).count();
            let arguments = if count == 1 {
                serde_json::json!({"accountId":account,"ifInState":state,"sendSchedulingMessages":send_scheduling_messages,"destroy":[event_id]})
            } else {
                let path = format!(
                    "calendarIds/{}",
                    calendar_id.replace('~', "~0").replace('/', "~1")
                );
                serde_json::json!({"accountId":account,"ifInState":state,"sendSchedulingMessages":send_scheduling_messages,"update":{event_id:{path:null}}})
            };
            let response: JmapResponse<serde_json::Value> = self
                .api_call(
                    auth,
                    &using,
                    vec![MethodCall::new("CalendarEvent/set", arguments, "decline")],
                )
                .await?;
            let result = response
                .method_responses
                .iter()
                .find(|method| method.0 == "CalendarEvent/set")
                .context("meeting decline failed")?;
            let confirmed = if count == 1 {
                result.1["destroyed"]
                    .as_array()
                    .is_some_and(|ids| ids.iter().any(|id| id == event_id))
            } else {
                result.1["updated"]
                    .as_object()
                    .is_some_and(|items| items.contains_key(event_id))
            };
            anyhow::ensure!(confirmed, "meeting decline was not confirmed");
            let response: JmapResponse<serde_json::Value> = self
                .api_call(
                    auth,
                    &using,
                    vec![MethodCall::new(
                        "CalendarEvent/get",
                        serde_json::json!({"accountId":account,
                    "ids":[event_id],"properties":["id","calendarIds"]}),
                        "verify",
                    )],
                )
                .await?;
            let result = response
                .method_responses
                .iter()
                .find(|method| method.0 == "CalendarEvent/get")
                .context("meeting decline verification failed")?;
            let items = result.1["list"]
                .as_array()
                .context("meeting decline verification failed")?;
            if count == 1 {
                anyhow::ensure!(
                    !items.iter().any(|item| item["id"] == event_id),
                    "declined meeting still exists"
                );
            } else {
                let remaining = items
                    .iter()
                    .find(|item| item["id"] == event_id)
                    .context("shared meeting disappeared")?;
                anyhow::ensure!(
                    remaining["calendarIds"][calendar_id] != true,
                    "declined calendar still contains meeting"
                );
                for (id, member) in calendars {
                    if id != calendar_id && *member == true {
                        anyhow::ensure!(
                            remaining["calendarIds"][id] == true,
                            "another calendar lost the meeting"
                        );
                    }
                }
            }
            if let Some(mime) = custom_mime {
                self.send_email(auth, mime, true).await?;
            }
            return Ok("1");
        }
        let participant_id = (*key).clone();
        let original_roles = participant["roles"].clone();
        let owner_email = owners[0]["email"].clone();
        let participant_path = key.replace('~', "~0").replace('/', "~1");
        let mut update = serde_json::Map::new();
        let mut exception_statuses = Vec::new();
        if let Some(instance_key) = instance_key.as_deref() {
            let mut occurrence_participants = participants.clone();
            occurrence_participants
                .get_mut(&participant_id)
                .context("meeting attendee disappeared")?["participationStatus"] =
                serde_json::json!(participation);
            let path = format!("recurrenceOverrides/{instance_key}/participants");
            update.insert(path, serde_json::Value::Object(occurrence_participants));
            exception_statuses.push((instance_key.to_owned(), participant_id.clone()));
        } else {
            let path = format!("participants/{participant_path}/participationStatus");
            update.insert(path, serde_json::json!(participation));
            update.insert(
                format!("participants/{participant_path}/roles"),
                participant["roles"].clone(),
            );
            if let Some(overrides) = event["recurrenceOverrides"].as_object() {
                for (key, override_value) in overrides {
                    let Some(override_participants) = override_value
                        .get("participants")
                        .and_then(|value| value.as_object())
                    else {
                        continue;
                    };
                    let Some((override_id, _)) = override_participants.iter().find(|(_, value)| {
                        value["email"]
                            .as_str()
                            .is_some_and(|email| email.eq_ignore_ascii_case(auth.username()))
                    }) else {
                        continue;
                    };
                    let mut updated_participants = override_participants.clone();
                    updated_participants
                        .get_mut(override_id)
                        .context("occurrence attendee disappeared")?["participationStatus"] =
                        serde_json::json!(participation);
                    let key = key.replace('~', "~0").replace('/', "~1");
                    update.insert(
                        format!("recurrenceOverrides/{key}/participants"),
                        serde_json::Value::Object(updated_participants),
                    );
                    exception_statuses.push((key, override_id.clone()));
                }
            }
        }
        let response: JmapResponse<serde_json::Value> = self
            .api_call(
                auth,
                &using,
                vec![MethodCall::new(
                    "CalendarEvent/set",
                    serde_json::json!({"accountId":account,
                "ifInState":state,"sendSchedulingMessages":send_scheduling_messages,
                "update":{event_id:update}}),
                    "set",
                )],
            )
            .await?;
        let result = response
            .method_responses
            .iter()
            .find(|method| method.0 == "CalendarEvent/set")
            .context("calendar response update failed")?;
        anyhow::ensure!(
            result.1["updated"]
                .as_object()
                .is_some_and(|items| items.contains_key(event_id)),
            "calendar response update was not confirmed"
        );
        let response: JmapResponse<serde_json::Value> = self
            .api_call(
                auth,
                &using,
                vec![MethodCall::new(
                    "CalendarEvent/get",
                    serde_json::json!({"accountId":account,
                "ids":[event_id],"properties":["id","participants","recurrenceOverrides"]}),
                    "verify",
                )],
            )
            .await?;
        let verified_event = response
            .method_responses
            .iter()
            .find(|method| method.0 == "CalendarEvent/get")
            .and_then(|method| method.1["list"].as_array())
            .and_then(|items| items.iter().find(|event| event["id"] == event_id))
            .context("meeting response verification failed")?;
        let verified_participants = instance_key
            .as_deref()
            .and_then(|key| verified_event["recurrenceOverrides"][key].get("participants"))
            .unwrap_or(&verified_event["participants"]);
        let participants = verified_participants
            .as_object()
            .context("meeting response verification failed")?;
        let attendee = participants
            .get(&participant_id)
            .context("meeting attendee disappeared")?;
        anyhow::ensure!(
            attendee["participationStatus"] == participation && attendee["roles"] == original_roles,
            "meeting attendee state was not retained"
        );
        for (key, participant_id) in exception_statuses {
            let key = key.replace('~', "~0").replace('/', "~1");
            anyhow::ensure!(
                verified_event["recurrenceOverrides"][key]["participants"][participant_id]
                    ["participationStatus"]
                    == participation,
                "meeting occurrence response was not retained"
            );
        }
        let owners: Vec<_> = participants
            .values()
            .filter(|p| p["roles"]["owner"] == true)
            .collect();
        anyhow::ensure!(
            owners.len() == 1 && owners[0]["email"] == owner_email,
            "meeting organizer was not retained"
        );
        if let Some(mime) = custom_mime {
            self.send_email(auth, mime, true).await?;
        }
        Ok("1")
    }

    pub async fn calendar_event_for_invitation(
        &self,
        auth: &AuthenticatedSession,
        mailbox: &str,
        email_id: &str,
        participation: &str,
    ) -> anyhow::Result<Option<(String, String, String)>> {
        let mail_account = auth
            .session
            .primary_account_for(capabilities::MAIL)
            .context("JMAP Mail capability is unavailable")?;
        let calendar_account = auth
            .session
            .primary_account_for(capabilities::CALENDARS)
            .context("JMAP Calendars capability is unavailable")?;
        let response: JmapResponse<serde_json::Value> = self
            .api_call(
                auth,
                &[capabilities::CORE.to_owned(), capabilities::MAIL.to_owned()],
                vec![MethodCall::new(
                    "Email/get",
                    serde_json::json!({"accountId":mail_account,"ids":[email_id],
                "properties":["id","mailboxIds","bodyStructure"]}),
                    "mail",
                )],
            )
            .await?;
        let result = response
            .method_responses
            .iter()
            .find(|method| method.0 == "Email/get")
            .context("meeting invitation lookup failed")?;
        let Some(email) = result.1["list"]
            .as_array()
            .and_then(|items| items.iter().find(|item| item["id"] == email_id))
        else {
            return Ok(None);
        };
        if email["mailboxIds"][mailbox] != true {
            return Ok(None);
        }
        fn calendar_blobs(part: &serde_json::Value, blobs: &mut Vec<String>) {
            if part["type"]
                .as_str()
                .is_some_and(|kind| kind.eq_ignore_ascii_case("text/calendar"))
            {
                if let Some(id) = part["blobId"].as_str() {
                    blobs.push(id.to_owned());
                }
            }
            if let Some(children) = part["subParts"].as_array() {
                for child in children {
                    calendar_blobs(child, blobs);
                }
            }
        }
        let mut blobs = Vec::new();
        calendar_blobs(&email["bodyStructure"], &mut blobs);
        let [blob] = blobs.as_slice() else {
            return Ok(None);
        };
        let using = [
            capabilities::CORE.to_owned(),
            capabilities::CALENDARS.to_owned(),
        ];
        let response: JmapResponse<serde_json::Value> = self
            .api_call(
                auth,
                &using,
                vec![
                    MethodCall::new(
                        "CalendarEvent/parse",
                        serde_json::json!({"accountId":calendar_account,"blobIds":[blob]}),
                        "parse",
                    ),
                    MethodCall::new(
                        "Calendar/get",
                        serde_json::json!({"accountId":calendar_account,"properties":["id","isDefault"]}),
                        "calendars",
                    ),
                ],
            )
            .await?;
        let parsed = response
            .method_responses
            .iter()
            .find(|method| method.0 == "CalendarEvent/parse")
            .and_then(|method| method.1["parsed"][blob].as_array())
            .context("meeting invitation could not be parsed")?;
        let [event] = parsed.as_slice() else {
            return Ok(None);
        };
        if event["@type"] != "Event"
            || event["method"]
                .as_str()
                .is_none_or(|method| !method.eq_ignore_ascii_case("request"))
        {
            return Ok(None);
        }
        let Some(uid) = event["uid"].as_str().filter(|uid| !uid.is_empty()) else {
            return Ok(None);
        };
        let Some(organizer) = event["organizerCalendarAddress"]
            .as_str()
            .or_else(|| event["replyTo"]["imip"].as_str())
            .and_then(|uri| {
                uri.get(..7)
                    .filter(|prefix| prefix.eq_ignore_ascii_case("mailto:"))
                    .and_then(|_| uri.get(7..))
            })
            .filter(|email| !email.is_empty())
        else {
            return Ok(None);
        };
        let calendar_result = response
            .method_responses
            .iter()
            .find(|method| method.0 == "Calendar/get")
            .context("calendar list unavailable")?;
        let calendars = calendar_result.1["list"]
            .as_array()
            .context("calendar list unavailable")?;
        let before = self.calendar_snapshot_state(auth).await?;
        anyhow::ensure!(
            calendar_result.1["state"].as_str() == Some(before.0.as_str()),
            "calendar list changed during invitation lookup"
        );
        let mut found = Vec::new();
        for calendar in calendars {
            let id = calendar["id"].as_str().context("calendar id missing")?;
            if let Some(event_id) = self.calendar_event_by_uid(auth, id, uid).await? {
                found.push((id.to_owned(), event_id, organizer.to_owned()));
            }
        }
        match found.as_slice() {
            [item] => return Ok(Some(item.clone())),
            [] => {}
            _ => anyhow::bail!("meeting invitation matches multiple calendar events"),
        }
        anyhow::ensure!(
            before == self.calendar_snapshot_state(auth).await?,
            "calendars changed during invitation lookup"
        );
        let defaults: Vec<_> = calendars
            .iter()
            .filter(|calendar| calendar["isDefault"] == true)
            .collect();
        let calendar = match defaults.as_slice() {
            [calendar] => *calendar,
            [] if calendars.len() == 1 => &calendars[0],
            _ => anyhow::bail!("default calendar unavailable"),
        };
        let calendar_id = calendar["id"].as_str().context("calendar id missing")?;
        if auth.capabilities.submission
            && self
                .mail_identities(auth)
                .await?
                .iter()
                .any(|identity| identity.email.eq_ignore_ascii_case(organizer))
        {
            return Ok(None);
        }
        let Some(mut created) = invitation_event_for_create(
            event,
            organizer,
            auth.username(),
            calendar_id,
            participation,
        )?
        else {
            return Ok(None);
        };
        if participation == "declined" {
            return Ok(Some((
                calendar_id.to_owned(),
                String::new(),
                organizer.to_owned(),
            )));
        }
        let response: JmapResponse<serde_json::Value> = self.api_call(auth, &using,
            vec![MethodCall::new("CalendarEvent/set", serde_json::json!({"accountId":calendar_account,
                "ifInState":before.1,"sendSchedulingMessages":false,"create":{"invitation":created.take()}}), "create")]).await?;
        let result = response
            .method_responses
            .iter()
            .find(|method| method.0 == "CalendarEvent/set")
            .context("meeting creation failed")?;
        let event_id = result.1["created"]["invitation"]["id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .context("meeting creation was not confirmed")?;
        Ok(Some((
            calendar_id.to_owned(),
            event_id.to_owned(),
            organizer.to_owned(),
        )))
    }

    pub async fn calendar_event_by_uid(
        &self,
        auth: &AuthenticatedSession,
        calendar_id: &str,
        uid: &str,
    ) -> anyhow::Result<Option<String>> {
        let items: Vec<CalendarEventObject> = self.query_objects(
            auth, capabilities::CALENDARS, "CalendarEvent",
            serde_json::json!({"operator":"AND","conditions":[{"inCalendar":calendar_id},{"uid":uid}]}),
            Some(&["id","uid"]), 2,
        ).await?;
        match items.as_slice() {
            [] => Ok(None),
            [item] if item.uid.as_deref() == Some(uid) => Ok(Some(item.id.clone())),
            _ => anyhow::bail!("ambiguous calendar UID"),
        }
    }

    async fn verify_calendar_coordinates(
        &self,
        auth: &AuthenticatedSession,
        id: &str,
        expected: &(String, String),
    ) -> anyhow::Result<()> {
        let account = auth
            .session
            .primary_account_for(capabilities::CALENDARS)
            .context("JMAP Calendars capability is unavailable")?;
        let response: JmapResponse<serde_json::Value> = self.api_call(auth,
            &[capabilities::CORE.to_owned(), capabilities::CALENDARS.to_owned()],
            vec![MethodCall::new("CalendarEvent/get", serde_json::json!({"accountId":account,"ids":[id],"properties":["id","locations"]}), "get")]).await?;
        let item = response
            .method_responses
            .iter()
            .find(|method| method.0 == "CalendarEvent/get")
            .and_then(|method| method.1["list"].as_array())
            .and_then(|items| items.iter().find(|item| item["id"] == id))
            .context("calendar coordinate verification failed")?;
        let expected = format!("geo:{},{}", expected.0, expected.1);
        anyhow::ensure!(
            item["locations"]
                .as_object()
                .is_some_and(|locations| locations
                    .values()
                    .any(|location| location["coordinates"] == expected)),
            "JMAP did not retain calendar coordinates"
        );
        Ok(())
    }

    async fn verify_calendar_disallow_new_time_proposal(
        &self,
        auth: &AuthenticatedSession,
        id: &str,
        expected: bool,
    ) -> anyhow::Result<()> {
        let account = auth
            .session
            .primary_account_for(capabilities::CALENDARS)
            .context("JMAP Calendars capability is unavailable")?;
        let response: JmapResponse<serde_json::Value> = self
            .api_call(
                auth,
                &[capabilities::CORE.to_owned(), capabilities::CALENDARS.to_owned()],
                vec![MethodCall::new(
                    "CalendarEvent/get",
                    serde_json::json!({"accountId":account,"ids":[id],"properties":["id","iCalendar"]}),
                    "verify-disallow-counter",
                )],
            )
            .await?;
        let item = response
            .method_responses
            .iter()
            .find(|method| method.0 == "CalendarEvent/get")
            .and_then(|method| method.1["list"].as_array())
            .and_then(|items| items.iter().find(|item| item["id"] == id))
            .context("calendar proposal verification failed")?;
        anyhow::ensure!(
            calendar_disallow_new_time_proposal(item.get("iCalendar")) == Some(expected),
            "JMAP did not retain DisallowNewTimeProposal"
        );
        Ok(())
    }

    pub async fn verify_calendar_reminder(
        &self,
        auth: &AuthenticatedSession,
        id: &str,
        reminder: Option<u32>,
    ) -> anyhow::Result<()> {
        let account = auth
            .session
            .primary_account_for(capabilities::CALENDARS)
            .context("JMAP Calendars capability is not available")?;
        let response: JmapResponse<serde_json::Value> = self.api_call(auth,
            &[capabilities::CORE.to_owned(), capabilities::CALENDARS.to_owned()],
            vec![MethodCall::new("CalendarEvent/get",serde_json::json!({
                "accountId":account,"ids":[id],"properties":["id","alerts","useDefaultAlerts"]
            }),"verify")]).await?;
        let item = response
            .method_responses
            .iter()
            .find(|method| method.0 == "CalendarEvent/get")
            .and_then(|method| method.1["list"].as_array())
            .and_then(|items| items.iter().find(|item| item["id"] == id))
            .context("calendar reminder verification failed")?;
        if item["useDefaultAlerts"] != false
            || !crate::jmap::calendar_reminder::unchanged(&item["alerts"], reminder)?
        {
            anyhow::bail!("JMAP did not retain the requested calendar reminder");
        }
        Ok(())
    }

    pub async fn verify_calendar_privacy(
        &self,
        auth: &AuthenticatedSession,
        id: &str,
        sensitivity: u8,
    ) -> anyhow::Result<()> {
        let expected = match sensitivity {
            0 => "public",
            2 => "private",
            _ => anyhow::bail!("unsupported calendar sensitivity"),
        };
        let account = auth
            .session
            .primary_account_for(capabilities::CALENDARS)
            .context("JMAP Calendars capability is not available")?;
        let response: JmapResponse<serde_json::Value> = self
            .api_call(
                auth,
                &[
                    capabilities::CORE.to_owned(),
                    capabilities::CALENDARS.to_owned(),
                ],
                vec![MethodCall::new(
                    "CalendarEvent/get",
                    serde_json::json!({
                        "accountId":account, "ids":[id], "properties":["id","privacy"]
                    }),
                    "verify",
                )],
            )
            .await?;
        let item = response
            .method_responses
            .iter()
            .find(|method| method.0 == "CalendarEvent/get")
            .and_then(|method| method.1["list"].as_array())
            .and_then(|items| items.iter().find(|item| item["id"] == id))
            .context("calendar privacy verification failed")?;
        let privacy = match item.get("privacy") {
            None | Some(serde_json::Value::Null) => "public",
            Some(serde_json::Value::String(value)) => value,
            _ => anyhow::bail!("invalid calendar privacy"),
        };
        anyhow::ensure!(privacy == expected, "JMAP did not retain calendar privacy");
        Ok(())
    }

    pub async fn verify_calendar_categories(
        &self,
        auth: &AuthenticatedSession,
        id: &str,
        categories: &[String],
    ) -> anyhow::Result<()> {
        let account = auth
            .session
            .primary_account_for(capabilities::CALENDARS)
            .context("JMAP Calendars capability is not available")?;
        let response: JmapResponse<serde_json::Value> = self
            .api_call(
                auth,
                &[
                    capabilities::CORE.to_owned(),
                    capabilities::CALENDARS.to_owned(),
                ],
                vec![MethodCall::new(
                    "CalendarEvent/get",
                    serde_json::json!({
                        "accountId":account, "ids":[id], "properties":["id","keywords"]
                    }),
                    "verify",
                )],
            )
            .await?;
        let item = response
            .method_responses
            .iter()
            .find(|method| method.0 == "CalendarEvent/get")
            .and_then(|method| method.1["list"].as_array())
            .and_then(|items| items.iter().find(|item| item["id"] == id))
            .context("calendar categories verification failed")?;
        let expected: BTreeMap<_, _> = categories.iter().map(|name| (name.clone(), true)).collect();
        let observed: BTreeMap<String, bool> = match &item["keywords"] {
            serde_json::Value::Null => BTreeMap::new(),
            value => serde_json::from_value(value.clone())?,
        };
        anyhow::ensure!(
            observed == expected,
            "JMAP did not retain calendar categories"
        );
        Ok(())
    }

    pub async fn verify_calendar_recurrence(
        &self,
        auth: &AuthenticatedSession,
        id: &str,
        expected: Option<&crate::wbxml::eas::CalendarRecurrence>,
    ) -> anyhow::Result<()> {
        let account = auth
            .session
            .primary_account_for(capabilities::CALENDARS)
            .context("JMAP Calendars capability is not available")?;
        let response: JmapResponse<serde_json::Value> = self
            .api_call(
                auth,
                &[capabilities::CORE.to_owned(), capabilities::CALENDARS.to_owned()],
                vec![MethodCall::new(
                    "CalendarEvent/get",
                    serde_json::json!({"accountId":account,"ids":[id],"properties":["id","timeZone","recurrenceRule","recurrenceOverrides"]}),
                    "verify",
                )],
            )
            .await?;
        let item = response
            .method_responses
            .iter()
            .find(|method| method.0 == "CalendarEvent/get")
            .and_then(|method| method.1["list"].as_array())
            .and_then(|items| items.iter().find(|item| item["id"] == id))
            .context("calendar recurrence verification failed")?;
        anyhow::ensure!(
            item.get("recurrenceOverrides")
                .and_then(serde_json::Value::as_object)
                .is_none_or(|overrides| overrides.is_empty()),
            "JMAP returned unsupported recurrence exceptions"
        );
        let observed = match item.get("recurrenceRule").filter(|rule| !rule.is_null()) {
            None => None,
            Some(rule) => Some(crate::jmap::calendar_recurrence::from_jscalendar(
                rule,
                item["timeZone"].as_str(),
            )?),
        };
        anyhow::ensure!(
            observed.as_ref() == expected,
            "JMAP did not retain the requested calendar recurrence"
        );
        Ok(())
    }

    /// Creates (`id: None`) or updates (`id: Some`) a calendar event from
    /// ActiveSync `CalendarFields`. EAS recurrence rules map to a single
    /// Stalwart's singular JSCalendar `recurrenceRule`. EAS 16.x series edits
    /// that change the rule or start/end clear existing `recurrenceOverrides`
    /// in the same state-guarded update; per-occurrence edits use
    /// `save_calendar_event_instance`. `start`/`duration`
    /// deliberately omit `timeZone` and treat the EAS UTC DateTime as a
    /// naive local-looking string -- the SAME convention this codebase's
    /// own read path (`local_to_utc_eas`) already uses for an absent
    /// timeZone (treated as already-UTC, not "floating" per a strict
    /// reading of JSCalendar/RFC 8984), so this isn't a new assumption,
    /// just the write-side mirror of an existing one. `CalendarEvent/set`
    /// genuinely supports in-place `update` (confirmed live the same way
    /// as `save_contact`: created a throwaway event, updated `title` on
    /// the SAME id, read it back changed, destroyed it) -- so, like
    /// Contacts and unlike Notes, the JMAP id itself is a stable
    /// ActiveSync ServerId across edits.
    pub async fn save_calendar_event(
        &self,
        auth: &AuthenticatedSession,
        calendar_id: &str,
        id: Option<&str>,
        uid: Option<&str>,
        fields: &crate::wbxml::eas::CalendarFields,
        clear_recurrence_exceptions: bool,
    ) -> anyhow::Result<String> {
        let Some(account_id) = auth.session.primary_account_for(capabilities::CALENDARS) else {
            anyhow::bail!("JMAP Calendars capability is not available");
        };

        let mut event = serde_json::Map::new();
        event.insert(
            "@type".to_owned(),
            serde_json::Value::String("Event".to_owned()),
        );
        let mut update_state = None;
        let mut recurrence_time_zone = None;
        if let Some(id) = id.filter(|_| {
            fields.reminder.is_some()
                || fields.location.is_some()
                || fields.start_time.is_some()
                || fields.end_time.is_some()
                || fields.attendees_present
                || fields.response_requested.is_some()
                || fields.disallow_new_time_proposal.is_some()
                || fields.recurrence_present
        }) {
            let response: JmapResponse<serde_json::Value> = self
                .api_call(
                    auth,
                    &[
                        capabilities::CORE.to_owned(),
                        capabilities::CALENDARS.to_owned(),
                    ],
                    vec![MethodCall::new(
                        "CalendarEvent/get",
                        serde_json::json!({"accountId":account_id,"ids":[id],"properties":["id","timeZone","iCalendar"]}),
                        "state",
                    )],
                )
                .await?;
            let result = response
                .method_responses
                .iter()
                .find(|method| method.0 == "CalendarEvent/get")
                .context("calendar state lookup failed")?;
            anyhow::ensure!(
                result.1["list"]
                    .as_array()
                    .is_some_and(|items| items.iter().any(|item| item["id"] == id)),
                "calendar event not found"
            );
            recurrence_time_zone = result.1["list"]
                .as_array()
                .and_then(|items| items.iter().find(|item| item["id"] == id))
                .and_then(|item| item["timeZone"].as_str())
                .map(str::to_owned);
            if let Some(disallow) = fields.disallow_new_time_proposal {
                let item = result.1["list"]
                    .as_array()
                    .and_then(|items| items.iter().find(|item| item["id"] == id))
                    .context("calendar event disappeared before custom-property update")?;
                event.insert(
                    "iCalendar".to_owned(),
                    calendar_icalendar_with_disallow_new_time_proposal(
                        item.get("iCalendar"),
                        disallow,
                    )?,
                );
            }
            update_state = Some(
                result.1["state"]
                    .as_str()
                    .context("calendar state missing")?
                    .to_owned(),
            );
        }
        if fields.recurrence_present {
            event.insert(
                "recurrenceRule".to_owned(),
                match &fields.recurrence {
                    Some(recurrence) => crate::jmap::calendar_recurrence::to_jscalendar(
                        recurrence,
                        recurrence_time_zone.as_deref(),
                    )?,
                    None => serde_json::Value::Null,
                },
            );
        }
        if id.is_none() {
            if let Some(disallow) = fields.disallow_new_time_proposal {
                event.insert(
                    "iCalendar".to_owned(),
                    calendar_icalendar_with_disallow_new_time_proposal(None, disallow)?,
                );
            }
        }
        if clear_recurrence_exceptions
            && (fields.recurrence_present
                || fields.start_time.is_some()
                || fields.end_time.is_some())
        {
            event.insert("recurrenceOverrides".to_owned(), serde_json::Value::Null);
        }
        let mut reminder_changed = false;
        if let (Some(reminder), None) = (fields.reminder, id) {
            crate::jmap::calendar_reminder::write_reminder(
                &mut event,
                &serde_json::Value::Null,
                reminder,
            )?;
            reminder_changed = true;
        }
        if let (Some(reminder), Some(id)) = (fields.reminder, id) {
            let response: JmapResponse<serde_json::Value> = self.api_call(auth,
                &[capabilities::CORE.to_owned(), capabilities::CALENDARS.to_owned()],
                vec![MethodCall::new("CalendarEvent/get", serde_json::json!({
                    "accountId":account_id, "ids":[id], "properties":["id","alerts","useDefaultAlerts","showWithoutTime"]
                }), "reminder")]).await?;
            let result = response
                .method_responses
                .iter()
                .find(|method| method.0 == "CalendarEvent/get")
                .context("calendar reminder lookup failed")?;
            anyhow::ensure!(
                result.1["state"].as_str() == update_state.as_deref(),
                "calendar changed during update"
            );
            let item = result.1["list"]
                .as_array()
                .and_then(|list| list.iter().find(|item| item["id"] == id))
                .context("calendar event not found")?;
            let alerts = item.get("alerts").context("calendar alerts missing")?;
            let mut effective = alerts;
            let mut defaults_value = None;
            let use_defaults = item["useDefaultAlerts"]
                .as_bool()
                .context("calendar default-alert setting missing")?;
            if use_defaults {
                let defaults: JmapResponse<serde_json::Value> = self.api_call(auth,
                    &[capabilities::CORE.to_owned(), capabilities::CALENDARS.to_owned()],
                    vec![MethodCall::new("Calendar/get", serde_json::json!({
                        "accountId":account_id, "ids":[calendar_id],
                        "properties":["id","defaultAlertsWithTime","defaultAlertsWithoutTime"]
                    }), "defaults")]).await?;
                let result = defaults
                    .method_responses
                    .iter()
                    .find(|method| method.0 == "Calendar/get")
                    .context("calendar default alerts lookup failed")?;
                let calendar = result.1["list"]
                    .as_array()
                    .and_then(|list| list.iter().find(|calendar| calendar["id"] == calendar_id))
                    .context("calendar default alerts unavailable")?;
                let property = if item["showWithoutTime"] == true {
                    "defaultAlertsWithoutTime"
                } else {
                    "defaultAlertsWithTime"
                };
                let defaults = calendar
                    .get(property)
                    .context("calendar default alerts missing")?;
                if !defaults.is_null() && !defaults.is_object() {
                    anyhow::bail!("invalid calendar default alerts");
                }
                if calendar[property]
                    .as_object()
                    .is_some_and(|values| !values.is_empty())
                {
                    defaults_value = Some(calendar[property].clone());
                }
            }
            if let Some(defaults) = &defaults_value {
                effective = defaults;
            }
            if !crate::jmap::calendar_reminder::unchanged(effective, reminder)? {
                if defaults_value.as_ref().is_some_and(|defaults| {
                    !crate::jmap::calendar_reminder::defaults_match(alerts, defaults)
                }) {
                    anyhow::bail!("calendar has dormant alerts that cannot be changed safely");
                }
                crate::jmap::calendar_reminder::write_reminder(&mut event, alerts, reminder)?;
                reminder_changed = true;
            }
        }
        if let Some(sensitivity) = fields.sensitivity {
            let privacy = match sensitivity {
                0 => "public",
                2 => "private",
                _ => anyhow::bail!("unsupported calendar sensitivity"),
            };
            event.insert("privacy".to_owned(), serde_json::json!(privacy));
        }
        if let Some(categories) = &fields.categories {
            let keywords: BTreeMap<_, _> = categories.iter().map(|name| (name, true)).collect();
            event.insert(
                "keywords".to_owned(),
                if keywords.is_empty() {
                    serde_json::Value::Null
                } else {
                    serde_json::json!(keywords)
                },
            );
        }
        if let Some(busy) = fields.busy_status {
            let value = match busy {
                0 => "free",
                1 => "x-eas-tentative",
                2 => "busy",
                3 => "x-eas-out-of-office",
                _ => anyhow::bail!("unsupported calendar availability"),
            };
            event.insert("freeBusyStatus".to_owned(), serde_json::json!(value));
        }
        if let Some(subject) = &fields.subject {
            event.insert(
                "title".to_owned(),
                serde_json::Value::String(subject.clone()),
            );
        }
        if let Some(body) = &fields.body {
            event.insert("description".to_owned(), serde_json::json!(body));
            event.insert(
                "descriptionContentType".to_owned(),
                serde_json::json!(if fields.body_type == Some(2) {
                    "text/html"
                } else {
                    "text/plain"
                }),
            );
        }
        if let Some(location) = &fields.location {
            if location.is_empty() && fields.location_coordinates.is_none() {
                event.insert("locations".to_owned(), serde_json::Value::Null);
            } else if let Some(id) = id {
                let response: JmapResponse<serde_json::Value> = self.api_call(
                    auth,
                    &[capabilities::CORE.to_owned(), capabilities::CALENDARS.to_owned()],
                    vec![MethodCall::new("CalendarEvent/get", serde_json::json!({
                        "accountId": account_id, "ids": [id], "properties": ["id", "locations"]
                    }), "0")],
                ).await?;
                let object = response
                    .method_responses
                    .iter()
                    .find(|method| method.0 == "CalendarEvent/get")
                    .and_then(|method| method.1["list"].as_array())
                    .and_then(|list| list.iter().find(|object| object["id"] == id))
                    .ok_or_else(|| {
                        anyhow::anyhow!("CalendarEvent/get omitted the event being edited")
                    })?;
                let locations = object["locations"].as_object();
                let selected = locations.and_then(|locations| {
                    locations
                        .iter()
                        .find(|(_, value)| value["name"].is_string())
                        .or_else(|| {
                            locations
                                .iter()
                                .find(|(_, value)| value["coordinates"].is_string())
                        })
                        .or_else(|| locations.iter().next())
                });
                if let Some((key, previous)) = selected {
                    if fields.location_coordinates.is_some()
                        && previous["coordinates"]
                            .as_str()
                            .is_some_and(|value| simple_geo(value).is_none())
                    {
                        anyhow::bail!("cannot replace extended location coordinates");
                    }
                    let key = key.replace('~', "~0").replace('/', "~1");
                    if !location.is_empty() {
                        event.insert(format!("locations/{key}/name"), serde_json::json!(location));
                    }
                    if let Some((lat, lon)) = &fields.location_coordinates {
                        event.insert(
                            format!("locations/{key}/coordinates"),
                            serde_json::json!(format!("geo:{lat},{lon}")),
                        );
                    }
                } else {
                    let mut value = serde_json::json!({"@type": "Location"});
                    if !location.is_empty() {
                        value["name"] = serde_json::json!(location);
                    }
                    if let Some((lat, lon)) = &fields.location_coordinates {
                        value["coordinates"] = serde_json::json!(format!("geo:{lat},{lon}"));
                    }
                    event.insert("locations".to_owned(), serde_json::json!({"loc1":value}));
                }
            } else {
                let mut value = serde_json::json!({"@type": "Location"});
                if !location.is_empty() {
                    value["name"] = serde_json::json!(location);
                }
                if let Some((lat, lon)) = &fields.location_coordinates {
                    value["coordinates"] = serde_json::json!(format!("geo:{lat},{lon}"));
                }
                event.insert("locations".to_owned(), serde_json::json!({"loc1":value}));
            }
        }
        if id.is_none() && fields.start_time.is_some() && fields.end_time.is_none() {
            let start = fields
                .start_time
                .as_deref()
                .and_then(eas_compact_to_local_iso)
                .ok_or_else(|| anyhow::anyhow!("invalid calendar start"))?;
            event.insert("start".to_owned(), serde_json::json!(start));
        } else if fields.start_time.is_some() || fields.end_time.is_some() {
            let existing = if let Some(id) = id {
                let response: JmapResponse<serde_json::Value> = self.api_call(
                    auth,
                    &[capabilities::CORE.to_owned(), capabilities::CALENDARS.to_owned()],
                    vec![MethodCall::new("CalendarEvent/get", serde_json::json!({
                        "accountId": account_id, "ids": [id], "properties": ["id", "start", "timeZone", "duration"]
                    }), "0")],
                ).await?;
                let object = response
                    .method_responses
                    .iter()
                    .find(|method| method.0 == "CalendarEvent/get")
                    .and_then(|method| method.1["list"].as_array())
                    .and_then(|list| list.iter().find(|object| object["id"] == id))
                    .ok_or_else(|| {
                        anyhow::anyhow!("CalendarEvent/get omitted the event being edited")
                    })?;
                Some(serde_json::from_value::<CalendarEventObject>(
                    object.clone(),
                )?)
            } else {
                None
            };
            let timezone = existing
                .as_ref()
                .and_then(|event| event.time_zone.as_deref());
            let previous = existing.as_ref().map(|event| {
                let mut value = CalendarEventObject::default();
                value.start.clone_from(&event.start);
                value.time_zone.clone_from(&event.time_zone);
                value.duration.clone_from(&event.duration);
                CalendarEvent::from(value)
            });
            let start = fields
                .start_time
                .as_deref()
                .or_else(|| {
                    previous
                        .as_ref()
                        .and_then(|event| event.start_utc.as_deref())
                })
                .ok_or_else(|| anyhow::anyhow!("calendar event has no start"))?;
            let end = fields
                .end_time
                .as_deref()
                .or_else(|| previous.as_ref().and_then(|event| event.end_utc.as_deref()))
                .ok_or_else(|| anyhow::anyhow!("calendar event has no end"))?;
            let start_local = eas_to_calendar_local(start, timezone).ok_or_else(|| {
                anyhow::anyhow!("calendar start cannot be represented in its time zone")
            })?;
            eas_to_calendar_local(end, timezone).ok_or_else(|| {
                anyhow::anyhow!("calendar end cannot be represented in its time zone")
            })?;
            let start_utc = chrono::NaiveDateTime::parse_from_str(start, "%Y%m%dT%H%M%SZ")?;
            let end_utc = chrono::NaiveDateTime::parse_from_str(end, "%Y%m%dT%H%M%SZ")?;
            let duration = end_utc.signed_duration_since(start_utc).num_seconds();
            if duration < 0 {
                anyhow::bail!("calendar end precedes start");
            }
            if fields.start_time.is_some() {
                event.insert(
                    "start".to_owned(),
                    serde_json::json!(start_local.format("%Y-%m-%dT%H:%M:%S").to_string()),
                );
            }
            event.insert(
                "duration".to_owned(),
                serde_json::json!(format!("PT{duration}S")),
            );
        }
        if let Some(all_day) = fields.all_day_event {
            event.insert(
                "showWithoutTime".to_owned(),
                serde_json::Value::Bool(all_day),
            );
        }
        if fields.attendees_present && fields.attendees.is_empty() {
            if let Some(id) = id {
                let response: JmapResponse<serde_json::Value> = self.api_call(
                    auth,
                    &[capabilities::CORE.to_owned(), capabilities::CALENDARS.to_owned()],
                    vec![MethodCall::new("CalendarEvent/get", serde_json::json!({
                        "accountId": account_id, "ids": [id], "properties": ["id", "participants"]
                    }), "0")],
                ).await?;
                let object = response
                    .method_responses
                    .iter()
                    .find(|method| method.0 == "CalendarEvent/get")
                    .and_then(|method| method.1["list"].as_array())
                    .and_then(|list| list.iter().find(|object| object["id"] == id))
                    .ok_or_else(|| {
                        anyhow::anyhow!("CalendarEvent/get omitted the event being edited")
                    })?;
                if let Some(participants) = object["participants"].as_object() {
                    for (key, participant) in participants {
                        if participant["roles"]["owner"] != true {
                            let key = key.replace('~', "~0").replace('/', "~1");
                            event.insert(format!("participants/{key}"), serde_json::Value::Null);
                        }
                    }
                }
            }
        }
        if let Some(id) = id.filter(|_| !fields.attendees.is_empty()) {
            let response: JmapResponse<serde_json::Value> = self.api_call(
                auth,
                &[capabilities::CORE.to_owned(), capabilities::CALENDARS.to_owned()],
                vec![MethodCall::new("CalendarEvent/get", serde_json::json!({
                    "accountId": account_id, "ids": [id], "properties": ["id", "participants", "replyTo"]
                }), "0")],
            ).await?;
            let object = response
                .method_responses
                .iter()
                .find(|method| method.0 == "CalendarEvent/get")
                .and_then(|method| method.1["list"].as_array())
                .and_then(|list| list.iter().find(|object| object["id"] == id))
                .ok_or_else(|| {
                    anyhow::anyhow!("CalendarEvent/get omitted the event being edited")
                })?;
            let mut remaining: BTreeMap<String, &crate::wbxml::eas::AttendeeField> =
                BTreeMap::new();
            for attendee in &fields.attendees {
                if remaining
                    .insert(attendee.email.to_lowercase(), attendee)
                    .is_some()
                {
                    anyhow::bail!("duplicate calendar attendee");
                }
            }
            let participants = object["participants"]
                .as_object()
                .cloned()
                .unwrap_or_default();
            anyhow::ensure!(
                participants
                    .values()
                    .filter(|participant| participant["roles"]["owner"] == true)
                    .count()
                    <= 1,
                "multiple calendar owners cannot be modified safely"
            );
            let mut owner_email = None;
            let mut changes = serde_json::Map::new();
            let mut existing_emails = std::collections::HashSet::new();
            for (key, participant) in &participants {
                if participant["roles"]["owner"] == true {
                    owner_email = participant["email"].as_str().map(str::to_owned);
                    continue;
                }
                let email = participant["email"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("calendar participant has no email"))?;
                if !existing_emails.insert(email.to_lowercase()) {
                    anyhow::bail!("ambiguous calendar participants");
                }
                let patch_key =
                    format!("participants/{}", key.replace('~', "~0").replace('/', "~1"));
                if let Some(attendee) = remaining.remove(&email.to_lowercase()) {
                    if let Some(response_requested) = fields.response_requested {
                        changes.insert(
                            format!("{patch_key}/expectReply"),
                            serde_json::Value::Bool(response_requested),
                        );
                    }
                    if let Some(name) = &attendee.name {
                        changes.insert(format!("{patch_key}/name"), serde_json::json!(name));
                    }
                    if participant["roles"].is_object() {
                        changes.insert(
                            format!("{patch_key}/roles/optional"),
                            if attendee.optional {
                                serde_json::Value::Bool(true)
                            } else {
                                serde_json::Value::Null
                            },
                        );
                    } else if attendee.optional {
                        changes.insert(
                            format!("{patch_key}/roles"),
                            serde_json::json!({"attendee":true, "optional":true}),
                        );
                    }
                } else {
                    changes.insert(patch_key, serde_json::Value::Null);
                }
            }
            let owner_email = owner_email.unwrap_or_else(|| auth.authorization.username.to_owned());
            if !participants.values().any(|p| p["roles"]["owner"] == true) {
                let key = format!("owner-{}", uuid::Uuid::new_v4().simple());
                changes.insert(format!("participants/{key}"), serde_json::json!({
                    "@type":"Participant", "email":owner_email,
                    "calendarAddress":format!("mailto:{owner_email}"), "roles":{"owner":true},
                    "participationStatus":"accepted", "sendTo":{"imip":format!("mailto:{owner_email}")}
                }));
            }
            for attendee in remaining.values() {
                let key = format!("att-{}", uuid::Uuid::new_v4().simple());
                let mut roles = serde_json::json!({"attendee":true});
                if attendee.optional {
                    roles["optional"] = serde_json::Value::Bool(true);
                }
                let participation_status = match attendee.status {
                    None => "needs-action",
                    Some(2) => "tentative",
                    Some(3) => "accepted",
                    Some(4) => "declined",
                    Some(5) => "needs-action",
                    Some(_) => anyhow::bail!("unsupported attendee status"),
                };
                let mut participant = serde_json::json!({
                    "@type":"Participant", "email":attendee.email,
                    "calendarAddress":format!("mailto:{}", attendee.email),
                    "roles":roles, "participationStatus":participation_status,
                    "expectReply":fields.response_requested.unwrap_or(true),
                    "sendTo":{"imip":format!("mailto:{}", attendee.email)}
                });
                if let Some(name) = &attendee.name {
                    participant["name"] = serde_json::json!(name);
                }
                changes.insert(format!("participants/{key}"), participant);
            }
            if object["replyTo"].is_null() {
                event.insert(
                    "replyTo".to_owned(),
                    serde_json::json!({"imip":format!("mailto:{owner_email}")}),
                );
            }
            if object["participants"].is_object() {
                event.extend(changes);
            } else {
                let mut new_participants = serde_json::Map::new();
                for (key, value) in changes {
                    new_participants
                        .insert(key.strip_prefix("participants/").unwrap().to_owned(), value);
                }
                event.insert(
                    "participants".to_owned(),
                    serde_json::Value::Object(new_participants),
                );
            }
        }
        if !fields.attendees_present {
            if let (Some(id), Some(response_requested)) = (id, fields.response_requested) {
                let response: JmapResponse<serde_json::Value> = self
                .api_call(
                    auth,
                    &[capabilities::CORE.to_owned(), capabilities::CALENDARS.to_owned()],
                    vec![MethodCall::new(
                        "CalendarEvent/get",
                        serde_json::json!({"accountId":account_id,"ids":[id],"properties":["id","participants"]}),
                        "response-requested",
                    )],
                )
                .await?;
                let object = response
                    .method_responses
                    .iter()
                    .find(|method| method.0 == "CalendarEvent/get")
                    .and_then(|method| method.1["list"].as_array())
                    .and_then(|list| list.iter().find(|object| object["id"] == id))
                    .ok_or_else(|| {
                        anyhow::anyhow!("CalendarEvent/get omitted the event being edited")
                    })?;
                if let Some(participants) = object["participants"].as_object() {
                    for (key, participant) in participants {
                        if participant["roles"]["attendee"] == true
                            && participant["roles"]["owner"] != true
                        {
                            let key = key.replace('~', "~0").replace('/', "~1");
                            event.insert(
                                format!("participants/{key}/expectReply"),
                                serde_json::Value::Bool(response_requested),
                            );
                        }
                    }
                }
            }
        }
        if !fields.attendees.is_empty() && id.is_none() {
            // Confirmed live 2026-08-25: Stalwart silently drops
            // `participants` entirely unless a top-level `replyTo` is
            // ALSO present (JSCalendar RFC 8984 §4.4.1 -- a companion
            // property to `participants`+`expectReply`, not a per-
            // participant field) -- this is the fix for the earlier
            // "blocked at the JMAP layer" verdict, it was never a
            // Stalwart capability gap, just an incomplete write payload.
            // A SECOND, separately-confirmed requirement (live-tested the
            // same way, isolating one field at a time): every participant
            // ALSO needs its own `calendarAddress`, not just `email` +
            // `sendTo.imip` -- omitting it reproduces the exact same
            // silent whole-map drop as a missing `replyTo` did. Neither
            // omission errors; both just vanish on read-back, which is
            // why this needed isolating field-by-field rather than
            // trusting the first fix that seemed to work.
            let organizer_email = auth.authorization.username.to_owned();
            let mut participants = serde_json::Map::new();
            participants.insert(
                "org".to_owned(),
                serde_json::json!({
                    "@type": "Participant",
                    "email": organizer_email,
                    "calendarAddress": format!("mailto:{organizer_email}"),
                    "roles": {"owner": true},
                    "participationStatus": "accepted",
                    "sendTo": {"imip": format!("mailto:{organizer_email}")}
                }),
            );
            for (i, attendee) in fields.attendees.iter().enumerate() {
                let mut roles = serde_json::Map::new();
                roles.insert("attendee".to_owned(), serde_json::Value::Bool(true));
                if attendee.optional {
                    roles.insert("optional".to_owned(), serde_json::Value::Bool(true));
                }
                let participation_status = match attendee.status {
                    None => "needs-action",
                    Some(2) => "tentative",
                    Some(3) => "accepted",
                    Some(4) => "declined",
                    Some(5) => "needs-action",
                    Some(_) => anyhow::bail!("unsupported attendee status"),
                };
                let mut participant = serde_json::json!({
                    "@type": "Participant",
                    "email": attendee.email,
                    "calendarAddress": format!("mailto:{}", attendee.email),
                    "roles": roles,
                    "participationStatus": participation_status,
                    "expectReply": fields.response_requested.unwrap_or(true),
                    "sendTo": {"imip": format!("mailto:{}", attendee.email)}
                });
                if let Some(name) = &attendee.name {
                    participant["name"] = serde_json::Value::String(name.clone());
                }
                participants.insert(format!("att{i}"), participant);
            }
            event.insert(
                "replyTo".to_owned(),
                serde_json::json!({ "imip": format!("mailto:{organizer_email}") }),
            );
            event.insert(
                "participants".to_owned(),
                serde_json::Value::Object(participants),
            );
        }

        let call = match id {
            Some(existing_id) => {
                let mut arguments = serde_json::json!({
                    "accountId":account_id, "update":{existing_id:event}
                });
                if let Some(state) = update_state {
                    arguments["ifInState"] = serde_json::json!(state);
                }
                MethodCall::new("CalendarEvent/set", arguments, "0")
            }
            None => {
                if let Some(uid) = uid {
                    event.insert("uid".to_owned(), serde_json::json!(uid));
                }
                event.insert(
                    "calendarIds".to_owned(),
                    serde_json::json!({ calendar_id: true }),
                );
                MethodCall::new(
                    "CalendarEvent/set",
                    serde_json::json!({
                        "accountId": account_id,
                        "create": { "e1": event }
                    }),
                    "0",
                )
            }
        };

        let response: JmapResponse<serde_json::Value> = self
            .api_call(
                auth,
                &[
                    capabilities::CORE.to_owned(),
                    capabilities::CALENDARS.to_owned(),
                ],
                vec![call],
            )
            .await?;

        for method in response.method_responses {
            if method.0 == "error" {
                anyhow::bail!("JMAP method error in CalendarEvent/set");
            }
            if method.0 == "CalendarEvent/set" {
                if let Some(existing_id) = id {
                    let updated = method
                        .1
                        .get("updated")
                        .and_then(|value| value.get(existing_id));
                    if updated.is_some() {
                        if let Some(disallow) = fields.disallow_new_time_proposal {
                            self.verify_calendar_disallow_new_time_proposal(
                                auth,
                                existing_id,
                                disallow,
                            )
                            .await?;
                        }
                        if let Some(reminder) = fields.reminder.filter(|_| reminder_changed) {
                            let verify: JmapResponse<serde_json::Value> = self
                                .api_call(
                                    auth,
                                    &[
                                        capabilities::CORE.to_owned(),
                                        capabilities::CALENDARS.to_owned(),
                                    ],
                                    vec![MethodCall::new(
                                        "CalendarEvent/get",
                                        serde_json::json!({
                                            "accountId":account_id, "ids":[existing_id],
                                            "properties":["id","alerts","useDefaultAlerts"]
                                        }),
                                        "verify",
                                    )],
                                )
                                .await?;
                            let result = verify
                                .method_responses
                                .iter()
                                .find(|method| method.0 == "CalendarEvent/get")
                                .context("calendar reminder verification failed")?;
                            let item = result.1["list"]
                                .as_array()
                                .and_then(|items| {
                                    items.iter().find(|item| item["id"] == existing_id)
                                })
                                .context("updated calendar event disappeared")?;
                            if item["useDefaultAlerts"] != false
                                || crate::jmap::calendar_reminder::read_reminder(&item["alerts"])
                                    != reminder
                            {
                                anyhow::bail!(
                                    "JMAP did not retain the requested calendar reminder"
                                );
                            }
                        }
                        if let Some(coordinates) = &fields.location_coordinates {
                            self.verify_calendar_coordinates(auth, existing_id, coordinates)
                                .await?;
                        }
                        if let Some(sensitivity) = fields.sensitivity {
                            self.verify_calendar_privacy(auth, existing_id, sensitivity)
                                .await?;
                        }
                        if let Some(categories) = &fields.categories {
                            self.verify_calendar_categories(auth, existing_id, categories)
                                .await?;
                        }
                        return Ok(existing_id.to_owned());
                    }
                    anyhow::bail!(
                        "CalendarEvent/set did not confirm update for {existing_id}: {:?}",
                        method.1.get("notUpdated")
                    );
                }
                if let Some(new_id) = method
                    .1
                    .get("created")
                    .and_then(|c| c.get("e1"))
                    .and_then(|event| event.get("id"))
                    .and_then(|value| value.as_str())
                {
                    if let Some(disallow) = fields.disallow_new_time_proposal {
                        self.verify_calendar_disallow_new_time_proposal(auth, new_id, disallow)
                            .await?;
                    }
                    if let Some(reminder) = fields.reminder {
                        self.verify_calendar_reminder(auth, new_id, reminder)
                            .await?;
                    }
                    if let Some(coordinates) = &fields.location_coordinates {
                        self.verify_calendar_coordinates(auth, new_id, coordinates)
                            .await?;
                    }
                    if let Some(sensitivity) = fields.sensitivity {
                        self.verify_calendar_privacy(auth, new_id, sensitivity)
                            .await?;
                    }
                    if let Some(categories) = &fields.categories {
                        self.verify_calendar_categories(auth, new_id, categories)
                            .await?;
                    }
                    return Ok(new_id.to_owned());
                }
                anyhow::bail!(
                    "CalendarEvent/set did not return a created id: {:?}",
                    method.1.get("notCreated")
                );
            }
        }
        anyhow::bail!("CalendarEvent/set response was missing")
    }

    /// Idempotent, matching `destroy_note`/`destroy_contact`'s own
    /// convention.
    pub async fn destroy_calendar_event(
        &self,
        auth: &AuthenticatedSession,
        id: &str,
    ) -> anyhow::Result<()> {
        let Some(account_id) = auth.session.primary_account_for(capabilities::CALENDARS) else {
            anyhow::bail!("JMAP Calendars capability is not available");
        };
        let response: JmapResponse<serde_json::Value> = self
            .api_call(
                auth,
                &[
                    capabilities::CORE.to_owned(),
                    capabilities::CALENDARS.to_owned(),
                ],
                vec![MethodCall::new(
                    "CalendarEvent/set",
                    serde_json::json!({ "accountId": account_id, "destroy": [id] }),
                    "0",
                )],
            )
            .await?;

        for method in response.method_responses {
            if method.0 == "CalendarEvent/set" {
                let destroyed = method
                    .1
                    .get("destroyed")
                    .and_then(|value| value.as_array())
                    .is_some_and(|list| list.iter().any(|v| v.as_str() == Some(id)));
                if destroyed {
                    return Ok(());
                }
                let not_found = method
                    .1
                    .get("notDestroyed")
                    .and_then(|value| value.get(id))
                    .and_then(|entry| entry.get("type"))
                    .and_then(|t| t.as_str())
                    == Some("notFound");
                if not_found {
                    return Ok(());
                }
                anyhow::bail!("CalendarEvent/set destroy did not report success for {id}");
            }
        }
        anyhow::bail!("CalendarEvent/set response was missing")
    }

    /// Excludes one recurrence occurrence using the EAS 16.x `InstanceId`.
    /// The identifier names the original occurrence start, so it is converted
    /// to the parent event's local JSCalendar recurrence key before patching.
    pub async fn exclude_calendar_event_instance(
        &self,
        auth: &AuthenticatedSession,
        id: &str,
        instance_id: &str,
    ) -> anyhow::Result<()> {
        let account_id = auth
            .session
            .primary_account_for(capabilities::CALENDARS)
            .context("JMAP Calendars capability is not available")?;
        let response: JmapResponse<serde_json::Value> = self
            .api_call(
                auth,
                &[capabilities::CORE.to_owned(), capabilities::CALENDARS.to_owned()],
                vec![MethodCall::new(
                    "CalendarEvent/get",
                    serde_json::json!({"accountId":account_id,"ids":[id],"properties":["id","timeZone","recurrenceRule","recurrenceOverrides"]}),
                    "instance",
                )],
            )
            .await?;
        let result = response
            .method_responses
            .iter()
            .find(|method| method.0 == "CalendarEvent/get")
            .context("calendar event lookup failed")?;
        let event = result.1["list"]
            .as_array()
            .and_then(|items| items.iter().find(|item| item["id"] == id))
            .context("calendar event not found")?;
        anyhow::ensure!(
            !event["recurrenceRule"].is_null(),
            "InstanceId requires a recurring calendar event"
        );
        let timezone = event["timeZone"].as_str();
        let key = recurrence_override_key(instance_id, timezone)?;
        let state = result.1["state"]
            .as_str()
            .context("calendar state missing")?;
        let update = serde_json::json!({
            "accountId": account_id,
            "ifInState": state,
            "update": { id: { format!("recurrenceOverrides/{key}"): {"excluded": true} } }
        });
        let response: JmapResponse<serde_json::Value> = self
            .api_call(
                auth,
                &[
                    capabilities::CORE.to_owned(),
                    capabilities::CALENDARS.to_owned(),
                ],
                vec![MethodCall::new("CalendarEvent/set", update, "exclude")],
            )
            .await?;
        let result = response
            .method_responses
            .iter()
            .find(|method| method.0 == "CalendarEvent/set")
            .context("calendar exception update failed")?;
        if let Some(error) = result.1["notUpdated"].get(id) {
            anyhow::bail!("calendar exception update rejected: {error}");
        }
        anyhow::ensure!(
            result.1["updated"].get(id).is_some(),
            "calendar exception update was not confirmed"
        );
        Ok(())
    }

    /// Updates EAS 16.x occurrence fields with a lossless JSCalendar
    /// override representation.
    pub async fn save_calendar_event_instance(
        &self,
        auth: &AuthenticatedSession,
        id: &str,
        instance_id: &str,
        fields: &crate::wbxml::eas::CalendarFields,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
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
                && fields
                    .body_type
                    .is_none_or(|body_type| matches!(body_type, 1 | 2)),
            "unsupported per-instance calendar fields"
        );
        let account_id = auth
            .session
            .primary_account_for(capabilities::CALENDARS)
            .context("JMAP Calendars capability is not available")?;
        let response: JmapResponse<serde_json::Value> = self
            .api_call(
                auth,
                &[capabilities::CORE.to_owned(), capabilities::CALENDARS.to_owned()],
                vec![MethodCall::new(
                    "CalendarEvent/get",
                    serde_json::json!({"accountId":account_id,"ids":[id],"properties":["id","timeZone","recurrenceRule","recurrenceOverrides","alerts","useDefaultAlerts","participants"]}),
                    "instance",
                )],
            )
            .await?;
        let result = response
            .method_responses
            .iter()
            .find(|method| method.0 == "CalendarEvent/get")
            .context("calendar event lookup failed")?;
        let event = result.1["list"]
            .as_array()
            .and_then(|items| items.iter().find(|item| item["id"] == id))
            .context("calendar event not found")?;
        anyhow::ensure!(
            !event["recurrenceRule"].is_null(),
            "InstanceId requires a recurring calendar event"
        );
        let key = recurrence_override_key(instance_id, event["timeZone"].as_str())?;
        anyhow::ensure!(
            event["recurrenceOverrides"][&key]["excluded"] != true,
            "cannot change a deleted recurrence occurrence"
        );
        let state = result.1["state"]
            .as_str()
            .context("calendar state missing")?;
        let mut patch = serde_json::Map::new();
        if let Some(reminder) = fields.reminder {
            anyhow::ensure!(
                event["useDefaultAlerts"] != true,
                "cannot change an occurrence reminder while calendar default alerts are active"
            );
            let base_alerts = event.get("alerts").unwrap_or(&serde_json::Value::Null);
            let exception = &event["recurrenceOverrides"][&key];
            let empty_patch = serde_json::Map::new();
            let occurrence_patch = if exception.is_null() {
                &empty_patch
            } else {
                exception
                    .as_object()
                    .context("invalid recurrence exception patch")?
            };
            let effective_alerts = merge_calendar_alert_patch(Some(base_alerts), occurrence_patch)?;
            let mut alert_patch = serde_json::Map::new();
            crate::jmap::calendar_reminder::write_reminder(
                &mut alert_patch,
                &effective_alerts,
                reminder,
            )?;
            alert_patch.remove("useDefaultAlerts");
            for (path, value) in alert_patch {
                patch.insert(format!("recurrenceOverrides/{key}/{path}"), value);
            }
        }
        if fields.attendees_present {
            let participants = event["participants"]
                .as_object()
                .cloned()
                .unwrap_or_default();
            let owner = participants
                .iter()
                .find(|(_, value)| value["roles"]["owner"] == true)
                .map(|(id, value)| (id.clone(), value.clone()));
            anyhow::ensure!(
                fields.attendees.is_empty() || owner.is_some(),
                "cannot add occurrence attendees without an existing organizer"
            );
            let mut replacement = serde_json::Map::new();
            if let Some((id, value)) = owner {
                replacement.insert(id, value);
            }
            let mut seen = BTreeSet::new();
            for (index, attendee) in fields.attendees.iter().enumerate() {
                anyhow::ensure!(
                    !attendee.email.is_empty() && seen.insert(attendee.email.to_ascii_lowercase()),
                    "invalid or duplicate occurrence attendee"
                );
                anyhow::ensure!(
                    !participants.values().any(|participant| {
                        participant["roles"]["owner"] == true
                            && participant["email"]
                                .as_str()
                                .is_some_and(|email| email.eq_ignore_ascii_case(&attendee.email))
                    }),
                    "the organizer cannot be listed as an occurrence attendee"
                );
                let existing = participants
                    .iter()
                    .find(|(_, participant)| {
                        participant["roles"]["owner"] != true
                            && participant["email"]
                                .as_str()
                                .is_some_and(|email| email.eq_ignore_ascii_case(&attendee.email))
                    })
                    .map(|(id, participant)| (id.clone(), participant.clone()));
                let (participant_id, mut participant) = existing.unwrap_or_else(|| {
                    (
                        format!("eas-occurrence-{index}"),
                        serde_json::json!({"@type":"Participant"}),
                    )
                });
                participant["@type"] = serde_json::json!("Participant");
                participant["email"] = serde_json::json!(attendee.email);
                participant
                    .as_object_mut()
                    .unwrap()
                    .entry("calendarAddress".to_owned())
                    .or_insert_with(|| serde_json::json!(format!("mailto:{}", attendee.email)));
                participant["roles"] = participant
                    .get("roles")
                    .filter(|roles| roles.is_object())
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({}));
                participant["roles"]["attendee"] = serde_json::json!(true);
                if attendee.optional {
                    participant["roles"]["optional"] = serde_json::json!(true);
                } else if let Some(roles) = participant["roles"].as_object_mut() {
                    roles.remove("optional");
                }
                if let Some(status) = attendee.status {
                    participant["participationStatus"] = serde_json::json!(match status {
                        2 => "tentative",
                        3 => "accepted",
                        4 => "declined",
                        5 => "needs-action",
                        _ => anyhow::bail!("unsupported attendee status"),
                    });
                } else if participant.get("participationStatus").is_none() {
                    participant["participationStatus"] = serde_json::json!("needs-action");
                }
                if let Some(name) = &attendee.name {
                    participant["name"] = serde_json::json!(name);
                }
                participant
                    .as_object_mut()
                    .unwrap()
                    .entry("sendTo".to_owned())
                    .or_insert_with(
                        || serde_json::json!({"imip":format!("mailto:{}", attendee.email)}),
                    );
                participant
                    .as_object_mut()
                    .unwrap()
                    .entry("expectReply".to_owned())
                    .or_insert(serde_json::Value::Bool(true));
                replacement.insert(participant_id, participant);
            }
            patch.insert(
                format!("recurrenceOverrides/{key}/participants"),
                if replacement.is_empty() {
                    serde_json::Value::Null
                } else {
                    serde_json::Value::Object(replacement)
                },
            );
        }
        if let Some(subject) = &fields.subject {
            patch.insert(
                format!("recurrenceOverrides/{key}/title"),
                serde_json::json!(subject),
            );
        }
        if let Some(body) = &fields.body {
            patch.insert(
                format!("recurrenceOverrides/{key}/description"),
                serde_json::json!(body),
            );
            patch.insert(
                format!("recurrenceOverrides/{key}/descriptionContentType"),
                serde_json::json!(if fields.body_type == Some(2) {
                    "text/html"
                } else {
                    "text/plain"
                }),
            );
        }
        if let Some(start) = fields.start_time.as_deref() {
            let local = eas_to_calendar_local(start, event["timeZone"].as_str())
                .context("calendar instance start cannot be represented in its time zone")?;
            patch.insert(
                format!("recurrenceOverrides/{key}/start"),
                serde_json::json!(local.format("%Y-%m-%dT%H:%M:%S").to_string()),
            );
        }
        if let Some(end) = fields.end_time.as_deref() {
            let start_utc = if let Some(start) = fields.start_time.as_deref() {
                chrono::NaiveDateTime::parse_from_str(start, "%Y%m%dT%H%M%SZ")?
            } else {
                let current_start = event["recurrenceOverrides"][&key]["start"]
                    .as_str()
                    .unwrap_or(&key);
                let compact = local_to_utc_eas(current_start, event["timeZone"].as_str())
                    .context("calendar instance start cannot be represented in its time zone")?;
                chrono::NaiveDateTime::parse_from_str(&compact, "%Y%m%dT%H%M%SZ")?
            };
            let end_utc = chrono::NaiveDateTime::parse_from_str(end, "%Y%m%dT%H%M%SZ")?;
            let seconds = end_utc.signed_duration_since(start_utc).num_seconds();
            anyhow::ensure!(seconds >= 0, "calendar instance end precedes start");
            patch.insert(
                format!("recurrenceOverrides/{key}/duration"),
                serde_json::json!(format!("PT{seconds}S")),
            );
        }
        if let Some(all_day) = fields.all_day_event {
            patch.insert(
                format!("recurrenceOverrides/{key}/showWithoutTime"),
                serde_json::Value::Bool(all_day),
            );
        }
        if let Some(busy_status) = fields.busy_status {
            let value = match busy_status {
                0 => "free",
                1 => "x-eas-tentative",
                2 => "busy",
                3 => "x-eas-out-of-office",
                _ => anyhow::bail!("unsupported calendar availability"),
            };
            patch.insert(
                format!("recurrenceOverrides/{key}/freeBusyStatus"),
                serde_json::json!(value),
            );
        }
        if let Some(categories) = &fields.categories {
            let keywords: BTreeMap<_, _> = categories.iter().map(|name| (name, true)).collect();
            patch.insert(
                format!("recurrenceOverrides/{key}/keywords"),
                serde_json::json!(keywords),
            );
        }
        if fields.location.is_some() || fields.location_coordinates.is_some() {
            let mut location = serde_json::Map::new();
            if let Some(name) = fields.location.as_deref().filter(|name| !name.is_empty()) {
                location.insert("name".to_owned(), serde_json::json!(name));
            }
            if let Some((latitude, longitude)) = &fields.location_coordinates {
                location.insert(
                    "coordinates".to_owned(),
                    serde_json::json!(format!("geo:{latitude},{longitude}")),
                );
            }
            let locations = if location.is_empty() {
                serde_json::json!({})
            } else {
                serde_json::json!({"loc1": location})
            };
            patch.insert(format!("recurrenceOverrides/{key}/locations"), locations);
        }
        if patch.is_empty() {
            return Ok(());
        }
        let update = serde_json::json!({
            "accountId": account_id,
            "ifInState": state,
            "update": { id: patch }
        });
        let response: JmapResponse<serde_json::Value> = self
            .api_call(
                auth,
                &[
                    capabilities::CORE.to_owned(),
                    capabilities::CALENDARS.to_owned(),
                ],
                vec![MethodCall::new(
                    "CalendarEvent/set",
                    update,
                    "instance-change",
                )],
            )
            .await?;
        let result = response
            .method_responses
            .iter()
            .find(|method| method.0 == "CalendarEvent/set")
            .context("calendar exception update failed")?;
        if let Some(error) = result.1["notUpdated"].get(id) {
            anyhow::bail!("calendar exception update rejected: {error}");
        }
        anyhow::ensure!(
            result.1["updated"].get(id).is_some(),
            "calendar exception update was not confirmed"
        );
        Ok(())
    }

    /// Tasks live in the SAME `CalendarEvent` storage as Events on this
    /// Stalwart instance -- confirmed live (2026-08-25) that `@type:
    /// "Task"` objects are accepted and round-trip `title`/`due`/`start`/
    /// `progress`/`percentComplete`/`priority`/`description` correctly.
    /// `CalendarEvent/query` has no server-side filter for `@type`
    /// (`{"type": "Task"}` and `{"@type": "Task"}` both come back
    /// `unsupportedFilter` -- checked live, not assumed), so this fetches
    /// every item in the calendar and filters to `@type == "Task"`
    /// client-side, same as it would for a mixed Events+Tasks calendar in
    /// any other JMAP client.
    pub async fn tasks_in_calendar(
        &self,
        auth: &AuthenticatedSession,
        calendar_id: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<Task>> {
        let items: Vec<TaskObject> = self
            .query_objects(
                auth,
                capabilities::CALENDARS,
                "CalendarEvent",
                serde_json::json!({ "inCalendar": calendar_id }),
                Some(&[
                    "id",
                    "@type",
                    "title",
                    "description",
                    "descriptionContentType",
                    "due",
                    "timeZone",
                    "progress",
                    "progressUpdated",
                    "start",
                    "recurrenceRule",
                    "priority",
                    "privacy",
                    "keywords",
                    "alerts",
                    "useDefaultAlerts",
                ]),
                limit,
            )
            .await?;
        items
            .into_iter()
            .filter(|item| item.kind == "Task")
            .map(try_task_from_object)
            .collect()
    }

    pub async fn searchable_tasks_in_calendar(
        &self,
        auth: &AuthenticatedSession,
        calendar_id: &str,
    ) -> anyhow::Result<Vec<(Task, serde_json::Value)>> {
        let items: Vec<serde_json::Value> = self
            .query_objects(
                auth,
                capabilities::CALENDARS,
                "CalendarEvent",
                serde_json::json!({ "inCalendar": calendar_id }),
                None,
                0,
            )
            .await?;
        items
            .into_iter()
            .filter(|item| item["@type"] == "Task")
            .map(|item| {
                let task =
                    try_task_from_object(serde_json::from_value::<TaskObject>(item.clone())?)?;
                Ok((task, item))
            })
            .collect()
    }

    /// Creates (`id: None`) or updates (`id: Some`) a Task-typed
    /// `CalendarEvent`, mirroring `save_calendar_event`'s structure and
    /// the same live-confirmed in-place `update` support (the JMAP id is
    /// a stable ActiveSync ServerId across edits, no workaround needed).
    /// Existing time-zoned tasks retain their zone when their UTC due date changes.
    pub async fn save_task(
        &self,
        auth: &AuthenticatedSession,
        calendar_id: &str,
        id: Option<&str>,
        fields: &crate::wbxml::eas::TaskFields,
    ) -> anyhow::Result<String> {
        let Some(account_id) = auth.session.primary_account_for(capabilities::CALENDARS) else {
            anyhow::bail!("JMAP Calendars capability is not available");
        };

        anyhow::ensure!(!fields.unsupported_body, "unsupported task body");
        anyhow::ensure!(!fields.unsupported_complete, "invalid task Complete value");
        anyhow::ensure!(
            !fields.unsupported_categories,
            "unsupported task categories"
        );
        anyhow::ensure!(!fields.unsupported_reminder, "unsupported task reminder");
        anyhow::ensure!(
            !fields.unsupported_recurrence,
            "unsupported task recurrence"
        );
        let mut task = serde_json::Map::new();
        if let Some(body) = &fields.body {
            task.insert("description".to_owned(), serde_json::json!(body));
            task.insert(
                "descriptionContentType".to_owned(),
                serde_json::json!(if fields.body_type == Some(2) {
                    "text/html"
                } else {
                    "text/plain"
                }),
            );
        }
        task.insert(
            "@type".to_owned(),
            serde_json::Value::String("Task".to_owned()),
        );
        if let Some(subject) = &fields.subject {
            task.insert(
                "title".to_owned(),
                serde_json::Value::String(subject.clone()),
            );
        }
        if let Some(importance) = fields.importance.as_deref() {
            let priority = eas_importance_to_jmap(importance).context("invalid task importance")?;
            task.insert("priority".to_owned(), serde_json::json!(priority));
        }
        if let Some(sensitivity) = fields.sensitivity.as_deref() {
            let privacy =
                eas_task_sensitivity_to_jmap(sensitivity).context("invalid task sensitivity")?;
            task.insert("privacy".to_owned(), serde_json::json!(privacy));
        }
        if let Some(categories) = &fields.categories {
            let keywords: std::collections::BTreeMap<_, _> =
                categories.iter().map(|category| (category, true)).collect();
            task.insert("keywords".to_owned(), serde_json::json!(keywords));
        }
        let mut update_state = None;
        let needs_lookup = fields.due_date.is_some()
            || fields.utc_due_date.is_some()
            || fields.start_date.is_some()
            || fields.utc_start_date.is_some()
            || fields.reminder_set.is_some()
            || fields.reminder_time.is_some()
            || fields.recurrence_present;
        let mut timezone = None;
        let mut current_alerts = serde_json::Value::Null;
        let mut use_default_alerts = false;
        if let Some(id) = id.filter(|_| needs_lookup) {
            let response: JmapResponse<serde_json::Value> = self
                .api_call(
                    auth,
                    &[
                        capabilities::CORE.to_owned(),
                        capabilities::CALENDARS.to_owned(),
                    ],
                    vec![MethodCall::new(
                        "CalendarEvent/get",
                        serde_json::json!({
                            "accountId":account_id,"ids":[id],"properties":["id","timeZone","alerts","useDefaultAlerts"]
                        }),
                        "0",
                    )],
                )
                .await?;
            let result = response
                .method_responses
                .iter()
                .find(|method| method.0 == "CalendarEvent/get")
                .context("task state lookup failed")?;
            update_state = Some(
                result.1["state"]
                    .as_str()
                    .context("task state missing")?
                    .to_owned(),
            );
            let item = result.1["list"]
                .as_array()
                .and_then(|list| list.iter().find(|item| item["id"] == id))
                .context("task not found")?;
            timezone = match item.get("timeZone") {
                None | Some(serde_json::Value::Null) => None,
                Some(zone) => Some(zone.as_str().context("invalid task time zone")?.to_owned()),
            };
            current_alerts = item
                .get("alerts")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            use_default_alerts = item["useDefaultAlerts"].as_bool().unwrap_or(false);
        }
        if fields.recurrence_present {
            match fields.recurrence.as_ref() {
                Some(recurrence) => {
                    task.insert(
                        "recurrenceRule".to_owned(),
                        crate::jmap::calendar_recurrence::to_jscalendar(
                            recurrence,
                            timezone.as_deref(),
                        )?,
                    );
                }
                None => {
                    task.insert("recurrenceRule".to_owned(), serde_json::Value::Null);
                }
            }
        }
        if fields.due_date.is_some()
            || fields.utc_due_date.is_some()
            || fields.start_date.is_some()
            || fields.utc_start_date.is_some()
        {
            let parse = |value: &str| -> anyhow::Result<String> {
                eas_compact_to_local_iso(value)
                    .or_else(|| {
                        chrono::DateTime::parse_from_rfc3339(value)
                            .ok()
                            .filter(|date| date.timestamp_subsec_nanos() == 0)
                            .map(|date| date.naive_utc().format("%Y-%m-%dT%H:%M:%S").to_string())
                    })
                    .context("invalid task date")
            };
            let write_date = |local: Option<String>,
                              utc: Option<String>,
                              label: &str|
             -> anyhow::Result<Option<String>> {
                if local.is_none() && utc.is_none() {
                    return Ok(None);
                }
                let value = if let Some(timezone) = timezone.as_deref() {
                    let utc = utc.as_deref().with_context(|| {
                        format!("task UTC {label} required for a time-zoned task")
                    })?;
                    let compact = chrono::NaiveDateTime::parse_from_str(utc, "%Y-%m-%dT%H:%M:%S")?
                        .format("%Y%m%dT%H%M%SZ")
                        .to_string();
                    eas_to_calendar_local(&compact, Some(timezone))
                        .with_context(|| {
                            format!("task {label} cannot be represented in its time zone")
                        })?
                        .format("%Y-%m-%dT%H:%M:%S")
                        .to_string()
                } else {
                    if let (Some(local), Some(utc)) = (&local, &utc) {
                        anyhow::ensure!(
                            local == utc,
                            "task local and UTC dates require a known time zone"
                        );
                    }
                    utc.or(local)
                        .with_context(|| format!("task {label} missing"))?
                };
                Ok(Some(value))
            };
            if let Some(start) = write_date(
                fields.start_date.as_deref().map(parse).transpose()?,
                fields.utc_start_date.as_deref().map(parse).transpose()?,
                "start date",
            )? {
                task.insert("start".to_owned(), serde_json::Value::String(start));
            }
            if let Some(due) = write_date(
                fields.due_date.as_deref().map(parse).transpose()?,
                fields.utc_due_date.as_deref().map(parse).transpose()?,
                "due date",
            )? {
                task.insert("due".to_owned(), serde_json::Value::String(due));
            }
        }
        if let Some(complete) = fields.complete {
            task.insert(
                "progress".to_owned(),
                serde_json::Value::String(
                    if complete {
                        "completed"
                    } else {
                        "needs-action"
                    }
                    .to_owned(),
                ),
            );
        }
        if let Some(date_completed) = fields.date_completed.as_deref() {
            let timestamp = eas_compact_to_local_iso(date_completed)
                .and_then(|date| {
                    chrono::NaiveDateTime::parse_from_str(&date, "%Y-%m-%dT%H:%M:%S").ok()
                })
                .map(|date| date.format("%Y-%m-%dT%H:%M:%SZ").to_string())
                .or_else(|| {
                    chrono::DateTime::parse_from_rfc3339(date_completed)
                        .ok()
                        .map(|date| {
                            date.with_timezone(&chrono::Utc)
                                .format("%Y-%m-%dT%H:%M:%SZ")
                                .to_string()
                        })
                })
                .context("invalid task completion date")?;
            anyhow::ensure!(
                fields.complete != Some(false),
                "completion date conflicts with incomplete task"
            );
            task.insert(
                "progressUpdated".to_owned(),
                serde_json::Value::String(timestamp),
            );
        }
        crate::jmap::task_reminder::write_reminder(
            &mut task,
            &current_alerts,
            use_default_alerts,
            fields.reminder_set,
            fields.reminder_time.as_deref(),
        )?;

        let call = match id {
            Some(existing_id) => MethodCall::new(
                "CalendarEvent/set",
                serde_json::json!({
                    "accountId": account_id,
                    "ifInState": update_state,
                    "update": { existing_id: task }
                }),
                "0",
            ),
            None => {
                task.insert(
                    "calendarIds".to_owned(),
                    serde_json::json!({ calendar_id: true }),
                );
                MethodCall::new(
                    "CalendarEvent/set",
                    serde_json::json!({
                        "accountId": account_id,
                        "create": { "t1": task }
                    }),
                    "0",
                )
            }
        };

        let response: JmapResponse<serde_json::Value> = self
            .api_call(
                auth,
                &[
                    capabilities::CORE.to_owned(),
                    capabilities::CALENDARS.to_owned(),
                ],
                vec![call],
            )
            .await?;

        for method in response.method_responses {
            if method.0 == "error" {
                anyhow::bail!("JMAP method error in CalendarEvent/set (task)");
            }
            if method.0 == "CalendarEvent/set" {
                if let Some(existing_id) = id {
                    let updated = method
                        .1
                        .get("updated")
                        .and_then(|value| value.get(existing_id));
                    if updated.is_some() {
                        return Ok(existing_id.to_owned());
                    }
                    anyhow::bail!(
                        "CalendarEvent/set did not confirm task update for {existing_id}: {:?}",
                        method.1.get("notUpdated")
                    );
                }
                if let Some(new_id) = method
                    .1
                    .get("created")
                    .and_then(|c| c.get("t1"))
                    .and_then(|task| task.get("id"))
                    .and_then(|value| value.as_str())
                {
                    return Ok(new_id.to_owned());
                }
                anyhow::bail!(
                    "CalendarEvent/set did not return a created task id: {:?}",
                    method.1.get("notCreated")
                );
            }
        }
        anyhow::bail!("CalendarEvent/set response was missing")
    }

    /// Fetches a single Email by its JMAP id (mail ServerId is the raw
    /// JMAP Email id, unlike Notes' separate stable-id scheme). Used by
    /// ItemOperations/Fetch -- the request iOS sends when a user opens a
    /// message from the Sync list, even though the full body was already
    /// included inline in that Sync response.
    pub async fn get_email_by_id(
        &self,
        auth: &AuthenticatedSession,
        email_id: &str,
    ) -> anyhow::Result<Option<Email>> {
        let Some(account_id) = auth.session.primary_account_for(capabilities::MAIL) else {
            return Ok(None);
        };

        let identities = self.draft_identity_map(auth).await?;
        let backend_id = identities.backend(email_id);
        let calls = vec![MethodCall::new(
            "Email/get",
            serde_json::json!({
                "accountId": account_id,
                "ids": [backend_id],
                "properties": [
                    "id", "blobId", "mailboxIds", "keywords", "receivedAt", "subject",
                    "from", "to", "cc", "bcc", "replyTo", "header:To:asRaw", "header:Cc:asRaw",
                            "header:Bcc:asRaw", "header:Reply-To:asRaw", "header:Importance:asText", "textBody", "htmlBody", "bodyValues", "bodyStructure", "attachments", "threadId"
                ],
                "fetchAllBodyValues": true,
                "maxBodyValueBytes": 65536
            }),
            "g",
        )];

        let response: JmapResponse<serde_json::Value> = self
            .api_call(
                auth,
                &[capabilities::CORE.to_owned(), capabilities::MAIL.to_owned()],
                calls,
            )
            .await?;

        for method in response.method_responses {
            match method.0.as_str() {
                "Email/get" => {
                    let get: GetResponse<EmailObject> =
                        serde_json::from_value(method.1).context("invalid Email/get response")?;
                    return Ok(get
                        .list
                        .into_iter()
                        .next()
                        .map(Email::from)
                        .map(|email| identities.email(email)));
                }
                "error" => anyhow::bail!("JMAP method error in email fetch"),
                _ => {}
            }
        }

        Ok(None)
    }

    /// Fetches decoded text bodies; zero means no byte limit (RFC 8621).
    pub async fn email_body_by_id(
        &self,
        auth: &AuthenticatedSession,
        email_id: &str,
        max_bytes: usize,
    ) -> anyhow::Result<Option<EmailBody>> {
        self.email_body_with_format(auth, email_id, max_bytes, false)
            .await
    }

    pub async fn email_plain_body_by_id(
        &self,
        auth: &AuthenticatedSession,
        email_id: &str,
    ) -> anyhow::Result<Option<EmailBody>> {
        self.email_body_with_format(auth, email_id, 0, true).await
    }

    async fn email_body_with_format(
        &self,
        auth: &AuthenticatedSession,
        email_id: &str,
        max_bytes: usize,
        plain: bool,
    ) -> anyhow::Result<Option<EmailBody>> {
        let account_id = auth
            .session
            .primary_account_for(capabilities::MAIL)
            .ok_or_else(|| anyhow::anyhow!("JMAP Mail capability is unavailable"))?;
        let identities = self.draft_identity_map(auth).await?;
        let backend_id = identities.backend(email_id);
        let response: JmapResponse<serde_json::Value> = self
            .api_call(
                auth,
                &[capabilities::CORE.to_owned(), capabilities::MAIL.to_owned()],
                vec![MethodCall::new(
                    "Email/get",
                    serde_json::json!({
                        "accountId": account_id, "ids": [backend_id],
                        "properties": ["id", "textBody", "htmlBody", "bodyValues"],
                        "fetchAllBodyValues": true, "maxBodyValueBytes": max_bytes
                    }),
                    "body",
                )],
            )
            .await?;
        for method in response.method_responses {
            match method.0.as_str() {
                "Email/get" => {
                    let get: GetResponse<EmailObject> = serde_json::from_value(method.1)
                        .context("invalid Email/get body response")?;
                    return Ok(get
                        .list
                        .into_iter()
                        .find(|email| email.id == backend_id)
                        .and_then(|mut email| {
                            if plain {
                                let [part] = email.text_body.as_slice() else {
                                    return None;
                                };
                                let valid = part
                                    .part_id
                                    .as_ref()
                                    .and_then(|id| email.body_values.get(id))
                                    .is_some_and(|value| !value.is_encoding_problem);
                                if !valid {
                                    return None;
                                }
                                body_value_for(
                                    &email.text_body,
                                    &mut email.body_values,
                                    EmailBodyType::Plain,
                                )
                            } else {
                                select_body(&mut email)
                            }
                        }));
                }
                "error" => anyhow::bail!("JMAP method error fetching the message body"),
                _ => {}
            }
        }
        anyhow::bail!("Email/get body response was missing")
    }

    pub async fn create_email_sync_item(
        &self,
        auth: &AuthenticatedSession,
        item: EmailSyncItem,
    ) -> anyhow::Result<DraftCreated> {
        let EmailSyncItem {
            folder,
            is_draft,
            replay,
            mut properties,
            mime,
            attachments,
        } = item;
        let identity_marker = replay.0.as_str();
        let content_marker = replay.1.as_str();
        let baseline = replay.2.as_str();
        let account_id = auth
            .session
            .primary_account_for(capabilities::MAIL)
            .context("JMAP Mail capability is not available")?;
        anyhow::ensure!(
            properties["mailboxIds"][folder] == true,
            "Sync Add target mailbox does not match its collection"
        );
        let item_key = if is_draft { "draft" } else { "item" };
        let response: JmapResponse<serde_json::Value> = self
            .api_call(
                auth,
                &[capabilities::CORE.to_owned(), capabilities::MAIL.to_owned()],
                vec![MethodCall::new(
                    "Email/query",
                    serde_json::json!({
                        "accountId":account_id, "filter":{"hasKeyword":identity_marker}, "limit":2
                    }),
                    "q",
                )],
            )
            .await?;
        let query = response
            .method_responses
            .iter()
            .find(|method| method.0 == "Email/query")
            .ok_or_else(|| anyhow::anyhow!("Sync Add replay query failed"))?;
        let ids = query.1["ids"]
            .as_array()
            .context("draft replay query omitted ids")?;
        match ids.as_slice() {
            [id] => {
                let id = id.as_str().context("invalid draft id")?;
                let response: JmapResponse<serde_json::Value> = self
                    .api_call(
                        auth,
                        &[capabilities::CORE.to_owned(), capabilities::MAIL.to_owned()],
                        vec![MethodCall::new(
                            "Email/get",
                            serde_json::json!({
                                "accountId":account_id, "ids":[id], "properties":["id","keywords"]
                            }),
                            "get",
                        )],
                    )
                    .await?;
                let object = response
                    .method_responses
                    .iter()
                    .find(|method| method.0 == "Email/get")
                    .and_then(|method| method.1["list"].as_array())
                    .and_then(|items| items.iter().find(|item| item["id"] == id))
                    .context("Sync Add replay item disappeared")?;
                if object["keywords"][content_marker] != true {
                    anyhow::bail!("draft replay content differs");
                }
                return self.finish_created_email(auth, id, &attachments).await;
            }
            [] => {}
            _ => anyhow::bail!("duplicate draft replay identity"),
        }
        if !attachments.is_empty() {
            let mut parts = Vec::new();
            for attachment in &attachments {
                let blob = self
                    .upload_blob(auth, attachment.content.clone(), &attachment.content_type)
                    .await?;
                let mut part = serde_json::json!({
                    "blobId":blob, "type":attachment.content_type, "name":attachment.name,
                    "disposition":if attachment.inline {"inline"} else {"attachment"}
                });
                if let Some(cid) = &attachment.content_id {
                    part["cid"] = serde_json::json!(cid);
                }
                if let Some(location) = &attachment.content_location {
                    part["location"] = serde_json::json!(location);
                }
                parts.push(part);
            }
            properties["attachments"] = serde_json::json!(parts);
        }
        let (method, arguments) = if let Some(mime) = mime {
            let blob = self.upload_blob(auth, mime, "message/rfc822").await?;
            let mut emails = serde_json::Map::new();
            emails.insert(
                item_key.to_owned(),
                serde_json::json!({"blobId":blob,"mailboxIds":properties["mailboxIds"],"keywords":properties["keywords"]}),
            );
            (
                "Email/import",
                serde_json::json!({
                    "accountId":account_id, "ifInState":baseline,
                    "emails":emails
                }),
            )
        } else {
            let mut create = serde_json::Map::new();
            create.insert(item_key.to_owned(), properties);
            (
                "Email/set",
                serde_json::json!({
                    "accountId":account_id, "ifInState":baseline, "create":create
                }),
            )
        };
        let response: JmapResponse<serde_json::Value> = self
            .api_call(
                auth,
                &[capabilities::CORE.to_owned(), capabilities::MAIL.to_owned()],
                vec![MethodCall::new(method, arguments, "create")],
            )
            .await?;
        let result = response
            .method_responses
            .iter()
            .find(|response| response.0 == method)
            .ok_or_else(|| anyhow::anyhow!("Sync Add creation failed"))?;
        let id = result.1["created"][item_key]["id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("JMAP did not create Sync Add item"))?;
        let verify: JmapResponse<serde_json::Value> = self
            .api_call(
                auth,
                &[capabilities::CORE.to_owned(), capabilities::MAIL.to_owned()],
                vec![MethodCall::new(
                    "Email/get",
                    serde_json::json!({
                        "accountId":account_id, "ids":[id], "properties":["id","keywords"]
                    }),
                    "verify",
                )],
            )
            .await?;
        let object = verify
            .method_responses
            .iter()
            .find(|method| method.0 == "Email/get")
            .and_then(|method| method.1["list"].as_array())
            .and_then(|items| items.iter().find(|item| item["id"] == id))
            .context("created Sync Add item is unavailable")?;
        if object["keywords"][identity_marker] != true || object["keywords"][content_marker] != true
        {
            anyhow::bail!("JMAP did not retain Sync Add replay markers");
        }
        self.finish_created_email(auth, id, &attachments).await
    }

    async fn finish_created_email(
        &self,
        auth: &AuthenticatedSession,
        id: &str,
        attachments: &[crate::model::DraftAttachment],
    ) -> anyhow::Result<DraftCreated> {
        if attachments.is_empty() {
            return Ok(DraftCreated {
                id: id.to_owned(),
                attachments: Vec::new(),
            });
        }
        let account_id = auth
            .session
            .primary_account_for(capabilities::MAIL)
            .context("JMAP Mail capability is not available")?;
        let response: JmapResponse<serde_json::Value> = self
            .api_call(
                auth,
                &[capabilities::CORE.to_owned(), capabilities::MAIL.to_owned()],
                vec![MethodCall::new(
                    "Email/get",
                    serde_json::json!({
                        "accountId":account_id, "ids":[id], "properties":["id","attachments"]
                    }),
                    "get",
                )],
            )
            .await?;
        let object = response
            .method_responses
            .iter()
            .find(|method| method.0 == "Email/get")
            .and_then(|method| method.1["list"].as_array())
            .and_then(|items| items.iter().find(|item| item["id"] == id))
            .context("created Sync Add item is unavailable")?;
        let mut candidates = object["attachments"]
            .as_array()
            .context("created draft omitted attachments")?
            .clone();
        if candidates.len() != attachments.len() {
            anyhow::bail!("Sync Add attachment count differs");
        }
        let mut refs = Vec::new();
        for expected in attachments {
            let mut matched = None;
            for (index, candidate) in candidates.iter().enumerate() {
                if candidate["name"].as_str() != Some(expected.name.as_str())
                    || candidate["size"].as_u64() != Some(expected.content.len() as u64)
                    || candidate["type"].as_str().is_none_or(|kind| {
                        !kind.eq_ignore_ascii_case(
                            expected.content_type.split(';').next().unwrap_or("").trim(),
                        )
                    })
                    || candidate["cid"].as_str() != expected.content_id.as_deref()
                    || candidate["location"].as_str() != expected.content_location.as_deref()
                    || (candidate["disposition"].as_str() == Some("inline")) != expected.inline
                {
                    continue;
                }
                let Some(blob) = candidate["blobId"].as_str() else {
                    continue;
                };
                let data = self.download_blob(auth, blob, &expected.name).await?;
                if data == expected.content {
                    matched = Some((index, blob.to_owned()));
                    break;
                }
            }
            let (index, blob) = matched.context("Sync Add attachment did not round-trip")?;
            candidates.remove(index);
            refs.push((expected.client_id.clone(), blob, expected.name.clone()));
        }
        Ok(DraftCreated {
            id: id.to_owned(),
            attachments: refs,
        })
    }

    pub async fn update_email_state(
        &self,
        auth: &AuthenticatedSession,
        email_id: &str,
        read: Option<bool>,
        flag: Option<&crate::model::MailFlag>,
    ) -> anyhow::Result<EmailState> {
        for attempt in 0..3 {
            match self
                .update_email_state_once(auth, email_id, read, flag)
                .await
            {
                Err(error) if error.is::<EmailStateMismatch>() && attempt < 2 => continue,
                result => return result,
            }
        }
        unreachable!()
    }

    async fn update_email_state_once(
        &self,
        auth: &AuthenticatedSession,
        email_id: &str,
        read: Option<bool>,
        flag: Option<&crate::model::MailFlag>,
    ) -> anyhow::Result<EmailState> {
        let account_id = auth
            .session
            .primary_account_for(capabilities::MAIL)
            .context("JMAP Mail capability is not available")?;
        let identities = self.draft_identity_map(auth).await?;
        let email_id = identities.backend(email_id);
        let response: JmapResponse<serde_json::Value> = self
            .api_call(
                auth,
                &[capabilities::CORE.to_owned(), capabilities::MAIL.to_owned()],
                vec![MethodCall::new(
                    "Email/get",
                    serde_json::json!({
                        "accountId":account_id, "ids":[email_id], "properties":["id","keywords"]
                    }),
                    "get",
                )],
            )
            .await?;
        let result = response
            .method_responses
            .iter()
            .find(|method| method.0 == "Email/get")
            .ok_or_else(|| anyhow::anyhow!("JMAP Email/get response was missing"))?;
        let state = result.1["state"]
            .as_str()
            .context("Email/get omitted state")?;
        let object = result.1["list"]
            .as_array()
            .and_then(|list| list.iter().find(|item| item["id"] == email_id))
            .context("Email/get omitted the message being updated")?;
        let mut keywords: BTreeMap<String, bool> =
            serde_json::from_value(object["keywords"].clone())?;
        let mut patch = serde_json::Map::new();
        if let Some(read) = read {
            patch.insert(
                "keywords/$seen".to_owned(),
                if read {
                    serde_json::json!(true)
                } else {
                    serde_json::Value::Null
                },
            );
            if read {
                keywords.insert("$seen".to_owned(), true);
            } else {
                keywords.remove("$seen");
            }
        }
        if let Some(flag) = flag {
            let existing = EmailState::from_keywords(&keywords);
            let mut merged = flag.clone();
            if flag.status == u8::MAX || flag.status == 1 {
                let previous = existing.flag.or_else(|| {
                    existing.flagged.then(|| crate::model::MailFlag {
                        status: 2,
                        fields: vec![(
                            crate::wbxml::eas::email::PAGE,
                            crate::wbxml::eas::email::FLAG_TYPE.token,
                            "Flag for follow up".to_owned(),
                        )],
                    })
                });
                if let Some(previous) = previous {
                    merged = previous;
                    if flag.status == 1 {
                        merged.status = 1;
                    }
                    for field in &flag.fields {
                        merged
                            .fields
                            .retain(|old| old.0 != field.0 || old.1 != field.1);
                        merged.fields.push(field.clone());
                    }
                } else if flag.status == u8::MAX {
                    anyhow::bail!("cannot update metadata on an unflagged email");
                }
            }
            let flag = &merged;
            for key in keywords
                .keys()
                .filter(|key| key.starts_with(crate::model::FLAG_KEYWORD_PREFIX))
                .cloned()
                .collect::<Vec<_>>()
            {
                patch.insert(format!("keywords/{key}"), serde_json::Value::Null);
                keywords.remove(&key);
            }
            if flag.status != 0 {
                for key in flag.keywords()? {
                    patch.insert(format!("keywords/{key}"), serde_json::json!(true));
                    keywords.insert(key, true);
                }
            }
            patch.insert(
                "keywords/$flagged".to_owned(),
                if flag.status == 2 {
                    serde_json::json!(true)
                } else {
                    serde_json::Value::Null
                },
            );
            if flag.status == 2 {
                keywords.insert("$flagged".to_owned(), true);
            } else {
                keywords.remove("$flagged");
            }
        }
        self.email_set(
            auth,
            serde_json::json!({
                "accountId":account_id, "ifInState":state, "update":{email_id:patch}
            }),
        )
        .await?;
        let response: JmapResponse<serde_json::Value> = self
            .api_call(
                auth,
                &[capabilities::CORE.to_owned(), capabilities::MAIL.to_owned()],
                vec![MethodCall::new(
                    "Email/get",
                    serde_json::json!({
                        "accountId":account_id, "ids":[email_id], "properties":["id","keywords"]
                    }),
                    "verify",
                )],
            )
            .await?;
        let verified = response
            .method_responses
            .iter()
            .find(|method| method.0 == "Email/get")
            .and_then(|method| method.1["list"].as_array())
            .and_then(|items| items.iter().find(|item| item["id"] == email_id))
            .context("updated email could not be verified")?;
        let observed: BTreeMap<String, bool> =
            serde_json::from_value(verified["keywords"].clone())?;
        let has = |map: &BTreeMap<String, bool>, key: &str| map.get(key).copied().unwrap_or(false);
        let flags = |map: &BTreeMap<String, bool>| -> BTreeSet<String> {
            map.iter()
                .filter(|(key, value)| {
                    **value && key.starts_with(crate::model::FLAG_KEYWORD_PREFIX)
                })
                .map(|(key, _)| key.clone())
                .collect()
        };
        if (read.is_some() && has(&observed, "$seen") != has(&keywords, "$seen"))
            || (flag.is_some()
                && (has(&observed, "$flagged") != has(&keywords, "$flagged")
                    || flags(&observed) != flags(&keywords)))
        {
            anyhow::bail!("JMAP did not retain the requested email state");
        }
        let keywords = observed;
        Ok(identities.state(email_id, EmailState::from_keywords(&keywords)))
    }

    pub async fn delete_email(
        &self,
        auth: &AuthenticatedSession,
        email_id: &str,
        mailbox_id: &str,
        as_move: bool,
    ) -> anyhow::Result<()> {
        if as_move {
            let trash = self
                .find_mailbox_by_role(auth, "trash")
                .await?
                .context("Deleted Items mailbox is unavailable")?;
            if trash != mailbox_id {
                return self.move_email(auth, email_id, mailbox_id, &trash).await;
            }
        }
        self.destroy_email(auth, email_id).await
    }

    pub async fn destroy_email(
        &self,
        auth: &AuthenticatedSession,
        email_id: &str,
    ) -> anyhow::Result<()> {
        let Some(account_id) = auth.session.primary_account_for(capabilities::MAIL) else {
            anyhow::bail!("JMAP Mail capability is not available");
        };
        let identities = self.draft_identity_map(auth).await?;
        let email_id = identities.backend(email_id);

        self.email_set(
            auth,
            serde_json::json!({
                "accountId": account_id,
                "destroy": [email_id]
            }),
        )
        .await
    }

    /// Mail-folder create, backing `FolderCreate`. `parent_id: None`
    /// means the mailbox Root folder (EAS ParentId "0"), matching
    /// [MS-ASCMD] section 2.2.1.3.
    pub async fn create_mailbox(
        &self,
        auth: &AuthenticatedSession,
        parent_id: Option<&str>,
        name: &str,
    ) -> anyhow::Result<CreateMailboxOutcome> {
        let Some(account_id) = auth.session.primary_account_for(capabilities::MAIL) else {
            anyhow::bail!("JMAP Mail capability is not available");
        };
        let mut create = serde_json::Map::new();
        create.insert(
            "name".to_owned(),
            serde_json::Value::String(name.to_owned()),
        );
        if let Some(parent_id) = parent_id {
            create.insert(
                "parentId".to_owned(),
                serde_json::Value::String(parent_id.to_owned()),
            );
        }
        let mut create_map = serde_json::Map::new();
        create_map.insert("c1".to_owned(), serde_json::Value::Object(create));

        let response: JmapResponse<serde_json::Value> = self
            .api_call(
                auth,
                &[capabilities::CORE.to_owned(), capabilities::MAIL.to_owned()],
                vec![MethodCall::new(
                    "Mailbox/set",
                    serde_json::json!({ "accountId": account_id, "create": create_map }),
                    "0",
                )],
            )
            .await?;

        for method in response.method_responses {
            if method.0 == "Mailbox/set" {
                if let Some(id) = method
                    .1
                    .get("created")
                    .and_then(|c| c.get("c1"))
                    .and_then(|m| m.get("id"))
                    .and_then(|v| v.as_str())
                {
                    return Ok(CreateMailboxOutcome::Created(id.to_owned()));
                }
                // Don't key-match the notCreated map by our own "c1"
                // client id -- take whatever single entry is there. Not
                // just defensive: confirmed live that Stalwart's
                // notUpdated/notDestroyed maps can echo back a
                // TRUNCATED version of a long id we sent as the key
                // (e.g. "doesnotexist999" came back as "esnotexist999"),
                // so exact-matching the key we sent is not reliable.
                let error_type = method
                    .1
                    .get("notCreated")
                    .and_then(|nc| nc.as_object())
                    .and_then(|nc| nc.values().next())
                    .and_then(|e| e.get("type"))
                    .and_then(|t| t.as_str());
                return Ok(match error_type {
                    Some("alreadyExists") => CreateMailboxOutcome::NameExists,
                    Some("invalidProperties") => CreateMailboxOutcome::ParentNotFound,
                    other => anyhow::bail!(
                        "Mailbox/set create failed with unrecognized error type {other:?}: {:?}",
                        method.1
                    ),
                });
            } else if method.0 == "error" {
                anyhow::bail!("JMAP method error in Mailbox/set create");
            }
        }
        anyhow::bail!("JMAP Mailbox/set create response was missing")
    }

    /// Rename/reparent, backing `FolderUpdate`. `parent_id: None` means
    /// move to Root (EAS ParentId "0").
    pub async fn update_mailbox(
        &self,
        auth: &AuthenticatedSession,
        mailbox_id: &str,
        parent_id: Option<&str>,
        name: &str,
    ) -> anyhow::Result<UpdateMailboxOutcome> {
        let Some(account_id) = auth.session.primary_account_for(capabilities::MAIL) else {
            anyhow::bail!("JMAP Mail capability is not available");
        };
        let mut patch = serde_json::Map::new();
        patch.insert(
            "name".to_owned(),
            serde_json::Value::String(name.to_owned()),
        );
        patch.insert(
            "parentId".to_owned(),
            parent_id
                .map(|p| serde_json::Value::String(p.to_owned()))
                .unwrap_or(serde_json::Value::Null),
        );
        let mut update = serde_json::Map::new();
        update.insert(mailbox_id.to_owned(), serde_json::Value::Object(patch));

        let response: JmapResponse<serde_json::Value> = self
            .api_call(
                auth,
                &[capabilities::CORE.to_owned(), capabilities::MAIL.to_owned()],
                vec![MethodCall::new(
                    "Mailbox/set",
                    serde_json::json!({ "accountId": account_id, "update": update }),
                    "0",
                )],
            )
            .await?;

        for method in response.method_responses {
            if method.0 == "Mailbox/set" {
                if method
                    .1
                    .get("updated")
                    .and_then(|u| u.as_object())
                    .is_some_and(|u| !u.is_empty())
                {
                    return Ok(UpdateMailboxOutcome::Updated);
                }
                let error_type = method
                    .1
                    .get("notUpdated")
                    .and_then(|nu| nu.as_object())
                    .and_then(|nu| nu.values().next())
                    .and_then(|e| e.get("type"))
                    .and_then(|t| t.as_str());
                return Ok(match error_type {
                    Some("notFound") => UpdateMailboxOutcome::NotFound,
                    Some("forbidden") => UpdateMailboxOutcome::Forbidden,
                    Some("alreadyExists") => UpdateMailboxOutcome::NameExists,
                    other => anyhow::bail!(
                        "Mailbox/set update failed with unrecognized error type {other:?}: {:?}",
                        method.1
                    ),
                });
            } else if method.0 == "error" {
                anyhow::bail!("JMAP method error in Mailbox/set update");
            }
        }
        anyhow::bail!("JMAP Mailbox/set update response was missing")
    }

    pub async fn destroy_mailbox(
        &self,
        auth: &AuthenticatedSession,
        mailbox_id: &str,
    ) -> anyhow::Result<DestroyMailboxOutcome> {
        let Some(account_id) = auth.session.primary_account_for(capabilities::MAIL) else {
            anyhow::bail!("JMAP Mail capability is not available");
        };
        let response: JmapResponse<serde_json::Value> = self
            .api_call(
                auth,
                &[capabilities::CORE.to_owned(), capabilities::MAIL.to_owned()],
                vec![MethodCall::new(
                    "Mailbox/set",
                    serde_json::json!({ "accountId": account_id, "destroy": [mailbox_id] }),
                    "0",
                )],
            )
            .await?;

        for method in response.method_responses {
            if method.0 == "Mailbox/set" {
                let destroyed = method
                    .1
                    .get("destroyed")
                    .and_then(|d| d.as_array())
                    .is_some_and(|d| !d.is_empty());
                if destroyed {
                    return Ok(DestroyMailboxOutcome::Destroyed);
                }
                let error_type = method
                    .1
                    .get("notDestroyed")
                    .and_then(|nd| nd.as_object())
                    .and_then(|nd| nd.values().next())
                    .and_then(|e| e.get("type"))
                    .and_then(|t| t.as_str());
                return Ok(match error_type {
                    Some("notFound") => DestroyMailboxOutcome::NotFound,
                    Some("forbidden") => DestroyMailboxOutcome::Forbidden,
                    other => anyhow::bail!(
                        "Mailbox/set destroy failed with unrecognized error type {other:?}: {:?}",
                        method.1
                    ),
                });
            } else if method.0 == "error" {
                anyhow::bail!("JMAP method error in Mailbox/set destroy");
            }
        }
        anyhow::bail!("JMAP Mailbox/set destroy response was missing")
    }

    pub async fn create_groupware_collection(
        &self,
        auth: &AuthenticatedSession,
        kind: GroupwareCollectionKind,
        name: &str,
    ) -> anyhow::Result<CreateGroupwareCollectionOutcome> {
        let Some(account_id) = auth.session.primary_account_for(kind.capability()) else {
            return Ok(CreateGroupwareCollectionOutcome::Unsupported);
        };
        let method_name = format!("{}/set", kind.object());
        let response: JmapResponse<serde_json::Value> = self
            .api_call(
                auth,
                &[capabilities::CORE.to_owned(), kind.capability().to_owned()],
                vec![MethodCall::new(
                    &method_name,
                    serde_json::json!({
                        "accountId": account_id,
                        "create": {"newCollection": {"name": name}}
                    }),
                    "0",
                )],
            )
            .await?;
        for method in response.method_responses {
            if method.0 == method_name {
                if let Some(id) = method
                    .1
                    .get("created")
                    .and_then(|created| created.get("newCollection"))
                    .and_then(|object| object.get("id"))
                    .and_then(serde_json::Value::as_str)
                {
                    return Ok(CreateGroupwareCollectionOutcome::Created(id.to_owned()));
                }
                let error_type = method
                    .1
                    .get("notCreated")
                    .and_then(serde_json::Value::as_object)
                    .and_then(|errors| errors.values().next())
                    .and_then(|error| error.get("type"))
                    .and_then(serde_json::Value::as_str);
                return match error_type {
                    Some("alreadyExists") => Ok(CreateGroupwareCollectionOutcome::NameExists),
                    Some("forbidden") | Some("accountReadOnly") => {
                        Ok(CreateGroupwareCollectionOutcome::Unsupported)
                    }
                    other => anyhow::bail!(
                        "{method_name} create failed with error type {other:?}: {:?}",
                        method.1
                    ),
                };
            }
            if method.0 == "error" {
                anyhow::bail!("JMAP method error in {method_name} create: {:?}", method.1);
            }
        }
        anyhow::bail!("JMAP {method_name} create response was missing")
    }

    pub async fn update_groupware_collection(
        &self,
        auth: &AuthenticatedSession,
        kind: GroupwareCollectionKind,
        id: &str,
        name: &str,
    ) -> anyhow::Result<UpdateGroupwareCollectionOutcome> {
        let Some(account_id) = auth.session.primary_account_for(kind.capability()) else {
            return Ok(UpdateGroupwareCollectionOutcome::Unsupported);
        };
        let method_name = format!("{}/set", kind.object());
        let response: JmapResponse<serde_json::Value> = self
            .api_call(
                auth,
                &[capabilities::CORE.to_owned(), kind.capability().to_owned()],
                vec![MethodCall::new(
                    &method_name,
                    serde_json::json!({
                        "accountId": account_id,
                        "update": {(id): {"name": name}}
                    }),
                    "0",
                )],
            )
            .await?;
        for method in response.method_responses {
            if method.0 == method_name {
                if method
                    .1
                    .get("updated")
                    .and_then(serde_json::Value::as_object)
                    .is_some_and(|updated| !updated.is_empty())
                {
                    return Ok(UpdateGroupwareCollectionOutcome::Updated);
                }
                let error_type = method
                    .1
                    .get("notUpdated")
                    .and_then(serde_json::Value::as_object)
                    .and_then(|errors| errors.values().next())
                    .and_then(|error| error.get("type"))
                    .and_then(serde_json::Value::as_str);
                return match error_type {
                    Some("notFound") => Ok(UpdateGroupwareCollectionOutcome::NotFound),
                    Some("forbidden") => Ok(UpdateGroupwareCollectionOutcome::Forbidden),
                    Some("alreadyExists") => Ok(UpdateGroupwareCollectionOutcome::NameExists),
                    other => anyhow::bail!(
                        "{method_name} update failed with error type {other:?}: {:?}",
                        method.1
                    ),
                };
            }
            if method.0 == "error" {
                anyhow::bail!("JMAP method error in {method_name} update: {:?}", method.1);
            }
        }
        anyhow::bail!("JMAP {method_name} update response was missing")
    }

    pub async fn destroy_groupware_collection(
        &self,
        auth: &AuthenticatedSession,
        kind: GroupwareCollectionKind,
        id: &str,
    ) -> anyhow::Result<DestroyGroupwareCollectionOutcome> {
        let Some(account_id) = auth.session.primary_account_for(kind.capability()) else {
            return Ok(DestroyGroupwareCollectionOutcome::Unsupported);
        };
        let method_name = format!("{}/set", kind.object());
        let response: JmapResponse<serde_json::Value> = self
            .api_call(
                auth,
                &[capabilities::CORE.to_owned(), kind.capability().to_owned()],
                vec![MethodCall::new(
                    &method_name,
                    serde_json::json!({ "accountId": account_id, "destroy": [id] }),
                    "0",
                )],
            )
            .await?;
        for method in response.method_responses {
            if method.0 == method_name {
                if method
                    .1
                    .get("destroyed")
                    .and_then(serde_json::Value::as_array)
                    .is_some_and(|destroyed| {
                        destroyed.iter().any(|destroyed_id| destroyed_id == id)
                    })
                {
                    return Ok(DestroyGroupwareCollectionOutcome::Destroyed);
                }
                let error_type = method
                    .1
                    .get("notDestroyed")
                    .and_then(serde_json::Value::as_object)
                    .and_then(|errors| errors.values().next())
                    .and_then(|error| error.get("type"))
                    .and_then(serde_json::Value::as_str);
                return match error_type {
                    Some("notFound") => Ok(DestroyGroupwareCollectionOutcome::NotFound),
                    Some("forbidden") => Ok(DestroyGroupwareCollectionOutcome::Forbidden),
                    other => anyhow::bail!(
                        "{method_name} destroy failed with error type {other:?}: {:?}",
                        method.1
                    ),
                };
            }
            if method.0 == "error" {
                anyhow::bail!("JMAP method error in {method_name} destroy: {:?}", method.1);
            }
        }
        anyhow::bail!("JMAP {method_name} destroy response was missing")
    }

    pub async fn move_email(
        &self,
        auth: &AuthenticatedSession,
        email_id: &str,
        source_mailbox_id: &str,
        destination_mailbox_id: &str,
    ) -> anyhow::Result<()> {
        let Some(account_id) = auth.session.primary_account_for(capabilities::MAIL) else {
            anyhow::bail!("JMAP Mail capability is not available");
        };
        let identities = self.draft_identity_map(auth).await?;
        let email_id = identities.backend(email_id);

        let mut patch = serde_json::Map::new();
        patch.insert(
            format!("mailboxIds/{source_mailbox_id}"),
            serde_json::Value::Null,
        );
        patch.insert(
            format!("mailboxIds/{destination_mailbox_id}"),
            serde_json::Value::Bool(true),
        );
        let mut update = serde_json::Map::new();
        update.insert(email_id.to_owned(), serde_json::Value::Object(patch));

        self.email_set(
            auth,
            serde_json::json!({
                "accountId": account_id,
                "update": update
            }),
        )
        .await
    }

    /// Uploads raw bytes as a JMAP blob and returns the assigned blobId.
    /// Nothing needed this before Notes (create/save flows didn't exist
    /// yet) -- general-purpose, will also be needed for SendMail/drafts/
    /// attachments later.
    pub async fn download_blob(
        &self,
        auth: &AuthenticatedSession,
        blob_id: &str,
        name: &str,
    ) -> anyhow::Result<Vec<u8>> {
        Ok(self.download_blob_with_type(auth, blob_id, name).await?.0)
    }

    pub async fn download_blob_with_type(
        &self,
        auth: &AuthenticatedSession,
        blob_id: &str,
        name: &str,
    ) -> anyhow::Result<(Vec<u8>, String)> {
        let Some(account_id) = auth.session.primary_account_for(capabilities::MAIL) else {
            anyhow::bail!("JMAP Mail capability is not available");
        };
        self.download_blob_from_account(auth, account_id, blob_id, name)
            .await
    }

    pub async fn download_blob_with_type_limited(
        &self,
        auth: &AuthenticatedSession,
        blob_id: &str,
        name: &str,
        max_bytes: usize,
    ) -> anyhow::Result<Option<(Vec<u8>, String)>> {
        let Some(account_id) = auth.session.primary_account_for(capabilities::MAIL) else {
            anyhow::bail!("JMAP Mail capability is not available");
        };
        self.download_blob_from_account_limited(auth, account_id, blob_id, name, Some(max_bytes))
            .await
    }

    pub async fn download_calendar_blob_with_type(
        &self,
        auth: &AuthenticatedSession,
        blob_id: &str,
        name: &str,
    ) -> anyhow::Result<(Vec<u8>, String)> {
        let Some(account_id) = auth.session.primary_account_for(capabilities::CALENDARS) else {
            anyhow::bail!("JMAP Calendars capability is not available");
        };
        self.download_blob_from_account(auth, account_id, blob_id, name)
            .await
    }

    async fn download_contact_blob_limited(
        &self,
        auth: &AuthenticatedSession,
        blob_id: &str,
        name: &str,
        max_bytes: usize,
    ) -> anyhow::Result<Option<(Vec<u8>, String)>> {
        let account_id = auth
            .session
            .primary_account_for(capabilities::CONTACTS)
            .context("JMAP Contacts capability is not available")?;
        self.download_blob_from_account_limited(auth, account_id, blob_id, name, Some(max_bytes))
            .await
    }

    async fn download_blob_from_account(
        &self,
        auth: &AuthenticatedSession,
        account_id: &str,
        blob_id: &str,
        name: &str,
    ) -> anyhow::Result<(Vec<u8>, String)> {
        self.download_blob_from_account_limited(auth, account_id, blob_id, name, None)
            .await?
            .ok_or_else(|| anyhow::anyhow!("unbounded JMAP blob download was limited"))
    }

    async fn download_blob_from_account_limited(
        &self,
        auth: &AuthenticatedSession,
        account_id: &str,
        blob_id: &str,
        name: &str,
        max_bytes: Option<usize>,
    ) -> anyhow::Result<Option<(Vec<u8>, String)>> {
        let url = auth
            .session
            .download_url
            .replace("{accountId}", &percent_encode_path_segment(account_id))
            .replace("{blobId}", &percent_encode_path_segment(blob_id))
            .replace("{name}", &percent_encode_path_segment(name))
            .replace("{type}", "application%2Foctet-stream");
        let response = self
            .http
            .get(&url)
            .basic_auth(
                &auth.authorization.username,
                Some(&auth.authorization.password),
            )
            .send()
            .await
            .context("failed to download JMAP blob")?;
        let status = response.status();
        if !status.is_success() {
            anyhow::bail!("JMAP blob download from {url} returned {status}");
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_owned();
        if let Some(max_bytes) = max_bytes {
            if response
                .content_length()
                .is_some_and(|length| length > max_bytes as u64)
            {
                return Ok(None);
            }
            let mut response = response;
            let mut bytes = Vec::with_capacity(
                response
                    .content_length()
                    .unwrap_or_default()
                    .min(max_bytes as u64) as usize,
            );
            while let Some(chunk) = response
                .chunk()
                .await
                .context("failed to read blob download body")?
            {
                if bytes.len().saturating_add(chunk.len()) > max_bytes {
                    return Ok(None);
                }
                bytes.extend_from_slice(&chunk);
            }
            return Ok(Some((bytes, content_type)));
        }
        let bytes = response
            .bytes()
            .await
            .context("failed to read blob download body")?
            .to_vec();
        Ok(Some((bytes, content_type)))
    }

    pub(crate) async fn upload_blob(
        &self,
        auth: &AuthenticatedSession,
        data: Vec<u8>,
        content_type: &str,
    ) -> anyhow::Result<String> {
        let Some(account_id) = auth.session.primary_account_for(capabilities::MAIL) else {
            anyhow::bail!("JMAP Mail capability is not available");
        };
        let url = auth
            .session
            .upload_url
            .replace("{accountId}", &percent_encode_path_segment(account_id));
        let response = self
            .http
            .post(&url)
            .basic_auth(
                &auth.authorization.username,
                Some(&auth.authorization.password),
            )
            .header(reqwest::header::CONTENT_TYPE, content_type)
            .body(data)
            .send()
            .await
            .context("failed to upload JMAP blob")?;

        let status = response.status();
        let body = response
            .text()
            .await
            .unwrap_or_else(|_| "<unreadable body>".to_owned());
        if !status.is_success() {
            anyhow::bail!("JMAP blob upload to {url} returned {status}: {body}");
        }

        #[derive(Deserialize)]
        struct UploadResponse {
            #[serde(rename = "blobId")]
            blob_id: String,
        }
        let parsed: UploadResponse =
            serde_json::from_str(&body).context("invalid blob upload JSON")?;
        Ok(parsed.blob_id)
    }

    /// Submits a raw RFC822 MIME message for delivery. Ports the exact
    /// sequence the PHP z-push fork (`jmap.php::SendMail()`) uses against
    /// this same Stalwart instance -- confirmed working live there
    /// (real send/receive test, per [[stalwart-relay-gotchas]]): upload the
    /// MIME as a blob, `Email/import` it (into Sent Items unless the
    /// client asked not to keep a copy), then `EmailSubmission/set` with
    /// `envelope: null` so Stalwart derives the envelope from the message
    /// headers itself. Also rewrites the `From:` header's display name to
    /// the account's JMAP Identity name when one exists, same as the PHP
    /// version -- some senders' MUAs don't set a friendly display name
    /// themselves, and this account's Identity does have one configured.
    pub async fn send_email(
        &self,
        auth: &AuthenticatedSession,
        mime: Vec<u8>,
        save_in_sent_items: bool,
    ) -> anyhow::Result<()> {
        let Some(account_id) = auth.session.primary_account_for(capabilities::MAIL) else {
            anyhow::bail!("JMAP Mail capability is not available");
        };

        let (identity_id, identity_name) = self.fetch_primary_identity(auth).await?;
        if identity_id.is_empty() {
            anyhow::bail!("account has no JMAP Identity configured -- cannot submit mail");
        }
        let mime = if identity_name.is_empty() {
            mime
        } else {
            rewrite_from_header(&mime, &identity_name)
        };

        let blob_id = self.upload_blob(auth, mime, "message/rfc822").await?;
        let role = if save_in_sent_items { "sent" } else { "drafts" };
        let mailbox_id = self
            .find_mailbox_by_role(auth, role)
            .await?
            .with_context(|| format!("account has no {role} mailbox"))?;
        let mailbox_ids = serde_json::json!({ mailbox_id: true });

        let import_response: JmapResponse<serde_json::Value> = self
            .api_call(
                auth,
                &[capabilities::CORE.to_owned(), capabilities::MAIL.to_owned()],
                vec![MethodCall::new(
                    "Email/import",
                    serde_json::json!({
                        "accountId": account_id,
                        "emails": {
                            "i1": {
                                "blobId": blob_id,
                                "mailboxIds": mailbox_ids,
                                "keywords": {"$seen": true},
                                "receivedAt": chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
                            }
                        }
                    }),
                    "im",
                )],
            )
            .await?;

        let mut imported_id = None;
        for method in import_response.method_responses {
            if method.0 == "Email/import" {
                imported_id = method
                    .1
                    .get("created")
                    .and_then(|c| c.get("i1"))
                    .and_then(|v| v.get("id"))
                    .and_then(|v| v.as_str())
                    .map(str::to_owned);
                if imported_id.is_none() {
                    let not_created = method.1.get("notCreated").and_then(|v| v.get("i1"));
                    anyhow::bail!("Email/import failed: {not_created:?}");
                }
            }
        }
        let Some(imported_id) = imported_id else {
            anyhow::bail!("Email/import response was missing");
        };

        let mut submission = serde_json::json!({
            "accountId": account_id,
            "create": {"s1": {"emailId": imported_id, "identityId": identity_id, "envelope": null}}
        });
        if !save_in_sent_items {
            submission["onSuccessDestroyEmail"] = serde_json::json!(["#s1"]);
        }
        let submit_response: JmapResponse<serde_json::Value> = self
            .api_call(
                auth,
                &[
                    capabilities::CORE.to_owned(),
                    capabilities::MAIL.to_owned(),
                    capabilities::SUBMISSION.to_owned(),
                ],
                vec![MethodCall::new("EmailSubmission/set", submission, "sub")],
            )
            .await?;

        confirm_submission(&submit_response)?;
        if !save_in_sent_items
            && !submit_response.method_responses.iter().any(|method| {
                method.0 == "Email/set"
                    && method.1["destroyed"]
                        .as_array()
                        .is_some_and(|ids| ids.iter().any(|id| id == &imported_id))
            })
        {
            tracing::warn!("submitted message temporary copy was not removed");
        }
        Ok(())
    }

    pub async fn mark_reply_or_forward(
        &self,
        auth: &AuthenticatedSession,
        id: &str,
        folder: Option<&str>,
        forwarded: bool,
    ) -> anyhow::Result<()> {
        let email = self
            .get_email_by_id(auth, id)
            .await?
            .context("source message not found")?;
        anyhow::ensure!(
            folder.is_none_or(|folder| email.mailbox_ids.iter().any(|id| id == folder)),
            "source message not in requested folder"
        );
        let account = auth
            .session
            .primary_account_for(capabilities::MAIL)
            .context("JMAP Mail capability is unavailable")?;
        let identities = self.draft_identity_map(auth).await?;
        let backend = identities.backend(id);
        let key = if forwarded {
            "keywords/$forwarded"
        } else {
            "keywords/$answered"
        };
        self.email_set(
            auth,
            serde_json::json!({"accountId":account, "update":{backend:{key:true}}}),
        )
        .await
    }

    pub async fn mail_identities(
        &self,
        auth: &AuthenticatedSession,
    ) -> anyhow::Result<Vec<MailIdentity>> {
        let account_id = auth
            .session
            .primary_account_for(capabilities::MAIL)
            .context("no primary mail account")?;
        let response: JmapResponse<serde_json::Value> = self
            .api_call(
                auth,
                &[
                    capabilities::CORE.to_owned(),
                    capabilities::MAIL.to_owned(),
                    capabilities::SUBMISSION.to_owned(),
                ],
                vec![MethodCall::new(
                    "Identity/get",
                    serde_json::json!({"accountId": account_id, "ids": null}),
                    "0",
                )],
            )
            .await?;
        for method in response.method_responses {
            if method.0 == "Identity/get" {
                let identities: GetResponse<MailIdentity> = serde_json::from_value(method.1)?;
                return Ok(identities.list);
            }
            if method.0 == "error" {
                anyhow::bail!("Identity/get failed");
            }
        }
        anyhow::bail!("missing Identity/get response")
    }

    async fn fetch_primary_identity(
        &self,
        auth: &AuthenticatedSession,
    ) -> anyhow::Result<(String, String)> {
        let identities = self.mail_identities(auth).await?;
        Ok(identities
            .into_iter()
            .next()
            .map(|identity| (identity.id, identity.name))
            .unwrap_or_default())
    }

    pub(crate) async fn find_mailbox_by_role(
        &self,
        auth: &AuthenticatedSession,
        role: &str,
    ) -> anyhow::Result<Option<String>> {
        let Some(account_id) = auth.session.primary_account_for(capabilities::MAIL) else {
            return Ok(None);
        };
        let response: JmapResponse<serde_json::Value> = self
            .api_call(
                auth,
                &[capabilities::CORE.to_owned(), capabilities::MAIL.to_owned()],
                vec![MethodCall::new(
                    "Mailbox/get",
                    serde_json::json!({ "accountId": account_id }),
                    "m",
                )],
            )
            .await?;
        for method in response.method_responses {
            if method.0 == "Mailbox/get" {
                let get: GetResponse<MailboxObject> = serde_json::from_value(method.1)
                    .context("invalid Mailbox/get response while locating mailbox by role")?;
                return Ok(get
                    .list
                    .into_iter()
                    .find(|mailbox| mailbox.role.as_deref() == Some(role))
                    .map(|mailbox| mailbox.id));
            }
        }
        Ok(None)
    }

    async fn email_set(
        &self,
        auth: &AuthenticatedSession,
        arguments: serde_json::Value,
    ) -> anyhow::Result<()> {
        let updates: Vec<String> = arguments["update"]
            .as_object()
            .map(|items| items.keys().cloned().collect())
            .unwrap_or_default();
        let destroys: Vec<String> = arguments["destroy"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|id| id.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        let response: JmapResponse<serde_json::Value> = self
            .api_call(
                auth,
                &[capabilities::CORE.to_owned(), capabilities::MAIL.to_owned()],
                vec![MethodCall::new("Email/set", arguments, "set")],
            )
            .await?;

        for method in response.method_responses {
            match method.0.as_str() {
                "Email/set" => {
                    for id in &updates {
                        if method.1["updated"].get(id).is_none() {
                            let kind = method.1["notUpdated"][id]["type"]
                                .as_str()
                                .unwrap_or("unconfirmed");
                            anyhow::bail!("JMAP did not update email ({kind})");
                        }
                    }
                    for id in &destroys {
                        let destroyed = method.1["destroyed"]
                            .as_array()
                            .is_some_and(|ids| ids.iter().any(|item| item.as_str() == Some(id)));
                        let kind = method.1["notDestroyed"][id]["type"]
                            .as_str()
                            .unwrap_or("unconfirmed");
                        if !destroyed && kind != "notFound" {
                            anyhow::bail!("JMAP did not delete email ({kind})");
                        }
                    }
                    return Ok(());
                }
                "error" if method.1["type"] == "stateMismatch" => {
                    return Err(EmailStateMismatch.into());
                }
                "error" => anyhow::bail!("JMAP method error in Email/set"),
                other => {
                    tracing::debug!(method = other, "ignoring unexpected JMAP method response")
                }
            }
        }

        anyhow::bail!("JMAP Email/set response was missing")
    }

    pub(crate) async fn api_call<T: DeserializeOwned>(
        &self,
        auth: &AuthenticatedSession,
        using: &[String],
        method_calls: Vec<MethodCall>,
    ) -> anyhow::Result<JmapResponse<T>> {
        let api_url = Url::parse(&auth.session.api_url).context("invalid JMAP apiUrl")?;
        let response = self
            .http
            .post(api_url)
            .basic_auth(
                &auth.authorization.username,
                Some(&auth.authorization.password),
            )
            .json(&JmapRequest {
                using,
                method_calls,
            })
            .send()
            .await
            .context("failed to call JMAP API")?;
        let status = response.status();
        if !status.is_success() {
            // RFC 7807 problem details from Stalwart: type/title/detail only,
            // never mail content. Truncated so a large body can't flood logs.
            let body = response.text().await.unwrap_or_default();
            let detail: String = body.chars().take(300).collect();
            anyhow::bail!("JMAP API returned error status {status}: {detail}");
        }

        response.json().await.context("invalid JMAP API JSON")
    }
}

#[derive(Clone, Debug, Default)]
pub struct MeetingResponseOptions<'a> {
    pub expected_organizer: Option<&'a str>,
    pub send_response: bool,
    pub instance_id: Option<&'a str>,
    pub custom_body: Option<MeetingResponseBody>,
    pub proposed_times: Option<MeetingResponseProposal>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MeetingResponseBody {
    pub kind: u8,
    pub data: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MeetingResponseProposal {
    pub start: chrono::DateTime<chrono::Utc>,
    pub end: chrono::DateTime<chrono::Utc>,
}

struct MeetingResponseMimeContext<'a> {
    uid: &'a str,
    organizer: &'a str,
    attendee: &'a str,
    participation: &'a str,
    recurrence_key: Option<&'a str>,
    instance_id: Option<&'a str>,
    time_zone: Option<&'a str>,
    title: &'a str,
    description: Option<&'a str>,
    location: Option<&'a str>,
    sequence: u64,
    recurrence_rule: Option<&'a serde_json::Value>,
}

fn counter_event_location(locations: &serde_json::Value) -> Option<String> {
    let names: Vec<_> = locations
        .as_object()?
        .values()
        .filter_map(|location| location["name"].as_str())
        .filter(|name| !name.trim().is_empty())
        .collect();
    match names.as_slice() {
        [] => None,
        [name] => Some((*name).to_owned()),
        // JSCalendar allows multiple locations while iCalendar LOCATION is a
        // single text property. Preserve each named option in deterministic
        // map order rather than silently dropping all but one.
        _ => Some(names.join(" / ")),
    }
}

fn series_counter_supported(event: &serde_json::Value) -> bool {
    let utc_series = event["timeZone"]
        .as_str()
        .is_none_or(|zone| matches!(zone, "UTC" | "Etc/UTC" | "GMT" | "Etc/GMT"));
    let has_exceptions = event["recurrenceOverrides"]
        .as_object()
        .is_some_and(|overrides| !overrides.is_empty());
    // The counter MIME emits UTC DTSTART/DTEND and RRULE, but cannot preserve
    // local wall time or per-instance exceptions yet.
    utc_series && !has_exceptions
}

fn counter_recurrence_rule(
    value: &serde_json::Value,
    time_zone: Option<&str>,
) -> anyhow::Result<String> {
    let recurrence = crate::jmap::calendar_recurrence::from_jscalendar(value, time_zone)?;
    let (frequency, weekday_mask) = match recurrence.kind {
        0 if recurrence.day_of_week.is_some() => ("WEEKLY", recurrence.day_of_week),
        0 => ("DAILY", None),
        1 => ("WEEKLY", recurrence.day_of_week),
        2 | 3 => ("MONTHLY", recurrence.day_of_week),
        5 | 6 => ("YEARLY", recurrence.day_of_week),
        _ => anyhow::bail!("unsupported recurrence for time counter"),
    };
    let mut fields = vec![format!("FREQ={frequency}")];
    if let Some(interval) = recurrence.interval.filter(|interval| *interval != 1) {
        fields.push(format!("INTERVAL={interval}"));
    }
    if let Some(mask) = weekday_mask {
        const DAYS: [(u8, &str); 7] = [
            (1, "SU"),
            (2, "MO"),
            (4, "TU"),
            (8, "WE"),
            (16, "TH"),
            (32, "FR"),
            (64, "SA"),
        ];
        let days: Vec<_> = DAYS
            .iter()
            .filter(|(bit, _)| mask & bit != 0)
            .map(|(_, day)| *day)
            .collect();
        anyhow::ensure!(!days.is_empty(), "empty recurrence weekday mask");
        fields.push(format!("BYDAY={}", days.join(",")));
    }
    if let Some(day) = recurrence.day_of_month {
        fields.push(format!("BYMONTHDAY={day}"));
    }
    if let Some(month) = recurrence.month_of_year {
        fields.push(format!("BYMONTH={month}"));
    }
    if let Some(position) = recurrence
        .week_of_month
        .filter(|_| matches!(recurrence.kind, 3 | 6))
    {
        if recurrence.day_of_week == Some(127) {
            fields.push(format!("BYMONTHDAY={position}"));
        } else {
            fields.push(format!(
                "BYSETPOS={}",
                if position == 5 { -1 } else { position as i8 }
            ));
        }
    }
    if let Some(first_day) = recurrence.first_day_of_week {
        let week_start = match first_day {
            0 => "SU",
            1 => "MO",
            2 => "TU",
            3 => "WE",
            4 => "TH",
            5 => "FR",
            6 => "SA",
            _ => anyhow::bail!("invalid first day of week"),
        };
        fields.push(format!("WKST={week_start}"));
    }
    if let Some(count) = recurrence.occurrences {
        fields.push(format!("COUNT={count}"));
    }
    if let Some(until) = recurrence.until.as_deref() {
        let parsed = chrono::DateTime::parse_from_rfc3339(until)
            .map(|value| value.with_timezone(&chrono::Utc))
            .or_else(|_| {
                chrono::NaiveDateTime::parse_from_str(until, "%Y%m%dT%H%M%SZ")
                    .map(|value| value.and_utc())
            })?;
        fields.push(format!("UNTIL={}", parsed.format("%Y%m%dT%H%M%SZ")));
    }
    Ok(fields.join(";"))
}

impl AuthenticatedSession {
    pub fn username(&self) -> &str {
        &self.authorization.username
    }
}

fn calendar_text_escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace(';', "\\;")
        .replace(',', "\\,")
        .replace("\r\n", "\\n")
        .replace(['\r', '\n'], "\\n")
}

fn fold_ical_lines(value: &str) -> String {
    let mut folded = String::with_capacity(value.len() + 32);
    for line in value.split("\r\n").filter(|line| !line.is_empty()) {
        let mut width = 0;
        for character in line.chars() {
            let bytes = character.len_utf8();
            if width + bytes > 75 {
                folded.push_str("\r\n ");
                width = 1;
            }
            folded.push(character);
            width += bytes;
        }
        folded.push_str("\r\n");
    }
    folded
}

fn wrap_mime_base64(data: &[u8]) -> String {
    data.chunks(57)
        .map(|line| STANDARD.encode(line))
        .collect::<Vec<_>>()
        .join("\r\n")
}

fn is_safe_mailbox(value: &str) -> bool {
    let Some((local, domain)) = value.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && !domain.is_empty()
        && !domain.contains('@')
        && local
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b".!#$%&'*+-/=?^_`{|}~".contains(&byte))
        && domain
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
}

fn meeting_response_mime(
    body: Option<&MeetingResponseBody>,
    proposal: Option<&MeetingResponseProposal>,
    context: MeetingResponseMimeContext<'_>,
) -> anyhow::Result<Vec<u8>> {
    let MeetingResponseMimeContext {
        uid,
        organizer,
        attendee,
        participation,
        recurrence_key,
        instance_id,
        time_zone,
        title,
        description,
        location,
        sequence,
        recurrence_rule,
    } = context;
    anyhow::ensure!(
        body.is_some() || proposal.is_some(),
        "empty custom meeting reply"
    );
    anyhow::ensure!(
        body.is_none_or(|body| matches!(body.kind, 1..=3)),
        "unsupported MeetingResponse body type"
    );
    anyhow::ensure!(!uid.is_empty(), "invalid meeting reply UID");
    anyhow::ensure!(
        is_safe_mailbox(organizer) && is_safe_mailbox(attendee),
        "invalid meeting reply address"
    );
    let partstat = match participation {
        "accepted" => "ACCEPTED",
        "tentative" => "TENTATIVE",
        "declined" => "DECLINED",
        _ => anyhow::bail!("invalid meeting participation status"),
    };
    let now = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
    let method = if proposal.is_some() {
        "COUNTER"
    } else {
        "REPLY"
    };
    let mut calendar = format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//EAS JMAP Gateway//Meeting Response//EN\r\nMETHOD:{method}\r\nBEGIN:VEVENT\r\nUID:{}\r\nDTSTAMP:{now}\r\nORGANIZER:mailto:{}\r\nATTENDEE;PARTSTAT={partstat}:mailto:{}\r\n",
        calendar_text_escape(uid),
        calendar_text_escape(organizer),
        calendar_text_escape(attendee)
    );
    if let Some(proposal) = proposal {
        calendar.push_str(&format!(
            "DTSTART:{}\r\nDTEND:{}\r\nSEQUENCE:{sequence}\r\nSUMMARY:{}\r\n",
            proposal.start.format("%Y%m%dT%H%M%SZ"),
            proposal.end.format("%Y%m%dT%H%M%SZ"),
            calendar_text_escape(title),
        ));
        if let Some(description) = description {
            calendar.push_str(&format!(
                "DESCRIPTION:{}\r\n",
                calendar_text_escape(description)
            ));
        }
        if let Some(location) = location {
            calendar.push_str(&format!("LOCATION:{}\r\n", calendar_text_escape(location)));
        }
        if instance_id.is_none() {
            if let Some(recurrence_rule) = recurrence_rule {
                calendar.push_str(&format!(
                    "RRULE:{}\r\n",
                    counter_recurrence_rule(recurrence_rule, time_zone)?
                ));
            }
        }
    }
    if proposal.is_some() {
        if let Some(instance_id) = instance_id {
            let occurrence = chrono::DateTime::parse_from_rfc3339(instance_id)?
                .with_timezone(&chrono::Utc)
                .format("%Y%m%dT%H%M%SZ");
            calendar.push_str(&format!("RECURRENCE-ID:{occurrence}\r\n"));
        }
    } else if let Some(recurrence_key) = recurrence_key {
        anyhow::ensure!(
            !recurrence_key
                .bytes()
                .any(|byte| matches!(byte, b'\r' | b'\n' | b';')),
            "invalid meeting recurrence key"
        );
        let compact =
            if let Ok(date) = chrono::NaiveDate::parse_from_str(recurrence_key, "%Y-%m-%d") {
                date.format("%Y%m%d").to_string()
            } else {
                chrono::NaiveDateTime::parse_from_str(recurrence_key, "%Y-%m-%dT%H:%M:%S")?
                    .format("%Y%m%dT%H%M%S")
                    .to_string()
            };
        if let Some(time_zone) = time_zone.filter(|zone| !zone.is_empty()) {
            anyhow::ensure!(
                time_zone.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'_' | b'-' | b'+')
                }),
                "invalid meeting time zone"
            );
            calendar.push_str(&format!("RECURRENCE-ID;TZID={time_zone}:{compact}\r\n"));
        } else {
            calendar.push_str(&format!("RECURRENCE-ID:{compact}\r\n"));
        }
    }
    calendar.push_str("END:VEVENT\r\nEND:VCALENDAR\r\n");

    calendar = fold_ical_lines(&calendar);
    let boundary = format!("eas-meeting-{}", uuid::Uuid::new_v4().simple());
    let mime = if let Some(body) = body {
        let content_type = match body.kind {
            2 => "text/html",
            3 => "text/rtf",
            _ => "text/plain",
        };
        let encoded_body = wrap_mime_base64(body.data.as_bytes());
        format!(
            "To: <{}>\r\nFrom: <{}>\r\nSubject: Meeting response\r\nMIME-Version: 1.0\r\nContent-Type: multipart/alternative; boundary=\"{boundary}\"\r\n\r\n--{boundary}\r\nContent-Type: {content_type}; charset=utf-8\r\nContent-Transfer-Encoding: base64\r\n\r\n{encoded_body}\r\n--{boundary}\r\nContent-Type: text/calendar; method={method}; charset=utf-8\r\nContent-Transfer-Encoding: 8bit\r\n\r\n{calendar}--{boundary}--\r\n",
            organizer, attendee
        )
    } else {
        format!(
            "To: <{}>\r\nFrom: <{}>\r\nSubject: Meeting time proposal\r\nMIME-Version: 1.0\r\nContent-Type: text/calendar; method={method}; charset=utf-8\r\nContent-Transfer-Encoding: 8bit\r\n\r\n{calendar}",
            organizer, attendee
        )
    };
    Ok(mime.into_bytes())
}

pub fn basic_credentials(headers: &HeaderMap) -> anyhow::Result<(String, String)> {
    let Some(value) = headers.get(AUTHORIZATION.as_str()) else {
        anyhow::bail!("missing Authorization header");
    };
    let value = value.to_str().context("Authorization is not valid ASCII")?;
    let Some(encoded) = value.strip_prefix("Basic ") else {
        anyhow::bail!("unsupported Authorization scheme");
    };
    let decoded = STANDARD
        .decode(encoded)
        .context("invalid Basic authorization")?;
    let decoded = String::from_utf8(decoded).context("Basic authorization is not UTF-8")?;
    let Some((username, password)) = decoded.split_once(':') else {
        anyhow::bail!("Basic authorization is missing password separator");
    };
    Ok((username.to_owned(), password.to_owned()))
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct JmapRequest<'a> {
    using: &'a [String],
    method_calls: Vec<MethodCall>,
}

#[derive(Debug, Serialize)]
pub(crate) struct MethodCall(String, serde_json::Value, String);

impl MethodCall {
    pub(crate) fn new(name: &str, arguments: serde_json::Value, id: &str) -> Self {
        Self(name.to_owned(), arguments, id.to_owned())
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct JmapResponse<T> {
    pub(crate) method_responses: Vec<MethodResponse<T>>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct MethodResponse<T>(pub(crate) String, pub(crate) T, pub(crate) String);
fn invitation_event_for_create(
    event: &serde_json::Value,
    organizer: &str,
    user: &str,
    calendar_id: &str,
    participation: &str,
) -> anyhow::Result<Option<serde_json::Value>> {
    if organizer.eq_ignore_ascii_case(user) {
        return Ok(None);
    }
    if !event["recurrenceRule"].is_null()
        || event["recurrenceOverrides"]
            .as_object()
            .is_some_and(|overrides| !overrides.is_empty())
    {
        anyhow::bail!("recurring invitations are not supported");
    }
    let mut event = event.clone();
    let object = event.as_object_mut().context("invalid meeting event")?;
    object.remove("method");
    object.remove("iCalendar");
    object.remove("organizerCalendarAddress");
    object.insert(
        "replyTo".into(),
        serde_json::json!({"imip":format!("mailto:{organizer}")}),
    );
    object.insert("calendarIds".into(), serde_json::json!({calendar_id:true}));
    let Some(participants) = object
        .get_mut("participants")
        .and_then(serde_json::Value::as_object_mut)
    else {
        return Ok(None);
    };
    let mut owners = 0;
    let mut attendees = 0;
    for participant in participants.values_mut() {
        let Some(participant) = participant.as_object_mut() else {
            return Ok(None);
        };
        let address = participant
            .get("calendarAddress")
            .and_then(serde_json::Value::as_str)
            .or_else(|| participant.get("sendTo").and_then(|v| v["imip"].as_str()))
            .map(str::to_owned);
        let email = participant
            .get("email")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .or_else(|| {
                address
                    .as_deref()
                    .and_then(|value| {
                        value
                            .get(..7)
                            .filter(|prefix| prefix.eq_ignore_ascii_case("mailto:"))
                            .and_then(|_| value.get(7..))
                    })
                    .map(str::to_owned)
            });
        let Some(email) =
            email.filter(|email| !email.is_empty() && !email.chars().any(char::is_control))
        else {
            return Ok(None);
        };
        if participant
            .get("roles")
            .is_some_and(|roles| roles["owner"] == true)
        {
            if !email.eq_ignore_ascii_case(organizer) {
                return Ok(None);
            }
            owners += 1;
        } else if email.eq_ignore_ascii_case(user) {
            participant.insert(
                "participationStatus".into(),
                serde_json::json!(participation),
            );
            attendees += 1;
        }
        participant.insert("email".into(), serde_json::json!(email));
        if !participant.contains_key("sendTo") {
            let Some(address) = address else {
                return Ok(None);
            };
            participant.insert("sendTo".into(), serde_json::json!({"imip":address}));
        }
        participant.insert("scheduleAgent".into(), serde_json::json!("client"));
    }
    if owners != 1 || attendees != 1 {
        return Ok(None);
    }
    Ok(Some(event))
}

fn confirm_submission(response: &JmapResponse<serde_json::Value>) -> anyhow::Result<()> {
    let method = response
        .method_responses
        .iter()
        .find(|method| method.2 == "sub")
        .context("EmailSubmission/set response was missing")?;
    anyhow::ensure!(
        method.0 == "EmailSubmission/set",
        "EmailSubmission/set returned a method error"
    );
    anyhow::ensure!(
        method.1["notCreated"]["s1"].is_null(),
        "EmailSubmission/set rejected the message"
    );
    anyhow::ensure!(
        method.1["created"]["s1"]["id"]
            .as_str()
            .is_some_and(|id| !id.is_empty()),
        "EmailSubmission/set did not confirm the message"
    );
    Ok(())
}

#[derive(Debug, Deserialize)]
pub(crate) struct GetResponse<T> {
    #[serde(default)]
    pub(crate) list: Vec<T>,
}

/// Just the `total` out of an Email/query response (requires
/// `calculateTotal: true` on the call) -- used to detect whether a
/// windowed query left more messages unfetched, for EAS's MoreAvailable.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct QueryResponse {
    #[serde(rename = "queryState", default)]
    query_state: Option<String>,
    #[serde(default)]
    total: Option<u64>,
    #[serde(default)]
    ids: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct MailIdentity {
    pub id: String,
    #[serde(default)]
    pub name: String,
    pub email: String,
}

/// Minimal shape for `emails_still_in_mailbox`'s deletion-candidate
/// check -- deliberately lighter than `EmailObject`, only the two
/// properties that call actually requests.
/// GAL search result, mapped from Stalwart's `Principal` object. See
/// `search_principals`'s doc comment for why `display_name` prefers
/// `description` over `name` (the account's own email, not a human
/// name).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PrincipalResult {
    pub id: String,
    pub alias: String,
    pub display_name: String,
    pub email: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PrincipalObject {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    description: Option<String>,
}

impl From<PrincipalObject> for PrincipalResult {
    fn from(value: PrincipalObject) -> Self {
        Self {
            id: value.id,
            alias: value.name.clone(),
            display_name: value
                .description
                .filter(|d| !d.is_empty())
                .unwrap_or(value.name),
            email: value.email,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EmailMembership {
    id: String,
    #[serde(default)]
    mailbox_ids: BTreeMap<String, bool>,
    #[serde(default)]
    keywords: BTreeMap<String, bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EmailObject {
    id: String,
    #[serde(default)]
    blob_id: Option<String>,
    #[serde(default)]
    thread_id: Option<String>,
    #[serde(default)]
    mailbox_ids: BTreeMap<String, bool>,
    #[serde(default)]
    keywords: BTreeMap<String, bool>,
    #[serde(default)]
    received_at: Option<String>,
    #[serde(default)]
    sent_at: Option<String>,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    subject: String,
    // `EmailAddress[]|null` per JMAP (RFC 8621 4.1.2.3): Stalwart sends a
    // literal `null`, not a missing key, when a header (e.g. Cc on an
    // ordinary 1:1 email) is absent. `#[serde(default)]` alone only covers
    // a MISSING key -- a `null` value still fails to deserialize into a
    // bare `Vec`. `Option<Vec<_>>` handles both. This broke Email/get for
    // every real message with no Cc; never caught in testing because the
    // test account's mail always happened to have all three headers set.
    #[serde(default)]
    from: Option<Vec<EmailAddress>>,
    #[serde(default)]
    to: Option<Vec<EmailAddress>>,
    #[serde(default)]
    cc: Option<Vec<EmailAddress>>,
    #[serde(default)]
    bcc: Option<Vec<EmailAddress>>,
    #[serde(default)]
    reply_to: Option<Vec<EmailAddress>>,
    #[serde(default, rename = "header:To")]
    raw_to: Option<String>,
    #[serde(default, rename = "header:Cc")]
    raw_cc: Option<String>,
    #[serde(default, rename = "header:Bcc")]
    raw_bcc: Option<String>,
    #[serde(default, rename = "header:Reply-To")]
    raw_reply_to: Option<String>,
    #[serde(default, rename = "header:Importance:asText")]
    importance_header: Option<String>,
    #[serde(default)]
    preview: Option<String>,
    #[serde(default)]
    text_body: Vec<EmailBodyPart>,
    #[serde(default)]
    html_body: Vec<EmailBodyPart>,
    #[serde(default)]
    body_values: BTreeMap<String, EmailBodyValue>,
    #[serde(default)]
    attachments: Vec<EmailAttachmentObject>,
    #[serde(default)]
    body_structure: Option<MimePart>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MimePart {
    #[serde(default)]
    part_id: Option<String>,
    #[serde(default)]
    blob_id: Option<String>,
    #[serde(default, rename = "type")]
    content_type: String,
    #[serde(default)]
    sub_parts: Option<Vec<MimePart>>,
}

impl MimePart {
    fn part_ids_by_blob(&self, parts: &mut BTreeMap<String, String>) {
        if let (Some(blob_id), Some(part_id)) = (&self.blob_id, &self.part_id) {
            parts.insert(blob_id.clone(), part_id.clone());
        }
        for part in self.sub_parts.as_deref().unwrap_or_default() {
            part.part_ids_by_blob(parts);
        }
    }

    fn smime_kind(&self) -> Option<crate::model::SmimeKind> {
        if ["application/pkcs7-mime", "application/x-pkcs7-mime"]
            .iter()
            .any(|kind| self.content_type.eq_ignore_ascii_case(kind))
        {
            Some(crate::model::SmimeKind::Opaque)
        } else if self.content_type.eq_ignore_ascii_case("multipart/signed")
            && self
                .sub_parts
                .as_deref()
                .unwrap_or_default()
                .get(1)
                .is_some_and(|part| {
                    [
                        "application/pkcs7-signature",
                        "application/x-pkcs7-signature",
                    ]
                    .iter()
                    .any(|kind| part.content_type.eq_ignore_ascii_case(kind))
                })
        {
            Some(crate::model::SmimeKind::MultipartSigned)
        } else {
            None
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EmailAttachmentObject {
    blob_id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default, rename = "type")]
    content_type: Option<String>,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    cid: Option<String>,
    #[serde(default)]
    location: Option<String>,
    #[serde(default)]
    disposition: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct EmailAddress {
    name: Option<String>,
    email: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EmailBodyPart {
    part_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EmailBodyValue {
    #[serde(default)]
    value: String,
    #[serde(default)]
    is_truncated: bool,
    #[serde(default)]
    is_encoding_problem: bool,
}

impl From<EmailObject> for Email {
    fn from(mut value: EmailObject) -> Self {
        let body = select_body(&mut value);
        let mut part_ids = BTreeMap::new();
        if let Some(body_structure) = &value.body_structure {
            body_structure.part_ids_by_blob(&mut part_ids);
        }
        let attachments = value
            .attachments
            .into_iter()
            .map(|attachment| crate::model::EmailAttachment {
                part_id: part_ids.get(&attachment.blob_id).cloned(),
                blob_id: attachment.blob_id,
                name: attachment.name.unwrap_or_else(|| "attachment".to_owned()),
                content_type: attachment
                    .content_type
                    .unwrap_or_else(|| "application/octet-stream".to_owned()),
                size: attachment.size,
                content_id: attachment.cid,
                content_location: attachment.location,
                inline: attachment.disposition.as_deref() == Some("inline"),
                extracted_text: None,
            })
            .collect();
        let is_draft = value.keywords.get("$draft").copied().unwrap_or(false);
        let address = |raw: Option<String>, parsed: Option<Vec<EmailAddress>>| {
            if is_draft {
                if let Some(raw) = raw {
                    return raw
                        .replace("\r\n ", " ")
                        .replace("\r\n\t", " ")
                        .trim()
                        .to_owned();
                }
            }
            format_addresses(parsed.as_deref().unwrap_or_default())
        };
        Self {
            draft_revision: None,
            id: value.id,
            mailbox_ids: value
                .mailbox_ids
                .into_iter()
                .filter_map(|(id, present)| present.then_some(id))
                .collect(),
            subject: value.subject,
            received_at: value.received_at,
            sent_at: value.sent_at,
            size: value.size,
            keywords: value
                .keywords
                .iter()
                .filter_map(|(keyword, present)| present.then_some(keyword.clone()))
                .collect(),
            from: format_addresses(value.from.as_deref().unwrap_or_default()),
            to: address(value.raw_to, value.to),
            cc: address(value.raw_cc, value.cc),
            bcc: address(value.raw_bcc, value.bcc),
            reply_to: address(value.raw_reply_to, value.reply_to),
            importance: match value.importance_header.as_deref().map(str::trim) {
                Some(value) if value.eq_ignore_ascii_case("low") => 0,
                Some(value) if value.eq_ignore_ascii_case("high") => 2,
                _ => 1,
            },
            preview: value.preview,
            read: value.keywords.get("$seen").copied().unwrap_or(false),
            body,
            smime: value.body_structure.as_ref().and_then(MimePart::smime_kind),
            attachments,
            blob_id: value.blob_id,
            thread_id: value.thread_id,
        }
    }
}

fn select_body(email: &mut EmailObject) -> Option<EmailBody> {
    body_value_for(
        &email.html_body,
        &mut email.body_values,
        EmailBodyType::Html,
    )
    .or_else(|| {
        body_value_for(
            &email.text_body,
            &mut email.body_values,
            EmailBodyType::Plain,
        )
    })
}

fn body_value_for(
    parts: &[EmailBodyPart],
    values: &mut BTreeMap<String, EmailBodyValue>,
    body_type: EmailBodyType,
) -> Option<EmailBody> {
    parts.iter().find_map(|part| {
        let body = values.get_mut(part.part_id.as_ref()?)?;
        if body.value.is_empty() && !body.is_truncated {
            return None;
        }
        Some(EmailBody {
            body_type,
            value: std::mem::take(&mut body.value),
            is_truncated: body.is_truncated,
        })
    })
}

fn format_addresses(addresses: &[EmailAddress]) -> String {
    addresses
        .iter()
        .filter_map(|address| match (&address.name, &address.email) {
            (Some(name), Some(email)) if !name.is_empty() => Some(format!("{name} <{email}>")),
            (_, Some(email)) => Some(email.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Rewrites the `From:` header's display name to `name`, preserving the
/// original email address. Port of the PHP z-push fork's
/// `rewriteFromHeader()` -- same reasoning: some MUAs never set a friendly
/// display name on outgoing mail, so the account's JMAP Identity name is
/// used instead. Operates on logical (fold-joined) header lines rather
/// than a single-shot regex; falls back to returning the MIME unchanged
/// if it isn't valid UTF-8 or has no From header with a discoverable
/// address, rather than risk corrupting an otherwise-sendable message.
fn rewrite_from_header(mime: &[u8], name: &str) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(mime) else {
        return mime.to_vec();
    };
    let eol = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let separator = format!("{eol}{eol}");
    let Some(sep_idx) = text.find(&separator) else {
        return mime.to_vec();
    };
    let headers = &text[..sep_idx];
    let body = &text[sep_idx + separator.len()..];

    let mut logical: Vec<String> = Vec::new();
    for line in headers.split(eol) {
        if (line.starts_with(' ') || line.starts_with('\t')) && !logical.is_empty() {
            let last = logical.last_mut().expect("checked non-empty above");
            last.push(' ');
            last.push_str(line.trim_start());
        } else {
            logical.push(line.to_owned());
        }
    }

    let mut rewrote = false;
    for entry in logical.iter_mut() {
        let Some(colon) = entry.find(':') else {
            continue;
        };
        if !entry[..colon].eq_ignore_ascii_case("From") {
            continue;
        }
        let value = entry[colon + 1..].trim();
        if let Some(email) = extract_email_address(value) {
            let quoted_name = name.replace('\\', "\\\\").replace('"', "\\\"");
            *entry = format!("From: \"{quoted_name}\" <{email}>");
            rewrote = true;
        }
        break;
    }

    if !rewrote {
        return mime.to_vec();
    }

    let mut result = logical.join(eol);
    result.push_str(&separator);
    result.push_str(body);
    result.into_bytes()
}

fn extract_email_address(value: &str) -> Option<String> {
    if let Some(start) = value.find('<') {
        if let Some(end) = value[start..].find('>') {
            let addr = &value[start + 1..start + end];
            if !addr.is_empty() {
                return Some(addr.to_owned());
            }
        }
    }
    value
        .split_whitespace()
        .find(|token| token.contains('@'))
        .map(|token| {
            token
                .trim_matches(|ch: char| !ch.is_ascii_alphanumeric() && !"@.-_+".contains(ch))
                .to_owned()
        })
}

/// JSContact (RFC 9553) card shape, as returned by Stalwart's
/// `ContactCard/get` -- confirmed live against a real created card (see
/// stalwart-sync-gateway-cutover memory / this session's own testing).
/// Only the properties this gateway maps to EAS Contacts fields are
/// modeled; everything else in a real card is ignored, not round-tripped.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ContactCardObject {
    id: String,
    #[serde(default)]
    address_book_ids: BTreeMap<String, bool>,
    #[serde(default)]
    name: Option<ContactCardName>,
    #[serde(default)]
    nicknames: BTreeMap<String, ContactCardNickname>,
    #[serde(default)]
    online_services: BTreeMap<String, ContactCardOnlineService>,
    #[serde(default)]
    media: BTreeMap<String, ContactCardMedia>,
    #[serde(default)]
    emails: BTreeMap<String, ContactCardEmail>,
    #[serde(default)]
    phones: BTreeMap<String, ContactCardPhone>,
    #[serde(default)]
    organizations: BTreeMap<String, ContactCardOrganization>,
    #[serde(default)]
    titles: BTreeMap<String, ContactCardTitle>,
    #[serde(default)]
    addresses: BTreeMap<String, ContactCardAddress>,
    #[serde(default)]
    keywords: BTreeMap<String, bool>,
    #[serde(default)]
    anniversaries: BTreeMap<String, ContactCardAnniversary>,
    #[serde(default)]
    links: BTreeMap<String, ContactCardLink>,
    #[serde(default)]
    notes: BTreeMap<String, ContactCardNote>,
}

#[derive(Debug, Default, Deserialize)]
struct ContactCardNote {
    note: String,
}

#[derive(Debug, Default, Deserialize)]
struct ContactCardAnniversary {
    kind: String,
    date: serde_json::Value,
}

#[derive(Debug, Default, Deserialize)]
struct ContactCardLink {
    uri: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ContactCardName {
    full: Option<String>,
    #[serde(default)]
    components: Vec<ContactCardNameComponent>,
}

#[derive(Debug, Default, Deserialize)]
struct ContactCardNameComponent {
    kind: String,
    value: String,
    phonetic: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ContactCardNickname {
    name: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ContactCardMedia {
    kind: Option<String>,
    uri: Option<String>,
    #[serde(rename = "blobId")]
    blob_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ContactCardOnlineService {
    user: Option<String>,
    uri: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ContactCardEmail {
    address: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ContactCardPhone {
    number: Option<String>,
    #[serde(default)]
    contexts: BTreeMap<String, bool>,
    #[serde(default)]
    features: BTreeMap<String, bool>,
    label: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ContactCardOrganization {
    name: Option<String>,
    sort_as: Option<String>,
    #[serde(default)]
    units: Vec<ContactCardOrgUnit>,
}

#[derive(Debug, Default, Deserialize)]
struct ContactCardOrgUnit {
    name: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ContactCardTitle {
    name: Option<String>,
    kind: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ContactCardAddress {
    label: Option<String>,
    full: Option<String>,
    #[serde(default)]
    contexts: BTreeMap<String, bool>,
    #[serde(default)]
    components: Vec<ContactCardAddressComponent>,
}

#[derive(Debug, Default, Deserialize)]
struct ContactCardAddressComponent {
    kind: String,
    value: String,
}

fn eas_contact_picture_to_data_uri(value: &str) -> anyhow::Result<String> {
    let compact: String = value
        .chars()
        .filter(|ch| !ch.is_ascii_whitespace())
        .collect();
    let bytes = STANDARD
        .decode(compact)
        .context("invalid EAS contact picture base64")?;
    anyhow::ensure!(
        !bytes.is_empty() && bytes.len() <= 5 * 1024 * 1024,
        "EAS contact picture is empty or exceeds 5 MiB"
    );
    let media_type = if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        "image/png"
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        "image/jpeg"
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        "image/gif"
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        "image/webp"
    } else {
        anyhow::bail!("unsupported EAS contact picture format")
    };
    Ok(format!(
        "data:{media_type};base64,{}",
        STANDARD.encode(bytes)
    ))
}

fn jscontact_photo_data_uri_to_bytes(uri: &str) -> Option<Vec<u8>> {
    let (header, data) = uri.split_once(',')?;
    if !header.starts_with("data:image/") || !header.ends_with(";base64") {
        return None;
    }
    let bytes = STANDARD.decode(data).ok()?;
    (bytes.len() <= 5 * 1024 * 1024).then_some(bytes)
}

fn eas_contact_date_to_partial(value: &str) -> anyhow::Result<serde_json::Value> {
    use chrono::Datelike;
    let date = chrono::NaiveDate::parse_from_str(value, "%Y%m%d")
        .map_err(|_| anyhow::anyhow!("invalid EAS contact date"))?;
    Ok(serde_json::json!({
        "year": date.year(),
        "month": date.month(),
        "day": date.day()
    }))
}

fn partial_date_to_eas_contact(value: &serde_json::Value) -> Option<String> {
    use chrono::Datelike;
    let year = i32::try_from(value.get("year")?.as_i64()?).ok()?;
    let month = u32::try_from(value.get("month")?.as_u64()?).ok()?;
    let day = u32::try_from(value.get("day")?.as_u64()?).ok()?;
    let date = chrono::NaiveDate::from_ymd_opt(year, month, day)?;
    Some(format!(
        "{:04}{:02}{:02}",
        date.year(),
        date.month(),
        date.day()
    ))
}

impl From<ContactCardObject> for Contact {
    fn from(value: ContactCardObject) -> Self {
        let mut first_name = None;
        let mut middle_name = None;
        let mut last_name = None;
        let mut suffix = None;
        let mut name_prefix = None;
        let mut yomi_first_name = None;
        let mut yomi_last_name = None;
        if let Some(name) = &value.name {
            for component in &name.components {
                match component.kind.as_str() {
                    "given" => first_name = Some(component.value.clone()),
                    "given2" => middle_name = Some(component.value.clone()),
                    "surname" => last_name = Some(component.value.clone()),
                    "generation" | "credential" => suffix = Some(component.value.clone()),
                    "prefix" => name_prefix = Some(component.value.clone()),
                    _ => {}
                }
                match component.kind.as_str() {
                    "given" => yomi_first_name = component.phonetic.clone(),
                    "surname" => yomi_last_name = component.phonetic.clone(),
                    _ => {}
                }
            }
        }
        let nickname = value
            .nicknames
            .get("eas_nickname")
            .or_else(|| value.nicknames.values().next())
            .and_then(|nickname| nickname.name.clone());
        let file_as = value.name.as_ref().and_then(|name| name.full.clone());
        let picture = value
            .media
            .get("eas_picture")
            .or_else(|| {
                value
                    .media
                    .values()
                    .find(|media| media.kind.as_deref() == Some("photo"))
            })
            .and_then(|media| media.uri.as_deref())
            .and_then(jscontact_photo_data_uri_to_bytes);
        let notes = value
            .notes
            .get("eas_notes")
            .or_else(|| value.notes.values().next())
            .map(|note| crate::model::EmailBody {
                body_type: EmailBodyType::Plain,
                value: note.note.clone(),
                is_truncated: false,
            });
        let online_service = |key: &str| {
            value
                .online_services
                .get(key)
                .and_then(|service| service.user.clone().or_else(|| service.uri.clone()))
        };
        let im_address1 = online_service("eas_im1");
        let im_address2 = online_service("eas_im2");
        let im_address3 = online_service("eas_im3");
        let mms = online_service("eas_mms");
        let account_name = online_service("eas_account_name");
        let emails = value
            .emails
            .into_values()
            .filter_map(|email| email.address)
            .collect();

        let mut mobile_phone = None;
        let mut home_phones = Vec::new();
        let mut business_phones = Vec::new();
        let mut home_fax = None;
        let mut business_fax = None;
        let mut pager = None;
        let mut car_phone = None;
        let mut radio_phone = None;
        let mut assistant_phone = None;
        let mut company_main_phone = None;
        for phone in value.phones.into_values() {
            let Some(number) = phone.number else {
                continue;
            };
            let is = |ctx: &str| phone.contexts.get(ctx).copied().unwrap_or(false);
            let feature = |name: &str| phone.features.get(name).copied().unwrap_or(false);
            if (feature("main-number")
                || phone
                    .label
                    .as_deref()
                    .is_some_and(|label| label.eq_ignore_ascii_case("company main phone")))
                && company_main_phone.is_none()
            {
                company_main_phone = Some(number);
            } else if phone
                .label
                .as_deref()
                .is_some_and(|label| label.eq_ignore_ascii_case("car"))
                && car_phone.is_none()
            {
                car_phone = Some(number);
            } else if phone
                .label
                .as_deref()
                .is_some_and(|label| label.eq_ignore_ascii_case("radio"))
                && radio_phone.is_none()
            {
                radio_phone = Some(number);
            } else if phone
                .label
                .as_deref()
                .is_some_and(|label| label.eq_ignore_ascii_case("assistant"))
                && assistant_phone.is_none()
            {
                assistant_phone = Some(number);
            } else if feature("fax") && is("private") && home_fax.is_none() {
                home_fax = Some(number);
            } else if feature("fax") && is("work") && business_fax.is_none() {
                business_fax = Some(number);
            } else if feature("pager") && pager.is_none() {
                pager = Some(number);
            } else if (is("mobile") || feature("mobile")) && mobile_phone.is_none() {
                mobile_phone = Some(number);
            } else if is("home") {
                home_phones.push(number);
            } else if is("work") || phone.contexts.is_empty() {
                business_phones.push(number);
            }
        }

        let organizations: Vec<_> = value.organizations.into_values().collect();
        let company_organization = organizations.iter().find(|org| org.name.is_some());
        let company_name = company_organization.and_then(|org| org.name.clone());
        let yomi_company_name = company_organization.and_then(|org| org.sort_as.clone());
        let department = organizations
            .iter()
            .find_map(|org| org.units.last().and_then(|unit| unit.name.clone()));
        let job_title = value
            .titles
            .into_values()
            .find(|title| title.kind.as_deref() == Some("title") || title.kind.is_none())
            .and_then(|title| title.name);
        let birthday = value
            .anniversaries
            .values()
            .find(|anniversary| anniversary.kind == "birth")
            .and_then(|anniversary| partial_date_to_eas_contact(&anniversary.date));
        let anniversary = value
            .anniversaries
            .values()
            .find(|anniversary| anniversary.kind == "wedding")
            .and_then(|anniversary| partial_date_to_eas_contact(&anniversary.date));
        let web_page = value.links.into_values().find_map(|link| {
            link.uri
                .filter(|uri| uri.starts_with("http://") || uri.starts_with("https://"))
        });

        let mut business_address = None;
        let mut home_address = None;
        let mut other_address = None;
        let mut office_location = None;
        for address in value.addresses.into_values() {
            if address
                .label
                .as_deref()
                .is_some_and(|label| label.eq_ignore_ascii_case("office location"))
            {
                office_location = address.full;
                continue;
            }
            let mut mapped = crate::model::ContactAddress::default();
            for component in address.components {
                match component.kind.as_str() {
                    "locality" => mapped.city = Some(component.value),
                    "country" => mapped.country = Some(component.value),
                    "postcode" => mapped.postal_code = Some(component.value),
                    "region" => mapped.state = Some(component.value),
                    "name" => mapped.street = Some(component.value),
                    _ => {}
                }
            }
            let is = |context: &str| address.contexts.get(context).copied().unwrap_or(false);
            if is("work") && business_address.is_none() {
                business_address = Some(mapped);
            } else if is("private") && home_address.is_none() {
                home_address = Some(mapped);
            } else if is("other") && other_address.is_none() {
                other_address = Some(mapped);
            }
        }

        Self {
            id: value.id,
            address_book_ids: value
                .address_book_ids
                .into_iter()
                .filter_map(|(id, present)| present.then_some(id))
                .collect(),
            first_name,
            nickname,
            im_address1,
            im_address2,
            im_address3,
            middle_name,
            last_name,
            suffix,
            name_prefix,
            yomi_first_name,
            yomi_last_name,
            yomi_company_name,
            office_location,
            birthday,
            anniversary,
            web_page,
            file_as,
            emails,
            mobile_phone,
            home_phone: home_phones.first().cloned(),
            home_phone2: home_phones.get(1).cloned(),
            business_phone: business_phones.first().cloned(),
            business_phone2: business_phones.get(1).cloned(),
            home_fax,
            business_fax,
            pager,
            car_phone,
            radio_phone,
            assistant_phone,
            company_main_phone,
            mms,
            account_name,
            picture,
            notes,
            company_name,
            department,
            job_title,
            business_address,
            home_address,
            other_address,
            categories: value
                .keywords
                .into_iter()
                .filter_map(|(category, present)| present.then_some(category))
                .collect(),
        }
    }
}

/// JSCalendar (RFC 8984) event shape, as returned by Stalwart's
/// `CalendarEvent/get` -- confirmed live against a real created event.
/// `start` is LOCAL (no offset); `timeZone` (an IANA name, or absent for
/// a floating/already-UTC time) says what it's local TO. Converting that
/// pair to a real UTC instant needs the IANA tz database, hence the
/// `chrono-tz` dependency -- getting this wrong silently shows every
/// event at the wrong time, the same class of bug the mail DateReceived
/// fix this session spent a long time chasing down.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CalendarEventObject {
    id: String,
    #[serde(default)]
    uid: Option<String>,
    #[serde(rename = "@type", default)]
    kind: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    updated: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    description_content_type: Option<String>,
    #[serde(default)]
    start: Option<String>,
    #[serde(default)]
    time_zone: Option<String>,
    #[serde(default)]
    duration: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    locations: Option<BTreeMap<String, CalendarEventLocation>>,
    #[serde(default)]
    show_without_time: bool,
    #[serde(default)]
    free_busy_status: Option<String>,
    #[serde(default)]
    privacy: Option<String>,
    #[serde(default)]
    keywords: Option<BTreeMap<String, bool>>,
    #[serde(default)]
    alerts: Option<serde_json::Value>,
    #[serde(default)]
    use_default_alerts: bool,
    /// Participant map keyed by an arbitrary JMAP-assigned id (NOT the
    /// email) -- confirmed live 2026-08-25 alongside `reply_to`: Stalwart
    /// silently drops a `participants`-only create with no top-level
    /// `replyTo`, but round-trips both perfectly once `replyTo` is
    /// present. That earlier "blocked at the JMAP layer" verdict was
    /// wrong -- it was an incomplete write payload, not a missing
    /// capability. See `save_calendar_event` for the write side.
    #[serde(default)]
    participants: Option<BTreeMap<String, ParticipantObject>>,
    #[serde(default)]
    reply_to: Option<BTreeMap<String, String>>,
    #[serde(default)]
    recurrence_rule: Option<serde_json::Value>,
    #[serde(default)]
    recurrence_overrides: Option<serde_json::Value>,
    #[serde(default)]
    links: Option<BTreeMap<String, serde_json::Value>>,
    #[serde(default)]
    virtual_locations: Option<BTreeMap<String, serde_json::Value>>,
    #[serde(default)]
    i_calendar: Option<serde_json::Value>,
}

fn calendar_disallow_new_time_proposal(icalendar: Option<&serde_json::Value>) -> Option<bool> {
    let properties = icalendar?.get("properties")?.as_array()?;
    let mut matches = properties.iter().filter(|property| {
        property
            .as_array()
            .and_then(|property| property.first())
            .and_then(serde_json::Value::as_str)
            .is_some_and(|name| name.eq_ignore_ascii_case("x-microsoft-disallow-counter"))
    });
    let property = matches.next()?.as_array()?;
    if matches.next().is_some() {
        return None;
    }
    let value = property.get(3)?;
    value
        .as_bool()
        .or_else(|| match value.as_str()?.to_ascii_lowercase().as_str() {
            "true" | "1" => Some(true),
            "false" | "0" => Some(false),
            _ => None,
        })
}

fn calendar_icalendar_with_disallow_new_time_proposal(
    existing: Option<&serde_json::Value>,
    value: bool,
) -> anyhow::Result<serde_json::Value> {
    let mut icalendar = match existing {
        Some(value) if value.is_object() => value.clone(),
        Some(serde_json::Value::Null) | None => serde_json::json!({}),
        Some(_) => anyhow::bail!("invalid JMAP iCalendar extension"),
    };
    let properties = icalendar
        .as_object_mut()
        .context("invalid JMAP iCalendar extension")?
        .entry("properties")
        .or_insert_with(|| serde_json::json!([]));
    let properties = properties
        .as_array_mut()
        .context("invalid JMAP iCalendar properties")?;
    let mut matches = properties.iter_mut().filter(|property| {
        property
            .as_array()
            .and_then(|property| property.first())
            .and_then(serde_json::Value::as_str)
            .is_some_and(|name| name.eq_ignore_ascii_case("X-MICROSOFT-DISALLOW-COUNTER"))
    });
    if let Some(property) = matches.next() {
        anyhow::ensure!(
            matches.next().is_none(),
            "ambiguous duplicate DisallowNewTimeProposal properties"
        );
        let property = property
            .as_array_mut()
            .context("invalid DisallowNewTimeProposal property")?;
        anyhow::ensure!(
            property.len() >= 4,
            "incomplete DisallowNewTimeProposal property"
        );
        property[3] = serde_json::Value::Bool(value);
    } else {
        properties.push(serde_json::json!([
            "X-MICROSOFT-DISALLOW-COUNTER",
            {},
            "boolean",
            value
        ]));
    }
    Ok(icalendar)
}

#[derive(Debug, Default, Deserialize)]
struct CalendarEventLocation {
    name: Option<String>,
    coordinates: Option<String>,
}

/// One JSCalendar `Participant` (RFC 8984 §4.4.2). The organizer is the
/// participant whose `roles` includes `"owner"` -- real MS-ASCAL
/// (confirmed against the live spec's own worked example) never lists
/// the organizer as an Attendee at all, so `From<CalendarEventObject>`
/// splits this map into `organizer_email`/`organizer_name` (the `"owner"`
/// entry) plus `attendees` (everyone else) rather than passing it through
/// as one flat list.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ParticipantObject {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    roles: BTreeMap<String, bool>,
    #[serde(default)]
    participation_status: Option<String>,
    #[serde(default)]
    expect_reply: Option<bool>,
    #[serde(default)]
    schedule_updated: Option<String>,
}

fn calendar_appointment_reply_time(
    participants: Option<&BTreeMap<String, ParticipantObject>>,
    user: &str,
) -> Option<String> {
    let mut matches = participants?.values().filter(|participant| {
        participant.roles.get("attendee") == Some(&true)
            && participant
                .email
                .as_deref()
                .is_some_and(|email| email.eq_ignore_ascii_case(user))
    });
    let participant = matches.next()?;
    if matches.next().is_some() {
        return None;
    }
    participant
        .schedule_updated
        .as_deref()
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
        .map(|date| {
            date.with_timezone(&chrono::Utc)
                .format("%Y%m%dT%H%M%SZ")
                .to_string()
        })
}

fn calendar_response_requested(participants: &BTreeMap<String, ParticipantObject>) -> Option<bool> {
    let mut attendee_preferences = participants
        .values()
        .filter(|participant| {
            participant.roles.get("attendee") == Some(&true)
                && participant.roles.get("owner") != Some(&true)
        })
        .map(|participant| participant.expect_reply.unwrap_or(false));
    let first = attendee_preferences.next()?;
    attendee_preferences
        .all(|preference| preference == first)
        .then_some(first)
}

fn calendar_response_type(event: &CalendarEvent, user: &str) -> Option<u8> {
    if event.attendees.is_empty() || event.organizer_email.is_none() {
        return None;
    }
    let mut matches = event
        .attendees
        .iter()
        .filter(|attendee| attendee.email.eq_ignore_ascii_case(user));
    let attendee = matches.next();
    if matches.next().is_some() {
        return None;
    }
    if event
        .organizer_email
        .as_deref()
        .is_some_and(|email| email.eq_ignore_ascii_case(user))
    {
        return attendee.is_none().then_some(1);
    }
    let attendee = attendee?;
    match attendee.participation_status.as_deref() {
        Some("tentative") => Some(2),
        Some("accepted") => Some(3),
        Some("declined") => Some(4),
        Some("needs-action") | None => Some(5),
        _ => None,
    }
}

fn simple_geo(value: &str) -> Option<(&str, &str)> {
    let (latitude, longitude) = value.strip_prefix("geo:")?.split_once(',')?;
    let lat = latitude.parse::<f64>().ok()?;
    let lon = longitude.parse::<f64>().ok()?;
    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
        return None;
    }
    Some((latitude, longitude))
}

struct CalendarExceptionContext<'a> {
    base_alerts: Option<&'a serde_json::Value>,
    base_participants: Option<&'a BTreeMap<String, ParticipantObject>>,
    base_links: Option<&'a BTreeMap<String, serde_json::Value>>,
    base_virtual_locations: Option<&'a BTreeMap<String, serde_json::Value>>,
    current_user: Option<&'a str>,
    timezone: Option<&'a str>,
    event_duration: Option<&'a str>,
}

fn calendar_recurrence_exceptions(
    overrides: Option<&serde_json::Value>,
    context: CalendarExceptionContext<'_>,
) -> anyhow::Result<Vec<crate::model::CalendarEventException>> {
    let CalendarExceptionContext {
        base_alerts,
        base_participants,
        base_links,
        base_virtual_locations,
        current_user,
        timezone,
        event_duration,
    } = context;
    let Some(overrides) = overrides else {
        return Ok(Vec::new());
    };
    if overrides.is_null() {
        return Ok(Vec::new());
    }
    let overrides = overrides
        .as_object()
        .context("invalid recurrenceOverrides map")?;
    overrides
        .iter()
        .map(|(local_start, patch)| {
            let patch = patch
                .as_object()
                .context("invalid recurrence exception patch")?;
            let instance_id = local_to_utc_eas(local_start, timezone)
                .context("invalid recurrence exception local start")?;
            if patch.get("excluded") == Some(&serde_json::Value::Bool(true)) {
                anyhow::ensure!(patch.len() == 1, "excluded exception has extra properties");
                return Ok(crate::model::CalendarEventException {
                    instance_id,
                    deleted: true,
                    ..Default::default()
                });
            }
            anyhow::ensure!(
                patch.keys().all(|key| {
                    matches!(
                        key.as_str(),
                        "title"
                            | "description"
                            | "descriptionContentType"
                            | "locations"
                            | "start"
                            | "duration"
                            | "showWithoutTime"
                            | "freeBusyStatus"
                            | "keywords"
                            | "alerts"
                            | "participants"
                            | "links"
                            | "virtualLocations"
                    ) || key.starts_with("alerts/")
                        || key.starts_with("participants/")
                        || key.starts_with("links/")
                        || key.starts_with("virtualLocations/")
                }),
                "unsupported recurrence exception fields: {:?}",
                patch.keys().collect::<Vec<_>>()
            );
            let title = patch
                .get("title")
                .map(|value| {
                    value
                        .as_str()
                        .context("invalid exception title")
                        .map(str::to_owned)
                })
                .transpose()?;
            anyhow::ensure!(!patch.is_empty(), "empty recurrence exception patch");
            let description = patch
                .get("description")
                .map(|value| value.as_str().context("invalid exception description"))
                .transpose()?;
            let description_content_type = patch
                .get("descriptionContentType")
                .map(|value| value.as_str().context("invalid exception description type"))
                .transpose()?;
            anyhow::ensure!(
                description_content_type.is_none_or(|content_type| {
                    content_type.eq_ignore_ascii_case("text/plain")
                        || content_type.eq_ignore_ascii_case("text/html")
                }),
                "unsupported exception description type"
            );
            anyhow::ensure!(
                description.is_some() || description_content_type.is_none(),
                "exception description type has no description"
            );
            let description = description.map(|value| crate::model::EmailBody {
                body_type: if description_content_type
                    .is_some_and(|content_type| content_type.eq_ignore_ascii_case("text/html"))
                {
                    crate::model::EmailBodyType::Html
                } else {
                    crate::model::EmailBodyType::Plain
                },
                value: value.to_owned(),
                is_truncated: false,
            });
            let (location, location_coordinates) = match patch.get("locations") {
                None => (None, None),
                Some(serde_json::Value::Null) => (Some(String::new()), None),
                Some(locations) => {
                    let locations = locations
                        .as_object()
                        .context("invalid exception locations")?;
                    anyhow::ensure!(
                        locations.len() <= 1,
                        "multiple exception locations are unsupported"
                    );
                    if let Some(value) = locations.values().next() {
                        let value: CalendarEventLocation = serde_json::from_value(value.clone())
                            .context("invalid exception location")?;
                        let coordinates = value
                            .coordinates
                            .as_deref()
                            .and_then(simple_geo)
                            .map(|(lat, lon)| (lat.to_owned(), lon.to_owned()));
                        anyhow::ensure!(
                            value.coordinates.is_none() || coordinates.is_some(),
                            "invalid exception location coordinates"
                        );
                        (value.name, coordinates)
                    } else {
                        (Some(String::new()), None)
                    }
                }
            };
            let start_local = patch
                .get("start")
                .map(|value| value.as_str().context("invalid exception start"))
                .transpose()?;
            let duration = patch
                .get("duration")
                .map(|value| value.as_str().context("invalid exception duration"))
                .transpose()?;
            let start_utc = start_local
                .map(|start| local_to_utc_eas(start, timezone).context("invalid exception start"))
                .transpose()?;
            let end_utc = if start_local.is_some() || duration.is_some() {
                let start = start_local.unwrap_or(local_start);
                Some(
                    calendar_end_utc(
                        start,
                        timezone,
                        duration.or(event_duration).unwrap_or("PT0S"),
                    )
                    .context("invalid exception end")?,
                )
            } else {
                None
            };
            let all_day = patch
                .get("showWithoutTime")
                .map(|value| value.as_bool().context("invalid exception all-day flag"))
                .transpose()?;
            let busy_status = match patch.get("freeBusyStatus") {
                None => None,
                Some(serde_json::Value::Null) => Some(2),
                Some(value) => Some(
                    match value.as_str().context("invalid exception availability")? {
                        "free" => 0,
                        "x-eas-tentative" => 1,
                        "busy" => 2,
                        "x-eas-out-of-office" => 3,
                        "x-eas-working-elsewhere" => 4,
                        _ => anyhow::bail!("unsupported exception availability"),
                    },
                ),
            };
            let categories = match patch.get("keywords") {
                None => None,
                Some(serde_json::Value::Null) => Some(Vec::new()),
                Some(value) => {
                    let keywords: BTreeMap<String, bool> = serde_json::from_value(value.clone())
                        .context("invalid exception keywords")?;
                    Some(
                        keywords
                            .into_iter()
                            .filter_map(|(name, present)| present.then_some(name))
                            .collect(),
                    )
                }
            };
            let has_alert_patch = patch
                .keys()
                .any(|key| key == "alerts" || key.starts_with("alerts/"));
            let reminder_minutes = if has_alert_patch {
                let alerts = merge_calendar_alert_patch(base_alerts, patch)?;
                let inherited = base_alerts
                    .map(crate::jmap::calendar_reminder::read_reminder)
                    .unwrap_or(None);
                let effective = crate::jmap::calendar_reminder::read_reminder(&alerts);
                (effective != inherited).then_some(effective)
            } else {
                None
            };
            let has_participant_patch = patch
                .keys()
                .any(|key| key == "participants" || key.starts_with("participants/"));
            let (attendees, appointment_reply_time) = if has_participant_patch {
                let (attendees, reply_time) = calendar_recurrence_exception_attendees(
                    base_participants,
                    patch,
                    current_user,
                )?;
                (Some(attendees), reply_time)
            } else {
                (None, None)
            };
            let has_link_patch = patch
                .keys()
                .any(|key| key == "links" || key.starts_with("links/"));
            let attachments = if has_link_patch {
                Some(calendar_recurrence_exception_attachments(
                    base_links, patch,
                )?)
            } else {
                None
            };
            let has_virtual_location_patch = patch
                .keys()
                .any(|key| key == "virtualLocations" || key.starts_with("virtualLocations/"));
            let (online_meeting_conf_link, online_meeting_external_link) =
                if has_virtual_location_patch {
                    let base_links = calendar_online_meeting_links(base_virtual_locations);
                    let virtual_locations = calendar_recurrence_exception_virtual_locations(
                        base_virtual_locations,
                        patch,
                    )?;
                    let updated_links = calendar_online_meeting_links(Some(&virtual_locations));
                    (
                        (updated_links.0 != base_links.0)
                            .then(|| updated_links.0.unwrap_or_default()),
                        (updated_links.1 != base_links.1)
                            .then(|| updated_links.1.unwrap_or_default()),
                    )
                } else {
                    (None, None)
                };
            Ok(crate::model::CalendarEventException {
                instance_id,
                deleted: false,
                title,
                location,
                location_coordinates,
                start_utc,
                end_utc,
                all_day,
                busy_status,
                categories,
                attendees,
                attachments,
                reminder_minutes,
                description,
                online_meeting_conf_link,
                online_meeting_external_link,
                appointment_reply_time,
            })
        })
        .collect()
}

fn calendar_recurrence_exception_virtual_locations(
    base: Option<&BTreeMap<String, serde_json::Value>>,
    override_patch: &serde_json::Map<String, serde_json::Value>,
) -> anyhow::Result<BTreeMap<String, serde_json::Value>> {
    fn unescape(token: &str) -> anyhow::Result<String> {
        let mut result = String::new();
        let mut chars = token.chars();
        while let Some(character) = chars.next() {
            if character == '~' {
                match chars
                    .next()
                    .context("invalid escaped recurrence virtual location path")?
                {
                    '0' => result.push('~'),
                    '1' => result.push('/'),
                    _ => anyhow::bail!("invalid escaped recurrence virtual location path"),
                }
            } else {
                result.push(character);
            }
        }
        Ok(result)
    }

    let mut locations = base.cloned().unwrap_or_default();
    if let Some(replacement) = override_patch.get("virtualLocations") {
        anyhow::ensure!(
            !override_patch
                .keys()
                .any(|key| key.starts_with("virtualLocations/")),
            "overlapping recurrence virtual location patches"
        );
        locations = if replacement.is_null() {
            BTreeMap::new()
        } else {
            serde_json::from_value(replacement.clone())
                .context("invalid recurrence virtual locations replacement")?
        };
    } else {
        for (path, value) in override_patch.iter().filter_map(|(key, value)| {
            key.strip_prefix("virtualLocations/")
                .map(|path| (path, value))
        }) {
            let tokens = path
                .split('/')
                .map(unescape)
                .collect::<anyhow::Result<Vec<_>>>()?;
            anyhow::ensure!(
                !tokens.is_empty(),
                "empty recurrence virtual location patch path"
            );
            let (last, parents) = tokens.split_last().unwrap();
            let mut root = serde_json::to_value(locations)?;
            let mut target = &mut root;
            for token in parents {
                target = target
                    .as_object_mut()
                    .and_then(|object| object.get_mut(token))
                    .context("recurrence virtual location patch parent is missing")?;
            }
            let object = target
                .as_object_mut()
                .context("recurrence virtual location patch parent is not an object")?;
            if value.is_null() {
                object.remove(last);
            } else {
                object.insert(last.clone(), value.clone());
            }
            locations =
                serde_json::from_value(root).context("invalid recurrence virtual locations map")?;
        }
    }
    Ok(locations)
}

fn calendar_online_meeting_links(
    locations: Option<&BTreeMap<String, serde_json::Value>>,
) -> (Option<String>, Option<String>) {
    let Some(locations) = locations else {
        return (None, None);
    };
    fn uri(value: &serde_json::Value) -> Option<&str> {
        value.get("uri").and_then(serde_json::Value::as_str)
    }
    let conference = locations
        .get("eas_meeting_conf")
        .and_then(uri)
        .or_else(|| {
            locations.values().filter_map(uri).find(|uri| {
                uri.split_once(':').is_some_and(|(scheme, _)| {
                    scheme.eq_ignore_ascii_case("sip") || scheme.eq_ignore_ascii_case("sips")
                })
            })
        })
        .map(str::to_owned);
    let external = locations
        .get("eas_meeting_external")
        .and_then(uri)
        .or_else(|| {
            locations.values().filter_map(uri).find(|uri| {
                url::Url::parse(uri).is_ok_and(|parsed| matches!(parsed.scheme(), "http" | "https"))
            })
        })
        .map(str::to_owned);
    (conference, external)
}

fn calendar_recurrence_exception_attendees(
    base: Option<&BTreeMap<String, ParticipantObject>>,
    override_patch: &serde_json::Map<String, serde_json::Value>,
    current_user: Option<&str>,
) -> anyhow::Result<(Vec<crate::model::Attendee>, Option<String>)> {
    fn unescape(token: &str) -> anyhow::Result<String> {
        let mut result = String::new();
        let mut chars = token.chars();
        while let Some(character) = chars.next() {
            if character == '~' {
                match chars
                    .next()
                    .context("invalid escaped recurrence participant path")?
                {
                    '0' => result.push('~'),
                    '1' => result.push('/'),
                    _ => anyhow::bail!("invalid escaped recurrence participant path"),
                }
            } else {
                result.push(character);
            }
        }
        Ok(result)
    }

    let mut participants = serde_json::Map::new();
    if let Some(base) = base {
        for (id, participant) in base {
            let mut value = serde_json::Map::new();
            if let Some(email) = &participant.email {
                value.insert("email".to_owned(), serde_json::json!(email));
            }
            if let Some(name) = &participant.name {
                value.insert("name".to_owned(), serde_json::json!(name));
            }
            if !participant.roles.is_empty() {
                value.insert("roles".to_owned(), serde_json::json!(participant.roles));
            }
            if let Some(status) = &participant.participation_status {
                value.insert("participationStatus".to_owned(), serde_json::json!(status));
            }
            if let Some(schedule_updated) = &participant.schedule_updated {
                value.insert(
                    "scheduleUpdated".to_owned(),
                    serde_json::json!(schedule_updated),
                );
            }
            participants.insert(id.clone(), serde_json::Value::Object(value));
        }
    }
    if let Some(replacement) = override_patch.get("participants") {
        anyhow::ensure!(
            !override_patch
                .keys()
                .any(|key| key.starts_with("participants/")),
            "overlapping recurrence participant patches"
        );
        participants = if replacement.is_null() {
            serde_json::Map::new()
        } else {
            replacement
                .as_object()
                .context("invalid recurrence participants replacement")?
                .clone()
        };
    } else {
        for (path, value) in override_patch
            .iter()
            .filter_map(|(key, value)| key.strip_prefix("participants/").map(|path| (path, value)))
        {
            let tokens = path
                .split('/')
                .map(unescape)
                .collect::<anyhow::Result<Vec<_>>>()?;
            anyhow::ensure!(
                !tokens.is_empty(),
                "empty recurrence participant patch path"
            );
            let (last, parents) = tokens.split_last().unwrap();
            let mut root = serde_json::Value::Object(participants);
            let mut target = &mut root;
            for token in parents {
                target = target
                    .as_object_mut()
                    .and_then(|object| object.get_mut(token))
                    .context("recurrence participant patch parent is missing")?;
            }
            let object = target
                .as_object_mut()
                .context("recurrence participant patch parent is not an object")?;
            if value.is_null() {
                object.remove(last);
            } else {
                object.insert(last.clone(), value.clone());
            }
            participants = root
                .as_object()
                .cloned()
                .context("invalid recurrence participants map")?;
        }
    }

    let appointment_reply_time = current_user.and_then(|user| {
        let typed: BTreeMap<String, ParticipantObject> =
            serde_json::from_value(serde_json::Value::Object(participants.clone())).ok()?;
        calendar_appointment_reply_time(Some(&typed), user)
    });

    let attendees = participants
        .into_values()
        .filter_map(|value| {
            let roles = value.get("roles")?.as_object()?;
            if roles.get("owner").and_then(serde_json::Value::as_bool) == Some(true)
                || roles.get("attendee").and_then(serde_json::Value::as_bool) != Some(true)
            {
                return None;
            }
            let roles = roles.clone();
            Some((value, roles))
        })
        .map(|(value, roles)| {
            let email = value
                .get("email")
                .and_then(serde_json::Value::as_str)
                .filter(|email| !email.is_empty())
                .context("recurrence attendee email missing")?
                .to_owned();
            let participation_status = match value
                .get("participationStatus")
                .and_then(serde_json::Value::as_str)
            {
                None | Some("needs-action") => Some("needs-action".to_owned()),
                Some("accepted") => Some("accepted".to_owned()),
                Some("declined") => Some("declined".to_owned()),
                Some("tentative") => Some("tentative".to_owned()),
                Some(_) => anyhow::bail!("unsupported recurrence attendee status"),
            };
            Ok(crate::model::Attendee {
                email,
                name: value
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
                participation_status,
                optional: roles.get("optional").and_then(serde_json::Value::as_bool) == Some(true),
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    Ok((attendees, appointment_reply_time))
}

fn calendar_recurrence_exception_attachments(
    base: Option<&BTreeMap<String, serde_json::Value>>,
    override_patch: &serde_json::Map<String, serde_json::Value>,
) -> anyhow::Result<Vec<crate::model::CalendarAttachment>> {
    fn unescape(token: &str) -> anyhow::Result<String> {
        let mut result = String::new();
        let mut chars = token.chars();
        while let Some(character) = chars.next() {
            if character == '~' {
                match chars
                    .next()
                    .context("invalid escaped recurrence link path")?
                {
                    '0' => result.push('~'),
                    '1' => result.push('/'),
                    _ => anyhow::bail!("invalid escaped recurrence link path"),
                }
            } else {
                result.push(character);
            }
        }
        Ok(result)
    }

    let mut links = base.cloned().unwrap_or_default();
    if let Some(replacement) = override_patch.get("links") {
        anyhow::ensure!(
            !override_patch.keys().any(|key| key.starts_with("links/")),
            "overlapping recurrence link patches"
        );
        links = if replacement.is_null() {
            BTreeMap::new()
        } else {
            serde_json::from_value(replacement.clone())
                .context("invalid recurrence links replacement")?
        };
    } else {
        for (path, value) in override_patch
            .iter()
            .filter_map(|(key, value)| key.strip_prefix("links/").map(|path| (path, value)))
        {
            let tokens = path
                .split('/')
                .map(unescape)
                .collect::<anyhow::Result<Vec<_>>>()?;
            anyhow::ensure!(!tokens.is_empty(), "empty recurrence link patch path");
            let (last, parents) = tokens.split_last().unwrap();
            let mut root = serde_json::to_value(links)?;
            let mut target = &mut root;
            for token in parents {
                target = target
                    .as_object_mut()
                    .and_then(|object| object.get_mut(token))
                    .context("recurrence link patch parent is missing")?;
            }
            let object = target
                .as_object_mut()
                .context("recurrence link patch parent is not an object")?;
            if value.is_null() {
                object.remove(last);
            } else {
                object.insert(last.clone(), value.clone());
            }
            links = serde_json::from_value(root).context("invalid recurrence links map")?;
        }
    }

    links
        .into_values()
        .filter(|link| link.get("rel").and_then(serde_json::Value::as_str) == Some("enclosure"))
        .map(|link| {
            let blob_id = link
                .get("blobId")
                .and_then(serde_json::Value::as_str)
                .filter(|id| !id.is_empty() && !id.contains("||"))
                .context("calendar enclosure has no usable JMAP blobId")?
                .to_owned();
            Ok(crate::model::CalendarAttachment {
                blob_id,
                name: link
                    .get("title")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
                content_type: link
                    .get("contentType")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
                size: link.get("size").and_then(serde_json::Value::as_u64),
                content_id: link
                    .get("cid")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
                content_location: link
                    .get("href")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
            })
        })
        .collect()
}

fn merge_calendar_alert_patch(
    base: Option<&serde_json::Value>,
    override_patch: &serde_json::Map<String, serde_json::Value>,
) -> anyhow::Result<serde_json::Value> {
    fn unescape(token: &str) -> anyhow::Result<String> {
        let mut result = String::new();
        let mut chars = token.chars();
        while let Some(character) = chars.next() {
            if character == '~' {
                match chars
                    .next()
                    .context("invalid escaped recurrence alert path")?
                {
                    '0' => result.push('~'),
                    '1' => result.push('/'),
                    _ => anyhow::bail!("invalid escaped recurrence alert path"),
                }
            } else {
                result.push(character);
            }
        }
        Ok(result)
    }

    let mut alerts = base
        .filter(|value| !value.is_null())
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    if let Some(replacement) = override_patch.get("alerts") {
        anyhow::ensure!(
            !override_patch.keys().any(|key| key.starts_with("alerts/")),
            "overlapping recurrence alert patches"
        );
        return if replacement.is_null() {
            Ok(serde_json::json!({}))
        } else {
            anyhow::ensure!(
                replacement.is_object(),
                "invalid recurrence alerts replacement"
            );
            Ok(replacement.clone())
        };
    }
    for (path, value) in override_patch
        .iter()
        .filter_map(|(key, value)| key.strip_prefix("alerts/").map(|path| (path, value)))
    {
        let tokens = path
            .split('/')
            .map(unescape)
            .collect::<anyhow::Result<Vec<_>>>()?;
        anyhow::ensure!(!tokens.is_empty(), "empty recurrence alert patch path");
        let (last, parents) = tokens.split_last().unwrap();
        let mut target = &mut alerts;
        for token in parents {
            target = target
                .as_object_mut()
                .and_then(|object| object.get_mut(token))
                .context("recurrence alert patch parent is missing")?;
        }
        let object = target
            .as_object_mut()
            .context("recurrence alert patch parent is not an object")?;
        if value.is_null() {
            object.remove(last);
        } else {
            object.insert(last.clone(), value.clone());
        }
    }
    Ok(alerts)
}

impl From<CalendarEventObject> for CalendarEvent {
    fn from(value: CalendarEventObject) -> Self {
        calendar_event_from(value, None)
    }
}

fn calendar_event_from(value: CalendarEventObject, current_user: Option<&str>) -> CalendarEvent {
    let appointment_reply_time = current_user
        .and_then(|user| calendar_appointment_reply_time(value.participants.as_ref(), user));
    let disallow_new_time_proposal = calendar_disallow_new_time_proposal(value.i_calendar.as_ref());
    let response_requested = value
        .participants
        .as_ref()
        .and_then(calendar_response_requested);
    let recurrence_rule = value.recurrence_rule;
    let recurrence_time_zone = value.time_zone.clone();
    let recurrence_exceptions = calendar_recurrence_exceptions(
        value.recurrence_overrides.as_ref(),
        CalendarExceptionContext {
            base_alerts: value.alerts.as_ref(),
            base_participants: value.participants.as_ref(),
            base_links: value.links.as_ref(),
            base_virtual_locations: value.virtual_locations.as_ref(),
            current_user,
            timezone: recurrence_time_zone.as_deref(),
            event_duration: value.duration.as_deref(),
        },
    );
    let recurrence_overrides_unsupported = recurrence_exceptions.is_err();
    let recurrence_exceptions = recurrence_exceptions.unwrap_or_default();
    let exception_uid_missing = !recurrence_exceptions.is_empty() && value.uid.is_none();
    let exception_without_rule = !recurrence_exceptions.is_empty()
        && recurrence_rule
            .as_ref()
            .is_none_or(serde_json::Value::is_null);
    let recurrence = (!recurrence_overrides_unsupported)
        .then(|| {
            recurrence_rule.as_ref().and_then(|rule| {
                crate::jmap::calendar_recurrence::from_jscalendar(
                    rule,
                    recurrence_time_zone.as_deref(),
                )
                .ok()
            })
        })
        .flatten();
    let unsupported_recurrence = recurrence_overrides_unsupported
        || exception_uid_missing
        || exception_without_rule
        || (recurrence_rule.is_some() && recurrence.is_none());
    let start_utc = value
        .start
        .as_deref()
        .and_then(|start| local_to_utc_eas(start, value.time_zone.as_deref()));
    let end_utc = value.start.as_deref().and_then(|start| {
        calendar_end_utc(
            start,
            value.time_zone.as_deref(),
            value.duration.as_deref().unwrap_or("PT0S"),
        )
    });
    let selected_location = value.locations.as_ref().and_then(|locations| {
        locations
            .values()
            .find(|location| location.name.is_some())
            .or_else(|| {
                locations
                    .values()
                    .find(|location| location.coordinates.is_some())
            })
    });
    let location = selected_location.and_then(|location| location.name.clone());
    let location_coordinates = selected_location
        .and_then(|location| simple_geo(location.coordinates.as_deref()?))
        .map(|(latitude, longitude)| (latitude.to_owned(), longitude.to_owned()));
    let (online_meeting_conf_link, online_meeting_external_link) =
        calendar_online_meeting_links(value.virtual_locations.as_ref());

    let participants: Vec<_> = value
        .participants
        .into_iter()
        .flatten()
        .map(|(_, participant)| participant)
        .collect();
    let reply_address = value
        .reply_to
        .as_ref()
        .and_then(|reply| reply.get("imip"))
        .and_then(|uri| {
            uri.get(..7)
                .filter(|prefix| prefix.eq_ignore_ascii_case("mailto:"))?;
            uri.get(7..)
        });
    let has_owner = participants
        .iter()
        .any(|participant| participant.roles.get("owner") == Some(&true));
    let reply_matches: Vec<_> = participants
        .iter()
        .enumerate()
        .filter(|(_, participant)| {
            (!has_owner || participant.roles.get("owner") == Some(&true))
                && participant
                    .email
                    .as_deref()
                    .zip(reply_address)
                    .is_some_and(|(email, reply)| email.eq_ignore_ascii_case(reply))
        })
        .map(|(index, _)| index)
        .collect();
    let owners: Vec<_> = participants
        .iter()
        .enumerate()
        .filter(|(_, participant)| participant.roles.get("owner") == Some(&true))
        .map(|(index, _)| index)
        .collect();
    let organizer = match reply_matches.as_slice() {
        [index] => Some(*index),
        [] => match owners.as_slice() {
            [index] => Some(*index),
            _ => None,
        },
        _ => None,
    };
    let mut organizer_email = None;
    let mut organizer_name = None;
    let mut attendees = Vec::new();
    for (index, participant) in participants.into_iter().enumerate() {
        let Some(email) = participant.email else {
            continue;
        };
        if Some(index) == organizer {
            organizer_email = Some(email);
            organizer_name = participant.name;
        } else {
            attendees.push(crate::model::Attendee {
                email,
                name: participant.name,
                participation_status: participant.participation_status,
                optional: participant.roles.get("optional").copied().unwrap_or(false),
            });
        }
    }

    CalendarEvent {
        id: value.id,
        uid: value.uid,
        calendar_ids: Vec::new(),
        title: value.title,
        updated_utc: value
            .updated
            .as_deref()
            .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
            .map(|date| {
                date.with_timezone(&chrono::Utc)
                    .format("%Y%m%dT%H%M%SZ")
                    .to_string()
            }),
        description: value.description.map(|description| {
            let content_type = value
                .description_content_type
                .as_deref()
                .unwrap_or("text/plain")
                .split(';')
                .next()
                .unwrap_or("")
                .trim();
            let body_type = if content_type.eq_ignore_ascii_case("text/html") {
                EmailBodyType::Html
            } else {
                EmailBodyType::Plain
            };
            EmailBody {
                body_type,
                value: description,
                is_truncated: false,
            }
        }),
        location,
        location_coordinates,
        start_utc,
        end_utc,
        time_zone: value.time_zone,
        all_day: value.show_without_time,
        busy_status: Some(match value.free_busy_status.as_deref() {
            Some("free") => 0,
            Some("x-eas-tentative") => 1,
            Some("x-eas-out-of-office") => 3,
            Some("x-eas-working-elsewhere") => 4,
            _ => 2,
        }),
        sensitivity: match value.privacy.as_deref() {
            None | Some("public") => 0,
            Some("private") => 2,
            _ => 3,
        },
        categories: value
            .keywords
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(name, present)| present.then_some(name))
            .collect(),
        recurrence,
        recurrence_exceptions,
        unsupported_recurrence,
        reminder_minutes: value
            .alerts
            .as_ref()
            .and_then(crate::jmap::calendar_reminder::read_reminder),
        organizer_email,
        organizer_name,
        attendees,
        is_organizer: false,
        cancelled: value.status.as_deref() == Some("cancelled"),
        response_type: None,
        response_requested,
        appointment_reply_time,
        disallow_new_time_proposal,
        online_meeting_conf_link,
        online_meeting_external_link,
    }
}

/// Same underlying JMAP object as `CalendarEventObject` (Task rides on
/// `CalendarEvent`, see `tasks_in_calendar`'s module doc) but only the
/// fields Tasks actually needs, plus `@type` so the caller can filter out
/// plain Events -- there is no server-side query filter for this (checked
/// live: `unsupportedFilter`).
#[derive(Debug, Default, Deserialize)]
struct TaskObject {
    id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default, rename = "descriptionContentType")]
    description_content_type: Option<String>,
    #[serde(default)]
    start: Option<String>,
    #[serde(default)]
    due: Option<String>,
    #[serde(default, rename = "timeZone")]
    time_zone: Option<String>,
    #[serde(default)]
    progress: Option<String>,
    #[serde(default, rename = "progressUpdated")]
    progress_updated: Option<String>,
    #[serde(default)]
    priority: Option<u8>,
    #[serde(default)]
    privacy: Option<String>,
    #[serde(default)]
    keywords: std::collections::BTreeMap<String, bool>,
    #[serde(default)]
    alerts: Option<serde_json::Value>,
    #[serde(default, rename = "useDefaultAlerts")]
    use_default_alerts: bool,
    #[serde(default, rename = "recurrenceRule")]
    recurrence_rule: Option<serde_json::Value>,
    #[serde(default, rename = "recurrenceOverrides")]
    recurrence_overrides: Option<serde_json::Value>,
    #[serde(rename = "@type", default)]
    kind: String,
}

fn try_task_from_object(value: TaskObject) -> anyhow::Result<Task> {
    if let Some(overrides) = value.recurrence_overrides.as_ref() {
        anyhow::ensure!(
            overrides.as_object().is_none_or(serde_json::Map::is_empty),
            "task recurrence exceptions are unsupported"
        );
    }
    let recurrence = value
        .recurrence_rule
        .as_ref()
        .filter(|rule| !rule.is_null())
        .map(|rule| {
            crate::jmap::calendar_recurrence::from_jscalendar(rule, value.time_zone.as_deref())
        })
        .transpose()?;
    anyhow::ensure!(
        recurrence
            .as_ref()
            .and_then(|recurrence| recurrence.occurrences)
            .is_none_or(|occurrences| occurrences <= u8::MAX as u32),
        "Task recurrence Occurrences exceeds MS-ASTASK unsignedByte range"
    );
    Ok(Task {
        id: value.id,
        title: value.title,
        description: value.description.map(|description| EmailBody {
            body_type: if value.description_content_type.as_deref() == Some("text/html") {
                EmailBodyType::Html
            } else {
                EmailBodyType::Plain
            },
            value: description,
            is_truncated: false,
        }),
        completed: value.progress.as_deref() == Some("completed"),
        date_completed: (value.progress.as_deref() == Some("completed"))
            .then(|| {
                value
                    .progress_updated
                    .as_deref()
                    .and_then(utc_iso_to_eas_datetime)
            })
            .flatten(),
        importance: jmap_task_priority_to_eas(value.priority),
        sensitivity: jmap_task_sensitivity_to_eas(value.privacy.as_deref()),
        categories: value
            .keywords
            .into_iter()
            .filter_map(|(category, enabled)| enabled.then_some(category))
            .collect(),
        reminder_time: (!value.use_default_alerts)
            .then(|| {
                value
                    .alerts
                    .as_ref()
                    .and_then(crate::jmap::task_reminder::read_reminder)
            })
            .flatten(),
        start: value.start.as_deref().and_then(|start| {
            if value
                .time_zone
                .as_deref()
                .is_some_and(|zone| zone.parse::<chrono_tz::Tz>().is_err())
            {
                return None;
            }
            local_to_utc_eas(start, value.time_zone.as_deref())
        }),
        start_local: value.start.as_deref().and_then(local_to_utc_eas_no_tz),
        due: value.due.as_deref().and_then(|due| {
            if value
                .time_zone
                .as_deref()
                .is_some_and(|zone| zone.parse::<chrono_tz::Tz>().is_err())
            {
                return None;
            }
            local_to_utc_eas(due, value.time_zone.as_deref())
        }),
        due_local: value.due.as_deref().and_then(local_to_utc_eas_no_tz),
        recurrence,
    })
}

fn eas_importance_to_jmap(importance: &str) -> Option<u8> {
    match importance {
        "0" => Some(9),
        "1" => Some(5),
        "2" => Some(1),
        _ => None,
    }
}

fn eas_task_sensitivity_to_jmap(sensitivity: &str) -> Option<&'static str> {
    match sensitivity {
        "0" => Some("public"),
        "1" | "2" => Some("private"),
        "3" => Some("secret"),
        _ => None,
    }
}

fn jmap_task_sensitivity_to_eas(privacy: Option<&str>) -> u8 {
    match privacy {
        None | Some("public") => 0,
        Some("secret") => 3,
        Some("private") | Some(_) => 2,
    }
}

fn jmap_task_priority_to_eas(priority: Option<u8>) -> Option<u8> {
    match priority? {
        1..=4 => Some(2),
        5 => Some(1),
        6..=9 => Some(0),
        _ => None,
    }
}

// Format the local wall time without applying a timezone offset.
fn local_to_utc_eas_no_tz(local_iso: &str) -> Option<String> {
    local_to_utc_eas(local_iso, None)
}

fn utc_iso_to_eas_datetime(value: &str) -> Option<String> {
    let parsed = chrono::DateTime::parse_from_rfc3339(value)
        .ok()?
        .with_timezone(&chrono::Utc);
    Some(parsed.format("%Y-%m-%dT%H:%M:%S.000Z").to_string())
}

/// Converts a JSCalendar LocalDateTime (`2026-08-25T14:00:00`, no offset)
/// plus its IANA `timeZone` (`"America/Toronto"`) into a UTC instant,
/// formatted as compact EAS DateTime. A missing/unparseable timeZone is
/// treated as already-UTC (JSCalendar's own convention for a floating/
/// UTC time), not silently dropped to a wrong instant.
fn local_to_utc_eas(local_iso: &str, timezone: Option<&str>) -> Option<String> {
    use chrono::TimeZone;

    let naive = chrono::NaiveDateTime::parse_from_str(local_iso, "%Y-%m-%dT%H:%M:%S").ok()?;
    let utc: chrono::DateTime<chrono::Utc> =
        match timezone.and_then(|tz| tz.parse::<chrono_tz::Tz>().ok()) {
            Some(tz) => {
                let local = tz.from_local_datetime(&naive).earliest();
                match local {
                    Some(local) => local.with_timezone(&chrono::Utc),
                    None => {
                        use chrono::Offset;
                        let offset = (1..=2880).find_map(|minutes| {
                            let earlier =
                                naive.checked_sub_signed(chrono::Duration::minutes(minutes))?;
                            tz.from_local_datetime(&earlier)
                                .earliest()
                                .map(|time| time.offset().fix())
                        })?;
                        offset
                            .from_local_datetime(&naive)
                            .single()?
                            .with_timezone(&chrono::Utc)
                    }
                }
            }
            None => chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(naive, chrono::Utc),
        };
    Some(utc.format("%Y%m%dT%H%M%SZ").to_string())
}

/// Convert an EAS instant to the event's local time only when it round-trips.
fn eas_to_calendar_local(compact: &str, timezone: Option<&str>) -> Option<chrono::NaiveDateTime> {
    let utc = chrono::NaiveDateTime::parse_from_str(compact, "%Y%m%dT%H%M%SZ")
        .ok()?
        .and_utc();
    let local = match timezone {
        Some(timezone) => utc
            .with_timezone(&timezone.parse::<chrono_tz::Tz>().ok()?)
            .naive_local(),
        None => utc.naive_utc(),
    };
    (local_to_utc_eas(&local.format("%Y-%m-%dT%H:%M:%S").to_string(), timezone).as_deref()
        == Some(compact))
    .then_some(local)
}

fn recurrence_override_key(instance_id: &str, timezone: Option<&str>) -> anyhow::Result<String> {
    let instant = chrono::DateTime::parse_from_rfc3339(instance_id)
        .context("invalid ActiveSync InstanceId")?
        .with_timezone(&chrono::Utc);
    let local = match timezone {
        Some(timezone) => {
            let timezone = timezone
                .parse::<chrono_tz::Tz>()
                .context("invalid calendar time zone")?;
            instant.with_timezone(&timezone).naive_local()
        }
        None => instant.naive_utc(),
    };
    let key = local.format("%Y-%m-%dT%H:%M:%S").to_string();
    Ok(key.replace('~', "~0").replace('/', "~1"))
}

fn eas_compact_to_local_iso(compact: &str) -> Option<String> {
    let parsed = chrono::NaiveDateTime::parse_from_str(compact, "%Y%m%dT%H%M%SZ").ok()?;
    Some(parsed.format("%Y-%m-%dT%H:%M:%S").to_string())
}

// Days and weeks are calendar durations; hours, minutes and seconds are elapsed time.
fn parse_calendar_duration(duration: &str) -> Option<(i64, i64)> {
    let rest = duration.strip_prefix('P')?;
    let (date, time) = rest
        .split_once('T')
        .map_or((rest, None), |(d, t)| (d, Some(t)));
    let (weeks, day_part) = date.split_once('W').map_or((0, date), |(weeks, days)| {
        (weeks.parse::<i64>().unwrap_or(-1), days)
    });
    let days = if day_part.is_empty() {
        0
    } else {
        day_part.strip_suffix('D')?.parse::<i64>().ok()?
    };
    if days < 0 {
        return None;
    }
    let days = weeks.checked_mul(7)?.checked_add(days)?;
    if weeks < 0 {
        return None;
    }
    let mut seconds = 0_i64;
    let mut remainder = time.unwrap_or("");
    let mut has_time = false;
    for (unit, scale) in [('H', 3600), ('M', 60), ('S', 1)] {
        if let Some((number, rest)) = remainder.split_once(unit) {
            let value = number.parse::<i64>().ok()?;
            if value < 0 {
                return None;
            }
            seconds = seconds.checked_add(value.checked_mul(scale)?)?;
            remainder = rest;
            has_time = true;
        }
    }
    if days < 0
        || !remainder.is_empty()
        || (date.is_empty() && !has_time)
        || (time.is_some() && !has_time)
    {
        return None;
    }
    Some((days, seconds))
}

fn calendar_end_utc(start: &str, timezone: Option<&str>, duration: &str) -> Option<String> {
    let (days, seconds) = parse_calendar_duration(duration)?;
    let local = chrono::NaiveDateTime::parse_from_str(start, "%Y-%m-%dT%H:%M:%S")
        .ok()?
        .checked_add_signed(chrono::Duration::try_days(days)?)?;
    let day_end = local_to_utc_eas(&local.format("%Y-%m-%dT%H:%M:%S").to_string(), timezone)?;
    let utc = chrono::NaiveDateTime::parse_from_str(&day_end, "%Y%m%dT%H%M%SZ")
        .ok()?
        .checked_add_signed(chrono::Duration::try_seconds(seconds)?)?;
    Some(utc.format("%Y%m%dT%H%M%SZ").to_string())
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MailboxObject {
    id: String,
    name: String,
    parent_id: Option<String>,
    role: Option<String>,
}

impl From<MailboxObject> for Collection {
    fn from(value: MailboxObject) -> Self {
        let folder_type = match value.role.as_deref() {
            Some("inbox") => eas_folder_type::INBOX,
            Some("drafts") => eas_folder_type::DRAFTS,
            Some("trash") => eas_folder_type::WASTEBASKET,
            Some("sent") => eas_folder_type::SENTMAIL,
            _ => eas_folder_type::USER_MAIL,
        };
        Self {
            id: value.id,
            parent_id: value.parent_id,
            name: value.name,
            kind: CollectionKind::Mail,
            role: value.role,
            folder_type,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AddressBookObject {
    id: String,
    name: String,
    #[serde(default)]
    is_default: bool,
}

impl From<AddressBookObject> for Collection {
    fn from(value: AddressBookObject) -> Self {
        Self {
            id: format!("ab_{}", value.id),
            parent_id: None,
            name: value.name,
            kind: CollectionKind::Contacts,
            role: None,
            folder_type: if value.is_default {
                eas_folder_type::CONTACT
            } else {
                eas_folder_type::USER_CONTACT
            },
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CalendarObject {
    id: String,
    name: String,
    #[serde(default)]
    is_default: bool,
}

impl From<CalendarObject> for Collection {
    fn from(value: CalendarObject) -> Self {
        Self {
            id: format!("cal_{}", value.id),
            parent_id: None,
            name: value.name,
            kind: CollectionKind::Calendar,
            role: None,
            folder_type: if value.is_default {
                eas_folder_type::APPOINTMENT
            } else {
                eas_folder_type::USER_APPOINTMENT
            },
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn jscontact_media_parses_blob_id_for_recipient_photo_downloads() {
        let media: super::ContactCardMedia = serde_json::from_value(serde_json::json!({
            "kind": "photo",
            "blobId": "photo-blob-1"
        }))
        .unwrap();
        assert_eq!(media.kind.as_deref(), Some("photo"));
        assert_eq!(media.blob_id.as_deref(), Some("photo-blob-1"));
    }

    #[test]
    fn microsoft_disallow_counter_property_maps_to_eas_flag() {
        let icalendar = serde_json::json!({
            "properties": [
                ["x-microsoft-disallow-counter", {}, "boolean", true]
            ]
        });
        assert_eq!(
            super::calendar_disallow_new_time_proposal(Some(&icalendar)),
            Some(true)
        );
        let event: super::CalendarEventObject = serde_json::from_value(serde_json::json!({
            "id":"event", "iCalendar": icalendar
        }))
        .unwrap();
        assert_eq!(
            super::calendar_event_from(event, None).disallow_new_time_proposal,
            Some(true)
        );
        assert_eq!(
            super::calendar_disallow_new_time_proposal(Some(&serde_json::json!({
                "properties": [["X-MICROSOFT-DISALLOW-COUNTER", {}, "boolean", false]]
            }))),
            Some(false)
        );
        assert_eq!(
            super::calendar_disallow_new_time_proposal(Some(&serde_json::json!({
                "properties": [
                    ["X-MICROSOFT-DISALLOW-COUNTER", {}, "boolean", true],
                    ["X-MICROSOFT-DISALLOW-COUNTER", {}, "boolean", false]
                ]
            }))),
            None
        );
    }

    #[test]
    fn microsoft_disallow_counter_write_preserves_other_icalendar_properties() {
        let existing = serde_json::json!({
            "properties": [
                ["X-OTHER-PROPERTY", {"X-PARAM":"kept"}, "text", "value"],
                ["X-MICROSOFT-DISALLOW-COUNTER", {}, "boolean", false]
            ],
            "components": [["VALARM", [], []]]
        });
        let updated =
            super::calendar_icalendar_with_disallow_new_time_proposal(Some(&existing), true)
                .unwrap();
        assert_eq!(updated["properties"].as_array().unwrap().len(), 2);
        assert_eq!(updated["properties"][0], existing["properties"][0]);
        assert_eq!(updated["properties"][1][3], true);
        assert_eq!(updated["components"], existing["components"]);
        assert_eq!(
            super::calendar_disallow_new_time_proposal(Some(&updated)),
            Some(true)
        );

        let created =
            super::calendar_icalendar_with_disallow_new_time_proposal(None, false).unwrap();
        assert_eq!(
            super::calendar_disallow_new_time_proposal(Some(&created)),
            Some(false)
        );
        let duplicate = serde_json::json!({"properties":[
            ["X-MICROSOFT-DISALLOW-COUNTER",{},"boolean",true],
            ["X-MICROSOFT-DISALLOW-COUNTER",{},"boolean",false]
        ]});
        assert!(
            super::calendar_icalendar_with_disallow_new_time_proposal(Some(&duplicate), true)
                .is_err()
        );
    }

    #[test]
    fn appointment_reply_time_uses_only_the_unique_authenticated_attendee() {
        let participants: std::collections::BTreeMap<String, super::ParticipantObject> =
            serde_json::from_value(serde_json::json!({
                "owner": {"email":"owner@example.test", "roles":{"owner":true}},
                "guest": {
                    "email":"guest@example.test", "roles":{"attendee":true},
                    "scheduleUpdated":"2026-11-24T12:34:56-05:00"
                }
            }))
            .unwrap();
        assert_eq!(
            super::calendar_appointment_reply_time(Some(&participants), "GUEST@example.test")
                .as_deref(),
            Some("20261124T173456Z")
        );
        assert_eq!(
            super::calendar_appointment_reply_time(Some(&participants), "owner@example.test"),
            None
        );
        assert_eq!(
            super::calendar_appointment_reply_time(Some(&participants), "absent@example.test"),
            None
        );
        let mut ambiguous = participants.clone();
        ambiguous.insert(
            "duplicate".into(),
            serde_json::from_value(serde_json::json!({
                "email":"guest@example.test", "roles":{"attendee":true},
                "scheduleUpdated":"2026-11-24T12:34:56Z"
            }))
            .unwrap(),
        );
        assert_eq!(
            super::calendar_appointment_reply_time(Some(&ambiguous), "guest@example.test"),
            None
        );
        let event: super::CalendarEventObject = serde_json::from_value(serde_json::json!({
            "id":"event",
            "participants": {
                "guest": {
                    "email":"guest@example.test", "roles":{"attendee":true},
                    "scheduleUpdated":"2026-11-24T12:34:56-05:00"
                }
            }
        }))
        .unwrap();
        assert_eq!(
            super::calendar_event_from(event, Some("guest@example.test"))
                .appointment_reply_time
                .as_deref(),
            Some("20261124T173456Z")
        );
    }

    #[test]
    fn calendar_virtual_locations_map_to_eas_online_meeting_links() {
        let event: super::CalendarEventObject = serde_json::from_value(serde_json::json!({
            "id": "event",
            "virtualLocations": {
                "conf": {"@type":"VirtualLocation", "uri":"sip:room@example.test"},
                "web": {"@type":"VirtualLocation", "uri":"https://meet.example.test/room"}
            }
        }))
        .unwrap();
        let event = crate::model::CalendarEvent::from(event);
        assert_eq!(
            event.online_meeting_conf_link.as_deref(),
            Some("sip:room@example.test")
        );
        assert_eq!(
            event.online_meeting_external_link.as_deref(),
            Some("https://meet.example.test/room")
        );
    }

    #[test]
    fn calendar_virtual_location_recurrence_patch_maps_link_changes() {
        let base: std::collections::BTreeMap<String, serde_json::Value> =
            serde_json::from_value(serde_json::json!({
                "conf": {"uri":"sip:old@example.test"},
                "web": {"uri":"https://meet.example.test/old"}
            }))
            .unwrap();
        let overrides = serde_json::json!({
            "2026-11-24T10:00:00": {
                "virtualLocations/conf/uri": "sip:new@example.test",
                "virtualLocations/web/uri": null
            }
        });
        let exceptions = super::calendar_recurrence_exceptions(
            Some(&overrides),
            super::CalendarExceptionContext {
                base_alerts: None,
                base_participants: None,
                base_links: None,
                base_virtual_locations: Some(&base),
                current_user: None,
                timezone: Some("Etc/UTC"),
                event_duration: Some("PT1H"),
            },
        )
        .unwrap();
        assert_eq!(exceptions.len(), 1);
        assert_eq!(
            exceptions[0].online_meeting_conf_link.as_deref(),
            Some("sip:new@example.test")
        );
        assert_eq!(
            exceptions[0].online_meeting_external_link.as_deref(),
            Some("")
        );
    }

    #[test]
    fn calendar_exception_reply_time_uses_authenticated_participant_override() {
        let participants: std::collections::BTreeMap<String, super::ParticipantObject> =
            serde_json::from_value(serde_json::json!({
                "owner": {"email":"owner@example.test", "roles":{"owner":true}},
                "guest": {
                    "email":"guest@example.test", "roles":{"attendee":true},
                    "participationStatus":"accepted",
                    "scheduleUpdated":"2026-11-23T00:00:00Z"
                }
            }))
            .unwrap();
        let overrides = serde_json::json!({
            "2026-11-24T10:00:00": {
                "participants/guest/scheduleUpdated":"2026-11-24T12:34:56-05:00"
            }
        });
        let exceptions = super::calendar_recurrence_exceptions(
            Some(&overrides),
            super::CalendarExceptionContext {
                base_alerts: None,
                base_participants: Some(&participants),
                base_links: None,
                base_virtual_locations: None,
                current_user: Some("guest@example.test"),
                timezone: Some("Etc/UTC"),
                event_duration: Some("PT1H"),
            },
        )
        .unwrap();
        assert_eq!(
            exceptions[0].appointment_reply_time.as_deref(),
            Some("20261124T173456Z")
        );
        assert_eq!(exceptions[0].attendees.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn task_importance_maps_to_jscalendar_priority_bands() {
        assert_eq!(super::eas_importance_to_jmap("0"), Some(9));
        assert_eq!(super::eas_importance_to_jmap("1"), Some(5));
        assert_eq!(super::eas_importance_to_jmap("2"), Some(1));
        assert_eq!(super::eas_importance_to_jmap("3"), None);
        for priority in 1..=4 {
            assert_eq!(super::jmap_task_priority_to_eas(Some(priority)), Some(2));
        }
        assert_eq!(super::jmap_task_priority_to_eas(Some(5)), Some(1));
        for priority in 6..=9 {
            assert_eq!(super::jmap_task_priority_to_eas(Some(priority)), Some(0));
        }
        assert_eq!(super::jmap_task_priority_to_eas(Some(0)), None);
        assert_eq!(super::jmap_task_priority_to_eas(None), None);
    }

    #[test]
    fn task_sensitivity_maps_to_jscalendar_privacy_levels() {
        assert_eq!(super::eas_task_sensitivity_to_jmap("0"), Some("public"));
        assert_eq!(super::eas_task_sensitivity_to_jmap("1"), Some("private"));
        assert_eq!(super::eas_task_sensitivity_to_jmap("2"), Some("private"));
        assert_eq!(super::eas_task_sensitivity_to_jmap("3"), Some("secret"));
        assert_eq!(super::eas_task_sensitivity_to_jmap("4"), None);
        assert_eq!(super::jmap_task_sensitivity_to_eas(None), 0);
        assert_eq!(super::jmap_task_sensitivity_to_eas(Some("public")), 0);
        assert_eq!(super::jmap_task_sensitivity_to_eas(Some("private")), 2);
        assert_eq!(super::jmap_task_sensitivity_to_eas(Some("secret")), 3);
        assert_eq!(super::jmap_task_sensitivity_to_eas(Some("vendor")), 2);
    }

    #[test]
    fn task_completion_time_round_trips_as_eas_datetime() {
        let task: super::TaskObject = serde_json::from_value(serde_json::json!({
            "id": "t", "@type": "Task", "progress": "completed",
            "progressUpdated": "2026-11-20T11:45:00-05:00",
            "start": "2026-11-20T11:30:00",
            "priority": 1,
            "privacy": "secret",
            "keywords": {"Work": true, "Hidden": false},
            "alerts": {"a":{"@type":"Alert","trigger":{"@type":"AbsoluteTrigger","when":"2026-11-20T11:45:00-05:00"}}}
        }))
        .unwrap();
        let task = super::try_task_from_object(task).unwrap();
        assert_eq!(
            task.date_completed.as_deref(),
            Some("2026-11-20T16:45:00.000Z")
        );
        assert_eq!(task.start.as_deref(), Some("20261120T113000Z"));
        assert_eq!(task.importance, Some(2));
        assert_eq!(task.sensitivity, 3);
        assert_eq!(task.categories, vec!["Work"]);
        assert_eq!(
            task.reminder_time.as_deref(),
            Some("2026-11-20T16:45:00.000Z")
        );

        let task: super::TaskObject = serde_json::from_value(serde_json::json!({
            "id": "t", "@type": "Task", "progress": "needs-action",
            "progressUpdated": "2026-11-20T16:45:00Z"
        }))
        .unwrap();
        assert_eq!(
            super::try_task_from_object(task).unwrap().date_completed,
            None
        );
    }

    #[test]
    fn task_recurrence_is_read_from_jscalendar_and_exceptions_fail_closed() {
        let task: super::TaskObject = serde_json::from_value(serde_json::json!({
            "id":"t", "@type":"Task",
            "recurrenceRule":{"frequency":"weekly","byDay":[{"day":"mo"},{"day":"we"}],"count":8}
        }))
        .unwrap();
        let task = super::try_task_from_object(task).unwrap();
        assert_eq!(task.recurrence.as_ref().unwrap().kind, 1);
        assert_eq!(task.recurrence.as_ref().unwrap().day_of_week, Some(10));
        assert_eq!(task.recurrence.as_ref().unwrap().occurrences, Some(8));

        let task: super::TaskObject = serde_json::from_value(serde_json::json!({
            "id":"t", "@type":"Task",
            "recurrenceRule":{"frequency":"weekly","byDay":[{"day":"mo"}],"count":256}
        }))
        .unwrap();
        assert!(super::try_task_from_object(task).is_err());

        let task: super::TaskObject = serde_json::from_value(serde_json::json!({
            "id":"t", "@type":"Task",
            "recurrenceRule":{"frequency":"weekly","byDay":[{"day":"mo"}]},
            "recurrenceOverrides":{"2026-11-25T09:00:00":{"excluded":true}}
        }))
        .unwrap();
        assert!(super::try_task_from_object(task).is_err());
    }

    #[test]
    fn calendar_updated_time_uses_compact_utc() {
        let event: super::CalendarEventObject = serde_json::from_value(
            serde_json::json!({"id":"e", "updated":"2024-02-29T23:59:58.123Z"}),
        )
        .unwrap();
        let event = crate::model::CalendarEvent::from(event);
        assert_eq!(event.updated_utc.as_deref(), Some("20240229T235958Z"));
        let event: super::CalendarEventObject =
            serde_json::from_value(serde_json::json!({"id":"e", "updated":"invalid"})).unwrap();
        assert_eq!(crate::model::CalendarEvent::from(event).updated_utc, None);
    }

    #[test]
    fn recurrence_alert_patches_preserve_unrelated_alerts_and_support_clear() {
        let base = serde_json::json!({
            "display":{"@type":"Alert","trigger":{"@type":"OffsetTrigger","relativeTo":"start","offset":"-PT30M"}},
            "email":{"@type":"Alert","action":"email","trigger":{"@type":"OffsetTrigger","relativeTo":"start","offset":"-PT1H"}}
        });
        let changed = serde_json::Map::from_iter([(
            "alerts/display/trigger".to_owned(),
            serde_json::json!({"@type":"OffsetTrigger","relativeTo":"start","offset":"-PT15M"}),
        )]);
        let merged = super::merge_calendar_alert_patch(Some(&base), &changed).unwrap();
        assert_eq!(merged["display"]["trigger"]["offset"], "-PT15M");
        assert!(merged.get("email").is_some());

        let cleared =
            serde_json::Map::from_iter([("alerts/display".to_owned(), serde_json::Value::Null)]);
        let merged = super::merge_calendar_alert_patch(Some(&base), &cleared).unwrap();
        assert!(merged.get("display").is_none());
        assert!(merged.get("email").is_some());
    }

    #[test]
    fn calendar_recurrence_rules_round_trip_and_map_excluded_exceptions() {
        let object: super::CalendarEventObject = serde_json::from_value(serde_json::json!({
            "id":"e",
            "recurrenceRule":{"frequency":"weekly","interval":2,"byDay":[{"day":"mo"},{"day":"we"}],"count":12},
            "recurrenceOverrides":{}
        }))
        .unwrap();
        let event = crate::model::CalendarEvent::from(object);
        assert_eq!(
            event.recurrence,
            Some(crate::wbxml::eas::CalendarRecurrence {
                kind: 1,
                interval: Some(2),
                day_of_week: Some(10),
                occurrences: Some(12),
                ..Default::default()
            })
        );
        assert!(!event.unsupported_recurrence);

        let object: super::CalendarEventObject = serde_json::from_value(serde_json::json!({
            "id":"e", "uid":"series-uid",
            "recurrenceRule":{"frequency":"weekly","byDay":[{"day":"mo"}]},
            "timeZone":"America/Toronto",
            "recurrenceOverrides":{"2026-11-25T12:00:00":{"excluded":true}}
        }))
        .unwrap();
        let event = crate::model::CalendarEvent::from(object);
        assert!(!event.unsupported_recurrence);
        assert!(event.recurrence.is_some());
        assert_eq!(event.time_zone.as_deref(), Some("America/Toronto"));
        assert_eq!(
            event.recurrence_exceptions,
            vec![crate::model::CalendarEventException {
                instance_id: "20261125T170000Z".to_owned(),
                deleted: true,
                ..Default::default()
            }]
        );

        let object: super::CalendarEventObject = serde_json::from_value(serde_json::json!({
            "id":"e", "recurrenceOverrides":{"2026-11-25T12:00:00":{"excluded":true}}
        }))
        .unwrap();
        assert!(crate::model::CalendarEvent::from(object).unsupported_recurrence);

        let object: super::CalendarEventObject = serde_json::from_value(serde_json::json!({
            "id":"e", "recurrenceRule":{"frequency":"weekly","byDay":[{"day":"mo"}]},
            "uid":"series-uid", "timeZone":"America/Toronto",
            "participants":{
                "owner":{"email":"owner@example.test","roles":{"owner":true}},
                "guest":{"email":"guest@example.test","name":"Guest","roles":{"attendee":true,"optional":true},"participationStatus":"accepted"}
            },
            "links":{"file":{"@type":"Link","blobId":"blob-123","rel":"enclosure","title":"series.pdf","contentType":"application/pdf","size":4096,"cid":"part-1","href":"cid:part-1"}},
            "alerts":{"display":{"@type":"Alert","trigger":{"@type":"OffsetTrigger","relativeTo":"start","offset":"-PT30M"}}},
            "recurrenceOverrides":{"2026-11-25T12:00:00":{
                "start":"2026-11-25T13:00:00", "duration":"PT3600S", "showWithoutTime":true,
                "freeBusyStatus":"x-eas-tentative",
                "alerts/display/trigger":{"@type":"OffsetTrigger","relativeTo":"start","offset":"-PT20M"},
                "participants/guest/participationStatus":"declined",
                "links/file/title":"occurrence.pdf",
                "keywords":{"Work":true,"Hidden":false},
                "title":"Changed occurrence", "description":"Exception body",
                "descriptionContentType":"text/plain",
                "locations":{"room":{"name":"Changed room", "coordinates":"geo:43.65,-79.38"}}
            }}
        }))
        .unwrap();
        let event = crate::model::CalendarEvent::from(object);
        assert!(!event.unsupported_recurrence);
        assert_eq!(
            event.recurrence_exceptions[0].title.as_deref(),
            Some("Changed occurrence")
        );
        assert_eq!(
            event.recurrence_exceptions[0].instance_id,
            "20261125T170000Z"
        );
        let body = event.recurrence_exceptions[0].description.as_ref().unwrap();
        assert_eq!(body.body_type, crate::model::EmailBodyType::Plain);
        assert_eq!(body.value, "Exception body");
        assert_eq!(
            event.recurrence_exceptions[0].location.as_deref(),
            Some("Changed room")
        );
        assert_eq!(
            event.recurrence_exceptions[0].location_coordinates,
            Some(("43.65".to_owned(), "-79.38".to_owned()))
        );
        assert_eq!(
            event.recurrence_exceptions[0].start_utc.as_deref(),
            Some("20261125T180000Z")
        );
        assert_eq!(
            event.recurrence_exceptions[0].end_utc.as_deref(),
            Some("20261125T190000Z")
        );
        assert_eq!(event.recurrence_exceptions[0].all_day, Some(true));
        assert_eq!(event.recurrence_exceptions[0].busy_status, Some(1));
        assert_eq!(
            event.recurrence_exceptions[0].reminder_minutes,
            Some(Some(20))
        );
        assert_eq!(
            event.recurrence_exceptions[0].categories,
            Some(vec!["Work".to_owned()])
        );
        assert_eq!(
            event.recurrence_exceptions[0].attendees,
            Some(vec![crate::model::Attendee {
                email: "guest@example.test".to_owned(),
                name: Some("Guest".to_owned()),
                participation_status: Some("declined".to_owned()),
                optional: true,
            }])
        );
        assert_eq!(
            event.recurrence_exceptions[0].attachments,
            Some(vec![crate::model::CalendarAttachment {
                blob_id: "blob-123".to_owned(),
                name: Some("occurrence.pdf".to_owned()),
                content_type: Some("application/pdf".to_owned()),
                size: Some(4096),
                content_id: Some("part-1".to_owned()),
                content_location: Some("cid:part-1".to_owned()),
            }])
        );

        let privacy_override: super::CalendarEventObject =
            serde_json::from_value(serde_json::json!({
                "id":"e", "recurrenceRule":{"frequency":"weekly"},
                "timeZone":"America/Toronto",
                "recurrenceOverrides":{"2026-11-25T12:00:00":{"privacy":"private"}}
            }))
            .unwrap();
        assert!(crate::model::CalendarEvent::from(privacy_override).unsupported_recurrence);

        let object: super::CalendarEventObject = serde_json::from_value(serde_json::json!({
            "id":"e", "recurrenceRule":{"frequency":"weekly","byDay":[{"day":"mo"}]},
            "uid":"series-uid", "timeZone":"America/Toronto",
            "recurrenceOverrides":{"2026-11-25T12:00:00":{"start":"2026-11-25T13:00:00"}}
        }))
        .unwrap();
        let event = crate::model::CalendarEvent::from(object);
        assert!(!event.unsupported_recurrence);
        assert_eq!(
            event.recurrence_exceptions[0].start_utc.as_deref(),
            Some("20261125T180000Z")
        );

        let object: super::CalendarEventObject = serde_json::from_value(serde_json::json!({
            "id":"e", "uid":"series-uid", "timeZone":"America/Toronto",
            "recurrenceRule":{"frequency":"weekly","byDay":[{"day":"we"}]},
            "recurrenceOverrides":{"2026-11-25T12:00:00":{
                "description":"HTML", "descriptionContentType":"text/html"
            }}
        }))
        .unwrap();
        let event = crate::model::CalendarEvent::from(object);
        assert!(!event.unsupported_recurrence);
        assert_eq!(
            event.recurrence_exceptions[0]
                .description
                .as_ref()
                .unwrap()
                .body_type,
            crate::model::EmailBodyType::Html
        );
        assert_eq!(
            event.recurrence_exceptions[0]
                .description
                .as_ref()
                .unwrap()
                .value,
            "HTML"
        );
    }

    #[test]
    fn calendar_coordinates_require_valid_simple_wgs84_uri() {
        for (coordinates, expected) in [
            ("geo:43.65,-79.38", Some(("43.65", "-79.38"))),
            ("geo:91,0", None),
            ("geo:0,181", None),
            ("geo:NaN,0", None),
            ("geo:43,-79;crs=other", None),
            ("geo:43,-79,4", None),
        ] {
            let object: super::CalendarEventObject = serde_json::from_value(serde_json::json!({
                "id":"e", "locations":{"l":{"name":"Room", "coordinates":coordinates}}
            }))
            .unwrap();
            let event = crate::model::CalendarEvent::from(object);
            assert_eq!(
                event
                    .location_coordinates
                    .as_ref()
                    .map(|(a, b)| (a.as_str(), b.as_str())),
                expected
            );
        }
    }

    use super::{
        calendar_end_utc, eas_compact_to_local_iso, eas_to_calendar_local, local_to_utc_eas,
        parse_calendar_duration,
    };

    #[test]
    fn invitation_creation_rejects_the_organizer_as_attendee() {
        let event = serde_json::json!({"participants": {
            "owner":{"email":"self@example.test","roles":{"owner":true}},
            "attendee":{"email":"self@example.test"}
        }});
        assert!(super::invitation_event_for_create(
            &event,
            "self@example.test",
            "SELF@example.test",
            "cal",
            "accepted"
        )
        .unwrap()
        .is_none());
    }

    #[test]
    fn submission_requires_a_confirmed_created_id() {
        for (response, expected) in [
            (
                serde_json::json!({"methodResponses":[["EmailSubmission/set",{"created":{"s1":{"id":"ok"}}},"sub"]]}),
                true,
            ),
            (
                serde_json::json!({"methodResponses":[["error",{"type":"serverFail"},"sub"]]}),
                false,
            ),
            (
                serde_json::json!({"methodResponses":[["EmailSubmission/set",{"notCreated":{"s1":{"type":"forbidden"}}},"sub"]]}),
                false,
            ),
            (
                serde_json::json!({"methodResponses":[["EmailSubmission/set",{"created":{}},"sub"]]}),
                false,
            ),
            (
                serde_json::json!({"methodResponses":[["EmailSubmission/set",{"created":{"s1":{"id":""}}},"sub"]]}),
                false,
            ),
            (
                serde_json::json!({"methodResponses":[["EmailSubmission/set",{"created":{"s1":{"id":"ok"}}},"wrong"]]}),
                false,
            ),
            (serde_json::json!({"methodResponses":[]}), false),
        ] {
            let parsed: super::JmapResponse<serde_json::Value> =
                serde_json::from_value(response).unwrap();
            assert_eq!(super::confirm_submission(&parsed).is_ok(), expected);
        }
    }

    #[test]
    fn calendar_organizer_uses_reply_address_when_multiple_owners_exist() {
        let item: super::CalendarEventObject = serde_json::from_value(serde_json::json!({
            "id":"e", "replyTo":{"imip":"mailto:owner@example.test"},
            "participants":{
                "owner":{"email":"owner@example.test","roles":{"owner":true}},
                "guest":{"email":"guest@example.test","roles":{"owner":true},"participationStatus":"accepted"}
            }
        })).unwrap();
        let event = crate::model::CalendarEvent::from(item);
        assert_eq!(event.organizer_email.as_deref(), Some("owner@example.test"));
        assert_eq!(event.attendees.len(), 1);
        assert_eq!(
            super::calendar_response_type(&event, "guest@example.test"),
            Some(3)
        );
        assert_eq!(
            super::calendar_response_type(&event, "owner@example.test"),
            Some(1)
        );

        let ambiguous: super::CalendarEventObject = serde_json::from_value(serde_json::json!({
            "id":"e", "participants":{
                "a":{"email":"a@example.test","roles":{"owner":true}},
                "b":{"email":"b@example.test","roles":{"owner":true}}
            }
        }))
        .unwrap();
        let ambiguous = crate::model::CalendarEvent::from(ambiguous);
        assert!(ambiguous.organizer_email.is_none());
        assert_eq!(
            super::calendar_response_type(&ambiguous, "a@example.test"),
            None
        );
        let delegated_reply: super::CalendarEventObject =
            serde_json::from_value(serde_json::json!({
                "id":"e", "replyTo":{"imip":"mailto:guest@example.test"},
                "participants":{
                    "owner":{"email":"owner@example.test","roles":{"owner":true}},
                    "guest":{"email":"guest@example.test","roles":{"attendee":true}}
                }
            }))
            .unwrap();
        let event = crate::model::CalendarEvent::from(delegated_reply);
        assert_eq!(event.organizer_email.as_deref(), Some("owner@example.test"));
    }

    #[test]
    fn calendar_response_type_uses_the_authenticated_participant() {
        let mut event = crate::model::CalendarEvent {
            organizer_email: Some("owner@example.test".into()),
            attendees: vec![crate::model::Attendee {
                email: "guest@example.test".into(),
                name: None,
                participation_status: None,
                optional: false,
            }],
            ..Default::default()
        };
        assert_eq!(
            super::calendar_response_type(&event, "OWNER@example.test"),
            Some(1)
        );
        for (status, expected) in [
            (None, Some(5)),
            (Some("needs-action"), Some(5)),
            (Some("tentative"), Some(2)),
            (Some("accepted"), Some(3)),
            (Some("declined"), Some(4)),
            (Some("delegated"), None),
        ] {
            event.attendees[0].participation_status = status.map(str::to_owned);
            assert_eq!(
                super::calendar_response_type(&event, "GUEST@example.test"),
                expected
            );
        }
        assert_eq!(
            super::calendar_response_type(&event, "other@example.test"),
            None
        );
        event.attendees[0].participation_status = Some("accepted".into());
        event.attendees.push(event.attendees[0].clone());
        assert_eq!(
            super::calendar_response_type(&event, "guest@example.test"),
            None
        );
        event.attendees.clear();
        assert_eq!(
            super::calendar_response_type(&event, "owner@example.test"),
            None
        );
    }

    #[test]
    fn calendar_response_requested_requires_a_uniform_attendee_preference() {
        for (participants, expected) in [
            (
                serde_json::json!({
                    "owner":{"email":"owner@example.test","roles":{"owner":true},"expectReply":true},
                    "a":{"email":"a@example.test","roles":{"attendee":true},"expectReply":true},
                    "b":{"email":"b@example.test","roles":{"attendee":true},"expectReply":true}
                }),
                Some(true),
            ),
            (
                serde_json::json!({
                    "owner":{"email":"owner@example.test","roles":{"owner":true}},
                    "a":{"email":"a@example.test","roles":{"attendee":true},"expectReply":false}
                }),
                Some(false),
            ),
            (
                serde_json::json!({
                    "a":{"email":"a@example.test","roles":{"attendee":true},"expectReply":true},
                    "b":{"email":"b@example.test","roles":{"attendee":true},"expectReply":false}
                }),
                None,
            ),
            (
                serde_json::json!({"owner":{"email":"owner@example.test","roles":{"owner":true}}}),
                None,
            ),
        ] {
            let participants: std::collections::BTreeMap<String, super::ParticipantObject> =
                serde_json::from_value(participants).unwrap();
            assert_eq!(super::calendar_response_requested(&participants), expected);
        }
    }

    #[test]
    fn calendar_privacy_maps_unknown_values_conservatively() {
        for (privacy, expected) in [
            (None, 0),
            (Some("public"), 0),
            (Some("private"), 2),
            (Some("secret"), 3),
            (Some("x-other"), 3),
        ] {
            let event = super::CalendarEventObject {
                privacy: privacy.map(str::to_owned),
                ..Default::default()
            };
            let event: crate::model::CalendarEvent = event.into();
            assert_eq!(event.sensitivity, expected);
        }
    }

    #[test]
    fn jmap_body_truncation_is_preserved_even_for_an_empty_prefix() {
        let mut email: super::EmailObject = serde_json::from_value(serde_json::json!({
            "id":"large", "htmlBody":[{"partId":"h"}],
            "textBody":[{"partId":"p"}],
            "bodyValues":{"h":{"value":"","isTruncated":true},"p":{"value":"fallback","isTruncated":false}}
        })).unwrap();
        let body = super::select_body(&mut email).unwrap();
        assert_eq!(body.body_type, crate::model::EmailBodyType::Html);
        assert!(body.is_truncated);
        assert!(body.value.is_empty());
        let mut email: super::EmailObject = serde_json::from_value(serde_json::json!({
            "id":"normal","textBody":[{"partId":"p"}],"bodyValues":{"p":{"value":"complete"}}
        }))
        .unwrap();
        let body = super::select_body(&mut email).unwrap();
        assert_eq!(body.value, "complete");
        assert!(!body.is_truncated);
        assert!(email.body_values["p"].value.is_empty());
    }

    #[test]
    fn jmap_sent_at_is_preserved_for_find_postfiltering() {
        let email: crate::model::Email =
            serde_json::from_value::<super::EmailObject>(serde_json::json!({
            "id":"sent-date",
            "sentAt":"2026-02-28T12:34:56Z"
            }))
            .unwrap()
            .into();
        assert_eq!(email.sent_at.as_deref(), Some("2026-02-28T12:34:56Z"));
    }

    #[test]
    fn eas_compact_to_local_iso_reformats_without_offset() {
        assert_eq!(
            eas_compact_to_local_iso("20260901T140000Z"),
            Some("2026-09-01T14:00:00".to_owned())
        );
    }

    #[test]
    fn calendar_utc_conversion_preserves_zone_and_rejects_ambiguous_time() {
        assert_eq!(
            eas_to_calendar_local("20261115T150000Z", Some("America/Toronto"))
                .unwrap()
                .format("%Y-%m-%dT%H:%M:%S")
                .to_string(),
            "2026-11-15T10:00:00"
        );
        assert!(eas_to_calendar_local("20261101T063000Z", Some("America/Toronto")).is_none());
        assert!(eas_to_calendar_local("20261115T150000Z", Some("Invalid/Zone")).is_none());
    }

    #[test]
    fn eas_instance_id_maps_to_the_original_local_recurrence_key() {
        assert_eq!(
            super::recurrence_override_key("2026-11-15T15:00:00.000Z", Some("America/Toronto"))
                .unwrap(),
            "2026-11-15T10:00:00"
        );
        assert_eq!(
            super::recurrence_override_key("2026-11-15T15:00:00Z", None).unwrap(),
            "2026-11-15T15:00:00"
        );
        assert!(super::recurrence_override_key("not-a-date", None).is_err());
        assert!(
            super::recurrence_override_key("2026-11-15T15:00:00Z", Some("Invalid/Zone")).is_err()
        );
    }

    #[test]
    fn duration_parses_hours_only() {
        assert_eq!(parse_calendar_duration("PT1H"), Some((0, 3600)));
    }

    #[test]
    fn duration_parses_hours_and_minutes() {
        assert_eq!(parse_calendar_duration("PT1H30M"), Some((0, 5400)));
    }

    #[test]
    fn duration_parses_days_only() {
        assert_eq!(parse_calendar_duration("P1D"), Some((1, 0)));
    }

    #[test]
    fn duration_parses_minutes_only() {
        assert_eq!(parse_calendar_duration("PT30M"), Some((0, 1800)));
    }

    #[test]
    fn duration_rejects_garbage() {
        assert_eq!(parse_calendar_duration("not a duration"), None);
    }

    #[test]
    fn calendar_duration_respects_dst_and_calendar_days() {
        let tz = Some("America/Toronto");
        assert_eq!(
            calendar_end_utc("2026-03-08T01:30:00", tz, "PT2H").as_deref(),
            Some("20260308T083000Z")
        );
        assert_eq!(
            calendar_end_utc("2026-11-01T00:30:00", tz, "PT2H").as_deref(),
            Some("20261101T063000Z")
        );
        assert_eq!(
            calendar_end_utc("2026-03-07T12:00:00", tz, "P1D").as_deref(),
            Some("20260308T160000Z")
        );
        assert_eq!(
            calendar_end_utc("2026-03-07T12:00:00", tz, "P1DT2H").as_deref(),
            Some("20260308T180000Z")
        );
        assert_eq!(parse_calendar_duration("P1W"), Some((7, 0)));
        assert_eq!(parse_calendar_duration("P1W2DT2H"), Some((9, 7200)));
        for invalid in [
            "P",
            "PT",
            "PT1Hgarbage",
            "P-1D",
            "PT-1H",
            "P1DT",
            "P-1W",
            "P1W-1D",
        ] {
            assert_eq!(parse_calendar_duration(invalid), None, "{invalid}");
        }
    }

    #[test]
    fn calendar_nonexistent_local_time_uses_pre_transition_offset() {
        assert_eq!(
            local_to_utc_eas("2026-03-08T02:30:00", Some("America/Toronto")).as_deref(),
            Some("20260308T073000Z")
        );
        assert_eq!(
            calendar_end_utc("2026-03-07T02:30:00", Some("America/Toronto"), "P1D").as_deref(),
            Some("20260308T073000Z")
        );
    }

    #[test]
    fn local_to_utc_converts_named_timezone() {
        // 2026-08-25 is EDT (UTC-4), not EST -- 14:00 America/Toronto is
        // 18:00 UTC. Picking a summer date deliberately, to catch a
        // fixed-offset-instead-of-real-tz-database bug (which would give
        // the same wrong answer year-round instead of only in winter).
        assert_eq!(
            local_to_utc_eas("2026-08-25T14:00:00", Some("America/Toronto")),
            Some("20260825T180000Z".to_owned())
        );
    }

    #[test]
    fn local_to_utc_converts_winter_dst_correctly() {
        // 2026-01-15 is EST (UTC-5) -- confirms this isn't just adding a
        // fixed 4-hour offset year-round.
        assert_eq!(
            local_to_utc_eas("2026-01-15T09:00:00", Some("America/Toronto")),
            Some("20260115T140000Z".to_owned())
        );
    }

    #[test]
    fn local_to_utc_treats_missing_timezone_as_already_utc() {
        assert_eq!(
            local_to_utc_eas("2026-08-25T14:00:00", None),
            Some("20260825T140000Z".to_owned())
        );
    }

    #[test]
    fn local_to_utc_rejects_unparseable_timestamp() {
        assert_eq!(
            local_to_utc_eas("not a date", Some("America/Toronto")),
            None
        );
    }
}

#[cfg(test)]
mod smime_tests {
    use super::*;

    #[test]
    fn email_attachments_keep_their_body_structure_part_ids() {
        let email: Email = serde_json::from_value::<EmailObject>(serde_json::json!({
            "id":"m",
            "attachments":[{"blobId":"blob-a","name":"a.txt"}],
            "bodyStructure":{"type":"multipart/mixed","subParts":[
                {"type":"text/plain","partId":"1","blobId":"body-blob"},
                {"type":"application/octet-stream","partId":"2","blobId":"blob-a"}
            ]}
        }))
        .unwrap()
        .into();
        assert_eq!(email.attachments[0].part_id.as_deref(), Some("2"));
    }

    #[test]
    fn mime_structure_classifies_only_whole_smime_messages() {
        use crate::model::SmimeKind::{MultipartSigned, Opaque};
        for (structure, expected) in [
            (
                serde_json::json!({"type":"application/pkcs7-mime","subParts":null}),
                Some(Opaque),
            ),
            (
                serde_json::json!({"type":"Application/X-PKCS7-Mime"}),
                Some(Opaque),
            ),
            (
                serde_json::json!({"type":"multipart/signed","subParts":[
                {"type":"text/plain"}, {"type":"application/pkcs7-signature"}]}),
                Some(MultipartSigned),
            ),
            (
                serde_json::json!({"type":"multipart/signed","subParts":[
                {"type":"text/plain"}, {"type":"application/pgp-signature"}]}),
                None,
            ),
            (
                serde_json::json!({"type":"multipart/mixed","subParts":[
                {"type":"text/plain"}, {"type":"application/pkcs7-mime"}]}),
                None,
            ),
            (
                serde_json::json!({"type":"text/plain","subParts":null}),
                None,
            ),
        ] {
            let email: Email = serde_json::from_value::<EmailObject>(
                serde_json::json!({"id":"m", "bodyStructure":structure}),
            )
            .unwrap()
            .into();
            assert_eq!(email.smime, expected);
        }
    }
}

#[cfg(test)]
mod calendar_update_state_tests {
    use super::*;
    use axum::{routing::post, Json, Router};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    #[tokio::test]
    async fn calendar_update_persists_and_verifies_disallow_new_time_proposal() {
        let requests = Arc::new(AtomicUsize::new(0));
        let request_count = requests.clone();
        let app = Router::new().route(
            "/",
            post(move |Json(request): Json<serde_json::Value>| {
                let request_count = request_count.clone();
                async move {
                    let call = &request["methodCalls"][0];
                    match request_count.fetch_add(1, Ordering::SeqCst) {
                        0 => {
                            assert_eq!(call[0], "CalendarEvent/get");
                            assert!(call[1]["properties"]
                                .as_array()
                                .unwrap()
                                .iter()
                                .any(|property| property == "iCalendar"));
                            Json(serde_json::json!({"methodResponses":[[
                                "CalendarEvent/get", {"state":"state-1", "list":[{
                                    "id":"event-1", "timeZone":"Etc/UTC",
                                    "iCalendar":{"properties":[[
                                        "X-OTHER-PROPERTY", {}, "text", "preserve me"
                                    ]]}
                                }]}, "state"
                            ]]}))
                        }
                        1 => {
                            assert_eq!(call[0], "CalendarEvent/set");
                            assert_eq!(call[1]["ifInState"], "state-1");
                            let update = &call[1]["update"]["event-1"];
                            assert_eq!(update["title"], "Changed title");
                            assert_eq!(
                                update["iCalendar"]["properties"].as_array().unwrap().len(),
                                2
                            );
                            assert_eq!(update["iCalendar"]["properties"][0][0], "X-OTHER-PROPERTY");
                            assert_eq!(
                                update["iCalendar"]["properties"][1][0],
                                "X-MICROSOFT-DISALLOW-COUNTER"
                            );
                            assert_eq!(update["iCalendar"]["properties"][1][3], true);
                            Json(serde_json::json!({"methodResponses":[[
                                "CalendarEvent/set", {"updated":{"event-1":null}}, "0"
                            ]]}))
                        }
                        2 => {
                            assert_eq!(call[0], "CalendarEvent/get");
                            Json(serde_json::json!({"methodResponses":[[
                                "CalendarEvent/get", {"list":[{
                                    "id":"event-1", "iCalendar":{"properties":[
                                        ["X-OTHER-PROPERTY", {}, "text", "preserve me"],
                                        ["X-MICROSOFT-DISALLOW-COUNTER", {}, "boolean", true]
                                    ]}
                                }]}, "verify-disallow-counter"
                            ]]}))
                        }
                        _ => panic!("unexpected JMAP call"),
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let session: JmapSession = serde_json::from_value(serde_json::json!({
            "capabilities":{}, "primaryAccounts":{capabilities::CALENDARS:"a"},
            "apiUrl":url, "downloadUrl":url, "uploadUrl":url, "username":"user", "state":"s"
        }))
        .unwrap();
        let auth = AuthenticatedSession {
            capabilities: GatewayCapabilities::from_session(&session),
            session,
            authorization: BasicAuthorization {
                username: "user".into(),
                password: "password".into(),
            },
        };
        let client = JmapClient::new(
            Config::from_env().unwrap(),
            Arc::new(crate::state::MemoryStateStore::default()),
        )
        .unwrap();
        let fields = crate::wbxml::eas::CalendarFields {
            subject: Some("Changed title".into()),
            disallow_new_time_proposal: Some(true),
            ..Default::default()
        };
        assert_eq!(
            client
                .save_calendar_event(&auth, "calendar", Some("event-1"), None, &fields, false)
                .await
                .unwrap(),
            "event-1"
        );
        assert_eq!(requests.load(Ordering::SeqCst), 3);
        server.abort();
    }

    #[tokio::test]
    async fn calendar_update_rejects_unretained_disallow_new_time_proposal() {
        let requests = Arc::new(AtomicUsize::new(0));
        let request_count = requests.clone();
        let app = Router::new().route(
            "/",
            post(move |Json(request): Json<serde_json::Value>| {
                let request_count = request_count.clone();
                async move {
                    let call = &request["methodCalls"][0];
                    match request_count.fetch_add(1, Ordering::SeqCst) {
                        0 => Json(serde_json::json!({"methodResponses":[[
                            "CalendarEvent/get", {"state":"state-1", "list":[{
                                "id":"event-1", "timeZone":"Etc/UTC",
                                "iCalendar":{"properties":[]}
                            }]}, "state"
                        ]]})),
                        1 => {
                            assert_eq!(call[0], "CalendarEvent/set");
                            Json(serde_json::json!({"methodResponses":[[
                                "CalendarEvent/set", {"updated":{"event-1":null}}, "0"
                            ]]}))
                        }
                        2 => Json(serde_json::json!({"methodResponses":[[
                            "CalendarEvent/get", {"list":[{
                                "id":"event-1", "iCalendar":{"properties":[]}
                            }]}, "verify-disallow-counter"
                        ]]})),
                        _ => panic!("unexpected JMAP call"),
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let session: JmapSession = serde_json::from_value(serde_json::json!({
            "capabilities":{}, "primaryAccounts":{capabilities::CALENDARS:"a"},
            "apiUrl":url, "downloadUrl":url, "uploadUrl":url, "username":"user", "state":"s"
        }))
        .unwrap();
        let auth = AuthenticatedSession {
            capabilities: GatewayCapabilities::from_session(&session),
            session,
            authorization: BasicAuthorization {
                username: "user".into(),
                password: "password".into(),
            },
        };
        let client = JmapClient::new(
            Config::from_env().unwrap(),
            Arc::new(crate::state::MemoryStateStore::default()),
        )
        .unwrap();
        let fields = crate::wbxml::eas::CalendarFields {
            disallow_new_time_proposal: Some(true),
            ..Default::default()
        };
        let error = client
            .save_calendar_event(&auth, "calendar", Some("event-1"), None, &fields, false)
            .await
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("did not retain DisallowNewTimeProposal"));
        assert_eq!(requests.load(Ordering::SeqCst), 3);
        server.abort();
    }

    #[tokio::test]
    async fn calendar_create_persists_and_verifies_disallow_new_time_proposal() {
        let requests = Arc::new(AtomicUsize::new(0));
        let request_count = requests.clone();
        let app = Router::new().route(
            "/",
            post(move |Json(request): Json<serde_json::Value>| {
                let request_count = request_count.clone();
                async move {
                    let call = &request["methodCalls"][0];
                    match request_count.fetch_add(1, Ordering::SeqCst) {
                        0 => {
                            assert_eq!(call[0], "CalendarEvent/set");
                            assert_eq!(
                                call[1]["create"]["e1"]["iCalendar"]["properties"][0][0],
                                "X-MICROSOFT-DISALLOW-COUNTER"
                            );
                            assert_eq!(
                                call[1]["create"]["e1"]["iCalendar"]["properties"][0][3],
                                true
                            );
                            Json(serde_json::json!({"methodResponses":[[
                                "CalendarEvent/set", {"created":{"e1":{"id":"event-1"}}}, "0"
                            ]]}))
                        }
                        1 => {
                            assert_eq!(call[0], "CalendarEvent/get");
                            Json(serde_json::json!({"methodResponses":[[
                                "CalendarEvent/get", {"list":[{
                                    "id":"event-1", "iCalendar":{"properties":[
                                        ["X-MICROSOFT-DISALLOW-COUNTER", {}, "boolean", true]
                                    ]}
                                }]}, "verify-disallow-counter"
                            ]]}))
                        }
                        _ => panic!("unexpected JMAP call"),
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let session: JmapSession = serde_json::from_value(serde_json::json!({
            "capabilities":{}, "primaryAccounts":{capabilities::CALENDARS:"a"},
            "apiUrl":url, "downloadUrl":url, "uploadUrl":url, "username":"user", "state":"s"
        }))
        .unwrap();
        let auth = AuthenticatedSession {
            capabilities: GatewayCapabilities::from_session(&session),
            session,
            authorization: BasicAuthorization {
                username: "user".into(),
                password: "password".into(),
            },
        };
        let client = JmapClient::new(
            Config::from_env().unwrap(),
            Arc::new(crate::state::MemoryStateStore::default()),
        )
        .unwrap();
        let fields = crate::wbxml::eas::CalendarFields {
            disallow_new_time_proposal: Some(true),
            ..Default::default()
        };
        assert_eq!(
            client
                .save_calendar_event(&auth, "calendar", None, None, &fields, false)
                .await
                .unwrap(),
            "event-1"
        );
        assert_eq!(requests.load(Ordering::SeqCst), 2);
        server.abort();
    }

    #[tokio::test]
    async fn calendar_create_maps_recurrence_and_response_requested() {
        let app = Router::new().route(
            "/",
            post(|Json(request): Json<serde_json::Value>| async move {
                let call = &request["methodCalls"][0];
                assert_eq!(call[0], "CalendarEvent/set");
                assert_eq!(
                    call[1]["create"]["e1"]["recurrenceRule"],
                    serde_json::json!({
                        "frequency":"weekly", "interval":2,
                        "byDay":[{"day":"mo"},{"day":"we"}], "count":12
                    })
                );
                assert_eq!(
                    call[1]["create"]["e1"]["participants"]["att0"]["expectReply"],
                    false
                );
                Json(serde_json::json!({"methodResponses":[[
                    "CalendarEvent/set", {"created":{"e1":{"id":"event-1"}}}, "0"
                ]]}))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let session: JmapSession = serde_json::from_value(serde_json::json!({
            "capabilities":{}, "primaryAccounts":{capabilities::CALENDARS:"a"},
            "apiUrl":url, "downloadUrl":url, "uploadUrl":url, "username":"user", "state":"s"
        }))
        .unwrap();
        let auth = AuthenticatedSession {
            capabilities: GatewayCapabilities::from_session(&session),
            session,
            authorization: BasicAuthorization {
                username: "user".into(),
                password: "password".into(),
            },
        };
        let client = JmapClient::new(
            Config::from_env().unwrap(),
            Arc::new(crate::state::MemoryStateStore::default()),
        )
        .unwrap();
        let fields = crate::wbxml::eas::CalendarFields {
            recurrence_present: true,
            recurrence: Some(crate::wbxml::eas::CalendarRecurrence {
                kind: 1,
                interval: Some(2),
                day_of_week: Some(10),
                occurrences: Some(12),
                ..Default::default()
            }),
            response_requested: Some(false),
            attendees: vec![crate::wbxml::eas::AttendeeField {
                email: "guest@example.test".to_owned(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(
            client
                .save_calendar_event(&auth, "calendar", None, None, &fields, false)
                .await
                .unwrap(),
            "event-1"
        );
        server.abort();
    }

    #[tokio::test]
    async fn calendar_response_requested_updates_existing_attendees_only() {
        let requests = Arc::new(AtomicUsize::new(0));
        let request_count = requests.clone();
        let app = Router::new().route(
            "/",
            post(move |Json(request): Json<serde_json::Value>| {
                let request_count = request_count.clone();
                async move {
                    let call = &request["methodCalls"][0];
                    match request_count.fetch_add(1, Ordering::SeqCst) {
                        0 => {
                            assert_eq!(call[0], "CalendarEvent/get");
                            Json(serde_json::json!({"methodResponses":[[
                                "CalendarEvent/get", {"state":"state-1","list":[{"id":"event-1","timeZone":"UTC"}]}, "state"
                            ]]}))
                        }
                        1 => {
                            assert_eq!(call[0], "CalendarEvent/get");
                            Json(serde_json::json!({"methodResponses":[[
                                "CalendarEvent/get", {"list":[{"id":"event-1","participants":{
                                    "owner":{"email":"owner@example.test","roles":{"owner":true}},
                                    "guest":{"email":"guest@example.test","roles":{"attendee":true}}
                                }}]}, "response-requested"
                            ]]}))
                        }
                        2 => {
                            assert_eq!(call[0], "CalendarEvent/set");
                            assert_eq!(call[1]["ifInState"], "state-1");
                            let update = &call[1]["update"]["event-1"];
                            assert_eq!(update["participants/guest/expectReply"], false);
                            assert!(update.get("participants/owner/expectReply").is_none());
                            Json(serde_json::json!({"methodResponses":[[
                                "CalendarEvent/set", {"updated":{"event-1":null}}, "0"
                            ]]}))
                        }
                        _ => panic!("unexpected extra JMAP request"),
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let session: JmapSession = serde_json::from_value(serde_json::json!({
            "capabilities":{}, "primaryAccounts":{capabilities::CALENDARS:"a"},
            "apiUrl":url, "downloadUrl":url, "uploadUrl":url, "username":"user", "state":"s"
        }))
        .unwrap();
        let auth = AuthenticatedSession {
            capabilities: GatewayCapabilities::from_session(&session),
            session,
            authorization: BasicAuthorization {
                username: "user".into(),
                password: "password".into(),
            },
        };
        let client = JmapClient::new(
            Config::from_env().unwrap(),
            Arc::new(crate::state::MemoryStateStore::default()),
        )
        .unwrap();
        let fields = crate::wbxml::eas::CalendarFields {
            response_requested: Some(false),
            ..Default::default()
        };
        assert_eq!(
            client
                .save_calendar_event(&auth, "calendar", Some("event-1"), None, &fields, false)
                .await
                .unwrap(),
            "event-1"
        );
        assert_eq!(requests.load(Ordering::SeqCst), 3);
        server.abort();
    }

    #[tokio::test]
    async fn calendar_attachment_download_uses_calendar_account() {
        let app = Router::new().route(
            "/download/:account_id/:blob_id/:name",
            axum::routing::get(
                |axum::extract::Path((account_id, blob_id, name)): axum::extract::Path<(
                    String,
                    String,
                    String,
                )>| async move {
                    assert_eq!(account_id, "calendar-account");
                    assert_eq!(blob_id, "blob-123");
                    assert_eq!(name, "occurrence.pdf");
                    ([("content-type", "application/pdf")], b"pdf bytes".to_vec())
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let session: JmapSession = serde_json::from_value(serde_json::json!({
            "capabilities":{}, "primaryAccounts":{
                capabilities::MAIL:"mail-account", capabilities::CALENDARS:"calendar-account"
            },
            "apiUrl":url,
            "downloadUrl":format!("{url}download/{{accountId}}/{{blobId}}/{{name}}?type={{type}}"),
            "uploadUrl":url, "username":"user", "state":"s"
        }))
        .unwrap();
        let auth = AuthenticatedSession {
            capabilities: GatewayCapabilities::from_session(&session),
            session,
            authorization: BasicAuthorization {
                username: "user".into(),
                password: "password".into(),
            },
        };
        let client = JmapClient::new(
            Config::from_env().unwrap(),
            Arc::new(crate::state::MemoryStateStore::default()),
        )
        .unwrap();
        let (bytes, content_type) = client
            .download_calendar_blob_with_type(&auth, "blob-123", "occurrence.pdf")
            .await
            .unwrap();
        assert_eq!(bytes, b"pdf bytes");
        assert_eq!(content_type, "application/pdf");
        server.abort();
    }

    #[tokio::test]
    async fn calendar_16_series_rule_change_clears_existing_occurrence_overrides() {
        let requests = Arc::new(AtomicUsize::new(0));
        let request_count = requests.clone();
        let app = Router::new().route(
            "/",
            post(move |Json(request): Json<serde_json::Value>| {
                let request_count = request_count.clone();
                async move {
                    let call = &request["methodCalls"][0];
                    match request_count.fetch_add(1, Ordering::SeqCst) {
                        0 => {
                            assert_eq!(call[0], "CalendarEvent/get");
                            Json(serde_json::json!({"methodResponses":[[
                                "CalendarEvent/get", {
                                    "state":"state-1",
                                    "list":[{"id":"event-1", "timeZone":"America/Toronto",
                                        "recurrenceRule":{"frequency":"weekly"},
                                        "recurrenceOverrides":{"2026-11-25T12:00:00":{"title":"exception"}}}]
                                }, "state"
                            ]]}))
                        }
                        1 => {
                            assert_eq!(call[0], "CalendarEvent/set");
                            assert_eq!(call[1]["ifInState"], "state-1");
                            let update = &call[1]["update"]["event-1"];
                            assert!(update["recurrenceRule"].is_object());
                            assert_eq!(update["recurrenceOverrides"], serde_json::Value::Null);
                            Json(serde_json::json!({"methodResponses":[[
                                "CalendarEvent/set", {"updated":{"event-1":null}}, "0"
                            ]]}))
                        }
                        _ => panic!("unexpected JMAP call"),
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let session: JmapSession = serde_json::from_value(serde_json::json!({
            "capabilities":{}, "primaryAccounts":{capabilities::CALENDARS:"a"},
            "apiUrl":url, "downloadUrl":url, "uploadUrl":url, "username":"user", "state":"s"
        }))
        .unwrap();
        let auth = AuthenticatedSession {
            capabilities: GatewayCapabilities::from_session(&session),
            session,
            authorization: BasicAuthorization {
                username: "user".into(),
                password: "password".into(),
            },
        };
        let client = JmapClient::new(
            Config::from_env().unwrap(),
            Arc::new(crate::state::MemoryStateStore::default()),
        )
        .unwrap();
        let fields = crate::wbxml::eas::CalendarFields {
            recurrence_present: true,
            recurrence: Some(crate::wbxml::eas::CalendarRecurrence {
                kind: 1,
                interval: Some(2),
                day_of_week: Some(10),
                ..Default::default()
            }),
            ..Default::default()
        };
        client
            .save_calendar_event(&auth, "calendar", Some("event-1"), None, &fields, true)
            .await
            .unwrap();
        assert_eq!(requests.load(Ordering::SeqCst), 2);
        server.abort();
    }

    #[tokio::test]
    async fn calendar_instance_delete_patches_an_excluded_recurrence_override() {
        let requests = Arc::new(AtomicUsize::new(0));
        let request_count = requests.clone();
        let app = Router::new().route(
            "/",
            post(move |Json(request): Json<serde_json::Value>| {
                let request_count = request_count.clone();
                async move {
                    let call = &request["methodCalls"][0];
                    match request_count.fetch_add(1, Ordering::SeqCst) {
                        0 => {
                            assert_eq!(call[0], "CalendarEvent/get");
                            Json(serde_json::json!({"methodResponses":[[
                                "CalendarEvent/get", {
                                    "state":"state-1",
                                    "list":[{"id":"event-1", "timeZone":"America/Toronto",
                                        "recurrenceRule":{"frequency":"weekly"}}]
                                }, "instance"
                            ]]}))
                        }
                        1 => {
                            assert_eq!(call[0], "CalendarEvent/set");
                            assert_eq!(call[1]["ifInState"], "state-1");
                            assert_eq!(
                                call[1]["update"]["event-1"]
                                    ["recurrenceOverrides/2026-11-15T10:00:00"],
                                serde_json::json!({"excluded":true})
                            );
                            Json(serde_json::json!({"methodResponses":[[
                                "CalendarEvent/set", {"updated":{"event-1":null}}, "exclude"
                            ]]}))
                        }
                        _ => panic!("unexpected JMAP call"),
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let session: JmapSession = serde_json::from_value(serde_json::json!({
            "capabilities":{}, "primaryAccounts":{capabilities::CALENDARS:"a"},
            "apiUrl":url, "downloadUrl":url, "uploadUrl":url, "username":"user", "state":"s"
        }))
        .unwrap();
        let auth = AuthenticatedSession {
            capabilities: GatewayCapabilities::from_session(&session),
            session,
            authorization: BasicAuthorization {
                username: "user".into(),
                password: "password".into(),
            },
        };
        let client = JmapClient::new(
            Config::from_env().unwrap(),
            Arc::new(crate::state::MemoryStateStore::default()),
        )
        .unwrap();
        client
            .exclude_calendar_event_instance(&auth, "event-1", "2026-11-15T15:00:00.000Z")
            .await
            .unwrap();
        assert_eq!(requests.load(Ordering::SeqCst), 2);
        server.abort();
    }

    #[tokio::test]
    async fn calendar_instance_change_patches_supported_exception_fields() {
        let requests = Arc::new(AtomicUsize::new(0));
        let request_count = requests.clone();
        let app = Router::new().route(
            "/",
            post(move |Json(request): Json<serde_json::Value>| {
                let request_count = request_count.clone();
                async move {
                    let call = &request["methodCalls"][0];
                    match request_count.fetch_add(1, Ordering::SeqCst) {
                        0 => Json(serde_json::json!({"methodResponses":[[
                            "CalendarEvent/get", {
                                "state":"state-2",
                                "list":[{"id":"event-2", "timeZone":"America/Toronto",
                                    "recurrenceRule":{"frequency":"weekly"},
                                    "alerts":{"display":{"@type":"Alert","trigger":{"@type":"OffsetTrigger","relativeTo":"start","offset":"-PT30M"}}},
                                    "participants":{"owner":{"email":"owner@example.test","roles":{"owner":true}},
                                        "guest":{"email":"guest@example.test","roles":{"attendee":true},"participationStatus":"accepted"}}}]
                            }, "instance"
                        ]]})),
                        1 => {
                            assert_eq!(call[0], "CalendarEvent/set");
                            assert_eq!(call[1]["ifInState"], "state-2");
                            let patch = &call[1]["update"]["event-2"];
                            assert_eq!(
                                patch["recurrenceOverrides/2026-11-15T10:00:00/title"],
                                "New title"
                            );
                            assert_eq!(
                                patch["recurrenceOverrides/2026-11-15T10:00:00/description"],
                                "Body"
                            );
                            assert_eq!(
                                patch["recurrenceOverrides/2026-11-15T10:00:00/descriptionContentType"],
                                "text/html"
                            );
                            assert_eq!(
                                patch["recurrenceOverrides/2026-11-15T10:00:00/locations"],
                                serde_json::json!({"loc1":{"name":"New room", "coordinates":"geo:43.65,-79.38"}})
                            );
                            assert_eq!(patch["recurrenceOverrides/2026-11-15T10:00:00/start"], "2026-11-15T13:00:00");
                            assert_eq!(patch["recurrenceOverrides/2026-11-15T10:00:00/duration"], "PT3600S");
                            assert_eq!(patch["recurrenceOverrides/2026-11-15T10:00:00/showWithoutTime"], true);
                            assert_eq!(patch["recurrenceOverrides/2026-11-15T10:00:00/freeBusyStatus"], "x-eas-out-of-office");
                            assert_eq!(patch["recurrenceOverrides/2026-11-15T10:00:00/keywords"], serde_json::json!({}));
                            assert_eq!(patch["recurrenceOverrides/2026-11-15T10:00:00/alerts/display/trigger"], serde_json::json!({"@type":"OffsetTrigger","relativeTo":"start","offset":"-PT15M"}));
                            assert_eq!(patch["recurrenceOverrides/2026-11-15T10:00:00/participants"]["owner"]["email"], "owner@example.test");
                            assert_eq!(patch["recurrenceOverrides/2026-11-15T10:00:00/participants"]["guest"]["participationStatus"], "tentative");
                            assert_eq!(patch["recurrenceOverrides/2026-11-15T10:00:00/participants"]["guest"]["roles"]["optional"], true);
                            Json(serde_json::json!({"methodResponses":[[
                                "CalendarEvent/set", {"updated":{"event-2":null}}, "instance-change"
                            ]]}))
                        }
                        _ => panic!("unexpected JMAP call"),
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let session: JmapSession = serde_json::from_value(serde_json::json!({
            "capabilities":{}, "primaryAccounts":{capabilities::CALENDARS:"a"},
            "apiUrl":url, "downloadUrl":url, "uploadUrl":url, "username":"user", "state":"s"
        }))
        .unwrap();
        let auth = AuthenticatedSession {
            capabilities: GatewayCapabilities::from_session(&session),
            session,
            authorization: BasicAuthorization {
                username: "user".into(),
                password: "password".into(),
            },
        };
        let client = JmapClient::new(
            Config::from_env().unwrap(),
            Arc::new(crate::state::MemoryStateStore::default()),
        )
        .unwrap();
        let fields = crate::wbxml::eas::CalendarFields {
            subject: Some("New title".to_owned()),
            body: Some("Body".to_owned()),
            body_type: Some(2),
            location: Some("New room".to_owned()),
            location_coordinates: Some(("43.65".to_owned(), "-79.38".to_owned())),
            start_time: Some("20261115T180000Z".to_owned()),
            end_time: Some("20261115T190000Z".to_owned()),
            all_day_event: Some(true),
            busy_status: Some(3),
            categories: Some(Vec::new()),
            reminder: Some(Some(15)),
            attendees_present: true,
            attendees: vec![crate::wbxml::eas::AttendeeField {
                email: "guest@example.test".to_owned(),
                name: Some("Guest".to_owned()),
                optional: true,
                status: Some(2),
            }],
            ..Default::default()
        };
        client
            .save_calendar_event_instance(&auth, "event-2", "2026-11-15T15:00:00Z", &fields)
            .await
            .unwrap();
        assert_eq!(requests.load(Ordering::SeqCst), 2);
        server.abort();
    }

    #[tokio::test]
    async fn tasks_in_calendar_requests_and_parses_recurrence_rule() {
        let app = Router::new().route(
            "/",
            post(|Json(request): Json<serde_json::Value>| async move {
                let calls = request["methodCalls"].as_array().unwrap();
                assert_eq!(calls[0][0], "CalendarEvent/query");
                assert_eq!(calls[1][0], "CalendarEvent/get");
                assert!(calls[1][1]["properties"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|property| property == "recurrenceRule"));
                Json(serde_json::json!({"methodResponses":[
                    ["CalendarEvent/query", {"queryState":"q1","ids":["task-1"]}, "q"],
                    ["CalendarEvent/get", {"list":[{
                        "id":"task-1","@type":"Task","title":"Recurring task",
                        "progress":"needs-action","start":"2026-11-24T09:00:00",
                        "recurrenceRule":{"frequency":"daily","interval":2,"count":3}
                    }]}, "g"]
                ]}))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let session: JmapSession = serde_json::from_value(serde_json::json!({
            "capabilities":{}, "primaryAccounts":{capabilities::CALENDARS:"a"},
            "apiUrl":url, "downloadUrl":url, "uploadUrl":url, "username":"user", "state":"s"
        }))
        .unwrap();
        let auth = AuthenticatedSession {
            capabilities: GatewayCapabilities::from_session(&session),
            session,
            authorization: BasicAuthorization {
                username: "user".into(),
                password: "password".into(),
            },
        };
        let client = JmapClient::new(
            Config::from_env().unwrap(),
            Arc::new(crate::state::MemoryStateStore::default()),
        )
        .unwrap();
        let tasks = client
            .tasks_in_calendar(&auth, "calendar", 1)
            .await
            .unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].title, "Recurring task");
        assert_eq!(
            tasks[0].recurrence,
            Some(crate::wbxml::eas::CalendarRecurrence {
                kind: 0,
                interval: Some(2),
                occurrences: Some(3),
                ..Default::default()
            })
        );
        server.abort();
    }

    #[tokio::test]
    async fn task_create_sends_a_jscalendar_recurrence_rule() {
        let app = Router::new().route(
            "/",
            post(|Json(request): Json<serde_json::Value>| async move {
                let call = &request["methodCalls"][0];
                assert_eq!(call[0], "CalendarEvent/set");
                assert_eq!(call[1]["create"]["t1"]["@type"], "Task");
                assert_eq!(
                    call[1]["create"]["t1"]["recurrenceRule"],
                    serde_json::json!({
                        "frequency":"weekly", "byDay":[{"day":"mo"},{"day":"we"}], "count":8
                    })
                );
                Json(serde_json::json!({"methodResponses":[[
                    "CalendarEvent/set", {"created":{"t1":{"id":"task-1"}}}, "0"
                ]]}))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let session: JmapSession = serde_json::from_value(serde_json::json!({
            "capabilities":{}, "primaryAccounts":{capabilities::CALENDARS:"a"},
            "apiUrl":url, "downloadUrl":url, "uploadUrl":url, "username":"user", "state":"s"
        }))
        .unwrap();
        let auth = AuthenticatedSession {
            capabilities: GatewayCapabilities::from_session(&session),
            session,
            authorization: BasicAuthorization {
                username: "user".into(),
                password: "password".into(),
            },
        };
        let client = JmapClient::new(
            Config::from_env().unwrap(),
            Arc::new(crate::state::MemoryStateStore::default()),
        )
        .unwrap();
        let fields = crate::wbxml::eas::TaskFields {
            recurrence_present: true,
            recurrence: Some(crate::wbxml::eas::CalendarRecurrence {
                kind: 1,
                day_of_week: Some(10),
                occurrences: Some(8),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            client
                .save_task(&auth, "calendar", None, &fields)
                .await
                .unwrap(),
            "task-1"
        );
        server.abort();
    }

    #[tokio::test]
    async fn location_update_rejects_a_changed_calendar_state() {
        let calls = Arc::new(AtomicUsize::new(0));
        let captured = calls.clone();
        let app = Router::new().route("/", post(move |Json(request): Json<serde_json::Value>| {
            let calls = captured.clone();
            async move {
                let call = &request["methodCalls"][0];
                let number = calls.fetch_add(1, Ordering::SeqCst);
                let response = match number {
                    0 => {
                        assert_eq!(call[0], "CalendarEvent/get");
                        serde_json::json!(["CalendarEvent/get", {"state":"before","list":[{"id":"e"}]}, "state"])
                    }
                    1 => {
                        assert_eq!(call[0], "CalendarEvent/get");
                        serde_json::json!(["CalendarEvent/get", {"state":"after","list":[{"id":"e","locations":{"room":{"name":"External change"}}}]}, "0"])
                    }
                    2 => {
                        assert_eq!(call[0], "CalendarEvent/set");
                        assert_eq!(call[1]["ifInState"], "before");
                        serde_json::json!(["error", {"type":"stateMismatch"}, "0"])
                    }
                    _ => panic!("unexpected JMAP request"),
                };
                Json(serde_json::json!({"methodResponses":[response]}))
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let session: JmapSession = serde_json::from_value(serde_json::json!({
            "capabilities":{},
            "primaryAccounts":{capabilities::CALENDARS:"a"},
            "apiUrl":url,
            "downloadUrl":url,
            "uploadUrl":url,
            "username":"user",
            "state":"s"
        }))
        .unwrap();
        let auth = AuthenticatedSession {
            capabilities: GatewayCapabilities::from_session(&session),
            session,
            authorization: BasicAuthorization {
                username: "user".into(),
                password: "password".into(),
            },
        };
        let client = JmapClient::new(
            Config::from_env().unwrap(),
            Arc::new(crate::state::MemoryStateStore::default()),
        )
        .unwrap();
        let fields = crate::wbxml::eas::CalendarFields {
            location: Some("Client change".into()),
            ..Default::default()
        };
        assert!(client
            .save_calendar_event(&auth, "calendar", Some("e"), None, &fields, false)
            .await
            .is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        server.abort();
    }
}

#[cfg(test)]
mod contact_address_tests {
    use super::*;
    use axum::{routing::post, Json, Router};
    use std::sync::Arc;

    #[tokio::test]
    async fn contact_address_sync_maps_eas_contexts_to_jscontact() {
        let app = Router::new().route(
            "/",
            post(|Json(request): Json<serde_json::Value>| async move {
                let call = &request["methodCalls"][0];
                assert_eq!(call[0], "ContactCard/set");
                assert_eq!(
                    call[1]["create"]["c1"]["addresses"],
                    serde_json::json!({
                        "work":{"@type":"Address","contexts":{"work":true},"components":[
                            {"kind":"locality","value":"Toronto"},
                            {"kind":"country","value":"Canada"},
                            {"kind":"postcode","value":"M5V 2T6"},
                            {"kind":"region","value":"Ontario"},
                            {"kind":"name","value":"100 King St W"}
                        ],"isOrdered":false},
                        "home":{"@type":"Address","contexts":{"private":true},"components":[
                            {"kind":"locality","value":"Ottawa"}
                        ],"isOrdered":false},
                        "other":{"@type":"Address","contexts":{"other":true},"components":[
                            {"kind":"locality","value":"Kingston"}
                        ],"isOrdered":false},
                        "eas_office_location":{"@type":"Address","label":"office location","full":"Building 4, Floor 2"}
                    })
                );
                assert_eq!(call[1]["create"]["c1"]["nicknames"], serde_json::json!({"eas_nickname":{"name":"Enchantress of Numbers"}}));
                assert_eq!(call[1]["create"]["c1"]["media"], serde_json::json!({"eas_picture":{"@type":"Media","kind":"photo","uri":"data:image/png;base64,iVBORw0KGgo="}}));
                assert_eq!(call[1]["create"]["c1"]["onlineServices"], serde_json::json!({"eas_im1":{"@type":"OnlineService","user":"ada@example.im","label":"EAS IM Address 1"},"eas_im2":{"@type":"OnlineService","user":"ada@chat.example","label":"EAS IM Address 2"},"eas_im3":{"@type":"OnlineService","user":"ada@xmpp.example","label":"EAS IM Address 3"},"eas_mms":{"@type":"OnlineService","user":"ada@mms.example","label":"EAS MMS"},"eas_account_name":{"@type":"OnlineService","user":"ada-eas-account","label":"EAS account name"}}));
                assert_eq!(
                    call[1]["create"]["c1"]["keywords"],
                    serde_json::json!({"Work":true,"Friends":true})
                );
                assert_eq!(
                    call[1]["create"]["c1"]["name"]["components"],
                    serde_json::json!([
                        {"kind":"given","value":"Ada","phonetic":"エイダ"},
                        {"kind":"given2","value":"Augusta"},
                        {"kind":"surname","value":"Lovelace","phonetic":"ラブレス"},
                        {"kind":"generation","value":"Jr."},
                        {"kind":"prefix","value":"Countess"}
                    ])
                );
                assert_eq!(call[1]["create"]["c1"]["name"]["phoneticScript"], "Jpan");
                assert_eq!(
                    call[1]["create"]["c1"]["anniversaries"],
                    serde_json::json!({
                        "eas_birthday":{"@type":"Anniversary","kind":"birth","date":{"year":1815,"month":12,"day":10}},
                        "eas_anniversary":{"@type":"Anniversary","kind":"wedding","date":{"year":1835,"month":11,"day":8}}
                    })
                );
                assert_eq!(
                    call[1]["create"]["c1"]["organizations"],
                    serde_json::json!({"o1":{"name":"Analytical Engines Ltd","sortAs":"アナリティカル エンジンズ","units":[{"@type":"OrgUnit","name":"Research"}]}})
                );
                assert_eq!(
                    call[1]["create"]["c1"]["phones"],
                    serde_json::json!({
                        "p_mobile":{"number":"555-0100","contexts":{"mobile":true}},
                        "p_home":{"number":"555-0101","contexts":{"home":true}},
                        "p_home2":{"number":"555-0102","contexts":{"home":true}},
                        "p_work":{"number":"555-0103","contexts":{"work":true}},
                        "p_work2":{"number":"555-0104","contexts":{"work":true}},
                        "p_home_fax":{"number":"555-0105","contexts":{"private":true},"features":{"fax":true}},
                        "p_work_fax":{"number":"555-0109","contexts":{"work":true},"features":{"fax":true}},
                        "p_pager":{"number":"555-0106","features":{"pager":true}},
                        "p_car":{"number":"555-0107","label":"car"},
                        "p_radio":{"number":"555-0108","label":"radio"},
                        "p_assistant":{"number":"555-0110","label":"assistant"},
                        "eas_company_main":{"number":"555-0199","features":{"main-number":true}}
                    })
                );
                assert_eq!(
                    call[1]["create"]["c1"]["links"],
                    serde_json::json!({"eas_web":{"uri":"https://example.com/ada"}})
                );
                assert_eq!(
                    call[1]["create"]["c1"]["notes"],
                    serde_json::json!({"eas_notes":{"@type":"Note","note":"Contact notes"}})
                );
                Json(serde_json::json!({"methodResponses":[
                    ["ContactCard/set",{"created":{"c1":{"id":"contact-1"}}},"0"]
                ]}))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let session: JmapSession = serde_json::from_value(serde_json::json!({
            "capabilities":{}, "primaryAccounts":{capabilities::CONTACTS:"a"}, "apiUrl":url,
            "downloadUrl":url,"uploadUrl":url,"username":"user","state":"s"
        }))
        .unwrap();
        let auth = AuthenticatedSession {
            capabilities: GatewayCapabilities::from_session(&session),
            session,
            authorization: BasicAuthorization {
                username: "user".into(),
                password: "password".into(),
            },
        };
        let client = JmapClient::new(
            Config::from_env().unwrap(),
            Arc::new(crate::state::MemoryStateStore::default()),
        )
        .unwrap();
        let fields = crate::wbxml::eas::ContactFields {
            first_name: Some("Ada".into()),
            middle_name: Some("Augusta".into()),
            last_name: Some("Lovelace".into()),
            suffix: Some("Jr.".into()),
            name_prefix: Some("Countess".into()),
            yomi_first_name: Some("エイダ".into()),
            yomi_last_name: Some("ラブレス".into()),
            birthday: Some("18151210".into()),
            anniversary: Some("18351108".into()),
            company_name: Some("Analytical Engines Ltd".into()),
            yomi_company_name: Some("アナリティカル エンジンズ".into()),
            department: Some("Research".into()),
            mobile_phone_number: Some("555-0100".into()),
            home_phone_number: Some("555-0101".into()),
            home_2_phone_number: Some("555-0102".into()),
            business_phone_number: Some("555-0103".into()),
            business_2_phone_number: Some("555-0104".into()),
            home_fax_number: Some("555-0105".into()),
            business_fax_number: Some("555-0109".into()),
            pager_number: Some("555-0106".into()),
            car_phone_number: Some("555-0107".into()),
            radio_phone_number: Some("555-0108".into()),
            assistant_phone_number: Some("555-0110".into()),
            web_page: Some("https://example.com/ada".into()),
            business_address_city: Some("Toronto".into()),
            business_address_country: Some("Canada".into()),
            business_address_postal_code: Some("M5V 2T6".into()),
            business_address_state: Some("Ontario".into()),
            business_address_street: Some("100 King St W".into()),
            home_address_city: Some("Ottawa".into()),
            other_address_city: Some("Kingston".into()),
            office_location: Some("Building 4, Floor 2".into()),
            nickname: Some("Enchantress of Numbers".into()),
            picture: Some("iVBORw0KGgo=".into()),
            company_main_phone: Some("555-0199".into()),
            im_address: Some("ada@example.im".into()),
            im_address_2: Some("ada@chat.example".into()),
            im_address_3: Some("ada@xmpp.example".into()),
            mms: Some("ada@mms.example".into()),
            account_name: Some("ada-eas-account".into()),
            categories: Some(vec!["Work".into(), "Friends".into()]),
            notes: Some("Contact notes".into()),
            ..Default::default()
        };
        assert_eq!(
            client
                .save_contact(&auth, "book", None, &fields)
                .await
                .unwrap(),
            "contact-1"
        );
        server.abort();
    }

    #[tokio::test]
    async fn contact_notes_update_preserves_other_jscontact_notes() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let captured = calls.clone();
        let app = Router::new().route(
            "/",
            post(move |Json(request): Json<serde_json::Value>| {
                let calls = captured.clone();
                async move {
                    let call = &request["methodCalls"][0];
                    let response = match calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
                        0 => {
                            assert_eq!(call[0], "ContactCard/get");
                            assert!(call[1]["properties"]
                                .as_array()
                                .unwrap()
                                .iter()
                                .any(|property| property == "notes"));
                            serde_json::json!(["ContactCard/get", {
                                "state":"before",
                                "list":[{"id":"contact-1","notes":{
                                    "other_note":{"@type":"Note","note":"Keep this text"},
                                    "eas_notes":{"@type":"Note","note":"Old contact note"}
                                }}]
                            }, "address-state"])
                        }
                        1 => {
                            assert_eq!(call[0], "ContactCard/set");
                            assert_eq!(call[1]["ifInState"], "before");
                            assert_eq!(call[1]["update"]["contact-1"]["notes"], serde_json::json!({
                                "other_note":{"@type":"Note","note":"Keep this text"},
                                "eas_notes":{"@type":"Note","note":"Updated contact note"}
                            }));
                            serde_json::json!(["ContactCard/set", {"updated":{"contact-1":null}}, "0"])
                        }
                        _ => panic!("unexpected JMAP request"),
                    };
                    Json(serde_json::json!({"methodResponses":[response]}))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let session: JmapSession = serde_json::from_value(serde_json::json!({
            "capabilities":{}, "primaryAccounts":{capabilities::CONTACTS:"a"}, "apiUrl":url,
            "downloadUrl":url,"uploadUrl":url,"username":"user","state":"s"
        }))
        .unwrap();
        let auth = AuthenticatedSession {
            capabilities: GatewayCapabilities::from_session(&session),
            session,
            authorization: BasicAuthorization {
                username: "user".into(),
                password: "password".into(),
            },
        };
        let client = JmapClient::new(
            Config::from_env().unwrap(),
            Arc::new(crate::state::MemoryStateStore::default()),
        )
        .unwrap();
        let fields = crate::wbxml::eas::ContactFields {
            notes: Some("Updated contact note".into()),
            ..Default::default()
        };
        assert_eq!(
            client
                .save_contact(&auth, "book", Some("contact-1"), &fields)
                .await
                .unwrap(),
            "contact-1"
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        server.abort();
    }

    #[tokio::test]
    async fn contact_address_change_preserves_unmodified_components_and_uses_state_guard() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let captured = calls.clone();
        let app = Router::new().route(
            "/",
            post(move |Json(request): Json<serde_json::Value>| {
                let calls = captured.clone();
                async move {
                    let call = &request["methodCalls"][0];
                    match calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
                        0 => {
                            assert_eq!(call[0], "ContactCard/get");
                            Json(serde_json::json!({"methodResponses":[[
                            "ContactCard/get", {"state":"before","list":[{"id":"c","nicknames":{"eas_nickname":{"name":"Enchantress"},"other":{"name":"Lady Ada"}},"media":{"eas_picture":{"@type":"Media","kind":"photo","uri":"data:image/png;base64,iVBORw0KGgo="},"other":{"kind":"logo","uri":"https://example.test/logo.png"}},"onlineServices":{"eas_im1":{"@type":"OnlineService","user":"old-im","label":"EAS IM Address 1"},"eas_mms":{"@type":"OnlineService","user":"old@mms.example","label":"EAS MMS"},"eas_account_name":{"@type":"OnlineService","user":"old-account","label":"EAS account name"},"other":{"user":"other-service","service":"Matrix"}},"name":{"full":"Ada Lovelace Jr.","phoneticScript":"Jpan","components":[
                                {"kind":"given","value":"Ada","phonetic":"エイダ"},
                                {"kind":"given2","value":"Augusta"},
                                {"kind":"surname","value":"Lovelace","phonetic":"ラブレス"},
                                {"kind":"generation","value":"Jr."},
                                {"kind":"prefix","value":"Countess"}
                            ]},"links":{"site":{"uri":"https://old.example/"},"mail":{"uri":"mailto:ada@example.com"}},"phones":{"work1":{"number":"555-0101","contexts":{"work":true}},"work2":{"number":"555-0102","contexts":{"work":true}},"fax":{"number":"555-0105","contexts":{"private":true},"features":{"fax":true}},"workfax":{"number":"555-0109","contexts":{"work":true},"features":{"fax":true}},"pager":{"number":"555-0106","features":{"pager":true}},"car":{"number":"555-0107","label":"car"},"radio":{"number":"555-0108","label":"radio"},"assistant":{"number":"555-0110","label":"assistant"},"eas_company_main":{"number":"555-0199","features":{"main-number":true}}},"organizations":{"org":{"name":"Analytical Engines Ltd","sortAs":"アナリティカル エンジンズ","units":[{"name":"Old Department"}]}},"anniversaries":{
                                "b":{"kind":"birth","date":{"year":1815,"month":12,"day":10}},
                                "w":{"kind":"wedding","date":{"year":1835,"month":11,"day":8}}
                            },"addresses":{
                                "office":{"label":"office location","full":"Building 1"},
                                "home":{"contexts":{"private":true},"components":[
                                    {"kind":"locality","value":"Ottawa"},
                                    {"kind":"name","value":"1 Home St"}
                                ]},
                                "work":{"contexts":{"work":true},"components":[
                                    {"kind":"locality","value":"Toronto"},
                                    {"kind":"name","value":"Old Office Rd"}
                                ]}
                            }}]},"address-state"]]}))
                        }
                        1 => {
                            assert_eq!(call[0], "ContactCard/set");
                            assert_eq!(call[1]["ifInState"], "before");
                            assert_eq!(
                                call[1]["update"]["c"]["name"]["components"],
                                serde_json::json!([
                                    {"kind":"surname","value":"Lovelace","phonetic":"ラブレス改"},
                                    {"kind":"generation","value":"Jr."},
                                    {"kind":"prefix","value":"Countess"},
                                    {"kind":"given","value":"Ada Byron","phonetic":"エイダ"},
                                    {"kind":"given2","value":"Byron"}
                                ])
                            );
                            assert_eq!(
                                call[1]["update"]["c"]["anniversaries"],
                                serde_json::json!({
                                    "b":{"@type":"Anniversary","kind":"birth","date":{"year":1815,"month":12,"day":11}},
                                    "w":{"kind":"wedding","date":{"year":1835,"month":11,"day":8}}
                                })
                            );
                            assert_eq!(call[1]["update"]["c"]["nicknames"], serde_json::json!({"eas_nickname":{"name":"Countess Ada"},"other":{"name":"Lady Ada"}}));
                            assert_eq!(call[1]["update"]["c"]["media"], serde_json::json!({"eas_picture":{"@type":"Media","kind":"photo","uri":"data:image/jpeg;base64,/9j/AA=="},"other":{"kind":"logo","uri":"https://example.test/logo.png"}}));
                            assert_eq!(call[1]["update"]["c"]["onlineServices"], serde_json::json!({"eas_im1":{"@type":"OnlineService","user":"new-im","label":"EAS IM Address 1"},"other":{"user":"other-service","service":"Matrix"},"eas_im3":{"@type":"OnlineService","user":"new-im3","label":"EAS IM Address 3"},"eas_mms":{"@type":"OnlineService","user":"new@mms.example","label":"EAS MMS"},"eas_account_name":{"@type":"OnlineService","user":"new-account","label":"EAS account name"}}));
                            assert_eq!(
                                call[1]["update"]["c"]["organizations"],
                                serde_json::json!({"org":{"name":"Analytical Engines Ltd","sortAs":"アナリティカル エンジンズ改","units":[{"@type":"OrgUnit","name":"New Department"}]}})
                            );
                            assert_eq!(
                                call[1]["update"]["c"]["phones"],
                                serde_json::json!({
                                    "work1":{"number":"555-0101","contexts":{"work":true}},
                                    "work2":{"number":"555-0104","contexts":{"work":true}},
                                    "fax":{"number":"555-0105","contexts":{"private":true},"features":{"fax":true}},
                                    "workfax":{"number":"555-0109","contexts":{"work":true},"features":{"fax":true}},
                                    "pager":{"number":"555-0107","features":{"pager":true}},
                                    "car":{"number":"555-0109","label":"car"},
                                    "radio":{"number":"555-0108","label":"radio"},
                                    "assistant":{"number":"555-0111","label":"assistant"},
                                    "eas_company_main":{"number":"555-0198","features":{"main-number":true}}
                                })
                            );
                            assert_eq!(
                                call[1]["update"]["c"]["links"],
                                serde_json::json!({
                                    "site":{"uri":"https://new.example/ada"},
                                    "mail":{"uri":"mailto:ada@example.com"}
                                })
                            );
                            let addresses = &call[1]["update"]["c"]["addresses"];
                            assert_eq!(
                                addresses["home"]["components"],
                                serde_json::json!([
                                    {"kind":"locality","value":"Vancouver"},
                                    {"kind":"name","value":"1 Home St"}
                                ])
                            );
                            assert_eq!(
                                addresses["work"]["components"],
                                serde_json::json!([
                                    {"kind":"locality","value":"Toronto"}
                                ])
                            );
                            assert_eq!(addresses["office"]["full"], "Floor 7, Tower B");
                            Json(serde_json::json!({"methodResponses":[[
                                "ContactCard/set", {"updated":{"c":null}}, "0"
                            ]]}))
                        }
                        _ => panic!("unexpected JMAP request"),
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let session: JmapSession = serde_json::from_value(serde_json::json!({
            "capabilities":{}, "primaryAccounts":{capabilities::CONTACTS:"a"}, "apiUrl":url,
            "downloadUrl":url,"uploadUrl":url,"username":"user","state":"s"
        }))
        .unwrap();
        let auth = AuthenticatedSession {
            capabilities: GatewayCapabilities::from_session(&session),
            session,
            authorization: BasicAuthorization {
                username: "user".into(),
                password: "password".into(),
            },
        };
        let client = JmapClient::new(
            Config::from_env().unwrap(),
            Arc::new(crate::state::MemoryStateStore::default()),
        )
        .unwrap();
        let fields = crate::wbxml::eas::ContactFields {
            middle_name: Some("Byron".into()),
            first_name: Some("Ada Byron".into()),
            yomi_last_name: Some("ラブレス改".into()),
            yomi_company_name: Some("アナリティカル エンジンズ改".into()),
            department: Some("New Department".into()),
            business_2_phone_number: Some("555-0104".into()),
            pager_number: Some("555-0107".into()),
            car_phone_number: Some("555-0109".into()),
            assistant_phone_number: Some("555-0111".into()),
            company_main_phone: Some("555-0198".into()),
            web_page: Some("https://new.example/ada".into()),
            birthday: Some("18151211".into()),
            office_location: Some("Floor 7, Tower B".into()),
            nickname: Some("Countess Ada".into()),
            picture: Some("/9j/AA==".into()),
            im_address: Some("new-im".into()),
            im_address_2: Some(String::new()),
            im_address_3: Some("new-im3".into()),
            mms: Some("new@mms.example".into()),
            account_name: Some("new-account".into()),
            home_address_city: Some("Vancouver".into()),
            business_address_street: Some(String::new()),
            ..Default::default()
        };
        assert_eq!(
            client
                .save_contact(&auth, "book", Some("c"), &fields)
                .await
                .unwrap(),
            "c"
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        server.abort();
    }

    #[test]
    fn contact_address_read_maps_jscontact_context_and_components() {
        let card: ContactCardObject = serde_json::from_value(serde_json::json!({
            "id":"c", "keywords":{"Work":true,"inactive":false}, "notes":{"eas_notes":{"@type":"Note","note":"Contact notes text"},"other":{"note":"Unrelated note"}}, "addresses":{"office":{"label":"office location","full":"Building 4, Floor 2"},
                "work":{"contexts":{"work":true},"components":[
                    {"kind":"locality","value":"Toronto"},
                    {"kind":"country","value":"Canada"},
                    {"kind":"postcode","value":"M5V 2T6"},
                    {"kind":"region","value":"Ontario"},
                    {"kind":"name","value":"100 King St W"}
                ]},
                "home":{"contexts":{"private":true},"components":[
                    {"kind":"locality","value":"Ottawa"}
                ]}
            }
        }))
        .unwrap();
        let contact = Contact::from(card);
        assert_eq!(
            contact.business_address,
            Some(crate::model::ContactAddress {
                city: Some("Toronto".into()),
                country: Some("Canada".into()),
                postal_code: Some("M5V 2T6".into()),
                state: Some("Ontario".into()),
                street: Some("100 King St W".into()),
            })
        );
        assert_eq!(
            contact
                .home_address
                .as_ref()
                .and_then(|a| a.city.as_deref()),
            Some("Ottawa")
        );
        assert_eq!(contact.categories, vec!["Work"]);
        assert_eq!(
            contact.notes.as_ref().map(|body| body.value.as_str()),
            Some("Contact notes text")
        );
        assert_eq!(
            contact.office_location.as_deref(),
            Some("Building 4, Floor 2")
        );
    }

    #[test]
    fn contact_name_read_maps_middle_name_and_suffix() {
        let card: ContactCardObject = serde_json::from_value(serde_json::json!({
            "id":"c", "media":{"eas_picture":{"kind":"photo","uri":"data:image/png;base64,iVBORw0KGgo="}}, "nicknames":{"eas_nickname":{"name":"Enchantress of Numbers"}}, "onlineServices":{"eas_im1":{"user":"ada@example.im"},"eas_im2":{"user":"ada@chat.example"},"eas_im3":{"user":"ada@xmpp.example"},"eas_mms":{"user":"ada@mms.example"},"eas_account_name":{"user":"ada-eas-account"}}, "name":{"full":"Ada Lovelace Jr.","phoneticScript":"Jpan","components":[
                {"kind":"given","value":"Ada","phonetic":"エイダ"},
                {"kind":"given2","value":"Augusta"},
                {"kind":"surname","value":"Lovelace","phonetic":"ラブレス"},
                {"kind":"generation","value":"Jr."},
                {"kind":"prefix","value":"Countess"}
            ]}, "links":{"website":{"uri":"https://example.com/ada"},"mail":{"uri":"mailto:ada@example.com"}}, "phones":{
                "mobile":{"number":"555-0100","contexts":{"mobile":true}},
                "home1":{"number":"555-0101","contexts":{"home":true}},
                "home2":{"number":"555-0102","contexts":{"home":true}},
                "work1":{"number":"555-0103","contexts":{"work":true}},
                "work2":{"number":"555-0104","contexts":{"work":true}}
                ,"workfax":{"number":"555-0109","contexts":{"work":true},"features":{"fax":true}}
                ,"fax":{"number":"555-0105","contexts":{"private":true},"features":{"fax":true}},
                "pager":{"number":"555-0106","features":{"pager":true}},
                "car":{"number":"555-0107","label":"car"},
                "radio":{"number":"555-0108","label":"radio"},
                "assistant":{"number":"555-0110","label":"assistant"},"eas_company_main":{"number":"555-0199","features":{"main-number":true}}
            }, "organizations":{"org":{"name":"Analytical Engines Ltd","sortAs":"アナリティカル エンジンズ","units":[{"name":"Research"}]}}, "anniversaries":{
                "b":{"kind":"birth","date":{"year":1815,"month":12,"day":10}},
                "w":{"kind":"wedding","date":{"year":1835,"month":11,"day":8}}
            }
        }))
        .unwrap();
        let contact = Contact::from(card);
        assert_eq!(contact.first_name.as_deref(), Some("Ada"));
        assert_eq!(contact.middle_name.as_deref(), Some("Augusta"));
        assert_eq!(contact.last_name.as_deref(), Some("Lovelace"));
        assert_eq!(contact.suffix.as_deref(), Some("Jr."));
        assert_eq!(contact.name_prefix.as_deref(), Some("Countess"));
        assert_eq!(contact.nickname.as_deref(), Some("Enchantress of Numbers"));
        assert_eq!(
            contact.picture.as_deref(),
            Some(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a][..])
        );
        assert_eq!(contact.company_main_phone.as_deref(), Some("555-0199"));
        assert_eq!(contact.im_address1.as_deref(), Some("ada@example.im"));
        assert_eq!(contact.im_address2.as_deref(), Some("ada@chat.example"));
        assert_eq!(contact.im_address3.as_deref(), Some("ada@xmpp.example"));
        assert_eq!(contact.mms.as_deref(), Some("ada@mms.example"));
        assert_eq!(contact.account_name.as_deref(), Some("ada-eas-account"));
        assert_eq!(contact.yomi_first_name.as_deref(), Some("エイダ"));
        assert_eq!(contact.yomi_last_name.as_deref(), Some("ラブレス"));
        assert_eq!(contact.birthday.as_deref(), Some("18151210"));
        assert_eq!(contact.anniversary.as_deref(), Some("18351108"));
        assert_eq!(
            contact.company_name.as_deref(),
            Some("Analytical Engines Ltd")
        );
        assert_eq!(contact.department.as_deref(), Some("Research"));
        assert_eq!(
            contact.yomi_company_name.as_deref(),
            Some("アナリティカル エンジンズ")
        );
        assert_eq!(contact.mobile_phone.as_deref(), Some("555-0100"));
        assert_eq!(contact.home_phone.as_deref(), Some("555-0101"));
        assert_eq!(contact.home_phone2.as_deref(), Some("555-0102"));
        assert_eq!(contact.business_phone.as_deref(), Some("555-0103"));
        assert_eq!(contact.business_phone2.as_deref(), Some("555-0104"));
        assert_eq!(contact.home_fax.as_deref(), Some("555-0105"));
        assert_eq!(contact.business_fax.as_deref(), Some("555-0109"));
        assert_eq!(contact.pager.as_deref(), Some("555-0106"));
        assert_eq!(contact.car_phone.as_deref(), Some("555-0107"));
        assert_eq!(contact.radio_phone.as_deref(), Some("555-0108"));
        assert_eq!(contact.assistant_phone.as_deref(), Some("555-0110"));
        assert_eq!(contact.web_page.as_deref(), Some("https://example.com/ada"));
    }

    #[test]
    fn eas_contact_dates_round_trip_as_jscontact_partial_dates() {
        let partial = eas_contact_date_to_partial("20000229").unwrap();
        assert_eq!(partial, serde_json::json!({"year":2000,"month":2,"day":29}));
        assert_eq!(
            partial_date_to_eas_contact(&partial).as_deref(),
            Some("20000229")
        );
        assert!(eas_contact_date_to_partial("20010229").is_err());
    }
}

#[cfg(test)]
mod email_state_retry_tests {
    use super::*;
    use axum::{routing::post, Json, Router};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    #[tokio::test]
    async fn retries_a_state_mismatch_with_fresh_keywords() {
        let calls = Arc::new(AtomicUsize::new(0));
        let captured = calls.clone();
        let app = Router::new().route("/", post(move |Json(request): Json<serde_json::Value>| {
            let calls = captured.clone();
            async move {
                let call = &request["methodCalls"][0];
                let number = calls.fetch_add(1, Ordering::SeqCst);
                let response = match number {
                    0 => serde_json::json!(["Email/get", {"state":"before", "list":[{"id":"m", "keywords":{}}]}, "get"]),
                    1 => {
                        assert_eq!(call[1]["ifInState"], "before");
                        serde_json::json!(["error", {"type":"stateMismatch"}, "set"])
                    }
                    2 => serde_json::json!(["Email/get", {"state":"after", "list":[{"id":"m", "keywords":{"other-client":true}}]}, "get"]),
                    3 => {
                        assert_eq!(call[1]["ifInState"], "after");
                        assert_eq!(call[1]["update"]["m"], serde_json::json!({"keywords/$seen":true}));
                        serde_json::json!(["Email/set", {"updated":{"m":null}}, "set"])
                    }
                    4 => serde_json::json!(["Email/get", {"list":[{"id":"m", "keywords":{"other-client":true,"$seen":true}}]}, "verify"]),
                    _ => panic!("unexpected JMAP request"),
                };
                Json(serde_json::json!({"methodResponses":[response]}))
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let session: JmapSession = serde_json::from_value(serde_json::json!({
            "capabilities":{}, "primaryAccounts":{capabilities::MAIL:"a"}, "apiUrl":url,
            "downloadUrl":url, "uploadUrl":url, "username":"user", "state":"s"
        }))
        .unwrap();
        let auth = AuthenticatedSession {
            capabilities: GatewayCapabilities::from_session(&session),
            session,
            authorization: BasicAuthorization {
                username: "user".into(),
                password: "password".into(),
            },
        };
        let client = JmapClient::new(
            Config::from_env().unwrap(),
            Arc::new(crate::state::MemoryStateStore::default()),
        )
        .unwrap();
        let state = client
            .update_email_state(&auth, "m", Some(true), None)
            .await
            .unwrap();
        assert!(state.read);
        assert_eq!(calls.load(Ordering::SeqCst), 5);
        server.abort();
    }

    #[tokio::test]
    async fn flag_retry_merges_metadata_from_the_latest_state() {
        let old = crate::model::MailFlag {
            status: 2,
            fields: vec![(2, 1, "old".into())],
        };
        let latest = crate::model::MailFlag {
            status: 2,
            fields: vec![(2, 1, "external".into())],
        };
        let requested = crate::model::MailFlag {
            status: u8::MAX,
            fields: vec![(2, 2, "client".into())],
        };
        let merged = crate::model::MailFlag {
            status: 2,
            fields: vec![(2, 1, "external".into()), (2, 2, "client".into())],
        };
        let keywords = |flag: &crate::model::MailFlag| {
            let mut values = serde_json::Map::new();
            values.insert("$flagged".into(), serde_json::json!(true));
            for key in flag.keywords().unwrap() {
                values.insert(key, serde_json::json!(true));
            }
            values
        };
        let old_keys = keywords(&old);
        let latest_keys = keywords(&latest);
        let merged_keys = keywords(&merged);
        let calls = Arc::new(AtomicUsize::new(0));
        let captured = calls.clone();
        let app = Router::new().route("/", post(move |Json(request): Json<serde_json::Value>| {
            let calls = captured.clone();
            let old_keys = old_keys.clone();
            let latest_keys = latest_keys.clone();
            let merged_keys = merged_keys.clone();
            async move {
                let call = &request["methodCalls"][0];
                let number = calls.fetch_add(1, Ordering::SeqCst);
                let response = match number {
                    0 => serde_json::json!(["Email/get", {"state":"before", "list":[{"id":"m", "keywords":old_keys}]}, "get"]),
                    1 => serde_json::json!(["error", {"type":"stateMismatch"}, "set"]),
                    2 => serde_json::json!(["Email/get", {"state":"after", "list":[{"id":"m", "keywords":latest_keys}]}, "get"]),
                    3 => {
                        assert_eq!(call[1]["ifInState"], "after");
                        let patch = call[1]["update"]["m"].as_object().unwrap();
                        let mut applied = latest_keys;
                        for (path, value) in patch {
                            let key = path.strip_prefix("keywords/").unwrap();
                            if value.is_null() { applied.remove(key); } else { applied.insert(key.into(), value.clone()); }
                        }
                        assert_eq!(applied, merged_keys);
                        serde_json::json!(["Email/set", {"updated":{"m":null}}, "set"])
                    }
                    4 => serde_json::json!(["Email/get", {"list":[{"id":"m", "keywords":merged_keys}]}, "verify"]),
                    _ => panic!("unexpected JMAP request"),
                };
                Json(serde_json::json!({"methodResponses":[response]}))
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let session: JmapSession = serde_json::from_value(serde_json::json!({
            "capabilities":{}, "primaryAccounts":{capabilities::MAIL:"a"}, "apiUrl":url,
            "downloadUrl":url, "uploadUrl":url, "username":"user", "state":"s"
        }))
        .unwrap();
        let auth = AuthenticatedSession {
            capabilities: GatewayCapabilities::from_session(&session),
            session,
            authorization: BasicAuthorization {
                username: "user".into(),
                password: "password".into(),
            },
        };
        let client = JmapClient::new(
            Config::from_env().unwrap(),
            Arc::new(crate::state::MemoryStateStore::default()),
        )
        .unwrap();
        let state = client
            .update_email_state(&auth, "m", None, Some(&requested))
            .await
            .unwrap();
        assert_eq!(state.flag, Some(merged));
        assert_eq!(calls.load(Ordering::SeqCst), 5);
        server.abort();
    }
}

#[cfg(test)]
mod meeting_response_send_tests {
    use super::*;
    use axum::{routing::post, Json, Router};
    use std::sync::Arc;

    #[test]
    fn custom_meeting_reply_includes_body_and_targets_organizer() {
        let mime = meeting_response_mime(
            Some(&MeetingResponseBody {
                kind: 1,
                data: "I will attend".into(),
            }),
            None,
            MeetingResponseMimeContext {
                uid: "event-uid@example.test",
                organizer: "organizer@example.test",
                attendee: "attendee@example.test",
                participation: "accepted",
                recurrence_key: Some("2026-11-25T10:00:00"),
                instance_id: Some("2026-11-25T15:00:00Z"),
                time_zone: Some("America/Toronto"),
                title: "Planning",
                description: Some("Details"),
                location: None,
                sequence: 0,
                recurrence_rule: None,
            },
        )
        .unwrap();
        let mime = String::from_utf8(mime).unwrap();
        assert!(mime.starts_with("To: <organizer@example.test>\r\nFrom: <attendee@example.test>"));
        assert!(mime.contains("Content-Type: text/plain; charset=utf-8"));
        assert!(mime.contains("METHOD:REPLY\r\n"));
        assert!(mime.contains("ATTENDEE;PARTSTAT=ACCEPTED:mailto:attendee@example.test"));
        assert!(mime.contains("RECURRENCE-ID;TZID=America/Toronto:20261125T100000"));
        assert!(mime.contains(&STANDARD.encode("I will attend")));

        let rtf = meeting_response_mime(
            Some(&MeetingResponseBody {
                kind: 3,
                data: r"{\rtf1 Hello}".into(),
            }),
            None,
            MeetingResponseMimeContext {
                uid: "event-uid@example.test",
                organizer: "organizer@example.test",
                attendee: "attendee@example.test",
                participation: "accepted",
                recurrence_key: None,
                instance_id: None,
                time_zone: None,
                title: "Planning",
                description: None,
                location: None,
                sequence: 0,
                recurrence_rule: None,
            },
        )
        .unwrap();
        assert!(String::from_utf8(rtf)
            .unwrap()
            .contains("Content-Type: text/rtf; charset=utf-8"));
    }

    #[test]
    fn proposed_time_email_uses_counter_method_and_event_sequence() {
        let mime = meeting_response_mime(
            None,
            Some(&MeetingResponseProposal {
                start: "2026-11-25T15:00:00Z".parse().unwrap(),
                end: "2026-11-25T16:00:00Z".parse().unwrap(),
            }),
            MeetingResponseMimeContext {
                uid: "event-uid@example.test",
                organizer: "organizer@example.test",
                attendee: "attendee@example.test",
                participation: "declined",
                recurrence_key: Some("2026-11-25T10:00:00"),
                instance_id: Some("2026-11-25T15:00:00Z"),
                time_zone: None,
                title: "Planning",
                description: Some("Discuss budget"),
                location: Some("Room; Floor 2"),
                sequence: 3,
                recurrence_rule: None,
            },
        )
        .unwrap();
        let mime = String::from_utf8(mime).unwrap();
        assert!(mime.starts_with("To: <organizer@example.test>\r\nFrom: <attendee@example.test>"));
        assert!(mime.contains("Content-Type: text/calendar; method=COUNTER; charset=utf-8"));
        assert!(mime.contains("METHOD:COUNTER\r\n"));
        assert!(mime.contains("DTSTART:20261125T150000Z\r\nDTEND:20261125T160000Z"));
        assert!(mime.contains("SEQUENCE:3\r\nSUMMARY:Planning"));
        assert!(mime.contains("DESCRIPTION:Discuss budget"));
        assert!(mime.contains("LOCATION:Room\\; Floor 2\r\n"));
        assert!(mime.contains("RECURRENCE-ID:20261125T150000Z\r\n"));
        assert!(!mime.contains("RECURRENCE-ID;TZID="));
    }

    #[test]
    fn counter_recurrence_maps_supported_jscalendar_rules_to_ical_rrule() {
        let rule = serde_json::json!({
            "@type":"RecurrenceRule","frequency":"weekly","interval":2,
            "byDay":[{"@type":"NDay","day":"mo"},{"@type":"NDay","day":"we"}],
            "firstDayOfWeek":"mo","count":5
        });
        assert_eq!(
            counter_recurrence_rule(&rule, None).unwrap(),
            "FREQ=WEEKLY;INTERVAL=2;BYDAY=MO,WE;WKST=MO;COUNT=5"
        );
        let mime = meeting_response_mime(
            None,
            Some(&MeetingResponseProposal {
                start: "2026-11-25T15:00:00Z".parse().unwrap(),
                end: "2026-11-25T16:00:00Z".parse().unwrap(),
            }),
            MeetingResponseMimeContext {
                uid: "event@example.test",
                organizer: "owner@example.test",
                attendee: "user@example.test",
                participation: "tentative",
                recurrence_key: None,
                instance_id: None,
                time_zone: Some("UTC"),
                title: "Weekly review",
                description: None,
                location: None,
                sequence: 1,
                recurrence_rule: Some(&rule),
            },
        )
        .unwrap();
        assert!(String::from_utf8(mime)
            .unwrap()
            .contains("RRULE:FREQ=WEEKLY;INTERVAL=2;BYDAY=MO,WE;WKST=MO;COUNT=5\r\n"));
    }

    #[test]
    fn series_counter_requires_utc_and_no_recurrence_exceptions() {
        let base = serde_json::json!({
            "timeZone":"UTC",
            "recurrenceOverrides":{},
        });
        assert!(series_counter_supported(&base));
        assert!(series_counter_supported(&serde_json::json!({
            "recurrenceOverrides":{}
        })));
        assert!(!series_counter_supported(&serde_json::json!({
            "timeZone":"America/Toronto",
            "recurrenceOverrides":{}
        })));
        assert!(!series_counter_supported(&serde_json::json!({
            "timeZone":"UTC",
            "recurrenceOverrides":{"2026-11-25T15:00:00":{"excluded":true}}
        })));
    }

    #[tokio::test]
    async fn instance_response_on_non_recurring_event_returns_eas_status_146() {
        let app = Router::new().route(
            "/",
            post(|Json(_request): Json<serde_json::Value>| async {
                Json(serde_json::json!({"methodResponses":[[
                    "CalendarEvent/get", {"state":"s1","list":[{
                        "id":"e","@type":"Event","calendarIds":{"cal":true},
                        "participants":{}
                    }]}, "get"
                ]]}))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let session: JmapSession = serde_json::from_value(serde_json::json!({
            "capabilities":{}, "primaryAccounts":{capabilities::CALENDARS:"a"}, "apiUrl":url,
            "downloadUrl":url,"uploadUrl":url,"username":"user@example.test","state":"s"
        }))
        .unwrap();
        let auth = AuthenticatedSession {
            capabilities: GatewayCapabilities::from_session(&session),
            session,
            authorization: BasicAuthorization {
                username: "user@example.test".into(),
                password: "password".into(),
            },
        };
        let client = JmapClient::new(
            Config::from_env().unwrap(),
            Arc::new(crate::state::MemoryStateStore::default()),
        )
        .unwrap();

        assert_eq!(
            client
                .respond_calendar_event(
                    &auth,
                    "cal",
                    "e",
                    "accepted",
                    MeetingResponseOptions {
                        instance_id: Some("2026-11-25T15:00:00.000Z"),
                        ..Default::default()
                    },
                )
                .await
                .unwrap(),
            "146"
        );
        server.abort();
    }

    #[tokio::test]
    async fn series_counter_without_instance_id_fails_closed_before_writing() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let captured = calls.clone();
        let app = Router::new().route(
            "/",
            post(move |Json(_request): Json<serde_json::Value>| {
                let calls = captured.clone();
                async move {
                    calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Json(serde_json::json!({"methodResponses":[[
                        "CalendarEvent/get", {"state":"s1","list":[{
                            "id":"e","uid":"event@example.test","@type":"Event",
                            "calendarIds":{"cal":true},"recurrenceRule":{"frequency":"weekly"},
                            "participants":{
                                "owner":{"email":"owner@example.test","roles":{"owner":true}},
                                "self":{"email":"user@example.test","roles":{"attendee":true}}
                            }
                        }]}, "get"
                    ]]}))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let session: JmapSession = serde_json::from_value(serde_json::json!({
            "capabilities":{}, "primaryAccounts":{capabilities::CALENDARS:"a"}, "apiUrl":url,
            "downloadUrl":url,"uploadUrl":url,"username":"user@example.test","state":"s"
        }))
        .unwrap();
        let auth = AuthenticatedSession {
            capabilities: GatewayCapabilities::from_session(&session),
            session,
            authorization: BasicAuthorization {
                username: "user@example.test".into(),
                password: "password".into(),
            },
        };
        let client = JmapClient::new(
            Config::from_env().unwrap(),
            Arc::new(crate::state::MemoryStateStore::default()),
        )
        .unwrap();
        assert_eq!(
            client
                .respond_calendar_event(
                    &auth,
                    "cal",
                    "e",
                    "declined",
                    MeetingResponseOptions {
                        send_response: true,
                        proposed_times: Some(MeetingResponseProposal {
                            start: "2026-12-01T15:00:00Z".parse().unwrap(),
                            end: "2026-12-01T16:00:00Z".parse().unwrap(),
                        }),
                        ..Default::default()
                    },
                )
                .await
                .unwrap(),
            "4"
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        server.abort();
    }

    #[tokio::test]
    async fn meeting_response_send_flag_controls_jmap_scheduling_messages() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let captured = calls.clone();
        let app = Router::new().route(
            "/",
            post(move |Json(request): Json<serde_json::Value>| {
                let calls = captured.clone();
                async move {
                    let call = &request["methodCalls"][0];
                    match calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
                        0 => {
                            assert_eq!(call[0], "CalendarEvent/get");
                            Json(serde_json::json!({"methodResponses":[[
                                "CalendarEvent/get", {"state":"s1","list":[{"id":"e","@type":"Event","calendarIds":{"cal":true},"participants":{
                                    "owner":{"email":"owner@example.test","roles":{"owner":true}},
                                    "self":{"email":"user@example.test","roles":{"attendee":true},"participationStatus":"needs-action"}
                                }}]}, "get"]]}))
                        }
                        1 => {
                            assert_eq!(call[0], "CalendarEvent/set");
                            assert_eq!(call[1]["sendSchedulingMessages"], true);
                            assert_eq!(call[1]["ifInState"], "s1");
                            Json(serde_json::json!({"methodResponses":[[
                                "CalendarEvent/set", {"updated":{"e":null}}, "set"]]}))
                        }
                        2 => {
                            assert_eq!(call[0], "CalendarEvent/get");
                            Json(serde_json::json!({"methodResponses":[[
                                "CalendarEvent/get", {"state":"s2","list":[{"id":"e","participants":{
                                    "owner":{"email":"owner@example.test","roles":{"owner":true}},
                                    "self":{"email":"user@example.test","roles":{"attendee":true},"participationStatus":"accepted"}
                                }}]}, "verify"]]}))
                        }
                        _ => panic!("unexpected JMAP request"),
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let session: JmapSession = serde_json::from_value(serde_json::json!({
            "capabilities":{}, "primaryAccounts":{capabilities::CALENDARS:"a"}, "apiUrl":url,
            "downloadUrl":url,"uploadUrl":url,"username":"user@example.test","state":"s"
        }))
        .unwrap();
        let auth = AuthenticatedSession {
            capabilities: GatewayCapabilities::from_session(&session),
            session,
            authorization: BasicAuthorization {
                username: "user@example.test".into(),
                password: "password".into(),
            },
        };
        let client = JmapClient::new(
            Config::from_env().unwrap(),
            Arc::new(crate::state::MemoryStateStore::default()),
        )
        .unwrap();
        assert_eq!(
            client
                .respond_calendar_event(
                    &auth,
                    "cal",
                    "e",
                    "accepted",
                    MeetingResponseOptions {
                        send_response: true,
                        ..Default::default()
                    },
                )
                .await
                .unwrap(),
            "1"
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 3);
        server.abort();
    }

    #[tokio::test]
    async fn meeting_response_updates_only_the_requested_occurrence() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let captured = calls.clone();
        let app = Router::new().route(
            "/",
            post(move |Json(request): Json<serde_json::Value>| {
                let calls = captured.clone();
                async move {
                    let call = &request["methodCalls"][0];
                    match calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
                        0 => Json(serde_json::json!({"methodResponses":[[
                            "CalendarEvent/get", {"state":"s1","list":[{
                                "id":"e","@type":"Event","calendarIds":{"cal":true},
                                "timeZone":"Etc/UTC","recurrenceRule":{"frequency":"daily"},
                                "recurrenceOverrides":{},"participants":{
                                    "owner":{"email":"owner@example.test","roles":{"owner":true}},
                                    "self":{"email":"user@example.test","roles":{"attendee":true},"participationStatus":"needs-action"}
                                }
                            }]}, "get"
                        ]]})),
                        1 => {
                            assert_eq!(call[0], "CalendarEvent/set");
                            assert_eq!(call[1]["ifInState"], "s1");
                            let patch = &call[1]["update"]["e"];
                            let path = "recurrenceOverrides/2026-11-25T15:00:00/participants";
                            assert_eq!(patch[path]["self"]["participationStatus"], "declined");
                            assert!(call[1].get("destroy").is_none());
                            assert!(patch["participants/self/participationStatus"].is_null());
                            Json(serde_json::json!({"methodResponses":[[
                                "CalendarEvent/set", {"updated":{"e":null}}, "set"
                            ]]}))
                        }
                        2 => Json(serde_json::json!({"methodResponses":[[
                            "CalendarEvent/get", {"state":"s2","list":[{
                                "id":"e","participants":{
                                    "owner":{"email":"owner@example.test","roles":{"owner":true}},
                                    "self":{"email":"user@example.test","roles":{"attendee":true},"participationStatus":"needs-action"}
                                },"recurrenceOverrides":{"2026-11-25T15:00:00":{"participants":{
                                    "owner":{"email":"owner@example.test","roles":{"owner":true}},
                                    "self":{"email":"user@example.test","roles":{"attendee":true},"participationStatus":"declined"}
                                }}}
                            }]}, "verify"
                        ]]})),
                        _ => panic!("unexpected JMAP request"),
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let session: JmapSession = serde_json::from_value(serde_json::json!({
            "capabilities":{}, "primaryAccounts":{capabilities::CALENDARS:"a"}, "apiUrl":url,
            "downloadUrl":url,"uploadUrl":url,"username":"user@example.test","state":"s"
        }))
        .unwrap();
        let auth = AuthenticatedSession {
            capabilities: GatewayCapabilities::from_session(&session),
            session,
            authorization: BasicAuthorization {
                username: "user@example.test".into(),
                password: "password".into(),
            },
        };
        let client = JmapClient::new(
            Config::from_env().unwrap(),
            Arc::new(crate::state::MemoryStateStore::default()),
        )
        .unwrap();
        assert_eq!(
            client
                .respond_calendar_event(
                    &auth,
                    "cal",
                    "e",
                    "declined",
                    MeetingResponseOptions {
                        send_response: true,
                        instance_id: Some("2026-11-25T15:00:00.000Z"),
                        ..Default::default()
                    },
                )
                .await
                .unwrap(),
            "1"
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 3);
        server.abort();
    }

    #[tokio::test]
    async fn meeting_response_to_series_updates_existing_attendee_overrides() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let captured = calls.clone();
        let app = Router::new().route(
            "/",
            post(move |Json(request): Json<serde_json::Value>| {
                let calls = captured.clone();
                async move {
                    let call = &request["methodCalls"][0];
                    match calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
                        0 => Json(serde_json::json!({"methodResponses":[[
                            "CalendarEvent/get", {"state":"s1","list":[{
                                "id":"e","@type":"Event","calendarIds":{"cal":true},
                                "recurrenceRule":{"frequency":"daily"},"recurrenceOverrides":{
                                    "2026-11-25T15:00:00":{"participants":{
                                        "owner":{"email":"owner@example.test","roles":{"owner":true}},
                                        "self":{"email":"user@example.test","roles":{"attendee":true},"participationStatus":"tentative"}
                                    }}
                                },"participants":{
                                    "owner":{"email":"owner@example.test","roles":{"owner":true}},
                                    "self":{"email":"user@example.test","roles":{"attendee":true},"participationStatus":"needs-action"}
                                }
                            }]}, "get"
                        ]]})),
                        1 => {
                            assert_eq!(call[0], "CalendarEvent/set");
                            let patch = &call[1]["update"]["e"];
                            assert_eq!(patch["participants/self/participationStatus"], "accepted");
                            assert_eq!(patch["recurrenceOverrides/2026-11-25T15:00:00/participants"]["self"]["participationStatus"], "accepted");
                            Json(serde_json::json!({"methodResponses":[[
                                "CalendarEvent/set", {"updated":{"e":null}}, "set"
                            ]]}))
                        }
                        2 => Json(serde_json::json!({"methodResponses":[[
                            "CalendarEvent/get", {"list":[{
                                "id":"e","participants":{
                                    "owner":{"email":"owner@example.test","roles":{"owner":true}},
                                    "self":{"email":"user@example.test","roles":{"attendee":true},"participationStatus":"accepted"}
                                },"recurrenceOverrides":{"2026-11-25T15:00:00":{"participants":{
                                    "owner":{"email":"owner@example.test","roles":{"owner":true}},
                                    "self":{"email":"user@example.test","roles":{"attendee":true},"participationStatus":"accepted"}
                                }}}
                            }]}, "verify"
                        ]]})),
                        _ => panic!("unexpected JMAP request"),
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let session: JmapSession = serde_json::from_value(serde_json::json!({
            "capabilities":{}, "primaryAccounts":{capabilities::CALENDARS:"a"}, "apiUrl":url,
            "downloadUrl":url,"uploadUrl":url,"username":"user@example.test","state":"s"
        }))
        .unwrap();
        let auth = AuthenticatedSession {
            capabilities: GatewayCapabilities::from_session(&session),
            session,
            authorization: BasicAuthorization {
                username: "user@example.test".into(),
                password: "password".into(),
            },
        };
        let client = JmapClient::new(
            Config::from_env().unwrap(),
            Arc::new(crate::state::MemoryStateStore::default()),
        )
        .unwrap();
        assert_eq!(
            client
                .respond_calendar_event(
                    &auth,
                    "cal",
                    "e",
                    "accepted",
                    MeetingResponseOptions::default()
                )
                .await
                .unwrap(),
            "1"
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 3);
        server.abort();
    }

    #[tokio::test]
    async fn principal_availability_batches_queries_and_preserves_per_recipient_errors() {
        let call_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let captured_count = call_count.clone();
        let app = Router::new().route(
            "/",
            post(move |Json(request): Json<serde_json::Value>| {
                let call_count = captured_count.clone();
                async move {
                let calls = request["methodCalls"].as_array().unwrap();
                assert_eq!(calls.len(), 1);
                assert!(request["using"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|cap| cap == capabilities::PRINCIPALS_AVAILABILITY));
                assert_eq!(calls[0][0], "Principal/getAvailability");
                assert_eq!(calls[0][1]["showDetails"], true);
                assert_eq!(calls[0][1]["eventProperties"], serde_json::json!(["freeBusyStatus"]));
                    let response = match call_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
                        0 => {
                            assert_eq!(calls[0][1]["id"], "p1");
                            serde_json::json!(["Principal/getAvailability", {"list":[{
                                "utcStart":"2042-05-05T10:00:00Z","utcEnd":"2042-05-05T11:00:00Z","busyStatus":"confirmed","event":null
                            }]}, "availability-0"])
                        }
                        1 => {
                            assert_eq!(calls[0][1]["id"], "p2");
                            serde_json::json!(["error", {"type":"forbidden"}, "availability-0"])
                        }
                        _ => panic!("unexpected availability request"),
                    };
                    Json(serde_json::json!({"methodResponses":[response]}))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let session: JmapSession = serde_json::from_value(serde_json::json!({
            "capabilities":{capabilities::CORE:{"maxCallsInRequest":1}}, "primaryAccounts":{capabilities::PRINCIPALS:"a"}, "apiUrl":url,
            "downloadUrl":url,"uploadUrl":url,"username":"user@example.test","state":"s"
        }))
        .unwrap();
        let auth = AuthenticatedSession {
            capabilities: GatewayCapabilities::from_session(&session),
            session,
            authorization: BasicAuthorization {
                username: "user@example.test".into(),
                password: "password".into(),
            },
        };
        let client = JmapClient::new(
            Config::from_env().unwrap(),
            Arc::new(crate::state::MemoryStateStore::default()),
        )
        .unwrap();
        let results = client
            .principal_availabilities(
                &auth,
                &["p1".into(), "p2".into()],
                "2042-05-05T10:00:00.000Z",
                "2042-05-05T11:00:00.000Z",
            )
            .await
            .unwrap();
        assert_eq!(
            results["p1"].as_ref().unwrap()[0]["busyStatus"],
            "confirmed"
        );
        assert_eq!(results["p2"].as_ref().unwrap_err(), &"163");
        assert_eq!(call_count.load(std::sync::atomic::Ordering::SeqCst), 2);
        server.abort();
    }
}
