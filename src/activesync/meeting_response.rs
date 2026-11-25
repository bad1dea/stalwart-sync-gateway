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

fn invalid_instance_id_status(value: Option<&str>) -> Option<&'static str> {
    value
        .filter(|value| chrono::DateTime::parse_from_rfc3339(value).is_err())
        .map(|_| "104")
}

fn response_payload(
    request: &Document,
) -> Result<
    (
        Option<crate::jmap::client::MeetingResponseBody>,
        Option<crate::jmap::client::MeetingResponseProposal>,
    ),
    (),
> {
    use crate::wbxml::eas::airsync_base as ab;
    let send_response = eas::child_elements(request)
        .into_iter()
        .find(|child| is(child, mr::SEND_RESPONSE));
    let Some(send_response) = send_response else {
        return Ok((None, None));
    };
    let fields = eas::child_elements(&send_response);
    if fields.iter().any(|field| {
        !is(field, ab::BODY)
            && !is(field, mr::PROPOSED_START_TIME)
            && !is(field, mr::PROPOSED_END_TIME)
    }) {
        return Err(());
    }
    let bodies: Vec<_> = fields.iter().filter(|field| is(field, ab::BODY)).collect();
    if bodies.len() > 1 {
        return Err(());
    }
    let body = if let Some(body) = bodies.first() {
        let body_fields = eas::child_elements(body);
        let types: Vec<_> = body_fields
            .iter()
            .filter(|field| is(field, ab::TYPE))
            .collect();
        let data: Vec<_> = body_fields
            .iter()
            .filter(|field| is(field, ab::DATA))
            .collect();
        let sizes: Vec<_> = body_fields
            .iter()
            .filter(|field| is(field, ab::ESTIMATED_DATA_SIZE))
            .collect();
        let truncations: Vec<_> = body_fields
            .iter()
            .filter(|field| is(field, ab::TRUNCATED))
            .collect();
        let previews: Vec<_> = body_fields
            .iter()
            .filter(|field| is(field, ab::PREVIEW))
            .collect();
        let allowed_fields =
            types.len() + data.len() + sizes.len() + truncations.len() + previews.len();
        if types.len() != 1
            || data.len() > 1
            || sizes.len() > 1
            || truncations.len() > 1
            || previews.len() > 1
            || body_fields.len() != allowed_fields
        {
            return Err(());
        }
        if let Some(size) = sizes.first() {
            eas::find_text_after(size, ab::ESTIMATED_DATA_SIZE)
                .and_then(|value| value.parse::<u32>().ok())
                .ok_or(())?;
        }
        if let Some(truncated) = truncations.first() {
            if !matches!(
                eas::find_text_after(truncated, ab::TRUNCATED),
                Some("0" | "1")
            ) {
                return Err(());
            }
        }
        let kind = eas::find_text_after(types[0], ab::TYPE)
            .and_then(|value| value.parse::<u8>().ok())
            .filter(|kind| matches!(kind, 1..=3))
            .ok_or(())?;
        let data = data
            .first()
            .and_then(|field| eas::find_text_after(field, ab::DATA))
            .unwrap_or("");
        if data.len() > 256 * 1024 {
            return Err(());
        }
        Some(crate::jmap::client::MeetingResponseBody {
            kind,
            data: data.to_owned(),
        })
    } else {
        None
    };
    let starts: Vec<_> = fields
        .iter()
        .filter(|field| is(field, mr::PROPOSED_START_TIME))
        .collect();
    let ends: Vec<_> = fields
        .iter()
        .filter(|field| is(field, mr::PROPOSED_END_TIME))
        .collect();
    if starts.len() > 1 || ends.len() > 1 || starts.is_empty() != ends.is_empty() {
        return Err(());
    }
    let proposal = if let (Some(start), Some(end)) = (starts.first(), ends.first()) {
        let start = eas::find_text_after(start, mr::PROPOSED_START_TIME)
            .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
            .map(|value| value.with_timezone(&chrono::Utc))
            .ok_or(())?;
        let end = eas::find_text_after(end, mr::PROPOSED_END_TIME)
            .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
            .map(|value| value.with_timezone(&chrono::Utc))
            .filter(|end| *end > start)
            .ok_or(())?;
        Some(crate::jmap::client::MeetingResponseProposal { start, end })
    } else {
        None
    };
    Ok((body, proposal))
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
            let instance_id = eas::find_text_after(&request, mr::INSTANCE_ID);
            let payload = response_payload(&request);
            let unsupported = payload.is_err();
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
                && count(mr::INSTANCE_ID) <= 1
                && count(mr::SEND_RESPONSE) <= 1
                && (instance_id.is_none() || !matches!(version, ProtocolVersion::V14_0))
                && (!send_response
                    || matches!(version, ProtocolVersion::V16_0 | ProtocolVersion::V16_1))
                && fields.iter().all(|field| {
                    required
                        .iter()
                        .chain([eas::search::LONG_ID].iter())
                        .chain([mr::INSTANCE_ID, mr::SEND_RESPONSE].iter())
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
            } else if let Some(status) = invalid_instance_id_status(instance_id) {
                status
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
                                    crate::jmap::client::MeetingResponseOptions {
                                        expected_organizer: organizer.as_deref(),
                                        send_response,
                                        custom_body: payload
                                            .as_ref()
                                            .ok()
                                            .and_then(|(body, _)| body.clone()),
                                        proposed_times: payload
                                            .as_ref()
                                            .ok()
                                            .and_then(|(_, proposal)| proposal.clone()),
                                        instance_id,
                                    },
                                )
                                .await
                            {
                                Ok("1") => {
                                    calendar_id = Some(event);
                                    "1"
                                }
                                Ok(status) => status,
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
            if status == "1"
                && instance_id.is_some()
                && matches!(version, ProtocolVersion::V16_0 | ProtocolVersion::V16_1)
            {
                b.leaf(mr::INSTANCE_ID, instance_id.unwrap_or_default());
            }
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

#[cfg(test)]
mod tests {
    use super::{invalid_instance_id_status, response_payload};
    use crate::wbxml::{
        eas::{airsync_base as ab, meeting_response as mr},
        Document, Node,
    };

    fn start(token: crate::wbxml::token::Token) -> Node {
        Node::Start(crate::wbxml::token::Token {
            has_content: true,
            ..token
        })
    }

    #[test]
    fn meeting_instance_id_uses_eas_invalid_format_status() {
        assert_eq!(invalid_instance_id_status(Some("not-a-date")), Some("104"));
        assert_eq!(invalid_instance_id_status(Some("")), Some("104"));
        assert_eq!(
            invalid_instance_id_status(Some("2026-11-25T15:00:00.000Z")),
            None
        );
        assert_eq!(invalid_instance_id_status(None), None);
    }

    #[test]
    fn custom_meeting_response_body_parses_plain_text_and_html_types() {
        for (kind, expected) in [("1", 1), ("2", 2), ("3", 3)] {
            let doc = Document {
                nodes: vec![
                    start(mr::REQUEST),
                    start(mr::SEND_RESPONSE),
                    start(ab::BODY),
                    start(ab::TYPE),
                    Node::Text(kind.into()),
                    Node::End,
                    start(ab::DATA),
                    Node::Text("<b>I'll attend</b>".into()),
                    Node::End,
                    Node::End,
                    Node::End,
                    Node::End,
                ],
            };
            let (body, proposal) = response_payload(&doc).unwrap();
            let body = body.unwrap();
            assert!(proposal.is_none());
            assert_eq!(body.kind, expected);
            assert_eq!(body.data, "<b>I'll attend</b>");
        }
    }

    #[test]
    fn custom_meeting_response_body_rejects_unsupported_and_oversized_payloads() {
        let invalid = Document {
            nodes: vec![
                start(mr::REQUEST),
                start(mr::SEND_RESPONSE),
                start(ab::BODY),
                start(ab::TYPE),
                Node::Text("4".into()),
                Node::End,
                start(ab::DATA),
                Node::Text("body".into()),
                Node::End,
                Node::End,
                Node::End,
                Node::End,
            ],
        };
        assert!(response_payload(&invalid).is_err());

        let oversized = Document {
            nodes: vec![
                start(mr::REQUEST),
                start(mr::SEND_RESPONSE),
                start(ab::BODY),
                start(ab::TYPE),
                Node::Text("1".into()),
                Node::End,
                start(ab::DATA),
                Node::Text("x".repeat(256 * 1024 + 1)),
                Node::End,
                Node::End,
                Node::End,
                Node::End,
            ],
        };
        assert!(response_payload(&oversized).is_err());
    }

    #[test]
    fn proposed_times_require_a_valid_ordered_pair() {
        let doc = Document {
            nodes: vec![
                start(mr::REQUEST),
                start(mr::SEND_RESPONSE),
                start(mr::PROPOSED_START_TIME),
                Node::Text("2026-11-25T15:00:00Z".into()),
                Node::End,
                start(mr::PROPOSED_END_TIME),
                Node::Text("2026-11-25T16:00:00Z".into()),
                Node::End,
                Node::End,
                Node::End,
            ],
        };
        let (body, proposal) = response_payload(&doc).unwrap();
        assert!(body.is_none());
        let proposal = proposal.unwrap();
        assert_eq!(proposal.start.to_rfc3339(), "2026-11-25T15:00:00+00:00");
        assert_eq!(proposal.end.to_rfc3339(), "2026-11-25T16:00:00+00:00");

        let reversed = Document {
            nodes: vec![
                start(mr::REQUEST),
                start(mr::SEND_RESPONSE),
                start(mr::PROPOSED_START_TIME),
                Node::Text("2026-11-25T16:00:00Z".into()),
                Node::End,
                start(mr::PROPOSED_END_TIME),
                Node::Text("2026-11-25T15:00:00Z".into()),
                Node::End,
                Node::End,
                Node::End,
            ],
        };
        assert!(response_payload(&reversed).is_err());
    }
}
