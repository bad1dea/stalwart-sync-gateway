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
        EmailBodyType, Task,
    },
};

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

impl JmapClient {
    pub fn new(config: Config) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent("stalwart-sync-gateway/0.1")
            .danger_accept_invalid_certs(!config.stalwart_tls_verify)
            .pool_idle_timeout(std::time::Duration::from_secs(90))
            .http2_adaptive_window(true)
            .build()
            .context("failed to build JMAP HTTP client")?;
        Ok(Self { http, config })
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
                        "from", "to", "cc", "textBody", "htmlBody", "bodyValues", "attachments", "threadId"
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
                    let emails: Vec<Email> = get.list.into_iter().map(Email::from).collect();
                    let has_more = total.is_some_and(|total| total as usize > limit);
                    return Ok((emails, has_more));
                }
                "error" => anyhow::bail!("JMAP method error in email sync"),
                _ => {}
            }
        }

        Ok((Vec::new(), false))
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
        let Some(account_id) = auth.session.primary_account_for(capabilities::MAIL) else {
            return Ok(BTreeSet::new());
        };

        let response: JmapResponse<serde_json::Value> = self
            .api_call(
                auth,
                &[capabilities::CORE.to_owned(), capabilities::MAIL.to_owned()],
                vec![MethodCall::new(
                    "Email/get",
                    serde_json::json!({
                        "accountId": account_id,
                        "ids": candidate_ids,
                        "properties": ["id", "mailboxIds"]
                    }),
                    "0",
                )],
            )
            .await?;

        for method in response.method_responses {
            if method.0 == "Email/get" {
                let get: GetResponse<EmailMembership> = serde_json::from_value(method.1)
                    .context("invalid Email/get response while checking mail deletion candidates")?;
                return Ok(get
                    .list
                    .into_iter()
                    .filter(|email| email.mailbox_ids.get(mailbox_id).copied().unwrap_or(false))
                    .map(|email| email.id)
                    .collect());
            } else if method.0 == "error" {
                anyhow::bail!("JMAP method error while checking mail deletion candidates");
            }
        }
        anyhow::bail!("JMAP Email/get response was missing while checking mail deletion candidates")
    }

    /// Lists contacts in a JSContact address book, newest-touched first.
    /// Contacts have no real two-way sync yet (this is read-only, like
    /// mail's own list-and-diff via seen_ids) -- see the
    /// `sync_contacts_collection` caller for that state-diffing.
    pub async fn contacts_in_address_book(
        &self,
        auth: &AuthenticatedSession,
        address_book_id: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<Contact>> {
        let Some(account_id) = auth.session.primary_account_for(capabilities::CONTACTS) else {
            return Ok(Vec::new());
        };

        let calls = vec![
            MethodCall::new(
                "ContactCard/query",
                serde_json::json!({
                    "accountId": account_id,
                    "filter": { "inAddressBook": address_book_id },
                    "limit": limit.clamp(1, 200)
                }),
                "q",
            ),
            MethodCall::new(
                "ContactCard/get",
                serde_json::json!({
                    "accountId": account_id,
                    "#ids": {
                        "resultOf": "q",
                        "name": "ContactCard/query",
                        "path": "/ids"
                    }
                }),
                "g",
            ),
        ];

        let response: JmapResponse<serde_json::Value> = self
            .api_call(
                auth,
                &[
                    capabilities::CORE.to_owned(),
                    capabilities::CONTACTS.to_owned(),
                ],
                calls,
            )
            .await?;

        for method in response.method_responses {
            match method.0.as_str() {
                "ContactCard/get" => {
                    let get: GetResponse<ContactCardObject> = serde_json::from_value(method.1)
                        .context("invalid ContactCard/get response")?;
                    return Ok(get.list.into_iter().map(Contact::from).collect());
                }
                "error" => anyhow::bail!("JMAP method error in contact sync"),
                _ => {}
            }
        }

        Ok(Vec::new())
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
        let Some(account_id) = auth.session.primary_account_for(capabilities::CONTACTS) else {
            anyhow::bail!("JMAP Contacts capability is not available");
        };

        let mut card = serde_json::Map::new();

        let mut components = Vec::new();
        if let Some(first) = &fields.first_name {
            components.push(serde_json::json!({"kind": "given", "value": first}));
        }
        if let Some(last) = &fields.last_name {
            components.push(serde_json::json!({"kind": "surname", "value": last}));
        }
        let mut name = serde_json::Map::new();
        if let Some(file_as) = &fields.file_as {
            name.insert("full".to_owned(), serde_json::Value::String(file_as.clone()));
        }
        if !components.is_empty() {
            name.insert(
                "components".to_owned(),
                serde_json::Value::Array(components),
            );
        }
        if !name.is_empty() {
            card.insert("name".to_owned(), serde_json::Value::Object(name));
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
        if let Some(business) = &fields.business_phone_number {
            phones.insert(
                "p_work".to_owned(),
                serde_json::json!({ "number": business, "contexts": { "work": true } }),
            );
        }
        if !phones.is_empty() {
            card.insert("phones".to_owned(), serde_json::Value::Object(phones));
        }

        if let Some(company) = &fields.company_name {
            card.insert(
                "organizations".to_owned(),
                serde_json::json!({ "o1": { "name": company } }),
            );
        }
        if let Some(title) = &fields.job_title {
            card.insert(
                "titles".to_owned(),
                serde_json::json!({ "t1": { "name": title, "kind": "title" } }),
            );
        }

        let call = match id {
            Some(existing_id) => {
                card.insert("@type".to_owned(), serde_json::Value::String("Card".to_owned()));
                card.insert("version".to_owned(), serde_json::Value::String("1.0".to_owned()));
                MethodCall::new(
                    "ContactCard/set",
                    serde_json::json!({
                        "accountId": account_id,
                        "update": { existing_id: card }
                    }),
                    "0",
                )
            }
            None => {
                card.insert("@type".to_owned(), serde_json::Value::String("Card".to_owned()));
                card.insert("version".to_owned(), serde_json::Value::String("1.0".to_owned()));
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
    /// right -- see `local_to_utc_eas`/`parse_iso8601_duration_seconds`.
    pub async fn calendar_events_in_calendar(
        &self,
        auth: &AuthenticatedSession,
        calendar_id: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<CalendarEvent>> {
        let Some(account_id) = auth.session.primary_account_for(capabilities::CALENDARS) else {
            return Ok(Vec::new());
        };

        let calls = vec![
            MethodCall::new(
                "CalendarEvent/query",
                serde_json::json!({
                    "accountId": account_id,
                    "filter": { "inCalendar": calendar_id },
                    "limit": limit.clamp(1, 200)
                }),
                "q",
            ),
            MethodCall::new(
                "CalendarEvent/get",
                serde_json::json!({
                    "accountId": account_id,
                    "#ids": {
                        "resultOf": "q",
                        "name": "CalendarEvent/query",
                        "path": "/ids"
                    },
                    "properties": [
                        "id", "title", "start", "timeZone", "duration",
                        "locations", "showWithoutTime"
                    ]
                }),
                "g",
            ),
        ];

        let response: JmapResponse<serde_json::Value> = self
            .api_call(
                auth,
                &[
                    capabilities::CORE.to_owned(),
                    capabilities::CALENDARS.to_owned(),
                ],
                calls,
            )
            .await?;

        for method in response.method_responses {
            match method.0.as_str() {
                "CalendarEvent/get" => {
                    let get: GetResponse<CalendarEventObject> = serde_json::from_value(method.1)
                        .context("invalid CalendarEvent/get response")?;
                    return Ok(get.list.into_iter().map(CalendarEvent::from).collect());
                }
                "error" => anyhow::bail!("JMAP method error in calendar sync"),
                _ => {}
            }
        }

        Ok(Vec::new())
    }

    /// Creates (`id: None`) or updates (`id: Some`) a non-recurring
    /// calendar event from ActiveSync `CalendarFields`. Recurrence,
    /// attendees, and reminders are explicitly out of scope, matching
    /// the read path (`calendar_events_in_calendar`). `start`/`duration`
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
        fields: &crate::wbxml::eas::CalendarFields,
    ) -> anyhow::Result<String> {
        let Some(account_id) = auth.session.primary_account_for(capabilities::CALENDARS) else {
            anyhow::bail!("JMAP Calendars capability is not available");
        };

        let mut event = serde_json::Map::new();
        event.insert(
            "@type".to_owned(),
            serde_json::Value::String("Event".to_owned()),
        );
        if let Some(subject) = &fields.subject {
            event.insert(
                "title".to_owned(),
                serde_json::Value::String(subject.clone()),
            );
        }
        if let Some(location) = &fields.location {
            event.insert(
                "locations".to_owned(),
                serde_json::json!({ "loc1": { "name": location } }),
            );
        }
        if let Some(start) = fields.start_time.as_deref().and_then(eas_compact_to_local_iso) {
            event.insert("start".to_owned(), serde_json::Value::String(start));
        }
        if let (Some(start), Some(end)) = (&fields.start_time, &fields.end_time) {
            if let Some(duration) = eas_compact_duration(start, end) {
                event.insert("duration".to_owned(), serde_json::Value::String(duration));
            }
        }
        if let Some(all_day) = fields.all_day_event {
            event.insert(
                "showWithoutTime".to_owned(),
                serde_json::Value::Bool(all_day),
            );
        }

        let call = match id {
            Some(existing_id) => MethodCall::new(
                "CalendarEvent/set",
                serde_json::json!({
                    "accountId": account_id,
                    "update": { existing_id: event }
                }),
                "0",
            ),
            None => {
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
        let Some(account_id) = auth.session.primary_account_for(capabilities::CALENDARS) else {
            return Ok(Vec::new());
        };

        let calls = vec![
            MethodCall::new(
                "CalendarEvent/query",
                serde_json::json!({
                    "accountId": account_id,
                    "filter": { "inCalendar": calendar_id },
                    "limit": limit.clamp(1, 200)
                }),
                "q",
            ),
            MethodCall::new(
                "CalendarEvent/get",
                serde_json::json!({
                    "accountId": account_id,
                    "#ids": {
                        "resultOf": "q",
                        "name": "CalendarEvent/query",
                        "path": "/ids"
                    },
                    "properties": ["id", "@type", "title", "due", "progress"]
                }),
                "g",
            ),
        ];

        let response: JmapResponse<serde_json::Value> = self
            .api_call(
                auth,
                &[
                    capabilities::CORE.to_owned(),
                    capabilities::CALENDARS.to_owned(),
                ],
                calls,
            )
            .await?;

        for method in response.method_responses {
            match method.0.as_str() {
                "CalendarEvent/get" => {
                    let get: GetResponse<TaskObject> = serde_json::from_value(method.1)
                        .context("invalid CalendarEvent/get response (tasks)")?;
                    return Ok(get
                        .list
                        .into_iter()
                        .filter(|item| item.kind == "Task")
                        .map(Task::from)
                        .collect());
                }
                "error" => anyhow::bail!("JMAP method error in task sync"),
                _ => {}
            }
        }

        Ok(Vec::new())
    }

    /// Creates (`id: None`) or updates (`id: Some`) a Task-typed
    /// `CalendarEvent`, mirroring `save_calendar_event`'s structure and
    /// the same live-confirmed in-place `update` support (the JMAP id is
    /// a stable ActiveSync ServerId across edits, no workaround needed).
    /// `due` deliberately omits `timeZone`, same "absent == already UTC"
    /// convention `save_calendar_event` uses for `start`.
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

        let mut task = serde_json::Map::new();
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
        if let Some(due) = fields.due_date.as_deref().and_then(eas_compact_to_local_iso) {
            task.insert("due".to_owned(), serde_json::Value::String(due));
        }
        if let Some(complete) = fields.complete {
            task.insert(
                "progress".to_owned(),
                serde_json::Value::String(
                    if complete { "completed" } else { "needs-action" }.to_owned(),
                ),
            );
        }

        let call = match id {
            Some(existing_id) => MethodCall::new(
                "CalendarEvent/set",
                serde_json::json!({
                    "accountId": account_id,
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

        let calls = vec![MethodCall::new(
            "Email/get",
            serde_json::json!({
                "accountId": account_id,
                "ids": [email_id],
                "properties": [
                    "id", "blobId", "mailboxIds", "keywords", "receivedAt", "subject",
                    "from", "to", "cc", "textBody", "htmlBody", "bodyValues", "attachments", "threadId"
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
                    return Ok(get.list.into_iter().next().map(Email::from));
                }
                "error" => anyhow::bail!("JMAP method error in email fetch"),
                _ => {}
            }
        }

        Ok(None)
    }

    pub async fn set_email_seen(
        &self,
        auth: &AuthenticatedSession,
        email_id: &str,
        seen: bool,
    ) -> anyhow::Result<()> {
        let Some(account_id) = auth.session.primary_account_for(capabilities::MAIL) else {
            anyhow::bail!("JMAP Mail capability is not available");
        };

        let mut patch = serde_json::Map::new();
        patch.insert(
            "keywords/$seen".to_owned(),
            if seen {
                serde_json::Value::Bool(true)
            } else {
                serde_json::Value::Null
            },
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

    pub async fn destroy_email(
        &self,
        auth: &AuthenticatedSession,
        email_id: &str,
    ) -> anyhow::Result<()> {
        let Some(account_id) = auth.session.primary_account_for(capabilities::MAIL) else {
            anyhow::bail!("JMAP Mail capability is not available");
        };

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
        let Some(account_id) = auth.session.primary_account_for(capabilities::MAIL) else {
            anyhow::bail!("JMAP Mail capability is not available");
        };
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
        Ok(response
            .bytes()
            .await
            .context("failed to read blob download body")?
            .to_vec())
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
        let sent_mailbox_id = if save_in_sent_items {
            self.find_mailbox_by_role(auth, "sent").await?
        } else {
            None
        };
        let mailbox_ids = match &sent_mailbox_id {
            Some(id) => serde_json::json!({ id: true }),
            None => serde_json::json!({}),
        };

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

        let submit_response: JmapResponse<serde_json::Value> = self
            .api_call(
                auth,
                &[
                    capabilities::CORE.to_owned(),
                    capabilities::MAIL.to_owned(),
                    capabilities::SUBMISSION.to_owned(),
                ],
                vec![MethodCall::new(
                    "EmailSubmission/set",
                    serde_json::json!({
                        "accountId": account_id,
                        "create": {
                            "s1": {
                                "emailId": imported_id,
                                "identityId": identity_id,
                                "envelope": null,
                            }
                        }
                    }),
                    "sub",
                )],
            )
            .await?;

        for method in submit_response.method_responses {
            if method.0 == "EmailSubmission/set" {
                if let Some(not_created) = method.1.get("notCreated").and_then(|v| v.as_object()) {
                    if !not_created.is_empty() {
                        anyhow::bail!("EmailSubmission/set failed: {not_created:?}");
                    }
                }
            }
        }

        Ok(())
    }

    async fn fetch_primary_identity(
        &self,
        auth: &AuthenticatedSession,
    ) -> anyhow::Result<(String, String)> {
        let Some(account_id) = auth.session.primary_account_for(capabilities::MAIL) else {
            return Ok((String::new(), String::new()));
        };
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
                if let Some(first) = method
                    .1
                    .get("list")
                    .and_then(|l| l.as_array())
                    .and_then(|a| a.first())
                {
                    let id = first
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_owned();
                    let name = first
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_owned();
                    return Ok((id, name));
                }
            }
        }
        Ok((String::new(), String::new()))
    }

    async fn find_mailbox_by_role(
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
        let response: JmapResponse<serde_json::Value> = self
            .api_call(
                auth,
                &[capabilities::CORE.to_owned(), capabilities::MAIL.to_owned()],
                vec![MethodCall::new("Email/set", arguments, "set")],
            )
            .await?;

        for method in response.method_responses {
            match method.0.as_str() {
                "Email/set" => return Ok(()),
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
            .context("failed to call JMAP API")?
            .error_for_status()
            .context("JMAP API returned error status")?;

        response.json().await.context("invalid JMAP API JSON")
    }
}

impl AuthenticatedSession {
    pub fn username(&self) -> &str {
        &self.authorization.username
    }
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
    #[serde(default)]
    total: Option<u64>,
}

/// Minimal shape for `emails_still_in_mailbox`'s deletion-candidate
/// check -- deliberately lighter than `EmailObject`, only the two
/// properties that call actually requests.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EmailMembership {
    id: String,
    #[serde(default)]
    mailbox_ids: BTreeMap<String, bool>,
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
    text_body: Vec<EmailBodyPart>,
    #[serde(default)]
    html_body: Vec<EmailBodyPart>,
    #[serde(default)]
    body_values: BTreeMap<String, EmailBodyValue>,
    #[serde(default)]
    attachments: Vec<EmailAttachmentObject>,
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
struct EmailBodyValue {
    #[serde(default)]
    value: String,
}

impl From<EmailObject> for Email {
    fn from(value: EmailObject) -> Self {
        let body = select_body(&value);
        let attachments = value
            .attachments
            .into_iter()
            .map(|attachment| crate::model::EmailAttachment {
                blob_id: attachment.blob_id,
                name: attachment.name.unwrap_or_else(|| "attachment".to_owned()),
                content_type: attachment
                    .content_type
                    .unwrap_or_else(|| "application/octet-stream".to_owned()),
                size: attachment.size,
            })
            .collect();
        Self {
            id: value.id,
            mailbox_ids: value
                .mailbox_ids
                .into_iter()
                .filter_map(|(id, present)| present.then_some(id))
                .collect(),
            subject: value.subject,
            received_at: value.received_at,
            keywords: value
                .keywords
                .iter()
                .filter_map(|(keyword, present)| present.then_some(keyword.clone()))
                .collect(),
            from: format_addresses(value.from.as_deref().unwrap_or_default()),
            to: format_addresses(value.to.as_deref().unwrap_or_default()),
            cc: format_addresses(value.cc.as_deref().unwrap_or_default()),
            read: value.keywords.get("$seen").copied().unwrap_or(false),
            body,
            attachments,
            blob_id: value.blob_id,
            thread_id: value.thread_id,
        }
    }
}

fn select_body(email: &EmailObject) -> Option<EmailBody> {
    if let Some(value) = body_value_for(&email.html_body, &email.body_values) {
        return Some(EmailBody {
            body_type: EmailBodyType::Html,
            value,
        });
    }
    body_value_for(&email.text_body, &email.body_values).map(|value| EmailBody {
        body_type: EmailBodyType::Plain,
        value,
    })
}

fn body_value_for(
    parts: &[EmailBodyPart],
    values: &BTreeMap<String, EmailBodyValue>,
) -> Option<String> {
    parts.iter().find_map(|part| {
        part.part_id
            .as_ref()
            .and_then(|id| values.get(id))
            .map(|body| body.value.clone())
            .filter(|value| !value.is_empty())
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
    emails: BTreeMap<String, ContactCardEmail>,
    #[serde(default)]
    phones: BTreeMap<String, ContactCardPhone>,
    #[serde(default)]
    organizations: BTreeMap<String, ContactCardOrganization>,
    #[serde(default)]
    titles: BTreeMap<String, ContactCardTitle>,
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
}

#[derive(Debug, Default, Deserialize)]
struct ContactCardOrganization {
    name: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ContactCardTitle {
    name: Option<String>,
    kind: Option<String>,
}

impl From<ContactCardObject> for Contact {
    fn from(value: ContactCardObject) -> Self {
        let mut first_name = None;
        let mut last_name = None;
        if let Some(name) = &value.name {
            for component in &name.components {
                match component.kind.as_str() {
                    "given" => first_name = Some(component.value.clone()),
                    "surname" => last_name = Some(component.value.clone()),
                    _ => {}
                }
            }
        }
        let file_as = value.name.as_ref().and_then(|name| name.full.clone());
        let emails = value
            .emails
            .into_values()
            .filter_map(|email| email.address)
            .collect();

        let mut mobile_phone = None;
        let mut home_phone = None;
        let mut business_phone = None;
        for phone in value.phones.into_values() {
            let Some(number) = phone.number else {
                continue;
            };
            let is = |ctx: &str| phone.contexts.get(ctx).copied().unwrap_or(false);
            if is("mobile") && mobile_phone.is_none() {
                mobile_phone = Some(number);
            } else if is("home") && home_phone.is_none() {
                home_phone = Some(number);
            } else if (is("work") || phone.contexts.is_empty()) && business_phone.is_none() {
                business_phone = Some(number);
            }
        }

        let company_name = value.organizations.into_values().find_map(|org| org.name);
        let job_title = value
            .titles
            .into_values()
            .find(|title| title.kind.as_deref() == Some("title") || title.kind.is_none())
            .and_then(|title| title.name);

        Self {
            id: value.id,
            address_book_ids: value
                .address_book_ids
                .into_iter()
                .filter_map(|(id, present)| present.then_some(id))
                .collect(),
            first_name,
            last_name,
            file_as,
            emails,
            mobile_phone,
            home_phone,
            business_phone,
            company_name,
            job_title,
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
    title: String,
    #[serde(default)]
    start: Option<String>,
    #[serde(default)]
    time_zone: Option<String>,
    #[serde(default)]
    duration: Option<String>,
    #[serde(default)]
    locations: BTreeMap<String, CalendarEventLocation>,
    #[serde(default)]
    show_without_time: bool,
}

#[derive(Debug, Default, Deserialize)]
struct CalendarEventLocation {
    name: Option<String>,
}

impl From<CalendarEventObject> for CalendarEvent {
    fn from(value: CalendarEventObject) -> Self {
        let start_utc = value
            .start
            .as_deref()
            .and_then(|start| local_to_utc_eas(start, value.time_zone.as_deref()));
        let end_utc = match (&start_utc, value.start.as_deref(), &value.duration) {
            (Some(_), Some(start), Some(duration)) => parse_iso8601_duration_seconds(duration)
                .and_then(|seconds| {
                    let naive = chrono::NaiveDateTime::parse_from_str(start, "%Y-%m-%dT%H:%M:%S")
                        .ok()?
                        + chrono::Duration::seconds(seconds);
                    local_to_utc_eas(
                        &naive.format("%Y-%m-%dT%H:%M:%S").to_string(),
                        value.time_zone.as_deref(),
                    )
                }),
            _ => None,
        };
        let location = value
            .locations
            .into_values()
            .find_map(|location| location.name);
        Self {
            id: value.id,
            calendar_ids: Vec::new(),
            title: value.title,
            location,
            start_utc,
            end_utc,
            all_day: value.show_without_time,
        }
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
    due: Option<String>,
    #[serde(default)]
    progress: Option<String>,
    #[serde(rename = "@type", default)]
    kind: String,
}

impl From<TaskObject> for Task {
    fn from(value: TaskObject) -> Self {
        Self {
            id: value.id,
            title: value.title,
            completed: value.progress.as_deref() == Some("completed"),
            due: value.due.as_deref().and_then(local_to_utc_eas_no_tz),
        }
    }
}

/// `due` on a JSCalendar Task is a LocalDateTime with no accompanying
/// `timeZone` property fetched here (this gateway doesn't currently
/// request/round-trip Task time zones) -- treated as already-UTC, the
/// same "absent timeZone" convention `local_to_utc_eas` itself uses.
fn local_to_utc_eas_no_tz(local_iso: &str) -> Option<String> {
    local_to_utc_eas(local_iso, None)
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
                let local = tz
                    .from_local_datetime(&naive)
                    .single()
                    .or_else(|| tz.from_local_datetime(&naive).earliest())?;
                local.with_timezone(&chrono::Utc)
            }
            None => chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(naive, chrono::Utc),
        };
    Some(utc.format("%Y%m%dT%H%M%SZ").to_string())
}

/// Write-side mirror of `local_to_utc_eas`'s "absent timeZone == already
/// UTC" convention: parses EAS's compact UTC DateTime
/// (`YYYYMMDDTHHMMSSZ`) and reformats it as the naive (no offset, no
/// trailing `Z`) ISO 8601 shape JSCalendar's `start` wants when no
/// `timeZone` property is sent alongside it.
fn eas_compact_to_local_iso(compact: &str) -> Option<String> {
    let parsed =
        chrono::NaiveDateTime::parse_from_str(compact, "%Y%m%dT%H%M%SZ").ok()?;
    Some(parsed.format("%Y-%m-%dT%H:%M:%S").to_string())
}

/// `end - start`, both in EAS's compact UTC DateTime form, as an ISO 8601
/// duration for JSCalendar's `duration` property. Deliberately the
/// simplest valid encoding (`PT<seconds>S`) rather than breaking into
/// days/hours/minutes -- `PT5400S` is exactly as spec-valid as `PT1H30M`
/// and has no ambiguity to get wrong.
fn eas_compact_duration(start: &str, end: &str) -> Option<String> {
    let start = chrono::NaiveDateTime::parse_from_str(start, "%Y%m%dT%H%M%SZ").ok()?;
    let end = chrono::NaiveDateTime::parse_from_str(end, "%Y%m%dT%H%M%SZ").ok()?;
    let seconds = (end - start).num_seconds();
    if seconds < 0 {
        return None;
    }
    Some(format!("PT{seconds}S"))
}

/// Minimal ISO 8601 duration parser covering the shapes JSCalendar event
/// durations actually use (`PT1H`, `PT30M`, `P1D`, `PT1H30M`) -- days plus
/// hours/minutes/seconds. Deliberately doesn't handle weeks/months/years
/// (ambiguous without an anchor date, and not something a calendar event
/// duration would realistically use).
fn parse_iso8601_duration_seconds(duration: &str) -> Option<i64> {
    let rest = duration.strip_prefix('P')?;
    let (date_part, time_part) = match rest.split_once('T') {
        Some((date, time)) => (date, Some(time)),
        None => (rest, None),
    };

    let mut seconds: i64 = 0;
    if !date_part.is_empty() {
        seconds += date_part.strip_suffix('D')?.parse::<i64>().ok()? * 86400;
    }
    if let Some(mut time_part) = time_part {
        if let Some(idx) = time_part.find('H') {
            seconds += time_part[..idx].parse::<i64>().ok()? * 3600;
            time_part = &time_part[idx + 1..];
        }
        if let Some(idx) = time_part.find('M') {
            seconds += time_part[..idx].parse::<i64>().ok()? * 60;
            time_part = &time_part[idx + 1..];
        }
        if let Some(idx) = time_part.find('S') {
            seconds += time_part[..idx].parse::<i64>().ok()?;
        }
    }
    Some(seconds)
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
    use super::{
        eas_compact_duration, eas_compact_to_local_iso, local_to_utc_eas,
        parse_iso8601_duration_seconds,
    };

    #[test]
    fn eas_compact_to_local_iso_reformats_without_offset() {
        assert_eq!(
            eas_compact_to_local_iso("20260901T140000Z"),
            Some("2026-09-01T14:00:00".to_owned())
        );
    }

    #[test]
    fn eas_compact_duration_computes_seconds_between() {
        assert_eq!(
            eas_compact_duration("20260901T140000Z", "20260901T153000Z"),
            Some("PT5400S".to_owned())
        );
    }

    #[test]
    fn eas_compact_duration_rejects_end_before_start() {
        assert_eq!(
            eas_compact_duration("20260901T153000Z", "20260901T140000Z"),
            None
        );
    }

    #[test]
    fn duration_parses_hours_only() {
        assert_eq!(parse_iso8601_duration_seconds("PT1H"), Some(3600));
    }

    #[test]
    fn duration_parses_hours_and_minutes() {
        assert_eq!(parse_iso8601_duration_seconds("PT1H30M"), Some(5400));
    }

    #[test]
    fn duration_parses_days_only() {
        assert_eq!(parse_iso8601_duration_seconds("P1D"), Some(86400));
    }

    #[test]
    fn duration_parses_minutes_only() {
        assert_eq!(parse_iso8601_duration_seconds("PT30M"), Some(1800));
    }

    #[test]
    fn duration_rejects_garbage() {
        assert_eq!(parse_iso8601_duration_seconds("not a duration"), None);
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
