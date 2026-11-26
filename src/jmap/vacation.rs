//! Settings>Oof (Automatic Replies) backed by JMAP's real `VacationResponse`
//! object (RFC 8621 section 6) -- a ratified, well-documented account-level
//! singleton (`id: "singleton"`, `get`/`set` only, no create/destroy),
//! unlike the EAS wire format around it which has needed empirical live
//! discovery for nearly everything else in this project. Confirmed present
//! in this Stalwart instance's advertised capabilities (see
//! `jmap::notes` module doc: a live session response listed
//! `vacationresponse` alongside `core/mail/calendars/...`).
//!
//! The enabled Get/Set path has now been exercised end to end against the
//! isolated Amy/Stalwart test account through the gateway. The test saved
//! the existing VacationResponse, enabled a temporary reply, checked both
//! Set and Get WBXML responses, and restored the original account state.
//! This verifies the enabled response against a real Stalwart instance, but
//! it does not verify a real client's UI or normal negotiation; that still
//! requires a real-device test. The enabled-state shape follows the
//! MS-ASSETTINGS OofMessage/AppliesToInternal/Enabled/ReplyMessage/BodyType
//! schema. The earlier disabled Get comparison against z-push remains a
//! separate reference check; z-push's JMAP backend does not itself implement
//! Oof against a live backend.

use anyhow::Context;
use serde::Deserialize;

use crate::jmap::{
    capabilities,
    client::{AuthenticatedSession, GetResponse, JmapClient, JmapResponse, MethodCall},
};

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VacationResponse {
    #[serde(default)]
    pub is_enabled: bool,
    #[serde(default)]
    pub from_date: Option<String>,
    #[serde(default)]
    pub to_date: Option<String>,
    #[serde(default)]
    pub subject: Option<String>,
    #[serde(default)]
    pub text_body: Option<String>,
}

/// Fields settable via `Settings>Oof>Set`. The outer `Option` distinguishes
/// an omitted EAS field (leave the JMAP property untouched) from an explicitly
/// supplied field; the inner `Option` distinguishes a value from a clear.
pub struct VacationResponseUpdate {
    pub is_enabled: Option<bool>,
    pub subject: Option<Option<String>>,
    pub text_body: Option<Option<String>>,
    pub from_date: Option<Option<String>>,
    pub to_date: Option<Option<String>>,
}

impl JmapClient {
    pub async fn get_vacation_response(
        &self,
        auth: &AuthenticatedSession,
    ) -> anyhow::Result<Option<VacationResponse>> {
        let Some(account_id) = auth.session.primary_account_for(capabilities::MAIL) else {
            return Ok(None);
        };
        let response: JmapResponse<serde_json::Value> = self
            .api_call(
                auth,
                &[
                    capabilities::CORE.to_owned(),
                    capabilities::VACATION_RESPONSE.to_owned(),
                ],
                vec![MethodCall::new(
                    "VacationResponse/get",
                    serde_json::json!({ "accountId": account_id }),
                    "0",
                )],
            )
            .await?;

        for method in response.method_responses {
            if method.0 == "VacationResponse/get" {
                let get: GetResponse<VacationResponse> = serde_json::from_value(method.1)
                    .context("invalid VacationResponse/get response")?;
                return Ok(get.list.into_iter().next());
            }
        }
        Ok(None)
    }

    pub async fn set_vacation_response(
        &self,
        auth: &AuthenticatedSession,
        update: VacationResponseUpdate,
    ) -> anyhow::Result<()> {
        let Some(account_id) = auth.session.primary_account_for(capabilities::MAIL) else {
            anyhow::bail!("JMAP Mail capability is not available");
        };

        let mut patch = serde_json::Map::new();
        if let Some(value) = update.is_enabled {
            patch.insert("isEnabled".to_owned(), serde_json::Value::Bool(value));
        }
        for (name, value) in [
            ("subject", update.subject),
            ("textBody", update.text_body),
            ("fromDate", update.from_date),
            ("toDate", update.to_date),
        ] {
            if let Some(value) = value {
                patch.insert(
                    name.to_owned(),
                    value
                        .map(serde_json::Value::String)
                        .unwrap_or(serde_json::Value::Null),
                );
            }
        }

        let response: JmapResponse<serde_json::Value> = self
            .api_call(
                auth,
                &[
                    capabilities::CORE.to_owned(),
                    capabilities::VACATION_RESPONSE.to_owned(),
                ],
                vec![MethodCall::new(
                    "VacationResponse/set",
                    serde_json::json!({
                        "accountId": account_id,
                        "update": { "singleton": patch }
                    }),
                    "0",
                )],
            )
            .await?;

        let mut saw_set_response = false;
        for method in response.method_responses {
            if method.0 == "error" {
                anyhow::bail!("JMAP method error in VacationResponse/set");
            }
            if method.0 == "VacationResponse/set" {
                saw_set_response = true;
                let singleton_updated = method
                    .1
                    .get("updated")
                    .and_then(serde_json::Value::as_object)
                    .is_some_and(|updated| updated.contains_key("singleton"));
                anyhow::ensure!(
                    singleton_updated,
                    "VacationResponse/set did not report singleton as updated"
                );
                let not_updated_empty = method
                    .1
                    .get("notUpdated")
                    .map(|value| value.is_null() || value.as_object().is_some_and(|m| m.is_empty()))
                    .unwrap_or(true);
                if !not_updated_empty {
                    anyhow::bail!(
                        "VacationResponse/set reported notUpdated: {:?}",
                        method.1.get("notUpdated")
                    );
                }
            }
        }
        anyhow::ensure!(
            saw_set_response,
            "JMAP response omitted VacationResponse/set result"
        );
        Ok(())
    }
}
