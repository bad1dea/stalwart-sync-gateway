use super::ProtocolVersion;
use crate::{
    http_server::AppState,
    jmap::client::AuthenticatedSession,
    model::{Collection, CollectionKind},
    wbxml::{
        self,
        eas::{self, airsync as air, search as srch, DocumentBuilder, SearchRequest},
        token::Token,
        Document, Node,
    },
};
use anyhow::{bail, Context};
use axum::{
    body::to_bytes,
    http::StatusCode,
    response::{IntoResponse, Response},
};

pub(super) async fn handle_multiple_stores(
    state: &AppState,
    auth: &AuthenticatedSession,
    document: &Document,
    version: ProtocolVersion,
    command: &str,
) -> Option<Response> {
    let stores: Vec<_> = eas::child_elements(document)
        .into_iter()
        .filter(|child| is(child, srch::STORE))
        .collect();
    if stores.len() <= 1 {
        return None;
    }

    let mut builder = DocumentBuilder::new();
    builder.start(srch::SEARCH);
    builder.leaf(srch::STATUS, "1");
    builder.start(srch::RESPONSE);
    for store in stores {
        let mut request = DocumentBuilder::new();
        request.start(srch::SEARCH);
        request.extend(store.nodes);
        request.end();
        let response = Box::pin(super::search_inner(
            state,
            auth,
            &request.finish(),
            version,
            command,
            false,
        ))
        .await;
        let response_store = async {
            anyhow::ensure!(response.status() == StatusCode::OK, "search store failed");
            let body = to_bytes(response.into_body(), usize::MAX).await?;
            let decoded = wbxml::decode_document(&body)?;
            let response = eas::child_elements(&decoded)
                .into_iter()
                .find(|child| is(child, srch::RESPONSE))
                .context("search response missing")?;
            eas::child_elements(&response)
                .into_iter()
                .find(|child| is(child, srch::STORE))
                .context("search store missing")
        }
        .await;
        match response_store {
            Ok(store) => builder.extend(store.nodes),
            Err(error) => {
                tracing::warn!(%error, "multi-store search failed");
                builder.start(srch::STORE);
                builder.leaf(srch::STATUS, "3");
                builder.end();
            }
        }
    }
    builder.end();
    builder.end();
    Some(
        (
            StatusCode::OK,
            [("Content-Type", "application/vnd.ms-sync.wbxml")],
            wbxml::encode_document(&builder.finish()),
        )
            .into_response(),
    )
}

fn is(document: &Document, expected: Token) -> bool {
    matches!(document.nodes.first(), Some(Node::Start(token)) if token.code_page == expected.code_page && token.token == expected.token)
}

fn append_without_classes(builder: &mut DocumentBuilder, clause: Document) {
    if is(&clause, air::CLASS) {
        return;
    }
    if is(&clause, srch::AND) {
        builder.start(srch::AND);
        for child in eas::child_elements(&clause) {
            append_without_classes(builder, child);
        }
        builder.end();
    } else {
        builder.extend(clause.nodes);
    }
}

fn query_for_class(
    document: &Document,
    class: &str,
    start: u64,
    end: u64,
) -> anyhow::Result<Document> {
    let stores: Vec<_> = eas::child_elements(document)
        .into_iter()
        .filter(|child| is(child, srch::STORE))
        .collect();
    anyhow::ensure!(stores.len() == 1, "multiple search stores are unsupported");
    let children = eas::child_elements(&stores[0]);
    anyhow::ensure!(
        children
            .iter()
            .filter(|child| is(child, srch::QUERY))
            .count()
            == 1,
        "search query missing"
    );
    anyhow::ensure!(
        children
            .iter()
            .filter(|child| is(child, srch::OPTIONS))
            .count()
            <= 1,
        "multiple search options are unsupported"
    );
    let mut builder = DocumentBuilder::new();
    builder.start(srch::SEARCH);
    builder.start(srch::STORE);
    let mut had_options = false;
    for child in children {
        if is(&child, srch::QUERY) {
            let mut depth = 0usize;
            for node in &child.nodes {
                match node {
                    Node::Start(token) if token.has_content => depth += 1,
                    Node::End => depth = depth.saturating_sub(1),
                    Node::Text(_) | Node::Opaque(_) if depth == 1 => {
                        bail!("unsupported text query")
                    }
                    _ => {}
                }
            }
            builder.start(srch::QUERY);
            builder.start(srch::AND);
            builder.leaf(air::CLASS, class);
            for clause in eas::child_elements(&child) {
                if is(&clause, srch::AND) {
                    for nested in eas::child_elements(&clause) {
                        append_without_classes(&mut builder, nested);
                    }
                } else {
                    append_without_classes(&mut builder, clause);
                }
            }
            builder.end();
            builder.end();
        } else if is(&child, srch::OPTIONS) {
            had_options = true;
            builder.start(srch::OPTIONS);
            for option in eas::child_elements(&child) {
                if !is(&option, srch::RANGE) {
                    builder.extend(option.nodes);
                }
            }
            builder.leaf(srch::RANGE, format!("{start}-{end}"));
            builder.end();
        } else {
            builder.extend(child.nodes);
        }
    }
    if !had_options {
        builder.start(srch::OPTIONS);
        builder.leaf(srch::RANGE, format!("{start}-{end}"));
        builder.end();
    }
    builder.end();
    builder.end();
    Ok(builder.finish())
}

#[derive(PartialEq, Eq)]
struct Snapshot {
    folders: Vec<Collection>,
    mail: Option<String>,
    contacts: Option<String>,
    calendar: Option<(String, String)>,
}

async fn snapshot(
    state: &AppState,
    auth: &AuthenticatedSession,
    classes: &[&str],
) -> anyhow::Result<Snapshot> {
    let mut folders = state.jmap.collections(auth).await?;
    folders.sort_by(|a, b| a.id.cmp(&b.id));
    let mail = if classes
        .iter()
        .any(|class| matches!(*class, "Email" | "Notes"))
    {
        let value = state.jmap.email_state(auth).await?;
        anyhow::ensure!(!value.is_empty(), "mail search state unavailable");
        Some(value)
    } else {
        None
    };
    let contacts = if classes.contains(&"Contacts") {
        Some(state.jmap.contact_snapshot_state(auth).await?)
    } else {
        None
    };
    let calendar = if classes
        .iter()
        .any(|class| matches!(*class, "Calendar" | "Tasks"))
    {
        Some(state.jmap.calendar_snapshot_state(auth).await?)
    } else {
        None
    };
    Ok(Snapshot {
        folders,
        mail,
        contacts,
        calendar,
    })
}

#[derive(Debug)]
struct SearchStatus(String);

impl std::fmt::Display for SearchStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "search status {}", self.0)
    }
}

impl std::error::Error for SearchStatus {}

struct Page {
    total: u64,
    results: Vec<Document>,
}

async fn page(
    state: &AppState,
    auth: &AuthenticatedSession,
    document: &Document,
    version: ProtocolVersion,
    command: &str,
    class: &str,
    range: (u64, u64),
) -> anyhow::Result<Page> {
    let (start, end) = range;
    let query = query_for_class(document, class, start, end)?;
    let response = Box::pin(super::search_inner(
        state, auth, &query, version, command, true,
    ))
    .await;
    anyhow::ensure!(
        response.status() == StatusCode::OK,
        "search class request failed"
    );
    let body = to_bytes(response.into_body(), usize::MAX).await?;
    let decoded = wbxml::decode_document(&body)?;
    let response = eas::child_elements(&decoded)
        .into_iter()
        .find(|child| is(child, srch::RESPONSE))
        .context("search response missing")?;
    let store = eas::child_elements(&response)
        .into_iter()
        .find(|child| is(child, srch::STORE))
        .context("search store missing")?;
    let children = eas::child_elements(&store);
    let status = children
        .iter()
        .find(|child| is(child, srch::STATUS))
        .and_then(|child| eas::find_text_after(child, srch::STATUS))
        .context("search status missing")?;
    if status != "1" {
        return Err(SearchStatus(status.to_owned()).into());
    }
    let total: u64 = children
        .iter()
        .find(|child| is(child, srch::TOTAL))
        .and_then(|child| eas::find_text_after(child, srch::TOTAL))
        .context("search total missing")?
        .parse()?;
    let range = children
        .iter()
        .find(|child| is(child, srch::RANGE))
        .and_then(|child| eas::find_text_after(child, srch::RANGE));
    let results: Vec<_> = children
        .iter()
        .filter(|child| is(child, srch::RESULT))
        .cloned()
        .collect();
    let expected = total.saturating_sub(start).min(end - start + 1) as usize;
    anyhow::ensure!(results.len() == expected, "incomplete search page");
    let expected_range = (expected > 0).then(|| format!("{start}-{}", start + expected as u64 - 1));
    anyhow::ensure!(range == expected_range.as_deref(), "invalid search range");
    Ok(Page { total, results })
}

pub(super) async fn handle(
    state: &AppState,
    auth: &AuthenticatedSession,
    document: &Document,
    request: &SearchRequest,
    version: ProtocolVersion,
    command: &str,
) -> Response {
    let result = async {
        let mut folders = state.jmap.collections(auth).await?;
        folders.sort_by(|a, b| a.id.cmp(&b.id));
        if let Some(folder) = &request.collection_id {
            anyhow::ensure!(
                folders.iter().any(|item| &item.id == folder),
                "unknown search folder"
            );
        }
        let mut classes = Vec::new();
        for (class, kind) in [
            ("Email", CollectionKind::Mail),
            ("Contacts", CollectionKind::Contacts),
            ("Calendar", CollectionKind::Calendar),
            ("Tasks", CollectionKind::Tasks),
            ("Notes", CollectionKind::Notes),
        ] {
            let requested = request.classes.is_empty()
                || request
                    .classes
                    .iter()
                    .any(|item| item.eq_ignore_ascii_case(class));
            let in_scope = folders.iter().any(|folder| {
                folder.kind == kind
                    && request
                        .collection_id
                        .as_ref()
                        .is_none_or(|id| &folder.id == id)
            });
            if requested && in_scope {
                classes.push(class);
            }
        }
        if request.received_after.is_some()
            || request.received_before.is_some()
            || request.conversation_id.is_some()
        {
            classes.retain(|class| *class == "Email");
        }
        if request
            .or_terms
            .as_ref()
            .is_some_and(|or| or.received_after.is_some() || or.received_before.is_some())
            && !classes.iter().all(|class| *class == "Email")
        {
            return Err(SearchStatus("8".to_owned()).into());
        }
        let before = snapshot(state, auth, &classes).await?;
        anyhow::ensure!(before.folders == folders, "folders changed during search");
        let (position, limit) = request
            .range
            .map(|(start, end)| (start as u64, (end - start + 1) as usize))
            .unwrap_or((0, 100));
        let mut total = 0u64;
        let mut results = Vec::new();
        for class in &classes {
            let class_start = position.saturating_sub(total);
            let remaining = limit - results.len();
            let range = if remaining == 0 {
                (0, 0)
            } else {
                (
                    class_start,
                    class_start
                        .checked_add(remaining as u64 - 1)
                        .context("search range overflow")?,
                )
            };
            let items = page(state, auth, document, version, command, class, range).await?;
            total = total
                .checked_add(items.total)
                .context("search total overflow")?;
            if remaining > 0 {
                results.extend(items.results);
            }
        }
        anyhow::ensure!(
            before == snapshot(state, auth, &classes).await?,
            "mailbox changed during search"
        );
        Ok::<_, anyhow::Error>((position, total, results))
    }
    .await;
    let mut builder = DocumentBuilder::new();
    builder.start(srch::SEARCH);
    builder.leaf(srch::STATUS, "1");
    builder.start(srch::RESPONSE);
    builder.start(srch::STORE);
    match result {
        Ok((position, total, results)) => {
            builder.leaf(srch::STATUS, "1");
            let count = results.len();
            for item in results {
                builder.extend(item.nodes);
            }
            if count > 0 {
                builder.leaf(
                    srch::RANGE,
                    format!("{position}-{}", position + count as u64 - 1),
                );
            }
            builder.leaf(srch::TOTAL, total.to_string());
        }
        Err(error) => {
            if let Some(status) = error.downcast_ref::<SearchStatus>() {
                builder.leaf(srch::STATUS, &status.0);
            } else {
                tracing::warn!(%error,"mixed-class search failed");
                builder.leaf(srch::STATUS, "3");
            }
        }
    }
    builder.end();
    builder.end();
    builder.end();
    (
        StatusCode::OK,
        [("Content-Type", "application/vnd.ms-sync.wbxml")],
        wbxml::encode_document(&builder.finish()),
    )
        .into_response()
}
