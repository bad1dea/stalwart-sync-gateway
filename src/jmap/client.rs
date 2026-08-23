use anyhow::Context;
use axum::http::HeaderMap;
use base64::{engine::general_purpose::STANDARD, Engine};
use reqwest::header::{ACCEPT, AUTHORIZATION};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::collections::BTreeMap;
use url::Url;

use crate::{
    config::Config,
    jmap::{
        capabilities::{self, GatewayCapabilities},
        session::JmapSession,
    },
    model::{eas_folder_type, Collection, CollectionKind, Email, EmailBody, EmailBodyType},
};

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
        let session: JmapSession = response.json().await.context("invalid JMAP session JSON")?;
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

        for method in response.method_responses {
            match method.0.as_str() {
                "Mailbox/get" => {
                    let get: GetResponse<MailboxObject> =
                        serde_json::from_value(method.1).context("invalid Mailbox/get response")?;
                    collections.extend(get.list.into_iter().map(Collection::from));
                }
                "AddressBook/get" => {
                    let get: GetResponse<AddressBookObject> = serde_json::from_value(method.1)
                        .context("invalid AddressBook/get response")?;
                    collections.extend(get.list.into_iter().map(Collection::from));
                }
                "Calendar/get" => {
                    let get: GetResponse<CalendarObject> = serde_json::from_value(method.1)
                        .context("invalid Calendar/get response")?;
                    collections.extend(get.list.into_iter().map(Collection::from));
                }
                "error" => anyhow::bail!("JMAP method error in collection discovery"),
                other => {
                    tracing::debug!(method = other, "ignoring unexpected JMAP method response")
                }
            }
        }

        Ok(collections)
    }

    pub async fn emails_in_mailbox(
        &self,
        auth: &AuthenticatedSession,
        mailbox_id: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<Email>> {
        let Some(account_id) = auth.session.primary_account_for(capabilities::MAIL) else {
            return Ok(Vec::new());
        };

        let calls = vec![
            MethodCall::new(
                "Email/query",
                serde_json::json!({
                    "accountId": account_id,
                    "filter": { "inMailbox": mailbox_id },
                    "sort": [{ "property": "receivedAt", "isAscending": false }],
                    "limit": limit.clamp(1, 100)
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
                        "id", "mailboxIds", "keywords", "receivedAt", "subject",
                        "from", "to", "cc", "textBody", "htmlBody", "bodyValues"
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

        for method in response.method_responses {
            match method.0.as_str() {
                "Email/get" => {
                    let get: GetResponse<EmailObject> =
                        serde_json::from_value(method.1).context("invalid Email/get response")?;
                    return Ok(get.list.into_iter().map(Email::from).collect());
                }
                "error" => anyhow::bail!("JMAP method error in email sync"),
                _ => {}
            }
        }

        Ok(Vec::new())
    }

    async fn api_call<T: DeserializeOwned>(
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
struct MethodCall(String, serde_json::Value, String);

impl MethodCall {
    fn new(name: &str, arguments: serde_json::Value, id: &str) -> Self {
        Self(name.to_owned(), arguments, id.to_owned())
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct JmapResponse<T> {
    method_responses: Vec<MethodResponse<T>>,
}

#[derive(Debug, Deserialize)]
struct MethodResponse<T>(String, T, String);

#[derive(Debug, Deserialize)]
struct GetResponse<T> {
    #[serde(default)]
    list: Vec<T>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EmailObject {
    id: String,
    #[serde(default)]
    mailbox_ids: BTreeMap<String, bool>,
    #[serde(default)]
    keywords: BTreeMap<String, bool>,
    #[serde(default)]
    received_at: Option<String>,
    #[serde(default)]
    subject: String,
    #[serde(default)]
    from: Vec<EmailAddress>,
    #[serde(default)]
    to: Vec<EmailAddress>,
    #[serde(default)]
    cc: Vec<EmailAddress>,
    #[serde(default)]
    text_body: Vec<EmailBodyPart>,
    #[serde(default)]
    html_body: Vec<EmailBodyPart>,
    #[serde(default)]
    body_values: BTreeMap<String, EmailBodyValue>,
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
            from: format_addresses(&value.from),
            to: format_addresses(&value.to),
            cc: format_addresses(&value.cc),
            read: value.keywords.get("$seen").copied().unwrap_or(false),
            body,
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
