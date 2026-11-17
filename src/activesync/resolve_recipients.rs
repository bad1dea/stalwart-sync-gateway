use crate::{
    http_server::AppState,
    jmap::{capabilities, client::AuthenticatedSession},
    wbxml::{
        self,
        eas::{self, resolve_recipients as rr, DocumentBuilder},
        token::Token,
        Document, Node,
    },
};
use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use std::collections::BTreeMap;

fn is(doc: &Document, expected: Token) -> bool {
    matches!(doc.nodes.first(), Some(Node::Start(token)) if token.code_page == expected.code_page && token.token == expected.token)
}

#[derive(Clone)]
struct Recipient {
    name: String,
    email: String,
    alias: String,
    kind: &'static str,
}

fn matching<'a>(query: &str, candidates: &'a [Recipient]) -> Vec<&'a Recipient> {
    let query = query.trim().to_lowercase();
    let exact: Vec<_> = candidates
        .iter()
        .filter(|r| {
            [&r.email, &r.name, &r.alias]
                .iter()
                .any(|s| s.to_lowercase() == query)
        })
        .collect();
    if !exact.is_empty() {
        return exact;
    }
    candidates
        .iter()
        .filter(|r| {
            [&r.email, &r.name, &r.alias]
                .iter()
                .any(|s| s.to_lowercase().contains(&query))
        })
        .collect()
}

fn request(doc: &Document) -> Result<(Vec<String>, usize), &'static str> {
    if !is(doc, rr::RESOLVE_RECIPIENTS) {
        return Err("5");
    }
    let mut queries = Vec::new();
    let mut max = 100usize;
    let mut options_seen = false;
    for child in eas::child_elements(doc) {
        if is(&child, rr::TO) {
            let value = eas::find_text_after(&child, rr::TO)
                .filter(|s| !s.trim().is_empty())
                .ok_or("5")?;
            queries.push(value.to_owned());
        } else if is(&child, rr::OPTIONS) && !options_seen {
            options_seen = true;
            let mut seen = std::collections::BTreeSet::new();
            for option in eas::child_elements(&child) {
                let Some(Node::Start(token)) = option.nodes.first() else {
                    return Err("5");
                };
                if !seen.insert((token.code_page, token.token)) {
                    return Err("5");
                }
                if is(&option, rr::MAX_AMBIGUOUS_RECIPIENTS) || is(&option, rr::MAX_CERTIFICATES) {
                    let value: usize = eas::find_text_after(&option, *token)
                        .ok_or("5")?
                        .parse()
                        .map_err(|_| "5")?;
                    if value > 9999 {
                        return Err("5");
                    }
                    if is(&option, rr::MAX_AMBIGUOUS_RECIPIENTS) {
                        max = value;
                    }
                } else if is(&option, rr::CERTIFICATE_RETRIEVAL) {
                    match eas::find_text_after(&option, rr::CERTIFICATE_RETRIEVAL) {
                        Some("1") => {}
                        Some("2" | "3") => return Err("6"),
                        _ => return Err("5"),
                    }
                } else if is(&option, rr::AVAILABILITY) || is(&option, rr::PICTURE) {
                    return Err("6");
                } else {
                    return Err("5");
                }
            }
        } else {
            return Err("5");
        }
    }
    if queries.is_empty() || queries.len() > 100 {
        return Err("5");
    }
    Ok((queries, max))
}

pub(super) async fn handle(
    state: &AppState,
    auth: &AuthenticatedSession,
    document: &Document,
) -> Response {
    let parsed = request(document);
    let mut b = DocumentBuilder::new();
    b.start(rr::RESOLVE_RECIPIENTS);
    let (queries, max) = match parsed {
        Ok(v) => v,
        Err(status) => {
            b.leaf(rr::STATUS, status);
            b.end();
            return response(b);
        }
    };
    let result = async {
        let mut candidates = BTreeMap::new();
        if auth
            .session
            .primary_account_for(capabilities::PRINCIPALS)
            .is_some()
        {
            for p in state.jmap.all_principals(auth).await? {
                if let Some(email) = p.email.filter(|s| !s.is_empty()) {
                    candidates.insert(
                        (email.to_lowercase(), "1"),
                        Recipient {
                            name: p.display_name,
                            email,
                            alias: p.alias,
                            kind: "1",
                        },
                    );
                }
            }
        }
        if auth
            .session
            .primary_account_for(capabilities::CONTACTS)
            .is_some()
        {
            let (contacts, count) = state
                .jmap
                .search_contacts(auth, None, &[], None, 0, 10000)
                .await?;
            anyhow::ensure!(
                count as usize == contacts.len(),
                "too many contacts to resolve"
            );
            for c in contacts {
                let name = c.file_as.filter(|s| !s.is_empty()).unwrap_or_else(|| {
                    [
                        c.first_name.as_deref().unwrap_or(""),
                        c.last_name.as_deref().unwrap_or(""),
                    ]
                    .join(" ")
                    .trim()
                    .to_owned()
                });
                for email in c.emails.into_iter().filter(|s| !s.is_empty()) {
                    candidates.insert(
                        (email.to_lowercase(), "2"),
                        Recipient {
                            name: if name.is_empty() {
                                email.clone()
                            } else {
                                name.clone()
                            },
                            email,
                            alias: String::new(),
                            kind: "2",
                        },
                    );
                }
            }
        }
        Ok::<_, anyhow::Error>(candidates.into_values().collect::<Vec<_>>())
    }
    .await;
    let candidates = match result {
        Ok(v) => v,
        Err(error) => {
            tracing::warn!(%error, "recipient resolution failed");
            b.leaf(rr::STATUS, "6");
            b.end();
            return response(b);
        }
    };
    b.leaf(rr::STATUS, "1");
    for query in queries {
        let items = matching(&query, &candidates);
        let count = items.len();
        let shown = if count == 1 { 1 } else { count.min(max) };
        b.start(rr::RESPONSE);
        b.leaf(rr::TO, query);
        b.leaf(
            rr::STATUS,
            match count {
                0 => "4",
                1 => "1",
                _ if shown < count => "3",
                _ => "2",
            },
        );
        b.leaf(rr::RECIPIENT_COUNT, count.to_string());
        for item in items.into_iter().take(shown) {
            b.start(rr::RECIPIENT);
            b.leaf(rr::TYPE, item.kind);
            b.leaf(rr::DISPLAY_NAME, &item.name);
            b.leaf(rr::EMAIL_ADDRESS, &item.email);
            b.end();
        }
        b.end();
    }
    b.end();
    response(b)
}

fn response(builder: DocumentBuilder) -> Response {
    (
        StatusCode::OK,
        [("Content-Type", "application/vnd.ms-sync.wbxml")],
        wbxml::encode_document(&builder.finish()),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_recipient_wins_over_suggestions() {
        let recipients = vec![
            Recipient {
                name: "Alice".into(),
                email: "alice@example.test".into(),
                alias: "alice".into(),
                kind: "1",
            },
            Recipient {
                name: "Alice Smith".into(),
                email: "smith@example.test".into(),
                alias: String::new(),
                kind: "2",
            },
        ];
        assert_eq!(matching("ALICE", &recipients).len(), 1);
        assert_eq!(matching("ali", &recipients).len(), 2);
        assert!(matching("unknown", &recipients).is_empty());
    }
}
