use super::{EasQuery, ProtocolVersion};
use crate::{
    http_server::AppState,
    jmap::client::AuthenticatedSession,
    wbxml::{
        self,
        eas::{self, meeting_response as mr, DocumentBuilder},
        token::Token,
        Document, Node,
    },
};
use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};

fn is(doc: &Document, expected: Token) -> bool {
    matches!(doc.nodes.first(), Some(Node::Start(token)) if token.code_page == expected.code_page && token.token == expected.token)
}

pub(super) async fn handle(
    state: &AppState,
    auth: &AuthenticatedSession,
    query: &EasQuery,
    doc: &Document,
    version: ProtocolVersion,
) -> Response {
    let mut b = DocumentBuilder::new();
    b.start(mr::MEETING_RESPONSE);
    let requests = eas::child_elements(doc);
    if !is(doc, mr::MEETING_RESPONSE) || requests.is_empty() || requests.len() > 100 {
        b.start(mr::RESULT);
        b.leaf(mr::STATUS, "2");
        b.end();
    } else {
        for request in requests {
            b.start(mr::RESULT);
            let id = eas::find_text_after(&request, mr::REQUEST_ID).unwrap_or("");
            if !id.is_empty() {
                b.leaf(mr::REQUEST_ID, id);
            }
            let long_id = eas::find_text_after(&request, eas::search::LONG_ID);
            let (folder, lookup_failed) = if long_id.is_some() {
                match state.jmap.find_mailbox_by_role(auth, "inbox").await {
                    Ok(folder) => (folder.unwrap_or_default(), false),
                    Err(error) => {
                        tracing::warn!(%error, "meeting inbox lookup failed");
                        (String::new(), true)
                    }
                }
            } else {
                (
                    eas::find_text_after(&request, mr::COLLECTION_ID)
                        .unwrap_or("")
                        .to_owned(),
                    false,
                )
            };
            let item_id = long_id.unwrap_or(id);
            let response = eas::find_text_after(&request, mr::USER_RESPONSE).unwrap_or("");
            let send_response = eas::contains_token(&request, mr::SEND_RESPONSE);
            let unsupported = [
                mr::INSTANCE_ID,
                mr::PROPOSED_START_TIME,
                mr::PROPOSED_END_TIME,
            ]
            .iter()
            .any(|t| eas::contains_token(&request, *t))
                || (send_response
                    && eas::contains_token(&request, crate::wbxml::eas::airsync_base::BODY));
            let fields = eas::child_elements(&request);
            let required = [mr::REQUEST_ID, mr::COLLECTION_ID, mr::USER_RESPONSE];
            let count = |token| fields.iter().filter(|field| is(field, token)).count();
            let valid_target = match (
                count(eas::search::LONG_ID),
                count(mr::REQUEST_ID),
                count(mr::COLLECTION_ID),
            ) {
                (1, 0, 0) => long_id.is_some_and(|id| !id.is_empty() && id.len() <= 256),
                (0, 1, 1) => !id.is_empty(),
                _ => false,
            };
            let valid_fields = valid_target
                && count(mr::USER_RESPONSE) == 1
                && (!send_response
                    || matches!(version, ProtocolVersion::V16_0 | ProtocolVersion::V16_1))
                && fields.iter().all(|field| {
                    required
                        .iter()
                        .chain([eas::search::LONG_ID].iter())
                        .chain(
                            [
                                mr::INSTANCE_ID,
                                mr::SEND_RESPONSE,
                                mr::PROPOSED_START_TIME,
                                mr::PROPOSED_END_TIME,
                            ]
                            .iter(),
                        )
                        .any(|token| is(field, *token))
                });
            let mut calendar_id = None;
            let status = if !is(&request, mr::REQUEST)
                || !valid_fields
                || item_id.is_empty()
                || (!lookup_failed && folder.is_empty())
                || !matches!(response, "1" | "2" | "3")
            {
                "2"
            } else if lookup_failed || unsupported {
                "4"
            } else if matches!(version, ProtocolVersion::V14_0) && folder.starts_with("cal_") {
                "2"
            } else {
                match state
                    .state
                    .get(
                        auth.username(),
                        query.device_id.as_deref().unwrap_or(""),
                        &folder,
                    )
                    .await
                {
                    Ok(None) => "2",
                    Err(error) => {
                        tracing::warn!(%error, "meeting sync state unavailable");
                        "4"
                    }
                    Ok(Some(_)) => {
                        let participation = match response {
                            "1" => "accepted",
                            "2" => "tentative",
                            _ => "declined",
                        };
                        let user_push = state.push.user(&auth.username().to_ascii_lowercase());
                        let lock = user_push.sync_lock("__meeting_response__");
                        let _guard = lock.lock().await;
                        let target = if let Some(calendar) = folder.strip_prefix("cal_") {
                            Ok(Some((calendar.to_owned(), id.to_owned(), None)))
                        } else {
                            state
                                .jmap
                                .calendar_event_for_invitation(
                                    auth,
                                    &folder,
                                    item_id,
                                    participation,
                                )
                                .await
                                .map(|target| {
                                    target.map(|(calendar, event, organizer)| {
                                        (calendar, event, Some(organizer))
                                    })
                                })
                        };
                        match target {
                            Ok(Some((_, event, _))) if event.is_empty() && response == "3" => "1",
                            Ok(Some((calendar, event, organizer))) => match state
                                .jmap
                                .respond_calendar_event(
                                    auth,
                                    &calendar,
                                    &event,
                                    participation,
                                    organizer.as_deref(),
                                    send_response,
                                )
                                .await
                            {
                                Ok(true) => {
                                    calendar_id = Some(event);
                                    "1"
                                }
                                Ok(false) => "2",
                                Err(error) => {
                                    tracing::warn!(%error, "meeting response failed");
                                    "4"
                                }
                            },
                            Ok(None) => "2",
                            Err(error) => {
                                tracing::warn!(%error, "meeting invitation lookup failed");
                                "4"
                            }
                        }
                    }
                }
            };
            b.leaf(mr::STATUS, status);
            if status == "1" && response != "3" {
                b.leaf(
                    mr::CALENDAR_ID,
                    calendar_id.expect("successful meeting has a calendar id"),
                );
            }
            b.end();
        }
    }
    b.end();
    (
        StatusCode::OK,
        [("Content-Type", "application/vnd.ms-sync.wbxml")],
        wbxml::encode_document(&b.finish()),
    )
        .into_response()
}
