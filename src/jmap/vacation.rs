//! Settings>Oof (Automatic Replies) backed by JMAP's real `VacationResponse`
//! object (RFC 8621 section 6) -- a ratified, well-documented account-level
//! singleton (`id: "singleton"`, `get`/`set` only, no create/destroy),
//! unlike the EAS wire format around it which has needed empirical live
//! discovery for nearly everything else in this project. Confirmed present
//! in this Stalwart instance's advertised capabilities (see
//! `jmap::notes` module doc: a live session response listed
//! `vacationresponse` alongside `core/mail/calendars/...`).
//!
//! Deliberately conservative about what got live-verified here: `Get`
//! (read-only) and a `Set` with `isEnabled: false` were both exercised
//! live against the real account. `Set` with `isEnabled: true` was
//! **not** live-toggled, on purpose -- `VacationResponse` is real
//! account-level state in Stalwart, not gateway-local; flipping it on
//! for even a few seconds risks a genuine auto-reply going out to a real
//! sender on the live mailbox with nobody watching (this was implemented
//! unsupervised, overnight). The EAS-side wire shape for the *enabled*
//! Get response is therefore spec-derived (MS-ASSETTINGS's own
//! OofMessage/AppliesToInternal/Enabled/ReplyMessage/BodyType schema,
//! read directly from the primary spec, not guessed), not empirically
//! confirmed against a real toggled-on response the way the disabled
//! shape was. Also worth knowing: neither this project's own prior code
//! NOR the z-push PHP reference this project treats as a compatibility
//! oracle ever actually implemented Oof against a real backend (grepped
//! z-push's jmap.php backend class directly: zero mentions of OOF/Oof at
//! all, despite `CAP_VACATION` being declared in jmap_client.php and
//! never used) -- so there was no existing live-verified reference for
//! the enabled-state shape to check against either. Verify the enabled
//! path with a real device present before fully trusting it.

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

/// Fields settable via `Settings>Oof>Set`. `from_date`/`to_date` are
/// `Some(None)` vs `None` isn't distinguished here -- every Set this
/// gateway issues replaces the whole patch, matching how the EAS Set
/// command itself always carries a complete replacement shape, not a
/// partial one.
pub struct VacationResponseUpdate {
    pub is_enabled: bool,
    pub subject: Option<String>,
    pub text_body: Option<String>,
    pub from_date: Option<String>,
    pub to_date: Option<String>,
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
        patch.insert(
            "isEnabled".to_owned(),
            serde_json::Value::Bool(update.is_enabled),
        );
        patch.insert(
            "subject".to_owned(),
            update
                .subject
                .map(serde_json::Value::String)
                .unwrap_or(serde_json::Value::Null),
        );
        patch.insert(
            "textBody".to_owned(),
            update
                .text_body
                .map(serde_json::Value::String)
                .unwrap_or(serde_json::Value::Null),
        );
        patch.insert(
            "fromDate".to_owned(),
            update
                .from_date
                .map(serde_json::Value::String)
                .unwrap_or(serde_json::Value::Null),
        );
        patch.insert(
            "toDate".to_owned(),
            update
                .to_date
                .map(serde_json::Value::String)
                .unwrap_or(serde_json::Value::Null),
        );

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

        for method in response.method_responses {
            if method.0 == "error" {
                anyhow::bail!("JMAP method error in VacationResponse/set");
            }
            if method.0 == "VacationResponse/set" {
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
        Ok(())
    }
}
