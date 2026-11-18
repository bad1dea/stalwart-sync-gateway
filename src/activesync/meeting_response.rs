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
            let folder = eas::find_text_after(&request, mr::COLLECTION_ID).unwrap_or("");
            let response = eas::find_text_after(&request, mr::USER_RESPONSE).unwrap_or("");
            let unsupported = [
                mr::INSTANCE_ID,
                mr::SEND_RESPONSE,
                mr::PROPOSED_START_TIME,
                mr::PROPOSED_END_TIME,
            ]
            .iter()
            .any(|t| eas::contains_token(&request, *t));
            let fields = eas::child_elements(&request);
            let required = [mr::REQUEST_ID, mr::COLLECTION_ID, mr::USER_RESPONSE];
            let valid_fields = required
                .iter()
                .all(|token| fields.iter().filter(|field| is(field, *token)).count() == 1)
                && fields.iter().all(|field| {
                    required
                        .iter()
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
            let status = if !is(&request, mr::REQUEST)
                || !valid_fields
                || id.is_empty()
                || folder.is_empty()
                || !matches!(response, "1" | "2" | "3")
            {
                "2"
            } else if unsupported || response == "3" {
                "4"
            } else if matches!(version, ProtocolVersion::V14_0) || !folder.starts_with("cal_") {
                "2"
            } else {
                match state
                    .state
                    .get(
                        auth.username(),
                        query.device_id.as_deref().unwrap_or(""),
                        folder,
                    )
                    .await
                {
                    Ok(None) => "2",
                    Err(error) => {
                        tracing::warn!(%error, "meeting sync state unavailable");
                        "4"
                    }
                    Ok(Some(_)) => {
                        let participation = if response == "1" {
                            "accepted"
                        } else {
                            "tentative"
                        };
                        match state
                            .jmap
                            .respond_calendar_event(auth, &folder[4..], id, participation)
                            .await
                        {
                            Ok(true) => "1",
                            Ok(false) => "2",
                            Err(error) => {
                                tracing::warn!(%error, "meeting response failed");
                                "4"
                            }
                        }
                    }
                }
            };
            b.leaf(mr::STATUS, status);
            if status == "1" {
                b.leaf(mr::CALENDAR_ID, id);
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
